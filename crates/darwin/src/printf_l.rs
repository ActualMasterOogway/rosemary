//! The variadic half of `xlocale.h`: `snprintf_l`, `sscanf_l`, `asprintf_l`.
//!
//! Separate from [`crate::xlocale`] only because of *how* they are written.
//! Each is the same one-line idea as the rest of that family - switch the
//! thread's locale and forward to the plain function - but a variadic C
//! function cannot be written in Rust on a stable compiler (`c_variadic`,
//! rust-lang/rust#44930), and there is no way to reach the caller's arguments
//! without one.
//!
//! So each entry point is a `global_asm!` trampoline that builds the SysV
//! `va_list` the ABI describes and calls an ordinary Rust function with a
//! pointer to it:
//!
//! - a **register save area** of the six integer argument registers followed by
//!   `xmm0..7`, which is where `va_arg` reads an argument that was passed in a
//!   register;
//! - `gp_offset`, the index into that area of the first *unnamed* integer
//!   argument - eight bytes per named register argument, so 32 for
//!   `snprintf_l`'s four and 24 for the other two's three;
//! - `fp_offset` 48, where the vector half of the save area begins;
//! - `overflow_arg_area` at `rbp+16`, the caller's own stack arguments, which
//!   begin after the saved frame pointer and the return address.
//!
//! Shifting the register arguments down by one and tail-jumping to the plain
//! function is **not** a substitute, and it is the obvious wrong answer: it is
//! right only while every argument the caller passed fits in a register, and
//! silently drops the seventh onwards.
//!
//! The vector registers are saved unconditionally rather than under the usual
//! `test al, al`. `al` counts the vector registers the caller used, and saving
//! eight registers nothing will read costs a hundred and twenty-eight bytes of
//! stack; getting the branch wrong costs a `%f` that prints garbage.
//!
//! **x86-64 only.** The trampolines are that ABI's, and on another host these
//! names stay unbound - which is where they were before this module existed.
//! The emulated backend does not come through here at all: it has the guest's
//! registers in hand and reads the arguments itself, in
//! `rosetta3::shims::printf`.

#![cfg(all(not(target_os = "macos"), target_arch = "x86_64"))]

use std::ffi::{c_char, c_int, c_void};

use machish::resolve::SyntheticDylib;

/// The SysV `va_list`, which is one of these and not a pointer.
///
/// `#[repr(C)]` because the trampoline writes it by offset and glibc reads it
/// by offset; the two have to agree on all twenty-four bytes.
#[repr(C)]
struct VaList {
    gp_offset: u32,
    fp_offset: u32,
    overflow_arg_area: *mut c_void,
    reg_save_area: *mut c_void,
}

unsafe extern "C" {
    fn vsnprintf(buffer: *mut c_char, size: usize, format: *const c_char, ap: *mut VaList)
    -> c_int;
    fn vsscanf(input: *const c_char, format: *const c_char, ap: *mut VaList) -> c_int;
    fn vasprintf(out: *mut *mut c_char, format: *const c_char, ap: *mut VaList) -> c_int;
}

/// Build a `va_list` for a function with `$named` register arguments and call
/// `$body` with a pointer to it in the next argument register.
///
/// `$slot` is that register, which is the trampoline's one hand-written detail:
/// the assembler cannot count the named arguments for us.
macro_rules! trampoline {
    ($name:literal, $body:path, gp_offset = $named:literal, slot = $slot:literal) => {
        core::arch::global_asm!(
            ".text",
            concat!(".globl ", $name),
            concat!(".hidden ", $name),
            concat!(".type ", $name, ", @function"),
            concat!($name, ":"),
            "push rbp",
            "mov rbp, rsp",
            // 176 bytes of register save area, then the 24-byte `va_list`,
            // rounded up to keep the stack sixteen-aligned for the call.
            "sub rsp, 208",
            "mov [rsp], rdi",
            "mov [rsp + 8], rsi",
            "mov [rsp + 16], rdx",
            "mov [rsp + 24], rcx",
            "mov [rsp + 32], r8",
            "mov [rsp + 40], r9",
            "movaps [rsp + 48], xmm0",
            "movaps [rsp + 64], xmm1",
            "movaps [rsp + 80], xmm2",
            "movaps [rsp + 96], xmm3",
            "movaps [rsp + 112], xmm4",
            "movaps [rsp + 128], xmm5",
            "movaps [rsp + 144], xmm6",
            "movaps [rsp + 160], xmm7",
            concat!("mov dword ptr [rsp + 176], ", $named),
            "mov dword ptr [rsp + 180], 48",
            "lea rax, [rbp + 16]",
            "mov [rsp + 184], rax",
            "mov rax, rsp",
            "mov [rsp + 192], rax",
            concat!("lea ", $slot, ", [rsp + 176]"),
            "call {body}",
            "mov rsp, rbp",
            "pop rbp",
            "ret",
            concat!(".size ", $name, ", . - ", $name),
            body = sym $body,
        );
    };
}

/// `int snprintf_l(char *, size_t, locale_t, const char *, ...)`
///
/// # Safety
///
/// The trampoline's: the arguments are the ones the caller passed, and the
/// `va_list` describes its own frame.
unsafe extern "C" fn snprintf_l_body(
    buffer: *mut c_char,
    size: usize,
    locale: libc::locale_t,
    format: *const c_char,
    ap: *mut VaList,
) -> c_int {
    let _switched = crate::xlocale::switch(locale);
    // SAFETY: the caller's buffer and format, and a `va_list` the trampoline
    // built for this frame.
    unsafe { vsnprintf(buffer, size, format, ap) }
}

/// `int sscanf_l(const char *, locale_t, const char *, ...)`
///
/// # Safety
///
/// As [`snprintf_l_body`].
unsafe extern "C" fn sscanf_l_body(
    input: *const c_char,
    locale: libc::locale_t,
    format: *const c_char,
    ap: *mut VaList,
) -> c_int {
    let _switched = crate::xlocale::switch(locale);
    // SAFETY: as above.
    unsafe { vsscanf(input, format, ap) }
}

/// `int asprintf_l(char **, locale_t, const char *, ...)`
///
/// The result is **moved into the guest's allocator** before it is handed over.
/// `vasprintf` allocates with this host's `malloc` and the caller frees it with
/// whichever `free` its own image binds - which is the crossing
/// [`crate::heap`] describes, and the one that does not fail at the call: the
/// guest's allocator accepts the foreign pointer, decides which of its pages
/// owns it from the pointer's own bits, and pushes a live block onto that
/// page's free list.
///
/// # Safety
///
/// As [`snprintf_l_body`].
unsafe extern "C" fn asprintf_l_body(
    out: *mut *mut c_char,
    locale: libc::locale_t,
    format: *const c_char,
    ap: *mut VaList,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    let mut host: *mut c_char = std::ptr::null_mut();
    let written = {
        let _switched = crate::xlocale::switch(locale);
        // SAFETY: a live `char *` slot, the caller's format, and a `va_list`
        // the trampoline built for this frame.
        unsafe { vasprintf(&raw mut host, format, ap) }
    };
    if written < 0 || host.is_null() {
        return -1;
    }
    // SAFETY: `vasprintf` NUL-terminates, so the block is `written + 1` bytes.
    let moved = unsafe { crate::heap::malloc(written as usize + 1) }.cast::<c_char>();
    if !moved.is_null() {
        // SAFETY: both blocks hold `written + 1` bytes and do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(host, moved, written as usize + 1) };
    }
    // SAFETY: this host's `malloc` allocated it, inside `vasprintf`.
    unsafe { libc::free(host.cast()) };
    if moved.is_null() {
        return -1;
    }
    // SAFETY: the caller states `out` is writable.
    unsafe { *out = moved };
    written
}

// `rdi, rsi, rdx, rcx, r8, r9` is the order, so the argument after four named
// ones is `r8` and after three is `rcx`.
trampoline!(
    "rosetta3_snprintf_l",
    snprintf_l_body,
    gp_offset = 32,
    slot = "r8"
);
trampoline!(
    "rosetta3_sscanf_l",
    sscanf_l_body,
    gp_offset = 24,
    slot = "rcx"
);
trampoline!(
    "rosetta3_asprintf_l",
    asprintf_l_body,
    gp_offset = 24,
    slot = "rcx"
);

unsafe extern "C" {
    fn rosetta3_snprintf_l();
    fn rosetta3_sscanf_l();
    fn rosetta3_asprintf_l();
}

pub const NAMES: &[&str] = &["snprintf_l", "sscanf_l", "asprintf_l"];

pub fn symbol(name: &str) -> Option<u64> {
    let f: unsafe extern "C" fn() = match name {
        "snprintf_l" => rosetta3_snprintf_l,
        "sscanf_l" => rosetta3_sscanf_l,
        "asprintf_l" => rosetta3_asprintf_l,
        _ => return None,
    };
    Some(f as *const () as usize as u64)
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
    use std::ffi::CStr;

    /// The trampolines are variadic C functions, which is exactly what Rust
    /// cannot declare - so each is called through a transmuted pointer with the
    /// signature the caller would have used.
    fn snprintf_l()
    -> unsafe extern "C" fn(*mut c_char, usize, libc::locale_t, *const c_char, ...) -> c_int {
        // SAFETY: the trampoline is a variadic function of this shape.
        unsafe { std::mem::transmute(symbol("snprintf_l").expect("published")) }
    }

    #[test]
    fn every_listed_name_resolves() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
        }
    }

    /// Six integer arguments and one past them: the seventh is on the caller's
    /// stack, and reading it is the whole reason this is not a register shuffle.
    #[test]
    fn an_argument_past_the_registers_is_still_read() {
        let mut buffer = [0i8; 64];
        // SAFETY: a buffer this test sized, and a format matching the
        // arguments after it.
        let written = unsafe {
            snprintf_l()(
                buffer.as_mut_ptr(),
                buffer.len(),
                std::ptr::null_mut(),
                c"%d %d %d %d %d %d %d".as_ptr(),
                1,
                2,
                3,
                4,
                5,
                6,
                7,
            )
        };
        assert_eq!(written, 13);
        // SAFETY: `vsnprintf` terminated it.
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        assert_eq!(text.to_bytes(), b"1 2 3 4 5 6 7");
    }

    /// And a floating-point argument, which travels in a vector register and is
    /// read out of the other half of the save area.
    #[test]
    fn a_double_is_read_from_the_vector_registers() {
        let mut buffer = [0i8; 64];
        // SAFETY: as above.
        unsafe {
            snprintf_l()(
                buffer.as_mut_ptr(),
                buffer.len(),
                std::ptr::null_mut(),
                c"%s=%.2f".as_ptr(),
                c"x".as_ptr(),
                9.25f64,
            );
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_bytes(), b"x=9.25");
        }
    }

    /// The locale is switched for the call and put back after it, which is what
    /// the whole `_l` family is for: the same number formats differently.
    #[test]
    fn the_locale_argument_is_the_one_the_number_is_formatted_in() {
        // SAFETY: a locale this test makes and frees.
        let c_locale =
            unsafe { libc::newlocale(libc::LC_ALL_MASK, c"C".as_ptr(), std::ptr::null_mut()) };
        if c_locale.is_null() {
            eprintln!("skipped: this host has no C locale");
            return;
        }
        let mut buffer = [0i8; 32];
        // SAFETY: as above, with a real locale rather than a null one.
        unsafe {
            let before = libc::uselocale(std::ptr::null_mut());
            snprintf_l()(
                buffer.as_mut_ptr(),
                buffer.len(),
                c_locale,
                c"%.1f".as_ptr(),
                0.5f64,
            );
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_bytes(), b"0.5");
            assert_eq!(
                libc::uselocale(std::ptr::null_mut()),
                before,
                "the thread is left where it was"
            );
            libc::freelocale(c_locale);
        }
    }
}
