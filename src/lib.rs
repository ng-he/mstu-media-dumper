use std::{
    format,
    fs::File,
    io::{BufWriter, Write},
    matches,
    path::PathBuf,
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use mstu_sdk::{
    CommandDescriptor, EventDescriptor, HostContext, Logger, Message, Metadata, PluginDescriptor,
    PluginHandle, ProcessContext, Schema, Slice, Str, Value, log_debug, log_error, log_info,
    log_trace, log_warn, message, slice,
};

use crate::schemas::{COMMANDS, INPUT_SCHEMA};

pub mod schemas;

static METADATA: Metadata = Metadata {
    name: Str::from_static("Media dumper"),
    version: Str::from_static(env!("CARGO_PKG_VERSION")),
};

pub struct MediaDumperPlugin {
    id: String,
    ctx: *const HostContext,
    log: Logger,
    output_dir: String,
    current_filename: String,
    /// Locked: a `file.ended` subscription finishes the file from the source's
    /// worker thread while this plugin's own worker may be writing.
    output: Mutex<Option<BufWriter<File>>>,

    /// Written to the current file, reported on finish. Atomic: the host polls
    /// them while `process` runs.
    chunks: AtomicU64,
    bytes: AtomicU64,

    /// Files opened, which keeps one from writing over the last.
    files: u64,
}

impl MediaDumperPlugin {
    fn new(ctx: *const HostContext, id: String) -> Self {
        let log = Logger::new(ctx, unsafe { METADATA.name.as_str() }, id.as_str());

        Self {
            ctx,
            log,
            id,
            output_dir: String::new(),
            current_filename: String::new(),
            output: Mutex::new(None),
            chunks: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            files: 0,
        }
    }

    /// Pushes what the UI shows; the host keeps the latest.
    fn publish_live(&self) {
        let name = match self.output.lock().unwrap().is_some() {
            true => Str::new(self.current_filename.as_str()),
            false => Str::from_static(""),
        };

        let values = [
            name.into(),
            self.chunks.load(Ordering::Relaxed).into(),
            self.bytes.load(Ordering::Relaxed).into(),
        ];

        unsafe {
            ((*self.ctx).publish_live)(
                Str::new(self.id.as_str()),
                Message {
                    values: Slice::from_raw_parts(values.as_ptr(), values.len()),
                },
            );
        }
    }

    fn change_codec(&mut self, codec: &str, extras: &[u8]) -> bool {
        self.finish();

        let ext = extension(codec);

        if ext == UNKNOWN {
            log_warn!(
                self.log,
                "unknown codec '{codec}', writing it as .{UNKNOWN}"
            );
        }

        // Frames of these carry no start codes, so a player has nothing to cut
        // them apart by until they are put in a container.
        if needs_a_container(codec) {
            log_warn!(
                self.log,
                "{codec} frames are written as they come; they need a container to be played"
            );
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        // The id keeps dumpers in the same pipeline from sharing a file, and
        // the count keeps one dumper from writing over its own: two files in
        // the same millisecond is an ordinary thing to ask for.
        let filename = format!("dump_{}_{}_{:05}.{}", timestamp, self.id, self.files, ext);

        self.files += 1;
        self.current_filename = filename.clone();
        let path = PathBuf::from(self.output_dir.clone()).join(filename);

        let file = match File::create(&path) {
            Ok(file) => file,
            Err(err) => {
                log_error!(self.log, "cannot create '{}': {err}", path.display());
                return false;
            }
        };

        log_info!(self.log, "writing {codec} to '{}'", path.display());

        *self.output.lock().unwrap() = Some(BufWriter::new(file));
        self.chunks.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);

        self.write(extras)
    }

    fn write(&mut self, data: &[u8]) -> bool {
        {
            let mut output = self.output.lock().unwrap();

            let Some(writer) = output.as_mut() else {
                log_warn!(self.log, "dropped {} byte(s), no file open", data.len());
                return true;
            };

            if let Err(e) = writer.write_all(data) {
                log_error!(self.log, "{e}");
                return false;
            }
        }

        let chunks = self.chunks.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;

        log_trace!(self.log, "wrote {} byte(s)", data.len());

        // A per-write log is far too loud, so mark progress now and then.
        if chunks % 1000 == 0 {
            log_debug!(
                self.log,
                "{chunks} chunk(s), {bytes} byte(s) into '{}'",
                self.current_filename
            );
        }

        self.publish_live();

        true
    }

    fn finish(&mut self) -> bool {
        let taken = self.output.lock().unwrap().take();

        if let Some(mut writer) = taken {
            if let Err(e) = writer.flush() {
                log_error!(self.log, "{e}");
                return false;
            }

            log_info!(
                self.log,
                "finished {}/{}: {} chunk(s), {} byte(s)",
                self.output_dir,
                self.current_filename,
                self.chunks.load(Ordering::Relaxed),
                self.bytes.load(Ordering::Relaxed)
            );
        }

        self.publish_live();

        true
    }

    fn set_output_path(&mut self, dir: String) {
        log_info!(self.log, "output directory is '{dir}'");

        self.output_dir = dir
    }
}

/// What a stream of an unnamed codec is written as.
const UNKNOWN: &str = "bin";

/// What the stream is called once it is on its own, outside the file it came
/// from. An unknown codec is still written: a dump nobody can name is worth
/// more than no dump at all.
fn extension(codec: &str) -> &'static str {
    match codec {
        "h264" | "avc" => "h264",
        "h265" | "hevc" => "h265",
        "av1" => "obu",
        "vp8" => "vp8",
        "vp9" => "vp9",
        "aac" => "aac",
        "mp3" => "mp3",
        "opus" => "opus",
        "jpeg" | "mjpeg" => "jpg",
        _ => UNKNOWN,
    }
}

/// Whether frames of this codec carry nothing to tell them apart by.
fn needs_a_container(codec: &str) -> bool {
    matches!(codec, "vp8" | "vp9")
}

extern "C" fn metadata() -> &'static Metadata {
    &METADATA
}

extern "C" fn create(ctx: *const HostContext, id: Str) -> PluginHandle {
    let plugin = MediaDumperPlugin::new(ctx, id.to_string());

    log_info!(plugin.log, "created");

    // So a page opening before anything happens has readings to show.
    plugin.publish_live();

    Box::into_raw(Box::new(plugin)) as PluginHandle
}

extern "C" fn id(instance: PluginHandle) -> Str {
    let plugin = unsafe { &*(instance as *mut MediaDumperPlugin) };
    Str::new(plugin.id.as_str())
}

extern "C" fn release(instance: PluginHandle) {
    unsafe {
        let mut plugin = Box::from_raw(instance as *mut MediaDumperPlugin);

        plugin.finish();

        log_info!(plugin.log, "released");

        drop(plugin);
    }
}

extern "C" fn process(instance: PluginHandle, p_ctx: *mut ProcessContext) -> bool {
    let plugin = unsafe { &mut *(instance as *mut MediaDumperPlugin) };

    // The engine checks the input against the schema, so field 0 is bytes.
    let field = unsafe { (*(*p_ctx).input).values.at(0).get::<Slice<u8>>() };

    let Some(data) = field else {
        log_error!(plugin.log, "input field 0 is not bytes");
        return false;
    };

    plugin.write(unsafe { data.as_slice() })
}

extern "C" fn process_input_schema() -> *const Schema {
    &INPUT_SCHEMA
}

extern "C" fn process_output_schema() -> *const Schema {
    ptr::null()
}

extern "C" fn settings_schema() -> *const Schema {
    &schemas::SETTINGS_SCHEMA
}

extern "C" fn settings(_instance: PluginHandle, _output: *mut message::Writer) -> bool {
    true
}

extern "C" fn set_parameter(instance: PluginHandle, field: usize, value: Value) -> bool {
    let plugin = unsafe { &mut *(instance as *mut MediaDumperPlugin) };

    match field {
        0 => unsafe {
            let output_dir: Str = value.get().unwrap_or(Str::from_static(""));
            plugin.set_output_path(String::from(output_dir.as_str()));
            true
        },
        _ => {
            log_warn!(plugin.log, "ignored unknown parameter {field}");
            true
        }
    }
}

/// UI folder, resolved by the host against the plugin library directory.
extern "C" fn ui() -> Str {
    Str::from_static("mstu-media-dumper-ui")
}

extern "C" fn live_schema() -> *const Schema {
    &schemas::LIVE_SCHEMA
}

extern "C" fn events() -> Slice<EventDescriptor> {
    Slice::empty()
}

extern "C" fn commands() -> Slice<CommandDescriptor> {
    slice!(COMMANDS)
}

extern "C" fn invoke(instance: PluginHandle, command: usize, input: *const Message) -> bool {
    let plugin = unsafe { &mut *(instance as *mut MediaDumperPlugin) };
    let values = unsafe { &(*input).values };

    log_debug!(plugin.log, "command {command} with {:?}", unsafe {
        &*input
    });

    match command {
        0 => {
            // The engine checks the payload against the command schema.
            let (Some(codec), Some(extras)) =
                (values.at(0).get::<Str>(), values.at(1).get::<Slice<u8>>())
            else {
                log_error!(plugin.log, "file.changed payload is not (string, bytes)");
                return false;
            };

            unsafe { plugin.change_codec(codec.as_str(), extras.as_slice()) }
        }
        1 => plugin.finish(),
        _ => {
            log_warn!(plugin.log, "ignored unknown command {command}");
            true
        }
    }
}

static PLUGIN: PluginDescriptor = PluginDescriptor {
    metadata,
    create,
    id,
    release,

    process_input_schema,
    process_output_schema,
    settings_schema,

    settings,
    set_parameter,

    ui,

    process,

    events,
    commands,
    invoke,

    live_schema,
};

#[unsafe(no_mangle)]
pub extern "C" fn plugin_descriptor() -> *const PluginDescriptor {
    &PLUGIN
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs, path::Path};

    use mstu_sdk::{LogLevel, message};

    use super::*;

    extern "C" fn log_nothing(_: LogLevel, _: Str, _: Str) {}
    extern "C" fn log_disabled(_: LogLevel) -> bool {
        false
    }
    extern "C" fn no_event(_: Str, _: usize) -> *const message::Writer {
        ptr::null()
    }
    extern "C" fn no_publish(_: Str, _: usize) {}
    thread_local! {
        /// The last live values the plugin published, as the host saw them.
        static PUBLISHED: RefCell<Vec<(String, u64, u64)>> = const { RefCell::new(Vec::new()) };
    }

    extern "C" fn no_live(_: Str, snapshot: Message) {
        let values = snapshot.values_slice();

        let name = values
            .first()
            .and_then(|value| value.get::<Str>())
            .map(|text| unsafe { text.as_str() }.to_string())
            .unwrap_or_default();

        let number = |at: usize| {
            values
                .get(at)
                .and_then(|value| value.get::<u64>())
                .unwrap_or(0)
        };

        PUBLISHED.with(|published| published.borrow_mut().push((name, number(1), number(2))));
    }

    fn published() -> Vec<(String, u64, u64)> {
        PUBLISHED.with(|published| published.borrow().clone())
    }

    fn forget_published() {
        PUBLISHED.with(|published| published.borrow_mut().clear());
    }

    /// A host that answers every call with nothing, so the plugin can be run
    /// without an engine behind it.
    fn host() -> Box<HostContext> {
        Box::new(HostContext {
            logger: log_nothing,
            log_enabled: log_disabled,
            new_event: no_event,
            publish_events: no_publish,
            publish_live: no_live,
        })
    }

    /// A directory of its own, taken away afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("mstu-dumper-{}-{name}", std::process::id()));

            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("a directory to write in");

            Self(path)
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .expect("the directory")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect();

            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn dumper(ctx: &HostContext, dir: &Path) -> MediaDumperPlugin {
        let mut plugin = MediaDumperPlugin::new(ctx, "TEST".to_string());
        plugin.set_output_path(dir.to_string_lossy().to_string());

        plugin
    }

    #[test]
    fn names_the_file_after_the_codec() {
        assert_eq!(extension("h264"), "h264");
        assert_eq!(extension("hevc"), "h265");
        assert_eq!(extension("av1"), "obu");
        assert_eq!(extension("opus"), "opus");
    }

    #[test]
    fn writes_a_codec_it_cannot_name_rather_than_refusing_it() {
        assert_eq!(extension("something-new"), UNKNOWN);
        assert_eq!(extension(""), UNKNOWN);
    }

    #[test]
    fn knows_which_frames_cannot_be_told_apart() {
        assert!(needs_a_container("vp9"));
        assert!(!needs_a_container("h264"));
        assert!(!needs_a_container("av1"));
    }

    /// A decoder opening a file for every picture does this all the time, and
    /// a name made of whole seconds would write each over the last.
    #[test]
    fn files_opened_in_one_millisecond_are_still_separate_files() {
        let ctx = host();
        let dir = Scratch::new("same-instant");
        let mut plugin = dumper(&ctx, &dir.0);

        for at in 0..4u8 {
            assert!(plugin.change_codec("jpeg", &[]));
            assert!(plugin.write(&[0xff, 0xd8, 0xff, at]));
        }

        assert!(plugin.finish());

        let written = dir.files();

        assert_eq!(written.len(), 4, "{written:?}");
        assert!(
            written.iter().all(|name| name.ends_with(".jpg")),
            "{written:?}"
        );

        // Sorted by name is sorted by when they were opened.
        let last = fs::read(dir.0.join(&written[3])).expect("the last dump");
        assert_eq!(last, [0xff, 0xd8, 0xff, 3]);
    }

    #[test]
    fn writes_the_extras_first_and_the_media_after() {
        let ctx = host();
        let dir = Scratch::new("extras");
        let mut plugin = dumper(&ctx, &dir.0);

        assert!(plugin.change_codec("h264", &[0, 0, 0, 1, 0x67]));
        assert!(plugin.write(&[0, 0, 0, 1, 0x65, 0x88]));
        assert!(plugin.finish());

        let written = dir.files();
        assert_eq!(written.len(), 1);
        assert!(written[0].ends_with(".h264"), "{written:?}");

        let bytes = fs::read(dir.0.join(&written[0])).expect("the dump");
        assert_eq!(bytes, vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x65, 0x88]);
    }

    #[test]
    fn counts_what_it_wrote() {
        let ctx = host();
        let dir = Scratch::new("counts");
        let mut plugin = dumper(&ctx, &dir.0);

        plugin.change_codec("h264", &[]);
        plugin.write(&[1, 2, 3]);
        plugin.write(&[4, 5]);

        // The extras count as a write of their own, empty as they are here.
        assert_eq!(plugin.chunks.load(Ordering::Relaxed), 3);
        assert_eq!(plugin.bytes.load(Ordering::Relaxed), 5);

        plugin.finish();
    }

    #[test]
    fn a_second_codec_starts_a_second_file() {
        let ctx = host();
        let dir = Scratch::new("second");
        let mut plugin = dumper(&ctx, &dir.0);

        plugin.change_codec("h264", &[]);
        plugin.write(&[1, 2, 3]);

        plugin.change_codec("av1", &[]);
        plugin.write(&[4, 5, 6]);
        plugin.finish();

        let written = dir.files();

        assert_eq!(written.len(), 2, "{written:?}");
        assert!(written.iter().any(|name| name.ends_with(".h264")));
        assert!(written.iter().any(|name| name.ends_with(".obu")));

        // Counting starts again with the new file.
        assert_eq!(plugin.bytes.load(Ordering::Relaxed), 3);
    }

    /// Media arriving before any codec has nowhere to go, and saying so is
    /// better than making a file nobody asked for.
    #[test]
    fn media_before_a_codec_is_dropped_without_failing() {
        let ctx = host();
        let dir = Scratch::new("early");
        let mut plugin = dumper(&ctx, &dir.0);

        assert!(plugin.write(&[1, 2, 3]));
        assert!(dir.files().is_empty());
    }

    // ---------- the surface the engine actually calls ----------

    /// A plugin made the way the engine makes one, taken away afterwards.
    struct Running {
        handle: PluginHandle,
        dir: Scratch,
    }

    impl Running {
        fn new(ctx: &HostContext, name: &str) -> Self {
            let dir = Scratch::new(name);
            let handle = create(ctx, Str::from_static("PLUG-TEST"));

            let path = dir.0.to_string_lossy().to_string();
            assert!(set_parameter(handle, 0, Str::new(path.as_str()).into()));

            Self { handle, dir }
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            release(self.handle);
        }
    }

    /// Media arrives as one field of bytes, which is what the input schema
    /// declares.
    fn send(handle: PluginHandle, bytes: &[u8]) -> bool {
        let values = [Slice::from_raw_parts(bytes.as_ptr(), bytes.len()).into()];
        let input = Message {
            values: Slice::from_raw_parts(values.as_ptr(), values.len()),
        };

        let mut ctx = ProcessContext {
            input: &input,
            output: ptr::null_mut(),
        };

        process(handle, &mut ctx)
    }

    fn change(handle: PluginHandle, codec: &str, extras: &[u8]) -> bool {
        let values = [
            Str::new(codec).into(),
            Slice::from_raw_parts(extras.as_ptr(), extras.len()).into(),
        ];

        let payload = Message {
            values: Slice::from_raw_parts(values.as_ptr(), values.len()),
        };

        invoke(handle, 0, &payload)
    }

    #[test]
    fn a_plugin_is_made_and_named_as_the_engine_names_it() {
        let ctx = host();
        let running = Running::new(&ctx, "created");

        assert_eq!(unsafe { id(running.handle).as_str() }, "PLUG-TEST");
        assert_eq!(unsafe { (metadata)().name.as_str() }, "Media dumper");
    }

    #[test]
    fn media_sent_through_the_engine_lands_in_the_file() {
        let ctx = host();
        let running = Running::new(&ctx, "process");

        assert!(change(running.handle, "h264", &[0, 0, 0, 1, 0x67]));
        assert!(send(running.handle, &[0, 0, 0, 1, 0x65, 0x88]));
        assert!(invoke(
            running.handle,
            1,
            &Message {
                values: Slice::empty()
            }
        ));

        let written = running.dir.files();
        assert_eq!(written.len(), 1, "{written:?}");

        let bytes = fs::read(running.dir.0.join(&written[0])).expect("the dump");
        assert_eq!(bytes, vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x65, 0x88]);
    }

    /// The engine checks the input against the schema, but a plugin that
    /// unwraps blindly is one bad wire away from going down.
    #[test]
    fn media_of_the_wrong_kind_is_refused_rather_than_unwrapped() {
        let ctx = host();
        let running = Running::new(&ctx, "wrong-kind");

        change(running.handle, "h264", &[]);

        let values = [7u64.into()];
        let input = Message {
            values: Slice::from_raw_parts(values.as_ptr(), values.len()),
        };

        let mut p_ctx = ProcessContext {
            input: &input,
            output: ptr::null_mut(),
        };

        assert!(!process(running.handle, &mut p_ctx));
    }

    #[test]
    fn a_codec_change_of_the_wrong_shape_is_refused() {
        let ctx = host();
        let running = Running::new(&ctx, "wrong-shape");

        // Two numbers where a name and its extras belong.
        let values = [1u64.into(), 2u64.into()];
        let payload = Message {
            values: Slice::from_raw_parts(values.as_ptr(), values.len()),
        };

        assert!(!invoke(running.handle, 0, &payload));
        assert!(running.dir.files().is_empty(), "nothing was opened");
    }

    #[test]
    fn a_command_it_does_not_have_is_ignored_rather_than_failed() {
        let ctx = host();
        let running = Running::new(&ctx, "unknown-command");

        assert!(invoke(
            running.handle,
            9,
            &Message {
                values: Slice::empty()
            }
        ));
    }

    #[test]
    fn a_setting_it_does_not_have_is_ignored_rather_than_failed() {
        let ctx = host();
        let running = Running::new(&ctx, "unknown-setting");

        assert!(set_parameter(running.handle, 9, 1u64.into()));
    }

    /// Two dumpers in one pipeline write beside each other, so the name has
    /// to say which wrote which.
    #[test]
    fn the_file_is_named_after_the_plugin_that_wrote_it() {
        let ctx = host();
        let running = Running::new(&ctx, "named");

        change(running.handle, "h264", &[]);
        invoke(
            running.handle,
            1,
            &Message {
                values: Slice::empty(),
            },
        );

        let written = running.dir.files();

        assert!(written[0].contains("PLUG-TEST"), "{written:?}");
        assert!(written[0].starts_with("dump_"), "{written:?}");
    }

    /// Finishing what was never started, and finishing twice, are both
    /// things the engine does on the way down.
    #[test]
    fn finishing_nothing_is_harmless() {
        let ctx = host();
        let running = Running::new(&ctx, "finish-nothing");

        assert!(invoke(
            running.handle,
            1,
            &Message {
                values: Slice::empty()
            }
        ));

        change(running.handle, "h264", &[]);
        send(running.handle, &[1, 2, 3]);

        assert!(invoke(
            running.handle,
            1,
            &Message {
                values: Slice::empty()
            }
        ));
        assert!(invoke(
            running.handle,
            1,
            &Message {
                values: Slice::empty()
            }
        ));

        assert_eq!(running.dir.files().len(), 1);
    }

    #[test]
    fn a_folder_that_cannot_be_written_to_is_reported_rather_than_assumed() {
        let ctx = host();
        let handle = create(&*ctx, Str::from_static("PLUG-TEST"));

        let missing = "/nowhere/at/all";
        assert!(set_parameter(handle, 0, Str::from_static(missing).into()));

        assert!(!change(handle, "h264", &[]), "there is nowhere to write");

        release(handle);
    }

    // ---------- what the plugin says about itself ----------

    /// The live values the plugin publishes have to match the schema it
    /// declares, or the engine drops every snapshot and the page goes blank.
    #[test]
    fn what_is_published_matches_the_live_schema() {
        let ctx = host();
        forget_published();

        let running = Running::new(&ctx, "live");

        change(running.handle, "h264", &[1, 2, 3]);
        send(running.handle, &[4, 5]);

        let published = published();
        let last = published.last().expect("something was published");

        let declared = unsafe { (*live_schema()).fields.as_slice() };

        assert_eq!(declared.len(), 3, "name, chunks and bytes");
        assert!(
            last.0.ends_with(".h264"),
            "the file being written: {}",
            last.0
        );
        assert_eq!(last.1, 2, "two writes: the extras and the media");
        assert_eq!(last.2, 5, "three bytes of extras and two of media");
    }

    #[test]
    fn a_finished_file_is_published_as_no_file_at_all() {
        let ctx = host();
        let running = Running::new(&ctx, "live-finish");

        change(running.handle, "h264", &[]);
        forget_published();

        invoke(
            running.handle,
            1,
            &Message {
                values: Slice::empty(),
            },
        );

        let last = published()
            .last()
            .cloned()
            .expect("something was published");

        assert_eq!(last.0, "", "nothing is open now");
    }

    /// What the engine reads off the descriptor to draw the node and wire it.
    #[test]
    fn the_descriptor_says_what_a_sink_says() {
        assert!(!process_input_schema().is_null(), "it takes media");
        assert!(process_output_schema().is_null(), "and produces none");
        assert!(!settings_schema().is_null());
        assert!(!live_schema().is_null());

        assert!(events().is_empty(), "a sink raises nothing");
        assert_eq!(commands().len, 2, "codec.change and write.finish");

        assert_eq!(unsafe { ui().as_str() }, "mstu-media-dumper-ui");
    }

    /// The commands the engine is told about are the ones invoke answers.
    #[test]
    fn the_commands_declared_are_the_ones_that_are_answered() {
        let declared = unsafe { commands().as_slice() };

        assert_eq!(unsafe { declared[0].command_name.as_str() }, "codec.change");
        assert_eq!(unsafe { declared[1].command_name.as_str() }, "write.finish");

        // The first takes a name and its extras, which is what invoke reads.
        let payload = unsafe { declared[0].schema.fields.as_slice() };

        assert_eq!(payload.len(), 2);
        assert_eq!(unsafe { payload[0].name.as_str() }, "codec");
    }

    #[test]
    fn the_descriptor_handed_to_the_engine_is_this_plugin() {
        let descriptor = unsafe { &*plugin_descriptor() };

        assert_eq!(
            unsafe { (descriptor.metadata)().name.as_str() },
            "Media dumper"
        );
        assert!((descriptor.process_output_schema)().is_null());
    }
}
