# svitek

svitek (Czech: scroll) is a workspace switcher panel for [sway](https://swaywm.org):
one resident process that pops a panel over the left edge of the focused output,
listing that output's workspaces with a thumbnail and the window titles of each.

## What it does

* `Mod+A` toggles the panel. It appears on the left edge of the output that has
  focus, and lists the workspaces of that output only.
* Every row shows the workspace name/number, a thumbnail of that workspace, and
  the titles of the windows on it. The focused workspace is marked.
* **Rest the pointer on a row and that workspace appears live behind the
  panel.** After about a tenth of a second svitek switches to it and leaves the
  panel up on top, so you are looking at the real workspace, not a thumbnail —
  moving, up to date, with everything on it. Move to another row and it follows;
  move back to the row you started on and you are back where you were.
* **The scroll wheel does the same thing without the pointer.** A wheel step
  anywhere on the screen while the panel is up — a row, the panel itself, or
  the empty space beside it — moves the selection one workspace down (wheel
  down) or up, and previews it exactly like hovering does. It stops at the
  first and last workspace instead of wrapping, and it follows the pointer:
  scroll after hovering a row and the next step counts from *that* row. Spin it
  through five workspaces and svitek switches once, when you stop, not five
  times on the way; if the list is longer than the screen it scrolls to keep
  the selection in view.
* Click a row to make that workspace the active one. The panel stays open,
  the focus marker moves to that row, and previews now start from there.
  Press `Enter` to take the workspace you have selected with the wheel (or are
  hovering) and close the panel in one go. `Esc`, `Mod+A` again, or a click
  anywhere outside the panel hides it and **puts you back on the active
  workspace** — the one you opened it from, or the last one you clicked —
  whatever you previewed in between. So looking around costs nothing, and the
  only way to end up somewhere new is to click a row or press `Enter`. That
  closing click is swallowed: svitek's surface covers the whole
  output, so dismissing the panel never also clicks the window behind it. The
  catch on a multi-output setup is that the surface covers only the output the
  panel is on — a click on another screen does not close the panel (it does move
  sway's focus there); `Esc` and `Mod+A` work from anywhere.
* A preview is a **real workspace switch** — sway renders only the workspace it
  is showing, so there is no other way to see one live. That means sway fires
  its usual `workspace` events for every preview, and anything watching them
  sees the visits: sway's own `workspace back_and_forth`, and any
  workspace-history tool, will remember the workspaces you hovered past, not
  just the one you picked. svitek cannot hide that — a preview you could not
  observe would not be a preview. (Its own commands do pass
  `--no-auto-back-and-forth`, so a switch to the workspace you are already on is
  a no-op instead of a jump back somewhere else.) If your workspace history ever
  looks busier than your day was, this is why.
* Toggling is a socket round trip to the already-running process — no cold
  start, no new window. Measured against the headless test sway on software
  rendering (pixman): `svitek toggle` returns in ~15 ms, nearly all of it
  spawning the client process; the daemon has the panel up a couple of
  milliseconds after the command reaches it (it first takes one fresh
  screencopy of the current workspace, ~1.5 ms); and the pixels are on screen
  ~35 ms after the toggle, ~65 ms on the very first, cold one — well under
  100 ms either way.

## The thumbnail compromise — read this before filing a bug

Sway renders only the *visible* workspace of each output. There is no way to ask
it for a live picture of a workspace you are not looking at; that image simply
does not exist anywhere. Hovering a row is the way around that — it makes the
workspace visible for real — but the little pictures in the list cannot be:

* While the panel is hidden, svitek keeps a throttled screencopy request per
  output (at most one frame every ~400 ms, and none at all when nothing
  repaints). Each frame is filed under whichever workspace was visible on that
  output at that moment.
* So the preview of a workspace you are *not* on is **the last frame captured
  while you were still on it** — typically a fraction of a second before you
  left. It does not update afterwards. A video playing on another workspace
  shows as a frozen frame; a window that appeared there since you left is not
  in the picture.
* When you leave a workspace its cached frame is already fresh; when you enter
  one it is re-captured ~150 ms later, once sway has drawn it; and on toggle the
  current workspace is captured fresh before the panel appears.
* Captures are paused while the panel is up, so the panel never appears inside
  a thumbnail. A workspace you preview by hovering therefore keeps its old
  thumbnail until the panel closes; you are looking at the live thing behind the
  panel anyway.
* A workspace you have not visited since svitek started has no frame at all and
  shows a "no preview yet" placeholder with the window titles.
* The cache lives in memory only. Restart svitek and every thumbnail is gone
  until you visit the workspaces again.

This is a deliberate compromise, not a bug.

## Build & install

```sh
cargo build --release
cp target/release/svitek ~/.local/bin/
```

Runtime dependencies: gtk4 ≥ 4.18, gtk4-layer-shell ≥ 1.3, and sway (or another
wlroots compositor) exposing `wlr-screencopy-unstable-v1`. Without screencopy
svitek refuses to start — thumbnails are the whole point.

## Sway config

```
exec svitek
bindsym $mod+a exec svitek toggle
```

Note that `$mod+a` is `focus parent` in sway's default config. The last
`bindsym` for a key wins, so put the svitek binding *after* the defaults (after
any `include`), move `focus parent` to another key if you use it, or bind svitek
to something else entirely.

## Commands

| command | what it does |
|---|---|
| `svitek` | run the resident panel (this is what `exec` starts) |
| `svitek toggle` | show the panel, or hide it if it is up |
| `svitek show` / `svitek hide` | one direction only |
| `svitek quit` | stop the resident process |
| `svitek --help` / `svitek --version` | |

Everything but the bare `svitek` is a one-line message to the running process;
if none is running the client says so and exits 1.

## Configuration

`~/.config/svitek/config.toml` (strictly `$XDG_CONFIG_HOME/svitek/config.toml`).
The file is optional; so is every key in it. An unknown key or a syntax error is
reported on stderr at start and the defaults are used.

```toml
# Width of the thumbnails in pixels; height follows the output's aspect ratio.
thumbnail_width = 240
# Which screen edge the panel is anchored to: "left" (default) or "right".
position = "left"

[colors]
background = "#1e1e2ecc"   # panel background (RGBA hex allowed)
foreground = "#cdd6f4"     # window titles
dim        = "#a6adc8"     # app_id, workspace labels
focused    = "#89b4fa"     # border/marker of the focused workspace row
                           # (the row being previewed gets the same colour at 60 %)
```

## Environment

* `RUST_LOG` — log level of the daemon, e.g. `RUST_LOG=svitek=debug svitek`.
* `SVITEK_SOCKET` — override the control socket path (default
  `$XDG_RUNTIME_DIR/svitek.sock`, or `/tmp/svitek-<uid>.sock` if
  `XDG_RUNTIME_DIR` is unset). Both the daemon and the client read it, so it is
  the way to run a second svitek against a second (e.g. nested or headless)
  sway without the two fighting over one socket.

## Testing

Never test svitek against the sway session you are working in — a panel that
grabs the keyboard while it is half-implemented is unpleasant to escape.
`tests/headless-sway.sh` starts a nested headless sway with two outputs for
exactly this:

```sh
eval "$(tests/headless-sway.sh start)"   # exports SWAYSOCK, WAYLAND_DISPLAY, SVITEK_TEST_DIR
export SVITEK_SOCKET=$XDG_RUNTIME_DIR/svitek-test.sock
cargo run                                # against the headless sway only
tests/headless-sway.sh stop
```

Use `SVITEK_TEST_DIR` to keep parallel headless instances apart. The unit tests
(`cargo test`) need no compositor at all.

`tests/e2e.sh` does the whole thing unattended — headless sway, build, daemon,
toggle/show/hide/quit and the 1 → 2 → back-to-1 thumbnail check (a pixel test on
the preview) — and exits non-zero if anything fails. It needs `foot`, `grim` and
either python3 + PIL or ImageMagick. `tools/inject/` is a separate test-only
crate that fakes pointer motion, clicks, wheel steps and key presses on the
headless seat, which has no input devices; see the comment at the top of
`tools/inject/src/main.rs`.

## Non-goals (v0)

No drag and drop of windows between workspaces, no reordering of workspaces, no
layout control, no compositors other than sway, no animations. Ideas that keep
coming back live in `TODO.md`; the design rationale lives in `DESIGN.md`.
