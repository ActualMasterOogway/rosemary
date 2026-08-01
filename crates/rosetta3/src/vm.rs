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
//! So execution takes `&self` and the mutable state is behind locks held only
//! across single operations, never across an entry into guest or host code.
//! Guest *memory* is exempt from this and reached through [`GuestView`], because
//! it is genuinely shared: the identity mapping means the host's own `libc`
//! writes into guest buffers whenever a shim forwards a call, and pretending
//! Rust owns those bytes exclusively would be a fiction.
//!
//! Milestone 4 turned those `RefCell`s into `Mutex`es, because the same
//! re-entrancy argument reappears between *threads*: the host may call a thunk
//! on a thread this runtime has never seen, while another thread is inside the
//! loop. The lock discipline is unchanged — take it, do one thing, drop it — and
//! the reason it is enough is that no lock is ever held across guest code.
//!
//! ## The stack the guest runs on
//!
//! One [`GuestStack`] **per thread**, in [`crate::thread`], and nested entries
//! continue below the innermost frame, which is what a real nested call does. A
//! live stack pointer is tracked and updated at every exit, so a host→guest
//! thunk entered from inside a host function starts below the guest frame that
//! made the call rather than back at the top of the stack, where it would
//! overwrite live frames.
//!
//! The per-thread part is not an elaboration of that; it is the same rule
//! applied one level out. Two guest threads sharing one stack would interleave
//! their frames exactly as two nested calls sharing one *offset* would, and with
//! as little to report about it afterwards.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

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
    ///
    /// `from` is how control got here, and it is the difference between an
    /// address and a lead. A garbage `rip` reached by `Return` is a corrupted
    /// return address — look at the stack. The same value reached by
    /// `UnknownJump` is a computed branch — look at the lift. The address alone
    /// cannot tell those apart, and they are not the same bug.
    NoCode { pc: u64, from: Option<Transfer> },
    /// Microlift could not translate the region — an instruction it does not
    /// implement, or a compilation failure. Named with the region it was
    /// lifting, because that is the unit of work to look at.
    Lift {
        pc: u64,
        start: u64,
        source: microlift::MicroliftError,
    },
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
    Trap {
        reason: ExitReason,
        code: u32,
        pc: u64,
    },
    /// The guest stack could not be reserved or written.
    Stack { source: machish::Error },
    /// A host→guest thunk could not be built.
    Thunk { guest: u64, reason: String },
    /// A guest callback the *host* invoked failed partway through. `guest` is
    /// the address the thunk enters at, which is the only thing that says which
    /// of a framework's many installed callbacks was running.
    Callback { guest: u64, reason: String },
    /// The program could not be loaded or linked.
    Load { path: String, reason: String },
    /// Guest execution nested deeper than [`MAX_DEPTH`] host→guest callbacks.
    TooDeep { pc: u64 },
}

/// How control reached the address that stopped the guest.
///
/// Recorded by the run loop for one exit only — the one immediately before —
/// because that is what a lost `rip` needs and keeping more would be a trace
/// rather than a diagnostic.
#[derive(Debug, Clone)]
pub struct Transfer {
    /// The region control left, as a *slid* address.
    pub pc: u64,
    /// The exit that transferred it, and microlift's sub-code.
    pub exit: ExitReason,
    pub code: u32,
    /// The guest stack pointer as the region left it. A `Return` to nonsense
    /// with a plausible `rsp` is a wrong value written to the stack; one with an
    /// implausible `rsp` is a lost frame.
    pub rsp: u64,
    /// `pc` as `image+offset`, filled in only on the error path.
    ///
    /// The mapping is chosen by `mmap` and moves every run, so a slid address is
    /// not something two runs — or a run and a disassembler — can be compared
    /// against. This is the form that can.
    pub at: Option<String>,
}

impl fmt::Display for Transfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reached from {:#x}", self.pc)?;
        if let Some(at) = &self.at {
            write!(f, " ({at})")?;
        }
        write!(
            f,
            " by {} (code {:#x}), rsp {:#x}",
            self.exit, self.code, self.rsp
        )
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::NoCode { pc, from: None } => write!(f, "no guest code at {pc:#x}"),
            VmError::NoCode {
                pc,
                from: Some(from),
            } => write!(f, "no guest code at {pc:#x}: {from}"),
            VmError::Lift { pc, start, source } => {
                write!(
                    f,
                    "lifting the region at {start:#x} (for pc {pc:#x}): {source}"
                )
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
            VmError::Callback { guest, reason } => {
                write!(f, "in the guest callback at {guest:#x}: {reason}")
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
///
/// ## What is *not* in the region list
///
/// The stacks of threads the guest spawned. Those are reserved per thread as
/// control reaches them (see [`crate::thread`]), and threading them into a
/// shared region list would put a lock in front of every byte of every
/// `read_cstr`. They do not need to be: a spawned stack is host memory the host
/// has mapped, so it takes the same path a `strerror` result already takes, at
/// the cost of one probe per page instead of a range check.
#[derive(Clone)]
pub struct GuestView {
    regions: Arc<Vec<Range<u64>>>,
    mapped: Arc<Mutex<BTreeSet<u64>>>,
}

impl GuestView {
    /// A view over `regions`, which must be identity-mapped host memory.
    pub fn new(regions: Vec<Range<u64>>) -> Self {
        GuestView {
            regions: Arc::new(regions),
            mapped: Arc::new(Mutex::new(BTreeSet::new())),
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
            let known = self
                .mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&at);
            if !known {
                if !page_is_mapped(at, page) {
                    return false;
                }
                self.mapped
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(at);
            }
            at += page;
        }
        true
    }
}

/// Whether the host has anything mapped at `[at, at+len)`.
fn page_is_mapped(at: u64, len: u64) -> bool {
    let Ok(addr) = usize::try_from(at) else {
        return false;
    };
    // SAFETY: `msync` inspects the process's own mappings and, for a private
    // anonymous one, has nothing to write back. An unmapped address is
    // reported as `ENOMEM` rather than faulting, which is the entire reason
    // for asking this way.
    let rc = unsafe {
        libc::msync(
            addr as *mut libc::c_void,
            len as libc::size_t,
            libc::MS_ASYNC,
        )
    };
    rc == 0
}

impl GuestMemory for GuestView {
    fn read(&self, va: u64, out: &mut [u8]) -> Result<(), machish::Error> {
        let src = self
            .resolve(va, out.len())
            .ok_or(machish::Error::OutOfBounds {
                addr: va,
                len: out.len(),
            })?;
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
            .ok_or(machish::Error::OutOfBounds {
                addr: va,
                len: data.len(),
            })?;
        // SAFETY: as for `read`. Writability is the guest's business: a store
        // to a page it made read-only faults exactly as it would have on x86.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
        Ok(())
    }
}

/// A loaded program and everything needed to run it.
///
/// ## What is shared between guest threads, and what is not
///
/// Everything in this struct is shared, because everything in it is either
/// immutable or genuinely common to every thread: a translation does not depend
/// on which thread runs it, and a guest function pointer must map to the *same*
/// thunk for every thread that sees it, or the guest could compare two and find
/// them unequal.
///
/// The per-thread half — a guest stack, the live stack pointer, the callback
/// nesting depth, the parked error — lives in [`crate::thread`]. Splitting them
/// that way is the whole of milestone 4's execution model: one guest thread per
/// host thread, sharing an address space and a code cache and nothing else.
pub struct Vm {
    /// The linked guest, identity-mapped. Immutable once loaded: guest *memory*
    /// changes constantly, but through [`GuestView`], not through this.
    pub program: SharedProgram,
    cache: Mutex<CodeCache>,
    thunks: Mutex<ThunkTable>,
    view: GuestView,
    /// The guest's Objective-C classes, once realized in the host runtime.
    ///
    /// A `OnceLock` rather than a `Mutex`, because registration happens exactly
    /// once — before any guest code runs, from [`crate::startup`] — and every
    /// later use is a read from whichever thread is dispatching a message. That
    /// is the same lifetime shape as the code cache's contents and a much
    /// simpler one than its locking.
    objc: std::sync::OnceLock<crate::objc::ObjcRuntime>,
}

/// A linked program, shareable between the threads running it.
///
/// The `unsafe impl` is deliberately confined to this one field rather than
/// asserted over the whole [`Vm`], so that everything else — the code cache, the
/// thunk table, the guest view — still has to convince the compiler on its own.
/// A blanket claim on `Vm` would have silently permitted an `Rc` inside the
/// cache, which is exactly the bug that would be invisible until two guest
/// threads dropped the same translation.
pub struct SharedProgram(Program<MmapSpace>);

// SAFETY: two claims, and the second is the interesting one.
//
// First, a `Program` is not mutated after construction. What makes it `!Sync`
// automatically is the raw pointers inside its `MmapSpace`, and those are
// read-only handles to mappings that live exactly as long as the `Vm`.
//
// Second, guest *memory* is shared and unsynchronised — deliberately. The guest
// is identity-mapped, its threads run in one address space, and two guest
// threads racing on a guest variable is a race the guest wrote and would have
// had on Intel too. Serialising it would be both impossible and wrong. What this
// runtime owes is that its *own* structures survive concurrent use, which is
// what the mutexes beside this field are for.
unsafe impl Sync for SharedProgram {}
unsafe impl Send for SharedProgram {}

impl std::ops::Deref for SharedProgram {
    type Target = Program<MmapSpace>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// A `Vm` is reached from more than one thread — through
// [`crate::thunk::publish`], which is how a thread the guest spawned finds it —
// and nothing in the type system was demanding it be sound, because a published
// VM travels through an `AtomicPtr` and a raw pointer carries no bounds.
//
// So the requirement is written down, and the compiler checks it. If a field
// ever gains an `Rc`, or a `Mutex` is dropped in favour of a `RefCell`, this
// fails to compile rather than producing a runtime that corrupts a refcount
// under a second guest thread.
const _: () = {
    const fn requires_sync<T: Sync>() {}
    requires_sync::<Vm>();
};

impl Vm {
    /// Load `path`, identity-mapped, with a guest stack and `argv`.
    pub fn load(path: &str, args: &[String]) -> Result<Self, VmError> {
        let program = Program::load_executable(path, &[]).map_err(|e| VmError::Load {
            path: path.to_string(),
            reason: e.to_string(),
        })?;
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

        // Every `_Thread_local` in the program becomes addressable here, before
        // a line of guest code runs — an initializer may touch one. See
        // [`crate::tls`] for why the registry is global rather than a field.
        crate::tls::register(&program.report);
        // Where `@executable_path` points, and where a guest `dlopen` of a bare
        // library name looks. Recorded rather than derived, because the running
        // executable is `rosetta3` — the same fact the bundle mirror exists for.
        crate::dyld::set_executable_path(exe_path);

        let cache = CodeCache::new(program.lifter_config_with(policy));
        // The calling thread becomes the guest's main thread: this is the stack
        // `argv` was laid out on, so it is the only one `main` can run on.
        crate::thread::install(crate::thread::ThreadState::new(stack));
        Ok(Vm {
            program: SharedProgram(program),
            cache: Mutex::new(cache),
            thunks: Mutex::new(ThunkTable::new()),
            view: GuestView::new(ranges),
            objc: std::sync::OnceLock::new(),
        })
    }

    /// Realize the guest's Objective-C classes in the host runtime.
    ///
    /// Idempotent, and deliberately *not* done in [`Vm::with_policy`]: it makes
    /// guest function pointers callable from the host, which means building
    /// thunks, which means compiling. A `Vm` that is only ever inspected should
    /// not pay for that, and the audit path is exactly such a caller.
    pub fn register_objc(&self) -> Result<&crate::objc::ObjcRuntime, VmError> {
        if let Some(existing) = self.objc.get() {
            return Ok(existing);
        }
        let rt = crate::objc::register(self)?;
        // A racing caller's value wins and ours is dropped; both describe the
        // same registration, because the host runtime interns by name.
        let _ = self.objc.set(rt);
        Ok(self.objc.get().expect("just set"))
    }

    /// The registered Objective-C classes, if [`Vm::register_objc`] has run.
    pub fn objc(&self) -> Option<&crate::objc::ObjcRuntime> {
        self.objc.get()
    }

    /// A class pointer on its way *out* to the host runtime.
    ///
    /// The guest names its own class by the address of the dead `objc_class` in
    /// its `__objc_data`; the host runtime knows the class it realized. Anything
    /// that is not a registered guest class — an object, a host class, a null —
    /// is itself on both sides and crosses unchanged. See [`crate::objc`].
    pub fn objc_host_class(&self, va: u64) -> u64 {
        self.objc().map_or(va, |rt| rt.host_or(va))
    }

    /// A class pointer on its way back *in*.
    ///
    /// Not symmetry for its own sake: a guest comparing `[obj class]` against
    /// `Greeter` must find them equal, and the only address it can compare
    /// against is the one it already has.
    pub fn objc_guest_class(&self, host: u64) -> u64 {
        self.objc().map_or(host, |rt| rt.guest_or(host))
    }

    /// Guest memory, as a value a shim can hold.
    pub fn view(&self) -> GuestView {
        self.view.clone()
    }

    /// How many regions have been translated so far.
    pub fn translated(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// `(argc, argv, envp, apple)` for the entry point and the initializers.
    ///
    /// Read from *this* thread's stack, which is where they were laid out — so
    /// this answers on the thread that built the [`Vm`] and reports zeros
    /// anywhere else. A spawned guest thread has no `argv` and no use for one.
    pub fn main_args(&self) -> (u64, u64, u64, u64) {
        crate::thread::current().map_or((0, 0, 0, 0), |t| t.main_args())
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
        let regs = [
            &mut ctx.rdi,
            &mut ctx.rsi,
            &mut ctx.rdx,
            &mut ctx.rcx,
            &mut ctx.r8,
        ];
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
        // A thread the guest spawned reaches here first, through a thunk, with
        // no stack of its own yet.
        let state = crate::thread::ensure()?;
        if state.depth.get() >= MAX_DEPTH {
            return Err(VmError::TooDeep { pc: entry });
        }

        // Below the innermost live frame, past the red zone the guest's ABI
        // lets a leaf function use without adjusting `rsp`.
        let rsp = ((state.rsp.get() - 128) & !0xf) - 8;
        self.view()
            .write(rsp, &RETURN_SENTINEL.to_le_bytes())
            .map_err(|source| VmError::Stack { source })?;
        ctx.rsp = rsp;
        ctx.rip = entry;

        let saved_rsp = state.rsp.get();
        state.rsp.set(rsp);
        state.depth.set(state.depth.get() + 1);
        // This entry gets an identity of its own, which `setjmp` records and
        // `longjmp` checks: a jump back into an *outer* entry would leave the
        // host frames between the two — the host's `qsort`, with a guest
        // comparator above it — on the host's stack with nothing to return
        // through them. See [`crate::jmpbuf`].
        let outer_entry = state.enter();
        // Publish the VM for the duration: a thunk called from inside a host
        // function has no other way to find it. See `crate::thunk`.
        let result = crate::thunk::with_current(self, || self.run_loop(state, &mut ctx));
        state.leave(outer_entry);
        state.depth.set(state.depth.get() - 1);
        state.rsp.set(saved_rsp);
        result
    }

    /// Enter regions and service exits until the call that started this returns.
    fn run_loop(
        &self,
        state: &crate::thread::ThreadState,
        ctx: &mut RegisterContext,
    ) -> Result<u64, VmError> {
        // How control left the previous region, kept for exactly one step. It is
        // what makes a lost `rip` diagnosable: see [`Transfer`].
        let mut from: Option<Transfer> = None;
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
            //
            // The pc may be the import's address or the address it *resolved
            // to*, and the second is not an exotic case. A `jmp qword
            // [rip+disp]` through the slot has no `rel32` for the lifter to
            // match, so it folds the load instead — the slot lives in a region
            // the config marks non-writable and was already bound when the
            // snapshot was taken — and what arrives here is the host function.
            // `objc_msgSend` is where that shows up, because a method whose body
            // is one message (`- (id)a { return [self b]; }`) compiles to
            // exactly that. Following it means running the host's own
            // `objc_msgSend` on the guest's registers.
            if let Some(site) = self
                .program
                .import_at(pc)
                .or_else(|| self.program.import_by_target(pc))
            {
                ctx.imported_function_va = pc;
                self.service_site_as(site, ctx, true)?;
                // Unless the call transferred control instead of returning. A
                // tail-called `jmp _longjmp` arrives here, and by the time the
                // shim is done `rip` is the `setjmp` it restored — popping a
                // return address over it would jump to the caller of the
                // function that longjmped, which is precisely the frame the
                // guest asked to leave. See [`crate::jmpbuf`].
                if state.took_redirect() {
                    continue;
                }
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

            // The lock is taken to look up or lift and released before guest
            // code runs, which is what keeps a callback re-entering the VM on
            // this same thread from deadlocking against its own caller.
            let translation = self
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_lift(&self.program, pc)
                .map_err(|e| match e {
                    VmError::NoCode { pc, from: None } => VmError::NoCode {
                        pc,
                        from: from.map(|mut t| {
                            t.at = self.program.locate(t.pc);
                            t
                        }),
                    },
                    other => other,
                })?;
            ctx.rip = pc;
            let exit = translation.enter(ctx);
            state.rsp.set(ctx.rsp);
            from = Some(Transfer {
                pc,
                exit,
                code: ctx.exit_code,
                rsp: ctx.rsp,
                // Left empty here: this runs on every exit, and rendering a name
                // per exit would be a trace. Filled in where it is reported.
                at: None,
            });

            if let Some(done) = self.step(ctx, exit)? {
                return Ok(done);
            }
            if let Some(err) = state.pending.borrow_mut().take() {
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
    fn step(&self, ctx: &mut RegisterContext, exit: ExitReason) -> Result<Option<u64>, VmError> {
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
                self.service_import(ctx, false)?;
                Ok(None)
            }

            ExitReason::IndirectCall => {
                self.dispatch_indirect(ctx)?;
                Ok(None)
            }

            // The guest asked what CPU it is running on. There is no honest
            // answer — it is running on arm64 — so the runtime answers for the
            // machine microlift actually presents. `rip` is already the
            // instruction after the `CPUID`, so the only work is the reply.
            ExitReason::Cpuid => {
                let [eax, ebx, ecx, edx] = cpuid(ctx.rax as u32, ctx.rcx as u32);
                // `CPUID` writes the 32-bit registers, which zero-extend.
                ctx.rax = eax as u64;
                ctx.rbx = ebx as u64;
                ctx.rcx = ecx as u64;
                ctx.rdx = edx as u64;
                Ok(None)
            }

            // Under high-level emulation the host's libSystem issues the real
            // syscalls natively, so a guest `syscall` means hand-written
            // assembly. Translating the x86-64 numbering is the fallback, not
            // the main path, and it is not written until something needs it:
            // the number is reported instead of guessed at.
            ExitReason::Syscall => Err(VmError::Syscall {
                number: ctx.rax,
                pc: ctx.rip,
            }),

            _ => Err(VmError::Trap {
                reason: exit,
                code: ctx.exit_code,
                pc: ctx.rip,
            }),
        }
    }

    /// Call the host function an intercepted import resolved to.
    ///
    /// `jumped` says how control got here: a tail `jmp` to an import, which the
    /// loop above recognises, or an intercepted `call`, which microlift exits
    /// before the return address is pushed. Only a shim that stands in for the
    /// transfer itself needs to know — see [`crate::objc::dispatch`] — and it is
    /// passed rather than inferred because inferring it from `rip` would be a
    /// coincidence that holds today.
    fn service_import(&self, ctx: &mut RegisterContext, jumped: bool) -> Result<(), VmError> {
        let va = ctx.imported_function_va;
        let Some(site) = self.program.import_at(va) else {
            return Err(AbiError::NoSignature {
                symbol: format!("<unregistered {va:#x}>"),
            }
            .into());
        };
        self.service_site_as(site, ctx, jumped)
    }

    /// [`Vm::service_import`] for a site already looked up.
    fn service_site_as(
        &self,
        site: &crate::program::ImportSite,
        ctx: &mut RegisterContext,
        jumped: bool,
    ) -> Result<(), VmError> {
        // Both flags are set here rather than read here, so each always
        // describes *this* call. Only the tail-call path consults `redirected`;
        // at an ordinary `ImportedFunctionCall` exit a redirected `rip` is
        // already what the loop continues at, and a leftover flag would make the
        // next tail call skip a return it really does have.
        if let Some(state) = crate::thread::current() {
            state.took_redirect();
            state.set_entered_by_jump(jumped);
        }
        self.call_site(site, ctx)
    }

    /// Marshal and perform the call `site` describes.
    fn call_site(
        &self,
        site: &crate::program::ImportSite,
        ctx: &mut RegisterContext,
    ) -> Result<(), VmError> {
        let mut mem = self.view();
        crate::abi::call_import_with(&site.symbol, site.tag, site.resolved, ctx, &mut mem, self)?;
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
            return self.service_import(ctx, false);
        }

        // One of our own thunks, handed out earlier and now called by the guest
        // through a variable. Calling it would be correct and absurd — out to
        // arm64 and straight back in — so the wrapper is unwrapped.
        let target = self
            .thunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .guest_for(callee)
            .unwrap_or(callee);

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

        // A host address the guest *loaded from an import slot* and then called
        // through a register, rather than at a `call` site the lifter could
        // match. `_Thread_local` access is the case that forces this: reading
        // one compiles to `leaq _x(%rip), %rdi; callq *(%rdi)`, so the callee is
        // the resolved `__tlv_bootstrap` address and there is no displacement to
        // register. An ordinary `int (*f)(const char*) = strlen; f(s)` arrives
        // the same way.
        //
        // Looked up rather than guessed at, exactly as everywhere else: the
        // answer is the signature the import already has, and a host address
        // nothing imported is still `UnknownIndirectCall`.
        if let Some(site) = self.program.import_by_target(callee) {
            return self.call_site(site, ctx);
        }

        Err(VmError::UnknownIndirectCall { callee, resume })
    }

    /// A native arm64 address that re-enters the guest at `guest_va`.
    ///
    /// See [`crate::thunk`]. Created once per distinct guest pointer and kept
    /// for the VM's lifetime, so the host may compare two thunks for equality
    /// and get the answer the guest would have.
    pub fn thunk_for(&self, guest_va: u64) -> Result<u64, VmError> {
        if let Some(existing) = self
            .thunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .host_for(guest_va)
        {
            return Ok(existing);
        }
        let thunk = crate::thunk::build(guest_va)?;
        let host = self
            .thunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thunk);

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
        let _ = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_lift(&self.program, guest_va);
        Ok(host)
    }

    /// How many guest function pointers have been handed to host code.
    pub fn thunks(&self) -> usize {
        self.thunks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether `va` is a thunk this VM handed out.
    pub fn guest_behind_thunk(&self, va: u64) -> Option<u64> {
        self.thunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .guest_for(va)
    }

    /// Record an error raised inside a host→guest callback, on the thread it
    /// happened on.
    ///
    /// **And say it out loud.** Parking alone was enough while the host frame in
    /// the middle was `qsort`'s: it returns, control comes back to the runtime,
    /// and [`Vm::take_pending`] raises the error where it can be reported. A
    /// callback the *application frameworks* invoke has no such guarantee. A
    /// guest `initWithFrame:` that fails returns nil to AppKit, AppKit raises an
    /// `NSInvalidArgumentException` about a nil subview, and `libc++abi` ends
    /// the process — with the real diagnostic still sitting in this slot,
    /// unread. The parked copy is still what a caller acts on; this line is so
    /// that the last failure before an abort is not silent.
    pub(crate) fn set_pending(&self, err: VmError) {
        let Some(state) = crate::thread::current() else {
            return;
        };
        let mut slot = state.pending.borrow_mut();
        if slot.is_none() {
            eprintln!("rosetta3: {err}");
            *slot = Some(err);
        }
    }

    /// Take whatever error a callback left behind on this thread.
    pub fn take_pending(&self) -> Option<VmError> {
        crate::thread::current().and_then(|s| s.pending.borrow_mut().take())
    }
}

/// The highest basic leaf this runtime answers. Leaves above it read as zero.
///
/// Real hardware returns the *highest supported* leaf's values for an
/// out-of-range basic leaf, which is a rule written for a CPU whose leaves are
/// all real. Here it would hand leaf 7's feature words to a guest that asked
/// leaf 0x16, which is worse than nothing; zero is the answer the well-behaved
/// caller — the one that checked leaf 0 first — is prepared for either way.
const MAX_BASIC_LEAF: u32 = 1;

/// The highest extended leaf answered: the last quarter of the brand string.
const MAX_EXTENDED_LEAF: u32 = 0x8000_0004;

/// The processor brand string, `NUL`-padded to the 48 bytes leaves
/// `0x80000002..=0x80000004` spell out.
const BRAND: [u8; 48] = brand();

const fn brand() -> [u8; 48] {
    let name = b"rosetta3 (microlift x86-64 on arm64)";
    let mut out = [0u8; 48];
    let mut i = 0;
    while i < name.len() {
        out[i] = name[i];
        i += 1;
    }
    out
}

/// The `CPUID` reply for `leaf`, as `[eax, ebx, ecx, edx]`.
///
/// ## What is advertised, and why so little
///
/// A feature bit here is a promise that the *lifter* can execute the
/// instructions it stands for. Microlift implements the x86-64 baseline —
/// x87, `CMOV`, `CMPXCHG8B`, SSE and SSE2 — and, of the later extensions,
/// scattered instructions rather than whole sets. So the reply is the baseline
/// and nothing else: no SSE3, no SSSE3, no SSE4, no `POPCNT`, no
/// `CMPXCHG16B`, no AVX.
///
/// Advertising more is not optimism, it is a crash with a worse diagnostic. A
/// guest that sees the AVX bit dispatches to its AVX `memcpy`, and the first
/// `vmovdqu` in it stops the VM at [`VmError::Trap`] — an unimplemented
/// instruction in code the guest chose to run *because this function said it
/// could*. Under-reporting costs a slower path that works.
///
/// `OSXSAVE` being clear is what keeps [`ExitReason::Xgetbv`] out of reach: a
/// guest reads `XCR0` only after this function tells it the OS enabled
/// extended state, and no set of features here does.
///
/// The vendor stays `GenuineIntel` because a guest built for x86-64 macOS has
/// only ever run on Intel, and any dispatch it does on vendor has exactly one
/// tested arm. The brand string is where the honesty goes: it says what this
/// actually is, and nothing dispatches on it.
///
/// No leaf this runtime answers is sub-leaf dependent, so the sub-leaf in `rcx`
/// is accepted and ignored.
fn cpuid(leaf: u32, _subleaf: u32) -> [u32; 4] {
    /// A four-byte slice of the brand string, as the little-endian word the
    /// register holds.
    fn brand_word(offset: usize) -> u32 {
        u32::from_le_bytes([
            BRAND[offset],
            BRAND[offset + 1],
            BRAND[offset + 2],
            BRAND[offset + 3],
        ])
    }

    match leaf {
        // Maximum basic leaf, and the vendor string in the odd `ebx`, `edx`,
        // `ecx` order the architecture specifies.
        0x0 => [
            MAX_BASIC_LEAF,
            u32::from_le_bytes(*b"Genu"),
            u32::from_le_bytes(*b"ntel"),
            u32::from_le_bytes(*b"ineI"),
        ],

        // Family/model/stepping and the feature words.
        0x1 => {
            // Family 6, model 15, stepping 1: a plausible 64-bit Intel part,
            // recent enough that nothing consults an errata list for it and
            // old enough to claim nothing this reply does not back up.
            let version = 0x0000_06f1;
            // Cache line size, in eight-byte units, at bits 15:8. The rest of
            // this word — brand index, logical processor count, APIC id — is
            // meaningful only alongside feature bits that are clear here.
            let misc = 8 << 8;
            // FPU | TSC | CX8 | CMOV | SSE | SSE2.
            let edx = (1 << 0) | (1 << 4) | (1 << 8) | (1 << 15) | (1 << 25) | (1 << 26);
            // Nothing in the `ecx` word is implemented, but bit 31 is not a
            // feature: it is the hypervisor-present bit, and saying so is both
            // true and useful — it is the one bit that tells a guest doing its
            // own timing or cache measurement that the numbers are not real.
            let ecx = 1 << 31;
            [version, misc, ecx, edx]
        }

        // Maximum extended leaf. Nothing else is reported here: the feature
        // words of `0x80000001` would only repeat the baseline.
        0x8000_0000 => [MAX_EXTENDED_LEAF, 0, 0, 0],

        // Long mode. The guest is 64-bit code that is already running, so this
        // bit describes the present rather than promising anything.
        0x8000_0001 => [0, 0, 0, 1 << 29],

        0x8000_0002..=0x8000_0004 => {
            let base = (leaf - 0x8000_0002) as usize * 16;
            [
                brand_word(base),
                brand_word(base + 4),
                brand_word(base + 8),
                brand_word(base + 12),
            ]
        }

        _ => [0, 0, 0, 0],
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
        self.thunk_for(va).map_err(|e: VmError| AbiError::Refused {
            symbol: symbol.to_string(),
            reason: match e {
                VmError::Thunk { .. } => "a host->guest thunk could not be built",
                _ => "the guest function pointer could not be made callable",
            },
        })
    }

    /// A thunk handed back by host code becomes the guest pointer it stands for.
    ///
    /// `signal` is what needs this: it *returns* the previous disposition, so a
    /// program that saves one and restores it later would otherwise install an
    /// arm64 trampoline as its own handler — and, worse, compare it against its
    /// own function and find them unequal.
    fn unsubstitute(&self, va: u64) -> u64 {
        self.guest_behind_thunk(va).unwrap_or(va)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest reads the vendor before it reads anything else, and it reads it
    /// out of three registers in an order that is easy to transpose.
    #[test]
    fn leaf_zero_spells_the_vendor() {
        let [eax, ebx, ecx, edx] = cpuid(0, 0);
        let mut vendor = Vec::new();
        vendor.extend_from_slice(&ebx.to_le_bytes());
        vendor.extend_from_slice(&edx.to_le_bytes());
        vendor.extend_from_slice(&ecx.to_le_bytes());
        assert_eq!(&vendor, b"GenuineIntel");
        assert_eq!(eax, MAX_BASIC_LEAF);
    }

    /// The whole point of the reply: SSE2 yes, everything microlift cannot
    /// lift no. A feature bit that turns on here without the semantics behind
    /// it is a guest crash in code this function invited it to run.
    #[test]
    fn leaf_one_advertises_the_baseline_and_no_more() {
        let [_, _, ecx, edx] = cpuid(1, 0);
        for (bit, name) in [
            (0, "fpu"),
            (4, "tsc"),
            (8, "cx8"),
            (25, "sse"),
            (26, "sse2"),
        ] {
            assert_ne!(edx & (1 << bit), 0, "{name} should be advertised");
        }
        // MMX and FXSR: baseline on real hardware, unimplemented here.
        for (bit, name) in [(23, "mmx"), (24, "fxsr")] {
            assert_eq!(edx & (1 << bit), 0, "{name} should not be advertised");
        }
        for (bit, name) in [
            (0, "sse3"),
            (9, "ssse3"),
            (13, "cmpxchg16b"),
            (19, "sse4.1"),
            (20, "sse4.2"),
            (23, "popcnt"),
            (26, "xsave"),
            (27, "osxsave"),
            (28, "avx"),
        ] {
            assert_eq!(ecx & (1 << bit), 0, "{name} should not be advertised");
        }
        // Emulated, and saying so.
        assert_ne!(ecx & (1 << 31), 0, "hypervisor bit should be set");
    }

    /// The brand string is one 48-byte block split across twelve registers,
    /// and a misordered word would still look like a plausible string.
    #[test]
    fn brand_string_reassembles() {
        let mut bytes = Vec::new();
        for leaf in 0x8000_0002..=0x8000_0004 {
            for word in cpuid(leaf, 0) {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
        }
        assert_eq!(bytes.len(), 48);
        let text = String::from_utf8(bytes).expect("brand is ascii");
        assert!(
            text.starts_with("rosetta3 (microlift x86-64 on arm64)"),
            "{text}"
        );
        assert!(text.ends_with('\0'), "brand string is NUL-terminated");
        assert_eq!(cpuid(0x8000_0000, 0)[0], MAX_EXTENDED_LEAF);
    }

    /// Long mode, since the code asking is 64-bit and already running.
    #[test]
    fn extended_leaf_one_reports_long_mode() {
        assert_ne!(cpuid(0x8000_0001, 0)[3] & (1 << 29), 0);
    }

    /// Out of range in either space reads as zero rather than as some other
    /// leaf's feature words, and the sub-leaf changes nothing.
    #[test]
    fn unknown_leaves_are_zero() {
        assert_eq!(cpuid(MAX_BASIC_LEAF + 1, 0), [0, 0, 0, 0]);
        assert_eq!(cpuid(7, 0), [0, 0, 0, 0]);
        assert_eq!(cpuid(0x4000_0000, 0), [0, 0, 0, 0]);
        assert_eq!(cpuid(MAX_EXTENDED_LEAF + 1, 0), [0, 0, 0, 0]);
        assert_eq!(cpuid(1, 0), cpuid(1, 7));
    }
}
