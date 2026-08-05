//! CoreGraphics entry points whose *return value* the signature table cannot
//! name.
//!
//! [`crate::abi::table`] describes everything else in the display family — they
//! are display IDs, mode handles and sizes, all scalars and pointers, and the
//! host's implementation is the right one because the displays it is asked
//! about are this machine's. `CGDisplayBounds` is the exception, for the same
//! reason [`crate::shims::math`]'s pair are: it returns a **struct by value**,
//! and the two architectures do not agree on how.
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
struct CgRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
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

/// One of this module's implementations, in the shape [`crate::shims`] wants.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        // `CGRect CGDisplayBounds(CGDirectDisplayID)` — where a display sits in
        // the global desktop coordinate space, which SDL asks for every display
        // it enumerates.
        "CGDisplayBounds" => Some(rect_of_display),
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
