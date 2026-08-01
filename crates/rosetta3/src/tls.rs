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
//! code and there is no guest code to point it at. The cost is a VM exit per
//! thread-local access, which is slow and correct — the same trade the roadmap
//! records for unresolved jump tables.
//!
//! Thread-local destructors (`__cxa_thread_atexit`) are not run. C `_Thread_local`
//! has none; a C++ thread-local with a non-trivial destructor is the case this
//! would matter for, and it belongs with the rest of the C++ runtime work.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Mutex;

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
static IMAGES: Mutex<Vec<Template>> = Mutex::new(Vec::new());

/// Every known `tlv_descriptor`, by its own address.
static SLOTS: Mutex<BTreeMap<u64, Slot>> = Mutex::new(BTreeMap::new());

thread_local! {
    /// This thread's blocks, one per image that has thread-locals, allocated on
    /// first touch and freed when the thread ends.
    static BLOCKS: RefCell<BTreeMap<usize, Block>> = const { RefCell::new(BTreeMap::new()) };
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

        let mut images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
        images.push(template);
        let index = images.len() - 1;
        drop(images);

        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
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
    let (image, offset) = {
        let slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        match slots.get(&at) {
            Some(slot) => (slot.image, slot.offset),
            None => return std::ptr::null_mut(),
        }
    };

    BLOCKS.with(|blocks| {
        let mut blocks = blocks.borrow_mut();
        if let std::collections::btree_map::Entry::Vacant(slot) = blocks.entry(image) {
            let Some(fresh) = allocate(image) else {
                return std::ptr::null_mut();
            };
            slot.insert(fresh);
        }
        let block = &blocks[&image];
        // SAFETY: `offset` was checked against `total_size` at registration and
        // the block was allocated with exactly that many bytes.
        unsafe { block.ptr.add(offset as usize) }
    })
}

/// A zeroed block for `image`, with its initialised prefix copied in.
fn allocate(image: usize) -> Option<Block> {
    let images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
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
}
