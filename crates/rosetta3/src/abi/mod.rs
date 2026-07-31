//! The ABI seam: what it takes to *call* an intercepted import.
//!
//! Milestone 1 left an import as an address and nothing else. That is enough to
//! stop the recompiler walking into a `__stubs` entry, and not nearly enough to
//! service the resulting exit: at an `ExitReason::ImportedFunctionCall` the
//! runtime holds a guest register context and a host function pointer, and has
//! to decide which of those registers are arguments, of what type, and where
//! the result goes. No Mach-O records that. A symbol table has a name and no
//! types, so the information has to come from a table the embedder writes —
//! which is exactly what [`machish::report::BindingRecord::tag`] exists to
//! index, and why `function_traits` enters here and never in `machish`.
//!
//! ## Refusing is the design decision
//!
//! The tempting default is a guess: forward six integer registers and eight
//! vector registers, take an integer return. It is right for most of libSystem
//! and silently wrong for anything passing or returning a struct by value, and
//! *wrong here does not fault* — the host reads a register that happens to hold
//! something, and the guest gets plausible garbage back. A wrong answer that
//! looks like a right one is worse than no answer, so an import with no
//! signature is **refused by name** ([`AbiError::NoSignature`]) and the guest
//! register context is left untouched. See [`table`] for what is known and what
//! is deliberately refused.
//!
//! ## Two consumers, one table
//!
//! - [`crate::Program::lifter_config`] promotes an import with a known
//!   non-variadic signature to `LifterConfig::register_fast_import`, so the
//!   lifter inlines the call and reads arguments from wherever the *guest's*
//!   ABI put them. No VM exit at all.
//! - [`call_import`] services the slow path for everything else.
//!
//! Fast is an optimisation of slow: both read the same [`FunctionAbi`], and a
//! slow import is *correct*, merely slower. That is why the marshaller here is
//! the thing tested against real host functions — if it is right, promoting an
//! import cannot change behaviour, only cost.
//!
//! ## The guest's convention, not the host's
//!
//! [`guest_call_abi`] is SysV AMD64 because the *guest binary* was compiled for
//! it. The host is arm64 and its convention is Cranelift's business (fast path)
//! or [`marshal`]'s (slow path). Keeping the two named separately is what makes
//! the seam readable.

use core::fmt;

use machish::memory::AddressSpace;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::abi::CallAbi;
pub use microlift::function_traits::{DynamicSymbol, FunctionArgumentType as ArgType};

pub mod marshal;
pub mod table;

pub use table::{FunctionAbi, Lookup};

/// The convention the guest passes arguments in.
///
/// A property of the binary being translated — an Intel macOS executable is
/// SysV AMD64 — and not of the machine this runs on. Named once here so the
/// lifter registration and the slow-path marshaller cannot disagree about it.
pub fn guest_call_abi() -> CallAbi {
    CallAbi::sysv_amd64()
}

/// Anything that stops an intercepted call from being made.
///
/// Every variant names the symbol, because the whole point of refusing is that
/// the failure is legible: "`_fts_open$INODE64` has no signature" is a work
/// item, and a corrupted register is a week of bisecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiError {
    /// Nothing in [`table`] describes this symbol.
    NoSignature { symbol: String },
    /// Known, and deliberately not callable — see the reason.
    Refused { symbol: String, reason: &'static str },
    /// Variadic, and no shim implements it. Variadics cannot be forwarded
    /// generically: SysV AMD64 passes them in registers with `al` holding the
    /// vector-register count, while Apple's arm64 ABI passes every variadic
    /// argument on the stack. See [`crate::shims`].
    NoShim { symbol: String },
    /// The signature is known but this marshaller cannot express the call:
    /// more than eight integer or eight floating-point arguments.
    Unmarshallable { symbol: String, reason: &'static str },
    /// The import resolved to nothing — an unsatisfied weak import, whose slot
    /// holds zero. Calling it would jump to null.
    Unsatisfied { symbol: String },
    /// A guest memory access failed: a stack argument or a string argument
    /// pointing outside anything mapped.
    Memory { symbol: String, addr: u64 },
}

impl AbiError {
    /// The symbol the failure is about. Always present, by construction.
    pub fn symbol(&self) -> &str {
        match self {
            AbiError::NoSignature { symbol }
            | AbiError::Refused { symbol, .. }
            | AbiError::NoShim { symbol }
            | AbiError::Unmarshallable { symbol, .. }
            | AbiError::Unsatisfied { symbol }
            | AbiError::Memory { symbol, .. } => symbol,
        }
    }
}

impl fmt::Display for AbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AbiError::NoSignature { symbol } => {
                write!(f, "{symbol}: no signature; add one to rosetta3::abi::table")
            }
            AbiError::Refused { symbol, reason } => write!(f, "{symbol}: not callable: {reason}"),
            AbiError::NoShim { symbol } => {
                write!(f, "{symbol}: variadic, and no shim implements it")
            }
            AbiError::Unmarshallable { symbol, reason } => {
                write!(f, "{symbol}: cannot be marshalled: {reason}")
            }
            AbiError::Unsatisfied { symbol } => {
                write!(f, "{symbol}: unsatisfied import, resolved to null")
            }
            AbiError::Memory { symbol, addr } => {
                write!(f, "{symbol}: guest memory at {addr:#x} is not readable")
            }
        }
    }
}

impl std::error::Error for AbiError {}

/// Result of an intercepted call.
pub type Result<T> = core::result::Result<T, AbiError>;

/// The guest's memory, as the marshaller and the shims need it.
///
/// Narrower than [`AddressSpace`] on purpose: nothing on this path reserves or
/// protects, and stating that keeps a shim from quietly gaining the ability to
/// re-map the guest. Errors carry the symbol being serviced, which
/// [`AddressSpace`]'s own error type has no room for, so this is also where the
/// two error vocabularies meet.
pub trait GuestMemory {
    fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error>;
    fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error>;
}

impl<S: AddressSpace> GuestMemory for S {
    fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
        AddressSpace::read(self, va, out)
    }

    fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
        AddressSpace::write(self, va, data)
    }
}

/// Longest C string a shim will follow before giving up.
///
/// Totality, not policy: a `%s` argument pointing at unterminated bytes must
/// end the read rather than the process. Anything legitimately longer than this
/// is a bug in the guest or a wrong pointer.
pub const MAX_CSTR: usize = 64 * 1024;

impl dyn GuestMemory + '_ {
    /// Read one 64-bit little-endian word.
    pub fn read_u64(&self, va: u64, symbol: &str) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.read(va, &mut buf)
            .map_err(|_| AbiError::Memory { symbol: symbol.to_string(), addr: va })?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Read a NUL-terminated string, capped at [`MAX_CSTR`] bytes.
    ///
    /// Reads a byte at a time rather than a block, because the string may end
    /// one byte before the end of a mapped region and a block read would fail
    /// on bytes the guest never asked about.
    pub fn read_cstr(&self, va: u64, symbol: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for i in 0..MAX_CSTR as u64 {
            let mut b = [0u8; 1];
            self.read(va.wrapping_add(i), &mut b)
                .map_err(|_| AbiError::Memory { symbol: symbol.to_string(), addr: va })?;
            if b[0] == 0 {
                return Ok(out);
            }
            out.push(b[0]);
        }
        Ok(out)
    }
}

/// Service one intercepted call.
///
/// `symbol` is the imported name with its leading underscore, as the Mach-O
/// spells it; `tag` is the provider's verbatim echo (see the crate note on why
/// the seam is a `u64`); `host` is the address the import resolved to. On
/// success the guest's return-value register has been written and `ctx` is
/// ready to resume at its `rip`.
///
/// **On any error `ctx` is unmodified.** That is the guarantee that makes
/// refusal useful: a missing signature costs a diagnostic, not a wrong value in
/// `rax` that surfaces three functions later.
pub fn call_import(
    symbol: &str,
    tag: u64,
    host: u64,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    call_import_with(symbol, tag, host, ctx, mem, &NoCallbacks)
}

/// [`call_import`], with somewhere for a guest function pointer to go.
///
/// The extra parameter is what makes `qsort` callable. Its argument is a guest
/// address the host would call as arm64, so it has to be replaced with a thunk
/// — and building one means entering the JIT, which is knowledge this module
/// must not have. Hence a trait: the ABI layer knows *which* arguments are
/// callbacks (from [`FunctionAbi::callbacks`]) and the VM knows what to do
/// about them.
pub fn call_import_with(
    symbol: &str,
    tag: u64,
    host: u64,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    if host == 0 {
        return Err(AbiError::Unsatisfied { symbol: symbol.to_string() });
    }
    // Shims first, unconditionally: a shim exists precisely because passing the
    // call through would be wrong, so it must win over any table entry.
    if let Some(result) = crate::shims::dispatch(symbol, ctx, mem) {
        return result;
    }
    match table::lookup(tag, symbol) {
        Lookup::Known(abi) if abi.variadic => Err(AbiError::NoShim { symbol: symbol.to_string() }),
        Lookup::Known(abi) => marshal::call_with(symbol, abi, host, ctx, mem, callbacks),
        Lookup::Refused(reason) => {
            Err(AbiError::Refused { symbol: symbol.to_string(), reason })
        }
        Lookup::Unknown => Err(AbiError::NoSignature { symbol: symbol.to_string() }),
    }
}

/// How a guest function pointer becomes something the host can call.
///
/// Implemented by [`crate::vm::Vm`]; see [`crate::thunk`] for what the answer
/// actually is.
pub trait Callbacks {
    /// What to pass the host in place of the guest's function-pointer argument
    /// `va`, while servicing `symbol`.
    ///
    /// One method rather than a "is it guest code" predicate and a builder,
    /// because the decision and the substitution have to be made together: an
    /// implementation that cannot build a thunk must **refuse**, not pass the
    /// pointer through. A null stays null (`atexit(NULL)` is the guest's error
    /// to have), and a pointer that is already a host address is passed
    /// unchanged — a program may hand back a callback it was given, and
    /// wrapping that would break the pointer equality the host can observe.
    fn substitute(&self, symbol: &str, va: u64) -> Result<u64>;
}

/// The bridge for a caller that has no JIT to re-enter — the marshaller's own
/// tests, and [`call_import`] itself.
///
/// It refuses rather than passing the pointer through, which keeps the M2
/// behaviour exactly: a callback-taking import is a diagnostic naming the
/// symbol, not a host calling x86-64 as arm64.
pub struct NoCallbacks;

impl Callbacks for NoCallbacks {
    fn substitute(&self, symbol: &str, va: u64) -> Result<u64> {
        if va == 0 {
            return Ok(0);
        }
        Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "takes a guest function pointer, and this caller has no JIT to re-enter",
        })
    }
}

/// How an import will be reached, once classified.
///
/// The summary the driver prints, and the shape of the M2 acceptance question:
/// every import of a program must be one of these and none may be
/// [`Classification::Unknown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classification {
    /// Signature known and marshallable: inlined by the lifter, no VM exit.
    Fast,
    /// Serviced at an `ImportedFunctionCall` exit — a variadic with a shim, or
    /// a signature this marshaller cannot inline.
    Slow,
    /// Known and deliberately not callable: data read through its slot, or a
    /// callback whose shape the host→guest thunk cannot carry (M4, M5).
    Refused(&'static str),
    /// Nothing describes it. The one outcome the acceptance test forbids.
    Unknown,
}

/// Decide how `symbol` will be reached, without calling anything.
pub fn classify(tag: u64, symbol: &str) -> Classification {
    // A shimmed symbol is slow whatever its signature says, because
    // [`crate::ImportPolicy`] refuses to promote one: `setjmp` has an ordinary
    // `int(void *)` signature and inlining it would call the *host's*. Asked
    // here as well as there so the audit reports how an import will really be
    // reached rather than how its types would allow.
    if crate::shims::is_shimmed(symbol) {
        return Classification::Slow;
    }
    match table::lookup(tag, symbol) {
        Lookup::Known(abi) if abi.variadic => Classification::Slow,
        // A fast import is inlined by the lifter, which passes the guest's
        // registers straight through — including the function pointer that has
        // to become a thunk first. Substitution only exists at an exit, so a
        // callback taker is slow by construction, not by preference.
        Lookup::Known(abi) if !abi.callbacks.is_empty() => Classification::Slow,
        Lookup::Known(abi) => {
            if guest_call_abi().can_fast_call(abi.types) && marshal::is_marshallable(abi) {
                Classification::Fast
            } else {
                Classification::Slow
            }
        }
        Lookup::Refused(reason) => Classification::Refused(reason),
        Lookup::Unknown => Classification::Unknown,
    }
}

/// The `DynamicSymbol` microlift wants for a fast import.
///
/// `name` is `&'static str` on microlift's side and a `String` on `machish`'s
/// (see the crate note) — the table's own static name is what bridges them, and
/// is the reason a fast import is reached through the table rather than through
/// the binding record's string.
pub fn dynamic_symbol(abi: &FunctionAbi, host: u64) -> DynamicSymbol {
    DynamicSymbol {
        name: abi.name,
        value: host as usize as *mut core::ffi::c_void,
        arg_types: abi.types.to_vec(),
        is_ellipsis_function: abi.variadic,
        has_execution_side_effects: false,
    }
}
