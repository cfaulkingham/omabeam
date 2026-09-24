# Releasing OmaBeam

For installation, updates, network setup, and removal, see the
[README](README.md#install). This document covers building, validating, and
publishing release artifacts. See [Development](docs/DEVELOPMENT.md) for local
builds and tests, and [Cast qualification status](docs/NATIVE-CAST-STATUS.md)
for the remaining checks before distributing a Cast-enabled build.

## Build a release bundle

Keep the versions aligned in `manifest.json` and `Cargo.toml`. Run the
**Package OmaBeam** workflow on the release commit. It builds and tests
Linux x86_64, building the hardware helper in an Arch Linux container so it
links the FFmpeg Omarchy ships, then uploads an archive, SHA-256 checksum,
and runtime-library report. It does not publish automatically.

A bundle's helper matches the FFmpeg major version current on Arch when it
was built. Rebuild bundles after Arch moves FFmpeg to a new major version, or
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

The workflow targets x86_64 GNU/Linux. The packager also accepts
`aarch64-unknown-linux-gnu` given a binary built and tested for that target.
Native Cast has not been qualified on Linux aarch64; omit it until its helper
has been built and tested there.
Executables use system libraries; check the workflow's
`linux-runtime-libraries.txt` against the target Omarchy machine.

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

Publish the source in a public GitHub repository with the root manifest,
README, and MIT license. Attach the verified bundle and checksum to a release,
then submit through the [Omarchy publishing guide](https://plugins.omarchy.org/publish.html).
