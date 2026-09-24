# Releasing OmaBeam

For installation, updates, network setup, and removal, see the
[README](README.md#install). This document covers building, validating, and
publishing release artifacts. See [Development](docs/DEVELOPMENT.md) for local
builds and tests, and [Cast qualification status](docs/NATIVE-CAST-STATUS.md)
for the remaining checks before distributing a Cast-enabled build.

## Build a release bundle

OmaBeam releases target **Omarchy**, including its bar shell and Lua-based
Hyprland configuration. These are not general-purpose Ubuntu, macOS or Windows
packages. Keep the versions aligned in `manifest.json` and `Cargo.toml`.

Run **Package OmaBeam** manually on the release branch to build and test both
architectures without creating a release:

```bash
gh workflow run package.yml --ref YOUR_RELEASE_BRANCH
```

| Bundle target | Native build and test runner | Runtime environment |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | Omarchy's Arch builder, stable mirror |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | Omarchy's Arch Linux ARM builder |

The runners host containers built from a pinned revision of
[Omarchy's package builder](https://github.com/omacom/omarchy-pkgs/tree/93b1d62ef09bdcd7515e2db6907833f73d2158ed/build).
The Rust app and FFmpeg encoder are compiled inside these containers, using
the target architecture's Arch libraries. ARM64 is experimental and requires
an existing compatible Omarchy installation; a successful build does not
establish support for a particular ARM device or GPU.

Both bundles include `omabeam`, `omabeam-encoder`, **`omabeam-cast`**, the
plugin, installer, documentation and license notices. The pinned Cast SDK
ships Linux x86_64 build tools, so its ARM64 helper is cross-compiled on
x86_64 with the pinned ARM64 sysroot, then executed and tested on the native
ARM64 runner. No emulator is needed. The workflow checks native Cast IPC,
workspace tests, encoder fallback, simulated desktop lifecycle, browser
playback, packaging and shared-library resolution. Physical Cast receivers,
GPU drivers and real Omarchy interactions still require the checks below.

Each architecture produces a `.tar.gz`, `.tar.gz.sha256`, and uniquely named
runtime-library report. Stable releases also produce an AUR recipe archive
containing `PKGBUILD` and `.SRCINFO`, with SHA-256 values computed from both
verified bundles. Download the combined `omabeam-release-assets` workflow
artifact. A failed architecture prevents release assembly.

A bundle's helper matches the FFmpeg major version in its build environment.
The x86_64 stable mirror and Arch Linux ARM can carry different versions;
consult each runtime-library report. Rebuild bundles after a target moves
FFmpeg to a new major version, or
the installer will warn that the helper cannot load. Source installs are
unaffected: they always build the helper against the machine's own FFmpeg.

To build a bundle locally, use an Arch or Omarchy machine with Python 3.11+,
Rust, and the dependencies in [Development](docs/DEVELOPMENT.md), so the
packaged helper matches Omarchy's FFmpeg:

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

For a local ARM64 bundle, build the Rust workspace on the target Arch Linux ARM
machine. Build Cast on an x86_64 Linux machine with
`python3 scripts/build-cast.py --sync --target-cpu arm64`, transfer the resulting
`target/native-cast/openscreen/out/omabeam/omabeam-cast` and
`target/native-cast/notices`, and run `tests/native_cast.py` on ARM64 before
packaging with `--target aarch64-unknown-linux-gnu`. Keep a separate Cast cache
per target when building both architectures locally.

Executables use system libraries; check the matching
`TARGET-runtime-libraries.txt` against the target Omarchy machine.

## AUR binary package

The generated package is named **`omabeam-bin`** and includes Cast. Its
template is `packaging/aur/PKGBUILD.in`; do not submit that unrendered template
to AUR. After downloading both bundles and their checksums into `dist/`:

```bash
python3 scripts/release-assets.py
cd dist/aur
makepkg --printsrcinfo > .SRCINFO
makepkg -si
```

The release assets must be published before `makepkg` can download them.
For pre-publication validation, place the matching downloaded `.tar.gz` beside
the generated `PKGBUILD`. Test `makepkg` on each architecture before submitting
the generated `PKGBUILD` and `.SRCINFO` to AUR. The workflow generates the
recipe but does not publish an AUR entry. AUR publishing uses a separate
repository and account. Pre-release versions produce test bundles without an
AUR recipe.

`makepkg` records the encoder's required FFmpeg library ABI versions from the
bundled executable. Rebuild and update `omabeam-bin` when those ABIs change;
do not bypass pacman's resulting dependency conflict.

Pacman owns the three executables in `/usr/lib/omabeam/`, the `omabeam` command,
plugin runtime files in `/usr/share/omabeam/plugin/`, and license notices in
`/usr/share/licenses/omabeam-bin/`. As the desktop user, enable the plugin with:

```bash
bash /usr/share/omabeam/plugin/install.sh
```

Run this again after package upgrades to refresh the user's QML plugin files.
The user plugin launcher uses the pacman-managed binaries, so they update
without copying executables into home directories. Package installation itself
does not edit user configuration, open firewall ports or enable the plugin.
The user-run installer retains its usual desktop and firewall behavior. When
migrating a Git-managed source installation, remove it with Omarchy's plugin
commands first; the installer preserves its existing refusal to overwrite it.

## Validate and publish

On Omarchy, run from the plugin directory:

```bash
omarchy plugin validate .
qmllint -I "$OMARCHY_PATH/shell" omarchy-plugin/BarWidget.qml omarchy-plugin/Panel.qml
```

Verify bar click, Escape, summon/hide, disable/re-enable, shell restart (an
open picker or send window stays open), and removal. Test capture, the viewer,
clipboard, portal picker, and nearby-device sharing on real Hyprland, including
a PIN-protected receiver and the official LocalSend apps. Sending to those apps
has not been verified yet; see
[Nearby sending](docs/DEVELOPMENT.md#nearby-sending).

Once the release commit is ready, tag that exact commit and push the tag
(substitute the version from the manifests):

```bash
git tag -a v0.1.0 -m 'OmaBeam 0.1.0'
git push origin v0.1.0
```

The workflow rejects a tag that disagrees with either manifest, builds both
architectures, verifies checksums and creates a **draft** GitHub release with
all release assets. It never publishes automatically. Rerunning can replace
assets on the draft, but refuses to alter a published release.

Review the draft, license reports, target runtime requirements and the real
device checks above, then publish it. Submit the plugin through the
[Omarchy publishing guide](https://plugins.omarchy.org/publish.html) and submit
the generated AUR recipe separately.
