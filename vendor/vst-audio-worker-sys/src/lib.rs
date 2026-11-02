#![deny(unsafe_code)]
//! Foreign plugin execution. Only the worker links this crate.
#[cfg(windows)]
pub mod editor;
#[cfg(target_os = "linux")]
#[path = "unix/editor_dispatch.rs"]
pub mod editor;
#[cfg(target_os = "macos")]
#[path = "unix/editor_macos.rs"]
pub mod editor;
#[cfg(windows)]
pub mod library;
#[cfg(unix)]
#[path = "unix/library.rs"]
pub mod library;
pub mod notifications;
pub mod plugin;
pub mod state_buffer;
pub mod vst2;
pub mod vst3;
mod vst3_support;

pub fn editor_platform() -> &'static std::ffi::CStr {
    #[cfg(windows)]
    {
        c"HWND"
    }
    #[cfg(target_os = "linux")]
    {
        c"X11EmbedWindowID"
    }
    #[cfg(target_os = "macos")]
    {
        c"NSView"
    }
}

#[cfg(target_os = "linux")]
pub mod linux_editor;
#[cfg(target_os = "linux")]
mod linux_runloop;
#[cfg(target_os = "linux")]
mod wayland_editor;

#[cfg(not(target_os = "linux"))]
pub(crate) fn native_editor_platform() -> vst_audio_protocol::EditorPlatform {
    #[cfg(windows)]
    {
        vst_audio_protocol::EditorPlatform::Win32
    }
    #[cfg(target_os = "macos")]
    {
        vst_audio_protocol::EditorPlatform::Cocoa
    }
    #[cfg(target_os = "linux")]
    {
        vst_audio_protocol::EditorPlatform::X11
    }
}

pub mod vst2_events;

mod vst3_events;

mod editor_probe;
mod editor_scaling;
