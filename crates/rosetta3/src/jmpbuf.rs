//! `setjmp` and `longjmp`, virtualized — the first half of milestone 5.
//!
//! ## Why passthrough is not merely unimplemented but wrong
//!
//! Every other libSystem entry point in this runtime is forwarded: the host's
//! `strlen` reads the guest's bytes, the host's `pthread_mutex_lock` locks the
//! guest's mutex in place, and `errno` needs no work at all because the guest
//! thread *is* the host thread. That works because those functions operate on
//! *memory*, and the guest and the host share an address space.
//!
//! `setjmp` operates on **registers and a stack**, and there the two sides stop
//! agreeing. The host's `setjmp` would save arm64 callee-saved registers and the
//! *host's* stack pointer — the JIT's own arm64 frame, one level below whichever
//! translation happened to be running. The host's `longjmp` would then restore
//! that frame, resuming inside the recompiler at a point the guest knows nothing
//! about, with the guest's own `rsp`, `rbx` and `rip` untouched. It does not
//! fault; it simply runs the wrong program. So both halves are implemented here,
//! against the guest's register context, and the pair must be intercepted
//! together: half of it forwarded would be worse than neither.
//!
//! ## The buffer's layout is ours, and that is the point
//!
//! A `jmp_buf` is opaque by specification. The guest allocates 148 bytes
//! (`int[37]` on x86-64 Darwin; a `sigjmp_buf` is 152) and hands their address to
//! `setjmp` and later to `longjmp` — it never reads a field, because there are no
//! documented fields to read. Since this runtime implements *both* ends, the
//! contents are its own format, and nothing here has to reproduce libSystem's.
//! [`SavedContext`] is therefore what the *guest* needs restored, plus what it
//! takes to notice a jump that cannot be performed, and it is checked against
//! [`GUEST_JMP_BUF`] so it can never outgrow the space the guest set aside.
//!
//! ## What is saved, and why so little
//!
//! SysV AMD64 makes `rbx`, `rbp`, `r12`–`r15` and `rsp` callee-saved; everything
//! else is caller-saved and therefore already dead across a call by the time
//! `setjmp` returns. `rip` is the resume point. Nothing else is restored, and the
//! reason is that a compiler must treat `setjmp` as `returns_twice`: it spills
//! anything live across the call to the stack rather than trusting a register, so
//! the callee-saved set plus the stack *is* the observable state. The one real
//! omission is the x87 control word and `MXCSR`, which the host's `_setjmp` does
//! save — see the scope note at the bottom.
//!
//! ## Interception is exact, not approximate
//!
//! Microlift intercepts a registered import **before** the `CALL` executes, so
//! at the exit `ctx.rsp` is already the value it will have when `setjmp`
//! returns, and `ctx.rip` is already the return point. Saving the two verbatim is
//! not an approximation of what the guest would have done — it is what the guest
//! would have done, one instruction early.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

/// Bytes an x86-64 Darwin guest sets aside for a `jmp_buf` (`int[37]`).
///
/// A `sigjmp_buf` is one `int` longer. The smaller of the two is the budget,
/// because a record that fits both is the only one that cannot overrun a guest
/// allocation it was handed the address of.
pub const GUEST_JMP_BUF: usize = 148;

/// The magic word a buffer this runtime wrote begins with.
///
/// Not decoration: a `longjmp` through a buffer that no `setjmp` of ours ever
/// wrote is a guest that saved its context some other way, and it must be a named
/// refusal rather than a jump to whatever those bytes happen to say. Uninitialised
/// stack is the common case and it is *usually* not this value, which is the most
/// a magic number can promise.
const MAGIC: u64 = 0x524f_5345_5454_4133; // "ROSETTA3"

/// A guest context, as `setjmp` records it.
///
/// Field order is the serialised order; see [`SavedContext::to_bytes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedContext {
    /// Where the guest resumes: the instruction after the `call _setjmp`.
    pub rip: u64,
    pub rsp: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    /// The [`crate::thread::ThreadState`] that saved this, as an integer.
    ///
    /// A `jmp_buf` is only meaningful on the thread that filled it — C says so,
    /// and here it is checkable rather than merely said, because a jump onto
    /// another thread's guest stack would corrupt a stack that thread is still
    /// running on.
    pub owner: u64,
    /// Which VM entry was live when this was saved. See [`longjmp_to`].
    pub entry: u64,
}

/// The magic word plus ten payload words: well inside what the guest allocated.
const RECORD: usize = 11 * 8;
const _: () = assert!(RECORD <= GUEST_JMP_BUF);

impl SavedContext {
    /// Everything `setjmp` must record, taken from a live exit context.
    ///
    /// `ctx.rsp` and `ctx.rip` are already the post-return values — the call was
    /// intercepted before it pushed anything — so this is a copy, not a fix-up.
    pub fn capture(ctx: &RegisterContext, owner: u64, entry: u64) -> Self {
        SavedContext {
            rip: ctx.rip,
            rsp: ctx.rsp,
            rbx: ctx.rbx,
            rbp: ctx.rbp,
            r12: ctx.r12,
            r13: ctx.r13,
            r14: ctx.r14,
            r15: ctx.r15,
            owner,
            entry,
        }
    }

    /// Put the guest back where [`SavedContext::capture`] found it.
    ///
    /// Callee-saved registers and the stack only: see the module note on why the
    /// caller-saved half is already dead. `rax` is the `longjmp` value and is
    /// written by the caller, because the C rule about zero belongs there.
    pub fn restore(&self, ctx: &mut RegisterContext) {
        ctx.rip = self.rip;
        ctx.rsp = self.rsp;
        ctx.rbx = self.rbx;
        ctx.rbp = self.rbp;
        ctx.r12 = self.r12;
        ctx.r13 = self.r13;
        ctx.r14 = self.r14;
        ctx.r15 = self.r15;
    }

    fn to_bytes(self) -> [u8; RECORD] {
        let words = [
            MAGIC, self.rip, self.rsp, self.rbx, self.rbp, self.r12, self.r13, self.r14,
            self.r15, self.owner, self.entry,
        ];
        let mut out = [0u8; RECORD];
        for (slot, word) in out.chunks_exact_mut(8).zip(words) {
            slot.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn from_bytes(bytes: &[u8; RECORD]) -> Option<Self> {
        let mut words = bytes.chunks_exact(8).map(|c| {
            let mut w = [0u8; 8];
            w.copy_from_slice(c);
            u64::from_le_bytes(w)
        });
        (words.next()? == MAGIC).then_some(())?;
        Some(SavedContext {
            rip: words.next()?,
            rsp: words.next()?,
            rbx: words.next()?,
            rbp: words.next()?,
            r12: words.next()?,
            r13: words.next()?,
            r14: words.next()?,
            r15: words.next()?,
            owner: words.next()?,
            entry: words.next()?,
        })
    }
}

/// `int setjmp(jmp_buf)` and every spelling of it.
///
/// Returns zero, as a first return must. The second return is [`longjmp`]'s
/// business, and it does not come back through here at all: it restores `rip`
/// directly, so the guest resumes at the same instruction with a different `rax`.
pub fn setjmp(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let env = ctx.rdi;
    let Some(state) = crate::thread::current() else {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "setjmp outside a running guest thread",
        });
    };
    let saved = SavedContext::capture(ctx, state as *const _ as u64, state.entry());
    mem.write(env, &saved.to_bytes())
        .map_err(|_| AbiError::Memory { symbol: symbol.to_string(), addr: env })?;
    ctx.rax = 0;
    Ok(())
}

/// `void longjmp(jmp_buf, int)`, and every spelling of it.
///
/// On success `ctx` has been replaced wholesale and the guest resumes at the
/// `setjmp` that filled the buffer. Nothing returns from here in the guest's
/// sense — which is why the runtime is told, through
/// [`ThreadState::redirect`](crate::thread::ThreadState::redirect), that this
/// call transferred control rather than returned.
pub fn longjmp(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let env = ctx.rdi;
    // C: a `longjmp` with zero makes `setjmp` return one, because zero is
    // reserved for the first return and a guest asking for it means "any
    // non-first return".
    let value = ctx.esi_as_int();
    longjmp_to(symbol, env, if value == 0 { 1 } else { value }, ctx, mem)
}

/// The half of [`longjmp`] that is about the buffer rather than the value.
///
/// Separated because a `siglongjmp` and a future signal-frame return want the
/// same restore with a different story about the value.
fn longjmp_to(
    symbol: &str,
    env: u64,
    value: i32,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let mut bytes = [0u8; RECORD];
    mem.read(env, &mut bytes)
        .map_err(|_| AbiError::Memory { symbol: symbol.to_string(), addr: env })?;
    let Some(saved) = SavedContext::from_bytes(&bytes) else {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "the jmp_buf was not filled in by a setjmp this runtime serviced",
        });
    };
    let Some(state) = crate::thread::current() else {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "longjmp outside a running guest thread",
        });
    };

    // Two refusals, and neither is an implementation shortcut that could be
    // dropped by trying harder here.
    //
    // A buffer from another thread names a stack that thread is still running
    // on; jumping onto it would corrupt a live stack rather than fail. C calls
    // this undefined and this runtime can see it, so it says so.
    if saved.owner != state as *const _ as u64 {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "the jmp_buf belongs to another thread",
        });
    }
    // A buffer from an outer VM entry names a guest frame that is *below* host
    // frames — the host's `qsort`, or `pthread_once`, with a guest callback
    // above it. Restoring the guest context would leave those host frames on the
    // host's stack with nothing ever returning through them. Getting there needs
    // the guest unwinder the rest of M5 is about, so for now it is a refusal
    // that names the case rather than a jump that half-performs it.
    if saved.entry != state.entry() {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "longjmp across a host frame the runtime cannot unwind yet (M5)",
        });
    }

    saved.restore(ctx);
    ctx.rax = value as i64 as u64;
    // The guest's live stack pointer moved without an exit reporting it, and a
    // host->guest callback entered before the next exit would otherwise start
    // below a frame that no longer exists.
    state.rsp.set(ctx.rsp);
    // `rip` is now the destination, not a return address: whoever serviced this
    // call must not synthesize a return over it. This is what a tail-called
    // `jmp _longjmp` would otherwise do — see [`crate::vm::Vm::run_loop`].
    state.redirect();
    Ok(())
}

/// `int sigsetjmp(sigjmp_buf, int savemask)`.
///
/// The `savemask` argument is accepted and ignored, and the ignoring is the
/// honest part: saving a signal mask would mean this runtime had one to save.
/// Signal delivery is not virtualized yet (the other half of M5), so a guest's
/// mask is the host thread's mask and is neither changed nor restored by
/// anything here. When signals arrive, this is the function that grows a field.
pub fn sigsetjmp(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    setjmp(symbol, ctx, mem)
}

/// `void siglongjmp(sigjmp_buf, int)` — [`longjmp`], with the same caveat.
pub fn siglongjmp(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    longjmp(symbol, ctx, mem)
}

/// The second argument as the `int` the C declaration says it is.
trait SecondArgument {
    fn esi_as_int(&self) -> i32;
}

impl SecondArgument for RegisterContext {
    fn esi_as_int(&self) -> i32 {
        self.rsi as u32 as i32
    }
}

/// One of this module's implementations, in the shape [`crate::shims`] dispatches.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
///
/// Both Darwin spellings of each: `setjmp`/`_setjmp` differ in whether the
/// *host's* implementation saves the signal mask, which is a distinction about a
/// mask this runtime does not model — so they are one implementation here, and
/// the scope note says so rather than the code pretending otherwise.
pub fn shim_for(name: &str) -> Option<Shim> {
    Some(match name {
        "setjmp" | "_setjmp" => setjmp,
        "sigsetjmp" => sigsetjmp,
        "longjmp" | "_longjmp" => longjmp,
        "siglongjmp" => siglongjmp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat span of guest memory at a known base.
    ///
    /// Enough for a `jmp_buf` and nothing else, deliberately: these tests are
    /// about the record and the refusals, and a real address space would put a
    /// loader between the assertion and what it is asserting.
    struct TestMem {
        base: u64,
        bytes: Vec<u8>,
    }

    impl TestMem {
        fn new(base: u64) -> Self {
            TestMem { base, bytes: vec![0; GUEST_JMP_BUF] }
        }

        fn at(&self, va: u64, len: usize) -> Option<std::ops::Range<usize>> {
            let off = usize::try_from(va.checked_sub(self.base)?).ok()?;
            (off + len <= self.bytes.len()).then_some(off..off + len)
        }
    }

    impl GuestMemory for TestMem {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            let r = self
                .at(va, out.len())
                .ok_or(machish::Error::OutOfBounds { addr: va, len: out.len() })?;
            out.copy_from_slice(&self.bytes[r]);
            Ok(())
        }

        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            let r = self
                .at(va, data.len())
                .ok_or(machish::Error::OutOfBounds { addr: va, len: data.len() })?;
            self.bytes[r].copy_from_slice(data);
            Ok(())
        }
    }

    /// Give this test thread somewhere to run a guest, as a real entry would.
    fn on_a_guest_thread() -> &'static crate::thread::ThreadState {
        crate::thread::ensure().expect("a guest stack for the test thread")
    }

    fn ctx_with(rip: u64, rsp: u64) -> RegisterContext {
        let mut ctx = RegisterContext::new();
        ctx.rip = rip;
        ctx.rsp = rsp;
        ctx.rbx = 0xb;
        ctx.rbp = 0xbb;
        ctx.r12 = 0xc;
        ctx.r13 = 0xd;
        ctx.r14 = 0xe;
        ctx.r15 = 0xf;
        ctx
    }

    #[test]
    fn a_record_round_trips_through_the_guests_buffer() {
        let ctx = ctx_with(0x1_0000_1234, 0x7fff_0000);
        let saved = SavedContext::capture(&ctx, 0x42, 7);
        let bytes = saved.to_bytes();
        assert_eq!(SavedContext::from_bytes(&bytes), Some(saved));
    }

    #[test]
    fn a_buffer_no_setjmp_of_ours_wrote_is_not_a_context() {
        // Uninitialised stack, which is what a guest's `jmp_buf` holds before
        // its first `setjmp`. Restoring from it would be a jump to garbage.
        assert_eq!(SavedContext::from_bytes(&[0u8; RECORD]), None);
        let mut bytes = SavedContext::capture(&ctx_with(1, 2), 0, 0).to_bytes();
        bytes[0] ^= 1;
        assert_eq!(SavedContext::from_bytes(&bytes), None);
    }

    #[test]
    fn restoring_touches_the_callee_saved_set_and_nothing_else() {
        let saved = SavedContext::capture(&ctx_with(0x1_0000_1234, 0x7fff_0000), 0, 0);
        let mut live = RegisterContext::new();
        live.rax = 0xdead;
        live.rcx = 0xbeef;
        live.rdi = 0xfeed;
        saved.restore(&mut live);
        assert_eq!((live.rip, live.rsp, live.rbx, live.r15), (0x1_0000_1234, 0x7fff_0000, 0xb, 0xf));
        // Caller-saved registers are dead across a call by definition, and a
        // `longjmp` that resurrected stale ones would be inventing state.
        assert_eq!((live.rax, live.rcx, live.rdi), (0xdead, 0xbeef, 0xfeed));
    }

    #[test]
    fn a_round_trip_restores_the_context_and_reports_the_value() {
        let state = on_a_guest_thread();
        let entry = state.enter();
        let mut mem = TestMem::new(0x2_0000_0000);

        let mut ctx = ctx_with(0x1_0000_1234, 0x7fff_1000);
        ctx.rdi = mem.base;
        setjmp("_setjmp", &mut ctx, &mut mem).expect("setjmp");
        assert_eq!(ctx.rax, 0, "a first return is zero");

        // The guest runs on: deeper stack, different callee-saved registers,
        // somewhere else entirely.
        let mut ctx = ctx_with(0x1_0000_9999, 0x7fff_0800);
        ctx.rbx = 0xdead;
        ctx.r15 = 0xbeef;
        ctx.rdi = mem.base;
        ctx.rsi = 7;
        longjmp("_longjmp", &mut ctx, &mut mem).expect("longjmp");

        assert_eq!((ctx.rip, ctx.rsp), (0x1_0000_1234, 0x7fff_1000));
        assert_eq!((ctx.rbx, ctx.r15), (0xb, 0xf), "callee-saved registers were not restored");
        assert_eq!(ctx.rax, 7, "setjmp must return what longjmp was given");
        // The stack pointer moved without an exit reporting it, and a callback
        // entered before the next one would otherwise start below a dead frame.
        assert_eq!(state.rsp.get(), 0x7fff_1000);
        state.leave(entry);
    }

    #[test]
    fn a_longjmp_says_it_transferred_control_rather_than_returned() {
        // What the tail-call path in [`crate::vm`] consults. A compiler is free
        // to emit `jmp _longjmp`, and finishing that call the ordinary way —
        // popping a return address over `rip` — would land in the caller of the
        // function that jumped, which is the one frame the guest asked to leave.
        let state = on_a_guest_thread();
        let entry = state.enter();
        let mut mem = TestMem::new(0x2_0000_0000);

        let mut ctx = ctx_with(0x1_0000_1234, 0x7fff_1000);
        ctx.rdi = mem.base;
        setjmp("_setjmp", &mut ctx, &mut mem).expect("setjmp");
        assert!(!state.took_redirect(), "a setjmp returns normally");

        ctx.rdi = mem.base;
        ctx.rsi = 0;
        longjmp("_longjmp", &mut ctx, &mut mem).expect("longjmp");
        assert_eq!(ctx.rax, 1, "a longjmp of zero arrives as one");
        assert!(state.took_redirect(), "a longjmp did not report the transfer");
        assert!(!state.took_redirect(), "the flag must not survive being read");
        state.leave(entry);
    }

    #[test]
    fn a_longjmp_into_an_outer_entry_is_refused_with_its_reason() {
        // The `setjmp` happens in one entry and the `longjmp` in a nested one —
        // which is what a guest callback called by the host's `qsort` is. The
        // host frames between them cannot be unwound yet, so this must be a
        // named refusal rather than a restore that abandons them.
        let state = on_a_guest_thread();
        let outer = state.enter();
        let mut mem = TestMem::new(0x2_0000_0000);

        let mut ctx = ctx_with(0x1_0000_1234, 0x7fff_1000);
        ctx.rdi = mem.base;
        setjmp("_setjmp", &mut ctx, &mut mem).expect("setjmp");

        let inner = state.enter();
        let mut jumping = ctx_with(0x1_0000_9999, 0x7fff_0800);
        jumping.rdi = mem.base;
        jumping.rsi = 3;
        let before = jumping;
        let err = longjmp("_longjmp", &mut jumping, &mut mem).expect_err("should refuse");

        assert_eq!(err.symbol(), "_longjmp");
        assert!(format!("{err}").contains("unwind"), "{err}");
        // The guest context is untouched on a refusal, exactly as a missing
        // signature leaves it: a diagnostic, not half a jump.
        assert_eq!((jumping.rip, jumping.rsp, jumping.rbx), (before.rip, before.rsp, before.rbx));
        assert!(!state.took_redirect());

        state.leave(inner);
        state.leave(outer);
    }

    #[test]
    fn a_longjmp_through_a_buffer_no_setjmp_wrote_is_refused() {
        let state = on_a_guest_thread();
        let entry = state.enter();
        let mut mem = TestMem::new(0x2_0000_0000);
        let mut ctx = ctx_with(0x1_0000_9999, 0x7fff_0800);
        ctx.rdi = mem.base;

        let err = longjmp("_longjmp", &mut ctx, &mut mem).expect_err("should refuse");
        assert!(format!("{err}").contains("setjmp"), "{err}");
        state.leave(entry);
    }

    #[test]
    fn a_buffer_outside_guest_memory_fails_by_address() {
        let state = on_a_guest_thread();
        let entry = state.enter();
        let mut mem = TestMem::new(0x2_0000_0000);
        let mut ctx = ctx_with(0x1_0000_1234, 0x7fff_1000);
        ctx.rdi = 0x9_0000_0000;
        ctx.rax = 0xdead;

        let err = setjmp("_setjmp", &mut ctx, &mut mem).expect_err("should fail");
        assert_eq!(err, AbiError::Memory { symbol: "_setjmp".into(), addr: 0x9_0000_0000 });
        // A `setjmp` that failed must not look like one that returned zero.
        assert_eq!(ctx.rax, 0xdead, "rax is untouched on failure");
        state.leave(entry);
    }

    #[test]
    fn every_darwin_spelling_has_a_shim() {
        for name in ["setjmp", "_setjmp", "sigsetjmp", "longjmp", "_longjmp", "siglongjmp"] {
            assert!(shim_for(name).is_some(), "{name} has no shim");
            // And the table agrees, so the audit does not report as refused
            // something that in fact runs.
            assert!(
                matches!(crate::abi::table::lookup(0, name), crate::abi::Lookup::Known(_)),
                "{name} is shimmed but not described"
            );
        }
        assert!(shim_for("strlen").is_none());
    }

    #[test]
    fn the_second_argument_is_read_as_a_signed_int() {
        let mut ctx = RegisterContext::new();
        // A guest passing -1 leaves the high half of `rsi` as whatever the
        // caller had; `int` is the low 32 bits.
        ctx.rsi = 0xdead_beef_ffff_ffff;
        assert_eq!(ctx.esi_as_int(), -1);
    }
}
