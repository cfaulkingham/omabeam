#!/usr/bin/env bash
# Run inside Omarchy's native Arch / Arch Linux ARM builder as its builder user.
set -euo pipefail
umask 022
cd "$(dirname "$0")/.."
target=${1:?Usage: ci-package.sh TARGET}
case "$target:$(uname -m)" in
  x86_64-unknown-linux-gnu:x86_64|aarch64-unknown-linux-gnu:aarch64) ;;
  *) echo "Build target does not match the native runner" >&2; exit 1 ;;
esac

sudo pacman -Syu --noconfirm --needed base-devel clang cmake nasm pkgconf rust \
  ffmpeg fontconfig freetype2 wayland libxkbcommon libxkbcommon-x11 libxcb \
  openssl vulkan-icd-loader python python-pip chromium rsync jq wl-clipboard

export CARGO_TARGET_DIR="$PWD/target/rust-build"
cargo test --workspace --locked
cargo build --release --locked
binary="$CARGO_TARGET_DIR/release/omabeam"
encoder="$CARGO_TARGET_DIR/release/omabeam-encoder"
cast="$CARGO_TARGET_DIR/release/omabeam-cast"
install -m 755 target/cast/omabeam-cast "$cast"

python tests/native_cast.py "$cast"
python tests/firewall.py
python tests/extended_desktop.py --binary "$binary"
python tests/hardware_encoding.py --binary "$binary"
python tests/packaging.py
python tests/release.py
python -m venv target/release-qa
target/release-qa/bin/pip install 'Pillow>=10,<13' 'playwright>=1.50,<2'
target/release-qa/bin/python tests/smoke.py --binary "$binary" --browser --browser-executable /usr/bin/chromium
target/release-qa/bin/python tests/webrtc.py --binary "$binary" --browser-executable /usr/bin/chromium

cargo install cargo-bundle-licenses --locked --root target/release-tools
target/release-tools/bin/cargo-bundle-licenses bundle-licenses --format yaml --output target/THIRDPARTY.yml
python scripts/package-plugin.py --binary "$binary" --encoder-helper "$encoder" \
  --cast-helper "$cast" --cast-licenses target/cast/notices \
  --target "$target" --licenses target/THIRDPARTY.yml

report="dist/$target-runtime-libraries.txt"
{
  cat /etc/os-release
  rustc -Vv
  ldd "$binary" "$encoder" "$cast"
  pacman -Q
} > "$report"
if grep -q 'not found' "$report"; then
  cat "$report"
  echo "A bundled executable cannot resolve its runtime libraries" >&2
  exit 1
fi
