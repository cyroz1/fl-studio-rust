//! Project media path resolution.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

const FACTORY_DATA_MACRO: &str = "%FLStudioFactoryData%";
const MAX_DECODED_SAMPLE_BYTES: usize = 512 * 1024 * 1024;

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

/// Decode common FL Studio sample formats by inspecting file content rather than its extension.
///
/// FLP projects can contain compressed audio whose filename extension does not match its
/// container. The decoded audio is bounded to 512 MiB of `f32` sample data; longer media needs
/// streaming playback instead of this in-memory helper.
pub fn decode_audio_file(path: impl AsRef<Path>) -> Result<DecodedAudio, String> {
    decode_audio_file_for_preview(path.as_ref(), None)
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

fn decoded_audio_to_stereo_samples(
    audio: &DecodedAudio,
    output_sample_rate: u32,
    maximum_seconds: Option<f64>,
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
    let duration = source_frames as f64 / f64::from(audio.sample_rate);
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
        let position = (frame as f64 * source_step).min((source_frames - 1) as f64);
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

fn summarize_audio_waveform(
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
    Ok(AudioWaveform {
        sample_rate: audio.sample_rate,
        frame_count: frame_count as u64,
        peaks,
        channel_peaks,
    })
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
