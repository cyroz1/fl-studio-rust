use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flp_rebuild::audio::ProjectPlaybackClock;
use flp_rebuild::midi::{
    MidiChannelMapping, MidiEventKind, MidiFile, fl_note_release_to_midi_velocity,
};
use flp_rebuild::sample_render::PlaylistTempoMap;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledMidiFrameMessage {
    pub frame: u64,
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

#[derive(Clone)]
pub struct ConnectedMidiOutput {
    connection: Arc<SharedMidiOutputConnection>,
    pub device_id: String,
    pub device_name: String,
}

struct SharedMidiOutputConnection {
    connection: Mutex<MidirOutputConnection>,
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

pub fn pattern_loop_frame_messages(
    pattern: &Pattern,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: u32,
    loop_length_ticks: u64,
) -> Result<(Vec<ScheduledMidiFrameMessage>, u64), String> {
    if loop_length_ticks == 0 {
        return Err("Pattern MIDI loop length must be greater than zero".to_owned());
    }
    if pattern.notes.len() > MAX_PATTERN_PREVIEW_NOTES {
        return Err(format!(
            "Pattern {} exceeds the {}-note MIDI output limit",
            pattern.id, MAX_PATTERN_PREVIEW_NOTES
        ));
    }
    let tempo_map = PlaylistTempoMap::constant(ppq, tempo_bpm)?;
    let loop_frames = tempo_map.frame_at_tick(loop_length_ticks, sample_rate)?;
    if loop_frames == 0 {
        return Err("Pattern MIDI loop length rounds to zero audio frames".to_owned());
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
        let start_frame = tempo_map.frame_at_tick(start_tick, sample_rate)?;
        let end_frame = tempo_map
            .frame_at_tick(end_tick, sample_rate)?
            .max(start_frame.saturating_add(1));
        ordered.push(OrderedMidiMessage {
            tick: start_frame % loop_frames,
            priority: 1,
            order: note_index.saturating_mul(2),
            bytes: [0x90 | channel, key, note.velocity],
        });
        ordered.push(OrderedMidiMessage {
            tick: end_frame % loop_frames,
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
    let messages = ordered
        .into_iter()
        .map(|event| ScheduledMidiFrameMessage {
            frame: event.tick,
            bytes: event.bytes.to_vec(),
        })
        .collect();
    Ok((messages, loop_frames))
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
        connection: Arc::new(SharedMidiOutputConnection {
            connection: Mutex::new(connection),
        }),
        device_id: device_id.to_owned(),
        device_name,
    })
}

impl ConnectedMidiOutput {
    pub fn send(&mut self, message: &[u8]) -> Result<(), String> {
        self.connection
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(message)
            .map_err(|error| format!("could not send MIDI output: {error}"))
    }

    pub fn send_all_notes_off(&mut self) -> Result<(), String> {
        let mut connection = self
            .connection
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut first_error = None;
        for channel in 0..16 {
            if let Err(error) = connection
                .send(&all_notes_off_message(channel))
                .map_err(|error| format!("could not send MIDI output: {error}"))
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

impl Drop for SharedMidiOutputConnection {
    fn drop(&mut self) {
        let connection = self
            .connection
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for channel in 0..16 {
            let _ = connection.send(&all_notes_off_message(channel));
        }
    }
}

pub struct MidiPlaylistScheduler {
    stop_sender: Option<Sender<()>>,
    error_receiver: Receiver<String>,
    completion_receiver: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

pub struct MidiPatternLoopScheduler {
    stop_sender: Option<Sender<()>>,
    error_receiver: Receiver<String>,
    completion_receiver: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl MidiPatternLoopScheduler {
    pub fn start(
        output: ConnectedMidiOutput,
        messages: Vec<ScheduledMidiFrameMessage>,
        loop_frames: u64,
        playback_clock: ProjectPlaybackClock,
    ) -> Result<Self, String> {
        if loop_frames == 0 {
            return Err("Pattern MIDI loop must contain at least one audio frame".to_owned());
        }
        if messages.iter().any(|message| message.frame >= loop_frames)
            || messages
                .windows(2)
                .any(|pair| pair[0].frame > pair[1].frame)
        {
            return Err("Pattern MIDI messages must be sorted within the audio loop".to_owned());
        }

        let (stop_sender, stop_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("Pattern MIDI scheduler".to_owned())
            .spawn(move || {
                schedule_midi_pattern_loop(
                    output,
                    messages,
                    loop_frames,
                    playback_clock,
                    stop_receiver,
                    error_sender,
                    completion_sender,
                )
            })
            .map_err(|error| format!("could not start Pattern MIDI scheduler: {error}"))?;

        Ok(Self {
            stop_sender: Some(stop_sender),
            error_receiver,
            completion_receiver,
            worker: Some(worker),
        })
    }

    pub fn take_error(&self) -> Option<String> {
        self.error_receiver.try_recv().ok()
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            self.completion_receiver.try_recv(),
            Ok(()) | Err(mpsc::TryRecvError::Disconnected)
        )
    }

    pub fn stop(mut self) -> Result<(), String> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), String> {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
        let worker_panicked = self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        if let Ok(error) = self.error_receiver.try_recv() {
            return Err(error);
        }
        if worker_panicked {
            return Err("Pattern MIDI scheduler stopped unexpectedly".to_owned());
        }
        Ok(())
    }
}

impl Drop for MidiPatternLoopScheduler {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl MidiPlaylistScheduler {
    pub fn start(
        output: ConnectedMidiOutput,
        messages: Vec<ScheduledMidiFrameMessage>,
        playback_clock: ProjectPlaybackClock,
    ) -> Result<Self, String> {
        if messages
            .windows(2)
            .any(|pair| pair[0].frame > pair[1].frame)
        {
            return Err("Playlist MIDI messages must be sorted by audio frame".to_owned());
        }

        let (stop_sender, stop_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("Playlist MIDI scheduler".to_owned())
            .spawn(move || {
                schedule_midi_playlist(
                    output,
                    messages,
                    playback_clock,
                    stop_receiver,
                    error_sender,
                    completion_sender,
                )
            })
            .map_err(|error| format!("could not start Playlist MIDI scheduler: {error}"))?;

        Ok(Self {
            stop_sender: Some(stop_sender),
            error_receiver,
            completion_receiver,
            worker: Some(worker),
        })
    }

    pub fn take_error(&self) -> Option<String> {
        self.error_receiver.try_recv().ok()
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            self.completion_receiver.try_recv(),
            Ok(()) | Err(mpsc::TryRecvError::Disconnected)
        )
    }

    pub fn stop(mut self) -> Result<(), String> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), String> {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
        let worker_panicked = self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        if let Ok(error) = self.error_receiver.try_recv() {
            return Err(error);
        }
        if worker_panicked {
            return Err("Playlist MIDI scheduler stopped unexpectedly".to_owned());
        }
        Ok(())
    }
}

impl Drop for MidiPlaylistScheduler {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn schedule_midi_playlist(
    mut output: ConnectedMidiOutput,
    messages: Vec<ScheduledMidiFrameMessage>,
    playback_clock: ProjectPlaybackClock,
    stop_receiver: Receiver<()>,
    error_sender: Sender<String>,
    completion_sender: Sender<()>,
) {
    let result = 'scheduler: {
        let mut next_message = 0;
        loop {
            if midi_playlist_stop_requested(&stop_receiver) || !playback_clock.is_active() {
                break Ok(());
            }

            let current_frame = playback_clock.position_frames();
            let due_end = due_midi_playlist_message_end(&messages, next_message, current_frame);
            while next_message < due_end {
                if midi_playlist_stop_requested(&stop_receiver) {
                    break 'scheduler Ok(());
                }
                if let Err(error) = output.send(&messages[next_message].bytes) {
                    break 'scheduler Err(error);
                }
                next_message += 1;
            }
            if next_message == messages.len() {
                break Ok(());
            }

            match stop_receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    };

    let mut errors = Vec::new();
    if let Err(error) = result {
        errors.push(error);
    }
    if let Err(error) = output.send_all_notes_off() {
        errors.push(error);
    }
    if !errors.is_empty() {
        let _ = error_sender.send(errors.join("; "));
    }
    let _ = completion_sender.send(());
}

fn midi_playlist_stop_requested(stop_receiver: &Receiver<()>) -> bool {
    matches!(
        stop_receiver.try_recv(),
        Ok(()) | Err(mpsc::TryRecvError::Disconnected)
    )
}

fn due_midi_playlist_message_end(
    messages: &[ScheduledMidiFrameMessage],
    start: usize,
    current_frame: u64,
) -> usize {
    start + messages[start..].partition_point(|message| message.frame <= current_frame)
}

fn schedule_midi_pattern_loop(
    mut output: ConnectedMidiOutput,
    messages: Vec<ScheduledMidiFrameMessage>,
    loop_frames: u64,
    playback_clock: ProjectPlaybackClock,
    stop_receiver: Receiver<()>,
    error_sender: Sender<String>,
    completion_sender: Sender<()>,
) {
    let result = 'scheduler: {
        let mut next_message = 0;
        let mut was_active = false;
        let mut last_cycle = None;
        let mut last_frame = 0;
        loop {
            if midi_playlist_stop_requested(&stop_receiver) {
                break Ok(());
            }
            if !playback_clock.is_active() {
                if was_active && let Err(error) = output.send_all_notes_off() {
                    break Err(error);
                }
                was_active = false;
                last_cycle = None;
                next_message = 0;
                match stop_receiver.recv_timeout(Duration::from_millis(1)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                }
            }

            let current_frame = playback_clock.position_frames();
            let cycle = current_frame / loop_frames;
            let phase_frame = current_frame % loop_frames;
            if !was_active || last_cycle != Some(cycle) || current_frame < last_frame {
                if let Err(error) = output.send_all_notes_off() {
                    break Err(error);
                }
                for note_on in midi_pattern_active_note_ons(&messages, phase_frame) {
                    if let Err(error) = output.send(&note_on) {
                        break 'scheduler Err(error);
                    }
                }
                next_message = messages.partition_point(|message| message.frame < phase_frame);
                was_active = true;
                last_cycle = Some(cycle);
            }

            let due_end = due_midi_playlist_message_end(&messages, next_message, phase_frame);
            while next_message < due_end {
                if midi_playlist_stop_requested(&stop_receiver) {
                    break 'scheduler Ok(());
                }
                if let Err(error) = output.send(&messages[next_message].bytes) {
                    break 'scheduler Err(error);
                }
                next_message += 1;
            }
            last_frame = current_frame;

            match stop_receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    };

    let mut errors = Vec::new();
    if let Err(error) = result {
        errors.push(error);
    }
    if let Err(error) = output.send_all_notes_off() {
        errors.push(error);
    }
    if !errors.is_empty() {
        let _ = error_sender.send(errors.join("; "));
    }
    let _ = completion_sender.send(());
}

fn midi_pattern_active_note_ons(
    messages: &[ScheduledMidiFrameMessage],
    phase_frame: u64,
) -> Vec<[u8; 3]> {
    let mut active = Vec::<[u8; 3]>::new();
    let before_phase = messages.partition_point(|message| message.frame < phase_frame);
    for message in &messages[..before_phase] {
        apply_midi_pattern_note_state(&mut active, &message.bytes);
    }
    let at_phase_end = messages.partition_point(|message| message.frame <= phase_frame);
    for message in &messages[before_phase..at_phase_end] {
        if is_midi_note_off(&message.bytes) {
            apply_midi_pattern_note_state(&mut active, &message.bytes);
        }
    }
    active
}

fn apply_midi_pattern_note_state(active: &mut Vec<[u8; 3]>, bytes: &[u8]) {
    if bytes.len() < 3 {
        return;
    }
    let status = bytes[0] & 0xF0;
    if status == 0x90 && bytes[2] > 0 {
        active.push([bytes[0], bytes[1], bytes[2]]);
    } else if is_midi_note_off(bytes)
        && let Some(index) = active
            .iter()
            .position(|note| note[0] & 0x0F == bytes[0] & 0x0F && note[1] == bytes[1])
    {
        active.remove(index);
    }
}

fn is_midi_note_off(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && (bytes[0] & 0xF0 == 0x80 || (bytes[0] & 0xF0 == 0x90 && bytes[2] == 0))
}

pub struct MidiClockScheduler {
    stop_sender: Option<Sender<()>>,
    error_receiver: Receiver<String>,
    worker: Option<JoinHandle<()>>,
}

struct MidiClockSchedule {
    tempo_map: PlaylistTempoMap,
    sample_rate: u32,
    ppq: u16,
    next_pulse_index: u64,
    playback_clock: ProjectPlaybackClock,
    stop_receiver: Receiver<()>,
    error_sender: Sender<String>,
}

impl MidiClockScheduler {
    pub fn start(
        mut output: ConnectedMidiOutput,
        resume: bool,
        current_tick: f64,
        sample_rate: u32,
        ppq: u16,
        tempo_map: PlaylistTempoMap,
        playback_clock: ProjectPlaybackClock,
    ) -> Result<Self, String> {
        if ppq == 0 || sample_rate == 0 || !current_tick.is_finite() || current_tick < 0.0 {
            return Err(
                "MIDI clock requires a valid Playlist position and audio timing".to_owned(),
            );
        }
        let next_pulse_index = midi_clock_pulse_count_at_tick(current_tick, ppq).saturating_add(1);
        let result = if resume || current_tick > 0.0 {
            output
                .send(&midi_song_position_pointer_message(
                    midi_sixteenth_notes_from_tick(current_tick, ppq),
                ))
                .and_then(|()| output.send(&MIDI_CONTINUE))
        } else {
            output.send(&MIDI_START)
        };
        result?;

        let worker_output = output.clone();
        let (stop_sender, stop_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("MIDI clock scheduler".to_owned())
            .spawn(move || {
                schedule_midi_clock(
                    worker_output,
                    MidiClockSchedule {
                        tempo_map,
                        sample_rate,
                        ppq,
                        next_pulse_index,
                        playback_clock,
                        stop_receiver,
                        error_sender,
                    },
                )
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                let _ = output.send(&MIDI_STOP);
                return Err(format!("could not start MIDI clock scheduler: {error}"));
            }
        };

        Ok(Self {
            stop_sender: Some(stop_sender),
            error_receiver,
            worker: Some(worker),
        })
    }

    pub fn take_error(&self) -> Option<String> {
        self.error_receiver.try_recv().ok()
    }

    pub fn stop(mut self) -> Result<(), String> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), String> {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
        let worker_panicked = self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        if let Ok(error) = self.error_receiver.try_recv() {
            return Err(error);
        }
        if worker_panicked {
            return Err("MIDI clock scheduler stopped unexpectedly".to_owned());
        }
        Ok(())
    }
}

impl Drop for MidiClockScheduler {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn schedule_midi_clock(mut output: ConnectedMidiOutput, schedule: MidiClockSchedule) {
    let MidiClockSchedule {
        tempo_map,
        sample_rate,
        ppq,
        mut next_pulse_index,
        playback_clock,
        stop_receiver,
        error_sender,
    } = schedule;
    loop {
        if !playback_clock.is_active() {
            stop_clock_output(&mut output, &error_sender);
            return;
        }
        let current_frame = playback_clock.position_frames();
        let mut sent = 0;
        while sent < 4 {
            let pulse_frame =
                match midi_clock_pulse_frame(&tempo_map, next_pulse_index, sample_rate, ppq) {
                    Ok(frame) => frame,
                    Err(error) => {
                        stop_clock_output(&mut output, &error_sender);
                        let _ = error_sender.send(error);
                        return;
                    }
                };
            if pulse_frame > current_frame {
                break;
            }
            if let Err(error) = output.send(&MIDI_CLOCK_TICK) {
                stop_clock_output(&mut output, &error_sender);
                let _ = error_sender.send(error);
                return;
            }
            let Some(next_index) = next_pulse_index.checked_add(1) else {
                stop_clock_output(&mut output, &error_sender);
                let _ = error_sender.send("MIDI clock pulse index overflowed".to_owned());
                return;
            };
            next_pulse_index = next_index;
            sent += 1;
        }
        if sent == 4
            && let Some(current_tick) = tempo_map.tick_at_frame(current_frame, sample_rate)
        {
            next_pulse_index = next_pulse_index
                .max(midi_clock_pulse_count_at_tick(current_tick, ppq).saturating_add(1));
        }

        match stop_receiver.recv_timeout(Duration::from_millis(1)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop_clock_output(&mut output, &error_sender);
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn stop_clock_output(output: &mut ConnectedMidiOutput, error_sender: &Sender<String>) {
    if let Err(error) = output.send(&MIDI_STOP) {
        let _ = error_sender.send(error);
    }
}

fn midi_clock_pulse_frame(
    tempo_map: &PlaylistTempoMap,
    pulse_index: u64,
    sample_rate: u32,
    ppq: u16,
) -> Result<u64, String> {
    if ppq == 0 {
        return Err("MIDI clock requires a non-zero project PPQ".to_owned());
    }
    let tick = pulse_index as f64 * f64::from(ppq) / 24.0;
    tempo_map.frame_at_fractional_tick(tick, sample_rate)
}

fn all_notes_off_message(channel: u8) -> [u8; 3] {
    [0xB0 | (channel & 0x0F), 123, 0]
}

pub fn midi_program_change_message(channel: u8, program: u8) -> Result<[u8; 2], String> {
    if channel > 15 {
        return Err("MIDI program-change channel must be between 0 and 15".to_owned());
    }
    if program > 127 {
        return Err("MIDI program number must be between 0 and 127".to_owned());
    }
    Ok([0xC0 | channel, program])
}

pub fn midi_control_change_message(
    channel: u8,
    controller: u8,
    value: u8,
) -> Result<[u8; 3], String> {
    if channel > 15 {
        return Err("MIDI control-change channel must be between 0 and 15".to_owned());
    }
    if controller > 127 {
        return Err("MIDI controller number must be between 0 and 127".to_owned());
    }
    if value > 127 {
        return Err("MIDI control-change value must be between 0 and 127".to_owned());
    }
    Ok([0xB0 | channel, controller, value])
}

pub fn midi_patch_setup_messages(
    bank_msb: &[Option<u8>; 16],
    bank_lsb: &[Option<u8>; 16],
    programs: &[Option<u8>; 16],
) -> Result<Vec<Vec<u8>>, String> {
    let mut messages = Vec::new();
    for channel in 0..16 {
        if let Some(value) = bank_msb[channel] {
            messages.push(midi_control_change_message(channel as u8, 0, value)?.to_vec());
        }
        if let Some(value) = bank_lsb[channel] {
            messages.push(midi_control_change_message(channel as u8, 32, value)?.to_vec());
        }
        if let Some(program) = programs[channel] {
            messages.push(midi_program_change_message(channel as u8, program)?.to_vec());
        }
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use flp_rebuild::sample_render::PlaylistTempoMap;
    use flp_rebuild::{Pattern, PatternNote};

    use super::{
        ScheduledMidiFrameMessage, all_notes_off_message, due_midi_playlist_message_end,
        midi_clock_pulse_count_at_tick, midi_clock_pulse_frame, midi_control_change_message,
        midi_patch_setup_messages, midi_pattern_active_note_ons, midi_program_change_message,
        midi_sixteenth_notes_from_tick, midi_song_position_pointer_message, midi_tick_messages,
        pattern_loop_frame_messages, pattern_preview_messages,
    };
    use flp_rebuild::midi::{MidiChannelMapping, MidiExportNote, MidiExportTrack, MidiFile};

    #[test]
    fn all_notes_off_uses_the_requested_midi_channel() {
        assert_eq!(all_notes_off_message(0), [0xB0, 123, 0]);
        assert_eq!(all_notes_off_message(15), [0xBF, 123, 0]);
    }

    #[test]
    fn program_change_encodes_channel_and_zero_based_program() {
        assert_eq!(midi_program_change_message(0, 0).unwrap(), [0xC0, 0]);
        assert_eq!(midi_program_change_message(15, 127).unwrap(), [0xCF, 127]);
        assert!(midi_program_change_message(16, 0).is_err());
        assert!(midi_program_change_message(0, 128).is_err());
    }

    #[test]
    fn control_change_encodes_channel_and_seven_bit_values() {
        assert_eq!(midi_control_change_message(0, 0, 0).unwrap(), [0xB0, 0, 0]);
        assert_eq!(
            midi_control_change_message(15, 127, 127).unwrap(),
            [0xBF, 127, 127]
        );
        assert!(midi_control_change_message(16, 0, 0).is_err());
        assert!(midi_control_change_message(0, 128, 0).is_err());
        assert!(midi_control_change_message(0, 0, 128).is_err());
    }

    #[test]
    fn patch_setup_sends_bank_msb_then_lsb_then_program_per_channel() {
        let mut bank_msb = [None; 16];
        let mut bank_lsb = [None; 16];
        let mut programs = [None; 16];
        bank_msb[0] = Some(1);
        bank_lsb[0] = Some(2);
        programs[0] = Some(3);
        bank_lsb[15] = Some(127);
        programs[15] = Some(127);

        assert_eq!(
            midi_patch_setup_messages(&bank_msb, &bank_lsb, &programs).unwrap(),
            vec![
                vec![0xB0, 0, 1],
                vec![0xB0, 32, 2],
                vec![0xC0, 3],
                vec![0xBF, 32, 127],
                vec![0xCF, 127],
            ]
        );

        programs[15] = Some(128);
        assert!(midi_patch_setup_messages(&bank_msb, &bank_lsb, &programs).is_err());
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

    #[test]
    fn pattern_loop_messages_follow_audio_frames_and_wrap_boundary_note_offs() {
        let pattern = Pattern {
            notes: vec![
                PatternNote {
                    length: 96,
                    key: 60,
                    velocity: 96,
                    ..PatternNote::default()
                },
                PatternNote {
                    position: 24,
                    length: 24,
                    key: 64,
                    velocity: 80,
                    ..PatternNote::default()
                },
            ],
            ..Pattern::default()
        };

        let (messages, loop_frames) = pattern_loop_frame_messages(&pattern, 96, 120.0, 48_000, 96)
            .expect("Pattern messages should use the audio timing map");

        assert_eq!(loop_frames, 24_000);
        assert_eq!(
            messages,
            [
                ScheduledMidiFrameMessage {
                    frame: 0,
                    bytes: vec![0x80, 60, 64],
                },
                ScheduledMidiFrameMessage {
                    frame: 0,
                    bytes: vec![0x90, 60, 96],
                },
                ScheduledMidiFrameMessage {
                    frame: 6_000,
                    bytes: vec![0x90, 64, 80],
                },
                ScheduledMidiFrameMessage {
                    frame: 12_000,
                    bytes: vec![0x80, 64, 64],
                },
            ]
        );
    }

    #[test]
    fn pattern_loop_resume_restores_notes_held_at_the_audio_phase() {
        let messages = [
            ScheduledMidiFrameMessage {
                frame: 10,
                bytes: vec![0x91, 60, 90],
            },
            ScheduledMidiFrameMessage {
                frame: 20,
                bytes: vec![0x81, 60, 64],
            },
            ScheduledMidiFrameMessage {
                frame: 30,
                bytes: vec![0x91, 64, 80],
            },
            ScheduledMidiFrameMessage {
                frame: 40,
                bytes: vec![0x81, 64, 64],
            },
        ];

        assert_eq!(
            midi_pattern_active_note_ons(&messages, 15),
            [[0x91, 60, 90]]
        );
        assert!(midi_pattern_active_note_ons(&messages, 20).is_empty());
        assert!(midi_pattern_active_note_ons(&messages, 30).is_empty());
        assert_eq!(
            midi_pattern_active_note_ons(&messages, 35),
            [[0x91, 64, 80]]
        );
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
            mod_x: 64,
            mod_y: 64,
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
                    bytes: vec![0xB2, 10, 64],
                },
                super::ScheduledMidiTickMessage {
                    tick: 96,
                    bytes: vec![0xB2, 74, 64],
                },
                super::ScheduledMidiTickMessage {
                    tick: 96,
                    bytes: vec![0xB2, 71, 64],
                },
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
                mod_x: 64,
                mod_y: 64,
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
                mod_x: 64,
                mod_y: 64,
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
        assert_eq!(at_tick[1].bytes, [0xB0, 10, 64]);
        assert_eq!(at_tick[2].bytes, [0xB0, 74, 64]);
        assert_eq!(at_tick[3].bytes, [0xB0, 71, 64]);
        assert_eq!(at_tick[4].bytes, [0x90, 60, 80]);
    }

    #[test]
    fn playlist_midi_messages_restore_pre_seek_pitch_and_controller_state() {
        let midi = midi_file_with_notes(vec![MidiExportNote {
            channel: 1,
            key: 67,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
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
            [101, 100, 6, 38, 101, 100, 10, 74, 71]
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
    fn midi_clock_pulse_frames_follow_project_tempo_and_ppq() {
        let tempo_map = PlaylistTempoMap::constant(96, 120.0).unwrap();

        assert_eq!(
            midi_clock_pulse_frame(&tempo_map, 1, 48_000, 96).unwrap(),
            1_000
        );
        assert_eq!(
            midi_clock_pulse_frame(&tempo_map, 24, 48_000, 96).unwrap(),
            24_000
        );
    }

    #[test]
    fn pattern_mode_midi_clock_tempo_remains_continuous_across_loop_boundaries() {
        let tempo_map = PlaylistTempoMap::constant(96, 120.0).unwrap();

        assert_eq!(tempo_map.tick_at_frame(96_000, 48_000), Some(384.0));
        assert_eq!(
            midi_clock_pulse_frame(&tempo_map, 96, 48_000, 96).unwrap(),
            96_000
        );
        assert_eq!(
            midi_clock_pulse_frame(&tempo_map, 97, 48_000, 96).unwrap(),
            97_000
        );
    }

    #[test]
    fn playlist_midi_scheduler_groups_due_messages_by_audio_frame() {
        let messages = [
            ScheduledMidiFrameMessage {
                frame: 10,
                bytes: vec![0x90, 60, 100],
            },
            ScheduledMidiFrameMessage {
                frame: 20,
                bytes: vec![0x80, 60, 0],
            },
            ScheduledMidiFrameMessage {
                frame: 20,
                bytes: vec![0xB0, 10, 64],
            },
            ScheduledMidiFrameMessage {
                frame: 50,
                bytes: vec![0x90, 64, 100],
            },
        ];

        assert_eq!(due_midi_playlist_message_end(&messages, 0, 9), 0);
        assert_eq!(due_midi_playlist_message_end(&messages, 0, 10), 1);
        assert_eq!(due_midi_playlist_message_end(&messages, 1, 20), 3);
        assert_eq!(due_midi_playlist_message_end(&messages, 3, 49), 3);
        assert_eq!(due_midi_playlist_message_end(&messages, 3, 50), 4);
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
