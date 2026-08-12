//! `pthread_create`, and the two stacks a guest thread really has.
//!
//! ## What passthrough gets wrong, and it is not the types
//!
//! Nearly all of the `pthread` surface passes through, correctly and for a
//! checked reason: every opaque `pthread` type is byte-identical on x86-64 and
//! arm64 macOS, so the guest allocates a `pthread_mutex_t` on its own stack and
//! the host's libpthread operates on it in place. [`crate::abi::table`] says so
//! at length and `tests/thread.rs` checks it rather than trusting it.
//!
//! `pthread_create` is the one call in the family where that reasoning stops
//! holding, and the reason is that a guest thread has **two** stacks. The host
//! thread gets one from libpthread, and this runtime's own frames live on it —
//! the run loop, Cranelift compiling a region, every shim. The guest's frames
//! live somewhere else entirely, on a [`GuestStack`](crate::stack::GuestStack)
//! this runtime reserves. Two stacks, one attribute set, and passthrough
//! silently applies every stack attribute to the wrong one of them:
//!
//! - **`pthread_attr_setstacksize`** sized the host thread. A guest asking for
//!   64 KiB got a 64 KiB stack for the *JIT* — which then compiles a region on
//!   it — while its own frames went on the 8 MiB default it never asked for.
//!   Both halves are wrong, and the first is wrong in the direction that
//!   corrupts memory rather than the direction that wastes it.
//! - **`pthread_attr_setstackaddr`** and **`pthread_attr_setstack`** are worse
//!   again. They hand libpthread a buffer the guest allocated, and libpthread
//!   duly runs *arm64* frames on it: the runtime's frames, on memory the guest
//!   believes it owns and is free to write, with the guest's own frames still
//!   elsewhere. A program that manages its own thread stacks — a scripting
//!   runtime placing a coroutine's, a game pooling them — reached this by doing
//!   nothing unusual.
//!
//! So this module owns `pthread_create` and separates the two. The guest's
//! stack attributes describe the guest's stack, which is what the guest wrote
//! them about; the host thread gets a stack of this runtime's choosing, sized
//! for the JIT, because nothing in the guest has an opinion about it and none of
//! the guest's frames will be on it.
//!
//! ## Why the attribute *setters* are still passthrough
//!
//! They are not doing anything wrong. A `pthread_attr_t` is a record, and the
//! host's setters record the guest's request faithfully — including the
//! validation the guest would meet natively, `EINVAL` for a size below
//! `PTHREAD_STACK_MIN` or an unaligned address. What was wrong was never the
//! writing down; it was the one place the record is *read*, which is here. So
//! the shim reads it back with the host's own getters (identical layout again,
//! and the guest's attr was initialised by the host's `pthread_attr_init`), and
//! the guest's own `pthread_attr_getstacksize` keeps answering with what it set.
//!
//! Which half of the range each call means is not guessable and was measured
//! against this machine's libpthread rather than assumed:
//!
//! - `pthread_attr_setstack(attr, addr, size)` — `addr` is the **lowest**
//!   address.
//! - `pthread_attr_setstackaddr(attr, addr)` — `addr` is the **highest**.
//! - `pthread_attr_getstackaddr` reports the highest either way, which is also
//!   what `pthread_get_stackaddr_np` reports for a running thread.
//!
//! Reading `getstackaddr` therefore needs no case analysis: it is the top, for
//! every way the guest could have set it, including ways added later.
//!
//! ## The host attribute set is a copy, not a rebuild
//!
//! The attributes for the host thread are the guest's *bytes*, with the stack
//! fields overwritten. Enumerating the ones worth copying was the obvious
//! implementation and it is the one that rots: detach state, scheduling policy,
//! parameters, inherit-sched, contention scope, guard size — and
//! `pthread_attr_set_qos_class_np`, which is not in POSIX, is not in the `libc`
//! crate, and is what a game actually sets. Copying the record wholesale carries
//! every attribute that exists now and every one Apple adds later, and the two
//! this runtime must not honour are then set explicitly, which is a list of two
//! rather than a list of everything else.
//!
//! ## Scope
//!
//! `pthread_create` only. `join`, `detach`, `equal`, `kill`, `exit`,
//! `getspecific` and the rest stay passthrough, because the `pthread_t` handed
//! back is the host's own and every one of those calls is genuinely about the
//! host thread. `pthread_get_stackaddr_np` and `pthread_get_stacksize_np` are
//! shimmed in [`super::machthread`], next to `thread_get_state`: what they have
//! in common is a garbage collector asking about a thread that is not the
//! caller.

use core::ffi::c_void;
use core::mem::MaybeUninit;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{Callbacks, GuestMemory, Result};
use crate::thread::{GuestStart, StackRequest};

use super::set_int_return;

/// `EINVAL`, the one failure this shim produces without asking the host.
const EINVAL: i32 = 22;

/// The stack the *host* thread gets: the runtime's own frames, and the JIT's.
///
/// The main thread's size, and deliberately not the guest's request. What runs
/// on this stack is the run loop and Cranelift compiling a region, whose depth
/// has nothing to do with how deep the guest recurses — and a guest that asks
/// for a small thread stack, which is the ordinary reason to set the attribute
/// at all, would otherwise be handing the compiler a stack too small to compile
/// on. Reservations are lazy, so the cost of being generous is address space.
const HOST_THREAD_STACK: usize = crate::stack::DEFAULT_STACK;

/// `int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
/// void *(*start_routine)(void *), void *arg)`
pub fn pthread_create(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (out, attr, start, arg) = (ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx);
    if out == 0 {
        set_int_return(ctx, EINVAL);
        return Ok(());
    }

    // The guest's attributes, read before anything is created: a refusal here
    // costs a return code, and a refusal after the thread exists costs a thread
    // running with the wrong stack.
    let Some(mut host_attr) = read_attr(symbol, attr, mem)? else {
        set_int_return(ctx, EINVAL);
        return Ok(());
    };
    let stack = stack_request(&host_attr);
    // Whatever the guest asked for describes the *guest's* stack; the host
    // thread gets this runtime's own. Both are set explicitly, because the copy
    // above carries the guest's answers for them.
    //
    // SAFETY: `host_attr` is a live, initialised attribute set — a byte copy of
    // one the host's own `pthread_attr_init` produced, which is what
    // `read_attr` checked before returning it.
    unsafe {
        libc::pthread_attr_setstacksize(&mut host_attr, HOST_THREAD_STACK);
        // Only when the guest supplied an address, and only then: Darwin's
        // setter also clears the guard page, so a thread whose guest never
        // mentioned a stack address keeps the one libpthread would have given
        // it.
        if matches!(stack, StackRequest::Supplied { .. }) {
            libc::pthread_attr_setstackaddr(&mut host_attr, core::ptr::null_mut());
        }
    }

    // The start routine is guest code and crosses out as a thunk, exactly as
    // `qsort`'s comparator does. Built here, on the calling thread, because
    // building one compiles a region and the new thread has no VM to compile
    // with until it is inside one.
    let entry = callbacks.substitute(symbol, start)?;
    if entry == 0 {
        set_int_return(ctx, EINVAL);
        return Ok(());
    }

    let result = crate::thread::spawn(GuestStart { entry, arg, stack }, &host_attr);
    // SAFETY: as above, and nothing holds a reference to it past this point —
    // `pthread_create` copies what it needs out of an attribute set.
    unsafe { libc::pthread_attr_destroy(&mut host_attr) };

    match result {
        Ok(thread) => {
            mem.write_u64(out, thread as u64, symbol)?;
            set_int_return(ctx, 0);
        }
        Err(rc) => set_int_return(ctx, rc),
    }
    Ok(())
}

/// The guest's attribute set, copied into one the host may be handed.
///
/// `None` when the guest passed something that is not an initialised
/// `pthread_attr_t` — which the host's own `pthread_create` answers with
/// `EINVAL`, and which is asked here with a getter because the signature check
/// it makes is exactly the one wanted and is not otherwise reachable.
///
/// A copy rather than the guest's pointer, though the layouts are identical and
/// the memory is identity-mapped: the host writes into an attribute set it is
/// given (`pthread_attr_setstacksize`, below), and the guest's is the guest's.
/// Reading it through [`GuestMemory`] also validates the pointer, so a null or
/// wild `attr` is a named error rather than a fault inside libpthread.
fn read_attr(
    symbol: &str,
    attr: u64,
    mem: &dyn GuestMemory,
) -> Result<Option<libc::pthread_attr_t>> {
    let mut copy = MaybeUninit::<libc::pthread_attr_t>::uninit();
    if attr == 0 {
        // No attributes: the host's own defaults, which is what libpthread
        // would have used — including its default *size*, 512 KiB on Darwin.
        // Reading it back out of a freshly initialised set rather than writing
        // that number down here is what makes a guest thread's stack the size
        // the same program gets natively, without this runtime having an
        // opinion about what that size is.
        // SAFETY: `pthread_attr_init` initialises the whole structure, which is
        // what makes the `assume_init` below sound.
        unsafe {
            libc::pthread_attr_init(copy.as_mut_ptr());
            return Ok(Some(copy.assume_init()));
        }
    }

    let bytes = mem.read_bytes(attr, size_of::<libc::pthread_attr_t>(), symbol)?;
    // SAFETY: `pthread_attr_t` is a `long` and an opaque byte array with no
    // padding, no pointers this runtime dereferences and no niches, so any
    // bytes are a valid value of it; whether they are a *meaningful* one is
    // what the getter below asks. The lengths agree by construction.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), copy.as_mut_ptr() as *mut u8, bytes.len());
    }
    // SAFETY: every byte was just written.
    let copy = unsafe { copy.assume_init() };

    let mut detach = 0;
    // SAFETY: a live attribute set and a live local; the getter checks the
    // structure's signature and writes nothing when it refuses.
    let ok = unsafe { libc::pthread_attr_getdetachstate(&copy, &mut detach) } == 0;
    Ok(ok.then_some(copy))
}

/// What `attr` says about the stack the *guest's* frames go on.
///
/// `getstackaddr` before `getstacksize`, and no case analysis between
/// `setstack` and `setstackaddr`: both leave the top in the same field. See the
/// module note, which records how that was established.
fn stack_request(attr: &libc::pthread_attr_t) -> StackRequest {
    let mut top: *mut c_void = core::ptr::null_mut();
    let mut size: usize = 0;
    // SAFETY: a live attribute set and two live locals. Both getters are
    // infallible for an initialised set, which `read_attr` established.
    unsafe {
        libc::pthread_attr_getstackaddr(attr, &mut top);
        libc::pthread_attr_getstacksize(attr, &mut size);
    }
    match (top as u64, size as u64) {
        // A supplied stack of no size is not a stack; fall back rather than
        // hand a thread a zero-byte range to run on.
        (top, size) if top != 0 && size != 0 => StackRequest::Supplied { top, size },
        (_, 0) => StackRequest::Default,
        (_, size) => StackRequest::Sized(size as usize),
    }
}

pub type Shim = crate::signal::Shim;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "pthread_create" => Some(pthread_create),
        // Off unless asked for: this turns every lock in the program into a VM
        // exit, which is not a price to pay for a diagnostic nobody is reading.
        // `ROSETTA3_DEBUG_MUTEX=1` is for one question — *which* mutex, and what
        // is in it — and a failed `pthread_mutex_lock` is otherwise invisible:
        // libc++ turns it into a `std::system_error` whose message names no
        // address, and a guest that does not catch it dies several frames later
        // in its own terminate handler.
        "pthread_mutex_lock" if debug_mutex() => Some(mutex_lock),
        _ => None,
    }
}

/// Whether `ROSETTA3_DEBUG_MUTEX` asked for the lock diagnostic. Read once, for
/// [`crate::trace::watching`]'s reason: the lift-time decision and the run-time
/// decision have to agree.
fn debug_mutex() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ROSETTA3_DEBUG_MUTEX").is_some())
}

/// `int pthread_mutex_lock(pthread_mutex_t *)`, reporting a failure by address.
///
/// The first eight bytes are printed because that is where the answer usually
/// is: Darwin's `pthread_mutex_t` starts with a signature word, `EINVAL` means
/// it is not one libpthread recognises, and the two interesting wrong values are
/// **zero** — memory that was never constructed, or was `calloc`ed over a mutex
/// — and anything else, which is a mutex something has written through.
fn mutex_lock(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    let at = ctx.rdi;
    // SAFETY: the mutex is guest memory, which is this process's memory, and
    // `pthread_mutex_lock` is the host's own — the passthrough the table
    // declares, performed here so its result can be looked at.
    let rc = unsafe { libc::pthread_mutex_lock(at as *mut libc::pthread_mutex_t) };
    if rc != 0 {
        let mut head = [0u8; 16];
        let read = mem.read(at, &mut head).is_ok();
        eprintln!(
            "rosetta3: {symbol}({at:#x}) failed with {rc}; first 16 bytes {}",
            match read {
                true => head.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
                false => "unreadable".to_string(),
            }
        );
    }
    ctx.rax = rc as u32 as u64;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        /// Not in the `libc` crate, and one of the two setters this module's
        /// claim about "which end of the range" is about — so the test that
        /// measures it has to declare it.
        fn pthread_attr_setstack(
            attr: *mut libc::pthread_attr_t,
            stackaddr: *mut c_void,
            stacksize: libc::size_t,
        ) -> libc::c_int;
    }

    /// An attribute set the host built, as the guest's would be.
    fn attr() -> libc::pthread_attr_t {
        let mut a = MaybeUninit::<libc::pthread_attr_t>::uninit();
        unsafe {
            libc::pthread_attr_init(a.as_mut_ptr());
            a.assume_init()
        }
    }

    #[test]
    fn a_default_attribute_set_asks_for_the_size_darwin_would_give() {
        // Not `Default`: an initialised attribute set really does carry a size —
        // 512 KiB on Darwin — and honouring it is what makes a guest thread's
        // stack the size the same program gets natively. `Default` is for the
        // guest that passed no attributes at all.
        let a = attr();
        assert!(
            matches!(stack_request(&a), StackRequest::Sized(n) if n >= 16 * 1024),
            "{:?}",
            stack_request(&a)
        );
    }

    #[test]
    fn a_requested_size_is_read_back_as_the_guests_own_stack() {
        let mut a = attr();
        unsafe { libc::pthread_attr_setstacksize(&mut a, 1024 * 1024) };
        assert_eq!(stack_request(&a), StackRequest::Sized(1024 * 1024));
    }

    /// The measurement the module note rests on, kept as a test: both ways of
    /// supplying a stack name the same top, and it is the *high* address.
    ///
    /// If a future libpthread ever disagreed, a guest's thread would run its
    /// frames below the buffer it supplied — silently, on somebody else's
    /// memory — so this is checked against the host rather than assumed.
    #[test]
    fn a_supplied_stack_is_read_as_its_high_address_whichever_setter_named_it() {
        let page = machish::memory::host_page_size();
        let base = 0x2_0000_0000u64;
        let size = 256 * 1024;
        assert_eq!(base % page, 0);

        let mut by_stack = attr();
        unsafe { pthread_attr_setstack(&mut by_stack, base as *mut c_void, size as usize) };
        assert_eq!(
            stack_request(&by_stack),
            StackRequest::Supplied {
                top: base + size,
                size
            },
            "pthread_attr_setstack takes the low address"
        );

        let mut by_addr = attr();
        unsafe {
            libc::pthread_attr_setstackaddr(&mut by_addr, (base + size) as *mut c_void);
            libc::pthread_attr_setstacksize(&mut by_addr, size as usize);
        }
        assert_eq!(
            stack_request(&by_addr),
            StackRequest::Supplied {
                top: base + size,
                size
            },
            "pthread_attr_setstackaddr takes the high address"
        );
    }

    #[test]
    fn a_null_attribute_pointer_is_the_defaults_and_garbage_is_refused() {
        struct Bytes(Vec<u8>);
        impl GuestMemory for Bytes {
            fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
                let at = va as usize;
                let end = at + out.len();
                if end > self.0.len() {
                    return Err(machish::Error::OutOfBounds {
                        addr: va,
                        len: out.len(),
                    });
                }
                out.copy_from_slice(&self.0[at..end]);
                Ok(())
            }
            fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
                Err(machish::Error::OutOfBounds {
                    addr: va,
                    len: data.len(),
                })
            }
        }

        // Address 8 rather than 0, because zero is the guest saying "no
        // attributes" and never reaches memory at all.
        const AT: u64 = 8;
        let width = size_of::<libc::pthread_attr_t>();
        let mut bytes = vec![0u8; AT as usize + width];

        assert!(
            read_attr("pthread_create", 0, &Bytes(bytes.clone()))
                .expect("a null attr touches no memory")
                .is_some(),
            "a null attr is the defaults, which is what the host does"
        );

        // Zeroed bytes are not an initialised attribute set — the signature
        // word is wrong — and `EINVAL` is what the host answers for one.
        assert!(
            read_attr("pthread_create", AT, &Bytes(bytes.clone()))
                .expect("the bytes are readable")
                .is_none(),
            "an uninitialised attribute set must be refused, not used"
        );

        // The same bytes, initialised by the host: accepted, and carrying what
        // the guest set — which is the whole claim that the layouts agree.
        let mut initialised = attr();
        unsafe { libc::pthread_attr_setstacksize(&mut initialised, 512 * 1024) };
        let raw =
            unsafe { core::slice::from_raw_parts((&raw const initialised) as *const u8, width) };
        bytes[AT as usize..].copy_from_slice(raw);
        let copied = read_attr("pthread_create", AT, &Bytes(bytes))
            .expect("readable")
            .expect("an initialised attribute set is accepted");
        assert_eq!(stack_request(&copied), StackRequest::Sized(512 * 1024));
    }
}
