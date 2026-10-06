use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

/// Finds VST bundles and DLL candidates in conventional Windows plug-in folders.
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
    if let Some(common_files) = env::var_os("CommonProgramFiles") {
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: PathBuf::from(common_files).join("VST3"),
        });
    }
    if let Some(program_files) = env::var_os("ProgramFiles") {
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
    if let Some(program_files_x86) = env::var_os("ProgramFiles(x86)") {
        let program_files_x86 = PathBuf::from(program_files_x86);
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: program_files_x86.join("Common Files").join("VST3"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path: program_files_x86.join("VstPlugins"),
        });
    }

    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(normalized_path(&root.path)));
    roots
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

    let directory_key = normalized_path(directory);
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
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}
