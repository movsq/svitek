# svitek — design notes (v0)

svitek (Czech: scroll) is a Sway workspace switcher panel. Not a WM, not a
compositor. Rust + gtk4 + gtk4-layer-shell + swayipc + wlr-screencopy.

## Process model

One resident process (`svitek`, started by `exec` in the sway config).
`svitek toggle` (bound to Mod+A) is a tiny client that writes one line to the
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
the size of that output. The panel proper is a fixed-width frame inside it
(thumbnail width + text column + chrome), pinned to the edge `position` names
and full height; the rest is an invisible scrim.

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

## Hover preview (and the wheel, and Enter)

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
  it by hovering, and the wheel moves it by rows — one step per detent, down
  the list for wheel-down, **clamped** at both ends rather than wrapping (a
  spin must never take the user somewhere they were not aiming for). A step
  counts from the pending preview if one is waiting out its debounce, else from
  `previewed`, so hover and wheel can never disagree: whichever acted last is
  what the next step moves from, and four quick steps land four rows away
  instead of one.
* **The wheel controller is on the window, in the CAPTURE phase, and always
  claims the event.** A step on the scrim has to work (the scrim is the window's
  child, so a window-level controller covers the whole surface), and the
  `ScrolledWindow` inside must *not* also scroll the list — the selection moving
  is the scroll. `ui.rs` scrolls the selected row into view itself instead
  (`scroll_into_view`, shared with the focused row at `show()`). The controller
  takes `VERTICAL` without `DISCRETE` and accumulates the raw deltas, so a mouse
  wheel (±1.0 per detent) and a touchpad (fractions) both come out as whole rows
  (`wheel_steps`, `clamp_step` — the two pure functions the unit tests cover).
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
  free; the only way to *end up* somewhere is to commit. A **click on a row**
  switches there and makes it the new origin while the panel stays open (the
  `.focused` marker moves, the selection restarts from it, and closing later
  stays there). **Enter** takes the current selection and closes; it sets `Panel::committed`
  before `hide()` so the `hidden` callback knows not to revert, and both then
  call `callbacks.switch`. (Enter with the selection still on the origin and
  nothing previewed has nothing to switch to, so it is a plain hide.) Setting a
  flag and reading it inside `hide()` is what makes it race-free: `hidden` fires
  from inside `hide()`, on the GTK thread, in one straight line. Enter is
  handled by the same CAPTURE-phase key controller as Esc, for the same reason:
  a focused child must not get to swallow it first.
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
