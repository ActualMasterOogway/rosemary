//! Inline process-private pthread mutexes.
//!
//! Ordinary mutexes keep their state directly in Darwin's 64-byte opaque object.
//! Priority protocols retain a tagged boxed-host fallback because a plain host
//! wait/wake operation cannot provide priority inheritance or protection.
//!
//! ```text
//! +0   AtomicU64  lifecycle/backend/type signature
//! +8   AtomicU32  lock state: 0 unlocked, 1 locked, 2 possibly contended
//! +12  AtomicU32  packed backend, type, and protocol
//! +16  AtomicU64  owning thread for error-checking and recursive mutexes
//! +24  AtomicU32  recursive acquisition count
//! +28  AtomicU32  slow-path callers, including kernel sleepers
//! +32  AtomicU64  guest address identity for copied-object detection
//! +40  AtomicU64  host pointer, only for a priority-protocol fallback
//! +48..64         reserved
//! ```

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::consts::*;
use super::object::{self, Kind, sig};
use super::sync::MutexAttr;
use crate::wait_word::{self, Scope, WakeCount};

const SIGNATURE: u64 = 0;
const STATE: u64 = 8;
const META: u64 = 12;
const OWNER: u64 = 16;
const RECURSION: u64 = 24;
const WAITERS: u64 = 28;
const IDENTITY: u64 = 32;
const HOST_POINTER: u64 = 40;

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
const CONTENDED: u32 = 2;
const RECURSION_LIMIT: u32 = u16::MAX as u32;

#[cfg(test)]
const BACKEND_MASK: u32 = 0xff;
const BACKEND_INLINE: u32 = 1;
const BACKEND_HOST: u32 = 2;
const KIND_SHIFT: u32 = 8;
const PROTOCOL_SHIFT: u32 = 16;

const LIVE_INLINE_NORMAL: u64 = object::ours(Kind::Mutex);
const LIVE_INLINE_ERRORCHECK: u64 = object::ours(Kind::Mutex) | 0x10;
const LIVE_INLINE_RECURSIVE: u64 = object::ours(Kind::Mutex) | 0x20;
const LIVE_HOST: u64 = object::ours(Kind::Mutex) | 0x30;

const _: () = assert!(HOST_POINTER as usize + size_of::<u64>() <= object::size::MUTEX);
const _: () = assert!(STATE.is_multiple_of(align_of::<AtomicU32>() as u64));
const _: () = assert!(OWNER.is_multiple_of(align_of::<AtomicU64>() as u64));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutexKind {
    Normal,
    ErrorCheck,
    Recursive,
}

impl MutexKind {
    fn from_darwin(value: i32) -> Option<Self> {
        match value {
            PTHREAD_MUTEX_NORMAL => Some(Self::Normal),
            PTHREAD_MUTEX_ERRORCHECK => Some(Self::ErrorCheck),
            PTHREAD_MUTEX_RECURSIVE => Some(Self::Recursive),
            _ => None,
        }
    }

    fn as_darwin(self) -> i32 {
        match self {
            Self::Normal => PTHREAD_MUTEX_NORMAL,
            Self::ErrorCheck => PTHREAD_MUTEX_ERRORCHECK,
            Self::Recursive => PTHREAD_MUTEX_RECURSIVE,
        }
    }
}

#[derive(Clone, Copy)]
enum Backend {
    Inline(MutexKind),
    Host(*mut libc::pthread_mutex_t),
}

struct MutexRecord {
    at: u64,
}

impl MutexRecord {
    /// # Safety
    ///
    /// `at` points to a writable, eight-byte-aligned Darwin `pthread_mutex_t`.
    unsafe fn at(at: u64) -> Self {
        Self { at }
    }

    fn u32(&self, offset: u64) -> &AtomicU32 {
        // SAFETY: every named offset is aligned and within the mutex object.
        unsafe { &*((self.at + offset) as usize as *const AtomicU32) }
    }

    fn u64(&self, offset: u64) -> &AtomicU64 {
        // SAFETY: every named offset is aligned and within the mutex object.
        unsafe { &*((self.at + offset) as usize as *const AtomicU64) }
    }

    fn signature(&self) -> &AtomicU64 {
        self.u64(SIGNATURE)
    }

    fn state(&self) -> &AtomicU32 {
        self.u32(STATE)
    }

    fn owner(&self) -> &AtomicU64 {
        self.u64(OWNER)
    }

    fn recursion(&self) -> &AtomicU32 {
        self.u32(RECURSION)
    }

    fn waiters(&self) -> &AtomicU32 {
        self.u32(WAITERS)
    }

    fn identity_matches(&self) -> bool {
        let identity = self.u64(IDENTITY).load(Ordering::Relaxed);
        if identity == self.at {
            true
        } else {
            object::say_copied(Kind::Mutex, self.at, identity);
            false
        }
    }

    fn meta(backend: u32, kind: MutexKind, protocol: i32) -> u32 {
        backend | ((kind.as_darwin() as u32) << KIND_SHIFT) | ((protocol as u32) << PROTOCOL_SHIFT)
    }

    fn publish_inline(&self, kind: MutexKind) {
        self.state().store(UNLOCKED, Ordering::Relaxed);
        self.u32(META).store(
            Self::meta(BACKEND_INLINE, kind, PTHREAD_PRIO_NONE),
            Ordering::Relaxed,
        );
        self.owner().store(0, Ordering::Relaxed);
        self.recursion().store(0, Ordering::Relaxed);
        self.waiters().store(0, Ordering::Relaxed);
        self.u64(IDENTITY).store(self.at, Ordering::Relaxed);
        self.u64(HOST_POINTER).store(0, Ordering::Relaxed);
        let signature = match kind {
            MutexKind::Normal => LIVE_INLINE_NORMAL,
            MutexKind::ErrorCheck => LIVE_INLINE_ERRORCHECK,
            MutexKind::Recursive => LIVE_INLINE_RECURSIVE,
        };
        self.signature().store(signature, Ordering::Release);
    }

    fn publish_host(&self, kind: MutexKind, protocol: i32, pointer: u64) {
        self.state().store(UNLOCKED, Ordering::Relaxed);
        self.u32(META)
            .store(Self::meta(BACKEND_HOST, kind, protocol), Ordering::Relaxed);
        self.owner().store(0, Ordering::Relaxed);
        self.recursion().store(0, Ordering::Relaxed);
        self.waiters().store(0, Ordering::Relaxed);
        self.u64(IDENTITY).store(self.at, Ordering::Relaxed);
        self.u64(HOST_POINTER).store(pointer, Ordering::Relaxed);
        self.signature().store(LIVE_HOST, Ordering::Release);
    }

    fn ready_backend(&self, signature: u64) -> Option<Backend> {
        if !self.identity_matches() {
            return None;
        }
        match signature {
            LIVE_INLINE_NORMAL => Some(Backend::Inline(MutexKind::Normal)),
            LIVE_INLINE_ERRORCHECK => Some(Backend::Inline(MutexKind::ErrorCheck)),
            LIVE_INLINE_RECURSIVE => Some(Backend::Inline(MutexKind::Recursive)),
            LIVE_HOST => {
                let pointer = self.u64(HOST_POINTER).load(Ordering::Relaxed);
                (pointer != 0).then_some(Backend::Host(pointer as usize as *mut _))
            }
            _ => None,
        }
    }

    fn backend_or_init(&self) -> Option<Backend> {
        loop {
            let signature = self.signature().load(Ordering::Acquire);
            if is_live_signature(signature) {
                return self.ready_backend(signature);
            }
            if signature == object::BUSY {
                std::hint::spin_loop();
                continue;
            }
            if !Kind::Mutex.is_static_initializer(signature) {
                return None;
            }
            if self
                .signature()
                .compare_exchange(
                    signature,
                    object::BUSY,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            let Some(kind) = static_kind(signature) else {
                self.signature().store(signature, Ordering::Release);
                return None;
            };
            self.publish_inline(kind);
            return Some(Backend::Inline(kind));
        }
    }
}

fn is_live_signature(signature: u64) -> bool {
    matches!(
        signature,
        LIVE_INLINE_NORMAL | LIVE_INLINE_ERRORCHECK | LIVE_INLINE_RECURSIVE | LIVE_HOST
    )
}

fn static_kind(signature: u64) -> Option<MutexKind> {
    MutexKind::from_darwin(match signature & sig::MUTEX_INIT_TYPE_MASK {
        sig::MUTEX_INIT_RECURSIVE => PTHREAD_MUTEX_RECURSIVE,
        sig::MUTEX_INIT_ERRORCHECK => PTHREAD_MUTEX_ERRORCHECK,
        sig::MUTEX_INIT_FIRSTFIT | sig::MUTEX_INIT_NORMAL => PTHREAD_MUTEX_NORMAL,
        _ => return None,
    })
}

fn host_mutex(kind: MutexKind, protocol: i32) -> Option<u64> {
    let mut attr: libc::pthread_mutexattr_t = unsafe { core::mem::zeroed() };
    if unsafe { libc::pthread_mutexattr_init(&mut attr) } != 0 {
        return None;
    }
    let host_kind = mutex_type_to_host(kind.as_darwin())?;
    let type_rc = unsafe { libc::pthread_mutexattr_settype(&mut attr, host_kind) };
    let protocol_rc = unsafe { super::raw::pthread_mutexattr_setprotocol(&mut attr, protocol) };
    if type_rc != 0 || protocol_rc != 0 {
        unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
        return None;
    }
    let pointer = Box::into_raw(Box::new(unsafe {
        core::mem::zeroed::<libc::pthread_mutex_t>()
    }));
    let rc = unsafe { libc::pthread_mutex_init(pointer, &attr) };
    unsafe { libc::pthread_mutexattr_destroy(&mut attr) };
    if rc == 0 {
        Some(pointer as usize as u64)
    } else {
        drop(unsafe { Box::from_raw(pointer) });
        None
    }
}

fn thread_id() -> u64 {
    let id = super::id() as u64;
    debug_assert_ne!(id, 0, "a live pthread identity is never zero");
    id
}

#[inline]
fn lock_normal(record: &MutexRecord, try_only: bool) -> i32 {
    if record
        .state()
        .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return 0;
    }
    if try_only {
        return err::EBUSY;
    }
    record.waiters().fetch_add(1, Ordering::Relaxed);
    loop {
        if record.state().swap(CONTENDED, Ordering::Acquire) == UNLOCKED {
            record.waiters().fetch_sub(1, Ordering::Relaxed);
            return 0;
        }
        let _ = unsafe { wait_word::wait_u32(record.state(), CONTENDED, None, Scope::Private) };
    }
}

#[inline]
fn unlock_normal(record: &MutexRecord) -> i32 {
    let previous = record.state().swap(UNLOCKED, Ordering::Release);
    if previous == CONTENDED {
        let _ = unsafe { wait_word::wake_u32(record.state(), WakeCount::One, Scope::Private) };
    }
    0
}

fn lock_inline(record: &MutexRecord, kind: MutexKind, try_only: bool) -> i32 {
    if kind == MutexKind::Normal {
        return lock_normal(record, try_only);
    }
    // Typed mutexes need a stable live-thread identity for their ownership checks.
    let self_id = thread_id();
    if record.owner().load(Ordering::Relaxed) == self_id {
        if kind == MutexKind::ErrorCheck {
            return if try_only { err::EBUSY } else { err::EDEADLK };
        }
        let count = record.recursion().load(Ordering::Relaxed);
        if count == RECURSION_LIMIT {
            return err::EAGAIN;
        }
        record.recursion().store(count + 1, Ordering::Relaxed);
        return 0;
    }

    if record
        .state()
        .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        record.owner().store(self_id, Ordering::Relaxed);
        if kind == MutexKind::Recursive {
            record.recursion().store(1, Ordering::Relaxed);
        }
        return 0;
    }
    if try_only {
        return err::EBUSY;
    }

    // This is the standard three-state futex mutex. A slow acquirer uses state 2,
    // preserving the conservative "there may be sleepers" bit until its unlock.
    // Release-swap to 0 followed by wake therefore cannot lose a waiter between its
    // failed acquisition and compare-and-sleep operation.
    record.waiters().fetch_add(1, Ordering::Relaxed);
    loop {
        if record.state().swap(CONTENDED, Ordering::Acquire) == UNLOCKED {
            record.waiters().fetch_sub(1, Ordering::Relaxed);
            record.owner().store(self_id, Ordering::Relaxed);
            if kind == MutexKind::Recursive {
                record.recursion().store(1, Ordering::Relaxed);
            }
            return 0;
        }
        // pthread_mutex_lock is not a cancellation point. EINTR, a changed compare,
        // faults caused by racing guest misuse, and spurious returns all lead back to
        // an acquire operation; none is mistaken for owning the mutex.
        let _ = unsafe { wait_word::wait_u32(record.state(), CONTENDED, None, Scope::Private) };
    }
}

fn unlock_inline(record: &MutexRecord, kind: MutexKind) -> i32 {
    if kind == MutexKind::Normal {
        return unlock_normal(record);
    }
    let self_id = thread_id();
    if record.owner().load(Ordering::Relaxed) != self_id {
        return err::EPERM;
    }
    if kind == MutexKind::Recursive {
        let count = record.recursion().load(Ordering::Relaxed);
        if count > 1 {
            record.recursion().store(count - 1, Ordering::Relaxed);
            return 0;
        }
        record.recursion().store(0, Ordering::Relaxed);
    }
    record.owner().store(0, Ordering::Relaxed);
    let previous = record.state().swap(UNLOCKED, Ordering::Release);
    if previous == CONTENDED {
        let _ = unsafe { wait_word::wake_u32(record.state(), WakeCount::One, Scope::Private) };
    }
    // Darwin intentionally tolerates normal-mutex unlock misuse, including an
    // already-unlocked mutex; error-checking and recursive mutexes reject it above.
    0
}

/// `int pthread_mutex_init(pthread_mutex_t *, const pthread_mutexattr_t *)`.
pub unsafe extern "C" fn mutex_init(at: u64, attr: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    let Some(settings) = (unsafe { MutexAttr::of_guest(attr) }) else {
        return err::EINVAL;
    };
    if settings.pshared == PTHREAD_PROCESS_SHARED {
        super::sync::say_unsupported("a process-shared mutex");
        return err::ENOTSUP;
    }
    let Some(kind) = MutexKind::from_darwin(settings.kind) else {
        return err::EINVAL;
    };
    let host = if settings.protocol == PTHREAD_PRIO_NONE {
        None
    } else {
        let Some(pointer) = host_mutex(kind, settings.protocol) else {
            return err::EINVAL;
        };
        Some(pointer)
    };

    let record = unsafe { MutexRecord::at(at) };
    let old_signature = record.signature().swap(object::BUSY, Ordering::AcqRel);
    let old_host = if is_live_signature(old_signature) && record.identity_matches() {
        match record.ready_backend(old_signature) {
            Some(Backend::Host(pointer)) => Some(pointer),
            _ => None,
        }
    } else {
        None
    };
    match host {
        Some(pointer) => record.publish_host(kind, settings.protocol, pointer),
        None => record.publish_inline(kind),
    }

    // Reinitialization of a live mutex is undefined. Reclaim an old fallback only
    // when its identity proves it belonged to this address; a byte-copied mutex must
    // never free the original object's pointer.
    if let Some(pointer) = old_host
        && unsafe { libc::pthread_mutex_destroy(pointer) } == 0
    {
        drop(unsafe { Box::from_raw(pointer) });
    }
    0
}

pub unsafe extern "C" fn mutex_destroy(at: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    let record = unsafe { MutexRecord::at(at) };
    loop {
        let signature = record.signature().load(Ordering::Acquire);
        if Kind::Mutex.is_static_initializer(signature) {
            return match record.signature().compare_exchange(
                signature,
                object::DEAD,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => 0,
                Err(_) => continue,
            };
        }
        if !is_live_signature(signature) || !record.identity_matches() {
            return err::EINVAL;
        }
        let Some(backend) = record.ready_backend(signature) else {
            return err::EINVAL;
        };
        if record
            .signature()
            .compare_exchange(signature, object::BUSY, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        return match backend {
            Backend::Inline(_)
                if record.state().load(Ordering::Acquire) != UNLOCKED
                    || record.waiters().load(Ordering::Acquire) != 0 =>
            {
                record.signature().store(signature, Ordering::Release);
                err::EBUSY
            }
            Backend::Inline(_) => {
                record.signature().store(object::DEAD, Ordering::Release);
                0
            }
            Backend::Host(pointer) => {
                let rc = unsafe { libc::pthread_mutex_destroy(pointer) };
                if rc != 0 {
                    record.signature().store(signature, Ordering::Release);
                    err::from_host(rc)
                } else {
                    drop(unsafe { Box::from_raw(pointer) });
                    record.signature().store(object::DEAD, Ordering::Release);
                    0
                }
            }
        };
    }
}

pub unsafe extern "C" fn mutex_lock(at: u64) -> i32 {
    mutex_operation(at, false, false)
}

pub unsafe extern "C" fn mutex_trylock(at: u64) -> i32 {
    mutex_operation(at, true, false)
}

pub unsafe extern "C" fn mutex_unlock(at: u64) -> i32 {
    mutex_operation(at, false, true)
}

fn mutex_operation(at: u64, try_only: bool, unlock: bool) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    let record = unsafe { MutexRecord::at(at) };
    // Keep the overwhelmingly common ordinary-inline path to two lifecycle loads
    // before entering the state machine. Other representations decode below.
    if record.signature().load(Ordering::Acquire) == LIVE_INLINE_NORMAL {
        if !record.identity_matches() {
            return err::EINVAL;
        }
        return if unlock {
            unlock_normal(&record)
        } else {
            lock_normal(&record, try_only)
        };
    }
    let Some(backend) = record.backend_or_init() else {
        return err::EINVAL;
    };
    match backend {
        Backend::Inline(kind) if unlock => unlock_inline(&record, kind),
        Backend::Inline(kind) => lock_inline(&record, kind, try_only),
        Backend::Host(pointer) if unlock => {
            err::from_host(unsafe { libc::pthread_mutex_unlock(pointer) })
        }
        Backend::Host(pointer) if try_only => {
            err::from_host(unsafe { libc::pthread_mutex_trylock(pointer) })
        }
        Backend::Host(pointer) => err::from_host(unsafe { libc::pthread_mutex_lock(pointer) }),
    }
}

/// The host pointer for a protocol fallback. Condition waits use this only when the
/// guest mutex selected that representation.
pub(super) unsafe fn host_pointer(at: u64) -> Option<*mut libc::pthread_mutex_t> {
    if at == 0 {
        return None;
    }
    let record = unsafe { MutexRecord::at(at) };
    match record.backend_or_init()? {
        Backend::Host(pointer) => Some(pointer),
        Backend::Inline(_) => None,
    }
}

pub(super) unsafe fn is_inline(at: u64) -> bool {
    if at == 0 {
        return false;
    }
    let record = unsafe { MutexRecord::at(at) };
    matches!(record.backend_or_init(), Some(Backend::Inline(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    #[repr(C, align(8))]
    struct GuestMutex([u8; object::size::MUTEX]);

    impl GuestMutex {
        fn new() -> Box<Self> {
            Box::new(Self([0; object::size::MUTEX]))
        }

        fn initialized(signature: u64) -> Box<Self> {
            let mut mutex = Self::new();
            mutex.0[..8].copy_from_slice(&signature.to_le_bytes());
            mutex
        }

        fn at(&self) -> u64 {
            self as *const Self as usize as u64
        }
    }

    unsafe impl Send for GuestMutex {}
    unsafe impl Sync for GuestMutex {}

    #[test]
    fn backend_signatures_are_distinct_from_lifecycle_and_darwin_signatures() {
        let live = [
            LIVE_INLINE_NORMAL,
            LIVE_INLINE_ERRORCHECK,
            LIVE_INLINE_RECURSIVE,
            LIVE_HOST,
        ];
        for (index, signature) in live.iter().enumerate() {
            assert!(!live[..index].contains(signature));
        }
        for signature in live {
            assert_ne!(signature, object::BUSY);
            assert_ne!(signature, object::DEAD);
            assert!(!Kind::Mutex.is_static_initializer(signature));
        }
    }

    #[repr(C, align(8))]
    struct GuestAttr([u8; object::size::MUTEXATTR]);

    impl GuestAttr {
        fn with(settings: MutexAttr) -> Box<Self> {
            let attr = Box::new(Self([0; object::size::MUTEXATTR]));
            let record = unsafe { object::Record::at(attr.at(), Kind::MutexAttr) };
            record.set_payload(settings.packed_public());
            attr
        }

        fn at(&self) -> u64 {
            self as *const Self as usize as u64
        }
    }

    #[test]
    fn default_initialization_is_inline_and_pointer_free() {
        let mutex = GuestMutex::new();
        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        let record = unsafe { MutexRecord::at(mutex.at()) };
        assert_eq!(
            record.u32(META).load(Ordering::Relaxed) & BACKEND_MASK,
            BACKEND_INLINE
        );
        assert_eq!(record.u64(HOST_POINTER).load(Ordering::Relaxed), 0);
        assert_eq!(record.u64(IDENTITY).load(Ordering::Relaxed), mutex.at());
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, err::EINVAL);
    }

    #[test]
    fn every_static_initializer_selects_its_declared_type() {
        for (signature, kind) in [
            (sig::MUTEX_INIT, MutexKind::Normal),
            (sig::FIRSTFIT_MUTEX_INIT, MutexKind::Normal),
            (sig::ERRORCHECK_MUTEX_INIT, MutexKind::ErrorCheck),
            (sig::RECURSIVE_MUTEX_INIT, MutexKind::Recursive),
        ] {
            let mutex = GuestMutex::initialized(signature);
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            let record = unsafe { MutexRecord::at(mutex.at()) };
            let signature = record.signature().load(Ordering::Acquire);
            assert!(
                matches!(record.ready_backend(signature), Some(Backend::Inline(found)) if found == kind)
            );
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
    }

    #[test]
    fn errorchecking_trylock_and_foreign_unlock_match_darwin() {
        let attr = GuestAttr::with(MutexAttr {
            kind: PTHREAD_MUTEX_ERRORCHECK,
            ..MutexAttr::default()
        });
        let mutex = Arc::new(GuestMutex::new());
        assert_eq!(unsafe { mutex_init(mutex.at(), attr.at()) }, 0);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
        assert_eq!(unsafe { mutex_trylock(mutex.at()) }, err::EBUSY);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, err::EDEADLK);
        let other = Arc::clone(&mutex);
        assert_eq!(
            std::thread::spawn(move || unsafe { mutex_unlock(other.at()) })
                .join()
                .unwrap(),
            err::EPERM
        );
        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
    }

    #[test]
    fn recursive_count_stops_at_darwins_ushort_limit() {
        let attr = GuestAttr::with(MutexAttr {
            kind: PTHREAD_MUTEX_RECURSIVE,
            ..MutexAttr::default()
        });
        let mutex = GuestMutex::new();
        assert_eq!(unsafe { mutex_init(mutex.at(), attr.at()) }, 0);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
        let record = unsafe { MutexRecord::at(mutex.at()) };
        record.recursion().store(RECURSION_LIMIT, Ordering::Relaxed);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, err::EAGAIN);
        record.recursion().store(1, Ordering::Relaxed);
        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
    }

    #[test]
    fn priority_protocols_never_select_the_inline_backend() {
        for protocol in [PTHREAD_PRIO_INHERIT, PTHREAD_PRIO_PROTECT] {
            let attr = GuestAttr::with(MutexAttr {
                protocol,
                ..MutexAttr::default()
            });
            let mutex = GuestMutex::new();
            let rc = unsafe { mutex_init(mutex.at(), attr.at()) };
            if rc == 0 {
                let record = unsafe { MutexRecord::at(mutex.at()) };
                assert_eq!(
                    record.u32(META).load(Ordering::Relaxed) & BACKEND_MASK,
                    BACKEND_HOST
                );
                assert_ne!(record.u64(HOST_POINTER).load(Ordering::Relaxed), 0);
                let lock_rc = unsafe { mutex_lock(mutex.at()) };
                if lock_rc == 0 {
                    assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
                } else {
                    // Linux may accept creation of a priority-protect mutex but
                    // reject locking it for the caller's scheduling parameters.
                    assert_eq!(lock_rc, err::EINVAL);
                }
                assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
            } else {
                assert_eq!(rc, err::EINVAL, "the host fallback refused the protocol");
            }
        }
    }

    #[test]
    fn contended_inline_mutex_excludes_many_threads() {
        struct Shared {
            mutex: GuestMutex,
            value: AtomicU32,
        }
        unsafe impl Send for Shared {}
        unsafe impl Sync for Shared {}

        let shared = Arc::new(Shared {
            mutex: GuestMutex([0; object::size::MUTEX]),
            value: AtomicU32::new(0),
        });
        assert_eq!(unsafe { mutex_init(shared.mutex.at(), 0) }, 0);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        assert_eq!(unsafe { mutex_lock(shared.mutex.at()) }, 0);
                        let value = shared.value.load(Ordering::Relaxed);
                        shared.value.store(value + 1, Ordering::Relaxed);
                        assert_eq!(unsafe { mutex_unlock(shared.mutex.at()) }, 0);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(shared.value.load(Ordering::Relaxed), 80_000);
        assert_eq!(unsafe { mutex_destroy(shared.mutex.at()) }, 0);
    }

    #[test]
    fn a_spurious_wake_does_not_grant_the_mutex() {
        let mutex = Arc::new(GuestMutex::new());
        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
        let acquired = Arc::new(AtomicU32::new(0));
        let waiter = {
            let mutex = Arc::clone(&mutex);
            let acquired = Arc::clone(&acquired);
            std::thread::spawn(move || {
                assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
                acquired.store(1, Ordering::Release);
                assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            })
        };
        let record = unsafe { MutexRecord::at(mutex.at()) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while record.state().load(Ordering::Acquire) != CONTENDED {
            assert!(
                std::time::Instant::now() < deadline,
                "waiter became contended"
            );
            std::thread::yield_now();
        }
        let _ = unsafe { wait_word::wake_u32(record.state(), WakeCount::One, Scope::Private) };
        for _ in 0..100 {
            assert_eq!(acquired.load(Ordering::Acquire), 0);
            std::thread::yield_now();
        }
        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        waiter.join().unwrap();
        assert_eq!(acquired.load(Ordering::Acquire), 1);
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
    }

    /// A deliberately ignored release-mode smoke benchmark. It keeps the workloads
    /// together so changes to the state machine can compare initialization, locality,
    /// uncontended traffic, handoff, and contention independently. Run with:
    /// `cargo test --release -p darwin mutex_microbenchmarks -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn mutex_microbenchmarks() {
        assert!(!cfg!(debug_assertions), "run this benchmark with --release");
        const ITERATIONS: u32 = 20_000;

        let mutex = GuestMutex::new();
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
        let inline_init = start.elapsed();

        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            let host = host_mutex(MutexKind::Normal, PTHREAD_PRIO_NONE).unwrap();
            let host = host as usize as *mut libc::pthread_mutex_t;
            assert_eq!(unsafe { libc::pthread_mutex_destroy(host) }, 0);
            drop(unsafe { Box::from_raw(host) });
        }
        let boxed_host_init = start.elapsed();

        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS * 10 {
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            std::hint::black_box(());
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        }
        let uncontended = start.elapsed();
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);

        let host = host_mutex(MutexKind::Normal, PTHREAD_PRIO_NONE).unwrap();
        let host = host as usize as *mut libc::pthread_mutex_t;
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS * 10 {
            assert_eq!(unsafe { libc::pthread_mutex_lock(host) }, 0);
            std::hint::black_box(());
            assert_eq!(unsafe { libc::pthread_mutex_unlock(host) }, 0);
        }
        let host_uncontended = start.elapsed();
        assert_eq!(unsafe { libc::pthread_mutex_destroy(host) }, 0);
        drop(unsafe { Box::from_raw(host) });

        // Retain the old guest-record -> boxed-host-pointer lookup in miniature so
        // the comparison includes the overhead the replaced implementation paid.
        let old = GuestMutex::new();
        let host = host_mutex(MutexKind::Normal, PTHREAD_PRIO_NONE).unwrap();
        let old_record = unsafe { object::Record::at(old.at(), Kind::Mutex) };
        old_record.set_payload(host);
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS * 10 {
            let object::Found::Ready(pointer) = old_record.peek() else {
                unreachable!();
            };
            let pointer = pointer as usize as *mut libc::pthread_mutex_t;
            assert_eq!(unsafe { libc::pthread_mutex_lock(pointer) }, 0);
            std::hint::black_box(());
            assert_eq!(unsafe { libc::pthread_mutex_unlock(pointer) }, 0);
        }
        let boxed_shim_uncontended = start.elapsed();
        let Some(Some(pointer)) = old_record.take() else {
            unreachable!();
        };
        let pointer = pointer as usize as *mut libc::pthread_mutex_t;
        assert_eq!(unsafe { libc::pthread_mutex_destroy(pointer) }, 0);
        drop(unsafe { Box::from_raw(pointer) });

        let many: Vec<_> = (0..4096).map(|_| GuestMutex::new()).collect();
        let start = std::time::Instant::now();
        for mutex in &many {
            assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        }
        for mutex in &many {
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
        let many_mutexes = start.elapsed();

        let statics: Vec<_> = (0..4096)
            .map(|_| GuestMutex::initialized(sig::MUTEX_INIT))
            .collect();
        let start = std::time::Instant::now();
        for mutex in &statics {
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
        let static_first_touch = start.elapsed();

        let contended = Arc::new(GuestMutex::new());
        assert_eq!(unsafe { mutex_init(contended.at(), 0) }, 0);
        let start = std::time::Instant::now();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let mutex = Arc::clone(&contended);
                std::thread::spawn(move || {
                    for _ in 0..ITERATIONS {
                        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
                        std::hint::black_box(());
                        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let four_thread_contention = start.elapsed();
        assert_eq!(unsafe { mutex_destroy(contended.at()) }, 0);

        let pingpong = Arc::new(GuestMutex::new());
        let turn = Arc::new(AtomicU32::new(0));
        assert_eq!(unsafe { mutex_init(pingpong.at(), 0) }, 0);
        let start = std::time::Instant::now();
        let threads: Vec<_> = (0..2)
            .map(|mine| {
                let mutex = Arc::clone(&pingpong);
                let turn = Arc::clone(&turn);
                std::thread::spawn(move || {
                    let mut completed = 0;
                    while completed != ITERATIONS {
                        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
                        if turn.load(Ordering::Relaxed) == mine {
                            turn.store(1 - mine, Ordering::Relaxed);
                            completed += 1;
                        }
                        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
                        if turn.load(Ordering::Relaxed) != mine {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let two_thread_pingpong = start.elapsed();
        assert_eq!(unsafe { mutex_destroy(pingpong.at()) }, 0);

        eprintln!(
            "inline init/destroy: {inline_init:?}\n\
             boxed host init/destroy: {boxed_host_init:?}\n\
             inline uncontended: {uncontended:?}\n\
             host uncontended: {host_uncontended:?}\n\
             old boxed-shim uncontended: {boxed_shim_uncontended:?}\n\
             inline two-thread ping-pong: {two_thread_pingpong:?}\n\
             inline four-thread contention: {four_thread_contention:?}\n\
             inline many mutexes: {many_mutexes:?}\n\
             inline static first touch: {static_first_touch:?}"
        );
    }
}
