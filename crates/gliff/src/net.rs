//! The client's network and decode worker: connect (TCP or ssh), run the
//! protocol, decode the streams on the GPU, and hand finished display frames
//! (dmabufs) to the GTK thread. Input commands flow the other way.
//!
//! The Vulkan objects live entirely on this worker thread; only dmabuf fds
//! and plain values cross to the GTK thread.

use std::rc::Rc;
use std::sync::mpsc::{Sender as StdSender, SyncSender};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{
    features, is_incompatible, version_mismatch, ChromaMode, ClientCaps, ClientMsg, ClipboardFile,
    ClipboardMsg, Codec, ServerMsg, VideoPipeline, PROTOCOL_VERSION,
};
use gliff_sw::VideoMode;
use gliff_transport::clipboard::progress::Progress;
use gliff_transport::clipboard::{outbound_channel, Side, Transfers};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use gliff_vk::{Decoder, DisplayFrame, Gpu};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, unbounded_channel, UnboundedReceiver};

use crate::clipboard::{Bridge, ToWorker};
use crate::keymap::{Keymap, FIRST_KEYMAP_WAIT};

/// Stream geometry one picture was configured under: stream size, view
/// size, and the effective scale x1000.
type Geometry = ((u32, u32), (u32, u32), u32);

const MAX_STREAM: (u32, u32) = (3840, 2160);
// Older protocol-v1 GPU servers ignore the advertised limits on their first
// stream. Accept their bounded encoder maximum so they can reach the resize.
const MAX_DECODE: (u32, u32) = (4096, 4096);
// The largest frame the integer zoom may produce: an 8K display's worth,
// about 127 MiB as BGRA. The UI derives the zoom from the server's view, so
// the worker bounds it here. With a fixed GPU ring and a bounded frame
// channel, a server can then cost at most what a real 8K session does.
// A larger window loses only the exact integer enlargement.
const MAX_ZOOMED_PIXELS: u64 = 7680 * 4320;
// No side past this, whatever the pixel budget allows for a thin stream.
const MAX_ZOOMED_SIDE: u32 = 8192;
// Matches the cursor size the UI accepts (`remote_cursor`).
const MAX_CURSOR: u32 = 1024;

/// A decoded frame with the geometry it was configured under, so the UI
/// never paints a frame with another configuration's view or scale (frames
/// and statuses travel on separate channels).
pub struct Picture {
    pub frame: Frame,
    /// Stream size in physical pixels.
    pub stream: (u32, u32),
    /// The full-quality fit size the frame should be drawn into.
    pub view: (u32, u32),
    /// Effective stream scale x1000 for pointer mapping.
    pub scale_milli: u32,
}

/// Status/telemetry the worker reports to the UI. Pointer-mapping geometry
/// is deliberately absent: it travels with each `Picture`, so the UI never
/// maps clicks against a configuration whose frame is not on screen yet.
pub enum Status {
    Connected {
        /// "server tier→client tier", e.g. "GPU→CPU".
        video: String,
        view_width: u32,
        view_height: u32,
        fps_cap: u32,
        /// The server serves a dedicated headless output whose frames carry
        /// the compositor-drawn cursor.
        headless: bool,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
        /// "server tier→client tier", e.g. "GPU→CPU".
        video: String,
    },
    /// A new remote cursor image waits in this slot, for the client to set
    /// as its widget cursor.
    Cursor(LatestCursor),
    /// The remote selection changed: put a proxy for these on the local
    /// clipboard, or clear ours when both lists are empty. Pastes from the
    /// proxy echo `serial`, so one racing a newer offer is refused.
    ClipboardOffer {
        serial: u32,
        mime_types: Vec<String>,
        files: Vec<ClipboardFile>,
    },
    /// The server wants `mime_type` from the local clipboard: read it into
    /// `reply` chunk by chunk and drop the sender at the end.
    ClipboardRead {
        mime_type: String,
        /// The UI's generation of the offer the request was made against.
        generation: u64,
        reply: mpsc::Sender<std::io::Result<Bytes>>,
    },
    /// A paste from the server's offer that is taking a while, or ended.
    ClipboardTransfer {
        id: u32,
        progress: Progress,
    },
    Error(String),
    /// The two ends speak different protocol versions, so retrying cannot
    /// help.
    Incompatible(String),
    Closed,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Incompatible(String);

/// A remote cursor image: `argb` holds `width` x `height` BGRA pixels.
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub argb: Vec<u8>,
}

/// The newest cursor shape the UI has not taken yet. Shapes replace each
/// other here instead of queueing on the status channel, so a server that
/// sends shapes faster than the UI polls holds at most one in memory.
#[derive(Clone, Default)]
pub struct LatestCursor(Arc<Mutex<Option<CursorShape>>>);

impl LatestCursor {
    /// Store `shape` over any untaken one. True when the slot was empty, so
    /// the UI needs a fresh notice; otherwise one is already on its way.
    fn put(&self, shape: CursorShape) -> bool {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.replace(shape).is_none()
    }

    pub fn take(&self) -> Option<CursorShape> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Hand `shape` to the UI through `slot`, sending a notice only when none
/// is pending already.
fn offer_cursor(slot: &LatestCursor, shape: CursorShape, status: &StdSender<Status>) {
    if slot.put(shape) {
        let _ = status.send(Status::Cursor(slot.clone()));
    }
}

/// Read a CursorShape's payload when it is exactly the shape's pixels.
/// Otherwise `read_msg` skips the payload and the shape comes back empty,
/// which the UI treats as a shape it cannot show.
async fn read_cursor<R: AsyncRead + Unpin>(
    reader: &mut Framed<R>,
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
    argb_len: u32,
) -> Result<CursorShape, gliff_transport::Error> {
    let argb = if cursor_payload_fits(width, height, argb_len) {
        reader.read_payload(argb_len).await?.to_vec()
    } else {
        tracing::debug!(width, height, argb_len, "ignoring a malformed cursor shape");
        Vec::new()
    };
    Ok(CursorShape {
        width,
        height,
        hot_x,
        hot_y,
        argb,
    })
}

/// The stream geometry a StreamConfig sets, only made by `Config::new`,
/// which checks the server's values.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Config {
    stream: (u32, u32),
    chroma: ChromaMode,
    scale_milli: u32,
    view: (u32, u32),
    fps_cap: u32,
}

impl Config {
    fn new(
        stream: (u32, u32),
        chroma: ChromaMode,
        scale_milli: u32,
        view: (u32, u32),
        fps_cap: u32,
    ) -> anyhow::Result<Self> {
        validate_stream_size(stream.0, stream.1)?;
        validate_view_size(view, stream)?;
        Ok(Self {
            stream,
            chroma,
            scale_milli,
            view,
            fps_cap,
        })
    }

    fn geometry(&self) -> Geometry {
        (self.stream, self.view, self.scale_milli)
    }

    /// Take a later config. True when the coded stream changed and needs a
    /// new decoder. Keep the requested zoom: each frame clamps it against
    /// its actual size, without another UI request or CPU keyframe.
    fn update(&mut self, next: Config, zoom: &mut Zoom) -> bool {
        let rebuild = (next.stream, next.chroma) != (self.stream, self.chroma);
        if rebuild {
            zoom.asked_gpu = 1;
        }
        *self = next;
        rebuild
    }
}

/// The GPU decoder's zoom control, behind a trait so the rule that a
/// refused zoom is asked for only once can be tested without a GPU.
trait ZoomOutput {
    fn set_zoom(&mut self, zoom: u32) -> gliff_vk::Result<u32>;
}

impl ZoomOutput for Decoder {
    fn set_zoom(&mut self, zoom: u32) -> gliff_vk::Result<u32> {
        Decoder::set_zoom(self, zoom)
    }
}

/// The decoder output zoom: what the UI wants, bounded per frame size.
struct Zoom {
    /// Display pixels per stream pixel the UI asked for.
    wanted: u32,
    /// The zoom last asked of the current GPU decoder, granted or not, so
    /// one it refused is not asked for again on every frame. A decoder made
    /// by a StreamConfig starts at zoom 1.
    asked_gpu: u32,
}

impl Zoom {
    fn new() -> Self {
        Self {
            wanted: 1,
            asked_gpu: 1,
        }
    }

    /// Take the UI's request. False when it is the zoom already wanted.
    fn want(&mut self, zoom: u32) -> bool {
        let zoom = zoom.max(1);
        zoom != std::mem::replace(&mut self.wanted, zoom)
    }

    /// Enlarge a CPU frame by the wanted zoom, within the budget.
    fn cpu_frame(
        &self,
        f: gliff_sw::BgraFrame,
        pool: &gliff_sw::PixelPool,
    ) -> gliff_sw::PooledFrame {
        let z = clamp_zoom(self.wanted, (f.width, f.height));
        f.zoomed(z, pool)
    }

    /// Ask the GPU decoder for the wanted zoom of a `stream`-sized output,
    /// within the budget. `None` when that zoom was asked for already.
    fn sync_gpu(
        &mut self,
        d: &mut impl ZoomOutput,
        stream: (u32, u32),
    ) -> Option<gliff_vk::Result<u32>> {
        let want = clamp_zoom(self.wanted, stream);
        if want == self.asked_gpu {
            return None;
        }
        self.asked_gpu = want;
        Some(d.set_zoom(want))
    }
}

/// How to reach the server.
#[derive(Clone)]
pub enum Endpoint {
    Tcp(String),
    Ssh(SshTarget),
}

/// A decoded frame for the GTK thread: a dmabuf from the GPU tier, or plain
/// BGRA pixels from the CPU tier.
pub enum Frame {
    Dmabuf(DisplayFrame),
    Bgra(gliff_sw::PooledFrame),
}

pub struct Worker {
    pub endpoint: Endpoint,
    pub video: VideoMode,
    /// Bounded so a stalled UI thread cannot make the decoder buffer frames
    /// without limit; when full, the newest frame is dropped (latest-wins).
    pub frames: SyncSender<Picture>,
    pub status: StdSender<Status>,
    pub input: UnboundedReceiver<ToWorker>,
    pub keymap: Keymap,
}

impl Worker {
    /// Run to completion on the calling thread (spawn it yourself).
    pub fn run(self) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = self.status.send(Status::Error(e.to_string()));
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        let status = self.status.clone();
        let result: anyhow::Result<()> = local.block_on(&rt, async move {
            match self.endpoint {
                Endpoint::Tcp(ref addr) => {
                    let stream = tokio::net::TcpStream::connect(addr).await?;
                    stream.set_nodelay(true)?;
                    let (rd, wr) = tokio::io::split(stream);
                    session(
                        rd,
                        wr,
                        self.video,
                        self.frames,
                        self.status,
                        self.input,
                        self.keymap,
                    )
                    .await
                }
                Endpoint::Ssh(ref target) => {
                    let ssh = spawn_ssh(target)?;
                    session(
                        ssh.stdout,
                        ssh.stdin,
                        self.video,
                        self.frames,
                        self.status,
                        self.input,
                        self.keymap,
                    )
                    .await
                }
            }
        });
        if let Err(e) = result {
            let _ = status.send(match e.downcast::<Incompatible>() {
                Ok(Incompatible(message)) => Status::Incompatible(message),
                Err(e) => Status::Error(e.to_string()),
            });
        } else {
            let _ = status.send(Status::Closed);
        }
    }
}

/// The decode pipeline: VA-API and Vulkan compute on the GPU, or OpenH264 on
/// the CPU.
enum VideoDecoder {
    Gpu(Box<Decoder>),
    Cpu(Box<gliff_sw::Decoder>),
}

/// Open the GPU for decoding, or `None` for the CPU tier. In `Gpu` mode a
/// machine without Vulkan compute or a VA-API H.264 decoder falls back to
/// the CPU.
fn open_gpu(mode: VideoMode) -> Option<Arc<Gpu>> {
    if mode == VideoMode::Cpu {
        tracing::info!("using the CPU video pipeline as requested");
        return None;
    }
    match Gpu::open(Some(&hypr_capture::render_node(None))) {
        Ok(gpu) if gpu.can_decode() => Some(gpu),
        Ok(gpu) => {
            tracing::warn!(gpu = %gpu.name, "no VA-API H.264 decoder; falling back to the CPU pipeline");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "no usable GPU; falling back to the CPU pipeline");
            None
        }
    }
}

fn new_decoder(
    gpu: &Option<Arc<Gpu>>,
    chroma: ChromaMode,
    width: u32,
    height: u32,
) -> anyhow::Result<VideoDecoder> {
    validate_stream_size(width, height)?;
    let dual = chroma != ChromaMode::Single420;
    if let Some(gpu) = gpu {
        match Decoder::new(gpu, dual, width, height) {
            Ok(d) => return Ok(VideoDecoder::Gpu(Box::new(d))),
            Err(e) => {
                tracing::warn!(error = %e, "cannot create the GPU decoder; falling back to the CPU pipeline");
            }
        }
    }
    Ok(VideoDecoder::Cpu(Box::new(gliff_sw::Decoder::new(
        dual, width, height,
    )?)))
}

impl VideoDecoder {
    fn label(&self) -> &'static str {
        match self {
            Self::Gpu(_) => "GPU",
            Self::Cpu(_) => "CPU",
        }
    }
}

/// The tiers in use, server then client, as the stats line shows them:
/// "GPU→GPU".
fn tier_label(pipeline: Option<VideoPipeline>, decoder: &VideoDecoder) -> String {
    let server = pipeline.map_or("?", |p| match p {
        VideoPipeline::Gpu => "GPU",
        VideoPipeline::Cpu => "CPU",
    });
    format!("{server}→{}", decoder.label())
}

async fn session<R, W>(
    rd: R,
    wr: W,
    video: VideoMode,
    frames: SyncSender<Picture>,
    status: StdSender<Status>,
    mut input: UnboundedReceiver<ToWorker>,
    mut keymap: Keymap,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let gpu = open_gpu(video);
    let caps = ClientCaps {
        codecs: vec![Codec::H264],
        max_width: MAX_STREAM.0,
        max_height: MAX_STREAM.1,
        // The CPU tier asks for one 4:2:0 stream so it decodes one stream,
        // not two; the recombine also costs CPU on this side.
        chroma: if gpu.is_some() {
            vec![ChromaMode::Dual420, ChromaMode::Single420]
        } else {
            vec![ChromaMode::Single420]
        },
        features: features(),
    };
    let _ = tokio::time::timeout(FIRST_KEYMAP_WAIT, keymap.wait_for(|k| !k.is_empty())).await;
    let first_keymap = keymap.borrow_and_update().clone();
    if first_keymap.is_empty() {
        tracing::warn!("no local keymap yet; server will default to us");
    }
    tracing::debug!(bytes = first_keymap.len(), "sending keymap");
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: first_keymap,
            caps,
        })
        .await?;

    // Between HelloAck and StreamConfig the server may send a Ping; answer
    // it before anything else (even GPU setup) so the server's round-trip
    // estimate is seeded ahead of the first frame ack.
    let mut got_ack = false;
    let mut headless = false;
    let (mut cfg, pipeline) = loop {
        match reader.read_msg::<ServerMsg>().await? {
            ServerMsg::HelloAck {
                version,
                features,
                session,
                ..
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(
                        Incompatible(version_mismatch(PROTOCOL_VERSION, version).into()).into(),
                    );
                }
                tracing::debug!(?features, "server features");
                headless = session.headless;
                got_ack = true;
            }
            ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                scale_milli,
                pipeline,
                view_width,
                view_height,
                fps_cap,
                ..
            } if got_ack => {
                let cfg = Config::new(
                    (width, height),
                    chroma,
                    scale_milli,
                    (view_width, view_height),
                    fps_cap,
                )?;
                break (cfg, pipeline);
            }
            ServerMsg::Error { code, message, .. } if is_incompatible(code) => {
                return Err(Incompatible(message).into())
            }
            ServerMsg::Error { code, message, .. } => {
                anyhow::bail!("server error {code}: {message}")
            }
            other => anyhow::bail!("unexpected message before StreamConfig: {other:?}"),
        }
    };
    let mut decoder = new_decoder(&gpu, cfg.chroma, cfg.stream.0, cfg.stream.1)?;
    let mut video_label = tier_label(pipeline, &decoder);
    let _ = status.send(Status::Connected {
        video: video_label.clone(),
        view_width: cfg.view.0,
        view_height: cfg.view.1,
        fps_cap: cfg.fps_cap,
        headless,
    });
    let decode_on = match (&decoder, &gpu) {
        (VideoDecoder::Gpu(_), Some(g)) => g.name.as_str(),
        _ => "cpu",
    };
    let ((width, height), scale_milli) = (cfg.stream, cfg.scale_milli);
    tracing::info!(width, height, scale_milli, decode_on, video = %video_label, "connected");
    let mut logged_first = false;

    // Writes run on their own task, fed by `out_tx`, so reads (draining video)
    // never block on a write and the two peers cannot deadlock. Both the reader
    // loop (acks, keyframe requests) and the UI thread (input) feed `out_tx`.
    let (out_tx, mut out_rx) = unbounded_channel::<(ClientMsg, Bytes)>();
    let mut writes = tokio::task::spawn_local(async move {
        while let Some((m, payload)) = out_rx.recv().await {
            if payload.is_empty() {
                writer.write_msg(&m).await?;
            } else {
                writer.write_msg_with_payloads(&m, &[&payload]).await?;
            }
        }
        Ok::<(), gliff_transport::Error>(())
    });
    // Clipboard transfers write through the same channel, in chunks.
    let (clip_out_tx, mut clip_out_rx) = outbound_channel();
    let clipboard = Rc::new(Bridge::new(
        Transfers::new(clip_out_tx, Side::Client),
        status.clone(),
    ));
    {
        let out_tx = out_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some((m, payload)) = clip_out_rx.recv().await {
                if out_tx.send((ClientMsg::from(m), payload)).is_err() {
                    break;
                }
            }
        });
    }
    // Forward UI input into the same write channel; clipboard commands from
    // the UI go to the bridge. The UI closes its end when it switches to
    // another machine; `ui_gone` then ends this session and, with it, the
    // ssh child. Keymap changes share the task and go first, so a key from
    // a newly used keyboard never reaches the server ahead of its keymap.
    let (ui_gone_tx, mut ui_gone) = tokio::sync::oneshot::channel::<()>();
    // Display pixels per stream pixel the UI wants; the main loop applies it.
    let (zoom_tx, mut zoom_rx) = unbounded_channel::<u32>();
    {
        let out_tx = out_tx.clone();
        let clipboard = clipboard.clone();
        tokio::task::spawn_local(async move {
            let mut follow_keymap = true;
            loop {
                let msg = tokio::select! {
                    biased;
                    changed = keymap.changed(), if follow_keymap => {
                        if changed.is_err() {
                            follow_keymap = false;
                            continue;
                        }
                        let text = keymap.borrow_and_update().clone();
                        tracing::debug!(bytes = text.len(), "sending changed keymap");
                        ClientMsg::Keymap { keymap: text }
                    }
                    cmd = input.recv() => match cmd {
                        Some(ToWorker::Send(m)) => m,
                        Some(ToWorker::Zoom(z)) => {
                            let _ = zoom_tx.send(z);
                            continue;
                        }
                        Some(other) => {
                            clipboard.on_ui(other);
                            continue;
                        }
                        None => break,
                    },
                };
                if out_tx.send((msg, Bytes::new())).is_err() {
                    return;
                }
            }
            let _ = ui_gone_tx.send(());
        });
    }

    let mut frames_since = 0u32;
    let mut bytes_since = 0u64;
    let mut decode_ms_acc = 0f32;
    let mut last_report = std::time::Instant::now();

    // The CPU decoder holds the newest picture of a High-profile
    // (GPU-encoded) stream until the next access unit arrives. When no video
    // frame has arrived for a while, drain it so the screen shows the latest
    // state. The deadline follows video frames only — cursor and other
    // messages must not postpone it — and a frame the full UI channel
    // rejected is retried at the same cadence.
    const IDLE_DRAIN: std::time::Duration = std::time::Duration::from_millis(150);
    let mut drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
    let mut undelivered: Option<Picture> = None;
    // Geometry of the access units inside the decoder, oldest first. The CPU
    // decoder returns the PREVIOUS access unit's picture, and a scale-only
    // config can land in between: a picture must carry the geometry of the
    // config it was encoded under, not whatever is current when it emerges.
    let mut in_decoder: std::collections::VecDeque<Geometry> = std::collections::VecDeque::new();
    let mut zoom = Zoom::new();
    let cursor = LatestCursor::default();
    let pool = gliff_sw::PixelPool::default();
    // Geometry of the picture the GPU decoder holds, for a redraw: a
    // geometry-only StreamConfig may have moved `view` on since.
    let mut last_gpu_geometry: Option<Geometry> = None;
    loop {
        let read = tokio::select! {
            read = reader.read_msg::<ServerMsg>() => read,
            _ = &mut ui_gone => return Ok(()),
            Some(z) = zoom_rx.recv() => {
                if !zoom.want(z) {
                    continue;
                }
                // A still screen sends no frame, so redraw the last picture
                // at the new zoom now. The CPU tier has no copy to redraw
                // and asks for a keyframe instead.
                let redrawn = match &mut decoder {
                    VideoDecoder::Gpu(d) => match zoom.sync_gpu(d.as_mut(), cfg.stream) {
                        None => None,
                        Some(set) => match set.and_then(|_| d.redraw()) {
                            Ok(frame) => {
                                tracing::info!(zoom = d.zoom(), "decoder output zoom");
                                frame.map(Frame::Dmabuf)
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "could not change the output zoom");
                                None
                            }
                        },
                    },
                    VideoDecoder::Cpu(_) => {
                        let _ = out_tx.send((ClientMsg::RequestKeyframe, Bytes::new()));
                        None
                    }
                };
                if let Some(frame) = redrawn {
                    let (stream, view, scale_milli) =
                        last_gpu_geometry.unwrap_or(cfg.geometry());
                    let picture = Picture {
                        frame,
                        stream,
                        view,
                        scale_milli,
                    };
                    // The redraw supersedes any older frame still waiting.
                    match frames.try_send(picture) {
                        Ok(()) => undelivered = None,
                        Err(std::sync::mpsc::TrySendError::Full(picture)) => {
                            undelivered = Some(picture)
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
                    }
                }
                continue;
            }
            written = &mut writes => {
                written.context("writer task")?.context("write to server")?;
                return Ok(());
            }
            _ = tokio::time::sleep_until(drain_at),
                if undelivered.is_some()
                    || matches!(&decoder, VideoDecoder::Cpu(d) if d.has_pending()) =>
            {
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                let picture = match undelivered.take() {
                    Some(picture) => Some(picture),
                    None => match &mut decoder {
                        VideoDecoder::Cpu(d) => match d.flush() {
                            Ok(frame) => frame.map(|f| {
                                let (stream, view, scale_milli) = in_decoder
                                    .pop_front()
                                    .unwrap_or(cfg.geometry());
                                Picture {
                                    frame: Frame::Bgra(zoom.cpu_frame(f, &pool)),
                                    stream,
                                    view,
                                    scale_milli,
                                }
                            }),
                            Err(e) => {
                                // As for a decode error: the picture is
                                // gone, and so is its geometry entry.
                                tracing::warn!(error = %e, "idle drain failed; requesting keyframe");
                                in_decoder.clear();
                                let _ = out_tx.send((ClientMsg::RequestKeyframe, Bytes::new()));
                                None
                            }
                        },
                        VideoDecoder::Gpu(_) => None,
                    },
                };
                if let Some(picture) = picture {
                    if let Err(std::sync::mpsc::TrySendError::Full(picture)) =
                        frames.try_send(picture)
                    {
                        undelivered = Some(picture);
                    }
                }
                continue;
            }
        };
        let msg = match read {
            Ok(m) => m,
            Err(gliff_transport::Error::Closed) => break,
            Err(e) => return Err(e.into()),
        };
        match msg {
            ServerMsg::VideoFrame {
                frame_id,
                keyframe: _,
                data_len,
                aux_len,
                ..
            } => {
                let main = reader.read_payload(data_len).await?;
                let aux = if aux_len > 0 {
                    reader.read_payload(aux_len).await?
                } else {
                    bytes::Bytes::new()
                };
                bytes_since += (data_len + aux_len) as u64;
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                in_decoder.push_back(cfg.geometry());
                let t0 = std::time::Instant::now();
                let decoded = match &mut decoder {
                    VideoDecoder::Gpu(d) => {
                        if let Some(Err(e)) = zoom.sync_gpu(d.as_mut(), cfg.stream) {
                            tracing::warn!(error = %e, "could not change the output zoom");
                        }
                        d.decode(&main, &aux)
                            .map(|f| f.map(Frame::Dmabuf))
                            .map_err(anyhow::Error::from)
                    }
                    VideoDecoder::Cpu(d) => d
                        .decode(&main, &aux)
                        .map(|f| f.map(|f| Frame::Bgra(zoom.cpu_frame(f, &pool))))
                        .map_err(anyhow::Error::from),
                };
                let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
                // Ack immediately so the server keeps pacing.
                let _ = out_tx.send((
                    ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_ms(),
                    },
                    Bytes::new(),
                ));
                match decoded {
                    Ok(Some(frame)) => {
                        if !logged_first {
                            tracing::info!("first frame decoded");
                            logged_first = true;
                        }
                        // Latest-wins: a frame the full UI channel rejects
                        // waits in `undelivered` (superseding any drained
                        // one) and is retried at the drain cadence. The
                        // geometry travels with the frame, taken from the
                        // access unit that produced this picture (the GPU
                        // decoder returns the one just fed; the CPU decoder
                        // returns the previous one).
                        let (stream, pic_view, pic_scale) =
                            in_decoder.pop_front().unwrap_or(cfg.geometry());
                        if matches!(frame, Frame::Dmabuf(_)) {
                            last_gpu_geometry = Some((stream, pic_view, pic_scale));
                        }
                        let picture = Picture {
                            frame,
                            stream,
                            view: pic_view,
                            scale_milli: pic_scale,
                        };
                        match frames.try_send(picture) {
                            Ok(()) => undelivered = None,
                            Err(std::sync::mpsc::TrySendError::Full(picture)) => {
                                undelivered = Some(picture)
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
                        }
                        frames_since += 1;
                        decode_ms_acc += dec_ms;
                    }
                    Ok(None) => {
                        // The GPU decoder never buffers across calls: no
                        // picture means this access unit produced none, so
                        // its geometry entry goes with it. The CPU decoder
                        // holds it (drained later or by the next decode).
                        if matches!(decoder, VideoDecoder::Gpu(_)) {
                            in_decoder.pop_front();
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "decode error; requesting keyframe");
                        in_decoder.clear();
                        let _ = out_tx.send((ClientMsg::RequestKeyframe, Bytes::new()));
                    }
                }
            }
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                scale_milli,
                pipeline,
                view_width,
                view_height,
                fps_cap,
                ..
            } => {
                let next = Config::new(
                    (width, height),
                    chroma,
                    scale_milli,
                    (view_width, view_height),
                    fps_cap,
                )?;
                // A pace-only change (fps cap) must not reset the decoder;
                // a coded-stream change does, and drops the frame waiting
                // for the UI with it.
                if cfg.update(next, &mut zoom) {
                    decoder = new_decoder(&gpu, chroma, width, height)?;
                    undelivered = None;
                    in_decoder.clear();
                }
                video_label = tier_label(pipeline, &decoder);
                let _ = status.send(Status::Connected {
                    video: video_label.clone(),
                    view_width,
                    view_height,
                    fps_cap,
                    headless,
                });
            }
            ServerMsg::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb_len,
                ..
            } => {
                let shape = read_cursor(&mut reader, width, height, hot_x, hot_y, argb_len).await?;
                offer_cursor(&cursor, shape, &status);
            }
            ServerMsg::Ping { t } => {
                let _ = out_tx.send((ClientMsg::Pong { t }, Bytes::new()));
            }
            ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } => {}
            ServerMsg::Error { code, message, .. } if is_incompatible(code) => {
                return Err(Incompatible(message).into())
            }
            ServerMsg::Error { code, message, .. } => {
                anyhow::bail!("server error {code}: {message}")
            }
            ServerMsg::HelloAck { .. } => {}
            other => {
                if let Ok(clip) = other.into_clipboard() {
                    let payload = match &clip {
                        ClipboardMsg::Data { data_len, .. } => {
                            if *data_len as usize > CHUNK {
                                anyhow::bail!(
                                    "clipboard chunk of {data_len} bytes exceeds the limit"
                                );
                            }
                            reader.read_payload(*data_len).await?
                        }
                        _ => Bytes::new(),
                    };
                    clipboard.on_peer_msg(clip, payload);
                }
            }
        }
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let secs = last_report.elapsed().as_secs_f32();
            let _ = status.send(Status::Stats {
                fps: frames_since as f32 / secs,
                mbit: bytes_since as f32 * 8.0 / 1_000_000.0 / secs,
                decode_ms: if frames_since > 0 {
                    decode_ms_acc / frames_since as f32
                } else {
                    0.0
                },
                video: video_label.clone(),
            });
            frames_since = 0;
            bytes_since = 0;
            decode_ms_acc = 0.0;
            last_report = std::time::Instant::now();
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_stream_size(width: u32, height: u32) -> anyhow::Result<()> {
    anyhow::ensure!(width > 0 && height > 0 && width <= MAX_DECODE.0 && height <= MAX_DECODE.1
        && width.is_multiple_of(2) && height.is_multiple_of(2),
        "invalid stream dimensions {width}x{height}; expected even dimensions within 4096x4096 decoder limit");
    Ok(())
}

/// Servers send the full-quality size the stream is scaled down from as
/// the view, so it is never smaller than the stream. Allowing half the
/// stream is conservative slack that still refuses a tiny view, which
/// would drive the UI's integer zoom to hundreds.
fn validate_view_size(view: (u32, u32), stream: (u32, u32)) -> anyhow::Result<()> {
    let (vw, vh) = view;
    anyhow::ensure!(
        vw > 0
            && vh > 0
            && vw <= MAX_DECODE.0
            && vh <= MAX_DECODE.1
            && u64::from(vw) * 2 >= u64::from(stream.0)
            && u64::from(vh) * 2 >= u64::from(stream.1),
        "invalid view {vw}x{vh} for a {}x{} stream",
        stream.0,
        stream.1
    );
    Ok(())
}

/// The largest zoom up to `zoom` that keeps a `size` frame within
/// `MAX_ZOOMED_PIXELS` and `MAX_ZOOMED_SIDE`, and at least 1.
fn clamp_zoom(zoom: u32, size: (u32, u32)) -> u32 {
    let (w, h) = (u64::from(size.0.max(1)), u64::from(size.1.max(1)));
    let side = u64::from(MAX_ZOOMED_SIDE);
    let by_side = (side / w).min(side / h);
    let by_pixels = (MAX_ZOOMED_PIXELS / (w * h)).isqrt();
    u64::from(zoom).clamp(1, by_side.min(by_pixels).max(1)) as u32
}

/// Whether a cursor shape's payload is exactly its tightly packed BGRA
/// pixels, within the size the UI accepts.
fn cursor_payload_fits(width: u32, height: u32, argb_len: u32) -> bool {
    (1..=MAX_CURSOR).contains(&width)
        && (1..=MAX_CURSOR).contains(&height)
        && u64::from(argb_len) == u64::from(width) * u64::from(height) * 4
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn reject_invalid_stream_geometry_before_creating_a_decoder() {
        for size in [
            (0, 1080),
            (1920, 0),
            (4098, 2160),
            (3840, 4098),
            (u32::MAX, 2),
            (1919, 1080),
        ] {
            assert!(validate_stream_size(size.0, size.1).is_err());
        }
        assert!(validate_stream_size(3840, 2160).is_ok());
        assert!(validate_stream_size(2, 2).is_ok());
    }

    #[test]
    fn accepts_legacy_server_startup_before_resize() {
        assert!(validate_stream_size(4096, 1152).is_ok());
        assert!(validate_stream_size(4096, 2304).is_ok());
        assert!(validate_stream_size(4096, 4096).is_ok());
    }

    #[test]
    fn reject_a_view_that_would_drive_an_unbounded_zoom() {
        for (view, stream) in [
            ((2, 2), (1920, 1080)),
            ((0, 1080), (1920, 1080)),
            ((1920, 0), (1920, 1080)),
            ((4098, 2160), (3840, 2160)),
            ((1920, 4098), (1920, 1080)),
            ((958, 540), (1920, 1080)),
            ((u32::MAX, u32::MAX), (2, 2)),
        ] {
            assert!(
                validate_view_size(view, stream).is_err(),
                "{view:?} {stream:?}"
            );
        }
        // What servers send: the view equals the stream, or the stream is
        // the ladder's half-size step of it, with rounding either way.
        assert!(validate_view_size((1920, 1080), (1920, 1080)).is_ok());
        assert!(validate_view_size((1920, 1080), (960, 540)).is_ok());
        assert!(validate_view_size((1922, 1082), (960, 540)).is_ok());
        assert!(validate_view_size((960, 540), (1920, 1080)).is_ok());
        assert!(validate_view_size((2, 2), (2, 2)).is_ok());
    }

    #[test]
    fn zoom_keeps_the_output_within_bounds() {
        // 1920x1080 at zoom 4 is exactly 8K.
        assert_eq!(clamp_zoom(540, (1920, 1080)), 4);
        assert_eq!(clamp_zoom(2, (1920, 1080)), 2);
        assert_eq!(clamp_zoom(3, (4096, 4096)), 1);
        assert_eq!(clamp_zoom(0, (1920, 1080)), 1);
        assert_eq!(clamp_zoom(u32::MAX, (2, 2)), 2880);
        // A thin stream is held by the side bound.
        assert_eq!(clamp_zoom(100, (4096, 2)), 2);
        // A frame already past the bound is left at zoom 1.
        assert_eq!(clamp_zoom(4, (u32::MAX, u32::MAX)), 1);
    }

    fn config(stream: (u32, u32), view: (u32, u32)) -> anyhow::Result<Config> {
        Config::new(stream, ChromaMode::Dual420, 1000, view, 60)
    }

    #[test]
    fn initial_and_later_configs_reject_a_bad_view() {
        // Both StreamConfig paths can only get a Config through Config::new.
        assert!(config((1920, 1080), (2, 2)).is_err());
        assert!(config((1920, 1080), (0, 1080)).is_err());
        assert!(config((1919, 1080), (1920, 1080)).is_err());
        assert!(config((1920, 1080), (1920, 1080)).is_ok());
    }

    #[test]
    fn a_new_stream_size_preserves_the_requested_zoom() {
        let mut cfg = config((1920, 1080), (1920, 1080)).unwrap();
        let mut zoom = Zoom::new();
        assert!(zoom.want(4));
        assert!(!zoom.want(4));
        // Pace and scale changes keep the decoder and the zoom.
        let mut next = cfg;
        next.fps_cap = 15;
        next.scale_milli = 1250;
        assert!(!cfg.update(next, &mut zoom));
        assert_eq!(zoom.wanted, 4);
        // A chroma change rebuilds the decoder at zoom 1 but keeps the wish.
        zoom.asked_gpu = 4;
        next.chroma = ChromaMode::Single420;
        assert!(cfg.update(next, &mut zoom));
        assert_eq!((zoom.wanted, zoom.asked_gpu), (4, 1));
        // A larger stream keeps the wish but applies its own safe clamp.
        let big = config((4096, 4096), (4096, 4096)).unwrap();
        assert!(cfg.update(big, &mut zoom));
        assert_eq!((zoom.wanted, zoom.asked_gpu), (4, 1));
        assert_eq!(clamp_zoom(zoom.wanted, big.stream), 1);
        assert_eq!(cfg, big);
    }

    #[test]
    fn cpu_frames_are_zoomed_within_the_budget() {
        // The decoded and the idle-drained CPU frames both go through
        // Zoom::cpu_frame.
        let pool = gliff_sw::PixelPool::default();
        let mut zoom = Zoom::new();
        zoom.want(540);
        let frame = |width: u32, height: u32| gliff_sw::BgraFrame {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        };
        let small = zoom.cpu_frame(frame(64, 36), &pool);
        assert_eq!((small.width, small.height), (64 * 120, 36 * 120));
        // 4000x4000 at zoom 2 would be 256 MB; it stays unzoomed.
        zoom.want(2);
        let big = zoom.cpu_frame(frame(4000, 4000), &pool);
        assert_eq!((big.width, big.height), (4000, 4000));
    }

    struct FakeOutput {
        asked: Vec<u32>,
        refuse: bool,
    }

    impl ZoomOutput for FakeOutput {
        fn set_zoom(&mut self, zoom: u32) -> gliff_vk::Result<u32> {
            self.asked.push(zoom);
            if self.refuse {
                Err(gliff_vk::Error::Unsupported("too large".into()))
            } else {
                Ok(zoom)
            }
        }
    }

    #[test]
    fn a_refused_gpu_zoom_is_not_asked_for_again() {
        let mut gpu = FakeOutput {
            asked: Vec::new(),
            refuse: true,
        };
        let mut zoom = Zoom::new();
        assert!(zoom.sync_gpu(&mut gpu, (1920, 1080)).is_none());
        zoom.want(2);
        assert!(matches!(
            zoom.sync_gpu(&mut gpu, (1920, 1080)),
            Some(Err(_))
        ));
        // Every later frame at the same zoom leaves the decoder alone.
        assert!(zoom.sync_gpu(&mut gpu, (1920, 1080)).is_none());
        assert!(zoom.sync_gpu(&mut gpu, (1920, 1080)).is_none());
        gpu.refuse = false;
        zoom.want(540);
        assert!(matches!(zoom.sync_gpu(&mut gpu, (1920, 1080)), Some(Ok(4))));
        assert_eq!(gpu.asked, [2, 4]);
    }

    #[test]
    fn cursor_payload_must_match_its_size() {
        assert!(cursor_payload_fits(24, 24, 24 * 24 * 4));
        assert!(cursor_payload_fits(1024, 1024, 1024 * 1024 * 4));
        assert!(!cursor_payload_fits(24, 24, 24 * 24 * 4 + 1));
        assert!(!cursor_payload_fits(24, 24, 64 << 20));
        assert!(!cursor_payload_fits(0, 24, 0));
        assert!(!cursor_payload_fits(1025, 1, 1025 * 4));
        assert!(!cursor_payload_fits(u32::MAX, u32::MAX, 4));
    }

    fn shape(width: u32) -> CursorShape {
        CursorShape {
            width,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb: vec![0; width as usize * 4],
        }
    }

    #[test]
    fn cursor_notices_coalesce_to_the_latest_shape() {
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = LatestCursor::default();
        for width in 1..=3 {
            offer_cursor(&slot, shape(width), &tx);
        }
        let notices: Vec<Status> = rx.try_iter().collect();
        assert_eq!(notices.len(), 1);
        let Status::Cursor(latest) = &notices[0] else {
            panic!("expected a cursor notice");
        };
        assert_eq!(latest.take().map(|s| s.width), Some(3));
        assert!(latest.take().is_none());
        // Once taken, the next shape sends a fresh notice.
        offer_cursor(&slot, shape(4), &tx);
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[tokio::test]
    async fn a_malformed_cursor_is_skipped_and_the_next_message_read() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut wr = Framed::new(a);
        let mut rd = Framed::new(b);
        let cursor = |argb_len: u32| ServerMsg::CursorShape {
            id: 1,
            width: 24,
            height: 24,
            hot_x: 3,
            hot_y: 3,
            argb_len,
        };
        let bad = vec![1u8; 24 * 24 * 4 + 4];
        let good = vec![2u8; 24 * 24 * 4];
        wr.write_msg_with_payloads(&cursor(bad.len() as u32), &[&bad])
            .await
            .unwrap();
        wr.write_msg(&ServerMsg::Ping { t: 7 }).await.unwrap();
        wr.write_msg_with_payloads(&cursor(good.len() as u32), &[&good])
            .await
            .unwrap();

        let ServerMsg::CursorShape { argb_len, .. } = rd.read_msg().await.unwrap() else {
            panic!("expected a cursor shape");
        };
        let shape = read_cursor(&mut rd, 24, 24, 3, 3, argb_len).await.unwrap();
        assert!(shape.argb.is_empty());
        assert!(matches!(
            rd.read_msg().await.unwrap(),
            ServerMsg::Ping { t: 7 }
        ));
        let ServerMsg::CursorShape { argb_len, .. } = rd.read_msg().await.unwrap() else {
            panic!("expected a cursor shape");
        };
        let shape = read_cursor(&mut rd, 24, 24, 3, 3, argb_len).await.unwrap();
        assert_eq!(shape.argb, good);
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use gliff_proto::SessionInfo;
    use std::future::Future;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
    use tokio::sync::mpsc::UnboundedSender;

    const WAIT: Duration = Duration::from_secs(5);

    /// The server end of a real worker session on the CPU tier, and the
    /// UI's channels to it.
    struct FakeServer {
        rd: Framed<ReadHalf<DuplexStream>>,
        wr: Framed<WriteHalf<DuplexStream>>,
        status: Receiver<Status>,
        input: UnboundedSender<ToWorker>,
        frames: Receiver<Picture>,
        _keymap: tokio::sync::watch::Sender<String>,
    }

    impl FakeServer {
        /// Answer the worker's Hello and send the first StreamConfig.
        async fn handshake(&mut self, config: ServerMsg) {
            let hello = self.rd.read_msg::<ClientMsg>().await.unwrap();
            assert!(matches!(hello, ClientMsg::Hello { .. }), "{hello:?}");
            let ack = ServerMsg::HelloAck {
                version: PROTOCOL_VERSION,
                session: SessionInfo {
                    headless: false,
                    output: "test".into(),
                },
                outputs: Vec::new(),
                features: features(),
            };
            self.wr.write_msg(&ack).await.unwrap();
            self.wr.write_msg(&config).await.unwrap();
        }

        /// The next client message `want` accepts, skipping others.
        async fn expect(&mut self, want: impl Fn(&ClientMsg) -> bool) -> ClientMsg {
            tokio::time::timeout(WAIT, async {
                loop {
                    let msg = self.rd.read_msg::<ClientMsg>().await.unwrap();
                    if want(&msg) {
                        return msg;
                    }
                }
            })
            .await
            .expect("the expected client message")
        }

        /// Statuses so far, waiting until one `until` accepts.
        async fn statuses_until(&self, until: impl Fn(&Status) -> bool) -> Vec<Status> {
            let mut seen = Vec::new();
            tokio::time::timeout(WAIT, async {
                loop {
                    while let Ok(s) = self.status.try_recv() {
                        let done = until(&s);
                        seen.push(s);
                        if done {
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the expected status");
            seen
        }
    }

    /// Run the real `session` on the CPU tier against `server` and return
    /// how the session ended. A session that ends on its own stops the
    /// server early; otherwise dropping the server closes the UI's input,
    /// which ends the session.
    fn with_session<F, Fut>(server: F) -> anyhow::Result<()>
    where
        F: FnOnce(FakeServer) -> Fut,
        Fut: Future<Output = ()>,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async move {
            let (client, end) = tokio::io::duplex(1 << 20);
            let (crd, cwr) = tokio::io::split(client);
            let (srd, swr) = tokio::io::split(end);
            let (frames_tx, frames_rx) = std::sync::mpsc::sync_channel(2);
            let (status_tx, status_rx) = std::sync::mpsc::channel();
            let (input_tx, input_rx) = unbounded_channel();
            let (keymap_tx, keymap_rx) = tokio::sync::watch::channel("keymap".to_string());
            let mut worker = tokio::task::spawn_local(session(
                crd,
                cwr,
                VideoMode::Cpu,
                frames_tx,
                status_tx,
                input_rx,
                keymap_rx,
            ));
            let server = server(FakeServer {
                rd: Framed::new(srd),
                wr: Framed::new(swr),
                status: status_rx,
                input: input_tx,
                frames: frames_rx,
                _keymap: keymap_tx,
            });
            let ended = tokio::time::timeout(WAIT, async {
                tokio::select! {
                    ended = &mut worker => Some(ended),
                    _ = server => None,
                }
            })
            .await
            .expect("the session or the server finishes");
            let ended = match ended {
                Some(ended) => ended,
                None => tokio::time::timeout(WAIT, worker)
                    .await
                    .expect("the session ends"),
            };
            ended.unwrap()
        })
    }

    fn config(stream: (u32, u32), view: (u32, u32)) -> ServerMsg {
        ServerMsg::StreamConfig {
            codec: Codec::H264,
            chroma: ChromaMode::Single420,
            pipeline: Some(VideoPipeline::Cpu),
            width: stream.0,
            height: stream.1,
            scale_milli: 1000,
            extradata: Vec::new(),
            aux_extradata: None,
            view_width: view.0,
            view_height: view.1,
            fps_cap: 60,
        }
    }

    fn connected(s: &Status) -> bool {
        matches!(s, Status::Connected { .. })
    }

    #[test]
    fn a_bad_first_view_ends_the_session() {
        let ended = with_session(|mut server| async move {
            server.handshake(config((1920, 1080), (2, 2))).await;
            // Stay open until the session ends on its own.
            std::future::pending::<()>().await;
        });
        let e = ended.expect_err("a 2x2 view is refused");
        assert!(e.to_string().contains("invalid view 2x2"), "{e}");
    }

    #[test]
    fn a_bad_later_view_ends_the_session() {
        let ended = with_session(|mut server| async move {
            server.handshake(config((1920, 1080), (1920, 1080))).await;
            server.statuses_until(connected).await;
            server
                .wr
                .write_msg(&config((1920, 1080), (2, 2)))
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let e = ended.expect_err("a 2x2 view is refused");
        assert!(e.to_string().contains("invalid view 2x2"), "{e}");
    }

    #[test]
    fn a_decoded_cpu_frame_respects_the_zoom_budget() {
        let (w, h) = (320, 240);
        let mut encoder = gliff_sw::Encoder::new(
            gliff_sw::EncoderSettings {
                width: w,
                height: h,
                bitrate: 2_000_000,
                framerate: 30,
            },
            false,
        )
        .unwrap();
        let bgra: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        let packet = encoder.encode_bgra(&bgra, true).unwrap();
        with_session(|mut server| async move {
            server.handshake(config((w, h), (w, h))).await;
            server.statuses_until(connected).await;
            // A window-sized zoom for a tiny view, far past the budget.
            let _ = server.input.send(ToWorker::Zoom(1000));
            server
                .expect(|m| matches!(m, ClientMsg::RequestKeyframe))
                .await;
            let frame = ServerMsg::VideoFrame {
                frame_id: 1,
                pts_us: 0,
                keyframe: true,
                damage: Vec::new(),
                data_len: packet.main.len() as u32,
                aux_len: 0,
            };
            server
                .wr
                .write_msg_with_payloads(&frame, &[&packet.main])
                .await
                .unwrap();
            server
                .expect(|m| matches!(m, ClientMsg::FrameAck { frame_id: 1, .. }))
                .await;
            let picture = tokio::time::timeout(WAIT, async {
                loop {
                    if let Ok(p) = server.frames.try_recv() {
                        return p;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("a decoded picture");
            let Frame::Bgra(f) = picture.frame else {
                panic!("the CPU tier delivers BGRA");
            };
            // 320x240 fits 20 times within 7680x4320 worth of pixels.
            assert_eq!((f.width, f.height), (w * 20, h * 20));
            assert!(u64::from(f.width) * u64::from(f.height) <= MAX_ZOOMED_PIXELS);
            assert_eq!(picture.stream, (w, h));
        })
        .unwrap();
    }

    #[test]
    fn a_new_stream_size_does_not_request_an_extra_keyframe() {
        with_session(|mut server| async move {
            server.handshake(config((1920, 1080), (1920, 1080))).await;
            server.statuses_until(connected).await;
            // The CPU tier answers a zoom change with a keyframe request.
            let _ = server.input.send(ToWorker::Zoom(2));
            server
                .expect(|m| matches!(m, ClientMsg::RequestKeyframe))
                .await;
            server
                .wr
                .write_msg(&config((960, 540), (960, 540)))
                .await
                .unwrap();
            server.statuses_until(connected).await;
            // The UI's unchanged zoom must not request a second keyframe.
            let _ = server.input.send(ToWorker::Zoom(2));
            assert!(tokio::time::timeout(
                Duration::from_millis(200),
                server.rd.read_msg::<ClientMsg>()
            )
            .await
            .is_err());
        })
        .unwrap();
    }

    #[test]
    fn cursor_shapes_are_checked_and_coalesced_in_the_session() {
        let shape = |width: u32, argb_len: u32| ServerMsg::CursorShape {
            id: width,
            width,
            height: 8,
            hot_x: 0,
            hot_y: 0,
            argb_len,
        };
        with_session(|mut server| async move {
            server.handshake(config((1920, 1080), (1920, 1080))).await;
            server.statuses_until(connected).await;

            // A payload longer than the shape is skipped, and the Ping after
            // it is still answered.
            let bad = vec![1u8; 8 * 8 * 4 + 4];
            server
                .wr
                .write_msg_with_payloads(&shape(8, bad.len() as u32), &[&bad])
                .await
                .unwrap();
            server
                .wr
                .write_msg(&ServerMsg::Ping { t: 1 })
                .await
                .unwrap();
            server
                .expect(|m| matches!(m, ClientMsg::Pong { t: 1 }))
                .await;
            let seen = server
                .statuses_until(|s| matches!(s, Status::Cursor(_)))
                .await;
            let Some(Status::Cursor(latest)) = seen.last() else {
                unreachable!()
            };
            assert!(latest.take().expect("a shape").argb.is_empty());

            // Three shapes the UI has not taken yet make one notice.
            for width in [8, 16, 24] {
                let argb = vec![2u8; width as usize * 8 * 4];
                server
                    .wr
                    .write_msg_with_payloads(&shape(width, argb.len() as u32), &[&argb])
                    .await
                    .unwrap();
            }
            server
                .wr
                .write_msg(&ServerMsg::Ping { t: 2 })
                .await
                .unwrap();
            server
                .expect(|m| matches!(m, ClientMsg::Pong { t: 2 }))
                .await;
            let notices: Vec<LatestCursor> = server
                .status
                .try_iter()
                .filter_map(|s| match s {
                    Status::Cursor(latest) => Some(latest),
                    _ => None,
                })
                .collect();
            assert_eq!(notices.len(), 1);
            let latest = notices[0].take().expect("the latest shape");
            assert_eq!((latest.width, latest.argb.len()), (24, 24 * 8 * 4));
        })
        .unwrap();
    }
}
