use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

const VST3_MANUAL_LATENCY_OFFSETS_FILE: &str = "vst3-manual-latency-offsets.txt";
const VST3_PROJECT_MANUAL_LATENCY_OFFSETS_DIRECTORY: &str = "vst3-project-manual-latency-offsets";

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
        encode_plugin_uid, parse_vst3_manual_latency_offsets, serialize_vst3_manual_latency_offsets,
    };

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
}
