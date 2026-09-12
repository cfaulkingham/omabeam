#!/bin/sh
# A private, headless compositor; does not use or change the user's desktop.
set -eu
binary=${1:-target/debug/omabeam}
review_runtime=$(mktemp -d)
chmod 700 "$review_runtime"
export XDG_RUNTIME_DIR="$review_runtime"
unset WAYLAND_DISPLAY SWAYSOCK
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
cat > "$review_runtime/sway.conf" <<'CONFIG'
output HEADLESS-1 mode 640x480
output HEADLESS-1 bg #336699 solid_color
CONFIG
sway --unsupported-gpu -c "$review_runtime/sway.conf" > "$review_runtime/sway.log" 2>&1 &
sway_pid=$!
cleanup() { kill "$sway_pid" 2>/dev/null || true; wait "$sway_pid" 2>/dev/null || true; rm -rf "$review_runtime"; }
trap cleanup EXIT INT TERM
attempt=0
while [ "$attempt" -lt 100 ]; do
    for socket in "$review_runtime"/wayland-*; do
        if [ -S "$socket" ]; then export WAYLAND_DISPLAY="$socket"; break 2; fi
    done
    if ! kill -0 "$sway_pid" 2>/dev/null; then cat "$review_runtime/sway.log"; exit 1; fi
    sleep 0.1
    attempt=$((attempt + 1))
done
if [ -z "${WAYLAND_DISPLAY:-}" ]; then cat "$review_runtime/sway.log"; exit 1; fi
python3 tests/smoke.py --binary "$binary" --capture-output HEADLESS-1 --capture-scale 1
for sway_socket in "$review_runtime"/sway-ipc.*.sock; do
    if [ -S "$sway_socket" ]; then
        swaymsg -s "$sway_socket" output HEADLESS-1 scale 2 >/dev/null
        python3 tests/smoke.py --binary "$binary" --capture-output HEADLESS-1 --capture-scale 2
        python3 tests/webrtc.py --binary "$binary" --capture-output HEADLESS-1 --sway-socket "$sway_socket"
        exit 0
    fi
done
echo "Could not find the test compositor's IPC socket" >&2
exit 1
