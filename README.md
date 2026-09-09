![OmaBeam — Your screen. One simple link. Screen sharing for Omarchy and Hyprland.](docs/assets/omabeam-marketing.png)

# OmaBeam

**Share a window, screen, or area. Watch in any browser.**

OmaBeam is a screen-sharing plugin for Omarchy and Hyprland. Choose a source,
preview what others will see, and share a browser link over your local network.
The native picker, browser viewer, nearby-device window, and Omarchy bar panel
share the same four-tile mark and Omarchy colors.

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

## Share your screen

1. Open OmaBeam from the bar or **Super + Shift + T**.
2. Choose **Window**, **Screen**, or **Area**, then check the preview.
3. Set quality, cursor visibility, and who can connect.
4. Select **Start sharing**. The picker closes and copies the browser link.
5. Open the link on another computer, or use **Send nearby** in the bar panel.

Window capture follows the selected window even when another window overlaps
it. If the compositor cannot capture it separately, select an area explicitly.
A lost source ends the share and clears the viewer image.

Defaults: 15 FPS, JPEG quality 55, native logical width, cursor off, and
local-network access on TCP **9847**. Presets offer Balanced, Crisp text, and
Smooth motion. Advanced exposes individual settings.

Links use a fresh random token and plain HTTP. Anyone with the link who can
reach the host can view the share. Choose **This computer** for local access,
or forward that port over SSH. If a firewall blocks LAN viewers, allow TCP
9847 from your intended local subnet.

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
