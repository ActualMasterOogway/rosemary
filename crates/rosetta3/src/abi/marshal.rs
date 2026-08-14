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
//! ## Past the eighth integer argument
//!
//! The ninth integer argument goes on the stack, and a prototype *can* put it
//! there — by declaring more than eight, which is what
//! [`MAX_INT_ARGS_TOTAL`] allows. AAPCS64 allocates in declaration order, so
//! twelve `u64` followed by eight `f64` fills `x0`–`x7`, then four stack slots,
//! then `v0`–`v7`; a callee with nine integer arguments and no more than eight
//! floating-point ones reads its ninth from `[sp]`, which is where the ninth
//! `u64` was put. `glTexImage2D` is why this exists — nine integer arguments,
//! and a texture upload is not optional in a program that draws.
//!
//! **The catch is Apple's packing rule, and it is why this is not simply
//! "widen to sixteen".** Unlike generic AAPCS64, Apple's arm64 ABI does not
//! round a stack argument up to eight bytes: an `int` on the stack occupies
//! four, and the argument after it starts at `[sp+4]`. A `u64` prototype would
//! place that next argument at `[sp+8]` instead.
//!
//! ## When widening the overflow is wrong, and when it is not
//!
//! "An overflowing argument must already be eight bytes wide" was the rule
//! here, and it is stricter than the ABI. Apple packs each stack argument at
//! its own size *and its own alignment*, so a four-byte argument followed by a
//! pointer still leaves the pointer on the next eight-byte boundary — exactly
//! where a `u64` declaration puts it. A narrow argument only moves what follows
//! when the next argument is narrow too and packs in beside it.
//!
//! [`widening_preserves_offsets`] is that test, and it is what lets
//! `-[NSBitmapImageRep initWithBitmapDataPlanes:…bitsPerPixel:]` — whose
//! `isPlanar:` is a one-byte `BOOL` on the stack with a pointer after it — go
//! through the plain prototype unchanged.
//!
//! Where the test fails, the tail is declared at its real widths instead: eight
//! `u64` for `x0`–`x7`, then one prototype per pattern of overflow widths, then
//! the eight `f64` — which stay in `v0`–`v7` and never reach the stack, so
//! putting them last changes nothing about where the tail lands. [`NarrowTail`]
//! enumerates the patterns, up to three overflowing arguments. Three is not a
//! principle, it is where the growth was stopped: `glTexSubImage3D` and
//! `glCompressedTexSubImage3D` are the shapes that need it, and a texture
//! upload is not optional in a program that draws. A longer mismatching tail is
//! still refused by name.
//!
//! That reasoning is the whole safety argument, and it is also the exact list
//! of things this cannot do:
//!
//! - **More than twelve integer or eight floating-point arguments**; more than
//!   three overflowing arguments when any of them is narrow; and a narrow
//!   overflow at all when the *return* is floating point, since only the
//!   integer-returning prototypes were given tails and nothing on this system
//!   both overflows and returns a `double`. Refused
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
//! ## The stack pointer at an import exit, which is not one answer
//!
//! Microlift exits *before* a `CALL` executes, so `rsp` still points where it
//! did at the call site and no return address has been pushed: a spilled
//! argument is at `rsp + offset`.
//!
//! A **tail `jmp` to an import is different**, and the difference is eight
//! bytes. `return getnameinfo(sa, sl, h, hl, s, sl2, flags);` compiles to
//! `jmp _getnameinfo` with the *original* return address still on the stack and
//! the outgoing arguments in the incoming argument area above it — so the same
//! spilled argument is at `rsp + 8 + offset`.
//!
//! Reading the wrong one does not fault. It takes the return address for an
//! argument and shifts every stack argument after it by a slot, which is the
//! silent wrong answer this module exists to refuse. [`stack_base`] is the
//! adjustment, and `tests/tailcall.rs` is a guest that loses a flags word this
//! way and gets a reverse DNS lookup it did not ask for.

use iced_x86::Register;

use microlift::arch::x86_64::abi::x86_reg;
use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::abi::{ArgLocation, GuestReg};

use super::aggregate::{self, Eightbyte, GuestClass, HostClass, Shape};
use super::{AbiError, ArgType, FunctionAbi, GuestMemory, Result, guest_call_abi_ref};

/// Host argument registers of each class, per AAPCS64.
const MAX_INT_ARGS: usize = 8;
const MAX_FP_ARGS: usize = 8;

/// Integer arguments the over-wide prototype declares, registers and stack
/// together. The eight past `MAX_INT_ARGS` land on the stack — see the module
/// note for the rule that makes them reproducible.
///
/// Twelve until `-[NSBitmapImageRep initWithBitmapDataPlanes:…bitsPerPixel:]`
/// wanted thirteen. Eleven parameters plus `self` and `_cmd` is not an exotic
/// selector — AppKit has a dozen of that shape — so the bank is sized to leave
/// room rather than to the next program that asks.
const MAX_INT_ARGS_TOTAL: usize = 16;

/// Whether `ty` occupies a full eight bytes.
fn is_doubleword(ty: ArgType) -> bool {
    matches!(
        ty,
        ArgType::Pointer | ArgType::SignedDoubleword | ArgType::UnsignedDoubleword
    )
}

/// How many bytes `ty` occupies in a stack slot, which is also its alignment.
fn stack_width(ty: ArgType) -> u64 {
    match ty {
        ArgType::SignedByte | ArgType::UnsignedByte => 1,
        ArgType::SignedHalfword | ArgType::UnsignedHalfword => 2,
        ArgType::SignedWord | ArgType::UnsignedWord | ArgType::SinglePrecisionFp => 4,
        _ => 8,
    }
}

/// Whether declaring every overflowing argument `u64` puts each of them where
/// Apple's rule puts it.
///
/// This is the rule the module note used to state as "an overflowing argument
/// must already be eight bytes wide", which is stricter than the ABI. Apple
/// packs each stack argument at its own size *and its own alignment*, so a
/// four-byte argument followed by a pointer still leaves the pointer at the
/// next eight-byte boundary — exactly where a `u64` declaration would have put
/// it. A narrow argument only moves what comes after it when the next argument
/// is narrow too and packs in beside it.
///
/// Writing eight bytes into a four- or one-byte slot is harmless in the cases
/// this accepts: the padding it covers belongs to no other argument, the callee
/// reads the low bytes it declared, and the caller reserved eight because that
/// is what the prototype says. Little-endian is doing the work, and it is worth
/// saying that out loud because on a big-endian machine none of this would
/// hold.
fn widening_preserves_offsets(overflow: &[ArgType]) -> bool {
    let mut packed = 0u64;
    for (slot, ty) in overflow.iter().enumerate() {
        let width = stack_width(*ty);
        packed = packed.next_multiple_of(width);
        if packed != slot as u64 * 8 {
            return false;
        }
        packed += width;
    }
    true
}

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
    let Some(placed) = guest_call_abi_ref().place_args(abi.types) else {
        return false;
    };
    // Counted by argument *class*, not by where the guest put it: SysV has only
    // six integer registers, so a seventh integer argument arrives on the guest
    // stack and still goes out in `x6`.
    let integers: Vec<ArgType> = placed
        .iter()
        .map(|(ty, _)| *ty)
        .filter(|ty| !is_fp(*ty))
        .collect();
    let fps = placed.len() - integers.len();
    if integers.len() > MAX_INT_ARGS_TOTAL || fps > MAX_FP_ARGS {
        return false;
    }
    // Everything the host will pass on its own stack is placed at its own
    // width, by a prototype chosen for the pattern those widths make.
    let ret = abi.types.first().copied().unwrap_or(ArgType::None);
    NarrowTail::of(&integers[MAX_INT_ARGS.min(integers.len())..], is_fp(ret)).is_some()
}

/// The prototype that places a run of overflowing arguments at their own
/// widths.
///
/// `None` is not "no overflow" — it is "no *narrow* overflow", which the
/// twelve-`u64` prototype already handles. A variant here exists only where
/// that one would put an argument in the wrong place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NarrowTail {
    /// Every overflowing argument is a full doubleword: the original prototype.
    None,
    W4,
    W44,
    W48,
    W84,
    W444,
    W448,
    W484,
    W488,
    W844,
    W848,
    W884,
}

impl NarrowTail {
    /// Which prototype places `overflow`, or `None` if nothing here can.
    ///
    /// `fp_return` refuses the whole question rather than answering it wrong:
    /// the tail prototypes below all return an integer, and a signature that
    /// both overflows and returns a `double` would silently take the
    /// doubleword-only path.
    fn of(overflow: &[ArgType], fp_return: bool) -> Option<Self> {
        if widening_preserves_offsets(overflow) {
            return Some(NarrowTail::None);
        }
        if fp_return {
            return None;
        }
        match overflow
            .iter()
            .map(|ty| if is_doubleword(*ty) { 8u8 } else { 4 })
            .collect::<Vec<_>>()
            .as_slice()
        {
            [4] => Some(NarrowTail::W4),
            [4, 4] => Some(NarrowTail::W44),
            [4, 8] => Some(NarrowTail::W48),
            [8, 4] => Some(NarrowTail::W84),
            [4, 4, 4] => Some(NarrowTail::W444),
            [4, 4, 8] => Some(NarrowTail::W448),
            [4, 8, 4] => Some(NarrowTail::W484),
            [4, 8, 8] => Some(NarrowTail::W488),
            [8, 4, 4] => Some(NarrowTail::W844),
            [8, 4, 8] => Some(NarrowTail::W848),
            [8, 8, 4] => Some(NarrowTail::W884),
            _ => None,
        }
    }
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
    let guest = guest_call_abi_ref();
    let placed = guest
        .place_args(types)
        .ok_or_else(|| AbiError::Unmarshallable {
            symbol: symbol.to_string(),
            reason: "the guest ABI cannot place its arguments",
        })?;

    // Collected first, so a memory failure on the last stack argument still
    // leaves the guest context untouched.
    let mut ints = [0u64; MAX_INT_ARGS_TOTAL];
    let mut fps = [0f64; MAX_FP_ARGS];
    let (mut n_int, mut n_fp) = (0usize, 0usize);
    // The integer arguments past the eighth, in order. Their widths decide
    // where each of them lands on the host's stack, and so which prototype
    // makes the call.
    let mut overflow: Vec<ArgType> = Vec::new();

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
            if n_int == MAX_INT_ARGS_TOTAL {
                return Err(AbiError::Unmarshallable {
                    symbol: symbol.to_string(),
                    reason: "more than sixteen integer arguments",
                });
            }
            if n_int >= MAX_INT_ARGS {
                overflow.push(*ty);
            }
            ints[n_int] = int_arg(*ty, raw);
            n_int += 1;
        }
    }

    let ret = types.first().copied().unwrap_or(ArgType::None);
    // Chosen after every argument is read and before anything is called, so a
    // signature whose tail no prototype places costs a named refusal with the
    // guest context untouched.
    let tail = NarrowTail::of(&overflow, is_fp(ret)).ok_or_else(|| AbiError::Unmarshallable {
        symbol: symbol.to_string(),
        reason: "no prototype places this signature's overflowing arguments",
    })?;
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
            unsafe { call_int_tail(host, &ints, &fps, tail) };
        }
        _ => {
            let v = unsafe { call_int_tail(host, &ints, &fps, tail) };
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
        ArgLocation::Stack(off) => mem.read_u64(
            ctx.rsp
                .wrapping_add(stack_base())
                .wrapping_add(u64::from(off)),
            symbol,
        ),
    }
}

/// Where the guest's outgoing stack arguments begin, relative to `rsp`.
///
/// Zero for an intercepted `call` and **eight for a tail `jmp`**, and the
/// difference is not cosmetic. Microlift exits a `call` *before* the return
/// address is pushed, so `rsp` points at the caller's outgoing argument area and
/// the seventh integer argument is at `rsp+0`. A compiler that tail-jumps —
/// `return getnameinfo(sa, sl, h, hl, s, sl2, flags);` compiles to exactly that
/// — leaves the *original* return address on the stack and reuses the incoming
/// argument area above it, so the same argument is at `rsp+8`.
///
/// Reading the wrong one does not fault. It reads the return address as an
/// argument and shifts every stack argument after it by one slot, which is the
/// silent wrong answer this module exists to refuse: `tests/tailcall.rs` is a
/// guest that loses `NI_NUMERICHOST` this way and gets a reverse DNS lookup it
/// did not ask for.
///
/// [`crate::thread::ThreadState::entered_by_jump`] is set by the run loop before
/// any shim runs, so it always describes *this* call. Outside a guest thread —
/// which is where this module's own tests run — there is no tail call to
/// account for and the answer is zero.
fn stack_base() -> u64 {
    match crate::thread::current() {
        Some(state) if state.entered_by_jump() => 8,
        _ => 0,
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
            $i[0], $i[1], $i[2], $i[3], $i[4], $i[5], $i[6], $i[7], $i[8], $i[9], $i[10], $i[11],
            $i[12], $i[13], $i[14], $i[15], $v[0], $v[1], $v[2], $v[3], $v[4], $v[5], $v[6], $v[7],
        )
    };
}

/// One prototype per overflow-width pattern.
///
/// Eight `u64` fill `x0`–`x7`; the tail types are what AAPCS64 then places on
/// the stack, each at its own width and alignment; the eight `f64` occupy
/// `v0`–`v7` and never reach the stack, which is why declaring them after the
/// tail does not move it. See the module note.
macro_rules! narrow_tail_call {
    ($name:ident, $(($t:ty, $at:expr)),+) => {
        /// # Safety
        /// `host` must be a native C function whose overflowing arguments have
        /// exactly the widths this variant names, returning an integer, a
        /// pointer or nothing.
        unsafe fn $name(
            host: u64,
            ints: &[u64; MAX_INT_ARGS_TOTAL],
            fps: &[f64; MAX_FP_ARGS],
        ) -> u64 {
            type F = unsafe extern "C" fn(
                u64, u64, u64, u64, u64, u64, u64, u64,
                $($t),+,
                f64, f64, f64, f64, f64, f64, f64, f64,
            ) -> u64;
            let f: F = unsafe { core::mem::transmute(host as usize as *const ()) };
            unsafe {
                f(
                    ints[0], ints[1], ints[2], ints[3],
                    ints[4], ints[5], ints[6], ints[7],
                    $(ints[$at] as $t),+,
                    fps[0], fps[1], fps[2], fps[3],
                    fps[4], fps[5], fps[6], fps[7],
                )
            }
        }
    };
}

narrow_tail_call!(call_w4, (u32, 8));
narrow_tail_call!(call_w44, (u32, 8), (u32, 9));
narrow_tail_call!(call_w48, (u32, 8), (u64, 9));
narrow_tail_call!(call_w84, (u64, 8), (u32, 9));
narrow_tail_call!(call_w444, (u32, 8), (u32, 9), (u32, 10));
narrow_tail_call!(call_w448, (u32, 8), (u32, 9), (u64, 10));
narrow_tail_call!(call_w484, (u32, 8), (u64, 9), (u32, 10));
narrow_tail_call!(call_w488, (u32, 8), (u64, 9), (u64, 10));
narrow_tail_call!(call_w844, (u64, 8), (u32, 9), (u32, 10));
narrow_tail_call!(call_w848, (u64, 8), (u32, 9), (u64, 10));
narrow_tail_call!(call_w884, (u64, 8), (u64, 9), (u32, 10));

/// Call `host` through whichever prototype places its overflowing arguments.
///
/// # Safety
/// As [`call_int`], plus: `tail` must describe `host`'s real overflow widths.
unsafe fn call_int_tail(
    host: u64,
    ints: &[u64; MAX_INT_ARGS_TOTAL],
    fps: &[f64; MAX_FP_ARGS],
    tail: NarrowTail,
) -> u64 {
    unsafe {
        match tail {
            NarrowTail::None => call_int(host, ints, fps),
            NarrowTail::W4 => call_w4(host, ints, fps),
            NarrowTail::W44 => call_w44(host, ints, fps),
            NarrowTail::W48 => call_w48(host, ints, fps),
            NarrowTail::W84 => call_w84(host, ints, fps),
            NarrowTail::W444 => call_w444(host, ints, fps),
            NarrowTail::W448 => call_w448(host, ints, fps),
            NarrowTail::W484 => call_w484(host, ints, fps),
            NarrowTail::W488 => call_w488(host, ints, fps),
            NarrowTail::W844 => call_w844(host, ints, fps),
            NarrowTail::W848 => call_w848(host, ints, fps),
            NarrowTail::W884 => call_w884(host, ints, fps),
        }
    }
}

/// # Safety
/// `host` must be a native C function of at most twelve integer and eight
/// floating-point scalar arguments returning an integer or pointer, with every
/// integer argument past the eighth a full doubleword. See the module note for
/// why the wider prototype is ABI-compatible with it.
unsafe fn call_int(host: u64, ints: &[u64; MAX_INT_ARGS_TOTAL], fps: &[f64; MAX_FP_ARGS]) -> u64 {
    let f: IntFn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

/// # Safety
/// As [`call_int`], for a callee returning `double`.
unsafe fn call_f64(host: u64, ints: &[u64; MAX_INT_ARGS_TOTAL], fps: &[f64; MAX_FP_ARGS]) -> f64 {
    let f: F64Fn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

/// # Safety
/// As [`call_int`], for a callee returning `float`.
unsafe fn call_f32(host: u64, ints: &[u64; MAX_INT_ARGS_TOTAL], fps: &[f64; MAX_FP_ARGS]) -> f32 {
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

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// The most bytes an indirectly-returned aggregate may occupy.
///
/// A bound rather than a limit anybody will meet: the largest struct Cocoa
/// returns by value is `CGAffineTransform` at forty-eight bytes. The buffer has
/// to be a concrete Rust type for the callee to be given its address in `x8`,
/// so it has to have *a* size, and this is one that no framework exceeds.
const MAX_INDIRECT: usize = 256;

/// Where the guest left one argument, in SysV AMD64's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Where {
    /// Index into `rdi, rsi, rdx, rcx, r8, r9`.
    Int(usize),
    /// Index into `xmm0`…`xmm7`, of which only the low eight bytes are read.
    Sse(usize),
    /// Byte offset from `rsp`, which at an import exit is the pre-`CALL` value.
    Stack(u64),
}

/// Where the guest left one whole argument.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    Scalar(Where),
    /// One [`Where`] per eightbyte, in order.
    Parts(Vec<Where>),
    /// `size` bytes at `rsp + offset`.
    Memory {
        offset: u64,
        size: usize,
    },
}

const SYSV_INT_REGS: usize = 6;
const SYSV_SSE_REGS: usize = 8;

/// Lay a signature out the way SysV AMD64 does, aggregates included.
///
/// Written here rather than taken from microlift's `CallAbi::place_args`
/// because that one knows only scalars, and an aggregate does not merely
/// occupy a slot — it consumes *two* registers of possibly different classes,
/// or none at all and a run of stack instead. Everything an all-scalar
/// signature does is unchanged, and
/// `the_two_placers_agree_on_every_scalar_signature` holds the two to each
/// other so this cannot drift from the path the rest of the project uses.
fn place_shapes(shapes: &[Shape]) -> (Vec<Place>, Option<Where>) {
    let mut int = 0usize;
    let mut sse = 0usize;
    let mut stack = 0u64;

    // A MEMORY-class return takes the first integer register for its hidden
    // buffer pointer, before any argument gets one. Everything after shifts,
    // which is the whole of why `objc_msgSend_stret` is a separate trampoline.
    let sret = match shapes.first() {
        Some(Shape::Struct(agg)) if agg.guest_class() == GuestClass::Memory => {
            int = 1;
            Some(Where::Int(0))
        }
        _ => None,
    };

    let mut places = Vec::with_capacity(shapes.len().saturating_sub(1));
    for shape in shapes.iter().skip(1) {
        places.push(match shape {
            Shape::Scalar(ty) if aggregate::is_fp(*ty) => {
                let w = match sse < SYSV_SSE_REGS {
                    true => Where::Sse(sse),
                    false => Where::Stack(take_stack(&mut stack, 8)),
                };
                sse += usize::from(sse < SYSV_SSE_REGS);
                Place::Scalar(w)
            }
            Shape::Scalar(_) => {
                let w = match int < SYSV_INT_REGS {
                    true => Where::Int(int),
                    false => Where::Stack(take_stack(&mut stack, 8)),
                };
                int += usize::from(int < SYSV_INT_REGS);
                Place::Scalar(w)
            }
            Shape::Struct(agg) => match agg.guest_class() {
                GuestClass::Memory => Place::Memory {
                    offset: take_stack(&mut stack, agg.size),
                    size: agg.size,
                },
                GuestClass::Registers(classes) => {
                    let need_int = classes.iter().filter(|c| **c == Eightbyte::Integer).count();
                    let need_sse = classes.len() - need_int;
                    // All or nothing: SysV puts the *whole* aggregate in memory
                    // when the registers its eightbytes want are not all free.
                    if int + need_int > SYSV_INT_REGS || sse + need_sse > SYSV_SSE_REGS {
                        Place::Memory {
                            offset: take_stack(&mut stack, agg.size),
                            size: agg.size,
                        }
                    } else {
                        let parts = classes
                            .iter()
                            .map(|c| match c {
                                Eightbyte::Integer => {
                                    int += 1;
                                    Where::Int(int - 1)
                                }
                                Eightbyte::Sse => {
                                    sse += 1;
                                    Where::Sse(sse - 1)
                                }
                            })
                            .collect();
                        Place::Parts(parts)
                    }
                }
            },
        });
    }
    (places, sret)
}

/// Reserve `size` bytes of the guest's outgoing stack area, eight-aligned.
fn take_stack(cursor: &mut u64, size: usize) -> u64 {
    let at = *cursor;
    *cursor += (size as u64).next_multiple_of(8);
    at
}

/// The bytes the guest left for one aggregate argument.
fn read_aggregate(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &dyn GuestMemory,
    place: &Place,
    size: usize,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size.next_multiple_of(8)];
    match place {
        Place::Memory { offset, .. } => {
            let at = ctx.rsp.wrapping_add(stack_base()).wrapping_add(*offset);
            mem.read(at, &mut buf[..size])
                .map_err(|_| AbiError::Memory {
                    symbol: symbol.to_string(),
                    addr: at,
                })?;
        }
        Place::Parts(parts) => {
            for (i, w) in parts.iter().enumerate() {
                let word = read_where(symbol, ctx, mem, *w)?;
                buf[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
            }
        }
        Place::Scalar(_) => {
            return Err(AbiError::Unmarshallable {
                symbol: symbol.to_string(),
                reason: "an aggregate placed as a scalar",
            });
        }
    }
    buf.truncate(size);
    Ok(buf)
}

fn read_where(symbol: &str, ctx: &RegisterContext, mem: &dyn GuestMemory, w: Where) -> Result<u64> {
    Ok(match w {
        Where::Int(i) => [ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx, ctx.r8, ctx.r9][i],
        Where::Sse(i) => ctx.xmm[i][0],
        Where::Stack(off) => {
            mem.read_u64(ctx.rsp.wrapping_add(stack_base()).wrapping_add(off), symbol)?
        }
    })
}

/// Read `width` bytes at `offset` as the scalar `ty`, zero-padded.
fn leaf_bits(buf: &[u8], offset: usize, width: usize) -> u64 {
    let mut word = [0u8; 8];
    let end = (offset + width).min(buf.len());
    if offset < end {
        word[..end - offset].copy_from_slice(&buf[offset..end]);
    }
    u64::from_le_bytes(word)
}

/// Call `host` with a signature that may contain aggregates.
///
/// The general form of [`call_types`], and the one an Objective-C message takes
/// whenever a method's own encoding names a struct. `shapes[0]` is the return.
///
/// The guarantee the rest of this module makes holds here too: **on any error
/// `ctx` is untouched**. Every fallible step — placement, reading the guest's
/// bytes, classifying the return — happens before the call, and the guest's
/// registers are written only once the host function has returned.
pub fn call_shapes(
    symbol: &str,
    shapes: &[Shape],
    host: u64,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    convert: &dyn Convert,
) -> Result<()> {
    let unmarshallable = |reason: &'static str| AbiError::Unmarshallable {
        symbol: symbol.to_string(),
        reason,
    };
    let ret = shapes
        .first()
        .cloned()
        .unwrap_or(Shape::Scalar(ArgType::None));
    // Classified before anything is read, so an unsupported return costs a
    // diagnostic rather than a half-made call.
    let ret_host = match &ret {
        Shape::Scalar(_) => None,
        Shape::Struct(agg) => {
            if agg.size > MAX_INDIRECT {
                return Err(unmarshallable(
                    "an aggregate return larger than this runtime buffers",
                ));
            }
            Some(agg.host_class().map_err(unmarshallable)?)
        }
    };

    let (places, sret) = place_shapes(shapes);

    let mut ints = [0u64; MAX_INT_ARGS_TOTAL];
    let mut fps = [0f64; MAX_FP_ARGS];
    let (mut n_int, mut n_fp) = (0usize, 0usize);

    // The copies AAPCS64 B.4 says the *caller* makes for an aggregate too large
    // for registers. Held here so every one of them outlives the call: the
    // callee is handed their addresses and reads through them.
    //
    // `u64` rather than `u8` so the allocation is eight-aligned, which is the
    // most any aggregate reaching this point asks for — `host_class` refuses a
    // sixteen-byte alignment before anything is copied.
    let mut copies: Vec<Vec<u64>> = Vec::new();

    // As in [`call_types`]: the integer arguments past the eighth decide which
    // prototype places them. An Objective-C method reaches this — a selector
    // with enough parameters is not rare, and one of Unity's is what found it.
    let mut overflow: Vec<ArgType> = Vec::new();
    let mut push_int =
        |v: u64, ty: ArgType, n_int: &mut usize, overflow: &mut Vec<ArgType>| -> Result<()> {
            if *n_int == MAX_INT_ARGS_TOTAL {
                return Err(unmarshallable("more than sixteen integer arguments"));
            }
            if *n_int >= MAX_INT_ARGS {
                overflow.push(ty);
            }
            ints[*n_int] = v;
            *n_int += 1;
            Ok(())
        };

    for (index, (shape, place)) in shapes.iter().skip(1).zip(places.iter()).enumerate() {
        match shape {
            Shape::Scalar(ty) => {
                let Place::Scalar(w) = place else {
                    return Err(unmarshallable("a scalar placed as an aggregate"));
                };
                let raw = convert.argument(symbol, index, read_where(symbol, ctx, mem, *w)?)?;
                if aggregate::is_fp(*ty) {
                    if n_fp == MAX_FP_ARGS {
                        return Err(unmarshallable("more than eight floating-point arguments"));
                    }
                    fps[n_fp] = fp_arg(*ty, raw);
                    n_fp += 1;
                } else {
                    push_int(int_arg(*ty, raw), *ty, &mut n_int, &mut overflow)?;
                }
            }
            Shape::Struct(agg) => {
                let bytes = read_aggregate(symbol, ctx, mem, place, agg.size)?;
                match agg.host_class().map_err(unmarshallable)? {
                    HostClass::Hfa { ty, count } => {
                        if n_fp + count > MAX_FP_ARGS {
                            return Err(unmarshallable("more than eight floating-point arguments"));
                        }
                        let width = aggregate::width_of(ty);
                        for i in 0..count {
                            fps[n_fp] = fp_arg(ty, leaf_bits(&bytes, i * width, width));
                            n_fp += 1;
                        }
                    }
                    HostClass::Integer { words } => {
                        for i in 0..words {
                            let w = leaf_bits(&bytes, i * 8, 8);
                            push_int(w, ArgType::UnsignedDoubleword, &mut n_int, &mut overflow)?;
                        }
                    }
                    // AAPCS64 B.4: a composite larger than sixteen bytes that is
                    // not an HVA is *copied by the caller* and the argument is
                    // replaced by a pointer to the copy — an ordinary integer
                    // argument, in an ordinary integer register. So the
                    // register prototype reaches it after all, which is why
                    // this is a copy and not the refusal it used to be.
                    //
                    // `-[MTLTexture replaceRegion:…]` is what found it: an
                    // `MTLRegion` is six `NSUInteger`s, and Unity's Metal
                    // backend sends that message for every texture it uploads.
                    HostClass::Indirect => {
                        let mut copy = vec![0u64; agg.size.div_ceil(8)];
                        for (i, w) in copy.iter_mut().enumerate() {
                            *w = leaf_bits(&bytes, i * 8, 8);
                        }
                        let addr = copy.as_ptr() as usize as u64;
                        copies.push(copy);
                        push_int(addr, ArgType::Pointer, &mut n_int, &mut overflow)?;
                    }
                }
            }
        }
    }

    // Only the integer-returning prototypes were given tails, so a narrow
    // overflow is refused for anything else — an aggregate return included,
    // since those go through their own prototypes and would silently take the
    // doubleword one.
    let plain_integer_return =
        ret_host.is_none() && !matches!(ret.scalar(), Some(t) if aggregate::is_fp(t));
    let tail = NarrowTail::of(&overflow, !plain_integer_return).ok_or_else(|| {
        unmarshallable("no prototype places this signature's overflowing arguments")
    })?;

    // Everything that can fail has failed by here.
    let (out, scalar_ret) = call_returning(host, &ints, &fps, ret_host, &ret, tail);
    // Said rather than left to scope: `ints` holds addresses into these, and the
    // callee has finished reading through them only now.
    drop(copies);
    write_return(ctx, mem, symbol, &ret, sret, &out, scalar_ret, convert)
}

/// Make the call, choosing the prototype the *host's* return class needs.
///
/// Returns the aggregate's bytes when there are any, and the scalar result
/// otherwise. One function rather than a `match` at the call site because every
/// arm is `unsafe` for the same reason and the reason is worth stating once.
fn call_returning(
    host: u64,
    ints: &[u64; MAX_INT_ARGS_TOTAL],
    fps: &[f64; MAX_FP_ARGS],
    ret_host: Option<HostClass>,
    ret: &Shape,
    tail: NarrowTail,
) -> (Vec<u8>, u64) {
    // SAFETY: `host` is a native arm64 C function — a `dlsym` result or an
    // Objective-C `IMP` the host runtime owns. The prototypes are wider than
    // its own in the argument registers only, which AAPCS64 permits (see the
    // module note), and the return variant matches the class computed from the
    // callee's own type encoding. An HFA prototype wider than the callee's own
    // return reads `v` registers the callee did not write, which is why only
    // the first `count` members are taken.
    unsafe {
        match ret_host {
            None => match ret.scalar() {
                Some(ArgType::SinglePrecisionFp) => {
                    (Vec::new(), u64::from(call_f32(host, ints, fps).to_bits()))
                }
                Some(ArgType::DoublePrecisionFp) => {
                    (Vec::new(), call_f64(host, ints, fps).to_bits())
                }
                _ => (Vec::new(), call_int_tail(host, ints, fps, tail)),
            },
            Some(HostClass::Hfa { ty, count }) => {
                let width = aggregate::width_of(ty);
                let mut out = Vec::with_capacity(count * width);
                if ty == ArgType::SinglePrecisionFp {
                    let v = call_f32x4(host, ints, fps);
                    for m in v.0.iter().take(count) {
                        out.extend_from_slice(&m.to_le_bytes());
                    }
                } else {
                    let v = call_f64x4(host, ints, fps);
                    for m in v.0.iter().take(count) {
                        out.extend_from_slice(&m.to_le_bytes());
                    }
                }
                (out, 0)
            }
            Some(HostClass::Integer { words }) => {
                let v = call_u64x2(host, ints, fps);
                let mut out = Vec::with_capacity(words * 8);
                for w in v.0.iter().take(words) {
                    out.extend_from_slice(&w.to_le_bytes());
                }
                (out, 0)
            }
            Some(HostClass::Indirect) => {
                let v = call_indirect(host, ints, fps);
                (v.0.to_vec(), 0)
            }
        }
    }
}

/// Put the result where the *guest's* convention says it goes.
#[expect(clippy::too_many_arguments)]
fn write_return(
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    symbol: &str,
    ret: &Shape,
    sret: Option<Where>,
    bytes: &[u8],
    scalar: u64,
    convert: &dyn Convert,
) -> Result<()> {
    let agg = match ret {
        Shape::Scalar(ArgType::SinglePrecisionFp) => {
            ctx.xmm[0] = [scalar & 0xffff_ffff, 0];
            return Ok(());
        }
        Shape::Scalar(ArgType::DoublePrecisionFp) => {
            ctx.xmm[0] = [scalar, 0];
            return Ok(());
        }
        Shape::Scalar(ArgType::None) => return Ok(()),
        Shape::Scalar(ty) => {
            ctx.rax = int_ret(*ty, convert.result(scalar));
            return Ok(());
        }
        Shape::Struct(agg) => agg,
    };

    match agg.guest_class() {
        // The hidden buffer the guest allocated, whose address it passed in the
        // first integer register and expects back in `rax`. `objc_msgSend_stret`
        // is the same convention with the trampoline's own name on it.
        GuestClass::Memory => {
            let Some(w) = sret else {
                return Err(AbiError::Unmarshallable {
                    symbol: symbol.to_string(),
                    reason: "a memory-class return with no buffer register",
                });
            };
            let dest = read_where(symbol, ctx, mem, w)?;
            let n = bytes.len().min(agg.size);
            mem.write(dest, &bytes[..n]).map_err(|_| AbiError::Memory {
                symbol: symbol.to_string(),
                addr: dest,
            })?;
            ctx.rax = dest;
        }
        GuestClass::Registers(classes) => {
            let (mut n_int, mut n_sse) = (0usize, 0usize);
            for (i, class) in classes.iter().enumerate() {
                let word = leaf_bits(bytes, i * 8, 8);
                match class {
                    Eightbyte::Integer => {
                        match n_int {
                            0 => ctx.rax = word,
                            _ => ctx.rdx = word,
                        }
                        n_int += 1;
                    }
                    Eightbyte::Sse => {
                        ctx.xmm[n_sse] = [word, 0];
                        n_sse += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Four doubles in `v0`–`v3`: what AAPCS64 returns a homogeneous float
/// aggregate of up to four `double`s in. Declaring the widest is enough — a
/// callee returning fewer writes fewer registers, and only its own members are
/// read back.
#[repr(C)]
#[derive(Clone, Copy)]
struct F64x4([f64; 4]);

/// As [`F64x4`], for `float` members, which occupy `s0`–`s3`.
#[repr(C)]
#[derive(Clone, Copy)]
struct F32x4([f32; 4]);

/// Two doublewords in `x0`/`x1`: a composite of at most sixteen bytes that is
/// not a homogeneous float aggregate.
#[repr(C)]
#[derive(Clone, Copy)]
struct U64x2([u64; 2]);

/// A buffer the callee fills through `x8`. Over sixteen bytes and not an HFA,
/// which is what makes AAPCS64 return it indirectly — and makes this a plain
/// out-parameter rather than anything register-shaped.
#[repr(C)]
#[derive(Clone, Copy)]
struct Indirect([u8; MAX_INDIRECT]);

type F64x4Fn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
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
) -> F64x4;

/// # Safety
/// As [`call_int`], for a callee whose return AAPCS64 places in `v0`–`v3`.
unsafe fn call_f64x4(
    host: u64,
    ints: &[u64; MAX_INT_ARGS_TOTAL],
    fps: &[f64; MAX_FP_ARGS],
) -> F64x4 {
    let f: F64x4Fn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

type F32x4Fn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
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
) -> F32x4;

/// # Safety
/// As [`call_int`], for a callee whose return AAPCS64 places in `s0`–`s3`.
unsafe fn call_f32x4(
    host: u64,
    ints: &[u64; MAX_INT_ARGS_TOTAL],
    fps: &[f64; MAX_FP_ARGS],
) -> F32x4 {
    let f: F32x4Fn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

type U64x2Fn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
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
) -> U64x2;

/// # Safety
/// As [`call_int`], for a callee whose return AAPCS64 places in `x0` and `x1`.
unsafe fn call_u64x2(
    host: u64,
    ints: &[u64; MAX_INT_ARGS_TOTAL],
    fps: &[f64; MAX_FP_ARGS],
) -> U64x2 {
    let f: U64x2Fn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}

type IndirectFn = unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
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
) -> Indirect;

/// # Safety
/// As [`call_int`], for a callee whose return AAPCS64 places in memory addressed by `x8`.
unsafe fn call_indirect(
    host: u64,
    ints: &[u64; MAX_INT_ARGS_TOTAL],
    fps: &[f64; MAX_FP_ARGS],
) -> Indirect {
    let f: IndirectFn = unsafe { core::mem::transmute(host as usize as *const ()) };
    unsafe { call_through!(f, ints, fps) }
}
