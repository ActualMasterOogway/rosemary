//! `locale_t`, asked the one question this runtime asks of it.
//!
//! Darwin has `querylocale`, a BSD extension that names one category of a `locale_t`.
//! glibc and musl do not; the portable spelling is `nl_langinfo_l`, which answers a
//! different question about the same object, and neither can be asked "what is this
//! locale called".
//!
//! Which is fine, because the only thing that needs asking is whether a `locale_t`
//! formats numbers the way `rosetta3::shims::printf` renders them - and that has a
//! portable answer.

/// Whether `loc` formats numbers the way the C locale does.
///
/// A null `loc` means **the caller's current locale**, which is what Darwin's `*_l`
/// functions take it to mean and therefore what a guest passing null is asking for.
/// Spelled explicitly on both hosts, because the portable spelling below would
/// dereference it.
///
/// `None` for a handle nothing recognises, which the caller must not treat as a yes.
///
/// # Safety
///
/// `loc` is a `locale_t` this process obtained, or null.
pub unsafe fn is_c_numeric(loc: *mut core::ffi::c_void) -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller guarantees a `locale_t` or null, both of which
        // `querylocale` accepts. It returns a NUL-terminated name owned by libc, or
        // null for a handle it does not know.
        let name = unsafe { libc::querylocale(libc::LC_NUMERIC_MASK, loc as _) };
        if name.is_null() {
            return None;
        }
        // SAFETY: non-null, and NUL-terminated by contract.
        let name = unsafe { core::ffi::CStr::from_ptr(name) }.to_str().ok()?;
        Some(matches!(name, "C" | "POSIX"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // No `querylocale` here, so the question is asked of the locale's own decimal
        // point instead. That *is* what "formats numbers the C way" means to the
        // renderer, and it is the property the caller depends on: a locale whose
        // decimal point is `.` renders identically to C, whatever it is called.
        let point = match loc.is_null() {
            // SAFETY: the non-`_l` form reads the caller's current locale and takes
            // no handle, which is what a null `loc` asks for.
            true => unsafe { libc::nl_langinfo(libc::RADIXCHAR) },
            // SAFETY: the caller guarantees a live `locale_t`. The result is a
            // NUL-terminated string owned by the locale, valid until it is freed.
            false => unsafe { libc::nl_langinfo_l(libc::RADIXCHAR, loc as libc::locale_t) },
        };
        if point.is_null() {
            return None;
        }
        // SAFETY: non-null, and NUL-terminated by contract.
        Some(unsafe { core::ffi::CStr::from_ptr(point) }.to_bytes() == b".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C locale must answer yes, on either host and by either mechanism.
    #[test]
    fn the_c_locale_formats_numbers_the_c_way() {
        let name = std::ffi::CString::new("C").unwrap();
        // SAFETY: a NUL-terminated name; `newlocale` returns null on failure, checked.
        let loc = unsafe { libc::newlocale(all_mask(), name.as_ptr(), core::ptr::null_mut()) };
        assert!(!loc.is_null(), "the C locale exists everywhere");
        // SAFETY: `loc` came from `newlocale` and has not been freed.
        assert_eq!(unsafe { is_c_numeric(loc as *mut _) }, Some(true));
        // SAFETY: as above; nothing uses `loc` after this.
        unsafe { libc::freelocale(loc) };
    }

    /// A guest passing null is asking about its current locale, which this runtime
    /// never changes - so it must get the same answer as the C locale itself, and
    /// get it on both hosts.
    /// `LC_ALL_MASK`, which the `libc` crate does not export for every host.
    ///
    /// The same number everywhere it is defined - every category bit set - so writing
    /// it out is stating the value rather than choosing one.
    fn all_mask() -> libc::c_int {
        #[cfg(target_os = "macos")]
        {
            libc::LC_ALL_MASK
        }
        #[cfg(not(target_os = "macos"))]
        {
            0x7fff_ffff
        }
    }

    #[test]
    fn a_null_handle_means_the_current_locale() {
        // SAFETY: null is one of the two values this accepts.
        assert_eq!(unsafe { is_c_numeric(core::ptr::null_mut()) }, Some(true));
    }
}
