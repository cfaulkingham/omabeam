use anyhow::{Context, Result, ensure};
use omabeam_encoder::{Config, MAX_HEADER, MAX_PACKET, Reply, inspect_h264, write_json};
use openh264::formats::{YUVBuffer, YUVSource};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::AsFd,
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_TIMEOUT: Duration = Duration::from_millis(750);

pub(super) struct Hardware {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    errors: Arc<Mutex<Vec<u8>>>,
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
        let mut stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let captured = errors.clone();
        let mut helper = Self {
            child,
            input,
            output,
            errors,
            first: true,
            dimensions: (config.width as usize, config.height as usize),
            name: String::new(),
        };
        // Continuously drain driver output, retaining only a bounded tail. A
        // verbose driver cannot block the encoder or grow the host's memory.
        thread::Builder::new()
            .name("omabeam-encoder-log".into())
            .spawn(move || {
                let mut buffer = [0; 1024];
                while let Ok(count) = stderr.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    let mut tail = captured.lock().unwrap();
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > 4096 {
                        let keep = tail.len() - 4096;
                        tail.drain(..keep);
                    }
                }
            })?;
        fcntl_setfl(
            &helper.input,
            fcntl_getfl(&helper.input)? | OFlags::NONBLOCK,
        )?;
        fcntl_setfl(
            &helper.output,
            fcntl_getfl(&helper.output)? | OFlags::NONBLOCK,
        )?;
        let mut header = Vec::new();
        write_json(&mut header, config)?;
        write(&mut helper.input, &header, Instant::now() + START_TIMEOUT)?;
        Ok(helper)
    }

    pub fn encode(&mut self, yuv: &YUVBuffer, pts: i64, force: bool) -> Result<(Vec<u8>, bool)> {
        let result = self.exchange(yuv, pts, force);
        result.map_err(|error| {
            let detail = String::from_utf8_lossy(&self.errors.lock().unwrap())
                .chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            error.context(if detail.is_empty() {
                "hardware encoder stopped responding".into()
            } else {
                detail.trim().to_owned()
            })
        })
    }
    fn exchange(&mut self, yuv: &YUVBuffer, pts: i64, force: bool) -> Result<(Vec<u8>, bool)> {
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
        write(&mut self.input, &[u8::from(force || self.first)], deadline)?;
        write(&mut self.input, &pts.to_le_bytes(), deadline)?;
        for plane in [yuv.y(), yuv.u(), yuv.v()] {
            write(&mut self.input, plane, deadline)?;
        }
        let mut size = [0; 4];
        read(&mut self.output, &mut size, deadline)?;
        let size = u32::from_le_bytes(size) as usize;
        ensure!(
            (1..=MAX_HEADER).contains(&size),
            "invalid hardware reply size"
        );
        let mut header = vec![0; size];
        read(&mut self.output, &mut header, deadline)?;
        let reply: Reply = serde_json::from_slice(&header)?;
        ensure!(
            !reply.encoder.is_empty() && reply.encoder.len() <= 160,
            "invalid hardware encoder name"
        );
        ensure!(
            (1..=MAX_PACKET).contains(&reply.bytes),
            "invalid hardware packet size"
        );
        let mut bytes = vec![0; reply.bytes];
        read(&mut self.output, &mut bytes, deadline)?;
        let idr = inspect_h264(&bytes)?;
        ensure!(
            !(force || self.first) || idr,
            "hardware did not return a requested IDR"
        );
        self.first = false;
        self.name = reply.encoder;
        Ok((bytes, idr))
    }
}
impl Drop for Hardware {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
fn write(fd: &mut (impl Write + AsFd), mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        ready(fd, PollFlags::OUT, deadline)?;
        match fd.write(bytes) {
            Ok(0) => anyhow::bail!("hardware encoder closed its input"),
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn read(fd: &mut (impl Read + AsFd), mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        ready(fd, PollFlags::IN, deadline)?;
        match fd.read(bytes) {
            Ok(0) => anyhow::bail!("hardware encoder exited"),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
