use std::ffi::{CStr, CString, c_char};
use std::sync::atomic::{AtomicPtr, Ordering};

/// Darwin's `__progname`: the basename of `argv[0]`.
static PROGNAME: AtomicPtr<c_char> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(target_os = "linux")]
unsafe extern "C" {
    static mut program_invocation_name: *mut c_char;
    static mut program_invocation_short_name: *mut c_char;
}

/// The part after the last slash, as Darwin's start-up code takes it.
fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(slash) => &path[slash + 1..],
        None => path,
    }
}

/// Name the process after the guest's `argv[0]`.
pub fn set(argv0: &CStr) {
    // Leaked: a name handed out by `getprogname` is never freed, and glibc
    // holds on to the same pointers.
    let full: &'static CStr = Box::leak(argv0.to_owned().into_boxed_c_str());
    let short = CString::new(basename(full.to_bytes())).unwrap_or_default();
    let short: &'static CStr = Box::leak(short.into_boxed_c_str());
    PROGNAME.store(short.as_ptr().cast_mut(), Ordering::Release);
    #[cfg(target_os = "linux")]
    // SAFETY: glibc's two documented name variables, pointed at strings that
    // live for the rest of the process.
    unsafe {
        program_invocation_name = full.as_ptr().cast_mut();
        program_invocation_short_name = short.as_ptr().cast_mut();
    }
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    // SAFETY: a string that lives for the rest of the process.
    unsafe {
        libc::setprogname(full.as_ptr())
    };
}

/// `const char *getprogname(void)`
pub extern "C" fn getprogname() -> *const c_char {
    let name = PROGNAME.load(Ordering::Acquire);
    match name.is_null() {
        true => c"".as_ptr(),
        false => name,
    }
}

/// `void setprogname(const char *progname)`
///
/// Darwin keeps a pointer into the caller's string rather than a copy, and so
/// does this.
///
/// # Safety
///
/// `progname` is NUL-terminated and outlives every later `getprogname`.
pub unsafe extern "C" fn setprogname(progname: *const c_char) {
    if progname.is_null() {
        return;
    }
    let bytes = unsafe { CStr::from_ptr(progname) }.to_bytes();
    let offset = bytes.len() - basename(bytes).len();
    // SAFETY: an offset inside the caller's string.
    PROGNAME.store(
        unsafe { progname.add(offset) }.cast_mut(),
        Ordering::Release,
    );
}

exports::exports! {
    functions {
        "getprogname" => getprogname,
        "setprogname" => setprogname,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_is_the_basename_of_what_it_was_given() {
        assert_eq!(basename(b"/bin/ls"), b"ls");
        assert_eq!(basename(b"ash"), b"ash");
        assert_eq!(basename(b"dir/"), b"");
        let name = c"/usr/local/bin/tool";
        // SAFETY: a literal that outlives the test.
        unsafe { setprogname(name.as_ptr()) };
        // SAFETY: `getprogname` returns a NUL-terminated string.
        assert_eq!(unsafe { CStr::from_ptr(getprogname()) }, c"tool");
    }
}
