//! `sysctl`, as the BSDs ask most of what Linux keeps in `/proc`.

use std::ffi::CStr;
use std::io;

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// A MIB's whole answer, however long it is.
pub fn bytes(mib: &[libc::c_int]) -> io::Result<Vec<u8>> {
    let mut mib = mib.to_vec();
    let mut needed = 0usize;
    // SAFETY: the MIB and output length are valid; a null output asks for size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as _,
            std::ptr::null_mut(),
            &raw mut needed,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(last_error());
    }
    // A table - descriptors, threads, the command line - may grow between the
    // two calls.
    for _ in 0..4 {
        let mut bytes = vec![0u8; needed.saturating_add(4096)];
        let mut length = bytes.len();
        // SAFETY: `bytes` is writable for `length` bytes, and the MIB is valid.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as _,
                bytes.as_mut_ptr().cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            bytes.truncate(length);
            return Ok(bytes);
        }
        let error = last_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error);
        }
        needed = length.max(bytes.len().saturating_mul(2));
    }
    Err(io::Error::from_raw_os_error(libc::ENOMEM))
}

/// A named key's answer as a plain value.
///
/// `T` is plain data - an integer, an array of them, a `timeval` - valid when
/// zeroed.
pub fn by_name<T: Copy>(name: &CStr) -> Option<T> {
    // SAFETY: as above.
    let mut value: T = unsafe { std::mem::zeroed() };
    let mut length = size_of::<T>();
    // SAFETY: `value` is writable for `length` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && length == size_of::<T>()).then_some(value)
}

/// A named key's answer as a string.
pub fn string_by_name(name: &CStr) -> Option<String> {
    let mut buffer = [0u8; 256];
    let mut length = buffer.len();
    // SAFETY: `buffer` is writable for `length` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&buffer[..length]).ok()?;
    Some(text.to_string_lossy().trim().to_owned())
}
