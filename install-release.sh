#!/usr/bin/env bash
# Install the published OmaBeam package and enable its Omarchy plugin:
# curl -fsSL https://raw.githubusercontent.com/cfaulkingham/omabeam/main/install-release.sh | bash

# Define the whole installer before running it when streamed through a pipe.
install_release() (
  set -euo pipefail
  fail() { echo "OmaBeam: $*" >&2; exit 1; }
  [[ $# == 0 ]] || fail "This installer takes no arguments."
  [[ $EUID != 0 ]] || fail "Run this as your desktop user, without sudo."
  [[ $(uname -s) == Linux ]] || fail "This installer requires Omarchy on Linux."
  case "$(uname -m)" in
    x86_64|aarch64) ;;
    *) fail "Release packages support x86_64 and ARM64 only." ;;
  esac
  for tool in curl pacman sudo omarchy omarchy-shell; do
    command -v "$tool" >/dev/null 2>&1 || fail "Missing $tool; run this inside Omarchy."
  done
  plugin_dir="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/io.github.cfaulkingham.omabeam"
  if [[ -e $plugin_dir/.git ]]; then
    fail "A Git-managed plugin exists at $plugin_dir. Remove it with Omarchy's plugin commands before switching to the binary package."
  fi
  if ! (: </dev/tty) 2>/dev/null; then
    fail "Run this command from a terminal so package installation can ask for sudo and confirmation."
  fi
  # pacman and the plugin installer must read the terminal, not the piped script.
  exec </dev/tty

  package_dir=$(mktemp -d "${TMPDIR:-/tmp}/omabeam-install.XXXXXXXX")
  trap 'rm -rf -- "$package_dir"' EXIT
  cd "$package_dir"
  echo "Downloading the OmaBeam binary package recipe..."
  curl -fsSL --retry 3 --output PKGBUILD \
    https://raw.githubusercontent.com/cfaulkingham/omabeam/main/packaging/aur/PKGBUILD

  if ! pacman -Qq base-devel >/dev/null 2>&1; then
    sudo pacman -S --needed base-devel
  fi
  command -v makepkg >/dev/null 2>&1 || fail "makepkg is missing; install base-devel and retry."
  # The recipe verifies the release archive's SHA-256 and installs dependencies.
  makepkg -si --needed
  bash /usr/share/omabeam/plugin/install.sh
  echo "OmaBeam installation complete."
)

install_release "$@"
