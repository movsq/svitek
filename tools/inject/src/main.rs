//! `inject` — a tiny input injector for the headless test sway.
//!
//! The headless wlroots backend has no input devices at all, so `swaymsg seat -
//! cursor …` does nothing and there is no way to click or type. This binary
//! creates *virtual* devices through `wlr-virtual-pointer-unstable-v1` and
//! `virtual-keyboard-unstable-v1` (both implemented by sway) and drives them.
//!
//! It is a TEST TOOL, not part of svitek: its own crate, its own Cargo.toml,
//! not a member of the svitek package, so `cargo build` / `cargo test` in the
//! repo root never build it and the main crate keeps its dependency list.
//!
//! # Build
//!
//! ```sh
//! CARGO_TARGET_DIR=$PWD/target \
//!   cargo build --release --manifest-path tools/inject/Cargo.toml
//! # binary: target/release/inject
//! ```
//!
//! # Usage
//!
//! Run it with `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` pointing at the *nested*
//! sway (never the real session).
//!
//! ```text
//! inject pointer <output> <x> <y> [click]
//!     Create a virtual pointer bound to <output> (falls back to the whole
//!     layout if the connector is unknown), move it to <x>,<y> — coordinates
//!     are output-local, in a 1280x720 extent, matching tests/headless-sway.sh
//!     — and optionally press+release BTN_LEFT there. The motion is repeated a
//!     few times because the first event can arrive before the client has
//!     created its wl_pointer, and the process lingers ~600 ms so the click is
//!     delivered before the seat loses the device.
//!
//! inject scroll <output> <x> <y> <steps>
//!     Same virtual pointer, moved to <x>,<y> first (so the wheel lands on
//!     whatever is under that point — a row, the panel padding, or the scrim),
//!     then |steps| mouse-wheel detents ~60 ms apart on the vertical axis.
//!     POSITIVE = wheel down / scroll away, negative = wheel up. Each detent is
//!     `axis_source(wheel)` + `axis_discrete(time, vertical_scroll, 15.0, ±1)`
//!     + `frame` — the 15.0 is the value libinput reports for one detent, and
//!     the discrete count is what makes wlroots forward it as a real wheel
//!     click (value120) rather than a smooth scroll.
//!
//! inject key <evdev-code> [delay_ms] [xkb_mod_mask]
//!     Create a virtual keyboard (keymap compiled with `xkbcli compile-keymap
//!     --layout us`, so libxkbcommon-tools must be installed), wait `delay_ms`,
//!     then press+release the evdev key <evdev-code>. `delay_ms` exists so the
//!     seat already advertises a keyboard *before* the surface under test is
//!     mapped: start `inject key 1 2000 &`, show the panel, and the Escape
//!     lands on the mapped panel. `xkb_mod_mask` is held around the key
//!     (Mod4/Super = 64), the way wtype does it.
//!
//!     Useful evdev codes: 1 = Escape, 15 = Tab, 28 = Return, 30 = A,
//!     125 = Left Super/Meta, 2..11 = 1..0.
//!
//! inject hold <mod-evdev-code|0> <xkb_mod_mask> <setup_ms> [action ...]
//!     The alt-tab gesture: create the virtual keyboard, wait `setup_ms` (so
//!     the seat advertises a keyboard before anything is shown), press and HOLD
//!     the modifier, run the actions in order, then release it and linger
//!     ~600 ms so the release is delivered. `0 0` for the modifier holds
//!     nothing at all, which is how the "modifier was already up" race is
//!     tested: a keyboard exists on the seat (so a layer surface can take
//!     keyboard focus) but no modifier is down.
//!
//!     BOTH halves are needed, and this is the whole reason this subcommand
//!     exists. Measured on sway 1.12 / wlroots: a virtual keyboard's *key*
//!     events do not move sway's own modifier state, so pressing evdev 125
//!     (Super_L) alone makes `bindsym Mod4+Tab` fire exactly zero times; the
//!     explicit `modifiers` request (Mod4 = 64) is what sway matches bindings
//!     against. But `modifiers` alone sends no *key* event, so a client
//!     watching for the release of the Super key would never see one. So we
//!     send the mask AND the real key, and undo them in the opposite order.
//!
//!     Actions:
//!       key:<code>    press+release that evdev key (while the modifier is down)
//!       sleep:<ms>    wait
//!       run:<command> run `sh -c <command>` and wait for it
//! ```
//!
//! Every subcommand prints `inject: …` progress lines on success and exits 0.
//! A missing or unparsable argument prints the usage text and exits 2; a
//! missing Wayland protocol or a compositor that will not talk panics.

use std::os::fd::AsFd;

use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};

const PRESSED: u32 = 1;
const RELEASED: u32 = 0;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "wl_seat" => {
                    st.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                "wl_output" => {
                    let o: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    st.outputs.push((o, None));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    st.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    st.keyboard_mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        st: &mut Self,
        out: &wl_output::WlOutput,
        ev: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = ev {
            for (o, n) in st.outputs.iter_mut() {
                if o == out {
                    *n = Some(name.clone());
                }
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32
}

/// The one place the command line is described; every bad-argument path ends
/// here, so a typo gets the usage text and exit 2 rather than an index panic.
fn usage() -> ! {
    eprintln!(
        "usage: inject pointer <output> <x> <y> [click]\n\
         \x20      inject scroll <output> <x> <y> <steps>   (+ = wheel down)\n\
         \x20      inject key <evdev-code> [delay_ms] [xkb_mod_mask]\n\
         \x20      inject hold <mod-evdev-code|0> <xkb_mod_mask> <setup_ms>\n\
         \x20          [key:<c>|sleep:<ms>|run:<cmd>]..."
    );
    std::process::exit(2);
}

/// Parse a positional argument, or explain which one is wrong and print usage.
fn num<T: std::str::FromStr>(s: &str, what: &str) -> T {
    s.parse().unwrap_or_else(|_| {
        eprintln!("inject: <{what}> must be a number, got {s:?}");
        usage()
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Check the shape of the command line before touching Wayland: a missing
    // argument should say so, not panic somewhere deep in a subcommand.
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or_default();
    let need = match mode {
        "pointer" => 5, // prog pointer <output> <x> <y>
        "scroll" => 6,  // prog scroll <output> <x> <y> <steps>
        "key" => 3,     // prog key <evdev-code>
        "hold" => 5,    // prog hold <mod-code> <mask> <setup_ms>
        "" => {
            eprintln!("inject: no subcommand");
            usage()
        }
        other => {
            eprintln!("inject: unknown subcommand {other:?}");
            usage()
        }
    };
    if args.len() < need {
        eprintln!(
            "inject: {mode} needs {} positional argument(s), got {}",
            need - 2,
            args.len().saturating_sub(2)
        );
        usage();
    }

    let conn = Connection::connect_to_env().expect("connect");
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    let _reg = conn.display().get_registry(&qh, ());
    let mut st = State::default();
    queue.roundtrip(&mut st).unwrap();
    queue.roundtrip(&mut st).unwrap(); // output names

    let seat = st.seat.clone().expect("no wl_seat");

    match mode {
        "pointer" | "scroll" => {
            let scrolling = mode == "scroll";
            let target = args[2].clone();
            let x: u32 = num(&args[3], "x");
            let y: u32 = num(&args[4], "y");
            let click = !scrolling && args.get(5).map(|s| s == "click").unwrap_or(false);
            // `scroll` takes the number of detents where `pointer` takes
            // `click`; positive is wheel down / scroll away.
            let steps: i32 = if scrolling { num(&args[5], "steps") } else { 0 };
            let mgr = st.pointer_mgr.clone().expect("no virtual pointer manager");
            let out = st
                .outputs
                .iter()
                .find(|(_, n)| n.as_deref() == Some(target.as_str()))
                .map(|(o, _)| o.clone());
            let ptr = match &out {
                Some(o) => mgr.create_virtual_pointer_with_output(Some(&seat), Some(o), &qh, ()),
                None => {
                    eprintln!("output {target} not found; using layout-wide extents");
                    mgr.create_virtual_pointer(Some(&seat), &qh, ())
                }
            };
            // Give sway AND the client's GDK a moment to notice the new pointer
            // device (the headless seat starts with no pointer capability at all).
            queue.roundtrip(&mut st).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1200));

            // Several motions: the first one may be delivered before the client
            // has created its wl_pointer, in which case it misses the enter.
            for i in 0..6 {
                ptr.motion_absolute(now(), x + (i % 2), y, 1280, 720);
                ptr.frame();
                conn.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
            }

            if click {
                const BTN_LEFT: u32 = 0x110;
                ptr.button(
                    now(),
                    BTN_LEFT,
                    wayland_client::protocol::wl_pointer::ButtonState::Pressed,
                );
                ptr.frame();
                conn.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(120));
                ptr.button(
                    now(),
                    BTN_LEFT,
                    wayland_client::protocol::wl_pointer::ButtonState::Released,
                );
                ptr.frame();
                conn.flush().unwrap();
            }

            if scrolling {
                use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
                // One detent per iteration: source, then the discrete step (the
                // 15.0 is what libinput reports for one wheel click), then the
                // frame that makes the set one event. Spaced out so the client
                // sees them as separate detents rather than one burst.
                let dir = if steps >= 0 { 1 } else { -1 };
                for _ in 0..steps.abs() {
                    ptr.axis_source(AxisSource::Wheel);
                    ptr.axis_discrete(now(), Axis::VerticalScroll, 15.0 * dir as f64, dir);
                    ptr.frame();
                    conn.flush().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(600));
            if scrolling {
                println!("inject: {steps} wheel step(s) on {target} at {x},{y}");
            } else {
                println!("inject: pointer done on {target} at {x},{y} click={click}");
            }
        }
        "key" => {
            let code: u32 = num(&args[2], "evdev-code");
            // The keyboard is created now but the key only pressed after
            // `delay_ms`, so the seat already has keyboard capability (and sway
            // has settled its keyboard focus) before the panel is shown.
            let delay_ms: u64 = args.get(3).map(|s| num(s, "delay_ms")).unwrap_or(0);
            use std::io::Write;
            let kb = keyboard(&mut st, &seat, &qh, &conn, &mut queue);
            println!("inject: virtual keyboard ready; waiting {delay_ms} ms");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));

            // args[4] (optional) = xkb modifier mask to hold while `code` is
            // pressed (Mod4/Super = 1 << 6 = 64), the way wtype does it.
            let mods: u32 = args.get(4).map(|s| num(s, "xkb_mod_mask")).unwrap_or(0);
            if mods != 0 {
                kb.modifiers(mods, 0, 0, 0);
                conn.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            kb.key(now(), code, PRESSED);
            conn.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(60));
            kb.key(now(), code, RELEASED);
            if mods != 0 {
                conn.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(40));
                kb.modifiers(0, 0, 0, 0);
            }
            conn.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
            println!("inject: key {code} sent");
        }
        "hold" => {
            let mod_code: u32 = num(&args[2], "mod-evdev-code");
            let mod_mask: u32 = num(&args[3], "xkb_mod_mask");
            let setup_ms: u64 = num(&args[4], "setup_ms");
            use std::io::Write;
            let kb = keyboard(&mut st, &seat, &qh, &conn, &mut queue);
            println!("inject: virtual keyboard ready; waiting {setup_ms} ms");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(setup_ms));

            if mod_mask != 0 {
                kb.modifiers(mod_mask, 0, 0, 0);
                conn.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            if mod_code != 0 {
                kb.key(now(), mod_code, PRESSED);
                conn.flush().unwrap();
                // Give sway time to tell the focused surface about it before
                // anything else happens.
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            println!("inject: modifier {mod_code} (mask {mod_mask}) down");
            std::io::stdout().flush().unwrap();

            for action in args.iter().skip(5) {
                let (kind, rest) = action.split_once(':').unwrap_or((action.as_str(), ""));
                match kind {
                    "key" => {
                        let code: u32 = num(rest, "key:<evdev-code>");
                        kb.key(now(), code, PRESSED);
                        conn.flush().unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(60));
                        kb.key(now(), code, RELEASED);
                        conn.flush().unwrap();
                        println!("inject: key {code} tapped");
                    }
                    "sleep" => {
                        let ms: u64 = num(rest, "sleep:<ms>");
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                    }
                    "run" => {
                        let status = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(rest)
                            .status()
                            .expect("sh");
                        println!("inject: ran {rest:?} -> {status}");
                    }
                    _ => {
                        eprintln!("inject: unknown action {action:?}");
                        usage();
                    }
                }
                std::io::stdout().flush().unwrap();
            }

            if mod_code != 0 {
                kb.key(now(), mod_code, RELEASED);
                conn.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            if mod_mask != 0 {
                kb.modifiers(0, 0, 0, 0);
                conn.flush().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(600));
            println!("inject: modifier {mod_code} up");
        }
        _ => usage(), // already rejected above
    }
}

/// Create the virtual keyboard and give it a real xkb keymap — required before
/// any key event, and the reason the key codes below are evdev codes on a `us`
/// layout. Returns once sway has had a moment to notice the new device: the
/// seat starts with no keyboard capability at all on the headless backend.
fn keyboard(
    st: &mut State,
    seat: &wl_seat::WlSeat,
    qh: &QueueHandle<State>,
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
) -> ZwpVirtualKeyboardV1 {
    let mgr = st
        .keyboard_mgr
        .clone()
        .expect("no virtual keyboard manager");
    let kb = mgr.create_virtual_keyboard(seat, qh, ());

    let keymap = std::process::Command::new("xkbcli")
        .args(["compile-keymap", "--layout", "us"])
        .output()
        .unwrap_or_else(|e| {
            eprintln!("inject: cannot run xkbcli (install libxkbcommon-tools): {e}");
            std::process::exit(2);
        });
    if !keymap.status.success() {
        eprintln!(
            "inject: xkbcli compile-keymap --layout us failed ({})",
            keymap.status
        );
        eprint!("{}", String::from_utf8_lossy(&keymap.stderr));
        std::process::exit(2);
    }
    let file = keymap_memfd(&keymap.stdout);
    kb.keymap(1, file.as_fd(), (keymap.stdout.len() + 1) as u32);
    conn.flush().unwrap();
    queue.roundtrip(st).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    kb
}

/// The keymap has to reach sway as a file descriptor it can mmap. An anonymous
/// memfd is the whole of it: nothing is ever created in the filesystem, so
/// there is no name to collide, no symlink to follow and nothing to clean up.
/// (The main crate does the same for its screencopy buffers.)
fn keymap_memfd(keymap: &[u8]) -> std::fs::File {
    use std::io::{Seek, Write};
    use std::os::fd::FromRawFd;

    let name = c"svitek-inject-keymap";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        eprintln!("inject: memfd_create: {}", std::io::Error::last_os_error());
        std::process::exit(2);
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    // NUL-terminated: the keymap format wants the trailing byte, and the size
    // handed to `keymap` below counts it.
    f.write_all(keymap).unwrap();
    f.write_all(&[0]).unwrap();
    f.flush().unwrap();
    f.rewind().unwrap();
    f
}
