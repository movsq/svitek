# svitek — design notes (v0)

svitek (Czech: scroll) is a Sway workspace switcher panel. Not a WM, not a
compositor. Rust + gtk4 + gtk4-layer-shell + swayipc + wlr-screencopy.

## Process model

One resident process (`svitek`, started by `exec` in the sway config).
`svitek toggle` (bound to Mod+A) is a tiny client that writes one line
(`toggle` | `show` | `hide` | `next` | `prev` | `quit`) to the
unix socket `$XDG_RUNTIME_DIR/svitek.sock` and exits. Cold start only happens
once, from the sway config.

Threads:

| thread    | owns                              | talks to main via         |
|-----------|-----------------------------------|---------------------------|
| ipc       | swayipc event connection          | `Msg::State`, `Msg::WorkspaceSwitched`, `Msg::WorkspaceRenamed`, `Msg::WorkspaceRemoved`, `Msg::IpcLost` |
| capture   | its own wayland connection, screencopy | `Msg::Frame`, `Msg::CaptureFailed`, `Msg::CaptureOutputs` |
| control   | unix socket listener              | `Msg::Control`            |
| main (GTK)| the panel window, all state       | calls `ipc::switch_to`, `Capturer::request/set_paused` |

All messages go through ONE `async_channel::Sender<Msg>`; `main.rs` drains it
with `glib::spawn_future_local` on the GTK main context. Keep every module
except `ui.rs` and `main.rs` free of GTK.

## Surface geometry

One layer-shell surface, Overlay, exclusive zone 0, keyboard mode Exclusive,
anchored to **all four edges** of the output the panel is shown on — so it is
the size of that output. The panel proper is a frame inside it; the rest is an
invisible scrim.

The frame has two shapes, and `config.position` picks one at startup (it is read
once). Both are the same `ScrolledWindow` with the same `.panel` background, the
same rows, and the same hit test — only the axis changes:

| | `center` (default) | `left` / `right` |
|---|---|---|
| frame | strip of cards, `halign`/`valign` Center | column, full height, at that edge |
| row | thumbnail over ≤ 3 window lines | thumbnail beside ≤ 6 window lines |
| width | its cards, capped at the output | fixed: thumb + text column + chrome |
| scrolls | horizontally (vscrollbar Never) | vertically (hscrollbar Never) |

Why centered by default: a switcher is looked at, not lived in, and the middle
of the screen is where the eyes already are. Cards also keep every thumbnail the
same size however many workspaces there are, where a column has to choose
between a taller panel and smaller pictures.

The cap is the interesting part. The strip asks for its natural width
(`propagate_natural_width`), which is `strip_width()` — cards, gaps, padding —
and that can easily exceed the output: nine 240 px cards want 2500 px on a
1280 px screen. Because `halign` is Center rather than Fill, GTK's
`adjust_for_align` allocates `MIN(natural, available)`, so the strip stops at
the output's width and its horizontal scrollbar takes over; nothing overflows
and nothing is clipped. A card, in turn, is exactly `card_width()` = thumbnail +
padding + border, and the window lines under it cannot widen it: their labels
ellipsize and their `max-width-chars` caps their *natural* width, so the line
asks for less than the card is worth and takes what the card has.

The scrim is not decoration: it is how a click outside the panel closes it. The
click lands on our surface, `pressed` hit-tests it against the frame's bounds
(a full-output surface means the compositor cannot answer "outside" for us), and
it goes no further — dismissing the panel must not also click the window under
the pointer. A press inside the frame denies the sequence, so the row's own
gesture still sees it and switches workspaces as before.

Limitation: the scrim covers one output, so a click on a *different* output does
not reach it and does not close the panel. A second surface per output would fix
that at the price of a surface per monitor and a second keyboard grab; Esc and
the toggle binding already work from anywhere, so v0 leaves it.

## Thumbnail rule (the compromise)

Sway renders only the visible workspace of each output, so a live picture of a
hidden workspace is impossible without making it visible (which is what the
hover preview below does, at the price of a real switch). For the little
pictures in the list, what we do instead:

1. While the panel is hidden, the capture thread keeps a throttled
   `copy_with_damage` request pending per output (min ~400 ms between frames;
   no repaints => no work). Each finished frame is attributed, in the main
   loop, to the workspace visible on that output at that moment. So when the
   user leaves a workspace, its most recent frame (≤ ~400 ms old + one repaint)
   is already cached under the workspace's name.
2. On toggle, the current workspace is captured fresh first; the panel is
   shown when that frame lands (or after ~60 ms, whichever comes first).
3. After a workspace switch, the newly entered workspace is captured once it
   has rendered (~150 ms later), so it too has a fresh frame.
4. Captures are paused while the panel is visible so the panel itself never
   appears in a thumbnail.
5. Workspaces never visited since start show a placeholder with titles only.
6. Cache is in memory only, keyed by workspace name. The name is not stable,
   so `ipc` corrects the key before the state that uses it: a `rename` moves the
   entry with the workspace, and the `empty` that precedes sway destroying a
   workspace drops it, so a later workspace of the same name starts blank
   instead of inheriting a picture of windows that no longer exist.

Ordering caveat: sway emits the workspace event before it renders the new
workspace, and the screencopy completes after that render, so frames are
attributed correctly in practice; a frame that lands in the same tick as the
switch could theoretically be attributed to the wrong side. Accepted for v0.

## Hover preview (and the wheel, Enter, and hold mode)

The thumbnail rule above says a live picture of a hidden workspace does not
exist. The way out is not to ask for one: rest the pointer on a row for 120 ms
and svitek **really switches** to that workspace, leaving the panel mapped on
top of it. The panel is an overlay on the same output, so the workspace it is
covering is the one that just became visible.

* **Origin.** The workspace focused when the panel was shown. `main.rs` keeps it
  in `App::preview_origin` for exactly as long as the panel is up, plus
  `previewing` — true once a preview has taken sway somewhere else.
* **Hovering row X** asks `main.rs` for a preview. X == origin with nothing
  previewed is a no-op; anything else is `ipc::switch_to(X)`. Hovering the
  origin again is a real switch back, and clears `previewing`.
* **One selection, two ways to move it.** `Panel::previewed` is not just
  bookkeeping for the hover debounce, it *is* the selection: the row the panel
  is currently showing. It starts on the origin at `show()`, the pointer moves
  it by hovering, and the wheel moves it by rows — one step per detent, **down
  the list** for wheel-down in a column and **one card to the right** in the
  centered strip (the same `clamp_step` either way: the cards run left to right
  in the order the rows run top to bottom), **clamped** at both ends rather than
  wrapping (a spin must never take the user somewhere they were not aiming for).
  A step counts from the pending preview if one is waiting out its debounce,
  else from `previewed`, so hover and wheel can never disagree: whichever acted
  last is what the next step moves from, and four quick steps land four rows
  away instead of one. Hold-mode Mod+Tab and `svitek next|prev` move the same
  selection through the same `step_selection`, with one difference: they
  **wrap** (`wrap_step`, next to `clamp_step` and unit-tested beside it). A key
  you tap repeatedly is a cycle — stopping dead at the last workspace would put
  half the list out of reach — where a wheel spin is not a count, and clamping
  is what keeps it from overshooting. They also ignore `PREVIEW_GRACE`: it is
  there for pointer and wheel events that were in flight when the surface
  mapped, and a second Mod+Tab 80 ms after the first is not one of those.
* **The wheel controller is on the window, in the CAPTURE phase, and always
  claims the event.** A step on the scrim has to work (the scrim is the window's
  child, so a window-level controller covers the whole surface), and the
  `ScrolledWindow` inside must *not* also scroll the list — the selection moving
  is the scroll. `ui.rs` scrolls the selected row into view itself instead
  (`scroll_into_view`, shared with the focused row at `show()`; it moves the
  horizontal adjustment in the centered layout and the vertical one in the
  columns, over the same arithmetic in `scroll_target`). The controller takes
  `VERTICAL` without `DISCRETE` and accumulates the raw deltas, so a mouse
  wheel (±1.0 per detent) and a touchpad (fractions) both come out as whole rows
  (`wheel_steps`, `clamp_step` — the pure functions the unit tests cover).
  `VERTICAL` in the centered layout too, where the selection moves sideways:
  most wheels only *have* a vertical axis, and "down is the next workspace" is
  then one gesture in both layouts. Adding `HORIZONTAL` would mean summing two
  axes and double-counting a diagonal touchpad swipe, for a gesture no mouse can
  make.
* **One debounce for both.** Hover and wheel share `PREVIEW_DEBOUNCE` (120 ms)
  and one `Panel::pending`, so spinning the wheel through five rows is one
  workspace switch, fired 120 ms after the *last* step, not five on the way.
  The pending preview records which of the two started it: the pointer leaving
  a row cancels a hover debounce, but must not cancel the wheel's, because
  scrolling the selection into view moves the list under a stationary pointer
  and the crossing events that produces are not the user changing their mind.
  For the same reason a wheel step disarms hovering until the pointer really
  moves again.
* **Closing reverts.** Esc, a click outside, `svitek hide|toggle` — the panel
  hides and, if `previewing`, switches back to the origin. Looking around is
  free; the only way to *end up* somewhere is to commit. **Enter** commits the
  current selection, and so — by default — does a **click on a row**, of that
  row: picking a workspace is what the panel is for, so the gesture that picks
  one also closes it. Both go through `Panel::commit`, which sets
  `Panel::committed`, calls `hide()`, and only then calls `callbacks.switch`.
  Setting a flag and reading it inside `hide()` is what makes that race-free:
  `hidden` fires from inside `hide()`, on the GTK thread, in one straight line —
  and the order matters the other way round too, because `main.rs` keys "a click
  adopted a new origin" off `shown_on`, which `hidden` has already cleared by
  the time `switch` runs. Committing where you already are is a plain hide
  (`commit_is_a_plain_hide`: the target is the origin, nothing is previewed
  away from it, and no debounce is in flight — a unit-tested pure function, so
  Enter and the click cannot drift apart). Enter is handled by the same
  CAPTURE-phase key controller as Esc, for the same reason: a focused child must
  not get to swallow it first.
* **`mode = "hold"` is the alt-tab reading of the whole panel.** The same
  widgets, three differences, all of them about who ends the gesture:
  1. Sway resolves `bindsym $mod+Tab` itself and swallows the combination, so
     **the panel never sees the Tab** — what it sees is another `svitek toggle`
     on the control socket. `main.rs` therefore reads `Command::Toggle` while
     the panel is up (or a show is pending) as `Panel::step(1)` rather than a
     hide. That is why hold mode is a *config* key and not a key binding: the
     daemon cannot tell the two presses apart any other way.
  2. **Releasing the modifier commits** — Super/Alt/Ctrl/Meta/Hyper, left or
     right, and deliberately not Shift, which is held for the Mod+Shift+Tab that
     goes backwards. The panel holds exclusive keyboard focus, so that release
     is delivered to it; the handler is a `key-released` on the same
     CAPTURE-phase controller as Esc and Enter, and it calls the same
     `commit_selection` Enter does. Because that prefers the *pending* target
     over the previewed one, a release inside the 120 ms debounce still commits
     where the user was going.
  3. **The tap that is over too soon.** On a fast Mod+Tab the modifier can be up
     before the layer surface has keyboard focus at all, so no release event
     will ever arrive and the panel would hang there. The first moment the
     question can be answered is `wl_keyboard.enter`, which carries the modifier
     state and shows up here as the window going active: `is_active_notify`
     asks the seat keyboard for `modifier_state()` once per showing, and a panel
     opened by a modifier that is already up commits at once. Measured on
     sway 1.12 + GTK 4.22 (headless, virtual keyboard): `SUPER_MASK` with Super
     held, empty without, both within one debug line of the panel mapping.
  4. **The opening press is the first step** (`hold_selects_next`, default
     true): `Ctx::step` from hidden begins the show and files the step on the
     `PendingShow`, so the panel maps with the next workspace already selected
     and its preview debounce running. That is what makes a quick tap a switch,
     as alt-tab is — the release (or the already-up check in 3.) commits the
     *pending* target. With it off the first press only opens the panel.
  Everything else — hover, the wheel, Enter, clicks, Esc, a click outside,
  `svitek hide` — means exactly what it means in `toggle` mode.
* **`close_on_select = false` is the other reading of a click.** Some people use
  the panel as a place to walk through workspaces rather than a menu to pick one
  from, so the old behaviour is a config key, not a deleted branch: the click
  switches, `adopt_origin` makes that row the new origin (the `.focused` marker
  moves, the selection restarts from it, closing later stays there), and the
  panel stays up. Enter closes either way — there has to be one gesture that
  always means "this one, done".
* **Leaving a row is not closing.** Moving onto the padding or the scrim only
  cancels a pending debounce. A preview stands until the panel closes, so the
  pointer can go anywhere — including out of the panel to look at the workspace
  behind it — without snapping back.
* **Rows are frozen while the panel is visible.** Every preview makes sway emit
  a focus change, which comes back as `Msg::WorkspaceSwitched` + `Msg::State`.
  Rebuilding the rows for it would destroy the widget under the pointer, and GTK
  hands the replacement an enter event even though nothing moved — a switch
  loop. So `Panel::update` only repaints CSS classes when the difference is
  focus/visibility flags (`model::flags_only_change`); a structural change
  (workspace added, removed, renamed, windows changed) still rebuilds. The
  `.focused` marker therefore stays on the *origin* row the whole time — it
  means "where Esc puts you back" — and the previewed row gets `.previewing`
  (the focused colour at 60 %).
* **Nothing previews itself.** Hovering is dead while the panel is hidden, for
  150 ms after `show()`, and until the pointer has actually *moved* over the
  panel (armed on the first `motion`, never on an `enter`): the pointer is very
  often already over a row when the surface maps, and a panel that switched
  workspaces just by opening would be a bug. A rebuild disarms it again, for the
  same reason. The wheel is dead over the same two windows (hidden, and the
  first 150 ms) — it needs no arming beyond that, because a wheel event is
  always something the user did.
* Post-switch `Entered` captures are still only scheduled while the panel is
  hidden, so previewed workspaces do not refresh their thumbnails until the
  panel closes — after which the origin is captured as before.

## Non-goals (v0)

No drag/drop, no reordering, no layout control, no other compositors, no
animations. Write ideas in TODO.md instead of building them.
