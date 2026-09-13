//! The Vulkan objects underneath Metal's device-level API.
//!
//! A `Gpu` is one logical Vulkan device: the queue Metal submits to, the
//! memory it allocates from, and the caches which keep one Vulkan object per
//! distinct Metal description. Metal's own object graph lives in `lib.rs`;
//! nothing here knows about Objective-C.
//!
//! The device itself is here; allocation, submission and the recycled
//! pools are in [`memory`], [`submit`] and [`pools`], with the Metal
//! format tables in [`format`] and the tracing knobs in [`trace`].

mod format;
mod memory;
mod pools;
mod submit;
mod trace;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ash::{Device, Instance, vk};
use static_init::dynamic;

use crate::heap::{Allocation, Heap};

use format::sample_count;
pub use format::{aspect_of, block_extent, pixel_format, texel_size};
use trace::dump_spirv;
pub use trace::{milestone, trace, trace_once};

type DescriptorLayoutKey = (Vec<(u32, vk::DescriptorType)>, vk::ShaderStageFlags, bool);
type DescriptorLayouts = HashMap<DescriptorLayoutKey, vk::DescriptorSetLayout>;

/// One logical device and the queue Metal's single command-queue model uses.
pub struct Gpu {
    pub device: Device,
    pub physical: vk::PhysicalDevice,
    pub queue_family: u32,
    /// Vulkan requires external synchronization on a queue, and Metal lets any
    /// thread commit a command buffer.
    queue: Mutex<vk::Queue>,
    memory: vk::PhysicalDeviceMemoryProperties,
    pub limits: vk::PhysicalDeviceLimits,
    /// One layout per distinct set of Metal binding indices, so pipelines
    /// sharing a resource shape share a layout.
    layouts: Mutex<DescriptorLayouts>,
    render_passes: Mutex<HashMap<RenderPassKey, vk::RenderPass>>,
    /// The pool and fence behind `one_shot`, which runs the small transfers
    /// and layout transitions Metal performs outside a command buffer.
    immediate: Mutex<Option<Immediate>>,
    /// The blocks Metal buffers are cut out of, made on first use because the
    /// memory type depends on what a buffer's requirements admit.
    buffers: OnceLock<Heap>,
    /// Two-timestamp query pools, one per command buffer in flight, kept for
    /// reuse by command-buffer timing and the optional statistics.
    timestamp_pools: Mutex<Vec<vk::QueryPool>>,
    timestamp_period: f32,
    timestamp_valid_bits: u32,
    /// Command pools, descriptor pools and upload chunks whose submission has
    /// retired, kept for the next command buffer. A game commits several
    /// command buffers a frame, and creating and destroying each one's pool,
    /// fence and descriptor pools every frame is driver work on the thread the
    /// guest records on.
    contexts: Mutex<Vec<CommandContext>>,
    descriptor_pools: Mutex<Vec<vk::DescriptorPool>>,
    upload_chunks: Mutex<Vec<GpuBuffer>>,
    /// `VK_KHR_push_descriptor`, where advertised, and the descriptor limit
    /// which decides whether one set in a pipeline can use it.
    push_descriptor_api: Option<ash::khr::push_descriptor::Device>,
    pub max_push_descriptors: u32,
    /// `VK_KHR_swapchain`'s device functions, where the device has it. `None`
    /// is a device which cannot present at all, and presentation falls back
    /// to reading the frame back and blitting it through SDL's window
    /// surface.
    swapchain_api: Option<ash::khr::swapchain::Device>,
}

struct Immediate {
    pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
}

/// One command buffer with the pool it was allocated from and the fence its
/// submission signals.
pub struct CommandContext {
    pub pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub fence: vk::Fence,
}

/// How many retired objects of each kind are kept for reuse. Enough for the
/// command buffers of a few frames in flight; a burst beyond it is destroyed
/// rather than held for the rest of the run.
const RECYCLED: usize = 32;

/// The size of one chunk `-setBytes:` copies are cut from. Metal limits one
/// copy to four kilobytes, so a chunk holds a frame's worth of them.
pub const UPLOAD_CHUNK: u64 = 256 << 10;

/// A render pass is keyed by the formats and load/store actions of its
/// attachments: Vulkan bakes into the pass what Metal carries on the
/// descriptor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenderPassKey {
    pub colors: Vec<(vk::Format, vk::AttachmentLoadOp, vk::AttachmentStoreOp)>,
    /// The single-sample attachment each colour is resolved into, where the
    /// Metal store action asks for a resolve, and `None` where it does not.
    /// One entry per colour, because Vulkan's resolve list is positional.
    pub color_resolves: Vec<Option<vk::Format>>,
    pub depth: Option<(vk::Format, vk::AttachmentLoadOp, vk::AttachmentStoreOp)>,
    pub stencil: Option<(vk::AttachmentLoadOp, vk::AttachmentStoreOp)>,
    pub samples: vk::SampleCountFlags,
}

impl Gpu {
    pub fn create(instance: &Instance, physical: vk::PhysicalDevice) -> Result<Self, String> {
        // SAFETY: the physical device came from this live instance.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let queue_family = families
            .iter()
            .position(|family| {
                family
                    .queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
            .ok_or("the Vulkan device has no graphics and compute queue family")?
            as u32;

        // Vulkan accepts every feature the device reports, and Metal's own
        // feature set is wide: a translated shader may need any of Int64,
        // format-less storage images, cube arrays or BC compression. Enabling
        // what the device already advertises costs nothing and avoids a
        // per-feature list that would drift from the emitter.
        // SAFETY: the physical device belongs to this live instance.
        let features = unsafe { instance.get_physical_device_features(physical) };

        // SAFETY: the physical device belongs to this live instance.
        let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }
            .map_err(|error| format!("vkEnumerateDeviceExtensionProperties failed: {error:?}"))?;
        let has = |name: &std::ffi::CStr| {
            extensions.iter().any(|property| {
                property
                    .extension_name_as_c_str()
                    .is_ok_and(|available| available == name)
            })
        };
        let mut enabled: Vec<*const std::ffi::c_char> = Vec::new();
        let presents = has(ash::khr::swapchain::NAME);
        if presents {
            enabled.push(ash::khr::swapchain::NAME.as_ptr());
        }
        let pushes = has(ash::khr::push_descriptor::NAME);
        if pushes {
            enabled.push(ash::khr::push_descriptor::NAME.as_ptr());
        }

        // The same reasoning as for the core features: enable what the device
        // reports. A translated shader which phis a pointer declares
        // `VariablePointers`, and Vulkan requires the matching feature to have
        // been enabled at device creation.
        let mut pointers = vk::PhysicalDeviceVariablePointersFeatures::default();
        let mut reported = vk::PhysicalDeviceFeatures2::default().push_next(&mut pointers);
        // SAFETY: the physical device belongs to this live instance, and both
        // structures live across the call.
        unsafe { instance.get_physical_device_features2(physical, &mut reported) };

        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_features(&features)
            .enabled_extension_names(&enabled)
            .push_next(&mut pointers);
        // SAFETY: every borrowed structure lives across the call.
        let device = unsafe { instance.create_device(physical, &create_info, None) }
            .map_err(|error| format!("vkCreateDevice failed with {error:?}"))?;
        // SAFETY: the family was requested above with one queue.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        // SAFETY: the physical device belongs to this live instance.
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        // SAFETY: as above.
        let properties = unsafe { instance.get_physical_device_properties(physical) };

        let swapchain_api = presents.then(|| ash::khr::swapchain::Device::new(instance, &device));
        let push_descriptor_api =
            pushes.then(|| ash::khr::push_descriptor::Device::new(instance, &device));
        let max_push_descriptors = if pushes {
            let mut push = vk::PhysicalDevicePushDescriptorPropertiesKHR::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut push);
            // SAFETY: the physical device belongs to this live instance, and
            // both structures live across the call.
            unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
            push.max_push_descriptors
        } else {
            0
        };

        Ok(Self {
            device,
            physical,
            queue_family,
            queue: Mutex::new(queue),
            memory,
            limits: properties.limits,
            layouts: Mutex::new(HashMap::new()),
            render_passes: Mutex::new(HashMap::new()),
            immediate: Mutex::new(None),
            buffers: OnceLock::new(),
            timestamp_pools: Mutex::new(Vec::new()),
            timestamp_period: properties.limits.timestamp_period,
            timestamp_valid_bits: families[queue_family as usize].timestamp_valid_bits,
            contexts: Mutex::new(Vec::new()),
            descriptor_pools: Mutex::new(Vec::new()),
            upload_chunks: Mutex::new(Vec::new()),
            push_descriptor_api,
            max_push_descriptors,
            swapchain_api,
        })
    }
}

/// Metal's `MTLTextureUsage`.
pub const USAGE_SHADER_READ: u64 = 1;
pub const USAGE_SHADER_WRITE: u64 = 2;
pub const USAGE_RENDER_TARGET: u64 = 4;

pub struct GpuBuffer {
    pub buffer: vk::Buffer,
    pub allocation: Allocation,
    pub contents: *mut u8,
    pub length: u64,
}

// SAFETY: `contents` is a permanent mapping of device memory, valid on every
// thread; see `Allocation`.
unsafe impl Send for GpuBuffer {}

pub struct GpuImage {
    pub image: vk::Image,
    pub memory: Option<vk::DeviceMemory>,
    pub view: vk::ImageView,
    pub format: vk::Format,
    pub aspect: vk::ImageAspectFlags,
    pub width: u32,
    pub height: u32,
    pub levels: u32,
    pub layers: u32,
    pub samples: vk::SampleCountFlags,
}

impl GpuBuffer {
    /// The buffer left behind when a real one is destroyed: it is never bound,
    /// because the object holding it is being deallocated.
    pub fn none() -> Self {
        Self {
            buffer: vk::Buffer::null(),
            allocation: Allocation::none(),
            contents: std::ptr::null_mut(),
            length: 0,
        }
    }
}

impl GpuImage {
    /// The image left behind when a drawable's own image is destroyed. It is
    /// never bound: the texture it belongs to is unreachable by then.
    pub fn none() -> Self {
        Self {
            image: vk::Image::null(),
            memory: None,
            view: vk::ImageView::null(),
            format: vk::Format::UNDEFINED,
            aspect: vk::ImageAspectFlags::COLOR,
            width: 0,
            height: 0,
            levels: 1,
            layers: 1,
            samples: vk::SampleCountFlags::TYPE_1,
        }
    }
}

pub struct ImageDescription {
    pub image_type: vk::ImageType,
    pub view_type: vk::ImageViewType,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub levels: u32,
    pub layers: u32,
    pub samples: u64,
    pub usage: u64,
    pub cube: bool,
}
