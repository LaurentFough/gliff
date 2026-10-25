//! Instance, device, queues and the small allocation/submission helpers the
//! rest of the crate builds on.

use std::ffi::{c_char, CStr};
use std::path::Path;
use std::sync::Arc;

use ash::vk;

use crate::{Error, Result};

/// Queue family indices.
#[derive(Debug, Clone, Copy)]
pub struct Families {
    pub compute: u32,
    /// The Vulkan Video H.264 encode family, when the device has one.
    pub encode: Option<u32>,
}

impl Families {
    /// Distinct families, for `SHARING_MODE_CONCURRENT` image creation.
    pub fn all(&self) -> Vec<u32> {
        let mut all = vec![self.compute];
        all.extend(self.encode.filter(|&e| e != self.compute));
        all
    }
}

/// H.264 encode through Vulkan Video (`VK_KHR_video_encode_h264`): the
/// queues, the extension tables, and what the driver reported for the
/// profile `crate::vkenc` uses.
pub(crate) struct VideoEncode {
    pub(crate) family: u32,
    pub(crate) queues: Vec<vk::Queue>,
    pub(crate) queue_fns: ash::khr::video_queue::Device,
    pub(crate) encode_fns: ash::khr::video_encode_queue::Device,
    pub(crate) max_extent: (u32, u32),
    pub(crate) std_header: vk::ExtensionProperties,
    pub(crate) bitstream_alignment: u64,
    pub(crate) usage_hints: bool,
}

/// What the Vulkan Video H.264 encoder offers, for `gliff-probe`.
#[derive(Debug, Clone)]
pub struct VkEncodeInfo {
    /// Largest coded size the device encodes.
    pub max_extent: (u32, u32),
    /// Encode queues the pipeline got: two means main and aux encode in parallel.
    pub queues: usize,
    /// The video std header the driver implements, and its version.
    pub std_header: String,
    pub std_header_version: String,
}

/// One Vulkan device with the queues, extension tables and pools the media
/// pipeline uses. Not thread-safe: every pipeline built on it lives on the
/// thread that created it, as the VA-API path did.
pub struct Gpu {
    pub(crate) instance: ash::Instance,
    pub(crate) physical: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    pub(crate) families: Families,
    pub(crate) compute_queue: vk::Queue,
    pub(crate) memory: vk::PhysicalDeviceMemoryProperties,
    pub(crate) external_fd: ash::khr::external_memory_fd::Device,
    pub(crate) drm_modifier: ash::ext::image_drm_format_modifier::Device,
    pub name: String,
    pub driver: String,
    /// The VA-API display on the same render node, and what it offers.
    pub va: Arc<gliff_va::Display>,
    pub va_caps: gliff_va::Caps,
    /// Vulkan Video H.264 encode, the codec for GPUs whose VA-API driver
    /// has no encoder (NVIDIA). `GLIFF_VK_ENCODE=0` leaves it off.
    pub(crate) video_encode: Option<VideoEncode>,
    /// The queue family index that stands for VA-API in ownership
    /// transfers: FOREIGN when the driver has it, else EXTERNAL.
    pub(crate) foreign_family: u32,
    _entry: ash::Entry,
}

const REQUIRED_EXTENSIONS: &[&CStr] = &[
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

const OPTIONAL_EXTENSIONS: &[&CStr] = &[ash::ext::queue_family_foreign::NAME];

const VIDEO_ENCODE_EXTENSIONS: &[&CStr] = &[
    ash::khr::video_queue::NAME,
    ash::khr::video_encode_queue::NAME,
    ash::khr::video_encode_h264::NAME,
];

impl Gpu {
    /// Open the GPU behind `render_node` (any suitable one when `None`).
    pub fn open(render_node: Option<&Path>) -> Result<Arc<Self>> {
        let node = render_node.unwrap_or(Path::new("/dev/dri/renderD128"));
        let va = gliff_va::Display::open(node)?;
        let va_caps = va.caps()?;
        tracing::info!(vendor = %va.vendor, ?va_caps, "va-api driver");
        // SAFETY: loading libvulkan and creating an instance with valid,
        // NUL-terminated names; nothing outlives the entry it came from.
        unsafe {
            let entry = ash::Entry::load()?;
            let app = vk::ApplicationInfo::default()
                .application_name(c"gliff")
                .api_version(vk::API_VERSION_1_3);
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )?;
            let wanted = render_node.and_then(drm_dev_number);
            let mut chosen = None;
            for pd in instance.enumerate_physical_devices()? {
                if let Some(want) = wanted {
                    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
                    instance.get_physical_device_properties2(pd, &mut props);
                    let matches = (drm.has_render == vk::TRUE
                        && (drm.render_major, drm.render_minor) == want)
                        || (drm.has_primary == vk::TRUE
                            && (drm.primary_major, drm.primary_minor) == want);
                    if !matches {
                        continue;
                    }
                }
                let exts: Vec<String> = instance
                    .enumerate_device_extension_properties(pd)?
                    .iter()
                    .map(|e| {
                        CStr::from_ptr(e.extension_name.as_ptr())
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                let has = |n: &CStr| exts.iter().any(|e| e.as_str() == n.to_str().unwrap_or(""));
                if !REQUIRED_EXTENSIONS.iter().all(|n| has(n)) {
                    continue;
                }
                let Some(mut families) = pick_families(&instance, pd) else {
                    continue;
                };
                let video_wanted = std::env::var("GLIFF_VK_ENCODE").map_or(true, |v| v != "0");
                if !(video_wanted && VIDEO_ENCODE_EXTENSIONS.iter().all(|n| has(n))) {
                    families.encode = None;
                }
                chosen = Some((pd, families, exts));
                break;
            }
            let Some((physical, families, exts)) = chosen else {
                instance.destroy_instance(None);
                return Err(Error::NoDevice(
                    "no Vulkan device with a compute queue and dmabuf import",
                ));
            };
            let has = |n: &CStr| exts.iter().any(|e| e.as_str() == n.to_str().unwrap_or(""));
            let mut names: Vec<*const c_char> =
                REQUIRED_EXTENSIONS.iter().map(|n| n.as_ptr()).collect();
            names.extend(
                OPTIONAL_EXTENSIONS
                    .iter()
                    .filter(|n| has(n))
                    .map(|n| n.as_ptr()),
            );
            // The encode capabilities decide whether the family is used at
            // all; a driver that lists the extensions but refuses the
            // profile leaves the device as it was.
            let mut families = families;
            let video_caps = families
                .encode
                .and_then(|_| crate::vkenc::query_caps(&entry, &instance, physical));
            if video_caps.is_none() {
                families.encode = None;
            }
            if families.encode.is_some() {
                names.extend(VIDEO_ENCODE_EXTENSIONS.iter().map(|n| n.as_ptr()));
            }
            // One encode queue per stream (main and aux) when the family
            // has two, so the streams encode in parallel.
            let encode_queue_count = families
                .encode
                .map(|f| queue_count(&instance, physical, f).min(2))
                .unwrap_or(0);

            let priority = [1.0f32, 1.0];
            let queue_infos: Vec<vk::DeviceQueueCreateInfo> = families
                .all()
                .into_iter()
                .map(|f| {
                    let n = if Some(f) == families.encode {
                        encode_queue_count
                    } else {
                        1
                    };
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(f)
                        .queue_priorities(&priority[..n as usize])
                })
                .collect();
            // Image robustness makes a shader fetch outside an image read
            // zero instead of undefined memory, as a second line behind the
            // decoders' own picture size checks. Optional: not every driver
            // has it.
            let robust_image_access = {
                let mut supported = vk::PhysicalDeviceVulkan13Features::default();
                let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut supported);
                instance.get_physical_device_features2(physical, &mut features);
                supported.robust_image_access == vk::TRUE
            };
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
                .synchronization2(true)
                .robust_image_access(robust_image_access);
            let create = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .enabled_extension_names(&names)
                .push_next(&mut f12)
                .push_next(&mut f13);
            let device = instance.create_device(physical, &create, None)?;
            let foreign_family = if has(ash::ext::queue_family_foreign::NAME) {
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                vk::QUEUE_FAMILY_EXTERNAL
            };

            let props = instance.get_physical_device_properties(physical);
            let mut drv = vk::PhysicalDeviceDriverProperties::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drv);
            instance.get_physical_device_properties2(physical, &mut p2);
            let name = CStr::from_ptr(props.device_name.as_ptr())
                .to_string_lossy()
                .into_owned();
            let driver = format!(
                "{} {}",
                CStr::from_ptr(drv.driver_name.as_ptr()).to_string_lossy(),
                CStr::from_ptr(drv.driver_info.as_ptr()).to_string_lossy()
            );
            tracing::info!(%name, %driver, ?families, robust_image_access, "vulkan device");
            let video_encode = match (families.encode, video_caps) {
                (Some(family), Some(caps)) => {
                    tracing::info!(
                        max = ?caps.max_extent,
                        queues = encode_queue_count,
                        "vulkan video h264 encode"
                    );
                    Some(VideoEncode {
                        family,
                        queues: (0..encode_queue_count)
                            .map(|i| device.get_device_queue(family, i))
                            .collect(),
                        queue_fns: ash::khr::video_queue::Device::new(&instance, &device),
                        encode_fns: ash::khr::video_encode_queue::Device::new(&instance, &device),
                        max_extent: caps.max_extent,
                        std_header: caps.std_header,
                        bitstream_alignment: caps.bitstream_alignment,
                        usage_hints: caps.usage_hints,
                    })
                }
                _ => None,
            };

            let gpu = Self {
                compute_queue: device.get_device_queue(families.compute, 0),
                memory: instance.get_physical_device_memory_properties(physical),
                external_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                drm_modifier: ash::ext::image_drm_format_modifier::Device::new(&instance, &device),
                name,
                driver,
                va,
                va_caps,
                video_encode,
                foreign_family,
                families,
                instance,
                physical,
                device,
                _entry: entry,
            };
            Ok(Arc::new(gpu))
        }
    }

    /// Whether the GPU encodes H.264 at all: through VA-API, or through
    /// Vulkan Video when the VA-API driver has no encoder.
    pub fn can_encode(&self) -> bool {
        self.can_va_encode() || self.video_encode.is_some()
    }

    pub fn can_va_encode(&self) -> bool {
        self.va_caps.can_encode().is_ok()
    }

    /// Which encoder `Encoder` uses, for logs.
    pub fn encoder_api(&self) -> &'static str {
        if self.can_va_encode() {
            "va-api"
        } else if self.video_encode.is_some() {
            "vulkan video"
        } else {
            "none"
        }
    }

    /// What the Vulkan Video encoder offers, when the device has one. `None`
    /// when `VK_KHR_video_encode_h264` is missing, the driver refuses the
    /// profile, or `GLIFF_VK_ENCODE=0`.
    pub fn vk_encode_info(&self) -> Option<VkEncodeInfo> {
        let video = self.video_encode.as_ref()?;
        let spec = video.std_header.spec_version;
        Some(VkEncodeInfo {
            max_extent: video.max_extent,
            queues: video.queues.len(),
            std_header: video
                .std_header
                .extension_name_as_c_str()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unnamed".into()),
            std_header_version: format!(
                "{}.{}.{}",
                vk::api_version_major(spec),
                vk::api_version_minor(spec),
                vk::api_version_patch(spec)
            ),
        })
    }

    pub fn can_decode(&self) -> bool {
        self.va_caps.can_decode().is_ok()
    }

    /// Index of a memory type allowed by `type_bits` with all of `flags`.
    pub(crate) fn memory_type(
        &self,
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<u32> {
        (0..self.memory.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && self.memory.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| Error::Unsupported(format!("no memory type for {flags:?}")))
    }

    /// Allocate memory for `reqs`, with an optional extra create-info chain
    /// (import/export/dedicated).
    pub(crate) fn allocate(
        &self,
        reqs: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
        chain: Option<&mut dyn vk::ExtendsMemoryAllocateInfo>,
    ) -> Result<vk::DeviceMemory> {
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(self.memory_type(reqs.memory_type_bits, flags)?);
        if let Some(c) = chain {
            info = info.push_next(c);
        }
        // SAFETY: a valid allocate info; the caller frees the memory.
        Ok(unsafe { self.device.allocate_memory(&info, None) }?)
    }

    /// How many memory planes an image of `format` with `modifier` has, and
    /// the features the driver gives it, or `None` when the driver does not
    /// list the modifier for the format.
    pub(crate) fn modifier_properties(
        &self,
        format: vk::Format,
        modifier: u64,
    ) -> Option<(u32, vk::FormatFeatureFlags)> {
        // SAFETY: two-call pattern on a valid physical device; the list is
        // sized from the first call before the second fills it.
        unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance
                .get_physical_device_format_properties2(self.physical, format, &mut props);
            let mut entries = vec![
                vk::DrmFormatModifierPropertiesEXT::default();
                list.drm_format_modifier_count as usize
            ];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut entries);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance
                .get_physical_device_format_properties2(self.physical, format, &mut props);
            entries
                .iter()
                .find(|e| e.drm_format_modifier == modifier)
                .map(|e| {
                    (
                        e.drm_format_modifier_plane_count,
                        e.drm_format_modifier_tiling_features,
                    )
                })
        }
    }

    /// Wait for the whole device to go idle (teardown, resize).
    pub fn wait_idle(&self) {
        // SAFETY: valid device.
        let _ = unsafe { self.device.device_wait_idle() };
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: every object created from this device was destroyed by its
        // owner before the Arc<Gpu> count reached zero.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn drm_dev_number(path: &Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    // Linux dev_t layout: major in bits 8..20 (and 32..), minor in 0..8 and 20..32.
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    Some((major as i64, minor as i64))
}

/// Choose a compute family, with no graphics preferred so it never
/// competes with the compositor.
fn pick_families(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<Families> {
    // SAFETY: valid physical device.
    let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let flags = |i: usize| props[i].queue_flags;
    let compute = (0..props.len())
        .find(|&i| {
            flags(i).contains(vk::QueueFlags::COMPUTE)
                && !flags(i).contains(vk::QueueFlags::GRAPHICS)
        })
        .or_else(|| (0..props.len()).find(|&i| flags(i).contains(vk::QueueFlags::COMPUTE)))?;
    let encode = (0..props.len()).find(|&i| {
        flags(i).contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
            && crate::vkenc::family_encodes_h264(instance, pd, i as u32)
    });
    Some(Families {
        compute: compute as u32,
        encode: encode.map(|e| e as u32),
    })
}

fn queue_count(instance: &ash::Instance, pd: vk::PhysicalDevice, family: u32) -> u32 {
    // SAFETY: valid physical device.
    let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    props.get(family as usize).map_or(0, |p| p.queue_count)
}

/// A command pool with one command buffer plus a fence, for one-shot
/// submissions on one queue.
pub(crate) struct Commands {
    gpu: Arc<Gpu>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    pub(crate) queue: vk::Queue,
    fence: vk::Fence,
}

impl Commands {
    pub(crate) fn new(gpu: &Arc<Gpu>, family: u32, queue: vk::Queue) -> Result<Self> {
        // SAFETY: valid device and family index. The fence starts signalled
        // so the first `run` does not wait on work that was never submitted.
        let (pool, cmd, fence) = unsafe {
            let info = vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::TRANSIENT);
            let pool = gpu.device.create_command_pool(&info, None)?;
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let fence = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
            (
                pool,
                gpu.device.allocate_command_buffers(&alloc)?[0],
                gpu.device.create_fence(&fence, None)?,
            )
        };
        Ok(Self {
            gpu: gpu.clone(),
            pool,
            cmd,
            queue,
            fence,
        })
    }

    /// Record with `f` and submit. Waits for `wait` (a timeline value) first
    /// and signals `signal` after; when `block` the call returns only once
    /// the work has finished.
    pub(crate) fn run(
        &self,
        timeline: vk::Semaphore,
        wait: Option<u64>,
        signal: Option<u64>,
        block: bool,
        f: impl FnOnce(vk::CommandBuffer) -> Result<()>,
    ) -> Result<()> {
        let dev = &self.gpu.device;
        let cmd = self.cmd;
        // SAFETY: the pool is reset only after the previous submission's fence
        // signalled (`block`, or the wait below), so the command buffer is not
        // in flight when it is recycled. A reset returns it to the initial
        // state but does not free it, so it is allocated once, in `new`.
        unsafe {
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
            dev.reset_fences(&[self.fence])?;
            dev.reset_command_pool(self.pool, vk::CommandPoolResetFlags::empty())?;
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            f(cmd)?;
            dev.end_command_buffer(cmd)?;

            let waits: Vec<vk::SemaphoreSubmitInfo> = wait
                .map(|v| {
                    vk::SemaphoreSubmitInfo::default()
                        .semaphore(timeline)
                        .value(v)
                        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                })
                .into_iter()
                .collect();
            let signals: Vec<vk::SemaphoreSubmitInfo> = signal
                .map(|v| {
                    vk::SemaphoreSubmitInfo::default()
                        .semaphore(timeline)
                        .value(v)
                        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                })
                .into_iter()
                .collect();
            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let submit = vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&cmds)
                .signal_semaphore_infos(&signals);
            dev.queue_submit2(self.queue, &[submit], self.fence)?;
            if block {
                dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
            }
        }
        Ok(())
    }

    pub(crate) fn wait(&self) -> Result<()> {
        // SAFETY: valid fence.
        unsafe {
            self.gpu
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)?
        };
        Ok(())
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        // SAFETY: wait for in-flight work before destroying the pool.
        unsafe {
            let _ = self
                .gpu
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX);
            self.gpu.device.destroy_fence(self.fence, None);
            self.gpu.device.destroy_command_pool(self.pool, None);
        }
    }
}

/// A timeline semaphore the compute submissions can be ordered by.
pub(crate) struct Timeline {
    gpu: Arc<Gpu>,
    pub(crate) semaphore: vk::Semaphore,
}

impl Timeline {
    pub(crate) fn new(gpu: &Arc<Gpu>) -> Result<Self> {
        let mut kind = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
        // SAFETY: valid device and create info.
        let semaphore = unsafe { gpu.device.create_semaphore(&info, None) }?;
        Ok(Self {
            gpu: gpu.clone(),
            semaphore,
        })
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        // SAFETY: owners wait for their work before dropping.
        unsafe { self.gpu.device.destroy_semaphore(self.semaphore, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_records_every_submission_into_the_same_command_buffer() {
        let Ok(gpu) = Gpu::open(None) else {
            eprintln!("no GPU; skipping");
            return;
        };
        let commands = Commands::new(&gpu, gpu.families.compute, gpu.compute_queue).unwrap();
        let timeline = Timeline::new(&gpu).unwrap();
        let mut recorded = Vec::new();
        for _ in 0..3 {
            commands
                .run(timeline.semaphore, None, None, true, |cmd| {
                    recorded.push(cmd);
                    Ok(())
                })
                .unwrap();
        }
        assert!(
            recorded.iter().all(|&cmd| cmd == recorded[0]),
            "{recorded:?}"
        );
    }
}
