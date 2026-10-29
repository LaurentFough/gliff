//! Make a test stream for `scripts/macos-video-check.py`, and time the CPU
//! decoder on it.
//!
//! Encodes `--frames` frames of a moving test pattern with gliff's own CPU
//! encoder (OpenH264; `--dual` for the AVC444 main + auxiliary pair), writes
//! each access unit as `main-NNNN.h264` / `aux-NNNN.h264`, decodes the same
//! stream with the CPU decoder the viewer falls back to, and prints one JSON
//! line: the CPU decode times, and the pattern's expected colours and the CPU
//! decoder's output at the check's probe points, for the last frame.
//!
//! The pattern: four solid stripes (red, green, blue, white) in the top half
//! check the channel order; one-pixel red/blue columns in the bottom half
//! separate 4:4:4 from 4:2:0, which blurs them to purple. It scrolls 8 px a
//! frame, so P-frames carry real motion; 8 is even, so column parity holds.
//!
//! cargo run --release -p gliff-macos --example video_fixture -- --out DIR --dual

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use gliff_sw::{Decoder, Encoder, EncoderSettings};

#[derive(Parser)]
struct Cli {
    /// Directory for the access units (created).
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 1920)]
    width: u32,
    #[arg(long, default_value_t = 1080)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    frames: u32,
    /// Encode AVC444 (Dual420): a main and an auxiliary stream.
    #[arg(long)]
    dual: bool,
    /// Bits per second per stream; default scales with the frame size.
    #[arg(long)]
    bitrate: Option<u32>,
}

const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const STRIPES: [[u8; 3]; 4] = [RED, [0, 255, 0], BLUE, [255, 255, 255]];
/// Pixels the pattern scrolls per frame (even, so column parity holds).
const SCROLL: usize = 8;

/// The pattern's RGB at (x, y) in frame `k`.
fn pixel(x: usize, y: usize, w: usize, h: usize, k: usize) -> [u8; 3] {
    let xs = x + SCROLL * k;
    if y < h / 2 {
        STRIPES[(xs * 4 / w) % 4]
    } else if xs.is_multiple_of(2) {
        RED
    } else {
        BLUE
    }
}

/// Frame `k` as packed BGRA.
fn frame(w: usize, h: usize, k: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = pixel(x, y, w, h, k);
            out.extend_from_slice(&[b, g, r, 255]);
        }
    }
    out
}

/// Where the check reads pixels back; the viewer's `GLIFF_VIDEO_PROBE`
/// samples the same points (see `GPUVideo.probePoints` in Gliff.swift):
/// the four stripe centres, then an even and an odd column of the
/// alternating half.
fn probe_points(w: usize, h: usize) -> [(usize, usize); 6] {
    let x0 = (w / 2) & !1;
    [
        (w / 8, h / 4),
        (3 * w / 8, h / 4),
        (5 * w / 8, h / 4),
        (7 * w / 8, h / 4),
        (x0, 3 * h / 4),
        (x0 + 1, 3 * h / 4),
    ]
}

/// RGB at the probe points of a packed BGRA image.
fn sample(bgra: &[u8], w: usize, h: usize) -> Vec<[u8; 3]> {
    probe_points(w, h)
        .iter()
        .map(|&(x, y)| {
            let i = (y * w + x) * 4;
            [bgra[i + 2], bgra[i + 1], bgra[i]]
        })
        .collect()
}

/// Average and 95th-percentile of times in milliseconds.
fn stats(times: &mut [f64]) -> (f64, f64) {
    if times.is_empty() {
        return (0.0, 0.0);
    }
    times.sort_by(f64::total_cmp);
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let p95 = times[((times.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)];
    (avg, p95)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    ensure!(
        cli.width >= 64
            && cli.height >= 64
            && cli.width.is_multiple_of(2)
            && cli.height.is_multiple_of(2),
        "the size must be even and at least 64x64"
    );
    ensure!(
        (1..=1000).contains(&cli.frames),
        "--frames must be 1 to 1000"
    );
    let (w, h) = (cli.width as usize, cli.height as usize);
    std::fs::create_dir_all(&cli.out).with_context(|| format!("create {}", cli.out.display()))?;
    //= Generous by default: the check compares colours, so blocking
    //= artefacts would only add noise.
    let bitrate = cli
        .bitrate
        .unwrap_or_else(|| (cli.width * cli.height).saturating_mul(20).max(8_000_000));
    let mut encoder = Encoder::new(
        EncoderSettings {
            width: cli.width,
            height: cli.height,
            bitrate,
            framerate: 60,
        },
        cli.dual,
    )?;
    let mut decoder = Decoder::new(cli.dual, cli.width, cli.height)?;
    let mut times = Vec::with_capacity(cli.frames as usize);
    let mut last = None;
    for k in 0..cli.frames as usize {
        let coded = encoder.encode_bgra(&frame(w, h, k), k == 0)?;
        std::fs::write(cli.out.join(format!("main-{k:04}.h264")), &coded.main)?;
        let aux = coded.aux.unwrap_or_default();
        if cli.dual {
            ensure!(!aux.is_empty(), "the encoder gave no auxiliary stream");
            std::fs::write(cli.out.join(format!("aux-{k:04}.h264")), &aux)?;
        }
        let t0 = Instant::now();
        let picture = decoder.decode(&coded.main, &aux)?;
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
        if picture.is_some() {
            last = picture;
        }
    }
    //= A High-profile stream holds the newest picture until the next unit.
    if let Some(picture) = decoder.flush()? {
        last = Some(picture);
    }
    let last = last.context("the CPU decoder gave no picture")?;
    ensure!(
        (last.width, last.height) == (cli.width, cli.height),
        "decoded {}x{}, expected {}x{}",
        last.width,
        last.height,
        cli.width,
        cli.height
    );
    let (avg, p95) = stats(&mut times);
    let summary = serde_json::json!({
        "width": cli.width,
        "height": cli.height,
        "dual": cli.dual,
        "frames": cli.frames,
        "bitrate": bitrate,
        "cpu_decode_ms": { "avg": avg, "p95": p95 },
        "expected": sample(&frame(w, h, cli.frames as usize - 1), w, h),
        "cpu": sample(&last.pixels, w, h),
    });
    println!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pattern_keeps_column_parity_while_it_scrolls() {
        let (w, h) = (64, 64);
        for k in 0..4 {
            assert_eq!(pixel(32, 48, w, h, k), RED);
            assert_eq!(pixel(33, 48, w, h, k), BLUE);
        }
        assert_eq!(pixel(w / 8, 0, w, h, 0), RED);
        assert_eq!(pixel(7 * w / 8, 0, w, h, 0), [255, 255, 255]);
    }

    #[test]
    fn probes_read_rgb_from_bgra() {
        let (w, h) = (64, 64);
        let s = sample(&frame(w, h, 0), w, h);
        assert_eq!(s, vec![RED, [0, 255, 0], BLUE, [255, 255, 255], RED, BLUE]);
    }

    #[test]
    fn stats_are_avg_and_p95() {
        let mut t: Vec<f64> = (1..=20).map(f64::from).collect();
        let (avg, p95) = stats(&mut t);
        assert!((avg - 10.5).abs() < 1e-9);
        assert_eq!(p95, 19.0);
    }
}
