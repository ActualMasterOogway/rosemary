//! `MTLBlitCommandEncoder`: copies and mipmap generation.

use super::*;

pub(super) unsafe extern "C" fn copy_buffer_to_buffer(
    receiver: Id,
    _sel: Sel,
    source: Id,
    source_offset: u64,
    destination: Id,
    destination_offset: u64,
    size: u64,
) {
    if source.is_null() || destination.is_null() || size == 0 {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only buffers of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Buffer>(source),
            object::body::<Buffer>(destination),
        )
    };
    let region = vk::BufferCopy::default()
        .src_offset(source_offset)
        .dst_offset(destination_offset)
        .size(size);
    // SAFETY: recording into this command buffer.
    unsafe {
        command.gpu.device.cmd_copy_buffer(
            command.command_buffer,
            from.inner.buffer,
            to.inner.buffer,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn copy_buffer_to_texture(
    receiver: Id,
    _sel: Sel,
    source: Id,
    source_offset: u64,
    bytes_per_row: u64,
    bytes_per_image: u64,
    size: MtlSize,
    destination: Id,
    slice: u64,
    level: u64,
    origin: MtlOrigin,
) {
    if source.is_null() || destination.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only resources of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Buffer>(source),
            object::body::<Texture>(destination),
        )
    };
    let texel = u64::from(gpu::texel_size(to.image.format));
    let (block_width, block_height) = gpu::block_extent(to.image.format);
    let row_texels = if bytes_per_row == 0 {
        size.width
    } else {
        bytes_per_row / texel * u64::from(block_width)
    };
    let rows = if bytes_per_image == 0 || bytes_per_row == 0 {
        size.height
    } else {
        bytes_per_image / bytes_per_row * u64::from(block_height)
    };
    let region = vk::BufferImageCopy::default()
        .buffer_offset(source_offset)
        .buffer_row_length(row_texels as u32)
        .buffer_image_height(rows as u32)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: to.image.aspect,
            mip_level: level as u32,
            base_array_layer: slice as u32,
            layer_count: 1,
        })
        .image_offset(vk::Offset3D {
            x: origin.x as i32,
            y: origin.y as i32,
            z: origin.z as i32,
        })
        .image_extent(vk::Extent3D {
            width: size.width.max(1) as u32,
            height: size.height.max(1) as u32,
            depth: size.depth.max(1) as u32,
        });
    // SAFETY: recording into this command buffer; the image is in GENERAL.
    unsafe {
        command.gpu.device.cmd_copy_buffer_to_image(
            command.command_buffer,
            from.inner.buffer,
            to.image.image,
            vk::ImageLayout::GENERAL,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn copy_texture_to_buffer(
    receiver: Id,
    _sel: Sel,
    source: Id,
    slice: u64,
    level: u64,
    origin: MtlOrigin,
    size: MtlSize,
    destination: Id,
    destination_offset: u64,
    bytes_per_row: u64,
    bytes_per_image: u64,
) {
    if source.is_null() || destination.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only resources of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Texture>(source),
            object::body::<Buffer>(destination),
        )
    };
    let texel = u64::from(gpu::texel_size(from.image.format));
    let (block_width, block_height) = gpu::block_extent(from.image.format);
    let row_texels = if bytes_per_row == 0 {
        size.width
    } else {
        bytes_per_row / texel * u64::from(block_width)
    };
    let rows = if bytes_per_image == 0 || bytes_per_row == 0 {
        size.height
    } else {
        bytes_per_image / bytes_per_row * u64::from(block_height)
    };
    let region = vk::BufferImageCopy::default()
        .buffer_offset(destination_offset)
        .buffer_row_length(row_texels as u32)
        .buffer_image_height(rows as u32)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: from.image.aspect,
            mip_level: level as u32,
            base_array_layer: slice as u32,
            layer_count: 1,
        })
        .image_offset(vk::Offset3D {
            x: origin.x as i32,
            y: origin.y as i32,
            z: origin.z as i32,
        })
        .image_extent(vk::Extent3D {
            width: size.width.max(1) as u32,
            height: size.height.max(1) as u32,
            depth: size.depth.max(1) as u32,
        });
    // SAFETY: recording into this command buffer; the image is in GENERAL.
    unsafe {
        command.gpu.device.cmd_copy_image_to_buffer(
            command.command_buffer,
            from.image.image,
            vk::ImageLayout::GENERAL,
            to.inner.buffer,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn copy_texture_to_texture(
    receiver: Id,
    _sel: Sel,
    source: Id,
    source_slice: u64,
    source_level: u64,
    source_origin: MtlOrigin,
    size: MtlSize,
    destination: Id,
    destination_slice: u64,
    destination_level: u64,
    destination_origin: MtlOrigin,
) {
    if source.is_null() || destination.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only textures of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Texture>(source),
            object::body::<Texture>(destination),
        )
    };
    let region = vk::ImageCopy::default()
        .src_subresource(vk::ImageSubresourceLayers {
            aspect_mask: from.image.aspect,
            mip_level: source_level as u32,
            base_array_layer: source_slice as u32,
            layer_count: 1,
        })
        .src_offset(vk::Offset3D {
            x: source_origin.x as i32,
            y: source_origin.y as i32,
            z: source_origin.z as i32,
        })
        .dst_subresource(vk::ImageSubresourceLayers {
            aspect_mask: to.image.aspect,
            mip_level: destination_level as u32,
            base_array_layer: destination_slice as u32,
            layer_count: 1,
        })
        .dst_offset(vk::Offset3D {
            x: destination_origin.x as i32,
            y: destination_origin.y as i32,
            z: destination_origin.z as i32,
        })
        .extent(vk::Extent3D {
            width: size.width.max(1) as u32,
            height: size.height.max(1) as u32,
            depth: size.depth.max(1) as u32,
        });
    // SAFETY: recording into this command buffer; both images are in GENERAL.
    unsafe {
        command.gpu.device.cmd_copy_image(
            command.command_buffer,
            from.image.image,
            vk::ImageLayout::GENERAL,
            to.image.image,
            vk::ImageLayout::GENERAL,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

/// `-generateMipmapsForTexture:`: a chain of halving blits, which is what
/// Metal's own implementation does for a colour texture.
pub(super) unsafe extern "C" fn generate_mipmaps(receiver: Id, _sel: Sel, texture: Id) {
    if texture.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only a texture of this crate's reaches a blit encoder.
    let image = unsafe { object::body::<Texture>(texture) };
    let device = &command.gpu.device;
    let vk_command = command.command_buffer;
    let mut width = image.image.width as i32;
    let mut height = image.image.height as i32;
    for level in 1..image.image.levels {
        let next_width = (width / 2).max(1);
        let next_height = (height / 2).max(1);
        let blit = vk::ImageBlit::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: image.image.aspect,
                mip_level: level - 1,
                base_array_layer: 0,
                layer_count: image.image.layers,
            })
            .src_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: width,
                    y: height,
                    z: 1,
                },
            ])
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: image.image.aspect,
                mip_level: level,
                base_array_layer: 0,
                layer_count: image.image.layers,
            })
            .dst_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: next_width,
                    y: next_height,
                    z: 1,
                },
            ]);
        // SAFETY: recording into this command buffer; the image is in GENERAL.
        unsafe {
            device.cmd_blit_image(
                vk_command,
                image.image.image,
                vk::ImageLayout::GENERAL,
                image.image.image,
                vk::ImageLayout::GENERAL,
                &[blit],
                vk::Filter::LINEAR,
            )
        };
        encoder.barrier();
        width = next_width;
        height = next_height;
    }
    command.retain(texture);
}
