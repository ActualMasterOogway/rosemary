//! The Vulkan objects underneath Metal's device-level API.
//!
//! A `Gpu` is one logical Vulkan device: the queue Metal submits to, the
//! memory it allocates from, and the caches which keep one Vulkan object per
//! distinct Metal description. Metal's own object graph lives in `lib.rs`;
//! nothing here knows about Objective-C.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ash::{Device, Instance, vk};

use crate::heap::{Allocation, Heap};

/// One logical device and the queue Metal's single command-queue model uses.
pub struct Gpu {
    pub device: Device,
    pub queue_family: u32,
    /// Vulkan requires external synchronization on a queue, and Metal lets any
    /// thread commit a command buffer.
    queue: Mutex<vk::Queue>,
    memory: vk::PhysicalDeviceMemoryProperties,
    pub limits: vk::PhysicalDeviceLimits,
    /// One layout per distinct set of Metal binding indices, so pipelines
    /// sharing a resource shape share a layout.
    layouts: Mutex<HashMap<Vec<(u32, vk::DescriptorType)>, vk::DescriptorSetLayout>>,
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
}

struct Immediate {
    pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
}

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
        if has(ash::khr::swapchain::NAME) {
            enabled.push(ash::khr::swapchain::NAME.as_ptr());
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

        Ok(Self {
            device,
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
        })
    }

    fn memory_type(&self, bits: u32, wanted: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.memory.memory_type_count).find(|index| {
            bits & (1 << index) != 0
                && self.memory.memory_types[*index as usize]
                    .property_flags
                    .contains(wanted)
        })
    }

    /// Allocate memory for `requirements`, preferring `wanted` and falling back
    /// to `accepted` when the device has no such heap.
    fn allocate(
        &self,
        requirements: vk::MemoryRequirements,
        wanted: vk::MemoryPropertyFlags,
        accepted: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory, String> {
        let index = self
            .memory_type(requirements.memory_type_bits, wanted)
            .or_else(|| self.memory_type(requirements.memory_type_bits, accepted))
            .ok_or("no Vulkan memory type satisfies the allocation")?;
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: a well-formed allocation on this live device.
        unsafe { self.device.allocate_memory(&info, None) }
            .map_err(|error| format!("vkAllocateMemory failed with {error:?}"))
    }

    /// A Metal buffer: host visible so `-contents` can hand the guest a
    /// pointer, and usable by every stage Metal allows a buffer in.
    ///
    /// The memory is ordinary host memory, cached where the device offers it.
    /// A heap which is host-visible *and* device-local looks like the better
    /// answer and is the worse one: on a discrete adapter that is the PCI
    /// aperture, a few hundred megabytes wide and uncached, so a game which
    /// streams its world in overruns it and every byte the guest reads back
    /// through `-contents` crosses the bus. `MTLStorageModeShared` on a Mac
    /// with a discrete GPU is system memory too.
    pub fn create_buffer(&self, length: u64) -> Result<GpuBuffer, String> {
        let usage = vk::BufferUsageFlags::STORAGE_BUFFER
            | vk::BufferUsageFlags::UNIFORM_BUFFER
            | vk::BufferUsageFlags::VERTEX_BUFFER
            | vk::BufferUsageFlags::INDEX_BUFFER
            | vk::BufferUsageFlags::INDIRECT_BUFFER
            | vk::BufferUsageFlags::TRANSFER_SRC
            | vk::BufferUsageFlags::TRANSFER_DST;
        let info = vk::BufferCreateInfo::default()
            .size(length.max(4))
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: a well-formed create info on this live device.
        let buffer = unsafe { self.device.create_buffer(&info, None) }
            .map_err(|error| format!("vkCreateBuffer failed with {error:?}"))?;
        // SAFETY: the buffer was just created by this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let heap = self
            .buffer_heap(requirements.memory_type_bits)
            .inspect_err(|_| {
                // SAFETY: nothing is bound to the buffer yet.
                unsafe { self.device.destroy_buffer(buffer, None) };
            })?;
        let allocation = heap
            .allocate(&self.device, requirements.size, requirements.alignment)
            .inspect_err(|_| {
                // SAFETY: as above.
                unsafe { self.device.destroy_buffer(buffer, None) };
            })?;
        // SAFETY: a fresh, unbound buffer and a range nothing else holds.
        unsafe {
            self.device
                .bind_buffer_memory(buffer, allocation.memory, allocation.offset())
        }
        .map_err(|error| format!("vkBindBufferMemory failed with {error:?}"))?;
        // A guest which reads a fresh buffer before writing it must not see the
        // bytes of a buffer this block last held. Memory the driver has just
        // handed over is already zero, and during a streaming burst - which is
        // where this matters, because it is gigabytes a second - almost all of
        // it is, so only a reused range is written over.
        if !allocation.pristine {
            // SAFETY: the mapping covers this whole range.
            crate::stats::timed(crate::stats::Counter::BufferZeroMicros, || unsafe {
                std::ptr::write_bytes(allocation.contents, 0, length as usize)
            });
        }
        Ok(GpuBuffer {
            buffer,
            allocation,
            contents: allocation.contents,
            length,
        })
    }

    /// Destroy a buffer this device made and return its range to the heap.
    ///
    /// The caller must have waited for every submission which referred to it.
    pub fn destroy_buffer(&self, buffer: GpuBuffer) {
        // SAFETY: the caller guarantees the buffer is idle.
        unsafe { self.device.destroy_buffer(buffer.buffer, None) };
        if let Ok(heap) = self.buffer_heap(u32::MAX) {
            heap.free(&self.device, buffer.allocation);
        }
    }

    /// The heap every Metal buffer is cut from.
    ///
    /// One heap, not one per requirement mask: every buffer this crate makes is
    /// created with the same usage flags, so every one of them reports the same
    /// admissible memory types.
    fn buffer_heap(&self, bits: u32) -> Result<&Heap, String> {
        if let Some(heap) = self.buffers.get() {
            return Ok(heap);
        }
        let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let index = self
            .memory_type(bits, host | vk::MemoryPropertyFlags::HOST_CACHED)
            .or_else(|| self.memory_type(bits, host))
            .ok_or("no Vulkan memory type is host visible and coherent")?;
        Ok(self.buffers.get_or_init(|| Heap::new(index)))
    }

    pub fn create_image(&self, description: &ImageDescription) -> Result<GpuImage, String> {
        let mut usage = vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST;
        if description.usage & USAGE_SHADER_READ != 0 {
            usage |= vk::ImageUsageFlags::SAMPLED;
        }
        if description.usage & USAGE_SHADER_WRITE != 0 {
            usage |= vk::ImageUsageFlags::STORAGE;
        }
        if description.usage & USAGE_RENDER_TARGET != 0 || description.usage == 0 {
            usage |= match aspect_of(description.format) {
                aspect if aspect.contains(vk::ImageAspectFlags::DEPTH) => {
                    vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
                }
                _ => vk::ImageUsageFlags::COLOR_ATTACHMENT,
            };
        }
        if description.usage == 0 {
            usage |= vk::ImageUsageFlags::SAMPLED;
        }
        let mut flags = vk::ImageCreateFlags::empty();
        if description.cube {
            flags |= vk::ImageCreateFlags::CUBE_COMPATIBLE;
        }
        let info = vk::ImageCreateInfo::default()
            .flags(flags)
            .image_type(description.image_type)
            .format(description.format)
            .extent(vk::Extent3D {
                width: description.width.max(1),
                height: description.height.max(1),
                depth: description.depth.max(1),
            })
            .mip_levels(description.levels.max(1))
            .array_layers(description.layers.max(1))
            .samples(sample_count(description.samples))
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: a well-formed create info on this live device.
        let image = unsafe { self.device.create_image(&info, None) }
            .map_err(|error| format!("vkCreateImage failed with {error:?}"))?;
        // SAFETY: the image was just created by this device.
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let memory = self
            .allocate(
                requirements,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                vk::MemoryPropertyFlags::empty(),
            )
            .inspect_err(|_| {
                // SAFETY: nothing is bound to the image yet.
                unsafe { self.device.destroy_image(image, None) };
            })?;
        // SAFETY: fresh, unbound image and memory from this device.
        unsafe { self.device.bind_image_memory(image, memory, 0) }
            .map_err(|error| format!("vkBindImageMemory failed with {error:?}"))?;
        let aspect = aspect_of(description.format);
        let levels = description.levels.max(1);
        let layers = description.layers.max(1);
        // Every texture stays in `GENERAL` from creation: see `render_pass`.
        self.one_shot(|device, command_buffer| {
            let barrier = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: aspect,
                    base_mip_level: 0,
                    level_count: levels,
                    base_array_layer: 0,
                    layer_count: layers,
                })
                .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
            // SAFETY: recording into a command buffer this call owns.
            unsafe {
                device.cmd_pipeline_barrier(
                    command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                )
            };
        })?;
        let view = self.create_view(image, description)?;
        Ok(GpuImage {
            image,
            memory: Some(memory),
            view,
            format: description.format,
            aspect: aspect_of(description.format),
            width: description.width.max(1),
            height: description.height.max(1),
            levels: description.levels.max(1),
            layers: description.layers.max(1),
            samples: sample_count(description.samples),
        })
    }

    fn create_view(
        &self,
        image: vk::Image,
        description: &ImageDescription,
    ) -> Result<vk::ImageView, String> {
        let view_type = description.view_type;
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(view_type)
            .format(description.format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect_of(description.format),
                base_mip_level: 0,
                level_count: description.levels.max(1),
                base_array_layer: 0,
                layer_count: description.layers.max(1),
            });
        // SAFETY: the image belongs to this device and outlives the view.
        unsafe { self.device.create_image_view(&info, None) }
            .map_err(|error| format!("vkCreateImageView failed with {error:?}"))
    }

    /// A view of one mip level of one array slice, which is what a framebuffer
    /// attachment must be.
    ///
    /// A texture's own view covers every level and every layer, and is what a
    /// shader samples. Metal names a level and a slice on a render pass
    /// attachment, and a game which renders a blur pyramid names a different
    /// one in every pass.
    pub fn create_attachment_view(
        &self,
        image: &GpuImage,
        level: u32,
        slice: u32,
    ) -> Result<vk::ImageView, String> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(image.format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: image.aspect,
                base_mip_level: level.min(image.levels.saturating_sub(1)),
                level_count: 1,
                base_array_layer: slice.min(image.layers.saturating_sub(1)),
                layer_count: 1,
            });
        // SAFETY: the image belongs to this device and outlives the view.
        unsafe { self.device.create_image_view(&info, None) }
            .map_err(|error| format!("vkCreateImageView failed with {error:?}"))
    }

    pub fn create_shader_module(
        &self,
        words: &[u32],
        entry: &str,
    ) -> Result<vk::ShaderModule, String> {
        dump_spirv(entry, words);
        let info = vk::ShaderModuleCreateInfo::default().code(words);
        // SAFETY: `words` lives across the call and is SPIR-V this process emitted.
        unsafe { self.device.create_shader_module(&info, None) }
            .map_err(|error| format!("vkCreateShaderModule failed with {error:?}"))
    }

    /// One descriptor-set layout per distinct binding shape, shared across the
    /// pipelines which declare it.
    pub fn descriptor_set_layout(
        &self,
        bindings: &[(u32, vk::DescriptorType)],
        stages: vk::ShaderStageFlags,
    ) -> Result<vk::DescriptorSetLayout, String> {
        let mut key = bindings.to_vec();
        key.sort_unstable();
        key.dedup();
        let mut cache = self.layouts.lock().expect("descriptor layout cache");
        if let Some(layout) = cache.get(&key) {
            return Ok(*layout);
        }
        let entries = key
            .iter()
            .map(|(binding, kind)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(*binding)
                    .descriptor_type(*kind)
                    .descriptor_count(1)
                    .stage_flags(stages)
            })
            .collect::<Vec<_>>();
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&entries);
        // SAFETY: the bindings live across the call.
        let layout = unsafe { self.device.create_descriptor_set_layout(&info, None) }
            .map_err(|error| format!("vkCreateDescriptorSetLayout failed with {error:?}"))?;
        cache.insert(key, layout);
        Ok(layout)
    }

    /// Every attachment is used in `GENERAL` layout.
    ///
    /// Metal has no image layouts: a texture is a texture, and a game rebinds
    /// one as an attachment, a sampled texture and a blit source in any order.
    /// Reconstructing Vulkan's optimal layouts would need per-subresource
    /// tracking across command buffers and threads. `GENERAL` is valid for
    /// every use and costs bandwidth on some hardware, not correctness.
    ///
    /// Using the optimal layout inside the pass and `GENERAL` outside it, which
    /// needs no tracking at all, was measured and is much worse: the home page
    /// went from 6.65 ms of device time a frame to 12.0 ms on a GTX 1660 SUPER,
    /// because each of the twenty-five passes in a frame then decompresses its
    /// attachments on the way out.
    pub fn render_pass(&self, key: &RenderPassKey) -> Result<vk::RenderPass, String> {
        let mut cache = self.render_passes.lock().expect("render pass cache");
        if let Some(pass) = cache.get(key) {
            return Ok(*pass);
        }
        let mut attachments = Vec::new();
        let mut colors = Vec::new();
        for (format, load, store) in &key.colors {
            colors.push(
                vk::AttachmentReference::default()
                    .attachment(attachments.len() as u32)
                    .layout(vk::ImageLayout::GENERAL),
            );
            attachments.push(
                vk::AttachmentDescription::default()
                    .format(*format)
                    .samples(key.samples)
                    .load_op(*load)
                    .store_op(*store)
                    .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                    .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                    .initial_layout(vk::ImageLayout::GENERAL)
                    .final_layout(vk::ImageLayout::GENERAL),
            );
        }
        let depth = key.depth.map(|(format, load, store)| {
            let (stencil_load, stencil_store) = key.stencil.unwrap_or((
                vk::AttachmentLoadOp::DONT_CARE,
                vk::AttachmentStoreOp::DONT_CARE,
            ));
            let reference = vk::AttachmentReference::default()
                .attachment(attachments.len() as u32)
                .layout(vk::ImageLayout::GENERAL);
            attachments.push(
                vk::AttachmentDescription::default()
                    .format(format)
                    .samples(key.samples)
                    .load_op(load)
                    .store_op(store)
                    .stencil_load_op(stencil_load)
                    .stencil_store_op(stencil_store)
                    .initial_layout(vk::ImageLayout::GENERAL)
                    .final_layout(vk::ImageLayout::GENERAL),
            );
            reference
        });
        // Metal resolves a multisampled attachment by naming the destination
        // on the pass descriptor; Vulkan does it with a second attachment the
        // subpass names positionally, so a colour without a resolve still needs
        // an entry. The resolve attachments follow the depth one, which is the
        // order `begin_render_pass` builds the framebuffer's views in.
        let mut resolves = Vec::new();
        if key.color_resolves.iter().any(Option::is_some) {
            for format in &key.color_resolves {
                let Some(format) = format else {
                    resolves
                        .push(vk::AttachmentReference::default().attachment(vk::ATTACHMENT_UNUSED));
                    continue;
                };
                resolves.push(
                    vk::AttachmentReference::default()
                        .attachment(attachments.len() as u32)
                        .layout(vk::ImageLayout::GENERAL),
                );
                attachments.push(
                    vk::AttachmentDescription::default()
                        .format(*format)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .load_op(vk::AttachmentLoadOp::DONT_CARE)
                        .store_op(vk::AttachmentStoreOp::STORE)
                        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                        .initial_layout(vk::ImageLayout::GENERAL)
                        .final_layout(vk::ImageLayout::GENERAL),
                );
            }
        }
        let mut subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colors);
        if !resolves.is_empty() {
            subpass = subpass.resolve_attachments(&resolves);
        }
        if let Some(depth) = &depth {
            subpass = subpass.depth_stencil_attachment(depth);
        }
        let subpasses = [subpass];
        let info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses);
        // SAFETY: every borrowed slice lives across the call.
        let pass = unsafe { self.device.create_render_pass(&info, None) }
            .map_err(|error| format!("vkCreateRenderPass failed with {error:?}"))?;
        cache.insert(key.clone(), pass);
        Ok(pass)
    }

    pub fn create_framebuffer(
        &self,
        pass: vk::RenderPass,
        views: &[vk::ImageView],
        width: u32,
        height: u32,
        layers: u32,
    ) -> Result<vk::Framebuffer, String> {
        let info = vk::FramebufferCreateInfo::default()
            .render_pass(pass)
            .attachments(views)
            .width(width.max(1))
            .height(height.max(1))
            .layers(layers.max(1));
        // SAFETY: the views outlive the framebuffer's use in this frame.
        unsafe { self.device.create_framebuffer(&info, None) }
            .map_err(|error| format!("vkCreateFramebuffer failed with {error:?}"))
    }

    /// Submit one recorded command buffer.
    pub fn submit(
        &self,
        command_buffer: vk::CommandBuffer,
        fence: vk::Fence,
        submitted: impl FnOnce(),
    ) -> Result<(), String> {
        let buffers = [command_buffer];
        let submit = vk::SubmitInfo::default().command_buffers(&buffers);
        {
            let queue = self.queue.lock().expect("Vulkan queue");
            // SAFETY: the queue is held exclusively for the submission.
            unsafe { self.device.reset_fences(&[fence]) }
                .map_err(|error| format!("vkResetFences failed with {error:?}"))?;
            // SAFETY: as above; the command buffer has finished recording.
            unsafe { self.device.queue_submit(*queue, &[submit], fence) }
                .map_err(|error| format!("vkQueueSubmit failed with {error:?}"))?;
            // Keep asynchronous retirement in the same order as queue
            // submission even when several guest threads commit concurrently.
            submitted();
        }
        Ok(())
    }

    /// Wait for a submitted command buffer's fence.
    pub fn wait(&self, fence: vk::Fence) -> Result<(), String> {
        crate::stats::timed(crate::stats::Counter::QueueMicros, || {
            // SAFETY: the fence belongs to this device and the submission above.
            unsafe { self.device.wait_for_fences(&[fence], true, u64::MAX) }
                .map_err(|error| format!("vkWaitForFences failed with {error:?}"))
        })
    }

    /// Submit one recorded command buffer and wait for it.
    fn submit_and_wait(
        &self,
        command_buffer: vk::CommandBuffer,
        fence: vk::Fence,
    ) -> Result<(), String> {
        self.submit(command_buffer, fence, || {})?;
        self.wait(fence)
    }

    /// Record and run one command list synchronously.
    ///
    /// Metal performs some work outside any command buffer the guest holds:
    /// putting a fresh texture into a usable layout, and copying bytes a guest
    /// handed to `-replaceRegion:`. Those go through here.
    pub fn one_shot(&self, body: impl FnOnce(&Device, vk::CommandBuffer)) -> Result<(), String> {
        let mut slot = self.immediate.lock().expect("immediate command state");
        if slot.is_none() {
            let pool_info = vk::CommandPoolCreateInfo::default()
                .queue_family_index(self.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            // SAFETY: a well-formed create info on this live device.
            let pool = unsafe { self.device.create_command_pool(&pool_info, None) }
                .map_err(|error| format!("vkCreateCommandPool failed with {error:?}"))?;
            let allocate = vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            // SAFETY: the pool was just created by this device.
            let command_buffer = unsafe { self.device.allocate_command_buffers(&allocate) }
                .map_err(|error| format!("vkAllocateCommandBuffers failed with {error:?}"))?[0];
            // SAFETY: a well-formed create info on this live device.
            let fence = unsafe {
                self.device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
            }
            .map_err(|error| format!("vkCreateFence failed with {error:?}"))?;
            *slot = Some(Immediate {
                pool,
                command_buffer,
                fence,
            });
        }
        let immediate = slot.as_ref().expect("just created");
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the mutex makes this the only user of the pool.
        unsafe {
            self.device
                .reset_command_pool(immediate.pool, vk::CommandPoolResetFlags::empty())
                .map_err(|error| format!("vkResetCommandPool failed with {error:?}"))?;
            self.device
                .begin_command_buffer(immediate.command_buffer, &begin)
                .map_err(|error| format!("vkBeginCommandBuffer failed with {error:?}"))?;
        }
        body(&self.device, immediate.command_buffer);
        // SAFETY: recording began above and has not ended.
        unsafe { self.device.end_command_buffer(immediate.command_buffer) }
            .map_err(|error| format!("vkEndCommandBuffer failed with {error:?}"))?;
        self.submit_and_wait(immediate.command_buffer, immediate.fence)
    }

    /// A pool of two timestamps for one command buffer, or a null handle where
    /// the device cannot time this queue.
    pub fn timestamp_pool(&self) -> vk::QueryPool {
        if self.timestamp_period == 0.0 || self.timestamp_valid_bits == 0 {
            return vk::QueryPool::null();
        }
        let mut pools = self.timestamp_pools.lock().expect("timestamp pools");
        if let Some(pool) = pools.pop() {
            return pool;
        }
        drop(pools);
        let info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(2);
        // SAFETY: a well-formed create info on this live device.
        unsafe { self.device.create_query_pool(&info, None) }.unwrap_or(vk::QueryPool::null())
    }

    /// Hand a pool back once its command buffer's work has completed.
    pub fn release_timestamp_pool(&self, pool: vk::QueryPool) {
        if pool != vk::QueryPool::null() {
            self.timestamp_pools
                .lock()
                .expect("timestamp pools")
                .push(pool);
        }
    }

    pub fn timestamp_period(&self) -> f64 {
        f64::from(self.timestamp_period)
    }

    pub fn timestamp_valid_bits(&self) -> u32 {
        self.timestamp_valid_bits
    }

    /// Destroy an image this device made. The caller must have waited for
    /// every submission which referred to it.
    pub fn destroy_image(&self, image: GpuImage) {
        // SAFETY: the caller guarantees the image is idle.
        unsafe {
            self.device.destroy_image_view(image.view, None);
            self.device.destroy_image(image.image, None);
            if let Some(memory) = image.memory {
                self.device.free_memory(memory, None);
            }
        }
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

fn sample_count(count: u64) -> vk::SampleCountFlags {
    match count {
        0 | 1 => vk::SampleCountFlags::TYPE_1,
        2 => vk::SampleCountFlags::TYPE_2,
        4 => vk::SampleCountFlags::TYPE_4,
        8 => vk::SampleCountFlags::TYPE_8,
        _ => vk::SampleCountFlags::TYPE_16,
    }
}

pub fn aspect_of(format: vk::Format) -> vk::ImageAspectFlags {
    match format {
        vk::Format::D16_UNORM | vk::Format::D32_SFLOAT | vk::Format::X8_D24_UNORM_PACK32 => {
            vk::ImageAspectFlags::DEPTH
        }
        vk::Format::D24_UNORM_S8_UINT
        | vk::Format::D32_SFLOAT_S8_UINT
        | vk::Format::D16_UNORM_S8_UINT => {
            vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL
        }
        vk::Format::S8_UINT => vk::ImageAspectFlags::STENCIL,
        _ => vk::ImageAspectFlags::COLOR,
    }
}

/// `MTLPixelFormat` to `VkFormat`.
///
/// Metal names a texel's components in memory order and Vulkan names them in
/// the same order, so most formats map one to one. A format this table does
/// not know returns `UNDEFINED` and the caller reports it rather than
/// substituting a different layout.
pub fn pixel_format(metal: u64) -> vk::Format {
    use vk::Format as F;
    match metal {
        1 => F::R8_UNORM, // A8Unorm: one 8-bit channel; swizzle is the view's
        10 => F::R8_UNORM,
        11 => F::R8_SRGB,
        12 => F::R8_SNORM,
        13 => F::R8_UINT,
        14 => F::R8_SINT,
        20 => F::R16_UNORM,
        22 => F::R16_SNORM,
        23 => F::R16_UINT,
        24 => F::R16_SINT,
        25 => F::R16_SFLOAT,
        30 => F::R8G8_UNORM,
        31 => F::R8G8_SRGB,
        32 => F::R8G8_SNORM,
        33 => F::R8G8_UINT,
        34 => F::R8G8_SINT,
        40 => F::R5G6B5_UNORM_PACK16,
        41 => F::A1R5G5B5_UNORM_PACK16,
        42 => F::R4G4B4A4_UNORM_PACK16,
        43 => F::B5G5R5A1_UNORM_PACK16,
        53 => F::R32_UINT,
        54 => F::R32_SINT,
        55 => F::R32_SFLOAT,
        60 => F::R16G16_UNORM,
        62 => F::R16G16_SNORM,
        63 => F::R16G16_UINT,
        64 => F::R16G16_SINT,
        65 => F::R16G16_SFLOAT,
        70 => F::R8G8B8A8_UNORM,
        71 => F::R8G8B8A8_SRGB,
        72 => F::R8G8B8A8_SNORM,
        73 => F::R8G8B8A8_UINT,
        74 => F::R8G8B8A8_SINT,
        80 => F::B8G8R8A8_UNORM,
        81 => F::B8G8R8A8_SRGB,
        90 => F::A2B10G10R10_UNORM_PACK32,
        91 => F::A2B10G10R10_UINT_PACK32,
        92 => F::B10G11R11_UFLOAT_PACK32,
        93 => F::E5B9G9R9_UFLOAT_PACK32,
        94 => F::A2R10G10B10_UNORM_PACK32,
        103 => F::R32G32_UINT,
        104 => F::R32G32_SINT,
        105 => F::R32G32_SFLOAT,
        110 => F::R16G16B16A16_UNORM,
        112 => F::R16G16B16A16_SNORM,
        113 => F::R16G16B16A16_UINT,
        114 => F::R16G16B16A16_SINT,
        115 => F::R16G16B16A16_SFLOAT,
        123 => F::R32G32B32A32_UINT,
        124 => F::R32G32B32A32_SINT,
        125 => F::R32G32B32A32_SFLOAT,
        130 => F::BC1_RGBA_UNORM_BLOCK,
        131 => F::BC1_RGBA_SRGB_BLOCK,
        132 => F::BC2_UNORM_BLOCK,
        133 => F::BC2_SRGB_BLOCK,
        134 => F::BC3_UNORM_BLOCK,
        135 => F::BC3_SRGB_BLOCK,
        140 => F::BC4_UNORM_BLOCK,
        141 => F::BC4_SNORM_BLOCK,
        142 => F::BC5_UNORM_BLOCK,
        143 => F::BC5_SNORM_BLOCK,
        150 => F::BC6H_SFLOAT_BLOCK,
        151 => F::BC6H_UFLOAT_BLOCK,
        152 => F::BC7_UNORM_BLOCK,
        153 => F::BC7_SRGB_BLOCK,
        250 => F::D16_UNORM,
        252 => F::D32_SFLOAT,
        253 => F::S8_UINT,
        // Metal's Depth24Unorm_Stencil8 has no exact portable Vulkan format on
        // every driver; D24_UNORM_S8_UINT is the same layout where present.
        255 => F::D24_UNORM_S8_UINT,
        260 => F::D32_SFLOAT_S8_UINT,
        // `X32_Stencil8` and `X24_Stencil8` are the stencil half of a combined
        // texture seen on its own. The image carries both halves either way;
        // which one a view reads is the aspect, not the format.
        261 => F::D32_SFLOAT_S8_UINT,
        262 => F::D24_UNORM_S8_UINT,
        _ => F::UNDEFINED,
    }
}

/// The number of bytes one texel of `format` occupies, for buffer/texture
/// copies. Block-compressed formats report the block's size.
pub fn texel_size(format: vk::Format) -> u32 {
    use vk::Format as F;
    match format {
        F::R8_UNORM | F::R8_SRGB | F::R8_SNORM | F::R8_UINT | F::R8_SINT | F::S8_UINT => 1,
        F::R8G8_UNORM
        | F::R8G8_SRGB
        | F::R8G8_SNORM
        | F::R8G8_UINT
        | F::R8G8_SINT
        | F::R16_UNORM
        | F::R16_SNORM
        | F::R16_UINT
        | F::R16_SINT
        | F::R16_SFLOAT
        | F::R5G6B5_UNORM_PACK16
        | F::A1R5G5B5_UNORM_PACK16
        | F::R4G4B4A4_UNORM_PACK16
        | F::B5G5R5A1_UNORM_PACK16
        | F::D16_UNORM => 2,
        F::R8G8B8A8_UNORM
        | F::R8G8B8A8_SRGB
        | F::R8G8B8A8_SNORM
        | F::R8G8B8A8_UINT
        | F::R8G8B8A8_SINT
        | F::B8G8R8A8_UNORM
        | F::B8G8R8A8_SRGB
        | F::A2B10G10R10_UNORM_PACK32
        | F::A2B10G10R10_UINT_PACK32
        | F::A2R10G10B10_UNORM_PACK32
        | F::B10G11R11_UFLOAT_PACK32
        | F::E5B9G9R9_UFLOAT_PACK32
        | F::R32_UINT
        | F::R32_SINT
        | F::R32_SFLOAT
        | F::R16G16_UNORM
        | F::R16G16_SNORM
        | F::R16G16_UINT
        | F::R16G16_SINT
        | F::R16G16_SFLOAT
        | F::D32_SFLOAT
        | F::D24_UNORM_S8_UINT => 4,
        F::D32_SFLOAT_S8_UINT => 5,
        F::R32G32_UINT
        | F::R32G32_SINT
        | F::R32G32_SFLOAT
        | F::R16G16B16A16_UNORM
        | F::R16G16B16A16_SNORM
        | F::R16G16B16A16_UINT
        | F::R16G16B16A16_SINT
        | F::R16G16B16A16_SFLOAT
        | F::BC1_RGBA_UNORM_BLOCK
        | F::BC1_RGBA_SRGB_BLOCK
        | F::BC4_UNORM_BLOCK
        | F::BC4_SNORM_BLOCK => 8,
        F::R32G32B32A32_UINT
        | F::R32G32B32A32_SINT
        | F::R32G32B32A32_SFLOAT
        | F::BC2_UNORM_BLOCK
        | F::BC2_SRGB_BLOCK
        | F::BC3_UNORM_BLOCK
        | F::BC3_SRGB_BLOCK
        | F::BC5_UNORM_BLOCK
        | F::BC5_SNORM_BLOCK
        | F::BC6H_SFLOAT_BLOCK
        | F::BC6H_UFLOAT_BLOCK
        | F::BC7_UNORM_BLOCK
        | F::BC7_SRGB_BLOCK => 16,
        _ => 4,
    }
}

/// Whether `format` is block compressed, and its block extent.
pub fn block_extent(format: vk::Format) -> (u32, u32) {
    use vk::Format as F;
    match format {
        F::BC1_RGBA_UNORM_BLOCK
        | F::BC1_RGBA_SRGB_BLOCK
        | F::BC2_UNORM_BLOCK
        | F::BC2_SRGB_BLOCK
        | F::BC3_UNORM_BLOCK
        | F::BC3_SRGB_BLOCK
        | F::BC4_UNORM_BLOCK
        | F::BC4_SNORM_BLOCK
        | F::BC5_UNORM_BLOCK
        | F::BC5_SNORM_BLOCK
        | F::BC6H_SFLOAT_BLOCK
        | F::BC6H_UFLOAT_BLOCK
        | F::BC7_UNORM_BLOCK
        | F::BC7_SRGB_BLOCK => (4, 4),
        _ => (1, 1),
    }
}

/// The process's Vulkan tracing switch, shared with `vulkan.rs`.
pub fn tracing() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ROSETTA3_TRACE_METAL").is_some())
}

/// Write one module to `ROSETTA3_SPIRV_OUT`, so that a driver which rejects or
/// crashes on a shader can be met with the exact bytes it was given.
///
/// A driver's SPIR-V front end is not a validator, and a module `spirv-val`
/// accepts can still take one down; the file is the only way to tell which of
/// the thousands a game compiles was the one.
fn dump_spirv(entry: &str, words: &[u32]) {
    let Some(directory) = std::env::var_os("ROSETTA3_SPIRV_OUT") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    // An entry point name is a Metal identifier, but nothing guarantees that of
    // a name a translated library carries, and this becomes a path.
    let safe: String = entry
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() || c == '_' {
            true => c,
            false => '_',
        })
        .collect();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    let mut path = directory.join(&safe);
    // Two libraries may carry the same entry point name with different bodies.
    let mut ordinal = 0;
    while path.exists() {
        ordinal += 1;
        path = directory.join(format!("{safe}.{ordinal}"));
    }
    path.set_extension("spv");
    let _ = std::fs::write(path, bytes);
}

pub fn trace(message: impl std::fmt::Display) {
    if tracing() {
        eprintln!("rosetta3: Metal: {message}");
    }
}

/// Report a message once however often the runtime reaches it, so that a gap a
/// frame runs into does not fill the log.
pub fn trace_once(message: impl std::fmt::Display) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if !tracing() {
        return;
    }
    let message = message.to_string();
    let mut seen = SEEN.lock().expect("trace list");
    if !seen.contains(&message) {
        eprintln!("rosetta3: Metal: {message}");
        seen.push(message);
    }
}

/// Report one milestone the first time the runtime reaches it.
///
/// A runtime which never draws and a runtime which draws nothing visible look
/// the same in a game's own log, so each first dispatch, draw and present says
/// so once.
pub fn milestone(what: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    if !tracing() {
        return;
    }
    let mut seen = SEEN.lock().expect("milestone list");
    if !seen.contains(&what) {
        seen.push(what);
        eprintln!("rosetta3: Metal: first {what}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metal_pixel_formats_map_to_their_vulkan_layout() {
        assert_eq!(pixel_format(80), vk::Format::B8G8R8A8_UNORM);
        assert_eq!(pixel_format(70), vk::Format::R8G8B8A8_UNORM);
        assert_eq!(pixel_format(252), vk::Format::D32_SFLOAT);
        // The single-channel 32-bit trio is numbered like the two- and
        // four-channel ones, three above a multiple of ten and not below it.
        assert_eq!(pixel_format(53), vk::Format::R32_UINT);
        assert_eq!(pixel_format(54), vk::Format::R32_SINT);
        assert_eq!(pixel_format(55), vk::Format::R32_SFLOAT);
        assert_eq!(pixel_format(50), vk::Format::UNDEFINED);
        assert_eq!(pixel_format(260), vk::Format::D32_SFLOAT_S8_UINT);
        assert_eq!(pixel_format(9999), vk::Format::UNDEFINED);
    }

    #[test]
    fn depth_formats_carry_their_aspect() {
        assert_eq!(
            aspect_of(vk::Format::D32_SFLOAT),
            vk::ImageAspectFlags::DEPTH
        );
        assert_eq!(
            aspect_of(vk::Format::D24_UNORM_S8_UINT),
            vk::ImageAspectFlags::DEPTH | vk::ImageAspectFlags::STENCIL
        );
        assert_eq!(
            aspect_of(vk::Format::B8G8R8A8_UNORM),
            vk::ImageAspectFlags::COLOR
        );
    }

    #[test]
    fn compressed_formats_report_their_block() {
        assert_eq!(block_extent(vk::Format::BC3_UNORM_BLOCK), (4, 4));
        assert_eq!(block_extent(vk::Format::R8G8B8A8_UNORM), (1, 1));
        assert_eq!(texel_size(vk::Format::BC1_RGBA_UNORM_BLOCK), 8);
    }
}
