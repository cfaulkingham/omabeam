//! Hyprland's socket1 protocol: `[flags]/command args`, with the reply ending
//! when the compositor closes the connection. Each request uses a new socket.
//! Protocol: https://wiki.hypr.land/IPC/
use anyhow::{Context, Result, bail, ensure};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// All compositor IPC allowed once shutdown starts, and again for recovery.
/// `--stop` sends SIGKILL 10 s after SIGTERM; joining capture and removing
/// the extended display must finish first. If time runs out before the
/// display is gone, display.json stays for a later `--stop`.
pub(crate) const TEARDOWN_BUDGET: Duration = Duration::from_secs(6);
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

/// Set by the first stop signal; later ones never extend it.
static TEARDOWN: OnceLock<Instant> = OnceLock::new();

/// Every later request, from any thread, ends within `budget` from now: each
/// gets the smaller of its usual timeout and what is left.
pub(crate) fn arm_teardown(budget: Duration) {
    if let Some(deadline) = Instant::now().checked_add(budget) {
        let _ = TEARDOWN.set(deadline);
    }
}

fn teardown_deadline() -> Option<Instant> {
    TEARDOWN.get().copied()
}

pub(super) struct Ipc {
    path: PathBuf,
    /// Shared by every request: recovery's total budget.
    deadline: Option<Instant>,
}

impl Ipc {
    pub(super) fn from_env() -> Result<Self> {
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
        let runtime = std::env::var_os("XDG_RUNTIME_DIR");
        Ok(Self {
            path: socket_path(
                signature.as_deref(),
                runtime.as_deref(),
                rustix::process::getuid().as_raw(),
            )?,
            deadline: None,
        })
    }

    /// Recovery (from `--stop` or a new share) shares one budget across all
    /// of its requests, so a slow compositor cannot hold it 5 s per request.
    pub(super) fn for_instance(signature: &str) -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR");
        Ok(Self {
            path: socket_path(
                Some(OsStr::new(signature)),
                runtime.as_deref(),
                rustix::process::getuid().as_raw(),
            )?,
            deadline: Instant::now().checked_add(TEARDOWN_BUDGET),
        })
    }

    /// A socket file can outlive the compositor that crashed: unlike a clean
    /// exit, nothing unlinks it. Connecting distinguishes a live compositor
    /// from that stale file without paying a full request round trip.
    pub(super) fn is_listening(&self) -> Result<bool> {
        match UnixStream::connect(&self.path) {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "could not check whether Hyprland is listening at {}",
                    self.path.display()
                )
            }),
        }
    }

    pub(super) fn query(&self, command: &str) -> Result<String> {
        self.request(&format!("j/{command}"))
    }

    pub(super) fn command(&self, command: &str) -> Result<()> {
        let reply = self.request(&format!("/{command}"))?;
        ensure!(
            reply.trim() == "ok",
            "Hyprland {command} failed: {}",
            reply.trim()
        );
        Ok(())
    }

    fn request(&self, request: &str) -> Result<String> {
        self.request_before(teardown_deadline(), request)
    }

    /// `teardown` is the process's stop deadline; tests pass their own.
    fn request_before(&self, teardown: Option<Instant>, request: &str) -> Result<String> {
        let failed = || format!("Hyprland IPC request {request:?} failed");
        let timeout = self
            .timeout(teardown, Instant::now())
            .with_context(failed)?;
        let mut socket = UnixStream::connect(&self.path)
            .with_context(|| format!("could not connect to Hyprland at {}", self.path.display()))?;
        exchange(&mut socket, request, timeout).with_context(failed)
    }

    /// The usual timeout, cut to what is left of any budget. With nothing
    /// left, fail before connecting.
    fn timeout(&self, teardown: Option<Instant>, now: Instant) -> Result<Duration> {
        [self.deadline, teardown]
            .into_iter()
            .flatten()
            .try_fold(IO_TIMEOUT, |timeout, deadline| {
                let left = deadline.saturating_duration_since(now);
                ensure!(!left.is_zero(), "cleanup time limit reached");
                Ok(timeout.min(left))
            })
    }
}

fn socket_path(signature: Option<&OsStr>, runtime: Option<&OsStr>, uid: u32) -> Result<PathBuf> {
    let signature = signature.filter(|value| !value.is_empty()).context(
        "HYPRLAND_INSTANCE_SIGNATURE is not set; run OmaBeam inside your Hyprland session",
    )?;
    ensure!(
        signature != "."
            && signature != ".."
            && !signature.as_bytes().iter().any(|b| matches!(b, b'/' | 0)),
        "invalid HYPRLAND_INSTANCE_SIGNATURE"
    );
    let runtime = match runtime.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(format!("/run/user/{uid}")),
    };
    ensure!(
        runtime.is_absolute(),
        "XDG_RUNTIME_DIR must be an absolute path"
    );
    Ok(runtime
        .join("hypr")
        .join(Path::new(signature))
        .join(".socket.sock"))
}

fn exchange(socket: &mut UnixStream, request: &str, timeout: Duration) -> Result<String> {
    // Poll with an absolute deadline. Updating SO_RCVTIMEO between reads can
    // fail on macOS after the peer closes, even with reply bytes still queued.
    socket
        .set_nonblocking(true)
        .context("could not make Hyprland IPC socket nonblocking")?;
    let deadline = Instant::now() + timeout;
    let mut pending = request.as_bytes();
    while !pending.is_empty() {
        wait_ready(socket, PollFlags::OUT, deadline)?;
        match socket.write(pending) {
            Ok(0) => bail!("Hyprland IPC closed while writing request"),
            Ok(count) => pending = &pending[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("could not write request"),
        }
    }
    // No newline, NUL terminator, or write shutdown: these are unframed socket1
    // requests. A short read is not EOF; large replies may arrive in many parts.
    let mut reply = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        wait_ready(socket, PollFlags::IN, deadline)?;
        let count = match socket.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("could not read reply"),
        };
        ensure!(
            reply.len() + count <= MAX_REPLY_BYTES,
            "Hyprland IPC reply exceeds {MAX_REPLY_BYTES} bytes"
        );
        reply.extend_from_slice(&buffer[..count]);
    }
    let reply = String::from_utf8(reply).context("Hyprland IPC reply is not UTF-8")?;
    ensure!(
        !reply.trim().is_empty(),
        "Hyprland IPC returned an empty reply"
    );
    ensure!(
        !reply.trim_start().starts_with("error:"),
        "{}",
        reply.trim()
    );
    Ok(reply)
}

fn wait_ready(socket: &UnixStream, events: PollFlags, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Hyprland IPC request timed out");
        let mut fds = [PollFd::new(socket, events)];
        match poll(&mut fds, Some(&Timespec::try_from(remaining)?)) {
            Ok(0) => bail!("Hyprland IPC request timed out"),
            // HUP/ERR also wake poll: read/write reports EOF or the actual error.
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error).context("could not poll Hyprland IPC socket"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::Shutdown, os::unix::net::UnixListener, thread};

    impl Ipc {
        /// No budget of its own, like `from_env`.
        fn at(path: PathBuf) -> Self {
            Self {
                path,
                deadline: None,
            }
        }
    }

    #[test]
    fn resolves_the_session_socket_and_default_runtime() {
        let signature = Some(OsStr::new("commit_123_456"));
        assert_eq!(
            socket_path(signature, Some(OsStr::new("/run/custom")), 1000).unwrap(),
            Path::new("/run/custom/hypr/commit_123_456/.socket.sock")
        );
        for runtime in [None, Some(OsStr::new(""))] {
            assert_eq!(
                socket_path(signature, runtime, 1000).unwrap(),
                Path::new("/run/user/1000/hypr/commit_123_456/.socket.sock")
            );
        }
        for signature in [None, Some(OsStr::new(""))] {
            assert!(
                socket_path(signature, None, 1000)
                    .unwrap_err()
                    .to_string()
                    .contains("Hyprland session")
            );
        }
        for invalid in [".", "..", "/absolute", "../other", "a/b", "a\0b"] {
            assert!(socket_path(Some(OsStr::new(invalid)), None, 1000).is_err());
        }
        assert!(socket_path(signature, Some(OsStr::new("relative")), 1000).is_err());
    }

    #[test]
    fn is_listening_distinguishes_live_stale_and_missing_sockets() {
        // macOS's default TMPDIR is often too long for a Unix socket path
        // (sun_path is capped around 104 bytes); /tmp keeps it short, as the
        // `peer` helper below also relies on.
        let dir = tempfile::Builder::new()
            .prefix("ts-ipc-listen-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join(".socket.sock");

        let listener = UnixListener::bind(&path).unwrap();
        assert!(Ipc::at(path.clone()).is_listening().unwrap());
        drop(listener);

        // Rust's UnixListener does not unlink its socket file on drop, so the
        // path still exists but nothing is bound behind it: a crashed
        // compositor leaves exactly this behind.
        assert!(!Ipc::at(path.clone()).is_listening().unwrap());

        let missing = dir.path().join("missing.sock");
        assert!(!Ipc::at(missing).is_listening().unwrap());
    }

    // Real Unix listeners test framing and acknowledgements without changing
    // process-wide session variables or needing a compositor on the test host.
    fn peer(expected: &str, reply: &[u8]) -> (tempfile::TempDir, Ipc, thread::JoinHandle<()>) {
        let dir = tempfile::Builder::new()
            .prefix("ts-ipc-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let expected = expected.as_bytes().to_vec();
        let reply = reply.to_vec();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = vec![0; expected.len()];
            socket.read_exact(&mut request).unwrap();
            assert_eq!(request, expected);
            socket.write_all(&reply).unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            let mut trailing = Vec::new();
            socket.read_to_end(&mut trailing).unwrap();
            assert!(
                trailing.is_empty(),
                "unexpected request terminator: {trailing:?}"
            );
        });
        (dir, Ipc::at(path), server)
    }

    #[test]
    fn sends_json_queries_and_lua_dispatch_without_shell_quoting() {
        let (_dir, ipc, server) = peer("j/clients", b"[]");
        assert_eq!(ipc.query("clients").unwrap(), "[]");
        server.join().unwrap();

        let command = r#"dispatch hl.dsp.window.move({ workspace = "special:omabeam", follow = false, window = "address:0xabc" })"#;
        let (_dir, ipc, server) = peer(&format!("/{command}"), b"ok\n");
        ipc.command(command).unwrap();
        server.join().unwrap();

        let (_dir, ipc, server) = peer("/reload", b"ok");
        ipc.command("reload").unwrap();
        server.join().unwrap();
    }

    #[test]
    fn rejects_compositor_errors_and_invalid_replies() {
        for reply in [
            b"error: 2 Lua dispatch failed".as_slice(),
            b"unknown request",
            b"",
            b"\xff",
        ] {
            let (_dir, ipc, server) = peer("/reload", reply);
            assert!(ipc.command("reload").is_err());
            server.join().unwrap();
        }
        let (_dir, ipc, server) = peer("j/monitors", b"error: compositor unavailable");
        assert!(
            format!("{:#}", ipc.query("monitors").unwrap_err()).contains("compositor unavailable")
        );
        server.join().unwrap();
    }

    #[test]
    fn reads_large_fragmented_replies_until_eof() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let expected = format!(r#"[{{"title":"{}"}}]"#, "Tile 🦀 ".repeat(10_000));
        let bytes = expected.clone().into_bytes();
        let server = thread::spawn(move || {
            let mut request = [0; 9];
            server.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"j/clients");
            // The first write is deliberately smaller than the read buffer.
            server.write_all(&bytes[..13]).unwrap();
            thread::sleep(Duration::from_millis(20));
            for chunk in bytes[13..].chunks(4093) {
                server.write_all(chunk).unwrap();
            }
        });
        assert_eq!(
            exchange(&mut client, "j/clients", IO_TIMEOUT).unwrap(),
            expected
        );
        server.join().unwrap();
    }

    #[test]
    fn bounds_reply_size() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let mut request = [0; 9];
            server.read_exact(&mut request).unwrap();
            let _ = server.write_all(&vec![b'x'; MAX_REPLY_BYTES + 1]);
        });
        let error = exchange(&mut client, "j/clients", IO_TIMEOUT).unwrap_err();
        drop(client);
        server.join().unwrap();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn times_out_instead_of_accepting_an_unfinished_reply() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.write_all(b"[").unwrap();
        let start = Instant::now();
        let error = exchange(&mut client, "j/clients", Duration::from_millis(40)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// Accepts and reads a request, then never answers: a hung compositor.
    fn stalled_peer() -> (tempfile::TempDir, Ipc, thread::JoinHandle<()>) {
        let dir = tempfile::Builder::new()
            .prefix("ts-ipc-stall-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            // Ends when the client gives up and closes its end.
            let mut request = Vec::new();
            socket.read_to_end(&mut request).unwrap();
            assert_eq!(request, b"j/monitors");
        });
        (dir, Ipc::at(path), server)
    }

    #[test]
    fn an_armed_budget_ends_a_stalled_request_within_it() {
        // The stop signal's teardown budget, then recovery's own budget.
        for recovery in [false, true] {
            let (_dir, mut ipc, server) = stalled_peer();
            let start = Instant::now();
            let deadline = start + Duration::from_millis(300);
            let result = if recovery {
                ipc.deadline = Some(deadline);
                ipc.query("monitors")
            } else {
                ipc.request_before(Some(deadline), "j/monitors")
            };
            let elapsed = start.elapsed();
            server.join().unwrap();
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains("timed out"), "{error}");
            assert!(
                (Duration::from_millis(250)..Duration::from_secs(2)).contains(&elapsed),
                "recovery {recovery}: took {elapsed:?}, not the 300 ms budget"
            );
        }
    }

    #[test]
    fn a_spent_budget_fails_before_connecting() {
        let dir = tempfile::Builder::new()
            .prefix("ts-ipc-spent-")
            .tempdir_in("/tmp")
            .unwrap();
        // Nothing listens here, so connecting would fail with another error.
        let mut ipc = Ipc::at(dir.path().join("socket"));
        let spent = Instant::now();
        for (own, teardown) in [(Some(spent), None), (None, Some(spent))] {
            ipc.deadline = own;
            let error = ipc.request_before(teardown, "j/monitors").unwrap_err();
            assert!(format!("{error:#}").contains("time limit"), "{error:#}");
        }
    }

    #[test]
    fn unarmed_requests_keep_the_normal_timeout() {
        let now = Instant::now();
        let mut ipc = Ipc::at(PathBuf::from("/unused"));
        assert_eq!(IO_TIMEOUT, Duration::from_secs(5));
        assert_eq!(ipc.timeout(None, now).unwrap(), IO_TIMEOUT);
        // Armed: the smaller of the usual timeout and what is left.
        let far = now + Duration::from_secs(60);
        let near = now + Duration::from_millis(300);
        assert_eq!(ipc.timeout(Some(far), now).unwrap(), IO_TIMEOUT);
        let short = Duration::from_millis(300);
        assert_eq!(ipc.timeout(Some(near), now).unwrap(), short);
        ipc.deadline = Some(far);
        assert_eq!(ipc.timeout(Some(near), now).unwrap(), short);
        ipc.deadline = Some(near);
        assert_eq!(ipc.timeout(Some(far), now).unwrap(), short);
    }

    #[test]
    fn recovery_has_its_own_total_budget() {
        let before = Instant::now();
        let ipc = Ipc::for_instance("commit_123_456").unwrap();
        let deadline = ipc.deadline.expect("recovery bounds all of its requests");
        assert!(deadline >= before + TEARDOWN_BUDGET);
        assert!(deadline <= Instant::now() + TEARDOWN_BUDGET);
    }
}
