//! Holding a block past the call that passed it.
//!
//! A block a guest writes inline lives on the caller's stack, so anything that
//! runs one later - a queued operation, a completion handler - has to copy it to
//! the heap first. The copy is libclosure's, not this crate's: reimplementing it
//! would mean running the block's own copy helper over its captures.

use objc::object::Id;

/// Byte offset of `invoke` in a `Block_layout`: `isa`, `flags`, `reserved`.
///
/// The same constant `rosetta3::shims::block` reads, and for the mirror-image
/// reason: that module rewrites the pointer so a host call reaches the guest,
/// and this one calls what it finds there.
pub(crate) const INVOKE_OFFSET: usize = 16;

/// libclosure's `_Block_copy` and `_Block_release`, by name.
///
/// Refused by name rather than skipped when the host has no libclosure: carrying
/// on would call a block whose captures are a dead stack frame, which is a wrong
/// value read as a live one rather than a failure anyone could see. `who` names
/// the selector that wanted the block, since that is what has to be looked at.
fn runtime(name: &std::ffi::CStr, who: &str) -> unsafe extern "C" fn(Id) -> Id {
    // SAFETY: a NUL-terminated name, and the default search order.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if found.is_null() {
        objc::fail!(
            "{who}: this host has no {}, so the block cannot be kept past the call that passed it",
            name.to_string_lossy()
        );
    }
    // SAFETY: both are `void *(*)(const void *)` in the one respect that
    // matters - one pointer in, one pointer out for the copy, and a return the
    // release's caller does not read.
    unsafe { std::mem::transmute(found) }
}

/// A heap copy of `block`, owned by the caller.
pub(crate) fn copy(block: Id, who: &str) -> Id {
    if block.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a block literal, which is what `_Block_copy` takes.
    unsafe { runtime(c"_Block_copy", who)(block) }
}

/// Release a copy [`copy`] made.
pub(crate) fn release(block: Id, who: &str) {
    if block.is_null() {
        return;
    }
    // SAFETY: a block `copy` returned.
    unsafe { runtime(c"_Block_release", who)(block) };
}

/// The `invoke` a block literal carries, or `None` for a block with none.
///
/// # Safety
///
/// `block` must be null or a block literal.
unsafe fn invoke(block: Id) -> Option<usize> {
    if block.is_null() {
        return None;
    }
    // SAFETY: the caller's obligation - `invoke` is the third word. Under the
    // emulated backend the pointer found there is already a thunk, because the
    // method's `@?` encoding had `rosetta3::shims::block` rewrite it on the way
    // in.
    let found = unsafe { *((block as *const u8).add(INVOKE_OFFSET) as *const usize) };
    (found != 0).then_some(found)
}

/// Call a `void (^)(void)`.
///
/// # Safety
///
/// `block` must be null or a block literal of that prototype.
pub(crate) unsafe fn call(block: Id) {
    // SAFETY: the caller's obligation.
    let Some(invoke) = (unsafe { invoke(block) }) else {
        return;
    };
    // SAFETY: as above.
    unsafe {
        let f: unsafe extern "C" fn(Id) = std::mem::transmute(invoke);
        f(block);
    }
}

/// Call a `void (^)(id)`.
///
/// # Safety
///
/// `block` must be null or a block literal of that prototype.
pub(crate) unsafe fn call_with(block: Id, argument: Id) {
    // SAFETY: the caller's obligation.
    let Some(invoke) = (unsafe { invoke(block) }) else {
        return;
    };
    // SAFETY: as above.
    unsafe {
        let f: unsafe extern "C" fn(Id, Id) = std::mem::transmute(invoke);
        f(block, argument);
    }
}

/// Building a block literal without a compiler that has blocks.
#[cfg(test)]
pub(crate) mod fake {
    use std::ffi::c_void;

    use objc::object::Id;

    /// A *global* block, which is what makes this possible: `_Block_copy` of one
    /// is the block itself, so a test still takes the copy path a real one does
    /// without a descriptor's copy and dispose helpers having to exist.
    #[repr(C)]
    pub(crate) struct GlobalBlock {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: *const c_void,
        descriptor: *const Descriptor,
    }

    #[repr(C)]
    pub(crate) struct Descriptor {
        reserved: u64,
        size: u64,
    }

    // SAFETY: a global block is immutable and outlives every thread that runs
    // it, which is what `BLOCK_IS_GLOBAL` tells libclosure.
    unsafe impl Sync for GlobalBlock {}

    static DESCRIPTOR: Descriptor = Descriptor {
        reserved: 0,
        size: size_of::<GlobalBlock>() as u64,
    };

    impl GlobalBlock {
        pub(crate) fn id(&self) -> Id {
            self as *const GlobalBlock as Id
        }
    }

    /// A block whose `invoke` is `imp`, which must take the block itself as its
    /// first argument and whatever the caller will pass after it.
    pub(crate) fn block(imp: *const c_void) -> Box<GlobalBlock> {
        // SAFETY: a NUL-terminated name, and the default search order.
        let isa = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_NSConcreteGlobalBlock".as_ptr()) };
        assert!(!isa.is_null(), "this host has no libclosure");
        Box::new(GlobalBlock {
            isa,
            // `BLOCK_IS_GLOBAL`, which is what makes the copy a no-op.
            flags: 1 << 28,
            reserved: 0,
            invoke: imp,
            descriptor: &DESCRIPTOR,
        })
    }
}
