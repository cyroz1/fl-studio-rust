# FL Studio Full Parity & Compatibility Roadmap

What "done" means: open any `.flp` from any FL Studio version, play it back
sounding identical, edit it with the same tools, and save it back without data
loss. This document enumerates everything that requires, grouped by subsystem,
with the current status of this project against each item.

Status legend:

- `[done]` — implemented and working
- `[partial]` — started; works for a subset of cases
- `[todo]` — not started

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
| Header fields: PPQ, tempo, time signature, project metadata | `[partial]` | Tempo, meter, build number, and Project Info strings are decoded; swing, master pitch, recording options, and broader version validation remain unmapped. |
| Project Info: title, author, comments, genre, web link | `[done]` | Read/write through the desktop Project Info dialog and CLI. Metadata events are edited without rewriting unrelated event bytes. |
| Channel records: all types (sampler, generator, layer, MIDI out, automation clip) | `[partial]` | Known channel kind values 0/2/3/4/5 are named while unknown raw values are retained; `0xC4` sample paths are decoded for kind-0 Sampler and kind-4 sample-backed channels; Layer child IDs (`0x5E`) and raw flags (`0x90`) are decoded, and child lists can be edited in the Channel Rack through named channel choices while preserving other channel events. The observed Random and Crossfade bits can be edited in the API, CLI, and Channel Rack without changing other flag bits. Legacy channel types and Layer playback remain incomplete. |
| Pattern/score events: note records, all encodings | `[partial]` | 24-byte records handled; empty-pattern and conflicting-encoding edge cases guarded |
| Playlist events: clips, tracks, arrangements | `[partial]` | Clip position/length and raw item/track/group/flags, offsets, and established scale field are editable while preserving other record bytes; exact-record clip duplication is available in the API, CLI, and Playlist editor; all three record sizes (80/60/32) are recognized, with structural inference when the stored version tag disagrees with the record width |
| Mixer state events | `[partial]` | Reads insert fields and raw `0xE1` records, recognizes candidate volume/pan/EQ parameter IDs and target bits, and can edit an existing record's value by exact record index in the API and Mixer inspector. Insert-name editing is available. Target-to-visible-track mapping, fader UI, FX state, and routing remain incomplete. |
| Automation events (channel envelopes, event automation) | `[partial]` | Reads type-5 channel automation points from `0xEA` blobs and edits, inserts, or removes points while preserving unmodified point bytes and the opaque header and era trailer. Song MIDI export samples linear type-5 automation on a Playlist clip targeting a channel named `TEMPO` once per tick and restores the project tempo after the clip. The desktop Automation view uses a straight-line preview; native curve tension, realtime tempo playback, event automation, target links, automation-blob creation, and clip creation remain incomplete. |
| Time markers, song position markers | `[partial]` | Reads arrangement-scoped marker positions, names, and time signatures. The project API, CLI, and Playlist editor can edit, create, or delete time/signature markers while preserving unknown high bits and unrelated events. The Playlist playhead follows device-consumed frames at the base tempo; seeking to markers, tempo-automation timing, signature-change bar numbering, and broader version verification remain incomplete. |
| Project settings: swing, master pitch, metronome, recording settings | `[partial]` | The Project model, CLI, and desktop dialog read and edit the FL Studio 26 Advanced options `Play truncated notes in clips` and `Fast declick for cut groups`. Swing, master pitch, metronome, recording settings, panning law, and other controls remain unmapped. |
| `set-tempo`, `rename-channel`, note add/edit/delete, clip edit, channel levels | `[done]` | CLI surface; each rewrites only affected bytes |
| Create missing objects (patterns, channels, clips) | `[partial]` | Creates an empty pattern through the desktop control or CLI when a unique existing `0xD0`/`0xE0` note-event encoding is available, creates a Sampler channel from a Browser audio drop with a unique ID and inferred project string encoding, and duplicates existing Playlist clips by copying their complete records. Other new channel types, targetless clips, unsupported/ambiguous layouts, and native FL Studio validation remain outstanding. |

### 1.2 Presets and packages

| Item | Status | Notes |
|---|---|---|
| `.fst` state preset reading (envelope level) | `[partial]` | Accepted by the lossless chunk/event reader. Browser details expand decoded channel names/kinds/plug-in IDs/sample paths, recognized plug-in identity and state-byte metadata, raw Mixer insert fields, and automation point counts. Plug-in payload contents remain opaque. |
| `.fst` generator vs effect vs mixer-state variants | `[partial]` | `FstPreset` classifies header formats 32 (channel), 48 (native plug-in), 49 (VST generator), 50 (VST effect), and 64 (Mixer insert); format 24 is identified as automation state. `preset-info` reports the kind and decoded event/channel/insert counts while preserving the complete source stream. Applying presets to hosted plug-ins, interpreting opaque state payloads, and native FL Studio comparison remain outstanding. |
| Zipped project packages (`.zip` with bundled samples) | `[partial]` | The desktop app opens standard ZIP packages, prefers a root-level FLP when present, extracts regular files into a temporary workspace for relative sample lookup, and writes edits back while retaining the other files. Plain FLP-to-ZIP save currently writes the FLP without collecting referenced samples; archive metadata, encrypted entries, and native FL Studio validation remain outstanding. |
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
- `[partial]` Patterns: note lists per channel; empty patterns can be created when the project supplies an unambiguous note-event encoding
- `[partial]` Playlist: tracks, arrangements, clips with targets, and time markers that can be edited, created, and deleted through the API, CLI, and desktop editor
- `[partial]` Mixer: recognized insert summaries and parameter kinds, with exact-record value editing and insert-name editing; full 125 inserts, master/sends, target mapping, fader UI, effects, and routing remain incomplete
- `[partial]` Automation: type-5 channel point curves can be read and points in an existing blob edited, inserted, or removed from the desktop view; linear tempo-map conversion is implemented for song MIDI export, while native curve interpolation, realtime tempo playback, parameter links, event automation, LFOs, and new clip creation remain incomplete
- `[partial]` Tempo automation: linear channel curves named `TEMPO` are sampled for MIDI song export; realtime playback and native tension/easing semantics remain
- `[todo]` Channel groups, colors, icons
- `[todo]` Swing / groove settings per channel
- `[partial]` Project settings: the two verified FL Studio 26 Advanced options `Play truncated notes in clips` and `Fast declick for cut groups` can be read and edited; other project-wide settings remain unmapped
- `[todo]` Layer channels (keyboard splits, crossfades)
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
- `[partial]` Sample-accurate event scheduling within a buffer: selected-pattern previews and Playlist Pattern Clip Sampler notes schedule at device-frame offsets, while VST3 notes are timestamped in plug-in sample frames before resampling to the device rate; project opening loads matching installed VST3 channel instances and attempts marker-12 component/controller restore from nested field-53 records. Other wrapper/state layouts, a shared transport command queue, and automation scheduling are not implemented
- `[partial]` Underrun/dropout detection and reporting: shared stream errors and exclusive worker errors reach the UI; counting, history, and recovery are not implemented
- `[todo]` Denormal protection in DSP code

### 3.3 Mixer graph

- `[todo]` 125 insert tracks + master, each with level/pan/mute/solo
- `[todo]` Per-track FX chains (10 slots each)
- `[todo]` Sends and sidechain routing (arbitrary track-to-track routing)
- `[todo]` Plugin delay compensation (PDC): per-plugin latency measurement, graph-wide delay alignment, manual + automatic modes
- `[todo]` 32-bit float internal processing throughout
- `[todo]` Multi-threaded rendering: parallelize independent mixer tracks across cores

### 3.4 Sample playback

- `[partial]` Offline: WAV/OGG/FLAC/MP3/AIFF/WavPack decode by content (symphonia + wavicle)
- `[partial]` Playlist playback: enabled audio-channel clips, enabled kind-0 Sampler notes, and notes for mapped VST3 instrument channels are mixed in bounded worker blocks at the device rate. Opening a project auto-loads a matching installed VST3 for recognized channel identities and attempts supported marker-12 state restoration; unavailable or unrecognized channels remain unloaded and are reported. Pattern Clips schedule plugin MIDI at absolute arrangement ticks; mono/stereo plugin output is resampled and mixed with audio and Sampler sources. The raw level mapping and pattern repeat/edge behavior are provisional and need comparison against native FL Studio output. Non-default time stretch, automation, routing, clip-state flags, Mixer effects, and plugin delay compensation are unsupported.
- `[partial]` Real-time sampler voice management: selected-pattern preview and Playlist Pattern Clips use a bounded voice pool with oldest-voice stealing, zero-length one-shot notes, and a short release ramp for keyed notes. Pattern Clips place notes at clip offsets, repeat by explicit pattern length (or a note-length inference when absent), and bound keyed notes at clip ends. FL envelope, loop, root-key, and polyphony settings are not decoded; volume/pan mapping and repeat behavior still need comparison against native output
- `[partial]` Resampling (project rate vs sample rate vs device rate): linear interpolation handles input/output rate differences and note-key transposition around an assumed MIDI-60 root; pitch-root metadata and higher-quality resampling remain
- `[todo]` FL's private RIFF-wrapped Ogg handling in realtime path
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
| VST3 hosting: load, process, native editor | `[partial]` | In-process load and editor open work. Opening a project auto-loads each channel with a recognized VST3 identity that matches an installed bundle, and maps successfully loaded instances to that channel for playback. Selected-pattern preview and Playlist Song transport process loaded instruments blockwise on a worker and stream through shared or WASAPI-exclusive output; the open editor and audio worker use the same loaded instance. Playlist Pattern Clips expand notes for mapped VST3 channels, and multiple instruments are mixed with audio clips and Samplers through the bounded stream. Matching is currently based on wrapper name or bundle name; plug-in crashes can still take down the app. Mixer graph processing, automation, PDC, multibus output, process isolation, and native audio comparison remain incomplete. |
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

Current: note create/drag/resize/snap, velocity editing, and a selected-note
inspector for raw flags/group, fine pitch, release, stored MIDI channel, pan,
and modulation X/Y. Quantize has channel and selected-note actions; Legato,
Chop, Glue, Flip, Strum, Flam, seeded Randomize and Humanize, pitch Limit,
Arpeggiate, and Slice can target the channel or selected notes. Scale highlighting, chord labels, ghost
channels, and MIDI-channel note colors are available in the Piano roll. The
CLI can edit score fields individually while preserving the reserved byte.
FL Studio's piano roll is famously deep — the full toolset:

**Tools:** `[partial]` Draw (P), Paint (B), Select (E), Zoom (Z), Playback (Y), Chord Stamp, and Slice-at-tick; single-note delete, modifier and box selection, Select All (Ctrl/Cmd+A), Invert (Shift+I), Deselect (Ctrl/Cmd+D), duplicate-to-right (Ctrl/Cmd+B) for selected notes or all notes in the target channel, group move/resize/delete, drag-to-zoom, background-click zoom out, cursor-centered Page Up/Down, and an independent zoom slider. Duplicate spacing follows the copied notes' time span; timeline-defined repeat intervals remain unsupported. Playback auditions Sampler or loaded VST3 notes on click and while dragging across notes; native instrument preview and continuous playhead scrubbing remain. Mute remains `[todo]`.

**Edit operations:**
- `[partial]` Quantize note starts on a selected snap grid with strength and swing, either channel-wide or on the selected notes; native swing semantics remain
- `[partial]` Legato extends notes to the next distinct onset, Chop splits eligible notes into equal segments, and Glue joins touching/overlapping notes with matching properties; channel and selected-note scopes are available, while native tool options remain
- `[partial]` Score flipper mirrors a channel around its latest note end or selected notes within their time bounds; native tool options remain
- `[partial]` Strum staggers simultaneous notes by pitch across a configurable tick spread within the channel or selection; native velocity and chord-overlap options remain
- `[partial]` Flam adds a configurable short stroke before or after channel or selected notes; tempo-based time, presets, and grouping remain
- `[partial]` Randomize applies seeded velocity, pan, and pitch changes with directional or bipolar level offsets and optional default-level reset to the channel or selection; scale-aware note generation remains
- `[partial]` Humanize applies seeded timing and velocity variation to the channel or selection; tempo-relative timing remains
- `[partial]` Arpeggiator converts selected same-onset chords to gated up/down runs with configurable step time and octave range; custom score patterns and sync modes remain
- `[todo]` Riff machine, Claw machine
- `[partial]` Limit folds channel or selected note pitches into a key range by octave and clamps keys that cannot fit; scale snapping remains
- `[partial]` Slice-at-tick splits notes crossing the chosen tick in the channel or selection; native directional cut gestures remain
- `[todo]` Scale levels, Articulate (LFO/envelopes on note properties)
- `[todo]` Slide notes and portamento (channel pitch slides)
- `[partial]` Ghost channels show notes from all channels in the selected pattern with the target channel emphasized; independent ghost editing controls remain
- `[partial]` Notes can be colored by their stored MIDI channel; native note color groups and color-based selection remain
- `[partial]` Chord Stamp supports common manual chords and scale-derived triads/sevenths, uses the snap length, and can return to Draw after one stamp; automatic top-down/bottom-up voicing, voice leading, percussion/slide presets, and chord preview remain
- `[partial]` The integrated Event editor shows and edits per-note velocity, pan, release, fine pitch, Mod X, and Mod Y values as draggable stems; target selection is available in the panel, with Shift+F cycling targets. Pattern-scoped controller automation, interpolation, selection editing for coincident notes, and native scale/target behavior remain incomplete
- `[partial]` Scale highlighting supports major, natural/harmonic minor, and major/minor pentatonic keys; the selected-note inspector identifies common same-onset chords; automatic scale detection and richer chord analysis remain

---

## 7. Step sequencer / Channel rack

- `[partial]` Channel list with plugin names, volume/pan sliders, and editable Layer child IDs
- `[partial]` Pattern/bar step grid toggles notes at sixteenth-note positions; new steps use key 60 and velocity 100. The Ctrl+K Graph Editor draws and edits per-step note key, velocity, pan, release, fine pitch, modulation X/Y, and shift, creates a C5 note on an empty step, and interpolates values when right-dragging across steps. The Rep lane, Ctrl scale-all gesture, Alt reset gesture, and multiple notes at one step remain incomplete
- `[todo]` Channel grouping, zipping, sorting
- `[todo]` "Send to piano roll", per-channel swing, time multiplier
- `[todo]` Keyboard editor view

---

## 8. Playlist

- `[partial]` Clip display, clip select, start/length and exposed record-field editing, and exact-record duplicate
- `[partial]` Audio-channel clips draw a decoded, combined min/max waveform, crop it to the clip's source offsets, and redraw its detail at the current Playlist zoom. Previews decode off the UI thread and cache bounded peak buckets; stereo/spectral view modes, fade and gain previews, and alternate stretch-mode rendering remain
- `[todo]` Audio clip fades, crossfades, gain envelopes
- `[todo]` Stretch modes per clip (resample, stretch, e3 generic — needs time-stretch engine)
- `[partial]` Pattern clips: the selected arrangement expands Sampler and mapped VST3 instrument notes at clip positions, repeats notes to clip length using explicit pattern lengths or an inferred note span, and clips keyed-note ends at the Playlist boundary. Project opening auto-loads matching installed VST3 instances for recognized channels; missing matches and failed loads are reported. Automation clips, clip flags, and non-default scale remain unsupported.
- `[partial]` Time markers and meter records can be read, listed, edited, created, and deleted through the API, CLI, and desktop editor; the Playlist playhead follows device-consumed frames at base tempo, while seeking, tempo-automation timing, and signature-change bar numbering remain incomplete
- `[todo]` Track grouping, mute/solo per playlist track
- `[partial]` Duplicate/copy of a selected clip preserves its full record; slip editing, clipboard paste, split, merge, and join remain
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
Clips, and mapped VST3 instruments in one bounded Playlist mix → stereo float
WAV. This first combined renderer uses base tempo and provisional channel
volume/pan; it does not apply automation, Mixer routing/effects, or PDC. Full
export surface:

- `[partial]` Full-song render through the complete mixer graph (instruments +
  samples + automation + FX + PDC): the desktop can render enabled Playlist
  audio clips, Sampler Pattern Clips, and mapped installed VST3 instruments in
  bounded blocks to a 32-bit float WAV. Automation, Mixer routing/effects, PDC,
  native level comparison, and export options remain incomplete.
- `[todo]` Formats: WAV (16/24/32-bit), MP3, OGG, FLAC
- `[todo]` Render options: quality (resampling), dithering, normalize, tail length
- `[todo]` Stem export: split mixer tracks, "split channel tracks"
- `[todo]` Playlist selection render, pattern render
- `[todo]` Burn to CD-era options: skip (obsolete)

---

## 14. MIDI hardware & scripting

- `[todo]` MIDI input: keyboards, controllers, MMC transport
- `[todo]` MIDI output to external hardware
- `[todo]` MIDI scripting API (FL uses Python `midi` module — device scripts)
- `[todo]` Controller templates for popular hardware

---

## 15. Desktop shell / UX

- `[partial]` Window management: Playlist, Channel Rack, Piano roll, Mixer, Plug-ins, Audio, and Automation views; F5–F9 shortcuts. The shell uses eframe's WGPU renderer; the Windows project-open view was visually verified with a 96-clip ZENOLOGY project and nine VST3 channel states restored. Mac/Linux visual presentation, FL-style detached windows, scaling, and control parity remain incomplete.
- `[todo]` Detached windows, multi-monitor layouts, window presets
- `[todo]` Browser panel, toolbar, hint bar, project picker
- `[todo]` Touch support
- `[todo]` Themes / UI scaling (HiDPI)
- `[todo]` Full keyboard shortcut map parity
- `[partial]` Undo/redo restores lossless FLP document snapshots across editors with a 64 MiB history cap; plug-in-host state and non-project UI state remain outside history
- `[partial]` Autosave and recovery: configurable 5/10/15-minute autosaves (or off) pause during playback; Frequent mode saves every five minutes during playback and before plug-in loading, while Very frequent mode saves every minute and before plug-in loading. Manual saves retain the prior project, a shared retention limit prunes old backups, startup/open offers recovery from a newer autosave, and File provides Backup now, Revert to last autosave, and numbered Save new version (Ctrl/Cmd+N). Opening another project or using File > Exit or the window close button with unsaved edits prompts to Save, Don't Save, or Cancel. Untitled projects cannot autosave, and backup location/controls do not yet match FL Studio exactly.

---

## 16. Platform specifics

| Item | Status | Notes |
|---|---|---|
| Windows installer (NSIS/WiX) | `[partial]` | CI builds installers; verify signed + working |
| macOS app bundle, notarization | `[partial]` | CI builds; AU hosting + CoreAudio pending |
| Linux builds (AppImage/deb) | `[partial]` | CI builds; XCB headers handled |
| Windows: ASIO support | `[todo]` | Pro-audio requirement on Windows |
| macOS: native menu bar, file associations | `[todo]` | |
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
