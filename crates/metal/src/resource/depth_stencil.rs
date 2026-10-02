use super::*;

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

pub(super) unsafe extern "C" fn depth_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated depth/stencil descriptor.
    *unsafe { object::body::<DepthStencilState>(receiver) } = DepthStencilState::default();
    receiver
}

pub(super) unsafe extern "C" fn set_depth_compare(receiver: Id, _sel: Sel, value: u64) {
    // SAFETY: a depth/stencil descriptor of this crate's.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_compare = value;
}

pub(super) unsafe extern "C" fn depth_compare(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: a depth/stencil descriptor of this crate's.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_compare
}

pub(super) unsafe extern "C" fn set_depth_write(receiver: Id, _sel: Sel, value: u64) {
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_write = value & 0xff != 0;
}

pub(super) unsafe extern "C" fn depth_write(receiver: Id, _sel: Sel) -> bool {
    // SAFETY: a depth/stencil descriptor of this crate's.
    unsafe { object::body::<DepthStencilState>(receiver) }.depth_write
}

/// `-setFrontFaceStencil:` and `-setBackFaceStencil:` copy the descriptor's
/// current values: Metal's stencil descriptor is a value type.
pub(super) unsafe extern "C" fn set_face_stencil(receiver: Id, sel: Sel, face: Id) {
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
pub(super) unsafe extern "C" fn face_stencil(receiver: Id, sel: Sel) -> Id {
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

pub(super) unsafe extern "C" fn stencil_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated stencil descriptor.
    *unsafe { object::body::<StencilState>(receiver) } = StencilState::default();
    receiver
}

pub(super) unsafe extern "C" fn set_stencil_value(receiver: Id, sel: Sel, value: u64) {
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
    object::make(c"RosemaryMTLDepthStencilState", state)
}

#[cfg(test)]
mod tests {
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
