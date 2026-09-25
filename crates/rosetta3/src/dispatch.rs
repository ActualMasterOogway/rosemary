use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use static_init::dynamic;

/// How many call sites one thread remembers. A power of two, so the index is a mask.
///
/// 1024 entries is 16 KiB a thread against a virtual-call working set that a program
/// which is not pathological measures in the hundreds.
const SLOTS: usize = 1024;

/// Overrides [`SLOTS`]. Rounded up to a power of two, floored at 64.
///
/// A knob because the argument above is a measurement on fixtures whose working set
/// fits, and a program whose does not looks different: Subnautica answers 77% from
/// 1024 slots and *evicts on 97% of its misses*, which is the shape the module note
/// says a program that disagrees would show.
pub const SLOTS_ENV: &str = "ROSETTA3_DISPATCH_SLOTS";

/// How many slots this run uses.
fn slots() -> usize {
    #[dynamic]
    static N: usize = match std::env::var(SLOTS_ENV) {
        Err(_) => SLOTS,
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n >= 64 => n.next_power_of_two(),
            _ => {
                eprintln!("rosetta3: {SLOTS_ENV}={v:?} is not an integer >= 64; using {SLOTS}");
                SLOTS
            }
        },
    };
    *N
}

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
    /// `slots.len() - 1`, which is the index mask. Held rather than read from
    /// [`slots`] so the mask is one load beside the table instead of a length
    /// check the bounds check would repeat.
    mask: usize,
    /// The generation these entries were decided under.
    generation: Cell<u64>,
}

impl IndirectCache {
    pub(crate) fn new() -> Self {
        let n = slots();
        IndirectCache {
            slots: (0..n).map(|_| Cell::new((EMPTY, 0))).collect(),
            mask: n - 1,
            generation: Cell::new(GENERATION.load(Ordering::Relaxed)),
        }
    }

    /// The slot an address maps to.
    ///
    /// Guest function addresses are aligned in the low bits and identical in the high
    /// ones, so the index comes from the middle of a multiply - as in
    /// [`crate::cache`], and for the same reason.
    fn slot(&self, callee: u64) -> usize {
        (callee.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 24) as usize & self.mask
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
        match self.slots[self.slot(callee)].get() {
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
        let slot = &self.slots[self.slot(callee)];
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
        // Searched for rather than computed: the table's width is a knob now, and
        // the point of the test is what a collision does, not which addresses
        // happen to collide at one width.
        let b = (a + 1..)
            .find(|&c| cache.slot(c) == cache.slot(a))
            .expect("some address shares a slot with `a`");
        assert!(!cache.put(a, a), "an empty slot evicts nothing");
        assert!(
            cache.put(b, b),
            "and the second address reports the eviction"
        );
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
