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

mod present;
#[cfg(test)]
mod tests;

use present::*;

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
