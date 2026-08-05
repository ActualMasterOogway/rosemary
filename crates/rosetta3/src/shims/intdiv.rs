//! `div`, `ldiv`, `lldiv` and `imaxdiv`: quotient and remainder, returned as a
//! struct by value.
//!
//! ## Why these are here rather than in the table
//!
//! The same reason as [`math`](super::math)'s `sincos` pair, one class of
//! register over: a struct by value is the one thing [`crate::abi::table`]
//! refuses to describe. `ldiv_t` is `{ long quot; long rem; }`, which SysV
//! AMD64 classifies as two INTEGER eightbytes and returns in `rax` and `rdx`,
//! while AAPCS64 returns in `x0` and `x1` — so forwarding the call and copying
//! `x0` back would deliver the quotient and lose the remainder. `div_t` is two
//! `int`s, eight bytes, so it is a *single* eightbyte and arrives entirely in
//! `rax`: quotient in the low half, remainder in the high one.
//!
//! And as with `sincos`, the shim does not forward the call at all. Integer
//! division truncated toward zero is two operators; computing it here removes
//! both the aggregate-return question and any question of what the host's own
//! `ldiv` does.
//!
//! ## Division by zero, and the one overflow
//!
//! C leaves both undefined, and on the guest's own hardware both are the same
//! event: `idiv` raises `#DE` for a zero divisor *and* for `LONG_MIN / -1`,
//! whose quotient is not representable. There is nothing to compute in either
//! case and no way to raise the guest's own trap from here, so both are refused
//! by name — which says what happened instead of returning a number nobody can
//! justify.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

/// One of this module's implementations, in the shape [`crate::shims`] wants.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "div" => Some(div),
        // `lldiv` and `imaxdiv` are `ldiv` under other names on an LP64
        // platform: `long`, `long long` and `intmax_t` are all 64 bits, and all
        // three result structs are two of them.
        "ldiv" | "lldiv" | "imaxdiv" => Some(ldiv),
        _ => None,
    }
}

/// `div_t div(int numer, int denom)` — eight bytes, so one eightbyte, so `rax`.
fn div(symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let (numer, denom) = (ctx.rdi as i32, ctx.rsi as i32);
    let (quot, rem) = match (numer.checked_div(denom), numer.checked_rem(denom)) {
        (Some(q), Some(r)) => (q, r),
        _ => return Err(would_have_trapped(symbol)),
    };
    ctx.rax = u64::from(quot as u32) | (u64::from(rem as u32) << 32);
    Ok(())
}

/// `ldiv_t ldiv(long numer, long denom)` — sixteen bytes, so `rax` and `rdx`.
fn ldiv(symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let (numer, denom) = (ctx.rdi as i64, ctx.rsi as i64);
    let (quot, rem) = match (numer.checked_div(denom), numer.checked_rem(denom)) {
        (Some(q), Some(r)) => (q, r),
        _ => return Err(would_have_trapped(symbol)),
    };
    ctx.rax = quot as u64;
    ctx.rdx = rem as u64;
    Ok(())
}

/// The refusal for the two argument pairs `idiv` itself would have faulted on.
fn would_have_trapped(symbol: &str) -> AbiError {
    AbiError::Refused {
        symbol: symbol.to_string(),
        reason: "a divisor of zero, or a quotient too large to represent — the \
                 division the guest asked for is the one its own hardware traps on",
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

    fn call(shim: Shim, numer: u64, denom: u64) -> (u64, u64) {
        let mut ctx = RegisterContext::new();
        ctx.rdi = numer;
        ctx.rsi = denom;
        shim("div", &mut ctx, &mut NoMemory).expect("divides");
        (ctx.rax, ctx.rdx)
    }

    #[test]
    fn a_div_t_arrives_whole_in_rax() {
        // 17 / 5 = 3 remainder 2, packed low-half-first.
        let (rax, _) = call(div, 17, 5);
        assert_eq!(rax & 0xffff_ffff, 3);
        assert_eq!(rax >> 32, 2);
    }

    #[test]
    fn division_truncates_toward_zero_as_c_says() {
        // Not floor division: -7 / 2 is -3 remainder -1, and the sign of the
        // remainder follows the numerator.
        let (rax, _) = call(div, (-7i32) as u32 as u64, 2);
        assert_eq!(rax as u32 as i32, -3);
        assert_eq!((rax >> 32) as u32 as i32, -1);

        let (quot, rem) = call(ldiv, (-7i64) as u64, 2);
        assert_eq!(quot as i64, -3);
        assert_eq!(rem as i64, -1);
    }

    #[test]
    fn an_ldiv_t_is_two_registers() {
        let (quot, rem) = call(ldiv, 1_000_000_000_007, 1_000_000);
        assert_eq!(quot, 1_000_000);
        assert_eq!(rem, 7);
    }

    #[test]
    fn a_division_the_hardware_would_trap_on_is_refused_rather_than_answered() {
        let mut ctx = RegisterContext::new();
        ctx.rdi = 1;
        ctx.rsi = 0;
        let before = ctx.rax;
        assert!(ldiv("_ldiv", &mut ctx, &mut NoMemory).is_err());
        assert_eq!(ctx.rax, before, "a refusal leaves the context untouched");

        // The other `#DE`: the quotient of the most negative value and -1 is
        // one past the top of the range.
        ctx.rdi = i64::MIN as u64;
        ctx.rsi = (-1i64) as u64;
        assert!(ldiv("_ldiv", &mut ctx, &mut NoMemory).is_err());
    }

    #[test]
    fn the_three_lp64_spellings_are_one_implementation() {
        for name in ["ldiv", "lldiv", "imaxdiv"] {
            assert!(shim_for(name).is_some(), "{name}");
        }
        assert!(shim_for("div").is_some());
        assert!(shim_for("fdiv").is_none());
    }
}
