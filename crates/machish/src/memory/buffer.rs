//! A plain in-memory address space (`no_std`). Used when loading an image whose
//! code is handed to a recompiler rather than executed, and throughout the test
//! suite, where it lets every mapping and fixup be asserted on without giving
//! the guest a single real page.
//!
//! Note what this deliberately does *not* need: sparse backing. An image's
//! preferred base is `0x1_0000_0000` on x86-64 macOS, which looks like it would
//! demand a 4 GiB allocation — but only because `__PAGEZERO` occupies
//! `[0, 0x1_0000_0000)`, and the loader never maps `__PAGEZERO`. The reserved
//! span starts at the first real segment, so the buffer is the size of the
//! actual image and `base` absorbs the rest.

use alloc::vec;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::memory::{align_up, AddressSpace, Placement, Prot};
use crate::raw::Word;

/// An owned, byte-addressable region rooted at a chosen base address.
pub struct BufferSpace {
    /// Where to place the image when [`Placement::Anywhere`] leaves it to us.
    default_base: Word,
    base: Word,
    mem: Vec<u8>,
}

impl BufferSpace {
    /// A space that will place an unconstrained image at `default_base`.
    ///
    /// A [`Placement::Preferred`] or [`Placement::Fixed`] request always wins
    /// over this: a buffer has no existing mappings to collide with, so it can
    /// honour any address, which is exactly what makes it useful for asserting
    /// that a fixup landed at the VA the file asked for.
    pub fn new(default_base: Word) -> Self {
        BufferSpace { default_base, base: default_base, mem: Vec::new() }
    }

    /// Base address of the reserved region.
    pub fn base(&self) -> Word {
        self.base
    }

    /// Total reserved length in bytes.
    pub fn len(&self) -> usize {
        self.mem.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mem.is_empty()
    }

    /// Borrow the backing bytes, e.g. to hand a region to a recompiler.
    pub fn bytes(&self) -> &[u8] {
        &self.mem
    }

    fn index(&self, addr: Word, len: usize) -> Result<usize> {
        let off = addr.checked_sub(self.base).ok_or(Error::OutOfBounds { addr, len })?;
        let off = usize::try_from(off).map_err(|_| Error::OutOfBounds { addr, len })?;
        let end = off.checked_add(len).ok_or(Error::OutOfBounds { addr, len })?;
        if end > self.mem.len() {
            return Err(Error::OutOfBounds { addr, len });
        }
        Ok(off)
    }
}

impl AddressSpace for BufferSpace {
    fn reserve(&mut self, size: usize, align: usize, placement: Placement) -> Result<Word> {
        if !self.mem.is_empty() {
            return Err(Error::Memory("BufferSpace already reserved"));
        }
        let align = (align.max(1) as Word).next_power_of_two();
        self.base = align_up(placement.wanted().unwrap_or(self.default_base), align);
        self.mem = vec![0u8; size];
        Ok(self.base)
    }

    fn write(&mut self, addr: Word, data: &[u8]) -> Result<()> {
        let off = self.index(addr, data.len())?;
        self.mem[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn read(&self, addr: Word, out: &mut [u8]) -> Result<()> {
        let off = self.index(addr, out.len())?;
        out.copy_from_slice(&self.mem[off..off + out.len()]);
        Ok(())
    }

    /// A buffer models no protection, so this validates the range and returns.
    /// Validating rather than ignoring means a loader bug that protects the
    /// wrong extent still surfaces in tests that use `BufferSpace`.
    fn protect(&mut self, addr: Word, len: usize, _prot: Prot) -> Result<()> {
        self.index(addr, len)?;
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
    fn double_reserve_is_rejected() {
        let mut mem = BufferSpace::new(0x1000);
        mem.reserve(0x100, 0x10, Placement::Anywhere).unwrap();
        assert!(mem.reserve(0x100, 0x10, Placement::Anywhere).is_err());
    }
}
