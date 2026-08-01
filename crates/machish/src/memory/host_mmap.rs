//! Host-memory address space backed by `mmap`.
//!
//! Two things here differ from `selfish/src/memory/host_mmap.rs`, and both are
//! deliberate.
//!
//! **No `MAP_JIT` for a guest image.** `selfish` maps `RWX | MAP_JIT` on macOS
//! and toggles `pthread_jit_write_protect_np` around every store
//! (`host_mmap.rs:58-65, 93-109`), because it loads aarch64 code that this CPU
//! will execute, and Apple Silicon refuses to make an ever-writable page
//! executable. Our situation is categorically different: the guest image is
//! x86-64, so the arm64 CPU cannot execute a byte of it under any protection —
//! it is *input data* to a recompiler, which emits its own arm64 into its own
//! buffer, where W^X is already handled. Opting into `MAP_JIT` anyway would
//! cost an entitlement, make every write depend on a *per-thread* protection
//! toggle, and — decisively — force `protect` to become a no-op
//! (`host_mmap.rs:118-125`), destroying any ability to model guest protection.
//! So the guest path is plain `mmap` + `mprotect`. [`MmapSpace::executable`]
//! re-enables the `selfish` behaviour for the one case that wants it: loading a
//! *native* arm64 image to call directly.
//!
//! **Never `MAP_FIXED`.** A Mach-O carries a preferred `vmaddr` and the
//! recompiler is identity-mapped, so the loader really does want a specific
//! address — but `MAP_FIXED` silently unmaps whatever is already there, which
//! on a collision would blow a hole in the host's own libraries. The address is
//! passed as an advisory hint instead and the result is checked.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use crate::error::{Error, Result};
use crate::memory::{AddressSpace, Placement, Prot, host_page_size};
use crate::raw::Word;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

/// One `mmap`ed region.
struct Region {
    base: *mut u8,
    len: usize,
}

impl Region {
    fn contains(&self, addr: usize, len: usize) -> Option<*mut u8> {
        let off = addr.checked_sub(self.base as usize)?;
        if off.checked_add(len)? > self.len {
            return None;
        }
        // SAFETY: `off + len` was just checked against this region's length.
        Some(unsafe { self.base.add(off) })
    }

    /// The region's bytes.
    ///
    /// The slice aliases memory the guest may be executing against — see
    /// [`Regions`](crate::memory::Regions) for why that is inherent to an
    /// identity-mapped space rather than something this could guard.
    fn as_slice(&self) -> &[u8] {
        // SAFETY: `base..base+len` is a live mapping this space owns and keeps
        // readable for its whole lifetime; the borrow is tied to `self`.
        unsafe { core::slice::from_raw_parts(self.base as *const u8, self.len) }
    }
}

/// `mmap`-backed host memory owning its mappings; all unmapped on drop.
///
/// Holds one region per [`reserve`](AddressSpace::reserve), because a link maps
/// one region per image and a program with a bundled dylib has more than one.
pub struct MmapSpace {
    regions: Vec<Region>,
    /// Whether the caller asked for host-executable memory.
    want_exec: bool,
    /// Whether the live mappings are `MAP_JIT` (implies `want_exec` on macOS).
    jit: bool,
}

impl MmapSpace {
    pub fn new() -> Self {
        MmapSpace {
            regions: Vec::new(),
            want_exec: false,
            jit: false,
        }
    }

    /// Request memory the host CPU may execute.
    ///
    /// Only meaningful when loading an image of the *host's* architecture. A
    /// guest x86-64 image must leave this off: the pages would gain `PROT_EXEC`
    /// that nothing can use, and on macOS the `MAP_JIT` mapping it implies
    /// makes per-segment protection unenforceable.
    pub fn executable(mut self, yes: bool) -> Self {
        self.want_exec = yes;
        self
    }

    fn in_bounds(&self, addr: Word, len: usize) -> Result<*mut u8> {
        let a = usize::try_from(addr).map_err(|_| Error::OutOfBounds { addr, len })?;
        self.regions
            .iter()
            .find_map(|r| r.contains(a, len))
            .ok_or(Error::OutOfBounds { addr, len })
    }

    /// Total bytes mapped across every region.
    pub fn len(&self) -> usize {
        self.regions.iter().map(|r| r.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// How many regions have been reserved.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Whether `[base, base+len)` overlaps a region already mapped *by us*.
    ///
    /// Only our own regions: the kernel is the authority on everything else,
    /// and `try_reserve_at` already defers to it by checking where the
    /// non-`MAP_FIXED` hint actually landed.
    fn collides(&self, base: usize, len: usize) -> bool {
        self.regions
            .iter()
            .any(|r| base < r.base as usize + r.len && (r.base as usize) < base + len)
    }

    fn mmap_flags(&self) -> (libc::c_int, libc::c_int) {
        if self.jit {
            (
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | map_jit(),
            )
        } else {
            (
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
            )
        }
    }

    /// Map `size` bytes wherever the kernel likes, trimming to `align`.
    /// Mirrors `selfish/src/memory/host_mmap.rs:50-91`.
    fn reserve_anywhere(&mut self, size: usize, align: usize) -> Result<Word> {
        let align = align.max(1).next_power_of_two();
        let over = size
            .checked_add(align)
            .ok_or(Error::Memory("reserve overflow"))?;
        let (prot, flags) = self.mmap_flags();

        // SAFETY: standard anonymous mmap; the result is checked against MAP_FAILED.
        let raw = unsafe { libc::mmap(ptr::null_mut(), over, prot, flags, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(Error::Memory("mmap failed"));
        }
        let raw = raw as usize;
        let aligned = (raw + (align - 1)) & !(align - 1);

        let head = aligned - raw;
        if head > 0 {
            // SAFETY: unmapping only the leading slack we just mapped ourselves.
            unsafe { libc::munmap(raw as *mut c_void, head) };
        }
        let tail = over - head - size;
        if tail > 0 {
            // SAFETY: unmapping only the trailing slack we just mapped ourselves.
            unsafe { libc::munmap((aligned + size) as *mut c_void, tail) };
        }

        self.regions.push(Region {
            base: aligned as *mut u8,
            len: size,
        });
        Ok(aligned as Word)
    }

    /// Try to map `size` bytes exactly at `want`, without `MAP_FIXED`.
    ///
    /// Returns `Ok(None)` if the kernel placed us elsewhere, having already
    /// released the mapping it gave us — the caller decides whether that is a
    /// slide to accept or an error.
    fn try_reserve_at(&mut self, size: usize, want: Word) -> Result<Option<Word>> {
        let hint = usize::try_from(want).map_err(|_| Error::Memory("placement too high"))?;
        let (prot, flags) = self.mmap_flags();

        // SAFETY: anonymous mmap with an *advisory* hint — no MAP_FIXED, so the
        // kernel relocates us rather than unmapping anything already at `hint`.
        // The result is checked against MAP_FAILED before use.
        let raw = unsafe { libc::mmap(hint as *mut c_void, size, prot, flags, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(Error::Memory("mmap failed"));
        }
        if raw as usize != hint {
            // SAFETY: unmapping exactly the region the kernel just handed us.
            unsafe { libc::munmap(raw, size) };
            return Ok(None);
        }

        self.regions.push(Region {
            base: raw as *mut u8,
            len: size,
        });
        Ok(Some(want))
    }
}

impl crate::memory::Regions for MmapSpace {
    fn region_at(&self, addr: Word) -> Option<(Word, &[u8])> {
        let a = usize::try_from(addr).ok()?;
        let r = self
            .regions
            .iter()
            .find(|r| a >= r.base as usize && a < r.base as usize + r.len)?;
        Some((r.base as Word, r.as_slice()))
    }

    fn regions(&self) -> Vec<(Word, &[u8])> {
        self.regions
            .iter()
            .map(|r| (r.base as Word, r.as_slice()))
            .collect()
    }
}

impl AddressSpace for MmapSpace {
    fn reserve(&mut self, size: usize, align: usize, placement: Placement) -> Result<Word> {
        if size == 0 {
            return Err(Error::Memory("zero-length reservation"));
        }
        // On macOS, host-executable memory must be MAP_JIT from the start:
        // a page that was ever writable cannot later become executable.
        self.jit = self.want_exec && cfg!(target_os = "macos");

        // Guard our own regions before asking the kernel. Without this a second
        // image preferring the same address as the first would get the hint
        // honoured — the range is genuinely mapped, just by us — and the two
        // images would be written over each other.
        if let Some(want) = placement.wanted()
            && let Ok(want) = usize::try_from(want)
            && self.collides(want, size)
        {
            return match placement {
                Placement::Fixed(_) => Err(Error::Memory("fixed placement already reserved")),
                _ => self.reserve_anywhere(size, align),
            };
        }

        match placement {
            Placement::Anywhere => self.reserve_anywhere(size, align),
            Placement::Preferred(want) => match self.try_reserve_at(size, want)? {
                Some(base) => Ok(base),
                None => self.reserve_anywhere(size, align),
            },
            Placement::Fixed(want) => self
                .try_reserve_at(size, want)?
                .ok_or(Error::Memory("fixed placement unavailable")),
        }
    }

    fn write(&mut self, addr: Word, data: &[u8]) -> Result<()> {
        let dst = self.in_bounds(addr, data.len())?;
        if self.jit {
            // SAFETY: toggling this thread's JIT write protection off, storing
            // within the owned mapping, re-enabling it, then flushing the
            // instruction cache for exactly the range written.
            unsafe {
                jit_write_protect(false);
                ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                jit_write_protect(true);
                icache_flush(dst as *mut c_void, data.len());
            }
        } else {
            // SAFETY: `dst` is within the owned mapping, which is currently writable.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
        }
        Ok(())
    }

    fn read(&self, addr: Word, out: &mut [u8]) -> Result<()> {
        let src = self.in_bounds(addr, out.len())?;
        // SAFETY: `src` is within the owned mapping, which is readable.
        unsafe { ptr::copy_nonoverlapping(src, out.as_mut_ptr(), out.len()) };
        Ok(())
    }

    fn protect(&mut self, addr: Word, len: usize, prot: Prot) -> Result<()> {
        // Under MAP_JIT the region is RWX at the hardware level with W^X
        // enforced dynamically by the write-protect toggle, so per-range
        // mprotect is both unnecessary and counterproductive. This is the
        // behaviour `selfish` has unconditionally on macOS; here it applies
        // only to the opt-in executable mode.
        if self.jit {
            return Ok(());
        }
        if len == 0 {
            return Ok(());
        }
        let ptr = self.in_bounds(addr, len)? as *mut c_void;
        let mut flags = 0;
        if prot.read {
            flags |= libc::PROT_READ;
        }
        if prot.write {
            flags |= libc::PROT_WRITE;
        }
        if prot.exec {
            flags |= libc::PROT_EXEC;
        }
        // SAFETY: protecting a sub-range of the owned mapping, bounds-checked above.
        let rc = unsafe { libc::mprotect(ptr, len, flags) };
        if rc != 0 {
            return Err(Error::Memory("mprotect failed"));
        }
        Ok(())
    }

    fn protect_granularity(&self) -> u64 {
        host_page_size()
    }

    fn host_ptr(&self, addr: Word) -> Option<*mut u8> {
        self.in_bounds(addr, 1).ok()
    }
}

impl Default for MmapSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MmapSpace {
    fn drop(&mut self) {
        for r in &self.regions {
            if !r.base.is_null() && r.len != 0 {
                // SAFETY: unmapping exactly a region we reserved and still own.
                unsafe { libc::munmap(r.base as *mut c_void, r.len) };
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn map_jit() -> libc::c_int {
    libc::MAP_JIT
}

#[cfg(not(target_os = "macos"))]
fn map_jit() -> libc::c_int {
    0
}

#[cfg(target_os = "macos")]
unsafe fn jit_write_protect(enable: bool) {
    // SAFETY: FFI to a thread-local toggle with no memory effects of its own.
    unsafe { libc::pthread_jit_write_protect_np(enable as libc::c_int) };
}

#[cfg(target_os = "macos")]
unsafe fn icache_flush(p: *mut c_void, len: usize) {
    // SAFETY: flushing the instruction cache over a range we just wrote.
    unsafe { sys_icache_invalidate(p, len) };
}

#[cfg(not(target_os = "macos"))]
unsafe fn jit_write_protect(_enable: bool) {}

#[cfg(not(target_os = "macos"))]
unsafe fn icache_flush(_p: *mut c_void, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_anywhere_is_zeroed_and_aligned() {
        let mut mem = MmapSpace::new();
        let base = mem.reserve(0x8000, 0x4000, Placement::Anywhere).unwrap();
        assert_eq!(base % 0x4000, 0);

        let mut out = [0xffu8; 16];
        mem.read(base, &mut out).unwrap();
        assert_eq!(out, [0u8; 16]);
    }

    #[test]
    fn write_read_round_trip() {
        let mut mem = MmapSpace::new();
        let base = mem.reserve(0x4000, 0x1000, Placement::Anywhere).unwrap();
        mem.write(base + 0x100, b"machish").unwrap();
        let mut out = [0u8; 7];
        mem.read(base + 0x100, &mut out).unwrap();
        assert_eq!(&out, b"machish");
    }

    #[test]
    fn bounds_are_enforced() {
        let mut mem = MmapSpace::new();
        let base = mem.reserve(0x4000, 0x1000, Placement::Anywhere).unwrap();
        assert!(mem.write(base - 1, &[0]).is_err());
        assert!(mem.write(base + 0x4000, &[0]).is_err());
        assert!(mem.read(base + 0x3fff, &mut [0u8; 2]).is_err());
        assert!(mem.host_ptr(base + 0x4000).is_none());
        assert!(mem.host_ptr(base).is_some());
    }

    #[test]
    fn preferred_placement_falls_back_without_clobbering() {
        // Reserve a region, then ask a second space to land on top of it. The
        // request must be denied and relocated, and the first space must be
        // completely intact afterward — that is the MAP_FIXED hazard.
        let mut first = MmapSpace::new();
        let base = first.reserve(0x4000, 0x1000, Placement::Anywhere).unwrap();
        first.write(base, b"sentinel").unwrap();

        let mut second = MmapSpace::new();
        let got = second
            .reserve(0x4000, 0x1000, Placement::Preferred(base))
            .unwrap();
        assert_ne!(got, base, "the occupied range must not have been taken");

        let mut out = [0u8; 8];
        first.read(base, &mut out).unwrap();
        assert_eq!(&out, b"sentinel", "the existing mapping was clobbered");
    }

    #[test]
    fn fixed_placement_fails_rather_than_clobbering() {
        let mut first = MmapSpace::new();
        let base = first.reserve(0x4000, 0x1000, Placement::Anywhere).unwrap();
        first.write(base, b"sentinel").unwrap();

        let mut second = MmapSpace::new();
        assert!(
            second
                .reserve(0x4000, 0x1000, Placement::Fixed(base))
                .is_err()
        );

        let mut out = [0u8; 8];
        first.read(base, &mut out).unwrap();
        assert_eq!(&out, b"sentinel");
    }

    #[test]
    fn fixed_placement_when_it_succeeds_is_exact() {
        // 0x1_0000_0000 is where an x86-64 Mach-O executable wants to land, and
        // in a normal host process that range is free. Whether it *is* free is
        // not ours to guarantee, so the assertion is conditional — but the
        // guarantee that matters is unconditional: `Fixed` either lands on the
        // exact address or fails. It must never quietly slide, because a
        // non-PIE image bound to a slid base would be silently wrong.
        let mut mem = MmapSpace::new();
        match mem.reserve(0x8000, 0x4000, Placement::Fixed(0x1_0000_0000)) {
            Ok(base) => {
                assert_eq!(base, 0x1_0000_0000);
                mem.write(base, b"guest").unwrap();
            }
            Err(e) => assert_eq!(e, Error::Memory("fixed placement unavailable")),
        }
    }

    #[test]
    fn guest_mapping_is_never_executable() {
        // The §4.4 invariant: a default (guest) space never asks for PROT_EXEC.
        let mut mem = MmapSpace::new();
        let base = mem.reserve(0x4000, 0x1000, Placement::Anywhere).unwrap();
        assert!(!mem.jit, "guest mappings must not be MAP_JIT");
        // Protecting read-execute is still a legal request the caller may make;
        // the loader is what declines to make it for a non-native spec.
        mem.protect(base, 0x1000, Prot::RO).unwrap();
    }

    #[test]
    fn several_reservations_coexist_without_clobbering_each_other() {
        // The multi-image case. Two reservations must both stay live and stay
        // independently addressable — a second `reserve` replacing the first
        // would leak one mapping and make every address in it invalid.
        let mut mem = MmapSpace::new();
        let a = mem.reserve(0x8000, 0x4000, Placement::Anywhere).unwrap();
        let b = mem.reserve(0x8000, 0x4000, Placement::Anywhere).unwrap();
        assert_eq!(mem.region_count(), 2);
        assert_ne!(a, b);

        mem.write(a, &[0xaa; 8]).unwrap();
        mem.write(b, &[0xbb; 8]).unwrap();
        let mut buf = [0u8; 8];
        mem.read(a, &mut buf).unwrap();
        assert_eq!(buf, [0xaa; 8]);
        mem.read(b, &mut buf).unwrap();
        assert_eq!(buf, [0xbb; 8]);
    }

    #[test]
    fn a_second_image_preferring_an_address_we_already_hold_slides() {
        // Without the self-collision check this is the dangerous case: the
        // kernel would honour the hint, because the range *is* mapped — by us —
        // and the second image would be written over the first.
        let mut mem = MmapSpace::new();
        let a = mem.reserve(0x8000, 0x4000, Placement::Anywhere).unwrap();
        let b = mem
            .reserve(0x4000, 0x4000, Placement::Preferred(a))
            .unwrap();
        assert_ne!(b, a, "a colliding preferred placement must slide");

        mem.write(a, &[0x11; 8]).unwrap();
        let mut buf = [0u8; 8];
        mem.read(b, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 8], "the second reservation overlapped the first");
    }

    #[test]
    fn a_fixed_placement_onto_our_own_region_is_refused() {
        let mut mem = MmapSpace::new();
        let a = mem.reserve(0x8000, 0x4000, Placement::Anywhere).unwrap();
        assert!(mem.reserve(0x4000, 0x4000, Placement::Fixed(a)).is_err());
    }
}
