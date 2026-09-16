//! Capture orchestration and local session lifecycle. HTTP and viewer state are
//! independent of the compositor, so the same path can be exercised by --demo.
mod config;
mod desktop;
mod diagnostics;
mod http;
mod signals;
mod state;
pub(crate) mod status;
#[cfg(test)]
mod tests;
mod webrtc;
pub use webrtc::probe_encoder;

use crate::{capture::CaptureRequest, portal::Selection};
use anyhow::{Context, Result, bail, ensure};
pub use config::{EncoderMode, LiveConfig};
pub use desktop::DesktopStats;
use diagnostics::FrameMeasurement;
pub use diagnostics::{StreamDiagnostics, TimingStats, ViewerDiagnostics};
pub use http::viewer_html;
use omabeam_capture::{CaptureSession, CapturedFrame};
use serde::{Deserialize, Serialize};
use state::FrameState;
pub use state::StreamStats;
use std::{
    io::Read,
    net::{IpAddr, TcpListener},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub use webrtc::WebRtcStats;

const BOUNDARY: &str = "omabeamframe";
pub const LIVE_PORT: u16 = 9847;
const ERROR_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSource {
    Extend(crate::hypr::desktop::DesktopConfig),
    Window {
        address: String,
        stable_id: String,
        label: String,
    },
    Output {
        name: String,
    },
    Region {
        output: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
}

impl LiveSource {
    pub fn label(&self) -> String {
        match self {
            Self::Extend(config) => format!(
                "Extended desktop {}×{} · {}",
                config.width,
                config.height,
                config.position.label()
            ),
            Self::Window { label, .. } => label.clone(),
            Self::Output { name } => format!("Output {name}"),
            Self::Region { output, w, h, .. } => format!("Region {output} {w}×{h}"),
        }
    }

    fn request(&self) -> Result<CaptureRequest> {
        match self {
            Self::Extend(_) => bail!("extended display must be created before capture"),
            Self::Window { stable_id, .. } => {
                ensure!(
                    !stable_id.is_empty(),
                    "This window cannot be captured separately. Choose Area to explicitly share its visible screen region."
                );
                Ok(CaptureRequest::Toplevel(stable_id.clone()))
            }
            Self::Output { name } => Ok(CaptureRequest::Output(name.clone())),
            Self::Region { output, x, y, w, h } => Ok(CaptureRequest::Region(Selection::Region {
                output: output.clone(),
                x: *x,
                y: *y,
                w: *w,
                h: *h,
            })),
        }
    }

    pub fn to_cli_args(&self) -> Vec<String> {
        match self {
            Self::Extend(config) => vec![
                "extend".into(),
                config.width.to_string(),
                config.height.to_string(),
                config.scale.to_string(),
                config.position.label().to_lowercase(),
            ],
            Self::Output { name } => vec!["output".into(), name.clone()],
            Self::Window {
                address,
                stable_id,
                label,
            } => vec![
                "window".into(),
                address.clone(),
                stable_id.clone(),
                label.clone(),
            ],
            Self::Region { output, x, y, w, h } => vec![
                "region".into(),
                output.clone(),
                x.to_string(),
                y.to_string(),
                w.to_string(),
                h.to_string(),
            ],
        }
    }

    pub fn from_cli_args(args: &[String]) -> Result<Self> {
        let Some(kind) = args.first().map(String::as_str) else {
            bail!("missing live source");
        };
        match kind {
            "extend" => Ok(Self::Extend(
                crate::hypr::desktop::DesktopConfig::from_args(&args[1..])?,
            )),
            "output" => {
                ensure!(args.len() == 2, "output needs exactly one name");
                let name = args
                    .get(1)
                    .cloned()
                    .filter(|name| !name.is_empty())
                    .context("missing output name")?;
                Ok(Self::Output { name })
            }
            "window" => {
                ensure!(
                    args.len() >= 3,
                    "window needs an address and stable identifier"
                );
                let address = args.get(1).cloned().context("missing window address")?;
                let stable_id = args.get(2).cloned().unwrap_or_default();
                let label = if args.len() > 3 {
                    args[3..].join(" ")
                } else {
                    "Window".into()
                };
                Ok(Self::Window {
                    address,
                    stable_id,
                    label,
                })
            }
            "region" => {
                if args.len() != 6 {
                    bail!("region needs output x y w h");
                }
                Ok(Self::Region {
                    output: args[1].clone(),
                    x: args[2].parse().context("region x")?,
                    y: args[3].parse().context("region y")?,
                    w: args[4].parse().context("region w")?,
                    h: args[5].parse().context("region h")?,
                })
            }
            other => bail!("unknown live source {other}"),
        }
    }
}

pub fn run_headless(source: LiveSource, config: LiveConfig) -> Result<()> {
    let signals = signals::SessionSignals::new()?;
    ensure!(
        current_status().is_none(),
        "a share is already running; stop it before starting another"
    );
    let session = LiveSession::start_with_config(source, config)?;
    run_session(session, &signals)
}

/// Synthetic frames use the real encoder, HTTP server, viewer accounting, and
/// pacing. Useful for testing on a machine without a Wayland desktop.
pub fn run_demo(config: LiveConfig) -> Result<()> {
    let signals = signals::SessionSignals::new()?;
    ensure!(current_status().is_none(), "a share is already running");
    let lock = status::session_lock()?;
    crate::hypr::desktop::recover()?;
    let mut counter = 0u32;
    let started = Instant::now();
    let first = omabeam_capture::demo_frame(counter);
    let mut session = LiveSession::start_frames(
        "OmaBeam demo".into(),
        config,
        first,
        started.elapsed(),
        None,
        move |_| {
            counter = counter.wrapping_add(1);
            Ok(Some(omabeam_capture::demo_frame(counter)))
        },
    )?;
    session.session_lock = Some(lock);
    run_session(session, &signals)
}

fn run_session(session: LiveSession, signals: &signals::SessionSignals) -> Result<()> {
    println!("{}", session.url);
    loop {
        let status = session.status();
        write_live_status(&status)?;
        if let Some(ended) = session.frames.inner.lock().unwrap().ended {
            if ended.elapsed() >= ERROR_GRACE {
                bail!("{}", status.stats.error.as_deref().unwrap_or("share ended"));
            }
        }
        if session.stop.load(Ordering::SeqCst) || signals.stopped() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub fn spawn_daemon(source: &LiveSource, config: &LiveConfig) -> Result<String> {
    ensure!(current_status().is_none(), "Already sharing live.");
    let exe = std::env::current_exe().context("failed to find omabeam")?;
    let log = status::open_live_log().context("failed to open live-share log")?;
    let log_path = status::log_path()?;
    let mut cmd = Command::new(exe);
    cmd.args(config.to_cli_args())
        .arg("--live")
        .arg("--")
        .args(source.to_cli_args());
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("failed to start live share")?;
    let pid = child.id();
    for _ in 0..500 {
        if let Some(status) = current_status().filter(|status| status.pid == pid) {
            if let Some(error) = status.stats.error {
                let _ = stop_and_cleanup();
                bail!("{error}");
            }
            return Ok(status.url);
        }
        if child.try_wait()?.is_some() {
            let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
            bail!("live share failed to start: {}", detail.trim());
        }
        thread::sleep(Duration::from_millis(40));
    }
    let _ = child.kill();
    let _ = child.wait();
    // SIGKILL cannot run destructors. Recover an output created during a hung
    // startup once the child has exited and released the session lock.
    if let Ok(_lock) = status::session_lock() {
        if let Err(error) = crate::hypr::desktop::recover() {
            eprintln!("Extended display cleanup needs a retry with omabeam --stop: {error:#}");
        }
    }
    bail!(
        "live share did not become ready within 20 seconds; see {}",
        log_path.display()
    )
}

pub struct LiveSession {
    pub url: String,
    pub title: String,
    stop: Arc<AtomicBool>,
    frames: Arc<FrameState>,
    capture: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
    rtc_worker: Option<JoinHandle<()>>,
    display: Option<Arc<Mutex<crate::hypr::desktop::VirtualDisplay>>>,
    session_lock: Option<std::fs::File>,
}

impl LiveSession {
    pub fn start(source: LiveSource) -> Result<Self> {
        Self::start_with_config(source, LiveConfig::default())
    }

    pub fn start_with_config(source: LiveSource, config: LiveConfig) -> Result<Self> {
        config.validate()?;
        let lock = status::session_lock()?;
        crate::hypr::desktop::recover()?;
        let display = match &source {
            LiveSource::Extend(config) => Some(Arc::new(Mutex::new(
                crate::hypr::desktop::VirtualDisplay::create(config)?,
            ))),
            _ => None,
        };
        let request = match &display {
            Some(display) => CaptureRequest::Output(display.lock().unwrap().name().to_owned()),
            None => source.request()?,
        };
        let mut capturer = CaptureSession::new_with_cursor(request.target()?, config.cursor)
            .context("live share capture initialization failed")?;
        let started = Instant::now();
        let first = capturer.capture()?;
        let desktop = match &source {
            LiveSource::Extend(config) => {
                Some(Arc::new(desktop::DesktopControl::new(config.clone())))
            }
            _ => None,
        };
        let capture_desktop = desktop.clone();
        let capture_display = display.clone();
        let cursor = config.cursor;
        let mut session = Self::start_frames(
            source.label(),
            config,
            first,
            started.elapsed(),
            desktop,
            move |timeout| {
                if let Some(control) = &capture_desktop
                    && let Some(resize) = control.take_resize()
                {
                    let display = capture_display.as_ref().unwrap().lock().unwrap();
                    let frame = control.apply_resize(resize, |size| {
                        display.resize(size)?;
                        // Reopen capture after a mode switch to discard in-flight
                        // buffers from the old geometry on either capture protocol.
                        capturer = CaptureSession::new_with_cursor(request.target()?, cursor)?;
                        let frame = capturer.capture()?;
                        ensure!(
                            frame.image.dimensions() == (size.width, size.height),
                            "capture did not match the requested display size"
                        );
                        Ok(frame)
                    })?;
                    return Ok(Some(frame));
                }
                capturer.next_frame(timeout)
            },
        )?;
        session.display = display;
        session.session_lock = Some(lock);
        Ok(session)
    }

    fn start_frames(
        title: String,
        config: LiveConfig,
        first: CapturedFrame,
        first_capture_wait: Duration,
        desktop: Option<Arc<desktop::DesktopControl>>,
        next: impl FnMut(Duration) -> Result<Option<CapturedFrame>> + Send + 'static,
    ) -> Result<Self> {
        config.validate()?;
        let token = random_token()?;
        let listener = TcpListener::bind((config.bind, config.port)).with_context(|| {
            format!(
                "cannot bind {}:{}; use --port 0 to choose an available port",
                config.bind, config.port
            )
        })?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let host = match config.bind {
            IpAddr::V4(ip) if ip.is_unspecified() => lan_ip().unwrap_or_else(|| "127.0.0.1".into()),
            IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".into(),
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        };
        let url = format!("http://{host}:{port}/s/{token}/");
        let mut frames = FrameState::new(title.clone());
        frames.desktop = desktop;
        let frames = Arc::new(frames);
        publish_frame(&frames, first, &config, first_capture_wait)?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut rtc_worker = if config.webrtc {
            Some(webrtc::start(&config, &frames, &stop)?)
        } else {
            None
        };
        let capture_frames = frames.clone();
        let capture_stop = stop.clone();
        let capture = match thread::Builder::new()
            .name("omabeam-capture".into())
            .spawn(move || capture_loop(next, config, capture_frames, capture_stop))
        {
            Ok(capture) => capture,
            Err(error) => {
                stop.store(true, Ordering::SeqCst);
                if let Some(worker) = rtc_worker.take() {
                    let _ = worker.join();
                }
                return Err(error.into());
            }
        };
        let server_frames = frames.clone();
        let server_stop = stop.clone();
        let server = match thread::Builder::new()
            .name("omabeam-http".into())
            .spawn(move || http::serve(listener, token, server_frames, server_stop))
        {
            Ok(server) => server,
            Err(error) => {
                stop.store(true, Ordering::SeqCst);
                frames.tick.notify_all();
                let _ = capture.join();
                if let Some(worker) = rtc_worker.take() {
                    let _ = worker.join();
                }
                return Err(error.into());
            }
        };
        let session = Self {
            url,
            title,
            stop,
            frames,
            capture: Some(capture),
            server: Some(server),
            rtc_worker,
            display: None,
            session_lock: None,
        };
        write_live_status(&session.status())?;
        Ok(session)
    }

    fn status(&self) -> LiveStatus {
        let stats = self.frames.stats();
        LiveStatus {
            pid: std::process::id(),
            starttime: status::self_starttime(),
            url: self.url.clone(),
            title: if stats.desktop.is_some() {
                stats.source.clone()
            } else {
                self.title.clone()
            },
            stats,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.frames.tick.notify_all();
        clear_own_status(&self.url);
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        let failed = self.frames.inner.lock().unwrap().ended.is_some();
        self.stop.store(true, Ordering::SeqCst);
        self.frames.tick.notify_all();
        if let Some(capture) = self.capture.take() {
            let _ = capture.join();
        }
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(worker) = self.rtc_worker.take() {
            let _ = worker.join();
        }
        // Release capture before removing its output, while retaining the lock.
        drop(self.display.take());
        if failed {
            let _ = write_live_status(&self.status());
        } else {
            clear_own_status(&self.url);
        }
    }
}

fn publish_frame(
    frames: &FrameState,
    frame: CapturedFrame,
    config: &LiveConfig,
    capture_wait: Duration,
) -> Result<()> {
    let mut config = config.clone();
    if frames.desktop.as_ref().is_some_and(|d| d.stats().matched) {
        config.pixel_mode = omabeam_capture::PixelMode::Native;
        config.max_width = None;
    }
    let encode_started_at = Instant::now();
    let (width, height) = frame.stream_dimensions(config.max_width, config.pixel_mode)?;
    let jpeg = if !config.webrtc || frames.viewers.load(Ordering::SeqCst) > 0 {
        frame
            .jpeg_with_mode(config.quality, config.max_width, config.pixel_mode)?
            .0
    } else {
        Vec::new()
    };
    let measurement = FrameMeasurement {
        pixel_mode: config.pixel_mode,
        capture_width: frame.image.width(),
        capture_height: frame.image.height(),
        logical_width: frame.logical_width,
        logical_height: frame.logical_height,
        capture_wait,
        encode: encode_started_at.elapsed(),
        encode_started_at,
    };
    let raw = config.webrtc.then(|| {
        Arc::new(webrtc::RawFrame {
            frame,
            config: config.clone(),
            captured_at: encode_started_at,
        })
    });
    frames.publish_raw(jpeg, width, height, measurement, raw);
    Ok(())
}

fn capture_loop(
    mut next: impl FnMut(Duration) -> Result<Option<CapturedFrame>>,
    config: LiveConfig,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
) {
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::SeqCst) {
            let started = Instant::now();
            if let Some(frame) = next(Duration::from_millis(250))? {
                let capture_wait = started.elapsed();
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                publish_frame(&frames, frame, &config, capture_wait)?;
            }
            // Viewer changes wake the wait so a new viewer need not wait out
            // a full idle second. Spurious wakeups retain the original deadline.
            let mut data = frames.inner.lock().unwrap();
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let interval = config.interval(frames.viewer_count());
                let Some(left) = interval
                    .checked_sub(started.elapsed())
                    .filter(|d| !d.is_zero())
                else {
                    break;
                };
                data = frames.tick.wait_timeout(data, left).unwrap().0;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("live share capture stopped: {error:#}");
        frames.fail(format!("Sharing stopped: {error:#}"));
        // Keep only diagnostics available briefly. Never reselect a window or
        // serve its last image after losing the source.
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveStatus {
    pub pid: u32,
    #[serde(default)]
    pub starttime: u64,
    pub url: String,
    pub title: String,
    #[serde(flatten)]
    pub stats: StreamStats,
}

pub use status::{
    current_status, latest_status, latest_status_report, pid_alive, status_dir, status_path,
    stop_live_process, write_live_status,
};

pub fn stop_and_cleanup() -> Result<bool> {
    let stopped = stop_live_process();
    let _lock = status::session_lock()?;
    Ok(crate::hypr::desktop::recover()? || stopped)
}

pub fn clear_live_status() {
    status::clear_live_status();
}

fn clear_own_status(url: &str) {
    if current_status().is_some_and(|s| s.pid == std::process::id() && s.url == url) {
        clear_live_status();
    }
}
pub fn copy_text(text: &str) -> bool {
    let mut child = match Command::new("/usr/bin/wl-copy")
        .arg("--")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .env(
            "XDG_RUNTIME_DIR",
            std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default(),
        )
        .env(
            "WAYLAND_DISPLAY",
            std::env::var_os("WAYLAND_DISPLAY").unwrap_or_default(),
        )
        .env("LANG", "C")
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let write = child
        .stdin
        .take()
        .and_then(|mut stdin| std::io::Write::write_all(&mut stdin, text.as_bytes()).ok());
    matches!(child.wait(), Ok(status) if status.success()) && write.is_some()
}

pub fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?
        .read_exact(&mut bytes)
        .context("could not create share token")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
