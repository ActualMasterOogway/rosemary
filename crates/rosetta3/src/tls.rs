//! Thread-local storage: `__tlv_bootstrap`, and the per-thread block behind it.
//!
//! ## What the guest actually compiles to
//!
//! A `_Thread_local int x` is not an address. It is a three-word
//! `tlv_descriptor` in `__DATA,__thread_vars` — `{ thunk, key, offset }` — and
//! reading the variable compiles to
//!
//! ```text
//! leaq _x(%rip), %rdi     ; the descriptor
//! callq *(%rdi)           ; call its thunk, which returns this thread's address
//! ```
//!
//! `thunk` is an import. dyld binds it to `__tlv_bootstrap`, which allocates the
//! calling thread's block on first touch and returns `block + offset`. That is
//! the whole protocol, and it is why milestone 1 could enumerate descriptors and
//! go no further: the block is *per thread*, so allocating one means having
//! decided what a guest thread is.
//!
//! ## Why the block is host memory and not a guest mapping
//!
//! The guest is identity-mapped, so an ordinary host allocation is already
//! something guest code can load and store through — the lifted arm64 does a
//! real dereference of a real address. Nothing about a thread block needs to be
//! at a guest virtual address: no fixup points at it, it is not in any segment,
//! and the descriptor's `offset` is relative to the block rather than absolute.
//! Allocating it here means it is freed when the thread ends, which is the
//! lifetime the guest expects and one no guest mapping would give.
//!
//! Shims reach it through [`GuestView`](crate::vm::GuestView), which permits
//! host addresses after probing that something is mapped — the same path a
//! `strerror` result already takes.
//!
//! ## Why the registry is process-global
//!
//! [`bootstrap`] is called by *guest* code through a pointer in a descriptor,
//! with no argument that could carry a VM and no thread-local to find one on:
//! it may be the first thing a brand-new guest thread does. What it is given is
//! the descriptor's address, and because the guest is identity-mapped that
//! address is unique in this process — so it is a usable key on its own. The
//! registry is therefore global and keyed by descriptor address, and a
//! re-registration at the same address replaces the older entry, which is what
//! makes a second program loaded over a first one's unmapped range correct
//! rather than stale.
//!
//! ## What is deliberately not done
//!
//! dyld overwrites `descriptor->thunk` with a fast path after the first call,
//! so later accesses go straight to the block. We leave the thunk alone and
//! service every access, because the fast path would have to be *guest*-callable
//! code and there is no guest code to point it at. The cost is a call per
//! thread-local access, which is slow and correct — the same trade the roadmap
//! records for unresolved jump tables.
//!
//! ## Why the answer is cached per thread
//!
//! Servicing every access is only affordable if servicing one is cheap, and the
//! first version of this was not: it took the process-global [`SLOTS`] mutex on
//! the way in, so *every* thread-local read in the program serialised against
//! every other one. That is invisible in a single-threaded guest and ruinous in
//! Roblox, which reads thread-locals from a job-system thread pool — the lock is
//! uncontended work with a contended cache line under it, and the more cores the
//! guest gets the worse it does. The mutex was doing no useful work either: the
//! map is written once per image at load and read forever after.
//!
//! What a descriptor resolves to is a pure function of the descriptor address
//! *and the calling thread*, and it never changes for the life of either — so it
//! is cached in the thread's own [`ThreadTls`], keyed by descriptor address, and
//! a hit touches no shared memory at all. Only the first access to a given
//! descriptor on a given thread reaches the registry, which is the number of
//! lock acquisitions dropping from "per access" to "per thread, per variable".
//!
//! The one thing that *can* change the answer is a re-registration — a second
//! program loaded over a first one's range, which the registry deliberately
//! allows. [`EPOCH`] is bumped whenever that happens and stamped into each
//! thread's cache, so a stale entry is discarded on the next access rather than
//! handing the new program the old program's block.
//!
//! Thread-local destructors (`__cxa_thread_atexit`) are not run. C `_Thread_local`
//! has none; a C++ thread-local with a non-trivial destructor is the case this
//! would matter for, and it belongs with the rest of the C++ runtime work.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use machish::report::LinkReport;

/// Largest thread block this will allocate.
///
/// Totality, not policy: `total_size` is arithmetic over two section headers
/// from an untrusted file, and a corrupt image must fail to allocate rather
/// than ask the allocator for a petabyte on every thread the guest spawns.
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
///
/// A lock read by many threads at once and written only while a program loads,
/// which is what makes it an `RwLock` rather than a `Mutex`: the readers are
/// guest threads touching a thread-local for the first time, and they have no
/// reason to queue behind each other.
static IMAGES: RwLock<Vec<Template>> = RwLock::new(Vec::new());

/// Every known `tlv_descriptor`, by its own address.
static SLOTS: RwLock<BTreeMap<u64, Slot>> = RwLock::new(BTreeMap::new());

/// How many times the registry has changed.
///
/// Read on every access and written once per [`register`], so it is the cheapest
/// shared thing in this module: one load off a line that is shared in every
/// core's cache and never dirtied while the guest runs — which is the property
/// the mutex it replaced did not have, since a lock dirties its line to be
/// *taken*.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Entries in one thread's descriptor cache. A power of two so the index is a
/// mask, and large enough that a program's whole working set of thread-locals
/// fits — a miss is correct, just a lock acquisition.
const CACHE: usize = 64;

/// Which cache line a descriptor address falls on.
///
/// Descriptors are three words apart in `__thread_vars`, so the low bits alone
/// would collide in runs; multiply-shift mixes the whole address before the
/// mask takes six bits of it.
fn line(at: u64) -> usize {
    (at.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize & (CACHE - 1)
}

/// One thread's view of thread-local storage.
struct ThreadTls {
    /// The [`EPOCH`] `cache` was filled under. A mismatch means some image was
    /// registered since, and every entry is discarded rather than trusted.
    epoch: u64,
    /// Direct-mapped descriptor address to this thread's address for it. A key
    /// of zero is an empty entry, which costs nothing: address zero is not a
    /// descriptor and is answered by the slow path anyway.
    cache: [(u64, *mut u8); CACHE],
    /// This thread's blocks, one per image that has thread-locals, allocated on
    /// first touch and freed when the thread ends.
    blocks: BTreeMap<usize, Block>,
}

thread_local! {
    /// Everything about thread-locals that belongs to *this* thread.
    ///
    /// The cache and the blocks are one object on purpose: the cached pointers
    /// point *into* the blocks, so they must not be able to outlive them.
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
///
/// Called once per program, before any guest code runs. Returns how many
/// descriptors were registered, which is what a test can assert on without
/// reaching into the registry.
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
                // Skipping it makes the access fail as a null dereference in the
                // guest rather than a wild write into the runtime's heap.
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
    // the new epoch cannot then read the map without seeing them. Bumped even
    // when nothing was registered: this is cheap, happens once per program, and
    // "no descriptors" is not something to reason about separately.
    EPOCH.fetch_add(1, Ordering::Release);
    registered
}

/// `void *__tlv_bootstrap(struct tlv_descriptor *)` — this thread's address for
/// the variable `descriptor` describes.
///
/// A real native arm64 function, which is the point: it is bound into the
/// guest's descriptor like any other import, and the guest's `call *(%rdi)`
/// reaches it through the ordinary indirect-call path
/// ([`Vm::dispatch_indirect`](crate::vm::Vm)) with the signature the table
/// records. Nothing about it needs a JIT.
///
/// Returns null for a descriptor nobody registered, which is the only answer
/// available inside a C ABI with no error channel — and it faults in the guest
/// at the access rather than corrupting anything.
pub extern "C" fn bootstrap(descriptor: *const u8) -> *mut u8 {
    let at = descriptor as usize as u64;
    // `try_with` rather than `with`: this is reached from guest code, and a
    // `thread_local` destructor that runs guest code — a C++ `thread_local` with
    // a destructor, reached through `_tlv_atexit` — can touch a thread-local
    // after this key has been destroyed. `with` panics there, and a panic out of
    // an `extern "C"` frame called by the JIT aborts the process with nothing
    // naming the cause. Null is the answer the C ABI has for it.
    match TLS.try_with(|tls| resolve(&mut tls.borrow_mut(), at)) {
        Ok(address) => address,
        Err(_) => {
            say_after_teardown(at);
            std::ptr::null_mut()
        }
    }
}

/// The body of [`bootstrap`], with this thread's storage already in hand.
///
/// The whole point of the split is the first six lines: a repeat access to a
/// descriptor this thread has already touched reads two words of its own
/// thread-local memory and one acquire load off a line no thread ever dirties,
/// and takes no lock.
fn resolve(tls: &mut ThreadTls, at: u64) -> *mut u8 {
    // Acquire pairs with the release in `register`: observing the new epoch
    // means observing the slots that came with it.
    let epoch = EPOCH.load(Ordering::Acquire);
    if tls.epoch != epoch {
        // A program was registered since these were resolved. The blocks stay —
        // image indices are append-only, so an old one still names its own
        // template — but a descriptor address may now mean something else.
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

    // Cached only now, and only for an answer that came from a block: a failure
    // is not a fact about this thread — it is a fact about what has been
    // registered so far — and the diagnostic it goes through has its own
    // once-each rule to keep it quiet.
    tls.cache[line] = (at, address);
    address
}

/// Name a descriptor this runtime has no block for, once each.
///
/// Null is the only value a C ABI with no error channel can return, and the
/// guest's very next instruction dereferences it — so without this the failure
/// arrives as a segfault inside guest code with nothing anywhere naming a
/// thread-local. Which is how it did arrive: an image loaded after start-up has
/// its own `__thread_vars`, and a `_Thread_local` in it reads as address zero.
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

/// Report a thread-local read that arrived after this thread's storage was
/// destroyed, once for the process.
///
/// Not a hypothetical: it is where a C++ `thread_local` with a destructor ends
/// up if the destructor reads another thread-local. The guest gets null, which
/// faults at the access — but it faults with something in the log saying the
/// access was late rather than unregistered, which are different bugs.
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
        // image: what is under test is the resolver, and its whole input is one
        // slot and one template.
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
