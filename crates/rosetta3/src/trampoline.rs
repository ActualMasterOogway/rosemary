//! Five bytes of x86-64 that turn a host function pointer back into an import.
//!
//! ## The cost this exists to remove
//!
//! Every intercepted call in this runtime has a *fast* form and a *slow* form,
//! and which one a call site gets is decided at lift time by
//! [`crate::abi::classify`]. A `call` whose target microlift can see — a
//! `__stubs` entry, a `call [rip+disp]` through a slot — is registered as a
//! fast import and becomes an inline native call with the arguments read from
//! wherever the guest's own ABI put them. Nothing is looked up at run time and
//! there is no VM exit at all.
//!
//! A call whose target microlift *cannot* see gets none of that. The guest
//! loaded a pointer — out of a slot, out of `dlsym`, out of a vtable — and
//! called through a register, so the only thing available at the call site is
//! the value itself. [`crate::vm::Vm::dispatch_indirect`] answers it by name:
//! hash the address to an [`ImportSite`], match the symbol through
//! [`crate::shims::dispatch`], look the signature up in [`crate::abi::table`],
//! walk its [`ArgType`](crate::abi::ArgType)s placing each argument, and call
//! through a widened prototype. The answer is the same every time and it is
//! recomputed on every call.
//!
//! That is not a corner. **Roblox and Starbound spend most of their CPU time in
//! it**: both resolve their own entry points with `dlsym` rather than importing
//! them, so essentially every host call they make misses the fast path that the
//! whole ABI seam was built to serve.
//!
//! ## What a trampoline is
//!
//! One entry of this arena is a single instruction:
//!
//! ```text
//!   T:  jmp   S          ; e9 rel32
//! ```
//!
//! `T` is **guest** memory, so it is lifted like any other guest code. `S` is an
//! address in the reservation above the arena that holds nothing, is never read,
//! and exists only to be *registered as an import* — see [`import_kind`], which
//! answers microlift's lift-time [`ImportRegistry`] query for it with the fast
//! import the symbol always deserved.
//!
//! So the trampoline's one instruction is a direct tail `jmp` to a registered
//! fast import, which is the case
//! `microlift::arch::x86_64::semantics::control_flow::emit_import_tail_jump`
//! already handles better than anything this runtime could: it pops the return
//! address, emits the inline call with the guest's arguments in place, and
//! returns to where the caller was. No symbol lookup, no signature walk, no
//! marshaller.
//!
//! ## Why a tail `jmp` and not a `call`
//!
//! Alignment is not the reason, and the real one is worth stating because it is
//! the whole correctness argument for the encoding. A `CallAbi`'s
//! [`ArgLocation::Stack`](microlift::core::abi::ArgLocation::Stack) offsets are
//! measured from the guest stack pointer *before the call pushes anything*, so
//! the seventh integer argument of a SysV call is at `rsp+0`. A trampoline that
//! did `call S; ret` would push a second return address underneath the
//! arguments and put every one of them eight bytes off — the same silent
//! one-slot shift [`crate::abi::marshal::stack_base`] exists to avoid, and just
//! as invisible.
//!
//! A tail `jmp` has no such problem, because microlift pops the return address
//! *before* the inline call: the stack is then in exactly the state a direct
//! call site would have left it, whichever way control arrived at `T`. A guest
//! that `call`s the pointer and a guest that tail-`jmp`s through it both end up
//! right, and neither costs an adjustment anybody has to remember.
//!
//! ## Where the address comes from: the slot, not the dispatch
//!
//! A trampoline only helps if the guest is *holding* one, so the pointer is
//! substituted at the point it is produced rather than recognised at the point
//! it is called:
//!
//! - **Import slots.** [`install`] rewrites the binding — the value the loader
//!   wrote into `__got`/`__la_symbol_ptr` — to name the trampoline instead of
//!   the host function. Every `mov rax, [rip+slot]; call rax` in the program
//!   then loads a trampoline without anything having to notice.
//! - **`dlsym`.** [`crate::dyld`] hands out the trampoline for a symbol that has
//!   one, which is the case this whole module exists for.
//!
//! **The fast-import registration is untouched.** A `call [rip+slot]` or a
//! `call` into a `__stubs` entry is matched by microlift on the *address*, and
//! it calls the host function directly without ever reading the slot — so the
//! static path keeps costing nothing and the slot's contents only matter to the
//! dynamic one. The two registrations are the same symbol reached two ways.
//!
//! [`crate::vm::Vm::dispatch_indirect`] keeps a trampoline lookup as a fallback,
//! for a host pointer that reached the guest by neither route.
//!
//! ## What it costs, honestly
//!
//! An indirect call that lands here is one region exit to enter the trampoline
//! and one to return out of it — the price any indirect call into guest code
//! pays. What goes away is the per-call symbol lookup and argument marshalling,
//! which is the part that showed up on the profile. A call site microlift *can*
//! see never comes here at all and keeps costing nothing.
//!
//! ## The one thing a substituted pointer changes
//!
//! A guest that hands an imported function pointer *to a host function* used to
//! hand over a real host address, and now hands over a guest one. Where the
//! signature declares the parameter a callback that is already handled — the
//! marshaller substitutes it — and [`host_behind`] makes the substitution the
//! identity it always was, by unwrapping the trampoline back to the host
//! function rather than building a thunk around it. Where the signature does
//! *not* declare it, the pointer crosses unexamined, which is the same hazard a
//! guest's own function pointer has always had in that position.
//!
//! ## What is deliberately not trampolined
//!
//! Only a symbol that classifies [`Classification::Fast`] gets an entry. That
//! excludes, by construction rather than by a list kept here:
//!
//! - **Shims.** A shimmed symbol exists precisely because calling the host's
//!   function would be wrong, and a trampoline calls the host's function.
//! - **Variadics and callback takers**, which a fast import cannot express.
//! - **Anything with no signature**, which stays a named refusal.
//! - **A traced symbol**, for the reason
//!   [`ImportPolicy::promotion_for`](crate::ImportPolicy) declines to promote
//!   one: an inlined call never reaches the seam, so watching it would report
//!   nothing.
//!
//! Everything excluded keeps the path it had. A trampoline is an optimisation
//! of a call that already worked, which is what makes `ROSETTA3_TRAMPOLINES=0`
//! a usable baseline rather than a debug crutch.
//!
//! [`ImportRegistry`]: microlift::core::bridge::ImportRegistry
//! [`Classification::Fast`]: crate::abi::Classification::Fast

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use microlift::core::FastImport;
use microlift::core::bridge::ImportKind;

use crate::abi::{Classification, FunctionAbi, Lookup};
use crate::program::{AddrMap, ImportSite};

/// `ROSETTA3_TRAMPOLINES=0` turns the whole thing off.
const ENV: &str = "ROSETTA3_TRAMPOLINES";

/// Bytes per entry. Five are used; the rest is `int3` padding, so that a walk
/// that peeks past the `jmp` reads something that is neither zero nor code.
const STRIDE: u64 = 16;

/// How many symbols may be trampolined.
///
/// Sized to be forgettable rather than to a measurement: an entry is created
/// once per *symbol* a program calls through a pointer, and the largest number
/// seen is in the low hundreds. The reservation is two of these, which at this
/// size is 256 KiB of address space and one page of it ever touched.
const SLOTS: usize = 8192;

/// Bytes in each half of the reservation.
const HALF: u64 = SLOTS as u64 * STRIDE;

/// Base of the whole reservation, or zero before the arena exists.
///
/// **One** atomic, not four, and that is a correctness property rather than
/// tidiness. The two halves are adjacent and constant-sized, so every extent is
/// `BASE`, `BASE + HALF` or `BASE + HALF * 2` — which means there is no window
/// in which a reader can see a published low bound against an unpublished high
/// one. Four separately-stored extents had exactly that window: [`contains`] is
/// on the hot path and reads without a lock, so it could observe a non-zero
/// `CODE_LO` and a still-zero `CODE_HI` and answer *no* for a trampoline the
/// guest is holding — which is an `UnknownIndirectCall` on a pointer that was
/// perfectly good.
///
/// Read on [`contains`], which [`crate::Program::is_guest_address`] asks at
/// every indirect call and answers "no" almost every time. One relaxed load
/// rejects a host address with no lock — the same shape as [`crate::dyld`]'s
/// extents and [`crate::shims::vmmap`]'s.
static BASE: AtomicU64 = AtomicU64::new(0);

/// One trampolined symbol.
struct Entry {
    /// The site the entry stands for, leaked so it can be handed out with a
    /// `'static` lifetime — the same thing [`crate::dyld`] does with a `dlsym`
    /// result, and for the same reason: the run loop wants an `&ImportSite` and
    /// the `Program` that owns the original is not in reach here.
    site: &'static ImportSite,
    /// Its signature, resolved once when the entry was made.
    abi: &'static FunctionAbi,
}

/// Everything that needs a lock: allocation, and the two directions of lookup.
///
/// Written once per symbol in a process's life and read at lift time only, so
/// a mutex is the whole of the concurrency design. The hot path — [`contains`]
/// — never touches it.
struct State {
    /// Entries in slot order.
    entries: Vec<Entry>,
    /// Host address to the trampoline that stands for it, so one symbol has one
    /// entry however many times it is asked for. Identity matters for the same
    /// reason it does in [`crate::Vm::thunk_for`]: the guest may compare two
    /// pointers it obtained separately, and a program that installs its own
    /// allocator by slot and then checks the slot must find what it wrote.
    by_target: AddrMap<u64>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Whether the run this is part of wants trampolines at all.
///
/// [`ImportPolicy::AlwaysSlow`](crate::ImportPolicy::AlwaysSlow) clears it, and
/// that is not tidiness: a trampoline's `jmp` reaches a synthetic import address
/// only the [import registry](crate::bridge::DynamicImports) can answer for, and
/// the slow policy installs no registry. It has to be a flag rather than a
/// parameter because the second place trampolines are made — an image the guest
/// `dlopen`s, from inside [`crate::dyld`] — has no policy in reach.
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Take trampolines out of the run, before any guest code has seen one.
pub fn disarm() {
    ARMED.store(false, Ordering::Relaxed);
}

/// Whether trampolines are in use.
fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ARMED.load(Ordering::Relaxed)
        && *ON.get_or_init(|| {
            std::env::var(ENV)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(1)
                != 0
        })
}

/// Entries made, slots substituted, and entries the guest actually reached.
///
/// The three numbers that say whether this is doing anything, and they are not
/// the same question. Entries made is how many symbols were *eligible*; slots
/// substituted is how many pointers the guest could now be holding; entries
/// reached is how many it called through — which is the only one that is a
/// claim about the program rather than about the table. A run with hundreds
/// substituted and none reached is a program that does not call through
/// pointers, and its cost is elsewhere.
///
/// Reported by [`crate::stats`] under `ROSETTA3_STATS`, and counted always: a
/// relaxed increment once per *symbol* is not measurable.
static MADE: AtomicU64 = AtomicU64::new(0);
static SUBSTITUTED: AtomicU64 = AtomicU64::new(0);
static REACHED: AtomicU64 = AtomicU64::new(0);

/// What the trampolines did this run, or `None` if there were none.
pub fn counts() -> Option<(u64, u64, u64)> {
    let made = MADE.load(Ordering::Relaxed);
    (made != 0).then(|| {
        (
            made,
            SUBSTITUTED.load(Ordering::Relaxed),
            REACHED.load(Ordering::Relaxed),
        )
    })
}

/// Whether `va` is a trampoline, and so guest code.
///
/// The one question on the hot path, and it is a single relaxed load because
/// [`crate::Program::is_guest_address`] asks it at every indirect call. The
/// upper bound is arithmetic rather than a second load — see [`BASE`], where
/// that is a correctness argument before it is a cost one.
pub fn contains(va: u64) -> bool {
    let base = BASE.load(Ordering::Relaxed);
    base != 0 && va >= base && va < base + HALF
}

/// Reserve the arena, once.
///
/// Two adjacent halves from one `mmap`, which is what makes the `jmp`'s
/// displacement a constant: every entry is exactly `HALF - 5` bytes below the
/// import address it names, and `HALF` is far inside the ±2 GiB an `e9` can
/// reach.
///
/// The code half is readable and writable *by this runtime* and never
/// executable — the guest is x86-64 and this CPU cannot run a byte of it, which
/// is the same decision every guest mapping is made under. The import half is
/// `PROT_NONE`: nothing must ever read it, and a stray transfer into it should
/// fault at the address that named it rather than find zeroes.
///
/// Called with [`STATE`] held, which is what makes "once" true. Unsynchronised,
/// two threads whose first pointer-call landed together would each `mmap` an
/// arena, each write entries into its own, and only one of them would be the one
/// [`contains`] answers for — so the loser's trampolines are addresses nothing
/// recognises as code, handed to the guest and stored in its slots. Two guest
/// threads starting at once is not a corner: it is what a thread pool does, and
/// [`crate::tls::bootstrap`] is reached on a new thread's first thread-local
/// read.
fn arena_locked() -> Option<u64> {
    let base = BASE.load(Ordering::Relaxed);
    if base != 0 {
        return Some(base);
    }
    // SAFETY: an anonymous private mapping of a fixed size at an address of the
    // kernel's choosing. Nothing is assumed about where it lands beyond the two
    // halves being adjacent, which one mapping guarantees.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            (HALF * 2) as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return None;
    }
    let base = base as usize as u64;
    // SAFETY: the upper half of the mapping just returned, page-aligned because
    // `HALF` is a multiple of the page size.
    unsafe {
        libc::mprotect(
            (base + HALF) as usize as *mut libc::c_void,
            HALF as usize,
            libc::PROT_NONE,
        );
    }
    // `int3` everywhere, so a decode that runs past an entry's `jmp` reads a
    // one-byte trap rather than the zeroes [`crate::region`] treats as garbage
    // and the padding of the *next* entry as an instruction.
    //
    // SAFETY: the code half of the mapping, writable and owned by this module.
    unsafe { std::ptr::write_bytes(base as usize as *mut u8, 0xCC, HALF as usize) };

    // Released after the fill: a reader that sees a non-zero base must find the
    // bytes already in place.
    BASE.store(base, Ordering::Release);
    Some(base)
}

/// The trampoline standing for `site`, creating it if this is the first ask.
///
/// `None` — meaning "call it the way you always did" — for every symbol a fast
/// import cannot express, which is the list in the module note. A caller must
/// already have established that `site.resolved` is *host* code; an import that
/// resolved into another guest image is a transfer, not a call out, and
/// [`crate::vm::Vm::call_site`] handles it.
pub fn for_site(site: &ImportSite) -> Option<u64> {
    if !enabled() || site.resolved == 0 {
        return None;
    }
    // The same three questions `ImportPolicy::promotion_for` asks of a static
    // call site, asked here because the answer must be the same: a trampoline is
    // the fast import a pointer-call was never able to have, so it may only
    // exist where the fast import itself would have been legal.
    if crate::trace::watching(&site.symbol) {
        return None;
    }
    if crate::abi::classify(site.tag, &site.symbol) != Classification::Fast {
        return None;
    }
    let Lookup::Known(abi) = crate::abi::table::lookup(site.tag, &site.symbol) else {
        return None;
    };

    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // Under the lock, so the reservation happens once however many threads
    // arrive together. See [`arena_locked`].
    let base = arena_locked()?;
    let state = guard.get_or_insert_with(|| State {
        entries: Vec::new(),
        by_target: AddrMap::default(),
    });
    if let Some(&existing) = state.by_target.get(&site.resolved) {
        return Some(existing);
    }
    let slot = state.entries.len();
    if slot >= SLOTS {
        // Out of arena. Not an error: the caller falls back to the marshaller,
        // which is what it did before this module existed.
        return None;
    }
    let va = base + slot as u64 * STRIDE;
    write_jmp(va);
    state.entries.push(Entry {
        site: Box::leak(Box::new(site.clone())),
        abi,
    });
    state.by_target.insert(site.resolved, va);
    MADE.fetch_add(1, Ordering::Relaxed);
    Some(va)
}

/// Encode `jmp rel32` at `va`, targeting the import address `HALF` bytes above.
///
/// The five bytes are written before the slot is published, and no other slot's
/// bytes are touched, so a concurrent lift of a *different* trampoline reads
/// only settled memory.
fn write_jmp(va: u64) {
    // `HALF` from the start of this entry, less the five bytes the instruction
    // itself occupies — a constant, since the two halves are the same size.
    let rel = (HALF - 5) as u32;
    let mut bytes = [0u8; 5];
    bytes[0] = 0xE9;
    bytes[1..].copy_from_slice(&rel.to_le_bytes());
    // SAFETY: `va` is the start of an unused slot in the code half of the arena
    // this module mapped read/write, so five bytes from it are in bounds and
    // owned here. No guest thread can hold this address yet — it is published by
    // the caller after this returns.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), va as usize as *mut u8, bytes.len()) };
}

/// The slot `va` belongs to, if `va` is the entry point of a trampoline.
///
/// Only the entry point, deliberately: the interior of an entry is padding, and
/// a request to execute it is a bug this runtime should report rather than
/// decode.
fn code_slot(va: u64) -> Option<usize> {
    if !contains(va) {
        return None;
    }
    let offset = va - BASE.load(Ordering::Relaxed);
    (offset.is_multiple_of(STRIDE)).then_some((offset / STRIDE) as usize)
}

/// The slot the import address `va` belongs to.
fn import_slot(va: u64) -> Option<usize> {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 || va < base + HALF || va >= base + HALF * 2 {
        return None;
    }
    let offset = va - (base + HALF);
    (offset.is_multiple_of(STRIDE)).then_some((offset / STRIDE) as usize)
}

/// Whatever `f` says about the entry in `slot`, if there is one.
fn with_entry<T>(slot: usize, f: impl FnOnce(&Entry) -> T) -> Option<T> {
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref()?.entries.get(slot).map(f)
}

/// The region to lift in order to execute the trampoline at `va`.
///
/// The buffer handed to the walk is **the one entry**, not the arena. That is
/// what keeps the `jmp`'s target out of the walk's reach without any special
/// case: the target is above the buffer's limit, so
/// [`crate::region`] declines to follow it exactly as it declines to follow any
/// branch out of the mapping it was given. It is also why nothing here has to
/// reason about the arena being written concurrently — sixteen bytes are copied
/// out and the walk never looks at another slot.
pub fn lift_region(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    let slot = code_slot(va)?;
    // An address in the arena that has not been handed out is not code. Asked
    // before the bytes are read so that `no guest code at …` stays the answer
    // for an address this runtime never published.
    with_entry(slot, |_| ())?;
    let mut bytes = [0u8; STRIDE as usize];
    // SAFETY: `code_slot` placed `va` at the start of a slot inside the code
    // half, so `STRIDE` bytes from it are within the mapping. The slot's five
    // instruction bytes were written before it was published and the rest is
    // the `int3` fill from `arena`, so nothing here is in flux.
    unsafe {
        std::ptr::copy_nonoverlapping(va as usize as *const u8, bytes.as_mut_ptr(), bytes.len())
    };
    // Counted here rather than at every call: a region is lifted once and then
    // cached, so this is "the guest reached this symbol through a pointer at
    // least once", which is the question worth answering.
    REACHED.fetch_add(1, Ordering::Relaxed);
    let region = crate::region::region_for(&bytes, va, va, false)?;
    let len = usize::try_from(region.range.end - va).ok()?;
    Some((region, bytes.get(..len)?.to_vec()))
}

/// How microlift should reach `va`, if `va` is a trampoline's import address.
///
/// The answer is always a fast import: [`for_site`] refuses to make an entry for
/// a symbol that could not have one, so an address in the import half either has
/// a placeable signature or does not exist.
pub fn import_kind(va: u64) -> Option<ImportKind> {
    let slot = import_slot(va)?;
    with_entry(slot, |entry| {
        ImportKind::Fast(FastImport {
            symbol: crate::abi::dynamic_symbol(entry.abi, entry.site.resolved),
            abi: crate::abi::guest_call_abi(),
        })
    })
}

/// The site behind a trampoline's import address.
///
/// Nothing on the fast path needs this: a registered fast import is inlined and
/// never reaches the run loop. It is here for the path that says so when
/// something has gone wrong — microlift declining to build the inline call and
/// falling back to an `ImportedFunctionCall` exit, which
/// [`crate::vm::Vm::service_import`] would otherwise answer with
/// `<unregistered 0x…>` instead of the symbol's name.
pub fn import_at(va: u64) -> Option<&'static ImportSite> {
    with_entry(import_slot(va)?, |entry| entry.site)
}

/// The host function a trampoline stands for, if `va` is one.
///
/// What makes substituting a slot's value invisible to everything that is not a
/// guest call. The marshaller replaces a guest function pointer with a thunk
/// before handing it to host code (see [`crate::abi::Callbacks`]); a trampoline
/// is a guest address, so without this a program that passes an imported
/// function to another host function — `atexit(cleanup)` where `cleanup` is
/// itself imported — would send the host out to arm64, into the JIT, through the
/// trampoline and back out again. Unwrapping restores the pointer the guest
/// would have passed before this module existed, which is also the one the host
/// may compare against its own.
pub fn host_behind(va: u64) -> Option<u64> {
    with_entry(code_slot(va)?, |entry| entry.site.resolved)
}

/// Point every eligible binding in `imports` at a trampoline, in `memory`.
///
/// The **relocation** is what moves, not the fast-import registration: a call
/// site microlift can see still calls the host function directly, and what
/// changes is only the value a guest reads when it loads the pointer itself. See
/// the module note.
///
/// Stub addresses are skipped — a `__stubs` entry is code, and overwriting the
/// first bytes of one would be rewriting the program rather than its imports.
/// So is an import that resolved into another **guest** image, which `is_guest`
/// answers: that is not a call out at all, and a trampoline for it would send a
/// guest-to-guest call through a fast import into memory this CPU cannot
/// execute. It is the same question
/// [`Program::lifter_config`](crate::Program::lifter_config) asks before
/// promoting a site, asked here for the same reason.
///
/// Returns how many bindings were substituted. A write that fails is skipped
/// rather than reported: a slot in a segment this runtime cannot write is a slot
/// that keeps the host address it already had, which is the behaviour every
/// import had before this existed.
pub fn install<'a>(
    imports: impl IntoIterator<Item = (u64, &'a ImportSite)>,
    is_guest: &dyn Fn(u64) -> bool,
    memory: &mut dyn crate::abi::GuestMemory,
) -> usize {
    if !enabled() {
        return 0;
    }
    let mut installed = 0;
    for (slot_va, site) in imports {
        if site.is_stub || is_guest(site.resolved) {
            continue;
        }
        let Some(va) = for_site(site) else {
            continue;
        };
        if memory.write(slot_va, &va.to_le_bytes()).is_ok() {
            installed += 1;
            SUBSTITUTED.fetch_add(1, Ordering::Relaxed);
        }
    }
    installed
}

/// `va` as a name, for a diagnostic. See [`crate::Program::locate`].
pub fn locate(va: u64) -> Option<String> {
    let slot = code_slot(va).or_else(|| import_slot(va))?;
    with_entry(slot, |entry| {
        format!("<trampoline for {}>", entry.site.symbol)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(symbol: &str, resolved: u64) -> ImportSite {
        ImportSite {
            symbol: symbol.to_string(),
            library: None,
            resolved,
            tag: 0,
            is_stub: false,
        }
    }

    /// Nothing is a trampoline until one is made, and the rejection is the
    /// answer `is_guest_address` gets for every host pointer in the process.
    #[test]
    fn an_address_outside_the_arena_is_not_one() {
        assert!(!contains(0));
        assert!(!contains(0x1000));
        assert!(lift_region(0x1000).is_none());
        assert!(import_kind(0x1000).is_none());
    }

    /// An unsatisfied import resolves to null, and a trampoline to null would be
    /// a `jmp` into a fast import whose address is zero.
    #[test]
    fn an_unresolved_import_gets_none() {
        assert!(for_site(&site("_strlen", 0)).is_none());
    }

    /// The whole exclusion list in one property: only a symbol that would have
    /// been a fast import gets an entry.
    #[test]
    fn only_a_fast_import_is_trampolined() {
        // No signature at all.
        assert!(for_site(&site("_no_such_symbol_anywhere", 0x1234)).is_none());
        // Variadic, and reached through a shim.
        assert!(for_site(&site("_printf", 0x1234)).is_none());
    }

    /// The entry is one `jmp rel32` whose displacement lands on the import
    /// address the registry answers for — which is the entire contract between
    /// the two halves of the arena.
    #[test]
    fn the_jump_lands_on_the_registered_import() {
        let Some(va) = for_site(&site("_strlen", 0x1234)) else {
            // `_strlen` has to be in the table for this to mean anything; if the
            // table ever stops describing it the other tests still hold.
            return;
        };
        assert!(contains(va));

        let mut bytes = [0u8; 5];
        // SAFETY: `va` is a published trampoline, so five bytes from it are the
        // instruction this module wrote.
        unsafe { std::ptr::copy_nonoverlapping(va as usize as *const u8, bytes.as_mut_ptr(), 5) };
        assert_eq!(bytes[0], 0xE9);
        let rel = i32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let target = (va + 5).wrapping_add(rel as i64 as u64);

        assert!(matches!(import_kind(target), Some(ImportKind::Fast(_))));
        assert_eq!(
            import_at(target).map(|s| s.symbol.as_str()),
            Some("_strlen")
        );
        // And the import address is not itself guest code, so a transfer to it
        // is the diagnostic it should be rather than a decode of `PROT_NONE`.
        assert!(!contains(target));
    }

    /// One symbol, one trampoline: the guest may compare two pointers it
    /// obtained separately and must get the answer it would have got natively.
    #[test]
    fn a_symbol_gets_one_entry() {
        let (a, b) = (
            for_site(&site("_strlen", 0x1234)),
            for_site(&site("_strlen", 0x1234)),
        );
        assert_eq!(a, b);
    }

    /// The lifted region is the `jmp` and nothing else — in particular the walk
    /// must not follow it into the import half, which is unreadable.
    #[test]
    fn the_region_is_the_jump_alone() {
        let Some(va) = for_site(&site("_strlen", 0x1234)) else {
            return;
        };
        let (region, code) = lift_region(va).expect("a published trampoline is code");
        assert_eq!(region.range, va..va + 5);
        assert_eq!(code, vec![0xE9, code[1], code[2], code[3], code[4]]);
        assert_eq!(region.starts, vec![va]);
    }

    /// Threads arriving together must all get trampolines out of **one**
    /// arena.
    ///
    /// The reservation used to be made outside the lock, so two first-callers
    /// each mapped one and only the last to publish was the one `contains`
    /// answered for. The loser's entries were then addresses that are not
    /// trampolines by the runtime's own test — written into guest slots and
    /// handed out by `dlsym`, to be met later as an indirect call to nothing.
    /// Not a corner: a thread pool's first thread-local reads arrive exactly
    /// this way, each through `dispatch_indirect`.
    ///
    /// Distinct symbols, so every thread has to *allocate* rather than hit
    /// `by_target` — which is the case that races.
    #[test]
    fn concurrent_first_callers_share_one_arena() {
        const NAMES: &[&str] = &[
            "_strlen", "_strcmp", "_memcpy", "_memset", "_strchr", "_strrchr", "_abs", "_atoi",
        ];
        // A barrier, so "together" is a fact rather than a hope: without one
        // the first thread usually finishes reserving before the second starts
        // and the race the test is about does not happen.
        let gate = std::sync::Barrier::new(NAMES.len());
        let made: Vec<u64> = std::thread::scope(|s| {
            let handles: Vec<_> = NAMES
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let gate = &gate;
                    s.spawn(move || {
                        gate.wait();
                        for_site(&site(name, 0x9000 + i as u64 * 0x10))
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect()
        });
        assert!(!made.is_empty(), "these symbols are all in the table");
        for va in made {
            assert!(
                contains(va),
                "{va:#x} was handed out and is not in the arena `contains` answers for"
            );
            assert!(lift_region(va).is_some(), "and it must be liftable");
        }
    }

    /// The padding inside an entry is not an entry point. A request to execute
    /// it is a bug worth reporting rather than decoding.
    #[test]
    fn the_interior_of_an_entry_is_not_code() {
        let Some(va) = for_site(&site("_strlen", 0x1234)) else {
            return;
        };
        assert!(lift_region(va + 1).is_none());
        assert!(lift_region(va + 5).is_none());
    }
}
