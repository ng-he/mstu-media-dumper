use mstu_sdk::{
    CommandDescriptor, Data, EnumSchema, EnumVariant, Field, Schema, Slice, Str, TYPE_BYTES,
    TYPE_STRING, TYPE_U64, Type, TypeKind::TypeEnum, TypeSchema, Value, ValueKind, slice,
};

pub static LIVE_SCHEMA_FIELDS: [Field; 3] = [
    Field {
        name: Str::from_static("file"),
        ty: &TYPE_STRING,
        description: Str::from_static("Name of the file being written, empty when none is open."),
    },
    Field {
        name: Str::from_static("chunks"),
        ty: &TYPE_U64,
        description: Str::from_static("Chunks written to the current file."),
    },
    Field {
        name: Str::from_static("bytes"),
        ty: &TYPE_U64,
        description: Str::from_static("Bytes written to the current file."),
    },
];

pub static LIVE_SCHEMA: Schema = Schema {
    fields: slice!(LIVE_SCHEMA_FIELDS),
};

pub static SETTINGS_SCHEMA_FIELDS: [Field; 1] = [Field {
    name: Str::from_static("output_path"),
    ty: &TYPE_STRING,
    description: Str::from_static(
        "Folder the files are written in. Each is named for the moment it was opened, so one \
         codec change is one file.",
    ),
}];

pub static SETTINGS_SCHEMA: Schema = Schema {
    fields: slice!(SETTINGS_SCHEMA_FIELDS),
};

pub static INPUT_SCHEMA_FIELDS: [Field; 1] = [Field {
    name: Str::from_static("data"),
    ty: &TYPE_BYTES,
    description: Str::from_static("Encoded media, written as it comes: one message is one write."),
}];

pub static INPUT_SCHEMA: Schema = Schema {
    fields: slice!(INPUT_SCHEMA_FIELDS),
};

/// What this knows how to name a file of. Anything else is written as it
/// comes, under `.bin`.
pub static CODEC_ENUM_VARIANTS: [EnumVariant; 9] = [
    codec("H264", "h264"),
    codec("H265", "h265"),
    codec("VP8", "vp8"),
    codec("VP9", "vp9"),
    codec("AV1", "av1"),
    codec("JPEG", "jpeg"),
    codec("AAC", "aac"),
    codec("MP3", "mp3"),
    codec("Opus", "opus"),
];

const fn codec(name: &'static str, value: &'static str) -> EnumVariant {
    EnumVariant {
        name: Str::from_static(name),
        value: Value {
            kind: ValueKind::ValueString,
            data: Data {
                string_: Str::from_static(value),
            },
        },
    }
}

pub static CODEC_ENUM_SCHEMA: EnumSchema = EnumSchema {
    variants: slice!(CODEC_ENUM_VARIANTS),
};

pub static CODEC_ENUM_TYPE: Type = Type {
    kind: TypeEnum,
    schema: TypeSchema {
        enum_: &CODEC_ENUM_SCHEMA,
    },
};

pub static CHANGE_CODEC_COMMAND_SCHEMA_FIELDS: [Field; 2] = [
    Field {
        name: Str::from_static("codec"),
        ty: &CODEC_ENUM_TYPE,
        description: Str::from_static(
            "What the frames that follow are. It decides the file's extension and nothing \
             else: frames are written as they come, never transcoded.",
        ),
    },
    Field {
        name: Str::from_static("extras"),
        ty: &TYPE_BYTES,
        description: Str::from_static(
            "What a decoder needs before the first frame -- parameter sets for h26x, the \
             sequence header for av1 -- written at the head of the file. Empty for anything \
             that carries its own, jpeg among them.",
        ),
    },
];

pub static CHANGE_CODEC_COMMAND_SCHEMA: Schema = Schema {
    fields: slice!(CHANGE_CODEC_COMMAND_SCHEMA_FIELDS),
};

pub static FINISH_WRITE_COMMAND_SCHEMA: Schema = Schema {
    fields: Slice::empty(),
};

pub static COMMANDS: [CommandDescriptor; 2] = [
    CommandDescriptor {
        command_name: Str::from_static("codec.change"),
        schema: &CHANGE_CODEC_COMMAND_SCHEMA,
        description: Str::from_static(
            "Says what the frames that follow are, which closes the file being written and \
             opens the next one.",
        ),
    },
    CommandDescriptor {
        command_name: Str::from_static("write.finish"),
        schema: &FINISH_WRITE_COMMAND_SCHEMA,
        description: Str::from_static(
            "Closes the file being written. Another arrives with the next codec change.",
        ),
    },
];
