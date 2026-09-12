//! `MTLRenderPassDescriptor`, and beginning the Vulkan render pass it describes.

use super::*;

/// One `MTLRenderPassAttachmentDescriptor`.
#[derive(Clone, Copy)]
pub(crate) struct AttachmentDescriptor {
    pub texture: Id,
    pub level: u64,
    pub slice: u64,
    pub load_action: u64,
    pub store_action: u64,
    pub clear_color: [f64; 4],
    pub clear_depth: f64,
    pub clear_stencil: u32,
    pub resolve_texture: Id,
    pub resolve_level: u64,
    pub resolve_slice: u64,
}

/// A `MTLStoreActionMultisampleResolve` from an attachment which has one
/// sample. Vulkan resolves only a multisampled attachment, so the destination
/// is filled with a copy instead, which is what a one-sample resolve is.
#[derive(Clone, Copy)]
pub(crate) struct PendingCopy {
    pub source: vk::Image,
    pub source_level: u32,
    pub source_slice: u32,
    pub destination: vk::Image,
    pub destination_level: u32,
    pub destination_slice: u32,
    pub width: u32,
    pub height: u32,
    pub aspect: vk::ImageAspectFlags,
}

pub(crate) const RENDER_TARGETS: usize = 8;

pub(crate) struct RenderPassDescriptor {
    pub colors: [AttachmentDescriptor; RENDER_TARGETS],
    pub depth: AttachmentDescriptor,
    pub stencil: AttachmentDescriptor,
    pub color_objects: Id,
    pub depth_object: Id,
    pub stencil_object: Id,
    /// FIXME (ACCURACY): kept so that a guest reading the property back sees
    /// what it set, and never written to. Occlusion queries are not
    /// implemented, so a guest which reads the counts finds whatever the
    /// buffer already held.
    pub visibility_result_buffer: Id,
}

/// `ROSETTA3_TRACE_PASSES=1`: each distinct render pass shape, once.
///
/// A frame's device time is spent in its passes, and a log which says only how
/// many there were cannot say which one to look at. The size, the sample count
/// and the load and store actions are what decide the cost of a pass before any
/// shader runs.
pub(super) fn trace_pass(key: &RenderPassKey, width: u32, height: u32) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    #[static_init::dynamic]
    static ENABLED: bool = std::env::var_os("ROSETTA3_TRACE_PASSES").is_some();
    if !*ENABLED {
        return;
    }
    let line = format!(
        "render pass {width}x{height} samples {:?} colors {:?} resolves {:?} depth {:?}",
        key.samples, key.colors, key.color_resolves, key.depth
    );
    let mut seen = SEEN.lock().expect("render pass trace");
    if !seen.contains(&line) {
        seen.push(line.clone());
        gpu::trace(line);
    }
}

/// One dimension of a mip level, which is the level-zero one halved that many
/// times and never zero.
pub(super) fn level_extent(base: u32, level: u32) -> u32 {
    (base >> level.min(31)).max(1)
}

// FIXME (ACCURACY): DONT_CARE semantics probably follow the same rule for loads as they
// do for stores that caused us lots of problems. TODO: should we just remove DONT_CARE here?
pub(super) fn load_op(metal: u64) -> vk::AttachmentLoadOp {
    match metal {
        1 => vk::AttachmentLoadOp::LOAD,
        2 => vk::AttachmentLoadOp::CLEAR,
        _ => vk::AttachmentLoadOp::DONT_CARE,
    }
}

/// Whether an `MTLStoreAction` asks for a multisample resolve:
/// `MultisampleResolve` is 2 and `StoreAndMultisampleResolve` is 3.
pub(super) fn resolves(metal: u64) -> bool {
    metal == 2 || metal == 3
}

/// What becomes of the attachment the pass rendered into: always `STORE`,
/// whatever `MTLStoreAction` the pass names.
///
/// Metal's `DontCare` is permission to discard, not an instruction to. On the
/// immediate-mode GPUs Metal runs on an attachment is plain memory whose
/// contents outlive the pass, so a game writes `DontCare` on every pass which
/// does not need the attachment kept and then reads that same texture later,
/// and it works. Vulkan's `DONT_CARE` is not a hint: on the NVIDIA driver this
/// runs on it invalidates the surface's compression metadata, and what the
/// texture then reads back as is a sparse, dithered version of whatever it held
/// before.
///
/// Storing costs the write-back of an attachment nothing reads again. It was
/// not measurable: at a matched 9,070 draws a second that scene spends 388 ms
/// of device time a second of wall clock discarding and 389 ms storing.
pub(super) fn store_op() -> vk::AttachmentStoreOp {
    vk::AttachmentStoreOp::STORE
}

/// Where one colour attachment is resolved to, when the store action asks for
/// a resolve and the destination can carry it.
///
/// Vulkan resolves only into an attachment of the same format, and Metal names
/// a mip level and array slice of the destination rather than the whole
/// texture. A destination this cannot express is reported and dropped: the
/// multisampled attachment is then stored instead, which is wrong but visible,
/// where a resolve nothing performs leaves the destination black.
pub(super) struct ResolveTarget {
    format: vk::Format,
    view: vk::ImageView,
    image: vk::Image,
    level: u32,
    slice: u32,
}

pub(super) fn resolve_attachment(
    color: &AttachmentDescriptor,
    format: vk::Format,
) -> Option<ResolveTarget> {
    if !resolves(color.store_action) || color.resolve_texture.is_null() {
        return None;
    }
    // SAFETY: only a texture of this crate's is attached.
    let texture = unsafe { object::body::<Texture>(color.resolve_texture) };
    if texture.image.format != format {
        gpu::trace_once(format!(
            "render pass resolves {format:?} into {:?}, which Vulkan cannot do",
            texture.image.format
        ));
        return None;
    }
    let level = color.resolve_level as u32;
    let slice = color.resolve_slice as u32;
    Some(ResolveTarget {
        format: texture.image.format,
        view: texture.attachment_view(level, slice),
        image: texture.image.image,
        level,
        slice,
    })
}

/// Begin the Vulkan render pass one `MTLRenderPassDescriptor` describes.
/// What one `MTLRenderPassDescriptor`'s attachments amount to in Vulkan.
struct Attachments {
    key: RenderPassKey,
    /// Attachment views, resolve views appended after the depth one.
    views: Vec<vk::ImageView>,
    resolve_views: Vec<vk::ImageView>,
    /// Which colour attachments resolve, as a bitmask over render targets.
    resolve_mask: u32,
    /// Copies Metal's `MTLStoreActionStoreAndMultisampleResolve` implies.
    copies: Vec<PendingCopy>,
    capture_targets: Vec<crate::capture::Target>,
    clears: Vec<vk::ClearValue>,
    /// The smallest attachment extent, which is the pass's own.
    width: u32,
    height: u32,
    /// The textures the pass reads or writes, retained for its lifetime.
    textures: Vec<Id>,
}

/// Gather what a descriptor's attachments amount to, without touching the
/// command buffer: the caller decides what to do when there are none.
fn gather_attachments(descriptor: &RenderPassDescriptor) -> Attachments {
    let mut key = RenderPassKey {
        colors: Vec::new(),
        color_resolves: Vec::new(),
        depth: None,
        stencil: None,
        samples: vk::SampleCountFlags::TYPE_1,
    };
    let mut views = Vec::new();
    let mut resolve_views = Vec::new();
    let mut resolve_mask = 0u32;
    let mut copies: Vec<PendingCopy> = Vec::new();
    let mut capture_targets: Vec<crate::capture::Target> = Vec::new();
    let mut clears = Vec::new();
    let mut width = u32::MAX;
    let mut height = u32::MAX;
    let mut textures = Vec::new();
    // The targets only matter to `ROSETTA3_PASS_OUT`, and naming each one
    // formats a string.
    let capturing = crate::capture::enabled();

    for color in &descriptor.colors {
        if color.texture.is_null() {
            continue;
        }
        // SAFETY: only a texture of this crate's is attached.
        let texture = unsafe { object::body::<Texture>(color.texture) };
        let format = texture.image.format;
        key.samples = texture.image.samples;
        let level = color.level as u32;
        views.push(texture.attachment_view(level, color.slice as u32));
        let (base_width, base_height) = (texture.image.width, texture.image.height);
        let source_image = texture.image.image;
        let multisampled = texture.image.samples != vk::SampleCountFlags::TYPE_1;
        let resolve = resolve_attachment(color, format);
        // A subpass resolve is the answer only for a multisampled attachment.
        // Where the attachment has one sample the same store action means a
        // plain copy, which is recorded once the pass has ended.
        let subpass_resolve = resolve.as_ref().filter(|_| multisampled);
        key.colors
            .push((format, load_op(color.load_action), store_op()));
        key.color_resolves
            .push(subpass_resolve.map(|target| target.format));
        if let Some(target) = &resolve {
            textures.push(color.resolve_texture);
            if multisampled {
                resolve_mask |= 1 << (key.colors.len() - 1);
                resolve_views.push(target.view);
            } else {
                copies.push(PendingCopy {
                    source: source_image,
                    source_level: level,
                    source_slice: color.slice as u32,
                    destination: target.image,
                    destination_level: target.level,
                    destination_slice: target.slice,
                    width: level_extent(base_width, level),
                    height: level_extent(base_height, level),
                    aspect: vk::ImageAspectFlags::COLOR,
                });
            }
        }
        if capturing {
            capture_targets.push(crate::capture::Target {
                image: source_image,
                aspect: vk::ImageAspectFlags::COLOR,
                level,
                slice: color.slice as u32,
                width: level_extent(base_width, level),
                height: level_extent(base_height, level),
                format,
                samples: texture.image.samples,
                role: format!("c{}", key.colors.len() - 1),
            });
        }
        if let Some(target) = resolve.as_ref().filter(|_| capturing) {
            capture_targets.push(crate::capture::Target {
                image: target.image,
                aspect: vk::ImageAspectFlags::COLOR,
                level: target.level,
                slice: target.slice,
                width: level_extent(base_width, level),
                height: level_extent(base_height, level),
                format: target.format,
                samples: vk::SampleCountFlags::TYPE_1,
                role: format!("r{}", key.colors.len() - 1),
            });
        }
        clears.push(vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [
                    color.clear_color[0] as f32,
                    color.clear_color[1] as f32,
                    color.clear_color[2] as f32,
                    color.clear_color[3] as f32,
                ],
            },
        });
        width = width.min(level_extent(base_width, level));
        height = height.min(level_extent(base_height, level));
        textures.push(color.texture);
    }
    let depth_attachment = if !descriptor.depth.texture.is_null() {
        Some(&descriptor.depth)
    } else if !descriptor.stencil.texture.is_null() {
        Some(&descriptor.stencil)
    } else {
        None
    };
    if let Some(attachment) = depth_attachment {
        // SAFETY: only a texture of this crate's is attached.
        let texture = unsafe { object::body::<Texture>(attachment.texture) };
        key.depth = Some((
            texture.image.format,
            load_op(attachment.load_action),
            store_op(),
        ));
        if !descriptor.stencil.texture.is_null() {
            key.stencil = Some((load_op(descriptor.stencil.load_action), store_op()));
        }
        // FIXME (ACCURACY): a depth or stencil resolve is not performed. Vulkan
        // spells it as a subpass extension read through `vkCreateRenderPass2`,
        // which nothing here uses yet; the resolve destination is left as it
        // was rather than silently holding one sample of the attachment.
        if resolves(attachment.store_action) && !attachment.resolve_texture.is_null() {
            gpu::trace_once("render pass asks for a depth resolve, which is not implemented");
        }
        key.samples = texture.image.samples;
        let level = attachment.level as u32;
        views.push(texture.attachment_view(level, attachment.slice as u32));
        clears.push(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: descriptor.depth.clear_depth as f32,
                stencil: descriptor.stencil.clear_stencil,
            },
        });
        width = width.min(level_extent(texture.image.width, level));
        height = height.min(level_extent(texture.image.height, level));
        if capturing {
            capture_targets.push(crate::capture::Target {
                image: texture.image.image,
                aspect: vk::ImageAspectFlags::DEPTH,
                level,
                slice: attachment.slice as u32,
                width: level_extent(texture.image.width, level),
                height: level_extent(texture.image.height, level),
                format: texture.image.format,
                samples: texture.image.samples,
                role: "d".to_owned(),
            });
        }
        textures.push(attachment.texture);
    }
    Attachments {
        key,
        views,
        resolve_views,
        resolve_mask,
        copies,
        capture_targets,
        clears,
        width,
        height,
        textures,
    }
}

pub(crate) fn begin_render_pass(encoder_object: Id, descriptor: &RenderPassDescriptor) {
    // SAFETY: an encoder this crate just made.
    let encoder = unsafe { object::body::<Encoder>(encoder_object) };
    let command = encoder.command();
    let gpu = command.gpu;

    let Attachments {
        key,
        mut views,
        resolve_views,
        resolve_mask,
        copies,
        capture_targets,
        clears,
        width,
        height,
        textures,
    } = gather_attachments(descriptor);
    if views.is_empty() {
        gpu::trace("renderCommandEncoderWithDescriptor: has no attachments");
        return;
    }
    // The render pass declares the resolve attachments after the depth one.
    views.extend(resolve_views);
    let width = width.max(1);
    let height = height.max(1);

    let pass = match gpu.render_pass(&key) {
        Ok(pass) => pass,
        Err(reason) => return gpu::trace(format!("render pass creation failed: {reason}")),
    };
    let framebuffer = match gpu.create_framebuffer(pass, &views, width, height, 1) {
        Ok(framebuffer) => framebuffer,
        Err(reason) => return gpu::trace(format!("framebuffer creation failed: {reason}")),
    };
    for texture in textures {
        command.retain(texture);
    }
    command.framebuffers.push(framebuffer);
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width, height },
    };
    trace_pass(&key, width, height);
    encoder.capture_pass = crate::capture::claim();
    if let Some(pass) = encoder.capture_pass {
        for target in &capture_targets {
            if let Some(capture) =
                crate::capture::record(gpu, command.command_buffer, pass, "pre", target)
            {
                command.captures.push(capture);
            }
        }
    }
    let begin = vk::RenderPassBeginInfo::default()
        .render_pass(pass)
        .framebuffer(framebuffer)
        .render_area(area)
        .clear_values(&clears);
    // SAFETY: recording into this command buffer.
    unsafe {
        gpu.device.cmd_begin_render_pass(
            command.command_buffer,
            &begin,
            vk::SubpassContents::INLINE,
        );
        // Metal's default viewport and scissor cover the attachments, and the
        // viewport is flipped for the same reason `-setViewport:` flips.
        gpu.device.cmd_set_viewport(
            command.command_buffer,
            0,
            &[vk::Viewport {
                x: 0.0,
                y: height as f32,
                width: width as f32,
                height: -(height as f32),
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        gpu.device
            .cmd_set_scissor(command.command_buffer, 0, &[area]);
        gpu.device
            .cmd_set_depth_bias(command.command_buffer, 0.0, 0.0, 0.0);
        gpu.device
            .cmd_set_blend_constants(command.command_buffer, &[0.0; 4]);
        gpu.device.cmd_set_stencil_reference(
            command.command_buffer,
            vk::StencilFaceFlags::FRONT_AND_BACK,
            0,
        );
    };
    gpu::milestone("render pass");
    encoder.in_render_pass = true;
    encoder.resolve_mask = resolve_mask;
    encoder.pending_copies = copies;
    encoder.capture_targets = capture_targets;
}

pub(super) unsafe extern "C" fn render_pass_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's. `-init` reaches a fresh
    // one, whose body is zero, and a reused one, which still owns what it holds.
    unsafe { release_pass_contents(receiver) };
    // SAFETY: as above.
    *unsafe { object::body::<RenderPassDescriptor>(receiver) } = RenderPassDescriptor::default();
    receiver
}

/// Give up every reference a render pass descriptor holds, leaving it null.
///
/// # Safety
///
/// `receiver` must be a `MTLRenderPassDescriptor` of this crate's.
pub(super) unsafe fn release_pass_contents(receiver: Id) {
    // SAFETY: the caller's obligation.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    let mut attachments: Vec<&mut AttachmentDescriptor> = descriptor.colors.iter_mut().collect();
    attachments.push(&mut descriptor.depth);
    attachments.push(&mut descriptor.stencil);
    for attachment in attachments {
        objc::arc::release(std::mem::replace(
            &mut attachment.texture,
            std::ptr::null_mut(),
        ));
        objc::arc::release(std::mem::replace(
            &mut attachment.resolve_texture,
            std::ptr::null_mut(),
        ));
    }
    for slot in [
        &mut descriptor.color_objects,
        &mut descriptor.depth_object,
        &mut descriptor.stencil_object,
        &mut descriptor.visibility_result_buffer,
    ] {
        objc::arc::release(std::mem::replace(slot, std::ptr::null_mut()));
    }
}

/// `-[MTLRenderPassDescriptor dealloc]`.
pub(super) unsafe extern "C" fn render_pass_descriptor_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a render pass descriptor of this crate's.
    unsafe { release_pass_contents(receiver) };
    objc::arc::finalize(receiver);
}

pub(super) unsafe extern "C" fn pass_visibility_result_buffer(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's.
    unsafe { object::body::<RenderPassDescriptor>(receiver) }.visibility_result_buffer
}

pub(super) unsafe extern "C" fn set_pass_visibility_result_buffer(
    receiver: Id,
    _sel: Sel,
    buffer: Id,
) {
    // SAFETY: as above.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    let previous = std::mem::replace(
        &mut descriptor.visibility_result_buffer,
        objc::arc::retain(buffer),
    );
    objc::arc::release(previous);
}

/// `+[MTLRenderPassDescriptor renderPassDescriptor]`.
pub(super) unsafe extern "C" fn new_render_pass_descriptor(_class: Id, _sel: Sel) -> Id {
    let object = object::make(c"MTLRenderPassDescriptor", RenderPassDescriptor::default());
    // SAFETY: a fresh object this call owns.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

/// Which attachment of a render pass descriptor one element names.
#[derive(Clone, Copy)]
pub(crate) struct AttachmentElement {
    pub owner: Id,
    pub slot: AttachmentSlot,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum AttachmentSlot {
    Color(usize),
    Depth,
    Stencil,
}

pub(super) fn attachment_of<'a>(element: Id) -> &'a mut AttachmentDescriptor {
    // SAFETY: an attachment element of this crate's.
    let element = *unsafe { object::body::<AttachmentElement>(element) };
    // SAFETY: the element holds the descriptor which made it.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(element.owner) };
    match element.slot {
        AttachmentSlot::Color(index) => &mut descriptor.colors[index],
        AttachmentSlot::Depth => &mut descriptor.depth,
        AttachmentSlot::Stencil => &mut descriptor.stencil,
    }
}

pub(super) unsafe extern "C" fn pass_color_attachments(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    if !descriptor.color_objects.is_null() {
        return descriptor.color_objects;
    }
    let elements = (0..RENDER_TARGETS)
        .map(|index| {
            object::make(
                c"R3MTLRenderPassAttachmentDescriptor",
                AttachmentElement {
                    owner: receiver,
                    slot: AttachmentSlot::Color(index),
                },
            )
        })
        .collect::<Vec<_>>();
    let collection = object::make(
        c"R3MTLIndexedCollection",
        crate::resource::IndexedCollection { elements },
    );
    // SAFETY: as above.
    unsafe { object::body::<RenderPassDescriptor>(receiver) }.color_objects = collection;
    collection
}

pub(super) unsafe extern "C" fn pass_single_attachment(receiver: Id, sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    let depth = unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() == b"depthAttachment";
    let existing = if depth {
        descriptor.depth_object
    } else {
        descriptor.stencil_object
    };
    if !existing.is_null() {
        return existing;
    }
    let element = object::make(
        c"R3MTLRenderPassAttachmentDescriptor",
        AttachmentElement {
            owner: receiver,
            slot: if depth {
                AttachmentSlot::Depth
            } else {
                AttachmentSlot::Stencil
            },
        },
    );
    // SAFETY: as above.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    if depth {
        descriptor.depth_object = element;
    } else {
        descriptor.stencil_object = element;
    }
    element
}

pub(super) unsafe extern "C" fn set_attachment_object(receiver: Id, sel: Sel, value: Id) {
    let attachment = attachment_of(receiver);
    // SAFETY: a selector the runtime owns.
    let slot = match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setTexture:" => &mut attachment.texture,
        b"setResolveTexture:" => &mut attachment.resolve_texture,
        _ => return,
    };
    objc::arc::release(std::mem::replace(slot, objc::arc::retain(value)));
}

pub(super) unsafe extern "C" fn set_attachment_number(receiver: Id, sel: Sel, value: u64) {
    let attachment = attachment_of(receiver);
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setLevel:" => attachment.level = value,
        b"setSlice:" => attachment.slice = value,
        b"setLoadAction:" => attachment.load_action = value,
        b"setStoreAction:" => attachment.store_action = value,
        b"setClearStencil:" => attachment.clear_stencil = value as u32,
        b"setResolveLevel:" => attachment.resolve_level = value,
        b"setResolveSlice:" => attachment.resolve_slice = value,
        b"setDepthPlane:" => (),
        _ => (),
    }
}

pub(super) unsafe extern "C" fn set_attachment_clear_color(
    receiver: Id,
    _sel: Sel,
    color: ClearColor,
) {
    attachment_of(receiver).clear_color = [color.red, color.green, color.blue, color.alpha];
}

pub(super) unsafe extern "C" fn set_attachment_clear_depth(receiver: Id, _sel: Sel, depth: f64) {
    attachment_of(receiver).clear_depth = depth;
}

pub(super) unsafe extern "C" fn new_compute_encoder(receiver: Id, _sel: Sel) -> Id {
    new_encoder(receiver, Kind::Compute)
}

pub(super) unsafe extern "C" fn new_blit_encoder(receiver: Id, _sel: Sel) -> Id {
    new_encoder(receiver, Kind::Blit)
}

pub(super) unsafe extern "C" fn new_render_encoder(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    let encoder = new_encoder(receiver, Kind::Render);
    if encoder.is_null() {
        return encoder;
    }
    // SAFETY: `MTLRenderPassDescriptor` is this crate's class.
    let described = unsafe { object::body::<RenderPassDescriptor>(descriptor) };
    begin_render_pass(encoder, described);
    encoder
}
