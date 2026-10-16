//! H.264 decode through VA-API.
//!
//! The application supplies what the hardware does not parse: the SPS and
//! PPS fields, the per-slice header fields, picture order counts, the
//! reference lists and the DPB state. Every DPB slot is a driver-owned
//! surface that is also the decode output, plus one spare for pictures that
//! are not references; the caller reads them through their exported dmabufs.

use std::sync::Arc;

use crate::bindings as va;
use crate::context::{Buffer, Config, Context};
use crate::display::{Caps, Display};
use crate::h264::annexb::{nal_units, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS};
use crate::h264::parser::{
    parse_pps, parse_slice_header, parse_sps, Mmco, Pps, ScalingLists, SliceHeader, SliceType, Sps,
};
use crate::surface::{Surface, UsageHint};
use crate::{Error, Result};

#[derive(Clone, Copy)]
struct DpbPicture {
    frame_num: u32,
    poc: i32,
}

struct Stream {
    sps: Sps,
    pps: Pps,
    context: Context,
    /// One per DPB slot, then the spare.
    surfaces: Vec<Surface>,
    slots: Vec<Option<DpbPicture>>,
    /// The surface the last `decode` returned, valid until the next one.
    last_output: Option<usize>,
}

pub struct H264Decoder {
    display: Arc<Display>,
    config: Config,
    max_width: u32,
    max_height: u32,
    /// The display size the stream was announced with (`StreamConfig`).
    expected: (u32, u32),
    sps_bytes: Vec<u8>,
    pps_bytes: Vec<u8>,
    stream: Option<Stream>,
    /// Bumped whenever the surfaces are replaced (a new SPS).
    generation: u64,
    poc: PocState,
}

/// Picture order count derivation state (8.2.1), POC types 0 and 2.
#[derive(Default)]
struct PocState {
    prev_msb: i32,
    prev_lsb: u32,
    prev_frame_num_offset: u32,
    prev_frame_num: u32,
}

impl H264Decoder {
    /// A decoder for a stream of `width` x `height` display pixels. An SPS
    /// of any other size is refused, see [`check_stream_size`].
    pub fn new(display: &Arc<Display>, caps: &Caps, width: u32, height: u32) -> Result<Self> {
        caps.can_decode()?;
        let config = Config::new(
            display,
            Caps::PROFILE,
            va::VAEntrypointVLD,
            &[(va::VAConfigAttribRTFormat, va::VA_RT_FORMAT_YUV420)],
        )?;
        let or = |v: u32| if v == 0 { 4096 } else { v };
        Ok(Self {
            display: display.clone(),
            config,
            max_width: or(caps.decode_max_width),
            max_height: or(caps.decode_max_height),
            expected: (width, height),
            sps_bytes: Vec::new(),
            pps_bytes: Vec::new(),
            stream: None,
            generation: 0,
            poc: PocState::default(),
        })
    }

    /// Display size of the current stream, once an SPS has been seen.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        self.stream.as_ref().map(|s| s.sps.display_size())
    }

    /// The surfaces pictures land in; replaced when `generation` changes.
    pub fn surfaces(&self) -> &[Surface] {
        self.stream.as_ref().map_or(&[], |s| s.surfaces.as_slice())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The surface the last `decode` returned, if any.
    pub fn last_output(&self) -> Option<usize> {
        self.stream.as_ref().and_then(|s| s.last_output)
    }

    /// Decode one access unit. Returns the index of the output surface once
    /// the decode has been submitted; `sync` it before reading. `None` for
    /// an access unit with no picture (parameter sets only).
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<usize>> {
        let nals = nal_units(access_unit);
        let mut slices = Vec::new();
        let mut headers = Vec::new();
        for nal in &nals {
            match nal.nal_type {
                NAL_SPS => {
                    if nal.data != self.sps_bytes.as_slice() {
                        let sps = parse_sps(nal.data)?;
                        // Remember the SPS only once it is in use, so a
                        // refused one is checked again when it repeats.
                        self.sps_bytes.clear();
                        self.pps_bytes.clear();
                        self.open_stream(sps)?;
                        self.sps_bytes = nal.data.to_vec();
                    }
                }
                NAL_PPS => {
                    if nal.data != self.pps_bytes.as_slice() {
                        let sps = self.stream.as_ref().map(|s| s.sps.clone());
                        let pps = parse_pps(nal.data, |_| sps.clone())?;
                        self.pps_bytes = nal.data.to_vec();
                        let stream = self
                            .stream
                            .as_mut()
                            .ok_or(Error::Bitstream("PPS before SPS"))?;
                        stream.pps = pps;
                    }
                }
                NAL_SLICE | NAL_IDR => {
                    let stream = self
                        .stream
                        .as_ref()
                        .ok_or(Error::Bitstream("slice before parameter sets"))?;
                    headers.push(parse_slice_header(nal.data, |_| {
                        Some((stream.pps.clone(), stream.sps.clone()))
                    })?);
                    slices.push(nal.data);
                }
                _ => {}
            }
        }
        let Some(header) = headers.first() else {
            return Ok(None);
        };
        if header.slice_type == Some(SliceType::B) {
            return Err(Error::Bitstream("B slices are not supported"));
        }
        if header.long_term_reference {
            return Err(Error::Bitstream(
                "long-term reference marking is not supported",
            ));
        }
        let stream = self.stream.as_mut().ok_or(Error::Bitstream("no stream"))?;

        let poc = self.poc.derive(&stream.sps, header)?;
        let is_ref = header.is_reference();
        if header.is_idr() {
            stream.slots.iter_mut().for_each(|s| *s = None);
        }
        // The reconstructed picture needs a free slot before decoding; the
        // marking process (which frees old references) runs after it.
        let setup_slot = if is_ref {
            let free = stream.slots.iter().position(Option::is_none);
            Some(match free {
                Some(i) => i,
                None => {
                    evict_oldest(&mut stream.slots, header.frame_num, &stream.sps);
                    stream
                        .slots
                        .iter()
                        .position(Option::is_none)
                        .ok_or(Error::Bitstream("no free DPB slot"))?
                }
            })
        } else {
            None
        };
        let output = setup_slot.unwrap_or(stream.slots.len());
        let refs: Vec<(usize, DpbPicture)> = stream
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|p| (i, p)))
            .collect();

        let current = DpbPicture {
            frame_num: header.frame_num,
            poc,
        };
        let mut buffers = vec![
            Buffer::new(
                &stream.context,
                va::VAPictureParameterBufferType,
                &picture_params(stream, &refs, output, current, header, is_ref),
            )?,
            Buffer::new(
                &stream.context,
                va::VAIQMatrixBufferType,
                &iq_matrix(&stream.sps, &stream.pps)?,
            )?,
        ];
        for (nal, sh) in slices.iter().zip(&headers) {
            let list0 = ref_list0(stream, &refs, sh)?;
            buffers.push(Buffer::new(
                &stream.context,
                va::VASliceParameterBufferType,
                &slice_params(nal, sh, &list0),
            )?);
            buffers.push(Buffer::data(
                &stream.context,
                va::VASliceDataBufferType,
                nal,
            )?);
        }
        stream.context.render(&stream.surfaces[output], &buffers)?;

        if let Some(slot) = setup_slot {
            stream.slots[slot] = Some(current);
            match &header.mmcos {
                Some(ops) => apply_mmco(
                    &mut stream.slots,
                    ops,
                    header.frame_num,
                    stream.sps.max_frame_num(),
                )?,
                None => sliding_window(&mut stream.slots, header.frame_num, &stream.sps),
            }
        }
        // prevFrameNum is the previous picture in decoding order, reference
        // or not (8.2.1); an MMCO 5 makes the current picture look like
        // frame 0 to the next one.
        let mmco5 = header
            .mmcos
            .as_ref()
            .is_some_and(|ops| ops.iter().any(|m| m.op == 5));
        self.poc.prev_frame_num = if mmco5 {
            self.poc.prev_frame_num_offset = 0;
            0
        } else {
            header.frame_num
        };
        stream.last_output = Some(output);
        Ok(Some(output))
    }

    fn open_stream(&mut self, sps: Sps) -> Result<()> {
        // Drop the old stream first: after a refused SPS, slices must not
        // decode against the previous one.
        self.stream = None;
        check_stream_size(&sps, self.expected)?;
        let (cw, ch) = sps.coded_size();
        if cw == 0 || ch == 0 || cw > self.max_width || ch > self.max_height {
            return Err(Error::Unsupported(format!(
                "stream size {cw}x{ch} exceeds the decoder limit {}x{}",
                self.max_width, self.max_height
            )));
        }
        // One slot per reference the stream may hold, plus one for the
        // picture being decoded before the marking process frees a slot.
        let refs = (sps.max_num_ref_frames as usize).clamp(1, 16);
        let dpb_slots = refs + 1;
        let surfaces = (0..dpb_slots + 1)
            .map(|_| Surface::new_nv12(&self.display, cw, ch, UsageHint::Decoder))
            .collect::<Result<Vec<_>>>()?;
        let targets: Vec<&Surface> = surfaces.iter().collect();
        let context = Context::new(&self.display, &self.config, cw, ch, &targets)?;
        tracing::info!(
            coded = format!("{cw}x{ch}"),
            dpb_slots,
            "va-api decoder stream opened"
        );
        self.stream = Some(Stream {
            sps,
            pps: Pps::default(),
            context,
            surfaces,
            slots: vec![None; dpb_slots],
            last_output: None,
        });
        self.generation += 1;
        self.poc = PocState::default();
        Ok(())
    }
}

/// The largest coded side accepted from any encoder, the client's decode
/// maximum. The driver limit in `open_stream` applies as well.
const MAX_CODED_SIDE: u32 = 4096;

/// Refuse an SPS whose picture is not the announced `expected` size: the
/// cropped display size must match exactly. Encoders other than ours may pad
/// the coded size beyond whole macroblocks, so any coded size from the
/// display size up to `MAX_CODED_SIDE` is accepted. The recombine shader
/// reads the announced size from the decoded surfaces, and the pipeline
/// checks that they cover it.
fn check_stream_size(sps: &Sps, expected: (u32, u32)) -> Result<()> {
    let display = sps.display_size();
    let coded = sps.coded_size();
    let fits = display.0 > 0
        && display.1 > 0
        && display == expected
        && coded.0 >= display.0
        && coded.1 >= display.1
        && coded.0 <= MAX_CODED_SIDE
        && coded.1 <= MAX_CODED_SIDE;
    if !fits {
        return Err(Error::StreamSize(format!(
            "picture {}x{} (coded {}x{}) does not match the announced {}x{}",
            display.0, display.1, coded.0, coded.1, expected.0, expected.1
        )));
    }
    Ok(())
}

fn va_picture(surface: &Surface, p: DpbPicture, flags: u32) -> va::VAPictureH264 {
    va::VAPictureH264 {
        picture_id: surface.id,
        frame_idx: p.frame_num,
        flags,
        TopFieldOrderCnt: p.poc,
        BottomFieldOrderCnt: p.poc,
        va_reserved: [0; 4],
    }
}

fn invalid_picture() -> va::VAPictureH264 {
    va::VAPictureH264 {
        picture_id: va::VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: va::VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    }
}

fn picture_params(
    stream: &Stream,
    refs: &[(usize, DpbPicture)],
    output: usize,
    current: DpbPicture,
    header: &SliceHeader,
    is_ref: bool,
) -> va::VAPictureParameterBufferH264 {
    let (sps, pps) = (&stream.sps, &stream.pps);
    // SAFETY: a plain C struct; every field accepts zero.
    let mut pic: va::VAPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
    pic.CurrPic = va_picture(&stream.surfaces[output], current, 0);
    for r in pic.ReferenceFrames.iter_mut() {
        *r = invalid_picture();
    }
    for (dst, (slot, p)) in pic.ReferenceFrames.iter_mut().zip(refs) {
        *dst = va_picture(
            &stream.surfaces[*slot],
            *p,
            va::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        );
    }
    pic.picture_width_in_mbs_minus1 = sps.pic_width_in_mbs_minus1 as u16;
    pic.picture_height_in_mbs_minus1 = sps.pic_height_in_map_units_minus1 as u16;
    pic.num_ref_frames = sps.max_num_ref_frames;
    // SAFETY: writing bitfields of zeroed union members.
    unsafe {
        let s = &mut pic.seq_fields.bits;
        s.set_chroma_format_idc(sps.chroma_format_idc as u32);
        s.set_residual_colour_transform_flag(sps.separate_colour_plane as u32);
        s.set_gaps_in_frame_num_value_allowed_flag(sps.gaps_in_frame_num_allowed as u32);
        s.set_frame_mbs_only_flag(sps.frame_mbs_only as u32);
        s.set_mb_adaptive_frame_field_flag(sps.mb_adaptive_frame_field as u32);
        s.set_direct_8x8_inference_flag(sps.direct_8x8_inference as u32);
        s.set_MinLumaBiPredSize8x8((sps.level_idc >= 31) as u32);
        s.set_log2_max_frame_num_minus4(sps.log2_max_frame_num_minus4 as u32);
        s.set_pic_order_cnt_type(sps.pic_order_cnt_type as u32);
        s.set_log2_max_pic_order_cnt_lsb_minus4(sps.log2_max_pic_order_cnt_lsb_minus4 as u32);
        s.set_delta_pic_order_always_zero_flag(sps.delta_pic_order_always_zero as u32);
        let p = &mut pic.pic_fields.bits;
        p.set_entropy_coding_mode_flag(pps.entropy_coding_mode as u32);
        p.set_weighted_pred_flag(pps.weighted_pred as u32);
        p.set_weighted_bipred_idc(pps.weighted_bipred_idc as u32);
        p.set_transform_8x8_mode_flag(pps.transform_8x8_mode as u32);
        p.set_field_pic_flag(0);
        p.set_constrained_intra_pred_flag(pps.constrained_intra_pred as u32);
        p.set_pic_order_present_flag(pps.bottom_field_pic_order_in_frame_present as u32);
        p.set_deblocking_filter_control_present_flag(pps.deblocking_filter_control_present as u32);
        p.set_redundant_pic_cnt_present_flag(pps.redundant_pic_cnt_present as u32);
        p.set_reference_pic_flag(is_ref as u32);
    }
    pic.num_slice_groups_minus1 = 0;
    pic.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
    pic.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
    pic.chroma_qp_index_offset = pps.chroma_qp_index_offset;
    pic.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;
    pic.frame_num = header.frame_num as u16;
    pic
}

/// The PPS lists win over the SPS lists; flat 16 where neither applies.
fn iq_matrix(sps: &Sps, pps: &Pps) -> Result<va::VAIQMatrixBufferH264> {
    let mut m = va::VAIQMatrixBufferH264 {
        ScalingList4x4: [[16; 16]; 6],
        ScalingList8x8: [[16; 64]; 2],
        va_reserved: [0; 4],
    };
    let mut apply = |l: &ScalingLists| -> Result<()> {
        if l.use_default_mask != 0 {
            return Err(Error::Bitstream(
                "default scaling matrices are not supported",
            ));
        }
        for i in 0..6 {
            if l.present_mask & (1 << i) != 0 {
                m.ScalingList4x4[i] = l.list_4x4[i];
            }
        }
        for i in 0..2 {
            if l.present_mask & (1 << (6 + i)) != 0 {
                m.ScalingList8x8[i] = l.list_8x8[i];
            }
        }
        Ok(())
    };
    if let Some(l) = &sps.scaling_lists {
        apply(l)?;
    }
    if let Some(l) = &pps.scaling_lists {
        apply(l)?;
    }
    Ok(m)
}

/// RefPicList0 for a P slice (8.2.4.2.1 then 8.2.4.3.1): short-term
/// references by descending PicNum, then the stream's modifications.
fn ref_list0(
    stream: &Stream,
    refs: &[(usize, DpbPicture)],
    sh: &SliceHeader,
) -> Result<Vec<va::VAPictureH264>> {
    if sh.slice_type != Some(SliceType::P) {
        return Ok(Vec::new());
    }
    let max_frame_num = stream.sps.max_frame_num() as i64;
    let pic_num = |p: &DpbPicture| {
        if p.frame_num > sh.frame_num {
            p.frame_num as i64 - max_frame_num
        } else {
            p.frame_num as i64
        }
    };
    let mut list: Vec<(usize, DpbPicture)> = refs.to_vec();
    list.sort_by_key(|(_, p)| std::cmp::Reverse(pic_num(p)));
    let mut pred = sh.frame_num as i64;
    for (index, m) in sh.ref_list_mods_l0.iter().enumerate() {
        let delta = m.value as i64 + 1;
        pred = match m.idc {
            0 => pred - delta,
            1 => pred + delta,
            _ => {
                return Err(Error::Bitstream(
                    "long-term reference lists are not supported",
                ))
            }
        };
        pred = pred.rem_euclid(max_frame_num);
        let wanted = if pred > sh.frame_num as i64 {
            pred - max_frame_num
        } else {
            pred
        };
        let Some(pos) = list.iter().position(|(_, p)| pic_num(p) == wanted) else {
            return Err(Error::Bitstream("reference list names a missing picture"));
        };
        let entry = list.remove(pos);
        list.insert(index.min(list.len()), entry);
    }
    list.truncate(sh.num_ref_idx_l0_active_minus1 as usize + 1);
    Ok(list
        .iter()
        .map(|(slot, p)| {
            va_picture(
                &stream.surfaces[*slot],
                *p,
                va::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            )
        })
        .collect())
}

fn slice_params(
    nal: &[u8],
    sh: &SliceHeader,
    list0: &[va::VAPictureH264],
) -> va::VASliceParameterBufferH264 {
    // SAFETY: a plain C struct; every field accepts zero.
    let mut sl: va::VASliceParameterBufferH264 = unsafe { std::mem::zeroed() };
    sl.slice_data_size = nal.len() as u32;
    sl.slice_data_offset = 0;
    sl.slice_data_flag = 0;
    sl.slice_data_bit_offset = (8 + sh.header_bits) as u16;
    sl.first_mb_in_slice = sh.first_mb_in_slice as u16;
    sl.slice_type = match sh.slice_type {
        Some(SliceType::P) => 0,
        Some(SliceType::B) => 1,
        _ => 2,
    };
    sl.direct_spatial_mv_pred_flag = sh.direct_spatial_mv_pred as u8;
    sl.num_ref_idx_l0_active_minus1 = sh.num_ref_idx_l0_active_minus1;
    sl.num_ref_idx_l1_active_minus1 = sh.num_ref_idx_l1_active_minus1;
    sl.cabac_init_idc = sh.cabac_init_idc;
    sl.slice_qp_delta = sh.slice_qp_delta;
    sl.disable_deblocking_filter_idc = sh.disable_deblocking_filter_idc;
    sl.slice_alpha_c0_offset_div2 = sh.slice_alpha_c0_offset_div2;
    sl.slice_beta_offset_div2 = sh.slice_beta_offset_div2;
    for r in sl.RefPicList0.iter_mut().chain(sl.RefPicList1.iter_mut()) {
        *r = invalid_picture();
    }
    for (dst, src) in sl.RefPicList0.iter_mut().zip(list0) {
        *dst = *src;
    }
    if let Some(t) = &sh.pred_weights {
        sl.luma_log2_weight_denom = t.luma_log2_weight_denom;
        sl.chroma_log2_weight_denom = t.chroma_log2_weight_denom;
        // A reference without explicit weights takes the inferred ones
        // (7.4.3.2): weight 2^denom, offset 0.
        let luma_default = 1i16 << t.luma_log2_weight_denom;
        let chroma_default = 1i16 << t.chroma_log2_weight_denom;
        for (i, w) in t.l0.iter().enumerate().take(32) {
            let (weight, offset) = w.luma.unwrap_or((luma_default, 0));
            sl.luma_weight_l0_flag |= w.luma.is_some() as u8;
            sl.luma_weight_l0[i] = weight;
            sl.luma_offset_l0[i] = offset;
            let c = w.chroma.unwrap_or([(chroma_default, 0); 2]);
            sl.chroma_weight_l0_flag |= w.chroma.is_some() as u8;
            for (k, (weight, offset)) in c.iter().enumerate() {
                sl.chroma_weight_l0[i][k] = *weight;
                sl.chroma_offset_l0[i][k] = *offset;
            }
        }
    }
    sl
}

impl PocState {
    /// Picture order count per 8.2.1 for POC types 0 and 2 (frames only).
    fn derive(&mut self, sps: &Sps, h: &SliceHeader) -> Result<i32> {
        match sps.pic_order_cnt_type {
            0 => {
                let max = sps.max_poc_lsb() as i32;
                let (prev_msb, prev_lsb) = if h.is_idr() {
                    (0, 0)
                } else {
                    (self.prev_msb, self.prev_lsb as i32)
                };
                let lsb = h.pic_order_cnt_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
                    prev_msb + max
                } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
                    prev_msb - max
                } else {
                    prev_msb
                };
                if h.is_reference() {
                    self.prev_msb = msb;
                    self.prev_lsb = h.pic_order_cnt_lsb;
                }
                Ok(msb + lsb)
            }
            2 => {
                let offset = if h.is_idr() {
                    0
                } else if self.prev_frame_num > h.frame_num {
                    self.prev_frame_num_offset + sps.max_frame_num()
                } else {
                    self.prev_frame_num_offset
                };
                self.prev_frame_num_offset = offset;
                let n = (offset + h.frame_num) as i32;
                Ok(if h.is_idr() {
                    0
                } else if h.is_reference() {
                    2 * n
                } else {
                    2 * n - 1
                })
            }
            _ => Err(Error::Bitstream("pic_order_cnt_type 1 is not supported")),
        }
    }
}

/// Sliding-window marking (8.2.5.3): after the current picture is stored,
/// drop the oldest references until `max_num_ref_frames` remain.
fn sliding_window(slots: &mut [Option<DpbPicture>], current_frame_num: u32, sps: &Sps) {
    let max_refs = (sps.max_num_ref_frames as usize).max(1);
    while slots.iter().flatten().count() > max_refs {
        evict_oldest(slots, current_frame_num, sps);
    }
}

/// Free the slot with the smallest FrameNumWrap.
fn evict_oldest(slots: &mut [Option<DpbPicture>], current_frame_num: u32, sps: &Sps) {
    let wrap = |f: u32| {
        if f > current_frame_num {
            f as i64 - sps.max_frame_num() as i64
        } else {
            f as i64
        }
    };
    let oldest = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|p| (wrap(p.frame_num), i)))
        .min();
    if let Some((_, i)) = oldest {
        slots[i] = None;
    }
}

/// Adaptive reference marking: only the short-term operations our streams
/// can contain (1 = unmark one picture, 5 = unmark all). Long-term marking
/// is not produced by the encoder and makes the stream unsupported.
fn apply_mmco(
    slots: &mut [Option<DpbPicture>],
    ops: &[Mmco],
    current_frame_num: u32,
    max_frame_num: u32,
) -> Result<()> {
    for op in ops {
        match op.op {
            1 => {
                let pic_num =
                    current_frame_num as i64 - (op.difference_of_pic_nums_minus1 as i64 + 1);
                for s in slots.iter_mut() {
                    let matches = s.is_some_and(|p| {
                        let wrap = if p.frame_num > current_frame_num {
                            p.frame_num as i64 - max_frame_num as i64
                        } else {
                            p.frame_num as i64
                        };
                        wrap == pic_num
                    });
                    if matches {
                        *s = None;
                    }
                }
            }
            5 => slots.iter_mut().for_each(|s| *s = None),
            _ => {
                return Err(Error::Bitstream(
                    "long-term reference marking is not supported",
                ))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::parameter_sets;
    use crate::h264::writer::{nal_unit, write_sps};
    use crate::EncoderSettings;

    fn settings(width: u32, height: u32) -> EncoderSettings {
        EncoderSettings {
            width,
            height,
            bitrate: 1_000_000,
            framerate: 60,
            vbv_ms: EncoderSettings::DEFAULT_VBV_MS,
        }
    }

    fn sps(coded: (u32, u32), crop: Option<[u32; 4]>) -> Sps {
        Sps {
            pic_width_in_mbs_minus1: coded.0 / 16 - 1,
            pic_height_in_map_units_minus1: coded.1 / 16 - 1,
            frame_mbs_only: true,
            frame_cropping: crop,
            ..Default::default()
        }
    }

    /// The VA-API and Vulkan encoders crop the macroblock padding off the
    /// right and bottom. Every even size they send must be accepted.
    #[test]
    fn accepts_our_encoders_cropped_sizes() {
        for (w, h) in [(1920, 1080), (1366, 768), (2560, 1440), (642, 362), (2, 2)] {
            let (sps, _) = parameter_sets(&settings(w, h));
            let parsed = parse_sps(&nal_unit(3, NAL_SPS, &write_sps(&sps))).unwrap();
            assert!(check_stream_size(&parsed, (w, h)).is_ok(), "{w}x{h}");
        }
    }

    /// A CPU-tier server's OpenH264 SPS, decoded on the GPU client: 1920x1080
    /// (coded 1920x1088) and 1366x768 (coded 1376x768), as gliff-sw encodes
    /// them.
    #[test]
    fn accepts_openh264_cropped_sizes() {
        let full_hd = [
            0x67, 0x42, 0xc0, 0x2a, 0x8c, 0x68, 0x07, 0x80, 0x22, 0x7e, 0x59, 0xa8, 0x08, 0x08,
            0x08, 0x3c, 0x22, 0x11, 0xa8,
        ];
        let wxga = [
            0x67, 0x42, 0xc0, 0x2a, 0x8c, 0x68, 0x05, 0x60, 0x61, 0xe6, 0xe6, 0xa0, 0x20, 0x20,
            0x20, 0xf0, 0x88, 0x46, 0xa0,
        ];
        let full_hd = parse_sps(&full_hd).unwrap();
        assert_eq!(full_hd.coded_size(), (1920, 1088));
        assert!(check_stream_size(&full_hd, (1920, 1080)).is_ok());
        let wxga = parse_sps(&wxga).unwrap();
        assert_eq!(wxga.coded_size(), (1376, 768));
        assert!(check_stream_size(&wxga, (1366, 768)).is_ok());
    }

    /// Another encoder may pad more than to whole macroblocks.
    #[test]
    fn accepts_padding_beyond_whole_macroblocks() {
        let padded = sps((1936, 1104), Some([0, 8, 0, 12]));
        assert_eq!(padded.display_size(), (1920, 1080));
        assert!(check_stream_size(&padded, (1920, 1080)).is_ok());
        let at_cap = sps((4096, 4096), Some([0, 1024, 0, 1024]));
        assert!(check_stream_size(&at_cap, (2048, 2048)).is_ok());
    }

    #[test]
    fn refuses_a_picture_of_another_size() {
        // A tiny picture under a large StreamConfig, the out-of-bounds case.
        assert!(check_stream_size(&sps((16, 16), None), (4096, 4096)).is_err());
        // Uncropped padding is not the announced size.
        assert!(check_stream_size(&sps((1920, 1088), None), (1920, 1080)).is_err());
        // Cropped below the announced size.
        assert!(check_stream_size(&sps((1920, 1088), Some([0, 0, 0, 8])), (1920, 1080)).is_err());
        // A coded side above the cap is refused, even with the right display size.
        let too_big = sps((4112, 1088), Some([0, 1096, 0, 4]));
        assert_eq!(too_big.display_size(), (1920, 1080));
        assert!(check_stream_size(&too_big, (1920, 1080)).is_err());
        // A crop larger than the picture leaves nothing to show.
        let empty = sps((16, 16), Some([0, 100, 0, 100]));
        assert_eq!(empty.display_size(), (0, 0));
        assert!(check_stream_size(&empty, (0, 0)).is_err());
        assert!(check_stream_size(&empty, (16, 16)).is_err());
    }
}
