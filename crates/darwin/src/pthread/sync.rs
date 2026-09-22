//! Mutexes, condition variables, read-write locks and `pthread_once`.
//!
//! Condition variables and read-write locks keep host objects on the heap. Ordinary
//! process-private mutexes are inline; see [`super::mutex`]. A condition wait uses a
//! small host-side gate to bridge its atomic release-and-wait operation to an inline
//! mutex without opening a lost-wakeup window.
//!
//! `pthread_once` needs no host object at all - it is one word of state, and the
//! guest's sixteen bytes have room for it.
//!
//! Compiled on both hosts and exercised by the tests on both, though only Linux binds
//! a guest import to any of it. On a Mac these call the same libpthread the guest
//! would have called, which is what makes the test run there worth anything.

use std::sync::atomic::Ordering;

use super::consts::*;
use super::object::{Kind, Record};

pub use super::mutex::{mutex_destroy, mutex_init, mutex_lock, mutex_trylock, mutex_unlock};

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

struct HostCond {
    cond: libc::pthread_cond_t,
    /// Serializes the inline-mutex release with entering the host condition wait.
    gate: libc::pthread_mutex_t,
}

fn host_cond() -> Option<u64> {
    let c = empty::<HostCond>();
    // SAFETY: both fields are live zeroed host objects and are initialized in place.
    let mutex_rc = unsafe { libc::pthread_mutex_init(&mut (*c).gate, core::ptr::null()) };
    if mutex_rc != 0 {
        drop(unsafe { Box::from_raw(c) });
        return None;
    }
    let cond_rc = unsafe { libc::pthread_cond_init(&mut (*c).cond, core::ptr::null()) };
    if cond_rc != 0 {
        unsafe { libc::pthread_mutex_destroy(&mut (*c).gate) };
        drop(unsafe { Box::from_raw(c) });
        return None;
    }
    Some(c as usize as u64)
}

/// # Safety
///
/// `at` is a guest `pthread_cond_t *`.
unsafe fn cond_of(at: u64) -> Option<*mut HostCond> {
    if at == 0 {
        return None;
    }
    // SAFETY: the caller's guarantee.
    let r = unsafe { Record::at(at, Kind::Cond) };
    r.payload_or_build(|_| host_cond())
        .map(|p| p as usize as *mut HostCond)
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
        drop(unsafe { reclaim::<HostCond>(previous) });
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
    let boxed = unsafe { reclaim::<HostCond>(host) };
    let raw = Box::into_raw(boxed);
    // SAFETY: a live condition variable this owns.
    let rc = unsafe { libc::pthread_cond_destroy(&mut (*raw).cond) };
    if rc != 0 {
        r.set_payload(host);
        return err::from_host(rc);
    }
    // No waiter can hold the gate after condition destruction succeeds.
    let gate_rc = unsafe { libc::pthread_mutex_destroy(&mut (*raw).gate) };
    debug_assert_eq!(gate_rc, 0);
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
            // The gate closes the release/enqueue window used by inline guest
            // mutexes. It is harmless for host-fallback mutex waiters too.
            let lock_rc = unsafe { libc::pthread_mutex_lock(&mut (*c).gate) };
            if lock_rc != 0 {
                return err::from_host(lock_rc);
            }
            let rc = unsafe { libc::$host(&mut (*c).cond) };
            unsafe { libc::pthread_mutex_unlock(&mut (*c).gate) };
            err::from_host(rc)
        }
    };
}

cond_signal_op!(cond_signal, pthread_cond_signal);
cond_signal_op!(cond_broadcast, pthread_cond_broadcast);

/// # Safety
///
/// As [`cond_init`], with `mutex` a guest `pthread_mutex_t *` the caller holds.
pub unsafe extern "C" fn cond_wait(at: u64, mutex: u64) -> i32 {
    let Some(c) = (unsafe { cond_of(at) }) else {
        return err::EINVAL;
    };
    if let Some(m) = unsafe { super::mutex::host_pointer(mutex) } {
        return err::from_host(unsafe { libc::pthread_cond_wait(&mut (*c).cond, m) });
    }
    if !unsafe { super::mutex::is_inline(mutex) } {
        return err::EINVAL;
    }
    unsafe { cond_wait_inline(c, mutex, None) }
}

/// `struct timespec` is a `time_t` and a `long` on both platforms, so the guest's
/// deadline is passed through rather than converted.
///
/// # Safety
///
/// As [`cond_wait`], with `abstime` a readable `struct timespec *`.
pub unsafe extern "C" fn cond_timedwait(at: u64, mutex: u64, abstime: u64) -> i32 {
    let Some(c) = (unsafe { cond_of(at) }) else {
        return err::EINVAL;
    };
    if abstime == 0 {
        return err::EINVAL;
    }
    let deadline = abstime as usize as *const libc::timespec;
    if let Some(m) = unsafe { super::mutex::host_pointer(mutex) } {
        return err::from_host(unsafe {
            libc::pthread_cond_timedwait(&mut (*c).cond, m, deadline)
        });
    }
    if !unsafe { super::mutex::is_inline(mutex) } {
        return err::EINVAL;
    }
    unsafe { cond_wait_inline(c, mutex, Some(deadline)) }
}

/// Atomically, with respect to this condition's signal paths, release an inline
/// guest mutex and enter a host condition wait. The guest mutex is reacquired before
/// returning, including on timeout and spurious wakeup.
unsafe fn cond_wait_inline(
    cond: *mut HostCond,
    mutex: u64,
    deadline: Option<*const libc::timespec>,
) -> i32 {
    let rc = unsafe { libc::pthread_mutex_lock(&mut (*cond).gate) };
    if rc != 0 {
        return err::from_host(rc);
    }
    let unlock_rc = unsafe { super::mutex::mutex_unlock(mutex) };
    if unlock_rc != 0 {
        unsafe { libc::pthread_mutex_unlock(&mut (*cond).gate) };
        return unlock_rc;
    }
    let wait_rc = match deadline {
        Some(deadline) => unsafe {
            libc::pthread_cond_timedwait(&mut (*cond).cond, &mut (*cond).gate, deadline)
        },
        None => unsafe { libc::pthread_cond_wait(&mut (*cond).cond, &mut (*cond).gate) },
    };
    unsafe { libc::pthread_mutex_unlock(&mut (*cond).gate) };
    let lock_rc = unsafe { super::mutex::mutex_lock(mutex) };
    if lock_rc != 0 {
        return lock_rc;
    }
    err::from_host(wait_rc)
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
pub(super) fn say_unsupported(what: &str) {
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
