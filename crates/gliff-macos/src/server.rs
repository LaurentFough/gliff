use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use clap::Parser;
use gliff_proto::{
    features, version_mismatch, ChromaMode, ClientMsg, Codec, Greeting, OutputInfo, ServerMsg,
    SessionInfo, ERROR_NO_CHROMA, ERROR_NO_CODEC, ERROR_VERSION, FEATURE_CURSOR_IN_VIDEO,
    PROTOCOL_VERSION,
};
use gliff_transport::Framed;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::{encoder::Encoder, keys, native};

#[derive(Parser, serde::Serialize, serde::Deserialize)]
#[command(
    name = "gliff-server",
    version,
    about = "Serve a macOS desktop over SSH"
)]
pub(crate) struct Cli {
    #[arg(long, conflicts_with = "listen")]
    stdio: bool,
    #[arg(long)]
    listen: Option<String>,
    #[arg(long, default_value = "auto", conflicts_with = "headless")]
    output: String,
    #[arg(long)]
    headless: bool,
    #[arg(long)]
    low_bandwidth: bool,
    #[arg(long)]
    full_chroma: bool,
    #[arg(long)]
    bitrate: Option<u32>,
    #[arg(long, value_parser = ["cpu", "gpu"])]
    video: Option<String>,
    /// List the physical displays available to mirror.
    #[arg(long)]
    outputs: bool,
    /// Check Screen Recording and Accessibility grants.
    #[arg(long)]
    permissions: bool,
    /// Ask macOS to show the permission prompts on the host.
    #[arg(long, requires = "permissions")]
    request: bool,
    #[arg(long, hide = true)]
    install_native: Option<std::path::PathBuf>,
}

pub fn server_main() -> Result<()> {
    let mut cli = Cli::parse();
    if let Some(path) = &cli.install_native {
        return native::install_helper(path);
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    // LaunchServices does not inherit the SSH process's environment. Resolve
    // the video preference before serializing it into the host bridge.
    cli.video = Some(
        match gliff_sw::VideoMode::resolve(cli.video.as_deref()) {
            gliff_sw::VideoMode::Cpu => "cpu",
            gliff_sw::VideoMode::Gpu => "gpu",
        }
        .into(),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = tokio::task::LocalSet::new().block_on(&rt, crate::host_bridge::relay(&cli));
    // Tokio's stdin reader uses an uncancellable blocking read. If the app
    // fails while SSH stdin is still open, Runtime::drop would wait forever
    // and prevent the client from receiving the final error/EOF.
    rt.shutdown_timeout(Duration::from_millis(100));
    result
}

pub(crate) async fn run<R, W>(rd: R, mut wr: W, cli: &Cli) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    if cli.outputs || cli.permissions {
        let args = if cli.request {
            vec!["--request".into()]
        } else {
            vec![]
        };
        let mut helper = native::spawn(
            if cli.outputs {
                "outputs"
            } else {
                "permissions"
            },
            &args,
        )?;
        let mut stdout = helper.stdout.take().unwrap();
        tokio::io::copy(&mut stdout, &mut wr).await?;
        ensure!(
            helper.wait().await?.success(),
            "macOS host permissions are not granted"
        );
        return Ok(());
    }
    if cli.stdio {
        use std::io::IsTerminal;
        ensure!(!std::io::stdout().is_terminal(), "--stdio requires a pipe");
    }
    if let Some(addr) = &cli.listen {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let (socket, _) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let (rd, wr) = tokio::io::split(socket);
        session(rd, wr, cli).await
    } else {
        session(rd, wr, cli).await
    }
}

async fn session<R, W>(rd: R, wr: W, cfg: &Cli) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);
    let caps = match reader.read_msg::<Greeting>().await? {
        Greeting::Hello { caps, .. } => caps,
        Greeting::OtherVersion(v) => {
            let m = version_mismatch(v, PROTOCOL_VERSION);
            writer
                .write_msg(&ServerMsg::error(ERROR_VERSION, m))
                .await?;
            bail!("{m}");
        }
    };
    if cfg.headless {
        writer
            .write_msg(&ServerMsg::error(
                256,
                "macOS supports mirroring physical displays; --headless requires a Hyprland host",
            ))
            .await?;
        bail!("macOS does not provide Hyprland headless outputs");
    }
    let error = if !caps.codecs.contains(&Codec::H264) {
        Some((ERROR_NO_CODEC, "No video codec in common"))
    } else if !caps
        .chroma
        .iter()
        .any(|c| matches!(c, ChromaMode::Single420 | ChromaMode::Dual420))
    {
        Some((ERROR_NO_CHROMA, "No chroma mode in common"))
    } else {
        None
    };
    if let Some((code, text)) = error {
        writer.write_msg(&ServerMsg::error(code, text)).await?;
        bail!("{text}");
    }
    let dual = caps.chroma.contains(&ChromaMode::Dual420)
        && (cfg.full_chroma && !cfg.low_bandwidth || !caps.chroma.contains(&ChromaMode::Single420));
    let chroma = if dual {
        ChromaMode::Dual420
    } else {
        ChromaMode::Single420
    };
    let args = vec![
        cfg.output.clone(),
        caps.max_width.clamp(2, 1920).to_string(),
        caps.max_height.clamp(2, 1200).to_string(),
        "30".into(),
    ];
    let mut helper = native::spawn("capture", &args)?;
    let mut native_reader = helper.stdout.take().unwrap();
    let mut native_writer = helper.stdin.take().unwrap();
    let (kind, ready) =
        tokio::time::timeout(Duration::from_secs(15), native::read(&mut native_reader))
            .await
            .context("screen capture startup timed out")??;
    ensure!(
        kind == native::EVENT,
        "native capture did not identify display"
    );
    let ready: Value = serde_json::from_slice(&ready)?;
    if ready["type"] == "error" {
        let m = ready["message"].as_str().unwrap_or("capture failed");
        writer.write_msg(&ServerMsg::error(256, m)).await?;
        bail!("{m}");
    }
    let mut width = ready["width"].as_u64().context("missing display width")? as u32;
    let mut height = ready["height"].as_u64().context("missing display height")? as u32;
    let mut scale = ready["scale_milli"]
        .as_u64()
        .context("missing display scale")? as u32;
    let name = ready["output"]
        .as_str()
        .context("missing display name")?
        .to_string();
    let bitrate = cfg
        .bitrate
        .unwrap_or_else(|| width.saturating_mul(height).saturating_mul(8))
        .max(100_000);
    let cpu = gliff_sw::VideoMode::resolve(cfg.video.as_deref()) == gliff_sw::VideoMode::Cpu;
    let mut encoder = Encoder::new(
        gliff_sw::EncoderSettings {
            width,
            height,
            bitrate,
            framerate: 30,
        },
        cpu,
        dual,
    )
    .await?;
    writer
        .write_msg(&ServerMsg::HelloAck {
            version: PROTOCOL_VERSION,
            session: SessionInfo {
                headless: false,
                output: name.clone(),
            },
            outputs: vec![OutputInfo {
                name,
                width,
                height,
                scale_milli: scale,
            }],
            features: features()
                .into_iter()
                .chain([FEATURE_CURSOR_IN_VIDEO.to_string()])
                .collect(),
        })
        .await?;
    writer
        .write_msg(&ServerMsg::StreamConfig {
            codec: Codec::H264,
            chroma,
            pipeline: Some(encoder.pipeline()),
            width,
            height,
            scale_milli: scale,
            extradata: vec![],
            aux_extradata: None,
            view_width: width,
            view_height: height,
            fps_cap: 30,
        })
        .await?;
    let (command_tx, mut commands) = mpsc::channel::<Value>(64);
    tokio::task::spawn_local(async move {
        while let Some(cmd) = commands.recv().await {
            native::write(
                &mut native_writer,
                native::EVENT,
                &serde_json::to_vec(&cmd)?,
            )
            .await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let (cap_tx, mut capture) = mpsc::channel::<Result<(u8, Vec<u8>)>>(1);
    tokio::task::spawn_local(async move {
        loop {
            let frame = native::read(&mut native_reader).await;
            let stopped = frame.is_err();
            if cap_tx.send(frame).await.is_err() || stopped {
                break;
            }
        }
    });
    let (msg_tx, mut input) = mpsc::channel::<Result<(ClientMsg, Bytes)>>(64);
    tokio::task::spawn_local(async move {
        loop {
            let msg = async {
                let msg = reader.read_msg::<ClientMsg>().await?;
                let p = reader
                    .read_payload(match &msg {
                        ClientMsg::ClipboardData { data_len, .. } => *data_len,
                        _ => 0,
                    })
                    .await?;
                Ok((msg, p))
            }
            .await;
            let stopped = msg.is_err();
            if msg_tx.send(msg).await.is_err() || stopped {
                break;
            }
        }
    });
    let clipboard = crate::clipboard::Bridge::new(
        gliff_transport::clipboard::Side::Server,
        crate::clipboard::native_commands(command_tx.clone()),
    );
    let mut clips = clipboard.take_outbound();
    let mut frame_id = 0_u64;
    let mut ack = 0_u64;
    let mut keyframe = true;
    let started = Instant::now();
    let mut last_frame = None;
    let mut refresh = tokio::time::interval(Duration::from_millis(250));
    let mut resend = false;
    let result: Result<()> = async {
        loop {
            tokio::select! {
                Some((msg, payload)) = clips.recv() => { writer.write_msg_with_payloads(&ServerMsg::from(msg), &[&payload]).await?; }
                _ = refresh.tick(), if last_frame.is_some() => { resend = true; }
                msg = input.recv() => {
                    let Some(msg) = msg else { break; };
                    let (msg, payload) = match msg { Ok(m) => m, Err(e) if e.downcast_ref::<gliff_transport::Error>().is_some_and(|e| matches!(e, gliff_transport::Error::Closed)) => break, Err(e) => return Err(e) };
                    match msg.into_clipboard() {
                        Ok(msg) => clipboard.peer(msg, payload),
                        Err(msg) => match msg {
                            ClientMsg::Bye => break,
                            ClientMsg::FrameAck { frame_id: id, .. } => { ack = ack.max(id.min(frame_id)); }
                            ClientMsg::RequestKeyframe => { keyframe = true; }
                            ClientMsg::Ping { t } => writer.write_msg(&ServerMsg::Pong { t, server_now_ms: crate::client::now_ms() }).await?,
                            other => if let Some(cmd) = input_command(other) { command_tx.send(cmd).await?; },
                        }
                    }
                }
                frame = capture.recv() => {
                    let (kind, frame) = frame.context("capture stopped")??;
                    if kind == native::EVENT { let ev: Value = serde_json::from_slice(&frame)?; if ev["type"] == "error" { bail!("{}", ev["message"]); }
                    if ev["type"] == "clipboard" { clipboard.local(&ev).await?; } else if ev["type"] == "paste" { clipboard.paste(&ev); } continue; }
                    ensure!(kind == native::PIXELS, "unknown capture packet");
                    let (w,h,s) = native::geometry(&frame)?;
                    if (w,h,s) != (width,height,scale) {
                        ensure!(w <= 1920 && h <= 1200 && w.is_multiple_of(2) && h.is_multiple_of(2), "invalid display extent");
                        width = w; height = h; scale = s;
                        encoder = Encoder::new(gliff_sw::EncoderSettings { width, height, bitrate, framerate: 30 }, cpu, dual).await?;
                        writer.write_msg(&ServerMsg::StreamConfig { codec: Codec::H264, chroma, pipeline: Some(encoder.pipeline()), width, height, scale_milli: scale, extradata: vec![], aux_extradata: None, view_width: width, view_height: height, fps_cap: 30 }).await?;
                        keyframe = true;
                    }
                    last_frame = Some(frame); resend = true;
                }
            }
            if !resend || frame_id.saturating_sub(ack) >= 2 { continue; }
            let Some(frame) = &last_frame else { continue; };
            let pipeline = encoder.pipeline();
            let coded = encoder.encode(&frame[12..], keyframe).await?;
            if encoder.pipeline() != pipeline {
                writer.write_msg(&ServerMsg::StreamConfig { codec: Codec::H264, chroma, pipeline: Some(encoder.pipeline()), width, height, scale_milli: scale, extradata: vec![], aux_extradata: None, view_width: width, view_height: height, fps_cap: 30 }).await?;
            }
            if coded.main.is_empty() { continue; }
            keyframe = false; resend = false; frame_id += 1;
            let aux = coded.aux.as_deref().unwrap_or_default();
            let msg = ServerMsg::VideoFrame { frame_id, pts_us: started.elapsed().as_micros() as u64, keyframe: coded.keyframe, damage: vec![], data_len: coded.main.len() as u32, aux_len: aux.len() as u32 };
            writer.write_msg_with_payloads(&msg, &[&coded.main, aux]).await?;
        }
        Ok(())
    }.await;
    let _ = command_tx.send(json!({"type":"release"})).await;
    // EOF lets the bridge release all held input before it exits.
    drop(clipboard);
    drop(command_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), helper.wait()).await;
    let _ = helper.kill().await;
    result
}

fn input_command(msg: ClientMsg) -> Option<Value> {
    Some(match msg {
        ClientMsg::Resize { width, height, .. } => {
            json!({"type":"resize", "width":width.clamp(2, 1920), "height":height.clamp(2, 1200)})
        }
        ClientMsg::Key { keycode, pressed } => {
            json!({"type":"key", "keycode": keys::mac(keycode)?, "pressed":pressed})
        }
        ClientMsg::PointerMotion { x, y } => {
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            json!({"type":"motion", "x":x, "y":y})
        }
        ClientMsg::PointerButton { button, pressed } => {
            json!({"type":"button", "button": match button { 272 => 0, 273 => 1, 274 => 2, _ => return None }, "pressed":pressed})
        }
        ClientMsg::PointerAxis {
            axis, value, stop, ..
        } if !stop && value.is_finite() => {
            json!({"type":"axis", "horizontal":axis == gliff_proto::Axis::Horizontal, "value":value})
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_translation_preserves_positions_and_bounds() {
        assert_eq!(
            input_command(ClientMsg::Key {
                keycode: 30,
                pressed: true
            })
            .unwrap(),
            json!({"type":"key", "keycode":0, "pressed":true})
        );
        assert!(input_command(ClientMsg::Key {
            keycode: u32::MAX,
            pressed: true
        })
        .is_none());
        assert!(input_command(ClientMsg::PointerMotion {
            x: f64::NAN,
            y: 0.0
        })
        .is_none());
        assert_eq!(
            input_command(ClientMsg::Resize {
                width: 800,
                height: 600,
                scale: 2.0
            })
            .unwrap(),
            json!({"type":"resize", "width":800, "height":600})
        );
    }
}
