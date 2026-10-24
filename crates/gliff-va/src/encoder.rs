//! H.264 encode through VA-API.
//!
//! Low-delay layout: IDR then P frames, one reference, no reordering. Two
//! driver-owned reconstruction surfaces alternate as the reference; the
//! input surface is written by the caller through its exported dmabuf.

use std::sync::Arc;

use crate::bindings as va;
use crate::context::{Buffer, Config, Context};
use crate::display::{Caps, Display};
use crate::h264::annexb::{has_start_code, nal_units, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS};
use crate::h264::parser::{Pps, SliceHeader, SliceType, Sps};
use crate::h264::writer::{nal_unit, write_pps, write_slice_header, write_sps};
use crate::settings::EncoderSettings;
use crate::surface::{Surface, UsageHint};
use crate::{Error, Result};

const LOG2_MAX_FRAME_NUM_MINUS4: u8 = 12;
const LOG2_MAX_POC_LSB_MINUS4: u8 = 12;
const PIC_INIT_QP: u8 = 26;
/// Starting QP for VBR on the i965 driver. Its default is visibly soft; 18
/// is sharp and spends about 10% more bits than 22.
const I965_VBR_INITIAL_QP: u32 = 18;
const LEVEL_IDC: u8 = 51;

#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub keyframe: bool,
    /// Annex B access unit; an IDR carries the SPS and PPS in front.
    pub data: Vec<u8>,
}

/// What the encoder knows about a reconstructed picture in a slot.
#[derive(Clone, Copy)]
struct SlotPicture {
    frame_num: u32,
    poc: i32,
}

/// An encode the driver is working on.
pub struct PendingEncode {
    idr: bool,
    _buffers: Vec<Buffer>,
}

pub struct H264Encoder {
    display: Arc<Display>,
    settings: EncoderSettings,
    _config: Config,
    context: Context,
    input: Surface,
    recon: [Surface; 2],
    coded: Buffer,
    sps: Sps,
    pps: Pps,
    /// Annex B framed SPS and PPS, as written on the wire.
    sps_nal: Vec<u8>,
    pps_nal: Vec<u8>,
    /// The same, unescaped, as handed to the driver as packed headers.
    sps_packed: Vec<u8>,
    pps_packed: Vec<u8>,
    slots: [Option<SlotPicture>; 2],
    current_ref: Option<usize>,
    frame_num: u32,
    idr_pic_id: u32,
    poc: i32,
    started: bool,
    pending_rate: Option<(u32, u32, u32)>,
    rate_control: u32,
}

impl H264Encoder {
    /// The largest coded size the driver encodes.
    pub fn max_coded_extent(caps: &Caps) -> (u32, u32) {
        let or = |v: u32| if v == 0 { 4096 } else { v };
        (or(caps.max_width), or(caps.max_height))
    }

    pub fn new(display: &Arc<Display>, caps: &Caps, settings: EncoderSettings) -> Result<Self> {
        caps.can_encode()?;
        let entrypoint = caps.encode_entrypoint.expect("checked by can_encode");
        let (cw, ch) = (settings.coded_width(), settings.coded_height());
        let (max_w, max_h) = Self::max_coded_extent(caps);
        if cw > max_w || ch > max_h {
            return Err(Error::Unsupported(format!(
                "{cw}x{ch} exceeds the encoder maximum {max_w}x{max_h}"
            )));
        }
        let rate_control = pick_rate_control(display, caps);
        let config = Config::new(
            display,
            Caps::PROFILE,
            entrypoint,
            &[
                (va::VAConfigAttribRTFormat, va::VA_RT_FORMAT_YUV420),
                (va::VAConfigAttribRateControl, rate_control),
                (va::VAConfigAttribEncPackedHeaders, Caps::PACKED_HEADERS),
            ],
        )?;
        let input = Surface::new_nv12(display, cw, ch, UsageHint::Encoder)?;
        let recon = [
            Surface::new_nv12(display, cw, ch, UsageHint::Encoder)?,
            Surface::new_nv12(display, cw, ch, UsageHint::Encoder)?,
        ];
        let context = Context::new(display, &config, cw, ch, &[&input, &recon[0], &recon[1]])?;
        let size = ((cw * ch * 2) as usize).max(1 << 20).next_multiple_of(4096);
        let coded = Buffer::coded(&context, size)?;
        let (sps, pps) = parameter_sets(&settings);
        let (sps_rbsp, pps_rbsp) = (write_sps(&sps), write_pps(&pps));
        let framed = |nal_type: u8, rbsp: &[u8], escaped: bool| {
            let mut out = vec![0, 0, 0, 1];
            if escaped {
                out.extend(nal_unit(3, nal_type, rbsp));
            } else {
                out.push(3 << 5 | nal_type);
                out.extend_from_slice(rbsp);
            }
            out
        };
        let sps_nal = framed(NAL_SPS, &sps_rbsp, true);
        let pps_nal = framed(NAL_PPS, &pps_rbsp, true);
        let sps_packed = framed(NAL_SPS, &sps_rbsp, false);
        let pps_packed = framed(NAL_PPS, &pps_rbsp, false);
        tracing::debug!(
            entrypoint = crate::display::entrypoint_name(entrypoint),
            coded = format!("{cw}x{ch}"),
            coded_buffer = size,
            "va-api encoder"
        );
        Ok(Self {
            display: display.clone(),
            settings,
            _config: config,
            context,
            input,
            recon,
            coded,
            sps,
            pps,
            sps_nal,
            pps_nal,
            sps_packed,
            pps_packed,
            slots: [None, None],
            current_ref: None,
            frame_num: 0,
            idr_pic_id: 0,
            poc: 0,
            started: false,
            pending_rate: None,
            rate_control,
        })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// The surface the caller fills with the picture to encode.
    pub fn input(&self) -> &Surface {
        &self.input
    }

    /// SPS then PPS, Annex B framed.
    pub fn parameter_sets(&self) -> Vec<u8> {
        [self.sps_nal.as_slice(), self.pps_nal.as_slice()].concat()
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.set_rate(bitrate, self.settings.framerate, self.settings.vbv_ms);
    }

    /// Change the CBR target, the frame rate it is spread over and the
    /// rate-control buffer, from the next frame on.
    pub fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        if bitrate != self.settings.bitrate
            || framerate != self.settings.framerate
            || vbv_ms != self.settings.vbv_ms
        {
            self.pending_rate = Some((bitrate, framerate.max(1), vbv_ms.max(20)));
        }
    }

    /// Encode the input surface. The caller must have finished writing it.
    pub fn submit(&mut self, force_keyframe: bool) -> Result<PendingEncode> {
        let rate_change = self.pending_rate.take();
        if let Some((b, f, v)) = rate_change {
            self.settings.bitrate = b;
            self.settings.framerate = f;
            self.settings.vbv_ms = v;
        }
        let idr = force_keyframe || !self.started || self.current_ref.is_none();
        if idr {
            self.frame_num = 0;
            self.poc = 0;
            if self.started {
                self.idr_pic_id = (self.idr_pic_id + 1) % 65536;
            }
            self.current_ref = None;
        }
        let setup_slot = self.current_ref.map_or(0, |r| 1 - r);
        let ref_slot = if idr { None } else { self.current_ref };
        let ctx = &self.context;
        let mut buffers = Vec::new();

        if idr || !self.started {
            buffers.push(Buffer::new(
                ctx,
                va::VAEncSequenceParameterBufferType,
                &self.sequence_params(),
            )?);
        }
        if !self.started || rate_change.is_some() {
            buffers.extend(self.rate_params()?);
        }
        if idr {
            buffers.extend(packed_header(
                ctx,
                va::VAEncPackedHeaderSequence,
                &self.sps_packed,
                None,
            )?);
            buffers.extend(packed_header(
                ctx,
                va::VAEncPackedHeaderPicture,
                &self.pps_packed,
                None,
            )?);
        }
        buffers.push(Buffer::new(
            ctx,
            va::VAEncPictureParameterBufferType,
            &self.picture_params(idr, setup_slot, ref_slot),
        )?);
        let header = SliceHeader {
            nal_type: if idr { NAL_IDR } else { NAL_SLICE },
            ref_idc: 3,
            slice_type: Some(if idr { SliceType::I } else { SliceType::P }),
            frame_num: self.frame_num,
            idr_pic_id: self.idr_pic_id,
            pic_order_cnt_lsb: (self.poc as u32) & (self.sps.max_poc_lsb() - 1),
            disable_deblocking_filter_idc: 1,
            ..Default::default()
        };
        let bits = write_slice_header(&header, &self.sps, &self.pps);
        let header_bits = bits.bit_len();
        let mut slice_nal = vec![0, 0, 0, 1, 3 << 5 | header.nal_type];
        slice_nal.extend(bits.into_bytes());
        buffers.extend(packed_header(
            ctx,
            va::VAEncPackedHeaderSlice,
            &slice_nal,
            Some(5 * 8 + header_bits),
        )?);
        buffers.push(Buffer::new(
            ctx,
            va::VAEncSliceParameterBufferType,
            &self.slice_params(&header, ref_slot),
        )?);
        ctx.render(&self.input, &buffers)?;

        self.slots[setup_slot] = Some(SlotPicture {
            frame_num: self.frame_num,
            poc: self.poc,
        });
        self.current_ref = Some(setup_slot);
        self.frame_num = (self.frame_num + 1) % self.sps.max_frame_num();
        // One per frame: OpenH264 on a CPU client shows each picture at once
        // only when the POC steps by at most one.
        self.poc += 1;
        self.started = true;
        if let Some((b, f, v)) = rate_change {
            tracing::info!(
                bitrate = b,
                framerate = f,
                vbv_ms = v,
                "encoder rate changed"
            );
        }
        Ok(PendingEncode {
            idr,
            _buffers: buffers,
        })
    }

    /// Wait for a submitted encode and collect its access unit.
    pub fn finish(&mut self, pending: PendingEncode) -> Result<EncodedPacket> {
        let (slice, status) = self.coded.read_coded()?;
        let failed = va::VA_CODED_BUF_STATUS_BAD_BITSTREAM
            | va::VA_CODED_BUF_STATUS_FRAME_SIZE_OVERFLOW
            | va::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK;
        if status & failed != 0 {
            return Err(Error::Unsupported(format!(
                "encode failed with coded buffer status {status:#x}"
            )));
        }
        let mut data =
            Vec::with_capacity(slice.len() + self.sps_nal.len() + self.pps_nal.len() + 4);
        let has_sps = nal_units(&slice).iter().any(|n| n.nal_type == NAL_SPS);
        if pending.idr && !has_sps {
            data.extend_from_slice(&self.sps_nal);
            data.extend_from_slice(&self.pps_nal);
        }
        if !has_start_code(&slice[..slice.len().min(5)]) {
            data.extend_from_slice(&[0, 0, 0, 1]);
        }
        data.extend_from_slice(&slice);
        Ok(EncodedPacket {
            keyframe: pending.idr,
            data,
        })
    }

    fn sequence_params(&self) -> va::VAEncSequenceParameterBufferH264 {
        // SAFETY: a plain C struct; every field accepts zero.
        let mut seq: va::VAEncSequenceParameterBufferH264 = unsafe { std::mem::zeroed() };
        seq.seq_parameter_set_id = 0;
        seq.level_idc = LEVEL_IDC;
        seq.intra_period = 0;
        seq.intra_idr_period = 0;
        seq.ip_period = 1;
        seq.bits_per_second = self.settings.bitrate;
        seq.max_num_ref_frames = 1;
        seq.picture_width_in_mbs = (self.sps.pic_width_in_mbs_minus1 + 1) as u16;
        seq.picture_height_in_mbs = (self.sps.pic_height_in_map_units_minus1 + 1) as u16;
        // SAFETY: writing bitfields of a zeroed union member.
        unsafe {
            let b = &mut seq.seq_fields.bits;
            b.set_chroma_format_idc(1);
            b.set_frame_mbs_only_flag(1);
            b.set_direct_8x8_inference_flag(1);
            b.set_log2_max_frame_num_minus4(LOG2_MAX_FRAME_NUM_MINUS4 as u32);
            b.set_pic_order_cnt_type(0);
            b.set_log2_max_pic_order_cnt_lsb_minus4(LOG2_MAX_POC_LSB_MINUS4 as u32);
        }
        if let Some([l, r, t, b]) = self.sps.frame_cropping {
            seq.frame_cropping_flag = 1;
            seq.frame_crop_left_offset = l;
            seq.frame_crop_right_offset = r;
            seq.frame_crop_top_offset = t;
            seq.frame_crop_bottom_offset = b;
        }
        seq
    }

    fn rate_params(&self) -> Result<Vec<Buffer>> {
        let s = &self.settings;
        // SAFETY: plain C structs; every field accepts zero.
        let mut rc: va::VAEncMiscParameterRateControl = unsafe { std::mem::zeroed() };
        rc.bits_per_second = s.bitrate;
        rc.target_percentage = 100;
        rc.window_size = s.vbv_ms;
        // SAFETY: writing bitfields of a zeroed union member.
        unsafe {
            rc.rc_flags.bits.set_disable_frame_skip(1);
            if self.rate_control == va::VA_RC_CBR {
                rc.rc_flags.bits.set_disable_bit_stuffing(1);
            }
        }
        if self.rate_control == va::VA_RC_VBR {
            rc.initial_qp = I965_VBR_INITIAL_QP;
        }
        // SAFETY: as above.
        let mut fr: va::VAEncMiscParameterFrameRate = unsafe { std::mem::zeroed() };
        fr.framerate = s.framerate | (1 << 16);
        let buffer_bits = (s.bitrate as u64 * s.vbv_ms as u64 / 1000).min(u32::MAX as u64) as u32;
        let hrd = va::VAEncMiscParameterHRD {
            initial_buffer_fullness: buffer_bits / 2,
            buffer_size: buffer_bits,
            va_reserved: [0; 4],
        };
        Ok(vec![
            Buffer::misc(&self.context, va::VAEncMiscParameterTypeRateControl, &rc)?,
            Buffer::misc(&self.context, va::VAEncMiscParameterTypeFrameRate, &fr)?,
            Buffer::misc(&self.context, va::VAEncMiscParameterTypeHRD, &hrd)?,
        ])
    }

    fn picture(&self, slot: usize) -> va::VAPictureH264 {
        let p = self.slots[slot].expect("slot holds a picture");
        va::VAPictureH264 {
            picture_id: self.recon[slot].id,
            frame_idx: p.frame_num,
            flags: va::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            TopFieldOrderCnt: p.poc,
            BottomFieldOrderCnt: p.poc,
            va_reserved: [0; 4],
        }
    }

    fn picture_params(
        &self,
        idr: bool,
        setup_slot: usize,
        ref_slot: Option<usize>,
    ) -> va::VAEncPictureParameterBufferH264 {
        // SAFETY: a plain C struct; every field accepts zero.
        let mut pic: va::VAEncPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
        pic.CurrPic = va::VAPictureH264 {
            picture_id: self.recon[setup_slot].id,
            frame_idx: self.frame_num,
            flags: 0,
            TopFieldOrderCnt: self.poc,
            BottomFieldOrderCnt: self.poc,
            va_reserved: [0; 4],
        };
        for r in pic.ReferenceFrames.iter_mut() {
            *r = invalid_picture();
        }
        if let Some(r) = ref_slot {
            pic.ReferenceFrames[0] = self.picture(r);
        }
        pic.coded_buf = self.coded.id;
        pic.frame_num = self.frame_num as u16;
        pic.pic_init_qp = PIC_INIT_QP;
        pic.num_ref_idx_l0_active_minus1 = 0;
        pic.num_ref_idx_l1_active_minus1 = 0;
        // SAFETY: writing bitfields of a zeroed union member.
        unsafe {
            let b = &mut pic.pic_fields.bits;
            b.set_idr_pic_flag(idr as u32);
            b.set_reference_pic_flag(1);
            b.set_entropy_coding_mode_flag(self.pps.entropy_coding_mode as u32);
            b.set_deblocking_filter_control_present_flag(
                self.pps.deblocking_filter_control_present as u32,
            );
            b.set_transform_8x8_mode_flag(self.pps.transform_8x8_mode as u32);
        }
        pic
    }

    fn slice_params(
        &self,
        header: &SliceHeader,
        ref_slot: Option<usize>,
    ) -> va::VAEncSliceParameterBufferH264 {
        // SAFETY: a plain C struct; every field accepts zero.
        let mut sl: va::VAEncSliceParameterBufferH264 = unsafe { std::mem::zeroed() };
        let mbs =
            (self.sps.pic_width_in_mbs_minus1 + 1) * (self.sps.pic_height_in_map_units_minus1 + 1);
        sl.macroblock_address = 0;
        sl.num_macroblocks = mbs;
        sl.macroblock_info = va::VA_INVALID_ID;
        sl.slice_type = match header.slice_type {
            Some(SliceType::I) => 2,
            _ => 0,
        };
        sl.pic_parameter_set_id = 0;
        sl.idr_pic_id = header.idr_pic_id as u16;
        sl.pic_order_cnt_lsb = header.pic_order_cnt_lsb as u16;
        for r in sl.RefPicList0.iter_mut().chain(sl.RefPicList1.iter_mut()) {
            *r = invalid_picture();
        }
        if let Some(r) = ref_slot {
            sl.RefPicList0[0] = self.picture(r);
        }
        sl.cabac_init_idc = header.cabac_init_idc;
        sl.slice_qp_delta = header.slice_qp_delta;
        sl.disable_deblocking_filter_idc = header.disable_deblocking_filter_idc;
        sl.slice_alpha_c0_offset_div2 = header.slice_alpha_c0_offset_div2;
        sl.slice_beta_offset_div2 = header.slice_beta_offset_div2;
        sl
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        let _ = self.input.sync();
        let _ = &self.display;
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

/// The packed header parameter and data buffers for `nal` (start code
/// included, unescaped). `bits` overrides the bit length for a slice header
/// that stops before the trailing bits.
fn packed_header(
    ctx: &Context,
    kind: va::VAEncPackedHeaderType,
    nal: &[u8],
    bits: Option<usize>,
) -> Result<[Buffer; 2]> {
    let param = va::VAEncPackedHeaderParameterBuffer {
        type_: kind,
        bit_length: bits.unwrap_or(nal.len() * 8) as u32,
        has_emulation_bytes: 0,
        va_reserved: [0; 4],
    };
    Ok([
        Buffer::new(ctx, va::VAEncPackedHeaderParameterBufferType, &param)?,
        Buffer::data(ctx, va::VAEncPackedHeaderDataBufferType, nal)?,
    ])
}

/// The SPS and PPS this encoder emits: High profile, CABAC, one reference,
/// POC type 0, cropped to the display size.
pub(crate) fn parameter_sets(s: &EncoderSettings) -> (Sps, Pps) {
    let (cw, ch) = (s.coded_width(), s.coded_height());
    let crop =
        (cw != s.width || ch != s.height).then(|| [0, (cw - s.width) / 2, 0, (ch - s.height) / 2]);
    let sps = Sps {
        profile_idc: 100,
        constraint_flags: 0,
        level_idc: LEVEL_IDC,
        chroma_format_idc: 1,
        log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
        pic_order_cnt_type: 0,
        log2_max_pic_order_cnt_lsb_minus4: LOG2_MAX_POC_LSB_MINUS4,
        max_num_ref_frames: 1,
        pic_width_in_mbs_minus1: cw / 16 - 1,
        pic_height_in_map_units_minus1: ch / 16 - 1,
        frame_mbs_only: true,
        direct_8x8_inference: true,
        frame_cropping: crop,
        ..Default::default()
    };
    let pps = Pps {
        entropy_coding_mode: true,
        deblocking_filter_control_present: true,
        pic_init_qp_minus26: PIC_INIT_QP as i8 - 26,
        ..Default::default()
    };
    (sps, pps)
}

/// CBR, except on the verified Haswell i965 path. Its rate control may
/// not pad the stream with bit stuffing off, and then logs "Unrepairable
/// overflow!" on near-static content; VBR needs no padding, at a sharper
/// starting QP than the driver's default.
fn pick_rate_control(display: &Display, caps: &Caps) -> u32 {
    let vbr = use_i965_vbr(&display.vendor, caps.rate_control);
    let choice = if vbr { va::VA_RC_VBR } else { va::VA_RC_CBR };
    tracing::info!(
        mode = if vbr { "VBR" } else { "CBR" },
        "va-api encoder rate control"
    );
    choice
}

fn use_i965_vbr(vendor: &str, supported: u32) -> bool {
    vendor.contains("i965")
        && vendor.to_ascii_lowercase().contains("haswell")
        && supported & va::VA_RC_VBR != 0
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    #[test]
    fn vbr_is_limited_to_the_verified_driver_and_generation() {
        let supported = va::VA_RC_CBR | va::VA_RC_VBR;
        assert!(use_i965_vbr(
            "Intel i965 driver for Intel(R) Haswell Mobile",
            supported
        ));
        for vendor in [
            "Intel i965 driver for Intel(R) Broadwell",
            "Intel i965 driver for Intel(R) Skylake",
            "Intel i965 driver for Intel(R) Sandybridge",
            "Intel iHD driver",
            "Mesa Gallium",
        ] {
            assert!(!use_i965_vbr(vendor, supported));
        }
        assert!(!use_i965_vbr(
            "Intel i965 driver for Intel(R) Haswell",
            va::VA_RC_CBR
        ));
    }
}
