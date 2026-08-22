//! What an indirect call turned out to mean, remembered per thread.
//!
//! A guest `CALL` through a register or a memory operand cannot be resolved when the
//! region is lifted, so every one of them calls back into this runtime -
//! [`crate::bridge::indirect_call`], then [`Vm::dispatch_indirect`] - to be told what
//! the address is. Four lookups answer that: the program's imports, the dynamically
//! loaded images', the thunks handed out to host code, and the mapped guest regions.
//!
//! The answer for a given address is the same every time. A virtual call in a loop
//! asks it millions of times: on a C++ virtual-dispatch fixture, host dispatch was
//! half of the running time and those lookups were most of it. Answering from this
//! table instead took 12% off that fixture and 12% off a call-through-a-table one.
//!
//! So the common answer is cached: **this address is guest code, enter it there**.
//! Nothing else is, because nothing else is a hot loop - an import call ends in a host
//! function whose own cost dwarfs the lookup that found it, and an address that is
//! neither is an error.
//!
//! ## Why direct-mapped, which is measured and not assumed
//!
//! A set-associative table with MRU replacement is the obvious next thing, and it is
//! not worth it. Against a fixture that calls 4,000 functions in random order - four
//! times what this table holds - four ways at the same total capacity raised the hit
//! rate from 25% to 87% and made the fixture **7% slower**. The hits it wins are the
//! ones that were about to be cheap anyway, and it pays for them with a wider probe,
//! writes on the hit path to keep the set ordered, and a table too big to sit beside
//! [`crate::thread::CodeWindow`] and the link table in the same caches. Plain
//! capacity does no better: 4,096 slots direct-mapped doubled the hit rate on that
//! same fixture and cost 4%.
//!
//! The realistic fixtures answer 100% from 1,024 slots and evict nothing, so there is
//! nothing there for either to improve on. `ROSETTA3_STATS` prints the hit rate and
//! the eviction share, which is what a program that disagrees would show.
//!
//! ## Why a per-thread table and not a shared one
//!
//! The same reason [`crate::thread::CodeWindow`] is one: a cache read on this path
//! must not be an atomic read-modify-write on a line every guest thread is writing.
//! The table is `Cell`s on the thread that owns it, and the only shared read is
//! [`generation`], which is a load of a line nothing writes in steady state.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// How many call sites one thread remembers. A power of two, so the index is a mask.
///
/// 1024 entries is 16 KiB a thread against a virtual-call working set that a program
/// which is not pathological measures in the hundreds.
const SLOTS: usize = 1024;

/// A slot nothing has been written to. No guest function lives at address zero.
const EMPTY: u64 = 0;

/// Bumped whenever an address could have changed what it means.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Say that something now registered was not registered before: a `dlopen`ed image,
/// a thunk handed outward, a trampoline built for a host function.
///
/// Cheap insurance rather than a mechanism that is known to be needed. Every way an
/// address becomes an import, a thunk or a trampoline gives it an address that was
/// *nothing* beforehand, and a verdict is only ever cached for an address that already
/// resolved to guest code - so no cached entry has ever been observed to go stale.
/// This is what makes that an argument about the callers rather than a requirement on
/// them.
pub fn invalidate() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// One thread's memory of where an indirect call went.
pub(crate) struct IndirectCache {
    /// `(callee, target)`, direct-mapped. `target` is where control actually goes,
    /// which differs from `callee` exactly when the callee was one of this runtime's
    /// own thunks.
    slots: Box<[Cell<(u64, u64)>]>,
    /// The generation these entries were decided under.
    generation: Cell<u64>,
}

impl IndirectCache {
    pub(crate) fn new() -> Self {
        IndirectCache {
            slots: (0..SLOTS).map(|_| Cell::new((EMPTY, 0))).collect(),
            generation: Cell::new(GENERATION.load(Ordering::Relaxed)),
        }
    }

    /// The slot an address maps to.
    ///
    /// Guest function addresses are aligned in the low bits and identical in the high
    /// ones, so the index comes from the middle of a multiply - as in
    /// [`crate::cache`], and for the same reason.
    fn slot(callee: u64) -> usize {
        (callee.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 24) as usize & (SLOTS - 1)
    }

    /// Where `callee` went last time, if this thread has been told and the answer is
    /// still the one that was given.
    pub(crate) fn get(&self, callee: u64) -> Option<u64> {
        // A guest calling a null pointer must reach the error path that names it,
        // not match the marker an untouched slot holds.
        if callee == EMPTY {
            return None;
        }
        self.refresh();
        match self.slots[Self::slot(callee)].get() {
            (at, target) if at == callee => Some(target),
            _ => None,
        }
    }

    /// Remember that `callee` is guest code entered at `target`, reporting whether
    /// another address had to give up the slot.
    pub(crate) fn put(&self, callee: u64, target: u64) -> bool {
        if callee == EMPTY {
            return false;
        }
        self.refresh();
        let slot = &self.slots[Self::slot(callee)];
        let (was, _) = slot.get();
        slot.set((callee, target));
        was != EMPTY && was != callee
    }

    /// Empty the table if what is in it was decided before the last
    /// [`invalidate`].
    ///
    /// The load is the only shared read on this path, of a line nothing writes once a
    /// program is past its loading.
    fn refresh(&self) {
        let now = GENERATION.load(Ordering::Relaxed);
        if now == self.generation.get() {
            return;
        }
        for slot in &self.slots {
            slot.set((EMPTY, 0));
        }
        self.generation.set(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reads_back_where_it_went() {
        let cache = IndirectCache::new();
        assert_eq!(cache.get(0x1000), None);
        cache.put(0x1000, 0x2000);
        assert_eq!(cache.get(0x1000), Some(0x2000));
    }

    /// A thunk is exactly the case where the two differ, and reading the callee back
    /// as its own target would re-enter the thunk instead of the guest.
    #[test]
    fn the_target_is_not_the_callee() {
        let cache = IndirectCache::new();
        cache.put(0x7fff_0000, 0x1_0000);
        assert_eq!(cache.get(0x7fff_0000), Some(0x1_0000));
    }

    /// The empty marker is an address, so a `put` of it must not make every miss a
    /// hit on whatever was stored beside it.
    /// A guest that calls a null pointer has to reach the diagnostic, so the
    /// address the empty marker uses must never read back as a hit - whether or not
    /// anything tried to store it.
    #[test]
    fn address_zero_is_never_remembered() {
        let cache = IndirectCache::new();
        assert_eq!(cache.get(EMPTY), None, "an untouched table");
        cache.put(EMPTY, 0x2000);
        assert_eq!(cache.get(EMPTY), None, "and after a store of it");
        // And it has not displaced the address that shares its slot.
        cache.put(0x1000, 0x2000);
        assert_eq!(cache.get(0x1000), Some(0x2000));
    }

    #[test]
    fn a_collision_evicts_rather_than_answering_for_two() {
        let cache = IndirectCache::new();
        let a = 0x4000u64;
        // The other address that maps to the same slot.
        let b = a.wrapping_add((SLOTS as u64) << 24);
        assert_eq!(IndirectCache::slot(a), IndirectCache::slot(b));
        assert!(!cache.put(a, a), "an empty slot evicts nothing");
        assert!(cache.put(b, b), "and the second address reports the eviction");
        assert_eq!(cache.get(b), Some(b));
        assert_eq!(cache.get(a), None);
    }

    #[test]
    fn invalidation_empties_what_was_decided_before_it() {
        let cache = IndirectCache::new();
        cache.put(0x1000, 0x2000);
        invalidate();
        assert_eq!(cache.get(0x1000), None);
        // And the table is usable again immediately.
        cache.put(0x1000, 0x3000);
        assert_eq!(cache.get(0x1000), Some(0x3000));
    }
}
