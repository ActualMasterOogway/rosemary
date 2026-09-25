use super::*;

use crate::object;
use objc::object::Id;
use objc::sel::Sel;

/// One element of `-[MTLRenderPipelineDescriptor colorAttachments]`, which
/// mutates the descriptor which vended it.
#[derive(Clone, Copy)]
pub(crate) struct ColorElement {
    pub owner: Id,
    pub index: usize,
}

impl crate::resource::Element for ColorElement {
    fn owner(&mut self) -> &mut Id {
        &mut self.owner
    }
}

pub(super) unsafe extern "C" fn descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pipeline descriptor of this crate's. This is `-reset`
    // as well as `-init`, and a reset descriptor still owns what it holds.
    unsafe { release_descriptor_contents(receiver, false) };
    *unsafe { object::body::<RenderPipelineDescriptor>(receiver) } = RenderPipelineDescriptor {
        vertex_function: std::ptr::null_mut(),
        fragment_function: std::ptr::null_mut(),
        vertex_descriptor: std::ptr::null_mut(),
        colors: [ColorAttachment::fresh(); COLOR_ATTACHMENTS],
        depth_format: 0,
        stencil_format: 0,
        sample_count: 1,
        alpha_to_coverage: false,
        color_objects: std::ptr::null_mut(),
    };
    receiver
}

/// Give up every reference a render pipeline descriptor holds.
///
/// `orphaning` only where the descriptor itself is going: `-reset` leaves it
/// alive, and Metal keeps setting through a `-colorAttachments` a guest took
/// before the reset.
///
/// # Safety
///
/// `receiver` must be a `MTLRenderPipelineDescriptor` of this crate's.
unsafe fn release_descriptor_contents(receiver: Id, orphaning: bool) {
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    for slot in [
        &mut descriptor.vertex_function,
        &mut descriptor.fragment_function,
        &mut descriptor.vertex_descriptor,
    ] {
        objc::arc::release(std::mem::replace(slot, std::ptr::null_mut()));
    }
    // The colour attachments point back at this descriptor, and a guest may
    // hold one after releasing it: orphan them before the body goes.
    let collection = std::mem::replace(&mut descriptor.color_objects, std::ptr::null_mut());
    if orphaning {
        crate::resource::orphan(collection);
    }
    objc::arc::release(collection);
}

/// `-[MTLRenderPipelineDescriptor dealloc]`.
pub(super) unsafe extern "C" fn descriptor_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a render pipeline descriptor of this crate's.
    unsafe { release_descriptor_contents(receiver, true) };
    objc::arc::finalize(receiver);
}

pub(super) unsafe extern "C" fn set_descriptor_object(receiver: Id, sel: Sel, value: Id) {
    // SAFETY: a render pipeline descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    let slot = match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setVertexFunction:" => &mut descriptor.vertex_function,
        b"setFragmentFunction:" => &mut descriptor.fragment_function,
        b"setVertexDescriptor:" => &mut descriptor.vertex_descriptor,
        _ => return,
    };
    objc::arc::release(std::mem::replace(slot, objc::arc::retain(value)));
}

pub(super) unsafe extern "C" fn set_descriptor_number(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a render pipeline descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setDepthAttachmentPixelFormat:" => descriptor.depth_format = value,
        b"setStencilAttachmentPixelFormat:" => descriptor.stencil_format = value,
        b"setSampleCount:" | b"setRasterSampleCount:" => descriptor.sample_count = value,
        b"setAlphaToCoverageEnabled:" => descriptor.alpha_to_coverage = value & 0xff != 0,
        _ => (),
    }
}

pub(super) unsafe extern "C" fn color_attachments(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pipeline descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(receiver) };
    if !descriptor.color_objects.is_null() {
        return descriptor.color_objects;
    }
    let elements = (0..COLOR_ATTACHMENTS)
        .map(|index| {
            object::make(
                c"R3MTLColorAttachmentDescriptor",
                ColorElement {
                    owner: receiver,
                    index,
                },
            )
        })
        .collect::<Vec<_>>();
    let collection = object::make(
        c"R3MTLIndexedCollection",
        crate::resource::IndexedCollection {
            elements,
            // SAFETY: every element here is an `R3MTLColorAttachmentDescriptor`.
            orphan: |element| unsafe { crate::resource::orphan_element::<ColorElement>(element) },
        },
    );
    unsafe { object::body::<RenderPipelineDescriptor>(receiver) }.color_objects = collection;
    collection
}

pub(super) unsafe extern "C" fn set_color_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a colour attachment element of this crate's.
    let element = *unsafe { object::body::<ColorElement>(receiver) };
    if element.owner.is_null() {
        // The descriptor which vended this element is gone.
        return;
    }
    // SAFETY: the element holds the descriptor which made it, which orphans
    // the element before it goes.
    let descriptor = unsafe { object::body::<RenderPipelineDescriptor>(element.owner) };
    let color = &mut descriptor.colors[element.index];
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setPixelFormat:" => color.pixel_format = value,
        b"setBlendingEnabled:" => color.blending = value & 0xff != 0,
        b"setSourceRGBBlendFactor:" => color.source_rgb = value,
        b"setDestinationRGBBlendFactor:" => color.destination_rgb = value,
        b"setRgbBlendOperation:" => color.rgb_operation = value,
        b"setSourceAlphaBlendFactor:" => color.source_alpha = value,
        b"setDestinationAlphaBlendFactor:" => color.destination_alpha = value,
        b"setAlphaBlendOperation:" => color.alpha_operation = value,
        b"setWriteMask:" => color.write_mask = value,
        _ => (),
    }
}

pub(super) unsafe extern "C" fn identity(receiver: Id, _sel: Sel) -> Id {
    receiver
}
