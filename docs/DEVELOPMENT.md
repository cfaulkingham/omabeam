# Developing OmaBeam

## Build and run

Use a current stable Rust toolchain with edition 2024 support. Linux builds
need a C/C++ toolchain, Clang, CMake, NASM (x86 H.264 assembly), pkg-config, Fontconfig, FreeType, Wayland,
libxkbcommon, libxcb, and OpenSSL development libraries. CI lists the Ubuntu
package names.

```bash
cargo build --locked
cargo run --locked
```

Real capture requires Hyprland. macOS builds support synthetic demos and
native UI review; they do not capture the Mac desktop:

```bash
cargo run --locked -- --demo --port 0
cargo run --locked -- --demo-picker
```

`--demo` sends generated frames over localhost through the real encoder and
HTTP/WebRTC server. `--demo-picker` uses synthetic sources with sharing disabled.

## Source layout

| Path | Responsibility |
| --- | --- |
| `src/app/` | Picker, preview worker, branding, settings, nearby-device UI |
| `src/live/` | Browser viewer, HTTP delivery, stream settings and state |
| `src/live/webrtc.rs`, `src/live/webrtc/encoder.rs` | LAN ICE/DTLS/RTP peers and shared software H.264 encoder |
| `src/localsend.rs` | Discovery and viewer-link sending |
| `src/hypr/` and `src/hypr.rs` | Hyprland IPC and picker positioning |
| `src/portal.rs` | Portal selection and stdout protocol |
| `crates/omabeam-capture/` | Wayland capture, encoding, region selection |
| `omarchy-plugin/` | Bar widget, panel, session model, native launcher |
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

Area selection uses layer-shell overlays. Drag with a mouse or one touch;
Escape or another button cancels. Space moves the selection and Shift makes
it square. Cross-monitor selections are clipped to the monitor containing
their top-left corner. Coordinates are output-relative.

Previews use one bounded background worker, stay in memory, and never start
a listener. Changing the selection or settings invalidates the old preview.
Live sharing requires a valid preview; portal mode can return a valid source
when a local preview is unavailable.

Live sessions reuse their capture connection and buffers. JPEG streams default
to logical output resolution. `--native-pixels` (also selected by Crisp text)
uses the captured pixel dimensions; `--width` caps either mode without
upscaling its pixel grid. Preview and live encoding use the same mode and width
limit. PNG screenshots keep capture resolution. The backend uses CPU-accessible shared memory; GPU
encoding, HDR color management, and DMA-BUF-only sources are unsupported.

All viewer routes require the session's 128-bit URL token. The server limits
concurrent clients to 64 and bounds request/response time. Pausing disconnects
that viewer. Capture failure clears the image and exposes diagnostics for
30 seconds before the background process exits.

State and logs live under `$XDG_RUNTIME_DIR/omabeam/`. The directory is created
0700; files are 0600, opened `O_NOFOLLOW`, and capped. `XDG_RUNTIME_DIR` is
required (no `/tmp` fallback). Incomplete or oversized session files are
rejected. Ended-session details remain until a new share or `--stop`.
Hyprland queries use its command socket directly. `--stop` signals only a
process whose pidfd still matches the recorded start time, uid, and `--live`
or `--demo` command.

```bash
target/debug/omabeam --hypr monitors
target/debug/omabeam --hypr clients
target/debug/omabeam --fps 30 --quality 72 --width 1280 --cursor
target/debug/omabeam --native-pixels --quality 90
target/debug/omabeam --live output DP-1 --bind 127.0.0.1 --port 9847
target/debug/omabeam --live region DP-1 20 30 400 300 --fps 15
```

### Stream diagnostics

The token-protected `/s/TOKEN/stats` response retains the original stream
fields and adds `diagnostics` plus `clients` for active stream connections.
`--status` and `live.json` include aggregate `diagnostics` and optional `webrtc` objects,
keeping the status file within its 8192-byte limit. Old status files without
diagnostics remain readable. No IP addresses or device identifiers are stored
in the viewer counters; IDs identify connections within the current session.

- `fps` counts captured frames published to the stream, not frames displayed remotely.
- `native_pixels`, `capture_width/height`, `logical_width/height`, and
  `jpeg_bytes` describe the latest capture and its cached JPEG (zero until requested in RTC-only mode). Existing `width/height`
  describe the actual stream dimensions after the width cap.
- `capture_wait_ms` times successful calls to the capturer, including waiting
  for compositor damage and copying/converting pixels. It is not a GPU capture
  latency measurement. Calls that return no changed frame add no sample.
- `encode_ms` includes resizing, alpha compositing, and JPEG encoding.
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

`--webrtc` opts into software OpenH264, built from source into the binary;
there is no runtime FFmpeg or encoder download. `str0m` supplies ICE-lite,
DTLS-SRTP, RTP H.264 packetization, retransmission, and feedback. Only one
receive-only video track, constrained-baseline H.264, and packetization mode
1 are negotiated. Audio, data channels, incoming media, and remote control
are outside this mode.

Capture publishes one latest raw frame. With only WebRTC viewers, JPEG
encoding is deferred until a snapshot/fallback asks for it. One encoder
worker sends frames through a capacity-one queue; on a dropped encoded frame
it forces an IDR before delivering another delta. New peers and PLI/FIR
request an IDR even on a static screen. Static content repeats once a second.
Resolution changes reinitialize OpenH264. I420 requires even dimensions;
odd right/bottom edges are extended by one pixel. The encoder supports up to
3840×2160 (or portrait), at least 16 pixels per edge, and at most 60 FPS; errors disable WebRTC for that share and leave JPEG
available. A new share can retry the encoder.

One network worker multiplexes at most eight peers. It drains str0m outputs
after every input or media write. UDP sockets bind concrete addresses within
`--bind` (default port 9848); no discovery server, STUN, TURN, or arbitrary
external relay is used. Pending offers expire after 12 seconds. The signaling
command queue holds at most 16 commands; JSON bodies are limited to 64 KiB
with the HTTP five-second deadline. Offer/close endpoints require the share
token, JSON, and matching Origin/Host when Origin is present. A separate
random identifier controls each peer's close request.

Each peer's retransmission cache is capped at 512 packets. A send queue over
2 MiB or 250 ms disconnects the peer instead of accumulating video latency.
An encoded frame over 2 MiB disables H.264 for the share. Viewer negotiation
has a ten-second first-playback deadline, then falls back to JPEG; stalled
decoding also falls back. Pause, page exit, source loss, and stale async
answers release peer resources.

The `webrtc` stats object reports software encoder timings, dimensions,
encoded FPS/frames/keyframes, dropped frames, connected/pending peers,
failures, and UDP bytes/rates (including DTLS/RTCP and retransmissions).
Existing `diagnostics` delivery counters remain JPEG-only. Browser
`RTCPeerConnection.getStats()` supplies actual decoded FPS/codec, receive
bitrate, loss, jitter, and average decode/jitter-buffer time. These are not
synchronized capture-to-display latency measurements. HTTP signaling remains
unencrypted, so DTLS-SRTP does not authenticate the link against an active
network attacker; use this on a trusted LAN.

## Automated checks

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo build --locked
python3 tests/firewall.py
python3 tests/packaging.py
python3 -m venv /tmp/omabeam-tests
/tmp/omabeam-tests/bin/pip install 'Pillow>=10,<13' 'playwright>=1.50,<2' 'PySide6-Essentials>=6.8,<6.11'
/tmp/omabeam-tests/bin/python -m playwright install chromium chrome
/tmp/omabeam-tests/bin/python tests/smoke.py --binary target/debug/omabeam --browser
/tmp/omabeam-tests/bin/python tests/webrtc.py --binary target/debug/omabeam
/tmp/omabeam-tests/bin/python tests/omarchy_ui.py --screenshots target/omarchy-qa
```

The WebRTC test needs a Chromium/Chrome build exposing H.264 in
`RTCRtpReceiver.getCapabilities("video")`. It prefers system Chrome/Chromium;
set `OMABEAM_TEST_CHROMIUM` or `--browser-executable` to select another build.
Playwright's bundled Chromium can lack H.264 and will exercise fallback only.

Rust tests cover capture protocols, stable window identity, errors, buffer
reuse, HTTP delivery, and IPC. Browser tests cover playback controls and
source loss. QML tests render the actual UI with a simulated shell/process
boundary. Packaging tests use temporary homes and mock desktop commands.
Firewall tests simulate UFW, sudo, and network discovery; they cover rule order,
subnet and protocol matching, scoped opening, repeat runs, and failure warnings
without reading or changing the host firewall.

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
popup positioning, panel switching, clipboard, and nearby-device delivery.
Those need the real desktop and receiving app.

Linux CI also runs output and region capture in a private headless Sway:

```bash
sh tests/linux-capture.sh target/debug/omabeam
```

See [Installation and releases](../RELEASING.md) for release validation.
