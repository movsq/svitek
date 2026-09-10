#!/usr/bin/env bash
# End-to-end smoke test for svitek against a nested headless sway.
#
#   tests/e2e.sh          # builds, runs, cleans up; exit 0 = everything passed
#
# Never touches the sway session you are sitting in: it starts its own headless
# compositor (tests/headless-sway.sh), its own control socket and its own
# XDG_CONFIG_HOME, and unsets I3SOCK (swayipc prefers it over SWAYSOCK).
# Needs: sway, foot, grim, and either python3+PIL or ImageMagick for the
# thumbnail pixel check.
set -uo pipefail

cd "$(dirname "$0")/.."
export SVITEK_TEST_DIR="${SVITEK_TEST_DIR:-${XDG_RUNTIME_DIR:-/tmp}/svitek-e2e-$$}"
SVITEK=./target/release/svitek
FAILED=0

pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
step() { printf '\n== %s\n' "$*"; }
check() { if [ "$1" = "$2" ]; then pass "$3 ($2)"; else fail "$3: expected '$2', got '$1'"; fi; }

cleanup() {
  local rc=$?
  [ "$rc" != 0 ] && FAILED=1   # an early `exit N` is a failure too
  [ -n "${DAEMON_STARTED:-}" ] && $SVITEK quit >/dev/null 2>&1
  # Kill only the windows *this* nested sway owns, never anything of the user's.
  swaymsg -t get_tree 2>/dev/null | grep -oE '"pid": [0-9]+' | grep -oE '[0-9]+' |
    while read -r p; do kill "$p" 2>/dev/null; done
  tests/headless-sway.sh stop >/dev/null 2>&1
  sleep 0.3
  [ "$FAILED" = 0 ] && printf '\n\033[32mall e2e checks passed\033[0m\n' \
                    || printf '\n\033[31me2e checks FAILED\033[0m (artifacts in %s)\n' "$SVITEK_TEST_DIR"
}
trap cleanup EXIT

# Green pixels (0-100 %) in the thumbnail column *below the first row*, i.e.
# in the previews of the workspaces we are not on. Nothing else in the panel is
# green, so this is how we see what those rows are showing.
THUMBS=270x560+0+160
green_pct() {
  if [ "$HAVE_PIL" = 1 ]; then
    python3 -c "
from PIL import Image
px=list(Image.open('$1').convert('RGB').crop((0,160,270,720)).get_flattened_data())
print(sum(1 for r,g,b in px if g>100 and g>r+60 and g>b+60)*100//len(px))"
  else
    magick "$1" -crop "$THUMBS" +repage \
      -fill black -fuzz 25% +opaque '#00be00' -fill white -opaque '#00be00' \
      -colorspace gray -format '%[fx:int(mean*100)]' info:
  fi
}
# Wait until $1 (a grep -E pattern) shows up in the daemon log, or fail.
wait_log() {
  for _ in $(seq 1 "${2:-50}"); do
    grep -qE "$1" "$LOG" && return 0
    sleep 0.1
  done
  fail "timed out waiting for log line /$1/"; return 1
}
panel_visible() { # "yes" when the focused-row border is on screen
  grim -o HEADLESS-1 "$SVITEK_TEST_DIR/probe.png"
  if [ "$HAVE_PIL" = 1 ]; then
    python3 -c "
from PIL import Image
px=list(Image.open('$SVITEK_TEST_DIR/probe.png').convert('RGB').crop((0,0,$PANEL_W,720)).get_flattened_data())
print('yes' if sum(1 for r,g,b in px if b>200 and 100<r<190 and g>150)>500 else 'no')"
  else
    n=$(magick "$SVITEK_TEST_DIR/probe.png" -crop "${PANEL_W}x720+0+0" +repage \
          -fill black -fuzz 12% +opaque '#89b4fa' -fill white -opaque '#89b4fa' \
          -colorspace gray -format '%[fx:int(mean*100000)]' info:)
    [ "$n" -gt 100 ] && echo yes || echo no
  fi
}
foot_on() { # foot_on <workspace> <title> [script-to-run-in-it]
  swaymsg "workspace $1" >/dev/null
  swaymsg exec "foot -T $2 -e sh ${3:-$SVITEK_TEST_DIR/idle.sh}" >/dev/null
  sleep 1.2
}

PANEL_W=546   # default config: 240 thumb + 260 text + 46 chrome
HAVE_PIL=0; python3 -c 'import PIL' 2>/dev/null && HAVE_PIL=1
[ "$HAVE_PIL" = 1 ] || command -v magick >/dev/null || { echo "need python3+PIL or ImageMagick"; exit 2; }

step "build"
cargo build --release || exit 2

step "headless sway"
rm -rf "$SVITEK_TEST_DIR"; mkdir -p "$SVITEK_TEST_DIR/cfg"
eval "$(tests/headless-sway.sh start)" || exit 2
unset I3SOCK
export XDG_CONFIG_HOME="$SVITEK_TEST_DIR/cfg"     # defaults, whatever the user has
export SVITEK_SOCKET="$SVITEK_TEST_DIR/svitek.sock"
LOG="$SVITEK_TEST_DIR/svitek.log"

echo 'sleep 4000' > "$SVITEK_TEST_DIR/idle.sh"
# Fills its whole terminal with green cells: a workspace change a thumbnail
# cannot miss. (`clear` after an SGR does not repaint with that background.)
cat > "$SVITEK_TEST_DIR/green.sh" <<'GREEN'
printf '\033[48;2;0;190;0m'
i=0; while [ $i -lt 80 ]; do printf '%300s\n' ' '; i=$((i+1)); done
sleep 4000
GREEN

step "client without a daemon"
$SVITEK toggle >/dev/null 2>&1; check "$?" 1 "toggle exits 1 when nothing is running"

step "daemon"
RUST_LOG=svitek=debug setsid $SVITEK >"$LOG" 2>&1 &
DAEMON_PID=$!
DAEMON_STARTED=1
wait_log 'listening on' || exit 1
wait_log 'initial snapshot' || exit 1
foot_on 1 ALPHA
foot_on 2 BRAVO
swaymsg workspace 1 >/dev/null; sleep 1

step "single instance"
RUST_LOG=off $SVITEK >"$SVITEK_TEST_DIR/second.log" 2>&1
check "$?" 1 "a second daemon refuses to start"

step "toggle / show / hide"
$SVITEK toggle; wait_log 'panel shown on HEADLESS-1'; sleep 0.4
check "$(panel_visible)" yes "toggle shows the panel"
$SVITEK toggle; sleep 0.5
check "$(panel_visible)" no  "toggle again hides it"
$SVITEK show;   sleep 0.6
check "$(panel_visible)" yes "show"
$SVITEK hide;   sleep 0.5
check "$(panel_visible)" no  "hide"

step "thumbnail rule: 1 -> 2 -> change 2 -> 1"
$SVITEK toggle; sleep 0.7
grim -o HEADLESS-1 "$SVITEK_TEST_DIR/before.png"
check "$(green_pct "$SVITEK_TEST_DIR/before.png")" 0 "no green in any thumbnail yet"
$SVITEK hide; sleep 0.4
# Change what workspace 2 shows, give the background capture time to file it.
foot_on 2 GREEN "$SVITEK_TEST_DIR/green.sh"
sleep 1
swaymsg workspace 1 >/dev/null; sleep 1.2
$SVITEK toggle; wait_log 'panel shown'; sleep 0.7
grim -o HEADLESS-1 "$SVITEK_TEST_DIR/after.png"
G=$(green_pct "$SVITEK_TEST_DIR/after.png")
if [ "$G" -ge 5 ]; then pass "workspace 2's row shows the CHANGED content (${G}% green)"
else fail "workspace 2's thumbnail is stale: only ${G}% green (see $SVITEK_TEST_DIR/after.png)"; fi
$SVITEK hide; sleep 0.4

step "quit"
$SVITEK quit; sleep 1
# Only *our* daemon: the developer may well have their own svitek running.
kill -0 "$DAEMON_PID" 2>/dev/null && fail "test daemon (pid $DAEMON_PID) is still alive" \
                              || pass "test daemon exited"
[ -e "$SVITEK_SOCKET" ] && fail "control socket $SVITEK_SOCKET was left behind" \
                        || pass "control socket removed"
DAEMON_STARTED=

step "daemon log"
if grep -E '(WARN|ERROR) +svitek' "$LOG"; then fail "svitek logged warnings/errors (above)"
else pass "no svitek warnings or errors"; fi

exit "$FAILED"
