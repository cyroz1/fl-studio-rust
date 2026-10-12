use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use clack_host::prelude::PluginEntry;

pub const PROBE_FLAG: &str = "--clap-probe";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;
const PROBE_HEADER: &str = "FL_STUDIO_REBUILD_CLAP_PROBE_V1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapPluginDescriptor {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClapInspectionState {
    Inspecting,
    Inspected(Vec<ClapPluginDescriptor>),
    Failed(String),
}

pub type ClapInspectionResponse = (PathBuf, Result<Vec<ClapPluginDescriptor>, String>);

pub fn start_worker(
    egui_context: eframe::egui::Context,
) -> (Sender<PathBuf>, Receiver<ClapInspectionResponse>) {
    let (request_sender, request_receiver) = mpsc::channel::<PathBuf>();
    let (response_sender, response_receiver) = mpsc::channel::<ClapInspectionResponse>();

    thread::Builder::new()
        .name("clap-descriptor-probe".to_owned())
        .spawn(move || {
            while let Ok(path) = request_receiver.recv() {
                let response = (path.clone(), inspect_in_subprocess(&path));
                if response_sender.send(response).is_err() {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("CLAP descriptor worker should start");

    (request_sender, response_receiver)
}

/// Loads one CLAP entry in a child process so a crashing plug-in cannot terminate the DAW.
pub fn inspect_in_subprocess(path: &Path) -> Result<Vec<ClapPluginDescriptor>, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the application executable: {error}"))?;
    let mut child = Command::new(executable)
        .arg(PROBE_FLAG)
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the CLAP descriptor probe: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "CLAP descriptor probe did not provide output".to_owned())?;
    let reader = thread::spawn(move || {
        let mut output = String::new();
        let result = stdout
            .take((MAX_PROBE_OUTPUT_BYTES + 1) as u64)
            .read_to_string(&mut output);
        result.map(|_| output)
    });

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader
                    .join()
                    .map_err(|_| "CLAP descriptor probe output reader stopped".to_owned())?
                    .map_err(|error| {
                        format!("could not read CLAP descriptor probe output: {error}")
                    })?;
                if output.len() > MAX_PROBE_OUTPUT_BYTES {
                    return Err("CLAP descriptor probe output exceeded 1 MiB".to_owned());
                }
                if !status.success() {
                    return Err(format!("CLAP descriptor probe exited with {status}"));
                }
                return parse_probe_output(&output);
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("CLAP descriptor probe timed out after 10 seconds".to_owned());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("could not wait for CLAP descriptor probe: {error}"));
            }
        }
    }
}

/// Entry point used only by the short-lived descriptor-probe child process.
pub fn run_child(path: &Path) -> i32 {
    match unsafe { PluginEntry::load(path) }
        .map_err(|error| format!("could not load CLAP entry: {error}"))
        .and_then(|entry| {
            let factory = entry
                .get_plugin_factory()
                .ok_or_else(|| "CLAP entry does not expose a plug-in factory".to_owned())?;
            let descriptors = factory
                .plugin_descriptors()
                .map(|descriptor| ClapPluginDescriptor {
                    id: descriptor
                        .id()
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    name: descriptor
                        .name()
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    vendor: descriptor
                        .vendor()
                        .map(|value| value.to_string_lossy().into_owned()),
                    version: descriptor
                        .version()
                        .map(|value| value.to_string_lossy().into_owned()),
                    description: descriptor
                        .description()
                        .map(|value| value.to_string_lossy().into_owned()),
                    features: descriptor
                        .features()
                        .map(|value| value.to_string_lossy().into_owned())
                        .collect(),
                })
                .collect::<Vec<_>>();
            Ok(encode_probe_output(&descriptors))
        }) {
        Ok(output) => {
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(output.as_bytes()).is_ok() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn encode_probe_output(descriptors: &[ClapPluginDescriptor]) -> String {
    let mut output = format!("{PROBE_HEADER}\n");
    for descriptor in descriptors {
        let features = descriptor
            .features
            .iter()
            .map(|feature| encode_hex(feature))
            .collect::<Vec<_>>()
            .join(",");
        output.push_str(&format!(
            "P\t{}\t{}\t{}\t{}\t{}\t{}\n",
            encode_hex(&descriptor.id),
            encode_hex(&descriptor.name),
            encode_optional(&descriptor.vendor),
            encode_optional(&descriptor.version),
            encode_optional(&descriptor.description),
            features,
        ));
    }
    output
}

fn encode_optional(value: &Option<String>) -> String {
    value.as_deref().map(encode_hex).unwrap_or_default()
}

fn encode_hex(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err("invalid odd-length CLAP descriptor field".to_owned());
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded).map_err(|_| "CLAP descriptor field was not UTF-8".to_owned())
}

fn hex_value(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("invalid hexadecimal CLAP descriptor field".to_owned()),
    }
}

fn decode_optional(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        Ok(None)
    } else {
        decode_hex(value).map(Some)
    }
}

fn parse_probe_output(output: &str) -> Result<Vec<ClapPluginDescriptor>, String> {
    let mut lines = output.lines();
    if !lines.any(|line| line == PROBE_HEADER) {
        return Err("CLAP descriptor probe returned an invalid response".to_owned());
    }

    let mut descriptors = Vec::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.first() != Some(&"P") {
            continue;
        }
        if fields.len() != 7 {
            return Err("CLAP descriptor probe returned an incomplete descriptor".to_owned());
        }
        let id = decode_hex(fields[1])?;
        let name = decode_hex(fields[2])?;
        if id.is_empty() || name.is_empty() {
            return Err("CLAP descriptor is missing its required ID or name".to_owned());
        }
        let features = if fields[6].is_empty() {
            Vec::new()
        } else {
            fields[6]
                .split(',')
                .map(decode_hex)
                .collect::<Result<Vec<_>, _>>()?
        };
        descriptors.push(ClapPluginDescriptor {
            id,
            name,
            vendor: decode_optional(fields[3])?,
            version: decode_optional(fields[4])?,
            description: decode_optional(fields[5])?,
            features,
        });
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::{ClapPluginDescriptor, encode_probe_output, parse_probe_output};

    #[test]
    fn descriptor_probe_protocol_round_trips_unicode_and_delimiters() {
        let descriptor = ClapPluginDescriptor {
            id: "org.example.test\tplugin".to_owned(),
            name: "Écho Synth\nMk II".to_owned(),
            vendor: Some("Vendor, Inc.".to_owned()),
            version: Some("1.2.3".to_owned()),
            description: None,
            features: vec!["instrument".to_owned(), "synthesizer".to_owned()],
        };
        let output = format!(
            "plug-in diagnostic output\n{}",
            encode_probe_output(std::slice::from_ref(&descriptor))
        );

        assert_eq!(
            parse_probe_output(&output).expect("probe output should parse"),
            vec![descriptor]
        );
    }
}
