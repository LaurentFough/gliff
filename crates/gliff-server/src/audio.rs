//! The desktop's sound: what the applications play, recorded by PipeWire's
//! `pw-record` (or PulseAudio's `parec` where it is missing) and coded as
//! Opus. The recorder runs as a child so the server links no audio library;
//! it is started when the client turns sound on and killed when it turns it
//! off or the session ends.

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Coded packets waiting for the session loop. Past this (half a second)
/// the newest are dropped, which the client conceals.
const QUEUE: usize = 50;

pub struct Capture {
    _child: Child,
    _linker: Option<Linker>,
    pub packets: mpsc::Receiver<Vec<u8>>,
}

impl Capture {
    pub fn start() -> Result<Self> {
        let (mut child, linker) = recorder()?;
        let mut stdout = child.stdout.take().context("recorder has no output")?;
        let (tx, packets) = mpsc::channel(QUEUE);
        tokio::task::spawn_local(async move {
            let mut encoder = match gliff_audio::Encoder::new() {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot create the audio encoder");
                    return;
                }
            };
            // 10 ms of f32 stereo per read at most; the encoder regroups it.
            let mut bytes = vec![0u8; gliff_audio::FRAME * gliff_audio::CHANNELS * 4];
            let mut have = 0;
            let mut pcm = Vec::with_capacity(bytes.len() / 4);
            loop {
                match stdout.read(&mut bytes[have..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => have += n,
                }
                let whole = have / 4 * 4;
                pcm.clear();
                pcm.extend(
                    bytes[..whole]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
                bytes.copy_within(whole..have, 0);
                have -= whole;
                let coded = match encoder.push(&pcm) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "audio encode failed");
                        break;
                    }
                };
                for packet in coded {
                    match tx.try_send(packet) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
            tracing::info!("audio capture ended");
        });
        Ok(Self {
            _child: child,
            _linker: linker,
            packets,
        })
    }
}

/// A command that finds the sound server: Tailscale SSH starts commands
/// without `XDG_RUNTIME_DIR`, and every PipeWire and PulseAudio tool looks
/// for its socket there.
fn command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        if let Ok(dir) = hypr_ipc::runtime_dir() {
            cmd.env("XDG_RUNTIME_DIR", dir);
        }
    }
    cmd
}

/// Start recording as raw 48 kHz f32 stereo on stdout, and the linker that
/// feeds the recorder when there is one.
fn recorder() -> Result<(Child, Option<Linker>)> {
    let rate = gliff_audio::SAMPLE_RATE.to_string();
    let channels = gliff_audio::CHANNELS.to_string();
    let name = format!("gliff-audio-{}", std::process::id());
    let linker = Linker::start(&name)
        .map_err(|e| tracing::warn!(error = %e, "no pw-dump; recording the whole output"))
        .ok();
    // With a linker the recorder waits unconnected for the links it makes;
    // without one it records the default output's monitor, which also holds
    // what a local Gliff client plays.
    let props = if linker.is_some() {
        format!("{{ node.name={name} node.description=Gliff node.autoconnect=false }}")
    } else {
        format!("{{ stream.capture.sink=true node.name={name} node.description=Gliff }}")
    };
    let mut pw = command("pw-record");
    pw.args([
        "--rate",
        &rate,
        "--channels",
        &channels,
        "--format",
        "f32",
        "--latency",
        "10ms",
        "-P",
        &props,
        "-",
    ])
    .stdout(std::process::Stdio::piped());
    match Command::from(pw).kill_on_drop(true).spawn() {
        Ok(child) => Ok((child, linker)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut parec = command("parec");
            parec
                .args([
                    "--device=@DEFAULT_MONITOR@",
                    "--raw",
                    "--format=float32le",
                    &format!("--rate={rate}"),
                    &format!("--channels={channels}"),
                    "--latency-msec=10",
                    "--client-name=Gliff",
                ])
                .stdout(std::process::Stdio::piped());
            let child = Command::from(parec)
                .kill_on_drop(true)
                .spawn()
                .context("neither pw-record nor parec is installed")?;
            Ok((child, None))
        }
        Err(e) => Err(e).context("start pw-record"),
    }
}

/// Programs whose playback is never recorded: the Gliff client plays the
/// other machine's sound, and sending that back would echo forever when two
/// machines show each other.
const EXCLUDED: &[&str] = &["gliff"];

/// Links every application's output into the recorder, except Gliff's own,
/// as applications come and go: `pw-dump --monitor` reports the graph and
/// `pw-link` adds what is missing. The links end with the recorder.
struct Linker {
    child: std::process::Child,
}

impl Linker {
    fn start(capture: &str) -> Result<Self> {
        let mut child = command("pw-dump")
            .args(["--monitor", "--no-colors"])
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().context("pw-dump has no output")?;
        let capture = capture.to_string();
        std::thread::Builder::new()
            .name("gliff-audio-links".into())
            .spawn(move || {
                let mut graph = std::collections::HashMap::new();
                let mut tried = std::collections::HashSet::new();
                let batches =
                    serde_json::Deserializer::from_reader(std::io::BufReader::new(stdout))
                        .into_iter::<Vec<serde_json::Value>>();
                for batch in batches {
                    let Ok(batch) = batch else { break };
                    for object in batch {
                        let Some(id) = object["id"].as_u64() else {
                            continue;
                        };
                        if object["info"].is_null() {
                            graph.remove(&id);
                        } else {
                            graph.insert(id, object);
                        }
                    }
                    for (out, input) in missing_links(&graph, &capture) {
                        if tried.insert((out, input)) {
                            let linked = command("pw-link")
                                .args([out.to_string(), input.to_string()])
                                .stdout(std::process::Stdio::null())
                                .status();
                            tracing::debug!(out, input, ?linked, "linked audio into the recorder");
                        }
                    }
                }
            })?;
        Ok(Self { child })
    }
}

impl Drop for Linker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The links from application outputs to the recorder `capture` that the
/// graph (pw-dump objects by id) does not have yet, as (output port, input
/// port) ids.
fn missing_links(
    graph: &std::collections::HashMap<u64, serde_json::Value>,
    capture: &str,
) -> Vec<(u64, u64)> {
    let props = |o: &serde_json::Value| o["info"]["props"].clone();
    let kind = |o: &serde_json::Value, t: &str| o["type"].as_str().is_some_and(|k| k.ends_with(t));
    let excluded = |p: &serde_json::Value| {
        p["application.process.binary"]
            .as_str()
            .is_some_and(|b| EXCLUDED.contains(&b))
    };
    let mut capture_node = None;
    let mut sources = std::collections::HashSet::new();
    for (&id, o) in graph {
        if !kind(o, "Node") {
            continue;
        }
        let p = props(o);
        if p["node.name"].as_str() == Some(capture) {
            capture_node = Some(id);
        } else if p["media.class"].as_str() == Some("Stream/Output/Audio") {
            let client = p["client.id"].as_u64().and_then(|c| graph.get(&c));
            if !excluded(&p) && !client.is_some_and(|c| excluded(&props(c))) {
                sources.insert(id);
            }
        }
    }
    let Some(capture_node) = capture_node else {
        return Vec::new();
    };
    let mut inputs = std::collections::HashMap::new();
    let mut outputs = Vec::new();
    for (&id, o) in graph {
        if !kind(o, "Port") {
            continue;
        }
        let p = props(o);
        let (Some(node), Some(channel)) = (p["node.id"].as_u64(), p["audio.channel"].as_str())
        else {
            continue;
        };
        let direction = o["info"]["direction"].as_str();
        if node == capture_node && direction == Some("input") {
            inputs.insert(channel.to_string(), id);
        } else if sources.contains(&node) && direction == Some("output") {
            outputs.push((id, channel.to_string()));
        }
    }
    let existing: std::collections::HashSet<(u64, u64)> = graph
        .values()
        .filter(|o| kind(o, "Link"))
        .filter_map(|o| {
            Some((
                o["info"]["output-port-id"].as_u64()?,
                o["info"]["input-port-id"].as_u64()?,
            ))
        })
        .collect();
    let mut wanted = Vec::new();
    for (out, channel) in outputs {
        // Left and right go to their side; a centre or mono channel to
        // both; the bass channel nowhere.
        let sides: &[&str] = match channel.as_str() {
            "FL" | "RL" | "SL" => &["FL"],
            "FR" | "RR" | "SR" => &["FR"],
            "LFE" => &[],
            _ => &["FL", "FR"],
        };
        for side in sides {
            if let Some(&input) = inputs.get(*side) {
                if !existing.contains(&(out, input)) {
                    wanted.push((out, input));
                }
            }
        }
    }
    wanted.sort_unstable();
    wanted
}

/// The session's sound: on while the client wants it and a recorder runs.
#[derive(Default)]
pub struct Sound {
    capture: Option<Capture>,
    seq: u32,
}

impl Sound {
    /// Turn sound on or off at the client's request. Starting announces the
    /// stream, so the client resets its decoder.
    pub fn set(&mut self, enabled: bool, writer: &crate::writer::Writer) {
        if !enabled {
            if self.capture.take().is_some() {
                tracing::info!("audio off");
            }
            return;
        }
        if self.capture.is_some() {
            return;
        }
        match Capture::start() {
            Ok(capture) => {
                tracing::info!("audio on");
                self.capture = Some(capture);
                self.seq = 0;
                writer.send(
                    gliff_proto::ServerMsg::AudioStart {
                        codec: gliff_proto::AudioCodec::Opus,
                        sample_rate: gliff_audio::SAMPLE_RATE,
                        channels: gliff_audio::CHANNELS as u32,
                    },
                    Vec::new(),
                );
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "no audio"),
        }
    }

    pub fn is_on(&self) -> bool {
        self.capture.is_some()
    }

    /// The next coded packet; `None` when the recorder stopped.
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        self.capture.as_mut()?.packets.recv().await
    }

    pub fn send(&mut self, packet: Option<Vec<u8>>, writer: &crate::writer::Writer) {
        let Some(packet) = packet else {
            self.capture = None;
            return;
        };
        writer.send(
            gliff_proto::ServerMsg::AudioFrame {
                seq: self.seq,
                data_len: packet.len() as u32,
            },
            vec![packet],
        );
        self.seq = self.seq.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: u64, props: serde_json::Value) -> (u64, serde_json::Value) {
        (
            id,
            json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"props": props}}),
        )
    }

    fn port(id: u64, node: u64, direction: &str, channel: &str) -> (u64, serde_json::Value) {
        (
            id,
            json!({"id": id, "type": "PipeWire:Interface:Port",
                   "info": {"direction": direction, "props": {"node.id": node, "audio.channel": channel}}}),
        )
    }

    #[test]
    fn links_every_application_but_gliff_into_the_recorder() {
        let graph: std::collections::HashMap<u64, serde_json::Value> = [
            node(10, json!({"node.name": "gliff-audio-1", "media.class": "Stream/Input/Audio"})),
            port(11, 10, "input", "FL"),
            port(12, 10, "input", "FR"),
            // A browser in stereo, already half linked.
            node(20, json!({"node.name": "Firefox", "media.class": "Stream/Output/Audio"})),
            port(21, 20, "output", "FL"),
            port(22, 20, "output", "FR"),
            (30, json!({"id": 30, "type": "PipeWire:Interface:Link",
                        "info": {"output-port-id": 21, "input-port-id": 11}})),
            // A mono application goes to both sides.
            node(40, json!({"node.name": "beep", "media.class": "Stream/Output/Audio"})),
            port(41, 40, "output", "MONO"),
            // The local Gliff client, named on its node or on its client.
            node(50, json!({"node.name": "alsa_playback.gliff", "media.class": "Stream/Output/Audio",
                            "application.process.binary": "gliff"})),
            port(51, 50, "output", "FL"),
            (60, json!({"id": 60, "type": "PipeWire:Interface:Client",
                        "info": {"props": {"application.process.binary": "gliff"}}})),
            node(61, json!({"node.name": "other", "media.class": "Stream/Output/Audio", "client.id": 60})),
            port(62, 61, "output", "FR"),
            // A sink's monitor is not an application.
            node(70, json!({"node.name": "speakers", "media.class": "Audio/Sink"})),
            port(71, 70, "output", "FL"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            missing_links(&graph, "gliff-audio-1"),
            vec![(22, 12), (41, 11), (41, 12)]
        );
        assert!(missing_links(&graph, "gliff-audio-2").is_empty());
    }
}
