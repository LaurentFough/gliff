//! Opus in the stream's one format, with packet-loss concealment for the
//! packets a host drops.

use anyhow::{ensure, Result};
use opus::{Application, Bitrate, Channels};

use crate::{BITRATE, CHANNELS, FRAME, SAMPLE_RATE};

/// The largest packet Opus produces for one frame is far below this.
const MAX_PACKET: usize = 4000;
/// A gap of at most this many packets is concealed; a longer one (the host
/// stopped sending) restarts the stream instead of filling it with noise.
const MAX_CONCEALED: u32 = 4;

pub struct Encoder {
    opus: opus::Encoder,
    /// Samples waiting for a whole frame.
    pending: Vec<f32>,
}

impl Encoder {
    pub fn new() -> Result<Self> {
        // Restricted low-delay: CELT only, the shortest look-ahead, and the
        // right mode for whatever a desktop plays.
        let mut opus = opus::Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        opus.set_bitrate(Bitrate::Bits(BITRATE))?;
        Ok(Self {
            opus,
            pending: Vec::with_capacity(FRAME * CHANNELS * 2),
        })
    }

    /// Add interleaved stereo samples; returns the packets they completed.
    pub fn push(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(pcm);
        let mut packets = Vec::new();
        let whole = self.pending.len() / (FRAME * CHANNELS) * (FRAME * CHANNELS);
        for frame in self.pending[..whole].chunks_exact(FRAME * CHANNELS) {
            packets.push(self.opus.encode_vec_float(frame, MAX_PACKET)?);
        }
        self.pending.drain(..whole);
        Ok(packets)
    }
}

pub struct Decoder {
    opus: opus::Decoder,
    next_seq: Option<u32>,
    out: Vec<f32>,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            opus: opus::Decoder::new(SAMPLE_RATE, Channels::Stereo)?,
            next_seq: None,
            out: vec![0.0; FRAME * CHANNELS * (MAX_CONCEALED as usize + 1)],
        })
    }

    /// Decode packet `seq` into interleaved stereo samples, preceded by a
    /// concealment of any packets missing before it.
    pub fn decode(&mut self, seq: u32, packet: &[u8]) -> Result<&[f32]> {
        let missing = self.next_seq.map_or(0, |next| seq.wrapping_sub(next));
        self.next_seq = Some(seq.wrapping_add(1));
        let mut at = 0;
        if (1..=MAX_CONCEALED).contains(&missing) {
            for _ in 0..missing {
                let n =
                    self.opus
                        .decode_float(&[], &mut self.out[at..at + FRAME * CHANNELS], false)?;
                at += n * CHANNELS;
            }
        } else if missing != 0 {
            self.opus.reset_state()?;
        }
        let n = self.opus.decode_float(packet, &mut self.out[at..], false)?;
        ensure!(n <= FRAME, "audio packet longer than a frame");
        Ok(&self.out[..at + n * CHANNELS])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, start: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.5;
                [v, -v]
            })
            .collect()
    }

    #[test]
    fn round_trips_a_tone_in_any_chunking() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let input = tone(FRAME * 20, 0);
        let mut packets = Vec::new();
        for chunk in input.chunks(333) {
            packets.extend(enc.push(chunk).unwrap());
        }
        assert_eq!(packets.len(), 20);
        let mut out = Vec::new();
        for (seq, p) in packets.iter().enumerate() {
            out.extend_from_slice(dec.decode(seq as u32, p).unwrap());
        }
        assert_eq!(out.len(), input.len());
        // Past the codec's start-up, the tone comes back close and in phase
        // apart from the look-ahead delay.
        let energy = |s: &[f32]| s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32;
        let tail = &out[out.len() / 2..];
        let e = energy(tail);
        assert!((e - 0.125).abs() < 0.02, "energy {e}");
        assert!(tail.chunks_exact(2).all(|f| (f[0] + f[1]).abs() < 0.05));
    }

    #[test]
    fn conceals_short_gaps_and_restarts_after_long_ones() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let packets = enc.push(&tone(FRAME * 10, 0)).unwrap();
        assert_eq!(dec.decode(0, &packets[0]).unwrap().len(), FRAME * CHANNELS);
        // Packets 1 and 2 never arrived: they are concealed.
        assert_eq!(
            dec.decode(3, &packets[3]).unwrap().len(),
            FRAME * CHANNELS * 3
        );
        // A long silence from the host is not filled.
        assert_eq!(dec.decode(40, &packets[4]).unwrap().len(), FRAME * CHANNELS);
        // The counter wraps.
        dec.decode(u32::MAX, &packets[5]).unwrap();
        assert_eq!(dec.decode(0, &packets[6]).unwrap().len(), FRAME * CHANNELS);
    }
}
