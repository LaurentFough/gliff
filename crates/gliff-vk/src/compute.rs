//! The two compute passes: the AVC444 split (BGRA -> 2x NV12) on the server
//! and the recombine (2x NV12 -> BGRA) on the client.

use std::sync::Arc;

use ash::vk;

use crate::device::Gpu;
use crate::image::Image;
use crate::Result;

const SPLIT_SPV: &[u8] = include_bytes!("../shaders/split.spv");
const RECOMBINE_SPV: &[u8] = include_bytes!("../shaders/recombine.spv");

#[repr(C)]
#[derive(Clone, Copy)]
struct Push {
    width: i32,
    height: i32,
    has_aux: i32,
    /// Decoder zoom, or the capture transform for the split shader.
    zoom: i32,
}

/// One compute pipeline with a fixed descriptor layout: some sampled images
/// followed by some storage images, all at set 0.
pub(crate) struct Pass {
    gpu: Arc<Gpu>,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    sampled: u32,
    storage: u32,
}

impl Pass {
    fn new(
        gpu: &Arc<Gpu>,
        spirv: &[u8],
        sampled: u32,
        storage: u32,
        filter: vk::Filter,
    ) -> Result<Self> {
        // SAFETY: valid create infos; every handle is destroyed in Drop.
        unsafe {
            let sampler = gpu.device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(filter)
                    .min_filter(filter)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?;
            let samplers = [sampler];
            let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..sampled + storage)
                .map(|i| {
                    let b = vk::DescriptorSetLayoutBinding::default()
                        .binding(i)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE);
                    if i < sampled {
                        b.descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                            .immutable_samplers(&samplers)
                    } else {
                        b.descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    }
                })
                .collect();
            let set_layout = gpu.device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )?;
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(std::mem::size_of::<Push>() as u32)];
            let set_layouts = [set_layout];
            let layout = gpu.device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )?;
            let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv))?;
            let module = gpu
                .device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let info = vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(layout);
            let pipeline = gpu
                .device
                .create_compute_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| e)?[0];
            gpu.device.destroy_shader_module(module, None);
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: sampled.max(1) * 4,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: storage.max(1) * 4,
                },
            ];
            let pool = gpu.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(4)
                    .pool_sizes(&sizes),
                None,
            )?;
            Ok(Self {
                gpu: gpu.clone(),
                sampler,
                set_layout,
                layout,
                pipeline,
                pool,
                sampled,
                storage,
            })
        }
    }

    /// Record one dispatch over `width x height` pixels in 2x2 blocks.
    /// `views` lists the sampled views then the storage views, in binding
    /// order; the caller has put the images in the right layouts.
    ///
    /// The descriptor pool is reset here, so at most one dispatch of this pass
    /// may be in flight: the callers submit and wait per frame.
    fn dispatch(
        &self,
        cmd: vk::CommandBuffer,
        views: &[vk::ImageView],
        width: u32,
        height: u32,
        has_aux: bool,
        zoom: u32,
    ) -> Result<()> {
        debug_assert_eq!(views.len() as u32, self.sampled + self.storage);
        let dev = &self.gpu.device;
        // SAFETY: valid handles; the set is used only by this command buffer,
        // which completes before the next dispatch resets the pool.
        unsafe {
            dev.reset_descriptor_pool(self.pool, vk::DescriptorPoolResetFlags::empty())?;
            let layouts = [self.set_layout];
            let set = dev.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&layouts),
            )?[0];
            let infos: Vec<[vk::DescriptorImageInfo; 1]> = views
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let layout = if (i as u32) < self.sampled {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    } else {
                        vk::ImageLayout::GENERAL
                    };
                    [vk::DescriptorImageInfo {
                        sampler: self.sampler,
                        image_view: v,
                        image_layout: layout,
                    }]
                })
                .collect();
            let writes: Vec<vk::WriteDescriptorSet> = infos
                .iter()
                .enumerate()
                .map(|(i, info)| {
                    let ty = if (i as u32) < self.sampled {
                        vk::DescriptorType::COMBINED_IMAGE_SAMPLER
                    } else {
                        vk::DescriptorType::STORAGE_IMAGE
                    };
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(i as u32)
                        .descriptor_type(ty)
                        .image_info(info)
                })
                .collect();
            dev.update_descriptor_sets(&writes, &[]);
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            dev.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[set],
                &[],
            );
            let push = Push {
                width: width as i32,
                height: height as i32,
                has_aux: has_aux as i32,
                zoom: zoom as i32,
            };
            let bytes = std::slice::from_raw_parts(
                (&push as *const Push).cast::<u8>(),
                std::mem::size_of::<Push>(),
            );
            dev.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::COMPUTE, 0, bytes);
            dev.cmd_dispatch(
                cmd,
                width.div_ceil(2).div_ceil(16),
                height.div_ceil(2).div_ceil(16),
                1,
            );
        }
        Ok(())
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        // SAFETY: owners wait for GPU work before dropping.
        unsafe {
            self.gpu.device.destroy_descriptor_pool(self.pool, None);
            self.gpu.device.destroy_pipeline(self.pipeline, None);
            self.gpu.device.destroy_pipeline_layout(self.layout, None);
            self.gpu
                .device
                .destroy_descriptor_set_layout(self.set_layout, None);
            self.gpu.device.destroy_sampler(self.sampler, None);
        }
    }
}

/// BGRA source -> main (+ aux) NV12 encoder inputs.
pub(crate) struct Split(Pass);

impl Split {
    pub(crate) fn new(gpu: &Arc<Gpu>) -> Result<Self> {
        // Linear: the split may downscale a mirrored screen to the window.
        Ok(Self(Pass::new(gpu, SPLIT_SPV, 1, 4, vk::Filter::LINEAR)?))
    }

    /// `src` must be in SHADER_READ_ONLY_OPTIMAL and the outputs in GENERAL.
    /// Without `aux` the main image gets plain 4:2:0 (Single420).
    pub(crate) fn record(
        &self,
        cmd: vk::CommandBuffer,
        src: &Image,
        main: &Image,
        aux: Option<&Image>,
        extent: (u32, u32),
        transform: u32,
    ) -> Result<()> {
        let aux_views = aux
            .map(|a| (a.plane_views[0], a.plane_views[1]))
            .unwrap_or((main.plane_views[0], main.plane_views[1]));
        let views = [
            src.view0(),
            main.plane_views[0],
            main.plane_views[1],
            aux_views.0,
            aux_views.1,
        ];
        self.0
            .dispatch(cmd, &views, extent.0, extent.1, aux.is_some(), transform)
    }
}

/// Decoded main (+ aux) NV12 -> BGRA display image.
pub(crate) struct Recombine(Pass);

impl Recombine {
    pub(crate) fn new(gpu: &Arc<Gpu>) -> Result<Self> {
        Ok(Self(Pass::new(
            gpu,
            RECOMBINE_SPV,
            4,
            1,
            vk::Filter::NEAREST,
        )?))
    }

    /// Inputs must be in SHADER_READ_ONLY_OPTIMAL and `dst` in GENERAL;
    /// `dst` is `zoom` times the stream size in each direction.
    pub(crate) fn record(
        &self,
        cmd: vk::CommandBuffer,
        main: &Image,
        aux: Option<&Image>,
        dst: &Image,
        (width, height): (u32, u32),
        zoom: u32,
    ) -> Result<()> {
        let aux_views = aux
            .map(|a| (a.plane_views[0], a.plane_views[1]))
            .unwrap_or((main.plane_views[0], main.plane_views[1]));
        let views = [
            main.plane_views[0],
            main.plane_views[1],
            aux_views.0,
            aux_views.1,
            dst.view0(),
        ];
        self.0
            .dispatch(cmd, &views, width, height, aux.is_some(), zoom.max(1))
    }
}
