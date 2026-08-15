//! One guest thread per host thread, and the state that makes that true.
//!
//! ## What a guest thread is
//!
//! Nothing. That is the design: a guest thread *is* a host thread, created by
//! the host's own `pthread_create`, scheduled by the host's kernel, joined by
//! the host's `pthread_join`. The guest's start routine goes out as an ordinary
//! [`crate::thunk`] like any other callback, and the only thing that makes it a
//! thread rather than a comparator is that the host calls it on a stack this
//! runtime has never seen.
//!
//! The one thing that is *not* nothing is [`spawn`], and it is one function
//! deep. A thread has to be given its guest stack before it runs a line of
//! guest code, and what stack it should get is something only the caller of
//! `pthread_create` heard — so the request crosses the thread boundary in a
//! box, and the trampoline underneath [`spawn`] does the installing before it
//! calls the thunk. Any other way of making a guest thread — a Mach
//! `thread_create`, an `NSThread`, whatever arrives next — needs a
//! [`StackRequest`] and nothing else from this module.
//!
//! Which is exactly what has to be fixed here. Milestone 3's VM held one guest
//! stack, one live stack pointer, one nesting depth and one parked error, on the
//! reasonable assumption that there was one guest. Two host threads sharing
//! those would interleave their frames on one stack, and the result is not a
//! diagnostic — it is a program that corrupts itself. So they move here, one set
//! per host thread, created on demand the first time a thread enters guest code.
//!
//! Everything *else* stays shared, and correctly so: the code cache (a
//! translation is thread-independent), the thunk table (a guest pointer must map
//! to the same host address for every thread that sees it), and guest memory,
//! which is shared because the guest's threads genuinely share an address space.
//! Races in it are the guest's own to have, exactly as they would be on Intel.
//!
//! ## Why this is a raw pthread key and not `thread_local!`
//!
//! Rust's `thread_local!` is pthread thread-specific data with a "destroyed"
//! flag in front of it, and macOS trips that flag on the main thread *inside*
//! `exit` — which is precisely when the guest's `atexit` handlers run and
//! re-enter the JIT. M3 recorded that as the reason a thunk falls back to a
//! process-lifetime pointer; the same hazard applies to everything here, and
//! this time there is no plausible fallback, because a stack pointer is
//! genuinely per-thread.
//!
//! A raw key has no such flag: the value stays readable until the thread ends,
//! and on the main thread it is simply never reclaimed, which at process exit is
//! the right answer anyway. The destructor frees a *secondary* thread's stack
//! when it finishes, so a guest that spawns and joins in a loop does not
//! accumulate one 8 MiB reservation per thread.
//!
//! The one thing the destructor cannot promise is ordering against the guest's
//! own thread-specific destructors: `pthread_key_create` hands the guest a
//! destructor slot too, and libpthread does not say whose runs first. A guest
//! destructor that runs after this one finds no state and gets a fresh stack,
//! which is wasteful and not wrong.
//!
//! ## `%gs`, and why it is the host's own thread-specific data
//!
//! An x86-64 macOS program does not have to call `pthread_getspecific` to read a
//! thread-specific value. `pthread_getspecific` *is* one instruction —
//! `movq %gs:(,%rax,8), %rax` — and a compiler that can see the key inlines it,
//! which Unity's allocator does for every temporary allocation it makes. So a
//! guest reaches thread-specific data through the `%gs` segment base, and this
//! runtime has to have one.
//!
//! It does, and it is not this runtime's to invent: the `%gs` base on x86-64
//! Darwin and `TPIDRRO_EL0` on arm64 Darwin address the *same array* — one
//! pointer per key, indexed by the key number, with the same reserved slots at
//! the front. The guest got its key from the host's own `pthread_key_create`
//! through native passthrough, so the number in its hand is already the host's.
//! Pointing the guest's `%gs` at [`tsd_base`] therefore makes the guest's
//! inlined read and the host's `pthread_getspecific` name one slot, rather than
//! two that would have to be kept in step.
//!
//! ## What another thread is allowed to see
//!
//! Everything above is the owning thread's own business, and the `Cell`s say
//! so. One thing is not: a **conservative garbage collector suspends every
//! other thread and asks the kernel for its registers**, which for a guest
//! thread is a question only this runtime can answer. Boehm — the collector
//! inside `libmonobdwgc-2.0`, which is the one Unity ships — asks with
//! `thread_get_state(x86_THREAD_STATE64)` and aborts the process if it fails.
//!
//! So the part of a guest thread another thread may read lives in
//! [`GuestThread`], separately and in atomics, and [`registry`] maps a Mach
//! thread port to it. `ThreadState` itself is never handed across a thread
//! boundary: it holds `Cell`s and a `RefCell`, and sharing it would be a data
//! race dressed up as a lookup.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::c_void;
use std::ops::Range;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::link::{BlockLinkConfig, BlockLinkEntry};

use crate::stack::GuestStack;
use crate::vm::VmError;

/// The part of a guest thread another thread may read, and the whole of it.
///
/// Atomics rather than `Cell`s because the reader is a collector running on
/// somebody else's thread, usually with this one suspended. Nothing here is a
/// lock: a suspended thread cannot release one, so a collector that took a lock
/// to read a suspended thread's registers would deadlock the process it was
/// trying to collect.
pub struct GuestThread {
    /// Address of the innermost live [`RegisterContext`] on this thread, or
    /// zero when the thread is not inside guest code.
    ///
    /// The context is a local of [`crate::vm::Vm::call_with_stack`], so it is
    /// live exactly while that frame is, which is what publishing and clearing
    /// it around the call says.
    context: AtomicU64,
    /// The guest stack, which is *not* the host stack this thread was created
    /// with — see [`crate::shims::machthread`] for why that matters to anything
    /// asking `pthread_get_stackaddr_np`.
    stack: Range<u64>,
}

impl GuestThread {
    /// The guest's registers, as far as this runtime can state them.
    ///
    /// `None` when the thread is registered but has no live guest frame: it has
    /// been inside guest code before and is not now, so there is nothing to
    /// report and nothing on the guest stack to scan.
    pub fn context(&self) -> Option<&RegisterContext> {
        let at = self.context.load(Ordering::Acquire);
        // SAFETY: non-zero means `call_with_stack` has published its own local
        // and has not yet returned, so the context outlives this borrow. The
        // reader is either this thread or one that has suspended this one,
        // which is the only arrangement in which the value can be read at all.
        (at != 0).then(|| unsafe { &*(at as *const RegisterContext) })
    }

    /// Where this thread's guest frames live.
    pub fn stack(&self) -> Range<u64> {
        self.stack.clone()
    }
}

/// One registry entry: a Mach thread port and the state behind it.
struct Slot {
    port: AtomicU32,
    thread: AtomicPtr<GuestThread>,
}

/// Guest threads by Mach thread port, looked up **without taking a lock**.
///
/// The reason is the caller. A collector suspends every other thread and then
/// asks about each one in turn, so anything it touches must be reachable while
/// arbitrary threads are stopped mid-instruction. A `Mutex` here would deadlock
/// the moment a thread was suspended between `lock` and `unlock` — a window of
/// a few instructions, entered once per thread, which is exactly the kind of
/// odds that turn into a hang after twenty minutes rather than a failure in
/// testing.
///
/// A fixed table of slots, claimed by compare-exchange, scanned linearly. Five
/// hundred and twelve is far more threads than a game has and the scan is
/// half a kilobyte of atomics, which costs nothing next to the `mach_msg` the
/// collector made to suspend everybody.
///
/// The one race left is a *miss*: a thread that has claimed its slot and not
/// yet published its port is invisible for those few instructions. It is also a
/// thread that has not yet entered guest code, so it has no guest frames to
/// report — the answer it would have been given is the empty one.
static TABLE: [Slot; SLOTS] = [const {
    Slot {
        port: AtomicU32::new(0),
        thread: AtomicPtr::new(core::ptr::null_mut()),
    }
}; SLOTS];

const SLOTS: usize = 512;

/// Publish `thread` under `port`. The `Arc` is leaked into the table.
///
/// Leaked rather than reference-counted because the reader may be holding the
/// pointer while this thread dies, and there is no way to hand a suspended
/// thread a retain. A [`GuestThread`] is five words; the stack it describes,
/// which is the eight megabytes worth freeing, is released as before.
fn register(port: u32, thread: Arc<GuestThread>) {
    let raw = Arc::into_raw(thread) as *mut GuestThread;
    for slot in &TABLE {
        // The slot is claimed by its *pointer* and published by its port, in
        // that order, so a lookup that matches a port always finds a pointer.
        if slot
            .thread
            .compare_exchange(
                core::ptr::null_mut(),
                raw,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            slot.port.store(port, Ordering::Release);
            return;
        }
    }
    // Out of slots. Not fatal and not silent: the thread runs, and a collector
    // asking about it gets the host's answer, which is the behaviour before any
    // of this existed.
    eprintln!(
        "rosetta3: more than {SLOTS} guest threads; thread {port} will not be          described to a collector"
    );
    // SAFETY: `raw` came from `Arc::into_raw` immediately above and no slot
    // took it, so this is the only reference.
    drop(unsafe { Arc::from_raw(raw) });
}

/// Withdraw `port`, freeing its slot for a thread created later.
fn unregister(port: u32) {
    for slot in &TABLE {
        if slot.port.load(Ordering::Acquire) == port {
            // Port first: a lookup racing this sees zero and misses, which is
            // the right answer for a thread that is ending.
            slot.port.store(0, Ordering::Release);
            slot.thread.store(core::ptr::null_mut(), Ordering::Release);
            return;
        }
    }
}

/// This host thread's Mach port name, which is what `thread_get_state` is given.
///
/// `pthread_mach_thread_np` rather than `mach_thread_self`, because the latter
/// takes a *reference* on the port every call and would have to be balanced by
/// a `mach_port_deallocate` that nothing here is placed to make. Both name the
/// same port.
pub fn mach_port() -> u32 {
    // SAFETY: `pthread_self` is always valid on a running thread and
    // `pthread_mach_thread_np` reads the port name out of it.
    unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
}

/// The guest thread registered under `port`, if it is one.
///
/// Returns a borrow rather than an `Arc` because the entry is leaked and so
/// outlives every caller; cloning an `Arc` here would be a needless atomic on a
/// path that runs with the world stopped.
pub fn by_port(port: u32) -> Option<&'static GuestThread> {
    if port == 0 {
        return None;
    }
    for slot in &TABLE {
        if slot.port.load(Ordering::Acquire) == port {
            let raw = slot.thread.load(Ordering::Acquire);
            // SAFETY: a non-null pointer in the table came from
            // `Arc::into_raw` in `register` and is never freed — see the note
            // there on why it is leaked.
            return (!raw.is_null()).then(|| unsafe { &*raw });
        }
    }
    None
}

/// Call `f` with the Mach port of every registered guest thread.
///
/// Takes no lock, for the reason the table itself takes none: the caller is a
/// [profiler](crate::profile) that suspends each thread as it is named, and a
/// lock held across a suspend is a lock the suspended thread can never be
/// waiting on again.
///
/// A thread that ends mid-walk is reported and then fails to suspend, which is
/// the ordinary race and is why the sampler treats a failed suspend as a miss
/// rather than an error.
pub fn each_port(mut f: impl FnMut(u32)) {
    for slot in &TABLE {
        let port = slot.port.load(Ordering::Acquire);
        if port != 0 {
            f(port);
        }
    }
}

/// Everything about executing guest code that belongs to one host thread.
pub struct ThreadState {
    /// The half of this state other threads read. Held here as well so the
    /// owning thread updates it without going near the registry's lock.
    published: Arc<GuestThread>,
    stack: GuestStack,
    /// Where the next nested guest entry should put its first frame.
    pub(crate) rsp: Cell<u64>,
    /// How many host→guest callbacks are live on this thread.
    pub(crate) depth: Cell<usize>,
    /// An error raised inside a host→guest callback, which cannot be returned
    /// through the host function that caused it. Collected here and re-raised as
    /// soon as control is back in the runtime *on this thread* — a callback's
    /// failure is not another thread's business.
    pub(crate) pending: RefCell<Option<VmError>>,
    /// Which entry into guest code is live on this thread.
    ///
    /// Nesting depth would not do: entries at the same depth are different
    /// entries, and the question this answers is identity, not distance. What
    /// asks it is [`crate::jmpbuf`] — a `longjmp` whose `setjmp` happened in an
    /// *outer* entry has host frames between the two, and restoring the guest
    /// context would abandon them silently. A monotonic id makes that case a
    /// comparison rather than a guess.
    entry: Cell<u64>,
    /// Whether the call being serviced set `rip` itself instead of returning.
    ///
    /// `longjmp` is the only thing that does. It matters because a compiler tail
    /// calls freely — `jmp _longjmp` at the end of a function — and the runtime's
    /// tail-call path finishes an intercepted call by popping a return address
    /// over `rip`, which for a `longjmp` would discard the destination it just
    /// restored. See [`crate::vm`].
    redirected: Cell<bool>,
    /// Whether the import being serviced was reached by a **jump** rather than a
    /// call.
    ///
    /// The mirror image of `redirected`, and it exists for the one shim that has
    /// to stand in for the call itself: `objc_msgSend` is a trampoline, so a
    /// message to a guest method is serviced by setting `rip` to the `IMP`. Where
    /// the callee's return address comes from depends entirely on how control
    /// arrived — an intercepted `call` has not pushed one and a tail `jmp` has —
    /// and guessing produces a return to the wrong frame rather than a fault. So
    /// the loop states it. See [`crate::objc::dispatch`].
    entered_by_jump: Cell<bool>,
    /// The exception being unwound on this thread, if any.
    ///
    /// Per thread because a stack is: two threads throwing at once are two
    /// independent walks over two stacks, and nothing about one is the other's
    /// business. See [`crate::unwind`].
    unwinding: Cell<Option<crate::unwind::InFlight>>,
    /// Exception headers whose `catch` blocks are running, innermost last.
    ///
    /// A stack rather than a single value because a `catch` block may itself
    /// contain a `try`, and `__cxa_end_catch` takes no argument: it means "the
    /// one this frame began", which is only answerable by remembering the order
    /// they began in.
    caught: RefCell<Vec<u64>>,
    /// Translations this thread has already entered, by the PC it entered them
    /// at. See [`CodeWindow`].
    code: CodeWindow,
}

impl ThreadState {
    /// Wrap a stack this thread will run the guest on.
    pub fn new(stack: GuestStack) -> Self {
        let rsp = stack.initial_rsp();
        let published = Arc::new(GuestThread {
            context: AtomicU64::new(0),
            stack: stack.range(),
        });
        ThreadState {
            published,
            stack,
            rsp: Cell::new(rsp),
            depth: Cell::new(0),
            pending: RefCell::new(None),
            entry: Cell::new(0),
            redirected: Cell::new(false),
            entered_by_jump: Cell::new(false),
            unwinding: Cell::new(None),
            caught: RefCell::new(Vec::new()),
            code: CodeWindow::new(),
        }
    }

    /// Publish `ctx` as this thread's live guest context, returning whatever
    /// was published before so a nested entry can restore it.
    pub(crate) fn publish_context(&self, ctx: *mut RegisterContext) -> u64 {
        self.published
            .context
            .swap(ctx as usize as u64, Ordering::Release)
    }

    /// Restore what [`ThreadState::publish_context`] displaced.
    pub(crate) fn restore_context(&self, previous: u64) {
        self.published.context.store(previous, Ordering::Release);
    }

    /// This thread's view of the code cache.
    pub(crate) fn code(&self) -> &CodeWindow {
        &self.code
    }

    /// The entry into guest code currently live on this thread, or zero when
    /// none is.
    pub(crate) fn entry(&self) -> u64 {
        self.entry.get()
    }

    /// Begin a new entry, returning the previous one to be restored after it.
    ///
    /// Ids come from one process-wide counter rather than a per-thread one, so
    /// that a buffer filled on another thread is distinguishable from one filled
    /// in another entry of this thread — two different refusals with two
    /// different reasons.
    pub(crate) fn enter(&self) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let previous = self.entry.get();
        self.entry
            .set(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        previous
    }

    /// Restore the entry that was live before [`ThreadState::enter`].
    pub(crate) fn leave(&self, previous: u64) {
        self.entry.set(previous);
    }

    /// The exception being unwound on this thread.
    pub(crate) fn in_flight(&self) -> Option<crate::unwind::InFlight> {
        self.unwinding.get()
    }

    /// Set or clear it. Cleared by `__cxa_end_catch`, and by a walk that ends
    /// with nowhere to go.
    pub(crate) fn set_in_flight(&self, flight: Option<crate::unwind::InFlight>) {
        self.unwinding.set(flight);
    }

    /// Note that a `catch` block for `header` has begun on this thread.
    pub(crate) fn begin_catch(&self, header: u64) {
        self.caught.borrow_mut().push(header);
    }

    /// The exception whose `catch` block is innermost, without ending it.
    pub(crate) fn caught(&self) -> Option<u64> {
        self.caught.borrow().last().copied()
    }

    /// End the innermost `catch` block, naming the exception it was for.
    pub(crate) fn end_catch(&self) -> Option<u64> {
        self.caught.borrow_mut().pop()
    }

    /// Record that the call being serviced transferred control rather than
    /// returning: its `rip` is a destination, not a resume point.
    pub(crate) fn redirect(&self) {
        self.redirected.set(true);
    }

    /// Whether the last serviced call redirected, clearing the flag.
    pub(crate) fn took_redirect(&self) -> bool {
        self.redirected.replace(false)
    }

    /// Record how the import now being serviced was reached.
    pub(crate) fn set_entered_by_jump(&self, jumped: bool) {
        self.entered_by_jump.set(jumped);
    }

    /// Whether it was a jump. Only meaningful while a shim is running.
    pub(crate) fn entered_by_jump(&self) -> bool {
        self.entered_by_jump.get()
    }

    /// `(argc, argv, envp, apple)`, as laid out on this thread's stack.
    ///
    /// Only the thread that started the program has them; a stack made for a
    /// spawned thread reports zeros, because a `pthread` start routine takes one
    /// argument and none of these.
    pub fn main_args(&self) -> (u64, u64, u64, u64) {
        self.stack.main_args()
    }

    /// The address range this thread's guest stack occupies.
    pub fn stack_range(&self) -> Range<u64> {
        self.stack.range()
    }

    /// The guest stack pointer as of the last thing that moved it.
    ///
    /// For [`crate::fault`], which asks on a thread that is about to die and
    /// wants the answer without a borrow it could not take: both sources below
    /// are plain reads of memory this thread owns, and the reader is that
    /// thread.
    ///
    /// The **live context** is preferred, and that is not a refinement: every
    /// region exit syncs `rsp` into it, while [`rsp`](Self::rsp) is set by the
    /// run loop — and since regions link to each other, a whole chain of exits
    /// can happen without the run loop seeing one. The `Cell` is the answer only
    /// when there is no live context to ask, which is a thread that is not
    /// inside guest code at all.
    pub fn live_rsp(&self) -> u64 {
        match self.published.context() {
            Some(ctx) => ctx.rsp,
            None => self.rsp.get(),
        }
    }
}

/// One thread's window onto the code cache: the translations it has entered,
/// keyed by the PC it entered them at.
///
/// ## Why the shared cache needed one in front of it
///
/// [`crate::cache::CodeCache`] lives behind a mutex, and the run loop asked it
/// for a translation **on every region entry** — which is the most frequent
/// thing this runtime does. Two costs came out of that, and a profile of
/// Starbound showed both: the main thread spent a quarter of its samples inside
/// the lookup itself (a `BTreeMap` walk down to `pc`, then a `covers` test per
/// candidate), and a second guest thread spent *every* sample parked in
/// `__psynch_mutexwait`, because one lock in front of every region entry makes
/// a multi-threaded guest single-threaded.
///
/// So each thread keeps its own answers. A hit takes no lock, no comparison
/// chain and no allocation — one mask, one load, one address compare — and the
/// shared cache is consulted only when this thread has not run that PC before.
///
/// ## Why a stale entry is not a stale answer
///
/// The window is never invalidated, and it does not need to be, for the reason
/// the cache itself is never invalidated: **guest code is assumed immutable**
/// (see [`crate::cache`]). A translation that could be entered at `pc` once can
/// be entered at `pc` forever. The shared cache may later lift a *different*
/// translation covering the same address — regions overlap — and entering
/// either is correct, so the two answers are interchangeable rather than one
/// being fresher.
///
/// The `Arc` is what makes that literally true rather than nearly: a region
/// re-lifted at a key that already had one *replaces* it in the shared map, and
/// the strong reference held here keeps the old code mapped for as long as this
/// thread might enter it.
///
/// ## Direct-mapped, and deliberately not more
///
/// One slot per index, no chaining, no eviction policy: a colliding PC simply
/// takes the slot. Sizing it a few times larger than the working set makes
/// collisions rare, and a collision costs exactly one trip to the shared cache
/// — the thing that used to happen every time. Anything cleverer would be
/// paying for a hit rate that is already in the high nineties, on the path
/// where nothing may be paid for.
///
/// ## The same window, readable by compiled code
///
/// [`links`](Self::links) is that map again in a second form: one
/// [`BlockLinkEntry`] per slot, holding the same translation's *code address*
/// rather than an `Arc` to it, laid out the way microlift's emitted probe reads
/// it. The two are written together by [`put`](Self::put) and are the same
/// answer — which is what makes the `Arc` in a slot the thing that keeps the
/// code address in the row beside it mapped.
///
/// It exists because the run loop's lookup, cheap as it is, is still a `RefCell`
/// borrow, an `Arc` clone and a return to Rust, per region entry, fifteen
/// million times a second. Translated code can answer the same question in eight
/// instructions without leaving. See [`microlift::core::link`] and
/// `docs/HOTSPOTS.md`.
pub(crate) struct CodeWindow {
    slots: RefCell<Box<[Option<(u64, std::sync::Arc<crate::cache::Translation>)>]>>,
    /// The rows lifted code probes, one per [`slots`](Self::slots) index.
    ///
    /// An `UnsafeCell` rather than a `RefCell` because the readers are not Rust:
    /// the address of this array is published in `ctx.link_table` and read by
    /// compiled guest code, which no borrow flag can be checked against. The
    /// discipline that makes it sound is stated once here —
    ///
    /// - **One thread.** A `CodeWindow` belongs to a [`ThreadState`], which is
    ///   this thread's alone; nothing else reads or writes these rows.
    /// - **Never while guest code runs.** The only writer is [`put`](Self::put),
    ///   which the run loop calls between region entries. A nested entry (a host
    ///   callback re-entering the guest) can write rows while an *outer* frame
    ///   is suspended inside a region — but that frame is stopped in a shim, not
    ///   executing the probe, and it re-reads the table when it resumes.
    /// - **A row is never dangling.** It is replaced only together with the
    ///   `Arc` in the same slot, and a replaced translation is retired rather
    ///   than dropped ([`crate::cache::CodeCache`]).
    /// - **A stale row is a correct row.** Same argument as the window's own:
    ///   two translations covering an address are interchangeable.
    links: UnsafeCell<Box<[BlockLinkEntry]>>,
}

/// How many PCs one thread remembers. A power of two, so the index is a mask.
///
/// 8192 slots is 128 KiB of pointers per guest thread, against a warm working
/// set that Starbound measures in the low thousands of regions.
const WINDOW: usize = 8192;

/// The table geometry, shared with the lifter so that the slot lifted code
/// computes and the slot [`CodeWindow::put`] writes are the same arithmetic
/// rather than two copies of it.
pub fn link_config() -> BlockLinkConfig {
    let config = BlockLinkConfig::new(WINDOW as u32);
    // Which of the two shapes of linking this process runs: a row handed back
    // to `cache::follow_links`, or a branch straight into the successor with
    // the guest registers still in registers. See [`crate::jit::pinned_link_abi`].
    match crate::jit::pinned_link_abi() {
        Some(abi) => config.pinned(abi),
        None => config,
    }
}

impl CodeWindow {
    fn new() -> Self {
        CodeWindow {
            slots: RefCell::new(vec![None; WINDOW].into_boxed_slice()),
            links: UnsafeCell::new(vec![BlockLinkEntry::default(); WINDOW].into_boxed_slice()),
        }
    }

    /// The slot a PC maps to.
    ///
    /// The low four bits are dropped before masking: guest functions are
    /// aligned, so those bits are the least distinguishing ones an address has,
    /// and using them would leave most of the table empty.
    ///
    /// Deferred to [`link_config`] so that this and the shift-and-mask microlift
    /// bakes into every region cannot drift apart: the link rows and the `Arc`s
    /// share an index, and an index they disagreed on would be a row published
    /// where nothing keeps its code mapped.
    fn slot(pc: u64) -> usize {
        link_config().slot(pc)
    }

    /// The address of the block-link table for this thread, as `ctx.link_table`
    /// wants it.
    pub(crate) fn link_table(&self) -> *mut BlockLinkEntry {
        // SAFETY: taking the address of the allocation, not reading it. The
        // `Box` is owned by this `CodeWindow` and never reallocated, so the
        // address is stable for the life of the thread — which is what lets it
        // be handed to compiled code at all.
        unsafe { (*self.links.get()).as_mut_ptr() }
    }

    /// The translation this thread last entered at `pc`, if it still holds one.
    pub(crate) fn get(&self, pc: u64) -> Option<std::sync::Arc<crate::cache::Translation>> {
        match self.slots.borrow().get(Self::slot(pc)) {
            Some(Some((at, translation))) if *at == pc => Some(std::sync::Arc::clone(translation)),
            _ => None,
        }
    }

    /// Remember that `translation` may be entered at `pc`.
    pub(crate) fn put(&self, pc: u64, translation: &std::sync::Arc<crate::cache::Translation>) {
        let slot = Self::slot(pc);
        // `try_borrow_mut` rather than `borrow_mut`: this is reached from the
        // run loop, and the run loop is re-entrant — a guest callback runs
        // another one inside the first. Nothing here is `&mut` across a call
        // into guest code, so a live borrow cannot actually overlap; refusing
        // to record rather than panicking is the honest way to say that this is
        // a cache and skipping it costs nothing.
        if let Ok(mut slots) = self.slots.try_borrow_mut() {
            slots[slot] = Some((pc, std::sync::Arc::clone(translation)));
            // The link row goes in with the `Arc`, and only with it: the
            // reference held here is what keeps the code address in the row
            // mapped, so a row published without one — or left behind when one
            // is replaced — is the one way this could name freed pages. The
            // borrow above is also what serialises this against a nested run
            // loop writing the same slot.
            //
            // SAFETY: the only writer, on the thread that owns this window, at a
            // point where this thread is in the run loop rather than inside a
            // region. See the field's own note.
            unsafe {
                (*self.links.get())[slot] = translation.link_row(pc);
            }
        }
    }
}

/// The key every thread's [`ThreadState`] hangs off.
static KEY: OnceLock<libc::pthread_key_t> = OnceLock::new();

fn key() -> libc::pthread_key_t {
    *KEY.get_or_init(|| {
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: `key` is a live local of the right type, and `release` matches
        // the destructor signature libpthread calls it with.
        let rc = unsafe { libc::pthread_key_create(&mut key, Some(release)) };
        assert_eq!(
            rc, 0,
            "pthread_key_create failed; the runtime cannot run a guest thread"
        );
        key
    })
}

/// Free a finished thread's state. Never called on the main thread.
unsafe extern "C" fn release(value: *mut c_void) {
    if value.is_null() {
        return;
    }
    // Unregistered first: after this the port may be reused by a thread created
    // later, and an entry naming a freed stack is worse than no entry. The
    // destructor runs on the thread that is ending, so `mach_port` is still its
    // own.
    unregister(mach_port());
    // SAFETY: the only thing ever stored under this key is a `Box::into_raw` of
    // a `ThreadState`, and libpthread calls this once, with the key cleared, so
    // the box is not reachable from anywhere else.
    drop(unsafe { Box::from_raw(value as *mut ThreadState) });
}

/// This thread's state, if it has entered guest code before.
///
/// The `'static` is the same lie [`crate::thunk::current`] tells and contained
/// the same way: the state lives until the thread does, and no caller stores the
/// reference past its own frame.
pub fn current() -> Option<&'static ThreadState> {
    // SAFETY: reading a thread-specific value, which is null or the pointer
    // `install` stored. Dereferencing is sound because `release` is the only
    // thing that frees it and it runs after the last code on this thread.
    let p = unsafe { libc::pthread_getspecific(key()) } as *const ThreadState;
    (!p.is_null()).then(|| unsafe { &*p })
}

/// Give this thread `state`, replacing and freeing whatever it had.
pub fn install(state: ThreadState) -> &'static ThreadState {
    // Replacing rather than adding: a thread runs one guest at a time, and a
    // second `Vm` built on this thread is a new program, not a second stack for
    // the old one.
    // SAFETY: reads this thread's slot; anything non-null there is a box this
    // module leaked, and the slot is about to be overwritten so nothing can
    // reach it again.
    let old = unsafe { libc::pthread_getspecific(key()) };
    if !old.is_null() {
        drop(unsafe { Box::from_raw(old as *mut ThreadState) });
    }
    let raw = Box::into_raw(Box::new(state));
    // SAFETY: `raw` is a live leaked allocation; `release` takes ownership of it
    // when the thread ends.
    unsafe { libc::pthread_setspecific(key(), raw as *mut c_void) };
    // SAFETY: just stored, and nothing has run since that could free it.
    let state = unsafe { &*raw };
    // Registered under this thread's Mach port, which is how a collector on
    // another thread finds it. Replacing rather than inserting, for the reason
    // above: a second `Vm` on this thread is a new program.
    register(mach_port(), Arc::clone(&state.published));
    clear_errno();
    state
}

/// Hand this thread the `errno` a thread that has just started really has.
///
/// `errno` is the one piece of "guest" state that is not this runtime's to
/// build: it is `__error()` on the host thread the guest is running on, and
/// that is *correct* — a guest syscall is the host's libSystem call on the same
/// thread, so the two agree with no work at all. See the crate note.
///
/// What passthrough does not get for free is the **initial** value. A real
/// process reaches `main` with `errno` zero; this one reaches it with whatever
/// dyld, libSystem and Rust's own pre-`main` start-up left on the thread, which
/// on this machine is `EPERM` — set before a line of this runtime runs, so
/// there is nothing here to stop doing. A guest that prints `errno` after a
/// call that *succeeded* then reads a stale 1 where it would natively read 0.
/// `tests/thread.rs`'s `errno` fixture is exactly that program.
///
/// Here rather than in [`crate::startup`] because it is a claim about a
/// **thread**, not about a process, and this is the one place every thread that
/// becomes a guest thread passes through: the main thread from
/// [`Vm::with_policy`](crate::Vm::with_policy) and every spawned one from
/// [`ensure`]. Once per thread, never on a re-entry, so an `errno` the guest is
/// entitled to keep across a call out is never touched.
fn clear_errno() {
    // SAFETY: `__error()` returns this thread's own `errno` slot, which is
    // live for as long as the thread is.
    unsafe { *libc::__error() = 0 };
}

/// The base of this host thread's thread-specific data array — the value a
/// guest's `%gs` segment base must hold.
///
/// On arm64 Darwin the array's address is `TPIDRRO_EL0` with its low three bits
/// masked off; the register's spare bits carry the CPU number, which is not part
/// of the address. This is what libpthread's own `_os_tsd_get_base` does, and
/// the reason it is open-coded here rather than called is that it is a static
/// inline in a header no crate on this system exports.
///
/// Verified rather than assumed: `tests/tls.rs` checks that slot zero holds
/// `pthread_self` and that a key created by `pthread_key_create` reads back
/// through this base exactly what `pthread_getspecific` returns — including a
/// key past 256, where the arrays could plausibly have diverged and do not.
#[cfg(target_arch = "aarch64")]
pub fn tsd_base() -> u64 {
    let tpidrro: u64;
    // SAFETY: `mrs` from `TPIDRRO_EL0` is unprivileged and readable from EL0 on
    // every arm64 Apple Silicon core; it reads a register and touches no memory.
    unsafe {
        std::arch::asm!("mrs {}, tpidrro_el0", out(reg) tpidrro, options(nomem, nostack, preserves_flags))
    };
    tpidrro & !0b111
}

/// A host that is not arm64 has no `TPIDRRO_EL0`, and this runtime has no other
/// host. Zero is what the guest context held before any of this existed, so a
/// build elsewhere is no worse than it was: a `%gs`-relative access faults at a
/// small address rather than reading a wrong one.
#[cfg(not(target_arch = "aarch64"))]
pub fn tsd_base() -> u64 {
    0
}

/// Stack for a guest thread that asked for no particular one.
///
/// The main thread's size, which is the choice that cannot surprise a guest that
/// never said: a thread created with a null `pthread_attr_t`, or one this
/// runtime adopted because a host framework called a guest callback on a thread
/// of its own. Reservations are lazy, so the cost is address space.
pub const THREAD_STACK: usize = crate::stack::DEFAULT_STACK;

/// Where a guest thread's frames are to live.
///
/// The one thing a threading front end has to decide, and the reason it is a
/// value rather than a constant: `pthread_attr_setstacksize` and
/// `pthread_attr_setstack` are statements about *this* stack — the guest's own
/// frames — and until [`crate::shims::pthread`] existed there was nowhere for
/// either to be said. A front end for some other way of making a thread (a Mach
/// `thread_create`, an `NSThread`, a guest coroutine library switching stacks)
/// builds one of these and hands it to [`adopt`]; nothing else about the thread
/// machinery has to know which one it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackRequest {
    /// Reserve [`THREAD_STACK`] bytes, the main thread's size.
    ///
    /// For a thread nothing asked about: one this runtime *adopted* because a
    /// host framework called a guest callback on a worker of its own. A guest
    /// calling `pthread_create(&t, NULL, ...)` does not land here — a null
    /// attribute set means the host's defaults, which carry a size of their own
    /// (512 KiB on Darwin), and honouring it is what makes the thread the size
    /// the same program gets natively.
    Default,
    /// Reserve `bytes`, rounded up to a page. What the guest asked for through
    /// `pthread_attr_setstacksize`.
    Sized(usize),
    /// Use memory the guest already owns: `top` is the highest address,
    /// exclusive, matching `pthread_attr_getstackaddr`. See
    /// [`GuestStack::supplied`].
    Supplied { top: u64, size: u64 },
}

impl StackRequest {
    /// Build the stack this asks for.
    fn build(self) -> Result<GuestStack, VmError> {
        match self {
            StackRequest::Default => GuestStack::bare(THREAD_STACK),
            // A page more than asked for, because what the guest asked for is
            // the part it may use: libpthread puts a spawned thread's guard
            // page *outside* the size the attribute names, and a guest that
            // sets a size and reads it back with `pthread_get_stacksize_np`
            // would otherwise be told it got a page less than it asked for. See
            // [`GuestStack::bare`], which is where the other convention is.
            StackRequest::Sized(bytes) => {
                GuestStack::bare(bytes + machish::memory::host_page_size() as usize)
            }
            StackRequest::Supplied { top, size } => Ok(GuestStack::supplied(top, size)),
        }
    }
}

/// Make this host thread a guest thread with the stack `request` describes,
/// replacing any state it already had.
///
/// The one entry point a threading front end needs. Called on the thread that
/// is becoming a guest thread and nowhere else — a stack is only installable by
/// its owner, which is what the thread-specific key underneath this says.
pub fn adopt(request: StackRequest) -> Result<&'static ThreadState, VmError> {
    Ok(install(ThreadState::new(request.build()?)))
}

/// What a new guest thread is to run, and on what.
pub struct GuestStart {
    /// The **thunk** for the guest's start routine, not the guest address: the
    /// host is what calls it, so it has to be arm64 by the time it gets here.
    /// See [`crate::thunk`].
    pub entry: u64,
    /// The `void *` the guest passed, untranslated. Only guest code ever
    /// dereferences it, and the two sides share an address space.
    pub arg: u64,
    /// Where the guest's frames go.
    pub stack: StackRequest,
}

/// Start a host thread that runs `start` as a guest thread.
///
/// `attr` is the **host's** attribute set and describes the host thread only —
/// the thread that will run the JIT. It is the caller's to build, because
/// translating a guest's `pthread_attr_t` into one is a `pthread` question and
/// this module's business is threads rather than that API; see
/// [`crate::shims::pthread`], which is where the two stacks are separated.
///
/// Returns the host `pthread_t`, which is also the guest's: every other
/// `pthread` call the guest makes about the thread — `join`, `detach`, `equal`,
/// `kill`, `get_stackaddr_np` — is the host's own, operating on the host's own
/// thread. Nothing is invented here that the guest could compare against
/// something real and find different.
pub fn spawn(start: GuestStart, attr: &libc::pthread_attr_t) -> Result<libc::pthread_t, i32> {
    let mut thread: libc::pthread_t = 0;
    let boxed = Box::into_raw(Box::new(start)) as *mut c_void;
    // SAFETY: `entry` is this module's own function; `boxed` is a live leaked
    // allocation which the new thread takes ownership of, and which is
    // reclaimed here if no thread is ever started to take it.
    let rc = unsafe { libc::pthread_create(&mut thread, attr, entry, boxed) };
    if rc != 0 {
        // SAFETY: `pthread_create` failed, so nothing else holds the pointer.
        drop(unsafe { Box::from_raw(boxed as *mut GuestStart) });
        return Err(rc);
    }
    Ok(thread)
}

/// What libpthread calls on the new thread: install the guest state, then enter
/// the guest.
///
/// The order is the whole point of routing a spawned thread through here rather
/// than letting libpthread call the thunk directly. [`ensure`] would give this
/// thread a stack too — that is what M4 did — but it would give it the
/// *default* one, having no way to hear what the guest asked for. Installing
/// first means the request survives the thread boundary, which is the only
/// place it could have been lost.
extern "C" fn entry(raw: *mut c_void) -> *mut c_void {
    // Unboxed before the guest runs, and the fields copied out, because the
    // guest's start routine may call `pthread_exit` — which ends this thread
    // from underneath these Rust frames without unwinding them. A `Box` still
    // live at that moment is a leak once per thread that exits that way.
    // SAFETY: the pointer came from `Box::into_raw` in `spawn` and is handed to
    // exactly one thread, which is this one.
    let GuestStart { entry, arg, stack } = *unsafe { Box::from_raw(raw as *mut GuestStart) };

    if let Err(e) = adopt(stack) {
        // No error channel: the caller is libpthread, which has nowhere to put
        // this, and the guest is already holding a `pthread_t` for a thread it
        // believes is running. Said out loud, and the thread ends without
        // entering the guest — a start routine that never ran is visible as a
        // join returning null, which is at least a fact the guest can see.
        eprintln!("rosetta3: a guest thread could not be given a stack: {e}");
        return core::ptr::null_mut();
    }

    // SAFETY: `entry` is a thunk this runtime compiled — an arm64 function
    // taking the guest's six integer argument registers and eight vector ones,
    // of which a start routine reads the first. It stays mapped for the VM's
    // lifetime, which outlives every thread that can reach it. See
    // [`crate::thunk`].
    let f: extern "C" fn(u64) -> u64 = unsafe { core::mem::transmute::<u64, _>(entry) };
    f(arg) as *mut c_void
}

/// This thread's state, creating a default one if it has none.
///
/// The path a thread this runtime did not start takes: a host framework calls a
/// guest callback on a worker of its own, the thunk enters the VM, and the VM
/// finds no stack to enter on. A thread spawned through
/// [`crate::shims::pthread`] has already been through [`adopt`] and is found
/// here rather than built.
pub fn ensure() -> Result<&'static ThreadState, VmError> {
    if let Some(existing) = current() {
        return Ok(existing);
    }
    adopt(StackRequest::Default)
}
