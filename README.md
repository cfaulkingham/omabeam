![OmaBeam — Your screen. One simple link. Screen sharing for Omarchy and Hyprland.](docs/assets/omabeam-marketing.png)

# OmaBeam

**Share a window, screen, or area. Watch in any browser.**

OmaBeam is a screen-sharing plugin for Omarchy and Hyprland. Choose a source,
preview what others will see, and share a browser link over your local network.

## Features

- **Window, Screen, or Area:** choose from a workspace map, select a monitor,
  or drag a region. Preview the selection before sharing.
- **Browser viewer:** pause/resume, snapshots, fullscreen, fit controls, and
  stream diagnostics. Viewers do not need an app.
- **Omarchy bar controls:** see the source and viewers, copy or open the link,
  send it to a nearby device, or stop sharing.
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
rerun. It does not change your firewall or portal configuration. `wl-copy`,
`rsync`, `jq`, Python 3, and the Omarchy shell commands must be available.

The plugin ID is `io.github.cfaulkingham.omabeam`. Its default location is:

```text
~/.config/omarchy/plugins/io.github.cfaulkingham.omabeam/
```

`XDG_CONFIG_HOME` overrides `~/.config`. The launcher inside that directory is
`omarchy-plugin/omabeam`; it runs the native app installed with the plugin.

See [Installation and releases](RELEASING.md) for Git-managed installs,
bundles, updates, and removal. Run `./install.sh --backend-only` inside a
plugin checkout to build the app without changing desktop configuration.

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
- Firewall or `xdph.conf` portal-picker lines you added yourself

A detached live process keeps serving until you stop it or it loses its
source. Stop sharing before `omarchy plugin remove` if you can still run
`omabeam --stop`.

## Share your screen

1. Open OmaBeam from the bar or **Super + Shift + T**.
2. Choose **Window**, **Screen**, or **Area**, then check the preview.
3. Set quality, video transport, and cursor visibility.
4. Select **Start sharing**. The picker closes and copies the browser link.
5. Open the link on another computer, or use **Send nearby** in the bar panel.

Window capture follows the selected window even when another window overlaps
it. If the compositor cannot capture it separately, select an area explicitly.
A lost source ends the share and clears the viewer image.

Defaults: 15 FPS, JPEG quality 55, native logical width, cursor off, and
local network (TCP **9847** on 0.0.0.0). Presets offer Balanced, Crisp text,
and Smooth motion. Advanced exposes individual settings.

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

Choose **Video transport → H.264 / WebRTC** for optional compressed video.
JPEG remains the default. WebRTC uses a built-in OpenH264 software encoder
with a 4 Mbit/s target; Advanced lets you adjust the target bitrate. There is
no hardware acceleration yet. The browser automatically falls back to JPEG
if H.264 negotiation or playback fails, and offers **Retry H.264**. Click
**Video: Auto** to select JPEG manually. Pause releases the connection.

H.264 uses 4:2:0 color, which can soften fine colored text. Preview and browser
snapshots remain JPEG; PNG screenshots are unchanged. Odd image dimensions
are padded by one pixel for H.264. Unsupported sizes fall back to JPEG without
silently lowering the selected resolution. WebRTC diagnostics show the
encoder, connected peers, bandwidth, and this browser's decoded codec/FPS.
Decode and jitter-buffer times do not measure end-to-end latency.

WebRTC needs UDP **9848** as well as the HTTP port. `--webrtc-port` changes
the UDP port; `0` selects an available one. Media uses encrypted DTLS-SRTP;
the page, signaling, and JPEG fallback still use plain HTTP. This mode uses
local host candidates only, with no external STUN/TURN servers. A TCP-only
SSH tunnel uses JPEG fallback. Up to eight WebRTC viewers can connect.

```bash
omabeam --webrtc --fps 30 --width 1280 --h264-bitrate 4000000
```

Links use a fresh random token and plain HTTP. Anyone on the local network
with the link can view the share. If a firewall blocks viewers, allow TCP 9847
from your intended subnet only. Use `--bind 127.0.0.1` to keep the stream on
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
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Switch Window, Screen, Area |
| `Enter` | Start sharing; copy in Screenshot mode; confirm in portal mode |
| `c` / `s` / `f` | Copy / save / share a screenshot |
| `v` | Start live sharing |
| `?` | Toggle keyboard help |
| `Esc` | Close or cancel |

In the bar panel, `c` copies the link, `o` opens the viewer, `n` sends nearby,
and `r` refreshes status.

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
