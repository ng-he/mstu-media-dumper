# mstu-media-dumper

An [mstu](https://github.com/ng-he/mstu) sink plugin: writes the media it is
given to a file, one per codec change, named after the time it was opened.

Takes `codec.change` to start a new file and `finish` to close the current one,
so it can be driven by a source's `file.changed` and `file.ended` events.

## What it writes

The codec names the file; the bytes are written as they arrive.

| Codec | File | Playable on its own |
| --- | --- | --- |
| `h264`, `h265` | `.h264`, `.h265` | yes, Annex-B elementary stream |
| `av1` | `.obu` | yes, low overhead bitstream |
| `vp8`, `vp9` | `.vp8`, `.vp9` | no: frames carry no start codes, so they need a container |
| `aac`, `mp3`, `opus` | `.aac`, `.mp3`, `.opus` | as framed by whatever sent them |
| anything else | `.bin` | whatever it was |

## Building and testing

```sh
./build.sh
./test.sh
```

The library lands in `target/release`; the center repo's `collect.sh` installs
it, along with `ui/`, where the engine looks. Both scripts are what the center
repository calls, so it need not know this one is written in Rust.
