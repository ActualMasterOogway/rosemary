//! The Darwin thread-specific data array, and the boundary around code that reads it.
//!
//! A Darwin thread keeps a flat array of `void *` at its thread pointer. Code reaches
//! it without calling anything: `pthread_self` is one load of slot 0, `errno` is a
//! load of slot 1, and a function compiled with `-fstack-protector` reads its canary
//! straight out of slot 6. On x86-64 the array is at `%gs`; on arm64 it is
//! `TPIDRRO_EL0` with the low three bits - which carry the CPU number, not address -
//! masked off.
//!
//! Three situations, and it is worth being exact about which is which, because two of
//! them need nothing at all:
//!
//! 1. An emulated guest. Lifted code does not execute `mrs`; it adds a base this
//!    runtime supplies to an address. So the base is a value, [`tsd_base`], and the
//!    host's own thread pointer is never disturbed. Host libc called from lifted code
//!    - every shim, every fast import - works throughout.
//! 2. Darwin code on a Darwin host. The host's array is the right one already.
//! 3. Darwin code on a host that is not Darwin. Here the thread pointer really
//!    does have to change across the call, which is what [`DarwinThread`] is for.
//!
//! Only (3) needs a trampoline, and only (3) is unfinished - see [`DarwinThread`].

/// Direct TSD slots, by index. Darwin's `<pthread/tsd_private.h>` names.
///
/// `pthread_getspecific(k)` for a small `k` is a single load of slot `k`, so these are
/// part of the guest's ABI in the same way a struct offset is.
pub mod slot {
    /// The array's own base, which is what makes `pthread_self` one load.
    pub const SELF: usize = 0;
    /// `int *`, this thread's `errno`. `__error()` returns it.
    pub const ERRNO: usize = 1;
    /// This thread's Mach port. Meaningless off Darwin; left zero.
    pub const MACH_THREAD_SELF: usize = 3;
    /// The `-fstack-protector` canary, read directly as `%gs:0x30` on x86-64.
    pub const STACK_GUARD: usize = 6;
    /// The cookie `_pthread_ptr_mangle` xors function pointers with.
    pub const PTR_MUNGE: usize = 7;
}

const SLOTS: usize = 512;

const HOST_THREAD_POINTER: usize = SLOTS;

/// How many teardown passes have reached this thread's array - see [`release`].
#[cfg(not(target_os = "macos"))]
const PASSES: usize = SLOTS + 1;

/// The whole array, direct slots and this module's two words past them.
#[cfg(not(target_os = "macos"))]
const WORDS: usize = SLOTS + 2;

/// How many times libpthread calls a key's destructor before giving up.
///
/// `PTHREAD_DESTRUCTOR_ITERATIONS`, four on Darwin and on glibc alike.
#[cfg(not(target_os = "macos"))]
const DESTRUCTOR_PASSES: u64 = 4;

/// The canary a synthetic array reports.
///
/// The same value `rosetta3::shims::runtime` binds `___stack_chk_guard` to, and it has
/// to be: a function may take its canary from the import and check it against slot 6,
/// or the other way about, and the two spellings are the same variable. Contains a
/// zero byte, so a string overflow cannot copy it back.
pub const STACK_CHK_GUARD: u64 = 0x00ff_dead_beef_c0de;

/// The base of this thread's Darwin TSD array - the value a guest's `%gs` must hold.
///
/// On Darwin this is the thread's real array, which is the answer that costs nothing
/// and keeps a guest's `pthread_self` and a host library's agreeing.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn tsd_base() -> u64 {
    let tpidrro: u64;
    // SAFETY: `mrs` from `TPIDRRO_EL0` is unprivileged and readable from EL0 on every
    // arm64 Apple Silicon core; it reads a register and touches no memory.
    unsafe {
        std::arch::asm!("mrs {}, tpidrro_el0", out(reg) tpidrro, options(nomem, nostack, preserves_flags))
    };
    tpidrro & !0b111
}

/// On x86-64 Darwin the array is what `%gs` already points at, and slot 0 holds the
/// base itself - which is how `pthread_self` is a single `mov` from `%gs:0`.
///
/// Read out of the segment rather than asked for by `pthread_self()`, because the two
/// are the same value and this one cannot call into the guest's shims.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn tsd_base() -> u64 {
    let base: u64;
    // SAFETY: every Darwin thread has `%gs` pointing at its own TSD array, and slot
    // zero of that array is live for as long as the thread is. One load, no writes.
    unsafe { std::arch::asm!("mov {}, gs:0", out(reg) base, options(nostack, preserves_flags)) };
    base
}

/// A host with no Darwin array, where one is synthesised per thread on first touch.
#[cfg(not(target_os = "macos"))]
pub fn tsd_base() -> u64 {
    let key = synthetic_key();
    // SAFETY: `key` was created by `pthread_key_create` below and is never deleted.
    let existing = unsafe { libc::pthread_getspecific(key) };
    if !existing.is_null() {
        return existing as usize as u64;
    }
    let base = allocate();
    // SAFETY: as above; `base` is a leaked allocation this thread now owns, and
    // `release` frees it when the thread ends.
    unsafe { libc::pthread_setspecific(key, base as usize as *const core::ffi::c_void) };
    base
}

/// The key this thread's array hangs off.
///
/// A raw pthread key rather than a Rust `thread_local!`, and the difference is not
/// stylistic. A guest thread is started by `pthread_create` directly rather than
/// through `std::thread`, and on such a thread a `thread_local!` **with a destructor**
/// cannot register one - `LocalKey::with` reports the slot as already destroyed and
/// panics. Measured: every guest thread's first entry into lifted code died in
/// `tsd_base` before this was a pthread key.
///
/// It is also what Darwin does, which is the other reason to prefer it: the array this
/// returns is the thing `pthread_getspecific` is reading in the first place.
#[cfg(not(target_os = "macos"))]
fn synthetic_key() -> libc::pthread_key_t {
    use std::sync::OnceLock;
    static KEY: OnceLock<libc::pthread_key_t> = OnceLock::new();
    *KEY.get_or_init(|| {
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: a live local; `release` is an `extern "C"` function of this module.
        let rc = unsafe { libc::pthread_key_create(&mut key, Some(release)) };
        assert_eq!(
            rc, 0,
            "a guest thread cannot run without thread-specific data"
        );
        key
    })
}

/// Free a thread's array when the thread ends.
///
/// Puts it back for every pass but the last, because a destructor that has not run yet
/// is Darwin code like any other: it reads `pthread_self` and its stack canary off this
/// array, and mangles pointers through slot 7. A fresh array would answer all three
/// differently halfway through the thread's teardown.
///
/// # Safety
///
/// Called by libpthread with the value this thread stored, which is an [`allocate`]
/// result and nothing else.
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn release(block: *mut core::ffi::c_void) {
    if block.is_null() {
        return;
    }
    let words = block as *mut u64;
    // SAFETY: the caller's guarantee, on the thread that owns the array.
    let passes = unsafe {
        let at = words.add(PASSES);
        *at += 1;
        *at
    };
    // SAFETY: the key is this module's own, taking back the array it just handed over.
    if passes < DESTRUCTOR_PASSES
        && unsafe { libc::pthread_setspecific(synthetic_key(), block as *const _) } == 0
    {
        return;
    }
    // SAFETY: as above. Reconstituting the same `Box<[u64]>` `allocate` leaked, with
    // the same length.
    drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(words, WORDS)) });
}

/// A fresh array for this thread, filled in and leaked into the key.
#[cfg(not(target_os = "macos"))]
fn allocate() -> u64 {
    let mut words = vec![0u64; WORDS].into_boxed_slice();
    let base = words.as_ptr() as usize as u64;
    // Slot 0 is the array's own address: `pthread_self` is a load of it, and a guest
    // comparing two threads' identities compares these.
    words[slot::SELF] = base;
    // The guest's slot, not the host's: a guest reading `%gs:8` reads a number
    // and must read Darwin's numbering of it. See `crate::errno::error`.
    words[slot::ERRNO] = crate::errno::guest_location() as usize as u64;
    words[slot::STACK_GUARD] = STACK_CHK_GUARD;
    // Non-zero, so a pointer mangled through it is not the pointer itself, and stable
    // per thread, which is all `_pthread_ptr_mangle` requires.
    words[slot::PTR_MUNGE] = base.rotate_left(17) | 1;
    words[HOST_THREAD_POINTER] = host_thread_pointer();
    Box::leak(words);
    base
}

/// This thread's own thread pointer, as the host keeps it.
///
/// Zero where the register cannot be read, which is not a failure: the value is only
/// ever used to put back what [`DarwinThread`] replaced, and a boundary that does not
/// replace anything has nothing to put back.
pub fn host_thread_pointer() -> u64 {
    #[cfg(all(not(target_os = "macos"), target_arch = "aarch64"))]
    {
        let tp: u64;
        // SAFETY: `TPIDR_EL0` is readable and writable from EL0 on Linux, and this
        // reads a register without touching memory.
        unsafe {
            std::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags))
        };
        tp
    }
    #[cfg(all(not(target_os = "macos"), target_arch = "x86_64"))]
    {
        let tp: u64;
        // SAFETY: Linux keeps the thread pointer in `%fs`, whose slot zero holds the
        // base. One load, no writes.
        unsafe { std::arch::asm!("mov {}, fs:0", out(reg) tp, options(nostack, preserves_flags)) };
        tp
    }
    #[cfg(any(
        target_os = "macos",
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ))]
    {
        0
    }
}

/// Make one call into Darwin code with this thread's Darwin thread pointer installed.
///
/// `call_as_darwin!(libc::qsort(base, nel, width, compare))` is that call and
/// nothing else: the arguments are evaluated and passed as written, the value of
/// the call is the value of the macro, and the boundary lasts exactly as long as
/// the call. It costs nothing on a thread that is inside Darwin code already -
/// see [`entered`] - so it is cheap enough to write wherever the question comes
/// up rather than only where the answer is known to be no.
///
/// This is the form to reach for wherever *host* code is about to call back into
/// the guest: a comparator handed to the host's `qsort`, a block a queue worker
/// of this runtime's runs. Guest code reads `pthread_self`, `errno` and its stack
/// canary straight off the thread pointer and never calls anything to do it, so
/// on a thread this runtime started - which has the host's pointer, or none - it
/// reads whatever those offsets happen to address.
///
/// # Safety
///
/// The macro adds nothing to the call's own contract: an `unsafe` call still
/// needs an `unsafe` block around the macro.
#[macro_export]
macro_rules! call_as_darwin {
    ($($call:tt)*) => {{
        let _darwin = $crate::tls::DarwinThread::enter();
        $($call)*
    }};
}

/// Held for the duration of a call into **Darwin machine code** on a host that is not
/// Darwin, with the thread pointer swapped to this thread's synthetic array.
///
/// A guard rather than a pair of calls, so an unwind through the Darwin frame cannot
/// leave the host's thread pointer replaced - which would break every host library the
/// unwinder itself calls on the way out.
///
/// **Where this stands.** Three cases, and only one of them needs anything:
///
/// * **macOS.** Empty, and compiles away. The host's array is already Darwin's.
/// * **x86-64 Linux.** A real swap, and cheap. Darwin keeps its array in `%gs` and
///   Linux keeps its own in `%fs`, so the two never collide and the host's
///   thread-locals stay live across the boundary. Both sides are SysV besides, so
///   nothing else about the call needs bridging.
/// * **aarch64 Linux.** Nothing, deliberately - see [`can_enter_darwin`]. Darwin arm64
///   code reads `TPIDRRO_EL0`, which Linux does not let userspace write. Writing
///   `TPIDR_EL0` instead is not a partial answer but a strictly worse one: Darwin code
///   still reads the register it always read, and *host* thread-local access on this
///   thread - the allocator, the panic machinery, this guard's own `Drop` - now
///   resolves through an array that is not the host's. Measured, rather than reasoned
///   about: it segfaults. Giving native arm64 Darwin code its array on Linux needs its
///   `mrs` rewritten at load time, which is a project of its own and one nothing needs
///   yet; until then this refuses rather than half-does it.
///
/// None of the three is reached by an emulated guest, which reads [`tsd_base`] as a
/// value and never touches the register at all.
pub struct DarwinThread {
    /// What the register held before, and `None` when nothing was swapped.
    saved: Option<u64>,
}

/// Whether [`DarwinThread`] can actually give Darwin code the array it expects.
///
/// False on aarch64 Linux, for the reason [`DarwinThread`] gives. A caller about to
/// run native Darwin code should ask *before* running it, so the failure is a sentence
/// rather than a fault inside somebody else's prologue.
pub const fn can_enter_darwin() -> bool {
    cfg!(target_os = "macos") || cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// Whether entering Darwin code requires changing the thread pointer at all.
///
/// False on macOS, where the host's array is Darwin's already.
const fn swaps() -> bool {
    can_enter_darwin() && !cfg!(target_os = "macos")
}

impl DarwinThread {
    /// Enter Darwin code on this thread.
    ///
    /// Nothing to do when this thread is inside Darwin code already, and saying
    /// so is not only an optimisation: a nested guard that saved and restored
    /// would put the *host's* pointer back while the outer frame was still
    /// running Darwin code.
    pub fn enter() -> DarwinThread {
        if entered() {
            return DarwinThread { saved: None };
        }
        let saved = host_thread_pointer();
        set_thread_pointer(tsd_base());
        DarwinThread { saved: Some(saved) }
    }
}

impl Drop for DarwinThread {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            set_thread_pointer(saved);
        }
    }
}

/// Put the host's own thread pointer back for the duration of a call *out* of Darwin
/// code and into host code - the other half of the boundary, taken by a shim that
/// Darwin code called.
///
/// The inverse of [`DarwinThread`], with the same guard discipline for the same reason.
/// The host's pointer is read from the word past the end of the array, where
/// [`DarwinThread`] parked it and no Darwin slot index can reach it.
pub struct HostThread {
    saved: Option<u64>,
}

impl HostThread {
    pub fn enter() -> HostThread {
        if !swaps() {
            return HostThread { saved: None };
        }
        let saved = host_thread_pointer();
        let base = tsd_base() as usize as *const u64;
        // SAFETY: `base` is this thread's own array, which has `WORDS` words.
        let host = unsafe { *base.add(HOST_THREAD_POINTER) };
        if host == 0 {
            return HostThread { saved: None };
        }
        set_thread_pointer(host);
        HostThread { saved: Some(saved) }
    }
}

impl Drop for HostThread {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            set_thread_pointer(saved);
        }
    }
}

/// Point this thread's hardware thread pointer at `base`.
///
/// Only ever called where [`swaps`] is true, which is the one configuration where
/// changing it does not take the host's own thread-locals with it.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn set_thread_pointer(base: u64) {
    /// `ARCH_SET_GS`, from `<asm/prctl.h>`. Not in the `libc` crate.
    const ARCH_SET_GS: libc::c_int = 0x1001;
    // SAFETY: `arch_prctl` with `ARCH_SET_GS` sets this thread's `%gs` base to the
    // value passed. Linux keeps its own thread pointer in `%fs`, so this disturbs
    // nothing the host relies on.
    unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, base) };
    INSTALLED.with(|at| at.set(base));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
thread_local! {
    /// What this runtime last installed as this thread's Darwin thread pointer.
    ///
    /// The register itself can only be read back with another system call, and
    /// [`entered`] is asked once per call into Darwin code - the guest's
    /// allocator among them, which is every object this tree makes.
    ///
    /// Const-initialised and destructor-free deliberately: a `thread_local!`
    /// that registers one cannot be touched from a guest thread at all, which
    /// is the reason [`synthetic_key`] is a raw pthread key.
    static INSTALLED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Whether this thread's thread pointer is the Darwin array already, so a
/// boundary around a call into Darwin code has nothing to do.
///
/// True wherever nothing is swapped: on Darwin the host's array *is* the guest's,
/// and where [`can_enter_darwin`] is false no boundary can be raised anyway.
pub fn entered() -> bool {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        INSTALLED.with(|at| at.get()) == tsd_base()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        !swaps()
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn set_thread_pointer(_base: u64) {
    debug_assert!(!swaps(), "a host that swaps must say how");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_s_base_is_stable_and_its_own() {
        let mine = tsd_base();
        assert_ne!(mine, 0);
        assert_eq!(tsd_base(), mine, "a second ask is the same array");

        let theirs = std::thread::spawn(tsd_base)
            .join()
            .expect("a thread has one too");
        assert_ne!(theirs, mine, "two threads must not share an array");
    }

    /// Slot zero holding the base is what makes `pthread_self` a single load, and a
    /// guest comparing thread identities compares exactly this word.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn slot_zero_is_the_array_itself() {
        let base = tsd_base();
        // SAFETY: `base` is this thread's own array, live for the thread.
        assert_eq!(unsafe { *(base as usize as *const u64) }, base);
    }

    /// The canary a guest reads from slot 6 and the one it imports as
    /// `___stack_chk_guard` are the same variable under two spellings, and a function
    /// may set from one and check against the other.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_canary_slot_holds_the_imported_canary() {
        let base = tsd_base() as usize as *const u64;
        // SAFETY: slot 6 is within the array's `SLOTS` words.
        assert_eq!(unsafe { *base.add(slot::STACK_GUARD) }, STACK_CHK_GUARD);
    }

    /// `errno` reached through the array and `errno` reached through `__error()` are
    /// the same slot - the property that lets a guest read `%gs:8` and a guest
    /// calling `__error` see each other. Both are the *guest's* slot, which holds
    /// Darwin's numbering; the host's is a separate one this crate reconciles with.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_errno_slot_is_this_thread_s_errno() {
        let base = tsd_base() as usize as *const u64;
        // SAFETY: slot 1 is within the array.
        let at = unsafe { *base.add(slot::ERRNO) } as usize as *mut libc::c_int;
        assert_eq!(at, crate::errno::guest_location());
        assert_ne!(
            at,
            crate::errno::location(),
            "the host's numbering is not the guest's"
        );
    }

    /// Every direct key must be inside the array, because `pthread_getspecific` of
    /// one is a load rather than a call and a short array reads off the end.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_array_covers_every_direct_key() {
        assert!(SLOTS >= 256);
        assert!(
            HOST_THREAD_POINTER >= SLOTS,
            "the host's word is past every slot"
        );
        assert!(PASSES > HOST_THREAD_POINTER && WORDS > PASSES);
    }

    /// A boundary raised inside Darwin code must leave the array installed when it
    /// ends: the frame around it is still Darwin's, and putting the host's pointer
    /// back there is what a saving-and-restoring nested guard used to do.
    #[test]
    fn a_nested_boundary_leaves_the_array_installed() {
        let outer = DarwinThread::enter();
        assert!(entered());
        {
            let _inner = DarwinThread::enter();
        }
        assert!(entered(), "the inner boundary took the array away");
        drop(outer);
    }

    /// The boundary must leave the thread pointer exactly as it found it, including
    /// when the Darwin frame unwinds rather than returns.
    #[test]
    fn the_boundary_puts_the_thread_pointer_back() {
        let before = host_thread_pointer();
        {
            let _darwin = DarwinThread::enter();
        }
        assert_eq!(host_thread_pointer(), before);

        let unwound = std::panic::catch_unwind(|| {
            let _darwin = DarwinThread::enter();
            panic!("out of Darwin code");
        });
        assert!(unwound.is_err());
        assert_eq!(
            host_thread_pointer(),
            before,
            "an unwind must not leave the host's thread pointer replaced"
        );
    }

    /// The host's own thread pointer is never what moves, on any of the three hosts.
    ///
    /// On x86-64 Linux that is the whole design and not an incidental property:
    /// Darwin's array goes in `%gs` and Linux keeps its own in `%fs`, so host
    /// thread-locals - the allocator, the panic machinery, this guard's own `Drop` -
    /// stay live for the duration of the call. A host that cannot separate the two
    /// swaps nothing at all rather than half-doing it; see [`can_enter_darwin`].
    #[test]
    fn entering_darwin_code_leaves_the_host_s_own_thread_pointer_alone() {
        let before = host_thread_pointer();
        let guard = DarwinThread::enter();
        assert_eq!(host_thread_pointer(), before);
        drop(guard);
        assert_eq!(host_thread_pointer(), before);
    }

    /// And the other half: a host that says it can enter Darwin code must actually
    /// put this thread's array where Darwin code looks for it. Reading `%gs:0` is
    /// how the guest's own `pthread_self` is compiled, so this is the guest's view.
    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn darwin_code_finds_its_array_in_gs() {
        let _darwin = DarwinThread::enter();
        let base: u64;
        // SAFETY: `DarwinThread::enter` set this thread's `%gs` base to its array,
        // whose slot zero is live for as long as the thread is.
        unsafe {
            std::arch::asm!("mov {}, gs:0", out(reg) base, options(nostack, preserves_flags))
        };
        assert_eq!(
            base,
            tsd_base(),
            "slot zero of the array in `%gs` is the array"
        );
    }
}
