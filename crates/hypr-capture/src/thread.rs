//! The capture thread: Wayland dispatch, buffer ring, session state machine.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use calloop::channel::{self, Sender};
use drm_fourcc::{DrmFourcc, DrmModifier};
use nix::sys::memfd::{memfd_create, MFdFlags};
use gbm::{BufferObjectFlags, Device};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1::{
    self as cursor_session, ExtImageCopyCaptureCursorSessionV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self as frame_proto, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    self as manager, ExtImageCopyCaptureManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self as session_proto, ExtImageCopyCaptureSessionV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;

use crate::cursor::{ShmBuffer, ShmLayout};
use crate::{
    Backing, CaptureBuffer, CaptureConfig, CaptureEvent, CaptureMode, CapturedFrame, DmabufInfo,
    Error, EventSink, OutputInfo, Plane, Rect, Result,
};
use hypr_wl::{LoopState, Outputs, Seat, Target};

pub enum Cmd {
    RequestFrame,
    /// Abandon the in-flight capture and start a fresh one with full
    /// damage, for a first frame the compositor never completed.
    Recapture,
    /// Replace the capture ring after a GPU encode failure.
    UseShm,
    Release {
        index: usize,
        generation: u64,
    },
    Stop,
}

/// Which session an object belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Screen,
    Cursor,
}

struct RingSlot {
    buffer: Arc<CaptureBuffer>,
    wl_buffer: WlBuffer,
    /// The pool behind a wl_shm buffer; `None` for dmabufs.
    pool: Option<WlShmPool>,
    busy: bool,
}

impl RingSlot {
    fn destroy(self) {
        self.wl_buffer.destroy();
        if let Some(pool) = self.pool {
            pool.destroy();
        }
    }
}

/// What a dmabuf ring needs: the protocol to wrap buffers and a GBM
/// device to allocate them.
struct Gbm {
    dmabuf: ZwpLinuxDmabufV1,
    device: Arc<Mutex<Device<File>>>,
}

#[derive(Default)]
struct Constraints {
    width: u32,
    height: u32,
    formats: Vec<(u32, Vec<u64>)>,
    shm_formats: Vec<u32>,
    done: bool,
}

struct State {
    cfg: CaptureConfig,
    sink: EventSink,
    conn: Connection,
    qh: QueueHandle<State>,
    outputs: Outputs,
    seat: Seat,
    /// `None` when the compositor offers no dmabuf or there is no render
    /// node to allocate on (Hyper-V guests): the ring is then wl_shm.
    gbm: Option<Gbm>,
    shm: Option<WlShm>,
    source_mgr: ExtOutputImageCaptureSourceManagerV1,
    copy_mgr: ExtImageCopyCaptureManagerV1,
    cmd_tx: Sender<Cmd>,

    output: Option<(WlOutput, OutputInfo)>,
    source: Option<ExtImageCaptureSourceV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    constraints: Constraints,
    ring: Vec<RingSlot>,
    ring_generation: u64,
    ring_format: Option<(u32, u64)>,
    in_flight: Option<(ExtImageCopyCaptureFrameV1, usize)>,
    pending_damage: Vec<Rect>,
    pending_presentation: u64,
    pending_transform: u32,
    want_frame: bool,
    sequence: u64,

    pointer: Option<WlPointer>,
    cursor_session: Option<ExtImageCopyCaptureCursorSessionV1>,
    cursor_capture: Option<ExtImageCopyCaptureSessionV1>,
    cursor_constraints: Constraints,
    cursor_buf: Option<ShmBuffer>,
    cursor_frame: Option<ExtImageCopyCaptureFrameV1>,
    cursor_hotspot: (i32, i32),
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    cursor_capture_dead: bool,
    cursor_restart_at: Option<std::time::Instant>,

    stopped: bool,
    quit: bool,
}

pub fn spawn(
    cfg: CaptureConfig,
    sink: EventSink,
) -> Result<(Sender<Cmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = channel::channel::<Cmd>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let tx2 = tx.clone();
    let join = std::thread::Builder::new()
        .name("hypr-capture".into())
        .spawn(move || {
            let mut sink = sink;
            // `run` reports the error through the sink itself; this send only
            // matters when it failed before signalling ready.
            if let Err(e) = run(cfg, &mut sink, tx2, rx, ready_tx.clone()) {
                let _ = ready_tx.send(Err(Error::Capture(e.to_string())));
            }
        })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((tx, join)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::ThreadGone),
    }
}

fn run(
    cfg: CaptureConfig,
    sink: &mut EventSink,
    cmd_tx: Sender<Cmd>,
    rx: channel::Channel<Cmd>,
    ready_tx: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let (conn, globals, queue) = hypr_wl::init::<State>(&cfg.target)?;
    let qh = queue.handle();
    let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=5, ()).ok();
    let source_mgr: ExtOutputImageCaptureSourceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| hypr_wl::Error::MissingGlobal("ext_output_image_capture_source_manager_v1"))?;
    let copy_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| hypr_wl::Error::MissingGlobal("ext_image_copy_capture_manager_v1"))?;
    let shm: Option<WlShm> = globals.bind(&qh, 1..=1, ()).ok();
    let outputs = Outputs::bind(&globals, &qh)?;
    let seat = Seat::bind(&globals, &qh)?;
    let gbm = match (cfg.mode, dmabuf) {
        // Asked for wl_shm: leave the render node alone, so a dmabuf the
        // compositor would allocate but never fill is never tried.
        (CaptureMode::Shm, _) => Err("wl_shm capture requested".to_string()),
        (_, None) => Err("the compositor does not offer zwp_linux_dmabuf_v1".to_string()),
        (_, Some(dmabuf)) => open_gbm(&cfg.render_node)
            .map(|device| Gbm {
                dmabuf,
                device: Arc::new(Mutex::new(device)),
            })
            .map_err(|e| e.to_string()),
    };
    let gbm = match gbm {
        Ok(gbm) => Some(gbm),
        Err(why) if cfg.mode == CaptureMode::Dmabuf => {
            return Err(Error::Capture(format!(
                "dmabuf capture unavailable ({why}) and --capture dmabuf allows no wl_shm"
            )))
        }
        Err(_) if cfg.mode == CaptureMode::Shm && shm.is_some() => {
            tracing::info!("capturing into wl_shm as requested (CPU video pipeline only)");
            None
        }
        Err(why) if shm.is_some() => {
            tracing::warn!(
                %why,
                "dmabuf capture unavailable; capturing into wl_shm (CPU video pipeline only)"
            );
            None
        }
        Err(why) => {
            return Err(Error::Capture(format!(
                "dmabuf capture unavailable ({why}) and the compositor offers no wl_shm"
            )))
        }
    };

    let mut state = State {
        sink: Box::new(|_| {}),
        conn: conn.clone(),
        qh: qh.clone(),
        outputs,
        seat,
        gbm,
        shm,
        source_mgr,
        copy_mgr,
        cmd_tx,
        output: None,
        source: None,
        session: None,
        constraints: Constraints::default(),
        ring: Vec::new(),
        ring_generation: 0,
        ring_format: None,
        in_flight: None,
        pending_damage: Vec::new(),
        pending_presentation: 0,
        pending_transform: 0,
        want_frame: false,
        sequence: 0,
        pointer: None,
        cursor_session: None,
        cursor_capture: None,
        cursor_constraints: Constraints::default(),
        cursor_buf: None,
        cursor_frame: None,
        cursor_hotspot: (0, 0),
        cursor_pos: (0, 0),
        cursor_visible: false,
        cursor_capture_dead: false,
        cursor_restart_at: None,
        stopped: false,
        quit: false,
        cfg,
    };
    std::mem::swap(&mut state.sink, sink);

    // From here the real sink lives in `state.sink`. Run the body, then always
    // swap it back out and emit any error through it, so a failure after
    // `ready_tx` (which the owner has already consumed) still reaches the owner.
    let result = run_loop(&mut state, conn, queue, rx, ready_tx);
    if let Err(e) = &result {
        state.emit(CaptureEvent::Error(e.to_string()));
    }
    state.teardown();
    // A flush only queues the destroy requests; the compositor may still
    // process a later `output remove` first and crash rendering into the
    // removed monitor (Hyprland 0.56). Block until it has handled them.
    let _ = state.conn.roundtrip();
    std::mem::swap(&mut state.sink, sink);
    result
}

fn open_gbm(render_node: &std::path::Path) -> Result<Device<File>> {
    let gbm_err = |e: std::io::Error| Error::Gbm(format!("open {}: {e}", render_node.display()));
    let file = File::options()
        .read(true)
        .write(true)
        .open(render_node)
        .map_err(gbm_err)?;
    Device::new(file).map_err(gbm_err)
}

/// The wl_shm format for the capture ring, as (DRM fourcc, wl_shm format),
/// or `None` to capture into dmabufs. `dmabuf` is the dmabuf format choice
/// (its modifiers) when a dmabuf ring is possible at all, `None` when the
/// compositor offers no dmabuf, there is no render node, or `CaptureMode::Shm`
/// asked for wl_shm. With dmabuf available, wl_shm is only used under
/// `prefer_linear` + `shm_fallback` when no format offers the linear
/// modifier (NVIDIA), and never under `CaptureMode::Dmabuf`.
fn shm_ring_format(
    cfg: &CaptureConfig,
    dmabuf: Option<Option<(u32, Vec<u64>)>>,
    offered: &[u32],
) -> Option<(u32, wl_shm::Format)> {
    if cfg.mode == CaptureMode::Dmabuf {
        return None;
    }
    if let Some(choice) = dmabuf {
        if !cfg.prefer_linear || !cfg.shm_fallback {
            return None;
        }
        if choice.is_some_and(|(_, mods)| mods == [u64::from(DrmModifier::Linear)]) {
            return None;
        }
    }
    [
        (DrmFourcc::Xrgb8888, wl_shm::Format::Xrgb8888),
        (DrmFourcc::Argb8888, wl_shm::Format::Argb8888),
        (DrmFourcc::Xbgr8888, wl_shm::Format::Xbgr8888),
        (DrmFourcc::Abgr8888, wl_shm::Format::Abgr8888),
    ]
    .into_iter()
    .find(|(_, shm)| offered.contains(&u32::from(*shm)))
    .map(|(drm, shm)| (drm as u32, shm))
}

/// Whether a failed dmabuf ring allocation should drop to wl_shm: a GBM
/// failure (QEMU virtio-gpu without 3D opens the render node but cannot
/// allocate on it) when the compositor offers wl_shm and the mode allows
/// the fallback.
fn gbm_failure_falls_back_to_shm(mode: CaptureMode, e: &Error, shm_offered: bool) -> bool {
    mode == CaptureMode::Auto && shm_offered && matches!(e, Error::Gbm(_))
}

fn run_loop(
    state: &mut State,
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    rx: channel::Channel<Cmd>,
    ready_tx: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let qh = state.qh.clone();
    queue.roundtrip(state)?;
    queue.roundtrip(state)?;
    let (output, info) = state.outputs.find(&state.cfg.output)?;
    tracing::info!(output = %info.name, w = info.width, h = info.height, scale = info.scale, "capturing output");
    state.output = Some((output.clone(), info));

    let source = state.source_mgr.create_source(&output, &qh, ());
    let options = if state.cfg.cursor_in_video {
        manager::Options::PaintCursors
    } else {
        manager::Options::empty()
    };
    let session = state
        .copy_mgr
        .create_session(&source, options, &qh, Kind::Screen);
    state.source = Some(source);
    state.session = Some(session);

    if state.cfg.cursor && !state.cfg.cursor_in_video && state.shm.is_some() {
        state.start_cursor_session();
    }
    let _ = ready_tx.send(Ok(()));
    Ok(hypr_wl::run_loop(conn, queue, rx, state)?)
}

pub fn list_outputs(target: &Target) -> Result<Vec<OutputInfo>> {
    struct S {
        outputs: Outputs,
    }
    impl Dispatch<WlRegistry, GlobalListContents> for S {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wayland_client::protocol::wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<WlOutput, ()> for S {
        fn event(
            s: &mut Self,
            o: &WlOutput,
            e: wl_output::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            s.outputs.handle(o, e);
        }
    }
    let (_conn, globals, mut queue) = hypr_wl::init::<S>(target)?;
    let qh = queue.handle();
    let mut s = S {
        outputs: Outputs::bind(&globals, &qh)?,
    };
    queue.roundtrip(&mut s)?;
    queue.roundtrip(&mut s)?;
    Ok(s.outputs.infos())
}

impl LoopState for State {
    type Cmd = Cmd;

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::RequestFrame => {
                self.want_frame = true;
                self.maybe_capture();
            }
            Cmd::Recapture => {
                if let Some((frame, _)) = self.in_flight.take() {
                    // Its slot was never marked busy, so it is reused.
                    frame.destroy();
                }
                self.want_frame = true;
                self.maybe_capture();
            }
            Cmd::UseShm => {
                if self.cfg.mode == CaptureMode::Dmabuf {
                    self.emit(CaptureEvent::Error(
                        "--capture dmabuf forbids CPU capture fallback".into(),
                    ));
                    self.stop();
                    return;
                }
                if let Some((frame, _)) = self.in_flight.take() {
                    frame.destroy();
                }
                self.cfg.mode = CaptureMode::Shm;
                self.gbm = None;
                if let Err(e) = self.allocate_ring() {
                    self.emit(CaptureEvent::Error(e.to_string()));
                    self.stop();
                    return;
                }
                self.want_frame = true;
                self.maybe_capture();
            }
            Cmd::Release { index, generation } => {
                if generation == self.ring_generation {
                    if let Some(slot) = self.ring.get_mut(index) {
                        slot.busy = false;
                    }
                }
                self.maybe_capture();
            }
            Cmd::Stop => self.stop(),
        }
    }

    fn stop(&mut self) {
        self.quit = true;
    }

    fn stop_requested(&self) -> bool {
        self.quit
    }
}

impl State {
    fn emit(&mut self, ev: CaptureEvent) {
        (self.sink)(ev);
    }

    fn teardown(&mut self) {
        if let Some((frame, _)) = self.in_flight.take() {
            frame.destroy();
        }
        self.teardown_cursor();
        for slot in self.ring.drain(..) {
            slot.destroy();
        }
        if let Some(s) = self.session.take() {
            s.destroy();
        }
        if let Some(s) = self.source.take() {
            s.destroy();
        }
        let _ = self.conn.flush();
    }

    fn choose_format(&self) -> Option<(u32, Vec<u64>)> {
        const PREFERRED: [DrmFourcc; 4] = [
            DrmFourcc::Xrgb8888,
            DrmFourcc::Argb8888,
            DrmFourcc::Xbgr8888,
            DrmFourcc::Abgr8888,
        ];
        for want in PREFERRED {
            if let Some((f, mods)) = self
                .constraints
                .formats
                .iter()
                .find(|(f, _)| *f == want as u32)
            {
                let linear = u64::from(DrmModifier::Linear);
                let mut mods = mods.clone();
                if self.cfg.prefer_linear && mods.contains(&linear) {
                    mods = vec![linear];
                }
                if mods.is_empty() {
                    mods.push(linear);
                }
                return Some((*f, mods));
            }
        }
        None
    }

    /// The wl_shm format to capture into, when the ring should be wl_shm
    /// rather than dmabuf.
    fn choose_shm_format(&self) -> Option<(u32, wl_shm::Format)> {
        self.shm.as_ref()?;
        let dmabuf = self.gbm.as_ref().map(|_| self.choose_format());
        shm_ring_format(&self.cfg, dmabuf, &self.constraints.shm_formats)
    }

    fn allocate_ring(&mut self) -> Result<()> {
        if self.constraints.width == 0 || self.constraints.height == 0 {
            // A new headless output has no mode yet. Wait for the
            // constraints that come with its first mode instead of
            // allocating an empty ring (a zero-sized wl_shm pool is a
            // protocol error that ends the connection).
            for slot in self.ring.drain(..) {
                slot.destroy();
            }
            tracing::debug!("output has no size yet; waiting before allocating the ring");
            return Ok(());
        }
        self.ring_generation += 1;
        if let Some((fourcc, format)) = self.choose_shm_format() {
            return self.allocate_shm_ring(fourcc, format);
        }
        match self.allocate_dmabuf_ring() {
            Err(e) if gbm_failure_falls_back_to_shm(self.cfg.mode, &e, self.shm.is_some()) => {
                tracing::warn!(
                    error = %e,
                    "dmabuf allocation failed; capturing into wl_shm (CPU video pipeline only)"
                );
                for slot in self.ring.drain(..) {
                    slot.destroy();
                }
                self.gbm = None;
                let (fourcc, format) = self.choose_shm_format().ok_or(e)?;
                self.allocate_shm_ring(fourcc, format)
            }
            result => result,
        }
    }

    /// A ring of GBM buffers handed to the compositor as dmabufs.
    fn allocate_dmabuf_ring(&mut self) -> Result<()> {
        let Some(gbm) = &self.gbm else {
            return Err(Error::Capture(format!(
                "no usable wl_shm format offered (got {:x?})",
                self.constraints.shm_formats
            )));
        };
        let (dmabuf, device) = (gbm.dmabuf.clone(), Arc::clone(&gbm.device));
        let (fourcc, modifiers) = self.choose_format().ok_or_else(|| {
            Error::Capture(format!(
                "no usable dmabuf format offered (got {:x?})",
                self.constraints.formats
            ))
        })?;
        let (w, h) = (self.constraints.width, self.constraints.height);
        for slot in self.ring.drain(..) {
            slot.destroy();
        }
        let format = DrmFourcc::try_from(fourcc).map_err(|e| Error::Gbm(e.to_string()))?;
        let mut chosen_modifier = None;
        for index in 0..self.cfg.buffers {
            let guard = device
                .lock()
                .map_err(|_| Error::Gbm("device mutex poisoned".into()))?;
            let bo = guard
                .create_buffer_object_with_modifiers2::<()>(
                    w,
                    h,
                    format,
                    modifiers.iter().map(|m| DrmModifier::from(*m)),
                    BufferObjectFlags::RENDERING,
                )
                .or_else(|_| {
                    guard.create_buffer_object::<()>(
                        w,
                        h,
                        format,
                        BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
                    )
                })
                .map_err(|e| Error::Gbm(format!("allocate {w}x{h} {format:?}: {e}")))?;
            drop(guard);
            let modifier: u64 = bo.modifier().map_err(|e| Error::Gbm(e.to_string()))?.into();
            let plane_count = bo.plane_count().map_err(|e| Error::Gbm(e.to_string()))?;
            let mut planes = Vec::new();
            for p in 0..plane_count as i32 {
                planes.push(Plane {
                    offset: bo.offset(p).map_err(|e| Error::Gbm(e.to_string()))?,
                    stride: bo
                        .stride_for_plane(p)
                        .map_err(|e| Error::Gbm(e.to_string()))?,
                });
            }
            let fd = bo.fd().map_err(|e| Error::Gbm(e.to_string()))?;
            let params = dmabuf.create_params(&self.qh, ());
            for (i, p) in planes.iter().enumerate() {
                params.add(
                    fd.as_fd(),
                    i as u32,
                    p.offset,
                    p.stride,
                    (modifier >> 32) as u32,
                    (modifier & 0xffff_ffff) as u32,
                );
            }
            let wl_buffer = params.create_immed(
                w as i32,
                h as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &self.qh,
                (),
            );
            params.destroy();
            chosen_modifier = Some(modifier);
            let buffer = Arc::new(CaptureBuffer {
                index,
                generation: self.ring_generation,
                info: DmabufInfo {
                    fd,
                    width: w,
                    height: h,
                    fourcc,
                    modifier,
                    planes,
                },
                backing: Backing::Gbm {
                    bo: Mutex::new(bo),
                    device: Arc::clone(&device),
                },
            });
            self.ring.push(RingSlot {
                buffer,
                wl_buffer,
                pool: None,
                busy: false,
            });
        }
        let modifier = chosen_modifier.unwrap_or(0);
        self.ring_ready(fourcc, modifier, "dmabuf");
        Ok(())
    }

    /// A ring of wl_shm buffers over memfds: the compositor does the GPU
    /// readback and the CPU path reads the pixels with `read_at`.
    fn allocate_shm_ring(&mut self, fourcc: u32, format: wl_shm::Format) -> Result<()> {
        let Some(shm) = self.shm.clone() else {
            return Err(Error::Capture("wl_shm is not available".into()));
        };
        let (w, h) = (self.constraints.width, self.constraints.height);
        let layout = ShmLayout::new(w, h)?;
        let stride = layout.row_bytes;
        for slot in self.ring.drain(..) {
            slot.destroy();
        }
        for index in 0..self.cfg.buffers {
            let fd = memfd_create(c"gliff-capture", MFdFlags::MFD_CLOEXEC)
                .map_err(|e| Error::Capture(format!("memfd: {e}")))?;
            let file = File::from(fd);
            file.set_len(layout.len)?;
            let pool = shm.create_pool(file.as_fd(), layout.size, &self.qh, ());
            let wl_buffer = pool.create_buffer(
                0,
                layout.width,
                layout.height,
                layout.stride,
                format,
                &self.qh,
                (),
            );
            let fd: OwnedFd = file.try_clone()?.into();
            let buffer = Arc::new(CaptureBuffer {
                index,
                generation: self.ring_generation,
                info: DmabufInfo {
                    fd,
                    width: w,
                    height: h,
                    fourcc,
                    modifier: u64::from(DrmModifier::Invalid),
                    planes: vec![Plane { offset: 0, stride }],
                },
                backing: Backing::Shm(file),
            });
            self.ring.push(RingSlot {
                buffer,
                wl_buffer,
                pool: Some(pool),
                busy: false,
            });
        }
        self.ring_ready(fourcc, u64::from(DrmModifier::Invalid), "shm");
        Ok(())
    }

    fn ring_ready(&mut self, fourcc: u32, modifier: u64, kind: &str) {
        let (w, h) = (self.constraints.width, self.constraints.height);
        self.ring_format = Some((fourcc, modifier));
        tracing::info!(
            w,
            h,
            kind,
            fourcc = format!("{:?}", DrmFourcc::try_from(fourcc)),
            modifier = format!("{modifier:#x}"),
            buffers = self.cfg.buffers,
            "capture ring allocated"
        );
        let output = self
            .output
            .as_ref()
            .map(|(_, i)| i.clone())
            .unwrap_or_default();
        self.emit(CaptureEvent::Ready {
            output,
            width: w,
            height: h,
            fourcc,
            modifier,
        });
    }

    fn maybe_capture(&mut self) {
        if !self.want_frame || self.in_flight.is_some() || self.stopped || self.ring.is_empty() {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(idx) = self.ring.iter().position(|s| !s.busy) else {
            return;
        };
        let frame = session.create_frame(&self.qh, Kind::Screen);
        frame.attach_buffer(&self.ring[idx].wl_buffer);
        let (w, h) = (
            self.ring[idx].buffer.info.width as i32,
            self.ring[idx].buffer.info.height as i32,
        );
        frame.damage_buffer(0, 0, w, h);
        frame.capture();
        tracing::debug!(slot = idx, "capture requested");
        self.pending_damage.clear();
        self.pending_presentation = 0;
        self.in_flight = Some((frame, idx));
        self.want_frame = false;
    }

    fn on_session_event(&mut self, kind: Kind, event: session_proto::Event) {
        tracing::debug!(?kind, ?event, "session event");
        let c = match kind {
            Kind::Screen => &mut self.constraints,
            Kind::Cursor => &mut self.cursor_constraints,
        };
        match event {
            session_proto::Event::BufferSize { width, height } => {
                c.width = width;
                c.height = height;
            }
            session_proto::Event::ShmFormat {
                format: wayland_client::WEnum::Value(f),
            } => {
                c.shm_formats.push(f as u32);
            }
            session_proto::Event::DmabufDevice { .. } => {}
            session_proto::Event::DmabufFormat { format, modifiers } => {
                let mods = modifiers
                    .chunks_exact(8)
                    .map(|b| u64::from_ne_bytes(b.try_into().unwrap_or([0; 8])))
                    .collect();
                c.formats.push((format, mods));
            }
            session_proto::Event::Done => {
                c.done = true;
                match kind {
                    Kind::Screen => {
                        let size_changed = self
                            .ring
                            .first()
                            .map(|s| {
                                s.buffer.info.width != self.constraints.width
                                    || s.buffer.info.height != self.constraints.height
                            })
                            .unwrap_or(true);
                        if size_changed {
                            if let Some((frame, _)) = self.in_flight.take() {
                                frame.destroy();
                                self.want_frame = true;
                            }
                            if let Err(e) = self.allocate_ring() {
                                self.emit(CaptureEvent::Error(e.to_string()));
                                self.quit = true;
                                return;
                            }
                        }
                        self.constraints.formats.clear();
                        self.constraints.shm_formats.clear();
                        self.maybe_capture();
                    }
                    Kind::Cursor => {
                        self.cursor_constraints.shm_formats.clear();
                        self.cursor_constraints.formats.clear();
                        // The compositor completes the in-flight frame right
                        // after new constraints; let it finish and reallocate
                        // once its result is in.
                        if self.cursor_frame.is_none() {
                            self.realloc_cursor_buffer();
                        }
                    }
                }
            }
            session_proto::Event::Stopped if kind == Kind::Screen => {
                self.stopped = true;
                self.emit(CaptureEvent::Stopped);
            }
            _ => {}
        }
    }

    fn on_frame_event(
        &mut self,
        kind: Kind,
        frame: &ExtImageCopyCaptureFrameV1,
        event: frame_proto::Event,
    ) {
        tracing::debug!(
            ?kind,
            ?event,
            in_flight = self.in_flight.as_ref().map(|(f, _)| f == frame),
            "frame event"
        );
        match kind {
            Kind::Screen => self.on_screen_frame_event(frame, event),
            Kind::Cursor => self.on_cursor_frame_event(frame, event),
        }
    }

    fn on_screen_frame_event(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: frame_proto::Event,
    ) {
        let Some((cur, idx)) = self.in_flight.as_ref() else {
            return;
        };
        if cur != frame {
            return;
        }
        let idx = *idx;
        match event {
            frame_proto::Event::Damage {
                x,
                y,
                width,
                height,
            } => self.pending_damage.push(Rect {
                x,
                y,
                width,
                height,
            }),
            frame_proto::Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let secs = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                self.pending_presentation = secs * 1_000_000_000 + tv_nsec as u64;
            }
            frame_proto::Event::Transform { transform } => {
                self.pending_transform = transform.into();
            }
            frame_proto::Event::Ready => {
                let (frame, _) = self.in_flight.take().expect("checked above");
                frame.destroy();
                self.ring[idx].busy = true;
                self.sequence += 1;
                let captured = CapturedFrame {
                    buffer: Arc::clone(&self.ring[idx].buffer),
                    damage: std::mem::take(&mut self.pending_damage),
                    presentation_ns: self.pending_presentation,
                    sequence: self.sequence,
                    transform: self.pending_transform,
                    release: Some(self.cmd_tx.clone()),
                };
                self.emit(CaptureEvent::Frame(captured));
                self.maybe_capture();
            }
            frame_proto::Event::Failed { reason } => {
                let (frame, _) = self.in_flight.take().expect("checked above");
                frame.destroy();
                tracing::warn!(?reason, "capture frame failed");
                match reason {
                    wayland_client::WEnum::Value(frame_proto::FailureReason::Stopped) => {
                        self.stopped = true;
                        self.emit(CaptureEvent::Stopped);
                    }
                    _ => {
                        // A transient failure need not be followed by fresh
                        // constraints, so re-drive capture now rather than
                        // waiting for a `done` that may never come.
                        self.want_frame = true;
                        self.maybe_capture();
                    }
                }
            }
            _ => {}
        }
    }

    fn teardown_cursor(&mut self) {
        if let Some(f) = self.cursor_frame.take() {
            f.destroy();
        }
        self.cursor_buf = None;
        if let Some(s) = self.cursor_capture.take() {
            s.destroy();
        }
        if let Some(s) = self.cursor_session.take() {
            s.destroy();
        }
        if let Some(p) = self.pointer.take() {
            p.release();
        }
        self.cursor_constraints = Constraints::default();
        self.cursor_capture_dead = false;
    }

    fn start_cursor_session(&mut self) {
        let (Some(seat), Some(source)) = (self.seat.seat.clone(), self.source.clone()) else {
            return;
        };
        let pointer = seat.get_pointer(&self.qh, ());
        let cs = self
            .copy_mgr
            .create_pointer_cursor_session(&source, &pointer, &self.qh, ());
        let capture = cs.get_capture_session(&self.qh, Kind::Cursor);
        self.pointer = Some(pointer);
        self.cursor_session = Some(cs);
        self.cursor_capture = Some(capture);
        // Hyprland drops the capture session without a word when the cursor
        // is not a compositor buffer at this moment; the sync tells us whether
        // constraints came back at all.
        self.conn.display().sync(&self.qh, CursorSync);
    }

    fn on_cursor_sync(&mut self) {
        if self.cursor_capture.is_some() && !self.cursor_constraints.done {
            tracing::debug!(
                "cursor capture session sent no constraints; will retry on cursor change"
            );
            self.cursor_capture_dead = true;
        }
    }

    /// Recreate a dead cursor capture session, at most once per second when
    /// only the position moved.
    fn maybe_restart_cursor_session(&mut self, cursor_changed: bool) {
        if !self.cursor_capture_dead {
            return;
        }
        let now = std::time::Instant::now();
        let due = self
            .cursor_restart_at
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1));
        if !cursor_changed && !due {
            return;
        }
        self.cursor_restart_at = Some(now);
        self.teardown_cursor();
        self.start_cursor_session();
    }

    fn realloc_cursor_buffer(&mut self) {
        let (w, h) = (
            self.cursor_constraints.width,
            self.cursor_constraints.height,
        );
        let same = self
            .cursor_buf
            .as_ref()
            .is_some_and(|b| b.width == w && b.height == h);
        if !same {
            self.cursor_buf = None;
            if w == 0 || h == 0 {
                return;
            }
            let Some(shm) = self.shm.as_ref() else { return };
            match ShmBuffer::new(shm, &self.qh, w, h) {
                Ok(b) => self.cursor_buf = Some(b),
                Err(e) => {
                    tracing::warn!(error = %e, "cursor buffer allocation failed");
                    return;
                }
            }
        }
        self.capture_cursor();
    }

    fn capture_cursor(&mut self) {
        if self.cursor_frame.is_some() {
            return;
        }
        let (Some(session), Some(buf)) = (self.cursor_capture.clone(), self.cursor_buf.as_ref())
        else {
            return;
        };
        let frame = session.create_frame(&self.qh, Kind::Cursor);
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, buf.width as i32, buf.height as i32);
        frame.capture();
        self.cursor_frame = Some(frame);
    }

    fn on_cursor_frame_event(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: frame_proto::Event,
    ) {
        if self.cursor_frame.as_ref() != Some(frame) {
            return;
        }
        match event {
            frame_proto::Event::Ready => {
                if let Some(f) = self.cursor_frame.take() {
                    f.destroy();
                }
                let current = self.cursor_buf.as_ref().is_some_and(|b| {
                    b.width == self.cursor_constraints.width
                        && b.height == self.cursor_constraints.height
                });
                if let Some(buf) = self.cursor_buf.as_ref().filter(|_| current) {
                    match buf.read_argb() {
                        Ok(argb) => {
                            let (hot_x, hot_y) = self.cursor_hotspot;
                            let (width, height) = (buf.width, buf.height);
                            let opaque = argb.chunks_exact(4).filter(|p| p[3] != 0).count();
                            tracing::debug!(width, height, hot_x, hot_y, opaque, "cursor shape");
                            self.emit(CaptureEvent::CursorShape {
                                width,
                                height,
                                hot_x,
                                hot_y,
                                argb,
                            });
                        }
                        Err(e) => tracing::warn!(error = %e, "cursor read failed"),
                    }
                }
                self.realloc_cursor_buffer();
            }
            frame_proto::Event::Failed { reason } => {
                if let Some(f) = self.cursor_frame.take() {
                    f.destroy();
                }
                tracing::debug!(?reason, "cursor frame failed; retrying");
                if !matches!(
                    reason,
                    wayland_client::WEnum::Value(frame_proto::FailureReason::Stopped)
                ) {
                    self.realloc_cursor_buffer();
                }
            }
            _ => {}
        }
    }

    fn on_cursor_session_event(&mut self, event: cursor_session::Event) {
        match event {
            cursor_session::Event::Enter => {
                self.maybe_restart_cursor_session(true);
                self.cursor_visible = true;
                let (x, y) = self.cursor_pos;
                self.emit(CaptureEvent::CursorPos {
                    x,
                    y,
                    visible: true,
                });
            }
            cursor_session::Event::Leave => {
                self.cursor_visible = false;
                let (x, y) = self.cursor_pos;
                self.emit(CaptureEvent::CursorPos {
                    x,
                    y,
                    visible: false,
                });
            }
            cursor_session::Event::Position { x, y } => {
                self.maybe_restart_cursor_session(false);
                self.cursor_pos = (x, y);
                let visible = self.cursor_visible;
                self.emit(CaptureEvent::CursorPos { x, y, visible });
            }
            cursor_session::Event::Hotspot { x, y } => {
                self.maybe_restart_cursor_session(true);
                self.cursor_hotspot = (x, y);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        o: &WlOutput,
        e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.outputs.handle(o, e);
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        s: &mut Self,
        _: &WlSeat,
        e: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.seat.handle(e);
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, Kind> for State {
    fn event(
        s: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        e: session_proto::Event,
        kind: &Kind,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.on_session_event(*kind, e);
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, Kind> for State {
    fn event(
        s: &mut Self,
        f: &ExtImageCopyCaptureFrameV1,
        e: frame_proto::Event,
        kind: &Kind,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.on_frame_event(*kind, f, e);
    }
}

struct CursorSync;

impl Dispatch<WlCallback, CursorSync> for State {
    fn event(
        s: &mut Self,
        _: &WlCallback,
        e: wl_callback::Event,
        _: &CursorSync,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = e {
            s.on_cursor_sync();
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ExtImageCopyCaptureCursorSessionV1,
        e: cursor_session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.on_cursor_session_event(e);
    }
}

delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlPointer);
delegate_noop!(State: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ExtImageCaptureSourceV1);
delegate_noop!(State: ExtImageCopyCaptureManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    const XRGB: u32 = DrmFourcc::Xrgb8888 as u32;
    const ARGB_SHM: u32 = wl_shm::Format::Argb8888 as u32;
    const XRGB_SHM: u32 = wl_shm::Format::Xrgb8888 as u32;

    fn cfg(prefer_linear: bool, shm_fallback: bool) -> CaptureConfig {
        let mut cfg = CaptureConfig::new("X");
        cfg.prefer_linear = prefer_linear;
        cfg.shm_fallback = shm_fallback;
        cfg
    }

    fn linear() -> u64 {
        u64::from(DrmModifier::Linear)
    }

    #[test]
    fn linear_dmabuf_keeps_the_dmabuf_ring() {
        let dmabuf = Some(Some((XRGB, vec![linear()])));
        assert_eq!(shm_ring_format(&cfg(true, true), dmabuf, &[XRGB_SHM]), None);
    }

    #[test]
    fn tiled_only_dmabuf_falls_back_to_shm_when_allowed() {
        let tiled = Some(Some((XRGB, vec![0x0300_0000_0000_0001])));
        assert_eq!(
            shm_ring_format(&cfg(true, true), tiled.clone(), &[XRGB_SHM]),
            Some((XRGB, wl_shm::Format::Xrgb8888))
        );
        assert_eq!(
            shm_ring_format(&cfg(false, true), tiled.clone(), &[XRGB_SHM]),
            None
        );
        assert_eq!(shm_ring_format(&cfg(true, false), tiled, &[XRGB_SHM]), None);
    }

    #[test]
    fn no_dmabuf_captures_into_shm_regardless_of_config() {
        // Hyper-V: no zwp_linux_dmabuf_v1 global, no render node.
        assert_eq!(
            shm_ring_format(&cfg(false, false), None, &[ARGB_SHM]),
            Some((DrmFourcc::Argb8888 as u32, wl_shm::Format::Argb8888))
        );
        assert_eq!(
            shm_ring_format(&cfg(true, true), None, &[ARGB_SHM, XRGB_SHM]),
            Some((XRGB, wl_shm::Format::Xrgb8888))
        );
    }

    #[test]
    fn gbm_allocation_failure_falls_back_to_shm_when_offered() {
        // QEMU virtio-gpu without 3D: the render node opens but cannot allocate.
        let gbm = Error::Gbm("allocate 1280x800 XR24: Permission denied".into());
        assert!(gbm_failure_falls_back_to_shm(CaptureMode::Auto, &gbm, true));
        assert!(!gbm_failure_falls_back_to_shm(
            CaptureMode::Auto,
            &gbm,
            false
        ));
        let other = Error::Capture("no usable dmabuf format offered".into());
        assert!(!gbm_failure_falls_back_to_shm(
            CaptureMode::Auto,
            &other,
            true
        ));
    }

    #[test]
    fn dmabuf_mode_never_falls_back_to_shm() {
        let mut cfg = cfg(true, true);
        cfg.mode = CaptureMode::Dmabuf;
        let tiled = Some(Some((XRGB, vec![0x0300_0000_0000_0001])));
        assert_eq!(shm_ring_format(&cfg, tiled, &[XRGB_SHM]), None);
        let gbm = Error::Gbm("allocate 1280x800 XR24: Permission denied".into());
        assert!(!gbm_failure_falls_back_to_shm(
            CaptureMode::Dmabuf,
            &gbm,
            true
        ));
    }

    #[test]
    fn shm_mode_captures_into_shm_without_a_dmabuf_choice() {
        // With `CaptureMode::Shm` the thread never opens GBM, so the ring
        // choice sees no dmabuf and takes the first usable wl_shm format.
        let mut cfg = cfg(false, false);
        cfg.mode = CaptureMode::Shm;
        assert_eq!(
            shm_ring_format(&cfg, None, &[ARGB_SHM, XRGB_SHM]),
            Some((XRGB, wl_shm::Format::Xrgb8888))
        );
        assert_eq!(shm_ring_format(&cfg, None, &[]), None);
    }

    #[test]
    fn capture_mode_parses_the_cli_value_first() {
        assert_eq!(CaptureMode::resolve(Some("shm")), CaptureMode::Shm);
        assert_eq!(CaptureMode::resolve(Some("dmabuf")), CaptureMode::Dmabuf);
        assert_eq!(CaptureMode::resolve(Some("auto")), CaptureMode::Auto);
        // An unknown value is ignored, like `VideoMode::resolve`.
        assert_eq!(CaptureMode::resolve(Some("mmap")), CaptureMode::Auto);
    }

    #[test]
    fn no_dmabuf_and_no_usable_shm_format_is_none() {
        let rgb565 = wl_shm::Format::Rgb565 as u32;
        assert_eq!(shm_ring_format(&cfg(true, true), None, &[rgb565]), None);
        assert_eq!(shm_ring_format(&cfg(true, true), None, &[]), None);
    }
}
