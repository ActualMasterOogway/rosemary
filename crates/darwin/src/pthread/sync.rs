//! Mutexes, condition variables, read-write locks and `pthread_once`.
//!
//! Each of the first three is a **live object** the guest allocates and this crate has
//! to keep alive across calls, so each guest object gets a host one on the heap and
//! keeps its address in its own bytes. See [`super::object`] for the record and for
//! why a heap object rather than one placed in the guest's block: a Darwin
//! `pthread_cond_t` is forty-eight bytes and so is glibc's, which leaves nowhere to
//! put the signature that says whose it is.
//!
//! `pthread_once` needs no host object at all - it is one word of state, and the
//! guest's sixteen bytes have room for it.
//!
//! Compiled on both hosts and exercised by the tests on both, though only Linux binds
//! a guest import to any of it. On a Mac these call the same libpthread the guest
//! would have called, which is what makes the test run there worth anything.

use std::sync::atomic::Ordering;

use super::consts::*;
use super::object::{Kind, Record, sig};

/// A zeroed host object on the heap, for `init` to initialise **in place**.
///
/// In place, and not initialised-then-moved, which is the whole reason this is a
/// function rather than `publish(make_one())`. A `pthread_rwlock_t` on Darwin records
/// its own address when it is initialised and checks it on every lock, so an object
/// initialised on the stack and then moved to the heap deadlocks on its first
/// `pthread_rwlock_rdlock` - measured, not reasoned about. Mutexes and condition
/// variables happen to tolerate the move on both hosts; relying on that would be
/// relying on an implementation detail of somebody else's libc.
///
/// Freed by the matching `destroy`. A guest that never destroys leaks one object per
/// lock, which is what a program that never destroys its locks does on a real system.
fn empty<T>() -> *mut T {
    // SAFETY: every pthread object this is used for is a plain byte block that its
    // own `init` fills in, and all-zero is a valid state for one to arrive in.
    Box::into_raw(Box::new(unsafe { core::mem::zeroed::<T>() }))
}

/// Hand back the address of a host object that initialised successfully, or free it.
///
/// # Safety
///
/// `object` came from [`empty`] and has not been reclaimed.
unsafe fn published<T>(object: *mut T, rc: i32) -> Option<u64> {
    if rc == 0 {
        return Some(object as usize as u64);
    }
    // SAFETY: the caller's guarantee, and nothing else holds it.
    drop(unsafe { Box::from_raw(object) });
    None
}

/// # Safety
///
/// `at` came from [`publish`] with the same `T` and has not been reclaimed.
unsafe fn reclaim<T>(at: u64) -> Box<T> {
    // SAFETY: the caller's guarantee.
    unsafe { Box::from_raw(at as usize as *mut T) }
}

/// A host mutex built for `kind`, or `None` if the host refused it.
fn host_mutex(darwin_type: i32, protocol: i32) -> Option<u64> {
    let mut attr: libc::pthread_mutexattr_t = unsafe { core::mem::zeroed() };
    // SAFETY: a live local; `pthread_mutexattr_init` initialises the whole structure.
    if unsafe { libc::pthread_mutexattr_init(&mut attr) } != 0 {
        return None;
    }
    let host_type = mutex_type_to_host(darwin_type)?;
    // SAFETY: an initialised attribute set and a type the translation vouched for.
    unsafe { libc::pthread_mutexattr_settype(&mut attr, host_type) };
    // These constants agree on Darwin and supported Unix hosts.
    if unsafe { super::raw::pthread_mutexattr_setprotocol(&mut attr, protocol) } != 0 {
        unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
        return None;
    }

    let m = empty::<libc::pthread_mutex_t>();
    // SAFETY: `m` is a live zeroed object of the right type and `attr` an initialised
    // attribute set. Initialised where it will live, never moved afterwards.
    let rc = unsafe { libc::pthread_mutex_init(m, &attr) };
    // SAFETY: initialised above and not used again.
    unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
    // SAFETY: `m` came from `empty` and nothing else holds it.
    unsafe { published(m, rc) }
}

/// Which mutex a static initializer asks for.
///
/// `PTHREAD_RECURSIVE_MUTEX_INITIALIZER` is a real thing a guest writes, and the
/// difference between it and the plain one is the difference between a lock that
/// recurses and one that deadlocks.
fn static_mutex_type(signature: u64) -> Option<i32> {
    Some(match signature & sig::MUTEX_INIT_TYPE_MASK {
        sig::MUTEX_INIT_RECURSIVE => PTHREAD_MUTEX_RECURSIVE,
        sig::MUTEX_INIT_ERRORCHECK => PTHREAD_MUTEX_ERRORCHECK,
        // First-fit is a *policy*, not a type: it changes which waiter wins, which no
        // correct program observes, and the mutex itself is an ordinary one.
        sig::MUTEX_INIT_FIRSTFIT | sig::MUTEX_INIT_NORMAL => PTHREAD_MUTEX_NORMAL,
        // libpthread accepts the signature mask first, then rejects an initializer
        // whose low-nibble type `_pthread_mutex_init` does not know.
        _ => return None,
    })
}

/// The host mutex behind a guest one, building it from a static initializer if that
/// is all the guest wrote.
///
/// # Safety
///
/// `at` is a guest `pthread_mutex_t *`.
unsafe fn mutex_of(at: u64) -> Option<*mut libc::pthread_mutex_t> {
    if at == 0 {
        return None;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Mutex) };
    r.payload_or_build(|signature| host_mutex(static_mutex_type(signature)?, PTHREAD_PRIO_NONE))
        .map(|p| p as usize as *mut libc::pthread_mutex_t)
}

/// `int pthread_mutex_init(pthread_mutex_t *, const pthread_mutexattr_t *)`.
///
/// # Safety
///
/// Called only through an import slot this crate defines.
pub unsafe extern "C" fn mutex_init(at: u64, attr: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let settings = unsafe { MutexAttr::of_guest(attr) };
    let Some(settings) = settings else {
        return err::EINVAL;
    };
    if settings.pshared == PTHREAD_PROCESS_SHARED {
        // A process-shared lock lives in memory two processes map, and the host
        // object behind it does not: it is on this process's heap. Refused rather
        // than silently made private, which would be a lock that excludes nobody.
        say_unsupported("a process-shared mutex");
        return err::ENOTSUP;
    }
    let Some(host) = host_mutex(settings.kind, settings.protocol) else {
        return err::EINVAL;
    };
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Mutex) };
    if let Some(previous) = r.reinitialise(host) {
        // Re-initialising a live mutex is undefined in POSIX; freeing rather than
        // leaking is the kinder undefined.
        // SAFETY: `previous` was published by this module with this type.
        drop(unsafe { reclaim::<libc::pthread_mutex_t>(previous) });
    }
    0
}

/// # Safety
///
/// As [`mutex_init`].
pub unsafe extern "C" fn mutex_destroy(at: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Mutex) };
    let Some(payload) = r.take() else {
        return err::EINVAL;
    };
    let Some(host) = payload else {
        // A static initializer that was never used. Nothing was built, so nothing is
        // freed, and destroying it is still success.
        return 0;
    };
    // SAFETY: published by this module with this type, and now unreachable from the
    // guest - `take` replaced the signature before this ran.
    let boxed = unsafe { reclaim::<libc::pthread_mutex_t>(host) };
    let raw = Box::into_raw(boxed);
    // SAFETY: a live, initialised mutex this owns.
    let rc = unsafe { libc::pthread_mutex_destroy(raw) };
    if rc != 0 {
        r.set_payload(host);
        return err::from_host(rc);
    }
    // SAFETY: destroy succeeded and `raw` is again exclusively owned here.
    drop(unsafe { Box::from_raw(raw) });
    0
}

macro_rules! mutex_op {
    ($name:ident, $host:ident) => {
        /// # Safety
        ///
        /// As [`mutex_init`].
        pub unsafe extern "C" fn $name(at: u64) -> i32 {
            // SAFETY: the caller's guarantee.
            let Some(m) = (unsafe { mutex_of(at) }) else {
                return err::EINVAL;
            };
            // SAFETY: a live host mutex this module owns for as long as the guest's
            // object says it does.
            err::from_host(unsafe { libc::$host(m) })
        }
    };
}

mutex_op!(mutex_lock, pthread_mutex_lock);
mutex_op!(mutex_trylock, pthread_mutex_trylock);
mutex_op!(mutex_unlock, pthread_mutex_unlock);

fn host_cond() -> Option<u64> {
    let c = empty::<libc::pthread_cond_t>();
    // SAFETY: a live zeroed object of the right type, initialised where it will live;
    // a null attribute pointer is the defaults.
    let rc = unsafe { libc::pthread_cond_init(c, core::ptr::null()) };
    // SAFETY: `c` came from `empty` and nothing else holds it.
    unsafe { published(c, rc) }
}

/// # Safety
///
/// `at` is a guest `pthread_cond_t *`.
unsafe fn cond_of(at: u64) -> Option<*mut libc::pthread_cond_t> {
    if at == 0 {
        return None;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Cond) };
    r.payload_or_build(|_| host_cond())
        .map(|p| p as usize as *mut libc::pthread_cond_t)
}

/// # Safety
///
/// Called only through an import slot this crate defines.
pub unsafe extern "C" fn cond_init(at: u64, attr: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let Some(shared) = (unsafe { simple_attr_pshared(attr, Kind::CondAttr) }) else {
        return err::EINVAL;
    };
    if shared == Some(PTHREAD_PROCESS_SHARED) {
        say_unsupported("a process-shared condition variable");
        return err::ENOTSUP;
    }
    let Some(host) = host_cond() else {
        return err::EINVAL;
    };
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Cond) };
    if let Some(previous) = r.reinitialise(host) {
        // SAFETY: published by this module with this type.
        drop(unsafe { reclaim::<libc::pthread_cond_t>(previous) });
    }
    0
}

/// # Safety
///
/// As [`cond_init`].
pub unsafe extern "C" fn cond_destroy(at: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Cond) };
    let Some(payload) = r.take() else {
        return err::EINVAL;
    };
    let Some(host) = payload else {
        return 0;
    };
    // SAFETY: published by this module, and now unreachable from the guest.
    let boxed = unsafe { reclaim::<libc::pthread_cond_t>(host) };
    let raw = Box::into_raw(boxed);
    // SAFETY: a live condition variable this owns.
    let rc = unsafe { libc::pthread_cond_destroy(raw) };
    if rc != 0 {
        r.set_payload(host);
        return err::from_host(rc);
    }
    // SAFETY: destroy succeeded and `raw` is again exclusively owned here.
    drop(unsafe { Box::from_raw(raw) });
    0
}

macro_rules! cond_signal_op {
    ($name:ident, $host:ident) => {
        /// # Safety
        ///
        /// As [`cond_init`].
        pub unsafe extern "C" fn $name(at: u64) -> i32 {
            // SAFETY: the caller's guarantee.
            let Some(c) = (unsafe { cond_of(at) }) else {
                return err::EINVAL;
            };
            // SAFETY: a live condition variable this module owns.
            err::from_host(unsafe { libc::$host(c) })
        }
    };
}

cond_signal_op!(cond_signal, pthread_cond_signal);
cond_signal_op!(cond_broadcast, pthread_cond_broadcast);

/// # Safety
///
/// As [`cond_init`], with `mutex` a guest `pthread_mutex_t *` the caller holds.
pub unsafe extern "C" fn cond_wait(at: u64, mutex: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    let (Some(c), Some(m)) = (unsafe { cond_of(at) }, unsafe { mutex_of(mutex) }) else {
        return err::EINVAL;
    };
    // SAFETY: both objects are live and this module owns them; the guest holds the
    // lock, which is its side of the contract.
    err::from_host(unsafe { libc::pthread_cond_wait(c, m) })
}

/// `struct timespec` is a `time_t` and a `long` on both platforms, so the guest's
/// deadline is passed through rather than converted.
///
/// # Safety
///
/// As [`cond_wait`], with `abstime` a readable `struct timespec *`.
pub unsafe extern "C" fn cond_timedwait(at: u64, mutex: u64, abstime: u64) -> i32 {
    // SAFETY: the caller's guarantee.
    let (Some(c), Some(m)) = (unsafe { cond_of(at) }, unsafe { mutex_of(mutex) }) else {
        return err::EINVAL;
    };
    if abstime == 0 {
        return err::EINVAL;
    }
    // SAFETY: as above; `abstime` is the guest's own `timespec`, whose two fields are
    // the same width and order here.
    err::from_host(unsafe {
        libc::pthread_cond_timedwait(c, m, abstime as usize as *const libc::timespec)
    })
}

/// `pthread_cond_timedwait_relative_np`, which is Darwin's and takes an *interval*.
///
/// Converted to a deadline here rather than left to the host, which has no such call.
/// The clock is the one `pthread_cond_timedwait` measures against, so a conversion
/// through any other would drift.
///
/// # Safety
///
/// As [`cond_timedwait`], with `reltime` a readable `struct timespec *`.
pub unsafe extern "C" fn cond_timedwait_relative(at: u64, mutex: u64, reltime: u64) -> i32 {
    if reltime == 0 {
        // A null interval means "wait forever", which is what Darwin does.
        // SAFETY: the caller's guarantee.
        return unsafe { cond_wait(at, mutex) };
    }
    // SAFETY: the caller states `reltime` is a readable `timespec`.
    let interval = unsafe { *(reltime as usize as *const libc::timespec) };
    if interval.tv_sec < 0 || !(0..1_000_000_000).contains(&interval.tv_nsec) {
        return err::EINVAL;
    }
    let mut now: libc::timespec = unsafe { core::mem::zeroed() };
    // SAFETY: a live local. `CLOCK_REALTIME` is what an untouched `pthread_cond_t`
    // measures its deadlines against on both platforms.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };

    let mut deadline = libc::timespec {
        tv_sec: now.tv_sec.saturating_add(interval.tv_sec),
        tv_nsec: now.tv_nsec + interval.tv_nsec,
    };
    if deadline.tv_nsec >= 1_000_000_000 {
        deadline.tv_nsec -= 1_000_000_000;
        deadline.tv_sec = deadline.tv_sec.saturating_add(1);
    }
    // SAFETY: `deadline` is a live local for the duration of the call, and the rest is
    // the caller's.
    unsafe {
        cond_timedwait(
            at,
            mutex,
            &deadline as *const libc::timespec as usize as u64,
        )
    }
}

fn host_rwlock() -> Option<u64> {
    let l = empty::<libc::pthread_rwlock_t>();
    // SAFETY: a live zeroed object of the right type, initialised where it will live -
    // see [`empty`] for why that matters more here than anywhere else.
    let rc = unsafe { libc::pthread_rwlock_init(l, core::ptr::null()) };
    // SAFETY: `l` came from `empty` and nothing else holds it.
    unsafe { published(l, rc) }
}

/// # Safety
///
/// `at` is a guest `pthread_rwlock_t *`.
unsafe fn rwlock_of(at: u64) -> Option<*mut libc::pthread_rwlock_t> {
    if at == 0 {
        return None;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::RwLock) };
    r.payload_or_build(|_| host_rwlock())
        .map(|p| p as usize as *mut libc::pthread_rwlock_t)
}

/// # Safety
///
/// Called only through an import slot this crate defines.
pub unsafe extern "C" fn rwlock_init(at: u64, attr: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let Some(shared) = (unsafe { simple_attr_pshared(attr, Kind::RwLockAttr) }) else {
        return err::EINVAL;
    };
    if shared == Some(PTHREAD_PROCESS_SHARED) {
        say_unsupported("a process-shared read-write lock");
        return err::ENOTSUP;
    }
    let Some(host) = host_rwlock() else {
        return err::EINVAL;
    };
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::RwLock) };
    if let Some(previous) = r.reinitialise(host) {
        // SAFETY: published by this module with this type.
        drop(unsafe { reclaim::<libc::pthread_rwlock_t>(previous) });
    }
    0
}

/// # Safety
///
/// As [`rwlock_init`].
pub unsafe extern "C" fn rwlock_destroy(at: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::RwLock) };
    let Some(payload) = r.take() else {
        return err::EINVAL;
    };
    let Some(host) = payload else {
        return 0;
    };
    // SAFETY: published by this module, and now unreachable from the guest.
    let boxed = unsafe { reclaim::<libc::pthread_rwlock_t>(host) };
    let raw = Box::into_raw(boxed);
    // SAFETY: a live lock this owns.
    let rc = unsafe { libc::pthread_rwlock_destroy(raw) };
    if rc != 0 {
        r.set_payload(host);
        return err::from_host(rc);
    }
    // SAFETY: destroy succeeded and `raw` is again exclusively owned here.
    drop(unsafe { Box::from_raw(raw) });
    0
}

macro_rules! rwlock_op {
    ($name:ident, $host:ident) => {
        /// # Safety
        ///
        /// As [`rwlock_init`].
        pub unsafe extern "C" fn $name(at: u64) -> i32 {
            // SAFETY: the caller's guarantee.
            let Some(l) = (unsafe { rwlock_of(at) }) else {
                return err::EINVAL;
            };
            // SAFETY: a live lock this module owns.
            err::from_host(unsafe { libc::$host(l) })
        }
    };
}

rwlock_op!(rwlock_rdlock, pthread_rwlock_rdlock);
rwlock_op!(rwlock_tryrdlock, pthread_rwlock_tryrdlock);
rwlock_op!(rwlock_wrlock, pthread_rwlock_wrlock);
rwlock_op!(rwlock_trywrlock, pthread_rwlock_trywrlock);
rwlock_op!(rwlock_unlock, pthread_rwlock_unlock);

/// What a `pthread_mutexattr_t` says.
#[derive(Clone, Copy, Debug)]
pub struct MutexAttr {
    pub kind: i32,
    pub pshared: i32,
    pub protocol: i32,
}

impl Default for MutexAttr {
    fn default() -> Self {
        MutexAttr {
            kind: PTHREAD_MUTEX_DEFAULT,
            pshared: PTHREAD_PROCESS_PRIVATE,
            protocol: PTHREAD_PRIO_NONE,
        }
    }
}

impl MutexAttr {
    /// Pack into the one spare word a sixteen-byte attribute object has.
    pub fn packed_public(self) -> u64 {
        self.packed()
    }

    /// [`MutexAttr::packed_public`] in reverse.
    pub fn unpacked_public(word: u64) -> MutexAttr {
        MutexAttr::unpacked(word)
    }

    fn packed(self) -> u64 {
        (self.kind as u32 as u64)
            | ((self.pshared as u32 as u64) << 16)
            | ((self.protocol as u32 as u64) << 32)
    }

    fn unpacked(word: u64) -> MutexAttr {
        MutexAttr {
            kind: (word & 0xffff) as i32,
            pshared: ((word >> 16) & 0xffff) as i32,
            protocol: ((word >> 32) & 0xffff) as i32,
        }
    }

    /// What the guest's `pthread_mutexattr_t` at `attr` says.
    ///
    /// A null pointer is the defaults, which is what a null attribute argument to
    /// `pthread_mutex_init` means. `None` is "that is not an attribute set".
    ///
    /// Always this crate's own record, on either host - unlike [`super::Attributes`],
    /// which reads a *thread* attribute set and may be handed one libpthread wrote.
    /// The difference is who filled it in: a `pthread_mutexattr_t` only ever reaches
    /// here from this crate's `pthread_mutexattr_init`, because the two are bound to
    /// the guest together or not at all.
    ///
    /// # Safety
    ///
    /// `attr` is null or a guest `pthread_mutexattr_t *`.
    pub unsafe fn of_guest(attr: u64) -> Option<MutexAttr> {
        if attr == 0 {
            return Some(MutexAttr::default());
        }
        // SAFETY: the caller's guarantee.
        let r = unsafe { Record::at(attr, Kind::MutexAttr) };
        r.is_ours()
            .then(|| MutexAttr::unpacked(r.cell().load(Ordering::Relaxed)))
    }
}

/// The `pshared` of a `pthread_condattr_t` or `pthread_rwlockattr_t`, which is the
/// only setting either carries that this crate acts on.
///
/// # Safety
///
/// `attr` is null or a guest attribute object of `kind`.
unsafe fn simple_attr_pshared(attr: u64, kind: Kind) -> Option<Option<i32>> {
    if attr == 0 {
        return Some(None);
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(attr, kind) };
    r.is_ours()
        .then(|| Some(r.cell().load(Ordering::Relaxed) as i32))
}

fn host_once() -> Option<u64> {
    Some(Box::into_raw(Box::new(libc::PTHREAD_ONCE_INIT)) as usize as u64)
}

thread_local! {
    static ONCE_BODY: std::cell::Cell<*mut core::ffi::c_void> =
        const { std::cell::Cell::new(core::ptr::null_mut()) };
}

/// Run `body` at most once for the `pthread_once_t` at `at`.
///
/// Not an `extern "C"` function, because the routine a guest passes is *guest code*
/// and reaching it needs a thunk - which is emulation vocabulary this crate does not
/// have. `rosetta3::shims::pthread` supplies the call and this supplies the
/// once-and-only-once.
///
/// # Safety
///
/// `at` is a guest `pthread_once_t *`.
pub unsafe fn once<F: FnOnce()>(at: u64, body: F) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Once) };
    let mut body = Some(body);
    let Some(host) = r.payload_or_build(|_| host_once()) else {
        return err::EINVAL;
    };

    extern "C" fn invoke<F: FnOnce()>() {
        ONCE_BODY.with(|slot| {
            let body = slot.get().cast::<Option<F>>();
            // SAFETY: `once` installed a live `Option<F>` for this thread, and the
            // host invokes this callback synchronously before returning.
            let body = unsafe { &mut *body }.take().expect("once body is present");
            body();
        });
    }

    // For some reason, on macOS, the libc crate makes init_routine an Option ???
    unsafe fn pthread_once_wrapper(
        once_control: *mut libc::pthread_once_t,
        init_routine: Option<unsafe extern "C" fn()>,
    ) -> libc::c_int {
        #[cfg(target_vendor = "apple")]
        return unsafe { libc::pthread_once(once_control, init_routine) };
        #[cfg(not(target_vendor = "apple"))]
        return unsafe {
            let init_routine_raw: extern "C" fn() = std::mem::transmute(init_routine);
            libc::pthread_once(once_control, init_routine_raw)
        };
    }

    ONCE_BODY.with(|slot| {
        let previous = slot.replace((&mut body as *mut Option<F>).cast());
        // SAFETY: a live host once object, and a callback using this thread's slot.
        let rc = unsafe {
            pthread_once_wrapper(
                host as usize as *mut libc::pthread_once_t,
                Some(invoke::<F>),
            )
        };
        slot.set(previous);
        err::from_host(rc)
    })
}

/// `int pthread_once(pthread_once_t *, void (*routine)(void))`
///
/// The entry point the *native* backend binds, where the guest's code is already
/// host code and the routine can simply be called. Under emulation the routine
/// is x86-64 that needs a thunk, so `rosetta3::shims::pthread` intercepts the
/// call by name and reaches [`once`] itself.
///
/// # Safety
///
/// `at` is a guest `pthread_once_t *` and `routine` is a function this process
/// can call.
pub unsafe extern "C" fn once_entry(at: u64, routine: Option<extern "C" fn()>) -> i32 {
    let Some(routine) = routine else {
        return err::EINVAL;
    };
    // SAFETY: the caller's guarantee.
    unsafe { once(at, || routine()) }
}

/// Report a pthread feature this crate declines to fake, once per process.
fn say_unsupported(what: &str) {
    use std::sync::atomic::AtomicBool;
    static SAID: AtomicBool = AtomicBool::new(false);
    if SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = what;
    const MESSAGE: &[u8] = b"rosetta3: pthread: a process-shared object is unsupported\n";
    // SAFETY: a fixed static buffer; `write` is async-signal-safe.
    unsafe { libc::write(libc::STDERR_FILENO, MESSAGE.as_ptr().cast(), MESSAGE.len()) };
}
