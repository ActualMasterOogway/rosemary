//! The encoder itself: descriptor binding, pipeline binding, barriers.

use super::*;

pub(crate) fn new_encoder(command_buffer: Id, kind: Kind) -> Id {
    stats::count(Counter::Encoders);
    // An encoder is vended autoreleased, as Metal vends one, and reads its
    // command buffer for as long as it lives - so it holds a reference to it
    // rather than trusting the guest to outlive the encoder it made.
    objc::arc::autorelease(object::make(
        c"R3MTLCommandEncoder",
        Encoder {
            command_buffer: objc::arc::retain(command_buffer),
            kind,
            compute_pipeline: std::ptr::null_mut(),
            render_pipeline: std::ptr::null_mut(),
            depth_state: std::ptr::null_mut(),
            buffers: [[BufferBinding::default(); SLOTS]; STAGES],
            textures: [[std::ptr::null_mut(); SLOTS]; STAGES],
            samplers: [[std::ptr::null_mut(); SLOTS]; STAGES],
            stencil_reference: 0,
            cull_mode: 0,
            winding: 0,
            fill_mode: 0,
            depth_clip: 0,
            in_render_pass: false,
            resolve_mask: 0,
            pending_copies: Vec::new(),
            capture_targets: Vec::new(),
            capture_pass: None,
            render_variant: None,
            compute_variant: None,
            bound: Default::default(),
            set_cache: Default::default(),
            wanted: Default::default(),
            vertex_buffers: [(vk::Buffer::null(), 0); SLOTS],
            index_buffer: (vk::Buffer::null(), 0, vk::IndexType::UINT16),
        },
    ))
}

/// `-[MTLCommandEncoder dealloc]`.
///
/// An encoder binds nothing of its own: the command buffer retains every
/// resource bound through it, because that is the object whose recording still
/// refers to them. What is left is the reference to that command buffer.
pub(super) unsafe extern "C" fn encoder_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    objc::arc::release(std::mem::replace(
        &mut encoder.command_buffer,
        std::ptr::null_mut(),
    ));
    // SAFETY: the body was established by `object::make` and is not read again.
    unsafe { std::ptr::drop_in_place(encoder as *mut Encoder) };
    objc::arc::finalize(receiver);
}

pub(super) fn stage_slot(stage: Stage) -> usize {
    match stage {
        Stage::Vertex => 1,
        Stage::Fragment => 2,
        _ => 0,
    }
}

pub(super) fn bind_point_slot(bind_point: vk::PipelineBindPoint) -> usize {
    usize::from(bind_point == vk::PipelineBindPoint::GRAPHICS)
}

pub(super) unsafe extern "C" fn end_encoding(receiver: Id, _sel: Sel) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    if encoder.in_render_pass {
        let command = encoder.command();
        // SAFETY: a render pass began when the encoder was made.
        unsafe {
            command
                .gpu
                .device
                .cmd_end_render_pass(command.command_buffer)
        };
        encoder.in_render_pass = false;
        for copy in std::mem::take(&mut encoder.pending_copies) {
            let source = vk::ImageSubresourceLayers {
                aspect_mask: copy.aspect,
                mip_level: copy.source_level,
                base_array_layer: copy.source_slice,
                layer_count: 1,
            };
            let destination = vk::ImageSubresourceLayers {
                aspect_mask: copy.aspect,
                mip_level: copy.destination_level,
                base_array_layer: copy.destination_slice,
                layer_count: 1,
            };
            let extent = vk::Extent3D {
                width: copy.width,
                height: copy.height,
                depth: 1,
            };
            let region = vk::ImageCopy::default()
                .src_subresource(source)
                .dst_subresource(destination)
                .extent(extent);
            // SAFETY: recording into this command buffer, outside the pass.
            unsafe {
                command.gpu.device.cmd_copy_image(
                    command.command_buffer,
                    copy.source,
                    vk::ImageLayout::GENERAL,
                    copy.destination,
                    vk::ImageLayout::GENERAL,
                    &[region],
                )
            };
        }
        let targets = std::mem::take(&mut encoder.capture_targets);
        if let Some(pass) = encoder.capture_pass.take() {
            for target in &targets {
                if let Some(capture) = crate::capture::record(
                    command.gpu,
                    command.command_buffer,
                    pass,
                    "post",
                    target,
                ) {
                    command.captures.push(capture);
                }
            }
        }
    }
    encoder.barrier();
}

/// Report a resource the pipeline declares which nothing satisfies.
///
/// The draw is still recorded: see [`Encoder::collect_writes`].
fn unbound(binding: &metal_translate::ResourceBinding, what: &str, represented: bool) {
    let result = if represented {
        "it is represented by a null descriptor"
    } else {
        "its descriptor keeps whatever the set last held"
    };
    gpu::trace_once(format!("a shader resource {what}; {result}"));
    gpu::trace(format!(
        "{:?} at Metal index {} of set {} {what}",
        binding.kind, binding.metal_index, binding.descriptor_set
    ));
}

impl Encoder {
    pub(super) fn command<'a>(&self) -> &'a mut CommandBuffer {
        // SAFETY: the encoder holds the command buffer which made it.
        unsafe { object::body::<CommandBuffer>(self.command_buffer) }
    }

    /// Write the descriptor sets one pipeline's resources need and bind them.
    ///
    /// `false` only where no descriptor set could be allocated at all, which
    /// is the caller's cue to record nothing: there is no set to bind, so the
    /// draw would consume whichever one the bind point last held.
    #[must_use]
    pub(super) fn bind_resources(
        &mut self,
        resources: &[metal_translate::ResourceBinding],
        layouts: &[vk::DescriptorSetLayout; SETS],
        pipeline_layout: vk::PipelineLayout,
        push_set: Option<usize>,
        bind_point: vk::PipelineBindPoint,
    ) -> bool {
        let start = stats::enabled().then(std::time::Instant::now);
        let bound =
            self.bind_resources_inner(resources, layouts, pipeline_layout, push_set, bind_point);
        if let Some(start) = start {
            stats::add(Counter::BindMicros, start.elapsed().as_micros() as u64);
        }
        bound
    }

    #[must_use]
    pub(super) fn bind_resources_inner(
        &mut self,
        resources: &[metal_translate::ResourceBinding],
        layouts: &[vk::DescriptorSetLayout; SETS],
        pipeline_layout: vk::PipelineLayout,
        push_set: Option<usize>,
        bind_point: vk::PipelineBindPoint,
    ) -> bool {
        let used = self.collect_writes(resources);
        let push_compatible = self.bound[bind_point_slot(bind_point)].layouts == *layouts;
        let Some(sets) = self.materialize_sets(layouts, &used, push_set) else {
            gpu::trace_once(
                "a descriptor set could not be allocated, so the draw or dispatch which needed \
                 it is not recorded",
            );
            return false;
        };
        self.bind_sets(&used, &sets, layouts, pipeline_layout, bind_point);
        if let Some(index) = push_set.filter(|index| used[*index]) {
            self.push_set(
                index,
                layouts[index],
                pipeline_layout,
                bind_point,
                push_compatible,
            );
        }
        true
    }

    /// Gather each binding into the descriptor writes its set wants, and
    /// report which sets the pipeline actually uses.
    ///
    /// With robustness2, a resource the pipeline declares but the guest never
    /// bound becomes an explicit null descriptor. Vulkan does not admit null
    /// standalone samplers, so those use the device's neutral sampler; the
    /// paired null image still supplies the defined zero result. Without the
    /// feature, retain the old skip fallback until reflection carries enough
    /// descriptor shape to create matching placeholder images.
    fn collect_writes(&mut self, resources: &[metal_translate::ResourceBinding]) -> [bool; SETS] {
        let (null_descriptors, null_sampler) = {
            let gpu = self.command().gpu;
            (gpu.null_descriptors, gpu.null_sampler)
        };
        let mut used = [false; SETS];
        for wanted in &mut self.wanted {
            wanted.clear();
        }
        for binding in resources {
            let set = binding.descriptor_set as usize;
            if set >= SETS {
                // Rejected by `layouts_for`, so not reachable through a
                // pipeline which exists.
                continue;
            }
            used[set] = true;
            let slot = stage_slot(binding.stage);
            let index = binding.metal_index as usize;
            let Some(kind) = descriptor_type(binding) else {
                continue;
            };
            let (handle, offset, range) = match binding.kind {
                ResourceKind::Buffer => {
                    let bound = self.buffers[slot].get(index).copied().unwrap_or_default();
                    if bound.buffer == vk::Buffer::null() {
                        let what = if index < SLOTS {
                            "has no buffer bound to it"
                        } else {
                            "is past the Metal buffer index this encoder tracks"
                        };
                        unbound(binding, what, null_descriptors);
                        if !null_descriptors {
                            continue;
                        }
                    }
                    use vk::Handle;
                    (
                        bound.buffer.as_raw(),
                        if bound.buffer == vk::Buffer::null() {
                            0
                        } else {
                            bound.offset.min(bound.end)
                        },
                        if bound.buffer == vk::Buffer::null() {
                            vk::WHOLE_SIZE
                        } else {
                            bound.range
                        },
                    )
                }
                ResourceKind::Texture => {
                    let bound = self.textures[slot]
                        .get(index)
                        .copied()
                        .unwrap_or(std::ptr::null_mut());
                    if bound.is_null() {
                        let what = if index < SLOTS {
                            "has no texture bound to it"
                        } else {
                            "is past the Metal texture index this encoder tracks"
                        };
                        unbound(binding, what, null_descriptors);
                        if !null_descriptors {
                            continue;
                        }
                        (0, 0, 0)
                    } else {
                        // SAFETY: only a texture of this crate's is ever bound.
                        let texture = unsafe { object::body::<Texture>(bound) };
                        use vk::Handle;
                        (texture.image.view.as_raw(), 0, 0)
                    }
                }
                ResourceKind::Sampler => {
                    let bound = self.samplers[slot]
                        .get(index)
                        .copied()
                        .unwrap_or(std::ptr::null_mut());
                    if bound.is_null() {
                        let what = if index < SLOTS {
                            "has no sampler bound to it"
                        } else {
                            "is past the Metal sampler index this encoder tracks"
                        };
                        unbound(binding, what, null_descriptors);
                        if !null_descriptors {
                            continue;
                        }
                        use vk::Handle;
                        (null_sampler.as_raw(), 0, 0)
                    } else {
                        // SAFETY: only a sampler state of this crate's is bound.
                        let sampler = unsafe { object::body::<SamplerState>(bound) };
                        use vk::Handle;
                        (sampler.sampler.as_raw(), 0, 0)
                    }
                }
                _ => continue,
            };
            self.wanted[set].push(DescriptorEntry {
                binding: binding.binding,
                kind,
                handle,
                offset,
                range,
            });
        }
        used
    }

    /// Allocate or reuse a descriptor set per used set index, writing it only
    /// when its contents differ from the one this encoder last wrote.
    fn materialize_sets(
        &mut self,
        layouts: &[vk::DescriptorSetLayout; SETS],
        used: &[bool; SETS],
        push_set: Option<usize>,
    ) -> Option<[vk::DescriptorSet; SETS]> {
        let command = self.command();
        let gpu = command.gpu;
        // A set whose layout and contents match the one this encoder last
        // wrote is still valid: nothing writes a set once a draw has used it.
        let mut sets = [vk::DescriptorSet::null(); SETS];
        let mut fresh = [false; SETS];
        for index in 0..SETS {
            if !used[index] || push_set == Some(index) {
                continue;
            }
            let cache = &mut self.set_cache[index];
            if cache.set != vk::DescriptorSet::null()
                && cache.layout == layouts[index]
                && cache.entries == self.wanted[index]
            {
                sets[index] = cache.set;
                continue;
            }
            let set = command.allocate_set(layouts[index])?;
            cache.layout = layouts[index];
            cache.set = set;
            std::mem::swap(&mut cache.entries, &mut self.wanted[index]);
            sets[index] = set;
            fresh[index] = true;
        }

        if fresh.iter().any(|fresh| *fresh) {
            let mut buffer_infos = Vec::new();
            let mut image_infos = Vec::new();
            for index in (0..SETS).filter(|index| fresh[*index]) {
                for entry in &self.set_cache[index].entries {
                    use vk::Handle;
                    match entry.kind {
                        vk::DescriptorType::STORAGE_BUFFER => buffer_infos.push(
                            vk::DescriptorBufferInfo::default()
                                .buffer(vk::Buffer::from_raw(entry.handle))
                                .offset(entry.offset)
                                .range(entry.range),
                        ),
                        vk::DescriptorType::SAMPLER => image_infos.push(
                            vk::DescriptorImageInfo::default()
                                .sampler(vk::Sampler::from_raw(entry.handle)),
                        ),
                        _ => image_infos.push(
                            vk::DescriptorImageInfo::default()
                                .image_view(vk::ImageView::from_raw(entry.handle))
                                .image_layout(vk::ImageLayout::GENERAL),
                        ),
                    }
                }
            }
            let (mut next_buffer, mut next_image) = (0, 0);
            let mut writes = Vec::with_capacity(buffer_infos.len() + image_infos.len());
            for index in (0..SETS).filter(|index| fresh[*index]) {
                for entry in &self.set_cache[index].entries {
                    let write = vk::WriteDescriptorSet::default()
                        .dst_set(sets[index])
                        .dst_binding(entry.binding)
                        .descriptor_type(entry.kind);
                    writes.push(match entry.kind {
                        vk::DescriptorType::STORAGE_BUFFER => {
                            next_buffer += 1;
                            write.buffer_info(std::slice::from_ref(&buffer_infos[next_buffer - 1]))
                        }
                        _ => {
                            next_image += 1;
                            write.image_info(std::slice::from_ref(&image_infos[next_image - 1]))
                        }
                    });
                }
            }
            if !writes.is_empty() {
                // SAFETY: every referenced info lives across the call, and the
                // sets are fresh, so no recorded command refers to them yet.
                unsafe { gpu.device.update_descriptor_sets(&writes, &[]) };
            }
        }
        Some(sets)
    }

    /// Record one set directly into the command buffer. Its contents belong
    /// to the bind point and remain live until an incompatible layout disturbs
    /// them, so an unchanged draw can skip the command just like a bound set.
    fn push_set(
        &mut self,
        index: usize,
        layout: vk::DescriptorSetLayout,
        pipeline_layout: vk::PipelineLayout,
        bind_point: vk::PipelineBindPoint,
        compatible: bool,
    ) {
        let cache = &self.set_cache[index];
        if compatible && cache.layout == layout && cache.entries == self.wanted[index] {
            return;
        }

        let entries = &self.wanted[index];
        let mut buffer_infos = Vec::new();
        let mut image_infos = Vec::new();
        for entry in entries {
            use vk::Handle;
            match entry.kind {
                vk::DescriptorType::STORAGE_BUFFER => buffer_infos.push(
                    vk::DescriptorBufferInfo::default()
                        .buffer(vk::Buffer::from_raw(entry.handle))
                        .offset(entry.offset)
                        .range(entry.range),
                ),
                vk::DescriptorType::SAMPLER => image_infos.push(
                    vk::DescriptorImageInfo::default().sampler(vk::Sampler::from_raw(entry.handle)),
                ),
                _ => image_infos.push(
                    vk::DescriptorImageInfo::default()
                        .image_view(vk::ImageView::from_raw(entry.handle))
                        .image_layout(vk::ImageLayout::GENERAL),
                ),
            }
        }
        let (mut next_buffer, mut next_image) = (0, 0);
        let mut writes = Vec::with_capacity(entries.len());
        for entry in entries {
            let write = vk::WriteDescriptorSet::default()
                .dst_binding(entry.binding)
                .descriptor_type(entry.kind);
            writes.push(match entry.kind {
                vk::DescriptorType::STORAGE_BUFFER => {
                    next_buffer += 1;
                    write.buffer_info(std::slice::from_ref(&buffer_infos[next_buffer - 1]))
                }
                _ => {
                    next_image += 1;
                    write.image_info(std::slice::from_ref(&image_infos[next_image - 1]))
                }
            });
        }
        let command = self.command();
        let api = command
            .gpu
            .push_descriptor_api()
            .expect("a push layout requires VK_KHR_push_descriptor");
        // SAFETY: every referenced info lives across the call, the selected
        // layout is the pipeline's unique push layout, and recording is live.
        unsafe {
            api.cmd_push_descriptor_set(
                command.command_buffer,
                bind_point,
                pipeline_layout,
                index as u32,
                &writes,
            )
        };
        stats::add(Counter::PushDescriptorWrites, entries.len() as u64);
        let cache = &mut self.set_cache[index];
        cache.layout = layout;
        cache.set = vk::DescriptorSet::null();
        std::mem::swap(&mut cache.entries, &mut self.wanted[index]);
    }

    /// Bind the sets the pipeline uses, skipping any already bound under a
    /// compatible layout.
    fn bind_sets(
        &mut self,
        used: &[bool; SETS],
        sets: &[vk::DescriptorSet; SETS],
        layouts: &[vk::DescriptorSetLayout; SETS],
        pipeline_layout: vk::PipelineLayout,
        bind_point: vk::PipelineBindPoint,
    ) {
        let command = self.command();
        let gpu = command.gpu;
        // Pipeline layouts built from the same set layouts are compatible, so
        // a set already bound under an identical shape stays bound.
        let bound = &mut self.bound[bind_point_slot(bind_point)];
        let compatible = bound.layouts == *layouts;
        if !compatible {
            bound.layouts = *layouts;
            bound.sets = [vk::DescriptorSet::null(); SETS];
        }
        for index in (0..SETS).filter(|index| used[*index]) {
            if bound.sets[index] == sets[index] {
                continue;
            }
            bound.sets[index] = sets[index];
            // SAFETY: recording into this command buffer.
            unsafe {
                gpu.device.cmd_bind_descriptor_sets(
                    command.command_buffer,
                    bind_point,
                    pipeline_layout,
                    index as u32,
                    &[sets[index]],
                    &[],
                )
            };
        }
    }

    /// Bind `pipeline` unless it is already what this bind point has bound.
    pub(super) fn bind_pipeline(
        &mut self,
        bind_point: vk::PipelineBindPoint,
        pipeline: vk::Pipeline,
    ) {
        let bound = &mut self.bound[bind_point_slot(bind_point)];
        if bound.pipeline == pipeline {
            return;
        }
        bound.pipeline = pipeline;
        let command = self.command();
        // SAFETY: recording into this command buffer.
        unsafe {
            command
                .gpu
                .device
                .cmd_bind_pipeline(command.command_buffer, bind_point, pipeline)
        };
    }

    /// Metal orders the work of one encoder against the next; nothing here
    /// tracks which resource a later encoder reads, so one full barrier
    /// between encoders is what keeps the order Metal promises.
    ///
    /// It is not what makes a frame slow: removing it entirely takes the Roblox
    /// home page from 6.65 ms of device time a frame to 6.45 ms.
    pub(super) fn barrier(&self) {
        let command = self.command();
        let memory = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
        // SAFETY: recording into this command buffer.
        unsafe {
            command.gpu.device.cmd_pipeline_barrier(
                command.command_buffer,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[memory],
                &[],
                &[],
            )
        };
    }

    /// Bind the pipeline variant and the resources one draw needs.
    pub(super) fn prepare_draw(
        &mut self,
        primitive: u64,
    ) -> Option<(&'static ash::Device, vk::CommandBuffer)> {
        if self.kind != Kind::Render || self.render_pipeline.is_null() || !self.in_render_pass {
            return None;
        }
        // SAFETY: only a render pipeline state of this crate's is ever bound.
        let state = unsafe { object::body::<RenderPipeline>(self.render_pipeline) };
        let depth = if self.depth_state.is_null() {
            DepthStencilState::default()
        } else {
            // SAFETY: only a depth/stencil state of this crate's is bound.
            *unsafe { object::body::<DepthStencilState>(self.depth_state) }
        };
        let key = RenderVariantKey {
            topology: primitive,
            cull_mode: self.cull_mode,
            winding: self.winding,
            fill_mode: self.fill_mode,
            depth_clip: self.depth_clip,
            depth: DepthStencilKey::from(&depth),
            resolve_mask: self.resolve_mask,
        };
        let pipeline = match self.render_variant {
            Some((object, cached, pipeline)) if object == self.render_pipeline && cached == key => {
                pipeline
            }
            _ => match state.variant(&key) {
                Ok(pipeline) => {
                    self.render_variant = Some((self.render_pipeline, key, pipeline));
                    pipeline
                }
                Err(reason) => {
                    gpu::trace(format!("render pipeline creation failed: {reason}"));
                    return None;
                }
            },
        };
        self.bind_pipeline(vk::PipelineBindPoint::GRAPHICS, pipeline);
        let command = self.command();
        let device = &command.gpu.device;
        let vk_command = command.command_buffer;

        // Vertex streams are the buffer indices the vertex descriptor claims;
        // every other bound buffer is a descriptor resource. The buffers were
        // retained when they were bound.
        for stream in &state.vertex_streams {
            let bound = self.buffers[VERTEX][*stream as usize % SLOTS];
            if bound.buffer == vk::Buffer::null() {
                continue;
            }
            let binding = (bound.buffer, bound.offset.min(bound.end));
            let Some(current) = self.vertex_buffers.get_mut(*stream as usize) else {
                continue;
            };
            if *current == binding {
                continue;
            }
            *current = binding;
            // SAFETY: recording into this command buffer.
            unsafe {
                device.cmd_bind_vertex_buffers(vk_command, *stream, &[binding.0], &[binding.1])
            };
        }
        if !self.bind_resources(
            &state.resources,
            &state.layouts.sets,
            state.layouts.pipeline,
            state.layouts.push_set,
            vk::PipelineBindPoint::GRAPHICS,
        ) {
            return None;
        }
        gpu::milestone("draw");
        stats::count(Counter::Draws);
        Some((device, vk_command))
    }
}
