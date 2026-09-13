#!/usr/bin/env bash
# Install OmaBeam on this Omarchy/Hyprland machine:
#   1. build the plugin-local native app (or use the release's bundled binary)
#   2. float the picker (sized to the monitor) and bind SUPER+SHIFT+T
#   3. copy and enable the Omarchy bar widget
#
# Safe to re-run. Firewall changes require --open-firewall CIDR.
# The portal picker is not changed.
# --remove-desktop removes only the marked Hyprland blocks this script wrote.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ID="io.github.cfaulkingham.omabeam"
PLUGIN_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/$PLUGIN_ID"
HYPRLAND_LUA="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/hyprland.lua"
BINDINGS_LUA="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/bindings.lua"
SHELL_JSON="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/shell.json"
BIN="$PLUGIN_DIR/omarchy-plugin/omabeam"
BIND_KEYS="SUPER + SHIFT + T"
USAGE="Usage: ./install.sh [--backend-only|--remove-desktop|--check-ports] [--subnet CIDR|--open-firewall CIDR]"

export PATH="$HOME/.cargo/bin:$PATH"

BACKEND_ONLY=false
REMOVE_DESKTOP=false
CHECK_PORTS=false
OPEN_FIREWALL=false
MODE=install
FIREWALL_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --backend-only|--remove-desktop|--check-ports)
      [[ $MODE == install ]] || { echo "$USAGE" >&2; exit 2; }
      MODE=$1
      case "$1" in
        --backend-only) BACKEND_ONLY=true ;;
        --remove-desktop) REMOVE_DESKTOP=true ;;
        --check-ports) CHECK_PORTS=true ;;
      esac
      shift ;;
    --subnet|--open-firewall)
      [[ $# -ge 2 ]] || { echo "$1 requires a viewer subnet in CIDR notation." >&2; exit 2; }
      [[ $1 != --open-firewall ]] || OPEN_FIREWALL=true
      FIREWALL_ARGS+=("$1" "$2")
      shift 2 ;;
    --help|-h)
      echo "$USAGE"
      echo "Default installs check TCP 9847 and UDP 9848 and warn if access cannot be verified."
      echo "--check-ports checks without building or installing; sudo authentication is available in a terminal."
      echo "--subnet CIDR checks access from a specific viewer subnet."
      echo "--open-firewall CIDR adds persistent UFW rules for that subnet and verifies them."
      exit 0 ;;
    *) echo "$USAGE" >&2; exit 2 ;;
  esac
done
if $REMOVE_DESKTOP && [[ ${#FIREWALL_ARGS[@]} -gt 0 ]]; then
  echo "--remove-desktop cannot be combined with firewall options." >&2
  exit 2
fi
[[ $(uname -s) == Linux ]] || { echo "OmaBeam installation requires Linux with Omarchy / Hyprland." >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "install.sh: missing required command: $1" >&2
    exit 1
  }
}

check_ports() {
  local extra=()
  if $CHECK_PORTS; then extra+=(--authenticate); fi
  # The conditional expansions also support empty arrays under Bash 3's nounset.
  if python3 "$ROOT/omarchy-plugin/firewall.py" ${FIREWALL_ARGS[@]+"${FIREWALL_ARGS[@]}"} ${extra[@]+"${extra[@]}"}; then
    return 0
  fi
  # A warning must not turn a successful default install into a failed build.
  # Explicit check/open requests report failure so automation can detect it.
  if $CHECK_PORTS || $OPEN_FIREWALL; then return 1; fi
  return 0
}

plugin_on_bar() {
  [[ -f $SHELL_JSON ]] || return 1
  jq -e --arg id "$PLUGIN_ID" '
    (.bar.layout | [.left, .center, .right] | add // [])
    | any(.id == $id)
  ' "$SHELL_JSON" >/dev/null 2>&1
}

edit_hypr() {
  local action=$1
  python3 - "$HYPRLAND_LUA" "$BINDINGS_LUA" "$BIN" "$action" "$BIND_KEYS" <<'PY'
import json
import os
import re
import stat
import subprocess
import sys
import tempfile

hypr_path, bind_path, binary, action, bind_keys = sys.argv[1:6]
MAX = 1_048_576
WINDOW = """-- omabeam (install.sh)
o.window("omabeam", {
  float = true,
  center = true,
  focus_on_activate = false,
  animation = "popin",
  size = { "(monitor_w*3/4)", "(monitor_h*3/4)" },
  max_size = { 980, 560 },
})
"""
BIND = (
    "-- omabeam (install.sh)\n"
    f'o.bind({json.dumps(bind_keys)}, "OmaBeam", {{ launch = {json.dumps(binary)} }})\n'
)
WINDOW_RE = re.compile(
    r"-- omabeam \(install.sh\)\n"
    r'o\.window\("omabeam", \{.*?\}\)\n?',
    re.S,
)
BIND_RE = re.compile(
    r"-- omabeam \(install.sh\)\n"
    r'o\.bind\("SUPER \+ SHIFT \+ T", "OmaBeam", \{ launch = .*? \}\)\n?',
)
flags = (
    os.O_RDONLY
    | getattr(os, "O_NOFOLLOW", 0)
    | getattr(os, "O_NONBLOCK", 0)
    | getattr(os, "O_CLOEXEC", 0)
)


def read_file(path):
    fd = os.open(path, flags)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise SystemExit(f"install.sh: refusing {path}: not a regular file")
        if info.st_size > MAX:
            raise SystemExit(f"install.sh: refusing {path}: too large")
        os.set_blocking(fd, True)
        data = b""
        while len(data) <= MAX:
            chunk = os.read(fd, min(65536, MAX + 1 - len(data)))
            if not chunk:
                break
            data += chunk
        if len(data) > MAX:
            raise SystemExit(f"install.sh: refusing {path}: too large")
        return data.decode()
    finally:
        os.close(fd)


def atomic_write(path, text):
    directory = os.path.dirname(path) or "."
    fd, tmp = tempfile.mkstemp(prefix=".omabeam.", dir=directory)
    try:
        os.fchmod(fd, stat.S_IMODE(os.stat(path).st_mode))
        payload = text.encode()
        view = memoryview(payload)
        while view:
            written = os.write(fd, view)
            view = view[written:]
        os.fsync(fd)
        os.close(fd)
        fd = -1
        os.replace(tmp, path)
        tmp = None
        dirfd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(dirfd)
        finally:
            os.close(dirfd)
    finally:
        if fd >= 0:
            os.close(fd)
        if tmp is not None:
            try:
                os.unlink(tmp)
            except OSError:
                pass


hypr = read_file(hypr_path)
bind = read_file(bind_path)
original = (hypr, bind)

if action == "remove":
    hypr, bind = WINDOW_RE.sub("", hypr), BIND_RE.sub("", bind)
    if (hypr, bind) == original:
        print("  no OmaBeam Hyprland blocks to remove")
        raise SystemExit(0)
else:
    if WINDOW_RE.search(hypr):
        hypr = WINDOW_RE.sub(WINDOW, hypr, count=1)
        print("  updated window rule")
    elif 'o.window("omabeam"' in hypr:
        print("  window rule present (left unchanged)")
    else:
        hypr = hypr.rstrip() + "\n\n" + WINDOW
        print("  appended window rule")
    if bind_keys in bind and "OmaBeam" not in bind:
        print(f"  {bind_keys} is already used; not replacing it", file=sys.stderr)
    elif BIND_RE.search(bind):
        bind = BIND_RE.sub(BIND, bind, count=1)
        print("  updated key bind")
    elif "OmaBeam" in bind:
        print("  key bind present (left unchanged)")
    else:
        bind = bind.rstrip() + "\n\n" + BIND
        print("  appended key bind")

if (hypr, bind) == original:
    raise SystemExit(0)

try:
    atomic_write(hypr_path, hypr)
    atomic_write(bind_path, bind)
except BaseException:
    atomic_write(hypr_path, original[0])
    atomic_write(bind_path, original[1])
    raise

def restore():
    atomic_write(hypr_path, original[0])
    atomic_write(bind_path, original[1])

if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
    print("==> skipping reload (OmaBeam is not installed)")
    raise SystemExit(0)
version = subprocess.run([binary, "--hypr", "version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
if version.returncode != 0:
    print("==> skipping reload (the current Hyprland session is not reachable)")
    raise SystemExit(0)
print("==> reloading Hyprland")
reload = subprocess.run([binary, "--hypr", "reload"], capture_output=True, text=True)
if reload.returncode != 0:
    restore()
    raise SystemExit("install.sh: hyprctl reload failed; previous configuration restored")
errors = subprocess.run([binary, "--hypr", "configerrors"], capture_output=True, text=True)
try:
    payload = json.loads(errors.stdout or "[]")
    bad = [item for item in payload if isinstance(item, str) and item]
except json.JSONDecodeError:
    restore()
    raise SystemExit("install.sh: could not read Hyprland configerrors; previous configuration restored")
if bad:
    restore()
    sys.stderr.write("install.sh: Hyprland configuration errors; previous configuration restored:\n")
    sys.stderr.write("\n".join(bad) + "\n")
    raise SystemExit(1)
if action == "remove":
    print("  removed OmaBeam Hyprland blocks")
PY
}

echo "==> checking tools"
[[ -f $ROOT/manifest.json ]] || {
  echo "install.sh: plugin manifest missing: $ROOT/manifest.json" >&2
  exit 1
}

if $REMOVE_DESKTOP; then
  need python3
  [[ -f $HYPRLAND_LUA && -f $BINDINGS_LUA ]] || {
    echo "install.sh: expected Omarchy Hyprland config in ~/.config/hypr/" >&2
    exit 1
  }
  echo "==> removing Hyprland blocks marked -- omabeam (install.sh)"
  edit_hypr remove
  echo "OmaBeam Hyprland blocks removed. The plugin files remain until:"
  echo "  omarchy plugin remove $PLUGIN_ID"
  exit 0
fi

need python3
# Validate CIDRs before building, editing desktop files, or invoking sudo.
python3 "$ROOT/omarchy-plugin/firewall.py" --validate-only ${FIREWALL_ARGS[@]+"${FIREWALL_ARGS[@]}"}
if $CHECK_PORTS; then
  check_ports
  exit 0
fi

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
  echo "==> building the optional hardware encoder helper"
  if ! CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/omabeam/build}" \
    cargo install --path "$ROOT/crates/omabeam-encoder" --root "$ROOT/omarchy-plugin/native" --locked --force; then
    echo "WARNING: Hardware encoder helper could not be built. Auto mode can use software H.264."
    echo "On Omarchy, install ffmpeg (including its development files) and rerun this installer for GPU encoding."
  fi
elif [[ ! -x $NATIVE ]]; then
  echo "install.sh: this bundle has neither source nor a native binary." >&2
  exit 1
fi
"$NATIVE" --help >/dev/null || {
  echo "install.sh: the native app cannot run here. Check the bundle architecture and runtime dependencies." >&2
  exit 1
}
echo "Check GPU encoding with: $NATIVE --check-encoders"
if $BACKEND_ONLY; then
  echo "OmaBeam native app ready. Open the bar panel and check status again."
  check_ports
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
echo "==> restarting Omarchy shell so $PLUGIN_ID reloads"
omarchy restart shell

echo "==> Hyprland window rule and bind $BIND_KEYS"
edit_hypr apply

echo
echo "OmaBeam installed."
echo "  binary:  $BIN"
echo "  plugin:  $PLUGIN_DIR"
echo "  launch:  $BIND_KEYS  or  $BIN"
echo
echo "Optional: set custom_picker_binary = $BIN in ~/.config/hypr/xdph.conf"
check_ports
echo "Remove desktop bindings with: ./install.sh --remove-desktop"
