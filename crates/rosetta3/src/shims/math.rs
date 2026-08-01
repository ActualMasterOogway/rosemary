//! The `sincos` pair, whose *return value* the signature table cannot name.
//!
//! ## Why these are here rather than in the table
//!
//! `__sincosf_stret` and `__sincos_stret` are Darwin's fused sine-and-cosine,
//! and clang emits a call to one whenever a program computes both of an angle —
//! which for a game engine is every frame. Both return **two values**, in a
//! struct, and that is the whole difficulty: a struct by value is the one thing
//! [`crate::abi::table`] refuses to describe, because the two architectures
//! classify it differently and a guess returns plausible garbage rather than
//! faulting.
//!
//! Here the difference is concrete and small enough to write out. SysV AMD64
//! classifies `__float2` — two `float`s, eight bytes — as a single SSE
//! eightbyte, so the guest expects `sinf` in `xmm0[31:0]` and `cosf` in
//! `xmm0[63:32]`. AAPCS64 sees a two-member homogeneous float aggregate and
//! returns it in `s0` and `s1`, two separate registers. `__double2` is the same
//! story one size up: `xmm0` and `xmm1` on the guest's side, `d0` and `d1` on
//! the host's.
//!
//! So the shim does not forward the *call*; it computes the two values and
//! places them the way the guest's ABI says. That also removes the question of
//! whether the host even exports these — they are Darwin-specific and this way
//! the answer does not matter.
//!
//! ## Accuracy
//!
//! `sin` and `cos` are the host libm's, called separately. The `_stret` entry
//! points exist to share argument reduction, which is a *performance*
//! guarantee, not an accuracy one: the results are those of `sin(x)` and
//! `cos(x)`, which is what the C library documents them to be.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

/// `__float2 __sincosf_stret(float x)`
///
/// The argument arrives in `xmm0[31:0]` and both results leave in `xmm0`, low
/// half first — one register in, one register out, because eight bytes of
/// floating point is a single SSE eightbyte on the way in and on the way back.
pub fn sincosf_stret(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let x = f32::from_bits(ctx.xmm[0][0] as u32);
    let (sin, cos) = (x.sin(), x.cos());
    ctx.xmm[0] = [
        u64::from(sin.to_bits()) | (u64::from(cos.to_bits()) << 32),
        0,
    ];
    Ok(())
}

/// `__double2 __sincos_stret(double x)`
///
/// Sixteen bytes, so SysV AMD64 splits it across `xmm0` and `xmm1` rather than
/// calling it memory — the boundary that makes this the *last* aggregate return
/// expressible in registers at all.
pub fn sincos_stret(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let x = f64::from_bits(ctx.xmm[0][0]);
    let (sin, cos) = (x.sin(), x.cos());
    ctx.xmm[0] = [sin.to_bits(), 0];
    ctx.xmm[1] = [cos.to_bits(), 0];
    Ok(())
}

/// One of this module's implementations, in the shape [`crate::shims`] wants.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
///
/// Both spellings are here because the Mach-O name carries a leading underscore
/// of its own: the C identifier is `__sincosf_stret`, so the import is
/// `___sincosf_stret` and one strip leaves two.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "__sincosf_stret" => Some(sincosf_stret),
        "__sincos_stret" => Some(sincos_stret),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoMemory;

    impl GuestMemory for NoMemory {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: out.len(),
            })
        }
        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: data.len(),
            })
        }
    }

    /// The placement is the claim, so it is read back the way the *guest* would:
    /// the low eightbyte of `xmm0` as two packed floats.
    #[test]
    fn both_floats_come_back_in_one_register_sine_first() {
        for x in [0.0f32, 0.5, -1.25, 3.0, 1e6] {
            let mut ctx = RegisterContext::new();
            ctx.xmm[0] = [u64::from(x.to_bits()), 0];
            sincosf_stret("___sincosf_stret", &mut ctx, &mut NoMemory).unwrap();

            let packed = ctx.xmm[0][0];
            let sin = f32::from_bits(packed as u32);
            let cos = f32::from_bits((packed >> 32) as u32);
            assert_eq!(sin, x.sin(), "sine belongs in the low half for {x}");
            assert_eq!(cos, x.cos(), "cosine belongs in the high half for {x}");
            assert_eq!(ctx.xmm[0][1], 0, "the high eightbyte is not part of it");
        }
    }

    #[test]
    fn the_double_form_uses_two_registers() {
        for x in [0.0f64, 0.5, -1.25, 3.0, 1e6] {
            let mut ctx = RegisterContext::new();
            ctx.xmm[0] = [x.to_bits(), 0];
            sincos_stret("___sincos_stret", &mut ctx, &mut NoMemory).unwrap();

            assert_eq!(f64::from_bits(ctx.xmm[0][0]), x.sin());
            assert_eq!(f64::from_bits(ctx.xmm[1][0]), x.cos());
        }
    }

    /// The Mach-O spelling is what reaches [`shim_for`] after one underscore is
    /// stripped, and getting that wrong means the shim silently never fires.
    #[test]
    fn the_mach_o_spelling_resolves() {
        for symbol in ["___sincosf_stret", "___sincos_stret"] {
            let name = symbol.strip_prefix('_').unwrap();
            assert!(shim_for(name).is_some(), "{symbol} is not claimed");
            assert!(crate::shims::is_shimmed(symbol), "{symbol} may be inlined");
        }
    }
}
