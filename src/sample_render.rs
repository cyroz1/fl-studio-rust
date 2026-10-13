//! Offline rendering for Playlist clips that reference FLP audio channels.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, FrameOffset, Stream};
use flacenc::error::Verify;
use flacenc::source::{Fill, FrameBuf};
use md5::{Digest, Md5};
use mp3lame_encoder::{
    Bitrate as Mp3Bitrate, Builder as Mp3EncoderBuilder, Encoder as Mp3Encoder, FlushNoGap,
    InterleavedPcm, Mode as Mp3Mode, MonoPcm, Quality as Mp3Quality,
};
#[cfg(test)]
use vorbis_rs::VorbisDecoder;
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoder, VorbisEncoderBuilder};

use crate::audio::{
    AudioInputRecording, MIXER_TRACK_METER_COUNT, MixerTrackPeakMeters, StreamingAudioWriter,
};
use crate::media::{DecodedAudio, SamplePathResolver, decode_audio_file, wav_sampler_metadata};
use crate::vst3::Vst3PlaylistStreamProcessor;
use crate::{
    Arrangement, ChannelNoteRouter, ChannelSummary, FlpDocument, MixerInsertSignalTransform,
    MixerRouteAudibility, MixerSourceRoutePlan, MixerSourceRouteTransform, Pattern, PatternNote,
    PlaylistClip, PlaylistClipTarget,
};

const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const MAX_MIX_BYTES: usize = 512 * 1024 * 1024;
const MAX_SOURCE_CACHE_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_SAMPLER_VOICE_LIMIT: usize = 64;
const MAX_SAMPLER_VOICE_LIMIT: usize = 256;
const SAMPLER_RELEASE_SECONDS: f64 = 0.005;
const SAMPLER_ROOT_KEY: u16 = 60;
const SAMPLER_BLOCK_FRAMES: usize = 1_024;
const FLAC_BLOCK_FRAMES: usize = 4_096;
const FLAC_MAX_SAMPLE_FRAMES: u64 = (1_u64 << 36) - 1;
const OGG_MIN_BITRATE_KBPS: u16 = 64;
const OGG_MAX_BITRATE_KBPS: u16 = 450;
pub const MP3_BITRATES_KBPS: [u16; 13] =
    [32, 40, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const MP3_SAMPLE_RATES: [u32; 3] = [32_000, 44_100, 48_000];
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug)]
struct TempoSegment {
    start_tick: u64,
    start_seconds: f64,
    tempo_bpm: f64,
}

/// Maps Playlist ticks to elapsed song time using the project's base tempo and the
/// supported linear `TEMPO` Automation Clip events used by song MIDI export.
#[derive(Clone, Debug)]
pub struct PlaylistTempoMap {
    ppq: u16,
    segments: Vec<TempoSegment>,
}

impl PlaylistTempoMap {
    /// Build the timing map for one Playlist arrangement.
    pub fn for_arrangement(document: &FlpDocument, arrangement_id: u16) -> Result<Self, String> {
        let ppq = document.header().ppq();
        let base_tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
        let arrangement = document
            .arrangements()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
        let events = crate::midi::song_tempo_events(document, &arrangement.clips)
            .map_err(|error| error.to_string())?;
        Self::from_tempo_events(ppq, base_tempo_bpm, &events)
    }

    /// Build a constant-tempo map for a standalone pattern or preview.
    pub fn constant(ppq: u16, tempo_bpm: f64) -> Result<Self, String> {
        Self::from_tempo_events(ppq, tempo_bpm, &[])
    }

    pub(crate) fn from_tempo_events(
        ppq: u16,
        base_tempo_bpm: f64,
        events: &[(u64, f64)],
    ) -> Result<Self, String> {
        if ppq == 0 {
            return Err("project PPQ must be greater than zero".to_owned());
        }
        if !base_tempo_bpm.is_finite() || base_tempo_bpm <= 0.0 {
            return Err("project tempo must be finite and positive".to_owned());
        }

        let mut sorted_events = events.to_vec();
        sorted_events.sort_by_key(|(tick, _)| *tick);
        let mut collapsed_events = Vec::<(u64, f64)>::with_capacity(sorted_events.len());
        for (tick, tempo_bpm) in sorted_events {
            if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
                return Err("tempo automation must be finite and positive".to_owned());
            }
            if let Some((last_tick, last_tempo)) = collapsed_events.last_mut()
                && *last_tick == tick
            {
                // Match the MIDI event stream's stable ordering: when more than one
                // event lands on the same tick, the last event takes effect.
                *last_tempo = tempo_bpm;
            } else {
                collapsed_events.push((tick, tempo_bpm));
            }
        }

        let mut segments = vec![TempoSegment {
            start_tick: 0,
            start_seconds: 0.0,
            tempo_bpm: base_tempo_bpm,
        }];
        for (tick, tempo_bpm) in collapsed_events {
            if tick == 0 {
                segments[0].tempo_bpm = tempo_bpm;
                continue;
            }
            let previous = segments.last().expect("tempo map has an initial segment");
            let elapsed_ticks = tick.saturating_sub(previous.start_tick);
            let start_seconds = previous.start_seconds
                + elapsed_ticks as f64 * 60.0 / (f64::from(ppq) * previous.tempo_bpm);
            if !start_seconds.is_finite() || start_seconds < previous.start_seconds {
                return Err("tempo automation timeline is outside the renderable range".to_owned());
            }
            segments.push(TempoSegment {
                start_tick: tick,
                start_seconds,
                tempo_bpm,
            });
        }
        Ok(Self { ppq, segments })
    }

    /// Convert an absolute Playlist tick position to a rounded sample-frame position.
    pub fn frame_at_tick(&self, tick: u64, sample_rate: u32) -> Result<u64, String> {
        let frames = self.precise_frame_at_tick(tick, f64::from(sample_rate))?;
        Ok(frames.round() as u64)
    }

    /// Convert a fractional Playlist tick position to a rounded sample-frame position.
    pub fn frame_at_fractional_tick(&self, tick: f64, sample_rate: u32) -> Result<u64, String> {
        if !tick.is_finite() || tick < 0.0 {
            return Err("Playlist position must be finite and non-negative".to_owned());
        }
        if sample_rate == 0 {
            return Err("sample rate must be finite and positive".to_owned());
        }
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_tick as f64 <= tick)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let elapsed_ticks = tick - segment.start_tick as f64;
        let seconds = segment.start_seconds
            + elapsed_ticks * 60.0 / (f64::from(self.ppq) * segment.tempo_bpm);
        let frames = seconds * f64::from(sample_rate);
        if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
            return Err("Playlist position is outside the renderable timeline".to_owned());
        }
        Ok(frames.round() as u64)
    }

    pub(crate) fn precise_frame_at_tick(&self, tick: u64, sample_rate: f64) -> Result<f64, String> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err("sample rate must be finite and positive".to_owned());
        }
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_tick <= tick)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let elapsed_ticks = tick.saturating_sub(segment.start_tick);
        let seconds = segment.start_seconds
            + elapsed_ticks as f64 * 60.0 / (f64::from(self.ppq) * segment.tempo_bpm);
        let frames = seconds * sample_rate;
        if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
            return Err("Playlist position is outside the renderable timeline".to_owned());
        }
        Ok(frames)
    }

    /// Convert a rounded song-frame position back to a fractional Playlist tick.
    pub fn tick_at_frame(&self, frame: u64, sample_rate: u32) -> Option<f64> {
        if sample_rate == 0 {
            return None;
        }
        let seconds = frame as f64 / f64::from(sample_rate);
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_seconds <= seconds)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let ticks = segment.start_tick as f64
            + (seconds - segment.start_seconds) * segment.tempo_bpm * f64::from(self.ppq) / 60.0;
        ticks.is_finite().then_some(ticks.max(0.0))
    }

    /// Convert an audio source's frame duration to a Playlist tick span at `start_tick`.
    /// The sample rate is the source rate, so the source duration remains fixed in seconds
    /// while the tick span changes with the arrangement tempo.
    pub fn ticks_for_audio_frames(
        &self,
        start_tick: u64,
        frame_count: u64,
        sample_rate: u32,
    ) -> Result<u64, String> {
        if frame_count == 0 {
            return Err("audio source duration must be greater than zero".to_owned());
        }
        let start_frame = self.frame_at_tick(start_tick, sample_rate)?;
        let end_frame = start_frame
            .checked_add(frame_count)
            .ok_or_else(|| "audio source duration exceeds the renderable timeline".to_owned())?;
        let end_tick = self
            .tick_at_frame(end_frame, sample_rate)
            .ok_or_else(|| "audio source end is outside the renderable timeline".to_owned())?;
        let tick_span = (end_tick - start_tick as f64).ceil();
        if !tick_span.is_finite() || tick_span <= 0.0 || tick_span > u64::MAX as f64 {
            return Err("audio source duration is outside the Playlist timeline".to_owned());
        }
        Ok(tick_span as u64)
    }

    /// Convert a half-open tick range to its rounded frame duration.
    pub fn frames_between_ticks(
        &self,
        start_tick: u64,
        end_tick: u64,
        sample_rate: u32,
    ) -> Result<u64, String> {
        if end_tick < start_tick {
            return Err("Playlist time range ends before it starts".to_owned());
        }
        let start = self.frame_at_tick(start_tick, sample_rate)?;
        let end = self.frame_at_tick(end_tick, sample_rate)?;
        Ok(end.saturating_sub(start))
    }
}

fn playlist_wav_cue_points(
    document: &FlpDocument,
    arrangement_id: u16,
    sample_rate: u32,
    save_playlist_markers: bool,
    save_note_markers: bool,
    soloed_playlist_track_range: Option<(u32, u32)>,
) -> Result<Vec<WavCuePoint>, String> {
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let tempo_map = PlaylistTempoMap::for_arrangement(document, arrangement_id)?;
    let mut cue_points = Vec::new();
    if save_playlist_markers {
        for (index, marker) in arrangement.time_markers.iter().enumerate() {
            let sample_offset =
                tempo_map.frame_at_tick(u64::from(marker.position_ticks()), sample_rate)?;
            let label = marker
                .name()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Time marker {}", index + 1));
            push_wav_cue_point(
                &mut cue_points,
                sample_offset,
                label,
                "Playlist marker is outside the RIFF/WAVE sample range",
            )?;
        }
    }
    if save_note_markers {
        let ppq = document.header().ppq();
        if ppq == 0 {
            return Err("project PPQ must be greater than zero".to_owned());
        }
        let disabled_track_ids = document
            .playlist_tracks()
            .into_iter()
            .filter(|track| track.enabled == Some(false))
            .map(|track| track.id)
            .collect::<BTreeSet<_>>();
        let track_filter =
            PlaylistTrackFilter::new(&disabled_track_ids, soloed_playlist_track_range);
        let channels = document.channels();
        let audio_channel_ids = channels
            .iter()
            .filter(|channel| channel.kind() == Some(4) && channel.enabled() != Some(false))
            .map(|channel| channel.id())
            .collect::<BTreeSet<_>>();

        let patterns = document.patterns().map_err(|error| error.to_string())?;
        let channel_swing_mix = channels
            .iter()
            .map(|channel| (channel.id(), channel.swing_mix()))
            .collect::<HashMap<_, _>>();
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            track_filter,
            PlaylistPatternScheduleOptions {
                ppq,
                global_swing_mix_raw: document.metadata().global_swing_mix(),
                play_truncated_notes_in_clips: document
                    .project_settings()
                    .is_none_or(|settings| settings.play_truncated_notes_in_clips),
            },
            |channel_id| channel_swing_mix.get(&channel_id).copied().unwrap_or(128),
            |channel_id, _, _| vec![channel_id],
        )?;
        append_wav_note_cue_points(
            &mut cue_points,
            &arrangement,
            &tempo_map,
            sample_rate,
            track_filter,
            &audio_channel_ids,
            &schedule,
        )?;
    }
    cue_points.sort_by_key(|point| (point.sample_offset, point.identifier));
    Ok(cue_points)
}

fn push_wav_cue_point(
    cue_points: &mut Vec<WavCuePoint>,
    sample_offset: u64,
    label: String,
    range_error: &str,
) -> Result<(), String> {
    let identifier = u32::try_from(cue_points.len() + 1)
        .map_err(|_| "Playlist contains too many WAV cue points".to_owned())?;
    let sample_offset = u32::try_from(sample_offset).map_err(|_| range_error.to_owned())?;
    cue_points.push(WavCuePoint {
        identifier,
        sample_offset,
        label,
    });
    Ok(())
}

fn append_wav_note_cue_points(
    cue_points: &mut Vec<WavCuePoint>,
    arrangement: &Arrangement,
    tempo_map: &PlaylistTempoMap,
    sample_rate: u32,
    track_filter: PlaylistTrackFilter<'_>,
    audio_channel_ids: &BTreeSet<u16>,
    schedule: &PlaylistPatternSchedule<'_>,
) -> Result<(), String> {
    for clip in &arrangement.clips {
        if track_filter.clip_is_muted(clip) {
            continue;
        }
        let PlaylistClipTarget::Channel { id } = clip.target() else {
            continue;
        };
        if !audio_channel_ids.contains(&id) {
            continue;
        }
        push_wav_cue_point(
            cue_points,
            tempo_map.frame_at_tick(u64::from(clip.position_ticks), sample_rate)?,
            "Audio Clip".to_owned(),
            "Audio Clip marker is outside the RIFF/WAVE sample range",
        )?;
    }
    for placed in &schedule.notes {
        push_wav_cue_point(
            cue_points,
            tempo_map.frame_at_tick(placed.start_tick, sample_rate)?,
            "Note".to_owned(),
            "Playlist note marker is outside the RIFF/WAVE sample range",
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PanLaw {
    #[default]
    Circular,
    Triangular,
}

/// Unknown stored modes fall back to FL Studio's documented default, Circular.
pub(crate) fn project_pan_law(document: &FlpDocument) -> PanLaw {
    match document.metadata().pan_law_raw() {
        Some(2) => PanLaw::Triangular,
        _ => PanLaw::Circular,
    }
}

fn resolve_sampler_root_key(
    saved_root_key: Option<u16>,
    sample_root_key: Option<u16>,
    read_sample_root_note: bool,
) -> u16 {
    let sample_root_key = sample_root_key.filter(|root_key| *root_key <= 127);
    saved_root_key
        .or(read_sample_root_note.then_some(sample_root_key).flatten())
        .unwrap_or(SAMPLER_ROOT_KEY)
}

fn resolve_sampler_channel_root_key(
    channel: &ChannelSummary,
    sample_root_key: Option<u16>,
    read_sample_root_note: bool,
) -> u16 {
    // Add to Key applies the channel root as a note-input offset. Keep the
    // sample's embedded or default root key as the source pitch reference.
    let saved_root_key = (!channel.keyboard_add_to_key_enabled())
        .then(|| channel.sampler_root_key())
        .flatten();
    resolve_sampler_root_key(saved_root_key, sample_root_key, read_sample_root_note)
}

fn sampler_note_pitch_offset_semitones(
    channel: &ChannelSummary,
    sample_root_key: u16,
    project_master_pitch_cents: Option<i16>,
) -> i16 {
    let root_key_offset = if channel.keyboard_add_to_key_enabled() {
        // A saved root above the sample root moves the incoming note down.
        i16::from(sample_root_key as u8)
            - i16::from(channel.sampler_root_key().unwrap_or(sample_root_key) as u8)
    } else {
        0
    };
    root_key_offset + channel.master_pitch_note_offset_semitones(project_master_pitch_cents)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioClipRenderOptions {
    pub arrangement_id: u16,
    pub sample_rate: u32,
    pub wav_sample_format: WavSampleFormat,
    pub wav_dither_mode: WavDitherMode,
    pub wav_channel_mode: WavChannelMode,
    pub resampling_quality: ResamplingQuality,
    /// Render only this zero-based arrangement clip, rebased to output time zero.
    pub clip_index: Option<usize>,
    /// Keep the selected clip at its original arrangement position in the output.
    pub start_from_song_start: bool,
    /// Render only this inclusive one-based Playlist track range when soloing.
    pub soloed_playlist_track_range: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioRenderOutput {
    Wav,
    Flac { bits_per_sample: u8 },
    Ogg { bitrate_kbps: u16 },
    Mp3 { bitrate_kbps: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioRenderEncodingOptions {
    pub wav_sample_format: WavSampleFormat,
    /// Dither for 16-bit WAV PCM and the 16-bit input stage used by MP3 exports.
    pub wav_dither_mode: WavDitherMode,
    pub wav_channel_mode: WavChannelMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFileRenderOptions {
    pub output_format: AudioRenderOutput,
    pub encoding: AudioRenderEncodingOptions,
}

impl Default for AudioRenderEncodingOptions {
    fn default() -> Self {
        Self {
            wav_sample_format: WavSampleFormat::Float32,
            wav_dither_mode: WavDitherMode::Off,
            wav_channel_mode: WavChannelMode::Stereo,
        }
    }
}

/// Backwards-compatible name retained for callers of the audio-clip renderer.
pub type AudioClipRenderOutput = AudioRenderOutput;

impl AudioRenderOutput {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac { .. } => "flac",
            Self::Ogg { .. } => "ogg",
            Self::Mp3 { .. } => "mp3",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioBufferEncodingSettings {
    sample_rate: u32,
    options: AudioRenderEncodingOptions,
}

impl Default for AudioClipRenderOptions {
    fn default() -> Self {
        Self {
            arrangement_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
            wav_sample_format: WavSampleFormat::Float32,
            wav_dither_mode: WavDitherMode::Off,
            wav_channel_mode: WavChannelMode::Stereo,
            resampling_quality: ResamplingQuality::RENDER_DEFAULT,
            clip_index: None,
            start_from_song_start: false,
            soloed_playlist_track_range: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplerPatternRenderOptions {
    pub pattern_id: u16,
    pub sample_rate: u32,
    pub voice_limit: usize,
    pub wav_sample_format: WavSampleFormat,
    pub wav_dither_mode: WavDitherMode,
    pub wav_channel_mode: WavChannelMode,
    pub resampling_quality: ResamplingQuality,
    pub read_sample_root_note: bool,
}

impl Default for SamplerPatternRenderOptions {
    fn default() -> Self {
        Self {
            pattern_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
            voice_limit: DEFAULT_SAMPLER_VOICE_LIMIT,
            wav_sample_format: WavSampleFormat::Float32,
            wav_dither_mode: WavDitherMode::Off,
            wav_channel_mode: WavChannelMode::Stereo,
            resampling_quality: ResamplingQuality::RENDER_DEFAULT,
            read_sample_root_note: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplerLiveNoteEvent {
    NoteOn { channel: u8, key: u8, velocity: u8 },
    NoteOff { channel: u8, key: u8 },
    PitchBend { channel: u8, cents: i16 },
    Sustain { channel: u8, down: bool },
    ResetControllers { channel: u8, pitch_bend_cents: i16 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamplerPatternRenderSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub notes_rendered: usize,
    pub voices_stolen: usize,
    pub sampler_channels_rendered: usize,
    pub source_files: usize,
    pub notes_skipped_unresolved_sample: usize,
    pub unresolved_sample_channels: Vec<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioClipRenderSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub clips_rendered: usize,
    pub clips_skipped_unsupported_scale: usize,
    pub source_files: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioRecordingSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub overflow_frames: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavSampleFormat {
    Pcm16,
    Pcm24,
    Float32,
}

impl WavSampleFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pcm16 => "16-bit integer PCM",
            Self::Pcm24 => "24-bit integer PCM",
            Self::Float32 => "32-bit float",
        }
    }

    const fn bits_per_sample(self) -> u16 {
        match self {
            Self::Pcm16 => 16,
            Self::Pcm24 => 24,
            Self::Float32 => 32,
        }
    }

    const fn bytes_per_sample(self) -> u16 {
        self.bits_per_sample() / 8
    }

    const fn format_code(self) -> u16 {
        match self {
            Self::Float32 => 3,
            Self::Pcm16 | Self::Pcm24 => 1,
        }
    }
}

/// Dither used before 16-bit WAV or MP3 encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavDitherMode {
    Off,
    Tpdf,
    HighPassTpdf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResamplingQuality {
    Linear,
    Hermite6,
    Sinc16,
    Sinc24,
    Sinc32,
    Sinc64,
    Sinc128,
    Sinc256,
    Sinc512,
}

impl ResamplingQuality {
    pub const LIVE_DEFAULT: Self = Self::Sinc24;
    pub const RENDER_DEFAULT: Self = Self::Sinc32;

    pub const SINC_OPTIONS: [Self; 7] = [
        Self::Sinc16,
        Self::Sinc24,
        Self::Sinc32,
        Self::Sinc64,
        Self::Sinc128,
        Self::Sinc256,
        Self::Sinc512,
    ];

    pub const LIVE_OPTIONS: [Self; 7] = [
        Self::Linear,
        Self::Hermite6,
        Self::Sinc24,
        Self::Sinc64,
        Self::Sinc128,
        Self::Sinc256,
        Self::Sinc512,
    ];

    pub const RENDER_OPTIONS: [Self; 9] = [
        Self::Linear,
        Self::Hermite6,
        Self::Sinc16,
        Self::Sinc24,
        Self::Sinc32,
        Self::Sinc64,
        Self::Sinc128,
        Self::Sinc256,
        Self::Sinc512,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Hermite6 => "6-point Hermite",
            Self::Sinc16 => "16-point sinc",
            Self::Sinc24 => "24-point sinc",
            Self::Sinc32 => "32-point sinc",
            Self::Sinc64 => "64-point sinc",
            Self::Sinc128 => "128-point sinc",
            Self::Sinc256 => "256-point sinc",
            Self::Sinc512 => "512-point sinc",
        }
    }

    pub const fn settings_key(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Hermite6 => "hermite6",
            Self::Sinc16 => "sinc16",
            Self::Sinc24 => "sinc24",
            Self::Sinc32 => "sinc32",
            Self::Sinc64 => "sinc64",
            Self::Sinc128 => "sinc128",
            Self::Sinc256 => "sinc256",
            Self::Sinc512 => "sinc512",
        }
    }

    pub fn from_settings_key(key: &str) -> Option<Self> {
        match key {
            "linear" => Some(Self::Linear),
            "hermite6" => Some(Self::Hermite6),
            "sinc16" => Some(Self::Sinc16),
            "sinc24" => Some(Self::Sinc24),
            "sinc32" => Some(Self::Sinc32),
            "sinc64" => Some(Self::Sinc64),
            "sinc128" => Some(Self::Sinc128),
            "sinc256" => Some(Self::Sinc256),
            "sinc512" => Some(Self::Sinc512),
            _ => None,
        }
    }

    const fn sinc_taps(self) -> Option<usize> {
        match self {
            Self::Sinc16 => Some(16),
            Self::Sinc24 => Some(24),
            Self::Sinc32 => Some(32),
            Self::Sinc64 => Some(64),
            Self::Sinc128 => Some(128),
            Self::Sinc256 => Some(256),
            Self::Sinc512 => Some(512),
            Self::Linear | Self::Hermite6 => None,
        }
    }
}

impl WavDitherMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Tpdf => "TPDF dither",
            Self::HighPassTpdf => "High-pass TPDF dither",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavChannelMode {
    Stereo,
    MonoMerged,
    MonoLeft,
    MonoRight,
}

impl WavChannelMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo",
            Self::MonoMerged => "Mono (merged)",
            Self::MonoLeft => "Mono (left only)",
            Self::MonoRight => "Mono (right only)",
        }
    }

    const fn channel_count(self) -> u16 {
        match self {
            Self::Stereo => 2,
            Self::MonoMerged | Self::MonoLeft | Self::MonoRight => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistRenderOptions {
    pub arrangement_id: u16,
    pub sample_rate: u32,
    pub sampler_voice_limit: usize,
    pub wav_sample_format: WavSampleFormat,
    pub wav_dither_mode: WavDitherMode,
    pub resampling_quality: ResamplingQuality,
    pub wav_channel_mode: WavChannelMode,
    /// Write Playlist time markers as RIFF/WAVE cue points and labels.
    pub wav_save_playlist_markers: bool,
    /// Write note onsets and Audio Clip starts as RIFF/WAVE cue points.
    pub wav_save_note_markers: bool,
    pub tail_seconds: u8,
    /// Render only this inclusive one-based Playlist track range when soloing.
    pub soloed_playlist_track_range: Option<(u32, u32)>,
    pub read_sample_root_note: bool,
}

impl Default for PlaylistRenderOptions {
    fn default() -> Self {
        Self {
            arrangement_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
            sampler_voice_limit: DEFAULT_SAMPLER_VOICE_LIMIT,
            wav_sample_format: WavSampleFormat::Float32,
            wav_dither_mode: WavDitherMode::Off,
            resampling_quality: ResamplingQuality::RENDER_DEFAULT,
            wav_channel_mode: WavChannelMode::Stereo,
            wav_save_playlist_markers: false,
            wav_save_note_markers: false,
            tail_seconds: 0,
            soloed_playlist_track_range: None,
            read_sample_root_note: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WavCuePoint {
    identifier: u32,
    sample_offset: u32,
    label: String,
}

/// Optional live Playlist meter output and seek position for device streaming.
#[derive(Clone, Copy)]
pub struct PlaylistStreamMonitor<'a> {
    start_frame: u64,
    meters: Option<&'a MixerTrackPeakMeters>,
}

impl<'a> PlaylistStreamMonitor<'a> {
    /// Stream from a frame without publishing per-insert meter data.
    pub const fn from_frame(start_frame: u64) -> Self {
        Self {
            start_frame,
            meters: None,
        }
    }

    /// Stream from a frame and publish peaks for supported direct and send insert routes.
    pub const fn with_meters(start_frame: u64, meters: &'a MixerTrackPeakMeters) -> Self {
        Self {
            start_frame,
            meters: Some(meters),
        }
    }
}

/// Peak sample amplitude recorded at a supported non-Master insert's pre-effect meter tap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerInsertPeak {
    track: usize,
    peak_bits: u32,
}

impl MixerInsertPeak {
    pub const fn track(self) -> usize {
        self.track
    }

    pub fn peak(self) -> f32 {
        f32::from_bits(self.peak_bits)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistRenderSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub audio_clips_rendered: usize,
    pub sampler_pattern_clips_rendered: usize,
    pub sampler_notes_rendered: usize,
    pub voices_stolen: usize,
    pub source_files: usize,
    pub audio_clips_skipped_unsupported_scale: usize,
    pub pattern_clips_skipped_unsupported_scale: usize,
    pub notes_skipped_unresolved_sample: usize,
    pub vst3_plugin_channels_rendered: usize,
    pub vst3_notes_rendered: usize,
    pub vst3_plugin_channels_unloaded: usize,
    /// Whether a non-silent Master stem was written by a Mixer stem render.
    pub mixer_master_stem_rendered: bool,
    /// Whether a non-silent Current Mixer track stem was written by a Mixer stem render.
    pub mixer_current_stem_rendered: bool,
    /// Non-zero supported insert peaks from a full-song offline render; `None` for live streams.
    pub mixer_insert_peaks: Option<Vec<MixerInsertPeak>>,
}

/// Which optional buses to write when rendering supported Mixer stems.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MixerStemRenderOptions {
    /// Whether to write the final supported mix as `Master.wav`.
    pub include_master: bool,
    /// Selected Mixer track ordinal to write as `Current.wav`.
    pub current_track: Option<usize>,
}

fn take_mixer_insert_peaks(meters: &MixerTrackPeakMeters) -> Vec<MixerInsertPeak> {
    (1..MIXER_TRACK_METER_COUNT)
        .filter_map(|track| {
            let peak = meters.take_peak(track);
            (peak > 0.0).then_some(MixerInsertPeak {
                track,
                peak_bits: peak.to_bits(),
            })
        })
        .collect()
}

/// Render audio-channel Playlist clips into a WAV using the selected bit depth and channel mode.
///
/// This early render path uses the project's tempo map, clip positions, decoded audio source
/// windows, and decoded audio-channel volume/pan. Legacy source offsets use milliseconds; inferred
/// 88-byte tick windows convert at the project base tempo. The raw volume/pan mapping is
/// provisional and has not been compared against native FL Studio output. Unstretched sources
/// follow project Master pitch when their channel enables it. Tempo-sync and time-stretch modes,
/// Pattern instruments, plug-ins, and Mixer effects are not rendered.
pub fn render_audio_clips_to_wav(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    output_path: impl AsRef<Path>,
) -> Result<AudioClipRenderSummary, String> {
    render_audio_clips_to_file(
        document,
        project_path,
        options,
        output_path,
        AudioRenderOutput::Wav,
    )
}

/// Render Playlist audio clips to WAV, FLAC, Ogg Vorbis, or MP3.
pub fn render_audio_clips_to_file(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    output_path: impl AsRef<Path>,
    output_format: AudioRenderOutput,
) -> Result<AudioClipRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path_with_extension(project_path, output_path, output_format.extension())?;
    let (mix, summary) = render_audio_clips_to_stereo_buffer(document, project_path, options)?;
    write_audio_buffer_to_file(
        output_path,
        &mix,
        summary.frames,
        AudioBufferEncodingSettings {
            sample_rate: options.sample_rate,
            options: AudioRenderEncodingOptions {
                wav_sample_format: options.wav_sample_format,
                wav_dither_mode: options.wav_dither_mode,
                wav_channel_mode: options.wav_channel_mode,
            },
        },
        output_format,
    )?;
    Ok(summary)
}

/// Drain a stopped or active input capture to a stereo 32-bit-float WAV on a worker thread.
///
/// The capture queue is bounded, so callers should start this writer promptly and keep it
/// running until [`AudioInputRecording::is_finished`] becomes true. Any input frames lost to
/// queue overflow are reported in the returned summary.
pub fn write_input_recording_to_wav(
    recording: &AudioInputRecording,
    output_path: impl AsRef<Path>,
) -> Result<AudioRecordingSummary, String> {
    let output_path = output_path.as_ref();
    if !output_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err("audio recording output must use the .wav extension".to_owned());
    }
    let sample_rate = recording.sample_rate();
    if !(8_000..=384_000).contains(&sample_rate) {
        return Err("audio recording sample rate must be between 8,000 and 384,000 Hz".to_owned());
    }

    let mut temporary = TemporaryRenderFile::create(output_path)?;
    let file = temporary
        .file
        .as_mut()
        .expect("temporary render file is open");
    // Reserve a stereo float header; the frame and data sizes are patched after capture ends.
    write_wav_header(
        file,
        sample_rate,
        1,
        8,
        WavSampleFormat::Float32,
        WavChannelMode::Stereo,
    )?;

    let mut frames = 0_u64;
    let mut frame_block = vec![[0.0_f32; 2]; STREAM_BLOCK_FRAMES];
    let mut bytes = Vec::with_capacity(STREAM_BLOCK_FRAMES * 8);
    loop {
        let count = recording.read_frames(&mut frame_block);
        if count == 0 {
            if recording.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }

        let next_frames = frames
            .checked_add(count as u64)
            .ok_or_else(|| "audio recording frame count overflow".to_owned())?;
        if next_frames
            .checked_mul(8)
            .is_none_or(|bytes| bytes > u64::from(u32::MAX - 36))
        {
            return Err("audio recording exceeds the RIFF/WAVE size limit".to_owned());
        }
        bytes.clear();
        for [left, right] in &frame_block[..count] {
            bytes.extend_from_slice(&left.to_le_bytes());
            bytes.extend_from_slice(&right.to_le_bytes());
        }
        file.write_all(&bytes)
            .map_err(|error| format!("could not write recorded WAV data: {error}"))?;
        frames = next_frames;
    }

    if frames == 0 {
        return Err("audio recording contains no frames".to_owned());
    }
    let data_bytes = u32::try_from(frames * 8)
        .map_err(|_| "audio recording exceeds the RIFF/WAVE size limit".to_owned())?;
    file.seek(SeekFrom::Start(4))
        .and_then(|_| file.write_all(&(36_u32 + data_bytes).to_le_bytes()))
        .and_then(|()| file.seek(SeekFrom::Start(40)).map(|_| ()))
        .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
        .map_err(|error| format!("could not finalize recorded WAV header: {error}"))?;

    let summary = AudioRecordingSummary {
        frames,
        sample_rate,
        overflow_frames: recording.overflow_frames(),
    };
    temporary.commit(output_path)?;
    Ok(summary)
}

/// Render enabled Playlist clips that reference audio channels into an interleaved stereo buffer.
/// The frame rate is `options.sample_rate`, so the result can be sent directly to a matching
/// output device without resampling in the real-time callback.
pub fn render_audio_clips_to_stereo_buffer(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
) -> Result<(Vec<f32>, AudioClipRenderSummary), String> {
    render_audio_clips_to_stereo_buffer_inner(document, project_path.as_ref(), options, None)
}

/// Cancellable variant for preparing audio on a background worker.
pub fn render_audio_clips_to_stereo_buffer_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    cancelled: &AtomicBool,
) -> Result<(Vec<f32>, AudioClipRenderSummary), String> {
    render_audio_clips_to_stereo_buffer_inner(
        document,
        project_path.as_ref(),
        options,
        Some(cancelled),
    )
}

/// Decode and mix enabled Playlist audio clips in bounded blocks for live device playback.
///
/// Source files are decoded once, while the song-length mix is never held in memory. The writer
/// applies backpressure outside the device callback and returns promptly after playback stops.
pub fn stream_audio_clips_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<AudioClipRenderSummary, String> {
    let render =
        prepare_audio_clip_render(document, project_path.as_ref(), options, Some(cancelled))?;
    require_audio_clips(&render)?;
    stream_prepared_audio_clip_render(
        &render,
        options.sample_rate,
        STREAM_BLOCK_FRAMES,
        cancelled,
        || writer.is_cancelled(),
        |block| writer.write_stereo_samples(block),
    )?;
    Ok(render.summary)
}

/// Stream enabled Playlist audio clips and Sampler notes placed by Pattern Clips.
///
/// Pattern clips are expanded on the worker thread into sample-offset note events. Audio-device
/// callbacks continue to consume only complete stereo frames from the bounded streaming queue.
/// The recognized Master mute, polarity, and stereo-swap flags are applied to the final mix.
/// Linear Playlist `TEMPO` automation updates audio, Sampler, and VST3 note timing; parameter
/// automation, Mixer routing and processing, and time stretching remain incomplete.
pub fn stream_playlist_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    stream_playlist_with_vst3_to_device(document, project_path, options, writer, cancelled, None)
}

/// Stream Playlist audio, Sampler Pattern Clips, and prepared VST3 instruments into one mix.
pub fn stream_playlist_with_vst3_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
) -> Result<PlaylistRenderSummary, String> {
    stream_playlist_with_vst3_to_device_from_frame(
        document,
        project_path,
        options,
        writer,
        cancelled,
        vst3_processor,
        0,
    )
}

/// Stream the Playlist from a song-frame offset while rendering earlier frames silently so
/// sampler voices and hosted instruments reach the requested position with their state intact.
pub fn stream_playlist_with_vst3_to_device_from_frame(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    start_frame: u64,
) -> Result<PlaylistRenderSummary, String> {
    stream_playlist_with_vst3_to_device_from_frame_inner(
        document,
        project_path,
        options,
        writer,
        cancelled,
        vst3_processor,
        PlaylistStreamMonitor::from_frame(start_frame),
    )
}

/// Stream a Playlist from a seek position and publish supported insert-route peaks.
pub fn stream_playlist_with_vst3_to_device_with_monitor(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    monitor: PlaylistStreamMonitor<'_>,
) -> Result<PlaylistRenderSummary, String> {
    stream_playlist_with_vst3_to_device_from_frame_inner(
        document,
        project_path,
        options,
        writer,
        cancelled,
        vst3_processor,
        monitor,
    )
}

fn stream_playlist_with_vst3_to_device_from_frame_inner(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    monitor: PlaylistStreamMonitor<'_>,
) -> Result<PlaylistRenderSummary, String> {
    let start_frame = monitor.start_frame;
    let meters = monitor.meters;
    let project_path = project_path.as_ref();
    let master_output = MixerMasterOutput::from_document(document);
    let audio = prepare_audio_clip_render(
        document,
        project_path,
        AudioClipRenderOptions {
            arrangement_id: options.arrangement_id,
            sample_rate: options.sample_rate,
            wav_sample_format: options.wav_sample_format,
            wav_dither_mode: options.wav_dither_mode,
            wav_channel_mode: options.wav_channel_mode,
            resampling_quality: options.resampling_quality,
            clip_index: None,
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
        },
        Some(cancelled),
    )?;
    let sampler = prepare_sampler_arrangement(document, project_path, options, Some(cancelled))?;
    if audio.clips.is_empty()
        && sampler.notes.is_empty()
        && vst3_processor
            .as_ref()
            .is_none_or(|processor| processor.summary().plugin_channels_rendered == 0)
    {
        if let Some(processor) = vst3_processor.as_ref() {
            let unloaded_channels = processor.summary().unloaded_plugin_channels;
            if !unloaded_channels.is_empty() {
                let channel_ids = unloaded_channels
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "arrangement has VST3 notes on channel(s) {channel_ids}, but no matching plug-in instance is loaded"
                ));
            }
        }
        return Err(
            "arrangement contains no supported audio clips, Sampler notes, or loaded VST3 notes in Pattern Clips"
                .to_owned(),
        );
    }

    let mut frames = audio.summary.frames.max(sampler.summary.frames);
    if let Some(processor) = vst3_processor.as_mut() {
        frames = frames.max(processor.summary().frames);
        processor.extend_to_output_frames(frames)?;
    }
    let mut source_paths: std::collections::BTreeSet<PathBuf> =
        audio.decoded_by_path.keys().cloned().collect();
    source_paths.extend(sampler.source_paths.iter().cloned());
    let vst3_summary = vst3_processor.as_ref().map(|processor| processor.summary());
    let mut summary = PlaylistRenderSummary {
        frames,
        sample_rate: options.sample_rate,
        audio_clips_rendered: audio.summary.clips_rendered,
        sampler_pattern_clips_rendered: sampler.pattern_clips_rendered,
        sampler_notes_rendered: sampler.summary.notes_rendered,
        voices_stolen: 0,
        source_files: source_paths.len(),
        audio_clips_skipped_unsupported_scale: audio.summary.clips_skipped_unsupported_scale,
        pattern_clips_skipped_unsupported_scale: sampler.pattern_clips_skipped_unsupported_scale,
        notes_skipped_unresolved_sample: sampler.summary.notes_skipped_unresolved_sample,
        vst3_plugin_channels_rendered: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.plugin_channels_rendered),
        vst3_notes_rendered: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.notes_scheduled),
        vst3_plugin_channels_unloaded: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.unloaded_plugin_channels.len()),
        mixer_master_stem_rendered: false,
        mixer_current_stem_rendered: false,
        mixer_insert_peaks: None,
    };

    let mut frames_to_skip = start_frame.min(frames);
    summary.voices_stolen = stream_prepared_playlist_render(
        PreparedPlaylistBlockMix {
            audio: &audio,
            sampler: &sampler,
            options,
            frames,
            master_output,
            vst3_processor: vst3_processor.as_mut(),
        },
        meters,
        cancelled,
        || writer.is_cancelled(),
        |block, _meter_buses| {
            let skipped_frames = frames_to_skip.min((block.len() / 2) as u64) as usize;
            frames_to_skip -= skipped_frames as u64;
            writer.write_stereo_samples(&block[skipped_frames * 2..])
        },
    )?;
    Ok(summary)
}

/// Render enabled Playlist audio clips, Sampler Pattern Clips, and mapped VST3 instruments to a
/// stereo WAV. Audio is mixed in bounded blocks and written to a temporary file, so song length
/// does not determine the in-memory mix size. The recognized Master mute, polarity, and stereo-swap
/// flags are applied to the final mix. Linear Playlist tempo automation is applied to audio,
/// Sampler, and VST3 event timing. Automatic PDC aligns supported direct-to-Master sources using
/// loaded VST3 instrument latency. Supported direct and paired send routes apply recognized
/// insert mute/solo and signal transforms; the summary includes their pre-effect insert peaks.
/// Effect processing, parameter automation, manual PDC, sidechain routing, and PDC through effect
/// or send chains remain unsupported.
pub fn render_playlist_with_vst3_to_wav_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    render_playlist_with_vst3_cancellable(
        document,
        project_path,
        options,
        output_path,
        PlaylistRenderOutput::Wav,
        vst3_processor,
        cancelled,
    )
}

/// Render a full Playlist arrangement to 16- or 24-bit FLAC in bounded audio blocks.
pub fn render_playlist_with_vst3_to_flac_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    bits_per_sample: u8,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    render_playlist_with_vst3_cancellable(
        document,
        project_path,
        options,
        output_path,
        PlaylistRenderOutput::Flac { bits_per_sample },
        vst3_processor,
        cancelled,
    )
}

/// Render a full Playlist arrangement to Ogg Vorbis at the requested target bitrate.
pub fn render_playlist_with_vst3_to_ogg_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    bitrate_kbps: u16,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    render_playlist_with_vst3_cancellable(
        document,
        project_path,
        options,
        output_path,
        PlaylistRenderOutput::Ogg { bitrate_kbps },
        vst3_processor,
        cancelled,
    )
}

/// Render a full Playlist arrangement to an MP3 at the requested constant bitrate.
pub fn render_playlist_with_vst3_to_mp3_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    bitrate_kbps: u16,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    render_playlist_with_vst3_cancellable(
        document,
        project_path,
        options,
        output_path,
        PlaylistRenderOutput::Mp3 { bitrate_kbps },
        vst3_processor,
        cancelled,
    )
}

#[derive(Clone, Copy)]
enum PlaylistRenderOutput {
    Wav,
    Flac { bits_per_sample: u8 },
    Ogg { bitrate_kbps: u16 },
    Mp3 { bitrate_kbps: u16 },
}

fn render_playlist_with_vst3_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    output: PlaylistRenderOutput,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    match output {
        PlaylistRenderOutput::Wav => validate_output_path(project_path, output_path)?,
        PlaylistRenderOutput::Flac { bits_per_sample } => {
            validate_flac_output_path(project_path, output_path)?;
            if !matches!(bits_per_sample, 16 | 24) {
                return Err("FLAC output supports 16- or 24-bit samples".to_owned());
            }
        }
        PlaylistRenderOutput::Ogg { bitrate_kbps } => {
            validate_ogg_output_path(project_path, output_path)?;
            if !(OGG_MIN_BITRATE_KBPS..=OGG_MAX_BITRATE_KBPS).contains(&bitrate_kbps) {
                return Err(format!(
                    "OGG output bitrate must be between {OGG_MIN_BITRATE_KBPS} and {OGG_MAX_BITRATE_KBPS} kbps"
                ));
            }
        }
        PlaylistRenderOutput::Mp3 { bitrate_kbps } => {
            validate_mp3_output_path(project_path, output_path)?;
            if !MP3_BITRATES_KBPS.contains(&bitrate_kbps) {
                return Err(format!("unsupported MP3 bitrate: {bitrate_kbps} kbps"));
            }
            if !MP3_SAMPLE_RATES.contains(&options.sample_rate) {
                return Err(
                    "MP3 output supports sample rates of 32000, 44100, or 48000 Hz".to_owned(),
                );
            }
        }
    }
    let master_output = MixerMasterOutput::from_document(document);
    let audio = prepare_audio_clip_render(
        document,
        project_path,
        AudioClipRenderOptions {
            arrangement_id: options.arrangement_id,
            sample_rate: options.sample_rate,
            wav_sample_format: options.wav_sample_format,
            wav_dither_mode: options.wav_dither_mode,
            wav_channel_mode: options.wav_channel_mode,
            resampling_quality: options.resampling_quality,
            clip_index: None,
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
        },
        Some(cancelled),
    )?;
    let sampler = prepare_sampler_arrangement(document, project_path, options, Some(cancelled))?;
    if audio.clips.is_empty()
        && sampler.notes.is_empty()
        && vst3_processor
            .as_ref()
            .is_none_or(|processor| processor.summary().plugin_channels_rendered == 0)
    {
        if let Some(processor) = vst3_processor.as_ref() {
            let unloaded_channels = processor.summary().unloaded_plugin_channels;
            if !unloaded_channels.is_empty() {
                let channel_ids = unloaded_channels
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "arrangement has VST3 notes on channel(s) {channel_ids}, but no matching plug-in instance is loaded"
                ));
            }
        }
        return Err(
            "arrangement contains no supported audio clips, Sampler notes, or loaded VST3 notes in Pattern Clips"
                .to_owned(),
        );
    }

    let mut content_frames = audio.summary.frames.max(sampler.summary.frames);
    if let Some(processor) = vst3_processor.as_mut() {
        content_frames = content_frames.max(processor.summary().frames);
    }
    let wav_cue_points = if matches!(output, PlaylistRenderOutput::Wav)
        && (options.wav_save_playlist_markers || options.wav_save_note_markers)
    {
        playlist_wav_cue_points(
            document,
            options.arrangement_id,
            options.sample_rate,
            options.wav_save_playlist_markers,
            options.wav_save_note_markers,
            options.soloed_playlist_track_range,
        )?
    } else {
        Vec::new()
    };
    if let Some(last_cue_frame) = wav_cue_points
        .iter()
        .map(|point| u64::from(point.sample_offset))
        .max()
    {
        content_frames = content_frames.max(last_cue_frame);
    }
    let frames = add_render_tail_frames(content_frames, options.sample_rate, options.tail_seconds)?;
    if let Some(processor) = vst3_processor.as_mut() {
        processor.extend_to_output_frames(frames)?;
    }
    if matches!(output, PlaylistRenderOutput::Flac { .. }) && frames > FLAC_MAX_SAMPLE_FRAMES {
        return Err("rendered FLAC exceeds the 36-bit sample-count limit".to_owned());
    }
    let mut source_paths: std::collections::BTreeSet<PathBuf> =
        audio.decoded_by_path.keys().cloned().collect();
    source_paths.extend(sampler.source_paths.iter().cloned());
    let vst3_summary = vst3_processor.as_ref().map(|processor| processor.summary());
    let mut summary = PlaylistRenderSummary {
        frames,
        sample_rate: options.sample_rate,
        audio_clips_rendered: audio.summary.clips_rendered,
        sampler_pattern_clips_rendered: sampler.pattern_clips_rendered,
        sampler_notes_rendered: sampler.summary.notes_rendered,
        voices_stolen: 0,
        source_files: source_paths.len(),
        audio_clips_skipped_unsupported_scale: audio.summary.clips_skipped_unsupported_scale,
        pattern_clips_skipped_unsupported_scale: sampler.pattern_clips_skipped_unsupported_scale,
        notes_skipped_unresolved_sample: sampler.summary.notes_skipped_unresolved_sample,
        vst3_plugin_channels_rendered: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.plugin_channels_rendered),
        vst3_notes_rendered: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.notes_scheduled),
        vst3_plugin_channels_unloaded: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.unloaded_plugin_channels.len()),
        mixer_master_stem_rendered: false,
        mixer_current_stem_rendered: false,
        mixer_insert_peaks: Some(Vec::new()),
    };

    let mixer_track_meters = MixerTrackPeakMeters::default();
    let mut temporary = TemporaryRenderFile::create(output_path)?;
    let file = temporary
        .file
        .as_mut()
        .expect("temporary render file is open");
    match output {
        PlaylistRenderOutput::Wav => {
            let frames_u32 = u32::try_from(frames)
                .map_err(|_| "render is too long for a RIFF/WAVE file".to_owned())?;
            let block_align = options.wav_sample_format.bytes_per_sample()
                * options.wav_channel_mode.channel_count();
            let data_bytes = frames_u32
                .checked_mul(u32::from(block_align))
                .filter(|bytes| *bytes <= u32::MAX - 36)
                .ok_or_else(|| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;
            write_wav_header(
                file,
                options.sample_rate,
                frames_u32,
                data_bytes,
                options.wav_sample_format,
                options.wav_channel_mode,
            )?;
            let mut bytes = Vec::with_capacity(
                STREAM_BLOCK_FRAMES
                    * usize::from(options.wav_channel_mode.channel_count())
                    * usize::from(options.wav_sample_format.bytes_per_sample()),
            );
            let mut dither_state =
                WavDitherState::new(options.wav_dither_mode, options.wav_sample_format);
            summary.voices_stolen = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options,
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _meter_buses| {
                    bytes.clear();
                    append_wav_block_samples_with_dither(
                        block,
                        options.wav_sample_format,
                        options.wav_channel_mode,
                        dither_state.as_mut(),
                        &mut bytes,
                    )?;
                    file.write_all(&bytes)
                        .map_err(|error| format!("could not write rendered WAV data: {error}"))
                },
            )?;
        }
        PlaylistRenderOutput::Flac { bits_per_sample } => {
            let mut encoder = StreamingFlacWriter::new(
                file,
                options.sample_rate,
                options.wav_channel_mode,
                bits_per_sample,
            )?;
            summary.voices_stolen = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options,
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _meter_buses| encoder.write_stereo_block(block, options.wav_channel_mode),
            )?;
            encoder.finalize(frames)?;
        }
        PlaylistRenderOutput::Ogg { bitrate_kbps } => {
            let mut encoder = StreamingOggWriter::new(
                file,
                options.sample_rate,
                options.wav_channel_mode,
                bitrate_kbps,
            )?;
            summary.voices_stolen = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options,
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _meter_buses| encoder.write_stereo_block(block),
            )?;
            encoder.finalize()?;
        }
        PlaylistRenderOutput::Mp3 { bitrate_kbps } => {
            let mut encoder = StreamingMp3Writer::new(
                file,
                options.sample_rate,
                options.wav_channel_mode,
                bitrate_kbps,
                options.wav_dither_mode,
            )?;
            summary.voices_stolen = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options,
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _meter_buses| encoder.write_stereo_block(block),
            )?;
            encoder.finalize()?;
        }
    }
    if matches!(output, PlaylistRenderOutput::Wav) {
        write_wav_cue_points(file, &wav_cue_points)?;
    }
    summary.mixer_insert_peaks = Some(take_mixer_insert_peaks(&mixer_track_meters));
    temporary.commit(output_path)?;
    Ok(summary)
}

/// Render supported Mixer insert buses to separate WAV files, optionally including Master and Current.
///
/// Insert stems use the renderer's pre-effect buses, including supported direct routes, paired
/// sends, insert level/pan transforms, and insert mute/solo state. Only recognized, enabled
/// inserts routed from a project channel are written. The optional Master stem uses the final
/// supported mix after Master controls. Mixer effects, parameter automation, and effect/send-chain
/// PDC are not included.
pub fn render_playlist_mixer_stems_with_vst3_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_directory: impl AsRef<Path>,
    stem_options: MixerStemRenderOptions,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_directory = output_directory.as_ref();
    let MixerStemRenderOptions {
        include_master,
        current_track,
    } = stem_options;
    fs::create_dir_all(output_directory).map_err(|error| {
        format!(
            "could not create Mixer stem folder {}: {error}",
            output_directory.display()
        )
    })?;
    if !output_directory.is_dir() {
        return Err(format!(
            "Mixer stem output {} is not a folder",
            output_directory.display()
        ));
    }

    let routing = MixerRouteAudibility::from_document(document);
    let routed_tracks = document
        .channels()
        .into_iter()
        .flat_map(|channel| {
            routing
                .source_route_plan_for_channel(channel.mixer_track())
                .insert_meter_transforms
                .into_iter()
                .map(|(track, _transform)| track)
        })
        .collect::<BTreeSet<_>>();
    let inserts = document
        .mixer_inserts()
        .into_iter()
        .filter(|insert| {
            insert.ordinal() > 0
                && insert.enabled() != Some(false)
                && routed_tracks.contains(&insert.ordinal())
        })
        .filter(|insert| insert.ordinal() < MIXER_TRACK_METER_COUNT)
        .map(|insert| {
            let track = insert.ordinal();
            let name = insert.name().unwrap_or("").trim();
            let sanitized_name = safe_stem_filename_component(name);
            let label = if sanitized_name.is_empty() {
                format!("Insert {track:02}")
            } else {
                format!("Insert {track:02} - {sanitized_name}")
            };
            MixerStemSpec {
                track,
                label,
                is_master_stem: false,
                is_current_stem: false,
            }
        })
        .collect::<Vec<_>>();
    let master_output = MixerMasterOutput::from_document(document);
    let mut stem_specs = inserts;
    if include_master && master_output.enabled != Some(false) {
        stem_specs.insert(
            0,
            MixerStemSpec {
                track: 0,
                label: "Master".to_owned(),
                is_master_stem: true,
                is_current_stem: false,
            },
        );
    }
    if let Some(track) = current_track {
        let enabled = if track == 0 {
            master_output.enabled
        } else {
            document
                .mixer_inserts()
                .into_iter()
                .find(|insert| insert.ordinal() == track)
                .ok_or_else(|| "the selected Current Mixer track is not recognized".to_owned())?
                .enabled()
        };
        stem_specs.push(mixer_current_stem_spec(track, enabled)?);
    }
    if stem_specs.is_empty() {
        return Err("project has no enabled Mixer stems selected for export".into());
    }

    let audio = prepare_audio_clip_render(
        document,
        project_path,
        AudioClipRenderOptions {
            arrangement_id: options.arrangement_id,
            sample_rate: options.sample_rate,
            wav_sample_format: options.wav_sample_format,
            wav_dither_mode: options.wav_dither_mode,
            wav_channel_mode: options.wav_channel_mode,
            resampling_quality: options.resampling_quality,
            clip_index: None,
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
        },
        Some(cancelled),
    )?;
    let sampler = prepare_sampler_arrangement(document, project_path, options, Some(cancelled))?;
    if audio.clips.is_empty()
        && sampler.notes.is_empty()
        && vst3_processor
            .as_ref()
            .is_none_or(|processor| processor.summary().plugin_channels_rendered == 0)
    {
        if let Some(processor) = vst3_processor.as_ref() {
            let unloaded_channels = processor.summary().unloaded_plugin_channels;
            if !unloaded_channels.is_empty() {
                let channel_ids = unloaded_channels
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "arrangement has VST3 notes on channel(s) {channel_ids}, but no matching plug-in instance is loaded"
                ));
            }
        }
        return Err(
            "arrangement contains no supported audio clips, Sampler notes, or loaded VST3 notes in Pattern Clips"
                .to_owned(),
        );
    }

    let mut content_frames = audio.summary.frames.max(sampler.summary.frames);
    if let Some(processor) = vst3_processor.as_mut() {
        content_frames = content_frames.max(processor.summary().frames);
    }
    let wav_cue_points = if options.wav_save_playlist_markers || options.wav_save_note_markers {
        playlist_wav_cue_points(
            document,
            options.arrangement_id,
            options.sample_rate,
            options.wav_save_playlist_markers,
            options.wav_save_note_markers,
            options.soloed_playlist_track_range,
        )?
    } else {
        Vec::new()
    };
    if let Some(last_cue_frame) = wav_cue_points
        .iter()
        .map(|point| u64::from(point.sample_offset))
        .max()
    {
        content_frames = content_frames.max(last_cue_frame);
    }
    let frames = add_render_tail_frames(content_frames, options.sample_rate, options.tail_seconds)?;
    if let Some(processor) = vst3_processor.as_mut() {
        processor.extend_to_output_frames(frames)?;
    }
    let frames_u32 =
        u32::try_from(frames).map_err(|_| "render is too long for a RIFF/WAVE file".to_owned())?;
    let block_align =
        options.wav_sample_format.bytes_per_sample() * options.wav_channel_mode.channel_count();
    let data_bytes = frames_u32
        .checked_mul(u32::from(block_align))
        .filter(|bytes| *bytes <= u32::MAX - 36)
        .ok_or_else(|| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;

    let mut outputs = stem_specs
        .into_iter()
        .map(|spec| {
            let output_path = output_directory.join(format!("{}.wav", spec.label));
            let mut temporary = TemporaryRenderFile::create(&output_path)?;
            write_wav_header(
                temporary
                    .file
                    .as_mut()
                    .expect("temporary render file is open"),
                options.sample_rate,
                frames_u32,
                data_bytes,
                options.wav_sample_format,
                options.wav_channel_mode,
            )?;
            Ok(MixerStemTemporaryOutput {
                track: spec.track,
                is_master_stem: spec.is_master_stem,
                is_current_stem: spec.is_current_stem,
                output_path,
                temporary,
                peak: 0.0,
                bytes: Vec::with_capacity(
                    STREAM_BLOCK_FRAMES
                        * usize::from(options.wav_channel_mode.channel_count())
                        * usize::from(options.wav_sample_format.bytes_per_sample()),
                ),
                dither_state: WavDitherState::new(
                    options.wav_dither_mode,
                    options.wav_sample_format,
                ),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let mut source_paths: std::collections::BTreeSet<PathBuf> =
        audio.decoded_by_path.keys().cloned().collect();
    source_paths.extend(sampler.source_paths.iter().cloned());
    let vst3_summary = vst3_processor.as_ref().map(|processor| processor.summary());
    let mut summary = PlaylistRenderSummary {
        frames,
        sample_rate: options.sample_rate,
        audio_clips_rendered: audio.summary.clips_rendered,
        sampler_pattern_clips_rendered: sampler.pattern_clips_rendered,
        sampler_notes_rendered: sampler.summary.notes_rendered,
        voices_stolen: 0,
        source_files: source_paths.len(),
        audio_clips_skipped_unsupported_scale: audio.summary.clips_skipped_unsupported_scale,
        pattern_clips_skipped_unsupported_scale: sampler.pattern_clips_skipped_unsupported_scale,
        notes_skipped_unresolved_sample: sampler.summary.notes_skipped_unresolved_sample,
        vst3_plugin_channels_rendered: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.plugin_channels_rendered),
        vst3_notes_rendered: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.notes_scheduled),
        vst3_plugin_channels_unloaded: vst3_summary
            .as_ref()
            .map_or(0, |summary| summary.unloaded_plugin_channels.len()),
        mixer_master_stem_rendered: false,
        mixer_current_stem_rendered: false,
        mixer_insert_peaks: Some(Vec::new()),
    };
    let meters = MixerTrackPeakMeters::default();
    summary.voices_stolen = stream_prepared_playlist_render(
        PreparedPlaylistBlockMix {
            audio: &audio,
            sampler: &sampler,
            options,
            frames,
            master_output,
            vst3_processor: vst3_processor.as_mut(),
        },
        Some(&meters),
        cancelled,
        || false,
        |block, meter_buses| {
            let meter_buses = meter_buses
                .ok_or_else(|| "Mixer insert buses were not prepared for stem export".to_owned())?;
            for output in &mut outputs {
                let stem_block = mixer_stem_block(output.track, block, meter_buses)?;
                output.peak = stem_block
                    .iter()
                    .fold(output.peak, |peak, sample| peak.max(sample.abs()));
                output.bytes.clear();
                append_wav_block_samples_with_dither(
                    stem_block,
                    options.wav_sample_format,
                    options.wav_channel_mode,
                    output.dither_state.as_mut(),
                    &mut output.bytes,
                )?;
                output
                    .temporary
                    .file
                    .as_mut()
                    .expect("temporary Mixer stem file is open")
                    .write_all(&output.bytes)
                    .map_err(|error| {
                        format!(
                            "could not write Mixer insert {} stem: {error}",
                            output.track
                        )
                    })?;
            }
            Ok(())
        },
    )?;
    summary.mixer_insert_peaks = Some(take_mixer_insert_peaks(&meters));
    summary.mixer_master_stem_rendered = outputs
        .iter()
        .any(|output| output.is_master_stem && output.peak > 0.0);
    summary.mixer_current_stem_rendered = outputs
        .iter()
        .any(|output| output.is_current_stem && output.peak > 0.0);
    let rendered_stems = outputs.iter().filter(|output| output.peak > 0.0).count();
    if rendered_stems == 0 {
        return Err("render contained no non-silent selected Mixer stems".into());
    }
    for mut output in outputs {
        if output.peak > 0.0 {
            write_wav_cue_points(
                output
                    .temporary
                    .file
                    .as_mut()
                    .expect("temporary Mixer stem file is open"),
                &wav_cue_points,
            )?;
            output.temporary.commit(&output.output_path)?;
        }
    }
    Ok(summary)
}

fn mixer_stem_block<'a>(
    track: usize,
    master_block: &'a [f32],
    insert_meter_buses: &'a [f32],
) -> Result<&'a [f32], String> {
    if !master_block.len().is_multiple_of(2) {
        return Err("render block must contain interleaved stereo frames".into());
    }
    if track == 0 {
        return Ok(master_block);
    }
    let samples_per_track = master_block.len();
    if track >= MIXER_TRACK_METER_COUNT {
        return Err("Mixer stem track exceeds the supported insert range".into());
    }
    if insert_meter_buses.len() != MIXER_TRACK_METER_COUNT.saturating_mul(samples_per_track) {
        return Err("Mixer insert buses do not match the render block".into());
    }
    let start = track
        .checked_mul(samples_per_track)
        .ok_or_else(|| "Mixer stem offset overflow".to_owned())?;
    let end = start
        .checked_add(samples_per_track)
        .filter(|end| *end <= insert_meter_buses.len())
        .ok_or_else(|| "Mixer stem block exceeds the insert meter buses".to_owned())?;
    Ok(&insert_meter_buses[start..end])
}

fn safe_stem_filename_component(value: &str) -> String {
    let mut safe = String::new();
    for character in value.chars() {
        let character = if character.is_control() || "<>:\"/\\|?*".contains(character) {
            '_'
        } else {
            character
        };
        if safe.len() + character.len_utf8() > 120 {
            break;
        }
        safe.push(character);
    }
    safe.trim_matches(|character: char| character == ' ' || character == '.')
        .to_owned()
}

struct MixerStemSpec {
    track: usize,
    label: String,
    is_master_stem: bool,
    is_current_stem: bool,
}

fn mixer_current_stem_spec(track: usize, enabled: Option<bool>) -> Result<MixerStemSpec, String> {
    if track >= MIXER_TRACK_METER_COUNT {
        return Err("Current Mixer track exceeds the supported insert range".into());
    }
    if enabled == Some(false) {
        return Err("the selected Current Mixer track is muted".into());
    }
    Ok(MixerStemSpec {
        track,
        label: "Current".to_owned(),
        is_master_stem: false,
        is_current_stem: true,
    })
}

struct MixerStemTemporaryOutput {
    track: usize,
    is_master_stem: bool,
    is_current_stem: bool,
    output_path: PathBuf,
    temporary: TemporaryRenderFile,
    peak: f32,
    bytes: Vec<u8>,
    dither_state: Option<WavDitherState>,
}

struct StreamingOggWriter<'a> {
    encoder: Option<VorbisEncoder<&'a mut File>>,
    channel_mode: WavChannelMode,
    channel_buffers: Vec<Vec<f32>>,
}

impl<'a> StreamingOggWriter<'a> {
    fn new(
        file: &'a mut File,
        sample_rate: u32,
        channel_mode: WavChannelMode,
        bitrate_kbps: u16,
    ) -> Result<Self, String> {
        let sample_rate = std::num::NonZeroU32::new(sample_rate)
            .ok_or_else(|| "OGG sample rate must be greater than zero".to_owned())?;
        let channel_count = channel_mode.channel_count() as u8;
        let channels = std::num::NonZeroU8::new(channel_count)
            .ok_or_else(|| "OGG channel count must be greater than zero".to_owned())?;
        let bitrate_bps = u32::from(bitrate_kbps)
            .checked_mul(1_000)
            .ok_or_else(|| "OGG bitrate is too large".to_owned())?;
        let bitrate = std::num::NonZeroU32::new(bitrate_bps)
            .ok_or_else(|| "OGG bitrate must be greater than zero".to_owned())?;
        let mut builder = VorbisEncoderBuilder::new(sample_rate, channels, file)
            .map_err(|error| format!("could not initialize OGG encoder: {error}"))?;
        builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::Abr {
            average_bitrate: bitrate,
        });
        let encoder = builder
            .build()
            .map_err(|error| format!("could not configure OGG encoder: {error}"))?;

        Ok(Self {
            encoder: Some(encoder),
            channel_mode,
            channel_buffers: (0..usize::from(channel_count))
                .map(|_| Vec::with_capacity(STREAM_BLOCK_FRAMES))
                .collect(),
        })
    }

    fn write_stereo_block(&mut self, stereo_block: &[f32]) -> Result<(), String> {
        if !stereo_block.len().is_multiple_of(2) {
            return Err("render block must contain interleaved stereo frames".to_owned());
        }
        for channel in &mut self.channel_buffers {
            channel.clear();
        }
        for frame in stereo_block.as_chunks::<2>().0 {
            match self.channel_mode {
                WavChannelMode::Stereo => {
                    self.channel_buffers[0].push(frame[0]);
                    self.channel_buffers[1].push(frame[1]);
                }
                WavChannelMode::MonoMerged => {
                    self.channel_buffers[0].push(frame[0] * 0.5 + frame[1] * 0.5);
                }
                WavChannelMode::MonoLeft => self.channel_buffers[0].push(frame[0]),
                WavChannelMode::MonoRight => self.channel_buffers[0].push(frame[1]),
            }
        }
        self.encoder
            .as_mut()
            .expect("OGG encoder is available before finalization")
            .encode_audio_block(&self.channel_buffers)
            .map_err(|error| format!("could not encode OGG audio block: {error}"))
    }

    fn finalize(mut self) -> Result<(), String> {
        self.encoder
            .take()
            .expect("OGG encoder is available before finalization")
            .finish()
            .map(|_| ())
            .map_err(|error| format!("could not finalize OGG stream: {error}"))
    }
}

struct StreamingMp3Writer<'a> {
    file: &'a mut File,
    encoder: Mp3Encoder,
    channel_mode: WavChannelMode,
    dither_state: Option<WavDitherState>,
    channel_buffers: Vec<Vec<f32>>,
    interleaved_buffer: Vec<f32>,
    encoded_buffer: Vec<u8>,
}

impl<'a> StreamingMp3Writer<'a> {
    fn new(
        file: &'a mut File,
        sample_rate: u32,
        channel_mode: WavChannelMode,
        bitrate_kbps: u16,
        dither_mode: WavDitherMode,
    ) -> Result<Self, String> {
        let channels = channel_mode.channel_count() as u8;
        let bitrate = mp3_bitrate(bitrate_kbps)
            .ok_or_else(|| format!("unsupported MP3 bitrate: {bitrate_kbps} kbps"))?;
        let mut builder =
            Mp3EncoderBuilder::new().ok_or_else(|| "could not allocate MP3 encoder".to_owned())?;
        builder
            .set_num_channels(channels)
            .map_err(|error| format!("could not configure MP3 channel count: {error:?}"))?;
        builder
            .set_sample_rate(sample_rate)
            .map_err(|error| format!("could not configure MP3 sample rate: {error:?}"))?;
        builder
            .set_output_sample_rate(std::num::NonZeroU32::new(sample_rate))
            .map_err(|error| format!("could not configure MP3 output rate: {error:?}"))?;
        builder
            .set_brate(bitrate)
            .map_err(|error| format!("could not configure MP3 bitrate: {error:?}"))?;
        builder
            .set_quality(Mp3Quality::Best)
            .map_err(|error| format!("could not configure MP3 quality: {error:?}"))?;
        builder
            .set_mode(if channels == 1 {
                Mp3Mode::Mono
            } else {
                Mp3Mode::JointStereo
            })
            .map_err(|error| format!("could not configure MP3 channel mode: {error:?}"))?;
        let encoder = builder
            .build()
            .map_err(|error| format!("could not initialize MP3 encoder: {error:?}"))?;
        let capacity = mp3lame_encoder::max_required_buffer_size(STREAM_BLOCK_FRAMES);

        Ok(Self {
            file,
            encoder,
            channel_mode,
            dither_state: WavDitherState::new(dither_mode, WavSampleFormat::Pcm16),
            channel_buffers: (0..usize::from(channels))
                .map(|_| Vec::with_capacity(STREAM_BLOCK_FRAMES))
                .collect(),
            interleaved_buffer: Vec::with_capacity(STREAM_BLOCK_FRAMES * 2),
            encoded_buffer: Vec::with_capacity(capacity),
        })
    }
}

impl StreamingMp3Writer<'_> {
    fn write_stereo_block(&mut self, stereo_block: &[f32]) -> Result<(), String> {
        if !stereo_block.len().is_multiple_of(2) {
            return Err("render block must contain interleaved stereo frames".to_owned());
        }
        let frame_count = stereo_block.len() / 2;
        for channel in &mut self.channel_buffers {
            channel.clear();
        }
        let channel_mode = self.channel_mode;
        {
            let dither_state = &mut self.dither_state;
            let channel_buffers = &mut self.channel_buffers;
            let interleaved_buffer = &mut self.interleaved_buffer;
            if channel_mode == WavChannelMode::Stereo && dither_state.is_some() {
                interleaved_buffer.clear();
                for frame in stereo_block.as_chunks::<2>().0 {
                    interleaved_buffer.push(quantize_mp3_input_sample(frame[0], dither_state, 0));
                    interleaved_buffer.push(quantize_mp3_input_sample(frame[1], dither_state, 1));
                }
            } else if channel_mode != WavChannelMode::Stereo {
                for frame in stereo_block.as_chunks::<2>().0 {
                    let sample = match channel_mode {
                        WavChannelMode::Stereo => unreachable!(),
                        WavChannelMode::MonoMerged => frame[0] * 0.5 + frame[1] * 0.5,
                        WavChannelMode::MonoLeft => frame[0],
                        WavChannelMode::MonoRight => frame[1],
                    };
                    channel_buffers[0].push(quantize_mp3_input_sample(sample, dither_state, 0));
                }
            }
        }
        self.encoded_buffer.clear();
        self.encoded_buffer
            .reserve(mp3lame_encoder::max_required_buffer_size(frame_count));
        let result = if self.channel_mode == WavChannelMode::Stereo {
            let input = if self.dither_state.is_some() {
                self.interleaved_buffer.as_slice()
            } else {
                stereo_block
            };
            self.encoder
                .encode_to_vec(InterleavedPcm(input), &mut self.encoded_buffer)
        } else {
            self.encoder
                .encode_to_vec(MonoPcm(&self.channel_buffers[0]), &mut self.encoded_buffer)
        };
        result.map_err(|error| format!("could not encode MP3 audio block: {error:?}"))?;
        self.file
            .write_all(&self.encoded_buffer)
            .map_err(|error| format!("could not write rendered MP3 data: {error}"))
    }

    fn finalize(mut self) -> Result<(), String> {
        self.encoded_buffer.clear();
        self.encoded_buffer
            .reserve(mp3lame_encoder::max_required_buffer_size(0));
        self.encoder
            .flush_to_vec::<FlushNoGap>(&mut self.encoded_buffer)
            .map_err(|error| format!("could not finalize MP3 stream: {error:?}"))?;
        self.file
            .write_all(&self.encoded_buffer)
            .map_err(|error| format!("could not write MP3 trailer: {error}"))
    }
}

fn mp3_bitrate(bitrate_kbps: u16) -> Option<Mp3Bitrate> {
    Some(match bitrate_kbps {
        32 => Mp3Bitrate::Kbps32,
        40 => Mp3Bitrate::Kbps40,
        48 => Mp3Bitrate::Kbps48,
        64 => Mp3Bitrate::Kbps64,
        80 => Mp3Bitrate::Kbps80,
        96 => Mp3Bitrate::Kbps96,
        112 => Mp3Bitrate::Kbps112,
        128 => Mp3Bitrate::Kbps128,
        160 => Mp3Bitrate::Kbps160,
        192 => Mp3Bitrate::Kbps192,
        224 => Mp3Bitrate::Kbps224,
        256 => Mp3Bitrate::Kbps256,
        320 => Mp3Bitrate::Kbps320,
        _ => return None,
    })
}

struct StreamingFlacWriter<'a> {
    file: &'a mut File,
    stream: Stream,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    channel_count: usize,
    bits_per_sample: u8,
    pending_pcm: Vec<i32>,
    frame_number: usize,
    sample_frames_written: u64,
    md5: Md5,
}

impl<'a> StreamingFlacWriter<'a> {
    fn new(
        file: &'a mut File,
        sample_rate: u32,
        channel_mode: WavChannelMode,
        bits_per_sample: u8,
    ) -> Result<Self, String> {
        if !matches!(bits_per_sample, 16 | 24) {
            return Err("FLAC output supports 16- or 24-bit samples".to_owned());
        }
        let channel_count = usize::from(channel_mode.channel_count());
        let stream = Stream::new(
            sample_rate as usize,
            channel_count,
            usize::from(bits_per_sample),
        )
        .map_err(|error| format!("could not initialize FLAC stream: {error}"))?;
        write_flac_stream_header(file, &stream)?;

        let mut config = flacenc::config::Encoder::default();
        config.block_size = FLAC_BLOCK_FRAMES;
        config.multithread = false;
        let config = config
            .into_verified()
            .map_err(|(_, error)| format!("invalid FLAC encoder settings: {error}"))?;

        Ok(Self {
            file,
            stream,
            config,
            channel_count,
            bits_per_sample,
            pending_pcm: Vec::with_capacity(FLAC_BLOCK_FRAMES * channel_count),
            frame_number: 0,
            sample_frames_written: 0,
            md5: Md5::new(),
        })
    }

    fn write_stereo_block(
        &mut self,
        stereo_block: &[f32],
        channel_mode: WavChannelMode,
    ) -> Result<(), String> {
        if !stereo_block.len().is_multiple_of(2) {
            return Err("render block must contain interleaved stereo frames".to_owned());
        }
        if channel_mode.channel_count() as usize != self.channel_count {
            return Err("FLAC channel mode changed during rendering".to_owned());
        }

        let scale = 2.0_f64.powi(i32::from(self.bits_per_sample) - 1);
        let max = scale - 1.0;
        let bytes_per_sample = usize::from(self.bits_per_sample / 8);
        for frame in stereo_block.as_chunks::<2>().0 {
            let samples = match channel_mode {
                WavChannelMode::Stereo => [Some(frame[0]), Some(frame[1])],
                WavChannelMode::MonoMerged => [Some(frame[0] * 0.5 + frame[1] * 0.5), None],
                WavChannelMode::MonoLeft => [Some(frame[0]), None],
                WavChannelMode::MonoRight => [Some(frame[1]), None],
            };
            for sample in samples.into_iter().take(self.channel_count).flatten() {
                let pcm = quantize_signed_pcm(sample, scale, max) as i32;
                self.pending_pcm.push(pcm);
                let pcm_bytes = pcm.to_le_bytes();
                self.md5.update(&pcm_bytes[..bytes_per_sample]);
            }
            if self.pending_pcm.len() >= FLAC_BLOCK_FRAMES * self.channel_count {
                self.write_pending_frame(FLAC_BLOCK_FRAMES, FLAC_BLOCK_FRAMES)?;
            }
        }
        Ok(())
    }

    fn write_pending_frame(
        &mut self,
        encoded_frames: usize,
        sample_frames: usize,
    ) -> Result<(), String> {
        if !(16..=FLAC_BLOCK_FRAMES).contains(&encoded_frames) || sample_frames > encoded_frames {
            return Err("FLAC frame size must be between 16 and 4096 samples".to_owned());
        }
        let needed_samples = encoded_frames
            .checked_mul(self.channel_count)
            .ok_or_else(|| "FLAC frame sample count overflow".to_owned())?;
        if self.pending_pcm.len() < needed_samples {
            return Err("FLAC frame buffer ended before the requested block".to_owned());
        }

        let mut frame_buffer = FrameBuf::with_size(self.channel_count, encoded_frames)
            .map_err(|error| format!("could not allocate FLAC frame: {error}"))?;
        frame_buffer
            .fill_interleaved(&self.pending_pcm[..needed_samples])
            .map_err(|error| format!("could not prepare FLAC frame: {error}"))?;
        let encoded = flacenc::encode_fixed_size_frame(
            &self.config,
            &frame_buffer,
            self.frame_number,
            self.stream.stream_info(),
        )
        .map_err(|error| format!("could not encode FLAC frame: {error}"))?;
        let (mut header, subframes) = encoded.into_parts();
        header.set_frame_offset(FrameOffset::StartSample(self.sample_frames_written));
        let frame = Frame::new(header, subframes.into_iter())
            .map_err(|error| format!("could not finalize FLAC frame header: {error}"))?;
        let mut sink = ByteSink::with_capacity(frame.count_bits());
        frame
            .write(&mut sink)
            .map_err(|error| format!("could not serialize FLAC frame: {error}"))?;
        self.file
            .write_all(sink.as_slice())
            .map_err(|error| format!("could not write rendered FLAC frame: {error}"))?;
        self.stream.stream_info_mut().update_frame_info(&frame);
        self.frame_number = self
            .frame_number
            .checked_add(1)
            .ok_or_else(|| "FLAC frame count overflow".to_owned())?;
        self.sample_frames_written = self
            .sample_frames_written
            .checked_add(sample_frames as u64)
            .ok_or_else(|| "FLAC sample count overflow".to_owned())?;
        self.pending_pcm.drain(..needed_samples);
        Ok(())
    }

    fn finalize(mut self, expected_frames: u64) -> Result<(), String> {
        if expected_frames == 0 {
            return Err("rendered FLAC contains no samples".to_owned());
        }
        if !self.pending_pcm.len().is_multiple_of(self.channel_count) {
            return Err("FLAC render ended on a partial sample frame".to_owned());
        }
        let pending_frames = self.pending_pcm.len() / self.channel_count;
        if pending_frames > 0 {
            if pending_frames < 16 {
                self.pending_pcm.resize(16 * self.channel_count, 0);
                self.write_pending_frame(16, pending_frames)?;
            } else {
                self.write_pending_frame(pending_frames, pending_frames)?;
            }
        }
        if self.sample_frames_written != expected_frames {
            return Err("FLAC encoder frame count does not match rendered audio".to_owned());
        }

        let total_samples = usize::try_from(expected_frames)
            .map_err(|_| "rendered FLAC sample count exceeds this platform".to_owned())?;
        self.stream
            .stream_info_mut()
            .set_total_samples(total_samples);
        let digest: [u8; 16] = self.md5.finalize().into();
        self.stream.stream_info_mut().set_md5_digest(&digest);
        rewrite_flac_stream_header(self.file, &self.stream)
    }
}

struct StreamingWavWriter<'a> {
    file: &'a mut File,
    sample_format: WavSampleFormat,
    dither_state: Option<WavDitherState>,
    channel_mode: WavChannelMode,
    expected_frames: u64,
    frames_written: u64,
    bytes: Vec<u8>,
}

impl<'a> StreamingWavWriter<'a> {
    fn new(
        file: &'a mut File,
        sample_rate: u32,
        expected_frames: u64,
        options: AudioRenderEncodingOptions,
    ) -> Result<Self, String> {
        let frames = u32::try_from(expected_frames)
            .map_err(|_| "render is too long for a RIFF/WAVE file".to_owned())?;
        let data_bytes = frames
            .checked_mul(u32::from(options.wav_channel_mode.channel_count()))
            .and_then(|samples| {
                samples.checked_mul(u32::from(options.wav_sample_format.bytes_per_sample()))
            })
            .filter(|bytes| *bytes <= u32::MAX - 36)
            .ok_or_else(|| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;
        write_wav_header(
            file,
            sample_rate,
            frames,
            data_bytes,
            options.wav_sample_format,
            options.wav_channel_mode,
        )?;

        Ok(Self {
            file,
            sample_format: options.wav_sample_format,
            dither_state: WavDitherState::new(options.wav_dither_mode, options.wav_sample_format),
            channel_mode: options.wav_channel_mode,
            expected_frames,
            frames_written: 0,
            bytes: Vec::with_capacity(
                STREAM_BLOCK_FRAMES
                    * usize::from(options.wav_channel_mode.channel_count())
                    * usize::from(options.wav_sample_format.bytes_per_sample()),
            ),
        })
    }

    fn write_stereo_block(&mut self, stereo_block: &[f32]) -> Result<(), String> {
        if !stereo_block.len().is_multiple_of(2) {
            return Err("render block must contain interleaved stereo frames".to_owned());
        }
        let frames = u64::try_from(stereo_block.len() / 2)
            .map_err(|_| "rendered WAV frame count exceeds this platform".to_owned())?;
        self.frames_written = self
            .frames_written
            .checked_add(frames)
            .filter(|written| *written <= self.expected_frames)
            .ok_or_else(|| "rendered WAV contains more frames than its header".to_owned())?;
        self.bytes.clear();
        append_wav_block_samples_with_dither(
            stereo_block,
            self.sample_format,
            self.channel_mode,
            self.dither_state.as_mut(),
            &mut self.bytes,
        )?;
        self.file
            .write_all(&self.bytes)
            .map_err(|error| format!("could not write rendered WAV audio data: {error}"))
    }

    fn finalize(self) -> Result<(), String> {
        if self.frames_written != self.expected_frames {
            return Err("WAV encoder frame count does not match rendered audio".to_owned());
        }
        Ok(())
    }
}

enum AudioFileStreamEncoder<'a> {
    Wav(StreamingWavWriter<'a>),
    Flac(StreamingFlacWriter<'a>),
    Ogg(StreamingOggWriter<'a>),
    Mp3(StreamingMp3Writer<'a>),
}

impl<'a> AudioFileStreamEncoder<'a> {
    fn new(
        file: &'a mut File,
        sample_rate: u32,
        expected_frames: u64,
        options: AudioRenderEncodingOptions,
        output_format: AudioRenderOutput,
    ) -> Result<Self, String> {
        if expected_frames == 0 {
            return Err("rendered audio contains no frames".to_owned());
        }
        match output_format {
            AudioRenderOutput::Wav => Ok(Self::Wav(StreamingWavWriter::new(
                file,
                sample_rate,
                expected_frames,
                options,
            )?)),
            AudioRenderOutput::Flac { bits_per_sample } => {
                Ok(Self::Flac(StreamingFlacWriter::new(
                    file,
                    sample_rate,
                    options.wav_channel_mode,
                    bits_per_sample,
                )?))
            }
            AudioRenderOutput::Ogg { bitrate_kbps } => {
                if !(OGG_MIN_BITRATE_KBPS..=OGG_MAX_BITRATE_KBPS).contains(&bitrate_kbps) {
                    return Err(format!(
                        "Ogg Vorbis bitrate must be between {OGG_MIN_BITRATE_KBPS} and {OGG_MAX_BITRATE_KBPS} kbps"
                    ));
                }
                Ok(Self::Ogg(StreamingOggWriter::new(
                    file,
                    sample_rate,
                    options.wav_channel_mode,
                    bitrate_kbps,
                )?))
            }
            AudioRenderOutput::Mp3 { bitrate_kbps } => {
                if !MP3_BITRATES_KBPS.contains(&bitrate_kbps) {
                    return Err(format!("unsupported MP3 bitrate: {bitrate_kbps} kbps"));
                }
                if !MP3_SAMPLE_RATES.contains(&sample_rate) {
                    return Err(
                        "MP3 output supports sample rates of 32000, 44100, or 48000 Hz".to_owned(),
                    );
                }
                Ok(Self::Mp3(StreamingMp3Writer::new(
                    file,
                    sample_rate,
                    options.wav_channel_mode,
                    bitrate_kbps,
                    options.wav_dither_mode,
                )?))
            }
        }
    }

    fn write_stereo_block(
        &mut self,
        block: &[f32],
        channel_mode: WavChannelMode,
    ) -> Result<(), String> {
        match self {
            Self::Wav(writer) => writer.write_stereo_block(block),
            Self::Flac(writer) => writer.write_stereo_block(block, channel_mode),
            Self::Ogg(writer) => writer.write_stereo_block(block),
            Self::Mp3(writer) => writer.write_stereo_block(block),
        }
    }

    fn finalize(self, expected_frames: u64) -> Result<(), String> {
        match self {
            Self::Wav(writer) => writer.finalize(),
            Self::Flac(writer) => writer.finalize(expected_frames),
            Self::Ogg(writer) => writer.finalize(),
            Self::Mp3(writer) => writer.finalize(),
        }
    }
}

fn write_flac_stream_header(file: &mut File, stream: &Stream) -> Result<(), String> {
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| format!("could not serialize FLAC stream header: {error}"))?;
    let header = sink.as_slice();
    if header.len() != 42 {
        return Err("FLAC stream header has an unexpected size".to_owned());
    }
    file.write_all(header)
        .map_err(|error| format!("could not write FLAC stream header: {error}"))
}

fn rewrite_flac_stream_header(file: &mut File, stream: &Stream) -> Result<(), String> {
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| format!("could not serialize final FLAC metadata: {error}"))?;
    let header = sink.as_slice();
    if header.len() != 42 {
        return Err("final FLAC stream header has an unexpected size".to_owned());
    }
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(header))
        .and_then(|()| file.seek(SeekFrom::End(0)).map(|_| ()))
        .map_err(|error| format!("could not finalize FLAC metadata: {error}"))
}

fn write_wav_header(
    file: &mut File,
    sample_rate: u32,
    frames: u32,
    data_bytes: u32,
    sample_format: WavSampleFormat,
    channel_mode: WavChannelMode,
) -> Result<(), String> {
    let channels = channel_mode.channel_count();
    let block_align = sample_format.bytes_per_sample() * channels;
    let byte_rate = sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "rendered WAV byte rate overflow".to_owned())?;
    file.write_all(b"RIFF")
        .and_then(|()| file.write_all(&(36 + data_bytes).to_le_bytes()))
        .and_then(|()| file.write_all(b"WAVEfmt "))
        .and_then(|()| file.write_all(&16u32.to_le_bytes()))
        .and_then(|()| file.write_all(&sample_format.format_code().to_le_bytes()))
        .and_then(|()| file.write_all(&channels.to_le_bytes()))
        .and_then(|()| file.write_all(&sample_rate.to_le_bytes()))
        .and_then(|()| file.write_all(&byte_rate.to_le_bytes()))
        .and_then(|()| file.write_all(&block_align.to_le_bytes()))
        .and_then(|()| file.write_all(&sample_format.bits_per_sample().to_le_bytes()))
        .and_then(|()| file.write_all(b"data"))
        .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
        .map_err(|error| format!("could not write rendered WAV header: {error}"))?;
    if frames == 0 {
        return Err("rendered WAV contains no frames".to_owned());
    }
    Ok(())
}

fn append_riff_chunk(
    output: &mut Vec<u8>,
    identifier: &[u8; 4],
    data: &[u8],
) -> Result<(), String> {
    let size = u32::try_from(data.len())
        .map_err(|_| "WAV marker metadata exceeds the RIFF chunk-size limit".to_owned())?;
    output.extend_from_slice(identifier);
    output.extend_from_slice(&size.to_le_bytes());
    output.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        output.push(0);
    }
    Ok(())
}

fn write_wav_cue_points(file: &mut File, cue_points: &[WavCuePoint]) -> Result<(), String> {
    if cue_points.is_empty() {
        return Ok(());
    }
    let cue_count = u32::try_from(cue_points.len())
        .map_err(|_| "Playlist contains too many WAV cue points".to_owned())?;
    let cue_capacity = cue_points
        .len()
        .checked_mul(24)
        .and_then(|size| size.checked_add(4))
        .ok_or_else(|| "WAV cue-point table size overflow".to_owned())?;
    let mut cue_data = Vec::with_capacity(cue_capacity);
    cue_data.extend_from_slice(&cue_count.to_le_bytes());
    for point in cue_points {
        cue_data.extend_from_slice(&point.identifier.to_le_bytes());
        cue_data.extend_from_slice(&point.sample_offset.to_le_bytes());
        cue_data.extend_from_slice(b"data");
        cue_data.extend_from_slice(&0_u32.to_le_bytes());
        cue_data.extend_from_slice(&0_u32.to_le_bytes());
        cue_data.extend_from_slice(&point.sample_offset.to_le_bytes());
    }

    let mut associated_data = Vec::new();
    associated_data.extend_from_slice(b"adtl");
    for point in cue_points {
        let label = point
            .label
            .chars()
            .filter(|character| *character != '\0')
            .collect::<String>();
        let label_length = 4_usize
            .checked_add(label.len())
            .and_then(|length| length.checked_add(1))
            .ok_or_else(|| "WAV cue label size overflow".to_owned())?;
        let mut label_data = Vec::with_capacity(label_length);
        label_data.extend_from_slice(&point.identifier.to_le_bytes());
        label_data.extend_from_slice(label.as_bytes());
        label_data.push(0);
        append_riff_chunk(&mut associated_data, b"labl", &label_data)?;
    }

    let mut chunks = Vec::new();
    append_riff_chunk(&mut chunks, b"cue ", &cue_data)?;
    append_riff_chunk(&mut chunks, b"LIST", &associated_data)?;
    let current_length = file
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("could not seek to the end of the WAV file: {error}"))?;
    let new_length = current_length
        .checked_add(chunks.len() as u64)
        .ok_or_else(|| "WAV file size overflow".to_owned())?;
    let riff_size = u32::try_from(new_length.saturating_sub(8))
        .map_err(|_| "WAV markers exceed the RIFF/WAVE size limit".to_owned())?;
    file.write_all(&chunks)
        .map_err(|error| format!("could not write WAV cue points: {error}"))?;
    file.seek(SeekFrom::Start(4))
        .and_then(|_| file.write_all(&riff_size.to_le_bytes()))
        .and_then(|_| file.seek(SeekFrom::End(0)).map(|_| ()))
        .map_err(|error| format!("could not finalize WAV cue-point metadata: {error}"))
}

#[cfg(test)]
fn append_wav_sample(sample: f32, format: WavSampleFormat, output: &mut Vec<u8>) {
    append_wav_sample_with_dither(sample, format, 0.0, output);
}

fn append_wav_sample_with_dither(
    sample: f32,
    format: WavSampleFormat,
    dither_noise_lsb: f64,
    output: &mut Vec<u8>,
) {
    match format {
        WavSampleFormat::Float32 => output.extend_from_slice(&sample.to_le_bytes()),
        WavSampleFormat::Pcm16 => {
            let quantized = quantize_pcm16_sample(sample, dither_noise_lsb);
            output.extend_from_slice(&quantized.to_le_bytes());
        }
        WavSampleFormat::Pcm24 => {
            let quantized = quantize_signed_pcm(sample, 8_388_608.0, 8_388_607.0) as i32;
            let packed = quantized as u32;
            output.extend_from_slice(&packed.to_le_bytes()[..3]);
        }
    }
}

#[cfg(test)]
fn append_wav_block_samples(
    stereo_block: &[f32],
    sample_format: WavSampleFormat,
    channel_mode: WavChannelMode,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    append_wav_block_samples_with_dither(stereo_block, sample_format, channel_mode, None, output)
}

fn append_wav_block_samples_with_dither(
    stereo_block: &[f32],
    sample_format: WavSampleFormat,
    channel_mode: WavChannelMode,
    mut dither_state: Option<&mut WavDitherState>,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    if !stereo_block.len().is_multiple_of(2) {
        return Err("render block must contain interleaved stereo frames".to_owned());
    }
    let mut append_sample = |sample, output_channel| {
        let noise = if sample_format == WavSampleFormat::Pcm16 {
            dither_state
                .as_deref_mut()
                .map_or(0.0, |state| state.next_lsb_noise(output_channel))
        } else {
            0.0
        };
        append_wav_sample_with_dither(sample, sample_format, noise, output);
    };
    for frame in stereo_block.as_chunks::<2>().0 {
        match channel_mode {
            WavChannelMode::Stereo => {
                append_sample(frame[0], 0);
                append_sample(frame[1], 1);
            }
            WavChannelMode::MonoMerged => {
                let merged = frame[0] * 0.5 + frame[1] * 0.5;
                append_sample(merged, 0);
            }
            WavChannelMode::MonoLeft => append_sample(frame[0], 0),
            WavChannelMode::MonoRight => append_sample(frame[1], 0),
        }
    }
    Ok(())
}

struct TpdfDither {
    state: u64,
}

enum WavDitherState {
    Tpdf(TpdfDither),
    HighPassTpdf(HighPassTpdfDither),
}

impl WavDitherState {
    fn new(mode: WavDitherMode, sample_format: WavSampleFormat) -> Option<Self> {
        if sample_format != WavSampleFormat::Pcm16 {
            return None;
        }
        match mode {
            WavDitherMode::Off => None,
            WavDitherMode::Tpdf => Some(Self::Tpdf(TpdfDither::new())),
            WavDitherMode::HighPassTpdf => Some(Self::HighPassTpdf(HighPassTpdfDither::new())),
        }
    }

    #[cfg(test)]
    fn from_seed(mode: WavDitherMode, seed: u64) -> Option<Self> {
        match mode {
            WavDitherMode::Off => None,
            WavDitherMode::Tpdf => Some(Self::Tpdf(TpdfDither::from_seed(seed))),
            WavDitherMode::HighPassTpdf => {
                Some(Self::HighPassTpdf(HighPassTpdfDither::from_seed(seed)))
            }
        }
    }

    fn next_lsb_noise(&mut self, output_channel: usize) -> f64 {
        match self {
            Self::Tpdf(dither) => dither.next_lsb_noise(),
            Self::HighPassTpdf(dither) => dither.next_lsb_noise(output_channel),
        }
    }
}

fn quantize_mp3_input_sample(
    sample: f32,
    dither_state: &mut Option<WavDitherState>,
    output_channel: usize,
) -> f32 {
    let Some(dither_state) = dither_state.as_mut() else {
        return sample;
    };
    let noise = dither_state.next_lsb_noise(output_channel);
    f32::from(quantize_pcm16_sample(sample, noise)) / 32_768.0
}

fn quantize_pcm16_sample(sample: f32, dither_noise_lsb: f64) -> i16 {
    let dithered_sample = sample + (dither_noise_lsb / 32_768.0) as f32;
    quantize_signed_pcm(dithered_sample, 32_768.0, 32_767.0) as i16
}

const HIGH_PASS_DITHER_POLE: f64 = 0.95;

struct HighPassTpdfDither {
    noise: TpdfDither,
    previous_noise: [f64; 2],
}

impl HighPassTpdfDither {
    fn new() -> Self {
        let noise = TpdfDither::new();
        Self::from_noise(noise)
    }

    #[cfg(test)]
    fn from_seed(seed: u64) -> Self {
        Self::from_noise(TpdfDither::from_seed(seed))
    }

    fn from_noise(mut noise: TpdfDither) -> Self {
        Self {
            previous_noise: [noise.next_lsb_noise(), noise.next_lsb_noise()],
            noise,
        }
    }

    fn next_lsb_noise(&mut self, output_channel: usize) -> f64 {
        let current_noise = self.noise.next_lsb_noise();
        let shaped_noise =
            current_noise - HIGH_PASS_DITHER_POLE * self.previous_noise[output_channel];
        self.previous_noise[output_channel] = current_noise;
        shaped_noise
    }
}

impl TpdfDither {
    fn new() -> Self {
        let time_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos() as u64);
        let counter = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let process_seed = u64::from(std::process::id()).rotate_left(17);
        Self::from_seed(time_seed ^ process_seed ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    fn from_seed(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0xA076_1D64_78BD_642F
            } else {
                seed
            },
        }
    }

    fn next_lsb_noise(&mut self) -> f64 {
        self.next_unit() + self.next_unit() - 1.0
    }

    fn next_unit(&mut self) -> f64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        (self.state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64
            * (1.0 / 9_007_199_254_740_992.0)
    }
}

fn quantize_signed_pcm(sample: f32, negative_scale: f64, positive_scale: f64) -> i64 {
    let sample = if sample.is_nan() {
        0.0
    } else {
        f64::from(sample).clamp(-1.0, 1.0)
    };
    let scale = if sample < 0.0 {
        negative_scale
    } else {
        positive_scale
    };
    (sample * scale).round() as i64
}

fn add_render_tail_frames(
    content_frames: u64,
    sample_rate: u32,
    tail_seconds: u8,
) -> Result<u64, String> {
    let tail_frames = u64::from(sample_rate)
        .checked_mul(u64::from(tail_seconds))
        .ok_or_else(|| "render tail length overflow".to_owned())?;
    content_frames
        .checked_add(tail_frames)
        .ok_or_else(|| "render length overflow".to_owned())
}

struct PreparedPlaylistBlockMix<'a> {
    audio: &'a PreparedAudioClipRender,
    sampler: &'a PreparedSamplerArrangement,
    options: PlaylistRenderOptions,
    frames: u64,
    master_output: MixerMasterOutput,
    vst3_processor: Option<&'a mut Vst3PlaylistStreamProcessor>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MixerMasterOutput {
    enabled: Option<bool>,
    polarity_reversed: Option<bool>,
    swap_left_right: Option<bool>,
    fader_volume_raw: Option<i32>,
    mixer_pan_raw: Option<i32>,
    stereo_separation_raw: Option<i32>,
    mixer_pan_law_raw: Option<u8>,
}

impl MixerMasterOutput {
    fn from_document(document: &FlpDocument) -> Self {
        let Some(master) = document.mixer_inserts().into_iter().next() else {
            return Self::default();
        };
        let routing = MixerRouteAudibility::from_document(document);
        Self {
            enabled: master.enabled(),
            polarity_reversed: master.polarity_reversed(),
            swap_left_right: master.swap_left_right(),
            fader_volume_raw: routing.master_fader_raw(),
            mixer_pan_raw: routing.master_pan_raw(),
            stereo_separation_raw: routing.master_stereo_separation_raw(),
            mixer_pan_law_raw: document.metadata().pan_law_raw(),
        }
    }

    fn apply(self, stereo_samples: &mut [f32]) {
        if self.enabled == Some(false) {
            stereo_samples.fill(0.0);
            return;
        }
        let (frames, _) = stereo_samples.as_chunks_mut::<2>();
        let master_transform = MixerInsertSignalTransform::default()
            .with_stereo_separation_raw(self.stereo_separation_raw)
            .with_mixer_pan_raw(self.mixer_pan_raw)
            .with_mixer_pan_law_raw(self.mixer_pan_law_raw)
            .with_fader_volume_raw(self.fader_volume_raw);
        for frame in frames {
            if self.swap_left_right == Some(true) {
                frame.swap(0, 1);
            }
            if self.polarity_reversed == Some(true) {
                frame[0] = -frame[0];
                frame[1] = -frame[1];
            }
            master_transform.apply_frame(frame);
        }
    }
}

fn stream_prepared_playlist_render(
    mut render: PreparedPlaylistBlockMix<'_>,
    meters: Option<&MixerTrackPeakMeters>,
    cancelled: &AtomicBool,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32], Option<&[f32]>) -> Result<(), String>,
) -> Result<usize, String> {
    crate::audio::enable_denormal_protection();
    let frames = render.frames;
    let options = render.options;
    let release_frames = (f64::from(options.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as usize;
    let mut sampler_engine = SamplerVoiceEngine::with_resampling_quality_and_pan_law(
        &render.sampler.sources_by_channel,
        &render.sampler.notes,
        options.sampler_voice_limit,
        release_frames,
        options.sample_rate,
        options.resampling_quality,
        render.sampler.pan_law,
    );
    sampler_engine.mixer_transforms_by_channel = Some(&render.sampler.mixer_transforms_by_channel);
    sampler_engine.mixer_meter_transforms_by_channel =
        Some(&render.sampler.mixer_meter_transforms_by_channel);
    let mut output = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
    let mut sampler_block = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
    let mut meter_buses = if meters.is_some() {
        vec![0.0f32; MIXER_TRACK_METER_COUNT * STREAM_BLOCK_FRAMES * 2]
    } else {
        Vec::new()
    };
    let direct_source_delay_frames = render.vst3_processor.as_deref().map_or(
        0,
        Vst3PlaylistStreamProcessor::pdc_direct_source_delay_frames,
    );
    let mut direct_source_delay = crate::audio::StereoFrameDelay::new(direct_source_delay_frames)?;
    if let Some(processor) = render.vst3_processor.as_deref_mut() {
        processor.start_processing()?;
    }
    let track_groups = audio_clips_grouped_by_mixer_track(&render.audio.clips);
    let worker_count =
        audio_clip_mix_worker_count(&render.audio.clips, &track_groups, options.sample_rate);
    let worker_clips = audio_clip_groups_for_workers(&track_groups, worker_count);
    let audio_mix_context = PreparedAudioClipMixContext {
        clips: &render.audio.clips,
        decoded_by_path: &render.audio.decoded_by_path,
        sample_rate: options.sample_rate,
        resampling_quality: options.resampling_quality,
        cancelled,
    };
    let mix_result = std::thread::scope(|scope| {
        let mut parallel_mixer = if worker_count > 1 {
            ParallelAudioClipMixer::new(
                scope,
                worker_clips,
                audio_mix_context.decoded_by_path,
                options.sample_rate,
                options.resampling_quality,
                cancelled,
                meters.is_some(),
            )
        } else {
            None
        };
        let mix_result = (|| {
            let mut block_start = 0u64;
            while block_start < frames {
                check_cancelled(Some(cancelled))?;
                if stream_cancelled() {
                    return Err("Playlist playback was stopped".to_owned());
                }
                let frame_count = (frames - block_start).min(STREAM_BLOCK_FRAMES as u64) as usize;
                let sample_count = frame_count * 2;
                let block = &mut output[..sample_count];
                block.fill(0.0);
                let mut meter_buses = if meters.is_some() {
                    let active_samples = MIXER_TRACK_METER_COUNT * sample_count;
                    let buses = &mut meter_buses[..active_samples];
                    buses.fill(0.0);
                    Some(buses)
                } else {
                    None
                };
                mix_prepared_audio_clip_block(
                    &audio_mix_context,
                    block_start,
                    block,
                    meter_buses.as_deref_mut(),
                    parallel_mixer.as_ref(),
                )?;
                let sampler_samples = &mut sampler_block[..sample_count];
                if let Some(meter_buses) = meter_buses.as_deref_mut() {
                    sampler_engine.render_block_with_meters(
                        block_start,
                        frame_count,
                        sampler_samples,
                        Some(meter_buses),
                        sample_count,
                    );
                } else {
                    sampler_engine.render_block(block_start, frame_count, sampler_samples);
                }
                for (mixed, sampler_sample) in block.iter_mut().zip(sampler_samples) {
                    *mixed += *sampler_sample;
                }
                direct_source_delay.process_interleaved(block)?;
                if let Some(processor) = render.vst3_processor.as_deref_mut() {
                    if let Some(meter_buses) = meter_buses.as_deref_mut() {
                        processor.mix_next_block_with_meter_buses(block, meter_buses)?;
                    } else {
                        processor.mix_next_block(block)?;
                    }
                }
                if let (Some(meters), Some(meter_buses)) = (meters, meter_buses.as_deref()) {
                    for (track, track_samples) in meter_buses.chunks_exact(sample_count).enumerate()
                    {
                        if track == 0 {
                            continue;
                        }
                        let peak = track_samples
                            .iter()
                            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                        meters.publish_peak(track, peak);
                    }
                }
                render.master_output.apply(block);
                write_block(block, meter_buses.as_deref())?;
                block_start += frame_count as u64;
            }
            Ok::<_, String>(())
        })();
        let worker_result = if let Some(parallel_mixer) = parallel_mixer.as_mut() {
            parallel_mixer.stop_and_join()
        } else {
            Ok(())
        };
        match (mix_result, worker_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    });
    let stop_result = if let Some(processor) = render.vst3_processor.as_deref_mut() {
        processor.stop_processing()
    } else {
        Ok(())
    };
    match (mix_result, stop_result) {
        (Err(error), _) => return Err(error),
        (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }
    Ok(sampler_engine.voices_stolen)
}

/// Render the sample voices in one FLP pattern to a WAV using the selected bit depth and channel mode.
///
/// This initial Sampler path schedules kind-0 channel notes on the project timeline, reads the
/// channel's `0xC4` sample path, resamples by note key, applies velocity and the channel's
/// provisional gain/pan mapping, and caps simultaneous voices. The default sample root key is
/// assumed to be MIDI 60; sampler root-key settings, envelopes, loop modes, filters, and effects
/// are not decoded yet.
pub fn render_sampler_pattern_to_wav(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    output_path: impl AsRef<Path>,
) -> Result<SamplerPatternRenderSummary, String> {
    render_sampler_pattern_to_file(
        document,
        project_path,
        options,
        output_path,
        AudioRenderOutput::Wav,
    )
}

/// Render enabled Sampler notes from one pattern to WAV, FLAC, Ogg Vorbis, or MP3.
pub fn render_sampler_pattern_to_file(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    output_path: impl AsRef<Path>,
    output_format: AudioRenderOutput,
) -> Result<SamplerPatternRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path_with_extension(project_path, output_path, output_format.extension())?;
    let (mix, summary) = render_sampler_pattern_to_stereo_buffer(document, project_path, options)?;
    write_audio_buffer_to_file(
        output_path,
        &mix,
        summary.frames,
        AudioBufferEncodingSettings {
            sample_rate: options.sample_rate,
            options: AudioRenderEncodingOptions {
                wav_sample_format: options.wav_sample_format,
                wav_dither_mode: options.wav_dither_mode,
                wav_channel_mode: options.wav_channel_mode,
            },
        },
        output_format,
    )?;
    Ok(summary)
}

/// Render all enabled kind-0 Sampler notes in one pattern into an interleaved stereo buffer.
pub fn render_sampler_pattern_to_stereo_buffer(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
) -> Result<(Vec<f32>, SamplerPatternRenderSummary), String> {
    let render = prepare_sampler_pattern(document, project_path.as_ref(), options, None)?;
    let output_frames = usize::try_from(render.summary.frames)
        .map_err(|_| "sampler render is too large for this platform".to_owned())?;
    let output_samples = output_frames
        .checked_mul(2)
        .ok_or_else(|| "sampler render size overflow".to_owned())?;
    let output_bytes = output_samples
        .checked_mul(std::mem::size_of::<f32>())
        .filter(|bytes| *bytes <= MAX_MIX_BYTES)
        .ok_or_else(|| {
            format!(
                "sampler render exceeds the {} MiB in-memory mix limit",
                MAX_MIX_BYTES / (1024 * 1024)
            )
        })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_bytes / std::mem::size_of::<f32>())
        .map_err(|error| format!("could not allocate sampler render buffer: {error}"))?;
    let summary = stream_prepared_sampler_pattern(
        &render,
        None,
        || false,
        |block| {
            output.extend_from_slice(block);
            Ok(())
        },
    )?;
    Ok((output, summary))
}

/// Render one pattern's Sampler notes on a worker and stream bounded blocks to an active device.
pub fn stream_sampler_pattern_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    let render =
        prepare_sampler_pattern(document, project_path.as_ref(), options, Some(cancelled))?;
    stream_prepared_sampler_pattern(
        &render,
        Some(cancelled),
        || writer.is_cancelled(),
        |block| writer.write_stereo_samples(block),
    )
}

/// Stream live MIDI note events through one Sampler channel while transport is stopped.
///
/// The pattern is used to prepare the selected channel's sample and routing state. Its saved
/// notes are not played; note events arrive through `events` and are rendered on the worker.
pub fn stream_sampler_midi_input_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    channel_id: u16,
    events: Receiver<SamplerLiveNoteEvent>,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let channels = document.channels();
    let channel = channels
        .iter()
        .find(|channel| channel.id() == channel_id)
        .ok_or_else(|| format!("Sampler channel {channel_id} was not found"))?;
    if channel.kind() != Some(0) || channel.enabled() == Some(false) {
        return Err(format!(
            "channel {channel_id} is not an enabled Sampler channel"
        ));
    }

    let mut preview_document = document.clone();
    let pattern = preview_document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pattern| pattern.id == options.pattern_id)
        .ok_or_else(|| format!("pattern {} was not found", options.pattern_id))?;
    if !pattern
        .notes
        .iter()
        .any(|note| note.channel_id == channel_id && !note.is_muted())
    {
        preview_document
            .add_pattern_note(
                options.pattern_id,
                PatternNote {
                    position: 0,
                    flags: crate::PATTERN_NOTE_DEFAULT_FLAGS,
                    channel_id,
                    length: 1,
                    key: SAMPLER_ROOT_KEY,
                    group: 0,
                    fine_pitch: 120,
                    reserved: 0,
                    release: crate::PATTERN_NOTE_DEFAULT_RELEASE,
                    midi_channel: 0,
                    pan: 64,
                    velocity: 1,
                    mod_x: 128,
                    mod_y: 128,
                },
            )
            .map_err(|error| format!("could not prepare Sampler input audition: {error}"))?;
    }

    let render = prepare_sampler_pattern_for_channel(
        &preview_document,
        project_path.as_ref(),
        options,
        Some(cancelled),
        Some(channel_id),
    )?;
    let source = render
        .sources_by_channel
        .get(&channel_id)
        .ok_or_else(|| format!("Sampler channel {channel_id} has no resolved sample"))?;
    let pitch_offset = sampler_note_pitch_offset_semitones(
        channel,
        source.root_key,
        document.metadata().master_pitch_cents(),
    );
    let empty_notes = [];
    let mut engine = SamplerVoiceEngine::with_resampling_quality_and_pan_law(
        &render.sources_by_channel,
        &empty_notes,
        render.voice_limit,
        (f64::from(render.summary.sample_rate) * SAMPLER_RELEASE_SECONDS)
            .round()
            .max(1.0) as usize,
        render.summary.sample_rate,
        render.resampling_quality,
        render.pan_law,
    );
    engine.mixer_transforms_by_channel = Some(&render.mixer_transforms_by_channel);

    const LIVE_BLOCK_FRAMES: usize = 128;
    let mut block = vec![0.0f32; LIVE_BLOCK_FRAMES * 2];
    let mut block_start = 0u64;
    loop {
        check_cancelled(Some(cancelled))?;
        if writer.is_cancelled() {
            return Err("Sampler input audition was stopped".to_owned());
        }
        loop {
            match events.try_recv() {
                Ok(SamplerLiveNoteEvent::NoteOn {
                    channel: midi_channel,
                    key,
                    velocity,
                }) => {
                    let input_key = u16::from(key.min(127));
                    if channel
                        .keyboard_key_region()
                        .is_none_or(|(low, high)| (low..=high).contains(&input_key))
                    {
                        let note_key = transpose_sampler_note_key(input_key, pitch_offset);
                        if let Some(slot_index) = engine.start_voice(ScheduledSamplerNote {
                            start_frame: block_start,
                            stop_frame: Some(u64::MAX),
                            channel_id,
                            key: note_key,
                            velocity,
                            pan: 64,
                            fine_pitch_offset_cents: 0,
                        }) {
                            engine.set_voice_input_midi_channel(slot_index, midi_channel);
                        }
                    }
                }
                Ok(SamplerLiveNoteEvent::NoteOff { channel, key }) => {
                    let input_key = u16::from(key.min(127));
                    let note_key = transpose_sampler_note_key(input_key, pitch_offset);
                    engine.release_note(channel_id, note_key, channel);
                }
                Ok(SamplerLiveNoteEvent::PitchBend { channel, cents }) => {
                    engine.apply_live_pitch_bend(channel, cents);
                }
                Ok(SamplerLiveNoteEvent::Sustain { channel, down }) => {
                    engine.set_live_sustain(channel, down);
                }
                Ok(SamplerLiveNoteEvent::ResetControllers {
                    channel,
                    pitch_bend_cents,
                }) => {
                    engine.set_live_sustain(channel, false);
                    engine.apply_live_pitch_bend(channel, pitch_bend_cents);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        engine.render_block(block_start, LIVE_BLOCK_FRAMES, &mut block);
        render.master_output.apply(&mut block);
        writer.write_stereo_samples(&block)?;
        block_start = block_start.saturating_add(LIVE_BLOCK_FRAMES as u64);
    }
}

fn stream_prepared_sampler_pattern(
    render: &PreparedSamplerPattern,
    cancelled: Option<&AtomicBool>,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<SamplerPatternRenderSummary, String> {
    crate::audio::enable_denormal_protection();
    let release_frames = (f64::from(render.summary.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as usize;
    let mut engine = SamplerVoiceEngine::with_resampling_quality_and_pan_law(
        &render.sources_by_channel,
        &render.notes,
        render.voice_limit,
        release_frames,
        render.summary.sample_rate,
        render.resampling_quality,
        render.pan_law,
    );
    engine.mixer_transforms_by_channel = Some(&render.mixer_transforms_by_channel);
    let mut block = vec![0.0f32; SAMPLER_BLOCK_FRAMES * 2];
    let mut block_start = 0u64;
    while block_start < render.summary.frames {
        check_cancelled(cancelled)?;
        if stream_cancelled() {
            return Err("Sampler pattern playback was stopped".to_owned());
        }
        let frame_count =
            (render.summary.frames - block_start).min(SAMPLER_BLOCK_FRAMES as u64) as usize;
        let block_samples = &mut block[..frame_count * 2];
        engine.render_block(block_start, frame_count, block_samples);
        render.master_output.apply(block_samples);
        write_block(block_samples)?;
        block_start += frame_count as u64;
    }
    let mut summary = render.summary.clone();
    summary.voices_stolen = engine.voices_stolen;
    Ok(summary)
}

fn stream_prepared_audio_clip_render(
    render: &PreparedAudioClipRender,
    output_rate: u32,
    block_frames: usize,
    cancelled: &AtomicBool,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<(), String> {
    crate::audio::enable_denormal_protection();
    if block_frames == 0 {
        return Err("audio stream block size must be greater than zero".to_owned());
    }
    let mut block = Vec::<f32>::with_capacity(block_frames * 2);
    let mut block_start = 0u64;
    while block_start < render.summary.frames {
        check_cancelled(Some(cancelled))?;
        if stream_cancelled() {
            return Err("audio clip playback was stopped".to_owned());
        }
        let frame_count = (render.summary.frames - block_start).min(block_frames as u64) as usize;
        block.resize(frame_count * 2, 0.0);
        block.fill(0.0);
        for clip in &render.clips {
            let audio = render
                .decoded_by_path
                .get(&clip.path)
                .expect("prepared clips have decoded sources");
            mix_clip_window_into_stereo_with_quality(
                &mut block,
                block_start,
                audio,
                clip,
                output_rate,
                render.resampling_quality,
                Some(cancelled),
            )
            .map_err(|error| {
                format!(
                    "could not mix Playlist clip {} from audio channel {}: {error}",
                    clip.clip_index, clip.channel_id
                )
            })?;
        }
        write_block(&block)?;
        block_start += frame_count as u64;
    }
    Ok(())
}

fn render_audio_clips_to_stereo_buffer_inner(
    document: &FlpDocument,
    project_path: &Path,
    options: AudioClipRenderOptions,
    cancelled: Option<&AtomicBool>,
) -> Result<(Vec<f32>, AudioClipRenderSummary), String> {
    crate::audio::enable_denormal_protection();
    let render = prepare_audio_clip_render(document, project_path, options, cancelled)?;
    require_audio_clips(&render)?;
    let output_frames = render.summary.frames;

    let output_frames_usize = usize::try_from(output_frames)
        .map_err(|_| "render mix buffer is too large for this platform".to_owned())?;
    let mix_samples = output_frames_usize
        .checked_mul(2)
        .ok_or_else(|| "rendered audio size overflow".to_owned())?;
    let _mix_bytes = mix_samples
        .checked_mul(std::mem::size_of::<f32>())
        .filter(|bytes| *bytes <= MAX_MIX_BYTES)
        .ok_or_else(|| {
            format!(
                "render exceeds the {} MiB in-memory mix limit",
                MAX_MIX_BYTES / (1024 * 1024)
            )
        })?;
    let mut mix = Vec::<f32>::new();
    mix.try_reserve_exact(mix_samples)
        .map_err(|error| format!("could not allocate render mix buffer: {error}"))?;
    mix.resize(mix_samples, 0.0);

    for clip in &render.clips {
        check_cancelled(cancelled)?;
        let audio = render
            .decoded_by_path
            .get(&clip.path)
            .expect("prepared clips have decoded sources");
        mix_clip_window_into_stereo_with_quality(
            &mut mix,
            0,
            audio,
            clip,
            options.sample_rate,
            options.resampling_quality,
            cancelled,
        )
        .map_err(|error| {
            format!(
                "could not mix Playlist clip {} from audio channel {}: {error}",
                clip.clip_index, clip.channel_id
            )
        })?;
    }

    Ok((mix, render.summary))
}

fn audio_clip_matches_selection(selected_clip_index: Option<usize>, clip_index: usize) -> bool {
    selected_clip_index.is_none_or(|selected_index| selected_index == clip_index)
}

fn audio_clip_selection_start_tick(
    selected_clip_position_ticks: Option<u32>,
    start_from_song_start: bool,
) -> u32 {
    selected_clip_position_ticks.map_or(
        0,
        |position| {
            if start_from_song_start { 0 } else { position }
        },
    )
}

fn audio_clip_render_start_tick(
    clip_position_ticks: u32,
    selection_start_tick: u32,
) -> Result<u64, String> {
    clip_position_ticks
        .checked_sub(selection_start_tick)
        .map(u64::from)
        .ok_or_else(|| "selected clip position precedes its selection start".to_owned())
}

fn prepare_audio_clip_render(
    document: &FlpDocument,
    project_path: &Path,
    options: AudioClipRenderOptions,
    cancelled: Option<&AtomicBool>,
) -> Result<PreparedAudioClipRender, String> {
    check_cancelled(cancelled)?;
    if !(8_000..=384_000).contains(&options.sample_rate) {
        return Err("render sample rate must be between 8000 and 384000 Hz".to_owned());
    }
    let ppq = document.header().ppq();
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    let base_tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    let tempo_map = PlaylistTempoMap::for_arrangement(document, options.arrangement_id)?;
    let pan_law = project_pan_law(document);
    let master_pitch_cents = document.metadata().master_pitch_cents();
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == options.arrangement_id)
        .ok_or_else(|| format!("arrangement {} was not found", options.arrangement_id))?;
    let disabled_track_ids = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    let track_filter = PlaylistTrackFilter {
        disabled_track_ids: &disabled_track_ids,
        soloed_track_range: options.soloed_playlist_track_range,
    };
    let channels = document.channels();
    let mixer_route_audibility = MixerRouteAudibility::from_document(document);
    let resolver = SamplePathResolver::new(project_path);

    let selected_clip = options
        .clip_index
        .map(|clip_index| {
            arrangement
                .clips
                .get(clip_index)
                .ok_or_else(|| format!("Playlist clip {clip_index} does not exist"))
        })
        .transpose()?;
    let selection_start_tick = audio_clip_selection_start_tick(
        selected_clip.map(|clip| clip.position_ticks),
        options.start_from_song_start,
    );

    let mut max_tick = 0u64;
    let mut clips_skipped_unsupported_scale = 0usize;
    let mut candidate_clips = Vec::<(
        usize,
        &PlaylistClip,
        u16,
        PathBuf,
        f32,
        f32,
        f64,
        Option<i8>,
        MixerSourceRoutePlan,
    )>::new();
    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        check_cancelled(cancelled)?;
        if !audio_clip_matches_selection(options.clip_index, clip_index) {
            continue;
        }
        let relative_position =
            audio_clip_render_start_tick(clip.position_ticks, selection_start_tick)?;
        max_tick = max_tick.max(relative_position + u64::from(clip.length_ticks));
        if track_filter.clip_is_muted(clip) {
            continue;
        }
        let PlaylistClipTarget::Channel { id } = clip.target() else {
            continue;
        };
        let Some(channel) = channels.iter().find(|channel| channel.id() == id) else {
            continue;
        };
        if channel.kind() != Some(4)
            || channel.enabled() == Some(false)
            || !mixer_route_audibility.allows_channel(channel.mixer_track())
        {
            continue;
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            clips_skipped_unsupported_scale += 1;
            if options.clip_index == Some(clip_index) {
                return Err(format!(
                    "selected Playlist clip {clip_index} uses a stretch scale unsupported by this renderer"
                ));
            }
            continue;
        }
        let sample_path = channel.sample_path().ok_or_else(|| {
            format!("audio channel {id} has no decoded sample path (Playlist clip {clip_index})")
        })?;
        let resolved_path = resolver.resolve(sample_path).map_err(|error| {
            format!("could not resolve audio channel {id} for Playlist clip {clip_index}: {error}")
        })?;
        let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
        let mixer_track = channel.mixer_track().filter(|track| *track > 0);
        let mixer_route_plan =
            mixer_route_audibility.source_route_plan_for_channel(channel.mixer_track());
        candidate_clips.push((
            clip_index,
            clip,
            id,
            resolved_path,
            gain,
            pan,
            audio_clip_master_pitch_ratio(channel.master_pitch_offset_cents(master_pitch_cents)),
            mixer_track,
            mixer_route_plan,
        ));
    }
    if let Some(clip_index) = options.clip_index
        && candidate_clips.is_empty()
    {
        return Err(format!(
            "selected Playlist clip {clip_index} does not target an enabled audio channel"
        ));
    }
    let selection_start_tick = u64::from(selection_start_tick);
    let timeline_end_tick = selection_start_tick
        .checked_add(max_tick)
        .ok_or_else(|| "render timeline length overflow".to_owned())?;
    let timeline_frames = tempo_map.frames_between_ticks(
        selection_start_tick,
        timeline_end_tick,
        options.sample_rate,
    )?;
    let mut decoded_by_path = HashMap::<PathBuf, DecodedAudio>::new();
    let mut cached_source_bytes = 0usize;
    for (_, _, _, path, _, _, _, _, _) in &candidate_clips {
        check_cancelled(cancelled)?;
        if decoded_by_path.contains_key(path) {
            continue;
        }
        let audio = decode_audio_file(path)?;
        let bytes = decoded_audio_bytes(&audio)?;
        cached_source_bytes = cached_source_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_SOURCE_CACHE_BYTES)
            .ok_or_else(|| {
                format!(
                    "audio sources exceed the {} MiB render cache limit",
                    MAX_SOURCE_CACHE_BYTES / (1024 * 1024)
                )
            })?;
        decoded_by_path.insert(path.clone(), audio);
    }

    let mut clips = Vec::with_capacity(candidate_clips.len());
    let mut output_frames = timeline_frames;
    for (
        clip_index,
        clip,
        channel_id,
        path,
        gain,
        pan,
        pitch_ratio,
        mixer_track,
        mixer_route_plan,
    ) in &candidate_clips
    {
        check_cancelled(cancelled)?;
        let audio = decoded_by_path
            .get(path)
            .expect("all candidate source files were decoded");
        if audio.channels.is_empty() || audio.channels.len() > 2 {
            return Err(format!(
                "audio channel {channel_id} source has {} channels; only mono and stereo are supported by this renderer",
                audio.channels.len()
            ));
        }
        let source_bounds =
            sample_source_bounds_at_project_tempo(clip, audio, *clip_index, ppq, base_tempo_bpm)?;
        let absolute_start_tick = u64::from(clip.position_ticks);
        let absolute_end_tick = absolute_start_tick
            .checked_add(u64::from(clip.length_ticks))
            .ok_or_else(|| "render timeline length overflow".to_owned())?;
        let absolute_start_frame =
            tempo_map.frame_at_tick(absolute_start_tick, options.sample_rate)?;
        let selection_start_frame =
            tempo_map.frame_at_tick(selection_start_tick, options.sample_rate)?;
        let start_frame = absolute_start_frame
            .checked_sub(selection_start_frame)
            .ok_or_else(|| "selected clip precedes its selection start".to_owned())?;
        let source_duration_frames = source_duration_to_frames(
            source_bounds.end - source_bounds.start,
            audio.sample_rate,
            options.sample_rate,
            *pitch_ratio,
        )?;
        let timeline_clip_frames = tempo_map.frames_between_ticks(
            absolute_start_tick,
            absolute_end_tick,
            options.sample_rate,
        )?;
        let duration_frames = source_duration_frames.min(timeline_clip_frames);
        let end_frame = start_frame
            .checked_add(duration_frames)
            .ok_or_else(|| "render timeline length overflow".to_owned())?;
        output_frames = output_frames.max(end_frame);
        clips.push(PreparedClip {
            clip_index: *clip_index,
            channel_id: *channel_id,
            mixer_track: *mixer_track,
            path: path.clone(),
            start_frame,
            source_bounds,
            pitch_ratio: *pitch_ratio,
            duration_frames,
            gain: *gain,
            pan: *pan,
            pan_law,
            mixer_transform: mixer_route_plan.output_transform,
            mixer_meter_transforms: mixer_route_plan.insert_meter_transforms.clone(),
        });
    }

    Ok(PreparedAudioClipRender {
        summary: AudioClipRenderSummary {
            frames: output_frames,
            sample_rate: options.sample_rate,
            clips_rendered: clips.len(),
            clips_skipped_unsupported_scale,
            source_files: decoded_by_path.len(),
        },
        resampling_quality: options.resampling_quality,
        decoded_by_path,
        clips,
    })
}

fn require_audio_clips(render: &PreparedAudioClipRender) -> Result<(), String> {
    if render.clips.is_empty() {
        Err("arrangement contains no enabled Playlist clips targeting audio channels".to_owned())
    } else {
        Ok(())
    }
}

const MAX_SCHEDULED_PLAYLIST_NOTES: usize = 2_000_000;

/// Shifts an onset on an even-numbered sixteenth step by the combined global
/// and per-channel swing mix. The full mix places the onset one third of a
/// sixteenth step late, matching a triplet swing ratio.
pub(crate) fn swing_note_start_tick(
    tick: u64,
    ppq: u16,
    global_mix_raw: u8,
    channel_mix_raw: u16,
) -> Result<u64, String> {
    if ppq == 0 || global_mix_raw == 0 || channel_mix_raw == 0 {
        return Ok(tick);
    }
    if global_mix_raw > 128 || channel_mix_raw > 128 {
        return Ok(tick);
    }

    let ppq = u128::from(ppq);
    let scaled_tick = u128::from(tick) * 4;
    let step_index = (scaled_tick + ppq / 2) / ppq;
    let step_tick = (step_index * ppq + 2) / 4;
    if step_tick != u128::from(tick) || step_index % 2 == 0 {
        return Ok(tick);
    }

    let denominator = 4 * 128 * 128 * 3;
    let numerator = ppq * u128::from(global_mix_raw) * u128::from(channel_mix_raw);
    let delay_ticks = (numerator + denominator / 2) / denominator;
    let shifted_tick = u128::from(tick) + delay_ticks;
    u64::try_from(shifted_tick)
        .map_err(|_| "swing-adjusted note position exceeds the supported tick range".to_owned())
}

pub(crate) struct PlaylistPatternNote<'a> {
    pub(crate) note: &'a PatternNote,
    pub(crate) target_channel_id: u16,
    pub(crate) start_tick: u64,
    pub(crate) stop_tick: Option<u64>,
    pub(crate) clip_index: usize,
}

pub(crate) struct PlaylistPatternSchedule<'a> {
    pub(crate) notes: Vec<PlaylistPatternNote<'a>>,
    pub(crate) clips_skipped_unsupported_scale: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct PlaylistTrackFilter<'a> {
    disabled_track_ids: &'a BTreeSet<u32>,
    soloed_track_range: Option<(u32, u32)>,
}

impl<'a> PlaylistTrackFilter<'a> {
    pub(crate) const fn new(
        disabled_track_ids: &'a BTreeSet<u32>,
        soloed_track_range: Option<(u32, u32)>,
    ) -> Self {
        Self {
            disabled_track_ids,
            soloed_track_range,
        }
    }

    fn clip_is_muted(self, clip: &PlaylistClip) -> bool {
        if clip.is_muted() {
            return true;
        }
        let track_id = clip.playlist_track_id();
        if let Some((first_track, last_track)) = self.soloed_track_range {
            return !track_id
                .is_some_and(|track_id| (first_track..=last_track).contains(&track_id));
        }
        track_id.is_some_and(|track_id| self.disabled_track_ids.contains(&track_id))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PlaylistPatternScheduleOptions {
    pub(crate) ppq: u16,
    pub(crate) global_swing_mix_raw: u8,
    pub(crate) play_truncated_notes_in_clips: bool,
}

pub(crate) fn schedule_playlist_pattern_notes<'a>(
    patterns: &'a [Pattern],
    arrangement: &Arrangement,
    track_filter: PlaylistTrackFilter<'_>,
    options: PlaylistPatternScheduleOptions,
    mut channel_swing_mix_raw: impl FnMut(u16) -> u16,
    mut resolve_targets: impl FnMut(u16, u64, u16) -> Vec<u16>,
) -> Result<PlaylistPatternSchedule<'a>, String> {
    let patterns_by_id: HashMap<_, _> = patterns
        .iter()
        .map(|pattern| (pattern.id, pattern))
        .collect();
    let mut schedule = PlaylistPatternSchedule {
        notes: Vec::new(),
        clips_skipped_unsupported_scale: 0,
    };

    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        if track_filter.clip_is_muted(clip) {
            continue;
        }
        let PlaylistClipTarget::Pattern { id } = clip.target() else {
            continue;
        };
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            schedule.clips_skipped_unsupported_scale += 1;
            continue;
        }
        let Some(pattern) = patterns_by_id.get(&id).copied() else {
            continue;
        };
        let clip_length = u64::from(clip.length_ticks);
        if clip_length == 0 {
            continue;
        }
        let inferred_length = pattern
            .notes
            .iter()
            .filter(|note| note.length > 0)
            .try_fold(0u64, |end, note| {
                let note_end = u64::from(note.position)
                    .checked_add(u64::from(note.length))
                    .ok_or_else(|| "pattern note end position overflow".to_owned())?;
                Ok::<_, String>(end.max(note_end))
            })?;
        let loop_length = pattern
            .length_ticks
            .map(u64::from)
            .filter(|length| *length > 0)
            .unwrap_or(if inferred_length > 0 {
                inferred_length
            } else {
                clip_length
            });
        if loop_length == 0 {
            continue;
        }
        let clip_end = u64::from(clip.position_ticks)
            .checked_add(clip_length)
            .ok_or_else(|| "Playlist pattern clip end position overflow".to_owned())?;
        let repetitions = clip_length.div_ceil(loop_length);
        for repetition in 0..repetitions {
            let repeat_start = repetition
                .checked_mul(loop_length)
                .ok_or_else(|| "pattern repeat position overflow".to_owned())?;
            for (note_index, note) in pattern.notes.iter().enumerate() {
                if note.is_muted() {
                    continue;
                }
                let relative_start = repeat_start
                    .checked_add(u64::from(note.position))
                    .ok_or_else(|| "repeated pattern note position overflow".to_owned())?;
                if relative_start >= clip_length {
                    continue;
                }
                let nominal_start_tick = u64::from(clip.position_ticks)
                    .checked_add(relative_start)
                    .ok_or_else(|| "Playlist pattern note position overflow".to_owned())?;
                let swung_relative_start = swing_note_start_tick(
                    relative_start,
                    options.ppq,
                    options.global_swing_mix_raw,
                    channel_swing_mix_raw(note.channel_id),
                )?;
                let swing_offset = swung_relative_start.saturating_sub(relative_start);
                let start_tick = u64::from(clip.position_ticks)
                    .checked_add(swung_relative_start)
                    .ok_or_else(|| "swung Playlist pattern note position overflow".to_owned())?;
                if start_tick >= clip_end {
                    continue;
                }
                let stop_tick = if note.length == 0 {
                    None
                } else {
                    let note_end_tick = nominal_start_tick
                        .checked_add(u64::from(note.length))
                        .and_then(|stop_tick| stop_tick.checked_add(swing_offset))
                        .ok_or_else(|| "Playlist pattern note end position overflow".to_owned())?;
                    Some(if options.play_truncated_notes_in_clips {
                        note_end_tick
                    } else {
                        note_end_tick.min(clip_end)
                    })
                };
                let note_seed = (clip_index as u64).rotate_left(32)
                    ^ repetition.rotate_left(17)
                    ^ (note_index as u64).rotate_left(3)
                    ^ nominal_start_tick.rotate_left(47);
                for target_channel_id in resolve_targets(note.channel_id, note_seed, note.key) {
                    if schedule.notes.len() >= MAX_SCHEDULED_PLAYLIST_NOTES {
                        return Err(format!(
                            "Playlist expands to more than {MAX_SCHEDULED_PLAYLIST_NOTES} pattern note events"
                        ));
                    }
                    schedule.notes.push(PlaylistPatternNote {
                        note,
                        target_channel_id,
                        start_tick,
                        stop_tick,
                        clip_index,
                    });
                }
            }
        }
    }
    schedule
        .notes
        .sort_by_key(|placed| (placed.start_tick, placed.clip_index));
    Ok(schedule)
}

struct PreparedSamplerArrangement {
    summary: SamplerPatternRenderSummary,
    pan_law: PanLaw,
    notes: Vec<ScheduledSamplerNote>,
    sources_by_channel: HashMap<u16, SamplerVoiceSource>,
    mixer_transforms_by_channel: HashMap<u16, MixerSourceRouteTransform>,
    mixer_meter_transforms_by_channel: HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>,
    pattern_clips_rendered: usize,
    pattern_clips_skipped_unsupported_scale: usize,
    source_paths: std::collections::BTreeSet<PathBuf>,
}

fn prepare_sampler_arrangement(
    document: &FlpDocument,
    project_path: &Path,
    options: PlaylistRenderOptions,
    cancelled: Option<&AtomicBool>,
) -> Result<PreparedSamplerArrangement, String> {
    check_cancelled(cancelled)?;
    if !(8_000..=384_000).contains(&options.sample_rate) {
        return Err("Playlist sample rate must be between 8000 and 384000 Hz".to_owned());
    }
    if !(1..=MAX_SAMPLER_VOICE_LIMIT).contains(&options.sampler_voice_limit) {
        return Err(format!(
            "Sampler voice limit must be between 1 and {MAX_SAMPLER_VOICE_LIMIT}"
        ));
    }
    let ppq = document.header().ppq();
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    let pan_law = project_pan_law(document);
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == options.arrangement_id)
        .ok_or_else(|| format!("arrangement {} was not found", options.arrangement_id))?;
    let tempo_map = PlaylistTempoMap::for_arrangement(document, options.arrangement_id)?;
    let disabled_track_ids = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    let track_filter = PlaylistTrackFilter {
        disabled_track_ids: &disabled_track_ids,
        soloed_track_range: options.soloed_playlist_track_range,
    };
    let patterns = document.patterns().map_err(|error| error.to_string())?;
    let channels = document.channels();
    let mixer_route_audibility = MixerRouteAudibility::from_document(document);
    let channels_by_id: HashMap<_, _> = channels
        .iter()
        .map(|channel| (channel.id(), channel))
        .collect();
    let channel_router = ChannelNoteRouter::new(channels.clone());
    let schedule = schedule_playlist_pattern_notes(
        &patterns,
        &arrangement,
        track_filter,
        PlaylistPatternScheduleOptions {
            ppq,
            global_swing_mix_raw: document.metadata().global_swing_mix(),
            play_truncated_notes_in_clips: document
                .project_settings()
                .is_none_or(|settings| settings.play_truncated_notes_in_clips),
        },
        |channel_id| {
            channels_by_id
                .get(&channel_id)
                .map_or(128, |channel| channel.swing_mix())
        },
        |channel_id, seed, key| {
            channel_router
                .targets_for_note(channel_id, seed, key)
                .into_iter()
                .filter(|target_channel_id| {
                    channels_by_id
                        .get(target_channel_id)
                        .is_some_and(|channel| {
                            channel.kind() == Some(0)
                                && channel.enabled() != Some(false)
                                && mixer_route_audibility.allows_channel(channel.mixer_track())
                        })
                })
                .collect()
        },
    )?;
    let sampler_note_count = schedule.notes.len();
    if sampler_note_count == 0 {
        return Ok(PreparedSamplerArrangement {
            summary: SamplerPatternRenderSummary {
                frames: 0,
                sample_rate: options.sample_rate,
                notes_rendered: 0,
                voices_stolen: 0,
                sampler_channels_rendered: 0,
                source_files: 0,
                notes_skipped_unresolved_sample: 0,
                unresolved_sample_channels: Vec::new(),
            },
            pan_law,
            notes: Vec::new(),
            sources_by_channel: HashMap::new(),
            mixer_transforms_by_channel: HashMap::new(),
            mixer_meter_transforms_by_channel: HashMap::new(),
            pattern_clips_rendered: 0,
            pattern_clips_skipped_unsupported_scale: schedule.clips_skipped_unsupported_scale,
            source_paths: std::collections::BTreeSet::new(),
        });
    }

    let resolver = SamplePathResolver::new(project_path);
    let mut decoded_by_path = HashMap::<PathBuf, Arc<DecodedAudio>>::new();
    let mut sources_by_channel = HashMap::<u16, SamplerVoiceSource>::new();
    let mut mixer_transforms_by_channel = HashMap::<u16, MixerSourceRouteTransform>::new();
    let mut mixer_meter_transforms_by_channel =
        HashMap::<u16, Vec<(usize, MixerSourceRouteTransform)>>::new();
    let mut unresolved_sample_channels = std::collections::BTreeSet::new();
    let mut notes = Vec::with_capacity(sampler_note_count);
    let mut rendered_pattern_clip_indices = std::collections::BTreeSet::new();
    let mut skipped_unresolved = 0usize;
    let mut cached_source_bytes = 0usize;
    let mut output_frames = 0u64;
    let release_frames = (f64::from(options.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as u64;

    for placed in &schedule.notes {
        check_cancelled(cancelled)?;
        let note = placed.note;
        let Some(channel) = channels_by_id.get(&placed.target_channel_id) else {
            continue;
        };
        if channel.kind() != Some(0) || channel.enabled() == Some(false) {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(source_entry) =
            sources_by_channel.entry(placed.target_channel_id)
        {
            let Some(sample_path) = channel.sample_path() else {
                unresolved_sample_channels.insert(placed.target_channel_id);
                skipped_unresolved += 1;
                continue;
            };
            let resolved_path = match resolver.resolve(sample_path) {
                Ok(path) => path,
                Err(_) => {
                    unresolved_sample_channels.insert(placed.target_channel_id);
                    skipped_unresolved += 1;
                    continue;
                }
            };
            let wave_metadata = wav_sampler_metadata(&resolved_path).unwrap_or_default();
            let loop_points = channel
                .sampler_uses_loop_points()
                .then_some(wave_metadata.loop_points)
                .flatten();
            let audio = if let Some(audio) = decoded_by_path.get(&resolved_path) {
                Arc::clone(audio)
            } else {
                let audio = decode_audio_file(&resolved_path).map_err(|error| {
                    format!(
                        "could not decode sample for Sampler channel {} at {}: {error}",
                        placed.target_channel_id,
                        resolved_path.display()
                    )
                })?;
                if !(1..=2).contains(&audio.channels.len()) {
                    return Err(format!(
                        "Sampler channel {} uses a {}-channel sample; only mono and stereo are supported by this renderer",
                        placed.target_channel_id,
                        audio.channels.len()
                    ));
                }
                let bytes = decoded_audio_bytes(&audio)?;
                cached_source_bytes = cached_source_bytes
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_SOURCE_CACHE_BYTES)
                    .ok_or_else(|| {
                        format!(
                            "sampler sources exceed the {} MiB render cache limit",
                            MAX_SOURCE_CACHE_BYTES / (1024 * 1024)
                        )
                    })?;
                let audio = Arc::new(audio);
                decoded_by_path.insert(resolved_path, Arc::clone(&audio));
                audio
            };
            let loop_bounds = validated_sampler_loop_bounds(loop_points, audio.frame_count());
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            source_entry.insert(SamplerVoiceSource {
                audio,
                gain: gain * sampler_wav_gain(wave_metadata.gain_db),
                pan,
                reverse: channel.sample_reversed(),
                root_key: resolve_sampler_channel_root_key(
                    channel,
                    wave_metadata.root_key,
                    options.read_sample_root_note,
                ),
                fine_tune_cents: wave_metadata.fine_tune_cents.unwrap_or_default()
                    + f64::from(channel.master_pitch_render_offset_cents(
                        document.metadata().master_pitch_cents(),
                    )),
                loop_bounds,
                ping_pong_loop: channel.sampler_ping_pong_loop_enabled(),
            });
            let route_plan =
                mixer_route_audibility.source_route_plan_for_channel(channel.mixer_track());
            mixer_transforms_by_channel
                .insert(placed.target_channel_id, route_plan.output_transform);
            mixer_meter_transforms_by_channel
                .insert(placed.target_channel_id, route_plan.insert_meter_transforms);
        }
        let Some(source) = sources_by_channel.get(&placed.target_channel_id) else {
            continue;
        };
        let note_key = transpose_sampler_note_key(
            note.key,
            channels_by_id
                .get(&placed.target_channel_id)
                .map_or(0, |channel| {
                    sampler_note_pitch_offset_semitones(
                        channel,
                        source.root_key,
                        document.metadata().master_pitch_cents(),
                    )
                }),
        );
        if source.audio.frame_count() == 0 {
            return Err(format!(
                "Sampler channel {} references an empty sample",
                placed.target_channel_id
            ));
        }
        let start_frame = tempo_map.frame_at_tick(placed.start_tick, options.sample_rate)?;
        let stop_frame = placed
            .stop_tick
            .map(|stop_tick| {
                tempo_map
                    .frame_at_tick(stop_tick, options.sample_rate)
                    .map(|frame| frame.max(start_frame.saturating_add(1)))
            })
            .transpose()?;
        let natural_duration = (source.audio.frame_count() as f64
            / sampler_source_step(
                source.audio.sample_rate,
                options.sample_rate,
                note_key,
                source.root_key,
                source.fine_tune_cents + f64::from(note.fine_pitch_offset_cents()),
            ))
        .ceil();
        if !natural_duration.is_finite()
            || natural_duration < 1.0
            || natural_duration > u64::MAX as f64
        {
            return Err("Sampler sample duration is outside the renderable range".to_owned());
        }
        let voice_end_frame = match stop_frame {
            Some(stop_frame) => stop_frame,
            None => start_frame
                .checked_add(natural_duration as u64)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
        };
        let tail_frames = if stop_frame.is_some() {
            release_frames
        } else {
            0
        };
        output_frames = output_frames.max(
            voice_end_frame
                .checked_add(tail_frames)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
        );
        rendered_pattern_clip_indices.insert(placed.clip_index);
        notes.push(ScheduledSamplerNote {
            start_frame,
            stop_frame,
            channel_id: placed.target_channel_id,
            key: note_key,
            velocity: note.velocity,
            pan: note.pan,
            fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
        });
    }
    notes.sort_by_key(|note| (note.start_frame, note.channel_id, note.key));
    Ok(PreparedSamplerArrangement {
        summary: SamplerPatternRenderSummary {
            frames: output_frames,
            sample_rate: options.sample_rate,
            notes_rendered: notes.len(),
            voices_stolen: 0,
            sampler_channels_rendered: sources_by_channel.len(),
            source_files: decoded_by_path.len(),
            notes_skipped_unresolved_sample: skipped_unresolved,
            unresolved_sample_channels: unresolved_sample_channels.into_iter().collect(),
        },
        pan_law,
        notes,
        sources_by_channel,
        mixer_transforms_by_channel,
        mixer_meter_transforms_by_channel,
        pattern_clips_rendered: rendered_pattern_clip_indices.len(),
        pattern_clips_skipped_unsupported_scale: schedule.clips_skipped_unsupported_scale,
        source_paths: decoded_by_path.keys().cloned().collect(),
    })
}

struct PreparedSamplerPattern {
    summary: SamplerPatternRenderSummary,
    pan_law: PanLaw,
    notes: Vec<ScheduledSamplerNote>,
    sources_by_channel: HashMap<u16, SamplerVoiceSource>,
    mixer_transforms_by_channel: HashMap<u16, MixerSourceRouteTransform>,
    master_output: MixerMasterOutput,
    voice_limit: usize,
    resampling_quality: ResamplingQuality,
}

#[derive(Clone)]
struct SamplerVoiceSource {
    audio: Arc<DecodedAudio>,
    gain: f32,
    pan: f32,
    reverse: bool,
    root_key: u16,
    fine_tune_cents: f64,
    loop_bounds: Option<SampleBounds>,
    ping_pong_loop: bool,
}

fn validated_sampler_loop_bounds(
    loop_points: Option<(usize, usize)>,
    frame_count: usize,
) -> Option<SampleBounds> {
    let (start, end) = loop_points?;
    (start < end && end <= frame_count).then_some(SampleBounds { start, end })
}

#[derive(Clone, Copy)]
struct ScheduledSamplerNote {
    start_frame: u64,
    stop_frame: Option<u64>,
    channel_id: u16,
    key: u16,
    velocity: u8,
    pan: u8,
    fine_pitch_offset_cents: i16,
}

fn prepare_sampler_pattern(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    cancelled: Option<&AtomicBool>,
) -> Result<PreparedSamplerPattern, String> {
    prepare_sampler_pattern_for_channel(document, project_path, options, cancelled, None)
}

fn prepare_sampler_pattern_for_channel(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    cancelled: Option<&AtomicBool>,
    channel_filter: Option<u16>,
) -> Result<PreparedSamplerPattern, String> {
    check_cancelled(cancelled)?;
    if !(8_000..=384_000).contains(&options.sample_rate) {
        return Err("sampler render rate must be between 8000 and 384000 Hz".to_owned());
    }
    if !(1..=MAX_SAMPLER_VOICE_LIMIT).contains(&options.voice_limit) {
        return Err(format!(
            "Sampler voice limit must be between 1 and {MAX_SAMPLER_VOICE_LIMIT}"
        ));
    }
    let ppq = document.header().ppq();
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    let pattern = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pattern| pattern.id == options.pattern_id)
        .ok_or_else(|| format!("pattern {} was not found", options.pattern_id))?;
    let pan_law = project_pan_law(document);
    let global_swing_mix_raw = document.metadata().global_swing_mix();
    let channels = document.channels();
    let mixer_route_audibility = MixerRouteAudibility::from_document(document);
    let channels_by_id: HashMap<_, _> = channels
        .iter()
        .map(|channel| (channel.id(), channel))
        .collect();
    let channel_router = ChannelNoteRouter::new(channels.clone());
    let sampler_notes = pattern
        .notes
        .iter()
        .enumerate()
        .filter(|(_, note)| !note.is_muted())
        .flat_map(|(note_index, note)| {
            channel_router
                .targets_for_note(note.channel_id, note_index as u64, note.key)
                .into_iter()
                .filter(|target_channel_id| {
                    channels_by_id
                        .get(target_channel_id)
                        .is_some_and(|channel| {
                            channel.kind() == Some(0)
                                && channel.enabled() != Some(false)
                                && mixer_route_audibility.allows_channel(channel.mixer_track())
                        })
                })
                .map(move |target_channel_id| (note, target_channel_id))
        })
        .filter(|(_, target_channel_id)| {
            channel_filter.is_none_or(|channel_id| *target_channel_id == channel_id)
        })
        .collect::<Vec<_>>();
    let sampler_note_count = sampler_notes.len();
    if sampler_note_count == 0 {
        return Err(format!(
            "pattern {} contains no notes on enabled Sampler channels",
            options.pattern_id
        ));
    }

    let resolver = SamplePathResolver::new(project_path);
    let mut decoded_by_path = HashMap::<PathBuf, Arc<DecodedAudio>>::new();
    let mut sources_by_channel = HashMap::<u16, SamplerVoiceSource>::new();
    let mut mixer_transforms_by_channel = HashMap::<u16, MixerSourceRouteTransform>::new();
    let mut unresolved_sample_channels = std::collections::BTreeSet::new();
    let mut notes = Vec::with_capacity(sampler_note_count);
    let mut skipped_unresolved = 0usize;
    let mut cached_source_bytes = 0usize;
    let mut output_frames = 0u64;
    let release_frames = (f64::from(options.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as u64;

    for (note, target_channel_id) in sampler_notes {
        check_cancelled(cancelled)?;
        let Some(channel) = channels_by_id.get(&target_channel_id) else {
            continue;
        };
        if channel.kind() != Some(0) || channel.enabled() == Some(false) {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(source_entry) =
            sources_by_channel.entry(target_channel_id)
        {
            let Some(sample_path) = channel.sample_path() else {
                unresolved_sample_channels.insert(target_channel_id);
                skipped_unresolved += 1;
                continue;
            };
            let resolved_path = match resolver.resolve(sample_path) {
                Ok(path) => path,
                Err(_) => {
                    unresolved_sample_channels.insert(target_channel_id);
                    skipped_unresolved += 1;
                    continue;
                }
            };
            let wave_metadata = wav_sampler_metadata(&resolved_path).unwrap_or_default();
            let loop_points = channel
                .sampler_uses_loop_points()
                .then_some(wave_metadata.loop_points)
                .flatten();
            let audio = if let Some(audio) = decoded_by_path.get(&resolved_path) {
                Arc::clone(audio)
            } else {
                let audio = decode_audio_file(&resolved_path).map_err(|error| {
                    format!(
                        "could not decode sample for Sampler channel {} at {}: {error}",
                        target_channel_id,
                        resolved_path.display()
                    )
                })?;
                if !(1..=2).contains(&audio.channels.len()) {
                    return Err(format!(
                        "Sampler channel {} uses a {}-channel sample; only mono and stereo are supported by this renderer",
                        target_channel_id,
                        audio.channels.len()
                    ));
                }
                let bytes = decoded_audio_bytes(&audio)?;
                cached_source_bytes = cached_source_bytes
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_SOURCE_CACHE_BYTES)
                    .ok_or_else(|| {
                        format!(
                            "sampler sources exceed the {} MiB render cache limit",
                            MAX_SOURCE_CACHE_BYTES / (1024 * 1024)
                        )
                    })?;
                let audio = Arc::new(audio);
                decoded_by_path.insert(resolved_path, Arc::clone(&audio));
                audio
            };
            let loop_bounds = validated_sampler_loop_bounds(loop_points, audio.frame_count());
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            source_entry.insert(SamplerVoiceSource {
                audio,
                gain: gain * sampler_wav_gain(wave_metadata.gain_db),
                pan,
                reverse: channel.sample_reversed(),
                root_key: resolve_sampler_channel_root_key(
                    channel,
                    wave_metadata.root_key,
                    options.read_sample_root_note,
                ),
                fine_tune_cents: wave_metadata.fine_tune_cents.unwrap_or_default()
                    + f64::from(channel.master_pitch_render_offset_cents(
                        document.metadata().master_pitch_cents(),
                    )),
                loop_bounds,
                ping_pong_loop: channel.sampler_ping_pong_loop_enabled(),
            });
            mixer_transforms_by_channel.insert(
                target_channel_id,
                mixer_route_audibility.source_route_transform_for_channel(channel.mixer_track()),
            );
        }
        let Some(source) = sources_by_channel.get(&target_channel_id) else {
            continue;
        };
        let note_key = transpose_sampler_note_key(
            note.key,
            channels_by_id.get(&target_channel_id).map_or(0, |channel| {
                sampler_note_pitch_offset_semitones(
                    channel,
                    source.root_key,
                    document.metadata().master_pitch_cents(),
                )
            }),
        );
        let channel_swing_mix_raw = channels_by_id
            .get(&note.channel_id)
            .map_or(128, |channel| channel.swing_mix());
        let nominal_start_tick = u64::from(note.position);
        let swung_start_tick = swing_note_start_tick(
            nominal_start_tick,
            ppq,
            global_swing_mix_raw,
            channel_swing_mix_raw,
        )?;
        let swing_offset = swung_start_tick.saturating_sub(nominal_start_tick);
        let start_frame = ticks_to_frames(swung_start_tick, ppq, tempo_bpm, options.sample_rate)?;
        // Reading the source here also validates that each prepared channel has usable frames.
        if source.audio.frame_count() == 0 {
            return Err(format!(
                "Sampler channel {} references an empty sample",
                target_channel_id
            ));
        }
        let stop_frame = if note.length == 0 {
            None
        } else {
            let end_tick = nominal_start_tick
                .checked_add(u64::from(note.length))
                .and_then(|tick| tick.checked_add(swing_offset))
                .ok_or_else(|| "Sampler note end position overflow".to_owned())?;
            Some(
                ticks_to_frames(end_tick, ppq, tempo_bpm, options.sample_rate)?
                    .max(start_frame.saturating_add(1)),
            )
        };
        let natural_duration = (source.audio.frame_count() as f64
            / sampler_source_step(
                source.audio.sample_rate,
                options.sample_rate,
                note_key,
                source.root_key,
                source.fine_tune_cents + f64::from(note.fine_pitch_offset_cents()),
            ))
        .ceil();
        if !natural_duration.is_finite()
            || natural_duration < 1.0
            || natural_duration > u64::MAX as f64
        {
            return Err("Sampler sample duration is outside the renderable range".to_owned());
        }
        let voice_end_frame = match stop_frame {
            Some(stop_frame) => stop_frame,
            None => start_frame
                .checked_add(natural_duration as u64)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
        };
        let tail_frames = if stop_frame.is_some() {
            release_frames
        } else {
            0
        };
        output_frames = output_frames.max(
            voice_end_frame
                .checked_add(tail_frames)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
        );
        notes.push(ScheduledSamplerNote {
            start_frame,
            stop_frame,
            channel_id: target_channel_id,
            key: note_key,
            velocity: note.velocity,
            pan: note.pan,
            fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
        });
    }

    if notes.is_empty() {
        let missing = unresolved_sample_channels
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "pattern {} has sampler notes, but no referenced sample could be resolved (channel IDs: {missing})",
            options.pattern_id
        ));
    }
    notes.sort_by_key(|note| (note.start_frame, note.channel_id, note.key));
    Ok(PreparedSamplerPattern {
        summary: SamplerPatternRenderSummary {
            frames: output_frames,
            sample_rate: options.sample_rate,
            notes_rendered: notes.len(),
            voices_stolen: 0,
            sampler_channels_rendered: sources_by_channel.len(),
            source_files: decoded_by_path.len(),
            notes_skipped_unresolved_sample: skipped_unresolved,
            unresolved_sample_channels: unresolved_sample_channels.into_iter().collect(),
        },
        pan_law,
        notes,
        sources_by_channel,
        mixer_transforms_by_channel,
        master_output: MixerMasterOutput::from_document(document),
        voice_limit: options.voice_limit,
        resampling_quality: options.resampling_quality,
    })
}

struct SamplerVoiceEngine<'a> {
    sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
    notes: &'a [ScheduledSamplerNote],
    mixer_transforms_by_channel: Option<&'a HashMap<u16, MixerSourceRouteTransform>>,
    mixer_meter_transforms_by_channel:
        Option<&'a HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>>,
    pan_law: PanLaw,
    next_note: usize,
    voices: Vec<Option<SamplerVoice>>,
    release_frames: usize,
    output_sample_rate: u32,
    resampling_quality: ResamplingQuality,
    voices_stolen: usize,
    pitch_bend_cents: [i16; 16],
    sustain_down: [bool; 16],
}

struct SamplerVoice {
    source: Arc<DecodedAudio>,
    channel_id: u16,
    key: u16,
    input_midi_channel: Option<u8>,
    source_position: f64,
    source_step: f64,
    base_source_step: f64,
    key_released: bool,
    loop_bounds: Option<SampleBounds>,
    ping_pong_loop: bool,
    stop_frame: Option<u64>,
    started_frame: u64,
    gain: f32,
    left_gain: f32,
    right_gain: f32,
    mixer_transform: MixerSourceRouteTransform,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    release_remaining: Option<usize>,
}

impl<'a> SamplerVoiceEngine<'a> {
    #[cfg(test)]
    fn new(
        sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
        notes: &'a [ScheduledSamplerNote],
        voice_limit: usize,
        release_frames: usize,
        output_sample_rate: u32,
    ) -> Self {
        Self::with_resampling_quality_and_pan_law(
            sources_by_channel,
            notes,
            voice_limit,
            release_frames,
            output_sample_rate,
            ResamplingQuality::Linear,
            PanLaw::Circular,
        )
    }

    fn with_resampling_quality_and_pan_law(
        sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
        notes: &'a [ScheduledSamplerNote],
        voice_limit: usize,
        release_frames: usize,
        output_sample_rate: u32,
        resampling_quality: ResamplingQuality,
        pan_law: PanLaw,
    ) -> Self {
        Self {
            sources_by_channel,
            notes,
            mixer_transforms_by_channel: None,
            mixer_meter_transforms_by_channel: None,
            pan_law,
            next_note: 0,
            voices: std::iter::repeat_with(|| None).take(voice_limit).collect(),
            release_frames,
            output_sample_rate,
            resampling_quality,
            voices_stolen: 0,
            pitch_bend_cents: [0; 16],
            sustain_down: [false; 16],
        }
    }

    fn render_block(&mut self, block_start: u64, frame_count: usize, output: &mut [f32]) {
        self.render_block_with_meters(block_start, frame_count, output, None, 0);
    }

    fn render_block_with_meters(
        &mut self,
        block_start: u64,
        frame_count: usize,
        output: &mut [f32],
        mut meter_buses: Option<&mut [f32]>,
        meter_stride_samples: usize,
    ) {
        output.fill(0.0);
        for offset in 0..frame_count {
            let frame = block_start + offset as u64;
            while self
                .notes
                .get(self.next_note)
                .is_some_and(|note| note.start_frame <= frame)
            {
                let note = self.notes[self.next_note];
                self.next_note += 1;
                let _ = self.start_voice(note);
            }
            let output_frame = &mut output[offset * 2..offset * 2 + 2];
            for voice_slot in &mut self.voices {
                let Some(voice) = voice_slot.as_mut() else {
                    continue;
                };
                if voice.release_remaining.is_none()
                    && voice
                        .stop_frame
                        .is_some_and(|stop_frame| frame >= stop_frame)
                {
                    voice.release_remaining = Some(self.release_frames);
                }
                let fade = match voice.release_remaining {
                    Some(0) => {
                        *voice_slot = None;
                        continue;
                    }
                    Some(remaining) => remaining as f32 / self.release_frames as f32,
                    None => 1.0,
                };
                let source_frames = voice.source.frame_count();
                if voice.source_position < 0.0 || voice.source_position >= source_frames as f64 {
                    *voice_slot = None;
                    continue;
                }
                let left = resample_sample(
                    &voice.source.channels[0],
                    voice.source_position,
                    voice.source_step,
                    self.resampling_quality,
                );
                let right = if voice.source.channels.len() == 1 {
                    left
                } else {
                    resample_sample(
                        &voice.source.channels[1],
                        voice.source_position,
                        voice.source_step,
                        self.resampling_quality,
                    )
                };
                let gain = voice.gain * fade;
                let mut routed_frame = [
                    left * gain * voice.left_gain,
                    right * gain * voice.right_gain,
                ];
                voice.mixer_transform.apply_frame(&mut routed_frame);
                output_frame[0] += routed_frame[0];
                output_frame[1] += routed_frame[1];
                if let Some(meter_buses) = meter_buses.as_deref_mut() {
                    let source_frame = [
                        left * gain * voice.left_gain,
                        right * gain * voice.right_gain,
                    ];
                    for (track, transform) in &voice.mixer_meter_transforms {
                        debug_assert!(*track < MIXER_TRACK_METER_COUNT);
                        let mut meter_frame = source_frame;
                        transform.apply_frame(&mut meter_frame);
                        let meter_offset = *track * meter_stride_samples + offset * 2;
                        meter_buses[meter_offset] += meter_frame[0];
                        meter_buses[meter_offset + 1] += meter_frame[1];
                    }
                }
                advance_sampler_voice_position(voice);
                if let Some(remaining) = voice.release_remaining.as_mut() {
                    *remaining = remaining.saturating_sub(1);
                    if *remaining == 0
                        || voice.source_position < 0.0
                        || voice.source_position >= source_frames as f64
                    {
                        *voice_slot = None;
                    }
                } else if voice.source_position < 0.0
                    || voice.source_position >= source_frames as f64
                {
                    *voice_slot = None;
                }
            }
        }
    }

    fn start_voice(&mut self, note: ScheduledSamplerNote) -> Option<usize> {
        let source = self.sources_by_channel.get(&note.channel_id)?;
        let slot_index = self
            .voices
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .filter_map(|(index, voice)| {
                        voice.as_ref().map(|voice| (index, voice.started_frame))
                    })
                    .min_by_key(|(_, started_frame)| *started_frame)
                    .map(|(index, _)| index)
                    .expect("voice pool has a positive capacity")
            });
        if self.voices[slot_index].is_some() {
            self.voices_stolen += 1;
        }
        let source_step = sampler_source_step(
            source.audio.sample_rate,
            self.output_sample_rate,
            note.key,
            source.root_key,
            source.fine_tune_cents + f64::from(note.fine_pitch_offset_cents),
        );
        let source_step = if source.reverse {
            -source_step
        } else {
            source_step
        };
        let effective_pan = sampler_note_pan(source.pan, note.pan);
        let (left_gain, right_gain) = channel_pan_gains(
            effective_pan,
            source.audio.channels.len() == 1,
            self.pan_law,
        );
        let velocity_gain = f32::from(note.velocity.min(127)) / 127.0;
        let mixer_transform = self
            .mixer_transforms_by_channel
            .and_then(|transforms| transforms.get(&note.channel_id).copied())
            .unwrap_or_default();
        let mixer_meter_transforms = self
            .mixer_meter_transforms_by_channel
            .and_then(|transforms| transforms.get(&note.channel_id))
            .cloned()
            .unwrap_or_default();
        self.voices[slot_index] = Some(SamplerVoice {
            source: Arc::clone(&source.audio),
            channel_id: note.channel_id,
            key: note.key,
            input_midi_channel: None,
            source_position: if source.reverse {
                source.audio.frame_count().saturating_sub(1) as f64
            } else {
                0.0
            },
            source_step,
            base_source_step: source_step,
            key_released: false,
            loop_bounds: source.loop_bounds,
            ping_pong_loop: source.ping_pong_loop,
            stop_frame: note.stop_frame,
            started_frame: note.start_frame,
            gain: source.gain * velocity_gain,
            left_gain,
            right_gain,
            mixer_transform,
            mixer_meter_transforms,
            release_remaining: None,
        });
        Some(slot_index)
    }

    fn set_voice_input_midi_channel(&mut self, slot_index: usize, channel: u8) {
        let channel = channel & 0x0F;
        let pitch_bend_cents = self.pitch_bend_cents[usize::from(channel)];
        if let Some(voice) = self.voices[slot_index].as_mut() {
            voice.input_midi_channel = Some(channel);
            voice.source_step =
                voice.base_source_step * audio_clip_master_pitch_ratio(pitch_bend_cents);
        }
    }

    fn apply_live_pitch_bend(&mut self, channel: u8, cents: i16) {
        let channel = channel & 0x0F;
        let cents = cents.clamp(-1200, 1200);
        self.pitch_bend_cents[usize::from(channel)] = cents;
        let ratio = audio_clip_master_pitch_ratio(cents);
        for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
            if voice.input_midi_channel == Some(channel) {
                voice.source_step = voice.base_source_step * ratio;
            }
        }
    }

    fn set_live_sustain(&mut self, channel: u8, down: bool) {
        let channel = channel & 0x0F;
        let channel_index = usize::from(channel);
        let was_down = self.sustain_down[channel_index];
        self.sustain_down[channel_index] = down;
        if was_down && !down {
            for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
                if voice.input_midi_channel == Some(channel)
                    && voice.key_released
                    && voice.release_remaining.is_none()
                {
                    voice.release_remaining = Some(self.release_frames);
                }
            }
        }
    }

    fn release_note(&mut self, channel_id: u16, key: u16, midi_channel: u8) {
        let midi_channel = midi_channel & 0x0F;
        let sustain_down = self.sustain_down[usize::from(midi_channel)];
        let voice = self
            .voices
            .iter_mut()
            .filter_map(Option::as_mut)
            .filter(|voice| {
                voice.channel_id == channel_id
                    && voice.key == key
                    && voice.input_midi_channel == Some(midi_channel)
                    && !voice.key_released
                    && voice.release_remaining.is_none()
            })
            .min_by_key(|voice| voice.started_frame);
        if let Some(voice) = voice {
            voice.key_released = true;
            if !sustain_down {
                voice.release_remaining = Some(self.release_frames);
            }
        }
    }
}

fn advance_sampler_voice_position(voice: &mut SamplerVoice) {
    let mut next_position = voice.source_position + voice.source_step;
    let Some(bounds) = voice.loop_bounds else {
        voice.source_position = next_position;
        return;
    };
    if voice.stop_frame.is_none() || voice.release_remaining.is_some() {
        voice.source_position = next_position;
        return;
    }

    let start = bounds.start as f64;
    let end = bounds.end as f64;
    if !voice.ping_pong_loop {
        let loop_length = end - start;
        if (voice.source_step > 0.0 && next_position >= end)
            || (voice.source_step < 0.0 && next_position < start)
        {
            next_position = start + (next_position - start).rem_euclid(loop_length);
        }
        voice.source_position = next_position;
        return;
    }

    let low = start;
    let high = end - 1.0;
    let step_magnitude = voice.source_step.abs();
    if (voice.source_step > 0.0 && next_position > high)
        || (voice.source_step < 0.0 && next_position < low)
    {
        let span = high - low;
        if span <= 0.0 {
            voice.source_position = low;
            voice.source_step = 0.0;
            return;
        }

        let phase = (next_position - low).rem_euclid(span * 2.0);
        if phase == 0.0 {
            voice.source_position = low;
            voice.source_step = step_magnitude;
        } else if phase == span {
            voice.source_position = high;
            voice.source_step = -step_magnitude;
        } else if phase < span {
            voice.source_position = low + phase;
        } else {
            voice.source_position = high - (phase - span);
            voice.source_step = -voice.source_step;
        }
    } else {
        voice.source_position = next_position;
    }
}

/// Apply the project pan law to mono sources; stereo sources retain linear balance panning.
pub(crate) fn channel_pan_gains(pan: f32, mono: bool, pan_law: PanLaw) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if mono {
        match pan_law {
            PanLaw::Circular => {
                let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                (angle.cos(), angle.sin())
            }
            PanLaw::Triangular => ((1.0 - pan) * 0.5, (1.0 + pan) * 0.5),
        }
    } else if pan < 0.0 {
        (1.0, 1.0 + pan)
    } else {
        (1.0 - pan, 1.0)
    }
}

/// Treats the stored 0–128 note pan as an offset from the channel pan, centered at 64.
fn sampler_note_pan(channel_pan: f32, note_pan: u8) -> f32 {
    let note_pan_offset = f32::from(note_pan.min(128)) / 64.0 - 1.0;
    (channel_pan + note_pan_offset).clamp(-1.0, 1.0)
}

fn sampler_wav_gain(gain_db: Option<i8>) -> f32 {
    gain_db.map_or(1.0, |gain_db| 10.0f32.powf(f32::from(gain_db) / 20.0))
}

fn audio_clip_master_pitch_ratio(pitch_offset_cents: i16) -> f64 {
    2.0f64.powf(f64::from(pitch_offset_cents) / 1_200.0)
}

fn transpose_sampler_note_key(key: u16, semitones: i16) -> u16 {
    (i32::from(key) + i32::from(semitones)).clamp(0, 127) as u16
}

fn sampler_source_step(
    source_rate: u32,
    output_rate: u32,
    key: u16,
    root_key: u16,
    fine_tune_cents: f64,
) -> f64 {
    let semitones = (i32::from(key) - i32::from(root_key)).clamp(-48, 48);
    f64::from(source_rate) / f64::from(output_rate.max(1))
        * 2.0f64.powf(f64::from(semitones) / 12.0)
        * 2.0f64.powf(fine_tune_cents / 1_200.0)
}

fn resample_sample(
    channel: &[f32],
    position: f64,
    source_step: f64,
    quality: ResamplingQuality,
) -> f32 {
    if channel.is_empty() || !position.is_finite() {
        return 0.0;
    }
    match quality.sinc_taps() {
        Some(taps) => windowed_sinc_sample(channel, position, source_step, taps),
        None => match quality {
            ResamplingQuality::Linear => {
                let first_index = (position.floor() as usize).min(channel.len() - 1);
                let second_index = (first_index + 1).min(channel.len() - 1);
                let fraction = (position - first_index as f64) as f32;
                let first = finite_audio_sample(channel[first_index]);
                let second = finite_audio_sample(channel[second_index]);
                first + (second - first) * fraction
            }
            ResamplingQuality::Hermite6 => hermite6_sample(channel, position),
            ResamplingQuality::Sinc16
            | ResamplingQuality::Sinc24
            | ResamplingQuality::Sinc32
            | ResamplingQuality::Sinc64
            | ResamplingQuality::Sinc128
            | ResamplingQuality::Sinc256
            | ResamplingQuality::Sinc512 => unreachable!("sinc quality has a tap count"),
        },
    }
}

fn hermite6_sample(channel: &[f32], position: f64) -> f32 {
    let last_index = (channel.len() - 1) as isize;
    let position = position.clamp(0.0, last_index as f64);
    let center = position.floor() as isize;
    let fraction = position - center as f64;
    let sample = |offset: isize| {
        finite_audio_sample(channel[(center + offset).clamp(0, last_index) as usize]) as f64
    };

    let p0 = sample(0);
    let p1 = sample(1);
    let derivative0 = (sample(-2) - 8.0 * sample(-1) + 8.0 * p1 - sample(2)) / 12.0;
    let curvature0 = (-sample(-2) + 16.0 * sample(-1) - 30.0 * p0 + 16.0 * p1 - sample(2)) / 12.0;
    let derivative1 = (sample(-1) - 8.0 * p0 + 8.0 * sample(2) - sample(3)) / 12.0;
    let curvature1 = (-sample(-1) + 16.0 * p0 - 30.0 * p1 + 16.0 * sample(2) - sample(3)) / 12.0;

    let remainder0 = p1 - p0 - derivative0 - curvature0 * 0.5;
    let remainder1 = derivative1 - derivative0 - curvature0;
    let remainder2 = curvature1 - curvature0;
    let coefficient3 = 10.0 * remainder0 - 4.0 * remainder1 + 0.5 * remainder2;
    let coefficient4 = -15.0 * remainder0 + 7.0 * remainder1 - remainder2;
    let coefficient5 = 6.0 * remainder0 - 3.0 * remainder1 + 0.5 * remainder2;
    let interpolated = (((((coefficient5 * fraction + coefficient4) * fraction + coefficient3)
        * fraction
        + curvature0 * 0.5)
        * fraction
        + derivative0)
        * fraction)
        + p0;

    finite_audio_sample(interpolated as f32)
}

fn finite_audio_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn windowed_sinc_sample(channel: &[f32], position: f64, source_step: f64, taps: usize) -> f32 {
    let taps = taps as isize;
    let left_taps = taps / 2 - 1;
    let window_radius = taps as f64 / 2.0;
    let center = position.floor() as isize;
    let last_index = (channel.len() - 1) as isize;
    let source_step = source_step.abs();
    let cutoff = if source_step.is_finite() && source_step > 1.0 {
        (1.0 / source_step).clamp(1.0e-6, 1.0)
    } else {
        1.0
    };
    let mut weighted_sample = 0.0;
    let mut weight_sum = 0.0;

    for tap in 0..taps {
        let source_index = center + tap - left_taps;
        let distance = source_index as f64 - position;
        let window_position = distance / window_radius;
        if window_position.abs() >= 1.0 {
            continue;
        }
        let window = 0.42
            + 0.5 * (std::f64::consts::PI * window_position).cos()
            + 0.08 * (2.0 * std::f64::consts::PI * window_position).cos();
        let scaled_distance = cutoff * distance;
        let sinc = if scaled_distance.abs() < 1.0e-12 {
            1.0
        } else {
            (std::f64::consts::PI * scaled_distance).sin()
                / (std::f64::consts::PI * scaled_distance)
        };
        let weight = cutoff * sinc * window;
        let sample_index = source_index.clamp(0, last_index) as usize;
        weighted_sample += f64::from(finite_audio_sample(channel[sample_index])) * weight;
        weight_sum += weight;
    }

    if weight_sum.abs() < 1.0e-12 {
        finite_audio_sample(channel[center.clamp(0, last_index) as usize])
    } else {
        (weighted_sample / weight_sum) as f32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SampleBounds {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
struct PreparedClip {
    clip_index: usize,
    channel_id: u16,
    mixer_track: Option<i8>,
    path: PathBuf,
    start_frame: u64,
    source_bounds: SampleBounds,
    pitch_ratio: f64,
    duration_frames: u64,
    gain: f32,
    pan: f32,
    pan_law: PanLaw,
    mixer_transform: MixerSourceRouteTransform,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
}

struct PreparedAudioClipRender {
    summary: AudioClipRenderSummary,
    resampling_quality: ResamplingQuality,
    decoded_by_path: HashMap<PathBuf, DecodedAudio>,
    clips: Vec<PreparedClip>,
}

const STREAM_BLOCK_FRAMES: usize = 1024;
const MAX_PARALLEL_AUDIO_MIX_WORKERS: usize = 8;
const MIN_PARALLEL_AUDIO_MIX_CLIPS: usize = 4;
const MIN_PARALLEL_AUDIO_MIX_SECONDS: u64 = 2;

struct AudioClipMixWorkerState {
    generation: u64,
    block_start_frame: u64,
    sample_count: usize,
    completed_workers: usize,
    worker_errors: Vec<Option<String>>,
    stopping: bool,
}

struct AudioClipMixWorkerControl {
    state: Mutex<AudioClipMixWorkerState>,
    task_available: Condvar,
    block_complete: Condvar,
}

struct AudioClipMixWorkerScratch {
    stereo: Vec<f32>,
    meter_buses: Option<Vec<f32>>,
}

struct AudioClipMixWorkerTask<'a> {
    worker_index: usize,
    worker_count: usize,
    clips: Vec<&'a PreparedClip>,
    decoded_by_path: &'a HashMap<PathBuf, DecodedAudio>,
    sample_rate: u32,
    resampling_quality: ResamplingQuality,
    cancelled: &'a AtomicBool,
    control: Arc<AudioClipMixWorkerControl>,
    scratch: Arc<Vec<Mutex<AudioClipMixWorkerScratch>>>,
}

struct PreparedAudioClipMixContext<'a> {
    clips: &'a [PreparedClip],
    decoded_by_path: &'a HashMap<PathBuf, DecodedAudio>,
    sample_rate: u32,
    resampling_quality: ResamplingQuality,
    cancelled: &'a AtomicBool,
}

struct ParallelAudioClipMixer<'scope> {
    control: Arc<AudioClipMixWorkerControl>,
    scratch: Arc<Vec<Mutex<AudioClipMixWorkerScratch>>>,
    workers: Vec<std::thread::ScopedJoinHandle<'scope, ()>>,
}

fn audio_clips_grouped_by_mixer_track(clips: &[PreparedClip]) -> Vec<Vec<&PreparedClip>> {
    let mut by_track = BTreeMap::<Option<i8>, Vec<&PreparedClip>>::new();
    for clip in clips {
        by_track.entry(clip.mixer_track).or_default().push(clip);
    }
    by_track.into_values().collect()
}

fn audio_clip_mix_worker_count(
    clips: &[PreparedClip],
    track_groups: &[Vec<&PreparedClip>],
    sample_rate: u32,
) -> usize {
    let total_clip_frames = clips.iter().fold(0_u64, |total, clip| {
        total.saturating_add(clip.duration_frames)
    });
    if clips.len() < MIN_PARALLEL_AUDIO_MIX_CLIPS
        || track_groups.len() < 2
        || total_clip_frames < u64::from(sample_rate) * MIN_PARALLEL_AUDIO_MIX_SECONDS
    {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, |count| {
        count
            .get()
            .min(MAX_PARALLEL_AUDIO_MIX_WORKERS)
            .min(track_groups.len())
    })
}

fn audio_clip_groups_for_workers<'a>(
    track_groups: &[Vec<&'a PreparedClip>],
    worker_count: usize,
) -> Vec<Vec<&'a PreparedClip>> {
    if worker_count == 0 {
        return Vec::new();
    }
    let mut workers = (0..worker_count).map(|_| Vec::new()).collect::<Vec<_>>();
    for (group_index, group) in track_groups.iter().enumerate() {
        workers[group_index % worker_count].extend(group.iter().copied());
    }
    workers
}

impl<'scope> ParallelAudioClipMixer<'scope> {
    fn new<'env>(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        worker_clips: Vec<Vec<&'scope PreparedClip>>,
        decoded_by_path: &'scope HashMap<PathBuf, DecodedAudio>,
        sample_rate: u32,
        resampling_quality: ResamplingQuality,
        cancelled: &'scope AtomicBool,
        include_meters: bool,
    ) -> Option<Self> {
        let worker_count = worker_clips.len();
        if worker_count < 2 {
            return None;
        }
        let control = Arc::new(AudioClipMixWorkerControl {
            state: Mutex::new(AudioClipMixWorkerState {
                generation: 0,
                block_start_frame: 0,
                sample_count: 0,
                completed_workers: 0,
                worker_errors: vec![None; worker_count],
                stopping: false,
            }),
            task_available: Condvar::new(),
            block_complete: Condvar::new(),
        });
        let scratch = Arc::new(
            (0..worker_count)
                .map(|_| {
                    Mutex::new(AudioClipMixWorkerScratch {
                        stereo: vec![0.0; STREAM_BLOCK_FRAMES * 2],
                        meter_buses: include_meters
                            .then(|| vec![0.0; MIXER_TRACK_METER_COUNT * STREAM_BLOCK_FRAMES * 2]),
                    })
                })
                .collect::<Vec<_>>(),
        );
        let mut workers = Vec::with_capacity(worker_count);
        for (worker_index, clips) in worker_clips.into_iter().enumerate() {
            let worker_control = Arc::clone(&control);
            let worker_scratch = Arc::clone(&scratch);
            let worker_result = std::thread::Builder::new()
                .name(format!("flp-audio-mix-{worker_index}"))
                .spawn_scoped(scope, move || {
                    audio_clip_mix_worker(AudioClipMixWorkerTask {
                        worker_index,
                        worker_count,
                        clips,
                        decoded_by_path,
                        sample_rate,
                        resampling_quality,
                        cancelled,
                        control: worker_control,
                        scratch: worker_scratch,
                    });
                });
            match worker_result {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    request_audio_clip_mix_shutdown(&control);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return None;
                }
            }
        }
        Some(Self {
            control,
            scratch,
            workers,
        })
    }

    fn mix_block(
        &self,
        block_start_frame: u64,
        stereo: &mut [f32],
        meter_buses: Option<&mut [f32]>,
    ) -> Result<(), String> {
        let worker_count = self.scratch.len();
        {
            let mut state = self
                .control
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.block_start_frame = block_start_frame;
            state.sample_count = stereo.len();
            state.completed_workers = 0;
            state.worker_errors.fill(None);
            state.generation = state.generation.wrapping_add(1);
        }
        self.control.task_available.notify_all();
        let worker_error = {
            let mut state = self
                .control
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.completed_workers < worker_count {
                state = self
                    .control
                    .block_complete
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            state.worker_errors.iter_mut().find_map(Option::take)
        };
        if let Some(error) = worker_error {
            return Err(error);
        }

        let mut meter_buses = meter_buses;
        let sample_count = stereo.len();
        for worker_scratch in self.scratch.iter() {
            let worker_scratch = worker_scratch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (sample, addition) in stereo
                .iter_mut()
                .zip(&worker_scratch.stereo[..sample_count])
            {
                *sample += addition;
            }
            if let (Some(output_buses), Some(worker_buses)) = (
                meter_buses.as_deref_mut(),
                worker_scratch.meter_buses.as_deref(),
            ) {
                let active_meter_samples = MIXER_TRACK_METER_COUNT * sample_count;
                for (sample, addition) in output_buses
                    .iter_mut()
                    .zip(&worker_buses[..active_meter_samples])
                {
                    *sample += addition;
                }
            }
        }
        Ok(())
    }

    fn stop_and_join(&mut self) -> Result<(), String> {
        request_audio_clip_mix_shutdown(&self.control);
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                return Err("Playlist audio mixer worker stopped unexpectedly".to_owned());
            }
        }
        Ok(())
    }
}

impl Drop for ParallelAudioClipMixer<'_> {
    fn drop(&mut self) {
        request_audio_clip_mix_shutdown(&self.control);
    }
}

fn request_audio_clip_mix_shutdown(control: &AudioClipMixWorkerControl) {
    let mut state = control
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.stopping = true;
    drop(state);
    control.task_available.notify_all();
}

fn audio_clip_mix_worker(task: AudioClipMixWorkerTask<'_>) {
    let AudioClipMixWorkerTask {
        worker_index,
        worker_count,
        clips,
        decoded_by_path,
        sample_rate,
        resampling_quality,
        cancelled,
        control,
        scratch,
    } = task;
    let mut observed_generation = 0;
    loop {
        let (generation, block_start_frame, sample_count) = {
            let mut state = control
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.stopping && state.generation == observed_generation {
                state = control
                    .task_available
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if state.stopping {
                return;
            }
            observed_generation = state.generation;
            (
                state.generation,
                state.block_start_frame,
                state.sample_count,
            )
        };
        let worker_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut worker_scratch = scratch[worker_index]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let AudioClipMixWorkerScratch {
                stereo: stereo_buffer,
                meter_buses: meter_buffer,
            } = &mut *worker_scratch;
            let stereo = &mut stereo_buffer[..sample_count];
            stereo.fill(0.0);
            let mut meter_buses = if let Some(worker_buses) = meter_buffer.as_mut() {
                let active_meter_samples = MIXER_TRACK_METER_COUNT * sample_count;
                let buses = &mut worker_buses[..active_meter_samples];
                buses.fill(0.0);
                Some(buses)
            } else {
                None
            };
            for clip in &clips {
                let source = decoded_by_path.get(&clip.path).ok_or_else(|| {
                    format!(
                        "prepared audio clip {} has no decoded source",
                        clip.clip_index
                    )
                })?;
                mix_clip_window_into_stereo_with_quality_and_meter(
                    stereo,
                    block_start_frame,
                    source,
                    clip,
                    sample_rate,
                    resampling_quality,
                    ClipMixMonitor {
                        cancelled: Some(cancelled),
                        meter_buses: meter_buses.as_deref_mut(),
                        meter_stride_samples: sample_count,
                    },
                )
                .map_err(|error| {
                    format!(
                        "could not mix Playlist clip {} from audio channel {}: {error}",
                        clip.clip_index, clip.channel_id
                    )
                })?;
            }
            Ok::<_, String>(())
        }))
        .unwrap_or_else(|_| Err("Playlist audio mixer worker panicked".to_owned()));
        let mut state = control
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.generation == generation {
            state.worker_errors[worker_index] = worker_result.err();
            state.completed_workers += 1;
            if state.completed_workers == worker_count {
                control.block_complete.notify_one();
            }
        }
    }
}

fn mix_prepared_audio_clip_block(
    context: &PreparedAudioClipMixContext<'_>,
    block_start_frame: u64,
    stereo: &mut [f32],
    meter_buses: Option<&mut [f32]>,
    parallel_mixer: Option<&ParallelAudioClipMixer<'_>>,
) -> Result<(), String> {
    if let Some(parallel_mixer) = parallel_mixer {
        return parallel_mixer.mix_block(block_start_frame, stereo, meter_buses);
    }
    let mut meter_buses = meter_buses;
    let meter_stride_samples = stereo.len();
    for clip in context.clips {
        let source = context.decoded_by_path.get(&clip.path).ok_or_else(|| {
            format!(
                "prepared audio clip {} has no decoded source",
                clip.clip_index
            )
        })?;
        mix_clip_window_into_stereo_with_quality_and_meter(
            stereo,
            block_start_frame,
            source,
            clip,
            context.sample_rate,
            context.resampling_quality,
            ClipMixMonitor {
                cancelled: Some(context.cancelled),
                meter_buses: meter_buses.as_deref_mut(),
                meter_stride_samples,
            },
        )
        .map_err(|error| {
            format!(
                "could not mix Playlist clip {} from audio channel {}: {error}",
                clip.clip_index, clip.channel_id
            )
        })?;
    }
    Ok(())
}

fn sample_source_bounds(
    clip: &PlaylistClip,
    audio: &DecodedAudio,
    clip_index: usize,
) -> Result<SampleBounds, String> {
    sample_source_bounds_with_timing(clip, audio, clip_index, None)
}

fn sample_source_bounds_at_project_tempo(
    clip: &PlaylistClip,
    audio: &DecodedAudio,
    clip_index: usize,
    ppq: u16,
    base_tempo_bpm: f64,
) -> Result<SampleBounds, String> {
    if clip.source_window_ticks().is_none() {
        return sample_source_bounds(clip, audio, clip_index);
    }
    sample_source_bounds_with_timing(clip, audio, clip_index, Some((ppq, base_tempo_bpm)))
}

fn sample_source_bounds_with_timing(
    clip: &PlaylistClip,
    audio: &DecodedAudio,
    clip_index: usize,
    timing: Option<(u16, f64)>,
) -> Result<SampleBounds, String> {
    let frames = audio.frame_count();
    if let Some((start_tick, end_tick)) = clip.source_window_ticks() {
        if start_tick == 0 && end_tick == 0 {
            if frames == 0 {
                return Err(format!("Playlist clip {clip_index} references empty audio"));
            }
            return Ok(SampleBounds {
                start: 0,
                end: frames,
            });
        }
        let Some((ppq, base_tempo_bpm)) = timing else {
            if clip.uses_full_source_window() {
                if frames == 0 {
                    return Err(format!("Playlist clip {clip_index} references empty audio"));
                }
                return Ok(SampleBounds {
                    start: 0,
                    end: frames,
                });
            }
            return Err(format!(
                "Playlist clip {clip_index} tick source window needs project timing"
            ));
        };
        if ppq == 0 || !base_tempo_bpm.is_finite() || base_tempo_bpm <= 0.0 {
            return Err(format!(
                "Playlist clip {clip_index} tick source window has invalid project timing"
            ));
        }
        let ticks_to_frame = |ticks: u32| -> Result<usize, String> {
            let frame = f64::from(ticks) * 60.0 * f64::from(audio.sample_rate)
                / (f64::from(ppq) * base_tempo_bpm);
            if !frame.is_finite() || frame < 0.0 || frame > usize::MAX as f64 {
                return Err(format!(
                    "Playlist clip {clip_index} tick source window is out of range"
                ));
            }
            Ok(frame.round() as usize)
        };
        let start = ticks_to_frame(start_tick)?.min(frames);
        let end = ticks_to_frame(end_tick)?.min(frames);
        if start >= end {
            return Err(format!(
                "Playlist clip {clip_index} tick source window is empty or outside the audio"
            ));
        }
        return Ok(SampleBounds { start, end });
    }
    if clip.uses_full_source_window() {
        if frames == 0 {
            return Err(format!("Playlist clip {clip_index} references empty audio"));
        }
        return Ok(SampleBounds {
            start: 0,
            end: frames,
        });
    }
    let Some((start_ms, end_ms)) = clip.source_window_milliseconds() else {
        return Err(format!(
            "Playlist clip {clip_index} has an unsupported audio source window"
        ));
    };
    let to_frame = |milliseconds: f64| -> Result<usize, String> {
        let frame = milliseconds * f64::from(audio.sample_rate) / 1000.0;
        if !frame.is_finite() || frame < 0.0 || frame > usize::MAX as f64 {
            return Err(format!(
                "Playlist clip {clip_index} audio offset is out of range"
            ));
        }
        Ok(frame.round() as usize)
    };
    let start = to_frame(start_ms)?.min(frames);
    let end = to_frame(end_ms)?.min(frames);
    if start >= end {
        return Err(format!(
            "Playlist clip {clip_index} source window {}..{} ms is outside its sample",
            start_ms, end_ms
        ));
    }
    Ok(SampleBounds { start, end })
}

fn ticks_to_frames(ticks: u64, ppq: u16, tempo_bpm: f64, sample_rate: u32) -> Result<u64, String> {
    let frames = ticks as f64 * 60.0 * f64::from(sample_rate) / (f64::from(ppq) * tempo_bpm);
    if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
        return Err("Playlist position is outside the renderable timeline".to_owned());
    }
    Ok(frames.round() as u64)
}

fn source_duration_to_frames(
    source_frames: usize,
    source_rate: u32,
    output_rate: u32,
    playback_rate_ratio: f64,
) -> Result<u64, String> {
    if source_rate == 0 || output_rate == 0 {
        return Err("audio sample rate must be greater than zero".to_owned());
    }
    if !playback_rate_ratio.is_finite() || playback_rate_ratio <= 0.0 {
        return Err("audio playback rate must be finite and greater than zero".to_owned());
    }
    let duration = source_frames as f64 * f64::from(output_rate)
        / (f64::from(source_rate) * playback_rate_ratio);
    if !duration.is_finite() || duration < 0.0 || duration > u64::MAX as f64 {
        return Err("audio clip duration is outside the renderable range".to_owned());
    }
    Ok(duration.ceil() as u64)
}

pub(crate) fn channel_gain_pan(volume: Option<u32>, pan: Option<i32>) -> (f32, f32) {
    let gain = volume.unwrap_or(10_000).min(12_800) as f32 / 10_000.0;
    let pan = pan
        .map(|raw| (raw.clamp(0, 12_800) as f32 / 6_400.0) - 1.0)
        .unwrap_or(0.0);
    (gain, pan)
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<(), String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err("audio render cancelled".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn mix_clip_into_stereo(
    mix: &mut [f32],
    source: &DecodedAudio,
    clip: &PreparedClip,
    output_rate: u32,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
    mix_clip_window_into_stereo(mix, 0, source, clip, output_rate, cancelled)
}

#[cfg(test)]
fn mix_clip_window_into_stereo(
    mix: &mut [f32],
    mix_start_frame: u64,
    source: &DecodedAudio,
    clip: &PreparedClip,
    output_rate: u32,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
    mix_clip_window_into_stereo_with_quality(
        mix,
        mix_start_frame,
        source,
        clip,
        output_rate,
        ResamplingQuality::Linear,
        cancelled,
    )
}

fn mix_clip_window_into_stereo_with_quality(
    mix: &mut [f32],
    mix_start_frame: u64,
    source: &DecodedAudio,
    clip: &PreparedClip,
    output_rate: u32,
    resampling_quality: ResamplingQuality,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
    mix_clip_window_into_stereo_with_quality_and_meter(
        mix,
        mix_start_frame,
        source,
        clip,
        output_rate,
        resampling_quality,
        ClipMixMonitor {
            cancelled,
            meter_buses: None,
            meter_stride_samples: mix.len(),
        },
    )
}

struct ClipMixMonitor<'a> {
    cancelled: Option<&'a AtomicBool>,
    meter_buses: Option<&'a mut [f32]>,
    meter_stride_samples: usize,
}

fn mix_clip_window_into_stereo_with_quality_and_meter(
    mix: &mut [f32],
    mix_start_frame: u64,
    source: &DecodedAudio,
    clip: &PreparedClip,
    output_rate: u32,
    resampling_quality: ResamplingQuality,
    monitor: ClipMixMonitor<'_>,
) -> Result<(), String> {
    let ClipMixMonitor {
        cancelled,
        mut meter_buses,
        meter_stride_samples,
    } = monitor;
    if source.channels.is_empty() || source.channels.len() > 2 {
        return Err("source must have one or two channels".to_owned());
    }
    if source.sample_rate == 0
        || output_rate == 0
        || clip.source_bounds.start >= clip.source_bounds.end
    {
        return Err("source audio range or sample rate is invalid".to_owned());
    }
    if source
        .channels
        .iter()
        .any(|channel| clip.source_bounds.end > channel.len())
    {
        return Err("source audio range exceeds a channel buffer".to_owned());
    }
    let end_frame = clip
        .start_frame
        .checked_add(clip.duration_frames)
        .ok_or_else(|| "audio clip end position overflow".to_owned())?;
    let mix_frames = mix.len() / 2;
    let mix_end_frame = mix_start_frame
        .checked_add(mix_frames as u64)
        .ok_or_else(|| "audio mix window end position overflow".to_owned())?;
    if !mix.len().is_multiple_of(2) {
        return Err("audio mix window must contain stereo frames".to_owned());
    }
    if meter_buses.as_ref().is_some_and(|meter_buses| {
        meter_stride_samples != mix.len()
            || meter_buses.len() != MIXER_TRACK_METER_COUNT.saturating_mul(mix.len())
    }) {
        return Err("Mixer meter buses must match the audio mix window".to_owned());
    }
    let overlap_start = clip.start_frame.max(mix_start_frame);
    let overlap_end = end_frame.min(mix_end_frame);
    if overlap_start >= overlap_end {
        return Ok(());
    }
    let (left_pan_gain, right_pan_gain) =
        channel_pan_gains(clip.pan, source.channels.len() == 1, clip.pan_law);
    let source_frames_per_output =
        f64::from(source.sample_rate) / f64::from(output_rate) * clip.pitch_ratio;
    for output_frame in overlap_start..overlap_end {
        let output_offset = output_frame - clip.start_frame;
        if output_offset.is_multiple_of(16_384) {
            check_cancelled(cancelled)?;
        }
        let position = (clip.source_bounds.start as f64
            + output_offset as f64 * source_frames_per_output)
            .min((clip.source_bounds.end - 1) as f64);
        let source_position = position - clip.source_bounds.start as f64;
        let left = resample_sample(
            &source.channels[0][clip.source_bounds.start..clip.source_bounds.end],
            source_position,
            source_frames_per_output,
            resampling_quality,
        );
        let right = if source.channels.len() == 1 {
            left
        } else {
            resample_sample(
                &source.channels[1][clip.source_bounds.start..clip.source_bounds.end],
                source_position,
                source_frames_per_output,
                resampling_quality,
            )
        };
        let output_index = usize::try_from(output_frame - mix_start_frame)
            .map_err(|_| "audio output frame index exceeds this platform".to_owned())?
            * 2;
        let mut frame = [
            left * clip.gain * left_pan_gain,
            right * clip.gain * right_pan_gain,
        ];
        clip.mixer_transform.apply_frame(&mut frame);
        mix[output_index] += frame[0];
        mix[output_index + 1] += frame[1];
        if let Some(meter_buses) = meter_buses.as_deref_mut() {
            for (track, transform) in &clip.mixer_meter_transforms {
                if *track >= MIXER_TRACK_METER_COUNT {
                    return Err("Mixer meter route exceeds the supported track range".to_owned());
                }
                let mut meter_frame = [
                    left * clip.gain * left_pan_gain,
                    right * clip.gain * right_pan_gain,
                ];
                transform.apply_frame(&mut meter_frame);
                let meter_offset = *track * meter_stride_samples + output_index;
                meter_buses[meter_offset] += meter_frame[0];
                meter_buses[meter_offset + 1] += meter_frame[1];
            }
        }
    }
    Ok(())
}

fn decoded_audio_bytes(audio: &DecodedAudio) -> Result<usize, String> {
    audio
        .channels
        .iter()
        .try_fold(0usize, |bytes, channel| {
            channel
                .len()
                .checked_mul(std::mem::size_of::<f32>())
                .and_then(|channel_bytes| bytes.checked_add(channel_bytes))
        })
        .ok_or_else(|| "decoded audio size overflow".to_owned())
}

fn validate_output_path(project_path: &Path, output_path: &Path) -> Result<(), String> {
    validate_output_path_with_extension(project_path, output_path, "wav")
}

fn validate_flac_output_path(project_path: &Path, output_path: &Path) -> Result<(), String> {
    validate_output_path_with_extension(project_path, output_path, "flac")
}

fn validate_ogg_output_path(project_path: &Path, output_path: &Path) -> Result<(), String> {
    validate_output_path_with_extension(project_path, output_path, "ogg")
}

fn validate_mp3_output_path(project_path: &Path, output_path: &Path) -> Result<(), String> {
    validate_output_path_with_extension(project_path, output_path, "mp3")
}

fn validate_output_path_with_extension(
    project_path: &Path,
    output_path: &Path,
    extension: &str,
) -> Result<(), String> {
    if !output_path
        .extension()
        .is_some_and(|output_extension| output_extension.eq_ignore_ascii_case(extension))
    {
        return Err(format!("render output must use the .{extension} extension"));
    }
    let project = fs::canonicalize(project_path).map_err(|error| {
        format!(
            "could not resolve project path {}: {error}",
            project_path.display()
        )
    })?;
    let output = if output_path.exists() {
        fs::canonicalize(output_path).map_err(|error| {
            format!(
                "could not resolve output path {}: {error}",
                output_path.display()
            )
        })?
    } else {
        let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|error| {
            format!(
                "could not resolve output folder {}: {error}",
                parent.display()
            )
        })?;
        let file_name = output_path
            .file_name()
            .ok_or_else(|| "render output path must include a file name".to_owned())?;
        parent.join(file_name)
    };
    let same = if cfg!(windows) {
        project
            .to_string_lossy()
            .eq_ignore_ascii_case(&output.to_string_lossy())
    } else {
        project == output
    };
    if same {
        return Err("render output path must not overwrite the FLP project".to_owned());
    }
    Ok(())
}

#[cfg(test)]
fn write_stereo_wav_from_buffer(
    output_path: &Path,
    samples: &[f32],
    sample_rate: u32,
    frames: u64,
    sample_format: WavSampleFormat,
    dither_mode: WavDitherMode,
    channel_mode: WavChannelMode,
) -> Result<(), String> {
    let frames =
        u32::try_from(frames).map_err(|_| "render is too long for a RIFF/WAVE file".to_owned())?;
    let expected_samples = usize::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(2))
        .ok_or_else(|| "rendered WAV sample count overflow".to_owned())?;
    if samples.len() != expected_samples {
        return Err("rendered WAV buffer does not match its header".to_owned());
    }
    let block_align = sample_format.bytes_per_sample() * channel_mode.channel_count();
    let data_bytes = frames
        .checked_mul(u32::from(block_align))
        .filter(|bytes| *bytes <= u32::MAX - 36)
        .ok_or_else(|| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;
    let mut temporary = TemporaryRenderFile::create(output_path)?;
    {
        let file = temporary
            .file
            .as_mut()
            .expect("temporary render file is open");
        write_wav_header(
            file,
            sample_rate,
            frames,
            data_bytes,
            sample_format,
            channel_mode,
        )?;
        let samples_per_frame = usize::from(channel_mode.channel_count());
        let bytes_per_sample = usize::from(sample_format.bytes_per_sample());
        let mut bytes =
            Vec::with_capacity(STREAM_BLOCK_FRAMES * samples_per_frame * bytes_per_sample);
        let mut dither_state = WavDitherState::new(dither_mode, sample_format);
        for block in samples.chunks(STREAM_BLOCK_FRAMES * 2) {
            bytes.clear();
            append_wav_block_samples_with_dither(
                block,
                sample_format,
                channel_mode,
                dither_state.as_mut(),
                &mut bytes,
            )?;
            file.write_all(&bytes)
                .map_err(|error| format!("could not write rendered WAV audio data: {error}"))?;
        }
    }
    temporary.commit(output_path)?;
    Ok(())
}

fn write_audio_buffer_to_file(
    output_path: &Path,
    samples: &[f32],
    frames: u64,
    settings: AudioBufferEncodingSettings,
    output_format: AudioRenderOutput,
) -> Result<(), String> {
    render_stereo_audio_to_file(
        output_path,
        frames,
        settings.sample_rate,
        AudioRenderEncodingOptions {
            wav_sample_format: settings.options.wav_sample_format,
            wav_dither_mode: settings.options.wav_dither_mode,
            wav_channel_mode: settings.options.wav_channel_mode,
        },
        output_format,
        |write_block| {
            for block in samples.chunks(STREAM_BLOCK_FRAMES * 2) {
                write_block(block)?;
            }
            Ok(())
        },
    )
}

pub(crate) fn render_stereo_audio_to_file<T>(
    output_path: &Path,
    expected_frames: u64,
    sample_rate: u32,
    options: AudioRenderEncodingOptions,
    output_format: AudioRenderOutput,
    render: impl FnOnce(&mut dyn FnMut(&[f32]) -> Result<(), String>) -> Result<T, String>,
) -> Result<T, String> {
    if !output_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(output_format.extension()))
    {
        return Err(format!(
            "render output must use the .{} extension",
            output_format.extension()
        ));
    }
    let mut temporary = TemporaryRenderFile::create(output_path)?;
    let result = {
        let file = temporary
            .file
            .as_mut()
            .expect("temporary render file is open");
        let mut encoder = AudioFileStreamEncoder::new(
            file,
            sample_rate,
            expected_frames,
            options,
            output_format,
        )?;
        let mut frames_written = 0_u64;
        let result = {
            let mut write_block = |block: &[f32]| {
                if !block.len().is_multiple_of(2) {
                    return Err("render block must contain interleaved stereo frames".to_owned());
                }
                let block_frames = u64::try_from(block.len() / 2)
                    .map_err(|_| "render frame count exceeds this platform".to_owned())?;
                frames_written = frames_written
                    .checked_add(block_frames)
                    .filter(|written| *written <= expected_frames)
                    .ok_or_else(|| "render produced more frames than expected".to_owned())?;
                encoder.write_stereo_block(block, options.wav_channel_mode)
            };
            render(&mut write_block)?
        };
        if frames_written != expected_frames {
            return Err("audio encoder frame count does not match rendered audio".to_owned());
        }
        encoder.finalize(expected_frames)?;
        result
    };
    temporary.commit(output_path)?;
    Ok(result)
}

struct TemporaryRenderFile {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl TemporaryRenderFile {
    fn create(output_path: &Path) -> Result<Self, String> {
        let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = output_path
            .file_name()
            .ok_or_else(|| "render output path must include a file name".to_owned())?;
        for _ in 0..100 {
            let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let mut temporary_name = file_name.to_os_string();
            temporary_name.push(format!(".{}.{}.tmp", std::process::id(), id));
            let path = parent.join(temporary_name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "could not create temporary render file beside {}: {error}",
                        output_path.display()
                    ));
                }
            }
        }
        Err("could not allocate a unique temporary render file".to_owned())
    }

    fn commit(mut self, output_path: &Path) -> Result<(), String> {
        let mut file = self.file.take().expect("temporary render file is open");
        file.flush()
            .map_err(|error| format!("could not flush rendered audio: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not sync rendered audio: {error}"))?;
        drop(file);
        if output_path.exists() {
            fs::remove_file(output_path)
                .map_err(|error| format!("could not replace {}: {error}", output_path.display()))?;
        }
        fs::rename(&self.path, output_path)
            .map_err(|error| format!("could not finalize {}: {error}", output_path.display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TemporaryRenderFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn mixer_master_mute_silences_the_playlist_mix() {
        let mut output = [0.25, -0.5, 0.75, -1.0];
        MixerMasterOutput {
            enabled: Some(false),
            ..MixerMasterOutput::default()
        }
        .apply(&mut output);
        assert_eq!(output, [0.0; 4]);
    }

    #[test]
    fn mixer_master_polarity_and_stereo_swap_transform_the_final_bus() {
        let mut output = [0.25, -0.5, 0.75, -1.0];
        MixerMasterOutput {
            enabled: Some(true),
            polarity_reversed: Some(true),
            swap_left_right: Some(true),
            ..MixerMasterOutput::default()
        }
        .apply(&mut output);
        assert_eq!(output, [0.5, -0.25, 1.0, -0.75]);
    }

    #[test]
    fn mixer_master_fader_scales_the_final_bus() {
        let mut output = [0.25, -0.5, 0.75, -1.0];
        MixerMasterOutput {
            enabled: Some(true),
            fader_volume_raw: Some(5_000),
            ..MixerMasterOutput::default()
        }
        .apply(&mut output);
        assert_eq!(output, [0.125, -0.25, 0.375, -0.5]);
    }

    #[test]
    fn mixer_master_pan_crossmixes_the_final_bus() {
        let mut output = [1.0, 2.0, 3.0, 4.0];
        MixerMasterOutput {
            enabled: Some(true),
            mixer_pan_raw: Some(6_400),
            ..MixerMasterOutput::default()
        }
        .apply(&mut output);
        assert_eq!(output, [0.0, 3.0, 0.0, 7.0]);
    }

    #[test]
    fn mixer_master_stereo_separation_changes_the_final_bus_width() {
        let mut mono = [1.0, -1.0];
        MixerMasterOutput {
            enabled: Some(true),
            stereo_separation_raw: Some(64),
            ..MixerMasterOutput::default()
        }
        .apply(&mut mono);
        assert_eq!(mono, [0.0, 0.0]);

        let mut widened = [1.0, -1.0];
        MixerMasterOutput {
            enabled: Some(true),
            stereo_separation_raw: Some(-64),
            ..MixerMasterOutput::default()
        }
        .apply(&mut widened);
        assert_eq!(widened, [2.0, -2.0]);
    }

    #[test]
    fn mixer_master_pan_uses_the_project_pan_law_at_center() {
        let mut circular = [1.0, 2.0];
        MixerMasterOutput {
            enabled: Some(true),
            mixer_pan_raw: Some(0),
            ..MixerMasterOutput::default()
        }
        .apply(&mut circular);
        let center_gain = std::f32::consts::FRAC_1_SQRT_2;
        assert!((circular[0] - center_gain).abs() < 1.0e-6);
        assert!((circular[1] - 2.0 * center_gain).abs() < 1.0e-6);

        let mut triangular = [1.0, 2.0];
        MixerMasterOutput {
            enabled: Some(true),
            mixer_pan_raw: Some(0),
            mixer_pan_law_raw: Some(2),
            ..MixerMasterOutput::default()
        }
        .apply(&mut triangular);
        assert_eq!(triangular, [1.0, 2.0]);
    }

    #[test]
    fn unknown_mixer_master_flags_leave_the_playlist_mix_unchanged() {
        let mut output = [0.25, -0.5];
        MixerMasterOutput::default().apply(&mut output);
        assert_eq!(output, [0.25, -0.5]);
    }

    fn test_pattern_clip(pattern_id: u16, position_ticks: u32, length_ticks: u32) -> PlaylistClip {
        PlaylistClip {
            position_ticks,
            pattern_base: 0x5000,
            item_index: 0x5000 + pattern_id,
            length_ticks,
            raw_track_index: 0,
            track_index: Some(0),
            group: 0,
            unknown_word: 0,
            item_flags: 0,
            muted: false,
            header_bytes: [0; 4],
            start_offset: -1.0,
            end_offset: -1.0,
            source_window_ticks: None,
            clip_id: None,
            reserved: Vec::new(),
            scale: Some(1.0),
            trailing_bytes: Vec::new(),
            record_size: 80,
            source_event_index: 0,
            source_record_index: 0,
        }
    }

    #[test]
    fn linear_resampling_preserves_existing_interpolation() {
        let samples = [0.0, 1.0, -1.0];
        assert_eq!(
            resample_sample(&samples, 0.25, 1.0, ResamplingQuality::Linear),
            0.25
        );
        assert_eq!(
            resample_sample(&samples, 1.5, 1.0, ResamplingQuality::Linear),
            0.0
        );
    }

    #[test]
    fn hermite6_resampling_preserves_dc_and_integer_samples() {
        let constant = vec![0.375; 16];
        assert!((hermite6_sample(&constant, 7.25) - 0.375).abs() < 1.0e-6);

        let samples = [0.0, 0.5, -0.25, 0.75, -1.0, 0.25, 0.125, -0.5];
        for (index, sample) in samples.iter().enumerate() {
            assert_eq!(hermite6_sample(&samples, index as f64), *sample);
        }
    }

    #[test]
    fn hermite6_resampling_reconstructs_a_quadratic_curve() {
        let samples = (0..12)
            .map(|index| (index * index) as f32)
            .collect::<Vec<_>>();
        let position = 5.25;
        let rendered = resample_sample(&samples, position, 1.0, ResamplingQuality::Hermite6);
        let expected = (position * position) as f32;
        assert!((rendered - expected).abs() < 1.0e-4);
    }

    #[test]
    fn resampling_quality_settings_keys_round_trip() {
        for quality in ResamplingQuality::RENDER_OPTIONS {
            assert_eq!(
                ResamplingQuality::from_settings_key(quality.settings_key()),
                Some(quality)
            );
        }
        assert_eq!(ResamplingQuality::LIVE_DEFAULT, ResamplingQuality::Sinc24);
        assert_eq!(ResamplingQuality::RENDER_DEFAULT, ResamplingQuality::Sinc32);
        assert_eq!(
            AudioClipRenderOptions::default().resampling_quality,
            ResamplingQuality::RENDER_DEFAULT
        );
        assert_eq!(
            SamplerPatternRenderOptions::default().resampling_quality,
            ResamplingQuality::RENDER_DEFAULT
        );
        assert_eq!(
            PlaylistRenderOptions::default().resampling_quality,
            ResamplingQuality::RENDER_DEFAULT
        );
        assert_eq!(
            ResamplingQuality::LIVE_OPTIONS,
            [
                ResamplingQuality::Linear,
                ResamplingQuality::Hermite6,
                ResamplingQuality::Sinc24,
                ResamplingQuality::Sinc64,
                ResamplingQuality::Sinc128,
                ResamplingQuality::Sinc256,
                ResamplingQuality::Sinc512,
            ]
        );
        assert_eq!(ResamplingQuality::from_settings_key("unknown"), None);
    }

    #[test]
    fn selected_audio_clip_is_filtered_and_rebased_to_time_zero() {
        assert!(audio_clip_matches_selection(None, 0));
        assert!(audio_clip_matches_selection(Some(2), 2));
        assert!(!audio_clip_matches_selection(Some(2), 1));
        assert_eq!(audio_clip_selection_start_tick(Some(384), false), 384);
        assert_eq!(audio_clip_selection_start_tick(Some(384), true), 0);
        assert_eq!(audio_clip_selection_start_tick(None, true), 0);
        assert_eq!(audio_clip_render_start_tick(384, 384).unwrap(), 0);
        assert_eq!(audio_clip_render_start_tick(384, 0).unwrap(), 384);
        assert!(audio_clip_render_start_tick(96, 192).is_err());
    }

    #[test]
    fn sinc_resampling_preserves_dc_at_fractional_positions() {
        let samples = vec![0.375; 1024];
        for quality in ResamplingQuality::SINC_OPTIONS {
            let rendered = resample_sample(&samples, 512.25, 1.0, quality);
            assert!(
                (rendered - 0.375).abs() < 1.0e-6,
                "{quality:?} did not preserve DC: {rendered}"
            );
        }
    }

    #[test]
    fn sinc_resampling_attenuates_nyquist_tone_when_downsampling() {
        let samples = (0usize..2048)
            .map(|index| if index.is_multiple_of(2) { 1.0 } else { -1.0 })
            .collect::<Vec<_>>();
        for quality in ResamplingQuality::SINC_OPTIONS {
            let rendered = resample_sample(&samples, 1024.0, 2.0, quality);
            assert!(
                rendered.abs() < 0.01,
                "{quality:?} left unexpected aliased output: {rendered}"
            );
        }
    }

    #[test]
    fn converts_project_ticks_to_output_frames() {
        assert_eq!(ticks_to_frames(192, 96, 120.0, 48_000).unwrap(), 48_000);
    }

    #[test]
    fn playlist_tempo_map_integrates_tempo_changes_and_inverts_frame_positions() {
        let map =
            PlaylistTempoMap::from_tempo_events(96, 120.0, &[(96, 60.0), (192, 120.0)]).unwrap();

        assert_eq!(map.frame_at_tick(0, 48_000).unwrap(), 0);
        assert_eq!(map.frame_at_tick(96, 48_000).unwrap(), 24_000);
        assert_eq!(map.frame_at_tick(192, 48_000).unwrap(), 72_000);
        assert_eq!(map.frame_at_tick(288, 48_000).unwrap(), 96_000);
        assert_eq!(map.frames_between_ticks(96, 192, 48_000).unwrap(), 48_000);
        assert!((map.tick_at_frame(48_000, 48_000).unwrap() - 144.0).abs() < 1e-9);
    }

    #[test]
    fn playlist_tempo_map_converts_fractional_ticks_across_tempo_changes() {
        let map = PlaylistTempoMap::from_tempo_events(96, 120.0, &[(96, 60.0)]).unwrap();

        assert_eq!(map.frame_at_fractional_tick(4.0, 48_000), Ok(1_000));
        assert_eq!(map.frame_at_fractional_tick(96.0, 48_000), Ok(24_000));
        assert_eq!(map.frame_at_fractional_tick(100.0, 48_000), Ok(26_000));
        assert!(map.frame_at_fractional_tick(f64::NAN, 48_000).is_err());
        assert!(map.frame_at_fractional_tick(4.0, 0).is_err());
    }

    #[test]
    fn playlist_tempo_map_sizes_audio_from_its_start_position() {
        let map =
            PlaylistTempoMap::from_tempo_events(96, 120.0, &[(96, 60.0), (192, 120.0)]).unwrap();

        assert_eq!(map.ticks_for_audio_frames(0, 48_000, 48_000).unwrap(), 144);
        assert_eq!(map.ticks_for_audio_frames(96, 48_000, 48_000).unwrap(), 96);
        assert!(map.ticks_for_audio_frames(0, 0, 48_000).is_err());
        assert!(map.ticks_for_audio_frames(0, 48_000, 0).is_err());
    }

    #[test]
    fn playlist_tempo_map_uses_last_tempo_event_at_a_shared_tick() {
        let map =
            PlaylistTempoMap::from_tempo_events(96, 120.0, &[(96, 60.0), (96, 90.0)]).unwrap();

        assert_eq!(map.frame_at_tick(192, 48_000).unwrap(), 56_000);
    }

    #[test]
    fn channel_rack_swing_shifts_even_sixteenth_onsets_by_the_combined_mix() {
        assert_eq!(swing_note_start_tick(0, 96, 128, 128).unwrap(), 0);
        assert_eq!(swing_note_start_tick(24, 96, 128, 128).unwrap(), 32);
        assert_eq!(swing_note_start_tick(48, 96, 128, 128).unwrap(), 48);
        assert_eq!(swing_note_start_tick(25, 96, 128, 128).unwrap(), 25);
        assert_eq!(swing_note_start_tick(24, 96, 64, 64).unwrap(), 26);
        assert_eq!(swing_note_start_tick(24, 96, 128, 0).unwrap(), 24);
        assert_eq!(swing_note_start_tick(24, 96, 0, 128).unwrap(), 24);
        assert_eq!(swing_note_start_tick(24, 96, 129, 128).unwrap(), 24);
    }

    #[test]
    fn adds_render_tail_frames_with_overflow_checks() {
        assert_eq!(add_render_tail_frames(12_000, 48_000, 0).unwrap(), 12_000);
        assert_eq!(add_render_tail_frames(12_000, 48_000, 2).unwrap(), 108_000);
        assert_eq!(
            add_render_tail_frames(u64::MAX, 48_000, 1),
            Err("render length overflow".to_owned())
        );
    }

    #[test]
    fn playlist_pattern_clips_place_repeat_and_clip_sampler_notes() {
        let pattern = Pattern {
            id: 5,
            length_ticks: Some(192),
            notes: vec![
                PatternNote {
                    position: 0,
                    length: 96,
                    channel_id: 2,
                    ..PatternNote::default()
                },
                PatternNote {
                    position: 144,
                    length: 96,
                    channel_id: 2,
                    ..PatternNote::default()
                },
            ],
            ..Pattern::default()
        };
        let arrangement = Arrangement {
            id: 0,
            clips: vec![test_pattern_clip(5, 96, 384)],
            ..Arrangement::default()
        };

        let patterns = [pattern];
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            PlaylistTrackFilter::new(&BTreeSet::new(), None),
            PlaylistPatternScheduleOptions {
                ppq: 96,
                global_swing_mix_raw: 0,
                play_truncated_notes_in_clips: false,
            },
            |_| 128,
            |channel_id, _, _| vec![channel_id],
        )
        .unwrap();
        assert_eq!(
            schedule
                .notes
                .iter()
                .map(|placed| (placed.start_tick, placed.stop_tick))
                .collect::<Vec<_>>(),
            vec![
                (96, Some(192)),
                (240, Some(336)),
                (288, Some(384)),
                (432, Some(480)),
            ]
        );
    }

    #[test]
    fn disabled_playlist_tracks_do_not_schedule_pattern_notes() {
        let pattern = Pattern {
            id: 2,
            length_ticks: Some(96),
            notes: vec![PatternNote {
                length: 48,
                channel_id: 1,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let arrangement = Arrangement {
            clips: vec![test_pattern_clip(2, 0, 96)],
            ..Arrangement::default()
        };
        let patterns = [pattern];
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            PlaylistTrackFilter::new(&BTreeSet::from([1]), None),
            PlaylistPatternScheduleOptions {
                ppq: 96,
                global_swing_mix_raw: 0,
                play_truncated_notes_in_clips: false,
            },
            |_| 128,
            |channel_id, _, _| vec![channel_id],
        )
        .unwrap();

        assert!(schedule.notes.is_empty());
    }

    #[test]
    fn playlist_solo_filter_uses_track_range_and_preserves_mute_state() {
        let first_track_clip = test_pattern_clip(2, 0, 96);
        let mut second_track_clip = test_pattern_clip(2, 0, 96);
        second_track_clip.track_index = Some(1);
        let mut unassigned_clip = test_pattern_clip(2, 0, 96);
        unassigned_clip.track_index = None;
        let disabled_track_ids = BTreeSet::from([1]);

        assert!(
            PlaylistTrackFilter::new(&disabled_track_ids, None).clip_is_muted(&first_track_clip)
        );
        assert!(
            !PlaylistTrackFilter::new(&disabled_track_ids, Some((1, 1)))
                .clip_is_muted(&first_track_clip)
        );
        assert!(
            PlaylistTrackFilter::new(&disabled_track_ids, Some((1, 1)))
                .clip_is_muted(&second_track_clip)
        );
        assert!(
            PlaylistTrackFilter::new(&disabled_track_ids, Some((1, 1)))
                .clip_is_muted(&unassigned_clip)
        );

        let mut muted_clip = test_pattern_clip(2, 0, 96);
        muted_clip.muted = true;
        assert!(
            PlaylistTrackFilter::new(&BTreeSet::new(), None).clip_is_muted(&muted_clip),
            "an individually muted clip stays muted even when its Playlist track is soloed"
        );
        assert!(
            PlaylistTrackFilter::new(&BTreeSet::new(), Some((1, 1))).clip_is_muted(&muted_clip)
        );
    }

    #[test]
    fn soloed_playlist_track_overrides_mute_and_excludes_other_tracks() {
        let pattern = Pattern {
            id: 2,
            length_ticks: Some(96),
            notes: vec![PatternNote {
                length: 48,
                channel_id: 1,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let mut other_track_clip = test_pattern_clip(2, 96, 96);
        other_track_clip.track_index = Some(1);
        let arrangement = Arrangement {
            clips: vec![test_pattern_clip(2, 0, 96), other_track_clip],
            ..Arrangement::default()
        };
        let patterns = [pattern];
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            PlaylistTrackFilter::new(&BTreeSet::from([1]), Some((1, 1))),
            PlaylistPatternScheduleOptions {
                ppq: 96,
                global_swing_mix_raw: 0,
                play_truncated_notes_in_clips: false,
            },
            |_| 128,
            |channel_id, _, _| vec![channel_id],
        )
        .unwrap();

        assert_eq!(schedule.notes.len(), 1);
        assert_eq!(schedule.notes[0].start_tick, 0);
    }

    #[test]
    fn playlist_pattern_notes_expand_to_ordered_layer_targets() {
        let pattern = Pattern {
            id: 8,
            length_ticks: Some(96),
            notes: vec![PatternNote {
                position: 24,
                length: 48,
                channel_id: 7,
                key: 72,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let arrangement = Arrangement {
            clips: vec![test_pattern_clip(8, 96, 192)],
            ..Arrangement::default()
        };

        let patterns = [pattern];
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            PlaylistTrackFilter::new(&BTreeSet::new(), None),
            PlaylistPatternScheduleOptions {
                ppq: 96,
                global_swing_mix_raw: 0,
                play_truncated_notes_in_clips: false,
            },
            |_| 128,
            |channel_id, _, key| {
                assert_eq!(key, 72);
                if channel_id == 7 {
                    vec![2, 5]
                } else {
                    vec![channel_id]
                }
            },
        )
        .unwrap();
        assert_eq!(
            schedule
                .notes
                .iter()
                .map(|placed| (placed.target_channel_id, placed.start_tick))
                .collect::<Vec<_>>(),
            vec![(2, 120), (5, 120), (2, 216), (5, 216)]
        );
    }

    #[test]
    fn one_shot_only_pattern_without_explicit_length_does_not_repeat_per_tick() {
        let pattern = Pattern {
            id: 3,
            notes: vec![PatternNote {
                position: 12,
                length: 0,
                channel_id: 1,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let arrangement = Arrangement {
            clips: vec![test_pattern_clip(3, 0, 768)],
            ..Arrangement::default()
        };

        let patterns = [pattern];
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            PlaylistTrackFilter::new(&BTreeSet::new(), None),
            PlaylistPatternScheduleOptions {
                ppq: 96,
                global_swing_mix_raw: 0,
                play_truncated_notes_in_clips: false,
            },
            |_| 128,
            |channel_id, _, _| vec![channel_id],
        )
        .unwrap();
        assert_eq!(schedule.notes.len(), 1);
        assert_eq!(schedule.notes[0].start_tick, 12);
        assert_eq!(schedule.notes[0].stop_tick, None);
    }

    #[test]
    fn playlist_pattern_schedule_applies_global_and_source_channel_swing() {
        let pattern = Pattern {
            id: 4,
            length_ticks: Some(96),
            notes: vec![PatternNote {
                position: 24,
                length: 24,
                channel_id: 7,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let arrangement = Arrangement {
            clips: vec![test_pattern_clip(4, 12, 96)],
            ..Arrangement::default()
        };

        let patterns = [pattern];
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            PlaylistTrackFilter::new(&BTreeSet::new(), None),
            PlaylistPatternScheduleOptions {
                ppq: 96,
                global_swing_mix_raw: 128,
                play_truncated_notes_in_clips: false,
            },
            |channel_id| if channel_id == 7 { 128 } else { 0 },
            |channel_id, _, _| vec![channel_id],
        )
        .unwrap();
        assert_eq!(schedule.notes.len(), 1);
        assert_eq!(schedule.notes[0].start_tick, 44);
        assert_eq!(schedule.notes[0].stop_tick, Some(68));
    }

    #[test]
    fn play_truncated_notes_in_clips_controls_pattern_note_tail() {
        let pattern = Pattern {
            id: 9,
            length_ticks: Some(96),
            notes: vec![PatternNote {
                position: 48,
                length: 96,
                channel_id: 1,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let arrangement = Arrangement {
            clips: vec![test_pattern_clip(9, 0, 96)],
            ..Arrangement::default()
        };
        let patterns = [pattern];

        let schedule = |play_truncated_notes_in_clips| {
            schedule_playlist_pattern_notes(
                &patterns,
                &arrangement,
                PlaylistTrackFilter::new(&BTreeSet::new(), None),
                PlaylistPatternScheduleOptions {
                    ppq: 96,
                    global_swing_mix_raw: 0,
                    play_truncated_notes_in_clips,
                },
                |_| 128,
                |channel_id, _, _| vec![channel_id],
            )
            .unwrap()
        };

        assert_eq!(schedule(false).notes[0].stop_tick, Some(96));
        assert_eq!(schedule(true).notes[0].stop_tick, Some(144));
    }

    #[test]
    fn playlist_stream_mixes_audio_clips_and_sampler_voices_in_each_block() {
        let path = PathBuf::from("source.wav");
        let audio = PreparedAudioClipRender {
            summary: AudioClipRenderSummary {
                frames: 2,
                sample_rate: 4,
                clips_rendered: 1,
                clips_skipped_unsupported_scale: 0,
                source_files: 1,
            },
            resampling_quality: ResamplingQuality::Linear,
            decoded_by_path: HashMap::from([(
                path.clone(),
                DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![1.0, 3.0], vec![2.0, 4.0]],
                },
            )]),
            clips: vec![PreparedClip {
                clip_index: 0,
                channel_id: 4,
                mixer_track: None,
                path,
                start_frame: 0,
                source_bounds: SampleBounds { start: 0, end: 2 },
                pitch_ratio: 1.0,
                duration_frames: 2,
                gain: 0.5,
                pan: 0.0,
                pan_law: PanLaw::Circular,
                mixer_transform: MixerSourceRouteTransform::from_insert_transform(
                    MixerInsertSignalTransform::default().with_fader_volume_raw(Some(15_000)),
                ),
                mixer_meter_transforms: vec![
                    (1, MixerSourceRouteTransform::default()),
                    (
                        2,
                        MixerSourceRouteTransform::from_insert_transform(
                            MixerInsertSignalTransform::default()
                                .with_fader_volume_raw(Some(5_000)),
                        ),
                    ),
                ],
            }],
        };
        let sampler = PreparedSamplerArrangement {
            summary: SamplerPatternRenderSummary {
                frames: 2,
                sample_rate: 4,
                notes_rendered: 1,
                voices_stolen: 0,
                sampler_channels_rendered: 1,
                source_files: 1,
                notes_skipped_unresolved_sample: 0,
                unresolved_sample_channels: Vec::new(),
            },
            pan_law: PanLaw::Circular,
            notes: vec![ScheduledSamplerNote {
                start_frame: 0,
                stop_frame: None,
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
            }],
            sources_by_channel: HashMap::from([(
                2,
                SamplerVoiceSource {
                    audio: Arc::new(DecodedAudio {
                        sample_rate: 4,
                        channels: vec![vec![2.0; 2]],
                    }),
                    gain: 1.0,
                    pan: -1.0,
                    reverse: false,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                },
            )]),
            mixer_transforms_by_channel: HashMap::from([(
                2,
                MixerSourceRouteTransform::from_insert_transform(
                    MixerInsertSignalTransform::default().with_fader_volume_raw(Some(15_000)),
                ),
            )]),
            mixer_meter_transforms_by_channel: HashMap::from([(
                2,
                vec![
                    (1, MixerSourceRouteTransform::default()),
                    (
                        2,
                        MixerSourceRouteTransform::from_insert_transform(
                            MixerInsertSignalTransform::default()
                                .with_fader_volume_raw(Some(5_000)),
                        ),
                    ),
                ],
            )]),
            pattern_clips_rendered: 1,
            pattern_clips_skipped_unsupported_scale: 0,
            source_paths: std::collections::BTreeSet::new(),
        };
        let mut rendered = Vec::new();

        let meters = MixerTrackPeakMeters::default();
        let voices_stolen = stream_prepared_playlist_render(
            PreparedPlaylistBlockMix {
                audio: &audio,
                sampler: &sampler,
                options: PlaylistRenderOptions {
                    arrangement_id: 0,
                    sample_rate: 4,
                    sampler_voice_limit: 4,
                    wav_sample_format: WavSampleFormat::Float32,
                    wav_dither_mode: WavDitherMode::Off,
                    resampling_quality: ResamplingQuality::Linear,
                    wav_channel_mode: WavChannelMode::Stereo,
                    wav_save_playlist_markers: false,
                    wav_save_note_markers: false,
                    tail_seconds: 0,
                    soloed_playlist_track_range: None,
                    read_sample_root_note: true,
                },
                frames: 2,
                master_output: MixerMasterOutput::default(),
                vst3_processor: None,
            },
            Some(&meters),
            &AtomicBool::new(false),
            || false,
            |block, _meter_buses| {
                rendered.extend_from_slice(block);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(voices_stolen, 0);
        assert_eq!(rendered, vec![3.75, 1.5, 5.25, 3.0]);
        assert_eq!(meters.take_peak(1), 3.5);
        assert_eq!(meters.take_peak(2), 1.75);
        assert_eq!(meters.take_peak(1), 0.0);
    }

    #[test]
    fn offline_mixer_peak_summary_keeps_non_master_signal_peaks() {
        let meters = MixerTrackPeakMeters::default();
        meters.publish_peak(0, 2.0);
        meters.publish_peak(2, 0.75);
        meters.publish_peak(4, 1.5);

        let peaks = take_mixer_insert_peaks(&meters);

        assert_eq!(
            peaks,
            vec![
                MixerInsertPeak {
                    track: 2,
                    peak_bits: 0.75_f32.to_bits(),
                },
                MixerInsertPeak {
                    track: 4,
                    peak_bits: 1.5_f32.to_bits(),
                },
            ]
        );
        assert_eq!(peaks[0].track(), 2);
        assert_eq!(peaks[0].peak(), 0.75);
        assert_eq!(peaks[1].track(), 4);
        assert_eq!(peaks[1].peak(), 1.5);
    }

    #[test]
    fn cancellation_is_observed_before_render_work() {
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            check_cancelled(Some(&cancelled)),
            Err("audio render cancelled".to_owned())
        );
    }

    #[test]
    fn wav_playlist_markers_write_standard_cue_points_and_labels() {
        let root = std::env::temp_dir().join(format!(
            "flp-rebuild-wav-cues-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("temporary test folder should be created");
        let path = root.join("markers.wav");
        let mut file = File::create(&path).expect("temporary WAV should be created");
        write_wav_header(
            &mut file,
            44_100,
            2,
            8,
            WavSampleFormat::Pcm16,
            WavChannelMode::Stereo,
        )
        .unwrap();
        let original_audio_data = [1, 2, 3, 4, 5, 6, 7, 8];
        file.write_all(&original_audio_data).unwrap();
        write_wav_cue_points(
            &mut file,
            &[
                WavCuePoint {
                    identifier: 1,
                    sample_offset: 1,
                    label: "Intro".to_owned(),
                },
                WavCuePoint {
                    identifier: 2,
                    sample_offset: 2,
                    label: "Verse 2".to_owned(),
                },
            ],
        )
        .unwrap();
        drop(file);

        let bytes = fs::read(&path).expect("rendered WAV should be readable");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(&bytes[44..52], &original_audio_data);
        assert_eq!(&bytes[52..56], b"cue ");
        assert_eq!(u32::from_le_bytes(bytes[56..60].try_into().unwrap()), 52);
        assert_eq!(u32::from_le_bytes(bytes[60..64].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[64..68].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[68..72].try_into().unwrap()), 1);
        assert_eq!(&bytes[72..76], b"data");
        assert_eq!(u32::from_le_bytes(bytes[84..88].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[88..92].try_into().unwrap()), 2);
        assert_eq!(&bytes[112..116], b"LIST");
        assert_eq!(&bytes[120..124], b"adtl");
        assert_eq!(&bytes[124..128], b"labl");
        assert_eq!(u32::from_le_bytes(bytes[132..136].try_into().unwrap()), 1);
        assert_eq!(&bytes[136..142], b"Intro\0");
        assert_eq!(&bytes[142..146], b"labl");
        assert_eq!(u32::from_le_bytes(bytes[150..154].try_into().unwrap()), 2);
        assert_eq!(&bytes[154..162], b"Verse 2\0");

        fs::remove_dir_all(root).expect("temporary test folder should be removed");
    }

    #[test]
    fn wav_note_markers_map_note_onsets_and_audio_clip_starts_to_frames() {
        let tempo_map = PlaylistTempoMap {
            ppq: 96,
            segments: vec![TempoSegment {
                start_tick: 0,
                start_seconds: 0.0,
                tempo_bpm: 120.0,
            }],
        };
        let mut audio_clip = test_pattern_clip(0, 24, 96);
        audio_clip.item_index = 2;
        let arrangement = Arrangement {
            id: 0,
            clips: vec![audio_clip, test_pattern_clip(5, 0, 192)],
            ..Arrangement::default()
        };
        let pattern = Pattern {
            id: 5,
            notes: vec![PatternNote {
                position: 0,
                length: 24,
                channel_id: 3,
                ..PatternNote::default()
            }],
            ..Pattern::default()
        };
        let schedule = PlaylistPatternSchedule {
            notes: vec![PlaylistPatternNote {
                note: &pattern.notes[0],
                target_channel_id: 3,
                start_tick: 96,
                stop_tick: Some(120),
                clip_index: 1,
            }],
            clips_skipped_unsupported_scale: 0,
        };
        let mut cue_points = Vec::new();
        append_wav_note_cue_points(
            &mut cue_points,
            &arrangement,
            &tempo_map,
            9_600,
            PlaylistTrackFilter::new(&BTreeSet::new(), None),
            &BTreeSet::from([2]),
            &schedule,
        )
        .unwrap();

        assert_eq!(
            cue_points
                .iter()
                .map(|point| (point.identifier, point.sample_offset, point.label.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, 1_200, "Audio Clip"), (2, 4_800, "Note")]
        );
    }

    #[test]
    fn cancellable_buffer_render_exits_before_project_work() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FLhd");
        bytes.extend_from_slice(&6_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&96_u16.to_le_bytes());
        bytes.extend_from_slice(b"FLdt");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        let document = FlpDocument::parse(&bytes).expect("the empty fixture should parse");
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            render_audio_clips_to_stereo_buffer_cancellable(
                &document,
                ".",
                AudioClipRenderOptions::default(),
                &cancelled,
            )
            .unwrap_err(),
            "audio render cancelled"
        );
    }

    #[test]
    fn parallel_mixer_track_audio_matches_serial_audio_and_meter_buses() {
        let path = PathBuf::from("parallel-source.wav");
        let source = DecodedAudio {
            sample_rate: 8,
            channels: vec![
                vec![0.25, -0.5, 0.75, -1.0, 0.5, -0.25, 1.0, -0.75],
                vec![-0.5, 0.25, -0.75, 0.5, -1.0, 0.75, -0.25, 1.0],
            ],
        };
        let clips = [
            (0, 1, 0, 0.75),
            (1, 1, 2, 0.5),
            (2, 2, 0, 0.25),
            (3, 2, 4, 0.625),
        ]
        .into_iter()
        .map(|(clip_index, mixer_track, start_frame, gain)| {
            let transform = MixerSourceRouteTransform::from_insert_transform(
                MixerInsertSignalTransform::default()
                    .with_fader_volume_raw(Some(if mixer_track == 1 { 8_000 } else { 6_000 })),
            );
            PreparedClip {
                clip_index,
                channel_id: u16::try_from(clip_index).unwrap(),
                mixer_track: Some(mixer_track),
                path: path.clone(),
                start_frame,
                source_bounds: SampleBounds { start: 0, end: 8 },
                pitch_ratio: 1.0,
                duration_frames: 8,
                gain,
                pan: 0.0,
                pan_law: PanLaw::Circular,
                mixer_transform: transform,
                mixer_meter_transforms: vec![(usize::try_from(mixer_track).unwrap(), transform)],
            }
        })
        .collect::<Vec<_>>();
        let decoded_by_path = HashMap::from([(path, source)]);
        let cancelled = AtomicBool::new(false);
        let audio_mix_context = PreparedAudioClipMixContext {
            clips: &clips,
            decoded_by_path: &decoded_by_path,
            sample_rate: 8,
            resampling_quality: ResamplingQuality::Linear,
            cancelled: &cancelled,
        };
        let mut serial = vec![0.0; 16];
        let mut serial_meters = vec![0.0; MIXER_TRACK_METER_COUNT * 16];
        mix_prepared_audio_clip_block(
            &audio_mix_context,
            0,
            &mut serial,
            Some(&mut serial_meters),
            None,
        )
        .unwrap();

        let track_groups = audio_clips_grouped_by_mixer_track(&clips);
        assert_eq!(track_groups.len(), 2);
        let mut parallel = vec![0.0; 16];
        let mut parallel_meters = vec![0.0; MIXER_TRACK_METER_COUNT * 16];
        std::thread::scope(|scope| {
            let worker_clips = audio_clip_groups_for_workers(&track_groups, 2);
            let mut mixer = ParallelAudioClipMixer::new(
                scope,
                worker_clips,
                &decoded_by_path,
                8,
                ResamplingQuality::Linear,
                &cancelled,
                true,
            )
            .expect("two source mixer tracks should start two workers");
            mix_prepared_audio_clip_block(
                &audio_mix_context,
                0,
                &mut parallel,
                Some(&mut parallel_meters),
                Some(&mixer),
            )
            .unwrap();
            mixer.stop_and_join().unwrap();
        });

        for (parallel_sample, serial_sample) in parallel.iter().zip(&serial) {
            assert!((parallel_sample - serial_sample).abs() < 1.0e-5);
        }
        for (parallel_sample, serial_sample) in parallel_meters.iter().zip(&serial_meters) {
            assert!((parallel_sample - serial_sample).abs() < 1.0e-5);
        }
    }

    #[test]
    fn mixes_mono_clips_to_both_stereo_channels_with_resampling() {
        let source = DecodedAudio {
            sample_rate: 2,
            channels: vec![vec![0.0, 1.0]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            channel_id: 0,
            mixer_track: None,
            path: PathBuf::new(),
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 2 },
            pitch_ratio: 1.0,
            duration_frames: 4,
            gain: 1.0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            mixer_meter_transforms: Vec::new(),
        };
        let mut mix = vec![0.0; 8];
        mix_clip_into_stereo(&mut mix, &source, &clip, 4, None).unwrap();
        let center_gain = std::f32::consts::FRAC_1_SQRT_2;
        let expected = [
            0.0,
            0.0,
            0.5 * center_gain,
            0.5 * center_gain,
            center_gain,
            center_gain,
            center_gain,
            center_gain,
        ];
        for (actual, expected) in mix.iter().zip(expected) {
            assert!((actual - expected).abs() < 1.0e-6);
        }
    }

    #[test]
    fn playlist_audio_clip_master_pitch_changes_playback_rate_and_duration() {
        let pitch_up = audio_clip_master_pitch_ratio(1_200);
        let pitch_down = audio_clip_master_pitch_ratio(-1_200);
        assert_eq!(pitch_up, 2.0);
        assert_eq!(pitch_down, 0.5);
        assert_eq!(audio_clip_master_pitch_ratio(0), 1.0);
        assert_eq!(source_duration_to_frames(4, 4, 4, pitch_up).unwrap(), 2);
        assert_eq!(source_duration_to_frames(4, 4, 4, pitch_down).unwrap(), 8);

        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![0.0, 1.0, 2.0, 3.0]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            channel_id: 0,
            mixer_track: None,
            path: PathBuf::new(),
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 4 },
            pitch_ratio: pitch_up,
            duration_frames: 2,
            gain: 1.0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            mixer_meter_transforms: Vec::new(),
        };
        let mut mix = vec![0.0; 4];
        mix_clip_into_stereo(&mut mix, &source, &clip, 4, None).unwrap();
        let center_gain = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(mix, vec![0.0, 0.0, 2.0 * center_gain, 2.0 * center_gain]);
    }

    #[test]
    fn mixer_insert_transform_swaps_and_reverses_audio_clip_frames() {
        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![0.25], vec![0.75]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            channel_id: 1,
            mixer_track: None,
            path: PathBuf::new(),
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 1 },
            pitch_ratio: 1.0,
            duration_frames: 1,
            gain: 1.0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::from_insert_transform(
                MixerInsertSignalTransform::new(true, true),
            ),
            mixer_meter_transforms: Vec::new(),
        };
        let mut mix = vec![0.0; 2];

        mix_clip_into_stereo(&mut mix, &source, &clip, 4, None).unwrap();

        assert_eq!(mix, vec![-0.75, -0.25]);
    }

    #[test]
    fn block_stream_mix_matches_offline_mix_with_channel_levels() {
        let path = PathBuf::from("source.wav");
        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![
                vec![1.0, 3.0, 5.0, 7.0, 9.0],
                vec![2.0, 4.0, 6.0, 8.0, 10.0],
            ],
        };
        let clip = PreparedClip {
            clip_index: 0,
            channel_id: 3,
            mixer_track: None,
            path: path.clone(),
            start_frame: 2,
            source_bounds: SampleBounds { start: 0, end: 5 },
            pitch_ratio: 1.0,
            duration_frames: 5,
            gain: 0.5,
            pan: 0.5,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            mixer_meter_transforms: Vec::new(),
        };
        let render = PreparedAudioClipRender {
            summary: AudioClipRenderSummary {
                frames: 9,
                sample_rate: 4,
                clips_rendered: 1,
                clips_skipped_unsupported_scale: 0,
                source_files: 1,
            },
            resampling_quality: ResamplingQuality::Linear,
            decoded_by_path: HashMap::from([(path, source.clone())]),
            clips: vec![clip.clone()],
        };
        let mut offline = vec![0.0; render.summary.frames as usize * 2];
        mix_clip_into_stereo(&mut offline, &source, &clip, 4, None).unwrap();

        let mut streamed = Vec::new();
        stream_prepared_audio_clip_render(
            &render,
            4,
            2,
            &AtomicBool::new(false),
            || false,
            |block| {
                streamed.extend_from_slice(block);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(streamed, offline);
        assert_eq!(
            streamed,
            vec![
                0.0, 0.0, 0.0, 0.0, 0.25, 1.0, 0.75, 2.0, 1.25, 3.0, 1.75, 4.0, 2.25, 5.0, 0.0,
                0.0, 0.0, 0.0
            ]
        );
    }

    #[test]
    fn channel_volume_and_pan_use_fl_default_values_as_unity_and_center() {
        assert_eq!(channel_gain_pan(Some(10_000), Some(6_400)), (1.0, 0.0));
        assert_eq!(channel_gain_pan(Some(5_000), Some(0)), (0.5, -1.0));
        assert_eq!(channel_gain_pan(Some(12_800), Some(12_800)), (1.28, 1.0));
        assert_eq!(channel_gain_pan(None, None), (1.0, 0.0));
    }

    #[test]
    fn mono_pan_gain_curves_match_circular_and_triangular_project_laws() {
        let circular_center = channel_pan_gains(0.0, true, PanLaw::Circular);
        let equal_power = std::f32::consts::FRAC_1_SQRT_2;
        assert!((circular_center.0 - equal_power).abs() < 1.0e-6);
        assert!((circular_center.1 - equal_power).abs() < 1.0e-6);

        assert_eq!(channel_pan_gains(0.0, true, PanLaw::Triangular), (0.5, 0.5));
        assert_eq!(
            channel_pan_gains(-1.0, true, PanLaw::Triangular),
            (1.0, 0.0)
        );
        assert_eq!(channel_pan_gains(1.0, true, PanLaw::Triangular), (0.0, 1.0));
    }

    #[test]
    fn stereo_balance_curve_is_independent_of_mono_pan_law() {
        assert_eq!(channel_pan_gains(-0.5, false, PanLaw::Circular), (1.0, 0.5));
        assert_eq!(
            channel_pan_gains(-0.5, false, PanLaw::Triangular),
            (1.0, 0.5)
        );
    }

    #[test]
    fn sampler_note_pan_is_a_centered_offset_from_channel_pan() {
        assert_eq!(sampler_note_pan(0.0, 64), 0.0);
        assert_eq!(sampler_note_pan(0.0, 0), -1.0);
        assert_eq!(sampler_note_pan(0.0, 128), 1.0);
        assert_eq!(sampler_note_pan(-0.5, 96), 0.0);
        assert_eq!(sampler_note_pan(0.5, 128), 1.0);
        assert_eq!(sampler_note_pan(-0.5, 0), -1.0);
    }

    #[test]
    fn sampler_voice_starts_at_exact_frame_and_renders_identically_across_blocks() {
        let source = Arc::new(DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![1.0; 16]],
        });
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: source,
                gain: 1.0,
                pan: 0.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 1,
            stop_frame: Some(3),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];

        let mut whole_engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 4);
        let mut whole = vec![0.0; 16];
        whole_engine.render_block(0, 8, &mut whole);

        let mut split_engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 4);
        let mut split = vec![0.0; 16];
        split_engine.render_block(0, 2, &mut split[..4]);
        split_engine.render_block(2, 6, &mut split[4..]);

        assert_eq!(whole, split);
        assert_eq!(&whole[..2], &[0.0, 0.0]);
        assert!((whole[2] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((whole[3] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert_eq!(&whole[14..], &[0.0, 0.0]);
    }

    #[test]
    fn sampler_voice_applies_each_note_pan_through_the_project_pan_law() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![1.0; 4]],
                }),
                gain: 1.0,
                pan: 0.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [
            ScheduledSamplerNote {
                start_frame: 0,
                stop_frame: None,
                channel_id: 7,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
                pan: 96,
                fine_pitch_offset_cents: 0,
            },
            ScheduledSamplerNote {
                start_frame: 4,
                stop_frame: None,
                channel_id: 7,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
                pan: 32,
                fine_pitch_offset_cents: 0,
            },
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 16];

        engine.render_block(0, 8, &mut output);

        let right_half = channel_pan_gains(0.5, true, PanLaw::Circular);
        let left_half = channel_pan_gains(-0.5, true, PanLaw::Circular);
        for (first_frame, expected) in [(0, right_half), (4, left_half)] {
            for frame_index in first_frame..first_frame + 4 {
                let sample_index = frame_index * 2;
                assert!((output[sample_index] - expected.0).abs() < 1.0e-6);
                assert!((output[sample_index + 1] - expected.1).abs() < 1.0e-6);
            }
        }
    }

    #[test]
    fn sampler_voice_applies_its_insert_phase_transform_after_channel_pan() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![0.5]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let transforms = HashMap::from([(
            7,
            MixerSourceRouteTransform::from_insert_transform(MixerInsertSignalTransform::new(
                true, false,
            )),
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        engine.mixer_transforms_by_channel = Some(&transforms);
        let mut output = vec![0.0; 2];

        engine.render_block(0, 1, &mut output);

        assert_eq!(output, vec![-0.5, 0.0]);
    }

    #[test]
    fn sampler_voice_plays_reversed_sample_from_end_to_start() {
        let source = Arc::new(DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![1.0, 2.0, 3.0, 4.0]],
        });
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: source,
                gain: 1.0,
                pan: -1.0,
                reverse: true,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 8];
        engine.render_block(0, 4, &mut output);

        assert_eq!(output, vec![4.0, 0.0, 3.0, 0.0, 2.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn held_sampler_voice_wraps_forward_at_wave_loop_points() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![0.0, 1.0, 2.0, 3.0, 4.0]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: Some(SampleBounds { start: 1, end: 4 }),
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 18];
        engine.render_block(0, 9, &mut output);

        assert_eq!(
            output,
            vec![
                0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0, 1.0, 0.0,
                2.0, 0.0,
            ]
        );
    }

    #[test]
    fn held_sampler_voice_bounces_between_wave_loop_points_in_ping_pong_mode() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![0.0, 1.0, 2.0, 3.0, 4.0]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: Some(SampleBounds { start: 1, end: 4 }),
                ping_pong_loop: true,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 18];
        engine.render_block(0, 9, &mut output);

        assert_eq!(
            output,
            vec![
                0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0, 2.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0,
                2.0, 0.0,
            ]
        );
    }

    #[test]
    fn sampler_voice_resamples_at_output_rate_and_transposes_by_note_key() {
        let source = Arc::new(DecodedAudio {
            sample_rate: 8,
            channels: vec![(0..16).map(|frame| frame as f32).collect()],
        });
        let sources = HashMap::from([(
            2,
            SamplerVoiceSource {
                audio: source,
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 2,
            key: SAMPLER_ROOT_KEY + 12,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 8];
        engine.render_block(0, 4, &mut output);

        assert_eq!(output[2], 4.0);
        assert_eq!(output[4], 8.0);
        assert_eq!(output[6], 12.0);
    }

    #[test]
    fn sampler_voice_transposes_from_the_saved_channel_root_key() {
        let sources = HashMap::from([(
            2,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![(0..8).map(|frame| frame as f32).collect()],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                root_key: 48,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 2,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 8];
        engine.render_block(0, 4, &mut output);

        assert_eq!(output, vec![0.0, 0.0, 2.0, 0.0, 4.0, 0.0, 6.0, 0.0]);
    }

    #[test]
    fn sampler_root_key_prefers_saved_channel_value_then_wave_metadata_then_c5() {
        assert_eq!(resolve_sampler_root_key(Some(48), Some(72), true), 48);
        assert_eq!(resolve_sampler_root_key(None, Some(72), true), 72);
        assert_eq!(
            resolve_sampler_root_key(None, Some(72), false),
            SAMPLER_ROOT_KEY
        );
        assert_eq!(resolve_sampler_root_key(None, None, true), SAMPLER_ROOT_KEY);
    }

    #[test]
    fn sampler_wav_gain_converts_decibels_to_linear_amplitude() {
        assert_eq!(sampler_wav_gain(None), 1.0);
        assert_eq!(sampler_wav_gain(Some(0)), 1.0);
        assert!((sampler_wav_gain(Some(-6)) - 10.0f32.powf(-6.0 / 20.0)).abs() < 1e-6);
        assert_eq!(sampler_wav_gain(Some(20)), 10.0);
    }

    #[test]
    fn sampler_source_step_applies_wav_fine_tuning_cents() {
        let untuned = sampler_source_step(48_000, 48_000, 60, 60, 0.0);
        let half_semitone_up = sampler_source_step(48_000, 48_000, 60, 60, 50.0);
        let half_semitone_down = sampler_source_step(48_000, 48_000, 60, 60, -50.0);

        assert_eq!(untuned, 1.0);
        assert!((half_semitone_up - 2.0f64.powf(0.5 / 12.0)).abs() < 1e-12);
        assert!((half_semitone_down - 2.0f64.powf(-0.5 / 12.0)).abs() < 1e-12);
    }

    #[test]
    fn sampler_voice_applies_scheduled_note_fine_pitch() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![1.0; 8]],
                }),
                gain: 1.0,
                pan: 0.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 50,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);

        engine.render_block(0, 1, &mut [0.0; 2]);

        let voice = engine.voices[0].as_ref().expect("note should start");
        assert!((voice.source_step - 2.0f64.powf(50.0 / 1_200.0)).abs() < 1e-12);
    }

    #[test]
    fn zero_length_sampler_note_plays_the_sample_to_its_end() {
        let source = Arc::new(DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![0.25; 4]],
        });
        let sources = HashMap::from([(
            3,
            SamplerVoiceSource {
                audio: source,
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 2,
            stop_frame: None,
            channel_id: 3,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 12];
        engine.render_block(0, 6, &mut output);

        assert_eq!(&output[..4], &[0.0; 4]);
        assert_eq!(
            &output[4..12],
            &[0.25, 0.0, 0.25, 0.0, 0.25, 0.0, 0.25, 0.0]
        );
    }

    #[test]
    fn sampler_voice_pool_steals_the_oldest_active_voice_at_its_limit() {
        let source_a = Arc::new(DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![1.0; 16]],
        });
        let source_b = Arc::new(DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![2.0; 16]],
        });
        let sources = HashMap::from([
            (
                1,
                SamplerVoiceSource {
                    audio: source_a,
                    gain: 1.0,
                    pan: -1.0,
                    reverse: false,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                },
            ),
            (
                2,
                SamplerVoiceSource {
                    audio: source_b,
                    gain: 1.0,
                    pan: -1.0,
                    reverse: false,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                },
            ),
        ]);
        let notes = [
            ScheduledSamplerNote {
                start_frame: 0,
                stop_frame: Some(12),
                channel_id: 1,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
            },
            ScheduledSamplerNote {
                start_frame: 1,
                stop_frame: Some(12),
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
            },
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 8];
        engine.render_block(0, 4, &mut output);

        assert_eq!(engine.voices_stolen, 1);
        assert_eq!(output[0], 1.0);
        assert_eq!(output[2], 2.0);
        assert_eq!(output[3], 0.0);
    }

    #[test]
    fn selects_full_source_for_default_offsets_and_milliseconds_for_slices() {
        let source = DecodedAudio {
            sample_rate: 1_000,
            channels: vec![vec![0.0; 2_000]],
        };
        let mut clip = PlaylistClip {
            position_ticks: 0,
            pattern_base: 0,
            item_index: 0,
            length_ticks: 0,
            raw_track_index: 0,
            track_index: None,
            group: 0,
            unknown_word: 0,
            item_flags: 0,
            muted: false,
            header_bytes: [0; 4],
            start_offset: -1.0,
            end_offset: -1.0,
            source_window_ticks: None,
            clip_id: None,
            reserved: Vec::new(),
            scale: None,
            trailing_bytes: Vec::new(),
            record_size: 0,
            source_event_index: 0,
            source_record_index: 0,
        };
        assert_eq!(
            sample_source_bounds(&clip, &source, 0).unwrap(),
            SampleBounds {
                start: 0,
                end: 2_000
            }
        );
        clip.start_offset = 500.0;
        clip.end_offset = 1_250.0;
        assert_eq!(
            sample_source_bounds(&clip, &source, 0).unwrap(),
            SampleBounds {
                start: 500,
                end: 1_250
            }
        );
    }

    #[test]
    fn writes_float_stereo_wav_and_protects_the_project_path() {
        let root = std::env::temp_dir().join(format!(
            "flp-sample-render-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let project = root.join("song.wav");
        let output = root.join("mix.wav");
        fs::write(&project, b"FLP project fixture").unwrap();

        assert!(validate_output_path(&project, &project).is_err());
        assert!(validate_output_path(&project, &root.join("mix.mp3")).is_err());
        write_stereo_wav_from_buffer(
            &output,
            &[0.25, -0.5],
            48_000,
            1,
            WavSampleFormat::Float32,
            WavDitherMode::Off,
            WavChannelMode::Stereo,
        )
        .unwrap();

        let bytes = fs::read(&output).unwrap();
        assert_eq!(bytes.len(), 52);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 44);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(f32::from_le_bytes(bytes[44..48].try_into().unwrap()), 0.25);
        assert_eq!(f32::from_le_bytes(bytes[48..52].try_into().unwrap()), -0.5);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mixer_stem_filename_components_are_safe_and_bounded() {
        assert_eq!(
            safe_stem_filename_component("Lead/Vox: take?"),
            "Lead_Vox_ take_"
        );
        assert_eq!(safe_stem_filename_component("...   "), "");
        let long_name = "界".repeat(80);
        assert!(safe_stem_filename_component(&long_name).len() <= 120);
    }

    #[test]
    fn current_mixer_stem_targets_the_selected_unmuted_track() {
        let current = mixer_current_stem_spec(7, Some(true)).unwrap();
        assert_eq!(current.track, 7);
        assert_eq!(current.label, "Current");
        assert!(!current.is_master_stem);
        assert!(current.is_current_stem);
        assert!(mixer_current_stem_spec(7, Some(false)).is_err());
        assert!(mixer_current_stem_spec(MIXER_TRACK_METER_COUNT, None).is_err());
    }

    #[test]
    fn mixer_stem_block_reads_master_mix_and_insert_meter_bus() {
        let master = [0.25_f32, -0.25, 0.5, -0.5];
        let mut insert_buses = vec![0.0_f32; MIXER_TRACK_METER_COUNT * master.len()];
        insert_buses[master.len()..2 * master.len()].copy_from_slice(&[0.125, -0.125, 0.75, -0.75]);

        assert_eq!(
            mixer_stem_block(0, &master, &insert_buses).unwrap(),
            master.as_slice()
        );
        assert_eq!(
            mixer_stem_block(1, &master, &insert_buses).unwrap(),
            &[0.125, -0.125, 0.75, -0.75][..]
        );
        assert!(mixer_stem_block(MIXER_TRACK_METER_COUNT, &master, &insert_buses).is_err());
    }

    #[test]
    fn writes_captured_input_to_float_stereo_wav() {
        let root = std::env::temp_dir().join(format!(
            "flp-input-recording-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let output = root.join("take.wav");
        let recording = AudioInputRecording::from_test_frames(48_000, &[[0.25, -0.5], [-1.0, 1.0]]);

        let summary = write_input_recording_to_wav(&recording, &output).unwrap();
        let bytes = fs::read(&output).unwrap();
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.sample_rate, 48_000);
        assert_eq!(summary.overflow_frames, 0);
        assert_eq!(bytes.len(), 60);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 52);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        assert_eq!(f32::from_le_bytes(bytes[44..48].try_into().unwrap()), 0.25);
        assert_eq!(f32::from_le_bytes(bytes[48..52].try_into().unwrap()), -0.5);
        assert_eq!(f32::from_le_bytes(bytes[52..56].try_into().unwrap()), -1.0);
        assert_eq!(f32::from_le_bytes(bytes[56..60].try_into().unwrap()), 1.0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_playlist_wav_formats_with_correct_headers_and_sample_data() {
        let root = std::env::temp_dir().join(format!(
            "flp-pcm-render-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();

        let cases: [(WavSampleFormat, u16, u16, u16, &[u8]); 3] = [
            (
                WavSampleFormat::Pcm16,
                1,
                16,
                4,
                &[0x00, 0x80, 0xff, 0x7f, 0x00, 0x40, 0x00, 0xc0],
            ),
            (
                WavSampleFormat::Pcm24,
                1,
                24,
                6,
                &[
                    0x00, 0x00, 0x80, 0xff, 0xff, 0x7f, 0x00, 0x00, 0x40, 0x00, 0x00, 0xc0,
                ],
            ),
            (
                WavSampleFormat::Float32,
                3,
                32,
                8,
                &[
                    0x00, 0x00, 0x80, 0xbf, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0x3f, 0x00,
                    0x00, 0x00, 0xbf,
                ],
            ),
        ];

        for (index, (format, format_code, bits_per_sample, block_align, expected_samples)) in
            cases.into_iter().enumerate()
        {
            let output = root.join(format!("pcm-{index}.wav"));
            let mut file = File::create(&output).unwrap();
            let mut samples = Vec::new();
            for sample in [-1.0, 1.0, 0.5, -0.5] {
                append_wav_sample(sample, format, &mut samples);
            }
            write_wav_header(
                &mut file,
                48_000,
                2,
                samples.len() as u32,
                format,
                WavChannelMode::Stereo,
            )
            .unwrap();
            file.write_all(&samples).unwrap();
            drop(file);

            let bytes = fs::read(&output).unwrap();
            assert_eq!(
                u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                bytes.len() as u32 - 8
            );
            assert_eq!(
                u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
                format_code
            );
            assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
            assert_eq!(
                u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
                48_000 * u32::from(block_align)
            );
            assert_eq!(
                u16::from_le_bytes(bytes[32..34].try_into().unwrap()),
                block_align
            );
            assert_eq!(
                u16::from_le_bytes(bytes[34..36].try_into().unwrap()),
                bits_per_sample
            );
            assert_eq!(
                u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
                samples.len() as u32
            );
            assert_eq!(&bytes[44..], expected_samples);
        }

        assert_eq!(
            quantize_signed_pcm(f32::NEG_INFINITY, 32_768.0, 32_767.0),
            -32_768
        );
        assert_eq!(
            quantize_signed_pcm(f32::INFINITY, 32_768.0, 32_767.0),
            32_767
        );
        assert_eq!(quantize_signed_pcm(f32::NAN, 32_768.0, 32_767.0), 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn streams_decodable_ogg_blocks_for_each_channel_mode() {
        let root = std::env::temp_dir().join(format!(
            "flp-ogg-render-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();

        for (index, (channel_mode, expected_channels)) in [
            (WavChannelMode::Stereo, 2),
            (WavChannelMode::MonoMerged, 1),
            (WavChannelMode::MonoLeft, 1),
            (WavChannelMode::MonoRight, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let output = root.join(format!("stream-{index}.ogg"));
            let mut file = File::create(&output).unwrap();
            let mut writer = StreamingOggWriter::new(&mut file, 48_000, channel_mode, 192).unwrap();
            for block_index in 0..48 {
                let mut stereo_block = Vec::with_capacity(STREAM_BLOCK_FRAMES * 2);
                for frame_index in 0..STREAM_BLOCK_FRAMES {
                    let sample_index = block_index * STREAM_BLOCK_FRAMES + frame_index;
                    let phase = std::f32::consts::TAU * sample_index as f32 / 96.0;
                    stereo_block.extend_from_slice(&[phase.sin() * 0.25, phase.cos() * 0.25]);
                }
                writer.write_stereo_block(&stereo_block).unwrap();
            }
            writer.finalize().unwrap();
            drop(file);

            let mut source = File::open(&output).unwrap();
            let mut decoder = VorbisDecoder::new(&mut source).unwrap();
            assert_eq!(decoder.sampling_frequency().get(), 48_000);
            assert_eq!(usize::from(decoder.channels().get()), expected_channels);
            let mut decoded_frames = 0;
            let mut contains_audio = false;
            while let Some(block) = decoder.decode_audio_block().unwrap() {
                let channels = block.samples();
                assert_eq!(channels.len(), expected_channels);
                assert!(channels.iter().all(|channel| !channel.is_empty()));
                decoded_frames += channels[0].len();
                contains_audio |= channels
                    .iter()
                    .flat_map(|channel| channel.iter())
                    .any(|sample| sample.abs() > 0.01);
            }
            assert!(decoded_frames > 0);
            assert!(contains_audio);
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn streams_decodable_mp3_blocks_for_each_channel_mode() {
        let root = std::env::temp_dir().join(format!(
            "flp-mp3-render-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();

        let dither_modes = [
            WavDitherMode::Off,
            WavDitherMode::Tpdf,
            WavDitherMode::HighPassTpdf,
        ];
        let channel_modes = [
            (WavChannelMode::Stereo, 2),
            (WavChannelMode::MonoMerged, 1),
            (WavChannelMode::MonoLeft, 1),
            (WavChannelMode::MonoRight, 1),
        ];
        for (dither_index, dither_mode) in dither_modes.into_iter().enumerate() {
            for (channel_index, (channel_mode, expected_channels)) in
                channel_modes.into_iter().enumerate()
            {
                let output = root.join(format!("stream-{dither_index}-{channel_index}.mp3"));
                let mut file = File::create(&output).unwrap();
                let mut writer =
                    StreamingMp3Writer::new(&mut file, 48_000, channel_mode, 192, dither_mode)
                        .unwrap();
                for block_index in 0..48 {
                    let mut stereo_block = Vec::with_capacity(STREAM_BLOCK_FRAMES * 2);
                    for frame_index in 0..STREAM_BLOCK_FRAMES {
                        let sample_index = block_index * STREAM_BLOCK_FRAMES + frame_index;
                        let phase = std::f32::consts::TAU * sample_index as f32 / 96.0;
                        stereo_block.extend_from_slice(&[phase.sin() * 0.25, phase.cos() * 0.25]);
                    }
                    writer.write_stereo_block(&stereo_block).unwrap();
                }
                writer.finalize().unwrap();
                drop(file);

                let decoded = decode_audio_file(&output).unwrap();
                assert_eq!(decoded.sample_rate, 48_000);
                assert_eq!(decoded.channels.len(), expected_channels);
                assert!(!decoded.channels[0].is_empty());
                assert!(
                    decoded
                        .channels
                        .iter()
                        .flatten()
                        .any(|sample| sample.abs() > 0.01)
                );
            }
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_decodable_buffer_formats_with_shared_channel_mode() {
        let root = std::env::temp_dir().join(format!(
            "flp-audio-buffer-format-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();

        let frames = FLAC_BLOCK_FRAMES * 2 + 137;
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let phase = std::f32::consts::TAU * frame as f32 / 96.0;
            samples.extend_from_slice(&[phase.sin() * 0.25, phase.cos() * 0.25]);
        }
        let cases = [
            (AudioRenderOutput::Wav, "wav"),
            (
                AudioRenderOutput::Flac {
                    bits_per_sample: 24,
                },
                "flac",
            ),
            (AudioRenderOutput::Ogg { bitrate_kbps: 192 }, "ogg"),
            (AudioRenderOutput::Mp3 { bitrate_kbps: 192 }, "mp3"),
        ];

        for (index, (output_format, extension)) in cases.into_iter().enumerate() {
            let output = root.join(format!("clip-{index}.{extension}"));
            write_audio_buffer_to_file(
                &output,
                &samples,
                frames as u64,
                AudioBufferEncodingSettings {
                    sample_rate: 48_000,
                    options: AudioRenderEncodingOptions {
                        wav_sample_format: WavSampleFormat::Pcm16,
                        wav_dither_mode: WavDitherMode::Off,
                        wav_channel_mode: WavChannelMode::MonoMerged,
                    },
                },
                output_format,
            )
            .unwrap();

            if matches!(output_format, AudioRenderOutput::Ogg { .. }) {
                let mut source = File::open(&output).unwrap();
                let mut decoder = VorbisDecoder::new(&mut source).unwrap();
                assert_eq!(decoder.sampling_frequency().get(), 48_000);
                assert_eq!(decoder.channels().get(), 1);
                let mut decoded_frames = 0;
                let mut contains_audio = false;
                while let Some(block) = decoder.decode_audio_block().unwrap() {
                    assert_eq!(block.samples().len(), 1);
                    decoded_frames += block.samples()[0].len();
                    contains_audio |= block.samples()[0].iter().any(|sample| sample.abs() > 0.01);
                }
                assert!(decoded_frames > 0);
                assert!(contains_audio);
            } else {
                let decoded = decode_audio_file(&output).unwrap();
                assert_eq!(decoded.sample_rate, 48_000);
                assert_eq!(decoded.channels.len(), 1);
                assert!(!decoded.channels[0].is_empty());
                assert!(decoded.channels[0].iter().any(|sample| sample.abs() > 0.01));
            }
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_audio_export_buffers_with_selected_depth_and_channel_mode() {
        let root = std::env::temp_dir().join(format!(
            "flp-audio-export-format-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();

        let cases = [
            (
                "stereo-pcm16.wav",
                WavSampleFormat::Pcm16,
                WavChannelMode::Stereo,
                2_u16,
                16_u16,
                4_u16,
                vec![0x00, 0x80, 0xff, 0x7f],
                vec![-1.0, 1.0],
            ),
            (
                "left-mono-pcm24.wav",
                WavSampleFormat::Pcm24,
                WavChannelMode::MonoLeft,
                1_u16,
                24_u16,
                3_u16,
                vec![0x00, 0x00, 0x80],
                vec![-1.0, 1.0],
            ),
            (
                "merged-mono-float.wav",
                WavSampleFormat::Float32,
                WavChannelMode::MonoMerged,
                1_u16,
                32_u16,
                4_u16,
                0.375_f32.to_le_bytes().to_vec(),
                vec![0.5, 0.25],
            ),
        ];

        for (
            file_name,
            sample_format,
            channel_mode,
            expected_channels,
            expected_bits,
            expected_block_align,
            expected_samples,
            samples,
        ) in cases
        {
            let output = root.join(file_name);
            write_stereo_wav_from_buffer(
                &output,
                &samples,
                48_000,
                1,
                sample_format,
                WavDitherMode::Off,
                channel_mode,
            )
            .unwrap();

            let bytes = fs::read(&output).unwrap();
            assert_eq!(
                u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
                expected_channels
            );
            assert_eq!(
                u16::from_le_bytes(bytes[32..34].try_into().unwrap()),
                expected_block_align
            );
            assert_eq!(
                u16::from_le_bytes(bytes[34..36].try_into().unwrap()),
                expected_bits
            );
            assert_eq!(
                u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
                expected_samples.len() as u32
            );
            assert_eq!(&bytes[44..], expected_samples);
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tpdf_dither_is_reproducible_and_limited_to_one_lsb_peak() {
        let mut first = TpdfDither::from_seed(0x1234_5678_9ABC_DEF0);
        let mut second = TpdfDither::from_seed(0x1234_5678_9ABC_DEF0);
        for _ in 0..1024 {
            let noise = first.next_lsb_noise();
            assert!((-1.0..=1.0).contains(&noise));
            assert_eq!(noise, second.next_lsb_noise());
        }
    }

    #[test]
    fn high_pass_tpdf_dither_is_reproducible_and_shapes_noise_per_channel() {
        let seed = 0x1234_5678_9ABC_DEF0;
        let mut first = WavDitherState::from_seed(WavDitherMode::HighPassTpdf, seed).unwrap();
        let mut second = WavDitherState::from_seed(WavDitherMode::HighPassTpdf, seed).unwrap();
        let mut previous = [0.0; 2];
        let mut adjacent_products = [0.0; 2];
        let mut powers = [0.0; 2];

        for _ in 0..16_384 {
            for channel in 0..2 {
                let noise = first.next_lsb_noise(channel);
                assert_eq!(noise, second.next_lsb_noise(channel));
                assert!((-1.95..=1.95).contains(&noise));
                adjacent_products[channel] += noise * previous[channel];
                powers[channel] += noise * noise;
                previous[channel] = noise;
            }
        }

        for channel in 0..2 {
            let adjacent_correlation = adjacent_products[channel] / powers[channel];
            assert!(
                adjacent_correlation < -0.4,
                "channel {channel} did not show the expected high-pass noise shape: {adjacent_correlation}"
            );
        }
    }

    #[test]
    fn mp3_dither_applies_a_sixteen_bit_stage_only_when_enabled() {
        let mut disabled = None;
        assert_eq!(
            quantize_mp3_input_sample(0.123_456, &mut disabled, 0),
            0.123_456
        );

        let mut enabled =
            WavDitherState::from_seed(WavDitherMode::HighPassTpdf, 0x1234_5678_9ABC_DEF0);
        let mut nonzero_dithered_samples = 0;
        for _ in 0..512 {
            let sample = quantize_mp3_input_sample(0.0, &mut enabled, 0);
            let quantized_level = sample * 32_768.0;
            assert_eq!(quantized_level.round(), quantized_level);
            assert!(quantized_level.abs() <= 2.0);
            if sample != 0.0 {
                nonzero_dithered_samples += 1;
            }
        }
        assert!(nonzero_dithered_samples > 0);
    }

    #[test]
    fn wav_dither_state_only_applies_to_sixteen_bit_pcm() {
        assert!(WavDitherState::new(WavDitherMode::HighPassTpdf, WavSampleFormat::Pcm16).is_some());
        assert!(WavDitherState::new(WavDitherMode::HighPassTpdf, WavSampleFormat::Pcm24).is_none());
        assert!(
            WavDitherState::new(WavDitherMode::HighPassTpdf, WavSampleFormat::Float32).is_none()
        );
        assert!(WavDitherState::new(WavDitherMode::Off, WavSampleFormat::Pcm16).is_none());
    }

    #[test]
    fn wav_dither_only_changes_sixteen_bit_pcm() {
        let mut undithered_pcm16 = Vec::new();
        let mut dithered_pcm16 = Vec::new();
        append_wav_sample(0.0, WavSampleFormat::Pcm16, &mut undithered_pcm16);
        append_wav_sample_with_dither(0.0, WavSampleFormat::Pcm16, 0.75, &mut dithered_pcm16);
        assert_eq!(undithered_pcm16, [0, 0]);
        assert_eq!(dithered_pcm16, [1, 0]);

        let mut undithered_pcm24 = Vec::new();
        let mut dithered_pcm24 = Vec::new();
        append_wav_sample(0.0, WavSampleFormat::Pcm24, &mut undithered_pcm24);
        append_wav_sample_with_dither(0.0, WavSampleFormat::Pcm24, 0.75, &mut dithered_pcm24);
        assert_eq!(dithered_pcm24, undithered_pcm24);

        let mut undithered_float = Vec::new();
        let mut dithered_float = Vec::new();
        append_wav_sample(0.0, WavSampleFormat::Float32, &mut undithered_float);
        append_wav_sample_with_dither(0.0, WavSampleFormat::Float32, 0.75, &mut dithered_float);
        assert_eq!(dithered_float, undithered_float);
    }

    #[test]
    fn writes_wav_channel_modes_with_correct_mono_samples_and_headers() {
        let root = std::env::temp_dir().join(format!(
            "flp-channel-render-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let stereo_block = [-0.5, 0.5, 0.25, 0.75];
        let cases: [(WavChannelMode, u16, &[f32]); 4] = [
            (WavChannelMode::Stereo, 2, &[-0.5, 0.5, 0.25, 0.75]),
            (WavChannelMode::MonoMerged, 1, &[0.0, 0.5]),
            (WavChannelMode::MonoLeft, 1, &[-0.5, 0.25]),
            (WavChannelMode::MonoRight, 1, &[0.5, 0.75]),
        ];

        for (index, (mode, channels, expected_samples)) in cases.into_iter().enumerate() {
            let output = root.join(format!("channels-{index}.wav"));
            let mut samples = Vec::new();
            append_wav_block_samples(&stereo_block, WavSampleFormat::Float32, mode, &mut samples)
                .unwrap();
            let block_align = channels * 4;
            let mut file = File::create(&output).unwrap();
            write_wav_header(
                &mut file,
                48_000,
                2,
                samples.len() as u32,
                WavSampleFormat::Float32,
                mode,
            )
            .unwrap();
            file.write_all(&samples).unwrap();
            drop(file);

            let bytes = fs::read(&output).unwrap();
            assert_eq!(
                u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
                channels
            );
            assert_eq!(
                u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
                48_000 * u32::from(block_align)
            );
            assert_eq!(
                u16::from_le_bytes(bytes[32..34].try_into().unwrap()),
                block_align
            );
            assert_eq!(
                u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
                samples.len() as u32
            );
            let decoded = bytes[44..]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|sample| f32::from_le_bytes(*sample))
                .collect::<Vec<_>>();
            assert_eq!(decoded, expected_samples);
        }

        assert!(
            append_wav_block_samples(
                &[0.25],
                WavSampleFormat::Float32,
                WavChannelMode::MonoMerged,
                &mut Vec::new()
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
