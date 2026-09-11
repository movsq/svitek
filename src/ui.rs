//! The GTK4 layer-shell panel. One resident window, hidden (unmapped) when not
//! shown, re-presented on the focused output on demand — that is what makes
//! toggle well under 100 ms.
//!
//! The layer surface covers the *whole* output (all four edges anchored,
//! exclusive zone 0), not just the panel: the panel proper is a frame inside
//! it, and the rest of the surface is a scrim that is invisible but not empty.
//! That is what lets a click outside the panel close it — the click lands on
//! our own surface, where we hit-test it against the frame, and it goes no
//! further; a surface only as wide as the panel would hand that click to
//! whatever is underneath, and clicking a window to dismiss the panel would
//! also click that window.
//!
//! The frame comes in two shapes, chosen by `config.position`, and everything
//! else in this module is written once for both:
//!
//! * **`center` (the default)** — a horizontal strip of *cards* floating in the
//!   middle of the output, one per workspace, side by side: thumbnail on top,
//!   up to three window lines under it. It is as wide as its cards and no
//!   wider than the output, so with more workspaces than fit it scrolls
//!   horizontally instead of being clipped. This is the layout that puts the
//!   workspaces where the eyes already are and keeps every thumbnail the same
//!   size however many there are.
//! * **`left` / `right`** — the original fixed-width, full-height column
//!   pinned to that edge, one row per workspace (thumbnail beside up to six
//!   window lines), scrolling vertically.
//!
//! Hovering a row (or a card — `Row` is the widget bundle either way) previews
//! that workspace, which — because sway only renders the workspace it is
//! showing — means really switching to it and leaving the panel up on top. The
//! wheel does the same thing without the pointer: a step moves the *selection*
//! one row down/up in the column layouts, one card right/left in the centered
//! one (clamped, no wrap), and previews it.
//! Hover and wheel share one notion of "what is being previewed right now"
//! (`Panel::previewed` plus the pending debounce), so whichever acted last is
//! where the next wheel step counts from. The rules live in two halves: this
//! module decides *when* (a 120 ms debounce, armed only by a real pointer
//! motion after `show()`, or by a wheel step) and `main.rs` decides *what*
//! (whether a switch is needed, and undoing it unless the user committed).
//! Enter always commits the selection: it marks the hide as a commit (so the
//! preview is not reverted) and switches. A click on a row does the same thing
//! by default — selecting a workspace is what the panel is for, so the click
//! that picks one also closes it — but `close_on_select = false` turns a click
//! into "go there and stay open" instead: the clicked row becomes the new
//! origin, previews measure from it, and several workspaces can be visited in
//! one showing. Both paths go through `Panel::commit`/`commit_is_a_plain_hide`,
//! so there is exactly one definition of what committing means. While the panel
//! is visible the rows are frozen: a focus change repaints CSS classes and
//! never rebuilds, because rebuilding would destroy the row under the pointer
//! and re-enter it.

use crate::config::{Colors, Config, Position};
use crate::model::{flags_only_change, Snapshot, Thumbnail, WindowInfo, WorkspaceInfo};

use gtk4::gdk;
use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Width of the text column (window titles) in px.
const TEXT_COLUMN_WIDTH: i32 = 260;
/// Horizontal padding/spacing that surrounds the thumbnail + text column.
/// row padding (8*2) + row border (2*2) + row spacing (10) + list padding (8*2).
const CHROME_WIDTH: i32 = 8 * 2 + 2 * 2 + 10 + 8 * 2;
/// How many window lines a row shows before collapsing into "+N more".
const MAX_WINDOW_LINES: usize = 6;
/// The same, for a *card* in the centered strip. Cards stand side by side, so
/// height is the scarce dimension there — and every card is as tall as the
/// tallest, so one busy workspace would otherwise stretch the whole strip.
/// Three lines is what keeps a card roughly as tall as it is wide.
const MAX_CARD_LINES: usize = 3;
/// Padding and border of a row/card, in px. The stylesheet is generated from
/// these, so `card_width` below and the CSS cannot drift apart.
const ROW_PADDING: i32 = 8;
const ROW_BORDER: i32 = 2;
/// Horizontal chrome around a card's thumbnail: padding and border, both sides.
/// A card is exactly as wide as its thumbnail plus this, and never wider: the
/// window lines under the thumbnail ellipsize instead of pushing it out (their
/// *natural* width is capped, see `window_line`).
const CARD_CHROME_WIDTH: i32 = 2 * (ROW_PADDING + ROW_BORDER);
/// Gap between two cards in the strip.
const CARD_SPACING: i32 = 10;
/// Padding between the strip's rounded background and the outermost cards.
/// Same 8 px as the column's `.ws-list`, so both frames breathe alike.
const STRIP_PADDING: i32 = 8;
/// Background of the part of the surface that is not the panel (the frame
/// paints its own on top). The scrim has to be invisible — the workspace under
/// it must look untouched, pixel for pixel — but it still has to *catch* the
/// click that closes the panel. Fully transparent does both here: measured on
/// sway 1.11 + GTK 4.22, a grim of the output is byte-identical outside the
/// frame while the panel is up, and an injected click at (900, 400) still
/// reaches the surface and never the window underneath. (If a GTK or sway
/// version ever drops the input region of a surface that paints nothing, the
/// fix is one step of alpha here — `rgba(0,0,0,0.01)` — at the price of the
/// pixels no longer being identical.)
const SCRIM_BG: &str = "transparent";
/// How long the pointer has to rest on a row — or the wheel has to sit still —
/// before the selection is previewed. Short enough to feel immediate, long
/// enough that sweeping the pointer across the list on the way to the row you
/// want does not switch workspaces on the way, and that spinning the wheel
/// through five rows is one workspace switch instead of five.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(120);
/// Hovering and the wheel are dead for this long after `show()`. The pointer is
/// very often already sitting over a row when the panel maps (that is where the
/// user's hand was), and a panel that switched workspaces the moment it opened
/// would be a bug. Belt and braces with the arm-on-motion rule below.
const PREVIEW_GRACE: Duration = Duration::from_millis(150);

/// A callback taking a workspace (name, num).
pub type WorkspaceFn = Box<dyn Fn(&str, Option<i32>)>;

/// What the panel asks the app to do.
pub struct PanelCallbacks {
    /// Switch to workspace (name, num) for good. Called on Enter, and on a row
    /// click — in either of the two shapes a click can take:
    ///
    /// * `close_on_select = true` (the default) and Enter: *after* the panel
    ///   has hidden itself with `committed = true`, so the app sees `hidden`
    ///   first and this call second, with the panel already down.
    /// * `close_on_select = false`: while the panel stays open, with that
    ///   workspace adopted as its new origin — closing later stays there.
    pub switch: WorkspaceFn,
    /// The pointer has rested on a row long enough to preview it (name, num).
    /// A preview is a *real* workspace switch — sway renders only the visible
    /// workspace, so showing one live means going there — with the panel left
    /// up on top of it. The app decides whether the switch is needed and
    /// remembers that it has to be undone when the panel closes.
    pub preview: WorkspaceFn,
    /// The panel was hidden (Esc, click, or `hide()`); the app resumes captures.
    ///
    /// `committed` is true when the hide is the first half of a commit — Enter,
    /// or a row click with `close_on_select` (the default): the `switch` that
    /// follows is what the user asked for, so a preview in progress must *not*
    /// be reverted. It is false for every other way out (Esc, a click outside,
    /// `svitek hide|toggle`), which is when the app takes the user back to the
    /// workspace they opened the panel on. With `close_on_select = false` a row
    /// click does not hide at all, so this callback is not part of it.
    pub hidden: Box<dyn Fn(bool)>,
}

/// Where a pending preview came from. The pointer leaving a row cancels a
/// *hover* debounce, but must not cancel one the wheel started: scrolling the
/// selected row into view moves the list under a stationary pointer, and the
/// crossing events that produces are not the user changing their mind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PreviewSource {
    Hover,
    Wheel,
}

/// A preview waiting out the debounce: the workspace it will switch to, and the
/// timer that will do it.
struct Pending {
    name: String,
    num: Option<i32>,
    source: PreviewSource,
    timer: glib::SourceId,
}

/// One rendered workspace row and the pieces of it we mutate in place.
struct Row {
    /// Workspace name — the key used by `set_thumbnail`.
    name: String,
    /// `num` from sway, carried so a wheel step or Enter can name the workspace
    /// to `PanelCallbacks` exactly the way a click on this row would.
    num: Option<i32>,
    /// The clickable row widget.
    root: gtk4::Box,
    /// Shows the thumbnail texture (empty when there is none yet).
    picture: gtk4::Picture,
    /// "no preview yet" placeholder, overlaid on the empty picture.
    hint: gtk4::Label,
    /// The overlay that fixes the thumbnail's size.
    thumb: gtk4::Overlay,
    /// One label per window line, in `WorkspaceInfo::windows` order (capped at
    /// `MAX_WINDOW_LINES`). Kept so a focus change can move the `.win-focused`
    /// class without rebuilding the row out from under the pointer.
    titles: Vec<gtk4::Label>,
}

/// The resident panel window. Single-threaded (GTK main context only).
pub struct Panel {
    window: gtk4::ApplicationWindow,
    /// The box inside the ScrolledWindow holding the rows: vertical (a column
    /// of rows) for `left`/`right`, horizontal (a strip of cards) for `center`.
    list: gtk4::Box,
    scroller: gtk4::ScrolledWindow,
    /// Which of the two layouts we built, kept because it decides how a row is
    /// assembled, which way the selection scrolls, and nothing else.
    position: Position,
    rows: RefCell<Vec<Row>>,
    /// The workspaces the current rows were built from — used to skip a rebuild
    /// when `update()` is called with unchanged data (the common case: a frame
    /// arrived, or an unrelated sway event fired).
    rendered: RefCell<Vec<WorkspaceInfo>>,
    callbacks: PanelCallbacks,
    thumb_width: i32,
    /// `config.close_on_select`: whether clicking a row closes the panel (the
    /// default) or only moves the origin to it and leaves the panel up.
    close_on_select: bool,
    visible: Cell<bool>,
    /// The workspace that was focused when the panel was shown, for as long as
    /// it is shown. The `.focused` marker stays on *this* row the whole time,
    /// even while a preview has moved sway's focus elsewhere: the marker means
    /// "where you came from", and it is where Esc will put you back.
    origin: RefCell<Option<String>>,
    /// The workspace the last preview asked for (the origin, at `show()`).
    /// An enter for the workspace already being previewed is ignored, which is
    /// what keeps a synthesized enter after a rebuild from firing again.
    ///
    /// This is also the wheel's **selection**: a wheel step counts from here
    /// (or from `pending`, when a preview is still waiting out its debounce),
    /// so hover and wheel never disagree about which row is live — whichever
    /// acted last is what the next step moves from.
    previewed: RefCell<Option<String>>,
    /// The debounce timer, with the workspace it will preview when it fires and
    /// what started it.
    pending: RefCell<Option<Pending>>,
    /// Leftover fraction of a wheel/touchpad delta that has not yet added up to
    /// a whole row. Reset on every `show()`.
    wheel_accum: Cell<f64>,
    /// False until the pointer has actually *moved* over the panel since it was
    /// shown. GTK delivers an enter to whatever is under a stationary pointer
    /// as soon as the surface maps (and again after every rebuild); that is not
    /// the user hovering anything, so it must not preview.
    hover_armed: Cell<bool>,
    /// When the panel was last shown; `PREVIEW_GRACE` is measured from here.
    shown_at: Cell<Option<Instant>>,
    /// Set by `commit()` just before `hide()`, so the `hidden` callback knows
    /// this hide is a commit and must not revert the preview. Read (and
    /// cleared) inside `hide()`, which is what makes the two race-free: they
    /// are one straight-line sequence on the GTK main thread.
    committed: Cell<bool>,
    /// Weak self-reference so widget callbacks built in `update()` can reach us
    /// without creating a reference cycle.
    me: RefCell<Weak<Panel>>,
}

impl Panel {
    /// Build the window (layer OVERLAY, anchored on all four edges so the
    /// surface covers the output, exclusive zone 0, keyboard mode so that Esc
    /// closes it) and the panel frame inside it — either a `panel_width`-wide,
    /// full-height column at the edge `config.position` names, or a centered
    /// strip of cards — with a click-swallowing scrim over the rest of the
    /// output. Does not show it. Installs the CSS built from `config.colors`.
    pub fn new(app: &gtk4::Application, config: &Config, callbacks: PanelCallbacks) -> Rc<Panel> {
        let thumb_width = config.thumbnail_width.clamp(80, 1000) as i32;
        let panel_width = thumb_width + TEXT_COLUMN_WIDTH + CHROME_WIDTH;
        let centered = config.position == Position::Center;

        install_css(&config.colors);

        // default_height(1), not a fixed height: the surface is stretched by the
        // compositor between the edges it is anchored to, and asking GTK for a
        // tall (or non-resizable) window instead fights that and loses the full
        // height. Same for the width now that left *and* right are anchored —
        // gtk4-layer-shell asks for 0 on an axis with both edges anchored, and
        // sway answers with the size of the output.
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("svitek")
            .default_width(1)
            .default_height(1)
            .build();
        window.add_css_class("svitek");

        // --- layer shell ---------------------------------------------------
        window.init_layer_shell();
        window.set_namespace(Some("svitek"));
        window.set_layer(Layer::Overlay);
        window.set_exclusive_zone(0);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        // KeyboardMode: the handoff asked for OnDemand, but sway does not hand
        // keyboard focus to an on-demand layer surface just because it mapped —
        // it keeps focus on the focused container until the surface is clicked,
        // so Esc silently does nothing until then. Measured on a nested sway
        // (virtual keyboard injected via virtual-keyboard-unstable-v1, panel
        // shown over a focused terminal):
        //
        //   OnDemand  -> seat focus stays on the window, window is-active=false,
        //                injected Escape never reaches the panel.
        //   Exclusive -> seat focus moves to the layer surface immediately,
        //                is-active=true within 200 ms, Escape hides the panel.
        //
        // Exclusive costs nothing here: sway resolves its own bindings before
        // forwarding keys to the focused surface (verified: Mod+A still fired
        // its sway binding while the panel held exclusive focus), and the
        // surface is unmapped for as long as the panel is hidden.
        window.set_keyboard_mode(KeyboardMode::Exclusive);

        // --- contents ------------------------------------------------------
        // Cards side by side, or rows stacked. One `gtk4::Box` either way, so
        // everything downstream (rebuild, hit tests, `compute_bounds`) is the
        // same code for both.
        let list = if centered {
            let strip = gtk4::Box::new(gtk4::Orientation::Horizontal, CARD_SPACING);
            strip.add_css_class("ws-strip");
            strip
        } else {
            let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            column.add_css_class("ws-list");
            column
        };

        let scroller = gtk4::ScrolledWindow::builder()
            // The scrollable axis is the one the workspaces run along.
            .hscrollbar_policy(if centered {
                gtk4::PolicyType::Automatic
            } else {
                gtk4::PolicyType::Never
            })
            .vscrollbar_policy(if centered {
                gtk4::PolicyType::Never
            } else {
                gtk4::PolicyType::Automatic
            })
            // A scrollable axis asks for nothing by default (that is the whole
            // point of scrolling). The strip must ask for its cards instead, so
            // that a handful of workspaces make a small strip rather than a
            // full-width one; the column's width is pinned below and its height
            // is meant to be the whole output, so it keeps asking for nothing.
            .propagate_natural_width(centered)
            .propagate_natural_height(centered)
            .vexpand(true)
            .child(&list)
            .build();
        scroller.add_css_class("panel");
        scroller.set_hexpand(true);
        if centered {
            // The strip is as wide as its cards and as tall as one card row —
            // and, because `halign`/`valign` are not Fill, GTK clamps that
            // natural size to what is available (`adjust_for_align` takes the
            // MIN). So nine cards on a 1280 px output do not overflow or get
            // clipped: the strip stops at the output's width and the horizontal
            // scrollbar takes over.
            scroller.set_halign(gtk4::Align::Center);
            scroller.set_valign(gtk4::Align::Center);
            scroller.set_vexpand(true);
            // A permanent scrollbar, not GTK's fade-in overlay: when more cards
            // exist than fit, the bar is the only hint that they do, and it has
            // to be there before the user scrolls, not after. Costs ~13 px of
            // height under the strip and shows only when there is overflow.
            scroller.set_overlay_scrolling(false);
        } else {
            // The panel proper: exactly `panel_width` wide (halign != Fill makes
            // GTK allocate the natural width, which the size request pins), full
            // height, at the configured edge of the surface.
            scroller.set_size_request(panel_width, -1);
            scroller.set_halign(match config.position {
                Position::Right => gtk4::Align::End,
                _ => gtk4::Align::Start,
            });
            scroller.set_valign(gtk4::Align::Fill);
        }

        let scrim = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        scrim.add_css_class("scrim");
        scrim.set_hexpand(true);
        scrim.set_vexpand(true);
        scrim.append(&scroller);
        window.set_child(Some(&scrim));

        // Realize now, at startup, rather than on the first `show()`: creating
        // the GdkSurface and the GSK renderer is the single most expensive step
        // (~280 ms measured on a software-rendered headless sway) and it would
        // otherwise land on the user's first toggle. Realizing does not map the
        // window and gtk4-layer-shell only creates the layer surface on map.
        gtk4::prelude::WidgetExt::realize(&window);

        let panel = Rc::new(Panel {
            window,
            list,
            scroller,
            position: config.position,
            rows: RefCell::new(Vec::new()),
            rendered: RefCell::new(Vec::new()),
            callbacks,
            thumb_width,
            close_on_select: config.close_on_select,
            visible: Cell::new(false),
            origin: RefCell::new(None),
            previewed: RefCell::new(None),
            pending: RefCell::new(None),
            wheel_accum: Cell::new(0.0),
            hover_armed: Cell::new(false),
            shown_at: Cell::new(None),
            committed: Cell::new(false),
            me: RefCell::new(Weak::new()),
        });
        *panel.me.borrow_mut() = Rc::downgrade(&panel);

        // --- Esc and Enter ---------------------------------------------------
        // CAPTURE phase: a focused child (button, scrolled window, …) must not
        // get a chance to swallow the key first. Esc cancels (hide + revert to
        // the origin); Enter commits the current selection — the same thing a
        // click on that row does, unless `close_on_select` is off, in which
        // case Enter is the only way to commit.
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(&panel);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => {
                    p.hide();
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => {
                    p.commit_selection();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        panel.window.add_controller(keys);

        // --- the wheel -------------------------------------------------------
        // On the *window*, in the CAPTURE phase, and always claimed: a wheel
        // step anywhere on the surface — a row, the panel padding, the scrim —
        // moves the selection, and the ScrolledWindow inside must not also
        // scroll the list out from under it. (We scroll the selected row into
        // view ourselves instead.) VERTICAL without DISCRETE gives us the raw
        // deltas: a mouse wheel arrives as ±1.0 per detent, a touchpad in
        // fractions, and `Panel::on_scroll` accumulates either into whole rows.
        //
        // VERTICAL in the centered layout too, where the selection moves
        // sideways: the wheel most people have only *has* a vertical axis, and
        // "down = the next workspace" is the same gesture in both layouts.
        // Adding HORIZONTAL would mean summing two axes and double-counting a
        // diagonal touchpad swipe, for a gesture no mouse can make.
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(&panel);
        scroll.connect_scroll(move |_, _dx, dy| {
            if let Some(p) = weak.upgrade() {
                p.on_scroll(dy);
            }
            glib::Propagation::Stop
        });
        panel.window.add_controller(scroll);

        // --- click outside the panel ---------------------------------------
        // The surface is the whole output, so "outside the panel" is a hit test
        // against the frame, not something the compositor answers by telling us
        // which surface was clicked. CAPTURE phase, on the window child: the
        // press has to be judged before any row sees it. Any button closes —
        // dismissing is not a left-click gesture.
        let outside = gtk4::GestureClick::new();
        outside.set_button(0);
        outside.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(&panel);
        let frame = panel.scroller.clone();
        let area = scrim.clone();
        outside.connect_pressed(move |gesture, _, x, y| {
            let inside = frame
                .compute_bounds(&area)
                .is_some_and(|b| b.contains_point(&graphene::Point::new(x as f32, y as f32)));
            if inside {
                // Give the sequence up explicitly so the row's own gesture gets
                // it: a click on a row must still switch workspaces.
                gesture.set_state(gtk4::EventSequenceState::Denied);
                return;
            }
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            if let Some(p) = weak.upgrade() {
                p.hide();
            }
        });
        scrim.add_controller(outside);

        // The layer surface must never be destroyed by a compositor `closed`
        // event or by GTK's own close handling; we only ever unmap it.
        panel
            .window
            .connect_close_request(|_| glib::Propagation::Stop);

        panel
    }

    /// Replace the rows with `snapshot`'s workspaces on `output`, marking the
    /// focused one, and using `thumbs` (keyed by workspace *name*) where
    /// available, a placeholder ("no preview yet") otherwise. Cheap enough to
    /// call on every `Msg::State` even while hidden.
    ///
    /// While the panel is *visible* a change that is only focus/visibility
    /// flags is applied in place instead of rebuilding: a hover preview is a
    /// real workspace switch, so sway reports a focus change for every preview,
    /// and rebuilding would destroy the row the pointer is on. See
    /// `model::flags_only_change`.
    pub fn update(&self, snapshot: &Snapshot, output: &str, thumbs: &HashMap<String, Thumbnail>) {
        let wanted: Vec<WorkspaceInfo> = snapshot.on_output(output).cloned().collect();

        let changed = *self.rendered.borrow() != wanted;
        if changed {
            let in_place = self.visible.get() && flags_only_change(&self.rendered.borrow(), &wanted);
            if in_place {
                log::debug!("focus flags changed while the panel is up; rows kept, classes repainted");
                self.apply_flags(&wanted);
            } else {
                self.rebuild(&wanted);
            }
            *self.rendered.borrow_mut() = wanted;
        }

        // Thumbnails are refreshed either way — they are not part of `Snapshot`.
        for row in self.rows.borrow().iter() {
            match thumbs.get(&row.name) {
                Some(t) => apply_thumb(row, t, self.thumb_width),
                None => clear_thumb(row, self.thumb_width),
            }
        }
    }

    /// Update one thumbnail in place without rebuilding rows (called when a
    /// `Msg::Frame` arrives while visible).
    pub fn set_thumbnail(&self, workspace: &str, thumb: &Thumbnail) {
        for row in self.rows.borrow().iter() {
            if row.name == workspace {
                apply_thumb(row, thumb, self.thumb_width);
            }
        }
    }

    /// Show on the GDK monitor whose connector is `output`. Grabs keyboard
    /// focus so Esc works immediately. `origin` is the workspace the user is
    /// on right now: the row that keeps the `.focused` marker while the panel
    /// is up, and the one a hover of which is a no-op until something else has
    /// been previewed.
    pub fn show(&self, output: &str, origin: Option<&str>) {
        match monitor_for(output) {
            Some(m) => self.window.set_monitor(Some(&m)),
            None => {
                log::warn!("no GDK monitor with connector {output:?}; using the compositor default");
                self.window.set_monitor(None);
            }
        }
        self.cancel_pending();
        *self.origin.borrow_mut() = origin.map(str::to_string);
        // The selection starts on the origin row: the first wheel step down
        // moves to the row below the one the user is on.
        *self.previewed.borrow_mut() = origin.map(str::to_string);
        self.wheel_accum.set(0.0);
        self.hover_armed.set(false);
        self.shown_at.set(Some(Instant::now()));
        self.mark_previewing(None);
        self.visible.set(true);
        self.window.present();
        // Rows are laid out during the first frame after `present()`, so the
        // focused row's position is only known on the next idle.
        let me = self.me.borrow().clone();
        glib::idle_add_local_once(move || {
            if let Some(p) = me.upgrade() {
                p.scroll_focused_into_view();
            }
        });
    }

    /// Keep the focused workspace visible when there are more rows than fit.
    fn scroll_focused_into_view(&self) {
        let rows = self.rows.borrow();
        let Some(row) = rows.iter().find(|r| r.root.has_css_class("focused")) else {
            return;
        };
        self.scroll_into_view(row);
    }

    /// Same, for the row the wheel just selected — the selection has to stay on
    /// screen when there are more workspaces than fit.
    fn scroll_name_into_view(&self, name: &str) {
        let rows = self.rows.borrow();
        let Some(row) = rows.iter().find(|r| r.name == name) else {
            return;
        };
        self.scroll_into_view(row);
    }

    /// Scroll `row` into view if it is not fully visible, centring it in the
    /// page. A no-op when everything fits (no scrolling to do).
    ///
    /// Along the axis the workspaces run in: the column layouts scroll the
    /// vertical adjustment, the centered strip the horizontal one. The
    /// arithmetic is the same either way and lives in `scroll_target`.
    fn scroll_into_view(&self, row: &Row) {
        let Some(bounds) = row.root.compute_bounds(&self.list) else {
            return;
        };
        let horizontal = self.position == Position::Center;
        let adj = if horizontal {
            self.scroller.hadjustment()
        } else {
            self.scroller.vadjustment()
        };
        let (start, extent) = if horizontal {
            (bounds.x() as f64, bounds.width() as f64)
        } else {
            (bounds.y() as f64, bounds.height() as f64)
        };
        if let Some(target) = scroll_target(
            start,
            extent,
            adj.value(),
            adj.page_size(),
            adj.lower(),
            adj.upper(),
        ) {
            adj.set_value(target);
        }
    }

    pub fn hide(&self) {
        let committed = self.committed.replace(false);
        if !self.visible.replace(false) {
            // Already hidden: `hidden()` fires exactly once per hide.
            return;
        }
        self.cancel_pending();
        self.hover_armed.set(false);
        self.wheel_accum.set(0.0);
        self.shown_at.set(None);
        *self.previewed.borrow_mut() = None;
        *self.origin.borrow_mut() = None;
        self.mark_previewing(None);
        self.window.set_visible(false);
        (self.callbacks.hidden)(committed);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.get()
    }

    /// The underlying window. Exposed for diagnostics/tests (latency probes);
    /// `main.rs` does not need it.
    #[allow(dead_code)]
    pub fn window(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }

    // --------------------------------------------------------- selection --

    /// A motion event landed on `name`'s row. The *first* one after `show()`
    /// only arms hovering; from then on any motion may start the debounce.
    fn on_hover_motion(&self, name: &str, num: Option<i32>) {
        if !self.visible.get() {
            return;
        }
        // Not the user hovering yet: the pointer has not moved since the panel
        // mapped, or it has barely had time to.
        if self.shown_at.get().is_none_or(|t| t.elapsed() < PREVIEW_GRACE) {
            return;
        }
        self.hover_armed.set(true);
        self.arm_hover(name, num);
    }

    /// The pointer crossed into `name`'s row. Only meaningful once hovering is
    /// armed: an enter also arrives for a stationary pointer when the surface
    /// maps, after every rebuild, and whenever the list scrolls under the
    /// pointer — which is exactly what a wheel step does.
    fn on_hover_enter(&self, name: &str, num: Option<i32>) {
        if self.visible.get() && self.hover_armed.get() {
            self.arm_hover(name, num);
        }
    }

    /// Start (or keep) the debounce for `name`. Leaving a row cancels it; only
    /// closing the panel reverts a preview, so this never fires anything on the
    /// way out.
    fn arm_hover(&self, name: &str, num: Option<i32>) {
        // Already showing this workspace: nothing to switch to, and this is the
        // guard that makes a post-rebuild enter under a stationary pointer a
        // no-op rather than a second switch.
        if self.previewed.borrow().as_deref() == Some(name) {
            self.cancel_hover_pending();
            return;
        }
        // A timer already running for this row keeps running: restarting it on
        // every motion event would mean a pointer that drifts never fires.
        if self
            .pending
            .borrow()
            .as_ref()
            .is_some_and(|p| p.name == name && p.source == PreviewSource::Hover)
        {
            return;
        }
        self.arm_preview(name, num, PreviewSource::Hover);
    }

    /// A wheel step (or a touchpad delta) on the surface: move the selection.
    ///
    /// `dy` is accumulated so a mouse wheel (±1.0 per detent) and a touchpad
    /// (fractions) both come out as whole rows, and every step restarts the
    /// debounce — spinning through five rows is one workspace switch, not five.
    fn on_scroll(&self, dy: f64) {
        if !self.visible.get() {
            return;
        }
        // Same grace as hovering, for the same reason: the panel must not
        // switch workspaces because of an event that was on its way in when it
        // mapped.
        if self.shown_at.get().is_none_or(|t| t.elapsed() < PREVIEW_GRACE) {
            log::debug!("wheel ignored: within {PREVIEW_GRACE:?} of show()");
            return;
        }

        let (steps, rest) = wheel_steps(self.wheel_accum.get() + dy);
        self.wheel_accum.set(rest);
        if steps == 0 {
            return;
        }

        // Count from whatever is live right now: the row a preview is already
        // waiting to switch to, else the previewed row, else the origin. That
        // is what makes hover and wheel one selection — and what makes four
        // quick steps land four rows down rather than one.
        let (base, len, target) = {
            let rows = self.rows.borrow();
            if rows.is_empty() {
                return;
            }
            let live = self
                .pending
                .borrow()
                .as_ref()
                .map(|p| p.name.clone())
                .or_else(|| self.previewed.borrow().clone())
                .or_else(|| self.origin.borrow().clone());
            let base = live
                .and_then(|n| rows.iter().position(|r| r.name == n))
                .unwrap_or(0);
            let target = clamp_step(base, steps, rows.len());
            (base, rows.len(), target)
        };

        if target == base {
            log::debug!("wheel {steps:+} clamped at row {base} of {len}; selection unchanged");
            return;
        }
        let (name, num) = {
            let rows = self.rows.borrow();
            (rows[target].name.clone(), rows[target].num)
        };
        log::debug!("wheel {steps:+}: selection {base} -> {target} ({name:?})");
        // The wheel acted last, so it owns the selection: a crossing event
        // produced by the list scrolling below must not re-arm hovering behind
        // its back. A real pointer motion arms it again.
        self.hover_armed.set(false);
        self.arm_preview(&name, num, PreviewSource::Wheel);
        self.scroll_name_into_view(&name);
    }

    /// Enter: take the selection for real. The selection is what a preview is
    /// on its way to, else what is being previewed (the origin, until something
    /// moved it), and `commit` does the rest.
    fn commit_selection(&self) {
        let pending = self.pending.borrow().as_ref().map(|p| (p.name.clone(), p.num));
        let target = pending.or_else(|| {
            let previewed = self.previewed.borrow().clone();
            previewed.map(|name| {
                let num = self.rows.borrow().iter().find(|r| r.name == name).and_then(|r| r.num);
                (name, num)
            })
        });
        self.commit(target, "Enter");
    }

    /// A row was clicked. With `close_on_select` (the default) that is a commit
    /// of *that* row — identical to Enter with the selection on it, whatever
    /// hover or the wheel had selected, and whatever preview is still in flight
    /// (`commit` hides, which cancels it, and the click's own target wins).
    /// Otherwise the click only moves the origin here and switches, leaving the
    /// panel up so more workspaces can be visited.
    fn on_row_clicked(&self, name: &str, num: Option<i32>) {
        if self.close_on_select {
            self.commit(Some((name.to_string(), num)), "click");
        } else {
            self.adopt_origin(name);
            log::debug!("click switches to workspace {name:?}; panel stays open");
            (self.callbacks.switch)(name, num);
        }
    }

    /// Take `target` for real: hide as a *commit* (so the `hidden` callback
    /// does not revert the preview), then ask for the switch — in that order,
    /// because `switch` must see the panel already down (see `main.rs`). When
    /// the target is where the user already is and nothing has been previewed
    /// away from it, there is nowhere to go and hiding *is* the commit.
    /// `why` only names the gesture in the log.
    fn commit(&self, target: Option<(String, Option<i32>)>, why: &str) {
        if !self.visible.get() {
            return;
        }
        let plain_hide = commit_is_a_plain_hide(
            target.as_ref().map(|(n, _)| n.as_str()),
            self.previewed.borrow().as_deref(),
            self.origin.borrow().as_deref(),
            self.pending.borrow().is_some(),
        );

        self.committed.set(true);
        self.hide();
        match target {
            Some((name, num)) if !plain_hide => {
                log::debug!("{why} commits workspace {name:?}");
                (self.callbacks.switch)(&name, num);
            }
            _ => log::debug!("{why} commits the origin workspace; nothing to switch to"),
        }
    }

    /// Arm the debounce for `name`, replacing whatever was pending.
    fn arm_preview(&self, name: &str, num: Option<i32>, source: PreviewSource) {
        self.cancel_pending();
        let me = self.me.borrow().clone();
        let target = name.to_string();
        let timer = glib::timeout_add_local_once(PREVIEW_DEBOUNCE, move || {
            let Some(p) = me.upgrade() else { return };
            // Our own source has just fired; drop the id without removing it.
            p.pending.borrow_mut().take();
            p.fire_preview(&target, num);
        });
        *self.pending.borrow_mut() = Some(Pending {
            name: name.to_string(),
            num,
            source,
            timer,
        });
    }

    fn cancel_pending(&self) {
        let pending = self.pending.borrow_mut().take();
        if let Some(p) = pending {
            p.timer.remove();
        }
    }

    /// Cancel a pending preview only if the *pointer* started it. The wheel's
    /// own pending must survive the pointer leaving a row, because scrolling
    /// the selected row into view is what moved the row out from under it.
    fn cancel_hover_pending(&self) {
        let is_hover = self
            .pending
            .borrow()
            .as_ref()
            .is_some_and(|p| p.source == PreviewSource::Hover);
        if is_hover {
            self.cancel_pending();
        }
    }

    /// The debounce elapsed with the pointer still on `name`: preview it.
    fn fire_preview(&self, name: &str, num: Option<i32>) {
        if !self.visible.get() {
            return;
        }
        *self.previewed.borrow_mut() = Some(name.to_string());
        self.mark_previewing(Some(name));
        (self.callbacks.preview)(name, num);
    }

    /// Make `name` the origin while the panel stays open (a row was clicked
    /// with `close_on_select = false`): the `.focused` marker moves to its row,
    /// any pending or shown preview is forgotten, and the wheel/hover selection
    /// restarts from here.
    fn adopt_origin(&self, name: &str) {
        self.cancel_pending();
        *self.origin.borrow_mut() = Some(name.to_string());
        *self.previewed.borrow_mut() = Some(name.to_string());
        for row in self.rows.borrow().iter() {
            if row.name == name {
                row.root.add_css_class("focused");
            } else {
                row.root.remove_css_class("focused");
            }
        }
        self.mark_previewing(None);
    }

    /// Put the `previewing` class on `name`'s row and nowhere else. The origin
    /// row never gets it: it already carries `.focused`, and hovering it is how
    /// you go *back*, not a preview of somewhere else.
    fn mark_previewing(&self, name: Option<&str>) {
        let origin = self.origin.borrow().clone();
        let wanted = name.filter(|n| origin.as_deref() != Some(*n));
        for row in self.rows.borrow().iter() {
            if wanted == Some(row.name.as_str()) {
                row.root.add_css_class("previewing");
            } else {
                row.root.remove_css_class("previewing");
            }
        }
    }

    /// Which row wears the `.focused` marker. While the panel is up that is the
    /// origin, whatever sway currently thinks is focused — a preview must not
    /// move the marker, or the user loses sight of where Esc takes them back to.
    fn is_marked_focused(&self, ws: &WorkspaceInfo) -> bool {
        match &*self.origin.borrow() {
            Some(origin) => ws.name == *origin,
            None => ws.focused,
        }
    }

    /// Repaint the focus-derived CSS classes without touching the widget tree.
    fn apply_flags(&self, workspaces: &[WorkspaceInfo]) {
        for (row, ws) in self.rows.borrow().iter().zip(workspaces) {
            if self.is_marked_focused(ws) {
                row.root.add_css_class("focused");
            } else {
                row.root.remove_css_class("focused");
            }
            for (label, w) in row.titles.iter().zip(&ws.windows) {
                if w.focused {
                    label.add_css_class("win-focused");
                } else {
                    label.remove_css_class("win-focused");
                }
            }
        }
    }

    // ---------------------------------------------------------------- rows --

    fn rebuild(&self, workspaces: &[WorkspaceInfo]) {
        // Every row the pointer could be on is about to be destroyed. GTK
        // hands the replacement widget an enter event even though the pointer
        // never moved, so hovering goes back to needing a real motion first.
        self.cancel_pending();
        self.hover_armed.set(false);
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut rows = Vec::with_capacity(workspaces.len());
        let me = self.me.borrow().clone();
        for ws in workspaces {
            let row = self.build_row(ws, &me);
            self.list.append(&row.root);
            rows.push(row);
        }
        if self.position == Position::Center {
            log::debug!(
                "centered strip of {} cards wants {} px",
                rows.len(),
                strip_width(rows.len(), self.thumb_width)
            );
        }
        *self.rows.borrow_mut() = rows;
    }

    fn build_row(&self, ws: &WorkspaceInfo, me: &Weak<Panel>) -> Row {
        let tw = self.thumb_width;
        let th = tw * 9 / 16;
        let card = self.position == Position::Center;

        let picture = gtk4::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk4::ContentFit::Cover);

        let hint = gtk4::Label::new(Some("no preview yet"));
        hint.add_css_class("hint");
        hint.set_halign(gtk4::Align::Center);
        hint.set_valign(gtk4::Align::Center);

        let name = gtk4::Label::new(Some(&ws.name));
        name.add_css_class("ws-name");
        name.set_halign(gtk4::Align::Start);
        name.set_valign(gtk4::Align::Start);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        name.set_max_width_chars(18);

        let thumb = gtk4::Overlay::new();
        thumb.add_css_class("thumb");
        thumb.set_overflow(gtk4::Overflow::Hidden);
        // A card is exactly thumbnail-wide, so Start and Center coincide; the
        // Center is insurance for the day a very long window title (or a very
        // large font) does widen a card after all.
        thumb.set_halign(if card {
            gtk4::Align::Center
        } else {
            gtk4::Align::Start
        });
        thumb.set_valign(gtk4::Align::Start);
        thumb.set_size_request(tw, th);
        thumb.set_child(Some(&picture));
        thumb.add_overlay(&hint);
        thumb.add_overlay(&name);

        // The window list: beside the thumbnail in a column row, under it in a
        // card. Same labels and classes either way, only the width budget and
        // the line cap differ.
        let mut titles = Vec::new();
        let text = window_list(ws, card, &mut titles);

        // A card stacks (thumbnail over titles) where a row lines up
        // (thumbnail beside titles); the class list keeps `.ws-row` so the
        // `.focused` / `.previewing` colours are one rule for both, with
        // `.ws-card` for what only the strip needs.
        let root = if card {
            let b = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            b.add_css_class("ws-card");
            // Pin the width so every card in the strip is the same size and
            // the thumbnails line up, whatever their titles are.
            b.set_size_request(card_width(tw), -1);
            b
        } else {
            gtk4::Box::new(gtk4::Orientation::Horizontal, 10)
        };
        root.add_css_class("ws-row");
        if self.is_marked_focused(ws) {
            root.add_css_class("focused");
        }
        if self.previewed.borrow().as_deref() == Some(ws.name.as_str())
            && self.origin.borrow().as_deref() != Some(ws.name.as_str())
        {
            root.add_css_class("previewing");
        }
        root.append(&thumb);
        root.append(&text);
        root.set_cursor_from_name(Some("pointer"));

        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let clicked = me.clone();
        let ws_name = ws.name.clone();
        let ws_num = ws.num;
        click.connect_released(move |_, _, _, _| {
            if let Some(p) = clicked.upgrade() {
                // Selecting a workspace: closes the panel by default, or (with
                // `close_on_select = false`) switches and leaves it up with
                // this row as the new origin. `on_row_clicked` decides.
                p.on_row_clicked(&ws_name, ws_num);
            }
        });
        root.add_controller(click);

        // --- hover preview ---------------------------------------------
        let motion = gtk4::EventControllerMotion::new();
        let ws_name = ws.name.clone();
        let entered = me.clone();
        motion.connect_enter(move |_, _, _| {
            if let Some(p) = entered.upgrade() {
                p.on_hover_enter(&ws_name, ws_num);
            }
        });
        let ws_name = ws.name.clone();
        let moved = me.clone();
        motion.connect_motion(move |_, _, _| {
            if let Some(p) = moved.upgrade() {
                p.on_hover_motion(&ws_name, ws_num);
            }
        });
        let left = me.clone();
        motion.connect_leave(move |_| {
            if let Some(p) = left.upgrade() {
                // Leaving only cancels a pending *hover* preview. A preview
                // that has already happened stands until the panel closes —
                // the pointer has to be able to get to the padding, the scrim
                // and the scrollbar without snapping back — and one the wheel
                // is waiting on is not the pointer's to cancel.
                p.cancel_hover_pending();
            }
        });
        root.add_controller(motion);

        Row {
            name: ws.name.clone(),
            num: ws.num,
            root,
            picture,
            hint,
            thumb,
            titles,
        }
    }
}

// ------------------------------------------------------------------ helpers --

/// The width of one card in the centered strip: its thumbnail plus the padding
/// and border around it, and nothing else. The window lines under the
/// thumbnail are capped and ellipsized so they can never widen it (see
/// `window_line`) — a strip of cards that were each as wide as their longest
/// window title would jump about every time a title changed.
fn card_width(thumb_width: i32) -> i32 {
    thumb_width + CARD_CHROME_WIDTH
}

/// What the whole strip asks for: `cards` cards, the gaps between them, and the
/// strip's own padding. Only the *wanted* width — GTK clamps it to the output
/// and scrolls the rest — so this is what tells us how many workspaces fit.
fn strip_width(cards: usize, thumb_width: i32) -> i32 {
    let n = cards as i32;
    let gaps = (n - 1).max(0);
    n * card_width(thumb_width) + gaps * CARD_SPACING + 2 * STRIP_PADDING
}

/// Where a scroll adjustment has to move so that the box `start .. start+extent`
/// is fully inside the page, centred in it; `None` when it already is, when
/// there is nothing to scroll, or when the box cannot fit anyway.
///
/// One function for both layouts: the column feeds it the row's top and height
/// against the vertical adjustment, the centered strip the card's left and
/// width against the horizontal one.
fn scroll_target(
    start: f64,
    extent: f64,
    value: f64,
    page: f64,
    lower: f64,
    upper: f64,
) -> Option<f64> {
    if page <= 0.0 || extent >= page {
        return None;
    }
    if start >= value && start + extent <= value + page {
        return None;
    }
    Some((start - (page - extent) / 2.0).clamp(lower, (upper - page).max(lower)))
}

/// The window list of one workspace: `card` picks the compact form that goes
/// under a thumbnail in the centered strip, otherwise the wide column that goes
/// beside one. Appends every title label to `titles`, in `ws.windows` order, so
/// `apply_flags` can move the `.win-focused` class without a rebuild.
fn window_list(ws: &WorkspaceInfo, card: bool, titles: &mut Vec<gtk4::Label>) -> gtk4::Box {
    let max_lines = if card { MAX_CARD_LINES } else { MAX_WINDOW_LINES };

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.add_css_class("wins");
    if card {
        text.add_css_class("card-wins");
        // No width request at all: the card's own request is the width budget,
        // and asking for more here is exactly how a card would grow past its
        // thumbnail.
        text.set_halign(gtk4::Align::Fill);
    } else {
        text.set_size_request(TEXT_COLUMN_WIDTH, -1);
    }
    text.set_hexpand(true);
    text.set_valign(gtk4::Align::Start);

    if ws.windows.is_empty() {
        let empty = gtk4::Label::new(Some("empty"));
        empty.add_css_class("dim");
        empty.set_xalign(0.0);
        empty.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        empty.set_max_width_chars(if card { 8 } else { -1 });
        text.append(&empty);
    } else {
        for w in ws.windows.iter().take(max_lines) {
            let (line, title) = window_line(w, card);
            text.append(&line);
            titles.push(title);
        }
        if ws.windows.len() > max_lines {
            let more = gtk4::Label::new(Some(&format!("+{} more", ws.windows.len() - max_lines)));
            more.add_css_class("dim");
            more.set_xalign(0.0);
            text.append(&more);
        }
    }
    text
}

/// One "title … app_id" line, and the title label (the one that carries
/// `.win-focused`).
///
/// Both labels ellipsize, and both have a `max-width-chars` cap — which in GTK
/// caps a label's *natural* width, not what it is given. That is the whole
/// trick behind a card that is never wider than its thumbnail: the line asks
/// for far less than the card is worth, the title takes whatever the card
/// actually has (`hexpand`), and the text that does not fit becomes an ellipsis
/// instead of pushing the card out. The caps are tighter in a card because the
/// budget there is the thumbnail's width, not a 260 px text column.
fn window_line(w: &WindowInfo, card: bool) -> (gtk4::Box, gtk4::Label) {
    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);

    let title = gtk4::Label::new(Some(if w.title.is_empty() {
        "(untitled)"
    } else {
        &w.title
    }));
    title.add_css_class("title");
    if w.focused {
        title.add_css_class("win-focused");
    }
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    title.set_max_width_chars(if card { 8 } else { 20 });
    line.append(&title);

    if let Some(id) = w.app_id.as_deref().filter(|s| !s.is_empty()) {
        let app = gtk4::Label::new(Some(id));
        app.add_css_class("appid");
        app.set_xalign(1.0);
        app.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        app.set_max_width_chars(if card { 8 } else { 12 });
        line.append(&app);
    }
    (line, title)
}

/// Fold an accumulated scroll delta into whole wheel steps, and return what is
/// left over to carry into the next event.
///
/// A mouse wheel arrives as ±1.0 per detent, so one detent is one row; a
/// touchpad arrives in fractions, which add up until they make a row. Rounding
/// towards zero (never past it) is what keeps a slow drag in one direction from
/// ever stepping the other way.
fn wheel_steps(accum: f64) -> (i32, f64) {
    if !accum.is_finite() {
        return (0, 0.0);
    }
    // A pathological delta must not turn into a pathological loop; the
    // selection is clamped to the list anyway.
    let whole = accum.trunc().clamp(-1000.0, 1000.0);
    (whole as i32, accum - whole)
}

/// Whether committing `target` has nowhere to go, so hiding the panel is the
/// whole of it.
///
/// True only when all three agree that nothing has moved since `show()`: the
/// commit names the origin, that is also what is being previewed (i.e. no
/// preview took sway anywhere else), and no preview is waiting out its
/// debounce. Anything else is a real switch — including a commit of the origin
/// *after* a preview, which is how the user comes back — and asking sway for it
/// costs nothing but is what makes coming back work.
fn commit_is_a_plain_hide(
    target: Option<&str>,
    previewed: Option<&str>,
    origin: Option<&str>,
    pending: bool,
) -> bool {
    !pending && target.is_some() && target == previewed && target == origin
}

/// Move a selection `steps` rows through a list of `len` rows. Clamped at both
/// ends: the wheel stops at the first and last workspace instead of wrapping,
/// so spinning it never takes the user somewhere they were not aiming for.
fn clamp_step(from: usize, steps: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len as i64 - 1;
    (from as i64 + steps as i64).clamp(0, last) as usize
}

fn apply_thumb(row: &Row, thumb: &Thumbnail, thumb_width: i32) {
    let (w, h) = (thumb.width, thumb.height);
    let needed = (w as usize) * (h as usize) * 4;
    if w == 0 || h == 0 || thumb.rgba.len() < needed {
        log::warn!(
            "thumbnail for {:?} is {}x{} but only {} bytes; ignoring",
            row.name,
            w,
            h,
            thumb.rgba.len()
        );
        clear_thumb(row, thumb_width);
        return;
    }
    // `Thumbnail` is straight (non-premultiplied) RGBA8, stride = width * 4.
    let bytes = glib::Bytes::from_owned(thumb.rgba.clone());
    let texture = gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        (w as usize) * 4,
    );
    row.picture.set_paintable(Some(&texture));
    row.hint.set_visible(false);
    // Keep the row height tied to the real aspect ratio once we know it.
    let height = ((thumb_width as i64) * (h as i64) / (w as i64)).max(1) as i32;
    row.thumb.set_size_request(thumb_width, height);
}

fn clear_thumb(row: &Row, thumb_width: i32) {
    row.picture.set_paintable(gdk::Paintable::NONE);
    row.hint.set_visible(true);
    row.thumb.set_size_request(thumb_width, thumb_width * 9 / 16);
}

/// The `gdk::Monitor` whose connector name is `output`, if the display knows one.
fn monitor_for(output: &str) -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let m = monitors.item(i)?.downcast::<gdk::Monitor>().ok()?;
        if m.connector().is_some_and(|c| c == output) {
            return Some(m);
        }
    }
    None
}

/// Build the stylesheet from the configured colors and install it on the
/// default display at APPLICATION priority.
fn install_css(colors: &Colors) {
    let css = stylesheet(colors);
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    } else {
        log::warn!("no default GDK display; svitek CSS not installed");
    }
}

fn stylesheet(colors: &Colors) -> String {
    let bg = css_color(&colors.background, "#1e1e2ecc");
    let fg = css_color(&colors.foreground, "#cdd6f4");
    let dim = css_color(&colors.dim, "#a6adc8");
    let focused = css_color(&colors.focused, "#89b4fa");
    format!(
        "\
window.svitek {{ background-color: {SCRIM_BG}; color: {fg}; }}
window.svitek scrolledwindow,
window.svitek viewport {{ background: none; background-color: transparent; }}
window.svitek scrolledwindow.panel {{ background-color: {bg}; }}
window.svitek .ws-list {{ padding: {STRIP_PADDING}px; }}
window.svitek .ws-strip {{ padding: {STRIP_PADDING}px; }}
window.svitek .ws-row {{
  padding: {ROW_PADDING}px;
  border: {ROW_BORDER}px solid transparent;
  border-radius: 10px;
  background-color: transparent;
}}
window.svitek .ws-row:hover {{ background-color: alpha({fg}, 0.10); }}
window.svitek .ws-row.focused {{
  border-color: {focused};
  background-color: alpha({focused}, 0.14);
}}
window.svitek .ws-row.previewing {{
  border-color: alpha({focused}, 0.6);
  background-color: alpha({focused}, 0.07);
}}
window.svitek .ws-card {{ padding: {ROW_PADDING}px; }}
window.svitek .thumb {{
  border-radius: 6px;
  background-color: alpha({dim}, 0.13);
}}
window.svitek .ws-name {{
  margin: 4px;
  padding: 1px 6px;
  border-radius: 4px;
  font-size: 88%;
  font-weight: bold;
  color: {fg};
  background-color: alpha(#000000, 0.55);
}}
window.svitek .hint {{ font-size: 85%; color: {dim}; }}
window.svitek .wins {{ margin-top: 1px; }}
window.svitek .card-wins {{ margin-top: 3px; }}
window.svitek .title {{ font-size: 95%; color: {fg}; }}
window.svitek .win-focused {{ font-weight: bold; color: {focused}; }}
window.svitek .appid {{ font-size: 82%; color: {dim}; }}
window.svitek .dim {{ font-size: 90%; color: {dim}; font-style: italic; }}
window.svitek scrollbar {{ background: none; }}
"
    )
}

/// Colors come from a user-editable TOML file; reject anything that is not a
/// plain `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa` literal or a bare CSS name so a
/// typo cannot inject arbitrary CSS into the generated stylesheet.
fn css_color(value: &str, fallback: &str) -> String {
    let v = value.trim();
    let ok = if let Some(hex) = v.strip_prefix('#') {
        matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
    } else {
        !v.is_empty()
            && v.len() <= 32
            && v.bytes().all(|b| b.is_ascii_alphabetic())
    };
    if ok {
        v.to_string()
    } else {
        log::warn!("invalid color {value:?} in config; using {fallback}");
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_validated() {
        assert_eq!(css_color("#1e1e2ecc", "#000"), "#1e1e2ecc");
        assert_eq!(css_color("#fff", "#000"), "#fff");
        assert_eq!(css_color("  #89b4fa ", "#000"), "#89b4fa");
        assert_eq!(css_color("red", "#000"), "red");
        // Anything that could break out of the declaration is rejected.
        assert_eq!(css_color("#fff; } * { color: red", "#000"), "#000");
        assert_eq!(css_color("", "#000"), "#000");
        assert_eq!(css_color("#12345", "#000"), "#000");
    }

    #[test]
    fn stylesheet_mentions_every_configured_color() {
        let c = Colors::default();
        let css = stylesheet(&c);
        for color in [&c.background, &c.foreground, &c.dim, &c.focused] {
            assert!(css.contains(color.as_str()), "missing {color} in {css}");
        }
    }

    #[test]
    fn the_frame_carries_the_background_and_the_scrim_is_invisible() {
        let c = Colors::default();
        let css = stylesheet(&c);
        // The window is the whole output now, so the configured background
        // belongs to the frame; the rest of the surface must stay invisible.
        assert!(css.contains(&format!("window.svitek {{ background-color: {SCRIM_BG};")));
        assert!(css.contains(&format!(
            "scrolledwindow.panel {{ background-color: {};",
            c.background
        )));
    }

    #[test]
    fn the_previewed_row_is_marked_apart_from_the_focused_one() {
        let css = stylesheet(&Colors::default());
        assert!(css.contains(".ws-row.previewing"));
        // Same hue, weaker: the focused row is where you came back to, the
        // previewed one is where you are looking.
        assert!(css.contains(&format!(
            "border-color: alpha({}, 0.6)",
            Colors::default().focused
        )));
    }

    #[test]
    fn a_mouse_wheel_detent_is_exactly_one_row() {
        assert_eq!(wheel_steps(1.0), (1, 0.0));
        assert_eq!(wheel_steps(-1.0), (-1, 0.0));
        assert_eq!(wheel_steps(0.0), (0, 0.0));
    }

    #[test]
    fn smooth_deltas_accumulate_into_steps() {
        // A touchpad: nothing happens until the fractions make a whole row,
        // and the remainder is carried, not thrown away.
        let mut accum = 0.0;
        let mut fired = 0;
        for _ in 0..7 {
            let (steps, rest) = wheel_steps(accum + 0.4);
            accum = rest;
            fired += steps;
        }
        assert_eq!(fired, 2, "7 × 0.4 = 2.8 is two rows, with 0.8 carried");
        assert!((accum - 0.8).abs() < 1e-9, "carried {accum}");
    }

    #[test]
    fn a_partial_delta_never_steps_the_wrong_way() {
        // Rounding must be towards zero: -0.6 is not yet a row up.
        assert_eq!(wheel_steps(-0.6).0, 0);
        assert_eq!(wheel_steps(0.9).0, 0);
        // …and several fast detents in one event are several rows.
        assert_eq!(wheel_steps(4.0).0, 4);
        assert_eq!(wheel_steps(-3.5), (-3, -0.5));
        // Nonsense in, nothing out.
        assert_eq!(wheel_steps(f64::NAN), (0, 0.0));
        assert_eq!(wheel_steps(f64::INFINITY), (0, 0.0));
    }

    #[test]
    fn the_selection_clamps_at_both_ends_and_never_wraps() {
        // 3 rows, selection on the first.
        assert_eq!(clamp_step(0, 1, 3), 1);
        assert_eq!(clamp_step(0, 2, 3), 2);
        // Past the end stops at the end — 1 + 4 rows down is the last row.
        assert_eq!(clamp_step(0, 4, 3), 2);
        assert_eq!(clamp_step(2, 5, 3), 2);
        // And past the start stops at the start, rather than wrapping round.
        assert_eq!(clamp_step(2, -2, 3), 0);
        assert_eq!(clamp_step(0, -1, 3), 0);
        assert_eq!(clamp_step(1, 0, 3), 1);
        // An empty list has no selection to move.
        assert_eq!(clamp_step(0, 3, 0), 0);
        // No overflow on absurd input.
        assert_eq!(clamp_step(0, i32::MAX, 3), 2);
        assert_eq!(clamp_step(2, i32::MIN, 3), 0);
    }

    #[test]
    fn committing_where_you_already_are_is_just_a_hide() {
        // The panel opened on "1", nothing hovered, nothing scrolled: Enter (or
        // a click on row 1) has nowhere to switch to.
        assert!(commit_is_a_plain_hide(Some("1"), Some("1"), Some("1"), false));
        // Origin "1", a preview took sway to "2": committing "2" is real…
        assert!(!commit_is_a_plain_hide(Some("2"), Some("2"), Some("1"), false));
        // …and so is coming back to "1" — that switch is how you come back.
        assert!(!commit_is_a_plain_hide(Some("1"), Some("2"), Some("1"), false));
        // A preview still waiting out its debounce means sway has not been
        // asked yet, so even a commit of the origin has to ask.
        assert!(!commit_is_a_plain_hide(Some("1"), Some("1"), Some("1"), true));
        // Nothing selected at all (no rows, no origin): nothing to switch to,
        // but `commit` logs that case separately — the rule must not claim it.
        assert!(!commit_is_a_plain_hide(None, None, None, false));
    }

    #[test]
    fn panel_width_leaves_room_for_the_text_column() {
        let cfg = Config::default();
        let w = cfg.thumbnail_width as i32 + TEXT_COLUMN_WIDTH + CHROME_WIDTH;
        assert!(w > cfg.thumbnail_width as i32 + TEXT_COLUMN_WIDTH);
        assert_eq!(w, 240 + 260 + 46);
    }

    #[test]
    fn the_centered_strip_has_its_own_selectors_without_touching_the_row_colours() {
        let c = Colors::default();
        let css = stylesheet(&c);
        assert!(css.contains(".ws-strip"), "the strip needs its own padding");
        assert!(css.contains(".ws-card"), "cards need their own rule");
        assert!(css.contains(".card-wins"));
        // A card is a `.ws-row` too, so focus and preview are one rule for both
        // layouts — changing either would change the column as well.
        assert!(css.contains(&format!(
            ".ws-row.focused {{\n  border-color: {};",
            c.focused
        )));
        assert!(css.contains(".ws-row.previewing"));
        // `card_width` counts the padding the stylesheet actually sets.
        assert!(css.contains(&format!(".ws-card {{ padding: {ROW_PADDING}px; }}")));
    }

    #[test]
    fn a_card_is_exactly_as_wide_as_its_thumbnail_plus_its_chrome() {
        assert_eq!(card_width(240), 240 + 20);
        assert_eq!(card_width(80), 100);
        // …and the strip is the cards, the gaps between them, and its padding.
        assert_eq!(strip_width(1, 240), 260 + 16);
        assert_eq!(strip_width(3, 240), 3 * 260 + 2 * 10 + 16);
        // Nine cards want more than a 1280 px output has: that is the case the
        // horizontal scrollbar exists for.
        assert!(strip_width(9, 240) > 1280);
        assert!(strip_width(4, 240) < 1280);
        // No gap to count when there is nothing to gap.
        assert_eq!(strip_width(0, 240), 16);
    }

    #[test]
    fn nothing_scrolls_while_the_selection_is_already_in_the_page() {
        // A 100-long box at 0 in a 500-long page that starts at 0: visible.
        assert_eq!(scroll_target(0.0, 100.0, 0.0, 500.0, 0.0, 900.0), None);
        assert_eq!(scroll_target(400.0, 100.0, 0.0, 500.0, 0.0, 900.0), None);
        // Everything fits (page >= content): there is nothing to scroll.
        assert_eq!(scroll_target(0.0, 500.0, 0.0, 500.0, 0.0, 500.0), None);
        // A page of zero (not laid out yet) is not something to compute with.
        assert_eq!(scroll_target(0.0, 10.0, 0.0, 0.0, 0.0, 0.0), None);
    }

    #[test]
    fn an_off_screen_selection_is_centred_in_the_page_and_clamped_to_the_ends() {
        // Below/right of the page: centred, 700 - (500-100)/2 = 500, with
        // enough content behind it (upper 1200) for that to be reachable.
        assert_eq!(
            scroll_target(700.0, 100.0, 0.0, 500.0, 0.0, 1200.0),
            Some(500.0)
        );
        // The last box cannot be centred past the end of the content.
        assert_eq!(
            scroll_target(800.0, 100.0, 0.0, 500.0, 0.0, 900.0),
            Some(400.0)
        );
        // Nor the first one before its start.
        assert_eq!(
            scroll_target(0.0, 100.0, 300.0, 500.0, 0.0, 900.0),
            Some(0.0)
        );
    }
}
