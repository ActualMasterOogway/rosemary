//! Host compare-and-wait operations on a 32-bit userspace word.
//!
//! This is deliberately smaller than either Darwin's ulock API or a pthread mutex.
//! Both are adapters over this layer and keep their own error and wake semantics.

use std::sync::atomic::AtomicU32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Private,
    Shared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WakeCount {
    One,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitError {
    Interrupted,
    TimedOut,
    Changed,
    Fault(i32),
}

impl WaitError {
    pub(crate) fn raw_os_error(self) -> i32 {
        match self {
            WaitError::Interrupted => libc::EINTR,
            WaitError::TimedOut => libc::ETIMEDOUT,
            WaitError::Changed => libc::EAGAIN,
            WaitError::Fault(error) => error,
        }
    }
}

fn classify(error: i32) -> WaitError {
    match error {
        libc::EINTR => WaitError::Interrupted,
        libc::ETIMEDOUT => WaitError::TimedOut,
        libc::EAGAIN => WaitError::Changed,
        error => WaitError::Fault(error),
    }
}

/// Sleep while `word` still equals `expected`.
///
/// # Safety
///
/// `word` must remain at a stable, naturally aligned address for the call.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn wait_u32(
    word: &AtomicU32,
    expected: u32,
    timeout_ns: Option<u64>,
    scope: Scope,
) -> Result<(), WaitError> {
    use rustix::thread::futex::{self, Flags, Timespec};

    let timeout = timeout_ns.map(|nanoseconds| Timespec {
        tv_sec: (nanoseconds / 1_000_000_000).min(i64::MAX as u64) as i64,
        tv_nsec: (nanoseconds % 1_000_000_000) as _,
    });
    let flags = match scope {
        Scope::Private => Flags::PRIVATE,
        Scope::Shared => Flags::empty(),
    };
    futex::wait(word, flags, expected, timeout.as_ref())
        .map_err(|error| classify(error.raw_os_error()))
}

/// Wake up to `count` threads sleeping on `word`.
///
/// # Safety
///
/// `word` must remain at a stable, naturally aligned address for the call.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn wake_u32(
    word: &AtomicU32,
    count: WakeCount,
    scope: Scope,
) -> Result<usize, i32> {
    use rustix::thread::futex::{self, Flags};

    let flags = match scope {
        Scope::Private => Flags::PRIVATE,
        Scope::Shared => Flags::empty(),
    };
    let count = match count {
        WakeCount::One => 1,
        WakeCount::All => i32::MAX as u32,
    };
    futex::wake(word, flags, count)
        .map(|woken| woken as usize)
        .map_err(|error| error.raw_os_error())
}

/// Portability fallback for hosts without a kernel wait-on-address primitive.
/// It is intentionally inefficient, but uses one deadline rather than restarting a
/// relative timeout after every spurious return.
#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn wait_u32(
    word: &AtomicU32,
    expected: u32,
    timeout_ns: Option<u64>,
    _scope: Scope,
) -> Result<(), WaitError> {
    use std::sync::atomic::Ordering;

    let deadline = timeout_ns.map(|nanoseconds| {
        std::time::Instant::now() + std::time::Duration::from_nanos(nanoseconds)
    });
    loop {
        if word.load(Ordering::Relaxed) != expected {
            return Err(WaitError::Changed);
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(WaitError::TimedOut);
        }
        std::thread::yield_now();
        if deadline.is_none() {
            return Ok(());
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) unsafe fn wake_u32(
    _word: &AtomicU32,
    _count: WakeCount,
    _scope: Scope,
) -> Result<usize, i32> {
    // Polling waiters need no notification. Report a possible waiter so the ulock
    // adapter retains its historical non-Linux behavior.
    Ok(1)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn compare_mismatch_is_distinct_from_a_kernel_failure() {
        let word = AtomicU32::new(2);
        assert_eq!(
            unsafe { wait_u32(&word, 1, None, Scope::Private) },
            Err(WaitError::Changed)
        );
        assert_eq!(
            unsafe { wait_u32(&word, 1, None, Scope::Shared) },
            Err(WaitError::Changed)
        );
    }

    #[test]
    fn relative_timeout_expires_once() {
        let word = AtomicU32::new(1);
        let start = std::time::Instant::now();
        assert_eq!(
            unsafe { wait_u32(&word, 1, Some(20_000_000), Scope::Private) },
            Err(WaitError::TimedOut)
        );
        assert!(start.elapsed() >= std::time::Duration::from_millis(10));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn wake_one_releases_at_most_one_and_wake_all_releases_the_rest() {
        const WAITERS: u32 = 4;
        let word = Arc::new(AtomicU32::new(7));
        let entered = Arc::new(AtomicU32::new(0));
        let returned = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..WAITERS)
            .map(|_| {
                let word = Arc::clone(&word);
                let entered = Arc::clone(&entered);
                let returned = Arc::clone(&returned);
                std::thread::spawn(move || {
                    entered.fetch_add(1, Ordering::Release);
                    let _ = unsafe { wait_u32(&word, 7, None, Scope::Private) };
                    returned.fetch_add(1, Ordering::Release);
                })
            })
            .collect();
        while entered.load(Ordering::Acquire) != WAITERS {
            std::thread::yield_now();
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let woke = loop {
            let woke = unsafe { wake_u32(&word, WakeCount::One, Scope::Private) }.unwrap();
            if woke != 0 {
                break woke;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waiters entered the kernel"
            );
            std::thread::yield_now();
        };
        assert_eq!(woke, 1);
        while returned.load(Ordering::Acquire) != 1 {
            assert!(std::time::Instant::now() < deadline, "one waiter returned");
            std::thread::yield_now();
        }

        word.store(8, Ordering::Release);
        let woke = unsafe { wake_u32(&word, WakeCount::All, Scope::Private) }.unwrap();
        assert_eq!(woke, (WAITERS - 1) as usize);
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(returned.load(Ordering::Acquire), WAITERS);
    }
}
