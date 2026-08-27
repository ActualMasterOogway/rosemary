//! The parts of a signal disposition whose *shape* differs, rather than its meaning.
//!
//! `struct sigaction`'s `sa_mask` is a `sigset_t`, and a `sigset_t` is a 32-bit word
//! on Darwin and a 128-byte structure under glibc. `siginfo_t`'s faulting address is a
//! field on Darwin and an accessor under Linux. Neither difference is interesting -
//! both sides mean the same thing - so both are spelled once here rather than at every
//! reader.
//!
//! The signal *numbers* are a different matter and are not bridged: see
//! [`crate::divergent`], which refuses to pass a guest's `sigaction` through.

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
        // Darwin's `_STRUCT_UCONTEXT` and thread state, which the `libc` crate does
        // not declare for Apple platforms - hence the offsets.
        //
        // Byte offset of `uc_mcontext` within `ucontext_t`: the same on both
        // architectures.
        const UC_MCONTEXT: usize = 48;
        // Byte offset of `__ss.__pc` within `__darwin_mcontext64`: sixteen bytes of
        // `__es`, then the twenty-nine general-purpose registers, `fp` and `lr`.
        #[cfg(target_arch = "aarch64")]
        const MC_PC: usize = 16 + 32 * 8;
        // Byte offset of `__ss.__rip`: sixteen bytes of `__es`, then the sixteen
        // general-purpose registers `_STRUCT_X86_THREAD_STATE64` lists before it.
        #[cfg(target_arch = "x86_64")]
        const MC_PC: usize = 16 + 16 * 8;

        // SAFETY: the kernel passes a live `ucontext_t` for the duration of the
        // handler, whose `uc_mcontext` is a pointer to a live thread state.
        unsafe {
            let mcontext =
                core::ptr::read((context as *const u8).add(UC_MCONTEXT) as *const *const u8);
            if mcontext.is_null() {
                return None;
            }
            Some(core::ptr::read(mcontext.add(MC_PC) as *const u64))
        }
    }
    #[cfg(all(not(target_os = "macos"), target_arch = "aarch64"))]
    // SAFETY: the kernel passes a live `ucontext_t`, which the `libc` crate does
    // declare here.
    unsafe {
        Some((*(context as *const libc::ucontext_t)).uc_mcontext.pc)
    }
    #[cfg(all(not(target_os = "macos"), target_arch = "x86_64"))]
    // SAFETY: as above. `REG_RIP` indexes the general-purpose register array.
    unsafe {
        Some((*(context as *const libc::ucontext_t)).uc_mcontext.gregs[libc::REG_RIP as usize] as u64)
    }
    #[cfg(all(
        not(target_os = "macos"),
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // SAFETY: as above.
        let mut other: libc::sigaction = unsafe { core::mem::zeroed() };
        saved.apply(&mut other);
        // SAFETY: `other.sa_mask` is a live, initialised set.
        assert_eq!(unsafe { libc::sigismember(&other.sa_mask, libc::SIGUSR1) }, 1);
        assert_eq!(unsafe { libc::sigismember(&other.sa_mask, libc::SIGUSR2) }, 0);
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
        assert_eq!(unsafe { libc::sigismember(&other.sa_mask, libc::SIGUSR2) }, 1);
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
}
