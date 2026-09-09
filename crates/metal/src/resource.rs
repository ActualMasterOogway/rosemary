//! Metal's resources: buffers, textures, samplers and depth/stencil state.
//!
//! Each is a real Vulkan object. Metal's descriptors are plain value types
//! which a game fills in and hands to the device, so they are stored as-is and
//! read back at creation time.

use objc::object::Id;
use objc::sel::Sel;

use crate::gpu::{self, Gpu, GpuBuffer, GpuImage, ImageDescription};
use crate::object::{self, methods};
use ash::vk;

pub(crate) struct Buffer {
    pub gpu: &'static Gpu,
    pub inner: GpuBuffer,
    pub options: u64,
}

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

/// A `MTLDepthStencilDescriptor` and the state a pipeline reads from it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DepthStencilState {
    pub depth_compare: u64,
    pub depth_write: bool,
    pub front: StencilState,
    pub back: StencilState,
    pub stencil_enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StencilState {
    pub compare: u64,
    pub fail: u64,
    pub depth_fail: u64,
    pub pass: u64,
    pub read_mask: u32,
    pub write_mask: u32,
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

/// `MTLVertexDescriptor`: 31 buffer layouts and 31 attributes, each a small
/// bag of integers the render pipeline reads at creation time.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VertexLayout {
    pub stride: u64,
    pub step_function: u64,
    pub step_rate: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VertexAttribute {
    pub format: u64,
    pub offset: u64,
    pub buffer_index: u64,
}

pub(crate) const VERTEX_SLOTS: usize = 31;

pub(crate) struct VertexDescriptor {
    pub layouts: [VertexLayout; VERTEX_SLOTS],
    pub attributes: [VertexAttribute; VERTEX_SLOTS],
    /// The `-layouts` and `-attributes` collections, made on first use because
    /// a game holds the element objects and mutates them in place.
    pub layout_objects: Id,
    pub attribute_objects: Id,
}

/// One element of a `MTLVertexDescriptor` collection: it points back into the
/// descriptor it belongs to, because Metal's setters mutate the descriptor
/// through the element a game holds.
#[derive(Clone, Copy)]
pub(crate) struct VertexElement {
    pub owner: Id,
    pub index: usize,
    pub layout: bool,
}

pub(crate) struct IndexedCollection {
    pub elements: Vec<Id>,
}

// ---------------------------------------------------------------- buffers

/// `ROSETTA3_TRACE_BUFFERS=1`: every `MTLBuffer` made and destroyed, with its
/// length, its `MTLResourceOptions` and how many are live.
///
/// The aggregate counters say a run is allocating gigabytes a second; only the
/// sizes say what it is allocating, and a game which asks for one size five
/// hundred times over is a different problem from one which asks for five
/// hundred sizes once each.
fn tracing_buffers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ROSETTA3_TRACE_BUFFERS").is_some())
}

pub(crate) fn new_buffer(gpu: &'static Gpu, length: usize, options: u64) -> Id {
    if length == 0 {
        return std::ptr::null_mut();
    }
    crate::stats::peak(crate::stats::Counter::BufferPeakBytes, length as u64);
    if tracing_buffers() {
        eprintln!(
            "rosetta3: Metal: newBuffer {length} options {options:#x} live {}",
            crate::live_buffers(1)
        );
    }
    let inner = match crate::stats::timed(crate::stats::Counter::BufferMicros, || {
        gpu.create_buffer(length as u64)
    }) {
        Ok(inner) => inner,
        Err(reason) => {
            gpu::trace(format!("newBufferWithLength: failed: {reason}"));
            return std::ptr::null_mut();
        }
    };
    crate::account(length as i64);
    crate::stats::count(crate::stats::Counter::BuffersCreated);
    crate::stats::add(crate::stats::Counter::BufferBytes, length as u64);
    object::make(
        c"R3MTLBuffer",
        Buffer {
            gpu,
            inner,
            options,
        },
    )
}

/// The mapping behind an `MTLBuffer` this crate made.
///
/// # Safety
///
/// `buffer` is a buffer of this crate's.
pub(crate) unsafe fn contents_of(buffer: Id) -> *mut u8 {
    // SAFETY: the caller's guarantee.
    unsafe { object::body::<Buffer>(buffer) }.inner.contents
}

unsafe extern "C" fn buffer_contents(receiver: Id, _sel: Sel) -> *mut u8 {
    // SAFETY: a buffer of this crate's.
    unsafe { object::body::<Buffer>(receiver) }.inner.contents
}

unsafe extern "C" fn buffer_length(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: a buffer of this crate's.
    unsafe { object::body::<Buffer>(receiver) }.inner.length as usize
}

unsafe extern "C" fn buffer_storage_mode(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: a buffer of this crate's.
    (unsafe { object::body::<Buffer>(receiver) }.options >> 4) & 0xf
}

/// `-[MTLBuffer dealloc]`.
///
/// Without this every buffer a guest makes is a `VkBuffer` and a range of a
/// heap block held for the life of the process. A game which streams its world
/// in makes gigabytes of them in a second, so the leak is not a slow one: it
/// exhausts the adapter's memory during the first minute of play.
///
/// The last reference is gone, so nothing can bind it again, and a command
/// buffer which did bind it holds a reference of its own until the work it was
/// recorded into has completed.
/// TEMPORARY: `ROSETTA3_METAL_LEAK=1` keeps every device resource alive, to tell
/// a fault caused by freeing one too early from every other kind.
fn leaking() -> bool {
    static LEAK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LEAK.get_or_init(|| std::env::var_os("ROSETTA3_METAL_LEAK").is_some())
}

unsafe extern "C" fn buffer_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a buffer of this crate's.
    let buffer = unsafe { object::body::<Buffer>(receiver) };
    if tracing_buffers() {
        eprintln!(
            "rosetta3: Metal: freeBuffer {} live {}",
            buffer.inner.length,
            crate::live_buffers(-1)
        );
    }
    crate::account(-(buffer.inner.length as i64));
    let inner = std::mem::replace(&mut buffer.inner, GpuBuffer::none());
    if !leaking() {
        buffer.gpu.destroy_buffer(inner);
    }
    objc::arc::finalize(receiver);
}

/// `-[MTLTexture dealloc]`, for the same reason as a buffer's.
unsafe extern "C" fn texture_dealloc(receiver: Id, _sel: Sel) {
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

// --------------------------------------------------------------- textures

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

fn descriptor_field(descriptor: &TextureDescriptor, sel: Sel) -> u64 {
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

fn set_descriptor_field(descriptor: &mut TextureDescriptor, sel: Sel, value: u64) {
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

unsafe extern "C" fn descriptor_value(receiver: Id, sel: Sel) -> u64 {
    // SAFETY: a texture descriptor of this crate's.
    descriptor_field(unsafe { object::body::<TextureDescriptor>(receiver) }, sel)
}

unsafe extern "C" fn set_descriptor_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: as above.
    set_descriptor_field(
        unsafe { object::body::<TextureDescriptor>(receiver) },
        sel,
        value,
    )
}

unsafe extern "C" fn texture_value(receiver: Id, sel: Sel) -> u64 {
    // SAFETY: a texture of this crate's.
    descriptor_field(
        &unsafe { object::body::<Texture>(receiver) }.descriptor,
        sel,
    )
}

unsafe extern "C" fn descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated texture descriptor.
    *unsafe { object::body::<TextureDescriptor>(receiver) } = TextureDescriptor::fresh();
    receiver
}

/// `-[MTLTexture replaceRegion:mipmapLevel:slice:withBytes:bytesPerRow:bytesPerImage:]`.
///
/// Metal copies immediately, outside any command buffer, so this stages the
/// bytes into a host-visible buffer and runs one copy synchronously.
unsafe extern "C" fn replace_region(
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

// --------------------------------------------------------------- samplers

pub(crate) fn filter(metal: u64) -> vk::Filter {
    match metal {
        1 => vk::Filter::LINEAR,
        _ => vk::Filter::NEAREST,
    }
}

fn mip_mode(metal: u64) -> vk::SamplerMipmapMode {
    match metal {
        2 => vk::SamplerMipmapMode::LINEAR,
        _ => vk::SamplerMipmapMode::NEAREST,
    }
}

fn address_mode(metal: u64) -> vk::SamplerAddressMode {
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

unsafe extern "C" fn sampler_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated sampler descriptor.
    *unsafe { object::body::<SamplerDescriptor>(receiver) } = SamplerDescriptor::default();
    receiver
}

unsafe extern "C" fn set_sampler_value(receiver: Id, sel: Sel, value: u64) {
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

unsafe extern "C" fn set_sampler_clamp(receiver: Id, sel: Sel, value: f32) {
    // SAFETY: a sampler descriptor of this crate's.
    let descriptor = unsafe { object::body::<SamplerDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setLodMinClamp:" => descriptor.lod_min_clamp = value,
        b"setLodMaxClamp:" => descriptor.lod_max_clamp = value,
        _ => unreachable!("only registered clamp setters reach this method"),
    }
}

// --------------------------------------------------------- depth/stencil

unsafe extern "C" fn depth_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated depth/stencil descriptor.
    *unsafe { object::body::<DepthStencilState>(receiver) } = DepthStencilState::default();
    receiver
}

unsafe extern "C" fn set_depth_compare(receiver: Id, _sel: Sel, value: u64) {
    // SAFETY: a depth/stencil descriptor of this crate's.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_compare = value;
}

unsafe extern "C" fn depth_compare(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: a depth/stencil descriptor of this crate's.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_compare
}

unsafe extern "C" fn set_depth_write(receiver: Id, _sel: Sel, value: u64) {
    // SAFETY: as above.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_write = value & 0xff != 0;
}

unsafe extern "C" fn depth_write(receiver: Id, _sel: Sel) -> bool {
    // SAFETY: a depth/stencil descriptor of this crate's.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_write
}

/// `-setFrontFaceStencil:` and `-setBackFaceStencil:` copy the descriptor's
/// current values: Metal's stencil descriptor is a value type.
unsafe extern "C" fn set_face_stencil(receiver: Id, sel: Sel, face: Id) {
    if face.is_null() {
        return;
    }
    // SAFETY: a stencil descriptor of this crate's.
    let state = *unsafe { object::body::<StencilState>(face) };
    // SAFETY: a depth/stencil descriptor of this crate's.
    let descriptor = unsafe { object::body::<DepthStencilState>(receiver) };
    descriptor.stencil_enabled = true;
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setFrontFaceStencil:" => descriptor.front = state,
        _ => descriptor.back = state,
    }
}

/// The face properties are values, not references into the depth descriptor.
/// Return an autoreleased copy just as Metal does, so a caller can read and
/// modify it before assigning it back with the corresponding setter.
unsafe extern "C" fn face_stencil(receiver: Id, sel: Sel) -> Id {
    // SAFETY: a depth/stencil descriptor of this crate's.
    let descriptor = unsafe { object::body::<DepthStencilState>(receiver) };
    if !descriptor.stencil_enabled {
        return std::ptr::null_mut();
    }
    // SAFETY: a selector the runtime owns.
    let state = match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"frontFaceStencil" => descriptor.front,
        b"backFaceStencil" => descriptor.back,
        _ => unreachable!("only registered stencil getters reach this method"),
    };
    objc::arc::autorelease(object::make(c"MTLStencilDescriptor", state))
}

unsafe extern "C" fn stencil_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated stencil descriptor.
    *unsafe { object::body::<StencilState>(receiver) } = StencilState::default();
    receiver
}

unsafe extern "C" fn set_stencil_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a stencil descriptor of this crate's.
    let state = unsafe { object::body::<StencilState>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setStencilCompareFunction:" => state.compare = value,
        b"setStencilFailureOperation:" => state.fail = value,
        b"setDepthFailureOperation:" => state.depth_fail = value,
        b"setDepthStencilPassOperation:" => state.pass = value,
        b"setReadMask:" => state.read_mask = value as u32,
        b"setWriteMask:" => state.write_mask = value as u32,
        _ => unreachable!("only registered stencil setters reach this method"),
    }
}

pub(crate) fn new_depth_stencil_state(state: DepthStencilState) -> Id {
    object::make(c"R3MTLDepthStencilState", state)
}

#[cfg(test)]
mod depth_stencil_tests {
    use super::*;

    #[test]
    fn reads_back_a_face_copy() {
        crate::bootstrap();
        let descriptor = object::make(c"MTLDepthStencilDescriptor", DepthStencilState::default());
        let stencil = object::make(c"MTLStencilDescriptor", StencilState::default());
        // SAFETY: these are methods on the two freshly made Metal descriptors.
        unsafe {
            set_stencil_value(stencil, objc::sel::get("setReadMask:"), 0x0f);
            set_face_stencil(descriptor, objc::sel::get("setBackFaceStencil:"), stencil);
        }

        let pool = objc::arc::pool_push();
        // SAFETY: this is the corresponding registered getter.
        let copy = unsafe { face_stencil(descriptor, objc::sel::get("backFaceStencil")) };
        assert!(!copy.is_null());
        // SAFETY: the getter made this a stencil descriptor with the copied state.
        assert_eq!(
            unsafe { object::body::<StencilState>(copy) }.read_mask,
            0x0f
        );
        objc::arc::pool_pop(pool);
    }
}

// ------------------------------------------------------- vertex descriptor

unsafe extern "C" fn vertex_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated vertex descriptor.
    let descriptor = unsafe { object::body::<VertexDescriptor>(receiver) };
    descriptor.layouts = [VertexLayout::default(); VERTEX_SLOTS];
    descriptor.attributes = [VertexAttribute::default(); VERTEX_SLOTS];
    descriptor.layout_objects = std::ptr::null_mut();
    descriptor.attribute_objects = std::ptr::null_mut();
    receiver
}

fn vertex_collection(receiver: Id, layout: bool) -> Id {
    // SAFETY: a vertex descriptor of this crate's.
    let descriptor = unsafe { object::body::<VertexDescriptor>(receiver) };
    let existing = if layout {
        descriptor.layout_objects
    } else {
        descriptor.attribute_objects
    };
    if !existing.is_null() {
        return existing;
    }
    let elements = (0..VERTEX_SLOTS)
        .map(|index| {
            object::make(
                c"R3MTLVertexElement",
                VertexElement {
                    owner: receiver,
                    index,
                    layout,
                },
            )
        })
        .collect::<Vec<_>>();
    let collection = object::make(c"R3MTLIndexedCollection", IndexedCollection { elements });
    // SAFETY: as above.
    let descriptor = unsafe { object::body::<VertexDescriptor>(receiver) };
    if layout {
        descriptor.layout_objects = collection;
    } else {
        descriptor.attribute_objects = collection;
    }
    collection
}

unsafe extern "C" fn vertex_layouts(receiver: Id, _sel: Sel) -> Id {
    vertex_collection(receiver, true)
}

unsafe extern "C" fn vertex_attributes(receiver: Id, _sel: Sel) -> Id {
    vertex_collection(receiver, false)
}

unsafe extern "C" fn indexed_element(receiver: Id, _sel: Sel, index: usize) -> Id {
    // SAFETY: an indexed collection of this crate's.
    unsafe { object::body::<IndexedCollection>(receiver) }
        .elements
        .get(index)
        .copied()
        .unwrap_or(std::ptr::null_mut())
}

unsafe extern "C" fn set_vertex_element_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a vertex element of this crate's.
    let element = *unsafe { object::body::<VertexElement>(receiver) };
    // SAFETY: the element holds the descriptor which made it.
    let descriptor = unsafe { object::body::<VertexDescriptor>(element.owner) };
    // SAFETY: a selector the runtime owns.
    let name = unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes();
    if element.layout {
        let layout = &mut descriptor.layouts[element.index];
        match name {
            b"setStride:" => layout.stride = value,
            b"setStepFunction:" => layout.step_function = value,
            b"setStepRate:" => layout.step_rate = value,
            _ => (),
        }
    } else {
        let attribute = &mut descriptor.attributes[element.index];
        match name {
            b"setFormat:" => attribute.format = value,
            b"setOffset:" => attribute.offset = value,
            b"setBufferIndex:" => attribute.buffer_index = value,
            _ => (),
        }
    }
}

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
    });
    methods!(collection, {
        "objectAtIndexedSubscript:" => indexed_element, c"@24@0:8Q16";
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
