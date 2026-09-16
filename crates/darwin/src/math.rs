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

/// `double __exp10(double)` and its float twin, which are Darwin's spelling of
/// ten to the power of x.
///
/// glibc has `exp10` and `exp10f` and *not* these names, so the import goes
/// unresolved on this host - and it is a plain function with an ordinary ABI,
/// which is the only reason it belongs beside the four above.
///
/// `powf(10, x)` rather than `exp(x * ln 10)`: the second is a rounding error
/// wide of the first at every integral exponent, and an integral exponent is
/// what a caller converting decibels or building a decimal scale passes.
extern "C" fn exp10_abi(x: f64) -> f64 {
    10f64.powf(x)
}

extern "C" fn exp10f_abi(x: f32) -> f32 {
    10f32.powf(x)
}

/// `<math.h>`'s classification macros, which on Darwin are calls.
///
/// `isnan(x)`, `isfinite(x)` and their siblings expand to a call to one of
/// these, so a program doing nothing more exotic than checking a float imports
/// them - and glibc spells only some of the family, under names of its own
/// (`__finitef` for `__isfinitef`, nothing at all for the `d` suffix). What the
/// missing ones cost is not a link error: the slot stays null and the first
/// `isfinite()` the program reaches calls zero.
///
/// The `f` forms take a `float` and the `d` forms a `double`; each answers an
/// `int`, as Darwin's `math.h` declares.
mod classify {
    /// Darwin's `FP_*`, which are **not** glibc's: `FP_NAN` is 1 here and 0
    /// there, and the two orders disagree the rest of the way down. A guest
    /// given the host's numbering reads "NaN" as "normal" and never says why.
    const NAN: i32 = 1;
    const INFINITE: i32 = 2;
    const ZERO: i32 = 3;
    const NORMAL: i32 = 4;
    const SUBNORMAL: i32 = 5;

    pub extern "C" fn isnanf(x: f32) -> i32 {
        x.is_nan() as i32
    }
    pub extern "C" fn isnand(x: f64) -> i32 {
        x.is_nan() as i32
    }
    pub extern "C" fn isinff(x: f32) -> i32 {
        x.is_infinite() as i32
    }
    pub extern "C" fn isinfd(x: f64) -> i32 {
        x.is_infinite() as i32
    }
    pub extern "C" fn isfinitef(x: f32) -> i32 {
        x.is_finite() as i32
    }
    pub extern "C" fn isfinited(x: f64) -> i32 {
        x.is_finite() as i32
    }
    pub extern "C" fn isnormalf(x: f32) -> i32 {
        x.is_normal() as i32
    }
    pub extern "C" fn isnormald(x: f64) -> i32 {
        x.is_normal() as i32
    }
    pub extern "C" fn signbitf(x: f32) -> i32 {
        x.is_sign_negative() as i32
    }
    pub extern "C" fn signbitd(x: f64) -> i32 {
        x.is_sign_negative() as i32
    }
    pub extern "C" fn fpclassifyf(x: f32) -> i32 {
        match x.classify() {
            std::num::FpCategory::Nan => NAN,
            std::num::FpCategory::Infinite => INFINITE,
            std::num::FpCategory::Zero => ZERO,
            std::num::FpCategory::Subnormal => SUBNORMAL,
            std::num::FpCategory::Normal => NORMAL,
        }
    }
    pub extern "C" fn fpclassifyd(x: f64) -> i32 {
        match x.classify() {
            std::num::FpCategory::Nan => NAN,
            std::num::FpCategory::Infinite => INFINITE,
            std::num::FpCategory::Zero => ZERO,
            std::num::FpCategory::Subnormal => SUBNORMAL,
            std::num::FpCategory::Normal => NORMAL,
        }
    }
}

/// The names this module publishes that are plain functions rather than a
/// Darwin return ABI: nothing above intercepts them, so a guest calls exactly
/// these.
///
/// Published only where the host defines no such name, so that on Darwin each
/// stays libSystem's. See [`SHADOWED`] for the ones that cannot be left to the
/// host even when it has them.
pub const PLAIN: &[&str] = &[
    "__exp10",
    "__exp10f",
    // The half of the classification family glibc has no spelling for at all.
    "__isnand",
    "__isinfd",
    "__isfinitef",
    "__isfinited",
    "__isnormalf",
    "__isnormald",
    "__signbitd",
];

/// Names the host also defines, but whose answer means something else there.
///
/// `__fpclassifyf` exists under glibc and returns glibc's `FP_*`, so binding a
/// guest to it is worse than leaving it unbound: every classification comes
/// back plausible and wrong. See [`classify`].
#[cfg(not(target_os = "macos"))]
pub const SHADOWED: &[&str] = &["__fpclassifyf", "__fpclassifyd"];

/// On Darwin the host is libSystem, whose answers these reproduce - so there is
/// nothing to shadow.
#[cfg(target_os = "macos")]
pub const SHADOWED: &[&str] = &[];

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
        "__exp10" => exp10_abi as *const (),
        "__exp10f" => exp10f_abi as *const (),
        "__isnanf" => classify::isnanf as *const (),
        "__isnand" => classify::isnand as *const (),
        "__isinff" => classify::isinff as *const (),
        "__isinfd" => classify::isinfd as *const (),
        "__isfinitef" => classify::isfinitef as *const (),
        "__isfinited" => classify::isfinited as *const (),
        "__isnormalf" => classify::isnormalf as *const (),
        "__isnormald" => classify::isnormald as *const (),
        "__signbitf" => classify::signbitf as *const (),
        "__signbitd" => classify::signbitd as *const (),
        "__fpclassifyf" => classify::fpclassifyf as *const (),
        "__fpclassifyd" => classify::fpclassifyd as *const (),
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten to an integral power is exact, which is what a caller building a
    /// decimal scale relies on and what `exp(x * ln 10)` would not give.
    #[test]
    fn ten_to_a_whole_power_is_exact() {
        let f = symbol("__exp10").expect("a published name");
        // SAFETY: `symbol` returned `exp10_abi`, whose signature this is.
        let f: extern "C" fn(f64) -> f64 = unsafe { core::mem::transmute(f) };
        assert_eq!(f(0.0), 1.0);
        assert_eq!(f(3.0), 1000.0);
        assert_eq!(f(-2.0), 0.01);
    }

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
