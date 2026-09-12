//! Metal pipeline state, compiled to Vulkan pipelines.
//!
//! Metal and Vulkan disagree about what belongs to a pipeline. Metal's compute
//! threadgroup size is a dispatch parameter, and its depth/stencil, cull and
//! winding state belong to the encoder; Vulkan bakes all of them into the
//! pipeline object. Each state object therefore holds the immutable half and
//! caches a Vulkan pipeline per distinct combination of the rest.

mod compute;
mod descriptor;
mod enums;
mod render;

use std::collections::HashMap;
use std::sync::Mutex;

use ash::vk;
use metal_translate::ir::{ResourceKind, Stage};
use metal_translate::{EntryArtifact, ResourceBinding};

use crate::gpu::{self, Gpu, RenderPassKey};
use crate::object::{self, methods};
use crate::resource::{DepthStencilState, VERTEX_SLOTS, VertexAttribute, VertexLayout, compare_op};
use objc::object::Id;
use objc::sel::Sel;

use compute::*;
use descriptor::*;
use enums::*;

pub(crate) use compute::ComputePipeline;
pub(crate) use render::{
    COLOR_ATTACHMENTS, ColorAttachment, DepthStencilKey, RenderPipeline, RenderPipelineDescriptor,
    RenderVariantKey,
};

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

// ---------------------------------------------------------------- render

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

// ------------------------------------------------- Objective-C descriptors

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
        "dealloc" => descriptor_dealloc, c"v16@0:8";
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
