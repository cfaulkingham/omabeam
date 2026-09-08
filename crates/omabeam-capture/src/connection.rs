use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use crate::{
    Rect,
    capture::Slot,
    pixels::BufferSpec,
    selector::{Seat, Selector},
};
use anyhow::{Context, Result, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
pub(crate) use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
};
pub(crate) use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
pub(crate) use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1 as top, ext_foreign_toplevel_list_v1 as tops,
    },
    image_capture_source::v1::client::{
        ext_foreign_toplevel_image_capture_source_manager_v1 as top_source,
        ext_image_capture_source_v1 as source,
        ext_output_image_capture_source_manager_v1 as output_source,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as copy,
        ext_image_copy_capture_session_v1 as session,
    },
};
pub(crate) use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1 as cursor_device, wp_cursor_shape_manager_v1 as cursor_manager,
};
pub(crate) use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1 as xdg_outputs, zxdg_output_v1 as xdg_output,
};
pub(crate) use wayland_protocols_wlr::{
    layer_shell::v1::client::{
        zwlr_layer_shell_v1 as layer_shell, zwlr_layer_surface_v1 as layer_surface,
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1 as wlr_frame, zwlr_screencopy_manager_v1 as wlr_copy,
    },
};

pub(crate) const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct Output {
    pub proxy: wl_output::WlOutput,
    pub name: String,
    pub position: (i32, i32),
    pub mode: (u32, u32),
    pub logical_size: Option<(u32, u32)>,
    pub scale: i32,
    pub transform: wl_output::Transform,
    pub xdg: Option<xdg_output::ZxdgOutputV1>,
}

impl Output {
    pub fn rect(&self) -> Rect {
        let (mut width, mut height) = self.mode;
        if matches!(
            self.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        ) {
            std::mem::swap(&mut width, &mut height);
        }
        let (width, height) = self.logical_size.unwrap_or((
            width / self.scale.max(1) as u32,
            height / self.scale.max(1) as u32,
        ));
        Rect {
            x: self.position.0,
            y: self.position.1,
            width,
            height,
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub outputs: HashMap<u32, Output>,
    pub toplevels:
        HashMap<wayland_client::backend::ObjectId, (top::ExtForeignToplevelHandleV1, String)>,
    pub shm: Option<wl_shm::WlShm>,
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub output_source: Option<output_source::ExtOutputImageCaptureSourceManagerV1>,
    pub top_source: Option<top_source::ExtForeignToplevelImageCaptureSourceManagerV1>,
    pub copy: Option<copy::ExtImageCopyCaptureManagerV1>,
    pub wlr_copy: Option<wlr_copy::ZwlrScreencopyManagerV1>,
    pub xdg_outputs: Option<xdg_outputs::ZxdgOutputManagerV1>,
    pub layer_shell: Option<layer_shell::ZwlrLayerShellV1>,
    pub cursor_manager: Option<cursor_manager::WpCursorShapeManagerV1>,
    pub seats: HashMap<u32, Seat>,
    pub selecting: bool,
    pub selector: Option<Selector>,
    pub slots: Vec<Slot>,
    pub error: Option<anyhow::Error>,
    synced: u64,
}

pub(crate) struct Runtime {
    pub conn: Connection,
    pub queue: wayland_client::EventQueue<State>,
    pub state: State,
    sync_id: u64,
}

impl Runtime {
    pub fn connect(selecting: bool) -> Result<Self> {
        Self::from_connection(
            Connection::connect_to_env().context("screen capture requires a Wayland session")?,
            selecting,
        )
    }

    pub fn from_connection(conn: Connection, selecting: bool) -> Result<Self> {
        let queue = conn.new_event_queue();
        conn.display().get_registry(&queue.handle(), ());
        let mut this = Self {
            conn,
            queue,
            state: State {
                selecting,
                ..State::default()
            },
            sync_id: 0,
        };
        this.sync()?;
        this.state.bind_xdg_outputs(&this.queue.handle());
        this.sync()?;
        this.state
            .shm
            .as_ref()
            .context("Wayland compositor does not provide shared-memory buffers")?;
        Ok(this)
    }

    pub fn sync(&mut self) -> Result<()> {
        self.sync_id += 1;
        self.conn.display().sync(&self.queue.handle(), self.sync_id);
        let deadline = Instant::now() + SETUP_TIMEOUT;
        while self.state.synced < self.sync_id {
            if Instant::now() >= deadline {
                bail!("Wayland compositor did not respond within 5 seconds");
            }
            self.pump(deadline.saturating_duration_since(Instant::now()))?;
        }
        Ok(())
    }

    /// A bounded event dispatch; never uses blocking_dispatch/roundtrip, so
    /// idle capture sessions can be cancelled and a wedged compositor times out.
    pub fn pump(&mut self, timeout: Duration) -> Result<()> {
        let dispatched = self
            .queue
            .dispatch_pending(&mut self.state)
            .context("Wayland event dispatch failed")?;
        self.check_error()?;
        let flush_blocked = match self.conn.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(e) => return Err(e).context("Wayland connection failed"),
        };
        if dispatched > 0 {
            return Ok(());
        }
        if let Some(guard) = self.queue.prepare_read() {
            let flags = PollFlags::IN
                | if flush_blocked {
                    PollFlags::OUT
                } else {
                    PollFlags::empty()
                };
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, flags)];
            let timeout = Timespec::try_from(timeout)?;
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => return Ok(()),
                Err(rustix::io::Errno::INTR) => return Ok(()),
                Err(e) => return Err(e).context("Wayland socket polling failed"),
                _ => {}
            }
            let events = fds[0].revents();
            if events.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
                bail!("Wayland compositor disconnected");
            }
            if events.contains(PollFlags::IN) {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e).context("Wayland connection failed"),
                }
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .context("Wayland event dispatch failed")?;
        self.check_error()
    }

    pub fn check_error(&mut self) -> Result<()> {
        if let Some(e) = self.state.error.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl State {
    fn bind_xdg_outputs(&mut self, qh: &QueueHandle<Self>) {
        if let Some(manager) = &self.xdg_outputs {
            for (id, output) in &mut self.outputs {
                if output.xdg.is_none() {
                    output.xdg = Some(manager.get_xdg_output(&output.proxy, qh, *id));
                }
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
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
            } => {
                macro_rules! bind {
                    ($field:ident, $ty:ty, $max:expr) => {
                        state.$field =
                            Some(registry.bind::<$ty, _, _>(name, version.min($max), qh, ()))
                    };
                }
                match interface.as_str() {
                    "wl_shm" => bind!(shm, wl_shm::WlShm, 1),
                    "wl_compositor" => bind!(compositor, wl_compositor::WlCompositor, 4),
                    "ext_output_image_capture_source_manager_v1" => bind!(
                        output_source,
                        output_source::ExtOutputImageCaptureSourceManagerV1,
                        1
                    ),
                    "ext_foreign_toplevel_image_capture_source_manager_v1" => bind!(
                        top_source,
                        top_source::ExtForeignToplevelImageCaptureSourceManagerV1,
                        1
                    ),
                    "ext_image_copy_capture_manager_v1" => {
                        bind!(copy, copy::ExtImageCopyCaptureManagerV1, 1)
                    }
                    "zwlr_screencopy_manager_v1" => {
                        bind!(wlr_copy, wlr_copy::ZwlrScreencopyManagerV1, 3)
                    }
                    "zxdg_output_manager_v1" => {
                        bind!(xdg_outputs, xdg_outputs::ZxdgOutputManagerV1, 3);
                        state.bind_xdg_outputs(qh);
                    }
                    "zwlr_layer_shell_v1" if state.selecting => {
                        bind!(layer_shell, layer_shell::ZwlrLayerShellV1, 4)
                    }
                    "wp_cursor_shape_manager_v1" if state.selecting => {
                        bind!(cursor_manager, cursor_manager::WpCursorShapeManagerV1, 1)
                    }
                    "ext_foreign_toplevel_list_v1" if !state.selecting => {
                        registry.bind::<tops::ExtForeignToplevelListV1, _, _>(name, 1, qh, ());
                    }
                    "wl_output" => {
                        if state.selector.is_some() {
                            state.error = Some(anyhow::anyhow!(
                                "region selection cancelled: display layout changed"
                            ));
                        }
                        let proxy = registry.bind::<wl_output::WlOutput, _, _>(
                            name,
                            version.min(4),
                            qh,
                            name,
                        );
                        state.outputs.insert(
                            name,
                            Output {
                                proxy,
                                name: String::new(),
                                position: (0, 0),
                                mode: (0, 0),
                                logical_size: None,
                                scale: 1,
                                transform: wl_output::Transform::Normal,
                                xdg: None,
                            },
                        );
                        state.bind_xdg_outputs(qh);
                    }
                    "wl_seat" if state.selecting => {
                        let proxy =
                            registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, name);
                        state.seats.insert(name, Seat::new(proxy));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    if let Some(xdg) = output.xdg {
                        xdg.destroy();
                    }
                    if output.proxy.version() >= 3 {
                        output.proxy.release();
                    }
                    if state.selecting || state.slots.iter().any(|s| s.output == Some(name)) {
                        state.error = Some(anyhow::anyhow!("selected output was disconnected"));
                    }
                }
                if state.seats.remove(&name).is_some() && state.selecting {
                    state.error = Some(anyhow::anyhow!(
                        "region selection cancelled: input device disconnected"
                    ));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = *id;
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x,
                y,
                transform: WEnum::Value(transform),
                ..
            } => {
                if output.xdg.is_none() {
                    output.position = (x, y);
                }
                output.transform = transform;
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                output.mode = (width.max(0) as u32, height.max(0) as u32);
            }
            wl_output::Event::Scale { factor } => output.scale = factor.max(1),
            wl_output::Event::Name { name } => output.name = name,
            _ => {}
        }
    }
}

impl Dispatch<xdg_output::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &xdg_output::ZxdgOutputV1,
        event: xdg_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            xdg_output::Event::LogicalPosition { x, y } => output.position = (x, y),
            xdg_output::Event::LogicalSize { width, height } => {
                output.logical_size = Some((width.max(0) as u32, height.max(0) as u32))
            }
            xdg_output::Event::Name { name } => output.name = name,
            _ => {}
        }
    }
}

impl Dispatch<tops::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &tops::ExtForeignToplevelListV1,
        event: tops::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let tops::Event::Toplevel { toplevel } = event {
            state
                .toplevels
                .insert(toplevel.id(), (toplevel, String::new()));
        }
    }
    wayland_client::event_created_child!(State, tops::ExtForeignToplevelListV1, [0 => (top::ExtForeignToplevelHandleV1, ())]);
}

impl Dispatch<top::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &top::ExtForeignToplevelHandleV1,
        event: top::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            top::Event::Identifier { identifier } => {
                if let Some(top) = state.toplevels.get_mut(&proxy.id()) {
                    top.1 = identifier;
                }
            }
            top::Event::Closed => {
                state.toplevels.remove(&proxy.id());
                proxy.destroy();
            }
            _ => {}
        }
    }
}
/// Anonymous, compositor-mapped storage. File I/O keeps memory access safe and
/// obeys the protocol's ownership: only read after ready, only draw after release.
pub(crate) struct Buffer {
    pub proxy: wl_buffer::WlBuffer,
    pub spec: BufferSpec,
    file: File,
    pub busy: bool,
}
impl Buffer {
    pub fn new(shm: &wl_shm::WlShm, spec: BufferSpec, qh: &QueueHandle<State>) -> Result<Self> {
        let size = spec.byte_len()?;
        let file = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => tempfile::tempfile_in(dir),
            None => tempfile::tempfile(),
        }
        .context("could not allocate Wayland shared-memory buffer")?;
        file.set_len(size as u64)?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let proxy = pool.create_buffer(
            0,
            spec.width as i32,
            spec.height as i32,
            spec.stride as i32,
            spec.format,
            qh,
            (),
        );
        pool.destroy();
        Ok(Self {
            proxy,
            spec,
            file,
            busy: false,
        })
    }
    pub fn read(&self) -> Result<Vec<u8>> {
        let mut bytes = vec![0; self.spec.byte_len()?];
        self.file.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() != self.spec.byte_len()? {
            bail!("incorrect Wayland buffer size");
        }
        self.file.write_all_at(bytes, 0)?;
        Ok(())
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(selector) = &mut state.selector
        {
            for overlay in &mut selector.overlays {
                for buffer in &mut overlay.buffers {
                    if buffer.proxy == *proxy {
                        buffer.busy = false;
                    }
                }
            }
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore output_source::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore top_source::ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(State: ignore source::ExtImageCaptureSourceV1);
delegate_noop!(State: ignore copy::ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore wlr_copy::ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore xdg_outputs::ZxdgOutputManagerV1);
delegate_noop!(State: ignore layer_shell::ZwlrLayerShellV1);
delegate_noop!(State: ignore cursor_manager::WpCursorShapeManagerV1);
delegate_noop!(State: ignore cursor_device::WpCursorShapeDeviceV1);
