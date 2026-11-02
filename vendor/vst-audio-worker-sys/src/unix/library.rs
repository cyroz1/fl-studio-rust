#![allow(unsafe_code)]
//! Native module ownership. A Linux ModuleEntry receives the dlopen handle.
use std::{
    ffi::{CStr, CString, c_void},
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
};

#[derive(Debug)]
pub struct Library {
    handle: *mut c_void,
    #[cfg(target_os = "macos")]
    bundle: bool,
}

impl Library {
    pub fn load(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "absolute plugin path required",
            ));
        }
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in module path"))?;
        #[cfg(target_os = "macos")]
        if path.is_dir() {
            return Self::load_bundle(&name);
        }
        #[cfg(target_os = "linux")]
        install_x11_error_handler();
        // SAFETY: terminated absolute path. Foreign initializers run exclusively
        // in the disposable worker; they are not assumed memory-safe.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: dlerror returns thread-local diagnostic storage after failed dlopen.
            let pointer = unsafe { libc::dlerror() };
            let detail = if pointer.is_null() {
                "unknown loader error".into()
            } else {
                // SAFETY: non-null dlerror pointer is a terminated string until next loader call.
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(io::Error::other(format!(
                "cannot load {}: {detail}",
                path.display()
            )));
        }
        Ok(Self {
            handle,
            #[cfg(target_os = "macos")]
            bundle: false,
        })
    }
    pub fn module_context(&self) -> *mut c_void {
        self.handle
    }
    pub fn symbol(&self, name: &CStr) -> Option<unsafe extern "system" fn() -> isize> {
        #[cfg(target_os = "macos")]
        if self.bundle {
            return self.bundle_symbol(name);
        }
        // SAFETY: retained native module and terminated export name.
        let symbol = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        if symbol.is_null() {
            return None;
        }
        // SAFETY: POSIX dlsym function address representation; the result remains
        // unsafe to call and the format adapter supplies the real signature.
        Some(unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "system" fn() -> isize>(symbol)
        })
    }
}
#[cfg(target_os = "linux")]
#[repr(C)]
struct XErrorEvent {
    _type: i32,
    _display: *mut c_void,
    _resource_id: libc::c_ulong,
    _serial: libc::c_ulong,
    error_code: u8,
    request_code: u8,
    minor_code: u8,
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn x11_error_handler(_display: *mut c_void, error: *mut XErrorEvent) -> i32 {
    if !error.is_null() {
        // SAFETY: Xlib invokes the handler synchronously with a readable XErrorEvent.
        let error = unsafe { &*error };
        // A failed stderr write must not panic inside this C callback.
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr().lock(),
            format_args!(
                "X11 protocol error: code={} request={}.{} resource={:#x}\n",
                error.error_code, error.request_code, error.minor_code, error._resource_id
            ),
        );
    }
    0
}

/// Xlib's default error handler exits the process. A plugin that is not a
/// standalone application leaves error handling to its host, and JUCE also loads
/// libX11 itself only when its editor first needs it, after the plugin module is
/// loaded. Load libX11 first and keep it loaded, so this handler is in place
/// however the plugin reaches Xlib. Without libX11 no plugin can use X11.
#[cfg(target_os = "linux")]
fn install_x11_error_handler() {
    static X11: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let x11 = *X11.get_or_init(|| {
        // SAFETY: loads the system Xlib by soname; the handle is never closed,
        // because unloading libX11 would forget the registered handler.
        unsafe { libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) as usize }
    }) as *mut c_void;
    if x11.is_null() {
        return;
    }
    // SAFETY: retained libX11 handle and terminated symbol name.
    let symbol = unsafe { libc::dlsym(x11, c"XSetErrorHandler".as_ptr()) };
    if !symbol.is_null() {
        type SetErrorHandler = unsafe extern "C" fn(
            Option<unsafe extern "C" fn(*mut c_void, *mut XErrorEvent) -> i32>,
        ) -> *mut c_void;
        // SAFETY: dlsym returned XSetErrorHandler from the retained libX11.
        let set_handler: SetErrorHandler = unsafe { std::mem::transmute(symbol) };
        // SAFETY: handler has Xlib's required calling convention and static lifetime.
        unsafe { set_handler(Some(x11_error_handler)) };
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.bundle {
            // SAFETY: retained CFBundle, all foreign COM objects already dropped.
            unsafe { CFBundleUnloadExecutable(self.handle) };
            // SAFETY: release the creation reference exactly once.
            unsafe { CFRelease(self.handle) };
            return;
        }
        // SAFETY: unique dlopen ownership, foreign objects dropped before the module.
        unsafe { libc::dlclose(self.handle) };
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: *const c_void,
        bytes: *const u8,
        len: isize,
        directory: u8,
    ) -> *mut c_void;
    fn CFBundleCreate(allocator: *const c_void, url: *const c_void) -> *mut c_void;
    fn CFBundleLoadExecutable(bundle: *mut c_void) -> u8;
    fn CFBundleUnloadExecutable(bundle: *mut c_void);
    fn CFBundleGetFunctionPointerForName(bundle: *mut c_void, name: *const c_void) -> *mut c_void;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        bytes: *const i8,
        encoding: u32,
    ) -> *mut c_void;
    fn CFRelease(value: *const c_void);
}
#[cfg(target_os = "macos")]
impl Library {
    fn load_bundle(path: &CStr) -> io::Result<Self> {
        let bytes = path.to_bytes();
        // SAFETY: valid byte slice with explicit length, default allocator.
        let url = unsafe {
            CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                1,
            )
        };
        if url.is_null() {
            return Err(io::Error::other("invalid bundle URL"));
        }
        // SAFETY: live retained file URL.
        let bundle = unsafe { CFBundleCreate(std::ptr::null(), url) };
        // SAFETY: release URL's creation reference after CFBundleCreate.
        unsafe { CFRelease(url) };
        if bundle.is_null() {
            return Err(io::Error::other("invalid plugin bundle"));
        }
        // SAFETY: live bundle. Foreign initializers execute in the worker.
        if unsafe { CFBundleLoadExecutable(bundle) } == 0 {
            // SAFETY: failed load still owns the bundle creation reference.
            unsafe { CFRelease(bundle) };
            return Err(io::Error::other("cannot load bundle executable"));
        }
        Ok(Self {
            handle: bundle,
            bundle: true,
        })
    }
    fn bundle_symbol(&self, name: &CStr) -> Option<unsafe extern "system" fn() -> isize> {
        // SAFETY: terminated export name, UTF-8 encoding constant, default allocator.
        let string =
            unsafe { CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), 0x0800_0100) };
        if string.is_null() {
            return None;
        }
        // SAFETY: loaded owned bundle and live CFString.
        let pointer = unsafe { CFBundleGetFunctionPointerForName(self.handle, string) };
        // SAFETY: release the name's creation reference.
        unsafe { CFRelease(string) };
        if pointer.is_null() {
            return None;
        }
        // SAFETY: native function address; returned function remains unsafe.
        Some(unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "system" fn() -> isize>(pointer)
        })
    }
}
