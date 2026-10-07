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
flp-rebuild sample-paths <file.flp>
flp-rebuild audio-info <audio-file>
flp-rebuild render-audio-clips <project.flp> <output.wav> [arrangement-id]
flp-rebuild render-pattern-vst3 <project.flp> <pattern-id> <channel-id> <plugin.vst3> <output.wav> [tail-seconds]
flp-rebuild plugin-state-preview <file.flp> <channel-id>
flp-rebuild vst3-state-probe <plugin.vst3> <file.flp> <channel-id>
flp-rebuild channels <file.flp>
flp-rebuild plugin-states <file.flp>
flp-rebuild channel-events <file.flp> <channel-id>
flp-rebuild mixer <file.flp>
flp-rebuild automation <file.flp>
flp-rebuild time-markers <file.flp>
flp-rebuild patterns <file.flp>
flp-rebuild playlist <file.flp> [start] [count]
flp-rebuild notes <file.flp> <pattern-id> [start] [count]
flp-rebuild events <file.flp> [start] [count]
flp-rebuild extract-event <file.flp> <index> <output.bin>
flp-rebuild roundtrip <input.flp> <output.flp>
flp-rebuild set-tempo <input.flp> <output.flp> <bpm>
flp-rebuild rename-channel <input.flp> <output.flp> <channel-id> <name>
flp-rebuild set-channel-levels <input.flp> <output.flp> <channel-id> <volume-0..12800> <pan-0..12800>
flp-rebuild set-layer-children <input.flp> <output.flp> <layer-channel-id> <child-ids-comma-separated|->
flp-rebuild edit-automation-point <input.flp> <output.flp> <channel-id> <point-index> <position-beats> <value> <tension>
flp-rebuild edit-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <position> <length> <key> <velocity>
flp-rebuild add-note <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <key> <velocity>
flp-rebuild delete-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index>
flp-rebuild import-midi <input.flp> <input.mid> <output.flp> <track> <pattern-id> <channel-id>
flp-rebuild edit-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position> <length>
```

Launch the native desktop shell with `cargo run --release --bin fl-studio-rebuild`. It opens maximized, can open/save FLP files, and displays the decoded Playlist, Channel Rack, Piano roll, Mixer, Plug-ins, and Audio views. F5, F6, F7, and F9 switch between Playlist, Channel Rack, Piano roll, and Mixer, respectively. The Audio view selects input/output devices and provides a live capture meter, input monitoring, and a quiet output test tone. Windows offers WASAPI shared and exclusive access; shared mode uses the Windows audio engine, while exclusive mode opens the selected endpoint directly and reports unsupported formats or access conflicts. The Playlist supports selecting a clip and editing its start and length. The Piano roll supports choosing a pattern and channel, double-clicking the grid to create a note, selecting and editing a note's start, length, key, and velocity, and deleting the selected note. Drag a note body to move it on the grid; drag its right edge to resize it. The snap menu offers no snap, subdivisions of a beat, one or two beats, and a bar sized to the project's time signature. Dragging vertically changes the key within the visible keyboard range. These note operations rewrite only the affected length-prefixed score event and preserve other event bytes. Tempo edits are also written into the loaded project. The Plug-ins tab scans conventional VST locations on Windows, macOS, and Linux, plus directories listed in `VST3_PATH` and `VST2_PATH`. It can load a selected VST3 into the process, open its native editor, and expose host-reported parameters. For recognized VST project state, it reads the VST name, vendor, bundle path, and class UID, suggests a matching installed bundle, and can try restoring the FLP `0xD5` record. A successfully loaded project-matched VST3 is linked to its Channel Rack row, which displays the plug-in name and can reopen its native editor window. Full Image-Line wrapper and state conversion remain incomplete. The Mixer view lists recognized insert names and raw input/output/color fields; faders, effect slot state, and editable routing are still undecoded.

The desktop executable also accepts an `.flp` path as its first argument to open that project at startup.

`channels <file.flp>` reports each channel's raw kind, known type label, observed Layer child IDs/flags, sample source path, volume, and pan. Known type labels cover Sampler, Native, Layer, Instrument, and Automation; unrecognized raw values remain available. `mixer <file.flp>` reports recognized Mixer insert names and raw fields from the observed `0x9A`/`0x93`/`0x95` sequence, plus the count of fixed-size `0xE1` parameter records. It does not yet map those parameter records to faders, effect slots, or routing controls. `sample-paths <file.flp>` tries to resolve each reference using the project folder, configured factory roots, and common FL Studio install locations; it reports paths it cannot find. Set `FL_STUDIO_ROOT` or `FL_STUDIO_FACTORY_DATA` to select a custom installation. `audio-info <audio-file>` decodes WAV, OGG/Vorbis, FLAC, MP3, AIFF, and lossless mono/stereo WavPack media by content, including FL Studio's private RIFF wrapper around Ogg data. `render-audio-clips <project.flp> <output.wav>` makes an experimental stereo float render of enabled Playlist clips that target audio channels, using the base project tempo and observed source offsets. Clips with non-default scale are skipped and counted. The transport decodes each distinct source once, then mixes and streams the arrangement in bounded blocks at the device rate; it applies decoded audio-channel volume and pan with a provisional mapping based on observed raw defaults, which still needs comparison against native FL Studio output. Stop cancels playback and worker activity. Pattern instruments, tempo automation, sample voices, plug-ins, and Mixer effects are not part of this playback path yet.

`time-markers <file.flp>` lists arrangement time markers with positions, names, signature status, and meter values. It removes only the documented time-signature flag from the displayed position and prints the original raw dword so unknown bits remain visible. It reads marker events independently of clip decoding, so it can inspect projects whose Playlist clip layout is not yet understood. The decoder is read-only; marker editing and playback-clock behavior are not implemented yet.

In the desktop Piano roll, **Open MIDI…** loads a MIDI file, lets you choose a track, and imports its notes into the selected pattern and channel. The selected track's note timing is converted from MIDI PPQ to project PPQ. **Render WAV…** exports the selected pattern/channel through its loaded VST3 instance. **Preview VST3** processes that pattern in VST3-sized blocks on a worker and streams the stereo output through the selected device at its sample rate, using the same plug-in instance as the open editor. **Render audio…** exports the selected arrangement's enabled audio-channel clips. VST3 preview currently plays one selected pattern channel; Playlist scheduling, Mixer routing/effects, and guaranteed underrun-free processing are not implemented.

The project cannot yet be rendered as a complete song. `render-pattern-vst3` renders one decoded pattern channel through a selected installed VST3 instrument to a 32-bit float WAV, scheduling notes from the project's PPQ and tempo and appending an optional release tail (2 seconds by default). `render-audio-clips` separately mixes Playlist clips targeting enabled audio channels into a stereo float WAV, using their source windows, base tempo, and decoded channel volume/pan. No path combines pattern instruments, sampler voices, tempo automation, Playlist routing, and Mixer effects into one song render. These are early paths, not full FL Studio render parity. The reader exposes channel summaries (including observed audio-channel sample paths), patterns and notes, Playlist track names and arrangement clips, and opaque channel plug-in payloads; most other event meanings remain opaque. `playlist` accepts an optional starting clip and count. `plugin-states` reports channel plug-in payload sizes and event indices without decoding them. `edit-note` changes an existing note's start, length, key, and velocity while retaining its other note fields. `add-note` appends a 24-byte note record to a pattern; for an empty pattern it uses a note-event encoding observed elsewhere in the project and refuses to guess when encodings conflict. `delete-note` removes a channel-scoped note by zero-based index. These note operations rewrite only the affected score event and its length prefix. `edit-clip` changes an existing Playlist clip's position and length while retaining its other record bytes. `set-tempo` and `rename-channel` edit existing fields and preserve every unrelated event's original wire bytes. The CLI does not yet create missing patterns, channels, clips, or other project objects.

The Standard MIDI File reader recognizes the `MThd` header and `MTrk` event streams, including running-status channel messages, meta events, SysEx, and system messages. `midi-info` reports track names, timing, paired note events, and tempo-map spans. `import-midi` appends notes from one PPQ-timed MIDI track to an existing FLP pattern and channel, scaling note positions and lengths to the project's PPQ and preserving the FLP tempo and unrelated event bytes. Unclosed MIDI notes use the remaining track duration, or one beat if the track ends at the note start. SMPTE-timed MIDI, tempo-map conversion, and automatic channel/pattern creation are not implemented yet.

`scan <directory>` inventories `.flp` and `.fst` files recursively, grouping successfully parsed files by project version, header format, and PPQ while reporting any unparsed files. `plugin-scan` lists VST3 bundles and VST2 DLL candidates in the conventional Windows plug-in folders without loading their code. `plugin-state-preview` reports a channel's opaque wrapper and plug-in payload sizes and leading bytes. `vst3-state-probe` transiently loads a VST3 and offers the selected FLP channel's plug-in state to it through the host state interface; this is an experiment, not yet project-wide compatibility. In the current ZENOLOGY probe, the VST3 accepted a 164,850-byte FLP payload and produced a 163,256-byte host snapshot. These commands map installed compatibility inputs before deeper project and plug-in support is added.

The original application's MIDI export experiments and their measured results are recorded in [`docs/oracle-experiments.md`](docs/oracle-experiments.md). `midi-events` displays a selected MIDI track's decoded events, including tempo and marker metadata, for further format comparison.

## Channel levels

`channels <file.flp>` reports raw channel volume and pan values when present, along with observed sample source paths. The desktop Channel Rack exposes volume and pan sliders for channels with a valid modern `0xDB` Levels event. `set-channel-levels` edits those first two fields and preserves the remaining event payload. Older byte and word controls are decoded for inspection but remain read-only until their scaling is verified. Playlist audio rendering applies a provisional volume/pan mapping based on observed raw defaults; native FL Studio output has not yet been used to verify the curve.

## Layer children

Layer child relationships and raw flags are available in the project model. The desktop Channel Rack exposes a child selector with channel names and IDs; `set-layer-children` replaces a Layer channel's repeated `0x5E` references while retaining other channel events. Pass comma-separated child IDs or `-` to clear the list. Layer playback and flag editing remain incomplete.

## Mixer

The `mixer <file.flp>` command reports recognized insert fields and counts known `0xE1` parameter IDs. The desktop Mixer view lists recognized inserts and raw route fields; existing names can be edited through their `0xCC` events while retaining the project's string encoding and unrelated bytes. The project API can edit the signed value of an existing `0xE1` record by event and record index. Fader controls, effect slots, and routing remain unmapped.

## Automation points

`automation <file.flp>` lists point curves from type-5 automation channels. `edit-automation-point` changes an existing point's position in beats, normalized value, and tension. `insert-automation-point` inserts at a zero-based slot (including the slot after the last point), and `delete-automation-point` removes a point by zero-based index. These operations preserve existing point tails and the blob's header and era trailer; new points use a zeroed opaque tail. They require a channel with an existing `0xEA` point blob. The desktop Automation view draws a type-5 curve and supports point selection, drag and numeric editing, insertion, and deletion. Its current preview uses straight segments and does not render tension/interpolation modes. Parameter links, event automation, automation-clip creation, and FL Studio's window-level curve workflow remain incomplete.

## Automated checks and installers

GitHub Actions runs formatting checks, Clippy, and the Rust test suite on Ubuntu, macOS, and Windows for pushes, pull requests, and manual runs. Each platform job also builds a native desktop package with Cargo Packager and uploads it as a workflow artifact: Windows NSIS installer, macOS DMG and app bundle, and Linux Debian, AppImage, and pacman packages. The package definitions are in [`Packager.toml`](Packager.toml).

## Compatibility plan

1. Project containers: broaden FLP envelope compatibility across releases, add semantic FST preset support, and handle ZIP project packages.
2. Musical model: broaden pattern and note editing, then decode channel and plugin state, Playlist clip variants, automation, mixer routing, and audio-channel clip state, while retaining unsupported fields.
3. Interchange: MIDI import/export and audio export with stable timing and channel mapping.
4. Playback: connect the shared/exclusive live device engine to transport, tempo clock, sample scheduling, mixing, and automation.
5. Instruments and effects: continue VST3 hosting from the current discovery, native editor window, parameter-list prototype, and single-pattern offline render into project-state restore, automation, MIDI/audio routing, Mixer processing, and whole-project rendering. Add VST2 hosting if an applicable legacy licensing path is available. Preserve plug-in identity and opaque state when a plug-in is unavailable. Rebuilding FL's bundled plug-in DSP is out of scope.
6. User experience: project browser, Playlist, piano roll, channel rack, mixer, plugin windows, editing tools, shortcuts, and accessibility.
7. Historical compatibility: fixture-driven support across older FLP versions, with explicit reporting where a project depends on unavailable plugins or media.

Full feature parity is a long-running objective. Each milestone should keep unsupported project data intact so opening and saving a project does not silently destroy state. Installed plug-ins are treated as external instruments/effects to host, rather than DSP to reimplement.

## Product references

- [FL Studio project file format](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_save_flp.htm)
- [FL Studio state file format](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_other_fst.htm)
- [Project files and ZIP project packages](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/browser.htm)
