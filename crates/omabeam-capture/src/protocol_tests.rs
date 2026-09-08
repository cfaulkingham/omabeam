//! An in-process Wayland compositor exercises real protocol dispatch and FD
//! passing. This runs on Unix without a desktop, GPU, or external screenshot tool.
use crate::{CaptureSession, CaptureTarget, Rect, Region, connection::Runtime};
use std::{
    fs::File,
    os::unix::{fs::FileExt, net::UnixStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use wayland_protocols::{
    ext::{
        foreign_toplevel_list::v1::server::{
            ext_foreign_toplevel_handle_v1 as top, ext_foreign_toplevel_list_v1 as tops,
        },
        image_capture_source::v1::server::{
            ext_foreign_toplevel_image_capture_source_manager_v1 as top_source,
            ext_image_capture_source_v1 as source,
            ext_output_image_capture_source_manager_v1 as output_source,
        },
        image_copy_capture::v1::server::{
            ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as copy,
            ext_image_copy_capture_session_v1 as session,
        },
    },
    xdg::xdg_output::zv1::server::{
        zxdg_output_manager_v1 as xdg_outputs, zxdg_output_v1 as xdg_output,
    },
};
use wayland_protocols_wlr::{
    layer_shell::v1::server::{
        zwlr_layer_shell_v1 as layer_shell, zwlr_layer_surface_v1 as layer_surface,
    },
    screencopy::v1::server::{
        zwlr_screencopy_frame_v1 as wlr_frame, zwlr_screencopy_manager_v1 as wlr_copy,
    },
};
use wayland_server::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm,
    wl_shm_pool, wl_surface, wl_touch,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

#[derive(Clone, Copy, Default)]
enum SelectionInput {
    #[default]
    None,
    Drag,
    Escape,
    RightClick,
    Touch,
    EscapeThenRelease,
    RightClickThenRelease,
    ConfirmThenEscape,
    TouchCancelThenUp,
}
#[derive(Clone, Copy)]
struct Config {
    ext: bool,
    idle: bool,
    unsupported: bool,
    input: SelectionInput,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            ext: true,
            idle: false,
            unsupported: false,
            input: SelectionInput::None,
        }
    }
}
#[derive(Default, Debug)]
struct Stats {
    sessions: usize,
    buffers: usize,
    frames: usize,
    destroyed_frames: usize,
    destroyed_layers: usize,
    window_sources: usize,
    cursor: bool,
}
#[derive(Clone)]
struct OutputData {
    name: &'static str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}
struct Pool(Arc<File>);
struct BufferData {
    file: Arc<File>,
    offset: u64,
    width: u32,
    height: u32,
    stride: u32,
}
#[derive(Default)]
struct SessionData {
    active: Arc<AtomicBool>,
}
#[derive(Default)]
struct FrameData {
    active: Arc<AtomicBool>,
    buffer: Mutex<Option<wl_buffer::WlBuffer>>,
    damaged: AtomicBool,
}
#[derive(Default)]
struct SurfaceData {
    buffer: Mutex<Option<wl_buffer::WlBuffer>>,
}
struct LayerData {
    surface: wl_surface::WlSurface,
    output: OutputData,
}
enum Command {
    Damage,
    Resize,
    Stop,
    RejectBuffer,
    Resume,
}
struct Server {
    config: Config,
    stats: Arc<Mutex<Stats>>,
    sessions: Vec<session::ExtImageCopyCaptureSessionV1>,
    pending: Vec<frame::ExtImageCopyCaptureFrameV1>,
    size: (u32, u32),
    color: u32,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    touch: Option<wl_touch::WlTouch>,
    layers: Vec<layer_surface::ZwlrLayerSurfaceV1>,
    sent_input: bool,
}
struct Fixture {
    conn: Option<wayland_client::Connection>,
    stats: Arc<Mutex<Stats>>,
    command: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(config: Config) -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let mut display = Display::<Server>::new().unwrap();
        let mut handle = display.handle();
        handle.insert_client(server_socket, Arc::new(())).unwrap();
        handle.create_global::<Server, wl_shm::WlShm, _>(1, ());
        for output in [
            OutputData {
                name: "LEFT",
                x: -100,
                y: 50,
                width: 100,
                height: 80,
            },
            OutputData {
                name: "RIGHT",
                x: 0,
                y: 50,
                width: 100,
                height: 80,
            },
        ] {
            handle.create_global::<Server, wl_output::WlOutput, _>(4, output);
        }
        handle.create_global::<Server, xdg_outputs::ZxdgOutputManagerV1, _>(3, ());
        handle.create_global::<Server, wl_compositor::WlCompositor, _>(4, ());
        handle.create_global::<Server, wl_seat::WlSeat, _>(5, ());
        handle.create_global::<Server, layer_shell::ZwlrLayerShellV1, _>(4, ());
        if config.ext {
            handle.create_global::<Server, tops::ExtForeignToplevelListV1, _>(1, ());
            handle.create_global::<Server, output_source::ExtOutputImageCaptureSourceManagerV1, _>(
                1,
                (),
            );
            handle.create_global::<Server, top_source::ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, ());
            handle.create_global::<Server, copy::ExtImageCopyCaptureManagerV1, _>(1, ());
        } else {
            handle.create_global::<Server, wlr_copy::ZwlrScreencopyManagerV1, _>(3, ());
        }
        let stats = Arc::new(Mutex::new(Stats::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (command, rx) = mpsc::channel();
        let mut state = Server {
            config,
            stats: stats.clone(),
            sessions: Vec::new(),
            pending: Vec::new(),
            size: (100, 80),
            color: 0x00ff0000,
            pointer: None,
            keyboard: None,
            touch: None,
            layers: Vec::new(),
            sent_input: false,
        };
        let running = stop.clone();
        let thread = thread::spawn(move || {
            while !running.load(Ordering::SeqCst) {
                display.dispatch_clients(&mut state).unwrap();
                while let Ok(command) = rx.try_recv() {
                    match command {
                        Command::Damage => {
                            state.color = 0x000000ff;
                            for f in std::mem::take(&mut state.pending) {
                                state.complete(&f);
                            }
                        }
                        Command::Resize => {
                            state.size = (60, 40);
                            for session in &state.sessions {
                                state.constraints(session);
                            }
                            for frame in std::mem::take(&mut state.pending) {
                                frame.failed(frame::FailureReason::BufferConstraints);
                            }
                        }
                        Command::RejectBuffer => {
                            for frame in std::mem::take(&mut state.pending) {
                                frame.failed(frame::FailureReason::BufferConstraints);
                            }
                        }
                        Command::Resume => state.config.idle = false,
                        Command::Stop => {
                            for session in &state.sessions {
                                session.stopped();
                            }
                        }
                    }
                }
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            conn: Some(wayland_client::Connection::from_socket(client_socket).unwrap()),
            stats,
            command,
            stop,
            thread: Some(thread),
        }
    }
    fn runtime(&mut self, selecting: bool) -> Runtime {
        Runtime::from_connection(self.conn.take().unwrap(), selecting).unwrap()
    }
    fn capture(&mut self, target: CaptureTarget) -> CaptureSession {
        CaptureSession::with_runtime(self.runtime(false), target).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Err(error) = self.thread.take().unwrap().join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}
impl Server {
    fn constraints(&self, session: &session::ExtImageCopyCaptureSessionV1) {
        session.buffer_size(self.size.0, self.size.1);
        session.shm_format(if self.config.unsupported {
            wl_shm::Format::Nv12
        } else {
            wl_shm::Format::Xrgb8888
        });
        session.done();
    }
    fn write_buffer(&self, buffer: &wl_buffer::WlBuffer) {
        let b = buffer.data::<BufferData>().unwrap();
        assert_eq!((b.width, b.height), self.size);
        let mut bytes = vec![0; b.stride as usize * b.height as usize];
        for y in 0..b.height {
            for x in 0..b.width {
                let offset = (y * b.stride + x * 4) as usize;
                bytes[offset..offset + 4].copy_from_slice(&self.color.to_ne_bytes());
            }
        }
        b.file.write_all_at(&bytes, b.offset).unwrap();
    }
    fn complete(&self, frame: &frame::ExtImageCopyCaptureFrameV1) {
        let data = frame.data::<FrameData>().unwrap();
        assert!(
            data.damaged.load(Ordering::SeqCst),
            "client must damage a new buffer"
        );
        let guard = data.buffer.lock().unwrap();
        let buffer = guard.as_ref().unwrap();
        let spec = buffer.data::<BufferData>().unwrap();
        if (spec.width, spec.height) != self.size {
            // Constraints may race a client's in-flight request. A compositor
            // rejects that buffer and permits retry after renegotiation.
            frame.failed(frame::FailureReason::BufferConstraints);
            return;
        }
        self.write_buffer(buffer);
        frame.transform(wl_output::Transform::Normal);
        frame.damage(0, 0, self.size.0 as i32, self.size.1 as i32);
        frame.presentation_time(0, 1, 0);
        frame.ready();
    }
    fn maybe_input(&mut self) {
        if self.sent_input || self.layers.len() != 2 {
            return;
        }
        if !self.layers.iter().all(|l| {
            l.data::<LayerData>()
                .unwrap()
                .surface
                .data::<SurfaceData>()
                .unwrap()
                .buffer
                .lock()
                .unwrap()
                .is_some()
        }) {
            return;
        }
        let layer = self
            .layers
            .iter()
            .find(|l| l.data::<LayerData>().unwrap().output.name == "LEFT")
            .unwrap();
        let surface = &layer.data::<LayerData>().unwrap().surface;
        match self.config.input {
            SelectionInput::None => return,
            SelectionInput::Drag => {
                let p = self.pointer.as_ref().unwrap();
                p.enter(1, surface, 10.0, 10.0);
                p.button(2, 0, 0x110, wl_pointer::ButtonState::Pressed);
                p.motion(1, 29.0, 24.0);
                p.button(3, 2, 0x110, wl_pointer::ButtonState::Released);
            }
            SelectionInput::EscapeThenRelease
            | SelectionInput::RightClickThenRelease
            | SelectionInput::ConfirmThenEscape => {
                let p = self.pointer.as_ref().unwrap();
                let k = self.keyboard.as_ref().unwrap();
                p.enter(1, surface, 10.0, 10.0);
                p.button(2, 0, 0x110, wl_pointer::ButtonState::Pressed);
                p.motion(1, 29.0, 24.0);
                match self.config.input {
                    SelectionInput::EscapeThenRelease => {
                        k.key(3, 2, 1, wl_keyboard::KeyState::Pressed)
                    }
                    SelectionInput::RightClickThenRelease => {
                        p.button(3, 2, 0x111, wl_pointer::ButtonState::Pressed)
                    }
                    _ => {}
                }
                p.button(4, 3, 0x110, wl_pointer::ButtonState::Released);
                if matches!(self.config.input, SelectionInput::ConfirmThenEscape) {
                    k.key(5, 4, 1, wl_keyboard::KeyState::Pressed);
                }
            }
            SelectionInput::TouchCancelThenUp => {
                let t = self.touch.as_ref().unwrap();
                t.down(1, 0, surface, 0, 10.0, 10.0);
                t.motion(1, 0, 29.0, 24.0);
                t.cancel();
                t.up(2, 2, 0);
            }
            SelectionInput::Escape => {
                self.keyboard
                    .as_ref()
                    .unwrap()
                    .key(1, 0, 1, wl_keyboard::KeyState::Pressed)
            }
            SelectionInput::RightClick => {
                let p = self.pointer.as_ref().unwrap();
                p.enter(1, surface, 10.0, 10.0);
                p.button(2, 0, 0x111, wl_pointer::ButtonState::Pressed);
            }
            SelectionInput::Touch => {
                let t = self.touch.as_ref().unwrap();
                t.down(1, 0, surface, 0, 10.0, 10.0);
                t.motion(1, 0, 29.0, 24.0);
                t.up(2, 2, 0);
            }
        }
        self.sent_input = true;
    }
}

macro_rules! global {
    ($ty:ty) => {
        impl GlobalDispatch<$ty, ()> for Server {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$ty>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(resource, ());
            }
        }
    };
}
macro_rules! noop {
    ($ty:ty, $data:ty) => {
        impl Dispatch<$ty, $data> for Server {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$ty,
                _: <$ty as Resource>::Request,
                _: &$data,
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}
global!(wl_shm::WlShm);
global!(xdg_outputs::ZxdgOutputManagerV1);
global!(output_source::ExtOutputImageCaptureSourceManagerV1);
global!(top_source::ExtForeignToplevelImageCaptureSourceManagerV1);
global!(copy::ExtImageCopyCaptureManagerV1);
global!(wlr_copy::ZwlrScreencopyManagerV1);
global!(wl_compositor::WlCompositor);
global!(layer_shell::ZwlrLayerShellV1);
noop!(wl_output::WlOutput, OutputData);
noop!(wl_buffer::WlBuffer, BufferData);
noop!(xdg_output::ZxdgOutputV1, ());
noop!(top::ExtForeignToplevelHandleV1, ());
noop!(tops::ExtForeignToplevelListV1, ());
noop!(source::ExtImageCaptureSourceV1, ());
noop!(wl_keyboard::WlKeyboard, ());
noop!(wl_pointer::WlPointer, ());
noop!(wl_touch::WlTouch, ());
noop!(wl_region::WlRegion, ());

impl GlobalDispatch<wl_output::WlOutput, OutputData> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_output::WlOutput>,
        data: &OutputData,
        init: &mut DataInit<'_, Self>,
    ) {
        let output = init.init(resource, data.clone());
        output.geometry(
            data.x,
            data.y,
            0,
            0,
            wl_output::Subpixel::Unknown,
            "Test".into(),
            "Output".into(),
            wl_output::Transform::Normal,
        );
        output.mode(wl_output::Mode::Current, data.width, data.height, 60000);
        output.scale(1);
        output.name(data.name.into());
        output.done();
    }
}
impl Dispatch<xdg_outputs::ZxdgOutputManagerV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_outputs::ZxdgOutputManagerV1,
        request: xdg_outputs::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_outputs::Request::GetXdgOutput { id, output } = request {
            let data = output.data::<OutputData>().unwrap();
            let xdg = init.init(id, ());
            xdg.logical_position(data.x, data.y);
            xdg.logical_size(data.width, data.height);
            xdg.name(data.name.into());
            output.done();
        }
    }
}
impl Dispatch<wl_shm::WlShm, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, .. } = request {
            init.init(id, Pool(Arc::new(File::from(fd))));
        }
    }
}
impl Dispatch<wl_shm_pool::WlShmPool, Pool> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &Pool,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer {
            id,
            offset,
            width,
            height,
            stride,
            ..
        } = request
        {
            state.stats.lock().unwrap().buffers += 1;
            init.init(
                id,
                BufferData {
                    file: data.0.clone(),
                    offset: offset as u64,
                    width: width as u32,
                    height: height as u32,
                    stride: stride as u32,
                },
            );
        }
    }
}
impl GlobalDispatch<tops::ExtForeignToplevelListV1, ()> for Server {
    fn bind(
        _: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<tops::ExtForeignToplevelListV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let list = init.init(resource, ());
        let top = client
            .create_resource::<top::ExtForeignToplevelHandleV1, (), Self>(dh, 1, ())
            .unwrap();
        list.toplevel(&top);
        top.identifier("stable-window".into());
        top.done();
    }
}
impl Dispatch<output_source::ExtOutputImageCaptureSourceManagerV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &output_source::ExtOutputImageCaptureSourceManagerV1,
        request: output_source::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let output_source::Request::CreateSource { source, .. } = request {
            init.init(source, ());
        }
    }
}
impl Dispatch<top_source::ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &top_source::ExtForeignToplevelImageCaptureSourceManagerV1,
        request: top_source::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let top_source::Request::CreateSource { source, .. } = request {
            state.stats.lock().unwrap().window_sources += 1;
            init.init(source, ());
        }
    }
}
impl Dispatch<copy::ExtImageCopyCaptureManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &copy::ExtImageCopyCaptureManagerV1,
        request: copy::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let copy::Request::CreateSession {
            session, options, ..
        } = request
        {
            state.stats.lock().unwrap().cursor = matches!(options, wayland_server::WEnum::Value(value) if value.contains(copy::Options::PaintCursors));
            let session = init.init(session, SessionData::default());
            state.constraints(&session);
            state.sessions.push(session);
            state.stats.lock().unwrap().sessions += 1;
        }
    }
}
impl Dispatch<session::ExtImageCopyCaptureSessionV1, SessionData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &session::ExtImageCopyCaptureSessionV1,
        request: session::Request,
        data: &SessionData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let session::Request::CreateFrame { frame } = request {
            let mut stats = state.stats.lock().unwrap();
            assert!(
                !data.active.swap(true, Ordering::SeqCst),
                "at most one frame per session"
            );
            stats.frames += 1;
            init.init(
                frame,
                FrameData {
                    active: data.active.clone(),
                    ..FrameData::default()
                },
            );
        }
    }
}
impl Dispatch<frame::ExtImageCopyCaptureFrameV1, FrameData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &frame::ExtImageCopyCaptureFrameV1,
        request: frame::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            frame::Request::AttachBuffer { buffer } => *data.buffer.lock().unwrap() = Some(buffer),
            frame::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                assert_eq!((x, y), (0, 0));
                assert!(width > 0 && height > 0);
                data.damaged.store(true, Ordering::SeqCst);
            }
            frame::Request::Capture => {
                if state.config.idle && state.stats.lock().unwrap().frames > 1 {
                    state.pending.push(resource.clone());
                } else {
                    state.complete(resource);
                }
            }
            frame::Request::Destroy => {
                data.active.store(false, Ordering::SeqCst);
                state.stats.lock().unwrap().destroyed_frames += 1;
            }
            _ => {}
        }
    }
}
impl Dispatch<wlr_copy::ZwlrScreencopyManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wlr_copy::ZwlrScreencopyManagerV1,
        request: wlr_copy::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let wlr_copy::Request::CaptureOutput {
            frame,
            overlay_cursor,
            ..
        } = request
        {
            state.stats.lock().unwrap().cursor = overlay_cursor != 0;
            let frame = init.init(frame, ());
            state.stats.lock().unwrap().frames += 1;
            // Include padding to exercise the compositor-provided stride.
            frame.buffer(
                wl_shm::Format::Xrgb8888,
                state.size.0,
                state.size.1,
                state.size.0 * 4 + 16,
            );
            frame.buffer_done();
        }
    }
}
impl Dispatch<wlr_frame::ZwlrScreencopyFrameV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &wlr_frame::ZwlrScreencopyFrameV1,
        request: wlr_frame::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wlr_frame::Request::Copy { buffer } = request {
            state.write_buffer(&buffer);
            resource.flags(wlr_frame::Flags::YInvert);
            resource.ready(0, 1, 0);
        }
    }
}

impl GlobalDispatch<wl_seat::WlSeat, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_seat::WlSeat>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ()).capabilities(
            wl_seat::Capability::Pointer
                | wl_seat::Capability::Keyboard
                | wl_seat::Capability::Touch,
        );
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => state.pointer = Some(init.init(id, ())),
            wl_seat::Request::GetKeyboard { id } => state.keyboard = Some(init.init(id, ())),
            wl_seat::Request::GetTouch { id } => state.touch = Some(init.init(id, ())),
            _ => {}
        }
    }
}
impl Dispatch<wl_compositor::WlCompositor, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                init.init(id, SurfaceData::default());
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_surface::WlSurface, SurfaceData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => *data.buffer.lock().unwrap() = buffer,
            wl_surface::Request::Commit => {
                if let Some(buffer) = data.buffer.lock().unwrap().as_ref() {
                    buffer.release();
                } else if let Some(layer) = state
                    .layers
                    .iter()
                    .find(|l| l.data::<LayerData>().unwrap().surface == *resource)
                {
                    let output = &layer.data::<LayerData>().unwrap().output;
                    layer.configure(1, output.width as u32, output.height as u32);
                }
                state.maybe_input();
            }
            _ => {}
        }
    }
}
impl Dispatch<layer_shell::ZwlrLayerShellV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &layer_shell::ZwlrLayerShellV1,
        request: layer_shell::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let layer_shell::Request::GetLayerSurface {
            id,
            surface,
            output: Some(output),
            ..
        } = request
        {
            let output = output.data::<OutputData>().unwrap().clone();
            state
                .layers
                .push(init.init(id, LayerData { surface, output }));
        }
    }
}
impl Dispatch<layer_surface::ZwlrLayerSurfaceV1, LayerData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &layer_surface::ZwlrLayerSurfaceV1,
        request: layer_surface::Request,
        _: &LayerData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let layer_surface::Request::Destroy = request {
            state.stats.lock().unwrap().destroyed_layers += 1;
        }
    }
}

#[test]
fn ext_capture_reuses_session_and_buffer_and_preserves_pending_idle_frame() {
    let mut fixture = Fixture::new(Config {
        idle: true,
        ..Config::default()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    assert_eq!(
        capture.capture().unwrap().image.get_pixel(0, 0).0,
        [255, 0, 0, 255]
    );
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    {
        let stats = fixture.stats.lock().unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.buffers, 1);
        assert_eq!(stats.frames, 2);
    }
    fixture.command.send(Command::Damage).unwrap();
    assert_eq!(
        capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .image
            .get_pixel(0, 0)
            .0,
        [0, 0, 255, 255]
    );
}
#[test]
fn captures_true_window_and_reallocates_on_resize() {
    let mut fixture = Fixture::new(Config::default());
    let mut capture = fixture.capture(CaptureTarget::Toplevel("stable-window".into()));
    assert_eq!(capture.capture().unwrap().image.dimensions(), (100, 80));
    fixture.command.send(Command::Resize).unwrap();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .image
            .dimensions(),
        (60, 40)
    );
    assert_eq!(fixture.stats.lock().unwrap().window_sources, 1);
    assert_eq!(fixture.stats.lock().unwrap().buffers, 2);
}
#[test]
fn capture_source_stop_returns_error_without_fallback() {
    let mut fixture = Fixture::new(Config::default());
    let mut capture = fixture.capture(CaptureTarget::Toplevel("stable-window".into()));
    capture.capture().unwrap();
    fixture.command.send(Command::Stop).unwrap();
    thread::sleep(Duration::from_millis(20));
    assert!(
        capture
            .next_frame(Duration::from_secs(1))
            .unwrap_err()
            .to_string()
            .contains("no longer available")
    );
    assert_eq!(fixture.stats.lock().unwrap().sessions, 1);
}
#[test]
fn wlr_fallback_captures_region_and_reuses_buffer() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..Config::default()
    });
    let mut capture = fixture.capture(CaptureTarget::Region(Region {
        output: "LEFT".into(),
        rect: Rect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        },
    }));
    for _ in 0..3 {
        assert_eq!(capture.capture().unwrap().image.dimensions(), (30, 40));
    }
    assert_eq!(fixture.stats.lock().unwrap().buffers, 1);
}
#[test]
fn unsupported_window_and_pixel_formats_fail_explicitly() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..Config::default()
    });
    assert!(
        CaptureSession::with_runtime(
            fixture.runtime(false),
            CaptureTarget::Toplevel("stable-window".into())
        )
        .is_err()
    );
    let mut fixture = Fixture::new(Config {
        unsupported: true,
        ..Config::default()
    });
    assert!(
        CaptureSession::with_runtime(fixture.runtime(false), CaptureTarget::Output("LEFT".into()))
            .is_err()
    );
}
#[test]
fn drag_and_touch_selection_unmap_every_overlay_before_returning() {
    for input in [SelectionInput::Drag, SelectionInput::Touch] {
        let mut fixture = Fixture::new(Config {
            input,
            ..Config::default()
        });
        let region = crate::selector::select(fixture.runtime(true)).unwrap();
        assert_eq!(
            region,
            Region {
                output: "LEFT".into(),
                rect: Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 15
                }
            }
        );
        assert_eq!(fixture.stats.lock().unwrap().destroyed_layers, 2);
    }
}
#[test]
fn escape_and_right_click_cancel_and_unmap_every_overlay() {
    for input in [SelectionInput::Escape, SelectionInput::RightClick] {
        let mut fixture = Fixture::new(Config {
            input,
            ..Config::default()
        });
        assert!(
            crate::selector::select(fixture.runtime(true))
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert_eq!(fixture.stats.lock().unwrap().destroyed_layers, 2);
    }
}

#[test]
fn composes_a_desktop_rectangle_across_two_ext_output_sessions() {
    let mut fixture = Fixture::new(Config::default());
    let mut capture = fixture.capture(CaptureTarget::DesktopRect(Rect {
        x: -25,
        y: 60,
        width: 50,
        height: 20,
    }));
    let frame = capture.capture().unwrap();
    assert_eq!(frame.image.dimensions(), (50, 20));
    assert!(frame.image.pixels().all(|p| p.0 == [255, 0, 0, 255]));
    assert_eq!(fixture.stats.lock().unwrap().sessions, 2);
}

#[test]
fn buffer_rejection_waits_for_new_constraints_before_retrying() {
    let mut fixture = Fixture::new(Config {
        idle: true,
        ..Config::default()
    });
    let mut capture = fixture.capture(CaptureTarget::Toplevel("stable-window".into()));
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    fixture.command.send(Command::RejectBuffer).unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture.stats.lock().unwrap().frames,
        2,
        "no stale-buffer retries"
    );
    fixture.command.send(Command::Resize).unwrap();
    fixture.command.send(Command::Resume).unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_eq!(frame.image.dimensions(), (60, 40));
    assert_eq!(fixture.stats.lock().unwrap().buffers, 2);
}

#[test]
fn queued_input_cannot_overwrite_a_completed_selection_or_cancellation() {
    for input in [
        SelectionInput::EscapeThenRelease,
        SelectionInput::RightClickThenRelease,
        SelectionInput::TouchCancelThenUp,
        SelectionInput::ConfirmThenEscape,
    ] {
        let mut fixture = Fixture::new(Config {
            input,
            ..Config::default()
        });
        let result = crate::selector::select(fixture.runtime(true));
        if matches!(input, SelectionInput::ConfirmThenEscape) {
            assert_eq!(
                result.unwrap().rect,
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 15
                }
            );
        } else {
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        }
        assert_eq!(fixture.stats.lock().unwrap().destroyed_layers, 2);
    }
}

#[test]
fn cursor_setting_reaches_ext_and_wlr_capture_protocols() {
    for ext in [true, false] {
        for cursor in [true, false] {
            let mut fixture = Fixture::new(Config {
                ext,
                ..Config::default()
            });
            let mut capture = CaptureSession::with_runtime_options(
                fixture.runtime(false),
                CaptureTarget::Output("LEFT".into()),
                cursor,
            )
            .unwrap();
            capture.capture().unwrap();
            assert_eq!(fixture.stats.lock().unwrap().cursor, cursor);
        }
    }
}
