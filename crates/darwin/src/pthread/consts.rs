//! Darwin's pthread constants, and what each means to the host.
//!
//! These are **the guest's** numbers, so they are written out here rather than taken
//! from the `libc` crate. Almost none of them agrees with Linux, and the two that
//! matter most are not merely different but *swapped*:
//!
//! | | Darwin | glibc / musl |
//! |---|---|---|
//! | `PTHREAD_MUTEX_NORMAL` | 0 | 0 |
//! | `PTHREAD_MUTEX_ERRORCHECK` | 1 | **2** |
//! | `PTHREAD_MUTEX_RECURSIVE` | 2 | **1** |
//!
//! A guest asking for a recursive mutex and getting an error-checking one does not
//! fail loudly: it takes the lock, recurses, and gets `EDEADLK` from a call whose
//! return value nobody checks - or deadlocks outright. That is the whole reason this
//! file exists.
//!
//! `PTHREAD_CREATE_JOINABLE` is 1 on Darwin and 0 on Linux, and `PTHREAD_CREATE_DETACHED`
//! is 2 and 1 - so a guest that passes the *default* detach state explicitly gets a
//! detached thread, and its `pthread_join` fails on a thread that is already gone.

/// `pthread_attr_setdetachstate`.
pub const PTHREAD_CREATE_JOINABLE: i32 = 1;
pub const PTHREAD_CREATE_DETACHED: i32 = 2;

/// `pthread_attr_setinheritsched`.
pub const PTHREAD_INHERIT_SCHED: i32 = 1;
pub const PTHREAD_EXPLICIT_SCHED: i32 = 2;

/// `pthread_attr_setscope`.
pub const PTHREAD_SCOPE_SYSTEM: i32 = 1;
pub const PTHREAD_SCOPE_PROCESS: i32 = 2;

/// `pthread_*attr_setpshared`.
pub const PTHREAD_PROCESS_SHARED: i32 = 1;
pub const PTHREAD_PROCESS_PRIVATE: i32 = 2;

/// `pthread_mutexattr_setprotocol`. The same three numbers on both, for once.
pub const PTHREAD_PRIO_NONE: i32 = 0;
pub const PTHREAD_PRIO_INHERIT: i32 = 1;
pub const PTHREAD_PRIO_PROTECT: i32 = 2;

/// `pthread_mutexattr_settype`. See the module note.
pub const PTHREAD_MUTEX_NORMAL: i32 = 0;
pub const PTHREAD_MUTEX_ERRORCHECK: i32 = 1;
pub const PTHREAD_MUTEX_RECURSIVE: i32 = 2;
pub const PTHREAD_MUTEX_DEFAULT: i32 = PTHREAD_MUTEX_NORMAL;

/// `pthread_mutexattr_setpolicy_np`. Darwin-only; the host has no equivalent, and
/// neither policy changes what a correct program observes.
pub const PTHREAD_MUTEX_POLICY_FAIRSHARE_NP: i32 = 1;
pub const PTHREAD_MUTEX_POLICY_FIRSTFIT_NP: i32 = 3;

/// `pthread_setcancelstate` and `pthread_setcanceltype`.
///
/// Both pairs are inverted relative to Linux, and `ENABLE`/`ASYNCHRONOUS` share the
/// value `0` on Darwin while `DISABLE`/`DEFERRED` share it on Linux - which is as
/// error-prone as it looks, and why the translation below is written per-call rather
/// than as one table.
pub const PTHREAD_CANCEL_ENABLE: i32 = 0x01;
pub const PTHREAD_CANCEL_DISABLE: i32 = 0x00;
pub const PTHREAD_CANCEL_DEFERRED: i32 = 0x02;
pub const PTHREAD_CANCEL_ASYNCHRONOUS: i32 = 0x00;

/// The value a cancelled thread's `pthread_join` reports.
pub const PTHREAD_CANCELED: u64 = 1;

/// Scheduling policies, from `<pthread/pthread_impl.h>`.
pub const SCHED_OTHER: i32 = 1;
pub const SCHED_FIFO: i32 = 4;
pub const SCHED_RR: i32 = 2;

/// `qos_class_t`, from `<sys/qos.h>`. The numbers are the kernel's thread-QoS
/// indices, not an ordering the host shares, so a class is only ever compared against
/// these names.
pub const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
pub const QOS_CLASS_USER_INITIATED: u32 = 0x19;
pub const QOS_CLASS_DEFAULT: u32 = 0x15;
pub const QOS_CLASS_UTILITY: u32 = 0x11;
pub const QOS_CLASS_BACKGROUND: u32 = 0x09;
pub const QOS_CLASS_UNSPECIFIED: u32 = 0x00;

/// The most negative `relative_priority` a QoS class accepts; 0 is the least.
pub const QOS_MIN_RELATIVE_PRIORITY: i32 = -15;

/// Darwin's `errno` values this module returns.
///
/// **Darwin's** numbers, written out, not the `libc` crate's. `pthread_*`
/// returns its error rather than setting `errno`, so these cross the seam as
/// return values and the guest compares them against its own header's numbers.
/// Five of the nine below disagree between the platforms - `EDEADLK` is 11 on
/// Darwin and 35 here, `EAGAIN` 35 and 11, `ETIMEDOUT` 60 and 110 - and each
/// disagreement is with a number that names a real error on the other side. See
/// [`crate::errno`] for the whole table.
pub mod err {
    pub const EINVAL: i32 = 22;
    pub const EBUSY: i32 = 16;
    pub const EDEADLK: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EPERM: i32 = 1;
    pub const ESRCH: i32 = 3;
    pub const ETIMEDOUT: i32 = 60;
    pub const ENOTSUP: i32 = 45;
    pub const EAGAIN: i32 = 35;

    /// A host `pthread_*` return value in Darwin's numbering.
    ///
    /// Every host call whose result this module hands back has to go through
    /// this: `libc++`'s `condition_variable::wait_for` compares the result of
    /// `pthread_cond_timedwait` against `ETIMEDOUT` and throws for anything
    /// else, so an untranslated 110 turns a timeout into a `system_error` with
    /// no message - Darwin has no error 110 to name.
    pub fn from_host(rc: i32) -> i32 {
        crate::errno::from_host(rc)
    }
}

/// Darwin's mutex type as the host's.
pub fn mutex_type_to_host(darwin: i32) -> Option<libc::c_int> {
    Some(match darwin {
        PTHREAD_MUTEX_NORMAL => libc::PTHREAD_MUTEX_NORMAL,
        PTHREAD_MUTEX_ERRORCHECK => libc::PTHREAD_MUTEX_ERRORCHECK,
        PTHREAD_MUTEX_RECURSIVE => libc::PTHREAD_MUTEX_RECURSIVE,
        _ => return None,
    })
}

/// Darwin's detach state as the host's.
pub fn detach_state_to_host(darwin: i32) -> Option<libc::c_int> {
    Some(match darwin {
        PTHREAD_CREATE_JOINABLE => libc::PTHREAD_CREATE_JOINABLE,
        PTHREAD_CREATE_DETACHED => libc::PTHREAD_CREATE_DETACHED,
        _ => return None,
    })
}

/// Darwin's scheduling policy as the host's.
pub fn sched_policy_to_host(darwin: i32) -> Option<libc::c_int> {
    Some(match darwin {
        SCHED_OTHER => libc::SCHED_OTHER,
        SCHED_FIFO => libc::SCHED_FIFO,
        SCHED_RR => libc::SCHED_RR,
        _ => return None,
    })
}

/// The host's scheduling policy as Darwin's, for a getter.
pub fn sched_policy_from_host(host: libc::c_int) -> i32 {
    match host {
        libc::SCHED_FIFO => SCHED_FIFO,
        libc::SCHED_RR => SCHED_RR,
        _ => SCHED_OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants are Darwin's, checked against the SDK's own declarations where
    /// this build has them. The point of the file is that they are *not* the host's,
    /// so on macOS this is a real cross-check and on Linux it would be a tautology.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_constants_are_the_ones_darwin_declares() {
        assert_eq!(PTHREAD_MUTEX_NORMAL, libc::PTHREAD_MUTEX_NORMAL);
        assert_eq!(PTHREAD_MUTEX_ERRORCHECK, libc::PTHREAD_MUTEX_ERRORCHECK);
        assert_eq!(PTHREAD_MUTEX_RECURSIVE, libc::PTHREAD_MUTEX_RECURSIVE);
        assert_eq!(PTHREAD_CREATE_JOINABLE, libc::PTHREAD_CREATE_JOINABLE);
        assert_eq!(PTHREAD_CREATE_DETACHED, libc::PTHREAD_CREATE_DETACHED);
        assert_eq!(PTHREAD_PROCESS_SHARED, libc::PTHREAD_PROCESS_SHARED);
        assert_eq!(PTHREAD_PROCESS_PRIVATE, libc::PTHREAD_PROCESS_PRIVATE);
    }

    /// And on Linux, that they are *not* the host's - which is the statement the
    /// translation exists to act on. Written as an assertion so that a host whose
    /// numbers ever did agree would say so rather than leaving dead code behind.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_two_mutex_types_that_matter_are_swapped() {
        assert_eq!(
            mutex_type_to_host(PTHREAD_MUTEX_RECURSIVE),
            Some(libc::PTHREAD_MUTEX_RECURSIVE)
        );
        assert_ne!(
            PTHREAD_MUTEX_RECURSIVE,
            libc::PTHREAD_MUTEX_RECURSIVE,
            "if these ever agree, the translation is a no-op and this file can shrink"
        );
        assert_ne!(PTHREAD_MUTEX_ERRORCHECK, libc::PTHREAD_MUTEX_ERRORCHECK);
        assert_ne!(PTHREAD_CREATE_JOINABLE, libc::PTHREAD_CREATE_JOINABLE);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_type_nobody_declares_is_refused_rather_than_defaulted() {
        assert_eq!(mutex_type_to_host(7), None);
        assert_eq!(
            detach_state_to_host(0),
            None,
            "0 is not a Darwin detach state"
        );
        assert_eq!(sched_policy_to_host(99), None);
    }
}
