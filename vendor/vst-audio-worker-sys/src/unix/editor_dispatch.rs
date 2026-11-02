//! Linux native window dispatch, sharing the X11 implementation for both X servers.
use std::{io, rc::Rc};
use vst_audio_protocol::{EditorImage, EditorPlatform, ParentWindow};
#[path = "editor_linux.rs"]
mod x11;
#[derive(Debug)]
pub struct EditorWindow(Backend);
#[derive(Debug)]
enum Backend {
    X11(Box<x11::EditorWindow>),
    Wayland(Box<crate::wayland_editor::Window>),
}
impl EditorWindow {
    pub fn open(title: &str, width: i32, height: i32) -> io::Result<Self> {
        crate::linux_editor::x11_selection()?;
        Ok(Self(Backend::X11(Box::new(x11::EditorWindow::open(
            title, width, height,
        )?))))
    }
    pub fn open_embedded(parent: ParentWindow, width: i32, height: i32) -> io::Result<Self> {
        if !crate::linux_editor::x11_available() {
            return Err(io::Error::other(
                "no display server is available for the editor",
            ));
        }
        Ok(Self(Backend::X11(Box::new(
            x11::EditorWindow::open_embedded(parent, width, height)?,
        ))))
    }
    pub(crate) fn open_wayland(
        host: Rc<crate::wayland_editor::WaylandHost>,
        title: &str,
        width: i32,
        height: i32,
        resizable: bool,
    ) -> io::Result<Self> {
        Ok(Self(Backend::Wayland(Box::new(
            crate::wayland_editor::Window::open(host, title, width, height, resizable)?,
        ))))
    }
    pub fn scale(&self) -> f32 {
        match &self.0 {
            Backend::X11(w) => w.scale(),
            Backend::Wayland(w) => w.scale(),
        }
    }
    pub fn platform(&self) -> EditorPlatform {
        match &self.0 {
            Backend::X11(_) => EditorPlatform::X11,
            Backend::Wayland(_) => EditorPlatform::Wayland,
        }
    }
    pub(crate) fn wayland(&self) -> Option<&crate::wayland_editor::Window> {
        match &self.0 {
            Backend::Wayland(w) => Some(w),
            _ => None,
        }
    }
    pub fn parent(&self) -> Option<ParentWindow> {
        match &self.0 {
            Backend::X11(w) => w.parent(),
            Backend::Wayland(_) => None,
        }
    }
    pub fn handle_value(&self) -> usize {
        match &self.0 {
            Backend::X11(w) => w.handle_value(),
            Backend::Wayland(w) => w.surface() as usize,
        }
    }
    pub fn pump(&self) -> bool {
        match &self.0 {
            Backend::X11(w) => w.pump(),
            Backend::Wayland(w) => w.pump(),
        }
    }
    pub fn show(&self, visible: bool) -> io::Result<()> {
        match &self.0 {
            Backend::X11(w) => w.show(visible),
            Backend::Wayland(w) => w.show(visible),
        }
    }
    pub fn focus(&self) -> io::Result<()> {
        match &self.0 {
            Backend::X11(w) => w.focus(),
            Backend::Wayland(w) => w.focus(),
        }
    }
    pub fn status(&self) -> io::Result<(bool, u32, u32)> {
        match &self.0 {
            Backend::X11(w) => w.status(),
            Backend::Wayland(w) => w.status(),
        }
    }
    pub fn resize(&self, width: i32, height: i32) -> io::Result<()> {
        match &self.0 {
            Backend::X11(w) => w.resize(width, height),
            Backend::Wayland(w) => w.resize(width, height),
        }
    }
    pub fn capture(&self) -> io::Result<EditorImage> {
        match &self.0 {
            Backend::X11(w) => w.capture(),
            Backend::Wayland(_) => Err(io::Error::other(
                "editor capture is not available for Wayland editors",
            )),
        }
    }
    pub fn request_close(&self) -> io::Result<()> {
        match &self.0 {
            Backend::X11(w) => w.request_close(),
            Backend::Wayland(w) => w.request_close(),
        }
    }
}
