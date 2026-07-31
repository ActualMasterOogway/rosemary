//! Host→guest calls: a native arm64 trampoline per guest function pointer.
//!
//! ## The direction that gets forgotten
//!
//! Import interception handles guest→host. The mirror of it is not optional and
//! has no static call sites to intercept: whenever a guest function pointer is
//! handed *out* to a host function, the host will later call it as if it were
//! native arm64 code. `qsort`'s comparator, `atexit`'s handler,
//! `pthread_create`'s start routine, `sigaction`'s handler, and — later and
//! pervasively — every Objective-C `IMP`.
//!
//! The thunk is also the whole of what a guest *thread* is. Milestone 4 added no
//! mechanism here: `pthread_create`'s start routine crosses out as an ordinary
//! callback, and the only thing that makes it a thread rather than a comparator
//! is that libpthread calls it on a new host thread — where the VM comes from
//! `PUBLISHED` and the guest stack is made on demand. See [`crate::thread`].
//!
//! Passing the raw pointer through cannot work: it is x86-64, and this CPU will
//! decode it as arm64 and do something arbitrary. So every guest pointer
//! crossing into a host argument is substituted with a **thunk** — a real arm64
//! function which, when the host calls it, places the arguments where the
//! guest's ABI expects them and re-enters the JIT at the guest address.
//!
//! ## Why the trampoline is compiled rather than assembled
//!
//! The obvious implementation is a hand-written arm64 stub that stashes an
//! index in a scratch register and jumps to a common entry. It needs assembly
//! (nothing in Rust can read `x16`), it needs a second stub to spill `x0`–`x7`
//! and `v0`–`v7` somewhere the Rust side can find them, and it has to be right
//! about the frame it builds.
//!
//! Cranelift is already here, compiling guest code. A thunk is a two-instruction
//! idea expressed as IR — spill the argument registers to a stack slot, call a
//! Rust function with the guest address and the two spill areas — and Cranelift
//! emits the prologue, the register moves, and the frame. No assembly, no
//! platform-specific frame layout, and the guest address is a constant baked
//! into each thunk rather than a table index to look up.
//!
//! ## What a thunk can carry
//!
//! Six integer arguments and eight floating-point ones, which is exactly what
//! the *guest's* SysV AMD64 convention passes in registers, and an integer
//! return. That is not an implementation limit chosen for convenience: a
//! seventh integer argument arrives on the host's stack and would have to be
//! copied to the guest's, and a callback returning a struct by value or a
//! `double` needs a differently-typed trampoline. Since the runtime does not
//! know a callback's signature — it is a bare pointer in an argument slot — the
//! shape supported is the one that is right for every C callback anyone
//! actually writes. A callback outside it is a wrong answer rather than a
//! diagnostic, which is the one thing this project refuses to produce
//! elsewhere; it is called out in the roadmap as the known cost of doing
//! host→guest at all without per-callback types.
//!
//! ## Identity
//!
//! One thunk per distinct guest address, kept for the VM's lifetime. Host code
//! compares function pointers — `atexit` deduplicates, a dispatch table is
//! checked against a sentinel — so handing out two thunks for one guest
//! function would answer a question the guest can ask, wrongly. Nothing is ever
//! freed: a released thunk whose address the host still holds is a wrong call,
//! and the roadmap's note stands — a leaked thunk is a slow leak, a stale one
//! is a bug.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicPtr, Ordering};

use cranelift_codegen::ir::{
    types, AbiParam, Function, InstBuilder, Signature, StackSlotData, StackSlotKind, UserFuncName,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use microlift::arch::x86_64::context::RegisterContext;
use microlift::backend::CraneliftBackend;
use microlift::os::get_host_calling_conv;

use crate::vm::{Vm, VmError};

/// Integer argument registers the guest's convention uses.
const GUEST_INT_ARGS: usize = 6;
/// Vector argument registers the guest's convention uses.
const GUEST_FP_ARGS: usize = 8;

/// One compiled trampoline.
pub struct Thunk {
    /// The guest function it re-enters.
    pub guest: u64,
    /// Its own address — what the host is given.
    pub host: u64,
    /// Keeps the JIT module, and therefore `host`'s pages, mapped.
    _backend: CraneliftBackend,
}

// SAFETY: as for `Translation` in `crate::cache`, and for the same reason. A
// thunk is immutable once compiled; `host` is an address the *host* calls, from
// whichever thread it likes, which is the entire point of it. `_backend` is held
// to keep that address mapped and is never read again.
unsafe impl Send for Thunk {}
unsafe impl Sync for Thunk {}

/// Every thunk handed out, both ways round.
///
/// The reverse direction is needed because a host function may hand a pointer
/// *back* — `signal` returns the previous handler, `atexit` hands nothing back
/// but a dispatch table might — and because the guest can call its own function
/// pointer after it has been through a thunk. Unwrapping it is not an
/// optimisation: entering arm64 code that immediately re-enters the JIT would
/// work, and the guest would see a pointer that is not equal to the one it
/// passed in.
pub struct ThunkTable {
    by_guest: BTreeMap<u64, Thunk>,
    by_host: BTreeMap<u64, u64>,
}

impl ThunkTable {
    pub fn new() -> Self {
        ThunkTable { by_guest: BTreeMap::new(), by_host: BTreeMap::new() }
    }

    /// The thunk for a guest address, if one has been built.
    pub fn host_for(&self, guest: u64) -> Option<u64> {
        self.by_guest.get(&guest).map(|t| t.host)
    }

    /// The guest address behind a thunk, if this is one.
    pub fn guest_for(&self, host: u64) -> Option<u64> {
        self.by_host.get(&host).copied()
    }

    /// How many distinct guest pointers have crossed outward.
    pub fn len(&self) -> usize {
        self.by_guest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_guest.is_empty()
    }

    pub fn insert(&mut self, thunk: Thunk) -> u64 {
        let (guest, host) = (thunk.guest, thunk.host);
        self.by_host.insert(host, guest);
        self.by_guest.insert(guest, thunk);
        host
    }
}

impl Default for ThunkTable {
    fn default() -> Self {
        Self::new()
    }
}

// The VM the current thread is executing, for the duration of a guest call.
//
// A thunk is called by *host* code — the host's `qsort`, deep inside a call the
// runtime made — so there is no argument to pass the VM through. It has to be
// found. A thread-local is the whole of the mechanism, and it is also the
// statement of a limit: one VM per thread.
//
// M4 did not change that and did not need to. A thread the guest spawned starts
// inside libpthread, where this is empty and `PUBLISHED` answers instead; from
// then on `with_current` fills it in for the duration of every guest call on
// that thread, exactly as on the first one. What is per-thread is the guest
// *state* — a stack, a stack pointer, a nesting depth — and that lives in
// `crate::thread` rather than here.
thread_local! {
    static CURRENT: Cell<*const Vm> = const { Cell::new(std::ptr::null()) };
}

// The VM to fall back on when the thread-local cannot answer.
//
// It exists for exactly one moment: process exit. The guest's `atexit`
// handlers run from the host's exit list, by which time Rust has already torn
// this thread's TLS down — reading the thread-local there is not merely empty,
// it panics. A process-lifetime pointer survives that, which is the whole
// requirement. `publish` is what sets it, and only a VM that will outlive the
// process's main function may be published.
static PUBLISHED: AtomicPtr<Vm> = AtomicPtr::new(std::ptr::null_mut());

/// Run `f` with `vm` reachable from a thunk on this thread.
///
/// Restores the previous value rather than clearing it, so nesting works: a
/// callback that itself calls out and back finds the same VM.
pub(crate) fn with_current<R>(vm: &Vm, f: impl FnOnce() -> R) -> R {
    let previous = CURRENT.with(|c| c.replace(vm as *const Vm));
    let out = f();
    CURRENT.with(|c| c.set(previous));
    out
}

/// Publish `vm` for the rest of the process's life, and for every thread.
///
/// Two things need this, and the second arrived with milestone 4.
///
/// The guest's `atexit` handlers are entries in the **host's** exit list — that
/// is what forwarding `atexit` with a thunk means — so they fire during `exit`,
/// after `main` has returned and the runtime has stopped driving anything. They
/// still have to find a VM.
///
/// And a thread the guest spawned is started by the *host's* libpthread, which
/// calls a thunk on a thread that has never been inside [`with_current`]. The
/// thread-local is empty there by construction, so a guest that calls
/// `pthread_create` runs its start routines only if a VM has been published.
///
/// In both cases the VM has to outlive the call, which is what [`Box::leak`] is
/// for; nothing is reclaimed at process exit in any case.
pub fn publish(vm: &'static Vm) {
    PUBLISHED.store(vm as *const Vm as *mut Vm, Ordering::Release);
}

/// The VM this thread is executing, if any.
///
/// Also how [`crate::unwind`] reaches the guest's tables: an unwinding shim is
/// handed a register context and guest memory, and needs the program itself.
pub(crate) fn current() -> Option<&'static Vm> {
    // `try_with`, not `with`: at process exit this thread's TLS has been
    // destroyed and reading it would panic inside a host `atexit` handler.
    let p = match CURRENT.try_with(|c| c.get()) {
        Ok(p) if !p.is_null() => p,
        _ => PUBLISHED.load(Ordering::Acquire) as *const Vm,
    };
    // SAFETY: non-null only between `with_current`'s replace and restore, during
    // which the `&Vm` it was given is alive and borrowed immutably — which is
    // what makes re-entering it here legal. The `'static` is a lie the private
    // scope contains: the reference never escapes `thunk_entry`.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// Compile a trampoline that re-enters the guest at `guest_va`.
pub(crate) fn build(guest_va: u64) -> Result<Thunk, VmError> {
    let conv = get_host_calling_conv();
    let fail = |reason: String| VmError::Thunk { guest: guest_va, reason };

    let mut sig = Signature::new(conv);
    for _ in 0..GUEST_INT_ARGS {
        sig.params.push(AbiParam::new(types::I64));
    }
    for _ in 0..GUEST_FP_ARGS {
        sig.params.push(AbiParam::new(types::F64));
    }
    sig.returns.push(AbiParam::new(types::I64));

    let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut fn_ctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fn_ctx);

    let ints = b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        (GUEST_INT_ARGS * 8) as u32,
        3,
    ));
    let floats = b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        (GUEST_FP_ARGS * 8) as u32,
        3,
    ));

    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);

    let params: Vec<_> = b.block_params(entry).to_vec();
    for (i, &v) in params.iter().take(GUEST_INT_ARGS).enumerate() {
        b.ins().stack_store(v, ints, (i * 8) as i32);
    }
    for (i, &v) in params.iter().skip(GUEST_INT_ARGS).enumerate() {
        b.ins().stack_store(v, floats, (i * 8) as i32);
    }

    let int_ptr = b.ins().stack_addr(types::I64, ints, 0);
    let fp_ptr = b.ins().stack_addr(types::I64, floats, 0);
    let guest = b.ins().iconst(types::I64, guest_va as i64);

    let mut callee_sig = Signature::new(conv);
    callee_sig.params.push(AbiParam::new(types::I64));
    callee_sig.params.push(AbiParam::new(types::I64));
    callee_sig.params.push(AbiParam::new(types::I64));
    callee_sig.returns.push(AbiParam::new(types::I64));
    let callee_sig = b.import_signature(callee_sig);
    let callee = b.ins().iconst(types::I64, thunk_entry as *const () as usize as i64);

    let call = b.ins().call_indirect(callee_sig, callee, &[guest, int_ptr, fp_ptr]);
    let result = b.inst_results(call)[0];
    b.ins().return_(&[result]);
    b.finalize();

    let mut backend = CraneliftBackend::new().map_err(|e| fail(e.to_string()))?;
    let code = backend.compile(func).map_err(|e| fail(e.to_string()))?;
    Ok(Thunk { guest: guest_va, host: code as usize as u64, _backend: backend })
}

/// What every thunk calls. Not public: its only legitimate caller is code this
/// module emitted.
///
/// An error here cannot be returned — the host function in the middle
/// (`qsort`, say) has no way to express one — so it is parked on the VM and
/// raised the moment control is back in the runtime. Same for a panic, which
/// must not cross this boundary at all.
unsafe extern "C" fn thunk_entry(guest_va: u64, ints: *const u64, floats: *const f64) -> u64 {
    let Some(vm) = current() else {
        // Nowhere to report this — the caller is `qsort`, or libpthread's thread
        // start, and neither has an error channel — so it is said out loud once
        // and the callback returns zero. M3 could leave this silent because the
        // only way to reach it was to drop a VM that was still handing out
        // thunks. M4 adds a second way that is easy to hit by accident: a
        // *spawned* thread has no thread-local to find a VM on and depends
        // entirely on `publish`, so an embedder that forgets it would otherwise
        // see every guest thread do nothing at all, successfully.
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "rosetta3: a guest callback at {guest_va:#x} ran with no VM reachable; \
                 an embedder whose guest spawns threads or registers `atexit` handlers \
                 must call `rosetta3::thunk::publish`"
            );
        });
        return 0;
    };

    // The VM that answered may not be the one that built this thunk: a process
    // can run more than one, and the process-lifetime fallback holds whichever
    // was published last. Entering it at an address it does not own would
    // execute whatever happens to be there, so a thunk that finds the wrong VM
    // declines rather than guesses.
    if !vm.program.is_guest_address(guest_va) {
        return 0;
    }

    // SAFETY: both point at stack slots the calling thunk just filled, sized
    // `GUEST_INT_ARGS` and `GUEST_FP_ARGS` words by the same code that emits
    // the call.
    let (ints, floats) = unsafe {
        (
            std::slice::from_raw_parts(ints, GUEST_INT_ARGS),
            std::slice::from_raw_parts(floats, GUEST_FP_ARGS),
        )
    };

    let mut ctx = RegisterContext::new();
    ctx.rdi = ints[0];
    ctx.rsi = ints[1];
    ctx.rdx = ints[2];
    ctx.rcx = ints[3];
    ctx.r8 = ints[4];
    ctx.r9 = ints[5];
    for (i, &f) in floats.iter().enumerate() {
        // The low half of `xmm_i`, which is where a `double` argument lives.
        ctx.xmm[i][0] = f.to_bits();
    }

    match std::panic::catch_unwind(AssertUnwindSafe(|| vm.call_with(guest_va, ctx))) {
        Ok(Ok(rax)) => rax,
        Ok(Err(e)) => {
            vm.set_pending(e);
            0
        }
        Err(_) => {
            vm.set_pending(VmError::Thunk {
                guest: guest_va,
                reason: "the guest callback panicked the runtime".into(),
            });
            0
        }
    }
}
