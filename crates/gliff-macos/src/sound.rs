//! Sound in a Mac viewer session: what to ask the server for, and playing
//! what it sends.
//!
//! The wire is the Linux client's (upstream PR #80): the client offers
//! `FEATURE_AUDIO` in `Hello`; a server that also lists it in `HelloAck`
//! starts sending once the client says `ClientMsg::Audio { enabled: true }`,
//! with an `AudioStart` and then one 10 ms Opus packet per `AudioFrame`.
//! Decoding, the jitter buffer and the output device are `gliff-audio`'s
//! [`Sink`], which plays through cpal (CoreAudio on macOS) and opens the
//! device only while sound is on.
//!
//! The switch is the viewer's Machine ▸ Play Sound item, saved as the
//! top-level `muted` key the Linux client's header button writes, so it
//! applies to every machine on both clients. `--no-audio` starts a run
//! muted without saving that.

use gliff_audio::Sink;
use gliff_proto::{AudioCodec, ClientMsg, FEATURE_AUDIO};
use serde_json::Value;

/// Largest `AudioFrame` payload accepted, as on Linux: a 10 ms Opus packet
/// at 128 kbit/s is about 160 bytes, so anything near this is not sound.
pub const MAX_AUDIO_PACKET: u32 = 4000;

/// One session's sound: whether the server offers it, whether this viewer
/// wants it, and the player.
pub struct Sound {
    /// The server listed `FEATURE_AUDIO` in its `HelloAck`.
    offered: bool,
    /// The Machine menu's switch for this session.
    wanted: bool,
    /// What the server was last told (`ClientMsg::Audio`).
    on: bool,
    sink: Sink,
}

impl Sound {
    pub fn new(wanted: bool) -> Self {
        Self {
            offered: false,
            wanted,
            on: false,
            sink: Sink::default(),
        }
    }

    /// The server's `HelloAck` features. Returns the request to send, when
    /// sound is wanted and offered.
    pub fn server_features(&mut self, features: &[String]) -> Option<ClientMsg> {
        self.offered = features.iter().any(|f| f == FEATURE_AUDIO);
        tracing::info!(offered = self.offered, wanted = self.wanted, "server sound");
        self.sync()
    }

    /// The switch moved. Returns the request to send, if the server's state
    /// has to change.
    pub fn want(&mut self, wanted: bool) -> Option<ClientMsg> {
        self.wanted = wanted;
        self.sync()
    }

    /// The server (re)started its sound stream.
    pub fn start(&mut self, codec: AudioCodec, sample_rate: u32, channels: u32) {
        if self.on {
            self.sink
                .start(codec == AudioCodec::Opus, sample_rate, channels);
        }
    }

    /// One packet. Packets still in flight after the switch went off are
    /// dropped.
    pub fn frame(&mut self, seq: u32, packet: &[u8]) {
        if self.on {
            self.sink.frame(seq, packet);
        }
    }

    /// Add the Machine menu's sound state to a `machine_state` command:
    /// `sound` (the switch) and `sound_available` (this server sends sound).
    pub fn annotate(&self, state: &mut Value) {
        if let Some(map) = state.as_object_mut() {
            map.insert("sound".into(), self.wanted.into());
            map.insert("sound_available".into(), self.offered.into());
        }
    }

    fn sync(&mut self) -> Option<ClientMsg> {
        let on = self.offered && self.wanted;
        if on == self.on {
            return None;
        }
        self.on = on;
        tracing::info!(on, "sound");
        if !on {
            //= Release the output device now, not when the server stops.
            self.sink.stop();
        }
        Some(ClientMsg::Audio { enabled: on })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio() -> Vec<String> {
        vec![FEATURE_AUDIO.to_string()]
    }

    #[test]
    fn asks_only_a_server_that_offers_sound() {
        let mut s = Sound::new(true);
        assert_eq!(s.server_features(&[]), None);
        let mut s = Sound::new(true);
        assert_eq!(
            s.server_features(&audio()),
            Some(ClientMsg::Audio { enabled: true })
        );
    }

    #[test]
    fn a_muted_viewer_asks_nothing_until_unmuted() {
        let mut s = Sound::new(false);
        assert_eq!(s.server_features(&audio()), None);
        assert_eq!(s.want(true), Some(ClientMsg::Audio { enabled: true }));
        assert_eq!(s.want(true), None, "no repeat for the same state");
        assert_eq!(s.want(false), Some(ClientMsg::Audio { enabled: false }));
    }

    #[test]
    fn the_switch_before_hello_is_kept_for_it() {
        let mut s = Sound::new(true);
        assert_eq!(
            s.want(false),
            None,
            "nothing to tell a server not yet heard"
        );
        assert_eq!(s.want(true), None);
        assert_eq!(
            s.server_features(&audio()),
            Some(ClientMsg::Audio { enabled: true })
        );
    }

    #[test]
    fn the_menu_state_reports_switch_and_availability() {
        let mut s = Sound::new(true);
        let mut state = serde_json::json!({"type": "machine_state"});
        s.annotate(&mut state);
        assert_eq!(state["sound"], true);
        assert_eq!(state["sound_available"], false);
        s.server_features(&audio());
        s.annotate(&mut state);
        assert_eq!(state["sound_available"], true);
    }

    #[test]
    fn packets_while_off_touch_no_device() {
        //= Sink::frame without a start never opens a device; this must not
        //= panic or block on a machine with no audio hardware (CI).
        let mut s = Sound::new(false);
        s.server_features(&audio());
        s.start(AudioCodec::Opus, 48_000, 2);
        s.frame(0, &[0xf8, 0xff, 0xfe]);
    }
}
