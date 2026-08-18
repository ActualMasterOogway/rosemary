//! Symbols the runtime supplies itself, because the host will not hand them out.

use machish::resolve::SyntheticDylib;

/// The guest's stack canary.
static STACK_CHK_GUARD: u64 = 0x00ff_dead_beef_c0de;

/// `int _NSGetExecutablePath(char *buf, uint32_t *bufsize)`.
///
/// # Safety
///
/// Called only through an import slot this module defines, with the two arguments the
/// declaration gives it.
unsafe extern "C" fn ns_get_executable_path(buf: *mut u8, bufsize: *mut u32) -> i32 {
    if bufsize.is_null() {
        return -1;
    }
    let path = std::env::current_exe().unwrap_or_default();
    let path = path.to_string_lossy();
    let needed = path.len() + 1;
    // SAFETY: a guest `uint32_t *`, directly addressable under the identity mapping.
    let available = unsafe { *bufsize } as usize;
    if buf.is_null() || available < needed {
        // SAFETY: as above.
        unsafe { *bufsize = needed as u32 };
        return -1;
    }
    // SAFETY: `buf` has at least `needed` bytes, just checked against the caller's count.
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), buf, path.len());
        *buf.add(path.len()) = 0;
    }
    0
}

/// Symbols the runtime supplies itself, consulted before the host.
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 runtime>");
    // Flat, so it answers a two-level import naming any library: the guest asks
    // libSystem for its canary, and the answer is not libSystem's to give.
    lib.flat(true).define_data(
        "___stack_chk_guard",
        &STACK_CHK_GUARD as *const u64 as usize as u64,
    );
    // Both spellings; which one an image imports depends on its toolchain vintage.
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        lib.define_fn(name, crate::tls::bootstrap as *const ());
    }
    // The host's own `_tlv_atexit` is the right implementation — it appends a
    // destructor to the list dyld runs when this thread ends, and a guest thread
    // is a host thread. It is denied only because the whole `__tlv_` prefix is.
    if let Some(f) = crate::dyld::host_symbol("_tlv_atexit") {
        lib.define_fn("__tlv_atexit", f as *const ());
        lib.define_fn("_tlv_atexit", f as *const ());
    }
    // The host's `open`/`openat` are variadic, and Apple's arm64 ABI puts a
    // variadic argument on the stack — so a fixed signature would leave the mode
    // in a register no `va_arg` reads. These bind to fixed-arity wrappers instead.
    super::openat::provide(&mut lib);
    // Two underscores as the Mach-O spells it: one from C mangling, one from the
    // identifier `_NSGetExecutablePath`.
    lib.define_fn("__NSGetExecutablePath", ns_get_executable_path as *const ());
    // Under high-level emulation the guest's Mach task *is* this process's. The
    // address, not the value.
    if let Some(port) = crate::dyld::host_symbol("mach_task_self_") {
        lib.define_data("_mach_task_self_", port);
    }
    if let Some(f) = crate::dyld::host_symbol("mach_host_self") {
        lib.define_fn("_mach_host_self", f as *const ());
    }
    // `getenv`/`setenv`/`putenv` are passed through, so a guest walking
    // `environ` directly must see the host's block.
    for (mach_o, host) in [
        ("__NSGetEnviron", "_NSGetEnviron"),
        ("__NSGetArgc", "_NSGetArgc"),
        ("__NSGetArgv", "_NSGetArgv"),
    ] {
        if let Some(f) = crate::dyld::host_symbol(host) {
            lib.define_fn(mach_o, f as *const ());
        }
    }
    // `environ` is a variable, so the slot takes its address, which on Darwin is
    // what `_NSGetEnviron()` is for.
    if let Some(f) = crate::dyld::host_symbol("_NSGetEnviron") {
        // SAFETY: `_NSGetEnviron` is `char ***(void)`, a pure accessor for a
        // libSystem global, so its result is valid for the life of the process.
        let get: unsafe extern "C" fn() -> *mut *mut *mut u8 =
            unsafe { core::mem::transmute(f as usize as *const ()) };
        let addr = unsafe { get() };
        if !addr.is_null() {
            lib.define_data("_environ", addr as usize as u64);
        }
    }
    lib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_has_a_zero_byte() {
        assert!(
            STACK_CHK_GUARD.to_le_bytes().contains(&0),
            "a canary without a zero byte can be copied back by a string overflow"
        );
    }

    #[test]
    fn the_executable_path_round_trips_and_reports_the_size_it_needs() {
        let expected = std::env::current_exe().unwrap();
        let expected = expected.to_string_lossy();

        // Too small a buffer must change nothing and report the size required,
        // including the NUL.
        let mut size: u32 = 1;
        // SAFETY: a null buffer with a size the function is being asked to grow.
        let rc = unsafe { ns_get_executable_path(std::ptr::null_mut(), &mut size) };
        assert_eq!(rc, -1);
        assert_eq!(size as usize, expected.len() + 1);

        let mut buf = vec![0xAAu8; size as usize + 8];
        // SAFETY: `buf` is at least `size` bytes, which is what was just asked for.
        let rc = unsafe { ns_get_executable_path(buf.as_mut_ptr(), &mut size) };
        assert_eq!(rc, 0);
        let end = buf.iter().position(|b| *b == 0).expect("NUL-terminated");
        assert_eq!(&buf[..end], expected.as_bytes());
        assert!(buf[end + 1..].iter().all(|b| *b == 0xAA));
    }
}
