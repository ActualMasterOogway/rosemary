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

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ops::Range;
use std::sync::OnceLock;

use crate::stack::GuestStack;
use crate::vm::VmError;

/// Everything about executing guest code that belongs to one host thread.
pub struct ThreadState {
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
}

impl ThreadState {
    /// Wrap a stack this thread will run the guest on.
    pub fn new(stack: GuestStack) -> Self {
        let rsp = stack.initial_rsp();
        ThreadState {
            stack,
            rsp: Cell::new(rsp),
            depth: Cell::new(0),
            pending: RefCell::new(None),
            entry: Cell::new(0),
            redirected: Cell::new(false),
            entered_by_jump: Cell::new(false),
            unwinding: Cell::new(None),
            caught: RefCell::new(Vec::new()),
        }
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
        self.entry.set(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
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
}

/// The key every thread's [`ThreadState`] hangs off.
static KEY: OnceLock<libc::pthread_key_t> = OnceLock::new();

fn key() -> libc::pthread_key_t {
    *KEY.get_or_init(|| {
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: `key` is a live local of the right type, and `release` matches
        // the destructor signature libpthread calls it with.
        let rc = unsafe { libc::pthread_key_create(&mut key, Some(release)) };
        assert_eq!(rc, 0, "pthread_key_create failed; the runtime cannot run a guest thread");
        key
    })
}

/// Free a finished thread's state. Never called on the main thread.
unsafe extern "C" fn release(value: *mut c_void) {
    if value.is_null() {
        return;
    }
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
    unsafe { &*raw }
}

/// Stack for a thread the guest spawned.
///
/// The guest's own `pthread_attr_setstacksize` is *not* consulted, and the
/// reason is that it is not about this stack: the attribute sizes the stack the
/// host gives the host thread, which the host duly honours and on which none of
/// the guest's frames live. A guest that asks for a small stack still gets a
/// small host stack; what it gets here is room for its own frames, and matching
/// the main thread is the choice that cannot surprise it. Reservations are lazy,
/// so the cost is address space.
pub const THREAD_STACK: usize = crate::stack::DEFAULT_STACK;

/// This thread's state, creating one with a bare guest stack if it has none.
///
/// The path a spawned guest thread takes: the host calls a thunk, the thunk
/// enters the VM, and the VM finds no stack to enter on.
pub fn ensure() -> Result<&'static ThreadState, VmError> {
    if let Some(existing) = current() {
        return Ok(existing);
    }
    Ok(install(ThreadState::new(GuestStack::bare(THREAD_STACK)?)))
}
