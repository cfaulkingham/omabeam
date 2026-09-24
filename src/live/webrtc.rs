//! LAN-only, receive-only H.264. One encoder feeds at most eight peers through
//! a one-frame channel. ICE/DTLS/RTP run independently of capture and encoding.
mod encoder;

use super::{
    LiveConfig,
    diagnostics::{Rate, TimingStats, Timings},
    state::FrameState,
};
use anyhow::{Context, Result, ensure};
#[cfg(test)]
use omabeam_capture::CapturedFrame;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, VecDeque},
    io::ErrorKind,
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig,
    change::SdpOffer,
    format::Codec,
    media::{MediaKind, MediaTime, Mid, Pt},
    net::{Protocol, Receive, Transmit},
};

const MAX_PEERS: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_QUEUE_AGE: Duration = Duration::from_millis(250);

fn h264_profile_level_id(width: u32, height: u32, fps: u32, bitrate: u32) -> u32 {
    let frame_mbs = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    let mbs_per_second = frame_mbs * u64::from(fps);
    // Keep level 3.1 as the floor because it is the baseline WebRTC profile
    // browsers commonly offer. Higher levels must cover the actual stream or
    // strict decoders can accept RTP but reject every frame.
    let levels = [
        (31, 3_600, 108_000, 14_000_000),
        (32, 5_120, 216_000, 20_000_000),
        (40, 8_192, 245_760, 20_000_000),
        (41, 8_192, 245_760, 50_000_000),
        (42, 8_704, 522_240, 50_000_000),
        (50, 22_080, 589_824, 135_000_000),
        (51, 36_864, 983_040, 240_000_000),
        (52, 36_864, 2_073_600, 240_000_000),
    ];
    let level = levels
        .into_iter()
        .find(|(_, max_frame, max_rate, max_bitrate)| {
            frame_mbs <= *max_frame
                && mbs_per_second <= *max_rate
                && u64::from(bitrate) <= *max_bitrate
        })
        .map(|(level, _, _, _)| level)
        .unwrap_or(52);
    0x42e000 | level
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebRtcStats {
    pub encoder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_note: Option<String>,
    pub udp_port: u16,
    pub target_bitrate: u32,
    pub peers: usize,
    pub connected: usize,
    pub width: u32,
    pub height: u32,
    pub encoded_frames: u64,
    pub encoded_fps: f64,
    pub encode_ms: TimingStats,
    #[serde(default)]
    pub capture_to_encode_ms: TimingStats,
    #[serde(default)]
    pub convert_ms: TimingStats,
    #[serde(default)]
    pub codec_ms: TimingStats,
    #[serde(default)]
    pub send_queue_ms: TimingStats,
    pub bytes_sent: u64,
    pub outgoing_mbps: f64,
    pub keyframes: u64,
    pub dropped_frames: u64,
    pub failed_peers: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_error: Option<String>,
    pub error: Option<String>,
}
struct Metrics {
    stats: WebRtcStats,
    encode: Timings,
    capture_to_encode: Timings,
    convert: Timings,
    codec: Timings,
    send_queue: Timings,
    output: Rate,
    encoded: Rate,
}

pub(super) struct Service {
    commands: SyncSender<Command>,
    connected: AtomicUsize,
    /// Viewers str0m has reported connected, ever. Unlike `connected`, a join
    /// still shows when another viewer leaves in the same network pass.
    joins: AtomicU64,
    /// A viewer's PLI/FIR, which the encoder throttles; joins never set it.
    keyframe: AtomicBool,
    metrics: Mutex<Metrics>,
    failed: AtomicBool,
    fps: u32,
    frames: std::sync::Weak<FrameState>,
    queued: AtomicBool,
    wake: std::os::unix::net::UnixDatagram,
}
enum Command {
    Offer(
        SdpOffer,
        Option<String>,
        Instant,
        SyncSender<Result<serde_json::Value>>,
    ),
    Close(String),
}
impl Service {
    fn wake_network(&self) {
        // A full socket already contains a wakeup. This must never block
        // capture, encoding or HTTP signaling behind the network worker.
        let _ = self.wake.send(&[1]);
    }
    fn wake_encoder(&self) {
        if let Some(frames) = self.frames.upgrade() {
            frames.wake();
        }
    }
    fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::SeqCst);
        self.wake_encoder();
    }
    /// A viewer connected; str0m reports that once per peer. The encoder
    /// answers every join with an IDR at once, unlike a throttled PLI/FIR.
    fn peer_joined(&self) {
        self.joins.fetch_add(1, Ordering::SeqCst);
        self.wake_encoder();
    }
    pub fn connected(&self) -> usize {
        self.connected.load(Ordering::SeqCst)
    }
    pub fn stats(&self) -> WebRtcStats {
        let metrics = self.metrics.lock().unwrap();
        let mut stats = metrics.stats.clone();
        stats.connected = self.connected();
        stats.encode_ms = metrics.encode.stats(Instant::now());
        stats.capture_to_encode_ms = metrics.capture_to_encode.stats(Instant::now());
        stats.convert_ms = metrics.convert.stats(Instant::now());
        stats.codec_ms = metrics.codec.stats(Instant::now());
        stats.send_queue_ms = metrics.send_queue.stats(Instant::now());
        stats.outgoing_mbps = metrics.output.values(Instant::now()).0;
        stats.encoded_fps = metrics.encoded.values(Instant::now()).1;
        stats
    }
    pub fn offer(&self, body: &[u8], connection: Option<String>) -> Result<serde_json::Value> {
        ensure!(
            !self.failed.load(Ordering::SeqCst),
            "H.264 is unavailable; use JPEG"
        );
        // Bound the negotiated shape before allocating a peer. We never accept
        // audio, incoming video, data channels, simulcast, or renegotiation.
        #[derive(Deserialize)]
        struct Description {
            sdp: String,
        }
        let description: Description = serde_json::from_slice(body)?;
        let lines: Vec<_> = description.sdp.lines().collect();
        ensure!(
            lines.iter().filter(|l| l.starts_with("m=")).count() == 1
                && lines.iter().any(|l| l.starts_with("m=video "))
                && lines.contains(&"a=recvonly")
                && !lines
                    .iter()
                    .any(|l| matches!(*l, "a=sendrecv" | "a=sendonly" | "a=inactive")
                        || l.starts_with("a=simulcast:")
                        || l.starts_with("a=rid:")),
            "offer must contain one receive-only video track"
        );
        let offer: SdpOffer = serde_json::from_slice(body)?;
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .try_send(Command::Offer(offer, connection, Instant::now(), tx))
            .map_err(|_| anyhow::anyhow!("WebRTC busy"))?;
        self.wake_network();
        rx.recv_timeout(Duration::from_secs(3))
            .context("WebRTC signaling timed out")?
    }
    pub fn close(&self, id: String) -> Result<()> {
        ensure!(
            id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid peer identifier"
        );
        self.commands
            .try_send(Command::Close(id))
            .map_err(|_| anyhow::anyhow!("WebRTC busy"))?;
        self.wake_network();
        Ok(())
    }
}

pub(super) fn start(
    config: &LiveConfig,
    frames: &Arc<FrameState>,
    stop: &Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let udp = Udp::new(bind_sockets(config)?)?;
    let port = udp.sockets[0].1.port();
    let (tx, rx) = mpsc::sync_channel(16);
    let (wake, wake_rx) = std::os::unix::net::UnixDatagram::pair()?;
    wake.set_nonblocking(true)?;
    wake_rx.set_nonblocking(true)?;
    let service = Arc::new(Service {
        commands: tx,
        connected: AtomicUsize::new(0),
        joins: AtomicU64::new(0),
        keyframe: AtomicBool::new(true),
        failed: AtomicBool::new(false),
        metrics: Mutex::new(Metrics {
            stats: WebRtcStats {
                encoder: "Waiting for viewer".into(),
                udp_port: port,
                target_bitrate: config.h264_bitrate,
                ..Default::default()
            },
            encode: Timings::default(),
            capture_to_encode: Timings::default(),
            convert: Timings::default(),
            codec: Timings::default(),
            send_queue: Timings::default(),
            output: Rate::new(Instant::now()),
            encoded: Rate::new(Instant::now()),
        }),
        fps: config.fps.min(60),
        frames: Arc::downgrade(frames),
        queued: AtomicBool::new(false),
        wake,
    });
    *frames.rtc.lock().unwrap() = Some(service.clone());
    let (frames, stop, config) = (frames.clone(), stop.clone(), config.clone());
    Ok(thread::Builder::new()
        .name("omabeam-webrtc".into())
        .spawn(move || {
            let (encoded_tx, encoded_rx) = mpsc::sync_channel(1);
            let encoder_service = service.clone();
            let encoder_frames = frames.clone();
            let encoder_stop = stop.clone();
            let encoder = thread::Builder::new()
                .name("omabeam-h264".into())
                .spawn(move || {
                    if let Err(error) = encoder::run(
                        config,
                        encoder_frames,
                        encoder_stop,
                        encoder_service.clone(),
                        encoded_tx,
                    ) {
                        encoder_service.metrics.lock().unwrap().stats.error =
                            Some(format!("H.264 unavailable: {error:#}"));
                        encoder_service.failed.store(true, Ordering::SeqCst);
                        encoder_service.wake_network();
                    }
                });
            if encoder.is_err() {
                service.failed.store(true, Ordering::SeqCst);
            }
            run(udp, &frames, &stop, &service, rx, encoded_rx, wake_rx);
            service.failed.store(true, Ordering::SeqCst);
            frames.wake();
            if let Ok(encoder) = encoder {
                let _ = encoder.join();
            }
        })?)
}

fn bind_sockets(config: &LiveConfig) -> Result<Vec<UdpSocket>> {
    let addresses = if config.bind.is_unspecified() {
        if_addrs::get_if_addrs()?
            .into_iter()
            .map(|i| i.ip())
            .filter(|ip| {
                ip.is_ipv4() == config.bind.is_ipv4()
                    && !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local())
            })
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::from([config.bind])
    };
    ensure!(!addresses.is_empty(), "no local addresses for WebRTC");
    let mut port = config.webrtc_port;
    let mut sockets = Vec::new();
    for ip in addresses.into_iter().take(16) {
        let socket = UdpSocket::bind((ip, port)).with_context(|| {
            format!("cannot bind WebRTC UDP {ip}:{port}; use --webrtc-port to choose another port")
        })?;
        port = socket.local_addr()?.port();
        socket.set_nonblocking(true)?;
        sockets.push(socket);
    }
    Ok(sockets)
}

/// Sends one datagram: `UdpSocket::send_to`, or a test's EAGAIN.
type SendTo = dyn Fn(&UdpSocket, &[u8], SocketAddr) -> std::io::Result<usize> + Send;

/// The host's non-blocking sockets, shared by every peer. Local addresses are
/// cached so routing a packet costs no getsockname call.
struct Udp {
    sockets: Vec<(UdpSocket, SocketAddr)>,
    send_to: Box<SendTo>,
}
impl Udp {
    fn new(sockets: Vec<UdpSocket>) -> Result<Self> {
        let mut bound = Vec::with_capacity(sockets.len());
        for socket in sockets {
            let local = socket.local_addr()?;
            bound.push((socket, local));
        }
        Ok(Self {
            sockets: bound,
            send_to: Box::new(|socket, bytes, to| socket.send_to(bytes, to)),
        })
    }
    /// Send from the packet's ICE source, retrying EINTR. EAGAIN is returned:
    /// callers queue the packet, so the network thread never waits on a socket.
    fn send(&self, packet: &Transmit) -> std::io::Result<usize> {
        let (socket, _) = self
            .sockets
            .iter()
            .find(|(_, local)| *local == packet.source)
            .ok_or_else(|| std::io::Error::other("unknown ICE source"))?;
        loop {
            match (self.send_to)(socket, &packet.contents, packet.destination) {
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

struct Peer {
    id: String,
    connection: Option<String>,
    rtc: Rtc,
    mid: Option<Mid>,
    pt: Option<Pt>,
    connected: bool,
    needs_keyframe: bool,
    started: Instant,
    deadline: Instant,
    dead: bool,
    failure: Option<String>,
    /// Packets a full socket refused, oldest first, with when they queued.
    /// Later packets wait behind them so RTP stays in order.
    outbox: VecDeque<(Transmit, Instant)>,
    /// Bytes in `outbox`.
    queued: usize,
    /// Bytes sent since `run` last recorded them.
    sent: usize,
}
impl Peer {
    fn new(offer: SdpOffer, udp: &Udp, profile_level_id: u32) -> Result<(Self, serde_json::Value)> {
        let mut config = RtcConfig::new()
            .clear_codecs()
            .set_ice_lite(true)
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            // Keep a whole 2 MiB frame (~1900 packets) for NACK resends, so a
            // loss early in a large keyframe can still be repaired.
            .set_send_buffer_video(2048);
        // Every backend must produce constrained baseline. Never negotiate Main/High or
        // packetization mode 0 and then feed it a different bitstream.
        config
            .codec_config()
            .add_h264(108.into(), Some(109.into()), true, profile_level_id);
        let now = Instant::now();
        let mut peer = Self {
            id: super::random_token()?,
            connection: None,
            rtc: config.build(now),
            mid: None,
            pt: None,
            connected: false,
            needs_keyframe: true,
            started: now,
            deadline: now,
            dead: false,
            failure: None,
            outbox: VecDeque::new(),
            queued: 0,
            sent: 0,
        };
        for (_, local) in &udp.sockets {
            peer.rtc
                .add_local_candidate(Candidate::host(*local, "udp")?);
            peer.drain(udp, None)?;
        }
        let answer = peer.rtc.sdp_api().accept_offer(offer)?;
        peer.drain(udp, None)?;
        // MediaAdded is delayed until DTLS has installed SRTP keys. Resolve the
        // single negotiated track from the answer now, before returning SDP.
        let sdp = answer.to_sdp_string();
        let mid: Mid = sdp
            .lines()
            .find_map(|line| line.strip_prefix("a=mid:"))
            .context("missing negotiated video track")?
            .into();
        peer.mid = Some(mid);
        peer.pt = peer.rtc.writer(mid).and_then(|writer| {
            writer
                .payload_params()
                .find(|p| p.spec().codec == Codec::H264)
                .map(|p| p.pt())
        });
        ensure!(peer.pt.is_some(), "browser did not offer compatible H.264");
        let result = serde_json::json!({"id": peer.id, "answer": answer});
        Ok((peer, result))
    }
    fn fail(&mut self, error: impl std::fmt::Display) {
        self.dead = true;
        self.failure = Some(format!("{error:#}").chars().take(600).collect());
    }
    /// Drain after EVERY input/write/mutation, as required by str0m's API.
    /// Never blocks: a packet a full socket refuses waits in the outbox.
    fn drain(&mut self, udp: &Udp, service: Option<&Service>) -> Result<()> {
        let mut refeeds = 0;
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(at) => {
                    // str0m packetizes written frames, and refreshes the pacer
                    // snapshot that releases their packets, only on a timeout.
                    // Feed a due one now, not a loop pass later: the first
                    // sends the frame, the second leaves a future deadline.
                    let now = Instant::now();
                    // The cap of two matches str0m 0.23.1's pacer; recheck on upgrade.
                    if at <= now && refeeds < 2 {
                        refeeds += 1;
                        self.rtc.handle_input(Input::Timeout(now))?;
                        continue;
                    }
                    self.deadline = at;
                    return Ok(());
                }
                Output::Transmit(packet) => {
                    if self.outbox.is_empty() {
                        match udp.send(&packet) {
                            Ok(sent) => {
                                self.sent += sent;
                                continue;
                            }
                            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    self.enqueue(packet, Instant::now())?;
                }
                Output::Event(event) => match event {
                    Event::MediaAdded(media) if media.kind == MediaKind::Video => {
                        self.mid = Some(media.mid);
                        self.pt = self.rtc.writer(media.mid).and_then(|writer| {
                            writer
                                .payload_params()
                                .find(|p| p.spec().codec == Codec::H264)
                                .map(|p| p.pt())
                        });
                    }
                    Event::Connected => {
                        self.connected = true;
                        self.needs_keyframe = true;
                        if let Some(service) = service {
                            service.peer_joined();
                        }
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        self.fail("ICE disconnected")
                    }
                    Event::KeyframeRequest(_) => {
                        if let Some(service) = service {
                            service.request_keyframe();
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    fn enqueue(&mut self, packet: Transmit, now: Instant) -> Result<()> {
        self.queued += packet.contents.len();
        self.outbox.push_back((packet, now));
        self.check_backlog(now)
    }
    /// A viewer whose packets wait 250 ms, or that has more than two maximum
    /// frames queued, is failed rather than trimmed: its browser reconnects or
    /// falls back to JPEG. Only this peer's own backlog counts, but on a
    /// saturated shared link several backlogs can cross the limit together.
    fn check_backlog(&self, now: Instant) -> Result<()> {
        let oldest = self
            .outbox
            .front()
            .map_or(Duration::ZERO, |(_, at)| now.saturating_duration_since(*at));
        ensure!(
            oldest < MAX_QUEUE_AGE && self.queued <= 2 * MAX_FRAME_BYTES,
            "WebRTC viewer cannot keep up ({} KiB queued for {} ms)",
            self.queued / 1024,
            oldest.as_millis()
        );
        Ok(())
    }
    fn send(&mut self, frame: &encoder::Encoded, udp: &Udp, service: &Service) -> Result<()> {
        if !self.connected || (self.needs_keyframe && !frame.keyframe) {
            return Ok(());
        }
        let mid = self.mid.context("missing video track")?;
        // Disconnect a congested peer; the viewer falls back to JPEG. Never
        // discard a queued reference frame and continue with dependent deltas.
        self.check_backlog(Instant::now())?;
        self.rtc
            .writer(mid)
            .context("video writer unavailable")?
            .write(
                self.pt.context("missing H.264 codec")?,
                frame.at,
                MediaTime::from_micros(frame.timestamp),
                frame.bytes.clone(),
            )?;
        self.needs_keyframe = false;
        self.drain(udp, Some(service))
    }
}

/// Send queued packets round-robin, one per peer per round, until every
/// outbox is empty or its socket is full; then fail viewers that cannot keep up.
fn flush(peers: &mut [Peer], udp: &Udp, turn: &mut usize) {
    // Each call starts one peer later, so no viewer always gets the first
    // claim on a congested shared socket.
    let start = *turn % peers.len().max(1);
    *turn = turn.wrapping_add(1);
    let (before, after) = peers.split_at_mut(start);
    let mut full = Vec::new();
    let mut progress = true;
    while progress {
        progress = false;
        let order = after.iter_mut().chain(before.iter_mut());
        for peer in order.filter(|peer| !peer.dead) {
            let Some((packet, _)) = peer.outbox.front() else {
                continue;
            };
            if full.contains(&packet.source) {
                continue;
            }
            match udp.send(packet) {
                Ok(sent) => {
                    peer.sent += sent;
                    peer.queued -= packet.contents.len();
                    peer.outbox.pop_front();
                    progress = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => full.push(packet.source),
                Err(error) => peer.fail(format!("WebRTC output failed: {error}")),
            }
        }
    }
    let now = Instant::now();
    for peer in peers.iter_mut().filter(|peer| !peer.dead) {
        if let Err(error) = peer.check_backlog(now) {
            peer.fail(error);
        }
    }
}

/// Read up to 64 datagrams per socket and give each to the peer accepting it.
fn receive(peers: &mut [Peer], udp: &Udp, service: &Service) {
    let mut buf = [0u8; 2048];
    for (socket, local) in &udp.sockets {
        for _ in 0..64 {
            match socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    let Ok(contents) = (&buf[..n]).try_into() else {
                        continue;
                    };
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: *local,
                            contents,
                        },
                    );
                    if let Some(peer) = peers.iter_mut().find(|peer| peer.rtc.accepts(&input)) {
                        if let Err(error) = peer.rtc.handle_input(input) {
                            peer.fail(format!("WebRTC input failed: {error}"));
                        } else if let Err(error) = peer.drain(udp, Some(service)) {
                            peer.fail(format!("WebRTC output failed: {error:#}"));
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
}

/// Feed due str0m timers and expire peers that never connected.
fn timers(peers: &mut [Peer], udp: &Udp, service: &Service) {
    for peer in peers {
        if peer.deadline <= Instant::now() {
            if let Err(error) = peer.rtc.handle_input(Input::Timeout(Instant::now())) {
                peer.fail(format!("WebRTC timeout handling failed: {error}"));
            } else if let Err(error) = peer.drain(udp, Some(service)) {
                peer.fail(format!("WebRTC timeout output failed: {error:#}"));
            }
        }
        if !peer.connected && peer.started.elapsed() > CONNECT_TIMEOUT {
            peer.fail("WebRTC connection timed out");
        }
    }
}

fn fan_out(peers: &mut [Peer], frame: &encoder::Encoded, udp: &Udp, service: &Service) {
    for peer in peers.iter_mut().filter(|peer| !peer.dead) {
        if let Err(error) = peer.send(frame, udp, service) {
            peer.fail(format!("WebRTC frame send failed: {error:#}"));
        }
    }
}

fn run(
    udp: Udp,
    frames: &FrameState,
    stop: &AtomicBool,
    service: &Service,
    commands: Receiver<Command>,
    encoded: Receiver<encoder::Encoded>,
    wake: std::os::unix::net::UnixDatagram,
) {
    let mut peers: Vec<Peer> = Vec::new();
    let mut turn = 0;
    while !stop.load(Ordering::SeqCst)
        && !service.failed.load(Ordering::SeqCst)
        && frames.inner.lock().unwrap().ended.is_none()
    {
        // Consume wakeups before work. A producer racing with this iteration
        // leaves the fd readable, so poll cannot lose its notification.
        while wake.recv(&mut [0; 64]).is_ok() {}
        for command in commands.try_iter().take(16) {
            match command {
                Command::Close(id) => peers.retain(|peer| peer.id != id),
                Command::Offer(offer, connection, at, reply) => {
                    if at.elapsed() > Duration::from_secs(2) {
                        continue;
                    }
                    if !frames.authorized(connection.as_deref()) {
                        let _ = reply.send(Err(anyhow::anyhow!(super::desktop::IN_USE)));
                        continue;
                    }
                    if peers.len() == MAX_PEERS {
                        let _ = reply.send(Err(anyhow::anyhow!("WebRTC viewer limit reached")));
                        continue;
                    }
                    let (width, height) = {
                        let data = frames.inner.lock().unwrap();
                        (data.width, data.height)
                    };
                    let profile_level_id = h264_profile_level_id(
                        width,
                        height,
                        service.fps,
                        service.metrics.lock().unwrap().stats.target_bitrate,
                    );
                    match Peer::new(offer, &udp, profile_level_id) {
                        Ok((mut peer, answer)) => {
                            peer.connection = connection;
                            if reply.send(Ok(answer)).is_ok() {
                                if frames.desktop.is_some() {
                                    peers.clear();
                                }
                                peers.push(peer);
                            }
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
            }
        }
        peers.retain(|peer| frames.authorized(peer.connection.as_deref()));
        flush(&mut peers, &udp, &mut turn);
        receive(&mut peers, &udp, service);
        timers(&mut peers, &udp, service);
        if let Ok(frame) = encoded.try_recv() {
            service.queued.store(false, Ordering::SeqCst);
            frames.wake();
            service
                .metrics
                .lock()
                .unwrap()
                .send_queue
                .record(Instant::now(), frame.ready_at.elapsed());
            fan_out(&mut peers, &frame, &udp, service);
        }
        let peer_error = peers
            .iter()
            .filter(|peer| peer.dead)
            .filter_map(|peer| peer.failure.clone())
            .next_back();
        let failed = peers.iter().filter(|peer| peer.dead).count();
        let sent: usize = peers
            .iter_mut()
            .map(|peer| std::mem::take(&mut peer.sent))
            .sum();
        peers.retain(|peer| !peer.dead);
        let count = peers.iter().filter(|peer| peer.connected).count();
        if service.connected.swap(count, Ordering::SeqCst) != count {
            frames.wake();
        }
        {
            let mut metrics = service.metrics.lock().unwrap();
            metrics.stats.peers = peers.len();
            metrics.stats.failed_peers += failed as u64;
            if let Some(error) = peer_error {
                metrics.stats.peer_error = Some(error);
            }
            if sent > 0 {
                metrics.stats.bytes_sent += sent as u64;
                metrics.output.record(Instant::now(), sent as u64, 0);
            }
        }
        // Wake on UDP input, a new encoded frame, a signaling command, the next
        // protocol timer, a queued packet's age limit, or a full socket draining.
        // Cap idle waits to keep shutdown/lease checks prompt.
        let now = Instant::now();
        let deadline = peers
            .iter()
            .map(|peer| match peer.outbox.front() {
                Some((_, at)) => peer.deadline.min(*at + MAX_QUEUE_AGE),
                None => peer.deadline,
            })
            .min()
            .unwrap_or(now + Duration::from_millis(100));
        let timeout = Timespec::try_from(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(100)),
        )
        .unwrap();
        let mut fds: Vec<_> = udp
            .sockets
            .iter()
            .map(|(socket, local)| {
                let backlog = peers.iter().any(|peer| {
                    let front = peer.outbox.front();
                    front.is_some_and(|(packet, _)| packet.source == *local)
                });
                let flags = if backlog {
                    PollFlags::IN | PollFlags::OUT
                } else {
                    PollFlags::IN
                };
                PollFd::new(socket, flags)
            })
            .collect();
        fds.push(PollFd::new(&wake, PollFlags::IN));
        if let Err(error) = poll(&mut fds, Some(&timeout)) {
            if error != rustix::io::Errno::INTR {
                service.metrics.lock().unwrap().stats.error =
                    Some(format!("WebRTC poll failed: {error}"));
                break;
            }
        }
    }
    service.connected.store(0, Ordering::SeqCst);
    service.metrics.lock().unwrap().stats.peers = 0;
    frames.wake();
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::{
        change::{SdpAnswer, SdpPendingOffer},
        media::Direction,
    };

    fn loopback() -> Udp {
        Udp::new(
            bind_sockets(&LiveConfig {
                bind: "127.0.0.1".parse().unwrap(),
                webrtc_port: 0,
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap()
    }

    /// A service for metrics and keyframe requests; nothing reads its channels.
    fn service() -> Arc<Service> {
        let (commands, _) = mpsc::sync_channel(1);
        let (wake, _) = std::os::unix::net::UnixDatagram::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        Arc::new(Service {
            commands,
            connected: AtomicUsize::new(0),
            joins: AtomicU64::new(0),
            keyframe: AtomicBool::new(true),
            failed: AtomicBool::new(false),
            metrics: Mutex::new(Metrics {
                stats: WebRtcStats::default(),
                encode: Timings::default(),
                capture_to_encode: Timings::default(),
                convert: Timings::default(),
                codec: Timings::default(),
                send_queue: Timings::default(),
                output: Rate::new(Instant::now()),
                encoded: Rate::new(Instant::now()),
            }),
            fps: 60,
            frames: std::sync::Weak::new(),
            queued: AtomicBool::new(false),
            wake,
        })
    }

    /// An Annex B access unit of about `size` bytes: SPS+PPS+IDR or a P slice.
    fn frame(keyframe: bool, size: usize, index: u64) -> encoder::Encoded {
        let mut bytes = Vec::with_capacity(size + 32);
        if keyframe {
            bytes.extend([0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0xab]);
            bytes.extend([0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        }
        bytes.extend([0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }]);
        // No zero bytes, so the payload never contains a start code.
        bytes.extend((0..size).map(|i| (i % 251) as u8 | 1));
        let now = Instant::now();
        encoder::Encoded {
            bytes: bytes.into(),
            keyframe,
            at: now,
            timestamp: index * 16_667,
            ready_at: now,
        }
    }

    /// Payload type and sequence number of an (S)RTP datagram; None for
    /// STUN, DTLS and RTCP (RFC 5761 demultiplexing).
    fn rtp(datagram: &[u8]) -> Option<(u8, u16)> {
        (datagram.len() >= 12 && datagram[0] >> 6 == 2 && !(192..=223).contains(&datagram[1])).then(
            || {
                (
                    datagram[1] & 0x7f,
                    u16::from_be_bytes([datagram[2], datagram[3]]),
                )
            },
        )
    }

    /// A browser stand-in: a full-ICE str0m peer on its own loopback socket.
    struct Client {
        rtc: Rtc,
        socket: UdpSocket,
        addr: SocketAddr,
        connected: bool,
    }

    impl Client {
        fn offer() -> (Self, SdpOffer, SdpPendingOffer) {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_nonblocking(true).unwrap();
            let addr = socket.local_addr().unwrap();
            let mut rtc = RtcConfig::new()
                .clear_codecs()
                .enable_h264(true)
                .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
                .build(Instant::now());
            rtc.add_local_candidate(Candidate::host(addr, "udp").unwrap());
            let mut changes = rtc.sdp_api();
            changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
            let (offer, pending) = changes.apply().unwrap();
            let client = Self {
                rtc,
                socket,
                addr,
                connected: false,
            };
            (client, offer, pending)
        }

        fn accept(&mut self, answer: &serde_json::Value, pending: SdpPendingOffer) {
            let answer: SdpAnswer = serde_json::from_value(answer["answer"].clone()).unwrap();
            self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
        }

        /// Transmit pending output and feed due timers.
        fn output(&mut self) {
            let mut timeouts = 0;
            loop {
                match self.rtc.poll_output().unwrap() {
                    Output::Transmit(packet) => {
                        let _ = self.socket.send_to(&packet.contents, packet.destination);
                    }
                    Output::Timeout(at) if at <= Instant::now() && timeouts < 3 => {
                        timeouts += 1;
                        self.rtc
                            .handle_input(Input::Timeout(Instant::now()))
                            .unwrap();
                    }
                    Output::Timeout(_) => return,
                    Output::Event(Event::Connected) => self.connected = true,
                    Output::Event(_) => {}
                }
            }
        }

        fn feed(&mut self, datagram: &[u8], source: SocketAddr) {
            if let Ok(contents) = datagram.try_into() {
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.addr,
                        contents,
                    },
                );
                self.rtc.handle_input(input).unwrap();
            }
            self.output();
        }

        /// Feed every waiting datagram to the client and return them raw.
        fn input(&mut self) -> Vec<Vec<u8>> {
            let mut received = Vec::new();
            let mut buf = [0; 2048];
            while let Ok((n, source)) = self.socket.recv_from(&mut buf) {
                received.push(buf[..n].to_vec());
                self.feed(&buf[..n], source);
            }
            self.output();
            received
        }

        /// Read without feeding until nothing arrives for 20 ms.
        fn read_raw(&mut self) -> Vec<(Vec<u8>, SocketAddr)> {
            let (mut received, mut idle) = (Vec::new(), Instant::now());
            let mut buf = [0; 2048];
            while idle.elapsed() < Duration::from_millis(20) {
                match self.socket.recv_from(&mut buf) {
                    Ok((n, source)) => {
                        received.push((buf[..n].to_vec(), source));
                        idle = Instant::now();
                    }
                    Err(_) => thread::sleep(Duration::from_millis(1)),
                }
            }
            received
        }

        /// Read, without any host pass, until `done` or `timeout`.
        fn read_until(
            &mut self,
            timeout: Duration,
            done: impl Fn(&[Vec<u8>]) -> bool,
        ) -> Vec<Vec<u8>> {
            let deadline = Instant::now() + timeout;
            let mut received = Vec::new();
            while !done(&received) && Instant::now() < deadline {
                received.extend(self.input());
                thread::sleep(Duration::from_millis(1));
            }
            received
        }
    }

    fn count_rtp(datagrams: &[Vec<u8>]) -> usize {
        datagrams.iter().filter(|d| rtp(d).is_some()).count()
    }

    /// RTP packets ending a frame (marker bit set).
    fn count_frames(datagrams: &[Vec<u8>]) -> usize {
        datagrams
            .iter()
            .filter(|d| rtp(d).is_some() && d[1] & 0x80 != 0)
            .count()
    }

    /// `run`'s network steps and loopback viewers, with passes driven by the test.
    struct Fixture {
        udp: Udp,
        service: Arc<Service>,
        peers: Vec<Peer>,
        clients: Vec<Client>,
        turn: usize,
    }

    impl Fixture {
        fn new(udp: Udp) -> Self {
            Self {
                udp,
                service: service(),
                peers: Vec::new(),
                clients: Vec::new(),
                turn: 0,
            }
        }

        /// The host part of one `run` pass, without a frame.
        fn host(&mut self) {
            flush(&mut self.peers, &self.udp, &mut self.turn);
            receive(&mut self.peers, &self.udp, &self.service);
            timers(&mut self.peers, &self.udp, &self.service);
        }

        /// One host pass, then each viewer's input.
        fn pass(&mut self) -> Vec<Vec<Vec<u8>>> {
            self.host();
            self.clients.iter_mut().map(Client::input).collect()
        }

        /// Add a viewer and pump until both ends report a connection.
        fn join(&mut self) {
            let (mut client, offer, pending) = Client::offer();
            let (peer, answer) = Peer::new(offer, &self.udp, 0x42e01f).unwrap();
            client.accept(&answer, pending);
            self.peers.push(peer);
            self.clients.push(client);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !(self.peers.last().unwrap().connected && self.clients.last().unwrap().connected)
            {
                assert!(Instant::now() < deadline, "loopback WebRTC did not connect");
                let peer = self.peers.last().unwrap();
                assert!(!peer.dead, "{:?}", peer.failure);
                self.pass();
                thread::sleep(Duration::from_millis(1));
            }
        }

        /// Pump until the link is quiet, so later datagrams are the test's own.
        fn settle(&mut self) {
            let (deadline, mut quiet) = (Instant::now() + Duration::from_secs(2), 0);
            while quiet < 20 && Instant::now() < deadline {
                let received: usize = self.pass().iter().map(Vec::len).sum();
                quiet = if received == 0 { quiet + 1 } else { 0 };
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// A socket send that reports EAGAIN while `full` returns true.
    fn refusing(full: impl Fn(SocketAddr) -> bool + Send + 'static) -> Udp {
        let mut udp = loopback();
        udp.send_to = Box::new(move |socket, bytes, to| {
            if full(to) {
                Err(ErrorKind::WouldBlock.into())
            } else {
                socket.send_to(bytes, to)
            }
        });
        udp
    }

    #[test]
    fn a_frame_leaves_during_send_and_the_peer_timer_moves_to_the_future() {
        let mut net = Fixture::new(loopback());
        net.join();
        net.settle();
        let peer = &mut net.peers[0];
        peer.send(&frame(true, 20_000, 0), &net.udp, &net.service)
            .unwrap();
        let due_in = peer.deadline.checked_duration_since(Instant::now());
        let pt = *peer.pt.unwrap();
        // No host pass runs from here: any RTP the viewer reads left inside send().
        let received = net.clients[0].read_until(Duration::from_secs(1), |r| count_rtp(r) > 0);
        assert!(
            received
                .iter()
                .any(|d| rtp(d).is_some_and(|(p, _)| p == pt)),
            "no H.264 RTP left during Peer::send; the frame waited for another loop pass"
        );
        assert!(
            due_in.is_some_and(|d| !d.is_zero()),
            "the peer timer is still due, forcing an immediate extra loop pass"
        );
    }

    #[test]
    fn each_connecting_viewer_is_one_join_and_not_a_keyframe_request() {
        let mut net = Fixture::new(loopback());
        net.service.keyframe.store(false, Ordering::SeqCst);
        for joins in 1..=2 {
            net.join();
            net.settle();
            assert_eq!(net.service.joins.load(Ordering::SeqCst), joins);
        }
        assert!(
            !net.service.keyframe.load(Ordering::SeqCst),
            "a join raised the throttled PLI/FIR flag"
        );
    }

    #[test]
    fn a_full_socket_queues_packets_without_blocking_and_keeps_their_order() {
        let full = Arc::new(AtomicBool::new(false));
        let refuse = full.clone();
        let mut net = Fixture::new(refusing(move |_| refuse.load(Ordering::SeqCst)));
        net.join();
        net.settle();
        full.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let result = net.peers[0].send(&frame(true, 6_000, 0), &net.udp, &net.service);
        let elapsed = started.elapsed();
        result.unwrap();
        assert!(
            elapsed < Duration::from_millis(5),
            "send blocked for {elapsed:?}"
        );
        let queued: Vec<Vec<u8>> = net.peers[0]
            .outbox
            .iter()
            .map(|(packet, _)| packet.contents.to_vec())
            .collect();
        assert!(
            count_rtp(&queued) >= 5,
            "the frame was not queued: {} datagrams",
            queued.len()
        );
        assert_eq!(
            net.peers[0].queued,
            queued.iter().map(Vec::len).sum::<usize>()
        );
        thread::sleep(Duration::from_millis(5));
        assert!(
            net.clients[0].input().is_empty(),
            "sent through a full socket"
        );
        // The socket drains: one flush sends the backlog, oldest first.
        full.store(false, Ordering::SeqCst);
        flush(&mut net.peers, &net.udp, &mut net.turn);
        assert!(net.peers[0].outbox.is_empty() && net.peers[0].queued == 0);
        assert!(!net.peers[0].dead, "{:?}", net.peers[0].failure);
        let received =
            net.clients[0].read_until(Duration::from_secs(1), |r| r.len() >= queued.len());
        assert_eq!(
            received, queued,
            "queued packets were lost, changed or reordered"
        );
        let sequence: Vec<u16> = received
            .iter()
            .filter_map(|d| rtp(d))
            .map(|r| r.1)
            .collect();
        assert!(
            sequence.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
            "{sequence:?}"
        );
    }

    #[test]
    fn a_backed_up_viewer_fails_alone_while_another_keeps_receiving() {
        let slow = Arc::new(Mutex::new(None));
        let refuse = slow.clone();
        let mut net = Fixture::new(refusing(move |to| *refuse.lock().unwrap() == Some(to)));
        net.join();
        net.join();
        net.settle();
        *slow.lock().unwrap() = Some(net.clients[0].addr);
        let (started, mut last, mut sent) = (Instant::now(), None::<Instant>, 0);
        let mut fast = Vec::new();
        while !net.peers[0].dead {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the backed-up viewer was never failed"
            );
            let pass = Instant::now();
            if last.is_none_or(|at| at.elapsed() >= Duration::from_millis(20)) {
                let frame = frame(sent == 0, 4_000, sent as u64);
                fan_out(&mut net.peers, &frame, &net.udp, &net.service);
                (last, sent) = (Some(Instant::now()), sent + 1);
            }
            net.host();
            let host = pass.elapsed();
            assert!(
                host < Duration::from_millis(50),
                "a network pass blocked for {host:?}"
            );
            fast.extend(net.clients[1].input());
            net.clients[0].input();
            thread::sleep(Duration::from_millis(2));
        }
        let failure = net.peers[0].failure.clone().unwrap();
        assert!(failure.contains("cannot keep up"), "{failure}");
        assert!(
            started.elapsed() >= MAX_QUEUE_AGE,
            "failed early: {failure}"
        );
        assert!(!net.peers[1].dead, "{:?}", net.peers[1].failure);
        fast.extend(net.clients[1].read_until(Duration::from_secs(1), |r| {
            count_frames(&fast) + count_frames(r) >= sent
        }));
        assert_eq!(
            count_frames(&fast),
            sent,
            "the healthy viewer missed frames"
        );
        // `run` drops the failed peer; the other keeps receiving new frames.
        net.peers.retain(|peer| !peer.dead);
        net.clients.remove(0);
        fan_out(
            &mut net.peers,
            &frame(false, 4_000, sent as u64),
            &net.udp,
            &net.service,
        );
        let received = net.clients[0].read_until(Duration::from_secs(1), |r| count_frames(r) > 0);
        assert_eq!(count_frames(&received), 1);
    }

    #[test]
    fn a_full_shared_socket_fails_only_the_viewer_whose_own_backlog_crosses_a_limit() {
        let full = Arc::new(AtomicBool::new(false));
        let refuse = full.clone();
        let mut net = Fixture::new(refusing(move |_| refuse.load(Ordering::SeqCst)));
        net.join();
        net.join();
        net.settle();
        assert_eq!(net.udp.sockets.len(), 1, "the viewers must share a socket");
        let outbox = |peer: &Peer| -> Vec<Vec<u8>> {
            peer.outbox
                .iter()
                .map(|(packet, _)| packet.contents.to_vec())
                .collect()
        };
        // Sent oldest first: RTP sequence numbers run on without a gap.
        let in_order = |datagrams: &[Vec<u8>]| {
            let sequence: Vec<u16> = datagrams
                .iter()
                .filter_map(|d| rtp(d))
                .map(|r| r.1)
                .collect();
            sequence.windows(2).all(|w| w[1] == w[0].wrapping_add(1))
        };
        // The socket refuses every destination. Both viewers queue, and neither
        // fails while its own backlog is under 250 ms and 4 MiB.
        full.store(true, Ordering::SeqCst);
        fan_out(
            &mut net.peers,
            &frame(true, 6_000, 0),
            &net.udp,
            &net.service,
        );
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(50) {
            net.host();
            for peer in &net.peers {
                assert!(!peer.dead, "{:?}", peer.failure);
            }
            thread::sleep(Duration::from_millis(2));
        }
        let queued: Vec<_> = net.peers.iter().map(outbox).collect();
        assert!(
            queued.iter().all(|q| count_rtp(q) >= 5),
            "a frame was not queued"
        );
        // Writable again: each backlog leaves whole and in order.
        full.store(false, Ordering::SeqCst);
        flush(&mut net.peers, &net.udp, &mut net.turn);
        for peer in &net.peers {
            assert!(!peer.dead && peer.outbox.is_empty() && peer.queued == 0);
        }
        for (client, queued) in net.clients.iter_mut().zip(&queued) {
            let received = client.read_until(Duration::from_secs(1), |r| r.len() >= queued.len());
            assert_eq!(
                &received, queued,
                "queued packets were lost, changed or reordered"
            );
            assert!(in_order(&received), "a backlog left out of order");
        }
        // Full again. Viewer 1's backlog starts 200 ms after viewer 0's, so only
        // viewer 0's reaches 250 ms. Only flush runs: a receive or timer pass
        // could queue a STUN or RTCP reply that starts viewer 1's backlog early.
        full.store(true, Ordering::SeqCst);
        let started = Instant::now();
        fan_out(
            &mut net.peers[..1],
            &frame(false, 6_000, 1),
            &net.udp,
            &net.service,
        );
        thread::sleep(Duration::from_millis(200));
        fan_out(
            &mut net.peers[1..],
            &frame(false, 6_000, 1),
            &net.udp,
            &net.service,
        );
        while !net.peers[0].dead {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "viewer 0 never failed"
            );
            flush(&mut net.peers, &net.udp, &mut net.turn);
            assert!(!net.peers[1].dead, "{:?}", net.peers[1].failure);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(started.elapsed() >= MAX_QUEUE_AGE, "viewer 0 failed early");
        let failure = net.peers[0].failure.clone().unwrap();
        assert!(failure.contains("cannot keep up"), "{failure}");
        let queued = outbox(&net.peers[1]);
        assert!(count_rtp(&queued) >= 5, "viewer 1's frame was not queued");
        full.store(false, Ordering::SeqCst);
        flush(&mut net.peers, &net.udp, &mut net.turn);
        assert!(!net.peers[1].dead && net.peers[1].outbox.is_empty());
        let received =
            net.clients[1].read_until(Duration::from_secs(1), |r| r.len() >= queued.len());
        assert_eq!(
            received, queued,
            "viewer 1's packets were lost or reordered"
        );
        assert!(in_order(&received), "viewer 1's backlog left out of order");
    }

    #[test]
    fn each_flush_offers_a_shared_socket_to_the_next_peer_first() {
        let mut udp = loopback();
        let mut peers: Vec<Peer> = (0..3)
            .map(|_| Peer::new(Client::offer().1, &udp, 0x42e01f).unwrap().0)
            .collect();
        // Peer i's packets go to port i + 1, so each send names its peer.
        let (source, now) = (udp.sockets[0].1, Instant::now());
        for (index, peer) in peers.iter_mut().enumerate() {
            for _ in 0..3 {
                let packet = Transmit {
                    proto: Protocol::Udp,
                    source,
                    destination: SocketAddr::from(([127, 0, 0, 1], index as u16 + 1)),
                    contents: vec![0x80; 100].into(),
                };
                peer.enqueue(packet, now).unwrap();
            }
        }
        // The shared socket has room for one datagram per flush.
        let (room, sent) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(Vec::new())),
        );
        let (space, log) = (room.clone(), sent.clone());
        udp.send_to = Box::new(move |_, bytes, to| {
            space
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .map_err(|_| std::io::Error::from(ErrorKind::WouldBlock))?;
            log.lock().unwrap().push(to.port());
            Ok(bytes.len())
        });
        let mut turn = 0;
        for _ in 0..6 {
            room.store(1, Ordering::SeqCst);
            flush(&mut peers, &udp, &mut turn);
        }
        assert_eq!(
            *sent.lock().unwrap(),
            [1, 2, 3, 1, 2, 3],
            "one peer kept first claim on the shared socket"
        );
        assert!(
            peers
                .iter()
                .all(|peer| !peer.dead && peer.outbox.len() == 1)
        );
    }

    #[test]
    fn a_nack_for_an_early_packet_of_a_large_keyframe_is_answered() {
        let mut net = Fixture::new(loopback());
        net.join();
        net.settle();
        let peer = &mut net.peers[0];
        let pt = peer.pt.unwrap();
        let writer = peer.rtc.writer(peer.mid.unwrap()).unwrap();
        let params = writer.payload_params().find(|p| p.pt() == pt);
        let rtx = *params.and_then(|p| p.resend()).unwrap();
        // About 900 packets, all sent inside send(): a 512-packet resend
        // buffer has evicted the second one before the viewer's NACK is read.
        peer.send(&frame(true, 1_000_000, 0), &net.udp, &net.service)
            .unwrap();
        assert!(peer.sent > 1_000_000 && peer.outbox.is_empty());
        // Lose that second packet. The viewer takes the first 60, so the gap
        // stays inside its NACK window, and asks for it.
        let client = &mut net.clients[0];
        let mut media = 0;
        for (datagram, source) in client.read_raw() {
            if rtp(&datagram).is_some_and(|(p, _)| p == *pt) {
                media += 1;
                if media == 2 || media > 60 {
                    continue;
                }
            }
            client.feed(&datagram, source);
        }
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            client.output();
            thread::sleep(Duration::from_millis(5));
        }
        receive(&mut net.peers, &net.udp, &net.service);
        let resent = |r: &[Vec<u8>]| r.iter().any(|d| rtp(d).is_some_and(|(p, _)| p == rtx));
        let received = net.clients[0].read_until(Duration::from_secs(1), resent);
        assert!(resent(&received), "the lost keyframe packet was not resent");
    }

    #[test]
    fn a_viewer_fails_with_two_frames_or_a_quarter_second_queued() {
        let udp = loopback();
        let peer = || {
            let (_client, offer, _) = Client::offer();
            Peer::new(offer, &udp, 0x42e01f).unwrap().0
        };
        let packet = |len: usize| Transmit {
            proto: Protocol::Udp,
            source: udp.sockets[0].1,
            destination: udp.sockets[0].1,
            contents: vec![0x80; len].into(),
        };
        let now = Instant::now();
        // Size: two maximum frames may wait; one more byte fails the viewer.
        let mut large = peer();
        for _ in 0..2 * MAX_FRAME_BYTES / 1024 {
            large.enqueue(packet(1024), now).unwrap();
        }
        assert!(large.check_backlog(now).is_ok());
        let error = large.enqueue(packet(1), now).unwrap_err();
        assert!(format!("{error:#}").contains("cannot keep up"), "{error:#}");
        // Age: the oldest packet may wait just under 250 ms.
        let mut old = peer();
        old.enqueue(packet(1200), now).unwrap();
        assert!(
            old.check_backlog(now + MAX_QUEUE_AGE - Duration::from_millis(1))
                .is_ok()
        );
        let error = old.check_backlog(now + MAX_QUEUE_AGE).unwrap_err();
        assert!(format!("{error:#}").contains("cannot keep up"), "{error:#}");
    }

    #[test]
    fn h264_level_covers_the_encoded_dimensions_and_rate() {
        assert_eq!(h264_profile_level_id(640, 360, 15, 4_000_000), 0x42e01f);
        assert_eq!(h264_profile_level_id(1_896, 1_030, 15, 4_000_000), 0x42e028);
        assert_eq!(h264_profile_level_id(1_920, 1_080, 60, 4_000_000), 0x42e02a);
        assert_eq!(h264_profile_level_id(3_840, 2_160, 60, 4_000_000), 0x42e034);
    }

    #[test]
    fn negotiates_h264_before_dtls_media_events_and_rejects_other_codecs() {
        let udp = loopback();
        for h264 in [true, false] {
            let mut rtc = RtcConfig::new()
                .clear_codecs()
                .enable_h264(h264)
                .enable_vp8(!h264)
                .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
                .build(Instant::now());
            let mut changes = rtc.sdp_api();
            changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
            let (offer, _) = changes.apply().unwrap();
            let result = Peer::new(offer, &udp, 0x42e028);
            if h264 {
                let (peer, answer) = result.unwrap();
                assert!(peer.mid.is_some() && peer.pt.is_some());
                assert!(!peer.connected);
                let sdp = answer["answer"]["sdp"].as_str().unwrap();
                assert!(sdp.contains("H264/90000"));
                assert!(sdp.contains("profile-level-id=42e028"));
            } else {
                assert!(result.is_err());
            }
        }
    }
}
