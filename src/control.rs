//! The control socket: how `svitek toggle` reaches the resident instance.
//!
//! Unix stream socket at `$XDG_RUNTIME_DIR/svitek.sock` (fallback
//! `/tmp/svitek-<uid>.sock`). Protocol: client sends one line
//! (`toggle` | `show` | `hide` | `next` | `prev` | `quit`), server replies
//! `ok\n` (or `err ...\n`) and closes. A stale socket file (nobody listening)
//! is removed on bind.
//!
//! `$SVITEK_SOCKET` overrides the path entirely (used by the tests and to run
//! several instances against several compositors).

use crate::model::{Command, Msg};
use async_channel::Sender;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A client must send its line within this long, or it is dropped; the accept
/// loop handles one connection at a time and must not be hostage to a peer.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// The daemon always answers immediately, so the client can be impatient too.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
/// A command line is a handful of bytes; refuse to buffer more than this.
const MAX_LINE: u64 = 1024;

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SVITEK_SOCKET") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("svitek.sock");
        }
    }
    PathBuf::from(format!("/tmp/svitek-{}.sock", unsafe { libc::getuid() }))
}

/// Bind the socket and spawn the accept thread. Fails if another instance is
/// alive on the socket (so the daemon never runs twice).
pub fn spawn(tx: Sender<Msg>) -> Result<std::thread::JoinHandle<()>, String> {
    spawn_at(&socket_path(), tx)
}

/// Remove the socket file (called on exit).
pub fn cleanup() {
    cleanup_at(&socket_path());
}

/// Client side: send `cmd` to the running instance. Errors if none is running.
pub fn send(cmd: Command) -> Result<(), String> {
    send_at(&socket_path(), cmd)
}

// ---------------------------------------------------------------------------
// path-explicit forms: the public API is these with `socket_path()` filled in.
// Tests use them directly so they never have to touch the environment.
// ---------------------------------------------------------------------------

fn spawn_at(path: &Path, tx: Sender<Msg>) -> Result<std::thread::JoinHandle<()>, String> {
    if path.exists() {
        match UnixStream::connect(path) {
            // Somebody answered: a live instance owns this socket.
            Ok(_) => return Err("svitek is already running".to_string()),
            // Nobody listening (or the file vanished under us): stale, drop it.
            Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
                log::info!("removing stale control socket {}", path.display());
                if let Err(e) = std::fs::remove_file(path) {
                    if e.kind() != ErrorKind::NotFound {
                        return Err(format!(
                            "cannot remove stale control socket {}: {e}",
                            path.display()
                        ));
                    }
                }
            }
            Err(e) => {
                return Err(format!(
                    "control socket {} exists but cannot be probed: {e}",
                    path.display()
                ))
            }
        }
    }

    let listener = UnixListener::bind(path)
        .map_err(|e| format!("cannot bind control socket {}: {e}", path.display()))?;
    log::debug!("listening on {}", path.display());

    let handle = std::thread::Builder::new()
        .name("svitek-control".to_string())
        .spawn(move || accept_loop(listener, tx))
        .map_err(|e| format!("cannot spawn control thread: {e}"))?;
    Ok(handle)
}

fn cleanup_at(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => log::debug!("removed control socket {}", path.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => log::warn!("cannot remove control socket {}: {e}", path.display()),
    }
}

fn send_at(path: &Path, cmd: Command) -> Result<(), String> {
    let mut stream = UnixStream::connect(path).map_err(|e| not_running(path, &e.to_string()))?;
    let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));

    stream
        .write_all(format!("{}\n", cmd.as_str()).as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| not_running(path, &e.to_string()))?;

    let mut reply = String::new();
    BufReader::new(&stream)
        .take(MAX_LINE)
        .read_line(&mut reply)
        .map_err(|e| format!("no reply from svitek on {}: {e}", path.display()))?;

    match reply.trim() {
        "ok" => Ok(()),
        "" => Err(not_running(path, "the running instance closed the connection")),
        other => Err(format!(
            "svitek refused `{}`: {}",
            cmd.as_str(),
            other.strip_prefix("err ").unwrap_or(other)
        )),
    }
}

fn not_running(path: &Path, why: &str) -> String {
    format!(
        "svitek is not running ({}: {why}) — is `exec svitek` in your sway config?",
        path.display()
    )
}

fn accept_loop(listener: UnixListener, tx: Sender<Msg>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if !serve(stream, &tx) {
                    log::debug!("control channel closed, control thread exiting");
                    return;
                }
            }
            Err(e) => {
                log::warn!("control socket accept failed: {e}");
                // EMFILE and friends would spin this loop; a transient error is
                // rare enough that giving the fd table a moment is enough.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Serve one connection. Returns false when the main loop is gone (shutdown).
fn serve(stream: UnixStream, tx: &Sender<Msg>) -> bool {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));

    let mut line = String::new();
    let read = BufReader::new(&stream).take(MAX_LINE).read_line(&mut line);
    let mut stream = stream;

    match read {
        Ok(0) => {
            log::debug!("control: empty connection");
            true
        }
        Ok(_) => match Command::parse(&line) {
            Some(cmd) => {
                log::debug!("control: {}", cmd.as_str());
                // Queue first, answer second: `ok` has to mean "accepted", so
                // that a client which has returned knows the daemon will act.
                // Blocking send on an unbounded channel never actually blocks;
                // it only fails once the receiver is gone (we are shutting down).
                match tx.send_blocking(Msg::Control(cmd)) {
                    Ok(()) => {
                        let _ = stream.write_all(b"ok\n");
                        let _ = stream.flush();
                        true
                    }
                    Err(_) => {
                        let _ = stream.write_all(b"err svitek is shutting down\n");
                        let _ = stream.flush();
                        false
                    }
                }
            }
            None => {
                log::warn!("control: unknown command {:?}", line.trim());
                let _ = stream.write_all(b"err unknown command\n");
                let _ = stream.flush();
                true
            }
        },
        Err(e) => {
            log::warn!("control: dropping connection: {e}");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Msg;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique socket path per test; no environment fiddling, so tests stay
    /// safe to run in parallel.
    fn temp_socket(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "svitek-test-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("svitek.sock")
    }

    #[test]
    fn round_trip() {
        let path = temp_socket("round-trip");
        let (tx, rx) = async_channel::unbounded();
        let _h = spawn_at(&path, tx).expect("bind");

        send_at(&path, Command::Toggle).expect("toggle accepted");
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            Msg::Control(Command::Toggle)
        ));

        send_at(&path, Command::Quit).expect("quit accepted");
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            Msg::Control(Command::Quit)
        ));

        cleanup_at(&path);
        assert!(!path.exists());
    }

    /// `ok` means the command is already queued: the moment `send_at` returns,
    /// the message must be readable without waiting for anything.
    #[test]
    fn ok_means_the_command_is_already_queued() {
        let path = temp_socket("queued-first");
        let (tx, rx) = async_channel::unbounded();
        let _h = spawn_at(&path, tx).expect("bind");

        for _ in 0..20 {
            send_at(&path, Command::Toggle).expect("toggle accepted");
            match rx.try_recv() {
                Ok(Msg::Control(Command::Toggle)) => {}
                other => panic!("reply arrived before the command was queued: {other:?}"),
            }
        }
        cleanup_at(&path);
    }

    /// With the main loop gone there is nothing to queue to, and the client is
    /// told so instead of being promised an action nobody will take.
    #[test]
    fn a_closed_main_loop_is_reported_as_an_error() {
        let path = temp_socket("closed-loop");
        let (tx, rx) = async_channel::unbounded::<Msg>();
        let _h = spawn_at(&path, tx).expect("bind");
        drop(rx);

        let err = send_at(&path, Command::Toggle).unwrap_err();
        assert!(err.contains("shutting down"), "{err}");
        cleanup_at(&path);
    }

    #[test]
    fn send_without_daemon_explains_itself() {
        let path = temp_socket("no-daemon");
        let err = send_at(&path, Command::Toggle).unwrap_err();
        assert!(err.contains("not running"), "{err}");
        assert!(err.contains("exec svitek"), "{err}");
    }

    #[test]
    fn stale_socket_is_recovered() {
        let path = temp_socket("stale");
        // A socket file nobody listens on: bind, then drop the listener.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());

        let (tx, rx) = async_channel::unbounded();
        let _h = spawn_at(&path, tx).expect("stale socket should be replaced");
        send_at(&path, Command::Show).expect("show accepted");
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            Msg::Control(Command::Show)
        ));
        cleanup_at(&path);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let path = temp_socket("unknown");
        let (tx, rx) = async_channel::unbounded();
        let _h = spawn_at(&path, tx).expect("bind");

        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"explode\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        assert_eq!(reply, "err unknown command\n");
        assert!(rx.is_empty(), "no Msg for an unknown command");

        // The listener survives a bad client.
        send_at(&path, Command::Hide).expect("still serving");
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            Msg::Control(Command::Hide)
        ));
        cleanup_at(&path);
    }

    #[test]
    fn second_instance_refuses_to_start() {
        let path = temp_socket("single-instance");
        let (tx, _rx) = async_channel::unbounded();
        let _first = spawn_at(&path, tx).expect("first instance binds");

        let (tx2, _rx2) = async_channel::unbounded();
        let err = spawn_at(&path, tx2).unwrap_err();
        assert_eq!(err, "svitek is already running");
        cleanup_at(&path);
    }
}
