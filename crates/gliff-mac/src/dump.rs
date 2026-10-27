//! Headless mode: connect, decode, and write the first N pictures as PNG.
//! Exercises the whole protocol and decode path without AppKit, so it runs
//! on Linux CI and over plain ssh sessions too:
//!
//! ```sh
//! gliff-mac --dump ./frames --frames 5 user@host
//! ```

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::mpsc::unbounded_channel;

use crate::session::{self, Picture, Status, UiLink, WorkerConfig};

//= Version of the dump format (file naming, PNG layout).
pub const DUMP_VERSION: &str = "0.1.0";

/// Write `count` frames into `dir`, giving up after `timeout`.
pub fn run(cfg: WorkerConfig, dir: &Path, count: u32, timeout: Duration) -> anyhow::Result<()> {
    anyhow::ensure!(count > 0, "--frames must be at least 1");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let (frames_tx, frames_rx) = mpsc::sync_channel::<Picture>(2);
    let (status_tx, status_rx) = mpsc::channel::<Status>();
    let (input_tx, input_rx) = unbounded_channel();
    let worker = session::spawn(
        WorkerConfig {
            reconnect: false,
            ..cfg
        },
        UiLink {
            frames: frames_tx,
            status: status_tx,
            input: input_rx,
        },
    )
    .context("spawn worker")?;

    let deadline = Instant::now() + timeout;
    let mut written = 0u32;
    let mut failure = None;
    while written < count {
        for s in status_rx.try_iter() {
            match s {
                Status::Connected { video, view, .. } => {
                    tracing::info!(%video, ?view, "dump: connected")
                }
                Status::Error(e) | Status::Incompatible(e) => failure = Some(e),
                _ => {}
            }
        }
        if let Some(e) = failure.take() {
            anyhow::bail!("session ended: {e}");
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            anyhow::bail!("timed out after {written} of {count} frames");
        }
        match frames_rx.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(p) => {
                let path = frame_path(dir, written);
                write_png(&path, &p).with_context(|| format!("write {}", path.display()))?;
                tracing::info!(path = %path.display(), w = p.frame.width, h = p.frame.height, "dump: wrote");
                written += 1;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let why = status_rx.try_iter().find_map(|s| match s {
                    Status::Error(e) | Status::Incompatible(e) => Some(e),
                    _ => None,
                });
                anyhow::bail!(
                    "worker stopped after {written} frames{}",
                    why.map(|w| format!(": {w}")).unwrap_or_default()
                );
            }
        }
    }
    drop(input_tx);
    let _ = worker.join();
    Ok(())
}

/// `frame-0000.png`, `frame-0001.png`, ...
pub fn frame_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("frame-{n:04}.png"))
}

/// BGRA (opaque) → RGB PNG.
fn write_png(path: &Path, p: &Picture) -> anyhow::Result<()> {
    let (w, h) = (p.frame.width, p.frame.height);
    let bgra = p.frame.pixels.as_ref();
    anyhow::ensure!(
        bgra.len() >= (w as usize) * (h as usize) * 4,
        "short frame: {} bytes for {w}x{h}",
        bgra.len()
    );
    let rgb = bgra_to_rgb(bgra, w as usize, h as usize);
    let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgb)?;
    Ok(())
}

/// Drop alpha and swap to RGB order.
pub fn bgra_to_rgb(bgra: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 3);
    for px in bgra.chunks_exact(4).take(w * h) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_bgra_to_rgb() {
        assert_eq!(
            bgra_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 0], 2, 1),
            vec![3, 2, 1, 6, 5, 4]
        );
    }

    #[test]
    fn names_frames_in_order() {
        assert_eq!(frame_path(Path::new("d"), 7), Path::new("d/frame-0007.png"));
    }
}
