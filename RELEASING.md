# Installing and releasing OmaBeam

OmaBeam ships as one Omarchy plugin repository. The native app includes the
picker, capture backend, browser viewer, HTTP server, and LocalSend sender.

Omarchy's [plugin installer](https://github.com/omacom/omarchy/blob/quattro/shell/README.md)
clones and validates a Git repository. It does not run build hooks or download
release assets. Source installs need an explicit build; compiled bundles
include the native app.

## Git-managed installation

Once the OmaBeam repository is published:

```bash
omarchy plugin add https://github.com/cfaulkingham/omabeam.git
cd "${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/io.github.cfaulkingham.omabeam"
./install.sh --backend-only
omarchy plugin enable io.github.cfaulkingham.omabeam --section right
```

The build installs `omarchy-plugin/native/bin/omabeam` in the checkout.
`--backend-only` leaves desktop configuration and the shell process alone.
The full `./install.sh` also adds a floating-window rule and shortcut.

After `omarchy plugin update io.github.cfaulkingham.omabeam`, rerun that
checkout's `./install.sh --backend-only` to rebuild the app.

For the experimental native Cast destination, use
`./install.sh --backend-only --with-cast`. It also builds the pinned Open Screen
helper and installs its notices under `licenses/cast`. This requires Python 3,
Git, pkg-config and the C/C++ build dependencies, and downloads several
gigabytes of dependency/toolchain files to `$XDG_CACHE_HOME/omabeam/cast`
(default `~/.cache/omabeam/cast`). The Cast helper is optional for browser shares.
Read [Cast qualification status](docs/NATIVE-CAST-STATUS.md) before distributing
a Cast-enabled build; physical-device qualification is still outstanding.

## Firewall checks

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
not diagnose Cast. See the [Cast network checks](docs/NATIVE-CAST-STATUS.md#network-checks).

## Build a release bundle

Keep the versions aligned in `manifest.json` and `Cargo.toml`. Run the
**Package OmaBeam** workflow on the release commit. It builds and tests
Linux x86_64, then uploads an archive, SHA-256 checksum, and runtime-library
report. It does not publish automatically.

To build locally on Linux with Python 3.11+, Rust, and the dependencies in
[Development](docs/DEVELOPMENT.md):

```bash
cargo build --release --locked
python3 scripts/build-cast.py --sync
cargo install cargo-bundle-licenses --locked
cargo bundle-licenses --format yaml --output target/THIRDPARTY.yml
# Review the report and fill missing license texts before distribution.
python3 scripts/package-plugin.py \
  --binary target/release/omabeam \
  --encoder-helper target/release/omabeam-encoder \
  --cast-helper target/release/omabeam-cast \
  --cast-licenses target/native-cast/notices \
  --target x86_64-unknown-linux-gnu \
  --licenses target/THIRDPARTY.yml
```

The packager checks the ELF architecture, entry point, and absence of symlinks.
It includes runtime files, the binary, installer, documentation, and licenses.
All included executables must have the target architecture; the hardware helper is
installed beside the app. It links system FFmpeg (`libavcodec`, `libavutil`,
`libavformat`); include it in the runtime-library report and verify the target
system has the matching ABI and GPU drivers. The main app can fall back to
software if the helper cannot load. Build caches and session data are excluded. Identical inputs produce identical
archives. Review `licenses/THIRDPARTY.yml` before distributing workflow artifacts.

Omit both Cast options for a browser-only bundle. Cast bundles also include
`licenses/cast/manifest.json` and the license/notice files collected from the
actual GN dependency graph. The packager verifies their hashes and the helper
binary hash; Cargo's license collector does not cover these C++ dependencies.
Include all three executables in the runtime-library report. The production
Cast helper does not link FFmpeg, SDL, Opus or VPX; the optional software test
receiver uses those libraries.

The workflow targets x86_64 GNU/Linux. The packager also accepts
`aarch64-unknown-linux-gnu` given a binary built and tested for that target.
Native Cast has not been qualified on Linux aarch64; omit it until its helper
has been built and tested there.
Executables use system libraries; check the workflow's
`linux-runtime-libraries.txt` against the target Omarchy machine.

## Install or update a bundle

Verify its `.sha256`, extract the archive, enter the
`io.github.cfaulkingham.omabeam` directory, and run `./install.sh`. No Rust is
needed. The installer checks the binary, validates the plugin, installs it,
and enables the widget.

For a plugin-only install, copy the extracted directory into
`${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/`, then run:

```bash
omarchy-shell shell rescanPlugins
omarchy plugin enable io.github.cfaulkingham.omabeam --section right
```

Stop active shares before replacing a bundle. The installer refuses to
overwrite a Git-managed install from another directory; use Git updates there.

## Validate and publish

On Omarchy, run from the plugin directory:

```bash
omarchy plugin validate .
qmllint -I "$OMARCHY_PATH/shell" omarchy-plugin/BarWidget.qml omarchy-plugin/Panel.qml
```

Verify bar click, Escape, summon/hide, disable/re-enable, shell restart, and
removal. Test capture, the viewer, clipboard, portal picker, and nearby-device
sharing on real Hyprland.

Publish the source in a public GitHub repository with the root manifest,
README, and MIT license. Attach the verified bundle and checksum to a release,
then submit through the [Omarchy publishing guide](https://plugins.omarchy.org/publish.html).

## Remove

Stop the active share, then run:

```bash
omarchy plugin remove io.github.cfaulkingham.omabeam
```

The plugin-local binary is removed with the plugin. If you used the full
installer, run `./install.sh --remove-desktop` first (or delete the blocks
marked `-- omabeam (install.sh)` from `hyprland.lua` and `bindings.lua`).
Stop the share before removing the plugin; a detached live process can outlive
the checkout. Screenshots and any firewall or portal settings you configured
separately are left in place. Session files live in `$XDG_RUNTIME_DIR/omabeam/`
and vanish at logout.
