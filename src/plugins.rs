use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PluginFormat {
    Vst2Candidate,
    Vst3,
}

impl fmt::Display for PluginFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vst2Candidate => formatter.write_str("VST2 candidate (not validated)"),
            Self::Vst3 => formatter.write_str("VST3"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginCandidate {
    pub format: PluginFormat,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginScanReport {
    pub search_roots: Vec<PathBuf>,
    pub candidates: Vec<PluginCandidate>,
    pub scan_errors: Vec<(PathBuf, String)>,
}

#[derive(Clone)]
struct SearchRoot {
    format: PluginFormat,
    path: PathBuf,
}

/// Finds VST bundles and DLL candidates in conventional platform plug-in folders.
/// `VST3_PATH` and `VST2_PATH` can add directories using platform path-list syntax.
/// VST2 DLLs are listed as candidates only; checking one requires loading executable code.
pub fn scan_installed_plugins() -> PluginScanReport {
    let roots = default_search_roots();
    let mut report = PluginScanReport {
        search_roots: roots.iter().map(|root| root.path.clone()).collect(),
        ..PluginScanReport::default()
    };
    let mut visited = HashSet::new();
    let mut found = HashSet::new();

    for root in roots {
        if !root.path.exists() {
            continue;
        }
        walk(
            &root.path,
            root.format,
            0,
            &mut report,
            &mut visited,
            &mut found,
        );
    }

    report.candidates.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    report
}

fn default_search_roots() -> Vec<SearchRoot> {
    let mut roots = Vec::new();
    for path in environment_paths("VST3_PATH") {
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path,
        });
    }
    for path in environment_paths("VST2_PATH") {
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path,
        });
    }

    #[cfg(target_os = "windows")]
    windows_search_roots(&mut roots);
    #[cfg(target_os = "macos")]
    macos_search_roots(&mut roots);
    #[cfg(target_os = "linux")]
    linux_search_roots(&mut roots);

    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert((root.format, normalized_path(&root.path))));
    roots
}

#[cfg(target_os = "windows")]
fn windows_search_roots(roots: &mut Vec<SearchRoot>) {
    for variable in ["CommonProgramW6432", "CommonProgramFiles"] {
        if let Some(common_files) = env::var_os(variable) {
            roots.push(SearchRoot {
                format: PluginFormat::Vst3,
                path: PathBuf::from(common_files).join("VST3"),
            });
        }
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(program_files) = env::var_os(variable) {
            let program_files = PathBuf::from(program_files);
            roots.push(SearchRoot {
                format: PluginFormat::Vst3,
                path: program_files.join("Common Files").join("VST3"),
            });
            roots.push(SearchRoot {
                format: PluginFormat::Vst2Candidate,
                path: program_files.join("VstPlugins"),
            });
            roots.push(SearchRoot {
                format: PluginFormat::Vst2Candidate,
                path: program_files.join("Steinberg").join("VstPlugins"),
            });
        }
    }
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let local_app_data = PathBuf::from(local_app_data);
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: local_app_data.join("Programs").join("Common").join("VST3"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path: local_app_data.join("Programs").join("VstPlugins"),
        });
    }
}

#[cfg(target_os = "macos")]
fn macos_search_roots(roots: &mut Vec<SearchRoot>) {
    let bases = [
        Some(PathBuf::from("/Library/Audio/Plug-Ins")),
        home_dir().map(|home| home.join("Library/Audio/Plug-Ins")),
    ];
    for base in bases.into_iter().flatten() {
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: base.join("VST3"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path: base.join("VST"),
        });
    }
}

#[cfg(target_os = "linux")]
fn linux_search_roots(roots: &mut Vec<SearchRoot>) {
    for path in ["/usr/lib/vst3", "/usr/local/lib/vst3", "/usr/lib64/vst3"] {
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: PathBuf::from(path),
        });
    }
    for path in ["/usr/lib/vst", "/usr/local/lib/vst", "/usr/lib64/vst"] {
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path: PathBuf::from(path),
        });
    }
    if let Some(home) = home_dir() {
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: home.join(".vst3"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path: home.join(".vst"),
        });
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn environment_paths(variable: &str) -> Vec<PathBuf> {
    env::var_os(variable)
        .map(|paths| env::split_paths(&paths).collect())
        .unwrap_or_default()
}

fn walk(
    directory: &Path,
    format: PluginFormat,
    depth: usize,
    report: &mut PluginScanReport,
    visited: &mut HashSet<String>,
    found: &mut HashSet<String>,
) {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        report.scan_errors.push((
            directory.to_path_buf(),
            format!("directory depth exceeded {MAX_DEPTH}"),
        ));
        return;
    }

    let directory_key = format!("{format:?}:{}", normalized_path(directory));
    if !visited.insert(directory_key) {
        return;
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            report
                .scan_errors
                .push((directory.to_path_buf(), error.to_string()));
            return;
        }
    };

    let mut children = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => children.push(entry.path()),
            Err(error) => report
                .scan_errors
                .push((directory.to_path_buf(), error.to_string())),
        }
    }
    children.sort();

    for path in children {
        let extension = path.extension().unwrap_or_else(|| OsStr::new(""));
        if format == PluginFormat::Vst3 && extension.eq_ignore_ascii_case("vst3") {
            add_candidate(&path, format, report, found);
            continue;
        }
        if format == PluginFormat::Vst2Candidate && extension.eq_ignore_ascii_case("dll") {
            add_candidate(&path, format, report, found);
            continue;
        }
        if path.is_dir() {
            walk(&path, format, depth + 1, report, visited, found);
        }
    }
}

fn add_candidate(
    path: &Path,
    format: PluginFormat,
    report: &mut PluginScanReport,
    found: &mut HashSet<String>,
) {
    let key = normalized_path(path);
    if !found.insert(key) {
        return;
    }
    let name = path
        .file_stem()
        .or_else(|| path.file_name())
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    report.candidates.push(PluginCandidate {
        format,
        name,
        path: path.to_path_buf(),
    });
}

fn normalized_path(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(target_os = "windows") {
        value.to_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginFormat, PluginScanReport, walk};
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("flp-plugin-scan-{nonce}"));
            fs::create_dir_all(&path).expect("temporary directory should be created");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scan_finds_nested_vst3_bundles_and_dll_candidates() {
        let temp = TestDirectory::new();
        let vst3 = temp.0.join("Vendor").join("Example.vst3");
        let dll = temp.0.join("legacy").join("Example.dll");
        fs::create_dir_all(vst3.join("Contents")).expect("bundle directory should be created");
        fs::create_dir_all(dll.parent().expect("DLL has parent"))
            .expect("legacy plug-in directory should be created");
        fs::write(vst3.join("Contents").join("module.bin"), b"bundle")
            .expect("bundle content should be written");
        fs::write(&dll, b"candidate").expect("DLL candidate should be written");

        let mut report = PluginScanReport::default();
        let mut visited = HashSet::new();
        let mut found = HashSet::new();
        walk(
            &temp.0,
            PluginFormat::Vst3,
            0,
            &mut report,
            &mut visited,
            &mut found,
        );
        walk(
            &temp.0,
            PluginFormat::Vst2Candidate,
            0,
            &mut report,
            &mut visited,
            &mut found,
        );

        assert_eq!(report.candidates.len(), 2);
        assert!(
            report.candidates.iter().any(|candidate| {
                candidate.format == PluginFormat::Vst3 && candidate.path == vst3
            })
        );
        assert!(report.candidates.iter().any(|candidate| {
            candidate.format == PluginFormat::Vst2Candidate && candidate.path == dll
        }));
    }
}
