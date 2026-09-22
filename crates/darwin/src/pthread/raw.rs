//! Declarations for the pthread functions the `libc` crate does not bind.
//!
//! Every one of these is in the C library on both hosts and in POSIX; the crate simply
//! has not declared it for one platform or the other. Declared here rather than at
//! each call site so the list of what is missing is one list, and so a future `libc`
//! that grows them is one deletion.

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

    // The libc crate does not currently expose these header constants on Linux;
    // these are the glibc/musl declarations used by supported non-Apple hosts.
    #[cfg(not(target_os = "macos"))]
    pub const ENABLE: libc::c_int = 0;
    #[cfg(not(target_os = "macos"))]
    pub const DISABLE: libc::c_int = 1;
    #[cfg(not(target_os = "macos"))]
    pub const DEFERRED: libc::c_int = 0;
    #[cfg(not(target_os = "macos"))]
    pub const ASYNCHRONOUS: libc::c_int = 1;
}
