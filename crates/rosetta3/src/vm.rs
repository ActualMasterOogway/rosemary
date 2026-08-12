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

use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
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
        /// `start` as `image+unslid-va`, the form a disassembler can be pointed
        /// at.
        ///
        /// The same thing [`Transfer::at`] carries and for the same reason: the
        /// mapping moves every run, so the slid address in this message is a
        /// number no two runs agree on and no `objdump` invocation accepts. A
        /// refused instruction is nearly always answered by *looking at the
        /// bytes*, and without this that takes a second run with
        /// `ROSETTA3_TRACE_VM` to find out where they are.
        at: Option<String>,
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

/// How many times the runtime will say the same thing before going quiet.
///
/// A parked failure is *taken* by the run loop and re-raised, so a host that
/// keeps calling a failing callback — a run loop, a retry, a framework that
/// treats nil as "try again" — produces one line per attempt. That is a
/// diagnostic while it is a handful and a denial of service when it is not.
const SAY_LIMIT: usize = 16;

/// Report a failure once, and stop repeating yourself.
///
/// Written after a five-second run produced a **6.7 GB** log. Two things
/// compounded, and the multiplication is the reason the number is that large:
/// the message was reprinted for every attempt, and every attempt also made it
/// *longer*, because a failure crossing a callback boundary was wrapped in
/// another [`VmError::Callback`] that prefixed the previous rendering. Repeated
/// *n* times that is O(n²) bytes. The wrapping is fixed at its source — see
/// [`crate::thunk`] — and this is the other half.
///
/// Identical consecutive messages are dropped rather than counted, because the
/// interesting event is the *first* one and everything after it is the host
/// retrying.
fn say(err: &VmError) {
    use std::sync::Mutex;
    static SAID: Mutex<(usize, String)> = Mutex::new((0, String::new()));

    let text = err.to_string();
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    let (count, last) = &mut *said;
    if *count > SAY_LIMIT || *last == text {
        return;
    }
    *count += 1;
    last.clear();
    last.push_str(&text);
    match *count > SAY_LIMIT {
        // The cap is announced rather than silently applied: a run that simply
        // stopped reporting would look like a run that stopped failing.
        true => eprintln!(
            "rosetta3: {SAY_LIMIT} failures reported; further ones are parked and not printed"
        ),
        false => eprintln!("rosetta3: {text}"),
    }
}

/// How often [`Vm::run_loop`] should report the guest's position, if at all.
///
/// Read once and cached, because the alternative is a `getenv` per region entry.
fn trace_interval() -> Option<u64> {
    static EVERY: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("ROSETTA3_TRACE_PC")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
    })
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
            VmError::Lift {
                pc,
                start,
                source,
                at,
            } => {
                write!(f, "lifting the region at {start:#x}")?;
                if let Some(at) = at {
                    write!(f, " ({at})")?;
                }
                write!(f, " (for pc {pc:#x}): {source}")
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
                write!(f, "guest stopped at {pc:#x}: {reason} (code {code:#x}")?;
                // For an unimplemented instruction the code *is* the decoder's
                // identity for it, and a number alone leaves the reader with no
                // way to find out which instruction the recompiler is missing —
                // which is the only thing this diagnostic is for. Naming it
                // costs one table lookup in a crate already linked in.
                if let Some(name) = instruction_name(*reason, *code) {
                    write!(f, ", {name}")?;
                }
                write!(f, ")")
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

/// The decoder's name for the instruction an `UnimplementedInstruction` exit
/// carries, or `None` for any other exit.
///
/// The lifter stores `iced_x86::Instruction::code()` in `exit_code`, so this is
/// a total function of that number and needs no access to guest memory — which
/// matters, because it is called from `Display` long after the guest is gone.
fn instruction_name(reason: ExitReason, code: u32) -> Option<String> {
    if reason != ExitReason::UnimplementedInstruction {
        return None;
    }
    let code = iced_x86::Code::try_from(code as usize).ok()?;
    // `Debug` is the enum variant, which is the mnemonic *plus* its operand
    // shape — `Vsqrtsd_xmm_xmm_xmmm64` rather than just `vsqrtsd`. That is more
    // useful here, not less: it says exactly which encoding to implement.
    Some(format!("{code:?}"))
}

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
/// ## Why the page cache is not a set behind a lock
///
/// It was, and `read_cstr`'s byte-at-a-time walk is what made that untenable:
/// a `char *` on a spawned thread's stack is outside the region list, so every
/// single byte of it took a process-wide mutex to ask whether its page had been
/// probed before. What replaced it is a fixed direct-mapped table of atomics —
/// one relaxed load to answer, one relaxed store to record — and the race it
/// permits is the only one worth having: two threads may probe the same page
/// twice, and `msync` is idempotent.
///
/// A colliding page evicts the previous occupant and is re-probed next time, so
/// the table is a cache in the strict sense: correct when it hits, correct when
/// it misses, and never consulted for anything but speed.
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
    mapped: Arc<PageCache>,
}

/// Pages [`GuestView`] has already probed, direct-mapped and lock-free.
///
/// A zero slot is empty: page zero is never mapped, so it doubles as the
/// vacancy marker without a second word.
pub struct PageCache {
    slots: Box<[std::sync::atomic::AtomicU64]>,
}

/// How many probed pages are remembered. A power of two, so the index is a mask.
const PAGES: usize = 4096;

impl PageCache {
    fn new() -> Self {
        PageCache {
            slots: (0..PAGES)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
        }
    }

    /// The slot a page-aligned address maps to. Shifted past the page offset
    /// first, which is all zeros and would leave most of the table empty.
    fn slot(page: u64) -> usize {
        (page >> 12) as usize & (PAGES - 1)
    }

    /// Whether `page` is known to be mapped.
    fn known(&self, page: u64) -> bool {
        self.slots[Self::slot(page)].load(Ordering::Relaxed) == page
    }

    /// Record that it is.
    fn remember(&self, page: u64) {
        self.slots[Self::slot(page)].store(page, Ordering::Relaxed);
    }
}

impl GuestView {
    /// A view over `regions`, which must be identity-mapped host memory.
    pub fn new(regions: Vec<Range<u64>>) -> Self {
        GuestView {
            regions: Arc::new(regions),
            mapped: Arc::new(PageCache::new()),
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
            if !self.mapped.known(at) {
                if !page_is_mapped(at, page) {
                    return false;
                }
                self.mapped.remember(at);
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
    /// The published, immutable half of `thunks` — see [`crate::thunk::ThunkView`].
    ///
    /// A leaked snapshot behind an `AtomicPtr`, swapped whole, exactly as
    /// `objc` below is and for the same reason: the question it answers is
    /// asked at every indirect call the guest makes, and asking it behind the
    /// mutex made every guest thread queue for the answer "no".
    thunk_view: AtomicPtr<crate::thunk::ThunkView>,
    /// How many thunks exist, readable without the lock.
    ///
    /// Stored *after* `thunk_view`, under the same lock, so a reader that finds
    /// the two equal knows its miss covers everything published. See
    /// [`crate::thunk::ThunkView`] on why a stale miss is not acceptable here
    /// when a stale translation is.
    thunk_count: AtomicUsize,
    view: GuestView,
    /// The guest's Objective-C classes, once realized in the host runtime.
    ///
    /// A leaked snapshot behind an `AtomicPtr`, swapped whole, for the reason
    /// [`crate::dyld`]'s image list is: registration is no longer one event.
    /// It happens at launch over the program's own images and again inside every
    /// `dlopen` that brings classes with it, while other threads are dispatching
    /// messages through the maps — so the extending pass builds a **copy**,
    /// leaks it, and publishes the pointer. A reader sees the old runtime or the
    /// new one and both stay valid forever, which is what keeps the read path,
    /// the hottest in [`crate::objc::dispatch`], off a lock entirely.
    ///
    /// A `OnceLock` was enough while only [`crate::startup`] registered.
    objc: AtomicPtr<crate::objc::ObjcRuntime>,
    /// Held across a registration pass, so two `dlopen`s cannot each extend the
    /// same snapshot and publish one of the results.
    objc_registration: Mutex<()>,
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
        crate::dyld::set_executable_path(exe_path, program.report.root().map_or(0, |i| i.slide));
        // Where the guest's own `mach_header` is, for the guest's own sake: a
        // program that walks the loaded images must be told about itself, and
        // `_dyld_get_image_header(0)` would describe `rosetta3`. See
        // [`crate::dyld`]'s image notices.
        // `ImageReport::base` is where the image *landed*, slide already
        // applied — adding the slide again is a plausible address in the middle
        // of nothing, which is what it produced.
        if let Some(root) = program.report.root() {
            crate::dyld::set_executable_header(root.base);
        }

        let cache = CodeCache::new(program.lifter_config_with(policy));
        // The calling thread becomes the guest's main thread: this is the stack
        // `argv` was laid out on, so it is the only one `main` can run on.
        crate::thread::install(crate::thread::ThreadState::new(stack));
        Ok(Vm {
            program: SharedProgram(program),
            cache: Mutex::new(cache),
            thunks: Mutex::new(ThunkTable::new()),
            thunk_view: AtomicPtr::new(core::ptr::null_mut()),
            thunk_count: AtomicUsize::new(0),
            view: GuestView::new(ranges),
            objc: AtomicPtr::new(core::ptr::null_mut()),
            objc_registration: Mutex::new(()),
        })
    }

    /// Realize the guest's Objective-C classes in the host runtime.
    ///
    /// Idempotent, and deliberately *not* done in [`Vm::with_policy`]: it makes
    /// guest function pointers callable from the host, which means building
    /// thunks, which means compiling. A `Vm` that is only ever inspected should
    /// not pay for that, and the audit path is exactly such a caller.
    pub fn register_objc(&self) -> Result<&'static crate::objc::ObjcRuntime, VmError> {
        let _guard = self
            .objc_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = self.objc() {
            return Ok(existing);
        }
        Ok(self.publish_objc(crate::objc::register(self)?))
    }

    /// Realize the classes of an image the guest `dlopen`ed, into the same
    /// runtime.
    ///
    /// The extending half of [`Vm::register_objc`], and the reason the runtime
    /// is a published snapshot rather than a `OnceLock`: SDL3's Cocoa backend
    /// declares its own `NSApplication` subclass, and until this pass runs a
    /// message to it is refused by name in [`crate::objc::dispatch`].
    ///
    /// Realizing the classes runs **no guest code**, which is why it is separate
    /// from [`Vm::run_loaded_load_methods`]: a `+load` is guest code in the
    /// image, and it must not run until the add-image notices have gone out. A
    /// callback that registers the image's `__DATA` as garbage-collector roots —
    /// which is what Boehm uses them for, and Mono ships Boehm — has to have
    /// done so before anything in the image can allocate. See
    /// [`crate::dyld::open`] for the order the two halves sit in.
    ///
    /// A `Vm` that has never registered at all is left alone: registering a
    /// loaded image's classes without the program's would put a subclass of a
    /// program class into the "superclass missing" list, and the audit path
    /// deliberately never registers anything.
    pub fn register_loaded_objc(
        &self,
        report: &machish::report::LinkReport,
    ) -> Result<(), VmError> {
        let _guard = self
            .objc_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(current) = self.objc() else {
            return Ok(());
        };
        let mut extended = current.clone();
        crate::objc::register_loaded(self, &mut extended, report)?;
        self.publish_objc(extended);
        Ok(())
    }

    /// Call the `+load` methods of an image [`Vm::register_loaded_objc`] just
    /// realized, returning how many ran.
    ///
    /// Deliberately outside that call's lock. A `+load` is guest code and guest
    /// code may `dlopen` — a plugin host's whole purpose — which arrives back at
    /// the registration lock. Holding it across one is a deadlock waiting for a
    /// program that does the ordinary thing.
    pub fn run_loaded_load_methods(
        &self,
        report: &machish::report::LinkReport,
    ) -> Result<usize, VmError> {
        match self.objc() {
            Some(rt) => crate::objc::call_loaded_load_methods(self, rt, report),
            None => Ok(0),
        }
    }

    /// Leak `rt` and make it the runtime every dispatching thread reads.
    fn publish_objc(&self, rt: crate::objc::ObjcRuntime) -> &'static crate::objc::ObjcRuntime {
        let leaked: &'static crate::objc::ObjcRuntime = Box::leak(Box::new(rt));
        self.objc.store(
            leaked as *const _ as *mut crate::objc::ObjcRuntime,
            Ordering::Release,
        );
        leaked
    }

    /// The registered Objective-C classes, if [`Vm::register_objc`] has run.
    pub fn objc(&self) -> Option<&'static crate::objc::ObjcRuntime> {
        let published = self.objc.load(Ordering::Acquire);
        // SAFETY: non-null only after `publish_objc` stored a leaked runtime,
        // which is never freed and never mutated after publication — an
        // extending pass builds a separate copy. See the field note.
        (!published.is_null()).then(|| unsafe { &*published })
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

    /// A `Protocol *` on its way out to the host runtime.
    ///
    /// [`Vm::objc_host_class`] for protocols: `@protocol(Foo)` is a direct
    /// reference to the guest's own `protocol_t`, which the host runtime has
    /// never seen. See [`crate::objc::ObjcRuntime::host_protocol_or`].
    pub fn objc_host_protocol(&self, va: u64) -> u64 {
        self.objc().map_or(va, |rt| rt.host_protocol_or(va))
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
    pub fn call_with(&self, entry: u64, ctx: RegisterContext) -> Result<u64, VmError> {
        self.call_with_stack(entry, ctx, &[]).map(|(rax, _)| rax)
    }

    /// As [`Vm::call_with`], with arguments the guest's ABI puts on its *stack*.
    ///
    /// Registers alone cannot express every call, and the case that forces this
    /// is not exotic: an `NSRect` is thirty-two bytes, SysV AMD64 classifies it
    /// MEMORY, and `-[NSView initWithFrame:]` therefore reads it from
    /// `0x10(%rbp)`. AAPCS64 hands the same rectangle to a callback in `v0`–`v3`,
    /// so nothing about the host's call says where the guest expects to find it.
    /// See [`crate::thunk`], which is the only caller that has the types to know.
    ///
    /// `stack` is placed exactly where a real `CALL` would have left it: at
    /// `rsp+8` on entry, immediately above the return address. The alignment
    /// matters and is easy to get subtly wrong — SysV requires `rsp % 16 == 0`
    /// *at the call site*, so a callee sees `rsp % 16 == 8` and its first stack
    /// argument sixteen-byte aligned, which is what `movaps 0x10(%rbp)` depends
    /// on not faulting.
    ///
    /// Returns `(rax, xmm0)`: an integer result and a floating-point one come
    /// back in different registers and the caller's type decides which is the
    /// answer.
    pub fn call_with_stack(
        &self,
        entry: u64,
        mut ctx: RegisterContext,
        stack: &[u8],
    ) -> Result<(u64, u64), VmError> {
        // A thread the guest spawned reaches here first, through a thunk, with
        // no stack of its own yet.
        let state = crate::thread::ensure()?;
        if state.depth.get() >= MAX_DEPTH {
            return Err(VmError::TooDeep { pc: entry });
        }

        // Below the innermost live frame, past the red zone the guest's ABI
        // lets a leaf function use without adjusting `rsp`.
        let top = (state.rsp.get() - 128) & !0xf;
        // Padded to a multiple of sixteen so the return address below it leaves
        // the callee at the alignment the ABI promises.
        let padded = (stack.len() as u64).next_multiple_of(16);
        let args_at = top - padded;
        if !stack.is_empty() {
            self.view()
                .write(args_at, stack)
                .map_err(|source| VmError::Stack { source })?;
        }
        let rsp = args_at - 8;
        self.view()
            .write(rsp, &RETURN_SENTINEL.to_le_bytes())
            .map_err(|source| VmError::Stack { source })?;
        ctx.rsp = rsp;
        ctx.rip = entry;
        // The guest reads thread-specific data off `%gs` without calling
        // anything, so the base has to be right before the first region runs and
        // it is a fact about *this* thread — which is why it is set here, in the
        // one place every entry into guest code passes through, rather than
        // wherever a context happens to be built. See [`crate::thread`].
        ctx.gs_base = crate::thread::tsd_base();

        let saved_rsp = state.rsp.get();
        state.rsp.set(rsp);
        state.depth.set(state.depth.get() + 1);
        // This entry gets an identity of its own, which `setjmp` records and
        // `longjmp` checks: a jump back into an *outer* entry would leave the
        // host frames between the two — the host's `qsort`, with a guest
        // comparator above it — on the host's stack with nothing to return
        // through them. See [`crate::jmpbuf`].
        let outer_entry = state.enter();
        // Publish the register context for the duration, for the same shape of
        // reason and a different reader: a collector suspends this thread and
        // asks the kernel for its registers, and only this runtime knows where
        // the guest's are. See [`crate::thread::GuestThread`].
        let outer_context = state.publish_context(&mut ctx);
        // Publish the VM for the duration: a thunk called from inside a host
        // function has no other way to find it. See `crate::thunk`.
        let result = crate::thunk::with_current(self, || self.run_loop(state, &mut ctx));
        state.restore_context(outer_context);
        state.leave(outer_entry);
        state.depth.set(state.depth.get() - 1);
        state.rsp.set(saved_rsp);
        // `xmm0` is read from the context the loop just finished with, which is
        // where a `double`-returning guest function left its answer.
        result.map(|rax| (rax, ctx.xmm[0][0]))
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
        let mut ticks: u64 = 0;
        loop {
            let pc = ctx.rip;

            // `ROSETTA3_TRACE_PC=n` reports where the guest is every `n` region
            // entries, as `image+offset`. Off unless set, and one compare when
            // it is.
            //
            // It exists because "100% CPU and no frames" is not a failure this
            // runtime can name: nothing refuses, nothing faults, the loop simply
            // never comes back. A profile of the *host* shows the runtime's own
            // overhead and says nothing about which guest code is repeating.
            // This is the smallest thing that answers that.
            if let Some(every) = trace_interval() {
                ticks += 1;
                if ticks.is_multiple_of(every) {
                    let at = self
                        .program
                        .locate(pc)
                        .unwrap_or_else(|| format!("{pc:#x}"));
                    // The two lookups are reported because a *livelock* is what
                    // this exists to find, and a livelock here is always one of
                    // them matching an address it should not. An unchanging
                    // `rsp` beside an unchanging `pc` is the signature.
                    let by_site = self.program.import_at(pc).map(|s| s.symbol.as_str());
                    let by_target = self.program.import_by_target(pc).map(|s| s.symbol.as_str());
                    // The previous exit is on the line because a livelock is
                    // always a region that leaves the way it came, and which
                    // exit it leaves by is the difference between "the guest is
                    // spinning" and "this runtime is".
                    eprintln!(
                        "rosetta3: trace {ticks}: {at} rsp={:#x} import_at={by_site:?} \
                         by_target={by_target:?} from={:?}",
                        ctx.rsp,
                        from.as_ref().map(|f| (f.exit, f.code, f.pc)),
                    );
                }
            }

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

            // This thread's own answer first, which is the whole hot path: a
            // PC it has already run needs no lock, no map walk and no
            // comparison chain. See [`crate::thread::CodeWindow`] for why a
            // remembered translation cannot go stale.
            let translation = match state.code().get(pc) {
                Some(hit) => hit,
                None => {
                    // The lock is taken to look up or lift and released before
                    // guest code runs, which is what keeps a callback
                    // re-entering the VM on this same thread from deadlocking
                    // against its own caller.
                    let lifted = self
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
                    state.code().put(pc, &lifted);
                    lifted
                }
            };
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

            // Control fell out of the *bottom* of the region: the last
            // instruction lifted was an ordinary fallthrough and the next
            // address is past the bytes microlift was given. It sets `rip` to
            // that address and exits with no reason, which is the whole of the
            // protocol — there is nothing to service, and continuing at `rip`
            // lifts the next region from it.
            //
            // Every block that does *not* end at a `ret`, an indirect jump or
            // an undecodable byte ends this way, which until now meant every
            // truncated region was a stop reading `guest stopped at …: none`.
            // Two things produce one: a region cut at the instruction cap, and
            // a block that ran into a hole — a jump table, alignment padding,
            // the inline argument after a call. See [`crate::region`].
            ExitReason::None => Ok(None),

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
                // Two of the kinds are not about the guest's control flow at
                // all — they are the translation saying it cannot serve this
                // address, either because the instruction there runs past the
                // bytes it was given or because its dispatch was never offered
                // that address. `rip` already names it, so continuing lifts a
                // region that starts there. Counted because a run full of them
                // means the region policy is cutting in the wrong places.
                Ok(TransferKind::RegionTail | TransferKind::NoResumePoint) => {
                    crate::stats::record_tail_exit();
                    Ok(None)
                }
                // A tail `jmp` through a register or a memory operand, whose
                // target may be **one of this runtime's own thunks**.
                //
                // [`Vm::dispatch_indirect`] has unwrapped that for an indirect
                // *call* since M6: the guest stored a function pointer that
                // crossed to the host, got a native trampoline back, and calling
                // it would go out to arm64 and straight back in. The same
                // pointer reached by a tail jump was not unwrapped, and the
                // difference was invisible until a guest tail-jumped through
                // one.
                //
                // Roblox does, and the shape is worth recording because nothing
                // about it looks like a callback. A block literal's `invoke` is
                // replaced in place when the block crosses to a host API — see
                // [`crate::shims::block`] — and the block stays the guest's
                // afterwards. `std::function` wrapping that block compiles its
                // call to `mov rdi, [rdi]; jmp qword [rdi+0x10]`, a tail jump
                // straight through the field this runtime rewrote. The guest
                // lands on a thunk, the run loop tries to lift host memory, and
                // the report is `no guest code at <a page-aligned address>` with
                // nothing naming a block.
                //
                // Nothing is pushed, and that is the difference from the call
                // form rather than an omission: a tail jump's return address is
                // already on the stack, put there by whoever called the
                // function being tail-jumped out of.
                Ok(TransferKind::IndirectJump) => {
                    if !self.program.is_guest_address(ctx.rip) {
                        if let Some(guest) = self.guest_behind_thunk(ctx.rip) {
                            ctx.rip = guest;
                        }
                    }
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

            // The answer is discarded here and load-bearing in
            // [`Vm::dispatch_indirect_bridged`]: at an exit the loop continues
            // at `rip` whether the call was made or the target is guest code,
            // and only a bridge has to tell those apart.
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

            // The one unimplemented instruction this runtime implements. It is
            // not lowered because no Cranelift backend but x64 can lower a
            // 128-bit atomic, and it is not *emulated* because a guest that
            // issues it is a guest with threads. See [`crate::atomic128`].
            ExitReason::UnimplementedInstruction
                if iced_x86::Code::try_from(ctx.exit_code as usize)
                    .map(|c| c.mnemonic() == iced_x86::Mnemonic::Cmpxchg16b)
                    .unwrap_or(false) =>
            {
                let mem = self.view();
                crate::atomic128::service(ctx, &mem)?;
                Ok(None)
            }

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

    /// Service an import from a fast-call bridge rather than from an exit.
    ///
    /// The same work [`Vm::service_import`] does, with one thing added to the
    /// answer: whether control was *redirected*. At an exit that question does
    /// not arise, because the loop continues at `rip` either way. Inside a
    /// bridge it decides everything — a serviced call returns to the instruction
    /// after it, while a `longjmp` or a transfer into another guest image has
    /// moved `rip` somewhere the lifted code cannot continue from. See
    /// [`crate::bridge`].
    ///
    /// `jumped` is false even for an import reached by a tail `jmp`: microlift
    /// pops the return address before calling the bridge and publishes it as
    /// `rip`, so a shim standing in for the transfer finds exactly what it finds
    /// at an intercepted `call` — the address the callee must return to, and a
    /// stack with no return address on it.
    pub(crate) fn service_bridged_import(
        &self,
        import_va: u64,
        ctx: &mut RegisterContext,
    ) -> Result<bool, VmError> {
        let Some(site) = self.program.import_at(import_va) else {
            return Err(AbiError::NoSignature {
                symbol: format!("<unregistered {import_va:#x}>"),
            }
            .into());
        };
        self.service_site_as(site, ctx, false)?;
        Ok(self.redirected())
    }

    /// [`Vm::dispatch_indirect`] from a fast-call bridge.
    ///
    /// `Ok(false)` means the call was made and the guest resumes inline;
    /// `Ok(true)` means `rip` now names guest code, which can only be entered
    /// through the JIT, so the run loop has to have it.
    pub(crate) fn dispatch_indirect_bridged(
        &self,
        target: u64,
        ctx: &mut RegisterContext,
    ) -> Result<bool, VmError> {
        // `dispatch_indirect` reads the callee from the context, and microlift
        // has already parked it there. Set explicitly rather than relied upon:
        // the two are the same value by contract, and this is the one line that
        // says so.
        ctx.imported_function_va = target;
        self.dispatch_indirect(ctx)
    }

    /// Whether the call just serviced moved `rip` instead of returning a value,
    /// clearing the flag.
    fn redirected(&self) -> bool {
        crate::thread::current().is_some_and(|s| s.took_redirect())
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
        // An import that resolved into another **guest** image, which happens as
        // soon as one exists: a `dlopen`ed plugin binds against the executable
        // that loaded it, and DELTARUNE's `libYoYoGamepad.dylib` imports ninety
        // of `GMGamePad`'s methods straight out of `Mac_Runner`.
        //
        // Every other import resolves to a host function and is marshalled.
        // Marshalling this one would call x86-64 as arm64 — or, because it has
        // no table entry, refuse with "no signature", which is a confusing thing
        // to say about a C++ method the guest defines itself. It is not a call
        // *out* at all; it is a call from one part of the guest to another, and
        // the registers are already the callee's.
        if site.resolved != 0 && self.program.is_guest_address(site.resolved) {
            return self.transfer_to_guest(site.resolved, ctx);
        }
        let mut mem = self.view();
        crate::abi::call_import_with(&site.symbol, site.tag, site.resolved, ctx, &mut mem, self)?;
        Ok(())
    }

    /// Continue the guest at `target`, as the intercepted transfer would have.
    ///
    /// The same shape as [`crate::objc::dispatch`]'s, and for the same reason:
    /// where the return address comes from depends on how control reached the
    /// interception. An intercepted `call` has not pushed one — microlift exits
    /// first — so it is this runtime's to write, and `rip` is already the
    /// instruction after the call, which is exactly where the callee must
    /// return. A tail `jmp` has the caller's return address on the stack
    /// already and must not gain a second.
    fn transfer_to_guest(&self, target: u64, ctx: &mut RegisterContext) -> Result<(), VmError> {
        let Some(state) = crate::thread::current() else {
            return Err(AbiError::Refused {
                symbol: format!("<guest import at {target:#x}>"),
                reason: "a guest-to-guest import outside a running guest thread",
            }
            .into());
        };
        if !state.entered_by_jump() {
            let rsp = ctx.rsp.wrapping_sub(8);
            self.view()
                .write(rsp, &ctx.rip.to_le_bytes())
                .map_err(|source| VmError::Stack { source })?;
            ctx.rsp = rsp;
        }
        ctx.rip = target;
        // The stack pointer moved without an exit reporting it, and `rip` is now
        // a destination rather than a resume point: whoever serviced this call
        // must not synthesize a return over it.
        state.rsp.set(ctx.rsp);
        state.redirect();
        Ok(())
    }

    /// Decide what an indirect call through a runtime-computed address means.
    ///
    /// The question the identity mapping forces on every one of them: a guest
    /// address must be entered through the JIT, a host address must be called
    /// natively, and there is nothing in the pointer to say which. Getting it
    /// backwards in either direction is a crash with no useful diagnostic, so
    /// the answer is looked up rather than inferred from the value.
    ///
    /// `Ok(true)` means control was transferred rather than a call being made:
    /// `rip` names guest code, or a shim redirected. Only the bridge form reads
    /// it — see [`Vm::dispatch_indirect_bridged`].
    fn dispatch_indirect(&self, ctx: &mut RegisterContext) -> Result<bool, VmError> {
        let callee = ctx.imported_function_va;
        let resume = ctx.rip;

        // A pointer read out of an import slot: the same host function the
        // import would have reached, so service it identically.
        if self.program.import_at(callee).is_some() {
            self.service_import(ctx, false)?;
            return Ok(self.redirected());
        }

        // One of our own thunks, handed out earlier and now called by the guest
        // through a variable. Calling it would be correct and absurd — out to
        // arm64 and straight back in — so the wrapper is unwrapped.
        let target = self.guest_behind_thunk(callee).unwrap_or(callee);

        if self.program.is_guest_address(target) {
            // Stand in for the `CALL` microlift declined to perform.
            let rsp = ctx.rsp - 8;
            self.view()
                .write(rsp, &resume.to_le_bytes())
                .map_err(|source| VmError::Stack { source })?;
            ctx.rsp = rsp;
            ctx.rip = target;
            return Ok(true);
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
            // `service_site_as`, not `call_site`: the flags it sets describe
            // *this* call, and a shim that stands in for the transfer itself
            // reads them. `objc_msgSend` reached as `callq *%r12` — which is how
            // clang compiles a send when it has already loaded the slot — is
            // exactly that case, and going straight to `call_site` left
            // `entered_by_jump` holding whatever the last *tail* call set.
            //
            // The consequence is not subtle once seen. `dispatch::transfer`
            // skips pushing the return address when it believes it was entered
            // by a jump, so a stale `true` sends the guest method off with `rsp`
            // pointing at the caller's **stack arguments**: it reads the wrong
            // one, and its `ret` pops an argument as a return address.
            // `+[YYOpenGLUtility loadTextureData:width:height:bpp:into:]` is
            // seven arguments wide, which is what made it visible.
            //
            // `false` because an indirect `call` is a call: microlift intercepts
            // it before the push, so the return address is this runtime's to
            // write.
            self.service_site_as(site, ctx, false)?;
            return Ok(self.redirected());
        }

        Err(VmError::UnknownIndirectCall { callee, resume })
    }

    /// A native arm64 address that re-enters the guest at `guest_va`.
    ///
    /// See [`crate::thunk`]. Created once per distinct guest pointer and kept
    /// for the VM's lifetime, so the host may compare two thunks for equality
    /// and get the answer the guest would have.
    /// [`Vm::thunk_for`], for a callback whose types are known.
    ///
    /// The generic trampoline forwards six integer and eight floating-point
    /// registers because a bare function pointer says nothing about its own
    /// shape. An Objective-C method is the exception — its encoding is in the
    /// class metadata — and this is what that knowledge buys: an aggregate
    /// argument placed where the guest's ABI puts it, which for an `NSRect` is
    /// the guest's stack. See [`crate::thunk::build_typed`].
    ///
    /// Identity is preserved the same way: one thunk per guest address, so a
    /// pointer that already crossed out keeps the address the host may have
    /// compared against.
    pub fn typed_thunk_for(
        &self,
        guest_va: u64,
        signature: crate::thunk::ThunkSignature,
    ) -> Result<u64, VmError> {
        if let Some(existing) = self.host_thunk_for(guest_va) {
            return Ok(existing);
        }
        let thunk = crate::thunk::build_typed(guest_va, signature)?;
        let host = self.insert_thunk(thunk);
        // As in `thunk_for`: handing the pointer out is the demand signal.
        let _ = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_lift(&self.program, guest_va);
        Ok(host)
    }

    pub fn thunk_for(&self, guest_va: u64) -> Result<u64, VmError> {
        if let Some(existing) = self.host_thunk_for(guest_va) {
            return Ok(existing);
        }
        let thunk = crate::thunk::build(guest_va)?;
        let host = self.insert_thunk(thunk);

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
        self.ask_thunks(|view| view.guest_for(va), |table| table.guest_for(va))
    }

    /// The thunk already built for a guest address, if there is one.
    fn host_thunk_for(&self, guest_va: u64) -> Option<u64> {
        self.ask_thunks(
            |view| view.host_for(guest_va),
            |table| table.host_for(guest_va),
        )
    }

    /// Ask the published snapshot, and fall back to the lock only when a miss
    /// might be stale.
    ///
    /// The published half is what every indirect call consults; see
    /// [`crate::thunk::ThunkView`] for why a miss cannot simply be believed.
    /// `count` is loaded *after* the snapshot, so an insert that lands between
    /// the two shows up as a disagreement and sends this one caller to the lock
    /// rather than being missed.
    fn ask_thunks(
        &self,
        from_view: impl Fn(&crate::thunk::ThunkView) -> Option<u64>,
        from_table: impl Fn(&ThunkTable) -> Option<u64>,
    ) -> Option<u64> {
        let published = self.thunk_view.load(Ordering::Acquire);
        if !published.is_null() {
            // SAFETY: the pointer was leaked by `publish_thunks` and is never
            // freed, so it stays valid for the life of the process.
            let view = unsafe { &*published };
            if let Some(hit) = from_view(view) {
                return Some(hit);
            }
            if view.count == self.thunk_count.load(Ordering::Acquire) {
                return None;
            }
        } else if self.thunk_count.load(Ordering::Acquire) == 0 {
            // Nothing has ever crossed outward, which is the whole answer for a
            // program that hands the host no function pointers at all.
            return None;
        }
        from_table(&self.thunks.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Add a thunk and republish the snapshot, both under the one lock.
    fn insert_thunk(&self, thunk: crate::thunk::Thunk) -> u64 {
        let mut table = self.thunks.lock().unwrap_or_else(|e| e.into_inner());
        let host = table.insert(thunk);
        let view = Box::leak(Box::new(table.view()));
        let count = view.count;
        self.thunk_view.store(view, Ordering::Release);
        // After the view, so a reader that sees this count has already had the
        // chance to see the view that produced it.
        self.thunk_count.store(count, Ordering::Release);
        host
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
            say(&err);
            *slot = Some(err);
        }
    }

    /// Take whatever error a callback left behind on this thread.
    pub fn take_pending(&self) -> Option<VmError> {
        crate::thread::current().and_then(|s| s.pending.borrow_mut().take())
    }

    /// Whether a callback left an error behind, without taking it.
    ///
    /// [`Vm::run_loop`] takes the parked error after every region exit, which
    /// was enough while every host call *was* one. A bridged call is not: it
    /// returns inline, and a failure raised underneath it — a guest callback the
    /// host invoked, which is where parked errors come from in the first place —
    /// would sit in the slot unread until some later exit happened to notice, or
    /// never. See [`crate::bridge`].
    pub(crate) fn has_pending(&self) -> bool {
        crate::thread::current().is_some_and(|s| s.pending.borrow().is_some())
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
    let name = b"rosetta3 (microlift x86-64 on arm64) @ 2.67GHz";
    let mut out = [0u8; 48];
    let mut i = 0;
    while i < name.len() {
        out[i] = name[i];
        i += 1;
    }
    out
}

fn cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
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
        // Vendor String: "GenuineIntel"
        0x0 => [
            MAX_BASIC_LEAF,
            u32::from_le_bytes(*b"Genu"),
            u32::from_le_bytes(*b"ntel"),
            u32::from_le_bytes(*b"ineI"),
        ],

        // Family / Model / Stepping & Standard Features
        0x1 => {
            // Family 6, Model 44 (0x2C), Stepping 0:
            // Stepping [3:0]   = 0x0
            // Base Model [7:4] = 0xC
            // Base Family [11:8] = 0x6
            // Ext Model [19:16] = 0x2
            // Ext Family [27:20] = 0x00
            // Signature = 0x000206C0
            let version = 0x0002_06C0;

            // Cache line size (64 bytes -> 8 units of 8-bytes) in bits 15:8
            let misc = 8 << 8;

            // FPU | TSC | CX8 | CMOV | SSE | SSE2
            let edx = (1 << 0) | (1 << 4) | (1 << 8) | (1 << 15) | (1 << 25) | (1 << 26);

            // SSE3 (bit 0) | SSSE3 (bit 9) | SSE4.1 (bit 19) | SSE4.2 (bit 20) | Hypervisor (bit 31)
            //
            // **AES-NI (bit 25) is deliberately absent, and this is the rule the
            // whole word is written by: a feature is advertised when microlift
            // can execute it, not when the host CPU can.** This CPU has AES
            // instructions; the recompiler does not lift `AESENC` and friends
            // yet, so a guest told otherwise stops at
            // `unimplemented-instruction (Aeskeygenassist_xmm_xmmm128_imm8)` at
            // the moment it first encrypts something. Roblox does, during its
            // network handshake, and the failure is a dead process rather than
            // a slow one.
            //
            // Not advertising it is not a loss of function: every library that
            // uses AES-NI reaches it through a run-time `CPUID` check precisely
            // so it can run on machines without it, and the software path it
            // falls back to is code microlift already handles. This bit goes
            // back the day microlift's x86-64 frontend grows the six AES
            // instructions — CPUID is the only thing that needs to change.
            let ecx = (1 << 0) | (1 << 9) | (1 << 19) | (1 << 20) | (1 << 31);

            [version, misc, ecx, edx]
        }

        // Leaf 4: Deterministic Cache Parameters
        0x4 => match subleaf {
            // L1 Data Cache (128 KB, 64-byte line, 8-way)
            0 => {
                let eax = (1 << 0) | (1 << 5) | (1 << 8); // Type: Data (1), Level: 1, Self Initializing
                let ebx = (63 << 0) | (0 << 12) | (7 << 22); // Line size (63+1=64), Partitions (0+1=1), Ways (7+1=8)
                let ecx = 255; // Sets (255+1=256) -> 256 * 64 * 1 * 8 = 128 KB
                let edx = 0;
                [eax, ebx, ecx, edx]
            }
            // L1 Instruction Cache (128 KB, 64-byte line, 8-way)
            1 => {
                let eax = (2 << 0) | (1 << 5) | (1 << 8); // Type: Instruction (2), Level: 1, Self Initializing
                let ebx = (63 << 0) | (0 << 12) | (7 << 22); // Line size 64, 8-way
                let ecx = 255; // Sets 256 -> 128 KB
                let edx = 0;
                [eax, ebx, ecx, edx]
            }
            // L2 Unified Cache (8192 KB / 8 MB, 64-byte line, 16-way)
            2 => {
                let eax = (3 << 0) | (2 << 5) | (1 << 8); // Type: Unified (3), Level: 2, Self Initializing
                let ebx = (63 << 0) | (0 << 12) | (15 << 22); // Line size 64, Ways (15+1=16)
                let ecx = 8191; // Sets (8191+1=8192) -> 8192 * 64 * 1 * 16 = 8192 KB
                let edx = 0;
                [eax, ebx, ecx, edx]
            }
            // No more caches
            _ => [0, 0, 0, 0],
        },

        // Leaf 7: Structured Extended Feature Flags
        0x7 => [0, 0, 0, 0], // Sub-leaf 0: No AVX2, BMI, AVX-512, SHA, etc.

        // Extended Leaves
        0x8000_0000 => [MAX_EXTENDED_LEAF, 0, 0, 0],

        // Extended Processor Info (64-bit Long Mode enabled)
        0x8000_0001 => [0, 0, 0, 1 << 29],

        // Brand String across leaves 0x80000002 to 0x80000004
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
            (13, "cmpxchg16b"),
            (23, "popcnt"),
            // The host CPU has AES instructions and microlift does not lift
            // them, which is the case this whole test is about: Roblox asks
            // here before its network handshake and stops at an unimplemented
            // `AESKEYGENASSIST` if the answer is yes.
            (25, "aes"),
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
