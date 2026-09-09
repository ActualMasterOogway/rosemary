//! `CAMetalLayer`'s drawables and how a rendered frame reaches the window.
//!
//! Metal presents through a drawable the layer vends. Here a drawable is an
//! ordinary Vulkan colour image which the game renders into exactly as it
//! renders into any other texture; `-presentDrawable:` reads it back and hands
//! the pixels to the window.
//!
//! FIXME (ACCURACY): a real implementation owns a `VkSwapchainKHR` on a
//! surface for the window, so the compositor scans out the image the GPU just
//! wrote. SDL only creates a Vulkan surface for a window it was told at
//! creation would be one, and a window's flags are fixed before anything knows
//! whether the guest draws with Metal. Reading the frame back and blitting it
//! through SDL's window surface is a whole frame of copies, and it is correct
//! for every guest.

use ash::vk;
use objc::object::Id;
use objc::sel::Sel;

use crate::gpu::{self, Gpu, ImageDescription};
use crate::object::{self, methods};
use crate::resource::Texture;

pub(crate) struct Drawable {
    pub gpu: &'static Gpu,
    pub texture: Id,
    pub layer: Id,
}

/// The drawable a layer currently vends, remade when the layer's size changes.
///
/// The two `Id`s are Objective-C objects, which the runtime keeps alive for
/// the life of the process; the addresses are what crosses threads, and every
/// use of one is back on the thread which sent the message.
struct Cached {
    layer: usize,
    width: u32,
    height: u32,
    drawable: usize,
}

static CACHE: std::sync::Mutex<Vec<Cached>> = std::sync::Mutex::new(Vec::new());

/// `-[CAMetalLayer nextDrawable]`.
unsafe extern "C" fn next_drawable(receiver: Id, _sel: Sel) -> Id {
    let device = send_id(receiver, "device");
    let device = if device.is_null() {
        // SAFETY: called on the Metal framework's own thread.
        unsafe { crate::MTLCreateSystemDefaultDevice() }
    } else {
        device
    };
    let Some(gpu) = crate::gpu_of_device(device) else {
        return std::ptr::null_mut();
    };
    let size = send_size(receiver, "drawableSize");
    let (mut width, mut height) = (size.0 as u32, size.1 as u32);
    if width == 0 || height == 0 {
        let Some((w, h)) = window_size() else {
            return std::ptr::null_mut();
        };
        width = w;
        height = h;
    }
    let pixel_format = send_u64(receiver, "pixelFormat");
    let pixel_format = if pixel_format == 0 { 80 } else { pixel_format };

    let mut cache = CACHE.lock().expect("drawable cache");
    if let Some(entry) = cache.iter().find(|entry| {
        entry.layer == receiver as usize && entry.width == width && entry.height == height
    }) {
        return entry.drawable as Id;
    }
    cache.retain(|entry| {
        if entry.layer != receiver as usize {
            return true;
        }
        // The layer resized: the old drawable will never be vended again, and
        // nothing is in flight because presentation waits for its readback.
        // SAFETY: a drawable this module made.
        let stale = unsafe { object::body::<Drawable>(entry.drawable as Id) };
        // SAFETY: a drawable's texture is this crate's.
        let texture = unsafe { object::body::<Texture>(stale.texture) };
        stale.gpu.destroy_image(std::mem::replace(
            &mut texture.image,
            crate::gpu::GpuImage::none(),
        ));
        false
    });

    let format = gpu::pixel_format(pixel_format);
    if format == vk::Format::UNDEFINED {
        gpu::trace(format!(
            "CAMetalLayer has no Vulkan form for MTLPixelFormat {pixel_format}"
        ));
        return std::ptr::null_mut();
    }
    let image = match gpu.create_image(&ImageDescription {
        image_type: vk::ImageType::TYPE_2D,
        view_type: vk::ImageViewType::TYPE_2D,
        format,
        width,
        height,
        depth: 1,
        levels: 1,
        layers: 1,
        samples: 1,
        usage: gpu::USAGE_RENDER_TARGET | gpu::USAGE_SHADER_READ,
        cube: false,
    }) {
        Ok(image) => image,
        Err(reason) => {
            gpu::trace(format!("drawable image creation failed: {reason}"));
            return std::ptr::null_mut();
        }
    };
    let texture = crate::resource::drawable_texture(gpu, image, pixel_format);
    if texture.is_null() {
        return std::ptr::null_mut();
    }
    let drawable = object::make(
        c"R3CAMetalDrawable",
        Drawable {
            gpu,
            texture,
            layer: receiver,
        },
    );
    if drawable.is_null() {
        return std::ptr::null_mut();
    }
    gpu::milestone("drawable");
    cache.push(Cached {
        layer: receiver as usize,
        width,
        height,
        drawable: drawable as usize,
    });
    drawable
}

unsafe extern "C" fn drawable_texture(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a drawable of this crate's.
    unsafe { object::body::<Drawable>(receiver) }.texture
}

unsafe extern "C" fn drawable_layer(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    unsafe { object::body::<Drawable>(receiver) }.layer
}

unsafe extern "C" fn drawable_present(receiver: Id, _sel: Sel) {
    present(receiver);
}

/// Read the drawable's image back and blit it into the window.
pub(crate) fn present(drawable: Id) {
    if drawable.is_null() || !object::is_kind(drawable, c"R3CAMetalDrawable") {
        return;
    }
    // SAFETY: a drawable of this crate's, checked above.
    let described = unsafe { object::body::<Drawable>(drawable) };
    // SAFETY: a drawable's texture is this crate's.
    let texture = unsafe { object::body::<Texture>(described.texture) };
    let gpu = described.gpu;
    let width = texture.image.width;
    let height = texture.image.height;
    let bytes = u64::from(width) * u64::from(height) * 4;

    let staging = match gpu.create_buffer(bytes) {
        Ok(staging) => staging,
        Err(reason) => return gpu::trace(format!("presentation staging failed: {reason}")),
    };
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(width)
        .buffer_image_height(height)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        });
    let image = texture.image.image;
    let result = gpu.one_shot(|device, command_buffer| {
        // SAFETY: recording into a command buffer the call owns; the image is
        // in GENERAL layout from creation.
        unsafe {
            device.cmd_copy_image_to_buffer(
                command_buffer,
                image,
                vk::ImageLayout::GENERAL,
                staging.buffer,
                &[copy],
            )
        };
    });
    if let Err(reason) = result {
        gpu::trace(format!("presentation readback failed: {reason}"));
    } else {
        // SAFETY: the readback completed and the staging buffer is mapped over
        // the whole allocation.
        let pixels = unsafe { std::slice::from_raw_parts(staging.contents, bytes as usize) };
        gpu::milestone("presented frame");
        blit_to_window(pixels, width, height, texture.image.format);
    }
    // SAFETY: the copy completed before `one_shot` returned.
    unsafe {
        gpu.device.destroy_buffer(staging.buffer, None);
        gpu.device.free_memory(staging.memory, None);
    }
}

/// SDL's window surface is `SDL_PIXELFORMAT_XRGB8888` on every backend this
/// runs on, which is B, G, R, X in memory. A drawable is usually
/// `MTLPixelFormatBGRA8Unorm`, which is the same order.
fn blit_to_window(pixels: &[u8], width: u32, height: u32, format: vk::Format) {
    let swap_red_and_blue =
        format == vk::Format::R8G8B8A8_UNORM || format == vk::Format::R8G8B8A8_SRGB;
    let windows = appkit::window::sdl_windows();
    let Some(window) = windows.first().copied() else {
        return;
    };
    sdl3::video::present_window(window, |surface| {
        let rows = height.min(surface.height.max(0) as u32);
        let columns = width.min(surface.width.max(0) as u32);
        for row in 0..rows {
            let source = (row * width * 4) as usize;
            let destination = row as usize * surface.pitch as usize;
            for column in 0..columns as usize {
                let from = source + column * 4;
                let Some(texel) = pixels.get(from..from + 4) else {
                    continue;
                };
                let (b, g, r) = if swap_red_and_blue {
                    (texel[2], texel[1], texel[0])
                } else {
                    (texel[0], texel[1], texel[2])
                };
                // SAFETY: SDL reported `pitch` bytes for each of `height`
                // rows, and this write stays inside row `row`.
                unsafe {
                    let out = surface.pixels.add(destination + column * 4);
                    out.write(b);
                    out.add(1).write(g);
                    out.add(2).write(r);
                    out.add(3).write(0xff);
                }
            }
        }
        rows > 0 && columns > 0
    });
}

fn window_size() -> Option<(u32, u32)> {
    let window = appkit::window::sdl_windows().first().copied()?;
    let (width, height) = sdl3::video::window_size_in_pixels(window)?;
    Some((width.max(1) as u32, height.max(1) as u32))
}

// The layer is AppKit's object, so its properties are read by message rather
// than by reaching into a layout this crate does not own.

fn send_id(receiver: Id, selector: &str) -> Id {
    let sel = objc::sel::get(selector);
    // SAFETY: null or an object the runtime allocated.
    let Some(method) = unsafe { objc::object::class_of(receiver) }.and_then(|c| c.method(sel))
    else {
        return std::ptr::null_mut();
    };
    // SAFETY: an `id (id, SEL)` accessor.
    let call: unsafe extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel) }
}

fn send_u64(receiver: Id, selector: &str) -> u64 {
    let sel = objc::sel::get(selector);
    // SAFETY: null or an object the runtime allocated.
    let Some(method) = unsafe { objc::object::class_of(receiver) }.and_then(|c| c.method(sel))
    else {
        return 0;
    };
    // SAFETY: an `NSUInteger (id, SEL)` accessor.
    let call: unsafe extern "C" fn(Id, Sel) -> u64 =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel) }
}

fn send_size(receiver: Id, selector: &str) -> (f64, f64) {
    let sel = objc::sel::get(selector);
    // SAFETY: null or an object the runtime allocated.
    let Some(method) = unsafe { objc::object::class_of(receiver) }.and_then(|c| c.method(sel))
    else {
        return (0.0, 0.0);
    };
    // SAFETY: a `CGSize (id, SEL)` accessor.
    let call: unsafe extern "C" fn(Id, Sel) -> foundation::geometry::NSSize =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    let size = unsafe { call(receiver, sel) };
    (size.width, size.height)
}

pub(crate) fn install() {
    appkit::bootstrap();
    object::register::<Drawable>(c"R3CAMetalDrawable");
    let drawable = objc::class::lookup(c"R3CAMetalDrawable").expect("just registered");
    methods!(drawable, {
        "texture" => drawable_texture, c"@16@0:8";
        "layer" => drawable_layer, c"@16@0:8";
        "present" => drawable_present, c"v16@0:8";
        "drawableID" => drawable_id, c"Q16@0:8";
    });
    let Some(layer) = objc::class::lookup(c"CAMetalLayer") else {
        return;
    };
    methods!(layer, {
        "nextDrawable" => next_drawable, c"@16@0:8";
        "setMaximumDrawableCount:" => crate::no_op, c"v24@0:8Q16";
    });
}

unsafe extern "C" fn drawable_id(_receiver: Id, _sel: Sel) -> u64 {
    0
}
