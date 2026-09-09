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

#[test]
fn default_bind_is_loopback() {
    assert!(LiveConfig::default().bind.is_loopback());
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
        "--bind",
        "::1",
        "--port",
        "0",
    ]))
    .unwrap();
    assert_eq!(rest, args(&["--live", "output", "DP-1"]));
    assert_eq!(config.fps, 30);
    assert!(config.cursor);
    assert_eq!(
        LiveConfig::parse_args(&config.to_cli_args()).unwrap().0,
        config
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
    frames.publish(vec![1, 2, 3], 100, 50);
    frames.fail("source closed".into());
    frames.publish(vec![4], 200, 100);
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
    frames.publish(vec![0xff, 0xd8, 0xff, 0xd9], 1, 1);
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
    frames.publish(vec![1, 2, 3], 1, 1);
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
    frames.publish(vec![1], 1, 1);
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
