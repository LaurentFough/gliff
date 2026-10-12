//! Opt-in SSH input/text-paste check against a dedicated foreground fixture.
//! Never use this on a desktop with an unrelated application in focus.
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use gliff_proto::{
    clipboard::TEXT_MIME, Axis, ChromaMode, ClientCaps, ClientMsg, ClipboardItem, Codec, ServerMsg,
    PROTOCOL_VERSION,
};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    ssh: String,
    #[arg(long)]
    input_script: PathBuf,
    #[arg(long)]
    expect_text: String,
    #[arg(long)]
    offer_text: String,
}

#[derive(Deserialize)]
struct Action {
    after_ms: u64,
    #[serde(flatten)]
    input: Input,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Input {
    Key { keycode: u32, pressed: bool },
    Motion { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
    Axis { horizontal: bool, value: f64 },
}
impl Input {
    fn message(&self) -> ClientMsg {
        match *self {
            Self::Key { keycode, pressed } => ClientMsg::Key { keycode, pressed },
            Self::Motion { x, y } => ClientMsg::PointerMotion { x, y },
            Self::Button { button, pressed } => ClientMsg::PointerButton { button, pressed },
            Self::Axis { horizontal, value } => ClientMsg::PointerAxis {
                axis: if horizontal {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                },
                value,
                discrete: None,
                stop: false,
            },
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let actions: Vec<Action> = serde_json::from_slice(&std::fs::read(&cli.input_script)?)?;
    ensure!(
        !actions.is_empty() && actions.len() <= 100,
        "expected 1..100 input actions"
    );
    ensure!(
        actions.windows(2).all(|a| a[0].after_ms <= a[1].after_ms)
            && actions.last().unwrap().after_ms < 9000,
        "input actions must be ordered and bounded"
    );
    ensure!(
        cli.offer_text.len() < 65536,
        "fixture text exceeds single-chunk size"
    );
    let mut ssh = spawn_ssh(&SshTarget::new(cli.ssh))?;
    let mut reader = Framed::new(ssh.stdout);
    let mut writer = Framed::new(ssh.stdin);
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: String::new(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 800,
                max_height: 600,
                chroma: vec![ChromaMode::Single420],
                features: vec![],
            },
        })
        .await?;
    let mut next = 0;
    let mut offered = false;
    let mut received = false;
    let mut served = false;
    let mut frames = 0;
    let mut text = Vec::new();
    let started = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    while next < actions.len() && elapsed >= actions[next].after_ms {
                        writer.write_msg(&actions[next].input.message()).await?;
                        next += 1;
                    }
                    if elapsed >= 3000 && !offered {
                        writer.write_msg(&ClientMsg::ClipboardOffer { serial: 100, mime_types: vec![TEXT_MIME.into()], files: vec![] }).await?;
                        offered = true;
                    }
                    if elapsed >= 10000 { break; }
                }
                packet = reader.read_msg::<ServerMsg>() => {
                    match packet? {
                        ServerMsg::VideoFrame { frame_id, data_len, aux_len, .. } => {
                            reader.read_payload(data_len.checked_add(aux_len).context("frame length overflow")?).await?;
                            writer.write_msg(&ClientMsg::FrameAck { frame_id, decoded_at_ms: 0 }).await?;
                            frames += 1;
                        }
                        ServerMsg::ClipboardOffer { serial, mime_types, .. } if !received && mime_types.iter().any(|m| m == TEXT_MIME) => {
                            text.clear();
                            writer.write_msg(&ClientMsg::ClipboardRequest { id: 1, serial, item: ClipboardItem::Mime(TEXT_MIME.into()) }).await?;
                        }
                        ServerMsg::ClipboardData { id, data_len, done, .. } => {
                            text.extend_from_slice(&reader.read_payload(data_len).await?);
                            ensure!(text.len() <= 1024 * 1024, "fixture clipboard exceeds limit");
                            if done { ensure!(text == cli.expect_text.as_bytes(), "native clipboard text differs from fixture"); received = true; }
                            else { writer.write_msg(&ClientMsg::ClipboardAck { id, received: text.len() as u64 }).await?; }
                        }
                        ServerMsg::ClipboardRequest { id, serial: 100, item: ClipboardItem::Mime(ref m) } if m == TEXT_MIME => {
                            writer.write_msg_with_payloads(&ClientMsg::ClipboardData { id, offset: 0, data_len: cli.offer_text.len() as u32, done: true }, &[cli.offer_text.as_bytes()]).await?;
                            served = true;
                        }
                        ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
                        ServerMsg::Error { message, .. } => bail!("host failed: {message}"),
                        ServerMsg::CursorShape { argb_len, .. } => { reader.read_payload(argb_len).await?; }
                        _ => {}
                    }
                }
            }
        }
        writer.write_msg(&ClientMsg::Bye).await?;
        ensure!(frames > 0 && received && served && next == actions.len(), "control check incomplete: frames={frames}, native clipboard={received}, remote paste={served}, actions={next}");
        println!("CONTROL TRANSPORT PASS: {frames} frames, native clipboard received and remote text paste served");
        Ok(())
    }.await;
    drop(reader);
    drop(writer);
    // Give the host time to release held keys before closing the SSH child.
    let _ = tokio::time::timeout(Duration::from_secs(3), ssh.child.wait()).await;
    let _ = ssh.child.kill().await;
    result
}
