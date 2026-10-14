//! The two ends of the stream as one object each.
//!
//! Server: captured dmabuf -> (import) -> split compute -> H.264 encode x2.
//! Client: H.264 decode x2 -> recombine compute -> BGRA dmabuf for display.
//!
//! Both use `Dual420` (main + aux streams, full 4:4:4) or `Single420` (main
//! only). Compute and video work are ordered on the GPU with a timeline
//! semaphore; the CPU waits once per frame, for the encoded bytes or the
//! finished display image.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use ash::vk;

use crate::compute::{Recombine, Split};
use crate::device::{Commands, Gpu, Timeline};
use crate::image::{DmabufPlane, ExportedDmabuf, HostBuffer, Image};
use crate::vkenc::{self, VkH264Encoder};
use crate::Result;
use gliff_va::{EncodedPacket, EncoderSettings, H264Decoder, H264Encoder};

pub struct EncodedFrame {
    pub main: Vec<u8>,
    pub aux: Option<Vec<u8>>,
    pub keyframe: bool,
}

/// One H.264 stream: the encoder and its input image as the split shader
/// sees it.
enum Stream {
    /// VA-API encode. The shader writes the encoder's input surface,
    /// imported into Vulkan, or a scratch image copied into it when the
    /// driver refused STORAGE on the import.
    Va {
        enc: Box<H264Encoder>,
        target: Image,
        scratch: Option<Image>,
    },
    /// Vulkan Video encode. Its input image cannot take STORAGE, so the
    /// shader writes a scratch image that is copied in.
    Vk {
        enc: Box<VkH264Encoder>,
        scratch: Image,
    },
}

enum Pending {
    Va(gliff_va::PendingEncode),
    Vk(vkenc::PendingEncode),
}

impl Stream {
    /// `index` picks the encode queue on the Vulkan path (0 main, 1 aux).
    fn new(gpu: &Arc<Gpu>, settings: &EncoderSettings, index: usize) -> Result<Self> {
        if !gpu.can_va_encode() && gpu.video_encode.is_some() {
            let enc = VkH264Encoder::new(gpu, settings.clone(), index)?;
            let input = enc.input();
            let scratch = Image::nv12(gpu, input.width, input.height)?;
            return Ok(Self::Vk {
                enc: Box::new(enc),
                scratch,
            });
        }
        let enc = H264Encoder::new(&gpu.va, &gpu.va_caps, settings.clone())?;
        let desc = enc.input().export()?;
        let (target, scratch) = match Image::import_nv12(gpu, &desc, true) {
            Ok(img) => (img, None),
            Err(e) => {
                tracing::info!(error = %e, "encoder input refuses STORAGE; the split is copied in");
                let target = Image::import_nv12(gpu, &desc, false)?;
                let scratch = Image::nv12(gpu, target.width, target.height)?;
                (target, Some(scratch))
            }
        };
        Ok(Self::Va {
            enc: Box::new(enc),
            target,
            scratch,
        })
    }

    /// The image the split shader writes.
    fn shader_output(&self) -> &Image {
        match self {
            Self::Va {
                target, scratch, ..
            } => scratch.as_ref().unwrap_or(target),
            Self::Vk { scratch, .. } => scratch,
        }
    }

    /// Before the split: take the shader's output back from VA-API, or
    /// ready the scratch image.
    fn record_acquire(&self, cmd: vk::CommandBuffer) {
        match self {
            Self::Va {
                target,
                scratch: None,
                ..
            } => target.acquire_foreign(cmd, vk::ImageLayout::GENERAL),
            Self::Va {
                scratch: Some(s), ..
            }
            | Self::Vk { scratch: s, .. } => s.transition(cmd, vk::ImageLayout::GENERAL),
        }
    }

    /// After the split: move a scratch image into the encoder's input if
    /// needed, then hand a VA-API surface to VA-API. The Vulkan encoder's
    /// input stays in TRANSFER_DST for the encode submission to take.
    fn record_release(&self, cmd: vk::CommandBuffer) {
        match self {
            Self::Va {
                target, scratch, ..
            } => {
                if let Some(s) = scratch {
                    s.transition(cmd, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
                    target.acquire_foreign(cmd, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                    target.copy_nv12_from(cmd, s);
                }
                target.release_foreign(cmd);
            }
            Self::Vk { enc, scratch } => {
                scratch.transition(cmd, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
                let input = enc.input();
                input.discard();
                input.transition(cmd, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                input.copy_nv12_from(cmd, scratch);
            }
        }
    }

    fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        match self {
            Self::Va { enc, .. } => enc.set_rate(bitrate, framerate, vbv_ms),
            Self::Vk { enc, .. } => enc.set_rate(bitrate, framerate, vbv_ms),
        }
    }

    fn submit(&mut self, force_keyframe: bool) -> Result<Pending> {
        Ok(match self {
            Self::Va { enc, .. } => Pending::Va(enc.submit(force_keyframe)?),
            Self::Vk { enc, .. } => Pending::Vk(enc.submit(force_keyframe)?),
        })
    }

    fn finish(&mut self, pending: Pending) -> Result<EncodedPacket> {
        Ok(match (self, pending) {
            (Self::Va { enc, .. }, Pending::Va(p)) => enc.finish(p)?,
            (Self::Vk { enc, .. }, Pending::Vk(p)) => enc.finish(p)?,
            _ => unreachable!("a stream finishes what it submitted"),
        })
    }
}

/// Server side: one encoder object per session.
pub struct Encoder {
    gpu: Arc<Gpu>,
    timeline: Timeline,
    compute: Commands,
    split: Split,
    main: Stream,
    aux: Option<Stream>,
    /// Imported capture buffers, keyed by the caller's buffer id.
    imports: HashMap<u64, Image>,
    settings: EncoderSettings,
}

impl Encoder {
    /// The largest size the device encodes, as (width, height).
    pub fn max_size(gpu: &Gpu) -> Result<(u32, u32)> {
        if !gpu.can_va_encode() {
            if let Some(max) = VkH264Encoder::max_coded_extent(gpu) {
                return Ok(max);
            }
        }
        Ok(H264Encoder::max_coded_extent(&gpu.va_caps))
    }

    pub fn new(gpu: &Arc<Gpu>, settings: EncoderSettings, dual: bool) -> Result<Self> {
        let main = Stream::new(gpu, &settings, 0)?;
        let aux = if dual {
            Some(Stream::new(gpu, &settings, 1)?)
        } else {
            None
        };
        Ok(Self {
            gpu: gpu.clone(),
            timeline: Timeline::new(gpu)?,
            compute: Commands::new(gpu, gpu.families.compute, gpu.compute_queue)?,
            split: Split::new(gpu)?,
            main,
            aux,
            imports: HashMap::new(),
            settings,
        })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// Change the target bitrate of both streams from the next frame on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        let (fps, vbv) = (self.settings.framerate, self.settings.vbv_ms);
        self.set_rate(bitrate, fps, vbv);
    }

    /// Change the target bitrate, the frame rate it is spread over, and the
    /// rate-control buffer of both streams from the next frame on.
    pub fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        self.settings.bitrate = bitrate;
        self.settings.framerate = framerate;
        self.settings.vbv_ms = vbv_ms;
        self.main.set_rate(bitrate, framerate, vbv_ms);
        if let Some(a) = &mut self.aux {
            a.set_rate(bitrate, framerate, vbv_ms);
        }
    }

    /// Encode a captured dmabuf. `key` identifies the buffer so its import
    /// is reused across frames; pass a new key when the buffer changes.
    pub fn encode_dmabuf(
        &mut self,
        key: u64,
        plane: &DmabufPlane,
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        if !self.imports.contains_key(&key) {
            // A new capture ring means the old buffers are gone.
            if self.imports.len() >= 8 {
                self.compute.wait()?;
                self.imports.clear();
            }
            self.imports
                .insert(key, Image::import_dmabuf(&self.gpu, plane)?);
        }
        let src = self.imports.remove(&key).expect("inserted above");
        let result = self.encode_image(&src, force_keyframe);
        self.imports.insert(key, src);
        result
    }

    /// Encode packed BGRA pixels (tests and the probe; one extra upload).
    pub fn encode_bgra(&mut self, bgra: &[u8], force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = (self.settings.width, self.settings.height);
        let staging = HostBuffer::new(&self.gpu, bgra.len(), vk::BufferUsageFlags::TRANSFER_SRC)?;
        staging.write(0, bgra);
        let src = Image::bgra_upload(&self.gpu, w, h)?;
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                src.transition(cmd, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                src.copy_rgba_from_buffer(cmd, &staging);
                Ok(())
            })?;
        self.encode_image(&src, force_keyframe)
    }

    fn encode_image(&mut self, src: &Image, force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = (self.settings.width, self.settings.height);
        let (split, main, aux) = (&self.split, &self.main, self.aux.as_ref());
        // The split blocks until it is done: the encoder reads the surfaces
        // through VA-API, which knows nothing of Vulkan's fences, or on the
        // Vulkan encode queue, which then needs no semaphore.
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                src.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                main.record_acquire(cmd);
                if let Some(a) = aux {
                    a.record_acquire(cmd);
                }
                let main_out = main.shader_output();
                let aux_out = aux.map(Stream::shader_output);
                split.record(cmd, src, main_out, aux_out, w, h)?;
                main_out.memory_barrier(cmd);
                main.record_release(cmd);
                if let Some(a) = aux {
                    a.record_release(cmd);
                }
                Ok(())
            })?;
        // Submit both encodes, then wait: the main stream's readback overlaps
        // the aux encode on the GPU.
        let main_pending = self.main.submit(force_keyframe)?;
        let aux_pending = match &mut self.aux {
            Some(a) => Some(a.submit(force_keyframe)?),
            None => None,
        };
        let main = self.main.finish(main_pending)?;
        let aux = match (&mut self.aux, aux_pending) {
            (Some(a), Some(pending)) => Some(a.finish(pending)?),
            _ => None,
        };
        Ok(EncodedFrame {
            keyframe: main.keyframe,
            main: main.data,
            aux: aux.map(|a| a.data),
        })
    }
}

/// A finished display frame: a linear BGRX dmabuf the display side imports.
/// The fd is a fresh duplicate the receiver owns. Dropping the frame returns
/// its image to the decoder's ring, so keep it alive until the display side
/// has finished with the texture.
#[derive(Debug)]
pub struct DisplayFrame {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub fourcc: drm_fourcc::DrmFourcc,
    pub modifier: u64,
    /// Display pixels per stream pixel, see [`Decoder::set_zoom`].
    pub zoom: u32,
    ring: u64,
    index: usize,
    release: Sender<(u64, usize)>,
}

impl Drop for DisplayFrame {
    fn drop(&mut self) {
        let _ = self.release.send((self.ring, self.index));
    }
}

/// Display images kept by the client decoder: one being written, one on
/// screen, one in transit between the two.
const DISPLAY_RING: usize = 3;

/// Client side: one decoder object per stream.
pub struct Decoder {
    gpu: Arc<Gpu>,
    timeline: Timeline,
    compute: Commands,
    recombine: Recombine,
    main: DecodeStream,
    aux: Option<DecodeStream>,
    outputs: Vec<(Image, ExportedDmabuf)>,
    /// Counts output rings; a release from an older ring is ignored.
    ring: u64,
    /// Images handed out as `DisplayFrame`s and not yet dropped.
    busy: Vec<bool>,
    /// Busy images in hand-out order, oldest first.
    handed_out: std::collections::VecDeque<usize>,
    release_tx: Sender<(u64, usize)>,
    release_rx: Receiver<(u64, usize)>,
    /// CPU readback staging, allocated on first use (tests and the probe).
    readback: Option<HostBuffer>,
    width: u32,
    height: u32,
    zoom: u32,
    /// The last `decode_to_output` produced a picture pair that is complete
    /// and in shader-read layout, so `redraw` may sample it.
    last_complete: bool,
}

impl Decoder {
    pub fn new(gpu: &Arc<Gpu>, dual: bool, width: u32, height: u32) -> Result<Self> {
        let (release_tx, release_rx) = channel();
        let outputs = Self::output_ring(gpu, width, height)?;
        Ok(Self {
            gpu: gpu.clone(),
            timeline: Timeline::new(gpu)?,
            compute: Commands::new(gpu, gpu.families.compute, gpu.compute_queue)?,
            recombine: Recombine::new(gpu)?,
            main: DecodeStream::new(gpu)?,
            aux: if dual {
                Some(DecodeStream::new(gpu)?)
            } else {
                None
            },
            outputs,
            ring: 0,
            busy: vec![false; DISPLAY_RING],
            handed_out: std::collections::VecDeque::new(),
            release_tx,
            release_rx,
            readback: None,
            width,
            height,
            zoom: 1,
            last_complete: false,
        })
    }

    fn output_ring(
        gpu: &Arc<Gpu>,
        width: u32,
        height: u32,
    ) -> Result<Vec<(Image, ExportedDmabuf)>> {
        (0..DISPLAY_RING)
            .map(|_| {
                let img = Image::exportable_bgra(gpu, width, height)?;
                let dmabuf = img.export_dmabuf()?;
                Ok((img, dmabuf))
            })
            .collect()
    }

    /// Write each stream pixel as a `zoom` x `zoom` block, so the display
    /// side shows the frame at that integer scale with no resampling. The
    /// request is capped so the output fits the device's image size limit.
    /// Returns the zoom in effect. Frames already handed out keep their old
    /// images alive through their dmabuf fds.
    pub fn set_zoom(&mut self, zoom: u32) -> Result<u32> {
        // SAFETY: valid instance and physical device handles.
        let max_side = unsafe {
            self.gpu
                .instance
                .get_physical_device_properties(self.gpu.physical)
                .limits
                .max_image_dimension2_d
        };
        let cap = (max_side / self.width.max(1)).min(max_side / self.height.max(1));
        let zoom = zoom.clamp(1, cap.max(1));
        if zoom == self.zoom {
            return Ok(zoom);
        }
        self.compute.wait()?;
        let outputs = Self::output_ring(&self.gpu, self.width * zoom, self.height * zoom)?;
        self.outputs = outputs;
        self.ring += 1;
        self.busy = vec![false; DISPLAY_RING];
        self.handed_out.clear();
        self.readback = None;
        self.zoom = zoom;
        Ok(zoom)
    }

    pub fn zoom(&self) -> u32 {
        self.zoom
    }

    /// Recombine the last decoded picture again into a fresh display frame,
    /// as after a zoom change on a still screen. `None` before the first
    /// complete picture, or after a decode that failed part way.
    pub fn redraw(&mut self) -> Result<Option<DisplayFrame>> {
        if !self.last_complete {
            return Ok(None);
        }
        let idx = self.free_output();
        let (dst, _) = &self.outputs[idx];
        let (recombine, w, h, zoom) = (&self.recombine, self.width, self.height, self.zoom);
        let Some(main_img) = self.main.last_image() else {
            return Ok(None);
        };
        let aux_img = match &self.aux {
            Some(a) => match a.last_image() {
                Some(img) => Some(img),
                None => return Ok(None),
            },
            None => None,
        };
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                main_img.acquire_foreign(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                if let Some(a) = aux_img {
                    a.acquire_foreign(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                }
                dst.transition(cmd, vk::ImageLayout::GENERAL);
                recombine.record(cmd, main_img, aux_img, dst, (w, h), zoom)?;
                dst.memory_barrier(cmd);
                main_img.release_foreign(cmd);
                if let Some(a) = aux_img {
                    a.release_foreign(cmd);
                }
                Ok(())
            })?;
        Ok(Some(self.hand_out(idx)?))
    }

    fn hand_out(&mut self, idx: usize) -> Result<DisplayFrame> {
        let (_, dmabuf) = &self.outputs[idx];
        self.busy[idx] = true;
        self.handed_out.push_back(idx);
        Ok(DisplayFrame {
            fd: dmabuf.fd.as_fd().try_clone_to_owned()?,
            width: dmabuf.width,
            height: dmabuf.height,
            stride: dmabuf.stride,
            offset: dmabuf.offset,
            fourcc: dmabuf.fourcc,
            modifier: dmabuf.modifier,
            zoom: self.zoom,
            ring: self.ring,
            index: idx,
            release: self.release_tx.clone(),
        })
    }

    /// Decode one access unit pair and recombine to a display frame. Blocks
    /// until the frame is complete. `None` when the unit had no picture.
    pub fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<DisplayFrame>> {
        let idx = self.decode_to_output(main, aux)?;
        let Some(idx) = idx else { return Ok(None) };
        Ok(Some(self.hand_out(idx)?))
    }

    /// Decode and read the BGRA pixels back to the CPU (tests and the probe).
    pub fn decode_to_bgra(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(idx) = self.decode_to_output(main, aux)? else {
            return Ok(None);
        };
        let (image, _) = &self.outputs[idx];
        let (w, h) = (
            self.width as usize * self.zoom as usize,
            self.height as usize * self.zoom as usize,
        );
        let size = w * h * 4;
        if self.readback.is_none() {
            self.readback = Some(HostBuffer::new(
                &self.gpu,
                size,
                vk::BufferUsageFlags::TRANSFER_DST,
            )?);
        }
        let buf = self.readback.as_ref().expect("allocated above");
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                image.copy_rgba_to_buffer(cmd, buf);
                Ok(())
            })?;
        Ok(Some(buf.read(0, size)))
    }

    /// An output image no `DisplayFrame` holds. Waits briefly for a release
    /// when all are out; a display that never releases gets the oldest reused.
    fn free_output(&mut self) -> usize {
        loop {
            while let Ok(r) = self.release_rx.try_recv() {
                self.mark_free(r);
            }
            if let Some(i) = self.busy.iter().position(|b| !b) {
                return i;
            }
            match self.release_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => self.mark_free(r),
                Err(_) => {
                    // Reclaim only the image handed out longest ago: it is the
                    // one least likely to still be on screen.
                    if let Some(i) = self.handed_out.pop_front() {
                        tracing::warn!(image = i, "display frame not released; reusing the oldest");
                        self.busy[i] = false;
                    }
                }
            }
        }
    }

    fn mark_free(&mut self, (ring, i): (u64, usize)) {
        if ring != self.ring {
            return;
        }
        self.busy[i] = false;
        self.handed_out.retain(|&h| h != i);
    }

    fn decode_to_output(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<usize>> {
        let t0 = std::time::Instant::now();
        self.last_complete = false;
        let idx = self.free_output();
        let Some(main_idx) = self.main.decode(main)? else {
            return Ok(None);
        };
        let t_main = t0.elapsed();
        let aux_idx = match &mut self.aux {
            Some(dec) => match dec.decode(aux)? {
                Some(i) => Some(i),
                None => return Ok(None),
            },
            None => None,
        };
        let t_aux = t0.elapsed() - t_main;
        // The decoders run through VA-API, which knows nothing of Vulkan's
        // semaphores: wait for their surfaces before the recombine reads them.
        self.main.wait(main_idx)?;
        if let (Some(dec), Some(i)) = (&self.aux, aux_idx) {
            dec.wait(i)?;
        }
        let t_wait = t0.elapsed() - t_main - t_aux;
        let main_img = &self.main.images[main_idx];
        let aux_img = self.aux.as_ref().zip(aux_idx).map(|(d, i)| &d.images[i]);
        let (dst, _) = &self.outputs[idx];
        let (recombine, w, h, zoom) = (&self.recombine, self.width, self.height, self.zoom);
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                main_img.acquire_foreign(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                if let Some(a) = aux_img {
                    a.acquire_foreign(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                }
                dst.transition(cmd, vk::ImageLayout::GENERAL);
                recombine.record(cmd, main_img, aux_img, dst, (w, h), zoom)?;
                dst.memory_barrier(cmd);
                main_img.release_foreign(cmd);
                if let Some(a) = aux_img {
                    a.release_foreign(cmd);
                }
                Ok(())
            })?;
        self.last_complete = true;
        tracing::debug!(
            submit_main_us = t_main.as_micros(),
            submit_aux_us = t_aux.as_micros(),
            wait_us = t_wait.as_micros(),
            total_us = t0.elapsed().as_micros(),
            "decode + recombine"
        );
        Ok(Some(idx))
    }
}

/// One H.264 stream on the client: the VA-API decoder and its output
/// surfaces as the recombine shader sees them.
struct DecodeStream {
    gpu: Arc<Gpu>,
    dec: H264Decoder,
    images: Vec<Image>,
    generation: u64,
}

impl DecodeStream {
    fn new(gpu: &Arc<Gpu>) -> Result<Self> {
        Ok(Self {
            gpu: gpu.clone(),
            dec: H264Decoder::new(&gpu.va, &gpu.va_caps)?,
            images: Vec::new(),
            generation: 0,
        })
    }

    /// Decode one access unit; the index names a surface in `images`.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<usize>> {
        let idx = self.dec.decode(access_unit)?;
        if self.dec.generation() != self.generation {
            self.images = self
                .dec
                .surfaces()
                .iter()
                .map(|s| Image::import_nv12(&self.gpu, &s.export()?, false))
                .collect::<Result<Vec<_>>>()?;
            self.generation = self.dec.generation();
        }
        Ok(idx)
    }

    fn wait(&self, idx: usize) -> Result<()> {
        Ok(self.dec.surfaces()[idx].sync()?)
    }

    fn last_image(&self) -> Option<&Image> {
        self.dec.last_output().and_then(|i| self.images.get(i))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.compute.wait();
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.compute.wait();
    }
}

/// How the split shader's output reaches a VA-API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfacePath {
    /// The shader writes the imported surface directly.
    Storage,
    /// The shader writes a Vulkan image that is then copied into the surface.
    Copy,
}

/// Probe helper: import an exported VA-API surface and write the 4:2:0
/// split of `bgra` (`width` x `height`) into it, taking the storage path
/// when the driver allows it. Returns the path used; the caller reads the
/// surface back through VA-API to check the pixels.
pub fn split_into_surface(
    gpu: &Arc<Gpu>,
    desc: &gliff_va::PrimeDescriptor,
    bgra: &[u8],
    width: u32,
    height: u32,
) -> Result<SurfacePath> {
    let (target, path) = match Image::import_nv12(gpu, desc, true) {
        Ok(img) => (img, SurfacePath::Storage),
        Err(e) => {
            tracing::info!(error = %e, "surface import with STORAGE refused; using a copy");
            (Image::import_nv12(gpu, desc, false)?, SurfacePath::Copy)
        }
    };
    let staging = HostBuffer::new(gpu, bgra.len(), vk::BufferUsageFlags::TRANSFER_SRC)?;
    staging.write(0, bgra);
    let src = Image::bgra_upload(gpu, width, height)?;
    let scratch = match path {
        SurfacePath::Storage => None,
        SurfacePath::Copy => Some(Image::nv12(gpu, target.width, target.height)?),
    };
    let timeline = Timeline::new(gpu)?;
    let compute = Commands::new(gpu, gpu.families.compute, gpu.compute_queue)?;
    let split = Split::new(gpu)?;
    compute.run(timeline.semaphore, None, None, true, |cmd| {
        src.transition(cmd, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        src.copy_rgba_from_buffer(cmd, &staging);
        src.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let out = scratch.as_ref().unwrap_or(&target);
        out.transition(cmd, vk::ImageLayout::GENERAL);
        split.record(cmd, &src, out, None, width, height)?;
        out.memory_barrier(cmd);
        if let Some(s) = &scratch {
            s.transition(cmd, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
            target.transition(cmd, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
            target.copy_nv12_from(cmd, s);
        }
        target.transition(cmd, vk::ImageLayout::GENERAL);
        Ok(())
    })?;
    compute.wait()?;
    Ok(path)
}
