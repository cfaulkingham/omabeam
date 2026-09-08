#!/usr/bin/env bash
# Install OmaBeam on this Omarchy/Hyprland machine:
#   1. build the plugin-local native app (or use the release's bundled binary)
#   2. float the picker (sized to the monitor) and bind SUPER+SHIFT+T
#   3. copy and enable the Omarchy bar widget
#
# Safe to re-run. Does not change the portal picker or open the firewall.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ID="io.github.cfaulkingham.omabeam"
PLUGIN_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/$PLUGIN_ID"
HYPRLAND_LUA="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/hyprland.lua"
BINDINGS_LUA="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/bindings.lua"
SHELL_JSON="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/shell.json"
BIN="$PLUGIN_DIR/omarchy-plugin/omabeam"
BIND_KEYS="SUPER + SHIFT + T"
MARKER="omabeam (install.sh)"

export PATH="$HOME/.cargo/bin:$PATH"

BACKEND_ONLY=false
case "${1:-}" in
  --backend-only) BACKEND_ONLY=true ;;
  "") ;;
  *) echo "Usage: ./install.sh [--backend-only]" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || { echo "Usage: ./install.sh [--backend-only]" >&2; exit 2; }
[[ $(uname -s) == Linux ]] || { echo "OmaBeam installation requires Linux with Omarchy / Hyprland." >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "install.sh: missing required command: $1" >&2
    exit 1
  }
}

plugin_on_bar() {
  [[ -f $SHELL_JSON ]] || return 1
  jq -e --arg id "$PLUGIN_ID" '
    (.bar.layout | [.left, .center, .right] | add // [])
    | any(.id == $id)
  ' "$SHELL_JSON" >/dev/null 2>&1
}

ensure_lua_snippet() {
  local file="$1"
  local needle="$2"
  local snippet="$3"

  if grep -Fq "$needle" "$file"; then
    echo "  already present in $file"
    return 0
  fi

  printf '\n%s\n' "$snippet" >>"$file"
  echo "  appended to $file"
}

echo "==> checking tools"
[[ -f $ROOT/manifest.json ]] || {
  echo "install.sh: plugin manifest missing: $ROOT/manifest.json" >&2
  exit 1
}
if ! $BACKEND_ONLY; then
  for tool in wl-copy rsync jq python3 omarchy omarchy-shell; do need "$tool"; done
  [[ -f $HYPRLAND_LUA && -f $BINDINGS_LUA ]] || {
    echo "install.sh: expected Omarchy Hyprland config in ~/.config/hypr/" >&2
    exit 1
  }
  if [[ -e $PLUGIN_DIR/.git && ! $ROOT -ef $PLUGIN_DIR ]]; then
    echo "install.sh: a Git-managed plugin already exists at $PLUGIN_DIR." >&2
    echo "Update it with omarchy plugin update $PLUGIN_ID, then run its install.sh." >&2
    exit 1
  fi
fi

NATIVE="$ROOT/omarchy-plugin/native/bin/omabeam"
if [[ -f $ROOT/Cargo.toml ]]; then
  need cargo
  echo "==> building the native app"
  # Keep build caches outside the plugin checkout (plugin folders forbid symlinks).
  CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/omabeam/build}" \
    cargo install --path "$ROOT" --root "$ROOT/omarchy-plugin/native" --locked --force
elif [[ ! -x $NATIVE ]]; then
  echo "install.sh: this bundle has neither source nor a native binary." >&2
  exit 1
fi
"$NATIVE" --help >/dev/null || {
  echo "install.sh: the native app cannot run here. Check the bundle architecture and runtime dependencies." >&2
  exit 1
}
if $BACKEND_ONLY; then
  echo "OmaBeam native app ready. Open the bar panel and check status again."
  exit 0
fi

echo "==> installing Omarchy bar plugin $PLUGIN_ID"
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
cp "$ROOT/manifest.json" "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/install.sh" "$ROOT/RELEASING.md" "$STAGING/"
rsync -a --exclude __pycache__ "$ROOT/omarchy-plugin/" "$STAGING/omarchy-plugin/"
[[ ! -d $ROOT/licenses ]] || cp -R "$ROOT/licenses" "$STAGING/"
[[ ! -d $ROOT/docs ]] || cp -R "$ROOT/docs" "$STAGING/"
mkdir -p "$STAGING/vendor/localsend"
cp "$ROOT/vendor/localsend/UPSTREAM.md" "$STAGING/vendor/localsend/"
omarchy plugin validate "$STAGING"
if [[ ! $ROOT -ef $PLUGIN_DIR ]]; then
  mkdir -p "$PLUGIN_DIR"
  # Only replace owned runtime files; never delete an installed Git checkout.
  rsync -a --delete "$STAGING/omarchy-plugin/" "$PLUGIN_DIR/omarchy-plugin/"
  cp "$STAGING/manifest.json" "$STAGING/README.md" "$STAGING/LICENSE" "$STAGING/install.sh" "$STAGING/RELEASING.md" "$PLUGIN_DIR/"
  [[ ! -d $STAGING/licenses ]] || cp -R "$STAGING/licenses" "$PLUGIN_DIR/"
  [[ ! -d $STAGING/docs ]] || cp -R "$STAGING/docs" "$PLUGIN_DIR/"
  mkdir -p "$PLUGIN_DIR/vendor/localsend"
  cp "$STAGING/vendor/localsend/UPSTREAM.md" "$PLUGIN_DIR/vendor/localsend/"
fi
omarchy-shell shell rescanPlugins
if plugin_on_bar; then
  echo "  already on the bar"
else
  omarchy plugin enable "$PLUGIN_ID" --section right
fi
# rescanPlugins does not drop Qt's compiled QML; the running shell will
# keep serving the previous widget until the process restarts with an
# empty disk cache.
QMLCACHE="${XDG_CACHE_HOME:-$HOME/.cache}/quickshell/qmlcache"
rm -rf "$QMLCACHE"
echo "==> restarting Omarchy shell so $PLUGIN_ID reloads"
omarchy restart shell

echo "==> Hyprland window rule"
WINDOW_RULE="$(cat <<EOF
-- $MARKER
o.window("omabeam", {
  float = true,
  center = true,
  focus_on_activate = false,
  animation = "popin",
  size = { "(monitor_w*3/4)", "(monitor_h*3/4)" },
  max_size = { 980, 560 },
})
EOF
)"
if grep -Fq 'o.window("omabeam"' "$HYPRLAND_LUA"; then
  python3 - "$HYPRLAND_LUA" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
text = path.read_text()
new = """-- omabeam (install.sh)
o.window("omabeam", {
  float = true,
  center = true,
  focus_on_activate = false,
  animation = "popin",
  size = { "(monitor_w*3/4)", "(monitor_h*3/4)" },
  max_size = { 980, 560 },
})
"""
pattern = re.compile(
    r"-- omabeam \(install.sh\)\n"
    r'o\.window\("omabeam", \{.*?\}\)\n?',
    re.S,
)
if pattern.search(text):
    path.write_text(pattern.sub(new, text, count=1))
    print("  updated window rule")
else:
    print("  present (left unchanged)")
PY
else
  ensure_lua_snippet "$HYPRLAND_LUA" 'o.window("omabeam"' "$WINDOW_RULE"
fi

echo "==> Hyprland bind $BIND_KEYS"
if grep -Fq "$BIND_KEYS" "$BINDINGS_LUA" && ! grep -Fq 'OmaBeam' "$BINDINGS_LUA"; then
  echo "  $BIND_KEYS is already used in $BINDINGS_LUA; not replacing it" >&2
else
  if grep -Fq -- "-- $MARKER" "$BINDINGS_LUA"; then
    python3 - "$BINDINGS_LUA" "$BIN" <<'PY'
from pathlib import Path
import json
import re
import sys
path = Path(sys.argv[1])
text = path.read_text()
pattern = r'(-- omabeam \(install.sh\)\n)o\.bind\("SUPER \+ SHIFT \+ T", "OmaBeam", \{ launch = .*? \}\)'
replacement = '-- omabeam (install.sh)\no.bind("SUPER + SHIFT + T", "OmaBeam", { launch = ' + json.dumps(sys.argv[2]) + ' })'
path.write_text(re.sub(pattern, lambda _: replacement, text))
PY
  fi
  ensure_lua_snippet "$BINDINGS_LUA" 'OmaBeam' "$(cat <<EOF
-- $MARKER
o.bind("$BIND_KEYS", "OmaBeam", { launch = "$BIN" })
EOF
)"
fi

if "$BIN" --hypr version >/dev/null 2>&1; then
  echo "==> reloading Hyprland"
  "$BIN" --hypr reload
  errors="$("$BIN" --hypr configerrors)"
  if ! jq -e '[.[] | select(type == "string" and . != "")] | length == 0' <<<"$errors" >/dev/null; then
    echo "install.sh: Hyprland configuration errors:" >&2
    jq -r '.[] | select(type == "string" and . != "")' <<<"$errors" >&2
    exit 1
  fi
else
  echo "==> skipping reload (the current Hyprland session is not reachable)"
fi

echo
echo "OmaBeam installed."
echo "  binary:  $BIN"
echo "  plugin:  $PLUGIN_DIR"
echo "  launch:  $BIND_KEYS  or  $BIN"
echo
echo "Optional: set custom_picker_binary = $BIN in ~/.config/hypr/xdph.conf"
echo "Optional: sudo ufw allow from 192.168.0.0/16 to any port 9847 proto tcp comment OmaBeam"
