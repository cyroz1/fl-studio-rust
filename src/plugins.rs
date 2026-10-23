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
    Clap,
    AudioUnitCandidate,
}

impl fmt::Display for PluginFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vst2Candidate => formatter.write_str("VST2 candidate (not validated)"),
            Self::Vst3 => formatter.write_str("VST3"),
            Self::Clap => formatter.write_str("CLAP"),
            Self::AudioUnitCandidate => formatter.write_str("Audio Unit candidate (not hosted)"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginCandidate {
    pub format: PluginFormat,
    pub name: String,
    pub path: PathBuf,
    /// VST3 processor class IDs declared by a bundle's validated moduleinfo.json,
    /// including IDs retired by its compatibility mappings.
    pub class_ids: Vec<String>,
    /// Audio Unit component identities parsed from a bundle's `Info.plist`.
    pub audio_unit_components: Vec<AudioUnitComponentInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioUnitComponentInfo {
    pub name: String,
    pub type_code: String,
    pub subtype_code: String,
    pub manufacturer_code: String,
    /// Whether macOS reports this exact identity in its Audio Component catalog.
    /// `None` means the current platform cannot query the catalog or an ID is malformed.
    pub registered: Option<bool>,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_subtype: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[cfg(target_os = "macos")]
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentCount(description: *const AudioComponentDescription) -> u32;
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

/// Finds VST, CLAP, and macOS Audio Unit bundle candidates in conventional
/// platform folders. `VST3_PATH`, `VST2_PATH`, and `CLAP_PATH` can add
/// directories using platform path-list syntax. VST2 DLLs, CLAP bundles, and
/// Audio Unit bundles are listed as candidates only; checking them requires
/// loading executable code or consulting the platform's plug-in registry.
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
    for path in environment_paths("CLAP_PATH") {
        roots.push(SearchRoot {
            format: PluginFormat::Clap,
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
                format: PluginFormat::Clap,
                path: program_files.join("Common Files").join("CLAP"),
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
            format: PluginFormat::Clap,
            path: local_app_data.join("Programs").join("Common").join("CLAP"),
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
            format: PluginFormat::Clap,
            path: base.join("CLAP"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::Vst2Candidate,
            path: base.join("VST"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::AudioUnitCandidate,
            path: base.join("Components"),
        });
    }
    roots.push(SearchRoot {
        format: PluginFormat::AudioUnitCandidate,
        path: PathBuf::from("/System/Library/Components"),
    });
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
    for path in ["/usr/lib/clap", "/usr/local/lib/clap", "/usr/lib64/clap"] {
        roots.push(SearchRoot {
            format: PluginFormat::Clap,
            path: PathBuf::from(path),
        });
    }
    if let Some(home) = home_dir() {
        roots.push(SearchRoot {
            format: PluginFormat::Vst3,
            path: home.join(".vst3"),
        });
        roots.push(SearchRoot {
            format: PluginFormat::Clap,
            path: home.join(".clap"),
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
        if format == PluginFormat::Clap && extension.eq_ignore_ascii_case("clap") {
            add_candidate(&path, format, report, found);
            continue;
        }
        if format == PluginFormat::AudioUnitCandidate
            && path.is_dir()
            && (extension.eq_ignore_ascii_case("component")
                || extension.eq_ignore_ascii_case("audiocomp"))
        {
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
    let mut class_ids = Vec::new();
    let mut audio_unit_components = Vec::new();
    if format == PluginFormat::Vst3 {
        match vst3_host::discovery::read_module_info(path) {
            Ok(Some(module_info)) => {
                for class in module_info
                    .classes
                    .iter()
                    .filter(|class| class.category.contains("Audio Module Class"))
                {
                    push_unique_class_id(&mut class_ids, &class.class_id);
                }
                for compatibility in module_info.compatibility {
                    push_unique_class_id(&mut class_ids, &compatibility.new_class_id);
                    for old_class_id in compatibility.old_class_ids {
                        push_unique_class_id(&mut class_ids, &old_class_id);
                    }
                }
            }
            Ok(None) => {}
            Err(error) => report
                .scan_errors
                .push((path.to_path_buf(), error.to_string())),
        }
    } else if format == PluginFormat::AudioUnitCandidate {
        match read_audio_unit_components(path) {
            Ok(components) => {
                audio_unit_components = components
                    .into_iter()
                    .map(|mut component| {
                        component.registered = audio_unit_component_registered(&component);
                        component
                    })
                    .collect();
            }
            Err(error) => report.scan_errors.push((path.to_path_buf(), error)),
        }
    }
    report.candidates.push(PluginCandidate {
        format,
        name,
        path: path.to_path_buf(),
        class_ids,
        audio_unit_components,
    });
}

fn read_audio_unit_components(bundle_path: &Path) -> Result<Vec<AudioUnitComponentInfo>, String> {
    const MAX_INFO_PLIST_BYTES: u64 = 4 * 1024 * 1024;
    let contents_info = bundle_path.join("Contents").join("Info.plist");
    let info_path = if contents_info.is_file() {
        contents_info
    } else {
        bundle_path.join("Info.plist")
    };
    let metadata = fs::metadata(&info_path)
        .map_err(|error| format!("could not read Audio Unit Info.plist metadata: {error}"))?;
    if metadata.len() > MAX_INFO_PLIST_BYTES {
        return Err(format!(
            "Audio Unit Info.plist is larger than {MAX_INFO_PLIST_BYTES} bytes"
        ));
    }
    let plist = plist::Value::from_file(&info_path)
        .map_err(|error| format!("could not parse Audio Unit Info.plist: {error}"))?;
    let dictionary = plist
        .as_dictionary()
        .ok_or_else(|| "Audio Unit Info.plist root is not a dictionary".to_owned())?;
    let entries = dictionary
        .get("AudioComponents")
        .and_then(plist::Value::as_array)
        .ok_or_else(|| "Audio Unit Info.plist has no AudioComponents array".to_owned())?;
    let mut components = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let entry = entry
            .as_dictionary()
            .ok_or_else(|| format!("AudioComponents entry {index} is not a dictionary"))?;
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(plist::Value::as_string)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("AudioComponents entry {index} has no string {key:?} field"))
        };
        components.push(AudioUnitComponentInfo {
            name: field("name")?,
            type_code: field("type")?,
            subtype_code: field("subtype")?,
            manufacturer_code: field("manufacturer")?,
            registered: None,
        });
    }
    if components.is_empty() {
        return Err("Audio Unit Info.plist has an empty AudioComponents array".to_owned());
    }
    Ok(components)
}

fn audio_unit_component_registered(component: &AudioUnitComponentInfo) -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        let description = AudioComponentDescription {
            component_type: audio_unit_fourcc(&component.type_code)?,
            component_subtype: audio_unit_fourcc(&component.subtype_code)?,
            component_manufacturer: audio_unit_fourcc(&component.manufacturer_code)?,
            component_flags: 0,
            component_flags_mask: 0,
        };
        // SAFETY: AudioComponentCount reads the fully initialized C-compatible
        // description and does not retain the pointer.
        Some(unsafe { AudioComponentCount(&description) > 0 })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = component;
        None
    }
}

#[cfg(any(target_os = "macos", test))]
fn audio_unit_fourcc(code: &str) -> Option<u32> {
    let bytes: [u8; 4] = code.as_bytes().try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn push_unique_class_id(class_ids: &mut Vec<String>, class_id: &str) {
    if !class_ids
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(class_id))
    {
        class_ids.push(class_id.to_owned());
    }
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);
            loop {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock should be after epoch")
                    .as_nanos();
                let directory_id = NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "flp-plugin-scan-{timestamp}-{}-{directory_id}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("temporary directory should be created: {error}"),
                }
            }
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
        let clap_bundle = temp.0.join("Vendor").join("Example.clap");
        let clap_library = temp.0.join("portable").join("Example.clap");
        fs::create_dir_all(vst3.join("Contents")).expect("bundle directory should be created");
        fs::create_dir_all(clap_bundle.join("Contents/MacOS"))
            .expect("CLAP bundle directory should be created");
        fs::create_dir_all(dll.parent().expect("DLL has parent"))
            .expect("legacy plug-in directory should be created");
        fs::create_dir_all(clap_library.parent().expect("CLAP has parent"))
            .expect("portable plug-in directory should be created");
        fs::write(vst3.join("Contents").join("module.bin"), b"bundle")
            .expect("bundle content should be written");
        fs::write(clap_bundle.join("Contents/MacOS/Example"), b"bundle")
            .expect("CLAP bundle content should be written");
        fs::write(&dll, b"candidate").expect("DLL candidate should be written");
        fs::write(&clap_library, b"candidate").expect("CLAP candidate should be written");

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
        walk(
            &temp.0,
            PluginFormat::Clap,
            0,
            &mut report,
            &mut visited,
            &mut found,
        );

        assert_eq!(report.candidates.len(), 4);
        assert!(
            report.candidates.iter().any(|candidate| {
                candidate.format == PluginFormat::Vst3 && candidate.path == vst3
            })
        );
        assert!(report.candidates.iter().any(|candidate| {
            candidate.format == PluginFormat::Vst2Candidate && candidate.path == dll
        }));
        assert!(report.candidates.iter().any(|candidate| {
            candidate.format == PluginFormat::Clap && candidate.path == clap_bundle
        }));
        assert!(report.candidates.iter().any(|candidate| {
            candidate.format == PluginFormat::Clap && candidate.path == clap_library
        }));
        assert!(report.scan_errors.is_empty());
    }

    #[test]
    fn scan_finds_audio_unit_bundle_candidates_by_bundle_extension() {
        let temp = TestDirectory::new();
        let component = temp.0.join("Vendor").join("Echo.component");
        let audio_component = temp.0.join("Vendor").join("Codec.audiocomp");
        for (bundle, name, type_code, subtype_code, manufacturer_code) in [
            (&component, "Echo", "aufx", "ECHO", "ACME"),
            (&audio_component, "Codec", "aufc", "CODE", "ACME"),
        ] {
            let info_path = bundle.join("Contents").join("Info.plist");
            fs::create_dir_all(info_path.parent().expect("Info.plist should have a parent"))
                .expect("Audio Unit Contents directory should be created");
            if name == "Codec" {
                let mut component_info = plist::Dictionary::new();
                component_info.insert("name".to_owned(), plist::Value::String(name.to_owned()));
                component_info.insert(
                    "type".to_owned(),
                    plist::Value::String(type_code.to_owned()),
                );
                component_info.insert(
                    "subtype".to_owned(),
                    plist::Value::String(subtype_code.to_owned()),
                );
                component_info.insert(
                    "manufacturer".to_owned(),
                    plist::Value::String(manufacturer_code.to_owned()),
                );
                let mut root = plist::Dictionary::new();
                root.insert(
                    "AudioComponents".to_owned(),
                    plist::Value::Array(vec![plist::Value::Dictionary(component_info)]),
                );
                plist::Value::Dictionary(root)
                    .to_file_binary(&info_path)
                    .expect("binary Audio Unit metadata should be written");
            } else {
                fs::write(
                    &info_path,
                    format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>AudioComponents</key><array><dict>
<key>name</key><string>{name}</string>
<key>type</key><string>{type_code}</string>
<key>subtype</key><string>{subtype_code}</string>
<key>manufacturer</key><string>{manufacturer_code}</string>
</dict></array></dict></plist>"#
                    ),
                )
                .expect("Audio Unit metadata should be written");
            }
        }

        let mut report = PluginScanReport::default();
        let mut visited = HashSet::new();
        let mut found = HashSet::new();
        walk(
            &temp.0,
            PluginFormat::AudioUnitCandidate,
            0,
            &mut report,
            &mut visited,
            &mut found,
        );

        assert_eq!(report.candidates.len(), 2);
        assert!(report.candidates.iter().any(|candidate| {
            candidate.format == PluginFormat::AudioUnitCandidate
                && candidate.name == "Echo"
                && candidate.path == component
        }));
        assert!(report.candidates.iter().any(|candidate| {
            candidate.format == PluginFormat::AudioUnitCandidate
                && candidate.name == "Codec"
                && candidate.path == audio_component
        }));
        let echo = report
            .candidates
            .iter()
            .find(|candidate| candidate.path.ends_with("Echo.component"))
            .expect("Audio Unit bundle should be discovered");
        assert_eq!(echo.audio_unit_components[0].name, "Echo");
        assert_eq!(echo.audio_unit_components[0].type_code, "aufx");
        assert_eq!(echo.audio_unit_components[0].subtype_code, "ECHO");
        assert_eq!(echo.audio_unit_components[0].manufacturer_code, "ACME");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(echo.audio_unit_components[0].registered, None);
        let codec = report
            .candidates
            .iter()
            .find(|candidate| candidate.path.ends_with("Codec.audiocomp"))
            .expect("audio component bundle should be discovered");
        assert_eq!(codec.audio_unit_components[0].name, "Codec");
        assert_eq!(codec.audio_unit_components[0].type_code, "aufc");
        assert!(report.scan_errors.is_empty());
    }

    #[test]
    fn audio_unit_four_character_codes_use_big_endian_bytes() {
        assert_eq!(super::audio_unit_fourcc("aufx"), Some(0x6175_6678));
        assert_eq!(super::audio_unit_fourcc("ACME"), Some(0x4143_4d45));
        assert_eq!(super::audio_unit_fourcc("abc"), None);
        assert_eq!(super::audio_unit_fourcc("é"), None);
    }

    #[test]
    fn scan_keeps_audio_unit_bundle_when_component_metadata_is_missing() {
        let temp = TestDirectory::new();
        let component = temp.0.join("Vendor").join("Unknown.component");
        fs::create_dir_all(&component).expect("Audio Unit candidate bundle should be created");

        let mut report = PluginScanReport::default();
        let mut visited = HashSet::new();
        let mut found = HashSet::new();
        walk(
            &temp.0,
            PluginFormat::AudioUnitCandidate,
            0,
            &mut report,
            &mut visited,
            &mut found,
        );

        assert_eq!(report.candidates.len(), 1);
        assert!(report.candidates[0].audio_unit_components.is_empty());
        assert_eq!(report.scan_errors.len(), 1);
        assert!(report.scan_errors[0].1.contains("Info.plist"));
    }

    #[test]
    fn scan_reads_vst3_processor_and_retired_class_ids_without_loading_the_bundle() {
        const CURRENT_CLASS_ID: &str = "00112233445566778899AABBCCDDEEFF";
        const RETIRED_CLASS_ID: &str = "FFEEDDCCBBAA99887766554433221100";

        let temp = TestDirectory::new();
        let vst3 = temp.0.join("Vendor").join("Example.vst3");
        let module_info = vst3.join("Contents/Resources/moduleinfo.json");
        fs::create_dir_all(
            module_info
                .parent()
                .expect("moduleinfo file should have a parent"),
        )
        .expect("VST3 resources directory should be created");
        fs::write(
            &module_info,
            format!(
                r#"{{
                    "Name": "Example",
                    "Classes": [
                        {{
                            "CID": "{CURRENT_CLASS_ID}",
                            "Category": "Audio Module Class",
                            "Name": "Example Processor"
                        }},
                        {{
                            "CID": "1234567890ABCDEF1234567890ABCDEF",
                            "Category": "Component Controller Class",
                            "Name": "Example Controller"
                        }}
                    ],
                    "Compatibility": [
                        {{ "New": "{CURRENT_CLASS_ID}", "Old": ["{RETIRED_CLASS_ID}"] }}
                    ]
                }}"#
            ),
        )
        .expect("VST3 module metadata should be written");

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

        let candidate = report
            .candidates
            .iter()
            .find(|candidate| candidate.path == vst3)
            .expect("VST3 bundle should remain a candidate");
        assert_eq!(
            candidate.class_ids,
            vec![CURRENT_CLASS_ID.to_owned(), RETIRED_CLASS_ID.to_owned()]
        );
        assert!(report.scan_errors.is_empty());
    }
}
