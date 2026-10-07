# Original FL Studio oracle experiments

These experiments use the installed FL Studio command-line MIDI export on copies of bundled projects. The source projects under `Data` are left unchanged. The manual describes `/M` as the batch MIDI render option: [Exporting Audio & MIDI](https://www.image-line.com/fl-studio-learning/fl-studio-beta-online-manual/html/fformats_save_export.htm).

## Project name and channel score export

`Data/Demo projects/NewStuff.flp` exported to a 42,801-byte, format-1 MIDI file at 96 PPQ. The export contained seven tracks: a tempo/marker track and five named instrument tracks with these note counts:

| MIDI track | Notes |
| --- | ---: |
| Synth Bass | 85 |
| Saw Bass | 94 |
| White Noise | 210 |
| Piano Dark | 196 |
| Tinke Bell | 1,920 |

Changing the equal-length UTF-16LE channel label `Synth Bass` to `Synth Test` in a copied FLP changed the exported MIDI track name to `Synth Test`. This confirms that event `0xCB` is the displayed channel name and that `/M` loaded the edited project.

## Base tempo and tempo automation

The empty template at 140 BPM exported MIDI with a 140 BPM tempo event. Editing its `0x9C` field from 140,000 to 100,000 using `set-tempo` produced an export at 100 BPM. This confirms that the project-level `0x9C` value uses thousandths of a BPM.

Changing NewStuff's `0x9C` value from 150,000 to 100,000 left its exported MIDI at 150 BPM. NewStuff has an enabled channel with id 40 and display name `TEMPO`; the channel contains an `0xEA` data event of 205 bytes. The exported MIDI tempo track has 3,079 tempo events from tick 0 through tick 55,297. Its tempo varies from 150 BPM down to 60 BPM near tick 55,295 and returns to 150 BPM at tick 55,297. This strongly indicates that the `TEMPO` channel automation overrides the project base tempo during export. The 205-byte automation payload layout remains undecoded, so the curve has only been observed through the original exporter, not reconstructed from the FLP bytes.

## Rust exporter comparison

The new `export-midi-song` path was run against the same `NewStuff.flp`. Its format-1 output has the same PPQ (96), base tempo (150 BPM), channel names, and note counts for Synth Bass (85), Piano Dark (196), and Tinke Bell (1,920). It currently exports 104 Saw Bass notes instead of the original export's 94, and 224 White Noise notes instead of 210. Its conductor track contains only the base tempo, so it also omits NewStuff's 3,079-event tempo automation map. These differences show that Playlist repeat/export rules and automation-to-tempo conversion still need to be matched against FL Studio.

## Arrangement payload boundaries

NewStuff contains one `0xE9` event with a 119,520-byte payload. Changing the first payload dword in a copied file, and separately changing the first character in event 114's 12-entry `0xFD` UTF-16LE string, left the MIDI export byte-identical. These experiments show that those changes do not affect this project's MIDI export. They do not establish the full meaning of either event; `0xE9` remains a candidate Playlist/arrangement container and the `0xFD` field's role is unknown.
