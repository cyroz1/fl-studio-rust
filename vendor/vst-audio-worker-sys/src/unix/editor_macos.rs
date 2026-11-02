#![allow(unsafe_code)]
//! AppKit belongs to the worker's main thread, independent of the caller's GUI.
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{
    MainThreadMarker, MainThreadOnly,
    rc::{Retained, autoreleasepool},
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSActivityOptions, NSDate, NSDefaultRunLoopMode, NSPoint, NSProcessInfo, NSRect, NSSize,
    NSString,
};
use std::{cell::Cell, io};

/// Keeps App Nap and timer coalescing off while it lives. A worker has no
/// visible window, so without it macOS may delay the audio thread's wakeups
/// past the heartbeat deadline.
pub struct Activity(Retained<ProtocolObject<dyn NSObjectProtocol>>);
impl std::fmt::Debug for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Activity")
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        // SAFETY: the token was returned by beginActivityWithOptions:reason: and
        // is ended exactly once, here.
        unsafe { NSProcessInfo::processInfo().endActivity(&self.0) };
    }
}

pub fn initialize() -> io::Result<Activity> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("worker must start on macOS main thread"))?;
    autoreleasepool(|_| {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                | NSActivityOptions::LatencyCritical,
            &NSString::from_str("vst-audio plugin worker"),
        );
        Ok(Activity(token))
    })
}

#[derive(Debug)]
pub struct EditorWindow {
    window: Retained<NSWindow>,
    view: Retained<NSView>,
    closed: Cell<bool>,
    hidden: Cell<bool>,
}
impl EditorWindow {
    pub fn open(title: &str, width: i32, height: i32) -> io::Result<Self> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| io::Error::other("editor requires main thread"))?;
        autoreleasepool(|_| {
            let rect = NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(width.clamp(1, 8192) as f64, height.clamp(1, 8192) as f64),
            );
            // SAFETY: main-thread allocation; retain ownership and disable AppKit's
            // automatic release on close before any window can be shown.
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    rect,
                    NSWindowStyleMask::Titled
                        | NSWindowStyleMask::Closable
                        | NSWindowStyleMask::Miniaturizable,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // SAFETY: retained Rust owner is solely responsible for releasing the window.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            let view = window
                .contentView()
                .ok_or_else(|| io::Error::other("window has no content view"))?;
            window.center();
            window.makeKeyAndOrderFront(None);
            Ok(Self {
                window,
                view,
                closed: Cell::new(false),
                hidden: Cell::new(false),
            })
        })
    }
    pub fn open_embedded(
        _parent: vst_audio_protocol::ParentWindow,
        _width: i32,
        _height: i32,
    ) -> io::Result<Self> {
        Err(io::Error::other(
            "embedding into another process's window is not supported on macOS",
        ))
    }
    pub fn parent(&self) -> Option<vst_audio_protocol::ParentWindow> {
        None
    }
    pub fn handle_value(&self) -> usize {
        Retained::as_ptr(&self.view) as usize
    }
    pub fn pump(&self) -> bool {
        autoreleasepool(|_| {
            let mtm = MainThreadMarker::new().expect("editor remains on main thread");
            let app = NSApplication::sharedApplication(mtm);
            let until = NSDate::distantPast();
            // SAFETY: immutable framework global, initialized by AppKit.
            let mode = unsafe { NSDefaultRunLoopMode };
            // Bound each dispatch batch so IPC and parent-liveness checks progress.
            for _ in 0..128 {
                let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    mode,
                    true,
                ) else {
                    break;
                };
                app.sendEvent(&event);
            }
            app.updateWindows();
            self.closed.get() || (!self.hidden.get() && !self.window.isVisible())
        })
    }
    pub fn show(&self, visible: bool) -> io::Result<()> {
        self.hidden.set(!visible);
        if visible {
            self.window.orderFront(None);
        } else {
            self.window.orderOut(None);
        }
        Ok(())
    }
    pub fn focus(&self) -> io::Result<()> {
        self.window.makeKeyAndOrderFront(None);
        self.hidden.set(false);
        Ok(())
    }
    pub fn status(&self) -> io::Result<(bool, u32, u32)> {
        let rect = self.view.frame();
        Ok((
            self.window.isVisible(),
            rect.size.width as u32,
            rect.size.height as u32,
        ))
    }
    pub fn capture(&self) -> io::Result<vst_audio_protocol::EditorImage> {
        let (visible, width, height) = self.status()?;
        if !visible || width == 0 || height == 0 || width > 2048 || height > 2048 {
            return Err(io::Error::other("editor is hidden or too large to capture"));
        }
        let rect = self.view.bounds();
        let bitmap = self
            .view
            .bitmapImageRepForCachingDisplayInRect(rect)
            .ok_or_else(|| io::Error::other("view cannot be captured"))?;
        self.view.cacheDisplayInRect_toBitmapImageRep(rect, &bitmap);
        let width = bitmap.pixelsWide() as u32;
        let height = bitmap.pixelsHigh() as u32;
        if width == 0
            || height == 0
            || width > 2048
            || height > 2048
            || bitmap.isPlanar()
            || bitmap.bitsPerSample() != 8
            || bitmap.samplesPerPixel() != 4
            || bitmap.bitsPerPixel() != 32
        {
            return Err(io::Error::other("unsupported AppKit bitmap format"));
        }
        let stride = bitmap.bytesPerRow() as usize;
        if stride < width as usize * 4 {
            return Err(io::Error::other("invalid bitmap stride"));
        }
        // SAFETY: retained bitmap owns its non-planar allocation for this call.
        let pointer = bitmap.bitmapData();
        if pointer.is_null() {
            return Err(io::Error::other("missing bitmap storage"));
        }
        let mut rgba = vec![0; width as usize * height as usize * 4];
        // SAFETY: AppKit guarantees bytesPerRow * pixelsHigh bytes for this retained bitmap.
        let bytes = unsafe { std::slice::from_raw_parts(pointer, stride * height as usize) };
        for y in 0..height as usize {
            rgba[y * width as usize * 4..(y + 1) * width as usize * 4]
                .copy_from_slice(&bytes[y * stride..y * stride + width as usize * 4]);
        }
        Ok(vst_audio_protocol::EditorImage {
            width,
            height,
            rgba,
        })
    }
    pub fn request_close(&self) -> io::Result<()> {
        // Defer destruction until the plugin receives removed/effEditClose.
        self.closed.set(true);
        Ok(())
    }
    pub fn resize(&self, width: i32, height: i32) -> io::Result<()> {
        self.window.setContentSize(NSSize::new(
            width.clamp(1, 8192) as f64,
            height.clamp(1, 8192) as f64,
        ));
        Ok(())
    }
}
impl Drop for EditorWindow {
    fn drop(&mut self) {
        self.window.close();
    }
}
