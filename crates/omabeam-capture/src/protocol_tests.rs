//! An in-process Wayland compositor exercises real protocol dispatch and FD
//! passing. This runs on Unix without a desktop, GPU, or external screenshot tool.
use crate::{
    AlphaMode, CaptureOptions, CaptureSession, CaptureTarget, Rect, Region, StillCapturer,
    connection::Runtime,
    pixels::{self, BufferSpec},
};
use std::{
    fs::File,
    os::unix::{fs::FileExt, net::UnixStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
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
    /// A 50-motion drag; frame callbacks are held and released every 10
    /// motions, and buffers are held until the next commit.
    Paced,
}
#[derive(Clone, Copy)]
struct Config {
    ext: bool,
    idle: bool,
    unsupported: bool,
    input: SelectionInput,
    /// ext frames describe their damage (a compositor may send none).
    damage_events: bool,
    /// Every capture fails: ext as not matching the buffer constraints.
    reject: bool,
    /// The transform ext frames report.
    transform: wl_output::Transform,
    wlr_version: u32,
    /// The shm format of capture buffers. Colors are pixels of this format.
    format: wl_shm::Format,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            ext: true,
            idle: false,
            unsupported: false,
            input: SelectionInput::None,
            damage_events: true,
            reject: false,
            transform: wl_output::Transform::Normal,
            wlr_version: 3,
            format: wl_shm::Format::Xrgb8888,
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
    overlays: std::collections::HashMap<&'static str, OverlayLog>,
    window_sources: usize,
    cursor: bool,
    /// ext captures into a buffer's first use, each checked for full damage.
    fresh_captures: usize,
    /// ext captures into a captured buffer that the client still damaged.
    redamaged: usize,
    /// wlr copy and copy_with_damage requests.
    copies: usize,
    damage_copies: usize,
    /// Rectangles requested with wlr capture_output_region.
    wlr_regions: Vec<[i32; 4]>,
    /// wlr frames the client destroyed while they waited for damage.
    replaced: usize,
    /// The windows capture sources were created for, in order.
    window_ids: Vec<&'static str>,
    destroyed_sessions: usize,
    destroyed_buffers: usize,
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
    /// An ext capture has filled this buffer.
    captured: AtomicBool,
}
#[derive(Default)]
struct SessionData {
    active: Arc<AtomicBool>,
    /// Screen changes since the session's last ready: the next frame's damage.
    damage: Mutex<Vec<[i32; 4]>>,
    /// A frame was ready, so later captures may wait for damage.
    captured: AtomicBool,
}
#[derive(Default)]
struct FrameData {
    active: Arc<AtomicBool>,
    buffer: Mutex<Option<wl_buffer::WlBuffer>>,
    /// The client's damage_buffer requests.
    damage: Mutex<Vec<[i32; 4]>>,
    session: Option<session::ExtImageCopyCaptureSessionV1>,
}
impl FrameData {
    fn session(&self) -> &SessionData {
        self.session.as_ref().unwrap().data().unwrap()
    }
}
/// A wlr frame's rectangle of the screen, in buffer pixels (scale 1).
struct WlrFrameData {
    origin: (i32, i32),
    size: (u32, u32),
}
type Callback = wayland_server::protocol::wl_callback::WlCallback;
#[derive(Default)]
struct SurfaceData {
    buffer: Mutex<Option<wl_buffer::WlBuffer>>,
    /// Pending damage boxes and frame callbacks, applied at commit.
    damage: Mutex<Vec<[i32; 4]>>,
    frames: Mutex<Vec<Callback>>,
    /// Callbacks of committed frames that a paced test has not released yet.
    held: Mutex<Vec<Callback>>,
    /// The buffer a paced test holds until the next commit replaces it.
    current: Mutex<Option<wl_buffer::WlBuffer>>,
    /// What the compositor shows; see `Server::present`.
    screen: Mutex<Vec<u8>>,
    finish_on_commit: AtomicBool,
}
/// Buffer commits on one selector overlay, and what the last one showed.
#[derive(Default, Debug)]
struct OverlayLog {
    commits: usize,
    /// Commits made while an earlier frame callback was still held.
    unpaced: usize,
    /// Commits whose whole buffer differs from the image composed from damage.
    stale: usize,
    frames_done: usize,
    buffer: Vec<u8>,
    screen: Vec<u8>,
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
    /// Paint `rect` in `color`, report it as damage and release waiting frames.
    DamageRect {
        rect: [i32; 4],
        color: u32,
    },
    /// Report another transform on later ext frames.
    Transform(wl_output::Transform),
    /// Finish waiting wlr frames with a BLUE marker instead of the screen, or
    /// fail them, as a compositor might just as the client replaces them.
    WlrLate {
        ready: bool,
    },
    /// Outputs switch to a larger mode, then waiting wlr frames complete:
    /// wlroots copies the area computed at request time while it fits.
    Grow,
    /// Outputs rotate by 90°, then waiting wlr frames complete: wlroots keeps
    /// the swapchain at the mode size, so their area still fits.
    Rotate,
    /// `Rotate` for one output; every waiting wlr frame still completes.
    RotateOutput {
        name: &'static str,
    },
    /// Outputs switch to scale 2 at the same mode (half the logical size),
    /// then waiting wlr frames complete with the area they were requested for.
    Scale,
    /// An output moves in the layout (xdg-output logical position).
    Move {
        name: &'static str,
        x: i32,
    },
    /// Later wlr frames announce a wider stride for the same picture.
    Restride,
    /// Another client connects; its end of the socket is sent back.
    Connect(mpsc::Sender<UnixStream>),
    /// A window maps and is announced to every toplevel list.
    Map(&'static str),
    /// The compositor closes the last connected client's connection, then
    /// replies.
    Disconnect(mpsc::Sender<()>),
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
    /// Rectangles painted over `color`, last on top, in buffer pixels.
    painted: Vec<([i32; 4], u32)>,
    /// wlr copy_with_damage requests waiting for damage.
    wlr_pending: Vec<(wlr_frame::ZwlrScreencopyFrameV1, wl_buffer::WlBuffer)>,
    /// Bound outputs and their xdg-outputs, to announce reconfigurations.
    wl_outputs: Vec<wl_output::WlOutput>,
    xdg: Vec<(xdg_output::ZxdgOutputV1, wl_output::WlOutput)>,
    /// Bytes of padding after each wlr buffer row.
    wlr_padding: u32,
    /// Mapped windows' identifiers, and the toplevel lists that announce them.
    windows: Vec<&'static str>,
    lists: Vec<tops::ExtForeignToplevelListV1>,
    /// Clients that connected with `Command::Connect`, each with a handle
    /// on the compositor's end of its socket.
    clients: Vec<(Client, UnixStream)>,
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
            handle.create_global::<Server, wlr_copy::ZwlrScreencopyManagerV1, _>(
                config.wlr_version,
                (),
            );
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
            color: RED,
            pointer: None,
            keyboard: None,
            touch: None,
            layers: Vec::new(),
            sent_input: false,
            painted: Vec::new(),
            wlr_pending: Vec::new(),
            wl_outputs: Vec::new(),
            xdg: Vec::new(),
            wlr_padding: 16,
            windows: vec!["stable-window"],
            lists: Vec::new(),
            clients: Vec::new(),
        };
        let running = stop.clone();
        let thread = thread::spawn(move || {
            while !running.load(Ordering::SeqCst) {
                display.dispatch_clients(&mut state).unwrap();
                while let Ok(command) = rx.try_recv() {
                    match command {
                        Command::Damage => {
                            state.color = BLUE;
                            state.painted.clear();
                            state.damage_sessions(state.whole());
                            state.release();
                        }
                        Command::DamageRect { rect, color } => {
                            state.painted.push((rect, color));
                            state.damage_sessions(rect);
                            state.release();
                        }
                        Command::Transform(transform) => state.config.transform = transform,
                        Command::WlrLate { ready } => state.late_wlr(ready),
                        Command::Grow => {
                            state.size = (120, 90);
                            for output in &state.wl_outputs {
                                output.mode(wl_output::Mode::Current, 120, 90, 60000);
                            }
                            for (xdg, _) in &state.xdg {
                                xdg.logical_size(120, 90);
                            }
                            state.reconfigured();
                        }
                        Command::Rotate => state.rotate(None),
                        Command::RotateOutput { name } => state.rotate(Some(name)),
                        Command::Connect(reply) => {
                            let (client, server) = UnixStream::pair().unwrap();
                            let end = server.try_clone().unwrap();
                            let mut handle = display.handle();
                            let data = Arc::new(());
                            let id = handle.insert_client(server, data).unwrap();
                            state.clients.push((id, end));
                            reply.send(client).unwrap();
                        }
                        Command::Disconnect(reply) => {
                            let (client, end) = state.clients.pop().unwrap();
                            let closed =
                                wayland_server::backend::DisconnectReason::ConnectionClosed;
                            display
                                .handle()
                                .backend_handle()
                                .kill_client(client.id(), closed);
                            // The backend closes a killed client's socket only once
                            // some client sends a request; a compositor closes it
                            // at once.
                            end.shutdown(std::net::Shutdown::Both).unwrap();
                            reply.send(()).unwrap();
                        }
                        Command::Map(id) => {
                            state.windows.push(id);
                            for list in &state.lists {
                                announce(&display.handle(), list, id);
                            }
                        }
                        Command::Scale => {
                            for output in &state.wl_outputs {
                                output.scale(2);
                            }
                            for (xdg, output) in &state.xdg {
                                let d = output.data::<OutputData>().unwrap();
                                xdg.logical_size(d.width / 2, d.height / 2);
                            }
                            state.reconfigured();
                        }
                        Command::Move { name, x } => {
                            for (xdg, output) in &state.xdg {
                                let d = output.data::<OutputData>().unwrap();
                                if d.name == name {
                                    xdg.logical_position(x, d.y);
                                    output.done();
                                }
                            }
                        }
                        Command::Restride => state.wlr_padding = 32,
                        Command::Resize => {
                            state.size = (60, 40);
                            for session in &state.sessions {
                                let damage = &session.data::<SessionData>().unwrap().damage;
                                *damage.lock().unwrap() = vec![state.whole()];
                                state.constraints(session);
                            }
                            for frame in std::mem::take(&mut state.pending) {
                                frame.failed(frame::FailureReason::BufferConstraints);
                            }
                            // wlroots fails a waiting wlr frame whose area no
                            // longer fits the output's buffer.
                            state.fail_wlr();
                        }
                        Command::RejectBuffer => {
                            for frame in std::mem::take(&mut state.pending) {
                                frame.failed(frame::FailureReason::BufferConstraints);
                            }
                            state.fail_wlr();
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
    /// Opens another client connection to this compositor on every call.
    fn connector(&self) -> impl FnMut() -> anyhow::Result<Runtime> + Send + 'static {
        let command = self.command.clone();
        move || {
            let (reply, socket) = mpsc::channel();
            command.send(Command::Connect(reply)).unwrap();
            let socket = socket.recv_timeout(Duration::from_secs(1))?;
            let conn = wayland_client::Connection::from_socket(socket)?;
            Runtime::from_connection(conn, false)
        }
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
// Xrgb8888 colors; the client decodes them to opaque red, green and blue.
const RED: u32 = 0x00ff0000;
const GREEN: u32 = 0x0000ff00;
const BLUE: u32 = 0x000000ff;

fn inside([x, y, width, height]: [i32; 4], px: i32, py: i32) -> bool {
    px >= x && py >= y && px < x.saturating_add(width) && py < y.saturating_add(height)
}

impl Server {
    fn constraints(&self, session: &session::ExtImageCopyCaptureSessionV1) {
        session.buffer_size(self.size.0, self.size.1);
        session.shm_format(if self.config.unsupported {
            wl_shm::Format::Nv12
        } else {
            self.config.format
        });
        session.done();
    }
    fn whole(&self) -> [i32; 4] {
        [0, 0, self.size.0 as i32, self.size.1 as i32]
    }
    fn pixel(&self, x: i32, y: i32) -> u32 {
        let painted = self.painted.iter().rev().find(|(r, _)| inside(*r, x, y));
        painted.map_or(self.color, |(_, color)| *color)
    }
    /// Writes the screen inside `rects` (buffer pixels, clipped to the
    /// buffer) and nothing else. The buffer's top-left shows screen point
    /// `origin`; `invert` stores rows bottom-up, as wlr y_invert describes.
    fn write_buffer(
        &self,
        buffer: &wl_buffer::WlBuffer,
        origin: (i32, i32),
        invert: bool,
        rects: &[[i32; 4]],
    ) {
        let b = buffer.data::<BufferData>().unwrap();
        let (width, height) = (b.width as i32, b.height as i32);
        for &[x, y, w, h] in rects {
            let (left, right) = (x.max(0), x.saturating_add(w).min(width));
            for by in y.max(0)..y.saturating_add(h).min(height) {
                let sy = origin.1 + if invert { height - 1 - by } else { by };
                let row: Vec<u8> = (left..right)
                    .flat_map(|bx| self.pixel(origin.0 + bx, sy).to_ne_bytes())
                    .collect();
                let offset = b.offset + by as u64 * u64::from(b.stride) + left as u64 * 4;
                b.file.write_all_at(&row, offset).unwrap();
            }
        }
    }
    /// Frame damage for every session: what changed since its last ready.
    fn damage_sessions(&self, rect: [i32; 4]) {
        for session in &self.sessions {
            let data = session.data::<SessionData>().unwrap();
            data.damage.lock().unwrap().push(rect);
        }
    }
    /// Completes frames waiting for damage.
    fn release(&mut self) {
        for frame in std::mem::take(&mut self.pending) {
            self.complete(&frame);
        }
        for (frame, buffer) in std::mem::take(&mut self.wlr_pending) {
            self.complete_wlr(&frame, &buffer, true);
        }
    }
    fn complete(&self, frame: &frame::ExtImageCopyCaptureFrameV1) {
        let data = frame.data::<FrameData>().unwrap();
        let guard = data.buffer.lock().unwrap();
        let buffer = guard.as_ref().unwrap();
        let spec = buffer.data::<BufferData>().unwrap();
        if (spec.width, spec.height) != self.size {
            // Constraints may race a client's in-flight request. A compositor
            // rejects that buffer and permits retry after renegotiation.
            frame.failed(frame::FailureReason::BufferConstraints);
            return;
        }
        let client = std::mem::take(&mut *data.damage.lock().unwrap());
        if spec.captured.swap(true, Ordering::SeqCst) {
            if !client.is_empty() {
                self.stats.lock().unwrap().redamaged += 1;
            }
        } else {
            assert!(
                client.contains(&self.whole()),
                "client must damage a new buffer in full"
            );
            self.stats.lock().unwrap().fresh_captures += 1;
        }
        let session = data.session();
        let damage = std::mem::take(&mut *session.damage.lock().unwrap());
        session.captured.store(true, Ordering::SeqCst);
        // Copy only what the protocol requires: the union of client and frame
        // damage. Anything else a client shows must come from earlier frames.
        self.write_buffer(buffer, (0, 0), false, &[&client[..], &damage].concat());
        frame.transform(self.config.transform);
        if self.config.damage_events {
            for [x, y, width, height] in damage {
                frame.damage(x, y, width, height);
            }
        }
        frame.presentation_time(0, 1, 0);
        frame.ready();
    }
    /// Outputs, all or the one named, rotate by 90° (`Command::Rotate`).
    fn rotate(&mut self, only: Option<&str>) {
        let chosen = |output: &wl_output::WlOutput| {
            only.is_none_or(|name| output.data::<OutputData>().unwrap().name == name)
        };
        for output in self.wl_outputs.iter().filter(|o| chosen(o)) {
            let d = output.data::<OutputData>().unwrap();
            let (make, model) = ("Test".into(), "Output".into());
            let subpixel = wl_output::Subpixel::Unknown;
            let rotated = wl_output::Transform::_90;
            output.geometry(d.x, d.y, 0, 0, subpixel, make, model, rotated);
        }
        for (xdg, output) in self.xdg.iter().filter(|(_, o)| chosen(o)) {
            let d = output.data::<OutputData>().unwrap();
            xdg.logical_size(d.height, d.width);
        }
        self.reconfigured();
    }
    /// Ends an output reconfiguration: done, then waiting wlr frames complete
    /// with the area they were requested for.
    fn reconfigured(&mut self) {
        for output in &self.wl_outputs {
            output.done();
        }
        for (frame, buffer) in std::mem::take(&mut self.wlr_pending) {
            self.complete_wlr(&frame, &buffer, true);
        }
    }
    fn late_wlr(&mut self, ready: bool) {
        for (frame, buffer) in std::mem::take(&mut self.wlr_pending) {
            if !ready {
                frame.failed();
                continue;
            }
            let b = buffer.data::<BufferData>().unwrap();
            let marker = BLUE
                .to_ne_bytes()
                .repeat(b.stride as usize * b.height as usize / 4);
            b.file.write_all_at(&marker, b.offset).unwrap();
            frame.flags(wlr_frame::Flags::YInvert);
            frame.damage(0, 0, b.width, b.height);
            frame.ready(0, 1, 0);
        }
    }
    fn fail_wlr(&mut self) {
        for (frame, _) in std::mem::take(&mut self.wlr_pending) {
            frame.failed();
        }
    }
    fn complete_wlr(
        &self,
        frame: &wlr_frame::ZwlrScreencopyFrameV1,
        buffer: &wl_buffer::WlBuffer,
        damage: bool,
    ) {
        let data = frame.data::<WlrFrameData>().unwrap();
        let b = buffer.data::<BufferData>().unwrap();
        assert_eq!((b.width, b.height), data.size, "wlr buffer size");
        let whole = [0, 0, b.width as i32, b.height as i32];
        self.write_buffer(buffer, data.origin, true, &[whole]);
        frame.flags(wlr_frame::Flags::YInvert);
        if damage {
            frame.damage(0, 0, b.width, b.height);
        }
        frame.ready(0, 1, 0);
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
            SelectionInput::Paced => {
                let pointer = self.pointer.clone().unwrap();
                let (left_layer, left) = (layer.clone(), surface.clone());
                let surfaces: Vec<_> = self
                    .layers
                    .iter()
                    .map(|l| {
                        let layer = l.data::<LayerData>().unwrap();
                        (layer.output.name, layer.surface.clone())
                    })
                    .collect();
                let stats = self.stats.clone();
                // Another thread paces the input so this compositor keeps
                // dispatching (and checking) the client's requests meanwhile.
                thread::spawn(move || {
                    let data = left.data::<SurfaceData>().unwrap();
                    let waiting = || !data.held.lock().unwrap().is_empty();
                    let mut paced = false;
                    pointer.enter(1, &left, 10.0, 10.0);
                    pointer.button(2, 0, 0x110, wl_pointer::ButtonState::Pressed);
                    for i in 1..=50 {
                        // Grow the rect to 61x31, then shrink it to 21x11.
                        let step = f64::from(if i <= 30 { i } else { 60 - i });
                        pointer.motion(i, 10.0 + 2.0 * step, 10.0 + step);
                        thread::sleep(Duration::from_millis(2));
                        if i % 10 != 0 {
                            continue;
                        }
                        // Release every held callback once LEFT waits on one.
                        let deadline = std::time::Instant::now() + Duration::from_secs(2);
                        while !waiting() && std::time::Instant::now() < deadline {
                            thread::sleep(Duration::from_millis(1));
                        }
                        paced = waiting();
                        if i == 30 {
                            // A new size mid-drag retires every buffer.
                            left_layer.configure(2, 50, 40);
                        }
                        if i == 50 {
                            // LEFT's next commit is its final frame; the button
                            // is released on it.
                            data.finish_on_commit.store(paced, Ordering::SeqCst);
                        }
                        for (name, surface) in &surfaces {
                            let data = surface.data::<SurfaceData>().unwrap();
                            let held = std::mem::take(&mut *data.held.lock().unwrap());
                            let mut stats = stats.lock().unwrap();
                            stats.overlays.entry(*name).or_default().frames_done += held.len();
                            for callback in held {
                                callback.done(0);
                            }
                        }
                    }
                    // A client that ignores frame callbacks has drawn it already.
                    if !paced {
                        pointer.button(3, 2, 0x110, wl_pointer::ButtonState::Released);
                    }
                });
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
impl Dispatch<wl_buffer::WlBuffer, BufferData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        request: wl_buffer::Request,
        _: &BufferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_buffer::Request::Destroy = request {
            state.stats.lock().unwrap().destroyed_buffers += 1;
        }
    }
}
noop!(xdg_output::ZxdgOutputV1, ());
noop!(top::ExtForeignToplevelHandleV1, &'static str);
noop!(tops::ExtForeignToplevelListV1, ());
noop!(source::ExtImageCaptureSourceV1, ());
noop!(wl_keyboard::WlKeyboard, ());
noop!(wl_pointer::WlPointer, ());
noop!(wl_touch::WlTouch, ());
noop!(wl_region::WlRegion, ());

impl GlobalDispatch<wl_output::WlOutput, OutputData> for Server {
    fn bind(
        state: &mut Self,
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
        state.wl_outputs.push(output);
    }
}
impl Dispatch<xdg_outputs::ZxdgOutputManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
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
            state.xdg.push((xdg, output));
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
                    captured: AtomicBool::new(false),
                },
            );
        }
    }
}
impl GlobalDispatch<tops::ExtForeignToplevelListV1, ()> for Server {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        _: &Client,
        resource: New<tops::ExtForeignToplevelListV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let list = init.init(resource, ());
        for id in &state.windows {
            announce(dh, &list, id);
        }
        state.lists.push(list);
    }
}
/// Announces window `id` on `list`, unless its client has gone; the handle
/// carries the identifier.
fn announce(dh: &DisplayHandle, list: &tops::ExtForeignToplevelListV1, id: &'static str) {
    let Some(client) = list.client() else {
        return;
    };
    let top = client
        .create_resource::<top::ExtForeignToplevelHandleV1, &'static str, Server>(dh, 1, id)
        .unwrap();
    list.toplevel(&top);
    top.identifier(id.into());
    top.done();
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
        if let top_source::Request::CreateSource {
            source,
            toplevel_handle,
        } = request
        {
            let mut stats = state.stats.lock().unwrap();
            stats.window_sources += 1;
            let id = toplevel_handle.data::<&'static str>().unwrap();
            stats.window_ids.push(*id);
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
            // The first frame in a session always carries full damage.
            let damage = Mutex::new(vec![state.whole()]);
            let session = init.init(
                session,
                SessionData {
                    damage,
                    ..SessionData::default()
                },
            );
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
        resource: &session::ExtImageCopyCaptureSessionV1,
        request: session::Request,
        data: &SessionData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            session::Request::CreateFrame { frame } => {
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
                        session: Some(resource.clone()),
                        ..FrameData::default()
                    },
                );
            }
            session::Request::Destroy => state.stats.lock().unwrap().destroyed_sessions += 1,
            _ => {}
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
                // Otherwise the protocol's invalid_buffer_damage error.
                assert!(x >= 0 && y >= 0 && width > 0 && height > 0);
                data.damage.lock().unwrap().push([x, y, width, height]);
            }
            frame::Request::Capture => {
                let captured = data.session().captured.load(Ordering::SeqCst);
                if state.config.reject {
                    resource.failed(frame::FailureReason::BufferConstraints);
                } else if state.config.idle && captured {
                    // Only a session's first frame must complete without damage.
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
        let (frame, overlay_cursor, region) = match request {
            wlr_copy::Request::CaptureOutput {
                frame,
                overlay_cursor,
                ..
            } => (frame, overlay_cursor, None),
            wlr_copy::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                x,
                y,
                width,
                height,
                ..
            } => (frame, overlay_cursor, Some([x, y, width, height])),
            _ => return,
        };
        let whole = state.whole();
        let [x, y, width, height] = region.unwrap_or(whole);
        {
            let mut stats = state.stats.lock().unwrap();
            stats.cursor = overlay_cursor != 0;
            stats.frames += 1;
            if let Some(region) = region {
                stats.wlr_regions.push(region);
            }
        }
        // Clipping to the output is the client's job here (scale 1).
        assert!(
            width > 0 && height > 0 && inside(whole, x, y),
            "region {region:?} starts outside the output"
        );
        assert!(
            x + width <= whole[2] && y + height <= whole[3],
            "region {region:?} is not clipped to the output"
        );
        let size = (width as u32, height as u32);
        let frame = init.init(
            frame,
            WlrFrameData {
                origin: (x, y),
                size,
            },
        );
        // Include padding to exercise the compositor-provided stride.
        let stride = size.0 * 4 + state.wlr_padding;
        frame.buffer(state.config.format, size.0, size.1, stride);
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}
impl Dispatch<wlr_frame::ZwlrScreencopyFrameV1, WlrFrameData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &wlr_frame::ZwlrScreencopyFrameV1,
        request: wlr_frame::Request,
        _: &WlrFrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wlr_frame::Request::Copy { .. } | wlr_frame::Request::CopyWithDamage { .. }
                if state.config.reject =>
            {
                resource.failed();
            }
            wlr_frame::Request::Copy { buffer } | wlr_frame::Request::CopyWithDamage { buffer }
                if state.wlr_pending.iter().any(|(_, held)| *held == buffer) =>
            {
                panic!("a buffer was attached while a waiting frame still held it");
            }
            wlr_frame::Request::Copy { buffer } => {
                state.stats.lock().unwrap().copies += 1;
                state.complete_wlr(resource, &buffer, false);
            }
            wlr_frame::Request::CopyWithDamage { buffer } => {
                state.stats.lock().unwrap().damage_copies += 1;
                // When idle, even a client's first copy_with_damage waits for
                // damage. wlroots is less strict: its per-client damage
                // tracker starts full, so that first one completes at once.
                if state.config.idle {
                    state.wlr_pending.push((resource.clone(), buffer));
                } else {
                    state.complete_wlr(resource, &buffer, true);
                }
            }
            wlr_frame::Request::Destroy => {
                let waiting = state.wlr_pending.iter().position(|(f, _)| f == resource);
                if let Some(index) = waiting {
                    state.wlr_pending.remove(index);
                    state.stats.lock().unwrap().replaced += 1;
                }
            }
            _ => {}
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
noop!(Callback, ());
impl Server {
    /// Shows a committed buffer the way wlroots does for shm: the first one (or
    /// one of a new size) is copied whole, later ones only where damaged.
    fn present(
        &self,
        surface: &wl_surface::WlSurface,
        buffer: &wl_buffer::WlBuffer,
        data: &SurfaceData,
    ) {
        let b = buffer.data::<BufferData>().unwrap();
        let stride = b.stride as usize;
        let mut pixels = vec![0; stride * b.height as usize];
        b.file.read_exact_at(&mut pixels, b.offset).unwrap();
        let mut screen = data.screen.lock().unwrap();
        if screen.len() != pixels.len() {
            screen.clone_from(&pixels);
        }
        for [x, y, width, height] in std::mem::take(&mut *data.damage.lock().unwrap()) {
            // Clip [from, from + len) to 0..=max.
            let clip = |from: i32, len: i32, max: u32| {
                let (from, max) = (i64::from(from), i64::from(max));
                let end = from + i64::from(len);
                (from.clamp(0, max) as usize, end.clamp(0, max) as usize)
            };
            let (x0, x1) = clip(x, width, b.width);
            let (y0, y1) = clip(y, height, b.height);
            for row in y0..y1 {
                let span = row * stride + x0 * 4..row * stride + x1 * 4;
                screen[span.clone()].copy_from_slice(&pixels[span]);
            }
        }
        let held = data.held.lock().unwrap().len();
        if let Some(layer) = self
            .layers
            .iter()
            .map(|l| l.data::<LayerData>().unwrap())
            .find(|l| l.surface == *surface)
        {
            let mut stats = self.stats.lock().unwrap();
            let log = stats.overlays.entry(layer.output.name).or_default();
            log.commits += 1;
            log.unpaced += usize::from(held > 0);
            log.stale += usize::from(pixels != *screen);
            log.buffer = pixels;
            log.screen.clone_from(&screen);
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
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => *data.buffer.lock().unwrap() = buffer,
            wl_surface::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                assert!(width > 0 && height > 0, "empty damage box");
                data.damage.lock().unwrap().push([x, y, width, height]);
            }
            // Surface-coordinate damage is taken as whole-surface damage.
            wl_surface::Request::Damage { .. } => {
                data.damage.lock().unwrap().push([0, 0, i32::MAX, i32::MAX]);
            }
            // wayland-backend panics unless every new object is initialized.
            wl_surface::Request::Frame { callback } => {
                data.frames.lock().unwrap().push(init.init(callback, ()));
            }
            wl_surface::Request::Commit => {
                let buffer = data.buffer.lock().unwrap().clone();
                if let Some(buffer) = buffer {
                    state.present(resource, &buffer, data);
                    // A paced compositor keeps each buffer until the next one
                    // replaces it, so the client has to rotate buffers.
                    let done = if matches!(state.config.input, SelectionInput::Paced) {
                        data.current.lock().unwrap().replace(buffer)
                    } else {
                        Some(buffer)
                    };
                    if let Some(done) = done {
                        done.release();
                    }
                } else if let Some(layer) = state
                    .layers
                    .iter()
                    .find(|l| l.data::<LayerData>().unwrap().surface == *resource)
                {
                    let output = &layer.data::<LayerData>().unwrap().output;
                    layer.configure(1, output.width as u32, output.height as u32);
                }
                let frames = std::mem::take(&mut *data.frames.lock().unwrap());
                if matches!(state.config.input, SelectionInput::Paced) {
                    data.held.lock().unwrap().extend(frames);
                } else {
                    for frame in frames {
                        frame.done(0);
                    }
                }
                if data.finish_on_commit.swap(false, Ordering::SeqCst) {
                    let pointer = state.pointer.as_ref().unwrap();
                    pointer.button(3, 2, 0x110, wl_pointer::ButtonState::Released);
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
fn selector_redraws_only_touched_overlays_once_per_frame_callback() {
    let mut fixture = Fixture::new(Config {
        input: SelectionInput::Paced,
        ..Config::default()
    });
    let runtime = fixture.runtime(true);
    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(crate::selector::select(runtime));
    });
    let region = result
        .recv_timeout(Duration::from_secs(30))
        .expect("selection did not finish")
        .unwrap();
    let rect = Rect {
        x: 10,
        y: 10,
        width: 21,
        height: 11,
    };
    assert_eq!(
        region,
        Region {
            output: "LEFT".into(),
            rect
        }
    );
    let stats = fixture.stats.lock().unwrap();
    let (left, right) = (&stats.overlays["LEFT"], &stats.overlays["RIGHT"]);
    // Each overlay's first commit draws it; every later one needs a callback.
    assert_eq!(left.unpaced, 0, "LEFT committed while a callback was held");
    assert!(
        left.commits <= 1 + left.frames_done,
        "LEFT made {} commits for {} frame callbacks",
        left.commits,
        left.frames_done
    );
    assert_eq!(
        right.commits, 1,
        "RIGHT was redrawn, but no rect touched it"
    );
    // Three or more commits reuse a buffer that holds an older frame.
    assert!(left.commits > 2, "only {} LEFT commits", left.commits);
    assert_eq!(left.stale, 0, "a LEFT buffer disagreed with its damage");
    // The drag ends on LEFT resized to 50x40.
    let expected = crate::selector::draw_overlay(
        crate::pixels::BufferSpec::packed(
            50,
            40,
            wayland_client::protocol::wl_shm::Format::Argb8888,
        )
        .unwrap(),
        Rect {
            x: -100,
            y: 50,
            width: 50,
            height: 40,
        },
        Some(Rect {
            x: rect.x - 100,
            y: rect.y + 50,
            ..rect
        }),
    )
    .unwrap();
    assert!(
        left.buffer == expected,
        "LEFT's last buffer is not the final rect"
    );
    assert!(
        left.screen == expected,
        "LEFT's damage does not compose to it"
    );
}

#[test]
fn overlay_buffers_take_only_whole_rows_inside_the_buffer() {
    let mut fixture = Fixture::new(Config::default());
    let runtime = fixture.runtime(true);
    // 3x4 pixels: 12-byte rows, 48 bytes.
    let spec =
        crate::pixels::BufferSpec::packed(3, 4, wayland_client::protocol::wl_shm::Format::Argb8888)
            .unwrap();
    let mut buffer = crate::connection::Buffer::new(
        runtime.state.shm.as_ref().unwrap(),
        spec,
        &runtime.queue.handle(),
    )
    .unwrap();
    buffer.write_rows(1, &[7; 24]).unwrap();
    buffer.write_rows(4, &[]).unwrap();
    for (row, len) in [(0, 11), (0, 13), (2, 36), (3, 24), (4, 12), (u32::MAX, 12)] {
        assert!(
            buffer.write_rows(row, &vec![9; len]).is_err(),
            "{len} bytes at row {row}"
        );
    }
    let bytes = buffer.read().unwrap();
    assert_eq!(bytes[..12], [0; 12]);
    assert_eq!(bytes[12..36], [7; 24]);
    assert_eq!(bytes[36..], [0; 12]);
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
                CaptureOptions {
                    cursor,
                    ..CaptureOptions::default()
                },
            )
            .unwrap();
            capture.capture().unwrap();
            assert_eq!(fixture.stats.lock().unwrap().cursor, cursor);
        }
    }
}

// Capture damage, regions and stalled constraints.

const LEFT: Rect = Rect {
    x: -100,
    y: 50,
    width: 100,
    height: 80,
};
const RIGHT: Rect = Rect {
    x: 0,
    y: 50,
    width: 100,
    height: 80,
};
const SIZE: (u32, u32) = (100, 80);
/// A whole 100x80 buffer as the client allocates it (packed rows).
const WHOLE: usize = 100 * 80 * 4;
const ROW: usize = 100 * 4;
const NORMAL: wl_output::Transform = wl_output::Transform::Normal;
const NEVER: Duration = Duration::from_secs(3600);

/// The fake's screen as a client decodes it: `background` with `painted` on
/// top, stored with `transform` applied.
fn expected(
    (width, height): (u32, u32),
    background: u32,
    painted: &[([i32; 4], u32)],
    transform: wl_output::Transform,
) -> image::RgbaImage {
    use crate::connection::{wl_output::Transform, wl_shm::Format};
    let spec = BufferSpec::packed(width, height, Format::Xrgb8888).unwrap();
    let mut bytes = Vec::new();
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let top = painted.iter().rev().find(|(r, _)| inside(*r, x, y));
            bytes.extend(top.map_or(background, |(_, color)| *color).to_ne_bytes());
        }
    }
    let transform = Transform::try_from(u32::from(transform)).unwrap();
    pixels::decode(&bytes, spec, false, transform).unwrap()
}

#[track_caller]
fn assert_image(actual: &image::RgbaImage, expected: &image::RgbaImage) {
    assert_eq!(actual.dimensions(), expected.dimensions(), "image size");
    let pixels = actual.enumerate_pixels().zip(expected.pixels());
    if let Some(((x, y, a), e)) = pixels.into_iter().find(|((_, _, a), e)| a != e) {
        panic!("pixel ({x}, {y}) is {:?}, expected {:?}", a.0, e.0);
    }
}

fn idle() -> Config {
    Config {
        idle: true,
        ..Config::default()
    }
}

#[test]
fn stalled_buffer_constraints_retry_with_a_fresh_buffer_after_the_grace() {
    let mut fixture = Fixture::new(idle());
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.constraints_grace = Duration::from_millis(250);
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    // The waiting frame's buffer is rejected, but no new constraints follow.
    fixture.command.send(Command::RejectBuffer).unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    {
        let stats = fixture.stats.lock().unwrap();
        assert_eq!((stats.frames, stats.buffers), (2, 1), "no retry yet");
    }
    fixture.command.send(Command::Resume).unwrap();
    let frame = capture
        .next_frame(Duration::from_secs(2))
        .unwrap()
        .expect("a retry after the grace");
    assert_image(&frame.image, &expected(SIZE, RED, &[], NORMAL));
    let stats = fixture.stats.lock().unwrap();
    assert_eq!((stats.frames, stats.buffers), (3, 2), "one fresh buffer");
}

#[test]
fn repeated_buffer_rejection_fails_the_capture_instead_of_freezing() {
    let mut fixture = Fixture::new(Config {
        reject: true,
        ..Config::default()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.constraints_grace = Duration::from_millis(20);
    let started = Instant::now();
    let error = capture.capture().unwrap_err().to_string();
    assert!(error.contains("BufferConstraints"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    // The first attempt and three retries, each with a fresh buffer.
    let stats = fixture.stats.lock().unwrap();
    assert_eq!((stats.frames, stats.buffers), (4, 4));
}

#[test]
fn ext_damage_updates_only_the_damaged_rows_of_a_reused_buffer() {
    let mut fixture = Fixture::new(idle());
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.refresh_interval = NEVER;
    assert_image(
        &capture.capture().unwrap().image,
        &expected(SIZE, RED, &[], NORMAL),
    );
    assert_eq!(capture.bytes_read(), [WHOLE]);
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    let rect = [20, 30, 10, 5];
    fixture
        .command
        .send(Command::DamageRect { rect, color: GREEN })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    // The fake copied only the damage: old pixels outside came from the client.
    assert_image(&frame.image, &expected(SIZE, RED, &[(rect, GREEN)], NORMAL));
    assert_eq!(capture.bytes_read(), [WHOLE + 5 * ROW], "rows 30..35");
    assert_eq!(capture.kept_images(), [true], "for the next row update");
    let stats = fixture.stats.lock().unwrap();
    assert_eq!(
        (stats.fresh_captures, stats.redamaged),
        (1, 0),
        "full buffer damage only on first use"
    );
}

#[test]
fn ext_whole_frame_damage_hands_the_image_out_instead_of_copying_it() {
    let mut fixture = Fixture::new(idle());
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    // The first frame's full damage is required, so it says nothing yet.
    assert_eq!(capture.kept_images(), [true]);
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    fixture.command.send(Command::Damage).unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_image(&frame.image, &expected(SIZE, BLUE, &[], NORMAL));
    assert_eq!(capture.kept_images(), [false], "no copy per frame");
}

#[test]
fn ext_damage_reaching_outside_the_buffer_is_clipped() {
    let mut fixture = Fixture::new(idle());
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    // Over-reported damage: columns -10..20 and rows 70..120 of a 100x80 buffer.
    let rect = [-10, 70, 30, 50];
    fixture
        .command
        .send(Command::DamageRect { rect, color: GREEN })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_image(&frame.image, &expected(SIZE, RED, &[(rect, GREEN)], NORMAL));
    assert_eq!(capture.bytes_read(), [WHOLE + 10 * ROW], "rows 70..80");
}

#[test]
fn ext_ready_without_damage_events_refreshes_the_whole_frame() {
    let mut fixture = Fixture::new(Config {
        damage_events: false,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    let rect = [20, 30, 10, 5];
    fixture
        .command
        .send(Command::DamageRect { rect, color: GREEN })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_image(&frame.image, &expected(SIZE, RED, &[(rect, GREEN)], NORMAL));
    assert_eq!(capture.bytes_read(), [2 * WHOLE]);
    assert_eq!(fixture.stats.lock().unwrap().redamaged, 0);
}

#[test]
fn ext_region_and_desktop_rect_skip_damage_they_do_not_show() {
    let desktop = Rect {
        x: -25,
        y: 60,
        width: 50,
        height: 20,
    };
    let region = Region {
        output: "LEFT".into(),
        rect: Rect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        },
    };
    for (target, sessions) in [
        (CaptureTarget::Region(region.clone()), 1),
        (CaptureTarget::DesktopRect(desktop), 2),
    ] {
        let mut fixture = Fixture::new(idle());
        let mut capture = fixture.capture(target.clone());
        capture.refresh_interval = NEVER;
        capture.capture().unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        let read = capture.bytes_read();
        // In rows the target shows, but in columns outside the region and
        // outside both outputs' parts of the rectangle.
        let outside = [60, 25, 10, 10];
        fixture
            .command
            .send(Command::DamageRect {
                rect: outside,
                color: GREEN,
            })
            .unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(100))
                .unwrap()
                .is_none(),
            "{target:?} published damage it does not show"
        );
        assert_eq!(capture.bytes_read(), read, "{target:?}");
        assert_eq!(
            fixture.stats.lock().unwrap().frames,
            3 * sessions,
            "{target:?} requested the next frame"
        );
        let shown = [15, 25, 5, 5];
        fixture
            .command
            .send(Command::DamageRect {
                rect: shown,
                color: BLUE,
            })
            .unwrap();
        let frame = capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("damage the target shows");
        let full = expected(SIZE, RED, &[(outside, GREEN), (shown, BLUE)], NORMAL);
        let want = match &target {
            CaptureTarget::Region(region) => pixels::crop(&full, LEFT, region.rect),
            _ => pixels::compose(&[(&full, LEFT), (&full, RIGHT)], desktop),
        }
        .unwrap();
        assert_image(&frame.image, &want.image);
    }
}

#[test]
fn ext_rotated_frames_with_partial_damage_match_a_full_decode() {
    let transform = wl_output::Transform::_90;
    let mut fixture = Fixture::new(Config {
        transform,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    capture.refresh_interval = NEVER;
    let first = capture.capture().unwrap();
    assert_image(&first.image, &expected(SIZE, RED, &[], transform));
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    let rect = [20, 30, 10, 5];
    fixture
        .command
        .send(Command::DamageRect { rect, color: GREEN })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_image(
        &frame.image,
        &expected(SIZE, RED, &[(rect, GREEN)], transform),
    );
    // Only untransformed buffers are updated row by row.
    assert_eq!(capture.bytes_read(), [2 * WHOLE]);
}

#[test]
fn ext_transform_change_at_the_same_size_decodes_in_full() {
    let flipped = wl_output::Transform::Flipped180;
    let region = Rect {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    };
    let targets = [
        CaptureTarget::Output("LEFT".into()),
        // Regions keep their image, so a change into Normal with partial
        // damage reaches the decode key.
        CaptureTarget::Region(Region {
            output: "LEFT".into(),
            rect: region,
        }),
    ];
    for target in targets {
        for (before, after) in [(NORMAL, flipped), (flipped, NORMAL)] {
            let mut fixture = Fixture::new(Config {
                transform: before,
                ..idle()
            });
            // Asymmetric content inside the region's rows either way up, so
            // a stale orientation cannot pass unnoticed.
            let band = [0, 22, 100, 4];
            fixture
                .command
                .send(Command::DamageRect {
                    rect: band,
                    color: BLUE,
                })
                .unwrap();
            let mut capture = fixture.capture(target.clone());
            capture.refresh_interval = NEVER;
            let shown = |full: &image::RgbaImage| match &target {
                CaptureTarget::Region(_) => pixels::crop(full, LEFT, region).unwrap().image,
                _ => full.clone(),
            };
            let first = capture.capture().unwrap();
            let want = shown(&expected(SIZE, RED, &[(band, BLUE)], before));
            assert_image(&first.image, &want);
            assert!(
                capture
                    .next_frame(Duration::from_millis(20))
                    .unwrap()
                    .is_none()
            );
            fixture.command.send(Command::Transform(after)).unwrap();
            let rect = [20, 30, 10, 5];
            fixture
                .command
                .send(Command::DamageRect { rect, color: GREEN })
                .unwrap();
            let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            let painted = [(band, BLUE), (rect, GREEN)];
            let want = shown(&expected(SIZE, RED, &painted, after));
            assert_image(&frame.image, &want);
        }
    }
}

#[test]
fn ext_new_buffer_after_resize_gets_full_damage_then_partial_updates() {
    let mut fixture = Fixture::new(idle());
    let mut capture = fixture.capture(CaptureTarget::Toplevel("stable-window".into()));
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    fixture.command.send(Command::Resize).unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
    let size = (60, 40);
    let first = [0, 0, 10, 10];
    fixture
        .command
        .send(Command::DamageRect {
            rect: first,
            color: GREEN,
        })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_image(
        &frame.image,
        &expected(size, RED, &[(first, GREEN)], NORMAL),
    );
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    let second = [5, 20, 10, 5];
    fixture
        .command
        .send(Command::DamageRect {
            rect: second,
            color: BLUE,
        })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    let painted = [(first, GREEN), (second, BLUE)];
    assert_image(&frame.image, &expected(size, RED, &painted, NORMAL));
    // The new buffer was read in full once, then only rows 20..25.
    assert_eq!(capture.bytes_read(), [60 * 40 * 4 + 5 * 60 * 4]);
    let stats = fixture.stats.lock().unwrap();
    assert_eq!(stats.buffers, 2);
    assert_eq!((stats.fresh_captures, stats.redamaged), (2, 0));
}

#[test]
fn ext_safety_refresh_and_escape_hatch_recopy_and_reread_whole_frames() {
    for escape in [false, true] {
        let mut fixture = Fixture::new(idle());
        let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
        if escape {
            capture.full_damage = true;
        } else {
            capture.refresh_interval = Duration::ZERO;
        }
        capture.capture().unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        let rect = [20, 30, 10, 5];
        fixture
            .command
            .send(Command::DamageRect { rect, color: GREEN })
            .unwrap();
        let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        assert_image(&frame.image, &expected(SIZE, RED, &[(rect, GREEN)], NORMAL));
        assert_eq!(capture.bytes_read(), [2 * WHOLE], "escape {escape}");
        // Whole frames every time need no image kept for row updates.
        assert_eq!(capture.kept_images(), [!escape], "escape {escape}");
        // The reused buffer was damaged in full again, so all of it was copied.
        assert_eq!(
            fixture.stats.lock().unwrap().redamaged,
            1,
            "escape {escape}"
        );
    }
}

#[test]
fn wlr_regions_capture_only_the_clipped_rectangle_without_cropping() {
    let spot = [10, 20, 5, 5];
    for (rect, clipped) in [
        (
            Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            },
            [10, 20, 30, 40],
        ),
        (
            Rect {
                x: 90,
                y: 70,
                width: 30,
                height: 40,
            },
            [90, 70, 10, 10],
        ),
    ] {
        let mut fixture = Fixture::new(Config {
            ext: false,
            ..Config::default()
        });
        fixture
            .command
            .send(Command::DamageRect {
                rect: spot,
                color: GREEN,
            })
            .unwrap();
        let mut capture = fixture.capture(CaptureTarget::Region(Region {
            output: "LEFT".into(),
            rect,
        }));
        let frame = capture.capture().unwrap();
        assert_eq!(fixture.stats.lock().unwrap().wlr_regions, [clipped]);
        let full = expected(SIZE, RED, &[(spot, GREEN)], NORMAL);
        let want = pixels::crop(&full, LEFT, rect).unwrap();
        assert_eq!(
            (frame.logical_width, frame.logical_height),
            (want.logical_width, want.logical_height)
        );
        assert_image(&frame.image, &want.image);
    }
}

#[test]
fn wlr_copy_with_damage_waits_for_damage_after_a_prompt_first_frame() {
    let rect = Rect {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    };
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Region(Region {
        output: "LEFT".into(),
        rect,
    }));
    // The refresh (tested separately) must not replace the waiting frame.
    capture.refresh_interval = NEVER;
    // A plain copy completes the first frame of a static screen.
    capture.capture().unwrap();
    // The next frame is the first copy_with_damage, which the fake holds;
    // wlroots would complete it at once (its damage tracker starts full) and
    // hold the one after.
    for _ in 0..2 {
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
    }
    {
        let stats = fixture.stats.lock().unwrap();
        assert_eq!(
            (stats.frames, stats.copies, stats.damage_copies),
            (2, 1, 1),
            "one frame waits for damage"
        );
    }
    let spot = [15, 25, 5, 5];
    fixture
        .command
        .send(Command::DamageRect {
            rect: spot,
            color: GREEN,
        })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    let full = expected(SIZE, RED, &[(spot, GREEN)], NORMAL);
    assert_image(
        &frame.image,
        &pixels::crop(&full, LEFT, rect).unwrap().image,
    );
}

#[test]
fn wlr_without_copy_with_damage_or_with_the_escape_hatch_keeps_plain_copy() {
    for (version, escape) in [(1, false), (2, false), (3, false), (3, true)] {
        let mut fixture = Fixture::new(Config {
            ext: false,
            wlr_version: version,
            ..idle()
        });
        let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
        capture.full_damage = escape;
        capture.refresh_interval = NEVER;
        capture.capture().unwrap();
        let second = capture.next_frame(Duration::from_millis(50)).unwrap();
        let stats = fixture.stats.lock().unwrap();
        let case = format!("version {version}, escape {escape}");
        if version == 1 || escape {
            assert!(second.is_some(), "{case}: a plain copy completes at once");
            assert_eq!((stats.copies, stats.damage_copies), (2, 0), "{case}");
        } else {
            assert!(second.is_none(), "{case}: waits for damage");
            assert_eq!((stats.copies, stats.damage_copies), (1, 1), "{case}");
        }
    }
}

#[test]
fn wlr_frames_failed_while_waiting_retry_with_a_plain_copy() {
    // For example a smaller mode or a rotation while the screen is static.
    for resize in [false, true] {
        let mut fixture = Fixture::new(Config {
            ext: false,
            ..idle()
        });
        let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
        capture.refresh_interval = NEVER;
        capture.capture().unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        let command = if resize {
            Command::Resize
        } else {
            Command::RejectBuffer
        };
        fixture.command.send(command).unwrap();
        // A plain copy completes without new damage.
        let frame = capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("a retried frame");
        let size = if resize { (60, 40) } else { SIZE };
        assert_image(&frame.image, &expected(size, RED, &[], NORMAL));
        let stats = fixture.stats.lock().unwrap();
        assert_eq!((stats.frames, stats.copies, stats.damage_copies), (3, 2, 1));
    }
    // A compositor that keeps failing frames still stops the capture.
    let mut fixture = Fixture::new(Config {
        ext: false,
        reject: true,
        ..Config::default()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    let error = capture.capture().unwrap_err().to_string();
    assert!(error.contains("compositor rejected the frame"), "{error}");
    assert_eq!(fixture.stats.lock().unwrap().frames, 4);
}

#[test]
fn wlr_new_buffers_start_with_a_plain_copy() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    // Not the refresh: a new buffer alone must not wait for damage.
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    // Same picture and configuration, but later frames need a new buffer.
    fixture.command.send(Command::Restride).unwrap();
    let spot = [15, 25, 5, 5];
    fixture
        .command
        .send(Command::DamageRect {
            rect: spot,
            color: GREEN,
        })
        .unwrap();
    let want = expected(SIZE, RED, &[(spot, GREEN)], NORMAL);
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    assert_image(&frame.image, &want);
    // The static screen's next frame goes into the new buffer: a plain copy.
    let frame = capture
        .next_frame(Duration::from_secs(1))
        .unwrap()
        .expect("a plain copy into the new buffer");
    assert_image(&frame.image, &want);
    let stats = fixture.stats.lock().unwrap();
    assert_eq!(stats.buffers, 2);
    assert_eq!((stats.copies, stats.damage_copies), (2, 1));
}

#[test]
fn wlr_frames_that_waited_across_an_output_change_are_dropped() {
    for rotate in [true, false] {
        let mut fixture = Fixture::new(Config {
            ext: false,
            ..idle()
        });
        // Asymmetric content shows the orientation.
        let spot = [0, 0, 10, 10];
        fixture
            .command
            .send(Command::DamageRect {
                rect: spot,
                color: BLUE,
            })
            .unwrap();
        let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
        capture.refresh_interval = NEVER;
        capture.capture().unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        let change = if rotate {
            Command::Rotate
        } else {
            Command::Grow
        };
        fixture.command.send(change).unwrap();
        // The waiting frame completes with the area it was requested for; it
        // must not be published as the new configuration.
        let frame = capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("a frame of the new configuration");
        let rotated = wl_output::Transform::_90;
        let (size, transform, logical) = if rotate {
            (SIZE, rotated, (80, 100))
        } else {
            ((120, 90), NORMAL, (120, 90))
        };
        let want = expected(size, RED, &[(spot, BLUE)], transform);
        assert_image(&frame.image, &want);
        let frame_logical = (frame.logical_width, frame.logical_height);
        assert_eq!(frame_logical, logical, "rotate {rotate}");
        // The dropped frame, then a plain copy right away.
        let stats = fixture.stats.lock().unwrap();
        assert_eq!(
            (stats.copies, stats.damage_copies),
            (2, 1),
            "rotate {rotate}"
        );
    }
}

#[test]
fn wlr_region_frames_that_waited_across_a_scale_change_are_dropped() {
    // At scale 2 the output is 50x40 logical pixels. The first region stays
    // inside it but covers other buffer pixels (the waiting frame's area is
    // stale); the second is also clipped to 30x20.
    for (rect, clipped) in [
        ((10, 10, 20, 20), [10, 10, 20, 20]),
        ((10, 20, 30, 40), [10, 20, 30, 20]),
    ] {
        let mut fixture = Fixture::new(Config {
            ext: false,
            ..idle()
        });
        let (x, y, width, height) = rect;
        let requested = [x, y, width as i32, height as i32];
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        let mut capture = fixture.capture(CaptureTarget::Region(Region {
            output: "LEFT".into(),
            rect,
        }));
        capture.refresh_interval = NEVER;
        capture.capture().unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        fixture.command.send(Command::Scale).unwrap();
        let frame = capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("a frame of the new configuration");
        let logical = (clipped[2] as u32, clipped[3] as u32);
        assert_eq!((frame.logical_width, frame.logical_height), logical);
        // The waiting frame was dropped: a new region capture, a plain copy.
        let stats = fixture.stats.lock().unwrap();
        assert_eq!(stats.wlr_regions, [requested, requested, clipped]);
        assert_eq!((stats.copies, stats.damage_copies), (2, 1));
    }
}

#[test]
fn ext_desktop_rect_after_an_output_move_decodes_in_full() {
    let mut fixture = Fixture::new(idle());
    // Only LEFT: its columns 40..70 and rows 10..30.
    let desktop = Rect {
        x: -60,
        y: 60,
        width: 30,
        height: 20,
    };
    let mut capture = fixture.capture(CaptureTarget::DesktopRect(desktop));
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    // Damage in the shown rows but beyond the shown columns is skipped.
    let hidden = [75, 12, 10, 5];
    fixture
        .command
        .send(Command::DamageRect {
            rect: hidden,
            color: GREEN,
        })
        .unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    // LEFT moves 20 px left: the rectangle now shows its columns 60..90,
    // including the skipped damage, at the same buffer size.
    let x = -120;
    fixture
        .command
        .send(Command::Move { name: "LEFT", x })
        .unwrap();
    let shown = [62, 20, 3, 3];
    fixture
        .command
        .send(Command::DamageRect {
            rect: shown,
            color: BLUE,
        })
        .unwrap();
    let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
    let full = expected(SIZE, RED, &[(hidden, GREEN), (shown, BLUE)], NORMAL);
    let moved = Rect { x, ..LEFT };
    let want = pixels::compose(&[(&full, moved)], desktop).unwrap();
    assert_image(&frame.image, &want.image);
}

#[test]
fn wlr_idle_sessions_refresh_with_a_plain_copy_at_the_interval() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    let interval = Duration::from_millis(200);
    capture.refresh_interval = interval;
    capture.capture().unwrap();
    let last = Instant::now();
    // Between refreshes the next frame waits for damage.
    assert!(
        capture
            .next_frame(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
    {
        let stats = fixture.stats.lock().unwrap();
        assert_eq!((stats.frames, stats.copies, stats.damage_copies), (2, 1, 1));
    }
    // Then the waiting frame gives way to a plain copy of the static screen.
    let frame = capture
        .next_frame(Duration::from_secs(2))
        .unwrap()
        .expect("a refresh");
    let waited = last.elapsed();
    assert!(
        waited >= interval / 2 && waited < Duration::from_secs(1),
        "{waited:?}"
    );
    assert_image(&frame.image, &expected(SIZE, RED, &[], NORMAL));
    {
        let stats = fixture.stats.lock().unwrap();
        let counts = (stats.frames, stats.copies, stats.damage_copies);
        assert_eq!((counts, stats.replaced), ((3, 2, 1), 1));
        assert_eq!(stats.buffers, 1, "the buffer is reused");
    }
    // After which frames wait for damage again.
    assert!(
        capture
            .next_frame(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
    let stats = fixture.stats.lock().unwrap();
    assert_eq!((stats.frames, stats.damage_copies), (4, 2));
}

#[test]
fn wlr_damage_before_the_refresh_arrives_at_once() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    let interval = Duration::from_millis(400);
    capture.refresh_interval = interval;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(200))
            .unwrap()
            .is_none()
    );
    let spot = [15, 25, 5, 5];
    fixture
        .command
        .send(Command::DamageRect {
            rect: spot,
            color: GREEN,
        })
        .unwrap();
    let started = Instant::now();
    let frame = capture.next_frame(Duration::from_secs(3)).unwrap().unwrap();
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(150), "{waited:?}");
    assert_image(&frame.image, &expected(SIZE, RED, &[(spot, GREEN)], NORMAL));
    // The refresh counts from the latest frame, so the next one waits for
    // damage although the first frame is older than the interval by then.
    assert!(capture.next_frame(interval * 5 / 8).unwrap().is_none());
    let stats = fixture.stats.lock().unwrap();
    let counts = (stats.copies, stats.damage_copies, stats.replaced);
    assert_eq!(counts, (1, 2, 0), "delivered by copy_with_damage");
}

#[test]
fn wlr_replaced_frames_never_deliver_into_their_successor() {
    for ready in [true, false] {
        let mut fixture = Fixture::new(Config {
            ext: false,
            ..idle()
        });
        let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
        let interval = Duration::from_millis(100);
        capture.refresh_interval = interval;
        capture.capture().unwrap();
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.stats.lock().unwrap().damage_copies, 1);
        // The compositor finishes (with a marker instead of the screen) or
        // fails the waiting frame, but the client reads that only after the
        // due refresh has destroyed the frame.
        fixture.command.send(Command::WlrLate { ready }).unwrap();
        thread::sleep(interval * 2);
        let frame = capture
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("the refresh");
        assert_image(&frame.image, &expected(SIZE, RED, &[], NORMAL));
        {
            let stats = fixture.stats.lock().unwrap();
            let counts = (stats.frames, stats.copies, stats.damage_copies);
            assert_eq!(counts, (3, 2, 1), "ready {ready}: one replacement");
        }
        // The session carries on: waiting for damage, which then arrives.
        assert!(
            capture
                .next_frame(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        let spot = [15, 25, 5, 5];
        fixture
            .command
            .send(Command::DamageRect {
                rect: spot,
                color: GREEN,
            })
            .unwrap();
        let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        assert_image(&frame.image, &expected(SIZE, RED, &[(spot, GREEN)], NORMAL));
    }
}

#[test]
fn buffer_row_reads_fetch_only_the_requested_rows() {
    use crate::connection::{Buffer, wl_shm::Format};
    let mut fixture = Fixture::new(Config::default());
    let runtime = fixture.runtime(false);
    // Four rows of two pixels, padded to a 12-byte stride.
    let spec = BufferSpec {
        width: 2,
        height: 4,
        stride: 12,
        format: Format::Xrgb8888,
    };
    let (shm, qh) = (runtime.state.shm.as_ref().unwrap(), runtime.queue.handle());
    let mut buffer = Buffer::new(shm, spec, &qh).unwrap();
    let bytes: Vec<u8> = (1..=48).collect();
    buffer.write(&bytes).unwrap();
    let read = buffer.read_rows(&[1..2, 3..4]).unwrap();
    let mut want = vec![0; 48];
    want[12..24].copy_from_slice(&bytes[12..24]);
    want[36..].copy_from_slice(&bytes[36..]);
    assert_eq!(read, want, "other rows keep what was read before");
    assert_eq!(buffer.bytes_read, 24);
    let reversed = std::ops::Range { start: 2, end: 1 };
    for rows in [3..5, 4..5, reversed] {
        let rows = std::slice::from_ref(&rows);
        assert!(buffer.read_rows(rows).is_err(), "{rows:?}");
    }
    assert_eq!(buffer.read_rows(&[2..2, 4..4]).unwrap(), want, "empty");
    assert_eq!(buffer.read().unwrap(), bytes);
}

#[test]
fn hidpi_logical_size_without_xdg_output_rounds() {
    let mut fixture = Fixture::new(Config::default());
    let runtime = fixture.runtime(false);
    let mut output = runtime.state.outputs.values().next().unwrap().clone();
    output.logical_size = None;
    for (mode, scale, logical) in [
        ((3840, 2160), 2, (1920, 1080)),
        ((2561, 1441), 2, (1281, 721)),
        ((3841, 2161), 3, (1280, 720)),
        ((3842, 2162), 3, (1281, 721)),
    ] {
        output.mode = mode;
        output.scale = scale;
        let rect = output.rect();
        assert_eq!((rect.width, rect.height), logical, "{mode:?} at {scale}");
    }
    output.transform = crate::connection::wl_output::Transform::_90;
    let rect = output.rect();
    assert_eq!((rect.width, rect.height), (721, 1281));
}

#[test]
fn wlr_desktop_rect_frames_never_compose_a_tile_dropped_after_rotation() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..idle()
    });
    // Asymmetric content inside rotated LEFT's part of the rectangle.
    let spot = [15, 0, 10, 3];
    fixture
        .command
        .send(Command::DamageRect {
            rect: spot,
            color: BLUE,
        })
        .unwrap();
    let desktop = Rect {
        x: -25,
        y: 60,
        width: 50,
        height: 20,
    };
    let mut capture = fixture.capture(CaptureTarget::DesktopRect(desktop));
    capture.refresh_interval = NEVER;
    capture.capture().unwrap();
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    // Only LEFT rotates. Both waiting frames complete in the same batch:
    // LEFT's with the area it was requested for, RIGHT's still current.
    fixture
        .command
        .send(Command::RotateOutput { name: "LEFT" })
        .unwrap();
    let frame = capture
        .next_frame(Duration::from_secs(1))
        .unwrap()
        .expect("a frame of the new layout");
    // LEFT's pre-rotation tile must not be composed with its new rectangle.
    let left = expected(SIZE, RED, &[(spot, BLUE)], wl_output::Transform::_90);
    let rotated = Rect {
        width: LEFT.height,
        height: LEFT.width,
        ..LEFT
    };
    let right = expected(SIZE, RED, &[(spot, BLUE)], NORMAL);
    let want = pixels::compose(&[(&left, rotated), (&right, RIGHT)], desktop).unwrap();
    assert_image(&frame.image, &want.image);
    // LEFT was copied again; RIGHT went on waiting for damage.
    let stats = fixture.stats.lock().unwrap();
    assert_eq!((stats.copies, stats.damage_copies), (3, 3));
}

// Opaque capture, and still images over one connection.

/// Premultiplied Argb8888: alpha 0x80 over color 0x60, 0x40, 0x20, and
/// alpha 0x40 over 0x10, 0x20, 0x30.
const HALF: u32 = 0x8060_4020;
const QUARTER: u32 = 0x4010_2030;

#[test]
fn opaque_sessions_deliver_premultiplied_color_over_black() {
    let damage = [20, 30, 10, 5];
    for (alpha, half, quarter) in [
        // Straight color, rounded as screenshots always have been.
        (AlphaMode::Straight, [191, 128, 64, 128], [64, 128, 191, 64]),
        (AlphaMode::Opaque, [96, 64, 32, 255], [16, 32, 48, 255]),
    ] {
        for ext in [true, false] {
            let case = format!("{alpha:?}, ext {ext}");
            let mut fixture = Fixture::new(Config {
                ext,
                format: wl_shm::Format::Argb8888,
                ..idle()
            });
            let whole = [0, 0, SIZE.0 as i32, SIZE.1 as i32];
            fixture
                .command
                .send(Command::DamageRect {
                    rect: whole,
                    color: HALF,
                })
                .unwrap();
            let options = CaptureOptions {
                cursor: false,
                alpha,
            };
            let target = CaptureTarget::Output("LEFT".into());
            let runtime = fixture.runtime(false);
            let mut capture =
                CaptureSession::with_runtime_options(runtime, target, options).unwrap();
            capture.refresh_interval = NEVER;
            let frame = capture.capture().unwrap();
            let wrong = frame.image.enumerate_pixels().find(|(.., p)| p.0 != half);
            assert_eq!(wrong, None, "{case}: first frame");
            assert!(
                capture
                    .next_frame(Duration::from_millis(20))
                    .unwrap()
                    .is_none()
            );
            fixture
                .command
                .send(Command::DamageRect {
                    rect: damage,
                    color: QUARTER,
                })
                .unwrap();
            let frame = capture.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            let wrong = frame.image.enumerate_pixels().find(|&(x, y, p)| {
                let damaged = inside(damage, x as i32, y as i32);
                p.0 != if damaged { quarter } else { half }
            });
            assert_eq!(wrong, None, "{case}: second frame");
            if ext {
                // An update of the damaged rows alone.
                assert_eq!(capture.bytes_read(), [WHOLE + 5 * ROW], "{case}");
            }
        }
    }
}

/// A still capturer whose connections go to `fixture`, and how many it opened.
fn still_capturer(fixture: &Fixture) -> (StillCapturer, Arc<AtomicUsize>) {
    let opened = Arc::new(AtomicUsize::new(0));
    let (count, mut connect) = (opened.clone(), fixture.connector());
    let stills = StillCapturer::with_open(move |target| {
        count.fetch_add(1, Ordering::SeqCst);
        CaptureSession::with_runtime_options(connect()?, target, CaptureOptions::default())
    });
    (stills, opened)
}

/// Between still captures only the connection remains: the client keeps no
/// slot, and the compositor has destroyed every session and buffer.
#[track_caller]
fn assert_released(fixture: &Fixture, stills: &StillCapturer) {
    assert_eq!(stills.kept_slots(), 0, "slots kept");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let (sessions, buffers) = {
            let s = fixture.stats.lock().unwrap();
            let sessions = (s.sessions, s.destroyed_sessions);
            (sessions, (s.buffers, s.destroyed_buffers))
        };
        if sessions.0 == sessions.1 && buffers.0 == buffers.1 {
            return;
        }
        let alive = format!("sessions {sessions:?}, buffers {buffers:?} (made, destroyed)");
        assert!(Instant::now() < deadline, "{alive}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn still_captures_share_one_connection_and_keep_nothing_between_them() {
    let fixture = Fixture::new(Config::default());
    let (mut stills, opened) = still_capturer(&fixture);
    let mut size = |target| {
        let frame = stills.capture(target).unwrap();
        assert_released(&fixture, &stills);
        frame.image.dimensions()
    };
    let window = |id: &str| CaptureTarget::Toplevel(id.into());
    assert_eq!(size(window("stable-window")), SIZE);
    // That window closes after its capture, and a new one maps.
    fixture.command.send(Command::Stop).unwrap();
    fixture.command.send(Command::Map("late-window")).unwrap();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(size(window("late-window")), SIZE);
    let rect = Rect {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    };
    let output = "LEFT".into();
    assert_eq!(
        size(CaptureTarget::Region(Region { output, rect })),
        (30, 40)
    );
    let desktop = Rect {
        x: -25,
        y: 60,
        width: 50,
        height: 20,
    };
    assert_eq!(size(CaptureTarget::DesktopRect(desktop)), (50, 20));
    assert_eq!(opened.load(Ordering::SeqCst), 1, "one connection");
    {
        let stats = fixture.stats.lock().unwrap();
        assert_eq!(stats.window_ids, ["stable-window", "late-window"]);
        assert_eq!(stats.sessions, 5);
    }
    // A missing window fails once, without trying another connection...
    let error = stills.capture(window("closed-window")).unwrap_err();
    assert!(error.to_string().contains("no longer available"), "{error}");
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    // ...and the error drops the connection: the next capture opens another.
    let frame = stills.capture(window("stable-window")).unwrap();
    assert_eq!(frame.image.dimensions(), SIZE);
    assert_eq!(opened.load(Ordering::SeqCst), 2, "a new connection");
}

#[test]
fn still_captures_replace_a_connection_the_compositor_closed() {
    let fixture = Fixture::new(Config::default());
    let (mut stills, opened) = still_capturer(&fixture);
    let window = || CaptureTarget::Toplevel("stable-window".into());
    stills.capture(window()).unwrap();
    // The compositor closes the idle connection.
    let (reply, closed) = mpsc::channel();
    fixture.command.send(Command::Disconnect(reply)).unwrap();
    closed.recv_timeout(Duration::from_secs(1)).unwrap();
    let frame = stills
        .capture(window())
        .expect("a capture over a new connection");
    assert_eq!(frame.image.dimensions(), SIZE);
    assert_eq!(opened.load(Ordering::SeqCst), 2);
}

#[test]
fn a_failed_still_capture_drops_its_connection() {
    // Every frame is rejected, so each capture fails after its retries.
    let fixture = Fixture::new(Config {
        reject: true,
        ..Config::default()
    });
    let opened = Arc::new(AtomicUsize::new(0));
    let (count, mut connect) = (opened.clone(), fixture.connector());
    let mut stills = StillCapturer::with_open(move |target| {
        count.fetch_add(1, Ordering::SeqCst);
        let options = CaptureOptions::default();
        let mut session = CaptureSession::with_runtime_options(connect()?, target, options)?;
        session.constraints_grace = Duration::from_millis(20);
        Ok(session)
    });
    for connections in [1, 2] {
        let window = CaptureTarget::Toplevel("stable-window".into());
        let error = stills.capture(window).unwrap_err().to_string();
        assert!(error.contains("BufferConstraints"), "{error}");
        assert_eq!(opened.load(Ordering::SeqCst), connections);
    }
}

#[test]
fn still_sessions_hand_their_image_out() {
    for still in [false, true] {
        let mut fixture = Fixture::new(Config::default());
        let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
        capture.still = still;
        capture.capture().unwrap();
        // A stream keeps its first frame for updates in place; a still
        // capture has no next frame, so its image is handed out, not copied.
        assert_eq!(capture.kept_images(), [!still], "still {still}");
    }
}

#[test]
fn retargeting_ignores_events_of_the_old_target() {
    let mut fixture = Fixture::new(Config::default());
    let mut capture = fixture.capture(CaptureTarget::Toplevel("stable-window".into()));
    capture.capture().unwrap();
    // The window closes after its capture; that session's stop is unread.
    fixture.command.send(Command::Stop).unwrap();
    thread::sleep(Duration::from_millis(20));
    capture
        .retarget(CaptureTarget::Output("LEFT".into()))
        .unwrap();
    assert_eq!(capture.capture().unwrap().image.dimensions(), SIZE);
}

#[test]
fn retargeting_ignores_the_sync_of_a_replaced_wlr_frame() {
    let mut fixture = Fixture::new(Config {
        ext: false,
        ..idle()
    });
    let mut capture = fixture.capture(CaptureTarget::Output("LEFT".into()));
    let interval = Duration::from_millis(200);
    capture.refresh_interval = interval;
    capture.capture().unwrap();
    // The next frame waits for damage until the refresh replaces it.
    assert!(
        capture
            .next_frame(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
    thread::sleep(interval + Duration::from_millis(50));
    // That destroys the frame and sends a sync, whose reply is still unread.
    assert!(capture.next_frame(Duration::ZERO).unwrap().is_none());
    let rect = Rect {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    };
    let output = "RIGHT".into();
    let region = CaptureTarget::Region(Region { output, rect });
    capture.retarget(region).unwrap();
    assert_eq!(capture.capture().unwrap().image.dimensions(), (30, 40));
    assert_eq!(fixture.stats.lock().unwrap().replaced, 1);
}
