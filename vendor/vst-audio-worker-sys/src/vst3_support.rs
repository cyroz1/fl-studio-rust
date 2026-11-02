#![allow(unsafe_code, non_snake_case)]
//! Host-owned COM objects. Controller callbacks are UI-thread-only (SDK threading model).
use crate::{
    editor::EditorWindow,
    notifications::{EditPhase, Notifications, ParameterEdit},
    state_buffer::StateBuffer,
};
use std::{
    collections::{HashMap, HashSet},
    ffi::{c_char, c_void},
    io::{Seek, SeekFrom, Write},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};
use vst3::{Class, ComRef, ComWrapper, Steinberg::Vst::*, Steinberg::*};

#[cfg(target_os = "linux")]
use vst3::{wl_display, wl_surface, xdg_surface, xdg_toplevel};

pub(crate) const CAPACITY: usize = 256;
const DEFERRED_EDIT_LIMIT: usize = 16_384;

pub(crate) struct Edits {
    slots: [(AtomicU32, AtomicU64); CAPACITY],
    read: AtomicUsize,
    write: AtomicUsize,
    pub flags: AtomicU32,
    pub notifications: Notifications,
    // Initialized and queried on the main thread only, never by the audio consumer.
    known_parameters: Mutex<HashSet<u32>>,
    // Editor edits that did not fit the processor queue, latest value per ID.
    // Main thread only; flushed in order before newer edits are queued.
    deferred: Mutex<Vec<(u32, f64)>>,
}
impl Edits {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| (AtomicU32::new(0), AtomicU64::new(0))),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            flags: AtomicU32::new(0),
            notifications: Notifications::new(),
            known_parameters: Mutex::new(HashSet::new()),
            deferred: Mutex::new(Vec::new()),
        }
    }
    /// Installs enumerated IDs on the main thread, before attaching its handler or
    /// while audio admission is closed and the consumer has left its busy scope.
    /// The audio consumer never accesses this set.
    pub fn set_known_parameters(&self, ids: impl IntoIterator<Item = u32>) {
        let mut known = self
            .known_parameters
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *known = ids.into_iter().collect();
        self.deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _)| known.contains(id));
        // Admission excludes the usual consumer: compact queued edits without
        // retaining stale IDs, preserving order and the latest surviving values.
        let mut pending = [(0, 0.0); CAPACITY];
        let mut count = 0;
        while let Some(edit) = self.pop() {
            if known.contains(&edit.0) {
                pending[count] = edit;
                count += 1;
            }
        }
        for &(id, value) in &pending[..count] {
            let _ = self.push(id, value);
        }
    }
    /// Queues an editor edit for the processor. A burst larger than the queue
    /// keeps each parameter's latest value for the next main-thread flush, so the
    /// processor never stays on a value the editor already replaced.
    fn push_edit(&self, id: u32, value: f64, known_count: usize) {
        let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        if deferred.is_empty() && self.push(id, value) {
            return;
        }
        let has_capacity = deferred.len() < known_count.min(DEFERRED_EDIT_LIMIT);
        match deferred.iter_mut().find(|(pending, _)| *pending == id) {
            Some(entry) => entry.1 = value,
            None if has_capacity => {
                deferred.push((id, value));
            }
            None => {
                self.flags.fetch_or(1, Ordering::Release);
            }
        }
    }
    /// Moves deferred editor edits into the processor queue as space frees.
    pub fn flush_deferred(&self) {
        let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let sent = deferred
            .iter()
            .take_while(|(id, value)| self.push(*id, *value))
            .count();
        deferred.drain(..sent);
    }
    /// Main thread only, with audio admission closed and no active consumer.
    /// State replacement supersedes processor edits, not notification history.
    pub fn clear_pending_edits(&self) {
        self.read
            .store(self.write.load(Ordering::Acquire), Ordering::Release);
        self.deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    /// An application value supersedes an older deferred editor value.
    pub fn forget_deferred(&self, id: u32) {
        self.deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(pending, _)| *pending != id);
    }
    /// Main-thread producer preflight; only the audio consumer can free space.
    pub(crate) fn has_capacity(&self) -> bool {
        self.write
            .load(Ordering::Relaxed)
            .wrapping_sub(self.read.load(Ordering::Acquire))
            < CAPACITY
    }
    pub(crate) fn push(&self, id: u32, value: f64) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        if write.wrapping_sub(self.read.load(Ordering::Acquire)) == CAPACITY {
            return false;
        }
        let slot = &self.slots[write % CAPACITY];
        slot.0.store(id, Ordering::Relaxed);
        slot.1.store(value.to_bits(), Ordering::Relaxed);
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }
    pub fn pop(&self) -> Option<(u32, f64)> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        let slot = &self.slots[read % CAPACITY];
        let edit = (
            slot.0.load(Ordering::Relaxed),
            f64::from_bits(slot.1.load(Ordering::Relaxed)),
        );
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(edit)
    }
}
const ATTRIBUTE_ENTRY_LIMIT: usize = 1024;
const ATTRIBUTE_VALUE_LIMIT: usize = 16 * 1024 * 1024;
const ATTRIBUTE_TOTAL_LIMIT: usize = 64 * 1024 * 1024;

enum AttributeValue {
    Int(i64),
    Float(f64),
    String(Vec<TChar>),
    Binary(Vec<u8>),
}
impl AttributeValue {
    fn size(&self) -> usize {
        match self {
            Self::Int(_) | Self::Float(_) => 8,
            Self::String(value) => value.len() * std::mem::size_of::<TChar>(),
            Self::Binary(value) => value.len(),
        }
    }
}

#[derive(Default)]
struct AttributeStorage {
    values: HashMap<Vec<u8>, AttributeValue>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct HostAttributeList(Mutex<AttributeStorage>);
impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}
/// Copies a plugin-supplied byte string, scanning at most `ATTRIBUTE_VALUE_LIMIT`
/// bytes for its terminator so an unterminated string cannot run the scan past
/// that bound.
///
/// # Safety
/// `text` must point to readable bytes up to and including a NUL terminator, or
/// to at least `ATTRIBUTE_VALUE_LIMIT + 1` readable bytes, for this synchronous
/// call. The bound only limits how far a broken caller's string is read; it
/// cannot validate an arbitrary pointer.
unsafe fn bounded_bytes(text: *const c_char) -> Result<Vec<u8>, tresult> {
    if text.is_null() {
        return Err(kInvalidArgument);
    }
    let mut length = 0;
    loop {
        if length > ATTRIBUTE_VALUE_LIMIT {
            return Err(kOutOfMemory);
        }
        // SAFETY: the caller guarantees this byte is readable: the terminator
        // has not been seen yet and the scan is still within the documented bound.
        if unsafe { text.wrapping_add(length).read() } == 0 {
            break;
        }
        length += 1;
    }
    let mut copy = Vec::new();
    copy.try_reserve_exact(length).map_err(|_| kOutOfMemory)?;
    // SAFETY: the scan above read exactly these `length` non-NUL bytes.
    copy.extend_from_slice(unsafe { std::slice::from_raw_parts(text.cast::<u8>(), length) });
    Ok(copy)
}

impl HostAttributeList {
    unsafe fn key(id: IAttributeList_::AttrID) -> Result<Vec<u8>, tresult> {
        // SAFETY: the SDK contract requires AttrID to point to a terminated byte
        // string that stays valid for this synchronous call.
        unsafe { bounded_bytes(id) }
    }

    fn set(&self, key: Vec<u8>, value: AttributeValue) -> tresult {
        if value.size() > ATTRIBUTE_VALUE_LIMIT {
            return kOutOfMemory;
        }
        let Ok(mut storage) = self.0.lock() else {
            return kResultFalse;
        };
        let existing = storage.values.get(&key);
        let previous = existing.map_or(0, |value| key.len() + value.size());
        if existing.is_none() {
            if storage.values.len() >= ATTRIBUTE_ENTRY_LIMIT {
                return kOutOfMemory;
            }
            if storage.values.try_reserve(1).is_err() {
                return kOutOfMemory;
            }
        }
        let Some(bytes) = storage
            .bytes
            .checked_sub(previous)
            .and_then(|bytes| bytes.checked_add(key.len()))
            .and_then(|bytes| bytes.checked_add(value.size()))
        else {
            return kOutOfMemory;
        };
        if bytes > ATTRIBUTE_TOTAL_LIMIT {
            return kOutOfMemory;
        }
        storage.values.insert(key, value);
        storage.bytes = bytes;
        kResultOk
    }
}
impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: IAttributeList_::AttrID, value: i64) -> tresult {
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        self.set(key, AttributeValue::Int(value))
    }

    unsafe fn getInt(&self, id: IAttributeList_::AttrID, value: *mut i64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        let Ok(storage) = self.0.lock() else {
            return kResultFalse;
        };
        let Some(AttributeValue::Int(stored)) = storage.values.get(&key) else {
            return kResultFalse;
        };
        // SAFETY: caller supplied the required writable scalar output.
        unsafe { *value = *stored };
        kResultOk
    }

    unsafe fn setFloat(&self, id: IAttributeList_::AttrID, value: f64) -> tresult {
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        self.set(key, AttributeValue::Float(value))
    }

    unsafe fn getFloat(&self, id: IAttributeList_::AttrID, value: *mut f64) -> tresult {
        if value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        let Ok(storage) = self.0.lock() else {
            return kResultFalse;
        };
        let Some(AttributeValue::Float(stored)) = storage.values.get(&key) else {
            return kResultFalse;
        };
        // SAFETY: caller supplied the required writable scalar output.
        unsafe { *value = *stored };
        kResultOk
    }

    unsafe fn setString(&self, id: IAttributeList_::AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        let mut length = 0usize;
        loop {
            if length >= ATTRIBUTE_VALUE_LIMIT / std::mem::size_of::<TChar>() {
                return kOutOfMemory;
            }
            // SAFETY: the SDK contract requires a terminated TChar string.
            let current = unsafe { string.add(length) };
            // SAFETY: current points to the next code unit required by that contract.
            if unsafe { *current } == 0 {
                break;
            }
            length += 1;
        }
        let mut stored = Vec::new();
        if stored.try_reserve_exact(length + 1).is_err() {
            return kOutOfMemory;
        }
        // SAFETY: the scan above established length readable code units plus the terminator.
        stored.extend_from_slice(unsafe { std::slice::from_raw_parts(string, length + 1) });
        self.set(key, AttributeValue::String(stored))
    }

    unsafe fn getString(
        &self,
        id: IAttributeList_::AttrID,
        string: *mut TChar,
        sizeInBytes: u32,
    ) -> tresult {
        if string.is_null() || sizeInBytes < std::mem::size_of::<TChar>() as u32 {
            return kInvalidArgument;
        }
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        let Ok(storage) = self.0.lock() else {
            return kResultFalse;
        };
        let Some(AttributeValue::String(stored)) = storage.values.get(&key) else {
            return kResultFalse;
        };
        let capacity = sizeInBytes as usize / std::mem::size_of::<TChar>();
        let copied = stored.len().saturating_sub(1).min(capacity - 1);
        // SAFETY: caller supplied sizeInBytes writable bytes; copied is within that capacity.
        unsafe { std::ptr::copy_nonoverlapping(stored.as_ptr(), string, copied) };
        // SAFETY: capacity is at least one and copied is strictly below capacity.
        let terminator = unsafe { string.add(copied) };
        // SAFETY: terminator points within the caller-provided writable buffer.
        unsafe { *terminator = 0 };
        kResultOk
    }

    unsafe fn setBinary(
        &self,
        id: IAttributeList_::AttrID,
        data: *const c_void,
        sizeInBytes: u32,
    ) -> tresult {
        if data.is_null() {
            return kInvalidArgument;
        }
        let size = sizeInBytes as usize;
        if size > ATTRIBUTE_VALUE_LIMIT {
            return kOutOfMemory;
        }
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        let mut stored = Vec::new();
        if stored.try_reserve_exact(size).is_err() {
            return kOutOfMemory;
        }
        // SAFETY: caller supplies sizeInBytes readable bytes for this synchronous copy.
        stored.extend_from_slice(unsafe { std::slice::from_raw_parts(data.cast(), size) });
        self.set(key, AttributeValue::Binary(stored))
    }

    unsafe fn getBinary(
        &self,
        id: IAttributeList_::AttrID,
        data: *mut *const c_void,
        sizeInBytes: *mut u32,
    ) -> tresult {
        if data.is_null() || sizeInBytes.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: forwarded SDK AttrID follows the interface contract.
        let key = match unsafe { Self::key(id) } {
            Ok(key) => key,
            Err(result) => return result,
        };
        let Ok(storage) = self.0.lock() else {
            return kResultFalse;
        };
        let Some(AttributeValue::Binary(stored)) = storage.values.get(&key) else {
            return kResultFalse;
        };
        // SAFETY: caller supplied both required writable outputs. Vec storage remains
        // allocated until this key is replaced or the attribute list is released.
        unsafe { *data = stored.as_ptr().cast() };
        // SAFETY: caller supplied a writable size output.
        unsafe { *sizeInBytes = stored.len() as u32 };
        kResultOk
    }
}

pub(crate) struct HostMessage {
    id: Mutex<Option<Vec<c_char>>>,
    attributes: vst3::ComPtr<IAttributeList>,
}
impl HostMessage {
    fn new() -> Self {
        let attributes = ComWrapper::new(HostAttributeList::default())
            .to_com_ptr::<IAttributeList>()
            .expect("attribute list interface");
        Self {
            id: Mutex::new(None),
            attributes,
        }
    }
}
impl Class for HostMessage {
    type Interfaces = (IMessage,);
}
impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        let Ok(id) = self.id.lock() else {
            return std::ptr::null();
        };
        id.as_ref().map_or(std::ptr::null(), |id| id.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let Ok(mut stored) = self.id.lock() else {
            return;
        };
        if id.is_null() {
            *stored = None;
            return;
        }
        // SAFETY: IMessage requires a terminated FIDString that stays valid for
        // this synchronous call; the scan stops at the host's length bound.
        let Ok(bytes) = (unsafe { bounded_bytes(id) }) else {
            return;
        };
        let mut copied = Vec::new();
        if copied.try_reserve_exact(bytes.len() + 1).is_err() {
            return;
        }
        copied.extend(bytes.iter().map(|byte| *byte as c_char));
        copied.push(0);
        *stored = Some(copied);
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes.as_ptr()
    }
}

pub(crate) struct Host<const WAYLAND: bool = false> {
    #[cfg(target_os = "linux")]
    pub run_loop: Rc<crate::linux_runloop::RunLoop>,
    #[cfg(target_os = "linux")]
    pub wayland: Rc<crate::wayland_editor::WaylandHost>,
    #[cfg(target_os = "linux")]
    wayland_interface: std::cell::Cell<*mut IWaylandHost>,
}
impl<const WAYLAND: bool> Host<WAYLAND> {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            run_loop: Rc::new(crate::linux_runloop::RunLoop::default()),
            #[cfg(target_os = "linux")]
            wayland: Rc::new(crate::wayland_editor::WaylandHost::default()),
            #[cfg(target_os = "linux")]
            wayland_interface: std::cell::Cell::new(std::ptr::null_mut()),
        }
    }
}
#[cfg(target_os = "linux")]
crate::linux_runloop::implement!(Host<false>, run_loop);
#[cfg(target_os = "linux")]
crate::linux_runloop::implement!(Host<true>, run_loop);
impl Class for Host {
    #[cfg(not(target_os = "linux"))]
    type Interfaces = (IHostApplication,);
    #[cfg(target_os = "linux")]
    type Interfaces = (IHostApplication, Linux::IRunLoop);
}
#[cfg(target_os = "linux")]
impl Class for Host<true> {
    type Interfaces = (IHostApplication, Linux::IRunLoop, IWaylandHost);
}
impl<const WAYLAND: bool> IHostApplicationTrait for Host<WAYLAND> {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: SDK supplies one writable String128 for this synchronous call.
        let name = unsafe { &mut *name };
        name.fill(0);
        for (dst, src) in name.iter_mut().zip("vst-audio".encode_utf16()) {
            *dst = src;
        }
        kResultOk
    }
    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        object: *mut *mut c_void,
    ) -> tresult {
        if object.is_null() || cid.is_null() || iid.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: caller supplied the required writable interface output.
        unsafe { *object = std::ptr::null_mut() };
        // SAFETY: createInstance supplies a readable class identifier.
        let class = unsafe { *cid };
        // SAFETY: createInstance supplies a readable interface identifier.
        let interface = unsafe { *iid };
        if class == IMessage_iid && interface == IMessage_iid {
            let message = ComWrapper::new(HostMessage::new())
                .to_com_ptr::<IMessage>()
                .expect("message interface");
            // SAFETY: transfer the newly-created object's one owned interface reference.
            unsafe { *object = message.into_raw().cast() };
            return kResultOk;
        }
        if class == IAttributeList_iid && interface == IAttributeList_iid {
            let attributes = ComWrapper::new(HostAttributeList::default())
                .to_com_ptr::<IAttributeList>()
                .expect("attribute list interface");
            // SAFETY: transfer the newly-created object's one owned interface reference.
            unsafe { *object = attributes.into_raw().cast() };
            return kResultOk;
        }
        #[cfg(target_os = "linux")]
        if WAYLAND && interface == IWaylandHost_iid {
            // SAFETY: wrap() installs this non-owning self pointer before publishing
            // the COM object. This synchronous call retains the object itself.
            let host = unsafe { ComRef::from_raw(self.wayland_interface.get()) }
                .expect("Wayland interface")
                .to_com_ptr();
            // SAFETY: caller provided a writable interface output; transfer one owned COM reference.
            unsafe { *object = host.into_raw().cast() };
            return kResultOk;
        }
        kNotImplemented
    }
}
#[cfg(target_os = "linux")]
impl IWaylandHostTrait for Host<true> {
    unsafe fn openWaylandConnection(&self) -> *mut wl_display {
        self.wayland.open().cast()
    }
    unsafe fn closeWaylandConnection(&self, display: *mut wl_display) -> tresult {
        if self.wayland.close(display.cast()) {
            kResultOk
        } else {
            kInvalidArgument
        }
    }
}
impl Host {
    pub fn wrap(self) -> vst3::ComPtr<FUnknown> {
        #[cfg(target_os = "linux")]
        if crate::linux_editor::wayland_allowed() {
            let host = ComWrapper::new(Host::<true> {
                run_loop: self.run_loop,
                wayland: self.wayland,
                wayland_interface: std::cell::Cell::new(std::ptr::null_mut()),
            });
            let interface = host.to_com_ptr::<IWaylandHost>().expect("Wayland host");
            host.wayland_interface.set(interface.as_ptr());
            return host.to_com_ptr::<FUnknown>().expect("host interface");
        }
        ComWrapper::new(self)
            .to_com_ptr::<FUnknown>()
            .expect("host interface")
    }
}
pub(crate) struct Handler(pub Arc<Edits>);
impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}
impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, id: u32) -> tresult {
        self.0.notifications.push(ParameterEdit {
            id,
            phase: EditPhase::Begin,
            value: 0.0,
        });
        kResultOk
    }
    unsafe fn performEdit(&self, id: u32, value: f64) -> tresult {
        let known = self
            .0
            .known_parameters
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Faulty VST3 controllers can invent IDs (audit #11 TD-33). Reject them
        // before either queue: perform_edit_unknown_id_flood_requests_refresh_without_retaining_edits
        // reproduces the unbounded overflow that otherwise follows.
        if !known.contains(&id) || !value.is_finite() || !(0.0..=1.0).contains(&value) {
            self.0.flags.fetch_or(1, Ordering::Release);
            return kInvalidArgument;
        }
        self.0.notifications.push(ParameterEdit {
            id,
            phase: EditPhase::Perform,
            value,
        });
        // Test only: never enable __test-drop-editor-edits in distribution builds.
        if !cfg!(feature = "__test-drop-editor-edits") {
            self.0.push_edit(id, value, known.len());
        }
        kResultOk
    }
    unsafe fn endEdit(&self, id: u32) -> tresult {
        self.0.notifications.push(ParameterEdit {
            id,
            phase: EditPhase::End,
            value: 0.0,
        });
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        if flags & (RestartFlags_::kParamValuesChanged | RestartFlags_::kParamTitlesChanged) != 0 {
            self.0.flags.fetch_or(1, Ordering::Release);
        }
        if flags & RestartFlags_::kLatencyChanged != 0 {
            self.0.flags.fetch_or(2, Ordering::Release);
        }
        if flags & RestartFlags_::kIoChanged != 0 {
            self.0.flags.fetch_or(4, Ordering::Release);
        }
        if flags & (RestartFlags_::kMidiCCAssignmentChanged | RestartFlags_::kParamIDMappingChanged)
            != 0
        {
            self.0.flags.fetch_or(8 | 1, Ordering::Release);
        }
        kResultOk
    }
}

pub(crate) struct Stream(pub Mutex<StateBuffer>);
impl Stream {
    pub fn new(bytes: Vec<u8>) -> ComWrapper<Self> {
        ComWrapper::new(Self(Mutex::new(StateBuffer::new(bytes))))
    }

    pub fn try_from_slice(bytes: &[u8]) -> std::io::Result<ComWrapper<Self>> {
        Ok(ComWrapper::new(Self(Mutex::new(
            StateBuffer::try_from_slice(bytes)?,
        ))))
    }
}
impl Class for Stream {
    type Interfaces = (IBStream,);
}
impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, count: i32, actual: *mut i32) -> tresult {
        if count < 0 || (count > 0 && buffer.is_null()) {
            return kInvalidArgument;
        }
        let Ok(mut stream) = self.0.try_lock() else {
            return kResultFalse;
        };
        // The caller's buffer may be uninitialized memory, so it is never viewed
        // as a `&mut [u8]`: the bytes are copied in through the raw pointer.
        let available = stream.remaining();
        let n = available.len().min(count as usize);
        if n > 0 {
            // SAFETY: the IBStream caller provides `count` writable bytes that do
            // not overlap this stream's private storage; n <= count.
            unsafe { std::ptr::copy_nonoverlapping(available.as_ptr(), buffer.cast::<u8>(), n) };
            stream.advance(n);
        }
        if !actual.is_null() {
            // SAFETY: optional writable byte-count output supplied by the caller.
            unsafe { *actual = n as i32 };
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, count: i32, actual: *mut i32) -> tresult {
        if count < 0 || (count > 0 && buffer.is_null()) {
            return kInvalidArgument;
        }
        let bytes = if count == 0 {
            &[][..]
        } else {
            // SAFETY: IBStream caller provides count readable bytes for this synchronous write.
            unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), count as usize) }
        };
        let Ok(mut stream) = self.0.try_lock() else {
            return kResultFalse;
        };
        let n = match stream.write(bytes) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::OutOfMemory => return kOutOfMemory,
            Err(_) => return kResultFalse,
        };
        if !actual.is_null() {
            // SAFETY: optional writable byte-count output supplied by the caller.
            unsafe { *actual = n as i32 };
        }
        kResultOk
    }
    unsafe fn seek(&self, position: i64, mode: i32, actual: *mut i64) -> tresult {
        let from = match mode {
            0 if position >= 0 => SeekFrom::Start(position as u64),
            1 => SeekFrom::Current(position),
            2 => SeekFrom::End(position),
            _ => return kInvalidArgument,
        };
        let Ok(mut stream) = self.0.try_lock() else {
            return kResultFalse;
        };
        let Ok(position) = stream.seek(from) else {
            return kInvalidArgument;
        };
        if !actual.is_null() {
            // SAFETY: optional writable seek-position output supplied by caller.
            unsafe { *actual = position as i64 };
        }
        kResultOk
    }
    unsafe fn tell(&self, position: *mut i64) -> tresult {
        if position.is_null() {
            return kInvalidArgument;
        }
        let Ok(stream) = self.0.try_lock() else {
            return kResultFalse;
        };
        // SAFETY: non-null writable position output required by IBStream.
        unsafe { *position = stream.position() as i64 };
        kResultOk
    }
}

pub(crate) use crate::vst3_events::Changes;

pub(crate) struct Frame {
    pub window: Rc<EditorWindow>,
    #[cfg(target_os = "linux")]
    pub run_loop: Rc<crate::linux_runloop::RunLoop>,
}
#[cfg(target_os = "linux")]
crate::linux_runloop::implement!(Frame, run_loop);
impl Class for Frame {
    #[cfg(not(target_os = "linux"))]
    type Interfaces = (IPlugFrame,);
    #[cfg(target_os = "linux")]
    type Interfaces = (IPlugFrame, Linux::IRunLoop, IWaylandFrame);
}
#[cfg(target_os = "linux")]
impl IWaylandFrameTrait for Frame {
    unsafe fn getWaylandSurface(&self, display: *mut wl_display) -> *mut wl_surface {
        self.window
            .wayland()
            .filter(|w| w.matches(display.cast()))
            .map_or(std::ptr::null_mut(), |w| w.surface().cast())
    }
    unsafe fn getParentSurface(
        &self,
        rect: *mut ViewRect,
        display: *mut wl_display,
    ) -> *mut xdg_surface {
        let Some(window) = self.window.wayland().filter(|w| w.matches(display.cast())) else {
            return std::ptr::null_mut();
        };
        if rect.is_null() {
            return std::ptr::null_mut();
        }
        let Ok((_, width, height)) = window.status() else {
            return std::ptr::null_mut();
        };
        // SAFETY: SDK caller supplies a writable ViewRect, used only during this UI-thread call.
        unsafe {
            *rect = ViewRect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }
        };
        window.xdg().cast()
    }
    unsafe fn getParentToplevel(&self, display: *mut wl_display) -> *mut xdg_toplevel {
        self.window
            .wayland()
            .filter(|w| w.matches(display.cast()))
            .map_or(std::ptr::null_mut(), |w| w.toplevel().cast())
    }
}
impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
        if rect.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: SDK resize callback supplies a rectangle valid for this call.
        let size = unsafe { *rect };
        let Some(width) = size.right.checked_sub(size.left) else {
            return kInvalidArgument;
        };
        let Some(height) = size.bottom.checked_sub(size.top) else {
            return kInvalidArgument;
        };
        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            return kInvalidArgument;
        }
        // SAFETY: callback supplies the requesting live view; borrow does not escape.
        let Some(view) = (unsafe { ComRef::from_raw(view) }) else {
            return kInvalidArgument;
        };
        if self.window.resize(width, height).is_err() {
            return kResultFalse;
        }
        // SAFETY: main-thread callback, rectangle remains valid; window has been resized first.
        unsafe { view.onSize(rect) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perform_edit_unknown_id_flood_requests_refresh_without_retaining_edits() {
        let edits = Arc::new(Edits::new());
        let handler = ComWrapper::new(Handler(edits.clone()))
            .to_com_ptr::<IComponentHandler>()
            .unwrap();
        // Audit #11 TD-33: a faulty controller invents thousands of parameter IDs,
        // including callbacks before the host has installed its metadata IDs.
        for id in 10_000..14_096 {
            // SAFETY: retained host handler called synchronously on its main thread;
            // performEdit takes scalar arguments and validates the foreign ID.
            let _ = unsafe { handler.performEdit(id, 0.5) };
        }
        assert_eq!(edits.deferred.lock().unwrap().len(), 0);
        assert_eq!(edits.pop(), None);
        assert!(edits.notifications.drain().is_empty());
        assert_eq!(edits.flags.load(Ordering::Acquire) & 1, 1);
    }

    #[cfg(not(feature = "__test-drop-editor-edits"))]
    #[test]
    fn perform_edit_known_overflow_keeps_latest_values_during_unknown_id_flood() {
        let edits = Arc::new(Edits::new());
        edits.set_known_parameters([7, 19]);
        let handler = ComWrapper::new(Handler(edits.clone()))
            .to_com_ptr::<IComponentHandler>()
            .unwrap();
        for _ in 0..256 {
            // SAFETY: retained handler on its main thread, scalar known ID/value.
            let _ = unsafe { handler.performEdit(7, 0.25) };
        }
        for (id, value) in [(7, 0.5), (19, 0.5), (7, 0.75), (19, 1.0)] {
            // SAFETY: retained handler on its main thread, scalar known ID/value.
            let _ = unsafe { handler.performEdit(id, value) };
        }
        assert_eq!(edits.deferred.lock().unwrap().len(), 2);
        assert_eq!(edits.notifications.drain().len(), 260);
        for id in 10_000..14_096 {
            // SAFETY: retained handler on its main thread; the concrete host rejects
            // these scalar IDs without invoking foreign plugin code.
            assert_eq!(unsafe { handler.performEdit(id, 0.5) }, kInvalidArgument);
        }
        assert_eq!(edits.deferred.lock().unwrap().len(), 2);
        assert!(edits.notifications.drain().is_empty());
        assert_eq!(edits.flags.load(Ordering::Acquire) & 1, 1);
        for _ in 0..256 {
            assert_eq!(edits.pop(), Some((7, 0.25)));
        }
        assert_eq!(edits.pop(), None);
        edits.flush_deferred();
        assert_eq!(edits.pop(), Some((7, 0.75)));
        assert_eq!(edits.pop(), Some((19, 1.0)));
        assert_eq!(edits.pop(), None);
        assert!(edits.deferred.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_host_query_obeys_policy_and_identity() {
        crate::linux_editor::configure("x11", true);
        let host = Host::new().wrap();
        assert!(host.cast::<IWaylandHost>().is_none());
        crate::linux_editor::configure("wayland", true);
        let host = Host::new().wrap();
        let context = host.cast::<IHostApplication>().unwrap();
        let queried = context.cast::<IWaylandHost>().unwrap();
        let mut iid = IWaylandHost_iid;
        let mut pointer = std::ptr::null_mut();
        assert_eq!(
            // SAFETY: retained context and writable IID and interface output.
            unsafe { context.createInstance(&mut iid, &mut iid, &mut pointer) },
            kResultOk
        );
        // SAFETY: successful createInstance transferred an owned interface reference.
        let created = unsafe { vst3::ComPtr::<IWaylandHost>::from_raw(pointer.cast()) }.unwrap();
        assert_eq!(queried.as_ptr(), created.as_ptr());
        drop((host, context, queried));
        assert!(created.cast::<IHostApplication>().is_some());
        crate::linux_editor::configure("auto", true);
    }

    #[test]
    fn host_message_and_attributes_round_trip() {
        let host = Host::new().wrap();
        let context = host.cast::<IHostApplication>().unwrap();
        let mut cid = IMessage_iid;
        let mut iid = IMessage_iid;
        let mut pointer = std::ptr::null_mut();
        assert_eq!(
            // SAFETY: retained host and writable class, interface and object pointers.
            unsafe { context.createInstance(&mut cid, &mut iid, &mut pointer) },
            kResultOk
        );
        // SAFETY: successful createInstance transferred one owned message reference.
        let message = unsafe { vst3::ComPtr::<IMessage>::from_raw(pointer.cast()) }.unwrap();
        // SAFETY: terminated string remains readable through this synchronous call.
        unsafe { message.setMessageID(c"editor".as_ptr()) };
        // SAFETY: message retains the live IMessage interface for this call.
        let id = unsafe { message.getMessageID() };
        assert_eq!(
            // SAFETY: setMessageID copied a terminated identifier; message is still
            // retained and its ID has not been changed since obtaining this pointer.
            unsafe { std::ffi::CStr::from_ptr(id) },
            c"editor"
        );
        // SAFETY: retained message owns one stable attribute-list object.
        let attributes = unsafe { message.getAttributes() };
        // SAFETY: same retained message remains alive between calls.
        assert_eq!(attributes, unsafe { message.getAttributes() });
        // SAFETY: borrowed pointer remains valid while message is retained.
        let attributes = unsafe { ComRef::from_raw(attributes) }.unwrap();
        assert_eq!(
            // SAFETY: message retains attributes and the static ID is NUL-terminated.
            unsafe { attributes.setInt(c"int".as_ptr(), -17) },
            kResultOk
        );
        assert_eq!(
            // SAFETY: message retains attributes and the static ID is NUL-terminated.
            unsafe { attributes.setFloat(c"float".as_ptr(), 3.25) },
            kResultOk
        );
        let mut integer = 0;
        let mut float = 0.0;
        assert_eq!(
            // SAFETY: retained attributes, terminated ID, and an exclusive writable i64 output.
            unsafe { attributes.getInt(c"int".as_ptr(), &mut integer) },
            kResultOk
        );
        assert_eq!(
            // SAFETY: retained attributes, terminated ID, and an exclusive writable f64 output.
            unsafe { attributes.getFloat(c"float".as_ptr(), &mut float) },
            kResultOk
        );
        assert_eq!(integer, -17);
        assert_eq!(float, 3.25);

        let text = [b'a' as TChar, b'b' as TChar, b'c' as TChar, 0];
        assert_eq!(
            // SAFETY: message retains attributes; text and the static ID are terminated.
            unsafe { attributes.setString(c"text".as_ptr(), text.as_ptr()) },
            kResultOk
        );
        let mut truncated = [99 as TChar; 2];
        assert_eq!(
            // SAFETY: message retains attributes, the ID is terminated, and the
            // exclusive TChar output has exactly the advertised byte capacity.
            unsafe {
                attributes.getString(
                    c"text".as_ptr(),
                    truncated.as_mut_ptr(),
                    std::mem::size_of_val(&truncated) as u32,
                )
            },
            kResultOk
        );
        assert_eq!(truncated, [b'a' as TChar, 0]);

        let binary = [1_u8, 2, 3, 4];
        assert_eq!(
            // SAFETY: message retains attributes, the ID is terminated, and binary
            // supplies the advertised four readable bytes for the synchronous copy.
            unsafe {
                attributes.setBinary(
                    c"data".as_ptr(),
                    binary.as_ptr().cast(),
                    binary.len() as u32,
                )
            },
            kResultOk
        );
        let mut data = std::ptr::null();
        let mut size = 0;
        assert_eq!(
            // SAFETY: message retains attributes; both outputs are exclusive and
            // writable for their types, and the static ID is terminated.
            unsafe { attributes.getBinary(c"data".as_ptr(), &mut data, &mut size) },
            kResultOk
        );
        assert_eq!(size, 4);
        assert_eq!(
            // SAFETY: getBinary returned four initialized bytes. The list is
            // retained by message and this value has not been replaced.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), 4) },
            binary
        );
        assert_eq!(
            // SAFETY: message retains attributes and the static ID is terminated.
            unsafe { attributes.setInt(c"other".as_ptr(), 1) },
            kResultOk
        );
        let mut same_data = std::ptr::null();
        let mut same_size = 0;
        assert_eq!(
            // SAFETY: message retains attributes; both outputs are exclusive and
            // writable for their types, and the static ID is terminated.
            unsafe { attributes.getBinary(c"data".as_ptr(), &mut same_data, &mut same_size) },
            kResultOk
        );
        assert_eq!((same_data, same_size), (data, size));
    }

    #[test]
    fn host_attributes_reject_missing_null_and_limits() {
        let attributes = HostAttributeList::default();
        let mut integer = 0;
        assert_eq!(
            // SAFETY: terminated missing key and exclusive writable i64 output.
            unsafe { attributes.getInt(c"missing".as_ptr(), &mut integer) },
            kResultFalse
        );
        assert_eq!(
            // SAFETY: this concrete implementation rejects a null ID in bounded_bytes
            // before dereferencing it; no foreign implementation is called.
            unsafe { attributes.setInt(std::ptr::null(), 1) },
            kInvalidArgument
        );
        assert_eq!(
            // SAFETY: getInt rejects the null output before any pointer access.
            unsafe { attributes.getInt(c"value".as_ptr(), std::ptr::null_mut()) },
            kInvalidArgument
        );
        assert_eq!(
            // SAFETY: setString rejects the null string before any pointer access.
            unsafe { attributes.setString(c"value".as_ptr(), std::ptr::null()) },
            kInvalidArgument
        );
        assert_eq!(
            // SAFETY: setBinary rejects the null data before any pointer access,
            // including for this zero-byte request.
            unsafe { attributes.setBinary(c"value".as_ptr(), std::ptr::null(), 0) },
            kInvalidArgument
        );
        let byte = 0_u8;
        assert_eq!(
            // SAFETY: this concrete setBinary rejects oversized input before
            // forming a slice or reading the deliberately undersized buffer.
            unsafe {
                attributes.setBinary(
                    c"large".as_ptr(),
                    (&byte as *const u8).cast(),
                    (ATTRIBUTE_VALUE_LIMIT + 1) as u32,
                )
            },
            kOutOfMemory
        );
        for index in 0..ATTRIBUTE_ENTRY_LIMIT {
            let key = std::ffi::CString::new(format!("key-{index}")).unwrap();
            // SAFETY: each CString is terminated and remains live for the call.
            assert_eq!(unsafe { attributes.setInt(key.as_ptr(), 1) }, kResultOk);
        }
        assert_eq!(
            // SAFETY: the static key is terminated and remains readable for the call.
            unsafe { attributes.setInt(c"one-too-many".as_ptr(), 1) },
            kOutOfMemory
        );
    }

    #[test]
    fn host_created_objects_start_with_one_reference() {
        let host = Host::new().wrap();
        let context = host.cast::<IHostApplication>().unwrap();
        for mut identifier in [IMessage_iid, IAttributeList_iid] {
            let mut iid = identifier;
            let mut pointer = std::ptr::null_mut();
            assert_eq!(
                // SAFETY: retained host and writable identifiers and object output.
                unsafe { context.createInstance(&mut identifier, &mut iid, &mut pointer) },
                kResultOk
            );
            let unknown = pointer.cast::<FUnknown>();
            // SAFETY: successful creation returned a live FUnknown-derived interface
            // with one owned reference and the FUnknown-compatible vtable prefix.
            let vtbl = unsafe { (*unknown).vtbl };
            // SAFETY: the live host-created COM object has an initialized FUnknown vtable.
            let add_ref = unsafe { (*vtbl).addRef };
            // SAFETY: add_ref belongs to this live interface, whose creation reference is held.
            let after_add = unsafe { add_ref(unknown) };
            assert_eq!(after_add, 2);
            // SAFETY: the creation reference and explicit addRef keep unknown alive.
            let vtbl = unsafe { (*unknown).vtbl };
            // SAFETY: the retained host-created COM object's vtable is initialized and readable.
            let release = unsafe { (*vtbl).release };
            // SAFETY: release belongs to unknown; balance addRef, retaining the creation reference.
            let after_release = unsafe { release(unknown) };
            assert_eq!(after_release, 1);
            // SAFETY: the original creation reference still keeps unknown alive.
            let vtbl = unsafe { (*unknown).vtbl };
            // SAFETY: the retained host-created COM object's vtable is initialized and readable.
            let release = unsafe { (*vtbl).release };
            // SAFETY: release the original reference exactly once; unknown is not used afterwards.
            assert_eq!(unsafe { release(unknown) }, 0);
        }
    }

    #[test]
    fn a_locked_stream_refuses_instead_of_racing() {
        let stream = Stream(Mutex::new(StateBuffer::default()));
        let _locked = stream.0.lock().unwrap();
        let mut byte = 1_u8;
        let mut actual = 0;
        let mut position = 0;
        assert_eq!(
            // SAFETY: pointers refer to live one-byte and output storage for this synchronous call.
            unsafe { IBStreamTrait::write(&stream, (&mut byte as *mut u8).cast(), 1, &mut actual) },
            kResultFalse
        );
        assert_eq!(
            // SAFETY: pointers refer to live one-byte and output storage for this synchronous call.
            unsafe { IBStreamTrait::read(&stream, (&mut byte as *mut u8).cast(), 1, &mut actual) },
            kResultFalse
        );
        assert_eq!(
            // SAFETY: position points to live writable storage for this synchronous call.
            unsafe { IBStreamTrait::tell(&stream, &mut position) },
            kResultFalse
        );
    }
}
