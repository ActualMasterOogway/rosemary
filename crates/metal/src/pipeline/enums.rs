use super::*;

pub(crate) fn topology(metal: u64) -> vk::PrimitiveTopology {
    match metal {
        0 => vk::PrimitiveTopology::POINT_LIST,
        1 => vk::PrimitiveTopology::LINE_LIST,
        2 => vk::PrimitiveTopology::LINE_STRIP,
        4 => vk::PrimitiveTopology::TRIANGLE_STRIP,
        _ => vk::PrimitiveTopology::TRIANGLE_LIST,
    }
}

pub(super) fn stencil_op(metal: u64) -> vk::StencilOp {
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

pub(super) fn blend_factor(metal: u64) -> vk::BlendFactor {
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

pub(super) fn blend_op(metal: u64) -> vk::BlendOp {
    match metal {
        1 => vk::BlendOp::SUBTRACT,
        2 => vk::BlendOp::REVERSE_SUBTRACT,
        3 => vk::BlendOp::MIN,
        4 => vk::BlendOp::MAX,
        _ => vk::BlendOp::ADD,
    }
}

pub(super) fn write_mask(metal: u64) -> vk::ColorComponentFlags {
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
