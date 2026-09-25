use std::sync::atomic::Ordering;

use super::consts::*;
use super::object::{self, Kind, Record, State, sig};
use super::sync::MutexAttr;
use crate::wait_word::{self, Scope, WakeCount};

const STATE: u64 = 0;
const KIND: u64 = 4;
const OWNER: u64 = 8;
const RECURSION: u64 = 16;
const WAITERS: u64 = 20;
const BODY: usize = 24;

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
const CONTENDED: u32 = 2;

/// Darwin counts recursive acquisitions in an `unsigned short`.
const RECURSION_LIMIT: u32 = u16::MAX as u32;

const _: () = assert!(16 + BODY <= object::size::MUTEX);

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

    /// Which kind a `PTHREAD_*_MUTEX_INITIALIZER` asked for.
    ///
    /// Darwin's first-fit initializer selects a waiter policy rather than a type, so
    /// it is a normal mutex here.
    fn from_static(signature: u64) -> Option<Self> {
        match signature & sig::MUTEX_INIT_TYPE_MASK {
            sig::MUTEX_INIT_RECURSIVE => Some(Self::Recursive),
            sig::MUTEX_INIT_ERRORCHECK => Some(Self::ErrorCheck),
            sig::MUTEX_INIT_FIRSTFIT | sig::MUTEX_INIT_NORMAL => Some(Self::Normal),
            _ => None,
        }
    }
}

/// Fill a freshly claimed mutex body. The signature follows, with release ordering.
fn lay_out(record: &Record, kind: MutexKind) {
    record.u32(STATE).store(UNLOCKED, Ordering::Relaxed);
    record.u32(KIND).store(kind as u32, Ordering::Relaxed);
    record.u64(OWNER).store(0, Ordering::Relaxed);
    record.u32(RECURSION).store(0, Ordering::Relaxed);
    record.u32(WAITERS).store(0, Ordering::Relaxed);
}

fn kind_of(record: &Record) -> MutexKind {
    match record.u32(KIND).load(Ordering::Relaxed) {
        k if k == MutexKind::ErrorCheck as u32 => MutexKind::ErrorCheck,
        k if k == MutexKind::Recursive as u32 => MutexKind::Recursive,
        _ => MutexKind::Normal,
    }
}

/// The record for a live mutex at `at`, built from a static initializer if that is
/// all the guest ever wrote.
///
/// # Safety
///
/// `at` is a guest `pthread_mutex_t *`.
unsafe fn live(at: u64) -> Option<Record> {
    if at == 0 {
        return None;
    }
    let record = unsafe { Record::at(at, Kind::Mutex) };
    record
        .live_or_init(
            |record, signature| match MutexKind::from_static(signature) {
                Some(kind) => {
                    lay_out(record, kind);
                    true
                }
                // A static initializer whose low nibble names no type libpthread knows.
                // Darwin's deferred initialisation refuses it too.
                None => false,
            },
        )
        .then_some(record)
}

fn thread_id() -> u64 {
    let id = super::id() as u64;
    debug_assert_ne!(id, 0, "a live pthread identity is never zero");
    id
}

/// The three-state futex lock, which is the whole of a normal mutex.
///
/// A slow acquirer takes the lock in state [`CONTENDED`] and leaves it there until its
/// own unlock, keeping the conservative "there may be sleepers" bit set. A release
/// that swaps to [`UNLOCKED`] and then wakes therefore cannot lose a waiter sitting
/// between its failed acquisition and its compare-and-sleep.
fn acquire(record: &Record, try_only: bool) -> bool {
    let state = record.u32(STATE);
    if state
        .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return true;
    }
    if try_only {
        return false;
    }
    record.u32(WAITERS).fetch_add(1, Ordering::Relaxed);
    while state.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
        // `pthread_mutex_lock` is not a cancellation point, and none of `EINTR`, a
        // changed compare, a fault from racing guest misuse or a spurious return is
        // mistaken for having acquired the mutex: every one of them leads back to the
        // swap above.
        // SAFETY: the state word lives in the guest's mutex, which the caller holds.
        let _ = unsafe { wait_word::wait_u32(state, CONTENDED, None, Scope::Private) };
    }
    record.u32(WAITERS).fetch_sub(1, Ordering::Relaxed);
    true
}

fn release(record: &Record) {
    let state = record.u32(STATE);
    if state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
        // SAFETY: as in [`acquire`].
        let _ = unsafe { wait_word::wake_u32(state, WakeCount::One, Scope::Private) };
    }
}

fn lock_record(record: &Record, kind: MutexKind, try_only: bool) -> i32 {
    if kind == MutexKind::Normal {
        return match acquire(record, try_only) {
            true => 0,
            false => err::EBUSY,
        };
    }

    // The typed kinds need a stable identity for their ownership checks.
    let self_id = thread_id();
    if record.u64(OWNER).load(Ordering::Relaxed) == self_id {
        if kind == MutexKind::ErrorCheck {
            return match try_only {
                true => err::EBUSY,
                false => err::EDEADLK,
            };
        }
        let count = record.u32(RECURSION).load(Ordering::Relaxed);
        if count == RECURSION_LIMIT {
            return err::EAGAIN;
        }
        record.u32(RECURSION).store(count + 1, Ordering::Relaxed);
        return 0;
    }

    if !acquire(record, try_only) {
        return err::EBUSY;
    }
    record.u64(OWNER).store(self_id, Ordering::Relaxed);
    if kind == MutexKind::Recursive {
        record.u32(RECURSION).store(1, Ordering::Relaxed);
    }
    0
}

fn unlock_record(record: &Record, kind: MutexKind) -> i32 {
    if kind == MutexKind::Normal {
        // Darwin tolerates normal-mutex unlock misuse, including unlocking one that
        // is already unlocked. The typed kinds reject it below.
        release(record);
        return 0;
    }
    if record.u64(OWNER).load(Ordering::Relaxed) != thread_id() {
        return err::EPERM;
    }
    if kind == MutexKind::Recursive {
        let count = record.u32(RECURSION).load(Ordering::Relaxed);
        if count > 1 {
            record.u32(RECURSION).store(count - 1, Ordering::Relaxed);
            return 0;
        }
        record.u32(RECURSION).store(0, Ordering::Relaxed);
    }
    record.u64(OWNER).store(0, Ordering::Relaxed);
    release(record);
    0
}

/// `int pthread_mutex_init(pthread_mutex_t *, const pthread_mutexattr_t *)`.
///
/// # Safety
///
/// `at` is a guest `pthread_mutex_t *` and `attr` is null or a guest
/// `pthread_mutexattr_t *`.
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
    let record = unsafe { Record::at(at, Kind::Mutex) };
    // Re-initialising a live mutex is undefined in POSIX. Nothing is owned, so there
    // is nothing to reclaim and the previous state is simply overwritten.
    record.seize();
    record.publish(|record| lay_out(record, kind));
    0
}

/// # Safety
///
/// As [`mutex_init`].
pub unsafe extern "C" fn mutex_destroy(at: u64) -> i32 {
    if at == 0 {
        return err::EINVAL;
    }
    let record = unsafe { Record::at(at, Kind::Mutex) };
    let state = record.seize();
    match state {
        State::Invalid => err::EINVAL,
        // A static initializer the guest never locked: nothing was ever built.
        State::Fresh(_) => {
            record.bury();
            0
        }
        State::Live
            if record.u32(STATE).load(Ordering::Acquire) != UNLOCKED
                || record.u32(WAITERS).load(Ordering::Acquire) != 0 =>
        {
            record.restore(state);
            err::EBUSY
        }
        State::Live => {
            record.bury();
            0
        }
    }
}

/// # Safety
///
/// As [`mutex_init`].
pub unsafe extern "C" fn mutex_lock(at: u64) -> i32 {
    match unsafe { live(at) } {
        Some(record) => lock_record(&record, kind_of(&record), false),
        None => err::EINVAL,
    }
}

/// # Safety
///
/// As [`mutex_init`].
pub unsafe extern "C" fn mutex_trylock(at: u64) -> i32 {
    match unsafe { live(at) } {
        Some(record) => lock_record(&record, kind_of(&record), true),
        None => err::EINVAL,
    }
}

/// # Safety
///
/// As [`mutex_init`].
pub unsafe extern "C" fn mutex_unlock(at: u64) -> i32 {
    match unsafe { live(at) } {
        Some(record) => unlock_record(&record, kind_of(&record)),
        None => err::EINVAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pthread::testing::Guest;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    type Mutex = Guest<{ object::size::MUTEX }>;
    type Attr = Guest<{ object::size::MUTEXATTR }>;

    fn attr_with(settings: MutexAttr) -> Arc<Attr> {
        let attr = Attr::zeroed();
        // SAFETY: a live, correctly sized attribute object.
        let record = unsafe { Record::at(attr.at(), Kind::MutexAttr) };
        object::publish_cell(&record, settings.packed_public());
        attr
    }

    #[test]
    fn default_initialization_is_a_normal_mutex_that_owns_nothing() {
        let mutex = Mutex::zeroed();
        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        // SAFETY: a live, correctly sized mutex object.
        let record = unsafe { Record::at(mutex.at(), Kind::Mutex) };
        assert_eq!(kind_of(&record), MutexKind::Normal);
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
            let mutex = Mutex::statically_initialised(signature);
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            // SAFETY: a live mutex the call above initialised.
            let record = unsafe { Record::at(mutex.at(), Kind::Mutex) };
            assert_eq!(kind_of(&record), kind);
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
    }

    #[test]
    fn errorchecking_trylock_and_foreign_unlock_match_darwin() {
        let attr = attr_with(MutexAttr {
            kind: PTHREAD_MUTEX_ERRORCHECK,
            ..MutexAttr::default()
        });
        let mutex = Mutex::zeroed();
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
        let attr = attr_with(MutexAttr {
            kind: PTHREAD_MUTEX_RECURSIVE,
            ..MutexAttr::default()
        });
        let mutex = Mutex::zeroed();
        assert_eq!(unsafe { mutex_init(mutex.at(), attr.at()) }, 0);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
        // SAFETY: a live mutex this thread holds.
        let record = unsafe { Record::at(mutex.at(), Kind::Mutex) };
        record
            .u32(RECURSION)
            .store(RECURSION_LIMIT, Ordering::Relaxed);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, err::EAGAIN);
        record.u32(RECURSION).store(1, Ordering::Relaxed);
        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
    }

    /// Darwin records the protocol and its lock paths ignore it, so asking for one
    /// must neither fail nor produce a different kind of mutex.
    #[test]
    fn a_priority_protocol_is_accepted_and_changes_nothing() {
        for protocol in [PTHREAD_PRIO_INHERIT, PTHREAD_PRIO_PROTECT] {
            let attr = attr_with(MutexAttr {
                protocol,
                ..MutexAttr::default()
            });
            let mutex = Mutex::zeroed();
            assert_eq!(unsafe { mutex_init(mutex.at(), attr.at()) }, 0);
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_trylock(mutex.at()) }, err::EBUSY);
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
    }

    #[test]
    fn contended_mutex_excludes_many_threads() {
        let mutex = Mutex::zeroed();
        let value = Arc::new(AtomicU32::new(0));
        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (mutex, value) = (Arc::clone(&mutex), Arc::clone(&value));
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
                        let seen = value.load(Ordering::Relaxed);
                        value.store(seen + 1, Ordering::Relaxed);
                        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(value.load(Ordering::Relaxed), 80_000);
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
    }

    /// A wake is permission to look again, never a grant: the state machine, not the
    /// kernel, decides who holds the mutex.
    #[test]
    fn a_spurious_wake_does_not_grant_the_mutex() {
        let mutex = Mutex::zeroed();
        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
        let acquired = Arc::new(AtomicU32::new(0));
        let waiter = {
            let (mutex, acquired) = (Arc::clone(&mutex), Arc::clone(&acquired));
            std::thread::spawn(move || {
                assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
                acquired.store(1, Ordering::Release);
                assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            })
        };
        // SAFETY: a live mutex this thread holds.
        let record = unsafe { Record::at(mutex.at(), Kind::Mutex) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while record.u32(STATE).load(Ordering::Acquire) != CONTENDED {
            assert!(std::time::Instant::now() < deadline, "the waiter blocked");
            std::thread::yield_now();
        }
        // SAFETY: the state word of a live mutex.
        let _ = unsafe { wait_word::wake_u32(record.u32(STATE), WakeCount::One, Scope::Private) };
        for _ in 0..100 {
            assert_eq!(acquired.load(Ordering::Acquire), 0);
            std::thread::yield_now();
        }
        assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        waiter.join().unwrap();
        assert_eq!(acquired.load(Ordering::Acquire), 1);
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
    }

    /// A deliberately ignored release-mode smoke benchmark, kept so that changes to
    /// the state machine can compare initialisation, locality, uncontended traffic,
    /// handoff and contention independently. Run with:
    /// `cargo test --release -p darwin mutex_microbenchmarks -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn mutex_microbenchmarks() {
        assert!(!cfg!(debug_assertions), "run this benchmark with --release");
        const ITERATIONS: u32 = 20_000;

        let mutex = Mutex::zeroed();
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
        let init = start.elapsed();

        assert_eq!(unsafe { mutex_init(mutex.at(), 0) }, 0);
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS * 10 {
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            std::hint::black_box(());
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
        }
        let uncontended = start.elapsed();
        assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);

        let host = Box::into_raw(Box::new(unsafe {
            core::mem::zeroed::<libc::pthread_mutex_t>()
        }));
        assert_eq!(
            unsafe { libc::pthread_mutex_init(host, core::ptr::null()) },
            0
        );
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS * 10 {
            assert_eq!(unsafe { libc::pthread_mutex_lock(host) }, 0);
            std::hint::black_box(());
            assert_eq!(unsafe { libc::pthread_mutex_unlock(host) }, 0);
        }
        let host_uncontended = start.elapsed();
        assert_eq!(unsafe { libc::pthread_mutex_destroy(host) }, 0);
        drop(unsafe { Box::from_raw(host) });

        let many: Vec<_> = (0..4096).map(|_| Mutex::zeroed()).collect();
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
            .map(|_| Mutex::statically_initialised(sig::MUTEX_INIT))
            .collect();
        let start = std::time::Instant::now();
        for mutex in &statics {
            assert_eq!(unsafe { mutex_lock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_unlock(mutex.at()) }, 0);
            assert_eq!(unsafe { mutex_destroy(mutex.at()) }, 0);
        }
        let static_first_touch = start.elapsed();

        let contended = Mutex::zeroed();
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

        eprintln!(
            "init/destroy: {init:?}\n\
             uncontended: {uncontended:?}\n\
             host uncontended: {host_uncontended:?}\n\
             four-thread contention: {four_thread_contention:?}\n\
             many mutexes: {many_mutexes:?}\n\
             static first touch: {static_first_touch:?}"
        );
    }
}
