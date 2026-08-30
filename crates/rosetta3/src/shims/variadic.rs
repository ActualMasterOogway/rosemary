//! A variadic entry point for the frameworks' format methods.
//!
//! `crates/foundation` is written for the emulated backend, where a variadic
//! message never reaches an `IMP` at all: `objc::dispatch` reads the format and
//! the guest's variadic tail out of its registers, renders them, and re-sends
//! the message with the result standing in for the format. A native image sends
//! through `objc_msgSend` for real, so the method *is* reached - and an `IMP` is
//! not variadic, so everything after the format is invisible to it.
//!
//! This installs the same rendering in front of each of those methods. What
//! they receive is what they already expect: a rendered string with every `%`
//! doubled, which their own `unescape_rendered` undoes.

use std::sync::OnceLock;

use crate::objc::host::{self, Imp, Sel};

/// The methods whose implementation this module stands in front of, as
/// `(class, selector, whether it is a class method)`.
///
/// All four take the format as the *first* message argument, which is what lets
/// one trampoline serve them.
const FORMATTERS: &[(&str, &str, bool)] = &[
    ("NSString", "stringWithFormat:", true),
    ("NSString", "localizedStringWithFormat:", true),
    ("NSString", "initWithFormat:", false),
    ("NSMutableString", "appendFormat:", false),
];

/// The implementation each selector had before this module replaced it.
///
/// Keyed by selector alone. That is only correct while no two entries above
/// share one, which [`install`] checks rather than assumes: a second class
/// installing the same selector would otherwise be dispatched to the first's.
static ORIGINALS: OnceLock<Vec<(u64, u64)>> = OnceLock::new();

/// Swizzle each format method onto [`trampoline`].
///
/// A method this tree does not define is skipped rather than reported: which
/// classes exist is the frameworks' business, and they are allowed to grow.
pub fn install() {
    let mut originals: Vec<(u64, u64)> = Vec::new();
    for (class, selector, is_class_method) in FORMATTERS {
        let (Some(c), Some(s)) = (host::c_string(class), host::c_string(selector)) else {
            continue;
        };
        // SAFETY: both strings are NUL-terminated and outlive the calls.
        let (method, sel) = unsafe {
            let cls = host::objc_lookUpClass(c.as_ptr());
            if cls.is_null() {
                continue;
            }
            let sel = host::sel_registerName(s.as_ptr());
            let method = match is_class_method {
                true => host::class_getClassMethod(cls, sel),
                false => host::class_getInstanceMethod(cls, sel),
            };
            (method, sel)
        };
        if method.is_null() {
            continue;
        }
        // SAFETY: `method` is a `Method` the runtime just returned.
        let previous = unsafe { host::method_setImplementation(method, trampoline as Imp) };
        if previous.is_null() || previous == trampoline as Imp {
            continue;
        }
        let key = sel as u64;
        if let Some(clash) = originals.iter().find(|(s, imp)| *s == key && *imp != previous as u64)
        {
            objc::fail!(
                "{selector} has two implementations ({:#x} and {:#x}), which the \
                 selector-keyed table here cannot tell apart",
                clash.1,
                previous as u64
            );
        }
        originals.push((key, previous as u64));
    }
    let _ = ORIGINALS.set(originals);
}

/// The implementation `sel` had before [`install`].
fn original(sel: Sel) -> Option<Imp> {
    let key = sel as u64;
    ORIGINALS
        .get()?
        .iter()
        .find(|(s, _)| *s == key)
        .map(|(_, imp)| *imp as usize as Imp)
}

/// Render the format against the tail and hand the original implementation the
/// result.
///
/// `tail` is a `va_list` on x86-64 and the address of the first stack argument
/// on AArch64, which is what Darwin's `va_list` is there.
unsafe extern "C" fn service(receiver: u64, sel: Sel, format: u64, tail: u64) -> u64 {
    let Some(original) = original(sel) else {
        objc::fail!("a format method was serviced before its implementation was recorded");
    };
    // SAFETY: `original` is the `IMP` the runtime held for this selector, and
    // all four of them take `(id, SEL, NSString *)`. The one returning void
    // leaves the return register undefined, which is what discarding it means.
    let call = |fmt: u64| unsafe {
        let f: unsafe extern "C" fn(u64, Sel, u64) -> u64 = core::mem::transmute(original);
        f(receiver, sel, fmt)
    };
    match super::nslog::native::render_escaped("stringWithFormat:", format, tail) {
        Some(rendered) => call(rendered),
        // An unreadable format is the caller's error to have, and passing it
        // through is what the emulated path does with one too.
        None => call(format),
    }
}

/// Three fixed arguments - receiver, selector, format - and the tail after them.
///
/// See `shims::nslog::native` for what each of the two ABIs needs; this is the
/// same capture with one more named parameter.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    core::arch::naked_asm!(
        "mov x3, sp",
        "b   {service}",
        service = sym service,
    )
}

#[cfg(not(target_arch = "aarch64"))]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    core::arch::naked_asm!(
        "push rbp",
        "mov  rbp, rsp",
        "sub  rsp, 208",
        "mov  [rsp + 24], rcx",
        "mov  [rsp + 32], r8",
        "mov  [rsp + 40], r9",
        "test al, al",
        "je   2f",
        "movaps [rsp + 48],  xmm0",
        "movaps [rsp + 64],  xmm1",
        "movaps [rsp + 80],  xmm2",
        "movaps [rsp + 96],  xmm3",
        "movaps [rsp + 112], xmm4",
        "movaps [rsp + 128], xmm5",
        "movaps [rsp + 144], xmm6",
        "movaps [rsp + 160], xmm7",
        "2:",
        // Three integer registers are the named parameters', so the tail starts
        // at the fourth.
        "mov  dword ptr [rsp + 176], 24",
        "mov  dword ptr [rsp + 180], 48",
        "lea  rax, [rbp + 16]",
        "mov  [rsp + 184], rax",
        "mov  [rsp + 192], rsp",
        "lea  rcx, [rsp + 176]",
        "call {service}",
        "mov  rsp, rbp",
        "pop  rbp",
        "ret",
        service = sym service,
    )
}
