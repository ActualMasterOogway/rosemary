//! Metal pipeline state, compiled to Vulkan pipelines.
//!
//! Metal and Vulkan disagree about what belongs to a pipeline. Metal's compute
//! threadgroup size is a dispatch parameter, and its depth/stencil, cull and
//! winding state belong to the encoder; Vulkan bakes all of them into the
//! pipeline object. Each state object therefore holds the immutable half and
//! caches a Vulkan pipeline per distinct combination of the rest.

use std::collections::HashMap;
use std::sync::Mutex;

use ash::vk;
use metal_translate::ir::{ResourceKind, Stage};
use metal_translate::{EntryArtifact, ResourceBinding};

use crate::gpu::{self, Gpu, RenderPassKey};
use crate::resource::{DepthStencilState, VERTEX_SLOTS, VertexAttribute, VertexLayout, compare_op};

/// The four descriptor sets of the Metal resource ABI.
pub(crate) const SETS: usize = 4;

pub(crate) struct Layouts {
    pub sets: [vk::DescriptorSetLayout; SETS],
    pub pipeline: vk::PipelineLayout,
}

pub(crate) fn descriptor_type(binding: &ResourceBinding) -> Option<vk::DescriptorType> {
    match binding.kind {
        ResourceKind::Buffer => Some(vk::DescriptorType::STORAGE_BUFFER),
        ResourceKind::Texture => Some(if binding.sampled {
            vk::DescriptorType::SAMPLED_IMAGE
        } else {
            vk::DescriptorType::STORAGE_IMAGE
        }),
        ResourceKind::Sampler => Some(vk::DescriptorType::SAMPLER),
        _ => None,
    }
}

fn stage_flags(stage: Stage) -> vk::ShaderStageFlags {
    match stage {
        Stage::Vertex => vk::ShaderStageFlags::VERTEX,
        Stage::Fragment => vk::ShaderStageFlags::FRAGMENT,
        _ => vk::ShaderStageFlags::COMPUTE,
    }
}

/// Build the four descriptor-set layouts and the pipeline layout for the
/// resources of one or two entry points.
pub(crate) fn layouts_for(gpu: &Gpu, resources: &[&[ResourceBinding]]) -> Result<Layouts, String> {
    let mut per_set: [Vec<(u32, vk::DescriptorType)>; SETS] = Default::default();
    let mut stages = vk::ShaderStageFlags::empty();
    for group in resources {
        for binding in *group {
            stages |= stage_flags(binding.stage);
            let Some(kind) = descriptor_type(binding) else {
                return Err(format!(
                    "resource kind {:?} has no Vulkan descriptor type",
                    binding.kind
                ));
            };
            let set = binding.descriptor_set as usize;
            if set >= SETS {
                return Err(format!("descriptor set {set} is outside the Metal ABI"));
            }
            per_set[set].push((binding.binding, kind));
        }
    }
    let mut sets = [vk::DescriptorSetLayout::null(); SETS];
    for (index, bindings) in per_set.iter().enumerate() {
        sets[index] = gpu.descriptor_set_layout(bindings, stages)?;
    }
    let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&sets);
    // SAFETY: the layouts live in the device's cache for its lifetime.
    let pipeline = unsafe { gpu.device.create_pipeline_layout(&info, None) }
        .map_err(|error| format!("vkCreatePipelineLayout failed with {error:?}"))?;
    Ok(Layouts { sets, pipeline })
}

// --------------------------------------------------------------- compute

pub(crate) struct ComputePipeline {
    pub gpu: &'static Gpu,
    pub device: objc::object::Id,
    pub module: vk::ShaderModule,
    pub name: std::ffi::CString,
    pub layouts: Layouts,
    pub resources: Vec<ResourceBinding>,
    /// One Vulkan pipeline per threadgroup size the guest has dispatched.
    variants: Mutex<HashMap<[u32; 3], vk::Pipeline>>,
}

impl ComputePipeline {
    pub fn new(
        gpu: &'static Gpu,
        device: objc::object::Id,
        entry: &EntryArtifact,
    ) -> Result<Self, String> {
        let module = gpu.create_shader_module(&entry.words, &entry.name)?;
        let layouts = layouts_for(gpu, &[&entry.resources])?;
        Ok(Self {
            gpu,
            device,
            module,
            name: std::ffi::CString::new(entry.name.as_str())
                .map_err(|_| "entry point name contains a NUL".to_string())?,
            layouts,
            resources: entry.resources.clone(),
            variants: Mutex::new(HashMap::new()),
        })
    }

    /// The Vulkan pipeline for one Metal threadgroup size.
    pub fn variant(&self, size: [u32; 3]) -> Result<vk::Pipeline, String> {
        let mut cache = self.variants.lock().expect("compute pipeline cache");
        if let Some(pipeline) = cache.get(&size) {
            return Ok(*pipeline);
        }
        let entries = metal_translate::WORKGROUP_SIZE_SPEC_IDS
            .iter()
            .enumerate()
            .map(|(index, id)| {
                vk::SpecializationMapEntry::default()
                    .constant_id(*id)
                    .offset((index * size_of::<u32>()) as u32)
                    .size(size_of::<u32>())
            })
            .collect::<Vec<_>>();
        let data: [u8; 12] = unsafe { std::mem::transmute(size) };
        let specialization = vk::SpecializationInfo::default()
            .map_entries(&entries)
            .data(&data);
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(self.module)
            .name(&self.name)
            .specialization_info(&specialization);
        let info = vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(self.layouts.pipeline);
        // Said before the call, not after: a driver's shader compiler is where a
        // bad module is met, and one that dies there leaves nothing to say it.
        gpu::trace(format!(
            "compiling compute pipeline {} for threadgroup {size:?}",
            self.name.to_string_lossy()
        ));
        // SAFETY: every borrowed structure lives across the call.
        let pipeline = unsafe {
            self.gpu
                .device
                .create_compute_pipelines(vk::PipelineCache::null(), &[info], None)
        }
        .map_err(|(_, error)| format!("vkCreateComputePipelines failed with {error:?}"))?[0];
        cache.insert(size, pipeline);
        Ok(pipeline)
    }
}

// ---------------------------------------------------------------- render

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

impl From<&DepthStencilState> for DepthStencilKey {
    fn from(state: &DepthStencilState) -> Self {
        let face = |face: &crate::resource::StencilState| {
            [
                face.compare,
                face.fail,
                face.depth_fail,
                face.pass,
                u64::from(face.read_mask),
                u64::from(face.write_mask),
            ]
        };
        Self {
            compare: state.depth_compare,
            write: state.depth_write,
            stencil: state.stencil_enabled,
            front: face(&state.front),
            back: face(&state.back),
        }
    }
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

pub(crate) fn topology(metal: u64) -> vk::PrimitiveTopology {
    match metal {
        0 => vk::PrimitiveTopology::POINT_LIST,
        1 => vk::PrimitiveTopology::LINE_LIST,
        2 => vk::PrimitiveTopology::LINE_STRIP,
        4 => vk::PrimitiveTopology::TRIANGLE_STRIP,
        _ => vk::PrimitiveTopology::TRIANGLE_LIST,
    }
}

fn stencil_op(metal: u64) -> vk::StencilOp {
    match metal {
        1 => vk::StencilOp::ZERO,
        2 => vk::StencilOp::REPLACE,
        3 => vk::StencilOp::INCREMENT_AND_CLAMP,
        4 => vk::StencilOp::DECREMENT_AND_CLAMP,
        5 => vk::StencilOp::INVERT,
        6 => vk::StencilOp::INCREMENT_AND_WRAP,
        7 => vk::StencilOp::DECREMENT_AND_WRAP,
        _ => vk::StencilOp::KEEP,
    }
}

fn blend_factor(metal: u64) -> vk::BlendFactor {
    match metal {
        1 => vk::BlendFactor::ONE,
        2 => vk::BlendFactor::SRC_COLOR,
        3 => vk::BlendFactor::ONE_MINUS_SRC_COLOR,
        4 => vk::BlendFactor::SRC_ALPHA,
        5 => vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        6 => vk::BlendFactor::DST_COLOR,
        7 => vk::BlendFactor::ONE_MINUS_DST_COLOR,
        8 => vk::BlendFactor::DST_ALPHA,
        9 => vk::BlendFactor::ONE_MINUS_DST_ALPHA,
        10 => vk::BlendFactor::SRC_ALPHA_SATURATE,
        11 => vk::BlendFactor::CONSTANT_COLOR,
        12 => vk::BlendFactor::ONE_MINUS_CONSTANT_COLOR,
        13 => vk::BlendFactor::CONSTANT_ALPHA,
        14 => vk::BlendFactor::ONE_MINUS_CONSTANT_ALPHA,
        15 => vk::BlendFactor::SRC1_COLOR,
        16 => vk::BlendFactor::ONE_MINUS_SRC1_COLOR,
        17 => vk::BlendFactor::SRC1_ALPHA,
        18 => vk::BlendFactor::ONE_MINUS_SRC1_ALPHA,
        _ => vk::BlendFactor::ZERO,
    }
}

fn blend_op(metal: u64) -> vk::BlendOp {
    match metal {
        1 => vk::BlendOp::SUBTRACT,
        2 => vk::BlendOp::REVERSE_SUBTRACT,
        3 => vk::BlendOp::MIN,
        4 => vk::BlendOp::MAX,
        _ => vk::BlendOp::ADD,
    }
}

fn write_mask(metal: u64) -> vk::ColorComponentFlags {
    let mut mask = vk::ColorComponentFlags::empty();
    if metal & 0x8 != 0 {
        mask |= vk::ColorComponentFlags::R;
    }
    if metal & 0x4 != 0 {
        mask |= vk::ColorComponentFlags::G;
    }
    if metal & 0x2 != 0 {
        mask |= vk::ColorComponentFlags::B;
    }
    if metal & 0x1 != 0 {
        mask |= vk::ColorComponentFlags::A;
    }
    mask
}

/// `MTLVertexFormat` to `VkFormat`.
pub(crate) fn vertex_format(metal: u64) -> Option<vk::Format> {
    use vk::Format as F;
    Some(match metal {
        1 => F::R8G8_UINT,
        2 => F::R8G8B8_UINT,
        3 => F::R8G8B8A8_UINT,
        4 => F::R8G8_SINT,
        5 => F::R8G8B8_SINT,
        6 => F::R8G8B8A8_SINT,
        7 => F::R8G8_UNORM,
        8 => F::R8G8B8_UNORM,
        9 => F::R8G8B8A8_UNORM,
        10 => F::R8G8_SNORM,
        11 => F::R8G8B8_SNORM,
        12 => F::R8G8B8A8_SNORM,
        13 => F::R16G16_UINT,
        14 => F::R16G16B16_UINT,
        15 => F::R16G16B16A16_UINT,
        16 => F::R16G16_SINT,
        17 => F::R16G16B16_SINT,
        18 => F::R16G16B16A16_SINT,
        19 => F::R16G16_UNORM,
        20 => F::R16G16B16_UNORM,
        21 => F::R16G16B16A16_UNORM,
        22 => F::R16G16_SNORM,
        23 => F::R16G16B16_SNORM,
        24 => F::R16G16B16A16_SNORM,
        25 => F::R16G16_SFLOAT,
        26 => F::R16G16B16_SFLOAT,
        27 => F::R16G16B16A16_SFLOAT,
        28 => F::R32_SFLOAT,
        29 => F::R32G32_SFLOAT,
        30 => F::R32G32B32_SFLOAT,
        31 => F::R32G32B32A32_SFLOAT,
        32 => F::R32_SINT,
        33 => F::R32G32_SINT,
        34 => F::R32G32B32_SINT,
        35 => F::R32G32B32A32_SINT,
        36 => F::R32_UINT,
        37 => F::R32G32_UINT,
        38 => F::R32G32B32_UINT,
        39 => F::R32G32B32A32_UINT,
        40 => F::A2B10G10R10_SNORM_PACK32,
        41 => F::A2B10G10R10_UNORM_PACK32,
        42 => F::B8G8R8A8_UNORM,
        45 => F::R8_UINT,
        46 => F::R8_SINT,
        47 => F::R8_UNORM,
        48 => F::R8_SNORM,
        49 => F::R16_UINT,
        50 => F::R16_SINT,
        51 => F::R16_UNORM,
        52 => F::R16_SNORM,
        53 => F::R16_SFLOAT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metal_blend_state_maps_to_vulkan() {
        assert_eq!(blend_factor(5), vk::BlendFactor::ONE_MINUS_SRC_ALPHA);
        assert_eq!(blend_op(2), vk::BlendOp::REVERSE_SUBTRACT);
        assert_eq!(
            write_mask(0xf),
            vk::ColorComponentFlags::R
                | vk::ColorComponentFlags::G
                | vk::ColorComponentFlags::B
                | vk::ColorComponentFlags::A
        );
        assert_eq!(write_mask(0), vk::ColorComponentFlags::empty());
    }

    #[test]
    fn a_metal_write_mask_names_red_in_its_high_bit() {
        assert_eq!(write_mask(0x8), vk::ColorComponentFlags::R);
        assert_eq!(write_mask(0x1), vk::ColorComponentFlags::A);
    }

    #[test]
    fn vertex_formats_cover_the_common_metal_spellings() {
        assert_eq!(vertex_format(31), Some(vk::Format::R32G32B32A32_SFLOAT));
        assert_eq!(vertex_format(9), Some(vk::Format::R8G8B8A8_UNORM));
        assert_eq!(vertex_format(9999), None);
    }
}

// ------------------------------------------------- Objective-C descriptors

use crate::object::{self, methods};
use objc::object::Id;
use objc::sel::Sel;

/// One element of `-[MTLRenderPipelineDescriptor colorAttachments]`, which
/// mutates the descriptor which vended it.
#[derive(Clone, Copy)]
pub(crate) struct ColorElement {
    pub owner: Id,
    pub index: usize,
}

unsafe extern "C" fn descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated render pipeline descriptor.
    *unsafe { object::body::<RenderPipelineDescriptor>(receiver) } = RenderPipelineDescriptor {
        vertex_function: std::ptr::null_mut(),
        fragment_function: std::ptr::null_mut(),
        vertex_descriptor: std::ptr::null_mut(),
        colors: [ColorAttachment::fresh(); COLOR_ATTACHMENTS],
        depth_format: 0,
        stencil_format: 0,
        sample_count: 1,
        alpha_to_coverage: false,
        color_objects: std::ptr::null_mut(),
    };
    receiver
}

unsafe extern "C" fn set_descriptor_object(receiver: Id, sel: Sel, value: Id) {
    // SAFETY: a render pipeline descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setVertexFunction:" => descriptor.vertex_function = objc::arc::retain(value),
        b"setFragmentFunction:" => descriptor.fragment_function = objc::arc::retain(value),
        b"setVertexDescriptor:" => descriptor.vertex_descriptor = objc::arc::retain(value),
        _ => (),
    }
}

unsafe extern "C" fn set_descriptor_number(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a render pipeline descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setDepthAttachmentPixelFormat:" => descriptor.depth_format = value,
        b"setStencilAttachmentPixelFormat:" => descriptor.stencil_format = value,
        b"setSampleCount:" | b"setRasterSampleCount:" => descriptor.sample_count = value,
        b"setAlphaToCoverageEnabled:" => descriptor.alpha_to_coverage = value & 0xff != 0,
        _ => (),
    }
}

unsafe extern "C" fn color_attachments(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pipeline descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    if !descriptor.color_objects.is_null() {
        return descriptor.color_objects;
    }
    let elements = (0..COLOR_ATTACHMENTS)
        .map(|index| {
            object::make(
                c"R3MTLColorAttachmentDescriptor",
                ColorElement {
                    owner: receiver,
                    index,
                },
            )
        })
        .collect::<Vec<_>>();
    let collection = object::make(
        c"R3MTLIndexedCollection",
        crate::resource::IndexedCollection { elements },
    );
    // SAFETY: as above.
    unsafe { object::body::<RenderPipelineDescriptor>(receiver) }.color_objects = collection;
    collection
}

unsafe extern "C" fn set_color_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a colour attachment element of this crate's.
    let element = *unsafe { object::body::<ColorElement>(receiver) };
    // SAFETY: the element holds the descriptor which made it.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(element.owner) };
    let color = &mut descriptor.colors[element.index];
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setPixelFormat:" => color.pixel_format = value,
        b"setBlendingEnabled:" => color.blending = value & 0xff != 0,
        b"setSourceRGBBlendFactor:" => color.source_rgb = value,
        b"setDestinationRGBBlendFactor:" => color.destination_rgb = value,
        b"setRgbBlendOperation:" => color.rgb_operation = value,
        b"setSourceAlphaBlendFactor:" => color.source_alpha = value,
        b"setDestinationAlphaBlendFactor:" => color.destination_alpha = value,
        b"setAlphaBlendOperation:" => color.alpha_operation = value,
        b"setWriteMask:" => color.write_mask = value,
        _ => (),
    }
}

unsafe extern "C" fn compute_pipeline_device(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a compute pipeline state of this crate's.
    unsafe { object::body::<ComputePipeline>(receiver) }.device
}

unsafe extern "C" fn compute_pipeline_thread_width(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: as above.
    let state = unsafe { object::body::<ComputePipeline>(receiver) };
    u64::from(state.gpu.limits.max_compute_work_group_invocations.min(32))
}

unsafe extern "C" fn compute_pipeline_max_threads(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: as above.
    let state = unsafe { object::body::<ComputePipeline>(receiver) };
    u64::from(state.gpu.limits.max_compute_work_group_invocations)
}

pub(crate) fn install() {
    let descriptor = object::register::<RenderPipelineDescriptor>(c"MTLRenderPipelineDescriptor");
    let color = object::register::<ColorElement>(c"R3MTLColorAttachmentDescriptor");
    // `MTLCompileOptions` carries nothing this translator reads: a game sets a
    // language version and preprocessor macros, and the MSL frontend accepts
    // one dialect. It exists so `-newLibraryWithSource:options:error:` has
    // something to be handed.
    let options = object::register::<()>(c"MTLCompileOptions");
    methods!(options, {
        "init" => identity, c"@16@0:8";
        "setFastMathEnabled:" => crate::no_op, c"v20@0:8c16";
        "setLanguageVersion:" => crate::no_op, c"v24@0:8Q16";
        "setPreprocessorMacros:" => crate::no_op, c"v24@0:8@16";
    });
    methods!(descriptor, {
        "init" => descriptor_init, c"@16@0:8";
        "colorAttachments" => color_attachments, c"@16@0:8";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
        "reset" => descriptor_init, c"@16@0:8";
    });
    for selector in [
        "setVertexFunction:",
        "setFragmentFunction:",
        "setVertexDescriptor:",
    ] {
        descriptor.replace_method(
            objc::sel::get(selector),
            set_descriptor_object as *const std::ffi::c_void,
            c"v24@0:8@16",
        );
    }
    for selector in [
        "setDepthAttachmentPixelFormat:",
        "setStencilAttachmentPixelFormat:",
        "setSampleCount:",
        "setRasterSampleCount:",
        "setAlphaToCoverageEnabled:",
    ] {
        descriptor.replace_method(
            objc::sel::get(selector),
            set_descriptor_number as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }
    for selector in [
        "setPixelFormat:",
        "setBlendingEnabled:",
        "setSourceRGBBlendFactor:",
        "setDestinationRGBBlendFactor:",
        "setRgbBlendOperation:",
        "setSourceAlphaBlendFactor:",
        "setDestinationAlphaBlendFactor:",
        "setAlphaBlendOperation:",
        "setWriteMask:",
    ] {
        color.replace_method(
            objc::sel::get(selector),
            set_color_value as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }

    let compute = objc::class::lookup(c"R3MTLComputePipelineState").expect("registered first");
    methods!(compute, {
        "device" => compute_pipeline_device, c"@16@0:8";
        "threadExecutionWidth" => compute_pipeline_thread_width, c"Q16@0:8";
        "maxTotalThreadsPerThreadgroup" => compute_pipeline_max_threads, c"Q16@0:8";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });
    let render = objc::class::lookup(c"R3MTLRenderPipelineState").expect("registered first");
    methods!(render, {
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });
}

unsafe extern "C" fn identity(receiver: Id, _sel: Sel) -> Id {
    receiver
}
