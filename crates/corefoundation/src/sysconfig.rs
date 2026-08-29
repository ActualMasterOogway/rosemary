//! `SCNetworkReachability`, answered by declining to answer.
//!
//! A program asks this whether a host is reachable *before* trying, so that it
//! can skip the attempt or tell the user why. There is no equivalent question on
//! a host with no SystemConfiguration: what the framework reports is the network
//! configuration daemon's opinion, not a probe, and a runtime that invented one
//! would have programs skipping requests that would have worked or making ones
//! that cannot.
//!
//! So `SCNetworkReachabilityCreateWithAddress` returns NULL, which is the
//! documented answer for "a reachability reference could not be created" and one
//! every caller already has to handle - Apple's returns it for a bad address.
//! Everything downstream then answers false, which is what those functions do
//! for a null target on Apple's too.
//!
//! Published rather than left out for the reason the run loop's mode names are:
//! an unresolved *function* import is refused at the call, which stops a program
//! that only wanted to print "offline".

// The C spellings are the interface, as they are in `api`.
#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::base::CFTypeRef;

/// `SCNetworkReachabilityRef SCNetworkReachabilityCreateWithAddress(CFAllocatorRef, const struct sockaddr *)`
pub unsafe extern "C" fn SCNetworkReachabilityCreateWithAddress(
    _allocator: CFTypeRef,
    _address: *const c_void,
) -> CFTypeRef {
    std::ptr::null()
}

/// `SCNetworkReachabilityRef SCNetworkReachabilityCreateWithName(CFAllocatorRef, const char *)`
pub unsafe extern "C" fn SCNetworkReachabilityCreateWithName(
    _allocator: CFTypeRef,
    _name: *const c_void,
) -> CFTypeRef {
    std::ptr::null()
}

/// `Boolean SCNetworkReachabilityGetFlags(SCNetworkReachabilityRef, SCNetworkReachabilityFlags *)`
///
/// False, and the flags are left alone: a caller that reads them after a false
/// return is reading its own uninitialised memory on Apple's too.
pub unsafe extern "C" fn SCNetworkReachabilityGetFlags(
    _target: CFTypeRef,
    _flags: *mut u32,
) -> u8 {
    0
}

/// `Boolean SCNetworkReachabilitySetCallback(SCNetworkReachabilityRef, SCNetworkReachabilityCallBack, SCNetworkReachabilityContext *)`
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

    /// The one property everything else depends on: a target is never created,
    /// so nothing downstream is ever handed one this module cannot describe.
    #[test]
    fn no_reachability_target_is_ever_made() {
        // SAFETY: both take a null allocator and a null argument, which is what
        // "no target" means to them.
        unsafe {
            assert!(
                SCNetworkReachabilityCreateWithAddress(
                    std::ptr::null(),
                    std::ptr::null()
                )
                .is_null()
            );
            assert!(
                SCNetworkReachabilityCreateWithName(std::ptr::null(), std::ptr::null()).is_null()
            );
        }
    }
}
