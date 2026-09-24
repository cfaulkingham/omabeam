#!/usr/bin/env bash
# Install OmaBeam on this Omarchy/Hyprland machine:
#   1. build the plugin-local native app (or use the release's bundled binary)
#   2. float the picker (sized to the monitor) and the send window, and bind
#      SUPER+SHIFT+T
#   3. copy and enable the Omarchy bar widget
#
# Safe to re-run. Missing UFW allows for the detected LAN are added after a
# sudo prompt. --open-firewall CIDR still selects a specific viewer subnet.
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
NATIVE="$ROOT/omarchy-plugin/native/bin/omabeam"
# AUR installations keep pacman-owned executables outside the user plugin.
if [[ ! -f $ROOT/Cargo.toml && ! -x $NATIVE && -x /usr/lib/omabeam/omabeam ]]; then
  NATIVE="/usr/lib/omabeam/omabeam"
fi
BIND_KEYS="SUPER + SHIFT + T"
USAGE="Usage: ./install.sh [--backend-only|--remove-desktop|--check-ports] [--with-cast] [--subnet CIDR|--open-firewall CIDR]"

export PATH="$HOME/.cargo/bin:$PATH"

BACKEND_ONLY=false
REMOVE_DESKTOP=false
CHECK_PORTS=false
OPEN_FIREWALL=false
WITH_CAST=false
HYPR_MANUAL=false
MODE=install
FIREWALL_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-cast) WITH_CAST=true; shift ;;
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
      echo "Default installs check TCP 9847 and UDP 9848 and add missing UFW rules for the detected LAN after a sudo prompt."
      echo "--check-ports checks without building, installing, or changing rules; sudo authentication is available in a terminal."
      echo "--subnet CIDR checks (and, during install, opens) access from a specific viewer subnet."
      echo "--open-firewall CIDR adds persistent UFW rules for that subnet and fails if they cannot be verified."
      echo "--with-cast builds the optional native Google Cast helper from pinned sources (large download and build)."
      exit 0 ;;
    *) echo "$USAGE" >&2; exit 2 ;;
  esac
done
if $WITH_CAST && { $REMOVE_DESKTOP || $CHECK_PORTS; }; then
  echo "--with-cast applies only to an installation." >&2
  exit 2
fi
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
  local extra=(--authenticate)
  if ! $CHECK_PORTS && ! $OPEN_FIREWALL; then extra+=(--open-missing); fi
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
  # The plugin launcher may not exist yet (a full install edits Hyprland
  # before staging plugin files), so IPC prefers the just-built native binary
  # and falls back to the installed launcher only when that binary is absent.
  local ipc_bin=$BIN
  [[ -x $NATIVE ]] && ipc_bin=$NATIVE
  python3 - "$HYPRLAND_LUA" "$BINDINGS_LUA" "$BIN" "$ipc_bin" "$action" "$BIND_KEYS" <<'PY'
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
from collections import Counter

hypr_path, bind_path, bind_binary, ipc_binary, action, bind_keys = sys.argv[1:7]
MAX = 1_048_576
# The send window ("omabeam-send") follows the picker's rule, so its own size
# wins wherever the picker's class would match it too.
WINDOW = """-- omabeam (install.sh)
o.window("omabeam", {
  float = true,
  center = true,
  focus_on_activate = false,
  animation = "popin",
  size = { "(monitor_w*3/4)", "(monitor_h*3/4)" },
  max_size = { 980, 560 },
})
o.window("omabeam-send", {
  float = true,
  center = true,
  focus_on_activate = false,
  animation = "popin",
  size = { 440, 560 },
})
"""
BIND = (
    "-- omabeam (install.sh)\n"
    f'o.bind({json.dumps(bind_keys)}, "OmaBeam", {{ launch = {json.dumps(bind_binary)} }})\n'
)
# The send window's rule is optional: blocks written before it existed hold
# only the picker's rule, and are upgraded or removed the same way.
WINDOW_RE = re.compile(
    r"-- omabeam \(install.sh\)\n"
    r'o\.window\("omabeam", \{.*?\}\)\n?'
    r'(?:o\.window\("omabeam-send", \{.*?\}\)\n?)?',
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


def unmanaged(path):
    """True if OmaBeam must not edit this path: a symlink, or otherwise not
    a plain regular file. Stow/chezmoi dotfiles commonly symlink these."""
    try:
        info = os.lstat(path)
    except OSError:
        return False
    return stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode)


guarded = [(hypr_path, "hyprland.lua", WINDOW), (bind_path, "bindings.lua", BIND)]
blocked = [item for item in guarded if unmanaged(item[0])]
if blocked:
    # Either file being linked/managed means neither gets edited, so both
    # files' manual instructions are needed -- not just the one that tripped
    # the check -- or the other file silently ends up with no block at all.
    blocked_paths = ", ".join(path for path, _, _ in blocked)
    if action == "remove":
        sys.stderr.write(
            "install.sh: OmaBeam does not edit linked or managed configuration files "
            f"({blocked_paths}); neither file was changed.\n"
            f'Delete the blocks marked "-- omabeam (install.sh)" from {hypr_path} (hyprland.lua) '
            f"and {bind_path} (bindings.lua) by hand.\n"
        )
        raise SystemExit(1)
    print(
        "install.sh: OmaBeam does not edit linked or managed configuration files "
        f"({blocked_paths}); neither file was changed."
    )
    print(f"Add this block to {hypr_path} (hyprland.lua) by hand:")
    print(WINDOW)
    print(f"Add this block to {bind_path} (bindings.lua) by hand:")
    print(BIND)
    # A distinct status (not 0) tells install.sh the edit was skipped, not
    # applied, so its closing banner does not claim the key bind is active.
    raise SystemExit(3)

try:
    hypr = read_file(hypr_path)
    bind = read_file(bind_path)
except OSError as error:
    raise SystemExit(f"install.sh: {error}")
original = (hypr, bind)

if action == "remove":
    hypr, bind = WINDOW_RE.sub("", hypr), BIND_RE.sub("", bind)
    if (hypr, bind) == original:
        print("  no OmaBeam Hyprland blocks to remove")
        raise SystemExit(0)
else:
    if WINDOW_RE.search(hypr):
        hypr = WINDOW_RE.sub(WINDOW, hypr, count=1)
        print("  updated window rules")
    elif 'o.window("omabeam"' in hypr:
        print("  window rule present (left unchanged)")
    else:
        hypr = hypr.rstrip() + "\n\n" + WINDOW
        print("  appended window rules")
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


def normalize(error):
    return re.sub(r"\d+", "#", error)


def partition_errors(baseline, current):
    """Split current errors into ones already in the baseline multiset --
    matched by normalized text, so an error whose line number only shifted
    still counts as the same one -- and genuinely new ones."""
    available = Counter(normalize(error) for error in baseline)
    new, remaining = [], []
    for error in current:
        key = normalize(error)
        if available[key] > 0:
            available[key] -= 1
            remaining.append(error)
        else:
            new.append(error)
    return new, remaining


def query_configerrors(binary):
    result = subprocess.run([binary, "--hypr", "configerrors"], capture_output=True, text=True)
    try:
        payload = json.loads(result.stdout or "[]")
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, list):
        return None
    return [item for item in payload if isinstance(item, str) and item]


def hypr_status(binary):
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        return "missing"
    version = subprocess.run([binary, "--hypr", "version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return "ready" if version.returncode == 0 else "unreachable"


def restore():
    atomic_write(hypr_path, original[0])
    atomic_write(bind_path, original[1])


# Read the baseline before writing anything, so errors Hyprland already had
# do not get blamed on this edit.
status = hypr_status(ipc_binary)
baseline = query_configerrors(ipc_binary) if status == "ready" else None
if baseline is None:
    baseline = []

try:
    atomic_write(hypr_path, hypr)
    atomic_write(bind_path, bind)
except BaseException:
    restore()
    raise

if status == "missing":
    print("==> skipping reload (OmaBeam is not installed)")
    raise SystemExit(0)
if status == "unreachable":
    print("==> skipping reload (the current Hyprland session is not reachable)")
    raise SystemExit(0)

print("==> reloading Hyprland")
reload = subprocess.run([ipc_binary, "--hypr", "reload"], capture_output=True, text=True)
if reload.returncode != 0:
    restore()
    raise SystemExit("install.sh: hyprctl reload failed; previous configuration restored")

current = query_configerrors(ipc_binary)
if current is None:
    restore()
    raise SystemExit("install.sh: could not read Hyprland configerrors; previous configuration restored")

new_errors, remaining_errors = partition_errors(baseline, current)
if new_errors:
    restore()
    sys.stderr.write("install.sh: Hyprland reported new configuration errors; previous configuration restored:\n")
    sys.stderr.write("\n".join(new_errors) + "\n")
    raise SystemExit(1)
if remaining_errors:
    print("Hyprland already reported these configuration errors before OmaBeam changed anything:")
    for error in remaining_errors:
        print(f"  {error}")
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
  if $WITH_CAST; then
    for tool in python3 git pkg-config; do need "$tool"; done
    CAST_CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/omabeam/cast"
    echo "==> building the optional native Google Cast helper"
    python3 "$ROOT/scripts/build-cast.py" --sync --cache "$CAST_CACHE"
    install -m 755 "$CAST_CACHE/openscreen/out/omabeam/omabeam-cast" "$ROOT/omarchy-plugin/native/bin/omabeam-cast"
    mkdir -p "$ROOT/licenses/cast"
    cp -R "$CAST_CACHE/notices/." "$ROOT/licenses/cast/"
  fi
elif [[ ! -x $NATIVE ]]; then
  echo "install.sh: this bundle has neither source nor a native binary." >&2
  exit 1
fi
CAST_NATIVE="$(dirname "$NATIVE")/omabeam-cast"
if [[ -x $CAST_NATIVE ]]; then
  case "$("$CAST_NATIVE" --version)" in
    "omabeam-cast protocol=1 "*) ;;
    *) echo "install.sh: incompatible native Cast helper; rebuild it with --with-cast." >&2; exit 1 ;;
  esac
elif $WITH_CAST; then
  echo "install.sh: this bundle does not include the native Cast helper." >&2
  exit 1
fi
"$NATIVE" --help >/dev/null || {
  echo "install.sh: the native app cannot run here. Check the bundle architecture and runtime dependencies." >&2
  exit 1
}
echo "Check GPU encoding with: $NATIVE --check-encoders"
ENCODER_NATIVE="$(dirname "$NATIVE")/omabeam-encoder"
if [[ -x $ENCODER_NATIVE ]] && command -v ldd >/dev/null 2>&1; then
  # ldd exits non-zero for a non-dynamic executable (e.g. a test double); the
  # `|| true` keeps that from tripping `set -e` through the pipefail'd pipe.
  missing=$(ldd "$ENCODER_NATIVE" 2>/dev/null | awk '/not found/ { printf "%s%s", (n++ ? " " : ""), $1 }') || true
  if [[ -n $missing ]]; then
    echo "WARNING: The hardware encoder helper cannot load on this system (missing: $missing)."
    echo "H.264 will use software encoding. Rebuild from source (./install.sh --backend-only in a source checkout) or install a bundle built for this system's FFmpeg."
  fi
fi
if $BACKEND_ONLY; then
  echo "OmaBeam native app ready. Open the bar panel and check status again."
  check_ports
  exit 0
fi

echo "==> Hyprland window rules and bind $BIND_KEYS"
hypr_rc=0
edit_hypr apply || hypr_rc=$?
case $hypr_rc in
  0) ;;
  3) HYPR_MANUAL=true ;;
  *) exit "$hypr_rc" ;;
esac

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

echo
echo "OmaBeam installed."
echo "  binary:  $BIN"
echo "  plugin:  $PLUGIN_DIR"
if $HYPR_MANUAL; then
  echo "  launch:  $BIN"
  echo "  Hyprland window rules and key bind were not added automatically; add them by hand (see above)."
else
  echo "  launch:  $BIND_KEYS  or  $BIN"
fi
echo
echo "Optional: set custom_picker_binary = $BIN in ~/.config/hypr/xdph.conf"
check_ports
echo "Remove desktop bindings with: ./install.sh --remove-desktop"
