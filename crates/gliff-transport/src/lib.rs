//! Length-prefixed framing over any tokio `AsyncRead + AsyncWrite`, the
//! clipboard transfer engine, plus ssh process spawning for the client.
//!
//! Each frame is a CBOR-encoded message (`ClientMsg`/`ServerMsg`) behind a
//! header that gives its length and the total length of the raw payload bytes
//! after it (the `data_len`, `aux_len`, `argb_len` the message declares).
//! Payloads never go through the codec; the writer sends them with
//! `write_vectored` and the reader returns them as a `Bytes` slice for the
//! decoder. Because the header covers the payload, a message this build does
//! not know is skipped whole.

#![forbid(unsafe_code)]

pub mod clipboard;
mod listener;
mod ssh;

use bytes::{Buf, Bytes, BytesMut};
pub use gliff_proto::frame::MAX_PAYLOAD;
use gliff_proto::frame::{HEADER_LEN, MAX_FRAME_BODY, MAX_FRAME_PAYLOAD};
use minicbor::decode::Decoder;
use minicbor::{Decode, Encode};
use std::io::IoSlice;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use listener::bind_development_listener;
pub use ssh::{spawn_ssh, SshTarget};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] minicbor::encode::Error<std::convert::Infallible>),
    #[error("decode: {0}")]
    Decode(#[from] minicbor::decode::Error),
    #[error("frame body of {0} bytes exceeds the limit")]
    BodyTooLarge(u32),
    #[error("payload of {0} bytes exceeds the limit")]
    PayloadTooLarge(u32),
    #[error("read {asked} payload bytes, but the frame has {left} left")]
    PayloadOverrun { asked: u32, left: u32 },
    #[error("connection closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy)]
struct Header {
    body_len: u32,
    payload_len: u32,
}

/// Framed reader/writer over a split or duplex stream.
pub struct Framed<S> {
    stream: S,
    read_buf: BytesMut,
    /// Header of a frame whose body has not arrived, so a cancelled
    /// `read_msg` resumes where it stopped.
    pending: Option<Header>,
    /// Payload bytes of the last message the caller has not read yet.
    payload_left: u32,
    /// Bytes to discard before the next header: a skipped message's payload,
    /// or payload the caller left unread.
    skip_left: u64,
}

impl<S> Framed<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
            pending: None,
            payload_left: 0,
            skip_left: 0,
        }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: AsyncWrite + Unpin> Framed<S> {
    /// Write a message with no trailing payload.
    pub async fn write_msg<M: Encode<()>>(&mut self, msg: &M) -> Result<()> {
        self.write_msg_with_payloads(msg, &[]).await
    }

    /// Write a message then the given payload slices, all in order. Uses
    /// `write_vectored` so a large encoder payload is not copied into a
    /// staging buffer first.
    pub async fn write_msg_with_payloads<M: Encode<()>>(
        &mut self,
        msg: &M,
        payloads: &[&[u8]],
    ) -> Result<()> {
        let body = minicbor::to_vec(msg)?;
        if body.len() > MAX_FRAME_BODY {
            return Err(Error::BodyTooLarge(saturating_u32(body.len())));
        }
        if let Some(p) = payloads.iter().find(|p| p.len() > MAX_PAYLOAD as usize) {
            return Err(Error::PayloadTooLarge(saturating_u32(p.len())));
        }
        let payload_len: usize = payloads.iter().map(|p| p.len()).sum();
        if payload_len > MAX_FRAME_PAYLOAD as usize {
            return Err(Error::PayloadTooLarge(saturating_u32(payload_len)));
        }
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
        header[4..].copy_from_slice(&(payload_len as u32).to_le_bytes());
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + payloads.len());
        parts.push(&header);
        parts.push(&body);
        parts.extend_from_slice(payloads);
        write_all_vectored(&mut self.stream, &parts).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> Framed<S> {
    /// Read one message body. Its payload (if any) must then be read with
    /// [`read_payload`] according to the message's declared lengths; payload
    /// left unread is discarded before the next message. A message this
    /// build does not know is skipped, payload and all.
    ///
    /// Cancel-safe: a caller may race this in a `select!`. A parsed header
    /// and a partly discarded payload are kept across a cancellation, and
    /// buffered stream bytes are only consumed once the full item is present.
    pub async fn read_msg<M: for<'b> Decode<'b, ()>>(&mut self) -> Result<M> {
        loop {
            self.skip_left += u64::from(std::mem::take(&mut self.payload_left));
            self.discard_skipped().await?;
            let header = match self.pending {
                Some(h) => h,
                None => {
                    let b = self.read_exact_bytes(HEADER_LEN).await?;
                    let h = Header {
                        body_len: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                        payload_len: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                    };
                    if h.body_len as usize > MAX_FRAME_BODY {
                        return Err(Error::BodyTooLarge(h.body_len));
                    }
                    if h.payload_len > MAX_FRAME_PAYLOAD {
                        return Err(Error::PayloadTooLarge(h.payload_len));
                    }
                    self.pending = Some(h);
                    h
                }
            };
            let body = self.read_exact_bytes(header.body_len as usize).await?;
            self.pending = None;
            match minicbor::decode::<M>(&body) {
                Ok(msg) => {
                    self.payload_left = header.payload_len;
                    return Ok(msg);
                }
                Err(e) if is_unknown_message(&body, &e) => {
                    tracing::debug!(error = %e, "skipping a message this build does not know");
                    self.skip_left += u64::from(header.payload_len);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Read exactly `len` payload bytes that follow a message.
    pub async fn read_payload(&mut self, len: u32) -> Result<Bytes> {
        if len > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge(len));
        }
        if len > self.payload_left {
            return Err(Error::PayloadOverrun {
                asked: len,
                left: self.payload_left,
            });
        }
        let bytes = self.read_exact_bytes(len as usize).await?.freeze();
        self.payload_left -= len;
        Ok(bytes)
    }

    async fn discard_skipped(&mut self) -> Result<()> {
        while self.skip_left > 0 {
            if self.read_buf.is_empty() && self.stream.read_buf(&mut self.read_buf).await? == 0 {
                return Err(Error::Closed);
            }
            let n = self.read_buf.len().min(self.skip_left as usize);
            self.read_buf.advance(n);
            self.skip_left -= n as u64;
        }
        Ok(())
    }

    async fn read_exact_bytes(&mut self, n: usize) -> Result<BytesMut> {
        while self.read_buf.len() < n {
            self.read_buf
                .reserve((n - self.read_buf.len()).max(32 * 1024));
            if self.stream.read_buf(&mut self.read_buf).await? == 0 {
                return Err(Error::Closed);
            }
        }
        Ok(self.read_buf.split_to(n))
    }
}

fn saturating_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Whether `e` is the message's own variant being unknown, as opposed to an
/// unknown value inside a message this build does know. Messages encode as
/// `[variant, fields]`, so the variant index sits right after the array head.
fn is_unknown_message(body: &[u8], e: &minicbor::decode::Error) -> bool {
    let mut d = Decoder::new(body);
    e.is_unknown_variant() && d.array().is_ok() && e.position() == Some(d.position())
}

/// Write every byte of `parts` in order using vectored writes, handling
/// partial writes by tracking a global offset and rebuilding the `IoSlice`
/// list for the unwritten remainder each iteration.
async fn write_all_vectored<S: AsyncWrite + Unpin>(stream: &mut S, parts: &[&[u8]]) -> Result<()> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut done = 0usize;
    while done < total {
        // Build IoSlices for the remainder starting at global offset `done`.
        let mut slices: Vec<IoSlice> = Vec::with_capacity(parts.len());
        let mut skip = done;
        for p in parts {
            if skip >= p.len() {
                skip -= p.len();
                continue;
            }
            slices.push(IoSlice::new(&p[skip..]));
            skip = 0;
        }
        let n = stream.write_vectored(&slices).await?;
        if n == 0 {
            return Err(Error::Closed);
        }
        done += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::frame::MAX_FRAME_BODY;
    use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, PROTOCOL_VERSION};

    #[tokio::test]
    async fn round_trips_message_and_payload() {
        let (a, b) = tokio::io::duplex(4096);
        let mut wr = Framed::new(a);
        let mut rd = Framed::new(b);
        let msg = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: "k".into(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 1920,
                max_height: 1080,
                chroma: vec![ChromaMode::Dual420],
                features: vec![],
            },
        };
        let payload = vec![7u8; 5000];
        let p2 = vec![9u8; 100];
        let m2 = msg.clone();
        let payload2 = payload.clone();
        let p2b = p2.clone();
        let task = tokio::spawn(async move {
            wr.write_msg_with_payloads(&m2, &[&payload2, &p2b])
                .await
                .unwrap();
        });
        let got: ClientMsg = rd.read_msg().await.unwrap();
        assert_eq!(got, msg);
        let gp = rd.read_payload(5000).await.unwrap();
        let gp2 = rd.read_payload(100).await.unwrap();
        assert_eq!(&gp[..], &payload[..]);
        assert_eq!(&gp2[..], &p2[..]);
        task.await.unwrap();
    }

    /// Messages a newer peer might send: one this build has no variant for,
    /// and a known one carrying an enum value this build does not know.
    #[derive(Encode)]
    enum NewerClientMsg {
        #[n(2)]
        Key {
            #[n(0)]
            keycode: u32,
            #[n(1)]
            pressed: bool,
        },
        #[n(5)]
        PointerAxis {
            #[n(0)]
            axis: NewerAxis,
            #[n(1)]
            value: f64,
            #[n(2)]
            discrete: Option<i32>,
            #[n(3)]
            stop: bool,
        },
        #[n(99)]
        Future {
            #[n(0)]
            data_len: u32,
        },
    }

    #[derive(Encode)]
    #[cbor(index_only)]
    enum NewerAxis {
        #[n(7)]
        Diagonal,
    }

    const KEY: NewerClientMsg = NewerClientMsg::Key {
        keycode: 30,
        pressed: true,
    };

    #[tokio::test]
    async fn skips_an_unknown_message_and_its_payload() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut wr = Framed::new(a);
        let mut rd = Framed::new(b);
        let payload = vec![1u8; 40_000];
        let task = tokio::spawn(async move {
            wr.write_msg_with_payloads(&NewerClientMsg::Future { data_len: 40_000 }, &[&payload])
                .await
                .unwrap();
            wr.write_msg(&KEY).await.unwrap();
        });
        let got: ClientMsg = rd.read_msg().await.unwrap();
        assert_eq!(
            got,
            ClientMsg::Key {
                keycode: 30,
                pressed: true
            }
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn discards_payload_the_caller_leaves_unread() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut wr = Framed::new(a);
        let mut rd = Framed::new(b);
        let chunk = ClientMsg::ClipboardData {
            id: 1,
            offset: 0,
            data_len: 3,
            done: true,
        };
        let sent = chunk.clone();
        let task = tokio::spawn(async move {
            wr.write_msg_with_payloads(&sent, &[b"abc"]).await.unwrap();
            wr.write_msg(&KEY).await.unwrap();
        });
        assert_eq!(rd.read_msg::<ClientMsg>().await.unwrap(), chunk);
        assert!(matches!(
            rd.read_payload(4).await,
            Err(Error::PayloadOverrun { asked: 4, left: 3 })
        ));
        assert!(matches!(
            rd.read_msg::<ClientMsg>().await.unwrap(),
            ClientMsg::Key { .. }
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn fails_on_an_unknown_value_inside_a_known_message() {
        let (a, b) = tokio::io::duplex(4096);
        let mut wr = Framed::new(a);
        let mut rd = Framed::new(b);
        let task = tokio::spawn(async move {
            let axis = NewerClientMsg::PointerAxis {
                axis: NewerAxis::Diagonal,
                value: 1.0,
                discrete: None,
                stop: false,
            };
            wr.write_msg(&axis).await.unwrap();
        });
        assert!(matches!(
            rd.read_msg::<ClientMsg>().await,
            Err(Error::Decode(e)) if e.is_unknown_variant()
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn frame_layout_is_stable() {
        let mut wr = Framed::new(Vec::new());
        wr.write_msg_with_payloads(&KEY, &[b"ab"]).await.unwrap();
        let hex: String = wr.into_inner().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "0600000002000000820282181ef56162");
    }

    #[tokio::test]
    async fn writer_refuses_frames_the_reader_would_reject() {
        let mut wr = Framed::new(Vec::new());
        let huge = ClientMsg::Keymap {
            keymap: "x".repeat(MAX_FRAME_BODY),
        };
        assert!(matches!(
            wr.write_msg(&huge).await,
            Err(Error::BodyTooLarge(_))
        ));
        assert!(wr.into_inner().is_empty());
        let mut wr = Framed::new(Vec::new());
        let big = vec![0u8; MAX_PAYLOAD as usize + 1];
        assert!(matches!(
            wr.write_msg_with_payloads(&KEY, &[&big]).await,
            Err(Error::PayloadTooLarge(_))
        ));
        assert!(wr.into_inner().is_empty());
    }

    #[tokio::test]
    async fn reader_refuses_oversized_headers() {
        let mut header = Vec::new();
        header.extend_from_slice(&6u32.to_le_bytes());
        header.extend_from_slice(&(MAX_FRAME_PAYLOAD + 1).to_le_bytes());
        let mut rd = Framed::new(&header[..]);
        assert!(matches!(
            rd.read_msg::<ClientMsg>().await,
            Err(Error::PayloadTooLarge(_))
        ));
        let mut header = Vec::new();
        header.extend_from_slice(&(MAX_FRAME_BODY as u32 + 1).to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        let mut rd = Framed::new(&header[..]);
        assert!(matches!(
            rd.read_msg::<ClientMsg>().await,
            Err(Error::BodyTooLarge(_))
        ));
    }

    /// Read a message while another branch keeps winning the race, as a
    /// `select!` in the session loop may.
    async fn read_racing<S: AsyncRead + Unpin>(
        rd: &mut Framed<S>,
        cancelled: &mut u32,
    ) -> ClientMsg {
        loop {
            tokio::select! {
                biased;
                _ = tokio::task::yield_now() => *cancelled += 1,
                msg = rd.read_msg::<ClientMsg>() => return msg.unwrap(),
            }
        }
    }

    /// `read_msg` is raced in `select!`, so it must resume cleanly after
    /// being cancelled mid-header, mid-body and mid-skip.
    #[tokio::test]
    async fn read_msg_survives_cancellation() {
        let mut frames = Framed::new(Vec::new());
        frames
            .write_msg_with_payloads(&NewerClientMsg::Future { data_len: 300 }, &[&[5u8; 300]])
            .await
            .unwrap();
        let chunk = ClientMsg::ClipboardData {
            id: 1,
            offset: 0,
            data_len: 3,
            done: true,
        };
        frames
            .write_msg_with_payloads(&chunk, &[b"abc"])
            .await
            .unwrap();
        frames.write_msg(&KEY).await.unwrap();
        let bytes = frames.into_inner();

        let (mut a, b) = tokio::io::duplex(1);
        let task = tokio::spawn(async move {
            for byte in bytes {
                a.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut rd = Framed::new(b);
        let mut cancelled = 0;
        assert_eq!(read_racing(&mut rd, &mut cancelled).await, chunk);
        assert_eq!(&rd.read_payload(3).await.unwrap()[..], b"abc");
        let key = read_racing(&mut rd, &mut cancelled).await;
        assert!(matches!(key, ClientMsg::Key { .. }));
        assert!(cancelled > 100, "only {cancelled} cancellations");
        task.await.unwrap();
    }
}
