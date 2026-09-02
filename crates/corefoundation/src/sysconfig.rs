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

use objc::class::Class;

use crate::base::CFTypeRef;
use crate::callbacks::Policy;

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
    if target.is_null() {
        return 0;
    }

    #[cfg(target_os = "macos")]
    if !unsafe { crate::foreign::is(target) } {
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


/// An `SCDynamicStoreRef`.
///
/// A handle and nothing else: the store below answers from the host's own
/// interfaces rather than from a session, and the name and callback a guest
/// passes name a notification channel this tree does not deliver on. It is a
/// real object of this runtime's because the caller releases it with
/// `CFRelease`, which walks the isa.
#[repr(C)]
struct Store {
    isa: *const Class,
}

fn store_class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        let class = crate::base::register(c"SCDynamicStore", None, size_of::<Store>(), true);
        objc::class::ptr(class) as usize
    });
    // SAFETY: the cell holds a pointer to a class this function registered.
    unsafe { objc::class::from_ptr(ptr as *const Class).expect("a class") }
}

/// `SCDynamicStoreRef SCDynamicStoreCreate(CFAllocatorRef, CFStringRef, SCDynamicStoreCallBack, SCDynamicStoreContext *)`
///
/// The store is created rather than declined, because a caller that gets NULL
/// here asks nothing further - and the keys below can be answered.
pub unsafe extern "C" fn SCDynamicStoreCreate(
    _allocator: CFTypeRef,
    _name: CFTypeRef,
    _callout: *const c_void,
    _context: *mut c_void,
) -> CFTypeRef {
    // SAFETY: `Store` begins with `isa` and is the layout `store_class` was
    // registered with.
    let store: *mut Store = unsafe { crate::base::allocate(store_class()) };
    store as CFTypeRef
}

/// `CFStringRef SCDynamicStoreKeyCreateNetworkGlobalEntity(CFAllocatorRef, CFStringRef domain, CFStringRef entity)`
///
/// The key is a path, and its spelling is the interface: `State:` and `IPv4`
/// make `State:/Network/Global/IPv4`, which is what [`SCDynamicStoreCopyValue`]
/// matches on.
pub unsafe extern "C" fn SCDynamicStoreKeyCreateNetworkGlobalEntity(
    _allocator: CFTypeRef,
    domain: CFTypeRef,
    entity: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - null or an object of this runtime's.
    let (Some(domain), Some(entity)) = (unsafe { text_of(domain) }, unsafe { text_of(entity) })
    else {
        return std::ptr::null();
    };
    let mut key = domain;
    key.extend_from_slice(b"/Network/Global/");
    key.extend_from_slice(&entity);
    crate::string::from_bytes(&key, false)
}

/// `CFPropertyListRef SCDynamicStoreCopyValue(SCDynamicStoreRef, CFStringRef key)`
///
/// One key is answered: the global IPv4 entity, whose `PrimaryInterface` is what
/// a program reads to find out how it is on the network. Every other key is NULL,
/// which is what Apple's returns for a key the store does not hold.
pub unsafe extern "C" fn SCDynamicStoreCopyValue(_store: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(key) = (unsafe { text_of(key) }) else {
        return std::ptr::null();
    };
    if key != b"State:/Network/Global/IPv4" {
        return std::ptr::null();
    }
    let Some(interface) = primary_interface() else {
        return std::ptr::null();
    };
    let name = crate::string::from_bytes(interface.as_bytes(), false);
    let value = crate::collections::new_dictionary(
        &[(interned(PRIMARY_INTERFACE), name)],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    // The dictionary retained it; this call's own reference is spent.
    // SAFETY: a string this call just made.
    unsafe { crate::api::CFRelease(name) };
    value
}

/// `SCPreferencesRef SCPreferencesCreate(CFAllocatorRef, CFStringRef, CFStringRef)`
///
/// NULL: the preferences file is the *system's* network configuration, and
/// nothing here has one to read. Apple's returns NULL for a caller that may not
/// open it, so a program already handles the answer.
pub unsafe extern "C" fn SCPreferencesCreate(
    _allocator: CFTypeRef,
    _name: CFTypeRef,
    _prefs_id: CFTypeRef,
) -> CFTypeRef {
    std::ptr::null()
}

/// `CFPropertyListRef SCPreferencesGetValue(SCPreferencesRef, CFStringRef)`
pub unsafe extern "C" fn SCPreferencesGetValue(_prefs: CFTypeRef, _key: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

/// `SCNetworkServiceRef SCNetworkServiceCopy(SCPreferencesRef, CFStringRef)`
///
/// NULL, and it cannot be otherwise: the only way to a service is through the
/// preferences [`SCPreferencesCreate`] declines to open.
pub unsafe extern "C" fn SCNetworkServiceCopy(
    _prefs: CFTypeRef,
    _service_id: CFTypeRef,
) -> CFTypeRef {
    std::ptr::null()
}

/// `SCNetworkInterfaceRef SCNetworkServiceGetInterface(SCNetworkServiceRef)`
pub unsafe extern "C" fn SCNetworkServiceGetInterface(_service: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

/// `CFStringRef SCNetworkInterfaceGetInterfaceType(SCNetworkInterfaceRef)`
pub unsafe extern "C" fn SCNetworkInterfaceGetInterfaceType(_interface: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

/// The characters of a string of this runtime's, or `None` for anything else.
///
/// # Safety
///
/// `string` must be null or an object this runtime allocated.
unsafe fn text_of(string: CFTypeRef) -> Option<Vec<u8>> {
    // SAFETY: the caller's obligation.
    unsafe { crate::string::as_string(string) }.map(|s| s.utf8())
}

/// The interface packets to the outside world leave by.
///
/// Asked of the routing table rather than guessed at, and asked without sending
/// anything: `connect` on a datagram socket picks the route and binds a local
/// address, which `getifaddrs` then names. `None` when nothing is routable,
/// which is the honest answer for a machine that is offline.
fn primary_interface() -> Option<String> {
    let local = routed_source_address()?;
    // SAFETY: `getifaddrs` fills in the pointer or returns non-zero.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut found = None;
        let mut entry = list;
        while !entry.is_null() {
            let address = (*entry).ifa_addr;
            if !address.is_null() && (*address).sa_family as i32 == libc::AF_INET {
                let address = &*(address as *const libc::sockaddr_in);
                if address.sin_addr.s_addr == local {
                    found = std::ffi::CStr::from_ptr((*entry).ifa_name)
                        .to_str()
                        .ok()
                        .map(str::to_string);
                    break;
                }
            }
            entry = (*entry).ifa_next;
        }
        libc::freeifaddrs(list);
        found
    }
}

/// The local IPv4 address a packet to the public internet would go out from, in
/// network order.
fn routed_source_address() -> Option<u32> {
    // SAFETY: every call below is checked, and the socket is closed on every
    // path out.
    unsafe {
        let socket = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if socket < 0 {
            return None;
        }
        // A public address that is not contacted: `connect` on a datagram
        // socket only consults the routing table.
        let mut to: libc::sockaddr_in = std::mem::zeroed();
        to.sin_family = libc::AF_INET as libc::sa_family_t;
        to.sin_port = 53u16.to_be();
        to.sin_addr.s_addr = u32::from_be_bytes([1, 1, 1, 1]).to_be();
        let mut local: libc::sockaddr_in = std::mem::zeroed();
        let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let routed = libc::connect(
            socket,
            std::ptr::from_ref(&to).cast(),
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) == 0
            && libc::getsockname(socket, std::ptr::from_mut(&mut local).cast(), &mut len) == 0;
        libc::close(socket);
        routed.then_some(local.sin_addr.s_addr)
    }
}

/// `kSCDynamicStorePropNetPrimaryInterface`, which is also the dictionary key
/// the answer above is read out of.
const PRIMARY_INTERFACE: &str = "PrimaryInterface";

/// The `kSC*` globals, each of which is a `CFStringRef` **variable** holding the
/// path component it names.
///
/// Published even where nothing here answers a call that uses one - see
/// `corefoundation::data_symbols`: an unresolved data import binds to zero and
/// the guest dereferences it.
const KEY_STRINGS: &[(&str, &str)] = &[
    ("_kSCDynamicStoreDomainState", "State:"),
    ("_kSCDynamicStoreDomainSetup", "Setup:"),
    (
        "_kSCDynamicStorePropNetPrimaryInterface",
        PRIMARY_INTERFACE,
    ),
    ("_kSCDynamicStorePropNetPrimaryService", "PrimaryService"),
    ("_kSCEntNetIPv4", "IPv4"),
    ("_kSCEntNetIPv6", "IPv6"),
    ("_kSCEntNetInterface", "Interface"),
    ("_kSCEntNetLink", "Link"),
    ("_kSCPrefNetworkServices", "NetworkServices"),
    ("_kSCPropNetInterfaceDeviceName", "DeviceName"),
    ("_kSCPropNetInterfaceType", "Type"),
    ("_kSCNetworkInterfaceTypeEthernet", "Ethernet"),
    ("_kSCNetworkInterfaceTypeIEEE80211", "IEEE80211"),
];

/// One interned string per distinct text, so that a guest comparing a key it
/// read back against the global gets identity as well as equality.
fn interned(text: &str) -> CFTypeRef {
    static STRINGS: std::sync::Mutex<Vec<(String, usize)>> = std::sync::Mutex::new(Vec::new());
    let mut strings = STRINGS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, object)) = strings.iter().find(|(t, _)| t == text) {
        return *object as CFTypeRef;
    }
    let object = crate::string::from_bytes(text.as_bytes(), false);
    // Retained forever: a global a guest may release is one it can make dangle.
    objc::arc::retain(object as objc::object::Id);
    strings.push((text.to_string(), object as usize));
    object
}

/// The `kSC*` globals, as `(Mach-O name, address)`.
pub fn data_symbols() -> Vec<(String, u64)> {
    crate::bootstrap();
    KEY_STRINGS
        .iter()
        .map(|(symbol, text)| {
            (
                (*symbol).to_string(),
                crate::base::global_cell(symbol, interned(text) as usize as u64),
            )
        })
        .collect()
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
        "SCDynamicStoreCreate" => at!(SCDynamicStoreCreate),
        "SCDynamicStoreKeyCreateNetworkGlobalEntity" => {
            at!(SCDynamicStoreKeyCreateNetworkGlobalEntity)
        }
        "SCDynamicStoreCopyValue" => at!(SCDynamicStoreCopyValue),
        "SCPreferencesCreate" => at!(SCPreferencesCreate),
        "SCPreferencesGetValue" => at!(SCPreferencesGetValue),
        "SCNetworkServiceCopy" => at!(SCNetworkServiceCopy),
        "SCNetworkServiceGetInterface" => at!(SCNetworkServiceGetInterface),
        "SCNetworkInterfaceGetInterfaceType" => at!(SCNetworkInterfaceGetInterfaceType),
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
    "SCDynamicStoreCreate",
    "SCDynamicStoreKeyCreateNetworkGlobalEntity",
    "SCDynamicStoreCopyValue",
    "SCPreferencesCreate",
    "SCPreferencesGetValue",
    "SCNetworkServiceCopy",
    "SCNetworkServiceGetInterface",
    "SCNetworkInterfaceGetInterfaceType",
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
