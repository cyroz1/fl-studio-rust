#![allow(unsafe_code)]
//! VST 2 lifecycle calls close a non-waiting audio admission gate. UI and
//! parameter calls may overlap DSP, using copied ABI function pointers rather
//! than Rust references to foreign mutable storage. Callbacks use atomics/TLS.
use crate::{
    editor::EditorWindow,
    editor_scaling,
    library::Library,
    notifications::{EditPhase, Notifications, ParameterEdit, PluginEvents, SuppressNotifications},
    vst2::*,
};
use std::{
    cell::Cell,
    ffi::{CStr, CString, c_void},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use vst_audio_protocol::EditorScaling;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PluginError(pub String);
#[derive(Clone, Debug)]
pub struct Info {
    pub name: String,
    pub vendor: String,
    pub inputs: u16,
    pub outputs: u16,
    pub latency: u32,
    pub input_types: Option<Vec<i32>>,
    pub output_types: Option<Vec<i32>>,
}
#[derive(Debug)]
struct CallbackState {
    flags: AtomicU32,
    rate: AtomicU32,
    frames: AtomicU32,
    size: AtomicU64,
    editor_open: AtomicBool,
    io_pending: AtomicBool,
    invalid: AtomicBool,
    directory: Option<CString>,
    events: crate::vst2_events::CallbackEvents,
    offline: AtomicBool,
    parameter_count: AtomicU32,
    notifications: Notifications,
}
#[derive(Debug)]
struct Shared {
    effect: usize,
    metadata: AEffect,
    admitted: AtomicBool,
    processing: AtomicBool,
    active: AtomicBool,
    closed: AtomicBool,
    callback: Arc<CallbackState>,
    _library: Library,
}
// SAFETY: the module and callback allocation outlive calls. Lifecycle is gated;
// VST2 permits UI/parameter calls alongside DSP. Metadata is an immutable copy,
// and no Rust reference to foreign instance storage crosses a foreign call.
unsafe impl Send for Shared {}
// SAFETY: lifecycle/header inspection is gated; concurrent calls use copied function
// pointers. Shared host state is atomic or immutable, never a mutable Rust reference.
unsafe impl Sync for Shared {}
impl Shared {
    fn control(&self) -> Control<'_> {
        self.admitted.store(false, Ordering::SeqCst);
        while self.processing.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        Control(self, true)
    }
    fn calls(&self) -> Control<'_> {
        Control(self, false)
    }
    fn effect(&self) -> *mut AEffect {
        self.effect as *mut AEffect
    }
}
struct Control<'a>(&'a Shared, bool);
impl Drop for Control<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.admitted.store(true, Ordering::SeqCst);
        }
    }
}
impl Control<'_> {
    fn effect(&self) -> AEffect {
        if self.1 {
            // SAFETY: lifecycle gate excludes DSP; copy without borrowing foreign storage.
            unsafe { self.0.effect().read() }
        } else {
            self.0.metadata
        }
    }
    fn call(
        &self,
        opcode: i32,
        index: i32,
        value: isize,
        pointer: *mut c_void,
        option: f32,
    ) -> isize {
        let Some(dispatch) = self.0.metadata.dispatcher else {
            return 0;
        };
        // SAFETY: validated dispatcher; caller supplies the payload required by this opcode.
        unsafe { dispatch(self.0.effect(), opcode, index, value, pointer, option) }
    }
    fn text(&self, opcode: i32) -> String {
        self.text_at(opcode, 0)
    }
    fn text_at(&self, opcode: i32, index: i32) -> String {
        let mut buffer = [0u8; 256];
        self.call(opcode, index, 0, buffer.as_mut_ptr().cast(), 0.0);
        let length = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..length]).into_owned()
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        // Load-error cleanup: no processor exists and this is the main thread.
        let control = self.control();
        if self.active.swap(false, Ordering::SeqCst) {
            control.call(STOP_PROCESS, 0, 0, std::ptr::null_mut(), 0.0);
            control.call(MAINS_CHANGED, 0, 0, std::ptr::null_mut(), 0.0);
        }
        control.call(CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
    }
}

#[derive(Debug)]
pub struct Vst2Plugin {
    shared: Arc<Shared>,
    info: Info,
    editor: Option<EditorWindow>,
    editor_mode: EditorScaling,
    editor_scaling: EditorScaling,
    editor_scale: f32,
    editor_scale_supported: bool,
    closed: bool,
    resized: Option<(u32, u32)>,
    configured: bool,
    parameter_count: u32,
    processor_taken: bool,
    format: vst_audio_ring::SampleFormat,
    _main: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[derive(Debug)]
pub struct Vst2Processor {
    shared: Arc<Shared>,
    inputs: usize,
    outputs: usize,
    max_frames: usize,
    rate: u32,
    format: vst_audio_ring::SampleFormat,
    parameter_count: u32,
    midi: Box<[crate::vst2_events::MidiEvent; vst_audio_ring::MAX_EVENTS]>,
    sysex: Box<[crate::vst2_events::SysExEvent; vst_audio_ring::MAX_EVENTS]>,
    sysex_bytes: Box<[[u8; vst_audio_ring::MAX_SYSEX_BYTES]; vst_audio_ring::MAX_EVENTS]>,
}
impl Vst2Plugin {
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        Self::load_class(path, None)
    }
    pub fn load_class(path: &Path, class: Option<[u8; 16]>) -> Result<Self, PluginError> {
        let selected = decode_class(class)?;
        let library = Library::load(path).map_err(|e| PluginError(e.to_string()))?;
        let symbol = library
            .symbol(c"VSTPluginMain")
            .or_else(|| library.symbol(c"main"))
            .ok_or_else(|| PluginError("missing VSTPluginMain".into()))?;
        // SAFETY: symbol is the required VST 2 entry point in a disposable host process.
        let entry: Entry = unsafe { std::mem::transmute(symbol) };
        // SAFETY: host callback has the clean-room ABI and static lifetime.
        let previous = CURRENT_ID.replace(selected.unwrap_or(0));
        // SAFETY: entry uses the validated ABI and static callback.
        let effect = unsafe { entry(callback) };
        CURRENT_ID.set(previous);
        if effect.is_null() || !effect.is_aligned() {
            return Err(PluginError("null or misaligned plugin instance".into()));
        }
        // SAFETY: successful entry returns an aligned AEffect that remains live until CLOSE.
        let fields = unsafe { effect.read() };
        let conforming = fields.magic == MAGIC && fields.dispatcher.is_some();
        if !conforming
            || (fields.process_replacing.is_none() && fields.process_double.is_none())
            || !(0..=64).contains(&fields.num_inputs)
            || !(0..=64).contains(&fields.num_outputs)
            || !(0..=16384).contains(&fields.num_params)
        {
            if conforming && let Some(close) = fields.dispatcher {
                // A real instance outside this host's limits is released before
                // its module unloads; a thread it started must not keep running
                // unloaded code.
                // SAFETY: a conforming entry returned a live instance and nothing
                // else was called on it; CLOSE is its final dispatcher call.
                unsafe { close(effect, CLOSE, 0, 0, std::ptr::null_mut(), 0.0) };
            }
            return Err(PluginError("invalid plugin ABI or channel count".into()));
        }
        let directory = plugin_directory(path);
        let mut shared = Arc::new(Shared {
            effect: effect as usize,
            metadata: fields,
            admitted: AtomicBool::new(true),
            processing: AtomicBool::new(false),
            active: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            callback: Arc::new(CallbackState {
                flags: AtomicU32::new(0),
                rate: AtomicU32::new(48000),
                frames: AtomicU32::new(480),
                size: AtomicU64::new(0),
                editor_open: AtomicBool::new(false),
                io_pending: AtomicBool::new(false),
                invalid: AtomicBool::new(false),
                directory,
                events: crate::vst2_events::CallbackEvents::default(),
                offline: AtomicBool::new(false),
                parameter_count: AtomicU32::new(fields.num_params as u32),
                notifications: Notifications::new(),
            }),
            _library: library,
        });
        // SAFETY: before any processing; resvd1 is reserved for the host. Callback state
        // stays at a stable Arc allocation until after effClose and all processors drop.
        unsafe { (*effect).resvd1 = Arc::as_ptr(&shared.callback).cast_mut().cast() };
        let control = shared.control();
        shared
            .callback
            .events
            .context(vst_audio_ring::Transport::default(), 48000, 0);
        control.call(OPEN, 0, 0, std::ptr::null_mut(), 0.0);
        let fields = control.effect();
        if !(0..=64).contains(&fields.num_inputs)
            || !(0..=64).contains(&fields.num_outputs)
            || !(0..=16384).contains(&fields.num_params)
            || selected.is_some_and(|id| id != fields.unique_id)
        {
            return Err(PluginError(
                "invalid post-OPEN counts or shell child ID".into(),
            ));
        }
        let parameter_count = fields.num_params as u32;
        // A VST2 plugin can violate its entry-time parameter declaration at OPEN.
        // Use the validated final count for callbacks too (audit #11 TD-09);
        // parameter_added_at_open_emits_edit_after_start covers fixture mode 22.
        shared
            .callback
            .parameter_count
            .store(parameter_count, Ordering::Release);
        let inputs = fields.num_inputs as u16;
        let outputs = fields.num_outputs as u16;
        let mut input = std::ptr::null_mut::<Arrangement>();
        let mut output = std::ptr::null_mut::<Arrangement>();
        let arrangements = control.call(
            GET_SPEAKER_ARRANGEMENT,
            0,
            (&mut input as *mut *mut Arrangement) as isize,
            (&mut output as *mut *mut Arrangement).cast(),
            0.0,
        );
        let types =
            |pointer: *mut Arrangement, count: u16| -> Result<Option<Vec<i32>>, PluginError> {
                if arrangements == 0 || pointer.is_null() {
                    return Ok(None);
                }
                if !pointer.is_aligned() {
                    return Err(PluginError("misaligned speaker arrangement".into()));
                }
                // SAFETY: successful arrangement query returns its live, aligned header.
                let channels = unsafe { (*pointer).channels };
                // JUCE declares its widest supported channel count in
                // numInputs/numOutputs and reports the current, possibly
                // narrower, layout here. Buffers follow the declared counts,
                // so a different arrangement leaves the speaker types unknown.
                if channels != i32::from(count) {
                    return Ok(None);
                }
                // SAFETY: address of the first speaker only; do not reference a fixed-size
                // struct since third-party plugins may allocate exactly channels entries.
                let first = unsafe { std::ptr::addr_of!((*pointer).speakers).cast::<Speaker>() };
                // SAFETY: count <= 64 was validated after OPEN and equals `channels`,
                // so the plugin's arrangement holds that many initialized, aligned
                // Speaker records in one readable allocation, live through this copy.
                let speakers = unsafe { std::slice::from_raw_parts(first, count as usize) };
                Ok(Some(speakers.iter().map(|speaker| speaker.kind).collect()))
            };
        let info = Info {
            name: control.text(GET_EFFECT_NAME),
            vendor: control.text(GET_VENDOR_STRING),
            inputs,
            outputs,
            latency: control.effect().initial_delay.max(0) as u32,
            input_types: types(input, inputs)?,
            output_types: types(output, outputs)?,
        };
        drop(control);
        Arc::get_mut(&mut shared)
            .expect("no processor yet")
            .metadata = fields;
        shared.callback.io_pending.store(false, Ordering::Release);
        Ok(Self {
            shared,
            info,
            editor: None,
            editor_mode: EditorScaling::Auto,
            editor_scaling: EditorScaling::Off,
            editor_scale: 1.0,
            editor_scale_supported: false,
            closed: false,
            resized: None,
            configured: false,
            parameter_count,
            processor_taken: false,
            format: vst_audio_ring::SampleFormat::F32,
            _main: std::marker::PhantomData,
        })
    }
    pub fn capabilities(&self) -> vst_audio_protocol::Capabilities {
        let control = self.shared.calls();
        vst_audio_protocol::Capabilities {
            f64: control.effect().flags & DOUBLE_REPLACING != 0
                && control.effect().process_double.is_some(),
            input_event_buses: 1,
            output_event_buses: 1,
            editor: control.effect().flags & HAS_EDITOR != 0,
            sysex: true,
            sample_accurate_parameters: true,
        }
    }
    pub fn info(&self) -> Info {
        self.info.clone()
    }
    pub fn configure(
        &mut self,
        rate: u32,
        frames: u32,
        inputs: u16,
        outputs: u16,
    ) -> Result<(), PluginError> {
        self.configure_precision(
            rate,
            frames,
            inputs,
            outputs,
            vst_audio_ring::SampleFormat::F32,
            false,
        )
    }
    pub fn configure_precision(
        &mut self,
        rate: u32,
        frames: u32,
        inputs: u16,
        outputs: u16,
        format: vst_audio_ring::SampleFormat,
        offline: bool,
    ) -> Result<(), PluginError> {
        if self.shared.active.load(Ordering::SeqCst)
            || self.processor_taken
            || rate < 1000
            || frames == 0
            || frames > 65536
            || inputs != self.info.inputs
            || outputs != self.info.outputs
        {
            return Err(PluginError("unsupported configuration".into()));
        }
        let control = self.shared.control();
        if (format == vst_audio_ring::SampleFormat::F64
            && (control.effect().flags & DOUBLE_REPLACING == 0
                || control.effect().process_double.is_none()))
            || (format == vst_audio_ring::SampleFormat::F32
                && (control.effect().flags & REPLACING == 0
                    || control.effect().process_replacing.is_none()))
        {
            return Err(PluginError("sample precision unsupported".into()));
        }
        self.format = format;
        self.shared
            .callback
            .offline
            .store(offline, Ordering::Relaxed);
        control.call(
            77,
            0,
            isize::from(format == vst_audio_ring::SampleFormat::F64),
            std::ptr::null_mut(),
            0.0,
        );
        self.shared.callback.rate.store(rate, Ordering::Relaxed);
        self.shared.callback.frames.store(frames, Ordering::Relaxed);
        self.shared
            .callback
            .events
            .context(vst_audio_ring::Transport::default(), rate, 0);
        control.call(SET_SAMPLE_RATE, 0, 0, std::ptr::null_mut(), rate as f32);
        control.call(
            SET_BLOCK_SIZE,
            0,
            frames as isize,
            std::ptr::null_mut(),
            0.0,
        );
        refresh(&self.shared, &control, &mut self.info, self.parameter_count);
        self.configured = true;
        Ok(())
    }
    pub fn activate(&mut self) {
        let control = self.shared.control();
        if !self.shared.active.swap(true, Ordering::SeqCst) {
            control.call(MAINS_CHANGED, 0, 1, std::ptr::null_mut(), 0.0);
            control.call(START_PROCESS, 0, 0, std::ptr::null_mut(), 0.0);
            refresh(&self.shared, &control, &mut self.info, self.parameter_count);
        }
    }
    pub fn deactivate(&mut self) {
        let control = self.shared.control();
        if self.shared.active.swap(false, Ordering::SeqCst) {
            control.call(STOP_PROCESS, 0, 0, std::ptr::null_mut(), 0.0);
            control.call(MAINS_CHANGED, 0, 0, std::ptr::null_mut(), 0.0);
        }
    }
    pub fn processor(&mut self) -> Result<Vst2Processor, PluginError> {
        if !self.configured || self.processor_taken {
            return Err(PluginError(
                "processor requires a unique configured instance".into(),
            ));
        }
        self.processor_taken = true;
        Ok(Vst2Processor {
            shared: self.shared.clone(),
            inputs: self.info.inputs as usize,
            outputs: self.info.outputs as usize,
            max_frames: self.shared.callback.frames.load(Ordering::Relaxed) as usize,
            rate: self.shared.callback.rate.load(Ordering::Relaxed),
            format: self.format,
            parameter_count: self.parameter_count,
            midi: Box::new([crate::vst2_events::MidiEvent::default(); vst_audio_ring::MAX_EVENTS]),
            sysex: Box::new(
                [crate::vst2_events::SysExEvent::default(); vst_audio_ring::MAX_EVENTS],
            ),
            sysex_bytes: Box::new(
                [[0; vst_audio_ring::MAX_SYSEX_BYTES]; vst_audio_ring::MAX_EVENTS],
            ),
        })
    }
    pub fn parameters(
        &mut self,
        start: u32,
        count: u16,
    ) -> Result<(u32, Vec<vst_audio_protocol::Parameter>), PluginError> {
        if count == 0 || count > 32 || start > self.parameter_count {
            return Err(PluginError("invalid parameter page".into()));
        }
        let control = self.shared.calls();
        let get = control
            .effect()
            .get_parameter
            .ok_or_else(|| PluginError("no parameter getter".into()))?;
        let mut entries = Vec::new();
        for id in start
            ..start
                .saturating_add(u32::from(count))
                .min(self.parameter_count)
        {
            // SAFETY: cached validated parameter index; VST2 permits this query alongside DSP.
            let value = f64::from(unsafe { get(self.shared.effect(), id) });
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(PluginError("invalid parameter value".into()));
            }
            entries.push(vst_audio_protocol::Parameter {
                id,
                name: control.text_at(PARAM_NAME, id as i32),
                unit: control.text_at(6, id as i32),
                value,
                default: None,
                steps: None,
                automatable: control.call(26, id as i32, 0, std::ptr::null_mut(), 0.0) == 1,
                read_only: control.effect().set_parameter.is_none(),
                hidden: false,
            });
        }
        Ok((self.parameter_count, entries))
    }
    pub fn set_parameter(&mut self, id: u32, value: f64) -> Result<(), PluginError> {
        if id >= self.parameter_count || !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(PluginError("invalid parameter edit".into()));
        }
        let control = self.shared.calls();
        let set = control
            .effect()
            .set_parameter
            .ok_or_else(|| PluginError("no parameter setter".into()))?;
        let _notifications = SuppressNotifications::enter();
        // SAFETY: validated index/value; the VST2 plugin synchronizes parameter writes with DSP.
        unsafe { set(self.shared.effect(), id, value as f32) };
        Ok(())
    }
    pub fn get_state(&mut self) -> Result<Vec<u8>, PluginError> {
        let control = self.shared.calls();
        if control.effect().flags & PROGRAM_CHUNKS != 0 {
            let mut data = std::ptr::null::<u8>();
            let size = control.call(GET_CHUNK, 0, 0, (&mut data as *mut *const u8).cast(), 0.0);
            if !(0..64 * 1024 * 1024 - 8).contains(&size) || (size != 0 && data.is_null()) {
                return Err(PluginError("invalid plugin chunk".into()));
            }
            if size == 0 {
                return Ok(Vec::new());
            }
            // SAFETY: GET_CHUNK guarantees size readable bytes until the next dispatcher call.
            return Ok(unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec());
        }
        // Violating VST2 getters must not poison recovery checkpoints (audit #11 TD-36;
        // bad_parameter_getter_does_not_replace_recovery_checkpoint).
        let program = if control.effect().num_programs > 0 {
            let program = control.call(GET_PROGRAM, 0, 0, std::ptr::null_mut(), 0.0);
            if program < 0 || program >= control.effect().num_programs as isize {
                return Err(PluginError("invalid program".into()));
            }
            program as i32
        } else {
            0
        };
        let mut state = program.to_le_bytes().to_vec();
        let count = control.effect().num_params;
        let get = control
            .effect()
            .get_parameter
            .ok_or_else(|| PluginError("missing getParameter".into()))?;
        for index in 0..count {
            // SAFETY: validated parameter index; concurrent parameter reads are a VST2 plugin obligation.
            let value = unsafe { get(self.shared.effect(), index as u32) };
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(PluginError("invalid parameter state value".into()));
            }
            state.extend(value.to_le_bytes());
        }
        Ok(state)
    }
    pub fn set_state(&mut self, blob: &[u8]) -> Result<(), PluginError> {
        if blob.len() >= 64 * 1024 * 1024 - 8 {
            return Err(PluginError("state too large".into()));
        }
        let control = self.shared.control();
        let _notifications = SuppressNotifications::enter();
        if control.effect().flags & PROGRAM_CHUNKS != 0 {
            let mut blob = blob.to_vec();
            control.call(
                SET_CHUNK,
                0,
                blob.len() as isize,
                blob.as_mut_ptr().cast(),
                0.0,
            );
        } else {
            let count = self.parameter_count as usize;
            if blob.len() != 4 + count * 4 {
                return Err(PluginError("invalid parameter state".into()));
            }
            if blob[4..].as_chunks::<4>().0.iter().any(|bytes| {
                let value = f32::from_le_bytes(*bytes);
                !value.is_finite() || !(0.0..=1.0).contains(&value)
            }) {
                return Err(PluginError("invalid parameter state value".into()));
            }
            if control.effect().num_programs > 0 {
                let program = i32::from_le_bytes(blob[..4].try_into().expect("four bytes"));
                if program < 0 || program >= control.effect().num_programs {
                    return Err(PluginError("invalid program".into()));
                }
                control.call(SET_PROGRAM, 0, program as isize, std::ptr::null_mut(), 0.0);
            }
            let set = control
                .effect()
                .set_parameter
                .ok_or_else(|| PluginError("missing setParameter".into()))?;
            for (index, value) in blob[4..].as_chunks::<4>().0.iter().enumerate() {
                let value = f32::from_le_bytes(*value);
                // SAFETY: exact state size checked; index is a valid parameter, control exclusive.
                unsafe { set(self.shared.effect(), index as u32, value) };
            }
        }
        refresh(&self.shared, &control, &mut self.info, self.parameter_count);
        if self.shared.callback.invalid.load(Ordering::Acquire) {
            return Err(PluginError(RESTART_REQUIRED.into()));
        }
        Ok(())
    }
    pub fn open_editor(&mut self, scaling: EditorScaling) -> Result<(), PluginError> {
        self.open_editor_at(None, scaling)
    }
    pub fn open_editor_in(
        &mut self,
        parent: vst_audio_protocol::ParentWindow,
        scaling: EditorScaling,
    ) -> Result<(), PluginError> {
        self.open_editor_at(Some(parent), scaling)
    }
    fn open_editor_at(
        &mut self,
        parent: Option<vst_audio_protocol::ParentWindow>,
        scaling: EditorScaling,
    ) -> Result<(), PluginError> {
        editor_scaling::validate(scaling, parent)?;
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.parent() == parent && self.editor_mode == scaling)
        {
            return Ok(());
        }
        teardown_vst2_editor(
            &self.shared,
            &mut self.editor,
            self.editor_scaling == EditorScaling::Plugin && self.editor_scale != 1.0,
            true,
            false,
        );
        let control = self.shared.calls();
        if control.effect().flags & HAS_EDITOR == 0 {
            return Err(PluginError("plugin has no editor".into()));
        }
        self.editor_mode = scaling;
        self.editor_scale = 1.0;
        self.editor_scale_supported = control.call(
            51,
            0,
            0,
            c"supportsViewDpiScaling".as_ptr().cast_mut().cast(),
            0.0,
        ) == 1;
        let request = !cfg!(target_os = "macos")
            && self.editor_scale_supported
            && editor_scaling::requests_scale(scaling);
        self.editor_scaling = if request {
            EditorScaling::Plugin
        } else {
            editor_scaling::fallback(scaling, parent)
        };
        // Create the actual native parent first so its own display/monitor is
        // authoritative. Scaling must precede even the first effEditGetRect.
        let editor = editor_scaling::open(&self.info.name, parent, 640, 480, self.editor_scaling)?;
        let _dpi = editor_scaling::scope(&editor);
        if request {
            self.editor_scale = editor_scaling::platform_scale(&editor);
            set_vst2_scale(&control, self.editor_scale);
        }
        // effEditGetRect writes a pointer to the plugin's own rectangle, or
        // leaves the out-pointer alone (some plugins return null instead).
        let fallback = Rect {
            top: 0,
            left: 0,
            bottom: 480,
            right: 640,
        };
        let mut rect: *const Rect = &fallback;
        control.call(
            EDIT_GET_RECT,
            0,
            0,
            (&mut rect as *mut *const Rect).cast(),
            0.0,
        );
        if !rect.is_aligned() {
            return Err(PluginError("misaligned editor rectangle".into()));
        }
        // SAFETY: `rect` is still `&fallback`, or null, or the plugin's aligned
        // rectangle, which stays valid until the next dispatcher call.
        let rect = unsafe { rect.as_ref() }.copied().unwrap_or(fallback);
        let (width, height) = (
            i32::from(rect.right) - i32::from(rect.left),
            i32::from(rect.bottom) - i32::from(rect.top),
        );
        editor
            .resize(width, height)
            .map_err(|e| PluginError(e.to_string()))?;
        self.shared
            .callback
            .editor_open
            .store(true, Ordering::Release);
        let opened = control.call(EDIT_OPEN, 0, 0, editor.handle_value() as *mut c_void, 0.0);
        // JUCE 6.0.0 to 6.1.2 drop a scale that arrives before the editor
        // exists. Repeat it; the rectangle read below picks up the new size.
        if request && self.editor_scale != 1.0 {
            set_vst2_scale(&control, self.editor_scale);
        }
        let mut after = std::ptr::null::<Rect>();
        control.call(
            EDIT_GET_RECT,
            0,
            0,
            (&mut after as *mut *const Rect).cast(),
            0.0,
        );
        if !after.is_null() && after.is_aligned() {
            // SAFETY: plugin owns the aligned rectangle until its next dispatcher call; copy now.
            let after = unsafe { after.read() };
            let w = i32::from(after.right) - i32::from(after.left);
            let h = i32::from(after.bottom) - i32::from(after.top);
            if w > 0
                && h > 0
                && (w, h) != (width, height)
                && let Err(error) = editor.resize(w, h)
            {
                eprintln!("VST2 post-open resize failed: {error}");
            }
        }
        if opened == 0 && !crate::editor_probe::has_child(editor.handle_value()) {
            control.call(EDIT_CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
            self.shared
                .callback
                .editor_open
                .store(false, Ordering::Release);
            self.shared.callback.size.store(0, Ordering::Release);
            return Err(PluginError("plugin did not create an editor".into()));
        }
        self.editor = Some(editor);
        Ok(())
    }
    pub fn editor_action(
        &mut self,
        action: vst_audio_protocol::EditorAction,
    ) -> Result<vst_audio_protocol::EditorReply, PluginError> {
        use vst_audio_protocol::{EditorAction as A, EditorReply, EditorStatus};
        #[cfg(windows)]
        if let A::Scale(scale) = action
            && self.editor_scaling == EditorScaling::Stretch
            && self.editor.is_some()
            && self.editor_scale_supported
            && scale.is_finite()
            && (0.5..=4.0).contains(&scale)
        {
            let visible = self
                .editor
                .as_ref()
                .and_then(|w| w.status().ok())
                .is_some_and(|s| s.0);
            self.open_editor_at(None, EditorScaling::Plugin)?;
            if !visible && let Some(window) = &self.editor {
                window.show(false).map_err(|e| PluginError(e.to_string()))?;
            }
        }
        let window = self
            .editor
            .as_ref()
            .ok_or_else(|| PluginError("editor is not open".into()))?;
        match action {
            A::Show => window.show(true).map_err(|e| PluginError(e.to_string()))?,
            A::Hide => window.show(false).map_err(|e| PluginError(e.to_string()))?,
            A::Focus => window.focus().map_err(|e| PluginError(e.to_string()))?,
            A::Inspect => {}
            A::Scale(scale) => {
                if !scale.is_finite() || !(0.5..=4.0).contains(&scale) {
                    return Err(PluginError("invalid content scale".into()));
                }
                if !self.editor_scale_supported || cfg!(target_os = "macos") {
                    return Err(PluginError("editor scaling unsupported".into()));
                }
                let _dpi = editor_scaling::scope(window);
                let control = self.shared.calls();
                set_vst2_scale(&control, scale);
                resize_vst2(&control, window)?;
                self.editor_scale = scale;
                self.editor_mode = EditorScaling::Plugin;
                self.editor_scaling = EditorScaling::Plugin;
                if let Ok((_, width, height)) = window.status() {
                    self.resized = Some((width, height));
                }
            }
            A::Resize { .. } => {
                return Err(PluginError(
                    "VST2 has no standard host-driven resize or scale contract".into(),
                ));
            }
            A::Capture => {
                return window
                    .capture()
                    .map(EditorReply::Image)
                    .map_err(|e| PluginError(e.to_string()));
            }
        }
        let (visible, width, height) = window.status().map_err(|e| PluginError(e.to_string()))?;
        Ok(EditorReply::Status(EditorStatus {
            #[cfg(target_os = "linux")]
            platform: window.platform(),
            #[cfg(not(target_os = "linux"))]
            platform: crate::native_editor_platform(),
            visible,
            embedded: window.parent().is_some(),
            width,
            height,
            scale: self.editor_scale,
            scaling: self.editor_scaling,
            resize_supported: false,
            // macOS leaves Retina scaling to the system and refuses `Scale`.
            scale_supported: self.editor_scale_supported && !cfg!(target_os = "macos"),
        }))
    }
    pub fn request_editor_close(&mut self) -> Result<(), PluginError> {
        if self.editor.is_some() {
            teardown_vst2_editor(
                &self.shared,
                &mut self.editor,
                self.editor_scaling == EditorScaling::Plugin && self.editor_scale != 1.0,
                true,
                false,
            );
            self.closed = true;
        }
        Ok(())
    }
    pub fn idle(&mut self) {
        if let Some(editor) = &self.editor {
            let _dpi = editor_scaling::scope(editor);
            let control = self.shared.calls();
            control.call(EDIT_IDLE, 0, 0, std::ptr::null_mut(), 0.0);
            let size = self.shared.callback.size.swap(0, Ordering::AcqRel);
            if size != 0 {
                let (width, height) = ((size >> 32) as u32, size as u32);
                if let Err(error) = editor.resize(width as i32, height as i32) {
                    eprintln!("VST2 sizeWindow failed: {error}");
                }
                if let Ok((_, width, height)) = editor.status() {
                    self.resized = Some((width, height));
                }
            }
            let closed = editor.pump();
            #[cfg(windows)]
            if !closed && let Some(change) = editor.take_dpi_change() {
                let _dpi = editor.dpi_scope();
                if self.editor_scaling == EditorScaling::Plugin {
                    set_vst2_scale(&control, change.scale);
                    self.editor_scale = change.scale;
                    if let Err(error) = resize_vst2(&control, editor) {
                        eprintln!("VST2 DPI resize failed: {error}");
                    }
                } else if let Err(error) = editor.apply_dpi_rectangle(change) {
                    eprintln!("VST2 DPI rectangle failed: {error}");
                }
                if let Ok((_, width, height)) = editor.status() {
                    self.resized = Some((width, height));
                }
            }
            if closed {
                teardown_vst2_editor(
                    &self.shared,
                    &mut self.editor,
                    self.editor_scaling == EditorScaling::Plugin && self.editor_scale != 1.0,
                    false,
                    false,
                );
                self.closed = true;
            }
        }
    }
    pub fn take_events(&mut self) -> PluginEvents {
        if self.shared.callback.io_pending.load(Ordering::Acquire) {
            let control = self.shared.control();
            refresh(&self.shared, &control, &mut self.info, self.parameter_count);
        }
        let flags = self.shared.callback.flags.swap(0, Ordering::AcqRel);
        PluginEvents {
            params_changed: flags & 1 != 0 || self.shared.callback.notifications.take_overflow(),
            parameter_edits: self.shared.callback.notifications.drain(),
            latency_changed: (flags & 2 != 0).then_some(self.info.latency),
            editor_closed: std::mem::take(&mut self.closed),
            editor_resized: self.resized.take(),
            fault: self
                .shared
                .callback
                .invalid
                .load(Ordering::Acquire)
                .then_some(RESTART_REQUIRED.into()),
        }
    }
}
fn teardown_vst2_editor(
    shared: &Shared,
    editor: &mut Option<EditorWindow>,
    reset_scale: bool,
    enter_dpi_scope: bool,
    exclusive: bool,
) {
    shared.callback.editor_open.store(false, Ordering::Release);
    shared.callback.size.store(0, Ordering::Release);
    if editor.is_some() {
        // Drop excludes DSP; ordinary editor calls retain audio admission.
        let control = if exclusive {
            shared.control()
        } else {
            shared.calls()
        };
        // Idle already holds its scope; Drop intentionally enters none.
        let _dpi = if enter_dpi_scope {
            editor.as_ref().map(editor_scaling::scope)
        } else {
            None
        };
        // Some VST2 adapters retain scale across EDIT_CLOSE. Reset only the
        // old Plugin session; Drop and Off/Stretch sessions send nothing.
        if reset_scale {
            set_vst2_scale(&control, 1.0);
        }
        control.call(EDIT_CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
        // The plugin view must close before its native parent is destroyed.
        *editor = None;
    }
}
// DPF b801214e distrho/src/DistrhoPluginVST2.cpp:948 and
// DistrhoUtils.hpp:83: d_cconst packs a<<24 | b<<16 | c<<8 | d.
// The vendor dispatch has no acceptance result (DPF returns zero).
fn set_vst2_scale(control: &Control<'_>, scale: f32) {
    control.call(50, 0x5072_6553, 0x4165_4373, std::ptr::null_mut(), scale);
}
fn resize_vst2(control: &Control<'_>, window: &EditorWindow) -> Result<(), PluginError> {
    let mut rect = std::ptr::null::<Rect>();
    control.call(
        EDIT_GET_RECT,
        0,
        0,
        (&mut rect as *mut *const Rect).cast(),
        0.0,
    );
    if rect.is_null() {
        return Err(PluginError("plugin returned no editor rectangle".into()));
    }
    if !rect.is_aligned() {
        return Err(PluginError("misaligned editor rectangle".into()));
    }
    // SAFETY: plugin-owned aligned rectangle is valid until the next foreign dispatcher call.
    let rect = unsafe { rect.read() };
    let width = i32::from(rect.right) - i32::from(rect.left);
    let height = i32::from(rect.bottom) - i32::from(rect.top);
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return Err(PluginError("invalid editor rectangle".into()));
    }
    window
        .resize(width, height)
        .map_err(|e| PluginError(e.to_string()))
}
impl Drop for Vst2Plugin {
    fn drop(&mut self) {
        teardown_vst2_editor(&self.shared, &mut self.editor, false, false, true);
        self.deactivate();
        let control = self.shared.control();
        control.call(CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
        self.shared.closed.store(true, Ordering::SeqCst);
    }
}
// SAFETY: unique audio owner; pointers in preallocated SysEx records target its
// owned boxed scratch and are used only during an admitted audio call.
unsafe impl Send for Vst2Processor {}
impl Vst2Processor {
    pub fn process(&mut self, planes: &mut [&mut [f32]], frames: usize) -> bool {
        self.process_block(
            planes,
            frames,
            &vst_audio_ring::Transport::default(),
            &[],
            &mut vst_audio_ring::EventBuffer::default(),
        )
    }
    pub fn process_block(
        &mut self,
        planes: &mut [&mut [f32]],
        frames: usize,
        context: &vst_audio_ring::Transport,
        input: &[vst_audio_ring::Event],
        output: &mut vst_audio_ring::EventBuffer,
    ) -> bool {
        self.process_inner(planes, frames, context, input, output)
    }
    pub fn process_block_f64(
        &mut self,
        planes: &mut [&mut [f64]],
        frames: usize,
        context: &vst_audio_ring::Transport,
        input: &[vst_audio_ring::Event],
        output: &mut vst_audio_ring::EventBuffer,
    ) -> bool {
        self.process_inner(planes, frames, context, input, output)
    }
    fn process_inner<T: crate::vst2_events::Sample>(
        &mut self,
        planes: &mut [&mut [T]],
        frames: usize,
        context: &vst_audio_ring::Transport,
        input: &[vst_audio_ring::Event],
        output: &mut vst_audio_ring::EventBuffer,
    ) -> bool {
        use vst_audio_ring::{EventData, MAX_EVENTS, SampleFormat};
        output.clear();
        if T::DOUBLE != (self.format == SampleFormat::F64)
            || frames == 0
            || frames > self.max_frames
            || planes.len() < self.inputs.max(self.outputs)
            || planes.iter().any(|p| p.len() < frames)
            || !context.valid()
            || input.len() > MAX_EVENTS
            || input.iter().any(|e| !e.valid(frames as u32) || e.bus != 0)
            || input
                .windows(2)
                .any(|p| p[0].sample_offset > p[1].sample_offset)
            || self.shared.closed.load(Ordering::SeqCst)
            || self.shared.callback.io_pending.load(Ordering::Acquire)
            || self.shared.callback.invalid.load(Ordering::Acquire)
            || !self.shared.admitted.load(Ordering::SeqCst)
            || self.shared.processing.swap(true, Ordering::SeqCst)
        {
            return false;
        }
        let _thread = ProcessingThread::enter(self.shared.callback.offline.load(Ordering::Relaxed));
        let result = (|| {
            if !self.shared.admitted.load(Ordering::SeqCst)
                || !self.shared.active.load(Ordering::SeqCst)
                || self.shared.closed.load(Ordering::SeqCst)
            {
                return false;
            }
            // SAFETY: admission retains the live plugin and excludes lifecycle/header mutation.
            let dispatch =
                unsafe { (*self.shared.effect()).dispatcher }.expect("validated dispatcher");
            // SAFETY: same admitted instance; the optional setter is copied before calls.
            let set = unsafe { (*self.shared.effect()).set_parameter };
            let mut base_context = *context;
            let mut context_origin = 0;
            let mut start = 0;
            while start < frames {
                if self.shared.callback.io_pending.load(Ordering::Acquire)
                    || self.shared.callback.invalid.load(Ordering::Acquire)
                {
                    return false;
                }
                for event in input.iter().filter(|e| e.sample_offset as usize == start) {
                    match event.data {
                        EventData::Parameter { id, value } => {
                            if id >= self.parameter_count || set.is_none() {
                                output.dropped = output.dropped.saturating_add(1);
                                continue;
                            }
                            let set = set.expect("validated setter");
                            let _notifications = SuppressNotifications::enter();
                            // SAFETY: admitted instance, validated parameter index and normalized value.
                            unsafe { set(self.shared.effect(), id, value as f32) };
                        }
                        EventData::Transport(value) => {
                            base_context = value;
                            context_origin = start;
                        }
                        _ => {}
                    }
                }
                let end = input
                    .iter()
                    .filter(|e| {
                        e.sample_offset as usize > start
                            && matches!(
                                e.data,
                                EventData::Parameter { .. } | EventData::Transport(_)
                            )
                    })
                    .map(|e| e.sample_offset as usize)
                    .next()
                    .unwrap_or(frames);
                self.shared.callback.events.context(
                    base_context.advanced(start - context_origin, self.rate),
                    self.rate,
                    start,
                );
                let mut list = crate::vst2_events::Events::<MAX_EVENTS> {
                    count: 0,
                    reserved: 0,
                    events: [std::ptr::null(); MAX_EVENTS],
                };
                for event in input
                    .iter()
                    .filter(|e| (start..end).contains(&(e.sample_offset as usize)))
                {
                    let i = list.count as usize;
                    match &event.data {
                        EventData::Midi([a, b, c]) => {
                            self.midi[i] = crate::vst2_events::MidiEvent {
                                kind: 1,
                                size: size_of::<crate::vst2_events::MidiEvent>() as i32,
                                offset: event.sample_offset as i32 - start as i32,
                                data: [*a, *b, *c, 0],
                                ..Default::default()
                            };
                            list.events[i] =
                                (&self.midi[i] as *const crate::vst2_events::MidiEvent).cast();
                            list.count += 1;
                        }
                        EventData::SysEx { len, bytes } => {
                            self.sysex_bytes[i].copy_from_slice(bytes);
                            self.sysex[i] = crate::vst2_events::SysExEvent {
                                offset: event.sample_offset as i32 - start as i32,
                                dump_bytes: i32::from(*len),
                                data: self.sysex_bytes[i].as_mut_ptr(),
                                ..Default::default()
                            };
                            list.events[i] =
                                (&self.sysex[i] as *const crate::vst2_events::SysExEvent).cast();
                            list.count += 1;
                        }
                        _ => {}
                    }
                }
                if list.count != 0 {
                    // SAFETY: bounded list points into owned scratch retained through this processing segment.
                    unsafe {
                        dispatch(
                            self.shared.effect(),
                            25,
                            0,
                            0,
                            (&mut list as *mut crate::vst2_events::Events<MAX_EVENTS>).cast(),
                            0.0,
                        )
                    };
                }
                let mut inputs = [std::ptr::null(); 64];
                let mut outputs = [std::ptr::null_mut(); 64];
                // One pointer per plane serves both arrays. Deriving the input
                // pointer from a shared borrow and then reborrowing the same plane
                // mutably would invalidate it before the plugin reads through it.
                for (channel, plane) in planes
                    .iter_mut()
                    .enumerate()
                    .take(self.inputs.max(self.outputs))
                {
                    let pointer = plane.as_mut_ptr().wrapping_add(start);
                    if channel < self.inputs {
                        inputs[channel] = pointer.cast_const();
                    }
                    if channel < self.outputs {
                        outputs[channel] = pointer;
                    }
                }
                if self.shared.callback.io_pending.load(Ordering::Acquire) {
                    return false;
                }
                // SAFETY: precision was configured, pointers cover every declared channel and the bounded segment.
                unsafe {
                    T::invoke(
                        self.shared.effect(),
                        inputs.as_ptr(),
                        outputs.as_mut_ptr(),
                        (end - start) as i32,
                    )
                };
                self.shared.callback.events.drain(output, frames);
                start = end;
            }
            true
        })();
        self.shared.processing.store(false, Ordering::SeqCst);
        result
    }
}

unsafe extern "C" fn callback(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    pointer: *mut c_void,
    option: f32,
) -> isize {
    if opcode == 2 {
        return CURRENT_ID.get() as isize;
    }
    if opcode == 23 {
        return PROCESS_LEVEL.get();
    }
    if opcode == 34 {
        return host_version();
    }
    if opcode == 37 {
        // SAFETY: canDo supplies a readable NUL-terminated capability string.
        return unsafe { host_can_do(pointer) };
    }
    if opcode == MASTER_VERSION {
        return 2400;
    }
    if opcode == MASTER_VENDOR || opcode == MASTER_PRODUCT {
        if pointer.is_null() {
            return 0;
        }
        let text = b"Plugin host\0";
        // SAFETY: callback opcode supplies a writable host-name buffer of at least 64 bytes.
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), pointer.cast(), text.len()) };
        return 1;
    }
    if effect.is_null() {
        return 0;
    }
    // SAFETY: callback's effect belongs to the plugin; resvd1 is host-owned storage.
    let state = unsafe { (*effect).resvd1.cast::<CallbackState>() };
    if state.is_null() {
        return 0;
    }
    // SAFETY: host installed a stable CallbackState before effOpen and retains it through effClose.
    let state = unsafe { &*state };
    match opcode {
        7 => state.events.time_pointer(),
        8 => {
            // SAFETY: host processEvents callback supplies a live VST2 event list.
            unsafe { state.events.receive(pointer) }
        }
        23 => PROCESS_LEVEL.get(),
        41 => state
            .directory
            .as_ref()
            .map_or(0, |directory| directory.as_ptr() as isize),
        42 => {
            state.flags.fetch_or(1, Ordering::Release);
            1
        }
        43 | 44 => {
            notify_parameter_edit(
                state,
                index,
                if opcode == 43 {
                    EditPhase::Begin
                } else {
                    EditPhase::End
                },
                0.0,
            );
            1
        }
        MASTER_SAMPLE_RATE => state.rate.load(Ordering::Relaxed) as isize,
        MASTER_BLOCK_SIZE => state.frames.load(Ordering::Relaxed) as isize,
        MASTER_AUTOMATE => {
            notify_parameter_edit(state, index, EditPhase::Perform, f64::from(option));
            1
        }
        MASTER_IO_CHANGED => {
            state.io_pending.store(true, Ordering::Release);
            1
        }
        MASTER_SIZE_WINDOW
            if state.editor_open.load(Ordering::Acquire)
                && index > 0
                && value > 0
                && index <= 8192
                && value <= 8192 =>
        {
            state
                .size
                .store(((index as u64) << 32) | value as u64, Ordering::Release);
            1
        }
        _ => 0,
    }
}

fn notify_parameter_edit(state: &CallbackState, index: i32, phase: EditPhase, value: f64) {
    if index < 0
        || index as u32 >= state.parameter_count.load(Ordering::Relaxed)
        || !value.is_finite()
        || !(0.0..=1.0).contains(&value)
    {
        state.flags.fetch_or(1, Ordering::Release);
        return;
    }
    state.notifications.push(ParameterEdit {
        id: index as u32,
        phase,
        value,
    });
}

const RESTART_REQUIRED: &str = "plugin changed its channel or parameter count; restart required";
thread_local! {
    static CURRENT_ID: Cell<i32> = const { Cell::new(0) };
    static PROCESS_LEVEL: Cell<isize> = const { Cell::new(1) };
}
struct ProcessingThread(isize);
impl ProcessingThread {
    fn enter(offline: bool) -> Self {
        Self(PROCESS_LEVEL.replace(if offline { 4 } else { 2 }))
    }
}
impl Drop for ProcessingThread {
    fn drop(&mut self) {
        PROCESS_LEVEL.set(self.0);
    }
}
fn host_version() -> isize {
    let part = |s: &str| s.parse::<isize>().unwrap_or(0);
    part(env!("CARGO_PKG_VERSION_MAJOR")) * 10000
        + part(env!("CARGO_PKG_VERSION_MINOR")) * 100
        + part(env!("CARGO_PKG_VERSION_PATCH"))
}
unsafe fn host_can_do(pointer: *mut c_void) -> isize {
    if pointer.is_null() {
        return 0;
    }
    let mut bytes = [0u8; 128];
    for (i, byte) in bytes.iter_mut().enumerate() {
        // SAFETY: caller supplies a C string; stop at its terminator, bounded to 128 bytes.
        *byte = unsafe { *pointer.cast::<u8>().wrapping_add(i) };
        if *byte == 0 {
            return isize::from(matches!(
                &bytes[..i],
                b"sendVstEvents"
                    | b"sendVstMidiEvent"
                    | b"receiveVstEvents"
                    | b"receiveVstMidiEvent"
                    | b"sendVstTimeInfo"
                    | b"sizeWindow"
                    | b"startStopProcess"
                    | b"shellCategory"
            ));
        }
    }
    0
}
fn refresh(shared: &Shared, control: &Control<'_>, info: &mut Info, params: u32) {
    shared.callback.io_pending.store(false, Ordering::Release);
    let fields = control.effect();
    if fields.num_inputs != i32::from(info.inputs)
        || fields.num_outputs != i32::from(info.outputs)
        || fields.num_params != params as i32
    {
        shared.callback.invalid.store(true, Ordering::Release);
    }
    let latency = fields.initial_delay.max(0) as u32;
    if latency != info.latency {
        info.latency = latency;
        shared.callback.flags.fetch_or(2, Ordering::Release);
    }
}
fn decode_class(class: Option<[u8; 16]>) -> Result<Option<i32>, PluginError> {
    match class {
        None => Ok(None),
        Some(id) if id == [0; 16] => Ok(None),
        Some(id) if &id[..4] == b"VST2" && id[8..] == [0; 8] => {
            let value = i32::from_be_bytes(id[4..8].try_into().expect("four bytes"));
            if value == 0 {
                Err(PluginError("invalid VST2 shell ID".into()))
            } else {
                Ok(Some(value))
            }
        }
        _ => Err(PluginError("invalid VST2 class ID".into())),
    }
}

fn encode_class(id: i32) -> [u8; 16] {
    let mut class = [0; 16];
    class[..4].copy_from_slice(b"VST2");
    class[4..8].copy_from_slice(&id.to_be_bytes());
    class
}

/// The plugin's directory for the host directory query, or none when it cannot be
/// expressed as the platform's narrow string. Never fails a load. macOS hosts
/// traditionally answered with a legacy file-system record, so none is given.
fn plugin_directory(path: &Path) -> Option<CString> {
    let parent = path.parent()?;
    #[cfg(target_os = "macos")]
    {
        let _ = parent;
        None
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(parent.as_os_str().as_bytes()).ok()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Globalization::{CP_ACP, WC_NO_BEST_FIT_CHARS, WideCharToMultiByte};
        let wide: Vec<u16> = parent.as_os_str().encode_wide().collect();
        let mut used = windows::core::BOOL(0);
        // SAFETY: valid UTF-16 input, query-only call with no output buffer.
        let size = unsafe {
            WideCharToMultiByte(
                CP_ACP,
                WC_NO_BEST_FIT_CHARS,
                &wide,
                None,
                None,
                Some(&mut used),
            )
        };
        if size <= 0 {
            return None;
        }
        let mut bytes = vec![0; size as usize];
        // SAFETY: output is sized by the identical conversion query; pointers live through call.
        let written = unsafe {
            WideCharToMultiByte(
                CP_ACP,
                WC_NO_BEST_FIT_CHARS,
                &wide,
                Some(&mut bytes),
                None,
                Some(&mut used),
            )
        };
        (written == size && !used.as_bool())
            .then(|| CString::new(bytes).ok())
            .flatten()
    }
}
impl Vst2Plugin {
    /// Tri-state plugin canDo result, retaining unknown (0) and no (-1).
    fn can_do(&self, name: &CStr) -> isize {
        self.shared
            .calls()
            .call(51, 0, 0, name.as_ptr().cast_mut().cast(), 0.0)
            .signum()
    }
    pub fn classes(&self) -> Result<Vec<vst_audio_protocol::ClassInfo>, PluginError> {
        let calls = self.shared.calls();
        let category = calls.call(35, 0, 0, std::ptr::null_mut(), 0.0);
        let _shell_capability = self.can_do(c"shellCategory");
        if category != 10 {
            return Ok(vec![vst_audio_protocol::ClassInfo {
                id: {
                    let unique_id = calls.effect().unique_id;
                    if unique_id == 0 {
                        [0; 16]
                    } else {
                        encode_class(unique_id)
                    }
                },
                name: self.info.name.clone(),
                vendor: self.info.vendor.clone(),
                category: if category == 2 || calls.effect().flags & (1 << 8) != 0 {
                    "Instrument"
                } else {
                    "Audio Module"
                }
                .into(),
            }]);
        }
        let mut entries = Vec::new();
        for index in 0..=1024 {
            let mut name = [0u8; 256];
            let child = calls.call(70, 0, 0, name.as_mut_ptr().cast(), 0.0) as i32;
            if child == 0 {
                return Ok(entries);
            }
            if index == 1024 {
                return Err(PluginError(
                    "VST2 shell enumeration exceeded 1024 entries".into(),
                ));
            }
            let id = encode_class(child);
            if entries
                .iter()
                .any(|e: &vst_audio_protocol::ClassInfo| e.id == id)
            {
                return Err(PluginError("duplicate VST2 shell child ID".into()));
            }
            let len = name.iter().position(|b| *b == 0).unwrap_or(name.len());
            entries.push(vst_audio_protocol::ClassInfo {
                id,
                name: String::from_utf8_lossy(&name[..len]).into_owned(),
                vendor: self.info.vendor.clone(),
                category: "Audio Module".into(),
            });
        }
        unreachable!("shell enumeration loop returns at its bound")
    }
    pub fn negotiate_layouts(&mut self, input: u64, output: u64) -> Result<(), PluginError> {
        fn arrangement(mask: u64, count: u16) -> Result<Arrangement, PluginError> {
            let (kind, types): (i32, &[i32]) = match (mask, count) {
                (0, 0) => (-1, &[]),
                (0 | 4, 1) => (0, &[0]),
                (0 | 3, 2) => (ARR_STEREO, &[1, 2]),
                (0 | 63, 6) => (ARR_51, &[1, 2, 3, 4, 5, 6]),
                _ => {
                    return Err(PluginError(
                        "VST 2 speaker arrangement negotiation is unavailable for this layout"
                            .into(),
                    ));
                }
            };
            Ok(Arrangement::new(kind, types))
        }
        // An unspecified layout retains the declaration, including uncommon channel counts.
        if input == 0 && output == 0 {
            return Ok(());
        }
        let mut input = arrangement(input, self.info.inputs)?;
        let mut output = arrangement(output, self.info.outputs)?;
        let control = self.shared.control();
        if control.call(
            SET_SPEAKER_ARRANGEMENT,
            0,
            (&mut input as *mut Arrangement) as isize,
            (&mut output as *mut Arrangement).cast(),
            0.0,
        ) != 1
        {
            return Err(PluginError(
                "VST2 plugin rejected speaker arrangement".into(),
            ));
        }
        refresh(&self.shared, &control, &mut self.info, self.parameter_count);
        Ok(())
    }
}
