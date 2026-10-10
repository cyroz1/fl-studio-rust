use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use flp_rebuild::PatternNote;
use midir::{Ignore, MidiInput, MidiInputConnection as MidirInputConnection};

pub const MIDI_INPUT_QUEUE_CAPACITY: usize = 4_096;
const MAX_CAPTURED_NOTES: usize = 100_000;
const MAX_NOTE_TICK: u32 = u32::MAX - 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiInputDevice {
    pub id: String,
    pub name: String,
}

pub struct ConnectedMidiInput {
    _connection: MidirInputConnection<()>,
    pub device_id: String,
    pub device_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiTransportCommand {
    Start,
    Continue,
    Stop,
    Pause,
    RecordStart,
    RecordStop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiInputEvent {
    NoteOn {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    PolyphonicPressure {
        channel: u8,
        key: u8,
        pressure: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    ChannelPressure {
        channel: u8,
        pressure: u8,
    },
    PitchBend {
        channel: u8,
        value: i16,
    },
    TimingClock,
    SongPosition {
        sixteenth_notes: u16,
    },
    Transport(MidiTransportCommand),
}

#[derive(Clone, Copy, Debug)]
pub struct ReceivedMidiMessage {
    pub timestamp_micros: u64,
    pub received_at: Instant,
    pub event: MidiInputEvent,
}

pub fn enumerate_input_devices() -> Result<Vec<MidiInputDevice>, String> {
    let input = MidiInput::new("FL Studio Rebuild MIDI Input")
        .map_err(|error| format!("could not initialize MIDI input: {error}"))?;
    input
        .ports()
        .into_iter()
        .map(|port| {
            let name = input
                .port_name(&port)
                .map_err(|error| format!("could not read MIDI input name: {error}"))?;
            Ok(MidiInputDevice {
                id: port.id(),
                name,
            })
        })
        .collect()
}

pub fn connect_input_device(
    device_id: &str,
    sender: SyncSender<ReceivedMidiMessage>,
    dropped_messages: Arc<AtomicU64>,
) -> Result<ConnectedMidiInput, String> {
    let mut input = MidiInput::new("FL Studio Rebuild MIDI Input")
        .map_err(|error| format!("could not initialize MIDI input: {error}"))?;
    input.ignore(Ignore::None);
    let port = input
        .ports()
        .into_iter()
        .find(|port| port.id() == device_id)
        .ok_or_else(|| "the selected MIDI input is no longer available".to_owned())?;
    let device_name = input
        .port_name(&port)
        .map_err(|error| format!("could not read MIDI input name: {error}"))?;
    let connection = input
        .connect(
            &port,
            "FL Studio Rebuild MIDI Input",
            move |timestamp_micros, bytes, _| {
                let Some(event) = parse_midi_message(bytes) else {
                    return;
                };
                let message = ReceivedMidiMessage {
                    timestamp_micros,
                    received_at: Instant::now(),
                    event,
                };
                if let Err(TrySendError::Full(_)) = sender.try_send(message) {
                    dropped_messages.fetch_add(1, Ordering::Relaxed);
                }
            },
            (),
        )
        .map_err(|error| format!("could not connect to MIDI input: {error}"))?;
    Ok(ConnectedMidiInput {
        _connection: connection,
        device_id: device_id.to_owned(),
        device_name,
    })
}

pub fn parse_midi_message(bytes: &[u8]) -> Option<MidiInputEvent> {
    let status = *bytes.first()?;
    match status {
        0xF0 => parse_mmc_message(bytes),
        0xF2 => Some(MidiInputEvent::SongPosition {
            sixteenth_notes: u16::from(*bytes.get(1)? & 0x7F)
                | (u16::from(*bytes.get(2)? & 0x7F) << 7),
        }),
        0xF8 => Some(MidiInputEvent::TimingClock),
        0xFA => Some(MidiInputEvent::Transport(MidiTransportCommand::Start)),
        0xFB => Some(MidiInputEvent::Transport(MidiTransportCommand::Continue)),
        0xFC => Some(MidiInputEvent::Transport(MidiTransportCommand::Stop)),
        0x80..=0xEF => {
            let channel = status & 0x0F;
            let kind = status & 0xF0;
            let first = *bytes.get(1)?;
            if first > 0x7F {
                return None;
            }
            let second = match kind {
                0xC0 | 0xD0 => None,
                _ => Some(*bytes.get(2)?),
            };
            if second.is_some_and(|value| value > 0x7F) {
                return None;
            }
            match kind {
                0x80 => Some(MidiInputEvent::NoteOff {
                    channel,
                    key: first,
                    velocity: second?,
                }),
                0x90 => {
                    let velocity = second?;
                    if velocity == 0 {
                        Some(MidiInputEvent::NoteOff {
                            channel,
                            key: first,
                            velocity,
                        })
                    } else {
                        Some(MidiInputEvent::NoteOn {
                            channel,
                            key: first,
                            velocity,
                        })
                    }
                }
                0xA0 => Some(MidiInputEvent::PolyphonicPressure {
                    channel,
                    key: first,
                    pressure: second?,
                }),
                0xB0 => Some(MidiInputEvent::ControlChange {
                    channel,
                    controller: first,
                    value: second?,
                }),
                0xC0 => Some(MidiInputEvent::ProgramChange {
                    channel,
                    program: first,
                }),
                0xD0 => Some(MidiInputEvent::ChannelPressure {
                    channel,
                    pressure: first,
                }),
                0xE0 => {
                    let value = u16::from(first) | (u16::from(second?) << 7);
                    Some(MidiInputEvent::PitchBend {
                        channel,
                        value: value as i16 - 8192,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_mmc_message(bytes: &[u8]) -> Option<MidiInputEvent> {
    if bytes.len() < 6 || bytes.last() != Some(&0xF7) || bytes[1] != 0x7F || bytes[3] != 0x06 {
        return None;
    }
    let command = match bytes[4] {
        0x01 => MidiTransportCommand::Stop,
        0x02 | 0x03 => MidiTransportCommand::Start,
        0x06 => MidiTransportCommand::RecordStart,
        0x07 | 0x08 => MidiTransportCommand::RecordStop,
        0x09 => MidiTransportCommand::Pause,
        _ => return None,
    };
    Some(MidiInputEvent::Transport(command))
}

pub fn describe_event(event: MidiInputEvent) -> String {
    match event {
        MidiInputEvent::NoteOn {
            channel,
            key,
            velocity,
        } => format!(
            "Note {} · channel {} · velocity {velocity}",
            midi_note_name(key),
            channel + 1
        ),
        MidiInputEvent::NoteOff {
            channel,
            key,
            velocity,
        } => format!(
            "Release {} · channel {} · velocity {velocity}",
            midi_note_name(key),
            channel + 1
        ),
        MidiInputEvent::PolyphonicPressure {
            channel,
            key,
            pressure,
        } => format!(
            "Pressure {} · channel {} · {pressure}",
            midi_note_name(key),
            channel + 1
        ),
        MidiInputEvent::ControlChange {
            channel,
            controller,
            value,
        } => format!("CC {controller} · channel {} · {value}", channel + 1),
        MidiInputEvent::ProgramChange { channel, program } => {
            format!("Program {program} · channel {}", channel + 1)
        }
        MidiInputEvent::ChannelPressure { channel, pressure } => {
            format!("Channel pressure · channel {} · {pressure}", channel + 1)
        }
        MidiInputEvent::PitchBend { channel, value } => {
            format!("Pitch bend · channel {} · {value}", channel + 1)
        }
        MidiInputEvent::TimingClock => "MIDI clock".to_owned(),
        MidiInputEvent::SongPosition { sixteenth_notes } => {
            format!("Song position · {sixteenth_notes} sixteenths")
        }
        MidiInputEvent::Transport(command) => match command {
            MidiTransportCommand::Start => "MIDI Start".to_owned(),
            MidiTransportCommand::Continue => "MIDI Continue".to_owned(),
            MidiTransportCommand::Stop => "MIDI Stop".to_owned(),
            MidiTransportCommand::Pause => "MMC Pause".to_owned(),
            MidiTransportCommand::RecordStart => "MMC Record".to_owned(),
            MidiTransportCommand::RecordStop => "MMC Record Exit".to_owned(),
        },
    }
}

fn midi_note_name(key: u8) -> String {
    const NOTE_NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    let octave = i16::from(key) / 12 - 1;
    format!("{}{}", NOTE_NAMES[usize::from(key % 12)], octave)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiRecordingResult {
    pub pattern_id: u16,
    pub channel_id: u16,
    pub project_path: Option<PathBuf>,
    pub notes: Vec<PatternNote>,
    pub overflowed: bool,
}

pub struct MidiPatternRecorder {
    pattern_id: u16,
    channel_id: u16,
    project_path: Option<PathBuf>,
    ppq: u16,
    tempo_bpm: f64,
    snap_ticks: Option<u32>,
    started_at: Instant,
    timestamp_origin_micros: Option<u64>,
    active_notes: BTreeMap<(u8, u8), VecDeque<(u32, u8)>>,
    notes: Vec<PatternNote>,
    started_note_count: usize,
    last_position_tick: u32,
    overflowed: bool,
}

impl MidiPatternRecorder {
    pub fn new(
        pattern_id: u16,
        channel_id: u16,
        project_path: Option<PathBuf>,
        ppq: u16,
        tempo_bpm: f64,
        snap_ticks: Option<u32>,
        started_at: Instant,
    ) -> Result<Self, &'static str> {
        if ppq == 0 {
            return Err("MIDI recording requires a non-zero project PPQ value");
        }
        if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            return Err("MIDI recording requires a positive project tempo");
        }
        if snap_ticks == Some(0) {
            return Err("MIDI recording snap requires a non-zero tick value");
        }
        Ok(Self {
            pattern_id,
            channel_id,
            project_path,
            ppq,
            tempo_bpm,
            snap_ticks,
            started_at,
            timestamp_origin_micros: None,
            active_notes: BTreeMap::new(),
            notes: Vec::new(),
            started_note_count: 0,
            last_position_tick: 0,
            overflowed: false,
        })
    }

    pub fn pattern_id(&self) -> u16 {
        self.pattern_id
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn record(&mut self, message: ReceivedMidiMessage) {
        if message.received_at < self.started_at {
            return;
        }
        let tick = self.tick_for_message(message.timestamp_micros, message.received_at);
        self.last_position_tick = self.last_position_tick.max(tick);
        match message.event {
            MidiInputEvent::NoteOn {
                channel,
                key,
                velocity,
            } => {
                if self.started_note_count >= MAX_CAPTURED_NOTES {
                    self.overflowed = true;
                    return;
                }
                self.started_note_count += 1;
                self.active_notes
                    .entry((channel, key))
                    .or_default()
                    .push_back((tick, velocity));
            }
            MidiInputEvent::NoteOff { channel, key, .. } => {
                let event_key = (channel, key);
                let (ended_note, remove_key) =
                    if let Some(active) = self.active_notes.get_mut(&event_key) {
                        let ended_note = active.pop_front();
                        (ended_note, active.is_empty())
                    } else {
                        (None, false)
                    };
                if remove_key {
                    self.active_notes.remove(&event_key);
                }
                if let Some((start_tick, velocity)) = ended_note {
                    let note = self.make_note(start_tick, tick, channel, key, velocity);
                    self.notes.push(note);
                }
            }
            _ => {}
        }
    }

    pub fn finish(
        mut self,
        timestamp_micros: Option<u64>,
        finished_at: Instant,
    ) -> MidiRecordingResult {
        let finish_tick = timestamp_micros
            .map(|timestamp| self.tick_for_message(timestamp, finished_at))
            .unwrap_or_else(|| {
                self.tick_for_elapsed(finished_at.saturating_duration_since(self.started_at))
            });
        let finish_tick = finish_tick.max(self.last_position_tick);
        for ((channel, key), active) in std::mem::take(&mut self.active_notes) {
            for (start_tick, velocity) in active {
                let note = self.make_note(start_tick, finish_tick, channel, key, velocity);
                self.notes.push(note);
            }
        }
        self.notes
            .sort_by_key(|note| (note.position, note.midi_channel, note.key, note.length));
        MidiRecordingResult {
            pattern_id: self.pattern_id,
            channel_id: self.channel_id,
            project_path: self.project_path,
            notes: self.notes,
            overflowed: self.overflowed,
        }
    }

    fn tick_for_message(&mut self, timestamp_micros: u64, received_at: Instant) -> u32 {
        if self.timestamp_origin_micros.is_none() {
            let before_first_message = received_at
                .saturating_duration_since(self.started_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            self.timestamp_origin_micros =
                Some(timestamp_micros.saturating_sub(before_first_message));
        }
        let origin = self
            .timestamp_origin_micros
            .expect("the timestamp origin is set above");
        let elapsed_micros = timestamp_micros.saturating_sub(origin);
        self.tick_for_elapsed(Duration::from_micros(elapsed_micros))
    }

    fn tick_for_elapsed(&self, elapsed: Duration) -> u32 {
        let ticks = elapsed.as_secs_f64() * self.tempo_bpm * f64::from(self.ppq) / 60.0;
        if !ticks.is_finite() || ticks >= f64::from(MAX_NOTE_TICK) {
            MAX_NOTE_TICK
        } else {
            ticks.round().max(0.0) as u32
        }
    }

    fn make_note(
        &self,
        start_tick: u32,
        end_tick: u32,
        midi_channel: u8,
        key: u8,
        velocity: u8,
    ) -> PatternNote {
        let position = quantize_recording_tick(start_tick, self.snap_ticks);
        let end_tick = quantize_recording_tick(end_tick, self.snap_ticks);
        PatternNote {
            position,
            channel_id: self.channel_id,
            length: end_tick.saturating_sub(position).max(1),
            key: u16::from(key),
            midi_channel,
            velocity,
            ..PatternNote::default()
        }
    }
}

fn quantize_recording_tick(tick: u32, snap_ticks: Option<u32>) -> u32 {
    let tick = u64::from(tick.min(MAX_NOTE_TICK));
    let Some(quantum) = snap_ticks.filter(|quantum| *quantum > 1) else {
        return tick as u32;
    };
    let quantum = u64::from(quantum);
    let rounded = tick.saturating_add(quantum / 2) / quantum * quantum;
    rounded.min(u64::from(MAX_NOTE_TICK)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_voice_messages_and_zero_velocity_note_off() {
        assert_eq!(
            parse_midi_message(&[0x93, 60, 101]),
            Some(MidiInputEvent::NoteOn {
                channel: 3,
                key: 60,
                velocity: 101,
            })
        );
        assert_eq!(
            parse_midi_message(&[0x93, 60, 0]),
            Some(MidiInputEvent::NoteOff {
                channel: 3,
                key: 60,
                velocity: 0,
            })
        );
        assert_eq!(
            parse_midi_message(&[0xB1, 74, 99]),
            Some(MidiInputEvent::ControlChange {
                channel: 1,
                controller: 74,
                value: 99,
            })
        );
        assert_eq!(
            parse_midi_message(&[0xE2, 0, 64]),
            Some(MidiInputEvent::PitchBend {
                channel: 2,
                value: 0,
            })
        );
    }

    #[test]
    fn parses_realtime_transport_and_mmc_commands() {
        assert_eq!(
            parse_midi_message(&[0xFA]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::Start))
        );
        assert_eq!(
            parse_midi_message(&[0xFB]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::Continue))
        );
        assert_eq!(
            parse_midi_message(&[0xFC]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::Stop))
        );
        assert_eq!(
            parse_midi_message(&[0xF0, 0x7F, 0x7F, 0x06, 0x06, 0xF7]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::RecordStart))
        );
        assert_eq!(
            parse_midi_message(&[0xF0, 0x7F, 0x01, 0x06, 0x07, 0xF7]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::RecordStop))
        );
        assert_eq!(parse_midi_message(&[0xF0, 0x7F, 0x00, 0x06, 0x02]), None);
    }

    #[test]
    fn records_note_timing_velocity_channel_and_target() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            7,
            42,
            Some(PathBuf::from("song.flp")),
            96,
            120.0,
            None,
            started_at,
        )
        .expect("valid project timing should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_000_000,
            received_at: started_at + Duration::from_millis(200),
            event: MidiInputEvent::NoteOn {
                channel: 9,
                key: 60,
                velocity: 101,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_300_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 9,
                key: 60,
                velocity: 0,
            },
        });
        let result = recorder.finish(Some(1_300_000), started_at + Duration::from_millis(500));
        assert_eq!(result.pattern_id, 7);
        assert_eq!(result.channel_id, 42);
        assert_eq!(result.project_path, Some(PathBuf::from("song.flp")));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 38);
        assert_eq!(result.notes[0].length, 58);
        assert_eq!(result.notes[0].key, 60);
        assert_eq!(result.notes[0].midi_channel, 9);
        assert_eq!(result.notes[0].velocity, 101);
    }

    #[test]
    fn closes_held_notes_when_recording_stops() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(1, 2, None, 96, 120.0, None, started_at)
            .expect("valid project timing should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 110_000,
            received_at: started_at + Duration::from_millis(100),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 48,
                velocity: 80,
            },
        });
        let result = recorder.finish(None, started_at + Duration::from_millis(600));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].length, 96);
    }

    #[test]
    fn quantizes_recorded_note_start_and_end_to_the_nearest_grid() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(1, 2, None, 96, 120.0, Some(24), started_at)
            .expect("valid project timing and snap should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_135_417,
            received_at: started_at + Duration::from_micros(135_417),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_364_583,
            received_at: started_at + Duration::from_micros(364_583),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 24);
        assert_eq!(result.notes[0].length, 48);
    }

    #[test]
    fn rejects_a_zero_length_recording_snap() {
        assert!(matches!(
            MidiPatternRecorder::new(1, 2, None, 96, 120.0, Some(0), Instant::now()),
            Err("MIDI recording snap requires a non-zero tick value")
        ));
    }
}
