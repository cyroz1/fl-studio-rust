use std::collections::{HashMap, HashSet};
use std::fmt;

pub mod audio;
pub mod media;
pub mod midi;
pub mod plugins;
pub mod sample_render;
pub mod vst3;

const FLHD: &[u8; 4] = b"FLhd";
const FLDT: &[u8; 4] = b"FLdt";
const MIN_HEADER_CONTENT_LENGTH: usize = 6;
const FLP_NOTE_RECORD_SIZE: usize = 24;
const FLP_PLAYLIST_RECORD_SIZES: [usize; 3] = [80, 60, 32];
const FLP_AUTOMATION_COUNT_OFFSET: usize = 17;
const FLP_AUTOMATION_POINTS_OFFSET: usize = 21;
const FLP_AUTOMATION_POINT_SIZE: usize = 24;
const TIME_MARKER_SIGNATURE_BIT: u32 = 0x0800_0000;
const TIME_MARKER_TICK_MASK: u32 = 0x07FF_FFFF;
const PROJECT_INFO_STRING_EVENTS: [u8; 5] = [0xC2, 0xCE, 0xCF, 0xC3, 0xC5];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlpHeader {
    format: u16,
    legacy_channel_count: u16,
    ppq: u16,
    extension: Vec<u8>,
}

impl FlpHeader {
    pub fn format(&self) -> u16 {
        self.format
    }

    pub fn legacy_channel_count(&self) -> u16 {
        self.legacy_channel_count
    }

    pub fn ppq(&self) -> u16 {
        self.ppq
    }

    pub fn extension(&self) -> &[u8] {
        &self.extension
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PayloadEncoding {
    Byte,
    Word,
    Dword,
    Data { length_prefix: Vec<u8> },
    FixedThreeBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlpEvent {
    opcode: u8,
    payload: Vec<u8>,
    encoding: PayloadEncoding,
    wire_bytes: Vec<u8>,
    file_offset: usize,
}

impl FlpEvent {
    pub fn opcode(&self) -> u8 {
        self.opcode
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn encoding(&self) -> &PayloadEncoding {
        &self.encoding
    }

    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire_bytes
    }

    pub fn file_offset(&self) -> usize {
        self.file_offset
    }

    fn replace_data_payload(&mut self, payload: Vec<u8>) -> Result<(), FlpError> {
        if !matches!(self.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a length-prefixed data payload",
            });
        }
        let payload_length = u32::try_from(payload.len()).map_err(|_| FlpError::LengthOverflow)?;
        let original_payload_length = self.payload.len();
        let length_prefix = match &self.encoding {
            PayloadEncoding::Data { length_prefix } if payload.len() == original_payload_length => {
                length_prefix.clone()
            }
            PayloadEncoding::Data { .. } => encode_leb128(payload_length),
            _ => unreachable!("data encoding was checked above"),
        };
        let mut wire_bytes = Vec::with_capacity(1 + length_prefix.len() + payload.len());
        wire_bytes.push(self.opcode);
        wire_bytes.extend_from_slice(&length_prefix);
        wire_bytes.extend_from_slice(&payload);
        self.payload = payload;
        self.encoding = PayloadEncoding::Data { length_prefix };
        self.wire_bytes = wire_bytes;
        Ok(())
    }

    fn new_data(opcode: u8, payload: Vec<u8>) -> Result<Self, FlpError> {
        let mut event = Self {
            opcode,
            payload: Vec::new(),
            encoding: PayloadEncoding::Data {
                length_prefix: encode_leb128(0),
            },
            wire_bytes: Vec::new(),
            file_offset: 0,
        };
        event.replace_data_payload(payload)?;
        Ok(event)
    }

    fn new_word(opcode: u8, value: u16) -> Self {
        let payload = value.to_le_bytes().to_vec();
        let mut wire_bytes = Vec::with_capacity(3);
        wire_bytes.push(opcode);
        wire_bytes.extend_from_slice(&payload);
        Self {
            opcode,
            payload,
            encoding: PayloadEncoding::Word,
            wire_bytes,
            file_offset: 0,
        }
    }

    fn new_dword(opcode: u8, value: u32) -> Self {
        let payload = value.to_le_bytes().to_vec();
        let mut wire_bytes = Vec::with_capacity(5);
        wire_bytes.push(opcode);
        wire_bytes.extend_from_slice(&payload);
        Self {
            opcode,
            payload,
            encoding: PayloadEncoding::Dword,
            wire_bytes,
            file_offset: 0,
        }
    }

    fn new_byte(opcode: u8, value: u8) -> Self {
        Self {
            opcode,
            payload: vec![value],
            encoding: PayloadEncoding::Byte,
            wire_bytes: vec![opcode, value],
            file_offset: 0,
        }
    }

    fn replace_byte_payload(&mut self, value: u8) -> Result<(), FlpError> {
        if self.encoding != PayloadEncoding::Byte || self.payload.len() != 1 {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a one-byte payload",
            });
        }
        let Some(wire_payload) = self.wire_bytes.get_mut(1) else {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "byte event does not contain its payload",
            });
        };
        *wire_payload = value;
        self.payload[0] = value;
        Ok(())
    }

    fn replace_dword_payload(&mut self, value: u32) -> Result<(), FlpError> {
        if self.encoding != PayloadEncoding::Dword || self.payload.len() != 4 {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a four-byte payload",
            });
        }
        let encoded = value.to_le_bytes();
        let Some(wire_payload) = self.wire_bytes.get_mut(1..5) else {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "dword event does not contain its payload",
            });
        };
        wire_payload.copy_from_slice(&encoded);
        self.payload.copy_from_slice(&encoded);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlpDocument {
    header: FlpHeader,
    events: Vec<FlpEvent>,
    project_version: Option<String>,
    metadata: ProjectMetadata,
    trailing_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectMetadata {
    tempo_milli_bpm: Option<u32>,
    time_signature: Option<(u8, u8)>,
    build_number: Option<u32>,
    title: Option<String>,
    author: Option<String>,
    comments: Option<String>,
    genre: Option<String>,
    web_link: Option<String>,
}

/// Fields that can be changed in FL Studio's Project Info dialog.
///
/// A `None` field is left unchanged. `Some("")` clears that field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectInfoEdit {
    pub title: Option<String>,
    pub author: Option<String>,
    pub comments: Option<String>,
    pub genre: Option<String>,
    pub web_link: Option<String>,
}

/// Project settings verified against FL Studio 26.1.6 saves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectSettings {
    pub play_truncated_notes_in_clips: bool,
    pub fast_declick_for_cut_groups: bool,
}

/// Fields that can be changed in the supported Project settings subset.
///
/// A `None` field is left unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectSettingsEdit {
    pub play_truncated_notes_in_clips: Option<bool>,
    pub fast_declick_for_cut_groups: Option<bool>,
}

/// A known FL channel kind, while retaining unrecognized raw values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelType {
    Sampler,
    Native,
    Layer,
    Instrument,
    Automation,
    Unknown(u8),
}

impl ChannelType {
    pub fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Sampler,
            2 => Self::Native,
            3 => Self::Layer,
            4 => Self::Instrument,
            5 => Self::Automation,
            other => Self::Unknown(other),
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            Self::Sampler => 0,
            Self::Native => 2,
            Self::Layer => 3,
            Self::Instrument => 4,
            Self::Automation => 5,
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChannelSummary {
    id: u16,
    kind: Option<u8>,
    enabled: Option<bool>,
    volume: Option<u32>,
    pan: Option<i32>,
    levels_editable: bool,
    volume_priority: u8,
    pan_priority: u8,
    plugin_identifier: Option<String>,
    display_name: Option<String>,
    sample_path: Option<String>,
    layer_children: Vec<u16>,
    layer_flags: Option<u32>,
    first_event_index: usize,
    end_event_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelPluginState {
    channel_id: u16,
    plugin_identifier: Option<String>,
    display_name: Option<String>,
    wrapper_payload: Option<Vec<u8>>,
    data_payload: Vec<u8>,
    data_event_index: usize,
    vst_metadata: Option<VstPluginStateMetadata>,
}

/// Identity fields embedded in an FLP VST plug-in state event.
///
/// The complete event remains available through [`ChannelPluginState::data_payload`];
/// this structure decodes the length-prefixed identity fields and state-field byte range.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VstPluginStateMetadata {
    format_marker: u32,
    plugin_info: Option<Vec<u8>>,
    fourcc: Option<String>,
    guid: Option<Vec<u8>>,
    name: Option<String>,
    path: Option<String>,
    vendor: Option<String>,
    state_data_range: Option<std::ops::Range<usize>>,
}

impl ChannelPluginState {
    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn plugin_identifier(&self) -> Option<&str> {
        self.plugin_identifier.as_deref()
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Opaque `0xD4` wrapper metadata, when present.
    pub fn wrapper_payload(&self) -> Option<&[u8]> {
        self.wrapper_payload.as_deref()
    }

    /// Opaque `0xD5` per-instance plug-in data.
    pub fn data_payload(&self) -> &[u8] {
        &self.data_payload
    }

    /// Nested state bytes from the recognized VST field 53, when present.
    /// The returned slice points into the original `0xD5` data payload.
    pub fn vst_state_bytes(&self) -> Option<&[u8]> {
        let range = self.vst_metadata.as_ref()?.state_data_range.as_ref()?;
        self.data_payload.get(range.clone())
    }

    pub fn data_event_index(&self) -> usize {
        self.data_event_index
    }

    /// VST identity metadata when the `0xD5` event has a recognized VST envelope.
    pub fn vst_metadata(&self) -> Option<&VstPluginStateMetadata> {
        self.vst_metadata.as_ref()
    }
}

impl VstPluginStateMetadata {
    /// FL's VST wrapper serialization marker, observed as 8, 10, and 12.
    pub fn format_marker(&self) -> u32 {
        self.format_marker
    }

    /// Opaque 16-byte plug-in information field, when present.
    pub fn plugin_info(&self) -> Option<&[u8]> {
        self.plugin_info.as_deref()
    }

    /// VST2 four-character identifier, when present.
    pub fn fourcc(&self) -> Option<&str> {
        self.fourcc.as_deref()
    }

    /// Raw 16-byte GUID stored in the FLP envelope.
    pub fn guid(&self) -> Option<&[u8]> {
        self.guid.as_deref()
    }

    /// VST factory name stored in the FLP envelope.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Original plug-in binary path stored in the FLP envelope.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Plug-in vendor stored in the FLP envelope.
    pub fn vendor(&self) -> Option<&str> {
        self.vendor.as_deref()
    }

    /// Size of the nested plug-in state field, without copying the state bytes.
    pub fn state_bytes(&self) -> Option<usize> {
        self.state_data_range
            .as_ref()
            .map(|range| range.end - range.start)
    }

    /// VST3-style 32-digit class UID converted to the format used by the host API.
    pub fn class_uid(&self) -> Option<String> {
        let guid: [u8; 16] = self.guid.as_deref()?.try_into().ok()?;
        Some(format!(
            "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{}",
            guid[3],
            guid[2],
            guid[1],
            guid[0],
            guid[5],
            guid[4],
            guid[7],
            guid[6],
            guid[8..]
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ))
    }
}

impl ChannelSummary {
    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn kind(&self) -> Option<u8> {
        self.kind
    }

    pub fn channel_type(&self) -> Option<ChannelType> {
        self.kind.map(ChannelType::from_raw)
    }

    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    /// Raw FL channel volume value, in the project's 0..=12800 control range.
    pub fn volume(&self) -> Option<u32> {
        self.volume
    }

    /// Raw FL channel pan value, in the project's 0..=12800 control range.
    pub fn pan(&self) -> Option<i32> {
        self.pan
    }

    /// Whether this channel has a recognized modern levels event that can be edited losslessly.
    pub fn levels_editable(&self) -> bool {
        self.levels_editable
    }

    pub fn plugin_identifier(&self) -> Option<&str> {
        self.plugin_identifier.as_deref()
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Sample source path decoded from a sample-bearing channel's `0xC4` string event.
    pub fn sample_path(&self) -> Option<&str> {
        self.sample_path.as_deref()
    }

    /// Channel IDs referenced by a Layer channel's repeated `0x5E` events.
    pub fn layer_child_ids(&self) -> Option<&[u16]> {
        (self.channel_type() == Some(ChannelType::Layer)).then_some(&self.layer_children)
    }

    /// Raw Layer flags from the `0x90` dword event.
    pub fn layer_flags(&self) -> Option<u32> {
        (self.channel_type() == Some(ChannelType::Layer))
            .then_some(self.layer_flags)
            .flatten()
    }

    /// Observed Layer flag bit 0, named `Random` by independent format research.
    pub fn layer_random_enabled(&self) -> Option<bool> {
        self.layer_flags().map(|flags| flags & 1 != 0)
    }

    /// Observed Layer flag bit 1, named `Crossfade` by independent format research.
    pub fn layer_crossfade_enabled(&self) -> Option<bool> {
        self.layer_flags().map(|flags| flags & 2 != 0)
    }

    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

/// One point from the `0xEA` payload of a kind-5 automation channel.
///
/// Positions are cumulative beats from the start of the automation clip. The
/// four trailing bytes are retained as opaque point state for forward
/// compatibility.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationPoint {
    position_beats: f64,
    value: f64,
    tension: f32,
    trailing_bytes: [u8; 4],
}

impl AutomationPoint {
    pub fn position_beats(&self) -> f64 {
        self.position_beats
    }

    /// Normalized automation value, usually within 0..=1.
    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn tension(&self) -> f32 {
        self.tension
    }

    /// Opaque four-byte point state preserved from the source project.
    pub fn trailing_bytes(&self) -> [u8; 4] {
        self.trailing_bytes
    }
}

/// Decoded automation points attached to one type-5 Channel Rack channel.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationChannel {
    channel_id: u16,
    display_name: Option<String>,
    points: Vec<AutomationPoint>,
    data_event_index: Option<usize>,
}

impl AutomationChannel {
    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub fn points(&self) -> &[AutomationPoint] {
        &self.points
    }

    /// Source `0xEA` event index, or `None` when the channel has no point blob.
    pub fn data_event_index(&self) -> Option<usize> {
        self.data_event_index
    }
}

/// Fields that can be changed on an existing automation point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomationPointEdit {
    pub position_beats: Option<f64>,
    pub value: Option<f64>,
    pub tension: Option<f32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Pattern {
    pub id: u16,
    pub name: Option<String>,
    pub length_ticks: Option<u32>,
    pub notes: Vec<PatternNote>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PatternNote {
    pub position: u32,
    pub flags: u16,
    pub channel_id: u16,
    pub length: u32,
    pub key: u16,
    pub group: u16,
    pub fine_pitch: u8,
    pub reserved: u8,
    pub release: u8,
    pub midi_channel: u8,
    pub pan: u8,
    pub velocity: u8,
    pub mod_x: u8,
    pub mod_y: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PatternNoteEdit {
    pub position: Option<u32>,
    pub flags: Option<u16>,
    pub length: Option<u32>,
    pub key: Option<u16>,
    pub group: Option<u16>,
    pub fine_pitch: Option<u8>,
    pub release: Option<u8>,
    pub midi_channel: Option<u8>,
    pub pan: Option<u8>,
    pub velocity: Option<u8>,
    pub mod_x: Option<u8>,
    pub mod_y: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpeggioDirection {
    Up,
    Down,
    UpDown,
}

/// Parameters for replacing simultaneous notes with a gated arpeggio sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpeggioOptions {
    pub step_ticks: u32,
    pub range_octaves: u8,
    pub gate_percent: u8,
    pub direction: ArpeggioDirection,
}

/// Parameters for seeded note velocity, pan, and pitch randomization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RandomizerOptions {
    pub seed: u64,
    pub velocity_amount_percent: i16,
    pub pan_amount_percent: i16,
    pub pitch_range_semitones: u8,
    pub bipolar: bool,
    pub reset_levels: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arrangement {
    pub id: u16,
    pub name: Option<String>,
    pub clips: Vec<PlaylistClip>,
    pub time_markers: Vec<TimeMarker>,
}

/// A Playlist marker or time-signature marker stored in an arrangement.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimeMarker {
    raw_position: u32,
    numerator: Option<u8>,
    denominator: Option<u8>,
    name: Option<String>,
    source_events: TimeMarkerSourceEvents,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TimeMarkerSourceEvents {
    position: Option<usize>,
    numerator: Option<usize>,
    denominator: Option<usize>,
    name: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimeMarkerEdit {
    pub position_ticks: Option<u32>,
    pub is_signature: Option<bool>,
    pub numerator: Option<u8>,
    pub denominator: Option<u8>,
    pub name: Option<String>,
}

impl TimeMarker {
    /// Original little-endian `0x94` value, including all type/flag bits.
    pub fn raw_position(&self) -> u32 {
        self.raw_position
    }

    /// Tick position with the documented signature-kind bit removed.
    /// The raw dword remains available through `raw_position()` for unknown high bits.
    pub fn position_ticks(&self) -> u32 {
        self.raw_position & TIME_MARKER_TICK_MASK
    }

    /// True when bit 27 marks this record as a time-signature marker.
    pub fn is_signature(&self) -> bool {
        self.raw_position & TIME_MARKER_SIGNATURE_BIT != 0
    }

    pub fn numerator(&self) -> Option<u8> {
        self.numerator
    }

    pub fn denominator(&self) -> Option<u8> {
        self.denominator
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

#[derive(Clone, Copy)]
enum TimeMarkerTarget {
    Arrangement {
        arrangement_index: usize,
        marker_index: usize,
    },
    Pending {
        marker_index: usize,
    },
}

fn time_marker_at_mut<'a>(
    target: TimeMarkerTarget,
    arrangements: &'a mut [Arrangement],
    pending: &'a mut [TimeMarker],
) -> Option<&'a mut TimeMarker> {
    match target {
        TimeMarkerTarget::Arrangement {
            arrangement_index,
            marker_index,
        } => arrangements
            .get_mut(arrangement_index)?
            .time_markers
            .get_mut(marker_index),
        TimeMarkerTarget::Pending { marker_index } => pending.get_mut(marker_index),
    }
}

fn next_randomizer_value(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value >> 12;
    value ^= value << 25;
    value ^= value >> 27;
    *state = value;
    value.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn randomizer_offset(state: &mut u64, range: i32, negative: bool, bipolar: bool) -> i32 {
    if range == 0 {
        return 0;
    }
    if bipolar {
        let width = u64::try_from(range * 2 + 1).expect("randomizer range is bounded");
        (next_randomizer_value(state) % width) as i32 - range
    } else {
        let width = u64::try_from(range + 1).expect("randomizer range is bounded");
        let value = (next_randomizer_value(state) % width) as i32;
        if negative { -value } else { value }
    }
}

fn note_index_in_scope(note_index: usize, selected_indices: Option<&HashSet<usize>>) -> bool {
    selected_indices.is_none_or(|indices| indices.contains(&note_index))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTrack {
    /// One-based Playlist track identifier stored in the `0xEE` record.
    pub id: u32,
    /// The adjacent `0x2B` byte is retained without assigning it a meaning.
    pub state_byte: Option<u8>,
    pub name: Option<String>,
    /// Raw 70-byte `0xEE` state payload.
    pub state_bytes: Vec<u8>,
}

/// Read-only Mixer insert fields recognized from the observed `0x9A`, `0x93`,
/// `0x95` sequence. The source events remain byte-exact in [`FlpDocument::events`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerInsertSummary {
    ordinal: usize,
    input_raw: i32,
    output_raw: i32,
    color_raw: u32,
    icon_raw: Option<i16>,
    name: Option<String>,
    first_event_index: usize,
    end_event_index: usize,
}

impl MixerInsertSummary {
    /// Zero-based order among Mixer insert records recognized in the event stream.
    pub fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Raw signed value carried by the insert's `0x9A` event.
    pub fn input_raw(&self) -> i32 {
        self.input_raw
    }

    /// Raw signed value carried by the insert's `0x93` event.
    pub fn output_raw(&self) -> i32 {
        self.output_raw
    }

    /// Raw four-byte value carried by the insert's `0x95` event.
    pub fn color_raw(&self) -> u32 {
        self.color_raw
    }

    /// Raw signed icon value carried by the insert's `0x5F` event, when present.
    pub fn icon_raw(&self) -> Option<i16> {
        self.icon_raw
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Event range associated with this insert record, ending before the next recognized record.
    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

/// Recognized parameter-ID interpretations for Mixer `0xE1` records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerParameterKind {
    SlotEnabled,
    SlotMix,
    RouteVolume,
    Volume,
    Pan,
    StereoSeparation,
    LowEqGain,
    MidEqGain,
    HighEqGain,
    LowEqFrequency,
    MidEqFrequency,
    HighEqFrequency,
    LowEqQ,
    MidEqQ,
    HighEqQ,
    Unknown(u8),
}

/// One 12-byte Mixer parameter record from a `0xE1` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerParameterRecord {
    event_index: usize,
    record_index: usize,
    prefix: [u8; 4],
    parameter_id: u8,
    reserved: u8,
    channel_data: u16,
    value: i32,
}

impl MixerParameterRecord {
    pub fn event_index(&self) -> usize {
        self.event_index
    }

    pub fn record_index(&self) -> usize {
        self.record_index
    }

    /// Four leading bytes retained without assigning them a meaning.
    pub fn prefix(&self) -> [u8; 4] {
        self.prefix
    }

    pub fn parameter_id(&self) -> u8 {
        self.parameter_id
    }

    /// Interprets parameter IDs recognized by an independent FLP parser.
    pub fn kind(&self) -> MixerParameterKind {
        match self.parameter_id {
            0 => MixerParameterKind::SlotEnabled,
            1 => MixerParameterKind::SlotMix,
            64..=191 => MixerParameterKind::RouteVolume,
            192 => MixerParameterKind::Volume,
            193 => MixerParameterKind::Pan,
            194 => MixerParameterKind::StereoSeparation,
            208 => MixerParameterKind::LowEqGain,
            209 => MixerParameterKind::MidEqGain,
            210 => MixerParameterKind::HighEqGain,
            216 => MixerParameterKind::LowEqFrequency,
            217 => MixerParameterKind::MidEqFrequency,
            218 => MixerParameterKind::HighEqFrequency,
            224 => MixerParameterKind::LowEqQ,
            225 => MixerParameterKind::MidEqQ,
            226 => MixerParameterKind::HighEqQ,
            value => MixerParameterKind::Unknown(value),
        }
    }

    /// Candidate target index extracted from bits 6–12 of `channel_data`.
    /// Its relationship to visible Mixer insert numbering still needs broader validation.
    pub fn target_index(&self) -> u8 {
        ((self.channel_data >> 6) & 0x7F) as u8
    }

    /// Candidate slot index extracted from bits 0–5 of `channel_data`.
    pub fn slot_index(&self) -> u8 {
        (self.channel_data & 0x3F) as u8
    }

    /// Uninterpreted top three bits of `channel_data`.
    pub fn target_scope_raw(&self) -> u8 {
        (self.channel_data >> 13) as u8
    }

    pub fn reserved(&self) -> u8 {
        self.reserved
    }

    /// Raw little-endian word retained without assigning its bit fields a meaning.
    pub fn channel_data(&self) -> u16 {
        self.channel_data
    }

    /// Raw signed parameter value.
    pub fn value(&self) -> i32 {
        self.value
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistClip {
    pub position_ticks: u32,
    pub pattern_base: u16,
    pub item_index: u16,
    pub length_ticks: u32,
    pub raw_track_index: u16,
    pub track_index: Option<u16>,
    pub group: u16,
    pub unknown_word: u16,
    pub item_flags: u16,
    pub header_bytes: [u8; 4],
    pub start_offset: f32,
    pub end_offset: f32,
    pub clip_id: Option<u32>,
    pub reserved: Vec<u8>,
    pub scale: Option<f64>,
    pub trailing_bytes: Vec<u8>,
    pub record_size: usize,
    source_event_index: usize,
    source_record_index: usize,
}

impl PlaylistClip {
    pub fn playlist_track_id(&self) -> Option<u32> {
        self.track_index
            .and_then(|track_index| u32::from(track_index).checked_add(1))
    }

    pub fn target(&self) -> PlaylistClipTarget {
        if self.item_index >= self.pattern_base {
            PlaylistClipTarget::Pattern {
                id: self.item_index - self.pattern_base,
            }
        } else {
            PlaylistClipTarget::Channel {
                id: self.item_index,
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistClipTarget {
    Pattern { id: u16 },
    Channel { id: u16 },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaylistClipEdit {
    pub position_ticks: Option<u32>,
    pub length_ticks: Option<u32>,
    pub item_index: Option<u16>,
    pub raw_track_index: Option<u16>,
    pub group: Option<u16>,
    pub item_flags: Option<u16>,
    pub start_offset: Option<f32>,
    pub end_offset: Option<f32>,
    pub scale: Option<f64>,
}

impl PatternNote {
    fn decode(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), FLP_NOTE_RECORD_SIZE);
        Self {
            position: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            flags: u16::from_le_bytes([bytes[4], bytes[5]]),
            channel_id: u16::from_le_bytes([bytes[6], bytes[7]]),
            length: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            key: u16::from_le_bytes([bytes[12], bytes[13]]),
            group: u16::from_le_bytes([bytes[14], bytes[15]]),
            fine_pitch: bytes[16],
            reserved: bytes[17],
            release: bytes[18],
            midi_channel: bytes[19],
            pan: bytes[20],
            velocity: bytes[21],
            mod_x: bytes[22],
            mod_y: bytes[23],
        }
    }

    fn encode_into(&self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), FLP_NOTE_RECORD_SIZE);
        bytes[0..4].copy_from_slice(&self.position.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.flags.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.channel_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.length.to_le_bytes());
        bytes[12..14].copy_from_slice(&self.key.to_le_bytes());
        bytes[14..16].copy_from_slice(&self.group.to_le_bytes());
        bytes[16] = self.fine_pitch;
        bytes[17] = self.reserved;
        bytes[18] = self.release;
        bytes[19] = self.midi_channel;
        bytes[20] = self.pan;
        bytes[21] = self.velocity;
        bytes[22] = self.mod_x;
        bytes[23] = self.mod_y;
    }

    fn apply(&mut self, edit: PatternNoteEdit) {
        if let Some(value) = edit.position {
            self.position = value;
        }
        if let Some(value) = edit.flags {
            self.flags = value;
        }
        if let Some(value) = edit.length {
            self.length = value;
        }
        if let Some(value) = edit.key {
            self.key = value;
        }
        if let Some(value) = edit.group {
            self.group = value;
        }
        if let Some(value) = edit.fine_pitch {
            self.fine_pitch = value;
        }
        if let Some(value) = edit.release {
            self.release = value;
        }
        if let Some(value) = edit.midi_channel {
            self.midi_channel = value;
        }
        if let Some(value) = edit.pan {
            self.pan = value;
        }
        if let Some(value) = edit.velocity {
            self.velocity = value;
        }
        if let Some(value) = edit.mod_x {
            self.mod_x = value;
        }
        if let Some(value) = edit.mod_y {
            self.mod_y = value;
        }
    }
}

impl ProjectMetadata {
    /// Returns the tempo as an integer number of thousandths of a beat per minute.
    pub fn tempo_milli_bpm(&self) -> Option<u32> {
        self.tempo_milli_bpm
    }

    pub fn tempo_bpm(&self) -> Option<f64> {
        self.tempo_milli_bpm
            .map(|milli_bpm| f64::from(milli_bpm) / 1000.0)
    }

    pub fn time_signature(&self) -> Option<(u8, u8)> {
        self.time_signature
    }

    pub fn build_number(&self) -> Option<u32> {
        self.build_number
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    pub fn comments(&self) -> Option<&str> {
        self.comments.as_deref()
    }

    pub fn genre(&self) -> Option<&str> {
        self.genre.as_deref()
    }

    pub fn web_link(&self) -> Option<&str> {
        self.web_link.as_deref()
    }
}

impl FlpDocument {
    pub fn parse(bytes: &[u8]) -> Result<Self, FlpError> {
        if bytes.len() < 8 {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLhd chunk header",
            });
        }
        require_magic(bytes, 0, FLHD)?;

        let header_content_length = read_u32(bytes, 4, "FLhd length")? as usize;
        if header_content_length < MIN_HEADER_CONTENT_LENGTH {
            return Err(FlpError::InvalidHeaderLength(header_content_length as u32));
        }
        let data_chunk_offset = 8usize
            .checked_add(header_content_length)
            .ok_or(FlpError::LengthOverflow)?;
        if data_chunk_offset > bytes.len() {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLhd contents",
            });
        }
        if data_chunk_offset < 14 {
            return Err(FlpError::InvalidHeaderLength(header_content_length as u32));
        }

        let header = FlpHeader {
            format: read_u16(bytes, 8, "FLhd format")?,
            legacy_channel_count: read_u16(bytes, 10, "FLhd channel count")?,
            ppq: read_u16(bytes, 12, "FLhd PPQ")?,
            extension: bytes[14..data_chunk_offset].to_vec(),
        };

        let event_start = data_chunk_offset
            .checked_add(8)
            .ok_or(FlpError::LengthOverflow)?;
        if event_start > bytes.len() {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLdt chunk header",
            });
        }
        require_magic(bytes, data_chunk_offset, FLDT)?;
        let data_length = read_u32(bytes, data_chunk_offset + 4, "FLdt length")? as usize;
        let event_end = event_start
            .checked_add(data_length)
            .ok_or(FlpError::LengthOverflow)?;
        if event_end > bytes.len() {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLdt event stream",
            });
        }

        let stream = &bytes[event_start..event_end];
        let project_version = detect_project_version(stream);
        let modern_ac_event = project_version
            .as_deref()
            .and_then(|value| value.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 25);

        let mut events = Vec::new();
        let mut cursor = 0usize;
        while cursor < stream.len() {
            let (event, next_cursor) = parse_event(stream, cursor, event_start, modern_ac_event)?;
            if next_cursor <= cursor {
                return Err(FlpError::InvalidEvent {
                    offset: event_start + cursor,
                    detail: "event parser did not advance",
                });
            }
            events.push(event);
            cursor = next_cursor;
        }

        let metadata = read_project_metadata(&events, project_version.as_deref());

        Ok(Self {
            header,
            events,
            project_version,
            metadata,
            trailing_bytes: bytes[event_end..].to_vec(),
        })
    }

    pub fn header(&self) -> &FlpHeader {
        &self.header
    }

    pub fn events(&self) -> &[FlpEvent] {
        &self.events
    }

    /// Returns channel summaries from the stream's `0x40` channel markers.
    /// Unrecognized channel data remains available in `events()`.
    pub fn channels(&self) -> Vec<ChannelSummary> {
        let mut channels = Vec::new();
        let mut current: Option<ChannelSummary> = None;

        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode == 0x40 && event.payload.len() == 2 {
                if let Some(mut channel) = current.take() {
                    channel.end_event_index = event_index;
                    channels.push(channel);
                }
                current = Some(ChannelSummary {
                    id: u16::from_le_bytes([event.payload[0], event.payload[1]]),
                    first_event_index: event_index,
                    end_event_index: self.events.len(),
                    ..ChannelSummary::default()
                });
                continue;
            }

            if event.opcode == 0x62 && current.is_some() {
                if let Some(mut channel) = current.take() {
                    channel.end_event_index = event_index;
                    channels.push(channel);
                }
                break;
            }

            let Some(channel) = current.as_mut() else {
                continue;
            };
            match event.opcode {
                0x00 if event.payload.len() == 1 => {
                    channel.enabled = Some(event.payload[0] != 0);
                }
                0x15 if event.payload.len() == 1 => channel.kind = Some(event.payload[0]),
                0x5E if event.payload.len() == 2 => {
                    channel
                        .layer_children
                        .push(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
                0x90 if event.payload.len() == 4 => {
                    channel.layer_flags = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                // FL 25+ stores channel pan and volume at the start of the 0xDB
                // Levels data event. Keep legacy byte/word values as fallbacks.
                0xDB if event.payload.len() >= 8 => {
                    channel.levels_editable = true;
                    if channel.pan_priority < 3 {
                        channel.pan = Some(i32::from_le_bytes(
                            event.payload[0..4]
                                .try_into()
                                .expect("the 0xDB pan field has four bytes"),
                        ));
                        channel.pan_priority = 3;
                    }
                    if channel.volume_priority < 3 {
                        channel.volume = Some(u32::from_le_bytes(
                            event.payload[4..8]
                                .try_into()
                                .expect("the 0xDB volume field has four bytes"),
                        ));
                        channel.volume_priority = 3;
                    }
                }
                0x48 if event.payload.len() == 2 && channel.volume_priority < 2 => {
                    channel.volume = Some(u32::from(u16::from_le_bytes([
                        event.payload[0],
                        event.payload[1],
                    ])));
                    channel.volume_priority = 2;
                }
                0x49 if event.payload.len() == 2 && channel.pan_priority < 2 => {
                    channel.pan = Some(i32::from(u16::from_le_bytes([
                        event.payload[0],
                        event.payload[1],
                    ])));
                    channel.pan_priority = 2;
                }
                0x02 if event.payload.len() == 1 && channel.volume_priority < 1 => {
                    channel.volume = Some(u32::from(event.payload[0]));
                    channel.volume_priority = 1;
                }
                0x03 if event.payload.len() == 1 && channel.pan_priority < 1 => {
                    channel.pan = Some(i32::from(event.payload[0]));
                    channel.pan_priority = 1;
                }
                0xC9 if channel.plugin_identifier.is_none() => {
                    channel.plugin_identifier =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                0xCB if channel.display_name.is_none() => {
                    channel.display_name =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                0xC0 if channel.display_name.is_none()
                    && uses_legacy_string_encoding(self.project_version.as_deref()) =>
                {
                    channel.display_name =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                // Sampler channels (kind 0) and sample-backed audio channels (kind 4)
                // both use `0xC4` for the source path. Other channel kinds can contain
                // unrelated data in their event range, so keep the scope check narrow.
                0xC4 if matches!(channel.kind, Some(0 | 4)) && channel.sample_path.is_none() => {
                    channel.sample_path =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                _ => {}
            }
        }

        if let Some(mut channel) = current {
            channel.end_event_index = self.events.len();
            channels.push(channel);
        }
        channels
    }

    /// Returns the points found in each type-5 automation channel's `0xEA` blob.
    /// The full event remains byte-exact in `events()`; the 17-byte header,
    /// per-point trailing bytes, and any era-specific trailer are not discarded.
    pub fn automation_channels(&self) -> Result<Vec<AutomationChannel>, FlpError> {
        self.channels()
            .into_iter()
            .filter(|channel| channel.kind == Some(5))
            .map(|channel| {
                let event_index = channel
                    .event_range()
                    .find(|index| self.events[*index].opcode == 0xEA);
                let points = match event_index {
                    Some(index) => decode_automation_points(&self.events[index])?,
                    None => Vec::new(),
                };
                Ok(AutomationChannel {
                    channel_id: channel.id,
                    display_name: channel.display_name,
                    points,
                    data_event_index: event_index,
                })
            })
            .collect()
    }

    /// Returns per-channel plug-in wrapper and data payloads when a `0xD5` event exists.
    /// The raw bytes are retained exactly; recognized VST envelopes also expose identity metadata.
    pub fn channel_plugin_states(&self) -> Vec<ChannelPluginState> {
        self.channels()
            .into_iter()
            .filter_map(|channel| {
                let mut wrapper_payload = None;
                let mut data_payload = None;
                let mut data_event_index = None;
                for index in channel.event_range() {
                    let event = &self.events[index];
                    match event.opcode {
                        0xD4 if wrapper_payload.is_none() => {
                            wrapper_payload = Some(event.payload.clone());
                        }
                        0xD5 if data_payload.is_none() => {
                            data_payload = Some(event.payload.clone());
                            data_event_index = Some(index);
                        }
                        _ => {}
                    }
                }
                let data_payload = data_payload?;
                Some(ChannelPluginState {
                    channel_id: channel.id,
                    plugin_identifier: channel.plugin_identifier,
                    display_name: channel.display_name,
                    wrapper_payload,
                    vst_metadata: parse_vst_plugin_state_metadata(&data_payload),
                    data_payload,
                    data_event_index: data_event_index?,
                })
            })
            .collect()
    }

    /// Returns pattern metadata and note records from the version-specific score event.
    pub fn patterns(&self) -> Result<Vec<Pattern>, FlpError> {
        let mut patterns = Vec::<Pattern>::new();
        let mut pattern_indices = HashMap::<u16, usize>::new();
        let mut current_pattern = None;
        let mut event_index = 0usize;

        while event_index < self.events.len() {
            let event = &self.events[event_index];
            match event.opcode {
                0x41 if event.payload.len() == 2 => {
                    let pattern_id = u16::from_le_bytes([event.payload[0], event.payload[1]]);
                    let pattern_index = *pattern_indices.entry(pattern_id).or_insert_with(|| {
                        let index = patterns.len();
                        patterns.push(Pattern {
                            id: pattern_id,
                            ..Pattern::default()
                        });
                        index
                    });
                    current_pattern = Some(pattern_index);

                    if let Some(notes_event) = self.events.get(event_index + 1)
                        && Self::is_pattern_note_event(notes_event)
                    {
                        if !notes_event
                            .payload
                            .len()
                            .is_multiple_of(FLP_NOTE_RECORD_SIZE)
                        {
                            return Err(FlpError::InvalidEvent {
                                offset: notes_event.file_offset,
                                detail: "pattern note payload is not a whole number of 24-byte records",
                            });
                        }
                        patterns[pattern_index].notes.extend(
                            notes_event
                                .payload
                                .as_chunks::<FLP_NOTE_RECORD_SIZE>()
                                .0
                                .iter()
                                .map(|record| PatternNote::decode(record)),
                        );
                        event_index += 1;
                    }
                }
                0x40 | 0x62 | 0x63 => current_pattern = None,
                0xC1 => {
                    if let Some(pattern_index) = current_pattern
                        && patterns[pattern_index].name.is_none()
                    {
                        patterns[pattern_index].name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                    }
                }
                0xA4 if event.payload.len() == 4 => {
                    if let Some(pattern_index) = current_pattern {
                        patterns[pattern_index].length_ticks = Some(u32::from_le_bytes([
                            event.payload[0],
                            event.payload[1],
                            event.payload[2],
                            event.payload[3],
                        ]));
                    }
                }
                _ => {}
            }
            event_index += 1;
        }
        patterns.sort_by_key(|pattern| pattern.id);
        Ok(patterns)
    }

    /// Creates an empty pattern using the note-event encoding already present in the project.
    ///
    /// Projects without a uniquely recognized pattern-note encoding are rejected rather than
    /// guessing between version-specific score event layouts.
    pub fn create_pattern(&mut self) -> Result<u16, FlpError> {
        let mut score_regions = Vec::new();
        let mut note_opcode = None;
        let mut conflicting_opcodes = false;

        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0x41 || event.payload.len() != 2 {
                continue;
            }
            if let Some(notes_event) = self.events.get(event_index + 1)
                && Self::is_pattern_note_event(notes_event)
            {
                if event.encoding != PayloadEncoding::Word
                    || !matches!(notes_event.encoding, PayloadEncoding::Data { .. })
                    || !notes_event
                        .payload
                        .len()
                        .is_multiple_of(FLP_NOTE_RECORD_SIZE)
                {
                    return Err(FlpError::UnsupportedEdit(
                        "the project's pattern note event layout is not recognized",
                    ));
                }
                let section_start = self.events[..event_index]
                    .iter()
                    .rposition(|candidate| matches!(candidate.opcode, 0x40 | 0x62 | 0x63))
                    .map_or(0, |boundary| boundary + 1);
                let section_end = self.events[event_index + 1..]
                    .iter()
                    .position(|candidate| matches!(candidate.opcode, 0x40 | 0x62 | 0x63))
                    .map_or(self.events.len(), |offset| event_index + 1 + offset);
                score_regions.push((section_start, section_end));

                match note_opcode {
                    None => note_opcode = Some(notes_event.opcode),
                    Some(opcode) if opcode != notes_event.opcode => {
                        conflicting_opcodes = true;
                    }
                    _ => {}
                }
            }
        }

        if conflicting_opcodes || note_opcode.is_none() {
            return Err(FlpError::UnsupportedEdit(
                "cannot infer a unique pattern note-event encoding for this project",
            ));
        }
        let Some((_, insert_index)) = score_regions.iter().copied().max_by_key(|range| range.0)
        else {
            return Err(FlpError::UnsupportedEdit(
                "the project has no recognized pattern marker to extend",
            ));
        };
        let mut pattern_ids = HashSet::new();
        for (region_start, region_end) in score_regions {
            for event in &self.events[region_start..region_end] {
                if event.opcode == 0x41
                    && event.encoding == PayloadEncoding::Word
                    && event.payload.len() == 2
                {
                    pattern_ids.insert(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
            }
        }
        let Some(highest_pattern_id) = pattern_ids.iter().max().copied() else {
            return Err(FlpError::UnsupportedEdit(
                "the recognized score section contains no pattern IDs",
            ));
        };

        let new_id = highest_pattern_id
            .checked_add(1)
            .ok_or(FlpError::LengthOverflow)?;

        let mut candidate = self.clone();
        candidate
            .events
            .insert(insert_index, FlpEvent::new_word(0x41, new_id));
        candidate.events.insert(
            insert_index + 1,
            FlpEvent::new_data(
                note_opcode.expect("unique opcode checked above"),
                Vec::new(),
            )?,
        );
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_id)
    }

    /// Returns playlist arrangements and their stored clip records.
    /// The source events remain available unchanged through `events()`.
    pub fn arrangements(&self) -> Result<Vec<Arrangement>, FlpError> {
        self.arrangements_impl(true)
    }

    /// Returns flattened time markers without decoding Playlist clip records.
    /// This lets callers inspect marker data even when a project's clip layout
    /// is not yet supported by `arrangements()`.
    pub fn time_markers(&self) -> Result<Vec<(u16, TimeMarker)>, FlpError> {
        let arrangements = self.arrangements_impl(false)?;
        let mut markers = Vec::new();
        for arrangement in arrangements {
            markers.extend(
                arrangement
                    .time_markers
                    .into_iter()
                    .map(|marker| (arrangement.id, marker)),
            );
        }
        Ok(markers)
    }

    /// Edits the stored fields of one Playlist time marker while retaining its raw flags
    /// and all unrelated events. Marker indexes are zero-based within the arrangement.
    pub fn edit_time_marker(
        &mut self,
        arrangement_id: u16,
        marker_index: usize,
        edit: TimeMarkerEdit,
    ) -> Result<(), FlpError> {
        if edit.name.as_deref().is_some_and(|name| name.contains('\0')) {
            return Err(FlpError::UnsupportedEdit(
                "time marker names cannot contain an embedded NUL character",
            ));
        }
        if edit.numerator == Some(0) || edit.denominator == Some(0) {
            return Err(FlpError::UnsupportedEdit(
                "time signature numerator and denominator must be positive",
            ));
        }
        if edit
            .position_ticks
            .is_some_and(|position| position > TIME_MARKER_TICK_MASK)
        {
            return Err(FlpError::UnsupportedEdit(
                "time marker positions must fit in the low 27 position bits",
            ));
        }

        let arrangements = self.arrangements_impl(false)?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement id is ambiguous",
            ));
        }
        let marker =
            arrangement
                .time_markers
                .get(marker_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested time marker does not exist",
                ))?;
        let source_events = marker.source_events.clone();
        let position_event_index = source_events.position.ok_or(FlpError::UnsupportedEdit(
            "the selected time marker has no source position event",
        ))?;

        let mut raw_position = marker.raw_position;
        if let Some(position_ticks) = edit.position_ticks {
            raw_position = (raw_position & !TIME_MARKER_TICK_MASK) | position_ticks;
        }
        if let Some(is_signature) = edit.is_signature {
            if is_signature {
                raw_position |= TIME_MARKER_SIGNATURE_BIT;
            } else {
                raw_position &= !TIME_MARKER_SIGNATURE_BIT;
            }
        }

        let is_signature = raw_position & TIME_MARKER_SIGNATURE_BIT != 0;
        let numerator = edit.numerator.or(marker.numerator);
        let denominator = edit.denominator.or(marker.denominator);
        if (edit.numerator.is_some() || edit.denominator.is_some()) && !is_signature {
            return Err(FlpError::UnsupportedEdit(
                "numerator and denominator can only be edited on a time-signature marker",
            ));
        }
        if (edit.numerator.is_some() || edit.denominator.is_some())
            && (numerator.is_none() || denominator.is_none())
        {
            return Err(FlpError::UnsupportedEdit(
                "editing a time signature requires both numerator and denominator values",
            ));
        }
        if edit.is_signature == Some(true)
            && !marker.is_signature()
            && (numerator.is_none() || denominator.is_none())
        {
            return Err(FlpError::UnsupportedEdit(
                "converting a marker to a time signature requires numerator and denominator values",
            ));
        }

        let mut candidate = self.clone();
        candidate.events[position_event_index].replace_dword_payload(raw_position)?;

        let mut insertion_index = position_event_index + 1;
        for event_index in [source_events.numerator, source_events.denominator]
            .into_iter()
            .flatten()
        {
            insertion_index = insertion_index.max(event_index + 1);
        }
        if let Some(name_event_index) = source_events.name {
            insertion_index = insertion_index.min(name_event_index);
        }

        let mut insertions = Vec::<(usize, u8, FlpEvent)>::new();
        for (value, event_index, opcode, rank) in [
            (edit.numerator, source_events.numerator, 0x21, 0),
            (edit.denominator, source_events.denominator, 0x22, 1),
        ] {
            let Some(value) = value else {
                continue;
            };
            if let Some(event_index) = event_index {
                candidate.events[event_index].replace_byte_payload(value)?;
            } else {
                insertions.push((insertion_index, rank, FlpEvent::new_byte(opcode, value)));
            }
        }

        if let Some(name) = edit.name {
            if let Some(name_event_index) = source_events.name {
                let event = &candidate.events[name_event_index];
                if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                    return Err(FlpError::UnsupportedEdit(
                        "the selected time marker name is not a length-prefixed data event",
                    ));
                }
                let utf16 =
                    project_string_is_utf16(&event.payload, candidate.project_version.as_deref());
                let payload = replace_project_string_payload(&event.payload, &name, utf16)?;
                candidate.events[name_event_index].replace_data_payload(payload)?;
            } else {
                let payload = encode_project_string(&name, candidate.project_strings_use_utf16())?;
                insertions.push((insertion_index, 2, FlpEvent::new_data(0xCD, payload)?));
            }
        }

        insertions.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        for (event_index, _, event) in insertions {
            candidate.events.insert(event_index, event);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Creates a Playlist time or signature marker in an arrangement.
    /// Marker fields use the same validation and wire representation as marker edits.
    pub fn create_time_marker(
        &mut self,
        arrangement_id: u16,
        edit: TimeMarkerEdit,
    ) -> Result<usize, FlpError> {
        if edit.name.as_deref().is_some_and(|name| name.contains('\0')) {
            return Err(FlpError::UnsupportedEdit(
                "time marker names cannot contain an embedded NUL character",
            ));
        }
        if edit
            .position_ticks
            .is_some_and(|position| position > TIME_MARKER_TICK_MASK)
        {
            return Err(FlpError::UnsupportedEdit(
                "time marker positions must fit in the low 27 position bits",
            ));
        }
        if edit.numerator == Some(0) || edit.denominator == Some(0) {
            return Err(FlpError::UnsupportedEdit(
                "time signature numerator and denominator must be positive",
            ));
        }

        let is_signature = edit.is_signature.unwrap_or(false);
        if is_signature {
            if edit.numerator.is_none() || edit.denominator.is_none() {
                return Err(FlpError::UnsupportedEdit(
                    "creating a time-signature marker requires numerator and denominator values",
                ));
            }
        } else if edit.numerator.is_some() || edit.denominator.is_some() {
            return Err(FlpError::UnsupportedEdit(
                "numerator and denominator can only be set on a time-signature marker",
            ));
        }

        let arrangements = self.arrangements_impl(false)?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let arrangement = matching_arrangements.next();
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement id is ambiguous",
            ));
        }
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);
        if arrangement.is_none() && (has_arrangement_markers || arrangement_id != 0) {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement does not exist",
            ));
        }

        let (region_start, region_end) = if has_arrangement_markers {
            let start = self
                .events
                .iter()
                .position(|event| {
                    event.opcode == 0x63
                        && event.payload.len() == 2
                        && u16::from_le_bytes([event.payload[0], event.payload[1]])
                            == arrangement_id
                })
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested Playlist arrangement does not exist",
                ))?;
            let region_start = start + 1;
            let region_end = self.events[region_start..]
                .iter()
                .position(|event| {
                    event.opcode == 0x62 || (event.opcode == 0x63 && event.payload.len() == 2)
                })
                .map_or(self.events.len(), |offset| region_start + offset);
            (region_start, region_end)
        } else {
            let region_end = self
                .events
                .iter()
                .rposition(|event| event.opcode == 0x62)
                .unwrap_or(self.events.len());
            (0, region_end)
        };

        let position_ticks = edit.position_ticks.unwrap_or(0);
        let markers =
            arrangement.map_or(&[][..], |arrangement| arrangement.time_markers.as_slice());
        let later_marker = markers
            .iter()
            .filter(|marker| marker.position_ticks() > position_ticks)
            .filter_map(|marker| marker.source_events.position)
            .find(|event_index| *event_index >= region_start && *event_index < region_end);
        let last_marker_end = markers
            .iter()
            .filter_map(|marker| {
                [
                    marker.source_events.position,
                    marker.source_events.numerator,
                    marker.source_events.denominator,
                    marker.source_events.name,
                ]
                .into_iter()
                .flatten()
                .max()
            })
            .map(|event_index| event_index + 1)
            .filter(|event_index| *event_index >= region_start && *event_index <= region_end);
        let first_clip_event = self.events[region_start..region_end]
            .iter()
            .position(|event| event.opcode == 0xE9)
            .map(|offset| region_start + offset);
        let insertion_index = later_marker
            .or_else(|| last_marker_end.max())
            .or(first_clip_event)
            .unwrap_or(region_end);

        let mut raw_position = position_ticks;
        if is_signature {
            raw_position |= TIME_MARKER_SIGNATURE_BIT;
        }
        let mut inserted = vec![FlpEvent::new_dword(0x94, raw_position)];
        if is_signature {
            inserted.push(FlpEvent::new_byte(
                0x21,
                edit.numerator.expect("signature numerator validated"),
            ));
            inserted.push(FlpEvent::new_byte(
                0x22,
                edit.denominator.expect("signature denominator validated"),
            ));
        }
        if let Some(name) = edit.name {
            let payload = encode_project_string(&name, self.project_strings_use_utf16())?;
            inserted.push(FlpEvent::new_data(0xCD, payload)?);
        }

        let mut candidate = self.clone();
        for (offset, event) in inserted.into_iter().enumerate() {
            candidate.events.insert(insertion_index + offset, event);
        }
        candidate.refresh_event_offsets()?;
        let marker_index = candidate
            .arrangements_impl(false)?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .and_then(|arrangement| {
                arrangement
                    .time_markers
                    .iter()
                    .position(|marker| marker.source_events.position == Some(insertion_index))
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the new Playlist marker could not be resolved after insertion",
            ))?;
        *self = candidate;
        Ok(marker_index)
    }

    /// Removes one marker's recognized source events while preserving unrelated events.
    pub fn delete_time_marker(
        &mut self,
        arrangement_id: u16,
        marker_index: usize,
    ) -> Result<(), FlpError> {
        let arrangements = self.arrangements_impl(false)?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement id is ambiguous",
            ));
        }
        let marker =
            arrangement
                .time_markers
                .get(marker_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested time marker does not exist",
                ))?;
        let mut source_event_indices = [
            marker.source_events.position,
            marker.source_events.numerator,
            marker.source_events.denominator,
            marker.source_events.name,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !source_event_indices.iter().any(|index| {
            self.events
                .get(*index)
                .is_some_and(|event| event.opcode == 0x94)
        }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected time marker has no source position event",
            ));
        }
        source_event_indices.sort_unstable();
        source_event_indices.dedup();

        let mut candidate = self.clone();
        for event_index in source_event_indices.into_iter().rev() {
            candidate.events.remove(event_index);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    fn arrangements_impl(&self, include_clips: bool) -> Result<Vec<Arrangement>, FlpError> {
        let mut arrangements = Vec::<Arrangement>::new();
        let mut current_arrangement = None;
        let mut current_time_marker = None;
        let mut pending_time_markers = Vec::new();
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);

        for (event_index, event) in self.events.iter().enumerate() {
            match event.opcode {
                0x63 if event.payload.len() == 2 => {
                    let id = u16::from_le_bytes([event.payload[0], event.payload[1]]);
                    arrangements.push(Arrangement {
                        id,
                        ..Arrangement::default()
                    });
                    current_arrangement = Some(arrangements.len() - 1);
                    if !pending_time_markers.is_empty() {
                        let arrangement_index = arrangements.len() - 1;
                        let marker_start = arrangements[arrangement_index].time_markers.len();
                        let pending_count = pending_time_markers.len();
                        arrangements[arrangement_index]
                            .time_markers
                            .append(&mut pending_time_markers);
                        current_time_marker = match current_time_marker {
                            Some(TimeMarkerTarget::Pending { marker_index })
                                if marker_index < pending_count =>
                            {
                                Some(TimeMarkerTarget::Arrangement {
                                    arrangement_index,
                                    marker_index: marker_start + marker_index,
                                })
                            }
                            _ => None,
                        };
                    } else {
                        current_time_marker = None;
                    }
                }
                0x62 => {
                    if has_arrangement_markers {
                        current_arrangement = None;
                    }
                    current_time_marker = None;
                }
                0xF1 => {
                    if let Some(arrangement_index) = current_arrangement
                        && arrangements[arrangement_index].name.is_none()
                    {
                        arrangements[arrangement_index].name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                    }
                }
                0x94 if event.payload.len() == 4 => {
                    let raw_position = u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword time-marker event has four bytes"),
                    );
                    if let Some(arrangement_index) = current_arrangement {
                        let marker_index = arrangements[arrangement_index].time_markers.len();
                        arrangements[arrangement_index]
                            .time_markers
                            .push(TimeMarker {
                                raw_position,
                                source_events: TimeMarkerSourceEvents {
                                    position: Some(event_index),
                                    ..TimeMarkerSourceEvents::default()
                                },
                                ..TimeMarker::default()
                            });
                        current_time_marker = Some(TimeMarkerTarget::Arrangement {
                            arrangement_index,
                            marker_index,
                        });
                    } else if has_arrangement_markers {
                        let marker_index = pending_time_markers.len();
                        pending_time_markers.push(TimeMarker {
                            raw_position,
                            source_events: TimeMarkerSourceEvents {
                                position: Some(event_index),
                                ..TimeMarkerSourceEvents::default()
                            },
                            ..TimeMarker::default()
                        });
                        current_time_marker = Some(TimeMarkerTarget::Pending { marker_index });
                    } else {
                        if arrangements.is_empty() {
                            arrangements.push(Arrangement::default());
                        }
                        current_arrangement = Some(0);
                        let marker_index = arrangements[0].time_markers.len();
                        arrangements[0].time_markers.push(TimeMarker {
                            raw_position,
                            source_events: TimeMarkerSourceEvents {
                                position: Some(event_index),
                                ..TimeMarkerSourceEvents::default()
                            },
                            ..TimeMarker::default()
                        });
                        current_time_marker = Some(TimeMarkerTarget::Arrangement {
                            arrangement_index: 0,
                            marker_index,
                        });
                    }
                }
                0x21 if event.payload.len() == 1 => {
                    if let Some(marker) = current_time_marker.and_then(|target| {
                        time_marker_at_mut(target, &mut arrangements, &mut pending_time_markers)
                    }) {
                        marker.numerator = Some(event.payload[0]);
                        marker.source_events.numerator = Some(event_index);
                    }
                }
                0x22 if event.payload.len() == 1 => {
                    if let Some(marker) = current_time_marker.and_then(|target| {
                        time_marker_at_mut(target, &mut arrangements, &mut pending_time_markers)
                    }) {
                        marker.denominator = Some(event.payload[0]);
                        marker.source_events.denominator = Some(event_index);
                    }
                }
                0xCD => {
                    if let Some(marker) = current_time_marker.and_then(|target| {
                        time_marker_at_mut(target, &mut arrangements, &mut pending_time_markers)
                    }) {
                        marker.name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                        marker.source_events.name = Some(event_index);
                    }
                    current_time_marker = None;
                }
                0xE9 if !include_clips => {}
                0xE9 => {
                    let arrangement_index = if has_arrangement_markers {
                        let Some(arrangement_index) = current_arrangement else {
                            continue;
                        };
                        arrangement_index
                    } else {
                        if arrangements.is_empty() {
                            arrangements.push(Arrangement::default());
                        }
                        0
                    };
                    let record_size = playlist_clip_record_size(
                        self.project_version.as_deref(),
                        &event.payload,
                        event.file_offset,
                    )?;
                    if event.payload.len() % record_size != 0 {
                        return Err(FlpError::InvalidEvent {
                            offset: event.file_offset,
                            detail: "playlist clip payload is not a whole number of supported records",
                        });
                    }
                    arrangements[arrangement_index].clips.extend(
                        event.payload.chunks_exact(record_size).enumerate().map(
                            |(record_index, record)| {
                                decode_playlist_clip(record, event_index, record_index)
                            },
                        ),
                    );
                }
                _ => {}
            }
        }
        if !pending_time_markers.is_empty() && !has_arrangement_markers {
            if arrangements.is_empty() {
                arrangements.push(Arrangement::default());
            }
            arrangements[0]
                .time_markers
                .append(&mut pending_time_markers);
        }
        Ok(arrangements)
    }

    /// Returns Playlist tracks from the observed `0xEE`, `0x2B`, `0xEF` event sequence.
    pub fn playlist_tracks(&self) -> Vec<PlaylistTrack> {
        let mut tracks = Vec::new();
        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0xEE || event.payload.len() != 70 {
                continue;
            }
            let id = u32::from_le_bytes([
                event.payload[0],
                event.payload[1],
                event.payload[2],
                event.payload[3],
            ]);
            let next_event = self.events.get(event_index + 1);
            let state_byte = next_event
                .filter(|next| next.opcode == 0x2B && next.payload.len() == 1)
                .map(|next| next.payload[0]);
            let name_event_index = event_index + if state_byte.is_some() { 2 } else { 1 };
            let name = self
                .events
                .get(name_event_index)
                .filter(|name_event| name_event.opcode == 0xEF)
                .and_then(|name_event| {
                    decode_project_string(&name_event.payload, self.project_version.as_deref())
                })
                .filter(|value| !value.is_empty());
            tracks.push(PlaylistTrack {
                id,
                state_byte,
                name,
                state_bytes: event.payload.clone(),
            });
        }
        tracks
    }

    /// Returns Mixer insert summaries for the observed adjacent `0x9A`, `0x93`, `0x95`
    /// record signature. This is intentionally read-only; the unparsed insert and effect data
    /// remains available byte-for-byte through `events()`.
    pub fn mixer_inserts(&self) -> Vec<MixerInsertSummary> {
        let starts = self
            .events
            .windows(3)
            .enumerate()
            .filter_map(|(index, window)| {
                (window[0].opcode == 0x9A
                    && window[0].payload.len() == 4
                    && window[1].opcode == 0x93
                    && window[1].payload.len() == 4
                    && window[2].opcode == 0x95
                    && window[2].payload.len() == 4)
                    .then_some(index)
            })
            .collect::<Vec<_>>();

        starts
            .iter()
            .enumerate()
            .map(|(ordinal, &start)| {
                let end = starts
                    .get(ordinal + 1)
                    .copied()
                    .unwrap_or(self.events.len());
                let insert_events = &self.events[start..end];
                let read_i32 = |offset: usize| {
                    i32::from_le_bytes(
                        self.events[start + offset].payload[..4]
                            .try_into()
                            .expect("the Mixer record signature checked four-byte fields"),
                    )
                };
                let color_raw = u32::from_le_bytes(
                    self.events[start + 2].payload[..4]
                        .try_into()
                        .expect("the Mixer record signature checked four-byte fields"),
                );
                let icon_raw = insert_events
                    .iter()
                    .find(|event| event.opcode == 0x5F && event.payload.len() == 2)
                    .map(|event| i16::from_le_bytes([event.payload[0], event.payload[1]]));
                let name = insert_events
                    .iter()
                    .find(|event| event.opcode == 0xCC)
                    .and_then(|event| {
                        decode_project_string(&event.payload, self.project_version.as_deref())
                    })
                    .filter(|value| !value.is_empty());

                MixerInsertSummary {
                    ordinal,
                    input_raw: read_i32(0),
                    output_raw: read_i32(1),
                    color_raw,
                    icon_raw,
                    name,
                    first_event_index: start,
                    end_event_index: end,
                }
            })
            .collect()
    }

    /// Renames a recognized Mixer insert through its existing `0xCC` name event.
    /// The event's string encoding, terminator convention, and bytes after the
    /// terminator are retained; every other event remains untouched.
    pub fn set_mixer_insert_name(
        &mut self,
        insert_ordinal: usize,
        name: &str,
    ) -> Result<(), FlpError> {
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "Mixer insert names cannot contain an embedded NUL character",
            ));
        }

        let insert = self
            .mixer_inserts()
            .into_iter()
            .find(|insert| insert.ordinal() == insert_ordinal)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer insert does not exist",
            ))?;
        let event_index = insert
            .event_range()
            .find(|index| self.events[*index].opcode == 0xCC)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected Mixer insert has no recognized 0xCC name event",
            ))?;
        let event = &self.events[event_index];
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected Mixer insert's 0xCC event is not a length-prefixed data event",
            ));
        }

        let old_payload = &event.payload;
        let utf16 = project_string_is_utf16(old_payload, self.project_version.as_deref());
        let (suffix_start, had_terminator) = if utf16 {
            old_payload
                .as_chunks::<2>()
                .0
                .iter()
                .position(|pair| u16::from_le_bytes(*pair) == 0)
                .map_or((old_payload.len(), false), |unit_index| {
                    ((unit_index + 1) * 2, true)
                })
        } else {
            old_payload
                .iter()
                .position(|byte| *byte == 0)
                .map_or((old_payload.len(), false), |byte_index| {
                    (byte_index + 1, true)
                })
        };

        let mut replacement_payload = Vec::new();
        if utf16 {
            for unit in name.encode_utf16() {
                replacement_payload.extend_from_slice(&unit.to_le_bytes());
            }
            if had_terminator {
                replacement_payload.extend_from_slice(&[0, 0]);
            }
        } else {
            for character in name.chars() {
                let byte = windows_1252_byte(character).ok_or(FlpError::UnsupportedEdit(
                    "the name contains a character unavailable in the project's legacy encoding",
                ))?;
                replacement_payload.push(byte);
            }
            if had_terminator {
                replacement_payload.push(0);
            }
        }
        replacement_payload.extend_from_slice(&old_payload[suffix_start..]);

        self.events[event_index].replace_data_payload(replacement_payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Decodes only the fixed 12-byte framing observed in Mixer `0xE1` parameter records.
    /// Unknown fields remain raw, and a different record size is reported instead of guessed.
    pub fn mixer_parameter_records(&self) -> Result<Vec<MixerParameterRecord>, FlpError> {
        const RECORD_SIZE: usize = 12;
        let mut records = Vec::new();
        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0xE1 {
                continue;
            }
            if !event.payload.len().is_multiple_of(RECORD_SIZE) {
                return Err(FlpError::InvalidEvent {
                    offset: event.file_offset,
                    detail: "Mixer parameter payload is not a whole number of 12-byte records",
                });
            }
            for (record_index, bytes) in event
                .payload
                .as_chunks::<RECORD_SIZE>()
                .0
                .iter()
                .enumerate()
            {
                records.push(MixerParameterRecord {
                    event_index,
                    record_index,
                    prefix: bytes[..4]
                        .try_into()
                        .expect("the Mixer parameter record has four prefix bytes"),
                    parameter_id: bytes[4],
                    reserved: bytes[5],
                    channel_data: u16::from_le_bytes([bytes[6], bytes[7]]),
                    value: i32::from_le_bytes(
                        bytes[8..12]
                            .try_into()
                            .expect("the Mixer parameter record has a four-byte value"),
                    ),
                });
            }
        }
        Ok(records)
    }

    /// Changes the signed value in one existing 12-byte Mixer parameter record.
    /// The record prefix, ID, reserved byte, target word, and all other records remain intact.
    pub fn set_mixer_parameter_record_value(
        &mut self,
        event_index: usize,
        record_index: usize,
        value: i32,
    ) -> Result<(), FlpError> {
        const RECORD_SIZE: usize = 12;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer parameter event does not exist",
            ))?;
        if event.opcode != 0xE1 {
            return Err(FlpError::UnsupportedEdit(
                "the requested event is not a Mixer parameter event",
            ));
        }
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Mixer parameter event is not a length-prefixed data event",
            ));
        }
        if !event.payload.len().is_multiple_of(RECORD_SIZE) {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "Mixer parameter payload is not a whole number of 12-byte records",
            });
        }
        let record_start = record_index
            .checked_mul(RECORD_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let value_start = record_start
            .checked_add(8)
            .ok_or(FlpError::LengthOverflow)?;
        let value_end = value_start.checked_add(4).ok_or(FlpError::LengthOverflow)?;
        if value_end > event.payload.len() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Mixer parameter record does not exist",
            ));
        }

        let mut payload = event.payload.clone();
        payload[value_start..value_end].copy_from_slice(&value.to_le_bytes());
        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Edits fields with established playlist record offsets while preserving every other byte.
    /// `clip_index` is zero-based in the selected arrangement's stored clip order.
    pub fn edit_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        edit: PlaylistClipEdit,
    ) -> Result<(), FlpError> {
        let arrangements = self.arrangements()?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested arrangement id is ambiguous",
            ));
        }
        let Some(clip) = arrangement.clips.get(clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the requested playlist clip does not exist",
            ));
        };
        if edit.start_offset.is_some_and(|value| !value.is_finite())
            || edit.end_offset.is_some_and(|value| !value.is_finite())
            || edit.scale.is_some_and(|value| !value.is_finite())
        {
            return Err(FlpError::UnsupportedEdit(
                "playlist clip offsets and scale must be finite numbers",
            ));
        }
        let record_start = clip
            .source_record_index
            .checked_mul(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let record_size = clip.record_size;

        if edit.scale.is_some() && record_size < 80 {
            return Err(FlpError::UnsupportedEdit(
                "this playlist clip record has no established scale field",
            ));
        }

        let mut writes = Vec::<(usize, Vec<u8>)>::new();
        if let Some(value) = edit.position_ticks {
            writes.push((record_start, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.item_index {
            writes.push((record_start + 6, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.length_ticks {
            writes.push((record_start + 8, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.raw_track_index {
            writes.push((record_start + 12, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.group {
            writes.push((record_start + 14, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.item_flags {
            writes.push((record_start + 18, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.start_offset {
            writes.push((record_start + 24, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.end_offset {
            writes.push((record_start + 28, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.scale {
            writes.push((record_start + 64, value.to_le_bytes().to_vec()));
        }
        for (payload_offset, bytes) in writes {
            write_event_payload_bytes(&mut self.events[event_index], payload_offset, &bytes)?;
        }
        Ok(())
    }

    /// Duplicates one clip's complete stored record into the same Playlist event.
    /// The duplicate is inserted immediately after the source record.
    pub fn duplicate_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        position_ticks: Option<u32>,
        raw_track_index: Option<u16>,
    ) -> Result<usize, FlpError> {
        let arrangements = self.arrangements()?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested arrangement id is ambiguous",
            ));
        }
        let Some(clip) = arrangement.clips.get(clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the requested playlist clip does not exist",
            ));
        };
        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the source Playlist event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the source Playlist clip is not in a length-prefixed data event",
            ));
        }
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the source Playlist record does not fit its event payload",
            ));
        }
        let mut duplicate = event.payload[record_start..record_end].to_vec();
        let position =
            position_ticks.unwrap_or_else(|| clip.position_ticks.saturating_add(clip.length_ticks));
        duplicate[..4].copy_from_slice(&position.to_le_bytes());
        if let Some(raw_track_index) = raw_track_index {
            duplicate[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        }
        let mut payload = event.payload.clone();
        payload.splice(record_end..record_end, duplicate);
        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(clip_index.saturating_add(1))
    }

    /// Edits selected fields of one existing note without changing the event's wire length.
    /// `note_index` is zero-based within the selected channel's notes in that pattern.
    pub fn edit_pattern_note(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_index: usize,
        edit: PatternNoteEdit,
    ) -> Result<(), FlpError> {
        let mut channel_note_index = 0usize;
        let mut event_index = 0usize;
        while event_index < self.events.len() {
            let marker = &self.events[event_index];
            if marker.opcode != 0x41 || marker.payload.len() != 2 {
                event_index += 1;
                continue;
            }
            let marker_id = u16::from_le_bytes([marker.payload[0], marker.payload[1]]);
            let notes_index = event_index + 1;
            let Some(notes_event) = self.events.get(notes_index) else {
                break;
            };
            if marker_id != pattern_id || !Self::is_pattern_note_event(notes_event) {
                event_index += 1;
                continue;
            }
            if !notes_event
                .payload
                .len()
                .is_multiple_of(FLP_NOTE_RECORD_SIZE)
            {
                return Err(FlpError::InvalidEvent {
                    offset: notes_event.file_offset,
                    detail: "pattern note payload is not a whole number of 24-byte records",
                });
            }

            for (record_index, record) in notes_event
                .payload
                .as_chunks::<FLP_NOTE_RECORD_SIZE>()
                .0
                .iter()
                .enumerate()
            {
                let mut note = PatternNote::decode(record);
                if note.channel_id != channel_id {
                    continue;
                }
                if channel_note_index != note_index {
                    channel_note_index += 1;
                    continue;
                }
                note.apply(edit);
                let byte_offset = record_index * FLP_NOTE_RECORD_SIZE;
                let event = &mut self.events[notes_index];
                let payload_end = byte_offset + FLP_NOTE_RECORD_SIZE;
                note.encode_into(&mut event.payload[byte_offset..payload_end]);
                let prefix_length = match &event.encoding {
                    PayloadEncoding::Data { length_prefix } => length_prefix.len(),
                    _ => {
                        return Err(FlpError::InvalidEvent {
                            offset: event.file_offset,
                            detail: "pattern note event does not have a data payload",
                        });
                    }
                };
                let wire_start = 1 + prefix_length + byte_offset;
                let wire_end = wire_start + FLP_NOTE_RECORD_SIZE;
                event.wire_bytes[wire_start..wire_end]
                    .copy_from_slice(&event.payload[byte_offset..payload_end]);
                return Ok(());
            }
            event_index += 2;
        }
        Err(FlpError::UnsupportedEdit(
            "the requested pattern, channel, or note index does not exist",
        ))
    }

    /// Moves selected channel notes toward the closest grid point with optional strength and swing.
    /// Only note positions change; all other score fields and bytes remain intact.
    pub fn quantize_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        grid_ticks: u32,
        strength: f64,
        swing: f64,
    ) -> Result<usize, FlpError> {
        self.quantize_pattern_notes_in_scope(
            pattern_id, channel_id, None, grid_ticks, strength, swing,
        )
    }

    /// Moves only the supplied channel-note indices toward the closest grid points.
    /// The indices are channel-local and all non-position fields remain unchanged.
    pub fn quantize_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        grid_ticks: u32,
        strength: f64,
        swing: f64,
    ) -> Result<usize, FlpError> {
        self.quantize_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            grid_ticks,
            strength,
            swing,
        )
    }

    fn quantize_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        grid_ticks: u32,
        strength: f64,
        swing: f64,
    ) -> Result<usize, FlpError> {
        if grid_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "quantize grid must be greater than zero ticks",
            ));
        }
        if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
            return Err(FlpError::UnsupportedEdit(
                "quantize strength must be between 0 and 1",
            ));
        }
        if !swing.is_finite() || !(0.0..=1.0).contains(&swing) {
            return Err(FlpError::UnsupportedEdit(
                "quantize swing must be between 0 and 1",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let grid = u64::from(grid_ticks);
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let edits = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| {
                selected_indices
                    .as_ref()
                    .is_none_or(|indices| indices.contains(note_index))
            })
            .filter_map(|(note_index, note)| {
                let position = u64::from(note.position);
                let grid_index = position.saturating_add(grid / 2) / grid;
                let straight_target = grid_index.saturating_mul(grid);
                let swing_offset = if grid_index % 2 == 1 {
                    (grid as f64 * 0.5 * swing).round() as u64
                } else {
                    0
                };
                let target = straight_target
                    .saturating_add(swing_offset)
                    .min(u64::from(u32::MAX)) as f64;
                let next_position = (position as f64 + (target - position as f64) * strength)
                    .round()
                    .clamp(0.0, f64::from(u32::MAX)) as u32;
                (next_position != note.position).then_some((note_index, next_position))
            })
            .collect::<Vec<_>>();
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, position) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    position: Some(*position),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Extends each onset group to the next distinct onset on the selected channel.
    /// Notes in the final onset group retain their original lengths.
    pub fn legato_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
    ) -> Result<usize, FlpError> {
        self.legato_pattern_notes_in_scope(pattern_id, channel_id, None)
    }

    /// Extends only the supplied channel-local note indices to their next distinct channel onset.
    pub fn legato_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
    ) -> Result<usize, FlpError> {
        self.legato_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices))
    }

    fn legato_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
    ) -> Result<usize, FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut onset_positions = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .map(|note| note.position)
            .collect::<Vec<_>>();
        onset_positions.sort_unstable();
        onset_positions.dedup();

        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let edits = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .filter_map(|(note_index, note)| {
                let next_onset =
                    onset_positions.partition_point(|position| *position <= note.position);
                let next_position = *onset_positions.get(next_onset)?;
                let next_length = next_position.saturating_sub(note.position);
                (next_length != note.length).then_some((note_index, next_length))
            })
            .collect::<Vec<_>>();
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, length) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    length: Some(*length),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Splits eligible channel notes into equal-duration segments without changing note properties.
    /// The remainder of a non-divisible length is assigned to the final segment.
    pub fn chop_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        divisions: u8,
    ) -> Result<usize, FlpError> {
        self.chop_pattern_notes_in_scope(pattern_id, channel_id, None, divisions)
    }

    /// Splits only the supplied channel-local note indices into equal-duration segments.
    pub fn chop_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        divisions: u8,
    ) -> Result<usize, FlpError> {
        self.chop_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices), divisions)
    }

    fn chop_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        divisions: u8,
    ) -> Result<usize, FlpError> {
        if !(2..=64).contains(&divisions) {
            return Err(FlpError::UnsupportedEdit(
                "note chop divisions must be between 2 and 64",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let divisions = u32::from(divisions);
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut first_lengths = Vec::new();
        let mut additions = Vec::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
        {
            let segment_length = note.length / divisions;
            if segment_length == 0 {
                continue;
            }
            let remainder = note.length % divisions;
            first_lengths.push((note_index, segment_length));
            for segment in 1..divisions {
                let offset = segment_length
                    .checked_mul(segment)
                    .ok_or(FlpError::LengthOverflow)?;
                let position = note
                    .position
                    .checked_add(offset)
                    .ok_or(FlpError::LengthOverflow)?;
                let mut chopped = note.clone();
                chopped.position = position;
                chopped.length = segment_length
                    + if segment == divisions - 1 {
                        remainder
                    } else {
                        0
                    };
                additions.push(chopped);
            }
        }
        if additions.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, length) in &first_lengths {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    length: Some(*length),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        updated.add_pattern_notes(pattern_id, &additions)?;
        *self = updated;
        Ok(additions.len())
    }

    /// Joins touching or overlapping notes when every stored property except position and length
    /// matches. The earliest record keeps its properties; later records in each joined group are
    /// removed.
    pub fn glue_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
    ) -> Result<usize, FlpError> {
        self.glue_pattern_notes_in_scope(pattern_id, channel_id, None)
    }

    /// Joins touching or overlapping notes only within the supplied channel-local selection.
    pub fn glue_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
    ) -> Result<usize, FlpError> {
        self.glue_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices))
    }

    fn glue_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
    ) -> Result<usize, FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut notes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .cloned()
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .collect::<Vec<_>>();
        notes.sort_by_key(|(index, note)| (note.position, *index));
        let properties_match = |left: &PatternNote, right: &PatternNote| {
            let mut left = left.clone();
            let mut right = right.clone();
            left.position = 0;
            left.length = 0;
            right.position = 0;
            right.length = 0;
            left == right
        };

        let mut updates = Vec::new();
        let mut removals = Vec::new();
        let Some((mut first_index, mut first_note)) = notes.first().cloned() else {
            return Ok(0);
        };
        let mut group_end = u64::from(first_note.position) + u64::from(first_note.length);
        for (note_index, note) in notes.into_iter().skip(1) {
            let note_start = u64::from(note.position);
            let note_end = note_start + u64::from(note.length);
            if properties_match(&first_note, &note) && note_start <= group_end {
                group_end = group_end.max(note_end);
                removals.push(note_index);
            } else {
                if group_end > u64::from(first_note.position) + u64::from(first_note.length) {
                    let merged_length = group_end - u64::from(first_note.position);
                    let merged_length =
                        u32::try_from(merged_length).map_err(|_| FlpError::LengthOverflow)?;
                    updates.push((first_index, merged_length));
                }
                first_index = note_index;
                first_note = note;
                group_end = note_end;
            }
        }
        if group_end > u64::from(first_note.position) + u64::from(first_note.length) {
            let merged_length = group_end - u64::from(first_note.position);
            let merged_length =
                u32::try_from(merged_length).map_err(|_| FlpError::LengthOverflow)?;
            updates.push((first_index, merged_length));
        }
        if removals.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, length) in &updates {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    length: Some(*length),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        removals.sort_unstable_by(|left, right| right.cmp(left));
        for note_index in &removals {
            updated.delete_pattern_note(pattern_id, channel_id, *note_index)?;
        }
        *self = updated;
        Ok(removals.len())
    }

    /// Mirrors one channel's note positions around the end of its latest note.
    /// Note lengths and every other score field remain unchanged.
    pub fn flip_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
    ) -> Result<usize, FlpError> {
        self.flip_pattern_notes_in_scope(pattern_id, channel_id, None)
    }

    /// Mirrors only the supplied selected notes around their own time-and-length bounds.
    pub fn flip_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
    ) -> Result<usize, FlpError> {
        self.flip_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices))
    }

    fn flip_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
    ) -> Result<usize, FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let notes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .collect::<Vec<_>>();
        let Some(extent) = notes
            .iter()
            .map(|(_, note)| u64::from(note.position) + u64::from(note.length))
            .max()
        else {
            return Ok(0);
        };
        let start = if note_indices.is_some() {
            notes
                .iter()
                .map(|(_, note)| note.position)
                .min()
                .unwrap_or(0)
        } else {
            0
        };
        if extent > u64::from(u32::MAX) {
            return Err(FlpError::LengthOverflow);
        }
        let extent = u32::try_from(extent).map_err(|_| FlpError::LengthOverflow)?;
        let edits = notes
            .iter()
            .filter_map(|(note_index, note)| {
                let end = note.position.checked_add(note.length)?;
                let position = start.checked_add(extent.checked_sub(end)?)?;
                (position != note.position).then_some((note_index, position))
            })
            .collect::<Vec<_>>();
        if edits.is_empty() {
            return Ok(0);
        }
        let mut updated = self.clone();
        for (note_index, position) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                **note_index,
                PatternNoteEdit {
                    position: Some(*position),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Staggers simultaneous notes by pitch order, spreading each chord across `spread_ticks`.
    /// Note lengths and every other score field remain unchanged.
    pub fn strum_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        spread_ticks: u32,
        descending: bool,
    ) -> Result<usize, FlpError> {
        self.strum_pattern_notes_in_scope(pattern_id, channel_id, None, spread_ticks, descending)
    }

    /// Staggers simultaneous notes only within the supplied channel-local selection.
    pub fn strum_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        spread_ticks: u32,
        descending: bool,
    ) -> Result<usize, FlpError> {
        self.strum_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            spread_ticks,
            descending,
        )
    }

    fn strum_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        spread_ticks: u32,
        descending: bool,
    ) -> Result<usize, FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut onset_groups = std::collections::BTreeMap::<u32, Vec<(usize, u16)>>::new();
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
        {
            if !note_index_in_scope(note_index, selected_indices.as_ref()) {
                continue;
            }
            onset_groups
                .entry(note.position)
                .or_default()
                .push((note_index, note.key));
        }

        let mut edits = Vec::new();
        for (onset, mut group) in onset_groups {
            if group.len() < 2 || spread_ticks == 0 {
                continue;
            }
            group.sort_by_key(|(note_index, key)| {
                (if descending { u16::MAX - *key } else { *key }, *note_index)
            });
            let steps = u64::try_from(group.len() - 1).map_err(|_| FlpError::LengthOverflow)?;
            for (rank, (note_index, _)) in group.into_iter().enumerate() {
                let rank = u64::try_from(rank).map_err(|_| FlpError::LengthOverflow)?;
                let offset = u64::from(spread_ticks) * rank / steps;
                let position = u64::from(onset) + offset;
                let position = u32::try_from(position).map_err(|_| FlpError::LengthOverflow)?;
                if position != onset {
                    edits.push((note_index, position));
                }
            }
        }
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, position) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    position: Some(*position),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Replaces simultaneous channel chords with a gated, repeating arpeggio sequence.
    pub fn arpeggiate_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: ArpeggioOptions,
    ) -> Result<usize, FlpError> {
        self.arpeggiate_pattern_notes_in_scope(pattern_id, channel_id, None, options)
    }

    /// Replaces only selected simultaneous notes with a gated arpeggio sequence.
    pub fn arpeggiate_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: ArpeggioOptions,
    ) -> Result<usize, FlpError> {
        self.arpeggiate_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices), options)
    }

    fn arpeggiate_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: ArpeggioOptions,
    ) -> Result<usize, FlpError> {
        let ArpeggioOptions {
            step_ticks,
            range_octaves,
            gate_percent,
            direction,
        } = options;
        if step_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "arpeggiator step must be greater than zero ticks",
            ));
        }
        if !(1..=4).contains(&range_octaves) {
            return Err(FlpError::UnsupportedEdit(
                "arpeggiator range must be between 1 and 4 octaves",
            ));
        }
        if !(1..=100).contains(&gate_percent) {
            return Err(FlpError::UnsupportedEdit(
                "arpeggiator gate must be between 1 and 100 percent",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut onset_groups = std::collections::BTreeMap::<u32, Vec<(usize, PatternNote)>>::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
        {
            if !note_index_in_scope(note_index, selected_indices.as_ref()) {
                continue;
            }
            onset_groups
                .entry(note.position)
                .or_default()
                .push((note_index, note.clone()));
        }

        let mut remove_indices = Vec::new();
        let mut additions = Vec::new();
        for (onset, group) in onset_groups {
            let mut chord = std::collections::BTreeMap::<u16, PatternNote>::new();
            for (_, note) in &group {
                chord.entry(note.key).or_insert_with(|| note.clone());
            }
            if chord.len() < 2 {
                continue;
            }
            let end = group
                .iter()
                .map(|(_, note)| u64::from(note.position) + u64::from(note.length))
                .max()
                .unwrap_or(u64::from(onset));
            let duration = end.saturating_sub(u64::from(onset));
            if duration == 0 {
                continue;
            }
            remove_indices.extend(group.iter().map(|(note_index, _)| *note_index));
            let step_count = duration.div_ceil(u64::from(step_ticks));
            if step_count > 1_000_000
                || additions.len().saturating_add(step_count as usize) > 1_000_000
            {
                return Err(FlpError::UnsupportedEdit(
                    "arpeggiator would create more than one million notes",
                ));
            }

            let mut pitches = Vec::with_capacity(chord.len() * usize::from(range_octaves));
            for octave in 0..range_octaves {
                let octave_offset = u16::from(octave)
                    .checked_mul(12)
                    .ok_or(FlpError::LengthOverflow)?;
                for (key, note) in &chord {
                    let key = key
                        .checked_add(octave_offset)
                        .ok_or(FlpError::LengthOverflow)?;
                    let mut pitch = note.clone();
                    pitch.key = key;
                    pitches.push(pitch);
                }
            }
            if direction == ArpeggioDirection::Down {
                pitches.reverse();
            }
            let mut order = (0..pitches.len()).collect::<Vec<_>>();
            if direction == ArpeggioDirection::UpDown && pitches.len() > 2 {
                order.extend((1..pitches.len() - 1).rev());
            }
            let note_length = (u64::from(step_ticks) * u64::from(gate_percent) / 100).max(1);
            for step_index in 0..step_count {
                let offset = step_index * u64::from(step_ticks);
                let position = u64::from(onset) + offset;
                let position = u32::try_from(position).map_err(|_| FlpError::LengthOverflow)?;
                let remaining = duration - offset;
                let length = u32::try_from(note_length.min(remaining))
                    .map_err(|_| FlpError::LengthOverflow)?;
                let pitch_index = order[(step_index as usize) % order.len()];
                let mut note = pitches[pitch_index].clone();
                note.position = position;
                note.length = length;
                additions.push(note);
            }
        }
        if additions.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        remove_indices.sort_unstable_by(|left, right| right.cmp(left));
        for note_index in remove_indices {
            updated.delete_pattern_note(pattern_id, channel_id, note_index)?;
        }
        updated.add_pattern_notes(pattern_id, &additions)?;
        *self = updated;
        Ok(additions.len())
    }

    /// Adds a short, same-pitch stroke before or after every note in a channel.
    /// The original notes are retained and the new strokes use the requested velocity.
    pub fn flam_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        stroke_ticks: u32,
        velocity: u8,
        before: bool,
    ) -> Result<usize, FlpError> {
        self.flam_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            None,
            stroke_ticks,
            velocity,
            before,
        )
    }

    /// Adds flam strokes only for the supplied selected channel-local note indices.
    pub fn flam_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        stroke_ticks: u32,
        velocity: u8,
        before: bool,
    ) -> Result<usize, FlpError> {
        self.flam_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            stroke_ticks,
            velocity,
            before,
        )
    }

    fn flam_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        stroke_ticks: u32,
        velocity: u8,
        before: bool,
    ) -> Result<usize, FlpError> {
        if stroke_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "flam stroke time must be greater than zero ticks",
            ));
        }
        if velocity > 127 {
            return Err(FlpError::UnsupportedEdit(
                "flam velocity must be between 0 and 127",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let strokes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .map(|(_, note)| {
                let mut stroke = note.clone();
                stroke.position = if before {
                    note.position.saturating_sub(stroke_ticks)
                } else {
                    note.position
                        .checked_add(stroke_ticks)
                        .ok_or(FlpError::LengthOverflow)?
                };
                stroke.length = stroke_ticks;
                stroke.velocity = velocity;
                Ok(stroke)
            })
            .collect::<Result<Vec<_>, FlpError>>()?;
        if strokes.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        updated.add_pattern_notes(pattern_id, &strokes)?;
        *self = updated;
        Ok(strokes.len())
    }

    /// Randomizes channel note velocity, pan, and pitch from a repeatable seed.
    /// Negative level amounts favor lower values; bipolar mode applies offsets in either direction.
    pub fn randomize_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: RandomizerOptions,
    ) -> Result<usize, FlpError> {
        self.randomize_pattern_notes_in_scope(pattern_id, channel_id, None, options)
    }

    /// Randomizes only selected notes, preserving the seeded behavior within that selection.
    pub fn randomize_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: RandomizerOptions,
    ) -> Result<usize, FlpError> {
        self.randomize_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices), options)
    }

    fn randomize_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: RandomizerOptions,
    ) -> Result<usize, FlpError> {
        let RandomizerOptions {
            seed,
            velocity_amount_percent,
            pan_amount_percent,
            pitch_range_semitones,
            bipolar,
            reset_levels,
        } = options;
        if !(-100..=100).contains(&velocity_amount_percent)
            || !(-100..=100).contains(&pan_amount_percent)
        {
            return Err(FlpError::UnsupportedEdit(
                "randomizer velocity and pan amounts must be between -100 and 100",
            ));
        }
        if pitch_range_semitones > 24 {
            return Err(FlpError::UnsupportedEdit(
                "randomizer pitch range must be between 0 and 24 semitones",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;

        let mut state = if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        };
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut edits = Vec::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
        {
            let velocity_base = if reset_levels {
                100
            } else {
                i32::from(note.velocity)
            };
            let pan_base = if reset_levels {
                64
            } else {
                i32::from(note.pan)
            };
            let velocity_range =
                (127 * i32::from(velocity_amount_percent.unsigned_abs()) + 50) / 100;
            let pan_range = (127 * i32::from(pan_amount_percent.unsigned_abs()) + 50) / 100;
            let velocity_offset = randomizer_offset(
                &mut state,
                velocity_range,
                velocity_amount_percent.is_negative(),
                bipolar,
            );
            let pan_offset = randomizer_offset(
                &mut state,
                pan_range,
                pan_amount_percent.is_negative(),
                bipolar,
            );
            let pitch_offset =
                randomizer_offset(&mut state, i32::from(pitch_range_semitones), false, bipolar);
            let velocity = if !reset_levels && velocity_amount_percent == 0 {
                note.velocity
            } else {
                (velocity_base + velocity_offset).clamp(0, 127) as u8
            };
            let pan = if !reset_levels && pan_amount_percent == 0 {
                note.pan
            } else {
                (pan_base + pan_offset).clamp(0, 127) as u8
            };
            let key = if pitch_range_semitones == 0 {
                note.key
            } else {
                (i32::from(note.key) + pitch_offset).clamp(0, i32::from(u16::MAX)) as u16
            };
            if velocity != note.velocity || pan != note.pan || key != note.key {
                edits.push((note_index, velocity, pan, key));
            }
        }
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, velocity, pan, key) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    velocity: Some(*velocity),
                    pan: Some(*pan),
                    key: Some(*key),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Adds deterministic timing and velocity variation to channel notes.
    pub fn humanize_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        seed: u64,
        timing_range_ticks: u32,
        velocity_variation_percent: u8,
    ) -> Result<usize, FlpError> {
        self.humanize_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            None,
            seed,
            timing_range_ticks,
            velocity_variation_percent,
        )
    }

    /// Adds timing and velocity variation only to selected notes.
    pub fn humanize_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        seed: u64,
        timing_range_ticks: u32,
        velocity_variation_percent: u8,
    ) -> Result<usize, FlpError> {
        self.humanize_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            seed,
            timing_range_ticks,
            velocity_variation_percent,
        )
    }

    fn humanize_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        seed: u64,
        timing_range_ticks: u32,
        velocity_variation_percent: u8,
    ) -> Result<usize, FlpError> {
        if velocity_variation_percent > 100 {
            return Err(FlpError::UnsupportedEdit(
                "humanize velocity variation must be between 0 and 100 percent",
            ));
        }
        if timing_range_ticks > i32::MAX as u32 {
            return Err(FlpError::UnsupportedEdit(
                "humanize timing range must fit a signed 32-bit tick offset",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut state = if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        };
        let velocity_range = (127 * i32::from(velocity_variation_percent) + 50) / 100;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut edits = Vec::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
        {
            let timing_offset = randomizer_offset(
                &mut state,
                i32::try_from(timing_range_ticks).map_err(|_| FlpError::LengthOverflow)?,
                false,
                true,
            );
            let position = (i64::from(note.position) + i64::from(timing_offset))
                .clamp(0, i64::from(u32::MAX)) as u32;
            let velocity_offset = randomizer_offset(&mut state, velocity_range, false, true);
            let velocity = if velocity_variation_percent == 0 {
                note.velocity
            } else {
                (i32::from(note.velocity) + velocity_offset).clamp(0, 127) as u8
            };
            if position != note.position || velocity != note.velocity {
                edits.push((note_index, position, velocity));
            }
        }
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, position, velocity) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    position: Some(*position),
                    velocity: Some(*velocity),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Folds channel note pitches by octaves into a key range, then clamps any pitch that cannot fit.
    pub fn limit_pattern_note_range(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        minimum_key: u16,
        maximum_key: u16,
    ) -> Result<usize, FlpError> {
        self.limit_pattern_note_range_in_scope(
            pattern_id,
            channel_id,
            None,
            minimum_key,
            maximum_key,
        )
    }

    /// Folds or clamps only selected note pitches into the requested key range.
    pub fn limit_pattern_note_selection_range(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        minimum_key: u16,
        maximum_key: u16,
    ) -> Result<usize, FlpError> {
        self.limit_pattern_note_range_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            minimum_key,
            maximum_key,
        )
    }

    fn limit_pattern_note_range_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        minimum_key: u16,
        maximum_key: u16,
    ) -> Result<usize, FlpError> {
        if minimum_key > maximum_key || maximum_key > 127 {
            return Err(FlpError::UnsupportedEdit(
                "note range must be ordered and remain within keys 0 through 127",
            ));
        }
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut edits = Vec::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
        {
            let mut key = i32::from(note.key);
            let minimum = i32::from(minimum_key);
            let maximum = i32::from(maximum_key);
            while key > maximum && key - 12 >= minimum {
                key -= 12;
            }
            while key < minimum && key + 12 <= maximum {
                key += 12;
            }
            if key < minimum || key > maximum {
                key = if (key - minimum).abs() <= (key - maximum).abs() {
                    minimum
                } else {
                    maximum
                };
            }
            let key = u16::try_from(key).map_err(|_| FlpError::LengthOverflow)?;
            if key != note.key {
                edits.push((note_index, key));
            }
        }
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, key) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    key: Some(*key),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        *self = updated;
        Ok(edits.len())
    }

    /// Splits channel notes that cross `position_ticks`, preserving all note properties.
    pub fn slice_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        position_ticks: u32,
    ) -> Result<usize, FlpError> {
        self.slice_pattern_notes_in_scope(pattern_id, channel_id, None, position_ticks)
    }

    /// Splits only selected channel-local notes that cross `position_ticks`.
    pub fn slice_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        position_ticks: u32,
    ) -> Result<usize, FlpError> {
        self.slice_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            position_ticks,
        )
    }

    fn slice_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        position_ticks: u32,
    ) -> Result<usize, FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let cut = u64::from(position_ticks);
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let mut edits = Vec::new();
        let mut right_segments = Vec::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
        {
            let start = u64::from(note.position);
            let end = start + u64::from(note.length);
            if cut <= start || cut >= end {
                continue;
            }
            let left_length = u32::try_from(cut - start).map_err(|_| FlpError::LengthOverflow)?;
            let right_length = u32::try_from(end - cut).map_err(|_| FlpError::LengthOverflow)?;
            edits.push((note_index, left_length));
            let mut right = note.clone();
            right.position = position_ticks;
            right.length = right_length;
            right_segments.push(right);
        }
        if right_segments.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, length) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    length: Some(*length),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        updated.add_pattern_notes(pattern_id, &right_segments)?;
        *self = updated;
        Ok(right_segments.len())
    }

    /// Appends notes to one pattern while retaining every unrelated event byte.
    /// Empty patterns use the note-event opcode observed in the project's other patterns.
    pub fn add_pattern_notes(
        &mut self,
        pattern_id: u16,
        notes: &[PatternNote],
    ) -> Result<(), FlpError> {
        if notes.is_empty() {
            return Ok(());
        }
        for note in notes {
            self.require_unique_channel(note.channel_id)?;
        }

        let mut target_marker = None;
        let mut target_note_event = None;
        let mut inferred_opcode = None;
        let mut conflicting_opcodes = false;
        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0x41 || event.payload.len() != 2 {
                continue;
            }
            let marker_id = u16::from_le_bytes([event.payload[0], event.payload[1]]);
            let notes_index = event_index + 1;
            let Some(notes_event) = self.events.get(notes_index) else {
                continue;
            };
            if Self::is_pattern_note_event(notes_event) {
                match inferred_opcode {
                    None => inferred_opcode = Some(notes_event.opcode),
                    Some(opcode) if opcode != notes_event.opcode => conflicting_opcodes = true,
                    _ => {}
                }
                if marker_id == pattern_id {
                    target_note_event = Some(notes_index);
                }
            }
            if marker_id == pattern_id {
                target_marker = Some(event_index);
            }
        }

        let records_length = notes
            .len()
            .checked_mul(FLP_NOTE_RECORD_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let mut records = Vec::with_capacity(records_length);
        for note in notes {
            let mut record = [0u8; FLP_NOTE_RECORD_SIZE];
            note.encode_into(&mut record);
            records.extend_from_slice(&record);
        }

        if let Some(event_index) = target_note_event {
            let mut payload = self.events[event_index].payload.clone();
            if !payload.len().is_multiple_of(FLP_NOTE_RECORD_SIZE) {
                return Err(FlpError::InvalidEvent {
                    offset: self.events[event_index].file_offset,
                    detail: "pattern note payload is not a whole number of 24-byte records",
                });
            }
            payload
                .len()
                .checked_add(records.len())
                .ok_or(FlpError::LengthOverflow)?;
            payload.extend_from_slice(&records);
            self.events[event_index].replace_data_payload(payload)?;
        } else {
            let marker_index = target_marker.ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
            if conflicting_opcodes || inferred_opcode.is_none() {
                return Err(FlpError::UnsupportedEdit(
                    "cannot infer the note-event encoding for this empty pattern",
                ));
            }
            self.events.insert(
                marker_index + 1,
                FlpEvent::new_data(inferred_opcode.expect("checked above"), records)?,
            );
        }
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Appends one note to a pattern while retaining every unrelated event byte.
    pub fn add_pattern_note(&mut self, pattern_id: u16, note: PatternNote) -> Result<(), FlpError> {
        self.add_pattern_notes(pattern_id, &[note])
    }

    /// Imports the note events from one Standard MIDI File track into an existing FLP
    /// pattern and channel. Note times are converted through the MIDI tempo map or
    /// SMPTE clock, then expressed against the existing FLP tempo.
    pub fn import_midi_track(
        &mut self,
        midi: &midi::MidiFile,
        track_index: usize,
        pattern_id: u16,
        channel_id: u16,
    ) -> Result<usize, FlpError> {
        self.require_unique_channel(channel_id)?;
        if !self
            .patterns()?
            .iter()
            .any(|pattern| pattern.id == pattern_id)
        {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ));
        }
        if self.header.ppq == 0 {
            return Err(FlpError::UnsupportedEdit(
                "MIDI import requires a non-zero project PPQ value",
            ));
        }
        let project_tempo_milli_bpm = self.metadata.tempo_milli_bpm.unwrap_or(140_000);
        if project_tempo_milli_bpm == 0 {
            return Err(FlpError::UnsupportedEdit(
                "MIDI import requires a positive project tempo",
            ));
        }
        let default_microseconds_per_quarter =
            60_000_000_000.0 / f64::from(project_tempo_milli_bpm);
        let track = midi
            .tracks()
            .get(track_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested MIDI track does not exist",
            ))?;
        midi.elapsed_microseconds_at_tick(0, track_index, default_microseconds_per_quarter)
            .map_err(|_| {
                FlpError::UnsupportedEdit(
                    "MIDI import requires a supported time division and valid tempo map",
                )
            })?;
        let midi_notes = track.notes();
        if midi_notes.is_empty() {
            return Ok(0);
        }

        let mut imported = Vec::with_capacity(midi_notes.len());
        for midi_note in &midi_notes {
            let start_microseconds = midi
                .elapsed_microseconds_at_tick(
                    midi_note.start_tick(),
                    track_index,
                    default_microseconds_per_quarter,
                )
                .map_err(|_| FlpError::UnsupportedEdit("could not convert MIDI note start time"))?;
            let position = scale_midi_microseconds_to_project_ticks(
                start_microseconds,
                project_tempo_milli_bpm,
                self.header.ppq,
            )?;

            let end_microseconds = if let Some(end_tick) = midi_note.end_tick() {
                midi.elapsed_microseconds_at_tick(
                    end_tick,
                    track_index,
                    default_microseconds_per_quarter,
                )
                .map_err(|_| FlpError::UnsupportedEdit("could not convert MIDI note end time"))?
            } else if track.end_tick() > midi_note.start_tick() {
                midi.elapsed_microseconds_at_tick(
                    track.end_tick(),
                    track_index,
                    default_microseconds_per_quarter,
                )
                .map_err(|_| FlpError::UnsupportedEdit("could not convert MIDI track end time"))?
            } else if let Some(source_ppq) = midi.ticks_per_quarter_note() {
                let fallback_end_tick = midi_note
                    .start_tick()
                    .checked_add(u64::from(source_ppq))
                    .ok_or(FlpError::LengthOverflow)?;
                midi.elapsed_microseconds_at_tick(
                    fallback_end_tick,
                    track_index,
                    default_microseconds_per_quarter,
                )
                .map_err(|_| {
                    FlpError::UnsupportedEdit("could not convert MIDI fallback note end time")
                })?
            } else {
                start_microseconds + default_microseconds_per_quarter
            };
            let end_position = scale_midi_microseconds_to_project_ticks(
                end_microseconds,
                project_tempo_milli_bpm,
                self.header.ppq,
            )?;
            let length = end_position.saturating_sub(position).max(1);
            imported.push(PatternNote {
                position,
                channel_id,
                length,
                key: u16::from(midi_note.key()),
                midi_channel: midi_note.channel(),
                velocity: midi_note.velocity(),
                ..PatternNote::default()
            });
        }

        let mut candidate = self.clone();
        candidate.add_pattern_notes(pattern_id, &imported)?;
        *self = candidate;
        Ok(imported.len())
    }

    /// Removes one channel-scoped note and rewrites only its containing score event.
    pub fn delete_pattern_note(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_index: usize,
    ) -> Result<(), FlpError> {
        self.require_unique_channel(channel_id)?;
        let mut channel_note_index = 0usize;
        let mut event_index = 0usize;
        while event_index < self.events.len() {
            let marker = &self.events[event_index];
            if marker.opcode != 0x41 || marker.payload.len() != 2 {
                event_index += 1;
                continue;
            }
            let marker_id = u16::from_le_bytes([marker.payload[0], marker.payload[1]]);
            let notes_index = event_index + 1;
            let Some(notes_event) = self.events.get(notes_index) else {
                break;
            };
            if marker_id != pattern_id || !Self::is_pattern_note_event(notes_event) {
                event_index += 1;
                continue;
            }
            let (records, remainder) = notes_event.payload.as_chunks::<FLP_NOTE_RECORD_SIZE>();
            if !remainder.is_empty() {
                return Err(FlpError::InvalidEvent {
                    offset: notes_event.file_offset,
                    detail: "pattern note payload is not a whole number of 24-byte records",
                });
            }

            let mut replacement = Vec::with_capacity(notes_event.payload.len());
            let mut removed = false;
            for record in records {
                let note = PatternNote::decode(record);
                if !removed && note.channel_id == channel_id {
                    if channel_note_index == note_index {
                        removed = true;
                        continue;
                    }
                    channel_note_index += 1;
                }
                replacement.extend_from_slice(record);
            }
            if removed {
                self.events[notes_index].replace_data_payload(replacement)?;
                self.refresh_event_offsets()?;
                return Ok(());
            }
            event_index += 2;
        }
        Err(FlpError::UnsupportedEdit(
            "the requested pattern, channel, or note index does not exist",
        ))
    }

    fn require_unique_channel(&self, channel_id: u16) -> Result<(), FlpError> {
        let mut matches = self
            .channels()
            .into_iter()
            .filter(|channel| channel.id() == channel_id);
        if matches.next().is_none() {
            return Err(FlpError::ChannelNotFound(channel_id));
        }
        if matches.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }
        Ok(())
    }

    pub fn project_version(&self) -> Option<&str> {
        self.project_version.as_deref()
    }

    pub fn metadata(&self) -> &ProjectMetadata {
        &self.metadata
    }

    /// Returns the project settings block when its event sequence is recognized.
    pub fn project_settings(&self) -> Option<ProjectSettings> {
        let anchor = find_project_settings_anchor(&self.events)?;
        let play_truncated_notes_in_clips = match anchor.checked_sub(1).and_then(|index| {
            self.events
                .get(index)
                .filter(|event| event.opcode == 0x64)
                .map(|event| (index, event))
        }) {
            Some((_, event)) if event.encoding == PayloadEncoding::Word => {
                if event.payload != [0, 0] {
                    return None;
                }
                false
            }
            Some(_) => return None,
            None => true,
        };
        let fast_declick = self.events.get(anchor + 2)?;
        if fast_declick.payload.len() != 1 || fast_declick.payload[0] > 1 {
            return None;
        }
        Some(ProjectSettings {
            play_truncated_notes_in_clips,
            fast_declick_for_cut_groups: fast_declick.payload[0] != 0,
        })
    }

    /// Updates the supported Project settings while preserving unrelated FLP events.
    pub fn set_project_settings(&mut self, edit: ProjectSettingsEdit) -> Result<(), FlpError> {
        if edit.play_truncated_notes_in_clips.is_none()
            && edit.fast_declick_for_cut_groups.is_none()
        {
            return Ok(());
        }
        let mut candidate = self.clone();
        let anchor =
            find_project_settings_anchor(&candidate.events).ok_or(FlpError::UnsupportedEdit(
                "the supported Project settings event block could not be identified",
            ))?;

        if let Some(enabled) = edit.fast_declick_for_cut_groups {
            candidate.events[anchor + 2].replace_byte_payload(u8::from(enabled))?;
        }

        if let Some(enabled) = edit.play_truncated_notes_in_clips {
            let existing_event = anchor.checked_sub(1).and_then(|index| {
                candidate
                    .events
                    .get(index)
                    .filter(|event| event.opcode == 0x64)
                    .map(|_| index)
            });
            if let Some(index) = existing_event {
                let event = &candidate.events[index];
                if event.encoding != PayloadEncoding::Word || event.payload != [0, 0] {
                    return Err(FlpError::UnsupportedEdit(
                        "the Play truncated notes event has an unrecognized payload",
                    ));
                }
            }
            match (enabled, existing_event) {
                (true, Some(index)) => {
                    candidate.events.remove(index);
                }
                (false, None) => candidate.events.insert(anchor, FlpEvent::new_word(0x64, 0)),
                _ => {}
            }
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Updates selected FL Studio Project Info fields while retaining each field's
    /// project-version string encoding, terminator convention, and trailing payload bytes.
    pub fn set_project_info(&mut self, edit: ProjectInfoEdit) -> Result<(), FlpError> {
        let mut candidate = self.clone();
        for (opcode, value) in [
            (0xC2, edit.title),
            (0xCF, edit.author),
            (0xC3, edit.comments),
            (0xCE, edit.genre),
            (0xC5, edit.web_link),
        ] {
            if let Some(value) = value {
                candidate.set_project_info_string(opcode, &value)?;
            }
        }
        candidate.metadata =
            read_project_metadata(&candidate.events, candidate.project_version.as_deref());
        *self = candidate;
        Ok(())
    }

    fn set_project_info_string(&mut self, opcode: u8, value: &str) -> Result<(), FlpError> {
        if !PROJECT_INFO_STRING_EVENTS.contains(&opcode) {
            return Err(FlpError::UnsupportedEdit(
                "the requested event is not a supported Project Info string",
            ));
        }
        if value.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "Project Info strings cannot contain a NUL character",
            ));
        }

        let channel_start = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        if let Some(event_index) = self.events[..channel_start]
            .iter()
            .rposition(|event| event.opcode == opcode)
        {
            let event = &self.events[event_index];
            if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                return Err(FlpError::UnsupportedEdit(
                    "the selected Project Info event is not a length-prefixed string",
                ));
            }
            let replacement = replace_project_string_payload(
                &event.payload,
                value,
                self.project_info_strings_use_utf16(),
            )?;
            self.events[event_index].replace_data_payload(replacement)?;
        } else {
            let rank = project_info_event_rank(opcode).expect("supported field has a rank");
            let insertion_index = self.events[..channel_start]
                .iter()
                .position(|event| {
                    project_info_event_rank(event.opcode)
                        .is_some_and(|other_rank| other_rank > rank)
                })
                .or_else(|| {
                    self.events[..channel_start]
                        .iter()
                        .rposition(|event| project_info_event_rank(event.opcode).is_some())
                        .map(|index| index + 1)
                })
                .unwrap_or(channel_start);
            let payload = encode_project_string(value, self.project_info_strings_use_utf16())?;
            self.events
                .insert(insertion_index, FlpEvent::new_data(opcode, payload)?);
        }
        self.refresh_event_offsets()?;
        Ok(())
    }

    fn project_info_strings_use_utf16(&self) -> bool {
        if self.project_version.is_some() {
            return !uses_legacy_string_encoding(self.project_version.as_deref());
        }
        let channel_start = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        self.events[..channel_start]
            .iter()
            .find(|event| PROJECT_INFO_STRING_EVENTS.contains(&event.opcode))
            .is_some_and(|event| project_string_is_utf16(&event.payload, None))
    }

    fn project_strings_use_utf16(&self) -> bool {
        if self.project_version.is_some() {
            return !uses_legacy_string_encoding(self.project_version.as_deref());
        }
        self.events
            .iter()
            .find(|event| {
                PROJECT_INFO_STRING_EVENTS.contains(&event.opcode)
                    || matches!(event.opcode, 0xC1 | 0xCB | 0xCD | 0xF1)
            })
            .is_some_and(|event| project_string_is_utf16(&event.payload, None))
    }

    /// Changes an existing four-byte `0x9C` tempo event without reserializing other events.
    pub fn set_tempo_milli_bpm(&mut self, milli_bpm: u32) -> Result<(), FlpError> {
        let event_index = self.events.iter().position(|event| {
            event.opcode == 0x9C
                && event.payload.len() == 4
                && event.encoding == PayloadEncoding::Dword
        });
        let Some(event_index) = event_index else {
            return Err(FlpError::UnsupportedEdit(
                "no recognized 0x9C tempo event exists in this file",
            ));
        };

        let encoded_value = milli_bpm.to_le_bytes();
        let event = &mut self.events[event_index];
        let Some(wire_payload) = event.wire_bytes.get_mut(1..5) else {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "0x9C event does not contain a four-byte payload",
            });
        };
        wire_payload.copy_from_slice(&encoded_value);
        event.payload.copy_from_slice(&encoded_value);
        self.metadata = read_project_metadata(&self.events, self.project_version.as_deref());
        Ok(())
    }

    /// Updates a channel's modern `0xDB` Levels event while preserving its other fields.
    ///
    /// Legacy projects that store these controls as byte or word events remain readable, but
    /// cannot be edited through this API because their control ranges are not equivalent.
    pub fn set_channel_levels(
        &mut self,
        channel_id: u16,
        volume: u32,
        pan: i32,
    ) -> Result<(), FlpError> {
        if volume > 12_800 || !(0..=12_800).contains(&pan) {
            return Err(FlpError::UnsupportedEdit(
                "channel volume and pan must be in the 0..=12800 range",
            ));
        }

        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }

        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xDB)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected channel has no editable 0xDB Levels event",
            ))?;
        let event = &mut self.events[event_index];
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) || event.payload.len() < 8 {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel's 0xDB Levels event is truncated or not length-prefixed",
            ));
        }
        let mut payload = event.payload.clone();
        payload[..4].copy_from_slice(&pan.to_le_bytes());
        payload[4..8].copy_from_slice(&volume.to_le_bytes());
        event.replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Replaces the referenced channel IDs for an existing Layer channel.
    ///
    /// The IDs are stored as repeated two-byte `0x5E` word events. All other
    /// channel events retain their original wire bytes. If the Layer has no
    /// current child references, new events are placed immediately after its
    /// recognized channel-kind event.
    pub fn set_layer_child_ids(
        &mut self,
        layer_channel_id: u16,
        child_channel_ids: &[u16],
    ) -> Result<(), FlpError> {
        self.require_unique_channel(layer_channel_id)?;
        let channel = self
            .channels()
            .into_iter()
            .find(|channel| channel.id() == layer_channel_id)
            .expect("the unique Layer channel was checked above");
        if channel.kind != Some(ChannelType::Layer.raw()) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel is not a Layer channel",
            ));
        }

        let event_range = channel.event_range();
        let child_event_indices = event_range
            .clone()
            .filter(|index| {
                let event = &self.events[*index];
                event.opcode == 0x5E
                    && event.payload.len() == 2
                    && event.encoding == PayloadEncoding::Word
            })
            .collect::<Vec<_>>();
        let mut insertion_index = child_event_indices
            .first()
            .copied()
            .or_else(|| {
                event_range
                    .clone()
                    .rfind(|index| {
                        let event = &self.events[*index];
                        event.opcode == 0x15
                            && event.payload.as_slice() == [ChannelType::Layer.raw()]
                    })
                    .map(|index| index + 1)
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the selected Layer channel has no recognized channel-kind event",
            ))?;

        for event_index in child_event_indices.into_iter().rev() {
            self.events.remove(event_index);
            if event_index < insertion_index {
                insertion_index -= 1;
            }
        }
        let replacement = child_channel_ids
            .iter()
            .copied()
            .map(|child_id| FlpEvent::new_word(0x5E, child_id))
            .collect::<Vec<_>>();
        self.events
            .splice(insertion_index..insertion_index, replacement);
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Edits the observed Layer Random and Crossfade bits in an existing `0x90` flags dword.
    /// Every other bit in the raw field is retained.
    pub fn set_layer_flags(
        &mut self,
        layer_channel_id: u16,
        random: Option<bool>,
        crossfade: Option<bool>,
    ) -> Result<(), FlpError> {
        if random.is_none() && crossfade.is_none() {
            return Err(FlpError::UnsupportedEdit(
                "at least one Layer flag must be provided",
            ));
        }
        self.require_unique_channel(layer_channel_id)?;
        let channel = self
            .channels()
            .into_iter()
            .find(|channel| channel.id() == layer_channel_id)
            .expect("the unique Layer channel was checked above");
        if channel.kind != Some(ChannelType::Layer.raw()) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel is not a Layer channel",
            ));
        }
        let flags_event_index = channel
            .event_range()
            .find(|index| {
                let event = &self.events[*index];
                event.opcode == 0x90
                    && event.payload.len() == 4
                    && event.encoding == PayloadEncoding::Dword
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the selected Layer channel has no recognized dword flags event",
            ))?;
        let mut flags = channel.layer_flags.unwrap_or_default();
        if let Some(enabled) = random {
            flags = (flags & !1) | u32::from(enabled);
        }
        if let Some(enabled) = crossfade {
            flags = (flags & !2) | (u32::from(enabled) << 1);
        }
        self.events[flags_event_index].replace_dword_payload(flags)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Edits one existing point in a type-5 automation channel and retains its
    /// opaque header, point tail, remaining points, and era-specific trailer.
    pub fn edit_automation_point(
        &mut self,
        channel_id: u16,
        point_index: usize,
        edit: AutomationPointEdit,
    ) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }
        if channel.kind != Some(5) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel is not a type-5 automation channel",
            ));
        }
        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xEA)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected automation channel has no existing 0xEA point blob",
            ))?;
        let points = decode_automation_points(&self.events[event_index])?;
        let Some(point) = points.get(point_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the requested automation point does not exist",
            ));
        };

        if let Some(value) = edit.value {
            validate_automation_point_value(value)?;
        }
        if let Some(tension) = edit.tension {
            validate_automation_point_tension(tension)?;
        }

        if let Some(position) = edit.position_beats {
            let previous = if point_index == 0 {
                0.0
            } else {
                points[point_index - 1].position_beats
            };
            let next = points
                .get(point_index + 1)
                .map(|next_point| next_point.position_beats);
            if !position.is_finite()
                || !previous.is_finite()
                || next.is_some_and(|next_position| !next_position.is_finite())
                || position < 0.0
                || position < previous
                || next.is_some_and(|next_position| position > next_position)
            {
                return Err(FlpError::UnsupportedEdit(
                    "automation point positions must remain finite, non-negative, and ordered",
                ));
            }
        }

        let mut payload = self.events[event_index].payload.clone();
        let point_offset = FLP_AUTOMATION_POINTS_OFFSET
            .checked_add(
                point_index
                    .checked_mul(FLP_AUTOMATION_POINT_SIZE)
                    .ok_or(FlpError::LengthOverflow)?,
            )
            .ok_or(FlpError::LengthOverflow)?;

        if let Some(position) = edit.position_beats
            && position != point.position_beats
        {
            let previous = if point_index == 0 {
                0.0
            } else {
                points[point_index - 1].position_beats
            };
            payload[point_offset..point_offset + 8]
                .copy_from_slice(&(position - previous).to_le_bytes());
            if let Some(next_point) = points.get(point_index + 1) {
                let next_offset = point_offset
                    .checked_add(FLP_AUTOMATION_POINT_SIZE)
                    .ok_or(FlpError::LengthOverflow)?;
                payload[next_offset..next_offset + 8]
                    .copy_from_slice(&(next_point.position_beats - position).to_le_bytes());
            }
        }
        if let Some(value) = edit.value {
            payload[point_offset + 8..point_offset + 16].copy_from_slice(&value.to_le_bytes());
        }
        if let Some(tension) = edit.tension {
            payload[point_offset + 16..point_offset + 20].copy_from_slice(&tension.to_le_bytes());
        }

        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Inserts a point into an existing type-5 channel automation blob.
    ///
    /// `point_index` is an insertion slot and may equal the current point count.
    /// The new point receives zeroed opaque tail bytes; existing point bytes and
    /// any era-specific payload trailer are retained. Channels without an
    /// existing `0xEA` blob are not created implicitly because their header
    /// format is not established.
    pub fn insert_automation_point(
        &mut self,
        channel_id: u16,
        point_index: usize,
        position_beats: f64,
        value: f64,
        tension: f32,
    ) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }
        if channel.kind != Some(5) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel is not a type-5 automation channel",
            ));
        }
        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xEA)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected automation channel has no existing 0xEA point blob",
            ))?;
        let points = decode_automation_points(&self.events[event_index])?;
        if point_index > points.len() {
            return Err(FlpError::UnsupportedEdit(
                "the requested automation insertion slot does not exist",
            ));
        }
        validate_automation_point_value(value)?;
        validate_automation_point_tension(tension)?;

        let previous_position = if point_index == 0 {
            0.0
        } else {
            points[point_index - 1].position_beats
        };
        let next_position = points
            .get(point_index)
            .map(|next_point| next_point.position_beats);
        if !position_beats.is_finite()
            || !previous_position.is_finite()
            || next_position.is_some_and(|position| !position.is_finite())
            || position_beats < previous_position
            || next_position.is_some_and(|position| position_beats > position)
        {
            return Err(FlpError::UnsupportedEdit(
                "automation point positions must remain finite, non-negative, and ordered",
            ));
        }

        let new_count = u32::try_from(
            points
                .len()
                .checked_add(1)
                .ok_or(FlpError::LengthOverflow)?,
        )
        .map_err(|_| FlpError::LengthOverflow)?;
        let original = self.events[event_index].payload.clone();
        let insertion_offset = FLP_AUTOMATION_POINTS_OFFSET
            .checked_add(
                point_index
                    .checked_mul(FLP_AUTOMATION_POINT_SIZE)
                    .ok_or(FlpError::LengthOverflow)?,
            )
            .ok_or(FlpError::LengthOverflow)?;
        let point_bytes_length = points
            .len()
            .checked_mul(FLP_AUTOMATION_POINT_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let old_points_end = FLP_AUTOMATION_POINTS_OFFSET
            .checked_add(point_bytes_length)
            .ok_or(FlpError::LengthOverflow)?;
        let capacity = original
            .len()
            .checked_add(FLP_AUTOMATION_POINT_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let mut payload = Vec::with_capacity(capacity);
        payload.extend_from_slice(&original[..FLP_AUTOMATION_POINTS_OFFSET]);
        payload[FLP_AUTOMATION_COUNT_OFFSET..FLP_AUTOMATION_POINTS_OFFSET]
            .copy_from_slice(&new_count.to_le_bytes());
        payload.extend_from_slice(&original[FLP_AUTOMATION_POINTS_OFFSET..insertion_offset]);
        let inserted_point_offset = payload.len();
        payload.extend_from_slice(&(position_beats - previous_position).to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
        payload.extend_from_slice(&tension.to_le_bytes());
        payload.extend_from_slice(&[0; 4]);
        payload.extend_from_slice(&original[insertion_offset..old_points_end]);
        if let Some(next_position) = next_position {
            let next_delta = next_position - position_beats;
            let next_delta_offset = inserted_point_offset
                .checked_add(FLP_AUTOMATION_POINT_SIZE)
                .ok_or(FlpError::LengthOverflow)?;
            let next_delta_end = next_delta_offset
                .checked_add(8)
                .ok_or(FlpError::LengthOverflow)?;
            payload[next_delta_offset..next_delta_end].copy_from_slice(&next_delta.to_le_bytes());
        }
        payload.extend_from_slice(&original[old_points_end..]);

        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Removes one point from an existing type-5 channel automation blob.
    /// The next point's delta is adjusted to retain its absolute position, and
    /// the original opaque header and era-specific trailer are preserved.
    pub fn delete_automation_point(
        &mut self,
        channel_id: u16,
        point_index: usize,
    ) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }
        if channel.kind != Some(5) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel is not a type-5 automation channel",
            ));
        }
        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xEA)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected automation channel has no existing 0xEA point blob",
            ))?;
        let points = decode_automation_points(&self.events[event_index])?;
        if point_index >= points.len() {
            return Err(FlpError::UnsupportedEdit(
                "the requested automation point does not exist",
            ));
        }

        let new_count = u32::try_from(points.len() - 1).map_err(|_| FlpError::LengthOverflow)?;
        let original = self.events[event_index].payload.clone();
        let point_bytes_length = points
            .len()
            .checked_mul(FLP_AUTOMATION_POINT_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let old_points_end = FLP_AUTOMATION_POINTS_OFFSET
            .checked_add(point_bytes_length)
            .ok_or(FlpError::LengthOverflow)?;
        let removed_offset = FLP_AUTOMATION_POINTS_OFFSET
            .checked_add(
                point_index
                    .checked_mul(FLP_AUTOMATION_POINT_SIZE)
                    .ok_or(FlpError::LengthOverflow)?,
            )
            .ok_or(FlpError::LengthOverflow)?;
        let removed_end = removed_offset
            .checked_add(FLP_AUTOMATION_POINT_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let capacity = original
            .len()
            .checked_sub(FLP_AUTOMATION_POINT_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let mut payload = Vec::with_capacity(capacity);
        payload.extend_from_slice(&original[..FLP_AUTOMATION_POINTS_OFFSET]);
        payload[FLP_AUTOMATION_COUNT_OFFSET..FLP_AUTOMATION_POINTS_OFFSET]
            .copy_from_slice(&new_count.to_le_bytes());
        payload.extend_from_slice(&original[FLP_AUTOMATION_POINTS_OFFSET..removed_offset]);
        payload.extend_from_slice(&original[removed_end..old_points_end]);
        if let Some(next_point) = points.get(point_index + 1) {
            let previous_position = if point_index == 0 {
                0.0
            } else {
                points[point_index - 1].position_beats
            };
            let next_delta = next_point.position_beats - previous_position;
            if !previous_position.is_finite() || !next_delta.is_finite() || next_delta < 0.0 {
                return Err(FlpError::UnsupportedEdit(
                    "automation point positions must remain finite, non-negative, and ordered",
                ));
            }
            let next_delta_offset = removed_offset;
            payload[next_delta_offset..next_delta_offset + 8]
                .copy_from_slice(&next_delta.to_le_bytes());
        }
        payload.extend_from_slice(&original[old_points_end..]);

        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Renames a channel through its verified UTF-16LE `0xCB` display-name event.
    /// Unedited event bytes are retained; the edited event is resized if needed.
    pub fn set_channel_name(&mut self, channel_id: u16, name: &str) -> Result<(), FlpError> {
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "channel names cannot contain an embedded NUL character",
            ));
        }

        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }

        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xCB)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected channel has no recognized 0xCB display-name event",
            ))?;
        let event = &self.events[event_index];
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel's 0xCB event is not a length-prefixed data event",
            ));
        }

        let old_payload = &event.payload;
        let suffix_start = old_payload
            .as_chunks::<2>()
            .0
            .iter()
            .position(|pair| u16::from_le_bytes([pair[0], pair[1]]) == 0)
            .map_or(old_payload.len(), |unit_index| (unit_index + 1) * 2);
        let mut replacement_payload = Vec::with_capacity(name.len().saturating_mul(2) + 2);
        for unit in name.encode_utf16() {
            replacement_payload.extend_from_slice(&unit.to_le_bytes());
        }
        replacement_payload.extend_from_slice(&[0, 0]);
        replacement_payload.extend_from_slice(&old_payload[suffix_start..]);

        self.events[event_index].replace_data_payload(replacement_payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    pub fn trailing_bytes(&self) -> &[u8] {
        &self.trailing_bytes
    }

    pub fn opcode_counts(&self) -> [usize; 256] {
        let mut counts = [0usize; 256];
        for event in &self.events {
            counts[event.opcode as usize] += 1;
        }
        counts
    }

    /// Rebuilds the two-chunk container while keeping each event's original wire bytes.
    pub fn encode_lossless(&self) -> Result<Vec<u8>, FlpError> {
        let header_content_length = MIN_HEADER_CONTENT_LENGTH
            .checked_add(self.header.extension.len())
            .ok_or(FlpError::LengthOverflow)?;
        let header_length_u32 =
            u32::try_from(header_content_length).map_err(|_| FlpError::LengthOverflow)?;

        let event_stream_length = self.events.iter().try_fold(0usize, |total, event| {
            total
                .checked_add(event.wire_bytes.len())
                .ok_or(FlpError::LengthOverflow)
        })?;
        let event_length_u32 =
            u32::try_from(event_stream_length).map_err(|_| FlpError::LengthOverflow)?;

        let output_capacity = 8usize
            .checked_add(header_content_length)
            .and_then(|size| size.checked_add(8))
            .and_then(|size| size.checked_add(event_stream_length))
            .and_then(|size| size.checked_add(self.trailing_bytes.len()))
            .ok_or(FlpError::LengthOverflow)?;
        let mut output = Vec::with_capacity(output_capacity);
        output.extend_from_slice(FLHD);
        output.extend_from_slice(&header_length_u32.to_le_bytes());
        output.extend_from_slice(&self.header.format.to_le_bytes());
        output.extend_from_slice(&self.header.legacy_channel_count.to_le_bytes());
        output.extend_from_slice(&self.header.ppq.to_le_bytes());
        output.extend_from_slice(&self.header.extension);
        output.extend_from_slice(FLDT);
        output.extend_from_slice(&event_length_u32.to_le_bytes());
        for event in &self.events {
            output.extend_from_slice(&event.wire_bytes);
        }
        output.extend_from_slice(&self.trailing_bytes);
        Ok(output)
    }

    fn refresh_event_offsets(&mut self) -> Result<(), FlpError> {
        let header_content_length = MIN_HEADER_CONTENT_LENGTH
            .checked_add(self.header.extension.len())
            .ok_or(FlpError::LengthOverflow)?;
        let mut offset = 8usize
            .checked_add(header_content_length)
            .and_then(|value| value.checked_add(8))
            .ok_or(FlpError::LengthOverflow)?;
        for event in &mut self.events {
            event.file_offset = offset;
            offset = offset
                .checked_add(event.wire_bytes.len())
                .ok_or(FlpError::LengthOverflow)?;
        }
        Ok(())
    }

    fn is_pattern_note_event(event: &FlpEvent) -> bool {
        matches!(event.opcode, 0xD0 | 0xE0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlpError {
    UnexpectedEof {
        offset: usize,
        context: &'static str,
    },
    BadMagic {
        offset: usize,
        expected: &'static str,
    },
    InvalidHeaderLength(u32),
    MalformedVarint {
        offset: usize,
    },
    LengthOverflow,
    InvalidEvent {
        offset: usize,
        detail: &'static str,
    },
    UnsupportedEdit(&'static str),
    ChannelNotFound(u16),
    AmbiguousChannelId(u16),
}

impl fmt::Display for FlpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { offset, context } => {
                write!(
                    formatter,
                    "unexpected end of file at byte {offset} while reading {context}"
                )
            }
            Self::BadMagic { offset, expected } => {
                write!(
                    formatter,
                    "invalid chunk marker at byte {offset}; expected {expected}"
                )
            }
            Self::InvalidHeaderLength(length) => {
                write!(
                    formatter,
                    "invalid FLhd content length {length}; expected at least 6"
                )
            }
            Self::MalformedVarint { offset } => {
                write!(
                    formatter,
                    "invalid or overlong event length at byte {offset}"
                )
            }
            Self::LengthOverflow => write!(formatter, "FLP length exceeds supported size"),
            Self::InvalidEvent { offset, detail } => {
                write!(formatter, "invalid event at byte {offset}: {detail}")
            }
            Self::UnsupportedEdit(detail) => write!(formatter, "cannot edit FLP field: {detail}"),
            Self::ChannelNotFound(channel_id) => {
                write!(
                    formatter,
                    "cannot edit FLP field: channel id {channel_id} was not found"
                )
            }
            Self::AmbiguousChannelId(channel_id) => write!(
                formatter,
                "cannot edit FLP field: channel id {channel_id} is not unique"
            ),
        }
    }
}

impl std::error::Error for FlpError {}

fn playlist_clip_record_size(
    project_version: Option<&str>,
    payload: &[u8],
    file_offset: usize,
) -> Result<usize, FlpError> {
    let version_size = project_version
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .map(|major| {
            if major >= 25 {
                80
            } else if major >= 21 {
                60
            } else {
                32
            }
        });

    if payload.is_empty() {
        return Ok(version_size.unwrap_or(80));
    }
    let candidates = FLP_PLAYLIST_RECORD_SIZES
        .into_iter()
        .filter(|size| payload.len().is_multiple_of(*size))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(FlpError::InvalidEvent {
            offset: file_offset,
            detail: "playlist clip payload is not divisible by a supported record size",
        });
    }
    if let [record_size] = candidates.as_slice() {
        return Ok(*record_size);
    }

    let mut ranked = candidates
        .iter()
        .map(|record_size| {
            let score = playlist_clip_layout_score(payload, *record_size);
            (*record_size, score)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.0.total_cmp(&left.1.0));
    if let Some((best, score)) = ranked.first()
        && let Some((_, next_score)) = ranked.get(1)
        && score.1 >= 0.5
        && score.0 - next_score.0 >= 1.0
    {
        return Ok(*best);
    }

    if let Some(expected) = version_size
        && candidates.contains(&expected)
    {
        return Ok(expected);
    }
    Err(FlpError::InvalidEvent {
        offset: file_offset,
        detail: "playlist clip record size is ambiguous from the payload and project version",
    })
}

fn playlist_clip_layout_score(payload: &[u8], record_size: usize) -> (f64, f64) {
    let records = payload.chunks_exact(record_size);
    let record_count = records.len();
    let mut headers = HashMap::<[u8; 4], usize>::new();
    let mut valid_tracks = 0usize;
    let mut nonzero_lengths = 0usize;
    for record in records {
        let header: [u8; 4] = record[20..24]
            .try_into()
            .expect("supported Playlist records include the common 32-byte prefix");
        *headers.entry(header).or_default() += 1;
        if u16::from_le_bytes([record[12], record[13]]) <= 499 {
            valid_tracks += 1;
        }
        if record[8..12].iter().any(|byte| *byte != 0) {
            nonzero_lengths += 1;
        }
    }
    if record_count == 0 {
        return (0.0, 0.0);
    }
    let header_ratio = headers.values().copied().max().unwrap_or(0) as f64 / record_count as f64;
    let track_ratio = valid_tracks as f64 / record_count as f64;
    let length_ratio = nonzero_lengths as f64 / record_count as f64;
    (
        header_ratio * 4.0 + track_ratio * 2.0 + length_ratio * 2.0,
        header_ratio,
    )
}

fn decode_automation_points(event: &FlpEvent) -> Result<Vec<AutomationPoint>, FlpError> {
    if event.payload.len() < FLP_AUTOMATION_POINTS_OFFSET {
        return Err(FlpError::InvalidEvent {
            offset: event.file_offset,
            detail: "automation point blob is shorter than its header and count",
        });
    }
    let count = usize::try_from(u32::from_le_bytes(
        event.payload[FLP_AUTOMATION_COUNT_OFFSET..FLP_AUTOMATION_POINTS_OFFSET]
            .try_into()
            .expect("the automation count field has four bytes"),
    ))
    .map_err(|_| FlpError::LengthOverflow)?;
    let points_length = count
        .checked_mul(FLP_AUTOMATION_POINT_SIZE)
        .ok_or(FlpError::LengthOverflow)?;
    let points_end = FLP_AUTOMATION_POINTS_OFFSET
        .checked_add(points_length)
        .ok_or(FlpError::LengthOverflow)?;
    if points_end > event.payload.len() {
        return Err(FlpError::InvalidEvent {
            offset: event.file_offset,
            detail: "automation point count extends beyond its payload",
        });
    }

    let mut position_beats = 0.0;
    let mut points = Vec::with_capacity(count);
    for index in 0..count {
        let offset = FLP_AUTOMATION_POINTS_OFFSET + index * FLP_AUTOMATION_POINT_SIZE;
        let position_delta = f64::from_le_bytes(
            event.payload[offset..offset + 8]
                .try_into()
                .expect("an automation point has an eight-byte position delta"),
        );
        let value = f64::from_le_bytes(
            event.payload[offset + 8..offset + 16]
                .try_into()
                .expect("an automation point has an eight-byte value"),
        );
        let tension = f32::from_le_bytes(
            event.payload[offset + 16..offset + 20]
                .try_into()
                .expect("an automation point has a four-byte tension"),
        );
        let trailing_bytes = event.payload[offset + 20..offset + 24]
            .try_into()
            .expect("an automation point has four trailing bytes");
        position_beats += position_delta;
        points.push(AutomationPoint {
            position_beats,
            value,
            tension,
            trailing_bytes,
        });
    }
    Ok(points)
}

fn validate_automation_point_value(value: f64) -> Result<(), FlpError> {
    if !value.is_finite() || !(-0.001..=1.001).contains(&value) {
        return Err(FlpError::UnsupportedEdit(
            "automation point values must be finite and between -0.001 and 1.001",
        ));
    }
    Ok(())
}

fn validate_automation_point_tension(tension: f32) -> Result<(), FlpError> {
    if !tension.is_finite() || !(-1.0..=1.0).contains(&tension) {
        return Err(FlpError::UnsupportedEdit(
            "automation point tension must be finite and between -1 and 1",
        ));
    }
    Ok(())
}

fn decode_playlist_clip(
    record: &[u8],
    source_event_index: usize,
    source_record_index: usize,
) -> PlaylistClip {
    debug_assert!(FLP_PLAYLIST_RECORD_SIZES.contains(&record.len()));
    let u16_at = |offset: usize| u16::from_le_bytes([record[offset], record[offset + 1]]);
    let u32_at = |offset: usize| {
        u32::from_le_bytes([
            record[offset],
            record[offset + 1],
            record[offset + 2],
            record[offset + 3],
        ])
    };
    let f32_at = |offset: usize| f32::from_le_bytes(record[offset..offset + 4].try_into().unwrap());
    let record_size = record.len();
    let clip_id = (record_size >= 60).then(|| u32_at(32));
    let reserved = match record_size {
        80 => record[36..64].to_vec(),
        60 => record[36..60].to_vec(),
        _ => Vec::new(),
    };
    let scale = (record_size >= 80).then(|| f64::from_le_bytes(record[64..72].try_into().unwrap()));
    let trailing_bytes = if record_size >= 80 {
        record[72..80].to_vec()
    } else {
        Vec::new()
    };

    PlaylistClip {
        position_ticks: u32_at(0),
        pattern_base: u16_at(4),
        item_index: u16_at(6),
        length_ticks: u32_at(8),
        raw_track_index: u16_at(12),
        track_index: 499u16.checked_sub(u16_at(12)),
        group: u16_at(14),
        unknown_word: u16_at(16),
        item_flags: u16_at(18),
        header_bytes: [record[20], record[21], record[22], record[23]],
        start_offset: f32_at(24),
        end_offset: f32_at(28),
        clip_id,
        reserved,
        scale,
        trailing_bytes,
        record_size,
        source_event_index,
        source_record_index,
    }
}

fn write_event_payload_bytes(
    event: &mut FlpEvent,
    payload_offset: usize,
    bytes: &[u8],
) -> Result<(), FlpError> {
    let payload_end = payload_offset
        .checked_add(bytes.len())
        .ok_or(FlpError::LengthOverflow)?;
    if event.payload.get(payload_offset..payload_end).is_none() {
        return Err(FlpError::InvalidEvent {
            offset: event.file_offset,
            detail: "edited playlist field exceeds its source event",
        });
    }
    let prefix_length = match &event.encoding {
        PayloadEncoding::Data { length_prefix } => length_prefix.len(),
        _ => {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "playlist clip event does not have a data payload",
            });
        }
    };
    let wire_start = 1usize
        .checked_add(prefix_length)
        .and_then(|value| value.checked_add(payload_offset))
        .ok_or(FlpError::LengthOverflow)?;
    let wire_end = wire_start
        .checked_add(bytes.len())
        .ok_or(FlpError::LengthOverflow)?;
    if event.wire_bytes.get(wire_start..wire_end).is_none() {
        return Err(FlpError::InvalidEvent {
            offset: event.file_offset,
            detail: "playlist clip wire bytes do not match the decoded payload",
        });
    }
    event.payload[payload_offset..payload_end].copy_from_slice(bytes);
    event.wire_bytes[wire_start..wire_end].copy_from_slice(bytes);
    Ok(())
}

fn require_magic(bytes: &[u8], offset: usize, expected: &'static [u8; 4]) -> Result<(), FlpError> {
    let end = offset.checked_add(4).ok_or(FlpError::LengthOverflow)?;
    let Some(actual) = bytes.get(offset..end) else {
        return Err(FlpError::UnexpectedEof {
            offset: bytes.len(),
            context: "chunk marker",
        });
    };
    if actual != expected {
        return Err(FlpError::BadMagic {
            offset,
            expected: if expected == FLHD { "FLhd" } else { "FLdt" },
        });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize, context: &'static str) -> Result<u16, FlpError> {
    let end = offset.checked_add(2).ok_or(FlpError::LengthOverflow)?;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(FlpError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, context: &'static str) -> Result<u32, FlpError> {
    let end = offset.checked_add(4).ok_or(FlpError::LengthOverflow)?;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(FlpError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn detect_project_version(stream: &[u8]) -> Option<String> {
    let mut cursor = 0usize;
    for _ in 0..64 {
        let (event, next_cursor) = parse_event(stream, cursor, 0, false).ok()?;
        if event.opcode == 0xC7 {
            if let Some(version) = ascii_version(&event.payload) {
                return Some(version);
            }
        } else if event.opcode == 0xC0
            && let Some(version) = utf16_banner_version(&event.payload)
        {
            return Some(version);
        }
        cursor = next_cursor;
        if cursor >= stream.len() {
            return None;
        }
    }
    None
}

fn ascii_version(payload: &[u8]) -> Option<String> {
    let text = payload
        .split(|byte| *byte == 0)
        .next()
        .and_then(|part| std::str::from_utf8(part).ok())?;
    let valid = text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && text.contains('.')
        && text
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit());
    valid.then(|| text.to_owned())
}

fn decode_utf16_z(payload: &[u8]) -> Option<String> {
    let mut units = Vec::with_capacity(payload.len() / 2);
    for pair in payload.as_chunks::<2>().0 {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16(&units).ok()
}

fn project_string_version(version: Option<&str>) -> Option<(u32, u32)> {
    version.and_then(|version| {
        let mut parts = version.split('.');
        Some((
            parts.next()?.parse::<u32>().ok()?,
            parts.next()?.parse::<u32>().ok()?,
        ))
    })
}

fn uses_legacy_string_encoding(version: Option<&str>) -> bool {
    project_string_version(version)
        .is_some_and(|(major, minor)| major < 11 || (major == 11 && minor < 5))
}

fn decode_project_string(payload: &[u8], version: Option<&str>) -> Option<String> {
    if project_string_is_utf16(payload, version) {
        decode_utf16_z(payload)
    } else {
        decode_windows_1252_z(payload)
    }
}

fn project_string_is_utf16(payload: &[u8], version: Option<&str>) -> bool {
    project_string_version(version)
        .map(|(major, minor)| major > 11 || (major == 11 && minor >= 5))
        .unwrap_or_else(|| {
            payload
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|pair| pair[1] == 0)
                .count()
                >= 2
        })
}

fn project_info_event_rank(opcode: u8) -> Option<usize> {
    PROJECT_INFO_STRING_EVENTS
        .iter()
        .position(|candidate| *candidate == opcode)
}

fn find_project_settings_anchor(events: &[FlpEvent]) -> Option<usize> {
    let mut matched = None;
    for index in 0..events.len().saturating_sub(5) {
        let block = &events[index..index + 6];
        let matches = block[0].opcode == 0x1D
            && block[0].encoding == PayloadEncoding::Byte
            && block[0].payload == [1]
            && block[1].opcode == 0x27
            && block[1].encoding == PayloadEncoding::Byte
            && block[1].payload == [1]
            && block[2].opcode == 0x28
            && block[2].encoding == PayloadEncoding::Byte
            && block[2].payload.len() == 1
            && block[2].payload[0] <= 1
            && block[3].opcode == 0x1F
            && block[3].encoding == PayloadEncoding::Byte
            && block[3].payload == [0]
            && block[4].opcode == 0x26
            && block[4].encoding == PayloadEncoding::Byte
            && block[4].payload == [1]
            && block[5].opcode == 0x67
            && block[5].encoding == PayloadEncoding::Word
            && block[5].payload == [0x12, 0];
        if matches {
            if matched.is_some() {
                return None;
            }
            matched = Some(index);
        }
    }
    matched
}

fn encode_project_string(value: &str, utf16: bool) -> Result<Vec<u8>, FlpError> {
    let mut payload = Vec::new();
    if utf16 {
        for unit in value.encode_utf16() {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
        payload.extend_from_slice(&[0, 0]);
    } else {
        for character in value.chars() {
            let byte = windows_1252_byte(character).ok_or(FlpError::UnsupportedEdit(
                "the Project Info text contains a character unavailable in the project's legacy encoding",
            ))?;
            payload.push(byte);
        }
        payload.push(0);
    }
    Ok(payload)
}

fn replace_project_string_payload(
    old_payload: &[u8],
    value: &str,
    utf16: bool,
) -> Result<Vec<u8>, FlpError> {
    let (suffix_start, had_terminator) = if utf16 {
        old_payload
            .as_chunks::<2>()
            .0
            .iter()
            .position(|pair| u16::from_le_bytes(*pair) == 0)
            .map_or((old_payload.len(), false), |unit_index| {
                ((unit_index + 1) * 2, true)
            })
    } else {
        old_payload
            .iter()
            .position(|byte| *byte == 0)
            .map_or((old_payload.len(), false), |byte_index| {
                (byte_index + 1, true)
            })
    };

    let mut payload = Vec::new();
    if utf16 {
        for unit in value.encode_utf16() {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
        if had_terminator {
            payload.extend_from_slice(&[0, 0]);
        }
    } else {
        for character in value.chars() {
            let byte = windows_1252_byte(character).ok_or(FlpError::UnsupportedEdit(
                "the Project Info text contains a character unavailable in the project's legacy encoding",
            ))?;
            payload.push(byte);
        }
        if had_terminator {
            payload.push(0);
        }
    }
    payload.extend_from_slice(&old_payload[suffix_start..]);
    Ok(payload)
}

fn decode_windows_1252_z(payload: &[u8]) -> Option<String> {
    let text = payload.split(|byte| *byte == 0).next()?;
    Some(text.iter().copied().map(windows_1252_char).collect())
}

fn windows_1252_char(byte: u8) -> char {
    const EXTENDED: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    if (0x80..=0x9F).contains(&byte) {
        EXTENDED[usize::from(byte - 0x80)]
    } else {
        char::from(byte)
    }
}

fn windows_1252_byte(character: char) -> Option<u8> {
    let scalar = u32::from(character);
    if scalar <= 0x7F || (0xA0..=0xFF).contains(&scalar) {
        return u8::try_from(scalar).ok();
    }
    (0x80..=0x9F).find(|byte| windows_1252_char(*byte) == character)
}

fn utf16_banner_version(payload: &[u8]) -> Option<String> {
    let mut units = Vec::with_capacity(payload.len() / 2);
    for pair in payload.as_chunks::<2>().0 {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let text = String::from_utf16(&units).ok()?;
    if !text.starts_with("FL Studio") {
        return None;
    }
    let start = text
        .char_indices()
        .find_map(|(index, ch)| ch.is_ascii_digit().then_some(index))?;
    let suffix = &text[start..];
    let version: String = suffix
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    (version.contains('.') && version.bytes().next().is_some_and(|b| b.is_ascii_digit()))
        .then_some(version)
}

fn read_project_metadata(events: &[FlpEvent], project_version: Option<&str>) -> ProjectMetadata {
    let mut modern_tempo = None;
    let mut legacy_coarse_tempo = None;
    let mut legacy_fine_tempo = 0u32;
    let mut numerator = None;
    let mut denominator = None;
    let mut build_number = None;
    let mut title = None;
    let mut author = None;
    let mut comments = None;
    let mut genre = None;
    let mut web_link = None;
    let channel_start = events
        .iter()
        .position(|event| event.opcode == 0x40)
        .unwrap_or(events.len());

    for (event_index, event) in events.iter().enumerate() {
        match event.opcode {
            0x11 if event.payload.len() == 1 => numerator = Some(event.payload[0]),
            0x12 if event.payload.len() == 1 => denominator = Some(event.payload[0]),
            0x42 if event.payload.len() == 2 => {
                legacy_coarse_tempo = Some(u32::from(u16::from_le_bytes([
                    event.payload[0],
                    event.payload[1],
                ])));
            }
            0x5D if event.payload.len() == 2 => {
                legacy_fine_tempo =
                    u32::from(u16::from_le_bytes([event.payload[0], event.payload[1]]));
            }
            0x9C if event.payload.len() == 4 => {
                modern_tempo = Some(u32::from_le_bytes([
                    event.payload[0],
                    event.payload[1],
                    event.payload[2],
                    event.payload[3],
                ]));
            }
            0x9F if event.payload.len() == 4 => {
                build_number = Some(u32::from_le_bytes([
                    event.payload[0],
                    event.payload[1],
                    event.payload[2],
                    event.payload[3],
                ]));
            }
            0xC2 if event_index < channel_start => {
                title = decode_project_string(&event.payload, project_version);
            }
            0xCF if event_index < channel_start => {
                author = decode_project_string(&event.payload, project_version);
            }
            0xC3 if event_index < channel_start => {
                comments = decode_project_string(&event.payload, project_version);
            }
            0xCE if event_index < channel_start => {
                genre = decode_project_string(&event.payload, project_version);
            }
            0xC5 if event_index < channel_start => {
                web_link = decode_project_string(&event.payload, project_version);
            }
            _ => {}
        }
    }

    let time_signature = numerator.zip(denominator);
    let tempo_milli_bpm = modern_tempo.or_else(|| {
        legacy_coarse_tempo.map(|coarse| {
            coarse
                .saturating_mul(1000)
                .saturating_add(legacy_fine_tempo)
        })
    });

    ProjectMetadata {
        tempo_milli_bpm,
        time_signature,
        build_number,
        title,
        author,
        comments,
        genre,
        web_link,
    }
}

fn parse_event(
    stream: &[u8],
    start: usize,
    absolute_start: usize,
    modern_ac_event: bool,
) -> Result<(FlpEvent, usize), FlpError> {
    let Some(&opcode) = stream.get(start) else {
        return Err(FlpError::UnexpectedEof {
            offset: absolute_start + start,
            context: "event opcode",
        });
    };

    let (encoding, payload_start, payload_length) = if opcode == 0xAC && modern_ac_event {
        (PayloadEncoding::FixedThreeBytes, start + 1, 3usize)
    } else if opcode <= 0x3F {
        (PayloadEncoding::Byte, start + 1, 1usize)
    } else if opcode <= 0x7F {
        (PayloadEncoding::Word, start + 1, 2usize)
    } else if opcode <= 0xBF {
        (PayloadEncoding::Dword, start + 1, 4usize)
    } else {
        let (length, prefix_end) = read_leb128(stream, start + 1, absolute_start)?;
        let prefix = stream[start + 1..prefix_end].to_vec();
        (
            PayloadEncoding::Data {
                length_prefix: prefix,
            },
            prefix_end,
            usize::try_from(length).map_err(|_| FlpError::LengthOverflow)?,
        )
    };

    let event_end = payload_start
        .checked_add(payload_length)
        .ok_or(FlpError::LengthOverflow)?;
    if event_end > stream.len() {
        return Err(FlpError::UnexpectedEof {
            offset: absolute_start + stream.len(),
            context: "event payload",
        });
    }

    Ok((
        FlpEvent {
            opcode,
            payload: stream[payload_start..event_end].to_vec(),
            encoding,
            wire_bytes: stream[start..event_end].to_vec(),
            file_offset: absolute_start + start,
        },
        event_end,
    ))
}

fn read_leb128(
    bytes: &[u8],
    start: usize,
    absolute_start: usize,
) -> Result<(u32, usize), FlpError> {
    let mut value = 0u32;
    let mut cursor = start;
    for shift in (0..35).step_by(7) {
        let Some(&byte) = bytes.get(cursor) else {
            return Err(FlpError::UnexpectedEof {
                offset: absolute_start + cursor,
                context: "event length",
            });
        };
        cursor += 1;
        if shift == 28 && byte & 0xF0 != 0 {
            return Err(FlpError::MalformedVarint {
                offset: absolute_start + start,
            });
        }
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
    }
    Err(FlpError::MalformedVarint {
        offset: absolute_start + start,
    })
}

fn encode_leb128(mut value: u32) -> Vec<u8> {
    let mut output = Vec::with_capacity(5);
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return output;
        }
    }
}

fn scale_midi_microseconds_to_project_ticks(
    microseconds: f64,
    project_tempo_milli_bpm: u32,
    project_ppq: u16,
) -> Result<u32, FlpError> {
    if !microseconds.is_finite() || microseconds < 0.0 || project_tempo_milli_bpm == 0 {
        return Err(FlpError::UnsupportedEdit(
            "MIDI note time or project tempo is invalid",
        ));
    }
    let ticks = microseconds * f64::from(project_tempo_milli_bpm) * f64::from(project_ppq)
        / 60_000_000_000.0;
    if !ticks.is_finite() || ticks < 0.0 {
        return Err(FlpError::LengthOverflow);
    }
    let rounded_ticks = ticks.round();
    if rounded_ticks > f64::from(u32::MAX) {
        return Err(FlpError::LengthOverflow);
    }
    Ok(rounded_ticks as u32)
}

fn parse_vst_plugin_state_metadata(payload: &[u8]) -> Option<VstPluginStateMetadata> {
    let marker = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
    if !matches!(marker, 8 | 10 | 12) {
        return None;
    }

    let mut metadata = VstPluginStateMetadata {
        format_marker: marker,
        ..VstPluginStateMetadata::default()
    };
    let mut cursor = 4usize;
    let mut field_count = 0usize;
    while cursor < payload.len() {
        let id_end = cursor.checked_add(4)?;
        let length_end = cursor.checked_add(12)?;
        let id = u32::from_le_bytes(payload.get(cursor..id_end)?.try_into().ok()?);
        let length = u64::from_le_bytes(payload.get(id_end..length_end)?.try_into().ok()?);
        let length = usize::try_from(length).ok()?;
        let data_end = length_end.checked_add(length)?;
        let data = payload.get(length_end..data_end)?;
        field_count += 1;

        match id {
            50 if metadata.plugin_info.is_none() => metadata.plugin_info = Some(data.to_vec()),
            51 if metadata.fourcc.is_none() => metadata.fourcc = decode_vst_text(data),
            52 if metadata.guid.is_none() => metadata.guid = Some(data.to_vec()),
            53 if metadata.state_data_range.is_none() => {
                metadata.state_data_range = Some(length_end..data_end)
            }
            54 if metadata.name.is_none() => metadata.name = decode_vst_text(data),
            55 if metadata.path.is_none() => metadata.path = decode_vst_text(data),
            56 if metadata.vendor.is_none() => metadata.vendor = decode_vst_text(data),
            _ => {}
        }
        cursor = data_end;
    }

    (field_count > 0).then_some(metadata)
}

fn decode_vst_text(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes)
        .ok()?
        .trim_end_matches('\0')
        .to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{
        FlpDocument, FlpError, MixerParameterKind, PatternNote, PayloadEncoding, ProjectInfoEdit,
        ProjectSettingsEdit, TimeMarkerEdit, midi::MidiChannelMapping, midi::MidiFile,
        parse_vst_plugin_state_metadata,
    };

    fn flp_fixture(event_stream: &[u8], header_extension: &[u8], trailing: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FLhd");
        bytes.extend_from_slice(&(6u32 + header_extension.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&96u16.to_le_bytes());
        bytes.extend_from_slice(header_extension);
        bytes.extend_from_slice(b"FLdt");
        bytes.extend_from_slice(&(event_stream.len() as u32).to_le_bytes());
        bytes.extend_from_slice(event_stream);
        bytes.extend_from_slice(trailing);
        bytes
    }

    fn append_data_event(event_stream: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
        event_stream.push(opcode);
        event_stream.extend_from_slice(&super::encode_leb128(payload.len() as u32));
        event_stream.extend_from_slice(payload);
    }

    fn utf16_project_string(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn append_project_info_string(event_stream: &mut Vec<u8>, opcode: u8, value: &str) {
        append_data_event(event_stream, opcode, &utf16_project_string(value));
    }

    fn append_project_settings_block(
        event_stream: &mut Vec<u8>,
        play_truncated: bool,
        fast_declick: bool,
    ) {
        if !play_truncated {
            event_stream.extend_from_slice(&[0x64, 0, 0]);
        }
        event_stream.extend_from_slice(&[
            0x1D,
            1,
            0x27,
            1,
            0x28,
            u8::from(fast_declick),
            0x1F,
            0,
            0x26,
            1,
            0x67,
            0x12,
            0,
        ]);
    }

    fn append_time_marker(
        event_stream: &mut Vec<u8>,
        raw_position: u32,
        numerator: u8,
        denominator: u8,
        name: &str,
    ) {
        event_stream.push(0x94);
        event_stream.extend_from_slice(&raw_position.to_le_bytes());
        event_stream.extend_from_slice(&[0x21, numerator, 0x22, denominator]);
        let mut name_payload = name
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        name_payload.extend_from_slice(&[0, 0]);
        append_data_event(event_stream, 0xCD, &name_payload);
    }

    fn append_vst_field(payload: &mut Vec<u8>, id: u32, data: &[u8]) {
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
        payload.extend_from_slice(data);
    }

    fn note_record(
        position: u32,
        channel_id: u16,
        length: u32,
        key: u16,
        velocity: u8,
    ) -> [u8; 24] {
        let note = PatternNote {
            position,
            channel_id,
            length,
            key,
            velocity,
            ..PatternNote::default()
        };
        let mut record = [0; 24];
        note.encode_into(&mut record);
        record
    }

    fn pattern_fixture(note_records: &[[u8; 24]], trailing_event: &[u8]) -> Vec<u8> {
        let mut event_stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0];
        event_stream.extend_from_slice(&super::encode_leb128((note_records.len() * 24) as u32));
        for record in note_records {
            event_stream.extend_from_slice(record);
        }
        event_stream.extend_from_slice(trailing_event);
        flp_fixture(&event_stream, &[0xA1], &[0xB2])
    }

    #[test]
    fn project_info_strings_decode_edit_and_roundtrip_losslessly() {
        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"26.0.0\0");
        append_project_info_string(&mut event_stream, 0xC2, "Old title");
        append_project_info_string(&mut event_stream, 0xCE, "Ambient");
        append_project_info_string(&mut event_stream, 0xCF, "Original author");
        event_stream.extend_from_slice(&[0xA7, 1, 2, 3, 4]);
        append_project_info_string(&mut event_stream, 0xC3, "Old comments");
        append_project_info_string(&mut event_stream, 0xC5, "https://old.example");
        event_stream.extend_from_slice(&[0x40, 7, 0, 0x15, 0, 0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[0xB1, 0xB2], &[0xD1, 0xD2]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        assert_eq!(document.metadata().title(), Some("Old title"));
        assert_eq!(document.metadata().author(), Some("Original author"));
        assert_eq!(document.metadata().comments(), Some("Old comments"));
        assert_eq!(document.metadata().genre(), Some("Ambient"));
        assert_eq!(document.metadata().web_link(), Some("https://old.example"));

        let unchanged_wire_events: Vec<_> = document
            .events()
            .iter()
            .filter(|event| !super::PROJECT_INFO_STRING_EVENTS.contains(&event.opcode()))
            .map(|event| event.wire_bytes().to_vec())
            .collect();
        document
            .set_project_info(ProjectInfoEdit {
                title: Some("New title".to_owned()),
                author: Some("Zoë".to_owned()),
                comments: Some("Line one\nLine two".to_owned()),
                genre: Some("Jazz".to_owned()),
                web_link: Some("https://new.example/project".to_owned()),
            })
            .expect("Project Info fields should be editable");

        assert_eq!(document.metadata().title(), Some("New title"));
        assert_eq!(document.metadata().author(), Some("Zoë"));
        assert_eq!(document.metadata().comments(), Some("Line one\nLine two"));
        assert_eq!(document.metadata().genre(), Some("Jazz"));
        assert_eq!(
            document.metadata().web_link(),
            Some("https://new.example/project")
        );
        assert_eq!(
            document
                .events()
                .iter()
                .filter(|event| !super::PROJECT_INFO_STRING_EVENTS.contains(&event.opcode()))
                .map(|event| event.wire_bytes().to_vec())
                .collect::<Vec<_>>(),
            unchanged_wire_events
        );

        let encoded = document
            .encode_lossless()
            .expect("edited project should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should reparse");
        assert_eq!(reparsed.metadata().title(), Some("New title"));
        assert_eq!(reparsed.metadata().author(), Some("Zoë"));
        assert_eq!(reparsed.metadata().comments(), Some("Line one\nLine two"));
        assert_eq!(reparsed.metadata().genre(), Some("Jazz"));
        assert_eq!(
            reparsed.metadata().web_link(),
            Some("https://new.example/project")
        );
        assert_eq!(reparsed.trailing_bytes(), &[0xD1, 0xD2]);
    }

    #[test]
    fn project_settings_decode_edit_and_roundtrip_losslessly() {
        let mut event_stream = vec![0xF2, 28];
        event_stream.extend_from_slice(&[0; 28]);
        append_project_settings_block(&mut event_stream, true, true);
        event_stream.extend_from_slice(&[0x62, 0, 0, 0x33, 1]);
        let original = flp_fixture(&event_stream, &[0xA1], &[0xD1, 0xD2]);
        let mut document = FlpDocument::parse(&original).expect("fixture should parse");
        assert_eq!(
            document.project_settings(),
            Some(super::ProjectSettings {
                play_truncated_notes_in_clips: true,
                fast_declick_for_cut_groups: true,
            })
        );

        let unrelated_events: Vec<_> = document
            .events()
            .iter()
            .filter(|event| !matches!(event.opcode(), 0x64 | 0x28))
            .map(|event| event.wire_bytes().to_vec())
            .collect();
        document
            .set_project_settings(ProjectSettingsEdit {
                play_truncated_notes_in_clips: Some(false),
                fast_declick_for_cut_groups: Some(false),
            })
            .expect("supported Project settings should be editable");
        assert_eq!(
            document.project_settings(),
            Some(super::ProjectSettings {
                play_truncated_notes_in_clips: false,
                fast_declick_for_cut_groups: false,
            })
        );
        assert_eq!(
            document
                .events()
                .iter()
                .filter(|event| !matches!(event.opcode(), 0x64 | 0x28))
                .map(|event| event.wire_bytes().to_vec())
                .collect::<Vec<_>>(),
            unrelated_events
        );
        assert!(document.events().iter().any(|event| event.opcode() == 0x64));

        document
            .set_project_settings(ProjectSettingsEdit {
                play_truncated_notes_in_clips: Some(true),
                fast_declick_for_cut_groups: Some(true),
            })
            .expect("settings should be re-enabled");
        let encoded = document.encode_lossless().expect("document should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should reparse");
        assert_eq!(
            reparsed.project_settings(),
            Some(super::ProjectSettings {
                play_truncated_notes_in_clips: true,
                fast_declick_for_cut_groups: true,
            })
        );
        assert_eq!(reparsed.trailing_bytes(), &[0xD1, 0xD2]);
    }

    #[test]
    fn project_settings_refuse_an_unrecognized_event_layout_without_mutation() {
        let original = flp_fixture(&[0x64, 0, 0, 0x40, 0, 0], &[], &[]);
        let mut document = FlpDocument::parse(&original).expect("fixture should parse");
        assert_eq!(document.project_settings(), None);
        assert!(
            document
                .set_project_settings(ProjectSettingsEdit {
                    fast_declick_for_cut_groups: Some(true),
                    ..ProjectSettingsEdit::default()
                })
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), original);
    }

    #[test]
    fn project_info_edits_insert_missing_events_and_keep_legacy_encoding() {
        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"10.9.0\0");
        append_data_event(&mut event_stream, 0xC2, b"old\0tail");
        event_stream.extend_from_slice(&[0x40, 3, 0, 0x15, 0, 0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("legacy fixture should parse");
        document
            .set_project_info(ProjectInfoEdit {
                title: Some("Café".to_owned()),
                author: Some("Zoë".to_owned()),
                comments: Some("First\nSecond".to_owned()),
                genre: Some("Jazz".to_owned()),
                web_link: Some("https://legacy.example".to_owned()),
            })
            .expect("legacy strings should use Windows-1252");

        assert_eq!(document.metadata().title(), Some("Café"));
        assert_eq!(document.metadata().author(), Some("Zoë"));
        assert_eq!(document.metadata().genre(), Some("Jazz"));
        assert_eq!(document.metadata().comments(), Some("First\nSecond"));
        assert_eq!(
            document.metadata().web_link(),
            Some("https://legacy.example")
        );
        assert_eq!(
            document
                .events()
                .iter()
                .find(|event| event.opcode() == 0xC2)
                .expect("title event should remain")
                .payload(),
            b"Caf\xE9\0tail"
        );
        let first_channel = document
            .events()
            .iter()
            .position(|event| event.opcode() == 0x40)
            .expect("channel marker should remain");
        assert!(
            document.events()[..first_channel]
                .iter()
                .any(|event| { event.opcode() == 0xCF && event.payload() == b"Zo\xEB\0" })
        );
        assert!(
            document.events()[..first_channel]
                .iter()
                .any(|event| event.opcode() == 0xCE)
        );
        assert!(
            document.events()[..first_channel]
                .iter()
                .any(|event| event.opcode() == 0xC3)
        );
        assert!(
            document.events()[..first_channel]
                .iter()
                .any(|event| event.opcode() == 0xC5)
        );

        let mut rejected = FlpDocument::parse(&input).expect("legacy fixture should parse");
        let original = rejected.clone();
        assert!(
            rejected
                .set_project_info(ProjectInfoEdit {
                    title: Some("🎹".to_owned()),
                    ..ProjectInfoEdit::default()
                })
                .is_err()
        );
        assert_eq!(rejected, original, "failed edits must be transactional");
    }

    fn channel_with_sample_path_fixture(kind: u8, sample_path: &str) -> Vec<u8> {
        let mut event_stream = vec![0x40, 7, 0, 0x15, kind, 0xC4];
        let payload = sample_path
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        event_stream.extend_from_slice(&super::encode_leb128(payload.len() as u32));
        event_stream.extend_from_slice(&payload);
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        flp_fixture(&event_stream, &[], &[])
    }

    fn channel_with_levels_fixture(id: u16, pan: i32, volume: u32, tail: &[u8]) -> Vec<u8> {
        let mut event_stream = vec![0x40];
        event_stream.extend_from_slice(&id.to_le_bytes());
        event_stream.extend_from_slice(&[0x15, 4, 0xDB]);
        let mut payload = Vec::from(pan.to_le_bytes());
        payload.extend_from_slice(&volume.to_le_bytes());
        payload.extend_from_slice(tail);
        event_stream.extend_from_slice(&super::encode_leb128(payload.len() as u32));
        event_stream.extend_from_slice(&payload);
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        flp_fixture(&event_stream, &[], &[])
    }

    fn automation_channel_fixture(points: &[(f64, f64, f32, [u8; 4])], trailer: &[u8]) -> Vec<u8> {
        let mut payload = vec![0xA7; 17];
        payload.extend_from_slice(&(points.len() as u32).to_le_bytes());
        let mut previous_position = 0.0;
        for (position, value, tension, tail) in points {
            payload.extend_from_slice(&(*position - previous_position).to_le_bytes());
            payload.extend_from_slice(&value.to_le_bytes());
            payload.extend_from_slice(&tension.to_le_bytes());
            payload.extend_from_slice(tail);
            previous_position = *position;
        }
        payload.extend_from_slice(trailer);

        let mut event_stream = vec![0x40, 9, 0, 0x15, 5];
        append_data_event(&mut event_stream, 0xEA, &payload);
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        flp_fixture(&event_stream, &[], &[])
    }

    fn layer_channel_fixture(flags: u32, children: &[u16]) -> Vec<u8> {
        let mut event_stream = vec![0x40, 0, 0, 0x15, 3, 0x90];
        event_stream.extend_from_slice(&flags.to_le_bytes());
        for child in children {
            event_stream.push(0x5E);
            event_stream.extend_from_slice(&child.to_le_bytes());
        }
        event_stream.extend_from_slice(&[0x40, 1, 0, 0x15, 2]);
        event_stream.extend_from_slice(&[0x40, 2, 0, 0x15, 4, 0x62, 0, 0]);
        flp_fixture(&event_stream, &[], &[])
    }

    #[test]
    fn decodes_time_markers_and_preserves_unknown_position_bits() {
        let mut event_stream = vec![0x63, 7, 0];
        append_time_marker(&mut event_stream, 0x0800_0000 | 1_536, 7, 8, "Signature");
        event_stream.extend_from_slice(&[0x21, 9, 0x22, 16]);
        append_time_marker(&mut event_stream, 0x0400_0900, 4, 4, "Verse");
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let document = FlpDocument::parse(&input).expect("fixture should parse");

        let arrangements = document.arrangements().expect("arrangements should decode");
        assert_eq!(arrangements.len(), 1);
        assert_eq!(arrangements[0].id, 7);
        assert_eq!(arrangements[0].time_markers.len(), 2);

        let signature = &arrangements[0].time_markers[0];
        assert_eq!(signature.raw_position(), 0x0800_0000 | 1_536);
        assert_eq!(signature.position_ticks(), 1_536);
        assert!(signature.is_signature());
        assert_eq!(signature.numerator(), Some(7));
        assert_eq!(signature.denominator(), Some(8));
        assert_eq!(signature.name(), Some("Signature"));

        let named_marker = &arrangements[0].time_markers[1];
        assert_eq!(named_marker.raw_position(), 0x0400_0900);
        assert_eq!(named_marker.position_ticks(), 0x0400_0900);
        assert!(!named_marker.is_signature());
        assert_eq!(named_marker.numerator(), Some(4));
        assert_eq!(named_marker.denominator(), Some(4));
        assert_eq!(named_marker.name(), Some("Verse"));
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn edits_time_marker_position_and_name_while_preserving_raw_flags() {
        let mut event_stream = vec![0x63, 7, 0];
        append_time_marker(&mut event_stream, 0x0800_0000 | 1_536, 7, 8, "Signature");
        append_time_marker(&mut event_stream, 0x8000_0900, 4, 4, "Verse");
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_events = document
            .events()
            .iter()
            .map(|event| event.wire_bytes().to_vec())
            .collect::<Vec<_>>();
        let position_event = document
            .events()
            .iter()
            .rposition(|event| event.opcode() == 0x94)
            .expect("second position event should exist");
        let name_event = document
            .events()
            .iter()
            .rposition(|event| event.opcode() == 0xCD)
            .expect("second name event should exist");

        document
            .edit_time_marker(
                7,
                1,
                TimeMarkerEdit {
                    position_ticks: Some(2_048),
                    name: Some("Chorus".to_owned()),
                    ..TimeMarkerEdit::default()
                },
            )
            .expect("marker should be editable");

        let markers = document.time_markers().expect("markers should decode");
        assert_eq!(markers[1].1.raw_position(), 0x8000_0800);
        assert_eq!(markers[1].1.position_ticks(), 2_048);
        assert!(!markers[1].1.is_signature());
        assert_eq!(markers[1].1.numerator(), Some(4));
        assert_eq!(markers[1].1.denominator(), Some(4));
        assert_eq!(markers[1].1.name(), Some("Chorus"));
        assert_eq!(document.events().len(), original_events.len());
        for (index, (before, after)) in original_events.iter().zip(document.events()).enumerate() {
            if index == position_event || index == name_event {
                continue;
            }
            assert_eq!(
                before,
                after.wire_bytes(),
                "event {index} should be unchanged"
            );
        }
        let encoded = document
            .encode_lossless()
            .expect("edited project should encode");
        FlpDocument::parse(&encoded).expect("edited project should parse again");
    }

    #[test]
    fn marker_edit_can_add_signature_fields_and_rejects_invalid_updates_atomically() {
        let mut event_stream = vec![0x63, 7, 0, 0x94];
        event_stream.extend_from_slice(&960u32.to_le_bytes());
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .edit_time_marker(
                7,
                0,
                TimeMarkerEdit {
                    is_signature: Some(true),
                    numerator: Some(3),
                    denominator: Some(8),
                    name: Some("Pickup".to_owned()),
                    ..TimeMarkerEdit::default()
                },
            )
            .expect("signature marker fields should be insertable");
        let marker = &document.time_markers().expect("marker should decode")[0].1;
        assert_eq!(marker.position_ticks(), 960);
        assert!(marker.is_signature());
        assert_eq!(marker.numerator(), Some(3));
        assert_eq!(marker.denominator(), Some(8));
        assert_eq!(marker.name(), Some("Pickup"));

        let before = document.encode_lossless().expect("project should encode");
        assert!(
            document
                .edit_time_marker(
                    7,
                    0,
                    TimeMarkerEdit {
                        position_ticks: Some(0x0800_0000),
                        ..TimeMarkerEdit::default()
                    },
                )
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), before);
    }

    #[test]
    fn attaches_leading_time_markers_to_the_first_arrangement() {
        let mut event_stream = Vec::new();
        append_time_marker(&mut event_stream, 960, 3, 4, "Pickup");
        event_stream.extend_from_slice(&[0x63, 11, 0, 0x62, 0, 0]);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("fixture should parse");

        let arrangements = document.arrangements().expect("arrangements should decode");
        assert_eq!(arrangements.len(), 1);
        assert_eq!(arrangements[0].id, 11);
        assert_eq!(arrangements[0].time_markers.len(), 1);
        assert_eq!(arrangements[0].time_markers[0].position_ticks(), 960);
        assert_eq!(arrangements[0].time_markers[0].name(), Some("Pickup"));
    }

    #[test]
    fn time_marker_inspection_does_not_depend_on_playlist_clip_layout() {
        let mut event_stream = vec![0x63, 2, 0];
        append_time_marker(&mut event_stream, 480, 4, 4, "Marker");
        append_data_event(&mut event_stream, 0xE9, &[0xAA]);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("fixture should parse");

        assert!(document.arrangements().is_err());
        let markers = document
            .time_markers()
            .expect("marker inspection should skip unsupported clip records");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].0, 2);
        assert_eq!(markers[0].1.position_ticks(), 480);
        assert_eq!(markers[0].1.name(), Some("Marker"));
    }

    #[test]
    fn playlist_record_size_uses_record_structure_when_project_version_is_stale() {
        let mut payload = Vec::new();
        for index in 0..4u32 {
            let mut record = [0xA0u8; 80];
            record[0..4].copy_from_slice(&(index * 960).to_le_bytes());
            record[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
            record[6..8].copy_from_slice(&29u16.to_le_bytes());
            record[8..12].copy_from_slice(&960u32.to_le_bytes());
            record[12..14].copy_from_slice(&471u16.to_le_bytes());
            record[14..16].copy_from_slice(&0u16.to_le_bytes());
            record[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
            record[24..28].copy_from_slice(&0.0f32.to_le_bytes());
            record[28..32].copy_from_slice(&1.0f32.to_le_bytes());
            payload.extend_from_slice(&record);
        }

        assert_eq!(
            super::playlist_clip_record_size(Some("24.2.99.4720"), &payload, 123).unwrap(),
            80,
            "the record fields should override a stale version-based size"
        );

        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"24.2.99.4720\0");
        event_stream.extend_from_slice(&[0x63, 0, 0]);
        append_data_event(&mut event_stream, 0xE9, &payload);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("the synthetic project should parse");
        let arrangements = document
            .arrangements()
            .expect("structure should select the 80-byte record layout");
        assert_eq!(arrangements[0].clips.len(), 4);
        assert!(
            arrangements[0]
                .clips
                .iter()
                .all(|clip| clip.record_size == 80)
        );
    }

    #[test]
    fn playlist_record_size_keeps_version_fallback_and_rejects_weak_ambiguity() {
        let ambiguous = [0u8; 320];
        assert_eq!(
            super::playlist_clip_record_size(Some("20.0.0"), &ambiguous, 0).unwrap(),
            32
        );
        assert!(super::playlist_clip_record_size(Some("24.2.0"), &ambiguous, 0).is_err());
        assert!(super::playlist_clip_record_size(None, &ambiguous, 0).is_err());
    }

    fn midi_fixture(track: &[u8], division: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MThd");
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&division.to_be_bytes());
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(track);
        bytes
    }

    #[test]
    fn decodes_observed_mixer_insert_fields_without_rewriting_source_events() {
        let mut event_stream = Vec::new();
        for (input, output, color, icon, name) in [
            (-1_i32, 0_i32, 0x001C_1F8Cu32, 75_i16, "KICK"),
            (-1_i32, -1_i32, 0x001D_2792u32, 76_i16, "TOMS"),
        ] {
            event_stream.push(0x9A);
            event_stream.extend_from_slice(&input.to_le_bytes());
            event_stream.push(0x93);
            event_stream.extend_from_slice(&output.to_le_bytes());
            event_stream.push(0x95);
            event_stream.extend_from_slice(&color.to_le_bytes());
            event_stream.push(0x5F);
            event_stream.extend_from_slice(&icon.to_le_bytes());
            let name = name
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            append_data_event(&mut event_stream, 0xCC, &name);
        }

        let original = flp_fixture(&event_stream, &[], &[]);
        let document = FlpDocument::parse(&original).expect("the fixture should parse");
        let inserts = document.mixer_inserts();

        assert_eq!(inserts.len(), 2);
        assert_eq!(inserts[0].ordinal(), 0);
        assert_eq!(inserts[0].name(), Some("KICK"));
        assert_eq!(inserts[0].input_raw(), -1);
        assert_eq!(inserts[0].output_raw(), 0);
        assert_eq!(inserts[0].color_raw(), 0x001C_1F8C);
        assert_eq!(inserts[0].icon_raw(), Some(75));
        assert_eq!(inserts[0].event_range(), 0..5);
        assert_eq!(inserts[1].name(), Some("TOMS"));
        assert_eq!(inserts[1].output_raw(), -1);
        assert_eq!(
            document
                .encode_lossless()
                .expect("lossless encoding should succeed"),
            original
        );
    }

    #[test]
    fn mixer_insert_rename_preserves_utf16_terminator_suffix_and_other_events() {
        let mut event_stream = vec![0x9A];
        event_stream.extend_from_slice(&(-1_i32).to_le_bytes());
        event_stream.push(0x93);
        event_stream.extend_from_slice(&0_i32.to_le_bytes());
        event_stream.push(0x95);
        event_stream.extend_from_slice(&0x001C_1F8Cu32.to_le_bytes());
        event_stream.push(0x5F);
        event_stream.extend_from_slice(&75_i16.to_le_bytes());
        let mut name_payload = "KICK"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        name_payload.extend_from_slice(&[0, 0, 0xAA, 0xBB]);
        append_data_event(&mut event_stream, 0xCC, &name_payload);
        append_data_event(&mut event_stream, 0xE8, &[0x55]);

        let original = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&original).expect("the fixture should parse");
        let original_events = document
            .events()
            .iter()
            .map(|event| event.wire_bytes().to_vec())
            .collect::<Vec<_>>();
        document
            .set_mixer_insert_name(0, "KICK 💥")
            .expect("UTF-16 names should support Unicode");

        let insert = &document.mixer_inserts()[0];
        assert_eq!(insert.name(), Some("KICK 💥"));
        let name_event_index = insert
            .event_range()
            .find(|index| document.events()[*index].opcode() == 0xCC)
            .expect("the insert should retain its name event");
        let mut expected_payload = "KICK 💥"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        expected_payload.extend_from_slice(&[0, 0, 0xAA, 0xBB]);
        assert_eq!(
            document.events()[name_event_index].payload(),
            expected_payload
        );
        for (index, event) in document.events().iter().enumerate() {
            if index != name_event_index {
                assert_eq!(event.wire_bytes(), original_events[index]);
            }
        }
        let encoded = document
            .encode_lossless()
            .expect("the renamed project should encode");
        assert_eq!(
            FlpDocument::parse(&encoded)
                .expect("the renamed project should parse")
                .mixer_inserts()[0]
                .name(),
            Some("KICK 💥")
        );
    }

    #[test]
    fn mixer_insert_rename_uses_legacy_encoding_and_rejects_unrepresentable_text() {
        let mut event_stream = vec![0x9A];
        event_stream.extend_from_slice(&(-1_i32).to_le_bytes());
        event_stream.push(0x93);
        event_stream.extend_from_slice(&0_i32.to_le_bytes());
        event_stream.push(0x95);
        event_stream.extend_from_slice(&0x001C_1F8Cu32.to_le_bytes());
        append_data_event(&mut event_stream, 0xCC, b"KICK\0");

        let mut document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("the fixture should parse");
        document.project_version = Some("10.0".to_owned());
        document
            .set_mixer_insert_name(0, "Café")
            .expect("representable legacy text should encode");
        assert_eq!(document.mixer_inserts()[0].name(), Some("Café"));
        let name_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xCC)
            .expect("the name event should remain");
        assert_eq!(name_event.payload(), b"Caf\xE9\0");

        let before = document.encode_lossless().expect("document should encode");
        assert!(matches!(
            document.set_mixer_insert_name(0, "Café 🥁"),
            Err(FlpError::UnsupportedEdit(_))
        ));
        assert_eq!(
            document.encode_lossless().expect("document should encode"),
            before,
            "failed renames should not partially mutate the name event"
        );
    }

    #[test]
    fn decodes_fixed_mixer_parameter_records_and_rejects_unmapped_record_sizes() {
        let mut payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 192, 0];
        payload.extend_from_slice(&0x0123_u16.to_le_bytes());
        payload.extend_from_slice(&12_800_i32.to_le_bytes());
        payload.extend_from_slice(&[0x11, 0x22, 0x33, 0x44, 193, 7]);
        payload.extend_from_slice(&0x4567_u16.to_le_bytes());
        payload.extend_from_slice(&(-6400_i32).to_le_bytes());
        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xE1, &payload);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("the fixed-size Mixer parameter event should parse");

        let records = document
            .mixer_parameter_records()
            .expect("12-byte records should decode");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].prefix(), [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(records[0].parameter_id(), 192);
        assert_eq!(records[0].kind(), MixerParameterKind::Volume);
        assert_eq!(records[0].reserved(), 0);
        assert_eq!(records[0].channel_data(), 0x0123);
        assert_eq!(records[0].target_index(), 4);
        assert_eq!(records[0].slot_index(), 0x23);
        assert_eq!(records[0].target_scope_raw(), 0);
        assert_eq!(records[0].value(), 12_800);
        assert_eq!(records[1].parameter_id(), 193);
        assert_eq!(records[1].kind(), MixerParameterKind::Pan);
        assert_eq!(records[1].reserved(), 7);
        assert_eq!(records[1].channel_data(), 0x4567);
        assert_eq!(records[1].target_index(), 21);
        assert_eq!(records[1].slot_index(), 0x27);
        assert_eq!(records[1].target_scope_raw(), 2);
        assert_eq!(records[1].value(), -6400);

        let mut malformed_stream = Vec::new();
        append_data_event(&mut malformed_stream, 0xE1, &[0xAA]);
        let malformed = FlpDocument::parse(&flp_fixture(&malformed_stream, &[], &[]))
            .expect("unknown parameter payloads remain parseable");
        assert!(matches!(
            malformed.mixer_parameter_records(),
            Err(FlpError::InvalidEvent { .. })
        ));
    }

    #[test]
    fn editing_mixer_parameter_value_preserves_record_and_event_bytes() {
        let mut payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 192, 0];
        payload.extend_from_slice(&0x1040_u16.to_le_bytes());
        payload.extend_from_slice(&12_800_i32.to_le_bytes());
        payload.extend_from_slice(&[0x11, 0x22, 0x33, 0x44, 193, 7]);
        payload.extend_from_slice(&0x1080_u16.to_le_bytes());
        payload.extend_from_slice(&(-6400_i32).to_le_bytes());
        let mut event_stream = Vec::new();
        event_stream.extend_from_slice(&[0xE1, 0x98, 0x00]);
        event_stream.extend_from_slice(&payload);
        append_data_event(&mut event_stream, 0xE8, &[0xA5, 0x5A]);

        let original = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&original).expect("the fixture should parse");
        let event_index = document
            .events()
            .iter()
            .position(|event| event.opcode() == 0xE1)
            .expect("the fixture should contain Mixer parameters");
        let other_event_index = event_index + 1;
        let other_event = document.events()[other_event_index].wire_bytes().to_vec();
        let mut expected_mixer_event = document.events()[event_index].wire_bytes().to_vec();
        expected_mixer_event[11..15].copy_from_slice(&10_000_i32.to_le_bytes());
        document
            .set_mixer_parameter_record_value(event_index, 0, 10_000)
            .expect("an existing parameter value should be editable");

        let records = document
            .mixer_parameter_records()
            .expect("the parameter records should remain valid");
        assert_eq!(records[0].value(), 10_000);
        assert_eq!(records[1].value(), -6_400);
        assert_eq!(records[0].prefix(), [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(records[0].channel_data(), 0x1040);
        assert_eq!(records[1].prefix(), [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(records[1].channel_data(), 0x1080);
        assert_eq!(
            document.events()[event_index].wire_bytes(),
            expected_mixer_event
        );
        assert_eq!(
            document.events()[event_index].encoding(),
            &PayloadEncoding::Data {
                length_prefix: vec![0x98, 0x00]
            }
        );
        assert_eq!(
            document.events()[other_event_index].wire_bytes(),
            other_event
        );
        let encoded = document
            .encode_lossless()
            .expect("the edited project should encode");
        let round_trip = FlpDocument::parse(&encoded).expect("the edited project should parse");
        assert_eq!(
            round_trip.mixer_parameter_records().unwrap()[0].value(),
            10_000
        );
    }

    #[test]
    fn mixer_parameter_value_edit_rejects_invalid_targets_without_mutation() {
        let mut event_stream = vec![0x01, 0x00];
        append_data_event(&mut event_stream, 0xE1, &[0; 12]);
        let original = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&original).expect("the fixture should parse");
        let before = document.encode_lossless().expect("document should encode");

        assert!(matches!(
            document.set_mixer_parameter_record_value(0, 0, 123),
            Err(FlpError::UnsupportedEdit(_))
        ));
        assert!(matches!(
            document.set_mixer_parameter_record_value(1, 1, 123),
            Err(FlpError::UnsupportedEdit(_))
        ));
        assert_eq!(
            document.encode_lossless().expect("document should encode"),
            before
        );
    }

    #[test]
    fn decodes_vst_identity_fields_and_class_uid_from_d5_envelope() {
        let mut payload = 12u32.to_le_bytes().to_vec();
        append_vst_field(&mut payload, 50, &[0; 16]);
        append_vst_field(
            &mut payload,
            52,
            &[
                0xDF, 0x55, 0x47, 0x32, 0xDF, 0x8F, 0x88, 0x47, 0xB4, 0xCE, 0x5B, 0x70, 0xA8, 0x03,
                0x7E, 0xC4,
            ],
        );
        append_vst_field(&mut payload, 54, b"ZENOLOGY");
        append_vst_field(
            &mut payload,
            55,
            b"/Library/Audio/Plug-Ins/VST3/Roland/ZENOLOGY.vst3",
        );
        append_vst_field(&mut payload, 56, b"Roland Cloud");
        append_vst_field(&mut payload, 53, &[1, 2, 3, 4]);
        append_vst_field(&mut payload, 999, &[5, 6]);

        let metadata = parse_vst_plugin_state_metadata(&payload)
            .expect("the VST envelope should be recognized");

        assert_eq!(metadata.format_marker(), 12);
        assert_eq!(metadata.name(), Some("ZENOLOGY"));
        assert_eq!(metadata.vendor(), Some("Roland Cloud"));
        assert_eq!(
            metadata.path(),
            Some("/Library/Audio/Plug-Ins/VST3/Roland/ZENOLOGY.vst3")
        );
        assert_eq!(metadata.state_bytes(), Some(4));
        assert_eq!(
            metadata.class_uid().as_deref(),
            Some("324755DF8FDF4788B4CE5B70A8037EC4")
        );
        let state = super::ChannelPluginState {
            channel_id: 0,
            plugin_identifier: None,
            display_name: None,
            wrapper_payload: None,
            data_payload: payload.clone(),
            data_event_index: 0,
            vst_metadata: Some(metadata),
        };
        assert_eq!(state.vst_state_bytes(), Some(&[1, 2, 3, 4][..]));
    }

    #[test]
    fn ignores_unsupported_or_truncated_vst_envelopes() {
        assert!(parse_vst_plugin_state_metadata(&42u32.to_le_bytes()).is_none());

        let mut truncated = 12u32.to_le_bytes().to_vec();
        truncated.extend_from_slice(&54u32.to_le_bytes());
        truncated.extend_from_slice(&8u64.to_le_bytes());
        truncated.push(b'X');
        assert!(parse_vst_plugin_state_metadata(&truncated).is_none());
    }

    #[test]
    fn decodes_sampler_and_audio_channel_sample_paths_losslessly() {
        let path = r"%FLStudioFactoryData%\Data\Patches\Sounds\voice.wav";
        for kind in [0, 4] {
            let input = channel_with_sample_path_fixture(kind, path);
            let document = FlpDocument::parse(&input).expect("fixture should parse");
            let channels = document.channels();

            assert_eq!(channels.len(), 1);
            assert_eq!(channels[0].sample_path(), Some(path));
            assert_eq!(document.encode_lossless().unwrap(), input);
        }
    }

    #[test]
    fn decodes_and_edits_channel_levels_without_touching_other_level_fields() {
        let tail = [0xA5; 16];
        let input = channel_with_levels_fixture(9, 3_200, 8_750, &tail);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let channel = &document.channels()[0];

        assert_eq!(channel.volume(), Some(8_750));
        assert_eq!(channel.pan(), Some(3_200));
        assert_eq!(document.encode_lossless().unwrap(), input);

        document
            .set_channel_levels(9, 12_000, 9_600)
            .expect("the modern levels event should be editable");

        let channel = &document.channels()[0];
        assert_eq!(channel.volume(), Some(12_000));
        assert_eq!(channel.pan(), Some(9_600));
        let levels = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xDB)
            .expect("levels event should remain");
        assert_eq!(&levels.payload()[..4], &9_600i32.to_le_bytes());
        assert_eq!(&levels.payload()[4..8], &12_000u32.to_le_bytes());
        assert_eq!(&levels.payload()[8..], &tail);
        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited file should parse");
        assert_eq!(reparsed.channels()[0].volume(), Some(12_000));
        assert_eq!(reparsed.channels()[0].pan(), Some(9_600));
    }

    #[test]
    fn prefers_modern_channel_levels_over_legacy_channel_controls() {
        let mut event_stream = vec![0x40, 3, 0, 0x02, 99, 0x48, 0x20, 0x03, 17, 0x49, 0x30, 0];
        event_stream.extend_from_slice(&[0x15, 4, 0xDB]);
        let mut payload = 6_400i32.to_le_bytes().to_vec();
        payload.extend_from_slice(&10_000u32.to_le_bytes());
        payload.extend_from_slice(&[0; 16]);
        event_stream.extend_from_slice(&super::encode_leb128(payload.len() as u32));
        event_stream.extend_from_slice(&payload);
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("fixture should parse");

        assert_eq!(document.channels()[0].volume(), Some(10_000));
        assert_eq!(document.channels()[0].pan(), Some(6_400));
    }

    #[test]
    fn channel_level_edit_rejects_out_of_range_values_and_legacy_only_channels() {
        let input = channel_with_levels_fixture(4, 6_400, 10_000, &[0; 16]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        assert!(document.set_channel_levels(4, 12_801, 6_400).is_err());
        assert!(document.set_channel_levels(4, 10_000, -1).is_err());

        let legacy = flp_fixture(&[0x40, 4, 0, 0x02, 100, 0x03, 64, 0x62, 0, 0], &[], &[]);
        let mut legacy = FlpDocument::parse(&legacy).expect("legacy fixture should parse");
        assert_eq!(legacy.channels()[0].volume(), Some(100));
        assert_eq!(legacy.channels()[0].pan(), Some(64));
        assert!(legacy.set_channel_levels(4, 10_000, 6_400).is_err());
    }

    #[test]
    fn does_not_treat_plugin_channel_factory_data_as_a_sample_path() {
        let input = channel_with_sample_path_fixture(2, r"%FLStudioFactoryData%\Data\sounds.wav");
        let document = FlpDocument::parse(&input).expect("fixture should parse");
        let channels = document.channels();

        assert_eq!(channels[0].sample_path(), None);
    }

    #[test]
    fn lossless_roundtrip_preserves_unknown_event_bytes_and_container_extensions() {
        let event_stream = [0xFF, 0x82, 0x00, 0xA1, 0xB2];
        let header_extension = [0xC3, 0xD4];
        let trailing = [0xE5, 0xF6];
        let input = flp_fixture(&event_stream, &header_extension, &trailing);

        let document = FlpDocument::parse(&input).expect("fixture should parse");

        assert_eq!(document.header().ppq(), 96);
        assert_eq!(document.header().extension(), &header_extension);
        assert_eq!(document.events().len(), 1);
        assert_eq!(document.events()[0].payload(), &[0xA1, 0xB2]);
        assert_eq!(
            document.events()[0].encoding(),
            &PayloadEncoding::Data {
                length_prefix: vec![0x82, 0x00]
            }
        );
        assert_eq!(document.trailing_bytes(), &trailing);
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn adding_notes_updates_the_length_prefix_and_preserves_other_events() {
        let original_note = note_record(0, 0, 48, 60, 100);
        let unknown_event = [0xFF, 0x02, 0xAA, 0xBB];
        let input = pattern_fixture(&[original_note], &unknown_event);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .add_pattern_note(
                7,
                PatternNote {
                    position: 120,
                    channel_id: 0,
                    length: 24,
                    key: 64,
                    velocity: 90,
                    ..PatternNote::default()
                },
            )
            .expect("the note should be inserted");

        let patterns = document.patterns().expect("patterns should decode");
        assert_eq!(patterns[0].notes.len(), 2);
        assert_eq!(patterns[0].notes[1].position, 120);
        let notes_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xD0)
            .expect("note event should remain");
        assert_eq!(
            notes_event.encoding(),
            &PayloadEncoding::Data {
                length_prefix: vec![0x30]
            }
        );
        assert_eq!(
            document.events().last().unwrap().wire_bytes(),
            &unknown_event
        );

        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited file should parse");
        assert_eq!(reparsed.patterns().unwrap()[0].notes.len(), 2);
        assert_eq!(reparsed.trailing_bytes(), &[0xB2]);
    }

    #[test]
    fn note_event_length_prefix_grows_to_two_bytes() {
        let original_note = note_record(0, 0, 48, 60, 100);
        let input = pattern_fixture(&[original_note], &[0xFF, 0]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        for index in 0..10 {
            document
                .add_pattern_note(
                    7,
                    PatternNote {
                        position: index * 24,
                        channel_id: 0,
                        length: 24,
                        key: 60,
                        velocity: 90,
                        ..PatternNote::default()
                    },
                )
                .expect("note should be appended");
        }
        let notes_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xD0)
            .expect("note event should remain");
        assert_eq!(
            notes_event.encoding(),
            &PayloadEncoding::Data {
                length_prefix: vec![0x88, 0x02]
            }
        );
        assert_eq!(document.patterns().unwrap()[0].notes.len(), 11);
    }

    #[test]
    fn adding_to_an_empty_pattern_uses_the_projects_observed_note_encoding() {
        let existing_note = note_record(0, 0, 48, 60, 100);
        let mut stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 0x18];
        stream.extend_from_slice(&existing_note);
        stream.extend_from_slice(&[0x41, 8, 0, 0xA4, 0, 0, 0, 0]);
        let input = flp_fixture(&stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .add_pattern_note(
                8,
                PatternNote {
                    position: 24,
                    channel_id: 0,
                    length: 24,
                    key: 65,
                    velocity: 90,
                    ..PatternNote::default()
                },
            )
            .expect("the empty pattern should use the observed D0 encoding");

        let patterns = document.patterns().unwrap();
        let empty_pattern = patterns.iter().find(|pattern| pattern.id == 8).unwrap();
        assert_eq!(empty_pattern.notes.len(), 1);
        assert_eq!(empty_pattern.notes[0].key, 65);
        let reparsed = FlpDocument::parse(&document.encode_lossless().unwrap()).unwrap();
        assert_eq!(reparsed.patterns().unwrap()[1].notes.len(), 1);
    }

    #[test]
    fn creating_an_empty_pattern_uses_the_observed_encoding_and_preserves_other_events() {
        let existing_note = note_record(0, 0, 48, 60, 100);
        let mut stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 0x18];
        stream.extend_from_slice(&existing_note);
        stream.extend_from_slice(&[0xA4, 0x80, 0x01, 0, 0]);
        stream.extend_from_slice(&[0x40, 1, 0, 0x15, 2]);
        stream.extend_from_slice(&[0x41, 7, 0, 0x96, 0, 0, 0, 0]);
        let input = flp_fixture(&stream, &[0xA1], &[0xB2]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_events: Vec<_> = document
            .events()
            .iter()
            .map(|event| event.wire_bytes().to_vec())
            .collect();

        let new_id = document
            .create_pattern()
            .expect("the existing pattern encoding should be reusable");

        assert_eq!(new_id, 8);
        let patterns = document.patterns().expect("patterns should decode");
        let created = patterns
            .iter()
            .find(|pattern| pattern.id == new_id)
            .expect("new pattern should be present");
        assert!(created.notes.is_empty());
        assert!(created.name.is_none());
        let new_marker = document
            .events()
            .iter()
            .position(|event| event.opcode() == 0x41 && event.payload() == new_id.to_le_bytes())
            .expect("new pattern marker should be present");
        assert_eq!(document.events()[new_marker + 1].opcode(), 0xD0);
        assert!(document.events()[new_marker + 1].payload().is_empty());

        let current_events: Vec<_> = document
            .events()
            .iter()
            .filter(|event| {
                !(event.opcode() == 0x41 && event.payload() == new_id.to_le_bytes())
                    && !(event.opcode() == 0xD0 && event.payload().is_empty())
            })
            .map(|event| event.wire_bytes().to_vec())
            .collect();
        assert_eq!(current_events, original_events);

        let encoded = document.encode_lossless().expect("project should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("created project should reparse");
        assert!(
            reparsed
                .patterns()
                .unwrap()
                .iter()
                .any(|pattern| pattern.id == new_id)
        );
        assert_eq!(reparsed.trailing_bytes(), &[0xB2]);
    }

    #[test]
    fn creating_a_pattern_refuses_ambiguous_note_event_encodings_without_mutation() {
        let note = note_record(0, 0, 48, 60, 100);
        let mut stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 0x18];
        stream.extend_from_slice(&note);
        stream.extend_from_slice(&[0x41, 8, 0, 0xE0, 0x18]);
        stream.extend_from_slice(&note);
        let input = flp_fixture(&stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        assert!(document.create_pattern().is_err());
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn ambiguous_empty_pattern_note_encoding_is_rejected_without_mutation() {
        let note = note_record(0, 0, 48, 60, 100);
        let mut stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 0x18];
        stream.extend_from_slice(&note);
        stream.extend_from_slice(&[0x41, 8, 0, 0xE0, 0x18]);
        stream.extend_from_slice(&note);
        stream.extend_from_slice(&[0x41, 9, 0, 0xA4, 0, 0, 0, 0]);
        let input = flp_fixture(&stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let error = document
            .add_pattern_note(
                9,
                PatternNote {
                    channel_id: 0,
                    length: 24,
                    key: 65,
                    velocity: 90,
                    ..PatternNote::default()
                },
            )
            .expect_err("conflicting project encodings must not be guessed");

        assert!(matches!(error, FlpError::UnsupportedEdit(_)));
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn deleting_a_note_preserves_the_remaining_records_and_can_be_reversed() {
        let first = note_record(0, 0, 48, 60, 100);
        let second = note_record(96, 0, 24, 64, 80);
        let input = pattern_fixture(&[first, second], &[0xFF, 0]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .delete_pattern_note(7, 0, 0)
            .expect("the first note should be removed");
        let remaining = document.patterns().unwrap();
        assert_eq!(remaining[0].notes.len(), 1);
        assert_eq!(remaining[0].notes[0].position, 96);
        assert_eq!(remaining[0].notes[0].key, 64);

        document
            .add_pattern_note(
                7,
                PatternNote {
                    position: 0,
                    channel_id: 0,
                    length: 48,
                    key: 60,
                    velocity: 100,
                    ..PatternNote::default()
                },
            )
            .expect("an empty note event should retain its encoding");
        let restored = document.patterns().unwrap();
        assert_eq!(restored[0].notes.len(), 2);
        assert_eq!(restored[0].notes[1].position, 0);
    }

    #[test]
    fn imports_midi_track_notes_scaled_to_project_ppq_and_preserves_other_events() {
        let original_note = note_record(0, 0, 48, 48, 80);
        let unknown_event = [0xFF, 0x02, 0xAA, 0xBB];
        let project_bytes = pattern_fixture(&[original_note], &unknown_event);
        let midi_bytes = midi_fixture(
            &[
                0x00, 0x99, 0x3C, 0x64, // Channel 9, key 60, velocity 100 at tick 0.
                0x83, 0x60, 0x89, 0x3C, 0x20, // Note off at tick 480.
                0x00, 0x90, 0x43, 0x50, // Channel 0, key 67, velocity 80 at tick 480.
                0x81, 0x70, 0x80, 0x43, 0x00, // Note off at tick 720.
                0x00, 0xFF, 0x2F, 0x00,
            ],
            480,
        );
        let midi = MidiFile::parse(&midi_bytes).expect("MIDI fixture should parse");
        let mut document = FlpDocument::parse(&project_bytes).expect("project should parse");

        assert_eq!(
            document
                .import_midi_track(&midi, 0, 7, 0)
                .expect("the track notes should import"),
            2
        );

        let notes = &document.patterns().unwrap()[0].notes;
        assert_eq!(notes.len(), 3);
        assert_eq!(
            (notes[1].position, notes[1].length, notes[1].key),
            (0, 96, 60)
        );
        assert_eq!(notes[1].velocity, 100);
        assert_eq!(notes[1].midi_channel, 9);
        assert_eq!(
            (notes[2].position, notes[2].length, notes[2].key),
            (96, 48, 67)
        );
        assert_eq!(notes[2].velocity, 80);
        assert_eq!(notes[2].midi_channel, 0);
        assert_eq!(
            document.events().last().unwrap().wire_bytes(),
            &unknown_event
        );

        let reparsed = FlpDocument::parse(&document.encode_lossless().unwrap()).unwrap();
        assert_eq!(reparsed.patterns().unwrap()[0].notes.len(), 3);
    }

    #[test]
    fn exports_pattern_notes_with_project_ppq_and_stored_midi_channels() {
        let mut note = note_record(96, 3, 48, 60, 96);
        note[19] = 9;
        let document = FlpDocument::parse(&pattern_fixture(&[note], &[]))
            .expect("project fixture should parse");
        let source_tempo = document.metadata().tempo_bpm().unwrap_or(140.0);

        let bytes = MidiFile::encode_project_pattern(
            &document,
            7,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("pattern should export");
        let midi = MidiFile::parse(&bytes).expect("exported file should parse");
        let track = midi
            .tracks()
            .iter()
            .find(|track| !track.notes().is_empty())
            .expect("export should contain the pattern note track");
        let exported_note = track.notes().remove(0);

        assert_eq!(midi.division(), 96);
        assert!(
            (midi.tracks()[0]
                .tempo_bpm()
                .expect("tempo event should export")
                - source_tempo)
                .abs()
                < 0.001
        );
        assert_eq!(exported_note.channel(), 9);
        assert_eq!(exported_note.key(), 60);
        assert_eq!(exported_note.velocity(), 96);
        assert_eq!(exported_note.start_tick(), 96);
        assert_eq!(exported_note.end_tick(), Some(144));
    }

    #[test]
    fn exports_song_pattern_clips_with_repeats_and_clip_boundary_truncation() {
        let note = note_record(0, 3, 48, 60, 100);
        let mut event_stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 24];
        event_stream.extend_from_slice(&note);
        event_stream.extend_from_slice(&[0x63, 0, 0]);
        let mut clip = [0u8; 80];
        clip[..4].copy_from_slice(&100u32.to_le_bytes());
        clip[6..8].copy_from_slice(&7u16.to_le_bytes());
        clip[8..12].copy_from_slice(&100u32.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        append_data_event(&mut event_stream, 0xE9, &clip);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("arrangement fixture should parse");

        let bytes =
            MidiFile::encode_project_song(&document, 0, MidiChannelMapping::PreserveNoteChannels)
                .expect("arrangement should export");
        let midi = MidiFile::parse(&bytes).expect("exported file should parse");
        let notes = midi
            .tracks()
            .iter()
            .flat_map(|track| track.notes())
            .collect::<Vec<_>>();

        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].start_tick(), 100);
        assert_eq!(notes[0].end_tick(), Some(148));
        assert_eq!(notes[1].start_tick(), 148);
        assert_eq!(notes[1].end_tick(), Some(196));
        assert_eq!(notes[2].start_tick(), 196);
        assert_eq!(notes[2].end_tick(), Some(200));
    }

    #[test]
    fn inferred_pattern_repeat_length_rounds_up_to_the_project_measure() {
        let note = note_record(0, 3, 1_501, 60, 100);
        let mut event_stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 24];
        event_stream.extend_from_slice(&note);
        event_stream.extend_from_slice(&[0x63, 0, 0]);
        let mut clip = [0u8; 80];
        clip[4..6].copy_from_slice(&0u16.to_le_bytes());
        clip[6..8].copy_from_slice(&7u16.to_le_bytes());
        clip[8..12].copy_from_slice(&1_536u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        append_data_event(&mut event_stream, 0xE9, &clip);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("arrangement fixture should parse");

        let bytes =
            MidiFile::encode_project_song(&document, 0, MidiChannelMapping::PreserveNoteChannels)
                .expect("arrangement should export");
        let midi = MidiFile::parse(&bytes).expect("exported file should parse");
        let notes = midi
            .tracks()
            .iter()
            .flat_map(|track| track.notes())
            .collect::<Vec<_>>();

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start_tick(), 0);
        assert_eq!(notes[0].end_tick(), Some(1_501));
    }

    #[test]
    fn exports_tempo_automation_per_tick_and_restores_the_project_tempo() {
        let mut event_stream = vec![0x40, 40, 0, 0x15, 5];
        append_project_info_string(&mut event_stream, 0xCB, "TEMPO");

        let points: [(f64, f64, f32, [u8; 4]); 3] = [
            (0.0, 0.75, 0.0f32, [0, 0, 0, 0]),
            (4.0, 0.75, -0.1142857f32, [0, 0, 0, 0xFF]),
            (8.0, 0.0, 0.0f32, [0, 0, 0, 2]),
        ];
        let mut automation = vec![0xA7; 17];
        automation.extend_from_slice(&(points.len() as u32).to_le_bytes());
        let mut previous_position = 0.0;
        for (position, value, tension, tail) in points {
            automation.extend_from_slice(&(position - previous_position).to_le_bytes());
            automation.extend_from_slice(&value.to_le_bytes());
            automation.extend_from_slice(&tension.to_le_bytes());
            automation.extend_from_slice(&tail);
            previous_position = position;
        }
        append_data_event(&mut event_stream, 0xEA, &automation);
        event_stream.extend_from_slice(&[0x62, 0, 0]);

        let mut clip = [0u8; 80];
        clip[4..6].copy_from_slice(&1_000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&40u16.to_le_bytes());
        clip[8..12].copy_from_slice(&768u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        append_data_event(&mut event_stream, 0xE9, &clip);

        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("tempo automation project fixture should parse");
        let bytes =
            MidiFile::encode_project_song(&document, 0, MidiChannelMapping::PreserveNoteChannels)
                .expect("song tempo automation should export");
        let midi = MidiFile::parse(&bytes).expect("exported MIDI should parse");
        let conductor = &midi.tracks()[0];
        let tempos = conductor.tempo_events();

        assert!(
            tempos
                .iter()
                .any(|tempo| tempo.tick() == 0 && (tempo.bpm() - 150.0).abs() < 0.001)
        );
        assert!(
            tempos
                .iter()
                .any(|tempo| tempo.tick() == 383 && (tempo.bpm() - 150.0).abs() < 0.001)
        );
        assert!(
            tempos
                .iter()
                .any(|tempo| tempo.tick() == 384 && (tempo.bpm() - 149.766).abs() < 0.01)
        );
        assert!(
            tempos
                .iter()
                .any(|tempo| tempo.tick() == 767 && (tempo.bpm() - 60.0).abs() < 0.001)
        );
        assert!(
            tempos
                .iter()
                .any(|tempo| tempo.tick() == 769 && (tempo.bpm() - 140.0).abs() < 0.001)
        );
        assert_eq!(conductor.end_tick(), 769);
    }

    #[test]
    fn midi_import_rejects_ambiguous_empty_pattern_encoding_without_mutation() {
        let note = note_record(0, 0, 48, 60, 100);
        let mut stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 0x18];
        stream.extend_from_slice(&note);
        stream.extend_from_slice(&[0x41, 8, 0, 0xE0, 0x18]);
        stream.extend_from_slice(&note);
        stream.extend_from_slice(&[0x41, 9, 0, 0xA4, 0, 0, 0, 0]);
        let project_bytes = flp_fixture(&stream, &[], &[]);
        let midi_bytes = midi_fixture(
            &[
                0x00, 0x90, 0x3C, 0x64, 0x81, 0x70, 0x80, 0x3C, 0x00, 0x00, 0xFF, 0x2F, 0x00,
            ],
            480,
        );
        let midi = MidiFile::parse(&midi_bytes).expect("MIDI fixture should parse");
        let mut document = FlpDocument::parse(&project_bytes).expect("project should parse");

        let error = document
            .import_midi_track(&midi, 0, 9, 0)
            .expect_err("conflicting project encodings must not be guessed");

        assert!(matches!(error, FlpError::UnsupportedEdit(_)));
        assert_eq!(document.encode_lossless().unwrap(), project_bytes);
    }

    #[test]
    fn automation_point_insert_delete_preserves_headers_trailers_and_original_bytes() {
        let source_points = [
            (1.0, 0.25, -0.25, [1, 2, 3, 4]),
            (3.0, 0.75, 0.5, [5, 6, 7, 8]),
        ];

        for trailer_length in [112, 136] {
            let trailer = (0..trailer_length)
                .map(|index| (index * 13 % 256) as u8)
                .collect::<Vec<_>>();
            let original = automation_channel_fixture(&source_points, &trailer);
            let mut document =
                FlpDocument::parse(&original).expect("automation fixture should parse");
            let original_payload = document
                .events()
                .iter()
                .find(|event| event.opcode() == 0xEA)
                .expect("fixture should contain an automation event")
                .payload()
                .to_vec();

            document
                .insert_automation_point(9, 1, 2.0, 0.5, 0.0)
                .expect("inserting between ordered points should succeed");
            let inserted = document.automation_channels().unwrap().remove(0);
            assert_eq!(inserted.points().len(), 3);
            assert_eq!(inserted.points()[0].position_beats(), 1.0);
            assert_eq!(inserted.points()[1].position_beats(), 2.0);
            assert_eq!(inserted.points()[2].position_beats(), 3.0);
            assert_eq!(inserted.points()[0].trailing_bytes(), [1, 2, 3, 4]);
            assert_eq!(inserted.points()[1].trailing_bytes(), [0; 4]);
            assert_eq!(inserted.points()[2].trailing_bytes(), [5, 6, 7, 8]);

            let inserted_payload =
                document.events()[inserted.data_event_index().unwrap()].payload();
            assert_eq!(&inserted_payload[..17], &original_payload[..17]);
            assert_eq!(
                &inserted_payload[inserted_payload.len() - trailer_length..],
                trailer
            );

            document
                .delete_automation_point(9, 1)
                .expect("deleting the inserted point should succeed");
            assert_eq!(document.encode_lossless().unwrap(), original);
        }
    }

    #[test]
    fn automation_point_delete_retains_absolute_positions_at_curve_edges() {
        let source_points = [
            (1.0, 0.1, 0.0, [1, 1, 1, 1]),
            (3.0, 0.5, 0.0, [2, 2, 2, 2]),
            (4.0, 0.9, 0.0, [3, 3, 3, 3]),
        ];
        let trailer = vec![0xD3; 112];

        for (deleted_index, expected_positions, expected_tails) in [
            (0, [3.0, 4.0], [[2, 2, 2, 2], [3, 3, 3, 3]]),
            (2, [1.0, 3.0], [[1, 1, 1, 1], [2, 2, 2, 2]]),
        ] {
            let mut document =
                FlpDocument::parse(&automation_channel_fixture(&source_points, &trailer))
                    .expect("automation fixture should parse");
            document
                .delete_automation_point(9, deleted_index)
                .expect("deleting an edge point should succeed");
            let channel = document.automation_channels().unwrap().remove(0);
            assert_eq!(channel.points().len(), 2);
            for index in 0..2 {
                assert_eq!(
                    channel.points()[index].position_beats(),
                    expected_positions[index]
                );
                assert_eq!(
                    channel.points()[index].trailing_bytes(),
                    expected_tails[index]
                );
            }

            let payload = document.events()[channel.data_event_index().unwrap()].payload();
            assert_eq!(&payload[payload.len() - trailer.len()..], trailer);
        }
    }

    #[test]
    fn decodes_layer_child_channel_ids_and_flags_losslessly() {
        let original = layer_channel_fixture(3, &[1, 2]);
        let document = FlpDocument::parse(&original).expect("layer fixture should parse");
        let channels = document.channels();
        assert_eq!(channels.len(), 3);
        assert_eq!(channels[0].kind(), Some(3));
        assert_eq!(channels[0].channel_type(), Some(super::ChannelType::Layer));
        assert_eq!(channels[0].layer_child_ids(), Some([1, 2].as_slice()));
        assert_eq!(channels[0].layer_flags(), Some(3));
        assert_eq!(channels[0].layer_random_enabled(), Some(true));
        assert_eq!(channels[0].layer_crossfade_enabled(), Some(true));
        assert_eq!(channels[1].channel_type(), Some(super::ChannelType::Native));
        assert_eq!(
            channels[2].channel_type(),
            Some(super::ChannelType::Instrument)
        );
        assert_eq!(document.encode_lossless().unwrap(), original);
    }

    #[test]
    fn editing_layer_children_preserves_other_channel_events() {
        let original = layer_channel_fixture(0xA500_0003, &[1, 2]);
        let mut document = FlpDocument::parse(&original).expect("layer fixture should parse");
        let non_child_events = document
            .events()
            .iter()
            .filter(|event| event.opcode() != 0x5E)
            .map(|event| event.wire_bytes().to_vec())
            .collect::<Vec<_>>();

        document
            .set_layer_child_ids(0, &[2, 1, 2])
            .expect("Layer child IDs should be editable");
        assert_eq!(
            document.channels()[0].layer_child_ids(),
            Some([2, 1, 2].as_slice())
        );
        assert_eq!(document.channels()[0].layer_flags(), Some(0xA500_0003));
        assert_eq!(
            document
                .events()
                .iter()
                .filter(|event| event.opcode() != 0x5E)
                .map(|event| event.wire_bytes().to_vec())
                .collect::<Vec<_>>(),
            non_child_events
        );

        let encoded = document
            .encode_lossless()
            .expect("edited Layer should encode");
        let reopened = FlpDocument::parse(&encoded).expect("edited Layer should parse again");
        assert_eq!(
            reopened.channels()[0].layer_child_ids(),
            Some([2, 1, 2].as_slice())
        );
    }

    #[test]
    fn layer_children_can_be_added_to_and_removed_from_an_empty_list() {
        let original = layer_channel_fixture(7, &[]);
        let mut document = FlpDocument::parse(&original).expect("empty Layer fixture should parse");

        document
            .set_layer_child_ids(0, &[1, 2])
            .expect("empty Layer child list should accept references");
        assert_eq!(
            document.channels()[0].layer_child_ids(),
            Some([1, 2].as_slice())
        );
        document
            .set_layer_child_ids(0, &[])
            .expect("Layer child references should be removable");
        assert_eq!(document.encode_lossless().unwrap(), original);
    }

    #[test]
    fn layer_child_edit_rejects_non_layer_and_missing_channels() {
        let original = layer_channel_fixture(0, &[1]);
        let mut document = FlpDocument::parse(&original).expect("layer fixture should parse");

        assert!(matches!(
            document.set_layer_child_ids(1, &[2]),
            Err(FlpError::UnsupportedEdit(_))
        ));
        assert!(matches!(
            document.set_layer_child_ids(99, &[2]),
            Err(FlpError::ChannelNotFound(99))
        ));
    }

    #[test]
    fn unknown_channel_type_round_trips_its_raw_value() {
        let channel_type = super::ChannelType::from_raw(1);
        assert_eq!(channel_type, super::ChannelType::Unknown(1));
        assert_eq!(channel_type.raw(), 1);
    }

    #[test]
    fn invalid_flp_magic_is_reported() {
        let error = FlpDocument::parse(b"not-an-flp").expect_err("bad magic must fail");
        assert!(matches!(error, FlpError::BadMagic { .. }));
    }
}
