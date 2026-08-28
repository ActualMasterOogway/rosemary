//! `objc_msgSend`.
//!
//! The one entry point that cannot be written in Rust: it takes its arguments in
//! whatever registers the call site used and must pass them through untouched to
//! an implementation whose prototype it does not know. So it is a naked function
//! that spills the argument registers, calls [`lookup`] for the `IMP`, restores
//! them, and jumps.
//!
//! Guest sends do not come through here - `rosetta3`'s dispatch intercepts
//! `objc_msgSend` by name and resolves the target itself. This is for messages
//! sent *by* the runtime and by this tree's own framework classes, and for the
//! address the guest's import slot has to be bound to.

use std::ffi::c_void;

use crate::object::Id;
use crate::sel::Sel;

/// The `IMP` to jump to for `(receiver, sel)`, or the "unrecognised" trampoline.
///
/// Called from [`msg_send`]'s prologue with the receiver and selector only; the
/// rest of the arguments are still spilled on its frame.
///
/// # Safety
///
/// `receiver` must be a non-null object pointer.
unsafe extern "C" fn lookup(receiver: Id, sel: Sel) -> *const c_void {
    // SAFETY: the caller's obligation.
    let Some(class) = (unsafe { crate::object::class_of(receiver) }) else {
        return unrecognised as *const c_void;
    };
    match class.method(sel) {
        Some(method) => method.implementation(),
        None => unrecognised as *const c_void,
    }
}

/// What a message no class answers lands on.
///
/// A named abort rather than a jump to null: the selector and the class are the
/// whole of the diagnostic, and a null call would print neither.
unsafe extern "C" fn unrecognised(receiver: Id, sel: Sel) -> ! {
    // SAFETY: reached only from `lookup`, which was given a live receiver.
    let class = unsafe { crate::object::class_of(receiver) };
    let name = class.map_or("nil", |c| c.name_str());
    let selector = match sel.is_null() {
        true => std::borrow::Cow::Borrowed("<null selector>"),
        // SAFETY: a registered selector is a NUL-terminated interned name.
        false => unsafe { std::ffi::CStr::from_ptr(sel) }.to_string_lossy(),
    };
    panic!("unrecognized selector sent to instance of {name}: {selector}");
}

/// `objc_msgSend`'s address.
pub fn entry() -> *const c_void {
    msg_send as *const c_void
}

/// The trampoline itself.
///
/// Naked rather than assembled separately so that the symbol is Rust's to name:
/// this runtime must never export `objc_msgSend` under that spelling, or a Mac
/// could not load it alongside the host's own libobjc - which is exactly the
/// arrangement that lets both be tested here.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn msg_send() {
    // x0 = receiver, x1 = selector, x2-x7 and v0-v7 the rest, x8 an indirect
    // return buffer. All of them have to survive the lookup call.
    core::arch::naked_asm!(
        "cbz     x0, 9f",
        "stp     x29, x30, [sp, #-16]!",
        "mov     x29, sp",
        "sub     sp, sp, #208",
        "stp     q0, q1, [sp, #0]",
        "stp     q2, q3, [sp, #32]",
        "stp     q4, q5, [sp, #64]",
        "stp     q6, q7, [sp, #96]",
        "stp     x0, x1, [sp, #128]",
        "stp     x2, x3, [sp, #144]",
        "stp     x4, x5, [sp, #160]",
        "stp     x6, x7, [sp, #176]",
        "str     x8,      [sp, #192]",
        "bl      {lookup}",
        "mov     x9, x0",
        "ldp     q0, q1, [sp, #0]",
        "ldp     q2, q3, [sp, #32]",
        "ldp     q4, q5, [sp, #64]",
        "ldp     q6, q7, [sp, #96]",
        "ldp     x0, x1, [sp, #128]",
        "ldp     x2, x3, [sp, #144]",
        "ldp     x4, x5, [sp, #160]",
        "ldp     x6, x7, [sp, #176]",
        "ldr     x8,      [sp, #192]",
        "add     sp, sp, #208",
        "ldp     x29, x30, [sp], #16",
        "br      x9",
        // Messaging nil is the language, not an error: the return registers are
        // zeroed and the send is over. x0 is already zero, which is the test.
        "9:",
        "mov     x1, #0",
        "movi    d0, #0",
        "movi    d1, #0",
        "movi    d2, #0",
        "movi    d3, #0",
        "ret",
        lookup = sym lookup,
    )
}

/// [`msg_send`] for x86-64, where this runtime is cross-built.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn msg_send() {
    // rdi = receiver, rsi = selector, rdx/rcx/r8/r9 and xmm0-7 the rest, and rax
    // the vector-register count a variadic call site set.
    core::arch::naked_asm!(
        "test    rdi, rdi",
        "je      9f",
        "push    rbp",
        "mov     rbp, rsp",
        "sub     rsp, 192",
        "movaps  [rsp + 0], xmm0",
        "movaps  [rsp + 16], xmm1",
        "movaps  [rsp + 32], xmm2",
        "movaps  [rsp + 48], xmm3",
        "movaps  [rsp + 64], xmm4",
        "movaps  [rsp + 80], xmm5",
        "movaps  [rsp + 96], xmm6",
        "movaps  [rsp + 112], xmm7",
        "mov     [rsp + 128], rdi",
        "mov     [rsp + 136], rsi",
        "mov     [rsp + 144], rdx",
        "mov     [rsp + 152], rcx",
        "mov     [rsp + 160], r8",
        "mov     [rsp + 168], r9",
        "mov     [rsp + 176], rax",
        "call    {lookup}",
        "mov     r11, rax",
        "movaps  xmm0, [rsp + 0]",
        "movaps  xmm1, [rsp + 16]",
        "movaps  xmm2, [rsp + 32]",
        "movaps  xmm3, [rsp + 48]",
        "movaps  xmm4, [rsp + 64]",
        "movaps  xmm5, [rsp + 80]",
        "movaps  xmm6, [rsp + 96]",
        "movaps  xmm7, [rsp + 112]",
        "mov     rdi, [rsp + 128]",
        "mov     rsi, [rsp + 136]",
        "mov     rdx, [rsp + 144]",
        "mov     rcx, [rsp + 152]",
        "mov     r8,  [rsp + 160]",
        "mov     r9,  [rsp + 168]",
        "mov     rax, [rsp + 176]",
        "add     rsp, 192",
        "pop     rbp",
        "jmp     r11",
        "9:",
        "xor     eax, eax",
        "xor     edx, edx",
        "xorps   xmm0, xmm0",
        "xorps   xmm1, xmm1",
        "ret",
        lookup = sym lookup,
    )
}
