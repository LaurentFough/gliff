//! Client and server messages, encoded as CBOR; see [`crate::frame`] for
//! how they travel. Every variant and field is numbered with `#[n]`, and a
//! message goes on the wire as `[variant, [field 0, field 1, ...]]`, so the
//! numbers are positions and names never appear.
//!
//! Peers from different builds stay compatible when changes follow these
//! rules:
//! - Never renumber or reuse a variant or field number.
//! - A new field takes the next number and is an `Option`, so an older peer
//!   ignores it and a peer that lacks it decodes `None`. An optional enum
//!   field decodes with [`crate::cbor::optional`].
//! - A required field that has shipped must be sent forever: removing it, or
//!   making it optional and sending `None`, breaks older peers.
//! - A new message, or a new value of an existing enum, goes only to a peer
//!   that listed the feature introducing it in the handshake (see
//!   [`FEATURES`]). A peer skips a message it does not know, but an unknown
//!   enum value inside a known message fails that message.
//! - `Hello` arrives before any feature is agreed, so its enum lists drop
//!   values this build does not know.
//! - The frame header, the head of `Hello` (`[0, [version, ...]]`) and the
//!   first two fields of `ServerMsg::Error` never change, so a version
//!   mismatch can always be reported; see [`Greeting`]. `Error` may gain
//!   more optional fields.
//!
//! A breaking change that these rules cannot express bumps
//! [`PROTOCOL_VERSION`].

use minicbor::decode::{Decoder, Error};
use minicbor::{Decode, Encode};

pub use crate::clipboard::{ClipboardFile, ClipboardItem, ClipboardMsg};

pub const PROTOCOL_VERSION: u16 = 1;

/// Optional behaviours this build supports, exchanged in the handshake. A
/// behaviour is used only when both ends list it.
pub const FEATURES: &[&str] = &[FEATURE_AUDIO];

/// Sound: the client turns it on with `ClientMsg::Audio`, and the server then
/// sends `AudioStart` and a stream of `AudioFrame`s while it has sound to
/// capture.
pub const FEATURE_AUDIO: &str = "audio";

pub fn features() -> Vec<String> {
    FEATURES.iter().map(|f| f.to_string()).collect()
}

/// `ServerMsg::Error` codes. Codes 1 to 255 mean the two ends cannot work
/// together, so reconnecting would fail the same way; a new code in that
/// range is fatal even to clients that predate it.
pub const ERROR_VERSION: u16 = 1;
pub const ERROR_NO_CHROMA: u16 = 2;
pub const ERROR_NO_CODEC: u16 = 3;

pub fn is_incompatible(code: u16) -> bool {
    (1..256).contains(&code)
}

/// Which end of a protocol version mismatch needs updating.
pub fn version_mismatch(client: u16, server: u16) -> &'static str {
    if client < server {
        "Client version is too old"
    } else {
        "Server version is too old"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[cbor(index_only)]
pub enum Codec {
    /// Uncompressed BGRA, phase 1 / localhost only.
    #[n(0)]
    RawBgra,
    #[n(1)]
    H264,
    #[n(2)]
    H265,
    #[n(3)]
    Av1,
}

/// Which pipeline encodes or decodes on one end, reported for the stats
/// display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[cbor(index_only)]
pub enum VideoPipeline {
    #[n(0)]
    Gpu,
    #[n(1)]
    Cpu,
}

impl VideoPipeline {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[cbor(index_only)]
pub enum ChromaMode {
    /// Single stream, encoder produced 4:4:4.
    #[n(0)]
    Native444,
    /// AVC444-style: main 4:2:0 stream plus an auxiliary 4:2:0 stream carrying
    /// the chroma the main stream dropped.
    #[n(1)]
    Dual420,
    /// Low-bandwidth fallback only.
    #[n(2)]
    Single420,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[cbor(index_only)]
pub enum AudioCodec {
    #[n(0)]
    Opus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[cbor(index_only)]
pub enum Axis {
    #[n(0)]
    Vertical,
    #[n(1)]
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct Rect {
    #[n(0)]
    pub x: i32,
    #[n(1)]
    pub y: i32,
    #[n(2)]
    pub width: i32,
    #[n(3)]
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ClientCaps {
    #[n(0)]
    #[cbor(decode_with = "crate::cbor::known")]
    pub codecs: Vec<Codec>,
    #[n(1)]
    pub max_width: u32,
    #[n(2)]
    pub max_height: u32,
    /// Chroma modes the client can decode, preferred first.
    #[n(3)]
    #[cbor(decode_with = "crate::cbor::known")]
    pub chroma: Vec<ChromaMode>,
    /// The client's [`FEATURES`].
    #[n(4)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct OutputInfo {
    #[n(0)]
    pub name: String,
    #[n(1)]
    pub width: u32,
    #[n(2)]
    pub height: u32,
    #[n(3)]
    pub scale_milli: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct SessionInfo {
    #[n(0)]
    pub headless: bool,
    #[n(1)]
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum ClientMsg {
    #[n(0)]
    Hello {
        #[n(0)]
        version: u16,
        /// XKB keymap text, or empty for the server's default. A client
        /// that cannot produce XKB text leaves it empty; a later optional
        /// field can carry layout names, which older servers ignore.
        #[n(1)]
        keymap: String,
        #[n(2)]
        caps: ClientCaps,
    },
    #[n(1)]
    Resize {
        #[n(0)]
        width: u32,
        #[n(1)]
        height: u32,
        #[n(2)]
        scale: f32,
    },
    #[n(2)]
    Key {
        #[n(0)]
        keycode: u32,
        #[n(1)]
        pressed: bool,
    },
    #[n(3)]
    PointerMotion {
        #[n(0)]
        x: f64,
        #[n(1)]
        y: f64,
    },
    #[n(4)]
    PointerButton {
        #[n(0)]
        button: u32,
        #[n(1)]
        pressed: bool,
    },
    #[n(5)]
    PointerAxis {
        #[n(0)]
        axis: Axis,
        #[n(1)]
        value: f64,
        #[n(2)]
        discrete: Option<i32>,
        #[n(3)]
        stop: bool,
    },
    #[n(6)]
    FrameAck {
        #[n(0)]
        frame_id: u64,
        #[n(1)]
        decoded_at_ms: u64,
    },
    #[n(7)]
    RequestKeyframe,
    /// The local selection changed; see [`ClipboardMsg::Offer`].
    #[n(8)]
    ClipboardOffer {
        #[n(0)]
        serial: u32,
        #[n(1)]
        mime_types: Vec<String>,
        #[n(2)]
        files: Vec<ClipboardFile>,
    },
    #[n(9)]
    ClipboardRequest {
        #[n(0)]
        id: u32,
        #[n(1)]
        serial: u32,
        #[n(2)]
        item: ClipboardItem,
    },
    /// `data_len` payload bytes follow the header.
    #[n(10)]
    ClipboardData {
        #[n(0)]
        id: u32,
        #[n(1)]
        offset: u64,
        #[n(2)]
        data_len: u32,
        #[n(3)]
        done: bool,
    },
    #[n(11)]
    Ping {
        #[n(0)]
        t: u64,
    },
    #[n(12)]
    Bye,
    #[n(13)]
    ClipboardAck {
        #[n(0)]
        id: u32,
        #[n(1)]
        received: u64,
    },
    #[n(14)]
    ClipboardAbort {
        #[n(0)]
        id: u32,
    },
    /// The local keymap changed since `Hello`; XKB text as in its `keymap`.
    #[n(15)]
    Keymap {
        #[n(0)]
        keymap: String,
    },
    /// Answer to `ServerMsg::Ping`, sent before anything else so the server
    /// can seed its round-trip estimate ahead of the first frame ack.
    #[n(16)]
    Pong {
        #[n(0)]
        t: u64,
    },
    /// Start or stop the server's sound. Sent only to a server that listed
    /// [`FEATURE_AUDIO`]; a client with several sessions turns it on for the
    /// one it shows.
    #[n(17)]
    Audio {
        #[n(0)]
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum ServerMsg {
    #[n(0)]
    HelloAck {
        #[n(0)]
        version: u16,
        #[n(1)]
        session: SessionInfo,
        #[n(2)]
        outputs: Vec<OutputInfo>,
        /// The server's [`FEATURES`].
        #[n(3)]
        features: Vec<String>,
    },
    #[n(1)]
    StreamConfig {
        #[n(0)]
        codec: Codec,
        #[n(1)]
        chroma: ChromaMode,
        /// Which pipeline encodes on the server, for the client's stats.
        #[n(2)]
        #[cbor(decode_with = "crate::cbor::optional", nil = "crate::cbor::none")]
        pipeline: Option<VideoPipeline>,
        /// Stream size in physical pixels.
        #[n(3)]
        width: u32,
        #[n(4)]
        height: u32,
        /// Effective stream scale x1000: the remote's logical size is
        /// `width / scale`, and pointer coordinates are sent in that logical
        /// space. It folds in any downscale of the stream, so it stays the
        /// number stream pixels are divided by.
        #[n(5)]
        scale_milli: u32,
        #[n(6)]
        #[cbor(with = "minicbor::bytes")]
        extradata: Vec<u8>,
        #[n(7)]
        #[cbor(with = "minicbor::bytes")]
        aux_extradata: Option<Vec<u8>>,
        /// The full-quality fit size in physical pixels: the box the client
        /// stretches the stream to before fitting it to the window. Equal to
        /// the stream size unless the server sends a reduced-resolution
        /// stream.
        #[n(8)]
        view_width: u32,
        #[n(9)]
        view_height: u32,
        /// The frame-rate ceiling in force, for the client's stats display.
        #[n(10)]
        fps_cap: u32,
    },
    #[n(2)]
    VideoFrame {
        #[n(0)]
        frame_id: u64,
        #[n(1)]
        pts_us: u64,
        #[n(2)]
        keyframe: bool,
        #[n(3)]
        damage: Vec<Rect>,
        #[n(4)]
        data_len: u32,
        /// `aux_len > 0` iff `chroma == Dual420`; bytes follow the header.
        #[n(5)]
        aux_len: u32,
    },
    #[n(3)]
    CursorShape {
        #[n(0)]
        id: u32,
        #[n(1)]
        width: u32,
        #[n(2)]
        height: u32,
        #[n(3)]
        hot_x: i32,
        #[n(4)]
        hot_y: i32,
        #[n(5)]
        argb_len: u32,
    },
    #[n(4)]
    CursorPos {
        #[n(0)]
        x: f64,
        #[n(1)]
        y: f64,
        #[n(2)]
        shape_id: u32,
        #[n(3)]
        visible: bool,
    },
    #[n(5)]
    ClipboardOffer {
        #[n(0)]
        serial: u32,
        #[n(1)]
        mime_types: Vec<String>,
        #[n(2)]
        files: Vec<ClipboardFile>,
    },
    #[n(6)]
    ClipboardRequest {
        #[n(0)]
        id: u32,
        #[n(1)]
        serial: u32,
        #[n(2)]
        item: ClipboardItem,
    },
    #[n(7)]
    ClipboardData {
        #[n(0)]
        id: u32,
        #[n(1)]
        offset: u64,
        #[n(2)]
        data_len: u32,
        #[n(3)]
        done: bool,
    },
    #[n(8)]
    Pong {
        #[n(0)]
        t: u64,
        #[n(1)]
        server_now_ms: u64,
    },
    #[n(9)]
    Error {
        #[n(0)]
        code: u16,
        #[n(1)]
        message: String,
        /// The server's [`PROTOCOL_VERSION`].
        #[n(2)]
        version: Option<u16>,
    },
    #[n(10)]
    ClipboardAck {
        #[n(0)]
        id: u32,
        #[n(1)]
        received: u64,
    },
    #[n(11)]
    ClipboardAbort {
        #[n(0)]
        id: u32,
    },
    /// Sent right after HelloAck; the client answers with `ClientMsg::Pong`
    /// ahead of anything else, seeding the server's round-trip estimate
    /// before the first frame is acked.
    #[n(12)]
    Ping {
        #[n(0)]
        t: u64,
    },
    /// The sound that follows in `AudioFrame`s; sent whenever capture
    /// (re)starts, so the client resets its decoder and buffer.
    #[n(13)]
    AudioStart {
        #[n(0)]
        codec: AudioCodec,
        #[n(1)]
        sample_rate: u32,
        #[n(2)]
        channels: u32,
    },
    /// One coded packet; `data_len` payload bytes follow the header. `seq`
    /// counts packets since the last `AudioStart` and wraps, so the client
    /// can conceal one the server dropped.
    #[n(14)]
    AudioFrame {
        #[n(0)]
        seq: u32,
        #[n(1)]
        data_len: u32,
    },
}

impl ServerMsg {
    pub fn error(code: u16, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
            version: Some(PROTOCOL_VERSION),
        }
    }
}

/// The client's first message as the server reads it. The version is read
/// before anything else, so a `Hello` from another protocol version is
/// recognised even when the rest of it has changed shape.
#[derive(Debug, Clone, PartialEq)]
pub enum Greeting {
    Hello { keymap: String, caps: ClientCaps },
    OtherVersion(u16),
}

impl<'b, C> Decode<'b, C> for Greeting {
    fn decode(d: &mut Decoder<'b>, ctx: &mut C) -> Result<Self, Error> {
        let start = d.position();
        d.array()?;
        if d.u32()? != 0 {
            return Err(Error::message("expected Hello"));
        }
        d.array()?;
        let version = d.u16()?;
        if version != PROTOCOL_VERSION {
            return Ok(Self::OtherVersion(version));
        }
        d.set_position(start);
        match ClientMsg::decode(d, ctx)? {
            ClientMsg::Hello { keymap, caps, .. } => Ok(Self::Hello { keymap, caps }),
            _ => Err(Error::message("expected Hello")),
        }
    }
}

/// The clipboard variants have the same shape in both directions so the
/// transfer logic can be shared; these convert to and from [`ClipboardMsg`].
macro_rules! clipboard_bridge {
    ($msg:ident) => {
        impl From<ClipboardMsg> for $msg {
            fn from(m: ClipboardMsg) -> Self {
                match m {
                    ClipboardMsg::Offer {
                        serial,
                        mime_types,
                        files,
                    } => $msg::ClipboardOffer {
                        serial,
                        mime_types,
                        files,
                    },
                    ClipboardMsg::Request { id, serial, item } => {
                        $msg::ClipboardRequest { id, serial, item }
                    }
                    ClipboardMsg::Data {
                        id,
                        offset,
                        data_len,
                        done,
                    } => $msg::ClipboardData {
                        id,
                        offset,
                        data_len,
                        done,
                    },
                    ClipboardMsg::Ack { id, received } => $msg::ClipboardAck { id, received },
                    ClipboardMsg::Abort { id } => $msg::ClipboardAbort { id },
                }
            }
        }

        impl $msg {
            /// Split off a clipboard message; any other message is handed back.
            pub fn into_clipboard(self) -> Result<ClipboardMsg, Self> {
                Ok(match self {
                    $msg::ClipboardOffer {
                        serial,
                        mime_types,
                        files,
                    } => ClipboardMsg::Offer {
                        serial,
                        mime_types,
                        files,
                    },
                    $msg::ClipboardRequest { id, serial, item } => {
                        ClipboardMsg::Request { id, serial, item }
                    }
                    $msg::ClipboardData {
                        id,
                        offset,
                        data_len,
                        done,
                    } => ClipboardMsg::Data {
                        id,
                        offset,
                        data_len,
                        done,
                    },
                    $msg::ClipboardAck { id, received } => ClipboardMsg::Ack { id, received },
                    $msg::ClipboardAbort { id } => ClipboardMsg::Abort { id },
                    other => return Err(other),
                })
            }
        }
    };
}

clipboard_bridge!(ClientMsg);
clipboard_bridge!(ServerMsg);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages() {
        let msgs = vec![
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                keymap: "xkb".into(),
                caps: ClientCaps {
                    codecs: vec![Codec::H264],
                    max_width: 3840,
                    max_height: 2160,
                    chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
                    features: vec!["x".into()],
                },
            },
            ClientMsg::Key {
                keycode: 30,
                pressed: true,
            },
            ClientMsg::PointerAxis {
                axis: Axis::Vertical,
                value: 1.5,
                discrete: Some(1),
                stop: false,
            },
            ClientMsg::FrameAck {
                frame_id: 42,
                decoded_at_ms: 1000,
            },
            ClientMsg::Keymap {
                keymap: "xkb".into(),
            },
        ];
        for m in msgs {
            let bytes = minicbor::to_vec(&m).unwrap();
            let back: ClientMsg = minicbor::decode(&bytes).unwrap();
            assert_eq!(m, back);
        }
        let s = ServerMsg::VideoFrame {
            frame_id: 1,
            pts_us: 2,
            keyframe: true,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }],
            data_len: 100,
            aux_len: 50,
        };
        let bytes = minicbor::to_vec(&s).unwrap();
        assert_eq!(minicbor::decode::<ServerMsg>(&bytes).unwrap(), s);
    }

    #[test]
    fn clipboard_messages_bridge_both_directions() {
        let msgs = vec![
            ClipboardMsg::Offer {
                serial: 4,
                mime_types: vec!["image/png".into()],
                files: vec![ClipboardFile {
                    path: "a/b.txt".into(),
                    size: 3,
                    dir: false,
                }],
            },
            ClipboardMsg::Request {
                id: 7,
                serial: 4,
                item: ClipboardItem::File(0),
            },
            ClipboardMsg::Data {
                id: 7,
                offset: 10,
                data_len: 3,
                done: true,
            },
            ClipboardMsg::Ack {
                id: 7,
                received: 13,
            },
            ClipboardMsg::Abort { id: 7 },
        ];
        for m in msgs {
            let c = ClientMsg::from(m.clone());
            let bytes = minicbor::to_vec(&c).unwrap();
            let back: ClientMsg = minicbor::decode(&bytes).unwrap();
            assert_eq!(back.into_clipboard(), Ok(m.clone()));
            let s = ServerMsg::from(m.clone());
            let bytes = minicbor::to_vec(&s).unwrap();
            let back: ServerMsg = minicbor::decode(&bytes).unwrap();
            assert_eq!(back.into_clipboard(), Ok(m));
        }
        assert_eq!(ClientMsg::Bye.into_clipboard(), Err(ClientMsg::Bye));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// These bytes are the protocol: a change here breaks peers from other
    /// builds, so it must follow the rules at the top of this file.
    #[test]
    fn encodings_are_stable() {
        let client = [
            (
                ClientMsg::Hello {
                    version: 1,
                    keymap: "k".into(),
                    caps: ClientCaps {
                        codecs: vec![Codec::H264],
                        max_width: 3840,
                        max_height: 2160,
                        chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
                        features: vec!["f".into()],
                    },
                },
                "82008301616b858101190f00190870820102816166",
            ),
            (
                ClientMsg::Key {
                    keycode: 30,
                    pressed: true,
                },
                "820282181ef5",
            ),
            (
                ClientMsg::PointerMotion { x: 12.5, y: 7.25 },
                "820382fb4029000000000000fb401d000000000000",
            ),
            (
                ClientMsg::FrameAck {
                    frame_id: 42,
                    decoded_at_ms: 1000,
                },
                "820682182a1903e8",
            ),
            (ClientMsg::RequestKeyframe, "820780"),
            (ClientMsg::Audio { enabled: true }, "821181f5"),
        ];
        for (msg, bytes) in client {
            assert_eq!(hex(&minicbor::to_vec(&msg).unwrap()), bytes, "{msg:?}");
        }
        let server = [
            (
                ServerMsg::HelloAck {
                    version: 1,
                    session: SessionInfo {
                        headless: true,
                        output: "o".into(),
                    },
                    outputs: vec![OutputInfo {
                        name: "o".into(),
                        width: 1920,
                        height: 1080,
                        scale_milli: 1000,
                    }],
                    features: vec![],
                },
                "8200840182f5616f8184616f1907801904381903e880",
            ),
            (
                ServerMsg::StreamConfig {
                    codec: Codec::H264,
                    chroma: ChromaMode::Dual420,
                    pipeline: Some(VideoPipeline::Gpu),
                    width: 1920,
                    height: 1080,
                    scale_milli: 1000,
                    extradata: vec![],
                    aux_extradata: None,
                    view_width: 1920,
                    view_height: 1080,
                    fps_cap: 60,
                },
                "82018b0101001907801904381903e840f6190780190438183c",
            ),
            (
                ServerMsg::VideoFrame {
                    frame_id: 1,
                    pts_us: 2,
                    keyframe: true,
                    damage: vec![Rect {
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 10,
                    }],
                    data_len: 100,
                    aux_len: 50,
                },
                "8202860102f5818400000a0a18641832",
            ),
            (ServerMsg::error(ERROR_VERSION, "m"), "82098301616d01"),
            (
                ServerMsg::AudioStart {
                    codec: AudioCodec::Opus,
                    sample_rate: 48000,
                    channels: 2,
                },
                "820d830019bb8002",
            ),
            (
                ServerMsg::AudioFrame {
                    seq: 7,
                    data_len: 160,
                },
                "820e820718a0",
            ),
        ];
        for (msg, bytes) in server {
            assert_eq!(hex(&minicbor::to_vec(&msg).unwrap()), bytes, "{msg:?}");
        }
    }

    /// A newer client's `Hello`: an extra field in the message and in its
    /// caps, and a codec and chroma mode this build does not know.
    #[derive(Encode)]
    enum NewerClientMsg {
        #[n(0)]
        Hello {
            #[n(0)]
            version: u16,
            #[n(1)]
            keymap: String,
            #[n(2)]
            caps: NewerCaps,
            #[n(3)]
            layout: String,
        },
    }

    #[derive(Encode)]
    struct NewerCaps {
        #[n(0)]
        codecs: Vec<NewerCodec>,
        #[n(1)]
        max_width: u32,
        #[n(2)]
        max_height: u32,
        #[n(3)]
        chroma: Vec<NewerCodec>,
        #[n(4)]
        features: Vec<String>,
        #[n(5)]
        max_fps: u32,
    }

    #[derive(Encode)]
    #[cbor(index_only)]
    enum NewerCodec {
        #[n(1)]
        H264OrDual420,
        #[n(9)]
        Future,
    }

    #[test]
    fn hello_from_a_newer_client_decodes() {
        let newer = NewerClientMsg::Hello {
            version: 1,
            keymap: "k".into(),
            caps: NewerCaps {
                codecs: vec![NewerCodec::Future, NewerCodec::H264OrDual420],
                max_width: 3840,
                max_height: 2160,
                chroma: vec![NewerCodec::H264OrDual420, NewerCodec::Future],
                features: vec!["future".into()],
                max_fps: 120,
            },
            layout: "us".into(),
        };
        let bytes = minicbor::to_vec(&newer).unwrap();
        assert_eq!(
            minicbor::decode::<ClientMsg>(&bytes).unwrap(),
            ClientMsg::Hello {
                version: 1,
                keymap: "k".into(),
                caps: ClientCaps {
                    codecs: vec![Codec::H264],
                    max_width: 3840,
                    max_height: 2160,
                    chroma: vec![ChromaMode::Dual420],
                    features: vec!["future".into()],
                },
            }
        );
    }

    #[test]
    fn greeting_reads_the_version_of_any_hello() {
        let hello = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: "k".into(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 1,
                max_height: 1,
                chroma: vec![ChromaMode::Dual420],
                features: vec![],
            },
        };
        let ClientMsg::Hello { keymap, caps, .. } = hello.clone() else {
            unreachable!()
        };
        assert_eq!(
            minicbor::decode::<Greeting>(&minicbor::to_vec(&hello).unwrap()).unwrap(),
            Greeting::Hello { keymap, caps }
        );

        /// A future protocol version whose `Hello` has a different shape.
        #[derive(Encode)]
        enum Future {
            #[n(0)]
            Hello {
                #[n(0)]
                version: u16,
                #[n(1)]
                caps: u64,
            },
        }
        let future = minicbor::to_vec(Future::Hello {
            version: PROTOCOL_VERSION + 1,
            caps: 7,
        })
        .unwrap();
        assert_eq!(
            minicbor::decode::<Greeting>(&future).unwrap(),
            Greeting::OtherVersion(PROTOCOL_VERSION + 1)
        );
    }

    /// A newer server's `StreamConfig`: a pipeline this build does not know,
    /// and an extra field after the ones it does.
    #[derive(Encode)]
    enum NewerServerMsg {
        #[n(1)]
        StreamConfig {
            #[n(0)]
            codec: Codec,
            #[n(1)]
            chroma: ChromaMode,
            #[n(2)]
            pipeline: NewerPipeline,
            #[n(3)]
            width: u32,
            #[n(4)]
            height: u32,
            #[n(5)]
            scale_milli: u32,
            #[n(6)]
            #[cbor(with = "minicbor::bytes")]
            extradata: Vec<u8>,
            #[n(7)]
            #[cbor(with = "minicbor::bytes")]
            aux_extradata: Option<Vec<u8>>,
            #[n(8)]
            view_width: u32,
            #[n(9)]
            view_height: u32,
            #[n(10)]
            fps_cap: u32,
            #[n(11)]
            hdr: bool,
        },
    }

    #[derive(Encode)]
    #[cbor(index_only)]
    enum NewerPipeline {
        #[n(9)]
        Npu,
    }

    #[test]
    fn stream_config_from_a_newer_server_decodes() {
        let newer = NewerServerMsg::StreamConfig {
            codec: Codec::H264,
            chroma: ChromaMode::Dual420,
            pipeline: NewerPipeline::Npu,
            width: 1920,
            height: 1080,
            scale_milli: 1000,
            extradata: vec![],
            aux_extradata: None,
            view_width: 1920,
            view_height: 1080,
            fps_cap: 60,
            hdr: true,
        };
        assert_eq!(
            minicbor::decode::<ServerMsg>(&minicbor::to_vec(&newer).unwrap()).unwrap(),
            ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: ChromaMode::Dual420,
                pipeline: None,
                width: 1920,
                height: 1080,
                scale_milli: 1000,
                extradata: vec![],
                aux_extradata: None,
                view_width: 1920,
                view_height: 1080,
                fps_cap: 60,
            }
        );
    }

    #[test]
    fn known_reads_indefinite_lists_and_keeps_other_errors() {
        let mut d = minicbor::Decoder::new(&[0x9f, 0x01, 0x09, 0x02, 0xff]);
        assert_eq!(
            crate::cbor::known::<(), Codec>(&mut d, &mut ()).unwrap(),
            vec![Codec::H264, Codec::H265]
        );
        let mut d = minicbor::Decoder::new(&[0x81, 0x61, 0x78]);
        assert!(crate::cbor::known::<(), Codec>(&mut d, &mut ()).is_err());
    }

    #[test]
    fn the_largest_file_offer_fits_in_a_frame() {
        use crate::clipboard::{MAX_FILE_ENTRIES, MAX_FILE_PATH_BYTES};
        let path_len = MAX_FILE_PATH_BYTES / MAX_FILE_ENTRIES;
        let offer = ClientMsg::ClipboardOffer {
            serial: u32::MAX,
            mime_types: crate::clipboard::TEXT_MIMES
                .iter()
                .map(|m| m.to_string())
                .collect(),
            files: (0..MAX_FILE_ENTRIES)
                .map(|i| ClipboardFile {
                    path: format!("{i:0path_len$}"),
                    size: u64::MAX,
                    dir: false,
                })
                .collect(),
        };
        assert!(minicbor::to_vec(&offer).unwrap().len() <= crate::frame::MAX_FRAME_BODY);
    }

    /// The shape every new field takes: an optional enum appended last.
    #[derive(Debug, PartialEq, Encode, Decode)]
    struct Grown {
        #[n(0)]
        width: u32,
        #[n(1)]
        #[cbor(decode_with = "crate::cbor::optional", nil = "crate::cbor::none")]
        pipeline: Option<VideoPipeline>,
    }

    #[derive(Debug, PartialEq, Encode, Decode)]
    struct Original {
        #[n(0)]
        width: u32,
    }

    #[test]
    fn an_appended_optional_enum_is_invisible_when_absent() {
        let grown = Grown {
            width: 7,
            pipeline: None,
        };
        let bytes = minicbor::to_vec(&grown).unwrap();
        assert_eq!(bytes, minicbor::to_vec(Original { width: 7 }).unwrap());
        assert_eq!(minicbor::decode::<Grown>(&bytes).unwrap(), grown);
        let with_null = [0x82, 0x07, 0xf6];
        assert_eq!(minicbor::decode::<Grown>(&with_null).unwrap(), grown);
        let set = Grown {
            width: 7,
            pipeline: Some(VideoPipeline::Cpu),
        };
        let bytes = minicbor::to_vec(&set).unwrap();
        assert_eq!(minicbor::decode::<Grown>(&bytes).unwrap(), set);
        assert_eq!(
            minicbor::decode::<Original>(&bytes).unwrap(),
            Original { width: 7 }
        );
    }
}
