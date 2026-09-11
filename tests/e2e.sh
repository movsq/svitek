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
# "yes" when the focused row/card marker (the border, #89b4fa) shows up in the
# vertical slice x0 <= x < x1 of a screenshot — that colour appears nowhere else
# on screen, so it is how we see both *that* the panel is up and *where*.
focus_marker_in() { # focus_marker_in <png> <x0> <x1>
  if [ "$HAVE_PIL" = 1 ]; then
    python3 -c "
from PIL import Image
px=list(Image.open('$1').convert('RGB').crop(($2,0,$3,720)).get_flattened_data())
print('yes' if sum(1 for r,g,b in px if b>200 and 100<r<190 and g>150)>500 else 'no')"
  else
    n=$(magick "$1" -crop "$(($3 - $2))x720+$2+0" +repage \
          -fill black -fuzz 12% +opaque '#89b4fa' -fill white -opaque '#89b4fa' \
          -colorspace gray -format '%[fx:int(mean*100000)]' info:)
    [ "$n" -gt 100 ] && echo yes || echo no
  fi
}
panel_visible() { # "yes" when the focused-row border is on screen
  grim -o HEADLESS-1 "$SVITEK_TEST_DIR/probe.png"
  focus_marker_in "$SVITEK_TEST_DIR/probe.png" 0 "$PANEL_W"
}
# The name of the workspace sway has focused right now. Shell only (the
# ImageMagick path of this script must work without python3): every workspace
# object in `get_workspaces` prints its "name" before its "focused".
focused_ws() { swaymsg -t get_workspaces | focused_ws_in /dev/stdin; }
# The same, from a saved `get_workspaces` dump: the hold-mode probes run from
# inside the injector (while the modifier is held) and are read back afterwards.
focused_ws_in() {
  grep -oE '"name": "[^"]*"|"focused": (true|false)' "$1" |
    awk -F'"' '/"name"/ { n = $4 } /focused/ { if ($0 ~ /true/) { print n; exit } }'
}
foot_on() { # foot_on <workspace> <title> [script-to-run-in-it]
  swaymsg "workspace $1" >/dev/null
  swaymsg exec "foot -T $2 -e sh ${3:-$SVITEK_TEST_DIR/idle.sh}" >/dev/null
  sleep 1.2
}
# Start the daemon and wait until it has sway's state. Each run gets its own log
# (so `wait_log` after a restart cannot match a line from the previous daemon,
# and the log check at the end still sees every run). The config file must
# already be written: svitek reads it once, at startup.
start_daemon() {
  DAEMON_RUNS=$((DAEMON_RUNS + 1))
  LOG="$SVITEK_TEST_DIR/svitek-$DAEMON_RUNS.log"
  RUST_LOG=svitek=debug setsid $SVITEK >"$LOG" 2>&1 &
  DAEMON_PID=$!
  DAEMON_STARTED=1
  wait_log 'listening on' || return 1
  wait_log 'initial snapshot' || return 1
}

# write_config <extra lines…>: the daemon reads this once, at startup. Every
# pixel probe up to the "centered layout" step measures the left edge of the
# output, so the column layout is pinned here rather than following the default.
write_config() {
  { echo 'position = "left"'; [ $# -gt 0 ] && printf '%s\n' "$@"; } \
    > "$XDG_CONFIG_HOME/svitek/config.toml"
}

PANEL_W=546   # `position = "left"`: 240 thumb + 260 text + 46 chrome
DAEMON_RUNS=0
# Where a row is, with `position = "left"` and the default 240 px thumbnails on
# the 1280x720 headless output: the list pads 8 px, and a row is 135 px of
# thumbnail (240 * 9/16) + 8 px padding + 2 px border top and bottom = 155 px,
# with 6 px between rows. So row 1 spans y 8..163 and row 2 y 169..324; click
# their centres. (Read off a grim screenshot of the panel, not just computed.)
ROW_X=270
ROW1_Y=85
ROW2_Y=246
HAVE_PIL=0; python3 -c 'import PIL' 2>/dev/null && HAVE_PIL=1
[ "$HAVE_PIL" = 1 ] || command -v magick >/dev/null || { echo "need python3+PIL or ImageMagick"; exit 2; }

step "build"
cargo build --release || exit 2
# The input injector is a separate, test-only crate (see tools/inject/src/main.rs);
# `cargo build` in the root deliberately does not build it.
[ -x ./target/release/inject ] || CARGO_TARGET_DIR=$PWD/target \
  cargo build --release --manifest-path tools/inject/Cargo.toml || exit 2
INJECT=./target/release/inject

step "headless sway"
rm -rf "$SVITEK_TEST_DIR"; mkdir -p "$SVITEK_TEST_DIR/cfg/svitek"
# Both of these have to be exported *before* sway starts: sway's `exec` inherits
# sway's environment, so this is what makes the `bindsym $mod+Tab exec svitek
# toggle` that headless-sway.sh adds for SVITEK_BIN talk to *our* daemon.
export SVITEK_SOCKET="$SVITEK_TEST_DIR/svitek.sock"
export SVITEK_BIN="$PWD/target/release/svitek"
eval "$(tests/headless-sway.sh start)" || exit 2
unset I3SOCK
export XDG_CONFIG_HOME="$SVITEK_TEST_DIR/cfg"     # our own config, whatever the user has
write_config

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
start_daemon || exit 1
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

step "click selects a workspace"
# (a) close_on_select defaults to true: clicking a row is Enter for that row —
# the panel closes and sway is left on it. (The injector's pointer motion also
# arms a hover preview of that row on the way in; the click has to win over it,
# which is the interesting half of this check.)
swaymsg workspace 1 >/dev/null; sleep 0.6
$SVITEK show; sleep 0.8
check "$(panel_visible)" yes "the panel is up before the click"
$INJECT pointer HEADLESS-1 $ROW_X $ROW2_Y click >/dev/null
sleep 1
check "$(panel_visible)" no "a click on workspace 2's row closes the panel"
check "$(focused_ws)" 2 "the click leaves sway on workspace 2"
if grep -q 'click commits workspace "2"' "$LOG"; then pass "the daemon logged the commit"
else fail "no 'click commits workspace \"2\"' in $LOG"; fi

# (b) close_on_select = false: the click switches and the panel stays up. The
# config is only read at startup, so the daemon has to be restarted for it.
$SVITEK quit; sleep 1
write_config 'close_on_select = false'
start_daemon || exit 1
swaymsg workspace 2 >/dev/null; sleep 0.8
$SVITEK show; sleep 0.8
$INJECT pointer HEADLESS-1 $ROW_X $ROW1_Y click >/dev/null
sleep 1
check "$(panel_visible)" yes "close_on_select = false keeps the panel open"
check "$(focused_ws)" 1 "the click still switches to workspace 1"
$SVITEK hide; sleep 0.5
check "$(panel_visible)" no "hide closes it"

step "centered layout"
# The default `position` is "center": a horizontal strip of cards floating in
# the middle of the output, not a column on an edge. The config is read once at
# startup, so this needs a fresh daemon (which also starts with an empty
# thumbnail cache — the cards show the "no preview yet" placeholder, and the
# focus marker is what we are probing for anyway).
$SVITEK quit; sleep 1
DAEMON_STARTED=
: > "$XDG_CONFIG_HOME/svitek/config.toml"        # no keys at all => the defaults
start_daemon || exit 1
swaymsg workspace 1 >/dev/null; sleep 1.2
$SVITEK toggle; wait_log 'panel shown on HEADLESS-1'; sleep 0.7
grim -o HEADLESS-1 "$SVITEK_TEST_DIR/center.png"
check "$(focus_marker_in "$SVITEK_TEST_DIR/center.png" 427 853)" yes \
      "the focused card is in the middle third of the output"
check "$(focus_marker_in "$SVITEK_TEST_DIR/center.png" 0 200)" no \
      "nothing of the strip reaches the left edge"

# A click on the scrim — well clear of the strip, which is centred vertically —
# still dismisses the panel in this layout.
$INJECT pointer HEADLESS-1 40 40 click >/dev/null 2>&1; sleep 0.7
check "$(panel_visible)" no "a click outside the strip hides it"

# One wheel detent moves the selection one card to the *right*, i.e. to the next
# workspace, and previews it for real; hiding then puts us back on the origin.
check "$(focused_ws)" 1 "the origin workspace before the wheel"
$SVITEK toggle; wait_log 'panel shown on HEADLESS-1'; sleep 0.7
$INJECT scroll HEADLESS-1 640 360 1 >/dev/null 2>&1; sleep 0.9
check "$(focused_ws)" 2 "a wheel step down previews the card to the right"
$SVITEK hide; sleep 0.7
check "$(focused_ws)" 1 "hiding reverts to the origin workspace"

step "hold mode"
# `mode = "hold"` is alt-tab: sway consumes Mod+Tab and runs the binding, so the
# panel only ever sees another `svitek toggle` — which steps the selection
# instead of hiding — and letting go of Super commits. Driven through the *real*
# key path: headless-sway.sh bound $mod+Tab to `svitek toggle` (SVITEK_BIN was
# exported before sway started, so the binding reaches this daemon), and
# `inject hold` holds Super down around the whole sequence. That is what proves
# both halves: sway never delivers the Tab to the panel, and the binding fires
# again for every Tab while Super stays down.
#
# The probes run from inside the injector, while Super is still held: each one
# saves a screenshot and a `get_workspaces` dump that is read back below.
$SVITEK quit; sleep 1
DAEMON_STARTED=
# `hold_selects_next = false` here so the first Mod+Tab only opens the panel and
# the sequences below count from the origin; the default is checked in (f).
write_config 'mode = "hold"' 'hold_selects_next = false'   # `position = "left"` too, so PANEL_W still holds
start_daemon || exit 1
cat > "$SVITEK_TEST_DIR/probe.sh" <<'PROBE'
#!/bin/sh
grim -o HEADLESS-1 "$SVITEK_TEST_DIR/hold-$1.png"
swaymsg -t get_workspaces > "$SVITEK_TEST_DIR/hold-$1.json"
PROBE
chmod +x "$SVITEK_TEST_DIR/probe.sh"
PROBE="$SVITEK_TEST_DIR/probe.sh"
SUPER=125   # evdev KEY_LEFTMETA
MOD4=64     # the xkb modifier mask sway matches $mod against
TAB=15
hold_panel() { focus_marker_in "$SVITEK_TEST_DIR/hold-$1.png" 0 "$PANEL_W"; }
hold_ws() { focused_ws_in "$SVITEK_TEST_DIR/hold-$1.json"; }

# (a) Super down, Tab, Tab, Super up.
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold $SUPER $MOD4 1200 \
  key:$TAB sleep:1200 run:"$PROBE a1" \
  key:$TAB sleep:1200 run:"$PROBE a2" >/dev/null 2>&1
sleep 0.8
check "$(hold_panel a1)" yes "Mod+Tab shows the panel"
check "$(hold_ws a1)" 1 "the selection starts on the workspace we came from"
check "$(hold_panel a2)" yes "a second Mod+Tab does not hide the panel"
check "$(hold_ws a2)" 2 "it steps the selection on and previews workspace 2"
check "$(panel_visible)" no "releasing Super hides the panel"
check "$(focused_ws)" 2 "releasing Super commits workspace 2"
if grep -q 'modifier release commits workspace "2"' "$LOG"; then pass "the daemon logged the commit"
else fail "no 'modifier release commits workspace \"2\"' in $LOG"; fi

# (b) One Tab too many wraps: with two workspaces the third Tab is back on the
# one we started from, so the release is a plain hide that goes nowhere.
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold $SUPER $MOD4 1200 \
  key:$TAB sleep:900 key:$TAB sleep:900 key:$TAB sleep:1200 run:"$PROBE b1" >/dev/null 2>&1
sleep 0.8
check "$(hold_panel b1)" yes "the panel is still up after three Mod+Tabs"
check "$(hold_ws b1)" 1 "the third step wraps round to workspace 1"
check "$(panel_visible)" no "releasing Super hides it"
check "$(focused_ws)" 1 "and leaves sway on workspace 1"
if grep -q 'modifier release commits the origin workspace' "$LOG"; then
  pass "committing the origin is logged as a plain hide"
else fail "no 'modifier release commits the origin workspace' in $LOG"; fi

# (c) `next` / `prev` work in both modes: from hidden they show the panel, from
# up they step the selection. `hide` still reverts, modifier held or not.
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold $SUPER $MOD4 1200 \
  run:"$SVITEK_BIN next" sleep:1000 run:"$PROBE c1" \
  run:"$SVITEK_BIN next" sleep:1000 run:"$PROBE c2" \
  run:"$SVITEK_BIN prev" sleep:1000 run:"$PROBE c3" \
  run:"$SVITEK_BIN hide" sleep:800 run:"$PROBE c4" >/dev/null 2>&1
sleep 0.5
check "$(hold_panel c1)" yes "next shows the panel when it is down"
check "$(hold_ws c1)" 1 "showing it does not move the selection"
check "$(hold_ws c2)" 2 "next steps the selection on"
check "$(hold_ws c3)" 1 "prev steps it back"
check "$(hold_panel c4)" no "hide closes it"
check "$(hold_ws c4)" 1 "and leaves sway where the panel was opened"

# (d) Two Mod+Tabs in one breath, possibly inside the ~60 ms the show waits for
# its fresh frame: the second one must be queued behind the pending show, never
# swallowed. Either way the selection ends up one workspace on.
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold $SUPER $MOD4 1200 key:$TAB key:$TAB sleep:1200 run:"$PROBE d1" >/dev/null 2>&1
sleep 0.8
check "$(hold_panel d1)" yes "two quick Mod+Tabs leave the panel up"
check "$(hold_ws d1)" 2 "and the selection one workspace on"
check "$(focused_ws)" 2 "releasing Super commits it"

# (e) The race: a tap so short that Super is already up before the surface has
# keyboard focus, so no release event will ever arrive. Here it is a `toggle`
# with no modifier held at all (the injector still creates the keyboard, or the
# seat would have none and the layer surface would never take focus). The panel
# must notice and commit at once rather than sitting there forever.
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold 0 0 1200 run:"$SVITEK_BIN toggle" sleep:1000 run:"$PROBE e1" >/dev/null 2>&1
check "$(hold_panel e1)" no "a panel opened with the modifier already up closes itself"
check "$(focused_ws)" 1 "and leaves sway where it was"
if grep -q 'the modifier was already up when the panel mapped' "$LOG"; then
  pass "the daemon logged the already-up modifier"
else fail "no 'the modifier was already up' in $LOG"; fi

# (f) The default, `hold_selects_next = true`: the opening press already selects
# the next workspace, so a single Mod+Tab tap is a switch — alt-tab's rule.
$SVITEK quit; sleep 1
DAEMON_STARTED=
write_config 'mode = "hold"'
start_daemon || exit 1
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold $SUPER $MOD4 1200 key:$TAB sleep:1200 run:"$PROBE f1" >/dev/null 2>&1
sleep 0.8
check "$(hold_panel f1)" yes "with hold_selects_next the first Mod+Tab shows the panel"
check "$(hold_ws f1)" 2 "and already previews the next workspace"
check "$(panel_visible)" no "releasing Super hides it"
check "$(focused_ws)" 2 "and lands on workspace 2"
# The quick tap: Super is up again almost at once, and that alone must switch.
swaymsg workspace 1 >/dev/null; sleep 1
$INJECT hold $SUPER $MOD4 1200 key:$TAB sleep:40 >/dev/null 2>&1
sleep 1.2
check "$(panel_visible)" no "a quick tap does not leave the panel up"
check "$(focused_ws)" 2 "a quick Mod+Tab tap switches to the next workspace"

# A picture of the interesting moment, for a human to look at.
if [ -d "$HOME/.cache/svitek-worktrees/shots" ]; then
  cp "$SVITEK_TEST_DIR/hold-a2.png" "$HOME/.cache/svitek-worktrees/shots/hold-after-second-tab.png"
fi

step "quit"
$SVITEK quit; sleep 1
# Only *our* daemon: the developer may well have their own svitek running.
kill -0 "$DAEMON_PID" 2>/dev/null && fail "test daemon (pid $DAEMON_PID) is still alive" \
                              || pass "test daemon exited"
[ -e "$SVITEK_SOCKET" ] && fail "control socket $SVITEK_SOCKET was left behind" \
                        || pass "control socket removed"
DAEMON_STARTED=

step "daemon log"
if grep -E '(WARN|ERROR) +svitek' "$SVITEK_TEST_DIR"/svitek-*.log; then
  fail "svitek logged warnings/errors (above)"
else pass "no svitek warnings or errors"; fi

exit "$FAILED"
