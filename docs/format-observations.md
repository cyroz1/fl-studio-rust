# FLP format observations

These notes record direct inspection of the bundled `Data/Demo projects/NewStuff.flp` file. They describe one FL Studio 25.2.3.5164 project and are not a claim that every FLP generation uses identical fields.

## Container

The file is 313,356 bytes. Its first 14 bytes decode as:

| Offset | Bytes | Observation |
| --- | --- | --- |
| `0x00` | `46 4C 68 64` | ASCII `FLhd` marker |
| `0x04` | `06 00 00 00` | Header content length is 6 bytes |
| `0x08` | `00 00` | Format field is 0 |
| `0x0A` | `2D 00` | Legacy channel count field is 45 |
| `0x0C` | `60 00` | PPQ field is 96 |
| `0x0E` | `46 4C 64 74` | ASCII `FLdt` marker |
| `0x12` | `F6 C7 04 00` | Event-stream length is 313,334 bytes |

The event stream ends exactly at the end of this file; no trailing bytes are present in this fixture.

## Event observations

The Rust reader found 4,404 event boundaries in this fixture. The first events include:

| File offset | Opcode | Payload | Observation |
| --- | --- | --- | --- |
| `0x16` | `C7` | `25.2.3.5164\0` | ASCII version string; preceding length byte is `0C` |
| `0x24` | `9F` | `2C 14 00 00` | Little-endian value 5164, matching the build suffix in the version string |
| `0x30` | `AC` | `01 01 00` | Three-byte payload; the next event begins at `0x34` |
| `0x34` | `C0` | UTF-16LE text | Begins with the `FL Studio 25...` product/version banner |
| `0x94` | `9C` | `F0 49 02 00` | Little-endian value 150,000; displayed as 150 BPM by the current metadata reader |
| `0xA3` | `11` | `04` | Numerator-like global field, currently displayed as 4 |
| `0xA5` | `12` | `04` | Denominator-like global field, currently displayed as 4 |

The opcode-size rules used by the reader are the conventional four groups visible in the event stream: byte payloads for `00`–`3F`, two-byte payloads for `40`–`7F`, four-byte payloads for `80`–`BF`, and length-prefixed data for `C0`–`FF`. In this fixture, `AC` is a version-specific exception with three payload bytes. Corpus scanning confirms these framing rules across the installed FLP/FST files.

## Channel grouping observations

The stream contains 45 `0x40` markers, matching the header's channel count. At the first marker, the following fields occur before the next channel marker: a one-byte `0x15` value of `2`, a UTF-16LE `0xC9` string `Harmless`, and a UTF-16LE `0xCB` string `Synth Bass`. A later one-byte `0x00` event carries `1`. Across all 45 groups, the `0xCB` text produces plausible channel labels such as `Grv Kick 27`, `Piano Dark`, and `TEMPO`; `0xC9` produces plugin labels such as `3x Osc` and `FLEX` where present.

The current reader therefore presents `0x40` groups as channel summaries, treating `0x15` as kind, `0x00` as enabled, `0xC9` as plugin identifier, and `0xCB` as display name. A same-length controlled rename of the `Synth Bass` channel to `Synth Test` changed the exported MIDI track name accordingly, confirming the `0xCB` mapping. The complete event ranges remain accessible for later decoding.

For channel kind `4` in the `Ookay - Thief.flp` demo project, the `0xC4` data event decodes as a UTF-16LE sample source path. Channel 29 (`Main Vox`) stores `%FLStudioFactoryData%\Data\Patches\Misc\Used by demo projects\Ookay - Thief\ookay thief vox.wav`, and the referenced file exists in the installation's `Data` tree. The channel summary now exposes this path while the original event bytes remain losslessly preserved. Macro expansion and external sample decoding are not implemented yet.

## Pattern and Piano roll note observations

Pattern identity is carried by `0x41` word events. In the FL 25.2.3 NewStuff project, note-bearing `0x41` events are immediately followed by `0xE0` data events. Their payload lengths are multiples of 24 bytes. Reading each record as a little-endian position, flags, channel id, length, key, group, and byte-sized note properties yields 213 stored notes across pattern ids 1, 2, 3, 4, 6, 7, and 8. The first four records in pattern 1 (positions 0, 384, 768, and 1,152; keys 47, 44, 49, and 39; length 384; velocity 100) match the original MIDI export's Synth Bass note-on/off events at ticks 0–384, 384–768, 768–1,152, and 1,152–1,536. The installed FL 10.9.0 `Charles Deluxe - Performance Demo.flp` also uses a 1,440-byte `0xE0` payload after pattern id 1, yielding 60 records across channel ids 11 and 12. This corpus evidence contradicts a simple rule that `0xE0` is only used starting in FL 25; the reader therefore checks the pattern marker context and accepts `0xD0` or `0xE0` rather than selecting an opcode solely by version. NewStuff's MIDI export contains 2,505 note spans, far more than its 213 stored note records. Playlist pattern repeats likely account for the expansion, but clip-to-pattern playback mapping remains undecoded.

The 10.9.0 sample stores pattern names as single-byte Windows-1252 text, while the 25.2.3 sample stores them as UTF-16LE. The current string decoder switches at FL 11.5 based on a public parser changelog; no installed project around that boundary was found, so it remains unconfirmed locally ([PyFLP changelog](https://github.com/demberto/PyFLP/blob/master/CHANGELOG.md)). Pattern names and note counts are readable for the two local fixtures; note timing and property fields have not yet been compared against the original Piano roll display. The official manual describes the Piano roll as channel-targeted note data within patterns and notes that Playlist Pattern Clips arrange those patterns on the timeline ([Piano roll](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/pianoroll.htm), [Patterns](https://www.image-line.com/fl-studio-learning-content/fl-studio-online-manual/html/playlist_patterns.htm)). A public reverse-engineered record layout was used as a cross-check, but local FL 10.9 bytes take precedence where the two disagree ([format reference](https://github.com/dawhubapp/flpdiff/blob/main/docs/fl-format/flp-format-spec.md)).

## Playlist clip observations

The FL 25.2.3 NewStuff project has an explicit arrangement marker at event 2257: `0x63` carries arrangement id 0, and the following `0xF1` event contains the UTF-16LE name `Arrangement`. Event 2260 (`0xE9`) carries 119,520 bytes, exactly 1,494 records of 80 bytes each. The first record begins with position 0, pattern base `0x5000`, item index 29, length 30,816, raw track index 471, group 0, and flags `0x0040`. The last 16 bytes contain a scale value of 1.0 plus eight trailing bytes. Records retain their original bytes in the event stream.

The installed FL 10.9.0 Charles Deluxe project has no `0x63` arrangement marker. Event 531 (`0xE9`) contains 1,152 bytes, exactly 36 records of 32 bytes, embedded before the final `0x62` channel boundary. The reader treats such unmarked records as the implicit arrangement id 0. Its record prefix uses the same position, base, item, length, raw-track, group, unknown-word, flags, four opaque bytes, and two float fields at offsets 0 through 31; unlike the newer record it has no clip id or scale field. One legacy clip refers to pattern id 1 through item index `0x5001`.

For NewStuff, clip `item_index` values at or above `pattern_base` resolve to pattern ids by subtraction; lower values resolve to channel ids. For example, clip item 29 points to channel 29 (`Main Vox`) while its visible Playlist row is named `Thief Vox`. Item 34 points to channel 34 (`MELODICS #2`), which is an automation channel. This distinguishes the clip's source object from the Playlist row that contains it. The current API resolves these as pattern or channel targets and retains the separate row name.

The parser chooses 32-byte records before FL 21, 60-byte records for FL 21–24, and 80-byte records for FL 25 and later. The local corpus directly confirms the 32-byte and 80-byte layouts; the 60-byte middle layout follows the public format reference and still needs a local FL 21–24 fixture. When a project has no version metadata, the parser uses record length only when that length identifies exactly one supported layout; otherwise it reports the ambiguity instead of guessing. The decoded visible track number uses the reference's `499 - raw_track_index` mapping, while retaining the raw value.

The same NewStuff event stream contains 500 Playlist track records (`0xEE`, 70-byte payloads). Each record is followed by an optional one-byte `0x2B` event and a `0xEF` UTF-16LE name. The decoder retains the 70-byte state payload and the adjacent byte without assigning undocumented meanings. Track records are numbered from one; a clip's decoded zero-based row maps to that identifier by adding one. For example, the first Playlist clip has raw track index 471, decoded row 28, and resolves to Playlist track id 29, `Thief Vox`, which matches the label in FL Studio. The next clip resolves to `MELODICS #2`. This confirms the clip-to-track-name association for this project but does not yet decode the rest of the `0xEE` state.

Opening NewStuff in the original application exposed the Channel Rack, Playlist, Piano roll, Mixer, and a Harmless instrument window. The Playlist shows pattern clips, audio clips, automation, and a tempo lane; the Piano roll includes the keyboard, bar grid, note lane, and velocity lane; the Mixer shows insert strips and effect slots. The new Rust desktop shell now has early Playlist, Channel Rack, and Piano roll views plus a Mixer placeholder. It can edit and save existing clip positions/lengths, note fields, and project tempo. It does not yet reproduce FL Studio's controls or window behavior closely, decode Mixer state, or play audio in real time.

The rewrite's plug-in compatibility direction is to host compatible plug-in binaries already installed on the system, restore each plug-in's project state, expose its editor window, and connect its parameters, MIDI/audio routing, automation, and offline rendering to the DAW. The user has clarified that FL's bundled plug-in DSP does not need to be reimplemented. Project records and opaque state for an unavailable plug-in still need to survive load/save. The current prototype can load VST3 plug-ins and their native editors, list and set host-reported parameters, and schedule pattern notes into an offline VST3 render. State conversion, project-wide routing, Mixer processing, automation, and complete-song rendering are still unimplemented.

The current Windows installation has nine VST3 bundles (Analog Lab V, Hive, Nexus, Portal, Serum 2, ShaperBox 3, Spire, Spoton, and ZENOLOGY) and one VST2 DLL candidate (`Hive.dll`). The new `plugin-scan` command locates these paths without loading binaries. It does not yet validate VST2 DLLs or inspect VST3 classes. Steinberg's published licensing FAQ says VST2 host distribution requires a license agreement signed before October 2018 and that new VST2 agreements are no longer offered; whether the project has an applicable legacy agreement remains unresolved ([Steinberg licensing FAQ](https://github.com/steinbergmedia/vst3_dev_portal/blob/main/src/pages/FAQ/Licensing.md)).

Channel plug-in records use adjacent `0xD4` wrapper metadata and `0xD5` variable-length plug-in data events in the observed projects. The local Harmless channel has a 52-byte `0xD4` and a 4,629-byte `0xD5`; FLEX has a 52-byte `0xD4` and a 2,707-byte `0xD5`. A public parser maps the VST envelope's subevent IDs to plug-in name, path, vendor, GUID, and state fields ([PyFLP plug-in model](https://github.com/demberto/PyFLP/blob/master/pyflp/plugin.py)). The Rust API now decodes these identity fields from recognized `0xD5` envelopes while preserving the complete original event bytes. In the local `zenology.flp`, channel 0's 164,850-byte payload uses marker 12 and contains a 164,552-byte nested state, name `ZENOLOGY`, vendor `Roland Cloud`, bundle path `/Library/Audio/Plug-Ins/VST3/Roland/ZENOLOGY.vst3`, and a 16-byte GUID. Converting that GUID's first three fields to the host's byte order produces class UID `324755DF8FDF4788B4CE5B70A8037EC4`, which loaded the installed ZENOLOGY class in a local probe. The desktop UI can match a local VST3 by bundle name, pass its class UID, and try the original `0xD5` event as state. Passing only the nested state subevent to this host caused the ZENOLOGY process to terminate with an access violation, so the app currently leaves the wrapper intact and treats state loading as an experiment. A host accepting bytes and opening its editor do not confirm exact preset restoration or rendering. This evidence covers one third-party plug-in and does not establish support for other envelope versions or project/plugin combinations.

## State preset observation

The installed `Data/Patches/Channel presets/3x Osc/Bassline.fst` preset is 829 bytes and uses the same `FLhd` and `FLdt` chunk markers. Its header has format value 32, legacy channel count 9, and PPQ 96. The event stream starts with version `3.5.2`, and the reader finds 41 events plus one channel marker. This demonstrates that the generic envelope reader can handle this older state preset; it does not establish compatibility with old `.flp` project files.

## Standard MIDI File observation

The installed `Data/Patches/Scores/FPC drumloops/Ambient Loops/fpc_ambient_groove_01.mid` file is 221 bytes. The reader recognizes it as format 1 with two tracks and a 96-tick-per-quarter-note division. Its first track is named `Tempo` and contains a 120 BPM tempo event. Its second track is named `CB Custom 1`, ends at tick 364, and contains 19 note-on and 19 note-off messages. The MIDI reader now exposes paired channel/key note spans while keeping the raw track event stream and original file bytes available.

## Installed project and preset corpus

The recursive corpus scan found 8,878 `.flp` and `.fst` files under `Data`: 165 projects and 8,713 state/preset files. The current chunk/event reader parsed all 8,878 without a framing error. The projects contain 54 distinct version strings from `10.9.0` through `26.1.0.5528`; the presets contain 237 distinct version strings from `2.5.4` through `25.2.99.5487`. Header formats across the combined corpus are 0, 32, 48, and 64, and PPQ values range from 24 through 768.

This is evidence for envelope and event-boundary coverage of the installed corpus, including older preset files. It does not establish semantic project compatibility: most event payloads remain opaque, and the installed project corpus does not include the earliest project versions.

## Compatibility limits

- The container reader parsed the installed corpus, but the semantic mappings above still come from a small number of inspected files. External projects, damaged files, and the earliest project versions remain unverified.
- Most event payload meanings remain unknown. Unknown events are retained byte-for-byte so later semantic support can be added without discarding data.
- VST3 hosting, editor integration, parameter access, and a single-pattern/channel offline render path exist as prototypes. FLP plug-in state restoration, VST2 hosting, automation, project-wide MIDI/audio routing, Mixer effects, and complete-song rendering are not implemented. The target is to use installed plug-in binaries, not recreate FL's bundled plug-in DSP.
- The base tempo field at `0x9C` is confirmed as milli-BPM by changing an empty project's value from 140,000 to 100,000 and observing 100 BPM in FL Studio's MIDI export. Time-signature fields `0x11` and `0x12` still need controlled-save confirmation. NewStuff contains a separate channel named `TEMPO`; its automation payload is not yet decoded.
