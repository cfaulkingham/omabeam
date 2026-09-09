# Developing OmaBeam

## Build and run

Use a current stable Rust toolchain with edition 2024 support. Linux builds
need a C/C++ toolchain, Clang, CMake, pkg-config, Fontconfig, FreeType, Wayland,
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
HTTP server. `--demo-picker` uses synthetic sources with sharing disabled.

## Source layout

| Path | Responsibility |
| --- | --- |
| `src/app/` | Picker, preview worker, branding, settings, nearby-device UI |
| `src/live/` | Browser viewer, HTTP delivery, stream settings and state |
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

Live sessions reuse their capture connection and buffers. JPEG streams use
logical output resolution with an optional width limit. PNG screenshots keep
capture resolution. The backend uses CPU-accessible shared memory; GPU
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
target/debug/omabeam --live output DP-1 --bind 127.0.0.1 --port 9847
target/debug/omabeam --live region DP-1 20 30 400 300 --fps 15
```

## Automated checks

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo build --locked
python3 tests/packaging.py
python3 -m venv /tmp/omabeam-tests
/tmp/omabeam-tests/bin/pip install 'Pillow>=10,<13' 'playwright>=1.50,<2' 'PySide6-Essentials>=6.8,<6.11'
/tmp/omabeam-tests/bin/python -m playwright install chromium
/tmp/omabeam-tests/bin/python tests/smoke.py --binary target/debug/omabeam --browser
/tmp/omabeam-tests/bin/python tests/omarchy_ui.py --screenshots target/omarchy-qa
```

Rust tests cover capture protocols, stable window identity, errors, buffer
reuse, HTTP delivery, and IPC. Browser tests cover playback controls and
source loss. QML tests render the actual UI with a simulated shell/process
boundary. Packaging tests use temporary homes and mock desktop commands.

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
