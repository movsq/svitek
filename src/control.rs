//! The control socket: how `svitek toggle` reaches the resident instance.
//!
//! Unix stream socket at `$XDG_RUNTIME_DIR/svitek.sock` (fallback
//! `/tmp/svitek-<uid>/svitek.sock`). Protocol: client sends one line
//! (`toggle` | `show` | `hide` | `next` | `prev` | `quit`), server replies
//! `ok\n` (or `err ...\n`) and closes. A stale socket file (nobody listening)
//! is replaced on bind.
//!
//! `$SVITEK_SOCKET` overrides the path entirely (used by the tests and to run
//! several instances against several compositors).
//!
//! # Who may talk to it
//!
//! Anything that can write one line here can move the user's workspaces around
//! and stop their panel, so the socket is the daemon's whole attack surface and
//! it is closed three ways:
//!
//! * **Mode 0600 on the socket itself**, and no window where it is anything
//!   else: `bind(2)` applies the umask, so the socket is created under a
//!   private name (`<path>.<pid>`), `chmod`ped, and only then `rename`d onto
//!   the real path. A client that can see the path can therefore never see it
//!   world-writable, and the rename is atomic, so `svitek toggle` either finds
//!   the old socket or the new one and never a half-built thing.
//! * **A private directory for the `/tmp` fallback.** `$XDG_RUNTIME_DIR` is
//!   already 0700 and ours; `/tmp` is world-writable, and a socket sitting
//!   directly in it can be replaced or pre-created by any local user. Without
//!   `$XDG_RUNTIME_DIR` we therefore make `/tmp/svitek-<uid>/` ourselves, 0700,
//!   verify (with `symlink_metadata`, so a symlink pointing somewhere else is
//!   not mistaken for a directory) that what is there is a directory we own
//!   with exactly those bits, and refuse to start if it is not.
//! * **`SO_PEERCRED` on every connection.** Permissions are the barrier; this
//!   is the check that the barrier held. A peer that is not our uid is dropped
//!   with a warning before its line is even read. (`UnixStream::peer_cred` is
//!   still unstable, hence `libc::getsockopt` by hand.)
//!
//! The unlink side is in `cleanup`: it removes the socket only while the path
//! still resolves to the inode we bound, so a quit that races a successor's
//! start cannot take the new daemon's socket with it.

use crate::model::{Command, Msg};
use async_channel::Sender;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// A client must send its line within this long, or it is dropped; the accept
/// loop handles one connection at a time and must not be hostage to a peer.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// The daemon always answers immediately, so the client can be impatient too.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
/// A command line is a handful of bytes; refuse to buffer more than this.
const MAX_LINE: u64 = 1024;
/// The socket is for this user and nobody else.
const SOCKET_MODE: u32 = 0o600;
/// …and so is the directory we have to make ourselves.
const FALLBACK_DIR_MODE: u32 = 0o700;

/// `(st_dev, st_ino)` of the socket this process bound, once it has one. Every
/// unlink is conditional on the path still being *that* inode.
static BOUND: OnceLock<(u64, u64)> = OnceLock::new();

/// Where the socket goes, and the directory we are responsible for creating
/// (`Some` only for the `/tmp` fallback — an explicit `$SVITEK_SOCKET` and
/// `$XDG_RUNTIME_DIR` both live in a directory somebody else already owns).
fn socket_location() -> (PathBuf, Option<PathBuf>) {
    if let Some(p) = std::env::var_os("SVITEK_SOCKET") {
        if !p.is_empty() {
            return (PathBuf::from(p), None);
        }
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return (PathBuf::from(dir).join("svitek.sock"), None);
        }
    }
    let dir = fallback_dir();
    let path = dir.join("svitek.sock");
    (path, Some(dir))
}

/// The private directory the socket lives in when there is no
/// `$XDG_RUNTIME_DIR`. One per uid, because `/tmp` is shared.
fn fallback_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/svitek-{}", unsafe { libc::getuid() }))
}

/// Where the socket is. Pure path arithmetic — it creates nothing and checks
/// nothing, so it is safe to call from anywhere (including the exit handler).
pub fn socket_path() -> PathBuf {
    socket_location().0
}

/// Bind the socket and spawn the accept thread. Fails if another instance is
/// alive on the socket (so the daemon never runs twice), or if the `/tmp`
/// fallback directory is not a private directory of ours.
pub fn spawn(tx: Sender<Msg>) -> Result<std::thread::JoinHandle<()>, String> {
    let (path, private_dir) = socket_location();
    if let Some(dir) = private_dir {
        ensure_private_dir(&dir)?;
    }
    let (handle, id) = spawn_at(&path, tx)?;
    let _ = BOUND.set(id);
    Ok(handle)
}

/// Remove the socket file (called on exit) — but only while it is still the
/// socket *we* bound. Between our last command and this call a successor
/// daemon can have started and bound its own socket over the same path (the
/// user's `svitek quit; svitek` is exactly that), and unlinking by path alone
/// would leave the new instance listening on a socket nobody can reach.
pub fn cleanup() {
    cleanup_at(&socket_path(), BOUND.get().copied());
}

/// `(st_dev, st_ino)` of the socket we bound, for the exit handler in `main`,
/// which has to make the same "still ours?" check without allocating.
pub fn socket_identity() -> Option<(u64, u64)> {
    BOUND.get().copied()
}

/// Client side: send `cmd` to the running instance. Errors if none is running.
pub fn send(cmd: Command) -> Result<(), String> {
    send_at(&socket_path(), cmd)
}

/// Create (or adopt) `dir` as a directory only we can enter.
///
/// `/tmp` is world-writable, so everything here is about not trusting what is
/// already at that path: `symlink_metadata` rather than `metadata` (a symlink
/// must fail the "is a directory" test instead of being followed to wherever it
/// points), the owner must be us, and the mode must be exactly 0700 — a group-
/// or world-accessible directory is not one we can put a control socket in,
/// whoever made it. We only ever chmod a directory we created ourselves.
fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    let created = match std::fs::DirBuilder::new()
        .mode(FALLBACK_DIR_MODE)
        .create(dir)
    {
        Ok(()) => true,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => false,
        Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
    };
    if created {
        // DirBuilder::mode is masked by the umask; say it again, unmasked.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(FALLBACK_DIR_MODE))
            .map_err(|e| format!("cannot set the mode of {}: {e}", dir.display()))?;
    }

    let md = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot stat {}: {e}", dir.display()))?;
    if !md.file_type().is_dir() {
        return Err(format!(
            "{} exists and is not a directory; refusing to put the control socket there",
            dir.display()
        ));
    }
    let uid = unsafe { libc::getuid() };
    if md.uid() != uid {
        return Err(format!(
            "{} is owned by uid {}, not {uid}; refusing to put the control socket there",
            dir.display(),
            md.uid()
        ));
    }
    let mode = md.mode() & 0o7777;
    if mode != FALLBACK_DIR_MODE {
        return Err(format!(
            "{} has mode {:04o}, expected {:04o}; refusing to put the control socket there",
            dir.display(),
            mode,
            FALLBACK_DIR_MODE
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// path-explicit forms: the public API is these with `socket_path()` filled in.
// Tests use them directly so they never have to touch the environment.
// ---------------------------------------------------------------------------

/// Bind, and hand back the listener together with the `(st_dev, st_ino)` of
/// the socket file it is bound to.
///
/// The socket is never created at `path`: `bind(2)` takes the umask, so a
/// permissive one (`umask 0`, and a login shell is not the only thing that
/// starts a daemon) would leave the socket world-writable for as long as it
/// takes to `chmod` it, and that window is all an attacker needs. It is bound
/// under `<path>.<pid>` instead — a name no client looks at — chmodded to
/// `SOCKET_MODE`, and only then `rename`d onto `path`. `rename(2)` is atomic,
/// so a concurrent `svitek toggle` sees either the old socket or the finished
/// new one, and a stale file at `path` is replaced by the same step (no
/// unlink-then-bind gap where the path does not exist at all).
fn bind_at(path: &Path) -> Result<(UnixListener, (u64, u64)), String> {
    match UnixStream::connect(path) {
        // Somebody answered: a live instance owns this socket.
        Ok(_) => return Err("svitek is already running".to_string()),
        // Nobody listening (or nothing there): stale or free, ours to take.
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
            if e.kind() == ErrorKind::ConnectionRefused {
                log::info!("replacing stale control socket {}", path.display());
            }
        }
        Err(e) => {
            return Err(format!(
                "control socket {} exists but cannot be probed: {e}",
                path.display()
            ))
        }
    }

    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    // A previous run that died between bind and rename left one of these; it
    // is named after a pid that is no longer us, or is us on a second attempt.
    if let Err(e) = std::fs::remove_file(&tmp) {
        if e.kind() != ErrorKind::NotFound {
            return Err(format!("cannot clear {}: {e}", tmp.display()));
        }
    }

    let listener = UnixListener::bind(&tmp)
        .map_err(|e| format!("cannot bind control socket {}: {e}", tmp.display()))?;

    // From here on every failure has to take the half-built socket with it.
    let finish = || -> std::io::Result<(u64, u64)> {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(SOCKET_MODE))?;
        // Stat before the rename: it is the same inode afterwards, and this
        // way we are looking at a path only we know the name of.
        let md = std::fs::symlink_metadata(&tmp)?;
        std::fs::rename(&tmp, path)?;
        Ok((md.dev(), md.ino()))
    };
    match finish() {
        Ok(id) => {
            log::debug!("listening on {} (mode {SOCKET_MODE:04o})", path.display());
            Ok((listener, id))
        }
        Err(e) => {
            drop(listener);
            let _ = std::fs::remove_file(&tmp);
            Err(format!(
                "cannot install control socket {}: {e}",
                path.display()
            ))
        }
    }
}

fn spawn_at(
    path: &Path,
    tx: Sender<Msg>,
) -> Result<(std::thread::JoinHandle<()>, (u64, u64)), String> {
    let (listener, id) = bind_at(path)?;

    let handle = std::thread::Builder::new()
        .name("svitek-control".to_string())
        .spawn(move || accept_loop(listener, tx))
        .map_err(|e| format!("cannot spawn control thread: {e}"))?;
    Ok((handle, id))
}

/// Unlink `path`, but only if it is still `expect`.
///
/// `None` means "whatever is there" and is what the tests use; the daemon
/// always passes the identity it bound, so that a cleanup arriving after a
/// successor has taken the path over is a no-op instead of a denial of service
/// on the new instance.
fn cleanup_at(path: &Path, expect: Option<(u64, u64)>) {
    if let Some(id) = expect {
        match std::fs::symlink_metadata(path) {
            Ok(md) if (md.dev(), md.ino()) == id => {}
            Ok(_) => {
                log::debug!(
                    "control socket {} belongs to another instance now; leaving it",
                    path.display()
                );
                return;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return,
            Err(e) => {
                log::warn!("cannot stat control socket {}: {e}", path.display());
                return;
            }
        }
    }
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
        "" => Err(not_running(
            path,
            "the running instance closed the connection",
        )),
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

/// The uid on the other end of `stream`, from the kernel.
///
/// `SO_PEERCRED` is recorded by the kernel at `connect(2)` time, so it cannot
/// be forged or changed afterwards by the peer, and unlike anything the peer
/// tells us it needs no protocol. (`UnixStream::peer_cred` is still unstable,
/// so this is `getsockopt` by hand; `libc::ucred` is Linux, which is all
/// sway runs on.)
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a live `ucred` and `len` its size; both outlive the
    // call, and the fd is owned by `stream`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Serve one connection. Returns false when the main loop is gone (shutdown).
///
/// The first thing that happens to a connection is the credential check: the
/// socket's mode already keeps other users out, so a peer that is not us means
/// either the permissions were subverted or the socket sits somewhere we were
/// wrong about, and in both cases the answer is to read nothing and say
/// nothing. Only after that is a line read.
fn serve(stream: UnixStream, tx: &Sender<Msg>) -> bool {
    let us = unsafe { libc::getuid() };
    match peer_uid(&stream) {
        Ok(uid) if uid == us => {}
        Ok(uid) => {
            log::warn!("control: refusing a connection from uid {uid} (we are {us})");
            return true;
        }
        Err(e) => {
            log::warn!("control: cannot read the peer's credentials, dropping it: {e}");
            return true;
        }
    }

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
    use std::os::unix::fs::FileTypeExt;
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
        let (_h, _id) = spawn_at(&path, tx).expect("bind");

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

        cleanup_at(&path, None);
        assert!(!path.exists());
    }

    /// `ok` means the command is already queued: the moment `send_at` returns,
    /// the message must be readable without waiting for anything.
    #[test]
    fn ok_means_the_command_is_already_queued() {
        let path = temp_socket("queued-first");
        let (tx, rx) = async_channel::unbounded();
        let (_h, _id) = spawn_at(&path, tx).expect("bind");

        for _ in 0..20 {
            send_at(&path, Command::Toggle).expect("toggle accepted");
            match rx.try_recv() {
                Ok(Msg::Control(Command::Toggle)) => {}
                other => panic!("reply arrived before the command was queued: {other:?}"),
            }
        }
        cleanup_at(&path, None);
    }

    /// With the main loop gone there is nothing to queue to, and the client is
    /// told so instead of being promised an action nobody will take.
    #[test]
    fn a_closed_main_loop_is_reported_as_an_error() {
        let path = temp_socket("closed-loop");
        let (tx, rx) = async_channel::unbounded::<Msg>();
        let (_h, _id) = spawn_at(&path, tx).expect("bind");
        drop(rx);

        let err = send_at(&path, Command::Toggle).unwrap_err();
        assert!(err.contains("shutting down"), "{err}");
        cleanup_at(&path, None);
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
        let (_h, _id) = spawn_at(&path, tx).expect("stale socket should be replaced");
        send_at(&path, Command::Show).expect("show accepted");
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            Msg::Control(Command::Show)
        ));
        cleanup_at(&path, None);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let path = temp_socket("unknown");
        let (tx, rx) = async_channel::unbounded();
        let (_h, _id) = spawn_at(&path, tx).expect("bind");

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
        cleanup_at(&path, None);
    }

    #[test]
    fn second_instance_refuses_to_start() {
        let path = temp_socket("single-instance");
        let (tx, _rx) = async_channel::unbounded();
        let (_first, _id) = spawn_at(&path, tx).expect("first instance binds");

        let (tx2, _rx2) = async_channel::unbounded();
        let err = spawn_at(&path, tx2).unwrap_err();
        assert_eq!(err, "svitek is already running");
        cleanup_at(&path, None);
    }

    /// Anyone who can write a line to this socket owns the user's workspaces,
    /// so it is 0600 the moment it is visible at its real name — not "0600
    /// after a chmod that follows the bind", which is a window, and not
    /// "whatever the umask said".
    #[test]
    fn the_socket_is_private_and_leaves_no_temporary_behind() {
        let path = temp_socket("mode");
        // A permissive umask is the case the bind-elsewhere-then-rename dance
        // exists for: with `bind` straight onto `path` this would be 0777.
        let old = unsafe { libc::umask(0) };
        let (tx, _rx) = async_channel::unbounded();
        let (_h, id) = spawn_at(&path, tx).expect("bind");
        unsafe { libc::umask(old) };

        let md = std::fs::symlink_metadata(&path).expect("the socket is at its real name");
        assert_eq!(
            md.mode() & 0o777,
            SOCKET_MODE,
            "control socket {} is mode {:04o}",
            path.display(),
            md.mode() & 0o777
        );
        assert!(md.file_type().is_socket());
        // …and it is the inode `spawn_at` reported, which is what every unlink
        // is checked against.
        assert_eq!((md.dev(), md.ino()), id);

        let tmp = PathBuf::from(format!("{}.{}", path.display(), std::process::id()));
        assert!(!tmp.exists(), "{} was left behind", tmp.display());

        // Still a working socket, permissions and all.
        send_at(&path, Command::Toggle).expect("toggle accepted");
        cleanup_at(&path, Some(id));
        assert!(!path.exists());
    }

    /// `svitek quit; svitek` can put a successor on the path before our exit
    /// handler runs. Unlinking by path alone would leave the new daemon
    /// listening on a socket no client can find.
    #[test]
    fn cleanup_spares_a_successors_socket() {
        let path = temp_socket("successor");
        let (tx, _rx) = async_channel::unbounded();
        let (_h, first) = spawn_at(&path, tx).expect("bind");

        // The successor: same path, a different inode.
        std::fs::remove_file(&path).unwrap();
        let (tx2, _rx2) = async_channel::unbounded();
        let (_h2, second) = spawn_at(&path, tx2).expect("the successor binds");
        assert_ne!(first, second);

        cleanup_at(&path, Some(first));
        assert!(path.exists(), "the successor's socket was unlinked");
        send_at(&path, Command::Toggle).expect("the successor is still reachable");

        cleanup_at(&path, Some(second));
        assert!(!path.exists(), "the owner's own cleanup must still work");
    }

    /// The `/tmp` fallback is only usable inside a directory that is a real
    /// directory, ours, and 0700. Everything else is somebody else's ground.
    #[test]
    fn the_fallback_directory_must_be_private() {
        let base = std::env::temp_dir().join(format!("svitek-test-{}-priv", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // Created by us: made 0700 whatever the umask, and accepted again on
        // the next start (the daemon adopts its own directory).
        let fresh = base.join("fresh");
        let old = unsafe { libc::umask(0) };
        ensure_private_dir(&fresh).expect("a directory we create is private");
        unsafe { libc::umask(old) };
        assert_eq!(
            std::fs::symlink_metadata(&fresh).unwrap().mode() & 0o7777,
            FALLBACK_DIR_MODE
        );
        ensure_private_dir(&fresh).expect("adopting our own directory");

        // Too open: refused rather than quietly chmodded, because we did not
        // make it and cannot know who has been in it.
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_private_dir(&open).unwrap_err();
        assert!(err.contains("mode 0755"), "{err}");

        // A symlink to a perfectly private directory is still not a directory:
        // `symlink_metadata` is what makes that visible.
        let link = base.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        let err = ensure_private_dir(&link).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");

        // A plain file in the way.
        let file = base.join("file");
        std::fs::write(&file, b"").unwrap();
        let err = ensure_private_dir(&file).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The credential check has to *pass* for the only peer that matters. (The
    /// refusal side needs a second uid, so it is not testable without root;
    /// what is testable is that the kernel tells us what we think it does.)
    #[test]
    fn the_peer_is_us() {
        let path = temp_socket("peercred");
        let (tx, _rx) = async_channel::unbounded();
        let (_h, id) = spawn_at(&path, tx).expect("bind");

        let stream = UnixStream::connect(&path).unwrap();
        assert_eq!(peer_uid(&stream).unwrap(), unsafe { libc::getuid() });
        drop(stream);

        send_at(&path, Command::Show).expect("our own connection is served");
        cleanup_at(&path, Some(id));
    }
}
