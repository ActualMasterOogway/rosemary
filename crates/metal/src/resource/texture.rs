//! `MTLTexture` and its descriptor.

use super::*;

pub(crate) struct Texture {
    pub gpu: &'static Gpu,
    pub descriptor: TextureDescriptor,
    pub image: GpuImage,
    /// One view per mip level and array slice a render pass has attached,
    /// made on first use. A texture usually has one and a blur pyramid has one
    /// per level, so this is a short list rather than a map.
    attachment_views: Vec<((u32, u32), vk::ImageView)>,
}

impl Texture {
    /// The framebuffer attachment for one mip level and array slice.
    pub fn attachment_view(&mut self, level: u32, slice: u32) -> vk::ImageView {
        if let Some((_, view)) = self
            .attachment_views
            .iter()
            .find(|(at, _)| *at == (level, slice))
        {
            return *view;
        }
        match self.gpu.create_attachment_view(&self.image, level, slice) {
            Ok(view) => {
                self.attachment_views.push(((level, slice), view));
                view
            }
            Err(reason) => {
                gpu::trace(format!("render pass attachment view failed: {reason}"));
                self.image.view
            }
        }
    }

    /// The bytes `-[MTLDevice currentAllocatedSize]` counts this texture as.
    /// Read from the image rather than the descriptor, so that the figure a
    /// deallocation subtracts is the one its creation added.
    pub fn accounted(&self) -> i64 {
        i64::from(gpu::texel_size(self.image.format))
            * i64::from(self.image.width)
            * i64::from(self.image.height)
            * i64::from(self.image.layers)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TextureDescriptor {
    pub texture_type: u64,
    pub pixel_format: u64,
    pub width: u64,
    pub height: u64,
    pub depth: u64,
    pub mipmap_level_count: u64,
    pub sample_count: u64,
    pub array_length: u64,
    pub storage_mode: u64,
    pub usage: u64,
}

impl TextureDescriptor {
    fn fresh() -> Self {
        // `-[MTLTextureDescriptor init]`'s documented defaults.
        Self {
            texture_type: 2, // MTLTextureType2D
            pixel_format: 0,
            width: 1,
            height: 1,
            depth: 1,
            mipmap_level_count: 1,
            sample_count: 1,
            array_length: 1,
            storage_mode: 1, // MTLStorageModeManaged on macOS
            usage: gpu::USAGE_SHADER_READ,
        }
    }

    /// The Vulkan image shape a Metal texture type asks for.
    fn shape(&self) -> (vk::ImageType, vk::ImageViewType, u32, bool) {
        match self.texture_type {
            0 => (vk::ImageType::TYPE_1D, vk::ImageViewType::TYPE_1D, 1, false),
            1 => (
                vk::ImageType::TYPE_1D,
                vk::ImageViewType::TYPE_1D_ARRAY,
                self.array_length.max(1) as u32,
                false,
            ),
            3 | 8 => (
                vk::ImageType::TYPE_2D,
                vk::ImageViewType::TYPE_2D_ARRAY,
                self.array_length.max(1) as u32,
                false,
            ),
            5 => (vk::ImageType::TYPE_2D, vk::ImageViewType::CUBE, 6, true),
            6 => (
                vk::ImageType::TYPE_2D,
                vk::ImageViewType::CUBE_ARRAY,
                6 * self.array_length.max(1) as u32,
                true,
            ),
            7 => (vk::ImageType::TYPE_3D, vk::ImageViewType::TYPE_3D, 1, false),
            // 2, 4 and 9 are the plain two-dimensional cases.
            _ => (vk::ImageType::TYPE_2D, vk::ImageViewType::TYPE_2D, 1, false),
        }
    }
}

/// `MTLRegion`: an origin and a size, both three `NSUInteger`s.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Region {
    pub origin: [u64; 3],
    pub size: [u64; 3],
}

/// `-[MTLTexture dealloc]`, for the same reason as a buffer's.
pub(super) unsafe extern "C" fn texture_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a texture of this crate's.
    let texture = unsafe { object::body::<Texture>(receiver) };
    crate::account(-texture.accounted());
    for (_, view) in std::mem::take(&mut texture.attachment_views) {
        // SAFETY: the last reference is gone, so nothing can attach it again,
        // and a command buffer which did holds a reference until it completes.
        if !leaking() {
            unsafe { texture.gpu.device.destroy_image_view(view, None) };
        }
    }
    let image = std::mem::replace(&mut texture.image, GpuImage::none());
    if !leaking() {
        texture.gpu.destroy_image(image);
    }
    objc::arc::finalize(receiver);
}

pub(crate) fn new_texture(gpu: &'static Gpu, descriptor: TextureDescriptor) -> Id {
    let format = gpu::pixel_format(descriptor.pixel_format);
    if format == vk::Format::UNDEFINED {
        gpu::trace(format!(
            "newTextureWithDescriptor: has no Vulkan form for MTLPixelFormat {}",
            descriptor.pixel_format
        ));
        return std::ptr::null_mut();
    }
    let (image_type, view_type, layers, cube) = descriptor.shape();
    let image = match gpu.create_image(&ImageDescription {
        image_type,
        view_type,
        format,
        width: descriptor.width.max(1) as u32,
        height: descriptor.height.max(1) as u32,
        depth: if image_type == vk::ImageType::TYPE_3D {
            descriptor.depth.max(1) as u32
        } else {
            1
        },
        levels: descriptor.mipmap_level_count.max(1) as u32,
        layers,
        samples: descriptor.sample_count,
        usage: descriptor.usage,
        cube,
    }) {
        Ok(image) => image,
        Err(reason) => {
            gpu::trace(format!("newTextureWithDescriptor: failed: {reason}"));
            return std::ptr::null_mut();
        }
    };
    let texture = object::make(
        c"R3MTLTexture",
        Texture {
            gpu,
            descriptor,
            image,
            attachment_views: Vec::new(),
        },
    );
    if !texture.is_null() {
        // SAFETY: the texture this call just made.
        crate::account(unsafe { object::body::<Texture>(texture) }.accounted());
    }
    texture
}

/// Wrap an existing image as a Metal texture, for presentation, whose images
/// are created and destroyed by the layer rather than by `-newTexture...`.
pub(crate) fn drawable_texture(gpu: &'static Gpu, image: GpuImage, pixel_format: u64) -> Id {
    let descriptor = TextureDescriptor {
        texture_type: 2,
        pixel_format,
        width: u64::from(image.width),
        height: u64::from(image.height),
        depth: 1,
        mipmap_level_count: 1,
        sample_count: 1,
        array_length: 1,
        storage_mode: 2,
        usage: gpu::USAGE_RENDER_TARGET | gpu::USAGE_SHADER_READ,
    };
    object::make(
        c"R3MTLTexture",
        Texture {
            gpu,
            descriptor,
            image,
            attachment_views: Vec::new(),
        },
    )
}

pub(super) fn descriptor_field(descriptor: &TextureDescriptor, sel: Sel) -> u64 {
    // SAFETY: a selector the runtime owns for the life of the process.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"textureType" => descriptor.texture_type,
        b"pixelFormat" => descriptor.pixel_format,
        b"width" => descriptor.width,
        b"height" => descriptor.height,
        b"depth" => descriptor.depth,
        b"mipmapLevelCount" => descriptor.mipmap_level_count,
        b"sampleCount" => descriptor.sample_count,
        b"arrayLength" => descriptor.array_length,
        b"storageMode" => descriptor.storage_mode,
        b"usage" => descriptor.usage,
        _ => unreachable!("only registered descriptor getters reach this method"),
    }
}

pub(super) fn set_descriptor_field(descriptor: &mut TextureDescriptor, sel: Sel, value: u64) {
    // SAFETY: as above.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setTextureType:" => descriptor.texture_type = value,
        b"setPixelFormat:" => descriptor.pixel_format = value,
        b"setWidth:" => descriptor.width = value,
        b"setHeight:" => descriptor.height = value,
        b"setDepth:" => descriptor.depth = value,
        b"setMipmapLevelCount:" => descriptor.mipmap_level_count = value,
        b"setSampleCount:" => descriptor.sample_count = value,
        b"setArrayLength:" => descriptor.array_length = value,
        b"setStorageMode:" => descriptor.storage_mode = value,
        b"setUsage:" => descriptor.usage = value,
        _ => unreachable!("only registered descriptor setters reach this method"),
    }
}

pub(super) unsafe extern "C" fn descriptor_value(receiver: Id, sel: Sel) -> u64 {
    // SAFETY: a texture descriptor of this crate's.
    descriptor_field(unsafe { object::body::<TextureDescriptor>(receiver) }, sel)
}

pub(super) unsafe extern "C" fn set_descriptor_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: as above.
    set_descriptor_field(
        unsafe { object::body::<TextureDescriptor>(receiver) },
        sel,
        value,
    )
}

pub(super) unsafe extern "C" fn texture_value(receiver: Id, sel: Sel) -> u64 {
    // SAFETY: a texture of this crate's.
    descriptor_field(
        &unsafe { object::body::<Texture>(receiver) }.descriptor,
        sel,
    )
}

pub(super) unsafe extern "C" fn descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated texture descriptor.
    *unsafe { object::body::<TextureDescriptor>(receiver) } = TextureDescriptor::fresh();
    receiver
}

/// `-[MTLTexture replaceRegion:mipmapLevel:slice:withBytes:bytesPerRow:bytesPerImage:]`.
///
/// Metal copies immediately, outside any command buffer, so this stages the
/// bytes into a host-visible buffer and runs one copy synchronously.
pub(super) unsafe extern "C" fn replace_region(
    receiver: Id,
    _sel: Sel,
    region: Region,
    level: u64,
    slice: u64,
    bytes: *const u8,
    bytes_per_row: u64,
    bytes_per_image: u64,
) {
    if bytes.is_null() {
        return;
    }
    let [origin_x, origin_y, origin_z] = region.origin;
    let [width, height, depth] = region.size;
    // SAFETY: a texture of this crate's.
    let texture = unsafe { object::body::<Texture>(receiver) };
    let (block_width, block_height) = gpu::block_extent(texture.image.format);
    let texel = u64::from(gpu::texel_size(texture.image.format));
    let rows = height.div_ceil(u64::from(block_height));
    let row_bytes = if bytes_per_row != 0 {
        bytes_per_row
    } else {
        width.div_ceil(u64::from(block_width)) * texel
    };
    let image_bytes = if bytes_per_image != 0 {
        bytes_per_image
    } else {
        row_bytes * rows
    };
    let total = image_bytes * depth.max(1);
    if total == 0 {
        return;
    }
    let staging = match texture.gpu.create_buffer(total) {
        Ok(staging) => staging,
        Err(reason) => return gpu::trace(format!("replaceRegion: staging failed: {reason}")),
    };
    // SAFETY: the guest promised `total` readable bytes, and the staging
    // buffer is mapped over at least that many.
    unsafe { std::ptr::copy_nonoverlapping(bytes, staging.contents, total as usize) };
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length((row_bytes / texel * u64::from(block_width)) as u32)
        .buffer_image_height((rows * u64::from(block_height)) as u32)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: texture.image.aspect,
            mip_level: level as u32,
            base_array_layer: slice as u32,
            layer_count: 1,
        })
        .image_offset(vk::Offset3D {
            x: origin_x as i32,
            y: origin_y as i32,
            z: origin_z as i32,
        })
        .image_extent(vk::Extent3D {
            width: width.max(1) as u32,
            height: height.max(1) as u32,
            depth: depth.max(1) as u32,
        });
    let image = texture.image.image;
    let result = texture.gpu.one_shot(|device, command_buffer| {
        // SAFETY: recording into a command buffer the call owns; the image is
        // in GENERAL layout from creation.
        unsafe {
            device.cmd_copy_buffer_to_image(
                command_buffer,
                staging.buffer,
                image,
                vk::ImageLayout::GENERAL,
                &[copy],
            )
        };
    });
    if let Err(reason) = result {
        gpu::trace(format!("replaceRegion: copy failed: {reason}"));
    }
    // The copy completed before `one_shot` returned.
    texture.gpu.destroy_buffer(staging);
}
