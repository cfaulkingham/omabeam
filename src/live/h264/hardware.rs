use anyhow::{Context, Result, ensure};
use omabeam_encoder::{Config, MAX_HEADER, MAX_PACKET, Reply, inspect_h264, write_json};
use openh264::formats::{YUVBuffer, YUVSource};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::AsFd,
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
};
use std::{
    io::{ErrorKind, IoSlice, Read, Write},
    path::Path,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_TIMEOUT: Duration = Duration::from_millis(750);
/// How long a failure waits for the helper's stderr to reach EOF. A driver
/// process that outlives the helper can hold it open indefinitely.
const LOG_GRACE: Duration = Duration::from_millis(200);
/// A 1080p I420 frame is about 3 MiB: 1 MiB pipes carry it in a few round
/// trips instead of dozens at the 64 KiB default.
#[cfg(target_os = "linux")]
const PIPE_SIZE: usize = 1 << 20;

pub(super) struct Hardware {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    log: Log,
    // Reply buffers, reused across frames.
    header: Vec<u8>,
    packet: Vec<u8>,
    first: bool,
    pub dimensions: (usize, usize),
    pub name: String,
}
impl Hardware {
    pub fn new(config: &Config) -> Result<Self> {
        let exe = std::env::current_exe()?;
        Self::spawn(&exe.with_file_name("omabeam-encoder"), config)
    }
    pub(super) fn spawn(path: &Path, config: &Config) -> Result<Self> {
        config.frame_len()?;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("hardware encoder helper could not start")?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut helper = Self {
            child,
            input,
            output,
            log: Log::default(),
            header: Vec::with_capacity(MAX_HEADER),
            packet: Vec::new(),
            first: true,
            dimensions: (config.width as usize, config.height as usize),
            name: String::new(),
        };
        helper.log.start(stderr)?;
        for pipe in [helper.input.as_fd(), helper.output.as_fd()] {
            // Best effort: past the per-user pipe limit this fails with EPERM
            // and the pipe keeps its default size.
            #[cfg(target_os = "linux")]
            let _ = rustix::pipe::fcntl_setpipe_size(pipe, PIPE_SIZE);
            fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK)?;
        }
        let mut header = Vec::new();
        write_json(&mut header, config)?;
        write_all(
            &mut helper.input,
            &mut [IoSlice::new(&header)],
            Instant::now() + START_TIMEOUT,
        )?;
        Ok(helper)
    }

    pub fn encode(&mut self, yuv: &YUVBuffer, pts: i64, force: bool) -> Result<(Vec<u8>, bool)> {
        match self.exchange(yuv, pts, force) {
            // The caller keeps the packet; the read buffers stay here.
            Ok(idr) => Ok((self.packet.clone(), idr)),
            Err(error) => {
                // Take the helper's last words before describing the failure.
                // Once it is reaped, stderr reaches EOF after everything it
                // wrote, unless a surviving driver process still holds it.
                let _ = self.child.kill();
                let _ = self.child.wait();
                let detail = self.log.last_words();
                Err(error.context(if detail.is_empty() {
                    "hardware encoder stopped responding".into()
                } else {
                    detail
                }))
            }
        }
    }
    fn exchange(&mut self, yuv: &YUVBuffer, pts: i64, force: bool) -> Result<bool> {
        ensure!(
            yuv.dimensions() == self.dimensions,
            "hardware encoder size changed"
        );
        let deadline = Instant::now()
            + if self.first {
                START_TIMEOUT
            } else {
                FRAME_TIMEOUT
            };
        let request = [u8::from(force || self.first)];
        let pts = pts.to_le_bytes();
        write_all(
            &mut self.input,
            &mut [
                IoSlice::new(&request),
                IoSlice::new(&pts),
                IoSlice::new(yuv.y()),
                IoSlice::new(yuv.u()),
                IoSlice::new(yuv.v()),
            ],
            deadline,
        )?;
        let mut size = [0; 4];
        read_exact(&mut self.output, &mut size, deadline)?;
        let size = u32::from_le_bytes(size) as usize;
        ensure!(
            (1..=MAX_HEADER).contains(&size),
            "invalid hardware reply size"
        );
        self.header.resize(size, 0);
        read_exact(&mut self.output, &mut self.header, deadline)?;
        let reply: Reply = serde_json::from_slice(&self.header)?;
        ensure!(
            !reply.encoder.is_empty() && reply.encoder.len() <= 160,
            "invalid hardware encoder name"
        );
        ensure!(
            (1..=MAX_PACKET).contains(&reply.bytes),
            "invalid hardware packet size"
        );
        self.packet.resize(reply.bytes, 0);
        read_exact(&mut self.output, &mut self.packet, deadline)?;
        let idr = inspect_h264(&self.packet)?;
        ensure!(
            !(force || self.first) || idr,
            "hardware did not return a requested IDR"
        );
        self.first = false;
        self.name = reply.encoder;
        Ok(idr)
    }
}
impl Drop for Hardware {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The bounded tail of the helper's stderr. A thread drains it continuously,
/// so a verbose driver can neither block the helper nor grow this process.
#[derive(Default)]
struct Log {
    tail: Arc<Mutex<Vec<u8>>>,
    reader: Option<(JoinHandle<()>, Receiver<()>)>,
}
impl Log {
    fn start(&mut self, mut stderr: ChildStderr) -> std::io::Result<()> {
        let tail = self.tail.clone();
        // Nothing is sent: the reader drops `finished` as it returns.
        let (finished, done) = mpsc::channel::<()>();
        let reader = thread::Builder::new()
            .name("omabeam-encoder-log".into())
            .spawn(move || {
                let _finished = finished;
                let mut buffer = [0; 1024];
                while let Ok(count) = stderr.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    let mut tail = tail.lock().unwrap();
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > 4096 {
                        let keep = tail.len() - 4096;
                        tail.drain(..keep);
                    }
                }
            })?;
        self.reader = Some((reader, done));
        Ok(())
    }

    /// The last 400 characters, once the reader has drained stderr or
    /// `LOG_GRACE` has passed. The helper must already have exited.
    fn last_words(&mut self) -> String {
        if let Some((reader, done)) = self.reader.take()
            && done.recv_timeout(LOG_GRACE) == Err(RecvTimeoutError::Disconnected)
        {
            let _ = reader.join();
        }
        let tail = self.tail.lock().unwrap();
        let text = String::from_utf8_lossy(&tail);
        let start = text.char_indices().rev().nth(399).map_or(0, |(at, _)| at);
        text[start..].trim().to_owned()
    }
}

fn ready(fd: &impl AsFd, flags: PollFlags, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("hardware encoder timed out")?;
        let timeout = Timespec::try_from(remaining)?;
        let mut fds = [PollFd::new(fd, flags)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => anyhow::bail!("hardware encoder timed out"),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}
/// Write every slice. Writes are tried first; a short write means the pipe
/// is full, so it waits for room rather than retrying into EAGAIN.
fn write_all(
    fd: &mut (impl Write + AsFd),
    mut slices: &mut [IoSlice<'_>],
    deadline: Instant,
) -> Result<()> {
    while !slices.is_empty() {
        match fd.write_vectored(slices) {
            Ok(0) => anyhow::bail!("hardware encoder closed its input"),
            Ok(n) => {
                IoSlice::advance_slices(&mut slices, n);
                if !slices.is_empty() {
                    ready(fd, PollFlags::OUT, deadline)?;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => ready(fd, PollFlags::OUT, deadline)?,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
/// Fill `bytes`. Reads are tried first; a short read means the pipe is
/// empty, so it waits for data rather than retrying into EAGAIN.
fn read_exact(fd: &mut (impl Read + AsFd), mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        match fd.read(bytes) {
            Ok(0) => anyhow::bail!("hardware encoder exited"),
            Ok(n) => {
                bytes = &mut bytes[n..];
                if !bytes.is_empty() {
                    ready(fd, PollFlags::IN, deadline)?;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => ready(fd, PollFlags::IN, deadline)?,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveConfig, h264::AdaptiveEncoder};
    use std::os::unix::fs::PermissionsExt;

    /// Python stand-ins for the helper share this protocol prologue.
    const PROLOGUE: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, struct, subprocess, sys, time
root = pathlib.Path(__file__).parent
source, target = sys.stdin.buffer, sys.stdout.buffer
config = json.loads(source.read(struct.unpack('<I', source.read(4))[0]))
length = 9 + config['width'] * config['height'] * 3 // 2
IDR = bytes([0, 0, 0, 1, 0x67, 66, 0xe0, 31, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1])
def reply(packet):
    header = json.dumps({'encoder': 'Fixture hardware', 'bytes': len(packet)}).encode()
    target.write(struct.pack('<I', len(header)) + header + packet)
    target.flush()
"#;

    fn helper(directory: &Path, body: &str) -> std::path::PathBuf {
        let path = directory.join("helper");
        std::fs::write(&path, format!("{PROLOGUE}{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn config(width: u32, height: u32) -> Config {
        Config {
            version: omabeam_encoder::VERSION,
            width,
            height,
            fps: 60,
            bitrate: 4_000_000,
            gop_frames: 0,
        }
    }

    #[test]
    fn a_1080p_frame_reaches_the_helper_intact_and_reply_buffers_are_reused() {
        let directory = tempfile::tempdir().unwrap();
        // Records each request and answers an IDR request with a packet
        // larger than a default pipe, anything else with a small delta.
        let path = helper(
            directory.path(),
            r#"
index = 0
while len(request := source.read(length)) == length:
    (root / f'frame-{index}').write_bytes(request)
    reply(IDR + b'\x01' * 300_000 if request[0] else bytes([0, 0, 1, 0x41]) + b'\x01' * 1_000)
    index += 1
"#,
        );
        let (width, height) = (1920, 1080);
        let mut hardware = Hardware::spawn(&path, &config(width as u32, height as u32)).unwrap();
        let mut buffers = None;
        for (index, (pts, force)) in [(0, false), (16_666, false), (33_333, true), (50_000, false)]
            .into_iter()
            .enumerate()
        {
            // Not periodic, so a reordered or misaligned plane shows.
            let pixels = (0..width * height * 3 / 2)
                .map(|i: usize| {
                    ((i.wrapping_mul(0x9e37_79b9) >> 16) as u8).wrapping_add(index as u8)
                })
                .collect();
            let frame = YUVBuffer::from_vec(pixels, width, height);
            let (packet, idr) = hardware.encode(&frame, pts, force).unwrap();
            // The first frame always asks for an IDR.
            let requested = force || index == 0;
            assert_eq!(
                (idr, packet.len()),
                if requested {
                    (true, 300_018)
                } else {
                    (false, 1_004)
                }
            );
            let mut sent = vec![u8::from(requested)];
            sent.extend(pts.to_le_bytes());
            for plane in [frame.y(), frame.u(), frame.v()] {
                sent.extend(plane);
            }
            let received = std::fs::read(directory.path().join(format!("frame-{index}"))).unwrap();
            assert!(received == sent, "frame {index} changed in transit");
            let current = (hardware.header.as_ptr(), hardware.packet.as_ptr());
            assert!(
                buffers.is_none_or(|previous| previous == current),
                "frame {index} reallocated a reply buffer"
            );
            buffers = Some(current);
        }
        assert_eq!(hardware.name, "Fixture hardware");
    }

    #[test]
    fn the_fallback_note_keeps_the_helpers_last_stderr_line() {
        let directory = tempfile::tempdir().unwrap();
        // A verbose driver: the reason comes last, after more than a pipe's
        // worth of log. The helper then exits without replying; closing
        // stdout first stands in for a Rust helper's near-instant exit
        // (Python's teardown would otherwise give the log reader time).
        let path = helper(
            directory.path(),
            r#"
source.read(length)
sys.stderr.write('driver chatter\n' * 5000 + 'MARKER: the encoder session was lost\n')
sys.stderr.flush()
os.close(1)
os._exit(1)
"#,
        );
        let frame = YUVBuffer::new(64, 64);
        let mut missed = Vec::new();
        for attempt in 0..50 {
            let mut encoder = AdaptiveEncoder::new(&LiveConfig::default()).unwrap();
            encoder.hardware = Some(Hardware::spawn(&path, &config(64, 64)).unwrap());
            encoder.attempted = true;
            // Software takes over the same frame with an IDR.
            assert!(encoder.encode(&frame, 0, true).unwrap().1);
            let note = encoder.note.take().unwrap_or_default();
            if !note.contains("MARKER: the encoder session was lost") {
                missed.push((attempt, note));
            }
        }
        assert!(
            missed.is_empty(),
            "{} of 50 notes missed the helper's last line, first: {:?}",
            missed.len(),
            missed.first()
        );
    }

    #[test]
    fn a_stalled_helper_fails_quickly_even_when_its_stderr_stays_open() {
        let directory = tempfile::tempdir().unwrap();
        // A driver process that outlives the helper keeps stderr open, so the
        // log reader never sees EOF.
        let path = helper(
            directory.path(),
            r#"
source.read(length)
reply(IDR)
source.read(length)
subprocess.Popen(['sleep', '5'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
sys.stderr.write('STALLED\n')
sys.stderr.flush()
time.sleep(30)
"#,
        );
        let frame = YUVBuffer::new(64, 64);
        let mut hardware = Hardware::spawn(&path, &config(64, 64)).unwrap();
        assert!(hardware.encode(&frame, 0, true).unwrap().1);
        let started = Instant::now();
        let error = format!("{:#}", hardware.encode(&frame, 16_666, false).unwrap_err());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
        assert!(
            error.contains("STALLED") && error.contains("timed out"),
            "{error}"
        );
    }
}
