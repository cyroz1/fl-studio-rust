//! Offline rendering for Playlist clips that reference FLP audio channels.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::media::{DecodedAudio, SamplePathResolver, decode_audio_file};
use crate::{FlpDocument, PlaylistClip, PlaylistClipTarget};

const DEFAULT_SAMPLE_RATE: u32 = 44_100;
const MAX_MIX_BYTES: usize = 512 * 1024 * 1024;
const MAX_SOURCE_CACHE_BYTES: usize = 512 * 1024 * 1024;
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
/// This early render path uses the project's base tempo, clip positions, and observed audio
/// source offsets. Clips with default `-1` offsets use the full source file. It does not render
/// pattern instruments, tempo automation, channel gain/pan, plug-ins, or Mixer effects.
pub fn render_audio_clips_to_wav(
    document: &FlpDocument,
    project_path: impl AsRef<Path>,
    options: AudioClipRenderOptions,
    output_path: impl AsRef<Path>,
) -> Result<AudioClipRenderSummary, String> {
    let project_path = project_path.as_ref();
    let output_path = output_path.as_ref();
    validate_output_path(project_path, output_path)?;
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
    let mut candidate_clips = Vec::<(usize, &PlaylistClip, u16, PathBuf)>::new();
    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        max_tick = max_tick.max(u64::from(clip.position_ticks) + u64::from(clip.length_ticks));
        let PlaylistClipTarget::Channel { id } = clip.target() else {
            continue;
        };
        let Some(matching_channels) = channels.iter().find(|channel| channel.id() == id) else {
            continue;
        };
        if matching_channels.kind() != Some(4) || matching_channels.enabled() == Some(false) {
            continue;
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            clips_skipped_unsupported_scale += 1;
            continue;
        }
        let sample_path = matching_channels.sample_path().ok_or_else(|| {
            format!("audio channel {id} has no decoded sample path (Playlist clip {clip_index})")
        })?;
        let resolved_path = resolver.resolve(sample_path).map_err(|error| {
            format!("could not resolve audio channel {id} for Playlist clip {clip_index}: {error}")
        })?;
        candidate_clips.push((clip_index, clip, id, resolved_path));
    }
    if candidate_clips.is_empty() {
        return Err(
            "arrangement contains no enabled Playlist clips targeting audio channels".to_owned(),
        );
    }

    let timeline_frames = ticks_to_frames(max_tick, ppq, tempo_bpm, options.sample_rate)?;
    let mut decoded_by_path = HashMap::<PathBuf, DecodedAudio>::new();
    let mut cached_source_bytes = 0usize;
    for (_, _, _, path) in &candidate_clips {
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

    let mut prepared = Vec::with_capacity(candidate_clips.len());
    let mut output_frames = timeline_frames;
    for (clip_index, clip, channel_id, path) in &candidate_clips {
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
        prepared.push(PreparedClip {
            clip_index: *clip_index,
            channel_id: *channel_id,
            path: path.clone(),
            start_frame,
            source_bounds,
            duration_frames,
        });
    }

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

    for clip in &prepared {
        let audio = decoded_by_path
            .get(&clip.path)
            .expect("prepared clips have decoded sources");
        mix_clip_into_stereo(
            &mut mix,
            audio,
            clip.source_bounds,
            clip.start_frame,
            clip.duration_frames,
            options.sample_rate,
        )
        .map_err(|error| {
            format!(
                "could not mix Playlist clip {} from audio channel {}: {error}",
                clip.clip_index, clip.channel_id
            )
        })?;
    }

    write_float_stereo_wav(output_path, &mix, options.sample_rate, output_frames)?;
    Ok(AudioClipRenderSummary {
        frames: output_frames,
        sample_rate: options.sample_rate,
        clips_rendered: prepared.len(),
        clips_skipped_unsupported_scale,
        source_files: decoded_by_path.len(),
    })
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
}

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

fn mix_clip_into_stereo(
    mix: &mut [f32],
    source: &DecodedAudio,
    bounds: SampleBounds,
    start_frame: u64,
    output_frames: u64,
    output_rate: u32,
) -> Result<(), String> {
    if source.channels.is_empty() || source.channels.len() > 2 {
        return Err("source must have one or two channels".to_owned());
    }
    if source.sample_rate == 0 || output_rate == 0 || bounds.start >= bounds.end {
        return Err("source audio range or sample rate is invalid".to_owned());
    }
    if source
        .channels
        .iter()
        .any(|channel| bounds.end > channel.len())
    {
        return Err("source audio range exceeds a channel buffer".to_owned());
    }
    let end_frame = start_frame
        .checked_add(output_frames)
        .ok_or_else(|| "audio clip end position overflow".to_owned())?;
    let mix_frames = mix.len() / 2;
    if !mix.len().is_multiple_of(2) || end_frame > mix_frames as u64 {
        return Err("audio clip exceeds the render mix buffer".to_owned());
    }
    let source_frames_per_output = f64::from(source.sample_rate) / f64::from(output_rate);
    for output_offset in 0..output_frames {
        let position = (bounds.start as f64 + output_offset as f64 * source_frames_per_output)
            .min((bounds.end - 1) as f64);
        let left = interpolate_sample(&source.channels[0], position);
        let right = if source.channels.len() == 1 {
            left
        } else {
            interpolate_sample(&source.channels[1], position)
        };
        let output_index = (start_frame + output_offset) as usize * 2;
        mix[output_index] += left;
        mix[output_index + 1] += right;
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
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn converts_project_ticks_to_output_frames() {
        assert_eq!(ticks_to_frames(192, 96, 120.0, 48_000).unwrap(), 48_000);
    }

    #[test]
    fn mixes_mono_clips_to_both_stereo_channels_with_resampling() {
        let source = DecodedAudio {
            sample_rate: 2,
            channels: vec![vec![0.0, 1.0]],
        };
        let mut mix = vec![0.0; 8];
        mix_clip_into_stereo(
            &mut mix,
            &source,
            SampleBounds { start: 0, end: 2 },
            0,
            4,
            4,
        )
        .unwrap();
        assert_eq!(mix, vec![0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.0, 1.0]);
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
