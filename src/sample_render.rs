//! Offline rendering for Playlist clips that reference FLP audio channels.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwapOption;
use crossbeam_queue::ArrayQueue;
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
use vst3_host::midi::{MidiChannel, MidiEvent};

use crate::audio::{
    AudioInputMonitorOutput, AudioInputRecording, LiveMixerSourceControl, LiveMixerSourceTargets,
    MIXER_TRACK_METER_COUNT, MixerTrackPeakMeters, StereoFrameDelay, StreamingAudioWriter,
};
use crate::media::{
    DecodedAudio, SamplePathResolver, decode_audio_file, process_audio_range_for_playback,
    sample_sampler_metadata,
};
use crate::vst3::{
    MAX_PDC_DELAY_BYTES, PlaylistMidiNote, Vst3LatencyReport, Vst3ParameterAutomation,
    Vst3PlaylistAutomationPoint, Vst3PlaylistParameterAutomation, Vst3PlaylistStreamProcessor,
    expand_pattern_parameter_automation_for_clip,
};
use crate::{
    Arrangement, ChannelNoteRouter, ChannelStretchMode, ChannelSummary, FlpDocument,
    MixerInsertSignalTransform, MixerLiveSourceRoutePlan, MixerRouteAudibility,
    MixerSourceRoutePlan, MixerSourceRouteTransform, PATTERN_NOTE_PORTA_FLAG, Pattern, PatternNote,
    PlaylistClip, PlaylistClipTarget, PlaylistTrackMotion, PlaylistTrackPress, PlaylistTrackSync,
};

const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const MAX_MIX_BYTES: usize = 512 * 1024 * 1024;
const MAX_SOURCE_CACHE_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_SAMPLER_VOICE_LIMIT: usize = 64;
const MAX_SAMPLER_VOICE_LIMIT: usize = 256;
const SAMPLER_LIVE_NOTE_QUEUE_CAPACITY: usize = 1_024;
const VST3_LIVE_INPUT_QUEUE_CAPACITY: usize = 1_024;
const LAYER_LIVE_INPUT_QUEUE_CAPACITY: usize = 1_024;
const SAMPLER_RELEASE_SECONDS: f64 = 0.005;
const SAMPLER_ROOT_KEY: u16 = 60;
const SAMPLER_POLYPHONY_SLIDE_DEFAULT_RAW: u32 = 820;
const SAMPLER_POLYPHONY_SLIDE_MAX_RAW: u32 = 1_660;
const SAMPLER_BLOCK_FRAMES: usize = 1_024;
const FLAC_BLOCK_FRAMES: usize = 4_096;
const FLAC_MAX_SAMPLE_FRAMES: u64 = (1_u64 << 36) - 1;
const OGG_MIN_BITRATE_KBPS: u16 = 64;
const OGG_MAX_BITRATE_KBPS: u16 = 450;
const PLAYLIST_REMAINDER_MAX_SECONDS: u8 = 60;
const PLAYLIST_REMAINDER_SILENCE_SECONDS: u64 = 2;
const PLAYLIST_REMAINDER_SILENCE_THRESHOLD: f32 = 0.000_031_622_78;
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

    /// Build a timing map from the base tempo and absolute tick/tempo pairs.
    pub fn from_tempo_events(
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

    /// Convert a fractional Playlist tick to elapsed seconds.
    pub fn seconds_at_fractional_tick(&self, tick: f64) -> Result<f64, String> {
        if !tick.is_finite() || tick < 0.0 {
            return Err("Playlist position must be finite and non-negative".to_owned());
        }
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_tick as f64 <= tick)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let elapsed_ticks = tick - segment.start_tick as f64;
        let seconds = segment.start_seconds
            + elapsed_ticks * 60.0 / (f64::from(self.ppq) * segment.tempo_bpm);
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("Playlist position is outside the renderable timeline".to_owned());
        }
        Ok(seconds)
    }

    /// Convert elapsed seconds to a fractional Playlist tick.
    pub fn tick_at_seconds(&self, seconds: f64) -> Option<f64> {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_seconds <= seconds)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let tick = segment.start_tick as f64
            + (seconds - segment.start_seconds) * segment.tempo_bpm * f64::from(self.ppq) / 60.0;
        tick.is_finite().then_some(tick.max(0.0))
    }

    /// Return the active tempo at an absolute Playlist tick.
    pub fn tempo_bpm_at_tick(&self, tick: u64) -> f64 {
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_tick <= tick)
            .saturating_sub(1);
        self.segments[segment_index].tempo_bpm
    }

    /// Return the tempo map's start tick and tempo for each constant-tempo segment.
    pub fn tempo_segments(&self) -> impl Iterator<Item = (u64, f64)> + '_ {
        self.segments
            .iter()
            .map(|segment| (segment.start_tick, segment.tempo_bpm))
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

    fn has_tempo_change_between(&self, start_tick: f64, end_tick: f64) -> bool {
        self.segments.iter().skip(1).any(|segment| {
            let segment_tick = segment.start_tick as f64;
            segment_tick > start_tick && segment_tick < end_tick
        })
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioClipRenderOptions {
    pub arrangement_id: u16,
    pub sample_rate: u32,
    pub wav_sample_format: WavSampleFormat,
    pub wav_dither_mode: WavDitherMode,
    pub wav_channel_mode: WavChannelMode,
    pub resampling_quality: ResamplingQuality,
    /// Render only this zero-based arrangement clip, rebased to output time zero.
    pub clip_index: Option<usize>,
    /// Render only these zero-based arrangement clips as one relative-timeline selection.
    pub clip_indices: Vec<usize>,
    /// Keep the selected clip at its original arrangement position in the output.
    pub start_from_song_start: bool,
    /// Render only this inclusive one-based Playlist track range when soloing.
    pub soloed_playlist_track_range: Option<(u32, u32)>,
    /// App-local per-instance audio controls keyed by arrangement and clip ID.
    pub clip_envelopes: Arc<Vec<PlaylistAudioClipEnvelope>>,
}

/// Local per-instance Audio Clip gain, edge fades, duration fitting, pitch, pan, and playback
/// direction. Values are kept separate from opaque FLP records until native Clip Properties
/// serialization has been verified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistAudioClipEnvelope {
    pub arrangement_id: u16,
    pub clip_id: u32,
    /// Gain in hundredths of a decibel; `i16::MIN` represents FL Studio's -infinity silence state.
    pub gain_centidb: i16,
    pub fade_in_ms: u32,
    pub fade_out_ms: u32,
    pub fade_in_curve: PlaylistAudioClipFadeCurve,
    pub fade_in_tension: i16,
    pub fade_out_curve: PlaylistAudioClipFadeCurve,
    pub fade_out_tension: i16,
    /// Fit this instance to its visible timeline duration using the Audio Clip channel's shared
    /// stretch mode.
    pub stretch_to_clip_duration: bool,
    /// App-local instance pitch offset in cents, limited to -1200 through +1200.
    pub stretch_pitch_cents: i16,
    /// App-local absolute pan override in percent (-100 left through +100 right).
    pub pan_override: Option<i16>,
    /// App-local playback direction override for this instance.
    pub reverse_override: Option<bool>,
}

pub const PLAYLIST_AUDIO_CLIP_GAIN_MINUS_INFINITY: i16 = i16::MIN;

impl PlaylistAudioClipEnvelope {
    pub const fn gain_centidb_is_supported(self) -> bool {
        self.gain_centidb == PLAYLIST_AUDIO_CLIP_GAIN_MINUS_INFINITY
            || (-9_600 <= self.gain_centidb && self.gain_centidb <= 3_600)
    }

    pub fn linear_gain(self) -> f32 {
        if self.gain_centidb == PLAYLIST_AUDIO_CLIP_GAIN_MINUS_INFINITY {
            0.0
        } else {
            10.0_f32.powf(f32::from(self.gain_centidb) / 2_000.0)
        }
    }
}

/// App-local shape for an Audio Clip instance's fade. FLP serialization remains opaque.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistAudioClipFadeCurve {
    Linear,
    Smooth,
    Single,
}

impl PlaylistAudioClipFadeCurve {
    pub const fn code(self) -> u8 {
        match self {
            Self::Linear => 0,
            Self::Smooth => 1,
            Self::Single => 2,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Linear),
            1 => Some(Self::Smooth),
            2 => Some(Self::Single),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Smooth => "Smooth (S)",
            Self::Single => "Single curve",
        }
    }
}

/// Maps a normalized position on one fade to its linear-amplitude gain.
/// Single-curve tension is an app-local power curve with a neutral value of zero.
pub fn playlist_audio_clip_fade_gain(
    progress: f32,
    curve: PlaylistAudioClipFadeCurve,
    tension: i16,
) -> f32 {
    let progress = if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        return 0.0;
    };
    match curve {
        PlaylistAudioClipFadeCurve::Linear => progress,
        PlaylistAudioClipFadeCurve::Smooth => progress * progress * (3.0 - 2.0 * progress),
        PlaylistAudioClipFadeCurve::Single => {
            let exponent = 2.0_f32.powf(f32::from(tension.clamp(-100, 100)) / 50.0);
            progress.powf(exponent)
        }
    }
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
            clip_indices: Vec::new(),
            start_from_song_start: false,
            soloed_playlist_track_range: None,
            clip_envelopes: Arc::new(Vec::new()),
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

/// Timing and prepared plug-in stream for live Pattern-mode playback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplerPatternLoopPlayback {
    pub loop_length_ticks: u64,
    pub start_tick: u64,
}

/// Pre-rendered stereo audio from a CLAP instrument for one Pattern loop.
/// Samples must contain exactly one loop at the Pattern stream's sample rate.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternAudioLayer {
    pub channel_id: u16,
    pub left_gain: f32,
    pub right_gain: f32,
    pub samples: Vec<f32>,
}

/// Pre-rendered stereo audio from one CLAP instrument across a Playlist timeline.
#[derive(Clone, Debug)]
pub struct PlaylistAudioLayer {
    pub channel_id: u16,
    pub left_gain: f32,
    pub right_gain: f32,
    pub samples: Vec<f32>,
}

impl PartialEq for PlaylistAudioLayer {
    fn eq(&self, other: &Self) -> bool {
        self.channel_id == other.channel_id
            && self.left_gain.to_bits() == other.left_gain.to_bits()
            && self.right_gain.to_bits() == other.right_gain.to_bits()
            && self.samples.len() == other.samples.len()
            && self
                .samples
                .iter()
                .zip(&other.samples)
                .all(|(left, right)| left.to_bits() == right.to_bits())
    }
}

impl Eq for PlaylistAudioLayer {}

/// Live Sampler Pattern loop inputs, meters, and block-boundary Mixer updates.
pub struct SamplerPatternLoopStreamControls<'a> {
    pub playback: SamplerPatternLoopPlayback,
    pub meters: &'a crate::audio::MixerTrackPeakMeters,
    pub latency_updates: Option<Vst3LatencyUpdateControls>,
    pub pattern_audio_layers: Vec<PatternAudioLayer>,
}

/// Live Sampler Pattern preview inputs, meters, and block-boundary Mixer updates.
pub struct SamplerPatternStreamControls<'a> {
    pub meters: &'a crate::audio::MixerTrackPeakMeters,
    pub latency_updates: Option<Vst3LatencyUpdateControls>,
}

/// Timing and prepared plug-in stream for live Pattern-mode playback.
pub struct Vst3PatternLoopPlayback {
    pub loop_length_ticks: u64,
    pub start_tick: u64,
    pub processor: Vst3PlaylistStreamProcessor,
}

/// Live Pattern-loop stream and controls for meters and VST3 latency refreshes.
pub struct Vst3PatternLoopStreamControls<'a> {
    pub playback: Vst3PatternLoopPlayback,
    pub meters: &'a crate::audio::MixerTrackPeakMeters,
    pub latency_updates: Option<Vst3LatencyUpdateControls>,
    pub pattern_audio_layers: Vec<PatternAudioLayer>,
}

/// Prepared Sampler/VST3 streams for one-shot playback of a complete Pattern.
pub struct Vst3PatternPlayback {
    pub duration_ticks: u64,
    pub processor: Vst3PlaylistStreamProcessor,
}

/// Live Sampler/VST3 Pattern preview inputs, meters, and block-boundary updates.
pub struct Vst3PatternStreamControls<'a> {
    pub playback: Vst3PatternPlayback,
    pub meters: &'a crate::audio::MixerTrackPeakMeters,
    pub latency_updates: Option<Vst3LatencyUpdateControls>,
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
    TempoBpmMilli {
        value: u32,
    },
    NoteOn {
        channel: u8,
        key: u8,
        velocity: u8,
        route_seed: u64,
    },
    NoteOff {
        channel: u8,
        key: u8,
    },
    PitchBend {
        channel: u8,
        cents: i16,
    },
    Pan {
        channel: u8,
        value: u8,
    },
    Pan14Bit {
        channel: u8,
        value: u16,
    },
    ChannelVolume {
        channel: u8,
        value: u8,
    },
    ChannelVolume14Bit {
        channel: u8,
        value: u16,
    },
    Expression {
        channel: u8,
        value: u8,
    },
    Expression14Bit {
        channel: u8,
        value: u16,
    },
    AllNotesOff {
        channel: u8,
    },
    AllSoundOff {
        channel: u8,
    },
    Sustain {
        channel: u8,
        down: bool,
    },
    Sostenuto {
        channel: u8,
        down: bool,
    },
    ResetControllers {
        channel: u8,
        pan: u8,
        pitch_bend_cents: i16,
    },
}

/// Lock-free producer for Sampler live-MIDI events.
#[derive(Clone)]
pub struct SamplerLiveNoteSender {
    events: Arc<ArrayQueue<SamplerLiveNoteEvent>>,
    overflowed: Arc<AtomicBool>,
}

/// Lock-free consumer for Sampler live-MIDI events.
pub struct SamplerLiveNoteReceiver {
    events: Arc<ArrayQueue<SamplerLiveNoteEvent>>,
    overflowed: Arc<AtomicBool>,
}

/// Creates a bounded, allocation-free queue for live MIDI input to a Sampler worker.
pub fn sampler_live_note_queue() -> (SamplerLiveNoteSender, SamplerLiveNoteReceiver) {
    let events = Arc::new(ArrayQueue::new(SAMPLER_LIVE_NOTE_QUEUE_CAPACITY));
    let overflowed = Arc::new(AtomicBool::new(false));
    (
        SamplerLiveNoteSender {
            events: Arc::clone(&events),
            overflowed: Arc::clone(&overflowed),
        },
        SamplerLiveNoteReceiver { events, overflowed },
    )
}

impl SamplerLiveNoteSender {
    /// Enqueues an event without blocking; a full queue marks the stream for panic recovery.
    pub fn send(&self, event: SamplerLiveNoteEvent) -> Result<(), SamplerLiveNoteEvent> {
        self.events.push(event).inspect_err(|_| {
            self.overflowed.store(true, Ordering::Release);
        })
    }
}

impl SamplerLiveNoteReceiver {
    fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    fn pop(&self) -> Option<SamplerLiveNoteEvent> {
        self.events.pop()
    }

    fn try_recv(&self) -> Result<SamplerLiveNoteEvent, TryRecvError> {
        self.pop().ok_or(TryRecvError::Empty)
    }

    fn discard_pending(&self) {
        for _ in 0..SAMPLER_LIVE_NOTE_QUEUE_CAPACITY {
            if self.events.pop().is_none() {
                break;
            }
        }
    }
}

/// Source channel and resolved Sampler destinations for live MIDI audition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamplerLiveInputRouting {
    /// Channel receiving the selected Pattern's live MIDI input.
    pub source_channel_id: u16,
    /// Enabled Sampler channels that may receive routed notes from the source.
    pub target_channel_ids: BTreeSet<u16>,
}

/// Live MIDI and Sampler controller state for a mixed-child Layer source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerLiveInputEvent {
    /// MIDI message forwarded to VST3 children and interpreted for Sampler children as applicable.
    Midi(Vst3LiveInputEvent),
    /// Updated Sampler pitch-bend offset for an input MIDI channel.
    SamplerPitchBend { channel: u8, cents: i16 },
    /// Updated 14-bit Sampler Pan for an input MIDI channel.
    SamplerPan14Bit { channel: u8, value: u16 },
    /// Updated 14-bit Sampler Channel Volume for an input MIDI channel.
    SamplerChannelVolume14Bit { channel: u8, value: u16 },
    /// Updated 14-bit Sampler Expression for an input MIDI channel.
    SamplerExpression14Bit { channel: u8, value: u16 },
    /// Reset Sampler sustain, channel-volume, expression, pan, and pitch-bend state
    /// for an input MIDI channel.
    SamplerResetControllers {
        channel: u8,
        pan: u8,
        pitch_bend_cents: i16,
    },
}

/// Prepared destinations and Sampler settings for a mixed-child Layer input stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayerLiveInputConfig {
    /// Channel receiving the selected Pattern's live MIDI input.
    pub source_channel_id: u16,
    /// Enabled Sampler children that receive routed Layer notes.
    pub sampler_target_channel_ids: BTreeSet<u16>,
    /// Loaded VST3 children that receive routed Layer notes.
    pub vst3_target_channel_ids: BTreeSet<u16>,
    /// Saved-project location used to resolve Sampler sample paths.
    pub project_path: PathBuf,
    /// Sample rate, resampling, and selected Pattern used for Sampler preparation.
    pub sampler_options: SamplerPatternRenderOptions,
}

/// Live MIDI messages sent to a selected VST3 channel while transport is stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vst3LiveInputEvent {
    NoteOn {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        key: u8,
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
    PitchBend {
        channel: u8,
        value: u16,
    },
    ChannelPressure {
        channel: u8,
        pressure: u8,
    },
    PolyphonicPressure {
        channel: u8,
        key: u8,
        pressure: u8,
    },
}

/// Lock-free producer for bounded live-MIDI event queues.
pub struct BoundedLiveInputSender<T: Send> {
    events: Arc<ArrayQueue<T>>,
    closed: Arc<AtomicBool>,
}

/// Lock-free consumer for bounded live-MIDI event queues.
pub struct BoundedLiveInputReceiver<T: Send> {
    events: Arc<ArrayQueue<T>>,
    closed: Arc<AtomicBool>,
}

pub type Vst3LiveInputSender = BoundedLiveInputSender<Vst3LiveInputEvent>;
pub type Vst3LiveInputReceiver = BoundedLiveInputReceiver<Vst3LiveInputEvent>;
pub type LayerLiveInputSender = BoundedLiveInputSender<LayerLiveInputEvent>;
pub type LayerLiveInputReceiver = BoundedLiveInputReceiver<LayerLiveInputEvent>;

fn bounded_live_input_queue<T: Send>(
    capacity: usize,
) -> (BoundedLiveInputSender<T>, BoundedLiveInputReceiver<T>) {
    let events = Arc::new(ArrayQueue::new(capacity));
    let closed = Arc::new(AtomicBool::new(false));
    (
        BoundedLiveInputSender {
            events: Arc::clone(&events),
            closed: Arc::clone(&closed),
        },
        BoundedLiveInputReceiver { events, closed },
    )
}

/// Creates a bounded, allocation-free queue for live MIDI input to a VST3 worker.
pub fn vst3_live_input_queue() -> (Vst3LiveInputSender, Vst3LiveInputReceiver) {
    bounded_live_input_queue(VST3_LIVE_INPUT_QUEUE_CAPACITY)
}

/// Creates a bounded, allocation-free queue for live MIDI input to a mixed Layer worker.
pub fn layer_live_input_queue() -> (LayerLiveInputSender, LayerLiveInputReceiver) {
    bounded_live_input_queue(LAYER_LIVE_INPUT_QUEUE_CAPACITY)
}

impl<T: Send> BoundedLiveInputSender<T> {
    /// Enqueues an event without blocking, returning it when the queue is full or closed.
    pub fn send(&self, event: T) -> Result<(), T> {
        if self.closed.load(Ordering::Acquire) {
            return Err(event);
        }
        self.events.push(event)
    }
}

impl<T: Send> Drop for BoundedLiveInputSender<T> {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl<T: Send> BoundedLiveInputReceiver<T> {
    fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.events.pop() {
            Some(event) => Ok(event),
            None if self.closed.load(Ordering::Acquire) => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }
}

impl<T: Send> Drop for BoundedLiveInputReceiver<T> {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
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
    /// Remove the initial automatic PDC delay from Playlist WAV and stem renders.
    pub wav_trim_pdc_silence: bool,
    /// Whether to stop a Playlist render after its post-content output has decayed to silence.
    pub tail_mode: PlaylistTailMode,
    pub tail_seconds: u8,
    /// Render only this inclusive one-based Playlist track range when soloing.
    pub soloed_playlist_track_range: Option<(u32, u32)>,
    pub read_sample_root_note: bool,
    /// App-local per-instance Audio Clip controls applied in live playback and renders.
    pub clip_envelopes: Arc<Vec<PlaylistAudioClipEnvelope>>,
    /// Pre-rendered CLAP instruments scheduled on the Playlist timeline.
    pub clap_audio_layers: Arc<Vec<PlaylistAudioLayer>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlaylistTailMode {
    /// Continue past the content endpoint until output stays below the render silence threshold.
    #[default]
    LeaveRemainder,
    /// Render only the selected fixed duration after the content endpoint.
    FixedDuration,
    /// Mix the rendered tail after the content endpoint onto the beginning of the output.
    WrapRemainder,
}

/// Output format and timeline range for a Playlist render that begins after song start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistRenderRange {
    pub output_format: AudioRenderOutput,
    pub output_start_frame: u64,
    pub minimum_content_frames: u64,
}

/// A Performance Mode Playlist Clip launch scheduled on the timeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistPerformanceLaunch {
    pub clip_index: usize,
    pub start_tick: u64,
    pub start_frame: u64,
    /// Playback phase within the source clip, in project ticks.
    pub position_offset_ticks: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistPerformanceCommand {
    Launch(PlaylistPerformanceLaunch),
    Enqueue(PlaylistPerformanceLaunch),
    AdvanceMotion {
        clip_index: usize,
        track_id: u32,
        start_tick: u64,
        start_frame: u64,
    },
    StopTrack {
        track_id: u32,
        start_tick: u64,
        start_frame: u64,
    },
}

/// Performance clips that are currently playing or scheduled in the live renderer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistPerformanceLiveStatus {
    pub playing_clip_indices: BTreeSet<usize>,
    pub scheduled_clip_indices: BTreeSet<usize>,
}

impl PlaylistPerformanceCommand {
    fn start_frame(self) -> u64 {
        match self {
            Self::Launch(launch) | Self::Enqueue(launch) => launch.start_frame,
            Self::AdvanceMotion { start_frame, .. } => start_frame,
            Self::StopTrack { start_frame, .. } => start_frame,
        }
    }
}

/// Runtime inputs for a Performance Mode Playlist stream.
pub struct PlaylistPerformanceStream {
    pub performance_zone_end_tick: u32,
    pub start_frame: u64,
    pub launches: Receiver<PlaylistPerformanceCommand>,
    pub mixer_routing_updates: Receiver<FlpDocument>,
    pub latency_updates: Receiver<BTreeSet<u64>>,
    pub latency_reports: Sender<Vst3LatencyReport>,
    pub live_status: Arc<Mutex<PlaylistPerformanceLiveStatus>>,
    pub meters: Option<Arc<MixerTrackPeakMeters>>,
}

/// Return the supported Sampler Pattern Clips that can be launched before the Start marker.
pub fn playlist_performance_sampler_clip_indices(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
) -> Result<BTreeSet<usize>, String> {
    let candidates = playlist_performance_sampler_clip_candidates(
        document,
        arrangement_id,
        performance_zone_end_tick,
    )?;
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let candidate_track_ids = candidates
        .iter()
        .filter_map(|index| arrangement.clips.get(*index)?.playlist_track_id())
        .collect::<BTreeSet<_>>();
    let eligible_track_ids = candidate_track_ids
        .into_iter()
        .filter(|track_id| {
            arrangement.clips.iter().enumerate().all(|(index, clip)| {
                clip.playlist_track_id() != Some(*track_id)
                    || !matches!(clip.target(), PlaylistClipTarget::Pattern { .. })
                    || clip.is_muted()
                    || candidates.contains(&index)
            })
        })
        .collect::<BTreeSet<_>>();
    Ok(candidates
        .into_iter()
        .filter(|index| {
            arrangement
                .clips
                .get(*index)
                .and_then(PlaylistClip::playlist_track_id)
                .is_some_and(|track_id| eligible_track_ids.contains(&track_id))
        })
        .collect())
}

fn playlist_performance_sampler_clip_candidates(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
) -> Result<BTreeSet<usize>, String> {
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let patterns = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|pattern| (pattern.id, pattern))
        .collect::<HashMap<_, _>>();
    let channels = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    let disabled_tracks = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();

    let candidates = arrangement
        .clips
        .iter()
        .enumerate()
        .filter_map(|(index, clip)| {
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                return None;
            };
            let track_id = clip.playlist_track_id()?;
            let pattern = patterns.get(&id)?;
            (clip.position_ticks < performance_zone_end_tick
                && clip.length_ticks > 0
                && !clip.is_muted()
                && !disabled_tracks.contains(&track_id)
                && clip
                    .scale
                    .is_none_or(|scale| scale.is_finite() && (scale - 1.0).abs() <= 1e-9)
                && pattern.controllers.is_empty()
                && pattern.notes.iter().all(|note| {
                    note.is_muted()
                        || playlist_performance_sampler_note_source_supported(
                            note.channel_id,
                            &channels,
                        )
                }))
            .then_some(index)
        })
        .collect::<BTreeSet<_>>();
    Ok(candidates)
}

fn playlist_performance_sampler_note_source_supported(
    source_channel_id: u16,
    channels: &HashMap<u16, ChannelSummary>,
) -> bool {
    let Some(source) = channels.get(&source_channel_id) else {
        return false;
    };
    if source.enabled() == Some(false) {
        return false;
    }
    if source.layer_child_ids().is_some() {
        return performance_layer_leaf_channel_ids(source_channel_id, channels).is_some_and(
            |leaf_ids| {
                !leaf_ids.is_empty()
                    && leaf_ids.iter().all(|channel_id| {
                        channels
                            .get(channel_id)
                            .is_some_and(|child| child.kind() == Some(0))
                    })
            },
        );
    }
    source.kind() == Some(0)
}

fn performance_layer_leaf_channel_ids(
    source_channel_id: u16,
    channels: &HashMap<u16, ChannelSummary>,
) -> Option<BTreeSet<u16>> {
    fn collect(
        channel_id: u16,
        channels: &HashMap<u16, ChannelSummary>,
        active_layers: &mut BTreeSet<u16>,
        leaves: &mut BTreeSet<u16>,
    ) -> Option<()> {
        let channel = channels.get(&channel_id)?;
        if channel.enabled() == Some(false) {
            return None;
        }
        let Some(child_ids) = channel.layer_child_ids() else {
            leaves.insert(channel_id);
            return Some(());
        };
        if child_ids.is_empty() || !active_layers.insert(channel_id) {
            return None;
        }
        let mut seen = BTreeSet::new();
        for child_id in child_ids {
            if !seen.insert(*child_id) {
                continue;
            }
            collect(*child_id, channels, active_layers, leaves)?;
        }
        active_layers.remove(&channel_id);
        Some(())
    }

    let mut leaves = BTreeSet::new();
    collect(
        source_channel_id,
        channels,
        &mut BTreeSet::new(),
        &mut leaves,
    )?;
    Some(leaves)
}

/// Return enabled Audio Clips before the Start marker that can be launched in Performance Mode.
pub fn playlist_performance_audio_clip_indices(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
) -> Result<BTreeSet<usize>, String> {
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let channels = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    let disabled_tracks = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();

    Ok(arrangement
        .clips
        .iter()
        .enumerate()
        .filter_map(|(index, clip)| {
            let PlaylistClipTarget::Channel { id } = clip.target() else {
                return None;
            };
            let track_id = clip.playlist_track_id()?;
            let channel = channels.get(&id)?;
            (clip.position_ticks < performance_zone_end_tick
                && clip.length_ticks > 0
                && !clip.is_muted()
                && !disabled_tracks.contains(&track_id)
                && clip
                    .scale
                    .is_none_or(|scale| scale.is_finite() && (scale - 1.0).abs() <= 1e-9)
                && channel.kind() == Some(4)
                && channel.enabled() != Some(false)
                && channel.sample_path().is_some())
            .then_some(index)
        })
        .collect())
}

fn playlist_performance_vst3_note_source_supported(
    source_channel_id: u16,
    channels: &HashMap<u16, ChannelSummary>,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> bool {
    let Some(source) = channels.get(&source_channel_id) else {
        return false;
    };
    if source.enabled() == Some(false) {
        return false;
    }
    if source.layer_child_ids().is_some() {
        return performance_layer_leaf_channel_ids(source_channel_id, channels).is_some_and(
            |leaf_ids| {
                !leaf_ids.is_empty()
                    && leaf_ids.iter().all(|channel_id| {
                        loaded_vst3_channels.contains(channel_id)
                            && channels.get(channel_id).is_some_and(|child| {
                                child.kind() != Some(3) && child.plugin_identifier().is_some()
                            })
                    })
            },
        );
    }
    loaded_vst3_channels.contains(&source_channel_id) && source.plugin_identifier().is_some()
}

fn playlist_performance_note_source_route_kinds(
    source_channel_id: u16,
    channels: &HashMap<u16, ChannelSummary>,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> Option<(bool, bool)> {
    let source = channels.get(&source_channel_id)?;
    if source.enabled() == Some(false) {
        return None;
    }
    if source.layer_child_ids().is_some() {
        let leaf_ids = performance_layer_leaf_channel_ids(source_channel_id, channels)?;
        if leaf_ids.is_empty() {
            return None;
        }
        let (mut has_sampler, mut has_vst3) = (false, false);
        for child_id in leaf_ids {
            let child = channels.get(&child_id)?;
            if child.kind() == Some(0) {
                has_sampler = true;
            } else if loaded_vst3_channels.contains(&child_id)
                && child.plugin_identifier().is_some()
            {
                has_vst3 = true;
            } else {
                return None;
            }
        }
        return Some((has_sampler, has_vst3));
    }
    if source.kind() == Some(0) {
        Some((true, false))
    } else if loaded_vst3_channels.contains(&source_channel_id)
        && source.plugin_identifier().is_some()
    {
        Some((false, true))
    } else {
        None
    }
}

fn playlist_performance_vst3_clip_candidates(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> Result<BTreeSet<usize>, String> {
    if loaded_vst3_channels.is_empty() {
        return Ok(BTreeSet::new());
    }
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let patterns = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|pattern| (pattern.id, pattern))
        .collect::<HashMap<_, _>>();
    let channels = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    let disabled_tracks = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    Ok(arrangement
        .clips
        .iter()
        .enumerate()
        .filter_map(|(index, clip)| {
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                return None;
            };
            let track_id = clip.playlist_track_id()?;
            let pattern = patterns.get(&id)?;
            let notes = pattern
                .notes
                .iter()
                .filter(|note| !note.is_muted())
                .collect::<Vec<_>>();
            (clip.position_ticks < performance_zone_end_tick
                && clip.length_ticks > 0
                && !clip.is_muted()
                && !disabled_tracks.contains(&track_id)
                && clip
                    .scale
                    .is_none_or(|scale| scale.is_finite() && (scale - 1.0).abs() <= 1e-9)
                && pattern.controllers.is_empty()
                && !notes.is_empty()
                && notes.iter().all(|note| {
                    playlist_performance_vst3_note_source_supported(
                        note.channel_id,
                        &channels,
                        loaded_vst3_channels,
                    )
                }))
            .then_some(index)
        })
        .collect())
}

fn playlist_performance_mixed_source_clip_candidates(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> Result<BTreeSet<usize>, String> {
    if loaded_vst3_channels.is_empty() {
        return Ok(BTreeSet::new());
    }
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let patterns = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|pattern| (pattern.id, pattern))
        .collect::<HashMap<_, _>>();
    let channels = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    let disabled_tracks = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    Ok(arrangement
        .clips
        .iter()
        .enumerate()
        .filter_map(|(index, clip)| {
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                return None;
            };
            let track_id = clip.playlist_track_id()?;
            let pattern = patterns.get(&id)?;
            let notes = pattern
                .notes
                .iter()
                .filter(|note| !note.is_muted())
                .collect::<Vec<_>>();
            let (mut has_sampler, mut has_vst3) = (false, false);
            let sources_supported = notes.iter().all(|note| {
                let Some((note_has_sampler, note_has_vst3)) =
                    playlist_performance_note_source_route_kinds(
                        note.channel_id,
                        &channels,
                        loaded_vst3_channels,
                    )
                else {
                    return false;
                };
                has_sampler |= note_has_sampler;
                has_vst3 |= note_has_vst3;
                true
            });
            (clip.position_ticks < performance_zone_end_tick
                && clip.length_ticks > 0
                && !clip.is_muted()
                && !disabled_tracks.contains(&track_id)
                && clip
                    .scale
                    .is_none_or(|scale| scale.is_finite() && (scale - 1.0).abs() <= 1e-9)
                && pattern.controllers.is_empty()
                && !notes.is_empty()
                && sources_supported
                && has_sampler
                && has_vst3)
                .then_some(index)
        })
        .collect())
}

/// Return supported Audio and Sampler clips before Start.
pub fn playlist_performance_clip_indices(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
) -> Result<BTreeSet<usize>, String> {
    playlist_performance_clip_indices_with_vst3(
        document,
        arrangement_id,
        performance_zone_end_tick,
        &BTreeSet::new(),
    )
}

/// Return supported Performance Mode clips, including Pattern Clips that combine enabled Sampler
/// and loaded VST3 destinations.
pub fn playlist_performance_clip_indices_with_vst3(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> Result<BTreeSet<usize>, String> {
    let mut clip_indices = playlist_performance_sampler_clip_candidates(
        document,
        arrangement_id,
        performance_zone_end_tick,
    )?;
    clip_indices.extend(playlist_performance_audio_clip_indices(
        document,
        arrangement_id,
        performance_zone_end_tick,
    )?);
    clip_indices.extend(playlist_performance_vst3_clip_candidates(
        document,
        arrangement_id,
        performance_zone_end_tick,
        loaded_vst3_channels,
    )?);
    clip_indices.extend(playlist_performance_mixed_source_clip_candidates(
        document,
        arrangement_id,
        performance_zone_end_tick,
        loaded_vst3_channels,
    )?);
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let candidate_track_ids = clip_indices
        .iter()
        .filter_map(|index| arrangement.clips.get(*index)?.playlist_track_id())
        .collect::<BTreeSet<_>>();
    let eligible_track_ids = candidate_track_ids
        .into_iter()
        .filter(|track_id| {
            arrangement.clips.iter().enumerate().all(|(index, clip)| {
                clip.playlist_track_id() != Some(*track_id)
                    || clip.is_muted()
                    || !matches!(
                        clip.target(),
                        PlaylistClipTarget::Pattern { .. } | PlaylistClipTarget::Channel { .. }
                    )
                    || clip_indices.contains(&index)
            })
        })
        .collect::<BTreeSet<_>>();
    clip_indices.retain(|index| {
        arrangement
            .clips
            .get(*index)
            .and_then(PlaylistClip::playlist_track_id)
            .is_some_and(|track_id| eligible_track_ids.contains(&track_id))
    });
    Ok(clip_indices)
}

/// Return VST3 and mixed Sampler/VST3 Pattern Clips on tracks whose Performance Zone media is fully
/// supported.
pub fn playlist_performance_vst3_clip_indices(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> Result<BTreeSet<usize>, String> {
    let mut candidates = playlist_performance_vst3_clip_candidates(
        document,
        arrangement_id,
        performance_zone_end_tick,
        loaded_vst3_channels,
    )?;
    candidates.extend(playlist_performance_mixed_source_clip_candidates(
        document,
        arrangement_id,
        performance_zone_end_tick,
        loaded_vst3_channels,
    )?);
    let supported = playlist_performance_clip_indices_with_vst3(
        document,
        arrangement_id,
        performance_zone_end_tick,
        loaded_vst3_channels,
    )?;
    Ok(candidates.intersection(&supported).copied().collect())
}

/// Return VST3 channels referenced directly or through nested Layer sources in Performance Mode
/// Pattern Clips.
pub fn playlist_performance_vst3_channel_ids(
    document: &FlpDocument,
    arrangement_id: u16,
    clip_indices: &BTreeSet<usize>,
) -> Result<BTreeSet<u16>, String> {
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
    let patterns = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|pattern| (pattern.id, pattern))
        .collect::<HashMap<_, _>>();
    let channels = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    Ok(clip_indices
        .iter()
        .filter_map(|clip_index| arrangement.clips.get(*clip_index))
        .filter_map(|clip| match clip.target() {
            PlaylistClipTarget::Pattern { id } => patterns.get(&id),
            PlaylistClipTarget::Channel { .. } => None,
        })
        .flat_map(|pattern| {
            pattern
                .notes
                .iter()
                .filter(|note| !note.is_muted())
                .flat_map(|note| {
                    performance_layer_leaf_channel_ids(note.channel_id, &channels)
                        .unwrap_or_default()
                })
        })
        .filter(|channel_id| {
            channels.get(channel_id).is_some_and(|channel| {
                channel.kind() != Some(3)
                    && channel.enabled() != Some(false)
                    && channel.plugin_identifier().is_some()
            })
        })
        .collect())
}

/// Return a copy of an arrangement with supported Performance Zone clips removed from static
/// playback. Other clips and tempo automation remain on the normal song timeline.
pub fn playlist_performance_static_document(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
) -> Result<(FlpDocument, BTreeSet<usize>), String> {
    let performance_clip_indices =
        playlist_performance_clip_indices(document, arrangement_id, performance_zone_end_tick)?;
    let mut static_document = document.clone();
    for clip_index in performance_clip_indices.iter().rev().copied() {
        static_document
            .delete_playlist_clip(arrangement_id, clip_index)
            .map_err(|error| format!("could not isolate Performance Zone tracks: {error}"))?;
    }
    Ok((static_document, performance_clip_indices))
}

/// Return an arrangement with supported Sampler, Audio, and loaded VST3 Performance clips
/// removed from static playback.
pub fn playlist_performance_static_document_with_vst3(
    document: &FlpDocument,
    arrangement_id: u16,
    performance_zone_end_tick: u32,
    loaded_vst3_channels: &BTreeSet<u16>,
) -> Result<(FlpDocument, BTreeSet<usize>), String> {
    let performance_clip_indices = playlist_performance_clip_indices_with_vst3(
        document,
        arrangement_id,
        performance_zone_end_tick,
        loaded_vst3_channels,
    )?;
    let mut static_document = document.clone();
    for clip_index in performance_clip_indices.iter().rev().copied() {
        static_document
            .delete_playlist_clip(arrangement_id, clip_index)
            .map_err(|error| format!("could not isolate Performance Zone tracks: {error}"))?;
    }
    Ok((static_document, performance_clip_indices))
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
            wav_trim_pdc_silence: true,
            tail_mode: PlaylistTailMode::LeaveRemainder,
            tail_seconds: 0,
            soloed_playlist_track_range: None,
            read_sample_root_note: true,
            clip_envelopes: Arc::new(Vec::new()),
            clap_audio_layers: Arc::new(Vec::new()),
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
#[derive(Clone)]
pub struct PlaylistStreamMonitor<'a> {
    start_frame: u64,
    meters: Option<&'a MixerTrackPeakMeters>,
    live_master_fader_gain: Option<Arc<AtomicU32>>,
    live_master_pan_raw: Option<Arc<AtomicI32>>,
    live_master_stereo_separation_raw: Option<Arc<AtomicI32>>,
    live_master_enabled: Option<Arc<AtomicI32>>,
    live_master_signal_flags: Option<Arc<AtomicI32>>,
}

/// Channels for requesting and reporting live VST3 latency changes in a stream.
pub struct Vst3LatencyUpdateControls {
    pub updates: Receiver<BTreeSet<u64>>,
    pub reports: Sender<Vst3LatencyReport>,
    /// Latest Mixer snapshots for block-boundary APDC and saved send-route updates.
    pub pdc_updates: Option<Receiver<FlpDocument>>,
}

/// Live device-stream controls for a Playlist render worker.
pub struct PlaylistLiveStreamControls<'a> {
    pub monitor: PlaylistStreamMonitor<'a>,
    pub latency_updates: Option<Vst3LatencyUpdateControls>,
}

struct LiveLatencyUpdateState {
    document: FlpDocument,
    pdc_updates: Option<Receiver<FlpDocument>>,
    updates: Receiver<BTreeSet<u64>>,
    reports: Sender<Vst3LatencyReport>,
}

impl<'a> PlaylistStreamMonitor<'a> {
    /// Stream from a frame without publishing per-insert meter data.
    pub const fn from_frame(start_frame: u64) -> Self {
        Self {
            start_frame,
            meters: None,
            live_master_fader_gain: None,
            live_master_pan_raw: None,
            live_master_stereo_separation_raw: None,
            live_master_enabled: None,
            live_master_signal_flags: None,
        }
    }

    /// Stream from a frame and publish peaks for supported direct and send insert routes.
    pub fn with_meters(start_frame: u64, meters: &'a MixerTrackPeakMeters) -> Self {
        Self {
            start_frame,
            meters: Some(meters),
            live_master_fader_gain: Some(meters.live_master_fader_gain()),
            live_master_pan_raw: Some(meters.live_master_pan_raw()),
            live_master_stereo_separation_raw: Some(meters.live_master_stereo_separation_raw()),
            live_master_enabled: Some(meters.live_master_enabled()),
            live_master_signal_flags: Some(meters.live_master_signal_flags()),
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
/// Pattern instruments, and plug-ins are not rendered by this source-mix path; the selected
/// single-clip file renderer can apply loaded VST3 Mixer effects.
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
    let (mix, summary) =
        render_audio_clips_to_stereo_buffer(document, project_path, options.clone())?;
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

/// Render one selected Audio Clip through prepared loaded Mixer effects to an audio file.
pub fn render_selected_audio_clip_to_file_with_mixer_effects(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    output_path: impl AsRef<Path>,
    output_format: AudioRenderOutput,
    processor: Vst3PlaylistStreamProcessor,
    render_from_song_start: bool,
) -> Result<AudioClipRenderSummary, String> {
    render_selected_audio_clips_to_file_with_mixer_effects(
        document,
        project_path,
        options,
        output_path,
        output_format,
        processor,
        render_from_song_start,
    )
}

/// Render a selected Audio Clip or group through prepared loaded Mixer effects to an audio file.
pub fn render_selected_audio_clips_to_file_with_mixer_effects(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    output_path: impl AsRef<Path>,
    output_format: AudioRenderOutput,
    mut processor: Vst3PlaylistStreamProcessor,
    render_from_song_start: bool,
) -> Result<AudioClipRenderSummary, String> {
    if selected_audio_clip_indices(&options)?.is_none() {
        return Err(
            "Mixer-effect Audio Clip renders require a selected clip or clip group".to_owned(),
        );
    }
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path_with_extension(project_path, output_path, output_format.extension())?;
    if processor.summary().sample_rate != options.sample_rate {
        return Err("Audio Clip and Mixer effect sample rates do not match".to_owned());
    }

    let mut source_options = options.clone();
    source_options.start_from_song_start = options.start_from_song_start || render_from_song_start;
    let render = prepare_audio_clip_render(document, project_path, source_options, false, None)?;
    require_audio_clips(&render)?;
    let clip_start_frame = render
        .clips
        .iter()
        .map(|clip| clip.start_frame)
        .min()
        .ok_or_else(|| "selected Audio Clip produced no render source".to_owned())?;
    let output_start_frame = if options.start_from_song_start {
        0
    } else {
        clip_start_frame
    };
    processor.extend_to_output_frames(render.summary.frames.max(processor.content_frames()))?;
    let processor_summary = processor.summary();
    let trim_frames = processor
        .pdc_render_start_trim_frames()
        .min(processor_summary.frames);
    let output_frames = processor_summary
        .frames
        .checked_sub(trim_frames)
        .and_then(|frames| frames.checked_sub(output_start_frame))
        .ok_or_else(|| "Audio Clip render endpoint precedes the selected clip".to_owned())?;
    let master_output = MixerMasterOutput::from_document(document);

    processor.start_processing()?;
    let render_result = render_stereo_audio_to_file(
        output_path,
        output_frames,
        processor_summary.sample_rate,
        AudioRenderEncodingOptions {
            wav_sample_format: options.wav_sample_format,
            wav_dither_mode: options.wav_dither_mode,
            wav_channel_mode: options.wav_channel_mode,
        },
        output_format,
        |write_block| {
            stream_prepared_audio_clip_render_with_mixer_effects(
                &render,
                &mut processor,
                processor_summary.frames,
                trim_frames,
                output_start_frame,
                &master_output,
                || false,
                write_block,
            )
        },
    );
    let stop_result = processor.stop_processing();
    let summary = render_result?;
    stop_result?;
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

/// Process live input through a selected Mixer route and loaded VST3 effects for monitoring.
pub fn stream_input_monitor_through_mixer(
    document: &FlpDocument,
    mixer_track: i8,
    recording: &AudioInputRecording,
    output: &AudioInputMonitorOutput,
    mixer_meters: &MixerTrackPeakMeters,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    if mixer_track < 0 {
        return Err("Mixer input monitoring requires Master or a recognized insert".to_owned());
    }
    let routing = MixerRouteAudibility::from_document(document);
    let route_plan = routing.source_route_plan_for_channel(Some(mixer_track));
    let sample_rate = recording.sample_rate();
    let live_source_control = routing
        .live_source_route_plan_for_channel(Some(mixer_track))
        .map(|route_plan| LiveMixerSourceControl::new(route_plan, sample_rate));
    let live_source_targets = live_source_control
        .as_ref()
        .and_then(|control| control.targets(mixer_meters));
    let live_route_controls_active = live_source_control.is_some() && live_source_targets.is_some();
    let route_is_audible = live_route_controls_active || routing.allows_channel(Some(mixer_track));
    let master_output = MixerMasterOutput::from_document(document)
        .with_live_master_targets(mixer_meters, sample_rate);
    let mixer_effect_buses_required = vst3_processor
        .as_ref()
        .is_some_and(Vst3PlaylistStreamProcessor::needs_mixer_insert_buses);
    let mixer_meter_buses_required = !route_plan.insert_meter_transforms.is_empty()
        || live_source_control
            .as_ref()
            .is_some_and(|control| !control.insert_meter_transforms().is_empty());
    let mixer_buses_required = mixer_meter_buses_required || mixer_effect_buses_required;
    let mixer_bus_planes = 1 + usize::from(mixer_effect_buses_required);
    const BLOCK_FRAMES: usize = 128;
    let mut input_frames = vec![[0.0_f32; 2]; BLOCK_FRAMES];
    let mut output_block = vec![0.0_f32; BLOCK_FRAMES * 2];
    let mut mixer_buses = mixer_buses_required
        .then(|| vec![0.0_f32; MIXER_TRACK_METER_COUNT * BLOCK_FRAMES * 2 * mixer_bus_planes]);
    let mut post_effect_buses = mixer_effect_buses_required
        .then(|| vec![0.0_f32; MIXER_TRACK_METER_COUNT * BLOCK_FRAMES * 2]);
    if let Some(processor) = vst3_processor.as_mut() {
        processor.set_live_mixer_source_targets(mixer_meters);
        processor.start_processing()?;
    }
    let stream_result = (|| {
        loop {
            if cancelled.load(Ordering::Acquire) || output.is_stopped() {
                return Err("Mixer input monitoring was stopped".to_owned());
            }
            let frame_count = recording.read_frames(&mut input_frames);
            if frame_count == 0 {
                if recording.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            let sample_count = frame_count * 2;
            let block = &mut output_block[..sample_count];
            block.fill(0.0);
            let active_bus_samples = MIXER_TRACK_METER_COUNT * sample_count * mixer_bus_planes;
            let mut buses = mixer_buses
                .as_deref_mut()
                .map(|buses| &mut buses[..active_bus_samples]);
            if let Some(buses) = buses.as_deref_mut() {
                buses.fill(0.0);
            }
            if let Some(buses) = post_effect_buses.as_deref_mut() {
                buses[..MIXER_TRACK_METER_COUNT * sample_count].fill(0.0);
            }
            for (frame_index, input_frame) in input_frames[..frame_count].iter().enumerate() {
                if !route_is_audible {
                    continue;
                }
                let mut routed_frame = *input_frame;
                let live_controls = live_source_control
                    .as_ref()
                    .zip(live_source_targets.as_ref())
                    .map(|(control, targets)| control.next_frame_controls(targets));
                if let (Some(control), Some(controls)) =
                    (live_source_control.as_ref(), live_controls)
                {
                    control.apply_output_frame(controls, &mut routed_frame);
                } else {
                    route_plan.output_transform.apply_frame(&mut routed_frame);
                }
                block[frame_index * 2] = routed_frame[0];
                block[frame_index * 2 + 1] = routed_frame[1];
                if let Some(buses) = buses.as_deref_mut() {
                    let meter_bus_len = MIXER_TRACK_METER_COUNT * sample_count;
                    let meter_transforms = live_source_control.as_ref().zip(live_controls).map_or(
                        route_plan.insert_meter_transforms.as_slice(),
                        |(control, _)| control.insert_meter_transforms(),
                    );
                    for (track, transform) in meter_transforms {
                        if *track >= MIXER_TRACK_METER_COUNT {
                            return Err(
                                "Mixer input monitor meter route exceeds the supported track range"
                                    .to_owned(),
                            );
                        }
                        let mut meter_frame = *input_frame;
                        if let (Some(control), Some(controls)) =
                            (live_source_control.as_ref(), live_controls)
                        {
                            control.apply_meter_frame(
                                controls,
                                *track,
                                *transform,
                                &mut meter_frame,
                            );
                        } else {
                            transform.apply_frame(&mut meter_frame);
                        }
                        let offset = *track * sample_count + frame_index * 2;
                        buses[offset] += meter_frame[0];
                        buses[offset + 1] += meter_frame[1];
                    }
                    if mixer_effect_buses_required {
                        let Some(effect_input_buses) = buses.get_mut(meter_bus_len..) else {
                            return Err(
                                "Mixer input monitor effect buses are incomplete".to_owned()
                            );
                        };
                        let effect_input_transforms =
                            live_source_control.as_ref().zip(live_controls).map_or(
                                route_plan.insert_effect_input_transforms.as_slice(),
                                |(control, _)| control.insert_effect_input_transforms(),
                            );
                        for (track, transform) in effect_input_transforms {
                            if *track >= MIXER_TRACK_METER_COUNT {
                                return Err(
                                    "Mixer input monitor effect route exceeds the supported track range"
                                        .to_owned(),
                                );
                            }
                            let mut effect_frame = *input_frame;
                            if let (Some(control), Some(controls)) =
                                (live_source_control.as_ref(), live_controls)
                            {
                                control.apply_effect_input_frame(
                                    controls,
                                    *track,
                                    *transform,
                                    &mut effect_frame,
                                );
                            } else {
                                transform.apply_frame(&mut effect_frame);
                            }
                            let offset = *track * sample_count + frame_index * 2;
                            effect_input_buses[offset] += effect_frame[0];
                            effect_input_buses[offset + 1] += effect_frame[1];
                        }
                    }
                }
            }
            if let Some(processor) = vst3_processor.as_mut() {
                if let Some(buses) = buses.as_deref_mut() {
                    if let Some(post_effect_buses) = post_effect_buses.as_deref_mut() {
                        processor.mix_next_block_with_post_effect_buses(
                            block,
                            buses,
                            &mut post_effect_buses[..MIXER_TRACK_METER_COUNT * sample_count],
                        )?;
                    } else {
                        processor.mix_next_block_with_meter_buses(block, buses)?;
                    }
                } else {
                    processor.mix_next_block(block)?;
                }
            }
            if let Some(buses) = buses.as_deref() {
                for (track, track_samples) in buses[..MIXER_TRACK_METER_COUNT * sample_count]
                    .chunks_exact(sample_count)
                    .enumerate()
                    .skip(1)
                {
                    let (frames, _) = track_samples.as_chunks::<2>();
                    let (left_peak, right_peak) =
                        frames
                            .iter()
                            .fold((0.0_f32, 0.0_f32), |(left_peak, right_peak), frame| {
                                (
                                    left_peak.max(frame[0].abs()),
                                    right_peak.max(frame[1].abs()),
                                )
                            });
                    mixer_meters.publish_stereo_peak(track, left_peak, right_peak);
                    let post_effect_track_samples =
                        post_effect_buses
                            .as_deref()
                            .map_or(track_samples, |post_effect_buses| {
                                let start = track * sample_count;
                                &post_effect_buses[start..start + sample_count]
                            });
                    let (post_effect_frames, _) = post_effect_track_samples.as_chunks::<2>();
                    let (post_effect_left_peak, post_effect_right_peak) = post_effect_frames
                        .iter()
                        .fold((0.0_f32, 0.0_f32), |(left_peak, right_peak), frame| {
                            (
                                left_peak.max(frame[0].abs()),
                                right_peak.max(frame[1].abs()),
                            )
                        });
                    mixer_meters.publish_post_effect_stereo_peak(
                        track,
                        post_effect_left_peak,
                        post_effect_right_peak,
                    );
                }
            }
            master_output.apply(block);
            output.write_stereo_samples(block)?;
        }
        Ok::<_, String>(())
    })();
    let stop_result = if let Some(processor) = vst3_processor.as_mut() {
        processor.stop_processing()
    } else {
        Ok(())
    };
    stream_result.and(stop_result)
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
    let render = prepare_audio_clip_render(
        document,
        project_path.as_ref(),
        options.clone(),
        false,
        Some(cancelled),
    )?;
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
/// Linear Playlist `TEMPO` automation updates audio, Sampler, and VST3 note timing. Playlist
/// Audio Clip channel Resample, Stretch, Elastique 3 Generic, and mono-only Elastique 3 Mono
/// modes are prepared before playback; stretch through tempo automation, other stretch modes,
/// parameter automation, and Mixer effects remain incomplete.
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
        PlaylistLiveStreamControls {
            monitor: PlaylistStreamMonitor::from_frame(start_frame),
            latency_updates: None,
        },
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
        PlaylistLiveStreamControls {
            monitor,
            latency_updates: None,
        },
    )
}

/// Stream Playlist playback with block-boundary VST3 latency refreshes.
pub fn stream_playlist_with_vst3_to_device_with_monitor_and_latency_updates(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    controls: PlaylistLiveStreamControls<'_>,
) -> Result<PlaylistRenderSummary, String> {
    stream_playlist_with_vst3_to_device_from_frame_inner(
        document,
        project_path,
        options,
        writer,
        cancelled,
        vst3_processor,
        controls,
    )
}

/// Stream arrangement audio while accepting quantized Audio and Sampler Clip launches.
///
/// The arrangement must contain supported clips before the named Start marker. Launches replace
/// content from the same Playlist track and repeat at the clip's tick length. Loaded VST3 Pattern
/// Clips can receive live launches alongside Sampler and Audio Clips.
pub fn stream_playlist_performance_with_vst3_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    performance: PlaylistPerformanceStream,
) -> Result<PlaylistRenderSummary, String> {
    let project_path = project_path.as_ref();
    let loaded_vst3_channels = vst3_processor
        .as_ref()
        .map(Vst3PlaylistStreamProcessor::performance_channel_ids)
        .unwrap_or_default();
    let vst3_volume_expression_channels = vst3_processor
        .as_ref()
        .map(Vst3PlaylistStreamProcessor::performance_volume_expression_channel_ids)
        .unwrap_or_default();
    let vst3_crossfade_volume_expression_channels = vst3_processor
        .as_ref()
        .map(|processor| processor.volume_expression_channel_ids().clone())
        .unwrap_or_default();
    let (static_document, performance_clip_indices) =
        playlist_performance_static_document_with_vst3(
            document,
            options.arrangement_id,
            performance.performance_zone_end_tick,
            &loaded_vst3_channels,
        )?;
    if performance_clip_indices.is_empty() {
        return Err("arrangement has no supported clips before the Start marker".into());
    }
    let mut performance_sampler_clip_indices = playlist_performance_sampler_clip_candidates(
        document,
        options.arrangement_id,
        performance.performance_zone_end_tick,
    )?;
    performance_sampler_clip_indices.extend(playlist_performance_mixed_source_clip_candidates(
        document,
        options.arrangement_id,
        performance.performance_zone_end_tick,
        &loaded_vst3_channels,
    )?);
    let mut performance_audio_clip_indices = playlist_performance_audio_clip_indices(
        document,
        options.arrangement_id,
        performance.performance_zone_end_tick,
    )?;
    performance_sampler_clip_indices.retain(|index| performance_clip_indices.contains(index));
    performance_audio_clip_indices.retain(|index| performance_clip_indices.contains(index));
    let mut performance_vst3_clip_indices = playlist_performance_vst3_clip_indices(
        document,
        options.arrangement_id,
        performance.performance_zone_end_tick,
        &loaded_vst3_channels,
    )?;
    performance_vst3_clip_indices.retain(|index| performance_clip_indices.contains(index));
    let mut audio = prepare_audio_clip_render(
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
            clip_indices: Vec::new(),
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
            clip_envelopes: Arc::clone(&options.clip_envelopes),
        },
        performance.meters.is_some(),
        Some(cancelled),
    )?;
    let mut sampler = prepare_sampler_arrangement(
        document,
        project_path,
        options.clone(),
        performance.meters.is_some(),
        &vst3_crossfade_volume_expression_channels,
        Some(cancelled),
    )?;
    let performance_pattern_automation = vst3_processor
        .as_ref()
        .map(Vst3PlaylistStreamProcessor::performance_pattern_automation)
        .cloned()
        .unwrap_or_default();
    let runtime = PlaylistPerformanceRuntime::new(
        document,
        options.clone(),
        performance.performance_zone_end_tick,
        performance.start_frame,
        PlaylistPerformanceControls {
            launches: performance.launches,
            mixer_routing_updates: performance.mixer_routing_updates,
            latency_updates: performance.latency_updates,
            latency_reports: performance.latency_reports,
            live_status: performance.live_status,
        },
        PlaylistPerformanceSources {
            sampler_clip_indices: &performance_sampler_clip_indices,
            audio_clip_indices: &performance_audio_clip_indices,
            vst3_clip_indices: &performance_vst3_clip_indices,
            vst3_channel_ids: &loaded_vst3_channels,
            vst3_volume_expression_channel_ids: &vst3_volume_expression_channels,
            pattern_automation: &performance_pattern_automation,
            sampler: &sampler,
            audio: &audio,
        },
    )?;
    audio
        .clips
        .retain(|clip| !performance_audio_clip_indices.contains(&clip.clip_index));
    sampler
        .notes
        .retain(|note| !performance_sampler_clip_indices.contains(&note.clip_index));

    let static_frames = audio.summary.frames.max(sampler.summary.frames);
    if let Some(processor) = vst3_processor.as_mut() {
        let vst3_summary = processor.summary();
        if static_frames == 0
            && vst3_summary.plugin_channels_rendered == 0
            && !vst3_summary.unloaded_plugin_channels.is_empty()
        {
            let channel_ids = vst3_summary
                .unloaded_plugin_channels
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "arrangement has VST3 notes on channel(s) {channel_ids}, but no matching plug-in instance is loaded"
            ));
        }
        let content_frames = static_frames.max(processor.content_frames());
        processor.extend_to_output_frames(content_frames)?;
    }
    let mut source_paths = audio.source_paths.iter().cloned().collect::<BTreeSet<_>>();
    source_paths.extend(sampler.source_paths.iter().cloned());
    let vst3_summary = vst3_processor.as_ref().map(|processor| processor.summary());
    let mut summary = PlaylistRenderSummary {
        frames: static_frames.max(vst3_summary.as_ref().map_or(0, |summary| summary.frames)),
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
    let mut frames_to_skip = performance.start_frame;
    let mut master_output = MixerMasterOutput::from_document(&static_document);
    if let Some(meters) = performance.meters.as_deref() {
        master_output = master_output.with_live_master_targets(meters, options.sample_rate);
    }
    stream_prepared_playlist_render_with_performance(
        PreparedPlaylistBlockMix {
            audio: &audio,
            sampler: &sampler,
            options,
            frames: u64::MAX,
            master_output,
            vst3_processor: vst3_processor.as_mut(),
            remainder_stop: None,
        },
        PlaylistPreparedRenderControls {
            performance: Some(runtime),
            latency_updates: None,
            meters: performance.meters.as_deref(),
            cancelled,
            capture_post_effect_buses: false,
        },
        || writer.is_cancelled(),
        |block, _mixer_buses| {
            writer.write_stereo_samples(skip_render_block_prefix(block, &mut frames_to_skip))
        },
    )?;
    if let Some(processor) = vst3_processor.as_ref() {
        summary.vst3_notes_rendered = processor.summary().notes_scheduled;
    }
    Ok(summary)
}

fn stream_playlist_with_vst3_to_device_from_frame_inner(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    controls: PlaylistLiveStreamControls<'_>,
) -> Result<PlaylistRenderSummary, String> {
    let PlaylistLiveStreamControls {
        monitor,
        latency_updates: latency_controls,
    } = controls;
    let start_frame = monitor.start_frame;
    let meters = monitor.meters;
    let live_master_fader_gain = monitor.live_master_fader_gain;
    let live_master_pan_raw = monitor.live_master_pan_raw;
    let live_master_stereo_separation_raw = monitor.live_master_stereo_separation_raw;
    let live_master_enabled = monitor.live_master_enabled;
    let live_master_signal_flags = monitor.live_master_signal_flags;
    let project_path = project_path.as_ref();
    let mut master_output = MixerMasterOutput::from_document(document);
    if let Some(gain_target) = live_master_fader_gain {
        master_output = master_output.with_live_fader_gain_target(gain_target, options.sample_rate);
    }
    if let Some(pan_target) = live_master_pan_raw {
        master_output = master_output.with_live_pan_raw_target(pan_target, options.sample_rate);
    }
    if let Some(separation_target) = live_master_stereo_separation_raw {
        master_output = master_output
            .with_live_stereo_separation_target(separation_target, options.sample_rate);
    }
    if let Some(enabled_target) = live_master_enabled {
        master_output = master_output.with_live_enabled_target(enabled_target, options.sample_rate);
    }
    if let Some(signal_flags_target) = live_master_signal_flags {
        master_output =
            master_output.with_live_signal_flags_target(signal_flags_target, options.sample_rate);
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
            clip_indices: Vec::new(),
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
            clip_envelopes: Arc::clone(&options.clip_envelopes),
        },
        meters.is_some(),
        Some(cancelled),
    )?;
    let vst3_crossfade_volume_expression_channels = vst3_processor
        .as_ref()
        .map(|processor| processor.volume_expression_channel_ids().clone())
        .unwrap_or_default();
    let sampler = prepare_sampler_arrangement(
        document,
        project_path,
        options.clone(),
        meters.is_some(),
        &vst3_crossfade_volume_expression_channels,
        Some(cancelled),
    )?;
    if audio.clips.is_empty()
        && sampler.notes.is_empty()
        && options.clap_audio_layers.is_empty()
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
            "arrangement contains no supported audio clips, Sampler notes, CLAP audio, or loaded VST3 notes in Pattern Clips"
                .to_owned(),
        );
    }

    let mut frames = audio
        .summary
        .frames
        .max(sampler.summary.frames)
        .max(playlist_clap_audio_frames(&options.clap_audio_layers));
    if let Some(processor) = vst3_processor.as_mut() {
        frames = frames.max(processor.content_frames());
        processor.extend_to_output_frames(frames)?;
        frames = frames.max(processor.summary().frames);
    }
    let mut source_paths: std::collections::BTreeSet<PathBuf> =
        audio.source_paths.iter().cloned().collect();
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

    let latency_updates = latency_controls.map(|controls| LiveLatencyUpdateState {
        document: document.clone(),
        pdc_updates: controls.pdc_updates,
        updates: controls.updates,
        reports: controls.reports,
    });
    let mut frames_to_skip = start_frame.min(frames);
    let render = PreparedPlaylistBlockMix {
        audio: &audio,
        sampler: &sampler,
        options,
        frames,
        master_output,
        vst3_processor: vst3_processor.as_mut(),
        remainder_stop: None,
    };
    let block_summary = if let Some(latency_updates) = latency_updates {
        stream_prepared_playlist_render_with_live_latency_updates(
            render,
            latency_updates,
            meters,
            cancelled,
            || writer.is_cancelled(),
            |block, _mixer_buses| {
                let skipped_frames = frames_to_skip.min((block.len() / 2) as u64) as usize;
                frames_to_skip -= skipped_frames as u64;
                writer.write_stereo_samples(&block[skipped_frames * 2..])
            },
        )?
    } else {
        stream_prepared_playlist_render(
            render,
            meters,
            cancelled,
            || writer.is_cancelled(),
            |block, _mixer_buses| {
                let skipped_frames = frames_to_skip.min((block.len() / 2) as u64) as usize;
                frames_to_skip -= skipped_frames as u64;
                writer.write_stereo_samples(&block[skipped_frames * 2..])
            },
        )?
    };
    summary.voices_stolen = block_summary.voices_stolen;
    summary.frames = summary.frames.max(block_summary.frames_rendered);
    if let Some(processor) = vst3_processor.as_ref() {
        summary.frames = summary.frames.max(processor.summary().frames);
    }
    Ok(summary)
}

/// Render enabled Playlist audio clips, Sampler Pattern Clips, and mapped VST3 instruments to a
/// stereo WAV. Audio is mixed in bounded blocks and written to a temporary file, so song length
/// does not determine the in-memory mix size. The recognized Master mute, polarity, and stereo-swap
/// flags are applied to the final mix. Linear Playlist tempo automation is applied to audio,
/// Sampler, and VST3 event timing. Automatic PDC aligns supported direct-to-Master sources using
/// loaded VST3 instrument latency. Supported direct and paired send routes apply recognized
/// insert mute/solo and signal transforms; the summary includes their pre-effect insert peaks.
/// The attached VST3 processor applies supported instrument and Mixer effect chains, route PDC,
/// and linked Pattern Event, LFO, and type-5 Automation Clip parameters. Native effect formats, other
/// effect automation links, and FL Studio output comparison remain incomplete.
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
        PlaylistRenderWindow {
            output: PlaylistRenderOutput::Wav,
            output_start_frame: 0,
            minimum_content_frames: 0,
            full_song_wav: true,
        },
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
        PlaylistRenderWindow {
            output: PlaylistRenderOutput::Flac { bits_per_sample },
            output_start_frame: 0,
            minimum_content_frames: 0,
            full_song_wav: false,
        },
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
        PlaylistRenderWindow {
            output: PlaylistRenderOutput::Ogg { bitrate_kbps },
            output_start_frame: 0,
            minimum_content_frames: 0,
            full_song_wav: false,
        },
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
        PlaylistRenderWindow {
            output: PlaylistRenderOutput::Mp3 { bitrate_kbps },
            output_start_frame: 0,
            minimum_content_frames: 0,
            full_song_wav: false,
        },
        vst3_processor,
        cancelled,
    )
}

/// Render from song start while writing only the portion beginning at `output_start_frame`.
/// Processing before that frame is retained so tempo automation and plug-in state follow the
/// original arrangement timeline. `minimum_content_frames` retains silence through a clip's end.
pub fn render_playlist_with_vst3_to_file_from_frame_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    range: PlaylistRenderRange,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    let output = match range.output_format {
        AudioRenderOutput::Wav => PlaylistRenderOutput::Wav,
        AudioRenderOutput::Flac { bits_per_sample } => {
            PlaylistRenderOutput::Flac { bits_per_sample }
        }
        AudioRenderOutput::Ogg { bitrate_kbps } => PlaylistRenderOutput::Ogg { bitrate_kbps },
        AudioRenderOutput::Mp3 { bitrate_kbps } => PlaylistRenderOutput::Mp3 { bitrate_kbps },
    };
    render_playlist_with_vst3_cancellable(
        document,
        project_path,
        options,
        output_path,
        PlaylistRenderWindow {
            output,
            output_start_frame: range.output_start_frame,
            minimum_content_frames: range.minimum_content_frames,
            full_song_wav: false,
        },
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

#[derive(Clone, Copy)]
struct PlaylistRenderWindow {
    output: PlaylistRenderOutput,
    output_start_frame: u64,
    minimum_content_frames: u64,
    full_song_wav: bool,
}

fn render_playlist_with_vst3_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    window: PlaylistRenderWindow,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    let PlaylistRenderWindow {
        output,
        output_start_frame,
        minimum_content_frames,
        full_song_wav,
    } = window;
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
            clip_indices: Vec::new(),
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
            clip_envelopes: Arc::clone(&options.clip_envelopes),
        },
        false,
        Some(cancelled),
    )?;
    let vst3_crossfade_volume_expression_channels = vst3_processor
        .as_ref()
        .map(|processor| processor.volume_expression_channel_ids().clone())
        .unwrap_or_default();
    let sampler = prepare_sampler_arrangement(
        document,
        project_path,
        options.clone(),
        false,
        &vst3_crossfade_volume_expression_channels,
        Some(cancelled),
    )?;
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

    let mut content_frames = audio
        .summary
        .frames
        .max(sampler.summary.frames)
        .max(minimum_content_frames);
    if let Some(processor) = vst3_processor.as_mut() {
        content_frames = content_frames.max(processor.content_frames());
    }
    let mut wav_cue_points = if matches!(output, PlaylistRenderOutput::Wav)
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
    if output_start_frame > 0 {
        wav_cue_points.retain_mut(|point| {
            let Some(offset) = u64::from(point.sample_offset).checked_sub(output_start_frame)
            else {
                return false;
            };
            let Ok(offset) = u32::try_from(offset) else {
                return false;
            };
            point.sample_offset = offset;
            true
        });
    }
    let minimum_tail_end_frame =
        add_render_tail_frames(content_frames, options.sample_rate, options.tail_seconds)?;
    let mut frames = match options.tail_mode {
        PlaylistTailMode::LeaveRemainder | PlaylistTailMode::WrapRemainder => {
            add_render_tail_frames(
                minimum_tail_end_frame,
                options.sample_rate,
                PLAYLIST_REMAINDER_MAX_SECONDS,
            )?
        }
        PlaylistTailMode::FixedDuration => minimum_tail_end_frame,
    };
    let remainder_stop = matches!(
        options.tail_mode,
        PlaylistTailMode::LeaveRemainder | PlaylistTailMode::WrapRemainder
    )
    .then(|| PlaylistRemainderStop {
        start_frame: minimum_tail_end_frame.max(output_start_frame),
        silence_frames: u64::from(options.sample_rate)
            .saturating_mul(PLAYLIST_REMAINDER_SILENCE_SECONDS),
    });
    if output_start_frame >= frames {
        return Err("render output start is after the end of the Playlist content".to_owned());
    }
    if let Some(processor) = vst3_processor.as_mut() {
        processor.extend_to_output_frames(frames)?;
        frames = frames.max(processor.summary().frames);
    }
    let pdc_trim_frames = if full_song_wav
        && matches!(output, PlaylistRenderOutput::Wav)
        && options.wav_trim_pdc_silence
        && output_start_frame == 0
    {
        vst3_processor
            .as_ref()
            .map_or(0, Vst3PlaylistStreamProcessor::pdc_render_start_trim_frames)
    } else {
        0
    };
    let render_skip_frames = output_start_frame.saturating_add(pdc_trim_frames);
    if render_skip_frames >= frames {
        return Err("PDC trim removes the entire Playlist render".to_owned());
    }
    let output_frames = if options.tail_mode == PlaylistTailMode::WrapRemainder {
        content_frames
            .checked_sub(render_skip_frames)
            .filter(|frames| *frames > 0)
            .ok_or_else(|| {
                "render output start is after the end of the Playlist content".to_owned()
            })?
    } else {
        frames - render_skip_frames
    };
    if matches!(output, PlaylistRenderOutput::Flac { .. }) && output_frames > FLAC_MAX_SAMPLE_FRAMES
    {
        return Err("rendered FLAC exceeds the 36-bit sample-count limit".to_owned());
    }
    let mut source_paths: std::collections::BTreeSet<PathBuf> =
        audio.source_paths.iter().cloned().collect();
    source_paths.extend(sampler.source_paths.iter().cloned());
    let vst3_summary = vst3_processor.as_ref().map(|processor| processor.summary());
    let mut summary = PlaylistRenderSummary {
        frames: output_frames,
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
    if options.tail_mode == PlaylistTailMode::WrapRemainder {
        let mut wrap_frames_to_skip = render_skip_frames;
        let mut float_spool = tempfile::tempfile()
            .map_err(|error| format!("could not create temporary render spool: {error}"))?;
        let render_result = stream_prepared_playlist_render(
            PreparedPlaylistBlockMix {
                audio: &audio,
                sampler: &sampler,
                options: options.clone(),
                frames,
                master_output,
                vst3_processor: vst3_processor.as_mut(),
                remainder_stop,
            },
            Some(&mixer_track_meters),
            cancelled,
            || false,
            |block, _mixer_buses| {
                let block = skip_render_block_prefix(block, &mut wrap_frames_to_skip);
                write_float_spool_block(&mut float_spool, block)
            },
        )?;
        summary.voices_stolen = render_result.voices_stolen;
        let spool_frames = render_result
            .frames_rendered
            .checked_sub(render_skip_frames)
            .ok_or_else(|| {
                "render output start is after the rendered Playlist content".to_owned()
            })?;
        if spool_frames < output_frames {
            return Err("wrapped render ended before the Playlist content endpoint".to_owned());
        }
        wrap_float_spool_remainder(&mut float_spool, output_frames, spool_frames)?;
        encode_float_spool(
            file,
            &mut float_spool,
            output_frames,
            options.sample_rate,
            AudioRenderEncodingOptions {
                wav_sample_format: options.wav_sample_format,
                wav_dither_mode: options.wav_dither_mode,
                wav_channel_mode: options.wav_channel_mode,
            },
            playlist_audio_render_output(output),
        )?;
        summary.frames = output_frames;
        if matches!(output, PlaylistRenderOutput::Wav) {
            write_wav_cue_points(file, &wav_cue_points)?;
            patch_wav_data_length(
                file,
                output_frames,
                options.wav_sample_format,
                options.wav_channel_mode,
            )?;
        }
        summary.mixer_insert_peaks = Some(take_mixer_insert_peaks(&mixer_track_meters));
        temporary.commit(output_path)?;
        return Ok(summary);
    }

    let mut frames_to_skip = render_skip_frames;
    let rendered_output_frames;
    let wav_sample_format = options.wav_sample_format;
    let wav_channel_mode = options.wav_channel_mode;
    match output {
        PlaylistRenderOutput::Wav => {
            let block_align = options.wav_sample_format.bytes_per_sample()
                * options.wav_channel_mode.channel_count();
            let header_frames = if remainder_stop.is_some() {
                1
            } else {
                u32::try_from(output_frames)
                    .map_err(|_| "render is too long for a RIFF/WAVE file".to_owned())?
            };
            let data_bytes = header_frames
                .checked_mul(u32::from(block_align))
                .filter(|bytes| *bytes <= u32::MAX - 36)
                .ok_or_else(|| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;
            write_wav_header(
                file,
                options.sample_rate,
                header_frames,
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
            let render_result = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options: options.clone(),
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                    remainder_stop,
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _mixer_buses| {
                    let block = skip_render_block_prefix(block, &mut frames_to_skip);
                    if block.is_empty() {
                        return Ok(());
                    }
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
            summary.voices_stolen = render_result.voices_stolen;
            rendered_output_frames = render_result
                .frames_rendered
                .checked_sub(render_skip_frames)
                .ok_or_else(|| {
                    "render output start is after the rendered Playlist content".to_owned()
                })?;
        }
        PlaylistRenderOutput::Flac { bits_per_sample } => {
            let mut encoder = StreamingFlacWriter::new(
                file,
                options.sample_rate,
                options.wav_channel_mode,
                bits_per_sample,
            )?;
            let render_result = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options: options.clone(),
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                    remainder_stop,
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _mixer_buses| {
                    let block = skip_render_block_prefix(block, &mut frames_to_skip);
                    if block.is_empty() {
                        Ok(())
                    } else {
                        encoder.write_stereo_block(block, options.wav_channel_mode)
                    }
                },
            )?;
            summary.voices_stolen = render_result.voices_stolen;
            rendered_output_frames = render_result
                .frames_rendered
                .checked_sub(render_skip_frames)
                .ok_or_else(|| {
                    "render output start is after the rendered Playlist content".to_owned()
                })?;
            encoder.finalize(rendered_output_frames)?;
        }
        PlaylistRenderOutput::Ogg { bitrate_kbps } => {
            let mut encoder = StreamingOggWriter::new(
                file,
                options.sample_rate,
                options.wav_channel_mode,
                bitrate_kbps,
            )?;
            let render_result = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options,
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                    remainder_stop,
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _mixer_buses| {
                    let block = skip_render_block_prefix(block, &mut frames_to_skip);
                    if block.is_empty() {
                        Ok(())
                    } else {
                        encoder.write_stereo_block(block)
                    }
                },
            )?;
            summary.voices_stolen = render_result.voices_stolen;
            rendered_output_frames = render_result
                .frames_rendered
                .checked_sub(render_skip_frames)
                .ok_or_else(|| {
                    "render output start is after the rendered Playlist content".to_owned()
                })?;
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
            let render_result = stream_prepared_playlist_render(
                PreparedPlaylistBlockMix {
                    audio: &audio,
                    sampler: &sampler,
                    options,
                    frames,
                    master_output,
                    vst3_processor: vst3_processor.as_mut(),
                    remainder_stop,
                },
                Some(&mixer_track_meters),
                cancelled,
                || false,
                |block, _mixer_buses| {
                    let block = skip_render_block_prefix(block, &mut frames_to_skip);
                    if block.is_empty() {
                        Ok(())
                    } else {
                        encoder.write_stereo_block(block)
                    }
                },
            )?;
            summary.voices_stolen = render_result.voices_stolen;
            rendered_output_frames = render_result
                .frames_rendered
                .checked_sub(render_skip_frames)
                .ok_or_else(|| {
                    "render output start is after the rendered Playlist content".to_owned()
                })?;
            encoder.finalize()?;
        }
    }
    summary.frames = rendered_output_frames;
    if matches!(output, PlaylistRenderOutput::Wav) {
        write_wav_cue_points(file, &wav_cue_points)?;
        patch_wav_data_length(
            file,
            rendered_output_frames,
            wav_sample_format,
            wav_channel_mode,
        )?;
    }
    summary.mixer_insert_peaks = Some(take_mixer_insert_peaks(&mixer_track_meters));
    temporary.commit(output_path)?;
    Ok(summary)
}

/// Render supported Mixer insert buses to separate WAV files, optionally including Master and Current.
///
/// Insert stems include loaded VST3 insert effects in saved order, supported direct routes, paired
/// sends, insert level/pan transforms, mute/solo state, and route PDC. Only recognized, enabled
/// inserts routed from a project channel are written. The optional Master stem uses the final
/// supported mix after Master controls and loaded VST3 Master effects. The optional Current stem
/// uses its selected insert's pre-effect bus. Linked Pattern Event, LFO, and type-5 Automation Clip
/// parameters drive loaded VST3 effects. Native effect formats, other effect automation links,
/// unsupported route types, and PDC silence trimming remain incomplete.
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
            clip_indices: Vec::new(),
            start_from_song_start: false,
            soloed_playlist_track_range: options.soloed_playlist_track_range,
            clip_envelopes: Arc::clone(&options.clip_envelopes),
        },
        false,
        Some(cancelled),
    )?;
    let vst3_crossfade_volume_expression_channels = vst3_processor
        .as_ref()
        .map(|processor| processor.volume_expression_channel_ids().clone())
        .unwrap_or_default();
    let sampler = prepare_sampler_arrangement(
        document,
        project_path,
        options.clone(),
        false,
        &vst3_crossfade_volume_expression_channels,
        Some(cancelled),
    )?;
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
        content_frames = content_frames.max(processor.content_frames());
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
    let mut frames =
        add_render_tail_frames(content_frames, options.sample_rate, options.tail_seconds)?;
    if let Some(processor) = vst3_processor.as_mut() {
        processor.extend_to_output_frames(frames)?;
        frames = frames.max(processor.summary().frames);
    }
    let stem_pdc_trim_frames = stem_specs
        .iter()
        .map(|spec| {
            if options.wav_trim_pdc_silence {
                vst3_processor.as_ref().map_or(0, |processor| {
                    processor.pdc_mixer_stem_start_trim_frames(spec.track, !spec.is_current_stem)
                })
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let master_pdc_trim_frames = if options.wav_trim_pdc_silence {
        vst3_processor
            .as_ref()
            .map_or(0, Vst3PlaylistStreamProcessor::pdc_render_start_trim_frames)
    } else {
        0
    };
    let logical_output_frames = frames.saturating_sub(master_pdc_trim_frames);
    let longest_stem_delay = stem_pdc_trim_frames.iter().copied().max().unwrap_or(0);
    let longest_stem_frames = logical_output_frames
        .checked_add(longest_stem_delay)
        .ok_or_else(|| "Mixer stem output length overflow".to_owned())?;
    frames = frames.max(longest_stem_frames);
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
        .zip(stem_pdc_trim_frames)
        .map(|(spec, frames_to_skip)| {
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
                frames_to_skip,
                frames_to_write: logical_output_frames,
                frames_written: 0,
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
        audio.source_paths.iter().cloned().collect();
    source_paths.extend(sampler.source_paths.iter().cloned());
    let vst3_summary = vst3_processor.as_ref().map(|processor| processor.summary());
    let mut summary = PlaylistRenderSummary {
        frames: logical_output_frames,
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
    summary.voices_stolen = stream_prepared_playlist_render_with_post_effect_stems(
        PreparedPlaylistBlockMix {
            audio: &audio,
            sampler: &sampler,
            options: options.clone(),
            frames,
            master_output,
            vst3_processor: vst3_processor.as_mut(),
            remainder_stop: None,
        },
        Some(&meters),
        cancelled,
        || false,
        |block, mixer_buses| {
            let pre_effect_buses = mixer_buses
                .pre_effect
                .ok_or_else(|| "Mixer insert buses were not prepared for stem export".to_owned())?;
            let post_effect_buses = mixer_buses.post_effect.unwrap_or(pre_effect_buses);
            for output in &mut outputs {
                let stem_block = mixer_stem_block(
                    output.track,
                    block,
                    pre_effect_buses,
                    post_effect_buses,
                    output.is_current_stem,
                )?;
                let stem_block = skip_render_block_prefix(stem_block, &mut output.frames_to_skip);
                let frames_to_write = output.frames_to_write.saturating_sub(output.frames_written);
                let block_frames = ((stem_block.len() / 2) as u64).min(frames_to_write) as usize;
                let stem_block = &stem_block[..block_frames * 2];
                output.frames_written = output
                    .frames_written
                    .checked_add((stem_block.len() / 2) as u64)
                    .ok_or_else(|| "Mixer stem frame count overflow".to_owned())?;
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
    )?
    .voices_stolen;
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
    summary.frames = outputs
        .iter()
        .map(|output| output.frames_written)
        .max()
        .unwrap_or(frames);
    for mut output in outputs {
        if output.peak > 0.0 {
            let file = output
                .temporary
                .file
                .as_mut()
                .expect("temporary Mixer stem file is open");
            write_wav_cue_points(file, &wav_cue_points)?;
            patch_wav_data_length(
                file,
                output.frames_written,
                options.wav_sample_format,
                options.wav_channel_mode,
            )?;
            output.temporary.commit(&output.output_path)?;
        }
    }
    Ok(summary)
}

fn mixer_stem_block<'a>(
    track: usize,
    master_block: &'a [f32],
    pre_effect_insert_buses: &'a [f32],
    post_effect_insert_buses: &'a [f32],
    current_stem: bool,
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
    let insert_buses = if current_stem {
        pre_effect_insert_buses
    } else {
        post_effect_insert_buses
    };
    if insert_buses.len() != MIXER_TRACK_METER_COUNT.saturating_mul(samples_per_track) {
        return Err("Mixer insert buses do not match the render block".into());
    }
    let start = track
        .checked_mul(samples_per_track)
        .ok_or_else(|| "Mixer stem offset overflow".to_owned())?;
    let end = start
        .checked_add(samples_per_track)
        .filter(|end| *end <= insert_buses.len())
        .ok_or_else(|| "Mixer stem block exceeds the insert meter buses".to_owned())?;
    Ok(&insert_buses[start..end])
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
    frames_to_skip: u64,
    frames_to_write: u64,
    frames_written: u64,
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

fn patch_wav_data_length(
    file: &mut File,
    frames: u64,
    sample_format: WavSampleFormat,
    channel_mode: WavChannelMode,
) -> Result<(), String> {
    let channels = channel_mode.channel_count();
    let block_align = sample_format.bytes_per_sample() * channels;
    let data_bytes = frames
        .checked_mul(u64::from(block_align))
        .and_then(|bytes| u32::try_from(bytes).ok())
        .filter(|bytes| *bytes <= u32::MAX - 36)
        .ok_or_else(|| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;
    let file_length = file
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("could not seek to the end of the WAV file: {error}"))?;
    let riff_size = u32::try_from(file_length.saturating_sub(8))
        .map_err(|_| "rendered WAV size exceeds the RIFF/WAVE limit".to_owned())?;
    file.seek(SeekFrom::Start(4))
        .and_then(|_| file.write_all(&riff_size.to_le_bytes()))
        .and_then(|()| file.seek(SeekFrom::Start(40)).map(|_| ()))
        .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
        .and_then(|()| file.seek(SeekFrom::End(0)).map(|_| ()))
        .map_err(|error| format!("could not patch rendered WAV length: {error}"))
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
    remainder_stop: Option<PlaylistRemainderStop>,
}

#[derive(Clone, Copy)]
struct PlaylistRemainderStop {
    start_frame: u64,
    silence_frames: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PlaylistBlockRenderSummary {
    voices_stolen: usize,
    frames_rendered: u64,
}

#[derive(Clone, Copy, Debug)]
struct PlaylistRemainderDetector {
    start_frame: u64,
    silence_frames: u64,
    quiet_frames: u64,
}

impl PlaylistRemainderDetector {
    fn new(stop: PlaylistRemainderStop) -> Self {
        Self {
            start_frame: stop.start_frame,
            silence_frames: stop.silence_frames,
            quiet_frames: 0,
        }
    }

    /// Returns the number of frames to keep from this block, stopping at the first qualifying
    /// quiet frame. The caller applies this only after the minimum rendered tail has elapsed.
    fn frames_to_keep(&mut self, block_start: u64, stereo_samples: &[f32]) -> (usize, bool) {
        let mut frames_to_keep = stereo_samples.len() / 2;
        let mut should_stop = false;
        for (index, frame) in stereo_samples.as_chunks::<2>().0.iter().enumerate() {
            let frame_position = block_start.saturating_add(index as u64);
            if frame_position < self.start_frame {
                self.quiet_frames = 0;
                continue;
            }

            if frame[0].abs().max(frame[1].abs()) <= PLAYLIST_REMAINDER_SILENCE_THRESHOLD {
                self.quiet_frames = self.quiet_frames.saturating_add(1);
            } else {
                self.quiet_frames = 0;
            }

            if self.quiet_frames >= self.silence_frames {
                frames_to_keep = index + 1;
                should_stop = true;
                break;
            }
        }
        (frames_to_keep, should_stop)
    }
}

#[derive(Clone, Debug, Default)]
struct MixerMasterOutput {
    enabled: Option<bool>,
    polarity_reversed: Option<bool>,
    swap_left_right: Option<bool>,
    fader_volume_raw: Option<i32>,
    mixer_pan_raw: Option<i32>,
    stereo_separation_raw: Option<i32>,
    mixer_pan_law_raw: Option<u8>,
    live_fader_gain_target: Option<Arc<AtomicU32>>,
    live_fader_gain_current: Cell<Option<f32>>,
    live_fader_smoothing_alpha: f32,
    live_pan_raw_target: Option<Arc<AtomicI32>>,
    live_pan_raw_current: Cell<Option<f32>>,
    live_pan_smoothing_alpha: f32,
    live_stereo_separation_raw_target: Option<Arc<AtomicI32>>,
    live_stereo_separation_raw_current: Cell<Option<f32>>,
    live_stereo_separation_smoothing_alpha: f32,
    live_enabled_target: Option<Arc<AtomicI32>>,
    live_enabled_gain_current: Cell<Option<f32>>,
    live_enabled_smoothing_alpha: f32,
    live_signal_flags_target: Option<Arc<AtomicI32>>,
    live_signal_flags_current: Cell<Option<(f32, f32)>>,
    live_signal_flags_smoothing_alpha: f32,
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
            ..Self::default()
        }
    }

    fn with_live_master_targets(self, meters: &MixerTrackPeakMeters, sample_rate: u32) -> Self {
        self.with_live_fader_gain_target(meters.live_master_fader_gain(), sample_rate)
            .with_live_pan_raw_target(meters.live_master_pan_raw(), sample_rate)
            .with_live_stereo_separation_target(
                meters.live_master_stereo_separation_raw(),
                sample_rate,
            )
            .with_live_enabled_target(meters.live_master_enabled(), sample_rate)
            .with_live_signal_flags_target(meters.live_master_signal_flags(), sample_rate)
    }

    fn with_live_fader_gain_target(
        mut self,
        gain_target: Arc<AtomicU32>,
        sample_rate: u32,
    ) -> Self {
        self.live_fader_gain_target = Some(gain_target);
        self.live_fader_smoothing_alpha =
            crate::audio::output_gain_smoothing_alpha(sample_rate as f32);
        self
    }

    fn with_live_pan_raw_target(mut self, pan_target: Arc<AtomicI32>, sample_rate: u32) -> Self {
        self.live_pan_raw_target = Some(pan_target);
        self.live_pan_smoothing_alpha =
            crate::audio::output_gain_smoothing_alpha(sample_rate as f32);
        self
    }

    fn with_live_stereo_separation_target(
        mut self,
        separation_target: Arc<AtomicI32>,
        sample_rate: u32,
    ) -> Self {
        self.live_stereo_separation_raw_target = Some(separation_target);
        self.live_stereo_separation_smoothing_alpha =
            crate::audio::output_gain_smoothing_alpha(sample_rate as f32);
        self
    }

    fn with_live_enabled_target(
        mut self,
        enabled_target: Arc<AtomicI32>,
        sample_rate: u32,
    ) -> Self {
        self.live_enabled_target = Some(enabled_target);
        self.live_enabled_smoothing_alpha =
            crate::audio::output_gain_smoothing_alpha(sample_rate as f32);
        self
    }

    fn with_live_signal_flags_target(
        mut self,
        signal_flags_target: Arc<AtomicI32>,
        sample_rate: u32,
    ) -> Self {
        self.live_signal_flags_target = Some(signal_flags_target);
        self.live_signal_flags_smoothing_alpha =
            crate::audio::output_gain_smoothing_alpha(sample_rate as f32);
        self
    }

    fn apply(&self, stereo_samples: &mut [f32]) {
        if self.enabled == Some(false) && self.live_enabled_target.is_none() {
            stereo_samples.fill(0.0);
            return;
        }
        let (frames, _) = stereo_samples.as_chunks_mut::<2>();
        let master_transform = MixerInsertSignalTransform::default()
            .with_stereo_separation_raw(if self.live_stereo_separation_raw_target.is_some() {
                None
            } else {
                self.stereo_separation_raw
            })
            .with_mixer_pan_raw(if self.live_pan_raw_target.is_some() {
                None
            } else {
                self.mixer_pan_raw
            })
            .with_mixer_pan_law_raw(self.mixer_pan_law_raw)
            .with_fader_volume_raw(if self.live_fader_gain_target.is_some() {
                None
            } else {
                self.fader_volume_raw
            });
        for frame in frames {
            let (polarity_mix, swap_mix) = self
                .live_signal_flags_target
                .as_ref()
                .and_then(|target| {
                    let flags = target.load(Ordering::Acquire);
                    (0..=3).contains(&flags).then_some(flags)
                })
                .map_or_else(
                    || {
                        (
                            if self.polarity_reversed.unwrap_or(false) {
                                1.0
                            } else {
                                0.0
                            },
                            if self.swap_left_right.unwrap_or(false) {
                                1.0
                            } else {
                                0.0
                            },
                        )
                    },
                    |target_flags| {
                        let target_polarity_mix = if target_flags & 1 == 0 { 0.0 } else { 1.0 };
                        let target_swap_mix = if target_flags & 2 == 0 { 0.0 } else { 1.0 };
                        let (current_polarity_mix, current_swap_mix) =
                            self.live_signal_flags_current.get().unwrap_or_else(|| {
                                (
                                    if self.polarity_reversed.unwrap_or(false) {
                                        1.0
                                    } else {
                                        0.0
                                    },
                                    if self.swap_left_right.unwrap_or(false) {
                                        1.0
                                    } else {
                                        0.0
                                    },
                                )
                            });
                        let next_polarity_mix = crate::audio::smooth_output_gain(
                            current_polarity_mix,
                            target_polarity_mix,
                            self.live_signal_flags_smoothing_alpha,
                        );
                        let next_swap_mix = crate::audio::smooth_output_gain(
                            current_swap_mix,
                            target_swap_mix,
                            self.live_signal_flags_smoothing_alpha,
                        );
                        self.live_signal_flags_current
                            .set(Some((next_polarity_mix, next_swap_mix)));
                        (next_polarity_mix, next_swap_mix)
                    },
                );
            apply_master_signal_transform(frame, polarity_mix, swap_mix);
            master_transform.apply_frame(frame);
            if let Some(target) = &self.live_stereo_separation_raw_target {
                let target_separation_raw = target.load(Ordering::Acquire);
                if (-64..=64).contains(&target_separation_raw) {
                    let current_separation_raw = self
                        .live_stereo_separation_raw_current
                        .get()
                        .unwrap_or_else(|| self.stereo_separation_raw.unwrap_or(0) as f32);
                    let next_separation_raw = crate::audio::smooth_output_gain(
                        current_separation_raw,
                        target_separation_raw as f32,
                        self.live_stereo_separation_smoothing_alpha,
                    );
                    self.live_stereo_separation_raw_current
                        .set(Some(next_separation_raw));
                    MixerInsertSignalTransform::default()
                        .with_stereo_separation_raw(Some(next_separation_raw.round() as i32))
                        .apply_frame(frame);
                }
            }
            if let Some(target) = &self.live_pan_raw_target {
                let target_pan_raw = target.load(Ordering::Acquire);
                if (-6_400..=6_400).contains(&target_pan_raw) {
                    let current_pan_raw = self
                        .live_pan_raw_current
                        .get()
                        .unwrap_or_else(|| self.mixer_pan_raw.unwrap_or(0) as f32);
                    let next_pan_raw = crate::audio::smooth_output_gain(
                        current_pan_raw,
                        target_pan_raw as f32,
                        self.live_pan_smoothing_alpha,
                    );
                    self.live_pan_raw_current.set(Some(next_pan_raw));
                    MixerInsertSignalTransform::default()
                        .with_mixer_pan_raw(Some(next_pan_raw.round() as i32))
                        .with_mixer_pan_law_raw(self.mixer_pan_law_raw)
                        .apply_frame(frame);
                }
            }
            if let Some(target) = &self.live_fader_gain_target {
                let current_gain = self.live_fader_gain_current.get().unwrap_or_else(|| {
                    self.fader_volume_raw.map_or(1.0, |volume_raw| {
                        volume_raw.clamp(0, 16_000) as f32 / 10_000.0
                    })
                });
                let target_gain = f32::from_bits(target.load(Ordering::Acquire));
                let next_gain = crate::audio::smooth_output_gain(
                    current_gain,
                    target_gain,
                    self.live_fader_smoothing_alpha,
                );
                self.live_fader_gain_current.set(Some(next_gain));
                frame[0] *= next_gain;
                frame[1] *= next_gain;
            }
            if let Some(target) = &self.live_enabled_target {
                let target_enabled = match target.load(Ordering::Acquire) {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                }
                .or(self.enabled)
                .unwrap_or(true);
                let target_gain = if target_enabled { 1.0 } else { 0.0 };
                let current_gain = self.live_enabled_gain_current.get().unwrap_or_else(|| {
                    if self.enabled == Some(false) {
                        0.0
                    } else {
                        1.0
                    }
                });
                let next_gain = crate::audio::smooth_output_gain(
                    current_gain,
                    target_gain,
                    self.live_enabled_smoothing_alpha,
                );
                self.live_enabled_gain_current.set(Some(next_gain));
                frame[0] *= next_gain;
                frame[1] *= next_gain;
            }
        }
    }
}

pub(crate) fn apply_mixer_master_output_to_stereo(
    document: &FlpDocument,
    stereo_samples: &mut [f32],
) -> Result<(), String> {
    if !stereo_samples.len().is_multiple_of(2) {
        return Err("Mixer Master output requires interleaved stereo samples".to_owned());
    }
    MixerMasterOutput::from_document(document).apply(stereo_samples);
    Ok(())
}

fn apply_master_signal_transform(frame: &mut [f32; 2], polarity_mix: f32, swap_mix: f32) {
    crate::apply_mixer_signal_flags_f32(frame, polarity_mix, swap_mix);
}

fn stream_prepared_playlist_render(
    render: PreparedPlaylistBlockMix<'_>,
    meters: Option<&MixerTrackPeakMeters>,
    cancelled: &AtomicBool,
    stream_cancelled: impl FnMut() -> bool,
    write_block: impl FnMut(&[f32], PlaylistRenderMixerBuses<'_>) -> Result<(), String>,
) -> Result<PlaylistBlockRenderSummary, String> {
    stream_prepared_playlist_render_with_performance(
        render,
        PlaylistPreparedRenderControls {
            performance: None,
            latency_updates: None,
            meters,
            cancelled,
            capture_post_effect_buses: false,
        },
        stream_cancelled,
        write_block,
    )
}

fn stream_prepared_playlist_render_with_live_latency_updates(
    render: PreparedPlaylistBlockMix<'_>,
    latency_updates: LiveLatencyUpdateState,
    meters: Option<&MixerTrackPeakMeters>,
    cancelled: &AtomicBool,
    stream_cancelled: impl FnMut() -> bool,
    write_block: impl FnMut(&[f32], PlaylistRenderMixerBuses<'_>) -> Result<(), String>,
) -> Result<PlaylistBlockRenderSummary, String> {
    stream_prepared_playlist_render_with_performance(
        render,
        PlaylistPreparedRenderControls {
            performance: None,
            latency_updates: Some(latency_updates),
            meters,
            cancelled,
            capture_post_effect_buses: false,
        },
        stream_cancelled,
        write_block,
    )
}

fn stream_prepared_playlist_render_with_post_effect_stems(
    render: PreparedPlaylistBlockMix<'_>,
    meters: Option<&MixerTrackPeakMeters>,
    cancelled: &AtomicBool,
    stream_cancelled: impl FnMut() -> bool,
    write_block: impl FnMut(&[f32], PlaylistRenderMixerBuses<'_>) -> Result<(), String>,
) -> Result<PlaylistBlockRenderSummary, String> {
    stream_prepared_playlist_render_with_performance(
        render,
        PlaylistPreparedRenderControls {
            performance: None,
            latency_updates: None,
            meters,
            cancelled,
            capture_post_effect_buses: true,
        },
        stream_cancelled,
        write_block,
    )
}

#[derive(Clone, Copy)]
struct PlaylistRenderMixerBuses<'a> {
    pre_effect: Option<&'a [f32]>,
    post_effect: Option<&'a [f32]>,
}

struct PlaylistPreparedRenderControls<'runtime> {
    performance: Option<PlaylistPerformanceRuntime>,
    latency_updates: Option<LiveLatencyUpdateState>,
    meters: Option<&'runtime MixerTrackPeakMeters>,
    cancelled: &'runtime AtomicBool,
    capture_post_effect_buses: bool,
}

fn update_live_source_pdc_delays(
    delay_frames: u64,
    direct_source_delay: &mut crate::audio::StereoFrameDelay,
    mixer_bus_delays: &mut Vec<crate::audio::StereoFrameDelay>,
    mixer_bus_delay_count: usize,
) -> Result<(), String> {
    let delay_line_count = u64::try_from(mixer_bus_delay_count)
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| "live VST3 PDC delay count overflow".to_owned())?;
    let delay_bytes = delay_frames
        .checked_mul(delay_line_count)
        .and_then(|frames| frames.checked_mul(std::mem::size_of::<[f32; 2]>() as u64))
        .ok_or_else(|| "live VST3 PDC delay storage size overflow".to_owned())?;
    if delay_bytes > MAX_PDC_DELAY_BYTES {
        return Err(format!(
            "live VST3 PDC needs {delay_bytes} bytes of source delay storage, exceeding the 128 MiB limit"
        ));
    }
    direct_source_delay.set_delay_frames(delay_frames)?;
    if mixer_bus_delays.len() == mixer_bus_delay_count {
        for delay in mixer_bus_delays {
            delay.set_delay_frames(delay_frames)?;
        }
        return Ok(());
    }
    let previous = std::mem::take(mixer_bus_delays);
    let mut previous = previous.into_iter();
    let mut updated = Vec::new();
    updated
        .try_reserve_exact(mixer_bus_delay_count)
        .map_err(|error| format!("could not allocate live Mixer PDC delays: {error}"))?;
    for _ in 0..mixer_bus_delay_count {
        let mut delay = previous.next().unwrap_or_default();
        delay.set_delay_frames(delay_frames)?;
        updated.push(delay);
    }
    *mixer_bus_delays = updated;
    Ok(())
}

fn refresh_vst3_latency_updates(
    updates: &mut LiveLatencyUpdateState,
    vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    meters: Option<&MixerTrackPeakMeters>,
    direct_source_delay: &mut crate::audio::StereoFrameDelay,
    mixer_bus_delays: &mut Vec<crate::audio::StereoFrameDelay>,
    mixer_bus_delay_count: usize,
) -> Result<bool, String> {
    let mut plugin_ids = BTreeSet::new();
    while let Ok(changed_plugin_ids) = updates.updates.try_recv() {
        plugin_ids.extend(changed_plugin_ids);
    }
    if plugin_ids.is_empty() {
        return Ok(false);
    }
    let Some(processor) = vst3_processor else {
        return Ok(false);
    };
    let reports = processor.refresh_live_latency_reports(&plugin_ids, &updates.document, meters)?;
    if reports.is_empty() {
        return Ok(false);
    }
    for report in reports {
        let _ = updates.reports.send(report);
    }
    update_live_source_pdc_delays(
        processor.pdc_direct_source_delay_frames(),
        direct_source_delay,
        mixer_bus_delays,
        mixer_bus_delay_count,
    )?;
    Ok(true)
}

struct LiveMixerPdcUpdateContext<'a, 'engine> {
    meters: Option<&'a MixerTrackPeakMeters>,
    sampler_engine: Option<&'a mut SamplerVoiceEngine<'engine>>,
    audio_clips: Option<&'a [PreparedClip]>,
    sample_rate: u32,
    direct_source_delay: &'a mut crate::audio::StereoFrameDelay,
    mixer_bus_delays: &'a mut Vec<crate::audio::StereoFrameDelay>,
    mixer_bus_delay_count: usize,
}

fn apply_live_pdc_updates(
    updates: &mut LiveLatencyUpdateState,
    vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    context: LiveMixerPdcUpdateContext<'_, '_>,
) -> Result<bool, String> {
    let LiveMixerPdcUpdateContext {
        meters,
        sampler_engine,
        audio_clips,
        sample_rate,
        direct_source_delay,
        mixer_bus_delays,
        mixer_bus_delay_count,
    } = context;
    let mut latest_document = None;
    if let Some(receiver) = updates.pdc_updates.as_ref() {
        while let Ok(document) = receiver.try_recv() {
            latest_document = Some(document);
        }
    }
    let Some(document) = latest_document else {
        return Ok(false);
    };
    if let Some(sampler_engine) = sampler_engine {
        sampler_engine.update_live_mixer_routing(&document, meters);
    }
    if let Some(audio_clips) = audio_clips {
        refresh_prepared_clip_mixer_routes(
            audio_clips,
            &MixerRouteAudibility::from_document(&document),
            sample_rate,
        );
    }
    let Some(processor) = vst3_processor else {
        updates.document = document;
        return Ok(false);
    };
    processor.apply_live_mixer_routing(&document, meters)?;
    update_live_source_pdc_delays(
        processor.pdc_direct_source_delay_frames(),
        direct_source_delay,
        mixer_bus_delays,
        mixer_bus_delay_count,
    )?;
    updates.document = document;
    Ok(true)
}

fn stream_prepared_playlist_render_with_performance(
    mut render: PreparedPlaylistBlockMix<'_>,
    controls: PlaylistPreparedRenderControls<'_>,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32], PlaylistRenderMixerBuses<'_>) -> Result<(), String>,
) -> Result<PlaylistBlockRenderSummary, String> {
    let PlaylistPreparedRenderControls {
        mut performance,
        mut latency_updates,
        meters,
        cancelled,
        capture_post_effect_buses,
    } = controls;
    crate::audio::enable_denormal_protection();
    let mut frames = render.frames;
    let options = render.options;
    let mut remainder_detector = render.remainder_stop.map(PlaylistRemainderDetector::new);
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
    sampler_engine.tempo_map = Some(&render.sampler.tempo_map);
    sampler_engine.mixer_transforms_by_channel = Some(&render.sampler.mixer_transforms_by_channel);
    if let Some(meters) = meters {
        sampler_engine.live_mixer_source_routes_by_channel =
            Some(&render.sampler.live_mixer_source_routes_by_channel);
        sampler_engine.live_mixer_source_targets = Some(meters);
    }
    sampler_engine.mixer_meter_transforms_by_channel =
        Some(&render.sampler.mixer_meter_transforms_by_channel);
    sampler_engine.mixer_effect_input_transforms_by_channel =
        Some(&render.sampler.mixer_effect_input_transforms_by_channel);
    let clap_mixer_routes = prepare_playlist_clap_mixer_routes(
        &options.clap_audio_layers,
        render.sampler,
        meters,
        options.sample_rate,
    );
    let mut output = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
    let mut sampler_block = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
    let mixer_effect_buses_required = render
        .vst3_processor
        .as_deref()
        .is_some_and(Vst3PlaylistStreamProcessor::needs_mixer_insert_buses);
    let mixer_post_effect_buses_required =
        mixer_effect_buses_required && (capture_post_effect_buses || meters.is_some());
    let mixer_post_effect_stems_required = capture_post_effect_buses && mixer_effect_buses_required;
    let mixer_bus_planes = 1 + usize::from(mixer_effect_buses_required);
    let meter_plane_samples = MIXER_TRACK_METER_COUNT * STREAM_BLOCK_FRAMES * 2;
    let mut meter_buses = if meters.is_some() || mixer_effect_buses_required {
        vec![0.0f32; meter_plane_samples * mixer_bus_planes]
    } else {
        Vec::new()
    };
    let mut post_effect_buses =
        mixer_post_effect_buses_required.then(|| vec![0.0f32; meter_plane_samples]);
    let direct_source_delay_frames = render.vst3_processor.as_deref().map_or(
        0,
        Vst3PlaylistStreamProcessor::pdc_direct_source_delay_frames,
    );
    let mixer_bus_delay_count = if (meters.is_some() || mixer_effect_buses_required)
        && (performance.is_some() || latency_updates.is_some() || direct_source_delay_frames > 0)
    {
        MIXER_TRACK_METER_COUNT * mixer_bus_planes
    } else {
        0
    };
    let mut direct_source_delay = crate::audio::StereoFrameDelay::default();
    let mut mixer_bus_delays = Vec::new();
    update_live_source_pdc_delays(
        direct_source_delay_frames,
        &mut direct_source_delay,
        &mut mixer_bus_delays,
        mixer_bus_delay_count,
    )?;
    if let Some(processor) = render.vst3_processor.as_deref_mut() {
        if let Some(meters) = meters {
            processor.set_live_mixer_source_targets(meters);
        }
        processor.start_processing()?;
    }
    let track_groups = audio_clips_grouped_by_mixer_track(&render.audio.clips);
    let worker_count =
        audio_clip_mix_worker_count(&render.audio.clips, &track_groups, options.sample_rate);
    let worker_clips = audio_clip_groups_for_workers(&track_groups, worker_count);
    let audio_mix_context = PreparedAudioClipMixContext {
        clips: &render.audio.clips,
        audio_sources: &render.audio.audio_sources,
        sample_rate: options.sample_rate,
        resampling_quality: options.resampling_quality,
        cancelled,
        live_mixer_source_targets: meters,
    };
    let performance_audio_mix_context = PreparedAudioClipMixContext {
        clips: &render.audio.clips,
        audio_sources: &render.audio.audio_sources,
        sample_rate: options.sample_rate,
        resampling_quality: options.resampling_quality,
        cancelled,
        live_mixer_source_targets: meters,
    };
    let mix_result = std::thread::scope(|scope| {
        let mut parallel_mixer = if worker_count > 1 {
            ParallelAudioClipMixer::new(
                scope,
                worker_clips,
                &audio_mix_context,
                meters.is_some() || mixer_effect_buses_required,
                mixer_effect_buses_required,
            )
        } else {
            None
        };
        let mix_result = (|| {
            let mut block_start = 0u64;
            let mut frames_rendered = 0u64;
            while block_start < frames {
                check_cancelled(Some(cancelled))?;
                if stream_cancelled() {
                    return Err("Playlist playback was stopped".to_owned());
                }
                if let Some(latency_updates) = latency_updates.as_mut()
                    && apply_live_pdc_updates(
                        latency_updates,
                        render.vst3_processor.as_deref_mut(),
                        LiveMixerPdcUpdateContext {
                            meters,
                            sampler_engine: Some(&mut sampler_engine),
                            audio_clips: Some(&render.audio.clips),
                            sample_rate: options.sample_rate,
                            direct_source_delay: &mut direct_source_delay,
                            mixer_bus_delays: &mut mixer_bus_delays,
                            mixer_bus_delay_count,
                        },
                    )?
                    && let Some(processor) = render.vst3_processor.as_deref()
                {
                    frames = frames.max(processor.summary().frames);
                }
                if let Some(latency_updates) = latency_updates.as_mut()
                    && refresh_vst3_latency_updates(
                        latency_updates,
                        render.vst3_processor.as_deref_mut(),
                        meters,
                        &mut direct_source_delay,
                        &mut mixer_bus_delays,
                        mixer_bus_delay_count,
                    )?
                    && let Some(processor) = render.vst3_processor.as_deref()
                {
                    frames = frames.max(processor.summary().frames);
                }
                let frame_count = (frames - block_start).min(STREAM_BLOCK_FRAMES as u64) as usize;
                let sample_count = frame_count * 2;
                let meter_bus_samples = MIXER_TRACK_METER_COUNT * sample_count;
                let active_bus_samples = meter_bus_samples * mixer_bus_planes;
                let block = &mut output[..sample_count];
                block.fill(0.0);
                let mut meter_buses = if meters.is_some() || mixer_effect_buses_required {
                    let buses = &mut meter_buses[..active_bus_samples];
                    buses.fill(0.0);
                    Some(buses)
                } else {
                    None
                };
                if let Some(post_effect_buses) = post_effect_buses.as_deref_mut() {
                    post_effect_buses[..meter_bus_samples].fill(0.0);
                }
                mix_prepared_audio_clip_block(
                    &audio_mix_context,
                    block_start,
                    block,
                    meter_buses.as_deref_mut(),
                    parallel_mixer.as_ref(),
                )?;
                if let Some(performance) = performance.as_mut() {
                    performance.update(
                        block_start..block_start.saturating_add(frame_count as u64),
                        &mut sampler_engine,
                        &performance_audio_mix_context,
                        block,
                        meter_buses.as_deref_mut(),
                        render.vst3_processor.as_deref_mut(),
                    )?;
                    let delay_frames = render.vst3_processor.as_deref().map_or(
                        0,
                        Vst3PlaylistStreamProcessor::pdc_direct_source_delay_frames,
                    );
                    update_live_source_pdc_delays(
                        delay_frames,
                        &mut direct_source_delay,
                        &mut mixer_bus_delays,
                        mixer_bus_delay_count,
                    )?;
                }
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
                mix_playlist_clap_audio_layers(
                    block,
                    block_start,
                    frame_count,
                    &options.clap_audio_layers,
                    &clap_mixer_routes,
                    meter_buses.as_deref_mut(),
                )?;
                direct_source_delay.process_interleaved(block)?;
                if let Some(meter_buses) = meter_buses.as_deref_mut() {
                    for (bus_index, delay) in mixer_bus_delays.iter_mut().enumerate() {
                        let bus_start = bus_index * sample_count;
                        delay.process_interleaved(
                            &mut meter_buses[bus_start..bus_start + sample_count],
                        )?;
                    }
                }
                if let Some(processor) = render.vst3_processor.as_deref_mut() {
                    if let (Some(meter_buses), Some(post_effect_buses)) =
                        (meter_buses.as_deref_mut(), post_effect_buses.as_deref_mut())
                    {
                        processor.mix_next_block_with_post_effect_buses(
                            block,
                            meter_buses,
                            post_effect_buses,
                        )?;
                    } else if let Some(meter_buses) = meter_buses.as_deref_mut() {
                        processor.mix_next_block_with_meter_buses(block, meter_buses)?;
                    } else {
                        processor.mix_next_block(block)?;
                    }
                }
                if let (Some(meters), Some(meter_buses)) = (meters, meter_buses.as_deref()) {
                    let meter_buses = &meter_buses[..meter_bus_samples];
                    for (track, track_samples) in meter_buses.chunks_exact(sample_count).enumerate()
                    {
                        if track == 0 {
                            continue;
                        }
                        let (frames, _) = track_samples.as_chunks::<2>();
                        let (left_peak, right_peak) = frames.iter().fold(
                            (0.0_f32, 0.0_f32),
                            |(left_peak, right_peak), frame| {
                                (
                                    left_peak.max(frame[0].abs()),
                                    right_peak.max(frame[1].abs()),
                                )
                            },
                        );
                        meters.publish_stereo_peak(track, left_peak, right_peak);
                        let post_effect_track_samples = post_effect_buses.as_deref().map_or(
                            track_samples,
                            |post_effect_buses| {
                                let start = track * sample_count;
                                &post_effect_buses[start..start + sample_count]
                            },
                        );
                        let (post_effect_frames, _) = post_effect_track_samples.as_chunks::<2>();
                        let (post_effect_left_peak, post_effect_right_peak) = post_effect_frames
                            .iter()
                            .fold((0.0_f32, 0.0_f32), |(left_peak, right_peak), frame| {
                                (
                                    left_peak.max(frame[0].abs()),
                                    right_peak.max(frame[1].abs()),
                                )
                            });
                        meters.publish_post_effect_stereo_peak(
                            track,
                            post_effect_left_peak,
                            post_effect_right_peak,
                        );
                    }
                }
                render.master_output.apply(block);
                let (frames_to_write, stop_for_silence) = remainder_detector
                    .as_mut()
                    .map_or((frame_count, false), |detector| {
                        detector.frames_to_keep(block_start, block)
                    });
                let samples_to_write = frames_to_write * 2;
                write_block(
                    &block[..samples_to_write],
                    PlaylistRenderMixerBuses {
                        pre_effect: meter_buses
                            .as_deref()
                            .map(|buses| &buses[..MIXER_TRACK_METER_COUNT * samples_to_write]),
                        post_effect: if mixer_post_effect_stems_required {
                            post_effect_buses
                                .as_deref()
                                .map(|buses| &buses[..MIXER_TRACK_METER_COUNT * samples_to_write])
                        } else {
                            None
                        },
                    },
                )?;
                frames_rendered = block_start.saturating_add(frames_to_write as u64);
                block_start = block_start.saturating_add(frame_count as u64);
                if stop_for_silence {
                    break;
                }
            }
            Ok::<_, String>(frames_rendered)
        })();
        let worker_result = if let Some(parallel_mixer) = parallel_mixer.as_mut() {
            parallel_mixer.stop_and_join()
        } else {
            Ok(())
        };
        match (mix_result, worker_result) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(frames_rendered), Ok(())) => Ok(frames_rendered),
        }
    });
    let stop_result = if let Some(processor) = render.vst3_processor.as_deref_mut() {
        processor.stop_processing()
    } else {
        Ok(())
    };
    let frames_rendered = match (mix_result, stop_result) {
        (Err(error), _) => return Err(error),
        (Ok(_), Err(error)) => return Err(error),
        (Ok(frames_rendered), Ok(())) => frames_rendered,
    };
    Ok(PlaylistBlockRenderSummary {
        voices_stolen: sampler_engine.voices_stolen,
        frames_rendered,
    })
}

fn skip_render_block_prefix<'a>(block: &'a [f32], frames_to_skip: &mut u64) -> &'a [f32] {
    let frames = (block.len() / 2) as u64;
    let skipped = (*frames_to_skip).min(frames);
    *frames_to_skip -= skipped;
    &block[skipped as usize * 2..]
}

fn playlist_audio_render_output(output: PlaylistRenderOutput) -> AudioRenderOutput {
    match output {
        PlaylistRenderOutput::Wav => AudioRenderOutput::Wav,
        PlaylistRenderOutput::Flac { bits_per_sample } => {
            AudioRenderOutput::Flac { bits_per_sample }
        }
        PlaylistRenderOutput::Ogg { bitrate_kbps } => AudioRenderOutput::Ogg { bitrate_kbps },
        PlaylistRenderOutput::Mp3 { bitrate_kbps } => AudioRenderOutput::Mp3 { bitrate_kbps },
    }
}

fn write_float_spool_block(file: &mut File, block: &[f32]) -> Result<(), String> {
    if !block.len().is_multiple_of(2) {
        return Err("render block must contain interleaved stereo frames".to_owned());
    }
    write_float_spool_samples(file, block)
}

fn write_float_spool_samples(file: &mut File, samples: &[f32]) -> Result<(), String> {
    let samples_per_chunk = STREAM_BLOCK_FRAMES * 2;
    for sample_chunk in samples.chunks(samples_per_chunk) {
        let byte_capacity = sample_chunk
            .len()
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| "render spool block size overflow".to_owned())?;
        let mut bytes = Vec::with_capacity(byte_capacity);
        for sample in sample_chunk {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        file.write_all(&bytes)
            .map_err(|error| format!("could not write temporary render spool: {error}"))?;
    }
    Ok(())
}

/// Adds post-content render frames onto the start of a float PCM spool and keeps its loop length.
fn wrap_float_spool_remainder(
    file: &mut File,
    content_frames: u64,
    rendered_frames: u64,
) -> Result<(), String> {
    if content_frames == 0 || rendered_frames < content_frames {
        return Err("wrapped render must include its full content range".to_owned());
    }
    let tail_frames = rendered_frames - content_frames;
    if tail_frames == 0 {
        return Ok(());
    }
    let head_frames = content_frames.min(tail_frames);
    let head_samples = usize::try_from(head_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(2))
        .ok_or_else(|| "wrapped render head is too large for this platform".to_owned())?;
    let head_bytes = head_samples
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| "wrapped render head size overflow".to_owned())?;
    if head_bytes > MAX_MIX_BYTES {
        return Err(format!(
            "wrapped render remainder exceeds the {} MiB working limit",
            MAX_MIX_BYTES / (1024 * 1024)
        ));
    }

    let mut head = Vec::new();
    head.try_reserve_exact(head_samples)
        .map_err(|error| format!("could not allocate wrapped render head: {error}"))?;
    head.resize(head_samples, 0.0_f32);
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not seek to wrapped render start: {error}"))?;
    read_float_spool_samples(file, head_samples, &mut head)?;

    let tail_offset_bytes = content_frames
        .checked_mul(2 * std::mem::size_of::<f32>() as u64)
        .ok_or_else(|| "wrapped render tail offset overflow".to_owned())?;
    file.seek(SeekFrom::Start(tail_offset_bytes))
        .map_err(|error| format!("could not seek to wrapped render tail: {error}"))?;
    let mut remaining_frames = tail_frames;
    let mut tail_bytes = vec![0_u8; STREAM_BLOCK_FRAMES * 2 * std::mem::size_of::<f32>()];
    let mut tail_samples = Vec::with_capacity(tail_bytes.len() / 4);
    let mut rendered_tail_frames = 0_u64;
    while remaining_frames > 0 {
        let frames = remaining_frames.min(STREAM_BLOCK_FRAMES as u64) as usize;
        let sample_count = frames * 2;
        let byte_count = sample_count * std::mem::size_of::<f32>();
        file.read_exact(&mut tail_bytes[..byte_count])
            .map_err(|error| format!("could not read wrapped render tail: {error}"))?;
        tail_samples.clear();
        tail_samples.extend(
            tail_bytes[..byte_count]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes)),
        );
        for (frame_offset, frame) in tail_samples
            .as_slice()
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
        {
            let wrapped_frame = (rendered_tail_frames + frame_offset as u64) % content_frames;
            let sample_offset = usize::try_from(wrapped_frame)
                .ok()
                .and_then(|frame| frame.checked_mul(2))
                .ok_or_else(|| "wrapped render destination offset overflow".to_owned())?;
            head[sample_offset] += frame[0];
            head[sample_offset + 1] += frame[1];
        }
        rendered_tail_frames = rendered_tail_frames.saturating_add(frames as u64);
        remaining_frames -= frames as u64;
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not seek to wrapped render start: {error}"))?;
    write_float_spool_samples(file, &head)
}

fn read_float_spool_samples(
    file: &mut File,
    sample_count: usize,
    samples: &mut Vec<f32>,
) -> Result<(), String> {
    samples.clear();
    let mut remaining_samples = sample_count;
    let mut bytes = vec![0_u8; STREAM_BLOCK_FRAMES * 2 * std::mem::size_of::<f32>()];
    while remaining_samples > 0 {
        let chunk_samples = remaining_samples.min(bytes.len() / 4);
        let byte_count = chunk_samples
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| "render spool read size overflow".to_owned())?;
        file.read_exact(&mut bytes[..byte_count])
            .map_err(|error| format!("could not read temporary render spool: {error}"))?;
        samples.extend(
            bytes[..byte_count]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes)),
        );
        remaining_samples -= chunk_samples;
    }
    Ok(())
}

fn encode_float_spool(
    output_file: &mut File,
    spool: &mut File,
    frames: u64,
    sample_rate: u32,
    options: AudioRenderEncodingOptions,
    output_format: AudioRenderOutput,
) -> Result<(), String> {
    let mut encoder =
        AudioFileStreamEncoder::new(output_file, sample_rate, frames, options, output_format)?;
    spool
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not seek to temporary render spool: {error}"))?;
    let mut remaining_frames = frames;
    let mut bytes = vec![0_u8; STREAM_BLOCK_FRAMES * 2 * std::mem::size_of::<f32>()];
    let mut block = Vec::with_capacity(bytes.len() / 4);
    while remaining_frames > 0 {
        let block_frames = remaining_frames.min(STREAM_BLOCK_FRAMES as u64) as usize;
        let sample_count = block_frames * 2;
        let byte_count = sample_count * std::mem::size_of::<f32>();
        spool
            .read_exact(&mut bytes[..byte_count])
            .map_err(|error| format!("could not read temporary render spool: {error}"))?;
        block.clear();
        block.extend(
            bytes[..byte_count]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|sample| f32::from_le_bytes(*sample)),
        );
        encoder.write_stereo_block(&block, options.wav_channel_mode)?;
        remaining_frames -= block_frames as u64;
    }
    encoder.finalize(frames)
}

/// Render the sample voices in one FLP pattern to a WAV using the selected bit depth and channel mode.
///
/// This Sampler path schedules kind-0 channel notes on the project timeline, reads the channel's
/// `0xC4` sample path and supported sample metadata, applies note velocity and channel routing, and
/// caps simultaneous voices. Channel-native Sampler filters remain unsupported; the effect-aware
/// file renderer can apply loaded VST3 Mixer insert and Master effects.
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

/// Render one Sampler Pattern through prepared loaded Mixer effects to an audio file.
pub fn render_sampler_pattern_to_file_with_mixer_effects(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    output_path: impl AsRef<Path>,
    output_format: AudioRenderOutput,
    mut processor: Vst3PlaylistStreamProcessor,
) -> Result<SamplerPatternRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path_with_extension(project_path, output_path, output_format.extension())?;
    if processor.summary().sample_rate != options.sample_rate {
        return Err("Sampler Pattern and Mixer effect sample rates do not match".to_owned());
    }

    let render = prepare_sampler_pattern(document, project_path, options, false, None)?;
    let content_frames = render.summary.frames.max(processor.content_frames());
    processor.extend_to_output_frames(content_frames)?;
    let processor_summary = processor.summary();
    let trim_frames = processor
        .pdc_render_start_trim_frames()
        .min(processor_summary.frames);

    processor.start_processing()?;
    let render_result = render_stereo_audio_to_file(
        output_path,
        processor_summary.frames.saturating_sub(trim_frames),
        processor_summary.sample_rate,
        AudioRenderEncodingOptions {
            wav_sample_format: options.wav_sample_format,
            wav_dither_mode: options.wav_dither_mode,
            wav_channel_mode: options.wav_channel_mode,
        },
        output_format,
        |write_block| {
            stream_prepared_sampler_pattern_with_mixer_effects(
                &render,
                Some(&mut processor),
                processor_summary.frames,
                trim_frames,
                None,
                || false,
                write_block,
            )
        },
    );
    let stop_result = processor.stop_processing();
    let summary = render_result?;
    stop_result?;
    Ok(summary)
}

/// Render all enabled kind-0 Sampler notes in one pattern into an interleaved stereo buffer.
pub fn render_sampler_pattern_to_stereo_buffer(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
) -> Result<(Vec<f32>, SamplerPatternRenderSummary), String> {
    let render = prepare_sampler_pattern(document, project_path.as_ref(), options, false, None)?;
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
    let render = prepare_sampler_pattern(
        document,
        project_path.as_ref(),
        options,
        false,
        Some(cancelled),
    )?;
    stream_prepared_sampler_pattern(
        &render,
        Some(cancelled),
        || writer.is_cancelled(),
        |block| writer.write_stereo_samples(block),
    )
}

/// Stream one Sampler Pattern with live Mixer updates at audio-block boundaries.
pub fn stream_sampler_pattern_to_device_with_live_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    controls: SamplerPatternStreamControls<'_>,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks: 1,
            start_tick: 0,
            vst3_processor: None,
            allow_empty_sampler: false,
            repeat_pattern: false,
            extend_preview_tail: false,
            pattern_audio_layers: Vec::new(),
        },
        PatternLiveStreamControls {
            meters: Some(controls.meters),
            latency_updates: controls.latency_updates,
        },
        writer,
        cancelled,
    )
}

/// Stream one Sampler Pattern repeatedly until the worker or audio writer is cancelled.
pub fn stream_sampler_pattern_loop_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    loop_length_ticks: u64,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_sampler_pattern_loop_from_tick_to_device(
        document,
        project_path,
        options,
        loop_length_ticks,
        0,
        writer,
        cancelled,
    )
}

/// Stream a Sampler Pattern repeatedly, starting at a local tick within its loop.
///
/// The voice engine renders and discards the portion before `start_tick`, so notes that are
/// already sounding at the seek point continue from their correct sample position.
pub fn stream_sampler_pattern_loop_from_tick_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    loop_length_ticks: u64,
    start_tick: u64,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks,
            start_tick,
            vst3_processor: None,
            allow_empty_sampler: false,
            repeat_pattern: true,
            extend_preview_tail: false,
            pattern_audio_layers: Vec::new(),
        },
        PatternLiveStreamControls {
            meters: None,
            latency_updates: None,
        },
        writer,
        cancelled,
    )
}

/// Stream a looping Sampler Pattern with live Master Mixer control smoothing.
pub fn stream_sampler_pattern_loop_from_tick_to_device_with_live_master_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    playback: SamplerPatternLoopPlayback,
    meters: &crate::audio::MixerTrackPeakMeters,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_sampler_pattern_loop_from_tick_to_device_with_live_controls(
        document,
        project_path,
        options,
        SamplerPatternLoopStreamControls {
            playback,
            meters,
            latency_updates: None,
            pattern_audio_layers: Vec::new(),
        },
        writer,
        cancelled,
    )
}

/// Stream a Sampler Pattern loop with live Mixer routing and latency updates at block boundaries.
pub fn stream_sampler_pattern_loop_from_tick_to_device_with_live_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    controls: SamplerPatternLoopStreamControls<'_>,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    let allow_empty_sampler = !controls.pattern_audio_layers.is_empty();
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks: controls.playback.loop_length_ticks,
            start_tick: controls.playback.start_tick,
            vst3_processor: None,
            allow_empty_sampler,
            repeat_pattern: true,
            extend_preview_tail: false,
            pattern_audio_layers: controls.pattern_audio_layers,
        },
        PatternLiveStreamControls {
            meters: Some(controls.meters),
            latency_updates: controls.latency_updates,
        },
        writer,
        cancelled,
    )
}

/// Stream a Pattern loop with enabled Sampler channels and loaded VST3 instruments in one mix.
pub fn stream_pattern_loop_with_vst3_from_tick_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    playback: Vst3PatternLoopPlayback,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks: playback.loop_length_ticks,
            start_tick: playback.start_tick,
            vst3_processor: Some(playback.processor),
            allow_empty_sampler: true,
            repeat_pattern: true,
            extend_preview_tail: false,
            pattern_audio_layers: Vec::new(),
        },
        PatternLiveStreamControls {
            meters: None,
            latency_updates: None,
        },
        writer,
        cancelled,
    )
}

/// Stream a Sampler and VST3 Pattern loop with live Master Mixer control smoothing.
pub fn stream_pattern_loop_with_vst3_from_tick_to_device_with_live_master_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    playback: Vst3PatternLoopPlayback,
    meters: &crate::audio::MixerTrackPeakMeters,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks: playback.loop_length_ticks,
            start_tick: playback.start_tick,
            vst3_processor: Some(playback.processor),
            allow_empty_sampler: true,
            repeat_pattern: true,
            extend_preview_tail: false,
            pattern_audio_layers: Vec::new(),
        },
        PatternLiveStreamControls {
            meters: Some(meters),
            latency_updates: None,
        },
        writer,
        cancelled,
    )
}

/// Stream a live Pattern loop with block-boundary VST3 latency refreshes.
pub fn stream_pattern_loop_with_vst3_from_tick_to_device_with_live_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    controls: Vst3PatternLoopStreamControls<'_>,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks: controls.playback.loop_length_ticks,
            start_tick: controls.playback.start_tick,
            vst3_processor: Some(controls.playback.processor),
            allow_empty_sampler: true,
            repeat_pattern: true,
            extend_preview_tail: false,
            pattern_audio_layers: controls.pattern_audio_layers,
        },
        PatternLiveStreamControls {
            meters: Some(controls.meters),
            latency_updates: controls.latency_updates,
        },
        writer,
        cancelled,
    )
}

/// Stream one complete Pattern with Sampler and loaded VST3 channels in one mix.
pub fn stream_pattern_with_vst3_to_device_with_live_master_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    playback: Vst3PatternPlayback,
    meters: &crate::audio::MixerTrackPeakMeters,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_with_vst3_to_device_with_live_controls(
        document,
        project_path,
        options,
        Vst3PatternStreamControls {
            playback,
            meters,
            latency_updates: None,
        },
        writer,
        cancelled,
    )
}

/// Stream one Pattern with live VST3 latency and Mixer updates at block boundaries.
pub fn stream_pattern_with_vst3_to_device_with_live_controls(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    controls: Vst3PatternStreamControls<'_>,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_pattern_from_tick_with_vst3_to_device(
        document,
        project_path,
        options,
        PatternStreamConfig {
            loop_length_ticks: controls.playback.duration_ticks,
            start_tick: 0,
            vst3_processor: Some(controls.playback.processor),
            allow_empty_sampler: true,
            repeat_pattern: false,
            extend_preview_tail: true,
            pattern_audio_layers: Vec::new(),
        },
        PatternLiveStreamControls {
            meters: Some(controls.meters),
            latency_updates: controls.latency_updates,
        },
        writer,
        cancelled,
    )
}

struct PatternStreamConfig {
    loop_length_ticks: u64,
    start_tick: u64,
    vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    allow_empty_sampler: bool,
    repeat_pattern: bool,
    extend_preview_tail: bool,
    pattern_audio_layers: Vec<PatternAudioLayer>,
}

fn mix_pattern_audio_layers(
    output: &mut [f32],
    frame_count: usize,
    block_start: u64,
    loop_frames: u64,
    layers: &[PatternAudioLayer],
    routes_by_channel: &HashMap<u16, PreparedClapMixerRoute>,
    mixer_buses: Option<&mut [f32]>,
) -> Result<(), String> {
    if layers.is_empty() || loop_frames == 0 {
        return Ok(());
    }
    debug_assert_eq!(output.len(), frame_count * 2);
    let sample_count = frame_count * 2;
    let meter_bus_samples = MIXER_TRACK_METER_COUNT * sample_count;
    let (mut meter_buses, mut effect_input_buses) = match mixer_buses {
        Some(buses) if buses.len() >= meter_bus_samples * 2 => {
            let (meter_buses, effect_input_buses) = buses.split_at_mut(meter_bus_samples);
            (Some(meter_buses), Some(effect_input_buses))
        }
        Some(buses) => (Some(buses), None),
        None => (None, None),
    };
    for layer in layers {
        let route = routes_by_channel.get(&layer.channel_id);
        let live_source_control = route.and_then(|route| route.live_source_control.as_ref());
        for frame_index in 0..frame_count {
            let source_frame_index =
                (block_start.saturating_add(frame_index as u64) % loop_frames) as usize;
            let source_sample = source_frame_index * 2;
            let target_sample = frame_index * 2;
            let source_frame = [
                layer.samples[source_sample] * layer.left_gain,
                layer.samples[source_sample + 1] * layer.right_gain,
            ];
            let mut routed_frame = source_frame;
            let live_controls = live_source_control.map(|(control, targets)| {
                let controls = control.next_frame_controls(targets);
                control.apply_output_frame(controls, &mut routed_frame);
                controls
            });
            if live_controls.is_none() {
                route.map_or_else(
                    || (),
                    |route| route.mixer_transform.apply_frame(&mut routed_frame),
                );
            }
            output[target_sample] += routed_frame[0];
            output[target_sample + 1] += routed_frame[1];

            if let Some(meter_buses) = meter_buses.as_deref_mut() {
                let meter_transforms = live_source_control.map_or_else(
                    || route.map_or(&[][..], |route| route.mixer_meter_transforms.as_slice()),
                    |(control, _)| control.insert_meter_transforms(),
                );
                for (track, transform) in meter_transforms {
                    if *track >= MIXER_TRACK_METER_COUNT {
                        return Err(
                            "CLAP Pattern Mixer meter route exceeds the supported track range"
                                .to_owned(),
                        );
                    }
                    let mut meter_frame = source_frame;
                    if let (Some((control, _)), Some(controls)) =
                        (live_source_control, live_controls)
                    {
                        control.apply_meter_frame(controls, *track, *transform, &mut meter_frame);
                    } else {
                        transform.apply_frame(&mut meter_frame);
                    }
                    let meter_offset = *track * sample_count + target_sample;
                    meter_buses[meter_offset] += meter_frame[0];
                    meter_buses[meter_offset + 1] += meter_frame[1];
                }
            }

            if let Some(effect_input_buses) = effect_input_buses.as_deref_mut() {
                let effect_input_transforms = live_source_control.map_or_else(
                    || {
                        route.map_or(&[][..], |route| {
                            route.mixer_effect_input_transforms.as_slice()
                        })
                    },
                    |(control, _)| control.insert_effect_input_transforms(),
                );
                for (track, transform) in effect_input_transforms {
                    if *track >= MIXER_TRACK_METER_COUNT {
                        return Err(
                            "CLAP Pattern Mixer effect route exceeds the supported track range"
                                .to_owned(),
                        );
                    }
                    let mut effect_input_frame = source_frame;
                    if let (Some((control, _)), Some(controls)) =
                        (live_source_control, live_controls)
                    {
                        control.apply_effect_input_frame(
                            controls,
                            *track,
                            *transform,
                            &mut effect_input_frame,
                        );
                    } else {
                        transform.apply_frame(&mut effect_input_frame);
                    }
                    let effect_input_offset = *track * sample_count + target_sample;
                    effect_input_buses[effect_input_offset] += effect_input_frame[0];
                    effect_input_buses[effect_input_offset + 1] += effect_input_frame[1];
                }
            }
        }
    }
    Ok(())
}

fn playlist_clap_audio_frames(layers: &[PlaylistAudioLayer]) -> u64 {
    layers
        .iter()
        .map(|layer| (layer.samples.len() / 2) as u64)
        .max()
        .unwrap_or(0)
}

struct PreparedClapMixerRoute {
    mixer_transform: MixerSourceRouteTransform,
    live_source_control: Option<(LiveMixerSourceControl, LiveMixerSourceTargets)>,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_effect_input_transforms: Vec<(usize, MixerSourceRouteTransform)>,
}

fn prepare_pattern_clap_mixer_routes(
    layers: &[PatternAudioLayer],
    document: &FlpDocument,
    meters: Option<&MixerTrackPeakMeters>,
    sample_rate: u32,
) -> HashMap<u16, PreparedClapMixerRoute> {
    let routing = MixerRouteAudibility::from_document(document);
    let channels_by_id = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    layers
        .iter()
        .map(|layer| {
            let mixer_track = channels_by_id
                .get(&layer.channel_id)
                .and_then(|channel| channel.mixer_track());
            let statically_audible = routing.allows_channel(mixer_track);
            let route_plan = routing.source_route_plan_for_channel(mixer_track);
            let live_source_control = meters.and_then(|meters| {
                routing
                    .allows_live_source_channel(mixer_track)
                    .then(|| routing.live_source_route_plan_for_channel(mixer_track))
                    .flatten()
                    .and_then(|route_plan| {
                        let control = LiveMixerSourceControl::new(route_plan, sample_rate);
                        control.targets(meters).map(|targets| (control, targets))
                    })
            });
            (
                layer.channel_id,
                PreparedClapMixerRoute {
                    mixer_transform: if statically_audible {
                        route_plan.output_transform
                    } else {
                        MixerSourceRouteTransform::zero()
                    },
                    live_source_control,
                    mixer_meter_transforms: if statically_audible {
                        route_plan.insert_meter_transforms
                    } else {
                        Vec::new()
                    },
                    mixer_effect_input_transforms: if statically_audible {
                        route_plan.insert_effect_input_transforms
                    } else {
                        Vec::new()
                    },
                },
            )
        })
        .collect()
}

fn prepare_playlist_clap_mixer_routes(
    layers: &[PlaylistAudioLayer],
    sampler: &PreparedSamplerArrangement,
    meters: Option<&MixerTrackPeakMeters>,
    sample_rate: u32,
) -> HashMap<u16, PreparedClapMixerRoute> {
    layers
        .iter()
        .map(|layer| {
            let mixer_transform = sampler
                .mixer_transforms_by_channel
                .get(&layer.channel_id)
                .copied()
                .unwrap_or_default();
            let live_source_control = sampler
                .live_mixer_source_routes_by_channel
                .get(&layer.channel_id)
                .cloned()
                .and_then(|route_plan| {
                    let control = LiveMixerSourceControl::new(route_plan, sample_rate);
                    meters
                        .and_then(|meters| control.targets(meters))
                        .map(|targets| (control, targets))
                });
            let mixer_meter_transforms = sampler
                .mixer_meter_transforms_by_channel
                .get(&layer.channel_id)
                .cloned()
                .unwrap_or_default();
            let mixer_effect_input_transforms = sampler
                .mixer_effect_input_transforms_by_channel
                .get(&layer.channel_id)
                .cloned()
                .unwrap_or_default();
            (
                layer.channel_id,
                PreparedClapMixerRoute {
                    mixer_transform,
                    live_source_control,
                    mixer_meter_transforms,
                    mixer_effect_input_transforms,
                },
            )
        })
        .collect()
}

fn mix_playlist_clap_audio_layers(
    output: &mut [f32],
    block_start: u64,
    frame_count: usize,
    layers: &[PlaylistAudioLayer],
    routes_by_channel: &HashMap<u16, PreparedClapMixerRoute>,
    mixer_buses: Option<&mut [f32]>,
) -> Result<(), String> {
    if layers.is_empty() {
        return Ok(());
    }
    debug_assert_eq!(output.len(), frame_count * 2);
    let sample_count = frame_count * 2;
    let meter_bus_samples = MIXER_TRACK_METER_COUNT * sample_count;
    let (mut meter_buses, mut effect_input_buses) = match mixer_buses {
        Some(buses) if buses.len() >= meter_bus_samples * 2 => {
            let (meter_buses, effect_input_buses) = buses.split_at_mut(meter_bus_samples);
            (Some(meter_buses), Some(effect_input_buses))
        }
        Some(buses) => (Some(buses), None),
        None => (None, None),
    };
    for layer in layers {
        let layer_frames = (layer.samples.len() / 2) as u64;
        let route = routes_by_channel.get(&layer.channel_id);
        let live_source_control = route.and_then(|route| route.live_source_control.as_ref());
        for frame_index in 0..frame_count {
            let source_frame_index = block_start.saturating_add(frame_index as u64);
            if source_frame_index >= layer_frames {
                continue;
            }
            let source_sample = (source_frame_index * 2) as usize;
            let target_sample = frame_index * 2;
            let source_frame = [
                layer.samples[source_sample] * layer.left_gain,
                layer.samples[source_sample + 1] * layer.right_gain,
            ];
            let mut routed_frame = source_frame;
            let live_controls = live_source_control.map(|(control, targets)| {
                let controls = control.next_frame_controls(targets);
                control.apply_output_frame(controls, &mut routed_frame);
                controls
            });
            if live_controls.is_none() {
                route.map_or_else(
                    || (),
                    |route| route.mixer_transform.apply_frame(&mut routed_frame),
                );
            }
            output[target_sample] += routed_frame[0];
            output[target_sample + 1] += routed_frame[1];

            if let Some(meter_buses) = meter_buses.as_deref_mut() {
                let meter_transforms = live_source_control.map_or_else(
                    || route.map_or(&[][..], |route| route.mixer_meter_transforms.as_slice()),
                    |(control, _)| control.insert_meter_transforms(),
                );
                for (track, transform) in meter_transforms {
                    if *track >= MIXER_TRACK_METER_COUNT {
                        return Err(
                            "CLAP Playlist Mixer meter route exceeds the supported track range"
                                .to_owned(),
                        );
                    }
                    let mut meter_frame = source_frame;
                    if let (Some((control, _)), Some(controls)) =
                        (live_source_control, live_controls)
                    {
                        control.apply_meter_frame(controls, *track, *transform, &mut meter_frame);
                    } else {
                        transform.apply_frame(&mut meter_frame);
                    }
                    let meter_offset = *track * sample_count + target_sample;
                    meter_buses[meter_offset] += meter_frame[0];
                    meter_buses[meter_offset + 1] += meter_frame[1];
                }
            }

            if let Some(effect_input_buses) = effect_input_buses.as_deref_mut() {
                let effect_input_transforms = live_source_control.map_or_else(
                    || {
                        route.map_or(&[][..], |route| {
                            route.mixer_effect_input_transforms.as_slice()
                        })
                    },
                    |(control, _)| control.insert_effect_input_transforms(),
                );
                for (track, transform) in effect_input_transforms {
                    if *track >= MIXER_TRACK_METER_COUNT {
                        return Err(
                            "CLAP Playlist Mixer effect route exceeds the supported track range"
                                .to_owned(),
                        );
                    }
                    let mut effect_input_frame = source_frame;
                    if let (Some((control, _)), Some(controls)) =
                        (live_source_control, live_controls)
                    {
                        control.apply_effect_input_frame(
                            controls,
                            *track,
                            *transform,
                            &mut effect_input_frame,
                        );
                    } else {
                        transform.apply_frame(&mut effect_input_frame);
                    }
                    let effect_input_offset = *track * sample_count + target_sample;
                    effect_input_buses[effect_input_offset] += effect_input_frame[0];
                    effect_input_buses[effect_input_offset + 1] += effect_input_frame[1];
                }
            }
        }
    }
    Ok(())
}

struct PatternLiveStreamControls<'a> {
    meters: Option<&'a crate::audio::MixerTrackPeakMeters>,
    latency_updates: Option<Vst3LatencyUpdateControls>,
}

struct SamplerPatternChannelOptions<'a> {
    channel_filter: Option<&'a BTreeSet<u16>>,
    allow_empty: bool,
    include_muted_mixer_sources: bool,
    vst3_volume_expression_channels: Option<&'a BTreeSet<u16>>,
    clap_audio_channels: Option<&'a BTreeSet<u16>>,
}

fn stream_pattern_from_tick_with_vst3_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    mut playback: PatternStreamConfig,
    controls: PatternLiveStreamControls<'_>,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<SamplerPatternRenderSummary, String> {
    let PatternLiveStreamControls {
        meters: live_master_meters,
        latency_updates,
    } = controls;
    let mut latency_updates = latency_updates.map(|controls| LiveLatencyUpdateState {
        document: document.clone(),
        pdc_updates: controls.pdc_updates,
        updates: controls.updates,
        reports: controls.reports,
    });
    let loop_length_ticks = playback.loop_length_ticks;
    if loop_length_ticks == 0 {
        return Err("pattern loop length must be greater than zero".to_owned());
    }
    let ppq = document.header().ppq();
    let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    let loop_frames = ticks_to_frames(loop_length_ticks, ppq, tempo_bpm, options.sample_rate)?;
    if loop_frames == 0 {
        return Err("pattern loop length rounds to zero audio frames".to_owned());
    }
    if !playback.repeat_pattern && !playback.pattern_audio_layers.is_empty() {
        return Err("pre-rendered Pattern audio layers require looping playback".to_owned());
    }
    let loop_sample_count = loop_frames
        .checked_mul(2)
        .and_then(|samples| usize::try_from(samples).ok())
        .ok_or_else(|| "Pattern audio loop size overflow".to_owned())?;
    for layer in &playback.pattern_audio_layers {
        if layer.samples.len() != loop_sample_count {
            return Err(format!(
                "CLAP audio layer for channel {} has {} samples; expected {loop_sample_count} for this Pattern loop",
                layer.channel_id,
                layer.samples.len()
            ));
        }
    }
    let start_tick = playback.start_tick % loop_length_ticks;
    let start_frame = ticks_to_frames(start_tick, ppq, tempo_bpm, options.sample_rate)?;
    if start_frame >= loop_frames {
        return Err("Pattern seek position rounds outside its audio loop".to_owned());
    }
    let vst3_volume_expression_channels = playback
        .vst3_processor
        .as_ref()
        .map(|processor| processor.volume_expression_channel_ids().clone())
        .unwrap_or_default();
    let clap_pattern_channels = playback
        .pattern_audio_layers
        .iter()
        .map(|layer| layer.channel_id)
        .collect::<BTreeSet<_>>();
    let render = if !clap_pattern_channels.is_empty() {
        let included_channels = document
            .channels()
            .iter()
            .map(|channel| channel.id())
            .filter(|channel_id| !clap_pattern_channels.contains(channel_id))
            .collect::<BTreeSet<_>>();
        prepare_sampler_pattern_for_channel_internal(
            document,
            project_path.as_ref(),
            options,
            Some(cancelled),
            SamplerPatternChannelOptions {
                channel_filter: Some(&included_channels),
                allow_empty: true,
                include_muted_mixer_sources: live_master_meters.is_some(),
                vst3_volume_expression_channels: playback
                    .vst3_processor
                    .as_ref()
                    .map(Vst3PlaylistStreamProcessor::volume_expression_channel_ids),
                clap_audio_channels: Some(&clap_pattern_channels),
            },
        )?
    } else if playback.allow_empty_sampler {
        prepare_sampler_pattern_allow_empty(
            document,
            project_path.as_ref(),
            options,
            live_master_meters.is_some(),
            &vst3_volume_expression_channels,
            Some(cancelled),
        )?
    } else {
        prepare_sampler_pattern(
            document,
            project_path.as_ref(),
            options,
            live_master_meters.is_some(),
            Some(cancelled),
        )?
    };
    let pattern_clap_mixer_routes = prepare_pattern_clap_mixer_routes(
        &playback.pattern_audio_layers,
        document,
        live_master_meters,
        options.sample_rate,
    );
    let mut master_output = render.master_output.clone();
    if let Some(meters) = live_master_meters {
        master_output = master_output.with_live_master_targets(meters, options.sample_rate);
    }
    if render.summary.notes_rendered == 0
        && playback
            .vst3_processor
            .as_ref()
            .is_none_or(|processor| processor.summary().plugin_channels_rendered == 0)
        && playback.pattern_audio_layers.is_empty()
    {
        return Err(format!(
            "Pattern {} has no supported Sampler notes or loaded VST3 instruments",
            options.pattern_id
        ));
    }
    let mut finite_output_frames = if playback.repeat_pattern {
        None
    } else {
        let base_frames = render.summary.frames.max(
            playback
                .vst3_processor
                .as_ref()
                .map_or(0, |processor| processor.summary().frames),
        );
        let tail_frames = if playback.extend_preview_tail {
            u64::from(options.sample_rate).saturating_mul(2)
        } else {
            0
        };
        let output_frames = base_frames
            .checked_add(tail_frames)
            .ok_or_else(|| "Pattern preview tail length overflow".to_owned())?;
        if let Some(processor) = playback.vst3_processor.as_mut() {
            processor.stop_pattern_repetition_after_first_pass();
            processor.extend_to_output_frames(output_frames)?;
        }
        Some(
            output_frames.max(
                playback
                    .vst3_processor
                    .as_ref()
                    .map_or(0, |processor| processor.summary().frames),
            ),
        )
    };
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
    engine.mixer_meter_transforms_by_channel = Some(&render.mixer_meter_transforms_by_channel);
    engine.mixer_effect_input_transforms_by_channel =
        Some(&render.mixer_effect_input_transforms_by_channel);
    if let Some(meters) = live_master_meters {
        engine.live_mixer_source_routes_by_channel =
            Some(&render.live_mixer_source_routes_by_channel);
        engine.live_mixer_source_targets = Some(meters);
    }
    if let Some(processor) = playback.vst3_processor.as_mut() {
        if let Some(meters) = live_master_meters {
            processor.set_live_mixer_source_targets(meters);
        }
        processor.start_processing()?;
    }
    let sampler_delay_frames = playback.vst3_processor.as_ref().map_or(
        0,
        Vst3PlaylistStreamProcessor::pdc_direct_source_delay_frames,
    );
    let mut sampler_delay = StereoFrameDelay::new(sampler_delay_frames)?;
    let mixer_effect_buses_required = playback
        .vst3_processor
        .as_ref()
        .is_some_and(Vst3PlaylistStreamProcessor::needs_mixer_insert_buses);
    let mixer_meter_buses_required = live_master_meters.is_some();
    let mixer_buses_required = mixer_meter_buses_required || mixer_effect_buses_required;
    let mixer_bus_planes = 1 + usize::from(mixer_effect_buses_required);
    let mixer_bus_delay_count = if mixer_buses_required {
        MIXER_TRACK_METER_COUNT * mixer_bus_planes
    } else {
        0
    };
    let mut mixer_buses = mixer_buses_required.then(|| {
        vec![0.0f32; MIXER_TRACK_METER_COUNT * SAMPLER_BLOCK_FRAMES * 2 * mixer_bus_planes]
    });
    let mut post_effect_buses = mixer_effect_buses_required
        .then(|| vec![0.0f32; MIXER_TRACK_METER_COUNT * SAMPLER_BLOCK_FRAMES * 2]);
    let mut mixer_bus_delays = if mixer_buses_required {
        (0..MIXER_TRACK_METER_COUNT * mixer_bus_planes)
            .map(|_| StereoFrameDelay::new(sampler_delay_frames))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let mut block = vec![0.0f32; SAMPLER_BLOCK_FRAMES * 2];
    let stream_result = (|| {
        let mut block_start = 0u64;
        while block_start < start_frame {
            check_cancelled(Some(cancelled))?;
            if writer.is_cancelled() {
                return Err("Sampler Pattern playback was stopped".to_owned());
            }
            if let Some(latency_updates) = latency_updates.as_mut() {
                apply_live_pdc_updates(
                    latency_updates,
                    playback.vst3_processor.as_mut(),
                    LiveMixerPdcUpdateContext {
                        meters: live_master_meters,
                        sampler_engine: Some(&mut engine),
                        audio_clips: None,
                        sample_rate: options.sample_rate,
                        direct_source_delay: &mut sampler_delay,
                        mixer_bus_delays: &mut mixer_bus_delays,
                        mixer_bus_delay_count,
                    },
                )?;
            }
            if let Some(latency_updates) = latency_updates.as_mut() {
                refresh_vst3_latency_updates(
                    latency_updates,
                    playback.vst3_processor.as_mut(),
                    live_master_meters,
                    &mut sampler_delay,
                    &mut mixer_bus_delays,
                    mixer_bus_delay_count,
                )?;
            }
            if let Some(output_frames) = finite_output_frames.as_mut()
                && let Some(processor) = playback.vst3_processor.as_ref()
            {
                *output_frames = (*output_frames).max(processor.summary().frames);
            }
            let frame_count = (start_frame - block_start).min(SAMPLER_BLOCK_FRAMES as u64) as usize;
            let sample_count = frame_count * 2;
            if let Some(mixer_buses) = mixer_buses.as_mut() {
                let active_buses =
                    &mut mixer_buses[..MIXER_TRACK_METER_COUNT * sample_count * mixer_bus_planes];
                active_buses.fill(0.0);
                engine.render_block_with_meters(
                    block_start,
                    frame_count,
                    &mut block[..sample_count],
                    Some(active_buses),
                    sample_count,
                );
                for (track, delay) in mixer_bus_delays.iter_mut().enumerate() {
                    let bus_start = track * sample_count;
                    delay.process_interleaved(
                        &mut active_buses[bus_start..bus_start + sample_count],
                    )?;
                }
            } else {
                engine.render_block(block_start, frame_count, &mut block[..sample_count]);
            }
            mix_pattern_audio_layers(
                &mut block[..sample_count],
                frame_count,
                block_start,
                loop_frames,
                &playback.pattern_audio_layers,
                &pattern_clap_mixer_routes,
                mixer_buses.as_deref_mut(),
            )?;
            sampler_delay.process_interleaved(&mut block[..sample_count])?;
            if let Some(processor) = playback.vst3_processor.as_mut() {
                if let Some(mixer_buses) = mixer_buses.as_mut() {
                    let active_buses = &mut mixer_buses
                        [..MIXER_TRACK_METER_COUNT * sample_count * mixer_bus_planes];
                    if let Some(post_effect_buses) = post_effect_buses.as_mut() {
                        let post_effect_buses =
                            &mut post_effect_buses[..MIXER_TRACK_METER_COUNT * sample_count];
                        post_effect_buses.fill(0.0);
                        processor.mix_next_block_with_post_effect_buses(
                            &mut block[..sample_count],
                            active_buses,
                            post_effect_buses,
                        )?;
                    } else {
                        processor.mix_next_block_with_meter_buses(
                            &mut block[..sample_count],
                            active_buses,
                        )?;
                    }
                } else {
                    processor.mix_next_block(&mut block[..sample_count])?;
                }
            }
            block_start += frame_count as u64;
        }
        let mut next_loop_start = loop_frames;
        loop {
            if finite_output_frames.is_some_and(|frames| block_start >= frames) {
                break Ok(());
            }
            check_cancelled(Some(cancelled))?;
            if writer.is_cancelled() {
                return Err("Sampler pattern playback was stopped".to_owned());
            }
            if let Some(latency_updates) = latency_updates.as_mut() {
                apply_live_pdc_updates(
                    latency_updates,
                    playback.vst3_processor.as_mut(),
                    LiveMixerPdcUpdateContext {
                        meters: live_master_meters,
                        sampler_engine: Some(&mut engine),
                        audio_clips: None,
                        sample_rate: options.sample_rate,
                        direct_source_delay: &mut sampler_delay,
                        mixer_bus_delays: &mut mixer_bus_delays,
                        mixer_bus_delay_count,
                    },
                )?;
            }
            if let Some(latency_updates) = latency_updates.as_mut() {
                refresh_vst3_latency_updates(
                    latency_updates,
                    playback.vst3_processor.as_mut(),
                    live_master_meters,
                    &mut sampler_delay,
                    &mut mixer_bus_delays,
                    mixer_bus_delay_count,
                )?;
            }
            if let Some(output_frames) = finite_output_frames.as_mut()
                && let Some(processor) = playback.vst3_processor.as_ref()
            {
                *output_frames = (*output_frames).max(processor.summary().frames);
            }
            let block_end = block_start.saturating_add(SAMPLER_BLOCK_FRAMES as u64);
            if playback.repeat_pattern {
                while next_loop_start <= block_end {
                    let repeated_notes =
                        render
                            .notes
                            .iter()
                            .copied()
                            .map(|mut note| {
                                note.start_frame =
                                    note.start_frame.checked_add(next_loop_start).ok_or_else(
                                        || "Sampler Pattern loop position overflow".to_owned(),
                                    )?;
                                note.stop_frame = note
                                    .stop_frame
                                    .map(|stop_frame| {
                                        stop_frame.checked_add(next_loop_start).ok_or_else(|| {
                                            "Sampler Pattern note end overflow".to_owned()
                                        })
                                    })
                                    .transpose()?;
                                Ok(note)
                            })
                            .collect::<Result<Vec<_>, String>>()?;
                    engine.queue_live_notes(repeated_notes);
                    next_loop_start = next_loop_start
                        .checked_add(loop_frames)
                        .ok_or_else(|| "Sampler Pattern loop position overflow".to_owned())?;
                }
            }

            let frame_count = finite_output_frames.map_or(SAMPLER_BLOCK_FRAMES, |frames| {
                frames
                    .saturating_sub(block_start)
                    .min(SAMPLER_BLOCK_FRAMES as u64) as usize
            });
            let sample_count = frame_count * 2;
            if let Some(mixer_buses) = mixer_buses.as_mut() {
                let active_buses =
                    &mut mixer_buses[..MIXER_TRACK_METER_COUNT * sample_count * mixer_bus_planes];
                active_buses.fill(0.0);
                engine.render_block_with_meters(
                    block_start,
                    frame_count,
                    &mut block[..sample_count],
                    Some(active_buses),
                    sample_count,
                );
                for (track, delay) in mixer_bus_delays.iter_mut().enumerate() {
                    let bus_start = track * sample_count;
                    delay.process_interleaved(
                        &mut active_buses[bus_start..bus_start + sample_count],
                    )?;
                }
            } else {
                engine.render_block(block_start, frame_count, &mut block[..sample_count]);
            }
            mix_pattern_audio_layers(
                &mut block[..sample_count],
                frame_count,
                block_start,
                loop_frames,
                &playback.pattern_audio_layers,
                &pattern_clap_mixer_routes,
                mixer_buses.as_deref_mut(),
            )?;
            sampler_delay.process_interleaved(&mut block[..sample_count])?;
            if let Some(processor) = playback.vst3_processor.as_mut() {
                if let Some(mixer_buses) = mixer_buses.as_mut() {
                    let active_buses = &mut mixer_buses
                        [..MIXER_TRACK_METER_COUNT * sample_count * mixer_bus_planes];
                    if let Some(post_effect_buses) = post_effect_buses.as_mut() {
                        let post_effect_buses =
                            &mut post_effect_buses[..MIXER_TRACK_METER_COUNT * sample_count];
                        post_effect_buses.fill(0.0);
                        processor.mix_next_block_with_post_effect_buses(
                            &mut block[..sample_count],
                            active_buses,
                            post_effect_buses,
                        )?;
                    } else {
                        processor.mix_next_block_with_meter_buses(
                            &mut block[..sample_count],
                            active_buses,
                        )?;
                    }
                } else {
                    processor.mix_next_block(&mut block[..sample_count])?;
                }
            }
            if let (Some(meters), Some(mixer_buses)) = (live_master_meters, mixer_buses.as_deref())
            {
                let pre_effect_buses = &mixer_buses[..MIXER_TRACK_METER_COUNT * sample_count];
                for (track, track_samples) in pre_effect_buses
                    .chunks_exact(sample_count)
                    .enumerate()
                    .skip(1)
                {
                    let (frames, _) = track_samples.as_chunks::<2>();
                    let (left_peak, right_peak) =
                        frames
                            .iter()
                            .fold((0.0_f32, 0.0_f32), |(left_peak, right_peak), frame| {
                                (
                                    left_peak.max(frame[0].abs()),
                                    right_peak.max(frame[1].abs()),
                                )
                            });
                    meters.publish_stereo_peak(track, left_peak, right_peak);
                    let post_effect_track_samples =
                        post_effect_buses
                            .as_deref()
                            .map_or(track_samples, |post_effect_buses| {
                                let start = track * sample_count;
                                &post_effect_buses[start..start + sample_count]
                            });
                    let (post_effect_frames, _) = post_effect_track_samples.as_chunks::<2>();
                    let (post_effect_left_peak, post_effect_right_peak) = post_effect_frames
                        .iter()
                        .fold((0.0_f32, 0.0_f32), |(left_peak, right_peak), frame| {
                            (
                                left_peak.max(frame[0].abs()),
                                right_peak.max(frame[1].abs()),
                            )
                        });
                    meters.publish_post_effect_stereo_peak(
                        track,
                        post_effect_left_peak,
                        post_effect_right_peak,
                    );
                }
            }
            master_output.apply(&mut block[..sample_count]);
            writer.write_stereo_samples(&block[..sample_count])?;
            block_start = block_start.saturating_add(frame_count as u64);
        }
    })();
    let stop_result = playback
        .vst3_processor
        .as_mut()
        .map_or(Ok(()), Vst3PlaylistStreamProcessor::stop_processing);
    match (stream_result, stop_result) {
        (Err(stream_error), _) => Err(stream_error),
        (Ok(_), Err(stop_error)) => Err(stop_error),
        (Ok(()), Ok(())) => Ok(render.summary),
    }
}

/// Stream live MIDI note events through one or more Sampler channels while transport is stopped.
///
/// The pattern is used to prepare each destination channel's sample and routing state. Its saved
/// notes are not played; note events arrive through `events` and are rendered on the worker.
fn prepare_live_sampler_destinations(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    source_channel_id: u16,
    target_channel_ids: &BTreeSet<u16>,
    cancelled: Option<&AtomicBool>,
) -> Result<(PreparedSamplerPattern, HashMap<u16, i16>), String> {
    let channels = document.channels();
    let channels_by_id = channels
        .iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    let source_channel = channels_by_id
        .get(&source_channel_id)
        .ok_or_else(|| format!("MIDI source channel {source_channel_id} was not found"))?;
    if source_channel.enabled() == Some(false) {
        return Err(format!(
            "MIDI source channel {source_channel_id} is disabled"
        ));
    }
    let expected_target_channel_ids = if source_channel.layer_child_ids().is_some() {
        let mut expected_targets = BTreeSet::new();
        collect_live_sampler_layer_targets(
            source_channel,
            &channels_by_id,
            &mut BTreeSet::new(),
            &mut expected_targets,
        );
        expected_targets
    } else if source_channel.kind() == Some(0) {
        BTreeSet::from([source_channel_id])
    } else {
        return Err(format!(
            "MIDI source channel {source_channel_id} is not a Sampler or Layer"
        ));
    };
    if target_channel_ids != &expected_target_channel_ids {
        return Err(format!(
            "Sampler destinations for source channel {source_channel_id} changed before audition started"
        ));
    }
    if target_channel_ids.is_empty() {
        return Err(format!(
            "MIDI source channel {source_channel_id} has no Sampler destinations"
        ));
    }
    for target_channel_id in target_channel_ids {
        let Some(channel) = channels_by_id.get(target_channel_id) else {
            return Err(format!("Sampler channel {target_channel_id} was not found"));
        };
        if channel.kind() != Some(0) || channel.enabled() == Some(false) {
            return Err(format!(
                "channel {target_channel_id} is not an enabled Sampler channel"
            ));
        }
    }

    let mut preparation_document = document.clone();
    let pattern_exists = preparation_document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .any(|pattern| pattern.id == options.pattern_id);
    if !pattern_exists {
        return Err(format!("pattern {} was not found", options.pattern_id));
    }
    for target_channel_id in target_channel_ids {
        let channel = channels_by_id[target_channel_id];
        let key = channel
            .keyboard_key_region()
            .map_or(SAMPLER_ROOT_KEY, |(low, _)| low.min(127));
        preparation_document
            .add_pattern_note(
                options.pattern_id,
                PatternNote {
                    position: 0,
                    flags: crate::PATTERN_NOTE_DEFAULT_FLAGS,
                    channel_id: *target_channel_id,
                    length: 1,
                    key,
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
        &preparation_document,
        project_path,
        options,
        cancelled,
        Some(target_channel_ids),
    )?;
    let mut pitch_offsets_by_channel = HashMap::new();
    for target_channel_id in target_channel_ids {
        let source = render
            .sources_by_channel
            .get(target_channel_id)
            .ok_or_else(|| format!("Sampler channel {target_channel_id} has no resolved sample"))?;
        let channel = channels_by_id[target_channel_id];
        pitch_offsets_by_channel.insert(
            *target_channel_id,
            sampler_note_pitch_offset_semitones(
                channel,
                source.root_key,
                document.metadata().master_pitch_cents(),
            ),
        );
    }
    Ok((render, pitch_offsets_by_channel))
}

fn collect_live_sampler_layer_targets(
    layer: &ChannelSummary,
    channels_by_id: &HashMap<u16, &ChannelSummary>,
    active_layers: &mut BTreeSet<u16>,
    targets: &mut BTreeSet<u16>,
) {
    if !active_layers.insert(layer.id()) {
        return;
    }
    let mut seen = BTreeSet::new();
    for child_id in layer.layer_child_ids().unwrap_or_default() {
        if !seen.insert(*child_id) {
            continue;
        }
        let Some(child) = channels_by_id.get(child_id) else {
            continue;
        };
        if child.enabled() == Some(false) {
            continue;
        }
        if child.layer_child_ids().is_some() {
            collect_live_sampler_layer_targets(child, channels_by_id, active_layers, targets);
        } else if child.kind() == Some(0) {
            targets.insert(*child_id);
        }
    }
    active_layers.remove(&layer.id());
}

pub fn stream_sampler_midi_input_to_device(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: SamplerPatternRenderOptions,
    routing: SamplerLiveInputRouting,
    events: SamplerLiveNoteReceiver,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let SamplerLiveInputRouting {
        source_channel_id,
        target_channel_ids,
    } = routing;
    let channels = document.channels();
    let (render, pitch_offsets_by_channel) = prepare_live_sampler_destinations(
        document,
        project_path.as_ref(),
        options,
        source_channel_id,
        &target_channel_ids,
        Some(cancelled),
    )?;
    let channels_by_id = channels
        .iter()
        .map(|channel| (channel.id(), channel))
        .collect::<HashMap<_, _>>();
    let ppq = document.header().ppq();
    let project_tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    let mut current_tempo_bpm = project_tempo_bpm;
    let mut base_resample_pitch_adjustment_by_channel = HashMap::<u16, f64>::new();
    let duration_stretch_channels = target_channel_ids
        .iter()
        .filter(|target_channel_id| {
            channels_by_id
                .get(target_channel_id)
                .and_then(|channel| channel.stretch_settings())
                .is_some_and(|settings| {
                    settings.time_raw() > 0
                        && matches!(
                            settings.mode(),
                            ChannelStretchMode::Stretch
                                | ChannelStretchMode::Elastique3Generic
                                | ChannelStretchMode::Elastique3Mono
                        )
                })
        })
        .copied()
        .collect::<BTreeSet<_>>();
    for target_channel_id in &target_channel_ids {
        let channel = channels_by_id
            .get(target_channel_id)
            .ok_or_else(|| format!("Sampler channel {target_channel_id} was not found"))?;
        let source = render
            .sources_by_channel
            .get(target_channel_id)
            .ok_or_else(|| format!("Sampler channel {target_channel_id} has no resolved sample"))?;
        if let Some((_, pitch_adjustment_cents)) = sampler_resample_stretch_pitch_adjustment(
            channel,
            source.audio.frame_count(),
            source.audio.sample_rate,
            ppq,
            project_tempo_bpm,
        )? {
            base_resample_pitch_adjustment_by_channel
                .insert(*target_channel_id, pitch_adjustment_cents);
        }
    }
    let channel_router = ChannelNoteRouter::new(channels.clone());
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
    let mut note_targets = HashMap::<(u8, u8), VecDeque<Vec<(u16, u16)>>>::new();
    let mut live_resample_fine_pitch_by_channel = HashMap::<u16, i16>::new();
    loop {
        check_cancelled(Some(cancelled))?;
        if writer.is_cancelled() {
            return Err("Sampler input audition was stopped".to_owned());
        }
        if events.take_overflowed() {
            events.discard_pending();
            note_targets.clear();
            for channel in 0_u8..16 {
                engine.stop_live_voices(channel);
                engine.set_live_sustain(channel, false);
                engine.set_live_sostenuto(channel, false);
                engine.apply_live_pitch_bend(channel, 0);
                engine.apply_live_pan(channel, 64);
                engine.apply_live_channel_volume(channel, 127);
                engine.apply_live_expression(channel, 127);
            }
        }
        loop {
            match events.try_recv() {
                Ok(SamplerLiveNoteEvent::TempoBpmMilli { value }) => {
                    let tempo_bpm = f64::from(value.max(1)) / 1000.0;
                    if (tempo_bpm - project_tempo_bpm).abs() > 0.0005
                        && let Some(target_channel_id) = duration_stretch_channels.first()
                    {
                        return Err(format!(
                            "Sampler channel {target_channel_id} uses duration-preserving time stretching; live tempo changes require rebuilding the stretched sample"
                        ));
                    }
                    current_tempo_bpm = tempo_bpm;
                    live_resample_fine_pitch_by_channel.clear();
                    for (target_channel_id, base_adjustment_cents) in
                        &base_resample_pitch_adjustment_by_channel
                    {
                        let channel = channels_by_id.get(target_channel_id).ok_or_else(|| {
                            format!("Sampler channel {target_channel_id} was not found")
                        })?;
                        let source = render
                            .sources_by_channel
                            .get(target_channel_id)
                            .ok_or_else(|| {
                                format!(
                                    "Sampler channel {target_channel_id} has no resolved sample"
                                )
                            })?;
                        let Some((_, pitch_adjustment_cents)) =
                            sampler_resample_stretch_pitch_adjustment(
                                channel,
                                source.audio.frame_count(),
                                source.audio.sample_rate,
                                ppq,
                                tempo_bpm,
                            )?
                        else {
                            return Err(format!(
                                "Sampler channel {target_channel_id} Resample settings changed while preparing live input"
                            ));
                        };
                        live_resample_fine_pitch_by_channel.insert(
                            *target_channel_id,
                            sampler_note_fine_pitch_with_delta(
                                0,
                                pitch_adjustment_cents - base_adjustment_cents,
                                *target_channel_id,
                            )?,
                        );
                    }
                }
                Ok(SamplerLiveNoteEvent::NoteOn {
                    channel: midi_channel,
                    key,
                    velocity,
                    route_seed,
                }) => {
                    let source_key = key.min(127);
                    let routed_targets = channel_router
                        .targets_for_note_with_layer_adjustments(
                            source_channel_id,
                            route_seed,
                            u16::from(source_key),
                        )
                        .into_iter()
                        .filter(|(target_channel_id, _, _)| {
                            target_channel_ids.contains(target_channel_id)
                        });
                    let mut active_targets = Vec::new();
                    for (target_channel_id, layer_gain, layer_pan_raw) in routed_targets {
                        let pitch_offset = pitch_offsets_by_channel
                            .get(&target_channel_id)
                            .copied()
                            .unwrap_or_default();
                        let note_key =
                            transpose_sampler_note_key(u16::from(source_key), pitch_offset);
                        if let Some(slot_index) = engine.start_voice(ScheduledSamplerNote {
                            clip_index: 0,
                            playlist_track_id: None,
                            start_tick: 0,
                            stop_tick: None,
                            start_frame: block_start,
                            stop_frame: Some(u64::MAX),
                            channel_id: target_channel_id,
                            key: note_key,
                            midi_channel: 0,
                            slide_note: false,
                            slide_duration_frames: engine
                                .sources_by_channel
                                .get(&target_channel_id)
                                .map_or(0, |source| {
                                    sampler_polyphony_slide_frames(
                                        source.polyphony_slide_raw,
                                        ppq,
                                        current_tempo_bpm,
                                        options.sample_rate,
                                    )
                                }),
                            velocity,
                            pan: engine.live_midi_pan(midi_channel),
                            fine_pitch_offset_cents: live_resample_fine_pitch_by_channel
                                .get(&target_channel_id)
                                .copied()
                                .unwrap_or_default(),
                            layer_gain,
                            layer_pan_raw,
                        }) {
                            engine.set_voice_input_midi_channel(slot_index, midi_channel);
                            engine.refresh_live_pan_for_channel(midi_channel);
                            active_targets.push((target_channel_id, note_key));
                        }
                    }
                    note_targets
                        .entry((midi_channel & 0x0F, source_key))
                        .or_default()
                        .push_back(active_targets);
                }
                Ok(SamplerLiveNoteEvent::NoteOff { channel, key }) => {
                    let source_note = (channel & 0x0F, key.min(127));
                    let active_targets = note_targets
                        .get_mut(&source_note)
                        .and_then(VecDeque::pop_front);
                    if note_targets
                        .get(&source_note)
                        .is_some_and(VecDeque::is_empty)
                    {
                        note_targets.remove(&source_note);
                    }
                    if let Some(active_targets) = active_targets {
                        for (target_channel_id, note_key) in active_targets {
                            engine.release_note(target_channel_id, note_key, channel);
                        }
                    }
                }
                Ok(SamplerLiveNoteEvent::PitchBend { channel, cents }) => {
                    engine.apply_live_pitch_bend(channel, cents);
                }
                Ok(SamplerLiveNoteEvent::Pan { channel, value }) => {
                    engine.apply_live_pan(channel, value);
                }
                Ok(SamplerLiveNoteEvent::Pan14Bit { channel, value }) => {
                    engine.apply_live_pan_14bit(channel, value);
                }
                Ok(SamplerLiveNoteEvent::ChannelVolume { channel, value }) => {
                    engine.apply_live_channel_volume(channel, value);
                }
                Ok(SamplerLiveNoteEvent::ChannelVolume14Bit { channel, value }) => {
                    engine.apply_live_channel_volume_14bit(channel, value);
                }
                Ok(SamplerLiveNoteEvent::Expression { channel, value }) => {
                    engine.apply_live_expression(channel, value);
                }
                Ok(SamplerLiveNoteEvent::Expression14Bit { channel, value }) => {
                    engine.apply_live_expression_14bit(channel, value);
                }
                Ok(SamplerLiveNoteEvent::AllNotesOff { channel }) => {
                    note_targets.retain(|(midi_channel, _), _| *midi_channel != (channel & 0x0F));
                    engine.release_live_notes(channel);
                }
                Ok(SamplerLiveNoteEvent::AllSoundOff { channel }) => {
                    note_targets.retain(|(midi_channel, _), _| *midi_channel != (channel & 0x0F));
                    engine.stop_live_voices(channel);
                }
                Ok(SamplerLiveNoteEvent::Sustain { channel, down }) => {
                    engine.set_live_sustain(channel, down);
                }
                Ok(SamplerLiveNoteEvent::Sostenuto { channel, down }) => {
                    engine.set_live_sostenuto(channel, down);
                }
                Ok(SamplerLiveNoteEvent::ResetControllers {
                    channel,
                    pan,
                    pitch_bend_cents,
                }) => {
                    engine.set_live_sustain(channel, false);
                    engine.set_live_sostenuto(channel, false);
                    engine.apply_live_pan(channel, pan);
                    engine.apply_live_channel_volume(channel, 127);
                    engine.apply_live_expression(channel, 127);
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

#[derive(Clone, Copy)]
enum LiveLayerNoteDestination {
    Sampler {
        channel_id: u16,
        key: u16,
    },
    Vst3 {
        channel_id: u16,
        note_id: i32,
        key: u8,
    },
}

/// Stream a mixed Sampler/VST3 Layer through one synchronized live audio path.
pub fn stream_mixed_layer_midi_input_to_device(
    mut processor: Vst3PlaylistStreamProcessor,
    document: FlpDocument,
    config: LayerLiveInputConfig,
    events: LayerLiveInputReceiver,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let available_vst3_channels = processor.performance_channel_ids();
    let vst3_volume_expression_channels = processor.performance_volume_expression_channel_ids();
    if !config
        .vst3_target_channel_ids
        .is_subset(&available_vst3_channels)
    {
        return Err(format!(
            "one or more VST3 children for Layer {} are not available for live MIDI audition",
            config.source_channel_id
        ));
    }
    if config.sampler_target_channel_ids.is_empty() || config.vst3_target_channel_ids.is_empty() {
        return Err("mixed Layer audition needs both Sampler and VST3 destinations".to_owned());
    }
    if !config
        .sampler_target_channel_ids
        .is_disjoint(&config.vst3_target_channel_ids)
    {
        return Err("mixed Layer destinations overlap".to_owned());
    }

    let (sampler_render, sampler_pitch_offsets) = prepare_live_sampler_destinations(
        &document,
        &config.project_path,
        config.sampler_options,
        config.source_channel_id,
        &config.sampler_target_channel_ids,
        Some(cancelled),
    )?;
    let channels = document.channels();
    let ppq = document.header().ppq();
    let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    let channel_router = ChannelNoteRouter::new(channels);
    let master_output = MixerMasterOutput::from_document(&document);
    let empty_notes = [];
    let mut sampler_engine = SamplerVoiceEngine::with_resampling_quality_and_pan_law(
        &sampler_render.sources_by_channel,
        &empty_notes,
        sampler_render.voice_limit,
        (f64::from(sampler_render.summary.sample_rate) * SAMPLER_RELEASE_SECONDS)
            .round()
            .max(1.0) as usize,
        sampler_render.summary.sample_rate,
        sampler_render.resampling_quality,
        sampler_render.pan_law,
    );
    sampler_engine.mixer_transforms_by_channel = Some(&sampler_render.mixer_transforms_by_channel);
    sampler_engine.mixer_meter_transforms_by_channel =
        Some(&sampler_render.mixer_meter_transforms_by_channel);
    sampler_engine.mixer_effect_input_transforms_by_channel =
        Some(&sampler_render.mixer_effect_input_transforms_by_channel);

    const LIVE_BLOCK_FRAMES: usize = 128;
    let mut block = vec![0.0f32; LIVE_BLOCK_FRAMES * 2];
    let mut sampler_block = vec![0.0f32; LIVE_BLOCK_FRAMES * 2];
    let direct_source_delay_frames = processor.pdc_direct_source_delay_frames();
    let mut direct_source_delay = crate::audio::StereoFrameDelay::new(direct_source_delay_frames)?;
    let mixer_effect_buses_required = processor.needs_mixer_insert_buses();
    let mixer_bus_planes = 1 + usize::from(mixer_effect_buses_required);
    let mut mixer_buses = mixer_effect_buses_required
        .then(|| vec![0.0f32; MIXER_TRACK_METER_COUNT * LIVE_BLOCK_FRAMES * 2 * mixer_bus_planes]);
    let mut mixer_bus_delays = if mixer_effect_buses_required {
        (0..MIXER_TRACK_METER_COUNT * mixer_bus_planes)
            .map(|_| StereoFrameDelay::new(direct_source_delay_frames))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let mut note_targets = HashMap::<(u8, u8), VecDeque<Vec<LiveLayerNoteDestination>>>::new();
    let mut note_seed = 0u64;
    processor.start_processing()?;
    let stream_result = (|| {
        let mut block_start = 0u64;
        loop {
            if cancelled.load(Ordering::Acquire) || writer.is_cancelled() {
                return Err("Layer input audition was stopped".to_owned());
            }
            let mut disconnected = false;
            loop {
                match events.try_recv() {
                    Ok(LayerLiveInputEvent::Midi(Vst3LiveInputEvent::NoteOn {
                        channel,
                        key,
                        velocity,
                    })) => {
                        let source_key = key.min(127);
                        let routed_targets = channel_router
                            .targets_for_note_with_mixed_crossfade(
                                config.source_channel_id,
                                note_seed,
                                u16::from(source_key),
                                &vst3_volume_expression_channels,
                            )
                            .into_iter()
                            .filter(|(target_channel_id, _, _)| {
                                config
                                    .sampler_target_channel_ids
                                    .contains(target_channel_id)
                                    || config.vst3_target_channel_ids.contains(target_channel_id)
                            });
                        note_seed = note_seed.wrapping_add(1);
                        let mut active_targets = Vec::new();
                        for (target_channel_id, layer_gain, layer_pan_raw) in routed_targets {
                            if config
                                .sampler_target_channel_ids
                                .contains(&target_channel_id)
                            {
                                let pitch_offset = sampler_pitch_offsets
                                    .get(&target_channel_id)
                                    .copied()
                                    .unwrap_or_default();
                                let rendered_key =
                                    transpose_sampler_note_key(u16::from(source_key), pitch_offset);
                                if let Some(slot_index) =
                                    sampler_engine.start_voice(ScheduledSamplerNote {
                                        clip_index: 0,
                                        playlist_track_id: None,
                                        start_tick: 0,
                                        stop_tick: None,
                                        start_frame: block_start,
                                        stop_frame: Some(u64::MAX),
                                        channel_id: target_channel_id,
                                        key: rendered_key,
                                        midi_channel: 0,
                                        slide_note: false,
                                        slide_duration_frames: sampler_engine
                                            .sources_by_channel
                                            .get(&target_channel_id)
                                            .map_or(0, |source| {
                                                sampler_polyphony_slide_frames(
                                                    source.polyphony_slide_raw,
                                                    ppq,
                                                    tempo_bpm,
                                                    sampler_render.summary.sample_rate,
                                                )
                                            }),
                                        velocity,
                                        pan: sampler_engine.live_midi_pan(channel),
                                        fine_pitch_offset_cents: 0,
                                        layer_gain,
                                        layer_pan_raw,
                                    })
                                {
                                    sampler_engine
                                        .set_voice_input_midi_channel(slot_index, channel);
                                    sampler_engine.refresh_live_pan_for_channel(channel);
                                    active_targets.push(LiveLayerNoteDestination::Sampler {
                                        channel_id: target_channel_id,
                                        key: rendered_key,
                                    });
                                }
                            } else if config.vst3_target_channel_ids.contains(&target_channel_id) {
                                let (note_id, rendered_key) = processor
                                    .schedule_live_input_note_on(
                                        target_channel_id,
                                        channel,
                                        source_key,
                                        velocity,
                                        layer_gain,
                                        layer_pan_raw,
                                    )?;
                                active_targets.push(LiveLayerNoteDestination::Vst3 {
                                    channel_id: target_channel_id,
                                    note_id,
                                    key: rendered_key,
                                });
                            }
                        }
                        note_targets
                            .entry((channel & 0x0F, source_key))
                            .or_default()
                            .push_back(active_targets);
                    }
                    Ok(LayerLiveInputEvent::Midi(Vst3LiveInputEvent::NoteOff { channel, key })) => {
                        let source_note = (channel & 0x0F, key.min(127));
                        let active_targets = note_targets
                            .get_mut(&source_note)
                            .and_then(VecDeque::pop_front);
                        if note_targets
                            .get(&source_note)
                            .is_some_and(VecDeque::is_empty)
                        {
                            note_targets.remove(&source_note);
                        }
                        if let Some(active_targets) = active_targets {
                            for target in active_targets {
                                match target {
                                    LiveLayerNoteDestination::Sampler { channel_id, key } => {
                                        sampler_engine.release_note(channel_id, key, channel);
                                    }
                                    LiveLayerNoteDestination::Vst3 {
                                        channel_id,
                                        note_id,
                                        key,
                                    } => {
                                        processor.schedule_live_input_note_off(
                                            channel_id, channel, key, note_id,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                    Ok(LayerLiveInputEvent::Midi(
                        live_event @ Vst3LiveInputEvent::PolyphonicPressure { channel, key, .. },
                    )) => {
                        let source_note = (channel & 0x0F, key.min(127));
                        let affected_channels = note_targets
                            .get(&source_note)
                            .into_iter()
                            .flatten()
                            .flatten()
                            .filter_map(|target| match target {
                                LiveLayerNoteDestination::Vst3 { channel_id, .. } => {
                                    Some(*channel_id)
                                }
                                LiveLayerNoteDestination::Sampler { .. } => None,
                            })
                            .collect::<BTreeSet<_>>();
                        let midi_event = live_input_controller_event(live_event)?;
                        for channel_id in affected_channels {
                            processor.schedule_live_input_midi_event(channel_id, midi_event)?;
                        }
                    }
                    Ok(LayerLiveInputEvent::Midi(live_event)) => {
                        if let Vst3LiveInputEvent::ControlChange {
                            channel,
                            controller,
                            value,
                        } = live_event
                        {
                            apply_live_layer_sampler_controller(
                                &mut sampler_engine,
                                channel,
                                controller,
                                value,
                            );
                            match controller {
                                120 => {
                                    note_targets.retain(|(midi_channel, _), _| {
                                        *midi_channel != (channel & 0x0F)
                                    });
                                    sampler_engine.stop_live_voices(channel);
                                }
                                123..=127 => {
                                    note_targets.retain(|(midi_channel, _), _| {
                                        *midi_channel != (channel & 0x0F)
                                    });
                                    sampler_engine.release_live_notes(channel);
                                }
                                _ => {}
                            }
                        }
                        let midi_event = live_input_controller_event(live_event)?;
                        for channel_id in &config.vst3_target_channel_ids {
                            processor.schedule_live_input_midi_event(*channel_id, midi_event)?;
                        }
                    }
                    Ok(LayerLiveInputEvent::SamplerPitchBend { channel, cents }) => {
                        sampler_engine.apply_live_pitch_bend(channel, cents);
                    }
                    Ok(LayerLiveInputEvent::SamplerPan14Bit { channel, value }) => {
                        sampler_engine.apply_live_pan_14bit(channel, value);
                    }
                    Ok(LayerLiveInputEvent::SamplerChannelVolume14Bit { channel, value }) => {
                        sampler_engine.apply_live_channel_volume_14bit(channel, value);
                    }
                    Ok(LayerLiveInputEvent::SamplerExpression14Bit { channel, value }) => {
                        sampler_engine.apply_live_expression_14bit(channel, value);
                    }
                    Ok(LayerLiveInputEvent::SamplerResetControllers {
                        channel,
                        pan,
                        pitch_bend_cents,
                    }) => {
                        reset_live_layer_sampler_controllers(
                            &mut sampler_engine,
                            channel,
                            pan,
                            pitch_bend_cents,
                        );
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                break;
            }
            if let Some(mixer_buses) = mixer_buses.as_mut() {
                mixer_buses.fill(0.0);
                sampler_engine.render_block_with_meters(
                    block_start,
                    LIVE_BLOCK_FRAMES,
                    &mut sampler_block,
                    Some(mixer_buses),
                    LIVE_BLOCK_FRAMES * 2,
                );
                for (track, delay) in mixer_bus_delays.iter_mut().enumerate() {
                    let bus_start = track * LIVE_BLOCK_FRAMES * 2;
                    delay.process_interleaved(
                        &mut mixer_buses[bus_start..bus_start + LIVE_BLOCK_FRAMES * 2],
                    )?;
                }
            } else {
                sampler_engine.render_block(block_start, LIVE_BLOCK_FRAMES, &mut sampler_block);
            }
            direct_source_delay.process_interleaved(&mut sampler_block)?;
            block.fill(0.0);
            for (mixed, sampler_sample) in block.iter_mut().zip(&sampler_block) {
                *mixed += *sampler_sample;
            }
            if let Some(mixer_buses) = mixer_buses.as_mut() {
                processor.mix_next_block_with_meter_buses(&mut block, mixer_buses)?;
            } else {
                processor.mix_next_block(&mut block)?;
            }
            master_output.apply(&mut block);
            writer.write_stereo_samples(&block)?;
            block_start = block_start.saturating_add(LIVE_BLOCK_FRAMES as u64);
        }
        Ok(())
    })();

    let cleanup_result = processor.stop_live_input_notes();
    let stop_result = processor.stop_processing();
    stream_result.and(cleanup_result).and(stop_result)
}

fn apply_live_layer_sampler_controller(
    engine: &mut SamplerVoiceEngine<'_>,
    channel: u8,
    controller: u8,
    value: u8,
) {
    match controller {
        10 => engine.apply_live_pan(channel, value),
        7 => engine.apply_live_channel_volume(channel, value),
        11 => engine.apply_live_expression(channel, value),
        64 => engine.set_live_sustain(channel, value >= 64),
        66 => engine.set_live_sostenuto(channel, value >= 64),
        _ => {}
    }
}

fn reset_live_layer_sampler_controllers(
    engine: &mut SamplerVoiceEngine<'_>,
    channel: u8,
    pan: u8,
    pitch_bend_cents: i16,
) {
    engine.set_live_sustain(channel, false);
    engine.set_live_sostenuto(channel, false);
    engine.apply_live_pan(channel, pan);
    engine.apply_live_channel_volume(channel, 127);
    engine.apply_live_expression(channel, 127);
    engine.apply_live_pitch_bend(channel, pitch_bend_cents);
}

/// Stream live MIDI events through prepared VST3 channels while transport is stopped.
///
/// Arrangement notes are removed from the prepared live stream before this function is called.
/// The processor still supplies each channel's mixer routing, latency compensation, and pitch
/// settings while note events arrive on the worker thread. Layer sources route notes through the
/// same key-range and Random selection rules used by rendered patterns.
pub fn stream_vst3_midi_input_to_device(
    mut processor: Vst3PlaylistStreamProcessor,
    document: FlpDocument,
    source_channel_id: u16,
    target_channel_ids: BTreeSet<u16>,
    events: Vst3LiveInputReceiver,
    writer: &StreamingAudioWriter,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let available_channel_ids = processor.performance_channel_ids();
    if !target_channel_ids.is_subset(&available_channel_ids) {
        return Err(format!(
            "one or more VST3 channels for source {source_channel_id} are not available for live MIDI audition"
        ));
    }
    let channel_router = ChannelNoteRouter::new(document.channels());
    let master_output = MixerMasterOutput::from_document(&document);
    processor.start_processing()?;

    const LIVE_BLOCK_FRAMES: usize = 128;
    let mut block = vec![0.0f32; LIVE_BLOCK_FRAMES * 2];
    let mixer_effect_buses_required = processor.needs_mixer_insert_buses();
    let mixer_bus_planes = 1 + usize::from(mixer_effect_buses_required);
    let mut mixer_buses = mixer_effect_buses_required
        .then(|| vec![0.0f32; MIXER_TRACK_METER_COUNT * LIVE_BLOCK_FRAMES * 2 * mixer_bus_planes]);
    let mut note_ids = HashMap::<(u8, u8), VecDeque<Vec<(u16, i32, u8)>>>::new();
    let mut note_seed = 0u64;
    let stream_result = (|| {
        loop {
            if cancelled.load(Ordering::Acquire) || writer.is_cancelled() {
                return Err("VST3 input audition was stopped".to_owned());
            }
            let mut disconnected = false;
            loop {
                match events.try_recv() {
                    Ok(Vst3LiveInputEvent::NoteOn {
                        channel,
                        key,
                        velocity,
                    }) => {
                        let source_key = key.min(127);
                        let routed_targets = channel_router
                            .targets_for_note_with_layer_adjustments(
                                source_channel_id,
                                note_seed,
                                u16::from(source_key),
                            )
                            .into_iter()
                            .filter(|(target_id, _, _)| target_channel_ids.contains(target_id));
                        note_seed = note_seed.wrapping_add(1);
                        let mut active_targets = Vec::new();
                        for (target_channel_id, layer_gain, layer_pan_raw) in routed_targets {
                            let (note_id, rendered_key) = processor.schedule_live_input_note_on(
                                target_channel_id,
                                channel,
                                source_key,
                                velocity,
                                layer_gain,
                                layer_pan_raw,
                            )?;
                            active_targets.push((target_channel_id, note_id, rendered_key));
                        }
                        note_ids
                            .entry((channel & 0x0F, source_key))
                            .or_default()
                            .push_back(active_targets);
                    }
                    Ok(Vst3LiveInputEvent::NoteOff { channel, key }) => {
                        let source_note = (channel & 0x0F, key.min(127));
                        let active_note =
                            note_ids.get_mut(&source_note).and_then(VecDeque::pop_front);
                        if note_ids.get(&source_note).is_some_and(VecDeque::is_empty) {
                            note_ids.remove(&source_note);
                        }
                        if let Some(active_targets) = active_note {
                            for (target_channel_id, note_id, rendered_key) in active_targets {
                                processor.schedule_live_input_note_off(
                                    target_channel_id,
                                    channel,
                                    rendered_key,
                                    note_id,
                                )?;
                            }
                        }
                    }
                    Ok(
                        live_event @ Vst3LiveInputEvent::PolyphonicPressure { channel, key, .. },
                    ) => {
                        let source_note = (channel & 0x0F, key.min(127));
                        let affected_targets = note_ids
                            .get(&source_note)
                            .into_iter()
                            .flatten()
                            .flatten()
                            .map(|(target_channel_id, _, _)| *target_channel_id)
                            .collect::<BTreeSet<_>>();
                        let midi_event = live_input_controller_event(live_event)?;
                        for target_channel_id in affected_targets {
                            processor
                                .schedule_live_input_midi_event(target_channel_id, midi_event)?;
                        }
                    }
                    Ok(live_event) => {
                        if let Vst3LiveInputEvent::ControlChange {
                            channel,
                            controller: 120 | 123..=127,
                            ..
                        } = live_event
                        {
                            note_ids
                                .retain(|(midi_channel, _), _| *midi_channel != (channel & 0x0F));
                        }
                        let midi_event = live_input_controller_event(live_event)?;
                        for target_channel_id in &target_channel_ids {
                            processor
                                .schedule_live_input_midi_event(*target_channel_id, midi_event)?;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                break;
            }
            if let Some(mixer_buses) = mixer_buses.as_mut() {
                mixer_buses.fill(0.0);
                processor.mix_next_block_with_meter_buses(&mut block, mixer_buses)?;
            } else {
                processor.mix_next_block(&mut block)?;
            }
            master_output.apply(&mut block);
            writer.write_stereo_samples(&block)?;
        }
        Ok(())
    })();

    let cleanup_result = processor.stop_live_input_notes();
    let stop_result = processor.stop_processing();
    stream_result.and(cleanup_result).and(stop_result)
}

fn live_input_controller_event(event: Vst3LiveInputEvent) -> Result<MidiEvent, String> {
    let channel = match event {
        Vst3LiveInputEvent::ControlChange { channel, .. }
        | Vst3LiveInputEvent::ProgramChange { channel, .. }
        | Vst3LiveInputEvent::PitchBend { channel, .. }
        | Vst3LiveInputEvent::ChannelPressure { channel, .. }
        | Vst3LiveInputEvent::PolyphonicPressure { channel, .. } => {
            MidiChannel::from_index(channel & 0x0F)
                .ok_or_else(|| "MIDI channel is outside 0..15".to_owned())?
        }
        Vst3LiveInputEvent::NoteOn { .. } | Vst3LiveInputEvent::NoteOff { .. } => {
            return Err("note events need VST3 note IDs".to_owned());
        }
    };
    Ok(match event {
        Vst3LiveInputEvent::ControlChange {
            controller, value, ..
        } => MidiEvent::ControlChange {
            channel,
            controller: controller.min(127),
            value: value.min(127),
        },
        Vst3LiveInputEvent::ProgramChange { program, .. } => MidiEvent::ProgramChange {
            channel,
            program: program.min(127),
        },
        Vst3LiveInputEvent::PitchBend { value, .. } => MidiEvent::PitchBend {
            channel,
            value: value.min(16_383),
        },
        Vst3LiveInputEvent::ChannelPressure { pressure, .. } => MidiEvent::ChannelAftertouch {
            channel,
            pressure: pressure.min(127),
        },
        Vst3LiveInputEvent::PolyphonicPressure { key, pressure, .. } => MidiEvent::PolyAftertouch {
            channel,
            note: key.min(127),
            pressure: pressure.min(127),
        },
        Vst3LiveInputEvent::NoteOn { .. } | Vst3LiveInputEvent::NoteOff { .. } => {
            unreachable!("note variants were handled before conversion")
        }
    })
}

fn stream_prepared_sampler_pattern(
    render: &PreparedSamplerPattern,
    cancelled: Option<&AtomicBool>,
    stream_cancelled: impl FnMut() -> bool,
    write_block: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<SamplerPatternRenderSummary, String> {
    stream_prepared_sampler_pattern_with_mixer_effects(
        render,
        None,
        render.summary.frames,
        0,
        cancelled,
        stream_cancelled,
        write_block,
    )
}

fn stream_prepared_sampler_pattern_with_mixer_effects(
    render: &PreparedSamplerPattern,
    mut processor: Option<&mut Vst3PlaylistStreamProcessor>,
    render_frames: u64,
    trim_frames: u64,
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
    if processor.is_some() {
        engine.mixer_meter_transforms_by_channel = Some(&render.mixer_meter_transforms_by_channel);
        engine.mixer_effect_input_transforms_by_channel =
            Some(&render.mixer_effect_input_transforms_by_channel);
    }
    let mut block = vec![0.0f32; SAMPLER_BLOCK_FRAMES * 2];
    let mut mixer_buses = Vec::new();
    let mut block_start = 0u64;
    while block_start < render_frames {
        check_cancelled(cancelled)?;
        if stream_cancelled() {
            return Err("Sampler pattern playback was stopped".to_owned());
        }
        let frame_count = (render_frames - block_start).min(SAMPLER_BLOCK_FRAMES as u64) as usize;
        let block_samples = &mut block[..frame_count * 2];
        if let Some(processor) = processor.as_deref_mut() {
            let bus_samples = frame_count
                .checked_mul(2)
                .and_then(|samples| samples.checked_mul(MIXER_TRACK_METER_COUNT))
                .and_then(|samples| samples.checked_mul(2))
                .ok_or_else(|| "Sampler Mixer effect bus size overflow".to_owned())?;
            mixer_buses.resize(bus_samples, 0.0);
            mixer_buses.fill(0.0);
            engine.render_block_with_meters(
                block_start,
                frame_count,
                block_samples,
                Some(&mut mixer_buses),
                frame_count * 2,
            );
            processor.mix_next_block_with_meter_buses(block_samples, &mut mixer_buses)?;
        } else {
            engine.render_block(block_start, frame_count, block_samples);
        }
        render.master_output.apply(block_samples);
        let skipped_frames = trim_frames
            .saturating_sub(block_start)
            .min(frame_count as u64);
        if skipped_frames < frame_count as u64 {
            let skip_samples = usize::try_from(skipped_frames)
                .ok()
                .and_then(|frames| frames.checked_mul(2))
                .ok_or_else(|| "Sampler Pattern PDC trim overflow".to_owned())?;
            write_block(&block_samples[skip_samples..])?;
        }
        block_start += frame_count as u64;
    }
    let mut summary = render.summary.clone();
    summary.voices_stolen = engine.voices_stolen;
    summary.frames = render_frames.saturating_sub(trim_frames);
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
            let audio = render.audio_sources.get(clip.source_id).ok_or_else(|| {
                format!(
                    "prepared audio clip {} from {} has no decoded source",
                    clip.clip_index,
                    clip.path.display()
                )
            })?;
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

#[allow(clippy::too_many_arguments)]
fn stream_prepared_audio_clip_render_with_mixer_effects(
    render: &PreparedAudioClipRender,
    processor: &mut Vst3PlaylistStreamProcessor,
    render_frames: u64,
    trim_frames: u64,
    output_start_frame: u64,
    master_output: &MixerMasterOutput,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<AudioClipRenderSummary, String> {
    crate::audio::enable_denormal_protection();
    let cancelled = AtomicBool::new(false);
    let context = PreparedAudioClipMixContext {
        clips: &render.clips,
        audio_sources: &render.audio_sources,
        sample_rate: render.summary.sample_rate,
        resampling_quality: render.resampling_quality,
        cancelled: &cancelled,
        live_mixer_source_targets: None,
    };
    let mut block = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
    let mut mixer_buses = Vec::new();
    let first_output_frame = trim_frames
        .checked_add(output_start_frame)
        .ok_or_else(|| "Audio Clip render output start overflow".to_owned())?;
    let mut block_start = 0u64;
    while block_start < render_frames {
        check_cancelled(Some(&cancelled))?;
        if stream_cancelled() {
            return Err("audio clip render was stopped".to_owned());
        }
        let frame_count = (render_frames - block_start).min(STREAM_BLOCK_FRAMES as u64) as usize;
        let sample_count = frame_count * 2;
        let block_samples = &mut block[..sample_count];
        block_samples.fill(0.0);
        let bus_samples = sample_count
            .checked_mul(MIXER_TRACK_METER_COUNT)
            .and_then(|samples| samples.checked_mul(2))
            .ok_or_else(|| "Audio Clip Mixer effect bus size overflow".to_owned())?;
        mixer_buses.resize(bus_samples, 0.0);
        mixer_buses.fill(0.0);
        mix_prepared_audio_clip_block(
            &context,
            block_start,
            block_samples,
            Some(&mut mixer_buses),
            None,
        )?;
        processor.mix_next_block_with_meter_buses(block_samples, &mut mixer_buses)?;
        master_output.apply(block_samples);

        let skipped_frames = first_output_frame
            .saturating_sub(block_start)
            .min(frame_count as u64);
        if skipped_frames < frame_count as u64 {
            let skip_samples = usize::try_from(skipped_frames)
                .ok()
                .and_then(|frames| frames.checked_mul(2))
                .ok_or_else(|| "Audio Clip PDC trim overflow".to_owned())?;
            write_block(&block_samples[skip_samples..])?;
        }
        block_start += frame_count as u64;
    }
    let mut summary = render.summary.clone();
    summary.frames = render_frames
        .checked_sub(first_output_frame)
        .ok_or_else(|| "Audio Clip render endpoint precedes its output start".to_owned())?;
    Ok(summary)
}

fn render_audio_clips_to_stereo_buffer_inner(
    document: &FlpDocument,
    project_path: &Path,
    options: AudioClipRenderOptions,
    cancelled: Option<&AtomicBool>,
) -> Result<(Vec<f32>, AudioClipRenderSummary), String> {
    crate::audio::enable_denormal_protection();
    let render =
        prepare_audio_clip_render(document, project_path, options.clone(), false, cancelled)?;
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
        let audio = render.audio_sources.get(clip.source_id).ok_or_else(|| {
            format!(
                "prepared audio clip {} from {} has no decoded source",
                clip.clip_index,
                clip.path.display()
            )
        })?;
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

fn selected_audio_clip_indices(
    options: &AudioClipRenderOptions,
) -> Result<Option<Vec<usize>>, String> {
    if options.clip_indices.is_empty() {
        return Ok(options.clip_index.map(|clip_index| vec![clip_index]));
    }
    if options.clip_index.is_some() {
        return Err("select either one Audio Clip or an Audio Clip group, not both".to_owned());
    }
    let mut clip_indices = options.clip_indices.clone();
    clip_indices.sort_unstable();
    let original_len = clip_indices.len();
    clip_indices.dedup();
    if clip_indices.len() != original_len {
        return Err("Audio Clip selection contains duplicate clips".to_owned());
    }
    Ok(Some(clip_indices))
}

fn audio_clip_matches_selection(
    selected_clip_indices: Option<&[usize]>,
    clip_index: usize,
) -> bool {
    selected_clip_indices.is_none_or(|selected_indices| selected_indices.contains(&clip_index))
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

#[derive(Clone, Copy)]
struct AudioSourceDimensions {
    frames: usize,
    channels: usize,
    sample_rate: u32,
}

fn channel_audio_clip_stretch_plan(
    settings: Option<(u32, i32, i32, i32)>,
    tempo_map: &PlaylistTempoMap,
    ppq: u16,
    clip_start_tick: u32,
    source: AudioSourceDimensions,
    clip_index: usize,
) -> Result<Option<AudioClipStretchPlan>, String> {
    let AudioSourceDimensions {
        frames: source_frames,
        channels: source_channels,
        sample_rate,
    } = source;
    let Some((time_raw, pitch_raw, multiplier_raw, mode_raw)) =
        settings.filter(|settings| settings.0 > 0)
    else {
        return Ok(None);
    };
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if source_frames == 0 {
        return Err(format!(
            "Playlist clip {clip_index} has an empty audio source range"
        ));
    }
    if !(-1_200..=1_200).contains(&pitch_raw) {
        return Err(format!(
            "Playlist clip {clip_index} has unsupported channel stretch pitch {pitch_raw} cents"
        ));
    }
    let multiplier = 2.0_f64.powf(f64::from(multiplier_raw) / 10_000.0);
    if !multiplier.is_finite() || !(0.25..=4.0).contains(&multiplier) {
        return Err(format!(
            "Playlist clip {clip_index} has channel stretch multiplier outside 0.25x to 4x"
        ));
    }

    let start_tick = f64::from(clip_start_tick);
    let target_ticks = f64::from(time_raw) * f64::from(ppq) / 192.0 * multiplier;
    let end_tick = start_tick + target_ticks;
    if !end_tick.is_finite() || end_tick <= start_tick {
        return Err(format!(
            "Playlist clip {clip_index} channel stretch duration is outside the renderable timeline"
        ));
    }
    if tempo_map.has_tempo_change_between(start_tick, end_tick) {
        return Err(format!(
            "Playlist clip {clip_index} channel time stretching crosses tempo automation, which this renderer cannot follow"
        ));
    }
    let start_frame = tempo_map.frame_at_fractional_tick(start_tick, sample_rate)?;
    let end_frame = tempo_map.frame_at_fractional_tick(end_tick, sample_rate)?;
    let target_duration_frames = end_frame.saturating_sub(start_frame);
    if target_duration_frames == 0 {
        return Err(format!(
            "Playlist clip {clip_index} channel stretch duration is shorter than one sample frame"
        ));
    }

    let pitch_factor = 2.0_f64.powf(f64::from(pitch_raw) / 1_200.0);
    match ChannelStretchMode::from_raw(mode_raw) {
        ChannelStretchMode::Resample => {
            let playback_rate_ratio =
                source_frames as f64 / target_duration_frames as f64 * pitch_factor;
            if !playback_rate_ratio.is_finite() || playback_rate_ratio <= 0.0 {
                return Err(format!(
                    "Playlist clip {clip_index} channel resample rate is outside the renderable range"
                ));
            }
            Ok(Some(AudioClipStretchPlan::Resample {
                playback_rate_ratio,
            }))
        }
        ChannelStretchMode::Elastique3Mono if source_channels != 1 => Err(format!(
            "Playlist clip {clip_index} uses Elastique 3 Mono with a non-mono source"
        )),
        ChannelStretchMode::Stretch
        | ChannelStretchMode::Elastique3Generic
        | ChannelStretchMode::Elastique3Mono => {
            let stretch_ratio = target_duration_frames as f64 / source_frames as f64;
            if !stretch_ratio.is_finite() || !(0.5..=2.0).contains(&stretch_ratio) {
                return Err(format!(
                    "Playlist clip {clip_index} needs a stretch ratio outside the supported 0.5x to 2x range"
                ));
            }
            Ok(Some(AudioClipStretchPlan::DurationPreserving {
                target_duration_frames,
                pitch_raw,
                mode_raw,
            }))
        }
        mode => Err(format!(
            "Playlist clip {clip_index} uses unsupported channel stretch mode {} ({mode:?})",
            mode_raw
        )),
    }
}

fn playlist_instance_audio_clip_stretch_plan(
    mode_raw: i32,
    pitch_raw: i32,
    tempo_map: &PlaylistTempoMap,
    clip_start_tick: u32,
    clip_length_ticks: u32,
    source: AudioSourceDimensions,
    clip_index: usize,
) -> Result<AudioClipStretchPlan, String> {
    if clip_length_ticks == 0 {
        return Err(format!(
            "Playlist clip {clip_index} has no timeline duration to stretch to"
        ));
    }
    if source.frames == 0 {
        return Err(format!(
            "Playlist clip {clip_index} has an empty audio source range"
        ));
    }
    if !(-1_200..=1_200).contains(&pitch_raw) {
        return Err(format!(
            "Playlist clip {clip_index} has unsupported instance stretch pitch {pitch_raw} cents"
        ));
    }
    let end_tick = clip_start_tick
        .checked_add(clip_length_ticks)
        .ok_or_else(|| format!("Playlist clip {clip_index} duration overflows the timeline"))?;
    let target_duration_frames = tempo_map.frames_between_ticks(
        u64::from(clip_start_tick),
        u64::from(end_tick),
        source.sample_rate,
    )?;
    if target_duration_frames == 0 {
        return Err(format!(
            "Playlist clip {clip_index} duration is shorter than one sample frame"
        ));
    }

    match ChannelStretchMode::from_raw(mode_raw) {
        ChannelStretchMode::Resample => {
            let pitch_factor = 2.0_f64.powf(f64::from(pitch_raw) / 1_200.0);
            let playback_rate_ratio =
                source.frames as f64 / target_duration_frames as f64 * pitch_factor;
            if !playback_rate_ratio.is_finite() || playback_rate_ratio <= 0.0 {
                return Err(format!(
                    "Playlist clip {clip_index} instance resample rate is outside the renderable range"
                ));
            }
            Ok(AudioClipStretchPlan::Resample {
                playback_rate_ratio,
            })
        }
        ChannelStretchMode::Elastique3Mono if source.channels != 1 => Err(format!(
            "Playlist clip {clip_index} uses Elastique 3 Mono with a non-mono source"
        )),
        ChannelStretchMode::Stretch
        | ChannelStretchMode::Elastique3Generic
        | ChannelStretchMode::Elastique3Mono => {
            let stretch_ratio = target_duration_frames as f64 / source.frames as f64;
            if !stretch_ratio.is_finite() || !(0.5..=2.0).contains(&stretch_ratio) {
                return Err(format!(
                    "Playlist clip {clip_index} needs a stretch ratio outside the supported 0.5x to 2x range"
                ));
            }
            Ok(AudioClipStretchPlan::DurationPreserving {
                target_duration_frames,
                pitch_raw,
                mode_raw,
            })
        }
        mode => Err(format!(
            "Playlist clip {clip_index} uses unsupported instance stretch mode {mode_raw} ({mode:?})"
        )),
    }
}

fn sampler_resample_stretch_pitch_adjustment(
    channel: &ChannelSummary,
    source_frames: usize,
    source_sample_rate: u32,
    ppq: u16,
    tempo_bpm: f64,
) -> Result<Option<(f64, f64)>, String> {
    let Some(settings) = channel.stretch_settings().filter(|settings| {
        settings.time_raw() > 0 && settings.mode() == ChannelStretchMode::Resample
    }) else {
        return Ok(None);
    };
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if source_frames == 0 || source_sample_rate == 0 {
        return Err(format!(
            "Sampler channel {} has an empty or invalid audio source range",
            channel.id()
        ));
    }
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    let pitch_raw = settings.pitch_raw();
    if !(-1_200..=1_200).contains(&pitch_raw) {
        return Err(format!(
            "Sampler channel {} has unsupported Resample pitch {pitch_raw} cents",
            channel.id()
        ));
    }
    let multiplier = 2.0_f64.powf(f64::from(settings.multiplier_raw()) / 10_000.0);
    if !multiplier.is_finite() || !(0.25..=4.0).contains(&multiplier) {
        return Err(format!(
            "Sampler channel {} has Resample multiplier outside 0.25x to 4x",
            channel.id()
        ));
    }

    let target_ticks = f64::from(settings.time_raw()) * f64::from(ppq) / 192.0 * multiplier;
    let target_seconds = target_ticks * 60.0 / (f64::from(ppq) * tempo_bpm);
    if !target_ticks.is_finite()
        || target_ticks <= 0.0
        || !target_seconds.is_finite()
        || target_seconds <= 0.0
    {
        return Err(format!(
            "Sampler channel {} Resample duration is outside the renderable range",
            channel.id()
        ));
    }

    let source_seconds = source_frames as f64 / f64::from(source_sample_rate);
    let pitch_factor = 2.0_f64.powf(f64::from(pitch_raw) / 1_200.0);
    let playback_rate_ratio = source_seconds / target_seconds * pitch_factor;
    if !playback_rate_ratio.is_finite() || playback_rate_ratio <= 0.0 {
        return Err(format!(
            "Sampler channel {} Resample rate is outside the renderable range",
            channel.id()
        ));
    }
    let pitch_adjustment_cents = 1_200.0 * playback_rate_ratio.log2();
    if !pitch_adjustment_cents.is_finite() {
        return Err(format!(
            "Sampler channel {} Resample pitch is outside the renderable range",
            channel.id()
        ));
    }
    Ok(Some((target_ticks, pitch_adjustment_cents)))
}

#[derive(Clone, Copy, Debug)]
struct SamplerDurationPreservingStretchPlan {
    target_ticks: f64,
    target_frames: usize,
    stretch_ratio: f64,
    pitch_raw: i32,
    tempo_bpm: f64,
}

#[derive(Clone, Copy)]
struct SamplerDurationPreservingStretchSettings {
    time_raw: u32,
    pitch_raw: i32,
    multiplier_raw: i32,
    mode_raw: i32,
}

fn sampler_duration_preserving_stretch_plan(
    channel: &ChannelSummary,
    source_frames: usize,
    source_channels: usize,
    source_sample_rate: u32,
    ppq: u16,
    tempo_bpm: f64,
) -> Result<Option<SamplerDurationPreservingStretchPlan>, String> {
    let Some(settings) = channel
        .stretch_settings()
        .filter(|settings| settings.time_raw() > 0)
    else {
        return Ok(None);
    };
    sampler_duration_preserving_stretch_plan_from_settings(
        channel.id(),
        SamplerDurationPreservingStretchSettings {
            time_raw: settings.time_raw(),
            pitch_raw: settings.pitch_raw(),
            multiplier_raw: settings.multiplier_raw(),
            mode_raw: settings.mode_raw(),
        },
        source_frames,
        source_channels,
        source_sample_rate,
        ppq,
        tempo_bpm,
    )
}

fn sampler_duration_preserving_stretch_plan_from_settings(
    channel_id: u16,
    settings: SamplerDurationPreservingStretchSettings,
    source_frames: usize,
    source_channels: usize,
    source_sample_rate: u32,
    ppq: u16,
    tempo_bpm: f64,
) -> Result<Option<SamplerDurationPreservingStretchPlan>, String> {
    match ChannelStretchMode::from_raw(settings.mode_raw) {
        ChannelStretchMode::Resample => return Ok(None),
        ChannelStretchMode::Stretch | ChannelStretchMode::Elastique3Generic => {}
        ChannelStretchMode::Elastique3Mono if source_channels == 1 => {}
        ChannelStretchMode::Elastique3Mono => {
            return Err(format!(
                "Sampler channel {channel_id} uses Elastique 3 Mono with a non-mono source"
            ));
        }
        mode => {
            return Err(format!(
                "Sampler channel {} uses unsupported time-stretch mode {} ({mode:?})",
                channel_id, settings.mode_raw
            ));
        }
    }
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if settings.time_raw == 0 {
        return Ok(None);
    }
    if source_frames == 0 || source_sample_rate == 0 {
        return Err(format!(
            "Sampler channel {channel_id} has an empty or invalid audio source range"
        ));
    }
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    if !(-1_200..=1_200).contains(&settings.pitch_raw) {
        return Err(format!(
            "Sampler channel {channel_id} has unsupported stretch pitch {} cents",
            settings.pitch_raw
        ));
    }
    let multiplier = 2.0_f64.powf(f64::from(settings.multiplier_raw) / 10_000.0);
    if !multiplier.is_finite() || !(0.25..=4.0).contains(&multiplier) {
        return Err(format!(
            "Sampler channel {channel_id} stretch multiplier is outside 0.25x to 4x"
        ));
    }

    let target_ticks = f64::from(settings.time_raw) * f64::from(ppq) / 192.0 * multiplier;
    let target_seconds = target_ticks * 60.0 / (f64::from(ppq) * tempo_bpm);
    let target_frames_f64 = target_seconds * f64::from(source_sample_rate);
    if !target_frames_f64.is_finite()
        || target_frames_f64 <= 0.0
        || target_frames_f64 > usize::MAX as f64
    {
        return Err(format!(
            "Sampler channel {channel_id} stretch duration is outside the renderable range"
        ));
    }
    let target_frames = target_frames_f64.round() as usize;
    let stretch_ratio = target_frames as f64 / source_frames as f64;
    if !target_ticks.is_finite()
        || target_ticks <= 0.0
        || !target_seconds.is_finite()
        || target_seconds <= 0.0
        || target_frames == 0
        || !stretch_ratio.is_finite()
        || !(0.5..=2.0).contains(&stretch_ratio)
    {
        return Err(format!(
            "Sampler channel {channel_id} needs a duration-preserving stretch ratio outside the supported 0.5x to 2x range"
        ));
    }

    Ok(Some(SamplerDurationPreservingStretchPlan {
        target_ticks,
        target_frames,
        stretch_ratio,
        pitch_raw: settings.pitch_raw,
        tempo_bpm,
    }))
}

fn process_sampler_duration_preserving_stretch(
    audio: &DecodedAudio,
    plan: SamplerDurationPreservingStretchPlan,
) -> Result<DecodedAudio, String> {
    let mut processed = process_audio_range_for_playback(
        audio,
        0..audio.frame_count(),
        plan.pitch_raw as f32 / 100.0,
        plan.stretch_ratio as f32,
    )?;
    if processed.frame_count() != plan.target_frames {
        for channel in &mut processed.channels {
            let last_sample = channel.last().copied().unwrap_or_default();
            channel.resize(plan.target_frames, last_sample);
        }
    }
    Ok(processed)
}

struct PreparedSamplerStretchSource {
    audio: Arc<DecodedAudio>,
    loop_points: Option<(usize, usize)>,
    release_loop_points: Option<(usize, usize)>,
    duration_stretch_plan: Option<SamplerDurationPreservingStretchPlan>,
}

fn prepare_sampler_stretch_source(
    channel: &ChannelSummary,
    audio: Arc<DecodedAudio>,
    loop_points: Option<(usize, usize)>,
    release_loop_points: Option<(usize, usize)>,
    ppq: u16,
    tempo_bpm: f64,
    cached_source_bytes: &mut usize,
) -> Result<PreparedSamplerStretchSource, String> {
    let Some(plan) = sampler_duration_preserving_stretch_plan(
        channel,
        audio.frame_count(),
        audio.channels.len(),
        audio.sample_rate,
        ppq,
        tempo_bpm,
    )?
    else {
        return Ok(PreparedSamplerStretchSource {
            audio,
            loop_points,
            release_loop_points,
            duration_stretch_plan: None,
        });
    };

    let source_frames = audio.frame_count();
    let stretched_audio = process_sampler_duration_preserving_stretch(&audio, plan)?;
    *cached_source_bytes = (*cached_source_bytes)
        .checked_add(decoded_audio_bytes(&stretched_audio)?)
        .filter(|total| *total <= MAX_SOURCE_CACHE_BYTES)
        .ok_or_else(|| {
            format!(
                "sampler sources exceed the {} MiB render cache limit",
                MAX_SOURCE_CACHE_BYTES / (1024 * 1024)
            )
        })?;
    let stretched_frames = stretched_audio.frame_count();
    Ok(PreparedSamplerStretchSource {
        audio: Arc::new(stretched_audio),
        loop_points: scale_sampler_loop_points(loop_points, source_frames, stretched_frames),
        release_loop_points: scale_sampler_loop_points(
            release_loop_points,
            source_frames,
            stretched_frames,
        ),
        duration_stretch_plan: Some(plan),
    })
}

fn scale_sampler_loop_points(
    loop_points: Option<(usize, usize)>,
    source_frames: usize,
    output_frames: usize,
) -> Option<(usize, usize)> {
    let (start, end) = loop_points?;
    if source_frames == 0 || start >= end || end > source_frames {
        return None;
    }
    let scale_floor =
        |frame: usize| (frame as u128 * output_frames as u128 / source_frames as u128) as usize;
    let scale_ceil = |frame: usize| {
        let numerator = frame as u128 * output_frames as u128;
        let denominator = source_frames as u128;
        let quotient = numerator / denominator;
        let rounded = quotient + u128::from(!numerator.is_multiple_of(denominator));
        usize::try_from(rounded).ok()
    };
    let scaled_start = scale_floor(start);
    let scaled_end = scale_ceil(end)?;
    (scaled_start < scaled_end && scaled_end <= output_frames).then_some((scaled_start, scaled_end))
}

fn validate_sampler_duration_stretch_tempo(
    plan: SamplerDurationPreservingStretchPlan,
    channel_id: u16,
    tempo_map: &PlaylistTempoMap,
    start_tick: u64,
    stop_tick: Option<u64>,
    has_loop: bool,
) -> Result<(), String> {
    let start_tick_f64 = start_tick as f64;
    let active_tempo = tempo_map.tempo_bpm_at_tick(start_tick);
    if (active_tempo - plan.tempo_bpm).abs() > 0.0005 {
        return Err(format!(
            "Sampler channel {channel_id} uses duration-preserving time stretching at {:.3} BPM, but a note starts at {:.3} BPM",
            plan.tempo_bpm, active_tempo
        ));
    }
    let target_end_tick = start_tick_f64 + plan.target_ticks;
    let active_end_tick = match (has_loop, stop_tick) {
        (true, Some(stop_tick)) => stop_tick as f64,
        (true, None) => f64::MAX,
        (false, Some(stop_tick)) => target_end_tick.min(stop_tick as f64),
        (false, None) => target_end_tick,
    };
    if tempo_map.has_tempo_change_between(start_tick_f64, active_end_tick) {
        return Err(format!(
            "Sampler channel {channel_id} uses duration-preserving time stretching across Playlist tempo automation, which this renderer cannot follow"
        ));
    }
    Ok(())
}

fn sampler_note_fine_pitch_with_delta(
    note_pitch_cents: i16,
    pitch_delta_cents: f64,
    channel_id: u16,
) -> Result<i16, String> {
    let adjusted_pitch_cents = f64::from(note_pitch_cents) + pitch_delta_cents;
    if !adjusted_pitch_cents.is_finite()
        || adjusted_pitch_cents < f64::from(i16::MIN)
        || adjusted_pitch_cents > f64::from(i16::MAX)
    {
        return Err(format!(
            "Sampler channel {channel_id} Resample pitch exceeds the supported range"
        ));
    }
    Ok(adjusted_pitch_cents.round() as i16)
}

fn prepare_audio_clip_render(
    document: &FlpDocument,
    project_path: &Path,
    options: AudioClipRenderOptions,
    include_muted_mixer_sources: bool,
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

    let selected_clip_indices = selected_audio_clip_indices(&options)?;
    if let Some(selected_clip_indices) = &selected_clip_indices {
        for clip_index in selected_clip_indices {
            if arrangement.clips.get(*clip_index).is_none() {
                return Err(format!("Playlist clip {clip_index} does not exist"));
            }
        }
    }
    let selected_clip_position_ticks = selected_clip_indices.as_ref().and_then(|clip_indices| {
        clip_indices
            .iter()
            .filter_map(|clip_index| arrangement.clips.get(*clip_index))
            .map(|clip| clip.position_ticks)
            .min()
    });
    let selection_start_tick = audio_clip_selection_start_tick(
        selected_clip_position_ticks,
        options.start_from_song_start,
    );

    let mut max_tick = 0u64;
    let mut clips_skipped_unsupported_scale = 0usize;
    let mut candidate_clips = Vec::<(
        usize,
        &PlaylistClip,
        u16,
        PathBuf,
        bool,
        f32,
        f32,
        f64,
        Option<i8>,
        Option<(u32, i32, i32, i32)>,
        MixerSourceRoutePlan,
    )>::new();
    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        check_cancelled(cancelled)?;
        let is_selected = selected_clip_indices
            .as_ref()
            .is_some_and(|selected_indices| selected_indices.contains(&clip_index));
        if !audio_clip_matches_selection(selected_clip_indices.as_deref(), clip_index) {
            continue;
        }
        let relative_position =
            audio_clip_render_start_tick(clip.position_ticks, selection_start_tick)?;
        max_tick = max_tick.max(relative_position + u64::from(clip.length_ticks));
        if track_filter.clip_is_muted(clip) {
            if is_selected {
                return Err(format!(
                    "selected Playlist clip {clip_index} is muted or on a disabled track"
                ));
            }
            continue;
        }
        let PlaylistClipTarget::Channel { id } = clip.target() else {
            if is_selected {
                return Err(format!(
                    "selected Playlist clip {clip_index} does not target an audio channel"
                ));
            }
            continue;
        };
        let Some(channel) = channels.iter().find(|channel| channel.id() == id) else {
            if is_selected {
                return Err(format!(
                    "selected Playlist clip {clip_index} references missing audio channel {id}"
                ));
            }
            continue;
        };
        let mixer_route_allowed = if include_muted_mixer_sources {
            mixer_route_audibility.allows_live_source_channel(channel.mixer_track())
        } else {
            mixer_route_audibility.allows_channel(channel.mixer_track())
        };
        if channel.kind() != Some(4) || channel.enabled() == Some(false) || !mixer_route_allowed {
            if is_selected {
                return Err(format!(
                    "selected Playlist clip {clip_index} does not target an enabled audio channel"
                ));
            }
            continue;
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            clips_skipped_unsupported_scale += 1;
            if is_selected {
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
            channel.audio_clip_reversed(),
            gain,
            pan,
            f64::from(channel.master_pitch_offset_cents(master_pitch_cents))
                + f64::from(channel.keyboard_fine_tune_cents().unwrap_or_default()),
            mixer_track,
            channel.stretch_settings().map(|settings| {
                (
                    settings.time_raw(),
                    settings.pitch_raw(),
                    settings.multiplier_raw(),
                    settings.mode_raw(),
                )
            }),
            mixer_route_plan,
        ));
    }
    if selected_clip_indices.is_some() && candidate_clips.is_empty() {
        return Err("selected Playlist audio clip group produced no render sources".to_owned());
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
    let mut source_id_by_path = HashMap::<PathBuf, usize>::new();
    let mut source_paths = BTreeSet::new();
    let mut audio_sources = Vec::<DecodedAudio>::new();
    let mut cached_source_bytes = 0usize;
    for (_, _, _, path, _, _, _, _, _, _, _) in &candidate_clips {
        check_cancelled(cancelled)?;
        if source_id_by_path.contains_key(path) {
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
        source_id_by_path.insert(path.clone(), audio_sources.len());
        source_paths.insert(path.clone());
        audio_sources.push(audio);
    }

    let mut processed_source_ids = HashMap::<ProcessedAudioCacheKey, usize>::new();
    let mut clips = Vec::with_capacity(candidate_clips.len());
    let mut output_frames = timeline_frames;
    let mut live_source_routes_by_track = HashMap::new();
    for (
        clip_index,
        clip,
        channel_id,
        path,
        reverse,
        gain,
        pan,
        pitch_offset_cents,
        mixer_track,
        stretch_settings,
        mixer_route_plan,
    ) in &candidate_clips
    {
        check_cancelled(cancelled)?;
        let raw_source_id = *source_id_by_path
            .get(path)
            .expect("all candidate source files were decoded");
        let audio = &audio_sources[raw_source_id];
        if audio.channels.is_empty() || audio.channels.len() > 2 {
            return Err(format!(
                "audio channel {channel_id} source has {} channels; only mono and stereo are supported by this renderer",
                audio.channels.len()
            ));
        }
        let mut source_bounds =
            sample_source_bounds_at_project_tempo(clip, audio, *clip_index, ppq, base_tempo_bpm)?;
        let clip_envelope = options
            .clip_envelopes
            .iter()
            .find(|envelope| {
                envelope.arrangement_id == options.arrangement_id
                    && clip.clip_id == Some(envelope.clip_id)
            })
            .copied()
            .unwrap_or(PlaylistAudioClipEnvelope {
                arrangement_id: options.arrangement_id,
                clip_id: clip.clip_id.unwrap_or_default(),
                gain_centidb: 0,
                fade_in_ms: 0,
                fade_out_ms: 0,
                fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
                fade_in_tension: 0,
                fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
                fade_out_tension: 0,
                stretch_to_clip_duration: false,
                stretch_pitch_cents: 0,
                pan_override: None,
                reverse_override: None,
            });
        if !clip_envelope.gain_centidb_is_supported()
            || clip_envelope.fade_in_ms > 600_000
            || clip_envelope.fade_out_ms > 600_000
            || !(-100..=100).contains(&clip_envelope.fade_in_tension)
            || !(-100..=100).contains(&clip_envelope.fade_out_tension)
            || !(-1_200..=1_200).contains(&clip_envelope.stretch_pitch_cents)
            || clip_envelope
                .pan_override
                .is_some_and(|pan| !(-100..=100).contains(&pan))
        {
            return Err(format!(
                "Playlist clip {clip_index} gain, fade, pan, or pitch value is outside the supported range"
            ));
        }
        let source_dimensions = AudioSourceDimensions {
            frames: source_bounds.end - source_bounds.start,
            channels: audio.channels.len(),
            sample_rate: audio.sample_rate,
        };
        let stretch_plan = if clip_envelope.stretch_to_clip_duration {
            let channel_pitch_raw = stretch_settings.map_or(0, |settings| settings.1);
            let mode_raw = stretch_settings.map_or(0, |settings| settings.3);
            playlist_instance_audio_clip_stretch_plan(
                mode_raw,
                channel_pitch_raw,
                &tempo_map,
                clip.position_ticks,
                clip.length_ticks,
                source_dimensions,
                *clip_index,
            )
            .map(Some)?
        } else {
            channel_audio_clip_stretch_plan(
                *stretch_settings,
                &tempo_map,
                ppq,
                clip.position_ticks,
                source_dimensions,
                *clip_index,
            )?
        };
        let mut source_id = raw_source_id;
        let instance_pitch_offset_cents =
            *pitch_offset_cents + f64::from(clip_envelope.stretch_pitch_cents);
        let mut effective_pitch_ratio = audio_clip_pitch_ratio(instance_pitch_offset_cents);
        match stretch_plan {
            Some(AudioClipStretchPlan::Resample {
                playback_rate_ratio,
            }) => effective_pitch_ratio *= playback_rate_ratio,
            Some(AudioClipStretchPlan::DurationPreserving {
                target_duration_frames,
                pitch_raw,
                mode_raw,
            }) => {
                let duration_preserving_pitch_raw = audio_clip_duration_preserving_pitch_cents(
                    instance_pitch_offset_cents,
                    pitch_raw,
                    *clip_index,
                )?;
                effective_pitch_ratio = 1.0;
                let cache_key = ProcessedAudioCacheKey {
                    path: path.clone(),
                    source_bounds,
                    target_duration_frames,
                    pitch_raw: duration_preserving_pitch_raw,
                    mode_raw,
                };
                source_id = if let Some(source_id) = processed_source_ids.get(&cache_key) {
                    *source_id
                } else {
                    check_cancelled(cancelled)?;
                    let source_frames = source_bounds.end - source_bounds.start;
                    let stretch_ratio = target_duration_frames as f32 / source_frames as f32;
                    let processed_audio = process_audio_range_for_playback(
                        &audio_sources[raw_source_id],
                        source_bounds.start..source_bounds.end,
                        duration_preserving_pitch_raw as f32 / 100.0,
                        stretch_ratio,
                    )
                    .map_err(|error| {
                        format!(
                            "could not prepare stretch settings for Playlist clip {clip_index}: {error}"
                        )
                    })?;
                    check_cancelled(cancelled)?;
                    let bytes = decoded_audio_bytes(&processed_audio)?;
                    cached_source_bytes = cached_source_bytes
                        .checked_add(bytes)
                        .filter(|total| *total <= MAX_SOURCE_CACHE_BYTES)
                        .ok_or_else(|| {
                            format!(
                                "audio sources and stretched audio exceed the {} MiB render cache limit",
                                MAX_SOURCE_CACHE_BYTES / (1024 * 1024)
                            )
                        })?;
                    let source_id = audio_sources.len();
                    audio_sources.push(processed_audio);
                    processed_source_ids.insert(cache_key, source_id);
                    source_id
                };
                source_bounds = SampleBounds {
                    start: 0,
                    end: audio_sources[source_id].frame_count(),
                };
            }
            None => {}
        }
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
            audio_sources[source_id].sample_rate,
            options.sample_rate,
            effective_pitch_ratio,
        )?;
        let timeline_clip_frames = tempo_map.frames_between_ticks(
            absolute_start_tick,
            absolute_end_tick,
            options.sample_rate,
        )?;
        let duration_frames = source_duration_frames.min(timeline_clip_frames);
        let fade_frames = |milliseconds: u32| {
            u64::from(milliseconds).saturating_mul(u64::from(options.sample_rate)) / 1_000
        };
        let (envelope_gain, fade_in_frames, fade_out_frames) = (
            clip_envelope.linear_gain(),
            fade_frames(clip_envelope.fade_in_ms).min(duration_frames),
            fade_frames(clip_envelope.fade_out_ms).min(duration_frames),
        );
        let end_frame = start_frame
            .checked_add(duration_frames)
            .ok_or_else(|| "render timeline length overflow".to_owned())?;
        output_frames = output_frames.max(end_frame);
        let live_mixer_source_control = (*mixer_track).and_then(|track| {
            let route_plan = live_source_routes_by_track
                .entry(track)
                .or_insert_with(|| {
                    mixer_route_audibility.live_source_route_plan_for_channel(Some(track))
                })
                .clone()?;
            Some(LiveMixerSourceControl::new(route_plan, options.sample_rate))
        });
        clips.push(PreparedClip {
            clip_index: *clip_index,
            playlist_track_id: clip.playlist_track_id(),
            channel_id: *channel_id,
            mixer_track: *mixer_track,
            path: path.clone(),
            source_id,
            start_frame,
            source_bounds,
            reverse: clip_envelope.reverse_override.unwrap_or(*reverse),
            pitch_ratio: effective_pitch_ratio,
            duration_frames,
            gain: *gain,
            envelope_gain,
            fade_in_frames,
            fade_out_frames,
            fade_in_curve: clip_envelope.fade_in_curve,
            fade_in_tension: clip_envelope.fade_in_tension,
            fade_out_curve: clip_envelope.fade_out_curve,
            fade_out_tension: clip_envelope.fade_out_tension,
            pan: clip_envelope
                .pan_override
                .map_or(*pan, |pan| f32::from(pan) / 100.0),
            pan_law,
            mixer_transform: mixer_route_plan.output_transform,
            live_mixer_source_control,
            mixer_meter_transforms: mixer_route_plan.insert_meter_transforms.clone(),
            mixer_effect_input_transforms: mixer_route_plan.insert_effect_input_transforms.clone(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
        });
    }

    Ok(PreparedAudioClipRender {
        summary: AudioClipRenderSummary {
            frames: output_frames,
            sample_rate: options.sample_rate,
            clips_rendered: clips.len(),
            clips_skipped_unsupported_scale,
            source_files: source_paths.len(),
        },
        resampling_quality: options.resampling_quality,
        source_paths,
        audio_sources,
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

pub struct PlaylistPatternNote<'a> {
    pub note: &'a PatternNote,
    pub target_channel_id: u16,
    pub target_gain: f32,
    pub target_pan_raw: i32,
    pub start_tick: u64,
    pub stop_tick: Option<u64>,
    pub clip_index: usize,
}

pub struct PlaylistPatternSchedule<'a> {
    pub notes: Vec<PlaylistPatternNote<'a>>,
    pub clips_skipped_unsupported_scale: usize,
}

#[derive(Clone, Copy)]
pub struct PlaylistTrackFilter<'a> {
    disabled_track_ids: &'a BTreeSet<u32>,
    soloed_track_range: Option<(u32, u32)>,
}

impl<'a> PlaylistTrackFilter<'a> {
    pub const fn new(
        disabled_track_ids: &'a BTreeSet<u32>,
        soloed_track_range: Option<(u32, u32)>,
    ) -> Self {
        Self {
            disabled_track_ids,
            soloed_track_range,
        }
    }

    pub fn clip_is_muted(self, clip: &PlaylistClip) -> bool {
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
pub struct PlaylistPatternScheduleOptions {
    pub ppq: u16,
    pub global_swing_mix_raw: u8,
    pub play_truncated_notes_in_clips: bool,
}

pub fn schedule_playlist_pattern_notes<'a>(
    patterns: &'a [Pattern],
    arrangement: &Arrangement,
    track_filter: PlaylistTrackFilter<'_>,
    options: PlaylistPatternScheduleOptions,
    channel_swing_mix_raw: impl FnMut(u16) -> u16,
    mut resolve_targets: impl FnMut(u16, u64, u16) -> Vec<u16>,
) -> Result<PlaylistPatternSchedule<'a>, String> {
    schedule_playlist_pattern_notes_with_adjustments(
        patterns,
        arrangement,
        track_filter,
        options,
        channel_swing_mix_raw,
        |channel_id, seed, key| {
            resolve_targets(channel_id, seed, key)
                .into_iter()
                .map(|target_channel_id| (target_channel_id, 1.0, 0))
                .collect()
        },
    )
}

pub fn schedule_playlist_pattern_notes_with_adjustments<'a>(
    patterns: &'a [Pattern],
    arrangement: &Arrangement,
    track_filter: PlaylistTrackFilter<'_>,
    options: PlaylistPatternScheduleOptions,
    mut channel_swing_mix_raw: impl FnMut(u16) -> u16,
    mut resolve_targets: impl FnMut(u16, u64, u16) -> Vec<(u16, f32, i32)>,
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
                for (target_channel_id, target_gain, target_pan_raw) in
                    resolve_targets(note.channel_id, note_seed, note.key)
                {
                    if schedule.notes.len() >= MAX_SCHEDULED_PLAYLIST_NOTES {
                        return Err(format!(
                            "Playlist expands to more than {MAX_SCHEDULED_PLAYLIST_NOTES} pattern note events"
                        ));
                    }
                    schedule.notes.push(PlaylistPatternNote {
                        note,
                        target_channel_id,
                        target_gain,
                        target_pan_raw,
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
    tempo_map: PlaylistTempoMap,
    pan_law: PanLaw,
    notes: Vec<ScheduledSamplerNote>,
    sources_by_channel: HashMap<u16, SamplerVoiceSource>,
    mixer_transforms_by_channel: HashMap<u16, MixerSourceRouteTransform>,
    live_mixer_source_routes_by_channel: HashMap<u16, MixerLiveSourceRoutePlan>,
    mixer_meter_transforms_by_channel: HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>,
    mixer_effect_input_transforms_by_channel: HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>,
    pattern_clips_rendered: usize,
    pattern_clips_skipped_unsupported_scale: usize,
    source_paths: std::collections::BTreeSet<PathBuf>,
    unresolved_clip_indices: BTreeSet<usize>,
}

fn prepare_sampler_arrangement(
    document: &FlpDocument,
    project_path: &Path,
    options: PlaylistRenderOptions,
    include_muted_mixer_sources: bool,
    vst3_volume_expression_channels: &BTreeSet<u16>,
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
    let fast_declick_for_cut_groups = document
        .project_settings()
        .is_some_and(|settings| settings.fast_declick_for_cut_groups);
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
    let mut mixer_transforms_by_channel = HashMap::<u16, MixerSourceRouteTransform>::new();
    let mut live_mixer_source_routes_by_channel = HashMap::<u16, MixerLiveSourceRoutePlan>::new();
    let mut mixer_meter_transforms_by_channel =
        HashMap::<u16, Vec<(usize, MixerSourceRouteTransform)>>::new();
    let mut mixer_effect_input_transforms_by_channel =
        HashMap::<u16, Vec<(usize, MixerSourceRouteTransform)>>::new();
    for layer in options.clap_audio_layers.iter() {
        let Some(channel) = channels_by_id.get(&layer.channel_id) else {
            continue;
        };
        let route = channel.mixer_track();
        let statically_audible = mixer_route_audibility.allows_channel(route);
        let live_audible =
            include_muted_mixer_sources && mixer_route_audibility.allows_live_source_channel(route);
        if !statically_audible && !live_audible {
            mixer_transforms_by_channel.insert(layer.channel_id, MixerSourceRouteTransform::zero());
            continue;
        }
        let route_plan = mixer_route_audibility.source_route_plan_for_channel(route);
        mixer_transforms_by_channel.insert(
            layer.channel_id,
            if statically_audible {
                route_plan.output_transform
            } else {
                MixerSourceRouteTransform::zero()
            },
        );
        if statically_audible {
            mixer_meter_transforms_by_channel
                .insert(layer.channel_id, route_plan.insert_meter_transforms);
            mixer_effect_input_transforms_by_channel
                .insert(layer.channel_id, route_plan.insert_effect_input_transforms);
        }
        if let Some(live_route_plan) =
            mixer_route_audibility.live_source_route_plan_for_channel(route)
        {
            live_mixer_source_routes_by_channel.insert(layer.channel_id, live_route_plan);
        }
    }
    let channel_router = ChannelNoteRouter::new(channels.clone());
    let clap_audio_channels = options
        .clap_audio_layers
        .iter()
        .map(|layer| layer.channel_id)
        .collect::<BTreeSet<_>>();
    let schedule = schedule_playlist_pattern_notes_with_adjustments(
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
            let has_clap_target = !clap_audio_channels.is_empty()
                && channel_router
                    .targets_for_note(channel_id, seed, key)
                    .iter()
                    .any(|target_channel_id| clap_audio_channels.contains(target_channel_id));
            let targets = if has_clap_target {
                channel_router.targets_for_note_with_clap_audio_layers(
                    channel_id,
                    seed,
                    key,
                    &clap_audio_channels,
                )
            } else {
                channel_router.targets_for_note_with_mixed_crossfade(
                    channel_id,
                    seed,
                    key,
                    vst3_volume_expression_channels,
                )
            };
            targets
                .into_iter()
                .filter(|(target_channel_id, _, _)| {
                    channels_by_id
                        .get(target_channel_id)
                        .is_some_and(|channel| {
                            channel.kind() == Some(0)
                                && channel.enabled() != Some(false)
                                && if include_muted_mixer_sources {
                                    mixer_route_audibility
                                        .allows_live_source_channel(channel.mixer_track())
                                } else {
                                    mixer_route_audibility.allows_channel(channel.mixer_track())
                                }
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
            tempo_map: tempo_map.clone(),
            pan_law,
            notes: Vec::new(),
            sources_by_channel: HashMap::new(),
            mixer_transforms_by_channel,
            live_mixer_source_routes_by_channel,
            mixer_meter_transforms_by_channel,
            mixer_effect_input_transforms_by_channel,
            pattern_clips_rendered: 0,
            pattern_clips_skipped_unsupported_scale: schedule.clips_skipped_unsupported_scale,
            source_paths: std::collections::BTreeSet::new(),
            unresolved_clip_indices: BTreeSet::new(),
        });
    }

    let resolver = SamplePathResolver::new(project_path);
    let mut decoded_by_path = HashMap::<PathBuf, Arc<DecodedAudio>>::new();
    let mut faded_audio_by_settings =
        HashMap::<(PathBuf, Option<u16>, Option<u16>), Arc<DecodedAudio>>::new();
    let mut sources_by_channel = HashMap::<u16, SamplerVoiceSource>::new();
    // Store each channel's tick-zero Resample rate, then offset scheduled notes from it to
    // match the tempo at their own start ticks.
    let mut resample_pitch_adjustment_by_channel = HashMap::<u16, f64>::new();
    let mut duration_stretch_plan_by_channel =
        HashMap::<u16, SamplerDurationPreservingStretchPlan>::new();
    let mut unresolved_sample_channels = std::collections::BTreeSet::new();
    let mut unresolved_clip_indices = BTreeSet::new();
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
                unresolved_clip_indices.insert(placed.clip_index);
                skipped_unresolved += 1;
                continue;
            };
            let resolved_path = match resolver.resolve(sample_path) {
                Ok(path) => path,
                Err(_) => {
                    unresolved_sample_channels.insert(placed.target_channel_id);
                    unresolved_clip_indices.insert(placed.clip_index);
                    skipped_unresolved += 1;
                    continue;
                }
            };
            let sample_metadata = sample_sampler_metadata(&resolved_path).unwrap_or_default();
            let loop_points = channel
                .sampler_uses_loop_points()
                .then_some(sample_metadata.loop_points)
                .flatten();
            let release_loop_points = channel
                .sampler_uses_loop_points()
                .then_some(sample_metadata.release_loop_points)
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
                decoded_by_path.insert(resolved_path.clone(), Arc::clone(&audio));
                audio
            };
            let fade_in_raw = supported_sampler_fade_raw(channel.sampler_fade_in_raw());
            let fade_out_raw = supported_sampler_fade_raw(channel.sampler_fade_out_raw());
            let audio = if fade_in_raw.is_none() && fade_out_raw.is_none() {
                audio
            } else {
                let key = (resolved_path.clone(), fade_in_raw, fade_out_raw);
                if let Some(audio) = faded_audio_by_settings.get(&key) {
                    Arc::clone(audio)
                } else {
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
                    let faded_audio = sampler_precompute_fades(&audio, fade_in_raw, fade_out_raw);
                    faded_audio_by_settings.insert(key, Arc::clone(&faded_audio));
                    faded_audio
                }
            };
            let prepared_stretch_source = prepare_sampler_stretch_source(
                channel,
                audio,
                loop_points,
                release_loop_points,
                ppq,
                tempo_map.tempo_bpm_at_tick(placed.start_tick),
                &mut cached_source_bytes,
            )?;
            if let Some(plan) = prepared_stretch_source.duration_stretch_plan {
                duration_stretch_plan_by_channel.insert(placed.target_channel_id, plan);
            }
            let audio = prepared_stretch_source.audio;
            let loop_bounds = validated_sampler_loop_bounds(
                prepared_stretch_source.loop_points,
                audio.frame_count(),
            );
            let release_loop_bounds = validated_sampler_loop_bounds(
                prepared_stretch_source.release_loop_points,
                audio.frame_count(),
            );
            let sampler_stretch = sampler_resample_stretch_pitch_adjustment(
                channel,
                audio.frame_count(),
                audio.sample_rate,
                ppq,
                tempo_map.tempo_bpm_at_tick(0),
            )?;
            if let Some((_, pitch_adjustment_cents)) = sampler_stretch {
                resample_pitch_adjustment_by_channel
                    .insert(placed.target_channel_id, pitch_adjustment_cents);
            }
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            let volume_envelope = sampler_volume_envelope_config(channel, options.sample_rate);
            source_entry.insert(SamplerVoiceSource {
                audio,
                gain: gain
                    * sampler_metadata_gain(sample_metadata.gain_db)
                    * if volume_envelope.is_some() {
                        1.0
                    } else {
                        sampler_volume_envelope_sustain_gain(channel)
                    },
                pan,
                reverse: channel.sample_reversed(),
                volume_envelope_enabled: channel.sampler_volume_envelope_enabled().unwrap_or(false),
                volume_envelope,
                max_polyphony_raw: channel.sampler_max_polyphony_raw(),
                mono_enabled: channel.sampler_mono_enabled().unwrap_or(false),
                portamento_enabled: channel.sampler_portamento_enabled().unwrap_or(false),
                polyphony_slide_raw: channel.sampler_polyphony_slide_raw(),
                channel_cut_groups: channel.channel_cut_groups(),
                fast_declick_for_cut_groups,
                key_range: sample_metadata.key_range,
                velocity_range: sample_metadata.velocity_range,
                root_key: resolve_sampler_channel_root_key(
                    channel,
                    sample_metadata.root_key,
                    options.read_sample_root_note,
                ),
                fine_tune_cents: sample_metadata.fine_tune_cents.unwrap_or_default()
                    + f64::from(channel.keyboard_fine_tune_cents().unwrap_or_default())
                    + f64::from(channel.master_pitch_render_offset_cents(
                        document.metadata().master_pitch_cents(),
                    ))
                    + sampler_stretch.map_or(0.0, |(_, cents)| cents),
                loop_bounds,
                ping_pong_loop: sample_metadata
                    .ping_pong_loop
                    .unwrap_or_else(|| channel.sampler_ping_pong_loop_enabled()),
                backward_loop: sample_metadata.backward_loop.unwrap_or(false),
                release_loop_bounds,
                release_ping_pong_loop: sample_metadata.release_ping_pong_loop.unwrap_or(false),
            });
            let route_plan =
                mixer_route_audibility.source_route_plan_for_channel(channel.mixer_track());
            mixer_transforms_by_channel
                .insert(placed.target_channel_id, route_plan.output_transform);
            if let Some(live_route_plan) =
                mixer_route_audibility.live_source_route_plan_for_channel(channel.mixer_track())
            {
                live_mixer_source_routes_by_channel
                    .insert(placed.target_channel_id, live_route_plan);
            }
            mixer_meter_transforms_by_channel
                .insert(placed.target_channel_id, route_plan.insert_meter_transforms);
            mixer_effect_input_transforms_by_channel.insert(
                placed.target_channel_id,
                route_plan.insert_effect_input_transforms,
            );
        }
        let Some(source) = sources_by_channel.get(&placed.target_channel_id) else {
            continue;
        };
        if let Some(plan) = duration_stretch_plan_by_channel.get(&placed.target_channel_id) {
            validate_sampler_duration_stretch_tempo(
                *plan,
                placed.target_channel_id,
                &tempo_map,
                placed.start_tick,
                placed.stop_tick,
                source.loop_bounds.is_some(),
            )?;
        }
        let playlist_track_id = arrangement
            .clips
            .get(placed.clip_index)
            .and_then(PlaylistClip::playlist_track_id);
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
        if !note.is_slide_note() && !source.accepts_note(note_key, note.velocity) {
            continue;
        }
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
        let slide_duration_ticks =
            sampler_polyphony_slide_ticks(channel.sampler_polyphony_slide_raw(), ppq);
        let slide_end_tick = placed.start_tick as f64 + slide_duration_ticks;
        let slide_end_frame =
            tempo_map.frame_at_fractional_tick(slide_end_tick, options.sample_rate)?;
        let slide_duration_frames = slide_end_frame.saturating_sub(start_frame);
        let mut fine_pitch_offset_cents = note.fine_pitch_offset_cents();
        if !note.is_slide_note()
            && let Some(base_pitch_adjustment_cents) =
                resample_pitch_adjustment_by_channel.get(&placed.target_channel_id)
        {
            let Some((target_ticks, note_pitch_adjustment_cents)) =
                sampler_resample_stretch_pitch_adjustment(
                    channel,
                    source.audio.frame_count(),
                    source.audio.sample_rate,
                    ppq,
                    tempo_map.tempo_bpm_at_tick(placed.start_tick),
                )?
            else {
                return Err(format!(
                    "Sampler channel {} Resample settings changed while preparing its Playlist notes",
                    placed.target_channel_id
                ));
            };
            let start_tick_f64 = placed.start_tick as f64;
            if tempo_map.has_tempo_change_between(start_tick_f64, start_tick_f64 + target_ticks)
                || (source.loop_bounds.is_some()
                    && placed.stop_tick.is_some_and(|stop_tick| {
                        tempo_map.has_tempo_change_between(
                            start_tick_f64 + target_ticks,
                            stop_tick as f64,
                        )
                    }))
            {
                return Err(format!(
                    "Sampler channel {} uses Resample time stretching across Playlist tempo automation, which this renderer cannot follow",
                    placed.target_channel_id
                ));
            }
            fine_pitch_offset_cents = sampler_note_fine_pitch_with_delta(
                fine_pitch_offset_cents,
                note_pitch_adjustment_cents - base_pitch_adjustment_cents,
                placed.target_channel_id,
            )?;
        }
        if note.is_slide_note() {
            rendered_pattern_clip_indices.insert(placed.clip_index);
            notes.push(ScheduledSamplerNote {
                clip_index: placed.clip_index,
                playlist_track_id,
                start_tick: placed.start_tick,
                stop_tick: placed.stop_tick,
                start_frame,
                stop_frame,
                channel_id: placed.target_channel_id,
                key: note_key,
                midi_channel: note.midi_channel,
                slide_note: true,
                slide_duration_frames,
                velocity: note.velocity,
                pan: note.pan,
                fine_pitch_offset_cents,
                layer_gain: placed.target_gain,
                layer_pan_raw: placed.target_pan_raw,
            });
            continue;
        }
        let natural_duration = (source.audio.frame_count() as f64
            / sampler_source_step(
                source.audio.sample_rate,
                options.sample_rate,
                note_key,
                source.root_key,
                source.fine_tune_cents + f64::from(fine_pitch_offset_cents),
            ))
        .ceil();
        if !natural_duration.is_finite()
            || natural_duration < 1.0
            || natural_duration > u64::MAX as f64
        {
            return Err("Sampler sample duration is outside the renderable range".to_owned());
        }
        let natural_end_frame = start_frame
            .checked_add(natural_duration as u64)
            .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?;
        let voice_end_frame = match stop_frame {
            Some(stop_frame) if source.volume_envelope_enabled => stop_frame,
            Some(stop_frame) if source.loop_bounds.is_some() => stop_frame
                .checked_add(natural_duration as u64)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
            Some(stop_frame) => natural_end_frame.max(stop_frame),
            None => natural_end_frame,
        };
        let tail_frames = if source.volume_envelope_enabled && stop_frame.is_some() {
            sampler_envelope_release_frames(source.volume_envelope, release_frames as usize) as u64
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
            clip_index: placed.clip_index,
            playlist_track_id,
            start_tick: placed.start_tick,
            stop_tick: placed.stop_tick,
            start_frame,
            stop_frame,
            channel_id: placed.target_channel_id,
            key: note_key,
            midi_channel: note.midi_channel,
            slide_note: false,
            slide_duration_frames,
            velocity: note.velocity,
            pan: note.pan,
            fine_pitch_offset_cents,
            layer_gain: placed.target_gain,
            layer_pan_raw: placed.target_pan_raw,
        });
    }
    notes.sort_by_key(|note| (note.start_frame, note.slide_note, note.channel_id, note.key));
    output_frames = output_frames.max(sampler_voice_render_end_frame(
        &notes,
        &sources_by_channel,
        options.sampler_voice_limit,
        release_frames as usize,
        options.sample_rate,
        options.resampling_quality,
        pan_law,
    )?);
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
        tempo_map: tempo_map.clone(),
        pan_law,
        notes,
        sources_by_channel,
        mixer_transforms_by_channel,
        live_mixer_source_routes_by_channel,
        mixer_meter_transforms_by_channel,
        mixer_effect_input_transforms_by_channel,
        pattern_clips_rendered: rendered_pattern_clip_indices.len(),
        pattern_clips_skipped_unsupported_scale: schedule.clips_skipped_unsupported_scale,
        source_paths: decoded_by_path.keys().cloned().collect(),
        unresolved_clip_indices,
    })
}

struct PreparedSamplerPattern {
    summary: SamplerPatternRenderSummary,
    pan_law: PanLaw,
    notes: Vec<ScheduledSamplerNote>,
    sources_by_channel: HashMap<u16, SamplerVoiceSource>,
    mixer_transforms_by_channel: HashMap<u16, MixerSourceRouteTransform>,
    mixer_meter_transforms_by_channel: HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>,
    mixer_effect_input_transforms_by_channel: HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>,
    live_mixer_source_routes_by_channel: HashMap<u16, MixerLiveSourceRoutePlan>,
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
    volume_envelope_enabled: bool,
    volume_envelope: Option<SamplerVolumeEnvelope>,
    max_polyphony_raw: Option<u32>,
    mono_enabled: bool,
    portamento_enabled: bool,
    polyphony_slide_raw: Option<u32>,
    channel_cut_groups: Option<(u16, u16)>,
    fast_declick_for_cut_groups: bool,
    key_range: Option<(u8, u8)>,
    velocity_range: Option<(u8, u8)>,
    root_key: u16,
    fine_tune_cents: f64,
    loop_bounds: Option<SampleBounds>,
    ping_pong_loop: bool,
    backward_loop: bool,
    release_loop_bounds: Option<SampleBounds>,
    release_ping_pong_loop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SamplerVolumeEnvelope {
    predelay_frames: u64,
    attack_frames: u64,
    hold_frames: u64,
    decay_frames: u64,
    release_frames: usize,
    sustain_level: f32,
    amount: f32,
}

impl SamplerVolumeEnvelope {
    fn level_at_frame(self, note_start_frame: u64, frame: u64) -> f32 {
        let elapsed = frame.saturating_sub(note_start_frame);
        if elapsed < self.predelay_frames {
            return 0.0;
        }
        let elapsed = elapsed - self.predelay_frames;
        if self.attack_frames > 0 && elapsed < self.attack_frames {
            return elapsed as f32 / self.attack_frames as f32;
        }
        let elapsed = elapsed.saturating_sub(self.attack_frames);
        if elapsed < self.hold_frames {
            return 1.0;
        }
        let elapsed = elapsed - self.hold_frames;
        if self.decay_frames > 0 && elapsed < self.decay_frames {
            let progress = elapsed as f32 / self.decay_frames as f32;
            return 1.0 + (self.sustain_level - 1.0) * progress;
        }
        self.sustain_level
    }

    fn gain_at_level(self, level: f32) -> f32 {
        (1.0 + self.amount * (level - 1.0)).clamp(0.0, 2.0)
    }

    fn gain_at_frame(self, note_start_frame: u64, frame: u64) -> f32 {
        self.gain_at_level(self.level_at_frame(note_start_frame, frame))
    }
}

impl SamplerVoiceSource {
    fn accepts_note(&self, key: u16, velocity: u8) -> bool {
        let Ok(key) = u8::try_from(key) else {
            return false;
        };
        let velocity = velocity.min(127);
        let key_is_in_range = match self.key_range {
            Some((low, high)) => (low..=high).contains(&key),
            None => true,
        };
        let velocity_is_in_range = match self.velocity_range {
            Some((low, high)) => (low..=high).contains(&velocity),
            None => true,
        };
        key_is_in_range && velocity_is_in_range
    }
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
    clip_index: usize,
    playlist_track_id: Option<u32>,
    start_tick: u64,
    stop_tick: Option<u64>,
    start_frame: u64,
    stop_frame: Option<u64>,
    channel_id: u16,
    key: u16,
    midi_channel: u8,
    slide_note: bool,
    slide_duration_frames: u64,
    velocity: u8,
    pan: u8,
    fine_pitch_offset_cents: i16,
    layer_gain: f32,
    layer_pan_raw: i32,
}

#[derive(Clone)]
struct PerformanceSamplerClip {
    track_id: u32,
    source_start_tick: u32,
    length_ticks: u64,
    notes: Vec<PerformanceSamplerNote>,
}

#[derive(Clone, Copy)]
struct PerformanceSamplerNote {
    relative_start_tick: u64,
    relative_stop_tick: Option<u64>,
    note: ScheduledSamplerNote,
}

struct ActivePerformanceSamplerClip {
    clip_index: usize,
    start_frame: u64,
    next_loop_tick: u64,
    initial_offset_ticks: u64,
    first_loop_end_tick: u64,
    loop_period_ticks: u64,
    first_pass_scheduled: bool,
}

#[derive(Clone)]
struct PerformanceAudioClip {
    track_id: u32,
    source_start_tick: u32,
    length_ticks: u64,
    source_duration_frames: u64,
    template: PreparedClip,
}

#[derive(Clone)]
struct PerformanceVst3Clip {
    track_id: u32,
    source_start_tick: u32,
    length_ticks: u64,
    notes: Vec<PerformanceVst3Note>,
    parameter_automation_by_channel: BTreeMap<u16, Vec<Vst3PlaylistParameterAutomation>>,
}

#[derive(Clone, Copy)]
struct PerformanceVst3Note {
    relative_start_tick: u64,
    relative_stop_tick: u64,
    channel_id: u16,
    key: u16,
    velocity: u8,
    midi_channel: u8,
    fine_pitch_offset_cents: i16,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
    release: u8,
    layer_gain: f32,
    layer_pan_raw: i32,
}

struct ActivePerformanceAudioClip {
    clip_index: usize,
    start_frame: u64,
    next_loop_tick: u64,
    initial_offset_ticks: u64,
    first_loop_end_tick: u64,
    loop_period_ticks: u64,
    first_pass_scheduled: bool,
}

struct ActivePerformanceVst3Clip {
    clip_index: usize,
    start_frame: u64,
    next_loop_tick: u64,
    initial_offset_ticks: u64,
    first_loop_end_tick: u64,
    loop_period_ticks: u64,
    first_pass_scheduled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformanceMotionReleaseStep {
    Launch(usize),
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformanceMotionPassAction {
    Loop,
    Advance,
    Stop,
}

struct PlaylistPerformanceSources<'a> {
    sampler_clip_indices: &'a BTreeSet<usize>,
    audio_clip_indices: &'a BTreeSet<usize>,
    vst3_clip_indices: &'a BTreeSet<usize>,
    vst3_channel_ids: &'a BTreeSet<u16>,
    vst3_volume_expression_channel_ids: &'a BTreeSet<u16>,
    pattern_automation: &'a BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    sampler: &'a PreparedSamplerArrangement,
    audio: &'a PreparedAudioClipRender,
}

struct PlaylistPerformanceRuntime {
    performance_zone_end_tick: u32,
    start_frame: u64,
    sample_rate: u32,
    tempo_map: PlaylistTempoMap,
    launches: Receiver<PlaylistPerformanceCommand>,
    mixer_routing_updates: Receiver<FlpDocument>,
    latency_updates: Receiver<BTreeSet<u64>>,
    latency_reports: Sender<Vst3LatencyReport>,
    live_status: Arc<Mutex<PlaylistPerformanceLiveStatus>>,
    current_document: FlpDocument,
    pending_commands: Vec<PlaylistPerformanceCommand>,
    queued_by_track: BTreeMap<u32, VecDeque<PlaylistPerformanceLaunch>>,
    motion_by_track: HashMap<u32, Option<PlaylistTrackMotion>>,
    motion_clip_order_by_track: HashMap<u32, Vec<usize>>,
    motion_group_clip_by_index: HashMap<usize, usize>,
    press_by_track: HashMap<u32, Option<PlaylistTrackPress>>,
    position_sync_by_track: HashMap<u32, Option<PlaylistTrackSync>>,
    ppq: u16,
    initial_time_signature: Option<(u8, u8)>,
    signature_changes: Vec<(u32, u8, u8)>,
    clips_by_index: HashMap<usize, PerformanceSamplerClip>,
    audio_clips_by_index: HashMap<usize, PerformanceAudioClip>,
    vst3_clips_by_index: HashMap<usize, PerformanceVst3Clip>,
    active_by_track: BTreeMap<u32, ActivePerformanceSamplerClip>,
    active_audio_by_track: BTreeMap<u32, ActivePerformanceAudioClip>,
    active_vst3_by_track: BTreeMap<u32, ActivePerformanceVst3Clip>,
    active_audio_playbacks: Vec<PreparedClip>,
}

struct PlaylistPerformanceControls {
    launches: Receiver<PlaylistPerformanceCommand>,
    mixer_routing_updates: Receiver<FlpDocument>,
    latency_updates: Receiver<BTreeSet<u64>>,
    latency_reports: Sender<Vst3LatencyReport>,
    live_status: Arc<Mutex<PlaylistPerformanceLiveStatus>>,
}

impl PlaylistPerformanceRuntime {
    fn new(
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        performance_zone_end_tick: u32,
        start_frame: u64,
        controls: PlaylistPerformanceControls,
        sources: PlaylistPerformanceSources<'_>,
    ) -> Result<Self, String> {
        let arrangement = document
            .arrangements()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|arrangement| arrangement.id == options.arrangement_id)
            .ok_or_else(|| format!("arrangement {} was not found", options.arrangement_id))?;
        let unresolved_performance_clips = sources
            .sampler
            .unresolved_clip_indices
            .intersection(sources.sampler_clip_indices)
            .copied()
            .collect::<Vec<_>>();
        if !unresolved_performance_clips.is_empty() {
            return Err(format!(
                "Performance Mode cannot launch Pattern Clip(s) {} because one or more Sampler samples could not be resolved",
                unresolved_performance_clips
                    .iter()
                    .map(|index| (index + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut clips_by_index = HashMap::new();
        for &clip_index in sources.sampler_clip_indices {
            let clip = arrangement.clips.get(clip_index).ok_or_else(|| {
                format!("Performance Pattern Clip {} disappeared", clip_index + 1)
            })?;
            let length_ticks = u64::from(clip.length_ticks);
            if length_ticks == 0 {
                return Err(format!(
                    "Performance Pattern Clip {} has no timeline length",
                    clip_index + 1
                ));
            }
            let track_id = clip.playlist_track_id().ok_or_else(|| {
                format!(
                    "Performance Pattern Clip {} has no Playlist track",
                    clip_index + 1
                )
            })?;
            let source_start_tick = u64::from(clip.position_ticks);
            let notes = sources
                .sampler
                .notes
                .iter()
                .filter(|note| note.clip_index == clip_index)
                .map(|note| PerformanceSamplerNote {
                    relative_start_tick: note.start_tick.saturating_sub(source_start_tick),
                    relative_stop_tick: note
                        .stop_tick
                        .map(|tick| tick.saturating_sub(source_start_tick)),
                    note: *note,
                })
                .collect();
            clips_by_index.insert(
                clip_index,
                PerformanceSamplerClip {
                    track_id,
                    source_start_tick: clip.position_ticks,
                    length_ticks,
                    notes,
                },
            );
        }
        let mut audio_clips_by_index = HashMap::new();
        for &clip_index in sources.audio_clip_indices {
            let clip = arrangement
                .clips
                .get(clip_index)
                .ok_or_else(|| format!("Performance Audio Clip {} disappeared", clip_index + 1))?;
            let length_ticks = u64::from(clip.length_ticks);
            if length_ticks == 0 {
                return Err(format!(
                    "Performance Audio Clip {} has no timeline length",
                    clip_index + 1
                ));
            }
            let track_id = clip.playlist_track_id().ok_or_else(|| {
                format!(
                    "Performance Audio Clip {} has no Playlist track",
                    clip_index + 1
                )
            })?;
            let template = sources
                .audio
                .clips
                .iter()
                .find(|prepared| prepared.clip_index == clip_index)
                .ok_or_else(|| {
                    format!(
                        "Performance Audio Clip {} could not be prepared for playback",
                        clip_index + 1
                    )
                })?
                .clone();
            let source = sources
                .audio
                .audio_sources
                .get(template.source_id)
                .ok_or_else(|| {
                    format!(
                        "Performance Audio Clip {} from {} has no decoded sample",
                        clip_index + 1,
                        template.path.display()
                    )
                })?;
            let source_duration_frames = source_duration_to_frames(
                template.source_bounds.end - template.source_bounds.start,
                source.sample_rate,
                options.sample_rate,
                template.pitch_ratio,
            )?;
            audio_clips_by_index.insert(
                clip_index,
                PerformanceAudioClip {
                    track_id,
                    source_start_tick: clip.position_ticks,
                    length_ticks,
                    source_duration_frames,
                    template,
                },
            );
        }
        let mut vst3_clips_by_index = HashMap::new();
        if !sources.vst3_clip_indices.is_empty() {
            let patterns = document.patterns().map_err(|error| error.to_string())?;
            let channels = document.channels();
            let channel_router = ChannelNoteRouter::new(channels.clone());
            let channels_by_id = channels
                .iter()
                .map(|channel| (channel.id(), channel))
                .collect::<HashMap<_, _>>();
            let disabled_track_ids = document
                .playlist_tracks()
                .into_iter()
                .filter(|track| track.enabled == Some(false))
                .map(|track| track.id)
                .collect::<BTreeSet<_>>();
            let track_filter =
                PlaylistTrackFilter::new(&disabled_track_ids, options.soloed_playlist_track_range);
            let mixer_route_audibility = MixerRouteAudibility::from_document(document);
            let schedule = schedule_playlist_pattern_notes_with_adjustments(
                &patterns,
                &arrangement,
                track_filter,
                PlaylistPatternScheduleOptions {
                    ppq: document.header().ppq(),
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
                |channel_id, note_seed, key| {
                    channel_router
                        .targets_for_note_with_mixed_crossfade(
                            channel_id,
                            note_seed,
                            key,
                            sources.vst3_volume_expression_channel_ids,
                        )
                        .into_iter()
                        .filter(|(target_channel_id, _, _)| {
                            sources.vst3_channel_ids.contains(target_channel_id)
                        })
                        .filter(|(target_channel_id, _, _)| {
                            channels_by_id
                                .get(target_channel_id)
                                .is_some_and(|channel| {
                                    channel.enabled() != Some(false)
                                        && channel.plugin_identifier().is_some()
                                        && mixer_route_audibility
                                            .allows_live_source_channel(channel.mixer_track())
                                })
                        })
                        .collect()
                },
            )?;
            let mut notes_by_clip = HashMap::<usize, Vec<PerformanceVst3Note>>::new();
            for placed in schedule
                .notes
                .iter()
                .filter(|placed| sources.vst3_clip_indices.contains(&placed.clip_index))
            {
                let Some(clip) = arrangement.clips.get(placed.clip_index) else {
                    continue;
                };
                let source_start_tick = u64::from(clip.position_ticks);
                let relative_start_tick = placed.start_tick.saturating_sub(source_start_tick);
                let stop_tick = placed
                    .stop_tick
                    .unwrap_or_else(|| placed.start_tick.saturating_add(1));
                let relative_stop_tick = stop_tick
                    .saturating_sub(source_start_tick)
                    .max(relative_start_tick.saturating_add(1));
                notes_by_clip
                    .entry(placed.clip_index)
                    .or_default()
                    .push(PerformanceVst3Note {
                        mod_y: placed.note.mod_y,
                        relative_start_tick,
                        relative_stop_tick,
                        channel_id: placed.target_channel_id,
                        key: placed.note.key,
                        velocity: placed.note.velocity,
                        midi_channel: placed.note.midi_channel_index(),
                        fine_pitch_offset_cents: placed.note.fine_pitch_offset_cents(),
                        pan: placed.note.pan,
                        mod_x: placed.note.mod_x,
                        release: placed.note.release,
                        layer_gain: placed.target_gain,
                        layer_pan_raw: placed.target_pan_raw,
                    });
            }
            for &clip_index in sources.vst3_clip_indices {
                let clip = arrangement.clips.get(clip_index).ok_or_else(|| {
                    format!(
                        "Performance VST3 Pattern Clip {} disappeared",
                        clip_index + 1
                    )
                })?;
                let track_id = clip.playlist_track_id().ok_or_else(|| {
                    format!(
                        "Performance VST3 Pattern Clip {} has no Playlist track",
                        clip_index + 1
                    )
                })?;
                let notes = notes_by_clip.remove(&clip_index).unwrap_or_default();
                let parameter_automation_by_channel = match clip.target() {
                    PlaylistClipTarget::Pattern { id: pattern_id } => {
                        let pattern = patterns
                            .iter()
                            .find(|pattern| pattern.id == pattern_id)
                            .ok_or_else(|| {
                                format!(
                                    "Performance VST3 Pattern Clip {} has no Pattern",
                                    clip_index + 1
                                )
                            })?;
                        expand_pattern_parameter_automation_for_clip(
                            pattern_id,
                            pattern,
                            clip,
                            document.header().ppq(),
                            document.metadata().time_signature().unwrap_or((4, 4)),
                            sources.pattern_automation,
                        )?
                    }
                    PlaylistClipTarget::Channel { .. } => BTreeMap::new(),
                };
                vst3_clips_by_index.insert(
                    clip_index,
                    PerformanceVst3Clip {
                        track_id,
                        source_start_tick: clip.position_ticks,
                        length_ticks: u64::from(clip.length_ticks),
                        notes,
                        parameter_automation_by_channel,
                    },
                );
            }
        }
        let tracks = document.playlist_tracks_in_arrangement(options.arrangement_id);
        let motion_by_track = tracks
            .iter()
            .map(|track| (track.id, track.motion))
            .collect();
        let ppq = document.header().ppq();
        let initial_time_signature = document.metadata().time_signature();
        let signature_changes = arrangement
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature())
            .filter_map(|marker| {
                Some((
                    marker.position_ticks(),
                    marker.numerator()?.max(1),
                    marker.denominator()?.max(1),
                ))
            })
            .collect::<Vec<_>>();
        let mut marker_group_positions = arrangement
            .time_markers
            .iter()
            .filter(|marker| !marker.is_signature())
            .map(|marker| marker.position_ticks())
            .filter(|position| *position < performance_zone_end_tick)
            .collect::<Vec<_>>();
        marker_group_positions.sort_unstable();
        marker_group_positions.dedup();
        let motion_clips = clips_by_index
            .iter()
            .filter_map(|(&clip_index, clip)| {
                let playlist_clip = arrangement.clips.get(clip_index)?;
                Some((
                    clip.track_id,
                    clip.source_start_tick,
                    playlist_clip.group,
                    clip_index,
                ))
            })
            .chain(
                audio_clips_by_index
                    .iter()
                    .filter_map(|(&clip_index, clip)| {
                        let playlist_clip = arrangement.clips.get(clip_index)?;
                        Some((
                            clip.track_id,
                            clip.source_start_tick,
                            playlist_clip.group,
                            clip_index,
                        ))
                    }),
            )
            .chain(
                vst3_clips_by_index
                    .iter()
                    .filter_map(|(&clip_index, clip)| {
                        let playlist_clip = arrangement.clips.get(clip_index)?;
                        Some((
                            clip.track_id,
                            clip.source_start_tick,
                            playlist_clip.group,
                            clip_index,
                        ))
                    }),
            );
        let (motion_clip_order_by_track, motion_group_clip_by_index) =
            performance_motion_clip_groups(
                motion_clips,
                &marker_group_positions,
                performance_zone_end_tick,
                ppq,
                initial_time_signature,
                &signature_changes,
            );
        let press_by_track = tracks.iter().map(|track| (track.id, track.press)).collect();
        let position_sync_by_track = tracks
            .into_iter()
            .map(|track| (track.id, track.position_sync))
            .collect();
        Ok(Self {
            performance_zone_end_tick,
            start_frame,
            sample_rate: options.sample_rate,
            tempo_map: PlaylistTempoMap::for_arrangement(document, options.arrangement_id)?,
            launches: controls.launches,
            mixer_routing_updates: controls.mixer_routing_updates,
            latency_updates: controls.latency_updates,
            latency_reports: controls.latency_reports,
            live_status: controls.live_status,
            current_document: document.clone(),
            pending_commands: Vec::new(),
            queued_by_track: BTreeMap::new(),
            motion_by_track,
            motion_clip_order_by_track,
            motion_group_clip_by_index,
            press_by_track,
            position_sync_by_track,
            ppq,
            initial_time_signature,
            signature_changes,
            clips_by_index,
            audio_clips_by_index,
            vst3_clips_by_index,
            active_by_track: BTreeMap::new(),
            active_audio_by_track: BTreeMap::new(),
            active_vst3_by_track: BTreeMap::new(),
            active_audio_playbacks: Vec::new(),
        })
    }

    fn active_clip_index_on_track(&self, track_id: u32) -> Option<usize> {
        self.active_by_track
            .get(&track_id)
            .map(|active| active.clip_index)
            .or_else(|| {
                self.active_audio_by_track
                    .get(&track_id)
                    .map(|active| active.clip_index)
            })
            .or_else(|| {
                self.active_vst3_by_track
                    .get(&track_id)
                    .map(|active| active.clip_index)
            })
    }

    fn cancel_pending_track_launches_from(&mut self, track_id: u32, start_frame: u64) {
        let clip_track_ids = self
            .clips_by_index
            .iter()
            .map(|(clip_index, clip)| (*clip_index, clip.track_id))
            .chain(
                self.audio_clips_by_index
                    .iter()
                    .map(|(clip_index, clip)| (*clip_index, clip.track_id)),
            )
            .chain(
                self.vst3_clips_by_index
                    .iter()
                    .map(|(clip_index, clip)| (*clip_index, clip.track_id)),
            )
            .collect::<HashMap<_, _>>();
        self.pending_commands.retain(|command| match *command {
            PlaylistPerformanceCommand::Launch(launch)
            | PlaylistPerformanceCommand::Enqueue(launch) => !performance_launch_cancelled_by_stop(
                clip_track_ids.get(&launch.clip_index).copied(),
                launch.start_frame,
                track_id,
                start_frame,
            ),
            PlaylistPerformanceCommand::AdvanceMotion {
                track_id: launch_track_id,
                start_frame: launch_frame,
                ..
            } => !performance_launch_cancelled_by_stop(
                Some(launch_track_id),
                launch_frame,
                track_id,
                start_frame,
            ),
            PlaylistPerformanceCommand::StopTrack { .. } => true,
        });
    }

    fn apply_pending_mixer_routing_update(
        &mut self,
        sampler_engine: &mut SamplerVoiceEngine<'_>,
        audio_context: &PreparedAudioClipMixContext<'_>,
        vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    ) -> Result<(), String> {
        let mut latest_document = None;
        while let Ok(document) = self.mixer_routing_updates.try_recv() {
            latest_document = Some(document);
        }
        let Some(document) = latest_document else {
            return Ok(());
        };
        self.current_document = document.clone();
        let routing = MixerRouteAudibility::from_document(&document);
        sampler_engine
            .update_live_mixer_routing(&document, audio_context.live_mixer_source_targets);
        refresh_prepared_clip_mixer_routes(
            audio_context.clips,
            &routing,
            audio_context.sample_rate,
        );
        refresh_prepared_clip_mixer_routes(
            &self
                .audio_clips_by_index
                .values()
                .map(|clip| clip.template.clone())
                .collect::<Vec<_>>(),
            &routing,
            audio_context.sample_rate,
        );
        if let Some(processor) = vst3_processor {
            processor
                .apply_live_mixer_routing(&document, audio_context.live_mixer_source_targets)?;
        }
        Ok(())
    }

    fn update(
        &mut self,
        block: std::ops::Range<u64>,
        sampler_engine: &mut SamplerVoiceEngine<'_>,
        audio_context: &PreparedAudioClipMixContext<'_>,
        audio_output: &mut [f32],
        meter_buses: Option<&mut [f32]>,
        mut vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    ) -> Result<(), String> {
        self.apply_pending_mixer_routing_update(
            sampler_engine,
            audio_context,
            vst3_processor.as_deref_mut(),
        )?;
        let mut latency_update_plugin_ids = BTreeSet::new();
        while let Ok(plugin_ids) = self.latency_updates.try_recv() {
            latency_update_plugin_ids.extend(plugin_ids);
        }
        if !latency_update_plugin_ids.is_empty()
            && let Some(processor) = vst3_processor.as_deref_mut()
        {
            let reports = processor.refresh_live_latency_reports(
                &latency_update_plugin_ids,
                &self.current_document,
                audio_context.live_mixer_source_targets,
            )?;
            for report in reports {
                let _ = self.latency_reports.send(report);
            }
        }
        let block_start = block.start;
        let frame_count = usize::try_from(block.end.saturating_sub(block_start))
            .map_err(|_| "Performance Mode audio block is too large".to_owned())?;
        let block_end = block.end;
        while let Ok(launch) = self.launches.try_recv() {
            self.pending_commands.push(launch);
        }
        self.pending_commands
            .sort_by_key(|command| command.start_frame());
        let due_command_count = self
            .pending_commands
            .partition_point(|command| command.start_frame() <= block_end);
        let due_commands = self
            .pending_commands
            .drain(..due_command_count)
            .collect::<Vec<_>>();
        let mut stopped_tracks_in_block = BTreeMap::new();
        for command in due_commands {
            match command {
                PlaylistPerformanceCommand::Launch(launch) => {
                    self.validate_launch(launch)?;
                    let track_id = self.performance_clip_track_id(launch.clip_index)?;
                    if stopped_tracks_in_block
                        .get(&track_id)
                        .is_some_and(|stop_frame| {
                            performance_launch_cancelled_by_stop(
                                Some(track_id),
                                launch.start_frame,
                                track_id,
                                *stop_frame,
                            )
                        })
                    {
                        continue;
                    }
                    self.queued_by_track.remove(&track_id);
                    if performance_latch_should_stop(
                        self.press_by_track.get(&track_id).copied().flatten(),
                        self.active_clip_index_on_track(track_id),
                        launch.clip_index,
                    ) {
                        self.stop_track_at(
                            track_id,
                            launch.start_frame,
                            sampler_engine,
                            vst3_processor.as_deref_mut(),
                        )?;
                    } else {
                        self.activate_launch(
                            launch,
                            sampler_engine,
                            vst3_processor.as_deref_mut(),
                        )?;
                    }
                }
                PlaylistPerformanceCommand::Enqueue(launch) => {
                    self.validate_launch(launch)?;
                    let track_id = self.performance_clip_track_id(launch.clip_index)?;
                    if stopped_tracks_in_block
                        .get(&track_id)
                        .is_some_and(|stop_frame| {
                            performance_launch_cancelled_by_stop(
                                Some(track_id),
                                launch.start_frame,
                                track_id,
                                *stop_frame,
                            )
                        })
                    {
                        continue;
                    }
                    let active_clip_index = self.active_clip_index_on_track(track_id);
                    if performance_latch_should_stop(
                        self.press_by_track.get(&track_id).copied().flatten(),
                        active_clip_index,
                        launch.clip_index,
                    ) {
                        self.queued_by_track.remove(&track_id);
                        self.stop_track_at(
                            track_id,
                            launch.start_frame,
                            sampler_engine,
                            vst3_processor.as_deref_mut(),
                        )?;
                    } else if active_clip_index.is_some() {
                        self.queued_by_track
                            .entry(track_id)
                            .or_default()
                            .push_back(launch);
                    } else {
                        let launch = self.launch_with_position_at(launch, launch.start_tick)?;
                        self.activate_launch(
                            launch,
                            sampler_engine,
                            vst3_processor.as_deref_mut(),
                        )?;
                    }
                }
                PlaylistPerformanceCommand::AdvanceMotion {
                    clip_index,
                    track_id,
                    start_tick,
                    start_frame,
                } => {
                    if start_tick < u64::from(self.performance_zone_end_tick)
                        || start_frame < self.start_frame
                    {
                        return Err(
                            "Performance Mode motion step must start at or after the Start marker"
                                .into(),
                        );
                    }
                    if self.performance_clip_track_id(clip_index)? != track_id {
                        return Err(
                            "Performance Mode motion step clip is on a different track".into()
                        );
                    }
                    if stopped_tracks_in_block
                        .get(&track_id)
                        .is_some_and(|stop_frame| start_frame >= *stop_frame)
                    {
                        continue;
                    }
                    if self.press_by_track.get(&track_id).copied().flatten()
                        != Some(PlaylistTrackPress::HoldMotion)
                    {
                        continue;
                    }
                    let motion_clip_index = self
                        .motion_group_clip_by_index
                        .get(&clip_index)
                        .copied()
                        .unwrap_or(clip_index);
                    let step = performance_motion_release_step_with_seed(
                        self.motion_by_track.get(&track_id).copied().flatten(),
                        motion_clip_index,
                        self.motion_clip_order_by_track
                            .get(&track_id)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                        u64::from(track_id) ^ start_frame.rotate_left(17),
                    );
                    let Some(step) = step else {
                        continue;
                    };
                    self.cancel_pending_track_launches_from(track_id, start_frame);
                    self.queued_by_track.remove(&track_id);
                    stopped_tracks_in_block.insert(track_id, start_frame);
                    let PerformanceMotionReleaseStep::Launch(next_clip_index) = step else {
                        self.stop_track_at(
                            track_id,
                            start_frame,
                            sampler_engine,
                            vst3_processor.as_deref_mut(),
                        )?;
                        continue;
                    };
                    let launch = self.launch_with_position_at(
                        PlaylistPerformanceLaunch {
                            clip_index: next_clip_index,
                            start_tick,
                            start_frame,
                            position_offset_ticks: 0,
                        },
                        start_tick,
                    )?;
                    self.activate_launch(launch, sampler_engine, vst3_processor.as_deref_mut())?;
                }
                PlaylistPerformanceCommand::StopTrack {
                    track_id,
                    start_tick,
                    start_frame,
                } => {
                    if start_tick < u64::from(self.performance_zone_end_tick)
                        || start_frame < self.start_frame
                    {
                        return Err(
                            "Performance Mode stop must start at or after the Start marker".into(),
                        );
                    }
                    self.queued_by_track.remove(&track_id);
                    self.cancel_pending_track_launches_from(track_id, start_frame);
                    self.stop_track_at(
                        track_id,
                        start_frame,
                        sampler_engine,
                        vst3_processor.as_deref_mut(),
                    )?;
                    stopped_tracks_in_block.insert(track_id, start_frame);
                }
            }
        }

        let horizon_frame = block_start
            .saturating_add(frame_count as u64)
            .saturating_add(u64::from(self.sample_rate));
        self.apply_queued_transitions(
            horizon_frame,
            sampler_engine,
            vst3_processor.as_deref_mut(),
        )?;
        self.apply_automatic_motion_transitions(
            block_start,
            horizon_frame,
            sampler_engine,
            vst3_processor.as_deref_mut(),
        )?;
        let motion_group_clip_by_index = &self.motion_group_clip_by_index;
        let mut queued_notes = Vec::new();
        for active in self.active_by_track.values_mut() {
            let clip = self
                .clips_by_index
                .get(&active.clip_index)
                .ok_or_else(|| "active Performance Mode clip disappeared".to_owned())?;
            let motion_clip_index = motion_group_clip_by_index
                .get(&active.clip_index)
                .copied()
                .unwrap_or(active.clip_index);
            let pass_action = performance_motion_pass_action(
                self.motion_by_track.get(&clip.track_id).copied().flatten(),
                self.press_by_track.get(&clip.track_id).copied().flatten(),
                motion_clip_index,
                self.motion_clip_order_by_track
                    .get(&clip.track_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
            let single_pass = pass_action != PerformanceMotionPassAction::Loop;
            if single_pass && active.first_pass_scheduled {
                continue;
            }
            loop {
                let loop_offset_ticks = active.initial_offset_ticks.min(clip.length_ticks);
                let loop_length_ticks = clip.length_ticks.saturating_sub(loop_offset_ticks);
                if loop_length_ticks == 0 {
                    active.initial_offset_ticks = 0;
                    continue;
                }
                let loop_end_tick = active
                    .next_loop_tick
                    .checked_add(loop_length_ticks)
                    .ok_or_else(|| "Performance Mode loop tick overflow".to_owned())?;
                let loop_frame = self
                    .tempo_map
                    .frame_at_tick(active.next_loop_tick, self.sample_rate)?;
                if loop_frame > horizon_frame {
                    break;
                }
                let loop_end_frame = self
                    .tempo_map
                    .frame_at_tick(loop_end_tick, self.sample_rate)?;
                for template in &clip.notes {
                    let (relative_start_tick, relative_stop_tick) = if loop_offset_ticks == 0 {
                        (template.relative_start_tick, template.relative_stop_tick)
                    } else if template.relative_start_tick >= loop_offset_ticks {
                        (
                            template.relative_start_tick - loop_offset_ticks,
                            template
                                .relative_stop_tick
                                .map(|tick| tick.saturating_sub(loop_offset_ticks)),
                        )
                    } else if template
                        .relative_stop_tick
                        .is_some_and(|stop| stop > loop_offset_ticks)
                    {
                        (
                            0,
                            template
                                .relative_stop_tick
                                .map(|tick| tick - loop_offset_ticks),
                        )
                    } else {
                        continue;
                    };
                    if single_pass && relative_start_tick >= loop_length_ticks {
                        continue;
                    }
                    let start_tick = active
                        .next_loop_tick
                        .checked_add(relative_start_tick)
                        .ok_or_else(|| "Performance Mode note start tick overflow".to_owned())?;
                    let mut note = template.note;
                    note.start_tick = start_tick;
                    note.stop_tick = match relative_stop_tick {
                        Some(tick) => {
                            Some(active.next_loop_tick.checked_add(tick).ok_or_else(|| {
                                "Performance Mode note end tick overflow".to_owned()
                            })?)
                        }
                        None => None,
                    };
                    note.start_frame =
                        self.tempo_map.frame_at_tick(start_tick, self.sample_rate)?;
                    note.stop_frame = note
                        .stop_tick
                        .map(|tick| {
                            self.tempo_map
                                .frame_at_tick(tick, self.sample_rate)
                                .map(|frame| frame.max(note.start_frame.saturating_add(1)))
                        })
                        .transpose()?;
                    if single_pass {
                        if note.start_frame >= loop_end_frame {
                            continue;
                        }
                        note.stop_frame = Some(
                            note.stop_frame
                                .unwrap_or(loop_end_frame)
                                .min(loop_end_frame)
                                .max(note.start_frame.saturating_add(1)),
                        );
                    }
                    queued_notes.push(note);
                }
                active.next_loop_tick = loop_end_tick;
                active.initial_offset_ticks = 0;
                if single_pass {
                    active.first_pass_scheduled = true;
                    break;
                }
            }
        }
        for active in self.active_audio_by_track.values_mut() {
            let clip = self
                .audio_clips_by_index
                .get(&active.clip_index)
                .ok_or_else(|| "active Performance Mode Audio Clip disappeared".to_owned())?;
            let motion_clip_index = motion_group_clip_by_index
                .get(&active.clip_index)
                .copied()
                .unwrap_or(active.clip_index);
            let pass_action = performance_motion_pass_action(
                self.motion_by_track.get(&clip.track_id).copied().flatten(),
                self.press_by_track.get(&clip.track_id).copied().flatten(),
                motion_clip_index,
                self.motion_clip_order_by_track
                    .get(&clip.track_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
            let single_pass = pass_action != PerformanceMotionPassAction::Loop;
            if single_pass && active.first_pass_scheduled {
                continue;
            }
            loop {
                let loop_start_tick = active.next_loop_tick;
                let loop_offset_ticks = active.initial_offset_ticks.min(clip.length_ticks);
                let loop_length_ticks = clip.length_ticks.saturating_sub(loop_offset_ticks);
                if loop_length_ticks == 0 {
                    active.initial_offset_ticks = 0;
                    continue;
                }
                let loop_frame = self
                    .tempo_map
                    .frame_at_tick(loop_start_tick, self.sample_rate)?;
                if loop_frame > horizon_frame {
                    break;
                }
                let loop_end_tick = loop_start_tick
                    .checked_add(loop_length_ticks)
                    .ok_or_else(|| "Performance Mode audio loop tick overflow".to_owned())?;
                let loop_end_frame = self
                    .tempo_map
                    .frame_at_tick(loop_end_tick, self.sample_rate)?;
                let timeline_duration_frames = loop_end_frame.saturating_sub(loop_frame);
                let source_offset_output_frames = (u128::from(clip.source_duration_frames)
                    * u128::from(loop_offset_ticks)
                    / u128::from(clip.length_ticks))
                .min(u128::from(u64::MAX)) as u64;
                let duration_frames = clip
                    .source_duration_frames
                    .saturating_sub(source_offset_output_frames)
                    .min(timeline_duration_frames);
                if duration_frames > 0 {
                    let mut playback = clip.template.clone();
                    playback.start_frame = loop_frame;
                    playback.duration_frames = duration_frames;
                    if loop_offset_ticks > 0 {
                        let source_frames = playback
                            .source_bounds
                            .end
                            .saturating_sub(playback.source_bounds.start);
                        let source_offset_frames =
                            (source_frames as u128 * u128::from(loop_offset_ticks)
                                / u128::from(clip.length_ticks))
                            .min(usize::MAX as u128) as usize;
                        playback.source_bounds.start = playback
                            .source_bounds
                            .start
                            .saturating_add(source_offset_frames)
                            .min(playback.source_bounds.end.saturating_sub(1));
                    }
                    self.active_audio_playbacks.push(playback);
                }
                active.next_loop_tick = loop_end_tick;
                active.initial_offset_ticks = 0;
                if single_pass {
                    active.first_pass_scheduled = true;
                    break;
                }
            }
        }
        let mut vst3_notes_by_track_and_channel =
            BTreeMap::<(u32, u16), Vec<PlaylistMidiNote>>::new();
        let mut vst3_automation_by_track_channel_parameter =
            BTreeMap::<(u32, u16, u32), Vec<Vst3PlaylistAutomationPoint>>::new();
        for (&track_id, active) in &mut self.active_vst3_by_track {
            let clip = self
                .vst3_clips_by_index
                .get(&active.clip_index)
                .ok_or_else(|| "active Performance Mode VST3 clip disappeared".to_owned())?;
            let motion_clip_index = self
                .motion_group_clip_by_index
                .get(&active.clip_index)
                .copied()
                .unwrap_or(active.clip_index);
            let pass_action = performance_motion_pass_action(
                self.motion_by_track.get(&track_id).copied().flatten(),
                self.press_by_track.get(&track_id).copied().flatten(),
                motion_clip_index,
                self.motion_clip_order_by_track
                    .get(&track_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
            let single_pass = pass_action != PerformanceMotionPassAction::Loop;
            if single_pass && active.first_pass_scheduled {
                continue;
            }
            loop {
                let loop_start_tick = active.next_loop_tick;
                let loop_offset_ticks = active.initial_offset_ticks.min(clip.length_ticks);
                let loop_length_ticks = clip.length_ticks.saturating_sub(loop_offset_ticks);
                if loop_length_ticks == 0 {
                    active.initial_offset_ticks = 0;
                    continue;
                }
                let loop_end_tick = active
                    .next_loop_tick
                    .checked_add(loop_length_ticks)
                    .ok_or_else(|| "Performance Mode VST3 loop tick overflow".to_owned())?;
                let loop_start_frame = self
                    .tempo_map
                    .frame_at_tick(active.next_loop_tick, self.sample_rate)?;
                if loop_start_frame > horizon_frame {
                    break;
                }
                let loop_end_frame = self
                    .tempo_map
                    .frame_at_tick(loop_end_tick, self.sample_rate)?;
                for template in &clip.notes {
                    let (relative_start_tick, relative_stop_tick) = if loop_offset_ticks == 0 {
                        (template.relative_start_tick, template.relative_stop_tick)
                    } else if template.relative_start_tick >= loop_offset_ticks {
                        (
                            template.relative_start_tick - loop_offset_ticks,
                            template
                                .relative_stop_tick
                                .saturating_sub(loop_offset_ticks),
                        )
                    } else if template.relative_stop_tick > loop_offset_ticks {
                        (0, template.relative_stop_tick - loop_offset_ticks)
                    } else {
                        continue;
                    };
                    if single_pass && relative_start_tick >= loop_length_ticks {
                        continue;
                    }
                    let start_tick = active
                        .next_loop_tick
                        .checked_add(relative_start_tick)
                        .ok_or_else(|| "Performance Mode VST3 note start overflow".to_owned())?;
                    let mut stop_tick = active
                        .next_loop_tick
                        .checked_add(relative_stop_tick)
                        .ok_or_else(|| "Performance Mode VST3 note end overflow".to_owned())?;
                    let start_frame = self.tempo_map.frame_at_tick(start_tick, self.sample_rate)?;
                    if single_pass {
                        if start_frame >= loop_end_frame {
                            continue;
                        }
                        stop_tick = stop_tick
                            .min(loop_end_tick)
                            .max(start_tick.saturating_add(1));
                    }
                    vst3_notes_by_track_and_channel
                        .entry((track_id, template.channel_id))
                        .or_default()
                        .push(PlaylistMidiNote {
                            mod_y: template.mod_y,
                            start_tick,
                            stop_tick: stop_tick.max(start_tick.saturating_add(1)),
                            key: template.key,
                            velocity: template.velocity,
                            midi_channel: template.midi_channel,
                            fine_pitch_offset_cents: template.fine_pitch_offset_cents,
                            pan: template.pan,
                            mod_x: template.mod_x,
                            release: template.release,
                            layer_gain: template.layer_gain,
                            layer_pan_raw: template.layer_pan_raw,
                        });
                }
                for ((channel_id, parameter_id), points) in
                    performance_parameter_automation_points_for_loop(
                        &clip.parameter_automation_by_channel,
                        loop_start_tick,
                        loop_offset_ticks,
                        loop_length_ticks,
                    )?
                {
                    vst3_automation_by_track_channel_parameter
                        .entry((track_id, channel_id, parameter_id))
                        .or_default()
                        .extend(points);
                }
                active.next_loop_tick = loop_end_tick;
                active.initial_offset_ticks = 0;
                if single_pass {
                    active.first_pass_scheduled = true;
                    break;
                }
            }
        }
        if !vst3_notes_by_track_and_channel.is_empty()
            || !vst3_automation_by_track_channel_parameter.is_empty()
        {
            let processor = vst3_processor
                .ok_or_else(|| "VST3 Performance Mode processor is unavailable".to_owned())?;
            for ((track_id, channel_id), notes) in vst3_notes_by_track_and_channel {
                processor.schedule_performance_notes(track_id, channel_id, &notes)?;
            }
            let mut automation_by_track_and_channel =
                BTreeMap::<(u32, u16), Vec<Vst3PlaylistParameterAutomation>>::new();
            for ((track_id, channel_id, parameter_id), mut points) in
                vst3_automation_by_track_channel_parameter
            {
                points.sort_by_key(|point| point.tick);
                let mut unique_points =
                    Vec::<Vst3PlaylistAutomationPoint>::with_capacity(points.len());
                for point in points {
                    if let Some(previous) = unique_points.last_mut()
                        && previous.tick == point.tick
                    {
                        previous.value = point.value;
                    } else {
                        unique_points.push(point);
                    }
                }
                if !unique_points.is_empty() {
                    automation_by_track_and_channel
                        .entry((track_id, channel_id))
                        .or_default()
                        .push(Vst3PlaylistParameterAutomation {
                            parameter_id,
                            points: unique_points,
                        });
                }
            }
            for ((track_id, channel_id), lanes) in automation_by_track_and_channel {
                processor
                    .schedule_performance_parameter_automation(track_id, channel_id, &lanes)?;
            }
        }
        let mut completed_stop_tracks = Vec::new();
        for (&track_id, active) in &self.active_by_track {
            let motion_clip_index = self
                .motion_group_clip_by_index
                .get(&active.clip_index)
                .copied()
                .unwrap_or(active.clip_index);
            if performance_motion_pass_action(
                self.motion_by_track.get(&track_id).copied().flatten(),
                self.press_by_track.get(&track_id).copied().flatten(),
                motion_clip_index,
                self.motion_clip_order_by_track
                    .get(&track_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            ) == PerformanceMotionPassAction::Stop
                && active.first_pass_scheduled
                && self
                    .tempo_map
                    .frame_at_tick(active.first_loop_end_tick, self.sample_rate)?
                    <= block_end
            {
                completed_stop_tracks.push(track_id);
            }
        }
        for (&track_id, active) in &self.active_audio_by_track {
            let motion_clip_index = self
                .motion_group_clip_by_index
                .get(&active.clip_index)
                .copied()
                .unwrap_or(active.clip_index);
            if performance_motion_pass_action(
                self.motion_by_track.get(&track_id).copied().flatten(),
                self.press_by_track.get(&track_id).copied().flatten(),
                motion_clip_index,
                self.motion_clip_order_by_track
                    .get(&track_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            ) == PerformanceMotionPassAction::Stop
                && active.first_pass_scheduled
                && self
                    .tempo_map
                    .frame_at_tick(active.first_loop_end_tick, self.sample_rate)?
                    <= block_end
            {
                completed_stop_tracks.push(track_id);
            }
        }
        for (&track_id, active) in &self.active_vst3_by_track {
            let motion_clip_index = self
                .motion_group_clip_by_index
                .get(&active.clip_index)
                .copied()
                .unwrap_or(active.clip_index);
            if performance_motion_pass_action(
                self.motion_by_track.get(&track_id).copied().flatten(),
                self.press_by_track.get(&track_id).copied().flatten(),
                motion_clip_index,
                self.motion_clip_order_by_track
                    .get(&track_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            ) == PerformanceMotionPassAction::Stop
                && active.first_pass_scheduled
                && self
                    .tempo_map
                    .frame_at_tick(active.first_loop_end_tick, self.sample_rate)?
                    <= block_end
            {
                completed_stop_tracks.push(track_id);
            }
        }
        for track_id in completed_stop_tracks {
            self.active_by_track.remove(&track_id);
            self.active_audio_by_track.remove(&track_id);
            self.active_vst3_by_track.remove(&track_id);
        }
        self.active_audio_playbacks
            .retain(|clip| clip.start_frame.saturating_add(clip.duration_frames) > block_start);
        self.publish_live_status(block_end);
        sampler_engine.queue_live_notes(queued_notes);
        let dynamic_audio_context = PreparedAudioClipMixContext {
            clips: &self.active_audio_playbacks,
            audio_sources: audio_context.audio_sources,
            sample_rate: audio_context.sample_rate,
            resampling_quality: audio_context.resampling_quality,
            cancelled: audio_context.cancelled,
            live_mixer_source_targets: audio_context.live_mixer_source_targets,
        };
        mix_prepared_audio_clip_block(
            &dynamic_audio_context,
            block_start,
            audio_output,
            meter_buses,
            None,
        )?;
        Ok(())
    }

    fn publish_live_status(&self, block_end: u64) {
        let mut status = PlaylistPerformanceLiveStatus::default();
        for active in self.active_by_track.values() {
            let target = if active.start_frame <= block_end {
                &mut status.playing_clip_indices
            } else {
                &mut status.scheduled_clip_indices
            };
            target.insert(active.clip_index);
        }
        for active in self.active_audio_by_track.values() {
            let target = if active.start_frame <= block_end {
                &mut status.playing_clip_indices
            } else {
                &mut status.scheduled_clip_indices
            };
            target.insert(active.clip_index);
        }
        for active in self.active_vst3_by_track.values() {
            let target = if active.start_frame <= block_end {
                &mut status.playing_clip_indices
            } else {
                &mut status.scheduled_clip_indices
            };
            target.insert(active.clip_index);
        }
        for command in &self.pending_commands {
            match *command {
                PlaylistPerformanceCommand::Launch(launch)
                | PlaylistPerformanceCommand::Enqueue(launch) => {
                    status.scheduled_clip_indices.insert(launch.clip_index);
                }
                PlaylistPerformanceCommand::AdvanceMotion { clip_index, .. } => {
                    status.scheduled_clip_indices.insert(clip_index);
                }
                PlaylistPerformanceCommand::StopTrack { .. } => {}
            }
        }
        for queue in self.queued_by_track.values() {
            status
                .scheduled_clip_indices
                .extend(queue.iter().map(|launch| launch.clip_index));
        }
        if let Ok(mut current_status) = self.live_status.lock() {
            *current_status = status;
        }
    }

    fn validate_launch(&self, launch: PlaylistPerformanceLaunch) -> Result<(), String> {
        if launch.start_tick < u64::from(self.performance_zone_end_tick)
            || launch.start_frame < self.start_frame
        {
            return Err("Performance Mode launch must start at or after the Start marker".into());
        }
        self.performance_clip_track_id(launch.clip_index)
            .map(|_| ())
    }

    fn performance_clip_track_id(&self, clip_index: usize) -> Result<u32, String> {
        self.clips_by_index
            .get(&clip_index)
            .map(|clip| clip.track_id)
            .or_else(|| {
                self.audio_clips_by_index
                    .get(&clip_index)
                    .map(|clip| clip.track_id)
            })
            .or_else(|| {
                self.vst3_clips_by_index
                    .get(&clip_index)
                    .map(|clip| clip.track_id)
            })
            .ok_or_else(|| {
                format!(
                    "Playlist Clip {} is not supported in Performance Mode",
                    clip_index + 1
                )
            })
    }

    fn activate_launch(
        &mut self,
        launch: PlaylistPerformanceLaunch,
        sampler_engine: &mut SamplerVoiceEngine<'_>,
        vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    ) -> Result<(), String> {
        let is_sampler_clip = self.clips_by_index.contains_key(&launch.clip_index);
        let is_audio_clip = self.audio_clips_by_index.contains_key(&launch.clip_index);
        let is_vst3_clip = self.vst3_clips_by_index.contains_key(&launch.clip_index);
        let (track_id, length_ticks) =
            if let Some(clip) = self.clips_by_index.get(&launch.clip_index) {
                (clip.track_id, clip.length_ticks)
            } else if let Some(clip) = self.audio_clips_by_index.get(&launch.clip_index) {
                (clip.track_id, clip.length_ticks)
            } else if let Some(clip) = self.vst3_clips_by_index.get(&launch.clip_index) {
                (clip.track_id, clip.length_ticks)
            } else {
                return Err(format!(
                    "Playlist Clip {} is not supported in Performance Mode",
                    launch.clip_index + 1
                ));
            };
        let offset_ticks = launch.position_offset_ticks.min(length_ticks);
        let first_loop_length = if offset_ticks >= length_ticks {
            length_ticks
        } else {
            length_ticks - offset_ticks
        };
        let first_loop_end_tick = launch
            .start_tick
            .checked_add(first_loop_length)
            .ok_or_else(|| "Performance Mode loop tick overflow".to_owned())?;
        self.stop_track_at(track_id, launch.start_frame, sampler_engine, vst3_processor)?;
        if is_sampler_clip {
            self.active_by_track.insert(
                track_id,
                ActivePerformanceSamplerClip {
                    clip_index: launch.clip_index,
                    start_frame: launch.start_frame,
                    next_loop_tick: launch.start_tick,
                    initial_offset_ticks: launch.position_offset_ticks,
                    first_loop_end_tick,
                    loop_period_ticks: length_ticks,
                    first_pass_scheduled: false,
                },
            );
        }
        if is_audio_clip {
            self.active_audio_by_track.insert(
                track_id,
                ActivePerformanceAudioClip {
                    clip_index: launch.clip_index,
                    start_frame: launch.start_frame,
                    next_loop_tick: launch.start_tick,
                    initial_offset_ticks: launch.position_offset_ticks,
                    first_loop_end_tick,
                    loop_period_ticks: length_ticks,
                    first_pass_scheduled: false,
                },
            );
        }
        if is_vst3_clip {
            self.active_vst3_by_track.insert(
                track_id,
                ActivePerformanceVst3Clip {
                    clip_index: launch.clip_index,
                    start_frame: launch.start_frame,
                    next_loop_tick: launch.start_tick,
                    initial_offset_ticks: launch.position_offset_ticks,
                    first_loop_end_tick,
                    loop_period_ticks: length_ticks,
                    first_pass_scheduled: false,
                },
            );
        }
        Ok(())
    }

    fn launch_with_position_at(
        &self,
        mut launch: PlaylistPerformanceLaunch,
        start_tick: u64,
    ) -> Result<PlaylistPerformanceLaunch, String> {
        let (track_id, source_start_tick, length_ticks) =
            if let Some(clip) = self.clips_by_index.get(&launch.clip_index) {
                (clip.track_id, clip.source_start_tick, clip.length_ticks)
            } else if let Some(clip) = self.audio_clips_by_index.get(&launch.clip_index) {
                (clip.track_id, clip.source_start_tick, clip.length_ticks)
            } else if let Some(clip) = self.vst3_clips_by_index.get(&launch.clip_index) {
                (clip.track_id, clip.source_start_tick, clip.length_ticks)
            } else {
                return Err(format!(
                    "Playlist Clip {} is not supported in Performance Mode",
                    launch.clip_index + 1
                ));
            };
        launch.position_offset_ticks = performance_position_offset_ticks(
            start_tick,
            u64::from(source_start_tick),
            length_ticks,
            self.ppq,
            self.initial_time_signature,
            &self.signature_changes,
            self.position_sync_by_track
                .get(&track_id)
                .copied()
                .flatten(),
        );
        launch.start_tick = start_tick;
        launch.start_frame = self.tempo_map.frame_at_tick(start_tick, self.sample_rate)?;
        Ok(launch)
    }

    fn apply_queued_transitions(
        &mut self,
        horizon_frame: u64,
        sampler_engine: &mut SamplerVoiceEngine<'_>,
        mut vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    ) -> Result<(), String> {
        let track_ids = self.queued_by_track.keys().copied().collect::<Vec<_>>();
        for track_id in track_ids {
            while let Some(queued_launch) = self
                .queued_by_track
                .get(&track_id)
                .and_then(|queue| queue.front())
                .copied()
            {
                let active_state = self
                    .active_by_track
                    .get(&track_id)
                    .map(|active| {
                        (
                            active.clip_index,
                            active.first_loop_end_tick,
                            active.loop_period_ticks,
                        )
                    })
                    .or_else(|| {
                        self.active_audio_by_track.get(&track_id).map(|active| {
                            (
                                active.clip_index,
                                active.first_loop_end_tick,
                                active.loop_period_ticks,
                            )
                        })
                    })
                    .or_else(|| {
                        self.active_vst3_by_track.get(&track_id).map(|active| {
                            (
                                active.clip_index,
                                active.first_loop_end_tick,
                                active.loop_period_ticks,
                            )
                        })
                    });
                if let Some((clip_index, first_end_tick, _)) = active_state
                    && queued_launch.start_tick > first_end_tick
                    && performance_motion_pass_action(
                        self.motion_by_track.get(&track_id).copied().flatten(),
                        self.press_by_track.get(&track_id).copied().flatten(),
                        self.motion_group_clip_by_index
                            .get(&clip_index)
                            .copied()
                            .unwrap_or(clip_index),
                        self.motion_clip_order_by_track
                            .get(&track_id)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    ) == PerformanceMotionPassAction::Advance
                {
                    break;
                }
                let boundary_tick = if let Some((_, first_end_tick, period_ticks)) = active_state {
                    performance_motion_transition_tick(
                        self.motion_by_track.get(&track_id).copied().flatten(),
                        first_end_tick,
                        period_ticks,
                        queued_launch.start_tick,
                    )?
                } else {
                    queued_launch.start_tick
                };
                let start_frame = self
                    .tempo_map
                    .frame_at_tick(boundary_tick, self.sample_rate)?;
                if start_frame > horizon_frame {
                    break;
                }
                let mut queued_launch = self
                    .queued_by_track
                    .get_mut(&track_id)
                    .and_then(|queue| queue.pop_front())
                    .expect("the queued launch was present above");
                queued_launch.start_tick = boundary_tick;
                queued_launch.start_frame = start_frame;
                queued_launch = self.launch_with_position_at(queued_launch, boundary_tick)?;
                self.activate_launch(queued_launch, sampler_engine, vst3_processor.as_deref_mut())?;
            }
            if self
                .queued_by_track
                .get(&track_id)
                .is_some_and(|queue| queue.is_empty())
            {
                self.queued_by_track.remove(&track_id);
            }
        }
        Ok(())
    }

    fn apply_automatic_motion_transitions(
        &mut self,
        block_start: u64,
        horizon_frame: u64,
        sampler_engine: &mut SamplerVoiceEngine<'_>,
        mut vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    ) -> Result<(), String> {
        let track_ids = self
            .active_by_track
            .keys()
            .chain(self.active_audio_by_track.keys())
            .chain(self.active_vst3_by_track.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        for track_id in track_ids {
            let active = self
                .active_by_track
                .get(&track_id)
                .map(|active| {
                    (
                        active.clip_index,
                        active.first_loop_end_tick,
                        active.first_pass_scheduled,
                    )
                })
                .or_else(|| {
                    self.active_audio_by_track.get(&track_id).map(|active| {
                        (
                            active.clip_index,
                            active.first_loop_end_tick,
                            active.first_pass_scheduled,
                        )
                    })
                })
                .or_else(|| {
                    self.active_vst3_by_track.get(&track_id).map(|active| {
                        (
                            active.clip_index,
                            active.first_loop_end_tick,
                            active.first_pass_scheduled,
                        )
                    })
                });
            let Some((clip_index, first_end_tick, first_pass_scheduled)) = active else {
                continue;
            };
            let ordered_clip_indices = self
                .motion_clip_order_by_track
                .get(&track_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let motion_clip_index = self
                .motion_group_clip_by_index
                .get(&clip_index)
                .copied()
                .unwrap_or(clip_index);
            let motion = self.motion_by_track.get(&track_id).copied().flatten();
            let press = self.press_by_track.get(&track_id).copied().flatten();
            if !first_pass_scheduled
                || performance_motion_pass_action(
                    motion,
                    press,
                    motion_clip_index,
                    ordered_clip_indices,
                ) != PerformanceMotionPassAction::Advance
            {
                continue;
            }

            let end_frame = self
                .tempo_map
                .frame_at_tick(first_end_tick, self.sample_rate)?;
            if end_frame > horizon_frame {
                continue;
            }
            let start_tick = if end_frame < block_start {
                let block_start_tick = self
                    .tempo_map
                    .tick_at_frame(block_start, self.sample_rate)
                    .ok_or_else(|| {
                        "could not map the start of a Performance Mode audio block".to_owned()
                    })?;
                first_end_tick.max(block_start_tick.ceil().max(0.0) as u64)
            } else {
                first_end_tick
            };
            let start_frame = self.tempo_map.frame_at_tick(start_tick, self.sample_rate)?;
            let step = performance_motion_release_step_with_seed(
                motion,
                motion_clip_index,
                ordered_clip_indices,
                u64::from(track_id) ^ start_frame.rotate_left(17),
            );
            match step {
                Some(PerformanceMotionReleaseStep::Launch(next_clip_index)) => {
                    let launch = self.launch_with_position_at(
                        PlaylistPerformanceLaunch {
                            clip_index: next_clip_index,
                            start_tick,
                            start_frame,
                            position_offset_ticks: 0,
                        },
                        start_tick,
                    )?;
                    self.activate_launch(launch, sampler_engine, vst3_processor.as_deref_mut())?;
                }
                Some(PerformanceMotionReleaseStep::Stop) => {
                    self.stop_track_at(
                        track_id,
                        start_frame,
                        sampler_engine,
                        vst3_processor.as_deref_mut(),
                    )?;
                }
                None => {}
            }
        }
        Ok(())
    }

    fn stop_audio_track_at(&mut self, track_id: u32, stop_frame: u64) {
        self.active_audio_playbacks.retain_mut(|clip| {
            if clip.playlist_track_id != Some(track_id) {
                return true;
            }
            if clip.start_frame >= stop_frame {
                return false;
            }
            if clip.start_frame.saturating_add(clip.duration_frames) > stop_frame {
                clip.duration_frames = stop_frame.saturating_sub(clip.start_frame);
            }
            clip.duration_frames > 0
        });
    }

    fn stop_track_at(
        &mut self,
        track_id: u32,
        stop_frame: u64,
        sampler_engine: &mut SamplerVoiceEngine<'_>,
        vst3_processor: Option<&mut Vst3PlaylistStreamProcessor>,
    ) -> Result<(), String> {
        sampler_engine.remove_queued_track_notes_from(track_id, stop_frame);
        sampler_engine.release_track_at(track_id, stop_frame);
        self.active_by_track.remove(&track_id);
        self.active_audio_by_track.remove(&track_id);
        self.active_vst3_by_track.remove(&track_id);
        self.stop_audio_track_at(track_id, stop_frame);
        if let Some(processor) = vst3_processor {
            processor.stop_performance_track(track_id, stop_frame)?;
        }
        Ok(())
    }
}

fn performance_next_loop_boundary(
    first_end_tick: u64,
    period_ticks: u64,
    earliest_tick: u64,
) -> Result<u64, String> {
    if period_ticks == 0 {
        return Err("Performance Mode clip has no loop length".to_owned());
    }
    if earliest_tick <= first_end_tick {
        return Ok(first_end_tick);
    }
    let loops = earliest_tick
        .saturating_sub(first_end_tick)
        .div_ceil(period_ticks);
    first_end_tick
        .checked_add(loops.saturating_mul(period_ticks))
        .ok_or_else(|| "Performance Mode loop tick overflow".to_owned())
}

fn performance_latch_should_stop(
    press: Option<PlaylistTrackPress>,
    active_clip_index: Option<usize>,
    requested_clip_index: usize,
) -> bool {
    press == Some(PlaylistTrackPress::Latch) && active_clip_index == Some(requested_clip_index)
}

fn performance_parameter_automation_points_for_loop(
    automation_by_channel: &BTreeMap<u16, Vec<Vst3PlaylistParameterAutomation>>,
    loop_start_tick: u64,
    loop_offset_ticks: u64,
    loop_length_ticks: u64,
) -> Result<BTreeMap<(u16, u32), Vec<Vst3PlaylistAutomationPoint>>, String> {
    let mut points_by_lane = BTreeMap::new();
    let loop_end_tick = loop_start_tick
        .checked_add(loop_length_ticks)
        .ok_or_else(|| "Performance Mode automation loop end overflow".to_owned())?;
    for (&channel_id, lanes) in automation_by_channel {
        for lane in lanes {
            if let Some(previous) = lane
                .points
                .iter()
                .filter(|point| point.tick < loop_offset_ticks)
                .max_by_key(|point| point.tick)
            {
                let value = lane
                    .points
                    .iter()
                    .find(|point| point.tick > loop_offset_ticks)
                    .map_or(previous.value, |next| {
                        let span = next.tick.saturating_sub(previous.tick);
                        if span == 0 {
                            previous.value
                        } else {
                            let fraction = loop_offset_ticks.saturating_sub(previous.tick) as f64
                                / span as f64;
                            crate::automation_segment_value_with_tension(
                                previous.value,
                                next.value,
                                fraction,
                                next.tension,
                            )
                        }
                    });
                points_by_lane
                    .entry((channel_id, lane.parameter_id))
                    .or_insert_with(Vec::new)
                    .push(Vst3PlaylistAutomationPoint {
                        tick: loop_start_tick,
                        value,
                        tension: 0.0,
                    });
            }
            for point in lane
                .points
                .iter()
                .filter(|point| point.tick >= loop_offset_ticks)
            {
                let relative_tick = point.tick.saturating_sub(loop_offset_ticks);
                if relative_tick >= loop_length_ticks {
                    continue;
                }
                let absolute_tick = loop_start_tick
                    .checked_add(relative_tick)
                    .ok_or_else(|| "Performance Mode automation position overflow".to_owned())?;
                if absolute_tick >= loop_end_tick {
                    continue;
                }
                points_by_lane
                    .entry((channel_id, lane.parameter_id))
                    .or_insert_with(Vec::new)
                    .push(Vst3PlaylistAutomationPoint {
                        tick: absolute_tick,
                        value: point.value,
                        tension: point.tension,
                    });
            }
        }
    }
    Ok(points_by_lane)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PerformanceMotionGroupKey {
    Marker(u64),
    Clip(u16),
}

fn performance_motion_clip_groups(
    clips: impl IntoIterator<Item = (u32, u32, u16, usize)>,
    marker_group_positions: &[u32],
    performance_zone_end_tick: u32,
    ppq: u16,
    initial_time_signature: Option<(u8, u8)>,
    signature_changes: &[(u32, u8, u8)],
) -> (HashMap<u32, Vec<usize>>, HashMap<usize, usize>) {
    let mut clips_by_track_and_group =
        BTreeMap::<u32, BTreeMap<PerformanceMotionGroupKey, Vec<(u32, usize)>>>::new();
    for (track_id, start_tick, clip_group, clip_index) in clips {
        if start_tick >= performance_zone_end_tick {
            continue;
        }
        let group_key = if clip_group != 0 {
            PerformanceMotionGroupKey::Clip(clip_group)
        } else {
            let group_start_tick = if marker_group_positions.is_empty() {
                performance_signature_and_bar_start(
                    u64::from(start_tick),
                    initial_time_signature,
                    signature_changes,
                    ppq,
                )
                .0
            } else {
                marker_group_positions
                    .iter()
                    .copied()
                    .take_while(|position| *position <= start_tick)
                    .last()
                    .map(u64::from)
                    .unwrap_or(0)
            };
            PerformanceMotionGroupKey::Marker(group_start_tick)
        };
        clips_by_track_and_group
            .entry(track_id)
            .or_default()
            .entry(group_key)
            .or_default()
            .push((start_tick, clip_index));
    }

    let mut ordered_clips_by_track = HashMap::new();
    let mut group_clip_by_index = HashMap::new();
    for (track_id, groups) in clips_by_track_and_group {
        let mut ordered_group_clips = Vec::with_capacity(groups.len());
        for clips in groups.into_values() {
            let mut clips = clips;
            clips.sort_unstable();
            let Some((group_start_tick, group_clip_index)) = clips.first().copied() else {
                continue;
            };
            ordered_group_clips.push((group_start_tick, group_clip_index));
            for (_, clip_index) in clips {
                group_clip_by_index.insert(clip_index, group_clip_index);
            }
        }
        ordered_group_clips.sort_unstable();
        let ordered_clips = ordered_group_clips
            .into_iter()
            .map(|(_, clip_index)| clip_index)
            .collect();
        ordered_clips_by_track.insert(track_id, ordered_clips);
    }
    (ordered_clips_by_track, group_clip_by_index)
}

fn performance_motion_is_one_shot(motion: Option<PlaylistTrackMotion>) -> bool {
    motion == Some(PlaylistTrackMotion::OneShot)
}

fn performance_motion_release_step_with_seed(
    motion: Option<PlaylistTrackMotion>,
    current_clip_index: usize,
    ordered_clip_indices: &[usize],
    seed: u64,
) -> Option<PerformanceMotionReleaseStep> {
    let current_position = ordered_clip_indices
        .iter()
        .position(|clip_index| *clip_index == current_clip_index)?;
    let next_position = current_position + 1;
    match motion? {
        PlaylistTrackMotion::Stay | PlaylistTrackMotion::OneShot => None,
        PlaylistTrackMotion::MarchWrap => Some(PerformanceMotionReleaseStep::Launch(
            ordered_clip_indices[next_position % ordered_clip_indices.len()],
        )),
        PlaylistTrackMotion::MarchStay => ordered_clip_indices
            .get(next_position)
            .copied()
            .map(PerformanceMotionReleaseStep::Launch),
        PlaylistTrackMotion::MarchStop => ordered_clip_indices
            .get(next_position)
            .copied()
            .map(PerformanceMotionReleaseStep::Launch)
            .or(Some(PerformanceMotionReleaseStep::Stop)),
        PlaylistTrackMotion::Random => Some(PerformanceMotionReleaseStep::Launch(
            ordered_clip_indices
                [performance_motion_random_position(seed, ordered_clip_indices.len())],
        )),
        PlaylistTrackMotion::ExclusiveRandom => {
            let alternatives = ordered_clip_indices
                .iter()
                .copied()
                .filter(|clip_index| *clip_index != current_clip_index)
                .collect::<Vec<_>>();
            let candidates = if alternatives.is_empty() {
                ordered_clip_indices
            } else {
                &alternatives
            };
            Some(PerformanceMotionReleaseStep::Launch(
                candidates[performance_motion_random_position(seed, candidates.len())],
            ))
        }
    }
}

fn performance_motion_pass_action(
    motion: Option<PlaylistTrackMotion>,
    press: Option<PlaylistTrackPress>,
    current_clip_index: usize,
    ordered_clip_indices: &[usize],
) -> PerformanceMotionPassAction {
    if performance_motion_is_one_shot(motion) {
        return PerformanceMotionPassAction::Stop;
    }
    if press == Some(PlaylistTrackPress::HoldMotion) {
        return PerformanceMotionPassAction::Loop;
    }
    match performance_motion_release_step_with_seed(
        motion,
        current_clip_index,
        ordered_clip_indices,
        0,
    ) {
        Some(PerformanceMotionReleaseStep::Launch(_)) => PerformanceMotionPassAction::Advance,
        Some(PerformanceMotionReleaseStep::Stop) => PerformanceMotionPassAction::Stop,
        None => PerformanceMotionPassAction::Loop,
    }
}

fn performance_motion_random_position(seed: u64, candidate_count: usize) -> usize {
    let mut value = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((value ^ (value >> 31)) as usize) % candidate_count
}

fn performance_motion_transition_tick(
    motion: Option<PlaylistTrackMotion>,
    first_end_tick: u64,
    period_ticks: u64,
    earliest_tick: u64,
) -> Result<u64, String> {
    if performance_motion_is_one_shot(motion) {
        Ok(earliest_tick.max(first_end_tick))
    } else {
        performance_next_loop_boundary(first_end_tick, period_ticks, earliest_tick)
    }
}

fn performance_launch_cancelled_by_stop(
    launch_track_id: Option<u32>,
    launch_frame: u64,
    track_id: u32,
    stop_frame: u64,
) -> bool {
    launch_track_id == Some(track_id) && launch_frame >= stop_frame
}

fn performance_position_offset_ticks(
    song_tick: u64,
    clip_start_tick: u64,
    clip_length_ticks: u64,
    ppq: u16,
    initial_time_signature: Option<(u8, u8)>,
    signature_changes: &[(u32, u8, u8)],
    position_sync: Option<PlaylistTrackSync>,
) -> u64 {
    if clip_length_ticks == 0 {
        return 0;
    }
    match position_sync {
        None | Some(PlaylistTrackSync::Off) => return 0,
        Some(PlaylistTrackSync::Auto) => {
            return song_tick.saturating_sub(clip_start_tick) % clip_length_ticks;
        }
        _ => {}
    }
    let Some(position_sync) = position_sync else {
        return 0;
    };
    let (bar_start_tick, signature) = performance_signature_and_bar_start(
        song_tick,
        initial_time_signature,
        signature_changes,
        ppq,
    );
    let bar_ticks = performance_bar_ticks(ppq, signature);
    let beat_ticks = performance_bar_ticks(ppq, (1, signature.1));
    let interval_ticks = match position_sync {
        PlaylistTrackSync::Off => return 0,
        PlaylistTrackSync::QuarterBeat => beat_ticks.div_ceil(4),
        PlaylistTrackSync::HalfBeat => beat_ticks.div_ceil(2),
        PlaylistTrackSync::Beat => beat_ticks,
        PlaylistTrackSync::TwoBeats => beat_ticks.saturating_mul(2),
        PlaylistTrackSync::FourBeats => beat_ticks.saturating_mul(4),
        PlaylistTrackSync::Auto => unreachable!("Auto is handled above"),
    }
    .max(1);
    let phase_in_bar = song_tick.saturating_sub(bar_start_tick);
    let grid_count = bar_ticks.div_ceil(interval_ticks).max(1);
    let lower_index = (phase_in_bar / interval_ticks).min(grid_count - 1);
    let upper_index = (lower_index + 1) % grid_count;
    let lower_tick = lower_index.saturating_mul(interval_ticks);
    let upper_tick = upper_index.saturating_mul(interval_ticks);
    let lower_distance = phase_in_bar.abs_diff(lower_tick);
    let upper_distance = if upper_tick >= phase_in_bar {
        upper_tick - phase_in_bar
    } else {
        bar_ticks
            .saturating_sub(phase_in_bar)
            .saturating_add(upper_tick)
    };
    if upper_distance < lower_distance {
        upper_tick % clip_length_ticks
    } else {
        lower_tick % clip_length_ticks
    }
}

fn performance_signature_and_bar_start(
    song_tick: u64,
    initial_time_signature: Option<(u8, u8)>,
    signature_changes: &[(u32, u8, u8)],
    ppq: u16,
) -> (u64, (u8, u8)) {
    let mut signature = initial_time_signature.unwrap_or((4, 4));
    signature.0 = signature.0.max(1);
    signature.1 = signature.1.max(1);
    let mut segment_start_tick = 0u64;
    let mut changes = signature_changes.to_vec();
    changes.sort_by_key(|(position, _, _)| *position);
    let mut change_index = 0;
    while let Some((position, _, _)) = changes.get(change_index).copied() {
        let position = u64::from(position);
        if position > song_tick {
            break;
        }
        segment_start_tick = position;
        while changes
            .get(change_index)
            .is_some_and(|(change_position, _, _)| u64::from(*change_position) == position)
        {
            let (_, numerator, denominator) = changes[change_index];
            signature = (numerator.max(1), denominator.max(1));
            change_index += 1;
        }
    }
    let bar_ticks = performance_bar_ticks(ppq, signature);
    let completed_bars = song_tick.saturating_sub(segment_start_tick) / bar_ticks;
    (
        segment_start_tick.saturating_add(completed_bars.saturating_mul(bar_ticks)),
        signature,
    )
}

fn performance_bar_ticks(ppq: u16, signature: (u8, u8)) -> u64 {
    (u64::from(ppq.max(1)) * u64::from(signature.0.max(1)) * 4 / u64::from(signature.1.max(1)))
        .max(1)
}

fn prepare_sampler_pattern(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    include_muted_mixer_sources: bool,
    cancelled: Option<&AtomicBool>,
) -> Result<PreparedSamplerPattern, String> {
    if include_muted_mixer_sources {
        prepare_sampler_pattern_for_channel_internal(
            document,
            project_path,
            options,
            cancelled,
            SamplerPatternChannelOptions {
                channel_filter: None,
                allow_empty: false,
                include_muted_mixer_sources: true,
                vst3_volume_expression_channels: None,
                clap_audio_channels: None,
            },
        )
    } else {
        prepare_sampler_pattern_for_channel(document, project_path, options, cancelled, None)
    }
}

fn prepare_sampler_pattern_for_channel(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    cancelled: Option<&AtomicBool>,
    channel_filter: Option<&BTreeSet<u16>>,
) -> Result<PreparedSamplerPattern, String> {
    prepare_sampler_pattern_for_channel_internal(
        document,
        project_path,
        options,
        cancelled,
        SamplerPatternChannelOptions {
            channel_filter,
            allow_empty: false,
            include_muted_mixer_sources: false,
            vst3_volume_expression_channels: None,
            clap_audio_channels: None,
        },
    )
}

fn prepare_sampler_pattern_allow_empty(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    include_muted_mixer_sources: bool,
    vst3_volume_expression_channels: &BTreeSet<u16>,
    cancelled: Option<&AtomicBool>,
) -> Result<PreparedSamplerPattern, String> {
    prepare_sampler_pattern_for_channel_internal(
        document,
        project_path,
        options,
        cancelled,
        SamplerPatternChannelOptions {
            channel_filter: None,
            allow_empty: true,
            include_muted_mixer_sources,
            vst3_volume_expression_channels: Some(vst3_volume_expression_channels),
            clap_audio_channels: None,
        },
    )
}

fn prepare_sampler_pattern_for_channel_internal(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    cancelled: Option<&AtomicBool>,
    channel_options: SamplerPatternChannelOptions<'_>,
) -> Result<PreparedSamplerPattern, String> {
    let SamplerPatternChannelOptions {
        channel_filter,
        allow_empty,
        include_muted_mixer_sources,
        vst3_volume_expression_channels,
        clap_audio_channels,
    } = channel_options;
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
    let fast_declick_for_cut_groups = document
        .project_settings()
        .is_some_and(|settings| settings.fast_declick_for_cut_groups);
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
            let has_clap_target = clap_audio_channels.is_some_and(|clap_channels| {
                channel_router
                    .targets_for_note(note.channel_id, note_index as u64, note.key)
                    .iter()
                    .any(|target_channel_id| clap_channels.contains(target_channel_id))
            });
            let routes = if has_clap_target {
                channel_router.targets_for_note_with_clap_audio_layers(
                    note.channel_id,
                    note_index as u64,
                    note.key,
                    clap_audio_channels.expect("a CLAP target requires the CLAP channel set"),
                )
            } else if let Some(volume_expression_channels) = vst3_volume_expression_channels {
                channel_router.targets_for_note_with_mixed_crossfade(
                    note.channel_id,
                    note_index as u64,
                    note.key,
                    volume_expression_channels,
                )
            } else {
                channel_router.targets_for_note_with_layer_adjustments(
                    note.channel_id,
                    note_index as u64,
                    note.key,
                )
            };
            routes
                .into_iter()
                .filter(|(target_channel_id, _, _)| {
                    channels_by_id
                        .get(target_channel_id)
                        .is_some_and(|channel| {
                            channel.kind() == Some(0)
                                && channel.enabled() != Some(false)
                                && if include_muted_mixer_sources {
                                    mixer_route_audibility
                                        .allows_live_source_channel(channel.mixer_track())
                                } else {
                                    mixer_route_audibility.allows_channel(channel.mixer_track())
                                }
                        })
                })
                .map(move |(target_channel_id, target_gain, target_pan_raw)| {
                    (note, target_channel_id, target_gain, target_pan_raw)
                })
        })
        .filter(|(_, target_channel_id, _, _)| {
            channel_filter.is_none_or(|channel_ids| channel_ids.contains(target_channel_id))
        })
        .collect::<Vec<_>>();
    let sampler_note_count = sampler_notes.len();
    if sampler_note_count == 0 && !allow_empty {
        return Err(format!(
            "pattern {} contains no notes on enabled Sampler channels",
            options.pattern_id
        ));
    }
    if sampler_note_count == 0 {
        return Ok(PreparedSamplerPattern {
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
            mixer_effect_input_transforms_by_channel: HashMap::new(),
            live_mixer_source_routes_by_channel: HashMap::new(),
            master_output: MixerMasterOutput::from_document(document),
            voice_limit: options.voice_limit,
            resampling_quality: options.resampling_quality,
        });
    }

    let resolver = SamplePathResolver::new(project_path);
    let mut decoded_by_path = HashMap::<PathBuf, Arc<DecodedAudio>>::new();
    let mut faded_audio_by_settings =
        HashMap::<(PathBuf, Option<u16>, Option<u16>), Arc<DecodedAudio>>::new();
    let mut sources_by_channel = HashMap::<u16, SamplerVoiceSource>::new();
    let mut mixer_transforms_by_channel = HashMap::<u16, MixerSourceRouteTransform>::new();
    let mut mixer_meter_transforms_by_channel =
        HashMap::<u16, Vec<(usize, MixerSourceRouteTransform)>>::new();
    let mut mixer_effect_input_transforms_by_channel =
        HashMap::<u16, Vec<(usize, MixerSourceRouteTransform)>>::new();
    let mut live_mixer_source_routes_by_channel = HashMap::<u16, MixerLiveSourceRoutePlan>::new();
    let mut unresolved_sample_channels = std::collections::BTreeSet::new();
    let mut notes = Vec::with_capacity(sampler_note_count);
    let mut skipped_unresolved = 0usize;
    let mut cached_source_bytes = 0usize;
    let mut output_frames = 0u64;
    let release_frames = (f64::from(options.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as u64;

    for (note, target_channel_id, layer_gain, layer_pan_raw) in sampler_notes {
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
            let sample_metadata = sample_sampler_metadata(&resolved_path).unwrap_or_default();
            let loop_points = channel
                .sampler_uses_loop_points()
                .then_some(sample_metadata.loop_points)
                .flatten();
            let release_loop_points = channel
                .sampler_uses_loop_points()
                .then_some(sample_metadata.release_loop_points)
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
                decoded_by_path.insert(resolved_path.clone(), Arc::clone(&audio));
                audio
            };
            let fade_in_raw = supported_sampler_fade_raw(channel.sampler_fade_in_raw());
            let fade_out_raw = supported_sampler_fade_raw(channel.sampler_fade_out_raw());
            let audio = if fade_in_raw.is_none() && fade_out_raw.is_none() {
                audio
            } else {
                let key = (resolved_path.clone(), fade_in_raw, fade_out_raw);
                if let Some(audio) = faded_audio_by_settings.get(&key) {
                    Arc::clone(audio)
                } else {
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
                    let faded_audio = sampler_precompute_fades(&audio, fade_in_raw, fade_out_raw);
                    faded_audio_by_settings.insert(key, Arc::clone(&faded_audio));
                    faded_audio
                }
            };
            let prepared_stretch_source = prepare_sampler_stretch_source(
                channel,
                audio,
                loop_points,
                release_loop_points,
                ppq,
                tempo_bpm,
                &mut cached_source_bytes,
            )?;
            let audio = prepared_stretch_source.audio;
            let loop_bounds = validated_sampler_loop_bounds(
                prepared_stretch_source.loop_points,
                audio.frame_count(),
            );
            let release_loop_bounds = validated_sampler_loop_bounds(
                prepared_stretch_source.release_loop_points,
                audio.frame_count(),
            );
            let sampler_stretch = sampler_resample_stretch_pitch_adjustment(
                channel,
                audio.frame_count(),
                audio.sample_rate,
                ppq,
                tempo_bpm,
            )?;
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            let volume_envelope = sampler_volume_envelope_config(channel, options.sample_rate);
            source_entry.insert(SamplerVoiceSource {
                audio,
                gain: gain
                    * sampler_metadata_gain(sample_metadata.gain_db)
                    * if volume_envelope.is_some() {
                        1.0
                    } else {
                        sampler_volume_envelope_sustain_gain(channel)
                    },
                pan,
                reverse: channel.sample_reversed(),
                volume_envelope_enabled: channel.sampler_volume_envelope_enabled().unwrap_or(false),
                volume_envelope,
                max_polyphony_raw: channel.sampler_max_polyphony_raw(),
                mono_enabled: channel.sampler_mono_enabled().unwrap_or(false),
                portamento_enabled: channel.sampler_portamento_enabled().unwrap_or(false),
                polyphony_slide_raw: channel.sampler_polyphony_slide_raw(),
                channel_cut_groups: channel.channel_cut_groups(),
                fast_declick_for_cut_groups,
                key_range: sample_metadata.key_range,
                velocity_range: sample_metadata.velocity_range,
                root_key: resolve_sampler_channel_root_key(
                    channel,
                    sample_metadata.root_key,
                    options.read_sample_root_note,
                ),
                fine_tune_cents: sample_metadata.fine_tune_cents.unwrap_or_default()
                    + f64::from(channel.keyboard_fine_tune_cents().unwrap_or_default())
                    + f64::from(channel.master_pitch_render_offset_cents(
                        document.metadata().master_pitch_cents(),
                    ))
                    + sampler_stretch.map_or(0.0, |(_, cents)| cents),
                loop_bounds,
                ping_pong_loop: sample_metadata
                    .ping_pong_loop
                    .unwrap_or_else(|| channel.sampler_ping_pong_loop_enabled()),
                backward_loop: sample_metadata.backward_loop.unwrap_or(false),
                release_loop_bounds,
                release_ping_pong_loop: sample_metadata.release_ping_pong_loop.unwrap_or(false),
            });
            let route_plan =
                mixer_route_audibility.source_route_plan_for_channel(channel.mixer_track());
            mixer_transforms_by_channel.insert(target_channel_id, route_plan.output_transform);
            mixer_meter_transforms_by_channel.insert(
                target_channel_id,
                route_plan.insert_meter_transforms.clone(),
            );
            mixer_effect_input_transforms_by_channel.insert(
                target_channel_id,
                route_plan.insert_effect_input_transforms.clone(),
            );
            if let Some(live_route_plan) =
                mixer_route_audibility.live_source_route_plan_for_channel(channel.mixer_track())
            {
                live_mixer_source_routes_by_channel.insert(target_channel_id, live_route_plan);
            }
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
        if !note.is_slide_note() && !source.accepts_note(note_key, note.velocity) {
            continue;
        }
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
        let slide_duration_frames = sampler_polyphony_slide_frames(
            channel.sampler_polyphony_slide_raw(),
            ppq,
            tempo_bpm,
            options.sample_rate,
        );
        if note.is_slide_note() {
            notes.push(ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame,
                stop_frame,
                channel_id: target_channel_id,
                key: note_key,
                midi_channel: note.midi_channel,
                slide_note: true,
                slide_duration_frames,
                velocity: note.velocity,
                pan: note.pan,
                fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
                layer_gain,
                layer_pan_raw,
            });
            continue;
        }
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
        let natural_end_frame = start_frame
            .checked_add(natural_duration as u64)
            .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?;
        let voice_end_frame = match stop_frame {
            Some(stop_frame) if source.volume_envelope_enabled => stop_frame,
            Some(stop_frame) if source.loop_bounds.is_some() => stop_frame
                .checked_add(natural_duration as u64)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
            Some(stop_frame) => natural_end_frame.max(stop_frame),
            None => natural_end_frame,
        };
        let tail_frames = if source.volume_envelope_enabled && stop_frame.is_some() {
            sampler_envelope_release_frames(source.volume_envelope, release_frames as usize) as u64
        } else {
            0
        };
        output_frames = output_frames.max(
            voice_end_frame
                .checked_add(tail_frames)
                .ok_or_else(|| "Sampler render timeline length overflow".to_owned())?,
        );
        notes.push(ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame,
            stop_frame,
            channel_id: target_channel_id,
            key: note_key,
            midi_channel: note.midi_channel,
            slide_note: false,
            slide_duration_frames,
            velocity: note.velocity,
            pan: note.pan,
            fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
            layer_gain,
            layer_pan_raw,
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
    notes.sort_by_key(|note| (note.start_frame, note.slide_note, note.channel_id, note.key));
    output_frames = output_frames.max(sampler_voice_render_end_frame(
        &notes,
        &sources_by_channel,
        options.voice_limit,
        release_frames as usize,
        options.sample_rate,
        options.resampling_quality,
        pan_law,
    )?);
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
        mixer_meter_transforms_by_channel,
        mixer_effect_input_transforms_by_channel,
        live_mixer_source_routes_by_channel,
        master_output: MixerMasterOutput::from_document(document),
        voice_limit: options.voice_limit,
        resampling_quality: options.resampling_quality,
    })
}

#[derive(Clone)]
struct PerformanceMixerChannelRoute {
    mixer_transform: MixerSourceRouteTransform,
    live_source_route: Option<MixerLiveSourceRoutePlan>,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_effect_input_transforms: Vec<(usize, MixerSourceRouteTransform)>,
}

impl PerformanceMixerChannelRoute {
    fn for_track(routing: &MixerRouteAudibility, mixer_track: Option<i8>) -> Self {
        let plan = routing.source_route_plan_for_channel(mixer_track);
        Self {
            mixer_transform: plan.output_transform,
            live_source_route: routing.live_source_route_plan_for_channel(mixer_track),
            mixer_meter_transforms: plan.insert_meter_transforms,
            mixer_effect_input_transforms: plan.insert_effect_input_transforms,
        }
    }
}

fn refresh_prepared_clip_mixer_routes(
    clips: &[PreparedClip],
    routing: &MixerRouteAudibility,
    sample_rate: u32,
) {
    for clip in clips {
        let route_plan = routing.source_route_plan_for_channel(clip.mixer_track);
        let live_mixer_source_control = routing
            .live_source_route_plan_for_channel(clip.mixer_track)
            .map(|route_plan| LiveMixerSourceControl::new(route_plan, sample_rate));
        clip.mixer_route_override
            .store(Some(Arc::new(PreparedClipMixerRouteOverride {
                mixer_transform: route_plan.output_transform,
                live_mixer_source_control,
                mixer_meter_transforms: route_plan.insert_meter_transforms,
                mixer_effect_input_transforms: route_plan.insert_effect_input_transforms,
            })));
    }
}

struct SamplerVoiceEngine<'a> {
    sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
    notes: &'a [ScheduledSamplerNote],
    tempo_map: Option<&'a PlaylistTempoMap>,
    live_notes: Vec<ScheduledSamplerNote>,
    mixer_transforms_by_channel: Option<&'a HashMap<u16, MixerSourceRouteTransform>>,
    live_mixer_source_routes_by_channel: Option<&'a HashMap<u16, MixerLiveSourceRoutePlan>>,
    live_mixer_source_targets: Option<&'a MixerTrackPeakMeters>,
    mixer_meter_transforms_by_channel:
        Option<&'a HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>>,
    mixer_effect_input_transforms_by_channel:
        Option<&'a HashMap<u16, Vec<(usize, MixerSourceRouteTransform)>>>,
    live_mixer_routes_by_channel: HashMap<u16, PerformanceMixerChannelRoute>,
    pan_law: PanLaw,
    current_frame: u64,
    next_note: usize,
    next_live_note: usize,
    voices: Vec<Option<SamplerVoice>>,
    last_portamento_values_by_group: HashMap<(u16, u8), SamplerPortamentoValues>,
    release_frames: usize,
    output_sample_rate: u32,
    resampling_quality: ResamplingQuality,
    voices_stolen: usize,
    pitch_bend_cents: [i16; 16],
    live_midi_pan: [f32; 16],
    live_midi_channel_volume: [f32; 16],
    live_midi_expression: [f32; 16],
    sustain_down: [bool; 16],
    sostenuto_down: [bool; 16],
}

struct SamplerVoice {
    source: Arc<DecodedAudio>,
    channel_id: u16,
    playlist_track_id: Option<u32>,
    key: u16,
    midi_channel: u8,
    note_fine_pitch_offset_cents: i16,
    input_midi_channel: Option<u8>,
    source_position: f64,
    source_step: f64,
    base_source_step: f64,
    pitch_offset_cents: f64,
    note_slide: Option<SamplerNoteSlide>,
    key_released: bool,
    sostenuto_held: bool,
    release_started: bool,
    volume_envelope_enabled: bool,
    volume_envelope: Option<SamplerVolumeEnvelope>,
    envelope_release_start_frame: Option<u64>,
    envelope_release_start_gain: f32,
    loop_bounds: Option<SampleBounds>,
    ping_pong_loop: bool,
    backward_loop: bool,
    release_loop_bounds: Option<SampleBounds>,
    release_ping_pong_loop: bool,
    stop_frame: Option<u64>,
    started_frame: u64,
    base_gain: f32,
    velocity_gain: f32,
    gain: f32,
    channel_pan: f32,
    note_pan_value: f32,
    left_gain: f32,
    right_gain: f32,
    mixer_transform: MixerSourceRouteTransform,
    live_mixer_source_control: Option<(LiveMixerSourceControl, LiveMixerSourceTargets)>,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_effect_input_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    release_remaining: Option<usize>,
    release_total_frames: Option<usize>,
    release_cosine: bool,
}

#[derive(Clone, Copy)]
struct SamplerNoteSlide {
    start_frame: u64,
    end_frame: u64,
    start_tick: Option<f64>,
    end_tick: Option<f64>,
    start_pitch_cents: f64,
    target_pitch_cents: f64,
    start_velocity_gain: f32,
    target_velocity_gain: f32,
    start_pan_value: f32,
    target_pan_value: f32,
}

#[derive(Clone, Copy)]
struct SamplerPortamentoValues {
    pitch_cents: f64,
    velocity_gain: f32,
    pan_value: f32,
}

fn update_sampler_voice_slide(
    voice: &mut SamplerVoice,
    frame: u64,
    tick: Option<f64>,
    pitch_bend_cents: [i16; 16],
    pan_law: PanLaw,
) {
    if let Some(slide) = voice.note_slide {
        let progress = match (tick, slide.start_tick, slide.end_tick) {
            (Some(tick), Some(start_tick), Some(end_tick)) if end_tick > start_tick => {
                (tick - start_tick) / (end_tick - start_tick)
            }
            _ => {
                if frame >= slide.end_frame {
                    1.0
                } else {
                    frame.saturating_sub(slide.start_frame) as f64
                        / slide.end_frame.saturating_sub(slide.start_frame).max(1) as f64
                }
            }
        }
        .clamp(0.0, 1.0);
        voice.pitch_offset_cents = slide.start_pitch_cents
            + (slide.target_pitch_cents - slide.start_pitch_cents) * progress;
        let progress = progress as f32;
        voice.velocity_gain = slide.start_velocity_gain
            + (slide.target_velocity_gain - slide.start_velocity_gain) * progress;
        voice.gain = voice.base_gain * voice.velocity_gain;
        voice.note_pan_value =
            slide.start_pan_value + (slide.target_pan_value - slide.start_pan_value) * progress;
        let effective_pan = sampler_note_pan_value(voice.channel_pan, voice.note_pan_value);
        (voice.left_gain, voice.right_gain) =
            channel_pan_gains(effective_pan, voice.source.channels.len() == 1, pan_law);
        if frame >= slide.end_frame {
            voice.note_slide = None;
        }
    }
    let pitch_bend = voice
        .input_midi_channel
        .map_or(0, |channel| pitch_bend_cents[usize::from(channel)]);
    set_sampler_voice_source_step(voice, pitch_bend);
}

fn set_sampler_voice_source_step(voice: &mut SamplerVoice, pitch_bend_cents: i16) {
    let slide_ratio = 2.0_f64.powf(voice.pitch_offset_cents / 1_200.0);
    let direction = if voice.source_step == 0.0 {
        voice.base_source_step.signum()
    } else {
        voice.source_step.signum()
    };
    voice.source_step = direction
        * voice.base_source_step.abs()
        * audio_clip_master_pitch_ratio(pitch_bend_cents)
        * slide_ratio;
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
            tempo_map: None,
            live_notes: Vec::new(),
            mixer_transforms_by_channel: None,
            live_mixer_source_routes_by_channel: None,
            live_mixer_source_targets: None,
            mixer_meter_transforms_by_channel: None,
            mixer_effect_input_transforms_by_channel: None,
            live_mixer_routes_by_channel: HashMap::new(),
            pan_law,
            current_frame: 0,
            next_note: 0,
            next_live_note: 0,
            voices: std::iter::repeat_with(|| None).take(voice_limit).collect(),
            last_portamento_values_by_group: HashMap::new(),
            release_frames,
            output_sample_rate,
            resampling_quality,
            voices_stolen: 0,
            pitch_bend_cents: [0; 16],
            live_midi_pan: [64.0; 16],
            live_midi_channel_volume: [1.0; 16],
            live_midi_expression: [1.0; 16],
            sustain_down: [false; 16],
            sostenuto_down: [false; 16],
        }
    }

    fn tick_at_frame(&self, frame: u64) -> Option<f64> {
        self.tempo_map?
            .tick_at_frame(frame, self.output_sample_rate)
    }

    fn slide_tick_bounds(&self, start_frame: u64, end_frame: u64) -> (Option<f64>, Option<f64>) {
        let Some(tempo_map) = self.tempo_map else {
            return (None, None);
        };
        let start_tick = self.tick_at_frame(start_frame);
        let end_tick = self.tick_at_frame(end_frame);
        if start_tick
            .zip(end_tick)
            .is_some_and(|(start, end)| tempo_map.has_tempo_change_between(start, end))
        {
            (start_tick, end_tick)
        } else {
            (None, None)
        }
    }

    fn update_live_mixer_routing(
        &mut self,
        document: &FlpDocument,
        meters: Option<&MixerTrackPeakMeters>,
    ) {
        let routing = MixerRouteAudibility::from_document(document);
        let channels_by_id = document
            .channels()
            .into_iter()
            .map(|channel| (channel.id(), channel))
            .collect::<HashMap<_, _>>();
        self.live_mixer_routes_by_channel = self
            .sources_by_channel
            .keys()
            .filter_map(|channel_id| {
                let channel = channels_by_id.get(channel_id)?;
                Some((
                    *channel_id,
                    PerformanceMixerChannelRoute::for_track(&routing, channel.mixer_track()),
                ))
            })
            .collect();
        for voice in self.voices.iter_mut().flatten() {
            let Some(route) = self.live_mixer_routes_by_channel.get(&voice.channel_id) else {
                continue;
            };
            voice.mixer_transform = route.mixer_transform;
            voice.mixer_meter_transforms = route.mixer_meter_transforms.clone();
            voice.mixer_effect_input_transforms = route.mixer_effect_input_transforms.clone();
            voice.live_mixer_source_control = route.live_source_route.clone().and_then(|plan| {
                let control = LiveMixerSourceControl::new(plan, self.output_sample_rate);
                let targets = control.targets(meters?)?;
                Some((control, targets))
            });
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
        meter_buses: Option<&mut [f32]>,
        meter_stride_samples: usize,
    ) {
        let _ = self.process_block(
            block_start,
            frame_count,
            Some(output),
            meter_buses,
            meter_stride_samples,
            true,
        );
    }

    fn advance_block_without_output(
        &mut self,
        block_start: u64,
        frame_count: usize,
    ) -> Option<u64> {
        self.process_block(block_start, frame_count, None, None, 0, false)
    }

    fn process_block(
        &mut self,
        block_start: u64,
        frame_count: usize,
        mut output: Option<&mut [f32]>,
        mut meter_buses: Option<&mut [f32]>,
        meter_stride_samples: usize,
        render_samples: bool,
    ) -> Option<u64> {
        if let Some(output) = output.as_deref_mut() {
            output.fill(0.0);
        }
        let (mut meter_buses, mut effect_input_buses) = if let Some(buses) = meter_buses.take() {
            let (meter_buses, effect_input_buses) =
                crate::audio::split_mixer_track_buses_mut(buses, meter_stride_samples)
                    .expect("Sampler Mixer buses must match their render block");
            (Some(meter_buses), effect_input_buses)
        } else {
            (None, None)
        };
        let live_midi_channel_volume = self.live_midi_channel_volume;
        let live_midi_expression = self.live_midi_expression;
        let mut last_active_output_frame = None;
        for offset in 0..frame_count {
            let frame = block_start + offset as u64;
            let current_tick = self
                .voices
                .iter()
                .any(|voice| {
                    voice.as_ref().is_some_and(|voice| {
                        voice
                            .note_slide
                            .is_some_and(|slide| slide.start_tick.is_some())
                    })
                })
                .then(|| self.tick_at_frame(frame))
                .flatten();
            self.current_frame = frame;
            for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
                if voice.note_slide.is_some() {
                    update_sampler_voice_slide(
                        voice,
                        frame,
                        current_tick,
                        self.pitch_bend_cents,
                        self.pan_law,
                    );
                }
            }
            while self
                .notes
                .get(self.next_note)
                .is_some_and(|note| note.start_frame <= frame)
            {
                let note = self.notes[self.next_note];
                self.next_note += 1;
                let _ = self.start_voice(note);
            }
            while self
                .live_notes
                .get(self.next_live_note)
                .is_some_and(|note| note.start_frame <= frame)
            {
                let note = self.live_notes[self.next_live_note];
                self.next_live_note += 1;
                let _ = self.start_voice(note);
            }
            let mut output_frame = output
                .as_deref_mut()
                .map(|output| &mut output[offset * 2..offset * 2 + 2]);
            for voice_slot in &mut self.voices {
                let Some(voice) = voice_slot.as_mut() else {
                    continue;
                };
                if !voice.release_started
                    && voice
                        .stop_frame
                        .is_some_and(|stop_frame| frame >= stop_frame)
                {
                    if voice.note_slide.is_some() {
                        update_sampler_voice_slide(
                            voice,
                            frame,
                            current_tick,
                            self.pitch_bend_cents,
                            self.pan_law,
                        );
                    }
                    voice.note_slide = None;
                    begin_sampler_voice_release(voice, frame, self.release_frames);
                }
                let fade = match voice.release_remaining {
                    Some(0) => {
                        *voice_slot = None;
                        continue;
                    }
                    Some(remaining) => {
                        let total = voice
                            .release_total_frames
                            .unwrap_or(self.release_frames)
                            .max(1) as f32;
                        if voice.release_cosine {
                            let progress = 1.0 - remaining as f32 / total;
                            0.5 * (1.0 + (std::f32::consts::PI * progress).cos())
                        } else if voice.volume_envelope.is_some() {
                            1.0
                        } else {
                            remaining as f32 / total
                        }
                    }
                    None => 1.0,
                };
                let source_frames = voice.source.frame_count();
                if voice.source_position < 0.0 || voice.source_position >= source_frames as f64 {
                    *voice_slot = None;
                    continue;
                }
                last_active_output_frame = Some(frame.saturating_add(1));
                if render_samples {
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
                    let envelope_gain = sampler_voice_volume_envelope_gain(voice, frame);
                    let midi_gain = voice.input_midi_channel.map_or(1.0, |channel| {
                        live_midi_expression[usize::from(channel)]
                            * live_midi_channel_volume[usize::from(channel)]
                    });
                    let gain = voice.gain * envelope_gain * midi_gain * fade;
                    let mut routed_frame = [
                        left * gain * voice.left_gain,
                        right * gain * voice.right_gain,
                    ];
                    let live_controls = voice.live_mixer_source_control.as_ref().map(
                        |(source_control, targets)| {
                            let controls = source_control.next_frame_controls(targets);
                            source_control.apply_output_frame(controls, &mut routed_frame);
                            (source_control, controls)
                        },
                    );
                    if live_controls.is_none() {
                        voice.mixer_transform.apply_frame(&mut routed_frame);
                    }
                    let output_frame = output_frame
                        .as_deref_mut()
                        .expect("rendering sampler samples has an output block");
                    output_frame[0] += routed_frame[0];
                    output_frame[1] += routed_frame[1];
                    let source_frame = [
                        left * gain * voice.left_gain,
                        right * gain * voice.right_gain,
                    ];
                    if let Some(meter_buses) = meter_buses.as_deref_mut() {
                        let meter_transforms = live_controls.map_or_else(
                            || voice.mixer_meter_transforms.as_slice(),
                            |(source_control, _)| source_control.insert_meter_transforms(),
                        );
                        for (track, transform) in meter_transforms {
                            debug_assert!(*track < MIXER_TRACK_METER_COUNT);
                            let mut meter_frame = source_frame;
                            if let Some((source_control, controls)) = live_controls {
                                source_control.apply_meter_frame(
                                    controls,
                                    *track,
                                    *transform,
                                    &mut meter_frame,
                                );
                            } else {
                                transform.apply_frame(&mut meter_frame);
                            }
                            let meter_offset = *track * meter_stride_samples + offset * 2;
                            meter_buses[meter_offset] += meter_frame[0];
                            meter_buses[meter_offset + 1] += meter_frame[1];
                        }
                    }
                    if let Some(effect_input_buses) = effect_input_buses.as_deref_mut() {
                        let effect_input_transforms = live_controls.map_or_else(
                            || voice.mixer_effect_input_transforms.as_slice(),
                            |(source_control, _)| source_control.insert_effect_input_transforms(),
                        );
                        for (track, transform) in effect_input_transforms {
                            debug_assert!(*track < MIXER_TRACK_METER_COUNT);
                            let mut effect_input_frame = source_frame;
                            if let Some((source_control, controls)) = live_controls {
                                source_control.apply_effect_input_frame(
                                    controls,
                                    *track,
                                    *transform,
                                    &mut effect_input_frame,
                                );
                            } else {
                                transform.apply_frame(&mut effect_input_frame);
                            }
                            let effect_input_offset = *track * meter_stride_samples + offset * 2;
                            effect_input_buses[effect_input_offset] += effect_input_frame[0];
                            effect_input_buses[effect_input_offset + 1] += effect_input_frame[1];
                        }
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
        self.current_frame = block_start.saturating_add(frame_count as u64);
        last_active_output_frame
    }

    fn start_voice(&mut self, note: ScheduledSamplerNote) -> Option<usize> {
        let source = self.sources_by_channel.get(&note.channel_id)?;
        if note.slide_note {
            self.apply_pitch_slide(note);
            return None;
        }
        if !source.accepts_note(note.key, note.velocity) {
            return None;
        }
        if let Some(cut_group) = source
            .channel_cut_groups
            .map(|(cut_group, _)| cut_group)
            .filter(|cut_group| *cut_group != 0)
        {
            let sources_by_channel = self.sources_by_channel;
            let voices_to_cut = self
                .voices
                .iter()
                .enumerate()
                .filter_map(|(index, voice)| {
                    let voice = voice.as_ref()?;
                    let cut_by = sources_by_channel
                        .get(&voice.channel_id)?
                        .channel_cut_groups?
                        .1;
                    (cut_by == cut_group).then_some(index)
                })
                .collect::<Vec<_>>();
            let fast_declick_frames = (f64::from(self.output_sample_rate) * 0.010)
                .round()
                .max(1.0) as usize;
            for voice_index in voices_to_cut {
                if source.fast_declick_for_cut_groups {
                    if let Some(voice) = self.voices[voice_index].as_mut() {
                        begin_sampler_voice_fast_cut_declick(
                            voice,
                            note.start_frame,
                            fast_declick_frames,
                        );
                    }
                } else {
                    self.voices[voice_index] = None;
                }
            }
        }
        let note_midi_channel = note.midi_channel & 0x0f;
        let note_portamento_enabled =
            source.portamento_enabled != (note.midi_channel & PATTERN_NOTE_PORTA_FLAG != 0);
        let portamento_start = if note_portamento_enabled || source.mono_enabled {
            self.last_portamento_values_by_group
                .get(&(note.channel_id, note_midi_channel))
                .copied()
                .map(|values| (values.pitch_cents, values.velocity_gain, values.pan_value))
        } else {
            None
        };
        if source.mono_enabled {
            for voice in self.voices.iter_mut().flatten().filter(|voice| {
                voice.channel_id == note.channel_id
                    && voice.midi_channel == note_midi_channel
                    && !voice.release_started
            }) {
                begin_sampler_voice_release(voice, note.start_frame, self.release_frames);
            }
        } else if let Some(max_polyphony) = source
            .max_polyphony_raw
            .filter(|max_polyphony| *max_polyphony > 0)
            .and_then(|max_polyphony| usize::try_from(max_polyphony).ok())
        {
            while self
                .voices
                .iter()
                .flatten()
                .filter(|voice| voice.channel_id == note.channel_id)
                .count()
                >= max_polyphony
            {
                let oldest_channel_voice = self
                    .voices
                    .iter()
                    .enumerate()
                    .filter_map(|(index, voice)| {
                        voice.as_ref().and_then(|voice| {
                            (voice.channel_id == note.channel_id)
                                .then_some((index, voice.started_frame))
                        })
                    })
                    .min_by_key(|(index, started_frame)| (*started_frame, *index))
                    .map(|(index, _)| index);
                let Some(oldest_channel_voice) = oldest_channel_voice else {
                    break;
                };
                self.voices[oldest_channel_voice] = None;
                self.voices_stolen += 1;
            }
        }
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
        let note_pan_value = f32::from(note.pan.min(128));
        let channel_pan = (source.pan + note.layer_pan_raw.clamp(-6_400, 6_400) as f32 / 6_400.0)
            .clamp(-1.0, 1.0);
        let effective_pan = sampler_note_pan_value(channel_pan, note_pan_value);
        let (left_gain, right_gain) = channel_pan_gains(
            effective_pan,
            source.audio.channels.len() == 1,
            self.pan_law,
        );
        let velocity_gain = f32::from(note.velocity.min(127)) / 127.0;
        let base_gain = source.gain * note.layer_gain;
        let performance_route = self.live_mixer_routes_by_channel.get(&note.channel_id);
        let live_source_route = performance_route
            .and_then(|route| route.live_source_route.as_ref())
            .or_else(|| {
                self.live_mixer_source_routes_by_channel
                    .and_then(|routes| routes.get(&note.channel_id))
            });
        let live_mixer_source_control = live_source_route.and_then(|route_plan| {
            let control = LiveMixerSourceControl::new(route_plan.clone(), self.output_sample_rate);
            let targets = control.targets(self.live_mixer_source_targets?)?;
            Some((control, targets))
        });
        let mixer_transform = performance_route
            .map(|route| route.mixer_transform)
            .or_else(|| {
                self.mixer_transforms_by_channel
                    .and_then(|transforms| transforms.get(&note.channel_id).copied())
            })
            .unwrap_or_default();
        let mixer_meter_transforms = performance_route
            .map(|route| route.mixer_meter_transforms.clone())
            .or_else(|| {
                self.mixer_meter_transforms_by_channel
                    .and_then(|transforms| transforms.get(&note.channel_id).cloned())
            })
            .unwrap_or_default();
        let mixer_effect_input_transforms = performance_route
            .map(|route| route.mixer_effect_input_transforms.clone())
            .or_else(|| {
                self.mixer_effect_input_transforms_by_channel
                    .and_then(|transforms| transforms.get(&note.channel_id).cloned())
            })
            .unwrap_or_default();
        self.voices[slot_index] = Some(SamplerVoice {
            source: Arc::clone(&source.audio),
            channel_id: note.channel_id,
            playlist_track_id: note.playlist_track_id,
            key: note.key,
            midi_channel: note_midi_channel,
            note_fine_pitch_offset_cents: note.fine_pitch_offset_cents,
            input_midi_channel: None,
            source_position: if source.reverse {
                source.audio.frame_count().saturating_sub(1) as f64
            } else {
                0.0
            },
            source_step,
            base_source_step: source_step,
            pitch_offset_cents: 0.0,
            note_slide: None,
            key_released: false,
            sostenuto_held: false,
            release_started: false,
            volume_envelope_enabled: source.volume_envelope_enabled,
            volume_envelope: source.volume_envelope,
            envelope_release_start_frame: None,
            envelope_release_start_gain: 1.0,
            loop_bounds: source.loop_bounds,
            ping_pong_loop: source.ping_pong_loop,
            backward_loop: source.backward_loop,
            release_loop_bounds: source.release_loop_bounds,
            release_ping_pong_loop: source.release_ping_pong_loop,
            stop_frame: note.stop_frame,
            started_frame: note.start_frame,
            base_gain,
            velocity_gain,
            gain: base_gain * velocity_gain,
            channel_pan,
            note_pan_value,
            left_gain,
            right_gain,
            mixer_transform,
            live_mixer_source_control,
            mixer_meter_transforms,
            mixer_effect_input_transforms,
            release_remaining: None,
            release_total_frames: None,
            release_cosine: false,
        });
        self.last_portamento_values_by_group.insert(
            (note.channel_id, note_midi_channel),
            SamplerPortamentoValues {
                pitch_cents: f64::from(note.key) * 100.0 + f64::from(note.fine_pitch_offset_cents),
                velocity_gain,
                pan_value: note_pan_value,
            },
        );
        if let Some((start_pitch_cents, start_velocity_gain, start_pan_value)) = portamento_start
            && note.slide_duration_frames > 0
        {
            let target_pitch_cents =
                f64::from(note.key) * 100.0 + f64::from(note.fine_pitch_offset_cents);
            let end_frame = note.start_frame.saturating_add(note.slide_duration_frames);
            let (start_tick, end_tick) = self.slide_tick_bounds(note.start_frame, end_frame);
            let current_tick = self.tick_at_frame(note.start_frame);
            if let Some(voice) = self.voices[slot_index].as_mut() {
                voice.note_slide = Some(SamplerNoteSlide {
                    start_frame: note.start_frame,
                    end_frame,
                    start_tick,
                    end_tick,
                    start_pitch_cents: start_pitch_cents - target_pitch_cents,
                    target_pitch_cents: 0.0,
                    start_velocity_gain,
                    target_velocity_gain: velocity_gain,
                    start_pan_value,
                    target_pan_value: note_pan_value,
                });
                update_sampler_voice_slide(
                    voice,
                    note.start_frame,
                    current_tick,
                    self.pitch_bend_cents,
                    self.pan_law,
                );
            }
        }
        Some(slot_index)
    }

    fn apply_pitch_slide(&mut self, note: ScheduledSamplerNote) {
        let Some(end_frame) = note.stop_frame.filter(|end| *end > note.start_frame) else {
            return;
        };
        let (start_tick, end_tick) = self.slide_tick_bounds(note.start_frame, end_frame);
        let current_tick = self.tick_at_frame(note.start_frame);
        let reference_pitch_cents = self
            .voices
            .iter()
            .flatten()
            .filter(|voice| {
                voice.channel_id == note.channel_id
                    && voice.midi_channel == (note.midi_channel & 0x0f)
                    && !voice.key_released
                    && voice
                        .stop_frame
                        .is_none_or(|stop_frame| stop_frame > note.start_frame)
            })
            .map(|voice| {
                f64::from(voice.key) * 100.0
                    + f64::from(voice.note_fine_pitch_offset_cents)
                    + voice.pitch_offset_cents
            })
            .max_by(f64::total_cmp);
        let Some(reference_pitch_cents) = reference_pitch_cents else {
            return;
        };
        let target_pitch_cents =
            f64::from(note.key) * 100.0 + f64::from(note.fine_pitch_offset_cents);
        let pitch_delta_cents = target_pitch_cents - reference_pitch_cents;
        let target_pan_value = f32::from(note.pan.min(128));
        let target_velocity_gain = f32::from(note.velocity.min(127)) / 127.0;
        for voice in self.voices.iter_mut().flatten().filter(|voice| {
            voice.channel_id == note.channel_id
                && voice.midi_channel == (note.midi_channel & 0x0f)
                && !voice.key_released
                && voice
                    .stop_frame
                    .is_none_or(|stop_frame| stop_frame > note.start_frame)
        }) {
            update_sampler_voice_slide(
                voice,
                note.start_frame,
                current_tick,
                self.pitch_bend_cents,
                self.pan_law,
            );
            let start_pitch_cents = voice.pitch_offset_cents;
            voice.note_slide = Some(SamplerNoteSlide {
                start_frame: note.start_frame,
                end_frame,
                start_tick,
                end_tick,
                start_pitch_cents,
                target_pitch_cents: start_pitch_cents + pitch_delta_cents,
                start_velocity_gain: voice.velocity_gain,
                target_velocity_gain,
                start_pan_value: voice.note_pan_value,
                target_pan_value,
            });
            update_sampler_voice_slide(
                voice,
                note.start_frame,
                current_tick,
                self.pitch_bend_cents,
                self.pan_law,
            );
        }
    }

    fn queue_live_notes(&mut self, notes: impl IntoIterator<Item = ScheduledSamplerNote>) {
        self.discard_consumed_live_notes();
        self.live_notes.extend(notes);
        self.live_notes.sort_by_key(|note| {
            (
                note.start_frame,
                note.slide_note,
                note.playlist_track_id,
                note.channel_id,
                note.key,
                note.clip_index,
            )
        });
    }

    fn remove_queued_track_notes_from(&mut self, track_id: u32, start_frame: u64) {
        self.discard_consumed_live_notes();
        self.live_notes.retain(|note| {
            note.playlist_track_id != Some(track_id) || note.start_frame < start_frame
        });
    }

    fn release_track_at(&mut self, track_id: u32, start_frame: u64) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.playlist_track_id == Some(track_id) {
                voice.stop_frame = Some(
                    voice
                        .stop_frame
                        .map_or(start_frame, |stop| stop.min(start_frame)),
                );
            }
        }
    }

    fn discard_consumed_live_notes(&mut self) {
        if self.next_live_note > 0 {
            self.live_notes.drain(..self.next_live_note);
            self.next_live_note = 0;
        }
    }

    fn set_voice_input_midi_channel(&mut self, slot_index: usize, channel: u8) {
        let channel = channel & 0x0F;
        if let Some(voice) = self.voices[slot_index].as_mut() {
            voice.midi_channel = channel;
            voice.input_midi_channel = Some(channel);
            set_sampler_voice_source_step(voice, self.pitch_bend_cents[usize::from(channel)]);
        }
    }

    fn apply_live_pitch_bend(&mut self, channel: u8, cents: i16) {
        let channel = channel & 0x0F;
        self.pitch_bend_cents[usize::from(channel)] = cents;
        for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
            if voice.input_midi_channel == Some(channel) {
                set_sampler_voice_source_step(voice, cents);
            }
        }
    }

    fn live_midi_pan(&self, channel: u8) -> u8 {
        self.live_midi_pan[usize::from(channel & 0x0F)].round() as u8
    }

    fn apply_live_pan(&mut self, channel: u8, value: u8) {
        let channel = channel & 0x0F;
        self.apply_live_pan_position(channel, f32::from(value.min(127)));
    }

    fn apply_live_pan_14bit(&mut self, channel: u8, value: u16) {
        let pan = (f32::from(value.min(16_383)) / 128.0).min(127.0);
        self.apply_live_pan_position(channel, pan);
    }

    fn apply_live_pan_position(&mut self, channel: u8, pan: f32) {
        let channel = channel & 0x0F;
        let pan = pan.clamp(0.0, 127.0);
        self.live_midi_pan[usize::from(channel)] = pan;
        let sources_by_channel = self.sources_by_channel;
        let pan_law = self.pan_law;
        for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
            if voice.input_midi_channel == Some(channel)
                && let Some(source) = sources_by_channel.get(&voice.channel_id)
            {
                voice.note_pan_value = pan;
                let effective_pan = sampler_note_pan_value(voice.channel_pan, voice.note_pan_value);
                (voice.left_gain, voice.right_gain) =
                    channel_pan_gains(effective_pan, source.audio.channels.len() == 1, pan_law);
            }
        }
    }

    fn refresh_live_pan_for_channel(&mut self, channel: u8) {
        let channel = channel & 0x0F;
        let pan = self.live_midi_pan[usize::from(channel)];
        self.apply_live_pan_position(channel, pan);
    }

    fn apply_live_expression(&mut self, channel: u8, value: u8) {
        self.live_midi_expression[usize::from(channel & 0x0F)] = f32::from(value.min(127)) / 127.0;
    }

    fn apply_live_expression_14bit(&mut self, channel: u8, value: u16) {
        self.live_midi_expression[usize::from(channel & 0x0F)] =
            f32::from(value.min(16_383)) / 16_383.0;
    }

    fn apply_live_channel_volume(&mut self, channel: u8, value: u8) {
        self.live_midi_channel_volume[usize::from(channel & 0x0F)] =
            f32::from(value.min(127)) / 127.0;
    }

    fn apply_live_channel_volume_14bit(&mut self, channel: u8, value: u16) {
        self.live_midi_channel_volume[usize::from(channel & 0x0F)] =
            f32::from(value.min(16_383)) / 16_383.0;
    }

    fn release_live_notes(&mut self, channel: u8) {
        let channel = channel & 0x0F;
        let sustain_down = self.sustain_down[usize::from(channel)];
        let release_frames = self.release_frames;
        let current_frame = self.current_frame;
        for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
            if voice.input_midi_channel == Some(channel) && !voice.key_released {
                if !sustain_down && !voice.sostenuto_held {
                    begin_sampler_voice_release(voice, current_frame, release_frames);
                } else {
                    voice.key_released = true;
                }
            }
        }
    }

    fn stop_live_voices(&mut self, channel: u8) {
        let channel = channel & 0x0F;
        for voice in &mut self.voices {
            if voice
                .as_ref()
                .is_some_and(|voice| voice.input_midi_channel == Some(channel))
            {
                *voice = None;
            }
        }
    }

    fn set_live_sustain(&mut self, channel: u8, down: bool) {
        let channel = channel & 0x0F;
        let channel_index = usize::from(channel);
        let was_down = self.sustain_down[channel_index];
        self.sustain_down[channel_index] = down;
        if was_down && !down {
            let current_frame = self.current_frame;
            for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
                if voice.input_midi_channel == Some(channel)
                    && voice.key_released
                    && !voice.sostenuto_held
                    && !voice.release_started
                {
                    begin_sampler_voice_release(voice, current_frame, self.release_frames);
                }
            }
        }
    }

    fn set_live_sostenuto(&mut self, channel: u8, down: bool) {
        let channel = channel & 0x0F;
        let channel_index = usize::from(channel);
        let was_down = self.sostenuto_down[channel_index];
        self.sostenuto_down[channel_index] = down;
        if !was_down && down {
            for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
                if voice.input_midi_channel == Some(channel) && !voice.key_released {
                    voice.sostenuto_held = true;
                }
            }
        } else if was_down && !down {
            let sustain_down = self.sustain_down[channel_index];
            let current_frame = self.current_frame;
            for voice in self.voices.iter_mut().filter_map(Option::as_mut) {
                if voice.input_midi_channel == Some(channel) {
                    voice.sostenuto_held = false;
                    if voice.key_released && !sustain_down && !voice.release_started {
                        begin_sampler_voice_release(voice, current_frame, self.release_frames);
                    }
                }
            }
        }
    }

    fn release_note(&mut self, channel_id: u16, key: u16, midi_channel: u8) {
        let midi_channel = midi_channel & 0x0F;
        let sustain_down = self.sustain_down[usize::from(midi_channel)];
        let current_frame = self.current_frame;
        let voice = self
            .voices
            .iter_mut()
            .filter_map(Option::as_mut)
            .filter(|voice| {
                voice.channel_id == channel_id
                    && voice.key == key
                    && voice.input_midi_channel == Some(midi_channel)
                    && !voice.key_released
            })
            .min_by_key(|voice| voice.started_frame);
        if let Some(voice) = voice {
            if !sustain_down && !voice.sostenuto_held {
                begin_sampler_voice_release(voice, current_frame, self.release_frames);
            } else {
                voice.key_released = true;
            }
        }
    }
}

fn sampler_voice_render_end_frame(
    notes: &[ScheduledSamplerNote],
    sources_by_channel: &HashMap<u16, SamplerVoiceSource>,
    voice_limit: usize,
    release_frames: usize,
    sample_rate: u32,
    resampling_quality: ResamplingQuality,
    pan_law: PanLaw,
) -> Result<u64, String> {
    if !notes.iter().any(|note| note.slide_note) {
        return Ok(0);
    }
    let mut engine = SamplerVoiceEngine::with_resampling_quality_and_pan_law(
        sources_by_channel,
        notes,
        voice_limit,
        release_frames,
        sample_rate,
        resampling_quality,
        pan_law,
    );
    let mut block_start = 0u64;
    let mut last_active_output_frame = 0u64;
    while engine.next_note < notes.len() || engine.voices.iter().any(Option::is_some) {
        if engine.voices.iter().all(Option::is_none) {
            while engine
                .notes
                .get(engine.next_note)
                .is_some_and(|note| note.slide_note)
            {
                engine.next_note += 1;
            }
            let Some(next_note) = engine.notes.get(engine.next_note) else {
                break;
            };
            block_start = block_start.max(next_note.start_frame);
        }
        let frames_until_overflow = u64::MAX - block_start;
        if frames_until_overflow == 0 {
            return Err("Sampler render timeline length overflow".to_owned());
        }
        let frame_count = frames_until_overflow.min(SAMPLER_BLOCK_FRAMES as u64) as usize;
        if let Some(end_frame) = engine.advance_block_without_output(block_start, frame_count) {
            last_active_output_frame = last_active_output_frame.max(end_frame);
        }
        block_start += frame_count as u64;
    }
    Ok(last_active_output_frame)
}

fn sampler_voice_volume_envelope_gain(voice: &SamplerVoice, frame: u64) -> f32 {
    let Some(envelope) = voice.volume_envelope else {
        return 1.0;
    };
    if voice.release_started {
        if voice.release_cosine {
            return voice.envelope_release_start_gain;
        }
        let start_frame = voice.envelope_release_start_frame.unwrap_or(frame);
        let total_frames = voice
            .release_total_frames
            .unwrap_or(envelope.release_frames)
            .max(1);
        let elapsed = frame.saturating_sub(start_frame);
        let progress = if total_frames <= 1 {
            1.0
        } else {
            (elapsed as f32 / (total_frames - 1) as f32).clamp(0.0, 1.0)
        };
        voice.envelope_release_start_gain * (1.0 - progress)
    } else {
        envelope.gain_at_frame(voice.started_frame, frame)
    }
}

fn begin_sampler_voice_release(voice: &mut SamplerVoice, start_frame: u64, release_frames: usize) {
    let envelope_start_gain = sampler_voice_volume_envelope_gain(voice, start_frame);
    voice.key_released = true;
    voice.release_started = true;
    voice.note_slide = None;
    voice.envelope_release_start_frame = Some(start_frame);
    voice.envelope_release_start_gain = envelope_start_gain;
    if voice.volume_envelope_enabled {
        let envelope_release_frames =
            sampler_envelope_release_frames(voice.volume_envelope, release_frames);
        voice.release_remaining = Some(envelope_release_frames);
        voice.release_total_frames = Some(envelope_release_frames);
        voice.release_cosine = false;
    }
}

fn sampler_envelope_release_frames(
    envelope: Option<SamplerVolumeEnvelope>,
    default_release_frames: usize,
) -> usize {
    envelope
        .map(|envelope| envelope.release_frames)
        .filter(|frames| *frames > 0)
        .unwrap_or(default_release_frames)
        .max(1)
}

fn begin_sampler_voice_fast_cut_declick(
    voice: &mut SamplerVoice,
    start_frame: u64,
    fade_frames: usize,
) {
    let envelope_start_gain = sampler_voice_volume_envelope_gain(voice, start_frame);
    voice.key_released = true;
    voice.release_started = true;
    voice.note_slide = None;
    voice.envelope_release_start_frame = Some(start_frame);
    voice.envelope_release_start_gain = envelope_start_gain;
    voice.release_remaining = Some(fade_frames);
    voice.release_total_frames = Some(fade_frames);
    voice.release_cosine = true;
}

fn advance_sampler_voice_position(voice: &mut SamplerVoice) {
    let mut next_position = voice.source_position + voice.source_step;
    let (loop_bounds, ping_pong_loop, backward_loop) =
        if voice.release_started && voice.volume_envelope_enabled {
            (
                voice.release_loop_bounds,
                voice.release_ping_pong_loop,
                false,
            )
        } else if voice.release_started {
            (None, false, false)
        } else {
            (voice.loop_bounds, voice.ping_pong_loop, voice.backward_loop)
        };
    let Some(bounds) = loop_bounds else {
        voice.source_position = next_position;
        return;
    };
    if voice.stop_frame.is_none() && !voice.release_started {
        voice.source_position = next_position;
        return;
    }

    let start = bounds.start as f64;
    let end = bounds.end as f64;
    if backward_loop {
        let high = end - 1.0;
        if voice.source_step > 0.0 && next_position > high {
            next_position = backward_loop_position(high - (next_position - high), bounds);
            voice.source_step = -voice.source_step.abs();
        } else if voice.source_step < 0.0 && next_position < start {
            next_position = backward_loop_position(next_position, bounds);
        }
        voice.source_position = next_position;
        return;
    }

    if !ping_pong_loop {
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

fn backward_loop_position(position: f64, bounds: SampleBounds) -> f64 {
    let start = bounds.start as f64;
    let end = bounds.end as f64;
    let length = end - start;
    if position >= start && position < end {
        return position;
    }
    let distance_from_start = (start - position).rem_euclid(length);
    if distance_from_start == 0.0 {
        end - 1.0
    } else {
        end - distance_from_start
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

fn sampler_note_pan_value(channel_pan: f32, note_pan: f32) -> f32 {
    let note_pan_offset = note_pan.clamp(0.0, 128.0) / 64.0 - 1.0;
    (channel_pan + note_pan_offset).clamp(-1.0, 1.0)
}

fn sampler_metadata_gain(gain_db: Option<i16>) -> f32 {
    gain_db.map_or(1.0, |gain_db| {
        10.0f64
            .powf(f64::from(gain_db) / 20.0)
            .min(f64::from(f32::MAX)) as f32
    })
}

/// Applies the saved endpoint attenuation as a provisional linear-amplitude taper across the
/// source sample. FL Studio documents these as precomputed fades but does not publish the curve.
fn sampler_precomputed_fade_gain(
    source_position: f64,
    source_frame_count: usize,
    fade_in_raw: Option<u16>,
    fade_out_raw: Option<u16>,
) -> f32 {
    if source_frame_count <= 1 || !source_position.is_finite() {
        return 1.0;
    }
    let progress = (source_position / (source_frame_count - 1) as f64).clamp(0.0, 1.0) as f32;
    let fade_in_amount = fade_in_raw
        .filter(|value| *value <= 1024)
        .map_or(0.0, |value| f32::from(value) / 1024.0);
    let fade_out_amount = fade_out_raw
        .filter(|value| *value <= 1024)
        .map_or(0.0, |value| f32::from(value) / 1024.0);
    let fade_in_gain = 1.0 - fade_in_amount * (1.0 - progress);
    let fade_out_gain = 1.0 - fade_out_amount * progress;
    fade_in_gain * fade_out_gain
}

fn supported_sampler_fade_raw(raw_value: Option<u16>) -> Option<u16> {
    raw_value.filter(|value| (1..=1024).contains(value))
}

fn sampler_precompute_fades(
    source: &Arc<DecodedAudio>,
    fade_in_raw: Option<u16>,
    fade_out_raw: Option<u16>,
) -> Arc<DecodedAudio> {
    let fade_in_raw = supported_sampler_fade_raw(fade_in_raw);
    let fade_out_raw = supported_sampler_fade_raw(fade_out_raw);
    if fade_in_raw.is_none() && fade_out_raw.is_none() {
        return Arc::clone(source);
    }

    let mut processed = (**source).clone();
    let source_frame_count = processed.frame_count();
    for channel in &mut processed.channels {
        for (frame, sample) in channel.iter_mut().enumerate() {
            *sample *= sampler_precomputed_fade_gain(
                frame as f64,
                source_frame_count,
                fade_in_raw,
                fade_out_raw,
            );
        }
    }
    Arc::new(processed)
}

fn audio_clip_pitch_ratio(pitch_offset_cents: f64) -> f64 {
    2.0f64.powf(pitch_offset_cents / 1_200.0)
}

fn audio_clip_master_pitch_ratio(pitch_offset_cents: i16) -> f64 {
    audio_clip_pitch_ratio(f64::from(pitch_offset_cents))
}

fn audio_clip_duration_preserving_pitch_cents(
    channel_pitch_cents: f64,
    stretch_pitch_cents: i32,
    clip_index: usize,
) -> Result<i32, String> {
    let combined_cents = channel_pitch_cents + f64::from(stretch_pitch_cents);
    if !combined_cents.is_finite() || !(-2_500.0..=2_500.0).contains(&combined_cents) {
        return Err(format!(
            "Playlist clip {clip_index} combines stretch and pitch offsets outside the supported ±25-semitone range"
        ));
    }
    Ok(combined_cents.round() as i32)
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SampleBounds {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug)]
enum AudioClipStretchPlan {
    Resample {
        playback_rate_ratio: f64,
    },
    DurationPreserving {
        target_duration_frames: u64,
        pitch_raw: i32,
        mode_raw: i32,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProcessedAudioCacheKey {
    path: PathBuf,
    source_bounds: SampleBounds,
    target_duration_frames: u64,
    pitch_raw: i32,
    mode_raw: i32,
}

#[derive(Debug)]
struct PreparedClipMixerRouteOverride {
    mixer_transform: MixerSourceRouteTransform,
    live_mixer_source_control: Option<LiveMixerSourceControl>,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_effect_input_transforms: Vec<(usize, MixerSourceRouteTransform)>,
}

#[derive(Clone, Debug)]
struct PreparedClip {
    clip_index: usize,
    playlist_track_id: Option<u32>,
    channel_id: u16,
    mixer_track: Option<i8>,
    path: PathBuf,
    source_id: usize,
    start_frame: u64,
    source_bounds: SampleBounds,
    reverse: bool,
    pitch_ratio: f64,
    duration_frames: u64,
    gain: f32,
    envelope_gain: f32,
    fade_in_frames: u64,
    fade_out_frames: u64,
    fade_in_curve: PlaylistAudioClipFadeCurve,
    fade_in_tension: i16,
    fade_out_curve: PlaylistAudioClipFadeCurve,
    fade_out_tension: i16,
    pan: f32,
    pan_law: PanLaw,
    mixer_transform: MixerSourceRouteTransform,
    live_mixer_source_control: Option<LiveMixerSourceControl>,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_effect_input_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_route_override: Arc<ArcSwapOption<PreparedClipMixerRouteOverride>>,
}

struct PreparedAudioClipRender {
    summary: AudioClipRenderSummary,
    resampling_quality: ResamplingQuality,
    source_paths: BTreeSet<PathBuf>,
    audio_sources: Vec<DecodedAudio>,
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
    audio_sources: &'a [DecodedAudio],
    sample_rate: u32,
    resampling_quality: ResamplingQuality,
    cancelled: &'a AtomicBool,
    live_mixer_source_targets: Option<&'a MixerTrackPeakMeters>,
    control: Arc<AudioClipMixWorkerControl>,
    scratch: Arc<Vec<Mutex<AudioClipMixWorkerScratch>>>,
}

struct PreparedAudioClipMixContext<'a> {
    clips: &'a [PreparedClip],
    audio_sources: &'a [DecodedAudio],
    sample_rate: u32,
    resampling_quality: ResamplingQuality,
    cancelled: &'a AtomicBool,
    live_mixer_source_targets: Option<&'a MixerTrackPeakMeters>,
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
        audio_context: &'scope PreparedAudioClipMixContext<'scope>,
        include_meters: bool,
        include_effect_input_buses: bool,
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
                        meter_buses: include_meters.then(|| {
                            vec![
                                0.0;
                                MIXER_TRACK_METER_COUNT
                                    * STREAM_BLOCK_FRAMES
                                    * 2
                                    * if include_effect_input_buses { 2 } else { 1 }
                            ]
                        }),
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
                        audio_sources: audio_context.audio_sources,
                        sample_rate: audio_context.sample_rate,
                        resampling_quality: audio_context.resampling_quality,
                        cancelled: audio_context.cancelled,
                        live_mixer_source_targets: audio_context.live_mixer_source_targets,
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
                let active_meter_samples = output_buses.len();
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
        audio_sources,
        sample_rate,
        resampling_quality,
        cancelled,
        live_mixer_source_targets,
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
                let max_single_plane_samples = MIXER_TRACK_METER_COUNT * STREAM_BLOCK_FRAMES * 2;
                let active_bus_samples = if worker_buses.len() > max_single_plane_samples {
                    active_meter_samples * 2
                } else {
                    active_meter_samples
                };
                let buses = &mut worker_buses[..active_bus_samples];
                buses.fill(0.0);
                Some(buses)
            } else {
                None
            };
            for clip in &clips {
                let source = audio_sources.get(clip.source_id).ok_or_else(|| {
                    format!(
                        "prepared audio clip {} from {} has no decoded source",
                        clip.clip_index,
                        clip.path.display()
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
                        live_mixer_source_targets,
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
        let source = context.audio_sources.get(clip.source_id).ok_or_else(|| {
            format!(
                "prepared audio clip {} from {} has no decoded source",
                clip.clip_index,
                clip.path.display()
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
                live_mixer_source_targets: context.live_mixer_source_targets,
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

/// Finds the nearest Audio Clip source zero crossing within ten milliseconds of a Playlist tick.
/// The returned tick is suitable for `split_playlist_audio_clip`; `None` means no crossing was
/// close enough and the caller should keep the requested tick.
pub fn nearest_playlist_audio_clip_zero_crossing_tick(
    document: &FlpDocument,
    project_path: &Path,
    arrangement_id: u16,
    clip_index: usize,
    requested_tick: u32,
) -> Result<Option<u32>, String> {
    let arrangements = document.arrangements().map_err(|error| error.to_string())?;
    let mut matching_arrangements = arrangements
        .iter()
        .filter(|arrangement| arrangement.id == arrangement_id);
    let Some(arrangement) = matching_arrangements.next() else {
        return Err("the requested Playlist arrangement does not exist".to_owned());
    };
    if matching_arrangements.next().is_some() {
        return Err("the requested Playlist arrangement is ambiguous".to_owned());
    }
    let Some(clip) = arrangement.clips.get(clip_index) else {
        return Err("the requested Playlist Audio Clip does not exist".to_owned());
    };
    let PlaylistClipTarget::Channel { id: channel_id } = clip.target() else {
        return Err("zero-crossing splits apply to Playlist Audio Clips".to_owned());
    };
    let channels = document.channels();
    let mut matching_channels = channels.iter().filter(|channel| channel.id() == channel_id);
    let Some(channel) = matching_channels.next() else {
        return Err("the Playlist Audio Clip channel does not exist".to_owned());
    };
    if matching_channels.next().is_some() || channel.kind() != Some(4) {
        return Err("the Playlist clip does not target one Audio Clip channel".to_owned());
    }
    if clip
        .scale
        .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
    {
        return Err("zero-crossing splits require the default Audio Clip scale".to_owned());
    }
    let relative_tick = requested_tick
        .checked_sub(clip.position_ticks)
        .filter(|tick| *tick > 0 && *tick < clip.length_ticks)
        .ok_or_else(|| "the requested split point must be inside the Audio Clip".to_owned())?;
    let ppq = document.header().ppq();
    let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    if ppq == 0 || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("zero-crossing splits require a valid project tempo and PPQ".to_owned());
    }
    let clip_end_tick = clip
        .position_ticks
        .checked_add(clip.length_ticks)
        .ok_or_else(|| "the Audio Clip timeline range overflows".to_owned())?;
    let tempo_channel_ids = channels
        .iter()
        .filter(|candidate| {
            candidate.kind() == Some(5)
                && candidate
                    .display_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
        })
        .map(ChannelSummary::id)
        .collect::<BTreeSet<_>>();
    if arrangement.clips.iter().any(|candidate| {
        let PlaylistClipTarget::Channel { id } = candidate.target() else {
            return false;
        };
        tempo_channel_ids.contains(&id)
            && candidate.position_ticks < clip_end_tick
            && candidate
                .position_ticks
                .saturating_add(candidate.length_ticks)
                > clip.position_ticks
    }) {
        return Err(
            "zero-crossing splits across Playlist tempo automation are unsupported".to_owned(),
        );
    }
    let sample_path = channel
        .sample_path()
        .ok_or_else(|| "the Audio Clip channel has no sample path".to_owned())?;
    let resolved_path = SamplePathResolver::new(project_path)
        .resolve(sample_path)
        .map_err(|error| format!("could not resolve the Audio Clip sample: {error}"))?;
    let audio = decode_audio_file(&resolved_path)?;
    if audio.channels.is_empty() || audio.channels.len() > 2 || audio.sample_rate == 0 {
        return Err("zero-crossing splits support mono or stereo sample files".to_owned());
    }
    let source_bounds =
        sample_source_bounds_at_project_tempo(clip, &audio, clip_index, ppq, tempo_bpm)?;
    if audio
        .channels
        .iter()
        .any(|channel_samples| channel_samples.len() < source_bounds.end)
    {
        return Err("the Audio Clip sample has inconsistent channel lengths".to_owned());
    }
    let desired_frame = source_bounds.start as f64
        + f64::from(relative_tick) * f64::from(audio.sample_rate) * 60.0
            / (f64::from(ppq) * tempo_bpm);
    let search_radius = (f64::from(audio.sample_rate) * 0.010).ceil().max(1.0) as usize;
    let desired_frame = desired_frame.round().clamp(0.0, usize::MAX as f64) as usize;
    let first_frame = desired_frame
        .saturating_sub(search_radius)
        .max(source_bounds.start.saturating_add(1));
    let last_frame = desired_frame
        .saturating_add(search_radius)
        .min(source_bounds.end.saturating_sub(1));
    let is_zero_crossing = |left: f32, right: f32| {
        left.is_finite()
            && right.is_finite()
            && ((left <= 0.0 && right >= 0.0) || (left >= 0.0 && right <= 0.0))
    };
    let mut nearest = None::<(usize, usize, f32, usize)>;
    for frame in first_frame..=last_frame {
        let mut crossing_channels = 0usize;
        let mut peak_amplitude = 0.0f32;
        for channel_samples in &audio.channels {
            let left = channel_samples[frame - 1];
            let right = channel_samples[frame];
            crossing_channels += usize::from(is_zero_crossing(left, right));
            peak_amplitude = peak_amplitude.max(left.abs()).max(right.abs());
        }
        if crossing_channels == 0 {
            continue;
        }
        let distance = frame.abs_diff(desired_frame);
        let candidate = (
            audio.channels.len().saturating_sub(crossing_channels),
            distance,
            peak_amplitude,
            frame,
        );
        if nearest.is_none_or(|current| {
            candidate.0 < current.0
                || (candidate.0 == current.0
                    && (candidate.1 < current.1
                        || (candidate.1 == current.1 && candidate.2.total_cmp(&current.2).is_lt())))
        }) {
            nearest = Some(candidate);
        }
    }
    let Some((_, _, _, crossing_frame)) = nearest else {
        return Ok(None);
    };
    let frames_per_tick = f64::from(audio.sample_rate) * 60.0 / (f64::from(ppq) * tempo_bpm);
    let source_offset_frames = crossing_frame.saturating_sub(source_bounds.start) as f64;
    let crossing_offset_ticks = (source_offset_frames / frames_per_tick).round();
    if !crossing_offset_ticks.is_finite()
        || crossing_offset_ticks < 1.0
        || crossing_offset_ticks >= f64::from(clip.length_ticks)
    {
        return Ok(None);
    }
    let crossing_tick = clip
        .position_ticks
        .checked_add(crossing_offset_ticks as u32)
        .filter(|tick| *tick > clip.position_ticks && *tick < clip_end_tick);
    Ok(crossing_tick)
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

fn sampler_polyphony_slide_frames(
    slide_raw: Option<u32>,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: u32,
) -> u64 {
    if ppq == 0 || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return 0;
    }
    let ticks = sampler_polyphony_slide_ticks(slide_raw, ppq);
    let frames = ticks * 60.0 * f64::from(sample_rate) / (f64::from(ppq) * tempo_bpm);
    if frames.is_finite() && frames > 0.0 {
        frames.round().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

fn sampler_polyphony_slide_ticks(slide_raw: Option<u32>, ppq: u16) -> f64 {
    if ppq == 0 {
        return 0.0;
    }
    let raw = f64::from(
        slide_raw
            .unwrap_or(SAMPLER_POLYPHONY_SLIDE_DEFAULT_RAW)
            .min(SAMPLER_POLYPHONY_SLIDE_MAX_RAW),
    );
    let normalized = raw / f64::from(SAMPLER_POLYPHONY_SLIDE_MAX_RAW);
    // The raw control is documented as nonlinear. This fourth-power curve is
    // an inferred fit to the published default (820 ~= half a step) and max
    // (1660 = eight steps), pending a controlled native render comparison.
    f64::from(ppq) * 2.0 * normalized.powi(4)
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

/// Return stereo output gains for a project channel's saved volume and pan controls.
pub fn project_channel_stereo_output_gains(
    document: &FlpDocument,
    channel_id: u16,
) -> Option<(f32, f32)> {
    project_channel_stereo_output_gains_with_layer_adjustments(document, channel_id, 1.0, 0)
}

/// Return project channel stereo gains with provisional Layer volume and pan adjustments applied.
pub fn project_channel_stereo_output_gains_with_layer_adjustments(
    document: &FlpDocument,
    channel_id: u16,
    layer_gain: f32,
    layer_pan_raw: i32,
) -> Option<(f32, f32)> {
    if !layer_gain.is_finite() || layer_gain < 0.0 {
        return None;
    }
    let channel = document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)?;
    let (channel_gain, channel_pan) = channel_gain_pan(channel.volume(), channel.pan());
    let pan = (channel_pan + layer_pan_raw.clamp(-6_400, 6_400) as f32 / 6_400.0).clamp(-1.0, 1.0);
    let (left_pan, right_pan) = channel_pan_gains(pan, false, project_pan_law(document));
    let gain = channel_gain * layer_gain;
    Some((gain * left_pan, gain * right_pan))
}

/// Returns the steady-state fallback for a Sampler Volume envelope when full timing is unavailable.
///
/// The envelope's saved raw time minimum (100) is documented as 0%, so an envelope whose
/// pre-delay, attack, hold, and decay are all at that minimum reaches sustain immediately.
/// Other timing shapes use `sampler_volume_envelope_config` when its fields and sync mode are
/// supported. Amount is a bipolar multiplier around the unmodulated level: zero leaves volume
/// unchanged, positive values move toward sustain, and negative values invert that movement.
fn sampler_volume_envelope_sustain_gain(channel: &ChannelSummary) -> f32 {
    let Some(envelope) = channel.sampler_volume_envelope() else {
        return 1.0;
    };
    sampler_volume_envelope_sustain_gain_from_raw(
        envelope.enabled(),
        envelope.predelay_raw(),
        envelope.attack_raw(),
        envelope.hold_raw(),
        envelope.decay_raw(),
        envelope.sustain_raw(),
        envelope.amount_raw(),
    )
}

fn sampler_volume_envelope_sustain_gain_from_raw(
    enabled: bool,
    predelay: Option<i32>,
    attack: Option<i32>,
    hold: Option<i32>,
    decay: Option<i32>,
    sustain: Option<i32>,
    amount: Option<i32>,
) -> f32 {
    if !enabled {
        return 1.0;
    }
    let (Some(predelay), Some(attack), Some(hold), Some(decay), Some(sustain), Some(amount)) =
        (predelay, attack, hold, decay, sustain, amount)
    else {
        return 1.0;
    };
    if [predelay, attack, hold, decay]
        .into_iter()
        .any(|raw| raw != 100)
        || !(0..=128).contains(&sustain)
        || !(-128..=128).contains(&amount)
    {
        return 1.0;
    }
    let envelope_amount = amount as f32 / 128.0;
    let sustain_level = sustain as f32 / 128.0;
    (1.0 + envelope_amount * (sustain_level - 1.0)).clamp(0.0, 2.0)
}

fn sampler_volume_envelope_config(
    channel: &ChannelSummary,
    sample_rate: u32,
) -> Option<SamplerVolumeEnvelope> {
    let envelope = channel.sampler_volume_envelope()?;
    if !envelope.enabled() {
        return None;
    }
    let (
        Some(predelay),
        Some(attack),
        Some(hold),
        Some(decay),
        Some(sustain),
        Some(release),
        Some(amount),
    ) = (
        envelope.predelay_raw(),
        envelope.attack_raw(),
        envelope.hold_raw(),
        envelope.decay_raw(),
        envelope.sustain_raw(),
        envelope.release_raw(),
        envelope.amount_raw(),
    )
    else {
        return None;
    };
    if !(0..=128).contains(&sustain)
        || !(-128..=128).contains(&amount)
        || [predelay, attack, hold, decay, release]
            .into_iter()
            .any(|raw| !(100..=65_536).contains(&raw))
    {
        return None;
    }
    if envelope.tempo_sync_enabled()
        && [predelay, attack, hold, decay, release]
            .into_iter()
            .any(|raw| raw != 100)
    {
        return None;
    }
    let frames = |raw| sampler_envelope_time_frames(raw, sample_rate);
    Some(SamplerVolumeEnvelope {
        predelay_frames: frames(predelay)?,
        attack_frames: frames(attack)?,
        hold_frames: frames(hold)?,
        decay_frames: frames(decay)?,
        release_frames: usize::try_from(frames(release)?).ok()?,
        sustain_level: sustain as f32 / 128.0,
        amount: amount as f32 / 128.0,
    })
}

fn sampler_envelope_time_frames(raw: i32, sample_rate: u32) -> Option<u64> {
    if !(100..=65_536).contains(&raw) || sample_rate == 0 {
        return None;
    }
    // Candidate conversion: SatyrDiamond/flp-parser computes this squared 0..36-second mapping
    // as an intermediate but returns the original raw value, so the mapping is unverified.
    // Native FL Studio comparison remains open.
    let normalized = f64::from(raw - 100) / 65_436.0;
    let seconds = (normalized * 6.0).powi(2);
    let frames = (seconds * f64::from(sample_rate)).round();
    (frames.is_finite() && (0.0..=u64::MAX as f64).contains(&frames)).then_some(frames as u64)
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
            live_mixer_source_targets: None,
        },
    )
}

struct ClipMixMonitor<'a> {
    cancelled: Option<&'a AtomicBool>,
    meter_buses: Option<&'a mut [f32]>,
    meter_stride_samples: usize,
    live_mixer_source_targets: Option<&'a MixerTrackPeakMeters>,
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
        meter_buses,
        meter_stride_samples,
        live_mixer_source_targets,
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
    let (mut meter_buses, mut effect_input_buses) = if let Some(meter_buses) = meter_buses {
        if meter_stride_samples != mix.len() {
            return Err("Mixer meter buses must match the audio mix window".to_owned());
        }
        let (meter_buses, effect_input_buses) =
            crate::audio::split_mixer_track_buses_mut(meter_buses, meter_stride_samples)?;
        (Some(meter_buses), effect_input_buses)
    } else {
        (None, None)
    };
    let overlap_start = clip.start_frame.max(mix_start_frame);
    let overlap_end = end_frame.min(mix_end_frame);
    if overlap_start >= overlap_end {
        return Ok(());
    }
    let route_override = clip.mixer_route_override.load_full();
    let mixer_transform = route_override
        .as_deref()
        .map_or(clip.mixer_transform, |route| route.mixer_transform);
    let live_mixer_source_control = route_override
        .as_deref()
        .map_or(clip.live_mixer_source_control.as_ref(), |route| {
            route.live_mixer_source_control.as_ref()
        });
    let mixer_meter_transforms = route_override
        .as_deref()
        .map_or(clip.mixer_meter_transforms.as_slice(), |route| {
            route.mixer_meter_transforms.as_slice()
        });
    let mixer_effect_input_transforms = route_override
        .as_deref()
        .map_or(clip.mixer_effect_input_transforms.as_slice(), |route| {
            route.mixer_effect_input_transforms.as_slice()
        });
    let live_mixer_targets = live_mixer_source_control.and_then(|source_control| {
        live_mixer_source_targets.and_then(|meters| source_control.targets(meters))
    });
    let (left_pan_gain, right_pan_gain) =
        channel_pan_gains(clip.pan, source.channels.len() == 1, clip.pan_law);
    let source_frames_per_output =
        f64::from(source.sample_rate) / f64::from(output_rate) * clip.pitch_ratio;
    for output_frame in overlap_start..overlap_end {
        let output_offset = output_frame - clip.start_frame;
        if output_offset.is_multiple_of(16_384) {
            check_cancelled(cancelled)?;
        }
        let last_source_position = (clip.source_bounds.end - clip.source_bounds.start - 1) as f64;
        let source_offset = output_offset as f64 * source_frames_per_output;
        let source_position = if clip.reverse {
            (last_source_position - source_offset).max(0.0)
        } else {
            source_offset.min(last_source_position)
        };
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
        let envelope_gain = playlist_audio_clip_envelope_gain(clip, output_offset);
        let mut frame = [
            left * clip.gain * envelope_gain * left_pan_gain,
            right * clip.gain * envelope_gain * right_pan_gain,
        ];
        let live_controls = match (live_mixer_source_control, live_mixer_targets.as_ref()) {
            (Some(source_control), Some(targets)) => {
                let controls = source_control.next_frame_controls(targets);
                source_control.apply_output_frame(controls, &mut frame);
                Some((source_control, controls))
            }
            _ => None,
        };
        if live_controls.is_none() {
            mixer_transform.apply_frame(&mut frame);
        }
        mix[output_index] += frame[0];
        mix[output_index + 1] += frame[1];
        if let Some(meter_buses) = meter_buses.as_deref_mut() {
            let meter_transforms = live_controls.map_or_else(
                || mixer_meter_transforms,
                |(source_control, _)| source_control.insert_meter_transforms(),
            );
            for (track, transform) in meter_transforms {
                if *track >= MIXER_TRACK_METER_COUNT {
                    return Err("Mixer meter route exceeds the supported track range".to_owned());
                }
                let mut meter_frame = [
                    left * clip.gain * envelope_gain * left_pan_gain,
                    right * clip.gain * envelope_gain * right_pan_gain,
                ];
                if let Some((source_control, controls)) = live_controls {
                    source_control.apply_meter_frame(
                        controls,
                        *track,
                        *transform,
                        &mut meter_frame,
                    );
                } else {
                    transform.apply_frame(&mut meter_frame);
                }
                let meter_offset = *track * meter_stride_samples + output_index;
                meter_buses[meter_offset] += meter_frame[0];
                meter_buses[meter_offset + 1] += meter_frame[1];
            }
        }
        if let Some(effect_input_buses) = effect_input_buses.as_deref_mut() {
            let effect_input_transforms = live_controls.map_or_else(
                || mixer_effect_input_transforms,
                |(source_control, _)| source_control.insert_effect_input_transforms(),
            );
            for (track, transform) in effect_input_transforms {
                if *track >= MIXER_TRACK_METER_COUNT {
                    return Err(
                        "Mixer effect input route exceeds the supported track range".to_owned()
                    );
                }
                let mut effect_input_frame = [
                    left * clip.gain * envelope_gain * left_pan_gain,
                    right * clip.gain * envelope_gain * right_pan_gain,
                ];
                if let Some((source_control, controls)) = live_controls {
                    source_control.apply_effect_input_frame(
                        controls,
                        *track,
                        *transform,
                        &mut effect_input_frame,
                    );
                } else {
                    transform.apply_frame(&mut effect_input_frame);
                }
                let effect_input_offset = *track * meter_stride_samples + output_index;
                effect_input_buses[effect_input_offset] += effect_input_frame[0];
                effect_input_buses[effect_input_offset + 1] += effect_input_frame[1];
            }
        }
    }
    Ok(())
}

fn playlist_audio_clip_envelope_gain(clip: &PreparedClip, output_offset: u64) -> f32 {
    let fade_in_gain = if clip.fade_in_frames == 0 {
        1.0
    } else {
        playlist_audio_clip_fade_gain(
            output_offset as f32 / clip.fade_in_frames as f32,
            clip.fade_in_curve,
            clip.fade_in_tension,
        )
    };
    let fade_out_gain = if clip.fade_out_frames == 0 {
        1.0
    } else {
        let remaining_frames = clip
            .duration_frames
            .saturating_sub(output_offset.saturating_add(1));
        playlist_audio_clip_fade_gain(
            remaining_frames as f32 / clip.fade_out_frames as f32,
            clip.fade_out_curve,
            clip.fade_out_tension,
        )
    };
    clip.envelope_gain * fade_in_gain * fade_out_gain
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
    fn pattern_audio_layers_follow_the_transport_seek_and_loop_phase() {
        let layer = PatternAudioLayer {
            channel_id: 7,
            left_gain: 1.0,
            right_gain: 1.0,
            samples: vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0],
        };
        let mut output = vec![0.0; 8];
        let routes = HashMap::from([(
            7,
            PreparedClapMixerRoute {
                mixer_transform: MixerSourceRouteTransform::default(),
                live_source_control: None,
                mixer_meter_transforms: Vec::new(),
                mixer_effect_input_transforms: Vec::new(),
            },
        )]);

        mix_pattern_audio_layers(&mut output, 4, 2, 3, &[layer], &routes, None).unwrap();

        assert_eq!(output, [3.0, 30.0, 1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);
    }

    #[test]
    fn playlist_clap_audio_layers_mix_at_their_absolute_timeline_frames() {
        let layer = PlaylistAudioLayer {
            channel_id: 9,
            left_gain: 1.0,
            right_gain: 1.0,
            samples: vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0],
        };
        let mut output = vec![0.0; 6];

        mix_playlist_clap_audio_layers(&mut output, 1, 3, &[layer], &HashMap::new(), None)
            .expect("Playlist CLAP audio should mix without routing");

        assert_eq!(output, [2.0, 20.0, 3.0, 30.0, 0.0, 0.0]);
    }

    fn performance_audio_clip_fixture(record_size: usize, scale: Option<f64>) -> FlpDocument {
        fn append_data_event(event_stream: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
            assert!(payload.len() < 128);
            event_stream.extend_from_slice(&[opcode, payload.len() as u8]);
            event_stream.extend_from_slice(payload);
        }

        assert!(matches!(record_size, 80 | 88));
        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"26.0.0\0");
        event_stream.extend_from_slice(&[0x40, 0, 0, 0x15, 4]);
        append_data_event(&mut event_stream, 0xC4, b"performance.wav\0");
        event_stream.extend_from_slice(&[0x62, 0, 0]);
        event_stream.extend_from_slice(&[0x63, 0, 0]);

        let mut playlist_clip = vec![0_u8; record_size];
        playlist_clip[4..6].copy_from_slice(&0x5000_u16.to_le_bytes());
        playlist_clip[6..8].copy_from_slice(&0_u16.to_le_bytes());
        playlist_clip[8..12].copy_from_slice(&384_u32.to_le_bytes());
        playlist_clip[12..14].copy_from_slice(&499_u16.to_le_bytes());
        if record_size == 88 {
            playlist_clip[24..28].copy_from_slice(&0_u32.to_le_bytes());
            playlist_clip[28..32].copy_from_slice(&384_u32.to_le_bytes());
        } else {
            playlist_clip[24..28].copy_from_slice(&(-1.0_f32).to_le_bytes());
            playlist_clip[28..32].copy_from_slice(&(-1.0_f32).to_le_bytes());
            if let Some(scale) = scale {
                playlist_clip[64..72].copy_from_slice(&scale.to_le_bytes());
            }
        }
        append_data_event(&mut event_stream, 0xE9, &playlist_clip);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FLhd");
        bytes.extend_from_slice(&6_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&96_u16.to_le_bytes());
        bytes.extend_from_slice(b"FLdt");
        bytes.extend_from_slice(&(event_stream.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&event_stream);
        FlpDocument::parse(&bytes).expect("Performance Mode Audio Clip fixture should parse")
    }

    #[test]
    fn performance_mode_accepts_default_scale_2026_audio_clips_without_scale_fields() {
        let modern = performance_audio_clip_fixture(88, None);
        let modern_clip = &modern.arrangements().unwrap()[0].clips[0];
        assert_eq!(modern_clip.record_size, 88);
        assert_eq!(modern_clip.scale, None);
        assert_eq!(
            playlist_performance_audio_clip_indices(&modern, 0, 384).unwrap(),
            BTreeSet::from([0])
        );
        assert_eq!(
            playlist_performance_clip_indices(&modern, 0, 384).unwrap(),
            BTreeSet::from([0])
        );

        let legacy = performance_audio_clip_fixture(80, Some(1.0));
        assert_eq!(
            playlist_performance_audio_clip_indices(&legacy, 0, 384).unwrap(),
            BTreeSet::from([0])
        );

        let stretched = performance_audio_clip_fixture(80, Some(0.75));
        assert!(
            playlist_performance_audio_clip_indices(&stretched, 0, 384)
                .unwrap()
                .is_empty()
        );
    }

    fn test_live_mixer_source_control(
        track: usize,
        saved_fader_gain: f32,
        saved_pan_raw: Option<i32>,
        saved_stereo_separation_raw: Option<i32>,
        output_transform: MixerSourceRouteTransform,
        insert_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
        sample_rate: u32,
    ) -> LiveMixerSourceControl {
        LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain,
                saved_pan_raw,
                saved_stereo_separation_raw,
                saved_signal_flags: None,
                saved_enabled: None,
                pan_law_raw: Some(2),
                output_transform,
                insert_meter_transforms,
                ..MixerLiveSourceRoutePlan::default()
            },
            sample_rate,
        )
    }

    #[test]
    fn latched_performance_clip_stops_on_retrigger_only() {
        assert!(performance_latch_should_stop(
            Some(PlaylistTrackPress::Latch),
            Some(4),
            4
        ));
        assert!(!performance_latch_should_stop(
            Some(PlaylistTrackPress::Latch),
            Some(5),
            4
        ));
        assert!(!performance_latch_should_stop(
            Some(PlaylistTrackPress::Latch),
            None,
            4
        ));
        assert!(!performance_latch_should_stop(
            Some(PlaylistTrackPress::Retrigger),
            Some(4),
            4
        ));
    }

    #[test]
    fn performance_one_shot_motion_plays_once() {
        assert!(performance_motion_is_one_shot(Some(
            PlaylistTrackMotion::OneShot
        )));
        assert!(!performance_motion_is_one_shot(Some(
            PlaylistTrackMotion::Stay
        )));
        assert!(!performance_motion_is_one_shot(Some(
            PlaylistTrackMotion::MarchWrap
        )));
        assert!(!performance_motion_is_one_shot(None));
        assert_eq!(
            performance_motion_transition_tick(Some(PlaylistTrackMotion::OneShot), 384, 384, 200)
                .unwrap(),
            384
        );
        assert_eq!(
            performance_motion_transition_tick(Some(PlaylistTrackMotion::OneShot), 384, 384, 500)
                .unwrap(),
            500
        );
        assert_eq!(
            performance_motion_transition_tick(Some(PlaylistTrackMotion::Stay), 384, 384, 500)
                .unwrap(),
            768
        );
    }

    #[test]
    fn hold_motion_release_follows_saved_track_motion() {
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchWrap),
                4,
                &[4, 9, 12],
                1,
            ),
            Some(PerformanceMotionReleaseStep::Launch(9))
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchWrap),
                12,
                &[4, 9, 12],
                1,
            ),
            Some(PerformanceMotionReleaseStep::Launch(4))
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchStay),
                9,
                &[4, 9, 12],
                1,
            ),
            Some(PerformanceMotionReleaseStep::Launch(12))
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchStay),
                12,
                &[4, 9, 12],
                1,
            ),
            None
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchStop),
                12,
                &[4, 9, 12],
                1,
            ),
            Some(PerformanceMotionReleaseStep::Stop)
        );
        let random_step = performance_motion_release_step_with_seed(
            Some(PlaylistTrackMotion::Random),
            9,
            &[4, 9, 12],
            1,
        );
        assert!(matches!(
            random_step,
            Some(PerformanceMotionReleaseStep::Launch(4 | 9 | 12))
        ));
        let exclusive_step = performance_motion_release_step_with_seed(
            Some(PlaylistTrackMotion::ExclusiveRandom),
            9,
            &[4, 9, 12],
            1,
        );
        assert!(matches!(
            exclusive_step,
            Some(PerformanceMotionReleaseStep::Launch(4 | 12))
        ));
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::Stay),
                9,
                &[4, 9, 12],
                1,
            ),
            None
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::OneShot),
                9,
                &[4, 9, 12],
                1,
            ),
            None
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchWrap),
                3,
                &[4, 9, 12],
                1,
            ),
            None
        );
        assert_eq!(
            performance_motion_release_step_with_seed(
                Some(PlaylistTrackMotion::MarchWrap),
                4,
                &[],
                1,
            ),
            None
        );
    }

    #[test]
    fn motion_modes_advance_after_pass_except_stay_and_hold_motion() {
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::MarchWrap),
                Some(PlaylistTrackPress::Retrigger),
                4,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Advance
        );
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::MarchWrap),
                Some(PlaylistTrackPress::HoldMotion),
                4,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Loop
        );
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::MarchStay),
                Some(PlaylistTrackPress::Retrigger),
                12,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Loop
        );
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::MarchStop),
                Some(PlaylistTrackPress::Retrigger),
                12,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Stop
        );
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::Random),
                Some(PlaylistTrackPress::Latch),
                9,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Advance
        );
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::Stay),
                Some(PlaylistTrackPress::Retrigger),
                4,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Loop
        );
        assert_eq!(
            performance_motion_pass_action(
                Some(PlaylistTrackMotion::OneShot),
                Some(PlaylistTrackPress::HoldMotion),
                4,
                &[4, 9, 12],
            ),
            PerformanceMotionPassAction::Stop
        );
    }

    #[test]
    fn track_stop_cancels_later_performance_launches_on_that_track() {
        assert!(performance_launch_cancelled_by_stop(Some(7), 101, 7, 100));
        assert!(performance_launch_cancelled_by_stop(Some(7), 100, 7, 100));
        assert!(!performance_launch_cancelled_by_stop(Some(7), 99, 7, 100));
        assert!(!performance_launch_cancelled_by_stop(Some(8), 101, 7, 100));
        assert!(!performance_launch_cancelled_by_stop(None, 101, 7, 100));
    }

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
    fn master_signal_transform_supports_smooth_intermediate_values() {
        let mut frame = [1.0, 0.25];
        apply_master_signal_transform(&mut frame, 0.0, 0.5);
        assert_eq!(frame, [0.625, 0.625]);

        frame = [1.0, 0.25];
        apply_master_signal_transform(&mut frame, 0.5, 0.0);
        assert_eq!(frame, [0.0, 0.0]);
    }

    #[test]
    fn live_master_signal_flags_are_smoothed_from_saved_state() {
        let target = Arc::new(AtomicI32::new(0));
        let output = MixerMasterOutput {
            enabled: Some(true),
            live_signal_flags_target: Some(Arc::clone(&target)),
            live_signal_flags_smoothing_alpha: crate::audio::output_gain_smoothing_alpha(48_000.0),
            ..MixerMasterOutput::default()
        };

        let mut frame = [1.0, 0.25];
        output.apply(&mut frame);
        assert_eq!(frame, [1.0, 0.25]);

        target.store(3, Ordering::Release);
        frame = [1.0, 0.25];
        output.apply(&mut frame);
        assert!(frame[0] > 0.99 && frame[0] < 1.0);
        assert!(frame[1] > 0.24 && frame[1] < 0.26);

        for _ in 0..100_000 {
            frame = [1.0, 0.25];
            output.apply(&mut frame);
        }
        assert!((frame[0] + 0.25).abs() < 0.001);
        assert!((frame[1] + 1.0).abs() < 0.001);
    }

    #[test]
    fn live_mixer_source_fader_smooths_changes_from_its_saved_gain() {
        let source_control = test_live_mixer_source_control(
            1,
            1.0,
            None,
            None,
            MixerSourceRouteTransform::default(),
            Vec::new(),
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, 0.5];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [1.0, 0.5]);

        meters.set_live_mixer_insert_fader_raw(1, Some(0));
        frame = [1.0, 0.5];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 1.0 && frame[0] > 0.99);
        assert_eq!(frame[1], frame[0] * 0.5);

        for _ in 0..100_000 {
            frame = [1.0, 0.5];
            let controls = source_control.next_frame_controls(&targets);
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0] < 0.001);
        assert_eq!(frame[1], frame[0] * 0.5);

        meters.set_live_mixer_insert_fader_raw(1, None);
        for _ in 0..100_000 {
            frame = [1.0, 0.5];
            let controls = source_control.next_frame_controls(&targets);
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0] > 0.999);
        assert_eq!(frame[1], frame[0] * 0.5);
    }

    #[test]
    fn live_mixer_source_stereo_separation_smooths_direct_and_send_routes() {
        let meter_transforms = vec![
            (1, MixerSourceRouteTransform::default()),
            (2, MixerSourceRouteTransform::default().scaled(0.5)),
        ];
        let source_control = test_live_mixer_source_control(
            1,
            1.0,
            None,
            Some(0),
            MixerSourceRouteTransform::default(),
            meter_transforms.clone(),
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_stereo_separation_raw(1, Some(0));
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, -1.0];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [1.0, -1.0]);

        meters.set_live_mixer_insert_stereo_separation_raw(1, Some(64));
        let controls = source_control.next_frame_controls(&targets);
        frame = [1.0, -1.0];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 1.0 && frame[0] > 0.9);
        assert!((frame[0] + frame[1]).abs() < 1e-6);

        let mut direct_meter = [1.0, -1.0];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[0].0,
            meter_transforms[0].1,
            &mut direct_meter,
        );
        assert_eq!(direct_meter, frame);
        let mut send_meter = [1.0, -1.0];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[1].0,
            meter_transforms[1].1,
            &mut send_meter,
        );
        assert!((send_meter[0] - frame[0] * 0.5).abs() < 1e-6);
        assert!((send_meter[1] - frame[1] * 0.5).abs() < 1e-6);

        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, -1.0];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0].abs() < 0.001);
        assert!(frame[1].abs() < 0.001);

        let controls = source_control.next_frame_controls(&targets);
        send_meter = [1.0, -1.0];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[1].0,
            meter_transforms[1].1,
            &mut send_meter,
        );
        assert!(send_meter[0].abs() < 0.0005);
        assert!(send_meter[1].abs() < 0.0005);
    }

    #[test]
    fn live_mixer_source_signal_flags_smooth_direct_and_send_routes() {
        let meter_transforms = vec![
            (1, MixerSourceRouteTransform::default()),
            (2, MixerSourceRouteTransform::default().scaled(0.5)),
        ];
        let source_control = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain: 1.0,
                saved_pan_raw: None,
                saved_stereo_separation_raw: None,
                saved_signal_flags: Some((false, false)),
                saved_enabled: None,
                pan_law_raw: Some(2),
                output_transform: MixerSourceRouteTransform::default(),
                insert_meter_transforms: meter_transforms.clone(),
                ..MixerLiveSourceRoutePlan::default()
            },
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_signal_flags(1, Some((false, false)));
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, 0.25];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [1.0, 0.25]);

        meters.set_live_mixer_insert_signal_flags(1, Some((true, true)));
        let controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 1.0 && frame[0] > -0.25);
        assert!(frame[1] > 0.25 && frame[1] < 0.26);

        let mut direct_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[0].0,
            meter_transforms[0].1,
            &mut direct_meter,
        );
        assert_eq!(direct_meter, frame);
        let mut send_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[1].0,
            meter_transforms[1].1,
            &mut send_meter,
        );
        assert!((send_meter[0] - frame[0] * 0.5).abs() < 1e-6);
        assert!((send_meter[1] - frame[1] * 0.5).abs() < 1e-6);

        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 0.25];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!((frame[0] + 0.25).abs() < 0.001);
        assert!((frame[1] + 1.0).abs() < 0.001);

        let controls = source_control.next_frame_controls(&targets);
        let mut send_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[1].0,
            meter_transforms[1].1,
            &mut send_meter,
        );
        assert!((send_meter[0] + 0.125).abs() < 0.0005);
        assert!((send_meter[1] + 0.5).abs() < 0.0005);
    }

    #[test]
    fn cloned_live_mixer_signal_flag_smoothers_advance_independently() {
        let source_control = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain: 1.0,
                saved_pan_raw: None,
                saved_stereo_separation_raw: None,
                saved_signal_flags: Some((false, false)),
                saved_enabled: None,
                pan_law_raw: Some(2),
                output_transform: MixerSourceRouteTransform::default(),
                insert_meter_transforms: Vec::new(),
                ..MixerLiveSourceRoutePlan::default()
            },
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_signal_flags(1, Some((false, false)));
        let targets = source_control.targets(&meters).unwrap();
        let controls = source_control.next_frame_controls(&targets);
        let mut frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        let cloned_control = source_control.clone();
        let cloned_targets = cloned_control.targets(&meters).unwrap();

        meters.set_live_mixer_insert_signal_flags(1, Some((true, true)));
        for _ in 0..1_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 0.25];
            source_control.apply_output_frame(controls, &mut frame);
        }
        let controls = cloned_control.next_frame_controls(&cloned_targets);
        let mut cloned_frame = [1.0, 0.25];
        cloned_control.apply_output_frame(controls, &mut cloned_frame);
        assert!(frame[0] < cloned_frame[0]);
        assert!(frame[1] < cloned_frame[1]);
    }

    #[test]
    fn live_mixer_source_mute_smooths_direct_and_send_routes() {
        let meter_transforms = vec![
            (1, MixerSourceRouteTransform::default()),
            (2, MixerSourceRouteTransform::default().scaled(0.5)),
        ];
        let source_control = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain: 1.0,
                saved_pan_raw: None,
                saved_stereo_separation_raw: None,
                saved_signal_flags: None,
                saved_enabled: Some(true),
                pan_law_raw: Some(2),
                output_transform: MixerSourceRouteTransform::default(),
                insert_meter_transforms: meter_transforms.clone(),
                ..MixerLiveSourceRoutePlan::default()
            },
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_enabled(1, Some(true));
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, 0.25];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [1.0, 0.25]);

        meters.set_live_mixer_insert_enabled(1, Some(false));
        let controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 1.0 && frame[0] > 0.99);
        assert!(frame[1] < 0.25 && frame[1] > 0.24);

        let mut direct_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[0].0,
            meter_transforms[0].1,
            &mut direct_meter,
        );
        assert_eq!(direct_meter, frame);
        let mut send_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            meter_transforms[1].0,
            meter_transforms[1].1,
            &mut send_meter,
        );
        assert!((send_meter[0] - frame[0] * 0.5).abs() < 1e-6);
        assert!((send_meter[1] - frame[1] * 0.5).abs() < 1e-6);

        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 0.25];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0].abs() < 0.001);
        assert!(frame[1].abs() < 0.001);

        meters.set_live_mixer_insert_enabled(1, Some(true));
        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 0.25];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!((frame[0] - 1.0).abs() < 0.001);
        assert!((frame[1] - 0.25).abs() < 0.001);
    }

    #[test]
    fn direct_mixer_solo_changes_fade_supported_sources() {
        let source_control = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                saved_fader_gain: 1.0,
                live_solo: Some(crate::MixerLiveSoloState {
                    any_soloed: true,
                    soloed: true,
                }),
                ..MixerLiveSourceRoutePlan::default()
            },
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_soloed_tracks(&[1]);
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, 0.25];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [1.0, 0.25]);

        meters.set_live_mixer_soloed_tracks(&[2]);
        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 0.25];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0].abs() < 0.001);
        assert!(frame[1].abs() < 0.001);

        meters.set_live_mixer_soloed_tracks(&[]);
        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 0.25];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!((frame[0] - 1.0).abs() < 0.001);
        assert!((frame[1] - 0.25).abs() < 0.001);
    }

    #[test]
    fn live_mixer_send_levels_and_flags_smooth_nested_output_and_insert_meters() {
        let source_control = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain: 1.0,
                saved_pan_raw: None,
                saved_stereo_separation_raw: None,
                saved_signal_flags: None,
                saved_enabled: Some(true),
                pan_law_raw: Some(2),
                output_transform: MixerSourceRouteTransform::default().scaled(0.5),
                insert_meter_transforms: vec![
                    (1, MixerSourceRouteTransform::default()),
                    (2, MixerSourceRouteTransform::default().scaled(0.5)),
                ],
                insert_effect_input_transforms: Vec::new(),
                live_send_target_keys: vec![
                    crate::MixerLiveSendTargetKey {
                        source_track: 1,
                        destination_track: 2,
                    },
                    crate::MixerLiveSendTargetKey {
                        source_track: 2,
                        destination_track: 0,
                    },
                ],
                saved_live_send_gains: vec![0.5, 1.0],
                saved_live_send_enabled: vec![true, true],
                live_send_output_solo_tracks: vec![vec![1, 2]],
                saved_live_send_output_solo_gains: vec![1.0],
                live_send_meter_solo_tracks: vec![vec![1, 2], vec![1, 2]],
                saved_live_send_meter_solo_gains: vec![1.0, 1.0],
                live_insert_enabled_tracks: vec![2],
                saved_live_insert_enabled: vec![true],
                live_send_output_paths: Some(vec![crate::MixerLiveSendOutputPath {
                    transform: MixerSourceRouteTransform::default(),
                    send_gains: vec![
                        crate::MixerLiveSendGain {
                            target_index: Some(0),
                            saved_gain: 0.5,
                            enabled_target_index: Some(0),
                            saved_enabled: true,
                        },
                        crate::MixerLiveSendGain {
                            target_index: Some(1),
                            saved_gain: 1.0,
                            enabled_target_index: Some(1),
                            saved_enabled: true,
                        },
                    ],
                    insert_enabled_gains: vec![crate::MixerLiveInsertEnabledGain {
                        target_index: Some(0),
                        saved_enabled: true,
                    }],
                }]),
                live_send_meter_paths: vec![
                    crate::MixerLiveSendMeterPath {
                        track: 1,
                        transform: MixerSourceRouteTransform::default(),
                        send_gains: Vec::new(),
                        insert_enabled_gains: Vec::new(),
                    },
                    crate::MixerLiveSendMeterPath {
                        track: 2,
                        transform: MixerSourceRouteTransform::default(),
                        send_gains: vec![crate::MixerLiveSendGain {
                            target_index: Some(0),
                            saved_gain: 0.5,
                            enabled_target_index: Some(0),
                            saved_enabled: true,
                        }],
                        insert_enabled_gains: vec![crate::MixerLiveInsertEnabledGain {
                            target_index: Some(0),
                            saved_enabled: true,
                        }],
                    },
                ],
                live_send_effect_input_paths: Vec::new(),
                live_solo: None,
            },
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, 0.25];
        let mut controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [0.5, 0.125]);

        meters.set_live_mixer_send_level_raw(1, 2, Some(12_800));
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] > 0.5 && frame[0] < 0.51);
        let mut destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert_eq!(destination_meter, [frame[0], frame[1]]);

        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!((frame[0] - 1.0).abs() < 0.001);
        assert!((frame[1] - 0.25).abs() < 0.001);

        meters.set_live_mixer_send_level_raw(2, 0, Some(6_400));
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 1.0 && frame[0] > 0.99);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!((destination_meter[0] - 1.0).abs() < 0.001);
        assert!((destination_meter[1] - 0.25).abs() < 0.001);

        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!((frame[0] - 0.5).abs() < 0.001);
        assert!((frame[1] - 0.125).abs() < 0.001);

        meters.set_live_mixer_send_enabled(1, 2, Some(false));
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 0.5 && frame[0] > 0.49);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!(destination_meter[0] < 1.0 && destination_meter[0] > 0.99);

        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0].abs() < 0.001);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!(destination_meter[0].abs() < 0.001);

        meters.set_live_mixer_send_enabled(1, 2, Some(true));
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] > 0.0 && frame[0] < 0.001);
        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!((frame[0] - 0.5).abs() < 0.001);

        meters.set_live_mixer_insert_enabled(2, Some(false));
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 0.5 && frame[0] > 0.49);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!(destination_meter[0] < 1.0 && destination_meter[0] > 0.99);
        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0].abs() < 0.001);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!(destination_meter[0].abs() < 0.001);

        meters.set_live_mixer_insert_enabled(2, Some(true));
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] > 0.0 && frame[0] < 0.001);
        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!((frame[0] - 0.5).abs() < 0.001);

        meters.set_live_mixer_soloed_tracks(&[3]);
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] < 0.5 && frame[0] > 0.49);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!(destination_meter[0] < 1.0 && destination_meter[0] > 0.99);
        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0].abs() < 0.001);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!(destination_meter[0].abs() < 0.001);

        meters.set_live_mixer_soloed_tracks(&[2]);
        controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] > 0.0 && frame[0] < 0.001);
        for _ in 0..100_000 {
            controls = source_control.next_frame_controls(&targets);
        }
        frame = [1.0, 0.25];
        source_control.apply_output_frame(controls, &mut frame);
        assert!((frame[0] - 0.5).abs() < 0.001);
        destination_meter = [1.0, 0.25];
        source_control.apply_meter_frame(
            controls,
            2,
            MixerSourceRouteTransform::default().scaled(0.5),
            &mut destination_meter,
        );
        assert!((destination_meter[0] - 1.0).abs() < 0.001);
    }

    #[test]
    fn cloned_live_mixer_sources_keep_independent_smoothing_state() {
        let source_control = test_live_mixer_source_control(
            1,
            1.0,
            None,
            None,
            MixerSourceRouteTransform::default(),
            Vec::new(),
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        let targets = source_control.targets(&meters).unwrap();
        let mut frame = [1.0, 1.0];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);

        meters.set_live_mixer_insert_fader_raw(1, Some(0));
        let controls = source_control.next_frame_controls(&targets);
        frame = [1.0, 1.0];
        source_control.apply_output_frame(controls, &mut frame);
        let cloned_control = source_control.clone();
        let cloned_targets = cloned_control.targets(&meters).unwrap();

        for _ in 0..100_000 {
            let controls = source_control.next_frame_controls(&targets);
            frame = [1.0, 1.0];
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0] < 0.001);

        let controls = cloned_control.next_frame_controls(&cloned_targets);
        let mut cloned_frame = [1.0, 1.0];
        cloned_control.apply_output_frame(controls, &mut cloned_frame);
        assert!(cloned_frame[0] > 0.99);
    }

    #[test]
    fn live_playlist_audio_clip_fader_smooths_changes_from_its_current_gain() {
        let source_control = test_live_mixer_source_control(
            1,
            1.0,
            None,
            None,
            MixerSourceRouteTransform::default(),
            Vec::new(),
            48_000,
        );
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        let targets = source_control.targets(&meters).unwrap();

        let mut frame = [1.0, 0.5];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [1.0, 0.5]);

        meters.set_live_mixer_insert_fader_raw(1, Some(0));
        frame = [1.0, 0.5];
        let controls = source_control.next_frame_controls(&targets);
        source_control.apply_output_frame(controls, &mut frame);
        let gain = frame[0];
        assert!(gain < 1.0 && gain > 0.99);
        assert_eq!(frame[1], frame[0] * 0.5);

        for _ in 0..100_000 {
            frame = [1.0, 0.5];
            let controls = source_control.next_frame_controls(&targets);
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0] < 0.001);
        assert_eq!(frame[1], frame[0] * 0.5);

        meters.set_live_mixer_insert_fader_raw(1, None);
        for _ in 0..100_000 {
            frame = [1.0, 0.5];
            let controls = source_control.next_frame_controls(&targets);
            source_control.apply_output_frame(controls, &mut frame);
        }
        assert!((frame[0] - 1.0).abs() < 0.001);
    }

    #[test]
    fn live_playlist_audio_clip_fader_scales_every_send_route() {
        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![1.0, 0.5], vec![0.25, -1.0]],
        };
        let saved_transform = MixerSourceRouteTransform::default().scaled(2.25);
        let clip = PreparedClip {
            clip_index: 0,
            playlist_track_id: None,
            channel_id: 1,
            mixer_track: Some(1),
            path: PathBuf::new(),
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 2 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 2,
            gain: 1.0,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: saved_transform,
            live_mixer_source_control: Some(test_live_mixer_source_control(
                1,
                1.5,
                None,
                None,
                MixerSourceRouteTransform::default().scaled(1.5),
                vec![
                    (1, MixerSourceRouteTransform::default()),
                    (2, MixerSourceRouteTransform::default().scaled(0.5)),
                ],
                48_000,
            )),
            mixer_meter_transforms: vec![
                (1, MixerSourceRouteTransform::default().scaled(1.5)),
                (2, MixerSourceRouteTransform::default().scaled(0.75)),
            ],
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
        };
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(5_000));
        let mut mix = vec![0.0; 4];
        let mut meter_buses = vec![0.0; MIXER_TRACK_METER_COUNT * mix.len()];

        mix_clip_window_into_stereo_with_quality_and_meter(
            &mut mix,
            0,
            &source,
            &clip,
            4,
            ResamplingQuality::Linear,
            ClipMixMonitor {
                cancelled: None,
                meter_buses: Some(&mut meter_buses),
                meter_stride_samples: 4,
                live_mixer_source_targets: Some(&meters),
            },
        )
        .unwrap();

        assert_eq!(mix, vec![0.75, 0.1875, 0.375, -0.75]);
        assert_eq!(&meter_buses[4..8], &[0.5, 0.125, 0.25, -0.5]);
        assert_eq!(&meter_buses[8..12], &[0.25, 0.0625, 0.125, -0.25]);

        let mut offline_mix = vec![0.0; 4];
        mix_clip_window_into_stereo_with_quality_and_meter(
            &mut offline_mix,
            0,
            &source,
            &clip,
            4,
            ResamplingQuality::Linear,
            ClipMixMonitor {
                cancelled: None,
                meter_buses: None,
                meter_stride_samples: 4,
                live_mixer_source_targets: None,
            },
        )
        .unwrap();
        assert_eq!(offline_mix, vec![2.25, 0.5625, 1.125, -2.25]);
    }

    #[test]
    fn live_playlist_audio_clip_pan_smooths_before_send_routes() {
        let source = DecodedAudio {
            sample_rate: 1_000,
            channels: vec![vec![1.0; 2], vec![1.0; 2]],
        };
        let meter_transforms = vec![
            (1, MixerSourceRouteTransform::default()),
            (2, MixerSourceRouteTransform::default().scaled(0.5)),
        ];
        let clip = PreparedClip {
            clip_index: 0,
            playlist_track_id: None,
            channel_id: 1,
            mixer_track: Some(1),
            path: PathBuf::new(),
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 2 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 2,
            gain: 1.0,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Triangular,
            mixer_transform: MixerSourceRouteTransform::default(),
            live_mixer_source_control: Some(test_live_mixer_source_control(
                1,
                1.0,
                Some(0),
                None,
                MixerSourceRouteTransform::default(),
                meter_transforms.clone(),
                1_000,
            )),
            mixer_meter_transforms: meter_transforms,
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
        };
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        meters.set_live_mixer_insert_pan_raw(1, Some(0));
        let mut initial_mix = vec![0.0; 2];
        let mut initial_meter_buses = vec![0.0; MIXER_TRACK_METER_COUNT * 2];
        mix_clip_window_into_stereo_with_quality_and_meter(
            &mut initial_mix,
            0,
            &source,
            &clip,
            1_000,
            ResamplingQuality::Linear,
            ClipMixMonitor {
                cancelled: None,
                meter_buses: Some(&mut initial_meter_buses),
                meter_stride_samples: 2,
                live_mixer_source_targets: Some(&meters),
            },
        )
        .unwrap();
        assert_eq!(initial_mix, vec![1.0, 1.0]);

        meters.set_live_mixer_insert_pan_raw(1, Some(6_400));
        let mut changed_mix = vec![0.0; 2];
        let mut changed_meter_buses = vec![0.0; MIXER_TRACK_METER_COUNT * 2];
        mix_clip_window_into_stereo_with_quality_and_meter(
            &mut changed_mix,
            1,
            &source,
            &clip,
            1_000,
            ResamplingQuality::Linear,
            ClipMixMonitor {
                cancelled: None,
                meter_buses: Some(&mut changed_meter_buses),
                meter_stride_samples: 2,
                live_mixer_source_targets: Some(&meters),
            },
        )
        .unwrap();

        assert!(changed_mix[0] < 1.0 && changed_mix[0] > 0.9);
        assert!(changed_mix[1] > 1.0 && changed_mix[1] < 1.1);
        assert!(changed_meter_buses[2] < 1.0 && changed_meter_buses[2] > 0.9);
        assert!(changed_meter_buses[3] > 1.0 && changed_meter_buses[3] < 1.1);
        assert!((changed_meter_buses[4] - changed_mix[0] * 0.5).abs() < 1e-6);
        assert!((changed_meter_buses[5] - changed_mix[1] * 0.5).abs() < 1e-6);
    }

    #[test]
    fn performance_audio_clip_smooths_live_fader_across_send_routes() {
        let path = PathBuf::from("performance-audio.wav");
        let audio_sources = vec![DecodedAudio {
            sample_rate: 1_000,
            channels: vec![vec![1.0; 64], vec![1.0; 64]],
        }];
        let cancelled = AtomicBool::new(false);
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        let no_arrangement_clips = [];
        let audio_context = PreparedAudioClipMixContext {
            clips: &no_arrangement_clips,
            audio_sources: &audio_sources,
            sample_rate: 1_000,
            resampling_quality: ResamplingQuality::Linear,
            cancelled: &cancelled,
            live_mixer_source_targets: Some(&meters),
        };
        let mixer_transform = MixerSourceRouteTransform::default().scaled(2.0);
        let meter_transforms = vec![
            (1, MixerSourceRouteTransform::default()),
            (2, MixerSourceRouteTransform::default().scaled(0.5)),
        ];
        let clip = PreparedClip {
            clip_index: 0,
            playlist_track_id: Some(1),
            channel_id: 1,
            mixer_track: Some(1),
            path,
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 64 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 50,
            gain: 1.0,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform,
            live_mixer_source_control: Some(test_live_mixer_source_control(
                1,
                1.0,
                None,
                None,
                MixerSourceRouteTransform::default(),
                meter_transforms.clone(),
                1_000,
            )),
            mixer_meter_transforms: meter_transforms,
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
        };
        let (launches, receiver) = std::sync::mpsc::channel();
        let (_routing_updates, routing_update_receiver) = std::sync::mpsc::channel();
        let (_latency_updates, latency_update_receiver) = std::sync::mpsc::channel();
        let (latency_report_sender, _latency_reports) = std::sync::mpsc::channel();
        let mut runtime = PlaylistPerformanceRuntime {
            performance_zone_end_tick: 0,
            start_frame: 0,
            sample_rate: 1_000,
            tempo_map: PlaylistTempoMap::constant(96, 120.0)
                .expect("constant tempo map should build"),
            launches: receiver,
            mixer_routing_updates: routing_update_receiver,
            latency_updates: latency_update_receiver,
            latency_reports: latency_report_sender,
            live_status: Arc::new(Mutex::new(PlaylistPerformanceLiveStatus::default())),
            current_document: performance_audio_clip_fixture(80, None),
            pending_commands: Vec::new(),
            queued_by_track: BTreeMap::new(),
            motion_by_track: HashMap::new(),
            motion_clip_order_by_track: HashMap::new(),
            motion_group_clip_by_index: HashMap::new(),
            press_by_track: HashMap::new(),
            position_sync_by_track: HashMap::new(),
            ppq: 96,
            initial_time_signature: None,
            signature_changes: Vec::new(),
            clips_by_index: HashMap::new(),
            audio_clips_by_index: HashMap::from([(
                0,
                PerformanceAudioClip {
                    track_id: 1,
                    source_start_tick: 0,
                    length_ticks: 384,
                    source_duration_frames: 50,
                    template: clip,
                },
            )]),
            vst3_clips_by_index: HashMap::new(),
            active_by_track: BTreeMap::new(),
            active_audio_by_track: BTreeMap::new(),
            active_vst3_by_track: BTreeMap::new(),
            active_audio_playbacks: Vec::new(),
        };
        launches
            .send(PlaylistPerformanceCommand::Launch(
                PlaylistPerformanceLaunch {
                    clip_index: 0,
                    start_tick: 0,
                    start_frame: 0,
                    position_offset_ticks: 0,
                },
            ))
            .expect("Performance Mode launch should queue");
        drop(launches);
        let sampler_sources = HashMap::new();
        let sampler_notes = [];
        let mut sampler_engine =
            SamplerVoiceEngine::new(&sampler_sources, &sampler_notes, 16, 1, 1_000);
        let mut initial_output = vec![0.0; 2];
        let mut initial_meter_buses = vec![0.0; MIXER_TRACK_METER_COUNT * 2];
        runtime
            .update(
                0..1,
                &mut sampler_engine,
                &audio_context,
                &mut initial_output,
                Some(&mut initial_meter_buses),
                None,
            )
            .expect("initial Performance Mode audio block should mix");
        assert_eq!(initial_output, vec![1.0, 1.0]);
        assert_eq!(&initial_meter_buses[4..6], &[0.5, 0.5]);

        meters.set_live_mixer_insert_fader_raw(1, Some(5_000));
        let mut changed_output = vec![0.0; 40];
        let mut changed_meter_buses = vec![0.0; MIXER_TRACK_METER_COUNT * changed_output.len()];
        runtime
            .update(
                1..21,
                &mut sampler_engine,
                &audio_context,
                &mut changed_output,
                Some(&mut changed_meter_buses),
                None,
            )
            .expect("updated Performance Mode audio block should mix");
        let first_changed_gain = changed_output[0];
        let final_changed_gain = changed_output[changed_output.len() - 2];
        assert!(first_changed_gain < 1.0 && first_changed_gain > 0.5);
        assert!(final_changed_gain < first_changed_gain && final_changed_gain > 0.5);
        let send_meter_offset = 2 * changed_output.len() + changed_output.len() - 2;
        assert!((changed_meter_buses[send_meter_offset] - final_changed_gain * 0.5).abs() < 1e-6);
    }

    #[test]
    fn performance_stream_smooths_live_master_fader_changes() {
        let meters = MixerTrackPeakMeters::default();
        let output = MixerMasterOutput::default().with_live_master_targets(&meters, 48_000);

        let mut frame = [1.0, 1.0];
        output.apply(&mut frame);
        assert_eq!(frame, [1.0, 1.0]);

        meters.set_live_master_fader_gain(0.25);
        frame = [1.0, 1.0];
        output.apply(&mut frame);
        assert!(frame[0] < 1.0 && frame[0] > 0.99);
        assert_eq!(frame[0], frame[1]);

        for _ in 0..100_000 {
            frame = [1.0, 1.0];
            output.apply(&mut frame);
        }
        assert!((frame[0] - 0.25).abs() < 0.001);
    }

    #[test]
    fn pattern_stream_smooths_live_master_fader_changes() {
        let meters = MixerTrackPeakMeters::default();
        let output = MixerMasterOutput::default().with_live_master_targets(&meters, 48_000);

        let mut frame = [1.0, 1.0];
        output.apply(&mut frame);
        assert_eq!(frame, [1.0, 1.0]);

        meters.set_live_master_fader_gain(0.25);
        frame = [1.0, 1.0];
        output.apply(&mut frame);
        assert!(frame[0] < 1.0 && frame[0] > 0.99);
        assert_eq!(frame[0], frame[1]);

        for _ in 0..100_000 {
            frame = [1.0, 1.0];
            output.apply(&mut frame);
        }
        assert!((frame[0] - 0.25).abs() < 0.001);
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
        assert!(audio_clip_matches_selection(Some(&[2]), 2));
        assert!(!audio_clip_matches_selection(Some(&[2]), 1));
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
        assert_eq!(map.seconds_at_fractional_tick(96.0), Ok(0.5));
        assert_eq!(map.seconds_at_fractional_tick(192.0), Ok(1.5));
        assert!((map.tick_at_seconds(1.0).unwrap() - 144.0).abs() < 1e-9);
        assert_eq!(map.tick_at_seconds(f64::NAN), None);
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
    fn performance_vst3_parameter_automation_rebases_offsets_and_repeats() {
        let automation = BTreeMap::from([(
            4,
            vec![Vst3PlaylistParameterAutomation {
                parameter_id: 21,
                points: vec![
                    Vst3PlaylistAutomationPoint {
                        tick: 24,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3PlaylistAutomationPoint {
                        tick: 48,
                        value: 0.8,
                        tension: 0.0,
                    },
                    Vst3PlaylistAutomationPoint {
                        tick: 120,
                        value: 0.4,
                        tension: 0.0,
                    },
                    Vst3PlaylistAutomationPoint {
                        tick: 192,
                        value: 0.6,
                        tension: 0.0,
                    },
                ],
            }],
        )]);

        let first_pass =
            performance_parameter_automation_points_for_loop(&automation, 500, 32, 160)
                .expect("the first launched pass should schedule");
        assert_eq!(
            first_pass[&(4, 21)],
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 500,
                    value: 0.4,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 516,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 588,
                    value: 0.4,
                    tension: 0.0,
                },
            ]
        );

        let repeated = performance_parameter_automation_points_for_loop(&automation, 660, 0, 192)
            .expect("the repeated pass should schedule");
        assert_eq!(
            repeated[&(4, 21)],
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 684,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 708,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 780,
                    value: 0.4,
                    tension: 0.0,
                },
            ]
        );
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
    fn wraps_playlist_remainder_onto_the_content_start_and_preserves_loop_length() {
        let mut spool = tempfile::tempfile().expect("temporary float spool should open");
        write_float_spool_block(
            &mut spool,
            &[1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0, 5.0, 50.0],
        )
        .expect("float samples should write");
        wrap_float_spool_remainder(&mut spool, 3, 5).expect("tail should wrap to the start");

        assert_eq!(
            spool.seek(SeekFrom::End(0)).unwrap(),
            5 * 2 * std::mem::size_of::<f32>() as u64,
            "the raw render keeps its original storage length"
        );
        spool.seek(SeekFrom::Start(0)).unwrap();
        let mut samples = Vec::new();
        read_float_spool_samples(&mut spool, 6, &mut samples).unwrap();
        assert_eq!(samples, [5.0, 50.0, 7.0, 70.0, 3.0, 30.0]);

        let mut multi_loop = tempfile::tempfile().expect("temporary float spool should open");
        write_float_spool_block(
            &mut multi_loop,
            &[1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0, 5.0, 50.0],
        )
        .expect("float samples should write");
        wrap_float_spool_remainder(&mut multi_loop, 2, 5)
            .expect("long tail should wrap over the full content more than once");
        multi_loop.seek(SeekFrom::Start(0)).unwrap();
        let mut samples = Vec::new();
        read_float_spool_samples(&mut multi_loop, 4, &mut samples).unwrap();
        assert_eq!(samples, [9.0, 90.0, 6.0, 60.0]);
    }

    #[test]
    fn leave_remainder_waits_for_continuous_quiet_after_the_minimum_tail() {
        let mut detector = PlaylistRemainderDetector::new(PlaylistRemainderStop {
            start_frame: 2,
            silence_frames: 4,
        });

        let (frames, stop) = detector.frames_to_keep(0, &[0.0, 0.0, 0.0, 0.0, 0.5, 0.5]);
        assert_eq!(frames, 3);
        assert!(!stop);

        let (frames, stop) = detector.frames_to_keep(3, &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(frames, 4);
        assert!(stop);
    }

    #[test]
    fn render_prefix_skip_spans_blocks_for_pdc_trim() {
        let first_block = [1.0, 1.0, 2.0, 2.0];
        let second_block = [3.0, 3.0, 4.0, 4.0];
        let mut frames_to_skip = 3;

        assert!(skip_render_block_prefix(&first_block, &mut frames_to_skip).is_empty());
        assert_eq!(frames_to_skip, 1);
        assert_eq!(
            skip_render_block_prefix(&second_block, &mut frames_to_skip),
            &[4.0, 4.0]
        );
        assert_eq!(frames_to_skip, 0);
    }

    #[test]
    fn performance_pdc_updates_retarget_direct_and_mixer_bus_delays() {
        let mut direct = crate::audio::StereoFrameDelay::new(2).unwrap();
        let mut mixer_buses = (0..3)
            .map(|_| crate::audio::StereoFrameDelay::new(2).unwrap())
            .collect::<Vec<_>>();

        update_live_source_pdc_delays(5, &mut direct, &mut mixer_buses, 3).unwrap();
        assert_eq!(direct.delay_frames(), 5);
        assert!(mixer_buses.iter().all(|delay| delay.delay_frames() == 5));

        update_live_source_pdc_delays(0, &mut direct, &mut mixer_buses, 3).unwrap();
        assert_eq!(direct.delay_frames(), 0);
        assert!(mixer_buses.iter().all(|delay| delay.delay_frames() == 0));

        update_live_source_pdc_delays(1, &mut direct, &mut mixer_buses, 1).unwrap();
        assert_eq!(direct.delay_frames(), 1);
        assert_eq!(mixer_buses.len(), 1);
        assert_eq!(mixer_buses[0].delay_frames(), 1);
    }

    #[test]
    fn playlist_wav_pdc_trim_is_enabled_by_default() {
        assert!(PlaylistRenderOptions::default().wav_trim_pdc_silence);
    }

    #[test]
    fn wav_tail_patch_updates_lengths_after_marker_chunks_are_written() {
        let output_path = std::env::temp_dir().join(format!(
            "flp-rebuild-tail-length-{}-{}.wav",
            std::process::id(),
            NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut temporary = TemporaryRenderFile::create(&output_path).unwrap();
        let file = temporary.file.as_mut().unwrap();
        write_wav_header(
            file,
            48_000,
            10,
            80,
            WavSampleFormat::Float32,
            WavChannelMode::Stereo,
        )
        .unwrap();
        file.write_all(&[0; 40]).unwrap();
        write_wav_cue_points(
            file,
            &[WavCuePoint {
                identifier: 1,
                sample_offset: 2,
                label: "Marker".to_owned(),
            }],
        )
        .unwrap();
        patch_wav_data_length(file, 5, WavSampleFormat::Float32, WavChannelMode::Stereo).unwrap();
        temporary.commit(&output_path).unwrap();

        let bytes = fs::read(&output_path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 40);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(&bytes[84..88], b"cue ");
        fs::remove_file(output_path).unwrap();
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
            source_paths: BTreeSet::from([path.clone()]),
            audio_sources: vec![DecodedAudio {
                sample_rate: 4,
                channels: vec![vec![1.0, 3.0], vec![2.0, 4.0]],
            }],
            clips: vec![PreparedClip {
                clip_index: 0,
                playlist_track_id: None,
                channel_id: 4,
                mixer_track: None,
                path,
                source_id: 0,
                start_frame: 0,
                source_bounds: SampleBounds { start: 0, end: 2 },
                reverse: false,
                pitch_ratio: 1.0,
                duration_frames: 2,
                gain: 0.5,
                envelope_gain: 1.0,
                fade_in_frames: 0,
                fade_out_frames: 0,
                fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
                fade_in_tension: 0,
                fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
                fade_out_tension: 0,
                pan: 0.0,
                pan_law: PanLaw::Circular,
                mixer_transform: MixerSourceRouteTransform::from_insert_transform(
                    MixerInsertSignalTransform::default().with_fader_volume_raw(Some(15_000)),
                ),
                live_mixer_source_control: None,
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
                mixer_effect_input_transforms: Vec::new(),
                mixer_route_override: Arc::new(ArcSwapOption::empty()),
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
            tempo_map: PlaylistTempoMap::constant(96, 120.0).unwrap(),
            pan_law: PanLaw::Circular,
            notes: vec![ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
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
                    volume_envelope_enabled: false,
                    volume_envelope: None,
                    max_polyphony_raw: None,
                    mono_enabled: false,
                    portamento_enabled: false,
                    polyphony_slide_raw: None,
                    channel_cut_groups: None,
                    fast_declick_for_cut_groups: false,
                    key_range: None,
                    velocity_range: None,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                    backward_loop: false,
                    release_loop_bounds: None,
                    release_ping_pong_loop: false,
                },
            )]),
            mixer_transforms_by_channel: HashMap::from([(
                2,
                MixerSourceRouteTransform::default().scaled(2.25),
            )]),
            live_mixer_source_routes_by_channel: HashMap::from([(
                2,
                MixerLiveSourceRoutePlan {
                    track: 1,
                    source_transform: MixerSourceRouteTransform::default(),
                    saved_fader_gain: 1.5,
                    saved_pan_raw: None,
                    saved_stereo_separation_raw: None,
                    saved_signal_flags: None,
                    saved_enabled: None,
                    pan_law_raw: Some(2),
                    output_transform: MixerSourceRouteTransform::default().scaled(1.5),
                    insert_meter_transforms: vec![
                        (1, MixerSourceRouteTransform::default()),
                        (2, MixerSourceRouteTransform::default().scaled(0.5)),
                    ],
                    ..MixerLiveSourceRoutePlan::default()
                },
            )]),
            mixer_meter_transforms_by_channel: HashMap::from([(
                2,
                vec![
                    (1, MixerSourceRouteTransform::default().scaled(1.5)),
                    (2, MixerSourceRouteTransform::default().scaled(0.75)),
                ],
            )]),
            mixer_effect_input_transforms_by_channel: HashMap::new(),
            pattern_clips_rendered: 1,
            pattern_clips_skipped_unsupported_scale: 0,
            source_paths: std::collections::BTreeSet::new(),
            unresolved_clip_indices: BTreeSet::new(),
        };
        let mut rendered = Vec::new();

        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(5_000));
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
                    wav_trim_pdc_silence: false,
                    tail_mode: PlaylistTailMode::FixedDuration,
                    tail_seconds: 0,
                    soloed_playlist_track_range: None,
                    read_sample_root_note: true,
                    clip_envelopes: Arc::new(Vec::new()),
                    clap_audio_layers: Arc::new(Vec::new()),
                },
                frames: 2,
                master_output: MixerMasterOutput::default(),
                vst3_processor: None,
                remainder_stop: None,
            },
            Some(&meters),
            &AtomicBool::new(false),
            || false,
            |block, _mixer_buses| {
                rendered.extend_from_slice(block);
                Ok(())
            },
        )
        .map(|result| result.voices_stolen)
        .unwrap();

        assert_eq!(voices_stolen, 0);
        assert_eq!(rendered, vec![2.25, 1.5, 3.75, 3.0]);
        assert_eq!(meters.take_peak(1), 2.5);
        assert_eq!(meters.take_peak(2), 1.25);
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
                target_gain: 1.0,
                target_pan_raw: 0,
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
                playlist_track_id: None,
                channel_id: u16::try_from(clip_index).unwrap(),
                mixer_track: Some(mixer_track),
                path: path.clone(),
                source_id: 0,
                start_frame,
                source_bounds: SampleBounds { start: 0, end: 8 },
                reverse: false,
                pitch_ratio: 1.0,
                duration_frames: 8,
                gain,
                envelope_gain: 1.0,
                fade_in_frames: 0,
                fade_out_frames: 0,
                fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
                fade_in_tension: 0,
                fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
                fade_out_tension: 0,
                pan: 0.0,
                pan_law: PanLaw::Circular,
                mixer_transform: transform,
                live_mixer_source_control: None,
                mixer_meter_transforms: vec![(usize::try_from(mixer_track).unwrap(), transform)],
                mixer_effect_input_transforms: Vec::new(),
                mixer_route_override: Arc::new(ArcSwapOption::empty()),
            }
        })
        .collect::<Vec<_>>();
        let audio_sources = vec![source];
        let cancelled = AtomicBool::new(false);
        let audio_mix_context = PreparedAudioClipMixContext {
            clips: &clips,
            audio_sources: &audio_sources,
            sample_rate: 8,
            resampling_quality: ResamplingQuality::Linear,
            cancelled: &cancelled,
            live_mixer_source_targets: None,
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
            let mut mixer =
                ParallelAudioClipMixer::new(scope, worker_clips, &audio_mix_context, true, false)
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
            playlist_track_id: None,
            channel_id: 0,
            mixer_track: None,
            path: PathBuf::new(),
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 2 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 4,
            gain: 1.0,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            live_mixer_source_control: None,
            mixer_meter_transforms: Vec::new(),
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
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
    fn playlist_audio_clip_gain_and_linear_fades_shape_each_mixed_frame() {
        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![1.0; 4]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            playlist_track_id: None,
            channel_id: 0,
            mixer_track: None,
            path: PathBuf::new(),
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 4 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 4,
            gain: 1.0,
            envelope_gain: 0.5,
            fade_in_frames: 2,
            fade_out_frames: 2,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            live_mixer_source_control: None,
            mixer_meter_transforms: Vec::new(),
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
        };
        let mut mix = vec![0.0; 8];

        mix_clip_into_stereo(&mut mix, &source, &clip, 4, None).unwrap();

        let expected_frame_gains = [0.0, 0.25, 0.25, 0.0];
        let center_gain = std::f32::consts::FRAC_1_SQRT_2;
        for (frame, expected_gain) in expected_frame_gains.into_iter().enumerate() {
            assert!((mix[frame * 2] - expected_gain * center_gain).abs() < 1.0e-6);
            assert!((mix[frame * 2 + 1] - expected_gain * center_gain).abs() < 1.0e-6);
        }
    }

    #[test]
    fn playlist_audio_clip_fade_shapes_follow_the_selected_curve_and_tension() {
        assert_eq!(
            playlist_audio_clip_fade_gain(0.25, PlaylistAudioClipFadeCurve::Linear, 0),
            0.25
        );
        assert_eq!(
            playlist_audio_clip_fade_gain(0.25, PlaylistAudioClipFadeCurve::Smooth, 0),
            0.15625
        );
        assert_eq!(
            playlist_audio_clip_fade_gain(0.5, PlaylistAudioClipFadeCurve::Single, 50),
            0.25
        );
        assert!(
            (playlist_audio_clip_fade_gain(0.5, PlaylistAudioClipFadeCurve::Single, -50)
                - std::f32::consts::FRAC_1_SQRT_2)
                .abs()
                < 1.0e-6
        );
        assert_eq!(
            playlist_audio_clip_fade_gain(2.0, PlaylistAudioClipFadeCurve::Linear, 0),
            1.0
        );
        assert_eq!(
            playlist_audio_clip_fade_gain(f32::NAN, PlaylistAudioClipFadeCurve::Linear, 0),
            0.0
        );
    }

    #[test]
    fn playlist_audio_clip_minus_infinity_gain_is_silence() {
        let envelope = PlaylistAudioClipEnvelope {
            arrangement_id: 0,
            clip_id: 1,
            gain_centidb: PLAYLIST_AUDIO_CLIP_GAIN_MINUS_INFINITY,
            fade_in_ms: 0,
            fade_out_ms: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            stretch_to_clip_duration: false,
            stretch_pitch_cents: 0,
            pan_override: None,
            reverse_override: None,
        };

        assert!(envelope.gain_centidb_is_supported());
        assert_eq!(envelope.linear_gain(), 0.0);
    }

    #[test]
    fn unstretched_playlist_audio_clip_master_pitch_changes_playback_rate_and_duration() {
        let pitch_up = audio_clip_pitch_ratio(1_200.0);
        let pitch_down = audio_clip_pitch_ratio(-1_200.0);
        let channel_fine_tune_up = audio_clip_pitch_ratio(50.0);
        assert_eq!(pitch_up, 2.0);
        assert_eq!(pitch_down, 0.5);
        assert_eq!(audio_clip_pitch_ratio(0.0), 1.0);
        assert!((channel_fine_tune_up - 2.0f64.powf(50.0 / 1_200.0)).abs() < 1e-12);
        assert_eq!(source_duration_to_frames(4, 4, 4, pitch_up).unwrap(), 2);
        assert_eq!(source_duration_to_frames(4, 4, 4, pitch_down).unwrap(), 8);

        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![0.0, 1.0, 2.0, 3.0]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            playlist_track_id: None,
            channel_id: 0,
            mixer_track: None,
            path: PathBuf::new(),
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 4 },
            reverse: false,
            pitch_ratio: pitch_up,
            duration_frames: 2,
            gain: 1.0,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            live_mixer_source_control: None,
            mixer_meter_transforms: Vec::new(),
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
        };
        let mut mix = vec![0.0; 4];
        mix_clip_into_stereo(&mut mix, &source, &clip, 4, None).unwrap();
        let center_gain = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(mix, vec![0.0, 0.0, 2.0 * center_gain, 2.0 * center_gain]);
    }

    #[test]
    fn stretched_playlist_audio_clip_combines_master_pitch_without_changing_duration() {
        let combined_pitch_cents =
            audio_clip_duration_preserving_pitch_cents(1_250.0, 200, 3).unwrap();
        assert_eq!(combined_pitch_cents, 1_450);
        let max_pitch_cents =
            audio_clip_duration_preserving_pitch_cents(-1_300.0, -1_200, 3).unwrap();
        assert_eq!(max_pitch_cents, -2_500);
        assert!(audio_clip_duration_preserving_pitch_cents(1_301.0, 1_200, 3).is_err());

        let audio = DecodedAudio {
            sample_rate: 8_000,
            channels: vec![
                (0..8_000)
                    .map(|frame| (std::f32::consts::TAU * frame as f32 / 80.0).sin())
                    .collect(),
            ],
        };
        let processed =
            process_audio_range_for_playback(&audio, 0..8_000, max_pitch_cents as f32 / 100.0, 0.5)
                .unwrap();
        assert_eq!(processed.frame_count(), 4_000);
    }

    #[test]
    fn channel_audio_clip_stretch_plan_maps_time_multiplier_and_pitch() {
        let tempo_map =
            PlaylistTempoMap::constant(96, 120.0).expect("constant tempo map should build");
        let generic = channel_audio_clip_stretch_plan(
            Some((192, 200, 0, 1)),
            &tempo_map,
            96,
            0,
            AudioSourceDimensions {
                frames: 8_000,
                channels: 1,
                sample_rate: 8_000,
            },
            3,
        )
        .unwrap();
        assert!(matches!(
            generic,
            Some(AudioClipStretchPlan::DurationPreserving {
                target_duration_frames: 4_000,
                pitch_raw: 200,
                mode_raw: 1,
            })
        ));

        let elastique_mono = channel_audio_clip_stretch_plan(
            Some((192, 200, 0, 2)),
            &tempo_map,
            96,
            0,
            AudioSourceDimensions {
                frames: 8_000,
                channels: 1,
                sample_rate: 8_000,
            },
            3,
        )
        .unwrap();
        assert!(matches!(
            elastique_mono,
            Some(AudioClipStretchPlan::DurationPreserving { mode_raw: 2, .. })
        ));
        assert!(
            channel_audio_clip_stretch_plan(
                Some((192, 200, 0, 2)),
                &tempo_map,
                96,
                0,
                AudioSourceDimensions {
                    frames: 8_000,
                    channels: 2,
                    sample_rate: 8_000,
                },
                3,
            )
            .unwrap_err()
            .contains("non-mono source")
        );

        let resample = channel_audio_clip_stretch_plan(
            Some((192, 1_200, 10_000, 0)),
            &tempo_map,
            96,
            0,
            AudioSourceDimensions {
                frames: 8_000,
                channels: 1,
                sample_rate: 8_000,
            },
            4,
        )
        .unwrap();
        assert!(matches!(
            resample,
            Some(AudioClipStretchPlan::Resample {
                playback_rate_ratio
            }) if (playback_rate_ratio - 2.0).abs() < 1e-12
        ));

        assert!(
            channel_audio_clip_stretch_plan(
                Some((0, 99_999, i32::MAX, 99)),
                &tempo_map,
                96,
                0,
                AudioSourceDimensions {
                    frames: 8_000,
                    channels: 1,
                    sample_rate: 8_000,
                },
                5,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            channel_audio_clip_stretch_plan(
                Some((192, 0, 0, 3)),
                &tempo_map,
                96,
                0,
                AudioSourceDimensions {
                    frames: 8_000,
                    channels: 1,
                    sample_rate: 8_000,
                },
                6,
            )
            .unwrap_err()
            .contains("unsupported channel stretch mode")
        );
    }

    #[test]
    fn playlist_instance_stretch_plan_targets_the_visible_clip_duration() {
        let tempo_map =
            PlaylistTempoMap::constant(96, 120.0).expect("constant tempo map should build");
        let source = AudioSourceDimensions {
            frames: 8_000,
            channels: 1,
            sample_rate: 8_000,
        };

        let resample =
            playlist_instance_audio_clip_stretch_plan(0, 0, &tempo_map, 0, 96, source, 1).unwrap();
        assert!(matches!(
            resample,
            AudioClipStretchPlan::Resample {
                playback_rate_ratio
            } if (playback_rate_ratio - 2.0).abs() < 1e-12
        ));

        for mode_raw in [-1, 1, 2] {
            let plan = playlist_instance_audio_clip_stretch_plan(
                mode_raw, 300, &tempo_map, 0, 96, source, 2,
            )
            .unwrap();
            assert!(matches!(
                plan,
                AudioClipStretchPlan::DurationPreserving {
                    target_duration_frames: 4_000,
                    pitch_raw: 300,
                    mode_raw: actual_mode,
                } if actual_mode == mode_raw
            ));
        }

        let stereo = AudioSourceDimensions {
            channels: 2,
            ..source
        };
        assert!(
            playlist_instance_audio_clip_stretch_plan(2, 0, &tempo_map, 0, 96, stereo, 3,)
                .unwrap_err()
                .contains("non-mono source")
        );
        assert!(
            playlist_instance_audio_clip_stretch_plan(1, 0, &tempo_map, 0, 48, source, 4,)
                .unwrap_err()
                .contains("0.5x to 2x")
        );
        assert!(
            playlist_instance_audio_clip_stretch_plan(1, 0, &tempo_map, u32::MAX, 1, source, 5,)
                .unwrap_err()
                .contains("overflows")
        );
    }

    #[test]
    fn channel_audio_clip_stretch_rejects_tempo_changes_inside_the_target_duration() {
        let tempo_map = PlaylistTempoMap::from_tempo_events(96, 120.0, &[(96, 100.0)])
            .expect("tempo automation map should build");

        let error = channel_audio_clip_stretch_plan(
            Some((384, 0, 0, -1)),
            &tempo_map,
            96,
            0,
            AudioSourceDimensions {
                frames: 16_000,
                channels: 1,
                sample_rate: 8_000,
            },
            7,
        )
        .unwrap_err();

        assert!(error.contains("crosses tempo automation"));
    }

    #[test]
    fn playback_audio_processor_returns_the_requested_stretched_duration() {
        let audio = DecodedAudio {
            sample_rate: 8_000,
            channels: vec![
                (0..8_000)
                    .map(|frame| (std::f32::consts::TAU * frame as f32 / 80.0).sin())
                    .collect(),
            ],
        };

        let processed = process_audio_range_for_playback(&audio, 0..8_000, 2.0, 0.5).unwrap();

        assert_eq!(processed.sample_rate, audio.sample_rate);
        assert_eq!(processed.channels.len(), 1);
        assert_eq!(processed.frame_count(), 4_000);
    }

    #[test]
    fn sampler_duration_preserving_stretch_keeps_target_frames_with_pitch_shift() {
        let audio = DecodedAudio {
            sample_rate: 8_000,
            channels: vec![
                (0..8_000)
                    .map(|frame| (std::f32::consts::TAU * frame as f32 / 80.0).sin())
                    .collect(),
            ],
        };
        let plan = SamplerDurationPreservingStretchPlan {
            target_ticks: 384.0,
            target_frames: 12_000,
            stretch_ratio: 1.5,
            pitch_raw: 700,
            tempo_bpm: 120.0,
        };

        let processed = process_sampler_duration_preserving_stretch(&audio, plan).unwrap();

        assert_eq!(processed.sample_rate, audio.sample_rate);
        assert_eq!(processed.channels.len(), 1);
        assert_eq!(processed.frame_count(), 12_000);
    }

    #[test]
    fn sampler_stretch_plan_maps_time_multiplier_pitch_and_supported_modes() {
        for mode_raw in [-1, 1, 2] {
            let plan = sampler_duration_preserving_stretch_plan_from_settings(
                7,
                SamplerDurationPreservingStretchSettings {
                    time_raw: 192,
                    pitch_raw: 700,
                    multiplier_raw: 20_000,
                    mode_raw,
                },
                16_000,
                1,
                8_000,
                96,
                120.0,
            )
            .unwrap()
            .expect("supported duration-preserving mode should produce a plan");
            assert!((plan.target_ticks - 384.0).abs() < 1e-12);
            assert_eq!(plan.target_frames, 16_000);
            assert!((plan.stretch_ratio - 1.0).abs() < 1e-12);
            assert_eq!(plan.pitch_raw, 700);
            assert_eq!(plan.tempo_bpm, 120.0);
        }

        assert!(
            sampler_duration_preserving_stretch_plan_from_settings(
                7,
                SamplerDurationPreservingStretchSettings {
                    time_raw: 192,
                    pitch_raw: 0,
                    multiplier_raw: 0,
                    mode_raw: 0,
                },
                8_000,
                1,
                8_000,
                96,
                120.0,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            sampler_duration_preserving_stretch_plan_from_settings(
                7,
                SamplerDurationPreservingStretchSettings {
                    time_raw: 192,
                    pitch_raw: 0,
                    multiplier_raw: 0,
                    mode_raw: 3,
                },
                8_000,
                1,
                8_000,
                96,
                120.0,
            )
            .unwrap_err()
            .contains("unsupported time-stretch mode")
        );
        assert!(
            sampler_duration_preserving_stretch_plan_from_settings(
                7,
                SamplerDurationPreservingStretchSettings {
                    time_raw: 192,
                    pitch_raw: 0,
                    multiplier_raw: 0,
                    mode_raw: 2,
                },
                8_000,
                2,
                8_000,
                96,
                120.0,
            )
            .unwrap_err()
            .contains("non-mono source")
        );
    }

    #[test]
    fn sampler_stretch_scales_sample_loop_points_to_the_new_source_length() {
        assert_eq!(
            scale_sampler_loop_points(Some((200, 600)), 800, 1_200),
            Some((300, 900))
        );
        assert_eq!(scale_sampler_loop_points(Some((2, 6)), 8, 4), Some((1, 3)));
        assert_eq!(scale_sampler_loop_points(Some((6, 8)), 8, 4), Some((3, 4)));
    }

    #[test]
    fn sampler_duration_stretch_checks_tempo_changes_only_while_the_voice_is_active() {
        let plan = SamplerDurationPreservingStretchPlan {
            target_ticks: 384.0,
            target_frames: 8_000,
            stretch_ratio: 1.0,
            pitch_raw: 0,
            tempo_bpm: 120.0,
        };
        let tempo_map = PlaylistTempoMap::from_tempo_events(96, 120.0, &[(192, 100.0)])
            .expect("tempo automation map should build");

        let error =
            validate_sampler_duration_stretch_tempo(plan, 3, &tempo_map, 0, Some(768), false)
                .unwrap_err();
        assert!(error.contains("across Playlist tempo automation"));

        let loop_tempo_map = PlaylistTempoMap::from_tempo_events(96, 120.0, &[(512, 100.0)])
            .expect("loop tempo automation map should build");

        assert!(
            validate_sampler_duration_stretch_tempo(plan, 3, &tempo_map, 0, Some(96), false)
                .is_ok()
        );

        let change_before_note_off =
            validate_sampler_duration_stretch_tempo(plan, 3, &tempo_map, 0, Some(288), false)
                .unwrap_err();
        assert!(change_before_note_off.contains("across Playlist tempo automation"));

        assert!(
            validate_sampler_duration_stretch_tempo(plan, 3, &loop_tempo_map, 0, Some(96), true)
                .is_ok()
        );

        let loop_error =
            validate_sampler_duration_stretch_tempo(plan, 3, &loop_tempo_map, 0, Some(768), true)
                .unwrap_err();
        assert!(loop_error.contains("across Playlist tempo automation"));

        let held_loop_error =
            validate_sampler_duration_stretch_tempo(plan, 3, &loop_tempo_map, 0, None, true)
                .unwrap_err();
        assert!(held_loop_error.contains("across Playlist tempo automation"));

        assert!(
            validate_sampler_duration_stretch_tempo(plan, 3, &loop_tempo_map, 0, Some(768), false)
                .is_ok()
        );

        let changed_start_error =
            validate_sampler_duration_stretch_tempo(plan, 3, &tempo_map, 384, None, false)
                .unwrap_err();
        assert!(changed_start_error.contains("note starts at 100.000 BPM"));
    }

    #[test]
    fn mixer_insert_transform_swaps_and_reverses_audio_clip_frames() {
        let source = DecodedAudio {
            sample_rate: 4,
            channels: vec![vec![0.25], vec![0.75]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            playlist_track_id: None,
            channel_id: 1,
            mixer_track: None,
            path: PathBuf::new(),
            source_id: 0,
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 1 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 1,
            gain: 1.0,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.0,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::from_insert_transform(
                MixerInsertSignalTransform::new(true, true),
            ),
            live_mixer_source_control: None,
            mixer_meter_transforms: Vec::new(),
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
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
            playlist_track_id: None,
            channel_id: 3,
            mixer_track: None,
            path: path.clone(),
            source_id: 0,
            start_frame: 2,
            source_bounds: SampleBounds { start: 0, end: 5 },
            reverse: false,
            pitch_ratio: 1.0,
            duration_frames: 5,
            gain: 0.5,
            envelope_gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            fade_in_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_in_tension: 0,
            fade_out_curve: PlaylistAudioClipFadeCurve::Linear,
            fade_out_tension: 0,
            pan: 0.5,
            pan_law: PanLaw::Circular,
            mixer_transform: MixerSourceRouteTransform::default(),
            live_mixer_source_control: None,
            mixer_meter_transforms: Vec::new(),
            mixer_effect_input_transforms: Vec::new(),
            mixer_route_override: Arc::new(ArcSwapOption::empty()),
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
            source_paths: BTreeSet::from([path]),
            audio_sources: vec![source.clone()],
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
    fn sampler_volume_envelope_applies_sustain_for_zero_time_stages() {
        let gain = |sustain, amount| {
            sampler_volume_envelope_sustain_gain_from_raw(
                true,
                Some(100),
                Some(100),
                Some(100),
                Some(100),
                Some(sustain),
                Some(amount),
            )
        };

        assert_eq!(gain(64, 128), 0.5);
        assert_eq!(gain(64, -128), 1.5);
        assert_eq!(gain(64, 0), 1.0);
        assert_eq!(gain(128, 128), 1.0);
        assert_eq!(gain(0, 64), 0.5);
    }

    #[test]
    fn sampler_envelope_time_candidate_maps_zero_and_maximum_raw_values() {
        assert_eq!(sampler_envelope_time_frames(100, 100), Some(0));
        assert_eq!(sampler_envelope_time_frames(65_536, 100), Some(3_600));
        assert_eq!(sampler_envelope_time_frames(100, 0), None);
        assert_eq!(sampler_envelope_time_frames(99, 100), None);
    }

    #[test]
    fn sampler_volume_envelope_interpolates_linear_pahds_stages_and_amount() {
        let envelope = SamplerVolumeEnvelope {
            predelay_frames: 2,
            attack_frames: 4,
            hold_frames: 2,
            decay_frames: 4,
            release_frames: 4,
            sustain_level: 0.25,
            amount: 0.5,
        };
        let gain = |frame| envelope.gain_at_frame(5, frame);

        assert_eq!(gain(5), 0.5);
        assert_eq!(gain(7), 0.5);
        assert_eq!(gain(8), 0.625);
        assert_eq!(gain(11), 1.0);
        assert_eq!(gain(12), 1.0);
        assert_eq!(gain(13), 1.0);
        assert_eq!(gain(14), 0.90625);
        assert_eq!(gain(17), 0.625);
    }

    #[test]
    fn sampler_volume_envelope_release_uses_saved_release_duration() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 4,
                channels: vec![vec![1.0; 32]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: true,
            volume_envelope: Some(SamplerVolumeEnvelope {
                predelay_frames: 0,
                attack_frames: 4,
                hold_frames: 0,
                decay_frames: 0,
                release_frames: 4,
                sustain_level: 1.0,
                amount: 1.0,
            }),
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([(7, source)]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(5),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 2, 4);
        let mut output = vec![0.0; 20];
        engine.render_block(0, 10, &mut output);

        let left = (0..10).map(|frame| output[frame * 2]).collect::<Vec<_>>();
        assert_eq!(left[0], 0.0);
        assert_eq!(left[1], 0.25);
        assert_eq!(left[2], 0.5);
        assert_eq!(left[3], 0.75);
        assert_eq!(left[4], 1.0);
        assert_eq!(left[5], 1.0);
        assert!((left[6] - 2.0 / 3.0).abs() < 1.0e-6);
        assert!((left[7] - 1.0 / 3.0).abs() < 1.0e-6);
        assert_eq!(left[8], 0.0);
        assert_eq!(left[9], 0.0);
    }

    #[test]
    fn sampler_volume_envelope_ignores_disabled_incomplete_or_unsupported_stages() {
        assert_eq!(
            sampler_volume_envelope_sustain_gain_from_raw(
                false,
                Some(100),
                Some(100),
                Some(100),
                Some(100),
                Some(0),
                Some(128),
            ),
            1.0
        );
        assert_eq!(
            sampler_volume_envelope_sustain_gain_from_raw(
                true,
                Some(101),
                Some(100),
                Some(100),
                Some(100),
                Some(0),
                Some(128),
            ),
            1.0
        );
        assert_eq!(
            sampler_volume_envelope_sustain_gain_from_raw(
                true,
                Some(100),
                Some(100),
                Some(100),
                Some(100),
                Some(129),
                Some(128),
            ),
            1.0
        );
        assert_eq!(
            sampler_volume_envelope_sustain_gain_from_raw(
                true,
                Some(100),
                None,
                Some(100),
                Some(100),
                Some(0),
                Some(128),
            ),
            1.0
        );
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
        assert_eq!(sampler_note_pan_value(0.0, 64.0), 0.0);
        assert_eq!(sampler_note_pan_value(0.0, 0.0), -1.0);
        assert_eq!(sampler_note_pan_value(0.0, 128.0), 1.0);
        assert_eq!(sampler_note_pan_value(-0.5, 96.0), 0.0);
        assert_eq!(sampler_note_pan_value(0.5, 128.0), 1.0);
        assert_eq!(sampler_note_pan_value(-0.5, 0.0), -1.0);
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
                volume_envelope_enabled: true,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 1,
            stop_frame: Some(3),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
    fn sampler_without_volume_envelope_plays_to_sample_end_after_note_off() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 8,
                    channels: vec![vec![1.0; 8]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(2),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 8);
        let mut output = vec![0.0; 16];

        engine.render_block(0, 8, &mut output);

        assert_eq!(
            output,
            vec![
                1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0
            ]
        );
    }

    #[test]
    fn sampler_without_volume_envelope_exits_sustain_loop_after_note_off() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 8,
                    channels: vec![(0..6).map(|frame| frame as f32).collect()],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: Some(SampleBounds { start: 1, end: 3 }),
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(4),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 8);
        let mut output = vec![0.0; 12];

        engine.render_block(0, 6, &mut output);

        assert_eq!(
            output,
            vec![0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0]
        );
    }

    #[test]
    fn sampler_precomputed_fades_process_sample_data_before_playback() {
        let source = Arc::new(DecodedAudio {
            sample_rate: 5,
            channels: vec![vec![1.0; 5], vec![-1.0; 5]],
        });
        let faded = sampler_precompute_fades(&source, Some(1024), None);

        assert_eq!(faded.channels[0], [0.0, 0.25, 0.5, 0.75, 1.0]);
        assert_eq!(faded.channels[1], [0.0, -0.25, -0.5, -0.75, -1.0]);
        assert_eq!(source.channels[0], [1.0; 5]);
        assert!(Arc::ptr_eq(
            &source,
            &sampler_precompute_fades(&source, Some(0), Some(1025))
        ));
    }

    #[test]
    fn sampler_loop_seek_warmup_preserves_the_current_voice_sample_position() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 8,
                    channels: vec![(0..16).map(|frame| frame as f32 / 16.0).collect()],
                }),
                gain: 1.0,
                pan: 0.0,
                reverse: false,
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: Some(10),
            start_frame: 1,
            stop_frame: Some(10),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];

        let mut complete_engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 8);
        let mut complete = vec![0.0; 24];
        complete_engine.render_block(0, 12, &mut complete);

        let mut seek_engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 8);
        let mut warmup = vec![0.0; 10];
        seek_engine.render_block(0, 5, &mut warmup);
        let mut sought_frame = [0.0; 2];
        seek_engine.render_block(5, 1, &mut sought_frame);

        assert_eq!(sought_frame, [complete[10], complete[11]]);
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 7,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 96,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 4,
                stop_frame: None,
                channel_id: 7,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 32,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
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
    fn sampler_layer_live_volume_expression_and_pan_scale_existing_and_new_voices_by_midi_channel()
    {
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let mut engine = SamplerVoiceEngine::new(&sources, &[], 2, 4, 4);
        let slot_index = engine
            .start_voice(ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 7,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            })
            .expect("live voice should start");
        engine.set_voice_input_midi_channel(slot_index, 2);
        apply_live_layer_sampler_controller(&mut engine, 2, 7, 64);

        let mut output = [0.0; 12];
        engine.render_block(0, 1, &mut output[..2]);
        let full_pan_gain = std::f32::consts::FRAC_1_SQRT_2;
        let channel_volume_gain = full_pan_gain * (64.0 / 127.0);
        assert!((output[0] - channel_volume_gain).abs() < 1e-6);
        assert!((output[1] - channel_volume_gain).abs() < 1e-6);

        apply_live_layer_sampler_controller(&mut engine, 2, 11, 64);
        engine.render_block(1, 1, &mut output[2..4]);
        let combined_gain = channel_volume_gain * (64.0 / 127.0);
        assert!((output[2] - combined_gain).abs() < 1e-6);
        assert!((output[3] - combined_gain).abs() < 1e-6);

        apply_live_layer_sampler_controller(&mut engine, 2, 7, 0);
        engine.render_block(2, 1, &mut output[4..6]);
        assert_eq!(&output[4..6], &[0.0, 0.0]);

        reset_live_layer_sampler_controllers(&mut engine, 2, 64, 0);
        engine.render_block(3, 1, &mut output[6..]);
        assert!((output[6] - full_pan_gain).abs() < 1e-6);
        assert!((output[7] - full_pan_gain).abs() < 1e-6);

        engine.apply_live_channel_volume_14bit(2, 8_192);
        engine.apply_live_expression_14bit(2, 4_096);
        engine.render_block(4, 1, &mut output[8..]);
        let high_resolution_gain = full_pan_gain * (8_192.0 / 16_383.0) * (4_096.0 / 16_383.0);
        assert!((output[8] - high_resolution_gain).abs() < 1e-6);
        assert!((output[9] - high_resolution_gain).abs() < 1e-6);

        engine.apply_live_pan_14bit(2, 8_193);
        engine.render_block(5, 1, &mut output[10..]);
        let pan_position = 8_193.0 / 128.0;
        let effective_pan = pan_position / 64.0 - 1.0;
        let angle = (effective_pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let controller_gain = (8_192.0 / 16_383.0) * (4_096.0 / 16_383.0);
        assert!(
            (output[10] - controller_gain * angle.cos()).abs() < 1e-6,
            "left output {} did not match expected {}",
            output[10],
            controller_gain * angle.cos()
        );
        assert!(
            (output[11] - controller_gain * angle.sin()).abs() < 1e-6,
            "right output {} did not match expected {}",
            output[11],
            controller_gain * angle.sin()
        );

        let new_slot_index = engine
            .start_voice(ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 6,
                stop_frame: None,
                channel_id: 7,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: engine.live_midi_pan(2),
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            })
            .expect("second live voice should start");
        engine.set_voice_input_midi_channel(new_slot_index, 2);
        engine.refresh_live_pan_for_channel(2);
        let expected_pan = 8_193.0 / 128.0;
        assert!(
            (engine.voices[new_slot_index]
                .as_ref()
                .expect("second live voice should remain active")
                .note_pan_value
                - expected_pan)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn sampler_live_sostenuto_captures_only_voices_active_at_pedal_down() {
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let mut engine = SamplerVoiceEngine::new(&sources, &[], 3, 4, 4);
        let start_voice = |key| ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        };
        let captured_slot = engine
            .start_voice(start_voice(SAMPLER_ROOT_KEY))
            .expect("first live voice should start");
        engine.set_voice_input_midi_channel(captured_slot, 3);
        engine.set_live_sostenuto(3, true);
        engine.release_note(7, SAMPLER_ROOT_KEY, 3);
        assert!(
            engine.voices[captured_slot]
                .as_ref()
                .is_some_and(|voice| voice.key_released
                    && voice.sostenuto_held
                    && !voice.release_started)
        );

        let uncaptured_slot = engine
            .start_voice(start_voice(SAMPLER_ROOT_KEY + 2))
            .expect("second live voice should start");
        engine.set_voice_input_midi_channel(uncaptured_slot, 3);
        engine.release_note(7, SAMPLER_ROOT_KEY + 2, 3);
        assert!(
            engine.voices[uncaptured_slot]
                .as_ref()
                .is_some_and(|voice| voice.key_released
                    && !voice.sostenuto_held
                    && voice.release_started)
        );

        engine.set_live_sostenuto(3, false);
        assert!(
            engine.voices[captured_slot]
                .as_ref()
                .is_some_and(|voice| !voice.sostenuto_held && voice.release_started)
        );
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
                volume_envelope_enabled: true,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let transforms = HashMap::from([(
            7,
            MixerSourceRouteTransform::from_insert_transform(MixerInsertSignalTransform::new(
                true, false,
            )),
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        engine.mixer_transforms_by_channel = Some(&transforms);
        let mut output = vec![0.0; 2];

        engine.render_block(0, 1, &mut output);

        assert_eq!(output, vec![-0.5, 0.0]);
    }

    #[test]
    fn sampler_voice_multiplies_the_layer_crossfade_gain() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![1.0]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 0.5,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 2];

        engine.render_block(0, 1, &mut output);

        assert_eq!(output, vec![0.5, 0.0]);
    }

    #[test]
    fn sampler_voice_applies_layer_pan_after_child_channel_pan() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![1.0]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 0.5,
            layer_pan_raw: 6_400,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 2];

        engine.render_block(0, 1, &mut output);

        let expected = 0.5 / std::f32::consts::SQRT_2;
        assert!((output[0] - expected).abs() < 1e-6);
        assert!((output[1] - expected).abs() < 1e-6);
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: Some(SampleBounds { start: 1, end: 4 }),
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: Some(SampleBounds { start: 1, end: 4 }),
                ping_pong_loop: true,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
    fn held_sampler_voice_repeats_wave_loop_points_backward() {
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: Some(SampleBounds { start: 1, end: 4 }),
                ping_pong_loop: false,
                backward_loop: true,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 20];
        engine.render_block(0, 10, &mut output);

        assert_eq!(
            output,
            vec![
                0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0, 2.0, 0.0, 1.0, 0.0, 3.0, 0.0, 2.0, 0.0,
                1.0, 0.0, 3.0, 0.0,
            ]
        );
    }

    #[test]
    fn sampler_voice_switches_to_the_release_loop_when_the_note_ends() {
        let sources = HashMap::from([(
            7,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                volume_envelope_enabled: true,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: Some(SampleBounds { start: 2, end: 4 }),
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(2),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 6, 4);
        let mut output = vec![0.0; 16];
        engine.render_block(0, 8, &mut output);

        assert_eq!(
            output,
            vec![
                0.0,
                0.0,
                1.0,
                0.0,
                2.0,
                0.0,
                2.5,
                0.0,
                4.0 / 3.0,
                0.0,
                1.5,
                0.0,
                2.0 / 3.0,
                0.0,
                0.5,
                0.0,
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 2,
            key: SAMPLER_ROOT_KEY + 12,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: 48,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 2,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
    fn sampler_source_honors_inclusive_sample_key_and_velocity_ranges() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1,
                channels: vec![vec![0.0]],
            }),
            gain: 1.0,
            pan: 0.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: Some((48, 72)),
            velocity_range: Some((32, 96)),
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };

        assert!(source.accepts_note(48, 32));
        assert!(source.accepts_note(72, 96));
        assert!(!source.accepts_note(47, 64));
        assert!(!source.accepts_note(73, 64));
        assert!(!source.accepts_note(60, 31));
        assert!(!source.accepts_note(60, 97));
        assert!(!source.accepts_note(128, 64));
    }

    #[test]
    fn sampler_metadata_gain_converts_decibels_to_linear_amplitude() {
        assert_eq!(sampler_metadata_gain(None), 1.0);
        assert_eq!(sampler_metadata_gain(Some(0)), 1.0);
        assert!((sampler_metadata_gain(Some(-6)) - 10.0f32.powf(-6.0 / 20.0)).abs() < 1e-6);
        assert_eq!(sampler_metadata_gain(Some(20)), 10.0);
        assert!(sampler_metadata_gain(Some(i16::MAX)).is_finite());
    }

    #[test]
    fn sampler_precomputed_fades_attenuate_source_endpoints() {
        assert_eq!(sampler_precomputed_fade_gain(0.0, 5, None, None), 1.0);
        assert_eq!(sampler_precomputed_fade_gain(0.0, 5, Some(0), Some(0)), 1.0);
        assert_eq!(sampler_precomputed_fade_gain(0.0, 5, Some(1024), None), 0.0);
        assert_eq!(sampler_precomputed_fade_gain(4.0, 5, Some(1024), None), 1.0);
        assert_eq!(sampler_precomputed_fade_gain(0.0, 5, None, Some(1024)), 1.0);
        assert_eq!(sampler_precomputed_fade_gain(4.0, 5, None, Some(1024)), 0.0);
        assert!((sampler_precomputed_fade_gain(2.0, 5, Some(1024), None) - 0.5).abs() < 1e-6);
        assert!((sampler_precomputed_fade_gain(2.0, 5, None, Some(1024)) - 0.5).abs() < 1e-6);
        assert_eq!(sampler_precomputed_fade_gain(2.0, 5, Some(1025), None), 1.0);
        assert_eq!(
            sampler_precomputed_fade_gain(0.0, 1, Some(1024), Some(1024)),
            1.0
        );
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 0,
            stop_frame: None,
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 50,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);

        engine.render_block(0, 1, &mut [0.0; 2]);

        let voice = engine.voices[0].as_ref().expect("note should start");
        assert!((voice.source_step - 2.0f64.powf(50.0 / 1_200.0)).abs() < 1e-12);

        engine.set_voice_input_midi_channel(0, 2);
        engine.apply_live_pitch_bend(3, 2400);
        assert!(
            (engine.voices[0].as_ref().unwrap().source_step - 2.0f64.powf(50.0 / 1200.0)).abs()
                < 1e-12
        );
        engine.apply_live_pitch_bend(2, 2400);
        assert!(
            (engine.voices[0].as_ref().unwrap().source_step - 4.0 * 2.0f64.powf(50.0 / 1200.0))
                .abs()
                < 1e-12
        );
        engine.voices[0].as_mut().unwrap().source_step *= -1.0;
        engine.apply_live_pitch_bend(2, 0);
        assert!(
            (engine.voices[0].as_ref().unwrap().source_step + 2.0f64.powf(50.0 / 1200.0)).abs()
                < 1e-12
        );
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
                volume_envelope_enabled: false,
                volume_envelope: None,
                max_polyphony_raw: None,
                mono_enabled: false,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: 2,
            stop_frame: None,
            channel_id: 3,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
                    volume_envelope_enabled: false,
                    volume_envelope: None,
                    max_polyphony_raw: None,
                    mono_enabled: false,
                    portamento_enabled: false,
                    polyphony_slide_raw: None,
                    channel_cut_groups: None,
                    fast_declick_for_cut_groups: false,
                    key_range: None,
                    velocity_range: None,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                    backward_loop: false,
                    release_loop_bounds: None,
                    release_ping_pong_loop: false,
                },
            ),
            (
                2,
                SamplerVoiceSource {
                    audio: source_b,
                    gain: 1.0,
                    pan: -1.0,
                    reverse: false,
                    volume_envelope_enabled: false,
                    volume_envelope: None,
                    max_polyphony_raw: None,
                    mono_enabled: false,
                    portamento_enabled: false,
                    polyphony_slide_raw: None,
                    channel_cut_groups: None,
                    fast_declick_for_cut_groups: false,
                    key_range: None,
                    velocity_range: None,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                    backward_loop: false,
                    release_loop_bounds: None,
                    release_ping_pong_loop: false,
                },
            ),
        ]);
        let notes = [
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: Some(12),
                channel_id: 1,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 1,
                stop_frame: Some(12),
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
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
    fn sampler_max_polyphony_stops_the_oldest_active_voice_on_its_channel() {
        let sources = HashMap::from([
            (
                1,
                SamplerVoiceSource {
                    audio: Arc::new(DecodedAudio {
                        sample_rate: 4,
                        channels: vec![vec![1.0, 3.0, 5.0, 7.0]],
                    }),
                    gain: 1.0,
                    pan: -1.0,
                    reverse: false,
                    volume_envelope_enabled: false,
                    volume_envelope: None,
                    max_polyphony_raw: Some(1),
                    mono_enabled: false,
                    portamento_enabled: false,
                    polyphony_slide_raw: None,
                    channel_cut_groups: None,
                    fast_declick_for_cut_groups: false,
                    key_range: None,
                    velocity_range: None,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                    backward_loop: false,
                    release_loop_bounds: None,
                    release_ping_pong_loop: false,
                },
            ),
            (
                2,
                SamplerVoiceSource {
                    audio: Arc::new(DecodedAudio {
                        sample_rate: 4,
                        channels: vec![vec![10.0; 4]],
                    }),
                    gain: 1.0,
                    pan: -1.0,
                    reverse: false,
                    volume_envelope_enabled: false,
                    volume_envelope: None,
                    max_polyphony_raw: None,
                    mono_enabled: false,
                    portamento_enabled: false,
                    polyphony_slide_raw: None,
                    channel_cut_groups: None,
                    fast_declick_for_cut_groups: false,
                    key_range: None,
                    velocity_range: None,
                    root_key: SAMPLER_ROOT_KEY,
                    fine_tune_cents: 0.0,
                    loop_bounds: None,
                    ping_pong_loop: false,
                    backward_loop: false,
                    release_loop_bounds: None,
                    release_ping_pong_loop: false,
                },
            ),
        ]);
        let notes = [
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 1,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 1,
                stop_frame: None,
                channel_id: 1,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 4);
        let mut output = vec![0.0; 6];
        engine.render_block(0, 3, &mut output);

        assert_eq!(engine.voices_stolen, 1);
        assert_eq!(output[0], 11.0);
        assert_eq!(output[2], 11.0);
        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .filter(|voice| voice.channel_id == 1)
                .count(),
            1
        );
        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .filter(|voice| voice.channel_id == 2)
                .count(),
            1
        );
    }

    #[test]
    fn sampler_mono_releases_the_previous_note_and_allows_its_tail_to_overlap() {
        let sources = HashMap::from([(
            1,
            SamplerVoiceSource {
                audio: Arc::new(DecodedAudio {
                    sample_rate: 4,
                    channels: vec![vec![1.0; 8]],
                }),
                gain: 1.0,
                pan: -1.0,
                reverse: false,
                volume_envelope_enabled: true,
                volume_envelope: None,
                max_polyphony_raw: Some(1),
                mono_enabled: true,
                portamento_enabled: false,
                polyphony_slide_raw: None,
                channel_cut_groups: None,
                fast_declick_for_cut_groups: false,
                key_range: None,
                velocity_range: None,
                root_key: SAMPLER_ROOT_KEY,
                fine_tune_cents: 0.0,
                loop_bounds: None,
                ping_pong_loop: false,
                backward_loop: false,
                release_loop_bounds: None,
                release_ping_pong_loop: false,
            },
        )]);
        let notes = [
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 1,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 1,
                stop_frame: None,
                channel_id: 1,
                key: SAMPLER_ROOT_KEY + 1,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 4);
        let mut output = vec![0.0; 4];
        engine.render_block(0, 2, &mut output);

        assert_eq!(engine.voices_stolen, 0);
        assert_eq!(output[0], 1.0);
        assert_eq!(output[2], 2.0);
        let released_voice = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.started_frame == 0)
            .expect("the previous note should still be in its release tail");
        assert!(released_voice.release_started);
        assert_eq!(released_voice.release_remaining, Some(3));
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.started_frame == 1 && !voice.release_started)
        );
    }

    #[test]
    fn sampler_cut_groups_stop_matching_channels_and_leave_other_groups_alone() {
        let make_source = |sample: f32, groups| SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![sample; 16]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: groups,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([
            (1, make_source(1.0, Some((0, 7)))),
            (2, make_source(10.0, Some((7, 0)))),
            (3, make_source(3.0, Some((0, 8)))),
        ]);
        let notes = [1, 3, 2].map(|channel_id| ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: if channel_id == 2 { 1 } else { 0 },
            stop_frame: None,
            channel_id,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        });
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 1_000);
        let mut output = vec![0.0; 6];
        engine.render_block(0, 3, &mut output);

        assert_eq!(output[0], 4.0);
        assert_eq!(output[2], 13.0);
        assert!(
            !engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.channel_id == 1)
        );
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.channel_id == 2)
        );
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.channel_id == 3)
        );
    }

    #[test]
    fn fast_declick_for_cut_groups_fades_then_stops_the_previous_sample() {
        let make_source = |sample: f32, groups, fast_declick_for_cut_groups| SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![sample; 32]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: groups,
            fast_declick_for_cut_groups,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([
            (1, make_source(1.0, Some((0, 7)), false)),
            (2, make_source(10.0, Some((7, 0)), true)),
        ]);
        let notes = [1, 2].map(|channel_id| ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: None,
            start_frame: u64::from(channel_id == 2),
            stop_frame: None,
            channel_id,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note: false,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        });
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 1_000);
        let mut initial = vec![0.0; 4];
        engine.render_block(0, 2, &mut initial);

        assert_eq!(initial[0], 1.0);
        assert_eq!(initial[2], 11.0);
        let fading_voice = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.channel_id == 1)
            .expect("fast declick should retain the voice during its fade");
        assert_eq!(fading_voice.release_remaining, Some(9));
        assert!(fading_voice.release_cosine);

        let mut fade_tail = vec![0.0; 18];
        engine.render_block(2, 9, &mut fade_tail);
        assert!(
            !engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.channel_id == 1)
        );
        let mut after_fade = vec![0.0; 2];
        engine.render_block(11, 1, &mut after_fade);
        assert_eq!(after_fade, [10.0, 0.0]);
    }

    #[test]
    fn sampler_slide_notes_glide_overlapping_notes_in_their_midi_color_group() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![1.0; 64]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([(1, source)]);
        let make_note =
            |start_frame, stop_frame, key, midi_channel, fine_pitch_offset_cents, slide_note| {
                ScheduledSamplerNote {
                    clip_index: 0,
                    playlist_track_id: None,
                    start_tick: start_frame,
                    stop_tick: stop_frame,
                    start_frame,
                    stop_frame,
                    channel_id: 1,
                    key,
                    midi_channel,
                    slide_note,
                    slide_duration_frames: 0,
                    velocity: 127,
                    pan: 64,
                    fine_pitch_offset_cents,
                    layer_gain: 1.0,
                    layer_pan_raw: 0,
                }
            };
        let notes = [
            make_note(0, Some(30), SAMPLER_ROOT_KEY, 0, 0, false),
            make_note(0, Some(30), SAMPLER_ROOT_KEY + 4, 0, 20, false),
            make_note(0, Some(30), SAMPLER_ROOT_KEY + 7, 1, 0, false),
            make_note(2, Some(12), SAMPLER_ROOT_KEY + 12, 0, -30, true),
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 4, 4, 1_000);

        engine.render_block(0, 2, &mut [0.0; 4]);
        assert_eq!(engine.voices.iter().flatten().count(), 3);
        engine.render_block(2, 5, &mut [0.0; 10]);

        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .filter(|voice| voice.midi_channel == 0)
                .map(|voice| voice.pitch_offset_cents)
                .collect::<Vec<_>>(),
            [300.0, 300.0]
        );
        let other_color_voice = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.midi_channel == 1)
            .expect("a different note color should keep playing");
        assert_eq!(other_color_voice.pitch_offset_cents, 0.0);

        engine.render_block(7, 6, &mut [0.0; 12]);
        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .filter(|voice| voice.midi_channel == 0)
                .map(|voice| (voice.pitch_offset_cents, voice.note_slide.is_none()))
                .collect::<Vec<_>>(),
            [(750.0, true), (750.0, true)]
        );
        assert_eq!(engine.voices.iter().flatten().count(), 3);
    }

    #[test]
    fn sampler_sequential_slide_notes_target_their_piano_roll_pitch() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![1.0; 256]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([(1, source)]);
        let note = |start_frame, stop_frame, key, slide_note| ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: start_frame,
            stop_tick: stop_frame,
            start_frame,
            stop_frame,
            channel_id: 1,
            key,
            midi_channel: 0,
            slide_note,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        };
        let notes = [
            note(0, None, SAMPLER_ROOT_KEY, false),
            note(2, Some(4), SAMPLER_ROOT_KEY + 12, true),
            note(5, Some(7), SAMPLER_ROOT_KEY + 7, true),
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 2, 4, 1_000);

        engine.render_block(0, 5, &mut [0.0; 10]);
        assert_eq!(
            engine.voices[0].as_ref().unwrap().pitch_offset_cents,
            1_200.0
        );
        engine.render_block(5, 3, &mut [0.0; 6]);

        assert_eq!(engine.voices[0].as_ref().unwrap().pitch_offset_cents, 700.0);
        assert!(engine.voices[0].as_ref().unwrap().note_slide.is_none());
    }

    #[test]
    fn sampler_slide_notes_interpolate_velocity_and_pan() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![1.0; 256]],
            }),
            gain: 1.0,
            pan: 0.75,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([(1, source)]);
        let note = |start_frame, stop_frame, velocity, pan, slide_note| ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: start_frame,
            stop_tick: stop_frame,
            start_frame,
            stop_frame,
            channel_id: 1,
            key: SAMPLER_ROOT_KEY,
            midi_channel: 0,
            slide_note,
            slide_duration_frames: 0,
            velocity,
            pan,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        };
        let notes = [
            note(0, None, 127, 128, false),
            note(2, Some(6), 0, 64, true),
        ];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 2, 4, 1_000);

        engine.render_block(0, 4, &mut [0.0; 8]);
        let voice = engine.voices[0].as_ref().unwrap();
        assert!((voice.velocity_gain - 0.75).abs() < 1e-6);
        assert!((voice.note_pan_value - 112.0).abs() < 1e-6);
        assert!(
            (sampler_note_pan_value(voice.channel_pan, voice.note_pan_value) - 1.0).abs() < 1e-6
        );

        engine.render_block(4, 3, &mut [0.0; 6]);
        let voice = engine.voices[0].as_ref().unwrap();
        assert_eq!(voice.velocity_gain, 0.0);
        assert_eq!(voice.note_pan_value, 64.0);
        assert_eq!(
            sampler_note_pan_value(voice.channel_pan, voice.note_pan_value),
            0.75
        );
        let expected_pan = channel_pan_gains(0.75, true, PanLaw::Circular);
        assert!((voice.left_gain - expected_pan.0).abs() < 1e-6);
        assert!((voice.right_gain - expected_pan.1).abs() < 1e-6);
        assert!(voice.note_slide.is_none());
    }

    #[test]
    fn sampler_downward_slide_extends_one_shot_render_duration() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![1.0; 64]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([(1, source)]);
        let notes = [
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 0,
                stop_tick: None,
                start_frame: 0,
                stop_frame: None,
                channel_id: 1,
                key: SAMPLER_ROOT_KEY,
                midi_channel: 0,
                slide_note: false,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            ScheduledSamplerNote {
                clip_index: 0,
                playlist_track_id: None,
                start_tick: 2,
                stop_tick: Some(4),
                start_frame: 2,
                stop_frame: Some(4),
                channel_id: 1,
                key: SAMPLER_ROOT_KEY - 12,
                midi_channel: 0,
                slide_note: true,
                slide_duration_frames: 0,
                velocity: 127,
                pan: 64,
                fine_pitch_offset_cents: 0,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
        ];
        let render_end_frame = sampler_voice_render_end_frame(
            &notes,
            &sources,
            2,
            4,
            1_000,
            ResamplingQuality::Linear,
            PanLaw::Circular,
        )
        .unwrap();
        assert!(render_end_frame > 64);

        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 2, 4, 1_000);
        engine.render_block(0, 64, &mut vec![0.0; 128]);
        assert!(engine.voices.iter().any(Option::is_some));
        let remaining_frames = usize::try_from(render_end_frame - 64).unwrap();
        engine.render_block(64, remaining_frames, &mut vec![0.0; remaining_frames * 2]);
        assert!(engine.voices.iter().all(Option::is_none));
    }

    #[test]
    fn sampler_slide_note_without_an_overlapping_voice_is_silent() {
        let source = SamplerVoiceSource {
            audio: Arc::new(DecodedAudio {
                sample_rate: 1_000,
                channels: vec![vec![1.0; 16]],
            }),
            gain: 1.0,
            pan: -1.0,
            reverse: false,
            volume_envelope_enabled: false,
            volume_envelope: None,
            max_polyphony_raw: None,
            mono_enabled: false,
            portamento_enabled: false,
            polyphony_slide_raw: None,
            channel_cut_groups: None,
            fast_declick_for_cut_groups: false,
            key_range: None,
            velocity_range: None,
            root_key: SAMPLER_ROOT_KEY,
            fine_tune_cents: 0.0,
            loop_bounds: None,
            ping_pong_loop: false,
            backward_loop: false,
            release_loop_bounds: None,
            release_ping_pong_loop: false,
        };
        let sources = HashMap::from([(1, source)]);
        let notes = [ScheduledSamplerNote {
            clip_index: 0,
            playlist_track_id: None,
            start_tick: 0,
            stop_tick: Some(4),
            start_frame: 0,
            stop_frame: Some(4),
            channel_id: 1,
            key: SAMPLER_ROOT_KEY + 7,
            midi_channel: 0,
            slide_note: true,
            slide_duration_frames: 0,
            velocity: 127,
            pan: 64,
            fine_pitch_offset_cents: 0,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 1_000);
        let mut output = [0.0; 8];

        engine.render_block(0, 4, &mut output);

        assert_eq!(output, [0.0; 8]);
        assert!(engine.voices.iter().all(Option::is_none));
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
    fn mixer_stem_block_reads_master_and_selects_post_effect_or_current_bus() {
        let master = [0.25_f32, -0.25, 0.5, -0.5];
        let mut pre_effect_buses = vec![0.0_f32; MIXER_TRACK_METER_COUNT * master.len()];
        let mut post_effect_buses = vec![0.0_f32; MIXER_TRACK_METER_COUNT * master.len()];
        pre_effect_buses[master.len()..2 * master.len()]
            .copy_from_slice(&[0.125, -0.125, 0.75, -0.75]);
        post_effect_buses[master.len()..2 * master.len()]
            .copy_from_slice(&[0.25, -0.25, 0.5, -0.5]);

        assert_eq!(
            mixer_stem_block(0, &master, &pre_effect_buses, &post_effect_buses, false).unwrap(),
            master.as_slice()
        );
        assert_eq!(
            mixer_stem_block(1, &master, &pre_effect_buses, &post_effect_buses, false).unwrap(),
            &[0.25, -0.25, 0.5, -0.5][..]
        );
        assert_eq!(
            mixer_stem_block(1, &master, &pre_effect_buses, &post_effect_buses, true).unwrap(),
            &[0.125, -0.125, 0.75, -0.75][..]
        );
        assert!(
            mixer_stem_block(
                MIXER_TRACK_METER_COUNT,
                &master,
                &pre_effect_buses,
                &post_effect_buses,
                false,
            )
            .is_err()
        );
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
