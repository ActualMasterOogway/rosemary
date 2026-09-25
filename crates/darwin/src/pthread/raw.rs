unsafe extern "C" {
    /// Not bound for Apple platforms, though libpthread exports it.
    pub fn pthread_attr_getguardsize(
        attr: *const libc::pthread_attr_t,
        guardsize: *mut libc::size_t,
    ) -> libc::c_int;

    /// As above.
    pub fn pthread_attr_setguardsize(
        attr: *mut libc::pthread_attr_t,
        guardsize: libc::size_t,
    ) -> libc::c_int;

    /// Bound for Apple platforms and not under musl, where the crate declares only
    /// the setter.
    pub fn pthread_attr_getdetachstate(
        attr: *const libc::pthread_attr_t,
        state: *mut libc::c_int,
    ) -> libc::c_int;

    /// Cancellation, which the crate binds on Linux and not on Apple platforms.
    pub fn pthread_setcancelstate(state: libc::c_int, oldstate: *mut libc::c_int) -> libc::c_int;

    /// As above.
    pub fn pthread_setcanceltype(kind: libc::c_int, oldtype: *mut libc::c_int) -> libc::c_int;
}

// Scheduling attributes, which libpthread exports on FreeBSD and the crate
// declares only for Linux and Apple platforms.
#[cfg(target_os = "freebsd")]
unsafe extern "C" {
    pub fn pthread_attr_setinheritsched(
        attr: *mut libc::pthread_attr_t,
        inherit: libc::c_int,
    ) -> libc::c_int;

    pub fn pthread_attr_setschedpolicy(
        attr: *mut libc::pthread_attr_t,
        policy: libc::c_int,
    ) -> libc::c_int;

    pub fn pthread_attr_setschedparam(
        attr: *mut libc::pthread_attr_t,
        param: *const libc::sched_param,
    ) -> libc::c_int;
}

/// FreeBSD's `<pthread.h>` values for the two the crate does not carry.
#[cfg(target_os = "freebsd")]
pub const PTHREAD_INHERIT_SCHED: libc::c_int = 0x4;

/// As above.
#[cfg(target_os = "freebsd")]
pub const PTHREAD_EXPLICIT_SCHED: libc::c_int = 0;

#[cfg(not(target_os = "freebsd"))]
pub use libc::{
    PTHREAD_EXPLICIT_SCHED, PTHREAD_INHERIT_SCHED, pthread_attr_setinheritsched,
    pthread_attr_setschedparam, pthread_attr_setschedpolicy,
};

/// The host's own cancellation constants.
///
/// Darwin's are in [`super::consts`] because they are the *guest's*; these are what
/// this process's libpthread wants, and the two are inverted.
pub mod cancel {
    #[cfg(target_os = "macos")]
    pub const ENABLE: libc::c_int = super::super::consts::PTHREAD_CANCEL_ENABLE;
    #[cfg(target_os = "macos")]
    pub const DISABLE: libc::c_int = super::super::consts::PTHREAD_CANCEL_DISABLE;
    #[cfg(target_os = "macos")]
    pub const DEFERRED: libc::c_int = super::super::consts::PTHREAD_CANCEL_DEFERRED;
    #[cfg(target_os = "macos")]
    pub const ASYNCHRONOUS: libc::c_int = super::super::consts::PTHREAD_CANCEL_ASYNCHRONOUS;

    // The libc crate does not currently expose these header constants on Linux
    // or FreeBSD; these are each header's own values. They agree except for
    // `ASYNCHRONOUS`, which FreeBSD numbers 2 - a mismatch that would set
    // deferred cancellation where asynchronous was asked for.
    #[cfg(not(target_os = "macos"))]
    pub const ENABLE: libc::c_int = 0;
    #[cfg(not(target_os = "macos"))]
    pub const DISABLE: libc::c_int = 1;
    #[cfg(not(target_os = "macos"))]
    pub const DEFERRED: libc::c_int = 0;
    #[cfg(target_os = "freebsd")]
    pub const ASYNCHRONOUS: libc::c_int = 2;
    #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
    pub const ASYNCHRONOUS: libc::c_int = 1;
}
