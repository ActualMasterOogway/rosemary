//! The four libSystem math entry points whose return ABI is Darwin's own.
//!
//! Each is a Darwin ABI rather than a missing function: `__sincos_stret` hands back
//! two doubles in the vector registers, and the two `_Complex` helpers hand back a
//! complex value, which no glibc entry point does. Both the runtime above (which
//! intercepts these for an emulated guest and marshals the registers itself) and the
//! native backend (whose guest calls the address in the import slot directly) need the
//! same arithmetic, so the arithmetic lives here as plain functions and the register
//! layout is expressed once, in the `extern "C"` wrappers below.
//!
//! The wrappers are correct on both host architectures without a `cfg`: SysV AMD64
//! returns a two-double aggregate in `xmm0`/`xmm1` and a two-float one packed in
//! `xmm0`, and AAPCS64 returns each as a homogeneous float aggregate in `v0`/`v1` -
//! which is what Darwin's `__double2`, `__float2` and `_Complex` returns already are.

/// `__float2`, and `float _Complex`: two floats, one eightbyte.
#[repr(C)]
pub struct Float2 {
    pub a: f32,
    pub b: f32,
}

/// `__double2`, and `double _Complex`: two doubles, two eightbytes.
#[repr(C)]
pub struct Double2 {
    pub a: f64,
    pub b: f64,
}

/// `__sincosf_stret`'s arithmetic: sine and cosine of one argument.
pub fn sincosf(x: f32) -> (f32, f32) {
    (x.sin(), x.cos())
}

/// `__sincos_stret`'s arithmetic.
pub fn sincos(x: f64) -> (f64, f64) {
    (x.sin(), x.cos())
}

/// `double _Complex __divdc3(double a, double b, double c, double d)`.
pub fn divdc3(a: f64, b: f64, c: f64, d: f64) -> (f64, f64) {
    let scale = c.abs().max(d.abs());
    let (cr, dr) = if scale.is_infinite() {
        // inf / inf is NaN, but an infinite divisor is represented by its
        // direction in the limiting quotient.
        (c.signum(), d.signum())
    } else if scale == 0.0 {
        // Preserve the signed zero information for the correction below.
        (c, d)
    } else {
        (c / scale, d / scale)
    };

    if scale == 0.0 {
        // A non-zero numerator divided by zero has an infinite magnitude.
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
    }
}

/// `float _Complex __mulsc3(float a, float b, float c, float d)`.
///
/// [`divdc3`]'s sibling one size down and in the other operation.
pub fn mulsc3(a: f32, b: f32, c: f32, d: f32) -> (f32, f32) {
    let (ac, bd, ad, bc) = (a * c, b * d, a * d, b * c);
    let mut real = ac - bd;
    let mut imag = ad + bc;

    if real.is_nan() && imag.is_nan() {
        // One operand was infinite and the other had a zero component, so the
        // naive product manufactured `inf * 0`. Annex G's recovery.
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

    (real, imag)
}

extern "C" fn sincosf_stret_abi(x: f32) -> Float2 {
    let (a, b) = sincosf(x);
    Float2 { a, b }
}

extern "C" fn sincos_stret_abi(x: f64) -> Double2 {
    let (a, b) = sincos(x);
    Double2 { a, b }
}

extern "C" fn divdc3_abi(a: f64, b: f64, c: f64, d: f64) -> Double2 {
    let (a, b) = divdc3(a, b, c, d);
    Double2 { a, b }
}

extern "C" fn mulsc3_abi(a: f32, b: f32, c: f32, d: f32) -> Float2 {
    let (a, b) = mulsc3(a, b, c, d);
    Float2 { a, b }
}

/// The address to publish for each name in [`crate::SHIMMED_ABOVE`].
///
/// An emulated guest never arrives here - its import is intercepted before the
/// address is called, and the slot only has to be non-null - but a native guest
/// calls exactly this.
pub fn symbol(name: &str) -> Option<*const ()> {
    let f = match name {
        "__sincosf_stret" => sincosf_stret_abi as *const (),
        "__sincos_stret" => sincos_stret_abi as *const (),
        "__divdc3" => divdc3_abi as *const (),
        "__mulsc3" => mulsc3_abi as *const (),
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sincos_agrees_with_the_two_calls_it_stands_for() {
        let (s, c) = sincos(0.5);
        assert_eq!((s, c), (0.5f64.sin(), 0.5f64.cos()));
    }

    /// The wrappers exist for their register layout, so the test that matters is
    /// that a call through the published address returns both halves.
    #[test]
    fn the_published_address_returns_both_halves() {
        let f = symbol("__sincos_stret").expect("a published name");
        // SAFETY: `symbol` returned `sincos_stret_abi`, whose signature this is.
        let f: extern "C" fn(f64) -> Double2 = unsafe { core::mem::transmute(f) };
        let got = f(0.5);
        assert_eq!((got.a, got.b), (0.5f64.sin(), 0.5f64.cos()));
    }

    #[test]
    fn a_complex_quotient_by_a_real_divisor_is_componentwise() {
        let (re, im) = divdc3(4.0, 6.0, 2.0, 0.0);
        assert_eq!((re, im), (2.0, 3.0));
    }

    #[test]
    fn i_times_i_is_minus_one() {
        let (re, im) = mulsc3(0.0, 1.0, 0.0, 1.0);
        assert_eq!((re, im), (-1.0, 0.0));
    }

    #[test]
    fn every_shimmed_above_name_has_an_address() {
        for name in crate::SHIMMED_ABOVE {
            assert!(symbol(name).is_some(), "{name} has no implementation");
        }
    }
}
