//! The playout buffer between the network and the output device.
//!
//! Packets arrive in bursts (they share a TCP stream with video), and the
//! host's audio clock never runs at exactly the device's rate, so the
//! buffer is read through a resampler whose ratio leans a fraction of a
//! percent toward the target fill: drift is absorbed without dropping or
//! repeating samples, and the pitch change is far below what anyone hears.
//! The same resampler converts 48 kHz to whatever rate the device runs at.
//!
//! The target fill adapts: an underrun raises it, and a long calm stretch
//! lowers it again, so a steady link plays with little delay and a jittery
//! one stops crackling. A burst far past the target (after a stall) is cut
//! back at once rather than played late.

use std::collections::VecDeque;

use crate::{CHANNELS, SAMPLE_RATE};

/// Source frames per millisecond.
const MS: usize = SAMPLE_RATE as usize / 1000;
const START_TARGET: usize = 60 * MS;
const MIN_TARGET: usize = 40 * MS;
const MAX_TARGET: usize = 250 * MS;
/// The target grows this much on an underrun...
const TARGET_STEP: usize = 20 * MS;
/// ...and shrinks this much after each calm stretch.
const TARGET_RELAX: usize = 5 * MS;
/// Source frames played without an underrun that count as a calm stretch.
const CALM: u64 = 30 * SAMPLE_RATE as u64;
/// More than this past the target is cut back to the target.
const MAX_EXCESS: usize = 150 * MS;
/// The furthest the ratio leans from nominal: 0.5%, about 9 cents.
const MAX_ADJUST: f64 = 0.005;
/// The lean that would bring the fill back to the target in this long.
const CATCH_UP_SECS: f64 = 2.0;
/// Frames of fade-in after silence or a cut, so neither clicks.
const FADE: usize = 2 * MS;

pub struct Jitter {
    buf: VecDeque<[f32; CHANNELS]>,
    /// Read position in `buf`, in source frames. While playing it stays in
    /// [1, 2): one frame of history before it for the interpolation.
    pos: f64,
    playing: bool,
    target: usize,
    /// Source frames per output frame at nominal rate.
    base_ratio: f64,
    ratio: f64,
    smoothed: f64,
    underruns: u64,
    calm: u64,
    fade: usize,
}

impl Jitter {
    /// A buffer playing into a device running at `out_rate`.
    pub fn new(out_rate: u32) -> Self {
        let base_ratio = SAMPLE_RATE as f64 / out_rate.max(1) as f64;
        Self {
            buf: VecDeque::with_capacity(MAX_TARGET + MAX_EXCESS),
            pos: 1.0,
            playing: false,
            target: START_TARGET,
            base_ratio,
            ratio: base_ratio,
            smoothed: 0.0,
            underruns: 0,
            calm: 0,
            fade: 0,
        }
    }

    /// Source frames buffered ahead of the read position.
    fn fill(&self) -> f64 {
        self.buf.len() as f64 - self.pos
    }

    pub fn buffered_ms(&self) -> f64 {
        self.fill().max(0.0) / MS as f64
    }

    pub fn target_ms(&self) -> f64 {
        self.target as f64 / MS as f64
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Add interleaved stereo samples at the stream rate.
    pub fn push(&mut self, pcm: &[f32]) {
        self.buf
            .extend(pcm.chunks_exact(CHANNELS).map(|f| [f[0], f[1]]));
        if !self.playing {
            // Start once the target is buffered, with the interpolation's
            // history and look-ahead on either side of it.
            if self.buf.len() >= self.target + 3 {
                self.playing = true;
                self.pos = 1.0;
                self.fade = FADE;
                self.smoothed = self.fill();
            }
        } else if self.fill() > (self.target + MAX_EXCESS) as f64 {
            let cut = (self.fill() - self.target as f64) as usize;
            self.buf.drain(..cut.min(self.buf.len() - 3));
            self.fade = FADE;
            self.smoothed = self.fill();
            tracing::debug!(cut_ms = cut / MS, "audio buffer cut back to its target");
        }
    }

    /// Fill a device buffer of `channels` interleaved channels.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        for frame in out.chunks_mut(channels) {
            let [l, r] = self.next();
            match frame {
                [mono] => *mono = 0.5 * (l + r),
                [left, right, rest @ ..] => {
                    *left = l;
                    *right = r;
                    rest.fill(0.0);
                }
                [] => {}
            }
        }
        if self.playing {
            // Lean the ratio toward the target fill.
            self.smoothed += (self.fill() - self.smoothed) * 0.05;
            let error_secs = (self.smoothed - self.target as f64) / SAMPLE_RATE as f64;
            let lean = (error_secs / CATCH_UP_SECS).clamp(-MAX_ADJUST, MAX_ADJUST);
            self.ratio = self.base_ratio * (1.0 + lean);
            self.calm += (out.len() / channels) as u64;
            if self.calm as f64 * self.base_ratio >= CALM as f64 {
                self.calm = 0;
                self.target = self.target.saturating_sub(TARGET_RELAX).max(MIN_TARGET);
            }
        }
    }

    fn next(&mut self) -> [f32; CHANNELS] {
        if !self.playing {
            return [0.0; CHANNELS];
        }
        let i = self.pos as usize;
        if i + 2 >= self.buf.len() {
            // Out of samples: go silent and rebuffer to a higher target.
            self.playing = false;
            self.underruns += 1;
            self.calm = 0;
            self.target = (self.target + TARGET_STEP).min(MAX_TARGET);
            tracing::debug!(target_ms = self.target_ms(), "audio underrun");
            return [0.0; CHANNELS];
        }
        let t = (self.pos - i as f64) as f32;
        let (a, b, c, d) = (
            self.buf[i - 1],
            self.buf[i],
            self.buf[i + 1],
            self.buf[i + 2],
        );
        let mut out = [
            hermite(a[0], b[0], c[0], d[0], t),
            hermite(a[1], b[1], c[1], d[1], t),
        ];
        if self.fade > 0 {
            let gain = 1.0 - self.fade as f32 / FADE as f32;
            out = out.map(|v| v * gain);
            self.fade -= 1;
        }
        self.pos += self.ratio;
        while self.pos >= 2.0 {
            self.buf.pop_front();
            self.pos -= 1.0;
        }
        out
    }
}

/// Catmull-Rom interpolation between `y1` and `y2`.
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host producing 10 ms packets of a 1 kHz tone at `host_rate` (its
    /// clock, in stream samples per real second) into a device pulling
    /// `block` frames at a time at `out_rate`, for `secs` seconds. Returns
    /// the buffer and everything the device played.
    fn run(host_rate: f64, out_rate: u32, block: usize, secs: f64) -> (Jitter, Vec<f32>) {
        let mut j = Jitter::new(out_rate);
        let mut out = Vec::new();
        let mut produced = 0usize;
        let mut buf = vec![0.0; block * 2];
        let mut t = 0.0;
        while t < secs {
            while (produced as f64) < t * host_rate {
                let pcm: Vec<f32> = (produced..produced + 480)
                    .flat_map(|n| {
                        let v = (n as f32 * 1000.0 * std::f32::consts::TAU / 48000.0).sin();
                        [v, v]
                    })
                    .collect();
                j.push(&pcm);
                produced += 480;
            }
            j.render(&mut buf, 2);
            out.extend_from_slice(&buf);
            t += block as f64 / out_rate as f64;
        }
        (j, out)
    }

    #[test]
    fn plays_steadily_near_the_target() {
        let (j, out) = run(48000.0, 48000, 256, 20.0);
        assert_eq!(j.underruns(), 0);
        assert!(
            j.buffered_ms() < j.target_ms() + 15.0,
            "{}",
            j.buffered_ms()
        );
        assert!(out[out.len() - 1000..].iter().any(|v| v.abs() > 0.5));
    }

    #[test]
    fn absorbs_clock_drift_without_underruns_or_growth() {
        // A host clock 0.1% fast, then 0.1% slow.
        for host in [48048.0, 47952.0] {
            let (j, _) = run(host, 48000, 512, 60.0);
            assert_eq!(j.underruns(), 0, "host {host}");
            assert!(
                (j.buffered_ms() - j.target_ms()).abs() < 15.0,
                "host {host}: {} vs {}",
                j.buffered_ms(),
                j.target_ms()
            );
        }
    }

    #[test]
    fn resamples_to_the_device_rate_keeping_pitch() {
        let (j, out) = run(48000.0, 44100, 441, 4.0);
        assert_eq!(j.underruns(), 0);
        // Count rising zero crossings over the last second: 1 kHz.
        let left: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
        let last = &left[left.len() - 44100..];
        let crossings = last
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((995..=1005).contains(&crossings), "{crossings}");
    }

    #[test]
    fn an_underrun_raises_the_target_and_a_burst_is_cut_back() {
        let mut j = Jitter::new(48000);
        let start = j.target_ms();
        j.push(&vec![0.1; (START_TARGET + 10) * 2]);
        let mut out = vec![0.0; 4800 * 2];
        j.render(&mut out, 2);
        j.render(&mut out, 2);
        assert_eq!(j.underruns(), 1);
        assert!(j.target_ms() > start);
        // A one-second burst while playing.
        j.push(&vec![0.1; (j.target + 10) * 2]);
        j.render(&mut out[..64], 2);
        j.push(&vec![0.1; 48000 * 2]);
        assert!(
            j.buffered_ms() <= j.target_ms() + 1.0,
            "{}",
            j.buffered_ms()
        );
    }

    #[test]
    fn renders_mono_and_surround() {
        let mut j = Jitter::new(48000);
        j.push(&[0.5, 0.1].repeat(START_TARGET + 600));
        let mut mono = vec![0.0; 400];
        j.render(&mut mono, 1);
        assert!((mono[399] - 0.3).abs() < 1e-4);
        let mut six = vec![9.0; 600];
        j.render(&mut six, 6);
        assert_eq!(&six[594..], &[0.5, 0.1, 0.0, 0.0, 0.0, 0.0]);
    }
}
