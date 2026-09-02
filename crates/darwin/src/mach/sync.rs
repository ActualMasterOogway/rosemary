//! Mach semaphores, which are named out of the port space and are not ports.
//!
//! `semaphore_t` is a `mach_port_t` on a Mac and the calls below take one, so
//! the names have to come from the same counter as the ports or a caller could
//! hold a semaphore and a port with the same number. Nothing else about them is
//! Mach-like: a semaphore is not sent to, and no message carries one.
//!
//! In-process only. A Mach semaphore *can* be shared by sending it in a message
//! and this cannot, which is a real divergence - but the sharing form needs a
//! kernel object two tasks can both block on, and the only shape of that here
//! is a futex in shared memory. Nothing this runtime has met asks for it, and a
//! semaphore that silently failed to synchronise two processes would be a
//! deadlock nobody could locate.

#![cfg(not(target_os = "macos"))]

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::kern::*;
use super::space::{self, MACH_PORT_NULL, Name, Right};

pub struct Semaphore {
    count: Mutex<i64>,
    woken: Condvar,
}

impl Semaphore {
    fn new(value: i64) -> Self {
        Semaphore {
            count: Mutex::new(value),
            woken: Condvar::new(),
        }
    }

    fn signal(&self, all: bool) {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        match all {
            // `semaphore_signal_all` releases every waiter and leaves the count
            // at zero, rather than adding one per waiter: a thread that arrives
            // afterwards blocks.
            true => {
                *count = 0;
                self.woken.notify_all();
            }
            false => {
                *count += 1;
                self.woken.notify_one();
            }
        }
    }

    fn wait(&self, deadline: Option<Instant>) -> i32 {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *count > 0 {
                *count -= 1;
                return KERN_SUCCESS;
            }
            let Some(deadline) = deadline else {
                count = self.woken.wait(count).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return KERN_OPERATION_TIMED_OUT;
            };
            let (held, timed_out) = self
                .woken
                .wait_timeout(count, left)
                .unwrap_or_else(|e| e.into_inner());
            count = held;
            if timed_out.timed_out() && *count <= 0 {
                return KERN_OPERATION_TIMED_OUT;
            }
        }
    }
}

fn held(name: Name) -> Option<Arc<Semaphore>> {
    space::with_right(name, |right| match right {
        Right::Semaphore(semaphore) => Some(semaphore.clone()),
        _ => None,
    })
    .flatten()
}

/// `kern_return_t semaphore_create(task_t, semaphore_t *, int policy, int value)`
///
/// The policy - FIFO, fixed priority - is the order waiters are released in,
/// which a condition variable does not let this choose. Accepted rather than
/// refused because no caller can observe the difference except as a fairness
/// question, and refusing would fail an initialisation over it.
///
/// # Safety
///
/// `out` is a writable `semaphore_t`.
pub unsafe extern "C" fn semaphore_create(
    task: Name,
    out: *mut Name,
    _policy: i32,
    value: i32,
) -> i32 {
    if out.is_null() {
        return KERN_INVALID_ADDRESS;
    }
    if task != space::TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if value < 0 {
        return KERN_INVALID_ARGUMENT;
    }
    let name = space::insert(Right::Semaphore(Arc::new(Semaphore::new(value as i64))));
    // SAFETY: the caller's `semaphore_t`.
    unsafe { out.write(name) };
    KERN_SUCCESS
}

/// `kern_return_t semaphore_destroy(task_t, semaphore_t)`
pub extern "C" fn semaphore_destroy(task: Name, semaphore: Name) -> i32 {
    if task != space::TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if held(semaphore).is_none() {
        return KERN_INVALID_ARGUMENT;
    }
    // Any thread still blocked holds an `Arc` of its own and is released here,
    // which is what Mach does: destroying a semaphore wakes its waiters.
    if let Some(semaphore) = held(semaphore) {
        semaphore.signal(true);
    }
    space::destroy(semaphore);
    KERN_SUCCESS
}

/// `kern_return_t semaphore_signal(semaphore_t)`
pub extern "C" fn semaphore_signal(semaphore: Name) -> i32 {
    match held(semaphore) {
        Some(semaphore) => {
            semaphore.signal(false);
            KERN_SUCCESS
        }
        None => KERN_INVALID_ARGUMENT,
    }
}

/// `kern_return_t semaphore_signal_all(semaphore_t)`
pub extern "C" fn semaphore_signal_all(semaphore: Name) -> i32 {
    match held(semaphore) {
        Some(semaphore) => {
            semaphore.signal(true);
            KERN_SUCCESS
        }
        None => KERN_INVALID_ARGUMENT,
    }
}

/// `kern_return_t semaphore_wait(semaphore_t)`
pub extern "C" fn semaphore_wait(semaphore: Name) -> i32 {
    match held(semaphore) {
        Some(semaphore) => semaphore.wait(None),
        None => KERN_INVALID_ARGUMENT,
    }
}

/// `kern_return_t semaphore_timedwait(semaphore_t, mach_timespec_t)`
///
/// `mach_timespec_t` is `{ unsigned int tv_sec; clock_res_t tv_nsec; }` - eight
/// bytes, **by value**, which the SysV AMD64 ABI passes in one integer register.
/// That is why this takes a `u64` and not a pointer: a prototype with a pointer
/// would read the two halves of the timeout as an address.
///
/// The timeout is relative, unlike `pthread_cond_timedwait`'s.
pub extern "C" fn semaphore_timedwait(semaphore: Name, timeout: u64) -> i32 {
    let seconds = timeout as u32;
    let nanoseconds = (timeout >> 32) as u32;
    let Some(semaphore) = held(semaphore) else {
        return KERN_INVALID_ARGUMENT;
    };
    let wait = Duration::new(seconds as u64, nanoseconds.min(999_999_999));
    semaphore.wait(Some(Instant::now() + wait))
}

/// `kern_return_t semaphore_signal_thread(semaphore_t, thread_t)`
///
/// Releasing one *named* thread is the part a condition variable cannot do, so
/// the thread is ignored and the semaphore is signalled: some waiter runs
/// rather than none. The alternative is refusing, which leaves the named thread
/// blocked forever - the same outcome the caller was trying to avoid, minus the
/// chance that any waiter will do.
pub extern "C" fn semaphore_signal_thread(semaphore: Name, _thread: Name) -> i32 {
    semaphore_signal(semaphore)
}

/// The `semaphore_t` a failed [`semaphore_create`] leaves behind.
pub const NO_SEMAPHORE: Name = MACH_PORT_NULL;

#[cfg(test)]
mod tests {
    use super::*;

    fn create(value: i32) -> Name {
        let mut name = NO_SEMAPHORE;
        // SAFETY: a live local for the out-parameter.
        let rc = unsafe { semaphore_create(space::TASK_SELF, &mut name, 0, value) };
        assert_eq!(rc, KERN_SUCCESS);
        name
    }

    #[test]
    fn a_semaphore_with_a_count_lets_that_many_waiters_through() {
        let semaphore = create(2);
        assert_eq!(semaphore_wait(semaphore), KERN_SUCCESS);
        assert_eq!(semaphore_wait(semaphore), KERN_SUCCESS);
        assert_eq!(semaphore_timedwait(semaphore, 0), KERN_OPERATION_TIMED_OUT);
        semaphore_destroy(space::TASK_SELF, semaphore);
    }

    /// The `mach_timespec_t` is in one register, seconds in the low half. A
    /// version that read the halves the other way round would wait for seconds
    /// where the caller asked for nanoseconds.
    #[test]
    fn a_timed_wait_reads_the_seconds_out_of_the_low_half() {
        let semaphore = create(0);
        let started = Instant::now();
        // 50 ms, spelled the way the structure lays it out.
        let timeout = (50_000_000u64 << 32) | 0;
        assert_eq!(
            semaphore_timedwait(semaphore, timeout),
            KERN_OPERATION_TIMED_OUT
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(40), "waited {waited:?}");
        assert!(waited < Duration::from_secs(5), "read the halves swapped");
        semaphore_destroy(space::TASK_SELF, semaphore);
    }

    #[test]
    fn a_signal_releases_a_thread_that_is_already_waiting() {
        let semaphore = create(0);
        let waiter = std::thread::spawn(move || semaphore_wait(semaphore));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(semaphore_signal(semaphore), KERN_SUCCESS);
        assert_eq!(waiter.join().unwrap(), KERN_SUCCESS);
        semaphore_destroy(space::TASK_SELF, semaphore);
    }

    /// `semaphore_signal_all` leaves the count at zero rather than at the
    /// number of threads it released, so a thread arriving afterwards blocks.
    #[test]
    fn signalling_all_does_not_leave_a_count_behind() {
        let semaphore = create(0);
        assert_eq!(semaphore_signal_all(semaphore), KERN_SUCCESS);
        assert_eq!(semaphore_timedwait(semaphore, 0), KERN_OPERATION_TIMED_OUT);
        semaphore_destroy(space::TASK_SELF, semaphore);
    }

    #[test]
    fn a_name_that_is_not_a_semaphore_is_refused() {
        assert_eq!(semaphore_wait(0x7fff_0000), KERN_INVALID_ARGUMENT);
        assert_eq!(semaphore_signal(space::TASK_SELF), KERN_INVALID_ARGUMENT);
    }
}
