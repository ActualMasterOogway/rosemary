//! AppKit's plain C functions that take an `NSRect` **by value**.
//!
//! Almost all of AppKit is Objective-C, and a message carrying an `NSRect` is
//! [`crate::objc::dispatch`]'s problem — it has the method's own type encoding
//! to work from. A handful of entry points are ordinary C functions instead,
//! and those have nothing to read a shape out of, so they are here for
//! [`super::coregraphics`]'s reason: SysV AMD64 classifies thirty-two bytes as
//! MEMORY and puts them on the caller's stack, AAPCS64 sees a four-member
//! homogeneous float aggregate and puts them in `v0`–`v3`, and no signature can
//! say that.
//!
//! `NSRect` *is* `CGRect` on 64-bit macOS, and `NSPoint` is `CGPoint`, so the
//! two structures come from [`super::coregraphics`] rather than being declared
//! a second time. What is different here is the argument *order*: both of these
//! have the aggregate in front of a scalar, so the scalar is in `rdi` — the
//! first integer register, not the second — because a MEMORY-class argument
//! consumes no register at all.

use microlift::arch::x86_64::context::RegisterContext;

use super::coregraphics::{CgPoint, CgRect, host_or_refuse, rect_argument};
use crate::abi::{GuestMemory, Result};

/// `BOOL NSMouseInRect(NSPoint aPoint, NSRect aRect, BOOL isFlipped)`
///
/// Whether a point is inside a rectangle, with the flipped-coordinates rule
/// deciding which edges count. SDL asks it about the cursor on every mouse
/// event.
///
/// The guest's registers are the whole reason this is not a table entry: the
/// point is two SSE eightbytes in `xmm0`/`xmm1`, the rectangle is on the stack,
/// and `isFlipped` — the *third* argument — is in `rdi`, the first integer
/// register, because neither argument before it took one.
fn mouse_in_rect(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let point = CgPoint {
        x: f64::from_bits(ctx.xmm[0][0]),
        y: f64::from_bits(ctx.xmm[1][0]),
    };
    let rect = rect_argument(symbol, ctx, mem, 0)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `BOOL (*)(NSPoint, NSRect, BOOL)`, resolved out of an AppKit this process
    // has loaded — the guest's own window brought it in.
    let f: extern "C" fn(CgPoint, CgRect, u8) -> u8 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(CgPoint, CgRect, u8) -> u8>(host) };
    // A `BOOL` is a signed char, so only the low byte of `rdi` carries it and
    // only the low byte of `rax` may be written back — the rest of `rax` is
    // whatever the guest's caller will ignore, and zeroing it is the honest
    // answer for a byte-wide return.
    ctx.rax = u64::from(f(point, rect, ctx.rdi as u8));
    Ok(())
}

/// `void NSRectFill(NSRect aRect)`
///
/// Fills the rectangle in the current graphics context with the current colour.
/// One argument, entirely on the guest's stack, and nothing comes back.
fn rect_fill(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    let rect = rect_argument(symbol, ctx, mem, 0)?;
    // SAFETY: as above, for `void (*)(NSRect)`.
    let f: extern "C" fn(CgRect) =
        unsafe { core::mem::transmute::<u64, extern "C" fn(CgRect)>(host) };
    f(rect);
    Ok(())
}

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<super::coregraphics::Shim> {
    match name {
        "NSMouseInRect" => Some(mouse_in_rect),
        "NSRectFill" => Some(rect_fill),
        _ => None,
    }
}
