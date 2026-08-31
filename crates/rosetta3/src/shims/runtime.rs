//! Symbols the runtime supplies itself, because the host will not hand them out.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

use machish::resolve::SyntheticDylib;

/// The guest's stack canary.
static STACK_CHK_GUARD: u64 = 0x00ff_dead_beef_c0de;

/// The guest's `argc`, as `_NSGetArgc` hands out the address of.
static GUEST_ARGC: AtomicI32 = AtomicI32::new(0);

/// The guest's `argv`, as `_NSGetArgv` hands out the address of.
static GUEST_ARGV: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());

/// Record the arguments the guest was entered with.
///
/// Called once the stack is laid out, which is after the chain that answers these
/// imports was built - so the two accessors below hand out the addresses of these
/// statics and read them when the guest calls, rather than at bind time.
///
/// The guest's arguments, not this process's: `argv[0]` has to be the program the
/// guest thinks it is, not `rosetta3`.
pub fn set_process_args(argc: u64, argv: u64) {
    GUEST_ARGC.store(argc as i32, Ordering::Relaxed);
    GUEST_ARGV.store(argv as usize as *mut *mut c_char, Ordering::Relaxed);
}

/// `int *_NSGetArgc(void)`.
extern "C" fn ns_get_argc() -> *mut i32 {
    GUEST_ARGC.as_ptr()
}

/// `char ***_NSGetArgv(void)`.
extern "C" fn ns_get_argv() -> *mut *mut *mut c_char {
    GUEST_ARGV.as_ptr()
}

/// The guest's own executable, as `_NSGetExecutablePath` must report it.
///
/// Empty until [`set_executable_path`] runs, which is before any guest code.
static GUEST_EXECUTABLE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Record the program the guest is, for the reason [`set_process_args`] records
/// its arguments: the path has to be the one the guest thinks it is running
/// from, not this runtime's.
///
/// A program locates its own resources by walking up from here - Roblox's
/// `Contents/Resources/ssl/cacert.pem` is found this way, and answering with
/// `rosetta3`'s path sent it looking beside the emulator instead, where the
/// missing CA bundle failed every TLS handshake it made.
///
/// Canonical, because a caller joins onto it: `Layout::enclosing` resolves the
/// same symlinks for the same reason.
pub fn set_executable_path(path: &std::path::Path) {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    *GUEST_EXECUTABLE.lock().unwrap_or_else(|e| e.into_inner()) =
        resolved.to_string_lossy().into_owned();
}

/// The address of the host's `environ` variable, or `None` if it has none.
///
/// `getenv`/`setenv`/`putenv` are passed through to the host, so a guest reading
/// `environ` - or calling `_NSGetEnviron` to get at it, which is how Darwin spells
/// taking its address - must be looking at the same block those calls edit.
///
/// Darwin does not export `environ` from a dylib at all, only the accessor; every
/// other host exports the variable and has no accessor. Try both.
fn host_environ() -> Option<u64> {
    if let Some(f) = crate::host::symbol("_NSGetEnviron") {
        // SAFETY: `_NSGetEnviron` is `char ***(void)`, a pure accessor for a
        // libSystem global, so its result is valid for the life of the process.
        let get: unsafe extern "C" fn() -> *mut *mut *mut c_char =
            unsafe { core::mem::transmute(f as usize as *const ()) };
        let addr = unsafe { get() };
        if !addr.is_null() {
            return Some(addr as usize as u64);
        }
    }
    crate::host::symbol("environ")
}

/// Whatever [`host_environ`] found, resolved once at provider time.
static ENVIRON: AtomicPtr<*mut *mut c_char> = AtomicPtr::new(std::ptr::null_mut());

/// A stable address inside *this* binary, for glibc's `dso_symbol` argument.
///
/// It is not read: `__cxa_thread_atexit_impl` looks the address up to find the
/// object that owns the destructor and holds it in memory until the destructor
/// has run. The guest's images are never unloaded, so this runtime's own handle
/// says the true thing - the code will still be there.
static DSO_ANCHOR: u8 = 0;

/// `void _tlv_atexit(void (*dtor)(void *), void *obj)`, for a host without one.
///
/// Darwin registers a thread-local's destructor with dyld, which runs it when
/// the thread ends. glibc's equivalent is `__cxa_thread_atexit_impl`, whose only
/// difference is the third argument - see [`DSO_ANCHOR`].
///
/// A host with neither drops the destructor rather than faulting. That leaks
/// whatever the thread-local owned, which is a worse program than one whose
/// destructors run and a much better one than a null call in a constructor.
///
/// # Safety
///
/// Called through an import slot this module defines, with the two arguments the
/// Darwin declaration gives it. `dtor` is guest code and `obj` is the guest's
/// object; neither is touched here.
unsafe extern "C" fn tlv_atexit(dtor: Option<unsafe extern "C" fn(*mut c_void)>, obj: *mut c_void) {
    /// `__cxa_thread_atexit_impl`, looked up once.
    static HOST: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();

    let Some(dtor) = dtor else { return };
    let Some(f) =
        *HOST.get_or_init(|| crate::host::symbol("__cxa_thread_atexit_impl").map(|f| f as usize))
    else {
        return;
    };
    // SAFETY: glibc's `int __cxa_thread_atexit_impl(void (*)(void *), void *,
    // void *)`, the prototype named here.
    let register: unsafe extern "C" fn(
        unsafe extern "C" fn(*mut c_void),
        *mut c_void,
        *mut c_void,
    ) -> i32 = unsafe { core::mem::transmute(f as *const ()) };
    // SAFETY: `dtor` and `obj` are the caller's own, and the handle is this
    // binary's.
    unsafe {
        register(dtor, obj, &DSO_ANCHOR as *const u8 as *mut c_void);
    }
}

/// `char ***_NSGetEnviron(void)`.
extern "C" fn ns_get_environ() -> *mut *mut *mut c_char {
    ENVIRON.load(Ordering::Relaxed)
}

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
    // The guest's, not this process's. Only a runtime that was handed no path -
    // which is no run that reaches a guest - falls back to its own.
    let path = GUEST_EXECUTABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let path = match path.is_empty() {
        false => path,
        true => std::env::current_exe()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    };
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
    // The host's own `_tlv_atexit` is the right implementation where there is
    // one - it appends a destructor to the list dyld runs when this thread ends,
    // and a guest thread is a host thread. It is denied only because the whole
    // `__tlv_` prefix is. Off Darwin nothing exports it and [`tlv_atexit`] is
    // the stand-in.
    let atexit = crate::host::symbol("_tlv_atexit")
        .map_or(tlv_atexit as *const (), |f| f as usize as *const ());
    lib.define_fn("__tlv_atexit", atexit);
    lib.define_fn("_tlv_atexit", atexit);
    // The host's `open`/`openat` are variadic, and Apple's arm64 ABI puts a
    // variadic argument on the stack - so a fixed signature would leave the mode
    // in a register no `va_arg` reads. These bind to fixed-arity wrappers instead.
    super::openat::provide(&mut lib);
    // Two underscores as the Mach-O spells it: one from C mangling, one from the
    // identifier `_NSGetExecutablePath`.
    lib.define_fn("__NSGetExecutablePath", ns_get_executable_path as *const ());
    // Answered here rather than left to the host because the host answers it
    // wrongly: dyld knows nothing of the images this runtime mapped, so its
    // reply to the vendored `libunwind` is "no unwind info" for every guest
    // frame. Defined in this provider rather than `dyld`'s because that one is
    // absent from the native backend's chain unless the builtin runtime is
    // selected, and the native backend is where a real host unwind happens.
    lib.define_fn(
        "__dyld_find_unwind_sections",
        crate::dyld::find_unwind_sections_natively as *const (),
    );
    // Under high-level emulation the guest's Mach task *is* this process's. The
    // address, not the value.
    if let Some(port) = crate::host::symbol("mach_task_self_") {
        lib.define_data("_mach_task_self_", port);
    }
    if let Some(f) = crate::host::symbol("mach_host_self") {
        lib.define_fn("_mach_host_self", f as *const ());
    }
    // The process arguments. Answered here on every host, not just off Darwin:
    // the host's own `_NSGetArgc`/`_NSGetArgv` describe *this* process, so a
    // guest asking for its arguments would be told it is `rosetta3`.
    lib.define_fn("__NSGetArgc", ns_get_argc as *const ());
    lib.define_fn("__NSGetArgv", ns_get_argv as *const ());
    // `environ` and the accessor for its address. Off Darwin nothing exports
    // `_NSGetEnviron`, and leaving the slot unbound is not a diagnosable
    // failure: `libmimalloc.3.dylib` reads its options out of `environ` from a
    // static initializer, so the program calls null before it has run a line.
    if let Some(addr) = host_environ() {
        ENVIRON.store(addr as usize as *mut *mut *mut c_char, Ordering::Relaxed);
        lib.define_data("_environ", addr);
        lib.define_fn("__NSGetEnviron", ns_get_environ as *const ());
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
