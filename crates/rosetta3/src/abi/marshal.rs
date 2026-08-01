//! Reading arguments out of a guest register context and making the host call.
//!
//! ## The call itself, and why it is legal
//!
//! There is no libffi here and no assembly. The host is arm64, where AAPCS64
//! allocates the first eight integer/pointer arguments to `x0`–`x7` and the
//! first eight floating-point arguments to `v0`–`v7`, from two *independent*
//! counters. A callee reads only the registers its own prototype declares and
//! ignores the rest.
//!
//! So one over-wide prototype — eight `u64` followed by eight `f64` — places
//! values in exactly those sixteen registers, and calling any C function of at
//! most eight integer and eight FP arguments through it delivers each argument
//! to the register that function expects. The extra registers hold values it
//! never looks at.
//!
//! That reasoning is the whole safety argument, and it is also the exact list
//! of things this cannot do:
//!
//! - **More than eight arguments of a class.** The ninth would go on the stack,
//!   where a wider prototype cannot put it. Refused
//!   ([`AbiError::Unmarshallable`]), not guessed.
//! - **Aggregates by value.** A struct argument or return is passed by rules
//!   ([`x8`], splitting across registers, memory) that no scalar prototype
//!   reproduces. The table simply does not describe such functions, so they are
//!   refused as unknown — see [`super::table`].
//! - **Variadics.** Apple's arm64 ABI passes every variadic argument on the
//!   *stack*, so a register prototype cannot reach them at all. They belong to
//!   [`crate::shims`].
//!
//! ## `f32` through an `f64` slot
//!
//! A single-precision argument lives in `s_n`, which aliases the low 32 bits of
//! `v_n` — the same register an `f64` argument uses. Passing
//! `f64::from_bits(bits32 as u64)` therefore puts precisely the right 32 bits
//! where the callee looks, whereas passing `value as f64` would widen the
//! *value* and leave the callee reading the low half of a double. Returns are
//! the mirror image, which is why there are three call variants rather than one
//! with a cast.
//!
//! ## The stack pointer at an import exit
//!
//! Microlift exits *before* the `CALL` executes, so `rsp` still points where it
//! did at the call site and no return address has been pushed. That is exactly
//! the base [`ArgLocation::Stack`] offsets are measured from, so a spilled
//! argument is at `rsp + offset` with no adjustment. (Nothing in the current
//! table spills, but the path is here because getting it wrong later would look
//! like a miscompile rather than an ABI bug.)

use iced_x86::Register;

use microlift::arch::x86_64::abi::x86_reg;
use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::abi::{ArgLocation, GuestReg};

use super::{AbiError, ArgType, FunctionAbi, GuestMemory, Result, guest_call_abi};

/// Host argument registers of each class, per AAPCS64.
const MAX_INT_ARGS: usize = 8;
const MAX_FP_ARGS: usize = 8;

/// Whether [`call`] can express this signature at all.
///
/// Checked separately from making the call so [`super::classify`] can answer
/// the question statically, at load time, for every import in a program at
/// once — which is what turns "this will fail" from a runtime surprise into a
/// line in the linking report.
pub fn is_marshallable(abi: &FunctionAbi) -> bool {
    if abi.variadic {
        return false;
    }
    let Some(placed) = guest_call_abi().place_args(abi.types) else {
        return false;
    };
    // Counted by argument *class*, not by where the guest put it: SysV has only
    // six integer registers, so a seventh integer argument arrives on the guest
    // stack and still goes out in `x6`.
    let ints = placed.iter().filter(|(ty, _)| !is_fp(*ty)).count();
    let fps = placed.len() - ints;
    ints <= MAX_INT_ARGS && fps <= MAX_FP_ARGS
}

/// Call `host` with the arguments `ctx` holds, and write the result back.
///
/// `ctx` is left untouched unless the call is actually made. Guest function
/// pointers are refused; see [`call_with`].
pub fn call(
    symbol: &str,
    abi: &FunctionAbi,
    host: u64,
    ctx: &mut RegisterContext,
    mem: &dyn GuestMemory,
) -> Result<()> {
    call_with(symbol, abi, host, ctx, mem, &super::NoCallbacks)
}

/// [`call`], with a bridge for the arguments [`FunctionAbi::callbacks`] names.
///
/// Substitution happens here rather than in the caller because *here* is where
/// an argument's parameter index is known: `place_args` returns locations in
/// declaration order, so the `i`-th of them is parameter `i` whatever registers
/// or stack slots it happened to land in.
pub fn call_with(
    symbol: &str,
    abi: &FunctionAbi,
    host: u64,
    ctx: &mut RegisterContext,
    mem: &dyn GuestMemory,
    callbacks: &dyn super::Callbacks,
) -> Result<()> {
    call_types(
        symbol,
        abi.types,
        host,
        ctx,
        mem,
        &ForCallbacks { abi, callbacks },
    )
}

/// What a value means on the other side of the seam.
///
/// The marshaller moves *bits*, and for almost every argument that is the whole
/// job. Two kinds of value mean something different on each side and have to be
/// translated as they cross: a guest function pointer, which the host would call
/// as arm64 (see [`super::Callbacks`]), and an Objective-C `Class`, which the
/// guest names by the address of the dead `objc_class` in its own `__objc_data`
/// (see [`crate::objc`]). Both are "this pointer is not what the other side
/// thinks it is", so both go through one hook rather than two mechanisms.
///
/// By index rather than by value, because *which* arguments are special is
/// stated by a signature and cannot be recovered from the bits.
pub trait Convert {
    /// The value the host should receive for parameter `index`.
    fn argument(&self, symbol: &str, index: usize, raw: u64) -> Result<u64>;

    /// The value the guest should receive back from an integer or pointer
    /// return. The default is the identity, which is right for everything that
    /// returns a plain scalar.
    fn result(&self, raw: u64) -> u64 {
        raw
    }
}

/// Every argument crosses unchanged. The common case, and the marshaller's own
/// tests.
pub struct Verbatim;

impl Convert for Verbatim {
    fn argument(&self, _symbol: &str, _index: usize, raw: u64) -> Result<u64> {
        Ok(raw)
    }
}

/// [`Convert`] for the parameters [`FunctionAbi::callbacks`] names.
struct ForCallbacks<'a> {
    abi: &'a FunctionAbi,
    callbacks: &'a dyn super::Callbacks,
}

impl Convert for ForCallbacks<'_> {
    fn argument(&self, symbol: &str, index: usize, raw: u64) -> Result<u64> {
        if self.abi.callbacks.contains(&(index as u8)) {
            self.callbacks.substitute(symbol, raw)
        } else {
            Ok(raw)
        }
    }
}

/// [`call_with`], for a signature that is not in the table.
///
/// The same marshaller driven by a `&[ArgType]` computed at run time rather than
/// by a `&'static FunctionAbi`. There is exactly one such signature source and
/// it is the reason M6.3 is possible at all: an Objective-C method carries its
/// own type encoding, so `objc_msgSend` — which has no signature of its own —
/// has *whichever* signature the selector's method declares. See
/// [`crate::objc::encoding`].
pub fn call_types(
    symbol: &str,
    types: &[ArgType],
    host: u64,
    ctx: &mut RegisterContext,
    mem: &dyn GuestMemory,
    convert: &dyn Convert,
) -> Result<()> {
    let guest = guest_call_abi();
    let placed = guest
        .place_args(types)
        .ok_or_else(|| AbiError::Unmarshallable {
            symbol: symbol.to_string(),
            reason: "the guest ABI cannot place its arguments",
        })?;

    // Collected first, so a memory failure on the last stack argument still
    // leaves the guest context untouched.
    let mut ints = [0u64; MAX_INT_ARGS];
    let mut fps = [0f64; MAX_FP_ARGS];
    let (mut n_int, mut n_fp) = (0usize, 0usize);

    for (index, (ty, loc)) in placed.iter().enumerate() {
        // A pointer that means something different on the far side — a guest
        // function address the host would call as arm64, or a guest class the
        // host runtime has never realized. Translated before anything else looks
        // at it, and a refusal here still leaves the guest context untouched:
        // nothing has been called yet.
        let raw = convert.argument(symbol, index, read_location(symbol, ctx, mem, *loc)?)?;
        if is_fp(*ty) {
            if n_fp == MAX_FP_ARGS {
                return Err(AbiError::Unmarshallable {
                    symbol: symbol.to_string(),
                    reason: "more than eight floating-point arguments",
                });
            }
            fps[n_fp] = fp_arg(*ty, raw);
            n_fp += 1;
        } else {
            if n_int == MAX_INT_ARGS {
                return Err(AbiError::Unmarshallable {
                    symbol: symbol.to_string(),
                    reason: "more than eight integer arguments",
                });
            }
            ints[n_int] = int_arg(*ty, raw);
            n_int += 1;
        }
    }

    let ret = types.first().copied().unwrap_or(ArgType::None);
    match ret {
        // SAFETY: `host` is a host function address a `SymbolProvider` resolved
        // via `dlsym`, i.e. a real native arm64 C function. The prototypes below
        // are wider than its own in the argument registers only, which AAPCS64
        // permits (see the module note); the return variant is chosen to match
        // the class the table declares. Signatures with stack arguments or
        // aggregates never reach here — `is_marshallable` and the loop above
        // reject them.
        ArgType::SinglePrecisionFp => {
            let bits = unsafe { call_f32(host, &ints, &fps) }.to_bits();
            // Only the low 32 bits of xmm0 are architecturally defined for a
            // float return; zeroing the rest is the tidier of two legal answers.
            ctx.xmm[0] = [u64::from(bits), 0];
        }
        ArgType::DoublePrecisionFp => {
            let v = unsafe { call_f64(host, &ints, &fps) };
            ctx.xmm[0] = [v.to_bits(), 0];
        }
        ArgType::None => {
            unsafe { call_int(host, &ints, &fps) };
        }
        _ => {
            let v = unsafe { call_int(host, &ints, &fps) };
            ctx.rax = int_ret(ret, convert.result(v));
        }
    }
    Ok(())
}

fn is_fp(ty: ArgType) -> bool {
    matches!(ty, ArgType::SinglePrecisionFp | ArgType::DoublePrecisionFp)
}

/// The raw 64 bits the guest left at `loc`.
fn read_location(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &dyn GuestMemory,
    loc: ArgLocation,
) -> Result<u64> {
    match loc {
        ArgLocation::IntReg(reg) => int_reg(symbol, ctx, reg),
        ArgLocation::FpReg(i) => {
            ctx.xmm
                .get(usize::from(i))
                .map(|x| x[0])
                .ok_or_else(|| AbiError::Unmarshallable {
                    symbol: symbol.to_string(),
                    reason: "vector argument register out of range",
                })
        }
        // See the module note: `rsp` is the pre-`CALL` value at an import exit,
        // which is the base these offsets are measured from.
        ArgLocation::Stack(off) => mem.read_u64(ctx.rsp.wrapping_add(u64::from(off)), symbol),
    }
}

/// Resolve a [`GuestReg`] to the field of the context that holds it.
///
/// A `GuestReg` payload *is* an `iced_x86::Register` discriminant, so this goes
/// through microlift's own accessor rather than matching raw numbers — the
/// numbering is iced's to change.
fn int_reg(symbol: &str, ctx: &RegisterContext, reg: GuestReg) -> Result<u64> {
    let unsupported = || AbiError::Unmarshallable {
        symbol: symbol.to_string(),
        reason: "argument in a register the context does not model",
    };
    let reg = x86_reg(reg).map_err(|_| unsupported())?;
    Ok(match reg {
        Register::RAX => ctx.rax,
        Register::RCX => ctx.rcx,
        Register::RDX => ctx.rdx,
        Register::RBX => ctx.rbx,
        Register::RSP => ctx.rsp,
        Register::RBP => ctx.rbp,
        Register::RSI => ctx.rsi,
        Register::RDI => ctx.rdi,
        Register::R8 => ctx.r8,
        Register::R9 => ctx.r9,
        Register::R10 => ctx.r10,
        Register::R11 => ctx.r11,
        Register::R12 => ctx.r12,
        Register::R13 => ctx.r13,
        Register::R14 => ctx.r14,
        Register::R15 => ctx.r15,
        _ => return Err(unsupported()),
    })
}

/// Narrow the guest's 64-bit register down to the declared argument type, then
/// widen it back the way the host expects to receive it.
///
/// Apple's arm64 ABI makes the *caller* extend arguments narrower than 32 bits,
/// so this is not cosmetic: passing a raw `rdi` where the callee declares `char`
/// hands it whatever the guest happened to leave in the upper bits.
fn int_arg(ty: ArgType, raw: u64) -> u64 {
    match ty {
        ArgType::SignedByte => raw as u8 as i8 as i64 as u64,
        ArgType::UnsignedByte => u64::from(raw as u8),
        ArgType::SignedHalfword => raw as u16 as i16 as i64 as u64,
        ArgType::UnsignedHalfword => u64::from(raw as u16),
        ArgType::SignedWord => raw as u32 as i32 as i64 as u64,
        ArgType::UnsignedWord => u64::from(raw as u32),
        _ => raw,
    }
}

/// The bits to put in `v_n` for a floating-point argument. See the module note
/// on why `f32` is a bit-pattern move and not a cast.
fn fp_arg(ty: ArgType, raw: u64) -> f64 {
    match ty {
        ArgType::SinglePrecisionFp => f64::from_bits(u64::from(raw as u32)),
        _ => f64::from_bits(raw),
    }
}

/// What the guest should find in `rax`.
///
/// x86-64 leaves only the declared width defined (a function returning `int`
/// defines `eax`), and the host leaves only the declared width defined in `x0`.
/// Extending here means the guest reading the full `rax` — which a translated
/// region may well do — sees the value's own sign rather than the host's
/// leftovers.
fn int_ret(ty: ArgType, raw: u64) -> u64 {
    int_arg(ty, raw)
}

type IntFn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
) -> u64;

type F64Fn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
) -> f64;

type F32Fn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
) -> f32;

macro_rules! call_through {
    ($f:expr, $i:expr, $v:expr) => {
        $f(
            $i[0], $i[1], $i[2], $i[3], $i[4], $i[5], $i[6], $i[7], $v[0], $v[1], $v[2], $v[3],
            $v[4], $v[5], $v[6], $v[7],
        )
    };
}

/// # Safety
/// `host` must be a native C function of at most eight integer and eight
/// floating-point scalar arguments returning an integer or pointer. See the
/// module note for why the wider prototype is ABI-compatible with it.
unsafe fn call_int(host: u64, ints: &[u64; MAX_INT_ARGS], fps: &[f64; MAX_FP_ARGS]) -> u64 {
    let f: IntFn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

/// # Safety
/// As [`call_int`], for a callee returning `double`.
unsafe fn call_f64(host: u64, ints: &[u64; MAX_INT_ARGS], fps: &[f64; MAX_FP_ARGS]) -> f64 {
    let f: F64Fn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

/// # Safety
/// As [`call_int`], for a callee returning `float`.
unsafe fn call_f32(host: u64, ints: &[u64; MAX_INT_ARGS], fps: &[f64; MAX_FP_ARGS]) -> f32 {
    let f: F32Fn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_integer_arguments_are_extended_by_signedness() {
        assert_eq!(
            int_arg(ArgType::SignedByte, 0xffff_ffff_ffff_ff80),
            u64::MAX - 127
        );
        assert_eq!(int_arg(ArgType::UnsignedByte, 0xdead_beef_0000_0080), 0x80);
        assert_eq!(
            int_arg(ArgType::SignedWord, 0x1234_5678_ffff_fffe),
            u64::MAX - 1
        );
        assert_eq!(
            int_arg(ArgType::UnsignedWord, 0x1234_5678_ffff_fffe),
            0xffff_fffe
        );
        assert_eq!(
            int_arg(ArgType::Pointer, 0x1234_5678_9abc_def0),
            0x1234_5678_9abc_def0
        );
    }

    #[test]
    fn a_float_argument_moves_bits_rather_than_widening_the_value() {
        // The callee reads `s_n`, the low half of the register. Passing
        // `1.5f32 as f64` would leave the low half holding the low bits of the
        // double — which is not 1.5f32 and not any nearby float.
        let bits = 1.5f32.to_bits();
        let passed = fp_arg(ArgType::SinglePrecisionFp, u64::from(bits));
        assert_eq!(passed.to_bits() as u32, bits);
        assert_ne!(passed.to_bits(), (1.5f64).to_bits());
    }
}
