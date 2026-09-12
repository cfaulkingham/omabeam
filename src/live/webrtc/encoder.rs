use super::*;
use openh264::{
    OpenH264API, Timestamp,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
        Profile, RateControlMode, UsageType,
    },
    formats::{RgbSliceU8, YUVBuffer},
};

pub(super) struct Encoded {
    pub bytes: Arc<[u8]>,
    pub keyframe: bool,
    pub at: Instant,
    pub timestamp: u64,
}

fn create(config: &LiveConfig) -> Result<Encoder> {
    Ok(Encoder::with_api_config(
        OpenH264API::from_source(),
        EncoderConfig::new()
            .bitrate(BitRate::from_bps(config.h264_bitrate))
            .max_frame_rate(FrameRate::from_hz(config.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .num_threads(2)
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.fps * 2)),
    )?)
}

/// Pad odd edges by one pixel for I420 without rescaling native text.
fn yuv(raw: &RawFrame) -> Result<YUVBuffer> {
    let (width, height) = raw
        .frame
        .stream_dimensions(raw.config.max_width, raw.config.pixel_mode)?;
    let (w, h) = (
        width.next_multiple_of(2) as usize,
        height.next_multiple_of(2) as usize,
    );
    ensure!(
        w >= 16 && h >= 16,
        "H.264 needs at least 16 pixels on each edge; use JPEG for this size"
    );
    ensure!(
        w.max(h) <= 3840 && w.min(h) <= 2160,
        "H.264 supports up to 3840×2160 (or portrait); choose a maximum width or use JPEG"
    );
    let rgb = raw
        .frame
        .stream_rgb(raw.config.max_width, raw.config.pixel_mode)?;
    if (w, h) == (width as usize, height as usize) {
        return Ok(YUVBuffer::from_rgb8_source(RgbSliceU8::new(
            rgb.as_raw(),
            (w, h),
        )));
    }
    let mut padded = vec![0; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            padded[(y * w + x) * 3..(y * w + x + 1) * 3].copy_from_slice(
                &rgb.get_pixel((x as u32).min(width - 1), (y as u32).min(height - 1))
                    .0,
            );
        }
    }
    Ok(YUVBuffer::from_rgb8_source(RgbSliceU8::new(
        &padded,
        (w, h),
    )))
}

pub(super) fn run(
    mut config: LiveConfig,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
    service: Arc<Service>,
    tx: SyncSender<Encoded>,
) -> Result<()> {
    config.fps = config.fps.min(60);
    let mut encoder = create(&config)?;
    let origin = Instant::now();
    let mut last_generation = 0;
    let mut last_frame = origin;
    let mut force = true;
    while !stop.load(Ordering::SeqCst) && !service.failed.load(Ordering::SeqCst) {
        let (generation, raw) = {
            let data = frames.inner.lock().unwrap();
            if data.ended.is_some() {
                break;
            }
            (data.generation, data.raw.clone())
        };
        if service.connected() == 0 {
            force = true;
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        force |= service.keyframe.swap(false, Ordering::SeqCst);
        // A static screen must still produce a new IDR after a join or PLI.
        // A one-second repeat also detects receiver stalls without screen damage.
        if (generation == last_generation
            && !force
            && last_frame.elapsed() < Duration::from_secs(1))
            || last_frame.elapsed() < config.interval(1)
        {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let Some(raw) = raw else {
            continue;
        };
        let at = Instant::now();
        let yuv = yuv(&raw)?;
        if force {
            encoder.force_intra_frame();
        }
        let bitstream = encoder.encode_at(
            &yuv,
            Timestamp::from_millis(origin.elapsed().as_millis() as u64),
        )?;
        let keyframe = bitstream.frame_type() == FrameType::IDR;
        let bytes = bitstream.to_vec();
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
            metrics.stats.width = yuv.dimensions().0 as u32;
            metrics.stats.height = yuv.dimensions().1 as u32;
            metrics.stats.encoded_frames += 1;
            metrics.stats.keyframes += u64::from(keyframe);
            metrics.encoded.record(Instant::now(), 0, 1);
            metrics.encode.record(Instant::now(), at.elapsed());
        }
        let frame = Encoded {
            bytes: bytes.into(),
            keyframe,
            at,
            timestamp: at.duration_since(origin).as_micros() as u64,
        };
        match tx.try_send(frame) {
            Ok(()) => force = false,
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
mod tests {
    use super::*;
    use openh264::{decoder::Decoder, formats::YUVSource};

    #[test]
    fn h264_decodes_odd_edges_keyframe_recovery_and_resolution_changes() {
        let config = LiveConfig {
            webrtc: true,
            ..Default::default()
        };
        let mut encoder = create(&config).unwrap();
        let mut decoder = Decoder::new().unwrap();
        for (index, (w, h)) in [(641, 361), (641, 361), (480, 270), (17, 17)]
            .into_iter()
            .enumerate()
        {
            let mut frame = omabeam_capture::demo_frame(index as u32);
            frame.logical_width = w;
            frame.logical_height = h;
            let raw = RawFrame {
                frame,
                config: config.clone(),
            };
            let yuv = yuv(&raw).unwrap();
            encoder.force_intra_frame();
            let bitstream = encoder
                .encode_at(&yuv, Timestamp::from_millis(index as u64 * 1000))
                .unwrap();
            assert_eq!(bitstream.frame_type(), FrameType::IDR);
            let bytes = bitstream.to_vec();
            let decoded = decoder
                .decode(&bytes)
                .unwrap()
                .expect("an actual decoded frame");
            assert_eq!(
                decoded.dimensions(),
                (
                    w.next_multiple_of(2) as usize,
                    h.next_multiple_of(2) as usize
                )
            );
        }
    }

    #[test]
    fn oversized_native_images_report_a_fallback_instead_of_silently_downscaling() {
        let mut frame = omabeam_capture::demo_frame(0);
        frame.logical_width = 8000;
        let raw = RawFrame {
            frame,
            config: LiveConfig::default(),
        };
        assert!(yuv(&raw).err().unwrap().to_string().contains("3840"));
    }
}

#[cfg(test)]
mod static_tests {
    use super::*;

    #[test]
    fn a_new_peer_gets_an_idr_without_any_new_capture_and_source_loss_stops_encoding() {
        let config = LiveConfig {
            webrtc: true,
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
        let (commands, _commands_rx) = mpsc::sync_channel(1);
        let service = Arc::new(Service {
            commands,
            connected: AtomicUsize::new(1),
            keyframe: AtomicBool::new(true),
            failed: AtomicBool::new(false),
            metrics: Mutex::new(Metrics {
                stats: WebRtcStats::default(),
                encode: Timings::default(),
                output: Rate::new(Instant::now()),
                encoded: Rate::new(Instant::now()),
            }),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let (worker_frames, worker_service, worker_stop) =
            (frames.clone(), service.clone(), stop.clone());
        let worker =
            thread::spawn(move || run(config, worker_frames, worker_stop, worker_service, tx));
        assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().keyframe);
        service.keyframe.store(true, Ordering::SeqCst);
        assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().keyframe);
        assert_eq!(frames.stats().frames, 1);
        assert!(frames.inner.lock().unwrap().jpeg.is_empty());
        frames.fail("window closed".into());
        worker.join().unwrap().unwrap();
        assert!(frames.inner.lock().unwrap().raw.is_none());
    }
}
