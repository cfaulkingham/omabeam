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
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::{fs, io};
#[cfg(target_os = "linux")]
use std::{thread, time::Duration};

pub const MAX_STATUS_BYTES: usize = 8192;
pub const MAX_TITLE_BYTES: usize = 200;
pub const MAX_ERROR_BYTES: usize = 400;
pub const MAX_URL_BYTES: usize = 256;
pub const MAX_SOURCE_BYTES: usize = 200;
const LEAF: &str = "omabeam";
const STATUS_NAME: &str = "live.json";
const LOG_NAME: &str = "live.log";

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
        "session.lock",
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

fn write_record(name: &str, payload: &[u8]) -> Result<()> {
    ensure!(
        payload.len() <= MAX_STATUS_BYTES,
        "session payload exceeds the byte limit"
    );
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
    read_status()
        .ok()
        .flatten()
        .filter(|status| pid_alive(status.pid))
}

pub fn latest_status() -> Option<LiveStatus> {
    latest_status_report().ok().flatten()
}

pub fn latest_status_report() -> Result<Option<LiveStatus>> {
    Ok(read_status()?.filter(|status| status.stats.state == "ended" || pid_alive(status.pid)))
}

pub fn pid_alive(pid: u32) -> bool {
    process_identity(pid).is_some_and(|id| id.uid == geteuid().as_raw())
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
    if data.len() > 8192 {
        return false;
    }
    data.split(|b| *b == 0)
        .any(|arg| arg == b"--live" || arg == b"--demo")
}

#[cfg(target_os = "linux")]
fn exe_is_omabeam(pid: u32) -> bool {
    let Ok(path) = fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let file_name = path.file_name().is_some_and(|name| name == "omabeam");
    match std::env::current_exe() {
        Ok(me) => path == me || file_name,
        Err(_) => file_name,
    }
}

#[cfg(target_os = "linux")]
fn matches_session(status: &LiveStatus, id: ProcessIdentity) -> bool {
    status.pid == id.pid
        && status.starttime != 0
        && status.starttime == id.starttime
        && id.uid == geteuid().as_raw()
        && exe_is_omabeam(id.pid)
        && cmdline_is_session(id.pid)
}

#[cfg(not(target_os = "linux"))]
pub fn stop_live_process() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn stop_live_process() -> bool {
    let Ok(Some(status)) = read_status() else {
        return false;
    };
    if !pid_alive(status.pid) {
        clear_live_status();
        return false;
    }
    let Some(expected) = process_identity(status.pid) else {
        return false;
    };
    if !matches_session(&status, expected) {
        return false;
    }
    let Some(pid) = rustix::process::Pid::from_raw(status.pid as i32) else {
        return false;
    };
    let Ok(pidfd) = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) else {
        return false;
    };
    let Some(again) = process_identity(status.pid) else {
        return false;
    };
    if again != expected || !matches_session(&status, again) {
        return false;
    }
    let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::TERM);
    // Capture and HTTP workers must stop before a virtual output is removed.
    // Allow the bounded compositor IPC cleanup to finish before forcing exit.
    for _ in 0..200 {
        if process_identity(status.pid).is_none_or(|id| id != expected) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if process_identity(status.pid).is_some_and(|id| id == expected) {
        let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
    }
    clear_live_status();
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
mod tests {
    use super::*;
    use crate::live::StreamStats;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    fn with_runtime<T>(run: impl FnOnce(&Path) -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
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

    #[test]
    fn refuses_missing_runtime_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
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
}
