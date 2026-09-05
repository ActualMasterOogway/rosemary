//! Catching a block freed by an allocator that did not make it, at the free.
//!
//! A guest that ships an allocator has two of them once this runtime is in the
//! picture, and `interpose::adopt_guest_allocator` is the claim that every
//! pointer crossing between them goes through a registry. This is what tests
//! the claim: the guest's `free` is replaced by one that asks the guest's own
//! allocator whether it owns the pointer first, and says where the call came
//! from when it does not.
//!
//! Worth the machinery because of what getting it wrong looks like without it.
//! mimalloc accepts a foreign pointer, works out which of its pages "owns" it
//! from the pointer's own bits, and pushes it onto that page's free list - so
//! the list now has a live block on it and the fault is thousands of
//! allocations later, on another thread, in code that has nothing to do with
//! the free. Every report of this bug so far has been that fault.
//!
//! The foreign free is **not performed**. Leaking the block is the lesser
//! wrong: the point of the lever is to find out whether the crossing is the
//! cause, and a run that leaks and survives says so where one that leaks and
//! still corrupts says the opposite.
//!
//! x86-64 only, because the trampoline that recovers the caller's return
//! address is written in its calling convention. Elsewhere the lever is absent
//! rather than silently doing nothing.

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use machish::memory::Regions;
use machish::report::LinkReport;

/// Set to check every free the guest makes against the guest's own allocator.
pub const ENV: &str = "ROSETTA3_CHECK_GUEST_FREE";

pub fn requested() -> bool {
    std::env::var_os(ENV).is_some_and(|v| v != "0")
}

/// The guest's real `free`, and the predicate that says whether it owns a
/// pointer. Written once, before any guest code runs.
static REAL_FREE: AtomicUsize = AtomicUsize::new(0);
static OWNS: AtomicUsize = AtomicUsize::new(0);
static REAL_REALLOC: AtomicUsize = AtomicUsize::new(0);
static FOREIGN: AtomicU64 = AtomicU64::new(0);
/// Proof the trampoline is on the path at all: a check that is never called
/// reports the same "no crossings" a clean run does.
static ANNOUNCED: std::sync::Once = std::sync::Once::new();

/// Return addresses already reported, so a crossing on a hot path says its
/// piece once rather than once per call.
static SEEN: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

/// `free(p)`, with `ra` appended by the trampoline.
/// Whether the guest's allocator made the block at `p`, reporting `ra` when it
/// did not. `false` means the caller must not hand it over.
fn owned(p: *mut c_void, ra: u64, what: &str) -> bool {
    ANNOUNCED.call_once(|| {
        eprintln!("rosetta3: heapcheck: the check is live - first {what} from {ra:#x}");
    });
    let owns: unsafe extern "C" fn(*const c_void) -> bool =
        // SAFETY: `OWNS` holds the address of the guest's
        // `mi_is_in_heap_region`, whose signature this is.
        unsafe { std::mem::transmute(OWNS.load(Ordering::Relaxed)) };
    // SAFETY: asking an allocator whether an address is one of its own is
    // defined for any address; that is the whole point of the call.
    if unsafe { owns(p) } {
        return true;
    }
    FOREIGN.fetch_add(1, Ordering::Relaxed);
    if SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(ra) {
        eprintln!(
            "rosetta3: heapcheck: {what}({p:?}) from {ra:#x} - the guest's allocator does not own \
             this block, so it came from somewhere else in this process. Left alone rather than \
             handed over."
        );
        eprintln!("rosetta3: heapcheck:   {ra:#x} is {}", where_is(ra));
    }
    false
}

/// `free(p)`, with `ra` appended by the trampoline.
unsafe extern "C" fn checked_free(p: *mut c_void, ra: u64) {
    if p.is_null() || !owned(p, ra, "free") {
        return;
    }
    let real: unsafe extern "C" fn(*mut c_void) =
        // SAFETY: `REAL_FREE` holds the guest's own `free`.
        unsafe { std::mem::transmute(REAL_FREE.load(Ordering::Relaxed)) };
    // SAFETY: the allocator has just said the block is its own.
    unsafe { real(p) };
}

/// `realloc(p, size)`. It frees too, and a foreign pointer reaching it is the
/// same crossing one step removed - so the block is copied out rather than
/// handed over, which is the only answer that keeps the caller's data.
unsafe extern "C" fn checked_realloc(p: *mut c_void, size: usize, ra: u64) -> *mut c_void {
    let real: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void =
        // SAFETY: `REAL_REALLOC` holds the guest's own `realloc`.
        unsafe { std::mem::transmute(REAL_REALLOC.load(Ordering::Relaxed)) };
    if p.is_null() || owned(p, ra, "realloc") {
        // SAFETY: either a plain allocation or a block the allocator owns.
        return unsafe { real(p, size) };
    }
    // SAFETY: a fresh block of `size`, filled from the foreign one. The old
    // block is left alone: this allocator cannot free it and the other one is
    // not reachable from here.
    unsafe {
        let fresh = real(std::ptr::null_mut(), size);
        if !fresh.is_null() {
            std::ptr::copy_nonoverlapping(p.cast::<u8>(), fresh.cast::<u8>(), size);
        }
        fresh
    }
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl rosetta3_checked_free",
    "rosetta3_checked_free:",
    // The return address is still at the top of the stack, so it becomes the
    // second argument and the tail jump leaves the frame exactly as `free`
    // would have left it.
    "mov rsi, [rsp]",
    "jmp {inner}",
    inner = sym checked_free,
);

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl rosetta3_checked_realloc",
    "rosetta3_checked_realloc:",
    "mov rdx, [rsp]",
    "jmp {inner}",
    inner = sym checked_realloc,
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn rosetta3_checked_free();
    fn rosetta3_checked_realloc();
}

/// Where each guest image landed, so a return address can be named without
/// taking the loader's lock from inside a free.
static IMAGES: Mutex<Vec<(String, u64, u64)>> = Mutex::new(Vec::new());

fn where_is(addr: u64) -> String {
    let images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    for (name, lo, hi) in images.iter() {
        if addr >= *lo && addr < *hi {
            let short = name.rsplit('/').next().unwrap_or(name);
            return format!("{short}+{:#x}", addr - lo);
        }
    }
    "in no guest image - so the caller is this runtime or a host library".to_string()
}

/// Every name that hands a block back to the allocator.
const FREES: &[&str] = &["free", "mi_free"];
const REALLOCS: &[&str] = &["realloc", "mi_realloc"];

/// How many foreign frees were caught, for the line printed at exit.
pub fn foreign_count() -> u64 {
    FOREIGN.load(Ordering::Relaxed)
}

/// Replace the guest's `free` with the checking one.
///
/// Call after [`crate::interpose::apply`], so the address recovered is the
/// allocator the guest's code actually ends up calling.
#[cfg(target_arch = "x86_64")]
pub fn install<M: Regions>(
    report: &mut LinkReport,
    memory: &mut M,
) -> Result<usize, machish::Error> {
    if !requested() {
        return Ok(0);
    }
    let resolved = |name: &str| {
        report
            .images
            .iter()
            .flat_map(|image| &image.bindings)
            .find(|b| darwin::base_name(&b.symbol) == name && b.resolved != 0)
            .map(|b| b.resolved)
    };
    let Some(owns) = resolved("mi_is_in_heap_region") else {
        eprintln!(
            "rosetta3: {ENV}: the guest imports no `mi_is_in_heap_region`, so there is nothing to \
             ask which blocks are its own - no check installed"
        );
        return Ok(0);
    };
    let Some(free) = FREES.iter().find_map(|n| resolved(n)) else {
        return Ok(0);
    };
    let realloc = REALLOCS.iter().find_map(|n| resolved(n));
    *IMAGES.lock().unwrap_or_else(|e| e.into_inner()) = report
        .images
        .iter()
        .map(|i| {
            let end = i
                .segments
                .iter()
                .map(|s| s.vmaddr.wrapping_add(s.vmsize))
                .max()
                .unwrap_or(i.base);
            (i.name.clone(), i.base, end)
        })
        .collect();
    OWNS.store(owns as usize, Ordering::Relaxed);
    REAL_FREE.store(free as usize, Ordering::Relaxed);
    REAL_REALLOC.store(realloc.unwrap_or(0) as usize, Ordering::Relaxed);

    let checked_free = rosetta3_checked_free as *const () as u64;
    let checked_realloc = rosetta3_checked_realloc as *const () as u64;
    let mut rebound = 0;
    for image in &mut report.images {
        for binding in &mut image.bindings {
            let name = darwin::base_name(&binding.symbol);
            let replacement = if FREES.contains(&name) && binding.resolved == free {
                checked_free
            } else if REALLOCS.contains(&name) && Some(binding.resolved) == realloc {
                checked_realloc
            } else {
                continue;
            };
            memory.write(binding.slot_va, &replacement.to_le_bytes())?;
            binding.resolved = replacement;
            rebound += 1;
        }
    }
    eprintln!(
        "rosetta3: {ENV}: {rebound} free binding(s) now checked against the guest's allocator"
    );
    Ok(rebound)
}

#[cfg(not(target_arch = "x86_64"))]
pub fn install<M: Regions>(
    _report: &mut LinkReport,
    _memory: &mut M,
) -> Result<usize, machish::Error> {
    Ok(0)
}
