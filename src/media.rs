//! Project media path resolution.

use std::path::{Path, PathBuf};

use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
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
    let path = path.as_ref();
    let file = std::fs::File::open(path)
        .map_err(|error| format!("could not open sample {}: {error}", path.display()))?;
    let source = MediaSourceStream::new(Box::new(file), Default::default());
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
        let sample_count = frame_count
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
                .try_reserve(frame_count)
                .map_err(|error| format!("could not allocate sample buffers: {error}"))?;
        }

        let mut interleaved = vec![f32::MID; decoded.samples_interleaved()];
        decoded.copy_to_slice_interleaved(&mut interleaved);
        for frame in interleaved[..sample_count].chunks_exact(channel_count) {
            for (channel, sample) in output_channels.iter_mut().zip(frame) {
                channel.push(*sample);
            }
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
}
