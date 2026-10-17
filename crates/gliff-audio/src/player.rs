//! The output device: a cpal stream on the default output that pulls from
//! a [`Jitter`] buffer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::Jitter;

pub struct Player {
    jitter: Arc<Mutex<Jitter>>,
    /// Set by the device's error callback, for example when it is unplugged;
    /// the owner opens a new player on the new default.
    failed: Arc<AtomicBool>,
    _stream: cpal::Stream,
}

impl Player {
    /// Start playing silence on the default output device.
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("no audio output device")?;
        let supported = device.default_output_config()?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let mut config = supported.config();
        // About 10 ms per device buffer where the device lets us choose:
        // some backends default to far longer.
        if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
            config.buffer_size = cpal::BufferSize::Fixed((rate / 100).clamp(*min, *max));
        }
        let jitter = Arc::new(Mutex::new(Jitter::new(rate)));
        let failed = Arc::new(AtomicBool::new(false));
        let on_error = {
            let failed = failed.clone();
            move |e: cpal::Error| {
                tracing::warn!(error = %e, "audio output failed");
                failed.store(true, Ordering::Relaxed);
            }
        };
        let source = jitter.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
                config,
                move |out, _| match source.lock() {
                    Ok(mut j) => j.render(out, channels),
                    Err(_) => out.fill(0.0),
                },
                on_error,
                None,
            )?,
            cpal::SampleFormat::I16 => {
                let mut scratch = Vec::new();
                device.build_output_stream::<i16, _, _>(
                    config,
                    move |out, _| {
                        scratch.resize(out.len(), 0.0);
                        match source.lock() {
                            Ok(mut j) => j.render(&mut scratch, channels),
                            Err(_) => scratch.fill(0.0),
                        }
                        for (o, s) in out.iter_mut().zip(&scratch) {
                            *o = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                    },
                    on_error,
                    None,
                )?
            }
            other => bail!("unsupported audio output format {other}"),
        };
        stream.play()?;
        tracing::info!(rate, channels, "audio output open");
        Ok(Self {
            jitter,
            failed,
            _stream: stream,
        })
    }

    /// Queue interleaved stereo samples at the stream rate.
    pub fn push(&self, pcm: &[f32]) {
        if let Ok(mut j) = self.jitter.lock() {
            j.push(pcm);
        }
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Buffered and target delay in milliseconds, and underruns so far.
    pub fn stats(&self) -> (f64, f64, u64) {
        self.jitter
            .lock()
            .map(|j| (j.buffered_ms(), j.target_ms(), j.underruns()))
            .unwrap_or_default()
    }
}

/// What a client plays: the decoder and the player for the server's sound
/// while it is on. The device opens on the first `AudioStart` and closes on
/// `stop`, so a client with sound off holds no audio device.
#[derive(Default)]
pub struct Sink {
    decoder: Option<crate::Decoder>,
    player: Option<Player>,
    /// When the device last failed to open, so a missing one is not retried
    /// on every packet.
    failed_at: Option<std::time::Instant>,
}

/// How long a device that would not open is left before trying again.
const REOPEN_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

impl Sink {
    /// The server (re)started its sound: a new decoder, and a device if
    /// there is none. A stream in a format this build cannot play is
    /// ignored until the next start.
    pub fn start(&mut self, opus: bool, sample_rate: u32, channels: u32) {
        if !opus || sample_rate != crate::SAMPLE_RATE || channels as usize != crate::CHANNELS {
            tracing::warn!(opus, sample_rate, channels, "unsupported audio stream");
            self.decoder = None;
            return;
        }
        self.decoder = crate::Decoder::new()
            .map_err(|e| tracing::warn!(error = %e, "cannot create the audio decoder"))
            .ok();
        self.ensure_player();
    }

    /// Play one packet.
    pub fn frame(&mut self, seq: u32, packet: &[u8]) {
        if self.decoder.is_none() {
            return;
        }
        if self.player.as_ref().is_some_and(Player::failed) {
            // The device went away (unplugged, or the default changed):
            // follow the new default.
            self.player = None;
        }
        self.ensure_player();
        let Some(decoder) = &mut self.decoder else {
            return;
        };
        let pcm = match decoder.decode(seq, packet) {
            Ok(pcm) => pcm,
            Err(e) => {
                tracing::debug!(error = %e, "audio packet did not decode");
                return;
            }
        };
        if let Some(player) = &self.player {
            player.push(pcm);
        }
    }

    /// Sound off: release the device.
    pub fn stop(&mut self) {
        self.decoder = None;
        self.player = None;
    }

    /// Buffered and target delay in milliseconds and underruns, while
    /// playing.
    pub fn stats(&self) -> Option<(f64, f64, u64)> {
        self.decoder.as_ref()?;
        self.player.as_ref().map(Player::stats)
    }

    fn ensure_player(&mut self) {
        if self.player.is_some() || self.failed_at.is_some_and(|t| t.elapsed() < REOPEN_AFTER) {
            return;
        }
        match Player::open() {
            Ok(player) => {
                self.player = Some(player);
                self.failed_at = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot open the audio output");
                self.failed_at = Some(std::time::Instant::now());
            }
        }
    }
}
