//! Thread-local storage: `__tlv_bootstrap`, and the per-thread block behind it.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use machish::report::LinkReport;

/// Largest thread block this will allocate.
const MAX_BLOCK: u64 = 64 * 1024 * 1024;

/// One image's block layout, as [`machish`] described it.
struct Template {
    /// Slid VA the initialised prefix is copied from.
    initial_va: u64,
    initial_size: usize,
    total_size: usize,
    align: usize,
}

/// What one descriptor address means.
struct Slot {
    /// Index into [`IMAGES`].
    image: usize,
    /// Byte offset of this variable within that image's block.
    offset: u64,
}

/// Every registered image's layout. Append-only; indices are handed out as
/// `Slot::image` and must stay valid.
static IMAGES: RwLock<Vec<Template>> = RwLock::new(Vec::new());

/// Every known `tlv_descriptor`, by its own address.
static SLOTS: RwLock<BTreeMap<u64, Slot>> = RwLock::new(BTreeMap::new());

/// How many times the registry has changed.
///
/// Read on every access and written once per [`register`], so it is the cheapest shared
/// thing in this module.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Entries in one thread's descriptor cache.
const CACHE: usize = 64;

/// Which cache line a descriptor address falls on.
fn line(at: u64) -> usize {
    (at.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize & (CACHE - 1)
}

/// One thread's view of thread-local storage.
struct ThreadTls {
    /// The [`EPOCH`] `cache` was filled under. A mismatch means some image was
    /// registered since, and every entry is discarded rather than trusted.
    epoch: u64,
    /// Direct-mapped descriptor address to this thread's address for it. A key of zero
    /// is an empty entry, which costs nothing.
    cache: [(u64, *mut u8); CACHE],
    /// This thread's blocks, one per image that has thread-locals, allocated on
    /// first touch and freed when the thread ends.
    blocks: BTreeMap<usize, Block>,
}

thread_local! {
    /// Everything about thread-locals that belongs to *this* thread.
    ///
    /// The cache and the blocks are one object on purpose.
    static TLS: RefCell<ThreadTls> = const {
        RefCell::new(ThreadTls {
            epoch: 0,
            cache: [(0, std::ptr::null_mut()); CACHE],
            blocks: BTreeMap::new(),
        })
    };
}

/// One thread's block for one image.
struct Block {
    ptr: *mut u8,
    layout: Layout,
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with exactly this layout and is
        // owned by this `Block`, which is reachable only from the thread-local
        // map being torn down.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// Make every thread-local in `report` addressable.
pub fn register(report: &LinkReport) -> usize {
    let mut registered = 0;
    for image in &report.images {
        let Some(t) = image.tlv_template else {
            continue;
        };
        if image.thread_locals.is_empty() || t.total_size == 0 || t.total_size > MAX_BLOCK {
            continue;
        }
        let template = Template {
            initial_va: t.initial_va,
            initial_size: (t.initial_size.min(t.total_size)) as usize,
            total_size: t.total_size as usize,
            align: 1usize << t.align.min(16),
        };

        let mut images = IMAGES.write().unwrap_or_else(|e| e.into_inner());
        images.push(template);
        let index = images.len() - 1;
        drop(images);

        let mut slots = SLOTS.write().unwrap_or_else(|e| e.into_inner());
        for d in &image.thread_locals {
            if d.offset >= t.total_size {
                // The descriptor indexes past the block the sections describe.
                continue;
            }
            slots.insert(
                d.at,
                Slot {
                    image: index,
                    offset: d.offset,
                },
            );
            registered += 1;
        }
    }
    // After the slots are visible, and released so that a thread which observes
    // the new epoch cannot then read the map without seeing them.
    EPOCH.fetch_add(1, Ordering::Release);
    registered
}

/// `void *__tlv_bootstrap(struct tlv_descriptor *)` — this thread's address for the
/// variable `descriptor` describes.
///
/// A real native arm64 function, which is the point:
///
/// Returns null for a descriptor nobody registered, which is the only answer available
/// inside a C ABI with no error channel — and it faults in the guest at the access
/// rather than corrupting anything.
pub extern "C" fn bootstrap(descriptor: *const u8) -> *mut u8 {
    let at = descriptor as usize as u64;
    // `try_with` rather than `with`.
    match TLS.try_with(|tls| resolve(&mut tls.borrow_mut(), at)) {
        Ok(address) => address,
        Err(_) => {
            say_after_teardown(at);
            std::ptr::null_mut()
        }
    }
}

/// [`bootstrap`] behind the register discipline a `tlv_descriptor` thunk owes its
/// caller.
///
/// dyld's `_tlv_get_addr` saves x1-x16 and q0-q7 around its slow path and the
/// compiler counts on it — Roblox's allocator holds a live pointer in x8 across
/// the `blr` through the descriptor. An ordinary C function clobbers all of
/// them, so a native image's descriptors point here instead.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn bootstrap_thunk(_descriptor: *const u8) -> *mut u8 {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        "stp x1, x2, [sp, #-16]!",
        "stp x3, x4, [sp, #-16]!",
        "stp x5, x6, [sp, #-16]!",
        "stp x7, x8, [sp, #-16]!",
        "stp x9, x10, [sp, #-16]!",
        "stp x11, x12, [sp, #-16]!",
        "stp x13, x14, [sp, #-16]!",
        "stp x15, x16, [sp, #-16]!",
        "stp q0, q1, [sp, #-32]!",
        "stp q2, q3, [sp, #-32]!",
        "stp q4, q5, [sp, #-32]!",
        "stp q6, q7, [sp, #-32]!",
        "bl {bootstrap}",
        "ldp q6, q7, [sp], #32",
        "ldp q4, q5, [sp], #32",
        "ldp q2, q3, [sp], #32",
        "ldp q0, q1, [sp], #32",
        "ldp x15, x16, [sp], #16",
        "ldp x13, x14, [sp], #16",
        "ldp x11, x12, [sp], #16",
        "ldp x9, x10, [sp], #16",
        "ldp x7, x8, [sp], #16",
        "ldp x5, x6, [sp], #16",
        "ldp x3, x4, [sp], #16",
        "ldp x1, x2, [sp], #16",
        "ldp x29, x30, [sp], #16",
        "ret",
        bootstrap = sym bootstrap,
    )
}

/// The body of [`bootstrap`], with this thread's storage already in hand.
fn resolve(tls: &mut ThreadTls, at: u64) -> *mut u8 {
    // Acquire pairs with the release in `register`: observing the new epoch
    // means observing the slots that came with it.
    let epoch = EPOCH.load(Ordering::Acquire);
    if tls.epoch != epoch {
        // A program was registered since these were resolved.
        tls.cache = [(0, std::ptr::null_mut()); CACHE];
        tls.epoch = epoch;
    }

    let line = line(at);
    if at != 0 && tls.cache[line].0 == at {
        return tls.cache[line].1;
    }

    let (image, offset) = {
        let slots = SLOTS.read().unwrap_or_else(|e| e.into_inner());
        match slots.get(&at) {
            Some(slot) => (slot.image, slot.offset),
            None => {
                say_unregistered(at);
                return std::ptr::null_mut();
            }
        }
    };

    if let std::collections::btree_map::Entry::Vacant(slot) = tls.blocks.entry(image) {
        let Some(fresh) = allocate(image) else {
            return std::ptr::null_mut();
        };
        slot.insert(fresh);
    }
    // SAFETY: `offset` was checked against `total_size` at registration and the
    // block was allocated with exactly that many bytes.
    let address = unsafe { tls.blocks[&image].ptr.add(offset as usize) };

    // Cached only now, and only for an answer that came from a block.
    tls.cache[line] = (at, address);
    address
}

/// Name a descriptor this runtime has no block for, once each.
///
/// Null is the only value a C ABI with no error channel can return, and the guest's
/// very next instruction dereferences it — so without this the failure arrives as a
/// segfault inside guest code with nothing anywhere naming a thread-local.
fn say_unregistered(at: u64) {
    static SAID: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&at) {
        return;
    }
    said.push(at);
    eprintln!(
        "rosetta3: __tlv_bootstrap({at:#x}): no thread-local registered at that descriptor \
         — the guest will read address zero"
    );
}

/// Report a thread-local read that arrived after this thread's storage was destroyed,
/// once for the process.
fn say_after_teardown(at: u64) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        eprintln!(
            "rosetta3: __tlv_bootstrap({at:#x}): this thread's thread-local storage is already \
             torn down — the guest will read address zero"
        );
    });
}

/// A zeroed block for `image`, with its initialised prefix copied in.
fn allocate(image: usize) -> Option<Block> {
    let images = IMAGES.read().unwrap_or_else(|e| e.into_inner());
    let t = images.get(image)?;
    let layout = Layout::from_size_align(t.total_size, t.align).ok()?;

    // SAFETY: the layout is non-zero-sized — `register` rejects `total_size` of
    // zero — and `alloc_zeroed` is the whole of the "and the rest is zero" half
    // of the block's contract.
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return None;
    }

    if t.initial_size > 0 {
        // SAFETY: `initial_va` is a slid VA inside the guest's own mapped
        // `__thread_data`, which is live for as long as the program is loaded,
        // and the guest is identity-mapped so the address is the pointer.
        // `initial_size` was clamped to `total_size` at registration, so the
        // destination has room.
        unsafe {
            std::ptr::copy_nonoverlapping(t.initial_va as usize as *const u8, ptr, t.initial_size);
        }
    }
    Some(Block { ptr, layout })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unregistered_descriptor_answers_null_rather_than_guessing() {
        assert!(bootstrap(0xdead_beef as *const u8).is_null());
        assert!(bootstrap(std::ptr::null()).is_null());
    }

    /// The cache must not turn a miss into a hit for a *different* descriptor
    /// that happens to land on the same line, which is the one way a
    /// direct-mapped cache can hand back someone else's variable.
    #[test]
    fn a_colliding_descriptor_is_not_mistaken_for_the_cached_one() {
        // Two addresses on the same line, found rather than assumed: the mixing
        // constant decides which ones collide.
        let base = 0x1_0000u64;
        let (a, b) = (base..base + 0x10_000)
            .step_by(24)
            .find_map(|a| {
                (a + 24..a + 0x10_000)
                    .step_by(24)
                    .find(|&b| line(b) == line(a))
                    .map(|b| (a, b))
            })
            .expect("two of ten thousand addresses share one of sixty-four lines");

        let mut tls = ThreadTls {
            epoch: EPOCH.load(Ordering::Relaxed),
            cache: [(0, std::ptr::null_mut()); CACHE],
            blocks: BTreeMap::new(),
        };
        let sentinel = 0x5eed_usize as *mut u8;
        tls.cache[line(a)] = (a, sentinel);

        assert_eq!(resolve(&mut tls, a), sentinel, "the cached entry is a hit");
        assert!(
            resolve(&mut tls, b).is_null(),
            "a collision must consult the registry, not the line"
        );
    }

    /// Two threads must not share a block, and a second access on one thread
    /// must return what the first did — which is the pair of properties the
    /// cache sits between, and the way it could break them is by being reached
    /// from more than the thread it belongs to.
    #[test]
    fn a_block_is_per_thread_and_a_repeat_access_is_the_same_address() {
        // Registered by hand rather than through a `LinkReport`, which needs an
        // image.
        let at = 0x3_0000u64;
        {
            let mut images = IMAGES.write().unwrap();
            images.push(Template {
                initial_va: 0,
                initial_size: 0,
                total_size: 64,
                align: 16,
            });
            let image = images.len() - 1;
            drop(images);
            SLOTS.write().unwrap().insert(at, Slot { image, offset: 8 });
        }
        EPOCH.fetch_add(1, Ordering::Release);

        let mine = bootstrap(at as *const u8);
        assert!(!mine.is_null(), "a registered descriptor has a block");
        assert_eq!(
            bootstrap(at as *const u8),
            mine,
            "the cached answer must be the answer"
        );
        // SAFETY: `mine` is eight bytes into a sixty-four byte block owned by
        // this thread, so a byte write is in bounds and unshared.
        unsafe { *mine = 0x5e };

        let theirs = std::thread::spawn(move || {
            let theirs = bootstrap(at as *const u8);
            // SAFETY: as above, on the spawned thread's own block.
            assert_eq!(unsafe { *theirs }, 0, "a fresh block is zeroed");
            theirs as usize
        })
        .join()
        .expect("the thread reads its own thread-local");
        assert_ne!(
            theirs, mine as usize,
            "a second thread must get its own block, not this one's"
        );
    }

    /// A registration invalidates what threads resolved before it, because a
    /// descriptor address may now belong to a different program.
    #[test]
    fn a_new_epoch_discards_what_was_resolved_under_the_old_one() {
        let at = 0x2_0000u64;
        let mut tls = ThreadTls {
            epoch: EPOCH.load(Ordering::Relaxed),
            cache: [(0, std::ptr::null_mut()); CACHE],
            blocks: BTreeMap::new(),
        };
        tls.cache[line(at)] = (at, 0x5eed_usize as *mut u8);

        EPOCH.fetch_add(1, Ordering::Release);
        assert!(
            resolve(&mut tls, at).is_null(),
            "the stale entry must not survive the epoch it was filled in"
        );
    }
}
