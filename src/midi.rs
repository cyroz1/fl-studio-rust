use std::collections::{BTreeMap, VecDeque};
use std::fmt;

const MTHD: &[u8; 4] = b"MThd";
const MTRK: &[u8; 4] = b"MTrk";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiFile {
    format: u16,
    division: u16,
    tracks: Vec<MidiTrack>,
    original: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiTrack {
    events: Vec<MidiEvent>,
    end_tick: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiEvent {
    delta_ticks: u32,
    absolute_tick: u64,
    kind: MidiEventKind,
    wire_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiNote {
    channel: u8,
    key: u8,
    velocity: u8,
    start_tick: u64,
    end_tick: Option<u64>,
    release_velocity: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiTempoEvent {
    tick: u64,
    microseconds_per_quarter: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiEventKind {
    ChannelVoice { status: u8, data: Vec<u8> },
    Meta { meta_type: u8, data: Vec<u8> },
    SysEx { status: u8, data: Vec<u8> },
    System { status: u8, data: Vec<u8> },
}

impl MidiFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, MidiError> {
        if bytes.len() < 8 {
            return Err(MidiError::UnexpectedEof {
                offset: bytes.len(),
                context: "MThd chunk header",
            });
        }
        require_tag(bytes, 0, MTHD)?;
        let header_length = read_u32_be(bytes, 4, "MThd length")? as usize;
        if header_length < 6 {
            return Err(MidiError::InvalidHeaderLength(header_length as u32));
        }
        let header_end = 8usize
            .checked_add(header_length)
            .ok_or(MidiError::LengthOverflow)?;
        if header_end > bytes.len() {
            return Err(MidiError::UnexpectedEof {
                offset: bytes.len(),
                context: "MThd contents",
            });
        }

        let format = read_u16_be(bytes, 8, "MIDI format")?;
        let track_count = read_u16_be(bytes, 10, "MIDI track count")?;
        let division = read_u16_be(bytes, 12, "MIDI time division")?;
        let mut cursor = header_end;
        let mut tracks = Vec::with_capacity(usize::from(track_count));

        while tracks.len() < usize::from(track_count) {
            if cursor >= bytes.len() {
                return Err(MidiError::UnexpectedEof {
                    offset: cursor,
                    context: "MTrk chunk",
                });
            }
            if cursor.checked_add(8).ok_or(MidiError::LengthOverflow)? > bytes.len() {
                return Err(MidiError::UnexpectedEof {
                    offset: bytes.len(),
                    context: "MIDI chunk header",
                });
            }

            let tag = &bytes[cursor..cursor + 4];
            let chunk_length = read_u32_be(bytes, cursor + 4, "MIDI chunk length")? as usize;
            let chunk_start = cursor + 8;
            let chunk_end = chunk_start
                .checked_add(chunk_length)
                .ok_or(MidiError::LengthOverflow)?;
            if chunk_end > bytes.len() {
                return Err(MidiError::UnexpectedEof {
                    offset: bytes.len(),
                    context: "MIDI chunk contents",
                });
            }

            cursor = chunk_end;
            if tag == MTRK {
                tracks.push(parse_track(&bytes[chunk_start..chunk_end], chunk_start)?);
            }
        }

        Ok(Self {
            format,
            division,
            tracks,
            original: bytes.to_vec(),
        })
    }

    pub fn format(&self) -> u16 {
        self.format
    }

    pub fn division(&self) -> u16 {
        self.division
    }

    pub fn ticks_per_quarter_note(&self) -> Option<u16> {
        (self.division & 0x8000 == 0).then_some(self.division)
    }

    pub fn tracks(&self) -> &[MidiTrack] {
        &self.tracks
    }

    /// Returns the untouched Standard MIDI File bytes.
    pub fn encode_lossless(&self) -> Vec<u8> {
        self.original.clone()
    }
}

impl MidiTrack {
    pub fn events(&self) -> &[MidiEvent] {
        &self.events
    }

    pub fn end_tick(&self) -> u64 {
        self.end_tick
    }

    pub fn name(&self) -> Option<String> {
        self.events.iter().find_map(|event| match &event.kind {
            MidiEventKind::Meta {
                meta_type: 0x03,
                data,
            } => Some(String::from_utf8_lossy(data).into_owned()),
            _ => None,
        })
    }

    pub fn tempo_bpm(&self) -> Option<f64> {
        self.tempo_events()
            .first()
            .copied()
            .map(MidiTempoEvent::bpm)
    }

    /// Returns every tempo event in file order, including repeated values.
    pub fn tempo_events(&self) -> Vec<MidiTempoEvent> {
        self.events
            .iter()
            .filter_map(|event| match &event.kind {
                MidiEventKind::Meta {
                    meta_type: 0x51,
                    data,
                } if data.len() == 3 => Some(MidiTempoEvent {
                    tick: event.absolute_tick,
                    microseconds_per_quarter: (u32::from(data[0]) << 16)
                        | (u32::from(data[1]) << 8)
                        | u32::from(data[2]),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn note_event_counts(&self) -> (usize, usize) {
        let mut note_on = 0usize;
        let mut note_off = 0usize;
        for event in &self.events {
            if let MidiEventKind::ChannelVoice { status, data } = &event.kind {
                if data.len() < 2 {
                    continue;
                }
                match status & 0xF0 {
                    0x90 if data[1] != 0 => note_on += 1,
                    0x80 | 0x90 => note_off += 1,
                    _ => {}
                }
            }
        }
        (note_on, note_off)
    }

    /// Pairs note-on and note-off messages in channel/key order. Notes without an
    /// ending message remain present with `end_tick == None`.
    pub fn notes(&self) -> Vec<MidiNote> {
        let mut notes = Vec::new();
        let mut active: BTreeMap<(u8, u8), VecDeque<usize>> = BTreeMap::new();

        for event in &self.events {
            let MidiEventKind::ChannelVoice { status, data } = &event.kind else {
                continue;
            };
            if data.len() < 2 {
                continue;
            }

            let channel = status & 0x0F;
            let key = data[0];
            match status & 0xF0 {
                0x90 if data[1] != 0 => {
                    let note_index = notes.len();
                    notes.push(MidiNote {
                        channel,
                        key,
                        velocity: data[1],
                        start_tick: event.absolute_tick,
                        end_tick: None,
                        release_velocity: None,
                    });
                    active
                        .entry((channel, key))
                        .or_default()
                        .push_back(note_index);
                }
                0x80 | 0x90 => {
                    let note_key = (channel, key);
                    let mut remove_key = false;
                    if let Some(queue) = active.get_mut(&note_key) {
                        if let Some(note_index) = queue.pop_front() {
                            notes[note_index].end_tick = Some(event.absolute_tick);
                            notes[note_index].release_velocity = Some(data[1]);
                        }
                        remove_key = queue.is_empty();
                    }
                    if remove_key {
                        active.remove(&note_key);
                    }
                }
                _ => {}
            }
        }
        notes
    }
}

impl MidiNote {
    pub fn channel(&self) -> u8 {
        self.channel
    }

    pub fn key(&self) -> u8 {
        self.key
    }

    pub fn velocity(&self) -> u8 {
        self.velocity
    }

    pub fn start_tick(&self) -> u64 {
        self.start_tick
    }

    pub fn end_tick(&self) -> Option<u64> {
        self.end_tick
    }

    pub fn duration_ticks(&self) -> Option<u64> {
        self.end_tick.map(|end| end.saturating_sub(self.start_tick))
    }

    pub fn release_velocity(&self) -> Option<u8> {
        self.release_velocity
    }
}

impl MidiTempoEvent {
    pub fn tick(self) -> u64 {
        self.tick
    }

    pub fn microseconds_per_quarter(self) -> u32 {
        self.microseconds_per_quarter
    }

    pub fn bpm(self) -> f64 {
        if self.microseconds_per_quarter == 0 {
            0.0
        } else {
            60_000_000.0 / f64::from(self.microseconds_per_quarter)
        }
    }
}

impl MidiEvent {
    pub fn delta_ticks(&self) -> u32 {
        self.delta_ticks
    }

    pub fn absolute_tick(&self) -> u64 {
        self.absolute_tick
    }

    pub fn kind(&self) -> &MidiEventKind {
        &self.kind
    }

    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiError {
    UnexpectedEof {
        offset: usize,
        context: &'static str,
    },
    InvalidTag {
        offset: usize,
        expected: &'static str,
    },
    InvalidHeaderLength(u32),
    InvalidVlq {
        offset: usize,
    },
    InvalidEvent {
        offset: usize,
        detail: &'static str,
    },
    LengthOverflow,
}

impl fmt::Display for MidiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { offset, context } => {
                write!(
                    formatter,
                    "unexpected end of MIDI file at byte {offset} while reading {context}"
                )
            }
            Self::InvalidTag { offset, expected } => {
                write!(
                    formatter,
                    "invalid MIDI chunk at byte {offset}; expected {expected}"
                )
            }
            Self::InvalidHeaderLength(length) => {
                write!(
                    formatter,
                    "invalid MThd length {length}; expected at least 6"
                )
            }
            Self::InvalidVlq { offset } => {
                write!(
                    formatter,
                    "invalid or overlong MIDI variable-length quantity at byte {offset}"
                )
            }
            Self::InvalidEvent { offset, detail } => {
                write!(formatter, "invalid MIDI event at byte {offset}: {detail}")
            }
            Self::LengthOverflow => write!(formatter, "MIDI length exceeds supported size"),
        }
    }
}

impl std::error::Error for MidiError {}

fn parse_track(bytes: &[u8], absolute_start: usize) -> Result<MidiTrack, MidiError> {
    let mut cursor = 0usize;
    let mut absolute_tick = 0u64;
    let mut running_status = None;
    let mut events = Vec::new();

    while cursor < bytes.len() {
        let event_start = cursor;
        let (delta_ticks, after_delta) = read_vlq(bytes, cursor, absolute_start)?;
        cursor = after_delta;
        absolute_tick = absolute_tick
            .checked_add(u64::from(delta_ticks))
            .ok_or(MidiError::LengthOverflow)?;

        let Some(&first_byte) = bytes.get(cursor) else {
            return Err(MidiError::UnexpectedEof {
                offset: absolute_start + cursor,
                context: "MIDI event status",
            });
        };

        let (status, first_data) = if first_byte & 0x80 != 0 {
            cursor += 1;
            (first_byte, None)
        } else {
            let Some(status) = running_status else {
                return Err(MidiError::InvalidEvent {
                    offset: absolute_start + cursor,
                    detail: "data byte encountered without running status",
                });
            };
            (status, Some(first_byte))
        };

        let kind = match status {
            0x80..=0xEF => {
                running_status = Some(status);
                let data_length = channel_data_length(status);
                let data =
                    read_data_bytes(bytes, &mut cursor, data_length, first_data, absolute_start)?;
                MidiEventKind::ChannelVoice { status, data }
            }
            0xF0 | 0xF7 => {
                running_status = None;
                let (length, after_length) = read_vlq(bytes, cursor, absolute_start)?;
                cursor = after_length;
                let data =
                    read_payload(bytes, &mut cursor, length, absolute_start, "SysEx payload")?;
                MidiEventKind::SysEx { status, data }
            }
            0xFF => {
                let Some(&meta_type) = bytes.get(cursor) else {
                    return Err(MidiError::UnexpectedEof {
                        offset: absolute_start + cursor,
                        context: "MIDI meta type",
                    });
                };
                cursor += 1;
                let (length, after_length) = read_vlq(bytes, cursor, absolute_start)?;
                cursor = after_length;
                let data = read_payload(
                    bytes,
                    &mut cursor,
                    length,
                    absolute_start,
                    "MIDI meta payload",
                )?;
                MidiEventKind::Meta { meta_type, data }
            }
            0xF1 | 0xF2 | 0xF3 | 0xF6 | 0xF8..=0xFE => {
                if status < 0xF8 {
                    running_status = None;
                }
                let data_length = system_data_length(status);
                let data =
                    read_data_bytes(bytes, &mut cursor, data_length, first_data, absolute_start)?;
                MidiEventKind::System { status, data }
            }
            _ => {
                return Err(MidiError::InvalidEvent {
                    offset: absolute_start + event_start,
                    detail: "unsupported MIDI system status",
                });
            }
        };

        events.push(MidiEvent {
            delta_ticks,
            absolute_tick,
            kind,
            wire_bytes: bytes[event_start..cursor].to_vec(),
        });
    }

    Ok(MidiTrack {
        events,
        end_tick: absolute_tick,
    })
}

fn channel_data_length(status: u8) -> usize {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

fn system_data_length(status: u8) -> usize {
    match status {
        0xF1 | 0xF3 => 1,
        0xF2 => 2,
        _ => 0,
    }
}

fn read_data_bytes(
    bytes: &[u8],
    cursor: &mut usize,
    length: usize,
    first_data: Option<u8>,
    absolute_start: usize,
) -> Result<Vec<u8>, MidiError> {
    let mut data = Vec::with_capacity(length);
    if let Some(first) = first_data {
        if length == 0 {
            return Err(MidiError::InvalidEvent {
                offset: absolute_start + *cursor,
                detail: "running status supplied data for a zero-length event",
            });
        }
        data.push(first);
        *cursor += 1;
    }
    while data.len() < length {
        let Some(&byte) = bytes.get(*cursor) else {
            return Err(MidiError::UnexpectedEof {
                offset: absolute_start + *cursor,
                context: "MIDI event data",
            });
        };
        if byte & 0x80 != 0 {
            return Err(MidiError::InvalidEvent {
                offset: absolute_start + *cursor,
                detail: "status byte encountered where event data was expected",
            });
        }
        data.push(byte);
        *cursor += 1;
    }
    Ok(data)
}

fn read_payload(
    bytes: &[u8],
    cursor: &mut usize,
    length: u32,
    absolute_start: usize,
    context: &'static str,
) -> Result<Vec<u8>, MidiError> {
    let length = usize::try_from(length).map_err(|_| MidiError::LengthOverflow)?;
    let end = cursor
        .checked_add(length)
        .ok_or(MidiError::LengthOverflow)?;
    let Some(payload) = bytes.get(*cursor..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: absolute_start + bytes.len(),
            context,
        });
    };
    *cursor = end;
    Ok(payload.to_vec())
}

fn read_vlq(bytes: &[u8], start: usize, absolute_start: usize) -> Result<(u32, usize), MidiError> {
    let mut value = 0u32;
    let mut cursor = start;
    for _ in 0..4 {
        let Some(&byte) = bytes.get(cursor) else {
            return Err(MidiError::UnexpectedEof {
                offset: absolute_start + cursor,
                context: "MIDI variable-length quantity",
            });
        };
        cursor += 1;
        value = (value << 7) | u32::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
    }
    Err(MidiError::InvalidVlq {
        offset: absolute_start + start,
    })
}

fn require_tag(bytes: &[u8], offset: usize, expected: &'static [u8; 4]) -> Result<(), MidiError> {
    let end = offset.checked_add(4).ok_or(MidiError::LengthOverflow)?;
    let Some(tag) = bytes.get(offset..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: bytes.len(),
            context: "MIDI chunk tag",
        });
    };
    if tag != expected {
        return Err(MidiError::InvalidTag {
            offset,
            expected: if expected == MTHD { "MThd" } else { "MTrk" },
        });
    }
    Ok(())
}

fn read_u16_be(bytes: &[u8], offset: usize, context: &'static str) -> Result<u16, MidiError> {
    let end = offset.checked_add(2).ok_or(MidiError::LengthOverflow)?;
    let Some(value) = bytes.get(offset..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn read_u32_be(bytes: &[u8], offset: usize, context: &'static str) -> Result<u32, MidiError> {
    let end = offset.checked_add(4).ok_or(MidiError::LengthOverflow)?;
    let Some(value) = bytes.get(offset..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

#[cfg(test)]
mod tests {
    use super::{MidiError, MidiFile};

    fn midi_fixture(track: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MThd");
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&480u16.to_be_bytes());
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(track);
        bytes
    }

    #[test]
    fn pairs_notes_across_running_status_and_preserves_original_bytes() {
        let track = [
            0x00, 0x90, 0x3C, 0x64, // Note on at tick 0.
            0x81, 0x70, 0x3C, 0x00, // Running-status note off at tick 240.
            0x00, 0xFF, 0x2F, 0x00, // End of track.
        ];
        let input = midi_fixture(&track);

        let midi = MidiFile::parse(&input).expect("fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(midi.division(), 480);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key(), 60);
        assert_eq!(notes[0].velocity(), 100);
        assert_eq!(notes[0].start_tick(), 0);
        assert_eq!(notes[0].end_tick(), Some(240));
        assert_eq!(notes[0].duration_ticks(), Some(240));
        assert_eq!(midi.encode_lossless(), input);
    }

    #[test]
    fn rejects_overlong_delta_time_varints() {
        let input = midi_fixture(&[0x81, 0x80, 0x80, 0x80]);

        let error = MidiFile::parse(&input).expect_err("overlong delta must fail");

        assert!(matches!(error, MidiError::InvalidVlq { .. }));
    }
}
