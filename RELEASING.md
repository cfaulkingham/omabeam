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

## Build a release bundle

Keep the versions aligned in `manifest.json` and `Cargo.toml`. Run the
**Package OmaBeam** workflow on the release commit. It builds and tests
Linux x86_64, then uploads an archive, SHA-256 checksum, and runtime-library
report. It does not publish automatically.

To build locally on Linux with Python 3.11+, Rust, and the dependencies in
[Development](docs/DEVELOPMENT.md):

```bash
cargo build --release --locked
cargo install cargo-bundle-licenses --locked
cargo bundle-licenses --format yaml --output target/THIRDPARTY.yml
# Review the report and fill missing license texts before distribution.
python3 scripts/package-plugin.py \
  --binary target/release/omabeam \
  --target x86_64-unknown-linux-gnu \
  --licenses target/THIRDPARTY.yml
```

The packager checks the ELF architecture, entry point, and absence of symlinks.
It includes runtime files, the binary, installer, documentation, and licenses.
Build caches and session data are excluded. Identical inputs produce identical
archives. Review `licenses/THIRDPARTY.yml` before distributing workflow artifacts.

The workflow targets x86_64 GNU/Linux. The packager also accepts
`aarch64-unknown-linux-gnu` given a binary built and tested for that target.
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
installer, remove the blocks marked `-- omabeam (install.sh)` from
`hyprland.lua` and `bindings.lua`. Screenshots and any firewall or portal
settings you configured separately are left in place.
