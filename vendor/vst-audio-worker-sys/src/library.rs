#![allow(unsafe_code)]
use std::ffi::CStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, FreeLibrary, HMODULE},
        System::LibraryLoader::{GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW},
    },
    core::PCWSTR,
};

#[derive(Debug)]
pub struct Library(HMODULE);
impl Library {
    pub fn load(path: &Path) -> windows::core::Result<Self> {
        if !path.is_absolute() {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG));
        }
        let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
        if name.contains(&0) {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG));
        }
        name.push(0);
        // SAFETY: terminated absolute path; code is loaded only in the disposable host.
        let module =
            unsafe { LoadLibraryExW(PCWSTR(name.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH) }?;
        Ok(Self(module))
    }
    pub fn symbol(&self, name: &CStr) -> Option<unsafe extern "system" fn() -> isize> {
        // SAFETY: live module and terminated symbol name; caller must validate callable ABI.
        unsafe { GetProcAddress(self.0, windows::core::PCSTR(name.as_ptr().cast())) }
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: uniquely owned module, dropped after all plugin objects and callbacks.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}
