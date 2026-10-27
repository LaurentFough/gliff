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
                if e.kind() == cpal::ErrorKind::Xrun {
                    tracing::debug!(error = %e, "audio output xrun");
                } else {
                    tracing::warn!(error = %e, "audio output failed");
                }
                note_output_error(&failed, &e);
            }
        };
        let format = supported.sample_format();
        let build = |config: cpal::StreamConfig| {
            Self::build_stream(
                &device,
                format,
                config,
                jitter.clone(),
                channels,
                on_error.clone(),
            )
        };
        // A device can report a buffer range it then refuses (ALSA under
        // QEMU's HDA reports 1024..=1024 only at build time), so a size of
        // our choosing falls back to the device's own.
        let stream = match build(config) {
            Ok(stream) => stream,
            Err(e) if config.buffer_size != cpal::BufferSize::Default => {
                tracing::warn!(error = %e, "audio output refused our buffer size; using the device default");
                config.buffer_size = cpal::BufferSize::Default;
                build(config)?
            }
            Err(e) => return Err(e),
        };
        stream.play()?;
        tracing::info!(rate, channels, "audio output open");
        Ok(Self {
            jitter,
            failed,
            _stream: stream,
        })
    }

    fn build_stream(
        device: &cpal::Device,
        format: cpal::SampleFormat,
        config: cpal::StreamConfig,
        source: Arc<Mutex<Jitter>>,
        channels: usize,
        on_error: impl FnMut(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream> {
        Ok(match format {
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
            cpal::SampleFormat::I32 => {
                let mut scratch = Vec::new();
                device.build_output_stream::<i32, _, _>(
                    config,
                    move |out, _| {
                        scratch.resize(out.len(), 0.0);
                        match source.lock() {
                            Ok(mut j) => j.render(&mut scratch, channels),
                            Err(_) => scratch.fill(0.0),
                        }
                        for (o, s) in out.iter_mut().zip(&scratch) {
                            *o = (s.clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
                        }
                    },
                    on_error,
                    None,
                )?
            }
            other => bail!("unsupported audio output format {other}"),
        })
    }

    fn reset(&self) {
        if let Ok(mut jitter) = self.jitter.lock() {
            jitter.restart();
        }
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
    opening: Option<Opening<Player>>,
    open_lock: Arc<Mutex<()>>,
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
            self.stop();
            return;
        }
        self.decoder = crate::Decoder::new()
            .map_err(|e| tracing::warn!(error = %e, "cannot create the audio decoder"))
            .ok();
        if let Some(player) = &self.player {
            player.reset();
        }
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
            self.failed_at = Some(std::time::Instant::now());
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
        // Dropping this receiver makes an opening device close on its
        // helper thread when it eventually returns.
        self.opening = None;
    }

    /// Buffered and target delay in milliseconds and underruns, while
    /// playing.
    pub fn stats(&self) -> Option<(f64, f64, u64)> {
        self.decoder.as_ref()?;
        self.player.as_ref().map(Player::stats)
    }

    fn ensure_player(&mut self) {
        if let Some(opening) = &self.opening {
            match opening.try_recv() {
                Ok(Ok(player)) => {
                    self.player = Some(player);
                    self.failed_at = None;
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "cannot open the audio output");
                    self.failed_at = Some(std::time::Instant::now());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.failed_at = Some(std::time::Instant::now());
                }
            }
            self.opening = None;
        }
        if self.player.is_some() || self.failed_at.is_some_and(|t| t.elapsed() < REOPEN_AFTER) {
            return;
        }
        match open_in_background(Player::open, self.open_lock.clone()) {
            Ok(opening) => self.opening = Some(opening),
            Err(e) => {
                tracing::warn!(error = %e, "cannot start the audio output worker");
                self.failed_at = Some(std::time::Instant::now());
            }
        }
    }
}

fn note_output_error(failed: &AtomicBool, error: &cpal::Error) {
    // CPAL reports recoverable xruns too. They do not mean the device is
    // gone and must not tear down the stream and its jitter buffer.
    if error.kind() != cpal::ErrorKind::Xrun {
        failed.store(true, Ordering::Relaxed);
    }
}

struct Opening<T> {
    receiver: std::sync::mpsc::Receiver<Result<T>>,
    cancelled: Arc<AtomicBool>,
}

impl<T> Opening<T> {
    fn try_recv(&self) -> std::result::Result<Result<T>, std::sync::mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl<T> Drop for Opening<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

fn open_in_background<T: Send + 'static>(
    open: impl FnOnce() -> Result<T> + Send + 'static,
    gate: Arc<Mutex<()>>,
) -> Result<Opening<T>> {
    let (tx, receiver) = std::sync::mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    std::thread::Builder::new()
        .name("gliff-audio-output".into())
        .spawn(move || {
            // An uncancellable native open may still be running after mute.
            // Serialize its completion and cleanup before the next open, and
            // skip obsolete requests that were cancelled while queued here.
            let _guard = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if worker_cancelled.load(Ordering::Acquire) {
                return;
            }
            let result = open();
            // On cancellation, send fails and drops any Player while the gate
            // is still held, so an exclusive ALSA device is closed first.
            let _ = tx.send(result);
        })?;
    Ok(Opening {
        receiver,
        cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_xrun_does_not_reopen_the_output_but_a_lost_device_does() {
        let failed = AtomicBool::new(false);
        note_output_error(&failed, &cpal::Error::new(cpal::ErrorKind::Xrun));
        assert!(!failed.load(Ordering::Relaxed));
        note_output_error(
            &failed,
            &cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable),
        );
        assert!(failed.load(Ordering::Relaxed));
    }

    #[test]
    fn a_slow_device_open_does_not_block_the_caller() {
        let (release, wait) = std::sync::mpsc::channel();
        let result = open_in_background(
            move || {
                wait.recv().unwrap();
                Ok(42)
            },
            Arc::new(Mutex::new(())),
        )
        .unwrap();
        assert!(matches!(
            result.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        assert_eq!(
            result
                .receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            42
        );
    }
}

#[cfg(test)]
mod open_cancellation_tests {
    use super::*;

    #[test]
    fn cancelled_opens_release_exclusive_devices_before_the_next_attempt() {
        struct Device(Arc<AtomicBool>);
        impl Drop for Device {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let gate = Arc::new(Mutex::new(()));
        let closed = Arc::new(AtomicBool::new(false));
        let (started, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let first_closed = closed.clone();
        let first = open_in_background(
            move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
                Ok(Device(first_closed))
            },
            gate.clone(),
        )
        .unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        drop(first);
        let second = open_in_background(
            move || {
                assert!(
                    closed.load(Ordering::Acquire),
                    "previous exclusive device is still open"
                );
                Ok(42)
            },
            gate,
        )
        .unwrap();
        release.send(()).unwrap();
        assert_eq!(
            second
                .receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            42
        );
    }
}
