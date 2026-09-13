use super::*;
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    sync::atomic::AtomicUsize,
};

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}
fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(10));
    }
}
fn exchange(frames: &Arc<FrameState>, request: &[u8]) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let frames = frames.clone();
    let worker = thread::spawn(move || {
        http::handle_client(
            listener.accept().unwrap().0,
            "/s/test",
            frames,
            Arc::new(AtomicBool::new(false)),
        )
    });
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket.write_all(request).unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).unwrap();
    worker.join().unwrap();
    response
}

fn desktop_post(frames: &Arc<FrameState>, path: &str, body: serde_json::Value) -> Vec<u8> {
    let body = body.to_string();
    exchange(frames, format!("POST /s/test/{path} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
}

#[test]
fn extended_media_cannot_bypass_the_display_claim() {
    let mut frames = FrameState::new("extended".into());
    frames.desktop = Some(Arc::new(desktop::DesktopControl::new(Default::default())));
    let frames = Arc::new(frames);
    publish_frame(
        &frames,
        omabeam_capture::demo_frame(0),
        &LiveConfig::default(),
        Duration::ZERO,
    )
    .unwrap();
    let client = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let page = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let other = "cccccccccccccccccccccccccccccccc";
    for route in ["frame.jpg", "stream", "webrtc/offer", "webrtc/close"] {
        let reply = exchange(
            &frames,
            format!("GET /s/test/{route} HTTP/1.1\r\n\r\n").as_bytes(),
        );
        assert!(reply.starts_with(b"HTTP/1.1 409"), "{route}");
    }
    assert!(
        desktop_post(
            &frames,
            "desktop/claim",
            serde_json::json!({"client": client, "connection": page})
        )
        .starts_with(b"HTTP/1.1 200")
    );
    assert!(
        desktop_post(
            &frames,
            "desktop/claim",
            serde_json::json!({"client": other, "connection": other})
        )
        .starts_with(b"HTTP/1.1 409")
    );
    assert!(
        exchange(
            &frames,
            format!("GET /s/test/frame.jpg?viewer={page} HTTP/1.1\r\n\r\n").as_bytes()
        )
        .starts_with(b"HTTP/1.1 200")
    );
    assert!(
        exchange(
            &frames,
            format!("GET /s/test/frame.jpg?viewer={other} HTTP/1.1\r\n\r\n").as_bytes()
        )
        .starts_with(b"HTTP/1.1 409")
    );
    let resize = serde_json::json!({"connection": other, "size": {"width": 1280, "height": 800, "scale": 1}});
    assert!(desktop_post(&frames, "desktop/size", resize).starts_with(b"HTTP/1.1 409"));
    let body = serde_json::json!({"client": client, "connection": page}).to_string();
    let cross_origin = format!(
        "POST /s/test/desktop/claim HTTP/1.1\r\nHost: localhost\r\nOrigin: http://other.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    assert!(exchange(&frames, cross_origin.as_bytes()).starts_with(b"HTTP/1.1 400"));
    desktop_post(
        &frames,
        "desktop/release",
        serde_json::json!({"connection": page}),
    );
    assert!(
        exchange(
            &frames,
            format!("GET /s/test/frame.jpg?viewer={page} HTTP/1.1\r\n\r\n").as_bytes()
        )
        .starts_with(b"HTTP/1.1 409")
    );
    frames.fail("display removed".into());
    assert!(
        desktop_post(
            &frames,
            "desktop/claim",
            serde_json::json!({"client": client, "connection": other})
        )
        .starts_with(b"HTTP/1.1 410")
    );
}

#[test]
fn matching_uses_native_pixels_and_restores_host_encoding_limits() {
    let mut frames = FrameState::new("extended".into());
    let control = Arc::new(desktop::DesktopControl::new(Default::default()));
    frames.desktop = Some(control.clone());
    let client = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    control.claim(client, client).unwrap();
    control.request_size(client, Some((1280, 800, 2))).unwrap();
    control
        .apply_resize(control.take_resize().unwrap(), |_| Ok(()))
        .unwrap();
    let mut frame = omabeam_capture::demo_frame(0);
    frame.logical_width = 320;
    frame.logical_height = 180;
    let config = LiveConfig {
        max_width: Some(160),
        webrtc: true,
        ..Default::default()
    };
    publish_frame(
        &frames,
        CapturedFrame {
            image: frame.image.clone(),
            logical_width: frame.logical_width,
            logical_height: frame.logical_height,
        },
        &config,
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!((frames.stats().width, frames.stats().height), (640, 360));
    assert_eq!(
        frames
            .inner
            .lock()
            .unwrap()
            .raw
            .as_ref()
            .unwrap()
            .config
            .max_width,
        None
    );
    assert!(control.request_size(client, None).is_err()); // Resize pacing applies.
    thread::sleep(Duration::from_millis(760));
    control.request_size(client, None).unwrap();
    control
        .apply_resize(control.take_resize().unwrap(), |_| Ok(()))
        .unwrap();
    publish_frame(&frames, frame, &config, Duration::ZERO).unwrap();
    assert_eq!((frames.stats().width, frames.stats().height), (160, 90));
    assert_eq!(
        frames.inner.lock().unwrap().raw.as_ref().unwrap().config,
        config
    );
}

/// Invoked by tests/extended_viewer.py with a private mock compositor socket.
/// Production HTTP, leases, resize transactions, IPC and encoders are used;
/// only Wayland pixels are synthetic so the fixture also runs on macOS.
#[test]
#[ignore = "browser fixture; run tests/extended_viewer.py"]
fn extended_desktop_browser_fixture() {
    let directory = std::path::PathBuf::from(
        std::env::var("OMABEAM_FIXTURE_DIR").expect("fixture directory required"),
    );
    let original = crate::hypr::desktop::DesktopConfig {
        width: 1280,
        height: 720,
        ..Default::default()
    };
    let display = Arc::new(Mutex::new(
        crate::hypr::desktop::VirtualDisplay::create(&original).unwrap(),
    ));
    let control = Arc::new(desktop::DesktopControl::new(original.clone()));
    let next_control = control.clone();
    let next_display = display.clone();
    fn pixels(config: &crate::hypr::desktop::DesktopConfig, tick: u32) -> CapturedFrame {
        CapturedFrame {
            image: image::RgbaImage::from_fn(config.width, config.height, |x, y| {
                image::Rgba([((x + tick * 7) % 256) as u8, (y % 256) as u8, 180, 255])
            }),
            logical_width: config.width / config.scale,
            logical_height: config.height / config.scale,
        }
    }
    let config = LiveConfig {
        bind: "127.0.0.1".parse().unwrap(),
        port: 0,
        webrtc_port: 0,
        webrtc: true,
        encoder: EncoderMode::Software,
        pixel_mode: omabeam_capture::PixelMode::Native,
        fps: 10,
        ..Default::default()
    };
    let mut counter = 0;
    let mut session = LiveSession::start_frames(
        "Extended desktop fixture".into(),
        config,
        pixels(&original, 0),
        Duration::ZERO,
        Some(control),
        move |_| {
            counter += 1;
            if let Some(resize) = next_control.take_resize() {
                return next_control.apply_resize(resize, |config| {
                    next_display.lock().unwrap().resize(config)?;
                    Ok(Some(pixels(config, counter)))
                });
            }
            Ok(Some(pixels(&next_control.stats().config, counter)))
        },
    )
    .unwrap();
    session.display = Some(display);
    std::fs::write(
        directory.join("ready.json"),
        serde_json::json!({"url": session.url}).to_string(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    while !directory.join("stop").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    drop(session);
}

#[test]
fn default_bind_is_local_network() {
    assert!(LiveConfig::default().bind.is_unspecified());
}

#[test]
fn options_validate_and_round_trip_through_daemon_arguments() {
    let (config, rest) = LiveConfig::parse_args(&args(&[
        "--fps",
        "30",
        "--live",
        "output",
        "DP-1",
        "--quality",
        "72",
        "--width",
        "1280",
        "--cursor",
        "--native-pixels",
        "--webrtc",
        "--webrtc-port",
        "0",
        "--h264-bitrate",
        "8000000",
        "--encoder",
        "hardware",
        "--bind",
        "::1",
        "--port",
        "0",
    ]))
    .unwrap();
    assert_eq!(rest, args(&["--live", "output", "DP-1"]));
    assert_eq!(config.fps, 30);
    assert!(config.cursor && config.webrtc);
    assert_eq!(config.h264_bitrate, 8_000_000);
    assert_eq!(config.encoder, EncoderMode::Hardware);
    assert_eq!(config.webrtc_port, 0);
    assert_eq!(config.pixel_mode, omabeam_capture::PixelMode::Native);
    assert_eq!(
        LiveConfig::parse_args(&config.to_cli_args()).unwrap().0,
        config
    );
    assert!(LiveConfig::default().webrtc);
    let (jpeg, _) = LiveConfig::parse_args(&args(&["--jpeg"])).unwrap();
    assert!(!jpeg.webrtc);
    assert_eq!(
        LiveConfig::parse_args(&jpeg.to_cli_args()).unwrap().0,
        jpeg
    );
    assert!(
        LiveConfig::parse_args(&args(&["--jpeg", "--webrtc"]))
            .unwrap()
            .0
            .webrtc
    );
    for input in [
        vec!["--fps", "0"],
        vec!["--fps", "121"],
        vec!["--quality", "96"],
        vec!["--width", "0"],
        vec!["--port", "65536"],
        vec!["--bind", "invalid"],
        vec!["--fps"],
        vec!["--wat"],
        vec!["--webrtc-port", "65536"],
        vec!["--h264-bitrate", "0"],
        vec!["--h264-bitrate", "50000001"],
        vec!["--encoder", "unknown"],
        vec!["--encoder"],
    ] {
        assert!(LiveConfig::parse_args(&args(&input)).is_err(), "{input:?}");
    }
    assert_eq!(
        LiveConfig::parse_args(&args(&["--live", "--", "output", "--cursor"]))
            .unwrap()
            .1,
        args(&["--live", "output", "--cursor"])
    );
}

#[test]
fn source_arguments_preserve_identity_and_never_fall_back_to_screen_pixels() {
    let sources = [
        LiveSource::Extend(crate::hypr::desktop::DesktopConfig::default()),
        LiveSource::Output {
            name: "DP-1".into(),
        },
        LiveSource::Window {
            address: "0xabc".into(),
            stable_id: "stable-1".into(),
            label: "foot — term".into(),
        },
        LiveSource::Region {
            output: "DP-2".into(),
            x: 10,
            y: 20,
            w: 100,
            h: 50,
        },
    ];
    for source in sources {
        assert_eq!(
            LiveSource::from_cli_args(&source.to_cli_args()).unwrap(),
            source
        );
    }
    let source = LiveSource::Window {
        address: "0xabc".into(),
        stable_id: "".into(),
        label: "term".into(),
    };
    assert!(
        source
            .request()
            .unwrap_err()
            .to_string()
            .contains("Choose Area")
    );
    assert!(LiveSource::from_cli_args(&args(&["output", "DP-1", "extra"])).is_err());
    assert!(
        LiveSource::Region {
            output: "DP-1".into(),
            x: 0,
            y: 0,
            w: -1,
            h: 10
        }
        .request()
        .unwrap()
        .target()
        .is_err()
    );
}

#[test]
fn stats_measure_elapsed_time_and_clear_frames_after_source_failure() {
    let frames = FrameState::new("title \"quoted\"\nUnicode —".into());
    let first = Instant::now();
    let times = VecDeque::from([
        first,
        first + Duration::from_millis(200),
        first + Duration::from_millis(400),
    ]);
    assert!((state::measured_fps(&times, first + Duration::from_millis(400)) - 5.0).abs() < 0.001);
    assert_eq!(
        state::measured_fps(&times, first + Duration::from_secs(4)),
        0.0
    );
    frames.publish(vec![1, 2, 3], 100, 50, FrameMeasurement::default());
    frames.fail("source closed".into());
    frames.publish(vec![4], 200, 100, FrameMeasurement::default());
    assert!(frames.inner.lock().unwrap().jpeg.is_empty());
    let stats = frames.stats();
    assert_eq!(stats.frames, 1);
    assert_eq!(stats.state, "ended");
    assert_eq!(stats.fps, 0.0);
    assert_eq!(
        serde_json::from_slice::<StreamStats>(&serde_json::to_vec(&stats).unwrap()).unwrap(),
        stats
    );
}

#[test]
fn status_requires_current_fields_and_has_strong_tokens() {
    assert!(
        serde_json::from_str::<LiveStatus>(r#"{"pid":123,"url":"http://example/","title":"term"}"#)
            .is_err()
    );
    let token = random_token().unwrap();
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert_ne!(token, random_token().unwrap());
    assert!(!pid_alive(0));
    let old = r#"{"fps":15.0,"width":640,"height":360,"frames":1,"uptime":1,"viewers":0,"source":"demo","state":"live","error":null}"#;
    let stats: StreamStats = serde_json::from_str(old).unwrap();
    assert_eq!(stats.diagnostics, StreamDiagnostics::default());
}

#[test]
fn publish_uses_native_pixels_and_reports_capture_encode_measurements() {
    let frames = FrameState::new("HiDPI".into());
    let hidpi = || {
        let mut frame = omabeam_capture::demo_frame(0);
        frame.logical_width = 320;
        frame.logical_height = 180;
        frame
    };
    let mut config = LiveConfig {
        webrtc: false,
        ..LiveConfig::default()
    };
    publish_frame(&frames, hidpi(), &config, Duration::from_millis(12)).unwrap();
    assert_eq!((frames.stats().width, frames.stats().height), (320, 180));
    config.pixel_mode = omabeam_capture::PixelMode::Native;
    publish_frame(&frames, hidpi(), &config, Duration::from_millis(18)).unwrap();
    let stats = frames.stats();
    assert_eq!((stats.width, stats.height), (640, 360));
    let d = stats.diagnostics;
    assert!(d.native_pixels);
    assert_eq!((d.capture_width, d.capture_height), (640, 360));
    assert_eq!((d.logical_width, d.logical_height), (320, 180));
    assert_eq!(d.capture_wait_ms.p95, Some(18.0));
    assert_eq!(d.encode_ms.samples, 2);
    assert!(d.encode_ms.p50.unwrap() > 0.0);
    assert_eq!(d.jpeg_bytes, frames.inner.lock().unwrap().jpeg.len());
    config.max_width = Some(480);
    publish_frame(&frames, hidpi(), &config, Duration::ZERO).unwrap();
    assert_eq!((frames.stats().width, frames.stats().height), (480, 270));
}

#[test]
fn per_viewer_diagnostics_do_not_expand_the_bounded_session_file() {
    let frames = FrameState::new("x".repeat(status::MAX_SOURCE_BYTES));
    publish_frame(
        &frames,
        omabeam_capture::demo_frame(0),
        &LiveConfig::default(),
        Duration::ZERO,
    )
    .unwrap();
    let viewers: Vec<_> = (0..64).map(|_| state::Viewer::new(&frames)).collect();
    let status = LiveStatus {
        pid: 1,
        starttime: 1,
        title: "x".repeat(status::MAX_TITLE_BYTES),
        url: "x".repeat(status::MAX_URL_BYTES),
        stats: frames.stats(),
    };
    assert_eq!(frames.viewer_diagnostics().len(), 64);
    assert!(serde_json::to_vec(&status).unwrap().len() < status::MAX_STATUS_BYTES);
    drop(viewers);
    assert!(frames.viewer_diagnostics().is_empty());
}

#[test]
fn http_validates_methods_routes_and_token_before_serving_frames_or_stats() {
    let frames = Arc::new(FrameState::new("demo".into()));
    for (request, expected) in [
        ("GET /s/test/frame.jpg HTTP/1.1\r\n\r\n", "503"),
        ("POST /s/test/ HTTP/1.1\r\n\r\n", "405"),
        ("GET /s/test/ HTTP/0.9\r\n\r\n", "400"),
        ("GET /s/test-more/stats HTTP/1.1\r\n\r\n", "404"),
        ("GET /s/wrong/frame.jpg HTTP/1.1\r\n\r\n", "404"),
        ("GET /stats HTTP/1.1\r\n\r\n", "404"),
        ("GET /s/test HTTP/1.1\r\n\r\n", "302"),
        ("GET /s/test/stats HTTP/1.1\r\n\r\n", "200"),
    ] {
        let response = exchange(&frames, request.as_bytes());
        assert!(String::from_utf8_lossy(&response).starts_with(&format!("HTTP/1.1 {expected}")));
    }
    frames.publish(
        vec![0xff, 0xd8, 0xff, 0xd9],
        1,
        1,
        FrameMeasurement::default(),
    );
    assert!(
        exchange(&frames, b"GET /s/test/frame.jpg HTTP/1.1\r\n\r\n")
            .ends_with(&[0xff, 0xd8, 0xff, 0xd9])
    );
    frames.fail("window closed".into());
    assert!(
        String::from_utf8_lossy(&exchange(
            &frames,
            b"GET /s/test/frame.jpg HTTP/1.1\r\n\r\n"
        ))
        .starts_with("HTTP/1.1 503")
    );
    assert!(
        String::from_utf8_lossy(&exchange(&frames, b"GET /s/test/stats HTTP/1.1\r\n\r\n"))
            .contains("window closed")
    );
}

#[test]
fn static_stream_sends_first_frame_immediately_and_reaps_disconnected_viewers() {
    let frames = Arc::new(FrameState::new("static".into()));
    frames.publish(vec![1, 2, 3], 1, 1, FrameMeasurement::default());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let (socket, _) = listener.accept().unwrap();
    let worker_frames = frames.clone();
    let worker = thread::spawn(move || {
        http::handle_client(
            socket,
            "/s/test",
            worker_frames,
            Arc::new(AtomicBool::new(false)),
        )
    });
    client
        .write_all(b"GET /s/test/stream HTTP/1.1\r\n\r\n")
        .unwrap();
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    line.clear();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "--omabeamframe\r\n");
    eventually(|| frames.viewers.load(Ordering::SeqCst) == 1);
    reader.get_ref().shutdown(Shutdown::Both).unwrap();
    drop(reader);
    eventually(|| frames.viewers.load(Ordering::SeqCst) == 0);
    worker.join().unwrap();
}

#[test]
fn capture_failure_stops_production_and_is_visible_to_viewer() {
    let frames = Arc::new(FrameState::new("window".into()));
    frames.publish(vec![1], 1, 1, FrameMeasurement::default());
    let calls = AtomicUsize::new(0);
    capture_loop(
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            bail!("selected source lost")
        },
        LiveConfig::default(),
        frames.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(frames.stats().state, "ended");
    assert!(
        frames
            .stats()
            .error
            .unwrap()
            .contains("selected source lost")
    );
    assert!(frames.inner.lock().unwrap().jpeg.is_empty());
}

#[test]
fn pacing_changes_with_viewers_and_idle_wait_can_be_cancelled() {
    let config = LiveConfig {
        fps: 30,
        ..LiveConfig::default()
    };
    assert_eq!(config.interval(0), Duration::from_secs(1));
    assert!(config.interval(1) < Duration::from_millis(34));
    let frames = Arc::new(FrameState::new("idle".into()));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_frames = frames.clone();
    let worker_stop = stop.clone();
    let worker =
        thread::spawn(move || capture_loop(|_| Ok(None), config, worker_frames, worker_stop));
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    stop.store(true, Ordering::SeqCst);
    frames.tick.notify_all();
    worker.join().unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn slow_reader_cannot_extend_a_write_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    let bytes = vec![0u8; 32 * 1024 * 1024];
    let start = Instant::now();
    assert!(http::write_parts(&mut server, &[&bytes], Duration::from_millis(100)).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(client);
}

#[test]
#[ignore = "requires a running Hyprland compositor"]
fn streams_a_jpeg_from_the_active_monitor() {
    let snapshot = crate::hypr::Snapshot::load().unwrap();
    let session = LiveSession::start_with_config(
        LiveSource::Output {
            name: snapshot.focused_monitor().unwrap().name.clone(),
        },
        LiveConfig {
            bind: "127.0.0.1".parse().unwrap(),
            port: 0,
            webrtc: false,
            ..LiveConfig::default()
        },
    )
    .unwrap();
    assert!(
        session
            .frames
            .inner
            .lock()
            .unwrap()
            .jpeg
            .starts_with(&[0xff, 0xd8])
    );
    session.stop();
}

#[test]
fn rtc_only_capture_encodes_jpeg_on_demand_and_clears_pixels_on_source_loss() {
    let frames = FrameState::new("RTC source".into());
    let config = LiveConfig {
        webrtc: true,
        max_width: Some(321),
        ..Default::default()
    };
    publish_frame(
        &frames,
        omabeam_capture::demo_frame(1),
        &config,
        Duration::ZERO,
    )
    .unwrap();
    assert!(frames.inner.lock().unwrap().jpeg.is_empty());
    assert_eq!(frames.stats().diagnostics.encode_ms.samples, 0);
    let (jpeg, generation, encoded_at) = frames.jpeg_frame().unwrap();
    assert!(jpeg.starts_with(&[0xff, 0xd8]));
    assert_eq!(generation, 1);
    assert_eq!(frames.stats().diagnostics.encode_ms.samples, 1);
    assert!(Arc::ptr_eq(&jpeg, &frames.jpeg_frame().unwrap().0));
    assert_eq!(frames.jpeg_frame().unwrap().2, encoded_at);
    frames.fail("selected window closed".into());
    assert!(frames.jpeg_frame().unwrap().0.is_empty());
    assert!(frames.inner.lock().unwrap().raw.is_none());
}
