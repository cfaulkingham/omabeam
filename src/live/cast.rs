//! Native Cast sessions own capture and the selected receiver. No HTTP viewer
//! or WebRTC listener is created for this destination.
use super::{
    h264::{AdaptiveEncoder, YuvConverter},
    signals::SessionSignals,
    state::FrameState,
    *,
};
use omabeam_cast::{Helper, Receiver, VideoConfig};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CastStats {
    pub session_id: String,
    pub receiver_id: String,
    pub receiver_name: String,
    pub connection: String,
    pub encoder: String,
    pub target_bitrate: u32,
    /// Bitrate agreed at negotiation. Loss can dip below it; a quiet interval
    /// climbs back. The bandwidth estimate must not replace this ceiling.
    #[serde(default)]
    pub negotiated_bitrate: u32,
    pub accepted_frames: u64,
    /// Transport ACK/cancellation count; it does not prove TV presentation.
    pub released_frames: u64,
    pub dropped_frames: u64,
    #[serde(default)]
    pub retransmitted_packets: u64,
    pub rtt_us: u64,
    #[serde(default)]
    pub control_heartbeats: u64,
}

fn helper_path() -> Result<PathBuf> {
    let path = std::env::current_exe()?.with_file_name("omabeam-cast");
    ensure!(
        path.is_file(),
        "Native Cast helper is unavailable; build/install omabeam-cast beside omabeam"
    );
    Ok(path)
}

pub fn available() -> bool {
    helper_path().is_ok()
}

/// Start one independently owned Cast session. A status file alone is not
/// readiness: wait for negotiation plus receiver transport feedback.
pub fn spawn_daemon(id: &str, source: &LiveSource, config: &LiveConfig) -> Result<()> {
    ensure!(current_status().is_none(), "Already sharing live");
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(config.to_cli_args())
        .args(["--cast", id, "--"])
        .args(source.to_cli_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(status::open_live_log()?);
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    let mut child = command.spawn().context("Failed to start Cast session")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let result = (|| {
        while Instant::now() < deadline {
            if let Some(current) = status::read_status()?.filter(|s| s.pid == child.id()) {
                if let Some(error) = current.stats.error {
                    bail!("{error}");
                }
                if current.stats.cast.as_ref().is_some_and(|cast| {
                    cast.connection == "streaming"
                        && cast.accepted_frames > 0
                        && cast.released_frames > 0
                }) {
                    return Ok(());
                }
            }
            if child.try_wait()?.is_some() {
                let log = std::fs::read_to_string(status::log_path()?).unwrap_or_default();
                bail!(
                    "Cast session could not start: {}",
                    status::cap_bytes(log.trim(), 1500)
                );
            }
            thread::sleep(Duration::from_millis(40));
        }
        bail!("Cast receiver did not become ready within 60 seconds")
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        if let Ok(_lock) = status::session_lock() {
            let _ = crate::hypr::desktop::recover();
            if status::read_status()
                .ok()
                .flatten()
                .is_some_and(|s| s.pid == child.id())
            {
                clear_live_status();
            }
        }
    }
    result
}

fn plain(value: &str, max: usize) -> String {
    let text: String = value.chars().filter(|c| !c.is_control() &&
        !matches!(*c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect();
    status::cap_bytes(&text, max)
}

pub fn discover(duration: Duration) -> Result<Vec<Receiver>> {
    discover_until(duration, || Ok(false))
}

fn discover_until(
    duration: Duration,
    cancelled: impl Fn() -> Result<bool>,
) -> Result<Vec<Receiver>> {
    let deadline = Instant::now() + duration.min(Duration::from_secs(30));
    let mut helper = Helper::spawn_timeout(&helper_path()?, None, duration)?;
    helper.discover()?;
    let mut found = BTreeMap::new();
    while Instant::now() < deadline {
        if cancelled()? {
            break;
        }
        if let Some(event) = helper.event(Duration::from_millis(100))? {
            match event["event"].as_str() {
                Some("receiver") => {
                    let mut receiver: Receiver = serde_json::from_value(event)?;
                    receiver.name = plain(&receiver.name, 160);
                    receiver.model = plain(&receiver.model, 100);
                    if receiver.id.len() <= 256 && found.len() < 128 {
                        found.insert(receiver.id.clone(), receiver);
                    }
                }
                Some("receiver_removed") => {
                    if let Some(id) = event["id"].as_str() {
                        found.remove(id);
                    }
                }
                Some("error") => bail!("Cast discovery: {}", event["message"]),
                _ => {}
            }
        }
    }
    Ok(found.into_values().collect())
}

fn receiver_session(event: &Value) -> Result<String> {
    let id = event["receiver_session"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .context("Missing receiver session identity")?;
    Ok(id.to_owned())
}

fn resumable(event: &Value) -> bool {
    event["event"] == "error"
        && matches!(
            event["code"].as_str(),
            Some("connection" | "receiver_timeout")
        )
}

struct ResumeSession<'a> {
    receiver: &'a Receiver,
    video: &'a VideoConfig,
    certificate: Option<&'a Path>,
    id: &'a str,
}

impl ResumeSession<'_> {
    fn connect(
        &self,
        signals: &SessionSignals,
        frames: &FrameState,
        cast: &mut CastStats,
        status: &mut LiveStatus,
        writer: &mut status::StatusWriter,
    ) -> Result<Option<Helper>> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let cancelled = || -> Result<bool> {
            if signals.stopped() {
                return Ok(true);
            }
            let data = frames.inner.lock().unwrap();
            ensure!(
                data.ended.is_none(),
                "{}",
                data.error.as_deref().unwrap_or("Capture ended")
            );
            Ok(false)
        };
        cast.connection = "reconnecting".into();
        status.stats.viewers = 0;
        status.stats.cast = Some(cast.clone());
        writer.force(status)?;
        let mut last_error = String::from("Receiver is unavailable on the local network");
        while Instant::now() < deadline {
            if cancelled()? {
                return Ok(None);
            }
            // Test endpoints are explicit loopback fixtures. Production always
            // re-resolves the same stable discovery ID, never a display name.
            let addresses = if self.certificate.is_some() {
                self.receiver.addresses.clone()
            } else {
                discover_until(
                    Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
                    &cancelled,
                )?
                .into_iter()
                .find(|r| r.id == self.receiver.id)
                .map(|r| r.addresses)
                .unwrap_or_default()
            };
            if cancelled()? {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                break;
            }
            if let Some(endpoint) = addresses.iter().find(|a| a.is_ipv4()).or(addresses.first()) {
                let mut helper = Helper::spawn_timeout(
                    &helper_path()?,
                    self.certificate,
                    deadline.saturating_duration_since(Instant::now()),
                )?;
                helper.resume(*endpoint, self.video, self.id)?;
                let attempt_deadline = deadline.min(Instant::now() + Duration::from_secs(4));
                while Instant::now() < attempt_deadline {
                    if cancelled()? {
                        return Ok(None);
                    }
                    match helper.event(Duration::from_millis(50)) {
                        Ok(Some(event)) if event["event"] == "negotiated" => {
                            ensure!(
                                receiver_session(&event)? == self.id,
                                "Receiver session changed during recovery"
                            );
                            apply_event(&event, cast)?;
                            cast.accepted_frames = 0;
                            cast.released_frames = 0;
                            cast.dropped_frames = 0;
                            cast.retransmitted_packets = 0;
                            return Ok(Some(helper));
                        }
                        Ok(Some(event)) if event["event"] == "error" => {
                            // Authentication/protocol/ownership failures are
                            // terminal; never launch over different content.
                            if !resumable(&event) {
                                apply_event(&event, cast)?;
                            }
                            last_error = event["message"]
                                .as_str()
                                .unwrap_or("Connection lost")
                                .into();
                            break;
                        }
                        Ok(Some(event))
                            if event["event"] == "state" && event["state"] == "ended" =>
                        {
                            bail!("Receiver ended the mirroring session during recovery");
                        }
                        Err(error) => {
                            last_error = format!("{error:#}");
                            break;
                        }
                        _ => {}
                    }
                }
                helper.abandon();
            }
            // Pace retries while preserving prompt Stop/capture-loss handling.
            let retry = deadline.min(Instant::now() + Duration::from_millis(500));
            while Instant::now() < retry {
                if cancelled()? {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
        bail!(
            "Cast could not resume within 15 seconds: {}",
            status::cap_bytes(&last_error, 1024)
        )
    }
}

pub fn run_source(id: &str, source: LiveSource, config: LiveConfig) -> Result<()> {
    run_selected(id, Some(source), config)
}

/// Synthetic capture with normal discovery and production receiver trust.
pub fn run_receiver_demo(id: &str, config: LiveConfig) -> Result<()> {
    run_selected(id, None, config)
}

/// Held from before any slow startup step until the Cast exits: its stop
/// signals, and the session lock whose record lets `--stop` find it.
struct Owner {
    signals: SessionSignals,
    _lock: std::fs::File,
}

/// Takes the session lock before discovery, which runs for seconds, so
/// `--stop` can find this Cast through the lock record meanwhile. A stop
/// then ends discovery and startup (`None`) before anything connects.
fn discover_selected(
    id: &str,
    stopped: &dyn Fn() -> bool,
    discover: impl FnOnce(&dyn Fn() -> Result<bool>) -> Result<Vec<Receiver>>,
) -> Result<Option<(std::fs::File, Receiver, SocketAddr)>> {
    let lock = status::session_lock_owned()?;
    crate::hypr::desktop::recover()?;
    let receivers = discover(&|| Ok(stopped()))?;
    if stopped() {
        return Ok(None);
    }
    let receiver = receivers
        .into_iter()
        .find(|r| r.id == id)
        .context("Selected Cast receiver is unavailable; run --cast-devices again")?;
    let endpoint = receiver
        .addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or(receiver.addresses.first())
        .copied()
        .context("Receiver has no usable address")?;
    Ok(Some((lock, receiver, endpoint)))
}

fn run_selected(id: &str, source: Option<LiveSource>, config: LiveConfig) -> Result<()> {
    config.validate()?;
    let signals = SessionSignals::new()?;
    let Some((lock, receiver, endpoint)) =
        discover_selected(id, &|| signals.stopped(), |cancelled| {
            discover_until(Duration::from_secs(5), cancelled)
        })?
    else {
        stopped_while_starting();
        return Ok(());
    };
    let owner = Owner {
        signals,
        _lock: lock,
    };
    run(owner, source, receiver, endpoint, config, None)
}

/// Explicit development command for exercising the real capture/encoder
/// pipeline with generated frames and an authenticated software receiver.
pub fn run_demo(endpoint: SocketAddr, certificate: &Path, config: LiveConfig) -> Result<()> {
    let receiver = Receiver {
        id: "software-test".into(),
        name: "Software test receiver".into(),
        model: "Open Screen".into(),
        busy: false,
        addresses: vec![endpoint],
    };
    config.validate()?;
    let signals = SessionSignals::new()?;
    let lock = status::session_lock_owned()?;
    crate::hypr::desktop::recover()?;
    let owner = Owner {
        signals,
        _lock: lock,
    };
    run(owner, None, receiver, endpoint, config, Some(certificate))
}

struct CaptureWorker {
    stop: Arc<AtomicBool>,
    frames: Arc<FrameState>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.frames.wake();
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

/// Callers validate `config`, then take the `Owner` and recover.
fn run(
    owner: Owner,
    source: Option<LiveSource>,
    receiver: Receiver,
    endpoint: SocketAddr,
    mut config: LiveConfig,
    certificate: Option<&Path>,
) -> Result<()> {
    let Owner { signals, _lock } = owner;
    // Stopped before anything is published, e.g. by --stop via the lock record.
    if signals.stopped() {
        stopped_while_starting();
        return Ok(());
    }
    config.fps = config.fps.min(30);
    config.webrtc = true; // Request shared raw H.264 frames; no WebRTC service.
    let (width, height) = if config.max_width.is_some_and(|w| w >= 1920) {
        (1920, 1080)
    } else {
        (1280, 720)
    };
    let video = VideoConfig {
        width,
        height,
        fps: config.fps,
        bitrate: config.h264_bitrate,
    };
    video.validate()?;
    let mut cast = CastStats {
        session_id: random_token()?,
        receiver_id: plain(&receiver.id, 256),
        receiver_name: plain(&receiver.name, 160),
        connection: "connecting".into(),
        target_bitrate: video.bitrate,
        ..Default::default()
    };
    let title = source
        .as_ref()
        .map_or_else(|| "OmaBeam Cast demo".into(), LiveSource::label);
    let mut session = LiveStatus {
        pid: std::process::id(),
        starttime: status::self_starttime(),
        url: String::new(),
        title: title.clone(),
        stats: StreamStats {
            state: "live".into(),
            source: title.clone(),
            width,
            height,
            cast: Some(cast.clone()),
            ..Default::default()
        },
    };
    let mut writer = status::StatusWriter::new();
    writer.force(&session)?;
    let result = (|| -> Result<()> {
        let mut helper = Helper::spawn(&helper_path()?, certificate)?;
        helper.connect(endpoint, &video)?;
        let deadline = Instant::now() + Duration::from_secs(45);
        let receiver_session_id = loop {
            if signals.stopped() {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "Cast negotiation timed out");
            if let Some(event) = helper.event(Duration::from_millis(50))? {
                apply_event(&event, &mut cast)?;
                session.stats.cast = Some(cast.clone());
                writer.force(&session)?;
                if event["event"] == "negotiated" {
                    break receiver_session(&event)?;
                }
                if event["event"] == "state" && event["state"] == "ended" {
                    bail!("Receiver ended the session before media negotiation");
                }
            }
        };
        // Do not create an extended output until this receiver accepts media,
        // or once the share has been stopped.
        if signals.stopped() {
            return Ok(());
        }
        let _display = match &source {
            Some(LiveSource::Extend(desktop)) => {
                Some(crate::hypr::desktop::VirtualDisplay::create(desktop)?)
            }
            _ => None,
        };
        let mut capturer = match &source {
            Some(source) => {
                let request = match &_display {
                    Some(display) => CaptureRequest::Output(display.name().to_owned()),
                    None => source.request()?,
                };
                Some(
                    CaptureSession::with_options(request.target()?, stream_options(config.cursor))
                        .context("Cast capture initialization failed")?,
                )
            }
            None => None,
        };
        let frames = Arc::new(FrameState::new(title));
        frames.cast_viewers.store(1, Ordering::SeqCst);
        let first = match &mut capturer {
            Some(c) => c.capture()?,
            None => omabeam_capture::demo_frame(0),
        };
        publish_frame(&frames, first, &config, Duration::ZERO)?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut worker = CaptureWorker {
            stop: stop.clone(),
            frames: frames.clone(),
            thread: None,
        };
        let (capture_config, capture_frames, capture_stop) =
            (config.clone(), frames.clone(), stop.clone());
        worker.thread = Some(
            thread::Builder::new()
                .name("omabeam-cast-capture".into())
                .spawn(move || {
                    let mut index = 0u32;
                    capture_loop(
                        move |timeout| match &mut capturer {
                            Some(c) => c.next_frame(timeout),
                            None => {
                                index = index.wrapping_add(1);
                                Ok(Some(omabeam_capture::demo_frame(index)))
                            }
                        },
                        capture_config,
                        capture_frames,
                        capture_stop,
                    );
                })?,
        );
        let mut converter = YuvConverter::default();
        config.h264_bitrate = cast.target_bitrate;
        let mut encoder = AdaptiveEncoder::new(&config)?;
        // A two-second IDR spends the whole bitrate budget and the next
        // frames are blocky. Recovery still forces an IDR on picture loss.
        encoder.idr_only_when_requested()?;
        let origin = Instant::now();
        let mut last_frame = origin;
        let mut last_status = origin;
        let mut last_generation = 0;
        let mut sequence = 0u64;
        let mut waiting = None;
        let mut force = true;
        let mut rate = cast.target_bitrate;
        let mut reconnect = false;
        loop {
            if signals.stopped() {
                break;
            }
            while !reconnect {
                let event = match helper.event(Duration::ZERO) {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => {
                        reconnect = true;
                        break;
                    }
                };
                if resumable(&event) {
                    reconnect = true;
                    break;
                }
                apply_event(&event, &mut cast)?;
                match event["event"].as_str() {
                    Some("state") if event["state"] == "ended" => return Ok(()),
                    Some("keyframe") => force = true,
                    Some("feedback") => force |= event["keyframe"].as_bool().unwrap_or(false),
                    Some("frame") if event["sequence"].as_u64() == Some(sequence) => {
                        match frame_admit(&event) {
                            FrameAdmit::Accepted { needs_keyframe } => {
                                waiting = None;
                                force |= needs_keyframe;
                            }
                            FrameAdmit::Deferred => {
                                // The helper still holds this access unit and
                                // will admit it when the receiver catches up.
                                waiting = Some(Instant::now());
                                force = false;
                            }
                            FrameAdmit::Discarded => {
                                waiting = None;
                                force = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if reconnect {
                helper.abandon();
                let resume = ResumeSession {
                    receiver: &receiver,
                    video: &video,
                    certificate,
                    id: &receiver_session_id,
                };
                match resume.connect(&signals, &frames, &mut cast, &mut session, &mut writer)? {
                    Some(next) => helper = next,
                    None => return Ok(()),
                }
                waiting = None;
                force = true;
                last_frame = Instant::now() - Duration::from_secs(1);
                reconnect = false;
            }
            let (generation, raw) = {
                let data = frames.inner.lock().unwrap();
                ensure!(
                    data.ended.is_none(),
                    "{}",
                    data.error.as_deref().unwrap_or("Capture ended")
                );
                (
                    data.generation,
                    data.raw.clone().context("No captured frame")?,
                )
            };
            if let Some(sent) = waiting {
                ensure!(
                    Instant::now().duration_since(sent) < Duration::from_secs(2),
                    "Cast helper stopped accepting frames"
                );
            } else if generation != last_generation
                || last_frame.elapsed()
                    >= if force {
                        config.interval(1)
                    } else {
                        Duration::from_secs(1)
                    }
            {
                let target = cast.target_bitrate;
                if target != rate {
                    encoder.set_bitrate(target)?;
                    rate = target;
                }
                let at = Instant::now();
                // Unchanged desktops still need IDR recovery and keepalive.
                let captured = if generation != last_generation {
                    raw.captured_at
                } else {
                    at
                };
                if at.duration_since(captured) > Duration::from_millis(250) {
                    // The latest retained image can be old on a static
                    // desktop. Resend it as a fresh presentation on the next
                    // iteration instead of waiting forever for pixel damage.
                    last_generation = generation;
                    force = true;
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let yuv = converter.fit(&raw.frame, width, height)?;
                let pts = at.duration_since(origin).as_micros() as u64;
                let (bytes, _) = encoder.encode(yuv, pts as i64, force)?;
                sequence += 1;
                if let Err(error) =
                    helper.frame(sequence, pts, captured.elapsed().as_micros() as u64, &bytes)
                {
                    if error.downcast_ref::<std::io::Error>().is_some() {
                        reconnect = true;
                        continue;
                    }
                    return Err(error);
                }
                cast.encoder = encoder.name.clone();
                force = false;
                last_generation = generation;
                last_frame = at;
                waiting = Some(Instant::now());
            }
            if last_status.elapsed() >= Duration::from_millis(250) {
                session.stats = frames.stats();
                session.stats.viewers = usize::from(cast.connection == "streaming");
                session.stats.width = width;
                session.stats.height = height;
                session.stats.cast = Some(cast.clone());
                // Readiness and connection changes are written at once.
                writer.write(&session)?;
                last_status = Instant::now();
            }
            thread::sleep(Duration::from_millis(2));
        }
        // The receiver may have disconnected concurrently with the user's
        // Stop. Local cleanup still completes through the owned helper Drop.
        let _ = helper.stop();
        Ok(())
    })();
    match result {
        Ok(()) => {
            if status::read_status()?.is_some_and(|current| {
                current.pid == session.pid
                    && current
                        .stats
                        .cast
                        .as_ref()
                        .is_some_and(|c| c.session_id == cast.session_id)
            }) {
                clear_live_status();
            }
            Ok(())
        }
        Err(error) => {
            session.stats.state = "ended".into();
            session.stats.error = Some(format!("Casting stopped: {error:#}"));
            cast.connection = "failed".into();
            session.stats.cast = Some(cast);
            let _ = writer.force(&session);
            Err(error)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameAdmit {
    Accepted {
        needs_keyframe: bool,
    },
    /// The receiver's in-flight window is full. The same access unit stays
    /// queued in the helper, so the next encode must not be a new frame.
    Deferred,
    /// The helper dropped the access unit. The next encoded frame must be an IDR.
    Discarded,
}

fn frame_admit(event: &Value) -> FrameAdmit {
    let accepted = event["accepted"].as_bool().unwrap_or(false);
    let retry = event["retry"].as_bool().unwrap_or(false);
    let needs_keyframe = event["keyframe"].as_bool().unwrap_or(!accepted && !retry);
    if accepted {
        FrameAdmit::Accepted { needs_keyframe }
    } else if retry && !needs_keyframe {
        FrameAdmit::Deferred
    } else {
        FrameAdmit::Discarded
    }
}

/// Hold the negotiated rate until packets are retransmitted or a frame is
/// actually discarded. A quiet interval climbs back by 150 kbit/s. The
/// bandwidth estimate is not an input: it under-reads a sender that is not
/// filling the link.
fn next_cast_bitrate(target: u32, negotiated: u32, loss: bool) -> u32 {
    const MIN_BITRATE: u32 = 300_000;
    const RECOVERY_STEP: u32 = 150_000;
    let ceiling = if negotiated == 0 {
        target.max(MIN_BITRATE)
    } else {
        negotiated.max(MIN_BITRATE)
    };
    let target = target.clamp(MIN_BITRATE, ceiling);
    if loss {
        return target
            .saturating_mul(3)
            .saturating_div(4)
            .clamp(MIN_BITRATE, ceiling);
    }
    if target < ceiling {
        return target.saturating_add(RECOVERY_STEP).min(ceiling);
    }
    target
}

fn apply_event(event: &Value, stats: &mut CastStats) -> Result<()> {
    match event["event"].as_str() {
        Some("error") => bail!(
            "{}: {}",
            event["code"].as_str().unwrap_or("Cast"),
            event["message"].as_str().unwrap_or("Receiver error")
        ),
        Some("state") => stats.connection = plain(event["state"].as_str().unwrap_or("unknown"), 40),
        Some("negotiated") => {
            stats.connection = "starting".into();
            stats.target_bitrate = event["bitrate"]
                .as_u64()
                .context("Missing negotiated bitrate")? as u32;
            stats.negotiated_bitrate = stats.target_bitrate;
            stats.dropped_frames = 0;
            stats.retransmitted_packets = 0;
        }
        Some("feedback") => {
            let dropped = event["dropped"].as_u64().unwrap_or(0);
            let retransmitted = event["retransmitted_packets"].as_u64().unwrap_or(0);
            let loss =
                dropped > stats.dropped_frames || retransmitted > stats.retransmitted_packets;
            stats.dropped_frames = dropped;
            stats.retransmitted_packets = retransmitted;
            stats.accepted_frames = event["accepted"].as_u64().unwrap_or(0);
            stats.released_frames = event["released"].as_u64().unwrap_or(0);
            stats.rtt_us = event["rtt_us"].as_u64().unwrap_or(0);
            stats.control_heartbeats = event["control_heartbeats"].as_u64().unwrap_or(0);
            stats.target_bitrate =
                next_cast_bitrate(stats.target_bitrate, stats.negotiated_bitrate, loss);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(bitrate: u32) -> CastStats {
        CastStats {
            target_bitrate: bitrate,
            negotiated_bitrate: bitrate,
            ..CastStats::default()
        }
    }

    fn feedback(bitrate: u32, dropped: u64, retransmitted: u64) -> Value {
        serde_json::json!({
            "event": "feedback",
            "bitrate": bitrate,
            "accepted": 10,
            "released": 10,
            "dropped": dropped,
            "retransmitted_packets": retransmitted,
            "rtt_us": 12_000,
            "control_heartbeats": 1
        })
    }

    #[test]
    fn feedback_without_loss_keeps_the_negotiated_bitrate() {
        let mut cast = stats(4_000_000);
        apply_event(&feedback(300_000, 0, 0), &mut cast).unwrap();
        assert_eq!(cast.target_bitrate, 4_000_000);
    }

    #[test]
    fn loss_cuts_a_quarter_and_a_quiet_interval_climbs_back() {
        let mut cast = stats(4_000_000);
        apply_event(&feedback(4_000_000, 0, 3), &mut cast).unwrap();
        assert_eq!(cast.target_bitrate, 3_000_000);
        apply_event(&feedback(4_000_000, 2, 3), &mut cast).unwrap();
        assert_eq!(cast.target_bitrate, 2_250_000);
        apply_event(&feedback(300_000, 2, 3), &mut cast).unwrap();
        assert_eq!(cast.target_bitrate, 2_400_000);
        let mut floor = stats(400_000);
        apply_event(&feedback(400_000, 1, 0), &mut floor).unwrap();
        assert_eq!(floor.target_bitrate, 300_000);
    }

    fn receiver(id: &str, addresses: &[&str]) -> Receiver {
        Receiver {
            id: id.into(),
            name: "Living room".into(),
            model: "Chromecast".into(),
            busy: false,
            addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn a_stop_during_discovery_ends_startup_before_connecting() {
        status::tests::with_runtime(|root| {
            let stop = std::cell::Cell::new(false);
            let record = root.join("omabeam/session.lock");
            let selected = discover_selected("tv", &|| stop.get(), |cancelled| {
                // `--stop` finds a Cast that is still discovering through the
                // record in the lock it holds.
                assert!(status::session_lock().is_err(), "lock not held");
                let owner = format!("{} {}\n", std::process::id(), status::self_starttime());
                assert_eq!(std::fs::read_to_string(&record).unwrap(), owner);
                assert!(!cancelled()?);
                stop.set(true); // --stop's SIGTERM
                assert!(cancelled()?, "a stop must end discovery early");
                Ok(Vec::new()) // Nothing found yet.
            });
            assert!(selected.unwrap().is_none());
            assert!(status::read_status().unwrap().is_none());
            // Startup released the lock.
            drop(status::tests::session_lock_when_free());
        });
    }

    #[test]
    fn discovery_selects_the_receiver_by_id_and_prefers_ipv4() {
        let found = || {
            Ok(vec![
                receiver("other", &["192.0.2.9:8009"]),
                receiver("tv", &["[2001:db8::1]:8009", "192.0.2.1:8009"]),
            ])
        };
        status::tests::with_runtime(|_| {
            let (_lock, chosen, endpoint) = discover_selected("tv", &|| false, |_| found())
                .unwrap()
                .unwrap();
            assert_eq!(chosen.id, "tv");
            assert_eq!(endpoint, "192.0.2.1:8009".parse().unwrap());
        });
        // A fresh lock file: a child another test spawns may still share the
        // lock above for a moment after it is dropped.
        status::tests::with_runtime(|_| {
            let error = discover_selected("gone", &|| false, |_| found()).unwrap_err();
            let error = format!("{error:#}");
            assert!(
                error.contains("Selected Cast receiver is unavailable"),
                "{error}"
            );
        });
    }

    #[test]
    fn deferred_frame_stays_in_the_prediction_chain() {
        assert_eq!(
            frame_admit(&serde_json::json!({
                "event": "frame", "accepted": false, "retry": true, "keyframe": false
            })),
            FrameAdmit::Deferred
        );
        assert_eq!(
            frame_admit(&serde_json::json!({
                "event": "frame", "accepted": false, "keyframe": true
            })),
            FrameAdmit::Discarded
        );
        assert_eq!(
            frame_admit(&serde_json::json!({
                "event": "frame", "accepted": true, "keyframe": false
            })),
            FrameAdmit::Accepted {
                needs_keyframe: false
            }
        );
    }
}
