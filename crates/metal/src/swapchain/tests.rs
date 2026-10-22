use super::*;

#[test]
fn mailbox_presentation_is_preferred_when_available() {
    assert_eq!(
        choose_present_mode(&[vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX]),
        vk::PresentModeKHR::MAILBOX
    );
    assert_eq!(
        choose_present_mode(&[vk::PresentModeKHR::IMMEDIATE, vk::PresentModeKHR::FIFO]),
        vk::PresentModeKHR::FIFO
    );
}

/// A window, a swapchain and a frame that reaches it.
///
/// Everything this module does is a call into a driver and a window
/// system, so there is nothing here a unit test can stand in for: the
/// test opens a real window on the host it runs on, presents into it, and
/// is skipped where there is no such host. `ROSEMARY_TEST_VULKAN=1` asks
/// for it, the same switch the adapter test uses.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "Cocoa needs the main thread; use rosemary's Metal guest tests"
)]
fn a_window_is_presented_to_through_its_swapchain() {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        return;
    }
    let Ok(adapters) = crate::vulkan::adapters() else {
        return;
    };
    let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
        return;
    };
    assert!(
        crate::vulkan::surface_api().is_some(),
        "the instance was created without VK_KHR_surface"
    );
    assert!(
        gpu.swapchain_api().is_some(),
        "the device was created without VK_KHR_swapchain"
    );

    // The flags a window of this runtime is actually made with: no
    // graphics API at all, so that this is the call that decides. A test
    // which asked for `VULKAN` here would not be testing the conversion
    // every real window goes through.
    let flags = sdl3::video::flags::HIGH_PIXEL_DENSITY;
    let Some(window) = sdl3::video::create_window(c"rosemary swapchain", 320, 240, flags) else {
        return;
    };
    sdl3::video::show_window(window);
    let (width, height) =
        sdl3::video::window_size_in_pixels(window).expect("a window SDL just made");
    let frame = appkit::view::Frame {
        window,
        x: 0,
        y: 0,
        width: width as u32,
        height: height as u32,
        window_width: width as u32,
        window_height: height as u32,
    };
    assert!(frame.covers_window());
    // The window is converted by the first presentation and not before.
    assert_eq!(
        sdl3::video::window_flags(window) & sdl3::video::flags::VULKAN,
        0,
        "the window was a Vulkan window before anything presented to it"
    );

    let source = gpu
        .create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: frame.width,
            height: frame.height,
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_RENDER_TARGET,
            cube: false,
            components: vk::ComponentMapping::default(),
        })
        .expect("a colour image");

    // More frames than the swapchain has images times the frames in
    // flight, so that every fence, every acquisition semaphore and every
    // per-image presentation semaphore is reused several times over.
    // Reuse is where a synchronization mistake shows up; the first frame
    // never does.
    let handle = || {
        SWAPCHAINS
            .lock()
            .expect("window swapchains")
            .as_ref()
            .and_then(|all| all.get(&(window as usize)))
            .map(|swapchain| swapchain.handle)
    };
    for _ in 0..12 {
        assert_eq!(
            present(gpu, 1, &frame, &source, &[]),
            Ok(true),
            "the window was not presented to through a swapchain"
        );
    }
    // ...and is one afterwards, having been converted in place from a
    // window with no graphics API - the case a real guest's window is
    // in, whatever order it created its `MTLDevice` in.
    assert_ne!(
        sdl3::video::window_flags(window) & sdl3::video::flags::VULKAN,
        0,
        "presenting did not make the window a Vulkan window"
    );
    let steady = handle().expect("a swapchain for the window");
    for _ in 0..12 {
        assert_eq!(present(gpu, 1, &frame, &source, &[]), Ok(true));
    }
    // A window that has not resized keeps the swapchain it has. A
    // surface with a `currentExtent` of its own answers a size that is
    // not the one that was asked for, and comparing against *that* would
    // rebuild the swapchain on every single frame.
    assert_eq!(
        handle(),
        Some(steady),
        "the swapchain was rebuilt without the window resizing"
    );

    // Presentation survives the window changing size, which is a
    // different swapchain and a different set of images.
    sdl3::video::set_window_size(window, 400, 300);
    let (width, height) = sdl3::video::window_size_in_pixels(window).expect("still a window");
    let resized = appkit::view::Frame {
        width: width as u32,
        height: height as u32,
        window_width: width as u32,
        window_height: height as u32,
        ..frame
    };
    assert_eq!(present(gpu, 1, &resized, &source, &[]), Ok(true));

    // Take the swapchain out of the map so that its `Drop` runs the
    // teardown while the window it presented to is still alive.
    SWAPCHAINS
        .lock()
        .expect("window swapchains")
        .as_mut()
        .and_then(|all| all.remove(&(window as usize)));
    gpu.destroy_image(source);
    sdl3::video::destroy_window(window);
}

/// Two layers in one window, the second of them a rectangle inside it,
/// which is the path that needs the composite image.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "Cocoa needs the main thread; use rosemary's Metal guest tests"
)]
fn two_layers_in_a_window_are_composited_into_one_image() {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        return;
    }
    let Ok(adapters) = crate::vulkan::adapters() else {
        return;
    };
    let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
        return;
    };
    let flags = sdl3::video::flags::HIGH_PIXEL_DENSITY;
    let Some(window) = sdl3::video::create_window(c"rosemary compositing", 320, 240, flags) else {
        return;
    };
    sdl3::video::show_window(window);
    let (width, height) =
        sdl3::video::window_size_in_pixels(window).expect("a window SDL just made");
    let (width, height) = (width as u32, height as u32);

    let image = |w: u32, h: u32| {
        gpu.create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: w,
            height: h,
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_RENDER_TARGET,
            cube: false,
            components: vk::ComponentMapping::default(),
        })
        .expect("a colour image")
    };
    let background = image(width, height);
    let overlay = image(width / 2, height / 2);

    let whole = appkit::view::Frame {
        window,
        x: 0,
        y: 0,
        width,
        height,
        window_width: width,
        window_height: height,
    };
    // Deliberately overhanging the bottom right corner, so the clipping
    // in `clamp` is what keeps the blit inside the image.
    let corner = appkit::view::Frame {
        x: (width / 2) as i32,
        y: (height / 2) as i32,
        width,
        height,
        ..whole
    };
    assert!(!corner.covers_window());

    // The first layer alone takes the direct path; the second brings the
    // composite into existence, and both are presented after that.
    assert_eq!(present(gpu, 1, &whole, &background, &[]), Ok(true));
    assert_eq!(present(gpu, 2, &corner, &overlay, &[]), Ok(true));
    assert_eq!(present(gpu, 1, &whole, &background, &[]), Ok(true));
    assert_eq!(present(gpu, 2, &corner, &overlay, &[]), Ok(true));

    let composited = SWAPCHAINS
        .lock()
        .expect("window swapchains")
        .as_ref()
        .and_then(|all| all.get(&(window as usize)))
        .is_some_and(|swapchain| swapchain.composite.is_some());
    assert!(
        composited,
        "a window with two layers presented without a composite image"
    );

    SWAPCHAINS
        .lock()
        .expect("window swapchains")
        .as_mut()
        .and_then(|all| all.remove(&(window as usize)));
    gpu.destroy_image(background);
    gpu.destroy_image(overlay);
    sdl3::video::destroy_window(window);
}

/// A window made with room for rounded corners is presented through the
/// corners pass, on a surface that shows its alpha, whatever size it is.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "Cocoa needs the main thread; use rosemary's Metal guest tests"
)]
fn a_transparent_window_is_presented_through_the_corners_pass() {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        return;
    }
    let Ok(adapters) = crate::vulkan::adapters() else {
        return;
    };
    let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
        return;
    };
    let flags = sdl3::video::flags::HIGH_PIXEL_DENSITY | sdl3::video::flags::TRANSPARENT;
    let Some(window) = sdl3::video::create_window(c"rosemary corners", 320, 240, flags) else {
        return;
    };
    sdl3::video::show_window(window);
    let frame_of = |window| {
        let (width, height) =
            sdl3::video::window_size_in_pixels(window).expect("a window SDL made");
        appkit::view::Frame {
            window,
            x: 0,
            y: 0,
            width: width as u32,
            height: height as u32,
            window_width: width as u32,
            window_height: height as u32,
        }
    };
    let frame = frame_of(window);
    let source = gpu
        .create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: frame.width,
            height: frame.height,
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_RENDER_TARGET,
            cube: false,
            components: vk::ComponentMapping::default(),
        })
        .expect("a colour image");
    // Enough frames to reuse every frame slot's descriptor set.
    for _ in 0..8 {
        assert_eq!(present(gpu, 1, &frame, &source, &[]), Ok(true));
    }
    let sealed = |check: fn(&Swapchain) -> bool| {
        SWAPCHAINS
            .lock()
            .expect("window swapchains")
            .as_ref()
            .and_then(|all| all.get(&(window as usize)))
            .is_some_and(check)
    };
    // A surface that cannot show premultiplied alpha is presented opaque
    // instead, which is not this test's subject.
    if sealed(|swapchain| swapchain.corners.is_some()) {
        assert!(
            sealed(|swapchain| swapchain.composite.is_some()),
            "the corners pass ran without a composite to read"
        );
        sdl3::video::set_window_size(window, 400, 300);
        let resized = frame_of(window);
        for _ in 0..4 {
            assert_eq!(present(gpu, 1, &resized, &source, &[]), Ok(true));
        }
        assert!(sealed(|swapchain| swapchain.corners.is_some()));
    }

    SWAPCHAINS
        .lock()
        .expect("window swapchains")
        .as_mut()
        .and_then(|all| all.remove(&(window as usize)));
    gpu.destroy_image(source);
    sdl3::video::destroy_window(window);
}

/// An OpenGL window is never converted into a Vulkan one.
///
/// SDL will do it, and on Wayland the result kills the process: the
/// `EGLSurface` SDL made with the window stays alive on the
/// `wl_surface`, the Vulkan driver claims the same surface, and the
/// compositer answers `wp_linux_drm_syncobj_manager_v1: surface already
/// exists` and closes the connection. Presentation must refuse such a
/// window and fall back to the readback rather than convert it.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "Cocoa needs the main thread; use rosemary's Metal guest tests"
)]
fn an_opengl_window_is_refused_rather_than_converted() {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        return;
    }
    let Ok(adapters) = crate::vulkan::adapters() else {
        return;
    };
    let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
        return;
    };
    let flags = sdl3::video::flags::OPENGL | sdl3::video::flags::HIGH_PIXEL_DENSITY;
    let Some(window) = sdl3::video::create_window(c"rosemary opengl", 320, 240, flags) else {
        return;
    };
    sdl3::video::show_window(window);
    let (w, h) = sdl3::video::window_size_in_pixels(window).expect("a window");
    let frame = appkit::view::Frame {
        window,
        x: 0,
        y: 0,
        width: w as u32,
        height: h as u32,
        window_width: w as u32,
        window_height: h as u32,
    };
    let source = gpu
        .create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: frame.width,
            height: frame.height,
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_RENDER_TARGET,
            cube: false,
            components: vk::ComponentMapping::default(),
        })
        .expect("a colour image");

    assert_eq!(
        present(gpu, 1, &frame, &source, &[]),
        Ok(false),
        "an OpenGL window was given a Vulkan surface"
    );
    // Still an OpenGL window afterwards: nothing converted it.
    assert_eq!(
        sdl3::video::window_flags(window) & sdl3::video::flags::VULKAN,
        0,
        "the window was converted to a Vulkan window anyway"
    );
    assert_ne!(
        sdl3::video::window_flags(window) & sdl3::video::flags::OPENGL,
        0,
        "the window lost its OpenGL flag"
    );
    // And it is remembered, so a run does not retry it every frame.
    assert_eq!(present(gpu, 1, &frame, &source, &[]), Ok(false));

    REFUSED
        .lock()
        .expect("refused windows")
        .as_mut()
        .and_then(|refused| refused.remove(&(window as usize)));
    gpu.destroy_image(source);
    sdl3::video::destroy_window(window);
}

/// The whole path a guest actually takes: an `NSWindow` made through
/// `-initWithContentRect:`, a view in it, a `CAMetalLayer` on the view,
/// and a drawable presented through the layer.
///
/// The tests above build the SDL window themselves, so they cannot see a
/// mistake in the flags `-initWithContentRect:` chooses - and that is
/// exactly what once left every real window unable to have a swapchain.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "Cocoa needs the main thread; use rosemary's Metal guest tests"
)]
fn a_layer_on_a_guests_own_window_is_presented_through_a_swapchain() {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        return;
    }
    let Ok(adapters) = crate::vulkan::adapters() else {
        return;
    };
    let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
        return;
    };
    crate::bootstrap();

    let rect = foundation::geometry::NSRect {
        origin: foundation::geometry::NSPoint { x: 0.0, y: 0.0 },
        size: foundation::geometry::NSSize {
            width: 320.0,
            height: 240.0,
        },
    };
    let class = objc::class::lookup(c"NSWindow").expect("NSWindow");
    let window = objc::object::create_instance(class, appkit::window::INSTANCE_SIZE);
    // SAFETY: `-initWithContentRect:styleMask:backing:defer:` is
    // `id (id, SEL, NSRect, NSUInteger, NSUInteger, BOOL)` on a fresh
    // window, which is how a guest makes one.
    unsafe {
        let send: unsafe extern "C" fn(
            objc::object::Id,
            objc::sel::Sel,
            foundation::geometry::NSRect,
            usize,
            usize,
            u8,
        ) = std::mem::transmute(objc::msg::entry());
        send(
            window,
            objc::sel::get("initWithContentRect:styleMask:backing:defer:"),
            rect,
            1, // NSWindowStyleMaskTitled
            0,
            0,
        );
    }
    // SAFETY: a window this test just made.
    let sdl = unsafe { appkit::window::sdl_window(window) };
    if sdl.is_null() {
        return; // no display on this host
    }
    assert_eq!(
        sdl3::video::window_flags(sdl) & sdl3::video::flags::OPENGL,
        0,
        "a guest's window claimed OpenGL, so it can never have a swapchain"
    );

    let view_class = objc::class::lookup(c"NSView").expect("NSView");
    let view = objc::object::create_instance(view_class, 0);
    // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`.
    unsafe {
        let send: unsafe extern "C" fn(
            objc::object::Id,
            objc::sel::Sel,
            foundation::geometry::NSRect,
        ) -> objc::object::Id = std::mem::transmute(objc::msg::entry());
        send(view, objc::sel::get("initWithFrame:"), rect);
    }
    // SAFETY: `-setContentView:` is `void (id, SEL, id)`.
    unsafe {
        let send: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel, objc::object::Id) =
            std::mem::transmute(objc::msg::entry());
        send(window, objc::sel::get("setContentView:"), view);
    }

    let layer_class = objc::class::lookup(c"CAMetalLayer").expect("CAMetalLayer");
    // SAFETY: `+layer` is `id (Class, SEL)`.
    let layer = unsafe {
        let send: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel) -> objc::object::Id =
            std::mem::transmute(objc::msg::entry());
        send(
            layer_class as *const objc::class::Class as objc::object::Id,
            objc::sel::get("layer"),
        )
    };
    assert!(!layer.is_null());
    // SAFETY: `-setLayer:` is `void (id, SEL, id)`, and this is what
    // records the layer's view so presentation can find its window.
    unsafe {
        let send: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel, objc::object::Id) =
            std::mem::transmute(objc::msg::entry());
        send(view, objc::sel::get("setLayer:"), layer);
    }

    let frame = crate::present::layer_frame(layer)
        .expect("the layer's view is in a window this runtime made");
    assert_eq!(frame.window, sdl);
    assert!(
        frame.covers_window(),
        "a content view the size of its window did not cover it"
    );

    let source = gpu
        .create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: frame.width.max(1),
            height: frame.height.max(1),
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_RENDER_TARGET,
            cube: false,
            components: vk::ComponentMapping::default(),
        })
        .expect("a colour image");
    assert_eq!(
        present(gpu, layer as usize, &frame, &source, &[]),
        Ok(true),
        "a guest's own window fell back to the readback"
    );

    SWAPCHAINS
        .lock()
        .expect("window swapchains")
        .as_mut()
        .and_then(|all| all.remove(&(sdl as usize)));
    gpu.destroy_image(source);
}

#[test]
fn a_frame_off_the_edge_of_its_window_is_clipped_to_it() {
    let extent = vk::Extent2D {
        width: 100,
        height: 80,
    };
    let frame = |x, y, width, height| appkit::view::Frame {
        window: std::ptr::null_mut(),
        x,
        y,
        width,
        height,
        window_width: extent.width,
        window_height: extent.height,
    };
    // Wholly inside, which is left alone.
    let [from, to] = clamp(&frame(10, 20, 30, 40), extent).expect("inside the window");
    assert_eq!((from.x, from.y, to.x, to.y), (10, 20, 40, 60));
    // Hanging off the far corner, clipped to the image.
    let [from, to] = clamp(&frame(80, 60, 100, 100), extent).expect("partly inside");
    assert_eq!((from.x, from.y, to.x, to.y), (80, 60, 100, 80));
    // Off the near corner, which a negative origin reaches.
    let [from, to] = clamp(&frame(-20, -30, 50, 50), extent).expect("partly inside");
    assert_eq!((from.x, from.y, to.x, to.y), (0, 0, 30, 20));
    // Entirely outside, which is nothing to blit rather than an empty
    // rectangle Vulkan would reject.
    assert!(clamp(&frame(500, 500, 10, 10), extent).is_none());
    assert!(clamp(&frame(-50, 0, 10, 10), extent).is_none());
    // A frame with no area of its own is the same answer.
    assert!(clamp(&frame(10, 10, 0, 40), extent).is_none());
}

/// Actual WSI presentation with a shortened AppKit NSWindow inside a full-
/// display SDL container. A pure clipping unit cannot validate Vulkan image
/// layout changes needed for the new black-band clear and cropped transfer.
#[test]
#[cfg_attr(target_os = "macos", ignore = "requires a Fedora X11 Vulkan window")]
fn a_shrunken_guest_fullscreen_is_presented_through_the_vulkan_swapchain() {
    if std::env::var_os("ROSEMARY_TEST_VULKAN").is_none() {
        return;
    }
    let Ok(adapters) = crate::vulkan::adapters() else {
        return;
    };
    let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
        return;
    };
    crate::bootstrap();
    let pool = objc::arc::pool_push();
    let class = objc::class::lookup(c"NSWindow").unwrap();
    let object = objc::object::create_instance(class, appkit::window::INSTANCE_SIZE);
    let mut frame = foundation::geometry::NSRect {
        origin: foundation::geometry::NSPoint { x: 80.0, y: 90.0 },
        size: foundation::geometry::NSSize {
            width: 400.0,
            height: 300.0,
        },
    };
    let init: unsafe extern "C" fn(
        objc::object::Id,
        objc::sel::Sel,
        foundation::geometry::NSRect,
        usize,
        usize,
        u8,
    ) -> objc::object::Id = unsafe { std::mem::transmute(objc::msg::entry()) };
    assert_eq!(
        unsafe {
            init(
                object,
                objc::sel::get("initWithContentRect:styleMask:backing:defer:"),
                frame,
                9,
                0,
                0,
            )
        },
        object
    );
    let sdl = unsafe { appkit::window::sdl_window(object) };
    assert!(!sdl.is_null());
    sdl3::video::show_window(sdl);
    let toggle: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel, objc::object::Id) =
        unsafe { std::mem::transmute(objc::msg::entry()) };
    unsafe {
        toggle(
            object,
            objc::sel::get("toggleFullScreen:"),
            std::ptr::null_mut(),
        )
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sdl3::video::window_flags(sdl) & sdl3::video::flags::FULLSCREEN == 0 {
        appkit::event::pump(std::ptr::null_mut());
        assert!(
            std::time::Instant::now() < deadline,
            "fullscreen did not settle"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    appkit::event::pump(std::ptr::null_mut());
    let get: unsafe extern "C" fn(
        objc::object::Id,
        objc::sel::Sel,
    ) -> foundation::geometry::NSRect = unsafe { std::mem::transmute(objc::msg::entry_stret()) };
    frame = unsafe { get(object, objc::sel::get("frame")) };
    let host_frame = frame;
    frame.origin.y += 31.0;
    frame.size.height -= 31.0;
    let set: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel, foundation::geometry::NSRect) =
        unsafe { std::mem::transmute(objc::msg::entry()) };
    unsafe { set(object, objc::sel::get("setFrame:"), frame) };
    let bounds = appkit::window::presentation_clip(sdl).unwrap();
    let (w, h) = sdl3::video::window_size_in_pixels(sdl).unwrap();
    assert!(
        bounds.h < h && bounds.h > 0,
        "logical window did not shrink"
    );
    let full = appkit::view::Frame {
        window: sdl,
        x: 0,
        y: 0,
        width: w as u32,
        height: h as u32,
        window_width: w as u32,
        window_height: h as u32,
    };
    let accepted = clamp(
        &full,
        vk::Extent2D {
            width: w as u32,
            height: h as u32,
        },
    )
    .unwrap();
    assert_eq!(accepted[1].y - accepted[0].y, bounds.h);
    let source = gpu
        .create_image(&ImageDescription {
            image_type: vk::ImageType::TYPE_2D,
            view_type: vk::ImageViewType::TYPE_2D,
            format: vk::Format::B8G8R8A8_UNORM,
            width: full.width,
            height: full.height,
            depth: 1,
            levels: 1,
            layers: 1,
            samples: 1,
            usage: gpu::USAGE_RENDER_TARGET,
            cube: false,
            components: vk::ComponentMapping::default(),
        })
        .unwrap();
    // Give the oversized layer an unmistakable red drawable: pixel-level
    // verification must not pass merely because an uninitialised source image
    // happened to be black.
    gpu.one_shot(|device, commands| unsafe {
        device.cmd_clear_color_image(
            commands,
            source.image,
            vk::ImageLayout::GENERAL,
            &vk::ClearColorValue {
                float32: [1.0, 0.0, 0.0, 1.0],
            },
            &[vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            }],
        );
    })
    .expect("red source cleared through Vulkan");
    for _ in 0..5 {
        assert_eq!(present(gpu, 333, &full, &source, &[]), Ok(true));
        // The second layer forces the accumulator/compositor path that GPU
        // DMA-BUF overlays share, rather than only direct layer presentation.
        assert_eq!(present(gpu, 334, &full, &source, &[]), Ok(true));
    }
    assert!(
        SWAPCHAINS
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|all| all.get(&(sdl as usize)))
            .is_some_and(|chain| chain.composite.is_some())
    );
    // Optional pixel oracle for real X11 Vulkan hosts with ImageMagick.
    // The test otherwise still executes Vulkan's recorded clear/blit/present.
    if std::env::var_os("ROSEMARY_TEST_VULKAN_PIXELS").is_some() {
        std::thread::sleep(std::time::Duration::from_millis(220));
        let image =
            std::env::temp_dir().join(format!("rosemary-vulkan-clip-{}.png", std::process::id()));
        let photographed = std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(&image)
            .status()
            .unwrap();
        assert!(
            photographed.success(),
            "X11 screenshot could not be captured"
        );
        let rgb = std::process::Command::new("magick")
            .arg(&image)
            .args(["-depth", "8", "RGB:-"])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&image);
        assert!(rgb.status.success());
        assert_eq!(rgb.stdout.len(), w as usize * h as usize * 3);
        let sample = |x: usize, y: usize| {
            let index = (y * w as usize + x) * 3;
            &rgb.stdout[index..index + 3]
        };
        let x = w as usize / 2;
        assert_eq!(
            sample(x, h as usize - 5),
            &[0, 0, 0],
            "pixels beyond logical frame escaped"
        );
        assert_eq!(
            sample(x, h as usize - 60),
            &[255, 0, 0],
            "valid Metal layer pixel not presented"
        );
    }
    // Restoring the full logical NSWindow must reveal the source pixels in
    // the previously excluded region, not leave a permanent black mask.
    unsafe { set(object, objc::sel::get("setFrame:"), host_frame) };
    let restored = appkit::window::presentation_clip(sdl).unwrap();
    assert_eq!(
        (restored.x, restored.y, restored.w, restored.h),
        (0, 0, w, h)
    );
    for _ in 0..3 {
        assert_eq!(present(gpu, 333, &full, &source, &[]), Ok(true));
        assert_eq!(present(gpu, 334, &full, &source, &[]), Ok(true));
    }
    if std::env::var_os("ROSEMARY_TEST_VULKAN_PIXELS").is_some() {
        std::thread::sleep(std::time::Duration::from_millis(220));
        let image = std::env::temp_dir().join(format!(
            "rosemary-vulkan-restored-{}.png",
            std::process::id()
        ));
        let screenshot = std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(&image)
            .status()
            .unwrap();
        assert!(screenshot.success());
        let rgb = std::process::Command::new("magick")
            .arg(&image)
            .args(["-depth", "8", "RGB:-"])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&image);
        assert!(rgb.status.success());
        let offset = ((h as usize - 5) * w as usize + w as usize / 2) * 3;
        assert_eq!(
            &rgb.stdout[offset..offset + 3],
            &[255, 0, 0],
            "restoring the logical frame did not reveal its lower pixels"
        );
    }
    SWAPCHAINS
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|all| all.remove(&(sdl as usize)));
    gpu.destroy_image(source);
    objc::arc::release(object);
    objc::arc::pool_pop(pool);
}

#[test]
fn fullscreen_logical_clip_preserves_metal_blit_source_coordinates() {
    let extent = vk::Extent2D {
        width: 100,
        height: 80,
    };
    let frame = appkit::view::Frame {
        window: std::ptr::null_mut(),
        x: -10,
        y: -5,
        width: 120,
        height: 90,
        window_width: 100,
        window_height: 80,
    };
    let allowed = sdl3::Rect {
        x: 0,
        y: 0,
        w: 100,
        h: 49,
    };
    let clipped =
        clamp_to(&frame, extent, Some(allowed)).expect("oversized layer intersects logical window");
    assert_eq!(
        (clipped[0].x, clipped[0].y, clipped[1].x, clipped[1].y),
        (0, 0, 100, 49)
    );
    let source = source_of(clipped, &frame, (240, 180));
    assert_eq!(
        (source[0].x, source[0].y, source[1].x, source[1].y),
        (20, 10, 220, 108)
    );
    assert!(
        clamp_to(
            &appkit::view::Frame {
                y: 51,
                height: 25,
                ..frame
            },
            extent,
            Some(allowed)
        )
        .is_none()
    );
    // The clipping region is computed in the destination coordinate space,
    // not by moving or shortening Frame before deriving source coordinates.
    assert_eq!(clamp_to(&frame, extent, None).unwrap()[1].y, 80);
}

/// The window's store is presented around the layers over it: a rectangle a
/// `CAMetalLayer` owns keeps what that layer last presented there, and the
/// rest of the store still reaches the window.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "Cocoa needs the main thread; use rosemary's Metal guest tests"
)]
fn the_store_is_presented_around_a_layers_rectangle() {
    let whole = Region {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let hole = Region {
        x: 20,
        y: 30,
        width: 40,
        height: 10,
    };
    let pieces = subtract(whole, &[hole]);
    // The bands above, below, left and right of the hole, and nothing else.
    assert_eq!(
        pieces,
        vec![
            Region {
                x: 0,
                y: 0,
                width: 100,
                height: 30
            },
            Region {
                x: 0,
                y: 40,
                width: 100,
                height: 60
            },
            Region {
                x: 0,
                y: 30,
                width: 20,
                height: 10
            },
            Region {
                x: 60,
                y: 30,
                width: 40,
                height: 10
            },
        ]
    );
    // Every piece is inside the whole and outside the hole, and together they
    // are the whole less the hole.
    let area: i32 = pieces.iter().map(|p| p.width * p.height).sum();
    assert_eq!(area, 100 * 100 - 40 * 10);

    // A layer covering the window leaves nothing of the store, and a hole
    // outside it takes nothing away.
    assert!(subtract(whole, &[whole]).is_empty());
    assert_eq!(
        subtract(
            whole,
            &[Region {
                x: 200,
                y: 0,
                width: 10,
                height: 10
            }]
        ),
        vec![whole]
    );
}

#[test]
fn a_region_of_a_layer_is_blitted_from_the_matching_part_of_its_drawable() {
    let frame = appkit::view::Frame {
        window: std::ptr::null_mut(),
        x: 10,
        y: 20,
        width: 100,
        height: 50,
        window_width: 400,
        window_height: 300,
    };
    let at = |x, y| vk::Offset3D { x, y, z: 0 };
    let to = |x, y| vk::Offset3D { x, y, z: 1 };
    // A drawable at twice the view's size: the lower half of the view comes
    // from the lower half of the drawable, not from the window's coordinates.
    assert_eq!(
        source_of([at(10, 45), to(110, 70)], &frame, (200, 100)),
        [at(0, 50), to(200, 100)]
    );
    // The window's own store is its size, so a region maps to itself.
    let store = appkit::view::Frame {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
        ..frame
    };
    assert_eq!(
        source_of([at(10, 45), to(110, 70)], &store, (400, 300)),
        [at(10, 45), to(110, 70)]
    );
}
