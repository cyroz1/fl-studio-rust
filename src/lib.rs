use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

pub mod audio;
pub mod media;
pub mod midi;
pub mod plugins;
pub mod project_package;
pub mod sample_render;
pub mod vst3;

const FLHD: &[u8; 4] = b"FLhd";
const FLDT: &[u8; 4] = b"FLdt";
const MIN_HEADER_CONTENT_LENGTH: usize = 6;
const FLP_NOTE_RECORD_SIZE: usize = 24;
const FLP_PATTERN_CONTROLLER_RECORD_SIZE: usize = 12;
const FLP_PLAYLIST_RECORD_SIZES: [usize; 3] = [80, 60, 32];
const MAX_MERGED_PATTERN_NOTES: usize = 2_000_000;
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

    fn replace_word_payload(&mut self, value: u16) -> Result<(), FlpError> {
        if self.encoding != PayloadEncoding::Word || self.payload.len() != 2 {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a two-byte payload",
            });
        }
        let encoded = value.to_le_bytes();
        let Some(wire_payload) = self.wire_bytes.get_mut(1..3) else {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "word event does not contain its payload",
            });
        };
        wire_payload.copy_from_slice(&encoded);
        self.payload.copy_from_slice(&encoded);
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

/// The state-file variant identified by the FL Studio `FLhd` format field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FstPresetKind {
    AutomationState,
    ChannelState,
    NativePluginState,
    VstGeneratorState,
    VstEffectState,
    MixerInsertState,
    UnknownFormat(u16),
}

impl FstPresetKind {
    pub fn from_format(format: u16) -> Self {
        match format {
            24 => Self::AutomationState,
            32 => Self::ChannelState,
            48 => Self::NativePluginState,
            49 => Self::VstGeneratorState,
            50 => Self::VstEffectState,
            64 => Self::MixerInsertState,
            other => Self::UnknownFormat(other),
        }
    }
}

impl fmt::Display for FstPresetKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AutomationState => formatter.write_str("automation state"),
            Self::ChannelState => formatter.write_str("channel state"),
            Self::NativePluginState => formatter.write_str("native plug-in state"),
            Self::VstGeneratorState => formatter.write_str("VST generator state"),
            Self::VstEffectState => formatter.write_str("VST effect state"),
            Self::MixerInsertState => formatter.write_str("Mixer insert state"),
            Self::UnknownFormat(format) => write!(formatter, "unknown state format ({format})"),
        }
    }
}

/// A lossless FL Studio state preset with its container variant identified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FstPreset {
    kind: FstPresetKind,
    document: FlpDocument,
}

impl FstPreset {
    pub fn parse(bytes: &[u8]) -> Result<Self, FlpError> {
        let document = FlpDocument::parse(bytes)?;
        let kind = FstPresetKind::from_format(document.header().format());
        Ok(Self { kind, document })
    }

    pub fn kind(&self) -> FstPresetKind {
        self.kind
    }

    pub fn document(&self) -> &FlpDocument {
        &self.document
    }

    pub fn into_document(self) -> FlpDocument {
        self.document
    }

    pub fn encode_lossless(&self) -> Result<Vec<u8>, FlpError> {
        self.document.encode_lossless()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectMetadata {
    tempo_milli_bpm: Option<u32>,
    time_signature: Option<(u8, u8)>,
    global_swing_mix: Option<u8>,
    pan_law_raw: Option<u8>,
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
    /// Raw project pan-law value from the global `0x17` byte event.
    pub pan_law_raw: Option<u8>,
    /// Project time-signature numerator and denominator from global `0x11`/`0x12` events.
    pub time_signature: Option<(u8, u8)>,
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

/// Channel Rack ordering used by [`FlpDocument::sort_channels`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelSortOrder {
    Color,
    MixerTrack,
    Name,
    Type,
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
    zipped: bool,
    color: Option<u32>,
    mixer_track: Option<i8>,
    sampler_fx_flags: Option<u16>,
    sampler_flags: Option<u32>,
    sampler_root_note: Option<u32>,
    keyboard_key_region: Option<(u32, u32)>,
    ping_pong_loop: bool,
    swing_mix: Option<u16>,
    group_number: Option<i32>,
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

/// A named Channel Rack display filter from the project-level `0xE7` events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelGroupSummary {
    index: i32,
    name: Option<String>,
}

impl ChannelGroupSummary {
    /// Zero-based group index referenced by channel `0x91` events.
    pub fn index(&self) -> i32 {
        self.index
    }

    /// Group name, if its text event could be decoded.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
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

    /// Whether FL Studio saved this channel in compact (zipped) Channel Rack mode.
    /// A missing `0x0F` event defaults to false.
    pub fn zipped(&self) -> bool {
        self.zipped
    }

    /// Whether a kind-0 Sampler's saved Reverse option is enabled.
    ///
    /// The Reverse bit is bit 1 of the channel's `0x46` FX flags word. Unknown
    /// flag bits remain untouched in the original event stream.
    pub fn sample_reversed(&self) -> bool {
        self.kind == Some(0)
            && self
                .sampler_fx_flags
                .is_some_and(|flags| flags & (1 << 1) != 0)
    }

    /// Whether a kind-0 Sampler uses embedded sample loop points.
    ///
    /// The `UsesLoopPoints` option is bit 3 of the `0x8F` sampler-flags dword.
    pub fn sampler_uses_loop_points(&self) -> bool {
        self.kind == Some(0)
            && self
                .sampler_flags
                .is_some_and(|flags| flags & (1 << 3) != 0)
    }

    /// Whether a kind-0 Sampler has its Ping-pong loop option enabled (`0x14`).
    pub fn sampler_ping_pong_loop_enabled(&self) -> bool {
        self.kind == Some(0) && self.ping_pong_loop
    }

    /// Saved root note for a kind-0 Sampler, when the raw value is a MIDI key number.
    pub fn sampler_root_key(&self) -> Option<u16> {
        if self.kind != Some(0) {
            return None;
        }
        self.sampler_root_note
            .filter(|note| *note <= 127)
            .map(|note| note as u16)
    }

    /// Inclusive playable MIDI key range from the channel Parameters event.
    /// Missing or invalid ranges are treated as unrestricted.
    pub fn keyboard_key_region(&self) -> Option<(u16, u16)> {
        let (low, high) = self.keyboard_key_region?;
        (low <= high && high <= 127).then_some((low as u16, high as u16))
    }

    /// Raw four-byte channel color value, in little-endian RGBA byte order.
    pub fn color(&self) -> Option<[u8; 4]> {
        self.color.map(u32::to_le_bytes)
    }

    /// Raw signed Mixer track assignment from the channel's one-byte `0x16` event.
    pub fn mixer_track(&self) -> Option<i8> {
        self.mixer_track
    }

    /// Zero-based Channel Rack display group from the signed `0x91` event.
    /// Missing or negative values represent an unassigned channel.
    pub fn group_number(&self) -> Option<i32> {
        self.group_number
    }

    /// Raw FL channel volume value, in the project's 0..=12800 control range.
    pub fn volume(&self) -> Option<u32> {
        self.volume
    }

    /// Raw FL channel pan value, in the project's 0..=12800 control range.
    pub fn pan(&self) -> Option<i32> {
        self.pan
    }

    /// Raw per-channel swing mix from the `0x61` word event.
    /// A missing event uses FL Studio's default of 128 (100%).
    pub fn swing_mix_raw(&self) -> Option<u16> {
        self.swing_mix
    }

    /// Per-channel swing mix in FL Studio's 0..=128 range, defaulting to 128 (100%).
    pub fn swing_mix(&self) -> u16 {
        self.swing_mix.unwrap_or(128)
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

/// Resolves note channels to the enabled channels that should receive their MIDI.
///
/// Layer Random uses a stable per-note pseudo-random choice in this runtime so
/// Sampler and VST3 render paths agree on the selected child. FL Studio's native
/// random sequence is not known, so rendered choices may differ from FL Studio.
pub(crate) struct ChannelNoteRouter {
    channels_by_id: HashMap<u16, ChannelSummary>,
}

impl ChannelNoteRouter {
    pub(crate) fn new(channels: impl IntoIterator<Item = ChannelSummary>) -> Self {
        Self {
            channels_by_id: channels
                .into_iter()
                .map(|channel| (channel.id(), channel))
                .collect(),
        }
    }

    /// Returns targets for a note after applying the source and child key regions.
    /// Layer Random chooses from the children that accept this note.
    pub(crate) fn targets_for_note(
        &self,
        source_channel_id: u16,
        note_seed: u64,
        key: u16,
    ) -> Vec<u16> {
        let Some(source) = self.channels_by_id.get(&source_channel_id) else {
            return Vec::new();
        };
        if source.enabled == Some(false) {
            return Vec::new();
        }
        if !channel_accepts_key(source, key) {
            return Vec::new();
        }
        if source.channel_type() != Some(ChannelType::Layer) {
            return vec![source_channel_id];
        }

        let mut seen = HashSet::new();
        let children = source
            .layer_children
            .iter()
            .copied()
            .filter(|child_id| seen.insert(*child_id))
            .filter(|child_id| {
                self.channels_by_id.get(child_id).is_some_and(|child| {
                    child.enabled != Some(false)
                        && child.channel_type() != Some(ChannelType::Layer)
                        && channel_accepts_key(child, key)
                })
            })
            .collect::<Vec<_>>();

        if children.is_empty() {
            return children;
        }
        if source.layer_random_enabled() != Some(true) {
            return children;
        }

        let mut random = note_seed ^ (u64::from(source_channel_id) << 32) ^ 0x9E37_79B9_7F4A_7C15;
        random = (random ^ (random >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        random = (random ^ (random >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        random ^= random >> 31;
        vec![children[(random as usize) % children.len()]]
    }
}

fn channel_accepts_key(channel: &ChannelSummary, key: u16) -> bool {
    channel
        .keyboard_key_region()
        .is_none_or(|(low, high)| (low..=high).contains(&key))
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
    pub controllers: Vec<PatternController>,
    pub time_markers: Vec<TimeMarker>,
}

/// Raw controller point record stored in a Pattern's `0xDF` event.
///
/// The channel and flags bytes are retained without interpretation. `value_bits` keeps the
/// original IEEE-754 representation, including unusual or non-finite values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PatternController {
    pub position: u32,
    pub reserved: [u8; 2],
    pub channel: u8,
    pub flags: u8,
    pub value_bits: u32,
}

impl PatternController {
    /// Decodes the stored float value while leaving its original bits available in `value_bits`.
    pub fn value(&self) -> f32 {
        f32::from_bits(self.value_bits)
    }

    fn decode(record: &[u8; FLP_PATTERN_CONTROLLER_RECORD_SIZE]) -> Self {
        Self {
            position: u32::from_le_bytes(record[..4].try_into().expect("four-byte position")),
            reserved: [record[4], record[5]],
            channel: record[6],
            flags: record[7],
            value_bits: u32::from_le_bytes(record[8..12].try_into().expect("four-byte value")),
        }
    }

    fn encode(self) -> [u8; FLP_PATTERN_CONTROLLER_RECORD_SIZE] {
        let mut record = [0; FLP_PATTERN_CONTROLLER_RECORD_SIZE];
        record[..4].copy_from_slice(&self.position.to_le_bytes());
        record[4..6].copy_from_slice(&self.reserved);
        record[6] = self.channel;
        record[7] = self.flags;
        record[8..12].copy_from_slice(&self.value_bits.to_le_bytes());
        record
    }
}

/// Fields that can be changed on an existing Pattern controller point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatternControllerEdit {
    pub position: Option<u32>,
    pub value: Option<f32>,
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

/// FL Studio score-note flag for slide notes.
pub const PATTERN_NOTE_SLIDE_FLAG: u16 = 1 << 3;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PatternNoteEdit {
    pub position: Option<u32>,
    pub flags: Option<u16>,
    pub slide: Option<bool>,
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

/// Direction used when a limited pitch needs to move onto a selected scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitSnapDirection {
    Up,
    Down,
    Alternate,
}

/// Pitch range, wrap mode, and optional scale settings for the Piano roll Limit operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitNoteOptions<'a> {
    pub minimum_key: u16,
    pub maximum_key: u16,
    pub wrap_to_bottom: bool,
    pub scale_root: Option<u8>,
    pub scale_intervals: Option<&'a [u8]>,
    pub snap_direction: LimitSnapDirection,
}

/// Parameters for replacing simultaneous notes with a gated arpeggio sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpeggioOptions {
    pub step_ticks: u32,
    pub range_octaves: u8,
    pub gate_percent: u8,
    pub direction: ArpeggioDirection,
}

/// Timing mode for the Riff Machine's Groove stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiffMachineQuantizeMode {
    LeaveDuration,
    LeaveEnd,
    QuantizeDuration,
    QuantizeEnd,
}

/// Settings for a seeded, scale-based Riff Machine pass over an existing note progression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiffMachineOptions<'a> {
    pub scale_root: u8,
    pub scale_intervals: &'a [u8],
    pub minimum_key: u16,
    pub maximum_key: u16,
    pub wrap_to_bottom: bool,
    pub snap_direction: LimitSnapDirection,
    pub step_ticks: u32,
    pub range_octaves: u8,
    pub gate_percent: u8,
    pub direction: ArpeggioDirection,
    /// Reverse the generated riff's note time order, or pitch order when preserving onsets.
    pub mirror_horizontal: bool,
    /// Keep generated onsets fixed and reverse the pitch sequence during horizontal mirroring.
    pub preserve_start_times: bool,
    /// Reflect generated pitches around the midpoint of their current pitch range.
    pub mirror_vertical: bool,
    /// Optional Piano roll snap grid for the Groove stage; `None` means snap is disabled.
    pub groove_snap_ticks: Option<u32>,
    /// Mix from original starts toward quantized starts, from 0 through 100 percent.
    pub groove_start_percent: u8,
    /// Quantize only features within this fraction of half a snap step, from 0 through 100.
    pub groove_sensitivity_percent: u8,
    /// Mix note durations or end positions toward their quantized values.
    pub groove_duration_percent: u8,
    pub groove_quantize_mode: RiffMachineQuantizeMode,
    /// Mix seeded offsets into note panning, from 0 through 100 percent.
    pub pan_variation_percent: u8,
    pub length_multiplier_percent: u8,
    pub velocity_variation_percent: u8,
    /// Mix seeded offsets into release and modulation levels, from 0 through 100 percent.
    pub release_variation_percent: u8,
    pub mod_x_variation_percent: u8,
    pub mod_y_variation_percent: u8,
    /// Randomly shift generated pitches by up to this many semitones before scale fitting.
    pub pitch_variation_semitones: u8,
    /// Reset velocity, pan, release, and modulation values before applying offsets.
    pub reset_levels: bool,
    /// Randomize levels above and below their baseline when enabled.
    pub bipolar_levels: bool,
    pub seed: u64,
}

impl<'a> Default for RiffMachineOptions<'a> {
    fn default() -> Self {
        Self {
            scale_root: 0,
            scale_intervals: &[0, 2, 4, 5, 7, 9, 11],
            minimum_key: 36,
            maximum_key: 83,
            wrap_to_bottom: false,
            snap_direction: LimitSnapDirection::Up,
            step_ticks: 24,
            range_octaves: 1,
            gate_percent: 80,
            direction: ArpeggioDirection::Up,
            mirror_horizontal: false,
            preserve_start_times: false,
            mirror_vertical: false,
            groove_snap_ticks: Some(24),
            groove_start_percent: 0,
            groove_sensitivity_percent: 50,
            groove_duration_percent: 0,
            groove_quantize_mode: RiffMachineQuantizeMode::LeaveDuration,
            pan_variation_percent: 0,
            length_multiplier_percent: 100,
            velocity_variation_percent: 10,
            release_variation_percent: 0,
            mod_x_variation_percent: 0,
            mod_y_variation_percent: 0,
            pitch_variation_semitones: 0,
            reset_levels: false,
            bipolar_levels: true,
            seed: 1,
        }
    }
}

fn riff_machine_groove_note_timing(
    note: &PatternNote,
    snap_ticks: u32,
    options: RiffMachineOptions<'_>,
) -> Result<(u32, u32), FlpError> {
    if snap_ticks == 0 {
        return Err(FlpError::UnsupportedEdit(
            "Riff Machine Groove needs an enabled Piano roll snap grid",
        ));
    }
    let grid = u64::from(snap_ticks);
    let original_start = u64::from(note.position);
    let original_length = u64::from(note.length);
    let original_end = original_start
        .checked_add(original_length)
        .ok_or(FlpError::LengthOverflow)?;

    let quantize = |tick: u64| (tick.saturating_add(grid / 2) / grid).saturating_mul(grid);
    let near_grid = |tick: u64, snapped: u64| {
        tick.abs_diff(snapped).saturating_mul(200)
            <= grid.saturating_mul(u64::from(options.groove_sensitivity_percent))
    };
    let mix = |original: u64, target: u64, amount: u8| {
        let delta = target as i64 - original as i64;
        (original as i64 + delta * i64::from(amount) / 100).max(0) as u64
    };

    let snapped_start = quantize(original_start);
    let target_start = if near_grid(original_start, snapped_start) {
        snapped_start
    } else {
        original_start
    };
    let mut start = mix(original_start, target_start, options.groove_start_percent);
    let snapped_end = quantize(original_end);
    let target_end = if near_grid(original_end, snapped_end) {
        snapped_end
    } else {
        original_end
    };
    let length = match options.groove_quantize_mode {
        RiffMachineQuantizeMode::LeaveDuration => original_length,
        RiffMachineQuantizeMode::LeaveEnd => {
            start = start.min(original_end.saturating_sub(1));
            original_end.saturating_sub(start)
        }
        RiffMachineQuantizeMode::QuantizeDuration => {
            let target_length = target_end.saturating_sub(target_start).max(1);
            mix(
                original_length,
                target_length,
                options.groove_duration_percent,
            )
            .max(1)
        }
        RiffMachineQuantizeMode::QuantizeEnd => {
            let end = mix(original_end, target_end, options.groove_duration_percent);
            start = start.min(end.saturating_sub(1));
            end.saturating_sub(start)
        }
    };
    let start = u32::try_from(start).map_err(|_| FlpError::LengthOverflow)?;
    let length =
        u32::try_from(length.min(u64::from(u32::MAX))).map_err(|_| FlpError::LengthOverflow)?;
    Ok((start, length.max(1)))
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

/// Parameters for scaling note velocity with a pivot and logarithmic tension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScaleLevelsOptions {
    pub center_percent: i16,
    pub tension_percent: i16,
    pub multiplier_percent: u16,
    pub offset_percent: i16,
}

impl Default for ScaleLevelsOptions {
    fn default() -> Self {
        Self {
            center_percent: 0,
            tension_percent: 0,
            multiplier_percent: 100,
            offset_percent: 0,
        }
    }
}

/// Parameters for scaling note lengths with an optional seeded variation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArticulateOptions {
    pub multiplier_percent: u8,
    pub variation_percent: u8,
    pub seed: u64,
    pub use_original_lengths: bool,
    pub chop_chords: bool,
}

/// Parameters for the Piano roll Claw Machine's periodic note gate and timing slew.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClawMachineOptions {
    pub period_ticks: u32,
    pub trash_every: u8,
    pub time_distortion_percent: i16,
    pub remove_short_notes: bool,
    pub stretch_to_compensate: bool,
}

impl Default for ClawMachineOptions {
    fn default() -> Self {
        Self {
            period_ticks: 384,
            trash_every: 4,
            time_distortion_percent: 0,
            remove_short_notes: false,
            stretch_to_compensate: false,
        }
    }
}

impl Default for ArticulateOptions {
    fn default() -> Self {
        Self {
            multiplier_percent: 100,
            variation_percent: 0,
            seed: 1,
            use_original_lengths: true,
            chop_chords: false,
        }
    }
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

fn note_pitch_in_scale(key: i32, scale_root: u8, scale_intervals: &[u8]) -> bool {
    let pitch_class = (key.rem_euclid(12) - i32::from(scale_root)).rem_euclid(12) as u8;
    scale_intervals.contains(&pitch_class)
}

fn claw_warp_position(
    position: u64,
    origin: u64,
    period_ticks: u32,
    distortion_percent: i16,
) -> Result<u32, FlpError> {
    let period = u64::from(period_ticks);
    let relative = position.saturating_sub(origin);
    let cycle_start = origin + (relative / period) * period;
    let phase = relative % period;
    let normalized = phase as f64 / period as f64;
    let exponent = 1.0 + 2.0 * (f64::from(distortion_percent.abs()) / 100.0);
    let warped = if distortion_percent >= 0 {
        normalized.powf(exponent)
    } else {
        1.0 - (1.0 - normalized).powf(exponent)
    };
    let warped_phase = (warped * period as f64)
        .round()
        .clamp(0.0, (period - 1) as f64) as u64;
    let warped_position = cycle_start + warped_phase;
    u32::try_from(warped_position).map_err(|_| FlpError::LengthOverflow)
}

fn fold_note_pitch_to_limit(key: i32, minimum: i32, maximum: i32, wrap_to_bottom: bool) -> i32 {
    if wrap_to_bottom {
        return (minimum + (key - minimum).rem_euclid(12)).min(maximum);
    }

    let mut key = key;
    while key > maximum && key - 12 >= minimum {
        key -= 12;
    }
    while key < minimum && key + 12 <= maximum {
        key += 12;
    }
    if key < minimum || key > maximum {
        if (key - minimum).abs() <= (key - maximum).abs() {
            minimum
        } else {
            maximum
        }
    } else {
        key
    }
}

fn snap_note_pitch_to_scale(
    key: i32,
    minimum: i32,
    maximum: i32,
    scale_root: u8,
    scale_intervals: &[u8],
    snap_up: bool,
) -> i32 {
    let in_scale = |candidate| note_pitch_in_scale(candidate, scale_root, scale_intervals);
    if snap_up {
        (key..=maximum)
            .find(|candidate| in_scale(*candidate))
            .or_else(|| (minimum..key).rev().find(|candidate| in_scale(*candidate)))
            .expect("the validated key range contains at least one scale pitch")
    } else {
        (minimum..=key)
            .rev()
            .find(|candidate| in_scale(*candidate))
            .or_else(|| (key..=maximum).find(|candidate| in_scale(*candidate)))
            .expect("the validated key range contains at least one scale pitch")
    }
}

fn riff_triad_keys(
    key: u16,
    minimum_key: u16,
    maximum_key: u16,
    scale_root: u8,
    scale_intervals: &[u8],
    wrap_to_bottom: bool,
    snap_up: bool,
) -> Result<Vec<u16>, FlpError> {
    let minimum = i32::from(minimum_key);
    let maximum = i32::from(maximum_key);
    let folded = fold_note_pitch_to_limit(i32::from(key), minimum, maximum, wrap_to_bottom);
    let root_key = snap_note_pitch_to_scale(
        folded,
        minimum,
        maximum,
        scale_root,
        scale_intervals,
        snap_up,
    );
    let root_interval = (root_key.rem_euclid(12) - i32::from(scale_root)).rem_euclid(12) as u8;
    let root_degree = scale_intervals
        .iter()
        .position(|interval| *interval == root_interval)
        .ok_or(FlpError::UnsupportedEdit(
            "the Riff Machine root does not belong to the selected scale",
        ))?;

    let mut pitches = Vec::with_capacity(3);
    for degree_offset in [0usize, 2, 4] {
        let degree = root_degree + degree_offset;
        let octave = degree / scale_intervals.len();
        let interval = i32::from(scale_intervals[degree % scale_intervals.len()]);
        let root_interval = i32::from(scale_intervals[root_degree]);
        let pitch =
            root_key + i32::try_from(octave).map_err(|_| FlpError::LengthOverflow)? * 12 + interval
                - root_interval;
        let pitch = fold_note_pitch_to_limit(pitch, minimum, maximum, wrap_to_bottom);
        let pitch =
            snap_note_pitch_to_scale(pitch, minimum, maximum, scale_root, scale_intervals, true);
        let pitch = u16::try_from(pitch).map_err(|_| FlpError::LengthOverflow)?;
        if !pitches.contains(&pitch) {
            pitches.push(pitch);
        }
    }
    pitches.sort_unstable();
    Ok(pitches)
}

fn scale_note_level(level: u8, options: ScaleLevelsOptions) -> u8 {
    let pivot = f64::from(options.center_percent) * 1.27;
    let distance = f64::from(level) - pivot;
    let distance_scale = pivot.abs().max((127.0 - pivot).abs()).max(1.0);
    let normalized_distance = (distance.abs() / distance_scale).clamp(0.0, 1.0);
    let tension = f64::from(options.tension_percent) / 100.0 * std::f64::consts::LN_10;
    let curved_distance = if tension == 0.0 {
        distance.abs()
    } else {
        let curved = ((1.0 + (tension.exp() - 1.0) * normalized_distance).ln()) / tension;
        curved * distance_scale
    };
    let tensioned = pivot + distance.signum() * curved_distance;
    let multiplier = f64::from(options.multiplier_percent) / 100.0;
    let offset = f64::from(options.offset_percent) * 1.27;
    (pivot + (tensioned - pivot) * multiplier + offset)
        .round()
        .clamp(0.0, 127.0) as u8
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTrack {
    /// One-based Playlist track identifier stored in the `0xEE` record.
    pub id: u32,
    /// The adjacent `0x2B` byte is retained without assigning it a meaning.
    pub state_byte: Option<u8>,
    pub name: Option<String>,
    /// Track enabled state from byte 12 of the `0xEE` state record. Disabled tracks are muted.
    pub enabled: Option<bool>,
    /// Whether this track is grouped with the Playlist track directly above it.
    pub grouped: Option<bool>,
    /// Raw 70-byte `0xEE` state payload.
    pub state_bytes: Vec<u8>,
}

/// Fields that can be changed on an existing Playlist track without rewriting its other state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaylistTrackEdit {
    pub enabled: Option<bool>,
    pub grouped: Option<bool>,
}

/// Mixer insert fields recognized from the observed `0x9A`, `0x93`, `0x95`
/// sequence. The source events remain byte-exact in [`FlpDocument::events`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerInsertSummary {
    ordinal: usize,
    input_raw: i32,
    output_raw: i32,
    color_raw: u32,
    icon_raw: Option<i16>,
    name: Option<String>,
    flags: Option<u32>,
    flags_event_index: Option<usize>,
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

    /// Raw insert flag bitmask from the version-specific Mixer state event.
    pub fn flags(&self) -> Option<u32> {
        self.flags
    }

    /// Whether this Mixer track is enabled. Disabled tracks are muted.
    pub fn enabled(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 3) != 0)
    }

    /// Whether this Mixer track is soloed.
    pub fn soloed(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 12) != 0)
    }

    /// Whether the track reverses signal polarity.
    pub fn polarity_reversed(&self) -> Option<bool> {
        self.flags.map(|flags| flags & 1 != 0)
    }

    /// Whether the track swaps its left and right channels.
    pub fn swap_left_right(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 1) != 0)
    }

    /// Whether effects processing is enabled for this Mixer track.
    pub fn effects_enabled(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 2) != 0)
    }

    /// Event range associated with this insert record, ending before the next recognized record.
    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

/// Conservative source-audibility rules for saved Mixer insert mute and solo state.
/// Unknown and context-dependent routes are left untouched.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MixerRouteAudibility {
    known_tracks: HashSet<i8>,
    disabled_tracks: HashSet<i8>,
    soloed_insert_tracks: HashSet<i8>,
    source_transforms: HashMap<i8, MixerInsertSignalTransform>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MixerInsertSignalTransform {
    polarity_reversed: bool,
    swap_left_right: bool,
}

impl MixerInsertSignalTransform {
    pub(crate) const fn new(polarity_reversed: bool, swap_left_right: bool) -> Self {
        Self {
            polarity_reversed,
            swap_left_right,
        }
    }

    pub(crate) fn apply_frame(self, frame: &mut [f32; 2]) {
        if self.swap_left_right {
            frame.swap(0, 1);
        }
        if self.polarity_reversed {
            frame[0] = -frame[0];
            frame[1] = -frame[1];
        }
    }
}

impl MixerRouteAudibility {
    pub(crate) fn from_inserts(inserts: impl IntoIterator<Item = MixerInsertSummary>) -> Self {
        let mut routing = Self::default();
        for insert in inserts {
            let Ok(track) = i8::try_from(insert.ordinal()) else {
                continue;
            };
            routing.known_tracks.insert(track);
            // Master is the final destination for every normally routed source. Its
            // saved mute/phase flags are applied to the completed stereo mix instead.
            if track == 0 {
                continue;
            }
            if insert.enabled() == Some(false) {
                routing.disabled_tracks.insert(track);
            }
            if insert.soloed() == Some(true) {
                routing.soloed_insert_tracks.insert(track);
            }
            let transform = MixerInsertSignalTransform::new(
                insert.polarity_reversed() == Some(true),
                insert.swap_left_right() == Some(true),
            );
            if transform != MixerInsertSignalTransform::default() {
                routing.source_transforms.insert(track, transform);
            }
        }
        routing
    }

    /// `None` is FL Studio's default Master route; `-1` means context-dependent
    /// Current insert, so it is kept audible until that context is understood.
    pub(crate) fn allows_channel(&self, route: Option<i8>) -> bool {
        let track = match route {
            None => 0,
            Some(track) if track < 0 => return true,
            Some(track) => track,
        };
        if track != 0 && !self.known_tracks.contains(&track) {
            return true;
        }
        if self.disabled_tracks.contains(&track) {
            return false;
        }
        self.soloed_insert_tracks.is_empty() || self.soloed_insert_tracks.contains(&track)
    }

    /// Returns the supported insert phase/stereo transform for a known source route.
    /// Master transforms are applied once to the final mix, and unknown routes are unchanged.
    pub(crate) fn transform_for_channel(&self, route: Option<i8>) -> MixerInsertSignalTransform {
        route
            .filter(|track| *track > 0 && self.known_tracks.contains(track))
            .and_then(|track| self.source_transforms.get(&track).copied())
            .unwrap_or_default()
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

/// Fields that can be toggled in an existing Mixer insert flag record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MixerInsertEdit {
    pub enabled: Option<bool>,
    pub soloed: Option<bool>,
    pub polarity_reversed: Option<bool>,
    pub swap_left_right: Option<bool>,
    pub effects_enabled: Option<bool>,
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

    /// Corpus-derived insert ordinal candidate for this target index.
    ///
    /// Across the inspected named Mixer inserts, target 64 corresponded to the first
    /// insert and subsequent targets advanced in order. Projects with missing or
    /// ambiguous parameter records still require a separate mapping check.
    pub fn candidate_insert_ordinal(&self) -> Option<usize> {
        self.target_index().checked_sub(64).map(usize::from)
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

/// An opaque, lossless copy of one Playlist clip record for later insertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistClipClipboard {
    raw_record: Vec<u8>,
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
    /// Whether this note carries FL Studio's slide-note flag.
    pub fn is_slide_note(&self) -> bool {
        self.flags & PATTERN_NOTE_SLIDE_FLAG != 0
    }

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
        if let Some(value) = edit.slide {
            if value {
                self.flags |= PATTERN_NOTE_SLIDE_FLAG;
            } else {
                self.flags &= !PATTERN_NOTE_SLIDE_FLAG;
            }
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

    /// Raw global Channel Rack swing mix from the project-level `0x0B` byte event.
    pub fn global_swing_mix_raw(&self) -> Option<u8> {
        self.global_swing_mix
    }

    /// Global Channel Rack swing mix in FL's 0..=128 range; missing events default to zero.
    pub fn global_swing_mix(&self) -> u8 {
        self.global_swing_mix.unwrap_or(0)
    }

    /// Raw project pan-law value from the global `0x17` byte event.
    /// FL Studio uses 0 for Circular (the default) and 2 for Triangular.
    pub fn pan_law_raw(&self) -> Option<u8> {
        self.pan_law_raw
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
                0x80 if event.encoding == PayloadEncoding::Dword && event.payload.len() == 4 => {
                    channel.color = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0x00 if event.payload.len() == 1 => {
                    channel.enabled = Some(event.payload[0] != 0);
                }
                0x0F if event.payload.len() == 1 => {
                    channel.zipped = event.payload[0] != 0;
                }
                0x14 if event.payload.len() == 1 => {
                    channel.ping_pong_loop = event.payload[0] != 0;
                }
                0x46 if event.encoding == PayloadEncoding::Word
                    && event.payload.len() == 2
                    && channel.sampler_fx_flags.is_none() =>
                {
                    channel.sampler_fx_flags =
                        Some(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
                0x8F if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.sampler_flags.is_none() =>
                {
                    channel.sampler_flags = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0x87 if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.sampler_root_note.is_none() =>
                {
                    channel.sampler_root_note = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0xC7 if matches!(event.encoding, PayloadEncoding::Data { .. })
                    && event.payload.len() >= 76
                    && channel.keyboard_key_region.is_none() =>
                {
                    let low = u32::from_le_bytes(
                        event.payload[68..72]
                            .try_into()
                            .expect("the key-region low bound has four payload bytes"),
                    );
                    let high = u32::from_le_bytes(
                        event.payload[72..76]
                            .try_into()
                            .expect("the key-region high bound has four payload bytes"),
                    );
                    channel.keyboard_key_region = Some((low, high));
                }
                0x16 if event.encoding == PayloadEncoding::Byte && event.payload.len() == 1 => {
                    channel.mixer_track = Some(event.payload[0] as i8);
                }
                0x61 if event.encoding == PayloadEncoding::Word
                    && event.payload.len() == 2
                    && channel.swing_mix.is_none() =>
                {
                    channel.swing_mix =
                        Some(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
                0x91 if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.group_number.is_none() =>
                {
                    channel.group_number = Some(i32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
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

    /// Returns Channel Rack display groups declared before the first channel marker.
    /// The same `0xE7` opcode is also used elsewhere in the project stream, so only the
    /// project header section is treated as display-group metadata.
    pub fn channel_groups(&self) -> Vec<ChannelGroupSummary> {
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        self.events[..first_channel]
            .iter()
            .filter(|event| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .enumerate()
            .map(|(index, event)| ChannelGroupSummary {
                index: i32::try_from(index).unwrap_or(i32::MAX),
                name: decode_project_string(&event.payload, self.project_version.as_deref())
                    .filter(|name| !name.is_empty()),
            })
            .collect()
    }

    fn project_string_encoding(&self) -> Option<bool> {
        let utf16_from_version = project_string_version(self.project_version.as_deref())
            .map(|(major, minor)| major > 11 || (major == 11 && minor >= 5));
        utf16_from_version.or_else(|| {
            let first_channel = self
                .events
                .iter()
                .position(|event| event.opcode == 0x40)
                .unwrap_or(self.events.len());
            self.events.iter().enumerate().find_map(|(index, event)| {
                let is_project_string =
                    matches!(event.opcode, 0xC2 | 0xC3 | 0xC4 | 0xC5 | 0xCB | 0xCE | 0xCF)
                        || (event.opcode == 0xE7 && index < first_channel);
                (is_project_string && matches!(event.encoding, PayloadEncoding::Data { .. }))
                    .then(|| project_string_is_utf16(&event.payload, None))
            })
        })
    }

    /// Creates a Channel Rack display group when `name` is new and assigns every listed channel
    /// to it. Existing groups are matched by exact name. All edits are applied atomically while
    /// preserving unrelated event bytes.
    pub fn group_channels(&mut self, channel_ids: &[u16], name: &str) -> Result<i32, FlpError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group needs a non-empty name without embedded NUL characters",
            ));
        }
        if channel_ids.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "at least one Channel Rack channel must be selected",
            ));
        }

        let mut selected_ids = BTreeSet::new();
        let channels = self.channels();
        for channel_id in channel_ids {
            if !selected_ids.insert(*channel_id) {
                return Err(FlpError::UnsupportedEdit(
                    "a Channel Rack channel was selected more than once",
                ));
            }
            let mut matching = channels
                .iter()
                .filter(|channel| channel.id() == *channel_id);
            if matching.next().is_none() {
                return Err(FlpError::ChannelNotFound(*channel_id));
            }
            if matching.next().is_some() {
                return Err(FlpError::AmbiguousChannelId(*channel_id));
            }
        }

        let groups = self.channel_groups();
        let matching_groups = groups
            .iter()
            .filter(|group| group.name() == Some(name))
            .map(ChannelGroupSummary::index)
            .collect::<Vec<_>>();
        if matching_groups.len() > 1 {
            return Err(FlpError::UnsupportedEdit(
                "multiple Channel Rack groups have the requested name",
            ));
        }
        let (group_index, create_group) = if let Some(index) = matching_groups.first() {
            (*index, false)
        } else {
            (
                i32::try_from(groups.len()).map_err(|_| {
                    FlpError::UnsupportedEdit("the project has too many Channel Rack groups")
                })?,
                true,
            )
        };

        let mut candidate = self.clone();
        if create_group {
            let utf16 = self
                .project_string_encoding()
                .ok_or(FlpError::UnsupportedEdit(
                    "the project's string encoding cannot be inferred for a new Channel Rack group",
                ))?;
            let name_payload = encode_project_string(name, utf16)?;
            let group_event = FlpEvent::new_data(0xE7, name_payload)?;
            let first_channel = candidate
                .events
                .iter()
                .position(|event| event.opcode == 0x40)
                .unwrap_or(candidate.events.len());
            let insertion_index = candidate.events[..first_channel]
                .iter()
                .rposition(|event| {
                    event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
                })
                .map_or(first_channel, |index| index + 1);
            candidate.events.insert(insertion_index, group_event);
        }

        let mut selected_channels = candidate
            .channels()
            .into_iter()
            .filter(|channel| selected_ids.contains(&channel.id()))
            .collect::<Vec<_>>();
        selected_channels.sort_by_key(|channel| std::cmp::Reverse(channel.first_event_index));
        let mut changed = create_group;
        let group_number_payload = u32::from_le_bytes(group_index.to_le_bytes());
        for channel in selected_channels {
            let mut group_events = channel
                .event_range()
                .filter(|index| candidate.events[*index].opcode == 0x91);
            let existing_index = group_events.next();
            if group_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "a selected channel has multiple display-group events",
                ));
            }
            if let Some(event_index) = existing_index {
                let event = &candidate.events[event_index];
                if event.encoding != PayloadEncoding::Dword || event.payload.len() != 4 {
                    return Err(FlpError::UnsupportedEdit(
                        "a selected channel's display-group event is not a dword",
                    ));
                }
                let current_index = i32::from_le_bytes(
                    event.payload[..4]
                        .try_into()
                        .expect("a dword event has four payload bytes"),
                );
                if current_index != group_index {
                    candidate.events[event_index].replace_dword_payload(group_number_payload)?;
                    changed = true;
                }
            } else {
                candidate.events.insert(
                    channel.event_range().end,
                    FlpEvent::new_dword(0x91, group_number_payload),
                );
                changed = true;
            }
        }
        if changed {
            candidate.refresh_event_offsets()?;
            *self = candidate;
        }
        Ok(group_index)
    }

    /// Adds an empty Channel Rack display group and returns its zero-based index.
    pub fn add_channel_group(&mut self, name: &str) -> Result<i32, FlpError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group needs a non-empty name without embedded NUL characters",
            ));
        }
        let groups = self.channel_groups();
        if groups.iter().any(|group| group.name() == Some(name)) {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group already has the requested name",
            ));
        }
        let group_index = i32::try_from(groups.len()).map_err(|_| {
            FlpError::UnsupportedEdit("the project has too many Channel Rack groups")
        })?;
        let utf16 = self
            .project_string_encoding()
            .ok_or(FlpError::UnsupportedEdit(
                "the project's string encoding cannot be inferred for a new Channel Rack group",
            ))?;
        let group_event = FlpEvent::new_data(0xE7, encode_project_string(name, utf16)?)?;
        let mut candidate = self.clone();
        let first_channel = candidate
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(candidate.events.len());
        let insertion_index = candidate.events[..first_channel]
            .iter()
            .rposition(|event| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .map_or(first_channel, |index| index + 1);
        candidate.events.insert(insertion_index, group_event);
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(group_index)
    }

    /// Renames a Channel Rack display group without changing its channel assignments.
    pub fn rename_channel_group(&mut self, group_index: i32, name: &str) -> Result<(), FlpError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group needs a non-empty name without embedded NUL characters",
            ));
        }
        let groups = self.channel_groups();
        let Some(group) = groups.iter().find(|group| group.index() == group_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ));
        };
        if group.name() == Some(name) {
            return Ok(());
        }
        if groups
            .iter()
            .any(|other| other.index() != group_index && other.name() == Some(name))
        {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group already has the requested name",
            ));
        }
        let utf16 = self
            .project_string_encoding()
            .ok_or(FlpError::UnsupportedEdit(
                "the project's string encoding cannot be inferred for a renamed Channel Rack group",
            ))?;
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        let group_event_index = self.events[..first_channel]
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .nth(usize::try_from(group_index).map_err(|_| {
                FlpError::UnsupportedEdit("the requested Channel Rack group does not exist")
            })?)
            .map(|(index, _)| index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ))?;
        let mut candidate = self.clone();
        candidate.events[group_event_index]
            .replace_data_payload(encode_project_string(name, utf16)?)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Deletes a Channel Rack group, unassigns its channels, and shifts higher group indexes down.
    pub fn delete_channel_group(&mut self, group_index: i32) -> Result<(), FlpError> {
        if !self
            .channel_groups()
            .iter()
            .any(|group| group.index() == group_index)
        {
            return Err(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ));
        }
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        let group_event_index = self.events[..first_channel]
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .nth(usize::try_from(group_index).map_err(|_| {
                FlpError::UnsupportedEdit("the requested Channel Rack group does not exist")
            })?)
            .map(|(index, _)| index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ))?;

        let mut candidate = self.clone();
        candidate.events.remove(group_event_index);
        let mut channels = candidate.channels();
        channels.sort_by_key(|channel| std::cmp::Reverse(channel.first_event_index));
        for channel in channels {
            let mut group_events = channel
                .event_range()
                .filter(|index| candidate.events[*index].opcode == 0x91);
            let existing_index = group_events.next();
            if group_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "a channel has multiple display-group events",
                ));
            }
            let Some(event_index) = existing_index else {
                continue;
            };
            let event = &candidate.events[event_index];
            if event.encoding != PayloadEncoding::Dword || event.payload.len() != 4 {
                return Err(FlpError::UnsupportedEdit(
                    "a channel's display-group event is not a dword",
                ));
            }
            let current_index = i32::from_le_bytes(
                event.payload[..4]
                    .try_into()
                    .expect("a dword event has four payload bytes"),
            );
            if current_index == group_index {
                candidate.events.remove(event_index);
            } else if current_index > group_index {
                candidate.events[event_index].replace_dword_payload((current_index - 1) as u32)?;
            }
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
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

    /// Replaces field 53 of a marker-12 VST3 channel state while retaining its
    /// wrapper metadata, identity fields, and every unrelated field byte-for-byte.
    pub fn replace_vst3_channel_state_bytes(
        &mut self,
        channel_id: u16,
        nested_state: &[u8],
    ) -> Result<(), FlpError> {
        let mut candidate = self.clone();
        let channel = candidate
            .channels()
            .into_iter()
            .find(|channel| channel.id() == channel_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the VST3 state channel does not exist",
            ))?;
        let data_events = channel
            .event_range()
            .filter(|index| candidate.events[*index].opcode == 0xD5)
            .collect::<Vec<_>>();
        let [data_event_index] = data_events.as_slice() else {
            return Err(FlpError::UnsupportedEdit(
                "the channel must contain exactly one VST plug-in data event",
            ));
        };
        let state = candidate
            .channel_plugin_states()
            .into_iter()
            .find(|state| state.channel_id() == channel_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the channel VST plug-in data event could not be decoded",
            ))?;
        let Some(metadata) = state.vst_metadata() else {
            return Err(FlpError::UnsupportedEdit(
                "the channel has an unsupported VST plug-in state envelope",
            ));
        };
        if metadata.format_marker() != 12 || metadata.fourcc().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "VST3 state write-back requires a marker-12 VST3 envelope",
            ));
        }
        if metadata.class_uid().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the channel VST3 state has no valid class UID",
            ));
        }
        let replacement_payload = replace_vst_state_field(
            state.data_payload(),
            metadata.state_data_range.as_ref(),
            nested_state,
        )?;
        candidate.events[*data_event_index].replace_data_payload(replacement_payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Returns pattern metadata, note records, and raw controller points.
    pub fn patterns(&self) -> Result<Vec<Pattern>, FlpError> {
        let mut patterns = Vec::<Pattern>::new();
        let mut pattern_indices = HashMap::<u16, usize>::new();
        let mut current_pattern = None;
        let mut current_time_marker = None;
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
                    current_time_marker = None;

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
                0x40 | 0x62 | 0x63 => {
                    current_pattern = None;
                    current_time_marker = None;
                }
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
                0xDF => {
                    if let Some(pattern_index) = current_pattern {
                        if !event
                            .payload
                            .len()
                            .is_multiple_of(FLP_PATTERN_CONTROLLER_RECORD_SIZE)
                        {
                            return Err(FlpError::InvalidEvent {
                                offset: event.file_offset,
                                detail: "pattern controller payload is not a whole number of 12-byte records",
                            });
                        }
                        patterns[pattern_index].controllers.extend(
                            event
                                .payload
                                .as_chunks::<FLP_PATTERN_CONTROLLER_RECORD_SIZE>()
                                .0
                                .iter()
                                .map(|record| PatternController {
                                    position: u32::from_le_bytes(
                                        record[..4]
                                            .try_into()
                                            .expect("a Pattern controller position has four bytes"),
                                    ),
                                    reserved: [record[4], record[5]],
                                    channel: record[6],
                                    flags: record[7],
                                    value_bits: u32::from_le_bytes(
                                        record[8..12]
                                            .try_into()
                                            .expect("a Pattern controller value has four bytes"),
                                    ),
                                }),
                        );
                    }
                }
                0x94 if event.payload.len() == 4 => {
                    current_time_marker = current_pattern.map(|pattern_index| {
                        let marker_index = patterns[pattern_index].time_markers.len();
                        patterns[pattern_index].time_markers.push(TimeMarker {
                            raw_position: u32::from_le_bytes(
                                event.payload[..4]
                                    .try_into()
                                    .expect("a dword time-marker event has four bytes"),
                            ),
                            source_events: TimeMarkerSourceEvents {
                                position: Some(event_index),
                                ..TimeMarkerSourceEvents::default()
                            },
                            ..TimeMarker::default()
                        });
                        (pattern_index, marker_index)
                    });
                }
                0x21 if event.payload.len() == 1 => {
                    if let Some((pattern_index, marker_index)) = current_time_marker
                        && let Some(marker) = patterns
                            .get_mut(pattern_index)
                            .and_then(|pattern| pattern.time_markers.get_mut(marker_index))
                    {
                        marker.numerator = Some(event.payload[0]);
                        marker.source_events.numerator = Some(event_index);
                    }
                }
                0x22 if event.payload.len() == 1 => {
                    if let Some((pattern_index, marker_index)) = current_time_marker
                        && let Some(marker) = patterns
                            .get_mut(pattern_index)
                            .and_then(|pattern| pattern.time_markers.get_mut(marker_index))
                    {
                        marker.denominator = Some(event.payload[0]);
                        marker.source_events.denominator = Some(event_index);
                    }
                }
                0xCD => {
                    if let Some((pattern_index, marker_index)) = current_time_marker
                        && let Some(marker) = patterns
                            .get_mut(pattern_index)
                            .and_then(|pattern| pattern.time_markers.get_mut(marker_index))
                    {
                        marker.name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                        marker.source_events.name = Some(event_index);
                    }
                    current_time_marker = None;
                }
                _ => {}
            }
            event_index += 1;
        }
        patterns.sort_by_key(|pattern| pattern.id);
        Ok(patterns)
    }

    /// Edits position or value in one raw Pattern controller record without changing its length.
    /// Controller channel and flags bytes remain untouched because their target semantics are
    /// still unknown.
    pub fn edit_pattern_controller(
        &mut self,
        pattern_id: u16,
        controller_index: usize,
        edit: PatternControllerEdit,
    ) -> Result<(), FlpError> {
        if edit.value.is_some_and(|value| !value.is_finite()) {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller values must be finite",
            ));
        }
        if edit.position.is_none() && edit.value.is_none() {
            return Ok(());
        }

        let (event_index, record_offset, _) =
            self.pattern_controller_record_location(pattern_id, controller_index)?;

        let mut candidate = self.clone();
        if let Some(position) = edit.position {
            write_event_payload_bytes(
                &mut candidate.events[event_index],
                record_offset,
                &position.to_le_bytes(),
            )?;
        }
        if let Some(value) = edit.value {
            write_event_payload_bytes(
                &mut candidate.events[event_index],
                record_offset + 8,
                &value.to_bits().to_le_bytes(),
            )?;
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Adds a raw Pattern controller point by copying an existing point's uninterpreted fields.
    /// Reserved, channel, and flags bytes are copied exactly; only position and value change.
    pub fn duplicate_pattern_controller(
        &mut self,
        pattern_id: u16,
        template_controller_index: usize,
        position: u32,
        value: f32,
    ) -> Result<usize, FlpError> {
        if !value.is_finite() {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller values must be finite",
            ));
        }
        let (event_index, _, mut controller) =
            self.pattern_controller_record_location(pattern_id, template_controller_index)?;
        let new_index = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .map(|pattern| pattern.controllers.len())
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        controller.position = position;
        controller.value_bits = value.to_bits();

        let mut candidate = self.clone();
        let mut payload = candidate.events[event_index].payload.clone();
        payload
            .len()
            .checked_add(FLP_PATTERN_CONTROLLER_RECORD_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        payload.extend_from_slice(&controller.encode());
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_index)
    }

    /// Removes one raw Pattern controller point by its zero-based Pattern index.
    pub fn delete_pattern_controller(
        &mut self,
        pattern_id: u16,
        controller_index: usize,
    ) -> Result<(), FlpError> {
        let (event_index, record_offset, _) =
            self.pattern_controller_record_location(pattern_id, controller_index)?;
        let mut candidate = self.clone();
        let mut payload = candidate.events[event_index].payload.clone();
        payload.drain(record_offset..record_offset + FLP_PATTERN_CONTROLLER_RECORD_SIZE);
        if payload.is_empty() {
            candidate.events.remove(event_index);
        } else {
            candidate.events[event_index].replace_data_payload(payload)?;
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    fn pattern_controller_record_location(
        &self,
        pattern_id: u16,
        controller_index: usize,
    ) -> Result<(usize, usize, PatternController), FlpError> {
        let mut current_pattern = None;
        let mut matching_pattern_markers = 0usize;
        let mut next_controller_index = 0usize;
        let mut target = None;
        for (event_index, event) in self.events.iter().enumerate() {
            match event.opcode {
                0x41 if event.payload.len() == 2 => {
                    current_pattern =
                        Some(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                    if current_pattern == Some(pattern_id) {
                        matching_pattern_markers += 1;
                    }
                }
                0x40 | 0x62 | 0x63 => current_pattern = None,
                0xDF if current_pattern == Some(pattern_id) => {
                    let (records, remainder) = event
                        .payload
                        .as_chunks::<FLP_PATTERN_CONTROLLER_RECORD_SIZE>();
                    if !remainder.is_empty() {
                        return Err(FlpError::InvalidEvent {
                            offset: event.file_offset,
                            detail: "pattern controller payload is not a whole number of 12-byte records",
                        });
                    }
                    if controller_index < next_controller_index.saturating_add(records.len()) {
                        let record_index = controller_index - next_controller_index;
                        target = Some((
                            event_index,
                            record_index * FLP_PATTERN_CONTROLLER_RECORD_SIZE,
                            PatternController::decode(&records[record_index]),
                        ));
                    }
                    next_controller_index = next_controller_index.saturating_add(records.len());
                }
                _ => {}
            }
        }
        if matching_pattern_markers == 0 {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ));
        }
        if matching_pattern_markers > 1 {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern ID is ambiguous",
            ));
        }
        target.ok_or(FlpError::UnsupportedEdit(
            "the requested Pattern or controller index does not exist",
        ))
    }

    /// Sets or creates a time-signature marker within one Pattern.
    /// Pattern markers are scoped by the `0x41` Pattern ID events and remain separate from
    /// project defaults and Playlist arrangement markers.
    pub fn set_pattern_time_signature(
        &mut self,
        pattern_id: u16,
        position_ticks: u32,
        numerator: u8,
        denominator: u8,
    ) -> Result<usize, FlpError> {
        if numerator == 0 || denominator == 0 {
            return Err(FlpError::UnsupportedEdit(
                "time signature numerator and denominator must be positive",
            ));
        }
        if position_ticks > TIME_MARKER_TICK_MASK {
            return Err(FlpError::UnsupportedEdit(
                "pattern time-signature positions must fit in the low 27 position bits",
            ));
        }

        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let matching_markers = pattern
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature() && marker.position_ticks() == position_ticks)
            .collect::<Vec<_>>();
        if matching_markers.len() > 1 {
            return Err(FlpError::UnsupportedEdit(
                "the Pattern has ambiguous signatures at the requested position",
            ));
        }

        let mut candidate = self.clone();
        if let Some(marker) = matching_markers.first() {
            let position_event_index =
                marker
                    .source_events
                    .position
                    .ok_or(FlpError::UnsupportedEdit(
                        "the selected Pattern time signature has no source position event",
                    ))?;
            let mut insertion_index = position_event_index + 1;
            for event_index in [
                marker.source_events.numerator,
                marker.source_events.denominator,
            ]
            .into_iter()
            .flatten()
            {
                insertion_index = insertion_index.max(event_index + 1);
            }
            if let Some(name_event_index) = marker.source_events.name {
                insertion_index = insertion_index.min(name_event_index);
            }

            let mut insertions = Vec::<(usize, u8, FlpEvent)>::new();
            for (value, event_index, opcode, rank) in [
                (numerator, marker.source_events.numerator, 0x21, 0),
                (denominator, marker.source_events.denominator, 0x22, 1),
            ] {
                if let Some(event_index) = event_index {
                    candidate.events[event_index].replace_byte_payload(value)?;
                } else {
                    insertions.push((insertion_index, rank, FlpEvent::new_byte(opcode, value)));
                }
            }
            insertions
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
            for (event_index, _, event) in insertions {
                candidate.events.insert(event_index, event);
            }
        } else {
            let marker_index = candidate
                .events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.opcode == 0x41
                        && event.payload.len() == 2
                        && event.payload.as_slice() == pattern_id.to_le_bytes()
                })
                .map(|(index, _)| index)
                .next_back()
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested Pattern has no source marker",
                ))?;
            let region_start = marker_index + 1;
            let region_end = candidate.events[region_start..]
                .iter()
                .position(|event| {
                    event.opcode == 0x41 || matches!(event.opcode, 0x40 | 0x62 | 0x63)
                })
                .map_or(candidate.events.len(), |offset| region_start + offset);
            let later_signature = pattern
                .time_markers
                .iter()
                .filter(|marker| marker.is_signature() && marker.position_ticks() > position_ticks)
                .filter_map(|marker| marker.source_events.position)
                .find(|event_index| *event_index >= region_start && *event_index < region_end);
            let last_marker_end = pattern
                .time_markers
                .iter()
                .flat_map(|marker| {
                    [
                        marker.source_events.position,
                        marker.source_events.numerator,
                        marker.source_events.denominator,
                        marker.source_events.name,
                    ]
                })
                .flatten()
                .filter(|event_index| *event_index >= region_start && *event_index < region_end)
                .max()
                .map(|event_index| event_index + 1);
            let insertion_index = later_signature.or(last_marker_end).unwrap_or(region_end);
            let raw_position = position_ticks | TIME_MARKER_SIGNATURE_BIT;
            candidate.events.splice(
                insertion_index..insertion_index,
                [
                    FlpEvent::new_dword(0x94, raw_position),
                    FlpEvent::new_byte(0x21, numerator),
                    FlpEvent::new_byte(0x22, denominator),
                ],
            );
        }
        candidate.refresh_event_offsets()?;
        let marker_index = candidate
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .and_then(|pattern| {
                pattern.time_markers.iter().position(|marker| {
                    marker.is_signature() && marker.position_ticks() == position_ticks
                })
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the updated Pattern time signature could not be decoded",
            ))?;
        *self = candidate;
        Ok(marker_index)
    }

    /// Deletes one Pattern time-signature marker at the requested tick.
    pub fn delete_pattern_time_signature(
        &mut self,
        pattern_id: u16,
        position_ticks: u32,
    ) -> Result<(), FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let matching_markers = pattern
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature() && marker.position_ticks() == position_ticks)
            .collect::<Vec<_>>();
        if matching_markers.len() != 1 {
            return Err(FlpError::UnsupportedEdit(if matching_markers.is_empty() {
                "the requested Pattern time signature does not exist"
            } else {
                "the Pattern has ambiguous signatures at the requested position"
            }));
        }
        let marker = matching_markers[0];
        let source_event_indices = [
            marker.source_events.position,
            marker.source_events.numerator,
            marker.source_events.denominator,
            marker.source_events.name,
        ]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
        if source_event_indices.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern time signature has no source events",
            ));
        }
        let mut candidate = self.clone();
        for event_index in source_event_indices.into_iter().rev() {
            candidate.events.remove(event_index);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
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

    /// Duplicates a pattern's note records, name, and explicit length into a new pattern.
    /// Unknown events and all existing pattern bytes remain untouched.
    pub fn duplicate_pattern(&mut self, pattern_id: u16) -> Result<u16, FlpError> {
        let source_pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut matching_markers = self.events.iter().enumerate().filter(|(_, event)| {
            event.opcode == 0x41
                && event.payload.len() == 2
                && event.payload.as_slice() == pattern_id.to_le_bytes()
        });
        let Some((source_marker_index, _)) = matching_markers.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern marker does not exist",
            ));
        };
        if matching_markers.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern id is ambiguous",
            ));
        }
        let source_note_payload = self
            .events
            .get(source_marker_index + 1)
            .filter(|event| Self::is_pattern_note_event(event))
            .map(|event| event.payload.clone());

        let source_region = &self.events[source_marker_index + 1..];
        let source_region_length = source_region
            .iter()
            .position(|event| {
                (event.opcode == 0x41 && event.payload.len() == 2)
                    || matches!(event.opcode, 0x40 | 0x62 | 0x63)
            })
            .unwrap_or(source_region.len());
        let source_region = &source_region[..source_region_length];
        let name_event_index = source_pattern.name.as_ref().and_then(|_| {
            source_region.iter().position(|event| {
                event.opcode == 0xC1
                    && decode_project_string(&event.payload, self.project_version.as_deref())
                        .is_some_and(|name| !name.is_empty())
            })
        });
        let length_event_index = source_pattern.length_ticks.and_then(|_| {
            source_region
                .iter()
                .rposition(|event| event.opcode == 0xA4 && event.payload.len() == 4)
        });
        let metadata_events = source_region
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                Some(*index) == name_event_index || Some(*index) == length_event_index
            })
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();

        let mut candidate = self.clone();
        let new_pattern_id = candidate.create_pattern()?;
        let new_marker_index = candidate
            .events
            .iter()
            .position(|event| {
                event.opcode == 0x41
                    && event.payload.len() == 2
                    && event.payload.as_slice() == new_pattern_id.to_le_bytes()
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the duplicate pattern marker could not be found",
            ))?;
        let new_note_event_index = new_marker_index
            .checked_add(1)
            .filter(|index| *index < candidate.events.len())
            .ok_or(FlpError::UnsupportedEdit(
                "the duplicate pattern note event could not be found",
            ))?;
        if !Self::is_pattern_note_event(&candidate.events[new_note_event_index]) {
            return Err(FlpError::UnsupportedEdit(
                "the duplicate pattern note event could not be found",
            ));
        }
        if let Some(payload) = source_note_payload {
            candidate.events[new_note_event_index].replace_data_payload(payload)?;
        }
        if !metadata_events.is_empty() {
            let insert_index = new_marker_index
                .checked_add(2)
                .filter(|index| *index <= candidate.events.len())
                .ok_or(FlpError::LengthOverflow)?;
            candidate
                .events
                .splice(insert_index..insert_index, metadata_events);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_pattern_id)
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
        let mut current_pattern = false;
        let mut pending_time_markers = Vec::new();
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);

        for (event_index, event) in self.events.iter().enumerate() {
            match event.opcode {
                0x41 if event.payload.len() == 2 => {
                    current_pattern = true;
                    current_time_marker = None;
                }
                0x40 => {
                    current_pattern = false;
                    current_time_marker = None;
                }
                0x63 if event.payload.len() == 2 => {
                    current_pattern = false;
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
                    current_pattern = false;
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
                0x94 if event.payload.len() == 4 && !current_pattern => {
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
                enabled: match event.payload[12] {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                },
                grouped: match event.payload[46] {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                },
                state_bytes: event.payload.clone(),
            });
        }
        tracks
    }

    /// Edits the observed mute and grouping fields of an existing Playlist track in place.
    /// All other track and project event bytes are preserved.
    pub fn edit_playlist_track(
        &mut self,
        track_id: u32,
        edit: PlaylistTrackEdit,
    ) -> Result<(), FlpError> {
        if track_id == 0 || track_id > 500 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track id must be between 1 and 500",
            ));
        }
        if edit.grouped == Some(true) && track_id == 1 {
            return Err(FlpError::UnsupportedEdit(
                "the first Playlist track cannot be grouped with a track above it",
            ));
        }
        if edit.enabled.is_none() && edit.grouped.is_none() {
            return Ok(());
        }

        let mut matching_events = self.events.iter().enumerate().filter_map(|(index, event)| {
            if event.opcode != 0xEE || event.payload.len() != 70 {
                return None;
            }
            let id = u32::from_le_bytes([
                event.payload[0],
                event.payload[1],
                event.payload[2],
                event.payload[3],
            ]);
            (id == track_id).then_some(index)
        });
        let Some(event_index) = matching_events.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist track does not exist",
            ));
        };
        if matching_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist track id is ambiguous",
            ));
        }

        if let Some(enabled) = edit.enabled {
            write_event_payload_bytes(&mut self.events[event_index], 12, &[u8::from(enabled)])?;
        }
        if let Some(grouped) = edit.grouped {
            write_event_payload_bytes(&mut self.events[event_index], 46, &[u8::from(grouped)])?;
        }
        Ok(())
    }

    /// Returns Mixer insert summaries for the observed adjacent `0x9A`, `0x93`, `0x95`
    /// record signature. This is intentionally read-only; the unparsed insert and effect data
    /// remains available byte-for-byte through `events()`.
    pub fn mixer_inserts(&self) -> Vec<MixerInsertSummary> {
        let flag_opcodes: &[u8] = match self
            .project_version
            .as_deref()
            .and_then(|version| version.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
        {
            Some(major) if major >= 25 => &[0xEC],
            Some(_) => &[0xDC],
            None => &[0xEC, 0xDC],
        };
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
                let mut flags_records = insert_events
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| {
                        flag_opcodes.contains(&event.opcode) && event.payload.len() == 12
                    })
                    .map(|(offset, event)| {
                        (
                            start + offset,
                            u32::from_le_bytes(
                                event.payload[4..8]
                                    .try_into()
                                    .expect("Mixer insert flags have four bytes"),
                            ),
                        )
                    });
                let flags_record = flags_records
                    .next()
                    .filter(|_| flags_records.next().is_none());

                MixerInsertSummary {
                    ordinal,
                    input_raw: read_i32(0),
                    output_raw: read_i32(1),
                    color_raw,
                    icon_raw,
                    name,
                    flags: flags_record.map(|(_, flags)| flags),
                    flags_event_index: flags_record.map(|(event_index, _)| event_index),
                    first_event_index: start,
                    end_event_index: end,
                }
            })
            .collect()
    }

    /// Edits known bits in one existing Mixer insert flag event while preserving
    /// unrecognized bits, reserved bytes, and all other source events.
    pub fn edit_mixer_insert_flags(
        &mut self,
        insert_ordinal: usize,
        edit: MixerInsertEdit,
    ) -> Result<(), FlpError> {
        let insert = self
            .mixer_inserts()
            .into_iter()
            .find(|insert| insert.ordinal() == insert_ordinal)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer insert does not exist",
            ))?;
        let Some(mut flags) = insert.flags else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Mixer insert has no unique recognized flags event",
            ));
        };
        let event_index = insert.flags_event_index.ok_or(FlpError::UnsupportedEdit(
            "the requested Mixer insert has no unique recognized flags event",
        ))?;
        for (bit, value) in [
            (3, edit.enabled),
            (12, edit.soloed),
            (0, edit.polarity_reversed),
            (1, edit.swap_left_right),
            (2, edit.effects_enabled),
        ] {
            if let Some(value) = value {
                let mask = 1_u32 << bit;
                if value {
                    flags |= mask;
                } else {
                    flags &= !mask;
                }
            }
        }
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer insert flags event does not exist",
            ))?;
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) || event.payload.len() != 12 {
            return Err(FlpError::UnsupportedEdit(
                "the requested Mixer insert flags event has an unsupported layout",
            ));
        }
        write_event_payload_bytes(&mut self.events[event_index], 4, &flags.to_le_bytes())
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

    /// Creates a Pattern Clip by copying a recognized Playlist clip record in the arrangement.
    /// Its position, item index, length, track, and (when present) clip ID are assigned; all
    /// other record bytes come from the existing template and remain opaque.
    pub fn create_playlist_pattern_clip(
        &mut self,
        arrangement_id: u16,
        pattern_id: u16,
        position_ticks: u32,
        length_ticks: u32,
        track_index: u16,
    ) -> Result<usize, FlpError> {
        if length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Pattern Clip length must be greater than zero",
            ));
        }
        if track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

        let patterns = self.patterns()?;
        let mut matching_patterns = patterns.iter().filter(|pattern| pattern.id == pattern_id);
        if matching_patterns.next().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ));
        }
        if matching_patterns.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern id is ambiguous",
            ));
        }

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
        let Some((template_index, template)) = arrangement
            .clips
            .iter()
            .enumerate()
            .find(|(_, clip)| matches!(clip.target(), PlaylistClipTarget::Pattern { .. }))
            .or_else(|| arrangement.clips.iter().enumerate().next())
        else {
            return Err(FlpError::UnsupportedEdit(
                "the arrangement has no recognized Playlist clip record to use as a template",
            ));
        };

        let item_index = template
            .pattern_base
            .checked_add(pattern_id)
            .ok_or(FlpError::LengthOverflow)?;
        let record_size = template.record_size;
        let record_start = template
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = template.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip template event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip template is not in a length-prefixed data event",
            ));
        }
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip template does not fit its event payload",
            ));
        }

        let raw_track_index = 499u16 - track_index;
        let mut record = event.payload[record_start..record_end].to_vec();
        record[0..4].copy_from_slice(&position_ticks.to_le_bytes());
        record[6..8].copy_from_slice(&item_index.to_le_bytes());
        record[8..12].copy_from_slice(&length_ticks.to_le_bytes());
        record[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        if record_size >= 60 {
            record[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.splice(record_end..record_end, record);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(template_index.saturating_add(1))
    }

    /// Creates an Audio Clip by copying a recognized Playlist clip record.
    ///
    /// The requested channel must be an Audio Clip channel. Position, channel target, length,
    /// track, and (when present) clip ID are assigned; all other record bytes remain copied from
    /// the template.
    pub fn create_playlist_audio_clip(
        &mut self,
        arrangement_id: u16,
        channel_id: u16,
        position_ticks: u32,
        length_ticks: u32,
        track_index: u16,
    ) -> Result<usize, FlpError> {
        if length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clip length must be greater than zero",
            ));
        }
        if track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

        let channels = self.channels();
        let mut matching_channels = channels.iter().filter(|channel| channel.id() == channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the requested channel is not one unambiguous Audio Clip channel",
            ));
        }
        if channel.sample_path().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Audio Clip channel has no recognized sample path",
            ));
        }

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
        let channel_ids = channels
            .iter()
            .filter(|channel| channel.kind() == Some(4))
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        let Some((template_index, template)) = arrangement
            .clips
            .iter()
            .enumerate()
            .find(|(_, clip)| {
                matches!(clip.target(), PlaylistClipTarget::Channel { id } if id == channel_id)
            })
            .or_else(|| {
                arrangement.clips.iter().enumerate().find(|(_, clip)| {
                    matches!(clip.target(), PlaylistClipTarget::Channel { id } if channel_ids.contains(&id))
                })
            })
        else {
            return Err(FlpError::UnsupportedEdit(
                "the arrangement has no recognized Audio Clip record to use as a template",
            ));
        };
        if channel_id >= template.pattern_base {
            return Err(FlpError::UnsupportedEdit(
                "the Audio Clip channel ID does not fit the template's channel target range",
            ));
        }

        let record_size = template.record_size;
        let record_start = template
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = template.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip template event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip template is not in a length-prefixed data event",
            ));
        }
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip template does not fit its event payload",
            ));
        }

        let raw_track_index = 499u16 - track_index;
        let mut record = event.payload[record_start..record_end].to_vec();
        record[0..4].copy_from_slice(&position_ticks.to_le_bytes());
        record[6..8].copy_from_slice(&channel_id.to_le_bytes());
        record[8..12].copy_from_slice(&length_ticks.to_le_bytes());
        record[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        if record_size >= 60 {
            record[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.splice(record_end..record_end, record);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(template_index.saturating_add(1))
    }

    /// Removes one Playlist clip's complete stored record without rewriting its neighbors.
    pub fn delete_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
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

        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip is not in a length-prefixed data event",
            ));
        }
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip record does not fit its event payload",
            ));
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.drain(record_start..record_end);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Copies a complete Playlist clip record for a later lossless paste.
    pub fn copy_playlist_clip(
        &self,
        arrangement_id: u16,
        clip_index: usize,
    ) -> Result<PlaylistClipClipboard, FlpError> {
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
        let record_start = clip
            .source_record_index
            .checked_mul(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event = self
            .events
            .get(clip.source_event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip is not in a length-prefixed data event",
            ));
        }
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(clip.record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip record does not fit its event payload",
            ));
        }

        Ok(PlaylistClipClipboard {
            raw_record: event.payload[record_start..record_end].to_vec(),
        })
    }

    /// Pastes a copied clip record into an arrangement, updating its position, track, and clip ID
    /// when present. A paste into an empty arrangement can reuse the copied ID.
    /// The complete record is appended to the arrangement's final clip event, or a new clip event
    /// is created at the end of the arrangement when it has no clip event yet.
    pub fn paste_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clipboard: &PlaylistClipClipboard,
        position_ticks: u32,
        raw_track_index: u16,
    ) -> Result<usize, FlpError> {
        let record_size = clipboard.raw_record.len();
        if !FLP_PLAYLIST_RECORD_SIZES.contains(&record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the copied Playlist clip has an unsupported record size",
            ));
        }
        if raw_track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

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
        let insertion_clip_index = arrangement.clips.len();
        let established_record_size = arrangement.clips.last().map(|clip| clip.record_size);

        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);
        let (region_start, region_end) = if has_arrangement_markers {
            let mut marker_indices = self
                .events
                .iter()
                .enumerate()
                .filter(|(_, event)| event.opcode == 0x63 && event.payload.len() == 2)
                .filter(|(_, event)| {
                    u16::from_le_bytes([event.payload[0], event.payload[1]]) == arrangement_id
                })
                .map(|(index, _)| index);
            let Some(marker_index) = marker_indices.next() else {
                return Err(FlpError::UnsupportedEdit(
                    "the requested arrangement does not exist",
                ));
            };
            if marker_indices.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "the requested arrangement id is ambiguous",
                ));
            }
            let region_end = self
                .events
                .iter()
                .enumerate()
                .skip(marker_index + 1)
                .find(|(_, event)| {
                    event.opcode == 0x62 || (event.opcode == 0x63 && event.payload.len() == 2)
                })
                .map_or(self.events.len(), |(index, _)| index);
            (marker_index + 1, region_end)
        } else {
            (0, self.events.len())
        };
        let target_event_index = self.events[region_start..region_end]
            .iter()
            .rposition(|event| event.opcode == 0xE9)
            .map(|offset| region_start + offset);

        let mut record = clipboard.raw_record.clone();
        record[0..4].copy_from_slice(&position_ticks.to_le_bytes());
        record[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        if record_size >= 60 {
            let preferred_id = u32::from_le_bytes(record[32..36].try_into().unwrap());
            record[32..36].copy_from_slice(
                &next_playlist_clip_id(arrangement, Some(preferred_id))?.to_le_bytes(),
            );
        }

        let mut candidate = self.clone();
        if let Some(event_index) = target_event_index {
            let event = &candidate.events[event_index];
            if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                return Err(FlpError::UnsupportedEdit(
                    "the target Playlist clip event is not length-prefixed data",
                ));
            }
            let target_record_size = if let Some(record_size) = established_record_size {
                record_size
            } else if event.payload.is_empty() {
                record_size
            } else {
                playlist_clip_record_size(
                    candidate.project_version.as_deref(),
                    &event.payload,
                    event.file_offset,
                )?
            };
            if target_record_size != record_size {
                return Err(FlpError::UnsupportedEdit(
                    "the copied clip record layout does not match the target arrangement",
                ));
            }
            if !event.payload.len().is_multiple_of(record_size) {
                return Err(FlpError::InvalidEvent {
                    offset: event.file_offset,
                    detail: "playlist clip payload is not a whole number of supported records",
                });
            }
            let mut payload = event.payload.clone();
            payload.extend_from_slice(&record);
            candidate.events[event_index].replace_data_payload(payload)?;
        } else {
            let event = FlpEvent::new_data(0xE9, record)?;
            candidate.events.insert(region_end, event);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(insertion_clip_index)
    }

    /// Duplicates one clip's complete stored record into the same Playlist event, assigning a new
    /// ID when the record layout contains one. The duplicate is inserted immediately after the
    /// source record.
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
        if record_size >= 60 {
            duplicate[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }
        let mut payload = event.payload.clone();
        payload.splice(record_end..record_end, duplicate);
        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(clip_index.saturating_add(1))
    }

    /// Splits one un-stretched Audio Clip at a Playlist tick while preserving its source window.
    /// The clip is split only when its source offsets can be mapped linearly to timeline time.
    /// `full_source_length_ms` is needed only for clips whose two offsets are the `-1` sentinel.
    /// The right-hand clip gets a new ID when the record layout contains one and is inserted
    /// immediately after the source record.
    pub fn split_playlist_audio_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        split_position_ticks: u32,
        full_source_length_ms: Option<f32>,
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
        let PlaylistClipTarget::Channel { id: channel_id } = clip.target() else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Audio Clips can be split",
            ));
        };
        let channel_summaries = self.channels();
        let mut matching_channels = channel_summaries
            .iter()
            .filter(|channel| channel.id() == channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip channel does not exist",
            ));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip does not target one unambiguous Audio Clip channel",
            ));
        }

        let relative_split_ticks = split_position_ticks.checked_sub(clip.position_ticks);
        let Some(relative_split_ticks) =
            relative_split_ticks.filter(|ticks| *ticks > 0 && *ticks < clip.length_ticks)
        else {
            return Err(FlpError::UnsupportedEdit(
                "the split point must fall inside the Playlist Audio Clip",
            ));
        };
        let tempo_channel_ids = channel_summaries
            .iter()
            .filter(|channel| {
                channel.kind() == Some(5)
                    && channel
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks <= split_position_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "splitting an Audio Clip across Playlist tempo automation is unsupported",
            ));
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || scale <= 0.0 || (scale - 1.0).abs() > 1e-9)
        {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clips with a non-default or invalid scale cannot be split",
            ));
        }

        let (source_start_ms, source_end_ms) =
            if clip.start_offset == -1.0 && clip.end_offset == -1.0 {
                let Some(source_length_ms) = full_source_length_ms else {
                    return Err(FlpError::UnsupportedEdit(
                        "splitting a full-source Audio Clip requires its sample length",
                    ));
                };
                if !source_length_ms.is_finite() || source_length_ms <= 0.0 {
                    return Err(FlpError::UnsupportedEdit(
                        "the full sample length must be a positive number of milliseconds",
                    ));
                }
                (0.0, source_length_ms)
            } else if clip.start_offset.is_finite()
                && clip.end_offset.is_finite()
                && clip.start_offset >= 0.0
                && clip.end_offset > clip.start_offset
            {
                (clip.start_offset, clip.end_offset)
            } else {
                return Err(FlpError::UnsupportedEdit(
                    "the Audio Clip has an unsupported source window",
                ));
            };

        let ppq = f64::from(self.header.ppq);
        let tempo_bpm = self.metadata.tempo_bpm().unwrap_or(140.0);
        if ppq == 0.0 || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            return Err(FlpError::UnsupportedEdit(
                "splitting an Audio Clip requires a valid project tempo and PPQ",
            ));
        }
        let elapsed_source_ms = f64::from(relative_split_ticks) * 60_000.0 / (ppq * tempo_bpm);
        let split_source_ms = (f64::from(source_start_ms) + elapsed_source_ms) as f32;
        if !split_source_ms.is_finite()
            || split_source_ms <= source_start_ms
            || split_source_ms >= source_end_ms
        {
            return Err(FlpError::UnsupportedEdit(
                "the split point must fall inside the Audio Clip's available source audio",
            ));
        }

        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip record does not fit its event payload",
            ));
        }

        let left_length = relative_split_ticks;
        let right_position = split_position_ticks;
        let right_length = clip.length_ticks - relative_split_ticks;
        let mut left_record = event.payload[record_start..record_end].to_vec();
        let mut right_record = left_record.clone();
        left_record[8..12].copy_from_slice(&left_length.to_le_bytes());
        left_record[24..28].copy_from_slice(&source_start_ms.to_le_bytes());
        left_record[28..32].copy_from_slice(&split_source_ms.to_le_bytes());
        right_record[0..4].copy_from_slice(&right_position.to_le_bytes());
        right_record[8..12].copy_from_slice(&right_length.to_le_bytes());
        right_record[24..28].copy_from_slice(&split_source_ms.to_le_bytes());
        right_record[28..32].copy_from_slice(&source_end_ms.to_le_bytes());
        if record_size >= 60 {
            right_record[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.splice(record_start..record_end, left_record);
        let right_record_start = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        payload.splice(right_record_start..right_record_start, right_record);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(clip_index.saturating_add(1))
    }

    /// Joins two contiguous, un-stretched Audio Clips that play adjacent source windows.
    /// The left record is kept and extended; the right record is removed. Both clips must be
    /// in the same Playlist data event and have matching opaque clip fields, excluding their IDs.
    pub fn join_adjacent_playlist_audio_clips(
        &mut self,
        arrangement_id: u16,
        left_clip_index: usize,
        right_clip_index: usize,
    ) -> Result<usize, FlpError> {
        if left_clip_index == right_clip_index {
            return Err(FlpError::UnsupportedEdit(
                "joining requires two different Playlist Audio Clips",
            ));
        }
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
        let Some(left_clip) = arrangement.clips.get(left_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the left Playlist Audio Clip does not exist",
            ));
        };
        let Some(right_clip) = arrangement.clips.get(right_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the right Playlist Audio Clip does not exist",
            ));
        };
        let (
            PlaylistClipTarget::Channel {
                id: left_channel_id,
            },
            PlaylistClipTarget::Channel {
                id: right_channel_id,
            },
        ) = (left_clip.target(), right_clip.target())
        else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Audio Clips can be joined",
            ));
        };
        if left_channel_id != right_channel_id {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must use the same sample channel",
            ));
        }
        let channels = self.channels();
        let mut matching_channels = channels
            .iter()
            .filter(|channel| channel.id() == left_channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip channel does not exist",
            ));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clips do not target one unambiguous Audio Clip channel",
            ));
        }
        if left_clip.raw_track_index != right_clip.raw_track_index {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must be on the same Playlist track",
            ));
        }
        if left_clip.record_size != right_clip.record_size
            || left_clip.source_event_index != right_clip.source_event_index
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must use the same Playlist record event and layout",
            ));
        }
        if left_clip.pattern_base != right_clip.pattern_base
            || left_clip.item_index != right_clip.item_index
            || left_clip.track_index != right_clip.track_index
            || left_clip.group != right_clip.group
            || left_clip.unknown_word != right_clip.unknown_word
            || left_clip.item_flags != right_clip.item_flags
            || left_clip.header_bytes != right_clip.header_bytes
            || left_clip.reserved != right_clip.reserved
            || left_clip.scale != right_clip.scale
            || left_clip.trailing_bytes != right_clip.trailing_bytes
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must have matching clip properties",
            ));
        }
        let default_scale = |clip: &PlaylistClip| {
            clip.scale
                .is_none_or(|scale| scale.is_finite() && (scale - 1.0).abs() <= 1e-9)
        };
        if !default_scale(left_clip) || !default_scale(right_clip) {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clips with a non-default or invalid scale cannot be joined",
            ));
        }
        let left_end_ticks = left_clip
            .position_ticks
            .checked_add(left_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        if left_clip.length_ticks == 0 || right_clip.length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must have nonzero timeline lengths",
            ));
        }
        if left_end_ticks != right_clip.position_ticks {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must touch on the Playlist timeline",
            ));
        }
        if !left_clip.start_offset.is_finite()
            || !left_clip.end_offset.is_finite()
            || !right_clip.start_offset.is_finite()
            || !right_clip.end_offset.is_finite()
            || left_clip.start_offset < 0.0
            || right_clip.start_offset < 0.0
            || left_clip.end_offset <= left_clip.start_offset
            || right_clip.end_offset <= right_clip.start_offset
            || left_clip.end_offset.to_bits() != right_clip.start_offset.to_bits()
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must have contiguous, supported sample windows",
            ));
        }
        let merged_end_ticks = right_clip
            .position_ticks
            .checked_add(right_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let merged_length_ticks = merged_end_ticks
            .checked_sub(left_clip.position_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let tempo_channel_ids = channels
            .iter()
            .filter(|candidate| {
                candidate.kind() == Some(5)
                    && candidate
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks < merged_end_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > left_clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "joining Audio Clips across Playlist tempo automation is unsupported",
            ));
        }

        let record_size = left_clip.record_size;
        let left_record_start = left_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let left_record_end = left_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_start = right_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_end = right_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = left_clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || left_record_end > event.payload.len()
            || right_record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip records do not fit their event payload",
            ));
        }
        if left_record_start == right_record_start {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip records are not distinct",
            ));
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload[left_record_start + 8..left_record_start + 12]
            .copy_from_slice(&merged_length_ticks.to_le_bytes());
        payload[left_record_start + 28..left_record_start + 32]
            .copy_from_slice(&right_clip.end_offset.to_le_bytes());
        payload.drain(right_record_start..right_record_end);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;

        Ok(if right_clip_index < left_clip_index {
            left_clip_index - 1
        } else {
            left_clip_index
        })
    }

    /// Joins adjacent Pattern Clips that repeat the same pattern without changing its playback.
    /// The left record is kept and extended; the right record is removed. The left clip length
    /// must end on a pattern repeat boundary, and notes may not cross that boundary. Clip IDs
    /// may differ and the left clip's ID is kept.
    pub fn join_adjacent_playlist_pattern_clips(
        &mut self,
        arrangement_id: u16,
        left_clip_index: usize,
        right_clip_index: usize,
    ) -> Result<usize, FlpError> {
        if left_clip_index == right_clip_index {
            return Err(FlpError::UnsupportedEdit(
                "joining requires two different Playlist Pattern Clips",
            ));
        }
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
        let Some(left_clip) = arrangement.clips.get(left_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the left Playlist Pattern Clip does not exist",
            ));
        };
        let Some(right_clip) = arrangement.clips.get(right_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the right Playlist Pattern Clip does not exist",
            ));
        };
        let (
            PlaylistClipTarget::Pattern {
                id: left_pattern_id,
            },
            PlaylistClipTarget::Pattern {
                id: right_pattern_id,
            },
        ) = (left_clip.target(), right_clip.target())
        else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Pattern Clips can be joined",
            ));
        };
        if left_pattern_id != right_pattern_id {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must reference the same pattern",
            ));
        }
        if left_clip.raw_track_index != right_clip.raw_track_index {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must be on the same Playlist track",
            ));
        }
        if left_clip.record_size != right_clip.record_size
            || left_clip.source_event_index != right_clip.source_event_index
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must use the same Playlist record event and layout",
            ));
        }
        if left_clip.pattern_base != right_clip.pattern_base
            || left_clip.item_index != right_clip.item_index
            || left_clip.track_index != right_clip.track_index
            || left_clip.group != right_clip.group
            || left_clip.unknown_word != right_clip.unknown_word
            || left_clip.item_flags != right_clip.item_flags
            || left_clip.header_bytes != right_clip.header_bytes
            || left_clip.start_offset.to_bits() != right_clip.start_offset.to_bits()
            || left_clip.end_offset.to_bits() != right_clip.end_offset.to_bits()
            || left_clip.reserved != right_clip.reserved
            || left_clip.scale.map(f64::to_bits) != right_clip.scale.map(f64::to_bits)
            || left_clip.trailing_bytes != right_clip.trailing_bytes
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must have matching clip properties",
            ));
        }
        if left_clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            return Err(FlpError::UnsupportedEdit(
                "Pattern Clips with a non-default or invalid scale cannot be joined",
            ));
        }
        if left_clip.length_ticks == 0 || right_clip.length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must have nonzero timeline lengths",
            ));
        }
        let left_end_ticks = left_clip
            .position_ticks
            .checked_add(left_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        if left_end_ticks != right_clip.position_ticks {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must touch on the Playlist timeline",
            ));
        }
        let merged_end_ticks = right_clip
            .position_ticks
            .checked_add(right_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let merged_length_ticks = merged_end_ticks
            .checked_sub(left_clip.position_ticks)
            .ok_or(FlpError::LengthOverflow)?;

        let pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == left_pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip pattern does not exist",
            ))?;
        let inferred_length = pattern
            .notes
            .iter()
            .filter(|note| note.length > 0)
            .map(|note| u64::from(note.position) + u64::from(note.length))
            .max()
            .unwrap_or(0);
        let repeat_length = pattern
            .length_ticks
            .map(u64::from)
            .filter(|length| *length > 0)
            .or_else(|| (inferred_length > 0).then_some(inferred_length))
            .ok_or(FlpError::UnsupportedEdit(
                "joining Pattern Clips requires a known nonzero pattern repeat length",
            ))?;
        if !u64::from(left_clip.length_ticks).is_multiple_of(repeat_length) {
            return Err(FlpError::UnsupportedEdit(
                "the left Pattern Clip must end on a pattern repeat boundary",
            ));
        }
        if pattern.notes.iter().any(|note| {
            u64::from(note.position) >= repeat_length
                || (note.length > 0
                    && u64::from(note.position) % repeat_length + u64::from(note.length)
                        > repeat_length)
        }) {
            return Err(FlpError::UnsupportedEdit(
                "joining these Pattern Clips would let a note cross the clip boundary",
            ));
        }

        let channels = self.channels();
        let tempo_channel_ids = channels
            .iter()
            .filter(|candidate| {
                candidate.kind() == Some(5)
                    && candidate
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks < merged_end_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > left_clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "joining Pattern Clips across Playlist tempo automation is unsupported",
            ));
        }

        let record_size = left_clip.record_size;
        let left_record_start = left_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let left_record_end = left_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_start = right_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_end = right_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = left_clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || left_record_end > event.payload.len()
            || right_record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip records do not fit their event payload",
            ));
        }
        if left_record_start == right_record_start {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip records are not distinct",
            ));
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload[left_record_start + 8..left_record_start + 12]
            .copy_from_slice(&merged_length_ticks.to_le_bytes());
        payload.drain(right_record_start..right_record_end);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;

        Ok(if right_clip_index < left_clip_index {
            left_clip_index - 1
        } else {
            left_clip_index
        })
    }

    fn pattern_score_for_playlist_merge(
        &self,
        pattern_id: u16,
    ) -> Result<(Vec<PatternNote>, Option<u8>), FlpError> {
        let mut matching_markers = self.events.iter().enumerate().filter(|(_, event)| {
            event.opcode == 0x41
                && event.encoding == PayloadEncoding::Word
                && event.payload.len() == 2
                && event.payload.as_slice() == pattern_id.to_le_bytes()
        });
        let Some((marker_index, _)) = matching_markers.next() else {
            return Err(FlpError::UnsupportedEdit(
                "a selected Pattern Clip references a missing pattern",
            ));
        };
        if matching_markers.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "a selected Pattern Clip references an ambiguous pattern",
            ));
        }

        let region_start = marker_index + 1;
        let region_end = self.events[region_start..]
            .iter()
            .position(|event| {
                (event.opcode == 0x41 && event.payload.len() == 2)
                    || matches!(event.opcode, 0x40 | 0x62 | 0x63)
            })
            .map_or(self.events.len(), |offset| region_start + offset);
        let note_event_index = self
            .events
            .get(region_start)
            .filter(|event| Self::is_pattern_note_event(event))
            .map(|_| region_start);
        let mut notes = Vec::new();
        let mut note_opcode = None;
        let mut length_event_count = 0usize;

        for event_index in region_start..region_end {
            let event = &self.events[event_index];
            if Some(event_index) == note_event_index {
                if !matches!(event.encoding, PayloadEncoding::Data { .. })
                    || !event.payload.len().is_multiple_of(FLP_NOTE_RECORD_SIZE)
                {
                    return Err(FlpError::UnsupportedEdit(
                        "a selected pattern has an unsupported score event layout",
                    ));
                }
                note_opcode = Some(event.opcode);
                notes.extend(
                    event
                        .payload
                        .as_chunks::<FLP_NOTE_RECORD_SIZE>()
                        .0
                        .iter()
                        .map(|record| PatternNote::decode(record)),
                );
                continue;
            }

            match event.opcode {
                0xC1 if matches!(event.encoding, PayloadEncoding::Data { .. }) => {}
                0xA4 if event.encoding == PayloadEncoding::Dword && event.payload.len() == 4 => {
                    length_event_count += 1;
                }
                _ => {
                    return Err(FlpError::UnsupportedEdit(
                        "selected patterns containing event automation or unknown pattern data cannot be merged",
                    ));
                }
            }
        }

        if length_event_count > 1 {
            return Err(FlpError::UnsupportedEdit(
                "a selected pattern has multiple explicit length events",
            ));
        }

        Ok((notes, note_opcode))
    }

    /// Merges selected Pattern Clip scores into a new pattern assigned to the uppermost clip.
    /// The output clip spans the selected timeline range; repeated source scores are expanded
    /// into that range and clipped at each source clip's endpoint. Original patterns remain
    /// unchanged. Pattern event automation, unknown per-pattern data, and scaled clips are
    /// rejected because their score timing or payload is not yet modeled.
    pub fn merge_playlist_pattern_clips(
        &mut self,
        arrangement_id: u16,
        clip_indices: &[usize],
    ) -> Result<usize, FlpError> {
        let mut selected_indices = BTreeSet::new();
        for &clip_index in clip_indices {
            if !selected_indices.insert(clip_index) {
                return Err(FlpError::UnsupportedEdit(
                    "selected Playlist clip indexes must be unique",
                ));
            }
        }
        if selected_indices.len() < 2 {
            return Err(FlpError::UnsupportedEdit(
                "merging requires at least two Playlist Pattern Clips",
            ));
        }

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

        let selected_indices = selected_indices.into_iter().collect::<Vec<_>>();
        let mut selected_clips = Vec::with_capacity(selected_indices.len());
        let mut merged_start = u64::MAX;
        let mut merged_end = 0u64;
        for &clip_index in &selected_indices {
            let Some(clip) = arrangement.clips.get(clip_index) else {
                return Err(FlpError::UnsupportedEdit(
                    "a selected Playlist clip does not exist",
                ));
            };
            if !matches!(clip.target(), PlaylistClipTarget::Pattern { .. }) {
                return Err(FlpError::UnsupportedEdit(
                    "only Playlist Pattern Clips can be merged",
                ));
            }
            if clip.track_index.is_none() {
                return Err(FlpError::UnsupportedEdit(
                    "a selected Pattern Clip has an unsupported Playlist track index",
                ));
            }
            if clip
                .scale
                .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
            {
                return Err(FlpError::UnsupportedEdit(
                    "Pattern Clips with a non-default or invalid scale cannot be merged",
                ));
            }
            if clip.length_ticks == 0 {
                return Err(FlpError::UnsupportedEdit(
                    "selected Pattern Clips must have nonzero timeline lengths",
                ));
            }
            let clip_start = u64::from(clip.position_ticks);
            let clip_end = clip_start
                .checked_add(u64::from(clip.length_ticks))
                .ok_or(FlpError::LengthOverflow)?;
            merged_start = merged_start.min(clip_start);
            merged_end = merged_end.max(clip_end);
            selected_clips.push((clip_index, clip.clone()));
        }
        let merged_length_ticks = u32::try_from(
            merged_end
                .checked_sub(merged_start)
                .ok_or(FlpError::LengthOverflow)?,
        )
        .map_err(|_| FlpError::UnsupportedEdit("the merged Playlist range is too long"))?;
        if merged_length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "the merged Playlist range must be nonzero",
            ));
        }

        let receiver_clip_index = selected_clips
            .iter()
            .min_by_key(|(clip_index, clip)| {
                (
                    clip.track_index.unwrap_or(u16::MAX),
                    clip.position_ticks,
                    *clip_index,
                )
            })
            .map(|(clip_index, _)| *clip_index)
            .expect("at least two selected clips were checked above");
        let receiver_clip = selected_clips
            .iter()
            .find(|(clip_index, _)| *clip_index == receiver_clip_index)
            .map(|(_, clip)| clip)
            .expect("the receiver is one of the selected clips");
        let PlaylistClipTarget::Pattern {
            id: receiver_pattern_id,
        } = receiver_clip.target()
        else {
            unreachable!("all selected clips were checked as Pattern Clips");
        };

        let patterns = self.patterns()?;
        let mut scores_by_pattern = HashMap::<u16, (Vec<PatternNote>, Option<u8>)>::new();
        for (_, clip) in &selected_clips {
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                unreachable!("all selected clips were checked as Pattern Clips");
            };
            if !patterns.iter().any(|pattern| pattern.id == id) {
                return Err(FlpError::UnsupportedEdit(
                    "a selected Pattern Clip references a missing pattern",
                ));
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = scores_by_pattern.entry(id) {
                entry.insert(self.pattern_score_for_playlist_merge(id)?);
            }
        }

        let receiver_pattern = patterns
            .iter()
            .find(|pattern| pattern.id == receiver_pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the uppermost Pattern Clip references a missing pattern",
            ))?;
        let pattern_name = receiver_pattern
            .name
            .as_deref()
            .unwrap_or("Merged")
            .to_owned();
        let mut merged_notes = Vec::<PatternNote>::new();

        for (_, clip) in &selected_clips {
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                unreachable!("all selected clips were checked as Pattern Clips");
            };
            let pattern = patterns
                .iter()
                .find(|pattern| pattern.id == id)
                .expect("selected pattern ids were checked above");
            let (source_notes, _) = scores_by_pattern
                .get(&id)
                .expect("selected pattern score was decoded above");
            let clip_length = u64::from(clip.length_ticks);
            let inferred_length = source_notes
                .iter()
                .filter(|note| note.length > 0)
                .try_fold(0u64, |end, note| {
                    let note_end = u64::from(note.position)
                        .checked_add(u64::from(note.length))
                        .ok_or(FlpError::LengthOverflow)?;
                    Ok::<_, FlpError>(end.max(note_end))
                })?;
            let repeat_length = pattern
                .length_ticks
                .map(u64::from)
                .filter(|length| *length > 0)
                .unwrap_or(if inferred_length > 0 {
                    inferred_length
                } else {
                    clip_length
                });
            let repetitions = clip_length.div_ceil(repeat_length);
            let clip_offset = u64::from(clip.position_ticks)
                .checked_sub(merged_start)
                .ok_or(FlpError::LengthOverflow)?;

            for note in source_notes {
                if u64::from(note.position) >= clip_length {
                    continue;
                }
                for repetition in 0..repetitions {
                    let source_position = repetition
                        .checked_mul(repeat_length)
                        .and_then(|start| start.checked_add(u64::from(note.position)))
                        .ok_or(FlpError::LengthOverflow)?;
                    if source_position >= clip_length {
                        continue;
                    }
                    if merged_notes.len() >= MAX_MERGED_PATTERN_NOTES {
                        return Err(FlpError::UnsupportedEdit(
                            "merging would expand beyond 2,000,000 Pattern Clip notes",
                        ));
                    }
                    let mut merged_note = note.clone();
                    merged_note.position = u32::try_from(
                        clip_offset
                            .checked_add(source_position)
                            .ok_or(FlpError::LengthOverflow)?,
                    )
                    .map_err(|_| FlpError::LengthOverflow)?;
                    if merged_note.length > 0 {
                        merged_note.length = u32::try_from(
                            u64::from(merged_note.length).min(clip_length - source_position),
                        )
                        .map_err(|_| FlpError::LengthOverflow)?;
                    }
                    merged_notes.push(merged_note);
                }
            }
        }

        let receiver_pattern_base = receiver_clip.pattern_base;
        let new_item_index = receiver_pattern_base;
        let mut candidate = self.clone();
        let merged_pattern_id = candidate.create_pattern()?;
        let merged_item_index = new_item_index
            .checked_add(merged_pattern_id)
            .ok_or(FlpError::LengthOverflow)?;
        candidate.add_pattern_notes(merged_pattern_id, &merged_notes)?;
        let name_payload =
            encode_project_string(&pattern_name, candidate.project_strings_use_utf16())?;
        let name_event = FlpEvent::new_data(0xC1, name_payload)?;
        let length_event = FlpEvent::new_dword(0xA4, merged_length_ticks);
        let merged_marker_index = candidate
            .events
            .iter()
            .position(|event| {
                event.opcode == 0x41
                    && event.encoding == PayloadEncoding::Word
                    && event.payload.as_slice() == merged_pattern_id.to_le_bytes()
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the merged Pattern marker could not be found",
            ))?;
        let metadata_insert_index = merged_marker_index
            .checked_add(2)
            .filter(|index| *index <= candidate.events.len())
            .ok_or(FlpError::LengthOverflow)?;
        candidate.events.splice(
            metadata_insert_index..metadata_insert_index,
            [name_event, length_event],
        );
        candidate.refresh_event_offsets()?;

        candidate.edit_playlist_clip(
            arrangement_id,
            receiver_clip_index,
            PlaylistClipEdit {
                position_ticks: Some(
                    u32::try_from(merged_start).map_err(|_| FlpError::LengthOverflow)?,
                ),
                item_index: Some(merged_item_index),
                length_ticks: Some(merged_length_ticks),
                ..PlaylistClipEdit::default()
            },
        )?;
        for clip_index in selected_indices
            .iter()
            .copied()
            .filter(|clip_index| *clip_index != receiver_clip_index)
            .rev()
        {
            candidate.delete_playlist_clip(arrangement_id, clip_index)?;
        }

        let clips_before_receiver = selected_indices
            .iter()
            .filter(|clip_index| **clip_index < receiver_clip_index)
            .count();
        let merged_clip_index = receiver_clip_index - clips_before_receiver;
        *self = candidate;
        Ok(merged_clip_index)
    }

    /// Slips the source window inside one Playlist Audio Clip while keeping its timeline bounds.
    /// The requested offset is in milliseconds; `source_length_ms` bounds the window to the file.
    /// Clips under Playlist tempo automation or with non-default scale are rejected.
    pub fn slip_playlist_audio_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        delta_ms: f64,
        source_length_ms: f32,
    ) -> Result<(), FlpError> {
        if !delta_ms.is_finite() {
            return Err(FlpError::UnsupportedEdit(
                "Audio Clip slip distance must be finite",
            ));
        }
        if !source_length_ms.is_finite() || source_length_ms <= 0.0 {
            return Err(FlpError::UnsupportedEdit(
                "the sample length must be a positive number of milliseconds",
            ));
        }
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
                "the requested Playlist Audio Clip does not exist",
            ));
        };
        let PlaylistClipTarget::Channel { id: channel_id } = clip.target() else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Audio Clips can be slip-edited",
            ));
        };
        let channels = self.channels();
        let mut matching_channels = channels.iter().filter(|channel| channel.id() == channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip channel does not exist",
            ));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip does not target one unambiguous Audio Clip channel",
            ));
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || scale <= 0.0 || (scale - 1.0).abs() > 1e-9)
        {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clips with a non-default or invalid scale cannot be slip-edited",
            ));
        }
        let (source_start_ms, source_end_ms) =
            if clip.start_offset == -1.0 && clip.end_offset == -1.0 {
                (0.0, source_length_ms)
            } else if clip.start_offset.is_finite()
                && clip.end_offset.is_finite()
                && clip.start_offset >= 0.0
                && clip.end_offset > clip.start_offset
            {
                (clip.start_offset, clip.end_offset)
            } else {
                return Err(FlpError::UnsupportedEdit(
                    "the Audio Clip has an unsupported sample window",
                ));
            };
        if source_end_ms > source_length_ms {
            return Err(FlpError::UnsupportedEdit(
                "the Audio Clip sample window extends beyond the sample",
            ));
        }
        let clip_end_ticks = clip
            .position_ticks
            .checked_add(clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let tempo_channel_ids = channels
            .iter()
            .filter(|candidate| {
                candidate.kind() == Some(5)
                    && candidate
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks < clip_end_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "slip-editing an Audio Clip across Playlist tempo automation is unsupported",
            ));
        }
        if delta_ms == 0.0 {
            return Ok(());
        }

        let window_length_ms = f64::from(source_end_ms) - f64::from(source_start_ms);
        let next_start_ms = f64::from(source_start_ms) + delta_ms;
        let next_end_ms = next_start_ms + window_length_ms;
        if next_start_ms < 0.0 || next_end_ms > f64::from(source_length_ms) {
            return Err(FlpError::UnsupportedEdit(
                "the slipped sample window must remain within the source audio",
            ));
        }
        let next_start_ms = next_start_ms as f32;
        let next_end_ms = next_end_ms as f32;
        if !next_start_ms.is_finite()
            || !next_end_ms.is_finite()
            || next_start_ms < 0.0
            || next_end_ms > source_length_ms
            || next_end_ms <= next_start_ms
        {
            return Err(FlpError::UnsupportedEdit(
                "the slipped sample window is not representable in the project",
            ));
        }

        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || record_size < 32
            || record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip record does not fit its event payload",
            ));
        }

        let mut candidate = self.clone();
        write_event_payload_bytes(
            &mut candidate.events[event_index],
            record_start + 24,
            &next_start_ms.to_le_bytes(),
        )?;
        write_event_payload_bytes(
            &mut candidate.events[event_index],
            record_start + 28,
            &next_end_ms.to_le_bytes(),
        )?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
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

    fn edit_pattern_notes_batch(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        edits: &HashMap<usize, PatternNoteEdit>,
    ) -> Result<(), FlpError> {
        if edits.is_empty() {
            return Ok(());
        }

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
            let prefix_length = match &notes_event.encoding {
                PayloadEncoding::Data { length_prefix } => length_prefix.len(),
                _ => {
                    return Err(FlpError::InvalidEvent {
                        offset: notes_event.file_offset,
                        detail: "pattern note event does not have a data payload",
                    });
                }
            };
            if notes_event.wire_bytes.len() != 1 + prefix_length + notes_event.payload.len() {
                return Err(FlpError::InvalidEvent {
                    offset: notes_event.file_offset,
                    detail: "pattern note wire payload does not match its decoded bytes",
                });
            }
            let channel_note_count = notes_event
                .payload
                .as_chunks::<FLP_NOTE_RECORD_SIZE>()
                .0
                .iter()
                .filter(|record| u16::from_le_bytes([record[6], record[7]]) == channel_id)
                .count();
            if edits.keys().any(|index| *index >= channel_note_count) {
                return Err(FlpError::UnsupportedEdit(
                    "the requested pattern, channel, or note index does not exist",
                ));
            }

            let event = &mut self.events[notes_index];
            let mut channel_note_index = 0usize;
            for record_index in 0..event.payload.len() / FLP_NOTE_RECORD_SIZE {
                let byte_offset = record_index * FLP_NOTE_RECORD_SIZE;
                let payload_end = byte_offset + FLP_NOTE_RECORD_SIZE;
                let mut note = PatternNote::decode(&event.payload[byte_offset..payload_end]);
                if note.channel_id != channel_id {
                    continue;
                }
                if let Some(edit) = edits.get(&channel_note_index) {
                    note.apply(edit.clone());
                    note.encode_into(&mut event.payload[byte_offset..payload_end]);
                    let wire_start = 1 + prefix_length + byte_offset;
                    let wire_end = wire_start + FLP_NOTE_RECORD_SIZE;
                    event.wire_bytes[wire_start..wire_end]
                        .copy_from_slice(&event.payload[byte_offset..payload_end]);
                }
                channel_note_index += 1;
            }
            return Ok(());
        }
        Err(FlpError::UnsupportedEdit(
            "the requested pattern or note event does not exist",
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

    /// Scales channel note lengths, using original lengths or legato distances.
    /// Variation adds a seeded bipolar offset relative to the scaled duration.
    pub fn articulate_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: ArticulateOptions,
    ) -> Result<usize, FlpError> {
        self.articulate_pattern_notes_in_scope(pattern_id, channel_id, None, options, false)
    }

    /// Scales selected note lengths; optional selected-only context controls legato boundaries.
    /// Variation adds a seeded bipolar offset relative to the scaled duration.
    pub fn articulate_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: ArticulateOptions,
        only_with_selection: bool,
    ) -> Result<usize, FlpError> {
        self.articulate_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            options,
            only_with_selection,
        )
    }

    fn articulate_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: ArticulateOptions,
        only_with_selection: bool,
    ) -> Result<usize, FlpError> {
        let ArticulateOptions {
            multiplier_percent,
            variation_percent,
            seed,
            use_original_lengths,
            chop_chords,
        } = options;
        if !(10..=100).contains(&multiplier_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Articulate multiplier must be between 10 and 100 percent",
            ));
        }
        if variation_percent > 100 {
            return Err(FlpError::UnsupportedEdit(
                "Articulate variation must be between 0 and 100 percent",
            ));
        }
        if only_with_selection && (note_indices.is_none() || use_original_lengths) {
            return Err(FlpError::UnsupportedEdit(
                "Only with selection requires selected notes and legato lengths",
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
        let mut onset_positions = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| {
                !only_with_selection || note_index_in_scope(*note_index, selected_indices.as_ref())
            })
            .map(|(_, note)| note.position)
            .collect::<Vec<_>>();
        onset_positions.sort_unstable();
        onset_positions.dedup();

        let multiplier = f64::from(multiplier_percent) / 100.0;
        let mut state = if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        };
        let edits = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .filter_map(|(note_index, note)| {
                let next_onset =
                    onset_positions.partition_point(|position| *position <= note.position);
                let next_onset_length = onset_positions
                    .get(next_onset)
                    .map(|position| position.saturating_sub(note.position));
                let base_length = if use_original_lengths {
                    note.length
                } else {
                    next_onset_length.unwrap_or(note.length)
                };
                let scaled_length = if base_length == 0 {
                    0
                } else {
                    (f64::from(base_length) * multiplier)
                        .round()
                        .clamp(1.0, f64::from(u32::MAX)) as u32
                };
                let varied_length = if scaled_length == 0 || variation_percent == 0 {
                    scaled_length
                } else {
                    let range = ((u64::from(scaled_length) * u64::from(variation_percent) + 50)
                        / 100)
                        .min((i32::MAX / 2) as u64) as i32;
                    let offset = randomizer_offset(&mut state, range, false, true);
                    (i64::from(scaled_length) + i64::from(offset)).clamp(1, i64::from(u32::MAX))
                        as u32
                };
                let length = if chop_chords {
                    next_onset_length.map_or(varied_length, |boundary| varied_length.min(boundary))
                } else {
                    varied_length
                };
                (length != note.length).then_some((note_index, length))
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

    /// Rebuilds a channel progression as scale triads, arpeggiates it, then applies levels,
    /// articulation, and key-range fitting as one rollback-safe operation.
    pub fn riff_machine_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: RiffMachineOptions<'_>,
    ) -> Result<usize, FlpError> {
        self.riff_machine_pattern_notes_in_scope(pattern_id, channel_id, None, options)
    }

    /// Runs the Riff Machine only on selected channel-local progression notes.
    pub fn riff_machine_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: RiffMachineOptions<'_>,
    ) -> Result<usize, FlpError> {
        self.riff_machine_pattern_notes_in_scope(
            pattern_id,
            channel_id,
            Some(note_indices),
            options,
        )
    }

    fn riff_machine_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: RiffMachineOptions<'_>,
    ) -> Result<usize, FlpError> {
        if options.scale_root > 11
            || options.scale_intervals.is_empty()
            || options.scale_intervals.first() != Some(&0)
            || options
                .scale_intervals
                .iter()
                .any(|interval| *interval > 11)
            || !options
                .scale_intervals
                .windows(2)
                .all(|intervals| intervals[0] < intervals[1])
        {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine scale intervals must be unique, ordered pitch classes starting at zero",
            ));
        }
        if options.minimum_key > options.maximum_key || options.maximum_key > 127 {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine key range must be ordered and remain within keys 0 through 127",
            ));
        }
        if options.step_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine step must be greater than zero ticks",
            ));
        }
        if !(1..=4).contains(&options.range_octaves) {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine octave range must be between 1 and 4",
            ));
        }
        if !(1..=100).contains(&options.gate_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine gate must be between 1 and 100 percent",
            ));
        }
        if !(10..=100).contains(&options.length_multiplier_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine length multiplier must be between 10 and 100 percent",
            ));
        }
        if options.velocity_variation_percent > 100
            || options.pan_variation_percent > 100
            || options.release_variation_percent > 100
            || options.mod_x_variation_percent > 100
            || options.mod_y_variation_percent > 100
        {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine level variations must be between 0 and 100 percent",
            ));
        }
        if options.pitch_variation_semitones > 24 {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine pitch variation must be between 0 and 24 semitones",
            ));
        }
        if options.groove_start_percent > 100
            || options.groove_sensitivity_percent > 100
            || options.groove_duration_percent > 100
        {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine Groove percentages must be between 0 and 100",
            ));
        }
        let groove_enabled =
            options.groove_start_percent > 0 || options.groove_duration_percent > 0;
        let levels_enabled = options.reset_levels
            || options.velocity_variation_percent > 0
            || options.pan_variation_percent > 0
            || options.release_variation_percent > 0
            || options.mod_x_variation_percent > 0
            || options.mod_y_variation_percent > 0
            || options.pitch_variation_semitones > 0;
        if groove_enabled && options.groove_snap_ticks.is_none_or(|ticks| ticks == 0) {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine Groove needs an enabled Piano roll snap grid",
            ));
        }

        let minimum = i32::from(options.minimum_key);
        let maximum = i32::from(options.maximum_key);
        if !(minimum..=maximum)
            .any(|key| note_pitch_in_scale(key, options.scale_root, options.scale_intervals))
        {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine key range contains no notes from the selected scale",
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
        let target_notes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .map(|(note_index, note)| (note_index, note.clone()))
            .collect::<Vec<_>>();
        if target_notes.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine needs a note progression in the target scope",
            ));
        }

        let channel_note_count = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .count();
        let unaffected_note_count = channel_note_count.saturating_sub(target_notes.len());
        let mut progression = std::collections::BTreeMap::<u32, PatternNote>::new();
        for (_, note) in &target_notes {
            progression
                .entry(note.position)
                .and_modify(|root| {
                    if note.key < root.key {
                        *root = note.clone();
                    }
                })
                .or_insert_with(|| note.clone());
        }

        let snap_up = options.snap_direction != LimitSnapDirection::Down;
        let mut chord_notes = Vec::new();
        for note in progression.into_values().filter(|note| note.length > 0) {
            let pitches = riff_triad_keys(
                note.key,
                options.minimum_key,
                options.maximum_key,
                options.scale_root,
                options.scale_intervals,
                options.wrap_to_bottom,
                snap_up,
            )?;
            if pitches.len() < 2 {
                return Err(FlpError::UnsupportedEdit(
                    "Riff Machine needs at least two distinct chord pitches inside the key range",
                ));
            }
            for key in pitches {
                let mut chord_note = note.clone();
                chord_note.key = key;
                chord_notes.push(chord_note);
            }
            if chord_notes.len() > MAX_MERGED_PATTERN_NOTES {
                return Err(FlpError::UnsupportedEdit(
                    "Riff Machine would create more than two million chord notes",
                ));
            }
        }
        if chord_notes.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine progression notes must have a positive length",
            ));
        }

        let mut updated = self.clone();
        let mut remove_indices = target_notes
            .iter()
            .map(|(note_index, _)| *note_index)
            .collect::<Vec<_>>();
        remove_indices.sort_unstable_by(|left, right| right.cmp(left));
        for note_index in remove_indices {
            updated.delete_pattern_note(pattern_id, channel_id, note_index)?;
        }
        updated.add_pattern_notes(pattern_id, &chord_notes)?;
        let chord_indices =
            (unaffected_note_count..unaffected_note_count + chord_notes.len()).collect::<Vec<_>>();
        let arpeggiated = updated.arpeggiate_pattern_note_selection(
            pattern_id,
            channel_id,
            &chord_indices,
            ArpeggioOptions {
                step_ticks: options.step_ticks,
                range_octaves: options.range_octaves,
                gate_percent: options.gate_percent,
                direction: options.direction,
            },
        )?;
        if arpeggiated == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Riff Machine could not arpeggiate the selected progression",
            ));
        }
        let generated_indices =
            (unaffected_note_count..unaffected_note_count + arpeggiated).collect::<Vec<_>>();
        if options.mirror_horizontal || options.mirror_vertical {
            let generated_index_set = generated_indices.iter().copied().collect::<HashSet<_>>();
            let patterns = updated.patterns()?;
            let pattern = patterns
                .iter()
                .find(|pattern| pattern.id == pattern_id)
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested pattern does not exist",
                ))?;
            let mut generated_notes = pattern
                .notes
                .iter()
                .filter(|note| note.channel_id == channel_id)
                .enumerate()
                .filter(|(note_index, _)| generated_index_set.contains(note_index))
                .map(|(note_index, note)| (note_index, note.position, note.length, note.key))
                .collect::<Vec<_>>();
            if generated_notes.is_empty() {
                return Err(FlpError::UnsupportedEdit(
                    "Riff Machine could not find its generated notes to mirror",
                ));
            }

            let mut mirror_edits = HashMap::new();
            if options.mirror_horizontal && options.preserve_start_times {
                generated_notes.sort_by_key(|(_, position, _, key)| (*position, *key));
                let reversed_keys = generated_notes
                    .iter()
                    .map(|(_, _, _, key)| *key)
                    .rev()
                    .collect::<Vec<_>>();
                for ((note_index, _, _, _), key) in generated_notes.iter().zip(reversed_keys) {
                    mirror_edits
                        .entry(*note_index)
                        .or_insert_with(PatternNoteEdit::default)
                        .key = Some(key);
                }
            }
            if options.mirror_horizontal && !options.preserve_start_times {
                let start = generated_notes
                    .iter()
                    .map(|(_, position, _, _)| u64::from(*position))
                    .min()
                    .unwrap_or(0);
                let extent = generated_notes
                    .iter()
                    .map(|(_, position, length, _)| u64::from(*position) + u64::from(*length))
                    .max()
                    .unwrap_or(start);
                if extent > u64::from(u32::MAX) {
                    return Err(FlpError::LengthOverflow);
                }
                for (note_index, position, length, _) in &generated_notes {
                    let end = u64::from(*position) + u64::from(*length);
                    let mirrored_position = start + extent.saturating_sub(end);
                    let mirrored_position =
                        u32::try_from(mirrored_position).map_err(|_| FlpError::LengthOverflow)?;
                    mirror_edits
                        .entry(*note_index)
                        .or_insert_with(PatternNoteEdit::default)
                        .position = Some(mirrored_position);
                }
            }
            if options.mirror_vertical {
                let minimum = generated_notes
                    .iter()
                    .map(|(_, _, _, key)| *key)
                    .min()
                    .unwrap_or(0);
                let maximum = generated_notes
                    .iter()
                    .map(|(_, _, _, key)| *key)
                    .max()
                    .unwrap_or(minimum);
                for (note_index, _, _, key) in &generated_notes {
                    let horizontally_mirrored_key = mirror_edits
                        .get(note_index)
                        .and_then(|edit| edit.key)
                        .unwrap_or(*key);
                    mirror_edits
                        .entry(*note_index)
                        .or_insert_with(PatternNoteEdit::default)
                        .key = Some(minimum + maximum - horizontally_mirrored_key);
                }
            }
            updated.edit_pattern_notes_batch(pattern_id, channel_id, &mirror_edits)?;
        }
        if levels_enabled {
            updated.apply_riff_machine_levels(
                pattern_id,
                channel_id,
                &generated_indices,
                options,
            )?;
        }
        updated.articulate_pattern_note_selection(
            pattern_id,
            channel_id,
            &generated_indices,
            ArticulateOptions {
                multiplier_percent: options.length_multiplier_percent,
                variation_percent: 0,
                seed: options.seed,
                use_original_lengths: true,
                chop_chords: false,
            },
            false,
        )?;
        if groove_enabled {
            updated.apply_riff_machine_groove(
                pattern_id,
                channel_id,
                &generated_indices,
                options,
            )?;
        }
        updated.limit_pattern_note_selection_range_with_options(
            pattern_id,
            channel_id,
            &generated_indices,
            LimitNoteOptions {
                minimum_key: options.minimum_key,
                maximum_key: options.maximum_key,
                wrap_to_bottom: options.wrap_to_bottom,
                scale_root: Some(options.scale_root),
                scale_intervals: Some(options.scale_intervals),
                snap_direction: options.snap_direction,
            },
        )?;
        *self = updated;
        Ok(arpeggiated)
    }

    fn apply_riff_machine_levels(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: RiffMachineOptions<'_>,
    ) -> Result<(), FlpError> {
        let mut state = if options.seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            options.seed
        };
        let selected_indices = note_indices.iter().copied().collect::<HashSet<_>>();
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut edits = HashMap::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| selected_indices.contains(note_index))
        {
            let velocity_base = if options.reset_levels {
                100
            } else {
                i32::from(note.velocity)
            };
            let pan_base = if options.reset_levels {
                64
            } else {
                i32::from(note.pan)
            };
            let release_base = if options.reset_levels {
                0
            } else {
                i32::from(note.release)
            };
            let mod_x_base = if options.reset_levels {
                0
            } else {
                i32::from(note.mod_x)
            };
            let mod_y_base = if options.reset_levels {
                0
            } else {
                i32::from(note.mod_y)
            };
            let velocity_range = (127 * i32::from(options.velocity_variation_percent) + 50) / 100;
            let pan_range = (127 * i32::from(options.pan_variation_percent) + 50) / 100;
            let pitch_range = i32::from(options.pitch_variation_semitones);
            let release_range = (127 * i32::from(options.release_variation_percent) + 50) / 100;
            let mod_x_range = (255 * i32::from(options.mod_x_variation_percent) + 50) / 100;
            let mod_y_range = (255 * i32::from(options.mod_y_variation_percent) + 50) / 100;
            // Keep the established velocity, pan, and pitch random stream order stable.
            let velocity_offset =
                randomizer_offset(&mut state, velocity_range, false, options.bipolar_levels);
            let pan_offset =
                randomizer_offset(&mut state, pan_range, false, options.bipolar_levels);
            let pitch_offset =
                randomizer_offset(&mut state, pitch_range, false, options.bipolar_levels);
            let release_offset =
                randomizer_offset(&mut state, release_range, false, options.bipolar_levels);
            let mod_x_offset =
                randomizer_offset(&mut state, mod_x_range, false, options.bipolar_levels);
            let mod_y_offset =
                randomizer_offset(&mut state, mod_y_range, false, options.bipolar_levels);
            let velocity = (velocity_base + velocity_offset).clamp(0, 127) as u8;
            let pan = (pan_base + pan_offset).clamp(0, 127) as u8;
            let key = (i32::from(note.key) + pitch_offset).clamp(0, 127) as u16;
            let release = (release_base + release_offset).clamp(0, 127) as u8;
            let mod_x = (mod_x_base + mod_x_offset).clamp(0, 255) as u8;
            let mod_y = (mod_y_base + mod_y_offset).clamp(0, 255) as u8;
            if velocity != note.velocity
                || pan != note.pan
                || key != note.key
                || release != note.release
                || mod_x != note.mod_x
                || mod_y != note.mod_y
            {
                edits.insert(
                    note_index,
                    PatternNoteEdit {
                        velocity: Some(velocity),
                        pan: Some(pan),
                        key: Some(key),
                        release: Some(release),
                        mod_x: Some(mod_x),
                        mod_y: Some(mod_y),
                        ..PatternNoteEdit::default()
                    },
                );
            }
        }
        self.edit_pattern_notes_batch(pattern_id, channel_id, &edits)
    }

    fn apply_riff_machine_groove(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: RiffMachineOptions<'_>,
    ) -> Result<(), FlpError> {
        let snap_ticks = options.groove_snap_ticks.filter(|ticks| *ticks > 0).ok_or(
            FlpError::UnsupportedEdit("Riff Machine Groove needs an enabled Piano roll snap grid"),
        )?;
        let selected_indices = note_indices.iter().copied().collect::<HashSet<_>>();
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut edits = HashMap::new();
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| selected_indices.contains(note_index))
        {
            let (position, length) = riff_machine_groove_note_timing(note, snap_ticks, options)?;
            if position != note.position || length != note.length {
                edits.insert(
                    note_index,
                    PatternNoteEdit {
                        position: Some(position),
                        length: Some(length),
                        ..PatternNoteEdit::default()
                    },
                );
            }
        }
        self.edit_pattern_notes_batch(pattern_id, channel_id, &edits)
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

    /// Scales velocity levels for every note in one channel of a pattern.
    /// The offset is a percentage of the full 0–127 velocity range.
    pub fn scale_pattern_note_levels(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        multiplier_percent: u16,
        offset_percent: i16,
    ) -> Result<usize, FlpError> {
        self.scale_pattern_note_levels_with_options(
            pattern_id,
            channel_id,
            ScaleLevelsOptions {
                multiplier_percent,
                offset_percent,
                ..ScaleLevelsOptions::default()
            },
        )
    }

    /// Scales velocity levels for every note in one channel using Scale Levels controls.
    pub fn scale_pattern_note_levels_with_options(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: ScaleLevelsOptions,
    ) -> Result<usize, FlpError> {
        self.scale_pattern_note_levels_in_scope(pattern_id, channel_id, None, options)
    }

    /// Scales velocity levels only for the selected channel-local note indices.
    /// The offset is a percentage of the full 0–127 velocity range.
    pub fn scale_pattern_note_selection_levels(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        multiplier_percent: u16,
        offset_percent: i16,
    ) -> Result<usize, FlpError> {
        self.scale_pattern_note_selection_levels_with_options(
            pattern_id,
            channel_id,
            note_indices,
            ScaleLevelsOptions {
                multiplier_percent,
                offset_percent,
                ..ScaleLevelsOptions::default()
            },
        )
    }

    /// Scales selected channel-local note indices using Scale Levels controls.
    pub fn scale_pattern_note_selection_levels_with_options(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: ScaleLevelsOptions,
    ) -> Result<usize, FlpError> {
        self.scale_pattern_note_levels_in_scope(pattern_id, channel_id, Some(note_indices), options)
    }

    fn scale_pattern_note_levels_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: ScaleLevelsOptions,
    ) -> Result<usize, FlpError> {
        if options.multiplier_percent > 200 {
            return Err(FlpError::UnsupportedEdit(
                "Scale Levels multiplier must be between 0 and 200 percent",
            ));
        }
        if !(-100..=100).contains(&options.offset_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Scale Levels offset must be between -100 and 100 percent",
            ));
        }
        if !(-100..=100).contains(&options.center_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Scale Levels center must be between -100 and 100 percent",
            ));
        }
        if !(-100..=100).contains(&options.tension_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Scale Levels tension must be between -100 and 100 percent",
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
        let edits = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .filter_map(|(note_index, note)| {
                let velocity = scale_note_level(note.velocity, options);
                (velocity != note.velocity).then_some((note_index, velocity))
            })
            .collect::<Vec<_>>();
        if edits.is_empty() {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, velocity) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    velocity: Some(*velocity),
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

    /// Applies a periodic gate and timing slew to notes in one channel.
    /// Each period is divided into 16 slices; every Nth slice is removed.
    pub fn claw_pattern_notes(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: ClawMachineOptions,
    ) -> Result<usize, FlpError> {
        self.claw_pattern_notes_in_scope(pattern_id, channel_id, None, options)
    }

    /// Applies the periodic gate and timing slew only to selected channel-local notes.
    pub fn claw_pattern_note_selection(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: ClawMachineOptions,
    ) -> Result<usize, FlpError> {
        self.claw_pattern_notes_in_scope(pattern_id, channel_id, Some(note_indices), options)
    }

    fn claw_pattern_notes_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: ClawMachineOptions,
    ) -> Result<usize, FlpError> {
        if options.period_ticks < 16 {
            return Err(FlpError::UnsupportedEdit(
                "Claw Machine period must be at least 16 ticks",
            ));
        }
        if !(2..=16).contains(&options.trash_every) {
            return Err(FlpError::UnsupportedEdit(
                "Claw Machine Trash every must be between 2 and 16",
            ));
        }
        if !(-100..=100).contains(&options.time_distortion_percent) {
            return Err(FlpError::UnsupportedEdit(
                "Claw Machine time distortion must be between -100 and 100 percent",
            ));
        }

        const SLICES_PER_PERIOD: u64 = 16;
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let selected_indices =
            note_indices.map(|indices| indices.iter().copied().collect::<HashSet<_>>());
        let targeted_notes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
            .map(|(note_index, note)| (note_index, note.clone()))
            .collect::<Vec<_>>();
        if targeted_notes.is_empty() {
            return Ok(0);
        }

        let origin = targeted_notes
            .iter()
            .map(|(_, note)| u64::from(note.position))
            .min()
            .expect("targeted notes are not empty");
        let scope_end = targeted_notes
            .iter()
            .map(|(_, note)| u64::from(note.position) + u64::from(note.length))
            .max()
            .expect("targeted notes are not empty");
        let period = u64::from(options.period_ticks);
        let short_note_threshold = options.period_ticks.div_ceil(16);
        let mut kept_notes = Vec::new();
        let mut removed_indices = Vec::new();
        let mut removed_positions = Vec::new();

        for (note_index, note) in targeted_notes {
            let relative = u64::from(note.position) - origin;
            let phase = relative % period;
            let slice = phase * SLICES_PER_PERIOD / period;
            let trashed = (slice + 1).is_multiple_of(u64::from(options.trash_every));
            let too_short = options.remove_short_notes && note.length < short_note_threshold;
            if trashed || too_short {
                removed_indices.push(note_index);
                removed_positions.push(note.position);
                continue;
            }

            let position = claw_warp_position(
                u64::from(note.position),
                origin,
                options.period_ticks,
                options.time_distortion_percent,
            )?;
            let original_length = note.length;
            kept_notes.push((note_index, note, position, original_length));
        }

        kept_notes.sort_unstable_by_key(|(note_index, note, _, _)| (note.position, *note_index));
        if options.stretch_to_compensate && !removed_positions.is_empty() {
            let mut kept_onsets = kept_notes
                .iter()
                .map(|(_, note, _, _)| note.position)
                .collect::<Vec<_>>();
            kept_onsets.sort_unstable();
            kept_onsets.dedup();
            for (_, note, position, _) in &mut kept_notes {
                let next_onset_index = kept_onsets.partition_point(|onset| *onset <= note.position);
                let next_original_onset = kept_onsets
                    .get(next_onset_index)
                    .copied()
                    .map(u64::from)
                    .unwrap_or(scope_end);
                let has_removed_note_in_gap = removed_positions.iter().any(|removed_position| {
                    u64::from(*removed_position) > u64::from(note.position)
                        && u64::from(*removed_position) < next_original_onset
                });
                if !has_removed_note_in_gap {
                    continue;
                }

                let compensated_end = if let Some(next_onset) = kept_onsets.get(next_onset_index) {
                    claw_warp_position(
                        u64::from(*next_onset),
                        origin,
                        options.period_ticks,
                        options.time_distortion_percent,
                    )?
                } else {
                    claw_warp_position(
                        scope_end,
                        origin,
                        options.period_ticks,
                        options.time_distortion_percent,
                    )?
                };
                let compensated_length = compensated_end.saturating_sub(*position).max(note.length);
                note.length = compensated_length;
            }
        }

        let edits = kept_notes
            .into_iter()
            .filter_map(|(note_index, note, position, original_length)| {
                (position != note.position || note.length != original_length).then_some((
                    note_index,
                    position,
                    note.length,
                ))
            })
            .collect::<Vec<_>>();
        let changed_count = edits.len() + removed_indices.len();
        if changed_count == 0 {
            return Ok(0);
        }

        let mut updated = self.clone();
        for (note_index, position, length) in &edits {
            updated.edit_pattern_note(
                pattern_id,
                channel_id,
                *note_index,
                PatternNoteEdit {
                    position: Some(*position),
                    length: Some(*length),
                    ..PatternNoteEdit::default()
                },
            )?;
        }
        removed_indices.sort_unstable_by(|left, right| right.cmp(left));
        for note_index in removed_indices {
            updated.delete_pattern_note(pattern_id, channel_id, note_index)?;
        }
        *self = updated;
        Ok(changed_count)
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
            LimitNoteOptions {
                minimum_key,
                maximum_key,
                wrap_to_bottom: false,
                scale_root: None,
                scale_intervals: None,
                snap_direction: LimitSnapDirection::Up,
            },
        )
    }

    /// Limits channel note pitches using optional wrapping and scale snapping.
    pub fn limit_pattern_note_range_with_options(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        options: LimitNoteOptions<'_>,
    ) -> Result<usize, FlpError> {
        self.limit_pattern_note_range_in_scope(pattern_id, channel_id, None, options)
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
            LimitNoteOptions {
                minimum_key,
                maximum_key,
                wrap_to_bottom: false,
                scale_root: None,
                scale_intervals: None,
                snap_direction: LimitSnapDirection::Up,
            },
        )
    }

    /// Limits selected channel note pitches using optional wrapping and scale snapping.
    pub fn limit_pattern_note_selection_range_with_options(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: &[usize],
        options: LimitNoteOptions<'_>,
    ) -> Result<usize, FlpError> {
        self.limit_pattern_note_range_in_scope(pattern_id, channel_id, Some(note_indices), options)
    }

    fn limit_pattern_note_range_in_scope(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_indices: Option<&[usize]>,
        options: LimitNoteOptions<'_>,
    ) -> Result<usize, FlpError> {
        if options.minimum_key > options.maximum_key || options.maximum_key > 127 {
            return Err(FlpError::UnsupportedEdit(
                "note range must be ordered and remain within keys 0 through 127",
            ));
        }
        let scale_snap = match (options.scale_root, options.scale_intervals) {
            (None, None) => None,
            (Some(scale_root), Some(scale_intervals)) => {
                if scale_root > 11
                    || scale_intervals.is_empty()
                    || scale_intervals.iter().any(|interval| *interval > 11)
                {
                    return Err(FlpError::UnsupportedEdit(
                        "scale root and intervals must describe pitch classes 0 through 11",
                    ));
                }
                let minimum = i32::from(options.minimum_key);
                let maximum = i32::from(options.maximum_key);
                if !(minimum..=maximum)
                    .any(|key| note_pitch_in_scale(key, scale_root, scale_intervals))
                {
                    return Err(FlpError::UnsupportedEdit(
                        "the requested key range contains no notes from the selected scale",
                    ));
                }
                Some((scale_root, scale_intervals, options.snap_direction))
            }
            _ => {
                return Err(FlpError::UnsupportedEdit(
                    "scale root and scale intervals must both be provided",
                ));
            }
        };
        let minimum = i32::from(options.minimum_key);
        let maximum = i32::from(options.maximum_key);
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
        let mut snap_up_next = true;
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .filter(|(note_index, _)| note_index_in_scope(*note_index, selected_indices.as_ref()))
        {
            let mut key = fold_note_pitch_to_limit(
                i32::from(note.key),
                minimum,
                maximum,
                options.wrap_to_bottom,
            );
            if let Some((scale_root, scale_intervals, snap_direction)) = scale_snap
                && !note_pitch_in_scale(key, scale_root, scale_intervals)
            {
                let snap_up = match snap_direction {
                    LimitSnapDirection::Up => true,
                    LimitSnapDirection::Down => false,
                    LimitSnapDirection::Alternate => {
                        let snap_up = snap_up_next;
                        snap_up_next = !snap_up_next;
                        snap_up
                    }
                };
                key = snap_note_pitch_to_scale(
                    key,
                    minimum,
                    maximum,
                    scale_root,
                    scale_intervals,
                    snap_up,
                );
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
        if edit
            .time_signature
            .is_some_and(|(numerator, denominator)| numerator == 0 || denominator == 0)
        {
            return Err(FlpError::UnsupportedEdit(
                "project time-signature numerator and denominator must be positive",
            ));
        }
        let advanced_edit = edit.play_truncated_notes_in_clips.is_some()
            || edit.fast_declick_for_cut_groups.is_some();
        if !advanced_edit && edit.pan_law_raw.is_none() && edit.time_signature.is_none() {
            return Ok(());
        }
        let mut candidate = self.clone();

        if advanced_edit {
            let anchor = find_project_settings_anchor(&candidate.events).ok_or(
                FlpError::UnsupportedEdit(
                    "the supported Project settings event block could not be identified",
                ),
            )?;

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
        }

        if let Some(pan_law_raw) = edit.pan_law_raw {
            let channel_start = candidate
                .events
                .iter()
                .position(|event| event.opcode == 0x40)
                .unwrap_or(candidate.events.len());
            let mut pan_law_event = None;
            for (index, event) in candidate.events.iter().take(channel_start).enumerate() {
                if event.opcode != 0x17 {
                    continue;
                }
                if pan_law_event.is_some() {
                    return Err(FlpError::UnsupportedEdit(
                        "the project has multiple global 0x17 pan-law events",
                    ));
                }
                if event.encoding != PayloadEncoding::Byte || event.payload.len() != 1 {
                    return Err(FlpError::UnsupportedEdit(
                        "the project's 0x17 pan-law event is not a one-byte value",
                    ));
                }
                pan_law_event = Some(index);
            }
            if let Some(index) = pan_law_event {
                candidate.events[index].replace_byte_payload(pan_law_raw)?;
            } else if pan_law_raw != 0 {
                candidate
                    .events
                    .insert(channel_start, FlpEvent::new_byte(0x17, pan_law_raw));
            }
        }

        if let Some((numerator, denominator)) = edit.time_signature {
            let channel_start = candidate
                .events
                .iter()
                .position(|event| event.opcode == 0x40)
                .unwrap_or(candidate.events.len());
            let mut numerator_event = None;
            let mut denominator_event = None;
            for (index, event) in candidate.events.iter().take(channel_start).enumerate() {
                let slot = match event.opcode {
                    0x11 => &mut numerator_event,
                    0x12 => &mut denominator_event,
                    _ => continue,
                };
                if slot.is_some() {
                    return Err(FlpError::UnsupportedEdit(
                        "the project has duplicate global time-signature events",
                    ));
                }
                if event.encoding != PayloadEncoding::Byte
                    || event.payload.len() != 1
                    || event.payload[0] == 0
                {
                    return Err(FlpError::UnsupportedEdit(
                        "a global time-signature event has an invalid byte value",
                    ));
                }
                *slot = Some(index);
            }

            if let Some(index) = numerator_event {
                candidate.events[index].replace_byte_payload(numerator)?;
            }
            if let Some(index) = denominator_event {
                candidate.events[index].replace_byte_payload(denominator)?;
            }
            if numerator_event.is_none() {
                let insert_index = denominator_event.unwrap_or(channel_start);
                candidate
                    .events
                    .insert(insert_index, FlpEvent::new_byte(0x11, numerator));
            }
            if denominator_event.is_none() {
                let new_channel_start = candidate
                    .events
                    .iter()
                    .position(|event| event.opcode == 0x40)
                    .unwrap_or(candidate.events.len());
                let numerator_index = candidate
                    .events
                    .iter()
                    .take(new_channel_start)
                    .position(|event| event.opcode == 0x11)
                    .unwrap_or(new_channel_start);
                candidate
                    .events
                    .insert(numerator_index + 1, FlpEvent::new_byte(0x12, denominator));
            }
        }
        candidate.refresh_event_offsets()?;
        candidate.metadata =
            read_project_metadata(&candidate.events, candidate.project_version.as_deref());
        *self = candidate;
        Ok(())
    }

    /// Sets the global Channel Rack swing mix in FL's 0..=128 range.
    /// The project-level `0x0B` byte event defaults to zero when absent.
    pub fn set_global_swing_mix(&mut self, swing_mix: u8) -> Result<(), FlpError> {
        if swing_mix > 128 {
            return Err(FlpError::UnsupportedEdit(
                "global swing mix must be in the 0..=128 range",
            ));
        }

        let channel_start = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        let mut swing_events = self
            .events
            .iter()
            .enumerate()
            .filter(|(index, event)| *index < channel_start && event.opcode == 0x0B)
            .map(|(index, _)| index);
        let existing_event = swing_events.next();
        if swing_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the project has multiple 0x0B global swing events",
            ));
        }

        let Some(event_index) = existing_event else {
            if swing_mix == 0 {
                return Ok(());
            }
            let mut candidate = self.clone();
            candidate
                .events
                .insert(channel_start, FlpEvent::new_byte(0x0B, swing_mix));
            candidate.refresh_event_offsets()?;
            candidate.metadata =
                read_project_metadata(&candidate.events, candidate.project_version.as_deref());
            *self = candidate;
            return Ok(());
        };

        let event = &self.events[event_index];
        if event.encoding != PayloadEncoding::Byte || event.payload.len() != 1 {
            return Err(FlpError::UnsupportedEdit(
                "the project's 0x0B global swing event is not a one-byte value",
            ));
        }
        if event.payload[0] == swing_mix {
            return Ok(());
        }

        let mut candidate = self.clone();
        candidate.events[event_index].replace_byte_payload(swing_mix)?;
        candidate.refresh_event_offsets()?;
        candidate.metadata =
            read_project_metadata(&candidate.events, candidate.project_version.as_deref());
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

    /// Replaces the source path in an existing sample-bearing channel's `0xC4` event.
    /// The channel's string encoding and bytes after the terminator are preserved.
    /// This does not create a missing sample-path event.
    pub fn set_channel_sample_path(&mut self, channel_id: u16, path: &str) -> Result<(), FlpError> {
        if path.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "sample paths cannot contain an embedded NUL character",
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
        if !matches!(channel.kind, Some(0 | 4)) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel is not a sample-bearing channel",
            ));
        }

        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xC4)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected sample-bearing channel has no recognized 0xC4 sample-path event",
            ))?;
        let event = &self.events[event_index];
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel's 0xC4 sample-path event is not length-prefixed",
            ));
        }

        let utf16 = project_string_is_utf16(&event.payload, self.project_version.as_deref());
        let payload = replace_project_string_payload(&event.payload, path, utf16)?;
        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Sets a channel's enabled state, which controls whether the Channel Rack row is muted.
    /// Existing `0x00` events are edited in place; channels without one get a byte event after
    /// their recognized `0x15` kind event. Other channel events are retained.
    pub fn set_channel_enabled(&mut self, channel_id: u16, enabled: bool) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }

        let mut enable_events = channel
            .event_range()
            .filter(|index| self.events[*index].opcode == 0x00);
        let existing_event = enable_events.next();
        if enable_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel has multiple 0x00 enabled-state events",
            ));
        }

        let mut candidate = self.clone();
        if let Some(event_index) = existing_event {
            candidate.events[event_index].replace_byte_payload(u8::from(enabled))?;
        } else {
            let mut kind_events = channel
                .event_range()
                .filter(|index| self.events[*index].opcode == 0x15);
            let Some(kind_index) = kind_events.next() else {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has no recognized 0x15 kind event",
                ));
            };
            if kind_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has multiple 0x15 kind events",
                ));
            }
            let kind_event = &candidate.events[kind_index];
            if kind_event.encoding != PayloadEncoding::Byte || kind_event.payload.len() != 1 {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel's 0x15 kind event is not a one-byte event",
                ));
            }
            candidate
                .events
                .insert(kind_index + 1, FlpEvent::new_byte(0x00, u8::from(enabled)));
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Sets the per-channel multiplier for Channel Rack swing in FL's 0..=128 range.
    /// The `0x61` word event defaults to 128 (100%) when absent. Sampler and Native instrument (generator)
    /// channels can be edited; audio, Layer, Automation, and unknown channel types are not supported.
    pub fn set_channel_swing_mix(
        &mut self,
        channel_id: u16,
        swing_mix: u16,
    ) -> Result<(), FlpError> {
        if swing_mix > 128 {
            return Err(FlpError::UnsupportedEdit(
                "channel swing mix must be in the 0..=128 range",
            ));
        }

        self.require_unique_channel(channel_id)?;
        let channel = self
            .channels()
            .into_iter()
            .find(|channel| channel.id == channel_id)
            .expect("the unique channel was checked above");
        if !matches!(channel.kind, Some(0 | 2)) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel type does not support swing mix",
            ));
        }

        let mut swing_events = channel
            .event_range()
            .filter(|index| self.events[*index].opcode == 0x61);
        let existing_event = swing_events.next();
        if swing_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel has multiple 0x61 swing-mix events",
            ));
        }

        let Some(event_index) = existing_event else {
            if swing_mix == 128 {
                return Ok(());
            }

            let mut kind_events = channel
                .event_range()
                .filter(|index| self.events[*index].opcode == 0x15);
            let Some(kind_index) = kind_events.next() else {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has no recognized 0x15 kind event",
                ));
            };
            if kind_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has multiple 0x15 kind events",
                ));
            }
            let kind_event = &self.events[kind_index];
            if kind_event.encoding != PayloadEncoding::Byte || kind_event.payload.len() != 1 {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel's 0x15 kind event is not a one-byte value",
                ));
            }

            let mut candidate = self.clone();
            candidate
                .events
                .insert(kind_index + 1, FlpEvent::new_word(0x61, swing_mix));
            candidate.refresh_event_offsets()?;
            *self = candidate;
            return Ok(());
        };

        let event = &self.events[event_index];
        if event.encoding != PayloadEncoding::Word || event.payload.len() != 2 {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel's 0x61 swing-mix event is not a two-byte word",
            ));
        }
        if channel.swing_mix_raw() == Some(swing_mix) {
            return Ok(());
        }

        let mut candidate = self.clone();
        candidate.events[event_index].replace_word_payload(swing_mix)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Sets the Channel Rack compact (zipped) state using the observed `0x0F` event.
    /// Existing events are edited in place; zipping a channel without one inserts a byte event
    /// after its recognized `0x15` kind event. Other channel events are retained.
    pub fn set_channel_zipped(&mut self, channel_id: u16, zipped: bool) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }

        let mut zip_events = channel
            .event_range()
            .filter(|index| self.events[*index].opcode == 0x0F);
        let existing_event = zip_events.next();
        if zip_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel has multiple 0x0F zipped-state events",
            ));
        }
        if channel.zipped == zipped {
            return Ok(());
        }

        let mut candidate = self.clone();
        if let Some(event_index) = existing_event {
            candidate.events[event_index].replace_byte_payload(u8::from(zipped))?;
        } else {
            let mut kind_events = channel
                .event_range()
                .filter(|index| self.events[*index].opcode == 0x15);
            let Some(kind_index) = kind_events.next() else {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has no recognized 0x15 kind event",
                ));
            };
            if kind_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has multiple 0x15 kind events",
                ));
            }
            let kind_event = &candidate.events[kind_index];
            if kind_event.encoding != PayloadEncoding::Byte || kind_event.payload.len() != 1 {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel's 0x15 kind event is not a one-byte event",
                ));
            }
            candidate
                .events
                .insert(kind_index + 1, FlpEvent::new_byte(0x0F, u8::from(zipped)));
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Sets the RGB components of a channel's `0x80` color event while retaining its fourth
    /// byte. Channels without a color event receive one immediately after their kind event.
    pub fn set_channel_color(&mut self, channel_id: u16, rgb: [u8; 3]) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }

        let mut color_events = channel
            .event_range()
            .filter(|index| self.events[*index].opcode == 0x80);
        let existing_event = color_events.next();
        if color_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel has multiple 0x80 color events",
            ));
        }

        let mut candidate = self.clone();
        if let Some(event_index) = existing_event {
            let event = &candidate.events[event_index];
            if event.encoding != PayloadEncoding::Dword || event.payload.len() != 4 {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel's 0x80 color event is not a four-byte value",
                ));
            }
            let fourth_byte = event.payload[3];
            let value = u32::from_le_bytes([rgb[0], rgb[1], rgb[2], fourth_byte]);
            candidate.events[event_index].replace_dword_payload(value)?;
        } else {
            let mut kind_events = channel
                .event_range()
                .filter(|index| self.events[*index].opcode == 0x15);
            let Some(kind_index) = kind_events.next() else {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has no recognized 0x15 kind event",
                ));
            };
            if kind_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel has multiple 0x15 kind events",
                ));
            }
            if candidate.events[kind_index].encoding != PayloadEncoding::Byte
                || candidate.events[kind_index].payload.len() != 1
            {
                return Err(FlpError::UnsupportedEdit(
                    "the selected channel's 0x15 kind event is not a one-byte value",
                ));
            }
            candidate.events.insert(
                kind_index + 1,
                FlpEvent::new_dword(0x80, u32::from_le_bytes([rgb[0], rgb[1], rgb[2], 0])),
            );
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Moves a channel to `target_index` in Channel Rack order without changing its ID or event
    /// bytes. The complete event range for the channel is moved as one block.
    pub fn move_channel(&mut self, channel_id: u16, target_index: usize) -> Result<(), FlpError> {
        let channels = self.channels();
        let mut matching = channels
            .iter()
            .enumerate()
            .filter(|(_, channel)| channel.id == channel_id);
        let Some((source_index, channel)) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }
        if target_index >= channels.len() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Channel Rack position does not exist",
            ));
        }
        if source_index == target_index {
            return Ok(());
        }

        let source_range = channel.event_range();
        let insertion_index = if target_index < source_index {
            channels[target_index].event_range().start
        } else {
            channels[target_index]
                .event_range()
                .end
                .checked_sub(source_range.len())
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested channel order has inconsistent event ranges",
                ))?
        };

        let mut candidate = self.clone();
        let moved_events = candidate
            .events
            .drain(source_range)
            .collect::<Vec<FlpEvent>>();
        candidate
            .events
            .splice(insertion_index..insertion_index, moved_events);
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Reorders every channel using a stable Channel Rack sort while preserving each channel's
    /// complete event block and ID. Name sorting is case-insensitive; color sorting follows hue
    /// from red through violet, then achromatic and uncolored channels; type sorting follows the
    /// documented Layer, generator, Sampler, audio-channel, and automation grouping. Mixer-track
    /// sorting keeps generators and Layer channels at the top, then sorts known assignments by
    /// track number while leaving missing or negative assignments at the end.
    pub fn sort_channels(&mut self, order: ChannelSortOrder) -> Result<(), FlpError> {
        let mut channels = self.channels();
        match order {
            ChannelSortOrder::Color => {
                channels.sort_by_key(|channel| channel_color_sort_key(channel.color()));
            }
            ChannelSortOrder::MixerTrack => {
                channels.sort_by_key(|channel| {
                    let is_generator = channel.sample_path().is_none()
                        && matches!(
                            channel.channel_type(),
                            Some(ChannelType::Native | ChannelType::Instrument)
                        );
                    let is_layer = channel.channel_type() == Some(ChannelType::Layer);
                    let rank = if is_generator || is_layer { 0 } else { 1 };
                    let track = channel
                        .mixer_track()
                        .filter(|track| *track >= 0)
                        .map_or(i16::MAX, i16::from);
                    (rank, track)
                });
            }
            ChannelSortOrder::Name => {
                channels.sort_by_key(|channel| {
                    channel.display_name().unwrap_or_default().to_lowercase()
                });
            }
            ChannelSortOrder::Type => {
                channels.sort_by_key(|channel| match channel.channel_type() {
                    Some(ChannelType::Layer) => (0, 0),
                    Some(ChannelType::Native) => (1, 0),
                    Some(ChannelType::Sampler) => (2, 0),
                    Some(ChannelType::Instrument) => (3, 0),
                    Some(ChannelType::Automation) => (4, 0),
                    Some(ChannelType::Unknown(raw)) => (5, raw),
                    None => (5, u8::MAX),
                })
            }
        }
        let ordered_ids = channels.iter().map(ChannelSummary::id).collect::<Vec<_>>();
        self.reorder_channels(&ordered_ids)
    }

    /// Applies a complete Channel Rack order without changing channel IDs or event bytes.
    /// Every existing channel ID must appear exactly once in `channel_ids`.
    pub fn reorder_channels(&mut self, channel_ids: &[u16]) -> Result<(), FlpError> {
        let channels = self.channels();
        if channel_ids.len() != channels.len() {
            return Err(FlpError::UnsupportedEdit(
                "a complete order for every Channel Rack channel is required",
            ));
        }
        let mut channel_indices = BTreeMap::new();
        for (index, channel) in channels.iter().enumerate() {
            if channel_indices.insert(channel.id(), index).is_some() {
                return Err(FlpError::AmbiguousChannelId(channel.id()));
            }
        }
        let mut seen = BTreeSet::new();
        for channel_id in channel_ids {
            if !seen.insert(*channel_id) {
                return Err(FlpError::AmbiguousChannelId(*channel_id));
            }
            if !channel_indices.contains_key(channel_id) {
                return Err(FlpError::ChannelNotFound(*channel_id));
            }
        }
        if channels.is_empty()
            || channels
                .iter()
                .map(ChannelSummary::id)
                .eq(channel_ids.iter().copied())
        {
            return Ok(());
        }
        if channels
            .windows(2)
            .any(|pair| pair[0].end_event_index != pair[1].first_event_index)
        {
            return Err(FlpError::UnsupportedEdit(
                "Channel Rack event ranges are not contiguous",
            ));
        }

        let first_event_index = channels[0].first_event_index;
        let trailing_event_index = channels[channels.len() - 1].end_event_index;
        if first_event_index > trailing_event_index || trailing_event_index > self.events.len() {
            return Err(FlpError::UnsupportedEdit(
                "Channel Rack event ranges are inconsistent",
            ));
        }

        let mut candidate = self.clone();
        let mut reordered_events = Vec::with_capacity(self.events.len());
        reordered_events.extend_from_slice(&self.events[..first_event_index]);
        for channel_id in channel_ids {
            let channel = &channels[channel_indices[channel_id]];
            reordered_events.extend_from_slice(&self.events[channel.event_range()]);
        }
        reordered_events.extend_from_slice(&self.events[trailing_event_index..]);
        candidate.events = reordered_events;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Adds a Sampler channel with the given source path and display name.
    ///
    /// The channel uses the project's recognized string encoding, gets the next available
    /// channel ID, and is inserted before the channel-list terminator so existing event bytes
    /// remain in order. Projects whose string encoding cannot be inferred are rejected.
    pub fn create_sampler_channel(&mut self, path: &str, name: &str) -> Result<u16, FlpError> {
        self.create_sample_channel(path, name, 0)
    }

    /// Adds a sample-backed Audio Clip channel with the given source path and display name.
    ///
    /// The channel uses the same recognized string encoding and unique-ID rules as a Sampler.
    pub fn create_audio_channel(&mut self, path: &str, name: &str) -> Result<u16, FlpError> {
        self.create_sample_channel(path, name, 4)
    }

    fn create_sample_channel(&mut self, path: &str, name: &str, kind: u8) -> Result<u16, FlpError> {
        if path.is_empty() || path.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a new sample channel requires a non-empty path without embedded NUL characters",
            ));
        }
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "channel names cannot contain an embedded NUL character",
            ));
        }

        let channels = self.channels();
        let used_ids = channels
            .iter()
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        let next_id = used_ids
            .iter()
            .max()
            .copied()
            .and_then(|value| value.checked_add(1))
            .filter(|candidate| !used_ids.contains(candidate))
            .or_else(|| (0..=u16::MAX).find(|candidate| !used_ids.contains(candidate)))
            .ok_or(FlpError::UnsupportedEdit(
                "the project has no unused channel IDs",
            ))?;

        let uses_utf16 = self
            .project_string_encoding()
            .ok_or(FlpError::UnsupportedEdit(
                "the project's string encoding cannot be inferred for a new channel",
            ))?;
        let sample_path = encode_project_string(path, uses_utf16)?;
        let display_name = encode_project_string(name, uses_utf16)?;
        let new_events = vec![
            FlpEvent::new_word(0x40, next_id),
            FlpEvent::new_byte(0x15, kind),
            FlpEvent::new_byte(0x00, 1),
            FlpEvent::new_data(0xC4, sample_path)?,
            FlpEvent::new_data(0xCB, display_name)?,
        ];
        let insertion_index = channels
            .last()
            .map(|channel| channel.event_range().end)
            .or_else(|| self.events.iter().position(|event| event.opcode == 0x62))
            .unwrap_or(self.events.len());
        let channel_count = u16::try_from(channels.len().saturating_add(1)).map_err(|_| {
            FlpError::UnsupportedEdit("the project has too many channels to add another")
        })?;

        let mut candidate = self.clone();
        candidate
            .events
            .splice(insertion_index..insertion_index, new_events);
        candidate.header.legacy_channel_count = channel_count;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(next_id)
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

fn channel_color_sort_key(color: Option<[u8; 4]>) -> (u8, u16, u8, u8, u8) {
    let Some([red, green, blue, _]) = color else {
        return (2, 0, 0, 0, 0);
    };
    let red_f = f32::from(red) / 255.0;
    let green_f = f32::from(green) / 255.0;
    let blue_f = f32::from(blue) / 255.0;
    let maximum = red_f.max(green_f).max(blue_f);
    let minimum = red_f.min(green_f).min(blue_f);
    let delta = maximum - minimum;
    if delta <= f32::EPSILON {
        return (1, 0, red, green, blue);
    }
    let hue = if maximum == red_f {
        60.0 * ((green_f - blue_f) / delta).rem_euclid(6.0)
    } else if maximum == green_f {
        60.0 * ((blue_f - red_f) / delta + 2.0)
    } else {
        60.0 * ((red_f - green_f) / delta + 4.0)
    };
    (0, (hue * 10.0).round() as u16, red, green, blue)
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

fn next_playlist_clip_id(
    arrangement: &Arrangement,
    preferred_id: Option<u32>,
) -> Result<u32, FlpError> {
    let used_ids = arrangement
        .clips
        .iter()
        .filter_map(|clip| clip.clip_id)
        .collect::<HashSet<_>>();
    if let Some(next_id) = used_ids
        .iter()
        .max()
        .and_then(|clip_id| clip_id.checked_add(1))
    {
        return Ok(next_id);
    }
    if let Some(preferred_id) = preferred_id.filter(|clip_id| !used_ids.contains(clip_id)) {
        return Ok(preferred_id);
    }
    (0..=u32::MAX)
        .find(|clip_id| !used_ids.contains(clip_id))
        .ok_or(FlpError::LengthOverflow)
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
            detail: "edited field exceeds its source event",
        });
    }
    let prefix_length = match &event.encoding {
        PayloadEncoding::Data { length_prefix } => length_prefix.len(),
        _ => {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "event does not have a data payload",
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
            detail: "event wire bytes do not match the decoded payload",
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
                "the string contains a character unavailable in the project's legacy encoding",
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
                "the string contains a character unavailable in the project's legacy encoding",
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
    let mut duplicate_time_signature_events = false;
    let mut invalid_time_signature_event = false;
    let mut global_swing_mix = None;
    let mut pan_law_event = None;
    let mut duplicate_pan_law_events = false;
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
            0x11 | 0x12 if event_index < channel_start => {
                let slot = if event.opcode == 0x11 {
                    &mut numerator
                } else {
                    &mut denominator
                };
                if slot.is_some() {
                    duplicate_time_signature_events = true;
                } else if event.encoding != PayloadEncoding::Byte
                    || event.payload.len() != 1
                    || event.payload[0] == 0
                {
                    invalid_time_signature_event = true;
                } else {
                    *slot = Some(event.payload[0]);
                }
            }
            0x0B if event_index < channel_start
                && event.encoding == PayloadEncoding::Byte
                && event.payload.len() == 1
                && global_swing_mix.is_none() =>
            {
                global_swing_mix = Some(event.payload[0]);
            }
            0x17 if event_index < channel_start => {
                if pan_law_event.is_some() {
                    duplicate_pan_law_events = true;
                } else {
                    pan_law_event = Some(event);
                }
            }
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

    let time_signature = if duplicate_time_signature_events || invalid_time_signature_event {
        None
    } else {
        numerator.zip(denominator)
    };
    let pan_law_raw = if duplicate_pan_law_events {
        None
    } else {
        pan_law_event.and_then(|event| {
            (event.encoding == PayloadEncoding::Byte && event.payload.len() == 1)
                .then_some(event.payload[0])
        })
    };
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
        global_swing_mix,
        pan_law_raw,
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

fn replace_vst_state_field(
    payload: &[u8],
    state_data_range: Option<&std::ops::Range<usize>>,
    state_bytes: &[u8],
) -> Result<Vec<u8>, FlpError> {
    let state_data_range = state_data_range.ok_or(FlpError::UnsupportedEdit(
        "the VST envelope has no state field 53",
    ))?;
    let mut cursor = 4usize;
    let mut state_field_count = 0usize;
    while cursor < payload.len() {
        let id_end = cursor.checked_add(4).ok_or(FlpError::LengthOverflow)?;
        let header_end = cursor.checked_add(12).ok_or(FlpError::LengthOverflow)?;
        let id = read_u32(payload, cursor, "VST field ID")?;
        let encoded_length = payload
            .get(id_end..header_end)
            .ok_or(FlpError::UnsupportedEdit(
                "the VST envelope contains a truncated field header",
            ))?;
        let encoded_length = u64::from_le_bytes(
            encoded_length
                .try_into()
                .map_err(|_| FlpError::LengthOverflow)?,
        );
        let length = usize::try_from(encoded_length).map_err(|_| FlpError::LengthOverflow)?;
        let data_end = header_end
            .checked_add(length)
            .ok_or(FlpError::LengthOverflow)?;
        if data_end > payload.len() {
            return Err(FlpError::UnsupportedEdit(
                "the VST envelope contains a truncated field payload",
            ));
        }
        if id == 53 {
            state_field_count += 1;
            if state_data_range != &(header_end..data_end) {
                return Err(FlpError::UnsupportedEdit(
                    "the VST state field range could not be verified",
                ));
            }
        }
        cursor = data_end;
    }
    if state_field_count != 1 {
        return Err(FlpError::UnsupportedEdit(
            "VST state write-back requires exactly one field 53",
        ));
    }
    let field_start = state_data_range
        .start
        .checked_sub(12)
        .ok_or(FlpError::LengthOverflow)?;
    let state_length = u64::try_from(state_bytes.len()).map_err(|_| FlpError::LengthOverflow)?;
    let capacity = payload
        .len()
        .checked_sub(state_data_range.len())
        .and_then(|length| length.checked_add(state_bytes.len()))
        .ok_or(FlpError::LengthOverflow)?;
    let mut replacement = Vec::with_capacity(capacity);
    replacement.extend_from_slice(&payload[..field_start]);
    replacement.extend_from_slice(&payload[field_start..field_start + 4]);
    replacement.extend_from_slice(&state_length.to_le_bytes());
    replacement.extend_from_slice(state_bytes);
    replacement.extend_from_slice(&payload[state_data_range.end..]);
    Ok(replacement)
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
        ArticulateOptions, ChannelGroupSummary, ChannelNoteRouter, ChannelSortOrder,
        ChannelSummary, ClawMachineOptions, FlpDocument, FlpError, FlpEvent, FstPreset,
        FstPresetKind, LimitNoteOptions, LimitSnapDirection, MixerInsertEdit,
        MixerInsertSignalTransform, MixerInsertSummary, MixerParameterKind, MixerRouteAudibility,
        PATTERN_NOTE_SLIDE_FLAG, PatternControllerEdit, PatternNote, PatternNoteEdit,
        PayloadEncoding, PlaylistClipEdit, PlaylistClipTarget, PlaylistTrackEdit, ProjectInfoEdit,
        ProjectSettingsEdit, RiffMachineOptions, RiffMachineQuantizeMode, ScaleLevelsOptions,
        TimeMarkerEdit, midi::MidiChannelMapping, midi::MidiFile, parse_vst_plugin_state_metadata,
        riff_machine_groove_note_timing,
    };

    fn articulate_options(
        multiplier_percent: u8,
        variation_percent: u8,
        seed: u64,
        use_original_lengths: bool,
    ) -> ArticulateOptions {
        ArticulateOptions {
            multiplier_percent,
            variation_percent,
            seed,
            use_original_lengths,
            chop_chords: false,
        }
    }

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

    fn state_preset_fixture(format: u16) -> Vec<u8> {
        let mut bytes = flp_fixture(&[], &[], &[]);
        bytes[8..10].copy_from_slice(&format.to_le_bytes());
        bytes
    }

    #[test]
    fn classifies_state_preset_variants_by_header_format_losslessly() {
        for (format, expected) in [
            (24, FstPresetKind::AutomationState),
            (32, FstPresetKind::ChannelState),
            (48, FstPresetKind::NativePluginState),
            (49, FstPresetKind::VstGeneratorState),
            (50, FstPresetKind::VstEffectState),
            (64, FstPresetKind::MixerInsertState),
            (0x1234, FstPresetKind::UnknownFormat(0x1234)),
        ] {
            let fixture = state_preset_fixture(format);
            let preset = FstPreset::parse(&fixture).expect("state preset envelope should parse");
            assert_eq!(preset.kind(), expected);
            assert_eq!(
                preset.encode_lossless().expect("preset should encode"),
                fixture
            );
        }
    }

    fn append_data_event(event_stream: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
        event_stream.push(opcode);
        event_stream.extend_from_slice(&super::encode_leb128(payload.len() as u32));
        event_stream.extend_from_slice(payload);
    }

    fn channel_key_region_parameters(low: u32, high: u32) -> Vec<u8> {
        let mut parameters = vec![0; 76];
        parameters[68..72].copy_from_slice(&low.to_le_bytes());
        parameters[72..76].copy_from_slice(&high.to_le_bytes());
        parameters
    }

    #[test]
    fn playlist_track_mute_and_group_edits_preserve_other_state_bytes() {
        let mut event_stream = Vec::new();
        let mut original_tracks = Vec::new();
        for track_id in 1_u32..=2 {
            let mut state = vec![0; 70];
            state[..4].copy_from_slice(&track_id.to_le_bytes());
            state[12] = 1;
            state[13] = 0xA5;
            state[46] = 0;
            state[47] = 0x5A;
            state[69] = 0xC3;
            append_data_event(&mut event_stream, 0xEE, &state);
            original_tracks.push(state);
        }

        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let tracks = document.playlist_tracks();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[1].enabled, Some(true));
        assert_eq!(tracks[1].grouped, Some(false));
        assert_eq!(tracks[1].state_bytes, original_tracks[1]);

        document
            .edit_playlist_track(
                2,
                PlaylistTrackEdit {
                    enabled: Some(false),
                    grouped: Some(true),
                },
            )
            .expect("mute and group fields should be editable");
        let tracks = document.playlist_tracks();
        assert_eq!(tracks[0].state_bytes, original_tracks[0]);
        assert_eq!(tracks[1].enabled, Some(false));
        assert_eq!(tracks[1].grouped, Some(true));
        let mut expected_state = original_tracks[1].clone();
        expected_state[12] = 0;
        expected_state[46] = 1;
        assert_eq!(tracks[1].state_bytes, expected_state);

        let before_invalid_edit = document
            .encode_lossless()
            .expect("edited project should encode");
        assert!(
            document
                .edit_playlist_track(
                    1,
                    PlaylistTrackEdit {
                        grouped: Some(true),
                        ..PlaylistTrackEdit::default()
                    },
                )
                .is_err()
        );
        assert_eq!(
            document.encode_lossless().unwrap(),
            before_invalid_edit,
            "invalid grouping should leave the project unchanged"
        );
        let reparsed = FlpDocument::parse(&before_invalid_edit)
            .expect("edited Playlist tracks should round-trip");
        assert_eq!(reparsed.playlist_tracks(), document.playlist_tracks());
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

    fn pattern_controller_record(
        position: u32,
        reserved: [u8; 2],
        channel: u8,
        flags: u8,
        value_bits: u32,
    ) -> [u8; 12] {
        let mut record = [0; 12];
        record[..4].copy_from_slice(&position.to_le_bytes());
        record[4..6].copy_from_slice(&reserved);
        record[6] = channel;
        record[7] = flags;
        record[8..12].copy_from_slice(&value_bits.to_le_bytes());
        record
    }

    fn adjacent_pattern_clip_fixture(
        left_length_ticks: u32,
        right_length_ticks: u32,
        note_position: u32,
        note_length: u32,
    ) -> FlpDocument {
        let note = note_record(note_position, 0, note_length, 60, 100);
        let mut event_stream = vec![0x40, 0, 0, 0x41, 7, 0, 0xD0, 24];
        event_stream.extend_from_slice(&note);
        event_stream.extend_from_slice(&[0xA4]);
        event_stream.extend_from_slice(&96u32.to_le_bytes());

        let mut left_clip = [0u8; 80];
        left_clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        left_clip[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        left_clip[8..12].copy_from_slice(&left_length_ticks.to_le_bytes());
        left_clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        left_clip[32..36].copy_from_slice(&1u32.to_le_bytes());
        left_clip[28..32].copy_from_slice(&1.0f32.to_le_bytes());
        left_clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        let mut right_clip = left_clip;
        right_clip[..4].copy_from_slice(&left_length_ticks.to_le_bytes());
        right_clip[8..12].copy_from_slice(&right_length_ticks.to_le_bytes());
        right_clip[32..36].copy_from_slice(&2u32.to_le_bytes());
        let mut clip_payload = left_clip.to_vec();
        clip_payload.extend_from_slice(&right_clip);

        event_stream.extend_from_slice(&[0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0]);
        append_data_event(&mut event_stream, 0xE9, &clip_payload);
        FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("Pattern Clip fixture should parse")
    }

    fn audio_clip_fixture() -> FlpDocument {
        let mut event_stream = vec![0x40, 9, 0, 0x15, 4];
        append_data_event(
            &mut event_stream,
            0xC4,
            &utf16_project_string("/samples/recording.wav"),
        );
        event_stream.extend_from_slice(&[0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0]);

        let mut pattern_clip = [0xA5u8; 80];
        pattern_clip[0..4].copy_from_slice(&1920u32.to_le_bytes());
        pattern_clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        pattern_clip[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        pattern_clip[8..12].copy_from_slice(&960u32.to_le_bytes());
        pattern_clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        pattern_clip[32..36].copy_from_slice(&1u32.to_le_bytes());
        pattern_clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        pattern_clip[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        pattern_clip[28..32].copy_from_slice(&1.0f32.to_le_bytes());
        pattern_clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());

        let mut audio_clip = pattern_clip;
        audio_clip[0..4].copy_from_slice(&0u32.to_le_bytes());
        audio_clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        audio_clip[32..36].copy_from_slice(&2u32.to_le_bytes());
        let mut clip_payload = pattern_clip.to_vec();
        clip_payload.extend_from_slice(&audio_clip);
        append_data_event(&mut event_stream, 0xE9, &clip_payload);

        let mut input = flp_fixture(&event_stream, &[], &[]);
        input[10..12].copy_from_slice(&1u16.to_le_bytes());
        FlpDocument::parse(&input).expect("Audio Clip fixture should parse")
    }

    fn merge_pattern_clips_fixture(include_unsupported_pattern_event: bool) -> FlpDocument {
        let mut event_stream = vec![0x40, 0, 0, 0x41, 7, 0];
        let receiver_note = note_record(0, 7, 48, 60, 100);
        append_data_event(&mut event_stream, 0xD0, &receiver_note);
        event_stream.extend_from_slice(&[0xA4]);
        event_stream.extend_from_slice(&96u32.to_le_bytes());

        event_stream.extend_from_slice(&[0x41, 8, 0]);
        let source_note = note_record(24, 7, 24, 64, 90);
        append_data_event(&mut event_stream, 0xD0, &source_note);
        if include_unsupported_pattern_event {
            append_project_info_string(&mut event_stream, 0xC2, "automation");
        }
        event_stream.extend_from_slice(&[0xA4]);
        event_stream.extend_from_slice(&48u32.to_le_bytes());

        event_stream.extend_from_slice(&[0x40, 7, 0, 0x15, 4, 0x62, 0, 0]);
        event_stream.extend_from_slice(&[0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0]);

        let mut source_clip = [0u8; 80];
        source_clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        source_clip[6..8].copy_from_slice(&0x5008u16.to_le_bytes());
        source_clip[8..12].copy_from_slice(&96u32.to_le_bytes());
        source_clip[12..14].copy_from_slice(&498u16.to_le_bytes());
        source_clip[28..32].copy_from_slice(&1.0f32.to_le_bytes());
        source_clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());

        let mut receiver_clip = source_clip;
        receiver_clip[0..4].copy_from_slice(&96u32.to_le_bytes());
        receiver_clip[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        receiver_clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        let mut clip_payload = source_clip.to_vec();
        clip_payload.extend_from_slice(&receiver_clip);
        append_data_event(&mut event_stream, 0xE9, &clip_payload);

        FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("Pattern Clip merge fixture should parse")
    }

    #[test]
    fn merges_selected_pattern_clips_into_the_uppermost_clip() {
        let mut document = merge_pattern_clips_fixture(false);
        let arrangement_id = document.arrangements().unwrap()[0].id;
        let original_patterns = document.patterns().expect("source patterns should decode");

        let merged_clip_index = document
            .merge_playlist_pattern_clips(arrangement_id, &[0, 1])
            .expect("different Pattern Clip scores should merge");

        assert_eq!(merged_clip_index, 0);
        let arrangement = document
            .arrangements()
            .expect("the merged arrangement should decode")
            .remove(0);
        assert_eq!(arrangement.clips.len(), 1);
        assert_eq!(arrangement.clips[0].position_ticks, 0);
        assert_eq!(arrangement.clips[0].length_ticks, 192);
        assert_eq!(arrangement.clips[0].track_index, Some(0));
        assert_eq!(
            arrangement.clips[0].target(),
            PlaylistClipTarget::Pattern { id: 9 }
        );

        let patterns = document.patterns().expect("merged pattern should decode");
        assert_eq!(patterns.len(), 3);
        assert_eq!(&patterns[..2], original_patterns.as_slice());
        let merged_pattern = patterns
            .iter()
            .find(|pattern| pattern.id == 9)
            .expect("the merged Pattern should be created");
        assert_eq!(merged_pattern.name.as_deref(), Some("Merged"));
        assert_eq!(merged_pattern.length_ticks, Some(192));
        assert_eq!(
            merged_pattern
                .notes
                .iter()
                .map(|note| (note.position, note.key, note.length))
                .collect::<Vec<_>>(),
            [(24, 64, 24), (72, 64, 24), (96, 60, 48)]
        );

        let encoded = document
            .encode_lossless()
            .expect("the merged project should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("the merged project should parse");
        assert_eq!(reparsed.patterns().unwrap(), patterns);
        assert_eq!(reparsed.arrangements().unwrap()[0].clips, arrangement.clips);
    }

    #[test]
    fn pattern_clip_merge_rejects_unmodeled_pattern_data_atomically() {
        let mut document = merge_pattern_clips_fixture(true);
        let arrangement_id = document.arrangements().unwrap()[0].id;
        let original = document
            .encode_lossless()
            .expect("the original project should encode");

        assert!(
            document
                .merge_playlist_pattern_clips(arrangement_id, &[0, 1])
                .is_err()
        );
        assert_eq!(
            document
                .encode_lossless()
                .expect("the project should still encode after rejection"),
            original
        );
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
        let mut event_stream = vec![0x17, 2, 0xF2, 28];
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
            .filter(|event| !matches!(event.opcode(), 0x17 | 0x64 | 0x28))
            .map(|event| event.wire_bytes().to_vec())
            .collect();
        document
            .set_project_settings(ProjectSettingsEdit {
                play_truncated_notes_in_clips: Some(false),
                fast_declick_for_cut_groups: Some(false),
                pan_law_raw: Some(0),
                ..ProjectSettingsEdit::default()
            })
            .expect("supported Project settings should be editable");
        assert_eq!(
            document.project_settings(),
            Some(super::ProjectSettings {
                play_truncated_notes_in_clips: false,
                fast_declick_for_cut_groups: false,
            })
        );
        assert_eq!(document.metadata().pan_law_raw(), Some(0));
        assert_eq!(
            document
                .events()
                .iter()
                .filter(|event| !matches!(event.opcode(), 0x17 | 0x64 | 0x28))
                .map(|event| event.wire_bytes().to_vec())
                .collect::<Vec<_>>(),
            unrelated_events
        );
        assert!(document.events().iter().any(|event| event.opcode() == 0x64));

        document
            .set_project_settings(ProjectSettingsEdit {
                play_truncated_notes_in_clips: Some(true),
                fast_declick_for_cut_groups: Some(true),
                pan_law_raw: Some(2),
                ..ProjectSettingsEdit::default()
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
        assert_eq!(reparsed.metadata().pan_law_raw(), Some(2));
        assert_eq!(reparsed.trailing_bytes(), &[0xD1, 0xD2]);
    }

    #[test]
    fn project_time_signature_edits_preserve_other_events_and_roundtrip() {
        let original = flp_fixture(
            &[0x11, 4, 0x12, 4, 0x20, 7, 0x40, 7, 0, 0x15, 0],
            &[0xA1, 0xA2],
            &[0xD1, 0xD2],
        );
        let mut document = FlpDocument::parse(&original).expect("fixture should parse");
        assert_eq!(document.metadata().time_signature(), Some((4, 4)));
        let unrelated_events: Vec<_> = document
            .events()
            .iter()
            .filter(|event| !matches!(event.opcode(), 0x11 | 0x12))
            .map(|event| event.wire_bytes().to_vec())
            .collect();

        document
            .set_project_settings(ProjectSettingsEdit {
                time_signature: Some((7, 8)),
                ..ProjectSettingsEdit::default()
            })
            .expect("the global project time signature should be editable");
        assert_eq!(document.metadata().time_signature(), Some((7, 8)));
        assert_eq!(
            document
                .events()
                .iter()
                .filter(|event| !matches!(event.opcode(), 0x11 | 0x12))
                .map(|event| event.wire_bytes().to_vec())
                .collect::<Vec<_>>(),
            unrelated_events
        );

        let encoded = document
            .encode_lossless()
            .expect("edited project should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should reparse");
        assert_eq!(reparsed.metadata().time_signature(), Some((7, 8)));
        assert_eq!(reparsed.trailing_bytes(), &[0xD1, 0xD2]);
    }

    #[test]
    fn project_time_signature_can_be_added_before_channels_without_reading_channel_bytes() {
        let original = flp_fixture(&[0x40, 7, 0, 0x11, 9, 0x12, 3, 0x15, 0], &[], &[0xD1]);
        let mut document = FlpDocument::parse(&original).expect("fixture should parse");
        assert_eq!(document.metadata().time_signature(), None);
        let channel_events: Vec<_> = document
            .events()
            .iter()
            .skip_while(|event| event.opcode() != 0x40)
            .map(|event| event.wire_bytes().to_vec())
            .collect();

        document
            .set_project_settings(ProjectSettingsEdit {
                time_signature: Some((3, 8)),
                ..ProjectSettingsEdit::default()
            })
            .expect("the project signature should be inserted before channel data");

        assert_eq!(document.metadata().time_signature(), Some((3, 8)));
        let first_channel = document
            .events()
            .iter()
            .position(|event| event.opcode() == 0x40)
            .expect("the channel marker should remain");
        assert_eq!(
            document.events()[..first_channel]
                .iter()
                .filter(|event| matches!(event.opcode(), 0x11 | 0x12))
                .map(|event| (event.opcode(), event.payload()[0]))
                .collect::<Vec<_>>(),
            [(0x11, 3), (0x12, 8)]
        );
        assert_eq!(
            document.events()[first_channel..]
                .iter()
                .map(|event| event.wire_bytes().to_vec())
                .collect::<Vec<_>>(),
            channel_events
        );
        let encoded = document
            .encode_lossless()
            .expect("edited project should encode");
        assert_eq!(
            FlpDocument::parse(&encoded)
                .expect("edited project should reparse")
                .metadata()
                .time_signature(),
            Some((3, 8))
        );

        for partial_events in [&[0x11, 4][..], &[0x12, 8][..]] {
            let mut partial = FlpDocument::parse(&flp_fixture(partial_events, &[], &[]))
                .expect("partial-signature fixture should parse");
            partial
                .set_project_settings(ProjectSettingsEdit {
                    time_signature: Some((5, 16)),
                    ..ProjectSettingsEdit::default()
                })
                .expect("a missing time-signature byte should be added");
            assert_eq!(partial.metadata().time_signature(), Some((5, 16)));
        }
    }

    #[test]
    fn ambiguous_or_invalid_global_time_signatures_are_refused_without_mutation() {
        for event_stream in [&[0x11, 4, 0x11, 3, 0x12, 4][..], &[0x11, 0, 0x12, 4][..]] {
            let original = flp_fixture(event_stream, &[], &[0xD1]);
            let mut document = FlpDocument::parse(&original).expect("fixture should parse");
            assert_eq!(document.metadata().time_signature(), None);
            assert!(
                document
                    .set_project_settings(ProjectSettingsEdit {
                        time_signature: Some((5, 8)),
                        ..ProjectSettingsEdit::default()
                    })
                    .is_err()
            );
            assert_eq!(document.encode_lossless().unwrap(), original);
        }
    }

    #[test]
    fn pan_law_defaults_to_circular_and_can_be_added_without_advanced_settings() {
        let original = flp_fixture(&[0x20, 7, 0x40, 0, 0], &[], &[0xD1, 0xD2]);
        let mut document = FlpDocument::parse(&original).expect("fixture should parse");
        assert_eq!(document.metadata().pan_law_raw(), None);

        document
            .set_project_settings(ProjectSettingsEdit {
                pan_law_raw: Some(0),
                ..ProjectSettingsEdit::default()
            })
            .expect("the default pan law should not need a new event");
        assert!(!document.events().iter().any(|event| event.opcode() == 0x17));

        document
            .set_project_settings(ProjectSettingsEdit {
                pan_law_raw: Some(2),
                ..ProjectSettingsEdit::default()
            })
            .expect("pan law should be editable without the Advanced settings block");
        assert_eq!(document.metadata().pan_law_raw(), Some(2));
        let encoded = document.encode_lossless().expect("document should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should reparse");
        assert_eq!(reparsed.metadata().pan_law_raw(), Some(2));
        let event_order = reparsed
            .events()
            .iter()
            .map(FlpEvent::opcode)
            .collect::<Vec<_>>();
        assert!(
            event_order.iter().position(|opcode| *opcode == 0x17)
                < event_order.iter().position(|opcode| *opcode == 0x40)
        );
        assert_eq!(reparsed.trailing_bytes(), &[0xD1, 0xD2]);
    }

    #[test]
    fn pan_law_edits_reject_duplicate_global_events_without_mutation() {
        let original = flp_fixture(&[0x17, 0, 0x17, 2], &[], &[0xD1]);
        let mut document = FlpDocument::parse(&original).expect("fixture should parse");
        assert_eq!(document.metadata().pan_law_raw(), None);
        assert!(
            document
                .set_project_settings(ProjectSettingsEdit {
                    pan_law_raw: Some(0),
                    ..ProjectSettingsEdit::default()
                })
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), original);
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
    fn reads_and_sets_global_swing_mix_losslessly() {
        let tail = [0xD1, 0xD2];
        for (initial_mix, requested_mix, expected_mix, inserted) in [
            (None, 32u8, Some(32u8), true),
            (Some(40u8), 77, Some(77u8), false),
            (None, 0, None, false),
        ] {
            let mut event_stream = vec![0x22, 0x99];
            if let Some(initial_mix) = initial_mix {
                event_stream.extend_from_slice(&[0x0B, initial_mix]);
            }
            event_stream.extend_from_slice(&[0x40, 7, 0, 0x15, 0, 0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &tail);
            let mut document = FlpDocument::parse(&input).expect("fixture should parse");
            assert_eq!(document.metadata().global_swing_mix_raw(), initial_mix);
            assert_eq!(
                document.metadata().global_swing_mix(),
                initial_mix.unwrap_or(0)
            );
            assert_eq!(document.encode_lossless().unwrap(), input);

            document
                .set_global_swing_mix(requested_mix)
                .expect("global swing mix should update");

            assert_eq!(document.metadata().global_swing_mix_raw(), expected_mix);
            assert_eq!(
                document.metadata().global_swing_mix(),
                expected_mix.unwrap_or(0)
            );
            let swing_events = document
                .events()
                .iter()
                .filter(|event| event.opcode() == 0x0B)
                .collect::<Vec<_>>();
            if let Some(expected_mix) = expected_mix {
                assert_eq!(swing_events.len(), 1);
                assert_eq!(swing_events[0].payload(), &[expected_mix]);
            } else {
                assert!(swing_events.is_empty());
            }
            if inserted {
                assert_eq!(document.events()[1].wire_bytes(), &[0x0B, 32]);
                assert_eq!(document.events()[2].opcode(), 0x40);
            }

            let encoded = document.encode_lossless().expect("document should encode");
            assert!(encoded.ends_with(&tail));
            let round_trip = FlpDocument::parse(&encoded).expect("edited project should parse");
            assert_eq!(round_trip.metadata().global_swing_mix_raw(), expected_mix);
        }
    }

    #[test]
    fn global_swing_ignores_channel_scoped_and_duplicate_events() {
        let mut channel_scoped = FlpDocument::parse(&flp_fixture(
            &[0x40, 7, 0, 0x15, 0, 0x0B, 16, 0x62, 0, 0],
            &[],
            &[],
        ))
        .expect("fixture should parse");
        assert_eq!(channel_scoped.metadata().global_swing_mix_raw(), None);
        channel_scoped
            .set_global_swing_mix(64)
            .expect("global swing should insert before channel data");
        assert_eq!(channel_scoped.metadata().global_swing_mix_raw(), Some(64));
        assert_eq!(
            channel_scoped
                .events()
                .iter()
                .filter(|event| event.opcode() == 0x0B)
                .count(),
            2
        );

        let mut duplicate = FlpDocument::parse(&flp_fixture(
            &[0x0B, 0, 0x0B, 64, 0x40, 7, 0, 0x15, 0, 0x62, 0, 0],
            &[],
            &[],
        ))
        .expect("fixture should parse");
        let original = duplicate.encode_lossless().unwrap();
        assert!(duplicate.set_global_swing_mix(96).is_err());
        assert_eq!(duplicate.encode_lossless().unwrap(), original);

        assert!(duplicate.set_global_swing_mix(129).is_err());
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
    fn creates_playlist_pattern_clip_from_a_lossless_clip_template() {
        let mut template = [0xA5; 80];
        template[0..4].copy_from_slice(&1920u32.to_le_bytes());
        template[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        template[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        template[8..12].copy_from_slice(&960u32.to_le_bytes());
        template[12..14].copy_from_slice(&499u16.to_le_bytes());
        template[14..16].copy_from_slice(&0x1234u16.to_le_bytes());
        template[32..36].copy_from_slice(&10u32.to_le_bytes());
        template[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        template[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        template[28..32].copy_from_slice(&1.0f32.to_le_bytes());

        let mut event_stream = Vec::new();
        event_stream.extend_from_slice(&[0x41, 7, 0]);
        append_data_event(&mut event_stream, 0xD0, &[]);
        event_stream.push(0xA4);
        event_stream.extend_from_slice(&1920u32.to_le_bytes());
        event_stream.extend_from_slice(&[0x41, 8, 0]);
        append_data_event(&mut event_stream, 0xD0, &[]);
        event_stream.push(0xA4);
        event_stream.extend_from_slice(&720u32.to_le_bytes());
        event_stream.extend_from_slice(&[0x63, 3, 0]);
        append_data_event(&mut event_stream, 0xE9, &template);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        let inserted_index = document
            .create_playlist_pattern_clip(3, 8, 3840, 720, 4)
            .expect("a clip should be created from the known record");
        assert_eq!(inserted_index, 1);

        let arrangements = document
            .arrangements()
            .expect("the updated Playlist should decode");
        assert_eq!(arrangements[0].clips.len(), 2);
        let added_clip = &arrangements[0].clips[inserted_index];
        assert_eq!(added_clip.position_ticks, 3840);
        assert_eq!(added_clip.length_ticks, 720);
        assert_eq!(added_clip.track_index, Some(4));
        assert_eq!(added_clip.target(), PlaylistClipTarget::Pattern { id: 8 });
        assert_eq!(added_clip.clip_id, Some(11));

        let mut expected_new_record = template;
        expected_new_record[0..4].copy_from_slice(&3840u32.to_le_bytes());
        expected_new_record[6..8].copy_from_slice(&0x5008u16.to_le_bytes());
        expected_new_record[8..12].copy_from_slice(&720u32.to_le_bytes());
        expected_new_record[12..14].copy_from_slice(&495u16.to_le_bytes());
        expected_new_record[32..36].copy_from_slice(&11u32.to_le_bytes());
        let mut expected_payload = template.to_vec();
        expected_payload.extend_from_slice(&expected_new_record);
        let clip_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist clip event should remain present");
        assert_eq!(clip_event.payload(), expected_payload);

        let encoded = document
            .encode_lossless()
            .expect("the edited project should encode");
        FlpDocument::parse(&encoded).expect("the edited project should parse again");
    }

    #[test]
    fn creates_playlist_audio_clip_from_an_audio_clip_template() {
        let mut document = audio_clip_fixture();
        let original_pattern_clip = document.arrangements().unwrap()[0].clips[0].clone();
        let original_audio_record = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .unwrap()
            .payload()[80..160]
            .to_vec();

        let inserted_index = document
            .create_playlist_audio_clip(3, 9, 3840, 720, 4)
            .expect("an Audio Clip should be created from its channel template");
        assert_eq!(inserted_index, 2);

        let arrangement = &document.arrangements().unwrap()[0];
        assert_eq!(arrangement.clips.len(), 3);
        assert_eq!(arrangement.clips[0], original_pattern_clip);
        let added_clip = &arrangement.clips[inserted_index];
        assert_eq!(added_clip.position_ticks, 3840);
        assert_eq!(added_clip.length_ticks, 720);
        assert_eq!(added_clip.track_index, Some(4));
        assert_eq!(added_clip.target(), PlaylistClipTarget::Channel { id: 9 });
        assert_eq!(added_clip.clip_id, Some(3));

        let mut expected_new_record = original_audio_record.clone();
        expected_new_record[0..4].copy_from_slice(&3840u32.to_le_bytes());
        expected_new_record[6..8].copy_from_slice(&9u16.to_le_bytes());
        expected_new_record[8..12].copy_from_slice(&720u32.to_le_bytes());
        expected_new_record[12..14].copy_from_slice(&495u16.to_le_bytes());
        expected_new_record[32..36].copy_from_slice(&3u32.to_le_bytes());
        let event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist clip event should remain present");
        assert_eq!(&event.payload()[80..160], original_audio_record);
        assert_eq!(&event.payload()[160..240], expected_new_record);

        let encoded = document
            .encode_lossless()
            .expect("edited project should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should parse");
        assert_eq!(reparsed.arrangements().unwrap()[0].clips.len(), 3);
    }

    #[test]
    fn playlist_clip_duplicate_and_paste_assign_unique_clip_ids() {
        let mut duplicate_document = audio_clip_fixture();
        duplicate_document
            .duplicate_playlist_clip(3, 1, Some(3_840), Some(495))
            .expect("the Audio Clip should duplicate");
        let duplicate_arrangements = duplicate_document.arrangements().unwrap();
        let duplicate_arrangement = &duplicate_arrangements[0];
        assert_eq!(duplicate_arrangement.clips[1].clip_id, Some(2));
        assert_eq!(duplicate_arrangement.clips[2].clip_id, Some(3));
        assert_eq!(duplicate_arrangement.clips[2].position_ticks, 3_840);
        assert_eq!(duplicate_arrangement.clips[2].track_index, Some(4));

        let mut paste_document = audio_clip_fixture();
        let clipboard = paste_document
            .copy_playlist_clip(3, 1)
            .expect("the Audio Clip should copy");
        paste_document
            .paste_playlist_clip(3, &clipboard, 3_840, 495)
            .expect("the copied Audio Clip should paste");
        let pasted_arrangements = paste_document.arrangements().unwrap();
        let pasted_arrangement = &pasted_arrangements[0];
        assert_eq!(pasted_arrangement.clips[1].clip_id, Some(2));
        assert_eq!(pasted_arrangement.clips[2].clip_id, Some(3));
        assert_eq!(
            pasted_arrangement.clips[2].target(),
            PlaylistClipTarget::Channel { id: 9 }
        );
    }

    #[test]
    fn rejects_playlist_audio_clip_creation_for_invalid_channels_or_length_atomically() {
        let input = audio_clip_fixture().encode_lossless().unwrap();
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        assert!(document.create_playlist_audio_clip(3, 9, 0, 0, 0).is_err());
        assert!(
            document
                .create_playlist_audio_clip(3, 7, 0, 960, 0)
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn rejects_playlist_audio_clip_creation_without_an_audio_template_atomically() {
        let mut document = audio_clip_fixture();
        document
            .delete_playlist_clip(3, 1)
            .expect("the only Audio Clip template should be removable");
        let input = document.encode_lossless().unwrap();

        assert!(
            document
                .create_playlist_audio_clip(3, 9, 0, 960, 0)
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn rejects_playlist_pattern_clip_creation_without_a_template_atomically() {
        let mut event_stream = vec![0x41, 7, 0];
        append_data_event(&mut event_stream, 0xD0, &[]);
        event_stream.extend_from_slice(&[0x63, 3, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        assert!(
            document
                .create_playlist_pattern_clip(3, 7, 0, 960, 0)
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);

        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");
        assert!(
            document
                .create_playlist_pattern_clip(3, 7, 0, 0, 0)
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn deletes_playlist_clip_records_without_rewriting_their_neighbors() {
        let mut first = [0xA5; 80];
        first[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        first[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        first[8..12].copy_from_slice(&960u32.to_le_bytes());
        first[12..14].copy_from_slice(&499u16.to_le_bytes());
        first[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        first[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        first[28..32].copy_from_slice(&1.0f32.to_le_bytes());
        let mut second = first;
        second[0..4].copy_from_slice(&1920u32.to_le_bytes());
        second[6..8].copy_from_slice(&0x5008u16.to_le_bytes());

        let mut clips_payload = first.to_vec();
        clips_payload.extend_from_slice(&second);
        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"26.0.0\0");
        event_stream.extend_from_slice(&[0x63, 3, 0]);
        append_data_event(&mut event_stream, 0xE9, &clips_payload);
        let input = flp_fixture(&event_stream, &[0xA1], &[0xB2]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        assert!(document.delete_playlist_clip(3, 2).is_err());
        assert_eq!(document.encode_lossless().unwrap(), input);

        document
            .delete_playlist_clip(3, 0)
            .expect("the first clip should be removable");
        let arrangements = document
            .arrangements()
            .expect("the Playlist should decode after deletion");
        assert_eq!(arrangements[0].clips.len(), 1);
        assert_eq!(
            arrangements[0].clips[0].target(),
            PlaylistClipTarget::Pattern { id: 8 }
        );
        let clip_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist event should remain present");
        assert_eq!(clip_event.payload(), second);

        document
            .delete_playlist_clip(3, 0)
            .expect("the final clip should be removable");
        assert!(document.arrangements().unwrap()[0].clips.is_empty());
        let encoded = document
            .encode_lossless()
            .expect("the empty Playlist should encode");
        FlpDocument::parse(&encoded).expect("the edited project should parse again");
    }

    #[test]
    fn splits_unstretched_playlist_audio_clips_and_preserves_the_source_window() {
        let mut clip = [0xA5; 80];
        clip[0..4].copy_from_slice(&0u32.to_le_bytes());
        clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        clip[8..12].copy_from_slice(&240u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[32..36].copy_from_slice(&1u32.to_le_bytes());
        clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        clip[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        clip[28..32].copy_from_slice(&1_000.0f32.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());

        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        let right_clip = document
            .split_playlist_audio_clip(3, 0, 96, None)
            .expect("a split inside the available source audio should succeed");
        assert_eq!(right_clip, 1);

        let clips = &document.arrangements().unwrap()[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].position_ticks, 0);
        assert_eq!(clips[0].length_ticks, 96);
        assert_eq!(clips[1].position_ticks, 96);
        assert_eq!(clips[1].length_ticks, 144);
        assert_eq!(clips[0].clip_id, Some(1));
        assert_eq!(clips[1].clip_id, Some(2));
        assert_eq!(clips[0].start_offset, 0.0);
        assert_eq!(clips[1].end_offset, 1_000.0);
        assert!((clips[0].end_offset - 428.57144).abs() < 0.001);
        assert_eq!(clips[1].start_offset, clips[0].end_offset);

        let mut expected_left = clip;
        expected_left[8..12].copy_from_slice(&96u32.to_le_bytes());
        expected_left[28..32].copy_from_slice(&clips[0].end_offset.to_le_bytes());
        let mut expected_right = clip;
        expected_right[0..4].copy_from_slice(&96u32.to_le_bytes());
        expected_right[8..12].copy_from_slice(&144u32.to_le_bytes());
        expected_right[24..28].copy_from_slice(&clips[0].end_offset.to_le_bytes());
        expected_right[32..36].copy_from_slice(&2u32.to_le_bytes());
        let clip_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist clip event should remain present");
        let mut expected_payload = expected_left.to_vec();
        expected_payload.extend_from_slice(&expected_right);
        assert_eq!(clip_event.payload(), expected_payload);

        let encoded = document
            .encode_lossless()
            .expect("the split project should encode");
        FlpDocument::parse(&encoded).expect("the split project should parse again");
    }

    #[test]
    fn joins_adjacent_playlist_audio_clips_back_to_their_original_record() {
        let mut clip = [0xA5; 80];
        clip[0..4].copy_from_slice(&0u32.to_le_bytes());
        clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        clip[8..12].copy_from_slice(&240u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        clip[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        clip[28..32].copy_from_slice(&1_000.0f32.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());

        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        document
            .split_playlist_audio_clip(3, 0, 96, None)
            .expect("the sample should split into two contiguous clips");
        let joined_index = document
            .join_adjacent_playlist_audio_clips(3, 0, 1)
            .expect("the contiguous audio segments should join");

        assert_eq!(joined_index, 0);
        let arrangements = document
            .arrangements()
            .expect("the joined arrangement should decode");
        assert_eq!(arrangements[0].clips.len(), 1);
        assert_eq!(arrangements[0].clips[0].length_ticks, 240);
        assert_eq!(arrangements[0].clips[0].start_offset, 0.0);
        assert_eq!(arrangements[0].clips[0].end_offset, 1_000.0);
        let clip_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist event should remain present");
        assert_eq!(clip_event.payload(), clip);

        let encoded = document
            .encode_lossless()
            .expect("the joined project should encode");
        FlpDocument::parse(&encoded).expect("the joined project should parse again");
    }

    #[test]
    fn joins_adjacent_playlist_pattern_clips_when_repeat_boundaries_match() {
        let mut document = adjacent_pattern_clip_fixture(192, 96, 0, 48);
        let joined_index = document
            .join_adjacent_playlist_pattern_clips(3, 0, 1)
            .expect("aligned Pattern Clips that share a pattern should join");

        assert_eq!(joined_index, 0);
        let arrangements = document
            .arrangements()
            .expect("the joined arrangement should decode");
        assert_eq!(arrangements[0].clips.len(), 1);
        assert_eq!(arrangements[0].clips[0].position_ticks, 0);
        assert_eq!(arrangements[0].clips[0].length_ticks, 288);
        assert_eq!(
            arrangements[0].clips[0].target(),
            PlaylistClipTarget::Pattern { id: 7 }
        );
        assert_eq!(arrangements[0].clips[0].clip_id, Some(1));

        let clip_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist clip event should remain present");
        assert_eq!(clip_event.payload().len(), 80);
        assert_eq!(
            u32::from_le_bytes(clip_event.payload()[8..12].try_into().unwrap()),
            288
        );
        let encoded = document
            .encode_lossless()
            .expect("the joined project should encode");
        FlpDocument::parse(&encoded).expect("the joined project should parse again");
    }

    #[test]
    fn playlist_pattern_clip_join_rejects_unsafe_boundaries_atomically() {
        for (left_length, note_position, note_length) in [(144, 0, 48), (192, 80, 24), (192, 96, 0)]
        {
            let mut document =
                adjacent_pattern_clip_fixture(left_length, 96, note_position, note_length);
            let before_join = document
                .encode_lossless()
                .expect("the original project should encode");

            assert!(
                document
                    .join_adjacent_playlist_pattern_clips(3, 0, 1)
                    .is_err()
            );
            assert_eq!(
                document
                    .encode_lossless()
                    .expect("the project should still encode after a rejected join"),
                before_join
            );
        }
    }

    #[test]
    fn playlist_audio_clip_join_rejects_discontinuous_source_windows_atomically() {
        let mut clip = [0xA5; 80];
        clip[0..4].copy_from_slice(&0u32.to_le_bytes());
        clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        clip[8..12].copy_from_slice(&240u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        clip[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        clip[28..32].copy_from_slice(&1_000.0f32.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");
        document
            .split_playlist_audio_clip(3, 0, 96, None)
            .expect("the sample should split into two clips");
        document
            .edit_playlist_clip(
                3,
                1,
                PlaylistClipEdit {
                    start_offset: Some(430.0),
                    ..PlaylistClipEdit::default()
                },
            )
            .expect("the right source offset should be editable");
        let before_join = document
            .encode_lossless()
            .expect("the split project should encode");

        assert!(
            document
                .join_adjacent_playlist_audio_clips(3, 0, 1)
                .is_err()
        );
        assert_eq!(
            document
                .encode_lossless()
                .expect("the project should still encode after a rejected join"),
            before_join
        );
    }

    #[test]
    fn slips_playlist_audio_clip_source_without_moving_its_timeline_bounds() {
        let mut clip = [0xA5; 80];
        clip[0..4].copy_from_slice(&120u32.to_le_bytes());
        clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        clip[8..12].copy_from_slice(&384u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        clip[24..28].copy_from_slice(&100.0f32.to_le_bytes());
        clip[28..32].copy_from_slice(&900.0f32.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        document
            .slip_playlist_audio_clip(3, 0, -100.0, 1_000.0)
            .expect("the window should slip to the sample start");
        document
            .slip_playlist_audio_clip(3, 0, 200.0, 1_000.0)
            .expect("the window should slip right while staying in the sample");

        let clips = &document.arrangements().unwrap()[0].clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].position_ticks, 120);
        assert_eq!(clips[0].length_ticks, 384);
        assert_eq!(clips[0].start_offset, 200.0);
        assert_eq!(clips[0].end_offset, 1_000.0);
        let mut expected_clip = clip;
        expected_clip[24..28].copy_from_slice(&200.0f32.to_le_bytes());
        expected_clip[28..32].copy_from_slice(&1_000.0f32.to_le_bytes());
        let clip_event = document
            .events()
            .iter()
            .find(|event| event.opcode() == 0xE9)
            .expect("the Playlist event should remain present");
        assert_eq!(clip_event.payload(), expected_clip);

        let before_rejected_slip = document
            .encode_lossless()
            .expect("the slipped project should encode");
        assert!(
            document
                .slip_playlist_audio_clip(3, 0, 1.0, 1_000.0)
                .is_err()
        );
        assert_eq!(
            document
                .encode_lossless()
                .expect("the project should encode after a rejected slip"),
            before_rejected_slip
        );
        let encoded = document
            .encode_lossless()
            .expect("the slipped project should encode");
        FlpDocument::parse(&encoded).expect("the slipped project should parse again");
    }

    #[test]
    fn playlist_audio_clip_slip_rejects_full_source_without_room_atomically() {
        let mut clip = [0xA5; 80];
        clip[0..4].copy_from_slice(&0u32.to_le_bytes());
        clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        clip[8..12].copy_from_slice(&384u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        clip[24..28].copy_from_slice(&(-1.0f32).to_le_bytes());
        clip[28..32].copy_from_slice(&(-1.0f32).to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        assert!(
            document
                .slip_playlist_audio_clip(3, 0, 1.0, 1_000.0)
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn playlist_audio_clip_split_rejects_unsupported_edits_atomically() {
        let mut clip = [0xA5; 80];
        clip[0..4].copy_from_slice(&0u32.to_le_bytes());
        clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        clip[8..12].copy_from_slice(&240u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        clip[24..28].copy_from_slice(&(-1.0f32).to_le_bytes());
        clip[28..32].copy_from_slice(&(-1.0f32).to_le_bytes());
        clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());
        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        assert!(document.split_playlist_audio_clip(3, 0, 96, None).is_err());
        assert_eq!(document.encode_lossless().unwrap(), input);
        let mut full_source_document =
            FlpDocument::parse(&input).expect("the full-source project fixture should parse");
        full_source_document
            .split_playlist_audio_clip(3, 0, 96, Some(1_000.0))
            .expect("a split should succeed when the sample duration is supplied");
        let full_source_clips = &full_source_document.arrangements().unwrap()[0].clips;
        assert_eq!(full_source_clips[0].start_offset, 0.0);
        assert!((full_source_clips[0].end_offset - 428.57144).abs() < 0.001);
        assert_eq!(
            full_source_clips[1].start_offset,
            full_source_clips[0].end_offset
        );
        assert_eq!(full_source_clips[1].end_offset, 1_000.0);
        assert!(
            document
                .split_playlist_audio_clip(3, 0, 96, Some(200.0))
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
        assert!(
            document
                .split_playlist_audio_clip(3, 0, 240, Some(1_000.0))
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);

        clip[64..72].copy_from_slice(&0.5f64.to_le_bytes());
        let mut event_stream = vec![0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0, 0x63, 3, 0];
        append_data_event(&mut event_stream, 0xE9, &clip);
        let scaled_input = flp_fixture(&event_stream, &[], &[]);
        let mut scaled_document =
            FlpDocument::parse(&scaled_input).expect("the scaled project fixture should parse");
        assert!(
            scaled_document
                .split_playlist_audio_clip(3, 0, 96, Some(1_000.0))
                .is_err()
        );
        assert_eq!(scaled_document.encode_lossless().unwrap(), scaled_input);
        assert!(
            scaled_document
                .slip_playlist_audio_clip(3, 0, 1.0, 1_000.0)
                .is_err()
        );
        assert_eq!(scaled_document.encode_lossless().unwrap(), scaled_input);
    }

    #[test]
    fn playlist_audio_clip_split_rejects_tempo_automation_atomically() {
        let mut audio_clip = [0xA5; 80];
        audio_clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        audio_clip[6..8].copy_from_slice(&9u16.to_le_bytes());
        audio_clip[8..12].copy_from_slice(&240u32.to_le_bytes());
        audio_clip[12..14].copy_from_slice(&499u16.to_le_bytes());
        audio_clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        audio_clip[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        audio_clip[28..32].copy_from_slice(&1_000.0f32.to_le_bytes());
        audio_clip[64..72].copy_from_slice(&1.0f64.to_le_bytes());

        let mut tempo_clip = [0xA5; 80];
        tempo_clip[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        tempo_clip[6..8].copy_from_slice(&10u16.to_le_bytes());
        tempo_clip[8..12].copy_from_slice(&240u32.to_le_bytes());
        tempo_clip[12..14].copy_from_slice(&498u16.to_le_bytes());
        tempo_clip[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        let mut clips_payload = audio_clip.to_vec();
        clips_payload.extend_from_slice(&tempo_clip);

        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"26.0.0\0");
        event_stream.extend_from_slice(&[0x40, 9, 0, 0x15, 4, 0x48, 9, 0]);
        event_stream.extend_from_slice(&[0x40, 10, 0, 0x15, 5, 0x48, 10, 0]);
        append_project_info_string(&mut event_stream, 0xCB, "TEMPO");
        event_stream.extend_from_slice(&[0x62, 0, 0, 0x63, 3, 0]);
        append_data_event(&mut event_stream, 0xE9, &clips_payload);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        assert!(document.split_playlist_audio_clip(3, 0, 96, None).is_err());
        assert_eq!(document.encode_lossless().unwrap(), input);
        assert!(
            document
                .slip_playlist_audio_clip(3, 0, 1.0, 1_000.0)
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn playlist_clip_clipboard_survives_cut_and_pastes_into_an_empty_arrangement() {
        let mut source_record = [0xA5; 80];
        source_record[0..4].copy_from_slice(&1_920u32.to_le_bytes());
        source_record[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        source_record[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        source_record[8..12].copy_from_slice(&960u32.to_le_bytes());
        source_record[12..14].copy_from_slice(&499u16.to_le_bytes());
        source_record[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        source_record[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        source_record[28..32].copy_from_slice(&1.0f32.to_le_bytes());

        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"26.0.0\0");
        event_stream.extend_from_slice(&[0x63, 3, 0]);
        append_data_event(&mut event_stream, 0xE9, &source_record);
        event_stream.extend_from_slice(&[0x62, 0, 0, 0x63, 4, 0, 0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[0xAA, 0xBB]);
        let mut document = FlpDocument::parse(&input).expect("the project fixture should parse");

        let clipboard = document
            .copy_playlist_clip(3, 0)
            .expect("the first clip should copy losslessly");
        assert_eq!(clipboard.raw_record, source_record);
        document
            .delete_playlist_clip(3, 0)
            .expect("the source clip should be cut from its arrangement");
        let inserted_index = document
            .paste_playlist_clip(4, &clipboard, 3_840, 495)
            .expect("the clip should paste into the empty arrangement");

        let arrangements = document
            .arrangements()
            .expect("both arrangements should decode after paste");
        assert!(arrangements[0].clips.is_empty());
        assert_eq!(inserted_index, 0);
        assert_eq!(arrangements[1].clips.len(), 1);
        let pasted = &arrangements[1].clips[0];
        assert_eq!(pasted.position_ticks, 3_840);
        assert_eq!(pasted.raw_track_index, 495);
        assert_eq!(pasted.track_index, Some(4));
        assert_eq!(pasted.target(), PlaylistClipTarget::Pattern { id: 7 });
        assert_eq!(
            pasted.clip_id,
            Some(u32::from_le_bytes(
                source_record[32..36].try_into().unwrap()
            ))
        );

        let mut expected_record = source_record;
        expected_record[0..4].copy_from_slice(&3_840u32.to_le_bytes());
        expected_record[12..14].copy_from_slice(&495u16.to_le_bytes());
        let clip_events = document
            .events()
            .iter()
            .filter(|event| event.opcode() == 0xE9)
            .collect::<Vec<_>>();
        assert_eq!(clip_events.len(), 2);
        assert!(clip_events[0].payload().is_empty());
        assert_eq!(clip_events[1].payload(), expected_record);

        let encoded = document
            .encode_lossless()
            .expect("the edited project should encode");
        FlpDocument::parse(&encoded).expect("the edited project should parse again");
    }

    #[test]
    fn playlist_clip_paste_rejects_a_different_record_layout_atomically() {
        let mut source_record = [0xA5; 80];
        source_record[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        source_record[6..8].copy_from_slice(&0x5007u16.to_le_bytes());
        source_record[8..12].copy_from_slice(&960u32.to_le_bytes());
        source_record[12..14].copy_from_slice(&499u16.to_le_bytes());
        source_record[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        source_record[24..28].copy_from_slice(&0.0f32.to_le_bytes());
        source_record[28..32].copy_from_slice(&1.0f32.to_le_bytes());
        let mut source_events = Vec::new();
        append_data_event(&mut source_events, 0xC7, b"26.0.0\0");
        source_events.extend_from_slice(&[0x63, 3, 0]);
        append_data_event(&mut source_events, 0xE9, &source_record);
        let source = FlpDocument::parse(&flp_fixture(&source_events, &[], &[]))
            .expect("the source fixture should parse");
        let clipboard = source
            .copy_playlist_clip(3, 0)
            .expect("the source clip should copy");

        let mut target_record = [0x5A; 32];
        target_record[4..6].copy_from_slice(&0x5000u16.to_le_bytes());
        target_record[6..8].copy_from_slice(&0x5008u16.to_le_bytes());
        target_record[8..12].copy_from_slice(&960u32.to_le_bytes());
        target_record[12..14].copy_from_slice(&499u16.to_le_bytes());
        target_record[20..24].copy_from_slice(&[0x40, 0x64, 0x80, 0x80]);
        let mut target_events = Vec::new();
        append_data_event(&mut target_events, 0xC7, b"20.0.0\0");
        target_events.extend_from_slice(&[0x63, 4, 0]);
        append_data_event(&mut target_events, 0xE9, &target_record);
        let original = flp_fixture(&target_events, &[], &[0xCC]);
        let mut target = FlpDocument::parse(&original).expect("the target fixture should parse");

        assert!(
            target
                .paste_playlist_clip(4, &clipboard, 1_920, 498)
                .is_err()
        );
        assert_eq!(
            target.encode_lossless().expect("target should encode"),
            original,
            "a rejected paste must leave every project byte unchanged"
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
    fn mixer_insert_flags_decode_and_edit_known_bits_losslessly() {
        let mut event_stream = vec![0x9A];
        event_stream.extend_from_slice(&(-1_i32).to_le_bytes());
        event_stream.push(0x93);
        event_stream.extend_from_slice(&(-1_i32).to_le_bytes());
        event_stream.push(0x95);
        event_stream.extend_from_slice(&0_u32.to_le_bytes());
        let flags = 0x8000_0005_u32;
        let mut flags_payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
        flags_payload.extend_from_slice(&flags.to_le_bytes());
        flags_payload.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        append_data_event(&mut event_stream, 0xEC, &flags_payload);

        let original = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&original).expect("the fixture should parse");
        let insert = &document.mixer_inserts()[0];
        assert_eq!(insert.flags(), Some(flags));
        assert_eq!(insert.enabled(), Some(false));
        assert_eq!(insert.soloed(), Some(false));
        assert_eq!(insert.polarity_reversed(), Some(true));
        assert_eq!(insert.swap_left_right(), Some(false));
        assert_eq!(insert.effects_enabled(), Some(true));

        let flags_event_index = insert.flags_event_index.expect("flags event is recognized");
        let original_events = document
            .events()
            .iter()
            .map(|event| event.wire_bytes().to_vec())
            .collect::<Vec<_>>();
        document
            .edit_mixer_insert_flags(
                0,
                MixerInsertEdit {
                    enabled: Some(true),
                    soloed: Some(true),
                    polarity_reversed: Some(false),
                    swap_left_right: Some(true),
                    effects_enabled: None,
                },
            )
            .expect("known Mixer flags should be editable");

        let insert = &document.mixer_inserts()[0];
        assert_eq!(insert.flags(), Some(0x8000_100E));
        assert_eq!(insert.enabled(), Some(true));
        assert_eq!(insert.soloed(), Some(true));
        assert_eq!(insert.polarity_reversed(), Some(false));
        assert_eq!(insert.swap_left_right(), Some(true));
        assert_eq!(insert.effects_enabled(), Some(true));
        let edited_payload = document.events()[flags_event_index].payload();
        assert_eq!(&edited_payload[..4], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(&edited_payload[4..8], &0x8000_100E_u32.to_le_bytes());
        assert_eq!(&edited_payload[8..], &[0x11, 0x22, 0x33, 0x44]);
        for (index, event) in document.events().iter().enumerate() {
            if index != flags_event_index {
                assert_eq!(event.wire_bytes(), original_events[index]);
            }
        }

        let encoded = document
            .encode_lossless()
            .expect("edited flags should encode losslessly");
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should parse");
        assert_eq!(reparsed.mixer_inserts()[0].flags(), Some(0x8000_100E));

        let mut legacy_stream = vec![0x9A];
        legacy_stream.extend_from_slice(&(-1_i32).to_le_bytes());
        legacy_stream.push(0x93);
        legacy_stream.extend_from_slice(&(-1_i32).to_le_bytes());
        legacy_stream.push(0x95);
        legacy_stream.extend_from_slice(&0_u32.to_le_bytes());
        let mut legacy_flags = vec![0; 4];
        legacy_flags.extend_from_slice(&(1_u32 << 3).to_le_bytes());
        legacy_flags.extend_from_slice(&[0; 4]);
        append_data_event(&mut legacy_stream, 0xDC, &legacy_flags);
        let mut legacy = FlpDocument::parse(&flp_fixture(&legacy_stream, &[], &[]))
            .expect("the legacy fixture should parse");
        legacy.project_version = Some("24.2.0".to_owned());
        assert_eq!(legacy.mixer_inserts()[0].enabled(), Some(true));
    }

    #[test]
    fn mixer_route_audibility_applies_insert_mute_and_solo_conservatively() {
        let insert = |ordinal, flags| MixerInsertSummary {
            ordinal,
            input_raw: 0,
            output_raw: 0,
            color_raw: 0,
            icon_raw: None,
            name: None,
            flags: Some(flags),
            flags_event_index: None,
            first_event_index: 0,
            end_event_index: 0,
        };
        let routing = MixerRouteAudibility::from_inserts([
            insert(0, (1 << 3) | (1 << 12) | 1 | (1 << 1)),
            insert(1, (1 << 3) | (1 << 12) | 1 | (1 << 1)),
            insert(2, 0), // Disabled insert.
        ]);

        assert!(!routing.allows_channel(None));
        assert!(routing.allows_channel(Some(1)));
        assert!(!routing.allows_channel(Some(2)));
        assert!(routing.allows_channel(Some(-1)));
        assert!(routing.allows_channel(Some(7)));
        let mut routed_frame = [0.25, -0.5];
        routing
            .transform_for_channel(Some(1))
            .apply_frame(&mut routed_frame);
        assert_eq!(routed_frame, [0.5, -0.25]);
        let mut master_frame = [0.25, -0.5];
        routing
            .transform_for_channel(None)
            .apply_frame(&mut master_frame);
        assert_eq!(master_frame, [0.25, -0.5]);

        let master_solo = MixerRouteAudibility::from_inserts([
            insert(0, (1 << 3) | (1 << 12)),
            insert(1, 1 << 3),
        ]);
        assert!(master_solo.allows_channel(None));
        assert!(master_solo.allows_channel(Some(1)));
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
        payload.extend_from_slice(&[0x55, 0x66, 0x77, 0x88, 192, 0]);
        payload.extend_from_slice(&(64_u16 << 6).to_le_bytes());
        payload.extend_from_slice(&12_800_i32.to_le_bytes());
        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xE1, &payload);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("the fixed-size Mixer parameter event should parse");

        let records = document
            .mixer_parameter_records()
            .expect("12-byte records should decode");
        assert_eq!(records.len(), 3);
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
        assert_eq!(records[2].target_index(), 64);
        assert_eq!(records[2].candidate_insert_ordinal(), Some(0));

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

    fn marker_12_vst3_state_fixture() -> Vec<u8> {
        let mut nested = 1u32.to_le_bytes().to_vec();
        nested.extend_from_slice(&3u32.to_le_bytes());
        nested.extend_from_slice(&2u64.to_le_bytes());
        nested.extend_from_slice(&[0x10, 0x20]);
        nested.extend_from_slice(&2u32.to_le_bytes());
        nested.extend_from_slice(&1u64.to_le_bytes());
        nested.push(0x55);
        nested.extend_from_slice(&4u32.to_le_bytes());
        nested.extend_from_slice(&1u64.to_le_bytes());
        nested.push(0xA0);

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
        append_vst_field(&mut payload, 53, &nested);
        append_vst_field(&mut payload, 54, b"Test VST3");
        append_vst_field(&mut payload, 999, &[0x99, 0x98]);

        let mut event_stream = vec![0x40, 7, 0, 0x15, 2];
        append_data_event(&mut event_stream, 0xD4, &[0xD4, 0xA5]);
        append_data_event(&mut event_stream, 0xD5, &payload);
        flp_fixture(&event_stream, &[], &[0xD1, 0xD2])
    }

    #[test]
    fn replaces_vst3_channel_state_and_preserves_all_other_bytes() {
        let input = marker_12_vst3_state_fixture();
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_state = document
            .channel_plugin_states()
            .into_iter()
            .next()
            .expect("fixture should have VST state");
        let original_payload = original_state.data_payload().to_vec();
        let original_wrapper = original_state.wrapper_payload().map(<[u8]>::to_vec);
        let original_events = document.events().len();
        let mut replacement = 1u32.to_le_bytes().to_vec();
        replacement.extend_from_slice(&3u32.to_le_bytes());
        replacement.extend_from_slice(&3u64.to_le_bytes());
        replacement.extend_from_slice(&[0x21, 0x22, 0x23]);
        replacement.extend_from_slice(&2u32.to_le_bytes());
        replacement.extend_from_slice(&1u64.to_le_bytes());
        replacement.push(0x55);
        replacement.extend_from_slice(&4u32.to_le_bytes());
        replacement.extend_from_slice(&2u64.to_le_bytes());
        replacement.extend_from_slice(&[0xA1, 0xA2]);

        document
            .replace_vst3_channel_state_bytes(7, &replacement)
            .expect("marker-12 VST3 state should be replaceable");
        let state = document
            .channel_plugin_states()
            .into_iter()
            .next()
            .expect("updated project should retain VST state");
        assert_eq!(state.vst_state_bytes(), Some(replacement.as_slice()));
        assert_eq!(state.wrapper_payload(), original_wrapper.as_deref());
        assert_eq!(document.events().len(), original_events);

        let updated_payload = state.data_payload();
        let original_range = original_state
            .vst_metadata()
            .unwrap()
            .state_data_range
            .clone()
            .unwrap();
        let updated_metadata = state
            .vst_metadata()
            .expect("updated envelope should remain recognizable");
        assert_eq!(updated_metadata.name(), Some("Test VST3"));
        assert_eq!(
            updated_metadata.class_uid(),
            original_state.vst_metadata().unwrap().class_uid()
        );
        let updated_range = updated_metadata.state_data_range.as_ref().unwrap();
        let original_field_start = original_range.start - 12;
        let updated_field_start = updated_range.start - 12;
        assert_eq!(
            &updated_payload[..updated_field_start],
            &original_payload[..original_field_start]
        );
        assert_eq!(
            &updated_payload[updated_range.end..],
            &original_payload[original_range.end..]
        );
        let original_unknown_field = original_payload
            .windows(4)
            .position(|bytes| bytes == 999u32.to_le_bytes())
            .expect("fixture should have an unknown field");
        let updated_unknown_field = updated_payload
            .windows(4)
            .position(|bytes| bytes == 999u32.to_le_bytes())
            .expect("unknown field should be retained");
        assert_eq!(
            updated_payload[updated_unknown_field..],
            original_payload[original_unknown_field..]
        );

        let encoded = document
            .encode_lossless()
            .expect("updated project should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("updated project should reparse");
        assert_eq!(
            reparsed.channel_plugin_states()[0].vst_state_bytes(),
            Some(replacement.as_slice())
        );
        assert_eq!(reparsed.trailing_bytes(), &[0xD1, 0xD2]);
    }

    #[test]
    fn vst3_channel_state_replacement_rejects_ambiguous_fields_without_mutation() {
        let mut input = marker_12_vst3_state_fixture();
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let before = document.encode_lossless().expect("document should encode");
        assert!(document.replace_vst3_channel_state_bytes(99, &[1]).is_err());
        assert_eq!(document.encode_lossless().unwrap(), before);

        let mut payload = document.channel_plugin_states()[0].data_payload().to_vec();
        append_vst_field(&mut payload, 53, &[0x66]);
        let mut duplicate_stream = vec![0x40, 7, 0, 0x15, 2];
        append_data_event(&mut duplicate_stream, 0xD5, &payload);
        input = flp_fixture(&duplicate_stream, &[], &[]);
        document = FlpDocument::parse(&input).expect("duplicate-field fixture should parse");
        let before = document.encode_lossless().expect("document should encode");
        assert!(document.replace_vst3_channel_state_bytes(7, &[1]).is_err());
        assert_eq!(document.encode_lossless().unwrap(), before);
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
    fn decodes_sampler_reverse_flag_and_preserves_other_flag_bits() {
        for (kind, fx_flags, expected) in [
            (0, 0xA507_u16, true),
            (0, 0xA504, false),
            (4, 0xA507, false),
        ] {
            let mut event_stream = vec![0x40, 7, 0, 0x15, kind, 0x46];
            event_stream.extend_from_slice(&fx_flags.to_le_bytes());
            event_stream.extend_from_slice(&[0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &[]);
            let document = FlpDocument::parse(&input).expect("fixture should parse");

            assert_eq!(document.channels()[0].sample_reversed(), expected);
            assert_eq!(document.encode_lossless().unwrap(), input);
        }
    }

    #[test]
    fn decodes_sampler_loop_point_and_ping_pong_flags_losslessly() {
        for (kind, flags, ping_pong, expected_loop_points, expected_ping_pong) in [
            (0, 0x0000_0008_u32, true, true, true),
            (0, 0x0000_0000, false, false, false),
            (4, 0x0000_0008, true, false, false),
        ] {
            let mut event_stream = vec![0x40, 7, 0, 0x15, kind, 0x14, u8::from(ping_pong), 0x8F];
            event_stream.extend_from_slice(&flags.to_le_bytes());
            event_stream.extend_from_slice(&[0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &[]);
            let document = FlpDocument::parse(&input).expect("fixture should parse");

            assert_eq!(
                document.channels()[0].sampler_uses_loop_points(),
                expected_loop_points
            );
            assert_eq!(
                document.channels()[0].sampler_ping_pong_loop_enabled(),
                expected_ping_pong
            );
            assert_eq!(document.encode_lossless().unwrap(), input);
        }
    }

    #[test]
    fn decodes_sampler_root_note_and_ignores_non_sampler_or_invalid_values() {
        for (kind, root_note, expected_root_key) in
            [(0, 72_u32, Some(72_u16)), (0, 128, None), (4, 72, None)]
        {
            let mut event_stream = vec![0x40, 7, 0, 0x15, kind, 0x87];
            event_stream.extend_from_slice(&root_note.to_le_bytes());
            event_stream.extend_from_slice(&[0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &[]);
            let document = FlpDocument::parse(&input).expect("fixture should parse");

            assert_eq!(document.channels()[0].sampler_root_key(), expected_root_key);
            assert_eq!(document.encode_lossless().unwrap(), input);
        }
    }

    #[test]
    fn decodes_channel_key_region_from_parameters_and_preserves_event_bytes() {
        for (low, high, expected) in [(36, 84, Some((36, 84))), (72, 60, None), (0, 128, None)] {
            let mut event_stream = vec![0x40, 7, 0, 0x15, 4];
            append_data_event(
                &mut event_stream,
                0xC7,
                &channel_key_region_parameters(low, high),
            );
            event_stream.extend_from_slice(&[0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &[]);
            let document = FlpDocument::parse(&input).expect("fixture should parse");

            assert_eq!(document.channels()[0].keyboard_key_region(), expected);
            assert_eq!(document.encode_lossless().unwrap(), input);
        }

        let mut short_event_stream = vec![0x40, 7, 0, 0x15, 0];
        append_data_event(&mut short_event_stream, 0xC7, &[0; 75]);
        short_event_stream.extend_from_slice(&[0x62, 0, 0]);
        let short_document = FlpDocument::parse(&flp_fixture(&short_event_stream, &[], &[]))
            .expect("short parameters fixture should parse");
        assert_eq!(short_document.channels()[0].keyboard_key_region(), None);
    }

    #[test]
    fn sets_channel_enabled_by_updating_or_inserting_its_event_losslessly() {
        let tail = [0xA5, 0x5A];
        for has_enabled_event in [false, true] {
            let mut event_stream = vec![0x40, 7, 0, 0x15, 0];
            if has_enabled_event {
                event_stream.extend_from_slice(&[0x00, 1]);
            }
            event_stream.extend_from_slice(&[0x48, 0x34, 0x12, 0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &tail);
            let mut document = FlpDocument::parse(&input).expect("fixture should parse");
            let mut expected_events = document
                .events()
                .iter()
                .map(|event| event.wire_bytes.clone())
                .collect::<Vec<_>>();

            if has_enabled_event {
                let enabled_index = document
                    .events()
                    .iter()
                    .position(|event| event.opcode() == 0x00)
                    .expect("fixture should have an enabled event");
                expected_events[enabled_index] = vec![0x00, 0];
            } else {
                let kind_index = document
                    .events()
                    .iter()
                    .position(|event| event.opcode() == 0x15)
                    .expect("fixture should have a channel kind event");
                expected_events.insert(kind_index + 1, vec![0x00, 0]);
            }

            document
                .set_channel_enabled(7, false)
                .expect("channel should be muted");

            assert_eq!(document.channels()[0].enabled(), Some(false));
            assert_eq!(document.channels()[0].volume(), Some(0x1234));
            assert_eq!(
                document
                    .events()
                    .iter()
                    .map(|event| event.wire_bytes.clone())
                    .collect::<Vec<_>>(),
                expected_events
            );
            let encoded = document.encode_lossless().expect("document should encode");
            assert!(encoded.ends_with(&tail));
            let round_trip = FlpDocument::parse(&encoded).expect("muted project should parse");
            assert_eq!(round_trip.channels()[0].enabled(), Some(false));
        }
    }

    #[test]
    fn reads_and_sets_channel_swing_mix_losslessly() {
        let tail = [0xDE, 0xAD];
        for (kind, initial_mix, requested_mix, expected_mix, inserted) in [
            (0, None, 32u16, Some(32u16), true),
            (2, Some(40u16), 77, Some(77u16), false),
            (0, None, 128, None, false),
        ] {
            let mut event_stream = vec![0x40, 7, 0, 0x15, kind];
            if let Some(initial_mix) = initial_mix {
                event_stream.extend_from_slice(&[0x61]);
                event_stream.extend_from_slice(&initial_mix.to_le_bytes());
            }
            event_stream.extend_from_slice(&[0x48, 0x34, 0x12, 0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &tail);
            let mut document = FlpDocument::parse(&input).expect("fixture should parse");
            assert_eq!(document.channels()[0].swing_mix_raw(), initial_mix);
            assert_eq!(
                document.channels()[0].swing_mix(),
                initial_mix.unwrap_or(128)
            );
            assert_eq!(document.encode_lossless().unwrap(), input);

            document
                .set_channel_swing_mix(7, requested_mix)
                .expect("channel swing mix should update");

            let channel = &document.channels()[0];
            assert_eq!(channel.swing_mix_raw(), expected_mix);
            assert_eq!(channel.swing_mix(), expected_mix.unwrap_or(128));
            let swing_events = document
                .events()
                .iter()
                .filter(|event| event.opcode() == 0x61)
                .collect::<Vec<_>>();
            if let Some(expected_mix) = expected_mix {
                assert_eq!(swing_events.len(), 1);
                assert_eq!(swing_events[0].payload(), &expected_mix.to_le_bytes());
            } else {
                assert!(swing_events.is_empty());
            }
            if inserted {
                let opcodes = document
                    .events()
                    .iter()
                    .map(FlpEvent::opcode)
                    .collect::<Vec<_>>();
                assert_eq!(opcodes[1..3], [0x15, 0x61]);
            }

            let encoded = document.encode_lossless().expect("document should encode");
            assert!(encoded.ends_with(&tail));
            let round_trip = FlpDocument::parse(&encoded).expect("edited project should parse");
            assert_eq!(round_trip.channels()[0].swing_mix_raw(), expected_mix);
        }
    }

    #[test]
    fn rejects_unsupported_or_ambiguous_channel_swing_mix_edits() {
        let mut out_of_range =
            FlpDocument::parse(&flp_fixture(&[0x40, 7, 0, 0x15, 0, 0x62, 0, 0], &[], &[]))
                .expect("fixture should parse");
        assert!(out_of_range.set_channel_swing_mix(7, 129).is_err());

        let mut layer =
            FlpDocument::parse(&flp_fixture(&[0x40, 7, 0, 0x15, 3, 0x62, 0, 0], &[], &[]))
                .expect("fixture should parse");
        assert!(layer.set_channel_swing_mix(7, 64).is_err());

        let mut audio =
            FlpDocument::parse(&flp_fixture(&[0x40, 7, 0, 0x15, 4, 0x62, 0, 0], &[], &[]))
                .expect("fixture should parse");
        assert!(audio.set_channel_swing_mix(7, 64).is_err());

        let mut duplicate = FlpDocument::parse(&flp_fixture(
            &[0x40, 7, 0, 0x15, 0, 0x61, 32, 0, 0x61, 64, 0, 0x62, 0, 0],
            &[],
            &[],
        ))
        .expect("fixture should parse");
        assert!(duplicate.set_channel_swing_mix(7, 96).is_err());
    }

    #[test]
    fn reads_and_edits_channel_zipped_state_losslessly() {
        let tail = [0xDE, 0xAD];
        for (stored_zip, requested_zip, initial_zip) in [
            (None, false, false),
            (None, true, false),
            (Some(0), true, false),
            (Some(1), false, true),
            (Some(2), true, true),
        ] {
            let mut event_stream = vec![0x40, 7, 0, 0x15, 0];
            if let Some(stored_zip) = stored_zip {
                event_stream.extend_from_slice(&[0x0F, stored_zip]);
            }
            event_stream.extend_from_slice(&[0x48, 0x34, 0x12, 0x62, 0, 0]);
            let input = flp_fixture(&event_stream, &[], &tail);
            let mut document = FlpDocument::parse(&input).expect("fixture should parse");
            assert_eq!(document.channels()[0].zipped(), initial_zip);

            let mut expected_events = document
                .events()
                .iter()
                .map(|event| event.wire_bytes.clone())
                .collect::<Vec<_>>();
            if let Some(stored_zip) = stored_zip {
                let zip_index = document
                    .events()
                    .iter()
                    .position(|event| event.opcode() == 0x0F)
                    .expect("fixture should have a zipped-state event");
                if (stored_zip != 0) != requested_zip {
                    expected_events[zip_index] = vec![0x0F, u8::from(requested_zip)];
                }
            } else if requested_zip {
                let kind_index = document
                    .events()
                    .iter()
                    .position(|event| event.opcode() == 0x15)
                    .expect("fixture should have a channel kind event");
                expected_events.insert(kind_index + 1, vec![0x0F, 1]);
            }

            document
                .set_channel_zipped(7, requested_zip)
                .expect("channel compact state should update");

            assert_eq!(document.channels()[0].zipped(), requested_zip);
            assert_eq!(document.channels()[0].volume(), Some(0x1234));
            assert_eq!(
                document
                    .events()
                    .iter()
                    .map(|event| event.wire_bytes.clone())
                    .collect::<Vec<_>>(),
                expected_events
            );
            let encoded = document.encode_lossless().expect("document should encode");
            assert!(encoded.ends_with(&tail));
            let round_trip = FlpDocument::parse(&encoded).expect("edited project should parse");
            assert_eq!(round_trip.channels()[0].zipped(), requested_zip);
        }
    }

    #[test]
    fn reads_and_sets_channel_rgb_while_preserving_the_fourth_color_byte() {
        let mut event_stream = vec![0x40, 7, 0, 0x15, 0, 0x80, 0x11, 0x22, 0x33, 0xA5];
        event_stream.extend_from_slice(&[0x48, 0x34, 0x12, 0x62, 0, 0]);
        let tail = [0xDE, 0xAD];
        let input = flp_fixture(&event_stream, &[], &tail);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        assert_eq!(
            document.channels()[0].color(),
            Some([0x11, 0x22, 0x33, 0xA5])
        );

        document
            .set_channel_color(7, [0xAA, 0xBB, 0xCC])
            .expect("existing channel color should update");
        assert_eq!(
            document.channels()[0].color(),
            Some([0xAA, 0xBB, 0xCC, 0xA5])
        );
        assert!(
            document
                .events()
                .iter()
                .any(|event| event.opcode() == 0x80 && event.payload() == [0xAA, 0xBB, 0xCC, 0xA5])
        );

        let encoded = document.encode_lossless().expect("document should encode");
        assert!(encoded.ends_with(&tail));
        let round_trip = FlpDocument::parse(&encoded).expect("updated project should parse");
        assert_eq!(
            round_trip.channels()[0].color(),
            Some([0xAA, 0xBB, 0xCC, 0xA5])
        );

        let missing_color_stream = vec![0x40, 8, 0, 0x15, 0, 0x48, 0x34, 0x12, 0x62, 0, 0];
        let input = flp_fixture(&missing_color_stream, &[], &[]);
        let mut missing_color = FlpDocument::parse(&input).expect("fixture should parse");
        missing_color
            .set_channel_color(8, [0x10, 0x20, 0x30])
            .expect("missing color should be inserted");
        assert_eq!(
            missing_color.channels()[0].color(),
            Some([0x10, 0x20, 0x30, 0])
        );
        let event_bytes = missing_color
            .events()
            .iter()
            .map(|event| event.wire_bytes.clone())
            .collect::<Vec<_>>();
        assert_eq!(event_bytes[2], [0x80, 0x10, 0x20, 0x30, 0]);
        assert_eq!(event_bytes[3], [0x48, 0x34, 0x12]);
    }

    #[test]
    fn sorts_channels_by_hue_then_achromatic_then_uncolored() {
        let mut event_stream = Vec::new();
        for (id, color) in [
            (7, Some([0, 0, 255, 0])),
            (8, Some([255, 0, 0, 0])),
            (9, Some([255, 0, 255, 0])),
            (10, Some([0, 255, 0, 0])),
            (11, Some([70, 70, 70, 0])),
            (12, None),
        ] {
            event_stream.extend_from_slice(&[0x40, id, 0, 0x15, 0]);
            if let Some([red, green, blue, alpha]) = color {
                event_stream.extend_from_slice(&[0x80, red, green, blue, alpha]);
            }
        }
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        document
            .sort_channels(ChannelSortOrder::Color)
            .expect("color sort should succeed");

        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::id)
                .collect::<Vec<_>>(),
            [8, 10, 7, 9, 11, 12]
        );
        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::color)
                .collect::<Vec<_>>(),
            [
                Some([255, 0, 0, 0]),
                Some([0, 255, 0, 0]),
                Some([0, 0, 255, 0]),
                Some([255, 0, 255, 0]),
                Some([70, 70, 70, 0]),
                None,
            ]
        );
    }

    #[test]
    fn sorts_channels_by_mixer_track_with_generators_and_layers_first() {
        let mut event_stream = Vec::new();
        for (id, kind, track) in [
            (10, 0, Some(3)),
            (11, 2, Some(2)),
            (12, 3, None),
            (13, 5, Some(1)),
            (14, 0, Some(0)),
            (15, 0, None),
        ] {
            event_stream.extend_from_slice(&[0x40, id, 0, 0x15, kind]);
            if let Some(track) = track {
                event_stream.extend_from_slice(&[0x16, track]);
            }
        }
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_blocks = document
            .channels()
            .iter()
            .map(|channel| {
                channel
                    .event_range()
                    .map(|index| document.events()[index].wire_bytes.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::mixer_track)
                .collect::<Vec<_>>(),
            [Some(3), Some(2), None, Some(1), Some(0), None]
        );

        document
            .sort_channels(ChannelSortOrder::MixerTrack)
            .expect("Mixer-track sort should succeed");

        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::id)
                .collect::<Vec<_>>(),
            [11, 12, 14, 13, 10, 15]
        );
        let mut expected_events = Vec::new();
        for index in [1, 2, 4, 3, 0, 5] {
            expected_events.extend(original_blocks[index].clone());
        }
        expected_events.extend([vec![0x62, 0, 0]]);
        assert_eq!(
            document
                .events()
                .iter()
                .map(|event| event.wire_bytes.clone())
                .collect::<Vec<_>>(),
            expected_events
        );
    }

    #[test]
    fn reads_channel_display_groups_and_signed_group_assignments() {
        let mut event_stream = Vec::new();
        append_project_info_string(&mut event_stream, 0xE7, "Drums");
        append_project_info_string(&mut event_stream, 0xE7, "Synths");
        event_stream.extend_from_slice(&[
            0x40, 7, 0, 0x15, 0, 0x91, 0, 0, 0, 0, 0x40, 8, 0, 0x15, 0, 0x91, 1, 0, 0, 0, 0x40, 9,
            0, 0x15, 0, 0x91, 0xFF, 0xFF, 0xFF, 0xFF, 0x62, 0, 0,
        ]);
        append_project_info_string(&mut event_stream, 0xE7, "Not a display group");
        let input = flp_fixture(&event_stream, &[], &[]);
        let document = FlpDocument::parse(&input).expect("fixture should parse");

        let groups = document.channel_groups();
        assert_eq!(
            groups
                .iter()
                .map(ChannelGroupSummary::index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(
            groups
                .iter()
                .map(ChannelGroupSummary::name)
                .collect::<Vec<_>>(),
            [Some("Drums"), Some("Synths")]
        );
        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::group_number)
                .collect::<Vec<_>>(),
            [Some(0), Some(1), Some(-1)]
        );
    }

    #[test]
    fn assigns_channels_to_an_existing_group_and_preserves_other_events() {
        let mut event_stream = Vec::new();
        append_project_info_string(&mut event_stream, 0xE7, "Drums");
        append_project_info_string(&mut event_stream, 0xE7, "Synths");
        event_stream.extend_from_slice(&[
            0x40, 7, 0, 0x15, 0, 0x91, 0, 0, 0, 0, 0x90, 0x78, 0x56, 0x34, 0x12, 0x40, 8, 0, 0x15,
            0, 0x48, 0x34, 0x12, 0x62, 0, 0,
        ]);
        let input = flp_fixture(&event_stream, &[], &[0xA5, 0x5A]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        assert_eq!(
            document
                .group_channels(&[7, 8], "Synths")
                .expect("selected channels should be grouped"),
            1
        );
        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::group_number)
                .collect::<Vec<_>>(),
            [Some(1), Some(1)]
        );
        assert_eq!(
            document
                .events()
                .iter()
                .filter(|event| event.opcode() == 0x90)
                .map(FlpEvent::wire_bytes)
                .collect::<Vec<_>>(),
            [&[0x90, 0x78, 0x56, 0x34, 0x12][..]]
        );
        assert!(
            document.events().iter().any(|event| {
                event.opcode() == 0x48 && event.wire_bytes() == [0x48, 0x34, 0x12]
            })
        );
        let encoded = document.encode_lossless().expect("edited project encodes");
        assert_eq!(
            FlpDocument::parse(&encoded)
                .expect("edited project parses")
                .encode_lossless()
                .expect("roundtrip encodes"),
            encoded
        );
    }

    #[test]
    fn creates_new_channel_group_and_reuses_it_on_repeat() {
        let mut event_stream = Vec::new();
        append_project_info_string(&mut event_stream, 0xE7, "Drums");
        event_stream.extend_from_slice(&[
            0x40, 7, 0, 0x15, 0, 0x90, 0x78, 0x56, 0x34, 0x12, 0x40, 8, 0, 0x15, 0, 0x91, 0, 0, 0,
            0, 0x62, 0, 0,
        ]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        assert_eq!(
            document
                .group_channels(&[7], "Strings")
                .expect("a new group should be created"),
            1
        );
        let after_create = document
            .encode_lossless()
            .expect("grouped project should encode");
        assert_eq!(
            document
                .channel_groups()
                .iter()
                .map(ChannelGroupSummary::name)
                .collect::<Vec<_>>(),
            [Some("Drums"), Some("Strings")]
        );
        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::group_number)
                .collect::<Vec<_>>(),
            [Some(1), Some(0)]
        );
        assert_eq!(
            document
                .group_channels(&[7], "Strings")
                .expect("the created group should be reused"),
            1
        );
        assert_eq!(
            document
                .encode_lossless()
                .expect("reused group should encode"),
            after_create
        );
        assert_eq!(
            FlpDocument::parse(&after_create)
                .expect("grouped project should parse")
                .encode_lossless()
                .expect("grouped project should roundtrip"),
            after_create
        );
    }

    #[test]
    fn adds_and_renames_empty_channel_groups_without_changing_assignments() {
        let mut event_stream = Vec::new();
        append_project_info_string(&mut event_stream, 0xE7, "Drums");
        append_project_info_string(&mut event_stream, 0xE7, "Synths");
        event_stream.extend_from_slice(&[
            0x40, 7, 0, 0x15, 0, 0x91, 1, 0, 0, 0, 0x90, 0x78, 0x56, 0x34, 0x12,
        ]);
        let input = flp_fixture(&event_stream, &[], &[0xA5, 0x5A]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        assert_eq!(
            document
                .add_channel_group("Empty")
                .expect("an empty display group should be added"),
            2
        );
        document
            .rename_channel_group(1, "Keys")
            .expect("a display group should be renamed");
        let renamed = document.encode_lossless().expect("renamed project encodes");
        assert_eq!(
            document
                .channel_groups()
                .iter()
                .map(ChannelGroupSummary::name)
                .collect::<Vec<_>>(),
            [Some("Drums"), Some("Keys"), Some("Empty")]
        );
        assert_eq!(document.channels()[0].group_number(), Some(1));
        assert!(document.rename_channel_group(2, "Drums").is_err());
        assert_eq!(
            document
                .encode_lossless()
                .expect("rejected rename is atomic"),
            renamed
        );
        assert!(document.events().iter().any(|event| {
            event.opcode() == 0x90 && event.wire_bytes() == [0x90, 0x78, 0x56, 0x34, 0x12]
        }));
        assert_eq!(
            FlpDocument::parse(&renamed)
                .expect("renamed project parses")
                .encode_lossless()
                .expect("renamed project roundtrips"),
            renamed
        );
    }

    #[test]
    fn deleting_channel_group_unassigns_channels_and_reindexes_later_groups() {
        let mut event_stream = Vec::new();
        append_project_info_string(&mut event_stream, 0xE7, "Drums");
        append_project_info_string(&mut event_stream, 0xE7, "Synths");
        append_project_info_string(&mut event_stream, 0xE7, "Effects");
        event_stream.extend_from_slice(&[
            0x40, 7, 0, 0x15, 0, 0x91, 0, 0, 0, 0, 0x90, 0x11, 0x22, 0x33, 0x44, 0x40, 8, 0, 0x15,
            0, 0x91, 1, 0, 0, 0, 0x40, 9, 0, 0x15, 0, 0x91, 2, 0, 0, 0,
        ]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .delete_channel_group(1)
            .expect("a display group should be deleted");
        assert_eq!(
            document
                .channel_groups()
                .iter()
                .map(ChannelGroupSummary::name)
                .collect::<Vec<_>>(),
            [Some("Drums"), Some("Effects")]
        );
        assert_eq!(
            document
                .channels()
                .iter()
                .map(ChannelSummary::group_number)
                .collect::<Vec<_>>(),
            [Some(0), None, Some(1)]
        );
        assert!(document.events().iter().any(|event| {
            event.opcode() == 0x90 && event.wire_bytes() == [0x90, 0x11, 0x22, 0x33, 0x44]
        }));
        let encoded = document.encode_lossless().expect("deleted project encodes");
        assert_eq!(
            FlpDocument::parse(&encoded)
                .expect("deleted project parses")
                .encode_lossless()
                .expect("deleted project roundtrips"),
            encoded
        );
    }

    #[test]
    fn moves_channel_event_blocks_without_changing_channel_data() {
        let mut event_stream = vec![0x40, 7, 0, 0x15, 0, 0x48, 0x11, 0];
        append_project_info_string(&mut event_stream, 0xCB, "Kick");
        event_stream.extend_from_slice(&[0x40, 8, 0, 0x15, 0, 0x48, 0x22, 0]);
        append_project_info_string(&mut event_stream, 0xCB, "Snare");
        event_stream.extend_from_slice(&[0x40, 9, 0, 0x15, 0, 0x48, 0x33, 0]);
        append_project_info_string(&mut event_stream, 0xCB, "Hat");
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let tail = [0xA5, 0x5A];
        let input = flp_fixture(&event_stream, &[], &tail);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_blocks = document
            .channels()
            .iter()
            .map(|channel| {
                channel
                    .event_range()
                    .map(|index| document.events()[index].wire_bytes.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut expected_events = original_blocks[0].clone();
        expected_events.extend(original_blocks[2].clone());
        expected_events.extend(original_blocks[1].clone());
        expected_events.extend_from_slice(&[vec![0x62, 0, 0]]);

        document
            .move_channel(8, 2)
            .expect("channel should move down one slot");

        let channels = document.channels();
        assert_eq!(
            channels
                .iter()
                .map(|channel| channel.id())
                .collect::<Vec<_>>(),
            [7, 9, 8]
        );
        assert_eq!(
            channels
                .iter()
                .map(|channel| channel.display_name())
                .collect::<Vec<_>>(),
            [Some("Kick"), Some("Hat"), Some("Snare")]
        );
        assert_eq!(
            channels
                .iter()
                .map(|channel| channel.volume())
                .collect::<Vec<_>>(),
            [Some(0x11), Some(0x33), Some(0x22)]
        );
        let actual_events = document
            .events()
            .iter()
            .map(|event| event.wire_bytes.clone())
            .collect::<Vec<_>>();
        assert_eq!(actual_events, expected_events);
        let encoded = document.encode_lossless().expect("document should encode");
        assert!(encoded.ends_with(&tail));
        let round_trip = FlpDocument::parse(&encoded).expect("reordered project should parse");
        assert_eq!(
            round_trip
                .channels()
                .iter()
                .map(|channel| channel.id())
                .collect::<Vec<_>>(),
            [7, 9, 8]
        );

        document
            .move_channel(9, 0)
            .expect("channel should move to the first position");
        assert_eq!(
            document
                .channels()
                .iter()
                .map(|channel| channel.id())
                .collect::<Vec<_>>(),
            [9, 7, 8]
        );
        let mut expected_up_events = original_blocks[2].clone();
        expected_up_events.extend(original_blocks[0].clone());
        expected_up_events.extend(original_blocks[1].clone());
        expected_up_events.push(vec![0x62, 0, 0]);
        assert_eq!(
            document
                .events()
                .iter()
                .map(|event| event.wire_bytes.clone())
                .collect::<Vec<_>>(),
            expected_up_events
        );
    }

    #[test]
    fn sorts_channel_event_blocks_by_name_and_type_without_changing_data() {
        let mut event_stream = vec![0x17, 0xA1];
        for (id, kind, name) in [
            (7, 4, "Zulu"),
            (8, 0, "Kick"),
            (9, 2, "Synth"),
            (10, 3, "Layer"),
            (11, 5, "Automation"),
        ] {
            event_stream.extend_from_slice(&[0x40, id, 0, 0x15, kind, 0x48, id, 0]);
            append_project_info_string(&mut event_stream, 0xCB, name);
        }
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let tail = [0xA5, 0x5A];
        let input = flp_fixture(&event_stream, &[], &tail);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_blocks = document
            .channels()
            .iter()
            .map(|channel| {
                channel
                    .event_range()
                    .map(|index| document.events()[index].wire_bytes.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let prefix = vec![vec![0x17, 0xA1]];
        let terminator = vec![vec![0x62, 0, 0]];

        document
            .sort_channels(ChannelSortOrder::Type)
            .expect("type sort should succeed");

        let typed_channels = document.channels();
        assert_eq!(
            typed_channels
                .iter()
                .map(ChannelSummary::id)
                .collect::<Vec<_>>(),
            [10, 9, 8, 7, 11]
        );
        let mut expected_type_events = prefix.clone();
        for index in [3, 2, 1, 0, 4] {
            expected_type_events.extend(original_blocks[index].clone());
        }
        expected_type_events.extend(terminator.clone());
        assert_eq!(
            document
                .events()
                .iter()
                .map(|event| event.wire_bytes.clone())
                .collect::<Vec<_>>(),
            expected_type_events
        );

        document
            .sort_channels(ChannelSortOrder::Name)
            .expect("name sort should succeed");

        let named_channels = document.channels();
        assert_eq!(
            named_channels
                .iter()
                .map(ChannelSummary::id)
                .collect::<Vec<_>>(),
            [11, 8, 10, 9, 7]
        );
        assert_eq!(
            named_channels
                .iter()
                .map(ChannelSummary::display_name)
                .collect::<Vec<_>>(),
            [
                Some("Automation"),
                Some("Kick"),
                Some("Layer"),
                Some("Synth"),
                Some("Zulu")
            ]
        );
        let mut expected_name_events = prefix;
        for index in [4, 1, 3, 2, 0] {
            expected_name_events.extend(original_blocks[index].clone());
        }
        expected_name_events.extend(terminator);
        assert_eq!(
            document
                .events()
                .iter()
                .map(|event| event.wire_bytes.clone())
                .collect::<Vec<_>>(),
            expected_name_events
        );
        let encoded = document.encode_lossless().expect("document should encode");
        assert!(encoded.ends_with(&tail));
    }

    #[test]
    fn creates_sampler_channel_before_channel_terminator() {
        let mut event_stream = vec![0x40, 7, 0, 0x15, 0];
        append_data_event(
            &mut event_stream,
            0xC4,
            &utf16_project_string("/samples/kick.wav"),
        );
        append_project_info_string(&mut event_stream, 0xCB, "Kick");
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let mut input = flp_fixture(&event_stream, &[], &[0xA5, 0x5A]);
        input[10..12].copy_from_slice(&1u16.to_le_bytes());

        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let channel_id = document
            .create_sampler_channel("/samples/snare.wav", "Snare")
            .expect("Sampler channel should be created");

        assert_eq!(channel_id, 8);
        assert_eq!(document.header().legacy_channel_count(), 2);
        let channels = document.channels();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].display_name(), Some("Kick"));
        assert_eq!(channels[1].id(), channel_id);
        assert_eq!(channels[1].kind(), Some(0));
        assert_eq!(channels[1].enabled(), Some(true));
        assert_eq!(channels[1].display_name(), Some("Snare"));
        assert_eq!(channels[1].sample_path(), Some("/samples/snare.wav"));
        let encoded = document.encode_lossless().expect("document should encode");
        assert!(encoded.ends_with(&[0xA5, 0x5A]));
        let round_trip = FlpDocument::parse(&encoded).expect("created channel should parse");
        assert_eq!(round_trip.channels().len(), 2);
        assert_eq!(
            round_trip.channels()[1].sample_path(),
            Some("/samples/snare.wav")
        );
    }

    #[test]
    fn creates_audio_clip_channel_before_channel_terminator() {
        let mut event_stream = vec![0x40, 7, 0, 0x15, 0];
        append_data_event(
            &mut event_stream,
            0xC4,
            &utf16_project_string("/samples/kick.wav"),
        );
        append_project_info_string(&mut event_stream, 0xCB, "Kick");
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let mut input = flp_fixture(&event_stream, &[], &[0xA5, 0x5A]);
        input[10..12].copy_from_slice(&1u16.to_le_bytes());

        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let channel_id = document
            .create_audio_channel("/samples/take.wav", "Take")
            .expect("Audio Clip channel should be created");

        assert_eq!(channel_id, 8);
        assert_eq!(document.header().legacy_channel_count(), 2);
        let channels = document.channels();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[1].id(), channel_id);
        assert_eq!(channels[1].kind(), Some(4));
        assert_eq!(channels[1].enabled(), Some(true));
        assert_eq!(channels[1].display_name(), Some("Take"));
        assert_eq!(channels[1].sample_path(), Some("/samples/take.wav"));

        let encoded = document.encode_lossless().expect("document should encode");
        assert!(encoded.ends_with(&[0xA5, 0x5A]));
        let round_trip = FlpDocument::parse(&encoded).expect("created channel should parse");
        assert_eq!(round_trip.channels()[1].kind(), Some(4));
        assert_eq!(
            round_trip.channels()[1].sample_path(),
            Some("/samples/take.wav")
        );
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
    fn pattern_controller_points_decode_edit_and_roundtrip_losslessly() {
        let first = pattern_controller_record(120, [0xA5, 0x5A], 3, 0x91, 0x7FC0_0001);
        let second = pattern_controller_record(480, [0xD3, 0x4C], 7, 0x2A, 0.25f32.to_bits());
        let mut controller_payload = first.to_vec();
        controller_payload.extend_from_slice(&second);
        let mut controller_event = Vec::new();
        append_data_event(&mut controller_event, 0xDF, &controller_payload);
        let input = pattern_fixture(&[], &controller_event);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let pattern = &document.patterns().expect("patterns should decode")[0];
        assert_eq!(pattern.controllers.len(), 2);
        assert_eq!(pattern.controllers[0].position, 120);
        assert_eq!(pattern.controllers[0].reserved, [0xA5, 0x5A]);
        assert_eq!(pattern.controllers[0].channel, 3);
        assert_eq!(pattern.controllers[0].flags, 0x91);
        assert_eq!(pattern.controllers[0].value_bits, 0x7FC0_0001);
        assert!(pattern.controllers[0].value().is_nan());
        assert_eq!(pattern.controllers[1].value(), 0.25);
        assert_eq!(document.encode_lossless().unwrap(), input);

        document
            .edit_pattern_controller(
                7,
                1,
                PatternControllerEdit {
                    position: Some(777),
                    value: Some(0.75),
                },
            )
            .expect("the selected controller point should be editable");
        let pattern = &document.patterns().unwrap()[0];
        assert_eq!(pattern.controllers[1].position, 777);
        assert_eq!(pattern.controllers[1].reserved, [0xD3, 0x4C]);
        assert_eq!(pattern.controllers[1].channel, 7);
        assert_eq!(pattern.controllers[1].flags, 0x2A);
        assert_eq!(pattern.controllers[1].value(), 0.75);
        assert_eq!(pattern.controllers[0].value_bits, 0x7FC0_0001);

        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited file should parse");
        assert_eq!(reparsed.patterns().unwrap(), document.patterns().unwrap());
        let edited_payload = reparsed
            .events()
            .iter()
            .find(|event| event.opcode() == 0xDF)
            .expect("controller event should remain")
            .payload();
        let mut expected_payload = first.to_vec();
        let mut expected_second = second;
        expected_second[..4].copy_from_slice(&777u32.to_le_bytes());
        expected_second[8..12].copy_from_slice(&0.75f32.to_bits().to_le_bytes());
        expected_payload.extend_from_slice(&expected_second);
        assert_eq!(edited_payload, expected_payload);
    }

    #[test]
    fn malformed_pattern_controller_payload_is_rejected() {
        let mut controller_event = Vec::new();
        append_data_event(&mut controller_event, 0xDF, &[0; 11]);
        let input = pattern_fixture(&[], &controller_event);
        let document = FlpDocument::parse(&input).expect("container fixture should parse");

        assert!(matches!(
            document.patterns(),
            Err(FlpError::InvalidEvent { .. })
        ));
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn pattern_controller_edit_rejects_non_finite_values_without_mutation() {
        let record = pattern_controller_record(0, [0; 2], 0, 0, 0.5f32.to_bits());
        let mut controller_event = Vec::new();
        append_data_event(&mut controller_event, 0xDF, &record);
        let input = pattern_fixture(&[], &controller_event);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        assert!(
            document
                .edit_pattern_controller(
                    7,
                    0,
                    PatternControllerEdit {
                        value: Some(f32::INFINITY),
                        ..PatternControllerEdit::default()
                    },
                )
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn duplicate_pattern_controller_copies_unknown_fields_and_roundtrips() {
        let first = pattern_controller_record(120, [0xA5, 0x5A], 3, 0x91, 0.25f32.to_bits());
        let template = pattern_controller_record(480, [0xD3, 0x4C], 7, 0x2A, 0.5f32.to_bits());
        let unknown_event = [0xFE, 0x02, 0xAA, 0xBB];
        let mut controller_payload = first.to_vec();
        controller_payload.extend_from_slice(&template);
        let mut controller_event = Vec::new();
        append_data_event(&mut controller_event, 0xDF, &controller_payload);
        controller_event.extend_from_slice(&unknown_event);
        let input = pattern_fixture(&[], &controller_event);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let new_index = document
            .duplicate_pattern_controller(7, 1, 777, 0.75)
            .expect("an existing point should provide an exact record template");
        assert_eq!(new_index, 2);
        let controllers = &document.patterns().unwrap()[0].controllers;
        assert_eq!(controllers.len(), 3);
        assert_eq!(controllers[0].position, 120);
        assert_eq!(controllers[0].reserved, [0xA5, 0x5A]);
        assert_eq!(controllers[1].value(), 0.5);
        assert_eq!(controllers[2].position, 777);
        assert_eq!(controllers[2].reserved, [0xD3, 0x4C]);
        assert_eq!(controllers[2].channel, 7);
        assert_eq!(controllers[2].flags, 0x2A);
        assert_eq!(controllers[2].value(), 0.75);

        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("duplicated file should parse");
        assert_eq!(reparsed.patterns().unwrap(), document.patterns().unwrap());
        assert_eq!(
            reparsed
                .events()
                .iter()
                .find(|event| event.opcode() == 0xFE)
                .expect("unknown event should remain")
                .wire_bytes(),
            &unknown_event
        );
    }

    #[test]
    fn delete_pattern_controller_removes_only_the_requested_records() {
        let first = pattern_controller_record(120, [0xA5, 0x5A], 3, 0x91, 0.25f32.to_bits());
        let second = pattern_controller_record(480, [0xD3, 0x4C], 7, 0x2A, 0.5f32.to_bits());
        let unknown_event = [0xFE, 0x02, 0xAA, 0xBB];
        let mut controller_payload = first.to_vec();
        controller_payload.extend_from_slice(&second);
        let mut controller_event = Vec::new();
        append_data_event(&mut controller_event, 0xDF, &controller_payload);
        controller_event.extend_from_slice(&unknown_event);
        let input = pattern_fixture(&[], &controller_event);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .delete_pattern_controller(7, 1)
            .expect("the selected point should be deleted");
        assert_eq!(document.patterns().unwrap()[0].controllers.len(), 1);
        assert_eq!(
            document
                .events()
                .iter()
                .find(|event| event.opcode() == 0xDF)
                .expect("the remaining point should keep the event")
                .payload(),
            &first
        );

        document
            .delete_pattern_controller(7, 0)
            .expect("the final point should be deleted");
        assert!(document.events().iter().all(|event| event.opcode() != 0xDF));
        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("deleted file should parse");
        assert!(reparsed.patterns().unwrap()[0].controllers.is_empty());
        assert_eq!(
            reparsed
                .events()
                .iter()
                .find(|event| event.opcode() == 0xFE)
                .expect("unknown event should remain")
                .wire_bytes(),
            &unknown_event
        );
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
    fn pattern_time_signatures_are_scoped_editable_and_roundtrip_losslessly() {
        let pattern_position = 120;
        let pattern_raw_position =
            super::TIME_MARKER_SIGNATURE_BIT | 0x1000_0000 | pattern_position;
        let playlist_position = 480;
        let mut event_stream = vec![
            0x11, 4, 0x12, 4, 0x40, 0, 0, 0x41, 7, 0, 0xD0, 0, 0x40, 0, 0, 0x41, 7, 0,
        ];
        append_time_marker(
            &mut event_stream,
            pattern_raw_position,
            3,
            8,
            "Pattern meter",
        );
        event_stream.extend_from_slice(&[0xFF, 2, 0xAA, 0xBB, 0x40, 0, 0]);
        event_stream.extend_from_slice(&[0x63, 1, 0]);
        append_time_marker(
            &mut event_stream,
            super::TIME_MARKER_SIGNATURE_BIT | playlist_position,
            5,
            4,
            "Playlist meter",
        );
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        let input = flp_fixture(&event_stream, &[], &[]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let pattern = document
            .patterns()
            .unwrap()
            .into_iter()
            .find(|pattern| pattern.id == 7)
            .expect("pattern 7 should decode");
        assert_eq!(pattern.time_markers.len(), 1);
        assert_eq!(pattern.time_markers[0].raw_position(), pattern_raw_position);
        assert!(pattern.time_markers[0].is_signature());
        assert_eq!(pattern.time_markers[0].position_ticks(), pattern_position);
        assert_eq!(pattern.time_markers[0].numerator(), Some(3));
        assert_eq!(pattern.time_markers[0].denominator(), Some(8));
        assert_eq!(pattern.time_markers[0].name(), Some("Pattern meter"));
        assert_eq!(document.metadata().time_signature(), Some((4, 4)));
        let playlist_markers = document.time_markers().unwrap();
        assert_eq!(playlist_markers.len(), 1);
        assert_eq!(playlist_markers[0].1.position_ticks(), playlist_position);
        assert_eq!(playlist_markers[0].1.numerator(), Some(5));
        let playlist_marker_summary = |document: &FlpDocument| {
            document
                .time_markers()
                .unwrap()
                .into_iter()
                .map(|(arrangement_id, marker)| {
                    (
                        arrangement_id,
                        marker.raw_position(),
                        marker.numerator(),
                        marker.denominator(),
                        marker.name().map(str::to_owned),
                    )
                })
                .collect::<Vec<_>>()
        };
        let original_playlist_marker_summary = playlist_marker_summary(&document);

        let edited_index = document
            .set_pattern_time_signature(7, pattern_position, 5, 16)
            .expect("the existing Pattern signature should be editable");
        assert_eq!(edited_index, 0);
        let edited_pattern = document.patterns().unwrap().remove(0);
        assert_eq!(
            edited_pattern.time_markers[0].raw_position(),
            pattern_raw_position
        );
        assert_eq!(edited_pattern.time_markers[0].numerator(), Some(5));
        assert_eq!(edited_pattern.time_markers[0].denominator(), Some(16));

        let created_index = document
            .set_pattern_time_signature(7, 384, 7, 8)
            .expect("a second Pattern signature should be created");
        assert_eq!(created_index, 1);
        let signatures = document.patterns().unwrap().remove(0).time_markers;
        assert_eq!(signatures.len(), 2);
        assert_eq!(signatures[1].position_ticks(), 384);
        assert_eq!(signatures[1].numerator(), Some(7));
        assert_eq!(signatures[1].denominator(), Some(8));

        document
            .delete_pattern_time_signature(7, 384)
            .expect("the created Pattern signature should be removable");
        document
            .delete_pattern_time_signature(7, pattern_position)
            .expect("the original Pattern signature should be removable");
        assert!(document.patterns().unwrap()[0].time_markers.is_empty());
        assert_eq!(document.metadata().time_signature(), Some((4, 4)));
        assert_eq!(
            playlist_marker_summary(&document),
            original_playlist_marker_summary
        );
        assert!(
            document
                .events()
                .iter()
                .any(|event| { event.opcode() == 0xFF && event.payload() == [0xAA, 0xBB] })
        );

        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should parse");
        assert!(reparsed.patterns().unwrap()[0].time_markers.is_empty());
        assert_eq!(
            playlist_marker_summary(&reparsed),
            original_playlist_marker_summary
        );
        assert_eq!(reparsed.metadata().time_signature(), Some((4, 4)));
    }

    #[test]
    fn slide_note_edits_toggle_only_the_slide_flag_and_roundtrip() {
        let mut original_note = note_record(0, 0, 48, 60, 100);
        let original_flags = 0xA5A5_u16;
        original_note[4..6].copy_from_slice(&original_flags.to_le_bytes());
        let mut document = FlpDocument::parse(&pattern_fixture(&[original_note], &[0xFF, 1, 0xAA]))
            .expect("fixture should parse");

        document
            .edit_pattern_note(
                7,
                0,
                0,
                PatternNoteEdit {
                    slide: Some(true),
                    ..PatternNoteEdit::default()
                },
            )
            .expect("slide flag should be set");
        let note = &document.patterns().unwrap()[0].notes[0];
        assert!(note.is_slide_note());
        assert_eq!(note.flags, original_flags | PATTERN_NOTE_SLIDE_FLAG);

        document
            .edit_pattern_note(
                7,
                0,
                0,
                PatternNoteEdit {
                    slide: Some(false),
                    ..PatternNoteEdit::default()
                },
            )
            .expect("slide flag should be cleared");
        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited project should parse");
        let note = &reparsed.patterns().unwrap()[0].notes[0];
        assert!(!note.is_slide_note());
        assert_eq!(note.flags, original_flags);
        assert_eq!(
            reparsed.events().last().unwrap().wire_bytes(),
            &[0xFF, 1, 0xAA]
        );
    }

    #[test]
    fn scale_levels_updates_only_selected_channel_velocities_and_roundtrips() {
        let mut first = note_record(0, 0, 48, 60, 20);
        first[4..6].copy_from_slice(&0xA5A5_u16.to_le_bytes());
        first[17] = 0xD3;
        let other_channel = note_record(24, 1, 52, 60, 64);
        let unselected = note_record(48, 0, 55, 60, 40);
        let mut clamped = note_record(72, 0, 60, 60, 100);
        clamped[20] = 0x91;
        let input = pattern_fixture(
            &[first, other_channel, unselected, clamped],
            &[0xFF, 1, 0xAA],
        );
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let changed = document
            .scale_pattern_note_selection_levels(7, 0, &[0, 2], 150, 10)
            .expect("selected note levels should scale");
        assert_eq!(changed, 2);

        let encoded = document
            .encode_lossless()
            .expect("edited FLP should encode");
        let reparsed = FlpDocument::parse(&encoded).expect("edited FLP should parse");
        let notes = reparsed.patterns().unwrap()[0].notes.clone();
        assert_eq!(notes[0].velocity, 43);
        assert_eq!(notes[0].flags, 0xA5A5);
        assert_eq!(notes[0].reserved, 0xD3);
        assert_eq!(notes[1].velocity, 64);
        assert_eq!(notes[2].velocity, 40);
        assert_eq!(notes[3].velocity, 127);
        assert_eq!(notes[3].pan, 0x91);
        assert_eq!(
            reparsed.events().last().unwrap().wire_bytes(),
            &[0xFF, 1, 0xAA]
        );
    }

    #[test]
    fn scale_levels_validates_limits_and_returns_zero_for_unchanged_notes() {
        let note = note_record(0, 0, 48, 60, 100);
        let mut document = FlpDocument::parse(&pattern_fixture(&[note], &[0xFF, 0]))
            .expect("fixture should parse");

        assert_eq!(document.scale_pattern_note_levels(7, 0, 100, 0).unwrap(), 0);
        assert!(document.scale_pattern_note_levels(7, 0, 201, 0).is_err());
        assert!(document.scale_pattern_note_levels(7, 0, 100, 101).is_err());
        assert!(
            document
                .scale_pattern_note_levels_with_options(
                    7,
                    0,
                    ScaleLevelsOptions {
                        center_percent: 101,
                        ..ScaleLevelsOptions::default()
                    },
                )
                .is_err()
        );
        assert!(
            document
                .scale_pattern_note_levels_with_options(
                    7,
                    0,
                    ScaleLevelsOptions {
                        tension_percent: -101,
                        ..ScaleLevelsOptions::default()
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn scale_levels_applies_center_pivot_and_logarithmic_tension() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 48, 60, 32),
                note_record(24, 0, 52, 60, 64),
                note_record(48, 0, 55, 60, 127),
            ],
            &[0xFF, 1, 0xAA],
        );
        let mut brighter = FlpDocument::parse(&input).expect("fixture should parse");
        brighter
            .scale_pattern_note_levels_with_options(
                7,
                0,
                ScaleLevelsOptions {
                    tension_percent: 100,
                    ..ScaleLevelsOptions::default()
                },
            )
            .expect("positive tension should scale notes");
        let brighter_notes = brighter.patterns().unwrap().remove(0).notes;
        assert!(brighter_notes[0].velocity > 32);
        assert!(brighter_notes[1].velocity > 64);
        assert_eq!(brighter_notes[2].velocity, 127);

        let mut softer = FlpDocument::parse(&input).expect("fixture should parse");
        softer
            .scale_pattern_note_levels_with_options(
                7,
                0,
                ScaleLevelsOptions {
                    tension_percent: -100,
                    ..ScaleLevelsOptions::default()
                },
            )
            .expect("negative tension should scale notes");
        let softer_notes = softer.patterns().unwrap().remove(0).notes;
        assert!(softer_notes[0].velocity < 32);
        assert!(softer_notes[1].velocity < 64);

        let mut centered = FlpDocument::parse(&pattern_fixture(
            &[note_record(0, 0, 48, 60, 64)],
            &[0xFF, 0],
        ))
        .expect("fixture should parse");
        centered
            .scale_pattern_note_levels_with_options(
                7,
                0,
                ScaleLevelsOptions {
                    center_percent: 50,
                    multiplier_percent: 200,
                    ..ScaleLevelsOptions::default()
                },
            )
            .expect("centered multiply should scale around its pivot");
        assert_eq!(centered.patterns().unwrap()[0].notes[0].velocity, 65);
    }

    #[test]
    fn riff_machine_builds_scale_arpeggios_from_a_note_progression() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 96, 60, 100),
                note_record(96, 0, 96, 62, 90),
                note_record(0, 1, 96, 48, 70),
            ],
            &[0xFF, 1, 0xA7],
        );
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let created = document
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should transform the progression");

        assert_eq!(created, 8);
        let notes = document.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        assert_eq!(
            channel_notes
                .iter()
                .map(|note| (note.position, note.key))
                .collect::<Vec<_>>(),
            [
                (0, 60),
                (24, 64),
                (48, 67),
                (72, 60),
                (96, 62),
                (120, 65),
                (144, 69),
                (168, 62),
            ]
        );
        assert_eq!(notes[0].channel_id, 1);
        assert_eq!(notes[0].key, 48);
        assert_eq!(
            document.events().last().unwrap().wire_bytes(),
            &[0xFF, 1, 0xA7]
        );

        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited file should parse");
        assert_eq!(reparsed.patterns().unwrap()[0].notes.len(), 9);
    }

    #[test]
    fn riff_machine_respects_selection_and_rolls_back_invalid_ranges() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 96, 60, 100),
                note_record(96, 0, 96, 67, 90),
                note_record(0, 1, 96, 48, 70),
            ],
            &[0xFF, 0],
        );
        let mut selected = FlpDocument::parse(&input).expect("fixture should parse");
        let created = selected
            .riff_machine_pattern_note_selection(
                7,
                0,
                &[1],
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should transform the selection");
        assert_eq!(created, 4);
        let notes = selected.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        assert_eq!(channel_notes.len(), 5);
        assert_eq!((channel_notes[0].position, channel_notes[0].key), (0, 60));
        assert_eq!((channel_notes[1].position, channel_notes[1].key), (96, 67));
        assert_eq!(
            notes.iter().find(|note| note.channel_id == 1).unwrap().key,
            48
        );

        let mut invalid = FlpDocument::parse(&input).expect("fixture should parse");
        let original = invalid.encode_lossless().unwrap();
        assert!(
            invalid
                .riff_machine_pattern_notes(
                    7,
                    0,
                    RiffMachineOptions {
                        minimum_key: 70,
                        maximum_key: 70,
                        ..RiffMachineOptions::default()
                    },
                )
                .is_err()
        );
        assert_eq!(invalid.encode_lossless().unwrap(), original);
    }

    #[test]
    fn riff_machine_horizontal_mirror_can_preserve_onsets() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 96, 60, 100),
                note_record(96, 0, 96, 62, 90),
            ],
            &[0xFF, 0],
        );
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        document
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    mirror_horizontal: true,
                    preserve_start_times: true,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should preserve onsets while reversing pitches");

        let notes = document.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .map(|note| (note.position, note.key))
            .collect::<Vec<_>>();
        assert_eq!(
            channel_notes,
            [
                (0, 62),
                (24, 69),
                (48, 65),
                (72, 62),
                (96, 60),
                (120, 67),
                (144, 64),
                (168, 60),
            ]
        );
    }

    #[test]
    fn riff_machine_mirror_reverses_time_or_pitch_without_touching_other_channels() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 96, 60, 100),
                note_record(96, 0, 96, 62, 90),
                note_record(0, 1, 96, 48, 70),
            ],
            &[0xFF, 0],
        );
        let mut time_mirrored = FlpDocument::parse(&input).expect("fixture should parse");
        time_mirrored
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    mirror_horizontal: true,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should reverse note times");
        let notes = time_mirrored.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .map(|note| (note.position, note.key))
            .collect::<Vec<_>>();
        assert_eq!(
            channel_notes,
            [
                (168, 60),
                (144, 64),
                (120, 67),
                (96, 60),
                (72, 62),
                (48, 65),
                (24, 69),
                (0, 62),
            ]
        );
        assert_eq!(notes[0].channel_id, 1);
        assert_eq!(notes[0].key, 48);

        let mut pitch_mirrored = FlpDocument::parse(&input).expect("fixture should parse");
        pitch_mirrored
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    mirror_vertical: true,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should invert note pitches");
        let notes = pitch_mirrored.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .map(|note| (note.position, note.key))
            .collect::<Vec<_>>();
        assert_eq!(
            channel_notes,
            [
                (0, 69),
                (24, 65),
                (48, 62),
                (72, 69),
                (96, 67),
                (120, 64),
                (144, 60),
                (168, 67),
            ]
        );
        assert_eq!(notes[0].channel_id, 1);
        assert_eq!(notes[0].key, 48);
    }

    #[test]
    fn riff_machine_groove_moves_starts_by_strength_and_sensitivity() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 96, 60, 100),
                note_record(96, 0, 96, 62, 90),
            ],
            &[0xFF, 0],
        );
        let mut softened = FlpDocument::parse(&input).expect("fixture should parse");
        softened
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    groove_snap_ticks: Some(36),
                    groove_start_percent: 50,
                    groove_sensitivity_percent: 100,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should mix starts toward the snap grid");
        let notes = softened.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        assert_eq!(channel_notes[0].position, 0);
        assert_eq!(channel_notes[1].position, 30);
        assert_eq!(channel_notes[2].position, 42);
        assert_eq!(channel_notes[1].length, 19);

        let mut sensitive = FlpDocument::parse(&input).expect("fixture should parse");
        sensitive
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    groove_snap_ticks: Some(36),
                    groove_start_percent: 100,
                    groove_sensitivity_percent: 50,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should leave distant starts unchanged");
        let notes = sensitive.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        assert_eq!(
            channel_notes
                .iter()
                .map(|note| note.position)
                .collect::<Vec<_>>(),
            [0, 24, 48, 72, 96, 120, 144, 168]
        );
    }

    #[test]
    fn riff_machine_groove_quantizes_duration_and_can_leave_note_ends_fixed() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 96, 60, 100),
                note_record(96, 0, 96, 62, 90),
            ],
            &[0xFF, 0],
        );
        let mut quantized = FlpDocument::parse(&input).expect("fixture should parse");
        quantized
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    groove_snap_ticks: Some(24),
                    groove_duration_percent: 100,
                    groove_sensitivity_percent: 100,
                    groove_quantize_mode: RiffMachineQuantizeMode::QuantizeDuration,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should quantize note durations");
        let notes = quantized.patterns().unwrap().remove(0).notes;
        assert!(
            notes
                .iter()
                .filter(|note| note.channel_id == 0)
                .all(|note| note.length == 24)
        );

        let mut fixed_end = FlpDocument::parse(&input).expect("fixture should parse");
        fixed_end
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    groove_snap_ticks: Some(36),
                    groove_start_percent: 100,
                    groove_sensitivity_percent: 100,
                    groove_quantize_mode: RiffMachineQuantizeMode::LeaveEnd,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should preserve note ends");
        let notes = fixed_end.patterns().unwrap().remove(0).notes;
        let channel_notes = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        assert_eq!(channel_notes[1].position + channel_notes[1].length, 43);
        assert_eq!(channel_notes[1].position, 36);
        assert_eq!(channel_notes[1].length, 7);
    }

    #[test]
    fn riff_machine_groove_distinguishes_quantized_duration_from_quantized_end() {
        let note = PatternNote {
            position: 30,
            length: 24,
            ..PatternNote::default()
        };
        let duration = riff_machine_groove_note_timing(
            &note,
            24,
            RiffMachineOptions {
                groove_start_percent: 25,
                groove_sensitivity_percent: 100,
                groove_duration_percent: 100,
                groove_quantize_mode: RiffMachineQuantizeMode::QuantizeDuration,
                ..RiffMachineOptions::default()
            },
        )
        .expect("duration quantization should succeed");
        let end = riff_machine_groove_note_timing(
            &note,
            24,
            RiffMachineOptions {
                groove_start_percent: 25,
                groove_sensitivity_percent: 100,
                groove_duration_percent: 100,
                groove_quantize_mode: RiffMachineQuantizeMode::QuantizeEnd,
                ..RiffMachineOptions::default()
            },
        )
        .expect("end-time quantization should succeed");
        assert_eq!(duration, (29, 24));
        assert_eq!(end, (29, 19));
    }

    #[test]
    fn riff_machine_levels_randomize_all_supported_note_properties_deterministically() {
        let mut first_root = note_record(0, 0, 96, 60, 80);
        first_root[18] = 60;
        first_root[20] = 30;
        first_root[22] = 20;
        first_root[23] = 240;
        let mut second_root = note_record(96, 0, 96, 64, 60);
        second_root[18] = 30;
        second_root[20] = 80;
        second_root[22] = 200;
        second_root[23] = 40;
        let mut other_channel = note_record(0, 1, 96, 48, 70);
        other_channel[18] = 45;
        other_channel[20] = 75;
        other_channel[22] = 125;
        other_channel[23] = 175;
        let input = pattern_fixture(&[first_root, second_root, other_channel], &[0xFF, 0]);
        let options = RiffMachineOptions {
            velocity_variation_percent: 100,
            pan_variation_percent: 100,
            release_variation_percent: 100,
            mod_x_variation_percent: 100,
            mod_y_variation_percent: 100,
            pitch_variation_semitones: 12,
            bipolar_levels: true,
            seed: 23,
            ..RiffMachineOptions::default()
        };
        let mut first = FlpDocument::parse(&input).expect("fixture should parse");
        let mut second = FlpDocument::parse(&input).expect("fixture should parse");
        let mut without_pitch_variation = FlpDocument::parse(&input).expect("fixture should parse");
        first
            .riff_machine_pattern_notes(7, 0, options)
            .expect("Riff Machine should randomize level properties");
        second
            .riff_machine_pattern_notes(7, 0, options)
            .expect("the same seed should produce the same levels");
        without_pitch_variation
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    pitch_variation_semitones: 0,
                    ..options
                },
            )
            .expect("Riff Machine should support disabling pitch variation");
        assert_eq!(
            first.encode_lossless().unwrap(),
            second.encode_lossless().unwrap()
        );

        let original_other = FlpDocument::parse(&input)
            .expect("fixture should parse")
            .patterns()
            .unwrap()
            .remove(0)
            .notes
            .remove(2);
        let notes = first.patterns().unwrap().remove(0).notes;
        let generated = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        let unvaried_keys = without_pitch_variation
            .patterns()
            .unwrap()
            .remove(0)
            .notes
            .into_iter()
            .filter(|note| note.channel_id == 0)
            .map(|note| note.key)
            .collect::<Vec<_>>();
        assert_eq!(generated.len(), 8);
        assert!(
            generated
                .iter()
                .zip(unvaried_keys)
                .any(|(varied, unvaried)| varied.key != unvaried)
        );
        assert!(
            generated
                .iter()
                .any(|note| note.velocity != 80 && note.velocity != 60)
        );
        assert!(
            generated
                .iter()
                .any(|note| note.pan != 30 && note.pan != 80)
        );
        assert!(
            generated
                .iter()
                .any(|note| note.release != 60 && note.release != 30)
        );
        assert!(
            generated
                .iter()
                .any(|note| note.mod_x != 20 && note.mod_x != 200)
        );
        assert!(
            generated
                .iter()
                .any(|note| note.mod_y != 240 && note.mod_y != 40)
        );
        assert_eq!(
            notes.iter().find(|note| note.channel_id == 1),
            Some(&original_other)
        );
    }

    #[test]
    fn riff_machine_can_reset_levels_before_randomizing() {
        let mut root = note_record(0, 0, 96, 60, 80);
        root[18] = 45;
        root[20] = 25;
        root[22] = 90;
        root[23] = 180;
        let input = pattern_fixture(&[root], &[0xFF, 0]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        document
            .riff_machine_pattern_notes(
                7,
                0,
                RiffMachineOptions {
                    velocity_variation_percent: 0,
                    reset_levels: true,
                    ..RiffMachineOptions::default()
                },
            )
            .expect("Riff Machine should reset generated note levels");
        let notes = document.patterns().unwrap().remove(0).notes;
        let generated = notes
            .iter()
            .filter(|note| note.channel_id == 0)
            .collect::<Vec<_>>();
        assert!(!generated.is_empty());
        assert!(generated.iter().all(|note| {
            note.velocity == 100
                && note.pan == 64
                && note.release == 0
                && note.mod_x == 0
                && note.mod_y == 0
        }));
    }

    #[test]
    fn riff_machine_groove_requires_snap_when_timing_is_enabled() {
        let input = pattern_fixture(&[note_record(0, 0, 96, 60, 100)], &[0xFF, 0]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original = document.encode_lossless().unwrap();
        assert!(
            document
                .riff_machine_pattern_notes(
                    7,
                    0,
                    RiffMachineOptions {
                        velocity_variation_percent: 0,
                        groove_snap_ticks: None,
                        groove_start_percent: 100,
                        ..RiffMachineOptions::default()
                    },
                )
                .is_err()
        );
        assert_eq!(document.encode_lossless().unwrap(), original);
    }

    #[test]
    fn claw_machine_trashes_periodic_slices_and_preserves_other_channels() {
        let note_records = [
            note_record(0, 0, 24, 60, 100),
            note_record(24, 0, 24, 62, 95),
            note_record(48, 0, 24, 64, 90),
            note_record(72, 0, 24, 65, 85),
            note_record(96, 0, 24, 67, 80),
            note_record(120, 0, 24, 69, 75),
            note_record(144, 0, 24, 71, 70),
            note_record(168, 0, 24, 72, 65),
            note_record(72, 1, 24, 48, 60),
        ];
        let unknown_event = [0xFF, 1, 0xA7];
        let mut document = FlpDocument::parse(&pattern_fixture(&note_records, &unknown_event))
            .expect("fixture should parse");

        let changed = document
            .claw_pattern_notes(
                7,
                0,
                ClawMachineOptions {
                    period_ticks: 384,
                    trash_every: 4,
                    ..ClawMachineOptions::default()
                },
            )
            .expect("Claw Machine should process the channel");

        assert_eq!(changed, 2);
        let notes = document.patterns().unwrap().remove(0).notes;
        assert_eq!(
            notes
                .iter()
                .filter(|note| note.channel_id == 0)
                .map(|note| note.position)
                .collect::<Vec<_>>(),
            [0, 24, 48, 96, 120, 144]
        );
        assert_eq!(notes.last().unwrap().position, 72);
        assert_eq!(
            document.events().last().unwrap().wire_bytes(),
            &unknown_event
        );

        let encoded = document.encode_lossless().unwrap();
        let reparsed = FlpDocument::parse(&encoded).expect("edited file should parse");
        assert_eq!(reparsed.patterns().unwrap()[0].notes.len(), 7);
    }

    #[test]
    fn claw_machine_stretches_removed_gaps_and_limits_selection_scope() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 12, 60, 100),
                note_record(24, 0, 12, 62, 90),
                note_record(48, 0, 12, 64, 80),
                note_record(72, 1, 12, 48, 70),
            ],
            &[0xFF, 0],
        );
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");

        let changed = document
            .claw_pattern_note_selection(
                7,
                0,
                &[0, 1],
                ClawMachineOptions {
                    period_ticks: 384,
                    trash_every: 2,
                    stretch_to_compensate: true,
                    ..ClawMachineOptions::default()
                },
            )
            .expect("Claw Machine should process the selection");

        assert_eq!(changed, 2);
        let notes = document.patterns().unwrap().remove(0).notes;
        assert_eq!(notes[0].position, 0);
        assert_eq!(notes[0].length, 36);
        assert_eq!(notes[1].position, 48);
        assert_eq!(notes[1].length, 12);
        assert_eq!(notes[2].channel_id, 1);
        assert_eq!(notes[2].position, 72);
    }

    #[test]
    fn claw_machine_slews_note_timing_and_validates_options() {
        let input = pattern_fixture(
            &[
                note_record(0, 0, 24, 60, 100),
                note_record(96, 0, 24, 62, 90),
                note_record(192, 0, 24, 64, 80),
            ],
            &[0xFF, 0],
        );
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        document
            .claw_pattern_notes(
                7,
                0,
                ClawMachineOptions {
                    period_ticks: 384,
                    trash_every: 16,
                    time_distortion_percent: 100,
                    ..ClawMachineOptions::default()
                },
            )
            .expect("Claw Machine should slew note timing");
        let notes = document.patterns().unwrap().remove(0).notes;
        assert_eq!(notes[0].position, 0);
        assert_eq!(notes[1].position, 6);
        assert_eq!(notes[2].position, 48);

        let mut invalid = FlpDocument::parse(&input).expect("fixture should parse");
        assert!(
            invalid
                .claw_pattern_notes(
                    7,
                    0,
                    ClawMachineOptions {
                        period_ticks: 15,
                        ..ClawMachineOptions::default()
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn limit_notes_snaps_to_scale_in_requested_direction_and_scope() {
        const MAJOR_INTERVALS: &[u8] = &[0, 2, 4, 5, 7, 9, 11];
        let input = pattern_fixture(
            &[
                note_record(0, 0, 60, 61, 100),
                note_record(24, 0, 60, 63, 90),
                note_record(48, 0, 60, 65, 80),
                note_record(72, 1, 60, 61, 70),
            ],
            &[0xFF, 1, 0xA7],
        );

        let mut snap_up = FlpDocument::parse(&input).expect("fixture should parse");
        snap_up
            .limit_pattern_note_range_with_options(
                7,
                0,
                LimitNoteOptions {
                    minimum_key: 60,
                    maximum_key: 66,
                    wrap_to_bottom: false,
                    scale_root: Some(0),
                    scale_intervals: Some(MAJOR_INTERVALS),
                    snap_direction: LimitSnapDirection::Up,
                },
            )
            .expect("notes should snap upward into C major");
        let notes = snap_up.patterns().unwrap().remove(0).notes;
        assert_eq!(notes[0].key, 62);
        assert_eq!(notes[1].key, 64);
        assert_eq!(notes[2].key, 65);
        assert_eq!(notes[3].key, 61);

        let mut snap_down = FlpDocument::parse(&input).expect("fixture should parse");
        snap_down
            .limit_pattern_note_range_with_options(
                7,
                0,
                LimitNoteOptions {
                    minimum_key: 60,
                    maximum_key: 66,
                    wrap_to_bottom: false,
                    scale_root: Some(0),
                    scale_intervals: Some(MAJOR_INTERVALS),
                    snap_direction: LimitSnapDirection::Down,
                },
            )
            .expect("notes should snap downward into C major");
        let notes = snap_down.patterns().unwrap().remove(0).notes;
        assert_eq!(notes[0].key, 60);
        assert_eq!(notes[1].key, 62);

        let mut alternating = FlpDocument::parse(&input).expect("fixture should parse");
        alternating
            .limit_pattern_note_selection_range_with_options(
                7,
                0,
                &[0, 1],
                LimitNoteOptions {
                    minimum_key: 60,
                    maximum_key: 66,
                    wrap_to_bottom: false,
                    scale_root: Some(0),
                    scale_intervals: Some(MAJOR_INTERVALS),
                    snap_direction: LimitSnapDirection::Alternate,
                },
            )
            .expect("selected notes should alternate their snap direction");
        let notes = alternating.patterns().unwrap().remove(0).notes;
        assert_eq!(notes[0].key, 62);
        assert_eq!(notes[1].key, 62);
        assert_eq!(notes[2].key, 65);
        assert_eq!(notes[3].key, 61);

        let mut no_in_scale_key = FlpDocument::parse(&input).expect("fixture should parse");
        assert!(
            no_in_scale_key
                .limit_pattern_note_range_with_options(
                    7,
                    0,
                    LimitNoteOptions {
                        minimum_key: 61,
                        maximum_key: 61,
                        wrap_to_bottom: false,
                        scale_root: Some(0),
                        scale_intervals: Some(MAJOR_INTERVALS),
                        snap_direction: LimitSnapDirection::Up,
                    },
                )
                .is_err()
        );

        let mut wrapped = FlpDocument::parse(&pattern_fixture(
            &[
                note_record(0, 0, 60, 72, 100),
                note_record(24, 0, 60, 84, 80),
            ],
            &[0xFF, 0],
        ))
        .expect("fixture should parse");
        wrapped
            .limit_pattern_note_range_with_options(
                7,
                0,
                LimitNoteOptions {
                    minimum_key: 48,
                    maximum_key: 83,
                    wrap_to_bottom: true,
                    scale_root: None,
                    scale_intervals: None,
                    snap_direction: LimitSnapDirection::Up,
                },
            )
            .expect("notes should wrap to the lowest octave in the range");
        let wrapped_notes = wrapped.patterns().unwrap().remove(0).notes;
        assert_eq!(wrapped_notes[0].key, 48);
        assert_eq!(wrapped_notes[1].key, 48);
    }

    #[test]
    fn articulate_uses_original_or_legato_lengths_and_selected_context() {
        let mut first = note_record(0, 0, 96, 48, 100);
        first[4..6].copy_from_slice(&0x9494_u16.to_le_bytes());
        let next = note_record(48, 0, 24, 55, 90);
        let other_channel = note_record(24, 1, 30, 60, 80);
        let input = pattern_fixture(&[first, next, other_channel], &[0xFF, 1, 0xA7]);

        let mut legato = FlpDocument::parse(&input).expect("fixture should parse");
        let changed = legato
            .articulate_pattern_notes(7, 0, articulate_options(50, 0, 1, false))
            .expect("channel articulation should succeed");
        assert_eq!(changed, 2);
        let notes = legato.patterns().unwrap()[0].notes.clone();
        assert_eq!(notes[0].length, 24);
        assert_eq!(notes[0].flags, 0x9494);
        assert_eq!(notes[1].length, 12);
        assert_eq!(notes[2].length, 30);

        let mut selected_context = FlpDocument::parse(&input).expect("fixture should parse");
        selected_context
            .articulate_pattern_note_selection(
                7,
                0,
                &[0],
                articulate_options(50, 0, 1, false),
                true,
            )
            .expect("selected-only articulation context should succeed");
        let notes = selected_context.patterns().unwrap()[0].notes.clone();
        assert_eq!(notes[0].length, 48);
        assert_eq!(notes[1].length, 24);

        let mut use_lengths = FlpDocument::parse(&input).expect("fixture should parse");
        use_lengths
            .articulate_pattern_note_selection(
                7,
                0,
                &[0],
                articulate_options(50, 0, 1, true),
                false,
            )
            .expect("original-length articulation should succeed");
        let reparsed = FlpDocument::parse(&use_lengths.encode_lossless().unwrap())
            .expect("edited FLP should parse");
        let notes = reparsed.patterns().unwrap()[0].notes.clone();
        assert_eq!(notes[0].length, 48);
        assert_eq!(notes[1].length, 24);
        assert_eq!(
            reparsed.events().last().unwrap().wire_bytes(),
            &[0xFF, 1, 0xA7]
        );
    }

    #[test]
    fn articulate_variation_is_seeded_and_bounded_by_the_scaled_length() {
        let first = note_record(0, 0, 96, 60, 100);
        let second = note_record(120, 0, 48, 64, 90);
        let input = pattern_fixture(&[first, second], &[0xFF, 1, 0x5A]);

        let mut fixed = FlpDocument::parse(&input).expect("fixture should parse");
        assert_eq!(
            fixed
                .articulate_pattern_notes(7, 0, articulate_options(100, 0, 1, true))
                .unwrap(),
            0
        );

        let mut varied_first = FlpDocument::parse(&input).expect("fixture should parse");
        varied_first
            .articulate_pattern_notes(7, 0, articulate_options(100, 50, 1, true))
            .expect("seeded variation should succeed");
        let first_notes = varied_first.patterns().unwrap()[0].notes.clone();
        assert_eq!(first_notes[0].length, 78);
        assert!((48..=144).contains(&first_notes[0].length));
        assert!((24..=72).contains(&first_notes[1].length));
        assert_eq!(first_notes[0].key, 60);
        assert_eq!(first_notes[0].velocity, 100);

        let mut varied_again = FlpDocument::parse(&input).expect("fixture should parse");
        varied_again
            .articulate_pattern_notes(7, 0, articulate_options(100, 50, 1, true))
            .expect("the same seed should be reusable");
        let repeated_notes = varied_again.patterns().unwrap()[0].notes.clone();
        assert_eq!(repeated_notes, first_notes);
        assert_eq!(
            FlpDocument::parse(&varied_first.encode_lossless().unwrap())
                .unwrap()
                .events()
                .last()
                .unwrap()
                .wire_bytes(),
            &[0xFF, 1, 0x5A]
        );
    }

    #[test]
    fn articulate_chop_chords_trims_only_notes_overlapping_the_next_onset() {
        let short_first = note_record(0, 0, 24, 60, 100);
        let long_first = note_record(0, 0, 96, 64, 90);
        let long_second = note_record(48, 0, 96, 67, 85);
        let short_second = note_record(48, 0, 24, 72, 80);
        let final_note = note_record(96, 0, 24, 70, 75);
        let other_channel = note_record(24, 1, 48, 48, 70);
        let input = pattern_fixture(
            &[
                short_first,
                long_first,
                long_second,
                short_second,
                final_note,
                other_channel,
            ],
            &[0xFF, 1, 0x5A],
        );
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let changed = document
            .articulate_pattern_notes(
                7,
                0,
                ArticulateOptions {
                    chop_chords: true,
                    ..ArticulateOptions::default()
                },
            )
            .expect("chord chopping should succeed");

        assert_eq!(changed, 2);
        let notes = document.patterns().unwrap()[0].notes.clone();
        assert_eq!(notes[0].length, 24);
        assert_eq!(notes[1].length, 48);
        assert_eq!(notes[2].length, 48);
        assert_eq!(notes[3].length, 24);
        assert_eq!(notes[4].length, 24);
        assert_eq!(notes[5].length, 48);
        assert_eq!(notes[1].key, 64);
        assert_eq!(notes[1].velocity, 90);
        assert_eq!(
            FlpDocument::parse(&document.encode_lossless().unwrap())
                .unwrap()
                .events()
                .last()
                .unwrap()
                .wire_bytes(),
            &[0xFF, 1, 0x5A]
        );
    }

    #[test]
    fn articulate_validates_ranges_and_context_requirements() {
        let note = note_record(0, 0, 48, 60, 100);
        let mut document = FlpDocument::parse(&pattern_fixture(&[note], &[0xFF, 0]))
            .expect("fixture should parse");

        assert!(
            document
                .articulate_pattern_notes(7, 0, articulate_options(9, 0, 1, true))
                .is_err()
        );
        assert!(
            document
                .articulate_pattern_notes(7, 0, articulate_options(101, 0, 1, true))
                .is_err()
        );
        assert!(
            document
                .articulate_pattern_notes(7, 0, articulate_options(50, 0, 1, false))
                .is_ok()
        );
        assert!(
            document
                .articulate_pattern_note_selection(
                    7,
                    0,
                    &[0],
                    articulate_options(50, 0, 1, false),
                    true,
                )
                .is_ok()
        );
        assert!(
            document
                .articulate_pattern_note_selection(
                    7,
                    0,
                    &[0],
                    articulate_options(50, 0, 1, true),
                    true,
                )
                .is_err()
        );
        assert!(
            document
                .articulate_pattern_notes(7, 0, articulate_options(100, 101, 1, true))
                .is_err()
        );

        let zero_length_note = note_record(0, 0, 0, 48, 90);
        let mut zero_length = FlpDocument::parse(&pattern_fixture(&[zero_length_note], &[0xFF, 0]))
            .expect("zero-length fixture should parse");
        assert_eq!(
            zero_length
                .articulate_pattern_notes(7, 0, articulate_options(50, 50, 1, true))
                .unwrap(),
            0
        );
        assert_eq!(zero_length.patterns().unwrap()[0].notes[0].length, 0);
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
    fn duplicating_a_pattern_copies_notes_name_and_length_losslessly() {
        let mut note = note_record(48, 0, 192, 64, 93);
        note[5] = 0xA5;
        note[11] = 0x37;
        let mut stream = Vec::new();
        append_data_event(&mut stream, 0xC7, b"26.0.0\0");
        stream.extend_from_slice(&[0x40, 0, 0, 0x41, 7, 0, 0xD0, 0x18]);
        stream.extend_from_slice(&note);
        stream.extend_from_slice(&[0xA4, 0x80, 0x07, 0, 0]);
        append_project_info_string(&mut stream, 0xC1, "Verse");
        stream.extend_from_slice(&[0x40, 1, 0, 0x15, 2]);
        let input = flp_fixture(&stream, &[0xA1], &[0xB2]);
        let mut document = FlpDocument::parse(&input).expect("fixture should parse");
        let original_event_bytes = document
            .events()
            .iter()
            .map(|event| event.wire_bytes().to_vec())
            .collect::<Vec<_>>();

        let new_pattern_id = document
            .duplicate_pattern(7)
            .expect("the recognized pattern should duplicate");

        assert_eq!(new_pattern_id, 8);
        let patterns = document.patterns().expect("patterns should decode");
        let source = patterns.iter().find(|pattern| pattern.id == 7).unwrap();
        let duplicate = patterns
            .iter()
            .find(|pattern| pattern.id == new_pattern_id)
            .unwrap();
        assert_eq!(duplicate.notes, source.notes);
        assert_eq!(duplicate.length_ticks, Some(1920));
        assert_eq!(duplicate.name.as_deref(), Some("Verse"));

        let duplicate_marker = document
            .events()
            .iter()
            .position(|event| {
                event.opcode() == 0x41 && event.payload() == new_pattern_id.to_le_bytes()
            })
            .expect("duplicate marker should be present");
        assert_eq!(document.events()[duplicate_marker + 1].opcode(), 0xD0);
        assert_eq!(document.events()[duplicate_marker + 2].opcode(), 0xA4);
        assert_eq!(document.events()[duplicate_marker + 3].opcode(), 0xC1);
        let event_bytes_without_duplicate = document
            .events()
            .iter()
            .enumerate()
            .filter(|(index, _)| !(*index >= duplicate_marker && *index < duplicate_marker + 4))
            .map(|(_, event)| event.wire_bytes().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(event_bytes_without_duplicate, original_event_bytes);

        let reopened = FlpDocument::parse(&document.encode_lossless().unwrap())
            .expect("duplicated project should reparse");
        let reopened_duplicate = reopened
            .patterns()
            .unwrap()
            .into_iter()
            .find(|pattern| pattern.id == new_pattern_id)
            .unwrap();
        assert_eq!(reopened_duplicate.notes, source.notes);
        assert_eq!(reopened_duplicate.length_ticks, Some(1920));
        assert_eq!(reopened_duplicate.name.as_deref(), Some("Verse"));
        assert_eq!(reopened.trailing_bytes(), &[0xB2]);
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
    fn layer_note_router_fans_out_and_uses_repeatable_random_child_choices() {
        let all_children = FlpDocument::parse(&layer_channel_fixture(0, &[1, 2]))
            .expect("Layer fixture should parse");
        let router = ChannelNoteRouter::new(all_children.channels());
        assert_eq!(router.targets_for_note(0, 23, 60), vec![1, 2]);
        assert!(router.targets_for_note(99, 23, 60).is_empty());

        let random_layer = FlpDocument::parse(&layer_channel_fixture(1, &[1, 2]))
            .expect("random Layer fixture should parse");
        let router = ChannelNoteRouter::new(random_layer.channels());
        let first_choice = router.targets_for_note(0, 23, 60);
        assert_eq!(first_choice.len(), 1);
        assert_eq!(router.targets_for_note(0, 23, 60), first_choice);
        let choices = (0..64)
            .flat_map(|seed| router.targets_for_note(0, seed, 60))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(choices, [1, 2].into_iter().collect());
    }

    #[test]
    fn layer_note_router_skips_missing_disabled_and_nested_layer_children() {
        let mut event_stream = vec![0x40, 0, 0, 0x15, 3, 0x90];
        event_stream.extend_from_slice(&0u32.to_le_bytes());
        for child in [1u16, 2, 3, 99] {
            event_stream.push(0x5E);
            event_stream.extend_from_slice(&child.to_le_bytes());
        }
        event_stream.extend_from_slice(&[0x40, 1, 0, 0x00, 0, 0x15, 2]);
        event_stream.extend_from_slice(&[0x40, 2, 0, 0x15, 3]);
        event_stream.extend_from_slice(&[0x40, 3, 0, 0x15, 4, 0x62, 0, 0]);
        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("Layer fixture should parse");
        let router = ChannelNoteRouter::new(document.channels());
        assert_eq!(router.targets_for_note(0, 0, 60), vec![3]);
    }

    #[test]
    fn note_router_applies_source_and_child_key_regions_before_random_selection() {
        let mut event_stream = vec![0x40, 0, 0, 0x15, 3, 0x90];
        event_stream.extend_from_slice(&1u32.to_le_bytes());
        event_stream.push(0x5E);
        event_stream.extend_from_slice(&1u16.to_le_bytes());
        event_stream.push(0x5E);
        event_stream.extend_from_slice(&2u16.to_le_bytes());
        append_data_event(
            &mut event_stream,
            0xC7,
            &channel_key_region_parameters(48, 84),
        );

        for (channel_id, low, high) in [(1_u16, 48, 72), (2_u16, 60, 84)] {
            event_stream.extend_from_slice(&[0x40, channel_id as u8, 0, 0x15, 0]);
            append_data_event(
                &mut event_stream,
                0xC7,
                &channel_key_region_parameters(low, high),
            );
        }
        event_stream.extend_from_slice(&[0x40, 3, 0, 0x15, 0]);
        append_data_event(
            &mut event_stream,
            0xC7,
            &channel_key_region_parameters(60, 60),
        );
        event_stream.extend_from_slice(&[0x62, 0, 0]);

        let document = FlpDocument::parse(&flp_fixture(&event_stream, &[], &[]))
            .expect("Layer key-region fixture should parse");
        let router = ChannelNoteRouter::new(document.channels());

        assert_eq!(router.targets_for_note(0, 1, 47), Vec::<u16>::new());
        assert_eq!(router.targets_for_note(0, 1, 48), vec![1]);
        assert_eq!(router.targets_for_note(0, 2, 59), vec![1]);
        assert_eq!(router.targets_for_note(0, 3, 73), vec![2]);
        assert_eq!(router.targets_for_note(0, 4, 84), vec![2]);
        assert_eq!(router.targets_for_note(0, 5, 85), Vec::<u16>::new());
        assert_eq!(router.targets_for_note(3, 6, 59), Vec::<u16>::new());
        assert_eq!(router.targets_for_note(3, 6, 60), vec![3]);
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
