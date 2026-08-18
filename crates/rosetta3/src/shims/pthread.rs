//! `pthread_create`, and the two stacks a guest thread really has.

use core::ffi::c_void;
use core::mem::MaybeUninit;

use microlift::arch::x86_64::context::RegisterContext;
use static_init::dynamic;

use crate::abi::{Callbacks, GuestMemory, Result};
use crate::thread::{GuestStart, StackRequest};

use super::set_int_return;

/// `EINVAL`, the one failure this shim produces without asking the host.
const EINVAL: i32 = 22;

/// The stack the *host* thread gets: the runtime's own frames, and the JIT's.
///
/// The main thread's size, and deliberately not the guest's request.
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
        // Only when the guest supplied an address, and only then.
        if matches!(stack, StackRequest::Supplied { .. }) {
            libc::pthread_attr_setstackaddr(&mut host_attr, core::ptr::null_mut());
        }
    }

    // The start routine is guest code and crosses out as a thunk, exactly as
    // `qsort`'s comparator does.
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
/// A copy rather than the guest's pointer, though the layouts are identical and the
/// memory is identity-mapped.
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
        "pthread_mutex_lock" if debug_mutex() => Some(mutex_lock),
        _ => None,
    }
}

/// Whether `ROSETTA3_DEBUG_MUTEX` asked for the lock diagnostic. Read once, for
/// [`crate::trace::watching`]'s reason.
fn debug_mutex() -> bool {
    #[dynamic]
    static ON: bool = std::env::var_os("ROSETTA3_DEBUG_MUTEX").is_some();
    *ON
}

/// `int pthread_mutex_lock(pthread_mutex_t *)`, reporting a failure by address.
///
/// The first eight bytes are printed because that is where the answer usually is.
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
                true => head
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" "),
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
        // Not `Default`: an initialised attribute set really does carry a size
        // — 512 KiB on Darwin — and honouring it is what makes a guest thread's
        // stack the size the same program gets natively.
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
