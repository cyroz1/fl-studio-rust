# Third-party notices

## MP3 encoding

MP3 export uses `mp3lame-encoder` 0.2.5 and `mp3lame-sys` 0.1.11, which
statically builds the bundled LAME 3.100 library. The LAME encoder and Rust
bindings are distributed under LGPL-3.0. The complete license text is included
at `assets/licenses/LGPL-3.0.txt`, and the dependency versions are pinned in
`Cargo.lock`.

- [mp3lame-encoder source](https://crates.io/crates/mp3lame-encoder/0.2.5)
- [mp3lame-sys source](https://crates.io/crates/mp3lame-sys/0.1.11)

## VST3 hosting

The VST3 hosting library is based on `vst3-host` 0.10.0 by Helge Sverre and is
included under its MIT license. The license text is included at
`vendor/vst3-host/LICENSE`; the dependency source and version are in
`vendor/vst3-host` and `Cargo.lock`.

- [vst3-host source](https://github.com/HelgeSverre/rust-vst3-host/tree/v0.10.0)

## VST2 hosting

VST2 class scanning and one-note audio rendering use `vst-audio` and
`vst-audio-worker-runtime` 0.7.0. The `vst-audio-worker-sys` 0.7.0 source is
vendored at `vendor/vst-audio-worker-sys` with a local patch that exposes
ordinary VST2 unique IDs through the existing class ID field; its shell child
IDs use the same encoding. The crates are MIT-licensed. Their VST2 ABI bindings
carry the BSD-3-Clause notice from Xaymar's translated header, and their MIDI
layouts carry the ISC notice from DISTRHO/DPF. The corresponding license and
provenance files are included as application resources under `assets/licenses/`;
registry dependency versions and checksums are pinned in `Cargo.lock`, and the
patched source retains its crate metadata and notices in `vendor/`. The project
uses a worker mode in its own executable, so the plug-in binary runs in a child
process without an additional worker binary.

- [vst-audio 0.7.0](https://crates.io/crates/vst-audio/0.7.0)
- [vst-audio-worker-runtime 0.7.0](https://crates.io/crates/vst-audio-worker-runtime/0.7.0)
