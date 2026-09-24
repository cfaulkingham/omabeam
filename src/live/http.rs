use super::{
    BOUNDARY,
    diagnostics::{LogThrottle, SendMeasurement},
    state::{FrameState, Viewer},
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

/// Connections that presented the share token.
const MAX_CLIENTS: usize = 64;
/// Connections that have not presented it yet. A viewer's request holds one
/// of these for milliseconds; a silent socket until the header deadline.
const MAX_PENDING: usize = 32;
const MAX_PENDING_PER_HOST: usize = 8;
const HEADER_DEADLINE: Duration = Duration::from_secs(2);
const IO_DEADLINE: Duration = Duration::from_secs(5);
/// How long a rejected client may keep sending before the socket closes.
const LINGER: Duration = Duration::from_millis(200);
const REJECT_QUEUE: usize = 16;
const ACCEPT_POLL: Duration = Duration::from_millis(250);
const BACKOFF_MIN: Duration = Duration::from_millis(50);
const BACKOFF_MAX: Duration = Duration::from_secs(1);
const BUSY: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

struct ClientSlot(Arc<AtomicUsize>);
impl ClientSlot {
    fn acquire(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_CLIENTS).then_some(n + 1)
            })
            .ok()?;
        Some(Self(count.clone()))
    }
}
impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct PendingCounts {
    total: usize,
    hosts: HashMap<IpAddr, usize>,
}

/// Pre-authentication admission, taken at accept before any byte is read.
#[derive(Default)]
struct PendingBudget(Mutex<PendingCounts>);

impl PendingBudget {
    fn acquire(self: &Arc<Self>, peer: IpAddr) -> Option<PendingSlot> {
        let host = host_group(peer);
        let mut counts = self.0.lock().unwrap();
        let held = counts.hosts.get(&host).copied().unwrap_or(0);
        if counts.total >= MAX_PENDING || held >= MAX_PENDING_PER_HOST {
            return None;
        }
        counts.total += 1;
        counts.hosts.insert(host, held + 1);
        Some(PendingSlot {
            budget: self.clone(),
            host,
        })
    }
}

struct PendingSlot {
    budget: Arc<PendingBudget>,
    host: IpAddr,
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        let mut counts = self.budget.0.lock().unwrap();
        counts.total -= 1;
        if let Some(held) = counts.hosts.get_mut(&self.host) {
            *held -= 1;
            if *held == 0 {
                counts.hosts.remove(&self.host);
            }
        }
    }
}

/// One host can use any address in its /64, so IPv6 peers share a budget per
/// /64. A dual-stack socket reports IPv4 peers as IPv4-mapped IPv6.
fn host_group(peer: IpAddr) -> IpAddr {
    match peer.to_canonical() {
        IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from(u128::from(ip) & !u128::from(u64::MAX))),
        ip => ip,
    }
}

/// A connection's place in the admission tiers until its token matches.
struct Admission {
    pending: PendingSlot,
    clients: Arc<AtomicUsize>,
}

impl Admission {
    /// Trades the pre-auth slot for a client slot. Without a free client slot
    /// the caller keeps the pre-auth slot while it writes the rejection.
    fn admit(self) -> Result<ClientSlot, Self> {
        let Some(slot) = ClientSlot::acquire(&self.clients) else {
            return Err(self);
        };
        drop(self.pending);
        Ok(slot)
    }
}

/// Answers connections refused at accept. One thread waits out their clean
/// closes, so a slow client never stalls the accept loop.
struct Rejecter(Option<SyncSender<TcpStream>>);

impl Rejecter {
    fn start() -> Self {
        let (queue, refused) = mpsc::sync_channel::<TcpStream>(REJECT_QUEUE);
        let worker = thread::Builder::new()
            .name("omabeam-reject".into())
            .spawn(move || {
                for mut stream in refused {
                    // BSD sockets inherit O_NONBLOCK from the listener.
                    let _ = stream.set_nonblocking(false);
                    if write_parts(&mut stream, &[BUSY], LINGER).is_ok() {
                        close_gracefully(&mut stream);
                    }
                }
            });
        match worker {
            Ok(_) => Self(Some(queue)),
            Err(error) => {
                eprintln!("live share server: refused connections will close abruptly: {error}");
                Self(None)
            }
        }
    }

    fn reject(&self, stream: TcpStream) {
        let mut stream = match &self.0 {
            Some(queue) => match queue.try_send(stream) {
                Ok(()) => return,
                Err(TrySendError::Full(stream) | TrySendError::Disconnected(stream)) => stream,
            },
            None => stream,
        };
        // The queue is full: answer without waiting, as before it existed.
        let _ = stream.set_nonblocking(true);
        let _ = stream.write(BUSY);
    }
}

/// Closing with unread request bytes makes the kernel send RST, which can
/// destroy the response before the client reads it. Send FIN after the
/// response, then discard what the client still sends until it closes.
fn close_gracefully(stream: &mut TcpStream) {
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let deadline = Instant::now() + LINGER;
    let mut discard = [0; 4096];
    while let Some(left) = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
    {
        if stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        match stream.read(&mut discard) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AcceptPolicy {
    /// Nothing is queued: poll the listener.
    Wait,
    /// Only this connection failed.
    Retry,
    /// Out of descriptors or memory. The connection stays queued, so an
    /// immediate retry would spin: log and back off.
    Backoff,
    /// The listener itself is unusable.
    Fatal,
}

fn accept_policy(error: &io::Error) -> AcceptPolicy {
    if error.kind() == io::ErrorKind::WouldBlock {
        return AcceptPolicy::Wait;
    }
    match Errno::from_io_error(error) {
        Some(Errno::BADF | Errno::INVAL | Errno::NOTSOCK) => AcceptPolicy::Fatal,
        // Linux reports the new connection's pending network errors from
        // accept(2), which says to retry them like EAGAIN. That connection is
        // dequeued, so an immediate retry cannot spin.
        Some(
            Errno::CONNABORTED
            | Errno::PROTO
            | Errno::INTR
            | Errno::NETDOWN
            | Errno::HOSTDOWN
            | Errno::HOSTUNREACH
            | Errno::NETUNREACH
            | Errno::NOPROTOOPT
            | Errno::OPNOTSUPP,
        ) => AcceptPolicy::Retry,
        // BSD has no ENONET.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Some(Errno::NONET) => AcceptPolicy::Retry,
        // EMFILE, ENFILE, ENOBUFS, ENOMEM. Unexpected errors are treated the
        // same rather than ending the share.
        _ => AcceptPolicy::Backoff,
    }
}

/// Waits until a connection is queued, at most one poll interval.
fn wait_for_connection(listener: &TcpListener) {
    let mut fds = [PollFd::new(listener, PollFlags::IN)];
    let polled = Timespec::try_from(ACCEPT_POLL)
        .ok()
        .map(|timeout| poll(&mut fds, Some(&timeout)));
    if !matches!(polled, Some(Ok(_) | Err(Errno::INTR))) {
        // Not expected for one valid descriptor; never spin on it.
        thread::sleep(BACKOFF_MIN);
    }
}

/// Sleeps for a backoff, still checking the stop flag every poll interval.
fn pause(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(ACCEPT_POLL));
    }
}

pub(super) fn serve(
    listener: TcpListener,
    token: String,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
) {
    serve_with(&listener, TcpListener::accept, &token, frames, stop);
}

fn serve_with(
    listener: &TcpListener,
    mut accept: impl FnMut(&TcpListener) -> io::Result<(TcpStream, SocketAddr)>,
    token: &str,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
) {
    // A blocking accept would never observe the stop flag.
    if let Err(error) = listener.set_nonblocking(true) {
        frames.fail(format!("Share server stopped: {error}"));
        return;
    }
    let prefix: Arc<str> = format!("/s/{token}").into();
    let clients = Arc::new(AtomicUsize::new(0));
    let budget = Arc::new(PendingBudget::default());
    let rejecter = Rejecter::start();
    let mut backoff = BACKOFF_MIN;
    let (mut accept_log, mut spawn_log) = (LogThrottle::default(), LogThrottle::default());
    while !stop.load(Ordering::SeqCst) {
        let (stream, peer) = match accept(listener) {
            Ok(accepted) => {
                backoff = BACKOFF_MIN;
                accepted
            }
            Err(error) => {
                match accept_policy(&error) {
                    AcceptPolicy::Wait => wait_for_connection(listener),
                    AcceptPolicy::Retry => {}
                    AcceptPolicy::Backoff => {
                        if accept_log.allow(Instant::now()) {
                            eprintln!(
                                "live share server: accept failed, retrying in {} ms: {error}",
                                backoff.as_millis()
                            );
                        }
                        pause(&stop, backoff);
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                    AcceptPolicy::Fatal => {
                        frames.fail(format!("Share server stopped: {error}"));
                        break;
                    }
                }
                continue;
            }
        };
        let Some(pending) = budget.acquire(peer.ip()) else {
            rejecter.reject(stream);
            continue;
        };
        let admission = Admission {
            pending,
            clients: clients.clone(),
        };
        if let Err((error, stream)) = spawn_client(stream, admission, &prefix, &frames, &stop) {
            if spawn_log.allow(Instant::now()) {
                eprintln!("live share server: could not start a client thread: {error}");
            }
            if let Some(stream) = stream {
                rejecter.reject(stream);
            }
        }
    }
}

/// A failed spawn drops the closure, and the stream with it, so the stream
/// waits in a shared slot from which the caller can reclaim it to answer.
fn spawn_client(
    stream: TcpStream,
    admission: Admission,
    prefix: &Arc<str>,
    frames: &Arc<FrameState>,
    stop: &Arc<AtomicBool>,
) -> Result<(), (io::Error, Option<TcpStream>)> {
    let handoff = Arc::new(Mutex::new(Some(stream)));
    let claim = handoff.clone();
    let (prefix, frames, stop) = (prefix.clone(), frames.clone(), stop.clone());
    thread::Builder::new()
        .name("omabeam-client".into())
        .spawn(move || {
            let Some(stream) = claim.lock().unwrap().take() else {
                return;
            };
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_nodelay(true);
            handle_connection(stream, &prefix, frames, stop, Some(admission));
        })
        .map(drop)
        .map_err(|error| (error, handoff.lock().unwrap().take()))
}

struct Request {
    header: String,
    buffered: Vec<u8>,
    deadline: Instant,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let deadline = Instant::now() + HEADER_DEADLINE;
    let mut header = Vec::new();
    let mut buf = [0; 1024];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        stream.set_read_timeout(Some(remaining)).ok()?;
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        header.extend_from_slice(&buf[..n]);
        if header.len() > 16 * 1024 {
            return None;
        }
        if let Some(end) = header.windows(4).position(|w| w == b"\r\n\r\n") {
            let buffered = header.split_off(end + 4);
            return Some(Request {
                header: String::from_utf8(header).ok()?,
                buffered,
                deadline,
            });
        }
    }
}

fn read_json_body(stream: &mut TcpStream, request: &Request) -> Option<Vec<u8>> {
    let mut length = None;
    let mut json = false;
    let mut origin = None;
    let mut host = None;
    for line in request
        .header
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty())
    {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => {
                if length.is_some() {
                    return None;
                }
                length = Some(value.parse::<usize>().ok()?);
            }
            "content-type" => json = value.split(';').next()?.trim() == "application/json",
            "transfer-encoding" => return None,
            "origin" => {
                if origin.is_some() {
                    return None;
                }
                origin = Some(value);
            }
            "host" => {
                if host.is_some() {
                    return None;
                }
                host = Some(value);
            }
            _ => {}
        }
    }
    if !json
        || origin.is_some_and(|origin| host.is_none_or(|host| origin != format!("http://{host}")))
    {
        return None;
    }
    let length = length.filter(|length| (1..=65536).contains(length))?;
    if request.buffered.len() > length {
        return None;
    }
    let mut body = request.buffered.clone();
    while body.len() < length {
        stream
            .set_read_timeout(Some(
                request.deadline.checked_duration_since(Instant::now())?,
            ))
            .ok()?;
        let mut buf = [0; 4096];
        let left = (length - body.len()).min(buf.len());
        let count = stream.read(&mut buf[..left]).ok()?;
        if count == 0 {
            return None;
        }
        body.extend_from_slice(&buf[..count]);
    }
    Some(body)
}

pub(super) fn write_parts(
    stream: &mut TcpStream,
    parts: &[&[u8]],
    timeout: Duration,
) -> io::Result<()> {
    write_parts_counted(stream, parts, timeout, &mut 0)
}

fn write_parts_counted(
    stream: &mut TcpStream,
    parts: &[&[u8]],
    timeout: Duration,
    bytes_written: &mut u64,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    for part in parts {
        let mut remaining = *part;
        while !remaining.is_empty() {
            let left = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "HTTP write deadline exceeded")
                })?;
            stream.set_write_timeout(Some(left))?;
            match stream.write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    *bytes_written += n as u64;
                    remaining = &remaining[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

fn response(
    stream: &mut TcpStream,
    status: &str,
    kind: &str,
    body: &[u8],
    extra: &str,
) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n{extra}\r\n",
        body.len()
    );
    write_parts(stream, &[header.as_bytes(), body], IO_DEADLINE)
}

/// Answers an error that may leave part of the request unread.
fn reject(stream: &mut TcpStream, status: &str, body: &[u8], extra: &str) {
    if response(stream, status, "text/plain", body, extra).is_ok() {
        close_gracefully(stream);
    }
}

/// The route after the share token. The comparison has no early exit, so
/// response timing does not reveal how many leading bytes were right.
fn strip_token<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = path.as_bytes().get(..prefix.len())?;
    let difference = candidate
        .iter()
        .zip(prefix.as_bytes())
        .fold(0, |difference, (a, b)| difference | (a ^ b));
    if std::hint::black_box(difference) != 0 {
        return None;
    }
    path.get(prefix.len()..)
}

fn share_ended(frames: &FrameState, stop: &AtomicBool) -> bool {
    stop.load(Ordering::SeqCst) || frames.inner.lock().unwrap().ended.is_some()
}

/// Serves one connection without the admission tiers.
#[cfg(test)]
pub(super) fn handle_client(
    stream: TcpStream,
    prefix: &str,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
) {
    handle_connection(stream, prefix, frames, stop, None);
}

fn handle_connection(
    mut stream: TcpStream,
    prefix: &str,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
    admission: Option<Admission>,
) {
    let Some(mut request) = read_request(&mut stream) else {
        return;
    };
    let fields: Vec<_> = request
        .header
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if fields.len() != 3
        || !matches!(fields[2], "HTTP/1.0" | "HTTP/1.1")
        || !fields[1].starts_with('/')
    {
        reject(&mut stream, "400 Bad Request", b"malformed request", "");
        return;
    }
    let path = fields[1].split('?').next().unwrap_or_default();
    // Match the complete token path before exposing any frames or diagnostics.
    let Some(route) = strip_token(path, prefix) else {
        reject(&mut stream, "404 Not Found", b"not found", "");
        return;
    };
    // Leave the pre-auth tier, and give a request body the full I/O deadline.
    let _client = match admission.map(Admission::admit) {
        None => None,
        Some(Ok(slot)) => Some(slot),
        Some(Err(_pending)) => {
            reject(
                &mut stream,
                "503 Service Unavailable",
                b"too many connections",
                "",
            );
            return;
        }
    };
    request.deadline = Instant::now() + IO_DEADLINE;
    let connections: Vec<_> = fields[1]
        .split_once('?')
        .map_or("", |(_, q)| q)
        .split('&')
        .filter_map(|part| part.strip_prefix("viewer="))
        .collect();
    let connection = if connections.len() == 1 {
        Some(connections[0])
    } else {
        None
    };
    if matches!(
        route,
        "/stream" | "/frame.jpg" | "/webrtc/offer" | "/webrtc/close"
    ) && !frames.authorized(connection)
    {
        reject(
            &mut stream,
            "409 Conflict",
            super::desktop::IN_USE.as_bytes(),
            "",
        );
        return;
    }
    if fields[0] == "POST"
        && matches!(
            route,
            "/desktop/claim" | "/desktop/heartbeat" | "/desktop/release" | "/desktop/size"
        )
    {
        if share_ended(&frames, &stop) {
            reject(&mut stream, "410 Gone", b"share ended", "");
            return;
        }
        let Some(desktop) = &frames.desktop else {
            reject(&mut stream, "404 Not Found", b"not an extended desktop", "");
            return;
        };
        #[derive(serde::Deserialize)]
        struct Command {
            connection: String,
            client: Option<String>,
            size: Option<Size>,
        }
        #[derive(serde::Deserialize)]
        struct Size {
            width: u32,
            height: u32,
            scale: u32,
        }
        let command = read_json_body(&mut stream, &request)
            .and_then(|body| serde_json::from_slice::<Command>(&body).ok());
        let Some(command) = command else {
            reject(
                &mut stream,
                "400 Bad Request",
                b"expected bounded, same-origin JSON",
                "",
            );
            return;
        };
        let result = match route {
            "/desktop/claim" => {
                desktop.claim(command.client.as_deref().unwrap_or(""), &command.connection)
            }
            "/desktop/heartbeat" => desktop.heartbeat(&command.connection),
            "/desktop/release" => {
                desktop.release(&command.connection);
                Ok(())
            }
            _ => desktop.request_size(
                &command.connection,
                command.size.map(|s| (s.width, s.height, s.scale)),
            ),
        };
        match result {
            Ok(()) => {
                frames.tick.notify_all();
                let _ = response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &serde_json::to_vec(&desktop.stats()).unwrap(),
                    "",
                );
            }
            Err((status, message)) => {
                let _ = response(&mut stream, status, "text/plain", message.as_bytes(), "");
            }
        }
        return;
    }
    if fields[0] == "POST" && matches!(route, "/webrtc/offer" | "/webrtc/close") {
        let rtc = frames.rtc.lock().unwrap().clone();
        let Some(rtc) = rtc else {
            reject(&mut stream, "404 Not Found", b"WebRTC is disabled", "");
            return;
        };
        if share_ended(&frames, &stop) {
            reject(&mut stream, "410 Gone", b"share ended", "");
            return;
        }
        let Some(body) = read_json_body(&mut stream, &request) else {
            reject(
                &mut stream,
                "400 Bad Request",
                b"expected bounded, same-origin JSON",
                "",
            );
            return;
        };
        let result = if route == "/webrtc/offer" {
            rtc.offer(&body, connection.map(str::to_owned))
        } else {
            #[derive(serde::Deserialize)]
            struct Close {
                id: String,
            }
            serde_json::from_slice::<Close>(&body)
                .map_err(anyhow::Error::from)
                .and_then(|body| rtc.close(body.id))
                .map(|_| serde_json::json!({"closed": true}))
        };
        match result {
            Ok(value) => {
                let _ = response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &serde_json::to_vec(&value).unwrap(),
                    "",
                );
            }
            Err(error) => {
                let _ = response(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    error.to_string().as_bytes(),
                    "",
                );
            }
        }
        return;
    }
    if fields[0] != "GET" {
        reject(
            &mut stream,
            "405 Method Not Allowed",
            b"use GET",
            "Allow: GET\r\n",
        );
        return;
    }
    match route {
        "" => {
            let _ = response(
                &mut stream,
                "302 Found",
                "text/plain",
                b"",
                &format!("Location: {prefix}/\r\n"),
            );
        }
        "/" => {
            let _ = response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                viewer_html().as_bytes(),
                "",
            );
        }
        "/stats" => {
            // Per-connection details are HTTP-only: keep the bounded on-disk
            // session status small even with all 64 client slots in use.
            #[derive(serde::Serialize)]
            struct Response {
                #[serde(flatten)]
                stream: super::StreamStats,
                clients: Vec<super::ViewerDiagnostics>,
            }
            let _ = response(
                &mut stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&Response {
                    stream: frames.stats(),
                    clients: frames.viewer_diagnostics(),
                })
                .unwrap(),
                "",
            );
        }
        // Checked before any header, so a stream does not start with a 200.
        "/frame.jpg" | "/stream" if share_ended(&frames, &stop) => {
            reject(&mut stream, "410 Gone", b"share ended", "");
        }
        "/frame.jpg" => match frames.jpeg_frame() {
            Ok((jpeg, ..)) if !jpeg.is_empty() => {
                let _ = response(&mut stream, "200 OK", "image/jpeg", &jpeg, "");
            }
            // The share can end while the request waits for the encoder.
            Ok(_) if share_ended(&frames, &stop) => {
                reject(&mut stream, "410 Gone", b"share ended", "");
            }
            Ok(_) => reject(
                &mut stream,
                "503 Service Unavailable",
                b"no frame available",
                "Retry-After: 1\r\n",
            ),
            Err(_) => reject(
                &mut stream,
                "500 Internal Server Error",
                b"could not encode the frame",
                "",
            ),
        },
        "/stream" => {
            let _ = write_mjpeg(&mut stream, &frames, &stop, connection);
        }
        _ => reject(&mut stream, "404 Not Found", b"not found", ""),
    }
}

fn peer_closed(stream: &TcpStream) -> bool {
    if stream
        .set_read_timeout(Some(Duration::from_millis(1)))
        .is_err()
    {
        return true;
    }
    match stream.peek(&mut [0; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ),
    }
}

fn write_mjpeg(
    stream: &mut TcpStream,
    frames: &FrameState,
    stop: &AtomicBool,
    connection: Option<&str>,
) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    );
    write_parts(stream, &[header.as_bytes()], IO_DEADLINE)?;
    let viewer = Viewer::new(frames);
    let mut last = 0;
    while !stop.load(Ordering::SeqCst) && frames.authorized(connection) {
        let data = frames.inner.lock().unwrap();
        let (data, _) = frames
            .tick
            .wait_timeout_while(data, Duration::from_millis(250), |d| {
                d.generation == last && d.ended.is_none() && !stop.load(Ordering::SeqCst)
            })
            .unwrap();
        if data.ended.is_some() || stop.load(Ordering::SeqCst) || !frames.authorized(connection) {
            break;
        }
        if data.generation == last {
            drop(data);
            if peer_closed(stream) {
                break;
            }
            continue;
        }
        drop(data);
        let (jpeg, generation, encode_started_at) = match frames.jpeg_frame() {
            Ok(frame) => frame,
            // Skip a frame that cannot be encoded; the next one may be fine.
            Err(failed) => {
                last = failed.generation;
                continue;
            }
        };
        let skipped = if last == 0 {
            0
        } else {
            generation.saturating_sub(last + 1)
        };
        last = generation;
        if jpeg.is_empty() {
            continue;
        }
        let part = format!(
            "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        );
        let started = Instant::now();
        let mut bytes = 0;
        let result = write_parts_counted(
            stream,
            &[part.as_bytes(), &jpeg, b"\r\n"],
            IO_DEADLINE,
            &mut bytes,
        );
        viewer.record_send(SendMeasurement {
            bytes,
            skipped,
            elapsed: started.elapsed(),
            frame_age: encode_started_at.elapsed(),
            completed: result.is_ok(),
        });
        result?;
    }
    Ok(())
}

pub fn viewer_html() -> String {
    themed_viewer_html(&gpui_omarchy::Theme::system_or_default())
}

fn themed_viewer_html(theme: &gpui_omarchy::Theme) -> String {
    // Only serialized colors enter CSS; theme names, paths, and arbitrary file
    // contents never enter the page. Reuse the native picker's palette loader.
    let mut css = String::from(":root {");
    for (name, color) in [
        ("background", theme.background),
        ("foreground", theme.foreground),
        ("accent", theme.accent),
        ("urgent", theme.danger),
    ] {
        let rgb: gpui_kit::Rgba = color.into();
        css.push_str(&format!(
            "--{name}:#{:02x}{:02x}{:02x};",
            (rgb.r * 255.).round() as u8,
            (rgb.g * 255.).round() as u8,
            (rgb.b * 255.).round() as u8,
        ));
    }
    css.push_str(if theme.background.l > theme.foreground.l {
        "color-scheme:light;}"
    } else {
        "color-scheme:dark;}"
    });
    include_str!("viewer.html").replace("/* host-theme */", &css)
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::FrameMeasurement;
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn slow_viewer_skips_old_frames_without_charging_a_new_viewer() {
        fn connect(frames: &Arc<FrameState>) -> (BufReader<TcpStream>, thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let (mut socket, _) = listener.accept().unwrap();
            let frames = frames.clone();
            let worker = thread::spawn(move || {
                let _ = write_mjpeg(&mut socket, &frames, &AtomicBool::new(false), None);
            });
            let mut reader = BufReader::new(client);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(!line.is_empty());
                if line == "\r\n" {
                    break;
                }
            }
            (reader, worker)
        }
        fn part_length(reader: &mut BufReader<TcpStream>) -> usize {
            let mut boundary = String::new();
            reader.read_line(&mut boundary).unwrap();
            assert_eq!(boundary, format!("--{BOUNDARY}\r\n"));
            let mut length = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(!line.is_empty());
                if line == "\r\n" {
                    return length.unwrap();
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    length = Some(value.trim().parse().unwrap());
                }
            }
        }
        fn consume(reader: &mut BufReader<TcpStream>, length: usize) {
            assert_eq!(
                io::copy(&mut (&mut *reader).take(length as u64), &mut io::sink()).unwrap(),
                length as u64
            );
            let mut end = [0; 2];
            reader.read_exact(&mut end).unwrap();
            assert_eq!(&end, b"\r\n");
        }
        fn until(mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !ready() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
        }

        let frames = Arc::new(FrameState::new("slow-reader".into()));
        let size = 32 * 1024 * 1024; // larger than the socket send buffer
        frames.publish(vec![1; size], 1, 1, FrameMeasurement::default());
        let (mut slow, slow_worker) = connect(&frames);
        assert_eq!(part_length(&mut slow), size);
        assert_eq!(frames.stats().diagnostics.frames_sent, 0);
        for value in [2, 3, 4] {
            frames.publish(vec![value], 1, 1, FrameMeasurement::default());
        }
        let (mut fast, fast_worker) = connect(&frames);
        assert_eq!(part_length(&mut fast), 1);
        consume(&mut fast, 1);
        until(|| frames.viewer_diagnostics()[1].frames_sent == 1);
        assert_eq!(frames.viewer_diagnostics()[1].frames_skipped, 0);
        consume(&mut slow, size);
        assert_eq!(part_length(&mut slow), 1);
        consume(&mut slow, 1);
        until(|| frames.stats().diagnostics.frames_sent == 3);
        let clients = frames.viewer_diagnostics();
        assert_eq!(clients[0].frames_sent, 2);
        assert_eq!(clients[0].frames_skipped, 2);
        assert_eq!(clients[1].frames_skipped, 0);
        assert_eq!(frames.stats().diagnostics.frames_skipped, 2);
        assert!(frames.stats().diagnostics.bytes_sent > size as u64);
        assert!(clients[0].frame_age_ms.is_some());
        for client in [&slow, &fast] {
            client.get_ref().shutdown(std::net::Shutdown::Both).unwrap();
        }
        drop((slow, fast));
        slow_worker.join().unwrap();
        fast_worker.join().unwrap();
        assert!(frames.viewer_diagnostics().is_empty());
        assert_eq!(frames.stats().diagnostics.frames_skipped, 2);
    }

    #[test]
    fn viewer_uses_host_colors_without_inserting_theme_metadata() {
        let mut theme = gpui_omarchy::Theme::flexoki_light();
        theme.name = "</style><script>alert(1)</script>".into();
        let html = themed_viewer_html(&theme);
        assert!(html.contains("--background:#fffcf0;"));
        assert!(html.contains("--accent:#205ea6;"));
        assert!(html.contains("color-scheme:light;"));
        assert!(!html.contains("alert(1)"));
        assert!(!html.contains("/* host-theme */"));
    }

    #[test]
    fn pre_auth_budget_caps_hosts_and_total_and_is_traded_for_a_client_slot() {
        let budget = Arc::new(PendingBudget::default());
        let ip = |text: &str| text.parse::<std::net::IpAddr>().unwrap();
        let mut held: Vec<_> = (0..8)
            .map(|_| budget.acquire(ip("10.0.0.1")).unwrap())
            .collect();
        assert!(budget.acquire(ip("10.0.0.1")).is_none());
        // A dual-stack socket reports IPv4 peers as IPv4-mapped IPv6.
        assert!(budget.acquire(ip("::ffff:10.0.0.1")).is_none());
        assert!(budget.acquire(ip("10.0.0.2")).is_some());
        // One host can hold a whole /64, so IPv6 peers share one budget per /64.
        held.extend((1..=8).map(|n| budget.acquire(ip(&format!("2001:db8::{n:x}"))).unwrap()));
        assert!(budget.acquire(ip("2001:db8::ffff:1234:5678")).is_none());
        held.push(budget.acquire(ip("2001:db8:0:1::1")).unwrap());
        held.extend((0..7).map(|_| budget.acquire(ip("10.0.0.2")).unwrap()));
        held.extend((0..8).map(|_| budget.acquire(ip("10.0.0.3")).unwrap()));
        assert_eq!(held.len(), 32);
        assert!(budget.acquire(ip("10.0.0.4")).is_none());

        // A token match releases the pre-auth slot for a client slot.
        let clients = Arc::new(AtomicUsize::new(0));
        let admitted = Admission {
            pending: held.pop().unwrap(),
            clients: clients.clone(),
        }
        .admit();
        assert!(admitted.is_ok());
        assert_eq!(clients.load(Ordering::SeqCst), 1);
        assert!(budget.acquire(ip("10.0.0.4")).is_some());
        // With every client slot taken, the pre-auth slot stays held until
        // the rejection has been written.
        let full: Vec<_> = (1..MAX_CLIENTS)
            .map(|_| ClientSlot::acquire(&clients).unwrap())
            .collect();
        let refused = Admission {
            pending: budget.acquire(ip("10.0.0.4")).unwrap(),
            clients: clients.clone(),
        }
        .admit();
        assert!(refused.is_err());
        assert!(budget.acquire(ip("10.0.0.5")).is_none());
        drop(refused);
        assert!(budget.acquire(ip("10.0.0.5")).is_some());
        drop((admitted, full));
        assert_eq!(clients.load(Ordering::SeqCst), 0);
        // Every slot returns, per host too.
        drop(held);
        let again: Vec<_> = (0..8).map(|_| budget.acquire(ip("10.0.0.1"))).collect();
        assert!(again.iter().all(Option::is_some));
    }

    #[test]
    fn accept_errors_end_the_share_only_when_the_listener_is_unusable() {
        use AcceptPolicy::{Backoff, Fatal, Retry, Wait};
        use rustix::io::Errno;
        for (errno, expected) in [
            (Errno::AGAIN, Wait),
            (Errno::BADF, Fatal),
            (Errno::INVAL, Fatal),
            (Errno::NOTSOCK, Fatal),
            (Errno::MFILE, Backoff),
            (Errno::NFILE, Backoff),
            (Errno::NOBUFS, Backoff),
            (Errno::NOMEM, Backoff),
            (Errno::CONNABORTED, Retry),
            (Errno::PROTO, Retry),
            (Errno::INTR, Retry),
            // accept(2): the new connection's pending network errors.
            (Errno::NETDOWN, Retry),
            (Errno::HOSTDOWN, Retry),
            (Errno::HOSTUNREACH, Retry),
            (Errno::NETUNREACH, Retry),
            (Errno::NOPROTOOPT, Retry),
            (Errno::OPNOTSUPP, Retry),
            // Unexpected errors neither end the share nor spin.
            (Errno::PERM, Backoff),
        ] {
            let error = io::Error::from_raw_os_error(errno.raw_os_error());
            assert_eq!(accept_policy(&error), expected, "{errno:?}");
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            accept_policy(&io::Error::from_raw_os_error(Errno::NONET.raw_os_error())),
            Retry
        );
        assert_eq!(accept_policy(&io::Error::other("no errno")), Backoff);
    }

    #[test]
    fn authorized_streams_leave_the_pre_auth_budget_and_fill_only_the_client_tier() {
        fn part(reader: &mut BufReader<TcpStream>) -> Vec<u8> {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, format!("--{BOUNDARY}\r\n"));
            let mut length = None;
            while line != "\r\n" {
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert!(!line.is_empty(), "stream closed in a part header");
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    length = Some(value.trim().parse().unwrap());
                }
            }
            let mut body = vec![0; length.unwrap()];
            reader.read_exact(&mut body).unwrap();
            let mut end = [0; 2];
            reader.read_exact(&mut end).unwrap();
            assert_eq!(&end, b"\r\n");
            body
        }

        let frames = Arc::new(FrameState::new("streams".into()));
        frames.publish(vec![1], 1, 1, FrameMeasurement::default());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (frames, stop) = (frames.clone(), stop.clone());
            thread::spawn(move || serve(listener, "test".into(), frames, stop))
        };
        // Reading the first frame proves the server admitted the stream.
        let open = || {
            let client = TcpStream::connect(address).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            (&client)
                .write_all(b"GET /s/test/stream HTTP/1.1\r\n\r\n")
                .unwrap();
            let mut reader = BufReader::new(client);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("HTTP/1.1 200"), "{line}");
            while line != "\r\n" {
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert!(!line.is_empty(), "stream closed in its header");
            }
            part(&mut reader);
            reader
        };
        // A reset fails read_to_end, and so the test.
        let get = |path: &str| {
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes())
                .unwrap();
            let mut reply = Vec::new();
            socket.read_to_end(&mut reply).unwrap();
            String::from_utf8_lossy(&reply).into_owned()
        };

        // Long-lived streams from one host, as many as its pre-auth budget.
        let mut streams: Vec<_> = (0..MAX_PENDING_PER_HOST).map(|_| open()).collect();
        let reply = get("/s/test/stats");
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        // They are still streaming.
        frames.publish(vec![2], 1, 1, FrameMeasurement::default());
        for stream in &mut streams {
            assert_eq!(part(stream), [2]);
        }

        // Streams count against the client tier alone.
        streams.extend((MAX_PENDING_PER_HOST..MAX_CLIENTS).map(|_| open()));
        assert_eq!(frames.viewers.load(Ordering::SeqCst), MAX_CLIENTS);
        let reply = get("/s/test/stats");
        assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
        // Without the token, the answer does not depend on the client tier.
        let reply = get("/s/wrong/stats");
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        // A closed stream returns its slot.
        let closed = streams.pop().unwrap();
        closed.get_ref().shutdown(Shutdown::Both).unwrap();
        drop(closed);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let reply = get("/s/test/stats");
            if reply.starts_with("HTTP/1.1 200") {
                break;
            }
            assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
            assert!(Instant::now() < deadline, "a closed stream kept its slot");
            thread::sleep(Duration::from_millis(20));
        }

        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        drop(streams);
        let deadline = Instant::now() + Duration::from_secs(3);
        while frames.viewers.load(Ordering::SeqCst) > 0 {
            assert!(
                Instant::now() < deadline,
                "stream threads outlived the share"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn descriptor_exhaustion_backs_off_and_the_queued_client_is_still_served() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        // Queued before the server's first accept, which then fails twice.
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .write_all(b"GET /s/test/stats HTTP/1.1\r\n\r\n")
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let frames = Arc::new(FrameState::new("descriptors".into()));
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let server = {
            let (frames, stop, calls) = (frames.clone(), stop.clone(), calls.clone());
            thread::spawn(move || {
                let accept = move |listener: &TcpListener| {
                    if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err(io::Error::from_raw_os_error(
                            rustix::io::Errno::MFILE.raw_os_error(),
                        ))
                    } else {
                        listener.accept()
                    }
                };
                serve_with(&listener, accept, "test", frames, stop);
            })
        };
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert!(
            reply.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&reply)
        );
        // 50 ms, then 100 ms: the queued connection did not make it spin.
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(frames.stats().state, "live");
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(calls.load(Ordering::SeqCst) < 10);
    }

    #[test]
    fn token_comparison_accepts_only_the_complete_prefix() {
        let prefix = "/s/0123456789abcdef0123456789abcdef";
        assert_eq!(
            strip_token(&format!("{prefix}/stats"), prefix),
            Some("/stats")
        );
        assert_eq!(strip_token(prefix, prefix), Some(""));
        for path in [
            "/s/0123456789abcdef0123456789abcdee/stats",
            "/s/1123456789abcdef0123456789abcdef",
            "/s/0123456789abcdef",
            "",
            // A multibyte character straddling the prefix end must not panic.
            "/s/0123456789abcdef0123456789abcdeé",
        ] {
            assert_eq!(strip_token(path, prefix), None, "{path}");
        }
    }

    #[test]
    fn client_limit_releases_capacity_when_handler_ends() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut slots: Vec<_> = (0..MAX_CLIENTS)
            .map(|_| ClientSlot::acquire(&count).unwrap())
            .collect();
        assert!(ClientSlot::acquire(&count).is_none());
        slots.pop();
        assert!(ClientSlot::acquire(&count).is_some());
        drop(slots);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ended_viewer_centers_disconnected_copy_instead_of_a_broken_image() {
        let html = viewer_html();
        assert!(html.contains("OmaBeam disconnected"));
        assert!(html.contains("id=\"disconnected\""));
        assert!(html.contains("id=\"disconnected-copy\""));
        assert!(html.contains("class=\"oma-logo\""));
        assert!(html.contains("class=\"oma-wordmark\""));
        assert!(!html.contains("Retry H.264"));
        assert!(!html.contains("Shared tile"));
        assert!(html.contains("#stage.ended #disconnected"));
    }

    #[test]
    fn viewer_hides_an_idle_pointer_and_requests_fullscreen_for_an_extended_display() {
        let html = viewer_html();
        assert!(html.contains("body.idle"));
        assert!(html.contains("cursor: none"));
        assert!(html.contains("id=\"chrome\""));
        assert!(html.contains("tryDesktopFullscreen"));
        assert!(html.contains("enterDesktopFullscreen"));
        assert!(html.contains("placeDesktopControls"));
        assert!(html.contains("Tap the picture to enter fullscreen"));
    }
}
