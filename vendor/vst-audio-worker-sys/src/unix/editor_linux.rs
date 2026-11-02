//! Worker-owned X11 floating or embedded window. No application GUI framework is involved.
use std::{
    cell::{Cell, RefCell},
    io,
    sync::Arc,
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::Connection,
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

#[derive(Debug)]
pub struct EditorWindow {
    connection: Arc<RustConnection>,
    window: u32,
    delete: u32,
    protocols: u32,
    closed: Cell<bool>,
    configured: Cell<bool>,
    parent: Option<vst_audio_protocol::ParentWindow>,
}
fn xft_scale(database: &[u8]) -> f32 {
    let Ok(database) = std::str::from_utf8(database) else {
        return 1.0;
    };
    database
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim() != "Xft.dpi" {
                return None;
            }
            let dpi = value.trim().parse::<f64>().ok()?;
            (dpi.is_finite() && dpi > 0.0).then(|| (dpi / 96.0).clamp(0.5, 4.0) as f32)
        })
        .next()
        .unwrap_or(1.0)
}
#[cfg(test)]
mod dpi_tests {
    use super::xft_scale;
    #[test]
    fn fractional_and_whitespace() {
        assert_eq!(
            xft_scale(b"! comment\n Xft.dpi : 132\nXft.antialias: 1"),
            1.375
        );
    }
    #[test]
    fn clamp() {
        assert_eq!(xft_scale(b"Xft.dpi: 12"), 0.5);
        assert_eq!(xft_scale(b"Xft.dpi: 960"), 4.0);
    }
    #[test]
    fn invalid_and_missing() {
        for value in [
            "",
            "Xft.dpi: NaN",
            "Xft.dpi: inf",
            "Xft.dpi: 0",
            "Xft.dpi: -96",
            "Xft.dpi: wrong",
            "Other.dpi: 192",
            "Xft.dpi=192",
        ] {
            assert_eq!(xft_scale(value.as_bytes()), 1.0, "{value}");
        }
        assert_eq!(xft_scale(&[0xff]), 1.0);
    }
}
fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
thread_local! {
    // Every editor window of this worker shares one connection. With one
    // connection per window, closing a window freed its resource-ID range and
    // the next window could take the ID of a top level the window manager was
    // still managing; weston's XWM then reparented a new embedded editor into
    // the stale frame of the floating editor it replaced.
    static SHARED: RefCell<Option<(Arc<RustConnection>, usize)>> = const { RefCell::new(None) };
}
fn shared() -> io::Result<(Arc<RustConnection>, usize)> {
    SHARED.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((connection, screen)) = slot.as_ref()
            && connection
                .get_input_focus()
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some()
        {
            return Ok((connection.clone(), *screen));
        }
        // A broken connection took its windows with it; start a new one.
        let (connection, screen) = x11rb::connect(None).map_err(error)?;
        let connection = Arc::new(connection);
        *slot = Some((connection.clone(), screen));
        Ok((connection, screen))
    })
}
impl EditorWindow {
    pub fn open(title: &str, width: i32, height: i32) -> io::Result<Self> {
        let (connection, screen) = shared()?;
        let screen = &connection.setup().roots[screen];
        let window = connection.generate_id().map_err(error)?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                0,
                0,
                width.clamp(1, 8192) as u16,
                height.clamp(1, 8192) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(screen.white_pixel)
                    .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        let atom = |name| -> io::Result<u32> {
            Ok(connection
                .intern_atom(false, name)
                .map_err(error)?
                .reply()
                .map_err(error)?
                .atom)
        };
        let protocols = atom(b"WM_PROTOCOLS")?;
        let delete = atom(b"WM_DELETE_WINDOW")?;
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                protocols,
                AtomEnum::ATOM,
                &[delete],
            )
            .map_err(error)?;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                title.as_bytes(),
            )
            .map_err(error)?;
        connection.map_window(window).map_err(error)?;
        connection.flush().map_err(error)?;
        let editor = Self {
            connection,
            window,
            protocols,
            delete,
            closed: Cell::new(false),
            configured: Cell::new(false),
            parent: None,
        };
        editor.wait_mapped()?;
        Ok(editor)
    }
    pub fn open_embedded(
        parent: vst_audio_protocol::ParentWindow,
        width: i32,
        height: i32,
    ) -> io::Result<Self> {
        let vst_audio_protocol::ParentWindow::X11 { window: id } = parent else {
            return Err(error("parent window platform does not match the worker"));
        };
        let id = u32::try_from(id)
            .ok()
            .filter(|id| *id != 0)
            .ok_or_else(|| error("parent window is not a window"))?;
        let (connection, _) = shared()?;
        connection
            .get_geometry(id)
            .map_err(error)?
            .reply()
            .map_err(|_| error("parent window is not a window"))?;
        // Geometry accepts pixmaps too; require an actual InputOutput window.
        let attributes = connection
            .get_window_attributes(id)
            .map_err(error)?
            .reply()
            .map_err(|_| error("parent window is not a window"))?;
        if attributes.class != WindowClass::INPUT_OUTPUT {
            return Err(error("parent window is not a window"));
        }
        connection
            .change_window_attributes(
                id,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        let window = connection.generate_id().map_err(error)?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                id,
                0,
                0,
                width.clamp(1, 8192) as u16,
                height.clamp(1, 8192) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        connection
            .map_window(window)
            .map_err(error)?
            .check()
            .map_err(error)?;
        connection.flush().map_err(error)?;
        Ok(Self {
            connection,
            window,
            delete: 0,
            protocols: 0,
            closed: Cell::new(false),
            configured: Cell::new(false),
            parent: Some(parent),
        })
    }
    pub fn scale(&self) -> f32 {
        // Query this window's root, not the default screen: embedded parents may
        // belong to another screen on the same display connection.
        let resource = (|| {
            let root = self
                .connection
                .get_geometry(self.window)
                .ok()?
                .reply()
                .ok()?
                .root;
            let atom = self
                .connection
                .intern_atom(true, b"RESOURCE_MANAGER")
                .ok()?
                .reply()
                .ok()?
                .atom;
            if atom == 0 {
                return None;
            }
            let reply = self
                .connection
                .get_property(false, root, atom, AtomEnum::STRING, 0, 262144)
                .ok()?
                .reply()
                .ok()?;
            if reply.format != 8 || reply.bytes_after != 0 {
                return None;
            }
            Some(reply.value)
        })();
        resource.as_deref().map_or(1.0, xft_scale)
    }
    pub fn parent(&self) -> Option<vst_audio_protocol::ParentWindow> {
        self.parent
    }
    pub fn handle_value(&self) -> usize {
        self.window as usize
    }
    pub fn pump(&self) -> bool {
        if self.closed.get() {
            return true;
        }
        let parent = match self.parent {
            Some(vst_audio_protocol::ParentWindow::X11 { window }) => u32::try_from(window).ok(),
            _ => None,
        };
        // The shared connection also carries events for windows this editor
        // replaced; only this window and its parent concern it.
        loop {
            let event = match self.connection.poll_for_event() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    // An exited X server is not an empty queue (audit #11 TD-34;
                    // x11_disconnect_closes_editor_and_preserves_audio).
                    eprintln!("X11 editor event polling failed: {error}");
                    self.closed.set(true);
                    break;
                }
            };
            match event {
                Event::ClientMessage(event)
                    if event.window == self.window
                        && event.type_ == self.protocols
                        && event.data.as_data32()[0] == self.delete =>
                {
                    self.closed.set(true)
                }
                // Only destruction closes an embedded editor: the application
                // may unmap its parent to minimize or switch views.
                Event::DestroyNotify(event)
                    if event.window == self.window || Some(event.window) == parent =>
                {
                    self.closed.set(true)
                }
                Event::ConfigureNotify(event) if event.window == self.window => {
                    self.configured.set(true);
                }
                _ => {}
            }
        }
        self.closed.get()
    }
    fn wait_mapped(&self) -> io::Result<()> {
        // A desktop WM redirects MapWindow asynchronously (unlike bare Xvfb).
        // Only floating windows wait; an embedded parent's visibility is external.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.parent.is_none() && !self.status()?.0 {
            if std::time::Instant::now() >= deadline {
                return Err(error("editor mapping timed out"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(())
    }
    pub fn show(&self, visible: bool) -> io::Result<()> {
        if visible {
            self.connection
                .map_window(self.window)
                .map_err(error)?
                .check()
                .map_err(error)?;
        } else {
            self.connection
                .unmap_window(self.window)
                .map_err(error)?
                .check()
                .map_err(error)?;
        }
        self.connection.flush().map_err(error)?;
        if visible {
            self.wait_mapped()?;
        }
        Ok(())
    }
    pub fn focus(&self) -> io::Result<()> {
        self.connection
            .set_input_focus(InputFocus::PARENT, self.window, x11rb::CURRENT_TIME)
            .map_err(error)?
            .check()
            .map_err(error)?;
        self.connection.flush().map_err(error)
    }
    pub fn status(&self) -> io::Result<(bool, u32, u32)> {
        let attributes = self
            .connection
            .get_window_attributes(self.window)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let rect = self
            .connection
            .get_geometry(self.window)
            .map_err(error)?
            .reply()
            .map_err(error)?;
        Ok((
            attributes.map_state == MapState::VIEWABLE,
            u32::from(rect.width),
            u32::from(rect.height),
        ))
    }
    pub fn capture(&self) -> io::Result<vst_audio_protocol::EditorImage> {
        let (visible, width, height) = self.status()?;
        if !visible || width == 0 || height == 0 || width > 2048 || height > 2048 {
            return Err(io::Error::other("editor is hidden or too large to capture"));
        }
        let image = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.window,
                0,
                0,
                width as u16,
                height as u16,
                u32::MAX,
            )
            .map_err(error)?
            .reply()
            .map_err(error)?;
        let setup = self.connection.setup();
        let format = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == image.depth)
            .ok_or_else(|| io::Error::other("unknown pixel depth"))?;
        let visual = setup
            .roots
            .iter()
            .flat_map(|s| &s.allowed_depths)
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == image.visual)
            .ok_or_else(|| io::Error::other("unknown visual"))?;
        if format.bits_per_pixel != 32 || visual.class != VisualClass::TRUE_COLOR {
            return Err(io::Error::other("capture requires 32-bit TrueColor"));
        }
        let stride = (width as usize * 32).div_ceil(format.scanline_pad as usize)
            * (format.scanline_pad as usize / 8);
        if image.data.len() < stride * height as usize {
            return Err(io::Error::other("truncated X11 image"));
        }
        let mut rgba = vec![255; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let bytes = image.data[y * stride + x * 4..y * stride + x * 4 + 4]
                    .try_into()
                    .expect("four bytes");
                let pixel = if setup.image_byte_order == ImageOrder::LSB_FIRST {
                    u32::from_le_bytes(bytes)
                } else {
                    u32::from_be_bytes(bytes)
                };
                for (index, mask) in [visual.red_mask, visual.green_mask, visual.blue_mask]
                    .into_iter()
                    .enumerate()
                {
                    if mask == 0 {
                        return Err(io::Error::other("missing TrueColor channel"));
                    }
                    rgba[(y * width as usize + x) * 4 + index] =
                        (((pixel & mask) >> mask.trailing_zeros()) * 255
                            / (mask >> mask.trailing_zeros())) as u8;
                }
            }
        }
        Ok(vst_audio_protocol::EditorImage {
            width,
            height,
            rgba,
        })
    }
    pub fn request_close(&self) -> io::Result<()> {
        self.closed.set(true);
        Ok(())
    }
    pub fn resize(&self, width: i32, height: i32) -> io::Result<()> {
        // Drain older notifications before starting this request.
        let (_, w, h) = self.status()?;
        self.pump();
        self.configured.set(false);
        let expected = (width.clamp(1, 8192) as u32, height.clamp(1, 8192) as u32);
        if (w, h) == expected {
            return Ok(());
        }
        self.connection
            .configure_window(
                self.window,
                &ConfigureWindowAux::new()
                    .width(width.clamp(1, 8192) as u32)
                    .height(height.clamp(1, 8192) as u32),
            )
            .map_err(error)?;
        self.connection.flush().map_err(error)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if self.pump() {
                return Err(error("editor closed during resize"));
            }
            if self.configured.get() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(error("editor resize timed out"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}
impl Drop for EditorWindow {
    fn drop(&mut self) {
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }
}
