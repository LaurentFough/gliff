//! Software media pipeline: the CPU fallback for machines without a VA-API
//! H.264 codec. BGRA conversion and the AVC444 split run as fused fixed-point
//! passes in `convert`, checked against the reference code in `gliff-proto`;
//! H.264 encode/decode is OpenH264.
//!
//! Mirrors the shape of `gliff-vk`'s `Encoder`/`Decoder` so the server and
//! client can hold either behind a small enum.

use gliff_proto::chroma::{recombine_yuv444, Nv12};
use openh264::decoder::{DecodedYUV, Decoder as H264Decoder, DecoderConfig, Flush};
use openh264::encoder::{
    BitRate, Encoder as H264Encoder, EncoderConfig, FrameRate, FrameType, Profile, QpRange,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};
use openh264_sys2::{ENCODER_OPTION_TRACE_LEVEL, WELS_LOG_QUIET};
use std::sync::{Arc, Mutex};

pub mod convert;

use convert::I420;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("openh264: {0}")]
    Codec(#[from] openh264::Error),
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A decoded picture's size differs from the announced stream size.
    #[error("stream size: {0}")]
    StreamSize(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Which video pipeline to use, from `--video` or `GLIFF_VIDEO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    /// Vulkan compute + VA-API, falling back to the CPU when unavailable
    /// (the default).
    Gpu,
    /// Force the CPU pipeline even on a machine with a capable GPU.
    Cpu,
}

impl VideoMode {
    /// `cpu` or `gpu` from the CLI, else `GLIFF_VIDEO`, else `Gpu`.
    pub fn resolve(cli: Option<&str>) -> Self {
        let parse = |s: &str, from: &str| match s {
            "gpu" => Some(Self::Gpu),
            "cpu" => Some(Self::Cpu),
            other => {
                tracing::warn!(value = other, "{from} is not `gpu` or `cpu`; ignoring");
                None
            }
        };
        if let Some(mode) = cli.and_then(|s| parse(s, "--video")) {
            return mode;
        }
        std::env::var("GLIFF_VIDEO")
            .ok()
            .and_then(|s| parse(&s, "GLIFF_VIDEO"))
            .unwrap_or(Self::Gpu)
    }
}

#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    /// Target bitrate in bits per second (CBR), per stream.
    pub bitrate: u32,
    pub framerate: u32,
}

pub struct EncodedFrame {
    pub main: Vec<u8>,
    pub aux: Option<Vec<u8>>,
    pub keyframe: bool,
}

/// Packed BGRA pixels of one decoded frame, `width * 4` bytes per row.
#[derive(Debug)]
pub struct BgraFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Recycles the large buffers the zoomed CPU frames are written into. A
/// fresh 60 MB allocation per frame costs more in page faults than the
/// copy itself; a buffer that comes back from the display side is mapped
/// already.
#[derive(Clone, Default)]
pub struct PixelPool(Arc<Mutex<Vec<Vec<u8>>>>);

/// Buffers kept in a [`PixelPool`]: the one being written, one on screen,
/// one in transit.
const POOL_KEEP: usize = 3;

impl PixelPool {
    /// A buffer of exactly `len` bytes: a free one with the capacity when
    /// there is one, so its pages are mapped already, else a new one.
    /// `None` when the allocation fails, which a frame survives unzoomed.
    fn take(&self, len: usize) -> Option<Vec<u8>> {
        let mut free = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = match free.iter().position(|b| b.capacity() >= len) {
            Some(i) => free.swap_remove(i),
            None => {
                let mut buf = Vec::new();
                buf.try_reserve_exact(len).ok()?;
                buf
            }
        };
        buf.resize(len, 0);
        Some(buf)
    }

    /// Keep `buf` for reuse. When the pool is full, a larger buffer
    /// replaces the smallest one, so the big zoomed buffers are the ones
    /// that stay when the zoom changes.
    fn give(&self, buf: Vec<u8>) {
        let mut free = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if free.len() < POOL_KEEP {
            free.push(buf);
            return;
        }
        if let Some(i) = (0..free.len()).min_by_key(|&i| free[i].capacity()) {
            if free[i].capacity() < buf.capacity() {
                free[i] = buf;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Pixels borrowed from a [`PixelPool`]; dropping them returns the buffer.
pub struct PooledPixels {
    buf: Vec<u8>,
    pool: PixelPool,
}

impl AsRef<[u8]> for PooledPixels {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for PooledPixels {
    fn drop(&mut self) {
        self.pool.give(std::mem::take(&mut self.buf));
    }
}

/// A frame ready for display, possibly enlarged, in pooled memory.
pub struct PooledFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: PooledPixels,
}

impl BgraFrame {
    /// Replicate every pixel into a `zoom` x `zoom` block, so the display
    /// side can show the frame at an integer scale without resampling. At
    /// zoom 1 the pixels move without a copy, as they do when the enlarged
    /// frame cannot be allocated (the caller bounds the zoom).
    pub fn zoomed(self, zoom: u32, pool: &PixelPool) -> PooledFrame {
        let pool = pool.clone();
        let z = zoom as usize;
        let (w, h) = (self.width as usize, self.height as usize);
        let zoomed = (|| {
            if zoom <= 1 {
                return None;
            }
            let dst_stride = w.checked_mul(4)?.checked_mul(z)?;
            let len = dst_stride.checked_mul(h)?.checked_mul(z)?;
            let width = self.width.checked_mul(zoom)?;
            let height = self.height.checked_mul(zoom)?;
            Some((dst_stride, pool.take(len)?, width, height))
        })();
        let Some((dst_stride, mut buf, width, height)) = zoomed else {
            return PooledFrame {
                width: self.width,
                height: self.height,
                pixels: PooledPixels {
                    buf: self.pixels,
                    pool,
                },
            };
        };
        let src_stride = w * 4;
        {
            use rayon::prelude::*;
            buf.par_chunks_exact_mut(dst_stride * z)
                .zip(self.pixels.par_chunks_exact(src_stride))
                .for_each(|(block, src_row)| {
                    let (first, rest) = block.split_at_mut(dst_stride);
                    for (dst, px) in first.chunks_exact_mut(4 * z).zip(src_row.chunks_exact(4)) {
                        for out in dst.chunks_exact_mut(4) {
                            out.copy_from_slice(px);
                        }
                    }
                    for row in rest.chunks_exact_mut(dst_stride) {
                        row.copy_from_slice(first);
                    }
                });
        }
        PooledFrame {
            width,
            height,
            pixels: PooledPixels { buf, pool },
        }
    }
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Server side: one encoder object per session, like `gliff_vk::Encoder`.
pub struct Encoder {
    main: H264Encoder,
    aux: Option<H264Encoder>,
    settings: EncoderSettings,
    frames: u64,
    /// When the encoders were last created; every creation costs a keyframe.
    rebuilt_at: std::time::Instant,
}

impl Encoder {
    /// The largest size OpenH264 encodes (level 5.2, landscape).
    pub const MAX_SIZE: (u32, u32) = (3840, 2160);

    pub fn new(settings: EncoderSettings, dual: bool) -> Result<Self> {
        let main = new_h264_encoder(&settings)?;
        let aux = dual.then(|| new_h264_encoder(&settings)).transpose()?;
        Ok(Self {
            main,
            aux,
            settings,
            frames: 0,
            rebuilt_at: std::time::Instant::now(),
        })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// When the encoders were last created, which sent a keyframe.
    pub fn rebuilt_at(&self) -> std::time::Instant {
        self.rebuilt_at
    }

    /// Change the target bitrate of both streams from the next frame on.
    /// OpenH264 fixes the target at initialization, so the encoders are
    /// rebuilt; the fresh IDR also carries the parameter sets in-band.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.set_rate(bitrate, self.settings.framerate);
    }

    /// Change the target bitrate and the frame rate it is spread over, so
    /// the per-frame budget matches the frames that really leave.
    pub fn set_rate(&mut self, bitrate: u32, framerate: u32) {
        if (bitrate, framerate) == (self.settings.bitrate, self.settings.framerate) {
            return;
        }
        self.settings.bitrate = bitrate;
        self.settings.framerate = framerate.max(1);
        match Self::new(self.settings.clone(), self.aux.is_some()) {
            Ok(fresh) => {
                self.main = fresh.main;
                self.aux = fresh.aux;
                self.rebuilt_at = fresh.rebuilt_at;
            }
            Err(e) => tracing::warn!(error = %e, bitrate, "could not re-create the encoder"),
        }
    }

    /// Encode packed BGRA pixels of exactly `settings` size, tightly packed.
    pub fn encode_bgra(&mut self, bgra: &[u8], force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = (self.settings.width as usize, self.settings.height as usize);
        let (main_i420, aux_i420) = if self.aux.is_some() {
            let (m, a) = convert::split_yuv444(&convert::bgra_to_yuv444(bgra, w * 4, w, h));
            (m, Some(a))
        } else {
            (convert::bgra_to_i420(bgra, w * 4, w, h), None)
        };
        let ts = Timestamp::from_millis(self.frames * 1000 / self.settings.framerate.max(1) as u64);
        self.frames += 1;
        if force_keyframe {
            self.main.force_intra_frame();
            if let Some(a) = &mut self.aux {
                a.force_intra_frame();
            }
        }
        let (main, keyframe) = encode_i420(&mut self.main, &main_i420, ts)?;
        let aux = match (&mut self.aux, &aux_i420) {
            (Some(enc), Some(i420)) => Some(encode_i420(enc, i420, ts)?.0),
            _ => None,
        };
        Ok(EncodedFrame {
            main,
            aux,
            keyframe,
        })
    }
}

/// OpenH264 stops at four encoder threads.
const MAX_ENCODER_THREADS: u16 = 4;

/// A slice size the encoder rarely reaches, so slicing follows the thread
/// count rather than the byte count.
const MAX_SLICE_BYTES: u32 = 1 << 20;

fn encoder_threads() -> u16 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(MAX_ENCODER_THREADS as usize) as u16)
        .unwrap_or(1)
}

fn new_h264_encoder(settings: &EncoderSettings) -> Result<H264Encoder> {
    let (max_w, max_h) = Encoder::MAX_SIZE;
    if settings.width > max_w || settings.height > max_h {
        return Err(Error::Unsupported(format!(
            "{}x{} exceeds the OpenH264 maximum {max_w}x{max_h}",
            settings.width, settings.height
        )));
    }
    // The 4:2:0 chroma split needs even dimensions.
    if settings.width == 0
        || settings.height == 0
        || !settings.width.is_multiple_of(2)
        || !settings.height.is_multiple_of(2)
    {
        return Err(Error::Unsupported(format!(
            "{}x{} is not an even, non-zero size",
            settings.width, settings.height
        )));
    }
    // Baseline profile: OpenH264's decoder skips its one-picture reorder
    // buffer only for Baseline streams, so they display with no delay.
    // OpenH264 encodes a picture on several threads only when it may cut
    // it into slices; a size limit turns that on.
    let config = EncoderConfig::new()
        .debug(false)
        .num_threads(encoder_threads())
        .max_slice_len(MAX_SLICE_BYTES)
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        // Left unset, the range becomes 26..=35 for screen content, and QP 26
        // caps the quality whatever the bitrate.
        .qp(QpRange::new(12, 35))
        .bitrate(BitRate::from_bps(settings.bitrate))
        .max_frame_rate(FrameRate::from_hz(settings.framerate as f32))
        .skip_frames(false)
        .profile(Profile::Baseline)
        .vui(VuiConfig::bt709());
    let mut encoder = H264Encoder::with_api_config(OpenH264API::from_source(), config)?;
    // Quiet: in `--stdio` mode nothing may write to the wire by accident.
    // `debug(false)` only takes effect after the codec initializes on the
    // first encode, and initialization itself logs warnings, so set the
    // level now as well.
    let mut level = WELS_LOG_QUIET;
    // SAFETY: the codec pointer is live for the encoder's lifetime, and
    // OpenH264 accepts the trace level before initialization, reading one
    // 32-bit value from the pointer during the call.
    unsafe {
        encoder
            .raw_api()
            .set_option(ENCODER_OPTION_TRACE_LEVEL, (&raw mut level).cast());
    }
    Ok(encoder)
}

fn encode_i420(encoder: &mut H264Encoder, i420: &I420, ts: Timestamp) -> Result<(Vec<u8>, bool)> {
    let stream = encoder.encode_at(i420, ts)?;
    let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
    Ok((stream.to_vec(), keyframe))
}

/// Client side: one decoder object per stream, like `gliff_vk::Decoder`.
pub struct Decoder {
    main: H264Decoder,
    aux: Option<H264Decoder>,
    /// Access units fed minus pictures returned: what the decoders still
    /// hold. Zero for Baseline streams. A High-profile stream holds its
    /// first picture until the next unit or a [`Decoder::flush`]; after
    /// that OpenH264 releases each picture at once, since our encoder steps
    /// the POC by one (see `gliff_vk` encoder).
    held: u32,
    /// The display size the stream was announced with (`StreamConfig`).
    size: (u32, u32),
}

impl Decoder {
    /// A decoder for a stream of `width` x `height` display pixels. A
    /// picture of any other size is a decode error.
    pub fn new(dual: bool, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            main: new_h264_decoder()?,
            aux: dual.then(new_h264_decoder).transpose()?,
            held: 0,
            size: (width, height),
        })
    }

    /// True when the decoders hold a picture that only a further access
    /// unit or a [`Decoder::flush`] will release.
    pub fn has_pending(&self) -> bool {
        self.held > 0
    }

    /// Decode one access unit pair and recombine to a BGRA frame. `None`
    /// when no picture is ready yet: the first picture of a High-profile
    /// stream comes out one access unit late. Both decoders are fed every
    /// unit so the pair stays in step.
    pub fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<BgraFrame>> {
        self.held += 1;
        let size = self.size;
        let frame = match &mut self.aux {
            Some(dec) => match (self.main.decode(main)?, dec.decode(aux)?) {
                (Some(m), Some(a)) => Some(recombine_checked(&m, &a, size)?),
                (None, None) => None,
                _ => {
                    return Err(Error::Unsupported(
                        "main and aux streams fell out of step".into(),
                    ))
                }
            },
            None => match self.main.decode(main)? {
                Some(m) => Some(convert_checked(&m, size)?),
                None => None,
            },
        };
        let Some(frame) = frame else {
            return Ok(None);
        };
        self.held = self.held.saturating_sub(1);
        Ok(Some(frame))
    }

    /// Drain the picture still buffered in the decoders, to put it on screen
    /// when the stream goes quiet. Call it rarely: each OpenH264 flush leaks
    /// one picture slot until the next IDR, and the fourth in a row fails
    /// every following decode with `dsOutOfMemory`. With a POC step of one
    /// only the first picture after a new decoder is ever held.
    pub fn flush(&mut self) -> Result<Option<BgraFrame>> {
        let frame = match self.flush_remaining() {
            Ok(frame) => frame,
            Err(e) => {
                // Stop draining after an error. The client asks for a
                // keyframe to recover.
                self.held = 0;
                return Err(e);
            }
        };
        let Some(frame) = frame else {
            // Nothing came out, so nothing is drainable: correct any drift
            // the counter picked up from failed decodes.
            self.held = 0;
            return Ok(None);
        };
        self.held = self.held.saturating_sub(1);
        Ok(Some(frame))
    }

    fn flush_remaining(&mut self) -> Result<Option<BgraFrame>> {
        let size = self.size;
        Ok(match &mut self.aux {
            Some(dec) => {
                let (main, aux) = (self.main.flush_remaining()?, dec.flush_remaining()?);
                match (main.first(), aux.first()) {
                    (Some(m), Some(a)) => Some(recombine_checked(m, a, size)?),
                    _ => None,
                }
            }
            None => match self.main.flush_remaining()?.first() {
                Some(m) => Some(convert_checked(m, size)?),
                None => None,
            },
        })
    }
}

fn new_h264_decoder() -> Result<H264Decoder> {
    // No flush after decode: every OpenH264 flush leaks a picture slot in
    // single-thread mode (the released picture's buffer is never unreferenced),
    // and the pool is exhausted after three, seen as dsOutOfMemory on the
    // fourth frame. Pictures come out without it, see `Decoder::flush`.
    Ok(H264Decoder::with_api_config(
        OpenH264API::from_source(),
        DecoderConfig::new().flush_after_decode(Flush::NoFlush),
    )?)
}

/// Refuse a decoded picture whose size, cropped to even dimensions as the
/// conversion does, is not the announced `expected` size. An empty picture
/// is refused whatever was announced: the conversion cannot cut it into rows.
fn check_picture_size(dimensions: (usize, usize), expected: (u32, u32)) -> Result<()> {
    let (w, h) = (dimensions.0 & !1, dimensions.1 & !1);
    if w == 0 || h == 0 || (w, h) != (expected.0 as usize, expected.1 as usize) {
        return Err(Error::StreamSize(format!(
            "decoded picture {}x{} does not match the announced {}x{}",
            dimensions.0, dimensions.1, expected.0, expected.1
        )));
    }
    Ok(())
}

fn convert_checked(image: &DecodedYUV, expected: (u32, u32)) -> Result<BgraFrame> {
    check_picture_size(image.dimensions(), expected)?;
    Ok(decoded_to_bgra(image))
}

fn recombine_checked(
    main: &DecodedYUV,
    aux: &DecodedYUV,
    expected: (u32, u32),
) -> Result<BgraFrame> {
    check_picture_size(main.dimensions(), expected)?;
    check_picture_size(aux.dimensions(), expected)?;
    recombine_to_bgra(main, aux)
}

/// Convert a decoded picture straight to BGRA, cropped to even dimensions,
/// with its chroma repeated over each 2x2 block.
fn decoded_to_bgra(image: &DecodedYUV) -> BgraFrame {
    let (w, h) = image.dimensions();
    let (w, h) = (w & !1, h & !1);
    let pixels = convert::i420_to_bgra(image.y(), image.u(), image.v(), image.strides(), w, h);
    BgraFrame {
        width: w as u32,
        height: h as u32,
        pixels,
    }
}

/// Recombine a main and auxiliary picture into 4:4:4 and convert to BGRA.
fn recombine_to_bgra(main: &DecodedYUV, aux: &DecodedYUV) -> Result<BgraFrame> {
    let (main, aux) = (decoded_to_nv12(main), decoded_to_nv12(aux));
    if (aux.width, aux.height) != (main.width, main.height) {
        return Err(Error::Unsupported(
            "main and aux stream sizes differ".into(),
        ));
    }
    let yuv = recombine_yuv444(&main, &aux);
    Ok(BgraFrame {
        width: yuv.width as u32,
        height: yuv.height as u32,
        pixels: convert::yuv444_to_bgra(&yuv),
    })
}

/// Copy a decoded I420 image (strides may exceed the width) into a tightly
/// packed NV12 frame, cropped to even dimensions.
fn decoded_to_nv12(image: &DecodedYUV) -> Nv12 {
    let (w, h) = image.dimensions();
    let (w, h) = (w & !1, h & !1);
    let (sy, su, sv) = image.strides();
    let mut out = Nv12::new(w, h);
    for (dst, src) in out.y.chunks_exact_mut(w).zip(image.y().chunks(sy)) {
        dst.copy_from_slice(&src[..w]);
    }
    let (u, v) = (image.u(), image.v());
    for row in 0..h / 2 {
        let uv = &mut out.uv[row * w..row * w + w];
        let (u, v) = (&u[row * su..], &v[row * sv..]);
        for col in 0..w / 2 {
            uv[col * 2] = u[col];
            uv[col * 2 + 1] = v[col];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn zoomed_replicates_each_pixel_into_a_block() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let z = frame.zoomed(2, &pool);
        assert_eq!((z.width, z.height), (4, 2));
        let row = [1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 7, 8];
        assert_eq!(&z.pixels.as_ref()[..16], &row);
        assert_eq!(&z.pixels.as_ref()[16..], &row);
    }

    #[test]
    fn zoom_one_moves_the_pixels() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 1,
            height: 1,
            pixels: vec![9, 9, 9, 9],
        };
        let z = frame.zoomed(1, &pool);
        assert_eq!(
            (z.width, z.height, z.pixels.as_ref()),
            (1, 1, &[9u8, 9, 9, 9][..])
        );
    }

    #[test]
    fn a_zoom_past_the_address_space_leaves_the_frame_as_it_is() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let z = frame.zoomed(u32::MAX, &pool);
        assert_eq!(
            (z.width, z.height, z.pixels.as_ref()),
            (2, 1, &[1u8, 2, 3, 4, 5, 6, 7, 8][..])
        );
    }

    #[test]
    fn a_zoomed_frame_that_cannot_be_allocated_stays_unzoomed() {
        // One pixel at zoom two billion: every size fits its type, but the
        // 16 EB buffer is past isize::MAX, so the reservation fails
        // deterministically, before the allocator is asked.
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 1,
            height: 1,
            pixels: vec![7; 4],
        };
        let z = frame.zoomed(2_000_000_000, &pool);
        assert_eq!(
            (z.width, z.height, z.pixels.as_ref()),
            (1, 1, &[7u8; 4][..])
        );
    }

    #[test]
    fn dropped_frames_return_their_buffers_to_the_pool() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 2,
            height: 2,
            pixels: vec![0; 16],
        };
        let z = frame.zoomed(2, &pool);
        assert_eq!(pool.len(), 0);
        drop(z);
        assert_eq!(pool.len(), 1);
        let small = pool.take(16).unwrap();
        assert_eq!(
            (small.len(), pool.len()),
            (16, 0),
            "a fitting buffer is reused"
        );
        drop(small);
        pool.give(vec![0; 8]);
        let big = pool.take(64).unwrap();
        assert_eq!((big.len(), pool.len()), (64, 1), "a too-small buffer stays");
    }

    #[test]
    fn a_full_pool_keeps_the_largest_buffers() {
        let pool = PixelPool::default();
        for _ in 0..POOL_KEEP {
            pool.give(vec![0; 8]);
        }
        pool.give(vec![0; 64]);
        assert_eq!(pool.len(), POOL_KEEP);
        assert_eq!(
            pool.take(64).unwrap().len(),
            64,
            "the large buffer replaced a small one"
        );
        pool.give(vec![0; 8]);
        pool.give(vec![0; 4]);
        assert!(
            pool.0.lock().unwrap().iter().all(|b| b.capacity() >= 8),
            "a smaller one is dropped"
        );
    }

    use super::*;
    use gliff_proto::chroma::{nv12_to_yuv444, yuv444_to_nv12};
    use gliff_proto::color::{bgra_to_yuv444, psnr, yuv444_to_bgra};

    fn synthetic_bgra(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
                let stripe = ((x + t * 3) / 32) % 4;
                let (b, g, r) = match stripe {
                    0 => (255, 0, 0),
                    1 => (0, 255, 0),
                    2 => (0, 0, 255),
                    _ => ((x * 255 / w) as u8, (y * 255 / h) as u8, 128),
                };
                p[0] = b;
                p[1] = g;
                p[2] = r;
                p[3] = 255;
            }
        }
        out
    }

    /// Dense, high-contrast detail, like small text.
    fn detailed_bgra(w: usize, h: usize) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut out = vec![0u8; w * h * 4];
        for px in out.chunks_exact_mut(4) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let v = if seed.is_multiple_of(3) { 230 } else { 20 };
            px.copy_from_slice(&[v, v, v, 255]);
        }
        out
    }

    fn rgb_channels(bgra: &[u8]) -> Vec<u8> {
        bgra.chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect()
    }

    /// What the lossless CPU reference makes of the same pixels in the same
    /// chroma mode, so 4:2:0's inherent loss is not counted against the codec.
    fn reference_bgra(src: &[u8], w: usize, h: usize, dual: bool) -> Vec<u8> {
        let yuv = bgra_to_yuv444(src, w * 4, w, h);
        if dual {
            yuv444_to_bgra(&yuv)
        } else {
            yuv444_to_bgra(&nv12_to_yuv444(&yuv444_to_nv12(&yuv)))
        }
    }

    /// Pictures come out in order but one access unit late, so outputs are
    /// compared against the sources in submission order.
    fn roundtrip(dual: bool) {
        let (w, h) = (640u32, 360u32);
        let settings = EncoderSettings {
            width: w,
            height: h,
            bitrate: 20_000_000,
            framerate: 60,
        };
        let mut encoder = Encoder::new(settings, dual).expect("encoder");
        let mut decoder = Decoder::new(dual, w, h).expect("decoder");
        let mut sources = Vec::new();
        let mut outputs = Vec::new();
        for i in 0..10 {
            let src = synthetic_bgra(w as usize, h as usize, i);
            let force = i == 5;
            let packet = encoder.encode_bgra(&src, force).expect("encode");
            if i == 0 || force {
                assert!(packet.keyframe, "frame {i} should be a keyframe");
            }
            assert_eq!(packet.aux.is_some(), dual);
            sources.push(src);
            let aux = packet.aux.as_deref().unwrap_or(&[]);
            if let Some(out) = decoder.decode(&packet.main, aux).expect("decode") {
                outputs.push(out);
            }
        }
        if let Some(out) = decoder.flush().expect("flush") {
            outputs.push(out);
        }
        assert!(
            outputs.len() >= 9,
            "decoded only {}/10 frames",
            outputs.len()
        );
        let mut min_psnr = f64::MAX;
        for (src, out) in sources.iter().zip(&outputs) {
            assert_eq!((out.width, out.height), (w, h));
            let reference = reference_bgra(src, w as usize, h as usize, dual);
            min_psnr = min_psnr.min(psnr(&rgb_channels(&reference), &rgb_channels(&out.pixels)));
        }
        assert!(min_psnr > 30.0, "min RGB PSNR {min_psnr:.1} dB too low");
    }

    #[test]
    fn dual420_roundtrip_is_high_quality() {
        roundtrip(true);
    }

    #[test]
    fn single420_roundtrip_is_high_quality() {
        roundtrip(false);
    }

    #[test]
    fn set_bitrate_keeps_the_stream_decodable() {
        let settings = EncoderSettings {
            width: 320,
            height: 240,
            bitrate: 8_000_000,
            framerate: 60,
        };
        let mut encoder = Encoder::new(settings, false).expect("encoder");
        let mut decoder = Decoder::new(false, 320, 240).expect("decoder");
        let mut decoded = 0;
        for i in 0..6 {
            if i == 3 {
                encoder.set_bitrate(2_000_000);
            }
            let src = synthetic_bgra(320, 240, i);
            let packet = encoder.encode_bgra(&src, false).expect("encode");
            if decoder.decode(&packet.main, &[]).expect("decode").is_some() {
                decoded += 1;
            }
        }
        assert!(decoded >= 5, "decoded only {decoded}/6 frames");
    }

    #[test]
    fn detailed_change_beats_the_default_screen_qp_floor() {
        let (w, h) = (640usize, 360usize);
        let settings = EncoderSettings {
            width: w as u32,
            height: h as u32,
            bitrate: (w * h * 60 / 10) as u32,
            framerate: 60,
        };
        let mut encoder = Encoder::new(settings, true).expect("encoder");
        let mut decoder = Decoder::new(true, w as u32, h as u32).expect("decoder");
        let dark = vec![16u8; w * h * 4];
        let detail = detailed_bgra(w, h);
        let mut last = None;
        for src in [&dark, &detail, &detail] {
            let packet = encoder.encode_bgra(src, false).expect("encode");
            let aux = packet.aux.as_deref().unwrap_or(&[]);
            last = decoder.decode(&packet.main, aux).expect("decode").or(last);
        }
        last = decoder.flush().expect("flush").or(last);
        let out = last.expect("a decoded picture");
        let reference = reference_bgra(&detail, w, h, true);
        let p = psnr(&rgb_channels(&reference), &rgb_channels(&out.pixels));
        assert!(p > 34.5, "detailed change PSNR {p:.1} dB");
    }

    #[test]
    fn picture_size_must_match_the_stream() {
        assert!(check_picture_size((640, 360), (640, 360)).is_ok());
        // An odd picture is cropped to even before conversion.
        assert!(check_picture_size((641, 361), (640, 360)).is_ok());
        assert!(check_picture_size((16, 16), (640, 360)).is_err());
        assert!(check_picture_size((640, 368), (640, 360)).is_err());
        // Empty pictures never convert, whatever was announced.
        assert!(check_picture_size((1, 1), (0, 0)).is_err());
        assert!(check_picture_size((0, 360), (0, 360)).is_err());
    }

    fn is_size_error<T>(result: &Result<T>) -> bool {
        matches!(result, Err(Error::StreamSize(_)))
    }

    fn keyframe(width: u32, height: u32, dual: bool) -> EncodedFrame {
        let settings = EncoderSettings {
            width,
            height,
            bitrate: 4_000_000,
            framerate: 60,
        };
        let mut encoder = Encoder::new(settings, dual).expect("encoder");
        let src = synthetic_bgra(width as usize, height as usize, 0);
        encoder.encode_bgra(&src, true).expect("encode")
    }

    /// A stream smaller than its StreamConfig is a size error in both chroma
    /// modes, never a frame of the wrong size.
    #[test]
    fn an_undersized_stream_is_a_size_error() {
        for dual in [false, true] {
            let packet = keyframe(320, 240, dual);
            let mut decoder = Decoder::new(dual, 640, 480).expect("decoder");
            let aux = packet.aux.as_deref().unwrap_or(&[]);
            let result = decoder.decode(&packet.main, aux);
            assert!(is_size_error(&result), "dual={dual}: {:?}", result.err());
        }
    }

    /// A Dual420 pair whose main picture matches but whose aux is smaller.
    #[test]
    fn an_undersized_aux_picture_is_a_size_error() {
        let main = keyframe(640, 480, true).main;
        let aux = keyframe(320, 240, true).aux.expect("aux");
        let mut decoder = Decoder::new(true, 640, 480).expect("decoder");
        let result = decoder.decode(&main, &aux);
        assert!(is_size_error(&result), "{:?}", result.err());
    }

    /// x264 High profile, 64x64: its first picture stays in OpenH264's
    /// reorder buffer until a flush, unlike our Baseline streams.
    const HIGH_PROFILE: &[u8] = include_bytes!("../../gliff-va/src/h264/testdata/x264-64x64.264");

    /// The parameter sets and IDR picture of `HIGH_PROFILE`, up to the
    /// start code of its P slice (NAL type 1).
    fn high_profile_idr() -> &'static [u8] {
        let p_slice = HIGH_PROFILE
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 1)
            .expect("a P slice");
        let start = if p_slice > 0 && HIGH_PROFILE[p_slice - 1] == 0 {
            p_slice - 1
        } else {
            p_slice
        };
        &HIGH_PROFILE[..start]
    }

    /// A picture that only comes out at flush is checked like any other: it
    /// converts at the announced size and is a size error at another.
    #[test]
    fn a_picture_released_at_flush_is_size_checked() {
        let idr = high_profile_idr();
        for dual in [false, true] {
            let aux = if dual { idr } else { &[][..] };
            let mut decoder = Decoder::new(dual, 64, 64).expect("decoder");
            assert!(decoder.decode(idr, aux).expect("decode").is_none());
            assert!(decoder.has_pending());
            let frame = decoder.flush().expect("flush").expect("a picture");
            assert_eq!((frame.width, frame.height), (64, 64));

            let mut decoder = Decoder::new(dual, 128, 128).expect("decoder");
            assert!(decoder.decode(idr, aux).expect("decode").is_none());
            let result = decoder.flush();
            assert!(is_size_error(&result), "dual={dual}: {:?}", result.err());
            // The refused picture is gone, so the idle drain stops.
            assert!(!decoder.has_pending());
        }
    }

    #[test]
    fn video_mode_resolves_cli_over_env() {
        assert_eq!(VideoMode::resolve(Some("cpu")), VideoMode::Cpu);
        assert_eq!(VideoMode::resolve(Some("gpu")), VideoMode::Gpu);
    }
}
