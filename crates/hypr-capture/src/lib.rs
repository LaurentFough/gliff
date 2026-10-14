//! Screen and cursor capture from Hyprland through `ext-image-copy-capture-v1`
//! into GBM-allocated dmabufs, or wl_shm buffers when the compositor offers
//! no linear dmabuf the CPU could map (NVIDIA), or no dmabuf at all, or there
//! is no render node to allocate on (Hyper-V guests).
//!
//! The capture runs on its own thread with a `calloop` loop; the owner sends
//! commands through [`Capturer`] and receives [`CaptureEvent`]s through a
//! callback. No `unsafe`: GBM allocation and mapping use the `gbm` crate's
//! safe API, and cursor pixels are read from a memfd with `read_at`.

#![forbid(unsafe_code)]

mod cursor;
mod thread;

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use calloop::channel::Sender;
use drm_fourcc::DrmFourcc;
use gbm::{BufferObject, Device};

pub use hypr_wl::{OutputInfo, Target};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Wayland(#[from] hypr_wl::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("gbm: {0}")]
    Gbm(String),
    #[error("capture: {0}")]
    Capture(String),
    #[error("capture thread is gone")]
    ThreadGone,
}

impl From<wayland_client::DispatchError> for Error {
    fn from(e: wayland_client::DispatchError) -> Self {
        Error::Wayland(hypr_wl::Error::Wayland(e.to_string()))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub target: Target,
    /// Output name, e.g. `HDMI-A-1` or `HEADLESS-2`.
    pub output: String,
    /// GBM device for dmabufs; when it cannot be opened capture uses wl_shm.
    pub render_node: PathBuf,
    /// Ring size; 3 keeps one frame in flight while one is consumed.
    pub buffers: usize,
    /// Prefer a linear modifier so the CPU can map frames cheaply.
    pub prefer_linear: bool,
    /// With `prefer_linear`, capture into wl_shm buffers when no dmabuf format
    /// offers the linear modifier. NVIDIA offers only tiled modifiers, which
    /// GBM cannot map for the CPU.
    pub shm_fallback: bool,
    /// Also run a cursor session and report shape/position.
    pub cursor: bool,
}

pub const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// The DRM render node to use: explicit, `GLIFF_RENDER_NODE`, or the default.
pub fn render_node(explicit: Option<&std::path::Path>) -> PathBuf {
    explicit
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::var_os("GLIFF_RENDER_NODE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RENDER_NODE))
}

impl CaptureConfig {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            target: Target::default(),
            output: output.into(),
            render_node: render_node(None),
            buffers: 3,
            prefer_linear: true,
            shm_fallback: true,
            cursor: true,
        }
    }
}

/// One plane of a dmabuf.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    pub offset: u32,
    pub stride: u32,
}

/// Import information for a captured buffer. For a wl_shm buffer `fd` is the
/// memfd and `modifier` is `DRM_FORMAT_MOD_INVALID`; it cannot be imported.
#[derive(Debug)]
pub struct DmabufInfo {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<Plane>,
}

/// A buffer in the capture ring. Shared with consumers through an `Arc`.
pub struct CaptureBuffer {
    pub index: usize,
    /// Ring generation this buffer belongs to; bumped on every reallocation so
    /// a release from a pre-resize frame cannot free a current buffer.
    pub generation: u64,
    pub info: DmabufInfo,
    backing: Backing,
}

enum Backing {
    Gbm {
        bo: Mutex<BufferObject<()>>,
        device: Arc<Mutex<Device<File>>>,
    },
    Shm(File),
}

impl std::fmt::Debug for CaptureBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureBuffer")
            .field("index", &self.index)
            .field("info", &self.info)
            .finish()
    }
}

impl CaptureBuffer {
    /// True for a wl_shm buffer, which only the CPU path can read.
    pub fn is_shm(&self) -> bool {
        matches!(self.backing, Backing::Shm(_))
    }

    /// Map the whole buffer for CPU reads and run `f(pixels, stride_bytes)`.
    /// A wl_shm buffer is read into a temporary copy first; `read_bgra` reads
    /// it directly instead.
    pub fn with_mapped<R>(&self, f: impl FnOnce(&[u8], u32) -> R) -> Result<R> {
        let (bo, device) = match &self.backing {
            Backing::Gbm { bo, device } => (bo, device),
            Backing::Shm(file) => {
                let stride = self.info.planes[0].stride;
                let mut pixels = vec![0u8; stride as usize * self.info.height as usize];
                file.read_exact_at(&mut pixels, 0)?;
                return Ok(f(&pixels, stride));
            }
        };
        let device = device
            .lock()
            .map_err(|_| Error::Gbm("device mutex poisoned".into()))?;
        let bo = bo
            .lock()
            .map_err(|_| Error::Gbm("buffer mutex poisoned".into()))?;
        let (w, h) = (self.info.width, self.info.height);
        let mapped = bo
            .map(&device, 0, 0, w, h, |m| f(m.buffer(), m.stride()))
            .map_err(|_| Error::Gbm("buffer belongs to another device".into()))?
            .map_err(|e| Error::Gbm(format!("map: {e}")))?;
        Ok(mapped)
    }

    /// Copy the pixels out as packed BGRA, whatever 32-bit RGB layout the
    /// compositor chose. The image is cropped to even dimensions, which the
    /// 4:2:0 codec path needs.
    pub fn read_bgra(&self) -> Result<BgraImage> {
        let width = self.info.width as usize & !1;
        let height = self.info.height as usize & !1;
        let bgra_in_memory = matches!(
            DrmFourcc::try_from(self.info.fourcc),
            Ok(DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888)
        );
        let pixels = match &self.backing {
            Backing::Shm(file) => {
                let stride = self.info.planes[0].stride;
                read_shm_bgra(file, stride, width, height, bgra_in_memory)?
            }
            Backing::Gbm { .. } => self.with_mapped(|mapped, stride| {
                let mut out = vec![0u8; width * height * 4];
                for (dst, src) in out
                    .chunks_exact_mut(width * 4)
                    .zip(mapped.chunks(stride as usize))
                {
                    if bgra_in_memory {
                        dst.copy_from_slice(&src[..width * 4]);
                    } else {
                        for (d, p) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                            d.copy_from_slice(&[p[2], p[1], p[0], p[3]]);
                        }
                    }
                }
                out
            })?,
        };
        Ok(BgraImage {
            width,
            height,
            pixels,
        })
    }
}

/// Read the top-left `width`x`height` of a wl_shm frame straight from the
/// memfd into packed rows: one copy, where `with_mapped` would read the whole
/// buffer first. RGBA layouts are swizzled in place afterwards.
fn read_shm_bgra(
    file: &File,
    stride: u32,
    width: usize,
    height: usize,
    bgra_in_memory: bool,
) -> Result<Vec<u8>> {
    let row = width * 4;
    let mut out = vec![0u8; row * height];
    if out.is_empty() {
        return Ok(out);
    }
    if stride as usize == row {
        file.read_exact_at(&mut out, 0)?;
    } else {
        for (y, dst) in out.chunks_exact_mut(row).enumerate() {
            file.read_exact_at(dst, y as u64 * u64::from(stride))?;
        }
    }
    if !bgra_in_memory {
        for p in out.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
    }
    Ok(out)
}

/// Packed BGRA pixels, `width * 4` bytes per row.
#[derive(Debug, Clone)]
pub struct BgraImage {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// A frame handed to the consumer. Dropping it returns the buffer to the ring.
pub struct CapturedFrame {
    pub buffer: Arc<CaptureBuffer>,
    pub damage: Vec<Rect>,
    /// Compositor presentation time in nanoseconds (CLOCK_MONOTONIC).
    pub presentation_ns: u64,
    pub sequence: u64,
    release: Option<Sender<thread::Cmd>>,
}

impl std::fmt::Debug for CapturedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedFrame")
            .field("buffer", &self.buffer.index)
            .field("damage", &self.damage)
            .field("sequence", &self.sequence)
            .finish()
    }
}

impl Drop for CapturedFrame {
    fn drop(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(thread::Cmd::Release {
                index: self.buffer.index,
                generation: self.buffer.generation,
            });
        }
    }
}

#[derive(Debug)]
pub enum CaptureEvent {
    /// Session constraints are known and buffers are allocated.
    Ready {
        output: OutputInfo,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
    },
    Frame(CapturedFrame),
    CursorShape {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    CursorPos {
        x: i32,
        y: i32,
        visible: bool,
    },
    Stopped,
    Error(String),
}

pub type EventSink = Box<dyn FnMut(CaptureEvent) + Send>;

/// Handle to the capture thread.
pub struct Capturer {
    cmd: Sender<thread::Cmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Capturer {
    /// Start capturing `config.output`. Events arrive on `sink` from the
    /// capture thread. Returns once the compositor connection is up.
    pub fn start(config: CaptureConfig, sink: EventSink) -> Result<Self> {
        let (cmd, join) = thread::spawn(config, sink)?;
        Ok(Self {
            cmd,
            join: Some(join),
        })
    }

    /// Ask for the next frame. Frames are only captured on demand.
    pub fn request_frame(&self) -> Result<()> {
        self.cmd
            .send(thread::Cmd::RequestFrame)
            .map_err(|_| Error::ThreadGone)
    }

    /// Abandon the in-flight capture and start a fresh full-damage one, for
    /// a first frame the compositor never completed.
    pub fn recapture(&self) -> Result<()> {
        self.cmd
            .send(thread::Cmd::Recapture)
            .map_err(|_| Error::ThreadGone)
    }

    pub fn stop(&self) {
        let _ = self.cmd.send(thread::Cmd::Stop);
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        self.stop();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// List outputs as the compositor reports them, without starting a capture.
pub fn list_outputs(target: &Target) -> Result<Vec<OutputInfo>> {
    thread::list_outputs(target)
}

/// Protocol names the capture path needs, for probing.
pub const REQUIRED_GLOBALS: &[&str] = &[
    "ext_output_image_capture_source_manager_v1",
    "ext_image_copy_capture_manager_v1",
    "zwp_linux_dmabuf_v1",
];
