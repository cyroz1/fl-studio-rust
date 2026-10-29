use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const VST3_MANUAL_LATENCY_OFFSETS_FILE: &str = "vst3-manual-latency-offsets.txt";
const VST3_PROJECT_MANUAL_LATENCY_OFFSETS_DIRECTORY: &str = "vst3-project-manual-latency-offsets";
const CLAP_PROJECT_INSTRUMENTS_DIRECTORY: &str = "clap-project-instruments";
const CLAP_PROJECT_MIXER_EFFECTS_DIRECTORY: &str = "clap-project-mixer-effects";
pub const MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CLAP_PROJECT_MIXER_AUXILIARY_BUSES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapProjectInstrumentAssignment {
    pub path: PathBuf,
    pub plugin_id: String,
    pub plugin_name: String,
    pub state: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapProjectMixerEffectAssignment {
    pub path: PathBuf,
    pub plugin_id: String,
    pub plugin_name: String,
    pub state: Option<Vec<u8>>,
    /// Maps source Mixer insert ordinals to CLAP auxiliary input port indices.
    pub auxiliary_input_source_map: BTreeMap<usize, usize>,
}

fn app_settings_directory() -> Option<PathBuf> {
    let root = if cfg!(target_os = "windows") {
        env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
            env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|home| home.join("AppData").join("Roaming"))
        })
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".config"))
            })
    }?;
    Some(root.join("fl-studio-rebuild"))
}

fn vst3_manual_latency_offsets_file() -> Option<PathBuf> {
    Some(app_settings_directory()?.join(VST3_MANUAL_LATENCY_OFFSETS_FILE))
}

fn vst3_project_manual_latency_offsets_file(project_id: &str) -> Option<PathBuf> {
    if project_id.len() != 16 || !project_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        app_settings_directory()?
            .join(VST3_PROJECT_MANUAL_LATENCY_OFFSETS_DIRECTORY)
            .join(format!("{}.txt", project_id.to_ascii_lowercase())),
    )
}

fn clap_project_instruments_file(project_id: &str) -> Option<PathBuf> {
    if project_id.len() != 16 || !project_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        app_settings_directory()?
            .join(CLAP_PROJECT_INSTRUMENTS_DIRECTORY)
            .join(format!("{}.txt", project_id.to_ascii_lowercase())),
    )
}

fn clap_project_mixer_effects_file(project_id: &str) -> Option<PathBuf> {
    if project_id.len() != 16 || !project_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        app_settings_directory()?
            .join(CLAP_PROJECT_MIXER_EFFECTS_DIRECTORY)
            .join(format!("{}.txt", project_id.to_ascii_lowercase())),
    )
}

/// Read machine-local VST3 latency offsets shared by the desktop app and CLI renderers.
pub fn load_vst3_manual_latency_offsets() -> BTreeMap<String, i32> {
    vst3_manual_latency_offsets_file()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| parse_vst3_manual_latency_offsets(&contents))
        .unwrap_or_default()
}

/// Save machine-local VST3 latency offsets shared by the desktop app and CLI renderers.
pub fn save_vst3_manual_latency_offsets(offsets: &BTreeMap<String, i32>) -> Result<(), String> {
    let path = vst3_manual_latency_offsets_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the VST3 latency offset path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    fs::write(&path, serialize_vst3_manual_latency_offsets(offsets))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Read project-scoped VST3 latency offsets keyed by a stable project instance identifier.
pub fn load_vst3_project_manual_latency_offsets(project_id: &str) -> BTreeMap<String, i32> {
    vst3_project_manual_latency_offsets_file(project_id)
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| parse_vst3_project_manual_latency_offsets(&contents))
        .unwrap_or_default()
}

/// Save project-scoped VST3 latency offsets outside the FLP, keyed by the project's stable path ID.
pub fn save_vst3_project_manual_latency_offsets(
    project_id: &str,
    offsets: &BTreeMap<String, i32>,
) -> Result<(), String> {
    let path = vst3_project_manual_latency_offsets_file(project_id)
        .ok_or_else(|| "the project latency offset path is not valid".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the project latency offset path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    fs::write(
        &path,
        serialize_vst3_project_manual_latency_offsets(offsets),
    )
    .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Read machine-local CLAP instrument assignments for a project and channel.
pub fn load_clap_project_instruments(
    project_id: &str,
) -> BTreeMap<u16, ClapProjectInstrumentAssignment> {
    clap_project_instruments_file(project_id)
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| parse_clap_project_instruments(&contents))
        .unwrap_or_default()
}

/// Save machine-local CLAP instrument assignments outside the FLP file.
pub fn save_clap_project_instruments(
    project_id: &str,
    assignments: &BTreeMap<u16, ClapProjectInstrumentAssignment>,
) -> Result<(), String> {
    if assignments.values().any(|assignment| {
        assignment
            .state
            .as_ref()
            .is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
    }) {
        return Err(format!(
            "CLAP plug-in state exceeds the {} MiB project limit",
            MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES / (1024 * 1024)
        ));
    }
    let path = clap_project_instruments_file(project_id)
        .ok_or_else(|| "the project CLAP instrument path is not valid".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the project CLAP instrument path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    fs::write(&path, serialize_clap_project_instruments(assignments))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Read machine-local CLAP effect assignments for project Mixer insert slots.
pub fn load_clap_project_mixer_effects(
    project_id: &str,
) -> BTreeMap<(usize, u16), ClapProjectMixerEffectAssignment> {
    clap_project_mixer_effects_file(project_id)
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| parse_clap_project_mixer_effects(&contents))
        .unwrap_or_default()
}

/// Save machine-local CLAP effect assignments outside the FLP file.
pub fn save_clap_project_mixer_effects(
    project_id: &str,
    assignments: &BTreeMap<(usize, u16), ClapProjectMixerEffectAssignment>,
) -> Result<(), String> {
    if assignments.values().any(|assignment| {
        assignment
            .state
            .as_ref()
            .is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
    }) {
        return Err(format!(
            "CLAP plug-in state exceeds the {} MiB project limit",
            MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES / (1024 * 1024)
        ));
    }
    let path = clap_project_mixer_effects_file(project_id)
        .ok_or_else(|| "the project CLAP Mixer effect path is not valid".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the project CLAP Mixer effect path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    fs::write(&path, serialize_clap_project_mixer_effects(assignments))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

pub fn parse_clap_project_mixer_effects(
    contents: &str,
) -> BTreeMap<(usize, u16), ClapProjectMixerEffectAssignment> {
    let mut assignments = BTreeMap::new();
    let Some(version) = contents.lines().next() else {
        return assignments;
    };
    let version_two = version == "clap-project-mixer-effects-v2";
    if !version_two && version != "clap-project-mixer-effects-v1" {
        return assignments;
    }
    for line in contents.lines().skip(1) {
        let mut fields = line.split('\t');
        let Some(insert_ordinal) = fields.next().and_then(|field| field.parse::<usize>().ok())
        else {
            continue;
        };
        let Some(slot_index) = fields.next().and_then(|field| field.parse::<u16>().ok()) else {
            continue;
        };
        let Some(path) = fields.next().and_then(decode_path) else {
            continue;
        };
        let Some(plugin_id) = fields.next().and_then(decode_plugin_uid) else {
            continue;
        };
        let Some(plugin_name) = fields.next().and_then(decode_plugin_uid) else {
            continue;
        };
        let Some(encoded_state) = fields.next() else {
            continue;
        };
        let state = if encoded_state == "-" {
            None
        } else if encoded_state.len() <= MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES * 2 {
            let Some(state) = decode_bytes_hex(encoded_state)
                .filter(|state| state.len() <= MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
            else {
                continue;
            };
            Some(state)
        } else {
            continue;
        };
        let auxiliary_input_source_map = if version_two {
            let Some(encoded_map) = fields.next() else {
                continue;
            };
            let Some(map) = parse_clap_mixer_auxiliary_input_source_map(encoded_map) else {
                continue;
            };
            map
        } else {
            BTreeMap::new()
        };
        if fields.next().is_some()
            || insert_ordinal >= crate::audio::MIXER_TRACK_METER_COUNT
            || slot_index >= 10
            || path.as_os_str().is_empty()
            || plugin_id.is_empty()
            || plugin_name.is_empty()
        {
            continue;
        }
        assignments.insert(
            (insert_ordinal, slot_index),
            ClapProjectMixerEffectAssignment {
                path,
                plugin_id,
                plugin_name,
                state,
                auxiliary_input_source_map,
            },
        );
    }
    assignments
}

pub fn serialize_clap_project_mixer_effects(
    assignments: &BTreeMap<(usize, u16), ClapProjectMixerEffectAssignment>,
) -> String {
    let mut contents = String::from("clap-project-mixer-effects-v2\n");
    for ((insert_ordinal, slot_index), assignment) in assignments {
        if *insert_ordinal >= crate::audio::MIXER_TRACK_METER_COUNT
            || *slot_index >= 10
            || assignment.path.as_os_str().is_empty()
            || assignment.plugin_id.is_empty()
            || assignment.plugin_name.is_empty()
            || assignment
                .state
                .as_ref()
                .is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
            || assignment
                .auxiliary_input_source_map
                .iter()
                .any(|(source_track, bus_index)| {
                    *source_track == 0
                        || *source_track >= crate::audio::MIXER_TRACK_METER_COUNT
                        || *bus_index >= MAX_CLAP_PROJECT_MIXER_AUXILIARY_BUSES
                })
        {
            continue;
        }
        contents.push_str(&insert_ordinal.to_string());
        contents.push('\t');
        contents.push_str(&slot_index.to_string());
        contents.push('\t');
        contents.push_str(&encode_path(&assignment.path));
        contents.push('\t');
        contents.push_str(&encode_plugin_uid(&assignment.plugin_id));
        contents.push('\t');
        contents.push_str(&encode_plugin_uid(&assignment.plugin_name));
        contents.push('\t');
        if let Some(state) = &assignment.state {
            contents.push_str(&encode_bytes_hex(state));
        } else {
            contents.push('-');
        }
        contents.push('\t');
        if assignment.auxiliary_input_source_map.is_empty() {
            contents.push('-');
        } else {
            for (index, (source_track, bus_index)) in
                assignment.auxiliary_input_source_map.iter().enumerate()
            {
                if index > 0 {
                    contents.push(',');
                }
                contents.push_str(&format!("{source_track}:{bus_index}"));
            }
        }
        contents.push('\n');
    }
    contents
}

fn parse_clap_mixer_auxiliary_input_source_map(encoded: &str) -> Option<BTreeMap<usize, usize>> {
    if encoded == "-" {
        return Some(BTreeMap::new());
    }
    let mut mapping = BTreeMap::new();
    for entry in encoded.split(',') {
        let (source_track, bus_index) = entry.split_once(':')?;
        let source_track = source_track.parse::<usize>().ok()?;
        let bus_index = bus_index.parse::<usize>().ok()?;
        if source_track == 0
            || source_track >= crate::audio::MIXER_TRACK_METER_COUNT
            || bus_index >= MAX_CLAP_PROJECT_MIXER_AUXILIARY_BUSES
            || mapping.insert(source_track, bus_index).is_some()
        {
            return None;
        }
    }
    Some(mapping)
}

pub fn parse_clap_project_instruments(
    contents: &str,
) -> BTreeMap<u16, ClapProjectInstrumentAssignment> {
    let mut assignments = BTreeMap::new();
    let mut lines = contents.lines();
    let version = match lines.next() {
        Some("clap-project-instruments-v1") => 1,
        Some("clap-project-instruments-v2") => 2,
        _ => return assignments,
    };
    for line in lines {
        let mut fields = line.split('\t');
        let Some(channel_id) = fields.next().and_then(|field| field.parse::<u16>().ok()) else {
            continue;
        };
        let Some(path) = fields.next().and_then(decode_path) else {
            continue;
        };
        let Some(plugin_id) = fields.next().and_then(decode_plugin_uid) else {
            continue;
        };
        let Some(plugin_name) = fields.next().and_then(decode_plugin_uid) else {
            continue;
        };
        let state = if version == 2 {
            let Some(encoded_state) = fields.next() else {
                continue;
            };
            if encoded_state == "-" {
                None
            } else if encoded_state.len() <= MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES * 2 {
                let Some(state) = decode_bytes_hex(encoded_state) else {
                    continue;
                };
                Some(state)
            } else {
                continue;
            }
        } else {
            None
        };
        if fields.next().is_some()
            || path.as_os_str().is_empty()
            || plugin_id.is_empty()
            || plugin_name.is_empty()
        {
            continue;
        }
        assignments.insert(
            channel_id,
            ClapProjectInstrumentAssignment {
                path,
                plugin_id,
                plugin_name,
                state,
            },
        );
    }
    assignments
}

pub fn serialize_clap_project_instruments(
    assignments: &BTreeMap<u16, ClapProjectInstrumentAssignment>,
) -> String {
    let mut contents = String::from("clap-project-instruments-v2\n");
    for (channel_id, assignment) in assignments {
        if assignment.path.as_os_str().is_empty()
            || assignment.plugin_id.is_empty()
            || assignment.plugin_name.is_empty()
        {
            continue;
        }
        contents.push_str(&channel_id.to_string());
        contents.push('\t');
        contents.push_str(&encode_path(&assignment.path));
        contents.push('\t');
        contents.push_str(&encode_plugin_uid(&assignment.plugin_id));
        contents.push('\t');
        contents.push_str(&encode_plugin_uid(&assignment.plugin_name));
        contents.push('\t');
        if let Some(state) = &assignment.state {
            contents.push_str(&encode_bytes_hex(state));
        } else {
            contents.push('-');
        }
        contents.push('\n');
    }
    contents
}

pub fn parse_vst3_manual_latency_offsets(contents: &str) -> BTreeMap<String, i32> {
    let mut offsets = BTreeMap::new();
    for line in contents.lines() {
        let Some(encoded) = line.strip_prefix("offset=") else {
            continue;
        };
        let mut fields = encoded.split('\t');
        let Some(plugin_uid) = fields.next().and_then(decode_plugin_uid) else {
            continue;
        };
        let Some(offset_samples) = fields.next().and_then(|field| field.parse::<i32>().ok()) else {
            continue;
        };
        if fields.next().is_some() || plugin_uid.is_empty() {
            continue;
        }
        offsets.insert(plugin_uid.to_ascii_lowercase(), offset_samples);
    }
    offsets
}

pub fn serialize_vst3_manual_latency_offsets(offsets: &BTreeMap<String, i32>) -> String {
    let mut contents = String::from("version=1\n");
    for (plugin_uid, offset_samples) in offsets {
        if plugin_uid.is_empty() {
            continue;
        }
        contents.push_str("offset=");
        contents.push_str(&encode_plugin_uid(plugin_uid));
        contents.push('\t');
        contents.push_str(&offset_samples.to_string());
        contents.push('\n');
    }
    contents
}

pub fn parse_vst3_project_manual_latency_offsets(contents: &str) -> BTreeMap<String, i32> {
    let mut offsets = BTreeMap::new();
    let mut lines = contents.lines();
    if lines.next() != Some("vst3-project-manual-latency-offsets-v1") {
        return offsets;
    }
    for line in lines {
        let mut fields = line.split('\t');
        let Some(key) = fields.next().and_then(decode_plugin_uid) else {
            continue;
        };
        let Some(offset_samples) = fields.next().and_then(|field| field.parse::<i32>().ok()) else {
            continue;
        };
        if fields.next().is_some() || key.is_empty() {
            continue;
        }
        offsets.insert(key, offset_samples);
    }
    offsets
}

pub fn serialize_vst3_project_manual_latency_offsets(offsets: &BTreeMap<String, i32>) -> String {
    let mut contents = String::from("vst3-project-manual-latency-offsets-v1\n");
    for (key, offset_samples) in offsets {
        if key.is_empty() {
            continue;
        }
        contents.push_str(&encode_plugin_uid(key));
        contents.push('\t');
        contents.push_str(&offset_samples.to_string());
        contents.push('\n');
    }
    contents
}

fn encode_plugin_uid(plugin_uid: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(plugin_uid.len() * 2);
    for byte in plugin_uid.bytes() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_plugin_uid(encoded: &str) -> Option<String> {
    let (pairs, remainder) = encoded.as_bytes().as_chunks::<2>();
    if pairs.is_empty() || !remainder.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).ok().filter(|uid| !uid.is_empty())
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    encode_bytes_hex(path.as_os_str().as_bytes())
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;

    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    encode_bytes_hex(&bytes)
}

#[cfg(not(any(unix, windows)))]
fn encode_path(path: &Path) -> String {
    encode_bytes_hex(path.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn decode_path(encoded: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    Some(PathBuf::from(std::ffi::OsString::from_vec(
        decode_bytes_hex(encoded)?,
    )))
}

#[cfg(windows)]
fn decode_path(encoded: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    let bytes = decode_bytes_hex(encoded)?;
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let wide = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect::<Vec<_>>();
    Some(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
fn decode_path(encoded: &str) -> Option<PathBuf> {
    String::from_utf8(decode_bytes_hex(encoded)?)
        .ok()
        .map(PathBuf::from)
}

fn encode_bytes_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_bytes_hex(encoded: &str) -> Option<Vec<u8>> {
    let (pairs, remainder) = encoded.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        ClapProjectInstrumentAssignment, encode_path, encode_plugin_uid,
        parse_clap_project_instruments, parse_vst3_manual_latency_offsets,
        serialize_clap_project_instruments, serialize_vst3_manual_latency_offsets,
    };
    use std::path::PathBuf;

    #[test]
    fn vst3_manual_latency_offsets_round_trip_and_normalize_class_uids() {
        let offsets = BTreeMap::from([
            ("com.example.synth".to_owned(), -2_048),
            ("org.example.fx".to_owned(), 256),
        ]);
        let encoded = serialize_vst3_manual_latency_offsets(&offsets);
        assert_eq!(parse_vst3_manual_latency_offsets(&encoded), offsets);

        let mixed_case = format!(
            "version=1\noffset={}\t-16\noffset={}\t7\textra\n",
            encode_plugin_uid("Com.Example.Plugin"),
            encode_plugin_uid("org.example.bad")
        );
        assert_eq!(
            parse_vst3_manual_latency_offsets(&mixed_case),
            BTreeMap::from([("com.example.plugin".to_owned(), -16)])
        );
    }

    #[test]
    fn clap_project_instrument_assignments_round_trip_escaped_fields() {
        let assignments = BTreeMap::from([(
            7,
            ClapProjectInstrumentAssignment {
                path: PathBuf::from("/tmp/Écho Synth\tMk II.clap"),
                plugin_id: "org.example\tvoice".to_owned(),
                plugin_name: "Écho\nMk II".to_owned(),
                state: Some(vec![0, 1, 0xff, b'\n']),
            },
        )]);

        let serialized = serialize_clap_project_instruments(&assignments);

        assert_eq!(parse_clap_project_instruments(&serialized), assignments);
    }

    #[test]
    fn clap_project_instrument_assignments_reject_malformed_records() {
        let parsed = parse_clap_project_instruments(
            "clap-project-instruments-v1\nnot-a-channel\t2f746d702f78\t6964\t6e616d65\n8\t2f746d702f78\t6964\t\n",
        );

        assert!(parsed.is_empty());
    }

    #[test]
    fn clap_project_instrument_assignments_read_v1_without_state() {
        let path = PathBuf::from("/tmp/x");
        let contents = format!(
            "clap-project-instruments-v1\n8\t{}\t{}\t{}\n",
            encode_path(&path),
            encode_plugin_uid("id"),
            encode_plugin_uid("name"),
        );
        let parsed = parse_clap_project_instruments(&contents);
        assert_eq!(
            parsed.get(&8),
            Some(&ClapProjectInstrumentAssignment {
                path,
                plugin_id: "id".to_owned(),
                plugin_name: "name".to_owned(),
                state: None,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn clap_project_instrument_assignments_preserve_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let assignments = BTreeMap::from([(
            4,
            ClapProjectInstrumentAssignment {
                path: PathBuf::from(std::ffi::OsString::from_vec(
                    b"/tmp/plug-\xff.clap".to_vec(),
                )),
                plugin_id: "org.example.synth".to_owned(),
                plugin_name: "Example Synth".to_owned(),
                state: None,
            },
        )]);

        assert_eq!(
            parse_clap_project_instruments(&serialize_clap_project_instruments(&assignments)),
            assignments
        );
    }
}
