//! Offline rendering for Playlist clips that reference FLP audio channels.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::audio::StreamingAudioWriter;
use crate::media::{DecodedAudio, SamplePathResolver, decode_audio_file};
use crate::{FlpDocument, PlaylistClip, PlaylistClipTarget};

const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const MAX_MIX_BYTES: usize = 512 * 1024 * 1024;
const MAX_SOURCE_CACHE_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_SAMPLER_VOICE_LIMIT: usize = 64;
const MAX_SAMPLER_VOICE_LIMIT: usize = 256;
const SAMPLER_RELEASE_SECONDS: f64 = 0.005;
const SAMPLER_ROOT_KEY: u16 = 60;
const SAMPLER_BLOCK_FRAMES: usize = 1_024;
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioClipRenderOptions {
    pub arrangement_id: u16,
    pub sample_rate: u32,
}

impl Default for AudioClipRenderOptions {
    fn default() -> Self {
        Self {
            arrangement_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplerPatternRenderOptions {
    pub pattern_id: u16,
    pub sample_rate: u32,
    pub voice_limit: usize,
}

impl Default for SamplerPatternRenderOptions {
    fn default() -> Self {
        Self {
            pattern_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
            voice_limit: DEFAULT_SAMPLER_VOICE_LIMIT,
        }
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

/// Render audio-channel Playlist clips into a stereo 32-bit-float WAV.
///
/// This early render path uses the project's base tempo, clip positions, observed audio source
/// offsets, and decoded audio-channel volume/pan. Clips with default `-1` offsets use the full
/// source file. The raw volume/pan mapping is provisional and has not been compared against
/// native FL Studio output. Pattern instruments, tempo automation, plug-ins, and Mixer effects
/// are not rendered.
pub fn render_audio_clips_to_wav(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    output_path: impl AsRef<Path>,
) -> Result<AudioClipRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path(project_path, output_path)?;
    let (mix, summary) = render_audio_clips_to_stereo_buffer(document, project_path, options)?;
    write_float_stereo_wav(output_path, &mix, options.sample_rate, summary.frames)?;
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

/// Render the sample voices in one FLP pattern to a stereo 32-bit-float WAV.
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
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path(project_path, output_path)?;
    let (mix, summary) = render_sampler_pattern_to_stereo_buffer(document, project_path, options)?;
    write_float_stereo_wav(output_path, &mix, options.sample_rate, summary.frames)?;
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

fn stream_prepared_sampler_pattern(
    render: &PreparedSamplerPattern,
    cancelled: Option<&AtomicBool>,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<SamplerPatternRenderSummary, String> {
    let release_frames = (f64::from(render.summary.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as usize;
    let mut engine = SamplerVoiceEngine::new(
        &render.sources_by_channel,
        &render.notes,
        render.voice_limit,
        release_frames,
        render.summary.sample_rate,
    );
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
            mix_clip_window_into_stereo(
                &mut block,
                block_start,
                audio,
                clip,
                output_rate,
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
    let render = prepare_audio_clip_render(document, project_path, options, cancelled)?;
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
        mix_clip_into_stereo(&mut mix, audio, clip, options.sample_rate, cancelled).map_err(
            |error| {
                format!(
                    "could not mix Playlist clip {} from audio channel {}: {error}",
                    clip.clip_index, clip.channel_id
                )
            },
        )?;
    }

    Ok((mix, render.summary))
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
    let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == options.arrangement_id)
        .ok_or_else(|| format!("arrangement {} was not found", options.arrangement_id))?;
    let channels = document.channels();
    let resolver = SamplePathResolver::new(project_path);

    let mut max_tick = 0u64;
    let mut clips_skipped_unsupported_scale = 0usize;
    let mut candidate_clips = Vec::<(usize, &PlaylistClip, u16, PathBuf, f32, f32)>::new();
    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        check_cancelled(cancelled)?;
        max_tick = max_tick.max(u64::from(clip.position_ticks) + u64::from(clip.length_ticks));
        let PlaylistClipTarget::Channel { id } = clip.target() else {
            continue;
        };
        let Some(channel) = channels.iter().find(|channel| channel.id() == id) else {
            continue;
        };
        if channel.kind() != Some(4) || channel.enabled() == Some(false) {
            continue;
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            clips_skipped_unsupported_scale += 1;
            continue;
        }
        let sample_path = channel.sample_path().ok_or_else(|| {
            format!("audio channel {id} has no decoded sample path (Playlist clip {clip_index})")
        })?;
        let resolved_path = resolver.resolve(sample_path).map_err(|error| {
            format!("could not resolve audio channel {id} for Playlist clip {clip_index}: {error}")
        })?;
        let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
        candidate_clips.push((clip_index, clip, id, resolved_path, gain, pan));
    }
    if candidate_clips.is_empty() {
        return Err(
            "arrangement contains no enabled Playlist clips targeting audio channels".to_owned(),
        );
    }

    let timeline_frames = ticks_to_frames(max_tick, ppq, tempo_bpm, options.sample_rate)?;
    let mut decoded_by_path = HashMap::<PathBuf, DecodedAudio>::new();
    let mut cached_source_bytes = 0usize;
    for (_, _, _, path, _, _) in &candidate_clips {
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
    for (clip_index, clip, channel_id, path, gain, pan) in &candidate_clips {
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
        let source_bounds = sample_source_bounds(clip, audio, *clip_index)?;
        let start_frame = ticks_to_frames(
            u64::from(clip.position_ticks),
            ppq,
            tempo_bpm,
            options.sample_rate,
        )?;
        let duration_frames = source_duration_to_frames(
            source_bounds.end - source_bounds.start,
            audio.sample_rate,
            options.sample_rate,
        )?;
        let end_frame = start_frame
            .checked_add(duration_frames)
            .ok_or_else(|| "render timeline length overflow".to_owned())?;
        output_frames = output_frames.max(end_frame);
        clips.push(PreparedClip {
            clip_index: *clip_index,
            channel_id: *channel_id,
            path: path.clone(),
            start_frame,
            source_bounds,
            duration_frames,
            gain: *gain,
            pan: *pan,
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
        decoded_by_path,
        clips,
    })
}

struct PreparedSamplerPattern {
    summary: SamplerPatternRenderSummary,
    notes: Vec<ScheduledSamplerNote>,
    sources_by_channel: HashMap<u16, SamplerVoiceSource>,
    voice_limit: usize,
}

#[derive(Clone)]
struct SamplerVoiceSource {
    audio: Arc<DecodedAudio>,
    gain: f32,
    pan: f32,
}

#[derive(Clone, Copy)]
struct ScheduledSamplerNote {
    start_frame: u64,
    stop_frame: Option<u64>,
    channel_id: u16,
    key: u16,
    velocity: u8,
}

fn prepare_sampler_pattern(
    document: &FlpDocument,
    project_path: &Path,
    options: SamplerPatternRenderOptions,
    cancelled: Option<&AtomicBool>,
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
    let channels_by_id: HashMap<_, _> = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel))
        .collect();
    let sampler_note_count = pattern
        .notes
        .iter()
        .filter(|note| {
            channels_by_id.get(&note.channel_id).is_some_and(|channel| {
                channel.kind() == Some(0) && channel.enabled() != Some(false)
            })
        })
        .count();
    if sampler_note_count == 0 {
        return Err(format!(
            "pattern {} contains no notes on enabled Sampler channels",
            options.pattern_id
        ));
    }

    let resolver = SamplePathResolver::new(project_path);
    let mut decoded_by_path = HashMap::<PathBuf, Arc<DecodedAudio>>::new();
    let mut sources_by_channel = HashMap::<u16, SamplerVoiceSource>::new();
    let mut unresolved_sample_channels = std::collections::BTreeSet::new();
    let mut notes = Vec::with_capacity(sampler_note_count);
    let mut skipped_unresolved = 0usize;
    let mut cached_source_bytes = 0usize;
    let mut output_frames = 0u64;
    let release_frames = (f64::from(options.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as u64;

    for note in &pattern.notes {
        check_cancelled(cancelled)?;
        let Some(channel) = channels_by_id.get(&note.channel_id) else {
            continue;
        };
        if channel.kind() != Some(0) || channel.enabled() == Some(false) {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(source_entry) =
            sources_by_channel.entry(note.channel_id)
        {
            let Some(sample_path) = channel.sample_path() else {
                unresolved_sample_channels.insert(note.channel_id);
                skipped_unresolved += 1;
                continue;
            };
            let resolved_path = match resolver.resolve(sample_path) {
                Ok(path) => path,
                Err(_) => {
                    unresolved_sample_channels.insert(note.channel_id);
                    skipped_unresolved += 1;
                    continue;
                }
            };
            let audio = if let Some(audio) = decoded_by_path.get(&resolved_path) {
                Arc::clone(audio)
            } else {
                let audio = decode_audio_file(&resolved_path).map_err(|error| {
                    format!(
                        "could not decode sample for Sampler channel {} at {}: {error}",
                        note.channel_id,
                        resolved_path.display()
                    )
                })?;
                if !(1..=2).contains(&audio.channels.len()) {
                    return Err(format!(
                        "Sampler channel {} uses a {}-channel sample; only mono and stereo are supported by this renderer",
                        note.channel_id,
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
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            source_entry.insert(SamplerVoiceSource { audio, gain, pan });
        }
        let Some(source) = sources_by_channel.get(&note.channel_id) else {
            continue;
        };
        let start_frame = ticks_to_frames(
            u64::from(note.position),
            ppq,
            tempo_bpm,
            options.sample_rate,
        )?;
        // Reading the source here also validates that each prepared channel has usable frames.
        if source.audio.frame_count() == 0 {
            return Err(format!(
                "Sampler channel {} references an empty sample",
                note.channel_id
            ));
        }
        let stop_frame = if note.length == 0 {
            None
        } else {
            let end_tick = u64::from(note.position)
                .checked_add(u64::from(note.length))
                .ok_or_else(|| "Sampler note end position overflow".to_owned())?;
            Some(
                ticks_to_frames(end_tick, ppq, tempo_bpm, options.sample_rate)?
                    .max(start_frame.saturating_add(1)),
            )
        };
        let natural_duration = (source.audio.frame_count() as f64
            / sampler_source_step(source.audio.sample_rate, options.sample_rate, note.key))
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
            channel_id: note.channel_id,
            key: note.key,
            velocity: note.velocity,
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
        notes,
        sources_by_channel,
        voice_limit: options.voice_limit,
    })
}

struct SamplerVoiceEngine<'a> {
    sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
    notes: &'a [ScheduledSamplerNote],
    next_note: usize,
    voices: Vec<Option<SamplerVoice>>,
    release_frames: usize,
    output_sample_rate: u32,
    voices_stolen: usize,
}

struct SamplerVoice {
    source: Arc<DecodedAudio>,
    source_position: f64,
    source_step: f64,
    stop_frame: Option<u64>,
    started_frame: u64,
    gain: f32,
    left_gain: f32,
    right_gain: f32,
    release_remaining: Option<usize>,
}

impl<'a> SamplerVoiceEngine<'a> {
    fn new(
        sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
        notes: &'a [ScheduledSamplerNote],
        voice_limit: usize,
        release_frames: usize,
        output_sample_rate: u32,
    ) -> Self {
        Self {
            sources_by_channel,
            notes,
            next_note: 0,
            voices: std::iter::repeat_with(|| None).take(voice_limit).collect(),
            release_frames,
            output_sample_rate,
            voices_stolen: 0,
        }
    }

    fn render_block(&mut self, block_start: u64, frame_count: usize, output: &mut [f32]) {
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
                self.start_voice(note);
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
                let source_index = voice.source_position.floor() as usize;
                if source_index >= source_frames {
                    *voice_slot = None;
                    continue;
                }
                let next_index = source_index.saturating_add(1).min(source_frames - 1);
                let fraction = (voice.source_position - source_index as f64) as f32;
                let left = interpolate_sample_at(
                    &voice.source.channels[0],
                    source_index,
                    next_index,
                    fraction,
                );
                let right = if voice.source.channels.len() == 1 {
                    left
                } else {
                    interpolate_sample_at(
                        &voice.source.channels[1],
                        source_index,
                        next_index,
                        fraction,
                    )
                };
                let gain = voice.gain * fade;
                output_frame[0] += left * gain * voice.left_gain;
                output_frame[1] += right * gain * voice.right_gain;
                voice.source_position += voice.source_step;
                if let Some(remaining) = voice.release_remaining.as_mut() {
                    *remaining = remaining.saturating_sub(1);
                    if *remaining == 0 || voice.source_position >= source_frames as f64 {
                        *voice_slot = None;
                    }
                } else if voice.source_position >= source_frames as f64 {
                    *voice_slot = None;
                }
            }
        }
    }

    fn start_voice(&mut self, note: ScheduledSamplerNote) {
        let Some(source) = self.sources_by_channel.get(&note.channel_id) else {
            return;
        };
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
        let source_step =
            sampler_source_step(source.audio.sample_rate, self.output_sample_rate, note.key);
        let (left_gain, right_gain) =
            sampler_pan_gains(source.pan, source.audio.channels.len() == 1);
        let velocity_gain = f32::from(note.velocity.min(127)) / 127.0;
        self.voices[slot_index] = Some(SamplerVoice {
            source: Arc::clone(&source.audio),
            source_position: 0.0,
            source_step,
            stop_frame: note.stop_frame,
            started_frame: note.start_frame,
            gain: source.gain * velocity_gain,
            left_gain,
            right_gain,
            release_remaining: None,
        });
    }
}

fn sampler_pan_gains(pan: f32, mono: bool) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if mono {
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        (angle.cos(), angle.sin())
    } else if pan < 0.0 {
        (1.0, 1.0 + pan)
    } else {
        (1.0 - pan, 1.0)
    }
}

fn sampler_source_step(source_rate: u32, output_rate: u32, key: u16) -> f64 {
    let semitones = (i32::from(key) - i32::from(SAMPLER_ROOT_KEY)).clamp(-48, 48);
    f64::from(source_rate) / f64::from(output_rate.max(1))
        * 2.0f64.powf(f64::from(semitones) / 12.0)
}

fn interpolate_sample_at(samples: &[f32], first: usize, second: usize, fraction: f32) -> f32 {
    let first = samples[first];
    let second = samples[second];
    let first = if first.is_finite() { first } else { 0.0 };
    let second = if second.is_finite() { second } else { 0.0 };
    first + (second - first) * fraction
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
    path: PathBuf,
    start_frame: u64,
    source_bounds: SampleBounds,
    duration_frames: u64,
    gain: f32,
    pan: f32,
}

struct PreparedAudioClipRender {
    summary: AudioClipRenderSummary,
    decoded_by_path: HashMap<PathBuf, DecodedAudio>,
    clips: Vec<PreparedClip>,
}

const STREAM_BLOCK_FRAMES: usize = 1024;

fn sample_source_bounds(
    clip: &PlaylistClip,
    audio: &DecodedAudio,
    clip_index: usize,
) -> Result<SampleBounds, String> {
    let frames = audio.frame_count();
    if clip.start_offset == -1.0 && clip.end_offset == -1.0 {
        if frames == 0 {
            return Err(format!("Playlist clip {clip_index} references empty audio"));
        }
        return Ok(SampleBounds {
            start: 0,
            end: frames,
        });
    }
    if !clip.start_offset.is_finite()
        || !clip.end_offset.is_finite()
        || clip.start_offset < 0.0
        || clip.end_offset <= clip.start_offset
    {
        return Err(format!(
            "Playlist clip {clip_index} has unsupported audio source offsets {}..{} ms",
            clip.start_offset, clip.end_offset
        ));
    }
    let to_frame = |milliseconds: f32| -> Result<usize, String> {
        let frame = f64::from(milliseconds) * f64::from(audio.sample_rate) / 1000.0;
        if !frame.is_finite() || frame < 0.0 || frame > usize::MAX as f64 {
            return Err(format!(
                "Playlist clip {clip_index} audio offset is out of range"
            ));
        }
        Ok(frame.round() as usize)
    };
    let start = to_frame(clip.start_offset)?.min(frames);
    let end = to_frame(clip.end_offset)?.min(frames);
    if start >= end {
        return Err(format!(
            "Playlist clip {clip_index} source window {}..{} ms is outside its sample",
            clip.start_offset, clip.end_offset
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
) -> Result<u64, String> {
    if source_rate == 0 || output_rate == 0 {
        return Err("audio sample rate must be greater than zero".to_owned());
    }
    let duration = source_frames as f64 * f64::from(output_rate) / f64::from(source_rate);
    if !duration.is_finite() || duration < 0.0 || duration > u64::MAX as f64 {
        return Err("audio clip duration is outside the renderable range".to_owned());
    }
    Ok(duration.ceil() as u64)
}

fn channel_gain_pan(volume: Option<u32>, pan: Option<i32>) -> (f32, f32) {
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

fn mix_clip_into_stereo(
    mix: &mut [f32],
    source: &DecodedAudio,
    clip: &PreparedClip,
    output_rate: u32,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
    mix_clip_window_into_stereo(mix, 0, source, clip, output_rate, cancelled)
}

fn mix_clip_window_into_stereo(
    mix: &mut [f32],
    mix_start_frame: u64,
    source: &DecodedAudio,
    clip: &PreparedClip,
    output_rate: u32,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
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
    let overlap_start = clip.start_frame.max(mix_start_frame);
    let overlap_end = end_frame.min(mix_end_frame);
    if overlap_start >= overlap_end {
        return Ok(());
    }
    let pan = clip.pan.clamp(-1.0, 1.0);
    let left_gain = (1.0 - pan.max(0.0)) * clip.gain;
    let right_gain = (1.0 + pan.min(0.0)) * clip.gain;
    let source_frames_per_output = f64::from(source.sample_rate) / f64::from(output_rate);
    for output_frame in overlap_start..overlap_end {
        let output_offset = output_frame - clip.start_frame;
        if output_offset.is_multiple_of(16_384) {
            check_cancelled(cancelled)?;
        }
        let position = (clip.source_bounds.start as f64
            + output_offset as f64 * source_frames_per_output)
            .min((clip.source_bounds.end - 1) as f64);
        let left = interpolate_sample(&source.channels[0], position);
        let right = if source.channels.len() == 1 {
            left
        } else {
            interpolate_sample(&source.channels[1], position)
        };
        let output_index = usize::try_from(output_frame - mix_start_frame)
            .map_err(|_| "audio output frame index exceeds this platform".to_owned())?
            * 2;
        mix[output_index] += left * left_gain;
        mix[output_index + 1] += right * right_gain;
    }
    Ok(())
}

fn interpolate_sample(channel: &[f32], position: f64) -> f32 {
    let first_index = (position.floor() as usize).min(channel.len() - 1);
    let second_index = (first_index + 1).min(channel.len() - 1);
    let fraction = (position - first_index as f64) as f32;
    channel[first_index] + (channel[second_index] - channel[first_index]) * fraction
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
            .ok_or_else(|| "output WAV path must include a file name".to_owned())?;
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
        return Err("output WAV path must not overwrite the FLP project".to_owned());
    }
    Ok(())
}

fn write_float_stereo_wav(
    output_path: &Path,
    samples: &[f32],
    sample_rate: u32,
    frames: u64,
) -> Result<(), String> {
    let frames =
        u32::try_from(frames).map_err(|_| "render is too long for a RIFF/WAVE file".to_owned())?;
    let data_bytes = frames
        .checked_mul(2)
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| "rendered WAV size overflow".to_owned())?;
    if samples.len() != (frames as usize) * 2 || data_bytes > u32::MAX - 36 {
        return Err("rendered WAV buffer does not match its header".to_owned());
    }
    let byte_rate = sample_rate
        .checked_mul(8)
        .ok_or_else(|| "rendered WAV byte rate overflow".to_owned())?;
    let mut temporary = TemporaryWav::create(output_path)?;
    {
        let file = temporary.file.as_mut().expect("temporary WAV is open");
        file.write_all(b"RIFF")
            .and_then(|()| file.write_all(&(36 + data_bytes).to_le_bytes()))
            .and_then(|()| file.write_all(b"WAVEfmt "))
            .and_then(|()| file.write_all(&16u32.to_le_bytes()))
            .and_then(|()| file.write_all(&3u16.to_le_bytes()))
            .and_then(|()| file.write_all(&2u16.to_le_bytes()))
            .and_then(|()| file.write_all(&sample_rate.to_le_bytes()))
            .and_then(|()| file.write_all(&byte_rate.to_le_bytes()))
            .and_then(|()| file.write_all(&8u16.to_le_bytes()))
            .and_then(|()| file.write_all(&32u16.to_le_bytes()))
            .and_then(|()| file.write_all(b"data"))
            .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
            .map_err(|error| format!("could not write WAV header: {error}"))?;
        let mut block = Vec::with_capacity(32_768);
        for chunk in samples.chunks(8_192) {
            block.clear();
            for sample in chunk {
                block.extend_from_slice(&sample.to_le_bytes());
            }
            file.write_all(&block)
                .map_err(|error| format!("could not write WAV audio data: {error}"))?;
        }
    }
    temporary.commit(output_path)?;
    Ok(())
}

struct TemporaryWav {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl TemporaryWav {
    fn create(output_path: &Path) -> Result<Self, String> {
        let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = output_path
            .file_name()
            .ok_or_else(|| "output WAV path must include a file name".to_owned())?;
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
                        "could not create temporary WAV beside {}: {error}",
                        output_path.display()
                    ));
                }
            }
        }
        Err("could not allocate a unique temporary WAV file".to_owned())
    }

    fn commit(mut self, output_path: &Path) -> Result<(), String> {
        let mut file = self.file.take().expect("temporary WAV is open");
        file.flush()
            .map_err(|error| format!("could not flush rendered WAV: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not sync rendered WAV: {error}"))?;
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

impl Drop for TemporaryWav {
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
    fn converts_project_ticks_to_output_frames() {
        assert_eq!(ticks_to_frames(192, 96, 120.0, 48_000).unwrap(), 48_000);
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
    fn mixes_mono_clips_to_both_stereo_channels_with_resampling() {
        let source = DecodedAudio {
            sample_rate: 2,
            channels: vec![vec![0.0, 1.0]],
        };
        let clip = PreparedClip {
            clip_index: 0,
            channel_id: 0,
            path: PathBuf::new(),
            start_frame: 0,
            source_bounds: SampleBounds { start: 0, end: 2 },
            duration_frames: 4,
            gain: 1.0,
            pan: 0.0,
        };
        let mut mix = vec![0.0; 8];
        mix_clip_into_stereo(&mut mix, &source, &clip, 4, None).unwrap();
        assert_eq!(mix, vec![0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.0, 1.0]);
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
            path: path.clone(),
            start_frame: 2,
            source_bounds: SampleBounds { start: 0, end: 5 },
            duration_frames: 5,
            gain: 0.5,
            pan: 0.5,
        };
        let render = PreparedAudioClipRender {
            summary: AudioClipRenderSummary {
                frames: 9,
                sample_rate: 4,
                clips_rendered: 1,
                clips_skipped_unsupported_scale: 0,
                source_files: 1,
            },
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
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 1,
            stop_frame: Some(3),
            channel_id: 7,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
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
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 0,
            stop_frame: Some(100),
            channel_id: 2,
            key: SAMPLER_ROOT_KEY + 12,
            velocity: 127,
        }];
        let mut engine = SamplerVoiceEngine::new(&sources, &notes, 1, 4, 4);
        let mut output = vec![0.0; 8];
        engine.render_block(0, 4, &mut output);

        assert_eq!(output[2], 4.0);
        assert_eq!(output[4], 8.0);
        assert_eq!(output[6], 12.0);
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
            },
        )]);
        let notes = [ScheduledSamplerNote {
            start_frame: 2,
            stop_frame: None,
            channel_id: 3,
            key: SAMPLER_ROOT_KEY,
            velocity: 127,
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
                },
            ),
            (
                2,
                SamplerVoiceSource {
                    audio: source_b,
                    gain: 1.0,
                    pan: -1.0,
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
            },
            ScheduledSamplerNote {
                start_frame: 1,
                stop_frame: Some(12),
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
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
            header_bytes: [0; 4],
            start_offset: -1.0,
            end_offset: -1.0,
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
        let project = root.join("song.flp");
        let output = root.join("mix.wav");
        fs::write(&project, b"FLP project fixture").unwrap();

        assert!(validate_output_path(&project, &project).is_err());
        write_float_stereo_wav(&output, &[0.25, -0.5], 48_000, 1).unwrap();

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
}
