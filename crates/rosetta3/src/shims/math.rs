//! Math helpers whose arguments or return values the signature table cannot
//! name.
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
//! `__divdc3` is compiler-rt's complex-double division helper. A complex
//! double is two scalar doubles, so its arguments and return value are split
//! across vector registers even though the signature table only describes
//! scalar returns. It is implemented here rather than forwarded to the host:
//! the host may not export compiler-rt's private spelling, and forwarding
//! would leave the host with the guest's register layout.
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

/// `double _Complex __divdc3(double a, double b, double c, double d)`.
///
/// The guest calls this as `(a + ib) / (c + id)`. Scaling the denominator by
/// its largest component keeps `c*c + d*d` from overflowing for ordinary
/// finite inputs. The two complex values are passed as four consecutive
/// doubles in `xmm0` through `xmm3`, and the result is returned in `xmm0` and
/// `xmm1`.
pub fn divdc3(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let (a, b, c, d) = (
        f64::from_bits(ctx.xmm[0][0]),
        f64::from_bits(ctx.xmm[1][0]),
        f64::from_bits(ctx.xmm[2][0]),
        f64::from_bits(ctx.xmm[3][0]),
    );

    let scale = c.abs().max(d.abs());
    let (cr, dr) = if scale.is_infinite() {
        // inf / inf is NaN, but an infinite divisor is represented by its
        // direction in the limiting quotient: an infinite component is ±1
        // and a finite component is zero.
        (c.signum(), d.signum())
    } else if scale == 0.0 {
        // Preserve the signed zero information for the correction below.
        (c, d)
    } else {
        (c / scale, d / scale)
    };

    let (real, imag) = if scale == 0.0 {
        // A non-zero numerator divided by zero has an infinite magnitude.
        // This also avoids manufacturing NaNs from inf * 0 for the common
        // real-only cases.
        let real = if a == 0.0 && b == 0.0 {
            f64::NAN
        } else {
            (a * c.signum() + b * d.signum()) * f64::INFINITY
        };
        let imag = if a == 0.0 && b == 0.0 {
            f64::NAN
        } else {
            (b * c.signum() - a * d.signum()) * f64::INFINITY
        };
        (real, imag)
    } else {
        let denom = cr * cr + dr * dr;
        (
            (a * cr + b * dr) / denom / scale,
            (b * cr - a * dr) / denom / scale,
        )
    };

    ctx.xmm[0] = [real.to_bits(), 0];
    ctx.xmm[1] = [imag.to_bits(), 0];
    Ok(())
}

/// `float _Complex __mulsc3(float a, float b, float c, float d)`.
///
/// [`divdc3`]'s sibling one size down and in the other operation, and its
/// register story is the one interesting thing about it. A `float _Complex` is
/// **eight** bytes, so SysV AMD64 makes it a single SSE eightbyte: both
/// arguments of a complex value share one `xmm` register — real in the low half,
/// imaginary in the high — and the result comes back packed the same way in
/// `xmm0`. AAPCS64 sees a two-member homogeneous float aggregate and uses `s0`
/// and `s1`. That is the `__sincosf_stret` disagreement, reached from
/// compiler-rt instead of libm.
///
/// The multiplication itself is the C standard's Annex G recipe: the naive
/// product first, then the recovery pass that turns the `inf * 0` NaNs an
/// infinite operand produces back into the infinity the limit says it is.
/// Skipping that is what makes a hand-rolled complex multiply wrong only for the
/// inputs nobody tests with.
pub fn mulsc3(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let half = |reg: usize, high: bool| {
        let word = ctx.xmm[reg][0];
        f32::from_bits(if high {
            (word >> 32) as u32
        } else {
            word as u32
        })
    };
    let (a, b) = (half(0, false), half(0, true));
    let (c, d) = (half(1, false), half(1, true));

    let (ac, bd, ad, bc) = (a * c, b * d, a * d, b * c);
    let mut real = ac - bd;
    let mut imag = ad + bc;

    if real.is_nan() && imag.is_nan() {
        // One operand was infinite and the other had a zero component, so the
        // naive product manufactured `inf * 0`. Annex G's recovery: treat the
        // infinity as a direction, every NaN component of the *other* operand
        // as a signed zero, and multiply again.
        let mut recovered = false;
        let (mut a, mut b, mut c, mut d) = (a, b, c, d);
        /// An infinite component becomes a signed one and a finite one a signed
        /// zero: the infinity's *direction*, which is what the limit depends on.
        fn direction(x: f32) -> f32 {
            let magnitude: f32 = if x.is_infinite() { 1.0 } else { 0.0 };
            magnitude.copysign(x)
        }
        /// A NaN in the other operand carried no information, so it becomes a
        /// signed zero and stops poisoning the product.
        fn tame(x: f32) -> f32 {
            match x.is_nan() {
                true => 0.0f32.copysign(x),
                false => x,
            }
        }
        if a.is_infinite() || b.is_infinite() {
            a = direction(a);
            b = direction(b);
            c = tame(c);
            d = tame(d);
            recovered = true;
        }
        if c.is_infinite() || d.is_infinite() {
            c = direction(c);
            d = direction(d);
            a = tame(a);
            b = tame(b);
            recovered = true;
        }
        if !recovered
            && (ac.is_infinite() || bd.is_infinite() || ad.is_infinite() || bc.is_infinite())
        {
            // The operands are finite but their products overflowed, which is
            // an infinite result rather than an indeterminate one.
            a = tame(a);
            b = tame(b);
            c = tame(c);
            d = tame(d);
            recovered = true;
        }
        if recovered {
            real = f32::INFINITY * (a * c - b * d);
            imag = f32::INFINITY * (a * d + b * c);
        }
    }

    ctx.xmm[0] = [
        u64::from(real.to_bits()) | (u64::from(imag.to_bits()) << 32),
        0,
    ];
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
        "__divdc3" => Some(divdc3),
        "__mulsc3" => Some(mulsc3),
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

    #[test]
    fn complex_division_uses_four_inputs_and_two_outputs() {
        let mut ctx = RegisterContext::new();
        for (slot, value) in [2.0_f64, 3.0, 4.0, -5.0].into_iter().enumerate() {
            ctx.xmm[slot] = [value.to_bits(), 0];
        }
        divdc3("___divdc3", &mut ctx, &mut NoMemory).unwrap();

        assert!((f64::from_bits(ctx.xmm[0][0]) + 7.0 / 41.0).abs() < 1e-15);
        assert!((f64::from_bits(ctx.xmm[1][0]) - 22.0 / 41.0).abs() < 1e-15);
        assert_eq!(ctx.xmm[0][1], 0);
        assert_eq!(ctx.xmm[1][1], 0);
    }

    #[test]
    fn complex_division_scales_a_large_denominator() {
        let mut ctx = RegisterContext::new();
        for (slot, value) in [1.0_f64, 1.0, 1e200, 1e200].into_iter().enumerate() {
            ctx.xmm[slot] = [value.to_bits(), 0];
        }
        divdc3("___divdc3", &mut ctx, &mut NoMemory).unwrap();

        let real = f64::from_bits(ctx.xmm[0][0]);
        let imag = f64::from_bits(ctx.xmm[1][0]);
        assert!((real - 1e-200).abs() < 1e-215);
        assert!(imag.abs() < 1e-215);
    }

    /// The Mach-O spelling is what reaches [`shim_for`] after one underscore is
    /// stripped, and getting that wrong means the shim silently never fires.
    #[test]
    fn the_mach_o_spelling_resolves() {
        for symbol in ["___divdc3", "___sincosf_stret", "___sincos_stret"] {
            let name = symbol.strip_prefix('_').unwrap();
            assert!(shim_for(name).is_some(), "{symbol} is not claimed");
            assert!(crate::shims::is_shimmed(symbol), "{symbol} may be inlined");
        }
    }
}
