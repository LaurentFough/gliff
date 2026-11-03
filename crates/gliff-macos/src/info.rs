//! Connection Info: what one Mac viewer session measures, once a second, for
//! the translucent overlay the viewer shows on request (the window's ⓘ
//! button or Machine ▸ Show Connection Info). The Linux client's stats line
//! (`crates/gliff/src/linux.rs`, `Status::Stats`) shows the same numbers.
//!
//! The session counts frames, video bytes and decode times as they happen
//! and the round trip from its `Ping`s; [`Meter::report`] turns the last
//! second into a `connection_info` command for the viewer, which adds the
//! CPU use of the client and the viewer (it can read both processes).
//!
//! Version: 1.0.0

use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How often the viewer gets a report (and the session pings for latency).
pub const INTERVAL: Duration = Duration::from_secs(1);

/// The stream as configured (`StreamConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    pub width: u32,
    pub height: u32,
    /// Dual420: full (4:4:4) colour.
    pub full_colour: bool,
    /// The server's frame-rate ceiling.
    pub fps_cap: u32,
}

/// One session's counters.
#[derive(Debug)]
pub struct Meter {
    host: String,
    since: Instant,
    frames: u32,
    bytes: u64,
    decode_ms: f64,
    decoded: u32,
    /// Last round trip, from the newest `Pong`.
    rtt_ms: Option<f64>,
    stream: Option<Stream>,
    /// The server's encoder tier (`StreamConfig.pipeline`): "GPU" or "CPU".
    server: Option<&'static str>,
}

impl Meter {
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            since: Instant::now(),
            frames: 0,
            bytes: 0,
            decode_ms: 0.0,
            decoded: 0,
            rtt_ms: None,
            stream: None,
            server: None,
        }
    }

    pub fn configured(&mut self, stream: Stream, server_gpu: Option<bool>) {
        self.stream = Some(stream);
        self.server = server_gpu.map(|gpu| if gpu { "GPU" } else { "CPU" });
    }

    /// A coded frame arrived (`VideoFrame` payload bytes).
    pub fn received(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
    }

    /// A frame was decoded; `ms` when the decoder timed it.
    pub fn decoded(&mut self, ms: Option<f64>) {
        self.frames += 1;
        if let Some(ms) = ms.filter(|m| m.is_finite() && *m >= 0.0) {
            self.decode_ms += ms;
            self.decoded += 1;
        }
    }

    /// A `Pong` for the `Ping` sent at `sent_ms` (both this Mac's clock).
    pub fn pong(&mut self, sent_ms: u64, now_ms: u64) {
        //= A Pong from before a clock step would read as hours: ignore it.
        if let Some(rtt) = now_ms.checked_sub(sent_ms).filter(|&d| d < 60_000) {
            self.rtt_ms = Some(rtt as f64);
        }
    }

    /// The last interval as a `connection_info` command, and a fresh interval.
    /// `gpu`: this viewer decodes in hardware; `audio_ms`: the sound's
    /// buffered delay while it plays.
    pub fn report(&mut self, gpu: bool, audio_ms: Option<f64>) -> Value {
        let secs = self.since.elapsed().as_secs_f64().max(0.001);
        let fps = f64::from(self.frames) / secs;
        let mbit = self.bytes as f64 * 8.0 / 1_000_000.0 / secs;
        let decode_ms = (self.decoded > 0).then(|| self.decode_ms / f64::from(self.decoded));
        let client = if gpu { "GPU" } else { "CPU" };
        let value = json!({
            "type": "connection_info",
            "host": self.host,
            "fps": round1(fps),
            "mbit": round1(mbit),
            "decode_ms": decode_ms.map(round1),
            "rtt_ms": self.rtt_ms,
            "width": self.stream.map(|s| s.width),
            "height": self.stream.map(|s| s.height),
            "colour": self.stream.map(|s| if s.full_colour { "4:4:4" } else { "4:2:0" }),
            "fps_cap": self.stream.map(|s| s.fps_cap),
            //= "server tier→client tier", as the Linux stats line: "GPU→GPU".
            "video": format!("{}→{client}", self.server.unwrap_or("?")),
            "audio_ms": audio_ms.map(|ms| ms.round()),
        });
        self.since = Instant::now();
        self.frames = 0;
        self.bytes = 0;
        self.decode_ms = 0.0;
        self.decoded = 0;
        value
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_covers_the_last_interval_then_starts_again() {
        let mut m = Meter::new("me@box");
        m.configured(
            Stream {
                width: 1920,
                height: 1200,
                full_colour: true,
                fps_cap: 30,
            },
            Some(true),
        );
        for _ in 0..3 {
            m.received(125_000);
            m.decoded(Some(2.0));
        }
        m.decoded(None);
        m.pong(1_000, 1_018);
        let r = m.report(true, Some(41.6));
        assert_eq!(r["type"], "connection_info");
        assert_eq!(r["host"], "me@box");
        assert!(r["fps"].as_f64().unwrap() > 0.0);
        assert!(r["mbit"].as_f64().unwrap() > 0.0);
        assert_eq!(
            r["decode_ms"], 2.0,
            "frames without a time are not averaged in"
        );
        assert_eq!(r["rtt_ms"], 18.0);
        assert_eq!(
            (r["width"].as_u64(), r["height"].as_u64()),
            (Some(1920), Some(1200))
        );
        assert_eq!(r["colour"], "4:4:4");
        assert_eq!(r["video"], "GPU→GPU");
        assert_eq!(r["audio_ms"], 42.0);
        let next = m.report(false, None);
        assert_eq!(next["fps"], 0.0);
        assert!(next["decode_ms"].is_null());
        assert_eq!(
            next["rtt_ms"], 18.0,
            "the latency is kept until the next Pong"
        );
        assert_eq!(next["video"], "GPU→CPU");
        assert!(next["audio_ms"].is_null());
    }

    #[test]
    fn before_a_stream_the_report_says_so_and_bad_input_is_ignored() {
        let mut m = Meter::new("h");
        m.pong(5_000, 1_000);
        m.pong(0, 3_600_000);
        m.decoded(Some(f64::NAN));
        let r = m.report(false, None);
        assert!(r["rtt_ms"].is_null() && r["width"].is_null() && r["decode_ms"].is_null());
        assert_eq!(r["video"], "?→CPU");
    }
}
