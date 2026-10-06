//! Runtime hosting for already-installed VST3 plug-ins.
//!
//! FLP wrapper/state events stay in the project parser as opaque bytes until their
//! Image-Line wrapper format is decoded. The API here accepts either state snapshots
//! written by `vst3-host` or a raw VST3 component state supplied by a caller that has
//! decoded the FLP wrapper.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vst3_host::{Plugin, PluginInfo, PluginWindow, Vst3Host};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedPluginInfo {
    pub id: u64,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub category: String,
    pub uid: String,
    pub path: PathBuf,
    pub has_editor: bool,
}

struct HostedPlugin {
    info: HostedPluginInfo,
    plugin: Arc<Mutex<Plugin>>,
    editor: Option<PluginWindow>,
}

/// Owns active VST3 instances and their native editor windows.
///
/// Plug-ins are loaded on demand. The host does not scan or execute every installed
/// plug-in during startup.
pub struct Vst3HostRuntime {
    host: Vst3Host,
    loaded: Vec<HostedPlugin>,
    next_id: u64,
}

impl Vst3HostRuntime {
    pub fn new(sample_rate: f64, block_size: usize) -> Result<Self, String> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err("sample rate must be finite and positive".to_owned());
        }
        if block_size == 0 {
            return Err("block size must be greater than zero".to_owned());
        }
        let host = Vst3Host::builder()
            .sample_rate(sample_rate)
            .block_size(block_size)
            .with_process_isolation(false)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            host,
            loaded: Vec::new(),
            next_id: 1,
        })
    }

    /// Load one audio class from an installed VST3 bundle.
    ///
    /// `class_id` may be omitted for single-class bundles. Multi-class bundles should
    /// pass the 32-character class UID stored in the project's wrapper metadata.
    pub fn load(
        &mut self,
        path: impl AsRef<Path>,
        class_id: Option<&str>,
    ) -> Result<HostedPluginInfo, String> {
        let plugin = match class_id {
            Some(class_id) => self.host.load_plugin_class(path, class_id),
            None => self.host.load_plugin(path),
        }
        .map_err(|error| error.to_string())?;

        let plugin_info = plugin.info().clone();
        let info = hosted_info(self.next_id, &plugin_info, plugin.has_editor());
        self.next_id = self.next_id.saturating_add(1);
        self.loaded.push(HostedPlugin {
            info: info.clone(),
            plugin: Arc::new(Mutex::new(plugin)),
            editor: None,
        });
        Ok(info)
    }

    /// Load a project/session state blob after the FLP wrapper has been decoded.
    pub fn restore_state(&mut self, id: u64, state: &[u8]) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .load_state(state)
            .map_err(|error| error.to_string())
    }

    /// Return the opaque VST3 host snapshot for an instance.
    ///
    /// This is not written directly into an FLP `0xD5` event; Image-Line's wrapper
    /// encoding must be applied first.
    pub fn save_state(&self, id: u64) -> Result<Vec<u8>, String> {
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .save_state()
            .map_err(|error| error.to_string())
    }

    pub fn open_editor(&mut self, id: u64) -> Result<(), String> {
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if !loaded.info.has_editor {
            return Err(format!("{} does not provide a custom editor", loaded.info.name));
        }
        if loaded.editor.is_none() {
            loaded.editor = Some(PluginWindow::new(loaded.plugin.clone()));
        }
        let editor = loaded.editor.as_mut().expect("editor was just created");
        if !editor.is_open() {
            editor.open().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn close_editor(&mut self, id: u64) -> Result<(), String> {
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if let Some(mut editor) = loaded.editor.take() {
            editor.close();
        }
        Ok(())
    }

    pub fn set_parameter(&self, id: u64, parameter_id: u32, value: f64) -> Result<(), String> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err("VST3 parameter values must be finite and normalized to 0..1".to_owned());
        }
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .set_parameter(parameter_id, value)
            .map_err(|error| error.to_string())
    }

    pub fn parameter_snapshot(
        &self,
        id: u64,
    ) -> Result<Vec<vst3_host::parameters::Parameter>, String> {
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .get_parameters()
            .map_err(|error| error.to_string())
    }

    /// Service native editor close/resize requests and the VST3 UI run loop where needed.
    pub fn service_editors(&mut self) -> Result<(), String> {
        for loaded in &mut self.loaded {
            if let Some(editor) = loaded.editor.as_ref() {
                editor
                    .service_platform_events()
                    .map_err(|error| error.to_string())?;
            }
            loaded
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?
                .service_run_loop();
        }
        Ok(())
    }

    pub fn loaded_plugins(&self) -> Vec<HostedPluginInfo> {
        self.loaded
            .iter()
            .map(|loaded| loaded.info.clone())
            .collect()
    }

    fn plugin(&self, id: u64) -> Result<&Arc<Mutex<Plugin>>, String> {
        self.loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .map(|loaded| &loaded.plugin)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))
    }
}

fn hosted_info(id: u64, info: &PluginInfo, has_editor: bool) -> HostedPluginInfo {
    HostedPluginInfo {
        id,
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        category: info.category.clone(),
        uid: info.uid.clone(),
        path: info.path.clone(),
        has_editor,
    }
}
