![OmaBeam — Your extra display. One simple link. Extend desktop and hardware-accelerated sharing for Omarchy and Hyprland.](docs/assets/omabeam-marketing.png)

# OmaBeam

**Turn any browser into an extra display. Share a window, screen, or area
over one local-network link.**

OmaBeam is a screen-sharing plugin for Omarchy and Hyprland. Use a tablet,
laptop, or another device as a real extra desktop, or share what is already
on screen. Preview first, then open the copied link in any browser. H.264 uses GPU
encoding when a compatible NVIDIA, Intel, or AMD encoder is available, and
falls back to JPEG automatically.

## Features

- **Extend desktop:** use another device's browser as a real extra Hyprland
  display. Choose resolution, 100% or 200% scale, landscape or portrait, and
  placement beside your existing screens. Move windows onto it with this
  computer's mouse and keyboard.
- **Hardware-accelerated H.264:** NVIDIA NVENC or VA-API on Linux, and
  VideoToolbox in macOS demo builds. Auto selects a working GPU encoder after
  a test frame, then falls back to OpenH264. H.264 / WebRTC is the default
  transport; JPEG remains available as a fallback and a manual choice.
- **Window, Screen, or Area:** choose from a workspace map, select a monitor,
  or drag a region. Preview the selection before sharing.
- **Browser viewer:** pause/resume, snapshots, fullscreen, fit controls, and
  stream diagnostics. The local pointer hides when idle. Extended-desktop
  sessions enter fullscreen when the browser allows, or when you tap the
  picture. Viewers do not need an app.
- **Omarchy bar controls:** see the source and viewers, copy or open the link,
  show a scannable QR code, send it to a nearby device, or stop sharing.
- **Nearby sharing:** send the link to OmaSend or LocalSend devices. OmaBeam
  includes the sending protocol; the receiver needs a LocalSend-compatible app.
- **Screenshots:** copy, save, or share an image through `omarchy share file`.
- **Portal picker:** optionally select sources for apps using
  xdg-desktop-portal-hyprland.

## Install

Run inside Omarchy / Hyprland. A source checkout needs Rust and the native
build dependencies in [Development](docs/DEVELOPMENT.md). A compiled Linux
release bundle includes the app and needs no Rust.

```bash
./install.sh
```

The installer builds or verifies the native app, installs and enables the bar
plugin, adds a floating-window rule, and binds **Super + Shift + T** when available. It can be
rerun. It checks TCP **9847** and UDP **9848** against UFW's incoming rules for
the detected LAN. If those ports are blocked, it prompts for sudo and adds
persistent, subnet-scoped allows. `--check-ports` inspects without changing
rules. `--open-firewall CIDR` still selects a specific viewer network and
fails if access cannot be verified. Portal configuration stays unchanged.
`wl-copy`, `rsync`, `jq`, Python 3, and the Omarchy shell commands must be
available.

To restrict or override the detected LAN, replace the example with your
viewer network:

```bash
./install.sh --open-firewall 192.168.2.0/24
```

Already installed? Check or open the ports without rebuilding:

```bash
./install.sh --check-ports
./install.sh --check-ports --open-firewall 192.168.2.0/24
```

Opening ports requires sudo access. A normal install still succeeds if you
decline sudo or UFW cannot be updated; an explicit check or `--open-firewall`
request exits unsuccessfully if access remains blocked or unverified. See
[Firewall checks](RELEASING.md#firewall-checks) for detection limits.

The plugin ID is `io.github.cfaulkingham.omabeam`. Its default location is:

```text
~/.config/omarchy/plugins/io.github.cfaulkingham.omabeam/
```

`XDG_CONFIG_HOME` overrides `~/.config`. The launcher inside that directory is
`omarchy-plugin/omabeam`; it runs the native app installed with the plugin.

See [Installation and releases](RELEASING.md) for Git-managed installs,
bundles, updates, and removal. Run `./install.sh --backend-only` inside a
plugin checkout to build the app without changing desktop configuration.

## Experimental Google Cast

This branch adds a **Google Cast** destination for native, video-only H.264
mirroring to one receiver. Build the optional helper with
`./install.sh --backend-only --with-cast`, then select Google Cast and a receiver
in the picker. The bar shows the receiver and a Stop action. Cast sessions do
not need a browser link.

The initial profiles are 720p and 1080p at up to 30 fps, subject to receiver
limits. Real Hyprland extended-display playback has been confirmed at 720p on a
Google Nest Hub and 1080p on an E65-E1 TV. Sustained performance and broader
device behavior still need qualification; see [current results and commands](docs/NATIVE-CAST-STATUS.md).
System audio is a later milestone.

## Removing

Stop an active share from the bar (or `omabeam --stop`), then:

```bash
omarchy plugin remove io.github.cfaulkingham.omabeam
```

That removes the plugin checkout, including the plugin-local binary. It does
**not** stop a share that is already running (the live process is detached),
and it does not edit Hyprland.

If you ran the full `./install.sh` (not `--backend-only`), also run
`./install.sh --remove-desktop` from a copy of the plugin **before** removing
it, or delete the blocks marked `-- omabeam (install.sh)` from
`hyprland.lua` and `bindings.lua`. Then reload Hyprland.

Left on disk after removal:

- `$XDG_RUNTIME_DIR/omabeam/` — `live.json` and `live.log` for the current
  session. These go away at logout. There is no `/tmp` fallback.
- Screenshots under `$OMARCHY_SCREENSHOT_DIR/omabeam`, `$XDG_PICTURES_DIR/omabeam`,
  or `~/Pictures/omabeam/`
- Firewall rules (including ones added during install or with `--open-firewall`) and any
  `xdph.conf` portal-picker lines you added

A detached live process keeps serving until you stop it or it loses its
source. Stop sharing before `omarchy plugin remove` if you can still run
`omabeam --stop`.

## Extend your desktop

Choose **Extend desktop** in the standalone picker to use another device's
browser as an extra display. Choose its resolution, 100% or 200% desktop scale,
landscape or portrait orientation, and placement beside your existing screens.
The layout preview shows where the new display will appear. It is created only
when you click **Extend desktop**.

Open the copied link — or the QR code in the bar panel — on your other device.
A first visit offers fullscreen and, for extended desktops, **Match this
device**; choose **Keep watching** to skip it. The choice is remembered per
tab. The viewer hides the local pointer
when idle so it does not cover the host cursor, and enters fullscreen when the
browser allows or when you tap the picture. Move windows onto the extra display
using your Omarchy computer's mouse or keyboard. The extra display starts empty;
windows and notifications placed there become visible to viewers. Native pixels
and the host cursor are selected when entering this mode. Extended desktop defaults
to 60 FPS; an explicitly selected frame rate or quality preset takes precedence. H.264 / WebRTC is
the default video transport; choose **Video transport → JPEG** if you want
MJPEG instead. JPEG and H.264 use
the same firewall ports as other shares.

An extended display accepts **one active browser client**. Additional devices
or tabs see “This display is already connected to another device.” Refreshing
the connected tab, or switching between JPEG and H.264, keeps its place.
Pausing, closing, or losing the client reserves its place for 15 seconds before
another device can connect. Regular screen sharing still supports multiple viewers.

Select **Match this device** in the viewer to resize the virtual desktop to its
available viewing area, including changes to fullscreen, window size, and
orientation. OmaBeam selects 100% or 200% desktop scale for the client's pixel
density, within the supported display limits (minimum 640×480, maximum
3840×2160 or portrait). Matching encodes at the display's native pixels,
temporarily overriding the host's pixel-detail and maximum-width settings.
Select **Device size: On** again to restore the resolution, scale, and encoding
settings chosen on the host. The preference survives a refresh of that tab.
If a requested mode cannot be applied or captured, OmaBeam attempts to restore
the previous mode and reports the failure in the viewer.

Stop sharing from the bar, or run `omabeam --stop`, to remove the extra display.
Closing or disconnecting the viewer leaves it available for reconnection.
Hyprland returns its workspaces to remaining displays when the output is removed.
OmaBeam leaves physical monitor settings and Hyprland configuration files alone.
If the process is killed, `--stop` or the next share retries cleanup using the
saved display record.

The equivalent CLI command is:

```bash
omabeam --native-pixels --cursor --live extend 1920 1080 1 right
```

The four values are width, height, desktop scale (`1` or `2`), and placement
(`right`, `left`, `above`, or `below`). This requires Hyprland with Lua monitor
configuration and a working headless output backend. It is separate from the
portal picker and screenshot mode. Input remains on the host computer; the
browser is a display, with its usual viewing controls.

## Share your screen

To share a window, monitor, or region instead of adding a display, follow
the steps below. Hardware-accelerated H.264 is available for every live share.

1. Open OmaBeam from the bar or **Super + Shift + T**.
2. Choose **Window**, **Screen**, or **Area**, then check the preview.
3. Set quality, video transport, and cursor visibility.
4. Select **Start sharing**. The picker closes and copies the browser link.
5. Open the link on another computer, or use **Send nearby** in the bar panel.

Window capture follows the selected window even when another window overlaps
it. If the compositor cannot capture it separately, select an area explicitly.
A lost source ends the share and clears the viewer image.

Defaults: 15 FPS (60 FPS for extended desktop), H.264 / WebRTC with JPEG fallback, JPEG quality 55, native
logical width, cursor off, 4 Mbit/s at 15 FPS (16 Mbit/s at 60 FPS), and local network (TCP **9847** and UDP **9848** on
0.0.0.0). Presets offer Balanced, Crisp text, and Smooth motion. Advanced exposes
individual settings.

Crisp text uses native captured pixels at JPEG quality 90, preserving fine
detail on HiDPI displays. Balanced and Smooth motion use logical pixels.
Advanced → Pixel detail lets you choose either mode; Maximum width caps the
encoded image in that mode. Preview uses the same pixel grid and width cap as the stream.
Native pixels can increase CPU use and bandwidth. PNG screenshots always
keep capture resolution.

Open **Stream diagnostics** in the browser viewer to see captured and encoded
sizes, capture/encode timing, outgoing bandwidth, and delivery counters for
each JPEG viewer connection. The main FPS value counts captured frames published on the host.
Sent frames measure socket writes; they do not measure browser playback or
end-to-end latency. Capture wait includes waiting for screen changes, so a
static source can report 0 FPS without a problem.

**H.264 / WebRTC** is the default video transport. Choose **Video transport →
JPEG** for MJPEG. The encoder defaults to **Auto**: OmaBeam tries NVIDIA NVENC or VA-API
(Intel/AMD and other compatible drivers) on Linux, and VideoToolbox in macOS
demo builds. A backend is selected only after it encodes a compatible frame.
If hardware is unavailable or fails during a share, Auto continues with the
built-in OpenH264 software encoder. Stream diagnostics show the selected
encoder and any fallback reason. Advanced settings also offer **Hardware**
(require GPU encoding) and **Software**.
H.264 targets 4 Mbit/s at 15 FPS and scales with frame rate up to 16 Mbit/s
(16 Mbit/s at 60 FPS). An explicit Advanced bitrate or `--h264-bitrate` is kept
as-is. NVIDIA NVENC uses VBR with a 2× burst cap so quality can recover after
fast motion without adding encode delay.
Advanced lets you adjust the target bitrate. The browser automatically falls back to JPEG
if H.264 negotiation or playback fails, and stays on JPEG. Click **Video: Auto**
to select JPEG manually, or **Video: JPEG** to try Auto again. Pause releases
the connection. `--jpeg` selects JPEG/MJPEG from the command line.

H.264 uses 4:2:0 color, which can soften fine colored text. Preview and browser
snapshots remain JPEG; PNG screenshots are unchanged. Odd image dimensions
are padded by one pixel for H.264. Unsupported sizes fall back to JPEG without
silently lowering the selected resolution. WebRTC diagnostics show the
encoder, connected peers, bandwidth, and this browser's decoded codec/FPS.
Diagnostics separate capture-ready wait, scaling/color conversion, H.264 encoding,
and the encoded-frame queue. Browser decode and jitter-buffer times cover the
latest sampling interval. These do not measure end-to-end latency. The viewer
requests minimal buffering where supported; the browser can retain more buffering
for network conditions.

WebRTC needs UDP **9848** as well as the HTTP port. `--webrtc-port` changes
the UDP port; `0` selects an available one. Media uses encrypted DTLS-SRTP;
the page, signaling, and JPEG fallback still use plain HTTP. This mode uses
local host candidates only, with no external STUN/TURN servers. A TCP-only
SSH tunnel uses JPEG fallback. Up to eight WebRTC viewers can connect.

```bash
omabeam --fps 30 --width 1280 --h264-bitrate 4000000
```

To test encoder detection with generated frames, without capturing your desktop:

```bash
omabeam --check-encoders
omabeam --check-encoders --encoder hardware
```

Hardware encoding uses the adjacent `omabeam-encoder` helper and system FFmpeg
libraries. The installer builds the helper when its dependencies are available;
on Omarchy these come from the `ffmpeg` package. Matching GPU drivers and device
permissions are also required. The main app still runs with software encoding
if the helper or its libraries are missing. `--encoder software` skips detection.

Links use a fresh random token and plain HTTP. Anyone on the local network
with the link can view the share. If a firewall blocks viewers, allow TCP 9847
and, for WebRTC, UDP 9848 from your intended subnet only. If JPEG works but
H.264 reports a playback timeout or a lost WebRTC connection, check UDP 9848
with the installer commands above. A missing H.264 decoder or encoder error
needs a separate fix. Use `--bind 127.0.0.1` to keep the stream on
this computer, or forward the port over SSH for remote viewing.

The viewer reads the host's Omarchy palette when opened; reload it after a
theme change. Capture slows to about one frame per second when nobody watches.

## Screenshots and keys

Select **Screenshot** for Copy, Save, and LocalSend actions. Saved captures
use `OMARCHY_SCREENSHOT_DIR`, then `XDG_PICTURES_DIR`, then `~/Pictures`,
with an `omabeam/` subdirectory.

| Control | Action |
| --- | --- |
| Arrows or `h j k l` | Select a source or panel action |
| `Tab` / `Shift+Tab` | Move between controls |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Switch Window, Screen, Area, Extend desktop |
| `Enter` | Start sharing; copy in Screenshot mode; confirm in portal mode |
| `c` / `s` / `f` | Copy / save / share a screenshot |
| `v` | Start live sharing |
| `q` | Toggle the share QR code (live panel) |
| `?` | Toggle keyboard help |
| `Esc` | Close or cancel |

In the bar panel, `c` copies the link, `o` opens the viewer, `n` sends nearby,
`q` shows the QR code, and `r` refreshes status.

## Use as a portal picker

Set the launcher in `~/.config/hypr/xdph.conf`, substituting your home directory
or custom config location:

```conf
screencopy {
    allow_token_by_default = true
    custom_picker_binary = /home/YOU/.config/omarchy/plugins/io.github.cfaulkingham.omabeam/omarchy-plugin/omabeam
}
```

Restart `xdg-desktop-portal-hyprland` or log out and back in to apply it.

## Command line and development

Run the installed launcher with `--help` for CLI options. `--status` prints
session JSON, `--stop` ends sharing, and `--send-link` opens nearby devices.
[Development](docs/DEVELOPMENT.md) covers builds, architecture, demos, and tests.

MIT licensed. LocalSend retains its own license and
[upstream provenance](vendor/localsend/UPSTREAM.md).
