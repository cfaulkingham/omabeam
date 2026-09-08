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
    time::{Duration, Instant},
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct Ipc {
    path: PathBuf,
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
        })
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
        let mut socket = UnixStream::connect(&self.path)
            .with_context(|| format!("could not connect to Hyprland at {}", self.path.display()))?;
        exchange(&mut socket, request, IO_TIMEOUT)
            .with_context(|| format!("Hyprland IPC request {request:?} failed"))
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
        (dir, Ipc { path }, server)
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
}
