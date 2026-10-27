//! The network and decode worker: reach the server (ssh or TCP), run the
//! gliff protocol, decode video on the CPU with OpenH264 (`gliff-sw`) and
//! hand finished BGRA pictures to the UI. Input flows the other way.
//!
//! Portable on purpose: nothing in here touches AppKit, so it builds and
//! tests on Linux too, and the `--dump` mode drives it headless.
//!
//! Adapted from the GTK client's `net.rs`; the differences are
//! - CPU decode only (VideoToolbox is the planned GPU tier, see
//!   `docs/macos.md`), so no Vulkan/VA-API and no integer output zoom —
//!   the AppKit layer scales with Core Animation instead;
//! - reconnection with backoff lives here rather than in the UI;
//! - clipboard messages are skipped for now (their payloads are discarded
//!   by `Framed::read_msg`).

use std::collections::VecDeque;
use std::sync::mpsc::{Sender as StdSender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use gliff_proto::{
    features, is_incompatible, version_mismatch, ChromaMode, ClientCaps, ClientMsg, Codec,
    ServerMsg, VideoPipeline, PROTOCOL_VERSION,
};
use gliff_sw::{PixelPool, PooledFrame};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::geometry::StreamGeometry;

//= Version of the worker's protocol behaviour; bump on any change to what
//= it sends or how it reacts.
pub const SESSION_VERSION: &str = "0.1.0";

/// Largest stream we ask for (same as the GTK client).
pub const MAX_STREAM: (u32, u32) = (3840, 2160);
/// Largest stream we accept (older servers may exceed `MAX_STREAM` once).
const MAX_DECODE: (u32, u32) = (4096, 4096);
/// Largest cursor image accepted.
const MAX_CURSOR: u32 = 1024;
/// Drain the decoder's held picture after this much video silence.
const IDLE_DRAIN: Duration = Duration::from_millis(150);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How to reach the server.
#[derive(Clone, Debug)]
pub enum Endpoint {
    Tcp(String),
    Ssh(SshTarget),
}

impl Endpoint {
    /// The machine name for titles and logs.
    pub fn name(&self) -> &str {
        match self {
            Self::Tcp(a) => a,
            Self::Ssh(t) => &t.host,
        }
    }
}

/// Which chroma layouts to ask the server for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChromaPref {
    /// Full 4:4:4 colour (Dual420, recombined on the CPU), falling back to
    /// 4:2:0. Sharp coloured text; twice the decode work.
    #[default]
    Full,
    /// One 4:2:0 stream: half the decode work, softer coloured text.
    Reduced,
}

impl ChromaPref {
    fn modes(self) -> Vec<ChromaMode> {
        match self {
            Self::Full => vec![ChromaMode::Dual420, ChromaMode::Single420],
            Self::Reduced => vec![ChromaMode::Single420],
        }
    }
}

impl std::str::FromStr for ChromaPref {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "full" | "444" => Ok(Self::Full),
            "reduced" | "420" => Ok(Self::Reduced),
            other => Err(format!("`{other}` is not full or reduced")),
        }
    }
}

/// Everything a worker needs besides its channels.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub endpoint: Endpoint,
    /// XKB keymap text for the server's virtual keyboard; empty means the
    /// server's default (US).
    pub keymap: String,
    pub chroma: ChromaPref,
    /// Reconnect after a dropped link or a recoverable server error.
    pub reconnect: bool,
}

/// A decoded frame and the geometry it was encoded under.
pub struct Picture {
    /// Packed BGRA, `width * 4` bytes per row.
    pub frame: PooledFrame,
    pub geometry: StreamGeometry,
}

/// A remote cursor image: `bgra` holds `width` x `height` straight-alpha
/// BGRA pixels.
#[derive(Clone, Debug)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub bgra: Vec<u8>,
}

/// The newest cursor shape not yet taken by the UI (latest wins, so a fast
/// server never queues shapes without bound).
#[derive(Clone, Default)]
pub struct LatestCursor(Arc<Mutex<Option<CursorShape>>>);

impl LatestCursor {
    fn put(&self, shape: CursorShape) -> bool {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        slot.replace(shape).is_none()
    }

    pub fn take(&self) -> Option<CursorShape> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// What the worker tells the UI.
pub enum Status {
    Connecting {
        attempt: u32,
    },
    Connected {
        /// "server tier→client tier", e.g. "GPU→CPU".
        video: String,
        chroma: ChromaMode,
        view: (u32, u32),
        fps_cap: u32,
        /// Private headless output: frames already carry the cursor.
        headless: bool,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
    },
    /// A new cursor shape waits in the slot.
    Cursor(LatestCursor),
    /// Recoverable; a reconnect may follow.
    Error(String),
    /// The two ends cannot work together; no reconnect.
    Incompatible(String),
    /// The worker has stopped.
    Stopped,
}

/// Commands from the UI.
pub enum ToWorker {
    Send(ClientMsg),
}

/// The UI's ends of the worker's channels.
pub struct UiLink {
    pub frames: SyncSender<Picture>,
    pub status: StdSender<Status>,
    pub input: UnboundedReceiver<ToWorker>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Incompatible(String);

/// How one session ended without an error.
enum End {
    /// The UI dropped its input sender: stop for good.
    UiGone,
    /// The server closed the stream.
    Closed,
}

/// Spawn the worker thread. It runs until the UI drops its input sender
/// (or forever reconnecting when `reconnect` is set).
pub fn spawn(cfg: WorkerConfig, link: UiLink) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("gliff-worker".into())
        .spawn(move || run(cfg, link))
}

/// Run the worker on the calling thread.
pub fn run(cfg: WorkerConfig, mut link: UiLink) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the async runtime");
            let _ = link.status.send(Status::Error(e.to_string()));
            let _ = link.status.send(Status::Stopped);
            return;
        }
    };
    rt.block_on(async {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let _ = link.status.send(Status::Connecting { attempt });
            tracing::info!(endpoint = cfg.endpoint.name(), attempt, "connecting");
            let started = Instant::now();
            let result = connect_and_run(&cfg, &mut link).await;
            match result {
                Ok(End::UiGone) => break,
                Ok(End::Closed) => {
                    tracing::info!("server closed the session");
                    let _ = link.status.send(Status::Error("disconnected".into()));
                }
                Err(e) => match e.downcast::<Incompatible>() {
                    Ok(Incompatible(m)) => {
                        tracing::error!(message = %m, "incompatible server");
                        let _ = link.status.send(Status::Incompatible(m));
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "session failed");
                        let _ = link.status.send(Status::Error(format!("{e:#}")));
                    }
                },
            }
            if !cfg.reconnect {
                break;
            }
            //= A session that ran for a while resets the backoff.
            if started.elapsed() > BACKOFF_MAX {
                attempt = 1;
            }
            let delay = backoff(attempt);
            tracing::info!(?delay, "reconnecting after a pause");
            //= Wait, but stop at once if the UI goes away meanwhile; input
            //= typed while disconnected is dropped.
            let deadline = tokio::time::sleep(delay);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    cmd = link.input.recv() => if cmd.is_none() {
                        //= `Stopped` is sent once, after the runtime ends.
                        return;
                    },
                }
            }
        }
    });
    let _ = link.status.send(Status::Stopped);
}

/// Exponential backoff: 1, 2, 4, ... seconds, capped.
pub fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    (BACKOFF_MIN * (1u32 << shift)).min(BACKOFF_MAX)
}

async fn connect_and_run(cfg: &WorkerConfig, link: &mut UiLink) -> anyhow::Result<End> {
    match &cfg.endpoint {
        Endpoint::Tcp(addr) => {
            let stream = tokio::net::TcpStream::connect(addr)
                .await
                .with_context(|| format!("connect to {addr}"))?;
            stream.set_nodelay(true)?;
            let (rd, wr) = stream.into_split();
            session(rd, wr, cfg, link).await
        }
        Endpoint::Ssh(target) => {
            let ssh = spawn_ssh(target).context("start ssh")?;
            //= `child` is kill_on_drop; keep it alive for the session.
            let _child = ssh.child;
            session(ssh.stdout, ssh.stdin, cfg, link).await
        }
    }
}

/// Checked stream geometry from a `StreamConfig`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Config {
    chroma: ChromaMode,
    geometry: StreamGeometry,
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
        validate_stream_size(stream)?;
        validate_view_size(view, stream)?;
        anyhow::ensure!(scale_milli > 0, "invalid stream scale 0");
        Ok(Self {
            chroma,
            geometry: StreamGeometry {
                stream,
                view,
                scale_milli,
            },
            fps_cap,
        })
    }

    /// True when the coded stream changed and needs a new decoder.
    fn needs_new_decoder(&self, next: &Config) -> bool {
        (self.geometry.stream, self.chroma) != (next.geometry.stream, next.chroma)
    }
}

fn validate_stream_size((w, h): (u32, u32)) -> anyhow::Result<()> {
    anyhow::ensure!(
        w > 0
            && h > 0
            && w <= MAX_DECODE.0
            && h <= MAX_DECODE.1
            && w.is_multiple_of(2)
            && h.is_multiple_of(2),
        "invalid stream dimensions {w}x{h}; expected even dimensions within 4096x4096"
    );
    Ok(())
}

/// The view is the size the stream is scaled down from, never much smaller
/// than the stream; half is the slack the GTK client allows too.
fn validate_view_size((vw, vh): (u32, u32), (sw, sh): (u32, u32)) -> anyhow::Result<()> {
    anyhow::ensure!(
        vw > 0
            && vh > 0
            && vw <= MAX_DECODE.0
            && vh <= MAX_DECODE.1
            && u64::from(vw) * 2 >= u64::from(sw)
            && u64::from(vh) * 2 >= u64::from(sh),
        "invalid view {vw}x{vh} for a {sw}x{sh} stream"
    );
    Ok(())
}

fn cursor_payload_fits(width: u32, height: u32, len: u32) -> bool {
    (1..=MAX_CURSOR).contains(&width)
        && (1..=MAX_CURSOR).contains(&height)
        && u64::from(width) * u64::from(height) * 4 == u64::from(len)
}

fn tier_label(pipeline: Option<VideoPipeline>) -> String {
    let server = match pipeline {
        Some(VideoPipeline::Gpu) => "GPU",
        Some(VideoPipeline::Cpu) => "CPU",
        None => "?",
    };
    format!("{server}→CPU")
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Offer a picture to the UI, latest-wins: a full channel keeps the newest
/// picture in `undelivered` for the next drain tick.
fn deliver(frames: &SyncSender<Picture>, picture: Picture, undelivered: &mut Option<Picture>) {
    match frames.try_send(picture) {
        Ok(()) => *undelivered = None,
        Err(TrySendError::Full(p)) => *undelivered = Some(p),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// The client's opening message for these settings.
pub fn hello(cfg: &WorkerConfig) -> ClientMsg {
    ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        keymap: cfg.keymap.clone(),
        caps: ClientCaps {
            codecs: vec![Codec::H264],
            max_width: MAX_STREAM.0,
            max_height: MAX_STREAM.1,
            chroma: cfg.chroma.modes(),
            features: features(),
        },
    }
}

async fn session<R, W>(rd: R, wr: W, cfg: &WorkerConfig, link: &mut UiLink) -> anyhow::Result<End>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);
    if cfg.keymap.is_empty() {
        tracing::info!("no keymap given; the server's virtual keyboard uses US");
    }
    writer.write_msg(&hello(cfg)).await.context("send Hello")?;

    //= Handshake: HelloAck, any Pings (answered at once to seed the server's
    //= round-trip estimate), then the first StreamConfig.
    let mut got_ack = false;
    let mut headless = false;
    let (mut cfg_now, pipeline) = loop {
        match reader.read_msg::<ServerMsg>().await? {
            ServerMsg::HelloAck {
                version,
                session,
                features,
                outputs,
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(
                        Incompatible(version_mismatch(PROTOCOL_VERSION, version).into()).into(),
                    );
                }
                tracing::info!(
                    output = %session.output,
                    headless = session.headless,
                    outputs = outputs.len(),
                    ?features,
                    "server accepted"
                );
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
                let c = Config::new(
                    (width, height),
                    chroma,
                    scale_milli,
                    (view_width, view_height),
                    fps_cap,
                )?;
                break (c, pipeline);
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

    let new_decoder = |c: &Config| -> anyhow::Result<gliff_sw::Decoder> {
        let (w, h) = c.geometry.stream;
        Ok(gliff_sw::Decoder::new(
            c.chroma != ChromaMode::Single420,
            w,
            h,
        )?)
    };
    let mut decoder = new_decoder(&cfg_now)?;
    let mut video_label = tier_label(pipeline);
    let connected = |c: &Config, label: &str| Status::Connected {
        video: label.to_string(),
        chroma: c.chroma,
        view: c.geometry.view,
        fps_cap: c.fps_cap,
        headless,
    };
    let _ = link.status.send(connected(&cfg_now, &video_label));
    tracing::info!(
        stream = ?cfg_now.geometry.stream,
        view = ?cfg_now.geometry.view,
        scale_milli = cfg_now.geometry.scale_milli,
        chroma = ?cfg_now.chroma,
        video = %video_label,
        "connected"
    );

    //= Writes run on their own task so reading video never waits on a write
    //= and the two peers cannot deadlock.
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();
    let mut writes = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            writer.write_msg(&m).await?;
        }
        Ok::<(), gliff_transport::Error>(())
    });
    //= The CPU decoder starts clean; ask for a keyframe in case the server
    //= is mid-GOP (a fresh session normally opens with one anyway).
    let _ = out_tx.send(ClientMsg::RequestKeyframe);

    let pool = PixelPool::default();
    let cursor = LatestCursor::default();
    //= Geometry of the access units inside the decoder, oldest first: the
    //= CPU decoder may return the PREVIOUS unit's picture.
    let mut in_decoder: VecDeque<StreamGeometry> = VecDeque::new();
    let mut undelivered: Option<Picture> = None;
    let mut drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
    let mut logged_first = false;
    let (mut frames_since, mut bytes_since, mut decode_ms_acc) = (0u32, 0u64, 0f32);
    let mut last_report = Instant::now();

    let end = loop {
        let read = tokio::select! {
            read = reader.read_msg::<ServerMsg>() => read,
            cmd = link.input.recv() => {
                match cmd {
                    Some(ToWorker::Send(m)) => { let _ = out_tx.send(m); }
                    None => {
                        let _ = out_tx.send(ClientMsg::Bye);
                        break End::UiGone;
                    }
                }
                continue;
            }
            written = &mut writes => {
                //= Return directly: a finished JoinHandle must not be polled
                //= again by the flush below.
                written.context("writer task")?.context("write to server")?;
                return Ok(End::Closed);
            }
            _ = tokio::time::sleep_until(drain_at),
                if undelivered.is_some() || decoder.has_pending() =>
            {
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                let picture = match undelivered.take() {
                    Some(p) => Some(p),
                    None => match decoder.flush() {
                        Ok(f) => f.map(|f| Picture {
                            geometry: in_decoder.pop_front().unwrap_or(cfg_now.geometry),
                            frame: f.zoomed(1, &pool),
                        }),
                        Err(e) => {
                            tracing::warn!(error = %e, "idle drain failed; requesting keyframe");
                            in_decoder.clear();
                            let _ = out_tx.send(ClientMsg::RequestKeyframe);
                            None
                        }
                    },
                };
                if let Some(p) = picture {
                    deliver(&link.frames, p, &mut undelivered);
                }
                continue;
            }
        };
        let msg = match read {
            Ok(m) => m,
            Err(gliff_transport::Error::Closed) => break End::Closed,
            Err(e) => return Err(e.into()),
        };
        match msg {
            ServerMsg::VideoFrame {
                frame_id,
                data_len,
                aux_len,
                ..
            } => {
                let main = reader.read_payload(data_len).await?;
                let aux = if aux_len > 0 {
                    reader.read_payload(aux_len).await?
                } else {
                    Bytes::new()
                };
                bytes_since += u64::from(data_len) + u64::from(aux_len);
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                in_decoder.push_back(cfg_now.geometry);
                let t0 = Instant::now();
                let decoded = decoder.decode(&main, &aux);
                let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
                //= Ack at once so the server keeps pacing.
                let _ = out_tx.send(ClientMsg::FrameAck {
                    frame_id,
                    decoded_at_ms: now_ms(),
                });
                match decoded {
                    Ok(Some(f)) => {
                        if !logged_first {
                            tracing::info!("first frame decoded");
                            logged_first = true;
                        }
                        let geometry = in_decoder.pop_front().unwrap_or(cfg_now.geometry);
                        let picture = Picture {
                            frame: f.zoomed(1, &pool),
                            geometry,
                        };
                        deliver(&link.frames, picture, &mut undelivered);
                        frames_since += 1;
                        decode_ms_acc += dec_ms;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "decode error; requesting keyframe");
                        in_decoder.clear();
                        let _ = out_tx.send(ClientMsg::RequestKeyframe);
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
                if cfg_now.needs_new_decoder(&next) {
                    tracing::info!(stream = ?next.geometry.stream, chroma = ?chroma, "new stream");
                    decoder = new_decoder(&next)?;
                    undelivered = None;
                    in_decoder.clear();
                }
                cfg_now = next;
                video_label = tier_label(pipeline);
                let _ = link.status.send(connected(&cfg_now, &video_label));
            }
            ServerMsg::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb_len,
                ..
            } => {
                let bgra = if cursor_payload_fits(width, height, argb_len) {
                    reader.read_payload(argb_len).await?.to_vec()
                } else {
                    tracing::debug!(width, height, argb_len, "ignoring a malformed cursor shape");
                    Vec::new()
                };
                let shape = CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    bgra,
                };
                if cursor.put(shape) {
                    let _ = link.status.send(Status::Cursor(cursor.clone()));
                }
            }
            ServerMsg::Ping { t } => {
                let _ = out_tx.send(ClientMsg::Pong { t });
            }
            ServerMsg::Error { code, message, .. } if is_incompatible(code) => {
                return Err(Incompatible(message).into())
            }
            ServerMsg::Error { code, message, .. } => {
                anyhow::bail!("server error {code}: {message}")
            }
            //= CursorPos, Pong, HelloAck, and clipboard traffic (not yet
            //= bridged to NSPasteboard): nothing to do; payloads are skipped.
            other => {
                tracing::trace!(?other, "ignored");
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            let secs = last_report.elapsed().as_secs_f32();
            let _ = link.status.send(Status::Stats {
                fps: frames_since as f32 / secs,
                mbit: bytes_since as f32 * 8.0 / 1_000_000.0 / secs,
                decode_ms: if frames_since > 0 {
                    decode_ms_acc / frames_since as f32
                } else {
                    0.0
                },
            });
            (frames_since, bytes_since, decode_ms_acc) = (0, 0, 0.0);
            last_report = Instant::now();
        }
    };
    //= Give the writer a moment to flush Bye before the stream drops.
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_millis(200), &mut writes).await;
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn cfg() -> WorkerConfig {
        WorkerConfig {
            endpoint: Endpoint::Tcp("test".into()),
            keymap: String::new(),
            chroma: ChromaPref::Full,
            reconnect: false,
        }
    }

    #[test]
    fn hello_advertises_h264_and_the_chroma_preference() {
        let ClientMsg::Hello { caps, version, .. } = hello(&cfg()) else {
            panic!()
        };
        assert_eq!(version, PROTOCOL_VERSION);
        assert_eq!(caps.codecs, vec![Codec::H264]);
        assert_eq!(
            caps.chroma,
            vec![ChromaMode::Dual420, ChromaMode::Single420]
        );
        let mut c = cfg();
        c.chroma = ChromaPref::Reduced;
        let ClientMsg::Hello { caps, .. } = hello(&c) else {
            panic!()
        };
        assert_eq!(caps.chroma, vec![ChromaMode::Single420]);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(50), BACKOFF_MAX);
    }

    #[test]
    fn geometry_is_checked() {
        assert!(Config::new((1920, 1080), ChromaMode::Single420, 1000, (1920, 1080), 60).is_ok());
        assert!(Config::new((1921, 1080), ChromaMode::Single420, 1000, (1921, 1080), 60).is_err());
        assert!(Config::new((1920, 1080), ChromaMode::Single420, 1000, (10, 10), 60).is_err());
        assert!(Config::new((1920, 1080), ChromaMode::Single420, 0, (1920, 1080), 60).is_err());
        assert!(cursor_payload_fits(2, 2, 16));
        assert!(!cursor_payload_fits(2, 2, 15));
        assert!(!cursor_payload_fits(2000, 2, 16000));
    }

    /// A fake server over an in-memory pipe: the client says Hello, answers
    /// the Ping, takes the StreamConfig, reports Connected, asks for a
    /// keyframe, forwards UI input, and says Bye when the UI goes away.
    #[test]
    fn handshake_input_and_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (client, server) = tokio::io::duplex(1 << 16);
            let (crd, cwr) = tokio::io::split(client);
            let (srd, swr) = tokio::io::split(server);
            let mut srd = Framed::new(srd);
            let mut swr = Framed::new(swr);

            let (frames_tx, _frames_rx) = mpsc::sync_channel(1);
            let (status_tx, status_rx) = mpsc::channel();
            let (input_tx, input_rx) = unbounded_channel();
            let mut link = UiLink {
                frames: frames_tx,
                status: status_tx,
                input: input_rx,
            };
            let cfg = cfg();
            let client_task = async move { session(crd, cwr, &cfg, &mut link).await };

            let server_task = async move {
                let hello: ClientMsg = srd.read_msg().await.unwrap();
                assert!(matches!(hello, ClientMsg::Hello { .. }));
                swr.write_msg(&ServerMsg::HelloAck {
                    version: PROTOCOL_VERSION,
                    session: gliff_proto::SessionInfo {
                        headless: true,
                        output: "HEADLESS-1".into(),
                    },
                    outputs: vec![],
                    features: vec![],
                })
                .await
                .unwrap();
                swr.write_msg(&ServerMsg::Ping { t: 9 }).await.unwrap();
                assert_eq!(
                    srd.read_msg::<ClientMsg>().await.unwrap(),
                    ClientMsg::Pong { t: 9 }
                );
                swr.write_msg(&ServerMsg::StreamConfig {
                    codec: Codec::H264,
                    chroma: ChromaMode::Single420,
                    pipeline: Some(VideoPipeline::Gpu),
                    width: 1280,
                    height: 800,
                    scale_milli: 2000,
                    extradata: vec![],
                    aux_extradata: None,
                    view_width: 1280,
                    view_height: 800,
                    fps_cap: 60,
                })
                .await
                .unwrap();
                assert_eq!(
                    srd.read_msg::<ClientMsg>().await.unwrap(),
                    ClientMsg::RequestKeyframe
                );
                input_tx
                    .send(ToWorker::Send(ClientMsg::Key {
                        keycode: 30,
                        pressed: true,
                    }))
                    .unwrap();
                assert_eq!(
                    srd.read_msg::<ClientMsg>().await.unwrap(),
                    ClientMsg::Key {
                        keycode: 30,
                        pressed: true
                    }
                );
                drop(input_tx);
                assert_eq!(srd.read_msg::<ClientMsg>().await.unwrap(), ClientMsg::Bye);
            };
            let (end, ()) = tokio::join!(client_task, server_task);
            assert!(matches!(end.unwrap(), End::UiGone));
            let connected = status_rx.try_iter().any(|s| {
                matches!(
                    s,
                    Status::Connected {
                        headless: true,
                        view: (1280, 800),
                        ..
                    }
                )
            });
            assert!(connected);
        });
    }

    #[test]
    fn an_incompatible_server_is_reported_as_such() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (client, server) = tokio::io::duplex(1 << 16);
            let (crd, cwr) = tokio::io::split(client);
            let (srd, swr) = tokio::io::split(server);
            let (mut srd, mut swr) = (Framed::new(srd), Framed::new(swr));
            let (frames_tx, _f) = mpsc::sync_channel(1);
            let (status_tx, _s) = mpsc::channel();
            let (_input_tx, input_rx) = unbounded_channel();
            let mut link = UiLink {
                frames: frames_tx,
                status: status_tx,
                input: input_rx,
            };
            let cfg = cfg();
            let server = async move {
                let _: ClientMsg = srd.read_msg().await.unwrap();
                swr.write_msg(&ServerMsg::error(gliff_proto::ERROR_VERSION, "too old"))
                    .await
                    .unwrap();
            };
            let (res, ()) = tokio::join!(session(crd, cwr, &cfg, &mut link), server);
            let err = res.err().unwrap();
            assert!(err.downcast_ref::<Incompatible>().is_some());
        });
    }
}
