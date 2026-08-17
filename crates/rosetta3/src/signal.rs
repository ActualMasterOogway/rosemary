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
//! ## `SA_SIGINFO` is accepted and not delivered, and the reason is not laziness
//!
//! A three-argument handler is called with a `ucontext_t` describing **the
//! machine state at the point the signal interrupted**. On this runtime that
//! point is inside a Cranelift-compiled arm64 translation, and the guest's
//! x86-64 registers at that instruction are not recoverable: they live in
//! whatever registers or stack slots the compiler chose for that block, with no
//! map back to `rax`, `rsp` and the rest. Synthesizing a plausible `mcontext64`
//! would be exactly the failure this project refuses everywhere else — a wrong
//! answer that does not fault. Recovering it is a *recompiler* work item — a
//! deopt map from a translation's PC back to the guest register context — not a
//! runtime one, which is the useful thing for the diagnostic to say.
//!
//! Until M7.4 this **stopped the guest at installation**, on the grounds that a
//! disposition that will not be honoured is a lie. Terraria is what showed that
//! reasoning to be half right. It embeds Mono, and the first thing Mono's
//! runtime does is install `SA_SIGINFO` handlers for `SIGSEGV`, `SIGBUS`,
//! `SIGFPE` and friends — the standard trick of turning a hardware fault into a
//! managed exception. Refusing at installation kills a program during start-up
//! for a handler that, in a run that never faults, is never called: the
//! *installation* is not the lie, the *delivery* would be.
//!
//! So the two are separated. The installation is accepted and recorded, `oldact`
//! round-trips the guest's own struct so a program that saves and restores a
//! disposition sees what it wrote, and **no host handler is installed** — the
//! signal keeps whatever disposition it had, which for a fault signal means the
//! process dies at the fault with a crash report naming the real instruction
//! rather than vanishing into a handler that would read a synthesized
//! `mcontext64`. Each signal is named once on stderr as it is accepted, so a run
//! that depends on delivery says so in its log rather than going quiet.
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
use static_init::dynamic;

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

/// The `SA_SIGINFO` dispositions this runtime accepted and did not install.
///
/// Keyed by signal number, holding the guest's *own* struct — the handler field
/// as the guest wrote it, never a thunk — because the only thing this map is for
/// is answering `oldact` with what the guest last said. See the module note.
fn recorded() -> &'static std::sync::Mutex<std::collections::HashMap<i32, SigAction>> {
    #[dynamic]
    static MAP: std::sync::Mutex<std::collections::HashMap<i32, SigAction>> = Default::default();
    &MAP
}

/// Say once, per signal, that a disposition was taken and will not be delivered.
///
/// Once per *signal* rather than once per call: a program that reinstalls the
/// same handler around every operation would otherwise fill a log, and the
/// interesting fact is which signals a run depends on, not how often.
fn say_undeliverable(signum: i32) {
    static SAID: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&signum) {
        return;
    }
    said.push(signum);
    eprintln!(
        "rosetta3: sigaction(signal {signum}): SA_SIGINFO handler recorded but not installed \
         — a ucontext_t would need the guest's registers at an interrupted PC, which the \
         recompiler cannot report; the signal keeps its previous disposition"
    );
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
        mem.read(act_va, &mut bytes).map_err(|_| AbiError::Memory {
            symbol: symbol.to_string(),
            addr: act_va,
        })?;
        let mut a = SigAction::from_bytes(bytes);
        if a.flags & SA_SIGINFO != 0 && !is_sentinel(a.handler) {
            // Accepted, recorded, and never installed. See the module note: the
            // installation is honest, the delivery would not be.
            say_undeliverable(signum);
            let previous = recorded()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(signum, a);
            if old_va != 0 {
                let old = previous.unwrap_or_default();
                mem.write(old_va, &old.to_bytes())
                    .map_err(|_| AbiError::Memory {
                        symbol: symbol.to_string(),
                        addr: old_va,
                    })?;
            }
            crate::shims::set_int_return(ctx, 0);
            return Ok(());
        }
        a.handler = outbound(symbol, a.handler, callbacks)?;
        act = Some(a);
    }

    // A query — or an ordinary installation — for a signal whose last
    // disposition this runtime took rather than installed. The host would answer
    // with whatever was there before the guest spoke, which is not what the
    // guest last said, so the recording answers instead.
    if old_va != 0
        && let Some(previous) = recorded()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&signum)
    {
        mem.write(old_va, &previous.to_bytes())
            .map_err(|_| AbiError::Memory {
                symbol: symbol.to_string(),
                addr: old_va,
            })?;
        let rc = match act {
            None => 0,
            // SAFETY: as the call below — a live, correctly sized structure.
            Some(a) => unsafe {
                libc::sigaction(signum, (&raw const a).cast(), std::ptr::null_mut())
            },
        };
        crate::shims::set_int_return(ctx, rc);
        return Ok(());
    }

    let mut old = SigAction::default();
    // SAFETY: both pointers are to live, correctly sized local structures with
    // the layout this platform's `sigaction` expects — the same sixteen bytes on
    // either architecture, which `tests/signal.rs` checks by compilation rather
    // than by assertion.
    let rc = unsafe {
        libc::sigaction(
            signum,
            act.as_ref()
                .map_or(std::ptr::null(), |a| a as *const SigAction)
                .cast(),
            if old_va == 0 {
                std::ptr::null_mut()
            } else {
                (&raw mut old).cast()
            },
        )
    };

    if rc == 0 && old_va != 0 {
        old.handler = inbound(old.handler, callbacks);
        mem.write(old_va, &old.to_bytes())
            .map_err(|_| AbiError::Memory {
                symbol: symbol.to_string(),
                addr: old_va,
            })?;
    }
    crate::shims::set_int_return(ctx, rc);
    Ok(())
}

/// `int sigaltstack(const stack_t *ss, stack_t *oss)`
///
/// Accepted and not installed, for the same reason and with the same shape as
/// the `SA_SIGINFO` case above — and with one hazard of its own that makes
/// passthrough worse than merely useless.
///
/// An alternate signal stack exists to run a handler installed with `SA_ONSTACK`.
/// The guest's fault handlers are recorded and never installed, so the stack it
/// is offering has nothing to run on it. But `sigaltstack` is a property of the
/// **host thread**, and this process has an `SA_ONSTACK` handler of its own:
/// Rust's `std` installs one for `SIGSEGV` so that a stack overflow prints
/// "stack overflow" instead of dying silently. Forwarding the guest's request
/// would move that handler onto a buffer the guest sized for itself, so the one
/// case where the alternate stack is load-bearing is the one case it would be
/// wrong.
///
/// `oss` is answered with `SS_DISABLE` and a null stack, which is what the guest
/// would see if it had never asked — true, and the only self-consistent answer
/// to give a caller that is about to be told its request succeeded.
pub fn sigaltstack(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    /// `stack_t`: `{ void *ss_sp; size_t ss_size; int ss_flags; }` — 24 bytes
    /// with the same layout on x86-64 and arm64 macOS.
    const STACK_T_SIZE: usize = 24;
    /// `SS_DISABLE`.
    const SS_DISABLE: u32 = 0x0004;

    let old_va = ctx.rsi;
    if old_va != 0 {
        let mut bytes = [0u8; STACK_T_SIZE];
        bytes[16..20].copy_from_slice(&SS_DISABLE.to_le_bytes());
        mem.write(old_va, &bytes).map_err(|_| AbiError::Memory {
            symbol: symbol.to_string(),
            addr: old_va,
        })?;
    }
    crate::shims::set_int_return(ctx, 0);
    Ok(())
}

/// `kern_return_t task_set_exception_ports(task, mask, port, behavior, flavor)`
///
/// The same fact as [`sigaction`]'s `SA_SIGINFO` case, reached through a
/// different API. A Mach exception port is the *other* way a program asks to be
/// told about a hardware fault, and it is the one Mono actually relies on: the
/// handler is a thread that receives an exception message carrying a
/// `thread_state` for the faulting thread. That thread state is this CPU's, and
/// the faulting instruction is inside a Cranelift translation, so what the guest
/// would receive is arm64 registers described as x86-64 ones — an answer that is
/// wrong in the specific way that does not fault.
///
/// Accepted with `KERN_SUCCESS` and **not honoured**: the port is not set, so the
/// process keeps its own exception handling and a real fault produces a real
/// crash report naming the real instruction. Announced once, like the signal
/// case, so a run that needed it says so.
///
/// It is a shim rather than a table entry for the same reason `sigaction` is:
/// what the arguments *mean* — a port the guest will listen on — is the part
/// that cannot cross, not how they are passed.
pub fn task_set_exception_ports(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "rosetta3: task_set_exception_ports(mask {:#x}) accepted but not honoured — a Mach \
             exception message carries this CPU's thread state for a fault inside a translation, \
             not the guest's registers",
            ctx.rsi as u32
        );
    }
    // KERN_SUCCESS. The guest asked to be told about faults and will not be.
    ctx.rax = 0;
    Ok(())
}

/// Say the exception-port sentence once, whichever entry point reached it.
///
/// Shared rather than repeated so that a program calling `task_set…` and then
/// `thread_swap…` does not print it twice: what the guest needs to be told is
/// that its exception ports are not honoured, not which of the six spellings it
/// used.
fn announce_exception_ports(symbol: &str, mask: u32) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "rosetta3: {symbol}(mask {mask:#x}) accepted but not honoured — a Mach exception \
             message carries this CPU's thread state for a fault inside a translation, not the \
             guest's registers",
        );
    }
}

/// The `…_get_exception_ports` half of the family: `task`, `host`.
///
/// `kern_return_t task_get_exception_ports(task_t, exception_mask_t,
/// exception_mask_array_t masks, mach_msg_type_number_t *masksCnt,
/// exception_handler_array_t, exception_behavior_array_t,
/// exception_flavor_array_t)` — seven parameters, five of them arrays the callee
/// fills in and one a count it writes back.
///
/// Answered with **zero ports**, which is not a fudge: nothing was installed, so
/// zero is what is there. The count is the only thing that has to be written —
/// the three parallel arrays are read up to it, so leaving them untouched is
/// correct and leaving the count untouched would have the guest read whatever
/// was on its stack as a length.
///
/// The two spellings differ only in what the first argument names, and this
/// treats neither as meaningful: a program asking a task or the host about its
/// exception ports gets the same empty answer.
fn get_exception_ports(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    // `masksCnt` is the fourth parameter.
    write_port_count(symbol, ctx.rcx, mem)?;
    crate::shims::set_int_return(ctx, 0);
    Ok(())
}

/// The `…_swap_exception_ports` half: `task`, `host`, `thread`.
///
/// `kern_return_t task_swap_exception_ports(task_t, exception_mask_t,
/// mach_port_t, exception_behavior_t, thread_state_flavor_t,
/// exception_mask_array_t, mach_msg_type_number_t *masksCnt,
/// exception_handler_array_t, exception_behavior_array_t,
/// exception_flavor_array_t)` — a set and a get in one call, so it is both of
/// the above: the new port is accepted and dropped, and the old ones come back
/// empty.
///
/// **Ten parameters**, which puts `masksCnt` — the seventh — past SysV AMD64's
/// six integer registers and onto the caller's stack. microlift exits before the
/// `CALL`, so `rsp` is the pre-call value and the seventh argument is the first
/// stack word, at `rsp + 0`. Reading it out of a register would write a count
/// through whatever the guest last left there.
fn swap_exception_ports(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    announce_exception_ports(symbol, ctx.rsi as u32);
    let masks_count = mem.read_u64(ctx.rsp, symbol)?;
    write_port_count(symbol, masks_count, mem)?;
    crate::shims::set_int_return(ctx, 0);
    Ok(())
}

/// `host_set_exception_ports`, which is [`task_set_exception_ports`] with a
/// `host_priv_t` in front — and which a program without host privileges could
/// not have used anyway.
fn set_exception_ports(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    announce_exception_ports(symbol, ctx.rsi as u32);
    crate::shims::set_int_return(ctx, 0);
    Ok(())
}

/// Write "no exception ports" through a `mach_msg_type_number_t *`.
///
/// A null pointer is legal — a caller that does not want the old ports passes
/// one — so it is not an error here.
fn write_port_count(symbol: &str, at: u64, mem: &mut dyn GuestMemory) -> Result<()> {
    if at == 0 {
        return Ok(());
    }
    // `mach_msg_type_number_t` is `natural_t`, a 32-bit unsigned integer on
    // both architectures.
    mem.write(at, &0u32.to_le_bytes())
        .map_err(|_| AbiError::Memory {
            symbol: symbol.to_string(),
            addr: at,
        })
}

/// One of this module's implementations, in the shape [`crate::shims`] wants.
///
/// Wider than [`crate::jmpbuf::Shim`] by one parameter, and that parameter is
/// the whole reason these are here: a signal disposition is a guest function
/// pointer crossing in both directions.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory, &dyn Callbacks) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    Some(match name {
        "signal" => signal,
        "sigaction" => sigaction,
        "sigaltstack" => sigaltstack,
        "task_set_exception_ports" => task_set_exception_ports,
        "host_set_exception_ports" => set_exception_ports,
        "task_get_exception_ports" | "host_get_exception_ports" => get_exception_ports,
        "task_swap_exception_ports"
        | "host_swap_exception_ports"
        | "thread_swap_exception_ports" => swap_exception_ports,
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
                None => Err(machish::Error::OutOfBounds {
                    addr: va,
                    len: out.len(),
                }),
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
                None => Err(machish::Error::OutOfBounds {
                    addr: va,
                    len: data.len(),
                }),
            }
        }
    }

    fn guest_memory() -> Bytes {
        Bytes {
            base: 0x3_0000_0000,
            bytes: vec![0; 64],
        }
    }

    #[test]
    fn the_struct_is_sixteen_bytes_of_the_shape_both_architectures_use() {
        assert_eq!(std::mem::size_of::<SigAction>(), SigAction::SIZE);
        assert_eq!(
            std::mem::size_of::<SigAction>(),
            std::mem::size_of::<libc::sigaction>()
        );
        // Round-tripping is what the shim relies on when it rewrites one field
        // and leaves the mask and flags exactly as the guest wrote them.
        let a = SigAction {
            handler: 0x1_0000_1234,
            mask: 0xdead_beef,
            flags: 0x1234_5678,
        };
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
        assert_eq!(
            outbound("_signal", 0x1_0000_1234, &Wrapping).unwrap(),
            0x1_0000_1234 | WRAP
        );
    }

    #[test]
    fn a_handler_crosses_out_as_a_thunk_and_back_as_the_guests_own_pointer() {
        // The direction that is easy to forget: a program that saves the old
        // disposition and restores it later must get back the pointer it gave,
        // not the arm64 trampoline standing in for it.
        let mut mem = guest_memory();
        let act = SigAction {
            handler: 0x1_0000_1234,
            mask: 0,
            flags: 0,
        };
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
    fn sa_siginfo_is_recorded_and_never_installed() {
        let mut mem = guest_memory();
        let act = SigAction {
            handler: 0x1_0000_1234,
            mask: 0,
            flags: SA_SIGINFO,
        };
        mem.write(mem.base, &act.to_bytes()).unwrap();

        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR2 as u64;
        ctx.rsi = mem.base;
        sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect("accepted");
        assert_eq!(ctx.rax as i32, 0, "the guest must be told it succeeded");

        // Accepted and *not installed*: the host's disposition for that signal
        // is untouched, which is the whole claim. A thunk here would mean a
        // handler that will be entered with a synthesized machine state.
        let mut current = SigAction::default();
        // SAFETY: querying the current disposition into a live local.
        unsafe { libc::sigaction(libc::SIGUSR2, std::ptr::null(), (&raw mut current).cast()) };
        assert_eq!(current.handler, 0, "a disposition was installed anyway");
    }

    #[test]
    fn a_recorded_sa_siginfo_disposition_comes_back_through_oldact() {
        // A program that saves a disposition and restores it later must see what
        // it wrote, not what the host would report — which is the disposition
        // from before the guest ever spoke.
        let mut mem = guest_memory();
        let first = SigAction {
            handler: 0x1_0000_1234,
            mask: 0,
            flags: SA_SIGINFO,
        };
        mem.write(mem.base, &first.to_bytes()).unwrap();

        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR2 as u64;
        ctx.rsi = mem.base;
        sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect("first");

        let second = SigAction {
            handler: 0x1_0000_5678,
            mask: 0,
            flags: SA_SIGINFO,
        };
        mem.write(mem.base, &second.to_bytes()).unwrap();
        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR2 as u64;
        ctx.rsi = mem.base;
        ctx.rdx = mem.base + 16;
        sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect("second");

        let mut back = [0u8; SigAction::SIZE];
        mem.read(mem.base + 16, &mut back).unwrap();
        let back = SigAction::from_bytes(back);
        assert_eq!(
            back.handler, 0x1_0000_1234,
            "the guest's own address, unwrapped"
        );
        assert_eq!(back.flags, SA_SIGINFO);
    }

    #[test]
    fn sigaltstack_succeeds_and_reports_the_stack_disabled() {
        // Accepted so a runtime that installs one can start; never forwarded,
        // because the alternate stack belongs to the *host* thread. See the
        // function's own note.
        let mut mem = guest_memory();
        let mut ctx = RegisterContext::new();
        ctx.rdi = 0;
        ctx.rdx = 0;
        ctx.rsi = mem.base + 16;
        sigaltstack("_sigaltstack", &mut ctx, &mut mem, &Wrapping).expect("accepted");
        assert_eq!(ctx.rax as i32, 0);

        let mut back = [0u8; 24];
        mem.read(mem.base + 16, &mut back).unwrap();
        assert_eq!(u64::from_le_bytes(back[0..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(back[16..20].try_into().unwrap()), 0x0004);
    }

    #[test]
    fn a_handler_pointer_outside_guest_memory_fails_by_address() {
        let mut mem = guest_memory();
        let mut ctx = RegisterContext::new();
        ctx.rdi = libc::SIGUSR1 as u64;
        ctx.rsi = 0x9_0000_0000;
        let err = sigaction("_sigaction", &mut ctx, &mut mem, &Wrapping).expect_err("fail");
        assert_eq!(
            err,
            AbiError::Memory {
                symbol: "_sigaction".into(),
                addr: 0x9_0000_0000
            }
        );
    }
}
