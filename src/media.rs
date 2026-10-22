//! Project media path resolution.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

const FACTORY_DATA_MACRO: &str = "%FLStudioFactoryData%";
const MAX_DECODED_SAMPLE_BYTES: usize = 512 * 1024 * 1024;
const WAVEFORM_SPECTRUM_WINDOW: usize = 256;
const WAVEFORM_SPECTRUM_COLUMNS: usize = 128;
const WAVEFORM_SPECTRUM_BANDS: usize = 32;
static NEXT_AUDIO_WAV_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Fully decoded, deinterleaved audio for sample preview and offline processing.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    /// Samples indexed `[channel][frame]` and converted to `f32` in approximately -1.0..1.0.
    pub channels: Vec<Vec<f32>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WaveformPeak {
    pub minimum: f32,
    pub maximum: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioWaveform {
    pub sample_rate: u32,
    pub frame_count: u64,
    /// Per-bucket extrema combined across channels, ordered from the sample start.
    pub peaks: Vec<WaveformPeak>,
    /// Per-channel extrema for separate stereo displays, ordered from the sample start.
    pub channel_peaks: Vec<Vec<WaveformPeak>>,
    /// Quantized log-frequency spectral intensity by time column and low-to-high frequency band.
    pub spectral_bands: Vec<Vec<u8>>,
}

/// FL Studio factory samples can wrap an Ogg stream in a RIFF/WAVE header with a private codec
/// tag. Present the embedded stream at offset zero so Symphonia can probe it by its Ogg signature.
struct OffsetMediaSource {
    file: File,
    offset: u64,
    length: u64,
}

impl OffsetMediaSource {
    fn new(mut file: File, offset: u64) -> io::Result<Self> {
        let file_length = file.metadata()?.len();
        let length = file_length.checked_sub(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "media offset exceeds file length",
            )
        })?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            file,
            offset,
            length,
        })
    }
}

impl Read for OffsetMediaSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for OffsetMediaSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let current = self
            .file
            .stream_position()?
            .checked_sub(self.offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid media position"))?;
        let target = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(current) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        if !(0..=i128::from(self.length)).contains(&target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media seek is outside the embedded stream",
            ));
        }
        let target = target as u64;
        self.file.seek(SeekFrom::Start(self.offset + target))?;
        Ok(target)
    }
}

impl MediaSource for OffsetMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}

impl DecodedAudio {
    pub fn frame_count(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn duration_seconds(&self) -> f64 {
        self.frame_count() as f64 / f64::from(self.sample_rate)
    }
}

/// Write a frame range from decoded audio to a new interleaved 32-bit-float WAV file.
///
/// The destination is replaced atomically after the complete WAV has been written. The source
/// buffer is never modified.
pub fn write_audio_range_to_wav(
    audio: &DecodedAudio,
    frame_range: Range<usize>,
    output_path: impl AsRef<Path>,
) -> Result<(), String> {
    write_audio_range_to_wav_with_fades(audio, frame_range, 0, 0, output_path)
}

/// Write a selected frame range to WAV with optional linear fades at its edges.
///
/// Fade lengths are measured in frames and may overlap; overlapping fade gains are multiplied.
/// The decoded source buffer is never modified.
pub fn write_audio_range_to_wav_with_fades(
    audio: &DecodedAudio,
    frame_range: Range<usize>,
    fade_in_frames: usize,
    fade_out_frames: usize,
    output_path: impl AsRef<Path>,
) -> Result<(), String> {
    write_audio_range_to_wav_with_processing(
        audio,
        frame_range,
        fade_in_frames,
        fade_out_frames,
        false,
        None,
        output_path,
    )
}

/// Write a selected frame range with optional linear fades and peak normalization.
///
/// The normalization target is expressed in dBFS and must be between -60 and 0. Fade lengths
/// are measured in frames and may overlap; overlapping fade gains are multiplied. Processing
/// affects only the exported WAV and never modifies the decoded source buffer.
pub fn write_audio_range_to_wav_with_processing(
    audio: &DecodedAudio,
    frame_range: Range<usize>,
    fade_in_frames: usize,
    fade_out_frames: usize,
    reverse: bool,
    normalization_target_dbfs: Option<f32>,
    output_path: impl AsRef<Path>,
) -> Result<(), String> {
    let output_path = output_path.as_ref();
    if !output_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err("trimmed audio output must use the .wav extension".to_owned());
    }
    if !(8_000..=384_000).contains(&audio.sample_rate) {
        return Err("sample rate must be between 8,000 and 384,000 Hz".to_owned());
    }
    let channel_count = audio.channels.len();
    if channel_count == 0 || channel_count > usize::from(u16::MAX) {
        return Err("audio must contain between 1 and 65,535 channels".to_owned());
    }
    let source_frames = audio.frame_count();
    if audio
        .channels
        .iter()
        .any(|channel| channel.len() != source_frames)
    {
        return Err("audio channels must have the same frame count".to_owned());
    }
    if frame_range.start >= frame_range.end || frame_range.end > source_frames {
        return Err("audio selection must contain frames from the source".to_owned());
    }
    let block_align = channel_count
        .checked_mul(std::mem::size_of::<f32>())
        .and_then(|bytes| u16::try_from(bytes).ok())
        .ok_or_else(|| "audio has too many channels for a WAV frame".to_owned())?;
    let byte_rate = audio
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "WAV byte rate overflow".to_owned())?;
    let frames = frame_range.end - frame_range.start;
    if fade_in_frames > frames || fade_out_frames > frames {
        return Err("fade lengths cannot exceed the selected frame range".to_owned());
    }
    if normalization_target_dbfs
        .is_some_and(|target| !target.is_finite() || !(-60.0..=0.0).contains(&target))
    {
        return Err("normalization target must be between -60 and 0 dBFS".to_owned());
    }
    let normalization_gain = normalization_target_dbfs.map_or(1.0, |target| {
        let peak = audio
            .channels
            .iter()
            .flat_map(|channel| &channel[frame_range.clone()])
            .copied()
            .filter(|sample| sample.is_finite())
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        if peak == 0.0 {
            1.0
        } else {
            10.0_f64.powf(f64::from(target) / 20.0) / f64::from(peak)
        }
    });
    let data_bytes = frames
        .checked_mul(usize::from(block_align))
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| "selected audio exceeds the RIFF/WAVE size limit".to_owned())?;
    let riff_size = 36_u32
        .checked_add(data_bytes)
        .ok_or_else(|| "selected audio exceeds the RIFF/WAVE size limit".to_owned())?;

    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = output_path
        .file_name()
        .ok_or_else(|| "WAV output path must include a file name".to_owned())?
        .to_string_lossy();
    let mut temporary_path = None;
    let mut temporary_file = None;
    for _ in 0..32 {
        let id = NEXT_AUDIO_WAV_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.tmp-{}-{id}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary_path = Some(candidate);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "could not create temporary WAV beside {}: {error}",
                    output_path.display()
                ));
            }
        }
    }
    let temporary_path =
        temporary_path.ok_or_else(|| "could not reserve a temporary WAV file name".to_owned())?;
    let write_result = (|| {
        let file = temporary_file.as_mut().expect("temporary WAV file is open");
        file.write_all(b"RIFF")
            .and_then(|()| file.write_all(&riff_size.to_le_bytes()))
            .and_then(|()| file.write_all(b"WAVEfmt "))
            .and_then(|()| file.write_all(&16_u32.to_le_bytes()))
            .and_then(|()| file.write_all(&3_u16.to_le_bytes()))
            .and_then(|()| file.write_all(&(channel_count as u16).to_le_bytes()))
            .and_then(|()| file.write_all(&audio.sample_rate.to_le_bytes()))
            .and_then(|()| file.write_all(&byte_rate.to_le_bytes()))
            .and_then(|()| file.write_all(&block_align.to_le_bytes()))
            .and_then(|()| file.write_all(&32_u16.to_le_bytes()))
            .and_then(|()| file.write_all(b"data"))
            .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
            .map_err(|error| format!("could not write WAV header: {error}"))?;
        let mut frame_bytes = Vec::with_capacity(usize::from(block_align));
        for offset in 0..frames {
            let frame = if reverse {
                frame_range.end - 1 - offset
            } else {
                frame_range.start + offset
            };
            frame_bytes.clear();
            let fade_in_gain = if fade_in_frames > 0 && offset < fade_in_frames {
                offset as f32 / fade_in_frames as f32
            } else {
                1.0
            };
            let remaining_frames = frames - offset - 1;
            let fade_out_gain = if fade_out_frames > 0 && remaining_frames < fade_out_frames {
                remaining_frames as f32 / fade_out_frames as f32
            } else {
                1.0
            };
            let gain = fade_in_gain * fade_out_gain;
            for channel in &audio.channels {
                let sample = channel[frame];
                frame_bytes.extend_from_slice(
                    &((if sample.is_finite() {
                        f64::from(sample) * normalization_gain * f64::from(gain)
                    } else {
                        0.0
                    }) as f32)
                        .to_le_bytes(),
                );
            }
            file.write_all(&frame_bytes)
                .map_err(|error| format!("could not write WAV samples: {error}"))?;
        }
        file.sync_all()
            .map_err(|error| format!("could not finalize WAV samples: {error}"))
    })();
    drop(temporary_file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    let backup = if fs::symlink_metadata(output_path).is_ok() {
        if !output_path.is_file() {
            let _ = fs::remove_file(&temporary_path);
            return Err("WAV output path is not a file".to_owned());
        }
        let (backup_directory, backup_path) = loop {
            let id = NEXT_AUDIO_WAV_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!(".{file_name}.backup-{}-{id}", std::process::id()));
            match fs::create_dir(&directory) {
                Ok(()) => break (directory.clone(), directory.join("previous.wav")),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    let _ = fs::remove_file(&temporary_path);
                    return Err(format!("could not reserve WAV backup path: {error}"));
                }
            }
        };
        if let Err(error) = fs::rename(output_path, &backup_path) {
            let _ = fs::remove_file(&temporary_path);
            let _ = fs::remove_dir(&backup_directory);
            return Err(format!(
                "could not preserve existing output {}: {error}",
                output_path.display()
            ));
        }
        Some((backup_directory, backup_path))
    } else {
        None
    };
    if let Err(error) = fs::rename(&temporary_path, output_path) {
        let restore_error = backup.as_ref().and_then(|(directory, backup_path)| {
            fs::rename(backup_path, output_path).err().map(|restore| {
                format!(
                    "; could not restore prior output from {}: {restore}",
                    directory.display()
                )
            })
        });
        let _ = fs::remove_file(&temporary_path);
        if let Some((directory, backup_path)) = &backup
            && restore_error.is_none()
        {
            let _ = fs::remove_file(backup_path);
            let _ = fs::remove_dir(directory);
        }
        return Err(format!(
            "could not save trimmed WAV to {}: {error}{}",
            output_path.display(),
            restore_error.unwrap_or_default()
        ));
    }
    if let Some((directory, backup_path)) = backup {
        let _ = fs::remove_file(backup_path);
        let _ = fs::remove_dir(directory);
    }
    Ok(())
}

/// Decode common FL Studio sample formats by inspecting file content rather than its extension.
///
/// FLP projects can contain compressed audio whose filename extension does not match its
/// container. The decoded audio is bounded to 512 MiB of `f32` sample data; longer media needs
/// streaming playback instead of this in-memory helper.
pub fn decode_audio_file(path: impl AsRef<Path>) -> Result<DecodedAudio, String> {
    decode_audio_file_for_preview(path.as_ref(), None)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SamplerFileMetadata {
    /// Root key from WAVE `inst`/`smpl` or AIFF `INST` metadata.
    pub(crate) root_key: Option<u16>,
    /// Fine tuning in cents from WAVE or AIFF sampler metadata.
    pub(crate) fine_tune_cents: Option<f64>,
    /// Suggested sample gain from the signed instrument-chunk decibel field.
    pub(crate) gain_db: Option<i16>,
    /// Inclusive MIDI key range from the instrument chunk.
    pub(crate) key_range: Option<(u8, u8)>,
    /// Inclusive MIDI velocity range from the instrument chunk.
    pub(crate) velocity_range: Option<(u8, u8)>,
    /// First supported WAVE or AIFF sustain loop as half-open source-frame bounds.
    pub(crate) loop_points: Option<(usize, usize)>,
    /// AIFF `INST` loop mode when its sustain markers resolve to valid frame bounds.
    pub(crate) ping_pong_loop: Option<bool>,
    /// AIFF `INST` release loop as half-open source-frame bounds.
    pub(crate) release_loop_points: Option<(usize, usize)>,
    /// AIFF `INST` loop mode when its release markers resolve to valid frame bounds.
    pub(crate) release_ping_pong_loop: Option<bool>,
}

/// Read standard sampler metadata from a RIFF/WAVE file.
///
/// The `inst` root key takes precedence over the `smpl` MIDI unity note. WAVE stores loop ends
/// inclusively, while playback code uses an exclusive end. Invalid or absent metadata is ignored.
pub(crate) fn wav_sampler_metadata(path: &Path) -> Option<SamplerFileMetadata> {
    let mut file = File::open(path).ok()?;
    let file_length = file.metadata().ok()?.len();
    if file_length < 12 {
        return None;
    }
    let mut header = [0; 12];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }
    let riff_size = u64::from(u32::from_le_bytes(header[4..8].try_into().ok()?));
    let riff_end = 8u64.checked_add(riff_size)?;
    if riff_size < 4 || riff_end > file_length {
        return None;
    }

    let mut cursor = 12u64;
    let mut inst_root_key = None;
    let mut inst_fine_tune_cents = None;
    let mut inst_gain_db = None;
    let mut inst_key_range = None;
    let mut inst_velocity_range = None;
    let mut smpl_root_key = None;
    let mut smpl_fine_tune_cents = None;
    let mut loop_points = None;
    while cursor.checked_add(8)? <= riff_end {
        file.seek(SeekFrom::Start(cursor)).ok()?;
        let mut chunk_header = [0; 8];
        file.read_exact(&mut chunk_header).ok()?;
        let chunk_size = u64::from(u32::from_le_bytes(chunk_header[4..8].try_into().ok()?));
        let body_start = cursor.checked_add(8)?;
        let body_end = body_start.checked_add(chunk_size)?;
        if body_end > riff_end {
            return None;
        }

        match &chunk_header[..4] {
            b"inst" if chunk_size >= 7 && inst_root_key.is_none() => {
                file.seek(SeekFrom::Start(body_start)).ok()?;
                let mut instrument = [0; 7];
                file.read_exact(&mut instrument).ok()?;
                inst_gain_db.get_or_insert_with(|| i16::from(i8::from_le_bytes([instrument[2]])));
                let low_note = instrument[3];
                let high_note = instrument[4];
                if low_note <= high_note && high_note <= 127 {
                    inst_key_range = Some((low_note, high_note));
                }
                let low_velocity = instrument[5];
                let high_velocity = instrument[6];
                if (1..=127).contains(&low_velocity)
                    && (1..=127).contains(&high_velocity)
                    && low_velocity <= high_velocity
                {
                    inst_velocity_range = Some((low_velocity, high_velocity));
                }
                if instrument[0] <= 127 {
                    inst_root_key = Some(u16::from(instrument[0]));
                    let fine_tune_cents = i8::from_le_bytes([instrument[1]]);
                    if (-50..=50).contains(&fine_tune_cents) {
                        inst_fine_tune_cents = Some(f64::from(fine_tune_cents));
                    }
                }
            }
            b"smpl" if chunk_size >= 36 => {
                file.seek(SeekFrom::Start(body_start)).ok()?;
                let mut sampler_header = [0; 36];
                file.read_exact(&mut sampler_header).ok()?;
                let root_key = u32::from_le_bytes(
                    sampler_header[12..16]
                        .try_into()
                        .expect("the MIDI unity note has four bytes"),
                );
                if smpl_root_key.is_none() && root_key <= 127 {
                    smpl_root_key = Some(root_key as u16);
                    let pitch_fraction = u32::from_le_bytes(
                        sampler_header[16..20]
                            .try_into()
                            .expect("the MIDI pitch fraction has four bytes"),
                    );
                    smpl_fine_tune_cents =
                        Some(f64::from(pitch_fraction) / 4_294_967_296.0 * 100.0);
                }

                if loop_points.is_none() {
                    let loop_count = u64::from(u32::from_le_bytes(
                        sampler_header[28..32]
                            .try_into()
                            .expect("the WAVE loop count has four bytes"),
                    ));
                    if loop_count > 0
                        && 36u64.checked_add(loop_count.checked_mul(24)?)? <= chunk_size
                    {
                        file.seek(SeekFrom::Start(body_start.checked_add(36)?))
                            .ok()?;
                        let mut first_loop = [0; 24];
                        file.read_exact(&mut first_loop).ok()?;
                        let start = u32::from_le_bytes(
                            first_loop[8..12]
                                .try_into()
                                .expect("the WAVE loop start has four bytes"),
                        );
                        let end_inclusive = u32::from_le_bytes(
                            first_loop[12..16]
                                .try_into()
                                .expect("the WAVE loop end has four bytes"),
                        );
                        if let Some(end) = end_inclusive.checked_add(1)
                            && start < end
                        {
                            loop_points = Some((start as usize, end as usize));
                        }
                    }
                }
            }
            _ => {}
        }

        cursor = body_end.checked_add(chunk_size & 1)?;
        if cursor > riff_end {
            return None;
        }
    }

    let metadata = SamplerFileMetadata {
        root_key: inst_root_key.or(smpl_root_key),
        fine_tune_cents: inst_fine_tune_cents.or(smpl_fine_tune_cents),
        gain_db: inst_gain_db,
        key_range: inst_key_range,
        velocity_range: inst_velocity_range,
        loop_points,
        ping_pong_loop: None,
        release_loop_points: None,
        release_ping_pong_loop: None,
    };
    (metadata.root_key.is_some()
        || metadata.gain_db.is_some()
        || metadata.key_range.is_some()
        || metadata.velocity_range.is_some()
        || metadata.loop_points.is_some())
    .then_some(metadata)
}

/// Read sampler metadata from a standard AIFF or AIFF-C `INST` and `MARK` chunk.
///
/// Both loops use half-open frame bounds. Invalid or absent sampler metadata is ignored, leaving
/// the project's channel root key and default tuning in effect.
pub(crate) fn aiff_sampler_metadata(path: &Path) -> Option<SamplerFileMetadata> {
    let mut file = File::open(path).ok()?;
    let file_length = file.metadata().ok()?.len();
    if file_length < 12 {
        return None;
    }
    let mut header = [0; 12];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"FORM" || !matches!(&header[8..12], b"AIFF" | b"AIFC") {
        return None;
    }
    let form_size = u64::from(u32::from_be_bytes(header[4..8].try_into().ok()?));
    let form_end = 8u64.checked_add(form_size)?;
    if form_size < 4 || form_end > file_length {
        return None;
    }

    let mut cursor = 12u64;
    let mut instrument_chunk_seen = false;
    let mut marker_chunk_seen = false;
    let mut root_key = None;
    let mut fine_tune_cents = None;
    let mut gain_db = None;
    let mut key_range = None;
    let mut velocity_range = None;
    let mut sustain_loop = None;
    let mut release_loop = None;
    let mut marker_positions = None;
    while cursor.checked_add(8)? <= form_end {
        file.seek(SeekFrom::Start(cursor)).ok()?;
        let mut chunk_header = [0; 8];
        file.read_exact(&mut chunk_header).ok()?;
        let chunk_size = u64::from(u32::from_be_bytes(chunk_header[4..8].try_into().ok()?));
        let body_start = cursor.checked_add(8)?;
        let body_end = body_start.checked_add(chunk_size)?;
        if body_end > form_end {
            return None;
        }

        if &chunk_header[..4] == b"INST" && !instrument_chunk_seen {
            instrument_chunk_seen = true;
            if chunk_size == 20 {
                file.seek(SeekFrom::Start(body_start)).ok()?;
                let mut instrument = [0; 20];
                file.read_exact(&mut instrument).ok()?;
                gain_db = Some(i16::from_be_bytes([instrument[6], instrument[7]]));
                let detune_cents = i8::from_be_bytes([instrument[1]]);
                if (-50..=50).contains(&detune_cents) {
                    fine_tune_cents = Some(f64::from(detune_cents));
                }
                if (1..=127).contains(&instrument[0]) {
                    root_key = Some(u16::from(instrument[0]));
                }
                let low_note = instrument[2];
                let high_note = instrument[3];
                if low_note <= high_note && high_note <= 127 {
                    key_range = Some((low_note, high_note));
                }
                let low_velocity = instrument[4];
                let high_velocity = instrument[5];
                if (1..=127).contains(&low_velocity)
                    && (1..=127).contains(&high_velocity)
                    && low_velocity <= high_velocity
                {
                    velocity_range = Some((low_velocity, high_velocity));
                }
                sustain_loop = Some((
                    i16::from_be_bytes([instrument[8], instrument[9]]),
                    i16::from_be_bytes([instrument[10], instrument[11]]),
                    i16::from_be_bytes([instrument[12], instrument[13]]),
                ));
                release_loop = Some((
                    i16::from_be_bytes([instrument[14], instrument[15]]),
                    i16::from_be_bytes([instrument[16], instrument[17]]),
                    i16::from_be_bytes([instrument[18], instrument[19]]),
                ));
            }
        } else if &chunk_header[..4] == b"MARK" && !marker_chunk_seen {
            marker_chunk_seen = true;
            marker_positions = read_aiff_marker_positions(&mut file, body_start, chunk_size);
        }

        cursor = body_end.checked_add(chunk_size & 1)?;
        if cursor > form_end {
            return None;
        }
    }
    if cursor != form_end {
        return None;
    }

    let (loop_points, ping_pong_loop) =
        resolve_aiff_loop(sustain_loop, marker_positions.as_deref())
            .map_or((None, None), |(points, ping_pong)| {
                (Some(points), Some(ping_pong))
            });
    let (release_loop_points, release_ping_pong_loop) =
        resolve_aiff_loop(release_loop, marker_positions.as_deref())
            .map_or((None, None), |(points, ping_pong)| {
                (Some(points), Some(ping_pong))
            });
    let metadata = SamplerFileMetadata {
        root_key,
        fine_tune_cents,
        gain_db,
        key_range,
        velocity_range,
        loop_points,
        ping_pong_loop,
        release_loop_points,
        release_ping_pong_loop,
    };
    (metadata.root_key.is_some()
        || metadata.gain_db.is_some()
        || metadata.key_range.is_some()
        || metadata.velocity_range.is_some()
        || metadata.loop_points.is_some()
        || metadata.release_loop_points.is_some())
    .then_some(metadata)
}

fn resolve_aiff_loop(
    loop_record: Option<(i16, i16, i16)>,
    marker_positions: Option<&[(i16, usize)]>,
) -> Option<((usize, usize), bool)> {
    let (play_mode, begin_marker, end_marker) = loop_record?;
    if !matches!(play_mode, 1 | 2) {
        return None;
    }
    let markers = marker_positions?;
    let start = markers
        .iter()
        .find_map(|(marker_id, frame)| (*marker_id == begin_marker).then_some(*frame))?;
    let end = markers
        .iter()
        .find_map(|(marker_id, frame)| (*marker_id == end_marker).then_some(*frame))?;
    (start < end).then_some(((start, end), play_mode == 2))
}

fn read_aiff_marker_positions(
    file: &mut File,
    body_start: u64,
    chunk_size: u64,
) -> Option<Vec<(i16, usize)>> {
    const MAX_MARKER_CHUNK_BYTES: u64 = 18 * 1024 * 1024;

    if !(2..=MAX_MARKER_CHUNK_BYTES).contains(&chunk_size) {
        return None;
    }
    file.seek(SeekFrom::Start(body_start)).ok()?;
    let mut count_bytes = [0; 2];
    file.read_exact(&mut count_bytes).ok()?;
    let marker_count = usize::from(u16::from_be_bytes(count_bytes));
    let mut consumed = 2u64;
    let mut positions: Vec<(i16, usize)> = Vec::with_capacity(marker_count.min(4096));
    let mut duplicate_ids = Vec::new();

    for _ in 0..marker_count {
        if consumed.checked_add(7)? > chunk_size {
            return None;
        }
        let mut marker_header = [0; 7];
        file.read_exact(&mut marker_header).ok()?;
        let marker_id = i16::from_be_bytes([marker_header[0], marker_header[1]]);
        let frame = u32::from_be_bytes([
            marker_header[2],
            marker_header[3],
            marker_header[4],
            marker_header[5],
        ]) as usize;
        let name_length = u64::from(marker_header[6]);
        let record_size = 7u64.checked_add(name_length)?;
        let padded_record_size = record_size.checked_add(record_size & 1)?;
        consumed = consumed.checked_add(padded_record_size)?;
        if consumed > chunk_size {
            return None;
        }
        file.seek(SeekFrom::Start(body_start.checked_add(consumed)?))
            .ok()?;

        if marker_id <= 0 || duplicate_ids.contains(&marker_id) {
            continue;
        }
        if let Some(existing_index) = positions
            .iter()
            .position(|(existing_id, _)| *existing_id == marker_id)
        {
            positions.remove(existing_index);
            duplicate_ids.push(marker_id);
        } else {
            positions.push((marker_id, frame));
        }
    }

    (consumed == chunk_size).then_some(positions)
}

/// Read sampler metadata from the supported WAVE and AIFF-family chunk formats.
pub(crate) fn sample_sampler_metadata(path: &Path) -> Option<SamplerFileMetadata> {
    wav_sampler_metadata(path).or_else(|| aiff_sampler_metadata(path))
}

/// Decode an audio file for the Browser and return interleaved stereo samples at the output rate.
/// A duration limit keeps the default short preview bounded; `None` returns the full sample.
pub fn decode_audio_preview(
    path: impl AsRef<Path>,
    output_sample_rate: u32,
    maximum_seconds: Option<f64>,
) -> Result<Vec<f32>, String> {
    if !(8_000..=384_000).contains(&output_sample_rate) {
        return Err("Browser preview output rate must be between 8 kHz and 384 kHz".into());
    }
    if maximum_seconds.is_some_and(|seconds| !seconds.is_finite() || seconds <= 0.0) {
        return Err("Browser preview duration must be finite and greater than zero".into());
    }
    let path = path.as_ref();
    let audio = decode_audio_file_for_preview(path, maximum_seconds)?;
    decoded_audio_to_stereo_samples(&audio, output_sample_rate, maximum_seconds).map_err(|error| {
        format!(
            "could not prepare sample preview {}: {error}",
            path.display()
        )
    })
}

/// Decode a Browser preview starting at a normalized position in the full source sample.
/// The returned preview is bounded by `maximum_seconds`, like `decode_audio_preview`.
pub fn decode_audio_preview_from_position(
    path: impl AsRef<Path>,
    output_sample_rate: u32,
    maximum_seconds: Option<f64>,
    start_fraction: f64,
) -> Result<Vec<f32>, String> {
    if !(8_000..=384_000).contains(&output_sample_rate) {
        return Err("Browser preview output rate must be between 8 kHz and 384 kHz".into());
    }
    if maximum_seconds.is_some_and(|seconds| !seconds.is_finite() || seconds <= 0.0) {
        return Err("Browser preview duration must be finite and greater than zero".into());
    }
    if !start_fraction.is_finite() || !(0.0..=1.0).contains(&start_fraction) {
        return Err("Browser preview start position must be between 0 and 1".into());
    }
    let path = path.as_ref();
    let audio = decode_audio_file_for_preview(path, None)?;
    if audio.frame_count() == 0 {
        return Err(format!("sample {} has no audio frames", path.display()));
    }
    let start_frame = ((audio.frame_count() - 1) as f64 * start_fraction).round() as usize;
    decoded_audio_to_stereo_samples_from(&audio, output_sample_rate, maximum_seconds, start_frame)
        .map_err(|error| {
            format!(
                "could not prepare sample preview {}: {error}",
                path.display()
            )
        })
}

fn decoded_audio_to_stereo_samples(
    audio: &DecodedAudio,
    output_sample_rate: u32,
    maximum_seconds: Option<f64>,
) -> Result<Vec<f32>, String> {
    decoded_audio_to_stereo_samples_from(audio, output_sample_rate, maximum_seconds, 0)
}

fn decoded_audio_to_stereo_samples_from(
    audio: &DecodedAudio,
    output_sample_rate: u32,
    maximum_seconds: Option<f64>,
    start_frame: usize,
) -> Result<Vec<f32>, String> {
    if audio.channels.is_empty() || audio.channels.len() > 2 || audio.sample_rate == 0 {
        return Err("only mono and stereo samples with a valid rate can be previewed".into());
    }
    let source_frames = audio.frame_count();
    if source_frames == 0
        || audio
            .channels
            .iter()
            .any(|channel| channel.len() != source_frames)
    {
        return Err("sample has no complete audio frames".into());
    }
    if start_frame >= source_frames {
        return Err("sample preview start position is outside the source".into());
    }
    let remaining_frames = source_frames - start_frame;
    let duration = remaining_frames as f64 / f64::from(audio.sample_rate);
    let duration = maximum_seconds.map_or(duration, |maximum| duration.min(maximum));
    let output_frames_f64 = (duration * f64::from(output_sample_rate)).ceil();
    let maximum_output_frames = MAX_DECODED_SAMPLE_BYTES / (2 * std::mem::size_of::<f32>());
    if !output_frames_f64.is_finite() || output_frames_f64 > maximum_output_frames as f64 {
        return Err(format!(
            "sample preview exceeds the {} MiB output limit",
            MAX_DECODED_SAMPLE_BYTES / (1024 * 1024)
        ));
    }
    let output_frames = (output_frames_f64 as usize).min(maximum_output_frames);
    if output_frames == 0 {
        return Err("sample preview contains no output frames".into());
    }
    let sample_count = output_frames
        .checked_mul(2)
        .ok_or_else(|| "sample preview size overflow".to_owned())?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(sample_count)
        .map_err(|error| format!("could not allocate sample preview: {error}"))?;
    let source_step = f64::from(audio.sample_rate) / f64::from(output_sample_rate);
    for frame in 0..output_frames {
        let position =
            (start_frame as f64 + frame as f64 * source_step).min((source_frames - 1) as f64);
        let first = position.floor() as usize;
        let second = (first + 1).min(source_frames - 1);
        let fraction = (position - first as f64) as f32;
        let interpolate = |channel: &[f32]| {
            let left = finite_sample(channel[first]);
            let right = finite_sample(channel[second]);
            (left + (right - left) * fraction).clamp(-1.0, 1.0)
        };
        let left = interpolate(&audio.channels[0]);
        let right = if audio.channels.len() == 1 {
            left
        } else {
            interpolate(&audio.channels[1])
        };
        output.extend_from_slice(&[left, right]);
    }
    Ok(output)
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn decode_audio_file_for_preview(
    path: &Path,
    maximum_seconds: Option<f64>,
) -> Result<DecodedAudio, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("could not open sample {}: {error}", path.display()))?;
    let mut signature = [0; 4];
    if file.read_exact(&mut signature).is_ok() && &signature == b"wvpk" {
        return decode_wavpack_file(file, path, maximum_seconds);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not seek sample {}: {error}", path.display()))?;
    let embedded_ogg_offset = find_embedded_ogg_offset(&mut file)
        .map_err(|error| format!("could not inspect sample {}: {error}", path.display()))?;
    let source: Box<dyn MediaSource> = match embedded_ogg_offset {
        Some(offset) => Box::new(OffsetMediaSource::new(file, offset).map_err(|error| {
            format!("could not read embedded sample {}: {error}", path.display())
        })?),
        None => Box::new(file),
    };
    let source = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("could not identify sample {}: {error}", path.display()))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| format!("sample {} has no audio track", path.display()))?;
    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .cloned()
        .ok_or_else(|| format!("sample {} has no audio codec parameters", path.display()))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|error| format!("could not initialize sample decoder: {error}"))?;

    let mut output_channels = Vec::<Vec<f32>>::new();
    let mut output_sample_rate = None;
    let mut total_samples = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(DecodeError::ResetRequired) => {
                return Err(format!(
                    "sample {} contains chained audio streams that need a refreshed track selection",
                    path.display()
                ));
            }
            Err(error) => {
                return Err(format!("could not read sample {}: {error}", path.display()));
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|error| format!("could not decode sample {}: {error}", path.display()))?;
        let channel_count = decoded.spec().channels().count();
        let sample_rate = decoded.spec().rate();
        if channel_count == 0 || sample_rate == 0 {
            return Err(format!(
                "sample {} has an invalid channel count or sample rate",
                path.display()
            ));
        }
        if let Some(output_rate) = output_sample_rate {
            if output_rate != sample_rate || output_channels.len() != channel_count {
                return Err(format!(
                    "sample {} changes sample rate or channel layout while decoding",
                    path.display()
                ));
            }
        } else {
            output_sample_rate = Some(sample_rate);
            output_channels.resize_with(channel_count, Vec::new);
        }

        let frame_count = decoded.frames();
        let frames_already_decoded = output_channels.first().map_or(0, Vec::len);
        let frame_limit = maximum_seconds
            .map(|seconds| (seconds * f64::from(sample_rate)).ceil() as usize)
            .unwrap_or(usize::MAX);
        let frames_to_keep = frame_count.min(frame_limit.saturating_sub(frames_already_decoded));
        if frames_to_keep == 0 {
            break;
        }
        let sample_count = frames_to_keep
            .checked_mul(channel_count)
            .ok_or_else(|| "decoded sample size overflow".to_owned())?;
        total_samples = total_samples
            .checked_add(sample_count)
            .filter(|count| {
                count
                    .checked_mul(std::mem::size_of::<f32>())
                    .is_some_and(|bytes| bytes <= MAX_DECODED_SAMPLE_BYTES)
            })
            .ok_or_else(|| {
                format!(
                    "sample {} exceeds the {} MiB in-memory decode limit",
                    path.display(),
                    MAX_DECODED_SAMPLE_BYTES / (1024 * 1024)
                )
            })?;
        for channel in &mut output_channels {
            channel
                .try_reserve(frames_to_keep)
                .map_err(|error| format!("could not allocate sample buffers: {error}"))?;
        }

        let mut interleaved = vec![f32::MID; decoded.samples_interleaved()];
        decoded.copy_to_slice_interleaved(&mut interleaved);
        for frame in interleaved[..sample_count].chunks_exact(channel_count) {
            for (channel, sample) in output_channels.iter_mut().zip(frame) {
                channel.push(*sample);
            }
        }
        if frames_already_decoded + frames_to_keep >= frame_limit {
            break;
        }
    }

    let sample_rate = output_sample_rate
        .filter(|_| total_samples > 0)
        .ok_or_else(|| format!("sample {} contains no decodable audio", path.display()))?;
    Ok(DecodedAudio {
        sample_rate,
        channels: output_channels,
    })
}

/// Decode an audio source and reduce it to bounded min/max waveform buckets.
pub fn decode_audio_waveform(
    path: impl AsRef<Path>,
    maximum_buckets: usize,
) -> Result<AudioWaveform, String> {
    let path = path.as_ref();
    let audio = decode_audio_file(path)?;
    if audio.frame_count() == 0 {
        return Err(format!(
            "sample {} contains no audio frames",
            path.display()
        ));
    }
    summarize_audio_waveform(&audio, maximum_buckets)
}

/// Summarize decoded audio as bounded waveform peaks and a log-frequency spectrum.
pub fn summarize_audio_waveform(
    audio: &DecodedAudio,
    maximum_buckets: usize,
) -> Result<AudioWaveform, String> {
    let frame_count = audio.frame_count();
    if frame_count == 0 {
        return Err("sample contains no audio frames".to_owned());
    }
    let maximum_buckets = maximum_buckets.max(1);
    let bucket_count = frame_count.min(maximum_buckets);
    let mut peaks = Vec::new();
    peaks
        .try_reserve_exact(bucket_count)
        .map_err(|error| format!("could not allocate waveform preview: {error}"))?;
    let mut channel_peaks = Vec::new();
    channel_peaks
        .try_reserve_exact(audio.channels.len())
        .map_err(|error| format!("could not allocate waveform channels: {error}"))?;
    for _ in &audio.channels {
        let mut channel = Vec::new();
        channel
            .try_reserve_exact(bucket_count)
            .map_err(|error| format!("could not allocate waveform preview: {error}"))?;
        channel_peaks.push(channel);
    }
    for bucket_index in 0..bucket_count {
        let start = bucket_index * frame_count / bucket_count;
        let end = ((bucket_index + 1) * frame_count / bucket_count).max(start + 1);
        let mut combined_minimum = f32::INFINITY;
        let mut combined_maximum = f32::NEG_INFINITY;
        for (channel_index, channel) in audio.channels.iter().enumerate() {
            let mut minimum = f32::INFINITY;
            let mut maximum = f32::NEG_INFINITY;
            for sample in &channel[start..end] {
                let sample = if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                minimum = minimum.min(sample);
                maximum = maximum.max(sample);
            }
            channel_peaks[channel_index].push(WaveformPeak { minimum, maximum });
            combined_minimum = combined_minimum.min(minimum);
            combined_maximum = combined_maximum.max(maximum);
        }
        peaks.push(WaveformPeak {
            minimum: combined_minimum,
            maximum: combined_maximum,
        });
    }
    let spectral_bands = summarize_spectral_bands(audio)?;
    Ok(AudioWaveform {
        sample_rate: audio.sample_rate,
        frame_count: frame_count as u64,
        peaks,
        channel_peaks,
        spectral_bands,
    })
}

fn summarize_spectral_bands(audio: &DecodedAudio) -> Result<Vec<Vec<u8>>, String> {
    let frame_count = audio.frame_count();
    if frame_count == 0 || audio.channels.is_empty() {
        return Ok(Vec::new());
    }
    let mut columns = Vec::new();
    columns
        .try_reserve_exact(WAVEFORM_SPECTRUM_COLUMNS)
        .map_err(|error| format!("could not allocate spectral preview: {error}"))?;
    let mut mono_window = [0.0; WAVEFORM_SPECTRUM_WINDOW];
    for column in 0..WAVEFORM_SPECTRUM_COLUMNS {
        let center = (frame_count as u128 * (2 * column + 1) as u128
            / (2 * WAVEFORM_SPECTRUM_COLUMNS) as u128) as isize;
        let start = center - (WAVEFORM_SPECTRUM_WINDOW / 2) as isize;
        for (window_index, destination) in mono_window.iter_mut().enumerate() {
            let source_index = start + window_index as isize;
            *destination = if (0..frame_count as isize).contains(&source_index) {
                let sum = audio
                    .channels
                    .iter()
                    .map(|channel| finite_audio_value(channel[source_index as usize]))
                    .sum::<f32>();
                sum / audio.channels.len() as f32
            } else {
                0.0
            };
        }
        columns.push(spectral_band_intensities(&mono_window));
    }
    Ok(columns)
}

fn spectral_band_intensities(samples: &[f32; WAVEFORM_SPECTRUM_WINDOW]) -> Vec<u8> {
    let magnitudes = windowed_fft_magnitudes(samples);
    let mut bands = Vec::with_capacity(WAVEFORM_SPECTRUM_BANDS);
    for band in 0..WAVEFORM_SPECTRUM_BANDS {
        let first_bin = (2.0f64
            .powf(band as f64 * 7.0 / WAVEFORM_SPECTRUM_BANDS as f64)
            .floor() as usize)
            .clamp(1, magnitudes.len() - 1);
        let end_bin = (2.0f64
            .powf((band + 1) as f64 * 7.0 / WAVEFORM_SPECTRUM_BANDS as f64)
            .ceil() as usize)
            .clamp(first_bin + 1, magnitudes.len());
        let magnitude = magnitudes[first_bin..end_bin]
            .iter()
            .copied()
            .fold(0.0f32, f32::max);
        let decibels = 20.0 * magnitude.max(1.0e-6).log10();
        let intensity = ((decibels + 72.0) / 72.0).clamp(0.0, 1.0);
        bands.push((intensity * 255.0).round() as u8);
    }
    bands
}

fn windowed_fft_magnitudes(samples: &[f32; WAVEFORM_SPECTRUM_WINDOW]) -> Vec<f32> {
    let mut real = [0.0; WAVEFORM_SPECTRUM_WINDOW];
    let mut imaginary = [0.0; WAVEFORM_SPECTRUM_WINDOW];
    let mut window_sum = 0.0;
    for (index, &sample) in samples.iter().enumerate() {
        let window = 0.5
            - 0.5
                * (2.0 * std::f64::consts::PI * index as f64
                    / (WAVEFORM_SPECTRUM_WINDOW - 1) as f64)
                    .cos();
        real[index] = f64::from(finite_audio_value(sample)) * window;
        window_sum += window;
    }

    let mut reversed = 0;
    for index in 1..WAVEFORM_SPECTRUM_WINDOW {
        let mut bit = WAVEFORM_SPECTRUM_WINDOW >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            real.swap(index, reversed);
            imaginary.swap(index, reversed);
        }
    }

    let mut stage_size = 2;
    while stage_size <= WAVEFORM_SPECTRUM_WINDOW {
        let angle = -2.0 * std::f64::consts::PI / stage_size as f64;
        let step_real = angle.cos();
        let step_imaginary = angle.sin();
        for stage_start in (0..WAVEFORM_SPECTRUM_WINDOW).step_by(stage_size) {
            let mut twiddle_real = 1.0;
            let mut twiddle_imaginary = 0.0;
            for offset in 0..stage_size / 2 {
                let even = stage_start + offset;
                let odd = even + stage_size / 2;
                let odd_real = real[odd] * twiddle_real - imaginary[odd] * twiddle_imaginary;
                let odd_imaginary = real[odd] * twiddle_imaginary + imaginary[odd] * twiddle_real;
                real[odd] = real[even] - odd_real;
                imaginary[odd] = imaginary[even] - odd_imaginary;
                real[even] += odd_real;
                imaginary[even] += odd_imaginary;
                let next_real = twiddle_real * step_real - twiddle_imaginary * step_imaginary;
                twiddle_imaginary = twiddle_real * step_imaginary + twiddle_imaginary * step_real;
                twiddle_real = next_real;
            }
        }
        stage_size *= 2;
    }

    let scale = 2.0 / window_sum.max(1.0);
    real[..WAVEFORM_SPECTRUM_WINDOW / 2]
        .iter()
        .zip(&imaginary[..WAVEFORM_SPECTRUM_WINDOW / 2])
        .map(|(&real, &imaginary)| (real.hypot(imaginary) * scale) as f32)
        .collect()
}

fn finite_audio_value(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn decode_wavpack_file(
    mut file: File,
    path: &Path,
    maximum_seconds: Option<f64>,
) -> Result<DecodedAudio, String> {
    let file_length = file
        .metadata()
        .map_err(|error| {
            format!(
                "could not inspect WavPack sample {}: {error}",
                path.display()
            )
        })?
        .len();
    let file_length = usize::try_from(file_length)
        .map_err(|_| format!("WavPack sample {} is too large", path.display()))?;
    if file_length > MAX_DECODED_SAMPLE_BYTES {
        return Err(format!(
            "WavPack sample {} exceeds the {} MiB compressed input limit",
            path.display(),
            MAX_DECODED_SAMPLE_BYTES / (1024 * 1024)
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not seek WavPack sample {}: {error}", path.display()))?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(file_length)
        .map_err(|error| format!("could not allocate WavPack input buffer: {error}"))?;
    file.read_to_end(&mut encoded)
        .map_err(|error| format!("could not read WavPack sample {}: {error}", path.display()))?;

    let stream_info = wavicle::StreamInfo::scan(&encoded).map_err(|error| {
        format!(
            "could not inspect WavPack sample {}: {error}",
            path.display()
        )
    })?;
    if !(1..=2).contains(&stream_info.channels) || stream_info.sample_rate == 0 {
        return Err(format!(
            "WavPack sample {} has an unsupported channel count or sample rate",
            path.display()
        ));
    }
    let total_sample_count = wavicle::Blocks::new(&encoded).try_fold(0u64, |total, block| {
        let block = block.map_err(|error| error.to_string())?;
        let count = u64::from(block.header.block_samples)
            .checked_mul(u64::from(block.header.flags.output_channels()))
            .and_then(|count| total.checked_add(count))
            .ok_or_else(|| "WavPack decoded sample count overflow".to_owned())?;
        Ok::<u64, String>(count)
    })?;
    let channel_count = u64::from(stream_info.channels);
    let total_frame_count = stream_info
        .total_samples
        .unwrap_or(total_sample_count / channel_count);
    let requested_frames = maximum_seconds
        .map(|seconds| (seconds * f64::from(stream_info.sample_rate)).ceil() as u64)
        .unwrap_or(total_frame_count)
        .min(total_frame_count);
    if requested_frames == 0 {
        return Err(format!(
            "WavPack sample {} contains no audio frames",
            path.display()
        ));
    }
    let mut prefix_length = 0usize;
    let mut prefix_frames = 0u64;
    for block in wavicle::Blocks::new(&encoded) {
        let block = block.map_err(|error| error.to_string())?;
        if block.header.block_samples > 0 && block.header.block_index >= requested_frames {
            break;
        }
        prefix_length = prefix_length
            .checked_add(block.header.block_len())
            .ok_or_else(|| "WavPack preview size overflow".to_owned())?;
        let block_end = block
            .header
            .block_index
            .checked_add(u64::from(block.header.block_samples))
            .ok_or_else(|| "WavPack preview frame count overflow".to_owned())?;
        prefix_frames = prefix_frames.max(block_end);
    }
    let _decoded_bytes = prefix_frames
        .checked_mul(channel_count)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>() as u64))
        .filter(|bytes| *bytes <= MAX_DECODED_SAMPLE_BYTES as u64)
        .ok_or_else(|| {
            format!(
                "WavPack sample {} exceeds the {} MiB decoded sample limit",
                path.display(),
                MAX_DECODED_SAMPLE_BYTES / (1024 * 1024)
            )
        })?;
    let decoded = wavicle::decode_stream(&encoded[..prefix_length]).map_err(|error| {
        format!(
            "could not decode WavPack sample {}: {error}",
            path.display()
        )
    })?;
    if decoded.channels != stream_info.channels || decoded.sample_rate != stream_info.sample_rate {
        return Err(format!(
            "WavPack sample {} changed format while decoding",
            path.display()
        ));
    }
    let channel_count = usize::try_from(decoded.channels)
        .map_err(|_| "WavPack channel count does not fit this platform".to_owned())?;
    if !decoded.samples.len().is_multiple_of(channel_count) {
        return Err(format!(
            "WavPack sample {} returned an incomplete frame",
            path.display()
        ));
    }
    let frame_count = decoded.samples.len() / channel_count;
    let mut channels = vec![Vec::<f32>::new(); channel_count];
    for channel in &mut channels {
        channel
            .try_reserve_exact(frame_count)
            .map_err(|error| format!("could not allocate decoded WavPack channels: {error}"))?;
    }
    let scale = 2f32.powi((decoded.bits_per_sample.saturating_sub(1)) as i32);
    for frame in decoded
        .samples
        .chunks_exact(channel_count)
        .take(frame_count)
    {
        for (channel, sample) in channels.iter_mut().zip(frame) {
            let value = if decoded.is_float {
                f32::from_bits(*sample as u32)
            } else {
                *sample as f32 / scale
            };
            channel.push(value);
        }
    }
    Ok(DecodedAudio {
        sample_rate: decoded.sample_rate,
        channels,
    })
}

fn find_embedded_ogg_offset(file: &mut File) -> io::Result<Option<u64>> {
    let file_length = file.metadata()?.len();
    if file_length < 12 {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(0))?;
    let mut header = [0; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Ok(None);
    }

    let mut chunk_offset = 12u64;
    while chunk_offset
        .checked_add(8)
        .is_some_and(|end| end <= file_length)
    {
        file.seek(SeekFrom::Start(chunk_offset))?;
        let mut chunk_header = [0; 8];
        file.read_exact(&mut chunk_header)?;
        let chunk_length = u64::from(u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]));
        let data_offset = chunk_offset + 8;
        let Some(chunk_end) = data_offset.checked_add(chunk_length) else {
            return Ok(None);
        };
        if chunk_end > file_length {
            return Ok(None);
        }
        if &chunk_header[..4] == b"data" && chunk_length >= 4 {
            file.seek(SeekFrom::Start(data_offset))?;
            let mut signature = [0; 4];
            file.read_exact(&mut signature)?;
            if &signature == b"OggS" {
                return Ok(Some(data_offset));
            }
        }
        let padded_length = chunk_length + (chunk_length & 1);
        let Some(next_offset) = data_offset.checked_add(padded_length) else {
            return Ok(None);
        };
        if next_offset <= chunk_offset {
            return Ok(None);
        }
        chunk_offset = next_offset;
    }
    Ok(None)
}

/// Resolves sample references against a project folder and FL Studio installation roots.
///
/// Factory roots can be supplied explicitly with [`with_factory_root`](Self::with_factory_root).
/// Automatic discovery also checks `FLStudioFactoryData`, `FL_STUDIO_FACTORY_DATA`, and
/// `FL_STUDIO_ROOT`, then common installation directories on the current platform.
#[derive(Clone, Debug)]
pub struct SamplePathResolver {
    project_file: PathBuf,
    factory_roots: Vec<PathBuf>,
}

/// Finds the installed FL Studio Empty.flp template used for MIDI imports that start a project.
/// Set `FL_STUDIO_EMPTY_PROJECT` to override automatic installation discovery.
pub fn find_empty_project_template() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FL_STUDIO_EMPTY_PROJECT").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    find_empty_project_template_in_roots(&discover_factory_roots())
}

fn find_empty_project_template_in_roots(roots: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
        for candidate in [
            root.join("Data/Templates/Empty/Empty.flp"),
            root.join("Templates/Empty/Empty.flp"),
            root.join("Resources/FL/Data/Templates/Empty/Empty.flp"),
            root.join("Contents/Resources/FL/Data/Templates/Empty/Empty.flp"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

impl SamplePathResolver {
    pub fn new(project_file: impl AsRef<Path>) -> Self {
        Self {
            project_file: project_file.as_ref().to_path_buf(),
            factory_roots: discover_factory_roots(),
        }
    }

    /// Add an installation root, searched before automatically discovered roots.
    ///
    /// The root may be the FL Studio installation directory or its `Data` directory.
    pub fn with_factory_root(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        if !self.factory_roots.contains(&root) {
            self.factory_roots.insert(0, root);
        }
        self
    }

    pub fn factory_roots(&self) -> &[PathBuf] {
        &self.factory_roots
    }

    /// Resolve a decoded FLP sample path to an existing file.
    pub fn resolve(&self, sample_path: &str) -> Result<PathBuf, String> {
        if sample_path.is_empty() {
            return Err("sample path is empty".to_owned());
        }

        if let Some(suffix) = sample_path
            .get(..FACTORY_DATA_MACRO.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(FACTORY_DATA_MACRO))
            .map(|_| &sample_path[FACTORY_DATA_MACRO.len()..])
        {
            let relative_suffix =
                PathBuf::from(normalize_separators(suffix.trim_start_matches(['\\', '/'])));
            for root in &self.factory_roots {
                for candidate in factory_root_candidates(root, &relative_suffix) {
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                }
            }
            if self.factory_roots.is_empty() {
                return Err(format!(
                    "cannot expand {FACTORY_DATA_MACRO}; set FL_STUDIO_ROOT or provide a factory root"
                ));
            }
            return Err(format!(
                "sample file was not found for factory path {sample_path:?}"
            ));
        }

        let expanded = expand_environment_tokens(sample_path)?;
        let normalized = PathBuf::from(normalize_separators(&expanded));
        if normalized.is_absolute() || is_windows_absolute(&expanded) {
            if normalized.is_file() {
                return Ok(normalized);
            }
            return Err(format!("sample file does not exist: {sample_path}"));
        }

        let project_directory = self.project_file.parent().unwrap_or_else(|| Path::new("."));
        let project_relative = project_directory.join(&normalized);
        if project_relative.is_file() {
            return Ok(project_relative);
        }
        for root in &self.factory_roots {
            for candidate in factory_root_candidates(root, &normalized) {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(format!(
            "sample file was not found relative to the project or known FL Studio roots: {sample_path}"
        ))
    }
}

fn factory_root_candidates(root: &Path, suffix: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![root.join(suffix)];
    if root
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Data"))
        && let Some(parent) = root.parent()
    {
        candidates.push(parent.join(suffix));
    }
    candidates
}

fn normalize_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.replace('\\', "/")
    }
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3 && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/'))
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

fn expand_environment_tokens(value: &str) -> Result<String, String> {
    let mut expanded = String::with_capacity(value.len());
    let mut cursor = 0usize;
    while let Some(relative_start) = value[cursor..].find('%') {
        let start = cursor + relative_start;
        expanded.push_str(&value[cursor..start]);
        let token_start = start + 1;
        let Some(relative_end) = value[token_start..].find('%') else {
            return Err(format!(
                "unterminated environment token in sample path {value:?}"
            ));
        };
        let end = token_start + relative_end;
        let name = &value[token_start..end];
        let replacement = std::env::var_os(name)
            .or_else(|| alias_environment_variable(name))
            .ok_or_else(|| format!("sample path uses unset environment variable %{name}%"))?;
        expanded.push_str(&replacement.to_string_lossy());
        cursor = end + 1;
    }
    expanded.push_str(&value[cursor..]);
    Ok(expanded)
}

fn alias_environment_variable(name: &str) -> Option<std::ffi::OsString> {
    let alias = match name.to_ascii_lowercase().as_str() {
        "flstudiofactorydata" => "FL_STUDIO_FACTORY_DATA",
        "flstudiouserdata" => "FL_STUDIO_USER_DATA",
        _ => return None,
    };
    std::env::var_os(alias)
}

fn discover_factory_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in [
        "FLStudioFactoryData",
        "FL_STUDIO_FACTORY_DATA",
        "FL_STUDIO_ROOT",
    ] {
        if let Some(root) = std::env::var_os(variable).map(PathBuf::from) {
            roots.push(root);
        }
    }

    #[cfg(windows)]
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(program_files) = std::env::var_os(variable).map(PathBuf::from) else {
            continue;
        };
        let image_line = program_files.join("Image-Line");
        if let Ok(entries) = std::fs::read_dir(image_line) {
            let mut installations = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path.file_name().is_some_and(|name| {
                            name.to_string_lossy()
                                .to_ascii_lowercase()
                                .starts_with("fl studio")
                        })
                })
                .collect::<Vec<_>>();
            installations.sort_by(|left, right| right.cmp(left));
            roots.extend(installations);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let app_resources = PathBuf::from("/Applications/FL Studio.app/Contents/Resources/FL");
        if app_resources.is_dir() {
            roots.push(app_resources);
        }
    }

    roots.dedup();
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(1);

    fn fixture_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "flp-rebuild-media-test-{}-{}",
            std::process::id(),
            NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn pcm16_wav_fixture() -> Vec<u8> {
        let samples = [0x00, 0x80, 0xFF, 0x7F];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&44_100u32.to_le_bytes());
        bytes.extend_from_slice(&88_200u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&samples);
        bytes
    }

    #[test]
    fn writes_selected_audio_frames_to_a_new_interleaved_float_wav() {
        let root = fixture_root();
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let output_path = root.join("trimmed.wav");
        let audio = DecodedAudio {
            sample_rate: 48_000,
            channels: vec![vec![0.0, 0.25, -0.5, 1.0], vec![0.1, 0.2, 0.3, 0.4]],
        };

        write_audio_range_to_wav(&audio, 1..3, &output_path)
            .expect("selected source frames should be written");

        let bytes = fs::read(&output_path).expect("trimmed WAV should be readable");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 52);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        let samples = bytes[44..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&sample| f32::from_le_bytes(sample))
            .collect::<Vec<_>>();
        assert_eq!(samples, [0.25, 0.2, -0.5, 0.3]);
        fs::remove_dir_all(root).expect("fixture directory should be removed");
    }

    #[test]
    fn writes_selected_audio_with_linear_edge_fades_without_changing_source() {
        let root = fixture_root();
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let output_path = root.join("faded.wav");
        let audio = DecodedAudio {
            sample_rate: 48_000,
            channels: vec![vec![1.0; 4], vec![0.5; 4]],
        };
        let original = audio.clone();

        write_audio_range_to_wav_with_fades(&audio, 0..4, 2, 2, &output_path)
            .expect("selected source frames should be faded and written");

        let bytes = fs::read(&output_path).expect("faded WAV should be readable");
        let samples = bytes[44..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&sample| f32::from_le_bytes(sample))
            .collect::<Vec<_>>();
        assert_eq!(samples, [0.0, 0.0, 0.5, 0.25, 0.5, 0.25, 0.0, 0.0]);
        assert_eq!(audio, original);
        fs::remove_dir_all(root).expect("fixture directory should be removed");
    }

    #[test]
    fn peak_normalizes_only_selected_audio_to_requested_dbfs() {
        let root = fixture_root();
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let output_path = root.join("normalized.wav");
        let audio = DecodedAudio {
            sample_rate: 48_000,
            channels: vec![vec![0.25, -0.5, 1.0, 0.75]],
        };

        write_audio_range_to_wav_with_processing(
            &audio,
            0..3,
            0,
            0,
            false,
            Some(-6.020_6),
            &output_path,
        )
        .expect("selected audio should be peak-normalized");

        let bytes = fs::read(&output_path).expect("normalized WAV should be readable");
        let samples = bytes[44..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&sample| f32::from_le_bytes(sample))
            .collect::<Vec<_>>();
        assert!((samples[0] - 0.125).abs() < 1.0e-5);
        assert!((samples[1] + 0.25).abs() < 1.0e-5);
        assert!((samples[2] - 0.5).abs() < 1.0e-5);
        assert_eq!(samples.len(), 3);
        fs::remove_dir_all(root).expect("fixture directory should be removed");
    }

    #[test]
    fn reverses_only_the_selected_audio_frames() {
        let root = fixture_root();
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let output_path = root.join("reversed.wav");
        let audio = DecodedAudio {
            sample_rate: 48_000,
            channels: vec![vec![0.0, 0.25, 0.5, 0.75]],
        };

        write_audio_range_to_wav_with_processing(&audio, 1..4, 0, 0, true, None, &output_path)
            .expect("selected audio frames should be reversed");

        let bytes = fs::read(&output_path).expect("reversed WAV should be readable");
        let samples = bytes[44..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&sample| f32::from_le_bytes(sample))
            .collect::<Vec<_>>();
        assert_eq!(samples, [0.75, 0.5, 0.25]);
        fs::remove_dir_all(root).expect("fixture directory should be removed");
    }

    #[test]
    fn rejects_empty_or_out_of_bounds_audio_selections() {
        let root = fixture_root();
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let output_path = root.join("trimmed.wav");
        let audio = DecodedAudio {
            sample_rate: 48_000,
            channels: vec![vec![0.0, 0.25]],
        };
        assert!(write_audio_range_to_wav(&audio, 1..1, &output_path).is_err());
        assert!(write_audio_range_to_wav(&audio, 0..3, &output_path).is_err());
        assert!(write_audio_range_to_wav_with_fades(&audio, 0..2, 3, 0, &output_path).is_err());
        assert!(
            write_audio_range_to_wav_with_processing(
                &audio,
                0..2,
                0,
                0,
                false,
                Some(1.0),
                &output_path,
            )
            .is_err()
        );
        assert!(!output_path.exists());
        fs::remove_dir_all(root).expect("fixture directory should be removed");
    }

    fn wav_with_sampler_loop_fixture() -> Vec<u8> {
        let mut sampler = vec![0u8; 60];
        sampler[12..16].copy_from_slice(&60u32.to_le_bytes());
        sampler[28..32].copy_from_slice(&1u32.to_le_bytes());
        sampler[36 + 8..36 + 12].copy_from_slice(&2u32.to_le_bytes());
        sampler[36 + 12..36 + 16].copy_from_slice(&4u32.to_le_bytes());

        let mut body = b"WAVE".to_vec();
        body.extend_from_slice(b"smpl");
        body.extend_from_slice(&(sampler.len() as u32).to_le_bytes());
        body.extend_from_slice(&sampler);
        body.extend_from_slice(b"data");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(&[0; 10]);

        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn wave_with_sampler_chunks(chunks: &[(&str, &[u8])]) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        for (id, payload) in chunks {
            assert_eq!(id.len(), 4);
            body.extend_from_slice(id.as_bytes());
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 != 0 {
                body.push(0);
            }
        }

        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn aiff_with_chunks(form_type: &[u8; 4], chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = form_type.to_vec();
        for (id, payload) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 != 0 {
                body.push(0);
            }
        }

        let mut bytes = b"FORM".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn aiff_marker_chunk(markers: &[(i16, u32, &str)]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(markers.len() as u16).to_be_bytes());
        for (marker_id, frame, name) in markers {
            let name = name.as_bytes();
            assert!(name.len() <= usize::from(u8::MAX));
            payload.extend_from_slice(&marker_id.to_be_bytes());
            payload.extend_from_slice(&frame.to_be_bytes());
            payload.push(name.len() as u8);
            payload.extend_from_slice(name);
            if (7 + name.len()) % 2 != 0 {
                payload.push(0);
            }
        }
        payload
    }

    #[test]
    fn waveform_summary_keeps_stereo_channels_separate_and_combined() {
        let audio = DecodedAudio {
            sample_rate: 44_100,
            channels: vec![vec![0.25, -0.25], vec![0.75, -0.75]],
        };

        let waveform = summarize_audio_waveform(&audio, 1).unwrap();

        assert_eq!(
            waveform.peaks,
            vec![WaveformPeak {
                minimum: -0.75,
                maximum: 0.75
            }]
        );
        assert_eq!(
            waveform.channel_peaks,
            vec![
                vec![WaveformPeak {
                    minimum: -0.25,
                    maximum: 0.25
                }],
                vec![WaveformPeak {
                    minimum: -0.75,
                    maximum: 0.75
                }]
            ]
        );
    }

    #[test]
    fn waveform_spectrum_places_high_tones_above_low_tones() {
        let make_tone = |frequency: f64| DecodedAudio {
            sample_rate: 22_050,
            channels: vec![
                (0..4_096)
                    .map(|frame| {
                        (2.0 * std::f64::consts::PI * frequency * frame as f64 / 22_050.0).sin()
                            as f32
                            * 0.5
                    })
                    .collect(),
            ],
        };
        let strongest_band = |waveform: &AudioWaveform| {
            (0..WAVEFORM_SPECTRUM_BANDS)
                .max_by_key(|&band| {
                    waveform
                        .spectral_bands
                        .iter()
                        .map(|column| column[band])
                        .max()
                        .unwrap_or_default()
                })
                .unwrap()
        };

        let low = summarize_audio_waveform(&make_tone(440.0), 32).unwrap();
        let high = summarize_audio_waveform(&make_tone(3_520.0), 32).unwrap();

        assert!(
            strongest_band(&high) >= strongest_band(&low) + 8,
            "high-frequency band did not move upward: low={} high={}",
            strongest_band(&low),
            strongest_band(&high)
        );
    }

    fn private_ogg_wave_fixture() -> Vec<u8> {
        let ogg = b"OggS-test";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(46u32 + ogg.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&26u32.to_le_bytes());
        bytes.extend_from_slice(&0x674Fu16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&44_100u32.to_le_bytes());
        bytes.extend_from_slice(&14_000u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(ogg.len() as u32).to_le_bytes());
        bytes.extend_from_slice(ogg);
        bytes
    }

    #[test]
    fn resolves_factory_macro_from_an_explicit_installation_root() {
        let root = fixture_root();
        let asset = root.join("Data").join("Patches").join("voice.wav");
        std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
        std::fs::write(&asset, b"fixture").unwrap();
        let project = root.join("Projects").join("song.flp");
        let resolver = SamplePathResolver::new(&project).with_factory_root(&root);

        let resolved = resolver
            .resolve(r"%FLStudioFactoryData%\Data\Patches\voice.wav")
            .unwrap();
        assert_eq!(resolved, asset);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_empty_project_template_in_installation_and_data_roots() {
        let root = fixture_root();
        let installation_template = root.join("Data/Templates/Empty/Empty.flp");
        std::fs::create_dir_all(installation_template.parent().unwrap()).unwrap();
        std::fs::write(&installation_template, b"template").unwrap();
        assert_eq!(
            find_empty_project_template_in_roots(std::slice::from_ref(&root)),
            Some(installation_template.clone())
        );

        let data_root = root.join("Data");
        let data_template = data_root.join("Templates/Empty/Empty.flp");
        std::fs::create_dir_all(data_template.parent().unwrap()).unwrap();
        std::fs::write(&data_template, b"data template").unwrap();
        assert_eq!(
            find_empty_project_template_in_roots(std::slice::from_ref(&data_root)),
            Some(data_template)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_project_relative_sample_paths() {
        let root = fixture_root();
        let project_directory = root.join("Projects");
        std::fs::create_dir_all(&project_directory).unwrap();
        let asset = project_directory.join("audio").join("voice.wav");
        std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
        std::fs::write(&asset, b"fixture").unwrap();
        let resolver = SamplePathResolver::new(project_directory.join("song.flp"));

        let resolved = resolver.resolve(r"audio\voice.wav").unwrap();
        assert_eq!(resolved, asset);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn decodes_pcm_wav_to_deinterleaved_float_samples() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("voice.wav");
        std::fs::write(&path, pcm16_wav_fixture()).unwrap();

        let decoded = decode_audio_file(&path).unwrap();
        assert_eq!(decoded.sample_rate, 44_100);
        assert_eq!(decoded.channels.len(), 1);
        assert_eq!(decoded.frame_count(), 2);
        assert_eq!(decoded.channels[0][0], -1.0);
        assert!((decoded.channels[0][1] - 0.999_969_5).abs() < 1e-6);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_first_wave_sampler_loop_as_half_open_frame_bounds() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("loop.wav");
        std::fs::write(&path, wav_with_sampler_loop_fixture()).unwrap();

        assert_eq!(
            wav_sampler_metadata(&path).and_then(|metadata| metadata.loop_points),
            Some((2, 5))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_missing_or_malformed_wave_sampler_loops() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("plain.wav");
        std::fs::write(&path, pcm16_wav_fixture()).unwrap();
        assert_eq!(
            wav_sampler_metadata(&path).and_then(|metadata| metadata.loop_points),
            None
        );

        let mut malformed = wav_with_sampler_loop_fixture();
        malformed[48..52].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&path, malformed).unwrap();
        assert_eq!(
            wav_sampler_metadata(&path).and_then(|metadata| metadata.loop_points),
            None
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_wav_root_note_from_inst_before_smpl_in_either_chunk_order() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("root-note.wav");
        let instrument = [
            72,
            (-23i8).to_le_bytes()[0],
            (-6i8).to_le_bytes()[0],
            0,
            127,
            1,
            127,
        ];
        let mut sampler = [0u8; 36];
        sampler[12..16].copy_from_slice(&60u32.to_le_bytes());
        sampler[16..20].copy_from_slice(&0x8000_0000u32.to_le_bytes());

        for chunks in [
            vec![
                ("inst", instrument.as_slice()),
                ("smpl", sampler.as_slice()),
            ],
            vec![
                ("smpl", sampler.as_slice()),
                ("inst", instrument.as_slice()),
            ],
        ] {
            std::fs::write(&path, wave_with_sampler_chunks(&chunks)).unwrap();
            let metadata = wav_sampler_metadata(&path).expect("WAVE root metadata should parse");
            assert_eq!(metadata.root_key, Some(72));
            assert_eq!(metadata.fine_tune_cents, Some(-23.0));
            assert_eq!(metadata.gain_db, Some(-6));
            assert_eq!(metadata.key_range, Some((0, 127)));
            assert_eq!(metadata.velocity_range, Some((1, 127)));
            assert_eq!(metadata.loop_points, None);
        }

        let invalid_instrument = [128, 0, 0, 0, 127, 0, 127];
        std::fs::write(
            &path,
            wave_with_sampler_chunks(&[
                ("inst", invalid_instrument.as_slice()),
                ("smpl", sampler.as_slice()),
            ]),
        )
        .unwrap();
        let metadata = wav_sampler_metadata(&path).expect("valid smpl root should parse");
        assert_eq!(metadata.root_key, Some(60));
        assert_eq!(metadata.fine_tune_cents, Some(50.0));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_invalid_wave_and_aiff_sampler_ranges_without_dropping_valid_metadata() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("invalid-ranges.wav");
        let wave_instrument = [60, 0, 0, 100, 50, 1, 127];
        std::fs::write(
            &path,
            wave_with_sampler_chunks(&[("inst", &wave_instrument)]),
        )
        .unwrap();
        let wave_metadata = wav_sampler_metadata(&path).expect("WAVE root metadata remains valid");
        assert_eq!(wave_metadata.root_key, Some(60));
        assert_eq!(wave_metadata.key_range, None);
        assert_eq!(wave_metadata.velocity_range, Some((1, 127)));

        let mut aiff_instrument = [0u8; 20];
        aiff_instrument[0] = 60;
        aiff_instrument[2] = 0;
        aiff_instrument[3] = 127;
        aiff_instrument[4] = 0;
        aiff_instrument[5] = 127;
        let path = root.join("invalid-ranges.aiff");
        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFF", &[(b"INST", &aiff_instrument)]),
        )
        .unwrap();
        let aiff_metadata = aiff_sampler_metadata(&path).expect("AIFF root metadata remains valid");
        assert_eq!(aiff_metadata.root_key, Some(60));
        assert_eq!(aiff_metadata.key_range, Some((0, 127)));
        assert_eq!(aiff_metadata.velocity_range, None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_aiff_instrument_root_detune_and_gain_from_aiff_and_aifc() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("instrument.aiff");
        let mut instrument = [0u8; 20];
        instrument[0] = 72;
        instrument[1] = (-23i8).to_be_bytes()[0];
        instrument[2] = 24;
        instrument[3] = 96;
        instrument[4] = 1;
        instrument[5] = 127;
        instrument[6..8].copy_from_slice(&(-6i16).to_be_bytes());

        for form_type in [b"AIFF", b"AIFC"] {
            let bytes =
                aiff_with_chunks(form_type, &[(b"JUNK", &[1, 2, 3]), (b"INST", &instrument)]);
            std::fs::write(&path, bytes).unwrap();

            let metadata = sample_sampler_metadata(&path).expect("AIFF sampler metadata parses");
            assert_eq!(metadata.root_key, Some(72));
            assert_eq!(metadata.fine_tune_cents, Some(-23.0));
            assert_eq!(metadata.gain_db, Some(-6));
            assert_eq!(metadata.key_range, Some((24, 96)));
            assert_eq!(metadata.velocity_range, Some((1, 127)));
            assert_eq!(metadata.loop_points, None);
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_zero_aiff_root_note_as_fl_studio_does() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("zero-root.aiff");
        let mut instrument = [0u8; 20];
        instrument[1] = 12;
        instrument[2] = 0;
        instrument[3] = 127;
        instrument[4] = 1;
        instrument[5] = 127;
        std::fs::write(&path, aiff_with_chunks(b"AIFF", &[(b"INST", &instrument)])).unwrap();

        let metadata = aiff_sampler_metadata(&path).expect("other AIFF metadata remains usable");
        assert_eq!(metadata.root_key, None);
        assert_eq!(metadata.fine_tune_cents, Some(12.0));
        assert_eq!(metadata.key_range, Some((0, 127)));
        assert_eq!(metadata.velocity_range, Some((1, 127)));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_aiff_sustain_loop_markers_in_either_chunk_order() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sustain-loop.aiff");
        let markers = aiff_marker_chunk(&[(11, 20, "loop"), (12, 80, "end")]);
        let mut instrument = [0u8; 20];
        instrument[0] = 60;
        instrument[8..10].copy_from_slice(&1i16.to_be_bytes());
        instrument[10..12].copy_from_slice(&11i16.to_be_bytes());
        instrument[12..14].copy_from_slice(&12i16.to_be_bytes());

        for (chunks, expected_ping_pong) in [
            (
                vec![
                    (b"MARK", markers.as_slice()),
                    (b"INST", instrument.as_slice()),
                ],
                Some(false),
            ),
            (
                vec![
                    (b"INST", instrument.as_slice()),
                    (b"MARK", markers.as_slice()),
                ],
                Some(false),
            ),
        ] {
            let bytes = aiff_with_chunks(b"AIFF", &chunks);
            std::fs::write(&path, bytes).unwrap();
            let metadata = aiff_sampler_metadata(&path).expect("AIFF loop metadata parses");
            assert_eq!(metadata.loop_points, Some((20, 80)));
            assert_eq!(metadata.ping_pong_loop, expected_ping_pong);
        }

        instrument[8..10].copy_from_slice(&2i16.to_be_bytes());
        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFF", &[(b"INST", &instrument), (b"MARK", &markers)]),
        )
        .unwrap();
        let metadata = aiff_sampler_metadata(&path).expect("AIFF ping-pong loop parses");
        assert_eq!(metadata.loop_points, Some((20, 80)));
        assert_eq!(metadata.ping_pong_loop, Some(true));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_aiff_release_loop_markers_and_play_modes() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("release-loop.aiff");
        let markers = aiff_marker_chunk(&[
            (11, 20, "sustain-start"),
            (12, 80, "sustain-end"),
            (21, 100, "release-start"),
            (22, 160, "release-end"),
        ]);
        let mut instrument = [0u8; 20];
        instrument[0] = 60;
        instrument[14..16].copy_from_slice(&1i16.to_be_bytes());
        instrument[16..18].copy_from_slice(&21i16.to_be_bytes());
        instrument[18..20].copy_from_slice(&22i16.to_be_bytes());

        for chunks in [
            vec![
                (b"MARK", markers.as_slice()),
                (b"INST", instrument.as_slice()),
            ],
            vec![
                (b"INST", instrument.as_slice()),
                (b"MARK", markers.as_slice()),
            ],
        ] {
            std::fs::write(&path, aiff_with_chunks(b"AIFF", &chunks)).unwrap();
            let metadata = aiff_sampler_metadata(&path).expect("release loop metadata parses");
            assert_eq!(metadata.loop_points, None);
            assert_eq!(metadata.release_loop_points, Some((100, 160)));
            assert_eq!(metadata.release_ping_pong_loop, Some(false));
        }

        instrument[14..16].copy_from_slice(&2i16.to_be_bytes());
        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFC", &[(b"INST", &instrument), (b"MARK", &markers)]),
        )
        .unwrap();
        let metadata = aiff_sampler_metadata(&path).expect("ping-pong release loop parses");
        assert_eq!(metadata.release_loop_points, Some((100, 160)));
        assert_eq!(metadata.release_ping_pong_loop, Some(true));

        instrument[16..18].copy_from_slice(&99i16.to_be_bytes());
        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFF", &[(b"INST", &instrument), (b"MARK", &markers)]),
        )
        .unwrap();
        let metadata = aiff_sampler_metadata(&path).expect("root metadata remains usable");
        assert_eq!(metadata.root_key, Some(60));
        assert_eq!(metadata.release_loop_points, None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_invalid_aiff_sustain_loop_modes_and_marker_references() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("invalid-loop.aiff");
        let markers = aiff_marker_chunk(&[(11, 80, "end"), (12, 20, "begin")]);
        let mut instrument = [0u8; 20];
        instrument[0] = 60;
        instrument[8..10].copy_from_slice(&1i16.to_be_bytes());
        instrument[10..12].copy_from_slice(&11i16.to_be_bytes());
        instrument[12..14].copy_from_slice(&12i16.to_be_bytes());
        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFF", &[(b"MARK", &markers), (b"INST", &instrument)]),
        )
        .unwrap();
        let reversed = aiff_sampler_metadata(&path).expect("root metadata remains usable");
        assert_eq!(reversed.root_key, Some(60));
        assert_eq!(reversed.loop_points, None);
        assert_eq!(reversed.ping_pong_loop, None);

        instrument[8..10].copy_from_slice(&3i16.to_be_bytes());
        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFF", &[(b"INST", &instrument), (b"MARK", &markers)]),
        )
        .unwrap();
        let unsupported = aiff_sampler_metadata(&path).expect("root metadata remains usable");
        assert_eq!(unsupported.loop_points, None);

        let duplicate_markers =
            aiff_marker_chunk(&[(11, 20, "one"), (11, 80, "two"), (12, 90, "end")]);
        instrument[8..10].copy_from_slice(&1i16.to_be_bytes());
        instrument[10..12].copy_from_slice(&11i16.to_be_bytes());
        instrument[12..14].copy_from_slice(&12i16.to_be_bytes());
        std::fs::write(
            &path,
            aiff_with_chunks(
                b"AIFF",
                &[(b"INST", &instrument), (b"MARK", &duplicate_markers)],
            ),
        )
        .unwrap();
        let duplicate = aiff_sampler_metadata(&path).expect("root metadata remains usable");
        assert_eq!(duplicate.loop_points, None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_or_truncated_aiff_instrument_chunks() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("invalid-instrument.aiff");
        let mut instrument = [0u8; 20];
        instrument[0] = 128;
        instrument[1] = 51;
        instrument[6..8].copy_from_slice(&3i16.to_be_bytes());
        std::fs::write(&path, aiff_with_chunks(b"AIFF", &[(b"INST", &instrument)])).unwrap();
        let metadata = aiff_sampler_metadata(&path).expect("AIFF gain remains usable");
        assert_eq!(metadata.root_key, None);
        assert_eq!(metadata.fine_tune_cents, None);
        assert_eq!(metadata.gain_db, Some(3));

        std::fs::write(
            &path,
            aiff_with_chunks(b"AIFF", &[(b"INST", &instrument[..19])]),
        )
        .unwrap();
        assert_eq!(aiff_sampler_metadata(&path), None);

        let mut truncated = aiff_with_chunks(b"AIFF", &[(b"INST", &instrument)]);
        truncated[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(&path, truncated).unwrap();
        assert_eq!(aiff_sampler_metadata(&path), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_wav_smpl_pitch_fraction_as_cents_when_inst_root_is_invalid() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("smpl-pitch-fraction.wav");
        let invalid_instrument = [128, 17, 0, 0, 127, 0, 127];
        let mut sampler = [0u8; 36];
        sampler[12..16].copy_from_slice(&60u32.to_le_bytes());
        sampler[16..20].copy_from_slice(&0x4000_0000u32.to_le_bytes());
        std::fs::write(
            &path,
            wave_with_sampler_chunks(&[
                ("inst", invalid_instrument.as_slice()),
                ("smpl", sampler.as_slice()),
            ]),
        )
        .unwrap();

        let metadata = wav_sampler_metadata(&path).expect("valid smpl root should parse");
        assert_eq!(metadata.root_key, Some(60));
        assert_eq!(metadata.fine_tune_cents, Some(25.0));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_wav_inst_gain_even_without_a_valid_root_note() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("inst-gain.wav");
        let instrument = [128, 0, (-6i8).to_le_bytes()[0], 0, 127, 0, 127];
        std::fs::write(&path, wave_with_sampler_chunks(&[("inst", &instrument)])).unwrap();

        let metadata = wav_sampler_metadata(&path).expect("WAVE instrument gain should parse");
        assert_eq!(metadata.root_key, None);
        assert_eq!(metadata.gain_db, Some(-6));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_truncated_wav_root_metadata() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("truncated-root.wav");
        std::fs::write(&path, wave_with_sampler_chunks(&[("inst", &[72, 0, 0])])).unwrap();

        assert_eq!(wav_sampler_metadata(&path), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_ogg_stream_embedded_in_fl_studio_wave_wrapper() {
        let root = fixture_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("wrapped.wav");
        std::fs::write(&path, private_ogg_wave_fixture()).unwrap();

        let mut file = File::open(&path).unwrap();
        let offset = find_embedded_ogg_offset(&mut file).unwrap().unwrap();
        assert_eq!(offset, 54);
        let mut source = OffsetMediaSource::new(file, offset).unwrap();
        assert_eq!(source.byte_len(), Some(9));
        assert_eq!(source.seek(SeekFrom::End(-9)).unwrap(), 0);
        let mut signature = [0; 4];
        source.read_exact(&mut signature).unwrap();
        assert_eq!(&signature, b"OggS");

        std::fs::remove_dir_all(root).unwrap();
    }
}
