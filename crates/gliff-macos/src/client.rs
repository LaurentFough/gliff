use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
#[cfg(any(target_os = "macos", test))]
use clap::Parser;
use gliff_proto::{
    features, is_incompatible, ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg,
    FEATURE_CURSOR_IN_VIDEO, PROTOCOL_VERSION,
};
#[cfg(target_os = "macos")]
use gliff_transport::spawn_ssh;
use gliff_transport::Framed;
#[cfg(any(target_os = "macos", test))]
use gliff_transport::SshTarget;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::{keys, native};

#[cfg(any(target_os = "macos", test))]
#[derive(Parser)]
#[command(name = "gliff", version, about = "Remote desktop over SSH")]
struct Cli {
    #[arg(long, hide = true)]
    host_bridge: Option<std::path::PathBuf>,
    host: Option<String>,
    /// List nearby Gliff computers as JSON without connecting or opening video.
    #[arg(long, conflicts_with_all = ["host", "connect", "check_frames"])]
    discover: bool,
    /// SSH port (default: SSH configuration, or the selected computer's advertised port).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    #[arg(long)]
    connect: Option<String>,
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    #[arg(long, conflicts_with = "output")]
    headless: bool,
    #[arg(long)]
    output: Option<String>,
    #[arg(long, value_parser = ["cpu", "gpu"])]
    video: Option<String>,
    /// Do not share the clipboard. A choice saved for the machine in
    /// Settings still wins, as on Linux.
    #[arg(long)]
    no_clipboard: bool,
    /// Decode a fixed number of frames without opening a window (integration test).
    #[arg(long)]
    check_frames: Option<u32>,
}

#[cfg(any(target_os = "macos", test))]
impl Cli {
    fn ssh_target(&self, selected: &crate::discovery::Selection) -> SshTarget {
        let mut target = SshTarget::new(selected.host.clone());
        target.server_bin = self.server_bin.clone();
        let port = self
            .port
            .or_else(|| self.host.is_none().then_some(selected.port));
        if let Some(port) = port {
            target.ssh_args.extend(["-p".into(), port.to_string()]);
        }
        target
    }
}

#[cfg(target_os = "macos")]
pub fn client_main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&rt, async move {
        if let Some(path) = &cli.host_bridge {
            return crate::host_bridge::app(path).await;
        }
        if cli.discover {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::discovery::list().await?)?
            );
            return Ok(());
        }
        let store = if cli.check_frames.is_some() {
            //= Integration runs neither read nor write the user's settings.
            gliff_settings::Store::in_memory()
        } else {
            gliff_settings::Store::open()
        };
        let shared = crate::settings::Shared::new(store);
        let cli_layer = crate::settings::cli_layer(
            cli.headless,
            cli.no_clipboard,
            &cli.server_bin,
            cli.video.as_deref(),
        );
        let selected = match cli.host.as_deref() {
            Some(h) => crate::discovery::Selection {
                host: h.into(),
                port: cli.port.unwrap_or(22),
                screen: crate::discovery::ScreenMode::Mirror,
            },
            None if cli.connect.is_some() => crate::discovery::Selection {
                host: String::new(),
                port: 22,
                screen: crate::discovery::ScreenMode::Mirror,
            },
            None => {
                let screen = if cli.headless {
                    Some(crate::discovery::ScreenMode::Private)
                } else if cli.output.is_some() {
                    Some(crate::discovery::ScreenMode::Mirror)
                } else {
                    None
                };
                let Some(selection) = crate::discovery::choose(screen, &shared, &cli_layer).await?
                else {
                    return Ok(());
                };
                selection
            }
        };
        let host = selected.host.clone();
        ensure!(
            !host.is_empty() || cli.connect.is_some(),
            "enter an SSH address"
        );
        if cli.connect.is_none() {
            selected.validate()?;
        }
        let machine = crate::settings::machine_key(match &cli.connect {
            Some(addr) if host.is_empty() => addr,
            _ => &host,
        });
        if cli.check_frames.is_none() {
            shared.remember(&machine);
        }
        let screen_locked = cli.output.is_some();
        let mut viewer = match cli.check_frames {
            None => {
                Some(Viewer::open(&format!("Gliff — {host} · Shift+Esc releases keyboard")).await?)
            }
            Some(_) => None,
        };
        //= One window for the whole visit: a settings change that needs a new
        //= session reconnects inside it.
        let result = loop {
            let opts = shared.resolve(&machine, &cli_layer);
            let live = viewer.as_mut().map(|viewer| Live {
                //= `--video cpu` or a saved CPU decode keeps the CPU path.
                gpu: viewer.gpu && opts.decode != gliff_settings::Video::Cpu,
                viewer,
                shared: shared.clone(),
                machine: machine.clone(),
                cli: cli_layer.clone(),
                opts: opts.clone(),
                screen_locked,
            });
            let outcome = if let Some(addr) = &cli.connect {
                match tokio::net::TcpStream::connect(addr).await {
                    Ok(stream) => {
                        stream.set_nodelay(true)?;
                        let (rd, wr) = tokio::io::split(stream);
                        run_session(rd, wr, &host, cli.check_frames, live).await
                    }
                    Err(e) => Err(e.into()),
                }
            } else {
                let mut target = cli.ssh_target(&selected);
                target.server_bin = opts.server_bin.clone();
                if cli.check_frames.is_none() {
                    target.askpass = Some(native::helper_path()?);
                }
                if let Some(output) = &cli.output {
                    target
                        .server_args
                        .extend(["--output".into(), output.clone()]);
                } else if opts.screen == gliff_settings::Screen::Own {
                    target.server_args.push("--headless".into());
                }
                match spawn_ssh(&target) {
                    Ok(mut ssh) => {
                        let r =
                            run_session(ssh.stdout, ssh.stdin, &host, cli.check_frames, live).await;
                        let _ = ssh.child.kill().await;
                        r
                    }
                    Err(e) => Err(e.into()),
                }
            };
            match outcome {
                Ok(Outcome::Reconnect) => {
                    tracing::info!(%machine, "settings changed; reconnecting");
                    if let Some(v) = &viewer {
                        v.message("Reconnecting with the new settings…").await;
                    }
                }
                Ok(Outcome::Done) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        if let Some(viewer) = viewer.take() {
            viewer.finish(&result).await;
        }
        result
    })
}

/// How a session ended without an error.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The window closed or the integration run finished.
    Done,
    /// A setting changed that the session was started with.
    Reconnect,
}

/// The viewer window, kept across reconnects: its helper process and the
/// two halves of its bridge. Only `client_main` (macOS) opens one; tests run
/// sessions without a window.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Viewer {
    child: Option<tokio::process::Child>,
    display: mpsc::Sender<(u8, Vec<u8>)>,
    events: Events,
    /// The helper can decode in hardware (VideoToolbox + Metal).
    gpu: bool,
}

/// The window's events, with any read ahead of time (while waiting for the
/// helper's `video_caps`) delivered first.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Events {
    rx: mpsc::Receiver<Result<Value>>,
    pending: std::collections::VecDeque<Result<Value>>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Events {
    async fn recv(&mut self) -> Option<Result<Value>> {
        match self.pending.pop_front() {
            Some(ev) => Some(ev),
            None => self.rx.recv().await,
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Viewer {
    #[cfg(target_os = "macos")]
    async fn open(title: &str) -> Result<Self> {
        let mut child = native::spawn("app", &[title.to_string()])?;
        let (display, mut display_rx) = mpsc::channel::<(u8, Vec<u8>)>(2);
        let (event_tx, events) = mpsc::channel::<Result<Value>>(64);
        let mut rd = child.stdout.take().context("viewer stdout")?;
        let mut wr = child.stdin.take().context("viewer stdin")?;
        tokio::task::spawn_local(async move {
            while let Some((kind, bytes)) = display_rx.recv().await {
                native::write(&mut wr, kind, &bytes).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        tokio::task::spawn_local(async move {
            loop {
                let ev = async {
                    let (kind, data) = native::read(&mut rd).await?;
                    ensure!(kind == native::EVENT, "unexpected UI packet");
                    Ok(serde_json::from_slice(&data)?)
                }
                .await;
                let stopped = ev.is_err();
                if event_tx.send(ev).await.is_err() || stopped {
                    break;
                }
            }
        });
        //= The helper's first event says whether it can decode in hardware.
        //= An older helper sends none: anything else read here is kept.
        let mut events = Events {
            rx: events,
            pending: Default::default(),
        };
        let mut gpu = false;
        match tokio::time::timeout(Duration::from_secs(5), events.rx.recv()).await {
            Ok(Some(Ok(ev))) if ev["type"] == "video_caps" => {
                gpu = ev["hardware"].as_bool() == Some(true);
            }
            Ok(Some(other)) => events.pending.push_back(other),
            Ok(None) | Err(_) => {}
        }
        tracing::info!(hardware = gpu, "viewer video decode");
        Ok(Self {
            child: Some(child),
            display,
            events,
            gpu,
        })
    }

    async fn command(&self, value: Value) -> Result<()> {
        self.display
            .send((native::EVENT, serde_json::to_vec(&value)?))
            .await
            .map_err(|_| anyhow::anyhow!("window closed"))
    }

    /// Replace the picture with a line of text.
    async fn message(&self, text: &str) {
        let _ = self.command(json!({ "message": text })).await;
    }

    /// Show a failure and wait for the user to close the window; then stop
    /// the helper.
    async fn finish(mut self, result: &Result<()>) {
        if let Err(error) = result {
            self.message(&format!("{error:#}\nClose this window and connect again."))
                .await;
            while let Some(Ok(event)) = self.events.recv().await {
                if event["type"] == "bye" {
                    break;
                }
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}

/// What a windowed session needs beyond the connection: the window, and the
/// settings it was started with, to follow changes made while it runs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Live<'a> {
    viewer: &'a mut Viewer,
    /// Decode in the viewer (VideoToolbox + Metal) instead of on the CPU.
    gpu: bool,
    shared: crate::settings::SharedRef,
    machine: String,
    cli: gliff_settings::Settings,
    opts: gliff_settings::Resolved,
    screen_locked: bool,
}

/// A session without a window (integration checks and tests).
#[cfg(test)]
async fn session<R, W>(rd: R, wr: W, host: &str, check: Option<u32>) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    run_session(rd, wr, host, check, None).await.map(|_| ())
}

async fn run_session<R, W>(
    rd: R,
    wr: W,
    host: &str,
    check: Option<u32>,
    live: Option<Live<'_>>,
) -> Result<Outcome>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);
    //= Hardware decode makes full colour cheap, so ask for it first.
    let mut gpu = live.as_ref().is_some_and(|l| l.gpu);
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: if check.is_none() {
                local_keymap().await?
            } else {
                String::new()
            },
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 3840,
                max_height: 2160,
                chroma: if gpu {
                    vec![ChromaMode::Dual420, ChromaMode::Single420]
                } else {
                    vec![ChromaMode::Single420, ChromaMode::Dual420]
                },
                features: features()
                    .into_iter()
                    //= Sound is Linux-only so far: do not offer it from a Mac.
                    .filter(|f| f != gliff_proto::FEATURE_AUDIO)
                    .chain([FEATURE_CURSOR_IN_VIDEO.to_string()])
                    .collect(),
            },
        })
        .await?;
    //= Split the window into its parts so each select! branch borrows only
    //= what it uses. Without a window, frames go to a channel nobody reads.
    let visible = live.is_some();
    let (display_tx, _unread_display) = match &live {
        Some(l) => (l.viewer.display.clone(), None),
        None => {
            let (tx, rx) = mpsc::channel::<(u8, Vec<u8>)>(2);
            (tx, Some(rx))
        }
    };
    let (mut events, mut ctx) = match live {
        Some(Live {
            viewer,
            shared,
            machine,
            cli,
            opts,
            screen_locked,
            ..
        }) => (
            Some(&mut viewer.events),
            Some((shared, machine, cli, opts, screen_locked)),
        ),
        None => (None, None),
    };
    let mut changes = ctx.as_ref().map(|(shared, ..)| shared.subscribe());
    let mut share_clipboard = ctx.as_ref().is_none_or(|(.., opts, _)| opts.clipboard);
    if let Some((_, _, _, opts, locked)) = &ctx {
        let state = crate::settings::machine_state(opts, *locked);
        display_tx
            .send((native::EVENT, serde_json::to_vec(&state)?))
            .await?;
        //= A reconnect needs the window's size again (a private screen
        //= follows it); a first session gets it from the window's start.
        display_tx
            .send((
                native::EVENT,
                serde_json::to_vec(&json!({"type": "report_size"}))?,
            ))
            .await?;
    }
    let (out, mut outgoing) = mpsc::channel::<(ClientMsg, Bytes)>(64);
    let mut writes = tokio::task::spawn_local(async move {
        while let Some((msg, payload)) = outgoing.recv().await {
            writer.write_msg_with_payloads(&msg, &[&payload]).await?;
        }
        Ok::<_, gliff_transport::Error>(())
    });
    let (incoming_tx, mut incoming) = mpsc::channel::<Result<(ServerMsg, Bytes)>>(8);
    let reads = tokio::task::spawn_local(async move {
        loop {
            let msg = async {
                let m = reader.read_msg::<ServerMsg>().await?;
                let p = reader.read_payload(server_payload(&m)).await?;
                Ok((m, p))
            }
            .await;
            let stopped = msg.is_err();
            if incoming_tx.send(msg).await.is_err() || stopped {
                break;
            }
        }
    });
    let mut decoder = None;
    let mut coded_stream = None;
    let mut scale = 1000;
    let mut frames = 0;
    let mut got_hello = false;
    let mut idle = tokio::time::interval(Duration::from_millis(100));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_id = 0;
    //= Consecutive hardware decode failures; past a few, use the CPU.
    let mut gpu_errors = 0u32;
    let clipboard =
        crate::clipboard::Bridge::new(gliff_transport::clipboard::Side::Client, display_tx.clone());
    let mut clips = clipboard.take_outbound();
    let mut deadline = Box::pin(tokio::time::sleep(Duration::from_secs(20)));
    let result: Result<Outcome> = async {
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                // An idle Linux desktop sends no video after refinement.
                // Ask for a reply so the response timeout measures liveness.
                out.send((ClientMsg::Ping { t: now_ms() }, Bytes::new())).await?;
            }
            _ = &mut deadline => bail!("server stopped responding for 20 seconds"),
            result = &mut writes => { result??; bail!("network writer stopped"); }
            ev = async { events.as_mut().unwrap().recv().await }, if events.is_some() => {
                let ev = ev.context("window closed")??;
                if ev["type"] == "bye" { out.send((ClientMsg::Bye, Bytes::new())).await?; break; }
                if ev["type"] == "clipboard" { if share_clipboard { clipboard.local(&ev).await?; } }
                else if ev["type"] == "paste" { if share_clipboard { clipboard.paste(&ev); } }
                else if ev["type"] == "open_settings" {
                    if let Some((shared, ..)) = &ctx {
                        let shared = shared.clone();
                        tokio::task::spawn_local(async move {
                            if let Err(e) = crate::settings::window(shared).await {
                                tracing::warn!(error = %e, "settings window failed");
                            }
                        });
                    }
                }
                else if ev["type"] == "setting" {
                    //= The Machine menu; the change arrives back on `changes`.
                    if let Some((shared, machine, ..)) = &ctx {
                        if let Err(e) = crate::settings::apply_machine_setting(shared, machine, &ev) {
                            tracing::warn!(error = %e, "menu setting refused");
                        }
                    }
                }
                else if ev["type"] == "decoded" {
                    //= The viewer finished this frame (decode + recombine):
                    //= acknowledge it now, so the server paces on real decode time.
                    if let Some(frame_id) = ev["frame_id"].as_u64() {
                        gpu_errors = 0;
                        frames += 1;
                        out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                    }
                }
                else if ev["type"] == "decode_error" || ev["type"] == "gpu_failed" {
                    let message = ev["message"].as_str().unwrap_or("unknown");
                    if !gpu {
                        //= Frames still in the pipe after the switch to the CPU:
                        //= acknowledge them; the keyframe is already requested.
                        if let Some(frame_id) = ev["frame_id"].as_u64() {
                            out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                        }
                        continue;
                    }
                    gpu_errors += 1;
                    if ev["type"] == "gpu_failed" || gpu_errors >= 3 {
                        tracing::warn!(%message, "hardware decode failed; using the CPU");
                        gpu = false;
                        if let Some((width, height, chroma)) = coded_stream {
                            decoder = Some(gliff_sw::Decoder::new(chroma == ChromaMode::Dual420, width, height)?);
                        }
                    } else {
                        tracing::warn!(%message, "hardware decode error; requesting keyframe");
                    }
                    if let Some(frame_id) = ev["frame_id"].as_u64() {
                        out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                    }
                    out.send((ClientMsg::RequestKeyframe, Bytes::new())).await?;
                }
                else if ev["type"] == "reconnect" {
                    out.send((ClientMsg::Bye, Bytes::new())).await?;
                    return Ok(Outcome::Reconnect);
                }
                else if ev["type"] == "keyboard_layout" {
                    let keymap = crate::keymap::from_event(&ev)?;
                    out.send((ClientMsg::Keymap { keymap }, Bytes::new())).await?;
                }
                else if let Some(msg) = input(&ev) { out.send((msg, Bytes::new())).await?; }
            }
            Some((msg, payload)) = clips.recv() => { out.send((ClientMsg::from(msg), payload)).await?; }
            changed = async { changes.as_mut().unwrap().changed().await }, if changes.is_some() => {
                if changed.is_err() { changes = None; continue; }
                let Some((shared, machine, cli, opts, locked)) = &mut ctx else { continue };
                let new = shared.resolve(machine, cli);
                match crate::settings::change(opts, &new, *locked) {
                    crate::settings::Change::Reconnect => {
                        out.send((ClientMsg::Bye, Bytes::new())).await?;
                        return Ok(Outcome::Reconnect);
                    }
                    crate::settings::Change::Clipboard(on) => {
                        share_clipboard = on;
                        //= Turning sharing off withdraws what this Mac offered.
                        if !on { clipboard.local(&json!({})).await?; }
                        tracing::info!(share = on, "clipboard sharing changed");
                    }
                    crate::settings::Change::None => {}
                }
                *opts = new;
                let state = crate::settings::machine_state(opts, *locked);
                display_tx.send((native::EVENT, serde_json::to_vec(&state)?)).await?;
            }
            _ = idle.tick(), if decoder.is_some() => {
                let decoder: &mut gliff_sw::Decoder = decoder.as_mut().unwrap();
                if decoder.has_pending() {
                    if let Some(frame) = decoder.flush()? {
                        show(&display_tx, visible, frame, scale).await?;
                        frames += 1;
                        out.send((ClientMsg::FrameAck { frame_id: last_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                    }
                }
            }
            packet = incoming.recv() => {
                let (msg, payload) = packet.context("server closed")??;
                deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(20));
                match msg.into_clipboard() {
                    Ok(msg) => { if share_clipboard { clipboard.peer(msg, payload) } }
                    Err(msg) => match msg {
                        ServerMsg::HelloAck { version, session, features, .. } => {
                            ensure!(version == PROTOCOL_VERSION, "incompatible server protocol {version}");
                            got_hello = true;
                            if visible {
                                let command = json!({"type":"cursor_mode", "embedded":cursor_in_video(session.headless, &features)});
                                display_tx.send((native::EVENT, serde_json::to_vec(&command)?)).await?;
                            }
                        }
                        ServerMsg::StreamConfig { codec, chroma, width, height, scale_milli, .. } => {
                            ensure!(got_hello && codec == Codec::H264 && width > 0 && height > 0 && scale_milli > 0, "unsupported stream configuration");
                            ensure!(matches!(chroma, ChromaMode::Single420 | ChromaMode::Dual420), "unsupported chroma mode");
                            // Pace, scale and pipeline metadata can change without
                            // a new coded stream or keyframe. Preserve references.
                            if coded_stream != Some((width, height, chroma)) {
                                coded_stream = Some((width, height, chroma));
                                if gpu {
                                    //= The viewer builds its decoders before this stream's frames.
                                    let config = json!({"type": "video_config", "width": width, "height": height,
                                        "dual": chroma == ChromaMode::Dual420, "scale_milli": scale_milli});
                                    display_tx.send((native::EVENT, serde_json::to_vec(&config)?)).await?;
                                    decoder = None;
                                } else {
                                    decoder = Some(gliff_sw::Decoder::new(chroma == ChromaMode::Dual420, width, height)?);
                                }
                            } else if gpu && scale != scale_milli {
                                let command = json!({"type": "video_scale", "scale_milli": scale_milli});
                                display_tx.send((native::EVENT, serde_json::to_vec(&command)?)).await?;
                            }
                            scale = scale_milli;
                            tracing::info!(width, height, scale_milli, ?chroma, gpu, "stream configured");
                        }
                        ServerMsg::VideoFrame { frame_id, data_len, aux_len, .. } if gpu => {
                            ensure!(payload.len() == data_len as usize + aux_len as usize, "invalid video payload");
                            ensure!(coded_stream.is_some(), "video before configuration");
                            last_id = frame_id;
                            //= Coded frames are never dropped (a lost P-frame corrupts
                            //= the next ones): wait for the window instead. The ack
                            //= follows its `decoded` event.
                            let packet = native::video_packet(frame_id, data_len, &payload)?;
                            display_tx.send((native::VIDEO, packet)).await?;
                        }
                        ServerMsg::VideoFrame { frame_id, data_len, aux_len, .. } => {
                            ensure!(payload.len() == data_len as usize + aux_len as usize, "invalid video payload");
                            let decoder = decoder.as_mut().context("video before configuration")?;
                            last_id = frame_id;
                            match decoder.decode(&payload[..data_len as usize], &payload[data_len as usize..]) {
                                Ok(Some(frame)) => { show(&display_tx, visible, frame, scale).await?; frames += 1; }
                                Ok(None) => {}
                                Err(e) => { tracing::warn!(error = %e, "decode failed; requesting keyframe"); out.send((ClientMsg::RequestKeyframe, Bytes::new())).await?; }
                            }
                            out.send((ClientMsg::FrameAck { frame_id, decoded_at_ms: now_ms() }, Bytes::new())).await?;
                        }
                        ServerMsg::Ping { t } => { out.send((ClientMsg::Pong { t }, Bytes::new())).await?; }
                        ServerMsg::Error { code, message, .. } => bail!("{}: {message}", if is_incompatible(code) { "incompatible server" } else { "server error" }),
                        _ => {}
                    }
                }
            }
        }
        if check.is_some_and(|n| frames >= n) {
            out.send((ClientMsg::Bye, Bytes::new())).await?;
            println!("PASS: decoded {frames} frames from {host}");
            break;
        }
    }
    Ok(Outcome::Done)
    }.await;
    // Stop the transport immediately on failure, even while its error window
    // remains open. EOF tears down the remote host and releases held input.
    reads.abort();
    writes.abort();
    result
}

async fn show(
    tx: &mpsc::Sender<(u8, Vec<u8>)>,
    visible: bool,
    frame: gliff_sw::BgraFrame,
    scale: u32,
) -> Result<()> {
    ensure!(
        frame.width > 0
            && frame.height > 0
            && frame.pixels.len() == frame.width as usize * frame.height as usize * 4,
        "invalid decoded frame"
    );
    if visible {
        let mut pixels = Vec::with_capacity(12 + frame.pixels.len());
        for n in [frame.width, frame.height, scale] {
            pixels.extend_from_slice(&n.to_le_bytes());
        }
        pixels.extend_from_slice(&frame.pixels);
        // A slow display must not stop network reads or accumulate stale frames.
        match tx.try_send((native::PIXELS, pixels)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("window closed"),
        }
    }
    Ok(())
}

fn input(ev: &Value) -> Option<ClientMsg> {
    match ev["type"].as_str()? {
        "key" => Some(ClientMsg::Key {
            keycode: keys::evdev(u16::try_from(ev["keycode"].as_u64()?).ok()?)?,
            pressed: ev["pressed"].as_bool()?,
        }),
        "motion" => Some(ClientMsg::PointerMotion {
            x: ev["x"].as_f64()?,
            y: ev["y"].as_f64()?,
        }),
        "button" => Some(ClientMsg::PointerButton {
            button: match ev["button"].as_u64()? {
                0 => 272,
                1 => 273,
                2 => 274,
                _ => return None,
            },
            pressed: ev["pressed"].as_bool()?,
        }),
        "resize" => {
            let width = u32::try_from(ev["width"].as_u64()?).ok()?;
            let height = u32::try_from(ev["height"].as_u64()?).ok()?;
            let scale = ev["scale"].as_f64()?;
            if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
                return None;
            }
            // Cap Retina viewports proportionally: independent width/height
            // clamps change the remote aspect ratio and create letterboxing.
            let ratio = (3840.0 / f64::from(width))
                .min(2160.0 / f64::from(height))
                .min(1.0);
            Some(ClientMsg::Resize {
                width: (f64::from(width) * ratio).round().max(2.0) as u32,
                height: (f64::from(height) * ratio).round().max(2.0) as u32,
                scale: (scale * ratio) as f32,
            })
        }
        "axis" => Some(ClientMsg::PointerAxis {
            axis: if ev["horizontal"].as_bool()? {
                gliff_proto::Axis::Horizontal
            } else {
                gliff_proto::Axis::Vertical
            },
            value: ev["value"].as_f64()?,
            discrete: None,
            stop: false,
        }),
        _ => None,
    }
}

async fn local_keymap() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let mut child = native::spawn("keyboard-layout", &[])?;
        let result = async {
            let (kind, bytes) = tokio::time::timeout(
                Duration::from_secs(5),
                native::read(child.stdout.as_mut().unwrap()),
            )
            .await
            .context("Mac keyboard layout query timed out")??;
            ensure!(kind == native::EVENT, "invalid keyboard layout response");
            crate::keymap::from_event(&serde_json::from_slice(&bytes)?)
        }
        .await;
        let _ = child.kill().await;
        result
    }
    #[cfg(not(target_os = "macos"))]
    Ok(String::new())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn cursor_in_video(headless: bool, features: &[String]) -> bool {
    // Older physical-output servers ignore the request and send cursor
    // shapes separately. Headless Hyprland frames already paint the cursor.
    headless || features.iter().any(|f| f == FEATURE_CURSOR_IN_VIDEO)
}

fn server_payload(m: &ServerMsg) -> u32 {
    match m {
        ServerMsg::VideoFrame {
            data_len, aux_len, ..
        } => data_len.saturating_add(*aux_len),
        ServerMsg::CursorShape { argb_len, .. } => *argb_len,
        ServerMsg::ClipboardData { data_len, .. } => *data_len,
        //= The Mac does not ask for sound, but a server that sends it anyway
        //= must not desynchronise the stream: read past the payload.
        ServerMsg::AudioFrame { data_len, .. } => *data_len,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_payloads_are_skipped_not_parsed() {
        let frame = ServerMsg::AudioFrame {
            seq: 7,
            data_len: 960,
        };
        assert_eq!(server_payload(&frame), 960);
    }

    #[cfg(unix)]
    #[test]
    fn ssh_config_port_is_preserved_unless_selected_or_overridden() {
        use std::io::Write;
        for (args, expected) in [
            (vec!["gliff", "office"], "port 2222"),
            (vec!["gliff"], "port 4422"),
            (vec!["gliff", "office", "--port", "9922"], "port 9922"),
            (vec!["gliff", "--port", "9922"], "port 9922"),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let selected = crate::discovery::Selection {
                host: "office".into(),
                port: 4422,
                screen: crate::discovery::ScreenMode::Mirror,
            };
            let target = cli.ssh_target(&selected);
            let mut ssh = std::process::Command::new("ssh")
                .args(["-G", "-F", "/dev/stdin"])
                .args(&target.ssh_args)
                .arg(&target.host)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            ssh.stdin
                .take()
                .unwrap()
                .write_all(b"Host office\n  HostName 127.0.0.1\n  Port 2222\n")
                .unwrap();
            let output = ssh.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == expected));
        }
    }

    #[tokio::test]
    async fn pace_reconfiguration_keeps_decoder_reference_frames() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, server) = tokio::io::duplex(65536);
                let (client_rd, client_wr) = tokio::io::split(client);
                let (server_rd, server_wr) = tokio::io::split(server);
                let server = tokio::task::spawn_local(async move {
                    let mut rd = Framed::new(server_rd);
                    let mut wr = Framed::new(server_wr);
                    rd.read_msg::<ClientMsg>().await?;
                    wr.write_msg(&ServerMsg::HelloAck {
                        version: PROTOCOL_VERSION,
                        session: gliff_proto::SessionInfo {
                            headless: false,
                            output: "fixture".into(),
                        },
                        outputs: vec![],
                        features: vec![],
                    })
                    .await?;
                    let config = |fps_cap| ServerMsg::StreamConfig {
                        codec: Codec::H264,
                        chroma: ChromaMode::Single420,
                        pipeline: Some(gliff_proto::VideoPipeline::Cpu),
                        width: 16,
                        height: 16,
                        scale_milli: 1000,
                        extradata: vec![],
                        aux_extradata: None,
                        view_width: 16,
                        view_height: 16,
                        fps_cap,
                    };
                    wr.write_msg(&config(30)).await?;
                    let mut encoder = gliff_sw::Encoder::new(
                        gliff_sw::EncoderSettings {
                            width: 16,
                            height: 16,
                            bitrate: 100000,
                            framerate: 30,
                        },
                        false,
                    )?;
                    for frame_id in 1..=8 {
                        if frame_id == 4 {
                            wr.write_msg(&config(10)).await?;
                        }
                        let frame = encoder.encode_bgra(
                            &vec![(frame_id * 20) as u8; 16 * 16 * 4],
                            frame_id == 1,
                        )?;
                        wr.write_msg_with_payloads(
                            &ServerMsg::VideoFrame {
                                frame_id,
                                pts_us: frame_id * 33000,
                                keyframe: frame.keyframe,
                                damage: vec![],
                                data_len: frame.main.len() as u32,
                                aux_len: 0,
                            },
                            &[&frame.main],
                        )
                        .await?;
                    }
                    // Keep the pipe open while the decoder drains its pending picture.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok::<_, anyhow::Error>(())
                });
                let result = tokio::time::timeout(
                    Duration::from_secs(3),
                    session(client_rd, client_wr, "fixture", Some(4)),
                )
                .await?;
                server.abort();
                result
            })
            .await
    }
    #[tokio::test(start_paused = true)]
    async fn idle_connection_stays_alive_with_pong_replies() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, server) = tokio::io::duplex(65536);
                let (client_rd, client_wr) = tokio::io::split(client);
                let (server_rd, server_wr) = tokio::io::split(server);
                let peer = tokio::task::spawn_local(async move {
                    let mut rd = Framed::new(server_rd);
                    let mut wr = Framed::new(server_wr);
                    rd.read_msg::<ClientMsg>().await?;
                    wr.write_msg(&ServerMsg::HelloAck {
                        version: PROTOCOL_VERSION,
                        session: gliff_proto::SessionInfo {
                            headless: false,
                            output: "idle".into(),
                        },
                        outputs: vec![],
                        features: vec![],
                    })
                    .await?;
                    let started = tokio::time::Instant::now();
                    let mut pings = 0;
                    while let ClientMsg::Ping { t } = rd.read_msg::<ClientMsg>().await? {
                        wr.write_msg(&ServerMsg::Pong {
                            t,
                            server_now_ms: now_ms(),
                        })
                        .await?;
                        pings += 1;
                        if started.elapsed() >= Duration::from_secs(30) {
                            return Ok::<_, anyhow::Error>(pings);
                        }
                    }
                    anyhow::bail!("unexpected idle input")
                });
                let result = session(client_rd, client_wr, "idle", Some(1)).await;
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("connection closed"));
                assert!(peer.await?? >= 7);
                Ok(())
            })
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_still_times_out() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, _silent_peer) = tokio::io::duplex(65536);
                let (rd, wr) = tokio::io::split(client);
                let started = tokio::time::Instant::now();
                let result = session(rd, wr, "silent", Some(1)).await;
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("stopped responding"));
                assert_eq!(started.elapsed(), Duration::from_secs(20));
                Ok(())
            })
            .await
    }

    #[test]
    fn cursor_visibility_follows_the_server_capability() {
        assert!(!cursor_in_video(false, &[])); // older mirrored server
        assert!(cursor_in_video(true, &[])); // older headless server
        assert!(cursor_in_video(false, &[FEATURE_CURSOR_IN_VIDEO.into()]));
    }

    #[test]
    fn native_positions_become_evdev_and_pointer_messages() {
        assert_eq!(
            input(&json!({"type":"key", "keycode":0, "pressed":true})),
            Some(ClientMsg::Key {
                keycode: 30,
                pressed: true
            })
        );
        assert_eq!(
            input(&json!({"type":"button", "button":1, "pressed":false})),
            Some(ClientMsg::PointerButton {
                button: 273,
                pressed: false
            })
        );
        assert_eq!(
            input(&json!({"type":"motion", "x":100.5, "y":200.0})),
            Some(ClientMsg::PointerMotion { x: 100.5, y: 200.0 })
        );
        assert_eq!(
            input(&json!({"type":"key", "keycode":65536, "pressed":true})),
            None
        );
    }

    #[test]
    fn retina_resize_preserves_aspect_and_logical_coordinates() {
        for (width, height, scale) in [(2940, 1912, 2.0), (4112, 2658, 2.0), (6016, 3384, 2.0)] {
            let Some(ClientMsg::Resize {
                width: w,
                height: h,
                scale: s,
            }) = input(&json!({"type":"resize", "width":width, "height":height, "scale":scale}))
            else {
                panic!("missing resize")
            };
            assert!(w <= 3840 && h <= 2160);
            assert!(
                (f64::from(w) / f64::from(h) - f64::from(width) / f64::from(height)).abs() < 0.001
            );
            assert!((f64::from(w) / f64::from(s) - f64::from(width) / scale).abs() < 1.0);
            assert!((f64::from(h) / f64::from(s) - f64::from(height) / scale).abs() < 1.0);
        }
        assert!(input(&json!({"type":"resize", "width":0, "height":720, "scale":2})).is_none());
        assert!(input(&json!({"type":"resize", "width":1100, "height":720, "scale":0})).is_none());
    }
}
