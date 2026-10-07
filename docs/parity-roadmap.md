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
| Header fields: PPQ, tempo, time signature, project metadata | `[partial]` | Basic fields decoded; full metadata surface unmapped |
| Channel records: all types (sampler, generator, layer, MIDI out, automation clip) | `[partial]` | Known channel kind values 0/2/3/4/5 are named while unknown raw values are retained; Layer child IDs (`0x5E`) and raw flags (`0x90`) are decoded, and child lists can be edited while preserving other channel events. Legacy channel types, Layer playback, and flag editing remain incomplete. |
| Pattern/score events: note records, all encodings | `[partial]` | 24-byte records handled; empty-pattern and conflicting-encoding edge cases guarded |
| Playlist events: clips, tracks, arrangements | `[partial]` | Clip position/length editable; all three record sizes (80/60/32) recognized |
| Mixer state events | `[partial]` | Reads insert names, raw input/output/color/icon fields from the observed `0x9A`/`0x93`/`0x95` sequence and preserves 12-byte records from `0xE1`; parameter meaning, FX state, and routing edits remain incomplete. |
| Automation events (channel envelopes, event automation) | `[partial]` | Reads type-5 channel automation points from `0xEA` blobs and edits, inserts, or removes points while preserving unmodified point bytes and the opaque header and era trailer. The desktop Automation view edits those curves with a straight-line preview; interpolation, event automation, target links, automation-blob creation, and clip creation remain incomplete. |
| Time markers, song position markers | `[partial]` | Reads arrangement-scoped marker positions, names, and time signatures. The documented signature bit is exposed separately; other position bits remain intact. Marker editing, song-position playback, and broader version verification remain incomplete. |
| Project settings: swing, master pitch, metronome, recording settings | `[todo]` | |
| `set-tempo`, `rename-channel`, note add/edit/delete, clip edit, channel levels | `[done]` | CLI surface; each rewrites only affected bytes |
| Create missing objects (patterns, channels, clips) | `[todo]` | CLI only edits existing objects today |

### 1.2 Presets and packages

| Item | Status | Notes |
|---|---|---|
| `.fst` state preset reading (envelope level) | `[partial]` | Accepted by chunk reader; most state uninterpeted |
| `.fst` generator vs effect vs mixer-state variants | `[todo]` | |
| Zipped project packages (`.zip` with bundled samples) | `[todo]` | Read + write |
| `.flp` "save as" version targeting | `[todo]` | Writing files older FL versions can open |

### 1.3 MIDI files

| Item | Status | Notes |
|---|---|---|
| SMF read: MThd/MTrk, running status, meta, SysEx | `[done]` | |
| MIDI import into pattern/channel with PPQ conversion | `[done]` | |
| SMPTE-timed MIDI | `[todo]` | |
| Tempo-map conversion on import | `[todo]` | Currently preserves FLP tempo |
| MIDI export (File > Export > MIDI) | `[todo]` | |
| MIDI export options: pattern vs song, channel mapping | `[todo]` | |

---

## 2. Project model (in-memory representation)

The decoded document must model everything the format can express.

- `[partial]` Channels: summaries, sample paths, plugin state blobs, levels, known kind mapping, and Layer child relationships/raw flags
- `[partial]` Patterns: note lists per channel
- `[partial]` Playlist: tracks, arrangements, clips with targets
- `[partial]` Mixer: recognized insert summaries and raw `0xE1` parameter records; full 125 inserts, master/sends, track mapping, and editable state remain incomplete
- `[partial]` Automation: type-5 channel point curves can be read and points in an existing blob edited, inserted, or removed from the desktop view; interpolation, parameter links, event automation, LFOs, and new clip creation remain incomplete
- `[todo]` Time signatures per pattern/arrangement, tempo automation
- `[todo]` Channel groups, colors, icons
- `[todo]` Swing / groove settings per channel
- `[todo]` Layer channels (keyboard splits, crossfades)
- `[todo]` Project info: title, author, comments, genre

---

## 3. Audio engine

The hardest subsystem. See the difficulty discussion in chat history: this is
where real-time constraints punish sloppy code.

### 3.1 Device I/O

| Item | Status | Notes |
|---|---|---|
| Windows: WASAPI (shared + exclusive), DirectSound, ASIO | `[partial]` | Shared capture/output through CPAL and direct WASAPI exclusive streams are implemented, including endpoint selection, rate/buffer settings, input metering/monitoring, an output test tone, block-streamed Playlist audio clips with provisional volume/pan mapping, and block-streamed VST3 pattern output through a bounded stereo ring. A 9,600-frame excerpt rendered from `Ookay - Thief.flp` was consumed by both shared and exclusive output on the current machine. A shared-mode input smoke run opened the current Focusrite endpoints; capture reported one startup discontinuity and then stayed active without further errors for the remainder of the 3-second check. These verify current hardware paths, not all endpoints or formats. DirectSound and ASIO are not implemented. |
| macOS: CoreAudio | `[partial]` | The shared CPAL path uses the platform's default audio host and supports block-streamed Playlist audio; hardware behavior is not verified yet. |
| Linux: ALSA, PulseAudio/PipeWire, JACK | `[partial]` | The shared CPAL path uses the platform's default audio host and supports block-streamed Playlist audio. Cross-platform build/test CI exists; backend and hardware behavior are not verified yet. |
| Device enumeration, sample-rate / buffer-size negotiation | `[partial]` | CPAL lists endpoints and opens selected/default devices at the requested sample rate and buffer size. Shared mode falls back to the backend's default buffer when a fixed size is rejected; complete capability negotiation is still needed. |
| Recommended starting point: `cpal` crate for cross-platform bring-up | `[done]` | CPAL is used for shared-mode input and output on desktop platforms. |

### 3.2 Real-time thread

- `[partial]` Lock-free audio callback: shared CPAL callbacks avoid allocation and locks; the WASAPI exclusive output event loop currently allocates a buffer for each event
- `[todo]` Lock-free command queue (UI thread → audio thread): transport, parameter changes, note events
- `[partial]` Lock-free metering/state queue (audio thread → UI thread): input peak is published through an atomic value; a general state queue is not implemented
- `[todo]` Sample-accurate event scheduling within a buffer (events carry sample offsets, not just buffer indices)
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
- `[partial]` Playlist audio clip playback: enabled audio-channel clips are mixed in bounded 1,024-frame blocks and streamed at the device rate, applying decoded channel volume/pan and source offsets. The raw level mapping is provisional and still needs comparison against native FL Studio output. Distinct sources are still decoded into a bounded in-memory cache, and non-default time stretch, automation, routing, pattern clips, and Mixer effects are unsupported.
- `[todo]` Real-time sampler voice management: polyphony, voice stealing
- `[todo]` Resampling (project rate vs sample rate vs device rate)
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
| VST3 hosting: load, process, native editor | `[partial]` | In-process load and editor open work. The selected pattern/channel can now be processed blockwise on a worker and streamed through shared or WASAPI-exclusive output; the open editor and audio worker use the same loaded instance. The device callback only drains complete stereo frames from a bounded ring. Processing can still underrun, note scheduling is limited to one pattern/channel, Playlist clip expansion and multi-instrument transport are absent, and Mixer processing/full-song playback remain unimplemented. |
| VST3 state save/restore via FLP `0xD5` record | `[partial]` | Single ZENOLOGY probe succeeded; not general |
| VST3 parameter automation | `[todo]` | |
| VST2 hosting | `[todo]` | Needs a VST2 SDK implementation (Steinberg discontinued the SDK; use vestige headers) |
| CLAP hosting | `[todo]` | FL Studio supports CLAP as of recent versions |
| AU hosting (macOS) | `[todo]` | |
| 32-bit plugin bridging on 64-bit host | `[todo]` | Out-of-process sandbox + IPC |
| Plugin crash isolation (sandboxed scanning) | `[todo]` | A crashing plugin must not take down the DAW |
| Latency reporting per plugin (feeds PDC) | `[todo]` | |

---

## 6. Piano roll

Current: note create/drag/resize/snap, velocity editing. FL Studio's piano
roll is famously deep — the full toolset:

**Tools:** Draw (pencil), Paint, Delete, Mute, Slice, Select, Zoom, Playback
`[todo]`

**Edit operations:**
- `[todo]` Quantize (with strength, swing)
- `[todo]` Chop, Glue, Legato
- `[todo]` Strum, Flam
- `[todo]` Arpeggiator, Riff machine, Claw machine
- `[todo]` Randomize (velocity/pan/pitch), Humanize
- `[todo]` Scale levels, Articulate (LFO-envelopes on note properties), Limit (note range filter)
- `[todo]` Score flipper, Claw machine
- `[todo]` Slide notes and portamento (channel pitch slides)
- `[todo]` Ghost channels (view other patterns' notes)
- `[todo]` Note colors / MIDI channel grouping
- `[todo]` Stamp tool (chord/scale stamps)
- `[todo]` Event editor (per-note velocity/pan/pitch/modulation curves)
- `[todo]` Helpers: scale highlighting, chord detection display

---

## 7. Step sequencer / Channel rack

- `[partial]` Channel list with plugin names, volume/pan sliders
- `[todo]` Step sequencer grid with per-step velocity/pan
- `[todo]` Graph editor
- `[todo]` Channel grouping, zipping, sorting
- `[todo]` "Send to piano roll", per-channel swing, time multiplier
- `[todo]` Keyboard editor view

---

## 8. Playlist

- `[partial]` Clip display, clip select, start/length editing
- `[todo]` Audio clip waveform rendering with zoom
- `[todo]` Audio clip fades, crossfades, gain envelopes
- `[todo]` Stretch modes per clip (resample, stretch, e3 generic — needs time-stretch engine)
- `[todo]` Pattern clips, automation clips on playlist tracks
- `[partial]` Time markers and meter records can be read and listed; marker editing and playback-clock behavior remain incomplete
- `[todo]` Track grouping, mute/solo per playlist track
- `[todo]` Slip editing, cut/copy/paste/split/merge/join
- `[todo]` Performance mode (clip launching)
- `[todo]` Playlist recording (audio + automation)

---

## 9. Mixer

- `[partial]` Full 125-track UI with routing visualization: current view lists recognized insert names and raw route fields, without controls or routing visualization
- `[todo]` Per-track EQ, stereo separation, phase invert
- `[todo]` FX slot management (10 slots/track), drag-reorder, save/load chains
- `[todo]` Send knobs, sidechain inputs
- `[todo]` Track freezing / smart disable
- `[todo]` Disk recording per track, stem export (multi-track render)
- `[todo]` Mixer snapshots / states

---

## 10. Browser

- `[todo]` File browser: samples, presets, projects, plugin database
- `[todo]` Audition/preview samples in browser (tempo-synced preview)
- `[todo]` Search, favorites, tagging
- `[todo]` Plugin database with favorites and custom categories

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

Current: experimental offline renders (audio clips → WAV, pattern via VST3 →
WAV). Full export surface:

- `[todo]` Full-song render through the complete mixer graph (instruments +
  samples + automation + FX + PDC) — the definition of "it plays back right"
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

- `[partial]` Window management: Playlist, Channel Rack, Piano roll, Mixer, and an Automation view; F5–F9 shortcuts
- `[todo]` Detached windows, multi-monitor layouts, window presets
- `[todo]` Browser panel, toolbar, hint bar, project picker
- `[todo]` Touch support
- `[todo]` Themes / UI scaling (HiDPI)
- `[todo]` Full keyboard shortcut map parity
- `[todo]` Undo/redo across all editors (global undo history)
- `[todo]` Autosave, backup versions, crash recovery

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
   Windows WASAPI exclusive access, and block-streamed Playlist audio clip
   playback with provisional channel volume/pan are implemented; next add real-time
   sample voices and pattern scheduling, then validate stable playback on each platform
3. **Realtime sampler + scheduler** — sample-accurate note scheduling, channel
   gain/pan, basic mixer summing (first true playback)
4. **VST3 realtime processing + state restore** — the compatibility crux
5. **Full-song offline render** — instruments + samples + FX in one graph
   (validates the engine without realtime pressure)
6. **PDC + sends/sidechain** — mixer correctness
7. **Piano roll tools** — the editing depth users expect
8. **Playlist audio** — waveforms, fades, stretch
9. **Recording + Edison-class editor**
10. **MIDI hardware + export**
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
