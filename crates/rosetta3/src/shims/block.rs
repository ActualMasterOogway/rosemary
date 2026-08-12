//! A block handed to the host: the guest function pointer that travels inside
//! a *literal*.
//!
//! ## Why this is not a `cbsig!` entry
//!
//! `dispatch_async(queue, ^{ ... })` looks like an ordinary callback taker, and
//! it is not one. The second argument is not a function pointer — it is a
//! pointer to a `Block_layout`, a structure the compiler emitted, whose fourth
//! field is the code. So the substitution [`crate::abi::marshal`] performs for
//! a named callback argument would replace the *block* with a thunk, and
//! libdispatch would then read a trampoline's first eight bytes as an `isa`.
//! This is the same shape as [`crate::shims::zlib`] and
//! [`crate::shims::audiounit`]: a pointer that is not an argument, reached
//! through one that is.
//!
//! ## What is guest and what is already the host's
//!
//! A block literal is less foreign than it looks. Its `isa` is an ordinary bind
//! of `_NSConcreteStackBlock` or `_NSConcreteGlobalBlock`, which native
//! passthrough resolves to the **host's** own — so `_Block_copy` recognises the
//! object, copies the right number of bytes (the descriptor says how many), and
//! reference-counts it correctly with nothing done here. The layout is fixed by
//! the Blocks ABI rather than by a compiler's choice, and every field in it is
//! a pointer or an `int32_t`, so it needs no translation.
//!
//! What is the guest's is the code: `invoke`, and — when the block captures
//! something that has to be retained — the descriptor's `copy` and `dispose`
//! helpers. Each is replaced with a thunk, in place, for
//! [`crate::shims::zlib`]'s reason: libdispatch copies the literal and calls
//! `invoke` off this thread long after the call returns, so a substitution kept
//! on the side would never be read.
//!
//! ## The descriptor is shared, and may not be writable
//!
//! `invoke` lives in the literal, which is on the guest's stack or its heap and
//! is always writable. The copy and dispose helpers live in the **descriptor**,
//! which the compiler emits once per block *type* in read-only data — so
//! patching them can fail, and when it does this refuses by name rather than
//! writing somewhere else. A block that captures nothing needing a helper has
//! no such fields at all (`BLOCK_HAS_COPY_DISPOSE` is clear), which is the
//! common case and the one that never reaches the question.

use core::ffi::c_void;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, Callbacks, GuestMemory, Result};

use super::bad_pointer;

/// Byte offset of `invoke` in a `Block_layout`: `isa`, `flags`, `reserved`.
const INVOKE_OFFSET: u64 = 16;
/// Byte offset of `descriptor`, immediately after `invoke`.
const DESCRIPTOR_OFFSET: u64 = 24;
/// Byte offset of `flags`, which says which optional descriptor fields exist.
const FLAGS_OFFSET: u64 = 8;
/// Byte offsets of `copy` and `dispose` in a `Block_descriptor_2`, which
/// follows `Block_descriptor_1`'s `reserved` and `size`.
const COPY_OFFSET: u64 = 16;
const DISPOSE_OFFSET: u64 = 24;
/// `BLOCK_HAS_COPY_DISPOSE`: the descriptor carries the two helper pointers.
const BLOCK_HAS_COPY_DISPOSE: u32 = 1 << 25;

/// Replace every guest code pointer reachable from the block at `block` with a
/// thunk, in place.
///
/// A null `invoke` is not tolerated the way [`crate::shims::zlib`] tolerates a
/// null allocator: zlib's field is optional and this one is the block itself.
///
/// Visible to the crate because a block does not only arrive as an argument to
/// a C function: `-[NSNotificationCenter addObserverForName:…usingBlock:]` is a
/// *message*, and [`crate::objc::dispatch`] needs exactly this and none of the
/// rest of this module. The literal is edited where it lies, so the caller
/// passes the same pointer on afterwards.
pub(crate) fn substitute_block(
    symbol: &str,
    block: u64,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    substitute_pointer(symbol, block.wrapping_add(INVOKE_OFFSET), mem, callbacks)?;

    let flags = read_u32(symbol, mem, block.wrapping_add(FLAGS_OFFSET))?;
    if flags & BLOCK_HAS_COPY_DISPOSE == 0 {
        return Ok(());
    }
    let descriptor = read_u64(symbol, mem, block.wrapping_add(DESCRIPTOR_OFFSET))?;
    if descriptor == 0 {
        return Err(bad_pointer(symbol, block.wrapping_add(DESCRIPTOR_OFFSET)));
    }
    for offset in [COPY_OFFSET, DISPOSE_OFFSET] {
        substitute_pointer(symbol, descriptor.wrapping_add(offset), mem, callbacks)?;
    }
    Ok(())
}

/// Read one guest code pointer, thunk it, and write the thunk back.
fn substitute_pointer(
    symbol: &str,
    at: u64,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let guest = read_u64(symbol, mem, at)?;
    let host = callbacks.substitute(symbol, guest)?;
    if host != guest {
        mem.write(at, &host.to_le_bytes())
            .map_err(|_| bad_pointer(symbol, at))?;
    }
    Ok(())
}

fn read_u64(symbol: &str, mem: &mut dyn GuestMemory, at: u64) -> Result<u64> {
    let mut buf = [0u8; 8];
    mem.read(at, &mut buf)
        .map_err(|_| bad_pointer(symbol, at))?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u32(symbol: &str, mem: &mut dyn GuestMemory, at: u64) -> Result<u32> {
    let mut buf = [0u8; 4];
    mem.read(at, &mut buf)
        .map_err(|_| bad_pointer(symbol, at))?;
    Ok(u32::from_le_bytes(buf))
}

/// Call a host libdispatch entry point after rewriting the guest block.
///
/// This deliberately does not hold any rosetta3 lock while libdispatch is
/// running the block. In particular, `dispatch_sync` is allowed to call the
/// runtime back on the same thread, and the block itself can call `dlopen` or
/// register another callback. Keeping this helper limited to memory rewriting
/// followed by the host call makes that re-entry explicit.
fn call_block(symbol: &str, host: u64, args: &[u64]) {
    // SAFETY: all block-taking dispatch functions handled here return void
    // and receive only integer/pointer arguments. The host ABI ignores the
    // unused argument registers, and the caller supplies the exact prefix for
    // the selected function.
    unsafe {
        match args {
            [a, b] => core::mem::transmute::<u64, extern "C" fn(u64, u64)>(host)(*a, *b),
            [a, b, c] => {
                core::mem::transmute::<u64, extern "C" fn(u64, u64, u64)>(host)(*a, *b, *c)
            }
            _ => unreachable!("unsupported block dispatch arity for {symbol}"),
        }
    }
}

/// `void dispatch_async(dispatch_queue_t queue, dispatch_block_t block)`
///
/// The queue is the host's — `dispatch_get_main_queue()` is the address of
/// `_dispatch_main_q`, which native passthrough bound to libdispatch's own — so
/// once the block's code pointers are thunks this is an ordinary two-pointer
/// call.
fn dispatch_async(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (queue, block) = (ctx.rdi, ctx.rsi);
    if block == 0 {
        return Err(bad_pointer(symbol, 0));
    }
    substitute_block(symbol, block, mem, callbacks)?;
    let host = host_or_refuse(symbol)?;
    call_block(symbol, host, &[queue, block]);
    Ok(())
}

/// `void dispatch_sync(dispatch_queue_t queue, dispatch_block_t block)`
fn dispatch_sync(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (queue, block) = (ctx.rdi, ctx.rsi);
    if block == 0 {
        return Err(bad_pointer(symbol, 0));
    }
    substitute_block(symbol, block, mem, callbacks)?;
    call_block(symbol, host_or_refuse(symbol)?, &[queue, block]);
    Ok(())
}

/// `void dispatch_after(dispatch_time_t when, dispatch_queue_t queue,
/// dispatch_block_t block)`
fn dispatch_after(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (when, queue, block) = (ctx.rdi, ctx.rsi, ctx.rdx);
    if block == 0 {
        return Err(bad_pointer(symbol, 0));
    }
    substitute_block(symbol, block, mem, callbacks)?;
    call_block(symbol, host_or_refuse(symbol)?, &[when, queue, block]);
    Ok(())
}

/// `void dispatch_once(dispatch_once_t *predicate, dispatch_block_t block)`
fn dispatch_once(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (predicate, block) = (ctx.rdi, ctx.rsi);
    if predicate == 0 || block == 0 {
        return Err(bad_pointer(
            symbol,
            if predicate == 0 { predicate } else { block },
        ));
    }
    substitute_block(symbol, block, mem, callbacks)?;
    call_block(symbol, host_or_refuse(symbol)?, &[predicate, block]);
    Ok(())
}

/// `void *_Block_copy(const void *block)`
///
/// **`_Block_copy` runs the guest's code before it returns**, which is the whole
/// reason this is a shim and not a signature. The table used to describe it as a
/// plain two-pointer call on the argument that copying a block "does not read
/// the `invoke` field" — true, and beside the point. When
/// `BLOCK_HAS_COPY_DISPOSE` is set, `_Block_copy` calls the descriptor's **copy
/// helper** on the new literal, immediately, to retain everything the block
/// captured. That helper is guest code, and the host calls it as arm64: a
/// `SIGBUS` inside `libsystem_blocks`' `_call_copy_helpers_excp`, with the
/// faulting address equal to the pc, one frame below anything that names a
/// block.
///
/// So the pointers are substituted first, exactly as they are for a block handed
/// to libdispatch. A block that captures nothing has no helpers and reaches none
/// of this — which is why the plain signature worked for four games.
///
/// `objc_retainBlock` is this function under another name and is fixed the same
/// way, in [`crate::objc::dispatch`]'s table rather than here, because that is
/// where the runtime's own entry points live.
fn block_copy(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let block = ctx.rdi;
    // A null block is legal here in a way it is not for `dispatch_async`:
    // `_Block_copy(NULL)` is defined to answer null, and ARC emits it on paths
    // where the block is optional.
    if block == 0 {
        ctx.rax = 0;
        return Ok(());
    }
    substitute_block(symbol, block, mem, callbacks)?;
    let host = host_or_refuse(symbol)?;
    // SAFETY: `host` is libsystem_blocks' own `_Block_copy`, whose prototype is
    // one pointer in and one pointer out. The block's code pointers are thunks
    // by the line above, so the copy helper this call runs is native.
    let f: extern "C" fn(u64) -> u64 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> u64>(host) };
    ctx.rax = f(block);
    Ok(())
}

/// `void dispatch_group_notify(dispatch_group_t group, dispatch_queue_t queue,
/// dispatch_block_t block)`
///
/// [`dispatch_after`]'s shape with a group in front of the queue. The block runs
/// once the group's last `dispatch_group_leave` lands, which may be on any
/// thread and long after this returns — so the substitution has to be *in* the
/// literal, not kept beside it.
fn dispatch_group_notify(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (group, queue, block) = (ctx.rdi, ctx.rsi, ctx.rdx);
    if block == 0 {
        return Err(bad_pointer(symbol, 0));
    }
    substitute_block(symbol, block, mem, callbacks)?;
    call_block(symbol, host_or_refuse(symbol)?, &[group, queue, block]);
    Ok(())
}

/// `CFRunLoopObserverRef CFRunLoopObserverCreateWithHandler(CFAllocatorRef
/// allocator, CFOptionFlags activities, Boolean repeats, CFIndex order,
/// void (^block)(CFRunLoopObserverRef, CFRunLoopActivity))`
///
/// The block-taking half of CoreFoundation's run-loop observer pair, and the
/// only entry point in this module that is not libdispatch's. Its sibling
/// `CFRunLoopObserverCreate` takes a callback *and* a context structure and is
/// `CFRunLoopSourceCreate`'s problem; this spelling puts everything in one
/// literal, which is exactly the shape that can be answered.
///
/// The block is the fifth argument, so it is in `r8`, and unlike every
/// `dispatch_*` above this one **returns** — the observer the guest will add to
/// a run loop and later invalidate.
fn run_loop_observer_create_with_handler(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let block = ctx.r8;
    if block == 0 {
        return Err(bad_pointer(symbol, 0));
    }
    substitute_block(symbol, block, mem, callbacks)?;
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, whose declaration is the one
    // above. `CFOptionFlags` and `CFIndex` are 64-bit and `Boolean` a byte, so
    // every argument is one integer register; the block's code pointers are
    // thunks by the line above.
    let f: extern "C" fn(u64, u64, u8, i64, u64) -> u64 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64, u64, u8, i64, u64) -> u64>(host) };
    ctx.rax = f(ctx.rdi, ctx.rsi, ctx.rdx as u8, ctx.rcx as i64, block);
    Ok(())
}

/// `dispatch_data_t dispatch_data_create(const void *buffer, size_t size,
/// dispatch_queue_t queue, dispatch_block_t destructor)`
fn dispatch_data_create(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let destructor = ctx.rcx;
    if destructor != 0 {
        substitute_block(symbol, destructor, mem, callbacks)?;
    }
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol matched the exact four-pointer/integer libdispatch
    // entry point. The data buffer and queue are host-visible identity-mapped
    // values; the destructor has been rewritten to a native thunk above.
    let f: extern "C" fn(*const c_void, u64, u64, u64) -> u64 = unsafe {
        core::mem::transmute::<u64, extern "C" fn(*const c_void, u64, u64, u64) -> u64>(host)
    };
    ctx.rax = f(ctx.rdi as *const c_void, ctx.rsi, ctx.rdx, destructor);
    Ok(())
}

/// The host's address for one of this module's names.
fn host_or_refuse(symbol: &str) -> Result<u64> {
    crate::dyld::host_symbol(symbol.strip_prefix('_').unwrap_or(symbol)).ok_or(AbiError::NoShim {
        symbol: symbol.to_string(),
    })
}

/// One of this module's implementations, in the shape [`crate::shims`] wants —
/// the wider one, carrying [`Callbacks`].
pub type Shim = crate::signal::Shim;

/// Whether `name` (underscore already stripped) is one of this module's.
///
/// Block-taking dispatch entry points must all rewrite the block before handing
/// it to libdispatch. Passing an x86-64 `invoke` pointer through unchanged is
/// especially bad for `dispatch_sync`: the queue waits for a callback that can
/// never execute correctly, which presents to the guest as a deadlock.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "dispatch_async" => Some(dispatch_async),
        "dispatch_sync" => Some(dispatch_sync),
        "dispatch_after" => Some(dispatch_after),
        "dispatch_once" => Some(dispatch_once),
        "dispatch_data_create" => Some(dispatch_data_create),
        "dispatch_group_notify" => Some(dispatch_group_notify),
        "CFRunLoopObserverCreateWithHandler" => Some(run_loop_observer_create_with_handler),
        // Both spellings: the Mach-O symbol is `__Block_copy`, and `c_name`
        // strips one underscore, so what arrives here still has the one the
        // function's own name begins with.
        "_Block_copy" | "Block_copy" => Some(block_copy),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout claim, which is what a wrong answer would violate silently:
    /// a block literal is `isa`, `flags`, `reserved`, `invoke`, `descriptor`,
    /// and the offsets above must be those fields.
    #[test]
    fn the_offsets_are_the_blocks_abi_layout() {
        #[repr(C)]
        struct BlockLayout {
            isa: u64,
            flags: u32,
            reserved: u32,
            invoke: u64,
            descriptor: u64,
        }
        let block = BlockLayout {
            isa: 0,
            flags: 0,
            reserved: 0,
            invoke: 0,
            descriptor: 0,
        };
        let base = &block as *const BlockLayout as u64;
        assert_eq!(&block.flags as *const u32 as u64 - base, FLAGS_OFFSET);
        assert_eq!(&block.invoke as *const u64 as u64 - base, INVOKE_OFFSET);
        assert_eq!(
            &block.descriptor as *const u64 as u64 - base,
            DESCRIPTOR_OFFSET
        );
    }

    /// The same for the descriptor, whose optional half is where the two
    /// helpers live: `reserved` and `size` come first.
    #[test]
    fn the_helper_offsets_follow_reserved_and_size() {
        #[repr(C)]
        struct Descriptor {
            reserved: u64,
            size: u64,
            copy: u64,
            dispose: u64,
        }
        let d = Descriptor {
            reserved: 0,
            size: 0,
            copy: 0,
            dispose: 0,
        };
        let base = &d as *const Descriptor as u64;
        assert_eq!(&d.copy as *const u64 as u64 - base, COPY_OFFSET);
        assert_eq!(&d.dispose as *const u64 as u64 - base, DISPOSE_OFFSET);
    }
}
