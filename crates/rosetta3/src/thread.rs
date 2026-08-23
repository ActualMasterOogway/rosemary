//! One guest thread per host thread, and the state that makes that true.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::c_void;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::link::{BlockLinkConfig, BlockLinkEntry};
use microlift::runtime::ThreadLinks;
use static_init::dynamic;

use crate::stack::GuestStack;
use crate::vm::VmError;

/// The part of a guest thread another thread may read, and the whole of it.
pub struct GuestThread {
    /// Address of the innermost live [`RegisterContext`] on this thread, or zero when
    /// the thread is not inside guest code.
    context: AtomicU64,
    /// The guest stack, which is *not* the host stack this thread was created
    /// with - see [`crate::shims::machthread`] for why that matters to anything
    /// asking `pthread_get_stackaddr_np`.
    stack: Range<u64>,
}

impl GuestThread {
    /// The guest's registers, as far as this runtime can state them.
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
/// The one race left is a *miss*.
static TABLE: [Slot; SLOTS] = [const {
    Slot {
        port: AtomicU32::new(0),
        thread: AtomicPtr::new(core::ptr::null_mut()),
    }
}; SLOTS];

const SLOTS: usize = 512;

/// Publish `thread` under `port`. The `Arc` is leaked into the table.
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
    // Out of slots. Not fatal and not silent.
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
pub fn mach_port() -> u32 {
    // SAFETY: `pthread_self` is always valid on a running thread and
    // `pthread_mach_thread_np` reads the port name out of it.
    unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
}

/// The guest thread registered under `port`, if it is one.
pub fn by_port(port: u32) -> Option<&'static GuestThread> {
    if port == 0 {
        return None;
    }
    for slot in &TABLE {
        if slot.port.load(Ordering::Acquire) == port {
            let raw = slot.thread.load(Ordering::Acquire);
            // SAFETY: a non-null pointer in the table came from
            // `Arc::into_raw` in `register` and is never freed - see the note
            // there on why it is leaked.
            return (!raw.is_null()).then(|| unsafe { &*raw });
        }
    }
    None
}

/// Call `f` with the Mach port of every registered guest thread.
///
/// A thread that ends mid-walk is reported and then fails to suspend, which is the
/// ordinary race and is why the sampler treats a failed suspend as a miss rather than
/// an error.
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
    /// How many host->guest callbacks are live on this thread.
    pub(crate) depth: Cell<usize>,
    /// An error raised inside a host->guest callback, which cannot be returned through
    /// the host function that caused it.
    pub(crate) pending: RefCell<Option<VmError>>,
    /// Which entry into guest code is live on this thread.
    entry: Cell<u64>,
    /// Whether the call being serviced set `rip` itself instead of returning.
    redirected: Cell<bool>,
    /// Whether the import being serviced was reached by a **jump** rather than a call.
    entered_by_jump: Cell<bool>,
    /// The exception being unwound on this thread, if any.
    unwinding: Cell<Option<crate::unwind::InFlight>>,
    /// Exception headers whose `catch` blocks are running, innermost last.
    caught: RefCell<Vec<u64>>,
    /// Translations this thread has already entered, by the PC it entered them
    /// at, plus the two tables lifted code reads. See [`CodeWindow`].
    code: CodeWindow,
    /// Where this thread's indirect calls went last time. See
    /// [`crate::dispatch::IndirectCache`].
    indirect: crate::dispatch::IndirectCache,
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
            indirect: crate::dispatch::IndirectCache::new(),
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

    /// This thread's memory of where its indirect calls went.
    pub(crate) fn indirect(&self) -> &crate::dispatch::IndirectCache {
        &self.indirect
    }

    /// The entry into guest code currently live on this thread, or zero when
    /// none is.
    pub(crate) fn entry(&self) -> u64 {
        self.entry.get()
    }

    /// Begin a new entry, returning the previous one to be restored after it.
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
    pub fn main_args(&self) -> (u64, u64, u64, u64) {
        self.stack.main_args()
    }

    /// The `rsp` an `LC_UNIXTHREAD` entry point is entered with, for the same stack and
    /// with the same restriction: only the thread that started the program has one.
    pub fn kernel_sp(&self) -> Option<u64> {
        self.stack.kernel_sp()
    }

    /// The address range this thread's guest stack occupies.
    ///
    /// Two loads and no search, which is what makes it worth asking before
    /// [`crate::vm::GuestView`] is asked the same question the general way - see
    /// `Vm::enter_guest_callee`.
    pub fn stack_range(&self) -> Range<u64> {
        self.stack.range()
    }

    /// The guest stack pointer as of the last thing that moved it.
    ///
    /// For [`crate::fault`], which asks on a thread that is about to die and wants the
    /// answer without a borrow it could not take.
    pub fn live_rsp(&self) -> u64 {
        match self.published.context() {
            Some(ctx) => ctx.rsp,
            None => self.rsp.get(),
        }
    }

    /// The register context of the entry below this one, if there is one.
    ///
    /// Published by [`publish_context`](Self::publish_context) for the duration of an
    /// entry, so during the *setup* of a nested one it is still the outer entry's -
    /// which is what
    /// [`ThreadLinks::resume_call_return`](microlift::runtime::ThreadLinks::resume_call_return)
    /// wants, and the only reader that needs it before the swap.
    pub(crate) fn published_context(&self) -> Option<&RegisterContext> {
        self.published.context()
    }
}

/// One thread's window onto the code cache, and the two tables lifted code reads.
///
/// Never invalidated, for the reason the cache itself is never invalidated.
pub(crate) struct CodeWindow {
    slots: RefCell<Box<[Option<(u64, std::sync::Arc<crate::cache::Translation>)>]>>,
    /// The two tables lifted code reads: the rows a probe hashes into, and the
    /// `<return address, link cell>` frames a `call` pushes and a `RET` pops.
    ///
    /// Microlift's own type rather than a pair of bare arrays, so that the slot a
    /// probe computes and the slot a publish writes are the same function called
    /// twice, and so that "which pointers a context owes a region" is stated in one
    /// place instead of here as well. See [`ThreadLinks`].
    links: UnsafeCell<ThreadLinks>,
    /// The geometry `slot` uses, copied out of [`link_config`] once.
    ///
    /// The *same value* `links` was built with, so the two cannot drift; what it
    /// avoids is the static read, which is not free and which `slot` performed twice
    /// per run-loop entry and once per published row - 2% of Roblox's running time.
    config: BlockLinkConfig,
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
    #[dynamic]
    static CONFIG: BlockLinkConfig = {
        let config = BlockLinkConfig::new(WINDOW as u32);
        // Which of the two shapes of linking this process runs.
        match crate::jit::pinned_link_abi() {
            Some(abi) => config.pinned(abi),
            None => config,
        }
    };
    *CONFIG
}

impl CodeWindow {
    fn new() -> Self {
        let config = link_config();
        CodeWindow {
            slots: RefCell::new(vec![None; WINDOW].into_boxed_slice()),
            links: UnsafeCell::new(ThreadLinks::new(config, crate::jit::call_return_stack())),
            config,
        }
    }

    /// The slot a PC maps to.
    ///
    /// Microlift's own `slot`, so this and the shift-and-mask baked into every region
    /// cannot drift apart - called on the copy this window was built with rather than
    /// on the static, which is the same arithmetic without the read.
    fn slot(&self, pc: u64) -> usize {
        self.config.slot(pc)
    }

    /// The address of the block-link table for this thread, as `ctx.link_table`
    /// wants it.
    pub(crate) fn link_table(&self) -> *mut BlockLinkEntry {
        // SAFETY: taking the address of the allocation, not reading it. The
        // table is owned by this `CodeWindow` and never reallocated, so the
        // address is stable for the life of the thread - which is what lets it
        // be handed to compiled code at all.
        unsafe { (*self.links.get()).link_table_ptr() }
    }

    /// Point `ctx` at both tables, so it may enter lifted code on this thread.
    ///
    /// One call rather than a store per field, and microlift's own, so a field
    /// added to the pair is not a store this crate has to remember to add.
    pub(crate) fn attach(&self, ctx: &mut RegisterContext) {
        // SAFETY: as `link_table`.
        unsafe { (*self.links.get()).attach(ctx) };
    }

    pub(crate) fn relink(&self, pc: u64, translation: &std::sync::Arc<crate::cache::Translation>) {
        // SAFETY: as `put` - the only writer, on the thread that owns this window,
        // in the run loop rather than inside a region.
        unsafe {
            let links = &mut *self.links.get();
            if links.table().get(pc).is_some() {
                return;
            }
            if links.publish_at(translation, pc) {
                crate::stats::record_relink();
            }
        }
    }

    /// The translation this thread last entered at `pc`, if it still holds one.
    pub(crate) fn get(&self, pc: u64) -> Option<std::sync::Arc<crate::cache::Translation>> {
        match self.slots.borrow().get(self.slot(pc)) {
            Some(Some((at, translation))) if *at == pc => Some(std::sync::Arc::clone(translation)),
            _ => None,
        }
    }

    /// Remember that `translation` may be entered at `pc`, and make it linkable from
    /// this thread at every address it can be resumed at.
    ///
    /// Not just at `pc`: publishing the whole set is what lets a link to an address
    /// this thread has never entered hit anyway, and it is the protocol microlift's
    /// own `ThreadRuntime::publish` states - a row must exist at exactly the resume
    /// points and nowhere else, since an address that is not one either runs the
    /// region from the top or exits having made no progress.
    ///
    /// A row for an address whose slot is taken evicts the row that was there - which
    /// costs a lookup, and which is why publishing the extra rows is not done
    /// unconditionally any more. See [`crate::jit::link_publish`] for the measurement
    /// that changed the default; `ROSETTA3_LINK_PUBLISH=all` restores it.
    pub(crate) fn put(&self, pc: u64, translation: &std::sync::Arc<crate::cache::Translation>) {
        let slot = self.slot(pc);
        // `try_borrow_mut` rather than `borrow_mut`.
        if let Ok(mut slots) = self.slots.try_borrow_mut() {
            slots[slot] = Some((pc, std::sync::Arc::clone(translation)));
            // The rows go in with the `Arc`, and only with it: a row names code
            // that must stay mapped, and the cache is what keeps it so - see
            // `Published::retained`. The borrow above is also what serialises this
            // against a nested run loop writing the same slot.
            //
            // SAFETY: the only writer, on the thread that owns this window, at a
            // point where this thread is in the run loop rather than inside a
            // region. See the field's own note.
            let rows = unsafe {
                match crate::jit::link_publish() {
                    crate::jit::LinkPublish::Entry => {
                        usize::from((*self.links.get()).publish_at(translation, pc))
                    }
                    crate::jit::LinkPublish::All => {
                        (*self.links.get()).publish(translation);
                        translation.resume_points.len()
                    }
                }
            };
            crate::stats::record_published(rows);
        }
    }
}

/// The key every thread's [`ThreadState`] hangs off.
///
/// A raw pthread key, not `thread_local!`. Rust's is thread-specific data with a
/// "destroyed" flag in front of it, and macOS trips that flag on the main thread
/// *inside* `exit` - precisely when the guest's `atexit` handlers run and re-enter the
/// JIT. A raw key has no such flag: the value stays readable until the thread ends.
/// The destructor frees a *secondary* thread's stack, so a guest that spawns and joins
/// in a loop does not accumulate an 8 MiB reservation per thread.
#[dynamic]
static KEY: libc::pthread_key_t = {
    let mut key: libc::pthread_key_t = 0;
    // SAFETY: `key` is a live local of the right type, and `release` matches
    // the destructor signature libpthread calls it with.
    let rc = unsafe { libc::pthread_key_create(&mut key, Some(release)) };
    assert_eq!(
        rc, 0,
        "pthread_key_create failed; the runtime cannot run a guest thread"
    );
    key
};

#[inline(always)]
fn key() -> libc::pthread_key_t {
    *KEY
}

/// Free a finished thread's state. Never called on the main thread.
unsafe extern "C" fn release(value: *mut c_void) {
    if value.is_null() {
        return;
    }
    // Unregistered first: after this the port may be reused by a thread created
    // later, and an entry naming a freed stack is worse than no entry.
    unregister(mach_port());
    // SAFETY: the only thing ever stored under this key is a `Box::into_raw` of
    // a `ThreadState`, and libpthread calls this once, with the key cleared, so
    // the box is not reachable from anywhere else.
    drop(unsafe { Box::from_raw(value as *mut ThreadState) });
}

/// This thread's state, if it has entered guest code before.
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
    // another thread finds it.
    register(mach_port(), Arc::clone(&state.published));
    clear_errno();
    state
}

/// Hand this thread the `errno` a thread that has just started really has.
#[inline(always)]
pub(crate) fn clear_errno() {
    // SAFETY: `__error()` returns this thread's own `errno` slot, which is
    // live for as long as the thread is.
    unsafe { *libc::__error() = 0 };
}

/// The guest's `errno`, put back exactly as it was.
///
/// **Not defensive tidying - a measured bug, twice.** `dlopen` of a framework this
/// system does not have sets `ENOENT`, and [`crate::gui`]'s `sys` does exactly that on
/// the main thread between [`clear_errno`] and the guest's `main`.
pub(crate) struct Errno(i32);

impl Errno {
    pub(crate) fn held() -> Errno {
        // SAFETY: `__error()` returns this thread's own `errno` slot, live for
        // as long as the thread is.
        Errno(unsafe { *libc::__error() })
    }
}

impl Drop for Errno {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe { *libc::__error() = self.0 };
    }
}

/// The base of this host thread's thread-specific data array - the value a guest's
/// `%gs` segment base must hold.
///
/// On arm64 Darwin the array's address is `TPIDRRO_EL0` with its low three bits masked
/// off; the register's spare bits carry the CPU number, which is not part of the
/// address.
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

/// On x86-64 Darwin the array is what `%gs` already points at, and `tsd[0]` holds the
/// base itself - which is how `pthread_self` is a single `mov` from `%gs:0`.
///
/// Read out of the segment rather than asked for by `pthread_self()`, because the two
/// are the same value and this one cannot call into the guest's shims.
#[cfg(target_arch = "x86_64")]
pub fn tsd_base() -> u64 {
    let base: u64;
    // SAFETY: every Darwin thread has `%gs` pointing at its own TSD array, and
    // slot zero of that array is live for as long as the thread is. One load,
    // no writes.
    unsafe {
        std::arch::asm!("mov {}, gs:0", out(reg) base, options(nostack, preserves_flags))
    };
    base
}

/// A host with neither `TPIDRRO_EL0` nor a Darwin `%gs`, which this runtime has not got.
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn tsd_base() -> u64 {
    0
}

/// Stack for a guest thread that asked for no particular one.
///
/// The main thread's size, which is the choice that cannot surprise a guest that never
/// said.
pub const THREAD_STACK: usize = crate::stack::DEFAULT_STACK;

/// Where a guest thread's frames are to live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackRequest {
    /// Reserve [`THREAD_STACK`] bytes, the main thread's size.
    Default,
    /// Reserve `bytes`, rounded up to a page. What the guest asked for through
    /// `pthread_attr_setstacksize`.
    Sized(usize),
    /// Use memory the guest already owns: `top` is the highest address, exclusive,
    /// matching `pthread_attr_getstackaddr`. See [`GuestStack::supplied`].
    Supplied { top: u64, size: u64 },
}

impl StackRequest {
    /// Build the stack this asks for.
    fn build(self) -> Result<GuestStack, VmError> {
        match self {
            StackRequest::Default => GuestStack::bare(THREAD_STACK),
            // The guard on top of what was asked for, because what the guest
            // asked for is the part it may use. Paired with the subtraction in
            // [`GuestStack::bare`], so the two have to name the same quantity.
            StackRequest::Sized(bytes) => GuestStack::bare(bytes + crate::stack::guard()),
            StackRequest::Supplied { top, size } => Ok(GuestStack::supplied(top, size)),
        }
    }
}

/// Make this host thread a guest thread with the stack `request` describes, replacing
/// any state it already had.
pub fn adopt(request: StackRequest) -> Result<&'static ThreadState, VmError> {
    Ok(install(ThreadState::new(request.build()?)))
}

/// What a new guest thread is to run, and on what.
pub struct GuestStart {
    /// The **thunk** for the guest's start routine, not the guest address.
    pub entry: u64,
    /// The `void *` the guest passed, untranslated. Only guest code ever
    /// dereferences it, and the two sides share an address space.
    pub arg: u64,
    /// Where the guest's frames go.
    pub stack: StackRequest,
}

/// Start a host thread that runs `start` as a guest thread.
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

/// What libpthread calls on the new thread: install the guest state, then enter the
/// guest.
extern "C" fn entry(raw: *mut c_void) -> *mut c_void {
    // Unboxed before the guest runs, and the fields copied out, because the
    // guest's start routine may call `pthread_exit` - which ends this thread
    // from underneath these Rust frames without unwinding them. A `Box` still
    // live at that moment is a leak once per thread that exits that way.
    // SAFETY: the pointer came from `Box::into_raw` in `spawn` and is handed to
    // exactly one thread, which is this one.
    let GuestStart { entry, arg, stack } = *unsafe { Box::from_raw(raw as *mut GuestStart) };

    if let Err(e) = adopt(stack) {
        // No error channel.
        eprintln!("rosetta3: a guest thread could not be given a stack: {e}");
        return core::ptr::null_mut();
    }

    // SAFETY: `entry` is a thunk this runtime compiled - an arm64 function
    // taking the guest's six integer argument registers and eight vector ones,
    // of which a start routine reads the first. It stays mapped for the VM's
    // lifetime, which outlives every thread that can reach it. See
    // [`crate::thunk`].
    let f: extern "C" fn(u64) -> u64 = unsafe { core::mem::transmute::<u64, _>(entry) };
    f(arg) as *mut c_void
}

/// This thread's state, creating a default one if it has none.
pub fn ensure() -> Result<&'static ThreadState, VmError> {
    if let Some(existing) = current() {
        return Ok(existing);
    }
    adopt(StackRequest::Default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard restores, rather than clearing.
    #[test]
    fn a_held_errno_comes_back_exactly_as_it_was() {
        // SAFETY: this thread's own slot, in a test that owns the thread.
        unsafe { *libc::__error() = libc::EPIPE };
        {
            let _held = Errno::held();
            unsafe { *libc::__error() = libc::ENOENT };
        }
        assert_eq!(unsafe { *libc::__error() }, libc::EPIPE);

        // Including zero, which is the value the guest sees for most of a run.
        unsafe { *libc::__error() = 0 };
        {
            let _held = Errno::held();
            unsafe { *libc::__error() = libc::ENOENT };
        }
        assert_eq!(unsafe { *libc::__error() }, 0);
    }
}
