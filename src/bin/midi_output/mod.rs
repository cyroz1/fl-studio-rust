use std::collections::BTreeMap;
use std::time::Duration;

use flp_rebuild::midi::{
    MidiChannelMapping, MidiEventKind, MidiFile, fl_note_release_to_midi_velocity,
};
use flp_rebuild::{FlpDocument, Pattern};
use midir::{MidiOutput, MidiOutputConnection as MidirOutputConnection};

const MAX_PATTERN_PREVIEW_NOTES: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledMidiMessage {
    pub after: Duration,
    pub bytes: [u8; 3],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledMidiTickMessage {
    pub tick: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OrderedMidiMessage {
    tick: u64,
    priority: u8,
    order: usize,
    bytes: [u8; 3],
}

struct OrderedMidiTickMessage {
    tick: u64,
    priority: u8,
    order: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiOutputDevice {
    pub id: String,
    pub name: String,
}

pub struct ConnectedMidiOutput {
    connection: MidirOutputConnection,
    pub device_id: String,
    pub device_name: String,
}

pub const MIDI_TEST_NOTE_ON: [u8; 3] = [0x90, 60, 96];
pub const MIDI_TEST_NOTE_OFF: [u8; 3] = [0x80, 60, 0];
pub const MIDI_CLOCK_TICK: [u8; 1] = [0xF8];
pub const MIDI_START: [u8; 1] = [0xFA];
pub const MIDI_CONTINUE: [u8; 1] = [0xFB];
pub const MIDI_STOP: [u8; 1] = [0xFC];

pub fn midi_song_position_pointer_message(sixteenth_notes: u16) -> [u8; 3] {
    let pointer = sixteenth_notes.min(0x3FFF);
    [0xF2, (pointer & 0x7F) as u8, ((pointer >> 7) & 0x7F) as u8]
}

pub fn midi_clock_pulse_count_at_tick(tick: f64, ppq: u16) -> u64 {
    if ppq == 0 || !tick.is_finite() || tick <= 0.0 {
        return 0;
    }
    (tick * 24.0 / f64::from(ppq))
        .floor()
        .clamp(0.0, u64::MAX as f64) as u64
}

pub fn midi_sixteenth_notes_from_tick(tick: f64, ppq: u16) -> u16 {
    if ppq == 0 || !tick.is_finite() || tick <= 0.0 {
        return 0;
    }
    (tick * 4.0 / f64::from(ppq)).floor().clamp(0.0, 16_383.0) as u16
}

pub fn pattern_preview_messages(
    pattern: &Pattern,
    ppq: u16,
    tempo_bpm: f64,
) -> Result<Vec<ScheduledMidiMessage>, String> {
    if ppq == 0 || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project timing must have a positive PPQ and tempo".to_owned());
    }
    if pattern.notes.len() > MAX_PATTERN_PREVIEW_NOTES {
        return Err(format!(
            "Pattern {} exceeds the {}-note MIDI preview limit",
            pattern.id, MAX_PATTERN_PREVIEW_NOTES
        ));
    }

    let mut ordered = Vec::with_capacity(pattern.notes.len().saturating_mul(2));
    for (note_index, note) in pattern.notes.iter().enumerate() {
        let Some(key) = u8::try_from(note.key).ok().filter(|key| *key <= 127) else {
            continue;
        };
        if note.midi_channel > 15 || note.velocity > 127 || note.velocity == 0 {
            continue;
        }
        let channel = note.midi_channel;
        let start_tick = u64::from(note.position);
        let end_tick = start_tick.saturating_add(u64::from(note.length.max(1)));
        ordered.push(OrderedMidiMessage {
            tick: start_tick,
            priority: 1,
            order: note_index.saturating_mul(2),
            bytes: [0x90 | channel, key, note.velocity],
        });
        ordered.push(OrderedMidiMessage {
            tick: end_tick,
            priority: 0,
            order: note_index.saturating_mul(2).saturating_add(1),
            bytes: [
                0x80 | channel,
                key,
                fl_note_release_to_midi_velocity(note.release),
            ],
        });
    }
    ordered.sort_unstable_by_key(|event| (event.tick, event.priority, event.order));

    let micros_per_tick = 60_000_000.0 / (f64::from(ppq) * tempo_bpm);
    ordered
        .into_iter()
        .map(|event| {
            let micros = (event.tick as f64 * micros_per_tick).round();
            if !micros.is_finite() || micros < 0.0 || micros > u64::MAX as f64 {
                return Err("Pattern MIDI preview duration is out of range".to_owned());
            }
            Ok(ScheduledMidiMessage {
                after: Duration::from_micros(micros as u64),
                bytes: event.bytes,
            })
        })
        .collect()
}

pub fn playlist_playback_messages(
    document: &FlpDocument,
    arrangement_id: u16,
    start_tick: u64,
) -> Result<Vec<ScheduledMidiTickMessage>, String> {
    let bytes = MidiFile::encode_project_song(
        document,
        arrangement_id,
        MidiChannelMapping::PreserveNoteChannels,
    )
    .map_err(|error| error.to_string())?;
    let midi = MidiFile::parse(&bytes).map_err(|error| error.to_string())?;
    midi_tick_messages(&midi, start_tick)
}

fn midi_tick_messages(
    midi: &MidiFile,
    start_tick: u64,
) -> Result<Vec<ScheduledMidiTickMessage>, String> {
    let mut ordered = Vec::new();
    let mut earlier_controller_messages = Vec::<(u64, u64, Vec<u8>)>::new();
    let mut earlier_channel_state = BTreeMap::<(u8, u8), (u64, u64, Vec<u8>)>::new();
    let mut channel_state_at_start = BTreeMap::<(u8, u8), ()>::new();

    for (track_index, track) in midi.tracks().iter().enumerate() {
        let track_order = (track_index as u64) << 32;
        for (note_index, note) in track.notes().iter().enumerate() {
            let Some(end_tick) = note.end_tick() else {
                continue;
            };
            if note.velocity() == 0 || end_tick <= start_tick {
                continue;
            }
            let order = track_order | note_index as u64;
            ordered.push(OrderedMidiTickMessage {
                tick: note.start_tick().max(start_tick),
                priority: 3,
                order,
                bytes: vec![0x90 | note.channel(), note.key(), note.velocity()],
            });
            ordered.push(OrderedMidiTickMessage {
                tick: end_tick,
                priority: 0,
                order,
                bytes: vec![
                    0x80 | note.channel(),
                    note.key(),
                    note.release_velocity().unwrap_or(0),
                ],
            });
        }

        for (event_index, event) in track.events().iter().enumerate() {
            let MidiEventKind::ChannelVoice { status, data } = event.kind() else {
                continue;
            };
            let message_type = status & 0xF0;
            if matches!(message_type, 0x80 | 0x90) {
                continue;
            }
            let data_length = match message_type {
                0xC0 | 0xD0 => 1,
                0xA0 | 0xB0 | 0xE0 => 2,
                _ => continue,
            };
            if data.len() < data_length {
                return Err("Playlist MIDI export contains a short channel message".to_owned());
            }
            let mut bytes = Vec::with_capacity(data_length + 1);
            bytes.push(*status);
            bytes.extend_from_slice(&data[..data_length]);
            let state_key = channel_state_key(*status, message_type, &data[..data_length]);
            let order = track_order | event_index as u64;
            if event.absolute_tick() >= start_tick {
                if event.absolute_tick() == start_tick
                    && let Some(key) = state_key
                {
                    channel_state_at_start.insert(key, ());
                }
                ordered.push(OrderedMidiTickMessage {
                    tick: event.absolute_tick(),
                    priority: 2,
                    order,
                    bytes,
                });
            } else if message_type == 0xB0 {
                // Preserve the complete RPN sequence used to set pitch-bend range.
                earlier_controller_messages.push((event.absolute_tick(), order, bytes));
            } else if let Some(key) = state_key {
                let state = (event.absolute_tick(), order, bytes);
                if earlier_channel_state
                    .get(&key)
                    .is_none_or(|previous| (previous.0, previous.1) < (state.0, state.1))
                {
                    earlier_channel_state.insert(key, state);
                }
            }
        }
    }

    earlier_controller_messages.sort_by_key(|(tick, order, _)| (*tick, *order));
    for (_, order, bytes) in earlier_controller_messages {
        ordered.push(OrderedMidiTickMessage {
            tick: start_tick,
            priority: 1,
            order,
            bytes,
        });
    }

    for (key, (_, order, bytes)) in earlier_channel_state {
        if !channel_state_at_start.contains_key(&key) {
            ordered.push(OrderedMidiTickMessage {
                tick: start_tick,
                priority: 1,
                order,
                bytes,
            });
        }
    }

    ordered.sort_unstable_by_key(|message| (message.tick, message.priority, message.order));
    Ok(ordered
        .into_iter()
        .map(|message| ScheduledMidiTickMessage {
            tick: message.tick,
            bytes: message.bytes,
        })
        .collect())
}

fn channel_state_key(status: u8, message_type: u8, data: &[u8]) -> Option<(u8, u8)> {
    match message_type {
        0xA0 => data.first().map(|index| (status, *index)),
        0xC0 | 0xD0 | 0xE0 => Some((status, 0)),
        _ => None,
    }
}

pub fn enumerate_output_devices() -> Result<Vec<MidiOutputDevice>, String> {
    let output = MidiOutput::new("FL Studio Rebuild MIDI Output")
        .map_err(|error| format!("could not initialize MIDI output: {error}"))?;
    output
        .ports()
        .into_iter()
        .map(|port| {
            let name = output
                .port_name(&port)
                .map_err(|error| format!("could not read MIDI output name: {error}"))?;
            Ok(MidiOutputDevice {
                id: port.id(),
                name,
            })
        })
        .collect()
}

pub fn connect_output_device(device_id: &str) -> Result<ConnectedMidiOutput, String> {
    let output = MidiOutput::new("FL Studio Rebuild MIDI Output")
        .map_err(|error| format!("could not initialize MIDI output: {error}"))?;
    let port = output
        .ports()
        .into_iter()
        .find(|port| port.id() == device_id)
        .ok_or_else(|| "the selected MIDI output is no longer available".to_owned())?;
    let device_name = output
        .port_name(&port)
        .map_err(|error| format!("could not read MIDI output name: {error}"))?;
    let connection = output
        .connect(&port, "FL Studio Rebuild")
        .map_err(|error| format!("could not connect to MIDI output: {error}"))?;
    Ok(ConnectedMidiOutput {
        connection,
        device_id: device_id.to_owned(),
        device_name,
    })
}

impl ConnectedMidiOutput {
    pub fn send(&mut self, message: &[u8]) -> Result<(), String> {
        self.connection
            .send(message)
            .map_err(|error| format!("could not send MIDI output: {error}"))
    }

    pub fn send_all_notes_off(&mut self) -> Result<(), String> {
        let mut first_error = None;
        for channel in 0..16 {
            if let Err(error) = self.send(&all_notes_off_message(channel))
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for ConnectedMidiOutput {
    fn drop(&mut self) {
        let _ = self.send_all_notes_off();
    }
}

fn all_notes_off_message(channel: u8) -> [u8; 3] {
    [0xB0 | (channel & 0x0F), 123, 0]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use flp_rebuild::{Pattern, PatternNote};

    use super::{
        all_notes_off_message, midi_clock_pulse_count_at_tick, midi_sixteenth_notes_from_tick,
        midi_song_position_pointer_message, midi_tick_messages, pattern_preview_messages,
    };
    use flp_rebuild::midi::{MidiChannelMapping, MidiExportNote, MidiExportTrack, MidiFile};

    #[test]
    fn all_notes_off_uses_the_requested_midi_channel() {
        assert_eq!(all_notes_off_message(0), [0xB0, 123, 0]);
        assert_eq!(all_notes_off_message(15), [0xBF, 123, 0]);
    }

    #[test]
    fn pattern_preview_schedules_note_on_and_off_by_ppq_and_tempo() {
        let pattern = Pattern {
            id: 2,
            notes: vec![PatternNote {
                position: 48,
                length: 48,
                key: 64,
                midi_channel: 2,
                velocity: 96,
                release: 128,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };

        assert_eq!(
            pattern_preview_messages(&pattern, 96, 120.0).unwrap(),
            [
                super::ScheduledMidiMessage {
                    after: Duration::from_micros(250_000),
                    bytes: [0x92, 64, 96],
                },
                super::ScheduledMidiMessage {
                    after: Duration::from_micros(500_000),
                    bytes: [0x82, 64, 127],
                },
            ]
        );
    }

    #[test]
    fn pattern_preview_sends_note_off_before_a_new_note_at_the_same_tick() {
        let pattern = Pattern {
            notes: vec![
                PatternNote {
                    length: 48,
                    key: 60,
                    velocity: 96,
                    release: 32,
                    ..PatternNote::default()
                },
                PatternNote {
                    position: 48,
                    length: 24,
                    key: 60,
                    velocity: 80,
                    release: 128,
                    ..PatternNote::default()
                },
            ],
            ..Pattern::default()
        };

        let messages = pattern_preview_messages(&pattern, 96, 120.0).unwrap();
        assert_eq!(messages[0].bytes, [0x90, 60, 96]);
        assert_eq!(messages[1].bytes, [0x80, 60, 32]);
        assert_eq!(messages[2].bytes, [0x90, 60, 80]);
        assert_eq!(messages[3].bytes, [0x80, 60, 127]);
    }

    #[test]
    fn pattern_preview_rejects_invalid_project_timing() {
        assert!(pattern_preview_messages(&Pattern::default(), 0, 120.0).is_err());
        assert!(pattern_preview_messages(&Pattern::default(), 96, f64::NAN).is_err());
    }

    fn midi_file_with_notes(notes: Vec<MidiExportNote>) -> MidiFile {
        let mut track = MidiExportTrack::new("MIDI output", Some(0));
        track.end_tick = notes.iter().map(|note| note.end_tick).max().unwrap_or(0);
        track.notes = notes;
        let bytes = MidiFile::encode_export_tracks(
            &[track],
            96,
            120.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .unwrap();
        MidiFile::parse(&bytes).unwrap()
    }

    #[test]
    fn playlist_midi_messages_start_active_notes_at_the_seek_tick() {
        let midi = midi_file_with_notes(vec![MidiExportNote {
            channel: 2,
            key: 64,
            velocity: 96,
            release_velocity: 32,
            pan: 64,
            start_tick: 48,
            end_tick: 144,
            fine_pitch_offset_cents: 0,
        }]);

        let messages = midi_tick_messages(&midi, 96).unwrap();
        assert_eq!(
            messages,
            [
                super::ScheduledMidiTickMessage {
                    tick: 96,
                    bytes: vec![0x92, 64, 96],
                },
                super::ScheduledMidiTickMessage {
                    tick: 144,
                    bytes: vec![0x82, 64, 32],
                },
            ]
        );
    }

    #[test]
    fn playlist_midi_messages_send_note_off_before_a_same_tick_note_on() {
        let midi = midi_file_with_notes(vec![
            MidiExportNote {
                channel: 0,
                key: 60,
                velocity: 96,
                release_velocity: 12,
                pan: 64,
                start_tick: 0,
                end_tick: 96,
                fine_pitch_offset_cents: 0,
            },
            MidiExportNote {
                channel: 0,
                key: 60,
                velocity: 80,
                release_velocity: 24,
                pan: 64,
                start_tick: 96,
                end_tick: 144,
                fine_pitch_offset_cents: 0,
            },
        ]);

        let messages = midi_tick_messages(&midi, 0).unwrap();
        let at_tick = messages
            .iter()
            .filter(|message| message.tick == 96)
            .collect::<Vec<_>>();
        assert_eq!(at_tick[0].bytes, [0x80, 60, 12]);
        assert_eq!(at_tick[1].bytes, [0x90, 60, 80]);
    }

    #[test]
    fn playlist_midi_messages_restore_pre_seek_pitch_and_controller_state() {
        let midi = midi_file_with_notes(vec![MidiExportNote {
            channel: 1,
            key: 67,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            start_tick: 48,
            end_tick: 144,
            fine_pitch_offset_cents: 700,
        }]);

        let messages = midi_tick_messages(&midi, 96).unwrap();
        let at_seek = messages
            .iter()
            .filter(|message| message.tick == 96)
            .collect::<Vec<_>>();
        assert!(at_seek.len() >= 2);
        assert_eq!(at_seek.last().unwrap().bytes, [0x91, 67, 100]);
        assert!(
            at_seek
                .iter()
                .any(|message| message.bytes[0] & 0xF0 == 0xE0)
        );
        assert!(
            at_seek
                .iter()
                .any(|message| message.bytes[0] & 0xF0 == 0xB0)
        );
        assert_eq!(
            at_seek
                .iter()
                .filter(|message| message.bytes[0] & 0xF0 == 0xB0)
                .map(|message| message.bytes[1])
                .collect::<Vec<_>>(),
            [101, 100, 6, 38, 101, 100]
        );
        assert_eq!(messages.last().unwrap().tick, 144);
    }

    #[test]
    fn midi_clock_position_uses_twenty_four_pulses_per_quarter_note() {
        assert_eq!(midi_clock_pulse_count_at_tick(0.0, 96), 0);
        assert_eq!(midi_clock_pulse_count_at_tick(4.0, 96), 1);
        assert_eq!(midi_clock_pulse_count_at_tick(96.0, 96), 24);
        assert_eq!(midi_clock_pulse_count_at_tick(f64::NAN, 96), 0);
        assert_eq!(midi_clock_pulse_count_at_tick(96.0, 0), 0);
    }

    #[test]
    fn midi_song_position_pointer_uses_seven_bit_bytes_and_clamps_to_fourteen_bits() {
        assert_eq!(midi_song_position_pointer_message(130), [0xF2, 2, 1]);
        assert_eq!(
            midi_song_position_pointer_message(u16::MAX),
            [0xF2, 127, 127]
        );
    }

    #[test]
    fn midi_song_position_from_ticks_uses_ppq_and_caps_to_the_protocol_range() {
        assert_eq!(midi_sixteenth_notes_from_tick(96.0, 96), 4);
        assert_eq!(midi_sixteenth_notes_from_tick(48.0, 96), 2);
        assert_eq!(midi_sixteenth_notes_from_tick(0.0, 96), 0);
        assert_eq!(midi_sixteenth_notes_from_tick(f64::NAN, 96), 0);
        assert_eq!(midi_sixteenth_notes_from_tick(f64::MAX, 1), 16_383);
    }
}
