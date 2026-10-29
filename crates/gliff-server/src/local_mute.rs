//! `--mute-local-audio`: silence this host's own speakers while a client
//! plays its sound, and put them back afterwards.
//!
//! Only safe when the recorder takes each application's stream directly
//! (PipeWire with the graph linker, see `audio.rs`): those links run in
//! parallel with the applications' links to the speakers, so muting the
//! default output silences the room and not the client. A recorder that
//! takes the output's monitor (the `parec` or `stream.capture.sink`
//! fallbacks) could record the mute, so the session never asks for one then.
//!
//! The work runs on a helper thread so `wpctl` never stalls the video loop:
//!
//! - **engage**: remember the default output's node and whether it was
//!   already muted, then mute it; every two seconds, follow a new default
//!   (headphones plugged in) by restoring the old one and muting the new.
//! - **release**: restore the remembered output (unmute only what this
//!   server muted), on sound off, a fallback recorder, or session end.
//! - **watchdog**: while muted, a detached `sh` waits for this server's PID
//!   and unmutes if the server dies without releasing (`kill -9`, a panic
//!   that aborts). Only a power cut leaves the speakers muted.
//!
//! Version: 0.1.0

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often an engaged mute checks for a new default output.
const FOLLOW: Duration = Duration::from_secs(2);
/// Longest a single `wpctl` call may take before it is abandoned.
const WPCTL_TIMEOUT: Duration = Duration::from_secs(2);

/// What the helper thread is told.
enum Cmd {
    Engage,
    Release,
}

/// The session's handle: dropping it restores the speakers and waits for
/// the helper thread (bounded by the `wpctl` timeouts).
pub struct LocalMute {
    tx: Option<mpsc::Sender<Cmd>>,
    thread: Option<std::thread::JoinHandle<()>>,
    engaged: bool,
}

impl LocalMute {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("gliff-local-mute".into())
            .spawn(move || helper(rx, &mut Wpctl::default()))
            .map_err(|e| tracing::warn!(error = %e, "cannot start the local mute helper"))
            .ok();
        Self {
            tx: thread.is_some().then_some(tx),
            thread,
            engaged: false,
        }
    }

    /// Mute the local speakers (idempotent).
    pub fn engage(&mut self) {
        if !self.engaged {
            self.engaged = true;
            self.send(Cmd::Engage);
        }
    }

    /// Put the local speakers back (idempotent).
    pub fn release(&mut self) {
        if self.engaged {
            self.engaged = false;
            self.send(Cmd::Release);
        }
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for LocalMute {
    fn drop(&mut self) {
        //= Closing the channel tells the helper to restore and stop.
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The sound server operations the helper needs; `Wpctl` in production, a
/// fake in tests.
trait Backend {
    /// The default output's node id.
    fn default_sink(&mut self) -> Option<u32>;
    /// Whether node `id` is muted.
    fn muted(&mut self, id: u32) -> Option<bool>;
    /// Mute or unmute node `id`; false when it failed.
    fn set_mute(&mut self, id: u32, muted: bool) -> bool;
    /// Start the crash watchdog for node `id`.
    fn guard(&mut self, id: u32);
    /// Stop the crash watchdog (a normal restore follows).
    fn unguard(&mut self);
}

/// An output this server muted, and how it was before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Held {
    id: u32,
    was_muted: bool,
}

/// The helper's state: `engaged` while a client plays sound; `held` the
/// output currently muted on its behalf.
#[derive(Debug, Default)]
struct State {
    engaged: bool,
    held: Option<Held>,
}

impl State {
    fn engage(&mut self, b: &mut impl Backend) {
        self.engaged = true;
        self.follow(b);
    }

    fn release(&mut self, b: &mut impl Backend) {
        self.engaged = false;
        self.restore(b);
    }

    /// Keep the current default output muted, moving the mute when the
    /// default changes.
    fn follow(&mut self, b: &mut impl Backend) {
        if !self.engaged {
            return;
        }
        let Some(id) = b.default_sink() else {
            return;
        };
        if self.held.is_some_and(|h| h.id == id) {
            return;
        }
        self.restore(b);
        let Some(was_muted) = b.muted(id) else {
            tracing::warn!(id, "cannot read the local output's mute state; leaving it");
            return;
        };
        if !was_muted {
            if !b.set_mute(id, true) {
                tracing::warn!(id, "cannot mute the local output");
                return;
            }
            b.guard(id);
        }
        tracing::info!(
            id,
            was_muted,
            "local speakers muted while the client plays sound"
        );
        self.held = Some(Held { id, was_muted });
    }

    /// Unmute what this server muted; an output that was muted before stays so.
    fn restore(&mut self, b: &mut impl Backend) {
        let Some(held) = self.held.take() else {
            return;
        };
        if !held.was_muted {
            b.unguard();
            if b.set_mute(held.id, false) {
                tracing::info!(id = held.id, "local speakers restored");
            } else {
                tracing::warn!(
                    id = held.id,
                    "cannot unmute the local output; run: wpctl set-mute @DEFAULT_AUDIO_SINK@ 0"
                );
            }
        }
    }
}

fn helper(rx: mpsc::Receiver<Cmd>, backend: &mut impl Backend) {
    let mut state = State::default();
    loop {
        match rx.recv_timeout(FOLLOW) {
            Ok(Cmd::Engage) => state.engage(backend),
            Ok(Cmd::Release) => state.release(backend),
            Err(mpsc::RecvTimeoutError::Timeout) => state.follow(backend),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.release(backend);
                return;
            }
        }
    }
}

/// PipeWire through WirePlumber's `wpctl`.
#[derive(Default)]
struct Wpctl {
    watchdog: Option<Child>,
}

impl Backend for Wpctl {
    fn default_sink(&mut self) -> Option<u32> {
        parse_inspect_id(&wpctl(&["inspect", "@DEFAULT_AUDIO_SINK@"])?)
    }

    fn muted(&mut self, id: u32) -> Option<bool> {
        parse_muted(&wpctl(&["get-volume", &id.to_string()])?)
    }

    fn set_mute(&mut self, id: u32, muted: bool) -> bool {
        wpctl(&["set-mute", &id.to_string(), if muted { "1" } else { "0" }]).is_some()
    }

    fn guard(&mut self, id: u32) {
        self.unguard();
        //= $1 is this server, $2 the node: unmute once the server is gone.
        let script =
            r#"while kill -0 "$1" 2>/dev/null; do sleep 1; done; exec wpctl set-mute "$2" 0"#;
        let mut cmd = crate::audio::command("sh");
        cmd.args([
            "-c",
            script,
            "gliff-unmute",
            &std::process::id().to_string(),
            &id.to_string(),
        ])
        //= Never the protocol pipe: in --stdio mode stdout is the wire.
        .stdout(Stdio::null())
        //= Its own process group, so a signal to the server's group (a
        //= terminal's Ctrl+C, a service stop) does not take it too.
        .process_group(0);
        match cmd.spawn() {
            Ok(child) => self.watchdog = Some(child),
            Err(e) => {
                tracing::warn!(error = %e, "no unmute watchdog; a crash would leave the speakers muted")
            }
        }
    }

    fn unguard(&mut self) {
        if let Some(mut child) = self.watchdog.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Wpctl {
    fn drop(&mut self) {
        self.unguard();
    }
}

/// Run `wpctl` with a deadline; its stdout on success.
fn wpctl(args: &[&str]) -> Option<String> {
    let mut child = crate::audio::command("wpctl")
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| tracing::debug!(error = %e, "cannot run wpctl"))
        .ok()?;
    let deadline = Instant::now() + WPCTL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                tracing::warn!(?args, "wpctl did not answer; giving up");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status.success().then_some(out)
}

/// The node id from `wpctl inspect`: `id 52, type PipeWire:Interface:Node/3`.
fn parse_inspect_id(text: &str) -> Option<u32> {
    let line = text.lines().next()?.trim();
    line.strip_prefix("id ")?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Whether `wpctl get-volume` says muted: `Volume: 0.40 [MUTED]`.
fn parse_muted(text: &str) -> Option<bool> {
    let line = text.lines().next()?.trim();
    line.starts_with("Volume:")
        .then(|| line.contains("[MUTED]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A sound server with outputs, their mute state, and a log of calls.
    #[derive(Default)]
    struct Fake {
        default: Option<u32>,
        mutes: HashMap<u32, bool>,
        guarded: Option<u32>,
        fail_set: bool,
    }

    impl Backend for Fake {
        fn default_sink(&mut self) -> Option<u32> {
            self.default
        }
        fn muted(&mut self, id: u32) -> Option<bool> {
            self.mutes.get(&id).copied()
        }
        fn set_mute(&mut self, id: u32, muted: bool) -> bool {
            if self.fail_set {
                return false;
            }
            self.mutes.insert(id, muted);
            true
        }
        fn guard(&mut self, id: u32) {
            self.guarded = Some(id);
        }
        fn unguard(&mut self) {
            self.guarded = None;
        }
    }

    fn fake(outputs: &[(u32, bool)], default: u32) -> Fake {
        Fake {
            default: Some(default),
            mutes: outputs.iter().copied().collect(),
            ..Fake::default()
        }
    }

    #[test]
    fn mutes_then_restores_the_default_output() {
        let mut b = fake(&[(52, false)], 52);
        let mut s = State::default();
        s.engage(&mut b);
        assert!(b.mutes[&52]);
        assert_eq!(b.guarded, Some(52), "a crash would still unmute");
        s.release(&mut b);
        assert!(!b.mutes[&52]);
        assert_eq!(b.guarded, None);
    }

    #[test]
    fn an_output_muted_before_stays_muted() {
        let mut b = fake(&[(52, true)], 52);
        let mut s = State::default();
        s.engage(&mut b);
        assert_eq!(b.guarded, None, "nothing of ours to undo");
        s.release(&mut b);
        assert!(b.mutes[&52]);
    }

    #[test]
    fn follows_a_new_default_output() {
        let mut b = fake(&[(52, false), (60, false)], 52);
        let mut s = State::default();
        s.engage(&mut b);
        b.default = Some(60); //= headphones plugged in
        s.follow(&mut b);
        assert!(!b.mutes[&52], "the old output is restored");
        assert!(b.mutes[&60]);
        assert_eq!(b.guarded, Some(60));
        s.release(&mut b);
        assert!(!b.mutes[&60]);
    }

    #[test]
    fn a_released_mute_does_not_follow() {
        let mut b = fake(&[(52, false)], 52);
        let mut s = State::default();
        s.engage(&mut b);
        s.release(&mut b);
        s.follow(&mut b);
        assert!(!b.mutes[&52]);
    }

    #[test]
    fn a_failed_mute_holds_nothing() {
        let mut b = fake(&[(52, false)], 52);
        b.fail_set = true;
        let mut s = State::default();
        s.engage(&mut b);
        assert_eq!(s.held, None);
        assert_eq!(b.guarded, None);
    }

    #[test]
    fn no_sound_server_is_not_an_error() {
        let mut b = Fake::default();
        let mut s = State::default();
        s.engage(&mut b);
        s.follow(&mut b);
        s.release(&mut b);
        assert_eq!(s.held, None);
    }

    #[test]
    fn closing_the_channel_restores() {
        let (tx, rx) = mpsc::channel();
        tx.send(Cmd::Engage).unwrap();
        drop(tx);
        let mut b = fake(&[(52, false)], 52);
        helper(rx, &mut b);
        assert!(!b.mutes[&52]);
        assert_eq!(b.guarded, None);
    }

    #[test]
    fn parses_wpctl_output() {
        assert_eq!(
            parse_inspect_id("id 52, type PipeWire:Interface:Node/3\n  * node.name = \"x\"\n"),
            Some(52)
        );
        assert_eq!(parse_inspect_id("Object not found\n"), None);
        assert_eq!(parse_muted("Volume: 0.40 [MUTED]\n"), Some(true));
        assert_eq!(parse_muted("Volume: 1.00\n"), Some(false));
        assert_eq!(parse_muted("Translate ID error\n"), None);
    }
}
