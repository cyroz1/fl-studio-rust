# Third-party notices

## MP3 encoding

MP3 export uses `mp3lame-encoder` 0.2.5 and `mp3lame-sys` 0.1.11, which
statically builds the bundled LAME 3.100 library. The LAME encoder and Rust
bindings are distributed under LGPL-3.0. The complete license text is included
at `assets/licenses/LGPL-3.0.txt`, and the dependency versions are pinned in
`Cargo.lock`.

- [mp3lame-encoder source](https://crates.io/crates/mp3lame-encoder/0.2.5)
- [mp3lame-sys source](https://crates.io/crates/mp3lame-sys/0.1.11)
