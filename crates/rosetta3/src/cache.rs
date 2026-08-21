//! The code cache: guest addresses to native translations.

use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use machish::memory::Regions;
use static_init::dynamic;

use microlift::arch::x86_64::X86_64;
use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::exit::ExitReason;
use microlift::core::link::BlockLinkEntry;
use microlift::core::{LifterConfig, lift_to_code};

use crate::program::Program;
use crate::vm::VmError;

/// One lifted, compiled guest region
pub use microlift::runtime::Translation;

/// Enter `translation`, resuming at `ctx.rip`, and report how it left.
pub fn enter(translation: &Translation, ctx: &mut RegisterContext) -> ExitReason {
    ctx.image_base = translation.image_base;
    ctx.exit_reason = ExitReason::None as u32;
    // Cleared here rather than by a probe that missed, so the store is on the
    // once-per-entry path instead of the once-per-exit one.
    ctx.link_hit = 0;
    // SAFETY: `entry` is the finalized address of a Cranelift function whose
    // signature is `(*mut RegisterContext) -> ()` in the host C convention,
    // compiled into the module this `Translation` owns and therefore still
    // mapped. It is `entry` and never `link_target`: under a pinned ABI those
    // are different functions and the second one is not callable from Rust.
    // The lifted code's only memory effects outside `ctx` are the guest's own -
    // the guest stack and its mapped images, all live for the duration.
    let entry: unsafe extern "C" fn(*mut RegisterContext) =
        unsafe { std::mem::transmute(translation.entry) };
    unsafe { entry(ctx as *mut RegisterContext) };
    // Under a pinned ABI a link is a branch: `link_hit` is never written and
    // the chain has already run itself out by the time this returns.
    match host_resolved_links() {
        true => follow_links(ctx),
        false => ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None),
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
        // SAFETY: `link_hit` holds the address of a `BlockLinkEntry` lifted code
        // resolved for itself - either a row of the table published in
        // `ctx.link_table`, which is this thread's own and which only this thread
        // writes, or a link cell in some region's own array, which the shared cache
        // writes under its lock. Both are alive here: a table row is written with the
        // `Arc` the window holds beside it, a cell is written through a `CellRef`
        // holding the owning region's, and this cache retires rather than drops (see
        // [`Published::retained`]).
        let row = unsafe { *(ctx.link_hit as *const BlockLinkEntry) };
        debug_assert_eq!(
            row.guest_pc, ctx.rip,
            "a link was taken for an address the region did not commit"
        );
        // Both counters, so that the entry *rate* and the exit *mix* keep
        // counting the events they always counted.
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
        // SAFETY: `code` is a translation's `link_target`, of the same signature
        // and with the same effects as the call in [`enter`] - under a host-resolved
        // ABI, which is the only ABI that reaches this loop, `link_target` and
        // `entry` are the same function. It was written by `Translation::row`,
        // through either `CodeWindow::put` or the shared cache's cell install.
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
    /// know of one - which a caller must read as "ask the regions", not as "there is
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

/// Translations, keyed by every guest address each may be entered at.
///
/// Two maps of the same thing, on purpose:
///
/// - [`microlift::runtime::CodeCache`] is the authority. It owns the translations, it
///   owns the page index invalidation would sweep, and - the reason this crate stopped
///   keeping its own map - it owns the **link cells**. Inserting a translation there is
///   what fills in the cells of every region that had been branching to it and the
///   cells of its own that name something already compiled, in both directions and
///   eagerly, which is the whole of what the new block linking buys.
/// - [`EntryIndex`] is a lock-free read of the same answer, for the one caller that
///   asks on a hot path.
///
/// A hit reads only the index; a miss takes the write lock and lifts. That the lock is
/// on the cold path is not an assumption - `CacheStats::miss_rate` measures it, minus
/// the hits the index answered without asking.
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
    cache: microlift::runtime::CodeCache,
    /// Every translation ever inserted, held so that none is ever freed.
    ///
    /// **This is what makes the raw pointers in [`EntryIndex`] sound**, and it is the
    /// same bargain the old `retired` vector struck for the same reason: an index that
    /// hands out a `*const Translation` without a lock cannot also be told when one
    /// dies. It covers two ways a translation would otherwise be dropped - a re-lift of
    /// an address two threads missed on at once, which displaces the loser from
    /// microlift's map, and an invalidation, whose retired list this crate never
    /// [`reclaim`](microlift::runtime::CodeCache::reclaim)s because it has no
    /// cross-thread safepoint to reclaim at.
    ///
    /// So nothing is unmapped for the life of the process. That was already true before
    /// the switch; what is new is that it is now the *only* thing keeping it true.
    retained: Vec<Arc<Translation>>,
}

// SAFETY: the one thing keeping a cache off a second thread is the raw pointer
// inside `LifterConfig`'s `DynamicSymbol`s - the *host address* a fast import
// resolved to. It is a function address, cloned into each lift and never
// dereferenced on this side; the code that eventually calls it is compiled arm64
// doing what any thread may do with a function pointer. The translations beside
// it carry their own claim, as `microlift::runtime::Translation` states. `Sync` on
// top of `Send` because the lock is inside the cache: threads share one by reference.
unsafe impl Send for CodeCache {}
unsafe impl Sync for CodeCache {}

impl CodeCache {
    /// A cache that lifts with `config`.
    pub fn new(config: LifterConfig) -> Self {
        CodeCache {
            published: RwLock::new(Published {
                cache: microlift::runtime::CodeCache::new(),
                retained: Vec::new(),
            }),
            index: EntryIndex::new(),
            config,
        }
    }

    /// How many regions have been translated.
    pub fn len(&self) -> usize {
        self.read().retained.len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().retained.is_empty()
    }

    /// What the shared cache has been asked to do, and how much of it block linking
    /// spared it. See [`microlift::runtime::CacheStats`].
    pub fn stats(&self) -> microlift::runtime::CacheStats {
        self.read().cache.stats()
    }

    /// `(installed, pending)` link cells.
    ///
    /// The second number is what separates the two ways a run can report no cells
    /// installed: a lift that emitted none, and cells waiting for a target nothing has
    /// translated yet. Only the first would mean the mechanism is off.
    pub fn cell_counts(&self) -> (usize, usize) {
        self.read().cache.cell_counts()
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
                     {} byte(s), {} link cell(s)) in {took:.1?}",
                    program
                        .locate(start)
                        .unwrap_or_else(|| "generated code".to_string()),
                    region.instructions(),
                    end - start,
                    compiled.link_sites.len(),
                );
            }
        }

        debug_assert!(
            compiled.resume_points.binary_search(&pc).is_ok(),
            "translation {start:#x}..{end:#x} cannot be entered at the {pc:#x} \
             it was lifted for"
        );
        // Where this region's compiled code went, so a fault inside it can be
        // named as a *guest* range rather than as an address in an anonymous
        // mapping. See [`crate::fault`]. The *body*, not the entry.
        crate::fault::placed(
            compiled.region.link_target as usize as u64,
            start,
            end,
            pc,
            &compiled.map,
        );
        crate::lldb::register(
            compiled.region.link_target as usize as u64,
            start,
            end,
            &program
                .locate(start)
                .unwrap_or_else(|| "generated code".to_string()),
            &compiled.guest_bytes,
            &compiled.map,
        );

        // `AddressMode::Direct`, so the address a region was lifted for is the address
        // it runs at, and the image base is the region's own start.
        let translation = Translation::from_compiled(
            compiled.region,
            start,
            compiled.resume_points,
            compiled.covered,
            compiled.data_dependencies,
            compiled.link_sites,
            compiled.backend,
        );

        // Two threads that missed on the same address both compiled it. The loser is
        // displaced from microlift's map but not dropped - see [`Published::retained`].
        let mut published = self.write();
        // The one call that matters: as well as making the translation findable, this
        // fills in every link cell whose target is already compiled, and every cell
        // *elsewhere* that has been waiting for one of these resume points.
        let translation = published.cache.insert(translation);
        published.retained.push(Arc::clone(&translation));
        self.index
            .publish(&translation.resume_points, Arc::as_ptr(&translation));
        Ok(translation)
    }

    /// An existing translation covering `pc`, if there is one.
    fn lookup(&self, pc: u64) -> Option<Arc<Translation>> {
        if let Some(hit) = self.index.get(pc) {
            // SAFETY: the pointer was taken from an `Arc` this cache put in
            // `retained`, which never gives one up. So the strong count is at least
            // one for as long as `&self` is borrowed, which is what makes an
            // increment off a bare pointer legal here and nowhere else.
            return Some(unsafe {
                Arc::increment_strong_count(hit);
                Arc::from_raw(hit)
            });
        }
        // The one race the index cannot cover: a probe that overtook another thread's
        // publish. Counted by microlift as a host dispatch, which it is - the index
        // hits above are the ones that never became one.
        self.write().cache.lookup(pc)
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

    // Cheap: the two fields with a program's worth of data in them are behind
    // `Arc`s, so this copies pointers. It did not always, and that clone was 7%
    // of a process's wall clock.
    let mut config = config.clone();
    // Declared per region rather than once, because the answer is per region.
    config.resume_points = declared.to_vec();
    // Which of these bytes are code.
    config.block_ranges = region.blocks.clone();

    let lifted = lift_to_code::<X86_64>(bytes, region.range.start, config)?;
    // Cloned out before the lift is consumed: these four are what the shared cache
    // needs to publish, invalidate and link this translation, and
    // `compile_lifted_region` takes the `LiftedCode` by value. `Translation::compile`
    // does exactly this and is not used here only because it discards the source map
    // that [`crate::fault`] and [`crate::lldb`] are handed below.
    let resume_points = lifted.resume_points.clone();
    let covered = lifted.covered.clone();
    let data_dependencies = lifted.data_dependencies.clone();
    let link_sites = lifted.link_sites.clone();
    let lift_took = clock.map(|c| c.elapsed());
    let mut backend = crate::jit::backend()?;
    let (compiled_region, map) = backend.compile_lifted_region(lifted)?;
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
        region: compiled_region,
        map,
        guest_bytes: crate::lldb::enabled()
            .then(|| bytes.to_vec())
            .unwrap_or_default(),
        resume_points,
        covered,
        data_dependencies,
        link_sites,
    })
}

/// One region through the lifter and the compiler, with everything about it that
/// only the lift knows.
struct Compiled {
    backend: microlift::backend::CraneliftBackend,
    /// The two entry points and this region's link-cell array.
    region: microlift::backend::CompiledRegion,
    /// Host offset -> guest instruction, so a fault inside this code can name
    /// the instruction rather than the region. See [`crate::fault`].
    map: microlift::core::srcloc::GuestCodeMap,
    /// Guest bytes retained only so the opt-in LLDB listing can show x86-64.
    guest_bytes: Vec<u8>,
    /// Where the compiled code may be entered, ascending - and so the exact set of
    /// addresses it may be published in a link table under.
    resume_points: Vec<u64>,
    /// The guest bytes this region was derived from: the invalidation key.
    covered: Vec<std::ops::Range<u64>>,
    /// Guest bytes outside `covered` that the lift read and baked in - a jump
    /// table, chiefly.
    data_dependencies: Vec<std::ops::Range<u64>>,
    /// The lift-time-known destinations this region has a cell for, in cell order.
    link_sites: Vec<microlift::core::link::LinkSite>,
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
