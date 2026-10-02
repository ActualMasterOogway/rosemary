/// A saved `sa_mask`, in a form a fault handler can stash without allocating.
///
/// Held as bytes rather than as a `sigset_t`, for two reasons that come to the same
/// thing: a `sigset_t` is a 32-bit word on Darwin and a 128-byte structure under
/// glibc, so it neither fits in an atomic nor has a `const` empty value under its own
/// name. All-zero is the empty set in both representations, which is what makes
/// [`Mask::empty`] a `const fn` and therefore a `static` initialiser.
#[derive(Clone, Copy)]
pub struct Mask([u8; core::mem::size_of::<libc::sigset_t>()]);

impl Mask {
    /// The empty mask, which is what a disposition nobody saved yet holds.
    pub const fn empty() -> Mask {
        Mask([0; core::mem::size_of::<libc::sigset_t>()])
    }

    /// The mask `action` carries.
    pub fn of(action: &libc::sigaction) -> Mask {
        let mut bytes = [0u8; core::mem::size_of::<libc::sigset_t>()];
        // SAFETY: `sa_mask` is a `sigset_t`, which is a plain bit set with no interior
        // pointers, so its bytes are its value. Source and destination are the same
        // size by construction.
        unsafe {
            core::ptr::copy_nonoverlapping(
                &action.sa_mask as *const libc::sigset_t as *const u8,
                bytes.as_mut_ptr(),
                bytes.len(),
            );
        }
        Mask(bytes)
    }

    /// Put this mask into `action`.
    pub fn apply(self, action: &mut libc::sigaction) {
        // SAFETY: as above, in the other direction.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.0.as_ptr(),
                &mut action.sa_mask as *mut libc::sigset_t as *mut u8,
                self.0.len(),
            );
        }
    }
}

/// A [`Mask`] in a `static`, written before the handler that reads it can run.
///
/// The shape a fault handler needs and an atomic cannot provide. Sound for the one use
/// it has: [`StaticMask::set`] is called from `install`, which runs once during
/// start-up, and every later access is a read from a handler.
pub struct StaticMask(core::cell::UnsafeCell<Mask>);

// SAFETY: written only by `set`, whose contract is that it is called before anything
// can read it, and read-only thereafter. `Mask` is `Copy` bytes with no owner.
unsafe impl Sync for StaticMask {}

impl StaticMask {
    pub const fn empty() -> StaticMask {
        StaticMask(core::cell::UnsafeCell::new(Mask::empty()))
    }

    /// Record `mask`.
    ///
    /// # Safety
    ///
    /// Called before any handler that reads this can be entered, and not concurrently
    /// with another `set` on the same cell.
    pub unsafe fn set(&self, mask: Mask) {
        // SAFETY: the caller guarantees no concurrent access.
        unsafe { *self.0.get() = mask };
    }

    /// What [`StaticMask::set`] recorded, or the empty mask if nothing did.
    pub fn get(&self) -> Mask {
        // SAFETY: the cell holds a valid `Mask` from its initialiser onwards, and
        // `Mask` is `Copy`.
        unsafe { *self.0.get() }
    }
}

/// Darwin's signal numbers, from `<sys/signal.h>`.
///
/// Only the first six and a handful after agree with Linux. The rest are a
/// re-shuffling, and three of the disagreements are the ones a program actually uses:
/// `SIGUSR1` is 30 on Darwin and 10 on Linux, `SIGCHLD` is 20 and 17, and `SIGBUS` is
/// 10 and 7 - so a guest sending itself `SIGUSR1` through an untranslated `pthread_kill`
/// sends `SIGPWR`, and one raising `SIGBUS` raises `SIGUSR1`.
pub mod signum {
    pub const SIGHUP: i32 = 1;
    pub const SIGINT: i32 = 2;
    pub const SIGQUIT: i32 = 3;
    pub const SIGILL: i32 = 4;
    pub const SIGTRAP: i32 = 5;
    pub const SIGABRT: i32 = 6;
    /// Darwin-only; Linux has no emulator trap.
    pub const SIGEMT: i32 = 7;
    pub const SIGFPE: i32 = 8;
    pub const SIGKILL: i32 = 9;
    pub const SIGBUS: i32 = 10;
    pub const SIGSEGV: i32 = 11;
    pub const SIGSYS: i32 = 12;
    pub const SIGPIPE: i32 = 13;
    pub const SIGALRM: i32 = 14;
    pub const SIGTERM: i32 = 15;
    pub const SIGURG: i32 = 16;
    pub const SIGSTOP: i32 = 17;
    pub const SIGTSTP: i32 = 18;
    pub const SIGCONT: i32 = 19;
    pub const SIGCHLD: i32 = 20;
    pub const SIGTTIN: i32 = 21;
    pub const SIGTTOU: i32 = 22;
    pub const SIGIO: i32 = 23;
    pub const SIGXCPU: i32 = 24;
    pub const SIGXFSZ: i32 = 25;
    pub const SIGVTALRM: i32 = 26;
    pub const SIGPROF: i32 = 27;
    pub const SIGWINCH: i32 = 28;
    /// Darwin-only; the status-request signal a BSD terminal sends.
    pub const SIGINFO: i32 = 29;
    pub const SIGUSR1: i32 = 30;
    pub const SIGUSR2: i32 = 31;
}

/// Darwin's signal number as this host's, or `None` for one the host has not got.
///
/// Identity on macOS. `SIGEMT` and `SIGINFO` have no Linux equivalent and are refused
/// rather than mapped onto a neighbour, because delivering the wrong signal is worse
/// than delivering none: a handler installed for one runs for the other.
pub fn signal_to_host(darwin: i32) -> Option<libc::c_int> {
    #[cfg(target_os = "macos")]
    {
        (1..=31).contains(&darwin).then_some(darwin)
    }
    #[cfg(not(target_os = "macos"))]
    {
        use signum as d;
        Some(match darwin {
            d::SIGHUP => libc::SIGHUP,
            d::SIGINT => libc::SIGINT,
            d::SIGQUIT => libc::SIGQUIT,
            d::SIGILL => libc::SIGILL,
            d::SIGTRAP => libc::SIGTRAP,
            d::SIGABRT => libc::SIGABRT,
            d::SIGFPE => libc::SIGFPE,
            d::SIGKILL => libc::SIGKILL,
            d::SIGBUS => libc::SIGBUS,
            d::SIGSEGV => libc::SIGSEGV,
            d::SIGSYS => libc::SIGSYS,
            d::SIGPIPE => libc::SIGPIPE,
            d::SIGALRM => libc::SIGALRM,
            d::SIGTERM => libc::SIGTERM,
            d::SIGURG => libc::SIGURG,
            d::SIGSTOP => libc::SIGSTOP,
            d::SIGTSTP => libc::SIGTSTP,
            d::SIGCONT => libc::SIGCONT,
            d::SIGCHLD => libc::SIGCHLD,
            d::SIGTTIN => libc::SIGTTIN,
            d::SIGTTOU => libc::SIGTTOU,
            d::SIGIO => libc::SIGIO,
            d::SIGXCPU => libc::SIGXCPU,
            d::SIGXFSZ => libc::SIGXFSZ,
            d::SIGVTALRM => libc::SIGVTALRM,
            d::SIGPROF => libc::SIGPROF,
            d::SIGWINCH => libc::SIGWINCH,
            d::SIGUSR1 => libc::SIGUSR1,
            d::SIGUSR2 => libc::SIGUSR2,
            // `SIGEMT` and `SIGINFO` are Darwin's own, and 0 is "test only".
            _ => return None,
        })
    }
}

/// The `SA_*` flags, whose *values* differ even where their meanings do not.
///
/// Darwin numbers them from one upward; Linux starts at the other end for the
/// four that predate POSIX and uses the low bits for the `SIGCHLD` pair. A guest
/// that asks for `SA_RESTART` and gets Linux's `SA_NOCLDSTOP` has installed a
/// handler with the wrong semantics and no diagnostic.
mod saflag {
    pub const DARWIN: &[(u32, &str)] = &[
        (0x0001, "ONSTACK"),
        (0x0002, "RESTART"),
        (0x0004, "RESETHAND"),
        (0x0008, "NOCLDSTOP"),
        (0x0010, "NODEFER"),
        (0x0020, "NOCLDWAIT"),
        (0x0040, "SIGINFO"),
    ];

    /// The host bit for each name above, in the same order.
    pub fn host(name: &str) -> Option<libc::c_int> {
        Some(match name {
            "ONSTACK" => libc::SA_ONSTACK,
            "RESTART" => libc::SA_RESTART,
            "RESETHAND" => libc::SA_RESETHAND,
            "NOCLDSTOP" => libc::SA_NOCLDSTOP,
            "NODEFER" => libc::SA_NODEFER,
            "NOCLDWAIT" => libc::SA_NOCLDWAIT,
            "SIGINFO" => libc::SA_SIGINFO,
            _ => return None,
        })
    }
}

/// Darwin's `sa_flags` as this host's. A bit with no counterpart is dropped.
pub fn flags_to_host(darwin: u32) -> libc::c_int {
    let mut out = 0;
    for (bit, name) in saflag::DARWIN {
        if darwin & bit != 0
            && let Some(host) = saflag::host(name)
        {
            out |= host;
        }
    }
    out
}

/// This host's `sa_flags` as Darwin's.
pub fn flags_from_host(host: libc::c_int) -> u32 {
    let mut out = 0;
    for (bit, name) in saflag::DARWIN {
        if saflag::host(name).is_some_and(|h| host & h != 0) {
            out |= bit;
        }
    }
    out
}

/// Signals whose host disposition belongs to the runtime, not to the guest.
///
/// A bit per Darwin signal number. Empty unless something calls [`retain`].
static RETAINED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// What the guest asked for on a retained signal.
///
/// A caller that installs a handler and reads its own installation back has to
/// see what it installed: finding `SIG_DFL` there is how a crash reporter
/// concludes it failed to start. Same reasoning as
/// `mach::task_get_exception_ports`.
static PRETENDED: [Pretended; 32] = [const { Pretended::new() }; 32];

struct Pretended {
    handler: core::sync::atomic::AtomicU64,
    mask: core::sync::atomic::AtomicU32,
    flags: core::sync::atomic::AtomicU32,
}

impl Pretended {
    const fn new() -> Pretended {
        use core::sync::atomic::{AtomicU32, AtomicU64};
        Pretended {
            handler: AtomicU64::new(0),
            mask: AtomicU32::new(0),
            flags: AtomicU32::new(0),
        }
    }

    fn get(&self) -> Disposition {
        use core::sync::atomic::Ordering::Relaxed;
        Disposition {
            handler: self.handler.load(Relaxed),
            mask: self.mask.load(Relaxed),
            flags: self.flags.load(Relaxed),
        }
    }

    fn set(&self, value: Disposition) {
        use core::sync::atomic::Ordering::Relaxed;
        self.handler.store(value.handler, Relaxed);
        self.mask.store(value.mask, Relaxed);
        self.flags.store(value.flags, Relaxed);
    }
}

/// Keep the host's disposition for `darwin_signum`, whatever the guest asks for.
///
/// The guest's `sigaction` and `signal` then succeed, change nothing, and report
/// back the disposition the guest last asked for. Meant for the fault signals
/// while the runtime's own reporter is what is being read: a guest that installs
/// a `SIGSEGV` handler takes every diagnostic with it, and an intermittent
/// corruption then dies with no output at all.
///
/// Not the default. A program is entitled to handle its own faults, and one that
/// recovers from `SIGBUS` by design would hang or loop instead.
pub fn retain(darwin_signum: i32) {
    if !(1..=31).contains(&darwin_signum) {
        return;
    }
    RETAINED.fetch_or(
        1u32 << (darwin_signum - 1),
        core::sync::atomic::Ordering::Relaxed,
    );
}

/// Whether [`retain`] named this signal.
pub fn retained(darwin_signum: i32) -> bool {
    (1..=31).contains(&darwin_signum)
        && RETAINED.load(core::sync::atomic::Ordering::Relaxed) & (1u32 << (darwin_signum - 1)) != 0
}

/// Whether the alternate signal stack belongs to the runtime too.
///
/// A thread has one alternate stack and every `SA_ONSTACK` handler runs on it,
/// so a guest that installs a small one - Crashpad installs `MINSIGSTKSZ`, two
/// kilobytes - is what the runtime's fault reporter then gets, and a report
/// longer than that overflows it and dies half-written. Retained beside the
/// handlers themselves, by the same lever and for the same reason.
static RETAIN_ALTSTACK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// What this thread's guest asked for on the retained alternate stack.
// Alternate stacks are thread-local kernel state. Keeping this process-global
// would make one guest thread read (and overwrite) another thread's stack.
thread_local! {
    static PRETENDED_ALTSTACK: core::cell::Cell<(usize, usize, i32)> =
        const { core::cell::Cell::new((0, 0, libc::SS_DISABLE)) };
}

/// Keep this thread's alternate signal stack, whatever the guest asks for.
pub fn retain_altstack() {
    RETAIN_ALTSTACK.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Whether [`retain_altstack`] was called.
pub fn altstack_retained() -> bool {
    RETAIN_ALTSTACK.load(core::sync::atomic::Ordering::Relaxed)
}

/// The alternate stack the guest last asked for, for a caller reading it back.
pub fn pretended_altstack() -> libc::stack_t {
    PRETENDED_ALTSTACK.with(|held| {
        let (sp, size, flags) = held.get();
        libc::stack_t {
            ss_sp: sp as *mut libc::c_void,
            ss_size: size,
            ss_flags: flags,
        }
    })
}

/// Record what the guest asked for without acting on it.
pub fn set_pretended_altstack(stack: &libc::stack_t) {
    PRETENDED_ALTSTACK.with(|held| {
        held.set((stack.ss_sp as usize, stack.ss_size, stack.ss_flags));
    });
}

/// A signal disposition in Darwin's shape: a handler, a 32-bit mask, and flags.
///
/// Sixteen bytes on both of Darwin's architectures, which is what the guest
/// reads and writes. The host's own `struct sigaction` is a different size and a
/// different order under glibc - 152 bytes, with a 128-byte `sigset_t` - so this
/// is a value to convert, never a structure to pass along.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Disposition {
    pub handler: u64,
    pub mask: u32,
    pub flags: u32,
}

/// The dispositions that are values rather than addresses: `SIG_DFL`, `SIG_IGN`
/// and `SIG_ERR`. Their numbers agree on both hosts.
fn is_sentinel(handler: u64) -> bool {
    handler == 0 || handler == 1 || handler as i64 == -1
}

/// One more than the highest host signal number a handler can be installed for.
const HOST_SIGNALS: usize = 65;

/// The guest's handler for each host signal, entered through [`deliver`].
///
/// Keyed by the *host's* number because that is what the kernel hands
/// [`deliver`]. Written before the host disposition is changed, so a signal
/// that arrives in between finds the handler it is about to be sent to.
static GUEST_HANDLERS: [core::sync::atomic::AtomicU64; HOST_SIGNALS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; HOST_SIGNALS];

/// Whether each handler above takes `SA_SIGINFO`'s three arguments.
static GUEST_SIGINFO: [core::sync::atomic::AtomicBool; HOST_SIGNALS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; HOST_SIGNALS];

/// Darwin's `SA_SIGINFO`.
const DARWIN_SA_SIGINFO: u32 = 0x0040;

/// What the kernel enters for every guest handler.
///
/// The guest was compiled against Darwin's numbering, and the kernel speaks
/// this host's: a `SIGUSR1` handler entered with 10 is being told `SIGBUS`,
/// and a shell's `trap` records the wrong signal and never runs. So the number
/// is translated here, and the guest's handler is called with Darwin's.
///
/// Async-signal-safe: two atomic loads and a table walk.
extern "C" fn deliver(host: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    let Some(slot) = usize::try_from(host).ok().filter(|s| *s < HOST_SIGNALS) else {
        return;
    };
    let handler = GUEST_HANDLERS[slot].load(core::sync::atomic::Ordering::Acquire);
    if is_sentinel(handler) {
        return;
    }
    let darwin = signal_from_host(host).unwrap_or(host);
    if GUEST_SIGINFO[slot].load(core::sync::atomic::Ordering::Acquire) {
        // SAFETY: installed by [`sigaction`] as a three-argument handler; the
        // two pointers are the kernel's, passed on as they came.
        unsafe {
            let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                core::mem::transmute(handler as usize);
            handler(darwin, info, context);
        }
    } else {
        // SAFETY: installed by [`sigaction`] or [`signal`] as a one-argument
        // handler.
        unsafe {
            let handler: extern "C" fn(libc::c_int) = core::mem::transmute(handler as usize);
            handler(darwin);
        }
    }
}

fn delivers(host_handler: libc::sighandler_t) -> bool {
    host_handler == deliver as *const () as usize as libc::sighandler_t
}

/// `sigaction`, with Darwin's signal number, flags and mask on the outside and
/// the host's `struct sigaction` on the inside.
///
/// Returns `-1` with `errno` set to `EINVAL` for a signal this host has not got,
/// rather than acting on a neighbouring one.
///
/// # Safety
///
/// `act.handler`, when it is not a sentinel, must be a function this process can
/// call as a signal handler - which for a guest's handler means a trampoline and
/// not the guest's own address.
pub unsafe fn sigaction(
    darwin_signum: i32,
    act: Option<Disposition>,
    old: Option<&mut Disposition>,
) -> libc::c_int {
    let Some(signum) = signal_to_host(darwin_signum) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };

    if retained(darwin_signum) {
        let previous = PRETENDED[darwin_signum as usize].get();
        if let Some(act) = act {
            PRETENDED[darwin_signum as usize].set(act);
        }
        if let Some(old) = old {
            *old = previous;
        }
        return 0;
    }

    // SAFETY: zeroed is a valid `struct sigaction` on both hosts - `SIG_DFL` with
    // an empty mask and no flags - and every field is written below before use.
    let mut host_act: libc::sigaction = unsafe { core::mem::zeroed() };
    let mut host_old: libc::sigaction = unsafe { core::mem::zeroed() };

    // What the guest last installed through [`deliver`], for reporting `old`.
    let slot = signum as usize;
    let previous_handler = GUEST_HANDLERS[slot].load(core::sync::atomic::Ordering::Acquire);
    let previous_siginfo = GUEST_SIGINFO[slot].load(core::sync::atomic::Ordering::Acquire);

    if let Some(act) = act {
        host_act.sa_flags = flags_to_host(act.flags);
        if is_sentinel(act.handler) {
            host_act.sa_sigaction = act.handler as usize as libc::sighandler_t;
        } else {
            GUEST_SIGINFO[slot].store(
                act.flags & DARWIN_SA_SIGINFO != 0,
                core::sync::atomic::Ordering::Release,
            );
            GUEST_HANDLERS[slot].store(act.handler, core::sync::atomic::Ordering::Release);
            host_act.sa_sigaction = deliver as *const () as usize as libc::sighandler_t;
            host_act.sa_flags |= libc::SA_SIGINFO;
        }
        // SAFETY: `sa_mask` is a `sigset_t` being emptied before bits are added.
        unsafe { libc::sigemptyset(&mut host_act.sa_mask) };
        for darwin_bit in 1..=31 {
            if act.mask & (1u32 << (darwin_bit - 1)) == 0 {
                continue;
            }
            if let Some(host_bit) = signal_to_host(darwin_bit) {
                // SAFETY: a `sigset_t` this function owns and a valid signal.
                unsafe { libc::sigaddset(&mut host_act.sa_mask, host_bit) };
            }
        }
    }

    // SAFETY: two live, correctly shaped `struct sigaction`s of this host's own.
    let rc = unsafe {
        libc::sigaction(
            signum,
            act.map_or(core::ptr::null(), |_| &raw const host_act),
            match old {
                Some(_) => &raw mut host_old,
                None => core::ptr::null_mut(),
            },
        )
    };

    if rc == 0
        && let Some(old) = old
    {
        let through_us = delivers(host_old.sa_sigaction);
        let handler = match through_us {
            true => previous_handler,
            false => host_old.sa_sigaction as u64,
        };
        if through_us && !previous_siginfo {
            host_old.sa_flags &= !libc::SA_SIGINFO;
        }
        let mut mask = 0u32;
        for darwin_bit in 1..=31 {
            let Some(host_bit) = signal_to_host(darwin_bit) else {
                continue;
            };
            // SAFETY: a `sigset_t` the kernel filled in, and a valid signal.
            if unsafe { libc::sigismember(&host_old.sa_mask, host_bit) } == 1 {
                mask |= 1u32 << (darwin_bit - 1);
            }
        }
        *old = Disposition {
            handler,
            mask,
            flags: flags_from_host(host_old.sa_flags),
        };
        // A sentinel is a sentinel on both hosts, and must not be reported as an
        // address the caller might try to translate back.
        debug_assert!(is_sentinel(handler) || handler > 0xffff);
    }
    rc
}

/// `signal`, with Darwin's signal number on the outside.
///
/// `SIG_ERR` - which is `-1` on both hosts - for a signal this host has not got.
///
/// # Safety
///
/// As [`sigaction`]: `handler` must be a sentinel or a function this process can
/// call as a signal handler.
pub unsafe fn signal(darwin_signum: i32, handler: u64) -> u64 {
    let Some(signum) = signal_to_host(darwin_signum) else {
        crate::errno::set(libc::EINVAL);
        return u64::MAX;
    };
    if retained(darwin_signum) {
        let previous = PRETENDED[darwin_signum as usize].get();
        PRETENDED[darwin_signum as usize].set(Disposition {
            handler,
            ..previous
        });
        return previous.handler;
    }
    let slot = signum as usize;
    let installed = match is_sentinel(handler) {
        true => handler as usize as libc::sighandler_t,
        false => deliver as *const () as usize as libc::sighandler_t,
    };
    let previous = match is_sentinel(handler) {
        true => GUEST_HANDLERS[slot].load(core::sync::atomic::Ordering::Acquire),
        false => {
            GUEST_SIGINFO[slot].store(false, core::sync::atomic::Ordering::Release);
            GUEST_HANDLERS[slot].swap(handler, core::sync::atomic::Ordering::AcqRel)
        }
    };
    // SAFETY: the caller's obligation, or [`deliver`].
    let old = unsafe { libc::signal(signum, installed) };
    match delivers(old) {
        true => previous,
        false => old as u64,
    }
}

/// This host's signal number as Darwin's, for a report handed back to the guest.
pub fn signal_from_host(host: libc::c_int) -> Option<i32> {
    (1..=31).find(|d| signal_to_host(*d) == Some(host))
}

/// The faulting address `info` reports.
///
/// A field on Darwin and an accessor under Linux, which is a difference in the `libc`
/// crate's binding rather than in the structure.
///
/// # Safety
///
/// `info` is a live `siginfo_t`, as the kernel passes one to a handler.
pub unsafe fn fault_address(info: &libc::siginfo_t) -> u64 {
    #[cfg(target_os = "macos")]
    {
        info.si_addr as usize as u64
    }
    #[cfg(not(target_os = "macos"))]
    // SAFETY: the caller guarantees a live `siginfo_t`, and `si_addr` is the faulting
    // address for the two signals this is asked about.
    unsafe {
        info.si_addr() as usize as u64
    }
}

/// Darwin's `stack_t`, embedded in `ucontext_t`.
#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinStack {
    _sp: *mut libc::c_void,
    _size: usize,
    _flags: i32,
}

/// Darwin's `ucontext_t` through the machine-context pointer.
///
/// The SDK does not expose this structure through the `libc` crate, so keep the
/// small ABI declaration we need here instead of finding its fields by byte offset.
#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinUcontext {
    _onstack: i32,
    _sigmask: u32,
    _stack: DarwinStack,
    _link: *mut DarwinUcontext,
    _mcsize: usize,
    mcontext: *const DarwinMcontext,
}

/// Darwin's machine context through the saved thread state.
#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinMcontext {
    _exception_state: DarwinExceptionState,
    thread_state: DarwinThreadState,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[repr(C)]
struct DarwinExceptionState {
    _fault_address: u64,
    _syndrome: u32,
    _exception: u32,
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
#[repr(C)]
struct DarwinExceptionState {
    _trap: u16,
    _cpu: u16,
    _error: u32,
    _fault_address: u64,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[repr(C)]
struct DarwinThreadState {
    _x: [u64; 29],
    _fp: u64,
    _lr: u64,
    _sp: u64,
    pc: u64,
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
#[repr(C)]
struct DarwinThreadState {
    _registers_before_rip: [u64; 16],
    pc: u64,
}

#[cfg(target_os = "macos")]
const _: () = {
    assert!(core::mem::offset_of!(DarwinUcontext, mcontext) == 48);
    assert!(core::mem::offset_of!(DarwinMcontext, thread_state) == 16);
    #[cfg(target_arch = "aarch64")]
    assert!(core::mem::offset_of!(DarwinThreadState, pc) == 256);
    #[cfg(target_arch = "x86_64")]
    assert!(core::mem::offset_of!(DarwinThreadState, pc) == 128);
};

/// The program counter the kernel interrupted, out of the `ucontext_t` it passed.
///
/// `None` on any null, because a handler that dereferences its way to a second fault
/// has destroyed the evidence it was called to preserve.
///
/// # Safety
///
/// `context` is the third argument of an `SA_SIGINFO` handler.
pub unsafe fn interrupted_pc(context: *mut libc::c_void) -> Option<u64> {
    if context.is_null() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the kernel passes a live `ucontext_t` for the duration of the
        // handler, whose `uc_mcontext` is a pointer to a live thread state.
        unsafe {
            let context = context.cast::<DarwinUcontext>();
            let mcontext = core::ptr::read(&raw const (*context).mcontext);
            if mcontext.is_null() {
                return None;
            }
            Some(core::ptr::read(&raw const (*mcontext).thread_state.pc))
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    // SAFETY: the kernel passes a live `ucontext_t`, which the `libc` crate does
    // declare here.
    unsafe {
        Some((*(context as *const libc::ucontext_t)).uc_mcontext.pc)
    }
    #[cfg(all(target_os = "freebsd", target_arch = "aarch64"))]
    // SAFETY: as above. FreeBSD keeps the interrupted PC in `gp_elr`.
    unsafe {
        Some(
            (*(context as *const libc::ucontext_t))
                .uc_mcontext
                .mc_gpregs
                .gp_elr as u64,
        )
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    // SAFETY: as above. `REG_RIP` indexes the general-purpose register array.
    unsafe {
        Some(
            (*(context as *const libc::ucontext_t)).uc_mcontext.gregs[libc::REG_RIP as usize]
                as u64,
        )
    }
    #[cfg(all(target_os = "freebsd", target_arch = "x86_64"))]
    // SAFETY: as above. FreeBSD names the registers rather than indexing them.
    unsafe {
        Some((*(context as *const libc::ucontext_t)).uc_mcontext.mc_rip as u64)
    }
    #[cfg(all(
        not(target_os = "macos"),
        not(all(
            any(target_os = "linux", target_os = "freebsd"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))
    ))]
    {
        None
    }
}

// The entry points a guest binds to. Everything above is the translation the
// runtime shares with them; everything below is the Darwin ABI itself.
//
// Published only where the host's own would be wrong. On macOS these are
// libSystem's and `symbol` answers nothing, which is what leaves the guest
// bound to the real ones.
#[cfg(not(target_os = "macos"))]
pub mod entry {
    use std::ffi::{CStr, c_char, c_int, c_void};

    use super::*;

    /// Darwin's `struct sigaction`: a handler, a **32-bit** `sigset_t`, and
    /// flags. Sixteen bytes, against glibc's 152.
    const SIGACTION_SIZE: usize = 16;

    /// Darwin's `stack_t`, whose fields are in a different *order* from Linux's:
    /// `{ void *ss_sp; size_t ss_size; int ss_flags; }` against glibc's
    /// `{ void *ss_sp; int ss_flags; size_t ss_size; }`. Passing one through
    /// reads the size out of the flags.
    const STACK_SIZE: usize = 24;

    /// `SS_ONSTACK` agrees; `SS_DISABLE` is 4 on Darwin and 2 here, and 4 here
    /// is not a defined bit - so the collision is one-way and still a collision.
    const SS_ONSTACK: i32 = 0x0001;
    const SS_DISABLE: i32 = 0x0004;

    /// `SIG_BLOCK`/`SIG_UNBLOCK`/`SIG_SETMASK` - and the worst kind of
    /// divergence, the kind that collides: Darwin's `SIG_BLOCK` is 1, which is
    /// this host's `SIG_UNBLOCK`. A `sigprocmask` passed through does the
    /// opposite of what the caller asked and succeeds.
    fn how_to_host(darwin: c_int) -> Option<c_int> {
        Some(match darwin {
            1 => libc::SIG_BLOCK,
            2 => libc::SIG_UNBLOCK,
            3 => libc::SIG_SETMASK,
            _ => return None,
        })
    }

    /// A Darwin `sigset_t` - a bit per signal, numbered Darwin's way - as this
    /// host's.
    pub(crate) fn mask_to_host(darwin: u32) -> libc::sigset_t {
        // SAFETY: emptied before any bit is added, which is what makes a zeroed
        // `sigset_t` valid to hand to `sigaddset`.
        let mut host: libc::sigset_t = unsafe { core::mem::zeroed() };
        // SAFETY: a live local.
        unsafe { libc::sigemptyset(&mut host) };
        for signum in 1..=31 {
            if darwin & (1u32 << (signum - 1)) == 0 {
                continue;
            }
            if let Some(host_signum) = signal_to_host(signum) {
                // SAFETY: a live local and a signal this host has.
                unsafe { libc::sigaddset(&mut host, host_signum) };
            }
        }
        host
    }

    fn mask_from_host(host: &libc::sigset_t) -> u32 {
        let mut darwin = 0u32;
        for signum in 1..=31 {
            let Some(host_signum) = signal_to_host(signum) else {
                continue;
            };
            // SAFETY: a `sigset_t` the caller owns and a signal this host has.
            if unsafe { libc::sigismember(host, host_signum) } == 1 {
                darwin |= 1u32 << (signum - 1);
            }
        }
        darwin
    }

    /// A signal this host has not got: `EINVAL` rather than a neighbour.
    fn no_such_signal() -> c_int {
        crate::errno::set(libc::EINVAL);
        -1
    }

    /// `void (*signal(int, void (*)(int)))(int)`
    ///
    /// # Safety
    ///
    /// `handler` is a sentinel or a function this process can call as a signal
    /// handler.
    pub unsafe extern "C" fn signal_(signum: c_int, handler: u64) -> u64 {
        // SAFETY: the caller's obligation, passed straight on.
        unsafe { super::signal(signum, handler) }
    }

    /// Read Darwin's sixteen-byte `struct sigaction`.
    ///
    /// # Safety
    ///
    /// `at` is [`SIGACTION_SIZE`] readable bytes.
    unsafe fn read_disposition(at: *const u8) -> Disposition {
        let mut bytes = [0u8; SIGACTION_SIZE];
        // SAFETY: the caller's structure, whose size is Darwin's and not this
        // host's - which is the whole reason it is copied rather than cast.
        unsafe { core::ptr::copy_nonoverlapping(at, bytes.as_mut_ptr(), SIGACTION_SIZE) };
        Disposition {
            handler: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            mask: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            flags: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        }
    }

    /// # Safety
    ///
    /// `at` is [`SIGACTION_SIZE`] writable bytes.
    unsafe fn write_disposition(at: *mut u8, held: Disposition) {
        let mut bytes = [0u8; SIGACTION_SIZE];
        bytes[0..8].copy_from_slice(&held.handler.to_le_bytes());
        bytes[8..12].copy_from_slice(&held.mask.to_le_bytes());
        bytes[12..16].copy_from_slice(&held.flags.to_le_bytes());
        // SAFETY: the caller's structure, of Darwin's size.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), at, SIGACTION_SIZE) };
    }

    /// `int sigaction(int, const struct sigaction *, struct sigaction *)`
    ///
    /// The one thing this cannot bridge, and says so once on stderr: an
    /// `SA_SIGINFO` handler is entered with **this host's** `siginfo_t` and
    /// `ucontext_t`, not Darwin's. The fields a handler usually reads - the
    /// signal number and `si_addr` - are in different places in the two, so a
    /// guest handler that inspects them reads the wrong words.
    ///
    /// Installed anyway rather than refused, because the alternative is not a
    /// diagnostic: an unbound `sigaction` is a call to address zero from inside
    /// whatever library was setting up its crash handler.
    ///
    /// # Safety
    ///
    /// `act` and `old` are [`SIGACTION_SIZE`] bytes, or null.
    pub unsafe extern "C" fn sigaction_(
        signum: c_int,
        act: *const c_void,
        old: *mut c_void,
    ) -> c_int {
        if signal_to_host(signum).is_none() {
            return no_such_signal();
        }
        // SAFETY: the caller's structure, if it passed one.
        let wanted = match act.is_null() {
            true => None,
            false => Some(unsafe { read_disposition(act.cast()) }),
        };
        if wanted.is_some_and(|w| w.flags & 0x0040 != 0) {
            announce_siginfo();
        }

        let mut previous = Disposition::default();
        // SAFETY: the handler is the guest's own, and under the native backend
        // the guest is host code - so it is a function this process can call.
        // The shape of the host's own `struct sigaction` is [`sigaction`]'s
        // business and not this entry point's.
        let rc = unsafe { super::sigaction(signum, wanted, Some(&mut previous)) };
        if rc == 0 && !old.is_null() {
            // SAFETY: the caller's structure.
            unsafe { write_disposition(old.cast(), previous) };
        }
        rc
    }

    fn announce_siginfo() {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "rosemary: an SA_SIGINFO handler was installed - it will be entered with this \
                 host's siginfo_t and ucontext_t rather than Darwin's, so a handler that reads \
                 si_addr or the interrupted registers reads the wrong words"
            );
        }
    }

    /// `int sigprocmask(int how, const sigset_t *set, sigset_t *oset)`
    ///
    /// # Safety
    ///
    /// `set` and `oset` are Darwin `sigset_t` - four bytes each - or null.
    pub unsafe extern "C" fn sigprocmask_(how: c_int, set: *const u32, oset: *mut u32) -> c_int {
        let host_how = match set.is_null() {
            // With no set, `how` is not read at all and any value is legal.
            true => libc::SIG_SETMASK,
            false => match how_to_host(how) {
                Some(how) => how,
                None => {
                    crate::errno::set(libc::EINVAL);
                    return -1;
                }
            },
        };
        // SAFETY: the caller's four bytes, if it passed any.
        let wanted = match set.is_null() {
            true => None,
            false => Some(mask_to_host(unsafe { set.read_unaligned() })),
        };
        // SAFETY: zeroed is the empty set, and it is written before it is read.
        let mut previous: libc::sigset_t = unsafe { core::mem::zeroed() };
        // SAFETY: two live `sigset_t` of this host's own shape.
        let rc = unsafe {
            libc::sigprocmask(
                host_how,
                wanted
                    .as_ref()
                    .map_or(core::ptr::null(), |set| set as *const _),
                &mut previous,
            )
        };
        if rc == 0 && !oset.is_null() {
            // SAFETY: the caller's four bytes.
            unsafe { oset.write_unaligned(mask_from_host(&previous)) };
        }
        rc
    }

    /// `int pthread_sigmask(int how, const sigset_t *set, sigset_t *oset)`
    ///
    /// Every collision [`sigprocmask_`] bridges, and one more: this call returns
    /// its error *as its return value* rather than through `errno`, so the
    /// number has to be Darwin's. `divergent` denies the whole `pthread_` family
    /// and this is one of the entries carved back out of it.
    ///
    /// # Safety
    ///
    /// As [`sigprocmask_`].
    pub unsafe extern "C" fn pthread_sigmask_(
        how: c_int,
        set: *const u32,
        oset: *mut u32,
    ) -> c_int {
        // SAFETY: the caller's obligation, which is the same one.
        match unsafe { sigprocmask_(how, set, oset) } {
            0 => 0,
            _ => crate::errno::from_host(crate::errno::get()),
        }
    }

    /// # Safety
    ///
    /// `at` is [`STACK_SIZE`] readable bytes.
    unsafe fn read_stack(at: *const u8) -> Result<libc::stack_t, c_int> {
        let mut bytes = [0u8; STACK_SIZE];
        // SAFETY: the caller's structure, of Darwin's size.
        unsafe { core::ptr::copy_nonoverlapping(at, bytes.as_mut_ptr(), STACK_SIZE) };
        let flags = i32::from_le_bytes(bytes[16..20].try_into().unwrap());
        if !matches!(flags, 0 | SS_DISABLE) {
            return Err(libc::EINVAL);
        }
        let stack = libc::stack_t {
            ss_sp: usize::from_le_bytes(bytes[0..8].try_into().unwrap()) as *mut c_void,
            ss_size: usize::from_le_bytes(bytes[8..16].try_into().unwrap()),
            ss_flags: match flags & SS_DISABLE != 0 {
                true => libc::SS_DISABLE,
                false => flags & SS_ONSTACK,
            },
        };
        // Modern Darwin requires a substantially larger signal stack than Linux.
        // Passing the value through would let 2 KiB succeed where Darwin returns
        // ENOMEM, only for a later handler to overflow it.
        const DARWIN_MINSIGSTKSZ: usize = 32 * 1024;
        if flags == 0 && stack.ss_size < DARWIN_MINSIGSTKSZ {
            return Err(libc::ENOMEM);
        }
        Ok(stack)
    }

    /// # Safety
    ///
    /// `at` is [`STACK_SIZE`] writable bytes.
    unsafe fn write_stack(at: *mut u8, stack: &libc::stack_t) {
        let mut flags = stack.ss_flags & SS_ONSTACK;
        if stack.ss_flags & libc::SS_DISABLE != 0 {
            flags |= SS_DISABLE;
        }
        let mut bytes = [0u8; STACK_SIZE];
        bytes[0..8].copy_from_slice(&(stack.ss_sp as usize).to_le_bytes());
        bytes[8..16].copy_from_slice(&stack.ss_size.to_le_bytes());
        bytes[16..20].copy_from_slice(&flags.to_le_bytes());
        // SAFETY: the caller's structure, of Darwin's size.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), at, STACK_SIZE) };
    }

    /// `int sigaltstack(const stack_t *ss, stack_t *oss)`
    ///
    /// # Safety
    ///
    /// `ss` and `oss` are [`STACK_SIZE`] bytes, or null.
    pub unsafe extern "C" fn sigaltstack_(ss: *const c_void, oss: *mut c_void) -> c_int {
        // SAFETY: the caller's structure, if it passed one.
        let wanted = match ss.is_null() {
            true => None,
            false => match unsafe { read_stack(ss.cast()) } {
                Ok(stack) => Some(stack),
                Err(error) => {
                    crate::errno::set(error);
                    return -1;
                }
            },
        };
        if altstack_retained() {
            let previous = pretended_altstack();
            if let Some(wanted) = wanted.as_ref() {
                set_pretended_altstack(wanted);
            }
            if !oss.is_null() {
                // SAFETY: the caller's structure.
                unsafe { write_stack(oss.cast(), &previous) };
            }
            return 0;
        }
        // SAFETY: zeroed is a disabled stack, and it is written before it is read.
        let mut previous: libc::stack_t = unsafe { core::mem::zeroed() };
        // SAFETY: two live `stack_t` of this host's own shape.
        let rc = unsafe {
            libc::sigaltstack(
                wanted.as_ref().map_or(core::ptr::null(), |s| s as *const _),
                &mut previous,
            )
        };
        if rc == 0 && !oss.is_null() {
            // SAFETY: the caller's structure.
            unsafe { write_stack(oss.cast(), &previous) };
        }
        rc
    }

    /// `int raise(int)`
    pub extern "C" fn raise_(signum: c_int) -> c_int {
        match signal_to_host(signum) {
            // SAFETY: a signal number this host has.
            Some(host) => unsafe { libc::raise(host) },
            None => no_such_signal(),
        }
    }

    /// `int kill(pid_t, int)`
    ///
    /// Signal zero is the existence probe - "is this process alive" - and is
    /// not a signal number, so it goes through untranslated. A crash handler
    /// watching its client is the caller that depends on this.
    pub extern "C" fn kill_(pid: c_int, signum: c_int) -> c_int {
        if signum == 0 {
            // SAFETY: a plain system call.
            return unsafe { libc::kill(pid, 0) };
        }
        match signal_to_host(signum) {
            // SAFETY: a signal number this host has.
            Some(host) => unsafe { libc::kill(pid, host) },
            None => no_such_signal(),
        }
    }

    /// `int killpg(pid_t, int)`
    pub extern "C" fn killpg_(pgrp: c_int, signum: c_int) -> c_int {
        match signum == 0 {
            true => kill_(-pgrp, 0),
            false => match signal_to_host(signum) {
                // SAFETY: a signal number this host has.
                Some(host) => unsafe { libc::killpg(pgrp, host) },
                None => no_such_signal(),
            },
        }
    }

    // The `sigset_t` operations. A Darwin `sigset_t` is a `uint32_t` whose bits
    // are numbered by *Darwin's* signal numbers, and the guest's numbers are
    // Darwin's - so these translate nothing and are pure bit arithmetic. They
    // are here because glibc's take a 128-byte structure and would write 124
    // bytes past the guest's word.

    /// # Safety
    ///
    /// `set` is a writable Darwin `sigset_t`, or null.
    pub unsafe extern "C" fn sigemptyset_(set: *mut u32) -> c_int {
        match set.is_null() {
            true => -1,
            // SAFETY: the caller's four bytes.
            false => unsafe {
                set.write_unaligned(0);
                0
            },
        }
    }

    /// # Safety
    ///
    /// As [`sigemptyset_`].
    pub unsafe extern "C" fn sigfillset_(set: *mut u32) -> c_int {
        match set.is_null() {
            true => -1,
            // SAFETY: the caller's four bytes.
            false => unsafe {
                set.write_unaligned(!0);
                0
            },
        }
    }

    /// The bit `signum` occupies, or `None` for a number that is not a signal.
    fn bit(signum: c_int) -> Option<u32> {
        (1..=31).contains(&signum).then(|| 1u32 << (signum - 1))
    }

    /// # Safety
    ///
    /// As [`sigemptyset_`].
    pub unsafe extern "C" fn sigaddset_(set: *mut u32, signum: c_int) -> c_int {
        let Some(bit) = bit(signum) else {
            return no_such_signal();
        };
        match set.is_null() {
            true => -1,
            // SAFETY: the caller's four bytes.
            false => unsafe {
                set.write_unaligned(set.read_unaligned() | bit);
                0
            },
        }
    }

    /// # Safety
    ///
    /// As [`sigemptyset_`].
    pub unsafe extern "C" fn sigdelset_(set: *mut u32, signum: c_int) -> c_int {
        let Some(bit) = bit(signum) else {
            return no_such_signal();
        };
        match set.is_null() {
            true => -1,
            // SAFETY: the caller's four bytes.
            false => unsafe {
                set.write_unaligned(set.read_unaligned() & !bit);
                0
            },
        }
    }

    /// # Safety
    ///
    /// `set` is a readable Darwin `sigset_t`, or null.
    pub unsafe extern "C" fn sigismember_(set: *const u32, signum: c_int) -> c_int {
        let Some(bit) = bit(signum) else {
            return no_such_signal();
        };
        match set.is_null() {
            true => -1,
            // SAFETY: the caller's four bytes.
            false => unsafe { (set.read_unaligned() & bit != 0) as c_int },
        }
    }

    /// `int sigpending(sigset_t *)`
    ///
    /// # Safety
    ///
    /// As [`sigemptyset_`].
    pub unsafe extern "C" fn sigpending_(set: *mut u32) -> c_int {
        if set.is_null() {
            crate::errno::set(libc::EFAULT);
            return -1;
        }
        // SAFETY: zeroed is the empty set, written before it is read.
        let mut host: libc::sigset_t = unsafe { core::mem::zeroed() };
        // SAFETY: a live `sigset_t` of this host's shape.
        let rc = unsafe { libc::sigpending(&mut host) };
        if rc == 0 {
            // SAFETY: the caller's four bytes.
            unsafe { set.write_unaligned(mask_from_host(&host)) };
        }
        rc
    }

    /// `int sigsuspend(const sigset_t *)`
    ///
    /// # Safety
    ///
    /// `mask` is a readable Darwin `sigset_t`.
    pub unsafe extern "C" fn sigsuspend_(mask: *const u32) -> c_int {
        if mask.is_null() {
            crate::errno::set(libc::EFAULT);
            return -1;
        }
        // SAFETY: the caller's four bytes.
        let host = mask_to_host(unsafe { mask.read_unaligned() });
        // SAFETY: a live `sigset_t` of this host's shape. Returns only when a
        // handler has run, always with `EINTR`.
        unsafe { libc::sigsuspend(&host) }
    }

    /// `char *strsignal(int)`, whose argument is a *Darwin* signal number.
    ///
    /// The same trap `strerror` has, and the same answer: the number names a
    /// different signal on each host, so it is translated before the host's
    /// table is asked. `SIGEMT` and `SIGINFO` have no host name at all and are
    /// spelled here rather than reported as some neighbour's.
    pub extern "C" fn strsignal_(signum: c_int) -> *const c_char {
        match signum {
            signum::SIGEMT => return c"EMT trap".as_ptr(),
            signum::SIGINFO => return c"Information request".as_ptr(),
            _ => {}
        }
        let Some(host) = signal_to_host(signum) else {
            return c"Unknown signal".as_ptr();
        };
        // SAFETY: a signal number this host has; `strsignal` returns storage the
        // caller does not own and does not free.
        unsafe { libc::strsignal(host) }
    }

    /// `void psignal(int, const char *)`
    ///
    /// # Safety
    ///
    /// `prefix` is a NUL-terminated string, or null.
    pub unsafe extern "C" fn psignal_(signum: c_int, prefix: *const c_char) {
        // SAFETY: this module's own storage or the host's, both NUL-terminated.
        let described = unsafe { CStr::from_ptr(strsignal_(signum)) }.to_string_lossy();
        match prefix.is_null() {
            true => eprintln!("{described}"),
            // SAFETY: the caller's NUL-terminated string.
            false => {
                let prefix = unsafe { CStr::from_ptr(prefix) }.to_string_lossy();
                eprintln!("{prefix}: {described}");
            }
        }
    }

    /// Darwin's `sys_signame`: each signal's lowercase name, indexed by
    /// Darwin's numbering, which is not this host's.
    #[repr(transparent)]
    pub struct SignalNames([*const std::ffi::c_char; 32]);

    // SAFETY: pointers to string literals, and the table is never written.
    unsafe impl Sync for SignalNames {}

    pub static SYS_SIGNAME: SignalNames = SignalNames([
        c"Signal 0".as_ptr(),
        c"hup".as_ptr(),
        c"int".as_ptr(),
        c"quit".as_ptr(),
        c"ill".as_ptr(),
        c"trap".as_ptr(),
        c"abrt".as_ptr(),
        c"emt".as_ptr(),
        c"fpe".as_ptr(),
        c"kill".as_ptr(),
        c"bus".as_ptr(),
        c"segv".as_ptr(),
        c"sys".as_ptr(),
        c"pipe".as_ptr(),
        c"alrm".as_ptr(),
        c"term".as_ptr(),
        c"urg".as_ptr(),
        c"stop".as_ptr(),
        c"tstp".as_ptr(),
        c"cont".as_ptr(),
        c"chld".as_ptr(),
        c"ttin".as_ptr(),
        c"ttou".as_ptr(),
        c"io".as_ptr(),
        c"xcpu".as_ptr(),
        c"xfsz".as_ptr(),
        c"vtalrm".as_ptr(),
        c"prof".as_ptr(),
        c"winch".as_ptr(),
        c"info".as_ptr(),
        c"usr1".as_ptr(),
        c"usr2".as_ptr(),
    ]);

    exports::exports! {
        functions {
            "signal" => signal_,
            "sigaction" => sigaction_,
            "sigprocmask" => sigprocmask_,
            "pthread_sigmask" => pthread_sigmask_,
            "sigaltstack" => sigaltstack_,
            "sigsuspend" => sigsuspend_,
            "sigpending" => sigpending_,
            "raise" => raise_,
            "kill" => kill_,
            "killpg" => killpg_,
            "sigemptyset" => sigemptyset_,
            "sigfillset" => sigfillset_,
            "sigaddset" => sigaddset_,
            "sigdelset" => sigdelset_,
            "sigismember" => sigismember_,
            "strsignal" => strsignal_,
            "psignal" => psignal_,
        }
        constants {
            // Data: the guest indexes the array at this address.
            "sys_signame" => &raw const SYS_SIGNAME as u64,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The collision worth the whole module: Darwin's `SIG_BLOCK` is this
        /// host's `SIG_UNBLOCK`, so a passed-through `sigprocmask` unblocks what
        /// the caller asked to block and reports success.
        #[test]
        fn the_sigprocmask_how_values_are_translated_rather_than_forwarded() {
            assert_eq!(how_to_host(1), Some(libc::SIG_BLOCK));
            assert_ne!(1, libc::SIG_BLOCK, "the collision being avoided");
            assert_eq!(1, libc::SIG_UNBLOCK, "and what forwarding would have meant");
            assert_eq!(how_to_host(3), Some(libc::SIG_SETMASK));
            assert_eq!(how_to_host(0), None);
        }

        /// `SIGUSR1` is 30 on Darwin and 10 here, so a mask with Darwin's bit 30
        /// set must come back as Darwin's bit 30 and not as bit 10.
        #[test]
        fn a_signal_mask_round_trips_through_the_hosts_shape() {
            let darwin = (1u32 << (signum::SIGUSR1 - 1)) | (1u32 << (signum::SIGCHLD - 1));
            let host = mask_to_host(darwin);
            // SAFETY: a live `sigset_t` this test owns.
            assert_eq!(unsafe { libc::sigismember(&host, libc::SIGUSR1) }, 1);
            assert_eq!(mask_from_host(&host), darwin);
        }

        /// Darwin puts `ss_size` where Linux puts `ss_flags`, so a structure
        /// passed through reads the size out of the flags word.
        #[test]
        fn an_alternate_stack_is_reordered_rather_than_passed_through() {
            let mut darwin = [0u8; STACK_SIZE];
            darwin[0..8].copy_from_slice(&0x1234_5000u64.to_le_bytes());
            darwin[8..16].copy_from_slice(&(64u64 * 1024).to_le_bytes());
            darwin[16..20].copy_from_slice(&SS_DISABLE.to_le_bytes());

            // SAFETY: `darwin` is `STACK_SIZE` bytes.
            let host = unsafe { read_stack(darwin.as_ptr()) }.expect("a valid disabled stack");
            assert_eq!(host.ss_sp as usize, 0x1234_5000);
            assert_eq!(host.ss_size, 64 * 1024);
            assert_eq!(host.ss_flags, libc::SS_DISABLE);
            assert_ne!(SS_DISABLE, libc::SS_DISABLE, "the collision being avoided");

            let mut back = [0u8; STACK_SIZE];
            // SAFETY: `back` is `STACK_SIZE` bytes.
            unsafe { write_stack(back.as_mut_ptr(), &host) };
            assert_eq!(back, darwin);
        }

        /// A Darwin `sigset_t` is one word, and the guest reserved exactly four
        /// bytes for it. glibc's own would write 128.
        #[test]
        fn the_set_operations_touch_four_bytes_and_number_the_bits_darwins_way() {
            let mut set = [0u32, 0xdead_beef];
            // SAFETY: a live word, and the second element is the guard.
            unsafe {
                assert_eq!(sigfillset_(set.as_mut_ptr()), 0);
                assert_eq!(sigismember_(set.as_ptr(), signum::SIGUSR1), 1);
                assert_eq!(sigdelset_(set.as_mut_ptr(), signum::SIGUSR1), 0);
                assert_eq!(sigismember_(set.as_ptr(), signum::SIGUSR1), 0);
                assert_eq!(sigismember_(set.as_ptr(), signum::SIGCHLD), 1);
                assert_eq!(sigemptyset_(set.as_mut_ptr()), 0);
                assert_eq!(sigaddset_(set.as_mut_ptr(), signum::SIGUSR1), 0);
            }
            assert_eq!(set[0], 1 << (signum::SIGUSR1 - 1));
            assert_eq!(set[1], 0xdead_beef, "nothing past the caller's word");
        }

        #[test]
        fn signal_32_is_not_accepted_as_a_darwin_signal() {
            let mut set = 0u32;
            crate::errno::set(0);
            // SAFETY: `set` is a live Darwin signal-set word.
            assert_eq!(unsafe { sigaddset_(&mut set, 32) }, -1);
            assert_eq!(crate::errno::get(), libc::EINVAL);
            assert_eq!(set, 0);
            crate::errno::set(0);
        }

        #[test]
        fn alternate_stack_input_is_validated_to_darwins_rules() {
            let mut darwin = [0u8; STACK_SIZE];
            darwin[0..8].copy_from_slice(&0x1234_5000u64.to_le_bytes());
            darwin[8..16].copy_from_slice(&2048u64.to_le_bytes());
            // SAFETY: `darwin` is a complete Darwin stack_t.
            assert_eq!(
                unsafe { sigaltstack_(darwin.as_ptr().cast(), std::ptr::null_mut()) },
                -1
            );
            assert_eq!(crate::errno::get(), libc::ENOMEM);

            darwin[8..16].copy_from_slice(&(32u64 * 1024).to_le_bytes());
            darwin[16..20].copy_from_slice(&2i32.to_le_bytes());
            assert_eq!(
                unsafe { sigaltstack_(darwin.as_ptr().cast(), std::ptr::null_mut()) },
                -1
            );
            assert_eq!(crate::errno::get(), libc::EINVAL);
            crate::errno::set(0);
        }

        /// `kill(pid, 0)` is the existence probe a crash handler watches its
        /// client with, and zero is not a signal number to translate.
        #[test]
        fn signal_zero_probes_existence_rather_than_being_translated() {
            let self_pid = unsafe { libc::getpid() };
            assert_eq!(kill_(self_pid, 0), 0);
            assert_eq!(kill_(self_pid, 99), -1, "not a signal");
        }

        /// Darwin's structure is sixteen bytes against glibc's hundred and
        /// fifty-two, and the guest reserved sixteen.
        #[test]
        fn a_disposition_occupies_darwins_sixteen_bytes_and_no_more() {
            let mut bytes = [0xAAu8; SIGACTION_SIZE + 8];
            let held = Disposition {
                handler: 0x1234_5678_9abc,
                mask: 0x0f0f_0f0f,
                flags: 0x0040,
            };
            // SAFETY: `bytes` is longer than `SIGACTION_SIZE`.
            unsafe {
                write_disposition(bytes.as_mut_ptr(), held);
                assert_eq!(read_disposition(bytes.as_ptr()), held);
            }
            assert!(
                bytes[SIGACTION_SIZE..].iter().all(|b| *b == 0xAA),
                "nothing past the guest's structure"
            );
        }

        #[test]
        fn a_signal_this_host_has_not_got_is_refused_rather_than_aimed_at_a_neighbour() {
            assert_eq!(raise_(signum::SIGEMT), -1);
            assert_eq!(raise_(signum::SIGINFO), -1);
            // SAFETY: this module's own NUL-terminated storage.
            let named = unsafe { CStr::from_ptr(strsignal_(signum::SIGINFO)) };
            assert_eq!(named.to_str().unwrap(), "Information request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A retained signal's disposition is the runtime's: the guest's install
    /// succeeds, the host's handler is untouched, and the guest reads its own
    /// installation back - a crash reporter that reads `SIG_DFL` there
    /// concludes it failed to start.
    #[test]
    fn a_retained_signal_is_accepted_recorded_and_not_applied() {
        // `SIGUSR2`, so nothing else in this process depends on the outcome.
        const SIGUSR2: i32 = signum::SIGUSR2;
        let host = signal_to_host(SIGUSR2).expect("this host has SIGUSR2");

        // SAFETY: a live `struct sigaction`, read only.
        let before = unsafe {
            let mut current: libc::sigaction = core::mem::zeroed();
            libc::sigaction(host, core::ptr::null(), &mut current);
            current.sa_sigaction
        };

        retain(SIGUSR2);
        assert!(retained(SIGUSR2));

        let wanted = Disposition {
            handler: 0xdead_beef,
            mask: 0,
            flags: 0,
        };
        let mut old = Disposition::default();
        // SAFETY: the handler is never entered - that is what being retained
        // means, and is what this test asserts.
        assert_eq!(
            unsafe { sigaction(SIGUSR2, Some(wanted), Some(&mut old)) },
            0
        );

        let after = unsafe {
            let mut current: libc::sigaction = core::mem::zeroed();
            libc::sigaction(host, core::ptr::null(), &mut current);
            current.sa_sigaction
        };
        assert_eq!(after, before, "the host's disposition was changed");

        let mut read_back = Disposition::default();
        // SAFETY: a query, with no disposition to install.
        assert_eq!(unsafe { sigaction(SIGUSR2, None, Some(&mut read_back)) }, 0);
        assert_eq!(read_back.handler, wanted.handler);

        RETAINED.fetch_and(
            !(1u32 << (SIGUSR2 - 1)),
            core::sync::atomic::Ordering::Relaxed,
        );
    }

    /// A mask taken out of a disposition and put back must be the same mask - which
    /// is the whole of what [`Mask`] promises, and the one thing a `u32` copy of a
    /// 128-byte `sigset_t` would break.
    #[test]
    fn a_mask_round_trips_through_a_disposition() {
        // SAFETY: zeroed is a valid `sigaction` - no handler, no flags, empty mask.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: a live local.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1);
        }
        let saved = Mask::of(&action);

        let mut other: libc::sigaction = unsafe { core::mem::zeroed() };
        saved.apply(&mut other);
        // SAFETY: `other.sa_mask` is a live, initialised set.
        assert_eq!(
            unsafe { libc::sigismember(&other.sa_mask, libc::SIGUSR1) },
            1
        );
        assert_eq!(
            unsafe { libc::sigismember(&other.sa_mask, libc::SIGUSR2) },
            0
        );
    }

    /// A mask parked in a `static` and taken back out is the mask that went in -
    /// which is the property `fault.rs` restores a displaced disposition with.
    #[test]
    fn a_mask_survives_a_static() {
        static SAVED: StaticMask = StaticMask::empty();
        // SAFETY: nothing else touches `SAVED`, and no handler reads it.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: a live local.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaddset(&mut action.sa_mask, libc::SIGUSR2);
            SAVED.set(Mask::of(&action));
        }

        // SAFETY: zeroed is a valid `sigaction`.
        let mut other: libc::sigaction = unsafe { core::mem::zeroed() };
        SAVED.get().apply(&mut other);
        // SAFETY: a live, initialised set.
        assert_eq!(
            unsafe { libc::sigismember(&other.sa_mask, libc::SIGUSR2) },
            1
        );
    }

    /// The numbers are Darwin's, cross-checked against the host's own declarations
    /// where the host is a Mac. On Linux the same assertions would be false, which is
    /// the point of the translation.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_signal_numbers_are_the_ones_darwin_declares() {
        assert_eq!(signum::SIGUSR1, libc::SIGUSR1);
        assert_eq!(signum::SIGBUS, libc::SIGBUS);
        assert_eq!(signum::SIGCHLD, libc::SIGCHLD);
        assert_eq!(signum::SIGSYS, libc::SIGSYS);
    }

    /// The three that bite, named: an untranslated `SIGUSR1` is `SIGPWR` on Linux,
    /// and an untranslated `SIGBUS` is `SIGUSR1`.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_numbers_that_differ_are_translated_rather_than_passed_through() {
        assert_eq!(signal_to_host(signum::SIGUSR1), Some(libc::SIGUSR1));
        assert_ne!(signum::SIGUSR1, libc::SIGUSR1);
        assert_eq!(signal_to_host(signum::SIGBUS), Some(libc::SIGBUS));
        assert_ne!(signum::SIGBUS, libc::SIGBUS);
        assert_eq!(signal_to_host(signum::SIGCHLD), Some(libc::SIGCHLD));
        assert_eq!(signal_to_host(signum::SIGINFO), None, "Darwin's own");
        assert_eq!(signal_to_host(signum::SIGEMT), None, "Darwin's own");
    }

    /// Every signal that translates must translate back to itself, or a guest asking
    /// which signal stopped it is told about a different one.
    #[test]
    fn translation_round_trips() {
        for darwin in 1..=31 {
            let Some(host) = signal_to_host(darwin) else {
                continue;
            };
            assert_eq!(
                signal_from_host(host),
                Some(darwin),
                "signal {darwin} does not come back as itself"
            );
        }
    }

    #[test]
    fn a_null_context_has_no_program_counter() {
        // SAFETY: null is the one argument this is documented to accept and refuse.
        assert_eq!(unsafe { interrupted_pc(core::ptr::null_mut()) }, None);
    }

    #[test]
    fn retained_alternate_stack_state_is_per_thread_and_initially_disabled() {
        let main = libc::stack_t {
            ss_sp: 0x1234usize as *mut libc::c_void,
            ss_size: 64 * 1024,
            ss_flags: 0,
        };
        set_pretended_altstack(&main);

        let other = std::thread::spawn(|| {
            let initial = pretended_altstack();
            assert!(initial.ss_sp.is_null());
            assert_eq!(initial.ss_flags, libc::SS_DISABLE);
            set_pretended_altstack(&libc::stack_t {
                ss_sp: 0x5678usize as *mut libc::c_void,
                ss_size: 32 * 1024,
                ss_flags: 0,
            });
            pretended_altstack().ss_sp as usize
        })
        .join()
        .expect("thread exits");

        assert_eq!(other, 0x5678);
        assert_eq!(pretended_altstack().ss_sp as usize, 0x1234);
    }
}

#[cfg(test)]
mod disposition_tests {
    use super::*;

    /// The claim this module exists to make: the guest's sixteen bytes are not
    /// the host's structure.
    ///
    /// A compile-time reminder rather than a runtime one. Handing a Darwin
    /// `struct sigaction` to glibc reads a 128-byte `sigset_t` off the end of a
    /// 16-byte value and writes 152 bytes into it, which is a smashed stack and
    /// a jump to zero rather than anything that looks like a signal bug.
    #[test]
    fn the_hosts_structure_is_not_the_guests_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<Disposition>(), 16);
        #[cfg(not(target_os = "macos"))]
        assert!(
            core::mem::size_of::<libc::sigaction>() > 16,
            "this host's struct sigaction is Darwin-shaped after all; \
             the conversion below is then merely redundant, not wrong"
        );
    }

    /// A handler installed and read back is the same handler, and the mask and
    /// flags survive the round trip in Darwin's spelling.
    #[test]
    fn a_disposition_round_trips_through_the_host() {
        unsafe extern "C" fn handler(_signum: libc::c_int) {}

        let wanted = Disposition {
            handler: handler as *const () as usize as u64,
            // `SIGUSR2` and `SIGINT`, in Darwin's numbering.
            mask: (1 << (signum::SIGUSR2 - 1)) | (1 << (signum::SIGINT - 1)),
            // Darwin's `SA_RESTART`, which is not Linux's value for it.
            flags: 0x0002,
        };

        // SAFETY: a real function of the right prototype, on a signal this test
        // restores below.
        let rc = unsafe { sigaction(signum::SIGUSR1, Some(wanted), None) };
        assert_eq!(rc, 0);

        let mut back = Disposition::default();
        // SAFETY: a query, which installs nothing.
        assert_eq!(
            unsafe { sigaction(signum::SIGUSR1, None, Some(&mut back)) },
            0
        );
        assert_eq!(back, wanted, "the disposition did not survive the host");

        // SAFETY: restoring the default for the signal this test took.
        unsafe { sigaction(signum::SIGUSR1, Some(Disposition::default()), None) };
    }

    /// A signal this host has not got is refused rather than aimed at a
    /// neighbour: a handler installed for `SIGINFO` must not run for something
    /// else.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_signal_this_host_lacks_is_refused() {
        // SAFETY: `SIGINFO` maps to nothing here, so nothing is installed.
        assert_eq!(unsafe { sigaction(signum::SIGINFO, None, None) }, -1);
        assert_eq!(unsafe { signal(signum::SIGEMT, 0) }, u64::MAX);
    }

    /// Darwin's `SA_*` bits are not this host's, and the translation is a
    /// bijection over the ones both have.
    #[test]
    fn the_flag_bits_survive_a_round_trip() {
        for (bit, name) in saflag::DARWIN {
            if saflag::host(name).is_none() {
                continue;
            }
            assert_eq!(
                flags_from_host(flags_to_host(*bit)),
                *bit,
                "SA_{name} did not round trip"
            );
        }
    }

    static HEARD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    extern "C" fn record(signum: libc::c_int) {
        HEARD.store(signum, std::sync::atomic::Ordering::SeqCst);
    }

    /// A guest handler is told Darwin's number, not the kernel's, and reading
    /// the disposition back names the guest's handler rather than the
    /// translator in front of it.
    #[test]
    fn a_guest_handler_is_entered_with_darwins_signal_number() {
        let darwin_usr1 = signum::SIGUSR1;
        let host_usr1 = signal_to_host(darwin_usr1).unwrap();
        let act = Disposition {
            handler: record as *const () as usize as u64,
            mask: 0,
            flags: 0,
        };
        // SAFETY: a handler of this test's own for a signal nothing else uses.
        unsafe {
            assert_eq!(sigaction(darwin_usr1, Some(act), None), 0);
            libc::raise(host_usr1);
        }
        assert_eq!(HEARD.load(std::sync::atomic::Ordering::SeqCst), darwin_usr1);

        let mut back = Disposition::default();
        // SAFETY: a query, and then the default put back.
        unsafe {
            assert_eq!(sigaction(darwin_usr1, None, Some(&mut back)), 0);
            assert_eq!(back.handler, record as *const () as usize as u64);
            assert_eq!(
                back.flags & DARWIN_SA_SIGINFO,
                0,
                "the translator's flag is not reported"
            );
            let previous = signal(darwin_usr1, 0);
            assert_eq!(previous, record as *const () as usize as u64);
        }
    }
}
