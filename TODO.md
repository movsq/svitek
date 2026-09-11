# TODO

Ideas explicitly out of scope for v0 (do not build without deciding first).
Each of these has been thought about and *deliberately* left out; if one of them
starts to look necessary, the decision to take it on is a separate one from the
decision to write the code.

* **Dragging windows between workspaces.** Drop targets on the rows, moving a
  window with `move container to workspace`. Pulls in a whole drag model, hit
  testing on thumbnails, and a per-window picture we do not have.
* **Reordering workspaces.** Drag a row to renumber. Sway has no "move
  workspace to position"; it would mean renaming workspaces underneath the user.
* **Layout control.** Splitting, tabbing, changing the layout of a workspace
  from the panel. That is a window manager's job, and svitek is not one.
* **niri / Hyprland backends.** Both the IPC and the screencopy paths are
  sway/wlroots specific. Would need a compositor abstraction that v0 has no
  second implementation to validate.
* **Animations.** Slide-in, fade, thumbnail cross-fade. They cost the toggle
  latency that is the point of the resident process.
* **Persisting the thumbnail cache to disk.** Would survive restarts, at the
  price of writing screenshots of the user's screen into their home directory.
  Needs a privacy decision first, not just code.
* **Keyboard navigation in the panel (j/k and friends).** Half of this is
  built: the scroll wheel moves a selection, previews it, and `Enter` commits
  it, so the questions this entry used to ask are answered — the selection is
  `Panel::previewed` (no separate focus model), it previews exactly the way
  hovering does, and hover and wheel share it so whichever acted last wins; a
  wheel spin clamps at the ends, an explicit step wraps. The other question it
  used to ask — what the toggle key should do while a row is selected — now has
  two answers, and they are a config key: `mode = "toggle"` hides and reverts,
  like Esc, and `mode = "hold"` steps the selection on and commits when the
  modifier goes up (`svitek next`/`prev` are the same step, bindable on their
  own). What is left is genuinely only the keys *inside* the panel: j/k,
  arrows, Home/End, and a workspace-number shortcut. They would reuse the
  `step_selection`/`arm_preview` path the wheel and Tab already use, so the
  remaining decisions are small ones about which bindings to spend.
* **Previewing a workspace that lives on another output.** The hover preview
  works because the panel is an overlay on the same output as the workspace it
  switches to. A row for a workspace on a *different* monitor would preview
  somewhere the user may not even be looking at, and `switch_to` moves focus to
  that output as a side effect. The panel only lists its own output's
  workspaces today, so the question does not arise; it would the moment that
  changed.
* **Making previews invisible to workspace history.** A preview is a real
  `workspace` command, so sway's `back_and_forth` and every history tool sees
  it. Hiding that would mean replaying the history sway kept before the panel
  opened, i.e. svitek writing to state it does not own. Filed here rather than
  attempted; see the note in README.md.
