//! Project media path resolution.

use std::path::{Path, PathBuf};

const FACTORY_DATA_MACRO: &str = "%FLStudioFactoryData%";

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
}
