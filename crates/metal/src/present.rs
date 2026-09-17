//! `CAMetalLayer`'s drawables and how a rendered frame reaches the window.
//!
//! Metal presents through a drawable the layer vends. Here a drawable is an
//! ordinary Vulkan colour image which the game renders into exactly as it
//! renders into any other texture, and `-presentDrawable:` puts it on the
//! screen.
//!
//! There are two ways it does that. The first is a real one: the layer's view
//! says which window it is in and where, and the drawable is blitted into
//! that window's `VkSwapchainKHR` on the device, so the compositor scans out
//! what the GPU wrote. [`crate::swapchain`] is that path and is where the
//! interesting parts are.
//!
//! The second is the fallback below, kept for everything the first cannot
//! serve: a host with no `VK_KHR_surface`, a device whose queue cannot
//! present to the window, or a `CAMetalLayer` in no window this runtime made.
//! It reads the frame back into a host buffer and blits it through SDL's
//! window surface, which is a whole frame of copies and is correct for every
//! guest. `ROSETTA3_PRESENT_READBACK` forces it, which is the only way to
//! compare the two on one host.
//!
//! FIXME (ACCURACY): the fallback blits to the window rather than to the
//! layer's rectangle of it, so a guest it catches with more than one layer
//! sees the last one presented, full-window, instead of a composite. It has
//! no swapchain to composite into and reading every layer back to composite
//! on the host would cost more than the path it is standing in for. A guest
//! which reaches it at all has no window this runtime placed, or no surface
//! at all, so it has nowhere better to put the pixels either.
//!
//! Two things about windows are worth recording here.
//!
//! A window is created with no graphics API at all, so that a surface can be
//! made for it whenever one is wanted. SDL will convert an OpenGL window into
//! a Vulkan one in place, but the conversion changes the flags and the loaded
//! libraries and not the backend's own state: on Wayland the `EGLSurface` SDL
//! made with the window survives it, both drivers claim the same
//! `wl_surface`, and the compositor kills the connection with
//! `wp_linux_drm_syncobj_manager_v1: surface already exists`. A window with
//! neither flag has no `EGLSurface` and converts cleanly, which is why
//! `appkit::window` makes them that way and why nothing has to predict
//! whether a guest draws with Metal. See `appkit::window::creation_flags`.
//!
//! A second presentable surface *inside* one window would need a subsurface,
//! and SDL has no such thing on any backend: `SDL_CreatePopupWindow` is a
//! separate OS window everywhere (a child `NSWindow`, a `WS_POPUP` `HWND`, an
//! override-redirect X11 window, an `xdg_popup` the compositor may dismiss).
//! Hence one swapchain per window with the view tree composited into it,
//! which is what AppKit does anyway.

use ash::vk;
use objc::object::Id;
use objc::sel::Sel;

use crate::constants::*;
use crate::gpu::{self, Gpu, GpuImage, ImageDescription};
use crate::object::{self, methods};
use crate::resource::Texture;

pub(crate) struct Drawable {
    pub gpu: &'static Gpu,
    pub texture: Id,
    pub layer: Id,
    /// The buffer the frame is read back into, kept for the life of the
    /// drawable. Making one per frame is a `vkCreateBuffer` and a range of the
    /// heap for every frame, and the drawable's size never changes: a layer
    /// which resizes vends a new drawable.
    readback: Option<crate::gpu::GpuBuffer>,
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
        // A layer with no drawable size of its own is the size of the view
        // holding it, which is what Core Animation gives it; a layer no view
        // holds falls back to the window, as it did before layers could be
        // placed at all.
        let Some((w, h)) = layer_frame(receiver)
            .map(|frame| (frame.width, frame.height))
            .or_else(window_size)
        else {
            return std::ptr::null_mut();
        };
        width = w;
        height = h;
    }
    let pixel_format = send_u64(receiver, "pixelFormat");
    let pixel_format = if pixel_format == MTL_PIXEL_FORMAT_INVALID {
        MTL_PIXEL_FORMAT_BGRA8_UNORM
    } else {
        pixel_format
    };

    let mut cache = CACHE.lock().expect("drawable cache");
    if let Some(entry) = cache.iter().find(|entry| {
        entry.layer == receiver as usize && entry.width == width && entry.height == height
    }) {
        // Metal vends an autoreleased drawable; the cache's own reference is
        // not the caller's to spend.
        return objc::arc::autorelease(objc::arc::retain(entry.drawable as Id));
    }
    cache.retain(|entry| {
        if entry.layer != receiver as usize {
            return true;
        }
        // The layer resized: the old drawable will never be vended again, so
        // the cache gives up its references. It does not destroy anything: a
        // command buffer which was handed this drawable retains it precisely
        // because its work can still be running, and the presentation that
        // follows still reads the image. The image and the readback buffer go
        // in `drawable_dealloc`, once the last holder has let go.
        let stale = entry.drawable as Id;
        // SAFETY: a drawable this module made, whose texture is this crate's.
        let texture = unsafe { object::body::<Drawable>(stale) }.texture;
        objc::arc::release(texture);
        objc::arc::release(stale);
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
            readback: None,
        },
    );
    if drawable.is_null() {
        return std::ptr::null_mut();
    }
    // The cache's own references. A drawable is vended again and again from
    // here, and a guest which released the one it was handed - having never
    // retained it - would otherwise leave this entry pointing at freed memory.
    objc::arc::retain(drawable);
    objc::arc::retain(texture);
    gpu::milestone("drawable");
    cache.push(Cached {
        layer: receiver as usize,
        width,
        height,
        drawable: drawable as usize,
    });
    // The reference `object::make` returned, given to the caller as Metal gives
    // it: autoreleased, on top of the cache's own.
    objc::arc::autorelease(drawable)
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

/// `-[CAMetalDrawable dealloc]`.
///
/// A drawable outlives the cache entry that vended it whenever a command
/// buffer still holds it, so this - and not the resize which evicted it - is
/// where its image and readback buffer go. Everything which could have the
/// device touching them has released its reference by now: a committed
/// command buffer releases the drawable when its submission retires, and the
/// readback path waits for its own copy before it returns.
unsafe extern "C" fn drawable_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a drawable of this crate's.
    let drawable = unsafe { object::body::<Drawable>(receiver) };
    if let Some(readback) = drawable.readback.take() {
        drawable.gpu.destroy_buffer(readback);
    }
    // The texture owns the drawable's image, and destroys it in its own
    // `dealloc`.
    objc::arc::release(std::mem::replace(
        &mut drawable.texture,
        std::ptr::null_mut(),
    ));
    // SAFETY: the body was established by `object::make` and is not read again.
    unsafe { std::ptr::drop_in_place(drawable as *mut Drawable) };
    objc::arc::finalize(receiver);
}

/// Read the drawable's image back and blit it into the window.
pub(crate) fn present(drawable: Id) {
    crate::stats::timed(crate::stats::Counter::PresentMicros, || {
        present_inner(drawable)
    });
    crate::stats::tick();
}

fn present_inner(drawable: Id) {
    if drawable.is_null() || !object::is_kind(drawable, c"R3CAMetalDrawable") {
        return;
    }
    // SAFETY: a drawable of this crate's, checked above.
    let described = unsafe { object::body::<Drawable>(drawable) };
    // SAFETY: a drawable's texture is this crate's.
    let texture = unsafe { object::body::<Texture>(described.texture) };
    let gpu = described.gpu;
    // SAFETY: a `CAMetalLayer` this runtime made.
    let view = unsafe { appkit::layer::view_of(described.layer) };
    if appkit::view::is_hidden_or_has_hidden_ancestor(view) {
        return;
    }
    let frame = layer_frame(described.layer);

    // The swapchain is the whole of presentation where the window has one:
    // the frame the guest drew stays on the device and the compositor scans
    // out what the blit put there. Everything below this is the fallback.
    if present_through_swapchain(described, texture, frame.as_ref()) {
        gpu::milestone("presented frame");
        crate::stats::count(crate::stats::Counter::Frames);
        return;
    }

    // The layer's own window where it has one, and the first window
    // otherwise - which is a guess, but the only one available for a layer
    // this runtime cannot place. It is right for every guest with one
    // window, which is every guest the readback path has ever run.
    let window = frame
        .as_ref()
        .map(|frame| frame.window)
        .or_else(|| appkit::window::sdl_windows().first().copied());

    let width = texture.image.width;
    let height = texture.image.height;
    let bytes = u64::from(width) * u64::from(height) * 4;

    if described.readback.is_none() {
        match gpu.create_buffer(bytes) {
            Ok(staging) => described.readback = Some(staging),
            Err(reason) => return gpu::trace(format!("presentation staging failed: {reason}")),
        }
    }
    let staging = described.readback.as_ref().expect("just made");
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
    let result = crate::stats::timed(crate::stats::Counter::ReadbackMicros, || {
        gpu.one_shot(|device, command_buffer| {
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
        })
    });
    if let Err(reason) = result {
        gpu::trace(format!("presentation readback failed: {reason}"));
    } else {
        // SAFETY: the readback completed and the staging buffer is mapped over
        // the whole allocation.
        let pixels = unsafe { std::slice::from_raw_parts(staging.contents, bytes as usize) };
        gpu::milestone("presented frame");
        crate::stats::count(crate::stats::Counter::Frames);
        dump_frame(pixels, width, height, texture.image.format);
        crate::stats::timed(crate::stats::Counter::BlitMicros, || {
            blit_to_window(pixels, width, height, texture.image.format, window)
        });
    }
}

/// Present the drawable through its window's swapchain, if it has one.
///
/// The layer says which view holds it and the view says where in which
/// window it is; without both there is no surface to present to, and the
/// caller reads the frame back instead. `ROSETTA3_PRESENT_READBACK` forces
/// that older path, which is the only way to compare the two on one host and
/// the escape hatch for a driver whose swapchain misbehaves.
fn present_through_swapchain(
    drawable: &Drawable,
    texture: &Texture,
    frame: Option<&appkit::view::Frame>,
) -> bool {
    if std::env::var_os("ROSETTA3_PRESENT_READBACK").is_some() {
        return false;
    }
    let Some(frame) = frame else {
        gpu::trace_once(
            "a CAMetalLayer is in no window this runtime made, so its drawables are presented \
             through a readback",
        );
        return false;
    };
    match crate::swapchain::present(
        drawable.gpu,
        drawable.layer as usize,
        frame,
        &texture.image,
        &[],
    ) {
        Ok(presented) => presented,
        Err(reason) => {
            gpu::trace(format!("swapchain presentation failed: {reason}"));
            false
        }
    }
}

/// Where a layer sits in its window, or `None` for a layer no view holds.
pub(crate) fn layer_frame(layer: Id) -> Option<appkit::view::Frame> {
    // SAFETY: a `CAMetalLayer` this runtime made, whose view outlives it.
    let view = unsafe { appkit::layer::view_of(layer) };
    // SAFETY: null or a view of AppKit's.
    unsafe { appkit::view::presentation_frame(view) }
}

/// Write a presented frame to `ROSETTA3_FRAME_OUT` as a binary PPM.
///
/// What reached the window is the one thing a log cannot say, and a headless
/// or remote session has no other way to see it. `ROSETTA3_FRAME_INTERVAL`
/// says how many frames to skip between files; the default keeps a run's
/// output to a few files a second rather than one per frame.
fn dump_frame(pixels: &[u8], width: u32, height: u32, format: vk::Format) {
    use std::sync::atomic::{AtomicU64, Ordering};

    let Some(directory) = std::env::var_os("ROSETTA3_FRAME_OUT") else {
        return;
    };
    static PRESENTED: AtomicU64 = AtomicU64::new(0);
    let frame = PRESENTED.fetch_add(1, Ordering::Relaxed);
    let interval = std::env::var("ROSETTA3_FRAME_INTERVAL")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|interval| *interval > 0)
        .unwrap_or(30);
    if !frame.is_multiple_of(interval) {
        return;
    }
    let directory = std::path::PathBuf::from(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let swap_red_and_blue =
        !(format == vk::Format::R8G8B8A8_UNORM || format == vk::Format::R8G8B8A8_SRGB);
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.reserve(width as usize * height as usize * 3);
    for texel in pixels.as_chunks::<4>().0 {
        match swap_red_and_blue {
            true => out.extend_from_slice(&[texel[2], texel[1], texel[0]]),
            false => out.extend_from_slice(&[texel[0], texel[1], texel[2]]),
        }
    }
    let _ = std::fs::write(directory.join(format!("frame-{frame:06}.ppm")), out);
}

/// SDL's window surface is `SDL_PIXELFORMAT_XRGB8888` on every backend this
/// runs on, which is B, G, R, X in memory. A drawable is usually
/// `MTLPixelFormatBGRA8Unorm`, which is the same order, so the common case is
/// a row copy: the byte the window ignores is the one the drawable calls
/// alpha. A drawable in R, G, B, A order needs its two ends exchanged, which
/// is done a pixel at a time but a whole pixel at a time.
fn blit_to_window(
    pixels: &[u8],
    width: u32,
    height: u32,
    format: vk::Format,
    window: Option<sdl3::Window>,
) {
    let swap_red_and_blue =
        format == vk::Format::R8G8B8A8_UNORM || format == vk::Format::R8G8B8A8_SRGB;
    let Some(window) = window else {
        return;
    };
    sdl3::video::present_window(window, |surface| {
        let rows = height.min(surface.height.max(0) as u32);
        let columns = width.min(surface.width.max(0) as u32) as usize;
        for row in 0..rows {
            let source = (row * width * 4) as usize;
            let Some(from) = pixels.get(source..source + columns * 4) else {
                continue;
            };
            // SAFETY: SDL reported `pitch` bytes for each of `height` rows, and
            // every write below stays inside row `row`.
            let to = unsafe { surface.pixels.add(row as usize * surface.pitch as usize) };
            if !swap_red_and_blue {
                // SAFETY: `columns * 4` bytes, from a slice of that length into
                // a row which is at least `width * 4` bytes wide.
                unsafe { std::ptr::copy_nonoverlapping(from.as_ptr(), to, columns * 4) };
                continue;
            }
            for (column, texel) in from.as_chunks::<4>().0.iter().enumerate() {
                // SAFETY: `column` is below `columns`, inside this row.
                unsafe {
                    let out = to.add(column * 4);
                    out.write(texel[2]);
                    out.add(1).write(texel[1]);
                    out.add(2).write(texel[0]);
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
    appkit::backing::set_presenter(present_store);
    appkit::window::add_destroy_hook(forget_window);
    object::register::<Drawable>(c"R3CAMetalDrawable");
    let drawable = objc::class::lookup(c"R3CAMetalDrawable").expect("just registered");
    methods!(drawable, {
        "dealloc" => drawable_dealloc, c"v16@0:8";
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

/// A window's CPU-drawn contents on the device: see [`present_store`].
struct StoreImage {
    gpu: &'static Gpu,
    image: GpuImage,
    staging: gpu::GpuBuffer,
}

// SAFETY: Vulkan handles and a permanent mapping, used under `STORE_IMAGES`.
unsafe impl Send for StoreImage {}

/// Keyed by the SDL window's address.
static STORE_IMAGES: std::sync::Mutex<Vec<(usize, StoreImage)>> = std::sync::Mutex::new(Vec::new());

/// Images replaced by a resize, with how many more presentations to keep them
/// for: a presentation already submitted may still be blitting one.
static RETIRED: std::sync::Mutex<Vec<(StoreImage, usize)>> = std::sync::Mutex::new(Vec::new());

/// More than the swapchain's frames in flight, so that every submission which
/// could name a retired image has finished before it is destroyed.
const FRAMES_RETIRED: usize = 4;

/// Present what AppKit's views drew on the CPU for `window`.
///
/// The store is uploaded and composited into the window's swapchain as one
/// more layer covering the window, so that it and any `CAMetalLayer`s share
/// the one surface the window has. A layer presenting afterwards lands over
/// it, which is AppKit's order for a layer-backed subview.
/// Drop what this module and the swapchains hold for a window being destroyed.
fn forget_window(window: sdl3::Window) {
    let key = window as usize;
    let mut images = STORE_IMAGES.lock().expect("store images");
    if let Some(at) = images.iter().position(|(existing, _)| *existing == key) {
        let (_, image) = images.remove(at);
        RETIRED
            .lock()
            .expect("retired images")
            .push((image, FRAMES_RETIRED));
    }
    drop(images);
    crate::swapchain::forget_window(window);
}

pub(crate) fn present_store(
    window: sdl3::Window,
    store: &appkit::backing::Store,
    holes: &[appkit::backing::Hole],
) -> bool {
    if std::env::var_os("ROSETTA3_PRESENT_READBACK").is_some() {
        return false;
    }
    // SAFETY: the Metal framework's exported constructor, which has no
    // preconditions.
    let device = unsafe { crate::MTLCreateSystemDefaultDevice() };
    let Some(gpu) = crate::gpu_of_device(device) else {
        return false;
    };
    let mut images = STORE_IMAGES.lock().expect("store images");
    let at = images.iter().position(|(key, _)| *key == window as usize);
    let stale = at.is_some_and(|at| {
        let image = &images[at].1.image;
        (image.width, image.height) != (store.width, store.height)
    });
    if stale {
        let (_, old) = images.remove(at.expect("checked above"));
        RETIRED
            .lock()
            .expect("retired images")
            .push((old, FRAMES_RETIRED));
    }
    if at.is_none() || stale {
        let image = match gpu.create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: store.width,
            height: store.height,
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_SHADER_READ,
            cube: false,
        }) {
            Ok(image) => image,
            Err(reason) => {
                gpu::trace(format!("window store image creation failed: {reason}"));
                return false;
            }
        };
        let staging = match gpu.create_buffer(store.pixels.len() as u64) {
            Ok(staging) => staging,
            Err(reason) => {
                gpu.destroy_image(image);
                gpu::trace(format!("window store staging failed: {reason}"));
                return false;
            }
        };
        images.push((
            window as usize,
            StoreImage {
                gpu,
                image,
                staging,
            },
        ));
    }
    let entry = &images
        .iter()
        .find(|(key, _)| *key == window as usize)
        .expect("made above")
        .1;
    // SAFETY: the staging buffer is mapped over the store's whole length.
    unsafe {
        std::ptr::copy_nonoverlapping(
            store.pixels.as_ptr(),
            entry.staging.contents,
            store.pixels.len(),
        )
    };
    let copy = vk::BufferImageCopy::default()
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width: store.width,
            height: store.height,
            depth: 1,
        });
    let (buffer, image) = (entry.staging.buffer, entry.image.image);
    let uploaded = entry.gpu.one_shot(|device, command_buffer| {
        // SAFETY: recording into a command buffer the call owns; the image is
        // in GENERAL layout from creation.
        unsafe {
            device.cmd_copy_buffer_to_image(
                command_buffer,
                buffer,
                image,
                vk::ImageLayout::GENERAL,
                &[copy],
            )
        };
    });
    if let Err(reason) = uploaded {
        gpu::trace(format!("window store upload failed: {reason}"));
        return false;
    }
    let frame = appkit::view::Frame {
        window,
        x: 0,
        y: 0,
        width: store.width,
        height: store.height,
        window_width: store.width,
        window_height: store.height,
    };
    let holes: Vec<(usize, crate::swapchain::Region)> = holes
        .iter()
        .map(|hole| {
            (
                hole.layer,
                crate::swapchain::Region {
                    x: hole.rect.x.clamp(0, i64::from(i32::MAX)) as i32,
                    y: hole.rect.y.clamp(0, i64::from(i32::MAX)) as i32,
                    width: hole.rect.width.clamp(0, i64::from(i32::MAX)) as i32,
                    height: hole.rect.height.clamp(0, i64::from(i32::MAX)) as i32,
                },
            )
        })
        .collect();
    let presented =
        match crate::swapchain::present(gpu, window as usize, &frame, &entry.image, &holes) {
            Ok(presented) => presented,
            Err(reason) => {
                gpu::trace(format!("window store presentation failed: {reason}"));
                false
            }
        };
    drop(images);
    let expired: Vec<StoreImage> = {
        let mut retired = RETIRED.lock().expect("retired images");
        for (_, remaining) in retired.iter_mut() {
            *remaining = remaining.saturating_sub(1);
        }
        let (expired, kept) = std::mem::take(&mut *retired)
            .into_iter()
            .partition::<Vec<_>, _>(|(_, remaining)| *remaining == 0);
        *retired = kept;
        expired.into_iter().map(|(image, _)| image).collect()
    };
    for old in expired {
        old.gpu.destroy_buffer(old.staging);
        old.gpu.destroy_image(old.image);
    }
    if presented {
        gpu::milestone("window store");
    }
    presented
}
