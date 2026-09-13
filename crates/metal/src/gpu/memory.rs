use super::*;

impl Gpu {
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
        // Nothing is bound to the buffer until the guard is taken, so every
        // failure below destroys it on the way out.
        let buffer = Guard::new(buffer, |buffer| {
            // SAFETY: a buffer of this device's which was never bound.
            unsafe { self.device.destroy_buffer(buffer, None) };
        });
        // SAFETY: the buffer was just created by this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(*buffer) };
        let heap = self.buffer_heap(requirements.memory_type_bits)?;
        let allocation = heap.allocate(&self.device, requirements.size, requirements.alignment)?;
        let allocation = Guard::new(allocation, |allocation| {
            heap.free(&self.device, allocation);
        });
        // SAFETY: a fresh, unbound buffer and a range nothing else holds.
        unsafe {
            self.device
                .bind_buffer_memory(*buffer, allocation.memory, allocation.offset())
        }
        .map_err(|error| format!("vkBindBufferMemory failed with {error:?}"))?;
        let (buffer, allocation) = (buffer.take(), allocation.take());
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
        let samples = sample_count(description.samples).ok_or_else(|| {
            format!(
                "texture has invalid Metal sample count {}",
                description.samples
            )
        })?;
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
            .samples(samples)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: a well-formed create info on this live device.
        let image = unsafe { self.device.create_image(&info, None) }
            .map_err(|error| format!("vkCreateImage failed with {error:?}"))?;
        // Everything below can fail; the guards unwind what has been made so
        // far, and only the last statement of the function takes them.
        let image = Guard::new(image, |image| {
            // SAFETY: an image of this device's, referred to by nothing.
            unsafe { self.device.destroy_image(image, None) };
        });
        // SAFETY: the image was just created by this device.
        let requirements = unsafe { self.device.get_image_memory_requirements(*image) };
        let memory = self.allocate(
            requirements,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::empty(),
        )?;
        let memory = Guard::new(memory, |memory| {
            // SAFETY: memory of this device's, bound to nothing which survives.
            unsafe { self.device.free_memory(memory, None) };
        });
        // SAFETY: fresh, unbound image and memory from this device.
        unsafe { self.device.bind_image_memory(*image, *memory, 0) }
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
                .image(*image)
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
        let view = self.create_view(*image, description)?;
        Ok(GpuImage {
            image: image.take(),
            memory: Some(memory.take()),
            view,
            format: description.format,
            aspect: aspect_of(description.format),
            width: description.width.max(1),
            height: description.height.max(1),
            depth: description.depth.max(1),
            levels: description.levels.max(1),
            layers: description.layers.max(1),
            samples,
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
        push: bool,
    ) -> Result<vk::DescriptorSetLayout, String> {
        let mut key = bindings.to_vec();
        key.sort_unstable();
        key.dedup();
        let mut cache = self.layouts.lock().expect("descriptor layout cache");
        let key = (key, stages, push);
        if let Some(layout) = cache.get(&key) {
            return Ok(*layout);
        }
        let entries = key
            .0
            .iter()
            .map(|(binding, kind)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(*binding)
                    .descriptor_type(*kind)
                    .descriptor_count(1)
                    .stage_flags(stages)
            })
            .collect::<Vec<_>>();
        let flags = if push {
            vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR
        } else {
            vk::DescriptorSetLayoutCreateFlags::empty()
        };
        let info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(flags)
            .bindings(&entries);
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
}
