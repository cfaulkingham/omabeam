use anyhow::Result;
use rustix::{
    fd::BorrowedFd,
    fs::{FileType, Mode, OFlags, fstat, open},
    stdio::{dup2_stderr, dup2_stdout, stderr, stdout},
};
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGTERM},
    flag,
    low_level::{register, unregister},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Handlers only set a flag (a hang-up also points stdout and stderr at
/// /dev/null unless they are regular files). Resource cleanup stays on the
/// session thread.
pub(super) struct SessionSignals {
    stop: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl SessionSignals {
    pub fn new() -> Result<Self> {
        let mut signals = Self {
            stop: Arc::new(AtomicBool::new(false)),
            ids: Vec::new(),
        };
        for signal in [SIGINT, SIGTERM] {
            signals
                .ids
                .push(flag::register(signal, signals.stop.clone())?);
        }
        // Closing the terminal of `omabeam --live ...` hangs it up, unless it
        // runs under nohup, which asks to keep sharing.
        if !hangup_ignored() {
            let stop = signals.stop.clone();
            let hung_up = AtomicBool::new(false);
            // SAFETY: the action uses only atomics and async-signal-safe calls
            // (fstat, open, dup2, close) that do not allocate.
            let id = unsafe {
                register(SIGHUP, move || {
                    // Writes to the hung-up terminal will fail, so any
                    // eprintln! would panic halfway through cleanup.
                    if !hung_up.swap(true, Ordering::SeqCst) {
                        silence_stdio();
                    }
                    stop.store(true, Ordering::SeqCst);
                })
            }?;
            signals.ids.push(id);
        }
        Ok(signals)
    }

    /// The first stop also bounds all later Hyprland IPC, so joining capture
    /// and removing an extended display fit in `--stop`'s grace period.
    pub fn stopped(&self) -> bool {
        let stopped = self.stop.load(Ordering::SeqCst);
        if stopped {
            crate::hypr::arm_teardown(crate::hypr::TEARDOWN_BUDGET);
        }
        stopped
    }
}

impl Drop for SessionSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            unregister(id);
        }
    }
}

/// Writes to a regular file (live.log, `> file 2>&1`) outlive a hang-up. A
/// terminal, pipe or socket fails them afterwards: EIO, or EPIPE once the
/// reader is gone, since SIGPIPE is ignored.
fn survives_hang_up(fd: BorrowedFd<'_>) -> bool {
    fstat(fd).is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode).is_file())
}

/// Runs in a signal handler: a C string path needs no allocation.
fn silence_stdio() {
    let (out, err) = (!survives_hang_up(stdout()), !survives_hang_up(stderr()));
    if !(out || err) {
        return;
    }
    if let Ok(null) = open(
        c"/dev/null",
        OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        if out {
            let _ = dup2_stdout(&null);
        }
        if err {
            let _ = dup2_stderr(&null);
        }
    }
}

/// Whether we started with SIGHUP ignored, as under `nohup`. Without libc
/// only Linux reports this (/proc); elsewhere a hang-up always stops.
fn hangup_ignored() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|status| ignores_hangup(&status))
}

/// `SigIgn` is a hex mask of ignored signals; signal N is bit N - 1.
fn ignores_hangup(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & (1 << (SIGHUP - 1)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::{fd::AsFd, unix::net::UnixStream};

    // Signal delivery is tested end to end by tests/smoke.py: in this binary
    // a spawned child would briefly hold other tests' sockets open.

    #[test]
    fn only_a_regular_file_keeps_its_output_after_a_hang_up() {
        // live.log, or `omabeam --live ... > file 2>&1`.
        let file = tempfile::tempfile().unwrap();
        assert!(survives_hang_up(file.as_fd()));
        let (reader, writer) = std::io::pipe().unwrap();
        assert!(!survives_hang_up(reader.as_fd()));
        assert!(!survives_hang_up(writer.as_fd()));
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert!(!survives_hang_up(socket.as_fd()));
        // Character devices, like a terminal; /dev/null is harmless to replace.
        let null = std::fs::File::options()
            .write(true)
            .open("/dev/null")
            .unwrap();
        assert!(!survives_hang_up(null.as_fd()));
    }

    #[test]
    fn reads_nohup_from_the_ignored_signal_mask() {
        let status = |mask: &str| {
            format!(
                "Name:\tomabeam\nSigBlk:\t0000000000000000\nSigIgn:\t{mask}\nSigCgt:\t0000000000004a02\n"
            )
        };
        // nohup's SIGHUP (bit 0) alongside Rust's ignored SIGPIPE (bit 12).
        assert!(ignores_hangup(&status("0000000000001001")));
        assert!(!ignores_hangup(&status("0000000000001000")));
        assert!(!ignores_hangup(&status("not hex")));
        assert!(!ignores_hangup("Name:\tomabeam\n"));
    }
}
