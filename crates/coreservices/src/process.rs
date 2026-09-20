//! Carbon's Process Manager, for the one process there is.
//!
//! A Mac program still reaches for this at startup: it asks for its own serial
//! number, turns itself from a background process into a foreground one so that
//! it can own the menu bar, and brings itself to the front. Under this tree
//! there is exactly one process and it is already the one in front, so the
//! answers are short - but they have to be given, because a program that
//! cannot name itself does not carry on.
//!
//! No other process can be named. `GetNextProcess` and the rest of the walk are
//! deliberately absent: this tree has no view of what else is running, and a
//! list with one entry in it would be a claim that nothing else is.

use std::ffi::{CStr, c_char, c_void};

type OSStatus = i32;
const NO_ERR: OSStatus = 0;
/// `paramErr`.
const PARAM_ERR: OSStatus = -50;
/// `procNotFound`: the serial number names no process this tree knows.
const PROC_NOT_FOUND: OSStatus = -600;

/// `ProcessSerialNumber { UInt32 highLongOfPSN; UInt32 lowLongOfPSN; }`, whose
/// layout is ABI: a guest declares one on its stack.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSerialNumber {
    pub high: u32,
    pub low: u32,
}

/// `kCurrentProcess`, which is `{ 0, 2 }` - and the only process here.
const CURRENT: ProcessSerialNumber = ProcessSerialNumber { high: 0, low: 2 };
/// `kNoProcess`, `{ 0, 0 }`.
const NO_PROCESS: ProcessSerialNumber = ProcessSerialNumber { high: 0, low: 0 };

/// Whether a serial number names this process.
///
/// `kCurrentProcess` does, and so does the number this tree hands out - they
/// are the same number. Anything else names a process that is not here.
///
/// # Safety
///
/// `psn` is null or a readable `ProcessSerialNumber`.
unsafe fn is_this_process(psn: *const ProcessSerialNumber) -> bool {
    if psn.is_null() {
        return false;
    }
    // SAFETY: the caller's obligation.
    let psn = unsafe { *psn };
    psn == CURRENT || psn == NO_PROCESS
}

/// `OSErr GetCurrentProcess(ProcessSerialNumber *psn)`
///
/// # Safety
///
/// `psn` is null or writable for a `ProcessSerialNumber`.
pub unsafe extern "C" fn GetCurrentProcess(psn: *mut ProcessSerialNumber) -> i16 {
    if psn.is_null() {
        return PARAM_ERR as i16;
    }
    // SAFETY: the caller states it is writable.
    unsafe { *psn = CURRENT };
    NO_ERR as i16
}

/// `OSErr GetFrontProcess(ProcessSerialNumber *psn)`
///
/// The one in front is this one: nothing else here has a window.
///
/// # Safety
///
/// As [`GetCurrentProcess`].
pub unsafe extern "C" fn GetFrontProcess(psn: *mut ProcessSerialNumber) -> i16 {
    // SAFETY: the caller's obligation.
    unsafe { GetCurrentProcess(psn) }
}

/// `OSErr SameProcess(const ProcessSerialNumber *, const ProcessSerialNumber *,
/// Boolean *result)`
///
/// # Safety
///
/// Both serial numbers are readable and `result` is writable.
pub unsafe extern "C" fn SameProcess(
    a: *const ProcessSerialNumber,
    b: *const ProcessSerialNumber,
    result: *mut u8,
) -> i16 {
    if a.is_null() || b.is_null() || result.is_null() {
        return PARAM_ERR as i16;
    }
    // SAFETY: the caller's obligation.
    let same = unsafe { *a == *b };
    // SAFETY: the caller states `result` is writable.
    unsafe { *result = u8::from(same) };
    NO_ERR as i16
}

/// `OSErr SetFrontProcess(const ProcessSerialNumber *psn)`
///
/// # Safety
///
/// `psn` is null or a readable `ProcessSerialNumber`.
pub unsafe extern "C" fn SetFrontProcess(psn: *const ProcessSerialNumber) -> i16 {
    // SAFETY: the caller's obligation.
    match unsafe { is_this_process(psn) } {
        true => NO_ERR as i16,
        false => PROC_NOT_FOUND as i16,
    }
}

/// `OSStatus TransformProcessType(const ProcessSerialNumber *,
/// ProcessApplicationTransformState)`
///
/// A program launched from a terminal rather than from a bundle is a background
/// process on the Mac, and this is how it promotes itself so that it can put up
/// a window and own the menu bar. Here a window is SDL's and is made whatever
/// the program's type is, so the promotion has nothing left to do - which is
/// why it succeeds rather than being refused.
///
/// # Safety
///
/// As [`SetFrontProcess`].
pub unsafe extern "C" fn TransformProcessType(
    psn: *const ProcessSerialNumber,
    _kind: u32,
) -> OSStatus {
    // SAFETY: the caller's obligation.
    match unsafe { is_this_process(psn) } {
        true => NO_ERR,
        false => PROC_NOT_FOUND,
    }
}

/// `OSStatus GetProcessBundleLocation(const ProcessSerialNumber *, FSRef *)`
///
/// # Safety
///
/// `psn` is readable, and `location` is null or storage for a whole `FSRef`.
pub unsafe extern "C" fn GetProcessBundleLocation(
    psn: *const ProcessSerialNumber,
    location: *mut c_void,
) -> OSStatus {
    if location.is_null() {
        return PARAM_ERR;
    }
    // SAFETY: the caller's obligation.
    if !unsafe { is_this_process(psn) } {
        return PROC_NOT_FOUND;
    }
    let Some(root) = foundation::bundle::main().map(|layout| layout.root.clone()) else {
        // A program outside a bundle has no bundle location, which is what
        // Apple answers for one too.
        return PROC_NOT_FOUND;
    };
    // SAFETY: the caller states `location` holds a whole `FSRef`.
    unsafe { crate::files::store_path(location, root) };
    NO_ERR
}

/// `const char *GetMacOSStatusCommentString(OSStatus)`
///
/// Apple's hands back a pointer into a static table. There is no table here, so
/// the number is written into a buffer of this module's own and that is handed
/// back - which is the same contract: a pointer the caller reads and does not
/// free.
///
/// The *comment* is the long form; [`GetMacOSStatusErrorString`] is the short
/// one, and neither has a name for a code this tree does not know. Saying the
/// number is better than naming the wrong error - the trap `darwin::errno`
/// describes, one table over.
pub unsafe extern "C" fn GetMacOSStatusCommentString(status: OSStatus) -> *const c_char {
    described(status)
}

/// `const char *GetMacOSStatusErrorString(OSStatus)`
pub unsafe extern "C" fn GetMacOSStatusErrorString(status: OSStatus) -> *const c_char {
    described(status)
}

/// The text for `status`, in storage that outlives the call.
///
/// One slot per thread, because the string is read after the call returns and
/// two threads asking at once must not be handed the same buffer.
fn described(status: OSStatus) -> *const c_char {
    const ROOM: usize = 32;
    thread_local! {
        static TEXT: std::cell::RefCell<[u8; ROOM]> = const {
            std::cell::RefCell::new([0; ROOM])
        };
    }
    TEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let text = format!("OSStatus {status}\0");
        let bytes = text.as_bytes();
        let used = bytes.len().min(ROOM - 1);
        slot[..used].copy_from_slice(&bytes[..used]);
        slot[used] = 0;
        // SAFETY: the bytes above are NUL-terminated and the storage is this
        // thread's for as long as the thread lives - which is the same promise
        // Apple's static table makes.
        unsafe { CStr::from_ptr(slot.as_ptr().cast()) }.as_ptr()
    })
}

exports::exports! {
    functions {
        "GetCurrentProcess" => GetCurrentProcess,
        "GetFrontProcess" => GetFrontProcess,
        "SameProcess" => SameProcess,
        "SetFrontProcess" => SetFrontProcess,
        "TransformProcessType" => TransformProcessType,
        "GetProcessBundleLocation" => GetProcessBundleLocation,
        "GetMacOSStatusCommentString" => GetMacOSStatusCommentString,
        "GetMacOSStatusErrorString" => GetMacOSStatusErrorString,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number a program gets back is `kCurrentProcess`, and it compares
    /// equal to itself - which is the whole of what a caller does with it.
    #[test]
    fn this_process_names_itself() {
        let mut psn = NO_PROCESS;
        // SAFETY: writable storage for a serial number.
        assert_eq!(unsafe { GetCurrentProcess(&mut psn) }, NO_ERR as i16);
        assert_eq!(psn, CURRENT);
        let mut same = 0_u8;
        // SAFETY: two readable serial numbers and a writable flag.
        assert_eq!(
            unsafe { SameProcess(&psn, &CURRENT, &mut same) },
            NO_ERR as i16
        );
        assert_eq!(same, 1);
        // SAFETY: a readable serial number.
        assert_eq!(unsafe { SetFrontProcess(&psn) }, NO_ERR as i16);
        // SAFETY: as above.
        assert_eq!(unsafe { TransformProcessType(&psn, 1) }, NO_ERR);
    }

    /// Another process is not found rather than silently treated as this one.
    #[test]
    fn another_process_is_not_found() {
        let other = ProcessSerialNumber { high: 0, low: 99 };
        // SAFETY: a readable serial number.
        assert_eq!(unsafe { SetFrontProcess(&other) }, PROC_NOT_FOUND as i16);
        // SAFETY: as above.
        assert_eq!(unsafe { TransformProcessType(&other, 1) }, PROC_NOT_FOUND);
    }

    /// A status with no name is reported as its number rather than as a
    /// neighbouring error's message.
    #[test]
    fn an_unknown_status_is_reported_as_its_number() {
        // SAFETY: the pointer is this thread's own storage, read before the
        // next call on this thread replaces it.
        let text = unsafe { CStr::from_ptr(GetMacOSStatusCommentString(-43)) };
        assert_eq!(text.to_bytes(), b"OSStatus -43");
    }
}
