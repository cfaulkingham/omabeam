//! Exercise the installed-style CLI with no external commands on PATH. The
//! fixture speaks Hyprland socket1 and checks every request from the real binary.
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixListener,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

fn run(args: &[&str], replies: Vec<(String, String)>) -> Output {
    let runtime = tempfile::Builder::new()
        .prefix("ob-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket_dir = runtime.path().join("hypr/test-instance");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let listener = UnixListener::bind(socket_dir.join(".socket.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        for (expected, reply) in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing request {expected:?}");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // Accepted sockets inherit the listener's nonblocking flag on macOS.
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = vec![0; expected.len()];
            socket.read_exact(&mut request).unwrap();
            assert_eq!(request, expected.as_bytes());
            socket.write_all(reply.as_bytes()).unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            let mut extra = Vec::new();
            socket.read_to_end(&mut extra).unwrap();
            assert!(extra.is_empty());
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_omabeam"))
        .args(args)
        .env_clear()
        .env("PATH", runtime.path().join("no-external-tools"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("HYPRLAND_INSTANCE_SIGNATURE", "test-instance")
        .output()
        .unwrap();
    server.join().unwrap();
    output
}

#[test]
fn queries_and_reloads_without_hyprctl() {
    for (command, reply) in [
        ("clients", "[]"),
        ("monitors", r#"[{"name":"DP-1"}]"#),
        ("workspaces", r#"[{"id":1,"name":"1"}]"#),
        ("activeworkspace", r#"{"id":1,"name":"1"}"#),
        ("activewindow", r#"{"stableId":"window-1"}"#),
        ("version", r#"{"version":"test"}"#),
        ("configerrors", "[]"),
        ("reload", "ok"),
    ] {
        let request = if command == "reload" {
            "/reload".into()
        } else {
            format!("j/{command}")
        };
        let output = run(&["--hypr", command], vec![(request, reply.into())]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), reply);
    }
}

#[test]
fn reports_failed_queries_and_reloads() {
    for (command, reply) in [
        ("reload", "error: reload failed"),
        ("reload", "unknown request"),
        ("monitors", "not json"),
    ] {
        let request = if command == "reload" {
            "/reload".into()
        } else {
            format!("j/{command}")
        };
        let output = run(&["--hypr", command], vec![(request, reply.into())]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    for args in [
        vec!["--hypr"],
        vec!["--hypr", "reload", "extra"],
        vec!["--hypr", "dispatch"],
    ] {
        let output = run(&args, vec![]);
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_omabeam"))
        .args(["--hypr", "monitors"])
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("HYPRLAND_INSTANCE_SIGNATURE"));
}

#[test]
fn hides_the_picker_using_snapshots_and_lua_over_ipc() {
    let snapshot = vec![
        ("j/clients".into(), r#"[{"address":"0xabc","class":"omabeam","at":[10,20],"size":[980,680],"workspace":{"id":1,"name":"1"},"monitor":0,"floating":true}]"#.into()),
        ("j/monitors".into(), "[]".into()),
        ("j/workspaces".into(), r#"[{"id":1,"name":"1"}]"#.into()),
        ("j/activeworkspace".into(), r#"{"id":1,"name":"1"}"#.into()),
    ];
    let mut replies = snapshot.clone();
    for expression in [
        r#"hl.dsp.window.set_prop({ prop = "no_screen_share", value = "0", window = "address:0xabc" })"#,
        r#"hl.dsp.window.resize({ x = 1, y = 1, relative = false, window = "address:0xabc" })"#,
        r#"hl.dsp.window.move({ x = -5000, y = -5000, relative = false, window = "address:0xabc" })"#,
        r#"hl.dsp.window.move({ workspace = "special:omabeam", follow = false, window = "address:0xabc" })"#,
    ] {
        replies.push((format!("/dispatch {expression}"), "ok".into()));
    }
    replies.extend(snapshot);
    let output = run(&["--hide"], replies);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}
