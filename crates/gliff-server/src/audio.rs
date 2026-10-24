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
    pub packets: mpsc::Receiver<(u32, Vec<u8>)>,
    pulse: bool,
    started: std::time::Instant,
}

impl Capture {
    pub fn start(prefer_pulse: bool) -> Result<Self> {
        let (mut child, linker, pulse) = recorder(prefer_pulse)?;
        let mut graph_failure = linker.as_ref().map(|linker| linker.failed.clone());
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
            let mut seq = 0;
            loop {
                let read = tokio::select! {
                    read = stdout.read(&mut bytes[have..]) => read,
                    _ = graph_failed(&mut graph_failure) => {
                        tracing::warn!("audio graph monitor ended; restarting capture");
                        break;
                    }
                };
                match read {
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
                    if !offer(&tx, &mut seq, packet) {
                        return;
                    }
                }
            }
            tracing::info!("audio capture ended");
        });
        Ok(Self {
            _child: child,
            _linker: linker,
            packets,
            pulse,
            started: std::time::Instant::now(),
        })
    }
}

async fn graph_failed(monitor: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    match monitor {
        Some(monitor) => {
            if !*monitor.borrow() {
                let _ = monitor.changed().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Number a packet and queue it. A packet dropped on a full queue keeps its
/// number, so the client sees the gap. False once the session stopped
/// listening.
fn offer(tx: &mpsc::Sender<(u32, Vec<u8>)>, seq: &mut u32, packet: Vec<u8>) -> bool {
    let numbered = (*seq, packet);
    *seq = seq.wrapping_add(1);
    !matches!(
        tx.try_send(numbered),
        Err(mpsc::error::TrySendError::Closed(_))
    )
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
fn recorder(prefer_pulse: bool) -> Result<(Child, Option<Linker>, bool)> {
    let rate = gliff_audio::SAMPLE_RATE.to_string();
    let channels = gliff_audio::CHANNELS.to_string();
    if prefer_pulse {
        return Ok((pulse_recorder(&rate, &channels)?, None, true));
    }
    static CAPTURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let capture = CAPTURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("gliff-audio-{}-{capture}", std::process::id());
    let linker = Linker::start(&name)
        .map_err(|e| tracing::warn!(error = %e, "cannot monitor the audio graph; recording the whole output"))
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
        // Without it, pw-record writes an AU file to stdout: a header, and
        // big-endian samples on PipeWire 1.6.10, not raw little-endian ones.
        "--raw",
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
        Ok(child) => Ok((child, linker, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((pulse_recorder(&rate, &channels)?, None, true))
        }
        Err(e) => Err(e).context("start pw-record"),
    }
}

fn pulse_recorder(rate: &str, channels: &str) -> Result<Child> {
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
    Command::from(parec)
        .kill_on_drop(true)
        .spawn()
        .context("start parec")
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
    failed: tokio::sync::watch::Receiver<bool>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Linker {
    fn start(capture: &str) -> Result<Self> {
        let mut child = command("pw-dump")
            .args(["--monitor", "--no-colors"])
            .stderr(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().context("pw-dump has no output")?;
        let capture = capture.to_string();
        let (failure, failed) = tokio::sync::watch::channel(false);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let linker = Self {
            child,
            failed,
            cancelled: cancelled.clone(),
        };
        let (updates, batches) = std::sync::mpsc::sync_channel(4);
        std::thread::Builder::new()
            .name("gliff-audio-graph".into())
            .spawn(move || {
                let objects =
                    serde_json::Deserializer::from_reader(std::io::BufReader::new(stdout))
                        .into_iter::<Vec<serde_json::Value>>();
                for batch in objects {
                    let batch = match batch {
                        Ok(batch) => batch,
                        Err(e) => {
                            tracing::warn!(error = %e, "audio graph monitor returned invalid data");
                            break;
                        }
                    };
                    if updates.send(batch).is_err() {
                        break;
                    }
                }
                let _ = failure.send(true);
            })?;
        std::thread::Builder::new()
            .name("gliff-audio-links".into())
            .spawn(move || {
                let mut links = LinkState::default();
                loop {
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    match batches.recv_timeout(LINK_RETRY) {
                        Ok(batch) => links.update(batch),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    for (out, input) in links.pending(&capture, std::time::Instant::now()) {
                        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        let linked = command("pw-link")
                            .args([out.to_string(), input.to_string()])
                            .stdout(std::process::Stdio::null())
                            .spawn()
                            .and_then(|child| wait_link(child, &cancelled));
                        tracing::debug!(out, input, ?linked, "linked audio into the recorder");
                    }
                }
            })?;
        Ok(linker)
    }
}

impl Drop for Linker {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A broken graph server must not leave a pw-link process and both helper
/// threads behind forever when sound is stopped.
fn wait_link(
    mut child: std::process::Child,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        if cancelled.load(std::sync::atomic::Ordering::Acquire)
            || std::time::Instant::now() >= deadline
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

const LINK_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Default)]
struct LinkState {
    graph: std::collections::HashMap<u64, serde_json::Value>,
    attempted: std::collections::HashMap<(u64, u64), std::time::Instant>,
}

impl LinkState {
    fn update(&mut self, batch: Vec<serde_json::Value>) {
        for object in batch {
            let Some(id) = numeric(&object["id"]) else {
                continue;
            };
            let prior = self
                .graph
                .get(&id)
                .and_then(|o| numeric(&o["info"]["props"]["object.serial"]));
            let next = numeric(&object["info"]["props"]["object.serial"]);
            let removed = object.get("info").is_some_and(serde_json::Value::is_null);
            let reused = prior.is_some() && next.is_some() && prior != next;
            if removed || reused {
                self.attempted
                    .retain(|&(out, input), _| out != id && input != id);
            }
            if removed {
                self.graph.remove(&id);
            } else if reused {
                self.graph.insert(id, object);
            } else {
                merge_update(self.graph.entry(id).or_default(), object);
            }
        }
    }

    fn pending(&mut self, capture: &str, now: std::time::Instant) -> Vec<(u64, u64)> {
        let wanted = missing_links(&self.graph, capture);
        self.attempted.retain(|key, _| wanted.contains(key));
        wanted
            .into_iter()
            .filter(|key| {
                let retry = self
                    .attempted
                    .get(key)
                    .is_none_or(|at| now.duration_since(*at) >= LINK_RETRY);
                if retry {
                    self.attempted.insert(*key, now);
                }
                retry
            })
            .collect()
    }
}

fn numeric(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn merge_update(current: &mut serde_json::Value, update: serde_json::Value) {
    match (current, update) {
        (serde_json::Value::Object(current), serde_json::Value::Object(update)) => {
            for (key, value) in update {
                merge_update(current.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (current, update) => *current = update,
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
            let client = numeric(&p["client.id"]).and_then(|c| graph.get(&c));
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
        let (Some(node), Some(channel)) = (numeric(&p["node.id"]), p["audio.channel"].as_str())
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
                numeric(&o["info"]["output-port-id"])?,
                numeric(&o["info"]["input-port-id"])?,
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

/// The session's requested sound, including bounded recovery when the
/// recorder or graph monitor goes away.
#[derive(Default)]
pub struct Sound {
    capture: Option<Capture>,
    wanted: bool,
    retry_at: Option<tokio::time::Instant>,
    retry_delay: std::time::Duration,
    prefer_pulse: bool,
}

impl Sound {
    pub fn set(&mut self, enabled: bool, writer: &crate::writer::Writer) {
        if !enabled {
            self.wanted = false;
            self.capture = None;
            self.retry_at = None;
            self.retry_delay = std::time::Duration::from_secs(1);
            self.prefer_pulse = false;
        } else if !self.wanted {
            self.wanted = true;
            self.start(writer);
        }
    }

    fn start(&mut self, writer: &crate::writer::Writer) {
        self.retry_at = None;
        match Capture::start(self.prefer_pulse) {
            Ok(capture) => {
                tracing::info!(pulse = capture.pulse, "audio on");
                self.capture = Some(capture);
                writer.send(
                    gliff_proto::ServerMsg::AudioStart {
                        codec: gliff_proto::AudioCodec::Opus,
                        sample_rate: gliff_audio::SAMPLE_RATE,
                        channels: gliff_audio::CHANNELS as u32,
                    },
                    Vec::new(),
                );
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture unavailable; retrying");
                self.prefer_pulse = !self.prefer_pulse;
                self.schedule_retry();
            }
        }
    }

    fn schedule_retry(&mut self) {
        let delay = self.retry_delay.max(std::time::Duration::from_secs(1));
        self.retry_at = Some(tokio::time::Instant::now() + delay);
        self.retry_delay = (delay * 2).min(std::time::Duration::from_secs(30));
    }

    fn capture_stopped(&mut self, started: std::time::Instant, pulse: bool) {
        let healthy = started.elapsed() >= std::time::Duration::from_secs(2);
        if healthy {
            // A quiet but long-lived recorder is healthy even when it has
            // emitted no packets since the previous recovery.
            self.retry_delay = std::time::Duration::from_secs(1);
        }
        self.prefer_pulse = !pulse && !healthy;
        self.schedule_retry();
    }

    pub fn is_on(&self) -> bool {
        self.wanted
    }

    pub async fn next(&mut self) -> Option<(u32, Vec<u8>)> {
        if let Some(capture) = &mut self.capture {
            capture.packets.recv().await
        } else if let Some(at) = self.retry_at {
            tokio::time::sleep_until(at).await;
            None
        } else {
            std::future::pending().await
        }
    }

    pub fn send(&mut self, packet: Option<(u32, Vec<u8>)>, writer: &crate::writer::Writer) {
        let Some((seq, packet)) = packet else {
            if let Some(capture) = self.capture.take() {
                // An immediate failure can mean an installed pw-record
                // without a running PipeWire daemon (or without --raw).
                // Try the explicit monitor fallback; healthy streams that
                // later fail instead retry their original PipeWire path.
                self.capture_stopped(capture.started, capture.pulse);
            } else if self.wanted {
                self.start(writer);
            }
            return;
        };
        self.retry_delay = std::time::Duration::from_secs(1);
        writer.send(
            gliff_proto::ServerMsg::AudioFrame {
                seq,
                data_len: packet.len() as u32,
            },
            vec![packet],
        );
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
    fn reused_ports_are_linked_again_and_failed_links_retry() {
        let objects = vec![
            node(10, json!({"node.name": "capture"})).1,
            port(11, 10, "input", "FL").1,
            node(20, json!({"media.class": "Stream/Output/Audio"})).1,
            port(21, 20, "output", "FL").1,
        ];
        let now = std::time::Instant::now();
        let mut state = LinkState::default();
        state.update(objects.clone());
        assert_eq!(state.pending("capture", now), [(21, 11)]);
        assert!(state.pending("capture", now).is_empty());
        // Even without graph updates, a failed or disappeared link is retried.
        assert_eq!(state.pending("capture", now + LINK_RETRY), [(21, 11)]);
        state.update(vec![json!({"id": 21, "info": null})]);
        state.update(objects);
        assert_eq!(state.pending("capture", now + LINK_RETRY), [(21, 11)]);
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

    #[test]
    fn a_packet_dropped_on_a_full_queue_leaves_a_gap() {
        let (tx, mut rx) = mpsc::channel(QUEUE);
        let mut seq = 0;
        for _ in 0..=QUEUE {
            assert!(offer(&tx, &mut seq, vec![0]));
        }
        for expected in 0..QUEUE as u32 {
            assert_eq!(rx.try_recv().unwrap().0, expected);
        }
        assert!(offer(&tx, &mut seq, vec![0]));
        assert_eq!(rx.try_recv().unwrap().0, QUEUE as u32 + 1);
        drop(rx);
        assert!(!offer(&tx, &mut seq, vec![0]));
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_graph_updates_keep_identity_and_accept_numeric_strings() {
        let mut state = LinkState::default();
        state.update(vec![
            json!({"id": "10", "type":"PipeWire:Interface:Node", "info":{"props":{"node.name":"capture", "object.serial":"100"}}}),
            json!({"id":11,"type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":"10","audio.channel":"FL"}}}),
            json!({"id":20,"type":"PipeWire:Interface:Node","info":{"props":{"media.class":"Stream/Output/Audio", "client.id":"30"}}}),
            json!({"id":21,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":"20","audio.channel":"FL"}}}),
            json!({"id":30,"type":"PipeWire:Interface:Client","info":{"props":{"application.process.binary":"Firefox"}}}),
        ]);
        state.update(vec![json!({"id":10,"info":{"state":"running"}})]);
        assert_eq!(
            state.pending("capture", std::time::Instant::now()),
            [(21, 11)]
        );
        // An updated client property must preserve its type and keep
        // Gliff excluded even when the referring client ID is quoted.
        state.update(vec![
            json!({"id":30,"info":{"props":{"application.process.binary":"gliff"}}}),
        ]);
        assert!(state
            .pending("capture", std::time::Instant::now())
            .is_empty());
        // A reused ID is a new global, not an update of the old capture.
        state.update(vec![json!({"id":10,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"different", "object.serial":"200"}}})]);
        assert_eq!(state.graph[&10]["info"]["props"]["node.name"], "different");
        assert!(state.graph[&10]["info"].get("state").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn capture_retries_wait_and_back_off_to_thirty_seconds() {
        let mut sound = Sound {
            wanted: true,
            ..Default::default()
        };
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            sound.schedule_retry();
            let before = tokio::time::Instant::now();
            assert!(tokio::time::timeout(
                std::time::Duration::from_secs(seconds - 1),
                sound.next()
            )
            .await
            .is_err());
            assert!(sound.next().await.is_none());
            assert_eq!(
                tokio::time::Instant::now() - before,
                std::time::Duration::from_secs(seconds)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn muting_cancels_scheduled_capture_retries() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_reader, writer) = tokio::io::duplex(1024);
                let (writer, _reports) =
                    crate::writer::Writer::spawn(gliff_transport::Framed::new(writer));
                let mut sound = Sound {
                    wanted: true,
                    ..Default::default()
                };
                sound.schedule_retry();
                sound.set(false, &writer);
                assert!(!sound.is_on());
                assert!(sound.retry_at.is_none());
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(60), sound.next())
                        .await
                        .is_err()
                );
                assert!(sound.capture.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn failed_graph_monitors_wake_capture_even_without_pcm() {
        let (failed, receiver) = tokio::sync::watch::channel(false);
        let mut monitor = Some(receiver);
        failed.send(true).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            graph_failed(&mut monitor),
        )
        .await
        .unwrap();
        let (failed, receiver) = tokio::sync::watch::channel(false);
        let mut monitor = Some(receiver);
        drop(failed);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            graph_failed(&mut monitor),
        )
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod isolated_pipewire_test {
    use super::*;

    #[test]
    #[ignore = "needs a private PipeWire daemon and GLIFF_AUDIO_TEST_PCM"]
    fn raw_capture_relinks_and_recovers_from_graph_monitor_failure() {
        assert_eq!(std::env::var("GLIFF_ISOLATED_PIPEWIRE").as_deref(), Ok("1"));
        let file = std::env::var("GLIFF_AUDIO_TEST_PCM").unwrap();
        let play = || {
            command("pw-cat").args([
                "--playback", "--raw", "--rate", "48000", "--channels", "2", "--format", "f32",
                "--target", "0", "--latency", "10ms", "-P",
                "{ node.name=gliff-review-tone node.always-process=true node.group=pipewire.dummy }", &file,
            ]).spawn().unwrap()
        };
        struct Playback(std::process::Child);
        impl Drop for Playback {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, async {
            let (_reader, writer) = tokio::io::duplex(128 * 1024);
            let (writer, _reports) =
                crate::writer::Writer::spawn(gliff_transport::Framed::new(writer));
            let mut sound = Sound::default();
            let mut playback = Playback(play());
            sound.set(true, &writer);
            let mut retired_recorder = None;
            for stage in 0..3 {
                let mut decoder = gliff_audio::Decoder::new().unwrap();
                let mut energy = 0.0f64;
                let mut samples = 0usize;
                tokio::time::timeout(std::time::Duration::from_secs(12), async {
                    let mut received = 0;
                    while received < 40 {
                        let packet = sound.next().await;
                        if let Some((seq, bytes)) = &packet {
                            let pcm = decoder.decode(*seq, bytes).unwrap();
                            energy += pcm.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                            samples += pcm.len();
                            received += 1;
                        }
                        sound.send(packet, &writer);
                    }
                })
                .await
                .expect("audio did not recover");
                let mean = energy / samples as f64;
                assert!(
                    (0.005..0.05).contains(&mean),
                    "stage {stage}: tone energy {mean}"
                );
                if stage == 0 {
                    drop(playback);
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    if let Some(capture) = &mut sound.capture {
                        while capture.packets.try_recv().is_ok() {}
                    }
                    playback = Playback(play());
                } else if stage == 1 {
                    retired_recorder = sound.capture.as_ref().unwrap()._child.id();
                    sound
                        .capture
                        .as_mut()
                        .unwrap()
                        ._linker
                        .as_mut()
                        .unwrap()
                        .child
                        .kill()
                        .unwrap();
                }
            }
            assert_ne!(
                sound.capture.as_ref().unwrap()._child.id(),
                retired_recorder
            );
            sound.set(false, &writer);
            assert!(!sound.is_on());
            assert!(sound.capture.is_none());
            assert!(sound.retry_at.is_none());
            drop(playback);
        });
    }
}

#[cfg(test)]
mod captured_graph_test {
    use super::*;

    #[test]
    fn captured_pipewire_updates_keep_application_ports_linkable() {
        // A projection of a private PipeWire 1.6.10 trace: only the test
        // clients, nodes and ports, with unrelated properties removed.
        let batches: Vec<Vec<serde_json::Value>> =
            serde_json::from_str(include_str!("../tests/fixtures/pipewire-monitor.json")).unwrap();
        let mut state = LinkState::default();
        let mut saw_missing_link = false;
        for batch in batches {
            state.update(batch);
            for object in state.graph.values() {
                let Some(name) = object["info"]["props"]["node.name"].as_str() else {
                    continue;
                };
                if name.starts_with("gliff-audio-") && !missing_links(&state.graph, name).is_empty()
                {
                    saw_missing_link = true;
                }
            }
        }
        assert!(
            saw_missing_link,
            "the real stream's ports must be recognized before linking"
        );
    }
}

#[cfg(test)]
mod pulse_fallback_test {
    use super::*;

    #[test]
    #[ignore = "needs the mocked recorder setup in scripts/audio-e2e.sh"]
    fn an_installed_but_failing_pw_record_recovers_with_parec() {
        assert_eq!(
            std::env::var("GLIFF_AUDIO_MOCK_BACKEND").as_deref(),
            Ok("1")
        );
        assert_eq!(std::env::var("GLIFF_ISOLATED_PIPEWIRE").as_deref(), Ok("1"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, async {
            let (_reader, writer) = tokio::io::duplex(64 * 1024);
            let (writer, _reports) =
                crate::writer::Writer::spawn(gliff_transport::Framed::new(writer));
            let mut sound = Sound::default();
            let mut decoder = gliff_audio::Decoder::new().unwrap();
            sound.set(true, &writer);
            let mut energy = 0.0f64;
            let mut samples = 0;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut received = 0;
                while received < 10 {
                    let packet = sound.next().await;
                    if let Some((seq, bytes)) = &packet {
                        assert!(sound.capture.as_ref().unwrap().pulse);
                        let pcm = decoder.decode(*seq, bytes).unwrap();
                        energy += pcm
                            .iter()
                            .map(|sample| (*sample as f64).powi(2))
                            .sum::<f64>();
                        samples += pcm.len();
                        received += 1;
                    }
                    sound.send(packet, &writer);
                }
            })
            .await
            .expect("PulseAudio monitor fallback did not recover");
            assert!((0.005..0.05).contains(&(energy / samples as f64)));
            sound.set(false, &writer);
        });
    }
}

#[cfg(test)]
mod healthy_capture_test {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_healthy_quiet_capture_resets_previous_failure_backoff() {
        let mut sound = Sound {
            wanted: true,
            retry_delay: std::time::Duration::from_secs(30),
            ..Default::default()
        };
        let before = tokio::time::Instant::now();
        sound.capture_stopped(
            std::time::Instant::now() - std::time::Duration::from_secs(3),
            false,
        );
        assert_eq!(
            sound.retry_at,
            Some(before + std::time::Duration::from_secs(1))
        );
        assert!(!sound.prefer_pulse);
    }
}
