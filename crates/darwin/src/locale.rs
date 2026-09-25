/// Whether `loc` formats numbers the way the C locale does.
///
/// A null `loc` means Darwin's C-locale sentinel for the `*_l` convenience
/// functions. Spelled explicitly because glibc's `_l` functions dereference a
/// null locale object.
///
/// `None` for a handle nothing recognises, which the caller must not treat as a yes.
///
/// # Safety
///
/// `loc` is a `locale_t` this process obtained, or null.
pub unsafe fn is_c_numeric(loc: *mut core::ffi::c_void) -> Option<bool> {
    if loc.is_null() {
        return Some(true);
    }
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
        let loc = loc as libc::locale_t;
        let _global = if loc == crate::xlocale::global_locale() {
            // glibc forbids passing this distinguished handle to `_l`
            // functions. Temporarily select it and use the plain query.
            crate::xlocale::switch(loc)
        } else {
            None
        };
        // SAFETY: an ordinary locale is accepted by the `_l` query. For the
        // global sentinel, the guard selected it as the thread locale first.
        let point = unsafe {
            if loc == crate::xlocale::global_locale() {
                libc::nl_langinfo(libc::RADIXCHAR)
            } else {
                libc::nl_langinfo_l(libc::RADIXCHAR, loc)
            }
        };
        if point.is_null() {
            return None;
        }
        // SAFETY: non-null, and NUL-terminated by contract.
        Some(unsafe { core::ffi::CStr::from_ptr(point) }.to_bytes() == b".")
    }
}

/// The mask selecting every locale category on this host's libc.
pub(crate) fn all_mask() -> libc::c_int {
    #[cfg(target_os = "macos")]
    {
        libc::LC_ALL_MASK
    }
    #[cfg(all(not(target_os = "macos"), target_env = "gnu"))]
    {
        0x1fbf
    }
    #[cfg(all(not(target_os = "macos"), not(target_env = "gnu")))]
    {
        0x7fff_ffff
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

    /// A null is Darwin's explicit C-locale sentinel for convenience functions.
    /// `LC_ALL_MASK`, which the `libc` crate does not export for every host.
    ///
    /// The same number everywhere it is defined - every category bit set - so writing
    /// it out is stating the value rather than choosing one.
    #[test]
    fn a_null_handle_means_the_c_locale() {
        // SAFETY: null is one of the two values this accepts.
        assert_eq!(unsafe { is_c_numeric(core::ptr::null_mut()) }, Some(true));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_global_locale_sentinel_is_not_passed_to_an_l_function() {
        // SAFETY: LC_GLOBAL_LOCALE is a valid explicit locale selection.
        assert!(unsafe { is_c_numeric(crate::xlocale::global_locale().cast()) }.is_some());
    }
}
