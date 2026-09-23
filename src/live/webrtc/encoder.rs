use super::*;
use crate::live::h264::{AdaptiveEncoder, YuvConverter};
pub(super) struct Encoded {
    pub bytes: Arc<[u8]>,
    pub keyframe: bool,
    pub at: Instant,
    pub timestamp: u64,
    pub ready_at: Instant,
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
    let mut converter = YuvConverter::default();
    let origin = Instant::now();
    let mut last_generation = 0;
    let mut last_frame = origin;
    let mut force = true;
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
                if service.connected() == 0 {
                    force = true;
                }
                force |= service.keyframe.swap(false, Ordering::SeqCst);
                let changed = data.generation != last_generation;
                // Capture supplies the only cadence for new frames. Only
                // repeated frames (PLI/join/keepalive) need an encoder deadline.
                let repeat_after = if force {
                    config.interval(1)
                } else {
                    Duration::from_secs(1)
                };
                let ready = changed || last_frame.elapsed() >= repeat_after;
                if service.connected() > 0
                    && !service.queued.load(Ordering::SeqCst)
                    && ready
                    && let Some(raw) = data.raw.clone()
                {
                    break (data.generation, raw);
                }
                let wait = if service.connected() == 0
                    || service.queued.load(Ordering::SeqCst)
                    || data.raw.is_none()
                {
                    Duration::from_millis(100)
                } else {
                    repeat_after
                        .saturating_sub(last_frame.elapsed())
                        .min(Duration::from_millis(100))
                };
                data = frames.tick.wait_timeout(data, wait).unwrap().0;
            }
        };
        let at = Instant::now();
        let changed = generation != last_generation;
        let yuv = converter.convert(&raw)?;
        let converted_at = Instant::now();
        let (bytes, keyframe) = encoder.encode(yuv, origin.elapsed().as_micros() as i64, force)?;
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
        service.request_keyframe();
        assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().keyframe);
        assert_eq!(frames.stats().frames, 1);
        assert!(frames.inner.lock().unwrap().jpeg.is_empty());
        frames.fail("window closed".into());
        worker.join().unwrap().unwrap();
        assert!(frames.inner.lock().unwrap().raw.is_none());
    }
}
