#!/usr/bin/env bash
# Start a nested, headless sway for testing svitek without touching the real
# screen. Prints the env vars to export. Usage:
#   eval "$(tests/headless-sway.sh start)"   # sets SWAYSOCK, WAYLAND_DISPLAY, SVITEK_TEST_DIR
#   tests/headless-sway.sh stop
set -euo pipefail
DIR="${SVITEK_TEST_DIR:-${XDG_RUNTIME_DIR:-/tmp}/svitek-test}"
case "${1:-start}" in
start)
  mkdir -p "$DIR"
  cat > "$DIR/config" <<'CFG'
# headless test config
set $mod Mod4
output HEADLESS-1 resolution 1280x720 position 0,0
output HEADLESS-2 resolution 1280x720 position 1280,0
workspace 1 output HEADLESS-1
workspace 2 output HEADLESS-1
workspace 3 output HEADLESS-2
focus_follows_mouse no
default_border pixel 2
bindsym $mod+a exec true
CFG
  # Two headless outputs: the first comes with the backend, the second via `create_output`.
  sock="$DIR/sway-ipc.sock"
  rm -f "$sock"
  env -u WAYLAND_DISPLAY -u DISPLAY SWAYSOCK="$sock" \
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    setsid sway -c "$DIR/config" -d >"$DIR/sway.log" 2>&1 &
  echo $! > "$DIR/sway.pid"
  for i in $(seq 1 50); do
    [ -S "$sock" ] && grep -q "Running compositor on wayland display" "$DIR/sway.log" && break
    sleep 0.1
  done
  [ -S "$sock" ] || { echo "sway did not start; see $DIR/sway.log" >&2; exit 1; }
  disp=$(grep -o "Running compositor on wayland display '[^']*'" "$DIR/sway.log" | head -1 | sed "s/.*'\(.*\)'/\1/")
  SWAYSOCK="$sock" swaymsg create_output >/dev/null || true
  sleep 0.3
  echo "export SWAYSOCK=$sock"
  echo "export WAYLAND_DISPLAY=$disp"
  echo "export SVITEK_TEST_DIR=$DIR"
  ;;
stop)
  if [ -f "$DIR/sway.pid" ]; then kill "$(cat "$DIR/sway.pid")" 2>/dev/null || true; rm -f "$DIR/sway.pid"; fi
  ;;
*) echo "usage: $0 start|stop" >&2; exit 2;;
esac
