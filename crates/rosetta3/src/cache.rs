//! The code cache: guest addresses to native translations.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use machish::memory::Regions;
use static_init::dynamic;

use microlift::arch::x86_64::X86_64;
use microlift::arch::x86_64::context::RegisterContext;
use microlift::backend::CraneliftBackend;
use microlift::core::exit::ExitReason;
use microlift::core::link::BlockLinkEntry;
use microlift::core::{LifterConfig, lift_to_code};

use crate::program::Program;
use crate::vm::VmError;

/// One lifted, compiled guest region.
pub struct Translation {
    /// Guest address the region begins at.
    pub start: u64,
    /// One past the last guest byte it covers.
    pub end: u64,
    /// The addresses it may be entered at, ascending — microlift's own account of which
    /// arms its entry dispatch has, not the region's instruction grid.
    entries: Vec<u64>,
    /// The compiled entry point. Takes a pointer to the guest context and
    /// returns having written an exit reason into it.
    entry: unsafe extern "C" fn(*mut RegisterContext),
    /// The address a block-link row must name to reach this translation, as a plain
    /// integer — a row is read by *compiled code* and so cannot hold a Rust function
    /// pointer, only its bits.
    ///
    /// The same address as [`entry`](Self::entry), **except under a pinned link ABI**
    /// ([`crate::jit::pinned_link_abi`]), where a link enters the region's
    /// tail-convention body with the guest registers already in place and `entry` is
    /// the thunk that fills them from the context.
    code: u64,
    /// Keeps the JIT module — and therefore `entry`'s pages — mapped.
    _backend: CraneliftBackend,
}

// SAFETY: a `Translation` is immutable once built. `enter` touches only the
// caller's own `RegisterContext` and guest memory, whose races are the guest's own.
// `_backend` is held so the code stays mapped and its `Drop` runs once, when the
// last `Arc` goes, by which time no thread can be inside the code. Written out
// rather than derived because `CraneliftBackend` is neither `Send` nor `Sync`: we
// use it as a finished artefact, which is narrower than the type allows.
unsafe impl Send for Translation {}
unsafe impl Sync for Translation {}

impl Translation {
    /// Enter the translation, resuming at `ctx.rip`.
    pub fn enter(&self, ctx: &mut RegisterContext) -> ExitReason {
        ctx.image_base = self.start;
        ctx.exit_reason = ExitReason::None as u32;
        ctx.link_hit = 0;
        // SAFETY: `entry` is the finalized address of a Cranelift function whose
        // signature is `(*mut RegisterContext) -> ()` in the host C convention,
        // compiled by the backend this `Translation` owns and therefore still
        // mapped. The lifted code's only memory effects outside `ctx` are the
        // guest's own — the guest stack and its mapped images, all live for the
        // duration of the call.
        unsafe { (self.entry)(ctx as *mut RegisterContext) };
        // Under a pinned ABI a link is a branch: `link_hit` is never written and
        // the chain has already run itself out by the time this returns.
        match host_resolved_links() {
            true => follow_links(ctx),
            false => ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None),
        }
    }

    /// Whether this translation can be entered at `pc`.
    pub fn covers(&self, pc: u64) -> bool {
        self.entries.binary_search(&pc).is_ok()
    }

    /// The row a block-link table needs to name this translation at `pc`.
    pub fn link_row(&self, pc: u64) -> BlockLinkEntry {
        debug_assert!(
            self.covers(pc),
            "a link row would offer {pc:#x} to a translation that cannot be entered there"
        );
        BlockLinkEntry {
            guest_pc: pc,
            code: self.code,
            image_base: self.start,
            reserved: 0,
        }
    }
}

/// Whether a link comes back here to be taken, rather than being a branch the host
/// never sees. Read once; the ABI is fixed for the process.
fn host_resolved_links() -> bool {
    #[dynamic]
    static HOST_RESOLVED: bool = crate::jit::pinned_link_abi().is_none();
    *HOST_RESOLVED
}

/// Run out the chain of regions the last exit resolved for itself, and report how the
/// last one ended.
///
/// Which is also why the exits that *do* need the run loop must never be linked, and
/// they are not.
fn follow_links(ctx: &mut RegisterContext) -> ExitReason {
    while ctx.link_hit != 0 {
        // SAFETY: `link_hit` holds an address lifted code read out of the table
        // published in `ctx.link_table` — this thread's own, whose rows only
        // this thread writes, and which is alive for as long as the thread is.
        // A row's `code` is kept mapped by the `Arc` the window holds in the
        // same slot, and by `CodeCache::retired` if that slot is ever replaced.
        let row = unsafe { *(ctx.link_hit as *const BlockLinkEntry) };
        debug_assert_eq!(
            row.guest_pc, ctx.rip,
            "a link was taken for an address the region did not commit"
        );
        // Both counters, so that the two figures `docs/HOTSPOTS.md` is written
        // against — the entry *rate* and the exit *mix* — keep counting the
        // events they always counted.
        if crate::stats::enabled() {
            crate::stats::record_exit(crate::vm::classify(
                ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None),
                ctx.exit_code,
            ));
            crate::stats::record_linked_entry();
        }
        ctx.image_base = row.image_base;
        ctx.exit_reason = ExitReason::None as u32;
        ctx.link_hit = 0;
        // SAFETY: `code` is the entry point of a compiled translation, of the
        // same signature and with the same effects as `Translation::enter`'s
        // own call; it is only ever written by `CodeWindow::put`, from the
        // `Translation` that had already been entered at this address.
        let code: unsafe extern "C" fn(*mut RegisterContext) =
            unsafe { std::mem::transmute(row.code as usize as *const u8) };
        unsafe { code(ctx as *mut RegisterContext) };
    }
    ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None)
}

/// Every address a published translation can be entered at, as an open-addressed
/// table read without taking the cache's lock.
///
/// It stands in for a walk. Regions overlap, so a map keyed by start address can only
/// answer "who can execute `pc`" by scanning back as far as the widest region reaches
/// and asking each candidate, and one wide region makes that scan thousands of entries
/// long: on Roblox the scan and the `BTreeMap` navigation under it were 27% of running
/// time. A probe here is one dependent load.
struct EntryIndex {
    table: AtomicPtr<Table>,
    /// Addresses in the live table. Written only by a lift, which holds the cache's
    /// write lock and is therefore the only writer.
    live: AtomicUsize,
}

struct Table {
    mask: u64,
    slots: Box<[Slot]>,
}

struct Slot {
    pc: AtomicU64,
    translation: AtomicPtr<Translation>,
}

/// A slot nothing has been written to. No guest address is `u64::MAX`.
const EMPTY: u64 = u64::MAX;

/// Guest addresses are page- and instruction-aligned in the low bits and identical in
/// the high ones, so the index is taken from the *middle* of a multiply.
fn mix(pc: u64) -> u64 {
    pc.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 24
}

impl Table {
    fn empty(capacity: usize) -> Box<Table> {
        let capacity = capacity.next_power_of_two();
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || Slot {
            pc: AtomicU64::new(EMPTY),
            translation: AtomicPtr::new(std::ptr::null_mut()),
        });
        Box::new(Table {
            mask: capacity as u64 - 1,
            slots: slots.into_boxed_slice(),
        })
    }

    fn get(&self, pc: u64) -> Option<*const Translation> {
        let mut at = mix(pc) & self.mask;
        loop {
            let slot = &self.slots[at as usize];
            match slot.pc.load(Ordering::Acquire) {
                EMPTY => return None,
                seen if seen == pc => {
                    return Some(slot.translation.load(Ordering::Acquire).cast_const());
                }
                _ => at = (at + 1) & self.mask,
            }
        }
    }

    /// Write `pc`, reporting whether the address was new. Writer side only.
    fn put(&self, pc: u64, translation: *const Translation) -> bool {
        let mut at = mix(pc) & self.mask;
        loop {
            let slot = &self.slots[at as usize];
            match slot.pc.load(Ordering::Relaxed) {
                EMPTY => {
                    // The pointer goes in first, so a reader that sees the address
                    // sees a translation to go with it.
                    slot.translation
                        .store(translation as *mut Translation, Ordering::Release);
                    slot.pc.store(pc, Ordering::Release);
                    return true;
                }
                // A re-lift of the same address. Either pointer answers for it.
                seen if seen == pc => {
                    slot.translation
                        .store(translation as *mut Translation, Ordering::Release);
                    return false;
                }
                _ => at = (at + 1) & self.mask,
            }
        }
    }
}

impl EntryIndex {
    fn new() -> Self {
        EntryIndex {
            table: AtomicPtr::new(Box::into_raw(Table::empty(1024))),
            live: AtomicUsize::new(0),
        }
    }

    fn table(&self) -> &'static Table {
        // SAFETY: every table this pointer has ever held was leaked by `Box::into_raw`
        // and is never freed, precisely so that a reader still probing the table a
        // grow displaced keeps reading a live allocation.
        unsafe { &*self.table.load(Ordering::Acquire) }
    }

    /// A translation that can be entered at `pc`, or `None` if this table does not
    /// know of one — which a caller must read as "ask the regions", not as "there is
    /// none": a lift racing this probe publishes after it.
    fn get(&self, pc: u64) -> Option<*const Translation> {
        self.table().get(pc)
    }

    /// Publish `translation` under every address it may be entered at.
    ///
    /// The caller holds the cache's write lock, which is what makes the stores below
    /// the only ones in flight.
    fn publish(&self, entries: &[u64], translation: *const Translation) {
        self.grow_for(entries.len());
        let table = self.table();
        let mut added = 0;
        for &pc in entries {
            added += usize::from(table.put(pc, translation));
        }
        self.live.fetch_add(added, Ordering::Relaxed);
    }

    /// Keep the table under half full, so a probe of a missing address always reaches
    /// an empty slot rather than running the whole array.
    fn grow_for(&self, extra: usize) {
        let table = self.table();
        let wanted = self.live.load(Ordering::Relaxed) + extra;
        if wanted * 2 <= table.slots.len() {
            return;
        }
        let bigger = Table::empty(wanted * 4);
        // Rehashed out of the old table rather than rebuilt from the regions: nothing
        // is ever removed from either, so the old table is exactly the set to keep.
        for slot in &table.slots {
            let pc = slot.pc.load(Ordering::Relaxed);
            if pc != EMPTY {
                bigger.put(pc, slot.translation.load(Ordering::Relaxed));
            }
        }
        // The displaced table is leaked. Capacity quadruples, so everything ever
        // leaked together is a third of the live table.
        self.table.store(Box::into_raw(bigger), Ordering::Release);
    }
}

/// Translations, keyed by the guest address each begins at.
///
/// A hit is a read and a lift is a write, at a ratio of thousands to one, so the map
/// is behind an `RwLock` rather than a mutex — and a compile runs outside the lock
/// entirely, which is what lets two guest threads lift at once instead of one waiting
/// out the other's Cranelift run.
pub struct CodeCache {
    published: RwLock<Published>,
    /// The lock-free half of [`published`](Self::published): what a hit reads.
    index: EntryIndex,
    /// Everything microlift needs to know before it lifts anything: the registered
    /// import addresses and the read-only segment hints. Immutable, so a lift reads it
    /// without taking anything.
    config: LifterConfig,
}

/// The half of the cache a lift mutates.
struct Published {
    regions: BTreeMap<u64, Arc<Translation>>,
    /// The widest `end - start` any translation in [`regions`](Self::regions) has,
    /// which is how far below `pc` [`CodeCache::lookup`] has to look. Without it
    /// the scan for an address no region covers — the case that then lifts — walks
    /// every region below it, and that walk is 2.2% of `docs/HOTSPOTS.md`.
    max_span: u64,
    /// Translations displaced from [`regions`](Self::regions) by a re-lift of the same
    /// address, kept so their compiled code stays mapped.
    retired: Vec<Arc<Translation>>,
}

// SAFETY: the one thing keeping a cache off a second thread is the raw pointer
// inside `LifterConfig`'s `DynamicSymbol`s — the *host address* a fast import
// resolved to. It is a function address, cloned into each lift and never
// dereferenced on this side; the code that eventually calls it is compiled arm64
// doing what any thread may do with a function pointer. The translations beside
// it carry their own claim (see `Translation` above). `Sync` on top of `Send`
// because the lock is now inside the cache: threads share one by reference.
unsafe impl Send for CodeCache {}
unsafe impl Sync for CodeCache {}

impl CodeCache {
    /// A cache that lifts with `config`.
    pub fn new(config: LifterConfig) -> Self {
        CodeCache {
            published: RwLock::new(Published {
                regions: BTreeMap::new(),
                max_span: 0,
                retired: Vec::new(),
            }),
            index: EntryIndex::new(),
            config,
        }
    }

    /// How many regions have been translated.
    pub fn len(&self) -> usize {
        self.read().regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().regions.is_empty()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Published> {
        self.published.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Published> {
        self.published.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The translation that can execute `pc`, lifting one if necessary.
    pub fn get_or_lift<M: Regions>(
        &self,
        program: &Program<M>,
        pc: u64,
    ) -> Result<Arc<Translation>, VmError> {
        if let Some(hit) = self.lookup(pc) {
            return Ok(hit);
        }

        let selecting = crate::stats::enabled().then(std::time::Instant::now);
        // A region always begins at the address it was made for, so its cache
        // key is `pc` and two entry points into one function are two keys.
        let (region, bytes) = program
            .lift_region(pc)
            .ok_or(VmError::NoCode { pc, from: None })?;
        if let Some(selecting) = selecting {
            crate::stats::record_select(selecting.elapsed());
        }
        let start = region.range.start;

        // How long one region took to compile, when tracing. Not curiosity. A
        // big region is not slow in proportion to its size.
        let started = crate::shims::vmmap::tracing().then(std::time::Instant::now);

        // Where control can arrive from outside without any lift knowing: a
        // function reached only through a pointer. See the module note.
        let mut declared = program.function_starts_in(region.range.clone());
        declared.push(pc);
        declared.sort_unstable();
        declared.dedup();

        // A region that will not compile is refused outright.
        let compiled = match compile_region(&self.config, &bytes, &region, &declared) {
            Ok(compiled) => compiled,
            Err(source) => {
                crate::stats::record_refused();
                if crate::shims::vmmap::tracing() {
                    let head: Vec<String> =
                        bytes.iter().take(64).map(|b| format!("{b:02x}")).collect();
                    eprintln!(
                        "rosetta3: region {start:#x}..{:#x} would not compile, bytes {}",
                        region.range.end,
                        head.join(" ")
                    );
                }
                return Err(VmError::Lift {
                    pc,
                    start,
                    source,
                    at: program.locate(start),
                });
            }
        };
        let end = region.range.end;

        if let Some(started) = started {
            let took = started.elapsed();
            // Only the ones worth reading. A warm program lifts thousands of
            // regions and a line each would bury the one that matters.
            if took.as_millis() >= 50 {
                eprintln!(
                    "rosetta3: compiled {} ({start:#x}..{end:#x}, {} instruction(s), \
                     {} byte(s)) in {took:.1?}",
                    program
                        .locate(start)
                        .unwrap_or_else(|| "generated code".to_string()),
                    region.instructions(),
                    end - start,
                );
            }
        }

        // SAFETY: `compile` returns the finalized entry of a function whose
        // signature the lifter fixed as `fn(*mut RegisterContext)` in the host
        // calling convention.
        let entry: unsafe extern "C" fn(*mut RegisterContext) =
            unsafe { std::mem::transmute(compiled.code) };

        debug_assert!(
            compiled.entries.binary_search(&pc).is_ok(),
            "translation {start:#x}..{end:#x} cannot be entered at the {pc:#x} \
             it was lifted for"
        );
        // Where this region's compiled code went, so a fault inside it can be
        // named as a *guest* range rather than as an address in an anonymous
        // mapping. See [`crate::fault`]. The *body*, not the entry.
        crate::fault::placed(
            compiled.link_target as usize as u64,
            start,
            end,
            pc,
            &compiled.map,
        );
        crate::lldb::register(
            compiled.link_target as usize as u64,
            start,
            end,
            &program
                .locate(start)
                .unwrap_or_else(|| "generated code".to_string()),
            &compiled.guest_bytes,
            &compiled.map,
        );

        let translation = Arc::new(Translation {
            start,
            end,
            entries: compiled.entries,
            entry,
            code: compiled.link_target as usize as u64,
            _backend: compiled.backend,
        });
        // Two threads that missed on the same address both compiled it; the loser's
        // translation is retired rather than dropped, because its code has already
        // been handed to `crate::fault` and `crate::lldb` and must stay mapped.
        let mut published = self.write();
        published.max_span = published.max_span.max(end.saturating_sub(start));
        if let Some(replaced) = published.regions.insert(start, Arc::clone(&translation)) {
            published.retired.push(replaced);
        }
        self.index
            .publish(&translation.entries, Arc::as_ptr(&translation));
        Ok(translation)
    }

    /// An existing translation covering `pc`, if there is one.
    fn lookup(&self, pc: u64) -> Option<Arc<Translation>> {
        if let Some(hit) = self.index.get(pc) {
            // SAFETY: the pointer was taken from an `Arc` this cache put in
            // `regions`, and neither `regions` nor `retired` ever gives one up — a
            // replaced translation is retired, not dropped. So the strong count is at
            // least one for as long as `&self` is borrowed, which is what makes an
            // increment off a bare pointer legal here and nowhere else.
            return Some(unsafe {
                Arc::increment_strong_count(hit);
                Arc::from_raw(hit)
            });
        }
        self.scan(pc)
    }

    /// [`lookup`](Self::lookup) the authoritative way: what the index is a cache of.
    ///
    /// Only a miss reaches this, and a miss is on its way to a lift — except for the
    /// one race the index cannot cover, a probe that overtook another thread's
    /// publish.
    fn scan(&self, pc: u64) -> Option<Arc<Translation>> {
        let published = self.read();
        // Regions may overlap, so the answer is not `range(..=pc).next_back()` —
        // but a region that could cover `pc` begins no earlier than `max_span`
        // before it, which bounds the walk.
        published
            .regions
            .range(pc.saturating_sub(published.max_span)..=pc)
            .rev()
            .find(|(_, t)| t.covers(pc))
            .map(|(_, t)| Arc::clone(t))
    }
}

/// Lift and compile `region`'s `bytes` into native code.
fn compile_region(
    config: &microlift::core::LifterConfig,
    bytes: &[u8],
    region: &crate::region::Region,
    declared: &[u64],
) -> microlift::Result<Compiled> {
    let clock = crate::stats::enabled().then(std::time::Instant::now);
    crate::stats::record_extent(region.range.start, region.range.end);

    // Cheap.
    // Cheap: the two fields with a program's worth of data in them are behind
    // `Arc`s, so this copies pointers. It did not always, and that clone was 7%
    // of a process's wall clock.
    let mut config = config.clone();
    // Declared per region rather than once, because the answer is per region.
    config.resume_points = declared.to_vec();
    // Which of these bytes are code.
    config.block_ranges = region.blocks.clone();

    let lifted = lift_to_code::<X86_64>(bytes, region.range.start, config)?;
    let entries = lifted.resume_points.clone();
    let lift_took = clock.map(|c| c.elapsed());
    let mut backend = crate::jit::backend()?;
    let (region_code, map) = backend.compile_lifted_region(lifted)?;
    if let (Some(clock), Some(lift)) = (clock, lift_took) {
        crate::stats::record_lift(
            region.instructions(),
            bytes.len(),
            lift,
            clock.elapsed() - lift,
        );
    }
    Ok(Compiled {
        backend,
        code: region_code.entry,
        link_target: region_code.link_target,
        map,
        guest_bytes: crate::lldb::enabled()
            .then(|| bytes.to_vec())
            .unwrap_or_default(),
        entries,
    })
}

/// One region through the lifter and the compiler, with the one fact about it
/// that only the lift knows.
struct Compiled {
    backend: microlift::backend::CraneliftBackend,
    code: *const u8,
    /// What a block-link row names — see [`Translation::code`].
    link_target: *const u8,
    /// Host offset → guest instruction, so a fault inside this code can name
    /// the instruction rather than the region. See [`crate::fault`].
    map: microlift::core::srcloc::GuestCodeMap,
    /// Guest bytes retained only so the opt-in LLDB listing can show x86-64.
    guest_bytes: Vec<u8>,
    /// Where the compiled code may be entered — see [`Translation::covers`].
    entries: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table never dereferences what it stores, so a test may store anything.
    fn marker(n: u64) -> *const Translation {
        n as usize as *const Translation
    }

    #[test]
    fn probes_find_every_published_address() {
        let index = EntryIndex::new();
        // Page-aligned and closely spaced, the two shapes real entry points take.
        let addresses: Vec<u64> = (0..4096u64)
            .map(|n| 0x1_0000_0000 + n * 0x1000)
            .chain((0..4096u64).map(|n| 0x2_0000_0000 + n * 16))
            .collect();
        for (n, &pc) in addresses.iter().enumerate() {
            index.publish(&[pc], marker(n as u64 + 1));
        }
        for (n, &pc) in addresses.iter().enumerate() {
            assert_eq!(index.get(pc), Some(marker(n as u64 + 1)), "{pc:#x}");
        }
        assert_eq!(index.get(0x3_0000_0000), None);
        assert_eq!(index.get(0x1_0000_0000 + 8), None);
    }

    #[test]
    fn a_relift_replaces_the_address_rather_than_filling_a_second_slot() {
        let index = EntryIndex::new();
        index.publish(&[0x1000, 0x2000], marker(1));
        index.publish(&[0x1000], marker(2));
        assert_eq!(index.get(0x1000), Some(marker(2)));
        assert_eq!(index.get(0x2000), Some(marker(1)));
        assert_eq!(index.live.load(Ordering::Relaxed), 2);
    }
}
