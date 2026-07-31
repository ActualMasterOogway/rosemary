//! A plain in-memory address space (`no_std`). Used when loading an image whose
//! code is handed to a recompiler rather than executed, and throughout the test
//! suite, where it lets every mapping and fixup be asserted on without giving
//! the guest a single real page.
//!
//! Note what this deliberately does *not* need: sparse backing. An image's
//! preferred base is `0x1_0000_0000` on x86-64 macOS, which looks like it would
//! demand a 4 GiB allocation — but only because `__PAGEZERO` occupies
//! `[0, 0x1_0000_0000)`, and the loader never maps `__PAGEZERO`. The reserved
//! span starts at the first real segment, so each region is the size of the
//! actual image and its base absorbs the rest.
//!
//! ## Several regions, not one
//!
//! A space holds one region per [`reserve`](AddressSpace::reserve), because a
//! link maps one region per *image* and a program with a bundled dylib has more
//! than one. Regions are allocated separately and are disjoint: a single
//! contiguous buffer spanning them would have to cover the gap between an
//! executable at `0x1_0000_0000` and a dylib the allocator placed elsewhere,
//! which is precisely the gigabytes-of-nothing the `__PAGEZERO` decision exists
//! to avoid.

use alloc::vec;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::memory::{align_up, AddressSpace, Placement, Prot};
use crate::raw::Word;

/// One reserved region.
struct Region {
    base: Word,
    mem: Vec<u8>,
}

impl Region {
    fn end(&self) -> Word {
        self.base.wrapping_add(self.mem.len() as Word)
    }

    fn overlaps(&self, base: Word, len: usize) -> bool {
        base < self.end() && self.base < base.wrapping_add(len as Word)
    }
}

/// An owned, byte-addressable set of regions.
pub struct BufferSpace {
    /// Where to place a region when [`Placement::Anywhere`] leaves it to us.
    /// Later unconstrained regions follow the highest one already placed.
    default_base: Word,
    regions: Vec<Region>,
}

impl BufferSpace {
    /// A space that will place an unconstrained image at `default_base`.
    ///
    /// A [`Placement::Preferred`] or [`Placement::Fixed`] request wins over
    /// this whenever the range is free — a buffer has no pre-existing mappings
    /// to collide with, which is exactly what makes it useful for asserting
    /// that a fixup landed at the VA the file asked for.
    pub fn new(default_base: Word) -> Self {
        BufferSpace { default_base, regions: Vec::new() }
    }

    /// Base address of the first region reserved.
    pub fn base(&self) -> Word {
        self.regions.first().map_or(self.default_base, |r| r.base)
    }

    /// Total bytes across every region.
    pub fn len(&self) -> usize {
        self.regions.iter().map(|r| r.mem.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// How many regions have been reserved.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// The region containing `addr`, as `(base, bytes)` — e.g. to hand an
    /// image's mapped contents to a recompiler.
    pub fn region_at(&self, addr: Word) -> Option<(Word, &[u8])> {
        let r = self.regions.iter().find(|r| addr >= r.base && addr < r.end())?;
        Some((r.base, &r.mem))
    }

    /// Every region, in reservation order.
    pub fn regions(&self) -> impl Iterator<Item = (Word, &[u8])> {
        self.regions.iter().map(|r| (r.base, r.mem.as_slice()))
    }

    /// Backing bytes of the first region. Kept because the single-image case is
    /// the common one, and making it name a region would be noise.
    pub fn bytes(&self) -> &[u8] {
        self.regions.first().map_or(&[][..], |r| r.mem.as_slice())
    }

    fn find(&self, addr: Word, len: usize) -> Result<(usize, usize)> {
        for (i, r) in self.regions.iter().enumerate() {
            let Some(off) = addr.checked_sub(r.base) else { continue };
            let Ok(off) = usize::try_from(off) else { continue };
            // `>=`, not `>`: an address exactly at one region's end is the
            // *next* region's first byte when they abut, and treating it as
            // belonging to this one would reject every access to the second
            // image the loader maps.
            if off >= r.mem.len() {
                continue;
            }
            // Inside this region. If the access runs off its end, that is an
            // error rather than a reason to try the next region: regions are
            // disjoint, so a straddling access is a bug either way, and finding
            // a different region for it would hide the bug.
            let Some(end) = off.checked_add(len) else {
                return Err(Error::OutOfBounds { addr, len });
            };
            if end > r.mem.len() {
                return Err(Error::OutOfBounds { addr, len });
            }
            return Ok((i, off));
        }
        Err(Error::OutOfBounds { addr, len })
    }

    /// The lowest address at or above `from` where `len` bytes are free.
    fn first_free(&self, from: Word, len: usize, align: Word) -> Word {
        let mut base = align_up(from, align);
        // Regions are few — one per image — so rescanning after each bump is
        // cheaper than maintaining sorted state.
        loop {
            match self.regions.iter().find(|r| r.overlaps(base, len)) {
                Some(hit) => base = align_up(hit.end(), align),
                None => return base,
            }
        }
    }
}

impl crate::memory::Regions for BufferSpace {
    fn region_at(&self, addr: Word) -> Option<(Word, &[u8])> {
        BufferSpace::region_at(self, addr)
    }

    fn regions(&self) -> alloc::vec::Vec<(Word, &[u8])> {
        BufferSpace::regions(self).collect()
    }
}

impl AddressSpace for BufferSpace {
    fn reserve(&mut self, size: usize, align: usize, placement: Placement) -> Result<Word> {
        if size == 0 {
            return Err(Error::Memory("zero-length reservation"));
        }
        let align = (align.max(1) as Word).next_power_of_two();

        let base = match placement {
            Placement::Fixed(want) => {
                let want = align_up(want, align);
                if self.regions.iter().any(|r| r.overlaps(want, size)) {
                    return Err(Error::Memory("fixed placement overlaps a reserved region"));
                }
                want
            }
            Placement::Preferred(want) => {
                let want = align_up(want, align);
                if self.regions.iter().any(|r| r.overlaps(want, size)) {
                    // Slide rather than fail: `Preferred` is the image saying it
                    // tolerates being moved, and the loader records the slide.
                    let highest =
                        self.regions.iter().map(|r| r.end()).max().unwrap_or(self.default_base);
                    self.first_free(highest, size, align)
                } else {
                    want
                }
            }
            Placement::Anywhere => {
                let from =
                    self.regions.iter().map(|r| r.end()).max().unwrap_or(self.default_base);
                self.first_free(from, size, align)
            }
        };

        self.regions.push(Region { base, mem: vec![0u8; size] });
        Ok(base)
    }

    fn write(&mut self, addr: Word, data: &[u8]) -> Result<()> {
        let (i, off) = self.find(addr, data.len())?;
        self.regions[i].mem[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn read(&self, addr: Word, out: &mut [u8]) -> Result<()> {
        let (i, off) = self.find(addr, out.len())?;
        out.copy_from_slice(&self.regions[i].mem[off..off + out.len()]);
        Ok(())
    }

    /// A buffer models no protection, so this validates the range and returns.
    /// Validating rather than ignoring means a loader bug that protects the
    /// wrong extent still surfaces in tests that use `BufferSpace`.
    fn protect(&mut self, addr: Word, len: usize, _prot: Prot) -> Result<()> {
        self.find(addr, len)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honours_preferred_placement() {
        let mut mem = BufferSpace::new(0);
        let base = mem.reserve(0x2000, 0x1000, Placement::Preferred(0x1_0000_0000)).unwrap();
        assert_eq!(base, 0x1_0000_0000);
        assert_eq!(mem.base(), 0x1_0000_0000);
        assert_eq!(mem.len(), 0x2000);
    }

    #[test]
    fn falls_back_to_default_base() {
        let mut mem = BufferSpace::new(0x4000_0000);
        let base = mem.reserve(0x1000, 0x1000, Placement::Anywhere).unwrap();
        assert_eq!(base, 0x4000_0000);
    }

    #[test]
    fn reserve_zeroes_and_round_trips() {
        let mut mem = BufferSpace::new(0x1000);
        mem.reserve(0x100, 0x10, Placement::Anywhere).unwrap();

        let mut out = [0xffu8; 8];
        mem.read(0x1000, &mut out).unwrap();
        assert_eq!(out, [0u8; 8], "reserved memory must read back as zero");

        mem.write(0x1040, &[1, 2, 3, 4]).unwrap();
        let mut out = [0u8; 4];
        mem.read(0x1040, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn rejects_out_of_bounds() {
        let mut mem = BufferSpace::new(0x1000);
        mem.reserve(0x100, 0x10, Placement::Anywhere).unwrap();

        // Below the base.
        assert!(mem.write(0x0fff, &[0]).is_err());
        // Past the end.
        assert!(mem.write(0x1100, &[0]).is_err());
        // Straddling the end.
        assert!(mem.write(0x10ff, &[0, 0]).is_err());
        // Overflowing the address computation.
        assert!(mem.read(u64::MAX, &mut [0u8; 2]).is_err());
    }

    #[test]
    fn several_images_get_several_disjoint_regions() {
        let mut mem = BufferSpace::new(0x1_0000_0000);
        let a = mem.reserve(0x2000, 0x1000, Placement::Preferred(0x1_0000_0000)).unwrap();
        let b = mem.reserve(0x2000, 0x1000, Placement::Anywhere).unwrap();
        assert_eq!(a, 0x1_0000_0000);
        assert!(b >= a + 0x2000, "the second region must not overlap the first");
        assert_eq!(mem.region_count(), 2);

        // Each addressable independently, with no bleed between them.
        mem.write(a, &[0xaa; 4]).unwrap();
        mem.write(b, &[0xbb; 4]).unwrap();
        let mut buf = [0u8; 4];
        mem.read(a, &mut buf).unwrap();
        assert_eq!(buf, [0xaa; 4]);
        mem.read(b, &mut buf).unwrap();
        assert_eq!(buf, [0xbb; 4]);
    }

    #[test]
    fn a_preferred_placement_that_collides_slides_instead_of_failing() {
        let mut mem = BufferSpace::new(0);
        let a = mem.reserve(0x2000, 0x1000, Placement::Fixed(0x1_0000_0000)).unwrap();
        let b = mem.reserve(0x1000, 0x1000, Placement::Preferred(0x1_0000_0000)).unwrap();
        assert_eq!(a, 0x1_0000_0000);
        assert!(b >= a + 0x2000, "a colliding preferred placement must slide");
    }

    #[test]
    fn a_fixed_placement_that_collides_is_an_error() {
        let mut mem = BufferSpace::new(0);
        mem.reserve(0x2000, 0x1000, Placement::Fixed(0x1_0000_0000)).unwrap();
        // A non-PIE image has its addresses baked into its code; sliding it
        // silently would produce something that links and jumps into nothing.
        assert!(mem.reserve(0x1000, 0x1000, Placement::Fixed(0x1_0000_1000)).is_err());
    }

    #[test]
    fn region_lookup_finds_the_right_backing_bytes() {
        let mut mem = BufferSpace::new(0);
        let a = mem.reserve(0x1000, 0x1000, Placement::Fixed(0x1_0000_0000)).unwrap();
        let b = mem.reserve(0x1000, 0x1000, Placement::Fixed(0x2_0000_0000)).unwrap();
        mem.write(b, &[0x77; 4]).unwrap();

        assert_eq!(mem.region_at(a).map(|(base, _)| base), Some(a));
        let (base, bytes) = mem.region_at(b + 2).unwrap();
        assert_eq!(base, b);
        assert_eq!(&bytes[..4], &[0x77; 4]);
        assert!(mem.region_at(0x9_0000_0000).is_none());
    }

    #[test]
    fn zero_length_reservations_are_rejected() {
        let mut mem = BufferSpace::new(0);
        assert!(mem.reserve(0, 0x1000, Placement::Anywhere).is_err());
    }
}
