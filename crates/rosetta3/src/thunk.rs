//! Host→guest calls: a native arm64 trampoline per guest function pointer.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicPtr, Ordering};

use cranelift_codegen::ir::{
    AbiParam, Function, InstBuilder, Signature, StackSlotData, StackSlotKind, UserFuncName, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use microlift::arch::x86_64::context::RegisterContext;
use microlift::backend::CraneliftBackend;
use microlift::os::get_host_calling_conv;

use crate::abi::ArgType as T;
use crate::abi::aggregate::{Aggregate, Eightbyte, GuestClass, HostClass, Shape};
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
/// The reverse direction is needed because a host function may hand a pointer *back* —
/// `signal` returns the previous handler, `atexit` hands nothing back but a dispatch
/// table might — and because the guest can call its own function pointer after it has
/// been through a thunk.
pub struct ThunkTable {
    by_guest: BTreeMap<u64, Thunk>,
    by_host: BTreeMap<u64, u64>,
}

impl ThunkTable {
    pub fn new() -> Self {
        ThunkTable {
            by_guest: BTreeMap::new(),
            by_host: BTreeMap::new(),
        }
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

    /// A snapshot of both directions, for [`ThunkView`].
    pub fn view(&self) -> ThunkView {
        ThunkView {
            by_guest: self.by_guest.iter().map(|(g, t)| (*g, t.host)).collect(),
            by_host: self.by_host.iter().map(|(h, g)| (*h, *g)).collect(),
            count: self.by_guest.len(),
        }
    }
}

impl Default for ThunkTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The read-only half of a [`ThunkTable`], published for lookups that must not take a
/// lock.
///
/// ## Why the lookups could not stay behind the mutex
///
/// ## Why a miss still has to be checked
///
/// A stale snapshot answers "no" for a thunk another thread created a moment ago, and
/// unlike a code translation that answer is not merely older — it is wrong, because
/// unwrapping a thunk is what keeps a guest pointer comparing equal to itself.
pub struct ThunkView {
    by_guest: Vec<(u64, u64)>,
    by_host: Vec<(u64, u64)>,
    /// How many thunks existed when this was built.
    pub count: usize,
}

impl ThunkView {
    /// The thunk for a guest address, if this snapshot has one.
    pub fn host_for(&self, guest: u64) -> Option<u64> {
        Self::find(&self.by_guest, guest)
    }

    /// The guest address behind a thunk, if this snapshot knows it.
    pub fn guest_for(&self, host: u64) -> Option<u64> {
        Self::find(&self.by_host, host)
    }

    fn find(pairs: &[(u64, u64)], key: u64) -> Option<u64> {
        pairs
            .binary_search_by_key(&key, |(k, _)| *k)
            .ok()
            .map(|i| pairs[i].1)
    }
}

// The VM the current thread is executing, for the duration of a guest call.
thread_local! {
    static CURRENT: Cell<*const Vm> = const { Cell::new(std::ptr::null()) };
}

// The VM to fall back on when the thread-local cannot answer. It exists for
// exactly one moment: process exit.
static PUBLISHED: AtomicPtr<Vm> = AtomicPtr::new(std::ptr::null_mut());

/// Run `f` with `vm` reachable from a thunk on this thread.
pub(crate) fn with_current<R>(vm: &Vm, f: impl FnOnce() -> R) -> R {
    let previous = CURRENT.with(|c| c.replace(vm as *const Vm));
    let out = f();
    CURRENT.with(|c| c.set(previous));
    out
}

/// Publish `vm` for the rest of the process's life, and for every thread.
///
/// And a thread the guest spawned is started by the *host's* libpthread, which calls a
/// thunk on a thread that has never been inside [`with_current`].
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

/// A callback whose argument and return types are known, as the emitted trampoline and
/// the Rust entry both need them.
pub struct ThunkSignature {
    /// `self` and `_cmd` included: these are the arguments the guest really
    /// takes, in order.
    params: Vec<Shape>,
    /// `None` for `void`.
    returns: Option<Shape>,
    /// How each parameter arrives from the *host*, flattened to the scalars the
    /// trampoline's own signature declares.
    host_leaves: Vec<Vec<T>>,
}

/// Why a callback's shape cannot be carried, in the register a refusal uses.
type Unsupported = &'static str;

impl ThunkSignature {
    /// Decide the host-side shape of every argument, or say what stopped it.
    ///
    /// An aggregate's host placement is indistinguishable from its leaves' — an HFA of
    /// *n* occupies the same registers as *n* floating-point arguments, and a composite
    /// of at most sixteen bytes the same as one or two integer ones — which is what
    /// lets the trampoline declare a signature of plain scalars and still be called
    /// correctly by AAPCS64.
    pub fn new(params: Vec<Shape>, returns: Option<Shape>) -> Result<Self, Unsupported> {
        let mut host_leaves = Vec::with_capacity(params.len());
        for shape in &params {
            host_leaves.push(match shape {
                Shape::Scalar(t) => vec![*t],
                Shape::Struct(agg) => match agg.host_class()? {
                    HostClass::Hfa { ty, count } => vec![ty; count],
                    HostClass::Integer { words } => {
                        vec![T::UnsignedDoubleword; words]
                    }
                    // AAPCS64 copies an argument this large to the *caller's*
                    // stack, and the trampoline would have to find it there.
                    // Refused rather than read from the wrong place.
                    HostClass::Indirect => {
                        return Err("takes an aggregate too large for AAPCS64's registers");
                    }
                },
            });
        }
        // An HFA return comes back in `v0`–`v3` while the guest returns through
        // a hidden buffer, and expressing "return four doubles in four vector
        // registers" is not something Cranelift's return lowering promises to
        // do the way AAPCS64 specifies.
        if let Some(Shape::Struct(_)) = &returns {
            return Err("returns an aggregate by value, which a thunk cannot carry");
        }
        Ok(ThunkSignature {
            params,
            returns,
            host_leaves,
        })
    }

    /// The Cranelift type one host-side leaf arrives as.
    fn cl_type(t: T) -> types::Type {
        match t {
            T::SinglePrecisionFp => types::F32,
            T::DoublePrecisionFp => types::F64,
            _ => types::I64,
        }
    }
}

/// Compile a trampoline that re-enters the guest at `guest_va`, placing arguments by
/// `signature` rather than by the generic shape.
///
/// The generic thunk takes six integer and eight floating-point registers because it
/// has no types to work from.
pub(crate) fn build_typed(guest_va: u64, signature: ThunkSignature) -> Result<Thunk, VmError> {
    let conv = get_host_calling_conv();
    let fail = |reason: String| VmError::Thunk {
        guest: guest_va,
        reason,
    };

    let leaves: Vec<T> = signature.host_leaves.iter().flatten().copied().collect();
    let mut sig = Signature::new(conv);
    for &t in &leaves {
        sig.params.push(AbiParam::new(ThunkSignature::cl_type(t)));
    }
    let ret_ty = signature
        .returns
        .as_ref()
        .and_then(Shape::scalar)
        .filter(|t| *t != T::None)
        .map(ThunkSignature::cl_type);
    if let Some(t) = ret_ty {
        sig.returns.push(AbiParam::new(t));
    }

    // The host machine this trampoline is being built for. Cranelift's frontend
    // is told rather than asked; see [`crate::jit::frontend_config`].
    let frontend = crate::jit::frontend_config().map_err(|e| fail(e.to_string()))?;
    let ptr = frontend.pointer_type();

    let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut fn_ctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fn_ctx);

    // One word per host-side leaf, in declaration order. The Rust entry knows
    // the type of each from the descriptor, so the slot carries no tags.
    let spill = b.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        (leaves.len().max(1) * 8) as u32,
        3,
    ));
    // Where the entry writes the result, so a floating-point return does not
    // have to travel back through an integer register.
    let out = b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));

    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);

    let params: Vec<_> = b.block_params(entry).to_vec();
    for (i, &v) in params.iter().enumerate() {
        b.ins().stack_store(ptr, v, spill, (i * 8) as i32);
    }

    let desc = Box::leak(Box::new(signature)) as *const ThunkSignature;
    let desc = b.ins().iconst(types::I64, desc as usize as i64);
    let spill_ptr = b.ins().stack_addr(types::I64, spill, 0);
    let out_ptr = b.ins().stack_addr(types::I64, out, 0);
    let guest = b.ins().iconst(types::I64, guest_va as i64);

    let mut callee_sig = Signature::new(conv);
    for _ in 0..4 {
        callee_sig.params.push(AbiParam::new(types::I64));
    }
    let callee_sig = b.import_signature(callee_sig);
    let callee = b
        .ins()
        .iconst(types::I64, typed_entry as *const () as usize as i64);
    b.ins()
        .call_indirect(callee_sig, callee, &[guest, desc, spill_ptr, out_ptr]);

    match ret_ty {
        Some(t) => {
            let v = b.ins().stack_load(ptr, t, out, 0);
            b.ins().return_(&[v]);
        }
        None => {
            b.ins().return_(&[]);
        }
    }
    b.finalize(frontend);

    let mut backend = crate::jit::backend().map_err(|e| fail(e.to_string()))?;
    let code = backend.compile(func).map_err(|e| fail(e.to_string()))?;
    Ok(Thunk {
        guest: guest_va,
        host: code as usize as u64,
        _backend: backend,
    })
}

/// Compile a trampoline that re-enters the guest at `guest_va`.
pub(crate) fn build(guest_va: u64) -> Result<Thunk, VmError> {
    let conv = get_host_calling_conv();
    let fail = |reason: String| VmError::Thunk {
        guest: guest_va,
        reason,
    };

    let mut sig = Signature::new(conv);
    for _ in 0..GUEST_INT_ARGS {
        sig.params.push(AbiParam::new(types::I64));
    }
    for _ in 0..GUEST_FP_ARGS {
        sig.params.push(AbiParam::new(types::F64));
    }
    sig.returns.push(AbiParam::new(types::I64));

    let frontend = crate::jit::frontend_config().map_err(|e| fail(e.to_string()))?;
    let ptr = frontend.pointer_type();

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
        b.ins().stack_store(ptr, v, ints, (i * 8) as i32);
    }
    for (i, &v) in params.iter().skip(GUEST_INT_ARGS).enumerate() {
        b.ins().stack_store(ptr, v, floats, (i * 8) as i32);
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
    let callee = b
        .ins()
        .iconst(types::I64, thunk_entry as *const () as usize as i64);

    let call = b
        .ins()
        .call_indirect(callee_sig, callee, &[guest, int_ptr, fp_ptr]);
    let result = b.inst_results(call)[0];
    b.ins().return_(&[result]);
    b.finalize(frontend);

    let mut backend = crate::jit::backend().map_err(|e| fail(e.to_string()))?;
    let code = backend.compile(func).map_err(|e| fail(e.to_string()))?;
    Ok(Thunk {
        guest: guest_va,
        host: code as usize as u64,
        _backend: backend,
    })
}

/// The guest's argument registers, being filled.
///
/// SysV AMD64 allocates them in declaration order and — the rule that matters for
/// aggregates — moves the *whole* aggregate to memory if the registers it needs are not
/// all still free.
#[derive(Default)]
struct GuestArgs {
    ints: Vec<u64>,
    sse: Vec<u64>,
    stack: Vec<u8>,
}

impl GuestArgs {
    fn push_int(&mut self, v: u64) {
        match self.ints.len() < GUEST_INT_ARGS {
            true => self.ints.push(v),
            false => self.stack.extend_from_slice(&v.to_le_bytes()),
        }
    }

    fn push_sse(&mut self, bits: u64) {
        match self.sse.len() < GUEST_FP_ARGS {
            true => self.sse.push(bits),
            false => self.stack.extend_from_slice(&bits.to_le_bytes()),
        }
    }

    /// Place an aggregate whose bytes are `raw`, by SysV's classification.
    fn push_aggregate(&mut self, agg: &Aggregate, raw: &[u8]) {
        let eightbytes = |raw: &[u8], i: usize| {
            let mut word = [0u8; 8];
            let start = i * 8;
            let take = raw.len().saturating_sub(start).min(8);
            word[..take].copy_from_slice(&raw[start..start + take]);
            u64::from_le_bytes(word)
        };
        match agg.guest_class() {
            GuestClass::Registers(classes) => {
                let wanted_int = classes.iter().filter(|c| **c == Eightbyte::Integer).count();
                let wanted_sse = classes.len() - wanted_int;
                // All or nothing: see the type note.
                if self.ints.len() + wanted_int <= GUEST_INT_ARGS
                    && self.sse.len() + wanted_sse <= GUEST_FP_ARGS
                {
                    for (i, class) in classes.iter().enumerate() {
                        let word = eightbytes(raw, i);
                        match class {
                            Eightbyte::Integer => self.ints.push(word),
                            Eightbyte::Sse => self.sse.push(word),
                        }
                    }
                    return;
                }
            }
            GuestClass::Memory => {}
        }
        // On the stack, eight-byte aligned, padded to a whole number of words.
        while !self.stack.len().is_multiple_of(8) {
            self.stack.push(0);
        }
        self.stack.extend_from_slice(raw);
        while !self.stack.len().is_multiple_of(8) {
            self.stack.push(0);
        }
    }
}

/// Rebuild an aggregate's bytes from the scalars AAPCS64 delivered.
///
/// An HFA arrives as its members, one per `v` register, so each goes back at its own
/// offset. A small composite arrives as raw words and its bytes are already the
/// struct's.
fn reassemble(agg: &Aggregate, host: HostClass, words: &[u64]) -> Vec<u8> {
    let mut raw = vec![0u8; agg.size];
    match host {
        HostClass::Hfa { ty, count } => {
            let width = crate::abi::aggregate::width_of(ty);
            for (leaf, word) in agg.leaves.iter().take(count).zip(words) {
                let end = (leaf.offset + width).min(raw.len());
                if leaf.offset < end {
                    raw[leaf.offset..end].copy_from_slice(&word.to_le_bytes()[..end - leaf.offset]);
                }
            }
        }
        HostClass::Integer { words: n } => {
            for i in 0..n {
                let start = i * 8;
                let end = (start + 8).min(raw.len());
                if start < end {
                    let word = words.get(i).copied().unwrap_or(0);
                    raw[start..end].copy_from_slice(&word.to_le_bytes()[..end - start]);
                }
            }
        }
        HostClass::Indirect => {}
    }
    raw
}

/// What a typed trampoline calls. Not public: its only legitimate caller is code this
/// module emitted.
unsafe extern "C" fn typed_entry(
    guest_va: u64,
    desc: *const ThunkSignature,
    spill: *const u64,
    out: *mut u64,
) {
    // SAFETY: `out` is an eight-byte stack slot of the calling trampoline.
    unsafe { *out = 0 };
    let Some(vm) = current() else {
        report_no_vm(guest_va);
        return;
    };
    if !vm.program.is_guest_address(guest_va) {
        return;
    }
    // SAFETY: `desc` was leaked at build time and is never freed; `spill` points
    // at a slot the calling trampoline just filled with exactly as many words as
    // `desc.host_leaves` flattens to.
    let (signature, spill) = unsafe {
        let signature = &*desc;
        let n: usize = signature.host_leaves.iter().map(Vec::len).sum();
        (signature, std::slice::from_raw_parts(spill, n))
    };

    let mut args = GuestArgs::default();
    let mut at = 0usize;
    for (shape, leaves) in signature.params.iter().zip(&signature.host_leaves) {
        let words = &spill[at..at + leaves.len()];
        at += leaves.len();
        match shape {
            Shape::Scalar(t) if crate::abi::aggregate::is_fp(*t) => args.push_sse(words[0]),
            Shape::Scalar(_) => args.push_int(words[0]),
            Shape::Struct(agg) => {
                let Ok(host) = agg.host_class() else { return };
                let raw = reassemble(agg, host, words);
                args.push_aggregate(agg, &raw);
            }
        }
    }

    let mut ctx = RegisterContext::new();
    for (slot, &v) in [
        &mut ctx.rdi,
        &mut ctx.rsi,
        &mut ctx.rdx,
        &mut ctx.rcx,
        &mut ctx.r8,
        &mut ctx.r9,
    ]
    .into_iter()
    .zip(&args.ints)
    {
        *slot = v;
    }
    for (i, &bits) in args.sse.iter().enumerate() {
        ctx.xmm[i][0] = bits;
    }
    // `al` is the count of vector registers used, which a variadic callee reads
    // and a fixed one ignores. Costs nothing and is what the ABI says.
    ctx.rax = args.sse.len() as u64;

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        vm.call_with_stack(guest_va, ctx, &args.stack)
    }));
    match result {
        Ok(Ok((rax, xmm0))) => {
            // Which register holds the answer is the *return type's* question,
            // not something to infer from the value.
            let value = match signature.returns.as_ref().and_then(Shape::scalar) {
                Some(t) if crate::abi::aggregate::is_fp(t) => xmm0,
                _ => rax,
            };
            // SAFETY: `out` is the calling trampoline's eight-byte slot.
            unsafe { *out = value };
        }
        Ok(Err(e)) => park_or_die(vm, in_callback(guest_va, e)),
        Err(_) => park_or_die(
            vm,
            VmError::Thunk {
                guest: guest_va,
                reason: "the guest callback panicked the runtime".into(),
            },
        ),
    }
}

/// The status a callback failure nothing can raise ends the process with.
const CALLBACK_FAILURE_STATUS: i32 = 70;

/// Park a callback's failure — or, when nothing will ever take it, say so and stop the
/// process.
fn park_or_die(vm: &Vm, err: VmError) {
    if crate::thread::current().is_some_and(|s| s.depth.get() > 0) {
        vm.set_pending(err);
        return;
    }
    // Said here rather than through `set_pending`, which deduplicates and caps:
    // the last thing printed before the process ends may not be suppressed.
    eprintln!("rosetta3: {err}");
    eprintln!(
        "rosetta3: that was the outermost guest frame on this thread, so nothing \
         will raise it — the callback would return to the host and this thread \
         would go quiet. Exiting."
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // SAFETY: `_exit` ends the process immediately. No invariant of this
    // runtime has to survive it, which is the point of choosing it.
    unsafe { libc::_exit(CALLBACK_FAILURE_STATUS) };
}

/// Name a failure by the callback it happened in — once.
fn in_callback(guest_va: u64, e: VmError) -> VmError {
    match e {
        already @ VmError::Callback { .. } => already,
        e => VmError::Callback {
            guest: guest_va,
            reason: e.to_string(),
        },
    }
}

/// Said once, from either entry: a callback ran with no VM to enter.
fn report_no_vm(guest_va: u64) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        eprintln!(
            "rosetta3: a guest callback at {guest_va:#x} ran with no VM reachable; \
             an embedder whose guest spawns threads or registers `atexit` handlers \
             must call `rosetta3::thunk::publish`"
        );
    });
}

/// What every thunk calls. Not public: its only legitimate caller is code this module
/// emitted.
///
/// An error here cannot be returned — the host function in the middle (`qsort`, say)
/// has no way to express one — so it is parked on the VM and raised the moment control
/// is back in the runtime.
unsafe extern "C" fn thunk_entry(guest_va: u64, ints: *const u64, floats: *const f64) -> u64 {
    let Some(vm) = current() else {
        // Nowhere to report this — the caller is `qsort`, or libpthread's
        // thread start, and neither has an error channel — so it is said out
        // loud once and the callback returns zero.
        report_no_vm(guest_va);
        return 0;
    };

    // The VM that answered may not be the one that built this thunk: a process
    // can run more than one, and the process-lifetime fallback holds whichever
    // was published last.
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
            park_or_die(vm, in_callback(guest_va, e));
            0
        }
        Err(_) => {
            park_or_die(
                vm,
                VmError::Thunk {
                    guest: guest_va,
                    reason: "the guest callback panicked the runtime".into(),
                },
            );
            0
        }
    }
}
