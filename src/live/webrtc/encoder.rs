use super::*;
use crate::live::h264::{AdaptiveEncoder, YuvConverter};
pub(super) struct Encoded {
    pub bytes: Arc<[u8]>,
    pub keyframe: bool,
    pub at: Instant,
    pub timestamp: u64,
    pub ready_at: Instant,
}

/// Minimum gap between viewer-requested (PLI/FIR) IDRs. One lossy peer must
/// not cost every other viewer a full frame back-to-back; a request inside
/// the window is kept pending rather than dropped, and fires as soon as the
/// window ends. A first frame, a new peer, a dropped reference frame, and a
/// hardware/software switch all bypass this and force immediately (handled
/// where each of those is already detected, below and in h264.rs).
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct KeyframePolicy {
    last_idr: Option<Instant>,
    pending_request: bool,
    /// The `Service::joins` count already answered.
    joins: u64,
}

impl KeyframePolicy {
    /// Remember a PLI/FIR-style request; `due`/`wait` decide when it fires.
    fn request(&mut self) {
        self.pending_request = true;
    }
    /// Whether viewers joined since the last check. A join forces the next
    /// frame regardless of the request throttle below, so a new viewer
    /// decodes at once. The connected count cannot show this: it stays the
    /// same when another viewer leaves in the same network pass.
    fn joined(&mut self, joins: u64) -> bool {
        let new = joins > self.joins;
        self.joins = joins;
        new
    }
    /// Whether a pending request has waited out the throttle window (or
    /// there has never been an IDR, so there is nothing to throttle against).
    fn due(&self, now: Instant) -> bool {
        self.pending_request
            && self
                .last_idr
                .is_none_or(|at| now.saturating_duration_since(at) >= KEYFRAME_REQUEST_INTERVAL)
    }
    /// Time left until a pending request becomes due, to cap the encoder's
    /// condvar wait so it is honored as soon as the window ends, not on the
    /// next unrelated poll.
    fn wait(&self, now: Instant) -> Option<Duration> {
        if !self.pending_request || self.due(now) {
            return None;
        }
        let at = self.last_idr?;
        Some(KEYFRAME_REQUEST_INTERVAL.saturating_sub(now.saturating_duration_since(at)))
    }
    /// Record that an IDR actually went out at `now`, satisfying any pending
    /// request no matter what forced it.
    fn produced(&mut self, now: Instant) {
        self.last_idr = Some(now);
        self.pending_request = false;
    }
}

pub(super) fn run(
    mut config: LiveConfig,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
    service: Arc<Service>,
    tx: SyncSender<Encoded>,
) -> Result<()> {
    config.fps = config.fps.min(60);
    let mut encoder = AdaptiveEncoder::new(&config)?;
    // Only the cases below (plus the long hardware safety GOP this still
    // configures) should force an IDR; the periodic/scene-change one this
    // used to rely on cost bitrate and softened every frame after it.
    encoder.idr_only_when_requested()?;
    // No bandwidth estimation: it would move str0m to its rate-limited pacer
    // (added latency), and NVENC needs an IDR to change bitrate anyway.
    let mut converter = YuvConverter::default();
    let origin = Instant::now();
    let mut last_generation = 0;
    let mut last_frame = origin;
    let mut force = true;
    let mut keyframes = KeyframePolicy::default();
    while !stop.load(Ordering::SeqCst) && !service.failed.load(Ordering::SeqCst) {
        let (generation, raw) = {
            let mut data = frames.inner.lock().unwrap();
            loop {
                if data.ended.is_some()
                    || stop.load(Ordering::SeqCst)
                    || service.failed.load(Ordering::SeqCst)
                {
                    return Ok(());
                }
                let now = Instant::now();
                let connected = service.connected();
                if keyframes.joined(service.joins.load(Ordering::SeqCst)) {
                    // A brand-new viewer must decode right away; never make
                    // it wait out the PLI/FIR throttle below.
                    force = true;
                }
                if service.keyframe.swap(false, Ordering::SeqCst) {
                    keyframes.request();
                }
                force |= keyframes.due(now);
                let changed = data.generation != last_generation;
                // Capture supplies the only cadence for new frames. Only
                // repeated frames (PLI/join/keepalive) need an encoder deadline.
                let repeat_after = if force {
                    config.interval(1)
                } else {
                    Duration::from_secs(1)
                };
                let ready = changed || last_frame.elapsed() >= repeat_after;
                if connected > 0
                    && !service.queued.load(Ordering::SeqCst)
                    && ready
                    && let Some(raw) = data.raw.clone()
                {
                    break (data.generation, raw);
                }
                let wait = if connected == 0
                    || service.queued.load(Ordering::SeqCst)
                    || data.raw.is_none()
                {
                    Duration::from_millis(100)
                } else {
                    repeat_after
                        .saturating_sub(last_frame.elapsed())
                        .min(Duration::from_millis(100))
                };
                let wait = keyframes
                    .wait(now)
                    .map_or(wait, |remaining| wait.min(remaining));
                data = frames.tick.wait_timeout(data, wait).unwrap().0;
            }
        };
        let at = Instant::now();
        let changed = generation != last_generation;
        let yuv = converter.convert(&raw)?;
        let converted_at = Instant::now();
        let (bytes, keyframe) = encoder.encode(yuv, origin.elapsed().as_micros() as i64, force)?;
        if keyframe {
            // Any IDR satisfies a pending request, not only one it caused.
            keyframes.produced(at);
        }
        last_frame = at;
        last_generation = generation;
        if bytes.is_empty() {
            continue;
        }
        ensure!(
            bytes.len() <= MAX_FRAME_BYTES,
            "H.264 frame exceeds 2 MiB; lower the resolution"
        );
        {
            use openh264::formats::YUVSource;
            let mut metrics = service.metrics.lock().unwrap();
            metrics.stats.encoder = encoder.name.clone();
            metrics.stats.encoder_note = encoder.note.clone();
            metrics.stats.width = yuv.dimensions().0 as u32;
            metrics.stats.height = yuv.dimensions().1 as u32;
            metrics.stats.encoded_frames += 1;
            metrics.stats.keyframes += u64::from(keyframe);
            metrics.encoded.record(Instant::now(), 0, 1);
            let now = Instant::now();
            metrics.encode.record(now, at.elapsed());
            if changed {
                metrics
                    .capture_to_encode
                    .record(now, at.saturating_duration_since(raw.captured_at));
            }
            metrics.convert.record(now, converted_at.duration_since(at));
            metrics.codec.record(now, now.duration_since(converted_at));
        }
        let frame = Encoded {
            bytes: bytes.into(),
            keyframe,
            at,
            timestamp: at.duration_since(origin).as_micros() as u64,
            ready_at: Instant::now(),
        };
        service.queued.store(true, Ordering::SeqCst);
        match tx.try_send(frame) {
            Ok(()) => {
                force = false;
                service.wake_network();
            }
            Err(mpsc::TrySendError::Full(_)) => {
                // The dropped encoded frame might be a reference. Restart the
                // sequence with an IDR before delivering anything else.
                force = true;
                service.metrics.lock().unwrap().stats.dropped_frames += 1;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod static_tests {
    use super::*;
    use crate::live::EncoderMode;

    fn service(frames: &Arc<FrameState>, config: &LiveConfig) -> Arc<Service> {
        let (commands, _commands_rx) = mpsc::sync_channel(1);
        let (wake, _wake_rx) = std::os::unix::net::UnixDatagram::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        Arc::new(Service {
            commands,
            connected: AtomicUsize::new(1),
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
            fps: config.fps.min(60),
            frames: Arc::downgrade(&frames),
            queued: AtomicBool::new(false),
            wake,
        })
    }

    #[test]
    fn new_frames_have_no_second_fps_wait_and_backpressure_keeps_the_latest_capture() {
        let config = LiveConfig {
            fps: 1,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let frames = Arc::new(FrameState::new("pacing fixture".into()));
        let service = service(&frames, &config);
        let publish = |width| {
            let frame = CapturedFrame {
                image: image::RgbaImage::from_pixel(width, 32, image::Rgba([80, 140, 200, 255])),
                logical_width: width,
                logical_height: 32,
            };
            crate::live::publish_frame(&frames, frame, &config, Duration::ZERO).unwrap();
        };
        publish(32);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let (worker_frames, worker_service, worker_stop, worker_config) = (
            frames.clone(),
            service.clone(),
            stop.clone(),
            config.clone(),
        );
        let worker = thread::spawn(move || {
            run(
                worker_config,
                worker_frames,
                worker_stop,
                worker_service,
                tx,
            )
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        publish(48);
        service.queued.store(false, Ordering::SeqCst);
        frames.wake();
        // The old independent 1 FPS deadline would hold this frame for ~1 s.
        rx.recv_timeout(Duration::from_millis(400))
            .expect("new capture waited for another FPS interval");
        for width in [64, 80, 96] {
            publish(width);
        }
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(
            service.stats().encoded_frames,
            2,
            "encoded while network queue was full"
        );
        service.queued.store(false, Ordering::SeqCst);
        frames.wake();
        rx.recv_timeout(Duration::from_millis(400)).unwrap();
        assert_eq!(service.stats().width, 96, "did not take newest capture");
        assert_eq!(service.stats().capture_to_encode_ms.samples, 3);
        frames.fail("test complete".into());
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn a_new_peer_gets_an_idr_without_any_new_capture_and_source_loss_stops_encoding() {
        let config = LiveConfig {
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let frames = Arc::new(FrameState::new("unchanged source".into()));
        crate::live::publish_frame(
            &frames,
            omabeam_capture::demo_frame(0),
            &config,
            Duration::ZERO,
        )
        .unwrap();
        let service = service(&frames, &config);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let (worker_frames, worker_service, worker_stop) =
            (frames.clone(), service.clone(), stop.clone());
        let worker =
            thread::spawn(move || run(config, worker_frames, worker_stop, worker_service, tx));
        assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().keyframe);
        service.queued.store(false, Ordering::SeqCst);
        service.peer_joined();
        assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().keyframe);
        assert_eq!(frames.stats().frames, 1);
        assert!(frames.inner.lock().unwrap().jpeg.is_empty());
        frames.fail("window closed".into());
        worker.join().unwrap().unwrap();
        assert!(frames.inner.lock().unwrap().raw.is_none());
    }

    // -- KeyframePolicy: pure logic, no threads/video needed. --

    #[test]
    fn a_request_shortly_after_an_idr_waits_for_the_throttle_mark_not_dropped() {
        let mut policy = KeyframePolicy::default();
        let start = Instant::now();
        policy.produced(start);
        policy.request();
        assert!(
            !policy.due(start + Duration::from_millis(100)),
            "must not fire inside the 500 ms window"
        );
        assert!(
            !policy.due(start + Duration::from_millis(499)),
            "must still be pending just before the mark"
        );
        assert!(
            policy.due(start + KEYFRAME_REQUEST_INTERVAL),
            "must fire once the window elapses, not be dropped"
        );
    }

    #[test]
    fn wait_reports_the_remaining_throttle_window_and_none_once_due() {
        let mut policy = KeyframePolicy::default();
        let start = Instant::now();
        assert_eq!(policy.wait(start), None, "nothing pending yet");
        policy.produced(start);
        policy.request();
        assert_eq!(
            policy.wait(start + Duration::from_millis(100)),
            Some(Duration::from_millis(400))
        );
        assert_eq!(
            policy.wait(start + KEYFRAME_REQUEST_INTERVAL),
            None,
            "already due; nothing left to wait for"
        );
    }

    #[test]
    fn frequent_requests_are_capped_to_the_throttle_rate() {
        let mut policy = KeyframePolicy::default();
        let start = Instant::now();
        let mut idrs = 0;
        for i in 0..40u64 {
            let now = start + Duration::from_millis(i * 50);
            policy.request();
            if policy.due(now) {
                policy.produced(now);
                idrs += 1;
            }
        }
        assert!(
            idrs <= 5,
            "expected at most 5 IDRs for 40 requests over 2 s, got {idrs}"
        );
    }

    #[test]
    fn a_join_forces_even_while_a_request_is_throttled() {
        let mut policy = KeyframePolicy::default();
        let start = Instant::now();
        assert!(!policy.joined(0), "no viewer has joined yet");
        policy.produced(start);
        policy.request();
        assert!(
            !policy.due(start + Duration::from_millis(100)),
            "a plain request is still inside the window"
        );
        assert!(
            policy.joined(1),
            "a new viewer must force regardless of the request throttle"
        );
        assert!(!policy.joined(1), "an answered join must not force again");
        assert!(
            policy.joined(3),
            "joins made while the encoder was busy still force"
        );
    }

    #[test]
    fn a_join_wakes_a_waiting_encoder() {
        let frames = Arc::new(FrameState::new("wake fixture".into()));
        let service = service(&frames, &LiveConfig::default());
        let (ready, waiting) = mpsc::channel();
        let waiter = thread::spawn({
            let frames = frames.clone();
            move || {
                // Signal while holding the lock, so the wake lands in the wait.
                let data = frames.inner.lock().unwrap();
                ready.send(()).unwrap();
                let started = Instant::now();
                drop(frames.tick.wait_timeout(data, Duration::from_secs(5)));
                started.elapsed()
            }
        });
        waiting.recv().unwrap();
        service.peer_joined();
        let waited = waiter.join().unwrap();
        assert!(
            waited < Duration::from_secs(1),
            "a join left the encoder waiting {waited:?}"
        );
    }

    // -- run(): real threads, verifying the policy is wired in correctly. --

    #[test]
    fn a_viewer_replacing_another_gets_an_idr_at_once_but_a_pli_waits_for_the_mark() {
        let config = LiveConfig {
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let frames = Arc::new(FrameState::new("replacing viewer fixture".into()));
        crate::live::publish_frame(
            &frames,
            omabeam_capture::demo_frame(0),
            &config,
            Duration::ZERO,
        )
        .unwrap();
        let service = service(&frames, &config);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let (worker_frames, worker_service, worker_stop, worker_config) = (
            frames.clone(),
            service.clone(),
            stop.clone(),
            config.clone(),
        );
        let worker = thread::spawn(move || {
            run(
                worker_config,
                worker_frames,
                worker_stop,
                worker_service,
                tx,
            )
        });
        let first = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(first.keyframe);
        service.queued.store(false, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        // One viewer connects and another leaves in the same network pass, well
        // inside the 500 ms PLI/FIR throttle: the connected count stays 1.
        service.peer_joined();
        let joined = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(joined.keyframe);
        // `at` is when the encoder picked the frame, and what the throttle
        // measures from.
        assert!(
            joined.at.duration_since(first.at) < KEYFRAME_REQUEST_INTERVAL,
            "a newly connected viewer waited for the request throttle"
        );
        service.queued.store(false, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        service.request_keyframe();
        let requested = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(requested.keyframe);
        assert!(
            requested.at.duration_since(joined.at) >= KEYFRAME_REQUEST_INTERVAL,
            "a PLI 100 ms after an IDR was not deferred to the 500 ms mark"
        );
        frames.fail("test complete".into());
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn the_frame_after_a_dropped_encoded_frame_is_an_idr() {
        let config = LiveConfig {
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let frames = Arc::new(FrameState::new("dropped frame fixture".into()));
        crate::live::publish_frame(
            &frames,
            omabeam_capture::demo_frame(0),
            &config,
            Duration::ZERO,
        )
        .unwrap();
        let service = service(&frames, &config);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let tx_test = tx.clone();
        let (worker_frames, worker_service, worker_stop, worker_config) = (
            frames.clone(),
            service.clone(),
            stop.clone(),
            config.clone(),
        );
        let worker = thread::spawn(move || {
            run(
                worker_config,
                worker_frames,
                worker_stop,
                worker_service,
                tx,
            )
        });
        assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().keyframe);
        service.queued.store(false, Ordering::SeqCst);
        // Occupy the one-slot channel so the encoder's next send finds it full.
        tx_test
            .try_send(Encoded {
                bytes: vec![0u8; 4].into(),
                keyframe: false,
                at: Instant::now(),
                timestamp: 0,
                ready_at: Instant::now(),
            })
            .unwrap();
        crate::live::publish_frame(
            &frames,
            omabeam_capture::demo_frame(1),
            &config,
            Duration::ZERO,
        )
        .unwrap();
        // Give the encoder a chance to attempt (and drop) its real encode
        // against the still-full channel before we drain the placeholder.
        thread::sleep(Duration::from_millis(200));
        assert_eq!(service.stats().dropped_frames, 1);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        service.queued.store(false, Ordering::SeqCst);
        frames.wake();
        let recovered = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(recovered.keyframe, "the frame after a drop must be an IDR");
        frames.fail("test complete".into());
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn two_hundred_changing_frames_with_no_requests_produce_exactly_one_idr() {
        let config = LiveConfig {
            webrtc: true,
            encoder: EncoderMode::Software,
            ..Default::default()
        };
        let frames = Arc::new(FrameState::new("changing source fixture".into()));
        let service = service(&frames, &config);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let (worker_frames, worker_service, worker_stop, worker_config) = (
            frames.clone(),
            service.clone(),
            stop.clone(),
            config.clone(),
        );
        let worker = thread::spawn(move || {
            run(
                worker_config,
                worker_frames,
                worker_stop,
                worker_service,
                tx,
            )
        });
        let mut keyframes = 0u32;
        for index in 0..200u32 {
            crate::live::publish_frame(
                &frames,
                omabeam_capture::demo_frame(index),
                &config,
                Duration::ZERO,
            )
            .unwrap();
            let frame = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            keyframes += u32::from(frame.keyframe);
            service.queued.store(false, Ordering::SeqCst);
        }
        assert_eq!(keyframes, 1, "only the very first frame should be an IDR");
        frames.fail("test complete".into());
        worker.join().unwrap().unwrap();
    }
}
