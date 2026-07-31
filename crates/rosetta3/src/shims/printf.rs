//! The `printf` family: walking a SysV variadic argument list and rendering it.
//!
//! ## Two halves
//!
//! [`VarArgs`] answers "where is the next variadic argument", which is a pure
//! ABI question: SysV AMD64 continues filling the same six integer and eight
//! vector registers the fixed arguments started on, then spills to the stack in
//! argument order. At an `ImportedFunctionCall` exit `rsp` is still the
//! pre-`CALL` value, so a spilled argument is at `rsp + offset` — see
//! [`crate::abi::marshal`] for why.
//!
//! [`render`] answers "what does this conversion print", which is a C question,
//! and is where the format string is *parsed* rather than relayed. It has to
//! be: the number and class of the variadic arguments is encoded only in that
//! string, so nothing can be fetched until it is read.
//!
//! ## Scope
//!
//! Conversions: `d i u o x X c s p f F e E g G %`, with the `- + space # 0`
//! flags, `*`-or-literal width and precision, and the `hh h l ll z j t L`
//! length modifiers. Everything else is emitted **literally**, including the
//! `%`, which is the failure mode that shows up in output rather than in a
//! register.
//!
//! Deliberately not implemented:
//!
//! - **`%n`**, which writes through a guest pointer. It is a write primitive
//!   driven by a format string; refusing is the same call every modern libc
//!   made.
//! - **Wide characters** (`%lc`, `%ls`) — treated as their narrow forms, which
//!   is wrong for non-ASCII and needs the guest's locale to be right.
//! - **Hex floats** (`%a`) and locale-dependent grouping (`%'d`).
//!
//! Rounding follows Rust's formatter, which is round-half-to-even on the
//! decimal digit — the same rule glibc and Apple's libc use for `%f`.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::{bad_pointer, set_int_return};

/// SysV AMD64 integer argument registers, in order.
const INT_REGS: usize = 6;
/// SysV AMD64 vector argument registers, in order.
const FP_REGS: usize = 8;
/// One stack argument slot.
const SLOT: u64 = 8;

/// A walk over the variadic tail of a guest call.
pub struct VarArgs<'a> {
    symbol: &'a str,
    ctx: &'a RegisterContext,
    mem: &'a dyn GuestMemory,
    int: usize,
    fp: usize,
    stack: u64,
}

impl<'a> VarArgs<'a> {
    /// Start after `fixed_int` integer arguments have already been consumed by
    /// the named parameters — one for `printf`'s format, two for `fprintf`'s
    /// stream and format, three for `snprintf`.
    pub fn new(
        symbol: &'a str,
        ctx: &'a RegisterContext,
        mem: &'a dyn GuestMemory,
        fixed_int: usize,
    ) -> Self {
        VarArgs { symbol, ctx, mem, int: fixed_int, fp: 0, stack: 0 }
    }

    fn next_stack(&mut self) -> Result<u64> {
        let addr = self.ctx.rsp.wrapping_add(self.stack);
        self.stack += SLOT;
        self.mem.read_u64(addr, self.symbol)
    }

    /// The next integer or pointer argument.
    pub fn next_int(&mut self) -> Result<u64> {
        if self.int >= INT_REGS {
            return self.next_stack();
        }
        let ctx = self.ctx;
        let v = [ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx, ctx.r8, ctx.r9][self.int];
        self.int += 1;
        Ok(v)
    }

    /// The next floating-point argument. C promotes a variadic `float` to
    /// `double`, so the register always holds a `double`.
    pub fn next_fp(&mut self) -> Result<f64> {
        if self.fp >= FP_REGS {
            return Ok(f64::from_bits(self.next_stack()?));
        }
        let v = self.ctx.xmm[self.fp][0];
        self.fp += 1;
        Ok(f64::from_bits(v))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Len {
    Char,
    Short,
    Int,
    Long,
    LongLong,
    Size,
}

#[derive(Default)]
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

/// Format `fmt` against `args`, exactly as C would print it.
///
/// Total on arbitrary bytes: an unterminated or nonsensical conversion is
/// copied out literally rather than panicking or consuming an argument, because
/// a format string is guest data and a malformed one must not be able to end
/// the process.
pub fn render(fmt: &[u8], args: &mut VarArgs<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(fmt.len());
    let mut i = 0usize;

    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;

        let mut spec = Spec::default();
        while i < fmt.len() {
            match fmt[i] {
                b'-' => spec.minus = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.hash = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            i += 1;
        }

        if i < fmt.len() && fmt[i] == b'*' {
            i += 1;
            let w = args.next_int()? as u32 as i32;
            if w < 0 {
                spec.minus = true;
                spec.width = w.unsigned_abs() as usize;
            } else {
                spec.width = w as usize;
            }
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                spec.width = spec.width.saturating_mul(10) + usize::from(fmt[i] - b'0');
                i += 1;
            }
        }

        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            if i < fmt.len() && fmt[i] == b'*' {
                i += 1;
                let p = args.next_int()? as u32 as i32;
                // A negative `*` precision is as if it were omitted.
                spec.prec = (p >= 0).then_some(p as usize);
            } else {
                let mut p = 0usize;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    p = p.saturating_mul(10) + usize::from(fmt[i] - b'0');
                    i += 1;
                }
                spec.prec = Some(p);
            }
        }

        let mut len = Len::Int;
        loop {
            match fmt.get(i) {
                Some(b'h') => {
                    len = if len == Len::Short { Len::Char } else { Len::Short };
                    i += 1;
                }
                Some(b'l') => {
                    len = if len == Len::Long { Len::LongLong } else { Len::Long };
                    i += 1;
                }
                // `z`, `j`, `t` are all 64-bit here; `L` only qualifies a
                // `long double`, which arrives promoted like any other double.
                Some(b'z') | Some(b'j') | Some(b't') => {
                    len = Len::Size;
                    i += 1;
                }
                Some(b'L') => i += 1,
                _ => break,
            }
        }

        let Some(&conv) = fmt.get(i) else {
            // A format string ending mid-conversion: emit what was there.
            out.extend_from_slice(&fmt[start..]);
            break;
        };
        i += 1;

        match conv {
            b'%' => out.push(b'%'),
            b'd' | b'i' => {
                let v = sign_extend(args.next_int()?, len);
                let neg = v < 0;
                let body = digits_with_precision(v.unsigned_abs(), 10, false, spec.prec);
                pad_number(&mut out, &body, neg, &spec, "", spec.prec);
            }
            b'u' | b'o' | b'x' | b'X' => {
                let v = zero_extend(args.next_int()?, len);
                let (base, upper) = match conv {
                    b'o' => (8, false),
                    b'x' => (16, false),
                    b'X' => (16, true),
                    _ => (10, false),
                };
                let prefix = match (spec.hash, conv, v) {
                    (true, _, 0) => "",
                    (true, b'o', _) => "0",
                    (true, b'x', _) => "0x",
                    (true, b'X', _) => "0X",
                    _ => "",
                };
                let body = digits_with_precision(v, base, upper, spec.prec);
                pad_number(&mut out, &body, false, &spec, prefix, spec.prec);
            }
            b'c' => {
                let v = args.next_int()? as u8;
                pad_bytes(&mut out, &[v], &spec);
            }
            b's' => {
                let ptr = args.next_int()?;
                // Read to the NUL first and truncate after. C allows `%.3s` on
                // a three-byte unterminated buffer; this reads further, which
                // is why `read_cstr` is bounded rather than trusting.
                let mut s = read_string(args, ptr)?;
                if let Some(p) = spec.prec {
                    s.truncate(p);
                }
                pad_bytes(&mut out, &s, &spec);
            }
            b'p' => {
                let v = args.next_int()?;
                // Apple's libc prints `0x0` for null and lower-case hex
                // otherwise, with no padding of its own.
                let body = format!("0x{}", String::from_utf8_lossy(&digits(v, 16, false)));
                let mut spec = Spec { prec: None, ..spec };
                spec.zero = false;
                pad_bytes(&mut out, body.as_bytes(), &spec);
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let v = args.next_fp()?;
                let (body, neg) = format_float(v, conv, &spec);
                pad_number(&mut out, body.as_bytes(), neg, &spec, "", None);
            }
            // Everything unhandled — `%n` included — comes out verbatim and
            // fetches no argument of its own, so the rest of the format still
            // lines up with the rest of the arguments. (A `*` width earlier in
            // the same conversion has already consumed one, which is what C
            // would have done too.)
            _ => out.extend_from_slice(&fmt[start..i]),
        }
    }
    Ok(out)
}

/// Follow a `%s` pointer into guest memory.
fn read_string(args: &VarArgs<'_>, ptr: u64) -> Result<Vec<u8>> {
    if ptr == 0 {
        // What both glibc and Apple's libc do rather than fault.
        return Ok(b"(null)".to_vec());
    }
    args.mem.read_cstr(ptr, args.symbol)
}

fn sign_extend(raw: u64, len: Len) -> i64 {
    match len {
        Len::Char => raw as u8 as i8 as i64,
        Len::Short => raw as u16 as i16 as i64,
        Len::Int => raw as u32 as i32 as i64,
        Len::Long | Len::LongLong | Len::Size => raw as i64,
    }
}

fn zero_extend(raw: u64, len: Len) -> u64 {
    match len {
        Len::Char => u64::from(raw as u8),
        Len::Short => u64::from(raw as u16),
        Len::Int => u64::from(raw as u32),
        Len::Long | Len::LongLong | Len::Size => raw,
    }
}

/// As [`digits`], except that zero with an explicit precision of zero prints
/// *nothing* — the one place C's integer precision changes the digits rather
/// than padding them.
fn digits_with_precision(v: u64, base: u64, upper: bool, prec: Option<usize>) -> Vec<u8> {
    if v == 0 && prec == Some(0) {
        return Vec::new();
    }
    digits(v, base, upper)
}

fn digits(mut v: u64, base: u64, upper: bool) -> Vec<u8> {
    if v == 0 {
        return vec![b'0'];
    }
    let alphabet: &[u8] = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut out = Vec::new();
    while v > 0 {
        out.push(alphabet[(v % base) as usize]);
        v /= base;
    }
    out.reverse();
    out
}

/// Render a float's magnitude, C-style, returning `(body, negative)`.
///
/// The sign is split out so [`pad_number`] can place it before any zero
/// padding, which is where C puts it.
fn format_float(v: f64, conv: u8, spec: &Spec) -> (String, bool) {
    let upper = conv.is_ascii_uppercase();
    let neg = v.is_sign_negative();
    let mag = v.abs();

    if mag.is_nan() {
        return (if upper { "NAN".into() } else { "nan".into() }, neg);
    }
    if mag.is_infinite() {
        return (if upper { "INF".into() } else { "inf".into() }, neg);
    }

    let prec = spec.prec.unwrap_or(6);
    let body = match conv {
        b'f' | b'F' => format!("{mag:.prec$}"),
        b'e' | b'E' => exponential(mag, prec, upper),
        _ => {
            // %g: C chooses %e when the exponent is < -4 or >= the precision,
            // and strips trailing zeros unless `#` was given.
            let p = prec.max(1);
            let exp = decimal_exponent(mag, p);
            let mut s = if exp < -4 || exp >= p as i32 {
                exponential(mag, p - 1, upper)
            } else {
                let decimals = (p as i32 - 1 - exp).max(0) as usize;
                format!("{mag:.decimals$}")
            };
            if !spec.hash {
                s = strip_trailing_zeros(&s);
            }
            s
        }
    };
    (body, neg)
}

/// `%e`: C requires at least two exponent digits, Rust's `{:e}` emits the
/// fewest, so the exponent is rebuilt rather than patched.
fn exponential(mag: f64, prec: usize, upper: bool) -> String {
    let exp = decimal_exponent(mag, prec + 1);
    let scaled = if mag == 0.0 { 0.0 } else { mag / powi10(exp) };
    // Rounding the mantissa can carry it to 10.0 (9.99 at precision 1); when it
    // does, the exponent is one larger and the mantissa is 1.0.
    let (mantissa, exp) = {
        let rounded = format!("{scaled:.prec$}");
        if rounded.starts_with("10") { (format!("{:.prec$}", scaled / 10.0), exp + 1) } else { (rounded, exp) }
    };
    let e = if upper { 'E' } else { 'e' };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}{e}{sign}{:02}", exp.abs())
}

/// The base-10 exponent of `mag` rounded to `sig` significant digits.
///
/// Computed from the printed form rather than from `log10`, because `log10` is
/// off by one at exact powers of ten often enough to matter and the rounding
/// has to agree with what is printed anyway.
fn decimal_exponent(mag: f64, sig: usize) -> i32 {
    if mag == 0.0 || !mag.is_finite() {
        return 0;
    }
    let s = format!("{:.*e}", sig.saturating_sub(1), mag);
    s.split('e').nth(1).and_then(|e| e.parse::<i32>().ok()).unwrap_or(0)
}

fn powi10(exp: i32) -> f64 {
    10f64.powi(exp)
}

fn strip_trailing_zeros(s: &str) -> String {
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    if !mantissa.contains('.') {
        return s.to_string();
    }
    let trimmed = mantissa.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}{exp}")
}

/// Place sign, prefix, precision zeros and field padding around a number.
///
/// `min_digits` is an *integer* conversion's precision, which is a minimum
/// digit count and suppresses the `0` flag. A float's precision counts decimal
/// places instead and does not suppress it, so it passes `None` — the two rules
/// look alike and are not, and conflating them zero-pads `%012.3f` with spaces.
fn pad_number(
    out: &mut Vec<u8>,
    body: &[u8],
    negative: bool,
    spec: &Spec,
    prefix: &str,
    min_digits: Option<usize>,
) {
    let sign: &[u8] = if negative {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    };

    let zeros = min_digits.map_or(0, |p| p.saturating_sub(body.len()));
    let zero_pad = spec.zero && min_digits.is_none();
    let fixed = sign.len() + prefix.len() + zeros + body.len();
    let pad = spec.width.saturating_sub(fixed);

    if !spec.minus && !zero_pad {
        out.extend(core::iter::repeat_n(b' ', pad));
    }
    out.extend_from_slice(sign);
    out.extend_from_slice(prefix.as_bytes());
    if !spec.minus && zero_pad {
        out.extend(core::iter::repeat_n(b'0', pad));
    }
    out.extend(core::iter::repeat_n(b'0', zeros));
    out.extend_from_slice(body);
    if spec.minus {
        out.extend(core::iter::repeat_n(b' ', pad));
    }
}

/// Field padding for `%c`, `%s` and `%p`, which never take a sign or zeros.
fn pad_bytes(out: &mut Vec<u8>, body: &[u8], spec: &Spec) {
    let pad = spec.width.saturating_sub(body.len());
    if !spec.minus {
        out.extend(core::iter::repeat_n(b' ', pad));
    }
    out.extend_from_slice(body);
    if spec.minus {
        out.extend(core::iter::repeat_n(b' ', pad));
    }
}

/// The host stream a guest `printf` writes to.
///
/// The guest's `stdout` *is* the host's: under high-level emulation the guest's
/// `___stdoutp` import resolved to the host's own, so writing through the host's
/// stdio keeps the guest's output interleaved correctly with anything the guest
/// wrote through a passed-through `fwrite`.
fn host_stdout() -> *mut libc::FILE {
    // Declared here rather than taken from the `libc` crate, which does not
    // export it: on macOS the `stdout` macro expands to this global, and it is
    // the same object the guest's own `___stdoutp` import resolved to.
    unsafe extern "C" {
        static __stdoutp: *mut libc::FILE;
    }
    // SAFETY: reading a pointer-sized global libSystem initializes before any
    // user code runs, so it is valid by the time this process has a `main`.
    unsafe { __stdoutp }
}

fn write_host(stream: *mut libc::FILE, bytes: &[u8]) {
    if stream.is_null() || bytes.is_empty() {
        return;
    }
    // SAFETY: `bytes` is a live slice of `len` bytes and `stream` is a host
    // `FILE*` — either the host's own stdout or a pointer the guest obtained
    // from a passed-through `fopen`/`___stdoutp`, both of which are host
    // pointers by construction of the native provider.
    unsafe {
        libc::fwrite(bytes.as_ptr().cast(), 1, bytes.len(), stream);
    }
}

/// `int printf(const char *fmt, ...)`
pub fn printf(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let out = format_from(symbol, ctx, &*mem, ctx.rdi, 1)?;
    write_host(host_stdout(), &out);
    set_int_return(ctx, out.len() as i32);
    Ok(())
}

/// `int fprintf(FILE *stream, const char *fmt, ...)`
pub fn fprintf(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let out = format_from(symbol, ctx, &*mem, ctx.rsi, 2)?;
    write_host(ctx.rdi as usize as *mut libc::FILE, &out);
    set_int_return(ctx, out.len() as i32);
    Ok(())
}

/// `int sprintf(char *dst, const char *fmt, ...)`
pub fn sprintf(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let mut out = format_from(symbol, ctx, &*mem, ctx.rsi, 2)?;
    let written = out.len();
    out.push(0);
    mem.write(ctx.rdi, &out).map_err(|_| bad_pointer(symbol, ctx.rdi))?;
    set_int_return(ctx, written as i32);
    Ok(())
}

/// `int snprintf(char *dst, size_t n, const char *fmt, ...)`
///
/// Returns the length the *whole* rendering would have had, per C — which is
/// why the truncation happens after the render and not during it.
pub fn snprintf(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let out = format_from(symbol, ctx, &*mem, ctx.rdx, 3)?;
    store_bounded(symbol, ctx, mem, ctx.rdi, ctx.rsi, out)
}

/// `int __sprintf_chk(char *dst, int flag, size_t dstlen, const char *fmt, ...)`
///
/// The fortified form. `dstlen` is the compile-time size of the destination;
/// the real implementation aborts on overflow, and so does this — but by
/// truncating and reporting the would-be length rather than by calling
/// `__chk_fail`, because the guest's abort path is guest code we cannot yet
/// enter. A truncated string is a visible bug; a silent overflow is not.
pub fn sprintf_chk(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let out = format_from(symbol, ctx, &*mem, ctx.rcx, 4)?;
    store_bounded(symbol, ctx, mem, ctx.rdi, ctx.rdx, out)
}

/// `int __snprintf_chk(char *dst, size_t n, int flag, size_t dstlen, const char *fmt, ...)`
pub fn snprintf_chk(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let out = format_from(symbol, ctx, &*mem, ctx.r8, 5)?;
    let limit = ctx.rsi.min(ctx.rcx);
    store_bounded(symbol, ctx, mem, ctx.rdi, limit, out)
}

/// Write `out` to `dst`, truncated to `limit` bytes including the terminator,
/// and leave the length the whole rendering would have had in `rax`.
fn store_bounded(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    dst: u64,
    limit: u64,
    out: Vec<u8>,
) -> Result<()> {
    let would_be = out.len();
    if limit > 0 {
        let keep = out.len().min((limit - 1) as usize);
        let mut buf = out[..keep].to_vec();
        buf.push(0);
        mem.write(dst, &buf).map_err(|_| bad_pointer(symbol, dst))?;
    }
    set_int_return(ctx, would_be as i32);
    Ok(())
}

/// Read the format string at `fmt_ptr` and render it, with `fixed_int` integer
/// registers already spoken for by named parameters.
fn format_from(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &dyn GuestMemory,
    fmt_ptr: u64,
    fixed_int: usize,
) -> Result<Vec<u8>> {
    if fmt_ptr == 0 {
        return Err(bad_pointer(symbol, 0));
    }
    let fmt = mem.read_cstr(fmt_ptr, symbol)?;
    let mut args = VarArgs::new(symbol, ctx, mem, fixed_int);
    render(&fmt, &mut args)
}
