//! Fat (universal) archive demultiplexing.
//!
//! A fat file is a thin big-endian directory prepended to several complete
//! Mach-O images. It has no ELF analogue, and it sits *below* the spec layer:
//! choosing a slice happens before there is an image to parse, so this module
//! deliberately does not take a [`MachSpec`](crate::spec::MachSpec) for its
//! enumeration path — an embedder listing what architectures a bundle supports
//! should not have to pick one first.
//!
//! Note the endianness: `fat_header` and `fat_arch` are big-endian on disk even
//! on little-endian targets, a NeXT-era decision that outlived its reason.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::raw::View;
use crate::spec::MachSpec;
use crate::spec::constants::{
    FAT_ARCH_64_SIZE, FAT_ARCH_SIZE, FAT_HEADER_SIZE, FAT_MAGIC, FAT_MAGIC_64, MH_CIGAM,
    MH_CIGAM_64, MH_MAGIC, MH_MAGIC_64,
};

/// A guard against absurd slice counts in a malformed header. Real universal
/// binaries carry a handful; anything near this is an attack or a mis-parse.
const MAX_SLICES: u32 = 64;

/// One architecture's extent within a fat file.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Slice {
    pub cputype: i32,
    pub cpusubtype: i32,
    /// Offset of the thin image within the fat file.
    pub offset: usize,
    /// Length of the thin image.
    pub size: usize,
    /// Alignment as a power of two (`2^align`).
    pub align: u32,
}

/// What a candidate set of bytes turned out to be.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A single Mach-O image, no fat wrapper.
    Thin,
    /// A fat archive with 32-bit offsets.
    Fat,
    /// A fat archive with 64-bit offsets.
    Fat64,
}

/// Identify `bytes` without parsing further.
pub fn kind(bytes: &[u8]) -> Result<Kind> {
    let view = View::new(bytes);
    // The fat magic is read big-endian, the Mach-O magic little-endian; both
    // are checked against the same four bytes.
    match view.u32_be(0)? {
        FAT_MAGIC => return Ok(Kind::Fat),
        FAT_MAGIC_64 => return Ok(Kind::Fat64),
        _ => {}
    }
    match view.u32(0)? {
        MH_MAGIC_64 => Ok(Kind::Thin),
        other => Err(Error::BadMagic(other)),
    }
}

/// Enumerate every slice in `bytes`.
///
/// A thin Mach-O yields a single slice covering the whole input, so callers can
/// treat both shapes uniformly. Each slice's extent is validated against the
/// file length, so a returned `Slice` can always be sub-sliced safely.
pub fn slices(bytes: &[u8]) -> Result<Vec<Slice>> {
    let view = View::new(bytes);
    let (entry_size, is_64) = match kind(bytes)? {
        Kind::Thin => {
            // A thin file still has a cputype/cpusubtype worth reporting.
            return Ok(alloc::vec![Slice {
                cputype: view.i32(4)?,
                cpusubtype: view.i32(8)?,
                offset: 0,
                size: bytes.len(),
                align: 0,
            }]);
        }
        Kind::Fat => (FAT_ARCH_SIZE, false),
        Kind::Fat64 => (FAT_ARCH_64_SIZE, true),
    };

    let nfat = view.u32_be(4)?;
    if nfat > MAX_SLICES {
        return Err(Error::TooDeep("fat slice count"));
    }

    let mut out = Vec::with_capacity(nfat as usize);
    for i in 0..nfat as usize {
        let off = FAT_HEADER_SIZE
            .checked_add(i.checked_mul(entry_size).ok_or(Error::BadFatSlice)?)
            .ok_or(Error::BadFatSlice)?;

        let cputype = view.u32_be(off)? as i32;
        let cpusubtype = view.u32_be(off + 4)? as i32;
        let (offset, size, align) = if is_64 {
            (
                view.u64_be(off + 8)?,
                view.u64_be(off + 16)?,
                view.u32_be(off + 24)?,
            )
        } else {
            (
                u64::from(view.u32_be(off + 8)?),
                u64::from(view.u32_be(off + 12)?),
                view.u32_be(off + 16)?,
            )
        };

        let offset = usize::try_from(offset).map_err(|_| Error::BadFatSlice)?;
        let size = usize::try_from(size).map_err(|_| Error::BadFatSlice)?;
        // Validate now so every `Slice` handed out is safe to sub-slice.
        let end = offset.checked_add(size).ok_or(Error::BadFatSlice)?;
        if end > bytes.len() {
            return Err(Error::BadFatSlice);
        }

        out.push(Slice {
            cputype,
            cpusubtype,
            offset,
            size,
            align,
        });
    }
    Ok(out)
}

/// Extract the bytes of a slice previously returned by [`slices`].
pub fn slice_bytes<'a>(bytes: &'a [u8], slice: &Slice) -> Result<&'a [u8]> {
    bytes
        .get(slice.offset..slice.offset + slice.size)
        .ok_or(Error::BadFatSlice)
}

/// Select the best slice for `spec` and return its bytes.
///
/// A thin file matching the spec passes through unchanged. Among acceptable
/// slices the one with the highest [`MachSpec::subtype_rank`] wins, which is how
/// a baseline `x86_64` slice is preferred over `x86_64h`.
pub fn select<'a, S: MachSpec>(bytes: &'a [u8], spec: &S) -> Result<&'a [u8]> {
    let all = slices(bytes)?;
    let best = all
        .iter()
        .filter(|s| s.cputype == S::CPU_TYPE && spec.accepts_subtype(s.cpusubtype))
        .max_by_key(|s| spec.subtype_rank(s.cpusubtype))
        .ok_or(Error::NoMatchingSlice {
            wanted: S::CPU_TYPE,
        })?;
    slice_bytes(bytes, best)
}

/// Whether `magic` is a Mach-O form we recognize but refuse.
///
/// Reported separately from an unrecognized magic so the error can say *why*:
/// a 32-bit or byte-swapped Mach-O is a real Mach-O, just not one we load.
pub fn is_rejected_macho_magic(magic: u32) -> bool {
    matches!(magic, MH_MAGIC | MH_CIGAM | MH_CIGAM_64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::constants::{CPU_SUBTYPE_X86_64_ALL, CPU_TYPE_ARM64, CPU_TYPE_X86_64};

    /// A minimal fat header with `n` slices, big-endian.
    fn fat_header(n: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    fn fat_arch(cputype: i32, cpusubtype: i32, offset: u32, size: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(cputype as u32).to_be_bytes());
        v.extend_from_slice(&(cpusubtype as u32).to_be_bytes());
        v.extend_from_slice(&offset.to_be_bytes());
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&12u32.to_be_bytes());
        v
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert!(kind(&[]).is_err());
        assert!(kind(&[0, 1, 2, 3]).is_err());
        assert_eq!(
            kind(&0xdead_beefu32.to_le_bytes()),
            Err(Error::BadMagic(0xdead_beef))
        );
    }

    #[test]
    fn thin_file_is_a_single_slice() {
        let mut v = Vec::new();
        v.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        v.extend_from_slice(&CPU_TYPE_X86_64.to_le_bytes());
        v.extend_from_slice(&CPU_SUBTYPE_X86_64_ALL.to_le_bytes());
        v.resize(64, 0);

        assert_eq!(kind(&v).unwrap(), Kind::Thin);
        let s = slices(&v).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].cputype, CPU_TYPE_X86_64);
        assert_eq!(s[0].offset, 0);
        assert_eq!(s[0].size, v.len());
    }

    #[test]
    fn fat_slices_are_enumerated() {
        let mut v = fat_header(2);
        v.extend(fat_arch(
            CPU_TYPE_X86_64,
            CPU_SUBTYPE_X86_64_ALL,
            0x1000,
            0x10,
        ));
        v.extend(fat_arch(CPU_TYPE_ARM64, 0, 0x2000, 0x20));
        v.resize(0x2020, 0);

        let s = slices(&v).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].cputype, CPU_TYPE_X86_64);
        assert_eq!(s[0].offset, 0x1000);
        assert_eq!(s[1].cputype, CPU_TYPE_ARM64);
        assert_eq!(s[1].size, 0x20);
    }

    #[test]
    fn rejects_slice_running_past_end() {
        let mut v = fat_header(1);
        v.extend(fat_arch(
            CPU_TYPE_X86_64,
            CPU_SUBTYPE_X86_64_ALL,
            0x1000,
            0x10,
        ));
        v.resize(0x100, 0); // far too short for the declared extent
        assert_eq!(slices(&v), Err(Error::BadFatSlice));
    }

    #[test]
    fn rejects_absurd_slice_count() {
        // nfat_arch of u32::MAX would otherwise drive a huge allocation.
        let v = fat_header(u32::MAX);
        assert_eq!(slices(&v), Err(Error::TooDeep("fat slice count")));
    }

    #[test]
    fn rejects_truncated_arch_table() {
        // Claims two slices but only supplies bytes for part of one.
        let mut v = fat_header(2);
        v.extend_from_slice(&[0u8; 8]);
        assert!(slices(&v).is_err());
    }

    #[cfg(feature = "arch-x86_64")]
    #[test]
    fn select_prefers_baseline_over_haswell() {
        use crate::spec::arch::X86_64;
        use crate::spec::constants::CPU_SUBTYPE_X86_64_H;

        let mut v = fat_header(2);
        // Haswell listed first, so a naive "first match" would pick it.
        v.extend(fat_arch(CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_H, 0x1000, 4));
        v.extend(fat_arch(CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL, 0x2000, 4));
        v.resize(0x2010, 0);
        v[0x1000..0x1004].copy_from_slice(b"HASW");
        v[0x2000..0x2004].copy_from_slice(b"BASE");

        assert_eq!(select(&v, &X86_64).unwrap(), b"BASE");
    }

    #[cfg(feature = "arch-x86_64")]
    #[test]
    fn select_reports_a_missing_architecture() {
        use crate::spec::arch::X86_64;
        let mut v = fat_header(1);
        v.extend(fat_arch(CPU_TYPE_ARM64, 0, 0x1000, 4));
        v.resize(0x1010, 0);
        assert_eq!(
            select(&v, &X86_64),
            Err(Error::NoMatchingSlice {
                wanted: CPU_TYPE_X86_64
            })
        );
    }
}
