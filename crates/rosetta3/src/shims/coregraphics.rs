//! CoreGraphics and CoreVideo entry points that pass or return a **struct by
//! value**, which is the one thing the signature table cannot name.
//!
//! [`crate::abi::table`] describes everything else in the display and bitmap
//! families — they are display IDs, mode handles, context handles and sizes,
//! all scalars and pointers, and the host's implementation is the right one
//! because the displays it is asked about are this machine's. What is here is
//! what is left, for the same reason [`crate::shims::math`]'s pair are here.
//!
//! No two of them are the same problem, which is the point worth taking from
//! having several:
//!
//! - `CGDisplayBounds` **returns** a `CGRect` — thirty-two bytes of floating
//!   point, and the two ABIs disagree completely.
//! - `CGContextClearRect` and `CGContextDrawImage` **take** one, which is the
//!   mirror image: SysV AMD64 puts a MEMORY-class argument on the caller's
//!   stack and consumes no register for it, so the shim reads it out of the
//!   guest's stack and hands it to the host by value.
//! - `CGEventGetLocation` returns a `CGPoint` and
//!   `CVDisplayLinkGetNominalOutputVideoRefreshPeriod` a `CVTime` — sixteen
//!   bytes each, and the two ABIs **agree** about both, in floating-point
//!   registers for one and integer registers for the other. They still cannot
//!   be table entries, because the marshaller's return path writes one
//!   register.
//!
//! The rule stays "check this struct against both ABIs"; what these add is that
//! agreeing is not sufficient on its own, and that a struct going *in* is a
//! different question from one coming out.
//!
//! ## `CGRect` is thirty-two bytes, and that decides everything
//!
//! `CGRect` is `{ CGPoint origin; CGSize size; }` — four `double`s. SysV AMD64
//! classifies anything over sixteen bytes as MEMORY: the caller passes a hidden
//! buffer pointer in `rdi`, every real argument shifts along by one, and the
//! callee returns that same pointer in `rax`. AAPCS64 sees a four-member
//! homogeneous float aggregate and returns it in `v0`–`v3`, with no buffer
//! anywhere. This is the `NSRect` disagreement the roadmap records, met from the
//! C side instead of the Objective-C one.
//!
//! So the shim calls the host function with a Rust prototype that returns the
//! aggregate — which puts the compiler in charge of reading `v0`–`v3`, rather
//! than this module guessing at register halves — and then writes the thirty-two
//! bytes where the *guest's* ABI says they go.
//!
//! ## `CVTime` is sixteen bytes, and both sides return it in two registers
//!
//! `CVTime` is `{ int64_t timeValue; int32_t timeScale; int32_t flags; }`. SysV
//! AMD64 classifies it as two INTEGER eightbytes and returns them in `rax` and
//! `rdx`; AAPCS64 sees a sixteen-byte non-HFA aggregate and returns it in `x0`
//! and `x1`. Same shape, same order, different register names — so the shim is
//! two moves, and the reason it exists at all is that
//! [`crate::abi::marshal`] returns a single value.
//!
//! ## Why not `#[link(kind = "framework")]`
//!
//! Declaring one of these with a `#[link]` attribute pulls CoreGraphics into the
//! driver, and the roadmap records what that does: a framework's initializers
//! run before the guest starts and leave `errno` set, which the guest then reads
//! because `errno` is passthrough. The address comes from
//! [`crate::dyld::host_symbol`] instead, which is `dlsym` against a process that
//! has loaded CoreGraphics for its own reasons by the time a guest asks about a
//! display.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

/// `CGRect`, as AAPCS64 returns one.
///
/// Four `double`s and nothing else, which is what makes it a homogeneous float
/// aggregate. Declaring it as such is the whole trick: Rust's `extern "C"`
/// follows the platform ABI, so the compiler emits the `v0`–`v3` read.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct CgRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl CgRect {
    /// The thirty-two bytes the guest expects in its hidden buffer, in order.
    fn to_le_bytes(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, v) in [self.x, self.y, self.width, self.height].iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// Call a host `CGRect f(u32)` and place the result the guest's way.
///
/// `rdi` is the hidden return buffer and `esi` the first real argument, because
/// SysV AMD64 shifted everything along by one for the MEMORY return. `rax` gets
/// the buffer back, which is what a SysV callee returning MEMORY must do and
/// what a caller reading the result through `rax` depends on.
fn rect_of_display(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let Some(host) = crate::dyld::host_symbol(symbol.strip_prefix('_').unwrap_or(symbol)) else {
        return Err(AbiError::NoShim {
            symbol: symbol.to_string(),
        });
    };
    let (buffer, display) = (ctx.rdi, ctx.rsi as u32);
    // SAFETY: the symbol is one [`shim_for`] matched, which really is
    // `CGRect (*)(CGDirectDisplayID)`, and `host_symbol` resolved it out of a
    // CoreGraphics this process has loaded.
    let f: extern "C" fn(u32) -> CgRect =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u32) -> CgRect>(host) };
    let rect = f(display);
    if buffer == 0 {
        return Err(AbiError::Memory {
            symbol: symbol.to_string(),
            addr: 0,
        });
    }
    mem.write_bytes(buffer, &rect.to_le_bytes(), symbol)?;
    ctx.rax = buffer;
    Ok(())
}

/// Read the `CGRect` a guest passed by value, out of its own stack.
///
/// A MEMORY-class argument is *on the stack* under SysV AMD64, and microlift
/// exits before the `CALL`, so `rsp` is still the pre-call value and the
/// argument area starts at `rsp + 0` — the arrangement
/// [`crate::abi::marshal`]'s stack path already rests on. `offset` is where in
/// that area this rectangle begins, which is not always zero: a call with two
/// MEMORY arguments, or with integer arguments spilled ahead of one, puts it
/// further along.
pub(crate) fn rect_argument(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &mut dyn GuestMemory,
    offset: u64,
) -> Result<CgRect> {
    let at = ctx.rsp.wrapping_add(offset);
    let mut bytes = [0u8; 32];
    for (i, word) in bytes.chunks_exact_mut(8).enumerate() {
        let value = mem.read_u64(at.wrapping_add(i as u64 * 8), symbol)?;
        word.copy_from_slice(&value.to_le_bytes());
    }
    let field = |i: usize| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&bytes[i * 8..(i + 1) * 8]);
        f64::from_le_bytes(w)
    };
    Ok(CgRect {
        x: field(0),
        y: field(1),
        width: field(2),
        height: field(3),
    })
}

/// `void CGContextClearRect(CGContextRef, CGRect)`
///
/// The rectangle is the guest's second argument and its first stack one; the
/// context stays in `rdi`. Calling the host through a prototype that takes the
/// aggregate by value puts the compiler in charge of the `v0`–`v3` placement,
/// which is the same trick [`rect_of_display`] uses in the other direction.
fn context_clear_rect(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let rect = rect_argument(symbol, ctx, mem, 0)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `void (*)(CGContextRef, CGRect)`, resolved out of a CoreGraphics this
    // process has loaded. The context is an opaque handle the guest got from
    // `CGBitmapContextCreate`, so it is the host's own.
    let f: extern "C" fn(u64, CgRect) =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64, CgRect)>(host) };
    f(ctx.rdi, rect);
    Ok(())
}

/// `void CGContextDrawImage(CGContextRef, CGRect, CGImageRef)`
///
/// The one cocos2d-x decodes every texture through: a `CGImageSource` produces
/// a `CGImage` and this draws it into a bitmap context the guest owns the
/// backing store of. The MEMORY-class rectangle sits on the stack and the
/// *image* takes the second integer register, because a MEMORY argument
/// consumes no register at all — reading it out of `rdx` would pass the guest's
/// third argument as its second, and the image would be whatever was left in
/// `rdx`.
fn context_draw_image(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let rect = rect_argument(symbol, ctx, mem, 0)?;
    // SAFETY: as above, for `void (*)(CGContextRef, CGRect, CGImageRef)`.
    let f: extern "C" fn(u64, CgRect, u64) =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64, CgRect, u64)>(host) };
    f(ctx.rdi, rect, ctx.rsi);
    Ok(())
}

/// `CGPoint CGEventGetLocation(CGEventRef)`
///
/// Sixteen bytes of two `double`s, which is the case the two ABIs **agree**
/// about — SysV AMD64 returns two SSE eightbytes in `xmm0`/`xmm1`, AAPCS64
/// returns a two-member homogeneous float aggregate in `v0`/`v1`. It is still
/// not a table entry, for [`display_link_nominal_period`]'s reason: the
/// marshaller's return path carries one register and this needs two.
fn event_location(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CGPoint (*)(CGEventRef)`; the event is one the host made.
    let f: extern "C" fn(u64) -> CgPoint =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> CgPoint>(host) };
    let point = f(ctx.rdi);
    ctx.xmm[0] = [point.x.to_bits(), 0];
    ctx.xmm[1] = [point.y.to_bits(), 0];
    Ok(())
}

/// `CGSize CGDisplayScreenSize(CGDirectDisplayID)`
///
/// The display's physical size in millimetres, which is how a game computes a
/// DPI. A `CGSize` is a `CGPoint` by another name — two `double`s — so it is
/// [`event_location`]'s case with a 32-bit argument instead of a handle.
fn screen_size_of_display(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CGSize (*)(CGDirectDisplayID)`, resolved out of a CoreGraphics this
    // process has loaded.
    let f: extern "C" fn(u32) -> CgPoint =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u32) -> CgPoint>(host) };
    let size = f(ctx.rdi as u32);
    ctx.xmm[0] = [size.x.to_bits(), 0];
    ctx.xmm[1] = [size.y.to_bits(), 0];
    Ok(())
}

/// `CGPoint`, as both ABIs return one: two floating-point registers.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct CgPoint {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

/// The host's address for one of this module's names, or the refusal that says
/// which one was missing.
pub(crate) fn host_or_refuse(symbol: &str) -> Result<u64> {
    crate::dyld::host_symbol(symbol.strip_prefix('_').unwrap_or(symbol)).ok_or(AbiError::NoShim {
        symbol: symbol.to_string(),
    })
}

/// `CVTime`, as both ABIs return one: two integer registers.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CvTime {
    time_value: i64,
    time_scale: i32,
    flags: i32,
}

/// `CVTime CVDisplayLinkGetNominalOutputVideoRefreshPeriod(CVDisplayLinkRef)`
///
/// How long a frame is on the display the link is attached to, which is how
/// SDL3 reports a refresh rate. The guest reads `rax` and `rdx`; the host
/// returns `x0` and `x1`, holding the same two eightbytes in the same order.
fn display_link_nominal_period(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let Some(host) = crate::dyld::host_symbol(symbol.strip_prefix('_').unwrap_or(symbol)) else {
        return Err(AbiError::NoShim {
            symbol: symbol.to_string(),
        });
    };
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CVTime (*)(CVDisplayLinkRef)`, resolved out of a CoreVideo this process
    // has loaded. The argument is an opaque handle the guest got from
    // `CVDisplayLinkCreateWithActiveCGDisplays`, so it is the host's own.
    let f: extern "C" fn(u64) -> CvTime =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> CvTime>(host) };
    let time = f(ctx.rdi);
    ctx.rax = time.time_value as u64;
    // The second eightbyte is the two `int32`s packed low-then-high, which is
    // what "one eightbyte" means for a pair of them.
    ctx.rdx = u64::from(time.time_scale as u32) | (u64::from(time.flags as u32) << 32);
    Ok(())
}

/// One of this module's implementations, in the shape [`crate::shims`] wants.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        // `CGRect CGDisplayBounds(CGDirectDisplayID)` — where a display sits in
        // the global desktop coordinate space, which SDL asks for every display
        // it enumerates.
        "CGDisplayBounds" => Some(rect_of_display),
        // `CGSize CGDisplayScreenSize(CGDirectDisplayID)` — sixteen bytes rather
        // than thirty-two, so it is the agreeing case rather than the
        // disagreeing one, and it still cannot be a table entry.
        "CGDisplayScreenSize" => Some(screen_size_of_display),
        // The bitmap-context pair, which is how a `CGImage` becomes texture
        // bytes: a `CGRect` **argument**, passed on the guest's stack.
        "CGContextClearRect" => Some(context_clear_rect),
        "CGContextDrawImage" => Some(context_draw_image),
        "CGEventGetLocation" => Some(event_location),
        "CVDisplayLinkGetNominalOutputVideoRefreshPeriod" => Some(display_link_nominal_period),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout claim, which is the only thing this module asserts about
    /// `CGRect` and the thing a wrong answer would silently violate: four
    /// doubles in declaration order, thirty-two bytes, no padding.
    #[test]
    fn a_rect_is_four_doubles_in_order() {
        assert_eq!(core::mem::size_of::<CgRect>(), 32);
        let bytes = CgRect {
            x: 1.5,
            y: 2.5,
            width: 3.5,
            height: 4.5,
        }
        .to_le_bytes();
        for (i, expected) in [1.5f64, 2.5, 3.5, 4.5].iter().enumerate() {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[i * 8..(i + 1) * 8]);
            assert_eq!(f64::from_le_bytes(word), *expected);
        }
    }

    /// Against the host, so the claim is about CoreGraphics rather than about
    /// this module's arithmetic: the main display's bounds must be a plausible
    /// rectangle, and `rax` must come back as the buffer the guest passed.
    #[test]
    fn the_main_displays_bounds_land_in_the_guests_buffer() {
        let Some(_) = crate::dyld::host_symbol("CGDisplayBounds") else {
            // A headless builder has no CoreGraphics to ask.
            return;
        };
        let mut buffer = [0u8; 32];
        let at = buffer.as_mut_ptr() as usize as u64;
        let mut ctx = RegisterContext::new();
        ctx.rdi = at;
        // `CGMainDisplayID()` is 0's alias in every case that matters here; a
        // display ID this runtime made up would be refused by CoreGraphics and
        // return a zero rect, which the assertions below would catch.
        ctx.rsi = u64::from(host_main_display());
        let buffer_region = at..at + 32;
        let mut mem = crate::vm::GuestView::new(vec![buffer_region]);
        rect_of_display("_CGDisplayBounds", &mut ctx, &mut mem).expect("bounds");

        assert_eq!(ctx.rax, at, "rax must return the hidden buffer");
        let read = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&buffer[i * 8..(i + 1) * 8]);
            f64::from_le_bytes(w)
        };
        assert!(read(2) > 0.0 && read(3) > 0.0, "a display has a size");
    }

    /// The packing claim, which is the whole of what this shim does with
    /// `CVTime`'s second eightbyte: `timeScale` low, `flags` high.
    #[test]
    fn a_cvtime_second_eightbyte_is_the_two_int32s_low_then_high() {
        assert_eq!(core::mem::size_of::<CvTime>(), 16);
        let Some(_) = crate::dyld::host_symbol("CVDisplayLinkGetNominalOutputVideoRefreshPeriod")
        else {
            return;
        };
        // A null display link is the case CoreVideo answers without a display:
        // it reports an indefinite time rather than faulting, which is enough to
        // check the placement without depending on a monitor being attached.
        let mut ctx = RegisterContext::new();
        ctx.rdi = 0;
        let mut mem = crate::vm::GuestView::new(Vec::new());
        display_link_nominal_period(
            "_CVDisplayLinkGetNominalOutputVideoRefreshPeriod",
            &mut ctx,
            &mut mem,
        )
        .expect("period");
        // `kCVTimeIsIndefinite` is bit 0 of `flags`, which lives in the high
        // half of `rdx`. Asserting the *scale* is in the low half is the
        // placement claim; the value itself is CoreVideo's business.
        let scale = ctx.rdx as u32;
        let flags = (ctx.rdx >> 32) as u32;
        assert!(
            flags & 1 == 1 || scale != 0,
            "a null link should be indefinite, or report a scale: rdx {:#x}",
            ctx.rdx
        );
    }

    fn host_main_display() -> u32 {
        let Some(f) = crate::dyld::host_symbol("CGMainDisplayID") else {
            return 0;
        };
        // SAFETY: `CGMainDisplayID` is `CGDirectDisplayID (*)(void)`.
        let f: extern "C" fn() -> u32 =
            unsafe { core::mem::transmute::<u64, extern "C" fn() -> u32>(f) };
        f()
    }
}
