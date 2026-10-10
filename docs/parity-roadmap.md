# FL Studio Full Parity & Compatibility Roadmap

What "done" means: open any `.flp` from any FL Studio version, play it back
sounding identical, edit it with the same tools, and save it back without data
loss. This document enumerates everything that requires, grouped by subsystem,
with the current status of this project against each item.

Status legend:

- `[done]` — implemented and working
- `[partial]` — started; works for a subset of cases
- `[todo]` — not started
- `[obsolete]` — removed from current FL Studio; not a current parity target

> Scope note: this covers the desktop DAW. FL Cloud, FL Studio Mobile, and
> online services are explicitly out of scope (see Non-goals).

---

## 1. File format compatibility

The foundation. Everything else depends on reading projects exactly.

### 1.1 FLP project files

| Item | Status | Notes |
|---|---|---|
| Chunk envelope (`FLhd` / `FLdt`) parsing | `[done]` | |
| Event stream decoding (byte/word/dword/data encodings) | `[done]` | |
| Lossless round-trip (unknown events preserved byte-exact) | `[done]` | Core project invariant: never corrupt what you don't understand |
| FLP versions 1.x–26.x (FruityLoops era through FL Studio 26) | `[partial]` | Modern versions tested; legacy header variants and old PPQ conventions need corpus testing |
| Header fields: PPQ, tempo, time signature, project metadata | `[partial]` | Tempo, the project-wide time signature, build number, and Project Info strings are decoded. The project signature is editable in Project Settings and through the CLI; Pattern and Playlist signatures are separately scoped and editable in the Piano roll, Playlist, and CLI. Master pitch, recording options, and broader version validation remain unmapped. |
| Project Info: title, author, comments, genre, web link | `[done]` | Read/write through the desktop Project Info dialog and CLI. Metadata events are edited without rewriting unrelated event bytes. |
| Channel records: all types (sampler, generator, layer, MIDI out, automation clip) | `[partial]` | Known channel kind values 0/2/3/4/5 are named while unknown raw values are retained; `0x00` enabled state is decoded and can be toggled from the Channel Rack by editing the existing event or inserting it after `0x15`, preserving unrelated channel events. `0x0F` compact state is decoded and editable through the API and UI while preserving unrelated channel events. The signed-byte Mixer track assignment (`0x16`) is decoded and used for stable Mixer-track sorting, with generator and Layer channels first. Display group names (`0xE7`) and signed group numbers (`0x91`) are decoded; All, Unsorted, and named groups can be filtered, selected channels can be assigned to an existing or newly created group, and groups can be added empty, renamed, and deleted while unassigning their channels and reindexing later groups. Muted state is honored by supported Sampler, audio-channel, and loaded VST3 song playback/rendering. `0x80` channel colors are decoded and can be edited from the Channel Rack or CLI and sorted by hue; the fourth byte is retained. `0xC4` sample paths are decoded for kind-0 Sampler and kind-4 sample-backed channels; Layer child IDs (`0x5E`) and raw flags (`0x90`) are decoded, and child lists can be edited in the Channel Rack through named channel choices while preserving other channel events. The observed Random and Crossfade bits can be edited in the API, CLI, and Channel Rack without changing other flag bits. Pattern notes on a Layer route to enabled Sampler and mapped VST3 children; Random chooses one child with a stable per-note pseudo-random choice. Native Random sequencing, Sequential mode, key/velocity ranges, Layer level/pan, Crossfade, and legacy channel types remain incomplete. |
| Pattern/score events: note records, all encodings | `[partial]` | 24-byte records handled; empty-pattern and conflicting-encoding edge cases guarded. Pattern-scoped time-signature markers are read, set, and deleted by tick from the API, CLI, and Piano roll; signature changes draw meter-aware bar divisions. Note/controller variants, native save verification, and remaining Pattern metadata remain incomplete. |
| Playlist events: clips, tracks, arrangements | `[partial]` | Clip position/length and raw item/track/group/flags, offsets, and established scale field are editable while preserving other record bytes; exact-record clip duplication is available in the API, CLI, and Playlist editor; the API and Playlist can create a Pattern Clip by copying a recognized clip record and changing only its established position, target, length, and track fields; selected clips can be deleted by removing only their record; Playlist clips can be copied, cut, and pasted as complete records; un-stretched Audio Clips with a linear source window can be split at a tick in the API, CLI, and Playlist context menu while retaining their sample source window, with non-default scale and Playlist tempo automation rejected; compatible adjacent Audio Clips can be joined through the API, CLI, and Playlist context menu when their timeline and sample windows meet and their track, clip properties, record layout, and event match; selected Pattern Clips can be merged into a new pattern through the API, CLI, and Playlist editor, with the combined score assigned to the uppermost selected clip and original patterns retained; pattern event automation, unknown per-pattern data, and non-default clip scale are rejected; the API, CLI, and Playlist toolbar Slip tool can slide an Audio Clip's source window while keeping its timeline bounds, with a loaded source duration required and non-default scale or Playlist tempo automation rejected; clips can be dragged between positions/tracks and Shift-dragged at the right edge to resize, with 1/16-step snapping or Alt for free movement. All three record sizes (80/60/32) are recognized, with structural inference when the stored version tag disagrees with the record width. Creating a first Pattern Clip through the template-based action still requires an existing clip record; clipboard paste can populate an empty arrangement. |
| Mixer state events | `[partial]` | Reads insert fields and raw `0xE1` records, recognizes candidate volume/pan/EQ parameter IDs and target bits, and can edit an existing record's value by exact record index in the API and Mixer inspector. Insert-name editing is available. Target-to-visible-track mapping, fader UI, FX state, and routing remain incomplete. |
| Automation events (channel envelopes, event automation) | `[partial]` | Reads type-5 channel automation points from `0xEA` blobs and edits, inserts, or removes points while preserving unmodified point bytes and the opaque header and era trailer. Song MIDI export samples linear type-5 automation on a Playlist clip targeting a channel named `TEMPO` once per tick and restores the project tempo after the clip. The desktop Automation view uses a straight-line preview; native curve tension, realtime tempo playback, event automation, target links, automation-blob creation, and clip creation remain incomplete. |
| Time markers, song position markers | `[partial]` | Reads arrangement-scoped and Pattern-scoped marker positions, names, and signatures. The project API and CLI can edit/create/delete Playlist markers; the Playlist editor exposes those operations. Pattern signatures can be set/deleted by Pattern ID and tick in the API and CLI, and added/edited/deleted from the Piano roll; the Pattern grid draws meter-aware bar divisions. The Playlist playhead follows device-consumed frames at the base tempo; seeking to markers, tempo-automation timing, signature-aware Playlist bar numbering, and controlled native-save verification remain incomplete. |
| Project settings: time signature, swing, pan law, master pitch, metronome, recording settings | `[partial]` | The Project model, CLI, and desktop dialog read and edit the project-wide time signature from `0x11`/`0x12` byte events; Pattern and Playlist signatures are separately scoped. Pattern signatures are set/deleted by Pattern ID and tick in the CLI, and managed in the Piano roll with meter-aware grid divisions. The same surfaces read and edit the FL Studio 26 Advanced options `Play truncated notes in clips` and `Fast declick for cut groups`. Project pan law is read and editable through the same surfaces (`0x17`, Circular `0` default, Triangular `2`); unknown raw bytes are retained. Global swing and the per-channel multiplier are editable and applied to aligned 16th-step onsets in Sampler and VST3 Pattern playback. The one-third-step maximum is inferred from triplet placement and needs native render comparison; note-length changes and other swing grids remain. Master pitch, metronome, recording settings, and other controls remain unmapped. |
| `set-tempo`, `rename-channel`, note add/edit/delete, clip edit, channel levels | `[done]` | CLI surface; each rewrites only affected bytes |
| Create missing objects (patterns, channels, clips) | `[partial]` | Creates an empty pattern through the desktop control or CLI when a unique existing `0xD0`/`0xE0` note-event encoding is available; duplicates a pattern's raw note-event payload plus its established name and explicit length; creates a Sampler channel from a Browser audio drop with a unique ID and inferred project string encoding; duplicates existing Playlist clips by copying their complete records, and supports clip copy/cut/paste; and creates a selected Pattern Clip from a recognized Playlist record template. Pattern automation, unrecognized pattern metadata, ordering immediately after the source pattern, other new channel types, creating a new Pattern Clip without a record template, targetless clips, unsupported/ambiguous layouts, and native FL Studio validation remain outstanding. |

### 1.2 Presets and packages

| Item | Status | Notes |
|---|---|---|
| `.fst` state preset reading (envelope level) | `[partial]` | Accepted by the lossless chunk/event reader. Browser details expand decoded channel names/kinds/plug-in IDs/sample paths, recognized plug-in identity and state-byte metadata, raw Mixer insert fields, and automation point counts. Plug-in payload contents remain opaque. |
| `.fst` generator vs effect vs mixer-state variants | `[partial]` | `FstPreset` classifies header formats 32 (channel), 48 (native plug-in), 49 (VST generator), 50 (VST effect), and 64 (Mixer insert); format 24 is identified as automation state. `preset-info` reports the kind and decoded event/channel/insert counts while preserving the complete source stream. Applying presets to hosted plug-ins, interpreting opaque state payloads, and native FL Studio comparison remain outstanding. |
| Zipped project packages (`.zip` with bundled samples) | `[partial]` | The desktop app opens standard ZIP packages, prefers a root-level FLP when present, extracts regular files into a temporary workspace for relative sample lookup, and writes edits back while retaining the other files. ZIP saves copy resolvable Sampler and audio-channel sample references into `Samples/`, rewrite those channel paths to package-relative references, and report unresolved paths. Plugin-internal samples and archive metadata, encrypted entries, and native FL Studio validation remain outstanding. |
| `.flp` "save as" version targeting | `[todo]` | Writing files older FL versions can open |

### 1.3 MIDI files

| Item | Status | Notes |
|---|---|---|
| SMF read: MThd/MTrk, running status, meta, SysEx | `[done]` | |
| MIDI import into pattern/channel with PPQ conversion | `[done]` | |
| SMPTE-timed MIDI | `[done]` | Imports supported -24, -25, drop-frame -29.97, and -30 frame clocks by converting absolute elapsed time into the existing project's tempo/PPQ grid; invalid frame codes and zero ticks-per-frame are rejected. |
| Tempo-map conversion on import | `[partial]` | PPQ source tempo changes and SMPTE elapsed time are converted to note positions at the existing FLP tempo, preserving note timing while leaving project tempo unchanged. Source tempo-map semantics are baked into note positions rather than imported as editable FL tempo automation. |
| MIDI export (File > Export > MIDI) | `[partial]` | Exports pattern notes and arrangement Pattern Clips as SMF format 1 with project PPQ, base tempo, time signature, named channel tracks, measure-aligned inferred pattern repeats, clip-edge note truncation, and arrangement time/signature markers on the conductor track. NewStuff's five instrument note counts match the prepared FL Studio export. Song export samples linear `TEMPO` automation once per tick and restores project tempo after its clip; a direct export matched event ticks, with up to 9 microseconds-per-quarter differences and three fewer redundant tempo events. Other native export modes/layouts, scaled clips, and note properties beyond key/velocity/channel remain. |
| MIDI export options: pattern vs song, channel mapping | `[partial]` | The CLI and desktop expose pattern or arrangement export; channel mapping can preserve each note's stored low four channel bits or assign one MIDI channel per FL channel. Full native export modes and channel mapping behavior remain to be compared against FL Studio. |

---

## 2. Project model (in-memory representation)

The decoded document must model everything the format can express.

- `[partial]` Channels: summaries, sample paths, plugin state blobs, levels, known kind mapping, and Layer child relationships/raw flags
- `[partial]` Patterns: note lists per channel; empty patterns can be created when the project supplies an unambiguous note-event encoding, and Alt/Option+C or Patterns > Duplicate selected pattern copies the selected pattern's raw note-event payload plus recognized name and explicit length. Pattern-scoped time-signature markers are read, set, and deleted. Pattern automation and other unrecognized pattern metadata remain opaque.
- `[partial]` Playlist: tracks, arrangements, clips with targets, and time markers that can be edited, created, and deleted through the API, CLI, and desktop editor
- `[partial]` Mixer: recognized insert summaries and parameter kinds, with exact-record value editing and insert-name editing; full 125 inserts, master/sends, target mapping, fader UI, effects, and routing remain incomplete
- `[partial]` Automation: type-5 channel point curves can be read and points in an existing blob edited, inserted, or removed from the desktop view; linear tempo-map conversion is implemented for song MIDI export, while native curve interpolation, realtime tempo playback, parameter links, event automation, LFOs, and new clip creation remain incomplete
- `[partial]` Tempo automation: linear channel curves named `TEMPO` are sampled for MIDI song export; realtime playback and native tension/easing semantics remain
- `[partial]` Channel colors: the model, CLI, and Channel Rack read/write per-channel RGB values and sort by hue; channel groups, icons, gradient coloring, selection-wide coloring, and native comparison remain
- `[partial]` Swing / groove settings: reads and edits the project-level `0x0B` byte mix (0–128, default 0) and `0x61` word multiplier (0–128, default 128); Sampler pattern previews, Playlist Pattern Clips, and VST3 pattern render/preview shift aligned even-numbered 16th-step onsets by the combined global and source-channel mix. The maximum one-third-step delay is inferred, so native render comparison, note-length behavior, swing-grid selection, and realtime tempo-automation interaction remain
- `[partial]` Project settings: the project-wide time signature (`0x11`/`0x12`) is read and editable through the model, CLI, and desktop dialog; Playlist and Pattern signatures remain distinct. The two verified FL Studio 26 Advanced options `Play truncated notes in clips` and `Fast declick for cut groups` can also be read and edited; pan law (`0x17`, Circular `0` default and Triangular `2`) is read and editable through the same surfaces. Other project-wide settings remain unmapped
- `[partial]` Layer channels route notes to enabled Sampler and mapped VST3 children, including a stable per-note pseudo-random choice for Random; native random sequencing, Sequential mode, keyboard/velocity splits, Layer level/pan, and Crossfade remain incomplete
- `[done]` Project info: title, author, comments, genre, and web link; the Project Info dialog and CLI can edit these fields.

---

## 3. Audio engine

The hardest subsystem. See the difficulty discussion in chat history: this is
where real-time constraints punish sloppy code.

### 3.1 Device I/O

| Item | Status | Notes |
|---|---|---|
| Windows: WASAPI (shared + exclusive), DirectSound, ASIO | `[partial]` | Shared capture/output through CPAL and direct WASAPI exclusive streams are implemented, including endpoint selection, rate/buffer settings, input metering/monitoring, an output test tone, and one bounded Playlist stream that mixes audio clips, Sampler notes, and project-matched VST3 instruments on Pattern Clips using provisional volume/pan mapping. Opening a project now auto-loads channels whose VST3 identity matches an installed bundle; marker-12 state restoration is attempted, with unsupported state layouts reported while the loaded instance remains mapped for playback. VST3 Song output is processed in worker blocks through the same instances used by open editors and resampled to the selected device rate. A 9,600-frame excerpt rendered from `Ookay - Thief.flp` was consumed by both shared and exclusive output on the current machine. A shared-mode input smoke run opened the current Focusrite endpoints; capture reported one startup discontinuity and then stayed active without further errors for the remainder of the 3-second check. These verify current hardware paths, not all endpoints or formats. DirectSound and ASIO are not implemented. |
| macOS: CoreAudio | `[partial]` | The shared CPAL path uses the platform's default audio host and supports block-streamed Playlist audio; hardware behavior is not verified yet. |
| Linux: ALSA, PulseAudio/PipeWire, JACK | `[partial]` | The shared CPAL path uses the platform's default audio host and supports block-streamed Playlist audio. Cross-platform build/test CI exists; backend and hardware behavior are not verified yet. |
| Device enumeration, sample-rate / buffer-size negotiation | `[partial]` | CPAL lists endpoints and opens selected/default devices at the requested sample rate and buffer size. Shared mode falls back to the backend's default buffer when a fixed size is rejected; complete capability negotiation is still needed. |
| Recommended starting point: `cpal` crate for cross-platform bring-up | `[done]` | CPAL is used for shared-mode input and output on desktop platforms. |

### 3.2 Real-time thread

- `[partial]` Lock-free audio callback: shared CPAL callbacks avoid allocation and locks; the WASAPI exclusive output event loop currently allocates a buffer for each event
- `[todo]` Lock-free command queue (UI thread → audio thread): transport, parameter changes, note events
- `[partial]` Lock-free metering/state queue (audio thread → UI thread): input peak is published through an atomic value; a general state queue is not implemented
- `[partial]` Sample-accurate event scheduling within a buffer: selected-pattern previews and Playlist Pattern Clip Sampler notes schedule at device-frame offsets, while VST3 notes are timestamped in plug-in sample frames before resampling to the device rate; Layer Pattern notes route to enabled Sampler and mapped VST3 child channels, with Random making a stable pseudo-random choice per placed note. Project opening loads matching installed VST3 channel instances and attempts marker-12 component/controller restore from nested field-53 records. Native Random sequence equivalence, other wrapper/state layouts, a shared transport command queue, and automation scheduling are not implemented
- `[partial]` Underrun/dropout detection and reporting: shared stream and exclusive worker errors are counted and retained in a bounded 16-message history shown in Audio settings; the current streamed-render producer underrun frame count is also exposed. Hardware xrun detection and automatic recovery remain incomplete.
- `[partial]` Denormal protection: DAW-owned x86-64 audio, render, and VST3 processing threads enable MXCSR flush-to-zero/denormals-are-zero; AArch64 enables FPCR flush-to-zero. Threads created and managed internally by plug-ins remain outside the host's control ([Intel FTZ/DAZ guidance](https://www.intel.com/content/www/us/en/docs/dpcpp-cpp-compiler/developer-guide-reference/2023-0/set-the-ftz-and-daz-flags.html), [Arm FPCR definition](https://documentation-service.arm.com/static/64a3e7fcdf6cd61d528c478f)).

### 3.3 Mixer graph

- `[todo]` 125 insert tracks + master, each with level/pan/mute/solo
- `[todo]` Per-track FX chains (10 slots each)
- `[todo]` Sends and sidechain routing (arbitrary track-to-track routing)
- `[todo]` Plugin delay compensation (PDC): per-plugin latency measurement, graph-wide delay alignment, manual + automatic modes
- `[todo]` 32-bit float internal processing throughout
- `[todo]` Multi-threaded rendering: parallelize independent mixer tracks across cores

### 3.4 Sample playback

- `[partial]` Offline: WAV/OGG/FLAC/MP3/AIFF/WavPack decode by content (symphonia + wavicle)
- `[partial]` Playlist playback: enabled audio-channel clips, enabled kind-0 Sampler notes, and notes for mapped VST3 instrument channels are mixed in bounded worker blocks at the device rate. Opening a project auto-loads a matching installed VST3 for recognized channel identities and attempts supported marker-12 state restoration; unavailable or unrecognized channels remain unloaded and are reported. Pattern Clips schedule plugin MIDI at absolute arrangement ticks and apply global/channel swing to aligned 16th-step onsets; mono/stereo plugin output is resampled and mixed with audio and Sampler sources. The swing maximum, raw level mapping, and pattern repeat/edge behavior remain provisional and need comparison against native FL Studio output. Non-default time stretch, automation, routing, clip-state flags, Mixer effects, and plugin delay compensation are unsupported.
- `[partial]` Real-time sampler voice management: selected-pattern preview and Playlist Pattern Clips use a bounded voice pool with oldest-voice stealing, zero-length one-shot notes, and a short release ramp for keyed notes. Pattern Clips place notes at clip offsets, repeat by explicit pattern length (or a note-length inference when absent), and bound keyed notes at clip ends. FL envelope, loop, root-key, and polyphony settings are not decoded; volume/pan mapping and repeat behavior still need comparison against native output
- `[partial]` Resampling (project rate vs sample rate vs device rate): linear interpolation handles input/output rate differences and note-key transposition around an assumed MIDI-60 root. Full Playlist rendering also offers a 64-tap Blackman-windowed sinc mode for pitched/downsampled material; pitch-root metadata, FL Studio's 6-point Hermite mode, and an exact match for its selectable sinc kernels remain
- `[done]` FL's private RIFF-wrapped Ogg handling in realtime path: the content-sniffing decoder finds the embedded Ogg stream and is used by Playlist and Sampler preparation before device playback streams the decoded audio in bounded blocks.
- `[todo]` Reverse playback, ping-pong loop modes

### 3.5 Recording

- `[todo]` Audio recording into playlist (per-track input selection)
- `[todo]` Edison-class audio editor: record, trim, spectral view, scripting
- `[partial]` Latency-compensated input monitoring: shared and WASAPI-exclusive input monitoring paths exist, but they are not latency compensated.

---

## 4. Native (stock) plugins

**Project decision (2026-10-06): stock plugin DSP is out of scope.** This
project will not reimplement Image-Line's instruments and effects. The
compatibility strategy for every stock plugin is a single one:

- **Host the original.** If the user has FL Studio (or the individual plugin)
  installed, load it like any other VST and restore its state from the FLP
  blob. A project opens and sounds right when the plugins are present;
  missing plugins are reported, not faked.

This removes the entire reimplementation surface (~110 plugins of proprietary
DSP: Sytrus, Harmor, FLEX, Gross Beat, Newtone, etc.). It also sidesteps the
legal risk of cloning trade-secret algorithms — hosting a plugin the user
legitimately owns is the standard DAW interop story.

What this still requires (tracked under §5, plugin hosting):

- Reliable VST2/VST3 state save/restore for Image-Line's own plugins, which
  use the same wrapper records as third-party plugins
- Graceful handling when a stock plugin isn't installed: show the channel,
  its settings blob size, and which plugin is missing — never silently drop
  the channel or corrupt the project
- `.fst` preset loading for stock plugins (envelope already readable;
  interpretation per plugin unnecessary since the plugin itself parses it)

No per-plugin DSP work is planned. If a stock plugin's state format ever needs
interpretation without the plugin present, that becomes a case-by-case
decision, not a roadmap item.

---

## 5. Third-party plugin hosting

| Item | Status | Notes |
|---|---|---|
| VST3 hosting: load, process, native editor | `[partial]` | In-process load and editor open work. Opening a project auto-loads each channel with a recognized VST3 identity that matches an installed bundle, and maps successfully loaded instances to that channel for playback. Selected-pattern preview and Playlist Song transport process loaded instruments blockwise on a worker and stream through shared or WASAPI-exclusive output; the open editor and audio worker use the same loaded instance. Playlist Pattern Clips expand notes for mapped VST3 channels, including Layer child routing and the same Random child choice used by the Sampler renderer; multiple instruments are mixed with audio clips and Samplers through the bounded stream. Matching is currently based on wrapper name or bundle name; plug-in crashes can still take down the app. Mixer graph processing, automation, PDC, multibus output, process isolation, and native audio comparison remain incomplete. |
| VST3 state save/restore via FLP `0xD5` record | `[partial]` | Marker-12 records with nested field-53 version 1 are converted to the host's component/controller snapshot and attempted automatically when a matching project channel is loaded. A local ZENOLOGY experiment verified that the restored component stream matches FLP record 3 byte-for-byte. Other wrapper markers, nested state layouts, state write-back, and plugin-level/audio comparison remain unverified; if restore fails, the matched instance remains mapped with its default state and the issue is reported. |
| VST3 parameter automation | `[todo]` | |
| VST2 hosting | `[todo]` | Distribution of a VST2 host binary requires an applicable legacy Steinberg agreement; no such path is confirmed. Public source must not include Steinberg's VST2 SDK headers. |
| CLAP hosting | `[todo]` | FL Studio supports CLAP as of recent versions |
| AU hosting (macOS) | `[todo]` | |
| 32-bit plugin bridging on 64-bit host | `[todo]` | Out-of-process sandbox + IPC |
| Plugin crash isolation (sandboxed scanning) | `[todo]` | A crashing plugin must not take down the DAW |
| Latency reporting per plugin (feeds PDC) | `[todo]` | |

---

## 6. Piano roll

Current: note create/drag/resize/snap, velocity editing, Pattern-scoped time-signature editing with bar divisions that follow meter changes, and a selected-note
inspector for raw flags/group, the per-note slide flag, fine pitch, release,
stored MIDI channel, pan, and modulation X/Y. Quantize has channel and selected-note actions; Legato,
Chop, Glue, Flip, Strum, Flam, seeded Randomize and Humanize, pitch Limit,
Arpeggiate, and Slice can target the channel or selected notes. Scale highlighting, chord labels, ghost
channels, and MIDI-channel note colors are available in the Piano roll. The
CLI can edit score fields individually while preserving the reserved byte.
FL Studio's piano roll is famously deep — the full toolset:

**Tools:** `[partial]` Draw (P), Paint (B), Select (E), Zoom (Z), Playback (Y), Chord Stamp, and Slice-at-tick; single-note delete, modifier and box selection, Select All (Ctrl/Cmd+A), Invert (Shift+I), Deselect (Ctrl/Cmd+D), duplicate-to-right (Ctrl/Cmd+B) for selected notes or all notes in the target channel, Shift+G group and Alt/Option+G ungroup, and group-aware move/resize/delete. Duplicate spacing follows the copied notes' time span; timeline-defined repeat intervals remain unsupported. Playback auditions Sampler or loaded VST3 notes on click and while dragging across notes; native instrument preview and continuous playhead scrubbing remain. Mute remains `[todo]`.

**Edit operations:**
- `[partial]` Quantize note starts on a selected snap grid with strength and swing, either channel-wide or on the selected notes; native swing semantics remain
- `[partial]` Legato extends notes to the next distinct onset, Chop splits eligible notes into equal segments, and Glue joins touching/overlapping notes with matching properties; channel and selected-note scopes are available, while native tool options remain
- `[partial]` Score flipper mirrors a channel around its latest note end or selected notes within their time bounds; native tool options remain
- `[partial]` Strum staggers simultaneous notes by pitch across a configurable tick spread within the channel or selection; native velocity and chord-overlap options remain
- `[partial]` Flam adds a configurable short stroke before or after channel or selected notes; tempo-based time, presets, and grouping remain
- `[partial]` Randomize applies seeded velocity, pan, and pitch changes with directional or bipolar level offsets and optional default-level reset to the channel or selection; scale-aware note generation remains
- `[partial]` Humanize applies seeded timing and velocity variation to the channel or selection; tempo-relative timing remains
- `[partial]` Arpeggiator converts selected same-onset chords to gated up/down runs with configurable step time and octave range; custom score patterns and sync modes remain
- `[partial]` Riff Machine uses the channel or selection as a progression, creates scale triads, arpeggiates them, supports horizontal time or onset-preserving pitch reversal and vertical pitch mirroring, applies seeded velocity variation and note-length scaling, and fits pitches to the selected scale/range ([Image-Line Riff Machine manual](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/pianoroll_riff.htm), [Mirror controls](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/pianoroll_riff_mirror.htm)); progression presets, groove, custom patterns, staged preview, exact native mirror semantics, and native voicing remain
- `[partial]` Claw machine exposes Period, Trash every, Time distortion, Remove short notes, and Stretch to compensate for the channel or selection, using a local 16-slice gate and timing curve; native note splitting and exact slice, distortion, and short-note behavior remain unverified
- `[partial]` Limit folds or clamps channel or selected note pitches into a key range, supports wrapping to the lowest octave, and can snap out-of-scale notes to the current Piano roll scale upward, downward, or alternately; exact alternate-snap edge behavior remains
- `[partial]` Slice-at-tick splits notes crossing the chosen tick in the channel or selection; native directional cut gestures remain
- `[partial]` Scale Levels applies Center, logarithmic Tension, Multiply, and Offset to selected notes or the target channel; Center and Tension use documented ranges with local formulas pending comparison against native FL Studio
- `[partial]` Articulate scales original note lengths or derives legato boundaries from the next onset, with selected-only context, seeded Variation, next-onset chord chopping, and quick Legato/Portato/Staccato/small-gap/chord-chop presets; exact native variation distribution and preset values remain unverified
- `[partial]` The Piano roll displays the observed per-note slide flag as a start marker and can toggle it while preserving other flag bits; native-instrument slide playback, portamento flag encoding, and portamento playback remain unsupported
- `[partial]` Ghost channels show notes from all channels in the selected pattern with the target channel emphasized; independent ghost editing controls remain
- `[partial]` Notes can be colored by their stored MIDI channel; native note color groups and color-based selection remain
- `[partial]` Chord Stamp supports common manual chords and scale-derived triads/sevenths, uses the snap length, and can return to Draw after one stamp; automatic top-down/bottom-up voicing, voice leading, percussion/slide presets, and chord preview remain
- `[partial]` The integrated Event editor shows and edits per-note velocity, pan, release, fine pitch, Mod X, and Mod Y values as draggable stems; target selection is available in the panel, with Shift+F cycling targets. Pattern-scoped controller automation, interpolation, selection editing for coincident notes, and native scale/target behavior remain incomplete
- `[partial]` Scale highlighting supports major, natural/harmonic minor, and major/minor pentatonic keys; the selected-note inspector identifies common same-onset chords; automatic scale detection and richer chord analysis remain

---

## 7. Step sequencer / Channel rack

- `[partial]` Channel list with plugin names, volume/pan sliders, per-channel color swatches and picker backed by `0x80`, per-channel mute buttons backed by `0x00`, Ctrl/Cmd-toggle and Shift-range multi-selection, double-click select-all, context-menu selection and batch mute/unmute, Zip/Unzip selected and Unzip all actions (Alt/Option+Z zips selected channels and Alt/Option+U unzips all), Group selected (Alt/Option+G) to create or move channels into named groups, an All/Unsorted/named-group display filter, up/down reordering that moves complete channel event blocks without changing IDs, stable color/name/type/Mixer-track sorting, and editable Layer child IDs
- `[partial]` Pattern/bar step grid toggles notes at sixteenth-note positions; new steps use key 60 and velocity 100. Zipped rows keep these step cells inline while hiding secondary channel controls. The Ctrl+K Graph Editor draws and edits per-step note key, velocity, pan, release, fine pitch, modulation X/Y, and shift, creates a C5 note on an empty step, interpolates values when right-dragging across steps, shifts all active lane values together on Ctrl-drag, and resets active values to lane defaults on Alt/Option-click or drag. The Rep lane and multiple notes at one step remain incomplete
- `[partial]` Zipped Channel Rack state is decoded from channel event `0x0F` and can be edited losslessly in the API and UI. The Channels menu supports Zip selected, Unzip selected, Unzip all, and Group selected; Alt/Option+Z zips selected channels, Alt/Option+U unzips all, and Alt/Option+G opens the group assignment dialog. Zipped rows keep their step cells inline and hide secondary controls. Mixer-track sorting uses the observed `0x16` assignment, keeps generators and Layers first, and orders the remaining channels by known route. All, Unsorted, and named groups are available in the display filter, with right-click actions to add, rename, and delete groups and Page Up/Down to switch named groups.
- `[partial]` The Channel Rack header's Swing menu edits the global `0x0B` mix, and right-clicking a channel name opens its Piano roll for the selected pattern; the row Swing submenu edits the `0x61` multiplier for Sampler and Native instrument (generator) channels. Both menus offer FL-style 0/25/50/75/100% presets and a custom percentage slider. Supported Pattern playback applies the two multipliers on aligned 16th-step onsets; native offset, note-length behavior, and swing grid selection remain incomplete.
- `[obsolete]` Separate Keyboard editor window: the current Channel Rack uses its integrated Graph Editor and Step/Piano roll view, and directs melodic entry to the Piano roll; Image-Line's forum response says the old standalone editor windows were removed ([current Channel Rack manual](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/channelrack.htm), [Image-Line forum response](https://forum.image-line.com/viewtopic.php?p=1927605))

---

## 8. Playlist

- `[partial]` Clip display, clip select, start/length and exposed record-field editing, exact-record duplicate and delete, and selected-pattern clip creation from an existing record template
- `[partial]` Audio-channel clips draw decoded min/max waveforms or a bounded log-frequency spectral preview, crop them to the clip's source offsets, and redraw detail at the current Playlist zoom. Previews decode off the UI thread and cache combined/per-channel peak buckets plus 128 columns of 32-band spectral intensity; Combined, Stereo, and Spectral views are available. The local windowed FFT display does not reproduce FL Studio's exact spectral coloring or analysis, while fade/gain previews and alternate stretch-mode rendering remain
- `[todo]` Audio clip fades, crossfades, gain envelopes
- `[todo]` Stretch modes per clip (resample, stretch, e3 generic — needs time-stretch engine)
- `[partial]` Pattern clips: the selected arrangement expands Sampler and mapped VST3 instrument notes at clip positions, routes Layer notes to enabled supported children, repeats notes to clip length using explicit pattern lengths or an inferred note span, and clips keyed-note ends at the Playlist boundary. **New Pattern Clip** adds the selected pattern at the arrangement end, using its explicit length, note span, or one measure as a fallback; it copies an existing clip record and changes only established position, target, length, and track fields. Selected Pattern Clips can be merged into a new pattern with Ctrl/Cmd+G or the context menu; the merged clip spans the selected timeline range, expands score repeats to clip edges, uses the uppermost selected clip as the receiver, and leaves source patterns intact. Pattern event automation, unknown per-pattern data, non-default clip scale, and note expansion above two million notes are rejected. The new action follows FL Studio's Playlist merge behavior ([Playlist manual](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/playlist.htm), [Piano roll manual](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/pianoroll.htm)). The action is unavailable when the arrangement has no clip record to use as a template. Layer Random selects one child by a stable pseudo-random per-note choice shared by the Sampler and VST3 renderers; native Random sequence, Sequential mode, key/velocity ranges, Layer volume/pan, and Crossfade remain unsupported. Project opening auto-loads matching installed VST3 instances for recognized channels; missing matches and failed loads are reported. Automation clips, clip flags, and non-default scale remain unsupported.
- `[partial]` Time markers and meter records can be read, listed, edited, created, and deleted through the API, CLI, and desktop editor; the Playlist playhead follows device-consumed frames at base tempo, while seeking, tempo-automation timing, and signature-change bar numbering remain incomplete
- `[todo]` Track grouping, mute/solo per playlist track
- `[partial]` Duplicate/copy of a selected clip preserves its full record; clipboard copy/cut/paste retains the complete source record and changes only position and track on paste; right-click or Delete/Backspace removes the selected clip's full record while retaining adjacent clips. The API, CLI, and Playlist context menu can split un-stretched Audio Clips at a tick while retaining the sample source window; non-default scale and Playlist tempo automation are rejected, and a full-source clip requires its sample duration. Compatible adjacent Audio Clips can be joined by the API, CLI, and Playlist context menu when timeline and sample windows touch and their clip fields and event layout match. Adjacent Pattern Clips that reference the same pattern can also be joined by the API, CLI, and Playlist context menu when the left clip ends on a known repeat boundary, note tails stay inside pattern cycles, and the clips have matching fields. Selected Pattern Clips that reference different patterns can be merged into a new score pattern; the uppermost clip receives it and source patterns remain unchanged. Pattern event automation, unknown per-pattern data, non-default scale, and expansions above two million notes are rejected. The API, CLI, and Ctrl/Cmd+G Playlist editor action support this merge. The API, CLI, and S-key Playlist Slip tool can slide an Audio Clip's source window while keeping timeline bounds when the source duration is known; UI slipping also requires room to move within the sample. Splitting Pattern and Automation Clips and merging selected non-Pattern clips remain incomplete.
- `[todo]` Performance mode (clip launching)
- `[todo]` Playlist recording (audio + automation)

---

## 9. Mixer

- `[partial]` Mixer track view: recognized insert records appear as selectable vertical strips with a focused inspector; existing names are editable, raw route/color/icon/event-range fields are visible, and the inspector shows ten opaque effect-slot placeholders. Mapping records to native track IDs, faders, meters, effect identity/state, and routing controls remain incomplete
- `[todo]` Per-track EQ, stereo separation, phase invert
- `[todo]` FX slot management (10 slots/track), drag-reorder, save/load chains
- `[todo]` Send knobs, sidechain inputs
- `[todo]` Track freezing / smart disable
- `[todo]` Disk recording per track, stem export (multi-track render)
- `[todo]` Mixer snapshots / states

---

## 10. Browser

- `[partial]` Local file browser navigates folders and filters audio samples, FLP/ZIP projects, presets, and MIDI files; FLP and ZIP projects open from the Browser and MIDI files can be queued for import. `.fst` presets open a read-only details dialog from double-click or **Inspect preset…**, showing the losslessly decoded preset kind, header format, FL version, and event/state counts. Expandable sections list decoded channel identities/paths, recognized plug-in state metadata, raw Mixer insert fields, and automation point counts; plug-in payload bytes remain opaque and applying the preset to a live channel is not implemented. Image-Line identifies `.fst` as a state file for generator and effects presets ([FL Studio State File manual](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_other_fst.htm)). Right-clicking audio in Files or Favorites can send it to the selected sample-bearing Channel Rack row by replacing its existing `0xC4` path event while preserving that string's encoding and trailing bytes. Files and Favorites also support Shift+Up/Down to send the previous/next visible audio result to the selected sample-bearing channel when a text field is not focused. Audio files can be dragged from Files onto an existing sample-bearing Channel Rack row to replace its sample, or onto the Channel Rack add area to create a Sampler channel with a unique ID and matching project string encoding. Image-Line documents sending compatible Browser content to the selected Channel or focused plug-in, Shift+Up/Down stepping, and dragging samples to Channels and the Playlist ([Browser manual](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/browser.htm), [Sampler Channel manual](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/chansettings_sampler.htm)); plug-in loading, preset application, and Playlist drag loading remain. Recursive search builds a bounded background index for the current folder tree or all saved Browser roots, skips symlink entries, and reuses the index for name and type filtering; Ctrl/Command+F starts the all-roots search and F3 advances through results. Up to 30 extra search roots can be added, persisted, and selected as the active Browser folder. Current-project data, scanned plug-in candidates, persisted recent projects, and persisted starred paths have separate Browser tabs; File lists the ten most recent project paths, More exposes up to 50, and Alt/Option+1–0 opens the first ten.
- `[partial]` Clicking supported mono/stereo samples in Files or Favorites opens the bottom Preview player and starts a five-second preview; Full sample, Play-to-end, volume, and Stop are available, and preview mixes with project output. Tempo/pitch synchronization, start-from-mouse-position, seek/loop controls, and broader channel-layout/codec support remain
- `[partial]` Case-insensitive name filtering applies to the current folder, plug-in list, and favorites; file and plug-in favorites persist in the user config folder. Recursive indexing runs on the active folder or across saved Browser roots. Per-file tags can be edited from Files list actions, persist locally, and participate in name search. Named Browser searches persist their query, file-type filter, folder scope, selected tags, tag logic, hidden-tab state, color, and symbol icon; Files and Favorites offer case-insensitive Any/All tag matching. Saved searches appear as selectable Browser tabs and can be deleted, renamed, cloned, hidden/shown, or moved left/right from each tab's context menu. Saving from an active saved tab defaults to that tab's name and style. Bulk tag editing, native icon assets, and online tabs remain
- `[partial]` The Browser lists and filters scanned VST2/VST3 candidates and supports persistent favorites; FL Studio's custom plug-in categories, icons, and preset loading remain
- `[done]` Dot-prefixed files and folders are hidden from the Files view and recursive indexes, keeping OS and tool metadata out of the visible results.

---

## 11. Automation system

- `[partial]` Type-5 channel points: read/edit/insert/remove points in existing `0xEA` blobs while preserving unmodified point bytes and opaque header/trailer data; desktop editing is implemented with a straight-line preview, while FL interpolation modes remain incomplete
- `[todo]` Event automation (pattern-scoped)
- `[todo]` "Create automation clip" / "Link to controller" for any parameter
- `[todo]` Internal controllers: LFO, envelope, formula, peak, X-Y, keyboard
- `[todo]` Remote control / MIDI learn ("Multilink to controllers")
- `[todo]` Audio-rate vs control-rate parameter smoothing (dezippering)

---

## 12. Time stretch / pitch shift

FL Studio uses Elastique (zplane, commercial license) plus legacy algorithms.
Options: license Elastique, or integrate Rubber Band (GPL — license conflict
with MIT; would need a commercial Rubber Band license or a clean-room
implementation).

- `[todo]` Real-time stretch modes per clip/channel
- `[todo]` Offline "stretch" with preserved transients
- `[todo]` Pitch shifting independent of tempo

---

## 13. Render / export

Current: experimental offline renders include audio clips, Sampler Pattern
Clips, and mapped VST3 instruments in one bounded Playlist mix → WAV. The
desktop dialog can choose 16/24-bit integer PCM or 32-bit float output, stereo
or mono channel modes, append up to five seconds after the final scheduled
clip, and optionally apply TPDF dither to 16-bit PCM. This first combined
renderer uses base tempo and provisional channel volume/pan; it does not apply
automation, Mixer routing/effects, or PDC. Full export surface:

- `[partial]` Full-song render through the complete mixer graph (instruments +
  samples + automation + FX + PDC): the desktop can render enabled Playlist
  audio clips, Sampler Pattern Clips, and mapped installed VST3 instruments in
  bounded blocks to WAV with 16/24-bit integer PCM or 32-bit float output and
  stereo, merged mono, left-only, or right-only channels. The dialog also has
  0/1/2/5-second tail presets. Automation, Mixer
  routing/effects, PDC, native level comparison, and other export options
  remain incomplete.
- `[partial]` Formats: the desktop Playlist renderer writes 16/24-bit integer PCM or 32-bit float WAV, with stereo, merged mono, left-only, or right-only channels as documented by [Image-Line](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_save_export.htm); MP3, OGG, and FLAC export remain todo
- `[partial]` Render options: the Playlist mix dialog offers channel modes, 0/1/2/5-second output tails for VST instrument releases, optional unshaped TPDF dither for 16-bit PCM, and Linear or 64-point windowed-sinc resampling. Image-Line documents high-frequency shaped dither and Linear, 6-point Hermite, and selectable sinc quality settings, so matching its noise-shaping profile, interpolation kernels, and full quality range remains todo; normalize and effect-based tail detection are also incomplete. The sinc choice is a local 64-tap Blackman-windowed kernel, not a claim of bit-identical FL Studio output ([Image-Line export manual](https://www.image-line.com/fl-studio-learning/fl-studio-online-manual/html/fformats_save_export.htm)).
- `[todo]` Stem export: split mixer tracks, "split channel tracks"
- `[partial]` Selected Playlist clip render: File > Export can write one
  selected enabled audio-channel clip's supported sample source to float stereo
  WAV from either the clip start or its original song position, clipped to the
  Playlist endpoint. Looping or stretching clips beyond their source duration,
  Pattern Clip rendering, multiple selection, automatic Audio
  Clip insertion, source muting, automation, and Mixer effects remain
  incomplete. FL Studio's consolidation also accepts Pattern Clips and supports
  song-start placement with source muting ([Image-Line Playlist
  manual](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/playlist.htm)).
- `[partial]` Pattern render: the Piano roll can render enabled Sampler notes
  from the selected pattern to float stereo WAV, and a mapped VST3 channel can
  render its selected pattern notes to WAV. FL Studio renders selected patterns
  to audio clips with configurable render settings; this app's Sampler export is
  a separate WAV file and does not render every instrument, add an Audio Clip,
  apply native render options, or include Mixer routing/effects ([Image-Line
  Patterns manual](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/menu_patterns.htm)).
- `[todo]` Burn to CD-era options: skip (obsolete)

---

## 14. MIDI hardware & scripting

- `[todo]` MIDI input: keyboards, controllers, MMC transport
- `[todo]` MIDI output to external hardware
- `[todo]` MIDI scripting API (FL uses Python `midi` module — device scripts)
- `[todo]` Controller templates for popular hardware

---

## 15. Desktop shell / UX

- `[partial]` Window management: Playlist, Channel Rack, Piano roll, Mixer, Plug-ins, Audio, and Automation views; F5–F9 shortcuts. The shell uses eframe's WGPU renderer; the Windows project-open view was visually verified with a 96-clip ZENOLOGY project and nine VST3 channel states restored, and Linux Weston scaling was visually checked at 80%, 100%, and 140%. Mac visual presentation, FL-style detached windows, and further cross-platform scaling/control parity remain incomplete.
- `[partial]` Main menu: File, Edit, Add, Patterns, View, Options, Tools, and Help expose implemented project commands, editor switches, selected-pattern duplication via Alt/Option+C, and a keyboard shortcut reference; remaining FL Studio menu commands are incomplete.
- `[todo]` Detached windows, multi-monitor layouts, window presets
- `[partial]` Browser and transport toolbar: Files, Project, Plug-ins, Favorites, and Recent tabs with a persisted drag-resizable divider and persisted View/Alt/Option+F8 visibility toggle, plus transport, tempo, position, and pattern selection/creation; the hint bar, project picker, PAT/SONG playback mode, detached layout, and remaining Browser controls are incomplete.
- `[todo]` Touch support
- `[partial]` Themes / UI scaling: the Options menu can scale the interface from 80% to 140% and saves the choice locally; Linux virtual-display checks at 80%, 100%, and 140% keep the Browser, workspace, and status bar visible. Separate popup/menu and toolbar scales and Mac HiDPI verification remain.
- `[partial]` Keyboard shortcuts cover the main editors (F5, F6, F7, F9), Space play/pause, recent projects (Alt/Option+1–0), save/undo/redo, and several Browser, Channel Rack, and Piano roll actions; the full FL Studio shortcut map remains incomplete.
- `[partial]` Undo/redo restores lossless FLP document snapshots across editors with a 64 MiB history cap; plug-in-host state and non-project UI state remain outside history
- `[partial]` Autosave and recovery: configurable 5/10/15-minute autosaves (or off) pause during playback; Frequent mode saves every five minutes during playback and before plug-in loading, while Very frequent mode saves every minute and before plug-in loading. Manual saves retain the prior project, a shared retention limit prunes old backups, startup/open offers recovery from a newer autosave, and File provides Backup now, Revert to last autosave, and numbered Save new version (Ctrl/Cmd+N). Opening another project or using File > Exit or the window close button with unsaved edits prompts to Save, Don't Save, or Cancel. Untitled projects cannot autosave, and backup location/controls do not yet match FL Studio exactly.

---

## 16. Platform specifics

| Item | Status | Notes |
|---|---|---|
| Windows installer (NSIS/WiX) | `[partial]` | CI builds installers; verify signed + working |
| macOS app bundle, notarization | `[partial]` | CI runs macOS checks and, without signing secrets, builds an unsigned app bundle, verifies its executable and `.flp` registration, then archives and uploads it as a 14-day `macos-unsigned-gui-preview` artifact for local UI review. Installer publication waits for Developer ID signing and Apple notarization secrets; signed builds are checked with Apple's signature, stapler, and Gatekeeper tools. AU hosting + CoreAudio remain pending. |
| Linux builds (AppImage/deb) | `[partial]` | CI builds; XCB headers handled |
| Windows: ASIO support | `[todo]` | Pro-audio requirement on Windows |
| macOS: native menu bar, file associations | `[partial]` | The packaged app registers `.flp` project files and the application opens a project path passed on launch. Functional command menus remain in the app window; a native macOS menu bar and macOS file-open event handling while the app is already running remain. |
| Cross-platform CI | `[done]` | |

---

## 17. Suggested milestone order

Ordered by dependency and by "most compatibility per unit effort":

1. **Format completion** — mixer state, automation events, all channel types
   (unlocks reading real-world projects fully)
2. **Audio engine bring-up (in progress)** — shared device capture/output,
   Windows WASAPI exclusive access, and one bounded Playlist stream that mixes
    audio clips, Sampler notes, and mapped VST3 instruments placed by Pattern
    Clips are implemented with provisional channel volume/pan. Matching
    installed VST3 channels now load on project open. Validate Playlist playback
    on each platform and compare timing, levels, and instrument behavior against
    native FL Studio output.
3. **Realtime sampler + scheduler (partial)** — selected-pattern and Playlist
   Sampler notes schedule at sample offsets with bounded polyphony, voice
   stealing, basic sample-rate conversion, and channel gain/pan. VST3 Pattern
   Clip transport now runs for mapped VST3 instances. Realtime tempo automation,
   envelopes, and command queues remain; linear `TEMPO` automation currently
   applies to MIDI song export only.
4. **VST3 project-wide autoload + full plugin routing (partial)** — project
   opening now loads recognized channels when a matching installed VST3 is
   found and attempts supported marker-12 state restore. Broaden identity and
   state-layout support, add state write-back, then add automation, multibus
   routing, Mixer integration, process isolation, and PDC.
5. **Full-song offline render (partial)** — Playlist audio, Samplers, and mapped
   VST3 instruments render together; add automation, Mixer routing/effects,
   PDC, native comparison, and export options to complete the graph
6. **PDC + sends/sidechain** — mixer correctness
7. **Piano roll tools** — the editing depth users expect
8. **Playlist audio** — waveforms, fades, stretch
9. **Recording + Edison-class editor**
10. **MIDI hardware + export (partial)** — pattern and arrangement MIDI export now works for decoded layouts, including per-tick linear `TEMPO` automation on song export; native event values/duplicates, scaled clips, full note properties, and hardware support remain.
11. **Polish**: undo everywhere, themes, shortcuts, autosave

(Stock plugin DSP is intentionally absent from this list — see §4.)

## 18. Non-goals

- Reimplementing stock Image-Line plugin DSP (project decision 2026-10-06 —
  host originals instead; see §4)
- FL Cloud / online content libraries / sample subscription services
- FL Studio Mobile (separate product)
- ReWire (deprecated by Propellerhead/Reason Studios)
- "FL Studio" trademark usage in product naming if this ships publicly —
  get legal advice before any public release branding
