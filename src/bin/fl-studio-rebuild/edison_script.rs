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

// ---------------------------------------------------------------------------
// Script execution engine
//
// A `.pyscript` runs once, top to bottom, against the sample loaded in the
// editor. Execution happens in a child Python process running the embedded
// `edison_script_runner.py`, which provides the `enveditor` module exactly
// as documented by Image-Line's FL Studio API stubs:
//
// * `Sample` (the `EditorSample` instance): `Length`, `NumChans`,
//   `SampleRate`, `RegionCount`, `GetSampleAt`, `SetSampleAt`,
//   `NormalizeFromTo`, `AmpFromTo`, `SilenceFromTo`, `SineFromTo`,
//   `PasteFromTo`, `MsToSamples`, `NormalizeFormat`, `GetRegion`, plus
//   `CenterFromTo`, which the stubs omit but real-world scripts use for
//   DC offset removal.
// * `Editor`: `SelectionStartS` / `SelectionEndS` (inclusive, as in FL).
// * `ScriptDialog`: `AddInput`, `AddInputKnob`, `AddInputCombo`,
//   `GetInputValue`, `Execute`.
// * `Utils`: `ProgressMsg`, `ShowMessage`.
//
// Dialogs are two-phase because the runner is one-shot: a first run whose
// script calls `Execute()` ends as `DialogRequested` with the dialog spec
// (any sample edits from that pass are discarded, so scripts are expected
// to call `Execute()` before processing, as the stubs prescribe); the host
// shows the dialog natively and re-runs the script with the chosen values,
// and `Execute()` then returns `true` immediately.
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const EDISON_SCRIPT_RUNNER: &str = include_str!("edison_script_runner.py");
const MAX_EDISON_SCRIPT_BYTES: u64 = 1024 * 1024;
const EDISON_SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

/// The sample buffer a script runs against: deinterleaved f32 channels.
#[derive(Clone, Debug, PartialEq)]
pub struct EdisonScriptSample {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl EdisonScriptSample {
    pub fn frame_count(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdisonScriptInputKind {
    /// `AddInput`: a 0.0..=1.0 value with a text box and knob.
    Value,
    /// `AddInputKnob`: a value within an explicit min/max range.
    Knob,
    /// `AddInputCombo`: an index into `options`.
    Combo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdisonScriptDialogInput {
    pub name: String,
    pub kind: EdisonScriptInputKind,
    pub default: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdisonScriptDialogSpec {
    pub title: String,
    pub description: String,
    pub inputs: Vec<EdisonScriptDialogInput>,
}

impl EdisonScriptDialogSpec {
    /// The values a run should use when the user accepts the dialog
    /// without touching a control.
    pub fn default_values(&self) -> BTreeMap<String, f64> {
        self.inputs
            .iter()
            .map(|input| (input.name.clone(), input.default))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EdisonScriptRunOutcome {
    Completed {
        sample: EdisonScriptSample,
        messages: Vec<String>,
        progress: Option<(String, i64, i64)>,
    },
    DialogRequested {
        dialog: EdisonScriptDialogSpec,
    },
    Failed {
        error: String,
    },
}

fn edison_python_command() -> Result<(PathBuf, Vec<String>), String> {
    if let Some(executable) = std::env::var_os("FL_STUDIO_REBUILD_PYTHON") {
        return Ok((PathBuf::from(executable), Vec::new()));
    }
    if cfg!(target_os = "windows") {
        Ok((PathBuf::from("py"), vec!["-3".to_owned()]))
    } else {
        Ok((PathBuf::from("python3"), Vec::new()))
    }
}

/// Run one Edison script against `sample`.
///
/// `selection_start` / `selection_end` use this app's editor convention
/// (end exclusive, may equal the frame count); they are converted to the
/// inclusive end FL scripts expect. When the script asks for a dialog and
/// `dialog_values` is `None`, the outcome is `DialogRequested`; pass the
/// user's values to run it to completion.
pub fn run_edison_script(
    script_path: &Path,
    sample: &EdisonScriptSample,
    selection_start: usize,
    selection_end: usize,
    dialog_values: Option<&BTreeMap<String, f64>>,
) -> Result<EdisonScriptRunOutcome, String> {
    let metadata = fs::metadata(script_path).map_err(|error| {
        format!(
            "could not read Edison script {}: {error}",
            script_path.display()
        )
    })?;
    if metadata.len() > MAX_EDISON_SCRIPT_BYTES {
        return Err(format!(
            "Edison script is larger than {} bytes: {}",
            MAX_EDISON_SCRIPT_BYTES,
            script_path.display()
        ));
    }
    let frame_count = sample.frame_count();
    let selection_start = if frame_count == 0 {
        0
    } else {
        selection_start.min(frame_count - 1)
    };
    let selection_end_inclusive = if frame_count == 0 {
        0
    } else {
        selection_end
            .min(frame_count)
            .saturating_sub(1)
            .max(selection_start)
    };

    let directory = tempfile::tempdir()
        .map_err(|error| format!("could not create Edison script work folder: {error}"))?;
    let work = directory.path();
    let runner_path = work.join("edison_script_runner.py");
    fs::write(&runner_path, EDISON_SCRIPT_RUNNER)
        .map_err(|error| format!("could not write Edison script runner: {error}"))?;
    let sample_path = work.join("sample.bin");
    let out_sample_path = work.join("sample-out.bin");
    let result_path = work.join("result.json");
    let job_path = work.join("job.json");
    let stdout_path = work.join("stdout.log");
    let stderr_path = work.join("stderr.log");
    {
        let mut buffer = Vec::with_capacity(4 * frame_count * sample.channels.len());
        for channel in &sample.channels {
            for value in channel {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
        }
        fs::write(&sample_path, buffer)
            .map_err(|error| format!("could not write Edison script sample: {error}"))?;
    }
    let job = json!({
        "script_path": script_path,
        "sample_file": sample_path,
        "out_sample_file": out_sample_path,
        "result_file": result_path,
        "num_chans": sample.channels.len(),
        "length": frame_count,
        "sample_rate": sample.sample_rate,
        "selection_start": selection_start,
        "selection_end": selection_end_inclusive,
        "dialog_values": dialog_values,
    });
    fs::write(
        &job_path,
        serde_json::to_vec(&job).expect("job JSON always serializes"),
    )
    .map_err(|error| format!("could not write Edison script job: {error}"))?;

    let (program, prefix_args) = edison_python_command()?;
    let stdout_file = fs::File::create(&stdout_path)
        .map_err(|error| format!("could not create Edison script log: {error}"))?;
    let stderr_file = fs::File::create(&stderr_path)
        .map_err(|error| format!("could not create Edison script log: {error}"))?;
    let mut child = Command::new(&program)
        .args(&prefix_args)
        .arg(&runner_path)
        .arg(&job_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|error| {
            format!(
                "could not start Python ({}) for Edison scripts: {error}",
                program.display()
            )
        })?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > EDISON_SCRIPT_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(EdisonScriptRunOutcome::Failed {
                        error: format!(
                            "the script did not finish within {} seconds and was stopped",
                            EDISON_SCRIPT_TIMEOUT.as_secs()
                        ),
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(format!("could not run Edison script: {error}"));
            }
        }
    };

    let result_text = fs::read_to_string(&result_path).unwrap_or_default();
    let result: Value = serde_json::from_str(&result_text).unwrap_or(Value::Null);
    if result == Value::Null {
        let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
        let detail = stderr.trim();
        return Ok(EdisonScriptRunOutcome::Failed {
            error: if detail.is_empty() {
                format!("the script runner exited with {status} without a result")
            } else {
                format!("the script runner failed: {detail}")
            },
        });
    }
    let messages = result["messages"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match result["status"].as_str() {
        Some("dialog") => Ok(EdisonScriptRunOutcome::DialogRequested {
            dialog: parse_edison_dialog_spec(&result["dialog"]),
        }),
        Some("error") => {
            let error = result["error"]
                .as_str()
                .unwrap_or("the script failed without an error message")
                .trim()
                .to_owned();
            Ok(EdisonScriptRunOutcome::Failed { error })
        }
        Some("completed") => {
            let num_chans = result["num_chans"].as_u64().unwrap_or(0) as usize;
            let length = result["length"].as_u64().unwrap_or(0) as usize;
            let sample_rate = result["sample_rate"].as_u64().unwrap_or(0) as u32;
            let raw = fs::read(&out_sample_path).map_err(|error| {
                format!("could not read the script's output sample: {error}")
            })?;
            if raw.len() != 4 * num_chans * length {
                return Ok(EdisonScriptRunOutcome::Failed {
                    error: format!(
                        "the script runner returned {} sample bytes for {num_chans} channels x {length} frames",
                        raw.len()
                    ),
                });
            }
            let mut channels = Vec::with_capacity(num_chans);
            for channel_index in 0..num_chans {
                let start = 4 * length * channel_index;
                let mut channel = Vec::with_capacity(length);
                for offset in 0..length {
                    let bytes = raw[start + 4 * offset..start + 4 * offset + 4]
                        .try_into()
                        .expect("4-byte sample slice");
                    channel.push(f32::from_le_bytes(bytes));
                }
                channels.push(channel);
            }
            let progress = result["progress"].as_object().map(|progress| {
                (
                    progress["message"].as_str().unwrap_or_default().to_owned(),
                    progress["position"].as_i64().unwrap_or(0),
                    progress["total"].as_i64().unwrap_or(0),
                )
            });
            Ok(EdisonScriptRunOutcome::Completed {
                sample: EdisonScriptSample {
                    sample_rate,
                    channels,
                },
                messages,
                progress,
            })
        }
        other => Ok(EdisonScriptRunOutcome::Failed {
            error: format!("the script runner reported an unknown status: {other:?}"),
        }),
    }
}

fn parse_edison_dialog_spec(value: &Value) -> EdisonScriptDialogSpec {
    let inputs = value["inputs"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| EdisonScriptDialogInput {
                    name: entry["name"].as_str().unwrap_or_default().to_owned(),
                    kind: match entry["kind"].as_str() {
                        Some("knob") => EdisonScriptInputKind::Knob,
                        Some("combo") => EdisonScriptInputKind::Combo,
                        _ => EdisonScriptInputKind::Value,
                    },
                    default: entry["default"].as_f64().unwrap_or(0.0),
                    minimum: entry["min"].as_f64().unwrap_or(0.0),
                    maximum: entry["max"].as_f64().unwrap_or(0.0),
                    options: entry["options"]
                        .as_array()
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|option| option.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    EdisonScriptDialogSpec {
        title: value["title"].as_str().unwrap_or_default().to_owned(),
        description: value["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        inputs,
    }
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

#[cfg(test)]
mod engine_tests {
    use super::{
        EdisonScriptInputKind, EdisonScriptRunOutcome, EdisonScriptSample, run_edison_script,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    fn write_script(directory: &tempfile::TempDir, source: &str) -> PathBuf {
        let path = directory.path().join("script.pyscript");
        fs::write(&path, source).unwrap();
        path
    }

    fn stereo_sample() -> EdisonScriptSample {
        EdisonScriptSample {
            sample_rate: 8_000,
            channels: vec![
                vec![0.1, 0.2, 0.3, 0.4],
                vec![-0.1, -0.2, -0.3, -0.4],
            ],
        }
    }

    fn completed(
        outcome: EdisonScriptRunOutcome,
    ) -> (EdisonScriptSample, Vec<String>, Option<(String, i64, i64)>) {
        match outcome {
            EdisonScriptRunOutcome::Completed {
                sample,
                messages,
                progress,
            } => (sample, messages, progress),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn amp_and_point_access_process_only_the_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(
            &temporary,
            "from enveditor import *\n\
             EditorSample.AmpFromTo(Editor.SelectionStartS, Editor.SelectionEndS, 2.0)\n\
             EditorSample.SetSampleAt(0, 0, EditorSample.GetSampleAt(0, 0) + 0.05)\n",
        );
        // Editor selection 1..3 (end exclusive) is FL's inclusive 1..2.
        let (sample, _, _) =
            completed(run_edison_script(&script, &stereo_sample(), 1, 3, None).unwrap());
        assert_eq!(sample.channels[0], vec![0.15000001, 0.4, 0.6, 0.4]);
        assert_eq!(sample.channels[1], vec![-0.1, -0.4, -0.6, -0.4]);
        assert_eq!(sample.sample_rate, 8_000);
    }

    #[test]
    fn normalize_loop_script_reports_progress_and_messages() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(
            &temporary,
            "from enveditor import *\n\
             if EditorSample.Length:\n\
             \x20   peak = 0.0\n\
             \x20   for i in range(EditorSample.Length):\n\
             \x20       Utils.ProgressMsg('Normalizing', i, EditorSample.Length)\n\
             \x20       peak = max(peak, abs(EditorSample.GetSampleAt(i, 0)))\n\
             \x20   EditorSample.NormalizeFromTo(0, EditorSample.Length - 1, 0.8, False)\n\
             \x20   Utils.ShowMessage('done')\n",
        );
        let (sample, messages, progress) =
            completed(run_edison_script(&script, &stereo_sample(), 0, 4, None).unwrap());
        assert_eq!(messages, ["done"]);
        assert_eq!(progress, Some(("Normalizing".to_owned(), 3, 4)));
        let peak = sample.channels[0]
            .iter()
            .chain(sample.channels[1].iter())
            .fold(0.0f32, |peak, value| peak.max(value.abs()));
        assert!((peak - 0.8).abs() < 0.0001, "peak was {peak}");
    }

    #[test]
    fn dialog_scripts_run_in_two_phases() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(
            &temporary,
            "from enveditor import *\n\
             dialog = ScriptDialog('Gain', 'Pick a gain')\n\
             dialog.AddInputKnob('Gain', 1.0, 0.0, 4.0)\n\
             dialog.AddInputCombo('Mode', 'All,Left', 0)\n\
             if dialog.Execute():\n\
             \x20   EditorSample.AmpFromTo(0, EditorSample.Length - 1, dialog.GetInputValue('Gain'))\n",
        );
        let outcome = run_edison_script(&script, &stereo_sample(), 0, 4, None).unwrap();
        let EdisonScriptRunOutcome::DialogRequested { dialog } = outcome else {
            panic!("expected a dialog request, got {outcome:?}");
        };
        assert_eq!(dialog.title, "Gain");
        assert_eq!(dialog.description, "Pick a gain");
        assert_eq!(dialog.inputs.len(), 2);
        assert_eq!(dialog.inputs[0].kind, EdisonScriptInputKind::Knob);
        assert_eq!(dialog.inputs[0].minimum, 0.0);
        assert_eq!(dialog.inputs[0].maximum, 4.0);
        assert_eq!(dialog.inputs[1].kind, EdisonScriptInputKind::Combo);
        assert_eq!(dialog.inputs[1].options, ["All", "Left"]);
        assert_eq!(dialog.default_values()["Gain"], 1.0);

        // Phase 1 discarded its (non-)edits; phase 2 applies the gain.
        let values = BTreeMap::from([("Gain".to_owned(), 3.0), ("Mode".to_owned(), 0.0)]);
        let (sample, _, _) = completed(
            run_edison_script(&script, &stereo_sample(), 0, 4, Some(&values)).unwrap(),
        );
        assert_eq!(sample.channels[0][0], 0.3);
        assert_eq!(sample.channels[0][1], 0.6);
        assert!((sample.channels[0][2] - 0.9).abs() < 0.0001);
        assert_eq!(sample.channels[0][3], 1.2);
    }

    #[test]
    fn script_errors_are_reported_with_their_message() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(&temporary, "raise ValueError('boom')\n");
        let outcome = run_edison_script(&script, &stereo_sample(), 0, 4, None).unwrap();
        let EdisonScriptRunOutcome::Failed { error } = outcome else {
            panic!("expected failure, got {outcome:?}");
        };
        assert!(error.contains("ValueError: boom"), "error was: {error}");
    }

    #[test]
    fn syntax_errors_are_reported() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(&temporary, "def broken(:\n");
        let outcome = run_edison_script(&script, &stereo_sample(), 0, 4, None).unwrap();
        let EdisonScriptRunOutcome::Failed { error } = outcome else {
            panic!("expected failure, got {outcome:?}");
        };
        assert!(error.contains("SyntaxError"), "error was: {error}");
    }

    #[test]
    fn sine_generation_and_sample_properties_match_the_documented_api() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(
            &temporary,
            "from enveditor import *\n\
             EditorSample.SineFromTo(0, EditorSample.Length - 1, 1000.0, 0.0, 0.5)\n\
             if EditorSample.MsToSamples(500.0) != 4000:\n\
             \x20   raise RuntimeError('MsToSamples mismatch')\n\
             if EditorSample.NumChans != 2 or EditorSample.SampleRate != 8000:\n\
             \x20   raise RuntimeError('property mismatch')\n",
        );
        let (sample, _, _) =
            completed(run_edison_script(&script, &stereo_sample(), 0, 4, None).unwrap());
        // 1000 Hz at 8 kHz: frame 1 is sin(pi/4) * 0.5.
        assert!((sample.channels[0][1] - 0.353_553_4).abs() < 0.0001);
        assert_eq!(sample.channels[0][2], 0.5);
        assert_eq!(sample.channels[0], sample.channels[1]);
    }

    #[test]
    fn empty_sample_scripts_can_report_through_show_message() {
        let temporary = tempfile::tempdir().unwrap();
        let script = write_script(
            &temporary,
            "from enveditor import *\n\
             if not EditorSample.Length:\n\
             \x20   Utils.ShowMessage('Editor is empty!')\n",
        );
        let empty = EdisonScriptSample {
            sample_rate: 44_100,
            channels: vec![Vec::new()],
        };
        let (sample, messages, _) =
            completed(run_edison_script(&script, &empty, 0, 0, None).unwrap());
        assert_eq!(messages, ["Editor is empty!"]);
        assert_eq!(sample.frame_count(), 0);
    }
}
