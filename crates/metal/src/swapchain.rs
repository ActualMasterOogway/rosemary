//! The window's swapchain, and the compositing a view tree needs on top of it.
//!
//! A `CAMetalLayer` is one layer of a window, not the window. AppKit
//! composites every view's layer into one backing store and the window server
//! scans that out; a swapchain is that backing store, and this module is the
//! compositing. A drawable is still an ordinary colour image the guest renders
//! into - nothing about `nextDrawable` changes - and `-presentDrawable:` now
//! blits it into the window's swapchain image on the device instead of reading
//! it back through the host.
//!
//! There is one swapchain per window and it is keyed by the `SDL_Window`,
//! because that is the only identity a surface has. A window with one layer
//! covering it - which is every game that draws a scene and nothing else - has
//! its drawable blitted straight into the acquired image. A window with more
//! than that keeps a composite image of its own: layers blit into the
//! composite, which is what survives between presentations, and the composite
//! is blitted into whichever image the presentation engine hands back. A
//! swapchain image cannot serve as the accumulator, because which one is
//! acquired next is the presentation engine's choice and its contents are
//! undefined when it arrives.
//!
//! FIXME (ACCURACY): compositing here is a blit per layer, so a layer is
//! opaque within its frame and stacking order is the order the guest presents
//! in rather than the view tree's. AppKit blends by the layer's own opacity
//! and composites back to front. A guest which presents two overlapping
//! `CAMetalLayer`s in an order that is not their view order sees them in the
//! wrong order; one which expects a transparent layer to show what is under it
//! sees it opaque. Neither is reachable from a game that draws its scene into
//! one layer, which is what this path is measured on.

use std::collections::HashMap;
use std::sync::Mutex;

use ash::vk;
use vk::Handle;

use crate::gpu::{self, Gpu, GpuImage, ImageDescription};

/// How many frames may be recorded before one has to have finished.
///
/// Two, so that the host can record the next presentation while the device is
/// still blitting the last. A third buys nothing here: presentation is one
/// blit, and the guest's own frame is what the device is actually waiting on.
const FRAMES_IN_FLIGHT: usize = 2;

/// The per-window state presentation goes through.
struct Swapchain {
    gpu: &'static Gpu,
    surface: vk::SurfaceKHR,
    handle: vk::SwapchainKHR,
    format: vk::Format,
    /// The size the swapchain was actually created at, which is the surface's
    /// answer and not necessarily the one that was asked for.
    extent: vk::Extent2D,
    /// The window size that was last asked for, which is what a later frame
    /// is compared against to decide whether the window resized.
    ///
    /// Not [`Self::extent`]: a surface with a `currentExtent` of its own
    /// overrides the request, and comparing against the overridden value
    /// would see a difference on every frame and rebuild the swapchain on
    /// every frame.
    requested: vk::Extent2D,
    images: Vec<vk::Image>,
    /// One presentation semaphore per swapchain image, rather than one per
    /// frame in flight.
    ///
    /// A presentation holds the semaphore it waits on until that image comes
    /// back from `vkAcquireNextImageKHR`, and how many images there are is
    /// the presentation engine's business, not this module's - a swapchain
    /// with four images and two frames in flight would signal a semaphore
    /// the engine was still waiting on. Indexing by the acquired image makes
    /// the acquisition itself the proof that the last presentation of this
    /// image is done with it.
    blitted: Vec<vk::Semaphore>,
    /// Where layers accumulate when the window has more than one. `None`
    /// while a single layer covers the window, which needs no accumulator.
    composite: Option<GpuImage>,
    /// The layers which have presented to this window. Its length is what
    /// decides whether the composite is needed.
    layers: Vec<usize>,
    pool: vk::CommandPool,
    frames: Vec<Frame>,
    frame: usize,
}

/// One frame's worth of recording and synchronization.
struct Frame {
    command_buffer: vk::CommandBuffer,
    /// Signalled when the presentation engine is done with the acquired
    /// image, waited on before anything is written into it. Per frame rather
    /// than per image, because which image an acquisition returns is not
    /// known until it has returned.
    acquired: vk::Semaphore,
    /// Signalled with the submission, so that this frame's command buffer and
    /// its acquisition semaphore are not reused while the device still has
    /// them.
    done: vk::Fence,
}

static SWAPCHAINS: Mutex<Option<HashMap<usize, Swapchain>>> = Mutex::new(None);

/// Whether a swapchain could not be made for this window, so that a run does
/// not retry - and re-report - a surface the host will never give.
static REFUSED: Mutex<Option<HashMap<usize, ()>>> = Mutex::new(None);

/// Blit `source` into `window`'s swapchain image at `frame` and present it.
///
/// `Ok(false)` is "this window cannot be presented to with a swapchain",
/// which is not an error: the caller falls back to reading the frame back and
/// blitting it through SDL's window surface, and every host without
/// `VK_KHR_surface` takes that path.
pub(crate) fn present(
    gpu: &'static Gpu,
    layer: usize,
    frame: &appkit::view::Frame,
    source: &GpuImage,
) -> Result<bool, String> {
    if gpu.swapchain_api().is_none() || crate::vulkan::surface_api().is_none() {
        return Ok(false);
    }
    let window = frame.window as usize;
    if REFUSED
        .lock()
        .expect("refused windows")
        .as_ref()
        .is_some_and(|refused| refused.contains_key(&window))
    {
        return Ok(false);
    }
    let mut all = SWAPCHAINS.lock().expect("window swapchains");
    let all = all.get_or_insert_with(HashMap::new);
    let extent = vk::Extent2D {
        width: frame.window_width.max(1),
        height: frame.window_height.max(1),
    };

    if let std::collections::hash_map::Entry::Vacant(slot) = all.entry(window) {
        match Swapchain::create(gpu, frame.window, extent) {
            Ok(swapchain) => {
                gpu::milestone("window swapchain");
                slot.insert(swapchain);
            }
            Err(reason) => {
                gpu::trace(format!(
                    "no swapchain for this window, presenting through a readback: {reason}"
                ));
                REFUSED
                    .lock()
                    .expect("refused windows")
                    .get_or_insert_with(HashMap::new)
                    .insert(window, ());
                return Ok(false);
            }
        }
    }
    let swapchain = all.get_mut(&window).expect("just created");
    if !swapchain.layers.contains(&layer) {
        swapchain.layers.push(layer);
    }
    // The window resized, or the presentation engine said the last image no
    // longer matches it. Either way every image in flight is finished with
    // before the old swapchain goes.
    if swapchain.requested != extent {
        swapchain.rebuild(extent)?;
    }
    swapchain.present(frame, source)
}

impl Swapchain {
    fn create(
        gpu: &'static Gpu,
        window: sdl3::Window,
        extent: vk::Extent2D,
    ) -> Result<Self, String> {
        // Never an OpenGL window. SDL will convert one in place, but it
        // converts the flags and the loaded libraries and not the backend's
        // own state: on Wayland the `EGLSurface` SDL created with the window
        // stays alive on the `wl_surface`, and when the Vulkan driver claims
        // the same surface the compositor answers
        // `wp_linux_drm_syncobj_manager_v1: surface already exists` and
        // closes the connection.
        //
        // A window with no graphics API - which is how this runtime makes
        // them, exactly so that this is possible - has no `EGLSurface` and
        // is converted safely, so only a guest that has actually asked for
        // an `NSOpenGLContext` ends up here.
        if sdl3::video::window_flags(window) & sdl3::video::flags::OPENGL != 0 {
            return Err("the window belongs to OpenGL, which cannot share it".into());
        }
        let instance = crate::vulkan::instance_handle().ok_or("the Vulkan instance is gone")?;
        let surface = sdl3::video::vulkan::create_surface(window, instance)
            .ok_or_else(|| format!("SDL_Vulkan_CreateSurface failed: {}", sdl3::error()))?;
        let surface = vk::SurfaceKHR::from_raw(surface);
        if !gpu.can_present(surface) {
            sdl3::video::vulkan::destroy_surface(instance, surface.as_raw());
            return Err("the Vulkan queue Metal submits to cannot present to this window".into());
        }
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(gpu.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: a well-formed create info on this live device.
        let pool = unsafe { gpu.device.create_command_pool(&pool_info, None) }
            .map_err(|error| format!("vkCreateCommandPool failed with {error:?}"))?;
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(FRAMES_IN_FLIGHT as u32);
        // SAFETY: the pool was just created by this device.
        let command_buffers = unsafe { gpu.device.allocate_command_buffers(&allocate) }
            .map_err(|error| format!("vkAllocateCommandBuffers failed with {error:?}"))?;
        let mut frames = Vec::with_capacity(FRAMES_IN_FLIGHT);
        for command_buffer in command_buffers {
            // A fence created signalled, because the first use of every frame
            // waits on it before it has ever been submitted.
            let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
            // SAFETY: well-formed create infos on this live device.
            let (acquired, done) = unsafe {
                (
                    gpu.device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None),
                    gpu.device.create_fence(&fence_info, None),
                )
            };
            frames.push(Frame {
                command_buffer,
                acquired: acquired
                    .map_err(|error| format!("vkCreateSemaphore failed with {error:?}"))?,
                done: done.map_err(|error| format!("vkCreateFence failed with {error:?}"))?,
            });
        }
        let mut swapchain = Self {
            gpu,
            surface,
            handle: vk::SwapchainKHR::null(),
            format: vk::Format::UNDEFINED,
            extent,
            requested: extent,
            images: Vec::new(),
            blitted: Vec::new(),
            composite: None,
            layers: Vec::new(),
            pool,
            frames,
            frame: 0,
        };
        swapchain.build(extent)?;
        Ok(swapchain)
    }

    /// Create the `VkSwapchainKHR` itself, replacing whatever is there.
    fn build(&mut self, requested: vk::Extent2D) -> Result<(), String> {
        self.requested = requested;
        let surface_api = crate::vulkan::surface_api().ok_or("VK_KHR_surface is gone")?;
        let api = self.gpu.swapchain_api().ok_or("VK_KHR_swapchain is gone")?;
        // SAFETY: a physical device and a surface of this instance's.
        let capabilities = unsafe {
            surface_api.get_physical_device_surface_capabilities(self.gpu.physical, self.surface)
        }
        .map_err(|error| format!("vkGetPhysicalDeviceSurfaceCapabilitiesKHR failed: {error:?}"))?;
        // SAFETY: as above.
        let formats = unsafe {
            surface_api.get_physical_device_surface_formats(self.gpu.physical, self.surface)
        }
        .map_err(|error| format!("vkGetPhysicalDeviceSurfaceFormatsKHR failed: {error:?}"))?;
        let chosen = choose_format(&formats).ok_or("the surface reports no format")?;
        // SAFETY: as above.
        let present_modes = unsafe {
            surface_api.get_physical_device_surface_present_modes(self.gpu.physical, self.surface)
        }
        .map_err(|error| format!("vkGetPhysicalDeviceSurfacePresentModesKHR failed: {error:?}"))?;
        let present_mode = choose_present_mode(&present_modes);

        // `0xffff_ffff` in either direction is a surface which takes its size
        // from the swapchain rather than the other way round.
        let extent = match capabilities.current_extent.width {
            u32::MAX => vk::Extent2D {
                width: requested.width.clamp(
                    capabilities.min_image_extent.width,
                    capabilities.max_image_extent.width,
                ),
                height: requested.height.clamp(
                    capabilities.min_image_extent.height,
                    capabilities.max_image_extent.height,
                ),
            },
            _ => capabilities.current_extent,
        };
        if extent.width == 0 || extent.height == 0 {
            return Err("the window has no area to present into".into());
        }
        // One more than the minimum, so that the host is not blocked in
        // `vkAcquireNextImageKHR` for as long as the engine holds one.
        let count = match capabilities.max_image_count {
            0 => capabilities.min_image_count + 1,
            most => (capabilities.min_image_count + 1).min(most),
        };
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(count)
            .image_format(chosen.format)
            .image_color_space(chosen.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // The frame arrives as a blit, never as an attachment: the guest
            // rendered into its own drawable long before this.
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(composite_alpha(&capabilities))
            .present_mode(present_mode)
            .clipped(true)
            .old_swapchain(self.handle);
        // SAFETY: a well-formed create info; the old swapchain is retired by
        // this call and destroyed below.
        let handle = unsafe { api.create_swapchain(&info, None) }
            .map_err(|error| format!("vkCreateSwapchainKHR failed with {error:?}"))?;
        if self.handle != vk::SwapchainKHR::null() {
            // SAFETY: retired by the creation above, and every submission
            // touching its images has been waited for by the caller.
            unsafe { api.destroy_swapchain(self.handle, None) };
        }
        // SAFETY: the swapchain was just created by this device.
        let images = unsafe { api.get_swapchain_images(handle) }
            .map_err(|error| format!("vkGetSwapchainImagesKHR failed with {error:?}"))?;
        self.handle = handle;
        self.format = chosen.format;
        self.extent = extent;
        // The old swapchain's images are gone with it, and so is the meaning
        // of a semaphore indexed by one.
        for semaphore in std::mem::take(&mut self.blitted) {
            // SAFETY: the queue was made idle before the rebuild, and the
            // swapchain these were presented through is destroyed above.
            unsafe { self.gpu.device.destroy_semaphore(semaphore, None) };
        }
        self.blitted = images
            .iter()
            .map(|_| {
                // SAFETY: a well-formed create info on this live device.
                unsafe {
                    self.gpu
                        .device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                }
                .map_err(|error| format!("vkCreateSemaphore failed with {error:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.images = images;
        if let Some(composite) = self.composite.take() {
            self.gpu.destroy_image(composite);
        }
        Ok(())
    }

    /// Wait for every frame in flight and rebuild at `extent`.
    fn rebuild(&mut self, extent: vk::Extent2D) -> Result<(), String> {
        self.idle();
        self.build(extent)
    }

    /// Block until nothing this swapchain gave the queue is still running.
    ///
    /// The queue rather than this swapchain's fences: a fence signals when
    /// the blit that signals it is done, but the presentation which waits on
    /// that frame's semaphore is queued behind it and holds the semaphore
    /// after the fence has fired. Destroying or retiring a swapchain while
    /// that is outstanding is a use of a semaphore the queue still owns.
    fn idle(&self) {
        self.gpu.wait_queue_idle();
    }

    /// The image layers accumulate into, made on the first frame that needs
    /// one and kept in `GENERAL` for the same reason every other image in
    /// this crate is.
    fn composite(&mut self) -> Result<vk::Image, String> {
        if self.composite.is_none() {
            let image = self.gpu.create_image(&ImageDescription {
                image_type: vk::ImageType::TYPE_2D,
                view_type: vk::ImageViewType::TYPE_2D,
                format: self.format,
                width: self.extent.width,
                height: self.extent.height,
                depth: 1,
                levels: 1,
                layers: 1,
                samples: 1,
                usage: gpu::USAGE_RENDER_TARGET,
                cube: false,
            })?;
            // A fresh image's contents are undefined, and the frame that
            // brings the composite into existence has only the one layer
            // that asked for it - so everywhere the other layers have not
            // reached yet would be whatever the allocation held.
            let clear = vk::ClearColorValue { float32: [0.0; 4] };
            let range = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            self.gpu.one_shot(|device, command_buffer| {
                // SAFETY: recording into a command buffer the call owns; the
                // image is in GENERAL from creation.
                unsafe {
                    device.cmd_clear_color_image(
                        command_buffer,
                        image.image,
                        vk::ImageLayout::GENERAL,
                        &clear,
                        &[range],
                    )
                };
            })?;
            self.composite = Some(image);
        }
        Ok(self.composite.as_ref().expect("just made").image)
    }

    fn present(&mut self, frame: &appkit::view::Frame, source: &GpuImage) -> Result<bool, String> {
        // One layer covering the window needs no accumulator: what it draws
        // is the whole of what the window shows, so it goes straight into the
        // image the presentation engine hands back.
        let direct = self.layers.len() <= 1 && frame.covers_window();
        let composite = match direct {
            true => None,
            false => Some(self.composite()?),
        };

        let slot = self.frame % self.frames.len().max(1);
        let this = &self.frames[slot];
        let (command_buffer, acquired, done) = (this.command_buffer, this.acquired, this.done);
        // SAFETY: a fence of this device's, signalled at creation or by the
        // submission this slot last made.
        unsafe { self.gpu.device.wait_for_fences(&[done], true, u64::MAX) }
            .map_err(|error| format!("vkWaitForFences failed with {error:?}"))?;

        let Some(index) = self.acquire(acquired)? else {
            // The swapchain was rebuilt underneath the acquisition. The frame
            // the guest drew is gone, which is what a resize costs; the next
            // one lands in the new swapchain.
            return Ok(true);
        };
        let target = self.images[index as usize];
        // Safe to signal: this image has just come back from the
        // presentation engine, so the presentation which last waited on this
        // semaphore is finished.
        let blitted = self.blitted[index as usize];

        let device = &self.gpu.device;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: this slot's fence was waited for above, so nothing is
        // reading the command buffer.
        unsafe {
            device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|error| format!("vkResetCommandBuffer failed with {error:?}"))?;
            device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(|error| format!("vkBeginCommandBuffer failed with {error:?}"))?;
        }

        // SAFETY: recording into a command buffer this call owns; every image
        // named below belongs to this device.
        unsafe {
            let whole = [
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: self.extent.width as i32,
                    y: self.extent.height as i32,
                    z: 1,
                },
            ];
            match composite {
                // The layer's own rectangle of the accumulator, then the
                // whole accumulator into the image being presented.
                Some(composite) => {
                    // A layer wholly off the edge of its window contributes
                    // nothing; what the composite already holds is still
                    // what the window shows, so it is presented unchanged.
                    if let Some(into) = clamp(frame, self.extent) {
                        blit(
                            device,
                            command_buffer,
                            source.image,
                            extent_of(source),
                            composite,
                            vk::ImageLayout::GENERAL,
                            into,
                        );
                    }
                    // The blit above must land before the one below reads it.
                    barrier(
                        device,
                        command_buffer,
                        composite,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::GENERAL,
                        vk::AccessFlags::TRANSFER_WRITE,
                        vk::AccessFlags::TRANSFER_READ,
                    );
                    transition_to_transfer_dst(device, command_buffer, target);
                    blit(
                        device,
                        command_buffer,
                        composite,
                        whole,
                        target,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        whole,
                    );
                }
                None => {
                    transition_to_transfer_dst(device, command_buffer, target);
                    blit(
                        device,
                        command_buffer,
                        source.image,
                        extent_of(source),
                        target,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        whole,
                    );
                }
            }
            transition_to_present(device, command_buffer, target);
            device
                .end_command_buffer(command_buffer)
                .map_err(|error| format!("vkEndCommandBuffer failed with {error:?}"))?;
        }

        self.gpu.submit_synchronized(
            command_buffer,
            done,
            &[(acquired, vk::PipelineStageFlags::TRANSFER)],
            &[blitted],
        )?;
        self.frame = self.frame.wrapping_add(1);

        match self.gpu.present(self.handle, index, blitted) {
            Ok(suboptimal) => {
                if suboptimal {
                    // The image still reached the window; the next frame is
                    // the one that gets a swapchain matching it.
                    self.rebuild(self.requested)?;
                }
                Ok(true)
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.rebuild(self.requested)?;
                Ok(true)
            }
            Err(error) => Err(format!("vkQueuePresentKHR failed with {error:?}")),
        }
    }

    /// The next image to write into, or `None` where the swapchain had to be
    /// rebuilt and this frame is lost with it.
    fn acquire(&mut self, signal: vk::Semaphore) -> Result<Option<u32>, String> {
        let api = self.gpu.swapchain_api().ok_or("VK_KHR_swapchain is gone")?;
        // SAFETY: a swapchain and a semaphore of this device's; no fence, so
        // the semaphore is the only signal.
        let acquired =
            unsafe { api.acquire_next_image(self.handle, u64::MAX, signal, vk::Fence::null()) };
        match acquired {
            Ok((index, _suboptimal)) => Ok(Some(index)),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.rebuild(self.requested)?;
                Ok(None)
            }
            Err(error) => Err(format!("vkAcquireNextImageKHR failed with {error:?}")),
        }
    }
}

/// Nothing in this runtime destroys an `SDL_Window` today - `-[NSWindow
/// close]` orders it out and the window stays in the list - so a swapchain is
/// only dropped if one is taken out of the map. Teardown lives here anyway
/// rather than in a function nothing calls: whichever way a swapchain stops
/// being reachable, the surface, the semaphores and the pool go with it, and
/// the order below is the one Vulkan requires.
impl Drop for Swapchain {
    fn drop(&mut self) {
        self.idle();
        let device = &self.gpu.device;
        // The swapchain first: its images are what the semaphores below were
        // signalled for, and a surface outlives the swapchain made on it.
        if let Some(api) = self.gpu.swapchain_api() {
            // SAFETY: the queue is idle, so nothing is in flight against it.
            unsafe { api.destroy_swapchain(self.handle, None) };
        }
        for frame in &self.frames {
            // SAFETY: objects of this device's, with nothing in flight after
            // the wait above.
            unsafe {
                device.destroy_semaphore(frame.acquired, None);
                device.destroy_fence(frame.done, None);
            }
        }
        for semaphore in std::mem::take(&mut self.blitted) {
            // SAFETY: as above, and the swapchain whose presentations waited
            // on these is destroyed first.
            unsafe { device.destroy_semaphore(semaphore, None) };
        }
        // SAFETY: the pool owns the command buffers, which are freed with it.
        unsafe { device.destroy_command_pool(self.pool, None) };
        if let Some(composite) = self.composite.take() {
            self.gpu.destroy_image(composite);
        }
        if let Some(instance) = crate::vulkan::instance_handle() {
            sdl3::video::vulkan::destroy_surface(instance, self.surface.as_raw());
        }
    }
}

/// A surface format to present in.
///
/// `B8G8R8A8_UNORM` first, because that is `MTLPixelFormatBGRA8Unorm` - what
/// a `CAMetalLayer` defaults to and what a guest almost always renders - so
/// the blit is a copy rather than a conversion. Anything else the surface
/// offers still works: `vkCmdBlitImage` converts, which is the whole reason
/// presentation is a blit and not a copy.
fn choose_format(formats: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    let wanted = |format: vk::Format| {
        formats.iter().copied().find(|candidate| {
            candidate.format == format && candidate.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
    };
    wanted(vk::Format::B8G8R8A8_UNORM)
        .or_else(|| wanted(vk::Format::R8G8B8A8_UNORM))
        .or_else(|| formats.first().copied())
}

/// Prefer low-latency, tear-free presentation where the surface supports it.
///
/// `FIFO` is required by Vulkan, so it is the safe fallback even if a broken
/// implementation returns an empty list.
fn choose_present_mode(modes: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    if modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else {
        vk::PresentModeKHR::FIFO
    }
}

/// `OPAQUE` where the surface offers it, which is what a game's window is.
fn composite_alpha(capabilities: &vk::SurfaceCapabilitiesKHR) -> vk::CompositeAlphaFlagsKHR {
    for candidate in [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::INHERIT,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
    ] {
        if capabilities.supported_composite_alpha.contains(candidate) {
            return candidate;
        }
    }
    vk::CompositeAlphaFlagsKHR::OPAQUE
}

/// A view's frame as two offsets into an image of `extent`, clipped to it.
///
/// A view may hang off the edge of its window - AppKit clips it to the
/// superview - and a blit whose destination leaves the image is invalid
/// rather than clipped, so the clipping happens here. `None` is a frame
/// wholly outside the window, which has no pixels to blit rather than a
/// rectangle to blit into: a scrolled-away view is the ordinary way to get
/// one, and Vulkan has no empty blit region.
fn clamp(frame: &appkit::view::Frame, extent: vk::Extent2D) -> Option<[vk::Offset3D; 2]> {
    let left = frame.x.clamp(0, extent.width as i32);
    let top = frame.y.clamp(0, extent.height as i32);
    let right = frame
        .x
        .saturating_add(frame.width as i32)
        .clamp(0, extent.width as i32);
    let bottom = frame
        .y
        .saturating_add(frame.height as i32)
        .clamp(0, extent.height as i32);
    (right > left && bottom > top).then_some([
        vk::Offset3D {
            x: left,
            y: top,
            z: 0,
        },
        vk::Offset3D {
            x: right,
            y: bottom,
            z: 1,
        },
    ])
}

/// The whole of an image's level zero, as a blit's pair of offsets.
fn extent_of(image: &GpuImage) -> [vk::Offset3D; 2] {
    [
        vk::Offset3D { x: 0, y: 0, z: 0 },
        vk::Offset3D {
            x: image.width.max(1) as i32,
            y: image.height.max(1) as i32,
            z: 1,
        },
    ]
}

/// One colour mip level, which is all any image here has.
fn whole_color() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// # Safety
///
/// Recording into `command_buffer`, with both images belonging to `device`
/// and in the layouts named.
unsafe fn blit(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: vk::Image,
    from: [vk::Offset3D; 2],
    target: vk::Image,
    target_layout: vk::ImageLayout,
    to: [vk::Offset3D; 2],
) {
    let region = vk::ImageBlit::default()
        .src_subresource(whole_color())
        .src_offsets(from)
        .dst_subresource(whole_color())
        .dst_offsets(to);
    // Linear, because a drawable is not always the size of the rectangle it
    // lands in: a layer whose `drawableSize` disagrees with its view is
    // scaled here rather than rejected, which is what Core Animation does
    // with a layer's `contentsGravity` left at its default.
    // SAFETY: the caller's obligation.
    unsafe {
        device.cmd_blit_image(
            command_buffer,
            source,
            vk::ImageLayout::GENERAL,
            target,
            target_layout,
            &[region],
            vk::Filter::LINEAR,
        )
    };
}

/// # Safety
///
/// Recording into `command_buffer`, with `image` belonging to `device`.
unsafe fn barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
    source_access: vk::AccessFlags,
    target_access: vk::AccessFlags,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(from)
        .new_layout(to)
        .src_access_mask(source_access)
        .dst_access_mask(target_access)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    // SAFETY: the caller's obligation.
    unsafe {
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        )
    };
}

/// Put a freshly acquired swapchain image where a blit can write it.
///
/// From `UNDEFINED` rather than from `PRESENT_SRC_KHR`: whatever the image
/// held is the frame before last and every pixel of it is about to be
/// overwritten, and discarding it is what lets a tiler avoid reloading the
/// old contents.
///
/// # Safety
///
/// As [`barrier`].
unsafe fn transition_to_transfer_dst(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
) {
    // SAFETY: the caller's obligation.
    unsafe {
        barrier(
            device,
            command_buffer,
            image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        )
    };
}

/// # Safety
///
/// As [`barrier`].
unsafe fn transition_to_present(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
) {
    // SAFETY: the caller's obligation.
    unsafe {
        barrier(
            device,
            command_buffer,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::PRESENT_SRC_KHR,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
        )
    };
}

#[cfg(test)]
mod tests {
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
    /// is skipped where there is no such host. `ROSETTA3_TEST_VULKAN=1` asks
    /// for it, the same switch the adapter test uses.
    #[test]
    fn a_window_is_presented_to_through_its_swapchain() {
        if std::env::var_os("ROSETTA3_TEST_VULKAN").is_none() {
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
        let Some(window) = sdl3::video::create_window(c"rosetta3 swapchain", 320, 240, flags)
        else {
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
                present(gpu, 1, &frame, &source),
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
            assert_eq!(present(gpu, 1, &frame, &source), Ok(true));
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
        assert_eq!(present(gpu, 1, &resized, &source), Ok(true));

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
    fn two_layers_in_a_window_are_composited_into_one_image() {
        if std::env::var_os("ROSETTA3_TEST_VULKAN").is_none() {
            return;
        }
        let Ok(adapters) = crate::vulkan::adapters() else {
            return;
        };
        let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
            return;
        };
        let flags = sdl3::video::flags::HIGH_PIXEL_DENSITY;
        let Some(window) = sdl3::video::create_window(c"rosetta3 compositing", 320, 240, flags)
        else {
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
        assert_eq!(present(gpu, 1, &whole, &background), Ok(true));
        assert_eq!(present(gpu, 2, &corner, &overlay), Ok(true));
        assert_eq!(present(gpu, 1, &whole, &background), Ok(true));
        assert_eq!(present(gpu, 2, &corner, &overlay), Ok(true));

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

    /// An OpenGL window is never converted into a Vulkan one.
    ///
    /// SDL will do it, and on Wayland the result kills the process: the
    /// `EGLSurface` SDL made with the window stays alive on the
    /// `wl_surface`, the Vulkan driver claims the same surface, and the
    /// compositer answers `wp_linux_drm_syncobj_manager_v1: surface already
    /// exists` and closes the connection. Presentation must refuse such a
    /// window and fall back to the readback rather than convert it.
    #[test]
    fn an_opengl_window_is_refused_rather_than_converted() {
        if std::env::var_os("ROSETTA3_TEST_VULKAN").is_none() {
            return;
        }
        let Ok(adapters) = crate::vulkan::adapters() else {
            return;
        };
        let Some(gpu) = adapters.first().and_then(|adapter| adapter.gpu().ok()) else {
            return;
        };
        let flags = sdl3::video::flags::OPENGL | sdl3::video::flags::HIGH_PIXEL_DENSITY;
        let Some(window) = sdl3::video::create_window(c"rosetta3 opengl", 320, 240, flags) else {
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
            })
            .expect("a colour image");

        assert_eq!(
            present(gpu, 1, &frame, &source),
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
        assert_eq!(present(gpu, 1, &frame, &source), Ok(false));

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
    fn a_layer_on_a_guests_own_window_is_presented_through_a_swapchain() {
        if std::env::var_os("ROSETTA3_TEST_VULKAN").is_none() {
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
            })
            .expect("a colour image");
        assert_eq!(
            present(gpu, layer as usize, &frame, &source),
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
}
