//! Offline rendering for Playlist clips that reference FLP audio channels.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::audio::StreamingAudioWriter;
use crate::media::{DecodedAudio, SamplePathResolver, decode_audio_file};
use crate::vst3::Vst3PlaylistStreamProcessor;
use crate::{
    Arrangement, ChannelNoteRouter, FlpDocument, Pattern, PatternNote, PlaylistClip,
    PlaylistClipTarget,
};

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
    /// Render only this zero-based arrangement clip, rebased to output time zero.
    pub clip_index: Option<usize>,
    /// Keep the selected clip at its original arrangement position in the output.
    pub start_from_song_start: bool,
}

impl Default for AudioClipRenderOptions {
    fn default() -> Self {
        Self {
            arrangement_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
            clip_index: None,
            start_from_song_start: false,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavDitherMode {
    Off,
    Tpdf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResamplingQuality {
    Linear,
    Sinc64,
}

impl ResamplingQuality {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Sinc64 => "64-point sinc",
        }
    }
}

impl WavDitherMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Tpdf => "TPDF dither",
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
    pub tail_seconds: u8,
}

impl Default for PlaylistRenderOptions {
    fn default() -> Self {
        Self {
            arrangement_id: 0,
            sample_rate: DEFAULT_SAMPLE_RATE,
            sampler_voice_limit: DEFAULT_SAMPLER_VOICE_LIMIT,
            wav_sample_format: WavSampleFormat::Float32,
            wav_dither_mode: WavDitherMode::Off,
            resampling_quality: ResamplingQuality::Linear,
            wav_channel_mode: WavChannelMode::Stereo,
            tail_seconds: 0,
        }
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
/// Plugin instruments, automation, time stretching, and Mixer processing are not included yet.
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
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    start_frame: u64,
) -> Result<PlaylistRenderSummary, String> {
    let project_path = project_path.as_ref();
    let audio = prepare_audio_clip_render(
        document,
        project_path,
        AudioClipRenderOptions {
            arrangement_id: options.arrangement_id,
            sample_rate: options.sample_rate,
            clip_index: None,
            start_from_song_start: false,
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
    };

    let mut frames_to_skip = start_frame.min(frames);
    summary.voices_stolen = stream_prepared_playlist_render(
        PreparedPlaylistBlockMix {
            audio: &audio,
            sampler: &sampler,
            options,
            frames,
            vst3_processor: vst3_processor.as_mut(),
        },
        cancelled,
        || writer.is_cancelled(),
        |block| {
            let skipped_frames = frames_to_skip.min((block.len() / 2) as u64) as usize;
            frames_to_skip -= skipped_frames as u64;
            writer.write_stereo_samples(&block[skipped_frames * 2..])
        },
    )?;
    Ok(summary)
}

/// Render enabled Playlist audio clips, Sampler Pattern Clips, and mapped VST3 instruments to a
/// stereo WAV. Audio is mixed in bounded blocks and written to a temporary file, so song length
/// does not determine the in-memory mix size. Mixer effects, routing, tempo automation, and plugin
/// delay compensation are not applied.
pub fn render_playlist_with_vst3_to_wav_cancellable(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: PlaylistRenderOptions,
    output_path: impl AsRef<Path>,
    mut vst3_processor: Option<Vst3PlaylistStreamProcessor>,
    cancelled: &AtomicBool,
) -> Result<PlaylistRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path(project_path, output_path)?;
    let audio = prepare_audio_clip_render(
        document,
        project_path,
        AudioClipRenderOptions {
            arrangement_id: options.arrangement_id,
            sample_rate: options.sample_rate,
            clip_index: None,
            start_from_song_start: false,
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
    };

    let mut temporary = TemporaryWav::create(output_path)?;
    {
        let file = temporary.file.as_mut().expect("temporary WAV is open");
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
        let mut dither_state = (options.wav_dither_mode == WavDitherMode::Tpdf
            && options.wav_sample_format == WavSampleFormat::Pcm16)
            .then(TpdfDither::new);
        summary.voices_stolen = stream_prepared_playlist_render(
            PreparedPlaylistBlockMix {
                audio: &audio,
                sampler: &sampler,
                options,
                frames,
                vst3_processor: vst3_processor.as_mut(),
            },
            cancelled,
            || false,
            |block| {
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
    temporary.commit(output_path)?;
    Ok(summary)
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
            let dithered_sample = sample + (dither_noise_lsb / 32_768.0) as f32;
            let quantized = quantize_signed_pcm(dithered_sample, 32_768.0, 32_767.0) as i16;
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
    mut dither_state: Option<&mut TpdfDither>,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    if !stereo_block.len().is_multiple_of(2) {
        return Err("render block must contain interleaved stereo frames".to_owned());
    }
    let mut append_sample = |sample| {
        let noise = if sample_format == WavSampleFormat::Pcm16 {
            dither_state
                .as_deref_mut()
                .map_or(0.0, TpdfDither::next_lsb_noise)
        } else {
            0.0
        };
        append_wav_sample_with_dither(sample, sample_format, noise, output);
    };
    for frame in stereo_block.as_chunks::<2>().0 {
        match channel_mode {
            WavChannelMode::Stereo => {
                append_sample(frame[0]);
                append_sample(frame[1]);
            }
            WavChannelMode::MonoMerged => {
                let merged = frame[0] * 0.5 + frame[1] * 0.5;
                append_sample(merged);
            }
            WavChannelMode::MonoLeft => append_sample(frame[0]),
            WavChannelMode::MonoRight => append_sample(frame[1]),
        }
    }
    Ok(())
}

struct TpdfDither {
    state: u64,
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
    vst3_processor: Option<&'a mut Vst3PlaylistStreamProcessor>,
}

fn stream_prepared_playlist_render(
    mut render: PreparedPlaylistBlockMix<'_>,
    cancelled: &AtomicBool,
    mut stream_cancelled: impl FnMut() -> bool,
    mut write_block: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<usize, String> {
    crate::audio::enable_denormal_protection();
    let frames = render.frames;
    let options = render.options;
    let release_frames = (f64::from(options.sample_rate) * SAMPLER_RELEASE_SECONDS)
        .round()
        .max(1.0) as usize;
    let mut sampler_engine = SamplerVoiceEngine::with_resampling_quality(
        &render.sampler.sources_by_channel,
        &render.sampler.notes,
        options.sampler_voice_limit,
        release_frames,
        options.sample_rate,
        options.resampling_quality,
    );
    if let Some(processor) = render.vst3_processor.as_deref_mut() {
        processor.start_processing()?;
    }
    let mut output = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
    let mut sampler_block = vec![0.0f32; STREAM_BLOCK_FRAMES * 2];
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
            for clip in &render.audio.clips {
                let source = render
                    .audio
                    .decoded_by_path
                    .get(&clip.path)
                    .expect("prepared audio clips have decoded sources");
                mix_clip_window_into_stereo_with_quality(
                    block,
                    block_start,
                    source,
                    clip,
                    options.sample_rate,
                    options.resampling_quality,
                    Some(cancelled),
                )
                .map_err(|error| {
                    format!(
                        "could not mix Playlist clip {} from audio channel {}: {error}",
                        clip.clip_index, clip.channel_id
                    )
                })?;
            }
            let sampler_samples = &mut sampler_block[..sample_count];
            sampler_engine.render_block(block_start, frame_count, sampler_samples);
            for (mixed, sampler_sample) in block.iter_mut().zip(sampler_samples) {
                *mixed += *sampler_sample;
            }
            if let Some(processor) = render.vst3_processor.as_deref_mut() {
                processor.mix_next_block(block)?;
            }
            write_block(block)?;
            block_start += frame_count as u64;
        }
        Ok::<_, String>(())
    })();
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
    crate::audio::enable_denormal_protection();
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
    let disabled_track_ids = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    let channels = document.channels();
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
    let mut candidate_clips = Vec::<(usize, &PlaylistClip, u16, PathBuf, f32, f32)>::new();
    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        check_cancelled(cancelled)?;
        if !audio_clip_matches_selection(options.clip_index, clip_index) {
            continue;
        }
        let relative_position =
            audio_clip_render_start_tick(clip.position_ticks, selection_start_tick)?;
        max_tick = max_tick.max(relative_position + u64::from(clip.length_ticks));
        if clip
            .playlist_track_id()
            .is_some_and(|track_id| disabled_track_ids.contains(&track_id))
        {
            continue;
        }
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
        candidate_clips.push((clip_index, clip, id, resolved_path, gain, pan));
    }
    if let Some(clip_index) = options.clip_index
        && candidate_clips.is_empty()
    {
        return Err(format!(
            "selected Playlist clip {clip_index} does not target an enabled audio channel"
        ));
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
        let start_tick = audio_clip_render_start_tick(clip.position_ticks, selection_start_tick)?;
        let start_frame = ticks_to_frames(start_tick, ppq, tempo_bpm, options.sample_rate)?;
        let source_duration_frames = source_duration_to_frames(
            source_bounds.end - source_bounds.start,
            audio.sample_rate,
            options.sample_rate,
        )?;
        let duration_frames = audio_clip_duration_frames(
            source_duration_frames,
            clip.length_ticks,
            ppq,
            tempo_bpm,
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
    pub(crate) clipped_stop_tick: Option<u64>,
    pub(crate) clip_index: usize,
}

pub(crate) struct PlaylistPatternSchedule<'a> {
    pub(crate) notes: Vec<PlaylistPatternNote<'a>>,
    pub(crate) clips_skipped_unsupported_scale: usize,
}

pub(crate) fn schedule_playlist_pattern_notes<'a>(
    patterns: &'a [Pattern],
    arrangement: &Arrangement,
    disabled_track_ids: &BTreeSet<u32>,
    ppq: u16,
    global_swing_mix_raw: u8,
    mut channel_swing_mix_raw: impl FnMut(u16) -> u16,
    mut resolve_targets: impl FnMut(u16, u64) -> Vec<u16>,
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
        if clip
            .playlist_track_id()
            .is_some_and(|track_id| disabled_track_ids.contains(&track_id))
        {
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
                    ppq,
                    global_swing_mix_raw,
                    channel_swing_mix_raw(note.channel_id),
                )?;
                let swing_offset = swung_relative_start.saturating_sub(relative_start);
                let start_tick = u64::from(clip.position_ticks)
                    .checked_add(swung_relative_start)
                    .ok_or_else(|| "swung Playlist pattern note position overflow".to_owned())?;
                if start_tick >= clip_end {
                    continue;
                }
                let clipped_stop_tick = if note.length == 0 {
                    None
                } else {
                    Some(
                        nominal_start_tick
                            .checked_add(u64::from(note.length))
                            .and_then(|stop_tick| stop_tick.checked_add(swing_offset))
                            .ok_or_else(|| {
                                "Playlist pattern note end position overflow".to_owned()
                            })?
                            .min(clip_end),
                    )
                };
                let note_seed = (clip_index as u64).rotate_left(32)
                    ^ repetition.rotate_left(17)
                    ^ (note_index as u64).rotate_left(3)
                    ^ nominal_start_tick.rotate_left(47);
                for target_channel_id in resolve_targets(note.channel_id, note_seed) {
                    if schedule.notes.len() >= MAX_SCHEDULED_PLAYLIST_NOTES {
                        return Err(format!(
                            "Playlist expands to more than {MAX_SCHEDULED_PLAYLIST_NOTES} pattern note events"
                        ));
                    }
                    schedule.notes.push(PlaylistPatternNote {
                        note,
                        target_channel_id,
                        start_tick,
                        clipped_stop_tick,
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
    notes: Vec<ScheduledSamplerNote>,
    sources_by_channel: HashMap<u16, SamplerVoiceSource>,
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
    let disabled_track_ids = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    let patterns = document.patterns().map_err(|error| error.to_string())?;
    let channels = document.channels();
    let channels_by_id: HashMap<_, _> = channels
        .iter()
        .map(|channel| (channel.id(), channel))
        .collect();
    let channel_router = ChannelNoteRouter::new(channels.clone());
    let schedule = schedule_playlist_pattern_notes(
        &patterns,
        &arrangement,
        &disabled_track_ids,
        ppq,
        document.metadata().global_swing_mix(),
        |channel_id| {
            channels_by_id
                .get(&channel_id)
                .map_or(128, |channel| channel.swing_mix())
        },
        |channel_id, seed| {
            channel_router
                .targets(channel_id, seed)
                .into_iter()
                .filter(|target_channel_id| {
                    channels_by_id
                        .get(target_channel_id)
                        .is_some_and(|channel| {
                            channel.kind() == Some(0) && channel.enabled() != Some(false)
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
            notes: Vec::new(),
            sources_by_channel: HashMap::new(),
            pattern_clips_rendered: 0,
            pattern_clips_skipped_unsupported_scale: schedule.clips_skipped_unsupported_scale,
            source_paths: std::collections::BTreeSet::new(),
        });
    }

    let resolver = SamplePathResolver::new(project_path);
    let mut decoded_by_path = HashMap::<PathBuf, Arc<DecodedAudio>>::new();
    let mut sources_by_channel = HashMap::<u16, SamplerVoiceSource>::new();
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
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            source_entry.insert(SamplerVoiceSource { audio, gain, pan });
        }
        let Some(source) = sources_by_channel.get(&placed.target_channel_id) else {
            continue;
        };
        if source.audio.frame_count() == 0 {
            return Err(format!(
                "Sampler channel {} references an empty sample",
                placed.target_channel_id
            ));
        }
        let start_frame = ticks_to_frames(placed.start_tick, ppq, tempo_bpm, options.sample_rate)?;
        let stop_frame = placed
            .clipped_stop_tick
            .map(|stop_tick| {
                ticks_to_frames(stop_tick, ppq, tempo_bpm, options.sample_rate)
                    .map(|frame| frame.max(start_frame.saturating_add(1)))
            })
            .transpose()?;
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
        rendered_pattern_clip_indices.insert(placed.clip_index);
        notes.push(ScheduledSamplerNote {
            start_frame,
            stop_frame,
            channel_id: placed.target_channel_id,
            key: note.key,
            velocity: note.velocity,
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
        notes,
        sources_by_channel,
        pattern_clips_rendered: rendered_pattern_clip_indices.len(),
        pattern_clips_skipped_unsupported_scale: schedule.clips_skipped_unsupported_scale,
        source_paths: decoded_by_path.keys().cloned().collect(),
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
    let global_swing_mix_raw = document.metadata().global_swing_mix();
    let channels = document.channels();
    let channels_by_id: HashMap<_, _> = channels
        .iter()
        .map(|channel| (channel.id(), channel))
        .collect();
    let channel_router = ChannelNoteRouter::new(channels.clone());
    let sampler_notes = pattern
        .notes
        .iter()
        .enumerate()
        .flat_map(|(note_index, note)| {
            channel_router
                .targets(note.channel_id, note_index as u64)
                .into_iter()
                .filter(|target_channel_id| {
                    channels_by_id
                        .get(target_channel_id)
                        .is_some_and(|channel| {
                            channel.kind() == Some(0) && channel.enabled() != Some(false)
                        })
                })
                .map(move |target_channel_id| (note, target_channel_id))
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
            let (gain, pan) = channel_gain_pan(channel.volume(), channel.pan());
            source_entry.insert(SamplerVoiceSource { audio, gain, pan });
        }
        let Some(source) = sources_by_channel.get(&target_channel_id) else {
            continue;
        };
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
            channel_id: target_channel_id,
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
    resampling_quality: ResamplingQuality,
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
        Self::with_resampling_quality(
            sources_by_channel,
            notes,
            voice_limit,
            release_frames,
            output_sample_rate,
            ResamplingQuality::Linear,
        )
    }

    fn with_resampling_quality(
        sources_by_channel: &'a HashMap<u16, SamplerVoiceSource>,
        notes: &'a [ScheduledSamplerNote],
        voice_limit: usize,
        release_frames: usize,
        output_sample_rate: u32,
        resampling_quality: ResamplingQuality,
    ) -> Self {
        Self {
            sources_by_channel,
            notes,
            next_note: 0,
            voices: std::iter::repeat_with(|| None).take(voice_limit).collect(),
            release_frames,
            output_sample_rate,
            resampling_quality,
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

fn resample_sample(
    channel: &[f32],
    position: f64,
    source_step: f64,
    quality: ResamplingQuality,
) -> f32 {
    if channel.is_empty() || !position.is_finite() {
        return 0.0;
    }
    match quality {
        ResamplingQuality::Linear => {
            let first_index = (position.floor() as usize).min(channel.len() - 1);
            let second_index = (first_index + 1).min(channel.len() - 1);
            let fraction = (position - first_index as f64) as f32;
            let first = finite_audio_sample(channel[first_index]);
            let second = finite_audio_sample(channel[second_index]);
            first + (second - first) * fraction
        }
        ResamplingQuality::Sinc64 => windowed_sinc64_sample(channel, position, source_step),
    }
}

fn finite_audio_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn windowed_sinc64_sample(channel: &[f32], position: f64, source_step: f64) -> f32 {
    const TAPS: isize = 64;
    const LEFT_TAPS: isize = 31;
    let center = position.floor() as isize;
    let last_index = (channel.len() - 1) as isize;
    let cutoff = if source_step.is_finite() && source_step > 1.0 {
        (1.0 / source_step).clamp(1.0e-6, 1.0)
    } else {
        1.0
    };
    let mut weighted_sample = 0.0;
    let mut weight_sum = 0.0;

    for tap in 0..TAPS {
        let source_index = center + tap - LEFT_TAPS;
        let distance = source_index as f64 - position;
        let window_position = distance / 32.0;
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

fn audio_clip_duration_frames(
    source_duration_frames: u64,
    clip_length_ticks: u32,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: u32,
) -> Result<u64, String> {
    let clip_duration_frames =
        ticks_to_frames(u64::from(clip_length_ticks), ppq, tempo_bpm, sample_rate)?;
    Ok(source_duration_frames.min(clip_duration_frames))
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
        mix[output_index] += left * left_gain;
        mix[output_index + 1] += right * right_gain;
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
            header_bytes: [0; 4],
            start_offset: -1.0,
            end_offset: -1.0,
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
        let rendered = resample_sample(&samples, 512.25, 1.0, ResamplingQuality::Sinc64);
        assert!((rendered - 0.375).abs() < 1.0e-6);
    }

    #[test]
    fn sinc_resampling_attenuates_nyquist_tone_when_downsampling() {
        let samples = (0usize..2048)
            .map(|index| if index.is_multiple_of(2) { 1.0 } else { -1.0 })
            .collect::<Vec<_>>();
        let rendered = resample_sample(&samples, 1024.0, 2.0, ResamplingQuality::Sinc64);
        assert!(
            rendered.abs() < 0.01,
            "unexpected aliased output: {rendered}"
        );
    }

    #[test]
    fn converts_project_ticks_to_output_frames() {
        assert_eq!(ticks_to_frames(192, 96, 120.0, 48_000).unwrap(), 48_000);
    }

    #[test]
    fn audio_clip_render_stops_at_the_playlist_clip_end() {
        assert_eq!(
            audio_clip_duration_frames(96_000, 192, 96, 120.0, 48_000).unwrap(),
            48_000
        );
        assert_eq!(
            audio_clip_duration_frames(48_000, 384, 96, 120.0, 48_000).unwrap(),
            48_000
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
            &BTreeSet::new(),
            96,
            0,
            |_| 128,
            |channel_id, _| vec![channel_id],
        )
        .unwrap();
        assert_eq!(
            schedule
                .notes
                .iter()
                .map(|placed| (placed.start_tick, placed.clipped_stop_tick))
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
            &BTreeSet::from([1]),
            96,
            0,
            |_| 128,
            |channel_id, _| vec![channel_id],
        )
        .unwrap();

        assert!(schedule.notes.is_empty());
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
            &BTreeSet::new(),
            96,
            0,
            |_| 128,
            |channel_id, _| {
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
            &BTreeSet::new(),
            96,
            0,
            |_| 128,
            |channel_id, _| vec![channel_id],
        )
        .unwrap();
        assert_eq!(schedule.notes.len(), 1);
        assert_eq!(schedule.notes[0].start_tick, 12);
        assert_eq!(schedule.notes[0].clipped_stop_tick, None);
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
            &BTreeSet::new(),
            96,
            128,
            |channel_id| if channel_id == 7 { 128 } else { 0 },
            |channel_id, _| vec![channel_id],
        )
        .unwrap();
        assert_eq!(schedule.notes.len(), 1);
        assert_eq!(schedule.notes[0].start_tick, 44);
        assert_eq!(schedule.notes[0].clipped_stop_tick, Some(68));
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
                path,
                start_frame: 0,
                source_bounds: SampleBounds { start: 0, end: 2 },
                duration_frames: 2,
                gain: 0.5,
                pan: 0.0,
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
            notes: vec![ScheduledSamplerNote {
                start_frame: 0,
                stop_frame: None,
                channel_id: 2,
                key: SAMPLER_ROOT_KEY,
                velocity: 127,
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
                },
            )]),
            pattern_clips_rendered: 1,
            pattern_clips_skipped_unsupported_scale: 0,
            source_paths: std::collections::BTreeSet::new(),
        };
        let mut rendered = Vec::new();

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
                    tail_seconds: 0,
                },
                frames: 2,
                vst3_processor: None,
            },
            &AtomicBool::new(false),
            || false,
            |block| {
                rendered.extend_from_slice(block);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(voices_stolen, 0);
        assert_eq!(rendered, vec![2.5, 1.0, 3.5, 2.0]);
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
