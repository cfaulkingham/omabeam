//! Descriptor-bound live-session files under `$XDG_RUNTIME_DIR/omabeam`.
//!
//! The share URL in `live.json` is a capability secret. Missing
//! `XDG_RUNTIME_DIR` fails closed; the plugin never falls back to `/tmp`.
use super::LiveStatus;
use anyhow::{Context, Result, bail, ensure};
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, fchmod, fstat, fsync, ftruncate, mkdirat, openat,
    renameat, unlinkat,
};
use rustix::io::{Errno, write};
use rustix::process::geteuid;
use std::io::Write as _;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, io};

pub const MAX_STATUS_BYTES: usize = 8192;
pub const MAX_TITLE_BYTES: usize = 200;
pub const MAX_ERROR_BYTES: usize = 400;
pub const MAX_URL_BYTES: usize = 256;
pub const MAX_SOURCE_BYTES: usize = 200;
const LEAF: &str = "omabeam";
const STATUS_NAME: &str = "live.json";
const LOG_NAME: &str = "live.log";
const LOCK_NAME: &str = "session.lock";
/// Unchanged fields are rewritten at most this often; the bar polls every 1-2 s.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(10);

pub fn status_dir() -> Result<PathBuf> {
    Ok(runtime_dir()?.join(LEAF))
}

pub fn status_path() -> Result<PathBuf> {
    Ok(status_dir()?.join(STATUS_NAME))
}

fn runtime_dir() -> Result<PathBuf> {
    let raw = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        bail!("XDG_RUNTIME_DIR is unset; refusing to store the share session in /tmp");
    };
    let path = PathBuf::from(raw);
    ensure!(
        path.is_absolute() && !path.as_os_str().as_encoded_bytes().contains(&0),
        "XDG_RUNTIME_DIR must be an absolute path"
    );
    Ok(path)
}

fn open_runtime_dir() -> Result<OwnedFd> {
    let path = runtime_dir()?;
    let fd = openat(
        CWD,
        path.as_os_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("cannot open {}", path.display()))?;
    let st = fstat(&fd)?;
    ensure!(
        FileType::from_raw_mode(st.st_mode).is_dir(),
        "XDG_RUNTIME_DIR is not a directory"
    );
    ensure!(
        st.st_uid == geteuid().as_raw(),
        "XDG_RUNTIME_DIR is not owned by this user"
    );
    Ok(fd)
}

fn open_omabeam_dir() -> Result<OwnedFd> {
    let parent = open_runtime_dir()?;
    let fd = match openat(
        &parent,
        LEAF,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => {
            match mkdirat(&parent, LEAF, Mode::from_raw_mode(0o700)) {
                Ok(()) => {}
                Err(Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
            openat(
                &parent,
                LEAF,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
        }
        Err(error) => return Err(error.into()),
    };
    let st = fstat(&fd)?;
    ensure!(
        FileType::from_raw_mode(st.st_mode).is_dir(),
        "refusing omabeam runtime path that is not a directory"
    );
    ensure!(
        st.st_uid == geteuid().as_raw(),
        "refusing omabeam runtime directory owned by another user"
    );
    if st.st_mode & 0o077 != 0 {
        fchmod(&fd, Mode::from_raw_mode(0o700))?;
    }
    repair_leaf(&fd)?;
    Ok(fd)
}

fn repair_leaf(dirfd: &OwnedFd) -> Result<()> {
    let listing = format!("/proc/self/fd/{}", dirfd.as_raw_fd());
    let entries = match fs::read_dir(&listing) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "." || name == ".." {
            continue;
        }
        if name.contains('/') || name == "." || name == ".." {
            continue;
        }
        match openat(
            dirfd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let Ok(st) = fstat(&fd) else { continue };
                drop(fd);
                if FileType::from_raw_mode(st.st_mode).is_dir() {
                    let _ = unlinkat(dirfd, name, AtFlags::REMOVEDIR);
                    continue;
                }
                if !FileType::from_raw_mode(st.st_mode).is_file()
                    || st.st_uid != geteuid().as_raw()
                    || st.st_nlink != 1
                {
                    let _ = unlinkat(dirfd, name, AtFlags::empty());
                    continue;
                }
                if let Ok(file) = openat(
                    dirfd,
                    name,
                    OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    let _ = fchmod(&file, Mode::from_raw_mode(0o600));
                }
            }
            Err(Errno::LOOP) | Err(Errno::NOENT) => {
                let _ = unlinkat(dirfd, name, AtFlags::empty());
            }
            Err(_) => {}
        }
    }
    Ok(())
}

pub fn cap_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if index + ch.len_utf8() > max {
            break;
        }
        end = index + ch.len_utf8();
    }
    text[..end].to_string()
}

pub fn bound_status(mut status: LiveStatus) -> LiveStatus {
    status.title = cap_bytes(&status.title, MAX_TITLE_BYTES);
    status.url = cap_bytes(&status.url, MAX_URL_BYTES);
    status.stats.source = cap_bytes(&status.stats.source, MAX_SOURCE_BYTES);
    if let Some(error) = status.stats.error.take() {
        status.stats.error = Some(cap_bytes(&error, MAX_ERROR_BYTES));
    }
    status
}

fn validate_decoded(status: &LiveStatus, raw_len: usize) -> Result<()> {
    ensure!(
        raw_len <= MAX_STATUS_BYTES,
        "session file exceeds the byte limit"
    );
    ensure!(
        status.title.len() <= MAX_TITLE_BYTES
            && status.url.len() <= MAX_URL_BYTES
            && status.stats.source.len() <= MAX_SOURCE_BYTES
            && status
                .stats
                .error
                .as_ref()
                .is_none_or(|error| error.len() <= MAX_ERROR_BYTES),
        "session fields exceed their limits"
    );
    Ok(())
}

fn write_all_fd(fd: impl AsFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match write(&fd, data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(Errno::INTR) => continue,
            Err(error) => return Err(io::Error::from_raw_os_error(error.raw_os_error())),
        }
    }
    Ok(())
}

fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 8];
    fs::File::open("/dev/urandom")?.read_exact_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

trait ReadExactBytes {
    fn read_exact_bytes(&mut self, buf: &mut [u8]) -> io::Result<()>;
}
impl ReadExactBytes for fs::File {
    fn read_exact_bytes(&mut self, buf: &mut [u8]) -> io::Result<()> {
        io::Read::read_exact(self, buf)
    }
}

pub fn write_live_status(status: &LiveStatus) -> Result<()> {
    let status = bound_status(status.clone());
    let payload = serde_json::to_vec(&status)?;
    write_record(STATUS_NAME, &payload)
}

pub(crate) fn write_display_state(payload: &[u8]) -> Result<()> {
    write_record("display.json", payload)
}

pub(crate) fn read_display_state() -> Result<Option<Vec<u8>>> {
    read_record("display.json")
}

pub(crate) fn clear_display_state() {
    clear_record("display.json");
}

/// Keep the inode in place: unlinking a flock file permits concurrent owners.
pub(crate) fn session_lock() -> Result<fs::File> {
    let dirfd = open_omabeam_dir()?;
    let fd = openat(
        &dirfd,
        LOCK_NAME,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let st = fstat(&fd)?;
    ensure!(
        FileType::from_raw_mode(st.st_mode).is_file()
            && st.st_uid == geteuid().as_raw()
            && st.st_nlink == 1,
        "refusing an unsafe session lock"
    );
    rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("a share is already running or starting; stop it before starting another")?;
    Ok(into_std_file(fd))
}

/// Share processes also record "pid starttime" in the lock they hold, so
/// `--stop` can reach one that is still starting and has no live.json yet.
pub(crate) fn session_lock_owned() -> Result<fs::File> {
    let lock = session_lock()?;
    let record = format!("{} {}\n", std::process::id(), self_starttime());
    ftruncate(&lock, 0).context("cannot record the session owner")?;
    write_all_fd(&lock, record.as_bytes()).context("cannot record the session owner")?;
    Ok(lock)
}

/// The share recorded in the session lock, only while that lock is held.
fn lock_owner() -> Option<(u32, u64)> {
    let dirfd = open_omabeam_dir().ok()?;
    let fd = openat(
        &dirfd,
        LOCK_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let st = fstat(&fd).ok()?;
    if !FileType::from_raw_mode(st.st_mode).is_file()
        || st.st_uid != geteuid().as_raw()
        || st.st_nlink != 1
    {
        return None;
    }
    // A shared lock is refused only while someone holds the exclusive one.
    // When granted, it lasts only until `fd` closes at the end of this probe.
    match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Err(Errno::WOULDBLOCK) => {}
        _ => return None,
    }
    let mut record = [0u8; 64];
    let len = rustix::io::pread(&fd, &mut record, 0).ok()?;
    if len == record.len() {
        return None;
    }
    parse_owner(&record[..len])
}

/// Exactly what `session_lock_owned` writes: "pid starttime", both non-zero,
/// and at most one final newline. No signs, padding, or other whitespace.
fn parse_owner(record: &[u8]) -> Option<(u32, u64)> {
    let record = std::str::from_utf8(record).ok()?;
    let (pid, starttime) = record
        .strip_suffix('\n')
        .unwrap_or(record)
        .split_once(' ')?;
    // `str::parse` alone would also take a leading '+' or zeros.
    let decimal = |field: &str| {
        !field.is_empty()
            && field.bytes().all(|b| b.is_ascii_digit())
            && !(field.len() > 1 && field.starts_with('0'))
    };
    if !(decimal(pid) && decimal(starttime)) {
        return None;
    }
    // A pid_t: `terminate` converts it back to i32.
    let pid: u32 = pid.parse().ok().filter(|&pid| i32::try_from(pid).is_ok())?;
    let starttime: u64 = starttime.parse().ok()?;
    (pid != 0 && starttime != 0).then_some((pid, starttime))
}

fn write_record(name: &str, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_STATUS_BYTES {
        return Err(StatusTooLarge.into());
    }
    let dirfd = open_omabeam_dir()?;
    let tmp_name = format!(".live.{}.json.tmp", random_suffix()?);
    let fd = openat(
        &dirfd,
        tmp_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let write_result = (|| -> Result<()> {
        fchmod(&fd, Mode::from_raw_mode(0o600))?;
        write_all_fd(&fd, &payload)?;
        fsync(&fd)?;
        renameat(&dirfd, tmp_name.as_str(), &dirfd, name)?;
        fsync(&dirfd)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = unlinkat(&dirfd, tmp_name.as_str(), AtFlags::empty());
    }
    write_result
}

pub fn clear_live_status() {
    clear_record(STATUS_NAME);
}

fn clear_record(name: &str) {
    let Ok(dirfd) = open_omabeam_dir() else {
        return;
    };
    if let Ok(fd) = openat(
        &dirfd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        let Ok(st) = fstat(&fd) else { return };
        if FileType::from_raw_mode(st.st_mode).is_file() && st.st_uid == geteuid().as_raw() {
            let _ = unlinkat(&dirfd, name, AtFlags::empty());
        }
    }
}

fn read_status_raw() -> Result<Option<Vec<u8>>> {
    read_record(STATUS_NAME)
}

fn read_record(name: &str) -> Result<Option<Vec<u8>>> {
    let dirfd = open_omabeam_dir()?;
    let fd = match openat(
        &dirfd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let st = fstat(&fd)?;
    ensure!(
        FileType::from_raw_mode(st.st_mode).is_file(),
        "refusing session file that is not a regular file"
    );
    ensure!(
        st.st_uid == geteuid().as_raw(),
        "refusing session file owned by another user"
    );
    ensure!(st.st_nlink == 1, "refusing session file with extra links");
    ensure!(
        st.st_mode & 0o077 == 0,
        "refusing session file that is group- or world-accessible"
    );
    ensure!(
        st.st_size >= 0 && (st.st_size as u64) <= MAX_STATUS_BYTES as u64,
        "session file exceeds the byte limit"
    );
    let mut file = into_std_file(fd);
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        ensure!(
            data.len() <= MAX_STATUS_BYTES,
            "session file grew past the limit"
        );
    }
    Ok(Some(data))
}

pub fn read_status() -> Result<Option<LiveStatus>> {
    let Some(raw) = read_status_raw()? else {
        return Ok(None);
    };
    let status: LiveStatus =
        serde_json::from_slice(&raw).context("session file is not valid status JSON")?;
    validate_decoded(&status, raw.len())?;
    Ok(Some(status))
}

pub fn current_status() -> Option<LiveStatus> {
    read_status().ok().flatten().filter(status_alive)
}

pub fn latest_status() -> Option<LiveStatus> {
    latest_status_report().ok().flatten()
}

pub fn latest_status_report() -> Result<Option<LiveStatus>> {
    Ok(read_status()?.filter(|status| status.stats.state == "ended" || status_alive(status)))
}

/// True while `pid` is a process of this user. A share record also needs its
/// start time to match; see `status_alive`.
pub fn pid_alive(pid: u32) -> bool {
    process_identity(pid).is_some_and(|id| id.uid == geteuid().as_raw())
}

/// A record describes a running share only while its pid still has the
/// recorded start time: after a crash the pid can belong to any process.
pub fn status_alive(status: &LiveStatus) -> bool {
    same_process(status.starttime, process_identity(status.pid))
}

fn same_process(starttime: u64, id: Option<ProcessIdentity>) -> bool {
    starttime != 0 && id.is_some_and(|id| id.starttime == starttime && id.uid == geteuid().as_raw())
}

/// A status that can never fit the session file: a programming error, unlike
/// the transient failures `StatusWriter` retries.
#[derive(Debug)]
pub(crate) struct StatusTooLarge;

impl std::fmt::Display for StatusTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session payload exceeds the byte limit")
    }
}

impl std::error::Error for StatusTooLarge {}

/// True at most once per 10 s, so a lasting fault cannot flood live.log.
pub(super) fn log_due(last: &mut Option<Instant>, now: Instant) -> bool {
    let due = last.is_none_or(|at| now.saturating_duration_since(at) >= FAILURE_LOG_INTERVAL);
    if due {
        *last = Some(now);
    }
    due
}

/// Paces live.json rewrites. Each write repairs the directory, fsyncs twice,
/// and renames, so changing counters are written at most once a second, while
/// what the bar and launchers act on is written at once. A failed write (out
/// of descriptors or space, an I/O error) is logged and retried on the next
/// call rather than ending the share.
pub(crate) struct StatusWriter<W = fn(&LiveStatus) -> Result<()>> {
    write: W,
    last: Option<(LiveStatus, Instant)>,
    retry: bool,
    logged: Option<Instant>,
}

impl StatusWriter {
    pub fn new() -> Self {
        Self::with(write_live_status)
    }
}

impl<W: FnMut(&LiveStatus) -> Result<()>> StatusWriter<W> {
    pub fn with(write: W) -> Self {
        Self {
            write,
            last: None,
            retry: false,
            logged: None,
        }
    }

    pub fn write(&mut self, status: &LiveStatus) -> Result<()> {
        self.write_at(status, Instant::now())
    }

    /// First and final records and Cast negotiation steps never wait.
    pub fn force(&mut self, status: &LiveStatus) -> Result<()> {
        self.store(status, Instant::now())
    }

    pub fn write_at(&mut self, status: &LiveStatus, now: Instant) -> Result<()> {
        let due = self.retry
            || self.last.as_ref().is_none_or(|(last, at)| {
                now.saturating_duration_since(*at) >= STATUS_INTERVAL
                    || salient_change(last, status)
            });
        if due { self.store(status, now) } else { Ok(()) }
    }

    fn store(&mut self, status: &LiveStatus, now: Instant) -> Result<()> {
        match (self.write)(status) {
            Ok(()) => {
                self.last = Some((status.clone(), now));
                self.retry = false;
                Ok(())
            }
            Err(error) if error.is::<StatusTooLarge>() => Err(error),
            Err(error) => {
                self.retry = true;
                if log_due(&mut self.logged, now) {
                    // stderr is live.log, which can fail the same way: never panic.
                    let _ = writeln!(
                        io::stderr(),
                        "live share: could not update {STATUS_NAME}, retrying: {error:#}"
                    );
                }
                Ok(())
            }
        }
    }
}

/// What the bar shows, and what `spawn_daemon` for Cast waits on.
fn salient_change(old: &LiveStatus, new: &LiveStatus) -> bool {
    fn cast(status: &LiveStatus) -> Option<(&str, bool)> {
        status.stats.cast.as_ref().map(|cast| {
            let ready = cast.accepted_frames > 0 && cast.released_frames > 0;
            (cast.connection.as_str(), ready)
        })
    }
    old.url != new.url
        || old.title != new.title
        || old.stats.state != new.stats.state
        || old.stats.error != new.stats.error
        || old.stats.source != new.stats.source
        || old.stats.viewers != new.stats.viewers
        || old.stats.desktop != new.stats.desktop
        || cast(old) != cast(new)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub starttime: u64,
    pub uid: u32,
}

pub fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    if pid == 0 {
        return None;
    }
    let path = format!("/proc/{pid}");
    let uid = fs::metadata(&path).ok()?.uid();
    let data = fs::read(format!("/proc/{pid}/stat")).ok()?;
    if data.len() > 4096 {
        return None;
    }
    let rest = data.rsplit(|&b| b == b')').next()?;
    let fields: Vec<&[u8]> = rest
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|f| !f.is_empty())
        .collect();
    if fields
        .first()
        .is_some_and(|state| *state == b"Z" || *state == b"X")
    {
        return None;
    }
    let starttime = std::str::from_utf8(*fields.get(19)?).ok()?.parse().ok()?;
    Some(ProcessIdentity {
        pid,
        starttime,
        uid,
    })
}

pub fn self_starttime() -> u64 {
    process_identity(std::process::id())
        .map(|id| id.starttime)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn cmdline_is_session(pid: u32) -> bool {
    let Ok(data) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    data.len() <= 8192 && runs_a_session(&data)
}

/// Commands that record themselves in the session lock: shares, demos, Cast.
#[cfg(any(target_os = "linux", test))]
fn runs_a_session(cmdline: &[u8]) -> bool {
    cmdline.split(|b| *b == 0).any(|arg| {
        matches!(
            arg,
            b"--live" | b"--demo" | b"--cast" | b"--cast-test" | b"--cast-demo"
        )
    })
}

fn without_deleted_suffix(path: &std::path::Path) -> &std::path::Path {
    path.to_str()
        .and_then(|text| text.strip_suffix(" (deleted)"))
        .map(std::path::Path::new)
        .unwrap_or(path)
}

fn omabeam_exe_name(path: &std::path::Path) -> bool {
    without_deleted_suffix(path)
        .file_name()
        .is_some_and(|name| name == "omabeam")
}

#[cfg(target_os = "linux")]
fn exe_is_omabeam(pid: u32) -> bool {
    let Ok(path) = fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let named = omabeam_exe_name(&path);
    match std::env::current_exe() {
        Ok(me) => path == me || without_deleted_suffix(&path) == me.as_path() || named,
        Err(_) => named,
    }
}

#[cfg(target_os = "linux")]
fn matches_session(pid: u32, starttime: u64, id: ProcessIdentity) -> bool {
    id.pid == pid
        && same_process(starttime, Some(id))
        && exe_is_omabeam(pid)
        && cmdline_is_session(pid)
}

/// Stops the share named by live.json or, while one is still starting and
/// has published nothing, the owner recorded in the session lock. Returns
/// whether a share was signaled. The caller clears stale records once it
/// holds the session lock.
pub fn stop_live_process() -> bool {
    if let Ok(Some(status)) = read_status()
        && terminate(status.pid, status.starttime)
    {
        return true;
    }
    lock_owner().is_some_and(|(pid, starttime)| terminate(pid, starttime))
}

/// How long `--stop` waits for a signaled share to exit before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// What a stopping share needs besides Hyprland IPC: noticing the signal
/// (run_session checks every 250 ms) and joining its capture thread.
const STOP_JOIN_ALLOWANCE: Duration = Duration::from_secs(2);
// SIGKILL must not interrupt a share removing its extended display.
const _: () = assert!(
    crate::hypr::TEARDOWN_BUDGET.as_millis() + STOP_JOIN_ALLOWANCE.as_millis()
        < STOP_GRACE.as_millis()
);

/// Without /proc a recorded process cannot be verified, so it is never signaled.
#[cfg(not(target_os = "linux"))]
fn terminate(_pid: u32, _starttime: u64) -> bool {
    false
}

/// Signals `pid` only while it is still this user's omabeam share with the
/// recorded start time; an unreadable or mismatched record is never signaled.
#[cfg(target_os = "linux")]
fn terminate(pid: u32, starttime: u64) -> bool {
    let Some(expected) = process_identity(pid) else {
        return false;
    };
    if !matches_session(pid, starttime, expected) {
        return false;
    }
    let Some(raw) = rustix::process::Pid::from_raw(pid as i32) else {
        return false;
    };
    let Ok(pidfd) = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty()) else {
        return false;
    };
    let Some(again) = process_identity(pid) else {
        return false;
    };
    if again != expected || !matches_session(pid, starttime, again) {
        return false;
    }
    let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::TERM);
    // Capture and HTTP workers must stop before a virtual output is removed.
    // Allow the bounded compositor IPC cleanup to finish before forcing exit.
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && process_identity(pid).is_some_and(|id| id == expected) {
        thread::sleep(Duration::from_millis(50));
    }
    if process_identity(pid).is_some_and(|id| id == expected) {
        let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
    }
    true
}

pub fn open_live_log() -> Result<fs::File> {
    let dirfd = open_omabeam_dir()?;
    let fd = match openat(
        &dirfd,
        LOG_NAME,
        OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => openat(
            &dirfd,
            LOG_NAME,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?,
        Err(error) => return Err(error.into()),
    };
    let st = fstat(&fd)?;
    ensure!(
        FileType::from_raw_mode(st.st_mode).is_file()
            && st.st_uid == geteuid().as_raw()
            && st.st_nlink == 1,
        "refusing live.log that is not a private regular file"
    );
    fchmod(&fd, Mode::from_raw_mode(0o600))?;
    ftruncate(&fd, 0)?;
    Ok(into_std_file(fd))
}

fn into_std_file(fd: OwnedFd) -> fs::File {
    let raw = fd.into_raw_fd();
    unsafe { fs::File::from_raw_fd(raw) }
}

pub fn log_path() -> Result<PathBuf> {
    Ok(status_dir()?.join(LOG_NAME))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::live::{DesktopStats, StreamStats, cast::CastStats};
    use std::cell::Cell;
    use std::os::unix::fs::{FileExt, PermissionsExt};
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // A failed test must not fail every later one through a poisoned lock.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sample() -> LiveStatus {
        LiveStatus {
            pid: std::process::id(),
            starttime: self_starttime(),
            url: "http://127.0.0.1:9847/s/0123456789abcdef0123456789abcdef/".into(),
            title: "term".into(),
            stats: StreamStats {
                state: "live".into(),
                ..StreamStats::default()
            },
        }
    }

    /// Serializes tests that point XDG_RUNTIME_DIR at a temporary directory.
    pub(crate) fn with_runtime<T>(run: impl FnOnce(&Path) -> T) -> T {
        let _guard = env_lock();
        let dir = tempfile::TempDir::new().unwrap();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(dir.path())));
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// The session lock once nothing holds it. A child that another test is
    /// spawning shares our descriptors until it execs (on macOS that can take
    /// tens of milliseconds), so a lock just released may still be held.
    pub(crate) fn session_lock_when_free() -> fs::File {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match session_lock() {
                Ok(lock) => return lock,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("the session lock stayed held: {error:#}"),
            }
        }
    }

    #[test]
    fn refuses_missing_runtime_dir() {
        let _guard = env_lock();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        let error = runtime_dir().unwrap_err().to_string();
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) },
            None => {}
        }
        assert!(error.contains("XDG_RUNTIME_DIR"), "{error}");
    }

    #[test]
    fn write_replaces_a_symlink_instead_of_following_it() {
        with_runtime(|root| {
            let omabeam = root.join(LEAF);
            fs::create_dir_all(&omabeam).unwrap();
            fs::set_permissions(&omabeam, fs::Permissions::from_mode(0o700)).unwrap();
            let victim = root.join("victim");
            fs::write(&victim, "must survive").unwrap();
            std::os::unix::fs::symlink(&victim, omabeam.join(STATUS_NAME)).unwrap();
            write_live_status(&sample()).unwrap();
            assert_eq!(fs::read_to_string(&victim).unwrap(), "must survive");
            let status = read_status().unwrap().unwrap();
            assert_eq!(status.title, "term");
        });
    }

    #[test]
    fn rejects_an_oversized_session_file() {
        with_runtime(|root| {
            write_live_status(&sample()).unwrap();
            let path = root.join(LEAF).join(STATUS_NAME);
            fs::write(&path, vec![b'x'; MAX_STATUS_BYTES + 1]).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(read_status().is_err());
        });
    }

    #[test]
    fn treats_a_replaced_omabeam_binary_as_the_same_process() {
        assert!(omabeam_exe_name(std::path::Path::new(
            "/opt/plugin/native/bin/omabeam"
        )));
        assert!(omabeam_exe_name(std::path::Path::new(
            "/opt/plugin/native/bin/omabeam (deleted)"
        )));
        assert!(!omabeam_exe_name(std::path::Path::new("/usr/bin/sleep")));
        assert!(!omabeam_exe_name(std::path::Path::new(
            "/usr/bin/sleep (deleted)"
        )));
    }

    /// Runs a binary the test just copied. A child that another test thread
    /// forked during the copy holds its write descriptor until that child
    /// execs, and exec fails with ETXTBSY meanwhile.
    #[cfg(target_os = "linux")]
    fn spawn_copied(command: &mut std::process::Command) -> std::process::Child {
        for _ in 0..20 {
            match command.spawn() {
                Err(error) if error.raw_os_error() == Some(Errno::TXTBSY.raw_os_error()) => {
                    thread::sleep(Duration::from_millis(25));
                }
                spawned => return spawned.unwrap(),
            }
        }
        command.spawn().unwrap()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn exe_is_omabeam_after_the_file_is_unlinked() {
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .map(std::path::Path::new)
            .find(|path| path.is_file())
            .expect("sleep");
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("omabeam");
        fs::copy(sleep, &bin).unwrap();
        let mut child = spawn_copied(std::process::Command::new(&bin).arg("8"));
        let pid = child.id();
        fs::remove_file(&bin).unwrap();
        assert!(
            exe_is_omabeam(pid),
            "{:?}",
            fs::read_link(format!("/proc/{pid}/exe"))
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stop_refuses_a_foreign_pid() {
        with_runtime(|_| {
            let mut child = std::process::Command::new("/usr/bin/sleep")
                .arg("5")
                .spawn()
                .unwrap();
            let pid = child.id();
            let mut status = sample();
            status.pid = pid;
            status.starttime = process_identity(pid).unwrap().starttime;
            write_live_status(&status).unwrap();
            assert!(!stop_live_process());
            let _ = child.kill();
            let _ = child.wait();
        });
    }

    #[test]
    fn stop_recognizes_the_commands_that_record_themselves_in_the_lock() {
        let cmdline = |args: Vec<String>| -> Vec<u8> {
            let args = ["/opt/omabeam/omabeam".to_owned()].into_iter().chain(args);
            args.flat_map(|arg| arg.into_bytes().into_iter().chain([0]))
                .collect()
        };
        let config = crate::live::LiveConfig::default().to_cli_args();
        let source = crate::live::LiveSource::Output {
            name: "DP-1".into(),
        }
        .to_cli_args();
        let words = |args: &[&str]| args.iter().map(|&arg| arg.to_owned()).collect::<Vec<_>>();
        // As cast::spawn_daemon starts it, and the development Cast commands.
        let daemon = [config, words(&["--cast", "receiver-id", "--"]), source].concat();
        for args in [
            daemon,
            words(&["--cast-demo", "receiver-id"]),
            words(&["--cast-test", "192.0.2.1:8009", "cert.pem"]),
            words(&["--live", "--", "output", "DP-1"]),
            words(&["--demo"]),
        ] {
            assert!(runs_a_session(&cmdline(args.clone())), "{args:?}");
        }
        for args in [&["--cast-devices"][..], &["--stop"], &["--status"], &[]] {
            assert!(!runs_a_session(&cmdline(words(args))), "{args:?}");
        }
    }

    #[test]
    fn a_record_is_live_only_while_its_pid_has_the_recorded_start_time() {
        let uid = geteuid().as_raw();
        let running = ProcessIdentity {
            pid: 42,
            starttime: 7_000,
            uid,
        };
        assert!(same_process(7_000, Some(running)));
        // After a crash the pid can belong to another process of this user.
        assert!(!same_process(6_999, Some(running)));
        // Records written before the start time was recorded are stale.
        let unrecorded = ProcessIdentity {
            starttime: 0,
            ..running
        };
        assert!(!same_process(0, Some(unrecorded)));
        let foreign = ProcessIdentity {
            uid: uid.wrapping_add(1),
            ..running
        };
        assert!(!same_process(7_000, Some(foreign)));
        assert!(!same_process(7_000, None));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn our_pid_is_live_only_with_our_start_time() {
        with_runtime(|_| {
            let mut status = sample();
            assert_ne!(status.starttime, 0);
            assert!(status_alive(&status));
            write_live_status(&status).unwrap();
            assert!(current_status().is_some());
            status.starttime += 1;
            assert!(!status_alive(&status));
            write_live_status(&status).unwrap();
            assert!(current_status().is_none());
            assert!(latest_status_report().unwrap().is_none());
            status.starttime = 0;
            assert!(!status_alive(&status));
        });
    }

    fn counted(writes: &Cell<usize>) -> StatusWriter<impl FnMut(&LiveStatus) -> Result<()> + '_> {
        StatusWriter::with(move |_: &LiveStatus| {
            writes.set(writes.get() + 1);
            Ok(())
        })
    }

    #[test]
    fn changing_counters_are_rewritten_at_most_once_a_second() {
        with_runtime(|root| {
            let path = root.join(LEAF).join(STATUS_NAME);
            let start = Instant::now();
            let mut writer = StatusWriter::new();
            let mut status = sample();
            let (mut renames, mut inode) = (0, None);
            // Five seconds of 250 ms ticks in which only counters change.
            for tick in 0..20u64 {
                status.stats.frames = tick;
                status.stats.uptime = tick / 4;
                let now = start + Duration::from_millis(250 * tick);
                writer.write_at(&status, now).unwrap();
                let current = fs::metadata(&path).unwrap().ino();
                if inode != Some(current) {
                    renames += 1;
                    inode = Some(current);
                }
            }
            assert_eq!(renames, 5);
            assert_eq!(read_status().unwrap().unwrap().stats.frames, 16);
            // A new viewer is written without waiting for the interval.
            status.stats.viewers = 1;
            writer
                .write_at(&status, start + Duration::from_millis(4_800))
                .unwrap();
            assert_ne!(Some(fs::metadata(&path).unwrap().ino()), inode);
            assert_eq!(read_status().unwrap().unwrap().stats.viewers, 1);
        });
    }

    type Change = (&'static str, fn(&mut LiveStatus));

    #[test]
    fn what_the_bar_and_launchers_act_on_is_written_at_once() {
        let mut base = sample();
        base.stats.cast = Some(CastStats {
            connection: "starting".into(),
            ..CastStats::default()
        });
        let changes: [Change; 9] = [
            ("state", |s| s.stats.state = "ended".into()),
            ("error", |s| {
                s.stats.error = Some("Selected window closed".into())
            }),
            ("url", |s| s.url.push('x')),
            ("title", |s| s.title.push('x')),
            ("source", |s| s.stats.source.push('x')),
            ("viewers", |s| s.stats.viewers += 1),
            ("desktop", |s| {
                s.stats.desktop = Some(DesktopStats {
                    config: Default::default(),
                    matched: true,
                    occupied: true,
                    updating: false,
                    error: None,
                    reconnect_seconds: 15,
                })
            }),
            ("cast connection", |s| {
                s.stats.cast.as_mut().unwrap().connection = "streaming".into()
            }),
            ("cast readiness", |s| {
                let cast = s.stats.cast.as_mut().unwrap();
                cast.accepted_frames = 1;
                cast.released_frames = 1;
            }),
        ];
        let start = Instant::now();
        for (field, change) in changes {
            let writes = Cell::new(0);
            let mut writer = counted(&writes);
            writer.write_at(&base, start).unwrap();
            let mut changed = base.clone();
            change(&mut changed);
            writer
                .write_at(&changed, start + Duration::from_millis(250))
                .unwrap();
            assert_eq!(writes.get(), 2, "{field} must be written at once");
        }
        // Counters and timings wait for the interval.
        let writes = Cell::new(0);
        let mut writer = counted(&writes);
        writer.write_at(&base, start).unwrap();
        let mut busy = base.clone();
        busy.stats.frames += 30;
        busy.stats.fps = 29.5;
        busy.stats.uptime += 1;
        busy.stats.diagnostics.bytes_sent += 4096;
        let cast = busy.stats.cast.as_mut().unwrap();
        cast.accepted_frames = 30; // not ready until frames are also released
        cast.rtt_us = 900;
        writer
            .write_at(&busy, start + Duration::from_millis(999))
            .unwrap();
        assert_eq!(writes.get(), 1);
        writer
            .write_at(&busy, start + Duration::from_secs(1))
            .unwrap();
        assert_eq!(writes.get(), 2);
        // First, final, and Cast negotiation records do not wait.
        writer.force(&busy).unwrap();
        assert_eq!(writes.get(), 3);
    }

    #[test]
    fn transient_write_failures_are_retried_and_only_an_oversized_record_is_fatal() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let (attempts, failures) = (Cell::new(0), Cell::new(3));
        let mut writer = StatusWriter::with(|_: &LiveStatus| {
            attempts.set(attempts.get() + 1);
            if failures.get() == 0 {
                return Ok(());
            }
            failures.set(failures.get() - 1);
            Err(io::Error::from(Errno::MFILE).into())
        });
        let status = sample();
        writer.write_at(&status, at(0)).unwrap();
        assert_eq!((attempts.get(), writer.logged), (1, Some(at(0))));
        // Nothing changed, but a failed write is retried on the next tick.
        writer.write_at(&status, at(250)).unwrap();
        writer.write_at(&status, at(500)).unwrap();
        assert_eq!((attempts.get(), writer.logged), (3, Some(at(0))));
        writer.write_at(&status, at(750)).unwrap();
        assert_eq!(attempts.get(), 4);
        writer.write_at(&status, at(1_000)).unwrap();
        assert_eq!(attempts.get(), 4, "written 250 ms ago");
        // A lasting failure keeps the share running and logs once per 10 s.
        failures.set(usize::MAX);
        for ms in (1_750..=11_750).step_by(250) {
            writer.write_at(&status, at(ms)).unwrap();
        }
        assert_eq!(attempts.get(), 4 + 41);
        assert_eq!(writer.logged, Some(at(10_000)));
        // Only a record that can never fit ends the share.
        let mut writer = StatusWriter::with(|_: &LiveStatus| Err(StatusTooLarge.into()));
        let error = writer.write_at(&status, start).unwrap_err();
        assert!(error.is::<StatusTooLarge>(), "{error:#}");
        let mut huge = sample();
        huge.stats.diagnostics.bytes_sent = u64::MAX;
        huge.stats.webrtc = Some(crate::live::WebRtcStats {
            encoder: "x".repeat(MAX_STATUS_BYTES),
            ..Default::default()
        });
        with_runtime(|_| {
            let error = StatusWriter::new().force(&huge).unwrap_err();
            assert!(error.is::<StatusTooLarge>(), "{error:#}");
        });
    }

    fn rewrite(lock: &fs::File, record: &str) {
        lock.set_len(0).unwrap();
        lock.write_all_at(record.as_bytes(), 0).unwrap();
    }

    #[test]
    fn the_lock_record_names_its_owner_only_while_the_lock_is_held() {
        with_runtime(|root| {
            let lock = session_lock().unwrap();
            rewrite(&lock, "4242 777\n");
            assert_eq!(lock_owner(), Some((4242, 777)));
            for bad in [
                "",
                "4242",
                "4242 0",
                "0 777",
                "-1 777",
                "x 777",
                "4242 777 1",
                "4242 99999999999999999999",
            ] {
                rewrite(&lock, bad);
                assert_eq!(lock_owner(), None, "{bad:?}");
            }
            rewrite(&lock, "4242 777\n");
            drop(lock);
            // A released lock's record is stale.
            let path = root.join(LEAF).join("session.lock");
            assert_eq!(fs::read_to_string(&path).unwrap(), "4242 777\n");
            assert_eq!(lock_owner(), None);
        });
    }

    #[test]
    fn the_lock_record_is_exactly_what_a_share_writes() {
        for (record, owner) in [
            (&b"1 2"[..], (1, 2)),
            (b"1 2\n", (1, 2)),
            (b"4242 777\n", (4242, 777)),
            // The largest pid_t, and the largest start time.
            (
                b"2147483647 18446744073709551615\n",
                (i32::MAX as u32, u64::MAX),
            ),
        ] {
            let text = String::from_utf8_lossy(record);
            assert_eq!(parse_owner(record), Some(owner), "{text:?}");
        }
        let rejected: [&[u8]; 21] = [
            b" 1 2",
            b"1  2",
            b"+1 2",
            b"1 +2",
            b"1 2 3",
            b"0 2",
            b"1 0",
            b"01 2",
            b"1 02",
            b"007 2",
            b"1 2\n\n",
            b"",
            b"1 2\xff",
            b"\xff",
            b"1 2 ",
            b"1\t2",
            b"1 2\r\n",
            b"\n1 2",
            b"2147483648 2",
            b"4294967296 2",
            b"1 18446744073709551616",
        ];
        let accepted: Vec<_> = rejected
            .into_iter()
            .filter(|record| parse_owner(record).is_some())
            .map(String::from_utf8_lossy)
            .collect();
        assert!(accepted.is_empty(), "accepted {accepted:?}");
    }

    #[test]
    fn a_share_records_itself_in_the_lock_it_holds() {
        with_runtime(|root| {
            let path = root.join(LEAF).join("session.lock");
            let lock = session_lock_owned().unwrap();
            let record = format!("{} {}\n", std::process::id(), self_starttime());
            assert_eq!(fs::read_to_string(&path).unwrap(), record);
            // A refused second owner leaves the record alone.
            assert!(session_lock_owned().is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), record);
            #[cfg(target_os = "linux")]
            assert_eq!(lock_owner(), Some((std::process::id(), self_starttime())));
            drop(lock);
            assert_eq!(lock_owner(), None);
        });
    }

    #[test]
    fn stop_clears_a_stale_record_only_under_the_session_lock() {
        with_runtime(|_| {
            let mut status = sample();
            // A reused pid: this process did not start at that time.
            status.starttime = self_starttime() + 1;
            write_live_status(&status).unwrap();
            let lock = session_lock().unwrap();
            assert!(crate::live::stop_and_cleanup().is_err());
            assert!(read_status().unwrap().is_some());
            drop(lock);
            assert!(!crate::live::stop_and_cleanup().unwrap());
            assert!(read_status().unwrap().is_none());
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stop_reaches_a_starting_share_through_the_lock_it_holds() {
        use std::os::unix::process::ExitStatusExt;
        with_runtime(|_| {
            let cat = ["/usr/bin/cat", "/bin/cat"]
                .into_iter()
                .map(Path::new)
                .find(|path| path.is_file())
                .expect("cat");
            let dir = tempfile::TempDir::new().unwrap();
            let bin = dir.path().join("omabeam");
            fs::copy(cat, &bin).unwrap();
            // Blocks on stdin; its exe and command line look like a share.
            let mut child = spawn_copied(
                std::process::Command::new(&bin)
                    .args(["--", "-", "--live"])
                    .stdin(std::process::Stdio::piped()),
            );
            let pid = child.id();
            let starttime = process_identity(pid).unwrap().starttime;
            let lock = session_lock().unwrap();
            rewrite(&lock, &format!("{pid} {starttime}\n"));
            drop(lock);
            assert!(!stop_live_process(), "the lock is not held");
            let lock = session_lock().unwrap();
            rewrite(&lock, &format!("{pid} {}\n", starttime + 1));
            assert!(!stop_live_process(), "the record names another process");
            assert!(child.try_wait().unwrap().is_none());
            rewrite(&lock, &format!("{pid} {starttime}\n"));
            assert!(stop_live_process());
            let terminated = child.wait().unwrap().signal();
            assert_eq!(terminated, Some(signal_hook::consts::SIGTERM));
            drop(lock);
        });
    }
}
