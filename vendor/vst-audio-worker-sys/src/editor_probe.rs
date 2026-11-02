#![allow(unsafe_code)]
//! Post-open evidence for plugins whose legacy EDIT_OPEN reply is zero.
#[cfg(target_os = "linux")]
pub(crate) fn has_child(handle: usize) -> bool {
    use x11rb::protocol::xproto::ConnectionExt;
    let Ok((connection, _)) = x11rb::connect(None) else {
        return false;
    };
    connection
        .query_tree(handle as u32)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|tree| !tree.children.is_empty())
}
#[cfg(windows)]
pub(crate) fn has_child(handle: usize) -> bool {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{GW_CHILD, GetWindow},
    };
    // SAFETY: handle is the worker-owned parent; query does not transfer ownership.
    unsafe { GetWindow(HWND(handle as *mut _), GW_CHILD) }.is_ok_and(|child| !child.0.is_null())
}
#[cfg(target_os = "macos")]
pub(crate) fn has_child(handle: usize) -> bool {
    use objc2::msg_send;
    use objc2_app_kit::NSView;
    use objc2_foundation::NSArray;
    // SAFETY: editor supplies its live NSView on its creating main thread.
    let children: *mut NSArray<NSView> = unsafe { msg_send![handle as *mut NSView, subviews] };
    if children.is_null() {
        return false;
    }
    // SAFETY: autoreleased subview array remains live through this immediate query.
    let count: usize = unsafe { msg_send![children, count] };
    count != 0
}
