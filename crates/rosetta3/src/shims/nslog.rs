//! `NSLog`, which is `printf` with two Cocoa-shaped complications.
//!
//! It is variadic, so it lands here for the reason every variadic does: SysV
//! AMD64 leaves the arguments in registers and Apple's arm64 ABI wants them on
//! the stack, and nothing bridges that generically. What makes it a module of
//! its own rather than another entry in [`super::printf`] is the other two:
//!
//! - **The format is an `NSString`, not a `char *`.** There is no C string to
//!   read at `rdi`; there is an object, and getting its bytes means asking the
//!   host's CoreFoundation.
//! - **`%@` is a conversion**, and rendering it means sending `description` to
//!   whatever the guest passed. A message, from inside a shim — which works,
//!   because the receiver is a host object either way: a guest object is a host
//!   allocation (M6.2) and a guest class becomes the host's before it crosses.
//!
//! ## Why it forwards rather than prints
//!
//! Rendering here and writing to `stderr` would be a line that looks like
//! `NSLog`'s and is not one: the real thing timestamps, names the process, and
//! goes to the unified log as well as to the terminal. A guest's log line is
//! something a *person* reads next to the host's own, so it is worth being the
//! same thing. The rendered text is handed to the host's own `NSLog` through a
//! `%s` format, which is the narrowest possible use of it — one C string, no
//! conversion left for it to reinterpret, and no chance of a `%` in guest data
//! being read as a conversion twice.
//!
//! ## Scope
//!
//! The conversions are [`super::printf`]'s, plus `%@`. `%C` and `%S` — Cocoa's
//! wide-character forms — are not implemented, and are emitted literally like
//! any other unrecognised conversion. An object whose `description` is not a
//! string, or a format that is not one, renders as an empty field rather than
//! failing the call: a logging call must not be the thing that stops a program.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::printf::{VarArgs, render};
use super::set_int_return;

/// `kCFStringEncodingUTF8`. A constant rather than an import: it is part of the
/// CoreFoundation *interface*, and one that has never changed.
const UTF8: u32 = 0x0800_0100;

/// The largest rendering of one object or format this shim will read back out
/// of CoreFoundation.
///
/// Totality, in the same spirit as [`crate::abi::MAX_CSTR`]: the string comes
/// from a guest object's `description`, and a runaway one must cost a truncated
/// log line rather than the process.
const MAX_TEXT: usize = 64 * 1024;

/// Service `NSLog`.
pub fn nslog(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let Some(out) = render_format(symbol, ctx, &*mem, ctx.rdi, 1)? else {
        // A null or unreadable format is the guest's error to have, and the
        // host's `NSLog` treats one as nothing to print.
        set_int_return(ctx, 0);
        return Ok(());
    };
    emit(&out);
    Ok(())
}

/// Render the `NSString` format at `fmt_obj` against the guest's variadic tail.
///
/// `fixed_int` is how many integer argument registers the *named* parameters
/// have already used, exactly as [`VarArgs::new`] means it: one for `NSLog`,
/// and for a method as many as `self`, `_cmd` and the arguments up to and
/// including the format itself.
///
/// `Ok(None)` means the format was not a readable string, which is the guest's
/// error and not this runtime's — a logging or formatting call must not be the
/// thing that stops a program.
pub fn render_format(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &dyn GuestMemory,
    fmt_obj: u64,
    fixed_int: usize,
) -> Result<Option<Vec<u8>>> {
    let Some(fmt) = cf_string_bytes(fmt_obj) else {
        return Ok(None);
    };
    let describe = |obj: u64| description_bytes(obj).unwrap_or_default();
    let mut args = VarArgs::with_objects(symbol, ctx, mem, fixed_int, &describe);
    render(&fmt, &mut args).map(Some)
}

/// `bytes` as an autoreleased `NSString`, with every `%` doubled.
///
/// The doubling is what makes this usable as a *replacement format*: a variadic
/// method whose arguments have already been rendered is re-sent with the result
/// standing in for its format string, and a `%` that came out of guest data
/// must not be read as a conversion a second time.
///
/// Autoreleased rather than owned, because the caller is standing in for a
/// method that took a format it did not own either. A string created `+1` here
/// and never released would leak once per formatted message, which for a
/// program that formats in a draw loop is every frame.
pub fn escaped_string(bytes: &[u8]) -> Option<u64> {
    let mut escaped = Vec::with_capacity(bytes.len());
    for b in bytes {
        escaped.push(*b);
        if *b == b'%' {
            escaped.push(b'%');
        }
    }
    let create = dlsym("CFStringCreateWithBytes")?;
    let autorelease = dlsym("CFAutorelease")?;
    // SAFETY: both are CoreFoundation's own, called with their declared
    // signatures. A null allocator means the default one; the byte slice
    // outlives the call, and `CFStringCreateWithBytes` copies it.
    unsafe {
        let f: unsafe extern "C" fn(*const c_void, *const u8, isize, u32, u8) -> *const c_void =
            core::mem::transmute(create);
        let s = f(
            core::ptr::null(),
            escaped.as_ptr(),
            escaped.len() as isize,
            UTF8,
            0,
        );
        if s.is_null() {
            return None;
        }
        let auto: unsafe extern "C" fn(*const c_void) -> *const c_void =
            core::mem::transmute(autorelease);
        Some(auto(s) as usize as u64)
    }
}

/// Hand `text` to the host's own `NSLog`, as the whole of a `%s` conversion.
fn emit(text: &[u8]) {
    let (Some(nslog), Some(fmt)) = (host_nslog(), percent_s()) else {
        return;
    };
    // An interior NUL would end the C string early; replacing it keeps the rest
    // of the line, which is what a reader wants from a log.
    let sanitised: Vec<u8> = text
        .iter()
        .map(|b| if *b == 0 { b'?' } else { *b })
        .collect();
    let Ok(c) = CString::new(sanitised) else {
        return;
    };
    // SAFETY: `nslog` is Foundation's own `NSLog`, called through a *variadic*
    // prototype — which is the load-bearing part, since Apple's arm64 ABI
    // passes a variadic argument on the stack and a fixed prototype would put
    // `c` in `x1` where the callee never looks. `fmt` is a `CFStringRef` this
    // module created and keeps alive for the process, and `c` outlives the
    // call.
    unsafe { nslog(fmt, c.as_ptr()) };
}

type NsLogFn = unsafe extern "C" fn(*const c_void, ...);

/// Foundation's `NSLog`, if this process has Foundation.
///
/// Looked up rather than linked, for the reason
/// [`crate::objc::dispatch`] looks its runtime up: a guest that never touches
/// Cocoa should not drag Foundation into the process because this file exists.
/// A guest that calls `NSLog` has loaded it by definition.
fn host_nslog() -> Option<NsLogFn> {
    static FOUND: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *FOUND.get_or_init(|| dlsym("NSLog").map(|p| p as usize));
    // SAFETY: `dlsym` returned Foundation's `NSLog`, whose C declaration is
    // `void NSLog(NSString *format, ...)`.
    addr.map(|a| unsafe { core::mem::transmute::<usize, NsLogFn>(a) })
}

/// A `CFStringRef` for `"%s"`, made once and never released.
fn percent_s() -> Option<*const c_void> {
    static FMT: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *FMT.get_or_init(|| {
        let create = dlsym("CFStringCreateWithCString")?;
        // SAFETY: the signature is CoreFoundation's, and a null allocator means
        // the default one. The literal outlives the call.
        let s = unsafe {
            let f: unsafe extern "C" fn(*const c_void, *const c_char, u32) -> *const c_void =
                core::mem::transmute(create);
            f(core::ptr::null(), c"%s".as_ptr(), UTF8)
        };
        (!s.is_null()).then_some(s as usize)
    });
    addr.map(|a| a as *const c_void)
}

/// The UTF-8 bytes of a `CFStringRef`/`NSString *`.
fn cf_string_bytes(obj: u64) -> Option<Vec<u8>> {
    if obj == 0 {
        return None;
    }
    let get = dlsym("CFStringGetCString")?;
    let length = dlsym("CFStringGetLength")?;
    // SAFETY: both are CoreFoundation's own, called with their declared
    // signatures. `obj` is a `CFStringRef` — either the host's, or a constant
    // `@"..."` in the guest's `__cfstring` whose `isa` is a bind of
    // `___CFConstantStringClassReference` and which the host runtime therefore
    // understands (see the roadmap).
    unsafe {
        let len_fn: unsafe extern "C" fn(*const c_void) -> isize = core::mem::transmute(length);
        let n = len_fn(obj as usize as *const c_void);
        if n < 0 {
            return None;
        }
        // Four bytes per UTF-16 unit is the worst case UTF-8 needs, plus the
        // NUL. Capped, because the string may be a guest object's own.
        let cap = (n as usize)
            .saturating_mul(4)
            .saturating_add(1)
            .min(MAX_TEXT);
        let mut buf = vec![0u8; cap];
        let get_fn: unsafe extern "C" fn(*const c_void, *mut c_char, isize, u32) -> u8 =
            core::mem::transmute(get);
        let ok = get_fn(
            obj as usize as *const c_void,
            buf.as_mut_ptr().cast(),
            cap as isize,
            UTF8,
        );
        if ok == 0 {
            return None;
        }
        let s = CStr::from_bytes_until_nul(&buf).ok()?;
        Some(s.to_bytes().to_vec())
    }
}

/// `[obj description]`, as UTF-8.
///
/// The one place this runtime sends a message with nothing of the guest's
/// involved: `description` is answered by the host runtime, and if the receiver
/// is a guest object it is answered by the guest's own `IMP` through the thunk
/// M6.2 registered — which is exactly what `NSLog("%@", myObject)` means.
fn description_bytes(obj: u64) -> Option<Vec<u8>> {
    if obj == 0 {
        // Cocoa prints `(null)` for a nil object, and a reader who sees an
        // empty field instead would look for the wrong bug.
        return Some(b"(null)".to_vec());
    }
    // SAFETY: `objc_msgSend` is taken as an address and called through the
    // prototype `- (NSString *)description` has, which is the same reasoning
    // [`crate::objc::dispatch`] uses and the reason `host::objc_msgSend` is
    // declared with no parameters.
    let text = unsafe {
        let send: unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void =
            core::mem::transmute(crate::objc::host::objc_msgSend as *const ());
        let sel = crate::objc::host::sel_registerName(c"description".as_ptr());
        send(obj as usize as *const c_void, sel.cast())
    };
    cf_string_bytes(text as usize as u64)
}

/// Resolve a symbol in an image this process has already loaded.
fn dlsym(name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` (-2) searches loaded images and answers null for a
    // symbol nothing exports, which is checked rather than dereferenced.
    let p = unsafe { libc::dlsym(-2isize as *mut c_void, c.as_ptr()) };
    (!p.is_null()).then_some(p)
}
