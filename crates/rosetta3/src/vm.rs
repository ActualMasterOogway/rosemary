//! The execution loop: lift, enter, service the exit, repeat.
//!
//! Milestone 2 supplied one arm of this `match` and nothing that drives it.
//! This is the driver.
//!
//! ```text
//! loop {
//!     let code = cache.get_or_lift(pc);
//!     match code.enter(&mut ctx) {
//!         Return               => pc = ctx.rip,  // or the call is finished
//!         UnknownJump          => pc = ctx.rip,
//!         ImportedFunctionCall => { call_host(...); pc = ctx.rip }
//!         IndirectCall         => pc = dispatch(...),
//!         ...
//!     }
//! }
//! ```
//!
//! Every exit that is not one of those is a *stop*, named and reported. There
//! is no arm that guesses.
//!
//! ## Why the VM is entered through `&self`
//!
//! A guest call can re-enter the VM. `qsort` is the smallest example: the guest
//! calls it, the runtime calls the host's `qsort`, and the host calls back into
//! a guest comparator — while the outer call is still on the stack. If running
//! the guest took `&mut self`, that re-entry would be a second mutable borrow
//! of the same object, and no amount of care would make it legal.
//!
//! So execution takes `&self` and the mutable state is behind [`RefCell`]s held
//! only across single operations, never across an entry into guest or host
//! code. Guest *memory* is exempt from this and reached through [`GuestView`],
//! because it is genuinely shared: the identity mapping means the host's own
//! `libc` writes into guest buffers whenever a shim forwards a call, and
//! pretending Rust owns those bytes exclusively would be a fiction.
//!
//! ## The stack the guest runs on
//!
//! One [`GuestStack`], and nested entries continue below the innermost frame,
//! which is what a real nested call does. A live stack pointer is tracked and
//! updated at every exit, so a host→guest thunk entered from inside a
//! host function starts below the guest frame that made the call rather than
//! back at the top of the stack, where it would overwrite live frames.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

use machish::memory::{MmapSpace, Regions};

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::exit::{ExitReason, TransferKind};

use crate::abi::{AbiError, GuestMemory};
use crate::cache::CodeCache;
use crate::program::Program;
use crate::stack::{GuestStack, RETURN_SENTINEL};
use crate::thunk::ThunkTable;

/// Anything that stops the guest.
#[derive(Debug)]
pub enum VmError {
    /// Nothing at this address decodes as guest code, or it is not mapped.
    NoCode { pc: u64 },
    /// Microlift could not translate the region — an instruction it does not
    /// implement, or a compilation failure. Named with the region it was
    /// lifting, because that is the unit of work to look at.
    Lift { pc: u64, start: u64, source: microlift::MicroliftError },
    /// An intercepted import could not be called. See [`AbiError`]: every
    /// variant names the symbol.
    Abi(AbiError),
    /// An indirect call to an address that is neither guest code nor anything
    /// the runtime can describe well enough to call.
    UnknownIndirectCall { callee: u64, resume: u64 },
    /// The guest executed a `syscall` instruction. A stop rather than a
    /// passthrough: under high-level emulation the host's libSystem issues the
    /// real syscalls, so reaching one means hand-written guest assembly, and
    /// translating the x86-64 numbering is a fallback to write when something
    /// needs it.
    Syscall { number: u64, pc: u64 },
    /// An exit the runtime has nothing to do about: an unimplemented or invalid
    /// instruction, a trap, a divide error.
    Trap { reason: ExitReason, code: u32, pc: u64 },
    /// The guest stack could not be reserved or written.
    Stack { source: machish::Error },
    /// A host→guest thunk could not be built.
    Thunk { guest: u64, reason: String },
    /// The program could not be loaded or linked.
    Load { path: String, reason: String },
    /// Guest execution nested deeper than [`MAX_DEPTH`] host→guest callbacks.
    TooDeep { pc: u64 },
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::NoCode { pc } => write!(f, "no guest code at {pc:#x}"),
            VmError::Lift { pc, start, source } => {
                write!(f, "lifting the region at {start:#x} (for pc {pc:#x}): {source}")
            }
            VmError::Abi(e) => write!(f, "{e}"),
            VmError::UnknownIndirectCall { callee, resume } => write!(
                f,
                "indirect call to {callee:#x} from {resume:#x}: neither guest code nor a \
                 described host function"
            ),
            VmError::Syscall { number, pc } => {
                write!(f, "guest syscall {number:#x} at {pc:#x}: not translated")
            }
            VmError::Trap { reason, code, pc } => {
                write!(f, "guest stopped at {pc:#x}: {reason} (code {code:#x})")
            }
            VmError::Stack { source } => write!(f, "guest stack: {source}"),
            VmError::Thunk { guest, reason } => {
                write!(f, "no host->guest thunk for {guest:#x}: {reason}")
            }
            VmError::Load { path, reason } => write!(f, "{path}: {reason}"),
            VmError::TooDeep { pc } => {
                write!(f, "guest re-entered {MAX_DEPTH} deep at {pc:#x}")
            }
        }
    }
}

impl std::error::Error for VmError {}

impl From<AbiError> for VmError {
    fn from(e: AbiError) -> Self {
        VmError::Abi(e)
    }
}

/// How many host→guest callbacks may nest before the runtime calls it runaway.
///
/// Totality again: each level costs a host stack frame, and unbounded guest
/// control over that is a stack overflow in the *host*, which is not a
/// diagnosable failure.
pub const MAX_DEPTH: usize = 64;

/// Guest memory, as a value that can be handed to a shim without borrowing the
/// VM.
///
/// The guest is identity-mapped, so this is nothing but a bounds check in front
/// of a raw pointer — which is the whole point: a shim servicing `snprintf`
/// writes into the guest's buffer, and that buffer is host memory the host's
/// own `libc` could equally write into.
///
/// ## Addresses outside the guest are permitted, and why
///
/// A guest under high-level emulation routinely holds pointers to *host*
/// memory: `strerror` returns one, `__stdoutp` is one, every host structure the
/// guest was handed is one. `printf("%s", strerror(errno))` is an ordinary
/// program doing an ordinary thing, and refusing to read the string because it
/// is not in a guest region would break it for no reason.
///
/// So an address outside every guest region is read as a host pointer, after
/// checking that anything is mapped there at all. The check is `msync`, which
/// is an odd-looking choice and the one that works: `mincore` on macOS reports
/// residency and returns success for an address with nothing mapped at it,
/// while `msync` fails with `ENOMEM` for exactly the range that has no mapping.
/// On a private anonymous mapping — which is what the guest's memory is — a
/// flush request has nothing to do, so the probe has no effect beyond its
/// answer.
///
/// It answers "is anything here", not "may I read this", which is enough to
/// turn the common bug — a null or wild pointer — into a named error instead of
/// a fault. It is not a guarantee. Results are cached per page, because
/// `read_cstr` walks a byte at a time.
#[derive(Clone)]
pub struct GuestView {
    regions: std::rc::Rc<Vec<Range<u64>>>,
    mapped: std::rc::Rc<RefCell<BTreeSet<u64>>>,
}

impl GuestView {
    /// A view over `regions`, which must be identity-mapped host memory.
    pub fn new(regions: Vec<Range<u64>>) -> Self {
        GuestView {
            regions: std::rc::Rc::new(regions),
            mapped: std::rc::Rc::new(RefCell::new(BTreeSet::new())),
        }
    }

    /// Whether `va` is inside a mapped guest region.
    pub fn contains(&self, va: u64) -> bool {
        self.regions.iter().any(|r| r.contains(&va))
    }

    /// Check `[va, va+len)` is addressable, and return the host pointer.
    fn resolve(&self, va: u64, len: usize) -> Option<*mut u8> {
        let end = va.checked_add(len as u64)?;
        if va == 0 {
            return None;
        }
        let inside = self.regions.iter().any(|r| r.contains(&va) && end <= r.end);
        if !inside && !self.host_readable(va, end) {
            return None;
        }
        usize::try_from(va).ok().map(|a| a as *mut u8)
    }

    /// Whether every page of `[va, end)` is mapped in this process.
    fn host_readable(&self, va: u64, end: u64) -> bool {
        let page = machish::memory::host_page_size();
        let mut at = va & !(page - 1);
        while at < end {
            if !self.mapped.borrow().contains(&at) {
                if !page_is_mapped(at, page) {
                    return false;
                }
                self.mapped.borrow_mut().insert(at);
            }
            at += page;
        }
        true
    }
}

/// Whether the host has anything mapped at `[at, at+len)`.
fn page_is_mapped(at: u64, len: u64) -> bool {
    let Ok(addr) = usize::try_from(at) else { return false };
    // SAFETY: `msync` inspects the process's own mappings and, for a private
    // anonymous one, has nothing to write back. An unmapped address is
    // reported as `ENOMEM` rather than faulting, which is the entire reason
    // for asking this way.
    let rc = unsafe {
        libc::msync(addr as *mut libc::c_void, len as libc::size_t, libc::MS_ASYNC)
    };
    rc == 0
}

impl GuestMemory for GuestView {
    fn read(&self, va: u64, out: &mut [u8]) -> Result<(), machish::Error> {
        let src = self
            .resolve(va, out.len())
            .ok_or(machish::Error::OutOfBounds { addr: va, len: out.len() })?;
        // SAFETY: `resolve` checked that `[va, va+len)` lies in a guest region
        // this VM keeps mapped, or in a page the host has mapped. Guest and
        // host share one address space by construction, so the address is the
        // pointer.
        unsafe { std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), out.len()) };
        Ok(())
    }

    fn write(&mut self, va: u64, data: &[u8]) -> Result<(), machish::Error> {
        let dst = self
            .resolve(va, data.len())
            .ok_or(machish::Error::OutOfBounds { addr: va, len: data.len() })?;
        // SAFETY: as for `read`. Writability is the guest's business: a store
        // to a page it made read-only faults exactly as it would have on x86.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
        Ok(())
    }
}

/// A loaded program and everything needed to run it.
pub struct Vm {
    /// The linked guest, identity-mapped. Immutable once loaded: guest *memory*
    /// changes constantly, but through [`GuestView`], not through this.
    pub program: Program<MmapSpace>,
    stack: RefCell<GuestStack>,
    cache: RefCell<CodeCache>,
    thunks: RefCell<ThunkTable>,
    view: GuestView,
    /// Where a nested guest entry should put its first frame.
    guest_rsp: Cell<u64>,
    depth: Cell<usize>,
    /// An error raised inside a host→guest callback, which cannot be returned
    /// through the host function that caused it. Collected here and re-raised
    /// as soon as control is back in the runtime.
    pending: RefCell<Option<VmError>>,
}

impl Vm {
    /// Load `path`, identity-mapped, with a guest stack and `argv`.
    pub fn load(path: &str, args: &[String]) -> Result<Self, VmError> {
        let program = Program::load_executable(path, &[])
            .map_err(|e| VmError::Load { path: path.to_string(), reason: e.to_string() })?;
        Self::new(program, path, args)
    }

    /// Wrap an already-linked program.
    pub fn new(
        program: Program<MmapSpace>,
        exe_path: &str,
        args: &[String],
    ) -> Result<Self, VmError> {
        Self::with_policy(program, exe_path, args, crate::ImportPolicy::default())
    }

    /// As [`Vm::new`], choosing how imports reach the host.
    ///
    /// [`ImportPolicy::AlwaysSlow`](crate::ImportPolicy::AlwaysSlow) is what
    /// makes milestone 2's claim testable *end to end* rather than by
    /// inspection: fast imports are an optimisation of slow ones, so the same
    /// program run both ways must produce the same output and differ only in
    /// how many VM exits it took to get there.
    pub fn with_policy(
        program: Program<MmapSpace>,
        exe_path: &str,
        args: &[String],
        policy: crate::ImportPolicy,
    ) -> Result<Self, VmError> {
        let stack = GuestStack::new(crate::stack::DEFAULT_STACK, exe_path, args)?;

        let mut ranges: Vec<Range<u64>> = program
            .memory
            .regions()
            .into_iter()
            .map(|(base, bytes)| base..base + bytes.len() as u64)
            .collect();
        ranges.push(stack.range());

        let cache = CodeCache::new(program.lifter_config_with(policy));
        let rsp = stack.initial_rsp();
        Ok(Vm {
            program,
            stack: RefCell::new(stack),
            cache: RefCell::new(cache),
            thunks: RefCell::new(ThunkTable::new()),
            view: GuestView::new(ranges),
            guest_rsp: Cell::new(rsp),
            depth: Cell::new(0),
            pending: RefCell::new(None),
        })
    }

    /// Guest memory, as a value a shim can hold.
    pub fn view(&self) -> GuestView {
        self.view.clone()
    }

    /// How many regions have been translated so far.
    pub fn translated(&self) -> usize {
        self.cache.borrow().len()
    }

    /// `(argc, argv, envp, apple)` for the entry point and the initializers.
    pub fn main_args(&self) -> (u64, u64, u64, u64) {
        self.stack.borrow().main_args()
    }

    /// Call a guest function with up to six integer arguments, and return
    /// whatever it leaves in `rax`.
    ///
    /// This is the *only* way into guest code, from the entry point, from an
    /// initializer, and from a host→guest thunk alike — so the argument
    /// placement, the sentinel return address, and the exit loop have exactly
    /// one implementation.
    pub fn call(&self, entry: u64, args: &[u64]) -> Result<u64, VmError> {
        let mut ctx = RegisterContext::new();
        let regs = [&mut ctx.rdi, &mut ctx.rsi, &mut ctx.rdx, &mut ctx.rcx, &mut ctx.r8];
        for (slot, &arg) in regs.into_iter().zip(args) {
            *slot = arg;
        }
        if let Some(&sixth) = args.get(5) {
            ctx.r9 = sixth;
        }
        self.call_with(entry, ctx)
    }

    /// As [`Vm::call`], with the whole initial register context supplied — the
    /// form a thunk needs, since it has floating-point arguments to place too.
    pub fn call_with(&self, entry: u64, mut ctx: RegisterContext) -> Result<u64, VmError> {
        if self.depth.get() >= MAX_DEPTH {
            return Err(VmError::TooDeep { pc: entry });
        }

        // Below the innermost live frame, past the red zone the guest's ABI
        // lets a leaf function use without adjusting `rsp`.
        let rsp = ((self.guest_rsp.get() - 128) & !0xf) - 8;
        self.view().write(rsp, &RETURN_SENTINEL.to_le_bytes()).map_err(|source| {
            VmError::Stack { source }
        })?;
        ctx.rsp = rsp;
        ctx.rip = entry;

        let saved_rsp = self.guest_rsp.get();
        self.guest_rsp.set(rsp);
        self.depth.set(self.depth.get() + 1);
        // Publish the VM for the duration: a thunk called from inside a host
        // function has no other way to find it. See `crate::thunk`.
        let result = crate::thunk::with_current(self, || self.run_loop(&mut ctx));
        self.depth.set(self.depth.get() - 1);
        self.guest_rsp.set(saved_rsp);
        result
    }

    /// Enter regions and service exits until the call that started this returns.
    fn run_loop(&self, ctx: &mut RegisterContext) -> Result<u64, VmError> {
        loop {
            let pc = ctx.rip;

            // Control *jumped* to an import rather than calling one: a tail
            // call, which compilers emit freely — `jmp _strcoll` where the
            // return value is the caller's. Microlift intercepts `CALL`s to a
            // registered import and not `JMP`s, correctly: a jump to a stub is
            // an ordinary transfer as far as the lifter is concerned. The
            // runtime has to notice, because "lift the target" would decode the
            // six-byte `__stubs` trampoline as guest code and follow it into
            // the host address in its slot.
            //
            // A tail call is a call and a return, so both halves happen here.
            if self.program.import_at(pc).is_some() {
                ctx.imported_function_va = pc;
                self.service_import(ctx)?;
                let mut ret = [0u8; 8];
                self.view()
                    .read(ctx.rsp, &mut ret)
                    .map_err(|source| VmError::Stack { source })?;
                ctx.rsp = ctx.rsp.wrapping_add(8);
                let ret = u64::from_le_bytes(ret);
                if ret == RETURN_SENTINEL {
                    return Ok(ctx.rax);
                }
                ctx.rip = ret;
                continue;
            }

            let translation = self.cache.borrow_mut().get_or_lift(&self.program, pc)?;
            ctx.rip = pc;
            let exit = translation.enter(ctx);
            self.guest_rsp.set(ctx.rsp);

            if let Some(done) = self.step(ctx, exit)? {
                return Ok(done);
            }
            if let Some(err) = self.pending.borrow_mut().take() {
                return Err(err);
            }
        }
    }

    /// Service one VM exit. `Ok(Some(v))` means the call is finished with `v`.
    ///
    /// ## What each arm assumes about the stack
    ///
    /// Microlift leaves the guest stack exactly as the instruction would have:
    /// an intercepted `CALL` has *not* pushed a return address (the call never
    /// executed), while an unregistered direct call *has*. So the only arm that
    /// touches `rsp` is the indirect call to guest code, where the runtime is
    /// standing in for a `CALL` that microlift declined to perform.
    fn step(
        &self,
        ctx: &mut RegisterContext,
        exit: ExitReason,
    ) -> Result<Option<u64>, VmError> {
        match exit {
            // The guest returned to the sentinel this VM pushed: the function
            // it called is finished. Any other `Return` is a return into a
            // region we simply have not translated yet.
            ExitReason::Return if ctx.rip == RETURN_SENTINEL => Ok(Some(ctx.rax)),
            ExitReason::Return => Ok(None),

            // Control left the region: a jump, a return into a region not yet
            // translated, or a call to one. The stack is already right in every
            // case — a direct call pushed its own return address — but *where*
            // to continue differs, and the difference is easy to get wrong in a
            // way nothing reports.
            //
            // For a direct call the guest's next instruction is the **callee**,
            // held in `imported_function_va`, while `rip` is the resume point
            // the callee will eventually return to. Continuing at `rip` would
            // skip the call and carry on as if it had happened: no fault, no
            // diagnostic, and a program that silently does nothing.
            ExitReason::UnknownJump => match TransferKind::try_from(ctx.exit_code) {
                Ok(TransferKind::DirectCall) => {
                    ctx.rip = ctx.imported_function_va;
                    Ok(None)
                }
                Ok(_) => Ok(None),
                Err(()) => Err(VmError::Trap {
                    reason: exit,
                    code: ctx.exit_code,
                    pc: ctx.rip,
                }),
            },

            ExitReason::ImportedFunctionCall => {
                self.service_import(ctx)?;
                Ok(None)
            }

            ExitReason::IndirectCall => {
                self.dispatch_indirect(ctx)?;
                Ok(None)
            }

            // Under high-level emulation the host's libSystem issues the real
            // syscalls natively, so a guest `syscall` means hand-written
            // assembly. Translating the x86-64 numbering is the fallback, not
            // the main path, and it is not written until something needs it:
            // the number is reported instead of guessed at.
            ExitReason::Syscall => {
                Err(VmError::Syscall { number: ctx.rax, pc: ctx.rip })
            }

            _ => Err(VmError::Trap { reason: exit, code: ctx.exit_code, pc: ctx.rip }),
        }
    }

    /// Call the host function an intercepted import resolved to.
    fn service_import(&self, ctx: &mut RegisterContext) -> Result<(), VmError> {
        let va = ctx.imported_function_va;
        let Some(site) = self.program.import_at(va) else {
            return Err(AbiError::NoSignature { symbol: format!("<unregistered {va:#x}>") }.into());
        };
        let mut mem = self.view();
        crate::abi::call_import_with(
            &site.symbol,
            site.tag,
            site.resolved,
            ctx,
            &mut mem,
            self,
        )?;
        Ok(())
    }

    /// Decide what an indirect call through a runtime-computed address means.
    ///
    /// The question the identity mapping forces on every one of them: a guest
    /// address must be entered through the JIT, a host address must be called
    /// natively, and there is nothing in the pointer to say which. Getting it
    /// backwards in either direction is a crash with no useful diagnostic, so
    /// the answer is looked up rather than inferred from the value.
    fn dispatch_indirect(&self, ctx: &mut RegisterContext) -> Result<(), VmError> {
        let callee = ctx.imported_function_va;
        let resume = ctx.rip;

        // A pointer read out of an import slot: the same host function the
        // import would have reached, so service it identically.
        if self.program.import_at(callee).is_some() {
            return self.service_import(ctx);
        }

        // One of our own thunks, handed out earlier and now called by the guest
        // through a variable. Calling it would be correct and absurd — out to
        // arm64 and straight back in — so the wrapper is unwrapped.
        let target = self.thunks.borrow().guest_for(callee).unwrap_or(callee);

        if self.program.is_guest_address(target) {
            // Stand in for the `CALL` microlift declined to perform.
            let rsp = ctx.rsp - 8;
            self.view()
                .write(rsp, &resume.to_le_bytes())
                .map_err(|source| VmError::Stack { source })?;
            ctx.rsp = rsp;
            ctx.rip = target;
            return Ok(());
        }

        Err(VmError::UnknownIndirectCall { callee, resume })
    }

    /// A native arm64 address that re-enters the guest at `guest_va`.
    ///
    /// See [`crate::thunk`]. Created once per distinct guest pointer and kept
    /// for the VM's lifetime, so the host may compare two thunks for equality
    /// and get the answer the guest would have.
    pub fn thunk_for(&self, guest_va: u64) -> Result<u64, VmError> {
        if let Some(existing) = self.thunks.borrow().host_for(guest_va) {
            return Ok(existing);
        }
        let thunk = crate::thunk::build(guest_va)?;
        let host = self.thunks.borrow_mut().insert(thunk);

        // Translate the callback now, while translating is possible.
        //
        // This is the one place the JIT translates something it is not about to
        // execute, and the reason is that handing a pointer out *is* the demand
        // signal: the host may call it at any moment, including one where the
        // runtime cannot compile. Process exit is exactly that moment — macOS
        // tears the main thread's thread-local storage down inside `exit`, and
        // Cranelift needs its own thread-locals to compile — so a guest
        // `atexit` handler translated on first call would be translated too
        // late. A failure here is left for the call to report, where the error
        // has somewhere to go.
        let _ = self.cache.borrow_mut().get_or_lift(&self.program, guest_va);
        Ok(host)
    }

    /// How many guest function pointers have been handed to host code.
    pub fn thunks(&self) -> usize {
        self.thunks.borrow().len()
    }

    /// Whether `va` is a thunk this VM handed out.
    pub fn guest_behind_thunk(&self, va: u64) -> Option<u64> {
        self.thunks.borrow().guest_for(va)
    }

    /// Record an error raised inside a host→guest callback.
    pub(crate) fn set_pending(&self, err: VmError) {
        let mut slot = self.pending.borrow_mut();
        if slot.is_none() {
            *slot = Some(err);
        }
    }

    /// Take whatever error a callback left behind.
    pub fn take_pending(&self) -> Option<VmError> {
        self.pending.borrow_mut().take()
    }
}

impl crate::abi::Callbacks for Vm {
    fn substitute(&self, symbol: &str, va: u64) -> Result<u64, AbiError> {
        if va == 0 {
            return Ok(0);
        }
        // Not guest code: a host pointer the guest is handing back, which the
        // host may compare against one it gave out. Wrapping it would be both
        // slower and observably wrong.
        if !self.program.is_guest_address(va) {
            return Ok(va);
        }
        self.thunk_for(va).map_err(|e| AbiError::Refused {
            symbol: symbol.to_string(),
            reason: match e {
                VmError::Thunk { .. } => "a host->guest thunk could not be built",
                _ => "the guest function pointer could not be made callable",
            },
        })
    }
}
