//! Shared plain-data types. No GTK, no wayland, no swayipc here — this is the
//! contract between the threads (ipc, capture, control) and the GTK main loop.

use std::sync::Arc;
use std::time::Instant;

/// One toplevel window as sway reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: i64,
    pub title: String,
    /// Wayland `app_id`, or the X11 class for xwayland windows, or None.
    pub app_id: Option<String>,
    pub focused: bool,
}

/// One workspace as sway reports it, with its windows in tree order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: i64,
    /// `num` from get_workspaces; None (or negative in sway) for purely named workspaces.
    pub num: Option<i32>,
    /// The workspace name — the stable key used everywhere (thumbnail cache, switching).
    pub name: String,
    /// Output (connector) name, e.g. "DP-2".
    pub output: String,
    /// True for exactly one workspace overall: the one holding keyboard focus.
    pub focused: bool,
    /// True for the workspace currently shown on its output (one per output).
    pub visible: bool,
    pub windows: Vec<WindowInfo>,
}

/// Everything the panel renders, as of one moment. Produced by `ipc`,
/// consumed by `ui`. Covers all outputs; the UI filters by `focused_output`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Name of the output that holds the focused workspace (sway's `focused` output).
    pub focused_output: String,
    /// All workspaces on all outputs, grouped by output (get_outputs order),
    /// numbered ones ascending, then named ones in sway's own (creation)
    /// order — the same order swaybar shows.
    pub workspaces: Vec<WorkspaceInfo>,
}

impl Snapshot {
    /// Workspaces on `output`, in display order.
    pub fn on_output<'a>(
        &'a self,
        output: &'a str,
    ) -> impl Iterator<Item = &'a WorkspaceInfo> + 'a {
        self.workspaces.iter().filter(move |w| w.output == output)
    }
    /// The workspace currently visible on `output`, if any.
    pub fn visible_on(&self, output: &str) -> Option<&WorkspaceInfo> {
        self.workspaces
            .iter()
            .find(|w| w.output == output && w.visible)
    }
}

/// True when `old` and `new` describe the *same rows*: the same workspaces, in
/// the same order, holding the same windows (same ids, same order, same
/// `app_id`). Everything that is left — the focus/visibility flags
/// (`WorkspaceInfo::focused`, `WorkspaceInfo::visible`, `WindowInfo::focused`)
/// and the window *titles* — the panel can apply to the widgets it already has.
///
/// This is what tells the panel that it may repaint CSS classes and retype the
/// title labels in place instead of rebuilding the rows. It matters because the
/// hover preview *is* a real workspace switch: while the panel is up, every
/// preview makes sway emit a focus change, and rebuilding the rows for it would
/// destroy the very widget the pointer is sitting on — GTK then synthesizes a
/// new enter on the replacement widget, which would preview again, in a loop.
///
/// A title is in the in-place set for the same reason, and it is the one that
/// bites in ordinary use: a terminal running `top` on a visible workspace
/// retitles itself once a second, and treating that as structural would cancel
/// the pending preview and disarm hovering under the user's pointer while they
/// were still choosing. The title only ever changes what one label *says*, and
/// `ui::Panel::apply_flags` says it.
///
/// Anything else is structural and still rebuilds: a workspace appearing,
/// vanishing, being renamed or moving output, a window opening or closing, the
/// windows changing order, or a window changing `app_id` (a row's second label
/// exists only for a window that has one, so that is a different widget tree).
pub fn flags_only_change(old: &[WorkspaceInfo], new: &[WorkspaceInfo]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(a, b)| {
            a.id == b.id
                && a.num == b.num
                && a.name == b.name
                && a.output == b.output
                && a.windows.len() == b.windows.len()
                && a.windows
                    .iter()
                    .zip(&b.windows)
                    .all(|(x, y)| x.id == y.id && x.app_id == y.app_id)
        })
}

/// A downscaled screenshot of one output. Straight (non-premultiplied) RGBA8,
/// row stride = width * 4, top row first.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    /// When the frame was received from the compositor. Not shown yet (v0);
    /// kept so the panel can display a thumbnail's age later.
    #[allow(dead_code)]
    pub taken_at: Instant,
}

/// Why a capture was asked for. Only affects logging/priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureReason {
    /// Periodic / damage-driven background refresh of a visible workspace.
    Background,
    /// The user pressed the toggle: capture the current workspace fresh before showing.
    Toggle,
    /// A workspace was just entered; capture it once it has rendered.
    Entered,
}

/// A command received on the control socket (`svitek toggle` etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Show,
    Hide,
    /// Step the selection one workspace on (wrapping), or show the panel when
    /// it is down. Bind it next to `toggle` for an explicit "previous/next".
    Next,
    /// The same, one workspace back.
    Prev,
    Quit,
}

impl Command {
    pub fn parse(s: &str) -> Option<Command> {
        match s.trim() {
            "toggle" => Some(Command::Toggle),
            "show" => Some(Command::Show),
            "hide" => Some(Command::Hide),
            "next" => Some(Command::Next),
            "prev" => Some(Command::Prev),
            "quit" => Some(Command::Quit),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Toggle => "toggle",
            Command::Show => "show",
            Command::Hide => "hide",
            Command::Next => "next",
            Command::Prev => "prev",
            Command::Quit => "quit",
        }
    }
}

/// Every message the background threads send to the GTK main loop.
/// There is exactly one channel; `main.rs` owns the receiver.
#[derive(Debug, Clone)]
pub enum Msg {
    /// Fresh full state from sway (sent at start and after every relevant event).
    State(Snapshot),
    /// A workspace focus change: `to` is now visible on `output`; `from` is the
    /// workspace that had focus before (on a cross-output switch it lives on
    /// another output and stays visible there).
    /// Sent *in addition to* the `State` that follows it, and before it.
    WorkspaceSwitched {
        output: String,
        from: Option<String>,
        to: String,
    },
    /// A workspace was renamed. Everything keyed by workspace name (the
    /// thumbnail cache) has to move `from` -> `to`; the workspace itself is
    /// the same one, with the same windows on it.
    /// Sent *in addition to* the `State` that follows it, and before it.
    WorkspaceRenamed { from: String, to: String },
    /// A workspace ceased to exist (sway destroys a workspace as soon as it
    /// becomes empty and invisible). Anything cached under `name` describes a
    /// workspace that is gone: a later workspace with the same name is a
    /// different one and must not inherit it.
    /// Sent *in addition to* the `State` that follows it, and before it.
    WorkspaceRemoved { name: String },
    /// A finished screencopy of `output`. The receiver attributes it to whichever
    /// workspace is visible on that output *at the moment this is handled*.
    Frame {
        output: String,
        thumb: Thumbnail,
        reason: CaptureReason,
    },
    /// A capture request failed or the output vanished; nothing to attribute.
    CaptureFailed {
        output: String,
        reason: CaptureReason,
        error: String,
    },
    /// The set of outputs the capturer knows about changed (names, in no particular order).
    CaptureOutputs(Vec<String>),
    /// A control-socket command.
    Control(Command),
    /// The sway IPC connection died; the app should exit (sway is gone).
    IpcLost(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command parses from its own name and prints back as it, and
    /// nothing else parses at all — the control socket and the argv parser are
    /// both this one function.
    #[test]
    fn commands_round_trip_through_their_names() {
        for cmd in [
            Command::Toggle,
            Command::Show,
            Command::Hide,
            Command::Next,
            Command::Prev,
            Command::Quit,
        ] {
            assert_eq!(Command::parse(cmd.as_str()), Some(cmd));
        }
        // The socket hands the line over with its newline still on it.
        assert_eq!(Command::parse("next\n"), Some(Command::Next));
        assert_eq!(Command::parse("  prev  "), Some(Command::Prev));
        assert_eq!(Command::parse("NEXT"), None);
        assert_eq!(Command::parse("previous"), None);
        assert_eq!(Command::parse(""), None);
    }

    fn win(id: i64, title: &str, focused: bool) -> WindowInfo {
        WindowInfo {
            id,
            title: title.to_string(),
            app_id: Some("foot".to_string()),
            focused,
        }
    }

    fn ws(
        id: i64,
        name: &str,
        focused: bool,
        visible: bool,
        windows: Vec<WindowInfo>,
    ) -> WorkspaceInfo {
        WorkspaceInfo {
            id,
            num: name.parse().ok(),
            name: name.to_string(),
            output: "HEADLESS-1".to_string(),
            focused,
            visible,
            windows,
        }
    }

    fn two() -> Vec<WorkspaceInfo> {
        vec![
            ws(1, "1", true, true, vec![win(10, "ALPHA", true)]),
            ws(2, "2", false, false, vec![win(20, "BRAVO", false)]),
        ]
    }

    #[test]
    fn identical_lists_are_a_flags_only_change() {
        assert!(flags_only_change(&two(), &two()));
    }

    #[test]
    fn a_hover_preview_switch_is_a_flags_only_change() {
        // Exactly what sway reports after the preview moved focus to 2: the
        // workspace flags flip, and so does the focused window inside them.
        let before = two();
        let mut after = two();
        after[0].focused = false;
        after[0].visible = false;
        after[0].windows[0].focused = false;
        after[1].focused = true;
        after[1].visible = true;
        after[1].windows[0].focused = true;
        assert_ne!(before, after, "the snapshots really do differ");
        assert!(flags_only_change(&before, &after));
    }

    /// The one that keeps hover and the wheel usable: a terminal running `top`
    /// retitles itself once a second, and a rebuild for that would cancel the
    /// pending preview and disarm hovering under a pointer that never moved.
    /// Same rows, same windows — only what a label *says* changed.
    #[test]
    fn a_retitled_window_is_a_flags_only_change() {
        let before = two();
        let mut after = two();
        after[0].windows[0].title = "top - 14:02:11".to_string();
        assert_ne!(before, after, "the snapshots really do differ");
        assert!(flags_only_change(&before, &after));

        // Including the empty title the panel renders as "(untitled)", in both
        // directions, and on a window that is not the focused one.
        let mut blanked = two();
        blanked[1].windows[0].title = String::new();
        assert!(flags_only_change(&before, &blanked));
        assert!(flags_only_change(&blanked, &before));

        // And together with the focus flags a preview flips, which is what an
        // actual `Msg::State` after a preview of a busy workspace looks like.
        let mut both = two();
        both[0].focused = false;
        both[0].visible = false;
        both[0].windows[0].focused = false;
        both[0].windows[0].title = "top - 14:02:12".to_string();
        both[1].focused = true;
        both[1].visible = true;
        both[1].windows[0].focused = true;
        assert!(flags_only_change(&before, &both));
    }

    #[test]
    fn structural_changes_still_rebuild() {
        let before = two();

        let mut renamed = two();
        renamed[1].name = "web".to_string();
        assert!(!flags_only_change(&before, &renamed));

        let mut window_gone = two();
        window_gone[1].windows.clear();
        assert!(!flags_only_change(&before, &window_gone));

        let mut new_window = two();
        new_window[0].windows.push(win(11, "DELTA", false));
        assert!(!flags_only_change(&before, &new_window));

        let mut different_window = two();
        different_window[0].windows[0].id = 99;
        assert!(!flags_only_change(&before, &different_window));

        let mut app_id_changed = two();
        app_id_changed[0].windows[0].app_id = None;
        assert!(!flags_only_change(&before, &app_id_changed));

        // Same ids, different order: the labels would end up on the wrong rows
        // if this were applied in place.
        let mut reordered = two();
        reordered[0].windows.push(win(11, "DELTA", false));
        let mut swapped = reordered.clone();
        swapped[0].windows.swap(0, 1);
        assert!(!flags_only_change(&reordered, &swapped));

        let mut ws_added = two();
        ws_added.push(ws(3, "3", false, false, vec![]));
        assert!(!flags_only_change(&before, &ws_added));

        let mut ws_gone = two();
        ws_gone.pop();
        assert!(!flags_only_change(&before, &ws_gone));

        let mut moved_output = two();
        moved_output[1].output = "HEADLESS-2".to_string();
        assert!(!flags_only_change(&before, &moved_output));
    }
}
