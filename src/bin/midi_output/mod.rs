use std::time::Duration;

use flp_rebuild::Pattern;
use midir::{MidiOutput, MidiOutputConnection as MidirOutputConnection};

const MAX_PATTERN_PREVIEW_NOTES: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledMidiMessage {
    pub after: Duration,
    pub bytes: [u8; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OrderedMidiMessage {
    tick: u64,
    priority: u8,
    order: usize,
    bytes: [u8; 3],
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
            bytes: [0x80 | channel, key, 0],
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

    use super::{all_notes_off_message, pattern_preview_messages};

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
                    bytes: [0x82, 64, 0],
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
                    ..PatternNote::default()
                },
                PatternNote {
                    position: 48,
                    length: 24,
                    key: 60,
                    velocity: 80,
                    ..PatternNote::default()
                },
            ],
            ..Pattern::default()
        };

        let messages = pattern_preview_messages(&pattern, 96, 120.0).unwrap();
        assert_eq!(messages[0].bytes, [0x90, 60, 96]);
        assert_eq!(messages[1].bytes, [0x80, 60, 0]);
        assert_eq!(messages[2].bytes, [0x90, 60, 80]);
        assert_eq!(messages[3].bytes, [0x80, 60, 0]);
    }

    #[test]
    fn pattern_preview_rejects_invalid_project_timing() {
        assert!(pattern_preview_messages(&Pattern::default(), 0, 120.0).is_err());
        assert!(pattern_preview_messages(&Pattern::default(), 96, f64::NAN).is_err());
    }
}
