//! H.264 encode through Vulkan Video (`VK_KHR_video_encode_h264`), for GPUs
//! whose VA-API driver has no encoder (NVIDIA ships only NVDEC there).
//!
//! The stream matches the VA-API encoder in `gliff-va` so a client cannot
//! tell them apart: IDR then P frames, one reference, no reordering, POC
//! stepping by one, High profile, CABAC, deblocking off, and an IDR carries
//! the SPS and PPS in front. Two DPB images alternate as the reference.
//! The caller fills `input()` (TRANSFER_DST_OPTIMAL) on another queue and
//! must have waited for that work before `submit`.

use std::sync::Arc;

use ash::vk;
use ash::vk::native as sv;
use gliff_va::{EncodedPacket, EncoderSettings};

use crate::device::Gpu;
use crate::image::{HostBuffer, Image, NV12};
use crate::{Error, Result};

const LOG2_MAX_FRAME_NUM_MINUS4: u8 = 12;
const LOG2_MAX_POC_LSB_MINUS4: u8 = 12;
const PIC_INIT_QP_MINUS26: i8 = 0;
/// H.264 syntax value 1: the deblocking filter is off. (The Std enum names
/// value 0 "DISABLED" after the syntax element, `disable_..._idc`.)
const DEBLOCKING_OFF: sv::StdVideoH264DisableDeblockingFilterIdc = 1;
/// No picture in this reference list entry.
const NO_REFERENCE: u8 = 0xFF;

/// What `query_caps` found for the encode profile.
pub(crate) struct Caps {
    pub(crate) max_extent: (u32, u32),
    pub(crate) std_header: vk::ExtensionProperties,
    pub(crate) bitstream_alignment: u64,
}

/// Build the encode profile (High, 4:2:0, 8-bit, desktop streaming at
/// ultra-low latency) and hand it to `f`. The chain lives on this stack
/// frame, so every use gets a fresh, identical copy.
fn with_profile<R>(f: impl FnOnce(&mut vk::VideoProfileInfoKHR) -> R) -> R {
    let mut h264 = vk::VideoEncodeH264ProfileInfoKHR::default()
        .std_profile_idc(sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);
    let mut usage = vk::VideoEncodeUsageInfoKHR::default()
        .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
        .video_content_hints(vk::VideoEncodeContentFlagsKHR::DESKTOP)
        .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY);
    let mut profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .push_next(&mut h264)
        .push_next(&mut usage);
    f(&mut profile)
}

/// Like `with_profile`, as the one-entry profile list images and buffers take.
fn with_profile_list<R>(f: impl FnOnce(&mut vk::VideoProfileListInfoKHR) -> R) -> R {
    with_profile(|p| {
        let profiles = [*p];
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        f(&mut list)
    })
}

/// Whether queue family `family` lists H.264 among its encode operations.
pub(crate) fn family_encodes_h264(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    family: u32,
) -> bool {
    // SAFETY: two-call pattern on a valid physical device; the output array
    // is sized from the first call and its chains point at live locals.
    unsafe {
        let n = instance.get_physical_device_queue_family_properties2_len(pd);
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut props: Vec<vk::QueueFamilyProperties2> = video
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        instance.get_physical_device_queue_family_properties2(pd, &mut props);
        drop(props);
        video.get(family as usize).is_some_and(|v| {
            v.video_codec_operations
                .contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
        })
    }
}

/// The driver's capabilities for the encode profile, or `None` when it
/// refuses the profile or lacks what the encoder needs (CBR, NV12 input).
pub(crate) fn query_caps(
    entry: &ash::Entry,
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
) -> Option<Caps> {
    let fns = ash::khr::video_queue::Instance::new(entry, instance);
    with_profile(|profile| {
        let mut h264 = vk::VideoEncodeH264CapabilitiesKHR::default();
        let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default()
            .push_next(&mut encode)
            .push_next(&mut h264);
        // SAFETY: valid physical device, profile chain and output chain.
        let r = unsafe {
            (fns.fp().get_physical_device_video_capabilities_khr)(pd, profile, &mut caps)
        };
        if r != vk::Result::SUCCESS {
            tracing::debug!(result = ?r, "vulkan video: h264 encode profile refused");
            return None;
        }
        let found = Caps {
            max_extent: (caps.max_coded_extent.width, caps.max_coded_extent.height),
            std_header: caps.std_header_version,
            bitstream_alignment: caps
                .min_bitstream_buffer_size_alignment
                .max(caps.min_bitstream_buffer_offset_alignment)
                .max(1),
        };
        let (dpb_slots, active_refs) = (caps.max_dpb_slots, caps.max_active_reference_pictures);
        if !encode
            .rate_control_modes
            .contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        {
            tracing::debug!(modes = ?encode.rate_control_modes, "vulkan video: no CBR");
            return None;
        }
        if dpb_slots < 2 || active_refs < 1 {
            tracing::debug!("vulkan video: too few DPB slots");
            return None;
        }
        if !encode_src_is_nv12(&fns, pd, profile) {
            tracing::debug!("vulkan video: encoder input is not NV12");
            return None;
        }
        Some(found)
    })
}

fn encode_src_is_nv12(
    fns: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    profile: &vk::VideoProfileInfoKHR,
) -> bool {
    let profiles = [*profile];
    let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST)
        .push_next(&mut list);
    let get = fns.fp().get_physical_device_video_format_properties_khr;
    // SAFETY: two-call pattern with a valid info chain.
    unsafe {
        let mut count = 0;
        if get(pd, &info, &mut count, std::ptr::null_mut()) != vk::Result::SUCCESS {
            return false;
        }
        let mut formats = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
        if get(pd, &info, &mut count, formats.as_mut_ptr()) != vk::Result::SUCCESS {
            return false;
        }
        formats.iter().any(|f| f.format == NV12)
    }
}

/// The reconstructed picture in a DPB slot.
#[derive(Clone, Copy)]
struct SlotPicture {
    frame_num: u32,
    poc: i32,
    idr: bool,
}

/// The CBR state the session holds: bitrate, frame rate, buffer in ms.
type Rate = (u32, u32, u32);

/// An encode the GPU is working on.
pub(crate) struct PendingEncode {
    idr: bool,
}

pub(crate) struct VkH264Encoder {
    gpu: Arc<Gpu>,
    settings: EncoderSettings,
    queue: vk::Queue,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    parameters: vk::VideoSessionParametersKHR,
    input: Image,
    dpb: [Image; 2],
    bitstream: HostBuffer,
    query_pool: vk::QueryPool,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// SPS then PPS, Annex B framed, as the driver wrote them.
    parameter_sets: Vec<u8>,
    slots: [Option<SlotPicture>; 2],
    current_ref: Option<usize>,
    frame_num: u32,
    idr_pic_id: u32,
    poc: i32,
    /// The rate control state the session has, once reset.
    rate: Option<Rate>,
    pending_rate: Option<Rate>,
}

impl VkH264Encoder {
    /// The largest coded size the device encodes.
    pub(crate) fn max_coded_extent(gpu: &Gpu) -> Option<(u32, u32)> {
        gpu.video_encode.as_ref().map(|v| v.max_extent)
    }

    /// An encoder on encode queue `stream` (modulo the queues there are),
    /// so main and aux run on separate queues when the family has two.
    pub(crate) fn new(gpu: &Arc<Gpu>, settings: EncoderSettings, stream: usize) -> Result<Self> {
        let video = gpu
            .video_encode
            .as_ref()
            .ok_or_else(|| Error::Unsupported("no Vulkan Video H.264 encoder".into()))?;
        let (cw, ch) = (settings.coded_width(), settings.coded_height());
        let (max_w, max_h) = video.max_extent;
        if cw > max_w || ch > max_h {
            return Err(Error::Unsupported(format!(
                "{cw}x{ch} exceeds the encoder maximum {max_w}x{max_h}"
            )));
        }
        let queue = video.queues[stream % video.queues.len()];
        let dev = &gpu.device;
        let qfns = video.queue_fns.fp();
        let extent = vk::Extent2D {
            width: cw,
            height: ch,
        };

        // SAFETY: Vulkan calls on a valid device with create infos whose
        // chains point at live locals. Everything created here is owned by
        // the returned encoder and destroyed in `Drop`; on an early error
        // the partially built encoder is dropped the same way.
        unsafe {
            let session = with_profile(|profile| {
                let info = vk::VideoSessionCreateInfoKHR::default()
                    .queue_family_index(video.family)
                    .video_profile(profile)
                    .picture_format(NV12)
                    .max_coded_extent(extent)
                    .reference_picture_format(NV12)
                    .max_dpb_slots(2)
                    .max_active_reference_pictures(1)
                    .std_header_version(&video.std_header);
                let mut session = vk::VideoSessionKHR::null();
                (qfns.create_video_session_khr)(dev.handle(), &info, std::ptr::null(), &mut session)
                    .result_with_success(session)
            })?;
            let mut enc = Self {
                gpu: gpu.clone(),
                settings: settings.clone(),
                queue,
                session,
                session_memory: Vec::new(),
                parameters: vk::VideoSessionParametersKHR::null(),
                input: with_profile_list(|list| {
                    Image::video_nv12(
                        gpu,
                        cw,
                        ch,
                        vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                            | vk::ImageUsageFlags::TRANSFER_DST,
                        list,
                    )
                })?,
                dpb: [
                    with_profile_list(|list| {
                        Image::video_nv12(
                            gpu,
                            cw,
                            ch,
                            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
                            list,
                        )
                    })?,
                    with_profile_list(|list| {
                        Image::video_nv12(
                            gpu,
                            cw,
                            ch,
                            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
                            list,
                        )
                    })?,
                ],
                bitstream: with_profile_list(|list| {
                    let size = ((cw * ch * 2) as u64)
                        .max(1 << 20)
                        .next_multiple_of(video.bitstream_alignment.max(4096));
                    HostBuffer::video(
                        gpu,
                        size as usize,
                        vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR,
                        list,
                    )
                })?,
                query_pool: vk::QueryPool::null(),
                pool: vk::CommandPool::null(),
                cmd: vk::CommandBuffer::null(),
                fence: vk::Fence::null(),
                parameter_sets: Vec::new(),
                slots: [None, None],
                current_ref: None,
                frame_num: 0,
                idr_pic_id: 0,
                poc: 0,
                rate: None,
                pending_rate: Some((
                    settings.bitrate,
                    settings.framerate.max(1),
                    settings.vbv_ms.max(20),
                )),
            };
            enc.bind_session_memory()?;
            enc.create_parameters()?;
            enc.parameter_sets = enc.encoded_parameter_sets()?;
            enc.query_pool = with_profile(|profile| {
                let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
                    .encode_feedback_flags(
                        vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                            | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
                    );
                let info = vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
                    .query_count(1)
                    .push_next(&mut feedback)
                    .push_next(profile);
                dev.create_query_pool(&info, None)
            })?;
            enc.pool = dev.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(video.family)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                None,
            )?;
            enc.cmd = dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(enc.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            enc.fence = dev.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?;
            tracing::debug!(
                coded = format!("{cw}x{ch}"),
                bitstream = enc.bitstream.size,
                parameter_sets = enc.parameter_sets.len(),
                "vulkan video encoder"
            );
            Ok(enc)
        }
    }

    /// The image the caller fills with the picture to encode, at the coded
    /// size. Leave it in TRANSFER_DST_OPTIMAL.
    pub(crate) fn input(&self) -> &Image {
        &self.input
    }

    /// Change the CBR target, the frame rate it is spread over and the
    /// rate-control buffer, from the next frame on.
    pub(crate) fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        let next = (bitrate, framerate.max(1), vbv_ms.max(20));
        if self.pending_rate.or(self.rate) != Some(next) {
            self.pending_rate = Some(next);
        }
    }

    /// Encode the input image. The caller must have finished writing it.
    pub(crate) fn submit(&mut self, force_keyframe: bool) -> Result<PendingEncode> {
        let idr = force_keyframe || self.current_ref.is_none();
        if idr {
            if self.slots.iter().any(Option::is_some) {
                self.idr_pic_id = (self.idr_pic_id + 1) % 65536;
            }
            self.frame_num = 0;
            self.poc = 0;
            self.current_ref = None;
        }
        let setup = self.current_ref.map_or(0, |r| 1 - r);
        let reference = if idr { None } else { self.current_ref };
        let new_rate = self.pending_rate.take();
        let dev = &self.gpu.device;
        let video = self.gpu.video_encode.as_ref().expect("checked in new");
        let qfns = video.queue_fns.fp();
        let efns = video.encode_fns.fp();
        let cmd = self.cmd;
        let extent = vk::Extent2D {
            width: self.settings.coded_width(),
            height: self.settings.coded_height(),
        };

        // SAFETY: the previous submission finished (its fence is waited
        // first), so the command buffer, query and bitstream are free. Every
        // pointer in the recorded structures refers to a local that outlives
        // the recording call it is passed to.
        unsafe {
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
            dev.reset_command_pool(self.pool, vk::CommandPoolResetFlags::empty())?;
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            dev.cmd_reset_query_pool(cmd, self.query_pool, 0, 1);
            self.input
                .transition(cmd, vk::ImageLayout::VIDEO_ENCODE_SRC_KHR);
            for d in &self.dpb {
                d.transition(cmd, vk::ImageLayout::VIDEO_ENCODE_DPB_KHR);
            }

            let resource = |img: &Image| {
                vk::VideoPictureResourceInfoKHR::default()
                    .coded_extent(extent)
                    .image_view_binding(img.view0())
            };
            let setup_resource = resource(&self.dpb[setup]);
            let ref_resource = reference.map(|r| resource(&self.dpb[r]));

            // Begin: the setup picture is bound without a slot (it becomes
            // one when this encode writes it), the reference with its slot.
            let mut begin_slots = vec![vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(-1)
                .picture_resource(&setup_resource)];
            if let (Some(r), Some(res)) = (reference, ref_resource.as_ref()) {
                begin_slots.push(
                    vk::VideoReferenceSlotInfoKHR::default()
                        .slot_index(r as i32)
                        .picture_resource(res),
                );
            }
            // A session with rate control set must be told the same state
            // again at every begin.
            let mut current_h264_layer = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
            let current_layers = self
                .rate
                .map(|r| [rate_layer(r).push_next(&mut current_h264_layer)]);
            let mut current_h264 = self.rate.map(|_| h264_rate_control());
            let mut current_rc = self
                .rate
                .zip(current_layers.as_ref())
                .map(|(r, l)| rate_control(r, l));
            let mut begin = vk::VideoBeginCodingInfoKHR::default()
                .video_session(self.session)
                .video_session_parameters(self.parameters)
                .reference_slots(&begin_slots);
            if let (Some(rc), Some(h264)) = (current_rc.as_mut(), current_h264.as_mut()) {
                begin = begin.push_next(rc).push_next(h264);
            }
            (qfns.cmd_begin_video_coding_khr)(cmd, &begin);

            // A fresh session must be reset before its first encode; the
            // reset also drops the rate control state, so set it again.
            let reset = self.rate.is_none();
            let apply = new_rate.or(if reset { self.rate } else { None });
            if reset || apply.is_some() {
                let rate = apply.unwrap_or((
                    self.settings.bitrate,
                    self.settings.framerate.max(1),
                    self.settings.vbv_ms.max(20),
                ));
                // NVIDIA refuses the rate control (VK_ERROR_INITIALIZATION_FAILED
                // at vkEndCommandBuffer) unless every layer carries its H.264
                // layer info, optional as that is in the spec.
                let mut h264_layer = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
                let layers = [rate_layer(rate).push_next(&mut h264_layer)];
                let mut h264_rc = h264_rate_control();
                let mut rc = rate_control(rate, &layers);
                let mut flags = vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL;
                if reset {
                    flags |= vk::VideoCodingControlFlagsKHR::RESET;
                }
                let control = vk::VideoCodingControlInfoKHR::default()
                    .flags(flags)
                    .push_next(&mut rc)
                    .push_next(&mut h264_rc);
                (qfns.cmd_control_video_coding_khr)(cmd, &control);
                self.rate = Some(rate);
                self.settings.bitrate = rate.0;
                self.settings.framerate = rate.1;
                self.settings.vbv_ms = rate.2;
                if !reset {
                    tracing::info!(
                        bitrate = rate.0,
                        framerate = rate.1,
                        vbv_ms = rate.2,
                        "encoder rate changed"
                    );
                }
            }

            let picture_type = if idr {
                sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
            } else {
                sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
            };
            let mut setup_std: sv::StdVideoEncodeH264ReferenceInfo = std::mem::zeroed();
            setup_std.primary_pic_type = picture_type;
            setup_std.FrameNum = self.frame_num;
            setup_std.PicOrderCnt = self.poc;
            let mut setup_dpb =
                vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
            let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(setup as i32)
                .picture_resource(&setup_resource)
                .push_next(&mut setup_dpb);

            let ref_std = reference.map(|r| {
                let p = self.slots[r].expect("the reference slot holds a picture");
                let mut info: sv::StdVideoEncodeH264ReferenceInfo = std::mem::zeroed();
                info.primary_pic_type = if p.idr {
                    sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
                } else {
                    sv::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
                };
                info.FrameNum = p.frame_num;
                info.PicOrderCnt = p.poc;
                info
            });
            let mut ref_dpb = ref_std
                .as_ref()
                .map(|s| vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(s));
            let mut ref_slots = Vec::new();
            if let (Some(r), Some(res), Some(dpb)) =
                (reference, ref_resource.as_ref(), ref_dpb.as_mut())
            {
                ref_slots.push(
                    vk::VideoReferenceSlotInfoKHR::default()
                        .slot_index(r as i32)
                        .picture_resource(res)
                        .push_next(dpb),
                );
            }

            let mut lists: sv::StdVideoEncodeH264ReferenceListsInfo = std::mem::zeroed();
            lists.num_ref_idx_l0_active_minus1 = 0;
            lists.num_ref_idx_l1_active_minus1 = 0;
            lists.RefPicList0 = [NO_REFERENCE; 32];
            lists.RefPicList1 = [NO_REFERENCE; 32];
            if let Some(r) = reference {
                lists.RefPicList0[0] = r as u8;
            }
            let mut picture: sv::StdVideoEncodeH264PictureInfo = std::mem::zeroed();
            picture.flags.set_IdrPicFlag(idr as u32);
            picture.flags.set_is_reference(1);
            picture.seq_parameter_set_id = 0;
            picture.pic_parameter_set_id = 0;
            picture.idr_pic_id = self.idr_pic_id as u16;
            picture.primary_pic_type = picture_type;
            picture.frame_num = self.frame_num;
            picture.PicOrderCnt = self.poc;
            picture.pRefLists = &lists;

            let mut slice: sv::StdVideoEncodeH264SliceHeader = std::mem::zeroed();
            slice.first_mb_in_slice = 0;
            slice.slice_type = if idr {
                sv::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I
            } else {
                sv::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P
            };
            slice.cabac_init_idc = sv::StdVideoH264CabacInitIdc_STD_VIDEO_H264_CABAC_INIT_IDC_0;
            slice.disable_deblocking_filter_idc = DEBLOCKING_OFF;
            let slices = [vk::VideoEncodeH264NaluSliceInfoKHR::default()
                .constant_qp(0)
                .std_slice_header(&slice)];
            let mut h264 = vk::VideoEncodeH264PictureInfoKHR::default()
                .nalu_slice_entries(&slices)
                .std_picture_info(&picture)
                .generate_prefix_nalu(false);

            let info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream.buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream.size as u64)
                .src_picture_resource(resource(&self.input))
                .setup_reference_slot(&setup_slot)
                .reference_slots(&ref_slots)
                .push_next(&mut h264);
            dev.cmd_begin_query(cmd, self.query_pool, 0, vk::QueryControlFlags::empty());
            (efns.cmd_encode_video_khr)(cmd, &info);
            dev.cmd_end_query(cmd, self.query_pool, 0);
            (qfns.cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
            dev.end_command_buffer(cmd)
                .map_err(|e| step_error("record the encode", e))?;

            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let submit = vk::SubmitInfo2::default().command_buffer_infos(&cmds);
            // Reset only now: a fence reset before a failed submit would
            // never signal, and the next wait (or `Drop`) would hang.
            dev.reset_fences(&[self.fence])?;
            dev.queue_submit2(self.queue, &[submit], self.fence)
                .map_err(|e| step_error("submit the encode", e))?;
        }

        self.slots[setup] = Some(SlotPicture {
            frame_num: self.frame_num,
            poc: self.poc,
            idr,
        });
        self.current_ref = Some(setup);
        self.frame_num = (self.frame_num + 1) % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4));
        // One per frame, as the VA-API encoder does: OpenH264 on a CPU
        // client shows each picture at once only when the POC steps by one.
        self.poc += 1;
        Ok(PendingEncode { idr })
    }

    /// Wait for a submitted encode and collect its access unit.
    pub(crate) fn finish(&mut self, pending: PendingEncode) -> Result<EncodedPacket> {
        let dev = &self.gpu.device;
        // One query: [offset, bytes written, status], in the spec's order.
        let mut feedback = [[0u32; 3]; 1];
        // SAFETY: valid fence and query pool; the result array matches one
        // query with two feedback values plus the status.
        unsafe {
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
            dev.get_query_pool_results(
                self.query_pool,
                0,
                &mut feedback,
                vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR,
            )
            .map_err(|e| step_error("read the encode feedback", e))?;
        }
        let [offset, bytes, status] = feedback[0];
        let status = status as i32;
        if status != vk::QueryResultStatusKHR::COMPLETE.as_raw() {
            // A failed encode leaves the reference undefined: start over.
            self.current_ref = None;
            return Err(Error::Unsupported(format!(
                "vulkan video encode failed with status {status}"
            )));
        }
        let (offset, bytes) = (offset as usize, bytes as usize);
        if offset + bytes > self.bitstream.size {
            self.current_ref = None;
            return Err(Error::Unsupported(format!(
                "encoded {bytes} bytes at {offset} overflow the {} byte bitstream buffer",
                self.bitstream.size
            )));
        }
        let slice = self.bitstream.read(offset, bytes);
        let mut data = Vec::with_capacity(bytes + self.parameter_sets.len() + 4);
        if pending.idr {
            data.extend_from_slice(&self.parameter_sets);
        }
        if !has_start_code(&slice) {
            data.extend_from_slice(&[0, 0, 0, 1]);
        }
        data.extend_from_slice(&slice);
        Ok(EncodedPacket {
            keyframe: pending.idr,
            data,
        })
    }

    /// Allocate and bind the memory the driver asks for behind the session.
    fn bind_session_memory(&mut self) -> Result<()> {
        // SAFETY: Vulkan calls on the encoder's live device and session,
        // with info chains and output arrays that point at live locals.
        unsafe {
            let video = self.gpu.video_encode.as_ref().expect("checked in new");
            let fns = video.queue_fns.fp();
            let dev = self.gpu.device.handle();
            let mut count = 0;
            (fns.get_video_session_memory_requirements_khr)(
                dev,
                self.session,
                &mut count,
                std::ptr::null_mut(),
            )
            .result()?;
            let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
            (fns.get_video_session_memory_requirements_khr)(
                dev,
                self.session,
                &mut count,
                reqs.as_mut_ptr(),
            )
            .result()?;
            let mut binds = Vec::with_capacity(reqs.len());
            for r in &reqs {
                let m = r.memory_requirements;
                let memory = self
                    .gpu
                    .allocate(m, vk::MemoryPropertyFlags::DEVICE_LOCAL, None)
                    .or_else(|_| self.gpu.allocate(m, vk::MemoryPropertyFlags::empty(), None))?;
                self.session_memory.push(memory);
                binds.push(
                    vk::BindVideoSessionMemoryInfoKHR::default()
                        .memory_bind_index(r.memory_bind_index)
                        .memory(memory)
                        .memory_offset(0)
                        .memory_size(m.size),
                );
            }
            (fns.bind_video_session_memory_khr)(
                dev,
                self.session,
                binds.len() as u32,
                binds.as_ptr(),
            )
            .result()?;
            Ok(())
        }
    }

    /// The SPS and PPS: High profile, CABAC, one reference, POC type 0,
    /// cropped to the display size, the same as the VA-API encoder's.
    fn create_parameters(&mut self) -> Result<()> {
        // SAFETY: Vulkan calls on the encoder's live device and session,
        // with info chains and output arrays that point at live locals.
        unsafe {
            let s = &self.settings;
            let (cw, ch) = (s.coded_width(), s.coded_height());
            let mut sps: sv::StdVideoH264SequenceParameterSet = std::mem::zeroed();
            sps.profile_idc = sv::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
            // 5.2: a 3440x1440 desktop at 60 fps is past 5.1's macroblock rate.
            sps.level_idc = sv::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2;
            sps.chroma_format_idc =
                sv::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
            sps.seq_parameter_set_id = 0;
            sps.log2_max_frame_num_minus4 = LOG2_MAX_FRAME_NUM_MINUS4;
            sps.pic_order_cnt_type = sv::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_0;
            sps.log2_max_pic_order_cnt_lsb_minus4 = LOG2_MAX_POC_LSB_MINUS4;
            sps.max_num_ref_frames = 1;
            sps.pic_width_in_mbs_minus1 = cw / 16 - 1;
            sps.pic_height_in_map_units_minus1 = ch / 16 - 1;
            sps.flags.set_frame_mbs_only_flag(1);
            sps.flags.set_direct_8x8_inference_flag(1);
            if cw != s.width || ch != s.height {
                // In 4:2:0 crop units of two pixels, as the VA-API encoder does.
                sps.flags.set_frame_cropping_flag(1);
                sps.frame_crop_right_offset = (cw - s.width) / 2;
                sps.frame_crop_bottom_offset = (ch - s.height) / 2;
            }
            let mut pps: sv::StdVideoH264PictureParameterSet = std::mem::zeroed();
            pps.seq_parameter_set_id = 0;
            pps.pic_parameter_set_id = 0;
            pps.num_ref_idx_l0_default_active_minus1 = 0;
            pps.num_ref_idx_l1_default_active_minus1 = 0;
            pps.weighted_bipred_idc =
                sv::StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_DEFAULT;
            pps.pic_init_qp_minus26 = PIC_INIT_QP_MINUS26;
            pps.flags.set_entropy_coding_mode_flag(1);
            pps.flags.set_deblocking_filter_control_present_flag(1);

            let spss = [sps];
            let ppss = [pps];
            let add = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
                .std_sp_ss(&spss)
                .std_pp_ss(&ppss);
            let mut h264 = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
                .max_std_sps_count(1)
                .max_std_pps_count(1)
                .parameters_add_info(&add);
            let info = vk::VideoSessionParametersCreateInfoKHR::default()
                .video_session(self.session)
                .push_next(&mut h264);
            let video = self.gpu.video_encode.as_ref().expect("checked in new");
            let mut params = vk::VideoSessionParametersKHR::null();
            (video.queue_fns.fp().create_video_session_parameters_khr)(
                self.gpu.device.handle(),
                &info,
                std::ptr::null(),
                &mut params,
            )
            .result()?;
            self.parameters = params;
            Ok(())
        }
    }

    /// The SPS and PPS as the driver encodes them (it may adjust fields it
    /// does not support), Annex B framed.
    fn encoded_parameter_sets(&self) -> Result<Vec<u8>> {
        // SAFETY: Vulkan calls on the encoder's live device and session,
        // with info chains and output arrays that point at live locals.
        unsafe {
            let video = self.gpu.video_encode.as_ref().expect("checked in new");
            let get = video
                .encode_fns
                .fp()
                .get_encoded_video_session_parameters_khr;
            let mut h264 = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
                .write_std_sps(true)
                .write_std_pps(true)
                .std_sps_id(0)
                .std_pps_id(0);
            let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
                .video_session_parameters(self.parameters)
                .push_next(&mut h264);
            let dev = self.gpu.device.handle();
            let mut size = 0usize;
            get(
                dev,
                &info,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
            )
            .result()?;
            let mut data = vec![0u8; size];
            get(
                dev,
                &info,
                std::ptr::null_mut(),
                &mut size,
                data.as_mut_ptr().cast(),
            )
            .result()?;
            data.truncate(size);
            if !has_start_code(&data) {
                return Err(Error::Unsupported(
                    "the driver's encoded SPS/PPS have no Annex B start code".into(),
                ));
            }
            Ok(data)
        }
    }
}

impl Drop for VkH264Encoder {
    fn drop(&mut self) {
        let dev = &self.gpu.device;
        // SAFETY: wait for the last submission, then destroy what `new`
        // created; null handles (a partially built encoder) are skipped.
        unsafe {
            if self.fence != vk::Fence::null() {
                let _ = dev.wait_for_fences(&[self.fence], true, 5_000_000_000);
                dev.destroy_fence(self.fence, None);
            }
            if self.pool != vk::CommandPool::null() {
                dev.destroy_command_pool(self.pool, None);
            }
            if self.query_pool != vk::QueryPool::null() {
                dev.destroy_query_pool(self.query_pool, None);
            }
            if let Some(video) = self.gpu.video_encode.as_ref() {
                let fns = video.queue_fns.fp();
                if self.parameters != vk::VideoSessionParametersKHR::null() {
                    (fns.destroy_video_session_parameters_khr)(
                        dev.handle(),
                        self.parameters,
                        std::ptr::null(),
                    );
                }
                (fns.destroy_video_session_khr)(dev.handle(), self.session, std::ptr::null());
            }
            for m in self.session_memory.drain(..) {
                dev.free_memory(m, None);
            }
        }
    }
}

fn rate_layer<'a>(rate: Rate) -> vk::VideoEncodeRateControlLayerInfoKHR<'a> {
    vk::VideoEncodeRateControlLayerInfoKHR::default()
        .average_bitrate(rate.0 as u64)
        .max_bitrate(rate.0 as u64)
        .frame_rate_numerator(rate.1)
        .frame_rate_denominator(1)
}

/// The H.264 side of the rate control, as NVIDIA prefers it for every
/// quality level: an endless GOP (one IDR, then P frames until a keyframe
/// is asked for), no B frames, one temporal layer.
fn h264_rate_control() -> vk::VideoEncodeH264RateControlInfoKHR<'static> {
    vk::VideoEncodeH264RateControlInfoKHR::default()
        .flags(
            vk::VideoEncodeH264RateControlFlagsKHR::REGULAR_GOP
                | vk::VideoEncodeH264RateControlFlagsKHR::REFERENCE_PATTERN_FLAT,
        )
        .gop_frame_count(u32::MAX)
        .idr_period(u32::MAX)
        .consecutive_b_frame_count(0)
        .temporal_layer_count(1)
}

fn rate_control<'a>(
    rate: Rate,
    layers: &'a [vk::VideoEncodeRateControlLayerInfoKHR<'a>],
) -> vk::VideoEncodeRateControlInfoKHR<'a> {
    vk::VideoEncodeRateControlInfoKHR::default()
        .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        .layers(layers)
        .virtual_buffer_size_in_ms(rate.2)
        .initial_virtual_buffer_size_in_ms(rate.2 / 2)
}

fn step_error(step: &str, e: vk::Result) -> Error {
    Error::Unsupported(format!("vulkan video: {step}: {e}"))
}

fn has_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}
