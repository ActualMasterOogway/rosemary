//! `MTLVertexDescriptor` and the indexed collections Metal vends it through.

use super::*;

/// `MTLVertexDescriptor`: 31 buffer layouts and 31 attributes, each a small
/// bag of integers the render pipeline reads at creation time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VertexLayout {
    pub stride: u64,
    pub step_function: u64,
    pub step_rate: u64,
}

impl Default for VertexLayout {
    fn default() -> Self {
        Self {
            stride: 0,
            // MTLVertexStepFunctionPerVertex.
            step_function: 1,
            step_rate: 1,
        }
    }
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
    /// How to tell one of `elements` that its owner is gone. See [`orphan`].
    pub orphan: fn(Id),
}

/// A child object which mutates the descriptor that vended it.
///
/// The descriptor owns the collection and the collection owns its elements,
/// but a guest may hold an element - or the collection - longer than the
/// descriptor: `-colorAttachments` is an object with its own retain count.
/// The element cannot retain the owner back without making a cycle neither
/// side can break, so the owner instead *orphans* every element it vended on
/// the way out, and an orphaned element's setters and getters do nothing
/// rather than writing through a dangling pointer.
pub(crate) trait Element {
    fn owner(&mut self) -> &mut Id;
}

/// Null the owner of one element of a `T`-shaped collection.
///
/// # Safety
///
/// `element` must be an instance of the class declared for this `T`.
pub(crate) unsafe fn orphan_element<T: Element>(element: Id) {
    // SAFETY: the caller's obligation.
    *unsafe { object::body::<T>(element) }.owner() = std::ptr::null_mut();
}

/// Tell every element of a collection - which may be null, because a
/// descriptor only makes one when a guest asks - that its owner is gone.
pub(crate) fn orphan(collection: Id) {
    if collection.is_null() {
        return;
    }
    // SAFETY: a collection of this crate's.
    let collection = unsafe { object::body::<IndexedCollection>(collection) };
    for element in &collection.elements {
        (collection.orphan)(*element);
    }
}

impl Element for VertexElement {
    fn owner(&mut self) -> &mut Id {
        &mut self.owner
    }
}

pub(super) unsafe extern "C" fn vertex_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated vertex descriptor.
    let descriptor = unsafe { object::body::<VertexDescriptor>(receiver) };
    descriptor.layouts = [VertexLayout::default(); VERTEX_SLOTS];
    descriptor.attributes = [VertexAttribute::default(); VERTEX_SLOTS];
    // `-init` is `-reset` here, and a reset descriptor still owns whatever a
    // previous one vended.
    release_collections(descriptor, false);
    receiver
}

/// Give up the collections a vertex descriptor vended.
///
/// `orphaning` only where the descriptor itself is going: a guest which holds
/// `-layouts` past the descriptor keeps the collection and its elements alive,
/// and orphaning leaves their setters harmless rather than writing into freed
/// memory. A `-reset` must not orphan, because the descriptor is still there
/// and Metal keeps setting through a collection a guest took before it.
fn release_collections(descriptor: &mut VertexDescriptor, orphaning: bool) {
    for slot in [
        &mut descriptor.layout_objects,
        &mut descriptor.attribute_objects,
    ] {
        let collection = std::mem::replace(slot, std::ptr::null_mut());
        if orphaning {
            orphan(collection);
        }
        objc::arc::release(collection);
    }
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
    let collection = object::make(
        c"R3MTLIndexedCollection",
        IndexedCollection {
            elements,
            // SAFETY: every element of this collection is an `R3MTLVertexElement`.
            orphan: |element| unsafe { orphan_element::<VertexElement>(element) },
        },
    );
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
    release_collections(unsafe { object::body::<VertexDescriptor>(receiver) }, true);
    objc::arc::finalize(receiver);
}

pub(super) unsafe extern "C" fn set_vertex_element_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: a vertex element of this crate's.
    let element = *unsafe { object::body::<VertexElement>(receiver) };
    if element.owner.is_null() {
        // The descriptor which vended this element is gone.
        return;
    }
    // SAFETY: the element holds the descriptor which made it, which orphans
    // the element before it goes.
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
