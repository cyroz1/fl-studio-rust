//! Shared policy, kept separate from the foreign view's acceptance of a scale.
use crate::{editor::EditorWindow, plugin::PluginError};
use vst_audio_protocol::{EditorScaling, ParentWindow};

pub(crate) fn validate(
    mode: EditorScaling,
    parent: Option<ParentWindow>,
) -> Result<(), PluginError> {
    if mode == EditorScaling::Stretch && !cfg!(target_os = "macos") && !stretches(parent) {
        return Err(PluginError(STRETCH_UNAVAILABLE.into()));
    }
    Ok(())
}
pub(crate) const STRETCH_UNAVAILABLE: &str = "stretched editors need a floating window on Windows \
     or in a Wayland session with the desktop's XWayland";
/// Whether the system can stretch a floating editor drawn at scale 1: a
/// GDI-scaled window on Windows, or the desktop's XWayland, which the
/// compositor scales like any X11 application (as its settings for X11
/// applications decide). A plain X server stretches nothing.
fn stretches(parent: Option<ParentWindow>) -> bool {
    #[cfg(target_os = "linux")]
    {
        parent.is_none() && crate::linux_editor::desktop_xwayland()
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(windows) && parent.is_none()
    }
}
pub(crate) fn requests_scale(mode: EditorScaling) -> bool {
    matches!(mode, EditorScaling::Auto | EditorScaling::Plugin)
}
pub(crate) fn fallback(mode: EditorScaling, parent: Option<ParentWindow>) -> EditorScaling {
    if (cfg!(windows) && parent.is_none() && mode == EditorScaling::Auto)
        || (mode == EditorScaling::Stretch && stretches(parent))
    {
        EditorScaling::Stretch
    } else {
        EditorScaling::Off
    }
}
pub(crate) fn open(
    title: &str,
    parent: Option<ParentWindow>,
    width: i32,
    height: i32,
    mode: EditorScaling,
) -> Result<EditorWindow, PluginError> {
    validate(mode, parent)?;
    match parent {
        Some(parent) => EditorWindow::open_embedded(parent, width, height)
            .map_err(|e| PluginError(e.to_string())),
        None => {
            #[cfg(windows)]
            let result =
                EditorWindow::open_scaled(title, width, height, mode == EditorScaling::Stretch);
            #[cfg(not(windows))]
            let result = EditorWindow::open(title, width, height);
            result.map_err(|e| PluginError(e.to_string()))
        }
    }
}
pub(crate) fn platform_scale(window: &EditorWindow) -> f32 {
    #[cfg(not(target_os = "macos"))]
    {
        window.scale()
    }
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        1.0
    }
}
#[cfg(windows)]
pub(crate) fn scope(window: &EditorWindow) -> crate::editor::DpiScope {
    window.dpi_scope()
}
#[cfg(not(windows))]
pub(crate) struct DpiScope;
#[cfg(not(windows))]
pub(crate) fn scope(_window: &EditorWindow) -> DpiScope {
    DpiScope
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_auto_and_plugin_request_scale() {
        assert!(requests_scale(EditorScaling::Auto));
        assert!(requests_scale(EditorScaling::Plugin));
        assert!(!requests_scale(EditorScaling::Off));
        assert!(!requests_scale(EditorScaling::Stretch));
    }
    #[test]
    fn plugin_and_off_never_fall_back_to_stretch() {
        assert_eq!(fallback(EditorScaling::Plugin, None), EditorScaling::Off);
        assert_eq!(fallback(EditorScaling::Off, None), EditorScaling::Off);
        assert_eq!(
            fallback(EditorScaling::Auto, Some(ParentWindow::X11 { window: 1 })),
            EditorScaling::Off
        );
    }
}
