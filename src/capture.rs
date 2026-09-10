//! Output screencopy (wlr-screencopy-unstable-v1) on a dedicated thread with
//! its own wayland connection (GTK owns the other one; never share).
//!
//! Produces downscaled `Thumbnail`s as `Msg::Frame`. The thread keeps a
//! throttled `copy_with_damage` loop per known output while not paused, so
//! there is always a recent frame of whatever is visible on each output
//! (cost is zero when nothing repaints). The main loop attributes frames to
//! the workspace visible on that output when the frame is handled.

use crate::model::{CaptureReason, Msg, Thumbnail};
use async_channel::Sender;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Commands the GTK thread pushes to the capture thread.
#[derive(Debug)]
enum Cmd {
    Request { output: String, reason: CaptureReason },
    SetPaused(bool),
    Shutdown,
}

/// Write end of the self-pipe used to wake the capture thread out of `poll`.
#[derive(Debug)]
struct Waker(OwnedFd);

impl Waker {
    fn wake(&self) {
        let b = [1u8];
        // A full pipe already means "wake up pending", so EAGAIN is fine.
        unsafe {
            libc::write(self.0.as_raw_fd(), b.as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// Handle to the capture thread. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Capturer {
    cmd: mpsc::Sender<Cmd>,
    waker: Arc<Waker>,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Capturer {
    /// Start the capture thread. `thumb_width` is the target thumbnail width
    /// in pixels; height follows the output's aspect ratio.
    /// `min_interval` is the minimum spacing between background captures of
    /// the same output (suggested 400 ms).
    pub fn spawn(
        tx: Sender<Msg>,
        thumb_width: u32,
        min_interval: std::time::Duration,
    ) -> Result<Capturer, String> {
        let (wake_r, wake_w) = pipe2_cloexec_nonblock()?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();

        let thumb_width = thumb_width.max(1);
        let join = std::thread::Builder::new()
            .name("svitek-capture".into())
            .spawn(move || {
                thread_main(tx, thumb_width, min_interval, cmd_rx, wake_r, init_tx);
            })
            .map_err(|e| format!("cannot spawn capture thread: {e}"))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Capturer {
                cmd: cmd_tx,
                waker: Arc::new(Waker(wake_w)),
                join: Arc::new(Mutex::new(Some(join))),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err("capture thread died during startup".into())
            }
        }
    }

    fn send(&self, cmd: Cmd) {
        if self.cmd.send(cmd).is_ok() {
            self.waker.wake();
        }
    }

    /// Ask for one capture of `output` as soon as possible (ignores the
    /// throttle and the pause). Result arrives as `Msg::Frame` / `Msg::CaptureFailed`.
    pub fn request(&self, output: &str, reason: CaptureReason) {
        self.send(Cmd::Request { output: output.to_string(), reason });
    }

    /// Pause / resume the background loop. Paused while the panel is shown so
    /// the panel itself never ends up in a thumbnail (the panel is an overlay
    /// on the same output).
    pub fn set_paused(&self, paused: bool) {
        self.send(Cmd::SetPaused(paused));
    }

    /// Ask the thread to exit.
    pub fn shutdown(&self) {
        self.send(Cmd::Shutdown);
        let handle = self.join.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

fn pipe2_cloexec_nonblock() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// ---------------------------------------------------------------------------
// Pixel handling (pure, unit-tested)
// ---------------------------------------------------------------------------

/// The wl_shm formats we can convert to straight RGBA8.
///
/// wl_shm 32-bit formats are little-endian packed words, so the byte order in
/// memory is the reverse of the name: `xrgb8888` is `[B, G, R, X]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixFmt {
    /// memory order B, G, R, A (xrgb8888 / argb8888)
    Bgra,
    /// memory order R, G, B, A (xbgr8888 / abgr8888)
    Rgba,
    /// memory order R, G, B, 3 bytes per pixel (bgr888 — what a real GPU
    /// output on sway commonly advertises for screencopy)
    Rgb,
    /// memory order B, G, R, 3 bytes per pixel (rgb888)
    Bgr,
}

impl PixFmt {
    fn from_wl(f: wl_shm::Format) -> Option<PixFmt> {
        match f {
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => Some(PixFmt::Bgra),
            wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => Some(PixFmt::Rgba),
            wl_shm::Format::Bgr888 => Some(PixFmt::Rgb),
            wl_shm::Format::Rgb888 => Some(PixFmt::Bgr),
            _ => None,
        }
    }
    /// Bytes per pixel in the source buffer.
    #[inline]
    fn bpp(self) -> usize {
        match self {
            PixFmt::Bgra | PixFmt::Rgba => 4,
            PixFmt::Rgb | PixFmt::Bgr => 3,
        }
    }
    /// (red, green, blue) byte offsets inside one source pixel.
    #[inline]
    fn offsets(self) -> (usize, usize, usize) {
        match self {
            PixFmt::Bgra | PixFmt::Bgr => (2, 1, 0),
            PixFmt::Rgba | PixFmt::Rgb => (0, 1, 2),
        }
    }
}

/// Thumbnail size for a `w`x`h` source at target width `target_w`, keeping the
/// aspect ratio and never upscaling.
pub(crate) fn thumb_size(w: u32, h: u32, target_w: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let out_w = target_w.max(1).min(w);
    let out_h = ((h as u64 * out_w as u64 + w as u64 / 2) / w as u64).max(1) as u32;
    (out_w, out_h)
}

/// Convert a captured shm buffer to straight RGBA8 and box-downscale it to
/// `out_w` x `out_h` in a single pass. `y_invert` flips vertically.
///
/// Every output pixel is the unweighted average of the source pixels whose
/// centres fall in its bucket (an area/box filter), which is what we want for
/// the large reduction ratios thumbnails use.
#[allow(clippy::too_many_arguments)] // a pixel routine; a struct would only rename the arguments
pub(crate) fn convert_downscale(
    src: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    fmt: PixFmt,
    y_invert: bool,
    out_w: u32,
    out_h: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    if w == 0 || h == 0 || out_w == 0 || out_h == 0 {
        return out;
    }
    let (ro, go, bo) = fmt.offsets();
    let bpp = fmt.bpp();
    let stride = stride as usize;

    // Precompute the horizontal buckets once; they are the same for every row.
    let mut xr: Vec<(u32, u32)> = Vec::with_capacity(out_w as usize);
    for ox in 0..out_w {
        let x0 = (ox as u64 * w as u64 / out_w as u64) as u32;
        let mut x1 = ((ox as u64 + 1) * w as u64 / out_w as u64) as u32;
        if x1 <= x0 {
            x1 = x0 + 1;
        }
        xr.push((x0, x1.min(w)));
    }

    for oy in 0..out_h {
        let y0 = (oy as u64 * h as u64 / out_h as u64) as u32;
        let mut y1 = ((oy as u64 + 1) * h as u64 / out_h as u64) as u32;
        if y1 <= y0 {
            y1 = y0 + 1;
        }
        let y1 = y1.min(h);
        let orow = (oy as usize) * (out_w as usize) * 4;

        for (ox, &(x0, x1)) in xr.iter().enumerate() {
            let (mut rs, mut gs, mut bs) = (0u32, 0u32, 0u32);
            let mut n = 0u32;
            for sy in y0..y1 {
                let real_y = if y_invert { h - 1 - sy } else { sy } as usize;
                let row = real_y * stride;
                for sx in x0..x1 {
                    let p = row + (sx as usize) * bpp;
                    if p + bpp > src.len() {
                        continue;
                    }
                    rs += src[p + ro] as u32;
                    gs += src[p + go] as u32;
                    bs += src[p + bo] as u32;
                    n += 1;
                }
            }
            let o = orow + ox * 4;
            if n == 0 {
                out[o + 3] = 255;
                continue;
            }
            let half = n / 2;
            out[o] = ((rs + half) / n) as u8;
            out[o + 1] = ((gs + half) / n) as u8;
            out[o + 2] = ((bs + half) / n) as u8;
            out[o + 3] = 255;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// shm buffers
// ---------------------------------------------------------------------------

struct ShmBuffer {
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    _fd: OwnedFd,
    ptr: *mut libc::c_void,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    /// Set by the compositor's `wl_buffer.release`; shared with the proxy's user data.
    released: Arc<AtomicBool>,
}

impl ShmBuffer {
    fn new<D>(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<ShmBuffer, String>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + 'static,
        D: Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> + 'static,
    {
        let len = stride as usize * height as usize;
        if len == 0 {
            return Err("zero-sized capture buffer".into());
        }
        let name = c"svitek-screencopy";
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(format!("ftruncate: {}", std::io::Error::last_os_error()));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }

        let released = Arc::new(AtomicBool::new(true));
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            released.clone(),
        );
        Ok(ShmBuffer {
            pool,
            buffer,
            _fd: fd,
            ptr,
            len,
            width,
            height,
            stride,
            format,
            released,
        })
    }

    fn matches(&self, width: u32, height: u32, stride: u32, format: wl_shm::Format) -> bool {
        self.width == width
            && self.height == height
            && self.stride == stride
            && self.format == format
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

// `*mut c_void` is not Send by default; the mapping is owned exclusively by the
// capture thread and never shared, so this is sound.
unsafe impl Send for ShmBuffer {}

// ---------------------------------------------------------------------------
// Per-output state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct Pending {
    id: u64,
    frame: ZwlrScreencopyFrameV1,
    reason: CaptureReason,
    started: Instant,
    /// shm buffer parameters from the `buffer` event, if any supported one arrived.
    info: Option<BufInfo>,
    /// set once we sent copy/copy_with_damage
    copying: bool,
    y_invert: bool,
}

struct Output {
    global: u32,
    wl: wl_output::WlOutput,
    name: Option<String>,
    pending: Option<Pending>,
    /// Earliest instant at which a new *background* capture may be started.
    next_due: Instant,
    buffer: Option<ShmBuffer>,
}

impl Output {
    fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("<unnamed>")
    }
}

// ---------------------------------------------------------------------------
// The capture thread's state
// ---------------------------------------------------------------------------

struct App {
    tx: Sender<Msg>,
    qh: QueueHandle<App>,
    shm: Option<wl_shm::WlShm>,
    manager: Option<ZwlrScreencopyManagerV1>,
    manager_version: u32,
    outputs: Vec<Output>,
    thumb_width: u32,
    min_interval: Duration,
    paused: bool,
    running: bool,
    next_frame_id: u64,
    last_sent_outputs: Vec<String>,
    outputs_dirty: bool,
    /// Set once the startup roundtrips are done; suppresses the partial
    /// `CaptureOutputs` bursts the initial global/name storm would produce.
    started: bool,
    registry: Option<wl_registry::WlRegistry>,
}

impl App {
    fn emit(&mut self, msg: Msg) {
        if self.tx.send_blocking(msg).is_err() {
            log::debug!("capture: message channel closed, stopping");
            self.running = false;
        }
    }

    fn find_by_global(&self, global: u32) -> Option<usize> {
        self.outputs.iter().position(|o| o.global == global)
    }
    fn find_by_name(&self, name: &str) -> Option<usize> {
        self.outputs.iter().position(|o| o.name.as_deref() == Some(name))
    }
    fn find_by_frame(&self, id: u64) -> Option<usize> {
        self.outputs
            .iter()
            .position(|o| o.pending.as_ref().map(|p| p.id) == Some(id))
    }

    /// Kill a pending capture without reporting anything.
    fn drop_pending(&mut self, idx: usize) {
        if let Some(p) = self.outputs[idx].pending.take() {
            log::debug!(
                "capture: dropping pending {:?} frame for {}",
                p.reason,
                self.outputs[idx].label()
            );
            p.frame.destroy();
        }
    }

    fn start_capture(&mut self, idx: usize, reason: CaptureReason) {
        let manager = match self.manager.clone() {
            Some(m) => m,
            None => return,
        };
        if self.outputs[idx].pending.is_some() {
            self.drop_pending(idx);
        }
        let id = self.next_frame_id;
        self.next_frame_id += 1;
        let wl = self.outputs[idx].wl.clone();
        let frame = manager.capture_output(0, &wl, &self.qh, id);
        log::debug!(
            "capture: start {:?} capture of {} (frame {})",
            reason,
            self.outputs[idx].label(),
            id
        );
        self.outputs[idx].pending = Some(Pending {
            id,
            frame,
            reason,
            started: Instant::now(),
            info: None,
            copying: false,
            y_invert: false,
        });
    }

    /// The `buffer`/`buffer_done` handshake is over: allocate and ask for the copy.
    fn begin_copy(&mut self, idx: usize) {
        let shm = match self.shm.clone() {
            Some(s) => s,
            None => return,
        };
        let (info, reason) = match self.outputs[idx].pending.as_ref() {
            Some(p) if !p.copying && p.info.is_some() => (p.info.unwrap(), p.reason),
            _ => return,
        };

        // Reuse the cached buffer when it fits and the compositor gave it back.
        let reusable = match self.outputs[idx].buffer.as_ref() {
            Some(b) => {
                b.matches(info.width, info.height, info.stride, info.format)
                    && b.released.load(Ordering::Acquire)
            }
            None => false,
        };
        if !reusable {
            self.outputs[idx].buffer = None;
            match ShmBuffer::new(
                &shm,
                &self.qh,
                info.width,
                info.height,
                info.stride,
                info.format,
            ) {
                Ok(b) => {
                    log::debug!(
                        "capture: allocated {}x{} shm buffer ({} KiB) for {}",
                        info.width,
                        info.height,
                        b.len / 1024,
                        self.outputs[idx].label()
                    );
                    self.outputs[idx].buffer = Some(b);
                }
                Err(e) => {
                    let name = self.outputs[idx].label().to_string();
                    log::warn!("capture: buffer allocation for {name} failed: {e}");
                    self.drop_pending(idx);
                    self.outputs[idx].next_due = Instant::now() + self.min_interval;
                    self.emit(Msg::CaptureFailed { output: name, reason, error: e });
                    return;
                }
            }
        }

        let buf = self.outputs[idx].buffer.as_ref().unwrap();
        buf.released.store(false, Ordering::Release);
        let wl_buf = buf.buffer.clone();
        let use_damage = reason == CaptureReason::Background && self.manager_version >= 2;
        if let Some(p) = self.outputs[idx].pending.as_mut() {
            if use_damage {
                p.frame.copy_with_damage(&wl_buf);
            } else {
                p.frame.copy(&wl_buf);
            }
            p.copying = true;
        }
    }

    fn on_ready(&mut self, idx: usize) {
        let p = match self.outputs[idx].pending.take() {
            Some(p) => p,
            None => return,
        };
        p.frame.destroy();
        let name = self.outputs[idx].label().to_string();
        self.outputs[idx].next_due = Instant::now() + self.min_interval;

        let info = match p.info {
            Some(i) => i,
            None => {
                self.emit(Msg::CaptureFailed {
                    output: name,
                    reason: p.reason,
                    error: "no supported shm buffer format".into(),
                });
                return;
            }
        };
        let fmt = match PixFmt::from_wl(info.format) {
            Some(f) => f,
            None => {
                self.emit(Msg::CaptureFailed {
                    output: name,
                    reason: p.reason,
                    error: format!("unsupported pixel format {:?}", info.format),
                });
                return;
            }
        };
        let buf = match self.outputs[idx].buffer.as_ref() {
            Some(b) if b.matches(info.width, info.height, info.stride, info.format) => b,
            _ => {
                self.emit(Msg::CaptureFailed {
                    output: name,
                    reason: p.reason,
                    error: "capture buffer vanished".into(),
                });
                return;
            }
        };

        let (out_w, out_h) = thumb_size(info.width, info.height, self.thumb_width);
        let t0 = Instant::now();
        let rgba = convert_downscale(
            buf.as_slice(),
            info.width,
            info.height,
            info.stride,
            fmt,
            p.y_invert,
            out_w,
            out_h,
        );
        log::debug!(
            "capture: {} {:?} {}x{} -> {}x{} in {:.2} ms (latency {:.2} ms)",
            name,
            p.reason,
            info.width,
            info.height,
            out_w,
            out_h,
            t0.elapsed().as_secs_f64() * 1e3,
            p.started.elapsed().as_secs_f64() * 1e3
        );
        let thumb = Thumbnail {
            width: out_w,
            height: out_h,
            rgba: Arc::from(rgba.into_boxed_slice()),
            taken_at: Instant::now(),
        };
        self.emit(Msg::Frame { output: name, thumb, reason: p.reason });
    }

    fn on_failed(&mut self, idx: usize) {
        let p = match self.outputs[idx].pending.take() {
            Some(p) => p,
            None => return,
        };
        p.frame.destroy();
        let name = self.outputs[idx].label().to_string();
        self.outputs[idx].next_due = Instant::now() + self.min_interval;
        log::debug!("capture: {} {:?} capture failed", name, p.reason);
        self.emit(Msg::CaptureFailed {
            output: name,
            reason: p.reason,
            error: "compositor reported frame copy failure".into(),
        });
    }

    /// Issue background captures for every idle, due output.
    fn schedule(&mut self) {
        if self.paused || self.manager.is_none() {
            return;
        }
        let now = Instant::now();
        let due: Vec<usize> = self
            .outputs
            .iter()
            .enumerate()
            .filter(|(_, o)| o.name.is_some() && o.pending.is_none() && o.next_due <= now)
            .map(|(i, _)| i)
            .collect();
        for idx in due {
            self.start_capture(idx, CaptureReason::Background);
        }
    }

    /// Milliseconds until the next background capture becomes due (`-1` = never).
    fn poll_timeout(&self) -> i32 {
        if self.paused || self.manager.is_none() {
            return -1;
        }
        let now = Instant::now();
        let mut best: Option<Duration> = None;
        for o in &self.outputs {
            if o.name.is_none() || o.pending.is_some() {
                continue;
            }
            let d = o.next_due.saturating_duration_since(now);
            best = Some(match best {
                Some(b) if b <= d => b,
                _ => d,
            });
        }
        match best {
            None => -1,
            Some(d) => (d.as_millis().min(i32::MAX as u128) as i32).max(0),
        }
    }

    fn publish_outputs(&mut self) {
        if !self.started || !self.outputs_dirty {
            return;
        }
        self.outputs_dirty = false;
        let mut names: Vec<String> =
            self.outputs.iter().filter_map(|o| o.name.clone()).collect();
        names.sort();
        if names == self.last_sent_outputs {
            return;
        }
        self.last_sent_outputs = names.clone();
        log::debug!("capture: outputs = {names:?}");
        self.emit(Msg::CaptureOutputs(names));
    }

    fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Shutdown => {
                log::debug!("capture: shutdown requested");
                self.running = false;
            }
            Cmd::SetPaused(p) => {
                if self.paused == p {
                    return;
                }
                self.paused = p;
                log::debug!("capture: paused = {p}");
                if p {
                    // A pending copy_with_damage would otherwise complete once
                    // the panel maps and capture the panel itself.
                    let bg: Vec<usize> = self
                        .outputs
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| {
                            o.pending.as_ref().map(|p| p.reason) == Some(CaptureReason::Background)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    for idx in bg {
                        self.drop_pending(idx);
                    }
                } else {
                    let now = Instant::now();
                    for o in &mut self.outputs {
                        o.next_due = now;
                    }
                }
            }
            Cmd::Request { output, reason } => match self.find_by_name(&output) {
                Some(idx) => self.start_capture(idx, reason),
                None => {
                    log::warn!("capture: request for unknown output {output}");
                    self.emit(Msg::CaptureFailed {
                        output,
                        reason,
                        error: "unknown output".into(),
                    });
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

const WANT_OUTPUT_VERSION: u32 = 4;
const WANT_MANAGER_VERSION: u32 = 3;

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => match &interface[..] {
                "wl_shm" => {
                    if state.shm.is_none() {
                        state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                    }
                }
                "zwlr_screencopy_manager_v1" => {
                    if state.manager.is_none() {
                        let v = version.min(WANT_MANAGER_VERSION);
                        state.manager_version = v;
                        state.manager =
                            Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, v, qh, ()));
                        if v < 2 {
                            log::warn!(
                                "capture: zwlr_screencopy_manager_v1 is only v{v}; \
                                 no copy_with_damage, background captures will poll"
                            );
                        }
                    }
                }
                "wl_output" => {
                    let v = version.min(WANT_OUTPUT_VERSION);
                    if v < 4 {
                        log::warn!(
                            "capture: wl_output is only v{v}; no `name` event, output ignored"
                        );
                        return;
                    }
                    let wl = registry.bind::<wl_output::WlOutput, _, _>(name, v, qh, name);
                    state.outputs.push(Output {
                        global: name,
                        wl,
                        name: None,
                        pending: None,
                        next_due: Instant::now(),
                        buffer: None,
                    });
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(idx) = state.find_by_global(name) {
                    state.drop_pending(idx);
                    let o = state.outputs.remove(idx);
                    log::debug!("capture: output {} removed", o.label());
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                    state.outputs_dirty = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for App {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idx = match state.find_by_global(*global) {
            Some(i) => i,
            None => return,
        };
        match event {
            wl_output::Event::Name { name } => {
                if state.outputs[idx].name.as_deref() != Some(name.as_str()) {
                    state.outputs[idx].name = Some(name);
                    state.outputs_dirty = true;
                }
            }
            wl_output::Event::Done => state.publish_outputs(),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, u64> for App {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Events for frames we already destroyed (paused, superseded, output
        // unplugged) simply have no owner any more.
        let idx = match state.find_by_frame(*id) {
            Some(i) => i,
            None => return,
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                let format = match format {
                    WEnum::Value(f) => f,
                    WEnum::Unknown(v) => {
                        log::debug!("capture: unknown shm format {v:#x}");
                        return;
                    }
                };
                if PixFmt::from_wl(format).is_none() {
                    log::debug!("capture: ignoring unsupported shm format {format:?}");
                    return;
                }
                if let Some(p) = state.outputs[idx].pending.as_mut() {
                    if p.info.is_none() {
                        p.info = Some(BufInfo { format, width, height, stride });
                    }
                }
                // v1/v2 have no buffer_done: the single buffer event is the cue.
                let v = state.outputs[idx]
                    .pending
                    .as_ref()
                    .map(|p| p.frame.version())
                    .unwrap_or(1);
                if v < 3 {
                    state.begin_copy(idx);
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                let has_info = state.outputs[idx]
                    .pending
                    .as_ref()
                    .map(|p| p.info.is_some())
                    .unwrap_or(false);
                if has_info {
                    state.begin_copy(idx);
                } else {
                    let name = state.outputs[idx].label().to_string();
                    let reason = state.outputs[idx].pending.as_ref().unwrap().reason;
                    state.drop_pending(idx);
                    state.outputs[idx].next_due = Instant::now() + state.min_interval;
                    state.emit(Msg::CaptureFailed {
                        output: name,
                        reason,
                        error: "compositor offered no supported shm format".into(),
                    });
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                let y_invert = match flags {
                    WEnum::Value(f) => f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
                    WEnum::Unknown(v) => v & 1 != 0,
                };
                if let Some(p) = state.outputs[idx].pending.as_mut() {
                    p.y_invert = y_invert;
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.on_ready(idx),
            zwlr_screencopy_frame_v1::Event::Failed => state.on_failed(idx),
            // `damage` and `linux_dmabuf` carry nothing we need (we use shm).
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for App {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        released: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            released.store(true, Ordering::Release);
        }
    }
}

delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ZwlrScreencopyManagerV1);

// ---------------------------------------------------------------------------
// Thread main
// ---------------------------------------------------------------------------

fn thread_main(
    tx: Sender<Msg>,
    thumb_width: u32,
    min_interval: Duration,
    cmd_rx: mpsc::Receiver<Cmd>,
    wake_r: OwnedFd,
    init_tx: mpsc::Sender<Result<(), String>>,
) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("wayland connect failed: {e}")));
            return;
        }
    };
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    let display = conn.display();
    let registry = display.get_registry(&qh, ());

    let mut app = App {
        tx,
        qh,
        shm: None,
        manager: None,
        manager_version: 0,
        outputs: Vec::new(),
        thumb_width,
        min_interval,
        paused: false,
        running: true,
        next_frame_id: 1,
        last_sent_outputs: Vec::new(),
        outputs_dirty: false,
        started: false,
        registry: Some(registry),
    };

    // First roundtrip: globals. Second: the wl_output name/mode/done bursts.
    if let Err(e) = queue.roundtrip(&mut app) {
        let _ = init_tx.send(Err(format!("wayland roundtrip failed: {e}")));
        return;
    }
    if let Err(e) = queue.roundtrip(&mut app) {
        let _ = init_tx.send(Err(format!("wayland roundtrip failed: {e}")));
        return;
    }
    if app.manager.is_none() {
        let _ = init_tx.send(Err(
            "compositor does not support zwlr_screencopy_manager_v1".into()
        ));
        return;
    }
    if app.shm.is_none() {
        let _ = init_tx.send(Err("compositor does not advertise wl_shm".into()));
        return;
    }
    log::debug!(
        "capture: screencopy manager v{}, {} output(s)",
        app.manager_version,
        app.outputs.len()
    );
    let _ = init_tx.send(Ok(()));

    // Report the initial output set even if no `done` arrived after the name.
    app.started = true;
    app.outputs_dirty = true;
    app.publish_outputs();

    let wake_fd = wake_r.as_raw_fd();
    while app.running {
        // 1. commands from the GTK thread
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => app.handle_cmd(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.running = false;
                    break;
                }
            }
        }
        if !app.running {
            break;
        }

        // 2. background captures that are due
        app.schedule();
        app.publish_outputs();

        // 3. push everything out before sleeping
        if let Err(e) = queue.flush() {
            log::warn!("capture: flush failed: {e}");
            break;
        }

        // 4. prepare a synchronised read, then poll
        let guard = match queue.prepare_read() {
            Some(g) => g,
            None => {
                // Events are already queued; dispatch them and loop.
                if let Err(e) = queue.dispatch_pending(&mut app) {
                    log::warn!("capture: dispatch failed: {e}");
                    break;
                }
                continue;
            }
        };
        let wl_fd = guard.connection_fd().as_raw_fd();
        let mut fds = [
            libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
        ];
        let timeout = app.poll_timeout();
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            drop(guard);
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("capture: poll failed: {err}");
            break;
        }
        if rc > 0 && fds[0].revents & libc::POLLIN != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(e) => {
                    if is_would_block(&e) {
                        // spurious wakeup
                    } else {
                        log::warn!("capture: wayland read failed: {e}");
                        break;
                    }
                }
            }
        } else {
            drop(guard);
        }
        if rc > 0 && fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            log::warn!("capture: wayland socket hung up");
            break;
        }
        if rc > 0 && fds[1].revents & libc::POLLIN != 0 {
            drain(wake_fd);
        }

        // 5. run the handlers
        if let Err(e) = queue.dispatch_pending(&mut app) {
            log::warn!("capture: dispatch failed: {e}");
            break;
        }
        app.publish_outputs();
    }

    // Tear down: destroy anything still pending so the compositor stops working
    // for us, then let Drop do the rest.
    for i in 0..app.outputs.len() {
        app.drop_pending(i);
    }
    for o in &mut app.outputs {
        o.buffer = None;
        if o.wl.version() >= 3 {
            o.wl.release();
        }
    }
    if let Some(m) = app.manager.take() {
        m.destroy();
    }
    // wl_registry has no destructor request; dropping the proxy is all we can do.
    app.registry = None;
    let _ = queue.flush();
    log::debug!("capture: thread exiting");
}

fn is_would_block(e: &wayland_client::backend::WaylandError) -> bool {
    match e {
        wayland_client::backend::WaylandError::Io(io) => {
            io.kind() == std::io::ErrorKind::WouldBlock
        }
        _ => false,
    }
}

fn drain(fd: libc::c_int) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure pixel maths only — no wayland)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(px: &[(u8, u8, u8)], w: usize, h: usize, stride: usize) -> Vec<u8> {
        let mut v = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = px[y * w + x];
                let p = y * stride + x * 4;
                v[p] = b;
                v[p + 1] = g;
                v[p + 2] = r;
                v[p + 3] = 7; // garbage alpha: must be forced to 255
            }
        }
        v
    }

    #[test]
    fn format_offsets() {
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Xrgb8888), Some(PixFmt::Bgra));
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Argb8888), Some(PixFmt::Bgra));
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Xbgr8888), Some(PixFmt::Rgba));
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Abgr8888), Some(PixFmt::Rgba));
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Bgr888), Some(PixFmt::Rgb));
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Rgb888), Some(PixFmt::Bgr));
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Rgb565), None);
        assert_eq!(PixFmt::from_wl(wl_shm::Format::Yuv420), None);
    }

    #[test]
    fn channel_order_bgra_vs_rgba() {
        // one pixel, memory bytes 10,20,30,40
        let src = [10u8, 20, 30, 40];
        let out = convert_downscale(&src, 1, 1, 4, PixFmt::Bgra, false, 1, 1);
        assert_eq!(out, vec![30, 20, 10, 255]);
        let out = convert_downscale(&src, 1, 1, 4, PixFmt::Rgba, false, 1, 1);
        assert_eq!(out, vec![10, 20, 30, 255]);
    }

    #[test]
    fn alpha_is_forced_opaque() {
        let src = bgra(&[(1, 2, 3)], 1, 1, 4);
        let out = convert_downscale(&src, 1, 1, 4, PixFmt::Bgra, false, 1, 1);
        assert_eq!(out[3], 255);
    }

    #[test]
    fn box_filter_averages_the_whole_block() {
        // 2x2 -> 1x1, must be the mean, not a corner sample (nearest neighbour
        // would give exactly one of the inputs).
        let px = [(0, 0, 0), (100, 100, 100), (200, 200, 200), (255, 255, 255)];
        let src = bgra(&px, 2, 2, 8);
        let out = convert_downscale(&src, 2, 2, 8, PixFmt::Bgra, false, 1, 1);
        let mean = ((0 + 100 + 200 + 255) + 2) / 4; // rounded
        assert_eq!(out, vec![mean as u8, mean as u8, mean as u8, 255]);
    }

    #[test]
    fn box_filter_4x4_to_2x2() {
        let mut px = Vec::new();
        for y in 0..4u32 {
            for x in 0..4u32 {
                let v = (y * 4 + x) as u8 * 16;
                px.push((v, v, v));
            }
        }
        let src = bgra(&px, 4, 4, 16);
        let out = convert_downscale(&src, 4, 4, 16, PixFmt::Bgra, false, 2, 2);
        // top-left quadrant = mean of 0,16,64,80 = 40
        assert_eq!(out[0], 40);
        // top-right quadrant = mean of 32,48,96,112 = 72
        assert_eq!(out[4], 72);
        // bottom-left = mean of 128,144,192,208 = 168
        assert_eq!(out[8], 168);
        assert_eq!(out[12], 200);
    }

    #[test]
    fn stride_padding_is_skipped() {
        // 2x2 image in a buffer with stride 12 (4 bytes of padding per row)
        let px = [(10, 10, 10), (20, 20, 20), (30, 30, 30), (40, 40, 40)];
        let mut src = bgra(&px, 2, 2, 12);
        // poison the padding
        for y in 0..2 {
            for i in 8..12 {
                src[y * 12 + i] = 0xff;
            }
        }
        let out = convert_downscale(&src, 2, 2, 12, PixFmt::Bgra, false, 1, 1);
        assert_eq!(out[0], 25);
    }

    #[test]
    fn y_invert_flips_rows() {
        let px = [(0, 0, 0), (0, 0, 0), (255, 255, 255), (255, 255, 255)];
        let src = bgra(&px, 2, 2, 8);
        // 2x2 -> 1x2 keeps the two rows distinct
        let normal = convert_downscale(&src, 2, 2, 8, PixFmt::Bgra, false, 1, 2);
        assert_eq!(normal[0], 0);
        assert_eq!(normal[4], 255);
        let flipped = convert_downscale(&src, 2, 2, 8, PixFmt::Bgra, true, 1, 2);
        assert_eq!(flipped[0], 255);
        assert_eq!(flipped[4], 0);
    }

    #[test]
    fn thumb_size_keeps_aspect_and_never_upscales() {
        assert_eq!(thumb_size(2560, 1440, 240), (240, 135));
        assert_eq!(thumb_size(1280, 720, 240), (240, 135));
        assert_eq!(thumb_size(1920, 1080, 300), (300, 169));
        // never upscale
        assert_eq!(thumb_size(100, 50, 240), (100, 50));
        // degenerate
        assert_eq!(thumb_size(0, 0, 240), (0, 0));
        // extreme aspect still yields at least one row
        assert_eq!(thumb_size(4000, 3, 240).1, 1);
    }

    #[test]
    fn output_length_and_opacity_are_right() {
        let px: Vec<(u8, u8, u8)> = (0..(64 * 32)).map(|i| ((i % 256) as u8, 1, 2)).collect();
        let src = bgra(&px, 64, 32, 64 * 4);
        let (w, h) = thumb_size(64, 32, 16);
        assert_eq!((w, h), (16, 8));
        let out = convert_downscale(&src, 64, 32, 64 * 4, PixFmt::Bgra, false, w, h);
        assert_eq!(out.len(), (w * h * 4) as usize);
        assert!(out.chunks_exact(4).all(|p| p[3] == 255));
        assert!(out.chunks_exact(4).all(|p| p[1] == 1 && p[2] == 2));
    }

    #[test]
    fn three_byte_formats_convert() {
        // bgr888: memory order R, G, B. 2x1 image, stride 6 (no padding).
        let src = [10u8, 20, 30, 50, 60, 70];
        let out = convert_downscale(&src, 2, 1, 6, PixFmt::Rgb, false, 2, 1);
        assert_eq!(out, vec![10, 20, 30, 255, 50, 60, 70, 255]);
        // rgb888: memory order B, G, R, with 2 bytes of row padding.
        let src = [30u8, 20, 10, 70, 60, 50, 0, 0];
        let out = convert_downscale(&src, 2, 1, 8, PixFmt::Bgr, false, 2, 1);
        assert_eq!(out, vec![10, 20, 30, 255, 50, 60, 70, 255]);
        // Box average over both pixels.
        let out = convert_downscale(&src, 2, 1, 8, PixFmt::Bgr, false, 1, 1);
        assert_eq!(out, vec![30, 40, 50, 255]);
    }
}
