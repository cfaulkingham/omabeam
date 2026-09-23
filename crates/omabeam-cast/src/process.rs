use crate::{VideoConfig, read_event, write_frame, write_json};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::Read,
    net::SocketAddr,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

pub struct Helper {
    child: Child,
    control: UnixStream,
    media: UnixStream,
    events: Receiver<Value>,
    failure: Arc<Mutex<Option<String>>>,
    log: Arc<Mutex<String>>,
    stopped: bool,
}
impl Helper {
    /// `certificate` is only for explicit software-receiver development tests.
    /// Ordinary sessions must pass None, preserving Google's trust roots.
    pub fn spawn(executable: &Path, certificate: Option<&Path>) -> Result<Self> {
        Self::spawn_timeout(executable, certificate, Duration::from_secs(5))
    }
    pub fn spawn_timeout(
        executable: &Path,
        certificate: Option<&Path>,
        ready_timeout: Duration,
    ) -> Result<Self> {
        let (control, child_control) = UnixStream::pair()?;
        let (media, child_media) = UnixStream::pair()?;
        control.set_write_timeout(Some(Duration::from_millis(250)))?;
        media.set_write_timeout(Some(Duration::from_millis(250)))?;
        let media_fd = child_media.as_raw_fd();
        let mut cmd = Command::new(executable);
        cmd.args(["--media-fd", "3"])
            .stdin(Stdio::from(OwnedFd::from(child_control)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cert) = certificate {
            cmd.arg("--developer-certificate").arg(cert);
        }
        // SAFETY: only async-signal-safe fd operations between fork and exec.
        // The parent keeps child_media alive until spawn has completed.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(media_fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Start Cast helper {}", executable.display()))?;
        drop(child_media);
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let failure = Arc::new(Mutex::new(None));
        let reader_failure = failure.clone();
        let (tx, events) = mpsc::sync_channel(256);
        thread::spawn(move || {
            loop {
                match read_event(&mut stdout) {
                    Ok(event) => {
                        if tx.try_send(event).is_err() {
                            *reader_failure.lock().unwrap() =
                                Some("Cast event queue overflow/closed".into());
                            break;
                        }
                    }
                    Err(error) => {
                        *reader_failure.lock().unwrap() = Some(format!("{error:#}"));
                        break;
                    }
                }
            }
        });
        let log = Arc::new(Mutex::new(String::new()));
        let reader_log = log.clone();
        thread::spawn(move || {
            let mut buffer = [0; 2048];
            while let Ok(n) = stderr.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let mut log = reader_log.lock().unwrap();
                log.push_str(&String::from_utf8_lossy(&buffer[..n]));
                if log.len() > 8192 {
                    let mut cut = log.len() - 8192;
                    while !log.is_char_boundary(cut) {
                        cut += 1;
                    }
                    log.drain(..cut);
                }
            }
        });
        let mut helper = Self {
            child,
            control,
            media,
            events,
            failure,
            log,
            stopped: false,
        };
        let event = helper
            .event(ready_timeout.min(Duration::from_secs(5)))?
            .context("Cast helper readiness timeout")?;
        ensure!(
            event["event"] == "ready",
            "Cast helper did not report ready"
        );
        Ok(helper)
    }
    pub fn discover(&mut self) -> Result<()> {
        write_json(
            &mut self.control,
            &serde_json::json!({"version":1,"command":"discover"}),
        )
    }
    pub fn connect(&mut self, endpoint: SocketAddr, video: &VideoConfig) -> Result<()> {
        write_json(&mut self.control, &video.connect(endpoint)?)
    }
    /// Resume only an existing, identical receiver-side session. The helper
    /// queries receiver status and never launches an app for this operation.
    pub fn resume(
        &mut self,
        endpoint: SocketAddr,
        video: &VideoConfig,
        session: &str,
    ) -> Result<()> {
        ensure!(
            !session.is_empty() && session.len() <= 256,
            "Invalid receiver session identity"
        );
        let mut command = video.connect(endpoint)?;
        command["resume_session"] = session.into();
        write_json(&mut self.control, &command)
    }
    pub fn frame(&mut self, sequence: u64, pts_us: u64, age_us: u64, bytes: &[u8]) -> Result<()> {
        if let Err(error) = write_frame(&mut self.media, sequence, pts_us, age_us, bytes) {
            // A partial packet cannot be resumed on this pipe. A fresh helper
            // may recover the same receiver session after an I/O failure.
            if error.downcast_ref::<std::io::Error>().is_some() {
                self.abandon();
            } else {
                let _ = self.stop();
            }
            return Err(error.context("Send Cast frame"));
        }
        Ok(())
    }
    pub fn event(&mut self, timeout: Duration) -> Result<Option<Value>> {
        let received = if timeout.is_zero() {
            match self.events.try_recv() {
                Ok(event) => Ok(event),
                Err(TryRecvError::Empty) => Err(RecvTimeoutError::Timeout),
                Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
            }
        } else {
            self.events.recv_timeout(timeout)
        };
        match received {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!(
                "{}: {}",
                self.failure
                    .lock()
                    .unwrap()
                    .as_deref()
                    .unwrap_or("Cast helper exited"),
                self.log.lock().unwrap()
            ),
        }
    }
    pub fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        write_json(
            &mut self.control,
            &serde_json::json!({"version":1,"command":"stop"}),
        )
    }
    /// Discard a broken helper without sending a receiver STOP. Only use for
    /// transport/process recovery; normal user Stop must use `stop`/Drop.
    pub fn abandon(&mut self) {
        self.stopped = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.stop();
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
