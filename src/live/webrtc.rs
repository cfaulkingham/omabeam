//! LAN-only, receive-only H.264. One encoder feeds at most eight peers through
//! a one-frame channel. ICE/DTLS/RTP run independently of capture and encoding.
mod encoder;
pub use encoder::probe as probe_encoder;

use super::{
    LiveConfig,
    diagnostics::{Rate, TimingStats, Timings},
    state::FrameState,
};
use anyhow::{Context, Result, bail, ensure};
use omabeam_capture::CapturedFrame;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::ErrorKind,
    net::{IpAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    net::{Protocol, Receive},
};

const MAX_PEERS: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_QUEUE_AGE: Duration = Duration::from_millis(250);

pub(super) struct RawFrame {
    pub frame: CapturedFrame,
    pub config: LiveConfig,
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
    pub bytes_sent: u64,
    pub outgoing_mbps: f64,
    pub keyframes: u64,
    pub dropped_frames: u64,
    pub failed_peers: u64,
    pub error: Option<String>,
}
struct Metrics {
    stats: WebRtcStats,
    encode: Timings,
    output: Rate,
    encoded: Rate,
}

pub(super) struct Service {
    commands: SyncSender<Command>,
    connected: AtomicUsize,
    keyframe: AtomicBool,
    metrics: Mutex<Metrics>,
    failed: AtomicBool,
}
enum Command {
    Offer(SdpOffer, Instant, SyncSender<Result<serde_json::Value>>),
    Close(String),
}
impl Service {
    pub fn connected(&self) -> usize {
        self.connected.load(Ordering::SeqCst)
    }
    pub fn stats(&self) -> WebRtcStats {
        let metrics = self.metrics.lock().unwrap();
        let mut stats = metrics.stats.clone();
        stats.connected = self.connected();
        stats.encode_ms = metrics.encode.stats(Instant::now());
        stats.outgoing_mbps = metrics.output.values(Instant::now()).0;
        stats.encoded_fps = metrics.encoded.values(Instant::now()).1;
        stats
    }
    pub fn offer(&self, body: &[u8]) -> Result<serde_json::Value> {
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
            .try_send(Command::Offer(offer, Instant::now(), tx))
            .map_err(|_| anyhow::anyhow!("WebRTC busy"))?;
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
            .map_err(|_| anyhow::anyhow!("WebRTC busy"))
    }
}

pub(super) fn start(
    config: &LiveConfig,
    frames: &Arc<FrameState>,
    stop: &Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let sockets = bind_sockets(config)?;
    let port = sockets[0].local_addr()?.port();
    let (tx, rx) = mpsc::sync_channel(16);
    let service = Arc::new(Service {
        commands: tx,
        connected: AtomicUsize::new(0),
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
            output: Rate::new(Instant::now()),
            encoded: Rate::new(Instant::now()),
        }),
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
                    }
                });
            if encoder.is_err() {
                service.failed.store(true, Ordering::SeqCst);
            }
            run(sockets, &frames, &stop, &service, rx, encoded_rx);
            service.failed.store(true, Ordering::SeqCst);
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

struct Peer {
    id: String,
    rtc: Rtc,
    mid: Option<Mid>,
    pt: Option<Pt>,
    connected: bool,
    needs_keyframe: bool,
    started: Instant,
    deadline: Instant,
    dead: bool,
}
impl Peer {
    fn new(offer: SdpOffer, sockets: &[UdpSocket]) -> Result<(Self, serde_json::Value)> {
        let mut config = RtcConfig::new()
            .clear_codecs()
            .set_ice_lite(true)
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .set_send_buffer_video(512);
        // Every backend must produce constrained baseline. Never negotiate Main/High or
        // packetization mode 0 and then feed it a different bitstream.
        config
            .codec_config()
            .add_h264(108.into(), Some(109.into()), true, 0x42e01f);
        let now = Instant::now();
        let mut peer = Self {
            id: super::random_token()?,
            rtc: config.build(now),
            mid: None,
            pt: None,
            connected: false,
            needs_keyframe: true,
            started: now,
            deadline: now,
            dead: false,
        };
        for socket in sockets {
            peer.rtc
                .add_local_candidate(Candidate::host(socket.local_addr()?, "udp")?);
            peer.drain(sockets, None)?;
        }
        let answer = peer.rtc.sdp_api().accept_offer(offer)?;
        peer.drain(sockets, None)?;
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
    /// Drain after EVERY input/write/mutation, as required by str0m's API.
    fn drain(&mut self, sockets: &[UdpSocket], service: Option<&Service>) -> Result<()> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(at) => {
                    self.deadline = at;
                    return Ok(());
                }
                Output::Transmit(packet) => {
                    let socket = sockets
                        .iter()
                        .find(|s| s.local_addr().ok() == Some(packet.source))
                        .context("unknown ICE source")?;
                    match socket.send_to(&packet.contents, packet.destination) {
                        Ok(n) => {
                            if let Some(service) = service {
                                let mut metrics = service.metrics.lock().unwrap();
                                metrics.stats.bytes_sent += n as u64;
                                metrics.output.record(Instant::now(), n as u64, 0);
                            }
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            bail!("UDP send queue full")
                        }
                        Err(error) => return Err(error.into()),
                    }
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
                            service.keyframe.store(true, Ordering::SeqCst);
                        }
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        self.dead = true
                    }
                    Event::KeyframeRequest(_) => {
                        if let Some(service) = service {
                            service.keyframe.store(true, Ordering::SeqCst);
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    fn send(
        &mut self,
        frame: &encoder::Encoded,
        sockets: &[UdpSocket],
        service: &Service,
    ) -> Result<()> {
        if !self.connected || (self.needs_keyframe && !frame.keyframe) {
            return Ok(());
        }
        let mid = self.mid.context("missing video track")?;
        // Disconnect a congested peer; the viewer falls back to JPEG. Never
        // discard a queued reference frame and continue with dependent deltas.
        if let Some(stream) = self.rtc.direct_api().stream_tx_by_mid(mid, None) {
            if let Some(queue) = stream.queue_info() {
                ensure!(
                    queue.byte_size() < MAX_FRAME_BYTES
                        && queue
                            .first_unsent()
                            .is_none_or(|at| at.elapsed() < MAX_QUEUE_AGE),
                    "WebRTC viewer cannot keep up"
                );
            }
        }
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
        self.drain(sockets, Some(service))
    }
}

fn run(
    sockets: Vec<UdpSocket>,
    frames: &FrameState,
    stop: &AtomicBool,
    service: &Service,
    commands: Receiver<Command>,
    encoded: Receiver<encoder::Encoded>,
) {
    let mut peers: Vec<Peer> = Vec::new();
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::SeqCst)
        && !service.failed.load(Ordering::SeqCst)
        && frames.inner.lock().unwrap().ended.is_none()
    {
        for command in commands.try_iter().take(16) {
            match command {
                Command::Close(id) => peers.retain(|peer| peer.id != id),
                Command::Offer(offer, at, reply) => {
                    if at.elapsed() > Duration::from_secs(2) {
                        continue;
                    }
                    if peers.len() == MAX_PEERS {
                        let _ = reply.send(Err(anyhow::anyhow!("WebRTC viewer limit reached")));
                        continue;
                    }
                    match Peer::new(offer, &sockets) {
                        Ok((peer, answer)) => {
                            if reply.send(Ok(answer)).is_ok() {
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
        for socket in &sockets {
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
                                destination: socket.local_addr().unwrap(),
                                contents,
                            },
                        );
                        if let Some(peer) = peers.iter_mut().find(|peer| peer.rtc.accepts(&input)) {
                            if peer.rtc.handle_input(input).is_err()
                                || peer.drain(&sockets, Some(service)).is_err()
                            {
                                peer.dead = true;
                            }
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        for peer in &mut peers {
            if peer.deadline <= Instant::now()
                && (peer
                    .rtc
                    .handle_input(Input::Timeout(Instant::now()))
                    .is_err()
                    || peer.drain(&sockets, Some(service)).is_err())
            {
                peer.dead = true;
            }
            if !peer.connected && peer.started.elapsed() > CONNECT_TIMEOUT {
                peer.dead = true;
            }
        }
        if let Ok(frame) = encoded.try_recv() {
            for peer in peers.iter_mut().filter(|peer| !peer.dead) {
                if peer.send(&frame, &sockets, service).is_err() {
                    peer.dead = true;
                }
            }
        }
        let failed = peers.iter().filter(|peer| peer.dead).count();
        peers.retain(|peer| !peer.dead);
        let count = peers.iter().filter(|peer| peer.connected).count();
        if service.connected.swap(count, Ordering::SeqCst) != count {
            frames.tick.notify_all();
        }
        {
            let mut metrics = service.metrics.lock().unwrap();
            metrics.stats.peers = peers.len();
            metrics.stats.failed_peers += failed as u64;
        }
        thread::sleep(Duration::from_millis(if peers.is_empty() { 20 } else { 5 }));
    }
    service.connected.store(0, Ordering::SeqCst);
    service.metrics.lock().unwrap().stats.peers = 0;
    frames.tick.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::media::Direction;

    #[test]
    fn negotiates_h264_before_dtls_media_events_and_rejects_other_codecs() {
        let sockets = bind_sockets(&LiveConfig {
            bind: "127.0.0.1".parse().unwrap(),
            webrtc_port: 0,
            ..Default::default()
        })
        .unwrap();
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
            let result = Peer::new(offer, &sockets);
            if h264 {
                let (peer, answer) = result.unwrap();
                assert!(peer.mid.is_some() && peer.pt.is_some());
                assert!(!peer.connected);
                assert!(
                    answer["answer"]["sdp"]
                        .as_str()
                        .unwrap()
                        .contains("H264/90000")
                );
            } else {
                assert!(result.is_err());
            }
        }
    }
}
