//! Entries a Mac's libSystem publishes and this host's libc does not export.
//!
//! Grouped because the reason is the same for all of them, and because it is not
//! the usual one: these are not Darwin *behaviour* differences, they are names a
//! `dlsym` here cannot find. Three ways that happens:
//!
//! - the symbol exists but only for a static link - `atexit` lives in glibc's
//!   `libc_nonshared.a`, so nothing dynamic defines it;
//! - the symbol is a BSD extension glibc never had - `srandomdev`;
//! - the symbol is optional C - `memset_s` is C11 Annex K, which glibc declined.
//!
//! `__assert_rtn` is the fourth kind: the same function under another name.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int, c_void};

use machish::resolve::SyntheticDylib;

unsafe extern "C" {
    /// glibc's, which *is* exported, and which `atexit` is a special case of.
    fn __cxa_atexit(func: *const (), arg: *mut c_void, dso: *const ()) -> c_int;
    #[cfg(target_env = "gnu")]
    /// glibc's exported `pthread_atfork`, which takes the DSO handle as well.
    fn __register_atfork(
        prepare: *const (),
        parent: *const (),
        child: *const (),
        dso: *const (),
    ) -> c_int;
}

/// The DSO handle this crate's registrations are made against.
///
/// Its own object rather than the guest's: a guest handle would ask the host's
/// exit machinery to key destructors on an image it never loaded.
static DSO_HANDLE: u8 = 0;

/// `int atexit(void (*handler)(void))`
///
/// Registered through `__cxa_atexit` with a null argument. A function compiled
/// to take none ignores the register it arrives in, under both ABIs in play.
///
/// The emulated backend does **not** use this: there `handler` is a guest
/// address that has to become a trampoline first, which is
/// `rosetta3::shims::atexit`'s job and which it still wins because interception
/// reads the link report rather than the slot. The slot has to hold something
/// for the native backend, where the guest's code already is host code.
///
/// # Safety
///
/// `handler` is a function this process can call and that outlives the process's
/// exit.
pub unsafe extern "C" fn atexit(handler: *const ()) -> c_int {
    // SAFETY: the caller's guarantee, and a DSO handle of this crate's own.
    unsafe {
        __cxa_atexit(
            handler,
            core::ptr::null_mut(),
            &raw const DSO_HANDLE as *const (),
        )
    }
}

/// `BLOCK_IS_GLOBAL` in a block literal's flags.
///
/// A global block's storage is its image's `__DATA`, so it outlives the process's
/// exit without being copied. That is what makes [`atexit_b`] work even where no
/// block runtime is registered, and the case in hand: a `^{ }` with nothing
/// captured is always global.
const BLOCK_IS_GLOBAL: u32 = 1 << 28;

/// Byte offset of `flags` in a block literal, after `isa`.
const BLOCK_FLAGS_OFFSET: u64 = 8;

/// Run a block registered by [`atexit_b`], and give back the reference taken for
/// it.
///
/// # Safety
///
/// Called by the host's exit machinery with the block `atexit_b` registered.
unsafe extern "C" fn run_exit_block(block: *mut c_void) {
    let block = block as u64;
    // SAFETY: a block this process copied or a global one, either way still live:
    // nothing releases it before exit.
    unsafe { crate::dispatch::block::invoke(block) };
    crate::dispatch::block::release(block);
}

/// `int atexit_b(void (^block)(void))`
///
/// The block form of [`atexit`], and a real entry point rather than a courtesy:
/// Qt's Cocoa plugin registers its application delegate's teardown this way, and
/// the call is a *tail* jump - so an unbound slot is a jump to address zero whose
/// backtrace does not even name the block it came from.
///
/// The block is copied, because a caller may pass a stack block and this one
/// outlives every frame. Where no block runtime is registered - see
/// `darwin::dispatch::block::set_runtime` - a **global** block is still accepted
/// as it stands, since its storage is its image's; anything else is refused by
/// returning non-zero, which is what a caller checks and is the truth: the
/// handler was not registered.
///
/// # Safety
///
/// `block` is null or a live block literal taking no arguments.
pub unsafe extern "C" fn atexit_b(block: *mut c_void) -> c_int {
    if block.is_null() {
        return -1;
    }
    let kept = match crate::dispatch::block::copy(block as u64) {
        Some(copied) => copied,
        None => {
            // SAFETY: the caller states this is a block literal, whose `flags`
            // are at a fixed offset by the Block ABI.
            let flags = unsafe { ((block as u64 + BLOCK_FLAGS_OFFSET) as *const u32).read() };
            if flags & BLOCK_IS_GLOBAL == 0 {
                return -1;
            }
            block as u64
        }
    };
    // SAFETY: a handler of this crate's own, the kept block as its argument, and
    // a DSO handle of this crate's own.
    unsafe {
        __cxa_atexit(
            run_exit_block as *const (),
            kept as *mut c_void,
            &raw const DSO_HANDLE as *const (),
        )
    }
}

/// `int pthread_atfork(void (*prepare)(void), void (*parent)(void), void (*child)(void))`
///
/// Here for exactly `atexit`'s reason: glibc keeps `pthread_atfork` in
/// `libc_nonshared.a` too, so `crate::pthread`'s passthrough for it found
/// nothing and the slot stayed unbound. `__register_atfork` is the exported
/// form, and takes the DSO handle the wrapper would have supplied.
///
/// As with `atexit`, the emulated backend does not use this - the three
/// handlers are guest addresses that have to become trampolines first, which is
/// why the name is on `crate::pthread::SHIMMED_ABOVE`.
///
/// # Safety
///
/// Each handler is null or a function this process can call.
pub unsafe extern "C" fn pthread_atfork(
    prepare: *const (),
    parent: *const (),
    child: *const (),
) -> c_int {
    // FIXME (ACCURACY): fork handlers are completely dropped on non-GNU systems.
    #[cfg(target_env = "gnu")]
    {
        // SAFETY: the caller's guarantee, and a DSO handle of this crate's own.
        unsafe { __register_atfork(prepare, parent, child, &raw const DSO_HANDLE as *const ()) }
    }
    #[cfg(not(target_env = "gnu"))]
    {
        return 0;
    }
}

/// `void __assert_rtn(const char *func, const char *file, int line, const char *expr)`
///
/// Darwin's spelling of what glibc calls `__assert_fail`, with the arguments in
/// a different order and the function name first rather than last. Forwarding it
/// under the host's name would print the expression as the file.
///
/// # Safety
///
/// Each pointer is a NUL-terminated string or null.
pub unsafe extern "C" fn assert_rtn(
    function: *const c_char,
    file: *const c_char,
    line: c_int,
    expression: *const c_char,
) -> ! {
    // SAFETY: the caller's guarantee; a null is reported as such rather than read.
    let text = |p: *const c_char| unsafe {
        match p.is_null() {
            true => "<null>".to_string(),
            false => CStr::from_ptr(p).to_string_lossy().into_owned(),
        }
    };
    eprintln!(
        "Assertion failed: ({}), function {}, file {}, line {}.",
        text(expression),
        text(function),
        text(file),
        line
    );
    std::process::abort()
}

/// `void srandomdev(void)`
///
/// Seeds `random()` from the kernel rather than from the clock. BSD's, and the
/// point of it is that the seed is not guessable, so a fallback that used the
/// time would answer the wrong question - this one fails to a seed drawn from
/// whatever entropy the host did give, and asks for it in the one way that
/// cannot fail silently.
pub unsafe extern "C" fn srandomdev() {
    let mut seed = [0u8; 4];
    // SAFETY: four writable bytes and their own length. A short read leaves the
    // rest zero, which is a weaker seed and not an incorrect one.
    let read = unsafe {
        libc::getrandom(
            seed.as_mut_ptr().cast::<c_void>(),
            seed.len(),
            0 as libc::c_uint,
        )
    };
    if read < seed.len() as isize {
        // Nothing better is available, and `random()` unseeded is worse than
        // `random()` seeded badly: it returns the same sequence every run.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        seed = now.to_ne_bytes();
    }
    // SAFETY: a scalar; `srand` touches no caller memory. glibc's `srandom` and
    // `srand` seed the same generator, and only the latter is declared here.
    unsafe { libc::srand(u32::from_ne_bytes(seed)) };
}

/// `errno_t memset_s(void *dest, rsize_t destsz, int ch, rsize_t count)`
///
/// C11 Annex K, which Apple ships and glibc does not. Two things separate it
/// from `memset` and both matter to the callers that reach for it - OpenSSL's
/// `OPENSSL_cleanse` among them:
///
/// - it takes the destination's *size* as well as the count, and refuses rather
///   than overruns;
/// - the write may not be optimised away, which is the whole reason a program
///   clearing a key calls this instead of `memset`.
///
/// The runtime-constraint handler Annex K also describes is not called: this
/// host has no `set_constraint_handler_s` to have installed one through, so
/// there is nothing that could have been registered.
///
/// # Safety
///
/// `dest` is `destsz` writable bytes, or null with `destsz` zero.
pub unsafe extern "C" fn memset_s(
    dest: *mut c_void,
    destsz: usize,
    ch: c_int,
    count: usize,
) -> c_int {
    const RSIZE_MAX: usize = isize::MAX as usize;
    if dest.is_null() {
        return libc::EINVAL;
    }
    if destsz > RSIZE_MAX || count > RSIZE_MAX {
        return libc::ERANGE;
    }
    // On a violation the standard still requires the destination be cleared as
    // far as it goes, so the caller is never left holding the secret it asked to
    // destroy.
    let written = count.min(destsz);
    // SAFETY: `written` is within `destsz`, which the caller says is writable.
    // Volatile so the store survives an optimiser that can see the buffer dies.
    unsafe {
        let bytes = dest.cast::<u8>();
        for offset in 0..written {
            bytes.add(offset).write_volatile(ch as u8);
        }
    }
    match count > destsz {
        true => libc::ERANGE,
        false => 0,
    }
}

pub const NAMES: &[&str] = &[
    "atexit",
    "atexit_b",
    "pthread_atfork",
    "__assert_rtn",
    "srandomdev",
    "memset_s",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "atexit" => atexit as *const (),
        "atexit_b" => atexit_b as *const (),
        "pthread_atfork" => pthread_atfork as *const (),
        "__assert_rtn" => assert_rtn as *const (),
        "srandomdev" => srandomdev as *const (),
        "memset_s" => memset_s as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(
            lib,
            &format!("_{name}"),
            symbol(name).unwrap_or(0) as *const (),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
        }
    }

    /// The count is what is written, and a count past the destination is an
    /// error that still clears as much as it may.
    #[test]
    fn memset_s_clears_what_it_can_and_refuses_the_rest() {
        let mut buffer = [0xAAu8; 8];
        // SAFETY: eight writable bytes, and a count within them.
        let rc = unsafe { memset_s(buffer.as_mut_ptr().cast(), buffer.len(), 0, 4) };
        assert_eq!(rc, 0);
        assert_eq!(buffer, [0, 0, 0, 0, 0xAA, 0xAA, 0xAA, 0xAA]);

        // SAFETY: as above, with a count that overruns the size it was given.
        let rc = unsafe { memset_s(buffer.as_mut_ptr().cast(), buffer.len(), 0, 16) };
        assert_eq!(rc, libc::ERANGE);
        assert_eq!(buffer, [0u8; 8], "the destination is still cleared");
    }

    #[test]
    fn memset_s_refuses_a_null_destination_rather_than_writing_to_it() {
        // SAFETY: null with a zero size, which is what the check is for.
        assert_eq!(
            unsafe { memset_s(core::ptr::null_mut(), 0, 0, 0) },
            libc::EINVAL
        );
    }

    /// Seeding twice from the kernel gives two different sequences, which is the
    /// difference between this and `srandom(1)`.
    #[test]
    fn srandomdev_seeds_from_something_that_changes() {
        // SAFETY: takes and touches nothing.
        unsafe { srandomdev() };
        let first = unsafe { libc::rand() };
        unsafe { srandomdev() };
        let second = unsafe { libc::rand() };
        assert_ne!(first, second, "the seed did not change between calls");
    }
}
