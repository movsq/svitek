//! Sway IPC: initial state, event subscription, and workspace switching.
//!
//! Runs on its own thread (swayipc is blocking). Sends `Msg::State` at start
//! and after every workspace / window / output event, plus — always before the
//! `State` that follows the same event — `Msg::WorkspaceSwitched` for workspace
//! `focus`, `Msg::WorkspaceRenamed` for `rename` and `Msg::WorkspaceRemoved`
//! for `empty` (sway destroys a workspace the moment it becomes empty).

use crate::model::{Msg, Snapshot, WindowInfo, WorkspaceInfo};
use async_channel::Sender;
use std::cell::RefCell;
use std::collections::HashMap;
use swayipc::{
    Connection, Event, EventType, Node, NodeType, Output, Workspace, WorkspaceChange,
    WorkspaceEvent,
};

/// The i3-compatible scratchpad workspace, which sway exposes on the fake
/// `__i3` output. Never shown to the user.
const SCRATCH_WORKSPACE: &str = "__i3_scratch";

/// Spawn the IPC thread. Returns once the thread is started (not once the
/// first snapshot arrives). The thread ends (sending `Msg::IpcLost`) when the
/// connection drops.
pub fn spawn(tx: Sender<Msg>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("svitek-ipc".to_owned())
        .spawn(move || run(tx))
        .expect("failed to spawn the ipc thread")
}

fn run(tx: Sender<Msg>) {
    // The connection we subscribe on becomes a one-way event stream, so state
    // is always re-read through a second, private connection.
    let sub = match Connection::new() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send_blocking(Msg::IpcLost(format!("cannot connect to sway: {e}")));
            return;
        }
    };

    // Workspace node id -> name, as of the last snapshot we sent. Sway leaves
    // `old` null on a rename event, so this is the only way to learn the name
    // the workspace had a moment ago (the node id survives a rename).
    let mut names: HashMap<i64, String>;

    match snapshot() {
        Ok(s) => {
            log::debug!(
                "initial snapshot: {} workspaces on {:?}",
                s.workspaces.len(),
                s.focused_output
            );
            names = workspace_names(&s);
            if tx.send_blocking(Msg::State(s)).is_err() {
                return; // main loop gone; nothing to report to.
            }
        }
        Err(e) => {
            let _ = tx.send_blocking(Msg::IpcLost(format!("initial snapshot failed: {e}")));
            return;
        }
    }

    let events = match sub.subscribe([EventType::Workspace, EventType::Window, EventType::Output]) {
        Ok(e) => e,
        Err(e) => {
            let _ = tx.send_blocking(Msg::IpcLost(format!("subscribe failed: {e}")));
            return;
        }
    };

    for event in events {
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                log::warn!("sway event stream error: {e}");
                let _ = tx.send_blocking(Msg::IpcLost(format!("event stream error: {e}")));
                return;
            }
        };

        // Workspace focus / rename / destruction are announced before the
        // state that follows them, so the UI can react to the transition
        // itself — and so the thumbnail cache, which is keyed by name, is
        // corrected before the new names show up in a `State`.
        if let Event::Workspace(ev) = &event {
            if ev.change == WorkspaceChange::Focus {
                if let Some(current) = &ev.current {
                    let to = current.name.clone().unwrap_or_default();
                    let output = current
                        .output
                        .clone()
                        .or_else(|| output_of_workspace(current.id))
                        .unwrap_or_default();
                    let from = ev.old.as_ref().and_then(|o| o.name.clone());
                    log::debug!("workspace focus on {output}: {from:?} -> {to}");
                    if tx
                        .send_blocking(Msg::WorkspaceSwitched { output, from, to })
                        .is_err()
                    {
                        return;
                    }
                }
            }
            if let Some(msg) = workspace_msg(ev, &names) {
                log::debug!("workspace event: {msg:?}");
                if tx.send_blocking(msg).is_err() {
                    return;
                }
            }
        }

        match snapshot() {
            Ok(s) => {
                names = workspace_names(&s);
                if tx.send_blocking(Msg::State(s)).is_err() {
                    return;
                }
            }
            Err(e) => {
                // A single failed re-read is usually sway shutting down; the
                // event stream will confirm it on the next iteration.
                log::warn!("snapshot after event failed: {e}");
            }
        }
    }

    log::debug!("sway event stream ended");
    let _ = tx.send_blocking(Msg::IpcLost("sway closed the event stream".to_owned()));
}

/// The workspace node id -> name map a `Rename` event is resolved against.
fn workspace_names(s: &Snapshot) -> HashMap<i64, String> {
    s.workspaces
        .iter()
        .map(|w| (w.id, w.name.clone()))
        .collect()
}

/// The extra message a workspace event carries for the name-keyed caches, if
/// any. Pure, so it can be unit-tested against recorded events.
///
/// * `Rename` — sway 1.12 sends `current` holding the node under its *new*
///   name and leaves `old` null (measured; i3 documents `old` as focus-only),
///   so the previous name is recovered from `names` via the node id, which a
///   rename does not change. A rename we cannot name both sides of is ignored:
///   the `State` that follows still carries the new name, only the cached
///   thumbnail is lost.
/// * `Empty` — the workspace is being destroyed. `current` is the workspace
///   that just became empty.
fn workspace_msg(ev: &WorkspaceEvent, names: &HashMap<i64, String>) -> Option<Msg> {
    let current = ev.current.as_ref()?;
    match ev.change {
        WorkspaceChange::Rename => {
            let to = current.name.clone().filter(|n| !n.is_empty())?;
            let from = ev
                .old
                .as_ref()
                .and_then(|o| o.name.clone())
                .or_else(|| names.get(&current.id).cloned())
                .filter(|n| !n.is_empty())?;
            if from == to {
                return None;
            }
            Some(Msg::WorkspaceRenamed { from, to })
        }
        WorkspaceChange::Empty => {
            let name = current.name.clone().filter(|n| !n.is_empty())?;
            Some(Msg::WorkspaceRemoved { name })
        }
        _ => None,
    }
}

/// Look up the output of a workspace by node id, for the rare case where a
/// workspace event node carries no `output` field.
fn output_of_workspace(id: i64) -> Option<String> {
    let mut conn = Connection::new().ok()?;
    let ws = conn.get_workspaces().ok()?;
    ws.into_iter().find(|w| w.id == id).map(|w| w.output)
}

/// Build a `Snapshot` from a fresh `get_workspaces` + `get_tree`.
/// Uses a private short-lived connection; safe to call from any thread.
pub fn snapshot() -> Result<Snapshot, String> {
    let mut conn = Connection::new().map_err(|e| e.to_string())?;
    let workspaces = conn.get_workspaces().map_err(|e| e.to_string())?;
    let tree = conn.get_tree().map_err(|e| e.to_string())?;
    let outputs = conn.get_outputs().map_err(|e| e.to_string())?;
    Ok(build_snapshot(&workspaces, &tree, &outputs))
}

/// Pure conversion, so it can be unit-tested against recorded fixtures.
fn build_snapshot(workspaces: &[Workspace], tree: &Node, outputs: &[Output]) -> Snapshot {
    let mut windows = windows_by_workspace(tree);

    let mut infos: Vec<WorkspaceInfo> = workspaces
        .iter()
        .filter(|w| w.name != SCRATCH_WORKSPACE)
        .map(|w| WorkspaceInfo {
            id: w.id,
            // sway reports -1 for workspaces whose name does not start with a number.
            num: if w.num < 0 { None } else { Some(w.num) },
            name: w.name.clone(),
            output: w.output.clone(),
            focused: w.focused,
            visible: w.visible,
            windows: windows.remove(&w.id).unwrap_or_default(),
        })
        .collect();

    // Outputs in get_outputs order; anything on an output sway did not list
    // (disconnected mid-read) keeps its relative position at the end.
    let output_rank = |name: &str| {
        outputs
            .iter()
            .position(|o| o.name == name)
            .unwrap_or(usize::MAX)
    };
    infos.sort_by(|a, b| {
        output_rank(&a.output)
            .cmp(&output_rank(&b.output))
            .then_with(|| a.output.cmp(&b.output))
            // Numbered workspaces ascending, then named ones in the order sway
            // itself lists them (creation order — what swaybar shows). The
            // sort is stable, so `get_workspaces` order is kept for ties.
            .then_with(|| match (a.num, b.num) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });

    let focused_output = outputs
        .iter()
        .find(|o| o.focused)
        .map(|o| o.name.clone())
        .or_else(|| infos.iter().find(|w| w.focused).map(|w| w.output.clone()))
        .or_else(|| outputs.first().map(|o| o.name.clone()))
        .unwrap_or_default();

    Snapshot {
        focused_output,
        workspaces: infos,
    }
}

/// Walk the tree once and collect the views of every real workspace, keyed by
/// the workspace node id (which equals the `get_workspaces` id).
fn windows_by_workspace(tree: &Node) -> std::collections::HashMap<i64, Vec<WindowInfo>> {
    let mut map = std::collections::HashMap::new();
    collect_workspaces(tree, &mut map);
    map
}

fn collect_workspaces(node: &Node, map: &mut std::collections::HashMap<i64, Vec<WindowInfo>>) {
    if node.node_type == NodeType::Workspace {
        if node.name.as_deref() == Some(SCRATCH_WORKSPACE) {
            return; // never descend into the scratchpad
        }
        let mut views = Vec::new();
        collect_views(node, &mut views);
        map.insert(node.id, views);
        return;
    }
    for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
        collect_workspaces(child, map);
    }
}

/// Depth-first, tiling children before floating ones — sway's own tree order.
/// Called with a workspace or container node; the node itself is never added.
fn collect_views(node: &Node, out: &mut Vec<WindowInfo>) {
    for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
        if is_view(child) {
            out.push(WindowInfo {
                id: child.id,
                title: child.name.clone().unwrap_or_default(),
                app_id: app_id_of(child),
                focused: child.focused,
            });
        } else {
            collect_views(child, out);
        }
    }
}

/// A leaf view: a `con`/`floating_con` with no children of its own. Split
/// containers have children and no identity of their own; sway gives real
/// views a `pid`, an `app_id` or (for xwayland) `window_properties`.
fn is_view(node: &Node) -> bool {
    if !matches!(node.node_type, NodeType::Con | NodeType::FloatingCon) {
        return false;
    }
    if !node.nodes.is_empty() || !node.floating_nodes.is_empty() {
        return false;
    }
    node.pid.is_some()
        || node.app_id.is_some()
        || node.window_properties.is_some()
        || node.name.is_some()
}

/// `app_id` for wayland views, the X11 class for xwayland ones.
fn app_id_of(node: &Node) -> Option<String> {
    node.app_id.clone().or_else(|| {
        node.window_properties
            .as_ref()
            .and_then(|p| p.class.clone())
    })
}

thread_local! {
    /// The command connection of whichever thread calls `switch_to` — in
    /// practice always the GTK main thread. Kept open across clicks: connecting
    /// costs a socket, a handshake and an allocation on every switch, and a
    /// click is exactly when we do not want to pay for that.
    static COMMAND_CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Switch to workspace `name` exactly like `swaymsg workspace <name>` would:
/// numbered workspaces via `workspace number N`, others via a quoted name.
/// Moves focus to another output if the workspace lives there. Blocking but
/// fast (one round trip); called from the GTK main loop on a row click.
pub fn switch_to(name: &str, num: Option<i32>) -> Result<(), String> {
    // The name is the stable key everywhere else, and `workspace <name>` also
    // finds a numbered workspace, so `num` is not needed here.
    let _ = num;
    let cmd = format!(
        "workspace --no-auto-back-and-forth \"{}\"",
        escape_for_sway(name)
    );
    run_sway_command(&cmd)
}

/// Move the cursor one pixel right and back. Sway only recomputes which
/// surface is under the pointer when the pointer moves, so a panel that maps
/// under a stationary cursor would not receive the wheel or the first click
/// until the user moved the mouse. The net movement is zero.
pub fn nudge_pointer() -> Result<(), String> {
    run_sway_command("seat - cursor move 1 0; seat - cursor move -1 0")
}

/// Run one sway command on the cached command connection (GTK thread only),
/// reconnecting once if sway closed it since the last call.
fn run_sway_command(cmd: &str) -> Result<(), String> {
    log::debug!("running sway command: {cmd}");
    COMMAND_CONN.with(|slot| {
        let mut slot = slot.borrow_mut();
        // A cached connection can have been closed by sway (restart, timeout)
        // since the last click; that shows up as a transport error, and the
        // one retry below is on a brand new connection.
        if let Some(conn) = slot.as_mut() {
            match run_command_on(conn, cmd) {
                Ok(outcome) => return outcome,
                Err(e) => {
                    log::debug!("sway command connection is stale ({e}); reconnecting");
                    *slot = None;
                }
            }
        }
        log::debug!("opening a sway command connection");
        let mut conn = Connection::new().map_err(|e| e.to_string())?;
        let outcome = run_command_on(&mut conn, cmd)?;
        *slot = Some(conn);
        outcome
    })
}

/// Run one command. `Err` means the *connection* failed and is to be thrown
/// away; `Ok(Err(..))` means sway answered and refused the command, which says
/// nothing about the connection.
fn run_command_on(conn: &mut Connection, cmd: &str) -> Result<Result<(), String>, String> {
    let outcomes = conn.run_command(cmd).map_err(|e| e.to_string())?;
    for outcome in outcomes {
        if let Err(e) = outcome {
            return Ok(Err(e.to_string()));
        }
    }
    Ok(Ok(()))
}

/// Backslash-escape what sway's command parser treats specially inside a
/// double-quoted argument.
///
/// Caveat, measured on sway 1.12: `strip_quotes()` runs only on arguments that
/// *start* with a quote, and it removes unescaped quote characters while
/// leaving every backslash in place. So `workspace "a\"b"` focuses a workspace
/// literally named `a\"b`, not `a"b`. Escaping keeps the command well-formed —
/// an unescaped quote would unbalance the argument and could swallow the rest
/// of the command — but a workspace whose name contains `"` or `\` is simply
/// not addressable through sway's parser in quoted form. Names with spaces,
/// colons and non-ASCII (`2: web`, `média`) round-trip exactly.
fn escape_for_sway(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use swayipc::CommandType;

    /// Deserialize a recorded `swaymsg -t ...` dump through swayipc's own
    /// reply decoder, so the tests exercise the real types.
    fn decode<D: serde::de::DeserializeOwned>(kind: CommandType, file: &str) -> D {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        let bytes = std::fs::read(format!("{path}{file}"))
            .unwrap_or_else(|e| panic!("cannot read fixture {file}: {e}"));
        kind.decode((u32::from(kind), bytes))
            .unwrap_or_else(|e| panic!("cannot decode fixture {file}: {e}"))
    }

    /// Decode a recorded `swaymsg -t subscribe` line through swayipc's own
    /// event decoder. Payload type 0 is the workspace event.
    fn workspace_event(file: &str) -> WorkspaceEvent {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        let bytes = std::fs::read(format!("{path}{file}"))
            .unwrap_or_else(|e| panic!("cannot read fixture {file}: {e}"));
        match Event::decode((0, bytes)) {
            Ok(Event::Workspace(ev)) => *ev,
            Ok(other) => panic!("fixture {file} is not a workspace event: {other:?}"),
            Err(e) => panic!("cannot decode fixture {file}: {e}"),
        }
    }

    fn fixture() -> Snapshot {
        let workspaces: Vec<Workspace> = decode(CommandType::GetWorkspaces, "workspaces.json");
        let tree: Node = decode(CommandType::GetTree, "tree.json");
        let outputs: Vec<Output> = decode(CommandType::GetOutputs, "outputs.json");
        build_snapshot(&workspaces, &tree, &outputs)
    }

    fn ws<'a>(s: &'a Snapshot, name: &str) -> &'a WorkspaceInfo {
        s.workspaces
            .iter()
            .find(|w| w.name == name)
            .unwrap_or_else(|| panic!("no workspace {name} in {:?}", names(s)))
    }

    fn names(s: &Snapshot) -> Vec<&str> {
        s.workspaces.iter().map(|w| w.name.as_str()).collect()
    }

    fn titles(w: &WorkspaceInfo) -> Vec<&str> {
        w.windows.iter().map(|x| x.title.as_str()).collect()
    }

    #[test]
    fn orders_outputs_then_numbered_then_named() {
        let s = fixture();
        // HEADLESS-1 first (get_outputs order), numbered ascending, then the
        // named ones in sway's own (creation) order — "abc" was created after
        // "media", so it comes last, exactly as swaybar shows it.
        assert_eq!(
            names(&s),
            vec!["1", "2: web", "alpha", "3", "10", "media", "abc"]
        );
        assert_eq!(
            s.workspaces
                .iter()
                .map(|w| w.output.as_str())
                .collect::<Vec<_>>(),
            vec![
                "HEADLESS-1",
                "HEADLESS-1",
                "HEADLESS-1",
                "HEADLESS-2",
                "HEADLESS-2",
                "HEADLESS-2",
                "HEADLESS-2"
            ]
        );
    }

    #[test]
    fn maps_negative_num_to_none() {
        let s = fixture();
        assert_eq!(ws(&s, "1").num, Some(1));
        assert_eq!(ws(&s, "2: web").num, Some(2));
        assert_eq!(ws(&s, "10").num, Some(10));
        assert_eq!(ws(&s, "alpha").num, None);
        assert_eq!(ws(&s, "media").num, None);
        assert!(s.workspaces.iter().all(|w| w.num != Some(-1)));
    }

    #[test]
    fn windows_in_tree_order_tiling_before_floating() {
        let s = fixture();
        // A tiled window and a floating one on workspace 1.
        assert_eq!(titles(ws(&s, "1")), vec!["one-tiled", "one-floating"]);
        // Workspace 3 holds a nested splitv container; its leaves come first,
        // in place of the container itself.
        assert_eq!(
            titles(ws(&s, "3")),
            vec!["three-left", "three-nested", "three-rt-top", "three-rt-bot"]
        );
    }

    #[test]
    fn no_split_container_is_reported_as_a_window() {
        let s = fixture();
        assert!(
            s.workspaces
                .iter()
                .flat_map(|w| &w.windows)
                .all(|w| !w.title.is_empty()),
            "a split container leaked in as a window"
        );
    }

    #[test]
    fn xwayland_class_is_used_as_app_id() {
        let s = fixture();
        let web = ws(&s, "2: web");
        assert_eq!(web.windows.len(), 1);
        // xterm is an xwayland view: no app_id, class "XTerm".
        assert_eq!(web.windows[0].app_id.as_deref(), Some("XTerm"));
    }

    #[test]
    fn wayland_app_id_is_used() {
        let s = fixture();
        assert_eq!(ws(&s, "1").windows[0].app_id.as_deref(), Some("foot"));
    }

    #[test]
    fn scratchpad_is_excluded() {
        let s = fixture();
        assert!(!names(&s).contains(&SCRATCH_WORKSPACE));
        assert!(
            s.workspaces
                .iter()
                .flat_map(|w| &w.windows)
                .all(|w| w.title != "scratch-win"),
            "a scratchpad window leaked into a workspace"
        );
    }

    #[test]
    fn focus_and_visibility_flags() {
        let s = fixture();
        assert_eq!(s.focused_output, "HEADLESS-1");
        let focused: Vec<&str> = s
            .workspaces
            .iter()
            .filter(|w| w.focused)
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(focused, vec!["1"]);
        let visible: Vec<&str> = s
            .workspaces
            .iter()
            .filter(|w| w.visible)
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(visible, vec!["1", "3"]);
        // Exactly one focused window overall, and it is the floating one.
        let focused_windows: Vec<&str> = s
            .workspaces
            .iter()
            .flat_map(|w| &w.windows)
            .filter(|w| w.focused)
            .map(|w| w.title.as_str())
            .collect();
        assert_eq!(focused_windows, vec!["one-floating"]);
        assert_eq!(
            s.visible_on("HEADLESS-2").map(|w| w.name.as_str()),
            Some("3")
        );
    }

    #[test]
    fn focused_output_falls_back_to_the_focused_workspace() {
        let workspaces: Vec<Workspace> = decode(CommandType::GetWorkspaces, "workspaces.json");
        let tree: Node = decode(CommandType::GetTree, "tree.json");
        let outputs: Vec<Output> = decode(CommandType::GetOutputs, "outputs_none_focused.json");
        assert!(outputs.iter().all(|o| !o.focused));
        let s = build_snapshot(&workspaces, &tree, &outputs);
        assert_eq!(s.focused_output, "HEADLESS-1");
    }

    #[test]
    fn focused_output_falls_back_to_the_first_output() {
        let tree: Node = decode(CommandType::GetTree, "tree.json");
        let outputs: Vec<Output> = decode(CommandType::GetOutputs, "outputs_none_focused.json");
        let s = build_snapshot(&[], &tree, &outputs);
        assert!(s.workspaces.is_empty());
        assert_eq!(s.focused_output, outputs[0].name);
    }

    #[test]
    fn empty_everything_is_not_a_panic() {
        let tree: Node = decode(CommandType::GetTree, "tree.json");
        let s = build_snapshot(&[], &tree, &[]);
        assert_eq!(s, Snapshot::default());
    }

    // -- workspace events -> the name-keyed cache messages ------------------

    /// Recorded from sway 1.12: `swaymsg rename workspace 2 to "2: mail"`.
    /// Sway leaves `old` null there, so the previous name comes from the node
    /// id, which the rename does not change.
    #[test]
    fn rename_without_old_is_resolved_through_the_node_id() {
        let ev = workspace_event("event_workspace_rename.json");
        assert!(ev.old.is_none(), "sway 1.12 sends no `old` on a rename");
        let names = HashMap::from([(7, "2".to_owned()), (4, "1".to_owned())]);
        assert!(matches!(
            workspace_msg(&ev, &names),
            Some(Msg::WorkspaceRenamed { from, to }) if from == "2" && to == "2: mail"
        ));
    }

    /// A workspace we have never seen a snapshot of cannot be renamed *from*
    /// anything; better to keep no thumbnail than the wrong one.
    #[test]
    fn rename_of_an_unknown_workspace_is_ignored() {
        let ev = workspace_event("event_workspace_rename.json");
        assert!(workspace_msg(&ev, &HashMap::new()).is_none());
    }

    /// If sway (or i3) ever does fill `old` in, that wins over the id lookup.
    #[test]
    fn rename_prefers_the_old_node_when_sway_sends_one() {
        let ev = workspace_event("event_workspace_rename_with_old.json");
        assert_eq!(ev.old.as_ref().and_then(|o| o.name.as_deref()), Some("2"));
        // A deliberately wrong map: the event's own `old` must be used.
        let names = HashMap::from([(7, "stale".to_owned())]);
        assert!(matches!(
            workspace_msg(&ev, &names),
            Some(Msg::WorkspaceRenamed { from, to }) if from == "2" && to == "2: mail"
        ));
    }

    /// Recorded from sway 1.12: leaving workspace 1 with nothing on it. The
    /// workspace is destroyed, so its cached frame has to go.
    #[test]
    fn empty_becomes_a_removal() {
        let ev = workspace_event("event_workspace_empty.json");
        assert!(matches!(
            workspace_msg(&ev, &HashMap::new()),
            Some(Msg::WorkspaceRemoved { name }) if name == "1"
        ));
    }

    /// `focus` has its own message (`WorkspaceSwitched`) and must not produce
    /// a second one — the workspace is neither renamed nor gone.
    #[test]
    fn focus_produces_no_cache_message() {
        let ev = workspace_event("event_workspace_focus.json");
        let names = HashMap::from([(7, "2".to_owned())]);
        assert!(workspace_msg(&ev, &names).is_none());
    }

    #[test]
    fn workspace_names_maps_ids_to_names() {
        let s = fixture();
        let names = workspace_names(&s);
        let ws = ws(&s, "2: web");
        assert_eq!(names.get(&ws.id).map(String::as_str), Some("2: web"));
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape_for_sway("2: web"), "2: web");
        assert_eq!(escape_for_sway(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_for_sway(r"back\slash"), r"back\\slash");
        assert_eq!(escape_for_sway("ěščř 🎉"), "ěščř 🎉");
    }

    /// The connection `switch_to` uses is opened once and kept: the first
    /// switch costs one file descriptor, every switch after it costs none.
    ///
    /// Talks to whatever `$SWAYSOCK` points at — throwaway nested sway only:
    /// `SWAYSOCK=... cargo test -- --ignored --nocapture live_switch`
    #[test]
    #[ignore]
    fn live_switch_reuses_one_connection() {
        fn open_fds() -> usize {
            std::fs::read_dir("/proc/self/fd").expect("/proc").count()
        }
        let s = snapshot().expect("snapshot failed");
        let name = s
            .workspaces
            .iter()
            .find(|w| w.focused)
            .expect("no focused workspace")
            .name
            .clone();

        let before = open_fds();
        switch_to(&name, None).expect("first switch");
        let after_first = open_fds();
        println!("fds: {before} -> {after_first} after the first switch");
        assert_eq!(
            after_first,
            before + 1,
            "the first switch should open one connection and hold on to it"
        );
        for _ in 0..10 {
            switch_to(&name, None).expect("switch");
        }
        assert_eq!(
            open_fds(),
            after_first,
            "further switches must reuse that connection"
        );
    }

    /// Talks to whatever `$SWAYSOCK` points at — run it only against a
    /// throwaway nested sway:
    /// `SWAYSOCK=... cargo test -- --ignored --nocapture live_snapshot`
    #[test]
    #[ignore]
    fn live_snapshot() {
        let s = snapshot().expect("snapshot failed");
        println!("focused_output: {}", s.focused_output);
        for w in &s.workspaces {
            println!(
                "  [{}] {:?} num={:?} on {} focused={} visible={}",
                w.id, w.name, w.num, w.output, w.focused, w.visible
            );
            for win in &w.windows {
                println!(
                    "      {:?} app_id={:?} focused={}",
                    win.title, win.app_id, win.focused
                );
            }
        }
        assert!(!s.focused_output.is_empty());
    }
}
