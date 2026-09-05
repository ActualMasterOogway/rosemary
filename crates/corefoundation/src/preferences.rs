//! `CFPreferences`, as a store this process keeps and nothing else reads.
//!
//! Preferences on a Mac are a `cfprefsd` question, not a file one: the daemon
//! owns the domain, several processes may hold it open at once, and a program
//! that wrote a plist itself would find its own write overwritten. There is no
//! such daemon here, so the choice is between an in-process store and nothing.
//!
//! It is an in-process store, and the reason is what "nothing" costs. An
//! unimplemented `CFPreferencesCopyAppValue` is an import bound to zero, which
//! is a crash in whatever asked - and a program asks in its first second, for
//! a setting it has never written. With a store the same program reads NULL,
//! which is exactly what it reads on a Mac the first time it runs, and carries
//! on.
//!
//! What it does **not** do is survive the process, and that is worth stating
//! rather than hiding: a setting written here is read back correctly by the run
//! that wrote it and is gone at the next launch. The direction matters - a
//! program that writes a preference and reads it back within the run behaves
//! normally, and one that relies on a preference outliving the run starts each
//! time as if freshly installed.
//!
//! The domain arguments are honoured. `kCFPreferencesCurrentApplication` and an
//! explicit bundle identifier are different domains, and a store that folded
//! them together would let one program's key answer another's.

// The C spellings are the interface, as they are in `api`.
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::sync::Mutex;

use crate::base::CFTypeRef;

/// One domain's keys, each holding a retained object as an address.
///
/// Keyed by text rather than by object, because a caller builds the key string
/// afresh on each call and two equal strings are two objects. The value is an
/// address rather than a `CFTypeRef` so the table can be a `static`: a raw
/// pointer is not `Send`, and what is stored here is owned by this module for
/// the life of the process rather than shared with a thread.
type Domain = HashMap<String, usize>;

fn store() -> &'static Mutex<HashMap<String, Domain>> {
    static STORE: std::sync::OnceLock<Mutex<HashMap<String, Domain>>> = std::sync::OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// The text of a `CFStringRef` argument, or `None` for one that is not a string.
///
/// # Safety
///
/// `value` is null or an object this runtime allocated.
unsafe fn text(value: CFTypeRef) -> Option<String> {
    // SAFETY: the caller's obligation.
    unsafe { crate::string::as_string(value) }
        .map(|s| String::from_utf8_lossy(&s.utf8()).into_owned())
}

/// The domain name to file a key under.
///
/// A null application id is `kCFPreferencesAnyApplication` as far as this store
/// is concerned - one place for a caller that did not name a domain, rather
/// than a silent failure.
///
/// # Safety
///
/// `application` is null or a `CFStringRef`.
unsafe fn domain_of(application: CFTypeRef) -> String {
    // SAFETY: the caller's obligation.
    unsafe { text(application) }.unwrap_or_else(|| "kCFPreferencesAnyApplication".to_string())
}

/// `CFPropertyListRef CFPreferencesCopyAppValue(CFStringRef, CFStringRef)`
///
/// Copy, so the caller owns a reference - which is what the name says and what
/// its `CFRelease` expects.
///
/// # Safety
///
/// Both arguments are null or objects this runtime allocated.
pub unsafe extern "C" fn CFPreferencesCopyAppValue(
    key: CFTypeRef,
    application: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let (Some(key), domain) = (unsafe { text(key) }, unsafe { domain_of(application) }) else {
        return std::ptr::null();
    };
    let store = store().lock().unwrap_or_else(|e| e.into_inner());
    let Some(value) = store.get(&domain).and_then(|d| d.get(&key)).copied() else {
        return std::ptr::null();
    };
    let value = value as CFTypeRef;
    // SAFETY: an object this module owns a reference to.
    unsafe { crate::api::CFRetain(value) }
}

/// `void CFPreferencesSetAppValue(CFStringRef, CFPropertyListRef, CFStringRef)`
///
/// A null value removes the key, which is how a program clears a setting.
///
/// # Safety
///
/// As [`CFPreferencesCopyAppValue`].
pub unsafe extern "C" fn CFPreferencesSetAppValue(
    key: CFTypeRef,
    value: CFTypeRef,
    application: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let (Some(key), domain) = (unsafe { text(key) }, unsafe { domain_of(application) }) else {
        return;
    };
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    let domain = store.entry(domain).or_default();
    if let Some(previous) = domain.remove(&key) {
        // SAFETY: a reference this module took when it stored the value.
        unsafe { crate::api::CFRelease(previous as CFTypeRef) };
    }
    if !value.is_null() {
        // SAFETY: the caller's object, which this store keeps a reference to
        // for as long as the key holds it.
        domain.insert(key, unsafe { crate::api::CFRetain(value) } as usize);
    }
}

/// `CFPropertyListRef CFPreferencesCopyValue(CFStringRef key, CFStringRef
/// applicationID, CFStringRef userName, CFStringRef hostName)`
///
/// The user and host arguments are ignored: this store is one process's, so
/// every domain in it is already "current user, current host".
///
/// # Safety
///
/// As [`CFPreferencesCopyAppValue`].
pub unsafe extern "C" fn CFPreferencesCopyValue(
    key: CFTypeRef,
    application: CFTypeRef,
    _user: CFTypeRef,
    _host: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    unsafe { CFPreferencesCopyAppValue(key, application) }
}

/// `void CFPreferencesSetValue(CFStringRef, CFPropertyListRef, CFStringRef,
/// CFStringRef, CFStringRef)`
///
/// # Safety
///
/// As [`CFPreferencesCopyAppValue`].
pub unsafe extern "C" fn CFPreferencesSetValue(
    key: CFTypeRef,
    value: CFTypeRef,
    application: CFTypeRef,
    _user: CFTypeRef,
    _host: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    unsafe { CFPreferencesSetAppValue(key, value, application) }
}

/// `Boolean CFPreferencesGetAppBooleanValue(CFStringRef, CFStringRef,
/// Boolean *keyExistsAndHasValidFormat)`
///
/// The out-parameter is the whole interface: false with `*exists` false means
/// "not set", and a caller reads its own default in that case. Reporting false
/// with `*exists` **true** would tell it the user had turned the setting off.
///
/// # Safety
///
/// `exists` is null or a writable `Boolean`.
pub unsafe extern "C" fn CFPreferencesGetAppBooleanValue(
    key: CFTypeRef,
    application: CFTypeRef,
    exists: *mut u8,
) -> u8 {
    // SAFETY: the caller's obligation.
    let value = unsafe { CFPreferencesCopyAppValue(key, application) };
    // SAFETY: a value this store holds, or null.
    let number = unsafe { crate::collections::number(value) }.map(|n| n.as_i64() != 0);
    if !value.is_null() {
        // SAFETY: the reference `CFPreferencesCopyAppValue` handed over.
        unsafe { crate::api::CFRelease(value) };
    }
    if !exists.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *exists = number.is_some() as u8 };
    }
    number.unwrap_or(false) as u8
}

/// `CFIndex CFPreferencesGetAppIntegerValue(CFStringRef, CFStringRef,
/// Boolean *)`
///
/// # Safety
///
/// As [`CFPreferencesGetAppBooleanValue`].
pub unsafe extern "C" fn CFPreferencesGetAppIntegerValue(
    key: CFTypeRef,
    application: CFTypeRef,
    exists: *mut u8,
) -> i64 {
    // SAFETY: the caller's obligation.
    let value = unsafe { CFPreferencesCopyAppValue(key, application) };
    // SAFETY: as above.
    let number = unsafe { crate::collections::number(value) }.map(|n| n.as_i64());
    if !value.is_null() {
        // SAFETY: the reference just handed over.
        unsafe { crate::api::CFRelease(value) };
    }
    if !exists.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *exists = number.is_some() as u8 };
    }
    number.unwrap_or(0)
}

/// `Boolean CFPreferencesAppSynchronize(CFStringRef)`
///
/// True, because there is nothing to flush: the store *is* the value every
/// later read sees. A false here is "the write did not land", which is not the
/// case and which callers treat as a failure of whatever they were configuring.
pub extern "C" fn CFPreferencesAppSynchronize(_application: CFTypeRef) -> u8 {
    1
}

/// `Boolean CFPreferencesSynchronize(CFStringRef, CFStringRef, CFStringRef)`
pub extern "C" fn CFPreferencesSynchronize(
    _application: CFTypeRef,
    _user: CFTypeRef,
    _host: CFTypeRef,
) -> u8 {
    1
}

pub const NAMES: &[&str] = &[
    "CFPreferencesCopyAppValue",
    "CFPreferencesSetAppValue",
    "CFPreferencesCopyValue",
    "CFPreferencesSetValue",
    "CFPreferencesGetAppBooleanValue",
    "CFPreferencesGetAppIntegerValue",
    "CFPreferencesAppSynchronize",
    "CFPreferencesSynchronize",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "CFPreferencesCopyAppValue" => CFPreferencesCopyAppValue as *const (),
        "CFPreferencesSetAppValue" => CFPreferencesSetAppValue as *const (),
        "CFPreferencesCopyValue" => CFPreferencesCopyValue as *const (),
        "CFPreferencesSetValue" => CFPreferencesSetValue as *const (),
        "CFPreferencesGetAppBooleanValue" => CFPreferencesGetAppBooleanValue as *const (),
        "CFPreferencesGetAppIntegerValue" => CFPreferencesGetAppIntegerValue as *const (),
        "CFPreferencesAppSynchronize" => CFPreferencesAppSynchronize as *const (),
        "CFPreferencesSynchronize" => CFPreferencesSynchronize as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> CFTypeRef {
        crate::string::from_bytes(text.as_bytes(), false)
    }

    #[test]
    fn every_listed_name_resolves() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
        }
    }

    /// A key nothing set is NULL and "does not exist", which is what a program
    /// reads on a Mac it has never run on - and the case that has to work.
    #[test]
    fn an_unset_key_is_absent_rather_than_false() {
        let (key, app) = (string("R3NeverSet"), string("com.example.absent"));
        let mut exists = 1u8;
        // SAFETY: two strings this test made, and a live `Boolean`.
        unsafe {
            assert!(CFPreferencesCopyAppValue(key, app).is_null());
            assert_eq!(
                CFPreferencesGetAppBooleanValue(key, app, &raw mut exists),
                0
            );
            assert_eq!(
                exists, 0,
                "false because it is unset, not because it is off"
            );
        }
    }

    /// What was written is read back, in the domain it was written to and not
    /// in another.
    #[test]
    fn a_value_reads_back_in_its_own_domain() {
        let key = string("R3Enabled");
        let (mine, theirs) = (string("com.example.mine"), string("com.example.theirs"));
        let value = crate::collections::new_number(crate::collections::Number::Integer(1));
        let mut exists = 0u8;
        // SAFETY: strings and a number this test made.
        unsafe {
            CFPreferencesSetAppValue(key, value, mine);
            assert_eq!(
                CFPreferencesGetAppBooleanValue(key, mine, &raw mut exists),
                1
            );
            assert_eq!(exists, 1);
            assert!(CFPreferencesCopyAppValue(key, theirs).is_null());
            assert_eq!(
                CFPreferencesGetAppIntegerValue(key, mine, std::ptr::null_mut()),
                1
            );

            // And a null value removes it again.
            CFPreferencesSetAppValue(key, std::ptr::null(), mine);
            assert!(CFPreferencesCopyAppValue(key, mine).is_null());
            crate::api::CFRelease(value);
        }
    }
}
