//! Main-thread Linux editor selection; the application never links these backends.
use std::{cell::Cell, io};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Policy {
    #[default]
    Auto,
    Wayland,
    X11,
}
thread_local! {
    static POLICY: Cell<Policy> = const { Cell::new(Policy::Auto) };
    // Only a named display counts: the worker never consumes an inherited
    // WAYLAND_SOCKET (see WaylandHost::connection).
    static WAYLAND_SESSION: bool = std::env::var_os("WAYLAND_DISPLAY").is_some();
}
pub fn configure(value: &str, _processes: bool) {
    let _ = wayland_session();
    POLICY.set(match value {
        "wayland" => Policy::Wayland,
        "x11" => Policy::X11,
        _ => Policy::Auto,
    });
}
pub(crate) fn policy() -> Policy {
    POLICY.get()
}
pub(crate) fn wayland_allowed() -> bool {
    matches!(policy(), Policy::Auto | Policy::Wayland)
}
pub(crate) fn wayland_session() -> bool {
    WAYLAND_SESSION.with(|available| *available)
}
pub(crate) fn x11_available() -> bool {
    std::env::var_os("DISPLAY").is_some()
}
/// A Wayland session whose desktop XWayland may host X11 editors: the
/// compositor then scales them like any X11 application (`Stretch`).
pub(crate) fn desktop_xwayland() -> bool {
    wayland_session() && x11_available() && policy() != Policy::Wayland
}
pub(crate) fn x11_selection() -> io::Result<()> {
    match policy() {
        Policy::Wayland => Err(io::Error::other(
            "the plugin does not support Wayland editors",
        )),
        Policy::Auto | Policy::X11 if x11_available() => Ok(()),
        _ => Err(io::Error::other(
            "no display server is available for the editor",
        )),
    }
}
