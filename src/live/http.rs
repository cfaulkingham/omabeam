use super::{
    BOUNDARY,
    state::{FrameState, Viewer},
};
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_CLIENTS: usize = 64;
const IO_DEADLINE: Duration = Duration::from_secs(5);

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

pub(super) fn serve(
    listener: TcpListener,
    token: String,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
) {
    let prefix = format!("/s/{token}");
    let clients = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let Some(slot) = ClientSlot::acquire(&clients) else {
                    // Never let an overloaded client block the accept loop.
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.write(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                };
                let frames = frames.clone();
                let prefix = prefix.clone();
                let stop = stop.clone();
                let _ = thread::Builder::new()
                    .name("omabeam-client".into())
                    .spawn(move || {
                        let _slot = slot;
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_nodelay(true);
                        handle_client(stream, &prefix, frames, stop);
                    });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(e) => {
                frames.fail(format!("Share server stopped: {e}"));
                break;
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    let deadline = Instant::now() + IO_DEADLINE;
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
        if header.windows(4).any(|w| w == b"\r\n\r\n") {
            return String::from_utf8(header).ok();
        }
    }
}

pub(super) fn write_parts(
    stream: &mut TcpStream,
    parts: &[&[u8]],
    timeout: Duration,
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
                Ok(n) => remaining = &remaining[n..],
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

pub(super) fn handle_client(
    mut stream: TcpStream,
    prefix: &str,
    frames: Arc<FrameState>,
    stop: Arc<AtomicBool>,
) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let fields: Vec<_> = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if fields.len() != 3
        || !matches!(fields[2], "HTTP/1.0" | "HTTP/1.1")
        || !fields[1].starts_with('/')
    {
        let _ = response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"malformed request",
            "",
        );
        return;
    }
    if fields[0] != "GET" {
        let _ = response(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"use GET",
            "Allow: GET\r\n",
        );
        return;
    }
    let path = fields[1].split('?').next().unwrap_or_default();
    // Match the complete token path before exposing any frames or diagnostics.
    let Some(route) = path.strip_prefix(prefix) else {
        let _ = response(&mut stream, "404 Not Found", "text/plain", b"not found", "");
        return;
    };
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
            let _ = response(
                &mut stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&frames.stats()).unwrap(),
                "",
            );
        }
        "/frame.jpg" => {
            let jpeg = frames.inner.lock().unwrap().jpeg.clone();
            if jpeg.is_empty() {
                let _ = response(
                    &mut stream,
                    "503 Service Unavailable",
                    "text/plain",
                    b"no frame available",
                    "Retry-After: 1\r\n",
                );
            } else {
                let _ = response(&mut stream, "200 OK", "image/jpeg", &jpeg, "");
            }
        }
        "/stream" => {
            let _ = write_mjpeg(&mut stream, &frames, &stop);
        }
        _ => {
            let _ = response(&mut stream, "404 Not Found", "text/plain", b"not found", "");
        }
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

fn write_mjpeg(stream: &mut TcpStream, frames: &FrameState, stop: &AtomicBool) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    );
    write_parts(stream, &[header.as_bytes()], IO_DEADLINE)?;
    let _viewer = Viewer::new(frames);
    let mut last = 0;
    while !stop.load(Ordering::SeqCst) {
        let data = frames.inner.lock().unwrap();
        let (data, _) = frames
            .tick
            .wait_timeout_while(data, Duration::from_millis(250), |d| {
                d.generation == last && d.ended.is_none() && !stop.load(Ordering::SeqCst)
            })
            .unwrap();
        if data.ended.is_some() || stop.load(Ordering::SeqCst) {
            break;
        }
        if data.generation == last {
            drop(data);
            if peer_closed(stream) {
                break;
            }
            continue;
        }
        last = data.generation;
        let jpeg = data.jpeg.clone();
        drop(data);
        if jpeg.is_empty() {
            continue;
        }
        let part = format!(
            "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        );
        write_parts(stream, &[part.as_bytes(), &jpeg, b"\r\n"], IO_DEADLINE)?;
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
    use super::*;
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
        assert!(!html.contains("Shared tile"));
        assert!(html.contains("#stage.ended #disconnected"));
    }
}
