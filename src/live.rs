//! Capture orchestration and local session lifecycle. HTTP and viewer state are
//! independent of the compositor, so the same path can be exercised by --demo.
mod config;
mod http;
mod state;
pub(crate) mod status;
#[cfg(test)]
mod tests;

use crate::{capture::CaptureRequest, portal::Selection};
use anyhow::{Context, Result, bail, ensure};
pub use config::LiveConfig;
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
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const BOUNDARY: &str = "omabeamframe";
pub const LIVE_PORT: u16 = 9847;
const ERROR_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSource {
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
            Self::Window { label, .. } => label.clone(),
            Self::Output { name } => format!("Output {name}"),
            Self::Region { output, w, h, .. } => format!("Region {output} {w}×{h}"),
        }
    }

    fn request(&self) -> Result<CaptureRequest> {
        match self {
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
    ensure!(
        current_status().is_none(),
        "a share is already running; stop it before starting another"
    );
    let session = LiveSession::start_with_config(source, config)?;
    run_session(session)
}

/// Synthetic frames use the real encoder, HTTP server, viewer accounting, and
/// pacing. Useful for testing on a machine without a Wayland desktop.
pub fn run_demo(config: LiveConfig) -> Result<()> {
    ensure!(current_status().is_none(), "a share is already running");
    let mut counter = 0u32;
    let first = omabeam_capture::demo_frame(counter);
    let session = LiveSession::start_frames("OmaBeam demo".into(), config, first, move |_| {
        counter = counter.wrapping_add(1);
        Ok(Some(omabeam_capture::demo_frame(counter)))
    })?;
    run_session(session)
}

fn run_session(session: LiveSession) -> Result<()> {
    println!("{}", session.url);
    loop {
        let status = session.status();
        write_live_status(&status)?;
        if let Some(ended) = session.frames.inner.lock().unwrap().ended {
            if ended.elapsed() >= ERROR_GRACE {
                bail!("{}", status.stats.error.as_deref().unwrap_or("share ended"));
            }
        }
        if session.stop.load(Ordering::SeqCst) {
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
}

impl LiveSession {
    pub fn start(source: LiveSource) -> Result<Self> {
        Self::start_with_config(source, LiveConfig::default())
    }

    pub fn start_with_config(source: LiveSource, config: LiveConfig) -> Result<Self> {
        config.validate()?;
        let mut capturer =
            CaptureSession::new_with_cursor(source.request()?.target()?, config.cursor)
                .context("live share capture initialization failed")?;
        let first = capturer.capture()?;
        Self::start_frames(source.label(), config, first, move |timeout| {
            capturer.next_frame(timeout)
        })
    }

    fn start_frames(
        title: String,
        config: LiveConfig,
        first: CapturedFrame,
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
        let frames = Arc::new(FrameState::new(title.clone()));
        publish_frame(&frames, first, &config)?;
        let stop = Arc::new(AtomicBool::new(false));
        let capture_frames = frames.clone();
        let capture_stop = stop.clone();
        let capture = thread::Builder::new()
            .name("omabeam-capture".into())
            .spawn(move || capture_loop(next, config, capture_frames, capture_stop))?;
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
        };
        write_live_status(&session.status())?;
        Ok(session)
    }

    fn status(&self) -> LiveStatus {
        LiveStatus {
            pid: std::process::id(),
            starttime: status::self_starttime(),
            url: self.url.clone(),
            title: self.title.clone(),
            stats: self.frames.stats(),
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
        if failed {
            let _ = write_live_status(&self.status());
        } else {
            clear_own_status(&self.url);
        }
    }
}

fn publish_frame(frames: &FrameState, frame: CapturedFrame, config: &LiveConfig) -> Result<()> {
    let (jpeg, width, height) = frame.jpeg_scaled(config.quality, config.max_width)?;
    frames.publish(jpeg, width, height);
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
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                publish_frame(&frames, frame, &config)?;
            }
            // Viewer changes wake the wait so a new viewer need not wait out
            // a full idle second. Spurious wakeups retain the original deadline.
            let mut data = frames.inner.lock().unwrap();
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let interval = config.interval(frames.viewers.load(Ordering::SeqCst));
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
