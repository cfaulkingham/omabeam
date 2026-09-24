# Developing OmaBeam

For installation and everyday use, start with the [README](../README.md).

- [Build and run](#build-and-run)
- [Installer behavior and firewall checks](#installer-behavior)
- [Source layout](#source-layout)
- [Capture and session behavior](#capture-and-session-behavior)
- [Stream defaults and saved settings](#stream-defaults-and-saved-settings)
- [Stream diagnostics](#stream-diagnostics) and [encoder checks](#encoder-checks)
- [Automated checks](#automated-checks) and [Linux desktop testing](#test-on-a-linux-desktop)
- [Extended desktop design](EXTENDED-DESKTOP.md)
- [Release packaging and publishing](../RELEASING.md)

Native Google Cast video mirroring is implemented on the development branch.
See [build commands and qualification status](NATIVE-CAST-STATUS.md) and the
[design and release gates](NATIVE-CAST.md). The status document records
user-confirmed Hyprland extended-display playback at 720p on a Nest Hub and
1080p on an E65-E1 TV, plus the remaining performance and release checks.

## Build and run

Use a current stable Rust toolchain with edition 2024 support. Linux builds
need a C/C++ toolchain, Clang, CMake, NASM (x86 H.264 assembly), pkg-config, Fontconfig, FreeType, Wayland,
libxkbcommon, libxcb, and OpenSSL development libraries. CI lists the Ubuntu
package names. The hardware helper also needs FFmpeg development libraries
(`libavcodec`, `libavutil`, `libavformat`). Arch/Omarchy supplies these in
`ffmpeg`; macOS development can use Homebrew's `ffmpeg`.

```bash
cargo build --locked
cargo run --locked
```

The workspace builds `omabeam` and the adjacent `omabeam-encoder` helper.
`cargo build -p omabeam --locked` builds just the app with software encoding
and does not require FFmpeg. Only the helper links to system FFmpeg libraries.

Real capture requires Hyprland. macOS builds support synthetic demos and
native UI review; they do not capture the Mac desktop:

```bash
cargo run --locked -- --demo --port 0
cargo run --locked -- --demo-picker
```

`--demo` sends generated frames over localhost through the real encoder and
HTTP/WebRTC server. `--demo-picker` uses synthetic sources with sharing disabled.
Extended desktop additionally requires Hyprland's Lua monitor configuration
and a working headless output backend; it is unavailable in portal-picker and
screenshot modes.

## Installer behavior

User-facing installation, update, and removal instructions live in the
[README](../README.md#install). Omarchy's plugin installer clones and validates
Git repositories; it does not run build hooks or download release assets.
Source installations therefore need an explicit native build. Compiled bundles
include the native app.

The plugin ID is `io.github.cfaulkingham.omabeam`. The default installation
root is `${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/io.github.cfaulkingham.omabeam/`.
Its `omarchy-plugin/omabeam` launcher runs the adjacent `native/bin/omabeam`;
the installer does not add a command to `PATH`. `--backend-only` builds there
without editing desktop configuration or restarting the shell. The full
installer also enables the bar plugin and adds marked Hyprland window rules
and a shortcut.

`--with-cast` additionally builds the pinned Open Screen helper and installs
its notices under `licenses/cast`. It needs Python 3, Git, pkg-config, and the
C/C++ build dependencies, and downloads several gigabytes to
`${XDG_CACHE_HOME:-$HOME/.cache}/omabeam/cast`. Browser shares do not need the
Cast helper.

### Firewall checks

Full and `--backend-only` installs check the default sharing ports: TCP 9847
for the viewer page and JPEG, and UDP 9848 for H.264/WebRTC. If UFW is active
and those ports are blocked for the detected LAN, the installer prompts for
sudo and adds only the missing allows. It prepends them before conflicting
user rules and verifies access afterward. A declined password or unverifiable
firewall does not fail an otherwise successful install.

Check again without building, installing, or changing rules:

```bash
./install.sh --check-ports
./install.sh --check-ports --subnet 192.168.2.0/24
```

The check infers IPv4 subnets on default-route interfaces using `ip`, or uses
the explicit `--subnet` CIDR. Check-only mode can request sudo authentication
in a terminal but does not write rules. To allow the two ports from a
specific viewer network, and fail if they cannot be verified:

```bash
./install.sh --check-ports --open-firewall 192.168.2.0/24
```

Replace the example with your viewer subnet. `--open-firewall CIDR` also works
with a full or backend-only install. These rules persist across restart and
plugin removal. The installer never enables a disabled firewall or changes
its default policy. Invalid or unrestricted `/0` CIDRs are rejected before
installation starts.

Explicit check/open requests exit nonzero when access is blocked or unverified.
If an update partly succeeds, added rules remain and the warning explains that
setup is incomplete. Rerunning skips access that is already allowed.

This checks UFW incoming user rules, not listening sockets or end-to-end packet
delivery. The app need not be running. Unsupported firewall managers, missing
permissions, and ambiguous rules produce warnings; custom nftables/iptables
rules, Wi-Fi client isolation, or another device's firewall may still prevent
viewing. After the check, start a share and test its link from another device.
Custom `--port` or `--webrtc-port` values need their own rules. Blocked UDP can
cause H.264 playback timeouts while the HTTP page and JPEG fallback still work.

Native Cast uses different traffic: mDNS discovery on UDP 5353, an outgoing TLS
connection to the receiver's advertised TCP port, and negotiated UDP media and
feedback. `--check-ports` only diagnoses browser ports; opening 9847/9848 does
not diagnose Cast. See the [Cast network checks](NATIVE-CAST-STATUS.md#network-checks).

## Source layout

| Path | Responsibility |
| --- | --- |
| `src/app/` | Picker, preview workers, branding, settings, nearby-device UI |
| `src/live/` | Browser viewer, HTTP delivery, stream settings and state |
| `src/live/desktop.rs` | Extended-display client lease and capture-worker resize transactions |
| `src/live/webrtc.rs`, `src/live/webrtc/encoder.rs` | Browser LAN ICE/DTLS/RTP peers and encode worker |
| `src/live/h264.rs` | Shared adaptive H.264 encoder and frame conversion |
| `src/live/cast.rs`, `crates/omabeam-cast/` | Cast session/capture ownership and bounded helper IPC |
| `native/omabeam-cast/` | Pinned Open Screen discovery, authentication and native mirroring transport |
| `crates/omabeam-encoder/` | Bounded pipe protocol and isolated FFmpeg hardware encoder helper |
| `src/localsend.rs` | Probe-only discovery, PIN-aware viewer-link sending, and the saved LocalSend identity |
| `src/hypr/` and `src/hypr.rs` | Hyprland IPC and picker positioning |
| `src/hypr/desktop.rs`, `src/app/desktop.rs` | Extended output ownership, recovery, placement, and picker controls |
| `src/portal.rs` | Portal selection and stdout protocol |
| `crates/omabeam-capture/` | Wayland capture, encoding, region selection |
| `omarchy-plugin/` | Bar widget, panel, session model, native launcher |
| `src/qr.rs` | Local share QR module grid for the bar panel |
| `vendor/localsend/` | Reviewed protocol dependency and its own tests |
| `scripts/package-plugin.py` | Linux release archive assembly |
| `tests/` | Streaming, compositor, QML, and packaging checks |

The root manifest loads the Omarchy plugin. The browser page is compiled into
the native binary. The bar and panel use Omarchy's existing Quickshell process.

## Capture and session behavior

Window capture uses stable foreign-toplevel identifiers with
`ext-image-copy-capture-v1`. Monitors support that protocol and
`wlr-screencopy-unstable-v1`. A lost or unsupported window never falls back to
capturing overlapping desktop pixels.

With ext-image-copy-capture, OmaBeam reads and converts only the rows the
compositor reports as damaged. Region and window-rectangle shares publish
nothing when the damage lies outside the selection. A whole frame is read for a
new buffer, when a frame carries no damage information, on rotated or flipped
outputs, and about once a second. Hyprland 0.56 reports whole-frame damage for
every capture and copies the whole buffer, so there output shares read and
convert every frame in full (the image is handed over without an extra copy,
except for the once-a-second refresh frame), and region and window-rectangle
shares read only their rows but publish every rendered frame. wlroots-based
compositors with this protocol (Sway 1.11 and later) report real damage. An ext
compositor may hold a capture until the screen changes, and neither Hyprland nor
wlroots completes one on a static screen except a new session's first frame. A
static ext share can therefore go without new frames, and a change the
compositor does not report as damage appears only with the next reported damage.

With wlr-screencopy, a region share reads back only the region. After the
first frames, each capture waits for damage (protocol version 2 or later),
but a capture still waiting a second after the last frame is replaced by a
plain copy. A static screen therefore still yields about one frame per
second, and changes the compositor never reports show up within a second. A
wlr frame that waited across an output rotation, mode change, or scale change
is dropped instead of shown, and a fresh copy follows at once. A wlr frame
that fails while waiting, for example after a switch to a smaller mode, is
retried with a plain copy.

Set `OMABEAM_CAPTURE_FULL_DAMAGE=1` to copy and convert whole frames every time
if a compositor under-reports damage; this changes nothing on Hyprland, which
already reports whole frames. When the compositor rejects a capture buffer
without sending new constraints, OmaBeam retries with a new buffer after 2
seconds and ends the share with an error after three failed retries, instead
of freezing the picture. Failed wlr frames count toward the same three
retries.

Live shares, Cast, stream previews, and window thumbnails capture opaque
frames (premultiplied color over black), which is what JPEG and H.264 show
anyway; PNG screenshots and the previews in screenshot and portal-picker modes
do not. With ext-image-copy-capture OmaBeam picks XRGB8888 whenever it is
offered. Hyprland 0.56 offers it for windows as well as ARGB8888, so Hyprland
window frames are already opaque in every mode, screenshots included. Opaque
capture matters on compositors that offer only alpha formats.

Area selection uses layer-shell overlays. Drag with a mouse or one touch;
Escape or another button cancels. Space moves the selection and Shift makes
it square. Cross-monitor selections are clipped to the monitor containing
their top-left corner. Coordinates are output-relative. An overlay redraws
only when the selection touches its monitor, at most once per compositor
frame, rewriting and damaging only the rows that changed.

Previews use two bounded background workers, one for the selected source and
one for window thumbnails, so a slow window cannot hold up the preview.
Thumbnails are requested one at a time, at most every 750 ms, and each window
at most every 10 seconds. One Wayland connection is reused for thumbnails and
reopened after an error or after the compositor closed it while idle; each
thumbnail's buffers are released after
capture. A preview worker that stops unexpectedly shows an error and restarts
with the next preview request (within about 5 seconds), or at once with Retry
preview. Previews stay in memory and never start a listener. Changing the
selection or settings invalidates the old preview.
Live sharing requires a valid preview; Extend desktop shows a proposed layout
before creating a new output. Portal mode can return a valid source
when a local preview is unavailable. Hiding and restoring the picker around a
capture or share run in background tasks, and the picker shows Working… until
they finish; only the Hyprland snapshots taken while the picker opens still
load on the UI thread. Desktop notifications are sent without waiting for
`notify-send`.

Live sessions reuse their capture connection and buffers. JPEG streams default
to logical output resolution. `--native-pixels` (also selected by Crisp text)
uses the captured pixel dimensions; `--width` caps either mode without
upscaling its pixel grid. Preview and live encoding use the same mode and width
limit. PNG screenshots keep capture resolution. Capture uses CPU-accessible
shared memory. Hardware encoding uploads these frames to the GPU; capture,
resizing, and RGB-to-YUV conversion still use the CPU. HDR color management
and DMA-BUF-only sources are unsupported.

All viewer routes require the session's 128-bit URL token, compared in
constant time. Until a request presents the token, its connection holds one of
32 pre-authentication slots, at most 8 per IPv4 address or IPv6 /64 (an
IPv4-mapped peer counts as IPv4), taken at accept before any byte is read, and
must send its request header within 2 seconds. With the token it moves to the
64-connection client limit, and a request body gets 5 seconds. Connections
over either limit get 503. Four IPv4 addresses on one host can therefore fill
every pre-authentication slot, and all link-local peers (`fe80::/64`) share one
per-host allowance, as do viewers behind one shared address (a NAT, VM, or
container); enough of those polling at once get 503. Error responses end with
a clean close (FIN, then up to 200 ms spent discarding the unread request)
instead of a connection reset, except a 503 sent while 16 rejections are
already waiting, which is written directly and may end in a reset.
Pausing disconnects that viewer.

Capture failure clears the image and exposes diagnostics for 30 seconds before
the background process exits. During that time `/stream` and `/frame.jpg` answer
410 Gone, and `/stats` still answers with `state: "ended"` and the error. A
share that stops normally also answers 410 from those routes, while `/stats`
reports `state: "live"` until the process exits. `/frame.jpg` answers 503 with
`Retry-After: 1` only before the first frame. On an extended-desktop share the
display-lease check comes first, so a page that does not hold the lease gets 409
rather than 410 from those routes and the WebRTC routes; its status poll still
reports the end.

A captured frame whose JPEG cannot be encoded is skipped instead of ending the
share, and a failed on-demand encode makes `/frame.jpg` answer 500 while
`/stream` skips that frame and continues. `live.log` records these at most
every 10 seconds, and `diagnostics.encode_errors` in `/stats` and `live.json`
counts them once per frame. A transient accept failure (out of descriptors,
buffers, or memory) does not end the share either: the server logs it at most
every 10 seconds and retries after a backoff of 50 ms, doubling up to 1
second. Per-connection network errors that accept reports are retried at once.
Only an unusable listener (EBADF, EINVAL, ENOTSOCK) ends the share.

State and logs live under `$XDG_RUNTIME_DIR/omabeam/`. The directory is created
0700; files are 0600 and opened `O_NOFOLLOW`. `live.json` and `display.json` are
capped at 8 KiB; `live.log` has no size cap, but repeated failures are logged
at most every 10 seconds. `XDG_RUNTIME_DIR` is
required (no `/tmp` fallback). Incomplete or oversized session files are
rejected. Ended-session details remain until a new share or `--stop`. A share
counts as running only while its recorded pid still has the recorded start
time, so a crashed share whose pid was reused does not block new shares, and
`--status` exits 1 for it. `--stop` removes any leftover `live.json` once it
holds the session lock. `live.json` is rewritten at most once a second;
changes to state, error, URL, title, source, viewer count, extended-display
state, or Cast connection and readiness are written at once. A failed write
(out of descriptors or space, an I/O error) does not end the share: it is
logged to `live.log` at most every 10 seconds and retried every 250 ms, so a
permanently unwritable runtime directory leaves a running share with a stale
`live.json`. Only a status too large for the 8 KiB limit is fatal.

Hyprland queries use its command socket directly. `--stop` signals only a
process whose pidfd still matches the recorded start time, uid, and `--live`,
`--demo`, `--cast`, `--cast-demo` or `--cast-test` command. Replacing the plugin binary while a share is running
leaves `/proc/<pid>/exe` as `omabeam (deleted)`; that still matches. While a
share is still starting and has not written `live.json`, `--stop` finds it
through `session.lock`, where every share process records its pid and start
time, and signals it only while the lock is held and the same checks pass. The
record must be exactly `pid starttime`: decimal digits without signs or
leading zeros, one space, at most one trailing newline, both non-zero. A share
checks for a stop after recovery, after creating the extended display, after
capture setup, and after the first frame; it then exits 0 with "Stopped
before the share started." and removes a display it had created. A Cast takes
the lock and recovers before it spends about 5 seconds discovering receivers,
so `--stop` also ends a Cast that is still discovering: discovery ends early
and the Cast exits the same way before it connects. It checks again before its
first status write and before creating its display. A share started while a
Cast discovers fails at once with "already running or starting".

`--stop` waits up to 10 seconds (`STOP_GRACE` in `src/live/status.rs`) for a
signaled share to exit before SIGKILL. The first SIGINT, SIGTERM, or SIGHUP a
share receives, not only one from `--stop`, arms a 6-second budget
(`TEARDOWN_BUDGET` in `src/hypr/ipc.rs`) when the share notices it, and all of
its remaining Hyprland IPC shares that budget. Each request waits for the
smaller of its usual 5 seconds and what is left, and fails at once when
nothing is left. A compile-time check keeps the budget plus 2 seconds for
noticing the signal and joining capture below the grace.
Connecting to the Hyprland socket, and joining a capture thread that is
reopening capture after a resize, are not bounded by it. Recovery (from
`--stop` or a new share) has its own 6-second budget, so `--stop` takes about
16 seconds at worst. A compositor too slow for the budget leaves the extended
display and its record, even after a terminal Ctrl-C, and the next `--stop`
or share finishes the removal.

A hang-up (SIGHUP, for example closing the terminal of `omabeam --live …`)
ends a share like SIGINT and SIGTERM. Its handler first points stdout and
stderr at /dev/null, but only when they are not regular files (a terminal,
pipe, or socket): writes to those fail after a hang-up (EIO, or EPIPE once the
reader is gone, since SIGPIPE is ignored), and `eprintln!` would panic halfway
through cleanup. A regular file, such as the bar's `live.log` or
`> file 2>&1`, keeps logging. SIGINT and SIGTERM keep stdio, so Ctrl-C still
shows cleanup errors. On Linux, a share started with SIGHUP ignored (`nohup`)
keeps ignoring it; elsewhere a hang-up always stops the share.

The bar watches `$XDG_RUNTIME_DIR/omabeam/` and, while idle, refreshes about
300 ms after the first change of a burst. It also polls `--status` every 30
seconds while idle, every 2 seconds while sharing, and every second while its
panel is open. Restarting the shell or reloading the plugin signals only
bounded commands such as `--stop`, never an open picker or send window. When a
failed command printed to stderr, its message ends with a short, sanitized
excerpt in parentheses. The bar gives `--stop` 25 seconds; a stop that
outlasts that is not reported as a failure, and the next status read decides.

Extended desktop sessions create a random `OMABEAM-` output through socket1,
pin existing monitors to their current coordinates once at creation, configure
the extra output with `eval hl.monitor(...)`, verify its layout, and capture
that named output. Pinning keeps Omarchy's catch-all `position = "auto"` from
shoving physical displays aside when the extra screen is attached. A session
lock serializes startup and recovery. A private `display.json` records the
exact output and compositor instance before creation.
Failed starts, graceful termination, and recovery all remove the owned output
after capture stops, then reload Hyprland's configuration so the user's own
monitor rules replace the temporary pins. Recovery after a forced kill uses
that record, never a prefix scan of monitors; a recorded compositor whose
socket is missing or refuses connections is treated as already exited. If
removal fails, the record remains for `omabeam --stop` to retry.
The small `session.lock` file remains in the runtime directory; its inode must
not be removed while a session might hold a lock. It holds the pid and start
time of the last share process that took it.

Each extended display has one in-memory browser lease, independent of its
media transport. `/desktop/claim`, `/desktop/heartbeat`, `/desktop/release`, and
`/desktop/size` accept bounded same-origin JSON under the share token. A random
tab identity survives refresh in sessionStorage; a separate random page identity
prevents a duplicated tab from replacing a live page. The page renews a 15-second
lease. Page exit/pause invalidates its media immediately while reserving the
tab's reconnection identity for that grace period. Both JPEG routes and WebRTC
signaling require `?viewer=PAGE_ID`; ongoing JPEG and RTC delivery also checks
the lease. Stats contain display configuration and occupancy, never either ID.
Lease ids are compared in constant time. A heartbeat or size request for the
page's own unreleased lease that has lapsed, when nobody has claimed the
display since, answers `412 Precondition Failed` and renews nothing; every
other unauthorized request answers 409. On a 412 the viewer drops ownership
without the in-use overlay, claims again through `/desktop/claim`, and then
resends any size the host refused while the lease had lapsed.

The viewer hides the local pointer after two idle seconds, including the
fullscreen controls on an extra display. Owning an extended display requests
fullscreen immediately; browsers that require a gesture retry when the picture
is tapped. Match-this-device stays available in the fullscreen overlay.
The viewer reads the host's Omarchy palette when opened; a theme change needs
a page reload. Capture slows to about one frame per second when nobody watches.

Client sizing is opt-in and debounced. It uses the viewer stage's CSS dimensions
and the nearest supported desktop density (1× or 2×), rounds to even pixels,
and caps both edges for JPEG/H.264 compatibility. The host applies one size
rule, `DesktopConfig::validate`, to CLI, picker, and viewer sizes: at least
640×480, long edge at most 3840, short edge at most 2160, and divisible by the
desktop scale. Requests run through one bounded pending resize slot. The
capture worker reconfigures only the owned output,
recomputes its placement against the other active monitors, reopens capture,
and verifies the captured dimensions before committing the new mode. Failed
changes restore and recapture the previous mode; failed restoration ends the
share. Disconnecting leaves the last applied mode intact. Disabling matching
restores the original host display and encoding settings. A size choice that
was toggled, or refused with 412 while the lease had lapsed, stays owed until
the host answers a size request, so window or fullscreen changes during the
800 ms debounce reschedule it rather than drop it. Regular shares have no
lease requirement or sizing API.

```bash
target/debug/omabeam --hypr monitors
target/debug/omabeam --hypr clients
target/debug/omabeam --fps 30 --quality 72 --width 1280 --cursor
target/debug/omabeam --native-pixels --quality 90
target/debug/omabeam --live output DP-1 --bind 127.0.0.1 --port 9847
target/debug/omabeam --live region DP-1 20 30 400 300 --fps 15
```

### Stream defaults and saved settings

Ordinary shares default to 15 FPS, H.264/WebRTC with JPEG fallback, JPEG
quality 55, logical pixels without a width cap, cursor off, and a 4 Mbit/s
H.264 target. The default listener is `0.0.0.0`, with TCP 9847 for HTTP and UDP
9848 for WebRTC. The bitrate scales with frame rate up to 16 Mbit/s at 60 FPS;
an explicit bitrate is kept as-is.

The picker saves stream settings in
`${XDG_CONFIG_HOME:-$HOME/.config}/omabeam/settings.json`. Window, screen, and
area shares reuse them. Value-setting flags such as `--fps`, `--quality`,
`--width`, and `--jpeg` override saved settings for one run; no flag turns off
a remembered cursor, native pixels, or width cap.

| Preset | FPS | JPEG quality | Pixel detail | Maximum width |
| --- | --- | --- | --- | --- |
| Balanced | 15 | 55 | Logical | No limit |
| Crisp text | 15 | 90 | Native | No limit |
| Smooth motion | 60 | 55 | Logical | 1280 |

Entering extended-desktop mode selects native pixels, the host cursor, and no
width cap; leaving it restores the earlier pixel, cursor, and width settings.
Extended desktop defaults to 60 FPS, but an explicitly selected frame rate or
quality preset takes precedence. Match this device temporarily overrides pixel
detail and width limits to encode the display at native resolution. Disabling
matching restores the host's chosen display and encoding settings.

H.264 uses 4:2:0 color, which can soften fine colored text. Preview and browser
snapshots remain JPEG, and PNG screenshots keep capture resolution. Unsupported
H.264 sizes fall back to JPEG without silently reducing the chosen resolution.

### Stream diagnostics

The token-protected `/s/TOKEN/stats` response retains the original stream
fields and adds `diagnostics` plus `clients` for active stream connections.
`--status` and `live.json` include aggregate `diagnostics` and optional `webrtc` objects,
keeping the status file within its 8192-byte limit. Old status files without
diagnostics remain readable. No IP addresses or device identifiers are stored
in the viewer counters; IDs identify connections within the current session.

- `fps` counts captured frames published to the stream, not frames displayed remotely.
  Region and window-rectangle shares publish a frame only when damage touches
  the selection, so on compositors that report partial damage they can show a
  low `fps` while other parts of the screen change. Hyprland reports
  whole-frame damage, so there they publish every rendered frame.
- `native_pixels`, `capture_width/height`, `logical_width/height`, and
  `jpeg_bytes` describe the latest capture and its cached JPEG (zero until requested in RTC-only mode). Existing `width/height`
  describe the actual stream dimensions after the width cap.
- `capture_wait_ms` times successful calls to the capturer, including waiting
  for compositor damage and copying and converting pixels. With
  ext-image-copy-capture that is only the damaged rows, except for a new
  buffer, about once a second, on rotated outputs, or with
  `OMABEAM_CAPTURE_FULL_DAMAGE=1`. It is not a GPU capture
  latency measurement. Calls that return no changed frame add no
  sample, nor does a frame skipped because its JPEG could not be encoded.
- `encode_ms` includes resizing and alpha compositing when needed (an unscaled
  opaque frame goes straight to the JPEG encoder), plus JPEG encoding.
- `encode_errors` counts frames whose JPEG could not be encoded, once per
  frame: frames capture skipped and failed on-demand encodes.
- `send_ms` times completed multipart frame writes to the local socket.
  Timing objects contain `samples`, `p50`, and `p95` in milliseconds, using at
  most 256 samples from the last five seconds. Empty windows return null
  percentiles.
- `outgoing_mbps` and each client's `sent_fps` use approximately two seconds
  of 250 ms buckets. Buckets aggregate every write, even with many viewers.
- `bytes_sent` includes multipart frame headers, JPEG payloads, and partial
  writes before an error. It excludes the HTTP response header, snapshots,
  and diagnostics requests. Bytes accepted by the local socket do not prove
  remote receipt. `frames_sent` counts fully written frames.
- `frames_skipped` counts generation gaps between attempted sends on an
  established stream. Joining at the current image does not count older
  frames as skipped. One slow viewer's counts do not affect another viewer.
- `write_errors` counts failed multipart writes, including timeouts. An
  orderly disconnect detected while idle is not a write error.
- Each client reports the last completed frame's `frame_age_ms`, from encoding
  start to write completion, and `last_sent_ago_ms`. These do
  not include network transit, browser decoding, or presentation delay.

Session totals survive viewer disconnections. Per-connection rows are removed
on disconnect; timing and rate windows expire during idle periods. The browser
labels these as sender measurements and keeps the detailed panel collapsed
until requested.

### H.264 over WebRTC

H.264 / WebRTC is the default transport. `--jpeg` selects JPEG/MJPEG.
`--webrtc` remains accepted. Auto hardware selection uses built-in OpenH264 fallback.
The separate `omabeam-encoder` binary uses system FFmpeg libraries and vendor
drivers: NVENC first, then up to eight sorted VA-API render nodes on Linux;
VideoToolbox on macOS. Detection attempts a real encode at the share's actual
resolution, rather than trusting GPU names or FFmpeg's codec list. NVENC is
tried with planar YUV420P first, which needs no NV12 interleave, and falls
back to NV12 when the open or the first encode fails; VideoToolbox takes NV12
directly because it delays planar frames, and VA-API uploads NV12. All hardware
output is checked for Annex B framing, constrained-baseline SPS, and SPS/PPS
on requested IDRs. VideoToolbox software fallback is disabled.

The helper communicates only through inherited pipes with versioned, bounded
headers and frame sizes. On Linux the host asks for 1 MiB pipes; this is best
effort, and a per-user pipe limit keeps the default size. A frame goes out in
one vectored write, and the host waits only when a pipe is full or empty.
Driver stderr is continuously drained into a bounded tail. The host allows
five seconds for initial encoding and 750 ms for later frames; a failed,
malformed, or stalled helper is killed and reaped, and the host then waits up
to 200 ms for its stderr to drain, so `encoder_note` keeps the helper's last
line. Auto falls back to OpenH264 with a fresh IDR and records `encoder_note`;
it does not retry a failed device until the next share. Working hardware is
reopened on a size
change. Explicit `--encoder hardware` instead reports a WebRTC encoder error,
allowing the existing JPEG fallback. `--encoder software` skips the helper.
Missing/incompatible FFmpeg runtime libraries cannot prevent the main app from
starting, because it does not link them. Release helper binaries must match the
target system's FFmpeg ABI; source installs build against the local libraries.

`str0m` supplies ICE-lite,
DTLS-SRTP, RTP H.264 packetization, retransmission, and feedback. Only one
receive-only video track, constrained-baseline H.264, and packetization mode
1 are negotiated. Audio, data channels, incoming media, and remote control
are outside this mode.

Capture publishes one latest raw frame. With only WebRTC viewers, JPEG
encoding is deferred until a snapshot/fallback asks for it. One encoder
worker sends frames through a capacity-one queue; on a dropped encoded frame
it forces an IDR before delivering another delta. The encoder makes no
periodic or scene-change IDRs; hardware encoders keep a safety GOP of 60
seconds' worth of frames. A newly connected viewer gets an IDR at once, even
on a static screen and even when another viewer leaves at the same moment.
These join IDRs are not throttled; the eight-peer limit and each peer's DTLS
handshake bound them. PLI/FIR requests also get an IDR on a static screen, but
at most one per 500 ms after the previous IDR; a request inside that window
waits for its end instead of being dropped. Static content repeats once a
second. Capture owns frame pacing (capped at 60 FPS for H.264). A new capture
wakes the encoder immediately, without another frame-period wait. The default H.264 bitrate
is 4 Mbit/s at 15 FPS and scales linearly with FPS up to 16 Mbit/s, unless
`--h264-bitrate` or Advanced set it. OpenH264 does not skip frames to meet that
budget. NVENC uses VBR with `maxrate` at twice the target, still `tune=ull` and
zerolatency, so a motion burst can spend bits without adding encode delay. While the encoded queue
is occupied, capture keeps replacing the latest raw frame and the encoder waits;
it resumes with the newest capture when the network worker consumes the queue.
Only static repeats/keyframe retries have an encoder deadline. Notifications wake
frame waiters on capture, viewer joins and other connection changes, keyframe
requests, and queue consumption.
The network worker polls UDP sockets and a private wake socket for signaling and
encoded frames, bounded by str0m's next timer and a 100 ms shutdown/lease check.
While a peer has queued packets, it also waits for socket writability and for
that backlog's 250 ms limit.

Single-output/window capture hands over the decoded image without cloning it
when the compositor reports whole-frame damage, as Hyprland does (the
once-a-second refresh frame is still copied); with partial
damage it keeps the image to update in place and hands out a copy.
Shared-memory read storage, I420 conversion storage, the even-sized RGBA
staging buffer, the hardware helper's CPU frame (YUV420P for NVENC; NV12 for
VA-API and VideoToolbox) and packet, and the host's reply buffers are reused.
After the first-frame probe the helper reads planes straight into that frame;
only NV12 stages the chroma. Opaque, even-sized native frames convert directly
to I420 after a word-wide alpha check. Odd-sized frames, frames with
translucent pixels, and exact 2:1 reductions (HiDPI logical mode) are written
once into the staging buffer: odd edges repeat the last row and column, and
only rows with translucent pixels are composited over black. Exact 2:1
reductions use a 2×2 box filter, which is visibly sharper on text than the
triangle filter used by JPEG and by other ratios; other ratios still resize
and composite through `stream_rgb`. Live shares and Cast capture opaque frames
(see [Capture and session behavior](#capture-and-session-behavior)), so frames
from a source in an alpha format take the direct path as well. FFmpeg makes
reused frames writable before modifying them, preserving frames still held by
the encoder. Hardware encoding still uses the bounded I420 pipe protocol and
GPU upload; this is not a zero-copy GPU capture pipeline.
Resolution changes reinitialize OpenH264. I420 requires even dimensions;
odd right/bottom edges are extended by one pixel. The encoder supports up to
3840×2160 (or portrait), at least 16 pixels per edge, and at most 60 FPS; errors disable WebRTC for that share and leave JPEG
available. A new share can retry the encoder.

One network worker multiplexes at most eight peers. It drains str0m outputs
after every input or media write, feeding a due str0m timer during the drain
(at most twice) so a written frame's RTP leaves in the same pass. UDP sockets
bind concrete addresses within `--bind` (default port 9848); no discovery
server, STUN, TURN, or arbitrary
external relay is used. Pending offers expire after 12 seconds. The signaling
command queue holds at most 16 commands; JSON bodies are limited to 64 KiB
with the HTTP five-second deadline. Offer/close endpoints require the share
token, JSON, and matching Origin/Host when Origin is present. A separate
random identifier controls each peer's close request.

Each peer's retransmission cache holds 2048 packets, enough to repair a loss
anywhere in a 2 MiB frame. Sends never block the network worker: packets a
full socket refuses wait in that peer's outbox, in order, and are flushed
round-robin when the socket drains. Each flush starts one peer later than the
last, so no viewer always gets first claim on a congested shared socket. A
peer whose oldest queued packet has waited 250 ms, or with more than 4 MiB
(two maximum frames) queued, is disconnected instead of accumulating video
latency (`WebRTC viewer cannot keep up (N KiB queued for M ms)`). The check
uses each peer's own backlog, so one slow viewer does not disconnect the
others. An encoded frame over 2 MiB disables H.264 for the share. Viewer
negotiation has a ten-second first-playback deadline, then falls back to JPEG; stalled
decoding also falls back. A failure before the first frame plays is sticky:
the page stays on JPEG until the viewer selects Auto again. H.264 is tried
again on the next reconnect after a stream that played and then dropped, a
network failure during the offer exchange, a 409 offer response (a lapsed
display lease that the page claims again), or any failure while status polls
are failing (the host is unreachable, so the failure says nothing about
H.264). Failed status polls back off (1, 2, 4, then 5 seconds) without
stopping media. After 30 seconds without a successful poll, the viewer stops
media, releases its display lease, shows that it cannot reach OmaBeam, and
keeps polling; the next successful poll reconnects. After a shorter outage
that still outlasted the 15-second lease grace, a 412 heartbeat or size answer
makes the page claim the display again and restart media without the in-use
overlay. Pause, page exit, source loss, and stale async answers release peer
resources.

The `webrtc` stats object reports the selected encoder, fallback reason, timings, dimensions,
encoded FPS/frames/keyframes, dropped frames, connected/pending peers,
failures, and UDP bytes/rates (including DTLS/RTCP and retransmissions).
Existing `diagnostics` delivery counters remain JPEG-only. Browser
`capture_to_encode_ms` measures capture return to encoder start for changed frames
(static repeats are excluded). `convert_ms` covers scaling/color conversion,
`codec_ms` covers encoding including the helper exchange, and `send_queue_ms`
covers encoded-frame readiness to network-worker dequeue. `encode_ms` retains
the combined conversion/encoding meaning. Each reports bounded p50/p95 samples.
`RTCPeerConnection.getStats()` supplies actual decoded FPS/codec, receive
bitrate, loss, jitter, and decode/jitter-buffer time over the latest sampling
interval, with no timing sample on first connection, counter reset, or idle.
The viewer feature-detects `RTCRtpReceiver.jitterBufferTarget` and requests 0 ms;
unsupported or rejected hints leave playback working. Browsers may clamp this
target to their required minimum. These are not
synchronized capture-to-display latency measurements. HTTP signaling remains
unencrypted, so DTLS-SRTP does not authenticate the link against an active
network attacker; use this on a trusted LAN.
`--webrtc-port` changes the UDP port, with `0` selecting an available port.
A TCP-only SSH tunnel uses JPEG fallback.

### Encoder checks

Test encoder detection with generated frames, without capturing the desktop:

```bash
target/debug/omabeam --check-encoders
target/debug/omabeam --check-encoders --encoder hardware
target/debug/omabeam --check-encoders 2560x1440 3440x1440
```

For an installed plugin, substitute its `omarchy-plugin/omabeam` launcher.
The check uses a fresh encoder at 640×360, 1920×1080, and 3840×2160 by default,
or at the `WxH` sizes supplied. The JSON `sizes` array reports each result.
Top-level fields show the first size that did not use the GPU, so `hardware`
is true only if every size did. `--encoder hardware` fails the check if any
size fails; `--encoder software` skips hardware detection.

The adjacent `omabeam-encoder` helper needs system FFmpeg libraries, matching
GPU drivers, and device permissions. The main app still runs with software
encoding if the helper or its libraries are missing. See the
[hardware verification commands](#automated-checks) for browser checks and
helper-failure coverage.

### Nearby sending

Send nearby (`--send-link`) opens its own window with app id `omabeam-send`;
only class `omabeam` counts as the picker. The window is split: the QR code and share link are on the left, and
numbered LocalSend computers are on the right. With none in range, the right
side shows the LocalSend icon and the text "Open LocalSend on the client device." OmaBeam runs no LocalSend server,
so it discovers devices as one that cannot receive: targeted discovery,
subnet scans, and answers to announcements confirm peers with `GET /info` and
never register OmaBeam with them. A 401 to prepare-upload shows a masked PIN
field and retries with the typed PIN. A 204 counts as sent when the offer's
500-character preview held the whole link. An upload that has not finished
after 30 seconds is cancelled. OmaBeam keeps one ECDSA P-256 identity, mode
0600, in `$XDG_CONFIG_HOME/omabeam/localsend-identity.json` (`~/.config` when
unset) and replaces a file that is missing, malformed, oversized, or does not
match its key. Two send windows opened at the same moment on first run can
race to create that file; the last one saved is kept. Interoperability with
the official LocalSend apps (ECDSA identities, `GET /info` discovery, and 204
replies to prepare-upload) has not been verified.

## Automated checks

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo test --manifest-path vendor/localsend/Cargo.toml --features discovery --target-dir target/localsend
cargo build --locked
python3 tests/firewall.py
python3 tests/extended_desktop.py --binary target/debug/omabeam
python3 tests/hardware_encoding.py --binary target/debug/omabeam
python3 tests/packaging.py
python3 -m venv /tmp/omabeam-tests
/tmp/omabeam-tests/bin/pip install 'Pillow>=10,<13' 'playwright>=1.50,<2' 'PySide6-Essentials>=6.8,<6.11'
/tmp/omabeam-tests/bin/python -m playwright install chromium chrome
/tmp/omabeam-tests/bin/python tests/smoke.py --binary target/debug/omabeam --browser
/tmp/omabeam-tests/bin/python tests/webrtc.py --binary target/debug/omabeam
/tmp/omabeam-tests/bin/python tests/extended_viewer.py --browser-executable /path/to/chrome
/tmp/omabeam-tests/bin/python tests/omarchy_ui.py --screenshots target/omarchy-qa
```

The WebRTC test needs a Chromium/Chrome build exposing H.264 in
`RTCRtpReceiver.getCapabilities("video")`. It prefers system Chrome/Chromium;
set `OMABEAM_TEST_CHROMIUM` or `--browser-executable` to select another build.
Playwright's bundled Chromium can lack H.264 and will exercise fallback only.
It binds 0.0.0.0 and loads the viewer from the LAN address. With the macOS
application firewall on, each newly built binary needs permission to accept
incoming connections, and until then the test times out; run it on Linux.

The vendored LocalSend crate is outside the workspace, so
`cargo test --workspace` skips it; the `--manifest-path` command runs its
tests. Cargo resolves it separately and writes a `vendor/localsend/Cargo.lock`,
which git ignores. Its `event_backpressure` subnet-scan test expects Linux
loopback, where all of 127.0.0.0/24 answers, and fails on macOS, where only
127.0.0.1 does.

Some checks read `/proc`, so they run only on Linux and are compiled out or
skipped elsewhere: the `--stop` and process-identity tests in
`src/live/status.rs`, the stop-during-startup and reused-pid tests in
`tests/extended_desktop.py`, and the `nohup` check in `tests/smoke.py`.
`tests/smoke.py` also checks that a hang-up ends a share cleanly with its
terminal gone, that a log in a regular file keeps logging through it, and that
Ctrl-C keeps messages visible.

Two ignored tests print release-build timings on demand:
`cargo test --release -p omabeam-capture -- --ignored --nocapture` (4K decode
and JPEG) and
`cargo test --release --lib report_conversion_timings -- --ignored --nocapture`
(H.264 conversion).

On a GPU-equipped machine, require actual hardware success at every checked
size (640×360, 1920×1080, and 3840×2160 by default; software fallback does not
pass these checks):

```bash
python3 tests/hardware_encoding.py --binary target/debug/omabeam --require-hardware
/tmp/omabeam-tests/bin/python tests/hardware_encoding.py --binary target/debug/omabeam --require-hardware --browser
/tmp/omabeam-tests/bin/python tests/webrtc.py --binary target/debug/omabeam --encoder hardware
```

`--check-encoders WxH ...` checks other sizes. NVENC passes at all three
default sizes. On the Apple Silicon Mac used for development, VideoToolbox
returns delayed packets at 2560×1440 and above, which the helper rejects, so
Auto uses OpenH264 at those sizes and `--require-hardware` fails at 4K.

Rust tests inject helper crashes, stalls, and oversized responses, verify bounded
failure handling, and decode the independent software IDR after a backend switch.
The hardware browser check also terminates its own encoder helper and confirms
that browser decoding continues through the switch to software.

Rust tests cover capture protocols, stable window identity, errors, buffer
reuse, HTTP delivery, and IPC. Their fake compositor reports partial or
missing damage, holds idle captures, rejects buffers, and rotates, rescales,
or moves outputs mid-capture. Browser tests cover playback controls and
source loss. QML tests render the actual UI with a simulated shell/process
boundary, including the bar's directory watch, poll cadence, and stop timeout.
Packaging tests use temporary homes and mock desktop commands, including the
upgrade of an older picker-only window-rule block.
Firewall tests simulate UFW, sudo, and network discovery; they cover rule order,
subnet and protocol matching, scoped opening, repeat runs, and failure warnings
without reading or changing the host firewall.

`tests/extended_desktop.py` runs the actual CLI against a temporary compositor
socket. It checks rejected configuration, capture failure, failed cleanup,
forced termination, recovery in the original compositor session, lock
contention, and a signal during startup. On Linux it also checks that
`--stop` reaches a share still creating its display (it holds creation until
the stop signal is pending) and that a record naming a reused pid is stale. It
never edits the host desktop.

`tests/extended_viewer.py` uses the same private compositor socket with a Rust
fixture that supplies synthetic pixels to the production media server and
resize transaction. It checks competing devices/tabs, protected media routes,
refresh, transport changes, pause/resume, lease expiry, HiDPI/portrait sizing,
fullscreen, rejected-mode rollback, and restoration of the host size. It also
checks the 412 re-claim of a lapsed lease (heartbeat and size, mocked, and
after a real outage past the 15-second grace, so the run takes about a
minute), a size restore that survives a resize, and that the physical monitor
stays unchanged and the owned output is removed when the fixture stops.
`--serve` starts this fixture for manual UI review.

For real extended-display acceptance, run inside Hyprland with no active share:

```bash
/tmp/omabeam-tests/bin/python tests/extended_desktop_live.py --binary target/debug/omabeam --browser
```

This creates a temporary 1280×720 output, verifies unchanged physical monitor
geometry and real captured pixels, checks browser playback and reconnection,
and verifies removal after `--stop`. Artifacts go to `target/extended-review/live`.
A GPU-backed Hyprland session is required; a Docker container with only a
Pixman Sway backend cannot initialize Hyprland's allocator.

## Test on a Linux desktop

Inside Hyprland, replace `DP-1` with an output from `--hypr monitors`:

```bash
cargo run -p omabeam-capture --example smoke -- output DP-1 /tmp/omabeam-output.png
cargo run -p omabeam-capture --example smoke -- region DP-1 20 30 400 300 /tmp/omabeam-region.png
cargo run -p omabeam-capture --example smoke -- select /tmp/omabeam-selected.png
cargo test streams_a_jpeg_from_the_active_monitor -- --ignored
```

Verify overlapping windows, resize/close, secondary monitors, fractional
scaling, rotation, region cancellation, and portal selection. Also verify
popup positioning, panel switching, clipboard, and nearby-device delivery,
including a PIN-protected receiver and the official LocalSend apps. Those need
the real desktop and receiving app.

For capture changes, also verify damage-limited updates on a wlroots-based ext
compositor (typing, scrolling, and cursor-only moves with `--cursor`), a
region share while only other parts of the screen change, rotated and flipped
monitors, and wlr-screencopy region shares on Sway, including fractional scale
and rotating or re-moding an output mid-share. A share started with
`OMABEAM_CAPTURE_FULL_DAMAGE=1` gives a whole-frame reference picture to
compare against. In the picker, check that window thumbnails fill in while
the socket count in `/proc/PID/fd` stays constant, that closing one window
leaves the others alone, and that a new window gets its thumbnail. Area
selection should stay smooth on a 4K scale-2 monitor, overlays on monitors the
selection never touches should stay static, and fast drags, Shift, and Space
should leave no stale borders.

Linux CI also runs output and region capture in a private headless Sway:

```bash
sh tests/linux-capture.sh target/debug/omabeam
```

CI runs on `ubuntu-latest`, currently Ubuntu 24.04 with Sway 1.9, which offers
only wlr-screencopy. On its static desktop, `smoke.py`
gets its second stream frame from the wlr refresh about a second later. Sway
1.11 and later add ext-image-copy-capture, whose captures never complete on a
static screen after a session's first frame, so that read would stall; the
compositor, not OmaBeam's damage handling, holds the capture. If CI moves to
such a Sway, pin `ubuntu-24.04` or add periodic damage in
`tests/linux-capture.sh`.

See [Release validation](../RELEASING.md#validate-and-publish) for packaging and publishing checks.
