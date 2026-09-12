//! `MTLSamplerState` and its descriptor.

use super::*;

pub(crate) struct SamplerState {
    pub sampler: vk::Sampler,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SamplerDescriptor {
    pub min_filter: u64,
    pub mag_filter: u64,
    pub mip_filter: u64,
    pub address: [u64; 3],
    pub max_anisotropy: u64,
    pub compare_function: u64,
    pub lod_min_clamp: f32,
    pub lod_max_clamp: f32,
    pub normalized_coordinates: bool,
    pub border_color: u64,
}

pub(crate) fn filter(metal: u64) -> vk::Filter {
    match metal {
        1 => vk::Filter::LINEAR,
        _ => vk::Filter::NEAREST,
    }
}

pub(super) fn mip_mode(metal: u64) -> vk::SamplerMipmapMode {
    match metal {
        2 => vk::SamplerMipmapMode::LINEAR,
        _ => vk::SamplerMipmapMode::NEAREST,
    }
}

pub(super) fn address_mode(metal: u64) -> vk::SamplerAddressMode {
    match metal {
        1 => vk::SamplerAddressMode::MIRROR_CLAMP_TO_EDGE,
        2 => vk::SamplerAddressMode::REPEAT,
        3 => vk::SamplerAddressMode::MIRRORED_REPEAT,
        // MTLSamplerAddressModeClampToZero has no Vulkan address mode; a
        // transparent-black border is the same result for a colour texture.
        4 | 5 => vk::SamplerAddressMode::CLAMP_TO_BORDER,
        _ => vk::SamplerAddressMode::CLAMP_TO_EDGE,
    }
}

pub(crate) fn compare_op(metal: u64) -> vk::CompareOp {
    match metal {
        0 => vk::CompareOp::NEVER,
        1 => vk::CompareOp::LESS,
        2 => vk::CompareOp::EQUAL,
        3 => vk::CompareOp::LESS_OR_EQUAL,
        4 => vk::CompareOp::GREATER,
        5 => vk::CompareOp::NOT_EQUAL,
        6 => vk::CompareOp::GREATER_OR_EQUAL,
        _ => vk::CompareOp::ALWAYS,
    }
}

pub(crate) fn new_sampler(gpu: &'static Gpu, descriptor: &SamplerDescriptor) -> Id {
    let border = match descriptor.border_color {
        1 => vk::BorderColor::FLOAT_OPAQUE_BLACK,
        2 => vk::BorderColor::FLOAT_OPAQUE_WHITE,
        _ => vk::BorderColor::FLOAT_TRANSPARENT_BLACK,
    };
    let anisotropy = descriptor.max_anisotropy.clamp(1, 16) as f32;
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(filter(descriptor.mag_filter))
        .min_filter(filter(descriptor.min_filter))
        .mipmap_mode(mip_mode(descriptor.mip_filter))
        .address_mode_u(address_mode(descriptor.address[0]))
        .address_mode_v(address_mode(descriptor.address[1]))
        .address_mode_w(address_mode(descriptor.address[2]))
        .anisotropy_enable(anisotropy > 1.0)
        .max_anisotropy(anisotropy)
        .compare_enable(descriptor.compare_function != 0)
        .compare_op(compare_op(descriptor.compare_function))
        .min_lod(descriptor.lod_min_clamp)
        .max_lod(if descriptor.mip_filter == 0 {
            // Metal's "not mipmapped" samples level zero whatever the texture
            // has; Vulkan spells that as a zero LOD clamp.
            0.25
        } else {
            descriptor.lod_max_clamp
        })
        .border_color(border)
        .unnormalized_coordinates(!descriptor.normalized_coordinates);
    // SAFETY: a well-formed create info on this live device.
    let sampler = match unsafe { gpu.device.create_sampler(&info, None) } {
        Ok(sampler) => sampler,
        Err(error) => {
            gpu::trace(format!("vkCreateSampler failed with {error:?}"));
            return std::ptr::null_mut();
        }
    };
    object::make(c"R3MTLSamplerState", SamplerState { sampler })
}

pub(super) unsafe extern "C" fn sampler_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated sampler descriptor.
    *unsafe { object::body::<SamplerDescriptor>(receiver) } = SamplerDescriptor::default();
    receiver
}

pub(super) unsafe extern "C" fn set_sampler_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a sampler descriptor of this crate's.
    let descriptor = unsafe { object::body::<SamplerDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setMinFilter:" => descriptor.min_filter = value,
        b"setMagFilter:" => descriptor.mag_filter = value,
        b"setMipFilter:" => descriptor.mip_filter = value,
        b"setSAddressMode:" => descriptor.address[0] = value,
        b"setTAddressMode:" => descriptor.address[1] = value,
        b"setRAddressMode:" => descriptor.address[2] = value,
        b"setMaxAnisotropy:" => descriptor.max_anisotropy = value,
        b"setCompareFunction:" => descriptor.compare_function = value,
        b"setBorderColor:" => descriptor.border_color = value,
        b"setNormalizedCoordinates:" => descriptor.normalized_coordinates = value & 0xff != 0,
        _ => unreachable!("only registered sampler setters reach this method"),
    }
}

pub(super) unsafe extern "C" fn set_sampler_clamp(receiver: Id, sel: Sel, value: f32) {
    // SAFETY: a sampler descriptor of this crate's.
    let descriptor = unsafe { object::body::<SamplerDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setLodMinClamp:" => descriptor.lod_min_clamp = value,
        b"setLodMaxClamp:" => descriptor.lod_max_clamp = value,
        _ => unreachable!("only registered clamp setters reach this method"),
    }
}
