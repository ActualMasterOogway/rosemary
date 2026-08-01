//! Address-space abstraction. The loader maps and binds through this trait, so
//! the *same* linking pipeline can target host memory (where a guest pointer is
//! literally a host pointer) or a plain buffer (for tests and for inspecting an
//! image without giving it any real address space).
//!
//! The one structural departure from `selfish`
//! (`selfish/src/memory/mod.rs:49-70`) is [`Placement`]. `selfish`'s `reserve`
//! always lets the allocator choose an address and computes a load bias from
//! whatever it gets, which is fine for ELF on Linux. Here it is not: Mach-O
//! images carry a preferred `vmaddr` (`0x1_0000_0000` for a typical executable)
//! and the recompiler this crate feeds is identity-mapped — guest VA *is* host
//! VA — so where an image lands is a decision the loader has to be able to
//! express.

mod buffer;
#[cfg(feature = "std")]
mod host_mmap;

pub use buffer::BufferSpace;
#[cfg(feature = "std")]
pub use host_mmap::MmapSpace;

use alloc::vec::Vec;

use crate::error::Result;
use crate::raw::Word;
use crate::spec::constants::{VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE};

/// The running kernel's page size, in bytes.
///
/// This is the granularity `mprotect` actually enforces — 16 KiB on Apple
/// Silicon — which is *not* the granularity an x86_64 Mach-O is laid out at.
/// See [`ProtectionPolicy`].
#[cfg(feature = "std")]
pub fn host_page_size() -> u64 {
    // SAFETY: `sysconf` with a valid name is a pure query with no side effects.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 0x4000 }
}

/// Page protection requested for a mapped region.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Prot {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Prot {
    pub const NONE: Prot = Prot {
        read: false,
        write: false,
        exec: false,
    };
    pub const RO: Prot = Prot {
        read: true,
        write: false,
        exec: false,
    };
    pub const RW: Prot = Prot {
        read: true,
        write: true,
        exec: false,
    };
    pub const RX: Prot = Prot {
        read: true,
        write: false,
        exec: true,
    };
    pub const RWX: Prot = Prot {
        read: true,
        write: true,
        exec: true,
    };

    /// Decode a Mach-O `initprot`/`maxprot` field.
    pub fn from_vm_prot(vm_prot: i32) -> Prot {
        Prot {
            read: vm_prot & VM_PROT_READ != 0,
            write: vm_prot & VM_PROT_WRITE != 0,
            exec: vm_prot & VM_PROT_EXECUTE != 0,
        }
    }

    /// The weakest protection permitting everything either operand permits.
    /// Used to reconcile segments that share a host page.
    pub fn union(self, other: Prot) -> Prot {
        Prot {
            read: self.read || other.read,
            write: self.write || other.write,
            exec: self.exec || other.exec,
        }
    }

    /// This protection with `exec` cleared — what a guest image gets, since the
    /// host CPU cannot execute it anyway.
    pub fn without_exec(self) -> Prot {
        Prot {
            exec: false,
            ..self
        }
    }
}

/// Where an image must land in the address space.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Anywhere the allocator likes; the loader computes a slide.
    Anywhere,
    /// At `addr` if that range is free, otherwise anywhere. The loader records
    /// whatever slide results.
    Preferred(Word),
    /// At `addr` or fail. Required for non-`MH_PIE` images, whose absolute
    /// addresses are baked into their code.
    Fixed(Word),
}

impl Placement {
    /// The requested address, if any.
    pub fn wanted(self) -> Option<Word> {
        match self {
            Placement::Anywhere => None,
            Placement::Preferred(a) | Placement::Fixed(a) => Some(a),
        }
    }

    /// Whether a different address than requested is an error.
    pub fn is_strict(self) -> bool {
        matches!(self, Placement::Fixed(_))
    }
}

/// How to reconcile a Mach-O image's segment boundaries with a coarser host
/// protection granularity.
///
/// This is a real conflict, not a hypothetical one: an x86_64 image is laid out
/// at 4 KiB while Apple Silicon enforces protection at 16 KiB, so `__TEXT` and
/// `__DATA` routinely share a host page. In `/bin/ls`, `__TEXT` ends and
/// `__DATA` begins at `0x1_0000_5000`, both inside host page `0x1_0000_4000`.
/// No assignment of permissions to that page is both correct and enforceable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ProtectionPolicy {
    /// Union the permissions of every segment overlapping each host page.
    /// Never removes a permission something needs; over-permissive at
    /// boundaries. The default, and the only *safe* enforcing option.
    #[default]
    Coalesce,
    /// Do not protect at all; leave everything readable and writable.
    ///
    /// The honest choice for a recompiled guest: the recompiler reads the image
    /// as data and never executes it, so MMU-level protection on those pages
    /// enforces nothing the guest can observe — it only adds ways to crash.
    None,
    /// Apply per-segment, failing loudly when a segment does not start and end
    /// on a host page boundary. For when you actually need MMU enforcement and
    /// would rather know it is unavailable.
    Strict,
}

/// A region of address space the loader can map an image into.
///
/// Addresses passed to [`write`](AddressSpace::write),
/// [`read`](AddressSpace::read), and [`protect`](AddressSpace::protect) are
/// *absolute* within this space — the loader has already applied the slide.
pub trait AddressSpace {
    /// Reserve `size` bytes with at least `align` alignment, honouring
    /// `placement`, and return the base address chosen. Reserved memory reads
    /// back as zero.
    fn reserve(&mut self, size: usize, align: usize, placement: Placement) -> Result<Word>;

    /// Copy `data` into the space starting at absolute address `addr`.
    fn write(&mut self, addr: Word, data: &[u8]) -> Result<()>;

    /// Read `out.len()` bytes from absolute address `addr`.
    fn read(&self, addr: Word, out: &mut [u8]) -> Result<()>;

    /// Set protection on `[addr, addr+len)`. May be a no-op for spaces that do
    /// not model protection.
    fn protect(&mut self, addr: Word, len: usize, prot: Prot) -> Result<()>;

    /// The granularity [`protect`](Self::protect) actually enforces.
    fn protect_granularity(&self) -> u64 {
        0x1000
    }

    /// If this space is host-native, the host pointer for `addr`.
    ///
    /// Returns `None` for spaces that are not host memory. Note that when this
    /// *does* return a pointer the answer is the identity: a host-native space
    /// maps a guest image at its own VA, which is what lets a resolved host
    /// function pointer be written straight into a guest GOT slot.
    fn host_ptr(&self, addr: Word) -> Option<*mut u8> {
        let _ = addr;
        None
    }
}

/// An address space that can hand its mapped bytes back as slices.
///
/// Separate from [`AddressSpace`] because it is a *stronger* promise: an
/// address space need only be able to copy bytes in and out at an address,
/// while this says the bytes exist contiguously somewhere the caller may look
/// at directly. A recompiler wants the second — it is handed a region's
/// contents once and decodes from them — and a loader only ever needs the
/// first, so the two are kept apart rather than merged for convenience.
///
/// For a host-native space the returned slice aliases live memory that guest
/// execution can write to. Nothing here can prevent that; it is the same
/// aliasing the identity mapping is *for*. Callers that need a stable view
/// (a lifter's constant-folding input, say) must copy.
pub trait Regions: AddressSpace {
    /// The region containing `addr`, as `(base, bytes)`.
    fn region_at(&self, addr: Word) -> Option<(Word, &[u8])>;

    /// Every region, in reservation order.
    fn regions(&self) -> Vec<(Word, &[u8])>;
}

/// Round `v` down to a multiple of `align`, which must be a power of two.
pub(crate) fn align_down(v: u64, align: u64) -> u64 {
    v & !(align - 1)
}

/// Round `v` up to a multiple of `align`, which must be a power of two.
pub(crate) fn align_up(v: u64, align: u64) -> u64 {
    v.wrapping_add(align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prot_decodes_vm_prot() {
        assert_eq!(Prot::from_vm_prot(0), Prot::NONE);
        assert_eq!(Prot::from_vm_prot(VM_PROT_READ), Prot::RO);
        assert_eq!(Prot::from_vm_prot(VM_PROT_READ | VM_PROT_WRITE), Prot::RW);
        assert_eq!(Prot::from_vm_prot(VM_PROT_READ | VM_PROT_EXECUTE), Prot::RX);
    }

    #[test]
    fn prot_union_never_removes_a_permission() {
        assert_eq!(Prot::RX.union(Prot::RW), Prot::RWX);
        assert_eq!(Prot::RO.union(Prot::NONE), Prot::RO);
    }

    #[test]
    fn alignment_helpers() {
        assert_eq!(align_down(0x1234, 0x1000), 0x1000);
        assert_eq!(align_up(0x1234, 0x1000), 0x2000);
        assert_eq!(align_up(0x2000, 0x1000), 0x2000);
        assert_eq!(align_down(0x1_0000_5000, 0x4000), 0x1_0000_4000);
    }
}
