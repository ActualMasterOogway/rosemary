//! `malloc_zone_statistics`, which is a question about an allocator this host
//! does not have.
//!
//! Darwin's malloc is zoned and its statistics are per zone; glibc's is not
//! zoned at all. A program calls this to report how much memory it is using -
//! Deltarune prints "Total memory used" from it - so the answer that matters is
//! the total, and the total is a question glibc *does* answer, through
//! `mallinfo2`.
//!
//! The zone argument is ignored, because there is one allocator and it is the
//! one being asked about. A guest that had obtained a zone of its own could not
//! have: `malloc_create_zone` is refused by name in `rosetta3::abi::table`.

#![cfg(not(target_os = "macos"))]

use std::ffi::c_void;

use machish::resolve::SyntheticDylib;

/// Darwin's `malloc_statistics_t`. Four fields, and the layout is ABI: a guest
/// reads `size_in_use` out of a structure it allocated on its own stack.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Statistics {
    pub blocks_in_use: u32,
    _pad: u32,
    pub size_in_use: usize,
    pub max_size_in_use: usize,
    pub size_allocated: usize,
}

const _: () = {
    assert!(size_of::<Statistics>() == 32);
    assert!(std::mem::offset_of!(Statistics, size_in_use) == 8);
};

/// glibc's `struct mallinfo2`, which is ten `size_t`s.
///
/// Declared here rather than taken from the `libc` crate, which does not carry
/// it: the two fields read below are the third and the eighth, and naming the
/// whole structure is what makes that a fact rather than an offset.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MallInfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    hblkhd: usize,
    usmblks: usize,
    fsmblks: usize,
    uordblks: usize,
    fordblks: usize,
    keepcost: usize,
}

/// `void malloc_zone_statistics(malloc_zone_t *zone, malloc_statistics_t *stats)`
///
/// # Safety
///
/// `stats` is null or a writable Darwin `malloc_statistics_t`.
pub unsafe extern "C" fn malloc_zone_statistics(_zone: *mut c_void, stats: *mut Statistics) {
    // Gated off to GNU because we don't know if other libc implementations have it, better to be safe than sorry.
    #[cfg(target_env = "gnu")]
    {
        unsafe extern "C" {
            fn mallinfo2() -> MallInfo2;
        }
        if stats.is_null() {
            return;
        }
        // SAFETY: takes no argument and returns by value.
        let info = unsafe { mallinfo2() };
        // SAFETY: the caller states `stats` is writable.
        unsafe {
            *stats = Statistics {
                // glibc counts no blocks, and a count invented from the byte total
                // would be a number with no meaning. Zero says "not reported",
                // which is what a caller that divides by it needs to see.
                blocks_in_use: 0,
                _pad: 0,
                // `uordblks` counts only what came out of the heap. A block past
                // `M_MMAP_THRESHOLD` - which is most of a game's memory - is mapped
                // instead and counted in `hblkhd`, so reporting the first alone says
                // a program using two hundred megabytes is using sixteen kilobytes.
                size_in_use: info.uordblks + info.hblkhd,
                // The high-water mark is not kept either. Reporting the current
                // total would say the program has never freed anything.
                max_size_in_use: 0,
                size_allocated: info.arena + info.hblkhd,
            };
        }
    }
}

pub const NAMES: &[&str] = &["malloc_zone_statistics"];

pub fn symbol(name: &str) -> Option<u64> {
    match name {
        "malloc_zone_statistics" => Some(malloc_zone_statistics as *const () as usize as u64),
        _ => None,
    }
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(
            lib,
            &format!("_{name}"),
            symbol(name).unwrap_or(0) as *const (),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The total is a real number and the fields nothing can answer are zero,
    /// which is the difference between "not reported" and "nothing allocated".
    #[test]
    fn the_total_in_use_is_reported() {
        // Two allocations, because glibc counts them in different fields: a
        // small one comes out of the heap and a large one is mapped, and a
        // report that missed either would still look plausible.
        // Through `black_box`, because in a release build an allocation whose
        // only use is its own length is one the optimiser deletes - and the test
        // then measures a program holding nothing.
        let small = std::hint::black_box(vec![0u8; 8 << 10]);
        let large = std::hint::black_box(vec![0u8; 4 << 20]);
        let mut stats = Statistics::default();
        // SAFETY: a writable structure of the declared layout.
        unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
        let held = small.len() + large.len();
        assert!(
            stats.size_in_use >= held,
            "{} bytes in use with {held} held",
            stats.size_in_use,
        );
        assert!(stats.size_allocated >= stats.size_in_use);
        assert_eq!(stats.max_size_in_use, 0, "a high-water mark nothing keeps");
        drop(std::hint::black_box((small, large)));
    }
}
