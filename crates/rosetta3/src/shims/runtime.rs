//! Symbols the runtime supplies itself, because the host will not hand them out.
//!
//! `machish`'s native provider passes imports through to the host's own
//! libSystem by `dlsym`, and a handful of symbols it deliberately declines.
//! `___stack_chk_guard` is the one every real program needs: it is the stack
//! protector's canary, and the compiler emits a load of it in the prologue of
//! any function with an array on its stack.
//!
//! It is declined for a good reason — `dlsym` does not export it, and the
//! host's own canary is not something to alias — and the consequence is precise
//! and fatal: the guest reads a null slot, dereferences it, and faults in its
//! own prologue before executing a line of its own code. Milestone 1's report
//! called this out as "1 import unresolved (need shims)". This is that shim.
//!
//! ## What the value has to be
//!
//! Any 64-bit value, as long as it is the *same* one for the whole run: the
//! prologue stores it and the epilogue compares. Its security purpose — being
//! unpredictable to an attacker — is not something a translation layer can
//! provide anyway, since the guest and the runtime share an address space, and
//! pretending otherwise would be theatre. What it must do is exist, be stable,
//! and have a zero byte in it, which is what makes a string overflow unable to
//! copy it back intact.
//!
//! ## `__tlv_bootstrap`
//!
//! The other symbol the host declines, for a better reason than `dlsym`
//! visibility: the host's copy allocates out of the *host's* image state, which
//! knows nothing about a guest descriptor. It is supplied here and implemented
//! in [`crate::tls`].

use machish::resolve::SyntheticDylib;

/// The guest's stack canary, in this process's memory.
///
/// One `static`, so its address is stable for the process lifetime and every
/// image that imports the symbol gets the same slot value — which is what the
/// prologue/epilogue pair requires.
///
/// The low byte is zero for the reason C libraries make it zero: it terminates
/// a `strcpy` that would otherwise restore the canary while overwriting the
/// return address.
static STACK_CHK_GUARD: u64 = 0x00ff_dead_beef_c0de;

/// `int _NSGetExecutablePath(char *buf, uint32_t *bufsize)`.
///
/// Denied to the host by [`machish`] for a good reason — it is host process
/// state, and the host process is `rosetta3` — but denied *and unanswered* it
/// resolves to null, and a guest that calls it stops with "unsatisfied import"
/// having asked a perfectly reasonable question.
///
/// The answer is available and it is the host's own path, because of
/// [`crate::bundle`]: an application is re-executed from a mirror of its bundle,
/// so this process really is running at
/// `…/UNDERTALE.app/Contents/MacOS/Mac_Runner`. That is also the path the guest
/// wants — it is what `mainBundle` resolves from and where its `Resources` are
/// symlinked — so forwarding is not an approximation here, it is the right
/// answer for the same reason the re-exec exists.
///
/// Written natively rather than through the marshaller because both arguments
/// are pointers into guest memory and the guest is identity-mapped: the host's
/// own `libc` writes into guest buffers on every forwarded call, and this is
/// that, with the string chosen here.
///
/// # Safety
///
/// Called only through an import slot this module defines, with the two
/// arguments the declaration gives it.
unsafe extern "C" fn ns_get_executable_path(buf: *mut u8, bufsize: *mut u32) -> i32 {
    if bufsize.is_null() {
        return -1;
    }
    let path = std::env::current_exe().unwrap_or_default();
    let path = path.to_string_lossy();
    // The trailing NUL is part of what the caller must have room for, which is
    // the detail the real implementation is precise about and a shim gets wrong
    // by one byte.
    let needed = path.len() + 1;
    // SAFETY: `bufsize` is a guest `uint32_t *`, which the identity mapping
    // makes directly addressable.
    let available = unsafe { *bufsize } as usize;
    if buf.is_null() || available < needed {
        // The documented failure: report the size required and change nothing.
        // SAFETY: as above.
        unsafe { *bufsize = needed as u32 };
        return -1;
    }
    // SAFETY: `buf` has at least `needed` bytes, just checked against the
    // caller's own count.
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), buf, path.len());
        *buf.add(path.len()) = 0;
    }
    0
}

/// C++'s global allocation operators, on `malloc` and `free`.
///
/// `libYoYoGamepad.dylib` is what needed them: DELTARUNE `dlopen`s it, it
/// imports `operator new` two-level from `/usr/lib/libc++.1.dylib`, and that
/// binding resolves to null. **Why it does is not explained** — the native
/// provider is flat, the name matches no deny rule, and `dlsym` finds `_Znwm`
/// in this process — so this is not a diagnosis, it is independence from one.
///
/// Supplying them is safe in a way most C++ interop is not, and the reason is
/// worth stating rather than assuming. These are `extern "C"` in everything but
/// name: the mangling encodes the signature and there is no `this`, no vtable,
/// no exception object, no class layout. `operator new` is `void *(size_t)` in
/// the plain SysV sense, so the marshaller already describes it exactly.
///
/// Mixing with the host's copies is also safe, and that is the part worth
/// checking rather than hoping: the host's `operator new` is itself a `malloc`
/// wrapper, so memory this allocates can be released by the host's
/// `operator delete` and vice versa. What must never happen is `malloc`/`free`
/// paired against a *different* allocator, and there is only one here.
///
/// The one honest gap is failure. `operator new` is specified to throw
/// `std::bad_alloc`, and throwing a C++ exception from Rust across the JIT
/// boundary is not something this runtime can do. It aborts with a message
/// instead, which is what a guest with no handler would have done anyway and is
/// at least attributable.
mod cxx {
    /// `operator new(size_t)` and `operator new[](size_t)`.
    ///
    /// # Safety
    ///
    /// Called only through an import slot this module defines.
    pub unsafe extern "C" fn new(size: usize) -> *mut u8 {
        // Zero is a legal request and must return a distinct, freeable pointer
        // rather than null, which would look like the failure below.
        let size = size.max(1);
        // SAFETY: `size` is non-zero; the result is checked.
        let p = unsafe { libc::malloc(size) } as *mut u8;
        if p.is_null() {
            eprintln!(
                "rosetta3: the guest's operator new({size}) failed, and this runtime \
                       cannot throw std::bad_alloc across the JIT boundary"
            );
            std::process::abort();
        }
        p
    }

    /// `operator delete(void *)` and its array and sized spellings.
    ///
    /// # Safety
    ///
    /// As above. A null pointer is explicitly a no-op, which the standard
    /// requires and `free` already honours.
    pub unsafe extern "C" fn delete(p: *mut u8) {
        // SAFETY: `p` came from `new` above or from the host's `operator new`,
        // which is also a `malloc` wrapper. See the module note.
        unsafe { libc::free(p as *mut core::ffi::c_void) }
    }

    /// The sized deletes, `operator delete(void *, size_t)`.
    ///
    /// The size is what the compiler knew at the call site and is an
    /// optimisation hint for a size-classed allocator; `free` does not want it.
    ///
    /// # Safety
    ///
    /// As above.
    pub unsafe extern "C" fn delete_sized(p: *mut u8, _size: usize) {
        // SAFETY: as `delete`.
        unsafe { libc::free(p as *mut core::ffi::c_void) }
    }
}

/// Symbols the runtime supplies itself, consulted before the host.
///
/// Flat, so it answers a two-level import naming any library: the guest asks
/// `libSystem` for its canary, and the answer is not libSystem's to give.
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 runtime>");
    lib.flat(true).define_data(
        "___stack_chk_guard",
        &STACK_CHK_GUARD as *const u64 as usize as u64,
    );
    // Both spellings, because which one an image imports depends on its
    // toolchain's vintage and `machish`'s TLV extension redirects whichever it
    // finds to a flat lookup — which is this one. See [`crate::tls`] for why the
    // implementation needs no VM.
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        lib.define_fn(name, crate::tls::bootstrap as *const ());
    }
    // Two underscores as the Mach-O spells it: one from C mangling, one from
    // the identifier `_NSGetExecutablePath`.
    lib.define_fn("__NSGetExecutablePath", ns_get_executable_path as *const ());
    // The global allocation operators. Two underscores as the Mach-O spells
    // them: one from C mangling, one already in the Itanium mangled name.
    for name in ["__Znwm", "__Znam"] {
        lib.define_fn(name, cxx::new as *const ());
    }
    for name in ["__ZdlPv", "__ZdaPv"] {
        lib.define_fn(name, cxx::delete as *const ());
    }
    for name in ["__ZdlPvm", "__ZdaPvm"] {
        lib.define_fn(name, cxx::delete_sized as *const ());
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

        // The documented failure first: too small a buffer must change nothing
        // and report the size required, *including* the NUL. A shim off by one
        // here hands back a path with its last character missing.
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
        // Nothing written past the terminator the caller was promised.
        assert!(buf[end + 1..].iter().all(|b| *b == 0xAA));
    }
}
