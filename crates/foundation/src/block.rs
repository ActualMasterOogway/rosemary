use objc::object::Id;

/// Byte offset of `invoke` in a `Block_layout`: `isa`, `flags`, `reserved`.
///
/// The same constant `rosetta3::shims::block` reads, and for the mirror-image
/// reason: that module rewrites the pointer so a host call reaches the guest,
/// and this one calls what it finds there.
pub(crate) const INVOKE_OFFSET: usize = 16;

/// The `invoke` a block literal carries, or `None` for a block with none.
///
/// # Safety
///
/// `block` must be null or a block literal.
unsafe fn invoke(block: Id) -> Option<usize> {
    if block.is_null() {
        return None;
    }
    // SAFETY: `invoke` is the third word. Under the
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
    let Some(invoke) = (unsafe { invoke(block) }) else {
        return;
    };
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
    let Some(invoke) = (unsafe { invoke(block) }) else {
        return;
    };
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
        objc::bootstrap();
        // `_NSConcreteGlobalBlock` under the name the class carries: this
        // runtime's own, so a block built here is one `objc::block` recognises.
        let isa = objc::class::lookup(c"__NSGlobalBlock__")
            .map(|class| objc::class::ptr(class) as *const c_void)
            .expect("registered by `objc::block`");
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
