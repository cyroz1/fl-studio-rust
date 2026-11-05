# Batch 8: FL Studio manual audit of `[partial]` roadmap items

Date: 2026-11-05. Branch: `batch-8-docs-audit` (from main `9c57ca9`). Roadmap status
tags were not changed.

Method: every `[partial]` item in `docs/parity-roadmap.md` (176 items) was checked
against the official FL Studio online manual (image-line.com/fl-studio-learning/
fl-studio-online-manual) where its blocker was behavioral. The manual documents
behavior thoroughly but documents zero `.flp` binary format, so items blocked on
record layouts, flag bits, or serialization stay blocked. Items whose completable
work already landed on the sibling branches batch-1 to batch-6 are marked
"already done (batch-N)" rather than reimplemented here (those branches are not
merged into this tree).

Manual pages consulted for this audit: Exporting Audio & MIDI
(`fformats_save_export.htm`, read in full), The Browser (`browser.htm`), Piano roll
Flam Tool (`pianoroll_flam.htm`), Piano roll menu (ghost channels, scale
highlighting), Edison (scripting workflow), Mixer manual.

## Implemented on this branch

| Item | Manual page | Notes |
| --- | --- | --- |
| §13 Render options (song length hierarchy) | `fformats_save_export.htm` | Commit `2760070`. Full-song Playlist and Mixer-stem renders now floor the length at the end of the last bar containing data (meter-aware, honoring signature markers) and at the last time-marker position, per the manual's length hierarchy (selection, last time marker beyond the last data bar, end of last data bar, then Tail). Repeat markers placed after the last clip extend the render via the same rule. Range/selection renders keep their exact bounds. |
| §13 Render options (Disable maximum polyphony) | `fformats_save_export.htm` | Commit `126c21c`. New `disable_maximum_polyphony` on `PlaylistRenderOptions` and `SamplerPatternRenderOptions`; when set, prepared Sampler sources get no max-polyphony limit so no voices are stolen. The separate Mono path is untouched, matching the manual ("does NOT ignore Mono option if selected"). Render-only, as in FL Studio. Desktop render dialog checkbox wired through all four render flows. |
| §10 Local file browser (middle-click) | `browser.htm` ("Middle mouse button - Loads compatible content on the selected Channel") | Middle-clicking an audio file in the Browser Files or Favorites list now loads it on the selected channel, the same action as the existing "Send to selected Channel" context menu and Shift+Up/Down stepping. |

### Manual-specified render options NOT implemented (still blocked, with reasons)

| Option | Manual page | Reason |
| --- | --- | --- |
| Enable insert effects / Enable master effects | `fformats_save_export.htm` | Mixer effect chains, PDC delays, and tails live inside the hosted-plugin stream processors that callers build per render (`PlaylistRenderPlugins`); there is no single library gate, and the VST3 processor also renders instrument channels, so it cannot simply be withheld. Blocked on a cross-cutting processor-architecture change, not on missing behavior spec. |
| Wrap remainder (last-bar rule) | `fformats_save_export.htm` ("starting the render at the last bar, then mixing any audio decaying after the last bar back into the start") | The repo renders the whole song to a float spool and wraps all post-end decay. Isolating decays produced only by notes in the last bar needs a cold-start render at the last bar; the prepared-plan renderer processes from frame 0 even for ranges, so held-over notes and effect state cannot be isolated, and the manual does not determine those boundary semantics. Not guessed. |
| Save tempo information | `fformats_save_export.htm` ("Saves project tempo in .wav files meta-data format") | The WAV metadata encoding is not specified. The repo's `acid` chunk reader treats that chunk as loop metadata for stretch-enabled loops, so writing it to song renders could mislabel them. Not guessed. |
| Save loop markers | `fformats_save_export.htm` | Which loop a song render should save is unspecified. |
| Dithering (shaped algorithm) | `fformats_save_export.htm` ("shift the majority of the hiss above 10 kHz") | No formula published; the existing dither modes stay as implemented and native comparison is outstanding. |
| Normalize | n/a | FL Studio's export dialog has no Normalize option per the manual page, so none was added. |

## Per-item audit

Verdicts: **implemented** = landed on this branch; **already done** = completable
work landed on a sibling batch-1..6 branch (not merged here); **still blocked** =
cannot be completed from the manual plus this repo, with the blocker class.

### §1 Binary format & project I/O

| Item | Verdict | Reason |
| --- | --- | --- |
| FLP versions 1.x-26.x | still blocked | binary format: version-specific record layouts need native saves |
| Header fields: PPQ, tempo, time signature, metadata | still blocked | binary format: remaining header field layouts unestablished |
| Sampler Envelope/LFO records (`0xDA`) | still blocked | binary format: remaining record fields unestablished |
| Channel records: all types | still blocked | binary format: per-kind record layouts unestablished |
| Pattern/score events: note records, all encodings | still blocked | binary format: alternate note encodings unestablished |
| Playlist events: clips, tracks, arrangements | still blocked | binary format: record variants (88-byte tail offsets) unestablished |
| Mixer state events | still blocked | binary format: remaining mixer record fields unestablished |
| Automatic Plugin Delay Compensation | still blocked | binary format + native render: automatic amount record and plugin latency reports |
| Automation events | still blocked | binary format: target/point record layouts unestablished |
| Time markers, song position markers | still blocked | native comparison: remaining marker-type semantics (loop/skip) |
| Project settings | still blocked | binary format: remaining settings field layouts unestablished |
| Create missing objects (patterns, channels, clips) | still blocked | binary format: native object-creation records unestablished (no create/delete track or arrangement op exists in the repo) |
| `.fst` state preset reading (envelope level) | still blocked | binary format: preset record internals unestablished |
| `.fst` generator vs effect vs mixer-state variants | still blocked | binary format: variant layouts unestablished |
| Zipped project packages (`.zip`) | still blocked | native comparison: remaining package edge cases |
| MIDI import into pattern/channel with PPQ conversion | still blocked | native comparison: remaining import metadata behavior |
| Tempo-map conversion on import | still blocked | native comparison |
| MIDI export (File > Export > MIDI) | still blocked | native comparison: SMF type-1 layout choices |
| MIDI export options: pattern vs song, channel mapping | still blocked | native comparison |

### §2 Document model & editing

| Item | Verdict | Reason |
| --- | --- | --- |
| Channels | still blocked | native comparison: per-note parameter rendering |
| Patterns | still blocked | native comparison |
| Playlist | still blocked | binary format: track/arrangement creation records; bulk track edits landed on batch-3, surfaces verified on batch-4 |
| Mixer | still blocked | native render comparison |
| Automation | still blocked | binary format: native automation target records |
| Tempo automation | still blocked | native render comparison |
| Channel colors | still blocked | binary format: color record variants |
| Swing / groove settings | still blocked | native semantics: groove template behavior |
| Project settings | still blocked | binary format: field layouts |
| Layer channels route notes to descendants (bullet) | still blocked | native-gated: Layer Sequential mode field and native Random sequence; routing termination on cyclic graphs verified with tests on batch-4 |

### §3 Audio engine & mixer DSP

| Item | Verdict | Reason |
| --- | --- | --- |
| Windows: WASAPI (shared + exclusive), DirectSound, ASIO | still blocked | platform/hardware: Windows device layer |
| macOS: CoreAudio | still blocked | platform/hardware: macOS device layer |
| Linux: ALSA, PulseAudio/PipeWire, JACK | still blocked | platform/hardware: device layer verification |
| Device enumeration, sample-rate / buffer-size negotiation | still blocked | platform/hardware |
| Lock-free audio callback | already done (batch-4) | verified with 20,000-command flood and Arc-swap churn stress tests; remainder is native timing comparison |
| Lock-free command queue (UI to audio) | already done (batch-2, batch-4) | streaming transport pause/resume runs through the queue; stress-tested on batch-4 |
| Lock-free metering/state queue (audio to UI) | still blocked | native comparison: meter ballistics |
| Sample-accurate event scheduling within a buffer | still blocked | native comparison |
| Underrun/dropout detection and reporting | already done (batch-2) | event model extended (StreamUnderrun, ProjectStreamFinished, take_events drain); remainder: WASAPI exclusive loop does not publish yet (platform) |
| Denormal protection | still blocked | native comparison |
| Up to 500 insert tracks + Master (Mixer manual) | still blocked | native comparison: track-count behavior already matches the manual |
| Saved Mixer send editing | still blocked | binary format: provisional send route/gain mappings unconfirmed; graph verified on batch-4 |
| Per-track FX chains (10 slots each) | still blocked | native comparison: hosted-plugin rendering |
| Paired Mixer send levels and routing flags (bullet) | still blocked | binary format: provisional mappings; verified internally on batch-4 |
| Plugin delay compensation (PDC) | still blocked | binary format + native: latency reporting chain |
| 32-bit float internal processing | still blocked | native comparison: f32 send graph verified on batch-4 (106 tests) |
| Multi-threaded offline rendering | still blocked | native comparison / architecture |
| Offline decoding | already done (batch-4) | decode-by-content and malformed-input tests (39 media tests); remainder: Opus would need a new dependency |
| Selected clip rendering and consolidation | still blocked | binary format: per-instance variant serialization |
| Playlist playback | still blocked | native comparison |
| Real-time sampler voice management | still blocked | native comparison |
| Resampling (project vs sample vs device rate) | still blocked | native comparison: SRC algorithm output |
| Reverse, sample-loop, ping-pong playback (bullet) | already done (batch-5) | waveform previews follow Reverse in all views; playback honors the kind-0 `0x46` bit; remainder is native comparison |
| Audio recording into Playlist | still blocked | platform/hardware: device input path |
| Edison-class audio editor | still blocked | Edison scripting: the manual documents the Python `.pyscript` runner workflow, but the function reference ships only as `Reference.txt` inside the FL installation and is not published; a spectral band-gain tool landed on batch-6 |
| Latency-compensated input monitoring | still blocked | platform/hardware |

### §5 Plugin hosting

| Item | Verdict | Reason |
| --- | --- | --- |
| VST3 hosting: load, process, native editor | still blocked | native comparison + platform windowing for native editors |
| VST3 audio effect processing | still blocked | native comparison |
| VST3 state save/restore via FLP `0xD5` record | still blocked | binary format: state record internals |
| VST3 parameter automation | still blocked | native comparison |
| VST2 hosting | still blocked | native comparison; Mixer effect tail refresh after parameter changes landed on batch-4 (protocol-level, no VST2 fixture in repo) |
| CLAP hosting | still blocked | native comparison |
| AU hosting (macOS) | still blocked | platform: macOS only |
| 32-bit plugin bridging on 64-bit host | still blocked | platform/architecture |
| Plugin crash isolation (sandboxed scanning) | still blocked | platform/architecture |
| Latency reporting per plugin (feeds PDC) | still blocked | native comparison + PDC chain |

### §6 Piano roll

Behavioral controls for the tools below are documented in the manual's Piano roll
tool pages (e.g. `pianoroll_flam.htm`: Time, Absolute, Before, Velocity, Group
notes) and are implemented; the remaining gaps are native tool-window/seed
comparisons and unpublished preset values, which the manual does not give.

| Item | Verdict | Reason |
| --- | --- | --- |
| Selection | still blocked | native comparison |
| Tools | still blocked | native comparison: tool dialog behavior |
| FL Studio score files | still blocked | binary format: `.fsc` layout |
| Delete channel notes / selected indexes (bullet) | already done (batch-1) | implemented; batch-1 closed the test-coverage gap |
| CLI adds notes with neutral defaults (bullet) | already done (batch-1) | implemented; batch-1 closed the test-coverage gap |
| Generate seeded notes on the snap grid | still blocked | native comparison: generator distribution |
| Quantize starts (strength, swing, sensitivity) | still blocked | native comparison |
| Rotate notes by one snap step | still blocked | native comparison |
| Legato / Quick Chop (bullet) | still blocked | native comparison |
| Score flipper (bullet) | still blocked | native comparison |
| Strum | still blocked | native comparison: preset values not published |
| Flam | still blocked | manual incomplete: controls documented and implemented; the Options preset names/values are not published |
| Randomize (bullet) | already done (batch-4) | verified with seeded tests; remainder is native seed comparison |
| Humanize (bullet) | still blocked | native comparison: seed/distribution |
| Arpeggiator | still blocked | native comparison |
| Riff Machine | still blocked | native semantics: chord voicing/progression behavior |
| Claw machine (bullet) | still blocked | native comparison |
| Limit pitches into a key range (bullet) | still blocked | native comparison |
| Slice-at-tick (bullet) | still blocked | native comparison |
| Scale Levels (bullet) | still blocked | native comparison |
| Articulate (bullet) | still blocked | native comparison |
| Per-note slide flag display/toggle (bullet) | still blocked | binary format: slide serialization against native saves |
| Ghost channels (Alt/Option+V) | still blocked | binary format: the manual fully documents the behavior (grey ghost notes, double right-click swaps channel, Ctrl+Alt+V editable ghosts) and it is implemented; the blocker is the FLP clip scale field's timeline semantics, which no manual page covers |
| 16-color picker / Change color (bullet) | still blocked | binary format: color-group record against native saves |
| Chord Stamp | still blocked | native comparison: stamp voicings |
| Event editor (per-note values) | still blocked | native comparison |
| Scale highlighting | still blocked | native comparison: behavior and scale list documented in the manual and implemented; remaining scales verified only against native |

### §7 Channel Rack & step sequencer

| Item | Verdict | Reason |
| --- | --- | --- |
| Channel list with plugin names, sliders, colors (bullet) | already done (batch-5) | verified implemented; remainder is native comparison |
| Channel Fine Tune (`0x8E`) | still blocked | native comparison |
| Pattern/bar step grid (bullet) | still blocked | native comparison |
| Zipped Channel Rack state (`0x0F`) (bullet) | already done (batch-5) | verified implemented |
| Channel Rack Swing menu / global `0x0B` mix (bullet) | still blocked | native semantics: groove interaction |
| Channel Parameters: Enable main pitch / Add to Key bytes (bullet) | still blocked | binary format: remaining parameter bytes unestablished |

### §8 Playlist & arrangement

| Item | Verdict | Reason |
| --- | --- | --- |
| Clip display and selection, per-layout edits | still blocked | binary format: 88-byte per-arrangement clip ID tail offsets unestablished; raw clip flags decoded and editable on batch-5 |
| Playlist Clip Focus | still blocked | native comparison |
| Individual clip mute (T shortcut, context menu) | still blocked | binary format: the muted clip flag bit is a candidate/inferred mapping (see `PLAYLIST_CLIP_MUTED_FLAG` note in `src/lib.rs`) needing a native save |
| Audio Clip waveform previews follow Reverse (bullet) | already done (batch-5) | verified in Combined, Stereo, and spectral views |
| Audio Clip Make unique / Make unique as sample | still blocked | binary format: new-object record creation |
| Browser audio dropped on Playlist creates Audio Clip (bullet) | already done (batch-3) | implemented via synthesized record |
| Audio-channel clips: waveforms / spectral preview, legacy crop | still blocked | native comparison |
| Channel Rack/CLI kind-4 `0x46` Reverse bit | already done (batch-5) | CLI channels summary prints `sample_reversed`; playback/renders reverse |
| Playlist Audio Clip per-instance gain | still blocked | binary format: per-instance variant serialization |
| Playlist Audio Clip instance controls (gain, pan, fades, pitch) | still blocked | binary format: per-instance variant serialization |
| Pattern clips | still blocked | binary format + native comparison |
| Double-click Pattern Clip follows "Edit patterns in" (bullet) | still blocked | native windowing behavior |
| Time markers and meter records CRUD (bullet) | still blocked | native comparison: remaining marker-type semantics; basic create/edit used by batch-8's render-length work |
| Playlist track mute, solo, basic grouping | still blocked | binary format: grouping records |
| Ctrl/Cmd+A select all, Ctrl/Cmd+D clear, Shift+I invert (bullet) | already done (batch-6) | Edit menu selection commands added on batch-6; selection ops existed |
| Performance Mode (Start marker) | still blocked | native comparison |
| Playlist recording | still blocked | platform/hardware: input recording path |

### §9 Mixer UI & state

| Item | Verdict | Reason |
| --- | --- | --- |
| Mixer track view | still blocked | native comparison |
| Native Mixer render comparison | still blocked | native render comparison |
| Insert phase invert and stereo swap (bullet) | still blocked | native comparison |
| FX slot summaries (names, colors, state counts) | still blocked | binary format: unscoped state records |
| Mixer inspector send levels / on-off flags (bullet) | still blocked | binary format: remaining send records |
| Track freezing | still blocked | native comparison + render architecture |
| Stem export (multi-track render) | still blocked | native comparison; batch-8 applied the manual's song-length floor to stem renders |
| Mixer state export/import | still blocked | binary format: `.fst` mixer-state variant |

### §10 Browser

| Item | Verdict | Reason |
| --- | --- | --- |
| Local file browser (folders, filters, FLP handling) | implemented (batch-8) | middle-click load-on-selected-channel added per `browser.htm`; remaining gaps are FL Cloud / online content (service-gated) and indexed search behavior |
| Preview player, five-second preview (bullet) | already done (batch-5) | multichannel samples fold down to stereo for preview; mono/stereo pass through bit-exact; remainder: Opus preview needs a new dependency |
| Case-insensitive name filtering (bullet) | still blocked | native comparison: text-filter semantics across views; Browser MIDI-file and plugin-format filters landed on batch-5 |
| Browser lists/filters scanned VST2/VST3 candidates (bullet) | already done (batch-5) | plugin format filter (All/VST2/VST3/CLAP/AU) added; remainder is native scan comparison |

### §11 Automation & modulation

| Item | Verdict | Reason |
| --- | --- | --- |
| Type-5 channel points | still blocked | binary format: point record layout |
| Event automation (pattern-scoped) | still blocked | binary format + native comparison |
| CLAP Pattern Event parameter automation | still blocked | native comparison |
| CLAP Mixer Event parameter automation | still blocked | native comparison |
| Playlist Automation focus source menu / placement (bullet) | still blocked | binary format: envelope point records |
| Native Automation target records | still blocked | binary format |
| Internal LFO | still blocked | native comparison |
| Remote control / MIDI learn ("Multilink to controllers") | still blocked | platform/hardware + native learn semantics |
| Audio-rate vs control-rate parameter smoothing (dezippering) | still blocked | live Sampler volume/expression/pan ramping landed on batch-3; plug-in parameter dezippering has no established semantics and live Mod X / Mod Y still step into filter state |

### §12 Sampler & audio editing

| Item | Verdict | Reason |
| --- | --- | --- |
| Channel Parameters time/pitch/multiplier/mode fields | still blocked | native comparison: stretch algorithm output |
| Sample Editor export / async preview with stretch (bullet) | already done (batch-5) | verified implemented (0.5x to 2.0x); remainder is other editor tools (native) |

### §13 Rendering & export

| Item | Verdict | Reason |
| --- | --- | --- |
| Full-song render through the complete mixer graph (bullet) | still blocked | native render comparison; batch-8 added the manual's length hierarchy to this path |
| Formats | still blocked | native comparison: encoder output; Opus would need a new dependency |
| Render options | implemented (batch-8) | length hierarchy and Disable maximum polyphony per `fformats_save_export.htm` (see Implemented table); sub-options still blocked individually: Enable insert/master effects (no contained gate in the processor architecture), Wrap remainder last-bar rule (renderer cannot isolate last-bar decays; boundary semantics undetermined), Save tempo information (WAV metadata encoding unspecified), Save loop markers (which loop unspecified) |
| Stem export | still blocked | native comparison; length floor applied on batch-8 |
| Selected Playlist clip render | still blocked | binary format: per-instance variants |
| Pattern Clip render and consolidation | still blocked | binary format + native comparison |
| Playlist track freeze | still blocked | native comparison |
| Pattern render | still blocked | native comparison |

### §14 MIDI, controllers & scripting

| Item | Verdict | Reason |
| --- | --- | --- |
| MIDI input | still blocked | platform/hardware + native comparison |
| MIDI input scale snapping | already done (batch-2) | verified implemented (manual documents the snapping behavior); remainder is native comparison |
| MIDI Thru | already done (batch-6) | internal routing verified (external engine plus live audition to Sampler/Layer/VST2/VST3); multi-channel internal routing has no target model defined in repo or manual |
| MIDI output | still blocked | platform/hardware |
| MIDI Mixer track information | still blocked | native: MIDI scripting API surface against a device |
| MIDI Mixer track selection | still blocked | native: scripting API surface |
| MIDI Mixer routing | still blocked | native: scripting API surface |
| MIDI channel-to-Mixer linking | still blocked | native: scripting API surface |
| MIDI CC links learned for VST3/VST2 parameters (bullet) | still blocked | native learn semantics |
| MIDI device scripts | still blocked | native: the manual documents the scripting API, but hosting user device scripts needs the native environment semantics |
| MIDI script Browser navigation, zoom and snap controls | still blocked | native: scripting API surface |
| MIDI script Patterns properties | still blocked | native: scripting API surface |
| MIDI Mixer track controls | still blocked | native: scripting API surface |
| MIDI global transport | still blocked | native: scripting API surface |
| Controller templates | still blocked | platform/hardware: device-specific templates |
| MIDI script recording resolution | still blocked | native comparison |

### §15 UI shell, menus & workflow

| Item | Verdict | Reason |
| --- | --- | --- |
| Window management | still blocked | native windowing comparison |
| Main menu | already done (batch-6) | hint bar from note hover and menu-bar project picker with recents added; remainder is native menu completeness |
| Detached windows | already done (batch-6) | monitor-membership check drops off-screen saved positions; remainder is native comparison |
| Browser and transport toolbar | still blocked | native comparison |
| Touch gestures (two-finger scroll, pinch zoom) (bullet) | still blocked | platform/hardware: touch input |
| Themes / UI scaling | already done (batch-6) | persisted touch-sized controls option added; remainder is native theme comparison |
| Keyboard shortcuts | still blocked | native comparison: shortcut completeness |
| Undo/redo document snapshots, 64 MiB history (bullet) | already done (batch-6) | verified by inspection; plug-in native-editor and CLAP state changes cannot be captured in the synchronous snapshot path (blocked) |
| Autosave and recovery | already done (batch-3) | Autosave settings window added; remainder is native recovery comparison |

### §16 Platform, packaging & OS integration

| Item | Verdict | Reason |
| --- | --- | --- |
| Windows installer (NSIS/WiX) | still blocked | platform: Windows packaging |
| macOS app bundle, notarization | still blocked | platform: macOS packaging |
| Windows: ASIO support | still blocked | platform/hardware |
| macOS: native menu bar, file associations | still blocked | platform: macOS integration |

## Remaining blocked, grouped by blocker type

- **Binary format** (record fields, flag bits, serialization the manual never
  documents): most of §1; §2 Playlist track/arrangement creation, Automation,
  Channel colors, Project settings; §3 send mappings, PDC amounts, clip variant
  serialization; §5 `0xD5` state records; §6 score files, slide/color records,
  ghost-channel clip scale field; §7 remaining parameter bytes; §8 clip IDs,
  clip mute bit, Make unique, grouping, per-instance controls; §9 FX/send state,
  mixer `.fst`; §11 automation point/target records.
- **Native render/save comparison** (needs FL Studio in the loop): §2 Mixer and
  Tempo automation; §3 mixer DSP, resampling, voice management, playback; §5
  plugin hosting output; §6 most tool internals and seed distributions; §9 Mixer
  render comparison, freezing; §12 stretch output; §13 formats and render paths;
  §14 scripting surfaces; §15 shell completeness.
- **Platform/hardware**: §3 device layers (WASAPI/CoreAudio/ALSA/JACK/ASIO),
  recording and monitoring; §5 AU hosting and 32-bit bridging; §8/§11 recording
  and MIDI learn devices; §14 controllers and templates; §15 touch gestures;
  all of §16.
- **Manual incomplete** (behavior partially published): §6 Flam presets;
  §13 Wrap remainder boundary semantics, Save tempo information encoding,
  Save loop markers target; Edison scripting function reference (ships only as
  `Reference.txt` inside the FL installation).
