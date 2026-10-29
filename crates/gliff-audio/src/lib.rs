//! Sound for gliff: hosts code what the desktop plays as Opus, clients
//! decode it and play it through a buffer that absorbs network jitter and
//! the drift between the two machines' audio clocks.
//!
//! The wire format is fixed: 48 kHz stereo, one 10 ms Opus packet per
//! `AudioFrame`. Hosts hand [`Encoder`] interleaved f32 samples at that rate
//! in any chunk size; clients feed each packet to [`Decoder`] and the samples
//! to a [`Player`].

mod codec;
mod jitter;
#[cfg(feature = "player")]
mod player;

pub use codec::{Decoder, Encoder};
pub use jitter::Jitter;
#[cfg(feature = "player")]
pub use player::{Player, Sink};

/// The stream's sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// The stream's channels, interleaved left then right.
pub const CHANNELS: usize = 2;
/// Samples per channel in one packet: 10 ms.
pub const FRAME: usize = 480;
/// Transparent for stereo music at Opus's low-delay mode.
pub const BITRATE: i32 = 128_000;
