#!/usr/bin/env bash
# Start a nested, headless sway for testing svitek without touching the real
# screen. Prints the env vars to export. Usage:
#   eval "$(tests/headless-sway.sh start)"   # sets SWAYSOCK, WAYLAND_DISPLAY, SVITEK_TEST_DIR
#   tests/headless-sway.sh stop
# `start` fails if a test sway is already running in SVITEK_TEST_DIR, and
# `stop` only ever kills a process that really is that sway.
# SVITEK_BIN (optional) adds `bindsym $mod+Tab exec <it> toggle` (and
# $mod+Shift+Tab -> prev) to the generated config.
set -euo pipefail
DIR="${SVITEK_TEST_DIR:-${XDG_RUNTIME_DIR:-/tmp}/svitek-test}"

# Is $1 a live process that is *our* nested sway (started with our config)?
is_our_sway() {
  local pid=${1:-}
  case "$pid" in ''|*[!0-9]*) return 1;; esac
  kill -0 "$pid" 2>/dev/null || return 1
  tr '\0' '\n' < "/proc/$pid/cmdline" 2>/dev/null | grep -qxF "$DIR/config"
}

case "${1:-start}" in
start)
  mkdir -p "$DIR"
  # Refuse to start a second one on top of the first: they would share $DIR,
  # the ipc socket and the pid file, and `stop` could then only find one.
  if [ -f "$DIR/sway.pid" ] && is_our_sway "$(cat "$DIR/sway.pid" 2>/dev/null || true)"; then
    echo "a test sway is already running in $DIR (pid $(cat "$DIR/sway.pid")); run '$0 stop' first" >&2
    exit 1
  fi
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
CFG
  # Optional: a real Mod+Tab binding, so a test can drive the alt-tab gesture
  # through sway itself instead of the control socket. Sway's `exec` inherits
  # sway's environment, so SVITEK_SOCKET must already be exported here for the
  # binding to reach the *test* daemon.
  if [ -n "${SVITEK_BIN:-}" ]; then
    printf 'bindsym $mod+Tab exec %s toggle\n' "$SVITEK_BIN" >> "$DIR/config"
    printf 'bindsym $mod+Shift+Tab exec %s prev\n' "$SVITEK_BIN" >> "$DIR/config"
  fi
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
  # The pid file can easily outlive the process it names, and pids get reused,
  # so never kill on the number alone: check that it is alive and that
  # /proc/<pid>/cmdline still holds the config we generated.
  if [ -f "$DIR/sway.pid" ]; then
    pid=$(cat "$DIR/sway.pid" 2>/dev/null || true)
    rm -f "$DIR/sway.pid"
    if is_our_sway "$pid"; then
      kill "$pid" 2>/dev/null || true
    elif [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      echo "$DIR/sway.pid named pid $pid, which is not the test sway (-c $DIR/config); left alone" >&2
    fi
  fi
  ;;
*) echo "usage: $0 start|stop" >&2; exit 2;;
esac
