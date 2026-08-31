//! `SCNetworkReachability`: asked of the host where there is one, declined where
//! there is not.
//!
//! A program asks this whether a host is reachable *before* trying, so that it
//! can skip the attempt or tell the user why. What the framework reports is the
//! network configuration daemon's opinion rather than a probe, and a runtime
//! that invented one would have programs skipping requests that would have
//! worked. On macOS the daemon is right there and its opinion is the true one,
//! so the synchronous half is forwarded - see [`crate::foreign`], which is what
//! lets the target Apple's framework hands back be released again.
//!
//! Off Darwin there is no such daemon, and no answer to invent:
//! `SCNetworkReachabilityCreateWithAddress` returns NULL, which is the
//! documented answer for "a reachability reference could not be created" and one
//! every caller already has to handle - Apple's returns it for a bad address.
//! Everything downstream then answers false, which is what those functions do
//! for a null target on Apple's too.
//!
//! The *asynchronous* half is declined on both hosts. Scheduling a target hands
//! the framework a `CFRunLoopRef`, and this tree's run loop is not Apple's: a
//! foreign target scheduled on a native loop would be read as a structure it is
//! not. A caller that polls `GetFlags` gets the right answer; one that waits for
//! a callback is told, by the false return, that it will not arrive.
//!
//! Published rather than left out for the reason the run loop's mode names are:
//! an unresolved *function* import is refused at the call, which stops a program
//! that only wanted to print "offline".

// The C spellings are the interface, as they are in `api`.
#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::base::CFTypeRef;

/// The host's SystemConfiguration entry point of that name, or `None`.
///
/// The framework is not linked by this tree, so it is opened on demand. A host
/// that has no SystemConfiguration - anything but macOS - never finds one, which
/// is what makes every caller below fall through to the declining answer.
#[cfg(target_os = "macos")]
fn host(name: &std::ffi::CStr) -> Option<*mut c_void> {
    use std::sync::OnceLock;
    static LOADED: OnceLock<bool> = OnceLock::new();
    if !crate::foreign::forwarding() {
        return None;
    }
    // Opened once and never closed: the entry points below are called for the
    // life of the process. `RTLD_DEFAULT` alone is not enough - nothing in this
    // tree links the framework, so it may not be in the process yet.
    LOADED.get_or_init(|| {
        // SAFETY: a NUL-terminated path, and a handle deliberately leaked.
        !unsafe {
            libc::dlopen(
                c"/System/Library/Frameworks/SystemConfiguration.framework/SystemConfiguration"
                    .as_ptr(),
                libc::RTLD_LAZY,
            )
        }
        .is_null()
    });
    crate::foreign::entry(name)
}

#[cfg(not(target_os = "macos"))]
fn host(_name: &std::ffi::CStr) -> Option<*mut c_void> {
    None
}

/// `SCNetworkReachabilityRef SCNetworkReachabilityCreateWithAddress(CFAllocatorRef, const struct sockaddr *)`
pub unsafe extern "C" fn SCNetworkReachabilityCreateWithAddress(
    allocator: CFTypeRef,
    address: *const c_void,
) -> CFTypeRef {
    let Some(f) = host(c"SCNetworkReachabilityCreateWithAddress") else {
        return std::ptr::null();
    };
    // SAFETY: the declaration above is the framework's own, and `address` is the
    // caller's `struct sockaddr` - a host layout on the one host this reaches.
    // The allocator is passed along; every caller in practice passes the default,
    // which is null in Apple's CoreFoundation as well as in this one.
    unsafe {
        let f: unsafe extern "C" fn(CFTypeRef, *const c_void) -> CFTypeRef =
            std::mem::transmute(f);
        f(allocator, address)
    }
}

/// `SCNetworkReachabilityRef SCNetworkReachabilityCreateWithName(CFAllocatorRef, const char *)`
pub unsafe extern "C" fn SCNetworkReachabilityCreateWithName(
    allocator: CFTypeRef,
    name: *const c_void,
) -> CFTypeRef {
    let Some(f) = host(c"SCNetworkReachabilityCreateWithName") else {
        return std::ptr::null();
    };
    // SAFETY: as above; `name` is the caller's NUL-terminated host name.
    unsafe {
        let f: unsafe extern "C" fn(CFTypeRef, *const c_void) -> CFTypeRef =
            std::mem::transmute(f);
        f(allocator, name)
    }
}

/// `Boolean SCNetworkReachabilityGetFlags(SCNetworkReachabilityRef, SCNetworkReachabilityFlags *)`
///
/// False for a target this tree did not get from the host, and the flags are
/// left alone: a caller that reads them after a false return is reading its own
/// uninitialised memory on Apple's too.
pub unsafe extern "C" fn SCNetworkReachabilityGetFlags(target: CFTypeRef, flags: *mut u32) -> u8 {
    // Every target that exists came from the host, because that is the only
    // place [`SCNetworkReachabilityCreateWithName`] gets one. Checked rather
    // than assumed: reading one of this runtime's objects as Apple's structure
    // is the failure `crate::foreign` exists to stop.
    // SAFETY: the caller's obligation - null or an object.
    if target.is_null() || !unsafe { crate::foreign::is(target) } {
        return 0;
    }
    let Some(f) = host(c"SCNetworkReachabilityGetFlags") else {
        return 0;
    };
    // SAFETY: a target Apple's framework made, handed back to it, and a `u32`
    // out-parameter the caller owns.
    unsafe {
        let f: unsafe extern "C" fn(CFTypeRef, *mut u32) -> u8 = std::mem::transmute(f);
        f(target, flags)
    }
}

/// `Boolean SCNetworkReachabilitySetCallback(SCNetworkReachabilityRef, SCNetworkReachabilityCallBack, SCNetworkReachabilityContext *)`
///
/// Declined even where the target is the host's: a callback only ever arrives
/// through the run loop the target is scheduled on, and scheduling is what
/// cannot be forwarded. See the module note.
pub unsafe extern "C" fn SCNetworkReachabilitySetCallback(
    _target: CFTypeRef,
    _callback: *const c_void,
    _context: *mut c_void,
) -> u8 {
    0
}

/// `Boolean SCNetworkReachabilityScheduleWithRunLoop(SCNetworkReachabilityRef, CFRunLoopRef, CFStringRef)`
pub unsafe extern "C" fn SCNetworkReachabilityScheduleWithRunLoop(
    _target: CFTypeRef,
    _loop: CFTypeRef,
    _mode: CFTypeRef,
) -> u8 {
    0
}

/// `Boolean SCNetworkReachabilityUnscheduleFromRunLoop(SCNetworkReachabilityRef, CFRunLoopRef, CFStringRef)`
pub unsafe extern "C" fn SCNetworkReachabilityUnscheduleFromRunLoop(
    _target: CFTypeRef,
    _loop: CFTypeRef,
    _mode: CFTypeRef,
) -> u8 {
    0
}

/// `CFDictionaryRef SCDynamicStoreCopyProxies(SCDynamicStoreRef)`
///
/// NULL, which is what Apple's returns when there is no proxy configuration to
/// report - and there is none here.
pub unsafe extern "C" fn SCDynamicStoreCopyProxies(_store: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "SCNetworkReachabilityCreateWithAddress" => at!(SCNetworkReachabilityCreateWithAddress),
        "SCNetworkReachabilityCreateWithName" => at!(SCNetworkReachabilityCreateWithName),
        "SCNetworkReachabilityGetFlags" => at!(SCNetworkReachabilityGetFlags),
        "SCNetworkReachabilitySetCallback" => at!(SCNetworkReachabilitySetCallback),
        "SCNetworkReachabilityScheduleWithRunLoop" => {
            at!(SCNetworkReachabilityScheduleWithRunLoop)
        }
        "SCNetworkReachabilityUnscheduleFromRunLoop" => {
            at!(SCNetworkReachabilityUnscheduleFromRunLoop)
        }
        "SCDynamicStoreCopyProxies" => at!(SCDynamicStoreCopyProxies),
        _ => return None,
    })
}

pub const NAMES: &[&str] = &[
    "SCNetworkReachabilityCreateWithAddress",
    "SCNetworkReachabilityCreateWithName",
    "SCNetworkReachabilityGetFlags",
    "SCNetworkReachabilitySetCallback",
    "SCNetworkReachabilityScheduleWithRunLoop",
    "SCNetworkReachabilityUnscheduleFromRunLoop",
    "SCDynamicStoreCopyProxies",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&&str> = NAMES.iter().filter(|n| symbol(n).is_none()).collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    /// Off Darwin no target is ever created, so nothing downstream is handed one
    /// this module cannot describe.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn no_reachability_target_is_ever_made() {
        // SAFETY: both take a null allocator and a null argument, which is what
        // "no target" means to them.
        unsafe {
            assert!(
                SCNetworkReachabilityCreateWithAddress(std::ptr::null(), std::ptr::null())
                    .is_null()
            );
            assert!(
                SCNetworkReachabilityCreateWithName(std::ptr::null(), std::ptr::null()).is_null()
            );
        }
    }

    /// The property that keeps the forwarding safe: a target that is not the
    /// host's is never read as one. Nothing here makes an object of this
    /// runtime's a target, so a string stands in for whatever a guest might
    /// pass by mistake.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_target_this_runtime_made_is_refused_rather_than_forwarded() {
        crate::bootstrap();
        let not_a_target = crate::string::from_bytes(b"not a target", false);
        let mut flags = 0u32;
        // SAFETY: a live object of this runtime's, and a `u32` out-parameter.
        unsafe {
            assert_eq!(SCNetworkReachabilityGetFlags(not_a_target, &mut flags), 0);
            assert_eq!(flags, 0, "the flags of a refused target are left alone");
            crate::api::CFRelease(not_a_target);
        }
    }

    /// The host's own opinion, asked the way a program asks it. Reachability to
    /// a name that never resolves is still a *reachable* answer - the flags say
    /// how - so the assertion is that a target is made at all, which is what
    /// declining to answer never did.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_name_gets_a_target_from_the_host() {
        crate::bootstrap();
        // SAFETY: a null allocator is the default one, and the name is a
        // NUL-terminated C string.
        unsafe {
            let target =
                SCNetworkReachabilityCreateWithName(std::ptr::null(), c"localhost".as_ptr().cast());
            assert!(!target.is_null(), "the host answers for a resolvable name");
            let mut flags = 0u32;
            assert_ne!(
                SCNetworkReachabilityGetFlags(target, &mut flags),
                0,
                "a target the host made answers its own flags"
            );
            crate::api::CFRelease(target);
        }
    }
}
