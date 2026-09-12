//! `MTLRenderPipelineState` and the variants a render pass asks it for.

use super::*;

/// The state Metal keeps on the encoder but Vulkan bakes into a pipeline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RenderVariantKey {
    pub topology: u64,
    pub cull_mode: u64,
    pub winding: u64,
    pub fill_mode: u64,
    pub depth_clip: u64,
    pub depth: DepthStencilKey,
    /// Which of the pass's colour attachments are being resolved. A Metal
    /// pipeline state says nothing about resolving - the pass descriptor does -
    /// but Vulkan render passes are compatible only when their resolve
    /// attachment references match, so the pipeline needs one variant per
    /// resolve shape it is drawn under.
    pub resolve_mask: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DepthStencilKey {
    pub compare: u64,
    pub write: bool,
    pub stencil: bool,
    pub front: [u64; 6],
    pub back: [u64; 6],
}

/// One `MTLRenderPipelineColorAttachmentDescriptor`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ColorAttachment {
    pub pixel_format: u64,
    pub blending: bool,
    pub source_rgb: u64,
    pub destination_rgb: u64,
    pub rgb_operation: u64,
    pub source_alpha: u64,
    pub destination_alpha: u64,
    pub alpha_operation: u64,
    pub write_mask: u64,
}

impl ColorAttachment {
    pub fn fresh() -> Self {
        Self {
            pixel_format: 0,
            blending: false,
            source_rgb: 1,      // One
            destination_rgb: 0, // Zero
            rgb_operation: 0,   // Add
            source_alpha: 1,
            destination_alpha: 0,
            alpha_operation: 0,
            write_mask: 0xf, // MTLColorWriteMaskAll
        }
    }
}

pub(crate) const COLOR_ATTACHMENTS: usize = 8;

pub(crate) struct RenderPipelineDescriptor {
    pub vertex_function: objc::object::Id,
    pub fragment_function: objc::object::Id,
    pub vertex_descriptor: objc::object::Id,
    pub colors: [ColorAttachment; COLOR_ATTACHMENTS],
    pub depth_format: u64,
    pub stencil_format: u64,
    pub sample_count: u64,
    pub alpha_to_coverage: bool,
    /// The `-colorAttachments` collection, made on first use.
    pub color_objects: objc::object::Id,
}

pub(crate) struct RenderPipeline {
    pub gpu: &'static Gpu,
    pub vertex_module: vk::ShaderModule,
    pub fragment_module: vk::ShaderModule,
    pub vertex_name: std::ffi::CString,
    pub fragment_name: std::ffi::CString,
    pub layouts: Layouts,
    pub resources: Vec<ResourceBinding>,
    /// Metal buffer indices which feed vertex attributes rather than a
    /// descriptor, taken from the pipeline's vertex descriptor.
    pub vertex_streams: Vec<u32>,
    bindings: Vec<vk::VertexInputBindingDescription>,
    attributes: Vec<vk::VertexInputAttributeDescription>,
    colors: Vec<ColorAttachment>,
    pub depth_format: vk::Format,
    pub stencil_format: vk::Format,
    pub samples: vk::SampleCountFlags,
    alpha_to_coverage: bool,
    /// The pass shape this pipeline was declared against, without resolves;
    /// `variant` adds those from the key and asks the cache for the pass.
    pass: RenderPassKey,
    variants: Mutex<HashMap<RenderVariantKey, vk::Pipeline>>,
}

impl RenderPipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gpu: &'static Gpu,
        vertex: &EntryArtifact,
        fragment: &EntryArtifact,
        descriptor: &RenderPipelineDescriptor,
        layouts_in: &[VertexLayout; VERTEX_SLOTS],
        attributes_in: &[VertexAttribute; VERTEX_SLOTS],
    ) -> Result<Self, String> {
        let vertex_module = gpu.create_shader_module(&vertex.words, &vertex.name)?;
        let fragment_module = gpu.create_shader_module(&fragment.words, &fragment.name)?;
        let layouts = layouts_for(gpu, &[&vertex.resources, &fragment.resources])?;

        let mut bindings = Vec::new();
        let mut attributes = Vec::new();
        let mut vertex_streams = Vec::new();
        for (index, attribute) in attributes_in.iter().enumerate() {
            if attribute.format == 0 {
                continue;
            }
            let format = vertex_format(attribute.format).ok_or_else(|| {
                format!("MTLVertexFormat {} has no Vulkan form", attribute.format)
            })?;
            attributes.push(
                vk::VertexInputAttributeDescription::default()
                    .location(index as u32)
                    .binding(attribute.buffer_index as u32)
                    .format(format)
                    .offset(attribute.offset as u32),
            );
            if !vertex_streams.contains(&(attribute.buffer_index as u32)) {
                vertex_streams.push(attribute.buffer_index as u32);
            }
        }
        for stream in &vertex_streams {
            let layout = &layouts_in[*stream as usize];
            bindings.push(
                vk::VertexInputBindingDescription::default()
                    .binding(*stream)
                    .stride(layout.stride as u32)
                    // MTLVertexStepFunctionPerInstance is 2; per-vertex is 1
                    // and "constant" is 0, which Vulkan spells as a per-vertex
                    // stream with a zero stride the caller already set.
                    .input_rate(match layout.step_function {
                        2 => vk::VertexInputRate::INSTANCE,
                        _ => vk::VertexInputRate::VERTEX,
                    }),
            );
        }

        let colors = descriptor
            .colors
            .iter()
            .take_while(|color| color.pixel_format != 0)
            .copied()
            .collect::<Vec<_>>();
        let mut color_formats = Vec::new();
        for color in &colors {
            let format = gpu::pixel_format(color.pixel_format);
            if format == vk::Format::UNDEFINED {
                return Err(format!(
                    "colour attachment has no Vulkan form for MTLPixelFormat {}",
                    color.pixel_format
                ));
            }
            color_formats.push((
                format,
                vk::AttachmentLoadOp::DONT_CARE,
                vk::AttachmentStoreOp::STORE,
            ));
        }
        let depth_format = gpu::pixel_format(descriptor.depth_format);
        let stencil_format = gpu::pixel_format(descriptor.stencil_format);
        let samples = match descriptor.sample_count {
            0 | 1 => vk::SampleCountFlags::TYPE_1,
            2 => vk::SampleCountFlags::TYPE_2,
            4 => vk::SampleCountFlags::TYPE_4,
            8 => vk::SampleCountFlags::TYPE_8,
            _ => vk::SampleCountFlags::TYPE_16,
        };
        let attachment_depth = if depth_format != vk::Format::UNDEFINED {
            Some(depth_format)
        } else if stencil_format != vk::Format::UNDEFINED {
            Some(stencil_format)
        } else {
            None
        };
        let key = RenderPassKey {
            color_resolves: vec![None; color_formats.len()],
            colors: color_formats,
            depth: attachment_depth.map(|format| {
                (
                    format,
                    vk::AttachmentLoadOp::DONT_CARE,
                    vk::AttachmentStoreOp::STORE,
                )
            }),
            stencil: (stencil_format != vk::Format::UNDEFINED).then_some((
                vk::AttachmentLoadOp::DONT_CARE,
                vk::AttachmentStoreOp::STORE,
            )),
            samples,
        };
        gpu.render_pass(&key)?;

        let mut resources = vertex.resources.clone();
        resources.extend(fragment.resources.iter().cloned());
        Ok(Self {
            gpu,
            vertex_module,
            fragment_module,
            vertex_name: std::ffi::CString::new(vertex.name.as_str())
                .map_err(|_| "entry point name contains a NUL".to_string())?,
            fragment_name: std::ffi::CString::new(fragment.name.as_str())
                .map_err(|_| "entry point name contains a NUL".to_string())?,
            layouts,
            resources,
            vertex_streams,
            bindings,
            attributes,
            colors,
            depth_format,
            stencil_format,
            samples,
            alpha_to_coverage: descriptor.alpha_to_coverage,
            pass: key,
            variants: Mutex::new(HashMap::new()),
        })
    }

    pub fn variant(&self, key: &RenderVariantKey) -> Result<vk::Pipeline, String> {
        let mut cache = self.variants.lock().expect("render pipeline cache");
        if let Some(pipeline) = cache.get(key) {
            return Ok(*pipeline);
        }
        let mut shape = self.pass.clone();
        for (index, resolve) in shape.color_resolves.iter_mut().enumerate() {
            if key.resolve_mask & (1 << index) != 0 {
                *resolve = Some(self.pass.colors[index].0);
            }
        }
        let pass = self.gpu.render_pass(&shape)?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vertex_module)
                .name(&self.vertex_name),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.fragment_module)
                .name(&self.fragment_name),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&self.bindings)
            .vertex_attribute_descriptions(&self.attributes);
        let assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology(key.topology));
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            // MTLDepthClipModeClamp is 1.
            .depth_clamp_enable(key.depth_clip == 1)
            .polygon_mode(match key.fill_mode {
                1 => vk::PolygonMode::LINE,
                _ => vk::PolygonMode::FILL,
            })
            .cull_mode(match key.cull_mode {
                1 => vk::CullModeFlags::FRONT,
                2 => vk::CullModeFlags::BACK,
                _ => vk::CullModeFlags::NONE,
            })
            // Metal's window origin is upper-left and Vulkan's is too once the
            // viewport is flipped, so a Metal clockwise front face is a Vulkan
            // clockwise front face.
            .front_face(match key.winding {
                1 => vk::FrontFace::COUNTER_CLOCKWISE,
                _ => vk::FrontFace::CLOCKWISE,
            })
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(self.samples)
            .alpha_to_coverage_enable(self.alpha_to_coverage);
        let stencil_face = |face: &[u64; 6]| {
            vk::StencilOpState::default()
                .compare_op(compare_op(face[0]))
                .fail_op(stencil_op(face[1]))
                .depth_fail_op(stencil_op(face[2]))
                .pass_op(stencil_op(face[3]))
                .compare_mask(face[4] as u32)
                .write_mask(face[5] as u32)
        };
        let has_depth = self.depth_format != vk::Format::UNDEFINED
            || self.stencil_format != vk::Format::UNDEFINED;
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            // Metal has no depth-test switch: `MTLCompareFunctionAlways` with
            // writes off is how a game turns depth off.
            .depth_test_enable(has_depth)
            .depth_write_enable(has_depth && key.depth.write)
            .depth_compare_op(compare_op(key.depth.compare))
            .stencil_test_enable(has_depth && key.depth.stencil)
            .front(stencil_face(&key.depth.front))
            .back(stencil_face(&key.depth.back));
        let blends = self
            .colors
            .iter()
            .map(|color| {
                vk::PipelineColorBlendAttachmentState::default()
                    .blend_enable(color.blending)
                    .src_color_blend_factor(blend_factor(color.source_rgb))
                    .dst_color_blend_factor(blend_factor(color.destination_rgb))
                    .color_blend_op(blend_op(color.rgb_operation))
                    .src_alpha_blend_factor(blend_factor(color.source_alpha))
                    .dst_alpha_blend_factor(blend_factor(color.destination_alpha))
                    .alpha_blend_op(blend_op(color.alpha_operation))
                    .color_write_mask(write_mask(color.write_mask))
            })
            .collect::<Vec<_>>();
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
            vk::DynamicState::STENCIL_REFERENCE,
            vk::DynamicState::DEPTH_BIAS,
            vk::DynamicState::BLEND_CONSTANTS,
        ];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(self.layouts.pipeline)
            .render_pass(pass)
            .subpass(0);
        // SAFETY: every borrowed structure lives across the call.
        let pipeline = unsafe {
            self.gpu
                .device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
        }
        .map_err(|(_, error)| format!("vkCreateGraphicsPipelines failed with {error:?}"))?[0];
        cache.insert(*key, pipeline);
        Ok(pipeline)
    }
}
