//! One client session: output setup, the capture/encode/send loop, and input.

use std::ops::ControlFlow;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{
    features, version_mismatch, ChromaMode, ClientCaps, ClientMsg, ClipboardMsg, Codec, Greeting,
    OutputInfo as ProtoOutput, Rect, ServerMsg, SessionInfo, VideoPipeline, ERROR_NO_CHROMA,
    ERROR_NO_CODEC, ERROR_VERSION, PROTOCOL_VERSION,
};
use gliff_sw::VideoMode;
use gliff_transport::clipboard::progress::Jobs;
use gliff_transport::clipboard::{outbound_channel, Side, Transfers};
use gliff_transport::Framed;
use gliff_vk::{DmabufPlane, EncodedFrame, Encoder, EncoderSettings, Gpu};

use crate::rate::{cpu_rebuild_due, Ladder, LinkEstimator, RateController};
use crate::writer::Writer;
use hypr_capture::{CaptureConfig, CaptureEvent, CaptureMode, CapturedFrame, Capturer};
use hypr_input::{Axis as InAxis, Clipboard, ClipboardEvent, Input, InputCmd, InputConfig};
use hypr_wl::Target;

use crate::clipboard::Bridge;
use crate::notify;

pub struct Config {
    pub target: Target,
    pub output: Option<String>,
    pub render_node: PathBuf,
    pub low_bandwidth: bool,
    pub bitrate: Option<u32>,
    pub video: VideoMode,
    /// Dual-stream 4:4:4 on the CPU tier too (it defaults to Single420).
    pub full_chroma: bool,
    /// What the capture ring is made of; `Shm` implies the CPU tier.
    pub capture: CaptureMode,
}

/// Events from the capture thread.
enum Incoming {
    Frame(CapturedFrame),
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    CursorPos {
        x: f64,
        y: f64,
        visible: bool,
    },
    Stopped,
    Error(String),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub async fn run<R, W>(rd: R, wr: W, cfg: Config) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let (keymap, caps) = handshake(&mut reader, &mut writer).await?;
    if !caps.codecs.contains(&Codec::H264) {
        writer
            .write_msg(&ServerMsg::error(
                ERROR_NO_CODEC,
                "No video codec in common",
            ))
            .await?;
        anyhow::bail!("client advertises no codec this server speaks");
    }
    if !caps.chroma.contains(&ChromaMode::Dual420) && !caps.chroma.contains(&ChromaMode::Single420)
    {
        writer
            .write_msg(&ServerMsg::error(
                ERROR_NO_CHROMA,
                "No chroma mode in common",
            ))
            .await?;
        anyhow::bail!("client advertises no chroma mode this server speaks");
    }

    let instance = cfg.target.instance().context("find Hyprland instance")?;
    let output = setup_output(&instance, &cfg, &caps)?;
    tracing::info!(output = %output.name, output.width, output.height, headless = output.is_headless(), "session output ready");

    let video = VideoTier::open(cfg.video, cfg.capture, &cfg.render_node);
    let codec = Codec::H264;
    writer
        .write_msg(&ServerMsg::HelloAck {
            version: PROTOCOL_VERSION,
            session: SessionInfo {
                headless: output.is_headless(),
                output: output.name.clone(),
            },
            outputs: vec![ProtoOutput {
                name: output.name.clone(),
                width: output.width,
                height: output.height,
                scale_milli: (output.scale * 1000.0).round() as u32,
            }],
            features: features(),
        })
        .await?;
    let ladder = Ladder::new(START_LEVEL);
    let start_fps = ladder.level().fps_cap.min(MAX_FPS);
    let fps_cmd = start_fps;
    let on_gpu = matches!(video, VideoTier::Gpu(_));
    let VideoStart {
        video,
        chroma,
        stream,
        encoder_max,
        ctl,
        settings,
        encoder,
    } = match VideoStart::open(video, &cfg, &caps, &output, fps_cmd) {
        Ok(start) => start,
        Err(e) if on_gpu => {
            tracing::warn!(error = %format!("{e:#}"), "cannot start the GPU video pipeline; falling back to the CPU pipeline");
            VideoStart::open(VideoTier::Cpu, &cfg, &caps, &output, fps_cmd)?
        }
        Err(e) => return Err(e),
    };
    // Pipelined behind HelloAck, never waited on: the Pong seeds the
    // round-trip estimate, usually before the first frame ack arrives.
    writer.write_msg(&ServerMsg::Ping { t: now_ms() }).await?;
    writer
        .write_msg(&stream_config(
            codec,
            chroma,
            video.pipeline(),
            &output,
            stream,
            stream,
            start_fps,
        ))
        .await?;

    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel();
    // The GPU tier imports the captured dmabuf, so it cannot take the wl_shm
    // ring the CPU tier falls back to when no linear dmabuf is offered.
    let allow_shm = !matches!(video, VideoTier::Gpu(_));
    let capturer = start_capture(
        &cfg.target,
        &output.name,
        &cfg.render_node,
        cfg.capture,
        allow_shm,
        cap_tx,
    )?;

    let input = start_input(&cfg.target, &output.name, &keymap)?;
    input.send(output.logical_extent()).ok();
    if output.is_headless() {
        // Focus follows the pointer, so put it on the new screen right away:
        // otherwise the first launched window lands on the remote's own screen.
        let InputCmd::SetExtent { width, height } = output.logical_extent() else {
            unreachable!()
        };
        input
            .send(InputCmd::Motion {
                x: width as f64 / 2.0,
                y: height as f64 / 2.0,
            })
            .ok();
    }

    let (clip_ev_tx, mut clip_ev_rx) = mpsc::unbounded_channel::<ClipboardEvent>();
    let compositor_clipboard = Clipboard::start(
        cfg.target.clone(),
        Box::new(move |ev| {
            let _ = clip_ev_tx.send(ev);
        }),
    )
    .map_err(|e| tracing::warn!(error = %e, "clipboard bridge unavailable"))
    .ok();
    let (clip_out_tx, mut clip_out_rx) = outbound_channel();
    let (report_tx, cancel_rx) = notify::start();
    let jobs = Jobs::new(move |id, progress| {
        let _ = report_tx.send((id, progress));
    });
    notify::forward_cancels(cancel_rx, jobs.clone());
    let clipboard = Bridge::new(
        Transfers::new(clip_out_tx, Side::Server),
        jobs,
        compositor_clipboard,
    );

    let mut msg_rx = spawn_reader(reader);
    let (writer, mut write_reports) = Writer::spawn(writer);

    capturer.request_frame().ok();
    let first_frame_deadline =
        tokio::time::Instant::from_std(Instant::now() + FIRST_FRAME_DEADLINE);
    let mut clip_open = true;
    let mut session = Session {
        writer,
        video,
        instance,
        output,
        stream,
        encoder_max,
        gpu_encoded: false,
        require_shm: false,
        client_extent: (caps.max_width, caps.max_height),
        caps,
        link: LinkEstimator::new(),
        ctl,
        codec,
        chroma,
        settings,
        encoder,
        input,
        capturer,
        pending: None,
        capture_asked: true,
        blocked_noted: false,
        frame_id: 0,
        want_keyframe: true,
        fps_cmd,
        encode_us: 0.0,
        next_send_at: Instant::now(),
        last_frame: None,
        refines: MAX_REFINES,
        next_refine_at: Instant::now(),
        recaptured: false,
        ladder,
        base_stream: stream,
        base_chroma: chroma,
        sent_fps_cap: start_fps,
        cursor_shape_id: 0,
    };

    loop {
        let flow = tokio::select! {
            msg = msg_rx.recv() => match msg {
                Some(Inbound::Msg(msg)) => session.on_client_msg(msg).await?,
                Some(Inbound::Clipboard(msg, payload)) => {
                    clipboard.on_peer_msg(msg, payload);
                    ControlFlow::Continue(())
                }
                None => {
                    tracing::info!("client disconnected");
                    ControlFlow::Break(())
                }
            },
            out = clip_out_rx.recv() => {
                if let Some((msg, payload)) = out {
                    session.writer.send(ServerMsg::from(msg), vec![payload.to_vec()]);
                }
                ControlFlow::Continue(())
            }
            // Disarmed once the sender is gone (clipboard bridge failed or
            // ended), or a closed channel would keep this arm always ready.
            ev = clip_ev_rx.recv(), if clip_open => {
                match ev {
                    Some(ev) => clipboard.on_compositor_event(ev),
                    None => clip_open = false,
                }
                ControlFlow::Continue(())
            }
            ev = cap_rx.recv() => session.on_capture(ev)?,
            report = write_reports.recv() => {
                let elapsed = report.context("writer task ended")??;
                // A write that took longer than a frame interval means the
                // socket, not the byte cap, was the bottleneck.
                if elapsed > session.frame_interval() {
                    session.ctl.note_writer_blocked();
                }
                ControlFlow::Continue(())
            }
            // The one-shot first-frame rescue: a static screen produces no
            // damage, so the capture never completes; retry it once with
            // full damage.
            _ = tokio::time::sleep_until(first_frame_deadline),
                if session.frame_id == 0 && !session.recaptured =>
            {
                tracing::info!("no first frame yet; recapturing with full damage");
                session.recaptured = true;
                session.capturer.recapture().ok();
                ControlFlow::Continue(())
            }
            // The refinement timer: fires when the screen is still and the
            // last frame can be re-encoded toward sharp.
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(session.refine_wake())),
                if session.can_refine() =>
            {
                ControlFlow::Continue(())
            }
            // The pace timer: fires when a frame waits only on the cadence.
            // The guard must not test the deadline itself, or the arm would
            // disarm exactly while waiting for it.
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(session.next_send_at)),
                if session.pending.is_some() && session.link_has_room() =>
            {
                ControlFlow::Continue(())
            }
        };
        if flow.is_break() {
            break;
        }
        session.pump_encoder()?;
    }

    session.inject(InputCmd::ReleaseAll);
    let Session {
        capturer,
        input,
        output,
        ..
    } = session;
    // Hyprland 0.56 aborts if a cursor capture session is still alive when
    // its monitor is removed, so end the capture and input threads before
    // dropping the output.
    drop(capturer);
    drop(input);
    drop(output);
    Ok(())
}

/// Read the client's Hello and check its protocol version.
async fn handshake<R, W>(
    reader: &mut Framed<R>,
    writer: &mut Framed<W>,
) -> Result<(String, ClientCaps)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match reader.read_msg::<Greeting>().await.context("read Hello")? {
        Greeting::Hello { keymap, caps } => {
            tracing::debug!(features = ?caps.features, "client features");
            Ok((keymap, caps))
        }
        Greeting::OtherVersion(version) => {
            let message = version_mismatch(version, PROTOCOL_VERSION);
            writer
                .write_msg(&ServerMsg::error(ERROR_VERSION, message))
                .await?;
            anyhow::bail!("{message}: client protocol {version}, server {PROTOCOL_VERSION}");
        }
    }
}

/// What the reader task hands the session.
enum Inbound {
    Msg(ClientMsg),
    /// A clipboard message with the chunk that followed it (empty otherwise).
    Clipboard(ClipboardMsg, Bytes),
}

/// Drain the socket on its own task so a burst of input (a mouse drag) can
/// never starve frame capture and a blocked write can never block reads.
/// Clipboard chunks are read here so the socket stays framed.
fn spawn_reader<R>(mut reader: Framed<R>) -> UnboundedReceiver<Inbound>
where
    R: AsyncRead + Unpin + 'static,
{
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        loop {
            let msg = match reader.read_msg::<ClientMsg>().await {
                Ok(m) => m,
                Err(gliff_transport::Error::Closed) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read from the client");
                    break;
                }
            };
            let inbound = match msg.into_clipboard() {
                Err(msg) => Inbound::Msg(msg),
                Ok(clip) => {
                    let payload = match &clip {
                        ClipboardMsg::Data { data_len, .. } if *data_len as usize <= CHUNK => {
                            match reader.read_payload(*data_len).await {
                                Ok(b) => b,
                                Err(_) => break,
                            }
                        }
                        ClipboardMsg::Data { .. } => break,
                        _ => Bytes::new(),
                    };
                    Inbound::Clipboard(clip, payload)
                }
            };
            if msg_tx.send(inbound).is_err() {
                break;
            }
        }
    });
    msg_rx
}

struct Session {
    writer: Writer,
    video: VideoTier,
    instance: hypr_ipc::Instance,
    output: SessionOutput,
    /// Encoded size. Equals the output size, except for a mirrored screen
    /// larger than the client's window, or any output larger than the
    /// encoder's maximum, which are scaled down to fit.
    stream: (u32, u32),
    /// The largest size the encoder accepts.
    encoder_max: (u32, u32),
    /// A GPU encode has succeeded, so a later failure is not a broken
    /// driver path to fall back from.
    gpu_encoded: bool,
    /// The CPU fallback has asked for a new ring; reject queued dmabuf frames.
    require_shm: bool,
    caps: ClientCaps,
    /// The client's last reported window size, so a mirrored output that
    /// changes mode can be refitted to the window.
    client_extent: (u32, u32),
    link: LinkEstimator,
    ctl: RateController,
    codec: Codec,
    chroma: ChromaMode,
    settings: EncoderSettings,
    encoder: VideoEncoder,
    input: Input,
    capturer: Capturer,
    /// The newest captured frame not yet encoded.
    pending: Option<CapturedFrame>,
    capture_asked: bool,
    /// The pending frame has already counted as blocked for the rate
    /// controller.
    blocked_noted: bool,
    frame_id: u64,
    want_keyframe: bool,
    /// The one commanded cadence: the pace timer and the encoder's
    /// programmed frame rate both use it.
    fps_cmd: u32,
    /// Smoothed encode time, for the sustainable-fps clamp.
    encode_us: f64,
    /// The pace slot for the next frame.
    next_send_at: Instant,
    /// The last frame that went out, kept for still-picture refinement and
    /// for the keyframe after a ladder step on a static screen. It pins one
    /// capture ring slot, which the ring's extra buffer pays for.
    last_frame: Option<CapturedFrame>,
    /// Refinement passes spent on `last_frame`.
    refines: u8,
    next_refine_at: Instant,
    /// The one-shot first-frame recapture has fired.
    recaptured: bool,
    /// Quality levels the link rate picks from: fps first, then chroma,
    /// then resolution.
    ladder: Ladder,
    /// The full-quality fit size (the client's view); the active stream is
    /// this scaled by the ladder level.
    base_stream: (u32, u32),
    /// The chroma the handshake negotiated; a ladder level may reduce the
    /// active `chroma` to Single420.
    base_chroma: ChromaMode,
    /// The fps cap in the last StreamConfig, so a pace-only level change
    /// still reaches the client.
    sent_fps_cap: u32,
    cursor_shape_id: u32,
}

/// The session-wide fps ceiling; ladder levels cap below it.
const MAX_FPS: u32 = 60;
/// Still-picture refinement: at most this many re-encodes of an unchanged
/// frame, at least this far apart.
const MAX_REFINES: u8 = 8;
const REFINE_EVERY: Duration = Duration::from_millis(200);
/// A static screen produces no damage and so no first frame; after this
/// long the capture is torn down and retried once with full damage.
const FIRST_FRAME_DEADLINE: Duration = Duration::from_millis(700);
/// The starting ladder level: 30 fps, full quality. It halves the first
/// burst against level 0 while the controller measures available capacity.
const START_LEVEL: usize = 1;

impl Session {
    async fn on_client_msg(&mut self, msg: ClientMsg) -> Result<ControlFlow<()>> {
        match msg {
            ClientMsg::Bye => return Ok(ControlFlow::Break(())),
            ClientMsg::FrameAck { frame_id, .. } => {
                let now = Instant::now();
                self.link.on_ack(now, frame_id);
                if let Some(reason) = self.ctl.on_ack(now, &mut self.link) {
                    tracing::debug!(
                        ?reason,
                        target = self.ctl.target(),
                        fps_cmd = self.fps_cmd,
                        rate_mbit = self
                            .link
                            .rate_max_bps(now)
                            .map(|r| format!("{:.2}", r / 1e6)),
                        queueing_ms = self.link.base_rtt_ms(now).map(|b| format!("{b:.0}")),
                        mdev_ms = format!("{:.0}", self.link.mdev_ms()),
                        inflight = self.link.inflight_bytes(),
                        cap = self.link.cap_bytes(now, self.ctl.is_slow_start()),
                        "rate changed"
                    );
                    self.apply_rate();
                }
                if self.pending.is_none() && self.link.inflight_bytes() == 0 {
                    self.ctl.note_idle(now);
                }
                let dual = self.base_chroma == ChromaMode::Dual420;
                let can_drop_aux = self.can_drop_aux();
                let pixels = self.base_stream.0 as u64 * self.base_stream.1 as u64;
                if let Some(level) = self.ladder.consider(
                    now,
                    &self.link,
                    self.ctl.is_slow_start(),
                    self.ctl.congested_recently(now),
                    pixels,
                    dual,
                    can_drop_aux,
                    MAX_FPS,
                ) {
                    tracing::info!(
                        level,
                        fps_cap = self.fps_cap(),
                        rate_mbit = self
                            .link
                            .rate_max_bps(now)
                            .map(|r| format!("{:.2}", r / 1e6)),
                        delivered_fps = format!("{:.1}", self.link.delivered_fps(now)),
                        "ladder step"
                    );
                    if self.apply_stream_params()? {
                        self.send_config();
                    }
                }
            }
            ClientMsg::RequestKeyframe => {
                self.want_keyframe = true;
                // A still screen has no capture pending, so answer with the
                // kept frame instead of waiting for the screen to change.
                if self.pending.is_none() && self.last_frame.is_some() {
                    self.refines = 0;
                    self.next_refine_at = Instant::now();
                }
            }
            ClientMsg::Key { keycode, pressed } => self.inject(InputCmd::Key {
                code: keycode,
                pressed,
            }),
            ClientMsg::PointerMotion { x, y } => self.inject(InputCmd::Motion { x, y }),
            ClientMsg::PointerButton { button, pressed } => {
                self.inject(InputCmd::Button { button, pressed })
            }
            ClientMsg::PointerAxis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let axis = match axis {
                    gliff_proto::Axis::Vertical => InAxis::Vertical,
                    gliff_proto::Axis::Horizontal => InAxis::Horizontal,
                };
                self.inject(InputCmd::Axis {
                    axis,
                    value,
                    discrete,
                    stop,
                })
            }
            ClientMsg::Resize {
                width,
                height,
                scale,
            } => self.resize(width, height, scale).await?,
            ClientMsg::Ping { t } => self.writer.send(
                ServerMsg::Pong {
                    t,
                    server_now_ms: now_ms(),
                },
                Vec::new(),
            ),
            ClientMsg::Pong { t } => {
                let rtt_ms = now_ms().saturating_sub(t) as f64;
                self.link.seed_rtt(Instant::now(), rtt_ms);
            }
            // The reader task routes clipboard messages to the Bridge.
            ClientMsg::ClipboardData { .. }
            | ClientMsg::ClipboardOffer { .. }
            | ClientMsg::ClipboardRequest { .. }
            | ClientMsg::ClipboardAck { .. }
            | ClientMsg::ClipboardAbort { .. } => {}
            ClientMsg::Keymap { keymap } if !keymap.is_empty() => {
                tracing::debug!(bytes = keymap.len(), "client keymap changed");
                self.inject(InputCmd::SetKeymap(keymap))
            }
            ClientMsg::Keymap { .. } => {}
            ClientMsg::Hello { .. } => anyhow::bail!("unexpected second Hello"),
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Forward an input event; a dead input thread ends the session elsewhere.
    fn inject(&self, cmd: InputCmd) {
        let _ = self.input.send(cmd);
    }

    /// Resize a headless output to the client's window, within the size it
    /// declared in Hello, and restart the encoder at the new size.
    /// Resize a headless output to the client's window at the client's
    /// scale, so the remote UI renders at the client's DPI.
    async fn resize(&mut self, width: u32, height: u32, scale: f32) -> Result<()> {
        let width = width.min(self.caps.max_width) & !1;
        let height = height.min(self.caps.max_height) & !1;
        let scale = if scale.is_finite() {
            scale.clamp(0.5, 4.0)
        } else {
            1.0
        };
        if width < 320 || height < 240 {
            return Ok(());
        }
        self.client_extent = (width, height);
        if !self.output.is_headless() {
            return self.fit_mirror(width, height);
        }
        let (width, height) = hypr_ipc::clean_mode(width, height, scale);
        let same_size = (width, height) == (self.output.width, self.output.height);
        let same_scale = (scale - self.output.scale).abs() < 0.01;
        if same_size && same_scale {
            return Ok(());
        }
        tracing::info!(width, height, scale, "resizing headless output");
        if let Err(e) = self
            .instance
            .set_monitor_mode(&self.output.name, width, height, 60, scale)
        {
            tracing::warn!(error = %e, "could not resize the headless output");
            return Ok(());
        }
        // Hyprland applies the mode asynchronously and may round the scale
        // so the logical size is whole; wait for it and use what it chose.
        let applied = self.wait_for_mode(width, height, scale).await;
        if !same_size {
            let base = EncoderSettings::fit_extent(width, height, self.encoder_max);
            if base != (width, height) {
                tracing::info!(
                    width = base.0,
                    height = base.1,
                    "scaling the stream to the encoder maximum"
                );
            }
            let old_base = self.base_stream;
            let (old_w, old_h) = (self.output.width, self.output.height);
            self.base_stream = base;
            self.output.width = width;
            self.output.height = height;
            if !self.apply_stream_params()? {
                // Keep streaming at the old size rather than end the session.
                self.base_stream = old_base;
                self.output.width = old_w;
                self.output.height = old_h;
                let s = self.output.scale;
                self.instance
                    .set_monitor_mode(&self.output.name, old_w, old_h, 60, s)
                    .ok();
                self.wait_for_mode(old_w, old_h, s).await;
                self.inject(self.output.logical_extent());
                return Ok(());
            }
        }
        self.output.scale = applied;
        self.inject(self.output.logical_extent());
        self.send_config();
        Ok(())
    }

    /// A mirrored screen keeps its size; when it is larger than the client's
    /// window the stream is scaled down to fit (never up), so the link and
    /// the decoder carry only what the window can show.
    fn fit_mirror(&mut self, win_w: u32, win_h: u32) -> Result<()> {
        let (ow, oh) = (self.output.width as f64, self.output.height as f64);
        let fit = (win_w as f64 / ow).min(win_h as f64 / oh).min(1.0);
        let base = EncoderSettings::fit_extent(
            ((ow * fit).round() as u32).max(2) & !1,
            ((oh * fit).round() as u32).max(2) & !1,
            self.encoder_max,
        );
        if base == self.base_stream {
            return Ok(());
        }
        tracing::info!(
            width = base.0,
            height = base.1,
            "scaling the mirrored screen to the window"
        );
        let old_base = self.base_stream;
        self.base_stream = base;
        if self.apply_stream_params()? {
            self.send_config();
        } else {
            self.base_stream = old_base;
        }
        Ok(())
    }

    /// Poll until the output reports the requested mode (Hyprland applies it
    /// asynchronously and may round the scale), for up to half a second,
    /// without blocking the session. Returns the scale in effect.
    async fn wait_for_mode(&self, width: u32, height: u32, scale: f32) -> f32 {
        let mut seen = None;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let Ok(mons) = self.instance.monitors() else {
                continue;
            };
            if let Some(m) = mons
                .iter()
                .find(|m| m.name == self.output.name && m.width == width && m.height == height)
            {
                seen = Some(m.scale);
                if (m.scale - scale).abs() < 0.15 {
                    return m.scale;
                }
            }
        }
        seen.unwrap_or(scale)
    }

    fn on_capture(&mut self, ev: Option<Incoming>) -> Result<ControlFlow<()>> {
        match ev {
            Some(Incoming::Frame(image)) => {
                if self.require_shm && !image.buffer.is_shm() {
                    return Ok(ControlFlow::Continue(()));
                }
                let size = image.extent();
                if !self.output.is_headless() && size != (self.output.width, self.output.height) {
                    self.output.width = size.0;
                    self.output.height = size.1;
                    if let Some(monitor) = self
                        .instance
                        .monitors()?
                        .iter()
                        .find(|m| m.name == self.output.name)
                    {
                        self.output.scale = monitor.scale;
                    }
                    self.inject(self.output.logical_extent());
                    let previous_stream = self.stream;
                    self.fit_mirror(self.client_extent.0, self.client_extent.1)?;
                    if self.stream == previous_stream {
                        // Same fitted size, but the output (and so the
                        // effective scale) changed: tell the client and
                        // restart from a keyframe.
                        self.send_config();
                        self.want_keyframe = true;
                    }
                    tracing::info!(width = size.0, height = size.1, "mirrored output resized");
                }
                self.pending = Some(image);
                self.capture_asked = false;
                self.blocked_noted = false;
            }
            Some(Incoming::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            }) => {
                self.cursor_shape_id += 1;
                let msg = ServerMsg::CursorShape {
                    id: self.cursor_shape_id,
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb_len: argb.len() as u32,
                };
                self.writer.send(msg, vec![argb]);
            }
            Some(Incoming::CursorPos { x, y, visible }) => {
                self.writer.send(
                    ServerMsg::CursorPos {
                        x,
                        y,
                        shape_id: self.cursor_shape_id,
                        visible,
                    },
                    Vec::new(),
                );
            }
            Some(Incoming::Stopped) => {
                tracing::warn!("capture stopped");
                return Ok(ControlFlow::Break(()));
            }
            Some(Incoming::Error(e)) => {
                tracing::error!(error = %e, "capture error");
                return Ok(ControlFlow::Break(()));
            }
            None => return Ok(ControlFlow::Break(())),
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Encode and queue the pending frame if the link has room and the pace
    /// slot is due, then ask the capture thread for the next one.
    fn pump_encoder(&mut self) -> Result<()> {
        let now = Instant::now();
        // Only a wait on the byte cap is congestion; count it once per
        // frame. A wait on the pace timer or the writer queue is not (the
        // writer has its own report-duration signal).
        if self.pending.is_some()
            && !self.blocked_noted
            && self.writer.video_ready()
            && !self.byte_cap_has_room(now)
        {
            self.ctl.note_blocked_at_cap();
            self.blocked_noted = true;
        }
        if self.link_has_room() && now >= self.next_send_at {
            if let Some(frame) = self.pending.take() {
                // A frame captured before a resize took effect is stale.
                if self.matches_output(&frame) {
                    if self.encode_and_send(&frame, false)?.is_none() {
                        return Ok(());
                    }
                    // Keep the frame: an idle link refines the still picture
                    // toward sharp, and a ladder step re-keyframes it at once.
                    self.last_frame = Some(frame);
                    self.refines = 0;
                    self.next_refine_at = now + REFINE_EVERY;
                }
            } else if now >= self.next_refine_at && self.refines < MAX_REFINES {
                if let Some(frame) = self.last_frame.take() {
                    if self.matches_output(&frame) {
                        // Re-encode the same buffer (import-cache hit) so a
                        // frame that arrived soft under a low budget
                        // converges to sharp while nothing changes on screen.
                        let Some(bytes) = self.encode_and_send(&frame, true)? else {
                            return Ok(());
                        };
                        self.refines += 1;
                        self.next_refine_at = now + REFINE_EVERY;
                        if (bytes as f64) < self.refined_done_bytes() {
                            // Converged: keep the frame for ladder steps,
                            // stop spending link on it.
                            self.refines = MAX_REFINES;
                        } else {
                            tracing::debug!(bytes, n = self.refines, "refined still frame");
                        }
                        self.last_frame = Some(frame);
                    }
                }
            }
        }
        // Keep one capture request outstanding even while blocked: a newer
        // frame replaces `pending` (latest wins), so the frame that finally
        // goes out on a slow link is current, not as old as the stall.
        if !self.capture_asked {
            self.capturer.request_frame().ok();
            self.capture_asked = true;
        }
        Ok(())
    }

    /// Room on the link: no frame in the writer queue and the bytes in
    /// flight stay under the cap. The pace timer is checked separately.
    fn link_has_room(&self) -> bool {
        self.writer.video_ready() && self.byte_cap_has_room(Instant::now())
    }

    fn byte_cap_has_room(&self, now: Instant) -> bool {
        self.link.inflight_bytes() == 0
            || self.link.inflight_bytes() + self.link.est_frame_bytes()
                <= self.link.cap_bytes(now, self.ctl.is_slow_start())
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps_cmd.max(1) as f64)
    }

    /// A frame captured before a resize took effect is stale. The check is
    /// against the CAPTURE-native (output) size, never the fitted stream
    /// size: a mirrored capture keeps output dimensions while its stream is
    /// fitted smaller.
    fn matches_output(&self, frame: &CapturedFrame) -> bool {
        frame.extent() == (self.output.width, self.output.height)
    }

    /// A refinement pass that codes below this many bytes changed nothing
    /// worth sending: the still picture has converged.
    fn refined_done_bytes(&self) -> f64 {
        let streams = if self.chroma == ChromaMode::Dual420 {
            2.0
        } else {
            1.0
        };
        0.2 * self.ctl.budget_per_frame(self.fps_cmd) as f64 * streams / 8.0
    }

    /// When the refinement timer should next fire, bounded by the pace.
    fn refine_wake(&self) -> Instant {
        self.next_refine_at.max(self.next_send_at)
    }

    fn can_refine(&self) -> bool {
        self.pending.is_none()
            && self.last_frame.is_some()
            && self.refines < MAX_REFINES
            && self.link_has_room()
    }

    /// The ladder level's frame-rate ceiling, bounded by the session's.
    fn fps_cap(&self) -> u32 {
        self.ladder.level().fps_cap.min(MAX_FPS)
    }

    /// A rung may only drop the auxiliary stream when the client can
    /// decode Single420; a Dual420-only client keeps both streams on every
    /// rung.
    fn can_drop_aux(&self) -> bool {
        self.caps.chroma.contains(&ChromaMode::Single420)
    }

    /// The chroma the current ladder level allows, within what the client
    /// advertised.
    fn active_chroma(&self) -> ChromaMode {
        if self.ladder.level().aux || !self.can_drop_aux() {
            self.base_chroma
        } else {
            ChromaMode::Single420
        }
    }

    /// One commanded cadence: the ladder's cap bounded by what the encoder
    /// can sustain.
    fn update_fps_cmd(&mut self) {
        let sustainable = if self.encode_us > 0.0 {
            (1e6 / (self.encode_us * 1.1)).clamp(5.0, MAX_FPS as f64) as u32
        } else {
            MAX_FPS
        };
        self.fps_cmd = self.fps_cap().min(sustainable).max(1);
    }

    /// Tell the client the active stream, view and fps cap.
    fn send_config(&mut self) {
        self.sent_fps_cap = self.fps_cap();
        self.writer.send(
            stream_config(
                self.codec,
                self.chroma,
                self.video.pipeline(),
                &self.output,
                self.stream,
                self.base_stream,
                self.sent_fps_cap,
            ),
            Vec::new(),
        );
    }

    /// The single choke point for stream geometry: derive the active stream
    /// from the full-quality fit size (`base_stream`) and the ladder level.
    /// A change of size or chroma rebuilds the encoder; an fps-only change
    /// updates the rate without one. Returns false when the encoder
    /// rejected the geometry; the caller sends the StreamConfig on success.
    fn apply_stream_params(&mut self) -> Result<bool> {
        self.update_fps_cmd();
        let level = self.ladder.level();
        let chroma = self.active_chroma();
        let want = EncoderSettings::fit_extent(
            ((self.base_stream.0 as f32 * level.scale) as u32).max(2) & !1,
            ((self.base_stream.1 as f32 * level.scale) as u32).max(2) & !1,
            self.encoder_max,
        );
        if want == self.stream && chroma == self.chroma {
            self.apply_rate();
            return Ok(true);
        }
        let streams = if chroma == ChromaMode::Dual420 { 2 } else { 1 };
        self.ctl
            .reconfigure(want.0 as u64 * want.1 as u64, streams, MAX_FPS);
        let settings = encoder_settings(
            want.0,
            want.1,
            self.ctl.stream_bitrate(),
            self.fps_cmd,
            self.ctl.vbv_ms(self.fps_cmd),
        );
        match VideoEncoder::new(&self.video, &settings, chroma == ChromaMode::Dual420) {
            Ok(encoder) => {
                tracing::info!(
                    width = want.0,
                    height = want.1,
                    ?chroma,
                    fps_cap = self.fps_cap(),
                    level = self.ladder.index(),
                    "stream reconfigured"
                );
                self.encoder = encoder;
                self.settings = settings;
                self.stream = want;
                self.chroma = chroma;
                // `pending` stays: a captured frame is output-sized, and a
                // ladder change does not touch the output. Dropping it here
                // would freeze the screen on its previous content whenever a
                // step lands just before the screen goes still (the pump's
                // matches_output check handles real output changes).
                self.want_keyframe = true;
                self.encode_us = 0.0;
                // The kept still frame produces the new config's keyframe at
                // once, even when nothing changes on screen.
                self.refines = 0;
                self.next_refine_at = Instant::now();
                Ok(true)
            }
            Err(e) => {
                tracing::warn!(error = %e, width = want.0, height = want.1, "encoder rejected the size; keeping the current one");
                Ok(false)
            }
        }
    }

    /// Fold one encode's duration into the sustainable-fps estimate and
    /// re-apply the rate when the commanded cadence moved by more than 15%.
    fn note_encode_time(&mut self, enc_us: f64) {
        self.encode_us = if self.encode_us == 0.0 {
            enc_us
        } else {
            0.8 * self.encode_us + 0.2 * enc_us
        };
        let before = self.fps_cmd;
        self.update_fps_cmd();
        let moved = (self.fps_cmd as f64 - before as f64).abs() / before.max(1) as f64;
        if moved > 0.15 {
            self.apply_rate();
        } else {
            self.fps_cmd = before;
        }
    }

    /// Program the encoder with the current target at the commanded cadence,
    /// so bits per frame match the frames that really leave.
    fn apply_rate(&mut self) {
        let bitrate = self.ctl.stream_bitrate();
        let vbv = self.ctl.vbv_ms(self.fps_cmd);
        self.settings.bitrate = bitrate;
        self.settings.framerate = self.fps_cmd;
        self.settings.vbv_ms = vbv;
        self.encoder.set_rate(bitrate, self.fps_cmd, vbv);
    }

    /// Refit the stream for CPU encoding and notify the client before its
    /// first CPU frame. Used both for a failed first GPU encode and when a
    /// later capture-ring allocation falls back to shared memory.
    fn fall_back_to_cpu(&mut self) -> Result<()> {
        self.video = VideoTier::Cpu;
        self.encoder_max = stream_limit(gliff_sw::Encoder::MAX_SIZE, &self.caps)?;
        self.base_stream =
            EncoderSettings::fit_extent(self.base_stream.0, self.base_stream.1, self.encoder_max);
        let level = self.ladder.level();
        self.stream = EncoderSettings::fit_extent(
            ((self.base_stream.0 as f32 * level.scale) as u32).max(2) & !1,
            ((self.base_stream.1 as f32 * level.scale) as u32).max(2) & !1,
            self.encoder_max,
        );
        let streams = if self.chroma == ChromaMode::Dual420 {
            2
        } else {
            1
        };
        self.ctl.reconfigure(
            self.stream.0 as u64 * self.stream.1 as u64,
            streams,
            MAX_FPS,
        );
        self.settings = encoder_settings(
            self.stream.0,
            self.stream.1,
            self.ctl.stream_bitrate(),
            self.fps_cmd,
            self.ctl.vbv_ms(self.fps_cmd),
        );
        self.encoder = VideoEncoder::new(
            &self.video,
            &self.settings,
            self.chroma == ChromaMode::Dual420,
        )?;
        self.encode_us = 0.0;
        self.want_keyframe = true;
        self.refines = 0;
        self.next_refine_at = Instant::now();
        self.send_config();
        Ok(())
    }

    /// Encode `frame` and queue it; returns the coded byte count.
    /// `app_limited` marks a send that does not use the capacity we have
    /// (a refinement pass), so it cannot lower the rate estimate.
    fn encode_and_send(
        &mut self,
        frame: &CapturedFrame,
        app_limited: bool,
    ) -> Result<Option<usize>> {
        // A resize can make GBM allocation fail after GPU encoding has
        // already succeeded. Switch tiers before importing the shm frame.
        if frame.buffer.is_shm() && matches!(self.video, VideoTier::Gpu(_)) {
            tracing::warn!("capture fell back to wl_shm; falling back to the CPU pipeline");
            self.fall_back_to_cpu()?;
        }
        let (width, height) = self.stream;
        let key = std::mem::take(&mut self.want_keyframe);
        let t0 = Instant::now();
        let encoded = match self.encoder.encode(frame, self.stream, key) {
            Ok(e) => e,
            Err(e) if matches!(self.video, VideoTier::Gpu(_)) && !self.gpu_encoded => {
                tracing::warn!(error = %format!("{e:#}"), "the first GPU encode failed; falling back to the CPU pipeline");
                self.fall_back_to_cpu()?;
                // A tiled NVIDIA dmabuf cannot be read by the CPU. Replace
                // the capture ring and wait for a fresh shared-memory frame.
                self.require_shm = true;
                self.pending = None;
                self.last_frame = None;
                self.refines = MAX_REFINES;
                self.want_keyframe = true;
                self.encode_us = 0.0;
                self.capturer.use_shm()?;
                self.capture_asked = true;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        self.gpu_encoded = true;
        let enc_us = t0.elapsed().as_micros();
        let aux = encoded.aux.unwrap_or_default();
        let msg = ServerMsg::VideoFrame {
            frame_id: self.frame_id,
            pts_us: now_ms() * 1000,
            keyframe: encoded.keyframe,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            }],
            data_len: encoded.main.len() as u32,
            aux_len: aux.len() as u32,
        };
        tracing::debug!(
            frame_id = self.frame_id,
            key = encoded.keyframe,
            main = encoded.main.len(),
            aux = aux.len(),
            enc_us,
            inflight_bytes = self.link.inflight_bytes(),
            "queued frame"
        );
        let total = encoded.main.len() + aux.len();
        // A frame that neither waited on the byte cap nor fills the pipe is
        // app-limited: its delivery rate reflects the content (a quiet
        // screen codes tiny frames), not what the link could carry. Such
        // samples may only raise the rate estimate.
        let now = Instant::now();
        let cap = self.link.cap_bytes(now, self.ctl.is_slow_start());
        let app_limited = app_limited
            || (!self.blocked_noted
                && self.link.inflight_bytes() + total as u64 + self.link.est_frame_bytes() <= cap);
        self.writer.send(msg, vec![encoded.main, aux]);
        self.ctl.note_frame_sent(now);
        self.link.on_sent(t0, self.frame_id, total, app_limited);
        self.note_encode_time(enc_us as f64);
        // Pace from the encode start so a slow encode adds no extra wait;
        // after an idle gap or a stall the cadence restarts one interval
        // after this encode, so resuming never sends a burst.
        let interval = self.frame_interval();
        self.next_send_at = (self.next_send_at + interval).max(t0 + interval);
        self.blocked_noted = false;
        self.frame_id += 1;
        Ok(Some(total))
    }
}

/// The selected video pipeline: Vulkan compute and VA-API on the GPU, or
/// OpenH264 on the CPU for machines without them.
enum VideoTier {
    Gpu(Arc<Gpu>),
    Cpu,
}

impl VideoTier {
    /// Open the requested tier. In `Gpu` mode a machine without Vulkan
    /// compute or a VA-API H.264 encoder falls back to the CPU instead of
    /// failing; a GPU whose encoder cannot then be created falls back in
    /// `VideoStart`, and one whose first encode fails in `encode_and_send`.
    fn open(mode: VideoMode, capture: CaptureMode, render_node: &std::path::Path) -> Self {
        if mode == VideoMode::Cpu {
            tracing::info!("using the CPU video pipeline as requested");
            return Self::Cpu;
        }
        if capture == CaptureMode::Shm {
            // Only the CPU pipeline can read a wl_shm frame.
            tracing::info!("using the CPU video pipeline for wl_shm capture");
            return Self::Cpu;
        }
        if !render_node.exists() {
            // Hyper-V guests have only a display node and no render node.
            tracing::warn!(render_node = %render_node.display(), "no render node; falling back to the CPU pipeline");
            return Self::Cpu;
        }
        match Gpu::open(Some(render_node)) {
            Ok(gpu) if gpu.can_encode() => {
                tracing::info!(gpu = %gpu.name, va = %gpu.va.vendor, encoder = gpu.encoder_api(), "using the GPU video pipeline");
                Self::Gpu(gpu)
            }
            Ok(gpu) => {
                let why = gpu
                    .va_caps
                    .can_encode()
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                tracing::warn!(gpu = %gpu.name, %why, "no VA-API or Vulkan Video H.264 encoder; falling back to the CPU pipeline");
                Self::Cpu
            }
            Err(e) => {
                tracing::warn!(error = %e, "no usable GPU; falling back to the CPU pipeline");
                Self::Cpu
            }
        }
    }

    fn encoder_max(&self) -> Result<(u32, u32)> {
        match self {
            Self::Gpu(gpu) => Ok(Encoder::max_size(gpu)?),
            Self::Cpu => Ok(gliff_sw::Encoder::MAX_SIZE),
        }
    }

    fn pipeline(&self) -> VideoPipeline {
        match self {
            Self::Gpu(_) => VideoPipeline::Gpu,
            Self::Cpu => VideoPipeline::Cpu,
        }
    }
}

/// The tier-dependent choices a session starts with, settled by creating
/// the encoder they describe.
struct VideoStart {
    video: VideoTier,
    chroma: ChromaMode,
    stream: (u32, u32),
    encoder_max: (u32, u32),
    ctl: RateController,
    settings: EncoderSettings,
    encoder: VideoEncoder,
}

impl VideoStart {
    fn open(
        video: VideoTier,
        cfg: &Config,
        caps: &ClientCaps,
        output: &SessionOutput,
        fps: u32,
    ) -> Result<Self> {
        // The CPU tier defaults to one 4:2:0 stream: dual-stream 4:4:4
        // doubles the encode work, which the CPU pays for where the GPU
        // does not. A preference only applies when the client advertises
        // the mode.
        let prefer_single =
            cfg.low_bandwidth || (matches!(video, VideoTier::Cpu) && !cfg.full_chroma);
        let chroma = if !caps.chroma.contains(&ChromaMode::Dual420)
            || (prefer_single && caps.chroma.contains(&ChromaMode::Single420))
        {
            ChromaMode::Single420
        } else {
            ChromaMode::Dual420
        };
        let encoder_max = stream_limit(video.encoder_max().context("query encoder limits")?, caps)?;
        let stream = EncoderSettings::fit_extent(output.width, output.height, encoder_max);
        if stream != (output.width, output.height) {
            tracing::info!(
                width = stream.0,
                height = stream.1,
                "scaling the stream to the encoder maximum"
            );
        }
        let streams = if chroma == ChromaMode::Dual420 { 2 } else { 1 };
        let ctl = RateController::new(
            stream.0 as u64 * stream.1 as u64,
            streams,
            MAX_FPS,
            cfg.bitrate,
        );
        let settings = encoder_settings(
            stream.0,
            stream.1,
            ctl.stream_bitrate(),
            fps,
            ctl.vbv_ms(fps),
        );
        let encoder = VideoEncoder::new(&video, &settings, chroma == ChromaMode::Dual420)
            .context("create encoder")?;
        Ok(Self {
            video,
            chroma,
            stream,
            encoder_max,
            ctl,
            settings,
            encoder,
        })
    }
}

enum VideoEncoder {
    Gpu(Box<Encoder>),
    Cpu(Box<gliff_sw::Encoder>),
}

impl VideoEncoder {
    fn new(tier: &VideoTier, settings: &EncoderSettings, dual: bool) -> Result<Self> {
        match tier {
            VideoTier::Gpu(gpu) => Ok(Self::Gpu(Box::new(Encoder::new(
                gpu,
                settings.clone(),
                dual,
            )?))),
            VideoTier::Cpu => Ok(Self::Cpu(Box::new(gliff_sw::Encoder::new(
                gliff_sw::EncoderSettings {
                    width: settings.width,
                    height: settings.height,
                    bitrate: settings.bitrate,
                    framerate: settings.framerate,
                },
                dual,
            )?))),
        }
    }

    /// Change the CBR target, the frame rate it is spread over, and (GPU
    /// only) the rate-control buffer window, from the next frame on. The
    /// CPU encoder has no VBV knob; OpenH264 manages its own buffer. It
    /// also cannot change its rate live — every change re-creates the
    /// encoders and costs a keyframe — so `cpu_rebuild_due` paces the
    /// rebuilds: a congestion cut at once, other moves only when large and
    /// at most every two seconds.
    fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        match self {
            Self::Gpu(enc) => enc.set_rate(bitrate, framerate, vbv_ms),
            Self::Cpu(enc) => {
                let s = enc.settings();
                if cpu_rebuild_due(
                    (s.bitrate, s.framerate),
                    (bitrate, framerate),
                    enc.rebuilt_at().elapsed(),
                ) {
                    enc.set_rate(bitrate, framerate);
                }
            }
        }
    }

    /// Encode a captured frame at `stream` size: the GPU imports the dmabuf
    /// and scales in its split shader; the CPU maps the buffer and scales
    /// the pixels before converting.
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        stream: (u32, u32),
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Gpu(enc) => {
                anyhow::ensure!(
                    !frame.buffer.is_shm(),
                    "the GPU tier needs dmabuf capture, but capture fell back to wl_shm (no linear \
                     dmabuf, no dmabuf global or no render node); run with --video cpu"
                );
                let info = &frame.buffer.info;
                let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc).map_err(|_| {
                    anyhow::anyhow!("capture fourcc {:#x} is not a DRM format", info.fourcc)
                })?;
                let plane = DmabufPlane {
                    fd: info.fd.as_fd(),
                    width: info.width,
                    height: info.height,
                    offset: info.planes[0].offset,
                    stride: info.planes[0].stride,
                    fourcc,
                    modifier: info.modifier,
                };
                // The import is cached per ring buffer; the generation changes
                // when the ring is reallocated (resize), so old imports are
                // never reused.
                let buffer_key = (frame.buffer.generation << 32) | frame.buffer.index as u64;
                Ok(enc.encode_dmabuf_transformed(
                    buffer_key,
                    &plane,
                    frame.transform,
                    force_keyframe,
                )?)
            }
            Self::Cpu(enc) => {
                let t0 = Instant::now();
                let image = frame.read_bgra()?;
                tracing::debug!(
                    read_us = t0.elapsed().as_micros(),
                    "read the captured frame"
                );
                let (w, h) = (stream.0 as usize, stream.1 as usize);
                let pixels = if (image.width, image.height) == (w, h) {
                    image.pixels
                } else {
                    gliff_proto::color::downscale_bgra(
                        &image.pixels,
                        image.width,
                        image.height,
                        w,
                        h,
                    )
                };
                let f = enc.encode_bgra(&pixels, force_keyframe)?;
                Ok(EncodedFrame {
                    main: f.main,
                    aux: f.aux,
                    keyframe: f.keyframe,
                })
            }
        }
    }
}

/// The output being served. A created headless output is removed on drop, on
/// every exit path including a failure later in setup.
struct SessionOutput {
    name: String,
    /// Physical (captured) size.
    width: u32,
    height: u32,
    /// Output scale; the logical size the pointer works in is size / scale.
    scale: f32,
    headless: Option<hypr_ipc::Instance>,
}

impl SessionOutput {
    fn is_headless(&self) -> bool {
        self.headless.is_some()
    }

    /// The logical extent Hyprland exposes to clients and the virtual pointer.
    fn logical_extent(&self) -> InputCmd {
        let s = self.scale.max(0.5);
        InputCmd::SetExtent {
            width: (self.width as f32 / s).round().max(1.0) as u32,
            height: (self.height as f32 / s).round().max(1.0) as u32,
        }
    }
}

impl Drop for SessionOutput {
    fn drop(&mut self) {
        if let Some(instance) = &self.headless {
            let _ = instance.remove_output(&self.name);
        }
    }
}

fn stream_config(
    codec: Codec,
    chroma: ChromaMode,
    pipeline: VideoPipeline,
    output: &SessionOutput,
    stream: (u32, u32),
    view: (u32, u32),
    fps_cap: u32,
) -> ServerMsg {
    // The scale the client divides stream pixels by to reach the remote's
    // logical space: the output scale times any downscale of the stream.
    let effective_scale = output.scale * stream.0 as f32 / output.width.max(1) as f32;
    // Parameter sets ride in-band on every keyframe, so extradata is empty.
    ServerMsg::StreamConfig {
        codec,
        chroma,
        pipeline: Some(pipeline),
        width: stream.0,
        height: stream.1,
        scale_milli: (effective_scale * 1000.0).round() as u32,
        extradata: Vec::new(),
        aux_extradata: None,
        view_width: view.0,
        view_height: view.1,
        fps_cap,
    }
}

fn encoder_settings(
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
    vbv_ms: u32,
) -> EncoderSettings {
    EncoderSettings {
        width,
        height,
        bitrate,
        framerate: fps,
        vbv_ms,
    }
}

/// Pick the named output, or create a headless one sized to the client.
fn setup_output(
    instance: &hypr_ipc::Instance,
    cfg: &Config,
    caps: &ClientCaps,
) -> Result<SessionOutput> {
    if let Some(name) = &cfg.output {
        let mons = instance.monitors()?;
        let m = if name == "auto" {
            main_monitor(&mons)
        } else {
            mons.iter().find(|m| &m.name == name)
        }
        .with_context(|| format!("no output {name}"))?;
        return Ok(SessionOutput {
            name: m.name.clone(),
            width: if m.transform & 1 != 0 {
                m.height
            } else {
                m.width
            } & !1,
            height: if m.transform & 1 != 0 {
                m.width
            } else {
                m.height
            } & !1,
            scale: m.scale,
            headless: None,
        });
    }
    let before: Vec<String> = instance.monitors()?.into_iter().map(|m| m.name).collect();
    let requested = format!("gliff-{}", std::process::id());
    // Hyprland can take longer than the socket timeout to create an output
    // (seen on NVIDIA), and creates it anyway. So a failed request is not
    // the end: wait for the output, and adopt it if it comes.
    let created = instance.create_headless_output(&requested);
    if let Err(e) = &created {
        tracing::warn!(error = %e, "headless output request failed; waiting for the output");
    }
    // Hyprland names the output itself, so find the one that appeared.
    let wait = if created.is_ok() { 4 } else { 50 };
    let mut appeared = None;
    for _ in 0..wait {
        std::thread::sleep(Duration::from_millis(200));
        appeared = instance
            .monitors()?
            .into_iter()
            .find(|m| !before.contains(&m.name))
            .map(|m| m.name);
        if appeared.is_some() {
            break;
        }
    }
    let Some(name) = appeared else {
        return Err(created
            .err()
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("headless output did not appear"))
            .context("create headless output"));
    };
    let mut output = SessionOutput {
        name,
        width: 0,
        height: 0,
        scale: 1.0,
        headless: Some(instance.clone()),
    };
    let width = caps.max_width.clamp(320, 1920) & !1;
    let height = caps.max_height.clamp(240, 1080) & !1;
    if let Err(e) = instance.set_monitor_mode(&output.name, width, height, 60, 1.0) {
        tracing::warn!(error = %e, "could not set the headless output mode");
    }
    // Hyprland applies the mode asynchronously; stream whatever it settled
    // on rather than the size we asked for.
    let mut applied = None;
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(50));
        let mons = instance.monitors()?;
        let Some(m) = mons.iter().find(|m| m.name == output.name) else {
            continue;
        };
        applied = Some((m.width, m.height, m.scale));
        if m.width == width && m.height == height {
            break;
        }
    }
    let (w, h, scale) = applied.context("headless output vanished")?;
    output.width = w & !1;
    output.height = h & !1;
    output.scale = scale;
    Ok(output)
}

/// The screen the user is most likely looking at: the focused monitor, else
/// the leftmost enabled one.
fn main_monitor(mons: &[hypr_ipc::Monitor]) -> Option<&hypr_ipc::Monitor> {
    let enabled = || mons.iter().filter(|m| !m.disabled);
    enabled()
        .find(|m| m.focused)
        .or_else(|| enabled().min_by_key(|m| (m.x, m.y)))
}

fn start_capture(
    target: &Target,
    output: &str,
    render_node: &std::path::Path,
    capture: CaptureMode,
    allow_shm: bool,
    tx: UnboundedSender<Incoming>,
) -> Result<Capturer> {
    let mut cc = CaptureConfig::new(output.to_string());
    cc.target = target.clone();
    cc.render_node = render_node.to_path_buf();
    cc.shm_fallback = allow_shm;
    cc.mode = capture;
    cc.cursor = true;
    // One extra ring slot: the session pins the last sent frame for
    // still-picture refinement.
    cc.buffers = 4;
    let sink = Box::new(move |ev: CaptureEvent| {
        let msg = match ev {
            CaptureEvent::Frame(frame) => Incoming::Frame(frame),
            CaptureEvent::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => Incoming::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            },
            CaptureEvent::CursorPos { x, y, visible } => Incoming::CursorPos {
                x: x as f64,
                y: y as f64,
                visible,
            },
            CaptureEvent::Stopped => Incoming::Stopped,
            CaptureEvent::Error(e) => Incoming::Error(e),
            CaptureEvent::Ready { .. } => return,
        };
        let _ = tx.send(msg);
    });
    Ok(Capturer::start(cc, sink)?)
}

fn start_input(target: &Target, output: &str, keymap: &str) -> Result<Input> {
    let mut ic = InputConfig::new(output.to_string());
    ic.target = target.clone();
    ic.keymap = (!keymap.is_empty()).then(|| keymap.to_string());
    tracing::debug!(bytes = keymap.len(), "client keymap");
    Ok(Input::start(ic, Box::new(|_| {}))?)
}

fn stream_limit(encoder: (u32, u32), caps: &ClientCaps) -> Result<(u32, u32)> {
    let limit = (
        encoder.0.min(caps.max_width),
        encoder.1.min(caps.max_height),
    );
    anyhow::ensure!(
        limit.0 >= 2 && limit.1 >= 2,
        "client has no usable stream dimensions"
    );
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_stream_respects_client_limits_before_first_resize() {
        let caps = ClientCaps {
            max_width: 3840,
            max_height: 2160,
            codecs: vec![Codec::H264],
            chroma: vec![ChromaMode::Single420],
            features: vec![],
        };
        let limit = stream_limit((4096, 4096), &caps).unwrap();
        assert_eq!(EncoderSettings::fit_extent(5120, 1440, limit), (3840, 1080));
        let bad = ClientCaps {
            max_width: 0,
            ..caps
        };
        assert!(stream_limit((4096, 4096), &bad).is_err());
    }
}
