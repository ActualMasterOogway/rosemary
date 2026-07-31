//! The x86-64 guest.

use crate::spec::constants::{
    subtype_of, CPU_SUBTYPE_X86_64_ALL, CPU_SUBTYPE_X86_64_H, CPU_TYPE_X86_64,
};
use crate::spec::MachSpec;

/// x86-64 Mach-O: 4 KiB segment granularity, and — on an Apple Silicon host —
/// *not* natively executable. Its instructions are input to a recompiler, so
/// the loader treats the image as data throughout.
#[derive(Copy, Clone, Debug, Default)]
pub struct X86_64;

impl X86_64 {
    pub const DEFAULT_PAGE_SIZE: u64 = 0x1000;
}

impl MachSpec for X86_64 {
    const CPU_TYPE: i32 = CPU_TYPE_X86_64;

    fn page_size(&self) -> u64 {
        Self::DEFAULT_PAGE_SIZE
    }

    fn accepts_subtype(&self, cpusubtype: i32) -> bool {
        matches!(
            subtype_of(cpusubtype),
            CPU_SUBTYPE_X86_64_ALL | CPU_SUBTYPE_X86_64_H
        )
    }

    fn subtype_rank(&self, cpusubtype: i32) -> u32 {
        // Prefer the baseline slice. `x86_64h` assumes Haswell — AVX2, BMI1/2,
        // FMA — and the lifter implements none of those yet, so picking it when
        // a baseline slice exists would trade working code for a decode error.
        if subtype_of(cpusubtype) == CPU_SUBTYPE_X86_64_ALL {
            1
        } else {
            0
        }
    }
}
