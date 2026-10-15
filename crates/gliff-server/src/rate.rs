//! Link estimation and rate control for a frame-paced stream.
//!
//! The estimator consumes transport-independent signals — bytes sent per
//! frame, one ack per frame, an optional seeded round trip — so a future
//! UDP transport feeds the same interface. The controller follows the shape
//! of Google Congestion Control (draft-ietf-rmcat-gcc): multiplicative
//! start-up, ~8%/s growth when quiet, a cut to 0.85x the delivered rate on
//! sustained delay growth. It works per frame, not per packet, because TCP
//! gives one ack per frame after decode.
//!
//! Everything here is pure logic: methods take `now` so tests drive a fake
//! clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A frame in flight, with the delivery snapshot taken when it was sent
/// (BBR-style delivery-rate sampling).
struct SentFrame {
    frame_id: u64,
    bytes: usize,
    sent_at: Instant,
    /// The sender was not using all the capacity it had (pace-bound, a
    /// static screen, or a refinement pass): the sample may only raise the
    /// rate estimate, never lower it.
    app_limited: bool,
    delivered_at_send: u64,
    delivered_time_at_send: Option<Instant>,
}

/// One delivery-rate sample.
struct RateSample {
    at: Instant,
    bps: f64,
    app_limited: bool,
}

/// Measures what the link delivers: base round trip, jitter, queueing
/// growth, delivered rate, and bytes in flight.
pub struct LinkEstimator {
    sent: VecDeque<SentFrame>,
    /// Cumulative acked bytes and the time of the last ack.
    delivered: u64,
    delivered_time: Option<Instant>,
    /// Raw round-trip samples for the sliding base window.
    rtt_window: VecDeque<(Instant, f64)>,
    srtt_ms: Option<f64>,
    mdev_ms: f64,
    rate_samples: VecDeque<RateSample>,
    /// Ack times for the delivered-fps estimate.
    ack_times: VecDeque<Instant>,
    /// Smallest round trip seen since the last `take_window_queueing_ms`.
    window_min_ms: Option<f64>,
    inflight_bytes: u64,
    /// Largest frame sent within the rate window, for the cap floor.
    largest_frame: usize,
    /// Smoothed sent-frame size, to estimate the next frame's cost.
    avg_frame_bytes: f64,
}

/// The base-RTT window: long enough to ride out loss-recovery stalls,
/// short enough to relearn a changed path.
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Delivered-fps window.
const FPS_WINDOW: Duration = Duration::from_secs(2);

impl LinkEstimator {
    pub fn new() -> Self {
        Self {
            sent: VecDeque::new(),
            delivered: 0,
            delivered_time: None,
            rtt_window: VecDeque::new(),
            srtt_ms: None,
            mdev_ms: 0.0,
            rate_samples: VecDeque::new(),
            ack_times: VecDeque::new(),
            window_min_ms: None,
            inflight_bytes: 0,
            largest_frame: 0,
            avg_frame_bytes: 0.0,
        }
    }

    pub fn on_sent(&mut self, now: Instant, frame_id: u64, bytes: usize, app_limited: bool) {
        let delivered_time_at_send = self
            .delivered_time
            .filter(|t| now.duration_since(*t) <= self.rate_window(now));
        self.sent.push_back(SentFrame {
            frame_id,
            bytes,
            sent_at: now,
            app_limited,
            delivered_at_send: self.delivered,
            delivered_time_at_send,
        });
        self.inflight_bytes += bytes as u64;
        self.largest_frame = self.largest_frame.max(bytes);
        self.avg_frame_bytes = if self.avg_frame_bytes == 0.0 {
            bytes as f64
        } else {
            0.8 * self.avg_frame_bytes + 0.2 * bytes as f64
        };
    }

    /// Record one ack. Pairs by frame id; earlier unacked frames count as
    /// delivered too — cumulative semantics that hold only on an ordered
    /// transport. A UDP path, where frame 12 can arrive while frame 11 is
    /// lost, must not call this per datagram; it needs its own entry point
    /// that credits exactly the frames that arrived.
    pub fn on_ack(&mut self, now: Instant, frame_id: u64) {
        let Some(pos) = self.sent.iter().position(|f| f.frame_id == frame_id) else {
            tracing::warn!(frame_id, "ack for an unknown frame");
            return;
        };
        if pos > 0 {
            tracing::debug!(frame_id, skipped = pos, "ack skipped earlier frames");
        }
        let mut frame = None;
        for f in self.sent.drain(..=pos) {
            self.inflight_bytes = self.inflight_bytes.saturating_sub(f.bytes as u64);
            self.delivered += f.bytes as u64;
            frame = Some(f);
        }
        self.delivered_time = Some(now);
        let frame = frame.expect("drained at least one frame");

        let rtt_ms = now.duration_since(frame.sent_at).as_secs_f64() * 1000.0;
        self.record_rtt(now, rtt_ms);

        // Delivery rate: bytes newly delivered since this frame left, over
        // the larger of the send and ack intervals, so neither a burst of
        // acks after a stall nor a long-idle first frame overstates it.
        if let Some(t) = frame.delivered_time_at_send {
            let elapsed = now
                .duration_since(t)
                .max(now.duration_since(frame.sent_at))
                .as_secs_f64();
            if elapsed > 0.0 {
                let bps = (self.delivered - frame.delivered_at_send) as f64 * 8.0 / elapsed;
                self.record_rate(now, bps, frame.app_limited);
            }
        }

        self.ack_times.push_back(now);
        while self
            .ack_times
            .front()
            .is_some_and(|t| now.duration_since(*t) > FPS_WINDOW)
        {
            self.ack_times.pop_front();
        }
    }

    /// Seed the round trip before any frame is acked (the handshake ping).
    pub fn seed_rtt(&mut self, now: Instant, rtt_ms: f64) {
        if self.srtt_ms.is_none() {
            self.record_rtt(now, rtt_ms);
        }
    }

    fn record_rtt(&mut self, now: Instant, rtt_ms: f64) {
        self.rtt_window.push_back((now, rtt_ms));
        while self
            .rtt_window
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > BASE_WINDOW)
        {
            self.rtt_window.pop_front();
        }
        match self.srtt_ms {
            None => {
                self.srtt_ms = Some(rtt_ms);
                self.mdev_ms = rtt_ms / 2.0;
            }
            Some(srtt) => {
                self.mdev_ms = 0.75 * self.mdev_ms + 0.25 * (rtt_ms - srtt).abs();
                self.srtt_ms = Some(0.875 * srtt + 0.125 * rtt_ms);
            }
        }
        self.window_min_ms = Some(self.window_min_ms.map_or(rtt_ms, |m: f64| m.min(rtt_ms)));
    }

    fn record_rate(&mut self, now: Instant, bps: f64, app_limited: bool) {
        // An app-limited sample proves capacity only when it beats the
        // current estimate; it never drags the estimate down.
        if app_limited && bps <= self.rate_max_bps(now).unwrap_or(0.0) {
            return;
        }
        self.rate_samples.push_back(RateSample {
            at: now,
            bps,
            app_limited,
        });
        let window = self.rate_window(now);
        while self
            .rate_samples
            .front()
            .is_some_and(|s| now.duration_since(s.at) > window)
        {
            self.rate_samples.pop_front();
        }
        self.largest_frame = self.avg_frame_bytes as usize;
    }

    fn rate_window(&self, now: Instant) -> Duration {
        let base = self.base_rtt_ms(now).unwrap_or(100.0);
        Duration::from_secs_f64((10.0 * base / 1000.0).clamp(1.0, 15.0))
    }

    /// Minimum round trip over the base window.
    pub fn base_rtt_ms(&self, _now: Instant) -> Option<f64> {
        self.rtt_window
            .iter()
            .map(|(_, ms)| *ms)
            .min_by(|a, b| a.total_cmp(b))
    }

    pub fn mdev_ms(&self) -> f64 {
        self.mdev_ms
    }

    /// Queueing growth beyond which the delay is congestion, not jitter.
    /// The upper clamp keeps the first samples (mdev = rtt/2) from setting
    /// a threshold nothing can cross.
    pub fn queue_threshold_ms(&self, now: Instant) -> f64 {
        let base = self.base_rtt_ms(now).unwrap_or(50.0);
        (4.0 * self.mdev_ms).clamp(50.0, (2.0 * base).max(50.0))
    }

    /// Smallest round trip since the last call, minus the base: the queueing
    /// every packet saw. `None` when no ack arrived in the window. Using the
    /// window minimum means one loss-recovery stall cannot register.
    pub fn take_window_queueing_ms(&mut self, now: Instant) -> Option<f64> {
        let min = self.window_min_ms.take()?;
        Some((min - self.base_rtt_ms(now)?).max(0.0))
    }

    /// Max-filtered delivered rate: capacity for growth and the byte cap.
    pub fn rate_max_bps(&self, _now: Instant) -> Option<f64> {
        self.rate_samples
            .iter()
            .map(|s| s.bps)
            .max_by(|a, b| a.total_cmp(b))
    }

    /// Recent delivered rate from samples the sender was not app-limited
    /// on: the basis for cuts.
    pub fn rate_recent_bps(&self, now: Instant) -> Option<f64> {
        let window =
            Duration::from_secs_f64((self.base_rtt_ms(now).unwrap_or(500.0) / 1000.0).max(0.5));
        let recent: Vec<f64> = self
            .rate_samples
            .iter()
            .filter(|s| !s.app_limited && now.duration_since(s.at) <= window)
            .map(|s| s.bps)
            .collect();
        if recent.is_empty() {
            return None;
        }
        Some(recent.iter().sum::<f64>() / recent.len() as f64)
    }

    /// Frames delivered per second, over the fps window.
    pub fn delivered_fps(&self, now: Instant) -> f64 {
        let n = self
            .ack_times
            .iter()
            .filter(|t| now.duration_since(**t) <= FPS_WINDOW)
            .count();
        n as f64 / FPS_WINDOW.as_secs_f64()
    }

    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes
    }

    /// The next frame's likely size, for the room check.
    pub fn est_frame_bytes(&self) -> u64 {
        self.avg_frame_bytes.max(1.0) as u64
    }

    /// Bytes allowed in flight. A frame-aware floor holds for the whole
    /// session: without it a short round trip shrinks the cap below one
    /// frame and nothing can ever be sent again.
    pub fn cap_bytes(&self, now: Instant, slow_start: bool) -> u64 {
        let floor = (2 * self.largest_frame.max(self.avg_frame_bytes as usize)).max(96 * 1024);
        let (Some(rate), Some(base)) = (self.rate_max_bps(now), self.base_rtt_ms(now)) else {
            return floor as u64;
        };
        let gain = if slow_start { 2.0 } else { 1.5 };
        let bdp_bytes = rate / 8.0 * base / 1000.0;
        (gain * bdp_bytes).max(floor as f64) as u64
    }
}

/// Why the controller changed the target, for the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    SlowStart,
    Grow,
    Track,
    Rtt,
    Rate,
}

/// One quality level: what the stream gives up when the link cannot pay
/// for full quality.
pub struct Level {
    /// Frame-rate ceiling; `u32::MAX` means the session maximum.
    pub fps_cap: u32,
    /// Keep the auxiliary chroma stream (Dual420) when the client has it.
    pub aux: bool,
    /// Stream resolution as a share of the full-quality fit size.
    pub scale: f32,
}

/// Frame rate degrades first, because the pointer is local and a sharp
/// still picture matters more on a desktop than motion; then the chroma
/// detail (Dual420 -> Single420 costs only coloured fringes); resolution
/// last, because it is the most visible loss on text. The fps floors are
/// policy: never below 15 before chroma is given up, never below 10 before
/// resolution, 5 is the absolute floor.
pub const LEVELS: [Level; 7] = [
    Level {
        fps_cap: u32::MAX,
        aux: true,
        scale: 1.0,
    },
    Level {
        fps_cap: 30,
        aux: true,
        scale: 1.0,
    },
    Level {
        fps_cap: 15,
        aux: true,
        scale: 1.0,
    },
    Level {
        fps_cap: 15,
        aux: false,
        scale: 1.0,
    },
    Level {
        fps_cap: 10,
        aux: false,
        scale: 1.0,
    },
    Level {
        fps_cap: 10,
        aux: false,
        scale: 0.5,
    },
    Level {
        fps_cap: 5,
        aux: false,
        scale: 0.5,
    },
];

/// Picks the quality level the measured rate can pay for, with hysteresis:
/// step down after a short hold, step up only after `hold_up`, which
/// doubles when an up-step flaps back down. During slow start, upward
/// moves use the first measured capacity while downward moves wait for
/// confirmed congestion.
pub struct Ladder {
    index: usize,
    hold_up: Duration,
    below_since: Option<Instant>,
    last_change: Option<Instant>,
    last_step_up: Option<Instant>,
}

const HOLD_DOWN: Duration = Duration::from_secs(2);
const HOLD_UP: Duration = Duration::from_secs(4);
const MAX_HOLD_UP: Duration = Duration::from_secs(300);
const DOWN_FLOOR_MULT: f64 = 0.9;

impl Ladder {
    pub fn new(start: usize) -> Self {
        Self {
            index: start.min(LEVELS.len() - 1),
            hold_up: HOLD_UP,
            below_since: None,
            last_change: None,
            last_step_up: None,
        }
    }

    pub fn level(&self) -> &'static Level {
        &LEVELS[self.index]
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Bits per pixel one stream gets at `level` when the link delivers
    /// `rate_bps` and the full-quality fit size is `pixels` at `max_fps`.
    /// A rung that would drop the auxiliary stream still pays for two when
    /// the client cannot decode Single420 (`can_drop_aux` false).
    fn affordable_bpp(
        rate_bps: f64,
        pixels: u64,
        dual: bool,
        can_drop_aux: bool,
        max_fps: u32,
        level: &Level,
    ) -> f64 {
        let streams = if dual && (level.aux || !can_drop_aux) {
            2.0
        } else {
            1.0
        };
        let fps = level.fps_cap.min(max_fps).max(1) as f64;
        let px = (pixels as f64 * level.scale as f64 * level.scale as f64).max(1.0);
        0.85 * rate_bps / (streams * fps * px)
    }

    /// The lowest (best) level whose per-stream budget stays at or above
    /// `floor_mult` x FLOOR_BPP; the last level when none does.
    fn select(
        rate_bps: f64,
        pixels: u64,
        dual: bool,
        can_drop_aux: bool,
        max_fps: u32,
        floor_mult: f64,
    ) -> usize {
        LEVELS
            .iter()
            .position(|l| {
                Self::affordable_bpp(rate_bps, pixels, dual, can_drop_aux, max_fps, l)
                    >= floor_mult * FLOOR_BPP
            })
            .unwrap_or(LEVELS.len() - 1)
    }

    /// Consider a move. Returns the new index when the level changed.
    #[allow(clippy::too_many_arguments)]
    pub fn consider(
        &mut self,
        now: Instant,
        link: &LinkEstimator,
        slow_start: bool,
        recent_congestion: bool,
        pixels: u64,
        dual: bool,
        can_drop_aux: bool,
        max_fps: u32,
    ) -> Option<usize> {
        let rate = link.rate_max_bps(now)?;
        // Down-moves need real (not app-limited) evidence: a static screen's
        // refinement trickle must not read as a slow link.
        let real = link.rate_recent_bps(now).is_some();
        let sel = Self::select(rate, pixels, dual, can_drop_aux, max_fps, 1.0);
        let down = Self::select(rate, pixels, dual, can_drop_aux, max_fps, DOWN_FLOOR_MULT);

        if slow_start {
            if sel < self.index || (down > self.index && real && recent_congestion) {
                self.move_to(now, sel);
                return Some(sel);
            }
            return None;
        }

        if sel < self.index {
            self.below_since = None;
            // Step up needs headroom (1.25x the floor) and a full hold.
            let up = Self::select(rate, pixels, dual, can_drop_aux, max_fps, 1.25);
            let held = self
                .last_change
                .is_none_or(|t| now.duration_since(t) >= self.hold_up);
            if up < self.index && held {
                self.last_step_up = Some(now);
                self.move_to(now, up);
                return Some(up);
            }
        } else if down > self.index && real {
            let since = *self.below_since.get_or_insert(now);
            if recent_congestion || now.duration_since(since) >= HOLD_DOWN {
                // An up-step that flaps straight back down halves how often
                // the link gets probed upward again.
                if self
                    .last_step_up
                    .is_some_and(|t| now.duration_since(t) <= 2 * self.hold_up)
                {
                    self.hold_up = (2 * self.hold_up).min(MAX_HOLD_UP);
                }
                self.move_to(now, sel);
                return Some(sel);
            }
        } else {
            self.below_since = None;
        }
        None
    }

    fn move_to(&mut self, now: Instant, index: usize) {
        self.index = index;
        self.last_change = Some(now);
        self.below_since = None;
    }
}

/// Adapts the CBR target (bits per second, all streams together) to what
/// the estimator measured.
pub struct RateController {
    min: u32,
    max: u32,
    /// Per-stream bits per frame under which the ladder should give
    /// something up, and the VBV window tightens.
    floor_bpf: u32,
    target: u32,
    /// Set by `--bitrate`: an upper cap the target never exceeds.
    fixed_cap: Option<u32>,
    slow_start: bool,
    started: Option<Instant>,
    startup_idle_since: Option<Instant>,
    last_congestion: Option<Instant>,
    last_eval: Option<Instant>,
    last_change: Option<Instant>,
    /// Cumulative delivered bytes at the last slow-start raise: the next
    /// raise needs a full flight of new evidence.
    raised_at_delivered: u64,
    /// Consecutive evaluations that looked like overuse.
    overuse_evals: u32,
    /// A frame waited on the byte cap since the last evaluation.
    blocked_at_cap: bool,
    /// A frame write took longer than a frame interval since the last
    /// evaluation (the writer, not the byte cap, was the bottleneck).
    writer_blocked: bool,
    got_ack: bool,
    streams: u32,
    pixels: u64,
}

pub const MAX_BPP: f64 = 0.1;
pub const MIN_BPP: f64 = 0.005;
pub const FLOOR_BPP: f64 = 0.03;
const INITIAL_BPS: u32 = 3_000_000;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const CONGESTION_RECENCY: Duration = Duration::from_secs(2);

impl RateController {
    pub fn new(pixels: u64, streams: u32, max_fps: u32, fixed_cap: Option<u32>) -> Self {
        let mut ctl = Self {
            min: 0,
            max: 0,
            floor_bpf: 0,
            target: 0,
            fixed_cap,
            slow_start: true,
            started: None,
            startup_idle_since: None,
            last_congestion: None,
            last_eval: None,
            last_change: None,
            raised_at_delivered: 0,
            overuse_evals: 0,
            blocked_at_cap: false,
            writer_blocked: false,
            got_ack: false,
            streams: streams.max(1),
            pixels,
        };
        ctl.set_bounds(pixels, streams, max_fps);
        ctl.target = INITIAL_BPS.clamp(ctl.min, ctl.effective_max());
        ctl
    }

    fn set_bounds(&mut self, pixels: u64, streams: u32, max_fps: u32) {
        self.pixels = pixels;
        self.streams = streams.max(1);
        self.max = ((pixels as f64 * streams.max(1) as f64 * max_fps as f64 * MAX_BPP) as u64)
            .min(80_000_000) as u32;
        // The floor must never exceed the ceiling (a tiny stream, or a
        // `--bitrate` cap below the floor), or the clamps panic.
        self.min = (((pixels as f64 * MIN_BPP * 5.0) as u64).max(100_000) as u32)
            .min(self.effective_max_raw())
            .max(1);
        self.floor_bpf = (pixels as f64 * FLOOR_BPP) as u32;
    }

    fn effective_max_raw(&self) -> u32 {
        self.fixed_cap.map_or(self.max, |c| c.min(self.max)).max(1)
    }

    fn effective_max(&self) -> u32 {
        self.effective_max_raw().max(self.min)
    }

    /// The stream geometry, stream count or fps ceiling changed: recompute
    /// the bounds and keep the same bits per second where possible.
    pub fn reconfigure(&mut self, pixels: u64, streams: u32, max_fps: u32) {
        self.set_bounds(pixels, streams, max_fps);
        self.target = self.target.clamp(self.min, self.effective_max());
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    /// The CBR bitrate handed to one encoder stream.
    pub fn stream_bitrate(&self) -> u32 {
        (self.target / self.streams).max(1)
    }

    pub fn is_slow_start(&self) -> bool {
        self.slow_start
    }

    pub fn congested_recently(&self, now: Instant) -> bool {
        self.last_congestion
            .is_some_and(|t| now.duration_since(t) <= CONGESTION_RECENCY)
    }

    pub fn note_idle(&mut self, now: Instant) {
        if self.slow_start {
            self.startup_idle_since.get_or_insert(now);
        }
    }

    pub fn note_frame_sent(&mut self, now: Instant) {
        if let (Some(started), Some(idle_since)) =
            (self.started.as_mut(), self.startup_idle_since.take())
        {
            *started += now - idle_since;
        }
    }

    /// Per-stream bits available for one frame at this pace.
    pub fn budget_per_frame(&self, fps_cmd: u32) -> u32 {
        self.target / (self.streams * fps_cmd.max(1))
    }

    /// The rate-control buffer window: small when the budget is tight or
    /// nothing is known yet, so one keyframe cannot stall a slow link.
    pub fn vbv_ms(&self, fps_cmd: u32) -> u32 {
        if !self.got_ack {
            250
        } else if self.budget_per_frame(fps_cmd) < 2 * self.floor_bpf {
            100
        } else {
            500
        }
    }

    pub fn note_blocked_at_cap(&mut self) {
        self.blocked_at_cap = true;
    }

    pub fn note_writer_blocked(&mut self) {
        self.writer_blocked = true;
    }

    fn eval_every(&self, link: &LinkEstimator, now: Instant) -> Duration {
        let base = link.base_rtt_ms(now).unwrap_or(0.0);
        Duration::from_millis(250).max(Duration::from_secs_f64(base / 1000.0))
    }

    fn grow_after(&self, link: &LinkEstimator, now: Instant) -> Duration {
        let base = link.base_rtt_ms(now).unwrap_or(0.0);
        Duration::from_millis(1500).max(Duration::from_secs_f64(2.0 * base / 1000.0))
    }

    /// Feed one ack. Returns the reason when the target changed.
    pub fn on_ack(&mut self, now: Instant, link: &mut LinkEstimator) -> Option<Reason> {
        self.got_ack = true;
        let started = *self.started.get_or_insert(now);

        if self.slow_start {
            if now.duration_since(started) > STARTUP_TIMEOUT {
                self.slow_start = false;
            } else if let Some(rate) = link.rate_max_bps(now) {
                // Raise once per completed flight of new evidence: acks for
                // frames sent at the previous target.
                if link.delivered_bytes() > self.raised_at_delivered {
                    let next = ((2.0 * rate) as u32).clamp(self.target, self.effective_max());
                    self.raised_at_delivered = link.delivered_bytes() + link.inflight_bytes();
                    if next > self.target {
                        self.apply(now, next, Reason::SlowStart);
                        return Some(Reason::SlowStart);
                    }
                    if next >= self.effective_max() {
                        self.slow_start = false;
                    }
                }
            }
        }

        let last_eval = *self.last_eval.get_or_insert(now);
        if now.duration_since(last_eval) < self.eval_every(link, now) {
            return None;
        }
        self.last_eval = Some(now);
        let blocked = self.blocked_at_cap || self.writer_blocked;
        self.blocked_at_cap = false;
        self.writer_blocked = false;

        // No ack landed in this window: no evidence either way.
        let queueing = link.take_window_queueing_ms(now)?;

        let rate_max = link.rate_max_bps(now);
        let rate_recent = link.rate_recent_bps(now);
        let overuse = queueing > link.queue_threshold_ms(now)
            || (!self.slow_start
                && matches!((rate_recent, rate_max), (Some(r), Some(m)) if r < 0.7 * m));
        if blocked && overuse {
            self.overuse_evals += 1;
        } else {
            self.overuse_evals = 0;
        }

        if self.overuse_evals >= 2 {
            self.overuse_evals = 0;
            self.slow_start = false;
            self.last_congestion = Some(now);
            // 0.85x what the link really delivered (GCC's beta). No clamp
            // relative to the old target: after a bandwidth collapse the
            // target must follow in one or two cuts, not by 10% per window.
            let basis = rate_recent.or(rate_max).unwrap_or(self.target as f64);
            let ceiling = ((self.target as f64 * 0.95) as u32).max(self.min);
            let next = ((0.85 * basis) as u32).clamp(self.min, ceiling);
            if next < self.target {
                let reason = if queueing > link.queue_threshold_ms(now) {
                    Reason::Rtt
                } else {
                    Reason::Rate
                };
                self.apply(now, next, reason);
                return Some(reason);
            }
            return None;
        }
        // One overuse window already seen: hold, never raise into a queue
        // the next window may confirm (a track jump here would undo the cut
        // that is about to happen).
        if self.overuse_evals > 0 || self.slow_start {
            return None;
        }
        // Quiet for a full hold after the last cut or jump: raise the
        // target. This is not gated on being blocked, or a target once cut
        // on a quiet link could never recover (the target is a ceiling;
        // unused headroom costs nothing).
        let last_change = self.last_change.unwrap_or(started);
        if now.duration_since(last_change) < self.grow_after(link, now) {
            return None;
        }
        if let Some(rate) = rate_max {
            let track = (0.85 * rate) as u32;
            if track > self.target {
                let next = track.min(self.effective_max());
                self.apply(now, next, Reason::Track);
                return Some(Reason::Track);
            }
        }
        let dt = self.eval_every(link, now).as_secs_f64();
        let next = ((self.target as f64 * (1.0 + 0.08 * dt)) as u32).min(self.effective_max());
        if next > self.target {
            self.apply(now, next, Reason::Grow);
            return Some(Reason::Grow);
        }
        None
    }

    fn apply(&mut self, now: Instant, next: u32, reason: Reason) {
        tracing::info!(from = self.target, to = next, ?reason, "adapting bitrate");
        self.target = next;
        // Growth runs once per evaluation while quiet (~8%/s); resetting the
        // hold on every grow would throttle it to one step per hold.
        if reason != Reason::Grow {
            self.last_change = Some(now);
        }
    }
}

impl LinkEstimator {
    fn delivered_bytes(&self) -> u64 {
        self.delivered
    }
}

/// The shortest time between two rebuilds of the CPU encoder for a rate
/// change that is not a congestion cut.
pub const CPU_REBUILD_INTERVAL: Duration = Duration::from_secs(2);

/// Whether the CPU encoder should be rebuilt for a new target. OpenH264
/// fixes its rate at creation, so every rebuild sends a keyframe, and a
/// keyframe at screen-content bitrates blurs fine text. The commanded frame
/// rate follows the measured encode time and swings by a third from frame
/// to frame on a busy CPU: rebuilding on every swing made one frame in three
/// a keyframe. So a cut of a quarter or more (congestion) applies at once,
/// and otherwise only a move of 10% in bitrate or 25% in frame rate does,
/// at most once per [`CPU_REBUILD_INTERVAL`].
pub fn cpu_rebuild_due(current: (u32, u32), wanted: (u32, u32), since_rebuild: Duration) -> bool {
    let ((bitrate, framerate), (new_bitrate, new_framerate)) = (current, wanted);
    let relative = |a: u32, b: u32| (b as f64 - a as f64).abs() / a.max(1) as f64;
    if (new_bitrate as f64) <= bitrate as f64 * 0.75 {
        return true;
    }
    since_rebuild >= CPU_REBUILD_INTERVAL
        && (relative(bitrate, new_bitrate) >= 0.10 || relative(framerate, new_framerate) >= 0.25)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_rebuilds_are_paced_except_for_congestion() {
        let soon = Duration::from_millis(300);
        let later = CPU_REBUILD_INTERVAL;
        // The frame rate swings that rebuilt the encoder on every frame.
        assert!(!cpu_rebuild_due((22_000_000, 30), (22_000_000, 25), soon));
        assert!(!cpu_rebuild_due((22_000_000, 30), (22_000_000, 35), soon));
        // A small move never rebuilds, however long it has been.
        assert!(!cpu_rebuild_due((22_000_000, 30), (21_000_000, 27), later));
        // A real move rebuilds once the interval has passed.
        assert!(cpu_rebuild_due((22_000_000, 30), (22_000_000, 20), later));
        assert!(cpu_rebuild_due((3_000_000, 30), (21_000_000, 30), later));
        assert!(!cpu_rebuild_due((3_000_000, 30), (21_000_000, 30), soon));
        // A congestion cut applies at once.
        assert!(cpu_rebuild_due((22_000_000, 30), (15_000_000, 30), soon));
    }

    const MS: Duration = Duration::from_millis(1);

    /// Drive a link at a fixed rtt and capacity: send when the cap allows,
    /// ack after one rtt. Returns (delivered fps, ctl) after `secs`.
    struct Sim {
        link: LinkEstimator,
        ctl: RateController,
        ladder: Ladder,
        pixels: u64,
        streams: u32,
        max_fps: u32,
        level_changes: Vec<(Instant, usize)>,
        now: Instant,
        frame_id: u64,
        pending_acks: VecDeque<(Instant, u64)>,
    }

    impl Sim {
        fn new(pixels: u64, streams: u32, max_fps: u32) -> Self {
            Self {
                link: LinkEstimator::new(),
                ctl: RateController::new(pixels, streams, max_fps, None),
                ladder: Ladder::new(1),
                pixels,
                streams,
                max_fps,
                level_changes: Vec::new(),
                now: Instant::now(),
                frame_id: 0,
                pending_acks: VecDeque::new(),
            }
        }

        /// Run at 1 ms steps: pace `fps`, frame size from the target, ack
        /// after `rtt_ms` if capacity `cap_bps` allows the pace, else at
        /// the capacity's pace (a saturated link queues, modelled as
        /// added delay).
        fn run(&mut self, secs: f64, fps: u32, rtt_ms: f64, cap_bps: f64) -> f64 {
            let mut next_send = self.now;
            let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            let mut acked = 0u64;
            let started = self.now;
            let mut link_free = self.now;
            while self.now < started + Duration::from_secs_f64(secs) {
                self.now += MS;
                while self
                    .pending_acks
                    .front()
                    .is_some_and(|(t, _)| *t <= self.now)
                {
                    let (_, id) = self.pending_acks.pop_front().unwrap();
                    self.link.on_ack(self.now, id);
                    self.ctl.on_ack(self.now, &mut self.link);
                    if let Some(level) = self.ladder.consider(
                        self.now,
                        &self.link,
                        self.ctl.is_slow_start(),
                        self.ctl.congested_recently(self.now),
                        self.pixels,
                        self.streams == 2,
                        true,
                        self.max_fps,
                    ) {
                        self.level_changes.push((self.now, level));
                    }
                    if fps == 0 && self.link.inflight_bytes() == 0 {
                        self.ctl.note_idle(self.now);
                    }
                    acked += 1;
                }
                if fps == 0 || self.now < next_send {
                    continue;
                }
                // One simulated frame carries every stream's share.
                let bytes = (self.ctl.target() / (fps * 8)).max(500) as usize;
                let room = self.link.inflight_bytes() + self.link.est_frame_bytes()
                    <= self.link.cap_bytes(self.now, self.ctl.is_slow_start());
                if !room {
                    self.ctl.note_blocked_at_cap();
                    continue;
                }
                // Serialization on the capacity plus the propagation delay.
                let tx = Duration::from_secs_f64(bytes as f64 * 8.0 / cap_bps);
                link_free = link_free.max(self.now) + tx;
                let ack_at = link_free + Duration::from_secs_f64(rtt_ms / 1000.0);
                self.ctl.note_frame_sent(self.now);
                self.link.on_sent(self.now, self.frame_id, bytes, false);
                self.pending_acks.push_back((ack_at, self.frame_id));
                self.frame_id += 1;
                next_send = self.now + interval;
            }
            acked as f64 / secs
        }
    }

    #[test]
    fn fps_is_not_bound_by_ack_count_at_high_rtt() {
        // 1.1 s rtt, plenty of capacity: the old 2..8 frame window capped
        // this at ~7 fps; the byte cap must not, once the ramp is done.
        let mut sim = Sim::new(1920 * 1080, 1, 30);
        sim.run(10.0, 15, 1100.0, 20e6);
        let fps = sim.run(10.0, 15, 1100.0, 20e6);
        assert!(fps > 13.0, "delivered only {fps:.1} fps after the ramp");
    }

    #[test]
    fn cap_never_falls_below_a_frame_on_a_lan() {
        // Sub-millisecond rtt at a low rate: bdp is under one frame, and
        // sending must still be possible (the review's deadlock case).
        let mut link = LinkEstimator::new();
        let mut now = Instant::now();
        for id in 0..50 {
            link.on_sent(now, id, 20_000, false);
            now += MS;
            link.on_ack(now, id);
            now += MS;
        }
        let cap = link.cap_bytes(now, false);
        assert!(
            cap >= 2 * 20_000,
            "cap {cap} smaller than two frames at rtt ~1 ms"
        );
    }

    #[test]
    fn app_limited_samples_never_lower_the_rate() {
        let mut link = LinkEstimator::new();
        let mut now = Instant::now();
        // Establish ~8 Mbit/s with real traffic.
        for id in 0..20 {
            link.on_sent(now, id, 100_000, false);
            now += Duration::from_millis(100);
            link.on_ack(now, id);
        }
        let before = link.rate_max_bps(now).unwrap();
        assert!(before > 6e6, "setup rate {before}");
        // Then a trickle of tiny app-limited refinements.
        for id in 20..30 {
            link.on_sent(now, id, 1_000, true);
            now += Duration::from_millis(100);
            link.on_ack(now, id);
        }
        let after = link.rate_max_bps(now).unwrap();
        assert!(
            after >= before * 0.99,
            "rate fell from {before} to {after} on app-limited samples"
        );
    }

    #[test]
    fn one_stalled_window_does_not_cut() {
        let mut sim = Sim::new(1920 * 1080, 1, 30);
        sim.run(6.0, 15, 100.0, 10e6);
        let before = sim.ctl.target();
        // One loss-recovery stall: a single 2.5 s rtt sample while blocked.
        sim.ctl.note_blocked_at_cap();
        sim.link.on_sent(sim.now, 999_000, 20_000, false);
        sim.now += Duration::from_millis(2500);
        sim.link.on_ack(sim.now, 999_000);
        sim.ctl.on_ack(sim.now, &mut sim.link);
        assert!(
            sim.ctl.target() >= before,
            "one stall cut the target from {before} to {}",
            sim.ctl.target()
        );
    }

    #[test]
    fn sustained_queueing_cuts_within_two_windows() {
        let mut sim = Sim::new(1920 * 1080, 1, 30);
        sim.run(6.0, 15, 100.0, 10e6);
        let before = sim.ctl.target();
        // Every ack now takes 100 ms base + 600 ms queue, over several
        // evaluation windows, while frames wait on the cap.
        for id in (999_000..).take(20) {
            sim.ctl.note_blocked_at_cap();
            sim.link.on_sent(sim.now, id, 20_000, false);
            sim.now += Duration::from_millis(700);
            sim.link.on_ack(sim.now, id);
            sim.ctl.on_ack(sim.now, &mut sim.link);
        }
        assert!(
            sim.ctl.target() < before,
            "sustained queueing did not cut ({before})"
        );
    }

    #[test]
    fn slow_start_reaches_a_lan_ceiling_quickly() {
        let mut sim = Sim::new(1920 * 1080, 2, 60);
        sim.run(1.0, 30, 5.0, 100e6);
        let target = sim.ctl.target();
        assert!(
            target >= sim.ctl.effective_max() / 2,
            "after 1 s on a lan the target is only {target}"
        );
    }

    #[test]
    fn an_idle_start_preserves_slow_start_until_capacity_is_known() {
        let pixels = 2560 * 1440;
        let mut active = Sim::new(pixels, 2, 60);
        active.run(1.0, 30, 70.0, 40e6);
        assert_eq!(
            Ladder::select(
                active.link.rate_max_bps(active.now).unwrap(),
                pixels,
                true,
                true,
                60,
                1.0,
            ),
            0
        );

        let mut sim = Sim::new(pixels, 2, 60);
        sim.run(0.001, 30, 70.0, 40e6);
        sim.run(12.0, 0, 70.0, 40e6);
        assert_eq!(sim.ctl.target(), INITIAL_BPS);
        assert!(sim.ctl.is_slow_start());

        sim.run(2.0, 30, 70.0, 40e6);
        assert!(
            sim.ctl.target() >= 15_000_000,
            "target {}",
            sim.ctl.target()
        );
        assert_eq!(sim.ladder.index(), 0);
        assert!(sim.level_changes.iter().all(|(_, level)| *level < 2));
    }

    #[test]
    fn a_slow_link_still_moves_down_during_startup() {
        let mut sim = Sim::new(2560 * 1440, 2, 60);
        sim.run(8.0, 30, 70.0, 2.5e6);
        assert!(sim.ladder.index() >= 2, "level {}", sim.ladder.index());
        assert!(sim.ctl.target() < 4_000_000, "target {}", sim.ctl.target());
    }

    #[test]
    fn a_capacity_drop_moves_the_ladder_down() {
        let mut sim = Sim::new(2560 * 1440, 2, 60);
        sim.run(6.0, 30, 70.0, 40e6);
        assert_eq!(sim.ladder.index(), 0);
        sim.run(8.0, 30, 70.0, 2.5e6);
        assert!(sim.ladder.index() >= 2, "level {}", sim.ladder.index());
        assert!(sim.ctl.target() < 4_000_000, "target {}", sim.ctl.target());
    }

    #[test]
    fn budget_tracks_a_slow_link() {
        let mut sim = Sim::new(2560 * 1440, 1, 15);
        sim.run(30.0, 15, 1100.0, 2.5e6);
        let target = sim.ctl.target() as f64;
        assert!(
            target > 0.6 * 2.5e6 && target < 1.1 * 2.5e6,
            "target {target} does not track a 2.5 Mbit/s link"
        );
    }

    #[test]
    fn a_collapse_recovers_in_a_few_cuts() {
        let mut sim = Sim::new(1920 * 1080, 2, 60);
        sim.run(4.0, 30, 20.0, 50e6);
        assert!(sim.ctl.target() > 10_000_000);
        // The link falls to 2 Mbit/s.
        sim.run(8.0, 30, 20.0, 2e6);
        let target = sim.ctl.target() as f64;
        assert!(
            target < 2.0 * 2e6,
            "target {target} still far above a collapsed 2 Mbit/s link"
        );
    }

    #[test]
    fn queue_threshold_stays_crossable() {
        let mut link = LinkEstimator::new();
        let now = Instant::now();
        link.seed_rtt(now, 1100.0);
        // First sample sets mdev to rtt/2; the threshold must stay below
        // the point where no queueing can ever cross it.
        let t = link.queue_threshold_ms(now);
        assert!(t <= 2200.0, "threshold {t} uncrossable at 1.1 s rtt");
        assert!(t >= 50.0);
    }

    /// A link with steady real traffic at `bps`, for ladder tests.
    fn link_at(bps: f64) -> (LinkEstimator, Instant) {
        let mut link = LinkEstimator::new();
        let mut now = Instant::now();
        let bytes = (bps / 8.0 / 20.0) as usize;
        for id in 0..20 {
            link.on_sent(now, id, bytes, false);
            now += Duration::from_millis(50);
            link.on_ack(now, id);
        }
        (link, now)
    }

    #[test]
    fn ladder_selects_by_rate() {
        let cases = [
            // (rate, pixels, dual, expected level)
            (50e6, 1920 * 1080, true, 0),  // lan: full quality
            (20e6, 1920 * 1080, true, 0),  // dsl still affords 60 fps dual
            (2.5e6, 2560 * 1440, true, 3), // satellite: 15 fps single, full res
            (0.3e6, 2560 * 1440, true, 6), // worse than the ladder: last level
        ];
        for (rate, pixels, dual, expected) in cases {
            let got = Ladder::select(rate, pixels, dual, true, 60, 1.0);
            assert_eq!(got, expected, "rate {rate} pixels {pixels}");
        }
    }

    #[test]
    fn a_dual_only_client_pays_for_two_streams_on_every_rung() {
        // The client decodes only Dual420: rungs cannot drop the aux
        // stream, so the satellite rate must buy a deeper rung (lower fps
        // or resolution) instead of an undecodable Single420 one.
        let sel = Ladder::select(2.5e6, 2560 * 1440, true, false, 60, 1.0);
        assert_eq!(sel, 5, "expected the half-resolution rung, got {sel}");
    }

    #[test]
    fn startup_downshift_waits_for_congestion() {
        let (link, now) = link_at(2.5e6);
        let mut ladder = Ladder::new(1);
        let moved = ladder.consider(now, &link, true, false, 2560 * 1440, true, true, 60);
        assert_eq!(moved, None);
        assert_eq!(
            ladder.consider(now, &link, true, true, 2560 * 1440, true, true, 60),
            Some(3)
        );
        let (link, now) = link_at(50e6);
        let mut ladder = Ladder::new(1);
        assert_eq!(
            ladder.consider(now, &link, true, false, 1920 * 1080, true, true, 60),
            Some(0)
        );
    }

    #[test]
    fn a_flap_doubles_the_up_hold() {
        let pixels = 2560 * 1440;
        let (link, mut now) = link_at(2.5e6);
        let mut ladder = Ladder::new(3);
        // Probe up succeeds after the hold on a briefly-better link...
        let (good, good_now) = link_at(8e6);
        now = now.max(good_now) + HOLD_UP;
        let up = ladder.consider(now, &good, false, false, pixels, true, true, 60);
        assert!(up.is_some_and(|i| i < 3), "no step up: {up:?}");
        // ...then the link is what it was; after HOLD_DOWN it steps back.
        now += HOLD_DOWN;
        let down = ladder.consider(now, &link, false, false, pixels, true, true, 60);
        // The estimator window still holds the 8 Mbit max, so the down-step
        // may need the max filter to age out; drive fresh slow samples.
        let mut link2 = LinkEstimator::new();
        let mut t = now;
        let bytes = (2.5e6 / 8.0 / 20.0) as usize;
        for id in 100..140 {
            link2.on_sent(t, id, bytes, false);
            t += Duration::from_millis(400);
            link2.on_ack(t, id);
            if ladder.index() >= 3 {
                break;
            }
            ladder.consider(t, &link2, false, false, pixels, true, true, 60);
        }
        let _ = down;
        assert!(ladder.index() >= 3, "did not step back down");
        assert!(ladder.hold_up > HOLD_UP, "hold_up did not double on a flap");
    }

    #[test]
    fn a_tiny_fixed_cap_never_panics() {
        // --bitrate below the computed floor, at construction and across a
        // reconfigure to a large stream (where the floor rises).
        let mut ctl = RateController::new(640 * 360, 1, 30, Some(50_000));
        assert!(ctl.target() > 0);
        ctl.reconfigure(3840 * 2160, 2, 60);
        assert!(ctl.target() <= 50_000.max(ctl.min));
        let mut link = LinkEstimator::new();
        ctl.on_ack(Instant::now(), &mut link);
    }

    #[test]
    fn no_track_jump_between_overuse_windows() {
        // Establish a high max, then collapse the link: while overuse
        // windows accumulate, the target must never jump back up toward the
        // stale max between the cuts.
        let mut sim = Sim::new(1920 * 1080, 2, 60);
        sim.run(4.0, 30, 20.0, 50e6);
        let high = sim.ctl.target();
        assert!(high > 10_000_000);
        let mut peak_after_first_cut = 0u32;
        let mut cut_seen = false;
        for id in (500_000..).take(40) {
            sim.ctl.note_blocked_at_cap();
            sim.link.on_sent(sim.now, id, 30_000, false);
            sim.now += Duration::from_millis(700);
            sim.link.on_ack(sim.now, id);
            let changed = sim.ctl.on_ack(sim.now, &mut sim.link);
            if matches!(changed, Some(Reason::Rtt | Reason::Rate)) {
                cut_seen = true;
            }
            if cut_seen {
                peak_after_first_cut = peak_after_first_cut.max(sim.ctl.target());
            }
        }
        assert!(cut_seen);
        assert!(
            peak_after_first_cut < high / 2,
            "target climbed back to {peak_after_first_cut} against a collapsed link"
        );
    }

    #[test]
    fn growth_compounds_per_evaluation() {
        // After a cut on a link with headroom, the target must recover at
        // roughly 8%/s, not one step per hold.
        let mut sim = Sim::new(1920 * 1080, 2, 60);
        sim.run(8.0, 30, 20.0, 3e6);
        let low = sim.ctl.target() as f64;
        assert!(low < 6e6, "collapse phase left target at {low}");
        // The link opens up: 10 s of quiet must recover a large share of it.
        sim.run(10.0, 30, 20.0, 100e6);
        let recovered = sim.ctl.target() as f64;
        assert!(
            recovered > low * 1.5,
            "recovered only {low} -> {recovered} in 10 s of quiet"
        );
    }

    #[test]
    fn reconfigure_keeps_the_rate() {
        let mut ctl = RateController::new(1920 * 1080, 2, 60, None);
        let before = ctl.target();
        ctl.reconfigure(1280 * 720, 1, 15);
        assert_eq!(ctl.target(), before.clamp(ctl.min, ctl.effective_max()));
    }
}
