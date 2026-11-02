#![allow(unsafe_code)]
//! One UI-thread connection shared with the view; no nested compositor.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    fs::File,
    io::{self, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd},
    rc::Rc,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::WaylandError,
    delegate_noop,
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::{
    activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1},
    decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1},
    shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
// A full socket or an empty read is not a broken connection: queued requests
// go out on the next flush and events arrive on the next read.
fn nonfatal<T: Default>(result: Result<T, WaylandError>) -> io::Result<T> {
    match result {
        Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => Ok(T::default()),
        result => result.map_err(err),
    }
}
fn flush(connection: &Connection) -> io::Result<()> {
    nonfatal(connection.flush())
}

#[derive(Default)]
pub(crate) struct WaylandHost {
    connection: RefCell<Option<Rc<Session>>>,
    references: Cell<usize>,
    // A connection replaced while a plugin still held its display, with the
    // plugin's open count. It is released once the plugin closes it; freeing it
    // earlier would leave the plugin a dangling pointer.
    retired: RefCell<Vec<(Rc<Session>, usize)>>,
}
impl WaylandHost {
    pub fn connection(&self) -> io::Result<Connection> {
        Ok(self.session()?.connection.clone())
    }
    fn session(&self) -> io::Result<Rc<Session>> {
        if let Some(session) = self.connection.borrow().as_ref() {
            return Ok(session.clone());
        }
        if !crate::linux_editor::wayland_allowed() || !crate::linux_editor::wayland_session() {
            return Err(err("no Wayland session is available"));
        }
        // connect_to_env would consume WAYLAND_SOCKET and remove it from the
        // process environment, which is unsound once plugin threads may be
        // reading `environ`. The application strips the variable before spawning
        // the worker; a worker that still sees it refuses rather than mutate.
        if std::env::var_os("WAYLAND_SOCKET").is_some() {
            return Err(err(
                "WAYLAND_SOCKET is not supported by the worker; use WAYLAND_DISPLAY",
            ));
        }
        let connection = Connection::connect_to_env().map_err(err)?;
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let session = Rc::new(Session {
            inner: RefCell::new(Inner {
                queue,
                state: State::default(),
            }),
            _registry: registry,
            connection,
        });
        {
            let mut inner = session.inner.borrow_mut();
            let Inner { queue, state } = &mut *inner;
            queue.roundtrip(state).map_err(err)?;
        }
        *self.connection.borrow_mut() = Some(session.clone());
        Ok(session)
    }
    pub fn open(&self) -> *mut c_void {
        match self.connection() {
            Ok(c) => {
                self.references.set(self.references.get().saturating_add(1));
                c.backend().display_ptr().cast()
            }
            Err(_) => std::ptr::null_mut(),
        }
    }
    pub fn close(&self, display: *mut c_void) -> bool {
        let mut retired = self.retired.borrow_mut();
        if let Some(index) = retired
            .iter()
            .position(|(c, _)| c.connection.backend().display_ptr().cast::<c_void>() == display)
        {
            retired[index].1 -= 1;
            if retired[index].1 == 0 {
                retired.swap_remove(index);
            }
            return true;
        }
        drop(retired);
        if self.references.get() == 0 || !self.matches(display) {
            return false;
        }
        self.references.set(self.references.get() - 1);
        // Keep the connection through controller/view teardown. COM host references
        // keep this owner alive if a plugin still retains the host interface.
        true
    }
    pub fn matches(&self, display: *mut c_void) -> bool {
        self.connection
            .borrow()
            .as_ref()
            .is_some_and(|c| c.connection.backend().display_ptr().cast::<c_void>() == display)
    }
    // Call only after removed()/frame teardown: guest proxies still borrow this display.
    pub fn reset(&self) {
        let connection = self.connection.borrow_mut().take();
        let references = self.references.replace(0);
        if let Some(connection) = connection
            && references > 0
        {
            self.retired.borrow_mut().push((connection, references));
        }
    }
    pub fn pump(&self, run_loop: &crate::linux_runloop::RunLoop) -> io::Result<()> {
        let Some(session) = self.connection.borrow().clone() else {
            return Ok(());
        };
        let connection = &session.connection;
        let backend = connection.backend();
        let fd = backend.poll_fd().as_raw_fd();
        // The run loop has called registered foreign handlers first. Never read
        // behind such a handler: it may already have drained the socket.
        if !run_loop.has_event(fd) {
            if let Some(guard) = backend.prepare_read()
                && readable(fd)
            {
                nonfatal(guard.read())?;
            }
            // SAFETY: this owner retains the display; UI thread, pending-only dispatch never reads.
            let result = unsafe {
                (wayland_sys::client::wayland_client_handle().wl_display_dispatch_pending)(
                    backend.display_ptr(),
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        backend.dispatch_inner_queue().map_err(err)?;
        let mut inner = session.inner.borrow_mut();
        let Inner { queue, state } = &mut *inner;
        queue.dispatch_pending(state).map_err(err)?;
        flush(connection)
    }
}
fn readable(fd: i32) -> bool {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized descriptor, zero timeout; no storage escapes.
    (unsafe { libc::poll(&mut descriptor, 1, 0) }) > 0
        && descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}
#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    decorations: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    activation: Option<xdg_activation_v1::XdgActivationV1>,
    surface: Option<wl_surface::WlSurface>,
    outputs: HashMap<u32, (wl_output::WlOutput, i32, bool)>,
    preferred_scale: Option<i32>,
    configured: bool,
    closed: bool,
    pending_size: Option<(i32, i32)>,
    buffers: Vec<Buffer>,
    redraw: bool,
}
struct Buffer {
    proxy: wl_buffer::WlBuffer,
    size: (i32, i32),
    busy: bool,
}
// A released slot may be replaced when the physical size changes, never a busy slot.
fn buffer_slot(busy: impl Iterator<Item = bool>, len: usize) -> Option<usize> {
    busy.enumerate()
        .find_map(|(i, busy)| (!busy).then_some(i))
        .or_else(|| (len < 2).then_some(len))
}
impl State {
    fn scale(&self) -> i32 {
        self.preferred_scale
            .unwrap_or_else(|| {
                self.outputs
                    .values()
                    .filter(|(_, _, entered)| *entered)
                    .map(|(_, scale, _)| *scale)
                    .max()
                    // Before the first enter, use the largest advertised scale.
                    // Mapping supplies the actual output before view attachment.
                    .or_else(|| self.outputs.values().map(|(_, scale, _)| *scale).max())
                    .unwrap_or(1)
            })
            .clamp(1, 8)
    }
}
struct Inner {
    queue: EventQueue<State>,
    state: State,
}
// The registry and its globals outlive each parent window, including hide/show.
struct Session {
    inner: RefCell<Inner>,
    _registry: wl_registry::WlRegistry,
    connection: Connection,
}
impl Drop for Session {
    fn drop(&mut self) {
        let state = &mut self.inner.get_mut().state;
        if let Some(wm) = &state.wm {
            wm.destroy();
        }
        if let Some(manager) = &state.decorations {
            manager.destroy();
        }
        if let Some(activation) = &state.activation {
            activation.destroy();
        }
        for (output, _, _) in state.outputs.values() {
            if output.version() >= 3 {
                output.release();
            }
        }
        // Registry, compositor v6 and shm v1 have no destructor request.
        // Their server objects live until this connection is disconnected.
        let _ = self.connection.flush();
    }
}
pub(crate) struct Window {
    session: Rc<Session>,
    connection: Connection,
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    dimensions: Cell<(i32, i32)>,
    visible: Cell<bool>,
    scale: Cell<i32>,
    _host: Rc<WaylandHost>,
}
impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandWindow")
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}
impl Window {
    pub fn open(
        host: Rc<WaylandHost>,
        title: &str,
        width: i32,
        height: i32,
        resizable: bool,
    ) -> io::Result<Self> {
        let session = host.session()?;
        let connection = session.connection.clone();
        let mut inner = session.inner.borrow_mut();
        let Inner { queue, state } = &mut *inner;
        queue.dispatch_pending(state).map_err(err)?;
        let qh = queue.handle();
        if state.wm.is_none() || state.shm.is_none() {
            return Err(err("xdg-shell or wl_shm is unavailable"));
        }
        let compositor = state
            .compositor
            .as_ref()
            .ok_or_else(|| err("Wayland compositor is unavailable"))?;
        let surface = compositor.create_surface(&qh, ());
        let xdg = state
            .wm
            .as_ref()
            .ok_or_else(|| err("xdg-shell is unavailable"))?
            .get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title(title.into());
        toplevel.set_app_id("vst-audio-worker".into());
        if !resizable {
            toplevel.set_min_size(width, height);
            toplevel.set_max_size(width, height);
        }
        let decoration = state.decorations.as_ref().map(|manager| {
            let decoration = manager.get_toplevel_decoration(&toplevel, &qh, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            decoration
        });
        state.surface = Some(surface.clone());
        surface.commit();
        drop(inner);
        let window = Self {
            session,
            connection,
            surface,
            xdg,
            toplevel,
            decoration,
            dimensions: Cell::new((width, height)),
            visible: Cell::new(false),
            scale: Cell::new(1),
            _host: host,
        };
        window.wait_configure()?;
        window.roundtrip()?;
        window.visible.set(true);
        window.draw()?;
        // Older compositors send enter only once mapped. Synchronize it before
        // returning the parent to attached(), then redraw at the chosen scale.
        window.roundtrip()?;
        window.draw()?;
        Ok(window)
    }
    fn wait_configure(&self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.session.inner.borrow().state.configured {
            flush(&self.connection)?;
            let backend = self.connection.backend();
            if let Some(read) = backend.prepare_read()
                && readable(backend.poll_fd().as_raw_fd())
            {
                read.read().map_err(err)?;
            }
            backend.dispatch_inner_queue().map_err(err)?;
            self.dispatch()?;
            if Instant::now() >= deadline {
                return Err(err("Wayland configure timed out"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    fn roundtrip(&self) -> io::Result<()> {
        let mut inner = self.session.inner.borrow_mut();
        let Inner { queue, state } = &mut *inner;
        queue.roundtrip(state).map_err(err)?;
        Ok(())
    }
    fn dispatch(&self) -> io::Result<()> {
        let mut inner = self.session.inner.borrow_mut();
        let Inner { queue, state } = &mut *inner;
        queue.dispatch_pending(state).map_err(err)?;
        Ok(())
    }
    fn draw(&self) -> io::Result<()> {
        let mut inner = self.session.inner.borrow_mut();
        let scale = inner.state.scale();
        let (width, height) = self.dimensions.get();
        let (w, h) = (width * scale, height * scale);
        let bytes = w
            .checked_mul(h)
            .and_then(|v| v.checked_mul(4))
            .filter(|v| *v <= 256 * 1024 * 1024)
            .ok_or_else(|| err("Wayland buffer is too large"))?;
        let Some(slot) = buffer_slot(
            inner.state.buffers.iter().map(|b| b.busy),
            inner.state.buffers.len(),
        ) else {
            inner.state.redraw = true;
            return Ok(());
        };
        let slot = if slot == inner.state.buffers.len() || inner.state.buffers[slot].size != (w, h)
        {
            if slot < inner.state.buffers.len() {
                inner.state.buffers.remove(slot).proxy.destroy();
            }
            // SAFETY: static NUL-terminated name, flags create an owned anonymous file.
            let fd = unsafe { libc::memfd_create(c"vst-audio-editor".as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: memfd_create returned a new owned descriptor, transferred exactly once.
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.set_len(bytes as u64)?;
            let row = vec![0x30_u8; (w * 4) as usize];
            for _ in 0..h {
                file.write_all(&row)?;
            }
            let qh = inner.queue.handle();
            let shm = inner
                .state
                .shm
                .as_ref()
                .ok_or_else(|| err("wl_shm is unavailable"))?;
            let pool = shm.create_pool(file.as_fd(), bytes, &qh, ());
            let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, &qh, ());
            pool.destroy();
            let buffer = Buffer {
                proxy: buffer,
                size: (w, h),
                busy: false,
            };
            inner.state.buffers.push(buffer);
            inner.state.buffers.len() - 1
        } else {
            slot
        };
        let buffer = &mut inner.state.buffers[slot];
        buffer.busy = true;
        self.surface.set_buffer_scale(scale);
        self.surface.attach(Some(&buffer.proxy), 0, 0);
        self.surface.damage(0, 0, width, height);
        self.xdg.set_window_geometry(0, 0, width, height);
        self.surface.commit();
        inner.state.redraw = false;
        self.scale.set(scale);
        flush(&self.connection)
    }
    pub fn pump(&self) -> bool {
        if self.dispatch().is_err() {
            return true;
        }
        if self.visible.get()
            && (self.session.inner.borrow().state.redraw
                || self.session.inner.borrow().state.scale() != self.scale.get())
            && self.draw().is_err()
        {
            return true;
        }
        self.session.inner.borrow().state.closed
    }
    pub fn commit(&self) -> io::Result<()> {
        self.surface.commit();
        flush(&self.connection)
    }
    pub fn take_size(&self) -> Option<(i32, i32)> {
        self.session.inner.borrow_mut().state.pending_size.take()
    }
    pub fn scale(&self) -> f32 {
        self.scale.get() as f32
    }
    pub fn resize(&self, w: i32, h: i32) -> io::Result<()> {
        let previous = self.dimensions.replace((w, h));
        if self.visible.get()
            && let Err(error) = self.draw()
        {
            self.dimensions.set(previous);
            return Err(error);
        }
        Ok(())
    }
    pub fn show(&self, visible: bool) -> io::Result<()> {
        if visible {
            Ok(())
        } else {
            // The VST 3 owner must release the view before destroying its parent.
            Err(err("Wayland hiding requires view teardown"))
        }
    }
    pub fn focus(&self) -> io::Result<()> {
        let inner = self.session.inner.borrow();
        if let Some(activation) = &inner.state.activation {
            let token =
                activation.get_activation_token(&inner.queue.handle(), self.surface.clone());
            token.set_app_id("vst-audio-worker".into());
            token.set_surface(&self.surface);
            token.commit();
        }
        flush(&self.connection)
    }
    pub fn request_close(&self) -> io::Result<()> {
        self.session.inner.borrow_mut().state.closed = true;
        Ok(())
    }
    pub fn status(&self) -> io::Result<(bool, u32, u32)> {
        let (w, h) = self.dimensions.get();
        Ok((self.visible.get(), w as u32, h as u32))
    }
    pub fn surface(&self) -> *mut c_void {
        self.surface.id().as_ptr().cast()
    }
    pub fn xdg(&self) -> *mut c_void {
        self.xdg.id().as_ptr().cast()
    }
    pub fn toplevel(&self) -> *mut c_void {
        self.toplevel.id().as_ptr().cast()
    }
    pub fn matches(&self, display: *mut c_void) -> bool {
        self.connection.backend().display_ptr().cast::<c_void>() == display
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        if let Some(decoration) = &self.decoration {
            decoration.destroy();
        }
        self.toplevel.destroy();
        self.xdg.destroy();
        self.surface.destroy();
        let mut inner = self.session.inner.borrow_mut();
        let state = &mut inner.state;
        for buffer in state.buffers.drain(..) {
            buffer.proxy.destroy();
        }
        state.surface = None;
        state.preferred_scale = None;
        state.configured = false;
        state.closed = false;
        state.pending_size = None;
        state.redraw = false;
        for (_, _, entered) in state.outputs.values_mut() {
            *entered = false;
        }
        let _ = self.connection.flush();
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => s.compositor = Some(registry.bind(name, version.min(6), qh, ())),
                "wl_shm" => s.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => s.wm = Some(registry.bind(name, version.min(6), qh, ())),
                "zxdg_decoration_manager_v1" => {
                    s.decorations = Some(registry.bind(name, 1, qh, ()))
                }
                "xdg_activation_v1" => s.activation = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    s.outputs.insert(
                        name,
                        (registry.bind(name, version.min(4), qh, ()), 1, false),
                    );
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some((output, _, _)) = s.outputs.remove(&name)
                    && output.version() >= 3
                {
                    output.release();
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        s: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The connection queue may still contain events from the removed parent.
        if s.surface.as_ref() != Some(surface) {
            return;
        }
        match event {
            wl_surface::Event::PreferredBufferScale { factor } => s.preferred_scale = Some(factor),
            wl_surface::Event::Enter { output } => {
                for (o, _, entered) in s.outputs.values_mut() {
                    if *o == output {
                        *entered = true;
                    }
                }
            }
            wl_surface::Event::Leave { output } => {
                for (o, _, entered) in s.outputs.values_mut() {
                    if *o == output {
                        *entered = false;
                    }
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            for (o, scale, _) in s.outputs.values_mut() {
                if o == output {
                    *scale = factor;
                }
            }
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        s: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if xdg.is_alive()
            && let xdg_surface::Event::Configure { serial } = event
        {
            xdg.ack_configure(serial);
            s.configured = true;
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        s: &mut Self,
        toplevel: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !toplevel.is_alive() {
            return;
        }
        match event {
            xdg_toplevel::Event::Close => s.closed = true,
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                s.pending_size = Some((width, height))
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        s: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(slot) = s.buffers.iter_mut().find(|b| b.proxy == *buffer)
        {
            slot.busy = false;
        }
    }
}
impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, wl_surface::WlSurface> for State {
    fn event(
        s: &mut Self,
        proxy: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        surface: &wl_surface::WlSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            if s.surface.as_ref() == Some(surface)
                && let Some(activation) = &s.activation
            {
                activation.activate(token, surface);
            }
            proxy.destroy();
        }
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(State: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
delegate_noop!(State: ignore xdg_activation_v1::XdgActivationV1);

#[cfg(test)]
mod tests {
    use super::buffer_slot;
    #[test]
    fn background_buffers_are_bounded_and_reused() {
        assert_eq!(buffer_slot([].into_iter(), 0), Some(0));
        assert_eq!(buffer_slot([true].into_iter(), 1), Some(1));
        assert_eq!(buffer_slot([true, true].into_iter(), 2), None);
        assert_eq!(buffer_slot([true, false].into_iter(), 2), Some(1));
        assert_eq!(buffer_slot([false, true].into_iter(), 2), Some(0));
    }
}
