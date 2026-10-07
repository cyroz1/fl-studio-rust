# Rust DAW compatibility rebuild

This is a clean-room Rust project started from the FL Studio 26.1.6.5639 installation supplied with the workspace. It is an independent rebuild; it does not load, patch, or link against the installed FL Studio executable or engine. The end goal is to open and modify any FL Studio project with backwards compatibility, matching controls, windows, and editing behavior, installed plug-in hosting, playback, and rendering. The current code contains the lossless file-format core and the first desktop editing shell; it is still far from full parity.

The first milestone is deliberately a compatibility foundation: inspect FL Studio project (`.flp`) files and preserve every event, including data the reader does not understand. The same chunk reader also accepts a bundled `.fst` state preset, though it does not yet interpret most preset state. FL Studio's own documentation describes `.flp` as its native project format, `.fst` as a state/preset format, and ZIP project packages as projects bundled with referenced sample files. The installed projects and presets provide a local compatibility corpus.

## Current milestone

`flp-rebuild` can read the FLP chunk envelope and event stream, report header information, event counts, channel summaries, and a small set of project fields, and write a lossless round-trip copy. Unknown event payloads and trailing bytes are retained exactly. The command-line surface is:

```text
flp-rebuild info <file.flp>
flp-rebuild midi-info <file.mid>
flp-rebuild midi-events <file.mid> <track> [start] [count]
flp-rebuild scan <directory>
flp-rebuild plugin-scan
flp-rebuild plugin-state-preview <file.flp> <channel-id>
flp-rebuild vst3-state-probe <plugin.vst3> <file.flp> <channel-id>
flp-rebuild channels <file.flp>
flp-rebuild plugin-states <file.flp>
flp-rebuild channel-events <file.flp> <channel-id>
flp-rebuild patterns <file.flp>
flp-rebuild playlist <file.flp> [start] [count]
flp-rebuild notes <file.flp> <pattern-id> [start] [count]
flp-rebuild events <file.flp> [start] [count]
flp-rebuild extract-event <file.flp> <index> <output.bin>
flp-rebuild roundtrip <input.flp> <output.flp>
flp-rebuild set-tempo <input.flp> <output.flp> <bpm>
flp-rebuild rename-channel <input.flp> <output.flp> <channel-id> <name>
flp-rebuild edit-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <position> <length> <key> <velocity>
flp-rebuild add-note <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <key> <velocity>
flp-rebuild delete-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index>
flp-rebuild import-midi <input.flp> <input.mid> <output.flp> <track> <pattern-id> <channel-id>
flp-rebuild edit-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position> <length>
```

Launch the native desktop shell with `cargo run --release --bin fl-studio-rebuild`. It opens maximized, can open/save FLP files, and displays the decoded Playlist, Channel Rack, Piano roll, and Mixer. F5, F6, F7, and F9 switch between Playlist, Channel Rack, Piano roll, and Mixer, respectively. The Playlist supports selecting a clip and editing its start and length. The Piano roll supports choosing a pattern and channel, double-clicking the grid to create a note, selecting and editing a note's start, length, key, and velocity, and deleting the selected note. Drag a note body to move it on the grid; drag its right edge to resize it. The snap menu offers no snap, subdivisions of a beat, one or two beats, and a bar sized to the project's time signature. Dragging vertically changes the key within the visible keyboard range. These note operations rewrite only the affected length-prefixed score event and preserve other event bytes. Tempo edits are also written into the loaded project. The Plug-ins tab scans conventional VST locations on Windows, macOS, and Linux, plus directories listed in `VST3_PATH` and `VST2_PATH`. It can load a selected VST3 into the process, open its native editor, and expose host-reported parameters. For recognized VST project state, it reads the VST name, vendor, bundle path, and class UID, suggests a matching installed bundle, and can try restoring the FLP `0xD5` record. A successfully loaded project-matched VST3 is linked to its Channel Rack row, which displays the plug-in name and can reopen its native editor window. Full Image-Line wrapper and state conversion remain incomplete. The Mixer view is a placeholder until mixer state is decoded.

The desktop executable also accepts an `.flp` path as its first argument to open that project at startup.

In the desktop Piano roll, **Open MIDI…** loads a MIDI file, lets you choose a track, and imports its notes into the selected pattern and channel. The selected track's note timing is converted from MIDI PPQ to project PPQ. The CLI offers the same import operation for a chosen track and output copy.

This is not yet an audio renderer or a complete music editor. The reader exposes channel summaries, patterns and notes, Playlist track names and arrangement clips, and opaque channel plug-in payloads; most other event meanings remain opaque. `playlist` accepts an optional starting clip and count. `plugin-states` reports channel plug-in payload sizes and event indices without decoding them. `edit-note` changes an existing note's start, length, key, and velocity while retaining its other note fields. `add-note` appends a 24-byte note record to a pattern; for an empty pattern it uses a note-event encoding observed elsewhere in the project and refuses to guess when encodings conflict. `delete-note` removes a channel-scoped note by zero-based index. These note operations rewrite only the affected score event and its length prefix. `edit-clip` changes an existing Playlist clip's position and length while retaining its other record bytes. `set-tempo` and `rename-channel` edit existing fields and preserve every unrelated event's original wire bytes. The CLI does not yet create missing patterns, channels, clips, or other project objects.

The Standard MIDI File reader recognizes the `MThd` header and `MTrk` event streams, including running-status channel messages, meta events, SysEx, and system messages. `midi-info` reports track names, timing, paired note events, and tempo-map spans. `import-midi` appends notes from one PPQ-timed MIDI track to an existing FLP pattern and channel, scaling note positions and lengths to the project's PPQ and preserving the FLP tempo and unrelated event bytes. Unclosed MIDI notes use the remaining track duration, or one beat if the track ends at the note start. SMPTE-timed MIDI, tempo-map conversion, and automatic channel/pattern creation are not implemented yet.

`scan <directory>` inventories `.flp` and `.fst` files recursively, grouping successfully parsed files by project version, header format, and PPQ while reporting any unparsed files. `plugin-scan` lists VST3 bundles and VST2 DLL candidates in the conventional Windows plug-in folders without loading their code. `plugin-state-preview` reports a channel's opaque wrapper and plug-in payload sizes and leading bytes. `vst3-state-probe` transiently loads a VST3 and offers the selected FLP channel's plug-in state to it through the host state interface; this is an experiment, not yet project-wide compatibility. In the current ZENOLOGY probe, the VST3 accepted a 164,850-byte FLP payload and produced a 163,256-byte host snapshot. These commands map installed compatibility inputs before deeper project and plug-in support is added.

The original application's MIDI export experiments and their measured results are recorded in [`docs/oracle-experiments.md`](docs/oracle-experiments.md). `midi-events` displays a selected MIDI track's decoded events, including tempo and marker metadata, for further format comparison.

## Automated checks and installers

GitHub Actions runs formatting checks, Clippy, and the Rust test suite on Ubuntu, macOS, and Windows for pushes, pull requests, and manual runs. Each platform job also builds a native desktop package with Cargo Packager and uploads it as a workflow artifact: Windows NSIS installer, macOS DMG and app bundle, and Linux Debian, AppImage, and pacman packages. The package definitions are in [`Packager.toml`](Packager.toml).

## Compatibility plan

1. Project containers: broaden FLP envelope compatibility across releases, add semantic FST preset support, and handle ZIP project packages.
2. Musical model: broaden pattern and note editing, then decode channel and plugin state, Playlist clip variants, automation, mixer routing, and referenced media, while retaining unsupported fields.
3. Interchange: MIDI import/export and audio export with stable timing and channel mapping.
4. Playback: transport, tempo clock, sample scheduling, mixing, automation, and a low-latency audio backend.
5. Instruments and effects: continue VST3 hosting from the current discovery, transient state probe, native editor window, and parameter-list prototype into project-state restore, automation, MIDI/audio routing, and offline rendering. Add VST2 hosting if an applicable legacy licensing path is available. Preserve plug-in identity and opaque state when a plug-in is unavailable. Rebuilding FL's bundled plug-in DSP is out of scope.
6. User experience: project browser, Playlist, piano roll, channel rack, mixer, plugin windows, editing tools, shortcuts, and accessibility.
7. Historical compatibility: fixture-driven support across older FLP versions, with explicit reporting where a project depends on unavailable plugins or media.

Full feature parity is a long-running objective. Each milestone should keep unsupported project data intact so opening and saving a project does not silently destroy state. Installed plug-ins are treated as external instruments/effects to host, rather than DSP to reimplement.

## Product references

- [FL Studio project file format](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_save_flp.htm)
- [FL Studio state file format](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_other_fst.htm)
- [Project files and ZIP project packages](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/browser.htm)
