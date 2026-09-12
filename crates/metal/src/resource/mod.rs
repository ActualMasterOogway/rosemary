//! Metal's resources: buffers, textures, samplers and depth/stencil state.
//!
//! Each is a real Vulkan object. Metal's descriptors are plain value types
//! which a game fills in and hands to the device, so they are stored as-is and
//! read back at creation time.
//!
//! One module per Metal object family; `install` stays here because the
//! Objective-C classes are registered together.

mod buffer;
mod depth_stencil;
mod sampler;
mod texture;
mod vertex;

use objc::object::Id;
use objc::sel::Sel;
use static_init::dynamic;

use crate::gpu::{self, Gpu, GpuBuffer, GpuImage, ImageDescription};
use crate::object::{self, methods};
use ash::vk;

use buffer::*;
use depth_stencil::*;
use sampler::*;
use texture::*;
use vertex::*;

pub(crate) use buffer::new_buffer;
pub(crate) use depth_stencil::{DepthStencilState, StencilState, new_depth_stencil_state};
pub(crate) use sampler::{SamplerDescriptor, SamplerState, compare_op, new_sampler};
pub(crate) use texture::{Texture, TextureDescriptor, drawable_texture, new_texture};
pub(crate) use vertex::{
    IndexedCollection, VERTEX_SLOTS, VertexAttribute, VertexDescriptor, VertexLayout,
};

pub(crate) struct Buffer {
    pub gpu: &'static Gpu,
    pub inner: GpuBuffer,
    pub options: u64,
}

impl Default for SamplerDescriptor {
    fn default() -> Self {
        Self {
            min_filter: 0,
            mag_filter: 0,
            mip_filter: 0,
            address: [0; 3],
            max_anisotropy: 1,
            // `MTLCompareFunctionNever` is the Metal default, and it means the
            // sampler does no comparison at all rather than always failing.
            compare_function: 0,
            lod_min_clamp: 0.0,
            lod_max_clamp: f32::MAX,
            normalized_coordinates: true,
            border_color: 0,
        }
    }
}

impl Default for StencilState {
    fn default() -> Self {
        Self {
            compare: 7, // Always
            fail: 0,
            depth_fail: 0,
            pass: 0,
            read_mask: u32::MAX,
            write_mask: u32::MAX,
        }
    }
}

impl Default for DepthStencilState {
    fn default() -> Self {
        Self {
            depth_compare: 7, // Always
            depth_write: false,
            front: StencilState::default(),
            back: StencilState::default(),
            stencil_enabled: false,
        }
    }
}

// --------------------------------------------------------------- textures

// --------------------------------------------------------------- samplers

// --------------------------------------------------------- depth/stencil

// ------------------------------------------------------- vertex descriptor

pub(crate) fn install() {
    let buffer = object::register::<Buffer>(c"R3MTLBuffer");
    methods!(buffer, {
        "dealloc" => buffer_dealloc, c"v16@0:8";
        "contents" => buffer_contents, c"^v16@0:8";
        "length" => buffer_length, c"Q16@0:8";
        "storageMode" => buffer_storage_mode, c"Q16@0:8";
        "didModifyRange:" => crate::no_op, c"v32@0:8{_NSRange=QQ}16";
        "addDebugMarker:range:" => crate::no_op, c"v40@0:8@16{_NSRange=QQ}24";
    });

    let descriptor = object::register::<TextureDescriptor>(c"MTLTextureDescriptor");
    let texture = object::register::<Texture>(c"R3MTLTexture");
    methods!(descriptor, {
        "init" => descriptor_init, c"@16@0:8";
    });
    for selector in [
        "setTextureType:",
        "setPixelFormat:",
        "setWidth:",
        "setHeight:",
        "setDepth:",
        "setMipmapLevelCount:",
        "setSampleCount:",
        "setArrayLength:",
        "setStorageMode:",
        "setUsage:",
    ] {
        descriptor.replace_method(
            objc::sel::get(selector),
            set_descriptor_value as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }
    for selector in [
        "textureType",
        "pixelFormat",
        "width",
        "height",
        "depth",
        "mipmapLevelCount",
        "sampleCount",
        "arrayLength",
        "storageMode",
        "usage",
    ] {
        descriptor.replace_method(
            objc::sel::get(selector),
            descriptor_value as *const std::ffi::c_void,
            c"Q16@0:8",
        );
        texture.replace_method(
            objc::sel::get(selector),
            texture_value as *const std::ffi::c_void,
            c"Q16@0:8",
        );
    }
    methods!(texture, {
        "dealloc" => texture_dealloc, c"v16@0:8";
        "replaceRegion:mipmapLevel:slice:withBytes:bytesPerRow:bytesPerImage:"
            => replace_region, c"v96@0:8{MTLRegion={MTLOrigin=QQQ}{MTLSize=QQQ}}16Q64Q72^v80Q88";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });

    let sampler_descriptor = object::register::<SamplerDescriptor>(c"MTLSamplerDescriptor");
    object::register::<SamplerState>(c"R3MTLSamplerState");
    methods!(sampler_descriptor, {
        "init" => sampler_descriptor_init, c"@16@0:8";
        "setLodMinClamp:" => set_sampler_clamp, c"v20@0:8f16";
        "setLodMaxClamp:" => set_sampler_clamp, c"v20@0:8f16";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });
    for selector in [
        "setMinFilter:",
        "setMagFilter:",
        "setMipFilter:",
        "setSAddressMode:",
        "setTAddressMode:",
        "setRAddressMode:",
        "setMaxAnisotropy:",
        "setCompareFunction:",
        "setBorderColor:",
        "setNormalizedCoordinates:",
    ] {
        sampler_descriptor.replace_method(
            objc::sel::get(selector),
            set_sampler_value as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }

    let depth = object::register::<DepthStencilState>(c"MTLDepthStencilDescriptor");
    object::register::<DepthStencilState>(c"R3MTLDepthStencilState");
    let stencil = object::register::<StencilState>(c"MTLStencilDescriptor");
    methods!(depth, {
        "init" => depth_descriptor_init, c"@16@0:8";
        "depthCompareFunction" => depth_compare, c"Q16@0:8";
        "setDepthCompareFunction:" => set_depth_compare, c"v24@0:8Q16";
        "isDepthWriteEnabled" => depth_write, c"c16@0:8";
        "setDepthWriteEnabled:" => set_depth_write, c"v20@0:8c16";
        "frontFaceStencil" => face_stencil, c"@16@0:8";
        "setFrontFaceStencil:" => set_face_stencil, c"v24@0:8@16";
        "backFaceStencil" => face_stencil, c"@16@0:8";
        "setBackFaceStencil:" => set_face_stencil, c"v24@0:8@16";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });
    methods!(stencil, {
        "init" => stencil_descriptor_init, c"@16@0:8";
    });
    for selector in [
        "setStencilCompareFunction:",
        "setStencilFailureOperation:",
        "setDepthFailureOperation:",
        "setDepthStencilPassOperation:",
        "setReadMask:",
        "setWriteMask:",
    ] {
        stencil.replace_method(
            objc::sel::get(selector),
            set_stencil_value as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }

    let vertex = object::register::<VertexDescriptor>(c"MTLVertexDescriptor");
    let collection = object::register::<IndexedCollection>(c"R3MTLIndexedCollection");
    let element = object::register::<VertexElement>(c"R3MTLVertexElement");
    methods!(vertex, {
        "init" => vertex_descriptor_init, c"@16@0:8";
        "layouts" => vertex_layouts, c"@16@0:8";
        "attributes" => vertex_attributes, c"@16@0:8";
        "dealloc" => vertex_descriptor_dealloc, c"v16@0:8";
    });
    methods!(collection, {
        "objectAtIndexedSubscript:" => indexed_element, c"@24@0:8Q16";
        "dealloc" => collection_dealloc, c"v16@0:8";
    });
    for selector in [
        "setStride:",
        "setStepFunction:",
        "setStepRate:",
        "setFormat:",
        "setOffset:",
        "setBufferIndex:",
    ] {
        element.replace_method(
            objc::sel::get(selector),
            set_vertex_element_value as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }
}
