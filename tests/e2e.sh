#!/usr/bin/env bash
# End-to-end smoke test for svitek against a nested headless sway.
#
#   tests/e2e.sh          # builds, runs, cleans up; exit 0 = everything passed
#
# Never touches the sway session you are sitting in. The first thing it does is
# unset SWAYSOCK, I3SOCK, WAYLAND_DISPLAY and DISPLAY, so nothing here — not
# even the EXIT trap on an early failure — can reach your compositor; it then
# starts its own headless sway (tests/headless-sway.sh) with its own control
# socket and its own XDG_CONFIG_HOME. (I3SOCK matters because swayipc prefers
# it over SWAYSOCK.)
#
# Needs:
#   * sway, foot, grim, swaymsg, setsid
#   * xkbcli (libxkbcommon-tools) — the injector compiles its keymap with it
#   * a Rust toolchain, plus the gtk4 and gtk4-layer-shell development
#     packages: the script builds svitek and the test injector from source
#   * python3 with Pillow, or ImageMagick 7 (the `magick` command — ImageMagick
#     6's `convert` is not enough), for the pixel probes
#
# Takes roughly 100 s. It writes screenshots, `get_workspaces` dumps and the
# daemon logs to $SVITEK_TEST_DIR and leaves them there whether it passes or
# fails; the final line says where. Set SVITEK_TEST_DIR to choose the place
# (it must be an absolute path whose last component starts with `svitek-`: the
# script wipes it before the run).
set -uo pipefail

# Before anything else, and before the EXIT trap below is armed: forget the
# user's session completely. Everything after this line can only talk to the
# nested sway, because there is nothing else left to talk to.
unset SWAYSOCK I3SOCK WAYLAND_DISPLAY DISPLAY

cd "$(dirname "$0")/.."
export SVITEK_TEST_DIR="${SVITEK_TEST_DIR:-${XDG_RUNTIME_DIR:-/tmp}/svitek-e2e-$$}"
SVITEK=./target/release/svitek
NESTED_SOCK="$SVITEK_TEST_DIR/sway-ipc.sock"
FAILED=0

pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
step() { printf '\n== %s\n' "$*"; }
check() { if [ "$1" = "$2" ]; then pass "$3 ($2)"; else fail "$3: expected '$2', got '$1'"; fi; }

# --- preflight -------------------------------------------------------------
# Everything the run needs, checked in one go with one message, before the
# trap is armed and before a single process is started.
HAVE_PY3=0; command -v python3 >/dev/null 2>&1 && HAVE_PY3=1
HAVE_PIL=0; [ "$HAVE_PY3" = 1 ] && python3 -c 'import PIL' 2>/dev/null && HAVE_PIL=1
missing=
for t in sway foot grim cargo xkbcli setsid swaymsg; do
  command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
done
[ "$HAVE_PIL" = 1 ] || command -v magick >/dev/null 2>&1 ||
  missing="$missing python3+Pillow-or-ImageMagick-7(magick)"
if [ -n "$missing" ]; then
  echo "tests/e2e.sh: need$missing" >&2
  echo "(needs sway foot grim cargo xkbcli setsid swaymsg, plus python3 with Pillow or ImageMagick 7," >&2
  echo " and the gtk4 / gtk4-layer-shell development packages to build svitek)" >&2
  exit 2
fi

# The one destructive thing this script does is `rm -rf "$SVITEK_TEST_DIR"`,
# and SVITEK_TEST_DIR comes from the environment. Refuse anything that is not
# obviously a scratch directory of ours.
assert_scratch_dir() {
  local d=$SVITEK_TEST_DIR why= base parent
  # Strip trailing slashes, but only down to "/" itself, which must stay
  # recognisable as "/" rather than turning into the empty string.
  while [ "${d%/}" != "$d" ] && [ "$d" != "/" ]; do d=${d%/}; done
  base=${d##*/}; parent=${d%/*}
  if [ -z "$d" ]; then why="it is empty"
  elif [ "$d" = "/" ]; then why="it is /"
  elif [ "${d#/}" = "$d" ]; then why="it is not an absolute path"
  elif [ -n "${HOME:-}" ] && [ "$d" = "${HOME%/}" ]; then why="it is \$HOME"
  elif [ "${base#svitek-}" = "$base" ]; then
    why="its last component '$base' does not start with 'svitek-'"
  elif [ -z "$parent" ]; then why="it sits directly in /"
  else
    case "$PWD/" in "$d"/*) why="it contains the repo ($PWD)";; esac
  fi
  if [ -n "$why" ]; then
    echo "tests/e2e.sh: refusing to wipe SVITEK_TEST_DIR='$SVITEK_TEST_DIR': $why" >&2
    echo "point it at a scratch directory, e.g. \${XDG_RUNTIME_DIR}/svitek-e2e" >&2
    exit 2
  fi
}
assert_scratch_dir

cleanup() {
  local rc=$?
  [ "$rc" != 0 ] && FAILED=1   # an early `exit N` is a failure too
  [ -n "${DAEMON_STARTED:-}" ] && $SVITEK quit >/dev/null 2>&1
  # Kill only the windows *this* nested sway owns, never anything of the
  # user's. Guarded twice over: the ambient SWAYSOCK is unset at the top of the
  # script, and this only ever runs when SWAYSOCK is the socket
  # headless-sway.sh made for us and that socket is really there. So an early
  # exit — a missing tool, a build failure, Ctrl-C — reaches nothing.
  if [ "${SWAYSOCK:-}" = "$NESTED_SOCK" ] && [ -S "$NESTED_SOCK" ]; then
    swaymsg -s "$NESTED_SOCK" -t get_tree 2>/dev/null |
      grep -oE '"pid": [0-9]+' | grep -oE '[0-9]+' |
      while read -r p; do kill "$p" 2>/dev/null; done
  fi
  tests/headless-sway.sh stop >/dev/null 2>&1
  sleep 0.3
  [ "$FAILED" = 0 ] && printf '\n\033[32mall e2e checks passed\033[0m (artifacts in %s)\n' "$SVITEK_TEST_DIR" \
                    || printf '\n\033[31me2e checks FAILED\033[0m (artifacts in %s)\n' "$SVITEK_TEST_DIR"
}
trap cleanup EXIT

# --- pixel probes ----------------------------------------------------------
# Both probes exist twice, once for Pillow and once for ImageMagick, so the
# script runs with either installed. Every rectangle and every threshold is a
# variable shared by the two implementations: they cannot drift apart.
OUT_H=720                                 # the headless output, see headless-sway.sh
THUMB_X=0; THUMB_Y=160; THUMB_W=270; THUMB_H=560

# Green pixels (0-100 %) in the thumbnail column *below the first row*, i.e.
# in the previews of the workspaces we are not on. Nothing else in the panel is
# green, so this is how we see what those rows are showing.
green_pct() { # green_pct <png>
  if [ "$HAVE_PIL" = 1 ]; then
    python3 -c "
from PIL import Image
im=Image.open('$1').convert('RGB').crop(($THUMB_X,$THUMB_Y,$THUMB_X+$THUMB_W,$THUMB_Y+$THUMB_H))
px=list(getattr(im,'get_flattened_data',im.getdata)())
print(sum(1 for r,g,b in px if g>100 and g>r+60 and g>b+60)*100//len(px))"
  else
    magick "$1" -crop "${THUMB_W}x${THUMB_H}+${THUMB_X}+${THUMB_Y}" +repage \
      -fill black -fuzz 25% +opaque '#00be00' -fill white -opaque '#00be00' \
      -colorspace gray -format '%[fx:int(mean*100)]' info:
  fi
}

# "yes" when the focused row/card marker (the border, #89b4fa) shows up in the
# vertical slice x0 <= x < x1 of a screenshot — that colour appears nowhere else
# on screen, so it is how we see both *that* the panel is up and *where*.
# MARKER_PPM is the same threshold for both backends: parts per million of the
# pixels in the slice, so a wide slice and a narrow one are judged alike.
MARKER_PPM=1000    # 0.1 % of the slice
focus_marker_in() { # focus_marker_in <png> <x0> <x1>
  if [ "$HAVE_PIL" = 1 ]; then
    python3 -c "
from PIL import Image
im=Image.open('$1').convert('RGB').crop(($2,0,$3,$OUT_H))
px=list(getattr(im,'get_flattened_data',im.getdata)())
hit=sum(1 for r,g,b in px if b>200 and 100<r<190 and g>150)
print('yes' if hit*1000000 >= len(px)*$MARKER_PPM else 'no')"
  else
    n=$(magick "$1" -crop "$(($3 - $2))x$OUT_H+$2+0" +repage \
          -fill black -fuzz 12% +opaque '#89b4fa' -fill white -opaque '#89b4fa' \
          -colorspace gray -format '%[fx:int(mean*1000000)]' info:)
    [ "$n" -ge "$MARKER_PPM" ] && echo yes || echo no
  fi
}
panel_visible() { # "yes" when the focused-row border is on screen
  grim -o HEADLESS-1 "$SVITEK_TEST_DIR/probe.png"
  focus_marker_in "$SVITEK_TEST_DIR/probe.png" 0 "$PANEL_W"
}

# --- log waits -------------------------------------------------------------
# The daemon shows the panel many times per run, so "does this line exist yet"
# is useless after the first show: it matches a line from an earlier one and
# returns instantly. Everything here is therefore counted.
log_count() { grep -cE "$1" "$LOG" 2>/dev/null || true; }
# Wait until $1 (a grep -E pattern) has matched at least $2 (default 1) times
# in the daemon log, or fail.
wait_log() { # wait_log <pattern> [count] [tries]
  local want=${2:-1}
  for _ in $(seq 1 "${3:-50}"); do
    [ "$(log_count "$1")" -ge "$want" ] && return 0
    sleep 0.1
  done
  fail "timed out waiting for $want x /$1/ in $LOG"; return 1
}
# Run `svitek <args>` and wait for a *new* "panel shown" line, counted from
# before the client ran so the show cannot be missed or matched early.
show_wait() { # show_wait <svitek args…>
  local n; n=$(log_count 'panel shown on HEADLESS-1')
  $SVITEK "$@"
  wait_log 'panel shown on HEADLESS-1' $((n + 1))
}

# The name of the workspace sway has focused right now.
focused_ws() { swaymsg -t get_workspaces | focused_ws_in /dev/stdin; }
# The same, from a saved `get_workspaces` dump: the hold-mode probes run from
# inside the injector (while the modifier is held) and are read back afterwards.
# python parses the JSON properly, so use it whenever there is a python3 at all
# (Pillow is not needed for this one). The awk fallback is for a box with no
# python: it relies on every workspace object printing its "name" before its
# "focused", which is true of sway's output but is not guaranteed by JSON.
focused_ws_in() { # focused_ws_in <json-file>
  if [ "$HAVE_PY3" = 1 ]; then
    python3 -c 'import json,sys
ws=json.load(open(sys.argv[1]))
print(next((w["name"] for w in ws if w.get("focused")), ""))' "$1"
  else
    grep -oE '"name": "[^"]*"|"focused": (true|false)' "$1" |
      awk -F'"' '/"name"/ { n = $4 } /focused/ { if ($0 ~ /true/) { print n; exit } }'
  fi
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
# Stop it again. DAEMON_STARTED means "a test daemon may still be running", and
# is what makes `cleanup` quit one on an early exit; it is set by start_daemon
# and cleared here, nowhere else.
stop_daemon() {
  $SVITEK quit
  DAEMON_STARTED=
  sleep 1
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

step "build"
cargo build --release || exit 2
# The input injector is a separate, test-only crate (see tools/inject/src/main.rs);
# `cargo build` in the root deliberately does not build it.
CARGO_TARGET_DIR=$PWD/target \
  cargo build --release --manifest-path tools/inject/Cargo.toml || exit 2
INJECT=./target/release/inject

step "headless sway"
assert_scratch_dir
rm -rf "$SVITEK_TEST_DIR"; mkdir -p "$SVITEK_TEST_DIR/cfg/svitek"
# Both of these have to be exported *before* sway starts: sway's `exec` inherits
# sway's environment, so this is what makes the `bindsym $mod+Tab exec svitek
# toggle` that headless-sway.sh adds for SVITEK_BIN talk to *our* daemon.
export SVITEK_SOCKET="$SVITEK_TEST_DIR/svitek.sock"
export SVITEK_BIN="$PWD/target/release/svitek"
# `eval "$(…)"` on its own cannot fail: eval of an empty string exits 0, so a
# sway that never came up would go unnoticed. Capture, then eval, then look.
env_out=$(tests/headless-sway.sh start) || exit 2
eval "$env_out"
[ -S "${SWAYSOCK:-}" ] || { echo "headless sway left no control socket" >&2; exit 2; }
[ "$SWAYSOCK" = "$NESTED_SOCK" ] || { echo "unexpected SWAYSOCK $SWAYSOCK" >&2; exit 2; }
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
show_wait toggle; sleep 0.4
check "$(panel_visible)" yes "toggle shows the panel"
$SVITEK toggle; sleep 0.5
check "$(panel_visible)" no  "toggle again hides it"
show_wait show; sleep 0.6
check "$(panel_visible)" yes "show"
$SVITEK hide;   sleep 0.5
check "$(panel_visible)" no  "hide"

step "thumbnail rule: 1 -> 2 -> change 2 -> 1"
show_wait toggle; sleep 0.7
grim -o HEADLESS-1 "$SVITEK_TEST_DIR/before.png"
G0=$(green_pct "$SVITEK_TEST_DIR/before.png")
# A percentage, so a stray anti-aliased pixel or two is not a failure.
if [ "$G0" -le 1 ]; then pass "no green in any thumbnail yet (${G0}%)"
else fail "a thumbnail is already green before the change: ${G0}%"; fi
$SVITEK hide; sleep 0.4
# Change what workspace 2 shows, give the background capture time to file it.
foot_on 2 GREEN "$SVITEK_TEST_DIR/green.sh"
sleep 1
swaymsg workspace 1 >/dev/null; sleep 1.2
show_wait toggle; sleep 0.7
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
show_wait show; sleep 0.8
check "$(panel_visible)" yes "the panel is up before the click"
$INJECT pointer HEADLESS-1 $ROW_X $ROW2_Y click >/dev/null
sleep 1
check "$(panel_visible)" no "a click on workspace 2's row closes the panel"
check "$(focused_ws)" 2 "the click leaves sway on workspace 2"
if grep -q 'click commits workspace "2"' "$LOG"; then pass "the daemon logged the commit"
else fail "no 'click commits workspace \"2\"' in $LOG"; fi

# (b) close_on_select = false: the click switches and the panel stays up. The
# config is only read at startup, so the daemon has to be restarted for it.
stop_daemon
write_config 'close_on_select = false'
start_daemon || exit 1
swaymsg workspace 2 >/dev/null; sleep 0.8
show_wait show; sleep 0.8
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
stop_daemon
: > "$XDG_CONFIG_HOME/svitek/config.toml"        # no keys at all => the defaults
start_daemon || exit 1
swaymsg workspace 1 >/dev/null; sleep 1.2
show_wait toggle; sleep 0.7
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
show_wait toggle; sleep 0.7
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
stop_daemon
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
stop_daemon
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

step "quit"
stop_daemon
# Only *our* daemon: the developer may well have their own svitek running.
kill -0 "$DAEMON_PID" 2>/dev/null && fail "test daemon (pid $DAEMON_PID) is still alive" \
                              || pass "test daemon exited"
[ -e "$SVITEK_SOCKET" ] && fail "control socket $SVITEK_SOCKET was left behind" \
                        || pass "control socket removed"

step "daemon log"
if grep -E '(WARN|ERROR) +svitek' "$SVITEK_TEST_DIR"/svitek-*.log; then
  fail "svitek logged warnings/errors (above)"
else pass "no svitek warnings or errors"; fi

exit "$FAILED"
