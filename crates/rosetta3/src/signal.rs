//! Signal handlers: a guest function the *kernel* calls.
//!
//! ## Why this is a shim and not a table entry
//!
//! `signal` and `sigaction` take a guest function pointer, which since M3 is an
//! ordinary thing to hand out: it leaves as a [`crate::thunk`], exactly like a
//! `qsort` comparator or a `pthread_create` start routine. What makes signals
//! different is not the outbound pointer but the *inbound* one — a handler is
//! also returned, in `signal`'s result and in `sigaction`'s `oldact`, and a
//! program that installs a handler, saves the old one, and restores it later
//! would otherwise get one of this runtime's arm64 thunks written into its own
//! `struct sigaction` and hand it back as if it were its own function. So the
//! substitution has to run in both directions, which is what a shim is for.
//!
//! The `struct sigaction` itself needs no translation: it is a pointer, a
//! 4-byte `sigset_t` and a 4-byte flag word — sixteen bytes with identical
//! layout on x86-64 and arm64 macOS, which `tests/signal.rs` checks by compiling
//! the same `sizeof` program for both architectures rather than trusting it.
//! Only the handler field is rewritten, and never in the guest's own struct: the
//! guest may read its copy back.
//!
//! ## `SA_SIGINFO` is refused, and the reason is not laziness
//!
//! A three-argument handler is called with a `ucontext_t` describing **the
//! machine state at the point the signal interrupted**. On this runtime that
//! point is inside a Cranelift-compiled arm64 translation, and the guest's
//! x86-64 registers at that instruction are not recoverable: they live in
//! whatever registers or stack slots the compiler chose for that block, with no
//! map back to `rax`, `rsp` and the rest. Synthesizing a plausible `mcontext64`
//! would be exactly the failure this project refuses everywhere else — a wrong
//! answer that does not fault.
//!
//! So installing an `SA_SIGINFO` handler stops with a named reason. Recovering
//! it is a *recompiler* work item (a deopt map from a translation's PC back to
//! the guest register context), not a runtime one, which is the useful thing for
//! a refusal to say.
//!
//! ## Two hazards a program can reach, written down rather than guarded
//!
//! - **Delivery is asynchronous, and the JIT is not.** A handler thunk re-enters
//!   the VM, which takes the code-cache lock to look a translation up. A signal
//!   arriving on a thread that is *already* inside that lookup would deadlock
//!   against itself. Translating the handler when its thunk is built (which
//!   [`crate::Vm::thunk_for`] already does for every callback) means the common
//!   path only ever takes the lock to find something already there, which
//!   narrows the window and does not close it. Closing it means a lock-free
//!   cache lookup.
//! - **A handler that `longjmp`s out** — the classic `SIGALRM` timeout idiom —
//!   jumps across the kernel's signal trampoline and every host frame under it,
//!   which [`crate::jmpbuf`] refuses by name for the same reason a `longjmp` out
//!   of a `qsort` comparator is refused.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, Callbacks, GuestMemory, Result};

/// `sa_flags` bit asking for the three-argument handler. See the module note.
const SA_SIGINFO: u32 = 0x0040;

/// The dispositions that are not addresses.
///
/// `SIG_DFL` (0), `SIG_IGN` (1) and `SIG_ERR` (-1) are sentinels the kernel
/// interprets, not function pointers, and wrapping one in a thunk would install
/// a handler where the guest asked for the default. They are also the values a
/// guest most often *compares* a returned disposition against.
fn is_sentinel(value: u64) -> bool {
    value == 0 || value == 1 || value as i64 == -1
}

/// `struct sigaction` as both architectures lay it out.
///
/// Sixteen bytes: a handler, a 4-byte mask, a 4-byte flag word. Declared here
/// rather than taken from `libc` because what matters is that this is the
/// *guest's* layout, and the claim that the host's is the same one is a test.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigAction {
    handler: u64,
    mask: u32,
    flags: u32,
}

impl SigAction {
    const SIZE: usize = 16;

    fn from_bytes(b: [u8; Self::SIZE]) -> Self {
        SigAction {
            handler: u64::from_le_bytes(b[0..8].try_into().unwrap_or_default()),
            mask: u32::from_le_bytes(b[8..12].try_into().unwrap_or_default()),
            flags: u32::from_le_bytes(b[12..16].try_into().unwrap_or_default()),
        }
    }

    fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..8].copy_from_slice(&self.handler.to_le_bytes());
        out[8..12].copy_from_slice(&self.mask.to_le_bytes());
        out[12..16].copy_from_slice(&self.flags.to_le_bytes());
        out
    }
}

/// A guest handler on its way *out*: a thunk, unless it is a sentinel.
fn outbound(symbol: &str, handler: u64, callbacks: &dyn Callbacks) -> Result<u64> {
    if is_sentinel(handler) {
        return Ok(handler);
    }
    callbacks.substitute(symbol, handler)
}

/// A handler on its way *back in*: the guest address behind a thunk, if it is
/// one of ours.
///
/// A disposition the guest never installed — one this process set up before the
/// guest ran — is returned unchanged, because it is what the kernel will really
/// call and the guest can only ever pass it back.
fn inbound(handler: u64, callbacks: &dyn Callbacks) -> u64 {
    if is_sentinel(handler) {
        return handler;
    }
    callbacks.unsubstitute(handler)
}

/// `void (*signal(int sig, void (*handler)(int)))(int)`
pub fn signal(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let signum = ctx.rdi as u32 as i32;
    let handler = outbound(symbol, ctx.rsi, callbacks)?;

    // SAFETY: `signal` takes an `int` and a function pointer the host will call
    // as arm64 — which is what `outbound` just guaranteed it is, either a thunk
    // this runtime compiled or one of the kernel's own sentinels.
    let previous = unsafe { libc::signal(signum, handler as libc::sighandler_t) } as u64;
    ctx.rax = inbound(previous, callbacks);
    Ok(())
}

/// `int sigaction(int sig, const struct sigaction *act, struct sigaction *oldact)`
pub fn sigaction(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let signum = ctx.rdi as u32 as i32;
    let (act_va, old_va) = (ctx.rsi, ctx.rdx);

    // The guest's own struct is never rewritten: it may read its copy back, and
    // finding one of this runtime's thunks in the field it wrote its own
    // function into would be a lie it could observe.
    let mut act = None;
    if act_va != 0 {
        let mut bytes = [0u8; SigAction::SIZE];
        mem.read(act_va, &mut bytes)
            .map_err(|_| AbiError::Memory { symbol: symbol.to_string(), addr: act_va })?;
        let mut a = SigAction::from_bytes(bytes);
        if a.flags & SA_SIGINFO != 0 {
            return Err(AbiError::Refused {
                symbol: symbol.to_string(),
                reason: "SA_SIGINFO: a handler taking a ucontext_t needs the guest's registers \
                         at an interrupted PC, which the recompiler cannot yet report (M5)",
            });
        }
        a.handler = outbound(symbol, a.handler, callbacks)?;
        act = Some(a);
    }

    let mut old = SigAction::default();
    // SAFETY: both pointers are to live, correctly sized local structures with
    // the layout this platform's `sigaction` expects — the same sixteen bytes on
    // either architecture, which `tests/signal.rs` checks by compilation rather
    // than by assertion.
    let rc = unsafe {
        libc::sigaction(
            signum,
            act.as_ref().map_or(std::ptr::null(), |a| a as *const SigAction).cast(),
            if old_va == 0 { std::ptr::null_mut() } else { (&raw mut old).cast() },
        )
    };

    if rc == 0 && old_va != 0 {
        old.handler = inbound(old.handler, callbacks);
        mem.write(old_va, &old.to_bytes())
            .map_err(|_| AbiError::Memory { symbol: symbol.to_string(), addr: old_va })?;
    }
    crate::shims::set_int_return(ctx, rc);
    Ok(())
}

/// One of this module's implementations, in the shape [`crate::shims`] wants.
///
/// Wider than [`crate::jmpbuf::Shim`] by one parameter, and that parameter is
/// the whole reason these are here: a signal disposition is a guest function
/// pointer crossing in both directions.
pub type Shim =
    fn(&str, &mut RegisterContext, &mut dyn GuestMemory, &dyn Callbacks) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    Some(match name {
        "signal" => signal,
        "sigaction" => sigaction,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what crossed, and pretends a thunk is the guest address with its
    /// top bit set — enough to tell the two directions apart in an assertion.
    struct Wrapping;

    const WRAP: u64 = 0x8000_0000_0000_0000;

    impl Callbacks for Wrapping {
        fn substitute(&self, _symbol: &str, va: u64) -> Result<u64> {
            Ok(va | WRAP)
        }

        fn unsubstitute(&self, va: u64) -> u64 {
            va & !WRAP
        }
    }

    struct Bytes {
        base: u64,
        bytes: Vec<u8>,
    }

    impl GuestMemory for Bytes {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            let off = (va.wrapping_sub(self.base)) as usize;
            let end = off.checked_add(out.len());
            match end.filter(|e| *e <= self.bytes.len()) {
                Some(e) => {
                    out.copy_from_slice(&self.bytes[off..e]);
                    Ok(())
                }
                None => Err(machish::Error::OutOfBounds { addr: va, len: out.len() }),
            }
        }

        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            let off = (va.wrapping_sub(self.base)) as usize;
            let end = off.checked_add(data.len());
            match end.filter(|e| *e <= self.bytes.len()) {
                Some(e) => {
                    self.bytes[off..e].copy_from_slice(data);
                    Ok(())
                }
                None => Err(machish::Error::OutOfBounds { addr: va, len: data.len() }),
            }
        }
    }

    fn guest_memory() -> Bytes {
        Bytes { base: 0x3_0000_0000, bytes: vec![0; 64] }
    }

    #[test]
    fn the_struct_is_sixteen_bytes_of_the_shape_both_architectures_use() {
        assert_eq!(std::mem::size_of::<SigAction>(), SigAction::SIZE);
        assert_eq!(std::mem::size_of::<SigAction>(), std::mem::size_of::<libc::sigaction>());
        // Round-tripping is what the shim relies on when it rewrites one field
        // and leaves the mask and flags exactly as the guest wrote them.
        let a = SigAction { handler: 0x1_0000_1234, mask: 0xdead_beef, flags: 0x1234_5678 };
        let b = SigAction::from_bytes(a.to_bytes());
        assert_eq!((b.handler, b.mask, b.flags), (a.handler, a.mask, a.flags));
    }

    #[test]
    fn the_kernels_sentinels_are_never_wrapped_in_a_thunk() {
        // `SIG_DFL`, `SIG_IGN` and `SIG_ERR` are dispositions, not addresses. A
        // thunk built for `SIG_IGN` would install a handler where the guest
        // asked for the signal to be ignored — and it would run.
        for sentinel in [0u64, 1, u64::MAX] {
            assert_eq!(outbound("_signal", sentinel, &Wrapping).unwrap(), sentinel);
            assert_eq!(inbound(sentinel, &Wrapping), sentinel);
        }
        assert_eq!(outbound("_signal", 0x1_0000_1234, &Wrapping).unwrap(), 0x1_0000_1234 | WRAP);
    }

    #[test]
    fn a_handler_crosses_out_as_a_thunk_and_back_as_the_guests_own_pointer() {
        // The direction that is easy to forget: a program that saves the old
        // disposition and restores it later must get back the pointer it gave,
        // not the arm64 trampoline standing in for it.
        let mut mem = guest_memory();
        let act = SigAction { handler: 0x1_0000_1234, mask: 0, flags: 0 };
        mem.write(mem.base, &act.to_bytes()).unwrap();

        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR1 as u64;
        ctx.rsi = mem.base;
        ctx.rdx = mem.base + 16;
        sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect("install");
        assert_eq!(ctx.rax as i32, 0);

        // Install again, asking for the previous disposition: what comes back
        // must be the guest's own address.
        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR1 as u64;
        ctx.rsi = 0;
        ctx.rdx = mem.base + 16;
        sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect("query");

        let mut back = [0u8; SigAction::SIZE];
        mem.read(mem.base + 16, &mut back).unwrap();
        assert_eq!(SigAction::from_bytes(back).handler, 0x1_0000_1234);

        // Leave the process as it was found.
        // SAFETY: restoring the default disposition for the signal this test
        // installed a handler for.
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
    }

    #[test]
    fn sa_siginfo_is_refused_by_name_with_its_reason() {
        let mut mem = guest_memory();
        let act = SigAction { handler: 0x1_0000_1234, mask: 0, flags: SA_SIGINFO };
        mem.write(mem.base, &act.to_bytes()).unwrap();

        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR2 as u64;
        ctx.rsi = mem.base;
        let err = sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect_err("refuse");

        assert_eq!(err.symbol(), "_sigaction");
        assert!(format!("{err}").contains("ucontext"), "{err}");
        // Refused *before* anything was installed: the guest's disposition for
        // that signal is untouched, which is the difference between a refusal
        // and a half-performed call.
        let mut current = SigAction::default();
        // SAFETY: querying the current disposition into a live local.
        unsafe { libc::sigaction(libc::SIGUSR2, std::ptr::null(), (&raw mut current).cast()) };
        assert_eq!(current.handler, 0, "the refusal installed something anyway");
    }

    #[test]
    fn a_handler_pointer_outside_guest_memory_fails_by_address() {
        let mut mem = guest_memory();
        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR1 as u64;
        ctx.rsi = 0x9_0000_0000;
        let err = sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect_err("fail");
        assert_eq!(err, AbiError::Memory { symbol: "_sigaction".into(), addr: 0x9_0000_0000 });
    }
}
