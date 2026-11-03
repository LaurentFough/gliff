//! Sound from a Mac host: what the Mac plays, captured by ScreenCaptureKit
//! in the capture helper (`native/Gliff.swift`, `Capture.sendAudio`) and
//! coded here as Opus, on the Linux server's wire (upstream PR #80).
//!
//! The helper sends [`crate::native::AUDIO`] packets: interleaved 48 kHz
//! stereo f32 little-endian samples, in whatever chunk ScreenCaptureKit
//! delivers. [`HostAudio`] regroups them into 10 ms Opus packets and numbers
//! them, as `gliff-server/src/audio.rs` does for PipeWire.
//!
//! Capture runs only while a client asked for sound (`ClientMsg::Audio`):
//! the session tells the helper `{"type":"audio","enabled":…}`, which turns
//! `SCStreamConfiguration.capturesAudio` on or off.
//!
//! Version: 1.0.0

use anyhow::{ensure, Result};
use gliff_proto::{AudioCodec, ServerMsg};

/// Largest PCM packet accepted from the helper: one second of stereo f32.
//= ScreenCaptureKit delivers ~10–20 ms per buffer; this only bounds a bad one.
pub const MAX_PCM_PACKET: usize = gliff_audio::SAMPLE_RATE as usize * gliff_audio::CHANNELS * 4;

/// One sound stream, from `AudioStart` until the client turns sound off.
pub struct HostAudio {
    encoder: gliff_audio::Encoder,
    /// The next `AudioFrame` number; wraps like the Linux server's.
    seq: u32,
    /// Bytes of a sample split across two helper packets.
    carry: Vec<u8>,
    pcm: Vec<f32>,
}

impl HostAudio {
    /// A new stream and the `AudioStart` that announces it.
    pub fn start() -> Result<(Self, ServerMsg)> {
        let audio = Self {
            encoder: gliff_audio::Encoder::new()?,
            seq: 0,
            carry: Vec::new(),
            pcm: Vec::new(),
        };
        let start = ServerMsg::AudioStart {
            codec: AudioCodec::Opus,
            sample_rate: gliff_audio::SAMPLE_RATE,
            channels: gliff_audio::CHANNELS as u32,
        };
        Ok((audio, start))
    }

    /// One helper packet in; the `AudioFrame`s (header, payload) it completes out.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<(ServerMsg, Vec<u8>)>> {
        ensure!(
            bytes.len() <= MAX_PCM_PACKET,
            "host sound packet of {} bytes exceeds the limit",
            bytes.len()
        );
        self.carry.extend_from_slice(bytes);
        let whole = self.carry.len() / 4 * 4;
        self.pcm.clear();
        self.pcm.extend(
            self.carry[..whole]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                //= A NaN or infinity would poison the encoder's state.
                .map(|s| {
                    if s.is_finite() {
                        s.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    }
                }),
        );
        self.carry.drain(..whole);
        let coded = self.encoder.push(&self.pcm)?;
        Ok(coded
            .into_iter()
            .map(|packet| {
                let msg = ServerMsg::AudioFrame {
                    seq: self.seq,
                    data_len: packet.len() as u32,
                };
                self.seq = self.seq.wrapping_add(1);
                (msg, packet)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: usize) -> Vec<u8> {
        (0..samples)
            .flat_map(|i| ((i as f32 * 0.01).sin() * 0.5).to_le_bytes())
            .collect()
    }

    #[test]
    fn ten_milliseconds_make_one_numbered_packet() {
        let (mut audio, start) = HostAudio::start().unwrap();
        assert!(matches!(
            start,
            ServerMsg::AudioStart {
                sample_rate: 48_000,
                channels: 2,
                ..
            }
        ));
        let ten_ms = gliff_audio::FRAME * gliff_audio::CHANNELS;
        //= Split mid-sample: the carry joins the halves.
        let bytes = pcm(ten_ms * 2);
        let (a, b) = bytes.split_at(ten_ms * 4 + 3);
        let first = audio.push(a).unwrap();
        let second = audio.push(b).unwrap();
        let seqs: Vec<u32> = first
            .iter()
            .chain(&second)
            .map(|(m, _)| match m {
                ServerMsg::AudioFrame { seq, .. } => *seq,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(seqs, [0, 1]);
        for (msg, payload) in first.iter().chain(&second) {
            let ServerMsg::AudioFrame { data_len, .. } = msg else {
                unreachable!()
            };
            assert_eq!(*data_len as usize, payload.len());
            assert!(!payload.is_empty() && payload.len() < crate::sound::MAX_AUDIO_PACKET as usize);
        }
    }

    #[test]
    fn bad_samples_are_silenced_and_huge_packets_refused() {
        let (mut audio, _) = HostAudio::start().unwrap();
        let ten_ms = gliff_audio::FRAME * gliff_audio::CHANNELS;
        let bad: Vec<u8> = (0..ten_ms)
            .flat_map(|i| if i % 2 == 0 { f32::NAN } else { f32::INFINITY }.to_le_bytes())
            .collect();
        assert_eq!(audio.push(&bad).unwrap().len(), 1);
        assert!(audio.push(&vec![0; MAX_PCM_PACKET + 4]).is_err());
    }
}
