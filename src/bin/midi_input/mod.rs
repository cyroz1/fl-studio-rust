use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use flp_rebuild::PatternNote;
use flp_rebuild::midi::{
    midi_modulation_to_fl_note_value, midi_pan_to_fl_note_pan,
    midi_pitch_bend_cents_to_fl_note_fine_pitch, midi_pitch_bend_offset_cents,
    midi_release_velocity_to_fl_note_release,
};
use midir::{Ignore, MidiInput, MidiInputConnection as MidirInputConnection};

use flp_rebuild::sample_render::PlaylistTempoMap;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiInputChannelState {
    pan: u8,
    mod_x: u8,
    mod_y: u8,
    pitch_bend_value: u16,
    pitch_bend_sensitivity_semitones: u8,
    pitch_bend_sensitivity_cents: u8,
    rpn_msb: u8,
    rpn_lsb: u8,
}

impl Default for MidiInputChannelState {
    fn default() -> Self {
        Self {
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            pitch_bend_value: 8192,
            pitch_bend_sensitivity_semitones: 2,
            pitch_bend_sensitivity_cents: 0,
            rpn_msb: 127,
            rpn_lsb: 127,
        }
    }
}

impl MidiInputChannelState {
    pub fn update(&mut self, event: MidiInputEvent) {
        match event {
            MidiInputEvent::ControlChange {
                controller: 10,
                value,
                ..
            } => self.pan = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 74,
                value,
                ..
            } => self.mod_x = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 71,
                value,
                ..
            } => self.mod_y = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 121, ..
            } => {
                self.pan = 64;
                self.mod_x = 64;
                self.mod_y = 64;
                self.pitch_bend_sensitivity_semitones = 2;
                self.pitch_bend_sensitivity_cents = 0;
                self.rpn_msb = 127;
                self.rpn_lsb = 127;
            }
            MidiInputEvent::ControlChange {
                controller: 101,
                value,
                ..
            } => self.rpn_msb = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 100,
                value,
                ..
            } => self.rpn_lsb = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 6,
                value,
                ..
            } if (self.rpn_msb, self.rpn_lsb) == (0, 0) => {
                self.pitch_bend_sensitivity_semitones = value.min(127);
            }
            MidiInputEvent::ControlChange {
                controller: 38,
                value,
                ..
            } if (self.rpn_msb, self.rpn_lsb) == (0, 0) => {
                self.pitch_bend_sensitivity_cents = value.min(99);
            }
            MidiInputEvent::PitchBend { value, .. } => {
                self.pitch_bend_value = (i32::from(value) + 8192).clamp(0, 0x3FFF) as u16;
            }
            _ => {}
        }
    }

    pub fn raw_pan(self) -> u8 {
        self.pan
    }

    pub fn raw_mod_x(self) -> u8 {
        self.mod_x
    }

    pub fn raw_mod_y(self) -> u8 {
        self.mod_y
    }

    pub fn raw_pitch_bend_value(self) -> u16 {
        self.pitch_bend_value
    }

    pub fn raw_pitch_bend_sensitivity_semitones(self) -> u8 {
        self.pitch_bend_sensitivity_semitones
    }

    pub fn raw_pitch_bend_sensitivity_cents(self) -> u8 {
        self.pitch_bend_sensitivity_cents
    }

    fn note_mod_x(self) -> u8 {
        midi_modulation_to_fl_note_value(self.mod_x)
    }

    fn note_mod_y(self) -> u8 {
        midi_modulation_to_fl_note_value(self.mod_y)
    }

    pub fn pitch_bend_offset_cents(self) -> i16 {
        midi_pitch_bend_offset_cents(
            self.pitch_bend_value,
            self.pitch_bend_sensitivity_semitones,
            self.pitch_bend_sensitivity_cents,
        )
    }

    fn note_fine_pitch(self) -> u8 {
        midi_pitch_bend_cents_to_fl_note_fine_pitch(self.pitch_bend_offset_cents())
    }
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

pub struct MidiPatternRecorderSettings {
    pub pattern_id: u16,
    pub channel_id: u16,
    pub project_path: Option<PathBuf>,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub start_tick: u32,
    pub snap_ticks: Option<u32>,
    pub timing_offset_ms: i16,
    pub song_timeline_start: Option<(PlaylistTempoMap, f64)>,
}

struct ActiveMidiNote {
    start_tick: u32,
    velocity: u8,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
    fine_pitch: u8,
}

pub struct MidiPatternRecorder {
    pattern_id: u16,
    channel_id: u16,
    project_path: Option<PathBuf>,
    ppq: u16,
    tempo_bpm: f64,
    start_tick: u32,
    snap_ticks: Option<u32>,
    timing_offset_ms: i16,
    song_timeline_start: Option<(PlaylistTempoMap, f64)>,
    song_start_frame_micros: Option<u64>,
    started_at: Instant,
    timestamp_origin_micros: Option<u64>,
    channel_state: [MidiInputChannelState; 16],
    active_notes: BTreeMap<(u8, u8), VecDeque<ActiveMidiNote>>,
    notes: Vec<PatternNote>,
    started_note_count: usize,
    last_position_tick: u32,
    overflowed: bool,
}

impl MidiPatternRecorder {
    pub fn new(
        settings: MidiPatternRecorderSettings,
        started_at: Instant,
    ) -> Result<Self, &'static str> {
        let MidiPatternRecorderSettings {
            pattern_id,
            channel_id,
            project_path,
            ppq,
            tempo_bpm,
            start_tick,
            snap_ticks,
            timing_offset_ms,
            song_timeline_start,
        } = settings;
        if ppq == 0 {
            return Err("MIDI recording requires a non-zero project PPQ value");
        }
        if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            return Err("MIDI recording requires a positive project tempo");
        }
        if snap_ticks == Some(0) {
            return Err("MIDI recording snap requires a non-zero tick value");
        }
        let song_start_frame_micros = match &song_timeline_start {
            Some((tempo_map, song_start_tick)) => {
                if !song_start_tick.is_finite() || *song_start_tick < 0.0 {
                    return Err("MIDI recording Playlist position must be finite and non-negative");
                }
                Some(
                    tempo_map
                        .frame_at_fractional_tick(*song_start_tick, 1_000_000)
                        .map_err(|_| "MIDI recording Playlist position is outside the timeline")?,
                )
            }
            None => None,
        };
        Ok(Self {
            pattern_id,
            channel_id,
            project_path,
            ppq,
            tempo_bpm,
            start_tick,
            snap_ticks,
            timing_offset_ms,
            song_timeline_start,
            song_start_frame_micros,
            started_at,
            timestamp_origin_micros: None,
            channel_state: [MidiInputChannelState::default(); 16],
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

    pub fn set_channel_state(&mut self, channel_state: [MidiInputChannelState; 16]) {
        self.channel_state = channel_state;
    }

    pub fn record(&mut self, message: ReceivedMidiMessage) {
        self.record_with_velocity_override(message, None);
    }

    pub fn record_with_velocity_override(
        &mut self,
        message: ReceivedMidiMessage,
        fixed_velocity: Option<u8>,
    ) {
        match message.event {
            MidiInputEvent::ControlChange { channel, .. }
            | MidiInputEvent::PitchBend { channel, .. } => {
                if let Some(state) = self.channel_state.get_mut(usize::from(channel)) {
                    state.update(message.event);
                }
            }
            _ => {}
        }
        if message.received_at < self.started_at {
            return;
        }
        let tick = self
            .tick_for_message(message.timestamp_micros, message.received_at)
            .saturating_add(self.start_tick)
            .min(MAX_NOTE_TICK);
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
                let channel_state = self
                    .channel_state
                    .get(usize::from(channel))
                    .copied()
                    .unwrap_or_default();
                self.active_notes
                    .entry((channel, key))
                    .or_default()
                    .push_back(ActiveMidiNote {
                        start_tick: tick,
                        velocity: fixed_velocity.unwrap_or(velocity).min(127),
                        pan: channel_state.raw_pan(),
                        mod_x: channel_state.note_mod_x(),
                        mod_y: channel_state.note_mod_y(),
                        fine_pitch: channel_state.note_fine_pitch(),
                    });
            }
            MidiInputEvent::NoteOff {
                channel,
                key,
                velocity: release_velocity,
            } => {
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
                if let Some(active_note) = ended_note {
                    let note =
                        self.make_note(active_note, tick, channel, key, Some(release_velocity));
                    self.notes.push(note);
                }
            }
            MidiInputEvent::ControlChange {
                channel,
                controller: 120 | 123..=127,
                ..
            } => self.finish_channel_notes(channel, tick),
            _ => {}
        }
    }

    fn finish_channel_notes(&mut self, channel: u8, end_tick: u32) {
        let active_keys = self
            .active_notes
            .keys()
            .copied()
            .filter(|(active_channel, _)| *active_channel == channel)
            .collect::<Vec<_>>();
        for (active_channel, key) in active_keys {
            if let Some(active) = self.active_notes.remove(&(active_channel, key)) {
                for active_note in active {
                    let note = self.make_note(active_note, end_tick, active_channel, key, None);
                    self.notes.push(note);
                }
            }
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
                let elapsed_micros = finished_at
                    .saturating_duration_since(self.started_at)
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64;
                self.tick_for_elapsed(Duration::from_micros(
                    self.corrected_elapsed_micros(elapsed_micros),
                ))
            })
            .saturating_add(self.start_tick)
            .min(MAX_NOTE_TICK);
        let finish_tick = finish_tick.max(self.last_position_tick);
        for ((channel, key), active) in std::mem::take(&mut self.active_notes) {
            for active_note in active {
                let note = self.make_note(active_note, finish_tick, channel, key, None);
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
        let elapsed_micros = self.corrected_elapsed_micros(timestamp_micros.saturating_sub(origin));
        self.tick_for_elapsed(Duration::from_micros(elapsed_micros))
    }

    fn corrected_elapsed_micros(&self, elapsed_micros: u64) -> u64 {
        let offset_micros = i64::from(self.timing_offset_ms) * 1_000;
        if offset_micros >= 0 {
            elapsed_micros.saturating_sub(offset_micros as u64)
        } else {
            elapsed_micros.saturating_add(offset_micros.unsigned_abs())
        }
    }

    fn tick_for_elapsed(&self, elapsed: Duration) -> u32 {
        if let (Some((tempo_map, song_start_tick)), Some(song_start_frame_micros)) =
            (&self.song_timeline_start, self.song_start_frame_micros)
        {
            let elapsed_micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
            let frame_micros = song_start_frame_micros.saturating_add(elapsed_micros);
            if let Some(song_tick) = tempo_map.tick_at_frame(frame_micros, 1_000_000) {
                let elapsed_ticks = (song_tick - song_start_tick).max(0.0);
                if !elapsed_ticks.is_finite() || elapsed_ticks >= f64::from(MAX_NOTE_TICK) {
                    return MAX_NOTE_TICK;
                }
                return elapsed_ticks.round() as u32;
            }
            return 0;
        }
        let ticks = elapsed.as_secs_f64() * self.tempo_bpm * f64::from(self.ppq) / 60.0;
        if !ticks.is_finite() || ticks >= f64::from(MAX_NOTE_TICK) {
            MAX_NOTE_TICK
        } else {
            ticks.round().max(0.0) as u32
        }
    }

    fn make_note(
        &self,
        active_note: ActiveMidiNote,
        end_tick: u32,
        midi_channel: u8,
        key: u8,
        release_velocity: Option<u8>,
    ) -> PatternNote {
        let position = quantize_recording_tick(active_note.start_tick, self.snap_ticks);
        let end_tick = quantize_recording_tick(end_tick, self.snap_ticks);
        let mut note = PatternNote {
            position,
            channel_id: self.channel_id,
            length: end_tick.saturating_sub(position).max(1),
            key: u16::from(key),
            midi_channel,
            pan: midi_pan_to_fl_note_pan(active_note.pan),
            mod_x: active_note.mod_x,
            mod_y: active_note.mod_y,
            velocity: active_note.velocity,
            fine_pitch: active_note.fine_pitch,
            ..PatternNote::default()
        };
        if let Some(release_velocity) = release_velocity {
            note.release = midi_release_velocity_to_fl_note_release(release_velocity);
        }
        note
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
            MidiPatternRecorderSettings {
                pattern_id: 7,
                channel_id: 42,
                project_path: Some(PathBuf::from("song.flp")),
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
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
                velocity: 64,
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
        assert_eq!(result.notes[0].pan, 64);
        assert_eq!(result.notes[0].release, 65);
    }

    #[test]
    fn records_channel_expression_at_each_note_on() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        recorder.record(message(
            100,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 10,
                value: 0,
            },
        ));
        recorder.record(message(
            110,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 74,
                value: 0,
            },
        ));
        recorder.record(message(
            120,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 71,
                value: 127,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        ));
        recorder.record(message(
            250,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 74,
                value: 127,
            },
        ));
        recorder.record(message(
            275,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 71,
                value: 0,
            },
        ));
        recorder.record(message(
            300,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 10,
                value: 127,
            },
        ));
        recorder.record(message(
            400,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        ));
        recorder.record(message(
            500,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 62,
                velocity: 90,
            },
        ));
        recorder.record(message(
            600,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 62,
                velocity: 0,
            },
        ));

        let result = recorder.finish(None, started_at + Duration::from_millis(700));
        assert_eq!(result.notes.len(), 2);
        assert_eq!(result.notes[0].pan, 0);
        assert_eq!(result.notes[0].mod_x, 0);
        assert_eq!(result.notes[0].mod_y, 255);
        assert_eq!(result.notes[1].pan, 128);
        assert_eq!(result.notes[1].mod_x, 255);
        assert_eq!(result.notes[1].mod_y, 0);
    }

    #[test]
    fn records_pitch_bend_at_each_note_on_using_rpn_sensitivity() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        for (elapsed_ms, controller, value) in
            [(100, 101, 0), (110, 100, 0), (120, 6, 4), (130, 38, 0)]
        {
            recorder.record(message(
                elapsed_ms,
                MidiInputEvent::ControlChange {
                    channel: 0,
                    controller,
                    value,
                },
            ));
        }
        recorder.record(message(
            150,
            MidiInputEvent::PitchBend {
                channel: 0,
                value: 8191,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        ));
        recorder.record(message(
            300,
            MidiInputEvent::PitchBend {
                channel: 0,
                value: -8192,
            },
        ));
        recorder.record(message(
            400,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        ));
        recorder.record(message(
            500,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 62,
                velocity: 90,
            },
        ));
        recorder.record(message(
            600,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 62,
                velocity: 0,
            },
        ));

        let result = recorder.finish(None, started_at + Duration::from_millis(700));
        assert_eq!(result.notes.len(), 2);
        assert_eq!(result.notes[0].fine_pitch, 160);
        assert_eq!(result.notes[1].fine_pitch, 80);
    }

    #[test]
    fn pitch_bend_uses_two_semitone_default_sensitivity() {
        let mut state = MidiInputChannelState::default();
        state.update(MidiInputEvent::PitchBend {
            channel: 0,
            value: 8191,
        });
        assert_eq!(state.note_fine_pitch(), 140);

        state.update(MidiInputEvent::PitchBend {
            channel: 0,
            value: -8192,
        });
        assert_eq!(state.note_fine_pitch(), 100);
    }

    #[test]
    fn midi_release_velocity_maps_to_fl_note_release_range() {
        assert_eq!(midi_release_velocity_to_fl_note_release(0), 0);
        assert_eq!(midi_release_velocity_to_fl_note_release(64), 65);
        assert_eq!(midi_release_velocity_to_fl_note_release(127), 128);
        assert_eq!(midi_release_velocity_to_fl_note_release(255), 128);
    }

    #[test]
    fn fixed_velocity_override_replaces_incoming_note_on_velocity() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        recorder.record_with_velocity_override(
            ReceivedMidiMessage {
                timestamp_micros: 1_000_000,
                received_at: started_at + Duration::from_millis(200),
                event: MidiInputEvent::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 17,
                },
            },
            Some(100),
        );
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_300_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(Some(1_300_000), started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].velocity, 100);
    }

    #[test]
    fn positive_timing_offset_moves_recorded_notes_earlier() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 100,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid timing correction should create a recorder");

        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_250_000,
            received_at: started_at + Duration::from_millis(250),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_500_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 29);
        assert_eq!(result.notes[0].length, 48);
    }

    #[test]
    fn closes_held_notes_when_recording_stops() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
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
        assert_eq!(result.notes[0].release, 64);
        assert_eq!(result.notes[0].length, 96);
    }

    #[test]
    fn quantizes_recorded_note_start_and_end_to_the_nearest_grid() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: Some(24),
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
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
            MidiPatternRecorder::new(
                MidiPatternRecorderSettings {
                    pattern_id: 1,
                    channel_id: 2,
                    project_path: None,
                    ppq: 96,
                    tempo_bpm: 120.0,
                    start_tick: 0,
                    snap_ticks: Some(0),
                    timing_offset_ms: 0,
                    song_timeline_start: None,
                },
                Instant::now(),
            ),
            Err("MIDI recording snap requires a non-zero tick value")
        ));
    }

    #[test]
    fn offsets_recorded_notes_from_the_selected_pattern_playhead_position() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 192,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing and playhead should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_200_000,
            received_at: started_at + Duration::from_millis(200),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_500_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 230);
        assert_eq!(result.notes[0].length, 58);
    }

    #[test]
    fn records_against_playlist_timeline_while_preserving_pattern_offset() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 24,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: Some((
                    PlaylistTempoMap::constant(96, 120.0).expect("valid constant tempo"),
                    96.0,
                )),
            },
            started_at,
        )
        .expect("valid Playlist timing should create a recorder");

        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_250_000,
            received_at: started_at + Duration::from_millis(250),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_500_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 72);
        assert_eq!(result.notes[0].length, 48);
    }
}
