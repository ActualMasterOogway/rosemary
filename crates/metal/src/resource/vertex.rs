//! `MTLVertexDescriptor` and the indexed collections Metal vends it through.

use super::*;

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

pub(super) unsafe extern "C" fn vertex_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated vertex descriptor.
    let descriptor = unsafe { object::body::<VertexDescriptor>(receiver) };
    descriptor.layouts = [VertexLayout::default(); VERTEX_SLOTS];
    descriptor.attributes = [VertexAttribute::default(); VERTEX_SLOTS];
    // `-init` is `-reset` here, and a reset descriptor still owns whatever a
    // previous one vended.
    objc::arc::release(std::mem::replace(
        &mut descriptor.layout_objects,
        std::ptr::null_mut(),
    ));
    objc::arc::release(std::mem::replace(
        &mut descriptor.attribute_objects,
        std::ptr::null_mut(),
    ));
    receiver
}

pub(super) fn vertex_collection(receiver: Id, layout: bool) -> Id {
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

pub(super) unsafe extern "C" fn vertex_layouts(receiver: Id, _sel: Sel) -> Id {
    vertex_collection(receiver, true)
}

pub(super) unsafe extern "C" fn vertex_attributes(receiver: Id, _sel: Sel) -> Id {
    vertex_collection(receiver, false)
}

pub(super) unsafe extern "C" fn indexed_element(receiver: Id, _sel: Sel, index: usize) -> Id {
    // SAFETY: an indexed collection of this crate's.
    unsafe { object::body::<IndexedCollection>(receiver) }
        .elements
        .get(index)
        .copied()
        .unwrap_or(std::ptr::null_mut())
}

/// `-[R3MTLIndexedCollection dealloc]`: the elements are the collection's own.
///
/// Metal vends `-colorAttachments`, `-layouts` and `-attributes` as objects a
/// game subscripts and mutates in place, so the descriptor which made one holds
/// it and each element it contains. Without this every render pass descriptor a
/// frame created left its eight attachment objects behind.
pub(super) unsafe extern "C" fn collection_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a collection of this crate's.
    let collection = unsafe { object::body::<IndexedCollection>(receiver) };
    for element in std::mem::take(&mut collection.elements) {
        objc::arc::release(element);
    }
    objc::arc::finalize(receiver);
}

/// `-[MTLVertexDescriptor dealloc]`.
pub(super) unsafe extern "C" fn vertex_descriptor_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a vertex descriptor of this crate's.
    let descriptor = unsafe { object::body::<VertexDescriptor>(receiver) };
    objc::arc::release(std::mem::replace(
        &mut descriptor.layout_objects,
        std::ptr::null_mut(),
    ));
    objc::arc::release(std::mem::replace(
        &mut descriptor.attribute_objects,
        std::ptr::null_mut(),
    ));
    objc::arc::finalize(receiver);
}

pub(super) unsafe extern "C" fn set_vertex_element_value(receiver: Id, sel: Sel, value: u64) {
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
