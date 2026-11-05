//! Edison audio script support: discovery of `.pyscript` files and the
//! run-last-script state.
//!
//! This follows the workflow documented in Image-Line's FL Studio manual
//! (Edison, Tools > Scripting): scripts are Python, saved as `.pyscript`
//! files in an "Audio scripts" folder, and listed by the Run script menu.
//! "Run last script" repeats the most recently run script. The sample
//! processing API surface scripts code against is the `enveditor` module
//! documented by Image-Line's FL Studio API stubs; the execution engine
//! lives in `edison_script_runner.py` next to this file.

use std::fs;
use std::path::{Path, PathBuf};

/// One discovered Edison script file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdisonScript {
    pub path: PathBuf,
    /// Display name: the file stem, as shown in the Run script menu.
    pub name: String,
    /// Subfolder below the scripts root, when the script lives in one.
    /// FL groups scripts from subfolders into submenus.
    pub group: Option<String>,
}

const MAX_DISCOVERY_DEPTH: usize = 8;
const MAX_DISCOVERED_SCRIPTS: usize = 512;

/// Recursively scan `root` for `.pyscript` files.
///
/// A missing root is not an error: it simply means no scripts are
/// installed, and the Run script menu is empty.
pub fn discover_edison_scripts(root: &Path) -> Result<Vec<EdisonScript>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut scripts = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| {
            format!(
                "could not read Edison scripts folder {}: {error}",
                directory.display()
            )
        })?;
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().ok();
            if file_type.is_some_and(|kind| kind.is_dir()) {
                if depth < MAX_DISCOVERY_DEPTH {
                    pending.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let is_script = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pyscript"));
            if !is_script {
                continue;
            }
            let name = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let group = path
                .parent()
                .and_then(|parent| parent.strip_prefix(root).ok())
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(|relative| relative.to_string_lossy().replace('\\', "/"));
            scripts.push(EdisonScript { path, name, group });
            if scripts.len() >= MAX_DISCOVERED_SCRIPTS {
                break;
            }
        }
        if scripts.len() >= MAX_DISCOVERED_SCRIPTS {
            break;
        }
    }
    scripts.sort_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(scripts)
}

/// Group discovered scripts for the Run script menu: ungrouped scripts
/// first, then one entry per subfolder, preserving discovery order.
pub fn grouped_edison_scripts(
    scripts: &[EdisonScript],
) -> Vec<(Option<String>, Vec<EdisonScript>)> {
    let mut groups: Vec<(Option<String>, Vec<EdisonScript>)> = Vec::new();
    for script in scripts {
        if let Some(existing) = groups
            .iter_mut()
            .find(|(group, _)| *group == script.group)
        {
            existing.1.push(script.clone());
        } else {
            groups.push((script.group.clone(), vec![script.clone()]));
        }
    }
    groups
}

/// The folders scanned for Edison scripts, in priority order.
///
/// The first mirrors this app's own settings layout (the same root that
/// holds `Settings/Hardware` for MIDI controller scripts). The second is
/// the FL Studio user data folder documented in the manual:
/// `Documents/Image-Line/FL Studio/Settings/Audio scripts`.
pub fn edison_script_directories(
    app_user_data_directory: Option<PathBuf>,
    home_directory: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(root) = app_user_data_directory {
        directories.push(root.join("Settings").join("Audio scripts"));
    }
    if let Some(home) = home_directory {
        directories.push(
            home.join("Documents")
                .join("Image-Line")
                .join("FL Studio")
                .join("Settings")
                .join("Audio scripts"),
        );
    }
    directories.dedup();
    directories
}

/// Discover scripts across every candidate folder. Earlier folders win
/// when two scripts share a name and group.
pub fn discover_all_edison_scripts(directories: &[PathBuf]) -> Vec<EdisonScript> {
    let mut scripts = Vec::new();
    for directory in directories {
        if let Ok(found) = discover_edison_scripts(directory) {
            for script in found {
                let duplicate = scripts.iter().any(|existing: &EdisonScript| {
                    existing.name == script.name && existing.group == script.group
                });
                if !duplicate {
                    scripts.push(script);
                }
            }
        }
    }
    scripts.sort_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    scripts
}

const LAST_SCRIPT_HEADER: &str = "version=1";
const LAST_SCRIPT_KEY: &str = "last_script=";

pub fn serialize_last_edison_script(script: Option<&Path>) -> String {
    let mut contents = String::from(LAST_SCRIPT_HEADER);
    contents.push('\n');
    if let Some(script) = script {
        contents.push_str(LAST_SCRIPT_KEY);
        contents.push_str(&script.to_string_lossy());
        contents.push('\n');
    }
    contents
}

pub fn parse_last_edison_script(contents: &str) -> Option<PathBuf> {
    contents.lines().find_map(|line| {
        line.strip_prefix(LAST_SCRIPT_KEY)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

pub fn load_last_edison_script(settings_file: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(settings_file).ok()?;
    let script = parse_last_edison_script(&contents)?;
    script.is_file().then_some(script)
}

pub fn save_last_edison_script(settings_file: &Path, script: &Path) -> Result<(), String> {
    if let Some(parent) = settings_file.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create Edison script settings folder {}: {error}",
                parent.display()
            )
        })?;
    }
    fs::write(settings_file, serialize_last_edison_script(Some(script))).map_err(|error| {
        format!(
            "could not save Edison script settings {}: {error}",
            settings_file.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        discover_all_edison_scripts, discover_edison_scripts, edison_script_directories,
        grouped_edison_scripts, load_last_edison_script, parse_last_edison_script,
        save_last_edison_script, serialize_last_edison_script,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn discovers_pyscript_files_recursively_and_ignores_other_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("Normalize.pyscript"), "# script").unwrap();
        fs::write(root.join("notes.txt"), "not a script").unwrap();
        fs::write(root.join("helper.py"), "# module, not a main script").unwrap();
        let generators = root.join("Generators");
        fs::create_dir_all(&generators).unwrap();
        fs::write(generators.join("Noise Generator.pyscript"), "# script").unwrap();
        fs::write(generators.join("UPPER.PYSCRIPT"), "# script").unwrap();

        let scripts = discover_edison_scripts(root).unwrap();

        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0].name, "Normalize");
        assert_eq!(scripts[0].group, None);
        assert_eq!(scripts[1].name, "Noise Generator");
        assert_eq!(scripts[1].group.as_deref(), Some("Generators"));
        assert_eq!(scripts[2].name, "UPPER");
        assert_eq!(scripts[2].group.as_deref(), Some("Generators"));
    }

    #[test]
    fn missing_scripts_folder_discovers_nothing() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("Audio scripts");
        assert!(discover_edison_scripts(&missing).unwrap().is_empty());
    }

    #[test]
    fn menu_grouping_keeps_ungrouped_scripts_first() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("B.pyscript"), "").unwrap();
        fs::write(root.join("A.pyscript"), "").unwrap();
        let folder = root.join("Tools");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("C.pyscript"), "").unwrap();

        let scripts = discover_edison_scripts(root).unwrap();
        let groups = grouped_edison_scripts(&scripts);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, None);
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[0].1[0].name, "A");
        assert_eq!(groups[1].0.as_deref(), Some("Tools"));
        assert_eq!(groups[1].1[0].name, "C");
    }

    #[test]
    fn script_directories_follow_the_documented_audio_scripts_layout() {
        let directories = edison_script_directories(
            Some(PathBuf::from("/app-data")),
            Some(PathBuf::from("/home/test")),
        );
        assert_eq!(
            directories,
            [
                PathBuf::from("/app-data/Settings/Audio scripts"),
                PathBuf::from(
                    "/home/test/Documents/Image-Line/FL Studio/Settings/Audio scripts"
                ),
            ]
        );
        assert!(edison_script_directories(None, None).is_empty());
    }

    #[test]
    fn discovery_across_directories_dedupes_by_name_and_group() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(first.path().join("Amp.pyscript"), "first").unwrap();
        fs::write(second.path().join("Amp.pyscript"), "second").unwrap();
        fs::write(second.path().join("Blur.pyscript"), "second").unwrap();

        let scripts = discover_all_edison_scripts(&[
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]);

        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].name, "Amp");
        assert_eq!(scripts[0].path, first.path().join("Amp.pyscript"));
        assert_eq!(scripts[1].name, "Blur");
    }

    #[test]
    fn last_script_state_round_trips_through_the_settings_file() {
        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("My Script.pyscript");
        fs::write(&script, "# script").unwrap();
        let settings_file = temporary.path().join("settings").join("edison-scripts.txt");

        assert_eq!(load_last_edison_script(&settings_file), None);
        save_last_edison_script(&settings_file, &script).unwrap();
        assert_eq!(
            load_last_edison_script(&settings_file).as_deref(),
            Some(script.as_path())
        );

        // A last script that no longer exists is forgotten, matching
        // "files saved here will be visible the next time" discovery.
        fs::remove_file(&script).unwrap();
        assert_eq!(load_last_edison_script(&settings_file), None);
    }

    #[test]
    fn last_script_serialization_is_stable() {
        assert_eq!(serialize_last_edison_script(None), "version=1\n");
        assert_eq!(
            parse_last_edison_script("version=1\nlast_script=/tmp/a b.pyscript\n"),
            Some(PathBuf::from("/tmp/a b.pyscript"))
        );
        assert_eq!(parse_last_edison_script("version=1\n"), None);
        assert_eq!(parse_last_edison_script("garbage"), None);
    }
}
