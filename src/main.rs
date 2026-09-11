//! svitek — a Sway workspace switcher panel.
//!
//! `svitek`            run the resident panel (from `exec` in the sway config)
//! `svitek toggle`     tell the running instance to toggle (bind this to Mod+A)
//! `svitek show|hide|next|prev|quit`

mod capture;
mod config;
mod control;
mod ipc;
mod model;
mod ui;

use crate::capture::Capturer;
use crate::model::{CaptureReason, Command, Msg, Snapshot, Thumbnail};
use async_channel::Sender;
use gtk4::gio::ApplicationFlags;
use gtk4::prelude::*;
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "org.movsq.svitek";

/// Minimum spacing between background captures of the same output.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(400);
/// How long to wait after a workspace switch before capturing the new
/// workspace — sway emits the event before it has rendered.
const ENTERED_DELAY: Duration = Duration::from_millis(150);
/// How long a toggle waits for its fresh frame before showing anyway.
const SHOW_FALLBACK: Duration = Duration::from_millis(60);
/// How long after showing the panel the cursor is nudged so sway gives the
/// panel pointer focus. Must stay well inside the panel's preview grace period.
const POINTER_NUDGE_DELAY: Duration = Duration::from_millis(40);

const HELP: &str = "\
svitek — a Sway workspace switcher panel

usage:
  svitek              run the panel daemon (put `exec svitek` in your sway config)
  svitek toggle       show the panel, or hide it if it is up (bind this to a key)
  svitek show         show the panel
  svitek hide         hide the panel
  svitek next         show the panel, or step the selection one workspace on
  svitek prev         the same, one workspace back (bind it to Mod+Shift+Tab)
  svitek quit         stop the running daemon
  svitek --help       this text
  svitek --version    print the version

environment:
  RUST_LOG            log level of the daemon, e.g. RUST_LOG=svitek=debug
  SVITEK_SOCKET       control socket path (default $XDG_RUNTIME_DIR/svitek.sock)";

fn main() {
    // swayipc reads I3SOCK before SWAYSOCK. Inside a sway session both point at
    // the same socket, but a shell that overrides SWAYSOCK (e.g. to talk to a
    // nested sway) usually still carries the outer I3SOCK, and then svitek
    // would silently attach to the wrong compositor. SWAYSOCK is the one that
    // means "this sway". First thing in main, before any thread exists.
    if let Some(sock) = std::env::var_os("SWAYSOCK") {
        if !sock.is_empty() {
            // SAFETY: single-threaded at this point; no other thread can be
            // reading the environment concurrently.
            unsafe { std::env::set_var("I3SOCK", sock) };
        }
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.len() {
        0 => run_daemon(),
        1 => run_client(&args[0]),
        _ => {
            eprintln!("svitek: expected at most one argument");
            eprintln!("{HELP}");
            std::process::exit(2);
        }
    }
}

fn run_client(arg: &str) {
    match arg {
        "-h" | "--help" => println!("{HELP}"),
        "--version" | "-V" => println!("svitek {}", env!("CARGO_PKG_VERSION")),
        other => match Command::parse(other) {
            Some(cmd) => {
                if let Err(e) = control::send(cmd) {
                    eprintln!("svitek: {e}");
                    std::process::exit(1);
                }
            }
            None => {
                eprintln!("svitek: unknown command `{other}`");
                eprintln!("{HELP}");
                std::process::exit(2);
            }
        },
    }
}

// ---------------------------------------------------------------------------
// daemon
// ---------------------------------------------------------------------------

fn run_daemon() {
    env_logger::init();

    let (config, config_error) = config::load();
    if let Some(e) = config_error {
        eprintln!("svitek: config: {e}");
        eprintln!("svitek: continuing with the built-in defaults");
    }

    // One channel for everything; the GTK main loop owns the receiver.
    let (tx, rx) = async_channel::unbounded::<Msg>();

    // First: the control socket doubles as the single-instance guard, so claim
    // it before starting any thread or touching the display.
    let _control = match control::spawn(tx.clone()) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("svitek: {e}");
            std::process::exit(1);
        }
    };
    // From here on the socket file is ours, and it must not outlive us — not
    // even when we are torn down without reaching the end of `run_daemon`.
    install_socket_reaper();
    install_wayland_death_watch();

    let _ipc = ipc::spawn(tx.clone());

    let capturer = match Capturer::spawn(tx.clone(), config.thumbnail_width, CAPTURE_INTERVAL) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("svitek: cannot start screen capture: {e}");
            eprintln!(
                "svitek: the panel needs the wlr-screencopy protocol (sway/wlroots) for its \
                 thumbnails; run svitek from inside your sway session"
            );
            control::cleanup();
            std::process::exit(1);
        }
    };

    install_signal_handlers(tx.clone());

    let app = gtk4::Application::new(Some(APP_ID), ApplicationFlags::NON_UNIQUE);
    {
        let capturer = capturer.clone();
        let started = std::cell::Cell::new(false);
        app.connect_activate(move |app| {
            // `activate` can in principle be emitted again; build once.
            if started.replace(true) {
                debug!("ignoring repeated activate");
                return;
            }
            activate(app, &config, capturer.clone(), rx.clone());
        });
    }

    // GTK must not see our own arguments (it would try to parse them).
    let code = app.run_with_args::<&str>(&[]);

    control::cleanup();
    capturer.shutdown();
    // The ipc / capture / control threads are detached on purpose: they block
    // on sway, wayland and accept(4) respectively and are killed by process
    // exit. Nothing here waits on them, so the process always gets to exit.
    std::process::exit(code.get() as i32);
}

fn activate(
    app: &gtk4::Application,
    config: &config::Config,
    capturer: Capturer,
    rx: async_channel::Receiver<Msg>,
) {
    // No window is mapped at start; without a hold the app would exit right away.
    let hold = app.hold();

    let state = Rc::new(RefCell::new(App::default()));

    let callbacks = ui::PanelCallbacks {
        switch: {
            let state = state.clone();
            Box::new(move |name: &str, num| {
                debug!("switching to workspace {name:?} (num {num:?})");
                {
                    // Only reached with the panel still up when a click did
                    // *not* close it (`close_on_select = false`): that click
                    // makes this workspace the origin, so closing later must
                    // not revert to where the panel opened.
                    //
                    // A committing switch — Enter, or a click with the default
                    // `close_on_select` — arrives after `hidden` has already
                    // run and cleared `shown_on` (ui.rs hides first, switches
                    // second, in one straight line on the GTK thread). So this
                    // block is correctly skipped there: `preview_origin` must
                    // stay `None` while the panel is down, and the revert has
                    // already been suppressed by `committed`.
                    let mut st = state.borrow_mut();
                    if st.shown_on.is_some() {
                        st.preview_origin = Some(name.to_string());
                        st.previewing = false;
                    }
                }
                if let Err(e) = ipc::switch_to(name, num) {
                    warn!("cannot switch to workspace {name:?}: {e}");
                }
            })
        },
        preview: {
            let state = state.clone();
            Box::new(move |name: &str, num| {
                // A preview is a real switch, so it is worth not making one at
                // all when we are already where the row points. Two cases:
                // the panel just opened and the pointer found the row the user
                // is already on (nothing has been previewed, so nothing to do),
                // and the pointer coming back to that row after a detour (that
                // one *is* a switch — it is how the user cancels a preview
                // without closing the panel).
                let target = {
                    let mut st = state.borrow_mut();
                    let Some(origin) = st.preview_origin.clone() else {
                        return;
                    };
                    if name == origin && !st.previewing {
                        debug!("preview of {name:?} skipped: already the origin");
                        return;
                    }
                    st.previewing = name != origin;
                    name.to_string()
                };
                debug!("previewing workspace {target:?} (num {num:?})");
                if let Err(e) = ipc::switch_to(&target, num) {
                    warn!("cannot preview workspace {target:?}: {e}");
                }
            })
        },
        hidden: {
            let state = state.clone();
            let capturer = capturer.clone();
            Box::new(move |committed: bool| {
                debug!("panel hidden (committed: {committed})");
                // Unless the user clicked a row, a preview has to be undone:
                // the panel is a look around, not a way to end up somewhere by
                // accident.
                let (pending, revert) = {
                    let mut st = state.borrow_mut();
                    st.shown_on = None;
                    let previewing = std::mem::take(&mut st.previewing);
                    let origin = st.preview_origin.take();
                    let revert = if previewing && !committed {
                        origin
                    } else {
                        None
                    };
                    (st.pending_show.take(), revert)
                };
                if let Some(p) = pending {
                    p.cancel();
                }
                capturer.set_paused(false);
                if let Some(origin) = revert {
                    debug!("reverting preview to workspace {origin:?}");
                    if let Err(e) = ipc::switch_to(&origin, None) {
                        warn!("cannot revert to workspace {origin:?}: {e}");
                    }
                }
            })
        },
    };

    let panel = ui::Panel::new(app, config, callbacks);

    let ctx = Ctx {
        state,
        panel,
        capturer,
        app: app.clone(),
        mode: config.mode,
        hold_selects_next: config.hold_selects_next,
    };

    glib::spawn_future_local(async move {
        // Keep the hold alive exactly as long as the message pump.
        let _hold = hold;
        while let Ok(msg) = rx.recv().await {
            ctx.handle(msg);
        }
        info!("message channel closed");
    });
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

/// A show that is waiting for its fresh capture (or for the fallback timer).
struct PendingShow {
    output: String,
    fallback: Option<glib::SourceId>,
    /// Steps asked for before the panel was even up. A second Mod+Tab can
    /// easily arrive inside the ~60 ms a show waits for its fresh frame, and
    /// dropping it would lose a keypress the user made; it is applied by
    /// `do_show` the moment the panel appears.
    steps: i32,
}

impl PendingShow {
    /// Drop the pending show without showing; cancels the fallback timer.
    fn cancel(self) {
        if let Some(id) = self.fallback {
            id.remove();
        }
    }
}

#[derive(Default)]
struct App {
    snapshot: Snapshot,
    /// Last known frame per workspace *name*. Memory only; lost on restart.
    thumbs: HashMap<String, Thumbnail>,
    /// The output the panel is currently shown on, if it is shown.
    shown_on: Option<String>,
    pending_show: Option<PendingShow>,
    /// The workspace that was focused when the panel was shown — where the user
    /// came from, and where hiding the panel puts them back. `Some` exactly
    /// while the panel is up.
    preview_origin: Option<String>,
    /// True once a hover preview has taken sway somewhere other than
    /// `preview_origin`. It is what makes hiding the panel a revert.
    previewing: bool,
}

/// Everything the message handling needs. Cloning is refcount bumps only, so
/// timer closures can hold their own copy.
#[derive(Clone)]
struct Ctx {
    state: Rc<RefCell<App>>,
    panel: Rc<ui::Panel>,
    capturer: Capturer,
    app: gtk4::Application,
    /// `config.mode`: the one thing that makes `Command::Toggle` mean
    /// something different here (a step, not a hide).
    mode: config::Mode,
    /// `config.hold_selects_next`: in hold mode, whether the press that opens
    /// the panel already counts as a step.
    hold_selects_next: bool,
}

impl Ctx {
    fn handle(&self, msg: Msg) {
        match msg {
            Msg::State(snapshot) => {
                self.state.borrow_mut().snapshot = snapshot;
                self.repaint();
            }

            // The workspace we *left* was already captured by the background
            // loop; only the newly entered one needs a fresh frame, and only
            // once sway has actually rendered it.
            Msg::WorkspaceSwitched { output, from, to } => {
                debug!("workspace switch on {output}: {from:?} -> {to}");
                // The full `State` follows only after ipc has re-queried sway,
                // and a frame of the *new* workspace can land in that gap.
                // Patch the visible flags now so attribution cannot file that
                // frame under the workspace we just left.
                {
                    let mut st = self.state.borrow_mut();
                    for w in &mut st.snapshot.workspaces {
                        if w.output == output {
                            w.visible = w.name == to;
                        }
                    }
                }
                let ctx = self.clone();
                glib::timeout_add_local_once(ENTERED_DELAY, move || {
                    if ctx.is_visible() {
                        // Captures are paused while the panel is up, and the
                        // panel would be in the frame anyway.
                        debug!("skipping post-switch capture of {output}: panel is visible");
                        return;
                    }
                    ctx.capturer.request(&output, CaptureReason::Entered);
                });
            }

            // The thumbnail cache is keyed by workspace name, so a rename has
            // to carry the cached frame over — the workspace, and what is on
            // it, did not change at all.
            Msg::WorkspaceRenamed { from, to } => {
                let moved = {
                    let mut st = self.state.borrow_mut();
                    // Whatever sat under the new name belonged to some earlier,
                    // now dead workspace of that name; this one owns it now.
                    st.thumbs.remove(&to);
                    match st.thumbs.remove(&from) {
                        Some(thumb) => {
                            st.thumbs.insert(to.clone(), thumb);
                            true
                        }
                        None => false,
                    }
                };
                debug!(
                    "workspace renamed {from:?} -> {to:?} ({})",
                    if moved {
                        "thumbnail moved"
                    } else {
                        "no thumbnail cached"
                    }
                );
                self.repaint();
            }

            // Sway destroys a workspace as soon as it is empty and hidden. A
            // later workspace of the same name is a different workspace, so the
            // cached frame must not survive: it shows windows that are gone.
            Msg::WorkspaceRemoved { name } => {
                let dropped = self.state.borrow_mut().thumbs.remove(&name).is_some();
                debug!(
                    "workspace {name:?} destroyed ({})",
                    if dropped {
                        "thumbnail dropped"
                    } else {
                        "no thumbnail cached"
                    }
                );
                self.repaint();
            }

            // A frame belongs to whatever is visible on that output *now*.
            Msg::Frame {
                output,
                thumb,
                reason,
            } => {
                let workspace = self
                    .state
                    .borrow()
                    .snapshot
                    .visible_on(&output)
                    .map(|w| w.name.clone());
                match workspace {
                    Some(name) => {
                        debug!(
                            "frame {}x{} for {output} -> workspace {name} ({reason:?})",
                            thumb.width, thumb.height
                        );
                        self.state
                            .borrow_mut()
                            .thumbs
                            .insert(name.clone(), thumb.clone());
                        if self.is_visible() {
                            self.panel.set_thumbnail(&name, &thumb);
                        }
                    }
                    None => debug!("frame for {output} has no visible workspace; dropped"),
                }
                if reason == CaptureReason::Toggle {
                    self.complete_pending_show(&output);
                }
            }

            Msg::CaptureFailed {
                output,
                reason,
                error,
            } => {
                warn!("capture of {output} failed ({reason:?}): {error}");
                if reason == CaptureReason::Toggle {
                    // Never leave the user waiting on a capture that will not come.
                    self.complete_pending_show(&output);
                }
            }

            Msg::CaptureOutputs(outputs) => info!("capture outputs: {}", outputs.join(", ")),

            Msg::Control(cmd) => match cmd {
                // In hold mode the binding is alt-tab, not a switch: sway
                // consumes Mod+Tab itself and runs the binding again, so a
                // second press reaches us as a second `toggle` and means "one
                // workspace on", never "close". Letting go of the modifier is
                // what closes it (ui.rs), and Esc / `hide` still do too.
                Command::Toggle if self.mode == config::Mode::Hold => self.step(1),
                Command::Toggle => {
                    if self.is_visible() || self.state.borrow().pending_show.is_some() {
                        self.hide();
                    } else {
                        self.begin_show();
                    }
                }
                Command::Show => {
                    if self.is_visible() {
                        debug!("show: already visible");
                    } else {
                        self.begin_show();
                    }
                }
                Command::Hide => self.hide(),
                // Explicit stepping, in both modes: bind `prev` to
                // Mod+Shift+Tab next to a `toggle` or `next` on Mod+Tab.
                Command::Next => self.step(1),
                Command::Prev => self.step(-1),
                Command::Quit => {
                    info!("quit requested");
                    self.shutdown();
                }
            },

            Msg::IpcLost(e) => {
                error!("sway IPC connection lost: {e}");
                self.shutdown();
            }
        }
    }

    fn is_visible(&self) -> bool {
        self.panel.is_visible()
    }

    /// Re-render the rows if the panel is up. Cheap enough for every `State`.
    fn repaint(&self) {
        if !self.is_visible() {
            return;
        }
        // Copy out first: never call into GTK while `state` is borrowed, a
        // widget callback could come back through us.
        let (snapshot, thumbs, output) = {
            let st = self.state.borrow();
            let output = st
                .shown_on
                .clone()
                .unwrap_or_else(|| st.snapshot.focused_output.clone());
            (st.snapshot.clone(), st.thumbs.clone(), output)
        };
        self.panel.update(&snapshot, &output, &thumbs);
    }

    /// Toggle/show: pause the background loop, ask for a fresh frame of the
    /// focused output, and show as soon as it lands (or after the fallback).
    fn begin_show(&self) {
        if self.state.borrow().pending_show.is_some() {
            debug!("show already pending");
            return;
        }
        let output = self.state.borrow().snapshot.focused_output.clone();
        if output.is_empty() {
            info!("no state from sway yet; ignoring show/toggle");
            return;
        }

        self.capturer.set_paused(true);
        self.capturer.request(&output, CaptureReason::Toggle);

        let fallback = {
            let ctx = self.clone();
            glib::timeout_add_local_once(SHOW_FALLBACK, move || {
                // Our own source has already fired: take the pending show
                // without touching (removing) the source id.
                let pending = ctx.state.borrow_mut().pending_show.take();
                if let Some(p) = pending {
                    debug!("toggle: no frame within {SHOW_FALLBACK:?}, showing anyway");
                    ctx.do_show(p.output, p.steps);
                }
            })
        };

        self.state.borrow_mut().pending_show = Some(PendingShow {
            output,
            fallback: Some(fallback),
            steps: 0,
        });
    }

    /// Move the selection `steps` workspaces on (wrapping), showing the panel
    /// first if it is down — `svitek next|prev`, and hold mode's Mod+Tab.
    ///
    /// The three states the panel can be in each answer differently: up, step
    /// it; still waiting for its first frame, queue the step (cancelling the
    /// show instead would swallow the keypress); down, this is a show — and in
    /// hold mode with `hold_selects_next` the opening press is itself the first
    /// step, the alt-tab convention, so a quick Mod+Tab tap lands on the next
    /// workspace. Otherwise the selection starts on the workspace the user is
    /// already on and the first press only opens the panel.
    fn step(&self, steps: i32) {
        if self.is_visible() {
            self.panel.step(steps);
            return;
        }
        let queued = match self.state.borrow_mut().pending_show.as_mut() {
            Some(p) => {
                p.steps += steps;
                true
            }
            None => false,
        };
        if queued {
            debug!("step {steps:+} queued behind the pending show");
            return;
        }
        self.begin_show();
        if self.mode == config::Mode::Hold && self.hold_selects_next {
            // `begin_show` may have declined (no output); only then is there
            // no pending show to hang the step on.
            if let Some(p) = self.state.borrow_mut().pending_show.as_mut() {
                debug!("hold mode: the opening press is the first step ({steps:+})");
                p.steps = steps;
            }
        }
    }

    /// The fresh frame for a pending show landed (or failed): show now.
    fn complete_pending_show(&self, output: &str) {
        let matches = matches!(&self.state.borrow().pending_show, Some(p) if p.output == output);
        if !matches {
            return;
        }
        let pending = self.state.borrow_mut().pending_show.take();
        let Some(p) = pending else { return };
        if let Some(id) = p.fallback {
            id.remove();
        }
        self.do_show(p.output, p.steps);
    }

    /// Map the panel on `output`, then apply any steps that arrived while it
    /// was still being prepared.
    fn do_show(&self, output: String, steps: i32) {
        let (snapshot, thumbs) = {
            let st = self.state.borrow();
            (st.snapshot.clone(), st.thumbs.clone())
        };
        // Where the user is right now. Every hover preview is measured against
        // this, and closing the panel comes back to it.
        let origin = snapshot
            .on_output(&output)
            .find(|w| w.focused)
            .or_else(|| snapshot.visible_on(&output))
            .map(|w| w.name.clone());
        self.panel.update(&snapshot, &output, &thumbs);
        self.panel.show(&output, origin.as_deref());
        debug!("panel shown on {output} (origin workspace {origin:?})");
        {
            let mut st = self.state.borrow_mut();
            st.shown_on = Some(output);
            st.preview_origin = origin;
            st.previewing = false;
        }
        if steps != 0 {
            debug!("applying {steps:+} step(s) queued while the show was pending");
            self.panel.step(steps);
        }
        // Once the surface is mapped, make sway re-evaluate what is under the
        // pointer (see `ipc::nudge_pointer`). Inside the panel's preview grace
        // period, so the resulting motion event cannot arm a hover preview.
        let ctx = self.clone();
        glib::timeout_add_local_once(POINTER_NUDGE_DELAY, move || {
            if ctx.is_visible() {
                if let Err(e) = ipc::nudge_pointer() {
                    warn!("cannot nudge the pointer: {e}");
                }
            }
        });
    }

    fn hide(&self) {
        let pending = self.state.borrow_mut().pending_show.take();
        if let Some(p) = pending {
            debug!("cancelling pending show");
            p.cancel();
        }
        if self.is_visible() {
            self.panel.hide();
        }
        // `PanelCallbacks::hidden` does the same thing (including the revert of
        // a preview, which is why this cannot happen twice: it clears
        // `previewing` first); both are idempotent and this way a cancelled
        // show can never leave captures paused or an origin behind.
        {
            let mut st = self.state.borrow_mut();
            st.shown_on = None;
            st.preview_origin = None;
            st.previewing = false;
        }
        self.capturer.set_paused(false);
    }

    fn shutdown(&self) {
        control::cleanup();
        self.capturer.shutdown();
        self.app.quit();
    }
}

// ---------------------------------------------------------------------------
// exit
// ---------------------------------------------------------------------------

/// Unlink the control socket from `exit()`, however we get there.
///
/// `Msg::Control(Quit)`, a signal and the end of `app.run_with_args` all reach
/// `control::cleanup()` on their own; this covers the `std::process::exit`
/// short-cuts and anything a library exits through, so that no path out of the
/// daemon leaves the file behind. (A stale socket is recovered at the next
/// start, but a process that exits cleanly must not leave one.)
///
/// It is deliberately *not* the whole answer for a dying sway: GTK ends the
/// process with `_exit(1)` when the Wayland connection drops, and `_exit` runs
/// no `atexit` handler at all. That path is `install_wayland_death_watch`.
///
/// Like every other cleanup it unlinks the path only while it still *is* the
/// socket this process bound: `svitek quit; svitek` can have the successor
/// bound and serving before our `exit()` gets this far, and an unconditional
/// `unlink` would take the new daemon's socket with it, leaving a daemon
/// running that no `svitek toggle` can reach. The `(st_dev, st_ino)` recorded
/// at bind time (`control::socket_identity`) is what tells the two apart.
///
/// The handler runs from `exit()`, not from a signal, but `exit()` can be
/// reached *from* one, so it is written to async-signal-safe rules anyway:
/// two syscalls on a `CString` and a `stat` buffer built while the process was
/// still healthy, no allocation, no locking, no formatting.
fn install_socket_reaper() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::OnceLock;

    static SOCKET: OnceLock<(CString, u64, u64)> = OnceLock::new();

    extern "C" fn reap() {
        let Some((path, dev, ino)) = SOCKET.get() else {
            return;
        };
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `path` is a live NUL-terminated string and `st` is a live,
        // correctly sized `stat` buffer. `lstat`, not `stat`: a symlink that
        // appeared at the path is not our socket, whatever it points at.
        if unsafe { libc::lstat(path.as_ptr(), st.as_mut_ptr()) } != 0 {
            return; // gone already — the normal paths remove it first
        }
        // SAFETY: `lstat` returned 0, so it filled the buffer.
        let st = unsafe { st.assume_init() };
        if st.st_dev == *dev && st.st_ino == *ino {
            // Nothing to do if this fails, and no one left to tell.
            unsafe { libc::unlink(path.as_ptr()) };
        }
    }

    let path = control::socket_path();
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        warn!("control socket path contains a NUL byte; not registering the exit handler");
        return;
    };
    let Some((dev, ino)) = control::socket_identity() else {
        warn!("no control socket is bound; not registering the exit handler");
        return;
    };
    if SOCKET.set((c_path, dev, ino)).is_err() {
        return; // already installed
    }
    if unsafe { libc::atexit(reap) } != 0 {
        warn!("cannot register the control socket exit handler");
    }
}

/// The one thing GTK says before it takes the process down with it.
const GDK_LOST_WAYLAND: &str = "Lost connection to Wayland compositor";

/// Clean up when GTK decides, on its own, that the process is over.
///
/// When sway exits, its Wayland socket goes down and GDK's event source answers
/// with exactly two calls: a `g_log_structured` of `GDK_LOST_WAYLAND`, and then
/// `_exit(1)` (measured in GTK 4.22.4, `gdk/wayland/gdkeventsource.c`). `_exit`
/// skips `atexit` handlers, and the main loop never runs again, so the
/// `Msg::IpcLost` the ipc thread queues at the same moment is never drained and
/// neither `control::cleanup()` nor the exit handler above would ever run.
///
/// A GLib writer function is the one hook GTK offers in between: it is called
/// synchronously from that log call, on this thread, just before the `_exit`.
/// Everything that is not that message is handed straight to the writer GLib
/// would have used anyway, so normal GTK logging is unchanged.
///
/// If a future GTK words it differently we simply lose the hook and are back to
/// today's stale socket, which the next start recovers from — so this is a
/// best-effort tidy-up, never something correctness depends on.
fn install_wayland_death_watch() {
    glib::log_set_writer_func(|level, fields| {
        let message = fields
            .iter()
            .find(|f| f.key() == "MESSAGE")
            .and_then(|f| f.value_str());
        if message.is_some_and(|m| m.contains(GDK_LOST_WAYLAND)) {
            info!("GTK lost the wayland connection; sway is gone");
            control::cleanup();
        }
        glib::log_writer_default(level, fields)
    });
}

// ---------------------------------------------------------------------------
// signals
// ---------------------------------------------------------------------------

/// SIGTERM/SIGINT become `Msg::Control(Quit)`, so they take the very same
/// shutdown path as `svitek quit`.
///
/// glib 0.22 no longer binds `g_unix_signal_add`, so this is the classic
/// self-pipe: the handler only writes one byte (async-signal-safe) and a
/// dedicated thread turns that into a message.
fn install_signal_handlers(tx: Sender<Msg>) {
    use std::sync::atomic::{AtomicI32, Ordering};
    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn handler(_signum: libc::c_int) {
        let fd = WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            // Nothing to do if this fails: we are in a signal handler.
            unsafe { libc::write(fd, b"q".as_ptr() as *const libc::c_void, 1) };
        }
    }

    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        warn!(
            "cannot create signal pipe: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    WRITE_FD.store(write_fd, Ordering::Relaxed);

    for signum in [libc::SIGTERM, libc::SIGINT] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signum, &action, std::ptr::null_mut()) != 0 {
                warn!(
                    "cannot install handler for signal {signum}: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    let spawned = std::thread::Builder::new()
        .name("svitek-signals".to_string())
        .spawn(move || loop {
            let mut buf = [0u8; 1];
            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
            if n <= 0 {
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return;
            }
            info!("caught a termination signal, shutting down");
            if tx.send_blocking(Msg::Control(Command::Quit)).is_err() {
                return;
            }
        });
    if let Err(e) = spawned {
        warn!("cannot spawn signal thread: {e}");
    }
}
