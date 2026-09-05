//! Putting the whole guest on this host's `malloc`, so the host's own
//! corruption checks apply.
//!
//! This is the other half. Every entry point the guest imports from its own
//! allocator is rebound to a host equivalent as well, so one allocator owns
//! every block, and a double free stops being a live block quietly pushed onto
//! a free list and becomes `free(): double free detected in tcache 2` naming
//! the call.
//!
//! Not a mode to run in - the allocator a program ships is usually the faster
//! one and always the one it was tested against. It is the lever to reach for
//! when the heap is corrupt and nothing says where.

use std::ffi::c_void;

use machish::memory::Regions;
use machish::report::LinkReport;

/// Set to move the guest onto this host's allocator. Implies
/// [`crate::interpose::NO_INTERPOSE_ENV`].
pub const ENV: &str = "ROSETTA3_HOST_ALLOCATOR";

pub fn requested() -> bool {
    std::env::var_os(ENV).is_some_and(|v| v != "0")
}

/// mimalloc's `mi_malloc_aligned(size, alignment)`, in the host's argument
/// order. `aligned_alloc` also requires the size to be a multiple of the
/// alignment, which mimalloc does not, so the size is rounded up.
unsafe extern "C" fn malloc_aligned(size: usize, alignment: usize) -> *mut c_void {
    let alignment = alignment.max(1);
    let size = size.next_multiple_of(alignment);
    // SAFETY: `alignment` is non-zero and `size` a multiple of it, which is
    // what `aligned_alloc` requires; a null return is the caller's to handle.
    unsafe { libc::aligned_alloc(alignment, size) }
}

/// `mi_realloc_aligned(p, newsize, alignment)`.
///
/// `realloc` makes no alignment promise beyond `max_align_t`, so an
/// over-aligned block has to be reallocated by hand. `malloc_usable_size` is
/// what bounds the copy: the old size is not passed and the allocator is the
/// only thing that knows it.
unsafe extern "C" fn realloc_aligned(
    p: *mut c_void,
    newsize: usize,
    alignment: usize,
) -> *mut c_void {
    if p.is_null() {
        // SAFETY: the null case is a plain aligned allocation.
        return unsafe { malloc_aligned(newsize, alignment) };
    }
    // SAFETY: `p` came from this allocator, so asking it for the block's size
    // is defined.
    #[cfg(not(target_os = "macos"))]
    let old = unsafe { libc::malloc_usable_size(p) };
    #[cfg(target_os = "macos")]
    let old = unsafe { libc::malloc_size(p) };
    // SAFETY: as above.
    let fresh = unsafe { malloc_aligned(newsize, alignment) };
    if fresh.is_null() {
        return fresh;
    }
    // SAFETY: both blocks are live and the copy is bounded by the smaller.
    unsafe {
        std::ptr::copy_nonoverlapping(p.cast::<u8>(), fresh.cast::<u8>(), old.min(newsize));
        libc::free(p);
    }
    fresh
}

/// `mi_is_in_heap_region`. Every pointer is, once there is one heap.
unsafe extern "C" fn in_heap_region(_p: *const c_void) -> bool {
    true
}

/// `mi_collect`, `mi_option_set`, `mi_register_output`: accepted and ignored.
unsafe extern "C" fn ignored() {}

/// `mi_option_get`. Zero for every option, which is what a caller reading one
/// back gets when nothing set it.
unsafe extern "C" fn option_get(_option: i32) -> i64 {
    0
}

/// The guest's allocator symbols and the host address each is rebound to.
///
/// Names without the Mach-O underscore. The libc half is what the
/// interposition would otherwise have moved; the `mi_` half is what the guest
/// calls directly and is the reason turning the interposition off is not
/// enough on its own.
fn replacements() -> Vec<(&'static str, u64)> {
    let host = |name: &str| crate::host::symbol(name);
    let mut out: Vec<(&'static str, u64)> = Vec::new();
    let mut same = |name: &'static str, of: &str| {
        if let Some(addr) = host(of) {
            out.push((name, addr));
        }
    };
    same("mi_malloc", "malloc");
    same("mi_free", "free");
    same("mi_realloc", "realloc");
    same("mi_calloc", "calloc");
    same("mi_strdup", "strdup");
    same("mi_strndup", "strndup");
    same("mi_usable_size", "malloc_usable_size");
    same("mi_malloc_usable_size", "malloc_usable_size");
    same("mi_zalloc", "malloc");
    out.push(("mi_malloc_aligned", malloc_aligned as *const () as u64));
    out.push(("mi_realloc_aligned", realloc_aligned as *const () as u64));
    out.push(("mi_is_in_heap_region", in_heap_region as *const () as u64));
    out.push(("mi_collect", ignored as *const () as u64));
    out.push(("mi_option_set", ignored as *const () as u64));
    out.push(("mi_option_set_default", ignored as *const () as u64));
    out.push(("mi_option_enable", ignored as *const () as u64));
    out.push(("mi_option_disable", ignored as *const () as u64));
    out.push(("mi_register_output", ignored as *const () as u64));
    out.push(("mi_option_get", option_get as *const () as u64));
    out
}

/// Rebind every guest import of the shipped allocator to the host's.
///
/// Call after [`crate::interpose::apply`], which under this lever moves
/// nothing, so the libc names are still the host's and only the `mi_` ones need
/// rewriting.
pub fn apply<M: Regions>(report: &mut LinkReport, memory: &mut M) -> Result<usize, machish::Error> {
    if !requested() {
        return Ok(0);
    }
    let replacements = replacements();
    let mut rebound = 0;
    for image in &mut report.images {
        for binding in &mut image.bindings {
            let name = darwin::base_name(&binding.symbol);
            let Some(&(_, addr)) = replacements.iter().find(|(n, _)| *n == name) else {
                continue;
            };
            if binding.resolved == addr {
                continue;
            }
            memory.write(binding.slot_va, &addr.to_le_bytes())?;
            binding.resolved = addr;
            binding.tag = 0;
            rebound += 1;
        }
    }
    eprintln!(
        "rosetta3: {ENV}: {rebound} allocator import(s) moved to this host's libc - the guest's \
         own allocator is not in use"
    );
    Ok(rebound)
}
