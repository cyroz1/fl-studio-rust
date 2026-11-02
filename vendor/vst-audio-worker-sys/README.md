# vst-audio

A GUI-independent Rust library for hosting VST2 and VST3 plugins
in separate processes on Windows, macOS and Linux.

Required CI executes native Windows x64, Linux x64, macOS ARM64 and macOS Intel
tests, four unrelated Rust GUI consumers (egui, iced, Slint and Tauri), and
independently built open-source plugins.

The contract is one worker per plugin instance, safe application-side
Rust APIs, bounded audio IPC and worker-owned native editor windows. Any Rust GUI
can request an editor over the same control protocol. Native editor embedding
inside an application's window is a separate, platform-specific capability.
Editors open with automatic DPI scaling. `Command::OpenEditorWith` selects
plugin-only scaling, Windows floating-window stretching or unscaled drawing;
`EditorStatus` reports the mode and scale actually in use. See the support guide
for X11 `Xft.dpi`, Wayland output scale and Windows mixed-DPI limits.
Process isolation contains crashes and hangs. An opt-in OS sandbox restricts
worker network access, program execution and filesystem writes; it is off by
default for plugin compatibility. Set `Configuration::sandbox` (or
`ProcessingOptions::sandbox`) to `Some(SandboxPolicy::default())`, and use
`Host::scan_sandboxed` for discovery. Unsupported restrictions fail startup
rather than silently running unsandboxed. See the support guide for platform
exceptions and remaining security limits.

See [architecture](https://github.com/115dkk/vst-audio/blob/main/docs/architecture.md), [implementation and evidence](https://github.com/115dkk/vst-audio/blob/main/docs/roadmap.md)
and [support and verification](https://github.com/115dkk/vst-audio/blob/main/docs/support.md).
[Independent plugin measurements](https://github.com/115dkk/vst-audio/blob/main/docs/independent-plugins.md),
[extended checks](https://github.com/115dkk/vst-audio/blob/main/docs/extended-tests.md) and
[worker distribution](https://github.com/115dkk/vst-audio/blob/main/docs/distribution.md) describe the release gates.
Source and license provenance is recorded in [NOTICE](https://github.com/115dkk/vst-audio/blob/main/NOTICE).

The application API has a nonblocking loading task, bounded control requests,
polled events and an independent supervisor. Any GUI can use this contract:

```rust,ignore
let host = vst_audio::Host::new(worker_path);
let task = host.spawn_async(plugin, vst_audio::Configuration::default(), None);
// In the GUI update loop:
if let Some(instance) = task.try_complete()? {
    instance.controller().request(vst_audio::Command::OpenEditor)?;
    // Keep `instance` alive; dropping it requests worker shutdown.
}
```

`Instance::take_audio` transfers the unique preallocated variable-callback
endpoint; `take_blocks` instead transfers a block-synchronous `BlockEndpoint`.
It accepts variable-length planar f32/f64 callbacks up to the configured
`max_callback_frames`, reblocks them into a prepared quantum, and carries bounded
sample-offset MIDI/SysEx, automation and transport data. The implemented ring
uses `Configuration::slots` (2, 4, 8 or 16) for the ring capacity and wraps its
modulo-2^32 sequence numbers continuously. `BlockLane` is the standalone
ring-crate client; block misses are reported for caller fallback. The audio
endpoint returns processed or explicit fallback output and reports
`(ceil(max_callback_frames / quantum) + 1) * quantum` plus plugin latency; the
default remains two quanta plus plugin latency. Recovered workers swap at audio
boundaries. The supervisor runs even when the GUI does not poll. Drop audio
resources on a control thread.

Build and test:

```text
cargo test --workspace
python tools/check_boundary.py
python tools/native_tests.py
python tools/third_party.py
python tools/package_check.py
python tools/package_worker.py
```

On headless Linux, run the native and worker-distribution commands under `xvfb-run -a`.
The [distribution guide](https://github.com/115dkk/vst-audio/blob/main/docs/distribution.md) explains executable placement,
checksums, license notices and the separate macOS signing/notarization steps.
The CLI `cargo run -p vst-audio --example editor -- <worker> <plugin>` opens an
editor without a GUI framework.

The independent [egui, iced, Slint and Tauri consumers](https://github.com/115dkk/vst-audio/tree/main/examples/gui) demonstrate the same
editor IPC contract. They are built and exercised separately in CI.

VST is a registered trademark of Steinberg Media Technologies GmbH.
This project is independent of Steinberg and is not endorsed by it.
