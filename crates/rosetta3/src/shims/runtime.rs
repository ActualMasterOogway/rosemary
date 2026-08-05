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
    // The task port, for the same reason and with the same answer. `machish`
    // denies it as host process state, which is right for a runtime that models
    // a separate task and wrong for this one: under high-level emulation the
    // guest's Mach task *is* this process's, because the guest's threads are
    // this process's threads and its memory is this process's memory. A guest
    // asking `mach_task_self()` while its slot is null asks the kernel about
    // port 0 and gets `KERN_INVALID_ARGUMENT` from every call that takes one.
    //
    // The address, not the value: the symbol names a `mach_port_t` variable and
    // `mach_task_self()` is a macro that reads it. Handing over the host's own
    // variable keeps the two spellings agreeing for the life of the process.
    if let Some(port) = crate::dyld::host_symbol("mach_task_self_") {
        lib.define_data("_mach_task_self_", port);
    }
    // `mach_host_self` is denied for the same reason and answered the same way,
    // as a *function* rather than a variable: it returns a send right to the
    // host port, and there is one host. Steam's tier0 asks it for the machine's
    // physical memory size, through `host_statistics`.
    if let Some(f) = crate::dyld::host_symbol("mach_host_self") {
        lib.define_fn("_mach_host_self", f as *const ());
    }
    // The rest of the process-state family, answered with the host's own.
    //
    // `machish` denies these so that a shim must decide, and here the decision
    // is forced by something already true: `getenv`, `setenv` and `putenv` are
    // passed through, so the environment the guest reads is the host's. A guest
    // walking `environ` directly must see that same block or the two spellings
    // disagree — and it is the right one anyway, since the variables a guest
    // cares about (`SteamAppId`, `HOME`) were set for this process.
    //
    // Left null instead, each of these is a load from a slot the guest
    // dereferences immediately, which is a fault with nothing naming a cause.
    //
    // `__NSGetArgv` is the one that is an approximation rather than an answer:
    // it reports this process's command line, `run <path>` and all, where the
    // guest's own vector is on its stack. That gap is not new — Foundation's
    // `[[NSProcessInfo processInfo] arguments]` already reports the same thing,
    // for the same reason — and it is tracked with the bundle work rather than
    // half-closed here.
    for (mach_o, host) in [
        ("__NSGetEnviron", "_NSGetEnviron"),
        ("__NSGetArgc", "_NSGetArgc"),
        ("__NSGetArgv", "_NSGetArgv"),
        // The host's image list, which is honestly what a crash reporter
        // linked into the guest is asking about: this process's images are
        // where its own frames are. Guest images are not in it, which is a
        // partial answer and a great deal better than a null one.
        ("__dyld_get_image_name", "_dyld_get_image_name"),
    ] {
        if let Some(f) = crate::dyld::host_symbol(host) {
            lib.define_fn(mach_o, f as *const ());
        }
    }
    // `environ` itself is a *variable*, so the slot takes its address rather
    // than a function's. `_NSGetEnviron()` is how a dylib asks for that address
    // on Darwin, which is the same reason the guest cannot simply be handed
    // `dlsym("environ")`.
    if let Some(f) = crate::dyld::host_symbol("_NSGetEnviron") {
        // SAFETY: `_NSGetEnviron` is `char ***(void)` and takes no arguments.
        // It is a pure accessor for the address of a libSystem global, so the
        // pointer it returns is valid for the life of the process.
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
