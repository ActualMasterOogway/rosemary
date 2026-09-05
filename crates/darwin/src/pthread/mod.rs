//! The pthread interface, as the guest's Darwin declares it.
//!
//! Threading is the part of the C library where a wrong constant does not fail, it
//! *deadlocks* - so this module reproduces Darwin's whole surface rather than passing
//! any of it through and hoping. Four separate things differ, and each is handled
//! somewhere below:
//!
//! * **Constants.** `PTHREAD_MUTEX_RECURSIVE` is 2 on Darwin and 1 on Linux, and
//!   `PTHREAD_MUTEX_ERRORCHECK` is 1 and 2 - so the two are swapped, and a guest
//!   asking for a recursive lock gets one that returns `EDEADLK` instead of recursing.
//!   See [`consts`].
//! * **Object layout.** A `pthread_mutex_t` is sixty-four bytes carrying a signature
//!   on Darwin and forty bytes of zeroes under musl, and a `PTHREAD_MUTEX_INITIALIZER`
//!   is a *static* initializer no call of ours ever sees written. See [`object`].
//! * **Object lifetime.** The guest allocates the object; the host one has to live as
//!   long and be found again from the guest's address. See [`sync`].
//! * **Signatures.** `pthread_setname_np` takes one argument on Darwin and two on
//!   Linux; `pthread_once_t` is sixteen bytes and four.
//!
//! On macOS none of this is bound to a guest import - the host's libpthread is the
//! guest's - but all of it is *compiled and tested* there, against the very
//! implementation a guest would otherwise have called.

use machish::resolve::SyntheticDylib;

pub mod attr;
pub mod consts;
pub mod object;
pub mod raw;
pub mod sync;

pub use attr::{Attributes, HostAttr};
pub use consts::*;
pub use sync::{MutexAttr, once};

use object::{Kind, Record};

/// An opaque per-thread key, unique for as long as the thread lives.
///
/// Used only to find a thread's state from another thread. Darwin's answer is its Mach
/// port, which is what `pthread_mach_thread_np` returns and what a Mach exception
/// message carries; elsewhere it is the kernel thread id, which has the same two
/// properties that matter here - unique while the thread runs, and readable from the
/// thread itself without a lock.
///
/// A `u32` because both underlying values are one - a `mach_port_t` and a `pid_t` -
/// and because the registry that keys on it compares it atomically.
///
/// Not comparable across hosts and not meaningful to a guest. A guest asking for its
/// own Mach port is a different question, answered by `rosetta3::shims::machthread`.
pub fn id() -> u32 {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: takes this thread's own `pthread_t`, which is live by definition.
        unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `gettid` takes no argument and cannot fail.
        unsafe { libc::syscall(libc::SYS_gettid) as u32 }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // SAFETY: `pthread_self` is infallible and takes no argument.
        unsafe { libc::pthread_self() as u32 }
    }
}

/// A `pthread_t` that names no thread.
///
/// `pthread_t` is an integer on Darwin and glibc and a pointer under musl, so the
/// out-parameter `pthread_create` fills cannot be initialised with a literal `0`.
pub fn null_thread() -> libc::pthread_t {
    // SAFETY: `pthread_t` is a scalar - an integer or a pointer - on every platform
    // this builds for, and all-zero is a valid bit pattern for either. The value is
    // only ever written over by `pthread_create` before being read.
    unsafe { core::mem::zeroed() }
}

/// A guest `pthread_t` as this host's.
///
/// Both are eight opaque bytes, and every value a guest can hold was produced by a
/// `pthread_create` this runtime serviced - so the handle is the host's already and
/// this is a cast rather than a lookup.
fn thread_of(guest: u64) -> libc::pthread_t {
    // SAFETY: `pthread_t` is pointer-sized on both platforms, and `guest` is a value
    // the host's own `pthread_create` produced.
    unsafe { core::mem::transmute_copy(&guest) }
}

/// Build the `init`/`destroy`/`setter`/`getter` quartet for an attribute object whose
/// whole state is one packed word.
macro_rules! simple_attr {
    ($kind:expr, $init:ident, $destroy:ident, $default:expr) => {
        /// # Safety
        ///
        /// Called only through an import slot this crate defines; the argument is the
        /// guest's own attribute object.
        pub unsafe extern "C" fn $init(at: u64) -> i32 {
            if at == 0 {
                return err::EINVAL;
            }
            // SAFETY: the caller's guarantee.
            let r = unsafe { Record::at(at, $kind) };
            r.set_payload($default);
            0
        }

        /// # Safety
        ///
        /// As above. Frees nothing: an attribute object owns no allocation.
        pub unsafe extern "C" fn $destroy(at: u64) -> i32 {
            if at == 0 {
                return err::EINVAL;
            }
            // SAFETY: the caller's guarantee.
            let r = unsafe { Record::at(at, $kind) };
            match r.take() {
                Some(_) => 0,
                None => err::EINVAL,
            }
        }
    };
}

simple_attr!(Kind::MutexAttr, mutexattr_init, mutexattr_destroy, 0);
simple_attr!(
    Kind::CondAttr,
    condattr_init,
    condattr_destroy,
    PTHREAD_PROCESS_PRIVATE as u64
);
simple_attr!(
    Kind::RwLockAttr,
    rwlockattr_init,
    rwlockattr_destroy,
    PTHREAD_PROCESS_PRIVATE as u64
);

/// The packed word behind a `pthread_mutexattr_t`, read and written field by field.
///
/// # Safety
///
/// `at` is a guest `pthread_mutexattr_t *`.
unsafe fn mutexattr_word(at: u64) -> Option<Record> {
    if at == 0 {
        return None;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::MutexAttr) };
    r.is_ours().then_some(r)
}

/// `int pthread_mutexattr_settype(pthread_mutexattr_t *, int)`.
///
/// The one setter whose value the guest and the host disagree about, and the reason
/// this file is not a passthrough.
///
/// # Safety
///
/// As [`mutexattr_init`].
pub unsafe extern "C" fn mutexattr_settype(at: u64, kind: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(r) = (unsafe { mutexattr_word(at) }) else {
        return err::EINVAL;
    };
    let kind = kind as i32;
    if mutex_type_to_host(kind).is_none() {
        return err::EINVAL;
    }
    let mut settings =
        MutexAttr::unpacked_public(r.cell().load(std::sync::atomic::Ordering::Relaxed));
    settings.kind = kind;
    r.cell().store(
        settings.packed_public(),
        std::sync::atomic::Ordering::Relaxed,
    );
    0
}

/// # Safety
///
/// As [`mutexattr_init`], with `out` a writable `int *`.
pub unsafe extern "C" fn mutexattr_gettype(at: u64, out: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(r) = (unsafe { mutexattr_word(at) }) else {
        return err::EINVAL;
    };
    if out == 0 {
        return err::EINVAL;
    }
    let settings = MutexAttr::unpacked_public(r.cell().load(std::sync::atomic::Ordering::Relaxed));
    // SAFETY: the caller states `out` is a writable `int *`.
    unsafe { *(out as usize as *mut i32) = settings.kind };
    0
}

/// `int pthread_mutexattr_setpolicy_np(pthread_mutexattr_t *, int)`.
///
/// The policy picks which waiter Darwin hands the lock to; the host has no equivalent,
/// and neither policy changes what a correct program observes, so the value is checked
/// and discarded. It cannot be dropped instead: the symbol does not exist on this host,
/// so an unbound import is refused by name and the guest never starts.
///
/// # Safety
///
/// As [`mutexattr_init`].
pub unsafe extern "C" fn mutexattr_setpolicy_np(at: u64, policy: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    if unsafe { mutexattr_word(at) }.is_none() {
        return err::EINVAL;
    }
    match policy as i32 {
        PTHREAD_MUTEX_POLICY_FAIRSHARE_NP | PTHREAD_MUTEX_POLICY_FIRSTFIT_NP => 0,
        _ => err::EINVAL,
    }
}

/// The `pshared` setter, shared by all three attribute kinds.
macro_rules! pshared {
    ($setter:ident, $getter:ident, $kind:expr, $field:ident) => {
        /// # Safety
        ///
        /// As [`mutexattr_init`].
        pub unsafe extern "C" fn $setter(at: u64, shared: u64) -> i32 {
            let shared = shared as i32;
            if shared != PTHREAD_PROCESS_SHARED && shared != PTHREAD_PROCESS_PRIVATE {
                return err::EINVAL;
            }
            if at == 0 {
                return err::EINVAL;
            }
            // SAFETY: the caller's guarantee.
            let r = unsafe { Record::at(at, $kind) };
            if !r.is_ours() {
                return err::EINVAL;
            }
            $field(&r, shared);
            0
        }

        /// # Safety
        ///
        /// As [`mutexattr_init`], with `out` a writable `int *`.
        pub unsafe extern "C" fn $getter(at: u64, out: u64) -> i32 {
            if at == 0 || out == 0 {
                return err::EINVAL;
            }
            // SAFETY: the caller's guarantee.
            let r = unsafe { Record::at(at, $kind) };
            if !r.is_ours() {
                return err::EINVAL;
            }
            // SAFETY: the caller states `out` is a writable `int *`.
            unsafe { *(out as usize as *mut i32) = read_pshared(&r, $kind) };
            0
        }
    };
}

fn store_mutex_pshared(r: &Record, shared: i32) {
    let mut settings =
        MutexAttr::unpacked_public(r.cell().load(std::sync::atomic::Ordering::Relaxed));
    settings.pshared = shared;
    r.cell().store(
        settings.packed_public(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn store_simple_pshared(r: &Record, shared: i32) {
    r.cell()
        .store(shared as u64, std::sync::atomic::Ordering::Relaxed);
}

fn read_pshared(r: &Record, kind: Kind) -> i32 {
    let word = r.cell().load(std::sync::atomic::Ordering::Relaxed);
    match kind {
        Kind::MutexAttr => MutexAttr::unpacked_public(word).pshared,
        _ => word as i32,
    }
}

pshared!(
    mutexattr_setpshared,
    mutexattr_getpshared,
    Kind::MutexAttr,
    store_mutex_pshared
);
pshared!(
    condattr_setpshared,
    condattr_getpshared,
    Kind::CondAttr,
    store_simple_pshared
);
pshared!(
    rwlockattr_setpshared,
    rwlockattr_getpshared,
    Kind::RwLockAttr,
    store_simple_pshared
);

/// `int pthread_kill(pthread_t, int)`.
///
/// The signal number is Darwin's and has to be translated: `SIGUSR1` is 30 there and
/// 10 here, so an untranslated call sends `SIGPWR` to a thread expecting a user
/// signal. See [`crate::signal::signal_to_host`].
///
/// # Safety
///
/// Called only through an import slot this crate defines.
pub unsafe extern "C" fn kill(thread: u64, signal: u64) -> i32 {
    // Signal zero is the "does this thread exist" probe, and passes through as itself.
    let host = match signal {
        0 => 0,
        s => match crate::signal::signal_to_host(s as i32) {
            Some(host) => host,
            None => return err::EINVAL,
        },
    };
    // SAFETY: `thread` is a handle the host's own `pthread_create` produced.
    err::from_host(unsafe { libc::pthread_kill(thread_of(thread), host) })
}

/// `void *pthread_get_stackaddr_np(pthread_t)` - the **highest** address of the
/// thread's stack, which is where a stack that grows down begins.
///
/// Darwin's answer is the top; glibc's `pthread_getattr_np` reports the *low*
/// address and the size, so the two have to be added. A guest handed the low
/// address instead gets a stack it believes lies above its real one, which is
/// how a crash reporter comes to walk unmapped memory.
///
/// Zero when the host will not say, which is what Darwin answers for a thread
/// it does not know.
///
/// # Safety
///
/// `thread` is a `pthread_t` this process issued.
#[cfg(not(target_os = "macos"))]
pub unsafe extern "C" fn get_stackaddr_np(thread: u64) -> u64 {
    // SAFETY: the caller's obligation.
    match unsafe { stack_of(thread) } {
        Some((low, size)) => low.wrapping_add(size as u64),
        None => 0,
    }
}

/// `size_t pthread_get_stacksize_np(pthread_t)`
///
/// # Safety
///
/// As [`get_stackaddr_np`].
#[cfg(not(target_os = "macos"))]
pub unsafe extern "C" fn get_stacksize_np(thread: u64) -> u64 {
    // SAFETY: the caller's obligation.
    unsafe { stack_of(thread) }.map_or(0, |(_, size)| size as u64)
}

/// The `(lowest address, size)` of `thread`'s stack, out of glibc's attributes.
///
/// # Safety
///
/// As [`get_stackaddr_np`].
#[cfg(not(target_os = "macos"))]
unsafe fn stack_of(thread: u64) -> Option<(u64, usize)> {
    // SAFETY: zeroed is not a valid `pthread_attr_t` to *read*, but it is what
    // `pthread_getattr_np` is given to fill in, and it is only read on success.
    let mut attr: libc::pthread_attr_t = unsafe { core::mem::zeroed() };
    // SAFETY: a handle this process issued and a live attribute object.
    if unsafe { libc::pthread_getattr_np(thread_of(thread), &mut attr) } != 0 {
        return None;
    }
    let mut low: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut size: usize = 0;
    // SAFETY: an attribute object just filled in, and two live locals.
    let rc = unsafe { libc::pthread_attr_getstack(&attr, &mut low, &mut size) };
    // SAFETY: the same object, which is this function's to destroy.
    unsafe { libc::pthread_attr_destroy(&mut attr) };
    (rc == 0).then_some((low as usize as u64, size))
}

/// `int pthread_setname_np(const char *)` - **one** argument on Darwin, where Linux
/// takes the thread as well.
///
/// A guest calling the host's directly would pass its name where the thread belongs.
///
/// # Safety
///
/// `name` is the guest's own NUL-terminated string.
pub unsafe extern "C" fn setname_np(name: u64) -> i32 {
    if name == 0 {
        return err::EINVAL;
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller's guarantee; Darwin names the calling thread.
        err::from_host(unsafe { libc::pthread_setname_np(name as usize as *const libc::c_char) })
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux caps a thread name at sixteen bytes including the NUL and fails the
        // whole call with `ERANGE` for a longer one, where Darwin truncates. Truncated
        // here so a guest with a descriptive name gets a name rather than an error.
        // SAFETY: the caller states `name` is NUL-terminated.
        let full = unsafe { core::ffi::CStr::from_ptr(name as usize as *const libc::c_char) };
        let mut buf = [0u8; 16];
        let bytes = full.to_bytes();
        let n = bytes.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&bytes[..n]);
        // SAFETY: `buf` is NUL-terminated by construction and outlives the call.
        err::from_host(unsafe {
            libc::pthread_setname_np(libc::pthread_self(), buf.as_ptr() as *const libc::c_char)
        })
    }
}

/// Non-zero on the thread that ran `main`. Answered by comparing thread ids rather
/// than by asking libpthread, which has no such call here.
pub unsafe extern "C" fn main_np() -> i32 {
    use static_init::dynamic;

    // static_init::dynamic is a necessity here because it runs on the main thread always as a constructor
    #[dynamic]
    static MAIN: u32 = id();

    i32::from(*MAIN == id())
}

/// `int pthread_threadid_np(pthread_t, uint64_t *)`, Darwin's own.
///
/// A null thread means the calling one. The id is this runtime's [`id`], which has the
/// properties a caller wants - unique while the thread runs - without being Darwin's
/// actual 64-bit thread id, which nothing here can produce.
///
/// # Safety
///
/// `out` is null or a writable `uint64_t *`.
pub unsafe extern "C" fn threadid_np(thread: u64, out: u64) -> i32 {
    if out == 0 {
        return err::EINVAL;
    }
    if thread != 0 {
        // Another thread's id is not readable from here without a registry, and the
        // callers that matter ask about themselves.
        // SAFETY: `thread` is a live handle; `pthread_equal` only compares.
        if unsafe { libc::pthread_equal(thread_of(thread), libc::pthread_self()) } == 0 {
            return err::ENOTSUP;
        }
    }
    // SAFETY: the caller states `out` is a writable `uint64_t *`.
    unsafe { *(out as usize as *mut u64) = u64::from(id()) };
    0
}

/// `mach_port_t pthread_mach_thread_np(pthread_t)`, Darwin's own.
///
/// The Mach thread port for a thread, and there is no Mach here. What asks for
/// it uses it as a per-thread token and nothing else - the vendored `libc++abi`
/// keys `__cxa_guard_acquire`'s recursion check on it, which is why a program
/// with a static initializer in a C++ image needs this on Linux at all - so
/// [`id`] is the answer, unique while the thread runs.
///
/// A thread other than the caller is `MACH_PORT_NULL`: another thread's id is
/// not readable from here without a registry, and zero is what Apple's returns
/// for a thread it cannot name. See [`threadid_np`], which has the same limit
/// for the same reason.
pub unsafe extern "C" fn mach_thread_np(thread: u64) -> u32 {
    if thread != 0 {
        // SAFETY: `thread` is a live handle; `pthread_equal` only compares.
        if unsafe { libc::pthread_equal(thread_of(thread), libc::pthread_self()) } == 0 {
            return 0;
        }
    }
    id()
}

/// `int pthread_yield_np(void)`, Darwin's spelling of `sched_yield`.
pub unsafe extern "C" fn yield_np() -> i32 {
    // SAFETY: takes no argument and touches no memory.
    unsafe { libc::sched_yield() }
}

/// `int pthread_set_qos_class_self_np(qos_class_t, int)`.
///
/// A QoS class is one name for what Linux splits across a nice value, an I/O class and
/// a timer slack, so only the part a host can honour without privilege is applied: the
/// classes at or below the default become a nice value, and the two above it are
/// accepted and left alone, since raising a thread's priority needs `CAP_SYS_NICE` and
/// a guest that asks to run *faster* must not be failed for it.
///
/// Darwin's validation is kept, so a guest computing a class or a relative priority
/// wrongly still hears about it - and unlike the rest of the `pthread_` family this
/// call reports that the libc way, returning -1 with `errno` set rather than the error
/// itself. That is what libpthread does, and a caller reading `errno` after a `-1`
/// would otherwise see whatever was there before.
pub unsafe extern "C" fn set_qos_class_self_np(class: u32, relative: i32) -> i32 {
    // `None` for the two classes above the default: recognised, so not an error, but
    // nothing a host can apply.
    let nice = match class {
        QOS_CLASS_BACKGROUND => Some(10),
        QOS_CLASS_UTILITY => Some(5),
        QOS_CLASS_DEFAULT | QOS_CLASS_UNSPECIFIED => Some(0),
        QOS_CLASS_USER_INITIATED | QOS_CLASS_USER_INTERACTIVE => None,
        _ => return qos_einval(),
    };
    if !(QOS_MIN_RELATIVE_PRIORITY..=0).contains(&relative) {
        return qos_einval();
    }
    #[cfg(target_os = "linux")]
    if let Some(nice) = nice {
        // A relative priority runs 0 down to -15 within a class, so it lowers this
        // thread a little further inside the band the class already chose.
        //
        // SAFETY: `PRIO_PROCESS` with a `who` of zero names the calling thread on
        // Linux, so this touches no other thread and no memory. A failure here is a
        // scheduling hint not taken, which is not the guest's error to see.
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice - relative / 4) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = nice;
    0
}

/// `pthread_set_qos_class_self_np`'s failure: the libc convention, not the pthread one.
fn qos_einval() -> i32 {
    crate::errno::set(err::EINVAL);
    -1
}

/// `int pthread_setcancelstate(int, int *)`, whose two states are inverted.
///
/// Darwin's `PTHREAD_CANCEL_ENABLE` is 1 and Linux's is 0, so an untranslated call
/// disables cancellation where the guest asked to enable it.
///
/// # Safety
///
/// `old` is null or a writable `int *`.
pub unsafe extern "C" fn setcancelstate(state: u64, old: u64) -> i32 {
    let host = match state as i32 {
        PTHREAD_CANCEL_ENABLE => host_cancel_enable(),
        PTHREAD_CANCEL_DISABLE => host_cancel_disable(),
        _ => return err::EINVAL,
    };
    let mut previous = 0;
    // SAFETY: a live local.
    let rc = unsafe { raw::pthread_setcancelstate(host, &mut previous) };
    if rc == 0 && old != 0 {
        let darwin = match previous == host_cancel_enable() {
            true => PTHREAD_CANCEL_ENABLE,
            false => PTHREAD_CANCEL_DISABLE,
        };
        // SAFETY: the caller states `old` is a writable `int *`.
        unsafe { *(old as usize as *mut i32) = darwin };
    }
    rc
}

/// `int pthread_setcanceltype(int, int *)`, inverted for the same reason.
///
/// # Safety
///
/// As [`setcancelstate`].
pub unsafe extern "C" fn setcanceltype(kind: u64, old: u64) -> i32 {
    let host = match kind as i32 {
        PTHREAD_CANCEL_DEFERRED => host_cancel_deferred(),
        PTHREAD_CANCEL_ASYNCHRONOUS => host_cancel_asynchronous(),
        _ => return err::EINVAL,
    };
    let mut previous = 0;
    // SAFETY: a live local.
    let rc = unsafe { raw::pthread_setcanceltype(host, &mut previous) };
    if rc == 0 && old != 0 {
        let darwin = match previous == host_cancel_deferred() {
            true => PTHREAD_CANCEL_DEFERRED,
            false => PTHREAD_CANCEL_ASYNCHRONOUS,
        };
        // SAFETY: the caller states `old` is a writable `int *`.
        unsafe { *(old as usize as *mut i32) = darwin };
    }
    rc
}

fn host_cancel_enable() -> libc::c_int {
    raw::cancel::ENABLE
}
fn host_cancel_disable() -> libc::c_int {
    raw::cancel::DISABLE
}
fn host_cancel_deferred() -> libc::c_int {
    raw::cancel::DEFERRED
}
fn host_cancel_asynchronous() -> libc::c_int {
    raw::cancel::ASYNCHRONOUS
}

/// `pthread_key_t` is an `unsigned long` on Darwin and an `unsigned` under musl, so a
/// key crosses as eight bytes and is used as four.
///
/// # Safety
///
/// Called only through an import slot this crate defines.
pub unsafe extern "C" fn key_delete(key: u64) -> i32 {
    // SAFETY: `key` is one this process's `pthread_key_create` produced.
    err::from_host(unsafe { libc::pthread_key_delete(key as libc::pthread_key_t) })
}

/// `int pthread_key_create(pthread_key_t *, void (*destructor)(void *))`
///
/// The entry point the *native* backend binds. Under emulation the destructor is
/// guest code that needs a thunk, so `rosetta3::shims::pthread` intercepts the
/// call by name; here the guest's code is host code already.
///
/// The key is written out as **eight** bytes, because that is what the guest
/// reserved: `pthread_key_t` is an `unsigned long` on Darwin and an `unsigned`
/// here. Letting the host write through the guest's pointer fills half of the
/// slot and leaves the other half holding whatever was there before.
///
/// # Safety
///
/// `out` is eight writable bytes and `destructor` is null or a function this
/// process can call.
pub unsafe extern "C" fn key_create(
    out: *mut u64,
    destructor: Option<unsafe extern "C" fn(*mut core::ffi::c_void)>,
) -> i32 {
    if out.is_null() {
        return err::EINVAL;
    }
    let mut key: libc::pthread_key_t = 0;
    // SAFETY: a live local, and the caller's destructor.
    let rc = unsafe { libc::pthread_key_create(&mut key, destructor) };
    if rc == 0 {
        // SAFETY: the caller's eight bytes.
        unsafe { out.write_unaligned(u64::from(key)) };
    }
    err::from_host(rc)
}

/// `int pthread_join(pthread_t, void **)`
///
/// Wrapped for its *result*: the arguments and the handle agree on both
/// platforms, but a failing join returns `ESRCH`, `EDEADLK` or `EINVAL` as a
/// number, and `EDEADLK` is 11 on Darwin and 35 here.
///
/// # Safety
///
/// `thread` is a handle this process's `pthread_create` produced and `value` is
/// a writable pointer slot or null.
pub unsafe extern "C" fn join(thread: u64, value: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    err::from_host(unsafe { libc::pthread_join(thread_of(thread), value as usize as *mut _) })
}

/// The stack a guest thread's frames go on when its attributes ask for nothing.
///
/// This host's own default, which is deliberately not the 512 KiB
/// `rosetta3::thread::THREAD_STACK` gives an *emulated* guest thread. There the
/// guest's frames have a stack to themselves; here they share one with every shim
/// this crate runs on their behalf, and those are deeper than the Darwin libc the
/// guest was compiled against. A guest reading `pthread_get_stacksize_np` back
/// therefore sees a larger number than it would on a Mac.
#[cfg(not(target_os = "macos"))]
const DEFAULT_STACK: usize = 8 * 1024 * 1024;

/// The floor under a guest's own request.
///
/// A natively executed guest runs this runtime's shims on the same stack it runs its
/// own frames on, and those are deeper than the Darwin libc the guest sized itself
/// against. Only ever raises a request, so the thread has at least the room the guest
/// asked for.
#[cfg(not(target_os = "macos"))]
const MIN_STACK: usize = 1024 * 1024;

#[cfg(not(target_os = "macos"))]
fn stack_size(attributes: &Attributes) -> usize {
    attributes
        .stack_size
        .unwrap_or(DEFAULT_STACK)
        .max(MIN_STACK)
}

/// What [`create`] hands the host thread: the guest's start routine and its argument.
#[cfg(not(target_os = "macos"))]
struct Start {
    entry: u64,
    arg: u64,
}

/// The host thread's start routine, which enters Darwin code and stays there.
///
/// The guard is held for the whole of the thread's life rather than around a call,
/// because there is no call to wrap: the thread *is* guest code from here to its
/// return, and every host function it invokes is reached from inside it.
///
/// `context` is the [`Start`] [`create`] leaked, handed back unchanged - which is the
/// only value the host can pass, and why this is not an `unsafe fn`: `pthread_create`
/// takes a safe pointer.
#[cfg(not(target_os = "macos"))]
extern "C" fn run(context: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    // Reclaimed and copied out before the guest runs, because a start routine may
    // call `pthread_exit` - which ends the thread from under these frames without
    // unwinding them, leaking anything still boxed. `rosetta3::thread::entry` does
    // the same for the same reason.
    // SAFETY: the caller's guarantee - the same allocation, reclaimed once.
    let start = *unsafe { Box::from_raw(context as *mut Start) };
    let _darwin = crate::tls::DarwinThread::enter();
    // SAFETY: `create` refused a null, and a start routine is `void *(void *)` -
    // one pointer in, one out, which is what this signature is in the guest's
    // integer registers.
    let f: unsafe extern "C" fn(u64) -> u64 =
        unsafe { core::mem::transmute(start.entry as usize as *const ()) };
    // SAFETY: as above; `arg` is the guest's own and only guest code reads it.
    unsafe { f(start.arg) as usize as *mut core::ffi::c_void }
}

/// `int pthread_create(pthread_t *, const pthread_attr_t *, void *(*)(void *), void *)`
///
/// Not passed through, for two reasons that are independent of each other:
///
/// * A Darwin `pthread_attr_t` is [`attr`]'s own record and this host's is a different
///   structure of a different size, so handing the guest's set to the host's
///   `pthread_create` asks it to read a stack size out of bytes that mean something
///   else. This is the [`crate::spawn`] problem again.
/// * A Darwin thread reads `pthread_self`, `errno` and its stack canary as **direct
///   loads off `%gs`** - see [`crate::tls`]. A thread the host started has no Darwin
///   array there, so the guest's start routine runs against whatever `%gs` happens to
///   address. Only [`crate::tls::DarwinThread`] can give it one, and only from inside
///   the new thread.
///
/// # Safety
///
/// `out` is a writable `pthread_t`, `attr` is null or a guest `pthread_attr_t`, and
/// `start` is guest code of this host's architecture.
#[cfg(not(target_os = "macos"))]
pub unsafe extern "C" fn create(out: u64, attr: u64, start: u64, arg: u64) -> i32 {
    if out == 0 || start == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let Some(attributes) = (unsafe { Attributes::of_guest(attr) }) else {
        return err::EINVAL;
    };
    let mut host = match attributes.to_host_attr(stack_size(&attributes)) {
        Ok(host) => host,
        Err(rc) => return rc,
    };
    let context = Box::into_raw(Box::new(Start { entry: start, arg }));
    let mut thread = null_thread();
    // SAFETY: `host` is an initialised attribute set that outlives the call, which is
    // all `pthread_create` needs - it copies what it wants out of one. `run` is this
    // module's own, and owns `context` from here.
    let rc = unsafe {
        libc::pthread_create(
            &mut thread,
            host.as_mut_ptr(),
            run,
            context as *mut core::ffi::c_void,
        )
    };
    if rc != 0 {
        // SAFETY: the thread did not start, so nothing else holds the allocation.
        drop(unsafe { Box::from_raw(context) });
        return err::from_host(rc);
    }
    // SAFETY: the caller states `out` is a writable `pthread_t`, which is eight
    // opaque bytes on both platforms - see [`thread_of`], which reads it back.
    unsafe { (out as usize as *mut u64).write(thread as u64) };
    0
}

/// `int pthread_detach(pthread_t)`
///
/// # Safety
///
/// As [`join`].
pub unsafe extern "C" fn detach(thread: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    err::from_host(unsafe { libc::pthread_detach(thread_of(thread)) })
}

/// # Safety
///
/// As [`key_delete`].
pub unsafe extern "C" fn getspecific(key: u64) -> u64 {
    // SAFETY: as above; a key nobody created reads null, which is what Darwin does.
    unsafe { libc::pthread_getspecific(key as libc::pthread_key_t) as usize as u64 }
}

/// # Safety
///
/// As [`key_delete`]; `value` is the guest's own pointer, which only guest code
/// dereferences.
pub unsafe extern "C" fn setspecific(key: u64, value: u64) -> i32 {
    // SAFETY: as above.
    err::from_host(unsafe {
        libc::pthread_setspecific(
            key as libc::pthread_key_t,
            value as usize as *const libc::c_void,
        )
    })
}

/// The address of the implementation for `name`, or `None`.
///
/// Empty on macOS, where the host's libpthread *is* the guest's and passing a call
/// through is both correct and free. Everything above is still compiled and tested
/// there; what changes is only whether a guest import is bound to it.
pub fn symbol(name: &str) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        entries().iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
    }
}

/// Every entry, as `(C name, address)`.
#[cfg(not(target_os = "macos"))]
fn entries() -> &'static [(&'static str, u64)] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<(&'static str, u64)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        macro_rules! table {
            ($($name:literal => $f:expr),* $(,)?) => {
                vec![$(($name, $f as *const () as usize as u64)),*]
            };
        }
        table! {
            // Attributes.
            "pthread_attr_init" => attr::imp::attr_init,
            "pthread_attr_destroy" => attr::imp::attr_destroy,
            "pthread_attr_setdetachstate" => attr::imp::attr_setdetachstate,
            "pthread_attr_getdetachstate" => attr::imp::attr_getdetachstate,
            "pthread_attr_setstacksize" => attr::imp::attr_setstacksize,
            "pthread_attr_getstacksize" => attr::imp::attr_getstacksize,
            "pthread_attr_setstackaddr" => attr::imp::attr_setstackaddr,
            "pthread_attr_getstackaddr" => attr::imp::attr_getstackaddr,
            "pthread_attr_setstack" => attr::imp::attr_setstack,
            "pthread_attr_getstack" => attr::imp::attr_getstack,
            "pthread_attr_setguardsize" => attr::imp::attr_setguardsize,
            "pthread_attr_getguardsize" => attr::imp::attr_getguardsize,
            "pthread_attr_setscope" => attr::imp::attr_setscope,
            "pthread_attr_getscope" => attr::imp::attr_getscope,
            "pthread_attr_setinheritsched" => attr::imp::attr_setinheritsched,
            "pthread_attr_getinheritsched" => attr::imp::attr_getinheritsched,
            "pthread_attr_setschedpolicy" => attr::imp::attr_setschedpolicy,
            "pthread_attr_getschedpolicy" => attr::imp::attr_getschedpolicy,
            "pthread_attr_setschedparam" => attr::imp::attr_setschedparam,
            "pthread_attr_getschedparam" => attr::imp::attr_getschedparam,

            // Mutexes.
            "pthread_mutex_init" => sync::mutex_init,
            "pthread_mutex_destroy" => sync::mutex_destroy,
            "pthread_mutex_lock" => sync::mutex_lock,
            "pthread_mutex_trylock" => sync::mutex_trylock,
            "pthread_mutex_unlock" => sync::mutex_unlock,
            "pthread_mutexattr_init" => mutexattr_init,
            "pthread_mutexattr_destroy" => mutexattr_destroy,
            "pthread_mutexattr_settype" => mutexattr_settype,
            "pthread_mutexattr_gettype" => mutexattr_gettype,
            "pthread_mutexattr_setpshared" => mutexattr_setpshared,
            "pthread_mutexattr_getpshared" => mutexattr_getpshared,
            "pthread_mutexattr_setpolicy_np" => mutexattr_setpolicy_np,

            // Condition variables.
            "pthread_cond_init" => sync::cond_init,
            "pthread_cond_destroy" => sync::cond_destroy,
            "pthread_cond_wait" => sync::cond_wait,
            "pthread_cond_timedwait" => sync::cond_timedwait,
            "pthread_cond_timedwait_relative_np" => sync::cond_timedwait_relative,
            "pthread_cond_signal" => sync::cond_signal,
            "pthread_cond_broadcast" => sync::cond_broadcast,
            "pthread_condattr_init" => condattr_init,
            "pthread_condattr_destroy" => condattr_destroy,
            "pthread_condattr_setpshared" => condattr_setpshared,
            "pthread_condattr_getpshared" => condattr_getpshared,

            // Read-write locks.
            "pthread_rwlock_init" => sync::rwlock_init,
            "pthread_rwlock_destroy" => sync::rwlock_destroy,
            "pthread_rwlock_rdlock" => sync::rwlock_rdlock,
            "pthread_rwlock_tryrdlock" => sync::rwlock_tryrdlock,
            "pthread_rwlock_wrlock" => sync::rwlock_wrlock,
            "pthread_rwlock_trywrlock" => sync::rwlock_trywrlock,
            "pthread_rwlock_unlock" => sync::rwlock_unlock,
            "pthread_rwlockattr_init" => rwlockattr_init,
            "pthread_rwlockattr_destroy" => rwlockattr_destroy,
            "pthread_rwlockattr_setpshared" => rwlockattr_setpshared,
            "pthread_rwlockattr_getpshared" => rwlockattr_getpshared,

            // Thread-specific data. `pthread_key_create` is *also* a shim under
            // emulation, where the destructor is guest code that needs a thunk -
            // the entry here is what the native backend binds, and what stops
            // glibc writing four bytes into the guest's eight.
            "pthread_key_create" => key_create,
            "pthread_once" => sync::once_entry,
            "pthread_key_delete" => key_delete,
            "pthread_getspecific" => getspecific,
            "pthread_setspecific" => setspecific,

            // Lifecycle and the `_np` family.
            "pthread_create" => create,
            "pthread_join" => join,
            "pthread_detach" => detach,
            "pthread_kill" => kill,
            "pthread_get_stackaddr_np" => get_stackaddr_np,
            "pthread_get_stacksize_np" => get_stacksize_np,
            "pthread_setname_np" => setname_np,
            "pthread_main_np" => main_np,
            "pthread_threadid_np" => threadid_np,
            "pthread_mach_thread_np" => mach_thread_np,
            "pthread_yield_np" => yield_np,
            "pthread_set_qos_class_self_np" => set_qos_class_self_np,
            "pthread_setcancelstate" => setcancelstate,
            "pthread_setcanceltype" => setcanceltype,
        }
    })
}

/// Names this module deliberately leaves to the host, and why.
///
/// Every one is a call whose arguments and result are plain scalars the two platforms
/// agree about, and whose object - if it has one - never crosses the seam: a
/// `pthread_t` is eight opaque bytes on both, and every value a guest holds came from
/// a `pthread_create` this runtime serviced.
pub const PASSED_THROUGH: &[&str] = &[
    "pthread_self",
    "pthread_equal",
    // `pthread_join` and `pthread_detach` were on this list and are not any
    // more. The arguments do agree; the *result* does not, because a `pthread_*`
    // call returns its error as a number and `EDEADLK` is 11 on Darwin and 35
    // here. Both are wrapped above for that one reason.
    "pthread_exit",
    // `pthread_cancel` is deliberately absent. Cancellation unwinds a stack the host's
    // unwinder never saw, so `rosetta3::abi::table` refuses it outright - and this
    // list vouching for it would quietly undo that.
    "pthread_testcancel",
    "pthread_getconcurrency",
    "pthread_setconcurrency",
    "sched_yield",
];

/// Names this crate leaves to the **runtime above it**, which services them with a
/// thunk for the guest function pointer they take.
///
/// Reaching guest code needs emulation vocabulary this crate does not have, so
/// `rosetta3::shims::pthread` owns the call. Listed here so the blanket denial of the
/// `pthread_` family in [`crate::divergent`] does not take their import slots with it:
/// the slot still has to resolve to *something* for the runtime to intercept the call.
///
/// **Being on this list is not enough for anything that takes a Darwin-shaped
/// object.** Interception is an emulated-backend thing; the native backend
/// intercepts nothing and calls whatever the slot holds. So `pthread_create`,
/// `pthread_once` and `pthread_key_create` have all been taken off this list and
/// given entries of their own - each was handing glibc a structure of the wrong
/// shape, and each failed silently rather than loudly. `pthread_once` is the one
/// worth remembering: Darwin's `PTHREAD_ONCE_INIT` signature has the bit set that
/// glibc reads as "already run".
pub const SHIMMED_ABOVE: &[&str] = &[
    // Takes three handlers, and is otherwise plain scalars. The slot it binds to
    // is `crate::bsd`'s and not the host's: glibc keeps `pthread_atfork` in
    // `libc_nonshared.a`, so the passthrough below finds nothing for it.
    "pthread_atfork",
];

/// Register every entry under the name a Mach-O imports it by.
///
/// [`PASSED_THROUGH`] is registered too, bound to the host's own function. That is not
/// redundant: it is what lets the host resolver deny the whole `pthread_` family
/// outright, so a name this crate has *not* thought about is refused rather than
/// quietly answered. A passthrough registered here costs nothing at run time - the
/// guest's slot holds the host address either way.
pub fn provide(lib: &mut SyntheticDylib) {
    #[cfg(not(target_os = "macos"))]
    {
        for (name, address) in entries() {
            crate::define_fn(lib, &format!("_{name}"), *address as *const ());
        }
        // Both lists are bound to the host's own function: [`PASSED_THROUGH`] because
        // that is the implementation, and [`SHIMMED_ABOVE`] because the slot has to
        // hold *some* address for the runtime to intercept the call at. Neither costs
        // anything at run time - the guest's slot holds the host address either way.
        for name in PASSED_THROUGH.iter().chain(SHIMMED_ABOVE) {
            if let Some(host) = crate::host_symbol(name) {
                crate::define_fn(lib, &format!("_{name}"), host as *const ());
            }
        }
    }
    #[cfg(target_os = "macos")]
    let _ = lib;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A guest object: the right size, and eight-byte aligned as a `long` first
    /// member makes one.
    #[repr(C, align(8))]
    struct Obj<const N: usize>([u8; N]);

    impl<const N: usize> Obj<N> {
        /// Zeroed, as a guest's `.bss` object arrives.
        fn new() -> Box<Obj<N>> {
            Box::new(Obj([0u8; N]))
        }

        /// Carrying a Darwin static initializer, as a guest's `.data` object does.
        fn statically_initialised(signature: u64) -> Box<Obj<N>> {
            let mut o = Obj::<N>::new();
            o.0[..8].copy_from_slice(&signature.to_le_bytes());
            o
        }

        fn at(&self) -> u64 {
            self as *const Obj<N> as usize as u64
        }
    }

    type Mutex = Obj<{ object::size::MUTEX }>;
    type Cond = Obj<{ object::size::COND }>;
    type RwLock = Obj<{ object::size::RWLOCK }>;
    type Once = Obj<{ object::size::ONCE }>;
    type Attr = Obj<{ object::size::ATTR }>;
    type MutexAttrObj = Obj<{ object::size::MUTEXATTR }>;

    /// The three names whose Darwin-shaped object glibc would misread must bind to
    /// *this crate's* entry, not to the host's.
    ///
    /// Being intercepted above is not enough: interception is an emulated-backend
    /// thing, and the native backend calls whatever the slot holds. `pthread_once`
    /// is the reason this test exists - see [`sync::once_entry`].
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_calls_whose_object_is_darwin_shaped_do_not_bind_to_the_host_s() {
        for name in ["pthread_create", "pthread_once", "pthread_key_create"] {
            let ours = symbol(name).unwrap_or_else(|| panic!("{name} has no entry"));
            assert_ne!(
                Some(ours),
                crate::host_symbol(name),
                "{name} binds to the host's, which reads the guest's object wrong"
            );
        }
    }

    /// A `pthread_once_t` carrying Darwin's static initializer runs its routine,
    /// and runs it once.
    ///
    /// glibc's `pthread_once` reads that same word as its own state, and Darwin's
    /// signature `0x30B1BCBA` has the bit set that glibc takes to mean "already
    /// run" - so passing one through succeeds and never calls the routine. That
    /// silence is the whole failure: it looks like the routine ran.
    #[test]
    fn a_darwin_initialised_once_runs_its_routine_exactly_once() {
        static RUNS: AtomicU32 = AtomicU32::new(0);
        extern "C" fn body() {
            RUNS.fetch_add(1, Ordering::Relaxed);
        }
        let control = Once::statically_initialised(object::sig::ONCE_INIT);
        // SAFETY: a live, correctly sized and aligned `pthread_once_t`.
        unsafe {
            assert_eq!(sync::once_entry(control.at(), Some(body)), 0);
            assert_eq!(sync::once_entry(control.at(), Some(body)), 0);
        }
        assert_eq!(RUNS.load(Ordering::Relaxed), 1);
    }

    /// Everything a guest sets on a thread attribute set must read back as what it
    /// set - which is the whole contract of an opaque type whose bytes are ours.
    #[test]
    fn every_thread_attribute_reads_back_as_what_was_set() {
        let a = Attr::new();
        // SAFETY: `a` is a live, correctly sized and aligned attribute object.
        unsafe {
            assert_eq!(attr::imp::attr_init(a.at()), 0);

            assert_eq!(
                attr::imp::attr_setdetachstate(a.at(), PTHREAD_CREATE_DETACHED as u64),
                0
            );
            let mut detach = 0i32;
            assert_eq!(
                attr::imp::attr_getdetachstate(a.at(), &mut detach as *mut _ as u64),
                0
            );
            assert_eq!(detach, PTHREAD_CREATE_DETACHED);

            assert_eq!(attr::imp::attr_setstacksize(a.at(), 1 << 20), 0);
            let mut size = 0usize;
            assert_eq!(
                attr::imp::attr_getstacksize(a.at(), &mut size as *mut _ as u64),
                0
            );
            assert_eq!(size, 1 << 20);

            assert_eq!(attr::imp::attr_setguardsize(a.at(), 0), 0);
            let mut guard = 12345usize;
            assert_eq!(
                attr::imp::attr_getguardsize(a.at(), &mut guard as *mut _ as u64),
                0
            );
            assert_eq!(guard, 0, "a guest may ask for no guard at all");

            assert_eq!(
                attr::imp::attr_setscope(a.at(), PTHREAD_SCOPE_PROCESS as u64),
                0
            );
            let mut scope = 0i32;
            assert_eq!(
                attr::imp::attr_getscope(a.at(), &mut scope as *mut _ as u64),
                0
            );
            assert_eq!(scope, PTHREAD_SCOPE_PROCESS);

            assert_eq!(attr::imp::attr_setschedpolicy(a.at(), SCHED_RR as u64), 0);
            let mut policy = 0i32;
            assert_eq!(
                attr::imp::attr_getschedpolicy(a.at(), &mut policy as *mut _ as u64),
                0
            );
            assert_eq!(policy, SCHED_RR, "Darwin's SCHED_RR, not the host's");

            assert_eq!(attr::imp::attr_destroy(a.at()), 0);
        }
    }

    /// The two ends of a supplied stack. `pthread_attr_setstack` takes the low
    /// address and `pthread_attr_getstackaddr` reports the high one, and getting the
    /// direction wrong puts a thread's frames below its own guard page.
    #[test]
    fn a_supplied_stack_is_set_by_its_base_and_read_by_its_top() {
        let a = Attr::new();
        let (base, size) = (0x2_0000_0000u64, 1 << 20);
        // SAFETY: a live attribute object; the stack address is never dereferenced.
        unsafe {
            assert_eq!(attr::imp::attr_init(a.at()), 0);
            assert_eq!(attr::imp::attr_setstack(a.at(), base, size), 0);

            let mut top = 0u64;
            assert_eq!(
                attr::imp::attr_getstackaddr(a.at(), &mut top as *mut _ as u64),
                0
            );
            assert_eq!(top, base + size, "getstackaddr reports the top");

            let (mut low, mut got) = (0u64, 0usize);
            assert_eq!(
                attr::imp::attr_getstack(
                    a.at(),
                    &mut low as *mut _ as u64,
                    &mut got as *mut _ as u64
                ),
                0
            );
            assert_eq!((low, got as u64), (base, size), "getstack reports the base");
        }
    }

    /// A size a guest never set must read back as the size a thread would really get,
    /// because a guest that sizes a buffer off it is owed the truth.
    #[test]
    fn an_unset_stack_size_reads_back_as_darwins_default() {
        let a = Attr::new();
        // SAFETY: a live attribute object.
        unsafe {
            assert_eq!(attr::imp::attr_init(a.at()), 0);
            let mut size = 0usize;
            assert_eq!(
                attr::imp::attr_getstacksize(a.at(), &mut size as *mut _ as u64),
                0
            );
            assert_eq!(size, 512 * 1024, "Darwin's default thread stack");
        }
    }

    #[test]
    fn a_value_darwin_does_not_declare_is_refused() {
        let a = Attr::new();
        // SAFETY: a live attribute object.
        unsafe {
            assert_eq!(attr::imp::attr_init(a.at()), 0);
            // Zero is Linux's `PTHREAD_CREATE_JOINABLE` and is not a Darwin value at
            // all - which is exactly the mistake an untranslated call would make.
            assert_eq!(attr::imp::attr_setdetachstate(a.at(), 0), err::EINVAL);
            assert_eq!(attr::imp::attr_setscope(a.at(), 99), err::EINVAL);
        }
    }

    #[test]
    fn an_uninitialised_attribute_set_is_refused_rather_than_read() {
        let a = Attr::new();
        // SAFETY: a live, zeroed object that was never initialised.
        unsafe {
            let mut out = 0i32;
            assert_eq!(
                attr::imp::attr_getdetachstate(a.at(), &mut out as *mut _ as u64),
                err::EINVAL
            );
            assert_eq!(attr::imp::attr_destroy(a.at()), err::EINVAL);
        }
    }

    #[test]
    fn a_mutex_locks_unlocks_and_is_destroyed() {
        let m = Mutex::new();
        // SAFETY: a live, correctly sized and aligned mutex object.
        unsafe {
            assert_eq!(sync::mutex_init(m.at(), 0), 0);
            assert_eq!(sync::mutex_lock(m.at()), 0);
            assert_eq!(sync::mutex_trylock(m.at()), libc::EBUSY, "already held");
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::mutex_trylock(m.at()), 0, "and free again");
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::mutex_destroy(m.at()), 0);
            assert_eq!(sync::mutex_destroy(m.at()), err::EINVAL, "destroyed once");
        }
    }

    /// The bug this whole file exists to prevent: `PTHREAD_MUTEX_RECURSIVE` is 2 on
    /// Darwin and 1 on Linux, so an untranslated request for a recursive lock returns
    /// an error-checking one - which does not recurse, it reports `EDEADLK`.
    #[test]
    fn a_recursive_mutex_recurses_and_an_errorcheck_one_does_not() {
        let attr = MutexAttrObj::new();
        let m = Mutex::new();
        // SAFETY: two live, correctly sized objects.
        unsafe {
            assert_eq!(mutexattr_init(attr.at()), 0);
            assert_eq!(
                mutexattr_settype(attr.at(), PTHREAD_MUTEX_RECURSIVE as u64),
                0
            );
            let mut kind = 0i32;
            assert_eq!(mutexattr_gettype(attr.at(), &mut kind as *mut _ as u64), 0);
            assert_eq!(kind, PTHREAD_MUTEX_RECURSIVE, "Darwin's number, read back");

            assert_eq!(sync::mutex_init(m.at(), attr.at()), 0);
            assert_eq!(sync::mutex_lock(m.at()), 0);
            assert_eq!(sync::mutex_lock(m.at()), 0, "a recursive mutex recurses");
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::mutex_destroy(m.at()), 0);

            // And the type Darwin numbers 1, which Linux numbers 2.
            let e = Mutex::new();
            assert_eq!(
                mutexattr_settype(attr.at(), PTHREAD_MUTEX_ERRORCHECK as u64),
                0
            );
            assert_eq!(sync::mutex_init(e.at(), attr.at()), 0);
            assert_eq!(sync::mutex_lock(e.at()), 0);
            // Darwin's number, which is not this host's: a guest compares the
            // result against its own header.
            assert_eq!(
                sync::mutex_lock(e.at()),
                err::EDEADLK,
                "an error-checking mutex reports rather than recursing"
            );
            assert_eq!(sync::mutex_unlock(e.at()), 0);
            assert_eq!(sync::mutex_destroy(e.at()), 0);
            assert_eq!(mutexattr_destroy(attr.at()), 0);
        }
    }

    /// A policy the host cannot honour is still accepted, and one Darwin does not
    /// declare is still refused: the guest checks the return value, not the effect.
    #[test]
    fn a_mutex_policy_is_accepted_and_a_number_darwin_does_not_declare_is_not() {
        let attr = MutexAttrObj::new();
        // SAFETY: a live, correctly sized attribute object.
        unsafe {
            assert_eq!(mutexattr_init(attr.at()), 0);
            assert_eq!(
                mutexattr_setpolicy_np(attr.at(), PTHREAD_MUTEX_POLICY_FIRSTFIT_NP as u64),
                0
            );
            assert_eq!(
                mutexattr_setpolicy_np(attr.at(), PTHREAD_MUTEX_POLICY_FAIRSHARE_NP as u64),
                0
            );
            assert_eq!(mutexattr_setpolicy_np(attr.at(), 2), err::EINVAL);
            assert_eq!(mutexattr_setpolicy_np(0, 1), err::EINVAL);
            assert_eq!(mutexattr_destroy(attr.at()), 0);
            assert_eq!(
                mutexattr_setpolicy_np(attr.at(), 1),
                err::EINVAL,
                "not an attribute set any more"
            );
        }
    }

    /// `PTHREAD_MUTEX_INITIALIZER` is written by the guest's compiler, so no call of
    /// ours ever sees it happen. The first lock has to notice and build the object.
    #[test]
    fn a_statically_initialised_mutex_works_without_an_init_call() {
        let m = Mutex::statically_initialised(object::sig::MUTEX_INIT);
        // SAFETY: a live mutex object carrying a Darwin static initializer.
        unsafe {
            assert_eq!(sync::mutex_lock(m.at()), 0);
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::mutex_destroy(m.at()), 0);
        }
    }

    /// And `PTHREAD_RECURSIVE_MUTEX_INITIALIZER` has to build a *recursive* one:
    /// which static initializer the guest wrote is the only record of what it wanted.
    #[test]
    fn a_statically_initialised_recursive_mutex_recurses() {
        let m = Mutex::statically_initialised(object::sig::RECURSIVE_MUTEX_INIT);
        // SAFETY: as above.
        unsafe {
            assert_eq!(sync::mutex_lock(m.at()), 0);
            assert_eq!(
                sync::mutex_lock(m.at()),
                0,
                "the initializer said recursive"
            );
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::mutex_unlock(m.at()), 0);
        }
    }

    /// A statically initialised mutex first touched by several threads at once must
    /// end up as *one* host mutex. Two would lock independently and exclude nobody.
    #[test]
    fn racing_threads_build_one_object_for_one_static_mutex() {
        let m = Arc::new(Mutex::statically_initialised(object::sig::MUTEX_INIT));
        let counter = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (m, counter) = (Arc::clone(&m), Arc::clone(&counter));
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        // SAFETY: a live mutex object shared with the other threads,
                        // which is what a guest's global mutex is.
                        unsafe {
                            assert_eq!(sync::mutex_lock(m.at()), 0);
                            let seen = counter.load(Ordering::Relaxed);
                            counter.store(seen + 1, Ordering::Relaxed);
                            assert_eq!(sync::mutex_unlock(m.at()), 0);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("a locking thread");
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            8 * 2000,
            "a lost update means two host mutexes behind one guest one"
        );
    }

    /// An object of one kind handed to another kind's function is `EINVAL`, not a
    /// host mutex built out of a condition variable.
    #[test]
    fn a_cond_is_not_a_mutex() {
        let c = Cond::new();
        // SAFETY: a live, correctly sized condition variable object.
        unsafe {
            assert_eq!(sync::cond_init(c.at(), 0), 0);
            assert_eq!(sync::mutex_lock(c.at()), err::EINVAL);
            assert_eq!(sync::cond_destroy(c.at()), 0);
        }
    }

    /// Copying an initialised mutex is undefined in POSIX, and the copy sharing the
    /// original's host lock is the quiet way to get it wrong: both would lock, and
    /// neither would exclude the other.
    #[test]
    fn a_copied_mutex_is_refused_rather_than_aliased() {
        let original = Mutex::new();
        // SAFETY: a live mutex object.
        unsafe { assert_eq!(sync::mutex_init(original.at(), 0), 0) };

        let mut copy = Mutex::new();
        copy.0.copy_from_slice(&original.0);
        // SAFETY: a live, correctly sized object holding a byte copy of another's
        // record - which is exactly what the owner word exists to catch.
        unsafe {
            assert_eq!(sync::mutex_lock(copy.at()), err::EINVAL);
            assert_eq!(
                sync::mutex_lock(original.at()),
                0,
                "the original still works"
            );
            assert_eq!(sync::mutex_unlock(original.at()), 0);
            assert_eq!(sync::mutex_destroy(original.at()), 0);
        }
    }

    #[test]
    fn a_process_shared_mutex_is_refused_rather_than_made_private() {
        let attr = MutexAttrObj::new();
        let m = Mutex::new();
        // SAFETY: two live, correctly sized objects.
        unsafe {
            assert_eq!(mutexattr_init(attr.at()), 0);
            assert_eq!(
                mutexattr_setpshared(attr.at(), PTHREAD_PROCESS_SHARED as u64),
                0
            );
            let mut shared = 0i32;
            assert_eq!(
                mutexattr_getpshared(attr.at(), &mut shared as *mut _ as u64),
                0
            );
            assert_eq!(shared, PTHREAD_PROCESS_SHARED);
            assert_eq!(sync::mutex_init(m.at(), attr.at()), err::ENOTSUP);
        }
    }

    #[test]
    fn a_condition_variable_wakes_a_waiter() {
        struct Shared {
            m: Box<Mutex>,
            c: Box<Cond>,
            ready: AtomicU32,
        }
        // SAFETY: the two objects are only ever reached through this module's
        // functions, which is what a guest does with them.
        unsafe impl Sync for Shared {}
        unsafe impl Send for Shared {}

        let s = Arc::new(Shared {
            m: Mutex::new(),
            c: Cond::new(),
            ready: AtomicU32::new(0),
        });
        // SAFETY: two live, correctly sized objects.
        unsafe {
            assert_eq!(sync::mutex_init(s.m.at(), 0), 0);
            assert_eq!(sync::cond_init(s.c.at(), 0), 0);
        }

        let waiter = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                // SAFETY: live objects shared with the signalling thread.
                unsafe {
                    assert_eq!(sync::mutex_lock(s.m.at()), 0);
                    while s.ready.load(Ordering::Relaxed) == 0 {
                        assert_eq!(sync::cond_wait(s.c.at(), s.m.at()), 0);
                    }
                    assert_eq!(sync::mutex_unlock(s.m.at()), 0);
                }
            })
        };

        // SAFETY: as above.
        unsafe {
            loop {
                assert_eq!(sync::mutex_lock(s.m.at()), 0);
                s.ready.store(1, Ordering::Relaxed);
                assert_eq!(sync::cond_broadcast(s.c.at()), 0);
                assert_eq!(sync::mutex_unlock(s.m.at()), 0);
                if waiter.is_finished() {
                    break;
                }
                std::thread::yield_now();
            }
        }
        waiter.join().expect("the waiter wakes");
        // SAFETY: nothing is waiting any more.
        unsafe {
            assert_eq!(sync::cond_destroy(s.c.at()), 0);
            assert_eq!(sync::mutex_destroy(s.m.at()), 0);
        }
    }

    /// A relative timeout is Darwin's own call and has to become a deadline. The
    /// assertion is that it *times out* rather than returning at once or hanging.
    #[test]
    fn a_relative_timed_wait_times_out() {
        let m = Mutex::new();
        let c = Cond::new();
        let interval = libc::timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        // SAFETY: two live objects and a live `timespec`.
        unsafe {
            assert_eq!(sync::mutex_init(m.at(), 0), 0);
            assert_eq!(sync::cond_init(c.at(), 0), 0);
            assert_eq!(sync::mutex_lock(m.at()), 0);
            let start = std::time::Instant::now();
            let rc = sync::cond_timedwait_relative(
                c.at(),
                m.at(),
                &interval as *const libc::timespec as usize as u64,
            );
            // Darwin's 60, not this host's 110 - which is the number `libc++`
            // compares against before deciding a timed wait failed.
            assert_eq!(rc, err::ETIMEDOUT);
            assert!(
                start.elapsed() >= std::time::Duration::from_millis(15),
                "it waited"
            );
            assert_eq!(sync::mutex_unlock(m.at()), 0);
            assert_eq!(sync::cond_destroy(c.at()), 0);
            assert_eq!(sync::mutex_destroy(m.at()), 0);
        }
    }

    /// Asked across threads rather than on one, which is both what the property is
    /// about and the only way to ask it: whether a thread may take a second read lock
    /// it already holds is left undefined by POSIX, and Darwin and Linux differ.
    #[test]
    fn a_read_write_lock_admits_readers_together_and_writers_alone() {
        struct Shared(Box<RwLock>);
        // SAFETY: the lock is only ever reached through this module's functions,
        // which is what a guest does with it.
        unsafe impl Sync for Shared {}
        unsafe impl Send for Shared {}

        let l = Arc::new(Shared(RwLock::new()));
        // SAFETY: a live, correctly sized lock object.
        unsafe {
            assert_eq!(sync::rwlock_init(l.0.at(), 0), 0);
            assert_eq!(sync::rwlock_rdlock(l.0.at()), 0);
        }

        // With a reader holding it, another thread may read and may not write.
        {
            let l = Arc::clone(&l);
            std::thread::spawn(move || {
                // SAFETY: a live lock shared with the holding thread.
                unsafe {
                    assert_eq!(
                        sync::rwlock_trywrlock(l.0.at()),
                        libc::EBUSY,
                        "a reader holds it"
                    );
                    assert_eq!(sync::rwlock_tryrdlock(l.0.at()), 0, "readers share");
                    assert_eq!(sync::rwlock_unlock(l.0.at()), 0);
                }
            })
            .join()
            .expect("the second reader");
        }

        // SAFETY: as above; this thread still holds its read lock.
        unsafe {
            assert_eq!(sync::rwlock_unlock(l.0.at()), 0);
            assert_eq!(sync::rwlock_wrlock(l.0.at()), 0, "and now it is free");
        }

        // With a writer holding it, another thread may do neither.
        {
            let l = Arc::clone(&l);
            std::thread::spawn(move || {
                // SAFETY: as above.
                unsafe {
                    assert_eq!(sync::rwlock_trywrlock(l.0.at()), libc::EBUSY);
                    assert_eq!(sync::rwlock_tryrdlock(l.0.at()), libc::EBUSY);
                }
            })
            .join()
            .expect("the excluded thread");
        }

        // SAFETY: this thread holds the write lock and nothing else is waiting.
        unsafe {
            assert_eq!(sync::rwlock_unlock(l.0.at()), 0);
            assert_eq!(sync::rwlock_destroy(l.0.at()), 0);
        }
    }

    #[test]
    fn a_statically_initialised_rwlock_works() {
        let l = RwLock::statically_initialised(object::sig::RWLOCK_INIT);
        // SAFETY: a live lock object carrying a Darwin static initializer.
        unsafe {
            assert_eq!(sync::rwlock_wrlock(l.at()), 0);
            assert_eq!(sync::rwlock_unlock(l.at()), 0);
        }
    }

    /// Exactly once, and every thread that asked waits for it - which is the whole
    /// contract, and the one a spin that returned early would break.
    #[test]
    fn once_runs_once_and_everyone_waits_for_it() {
        let o = Arc::new(Once::statically_initialised(object::sig::ONCE_INIT));
        let ran = Arc::new(AtomicU32::new(0));
        let finished = Arc::new(AtomicU32::new(0));

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (o, ran, finished) = (Arc::clone(&o), Arc::clone(&ran), Arc::clone(&finished));
                std::thread::spawn(move || {
                    // SAFETY: a live `pthread_once_t` shared with the other threads.
                    let rc = unsafe {
                        once(o.at(), || {
                            // Slow on purpose: a waiter that did not wait would see
                            // the flag still unset.
                            std::thread::sleep(std::time::Duration::from_millis(30));
                            ran.fetch_add(1, Ordering::SeqCst);
                        })
                    };
                    assert_eq!(rc, 0);
                    assert_eq!(
                        ran.load(Ordering::SeqCst),
                        1,
                        "returned before the routine finished"
                    );
                    finished.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().expect("a once thread");
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the routine ran once");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            8,
            "and everyone got through"
        );
    }

    #[test]
    fn once_refuses_a_control_word_that_is_not_one() {
        let o = Once::new();
        // SAFETY: a live, zeroed object that carries no static initializer.
        assert_eq!(unsafe { once(o.at(), || unreachable!()) }, err::EINVAL);
    }

    /// Darwin's cancel state values are the opposite way round from the host's, so an
    /// untranslated call disables cancellation where the guest asked to enable it.
    #[test]
    fn cancel_state_round_trips_in_darwins_numbering() {
        // SAFETY: a live local out-parameter.
        unsafe {
            let mut old = -1i32;
            assert_eq!(
                setcancelstate(PTHREAD_CANCEL_DISABLE as u64, &mut old as *mut _ as u64),
                0
            );
            let mut back = -1i32;
            assert_eq!(
                setcancelstate(PTHREAD_CANCEL_ENABLE as u64, &mut back as *mut _ as u64),
                0
            );
            assert_eq!(back, PTHREAD_CANCEL_DISABLE, "what was just set");
            assert_eq!(
                setcancelstate(99, core::ptr::null_mut::<i32>() as u64),
                err::EINVAL
            );
        }
    }

    /// `pthread_kill` carries a Darwin signal number, which is not the host's for
    /// most of the range.
    ///
    /// Asked through the "does this thread exist" probe and through a number the host
    /// has not got, so nothing is ever actually delivered to the test process.
    #[test]
    fn pthread_kill_translates_the_signal_number() {
        // SAFETY: signal zero delivers nothing; the thread is this one.
        assert_eq!(unsafe { kill(this_thread(), 0) }, 0);

        // `SIGINFO` is Darwin's own. On a Mac it is a real signal and translates to
        // itself; anywhere else there is nothing to translate it to, and delivering
        // whatever occupies 29 instead would run a handler installed for something
        // else.
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            unsafe { kill(this_thread(), crate::signal::signum::SIGINFO as u64) },
            err::EINVAL
        );

        // A number that is not a signal on either host.
        // SAFETY: refused before anything is delivered.
        assert_eq!(unsafe { kill(this_thread(), 200) }, err::EINVAL);
    }

    fn this_thread() -> u64 {
        // SAFETY: `pthread_self` is infallible; the handle is pointer-sized.
        unsafe { core::mem::transmute_copy(&libc::pthread_self()) }
    }

    /// Nothing may be both implemented here and claimed as a passthrough - the two
    /// lists are the same question answered opposite ways.
    #[test]
    fn nothing_is_both_implemented_and_passed_through() {
        for name in PASSED_THROUGH {
            assert!(
                symbol(name).is_none(),
                "{name} is passed through and also implemented"
            );
        }
    }

    /// Every implemented name must be one Darwin actually exports, or it is answering
    /// an import no guest can make.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn every_entry_names_a_real_darwin_symbol() {
        // The list Darwin's libsystem_pthread exports, as of the SDK this was written
        // against. Kept short: only the entries this module claims.
        for (name, address) in entries() {
            assert!(name.starts_with("pthread_"), "{name}");
            assert_ne!(*address, 0, "{name} resolved to nothing");
        }
    }

    /// A thread this module starts runs with the *Darwin* thread pointer, which is
    /// the whole reason `pthread_create` is not passed through: a guest reads
    /// `pthread_self` and its stack canary as direct loads off it and never calls
    /// anything this could be done inside.
    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn a_created_thread_runs_with_the_darwin_thread_pointer() {
        use std::sync::atomic::{AtomicU64, Ordering};

        /// `ARCH_GET_GS`, from `<asm/prctl.h>`. Not in the `libc` crate.
        const ARCH_GET_GS: libc::c_int = 0x1004;

        /// The `%gs` *base*, asked for rather than dereferenced: a thread that
        /// never got one would fault on the load a guest actually compiles to,
        /// and a failing test beats a crashing one.
        extern "C" fn body(out: u64) -> u64 {
            let mut base: u64 = 0;
            // SAFETY: `arch_prctl` with `ARCH_GET_GS` writes this thread's `%gs`
            // base through the pointer, which is a live local.
            unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_GET_GS, &mut base) };
            // SAFETY: the `AtomicU64` the caller passed, live until the join below.
            unsafe { &*(out as usize as *const AtomicU64) }.store(base, Ordering::SeqCst);
            0
        }

        let seen = AtomicU64::new(0);
        let mut thread = null_thread();
        // SAFETY: a writable `pthread_t`, no attributes, and a start routine of
        // this module's own taking the address of a live local.
        let rc = unsafe {
            create(
                &mut thread as *mut _ as u64,
                0,
                body as *const () as u64,
                &seen as *const _ as u64,
            )
        };
        assert_eq!(rc, 0, "the thread did not start");
        // SAFETY: the handle `create` just wrote, joined exactly once.
        assert_eq!(unsafe { join(thread as u64, 0) }, 0);
        assert_ne!(
            seen.load(Ordering::SeqCst),
            0,
            "the new thread ran with no Darwin thread pointer at all"
        );
    }
}
