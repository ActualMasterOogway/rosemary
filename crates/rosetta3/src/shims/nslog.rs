//! `NSLog`, which is `printf` with two Cocoa-shaped complications.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;

use microlift::arch::x86_64::context::RegisterContext;

use crate::host::symbol_ptr as dlsym;
use crate::objc::send::{Id, class, responds, sel, send0, send1};
use crate::abi::{GuestMemory, Result};

use super::printf::{VarArgs, render};
use super::set_int_return;

/// `kCFStringEncodingUTF8`. A constant rather than an import: it is part of the
/// CoreFoundation *interface*, and one that has never changed.
const UTF8: u32 = 0x0800_0100;

/// The largest rendering of one object or format this shim will read back out of
/// CoreFoundation.
///
/// Totality, in the same spirit as [`crate::abi::MAX_CSTR`].
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

/// Service `NSRunAlertPanel`.
///
/// `NSInteger NSRunAlertPanel(NSString *title, NSString *msgFormat, NSString
/// *defaultButton, NSString *alternateButton, NSString *otherButton, ...)` - the same
/// two complications as `NSLog` (variadic, `NSString` format) with the format in the
/// *second* position rather than the first, so five integer registers are named rather
/// than one.
pub fn run_alert_panel(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    // Five named parameters, and the format is the second. Everything after
    // `otherButton` is the variadic tail the format consumes.
    let out = render_format(symbol, ctx, &*mem, ctx.rsi, 5)?.unwrap_or_default();
    let title = cf_string_bytes(ctx.rdi).unwrap_or_default();
    eprintln!(
        "rosetta3: guest alert: {}: {}",
        String::from_utf8_lossy(&title),
        String::from_utf8_lossy(&out)
    );

    let (Some(host), Some(message)) = (dlsym(symbol), escaped_string(&out)) else {
        // Nothing to show it with, but the text is already out; the guest's
        // caller sees the default button, which is what dismissing the panel
        // would have returned.
        ctx.rax = NS_ALERT_DEFAULT_RETURN;
        return Ok(());
    };
    // SAFETY: `symbol` resolved in an AppKit this process has loaded - the
    // guest's own window brought it in - and its declaration is the one above.
    // Called through a *variadic* prototype with no variadic arguments, which
    // is what the rendered format standing in for the guest's makes correct:
    // there is nothing left for it to convert. The buttons are the guest's own
    // `NSString`s, which outlive a modal call it is blocked in.
    let n = unsafe {
        let f: unsafe extern "C" fn(u64, u64, u64, u64, u64, ...) -> i64 =
            core::mem::transmute(host as usize);
        f(ctx.rdi, message, ctx.rdx, ctx.rcx, ctx.r8)
    };
    // Which button the panel came back with is part of the diagnostic.
    eprintln!("rosetta3: guest alert: answered {n}");
    // `NSInteger` is sixty-four bits wide, so the whole of `rax` carries it
    // rather than [`super::set_int_return`]'s `int`.
    ctx.rax = n as u64;
    Ok(())
}

/// `NSAlertDefaultReturn`, which is what dismissing a panel with its default
/// button gives - the answer a guest handles by definition, since that button
/// is the one it always supplies.
const NS_ALERT_DEFAULT_RETURN: u64 = 1;

/// Render the `NSString` format at `fmt_obj` against the guest's variadic tail.
///
/// `fixed_int` is how many integer argument registers the *named* parameters have
/// already used, exactly as [`VarArgs::new`] means it:
///
/// `Ok(None)` means the format was not a readable string, which is the guest's error
/// and not this runtime's - a logging or formatting call must not be the thing that
/// stops a program.
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
///
/// With no Foundation to hand it to, it goes to stderr in the shape Apple's
/// `NSLog` writes. Losing the guest's log entirely is the one outcome worth
/// avoiding: it is how a Cocoa program says what went wrong.
fn emit(text: &[u8]) {
    // An interior NUL would end the C string early; replacing it keeps the rest
    // of the line, which is what a reader wants from a log.
    let sanitised: Vec<u8> = text
        .iter()
        .map(|b| if *b == 0 { b'?' } else { *b })
        .collect();
    let (Some(nslog), Some(fmt)) = (host_nslog(), percent_s()) else {
        eprintln!("{}", String::from_utf8_lossy(&sanitised));
        return;
    };
    let Ok(c) = CString::new(sanitised) else {
        return;
    };
    // SAFETY: `nslog` is Foundation's own `NSLog`, called through a *variadic*
    // prototype - which is the load-bearing part, since Apple's arm64 ABI
    // passes a variadic argument on the stack and a fixed prototype would put
    // `c` in `x1` where the callee never looks. `fmt` is a `CFStringRef` this
    // module created and keeps alive for the process, and `c` outlives the
    // call.
    unsafe { nslog(fmt, c.as_ptr()) };
}

type NsLogFn = unsafe extern "C" fn(*const c_void, ...);

/// Foundation's `NSLog`, if this process has Foundation.
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
///
/// A word that is not an object *at all* is still not survivable, and cannot be.
fn cf_string_bytes(obj: u64) -> Option<Vec<u8>> {
    if obj == 0 {
        return None;
    }
    if !is_kind_of(obj, "NSString") {
        return None;
    }
    let get = dlsym("CFStringGetCString")?;
    let length = dlsym("CFStringGetLength")?;
    // SAFETY: both are CoreFoundation's own, called with their declared
    // signatures. `obj` is a `CFStringRef` - either the host's, or a constant
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
fn description_bytes(obj: u64) -> Option<Vec<u8>> {
    if obj == 0 {
        // Cocoa prints `(null)` for a nil object, and a reader who sees an
        // empty field instead would look for the wrong bug.
        return Some(b"(null)".to_vec());
    }
    // Every `NSObject` implements `description`, and not every object is an
    // `NSObject` - a class built on `NSProxy`, or one of the few root classes
    // Foundation keeps, answers `doesNotRecognizeSelector:` and raises.
    if !responds_to(obj, "description") {
        return None;
    }
    // SAFETY: the prototype `- (NSString *)description` has, against an object
    // that was just shown to respond to it.
    let text = unsafe { send0::<Id>(obj as usize as Id, sel("description")) };
    cf_string_bytes(text as usize as u64)
}

/// Whether `obj` is an instance of `class_name` or one of its subclasses.
fn is_kind_of(obj: u64, class_name: &str) -> bool {
    let cls = class(class_name);
    if cls.is_null() {
        return false;
    }
    // SAFETY: `- (BOOL)isKindOfClass:(Class)`, whose argument is the class just
    // looked up. A `BOOL` is one byte.
    unsafe { send1::<u8, Id>(obj as usize as Id, sel("isKindOfClass:"), cls) != 0 }
}

/// Whether `obj`'s class implements `selector`.
fn responds_to(obj: u64, selector: &str) -> bool {
    // SAFETY: `obj` is an object pointer - the host runtime's own allocation,
    // or a guest constant whose `isa` is a bind of a Foundation class.
    unsafe { responds(obj as usize as Id, sel(selector)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load Foundation, and say whether these tests can run at all.
    ///
    /// The driver deliberately never links it - the roadmap records what a framework's
    /// initializers do to the `errno` a guest then reads - so a test binary has no
    /// `NSString` until it asks for one.
    ///
    /// Also false under `ROSETTA3_OBJC=builtin`. Everything below reaches the
    /// host's CoreFoundation by `dlsym` and hands it an object found by name,
    /// and under the builtin runtime that object is one of ours. The subject of
    /// these tests is the host's CF, so with the host's runtime out of the
    /// picture there is nothing here to test rather than something failing.
    fn foundation() -> bool {
        if crate::objc::host::backend() != crate::objc::host::Backend::Native {
            return false;
        }
        // SAFETY: `dlopen` of an absolute path, answering null when absent.
        let handle = unsafe {
            libc::dlopen(
                c"/System/Library/Frameworks/Foundation.framework/Foundation".as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_GLOBAL,
            )
        };
        !handle.is_null()
    }

    /// The guard, against the host's own runtime: an object that is not a string must
    /// come back as `None` rather than raise.
    #[test]
    fn a_non_string_object_is_not_asked_for_its_bytes() {
        if !foundation() {
            return;
        }
        // SAFETY: `objc_getClass` answers null for a name no class has, which
        // is what the skip below checks. A class object is an object.
        let object_class = unsafe { crate::objc::host::objc_getClass(c"NSObject".as_ptr()) };
        if object_class.is_null() {
            return;
        }
        assert_eq!(cf_string_bytes(object_class as u64), None);
    }

    /// The other side of the same claim: a real string still renders, so the
    /// guard is a filter rather than a wall.
    #[test]
    fn a_real_string_still_reads_back() {
        if !foundation() {
            return;
        }
        let Some(fmt) = percent_s() else {
            return;
        };
        assert_eq!(
            cf_string_bytes(fmt as usize as u64).as_deref(),
            Some(&b"%s"[..])
        );
    }

    /// `description` is guarded the same way, and a class object answers it -
    /// so the guard must not refuse the case it is meant to let through.
    #[test]
    fn a_class_describes_itself() {
        if !foundation() {
            return;
        }
        // SAFETY: as above.
        let object_class = unsafe { crate::objc::host::objc_getClass(c"NSObject".as_ptr()) };
        if object_class.is_null() {
            return;
        }
        assert_eq!(
            description_bytes(object_class as u64).as_deref(),
            Some(&b"NSObject"[..])
        );
    }
}
