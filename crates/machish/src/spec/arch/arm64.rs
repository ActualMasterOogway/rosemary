//! The arm64 host architecture.
//!
//! Present not to *run* arm64 images — the host's own dyld does that — but to
//! inspect them: enumerating the slices of a fat bundle dylib, or reading a
//! native library's export trie to check whether it really defines a symbol
//! before binding a guest import to it.

use crate::spec::constants::{
    subtype_of, CPU_SUBTYPE_ARM64_ALL, CPU_SUBTYPE_ARM64E, CPU_TYPE_ARM64,
};
use crate::spec::MachSpec;

/// arm64 Mach-O: 16 KiB segment granularity.
#[derive(Copy, Clone, Debug, Default)]
pub struct Arm64;

impl Arm64 {
    pub const DEFAULT_PAGE_SIZE: u64 = 0x4000;
}

impl MachSpec for Arm64 {
    const CPU_TYPE: i32 = CPU_TYPE_ARM64;

    fn page_size(&self) -> u64 {
        Self::DEFAULT_PAGE_SIZE
    }

    fn accepts_subtype(&self, cpusubtype: i32) -> bool {
        // `arm64e` is accepted for *parsing* only. Its pointer-authenticated
        // chained fixups are rejected by the fixup decoder, which is the right
        // place for that refusal: reading an arm64e dylib's exports is useful,
        // applying its fixups is not something we can do correctly.
        matches!(
            subtype_of(cpusubtype),
            CPU_SUBTYPE_ARM64_ALL | CPU_SUBTYPE_ARM64E
        )
    }

    fn subtype_rank(&self, cpusubtype: i32) -> u32 {
        if subtype_of(cpusubtype) == CPU_SUBTYPE_ARM64_ALL {
            1
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_arm64e_with_capability_bits_set() {
        // Regression: the capability bits live in the *top* byte of
        // `cpusubtype`, not bits 16-23. `/bin/ls`'s arm64e slice reports
        // 0x8000_0002 (ABI version 0x80 | CPU_SUBTYPE_ARM64E), and masking the
        // wrong byte made it compare unequal to CPU_SUBTYPE_ARM64E, so the
        // slice was reported as missing entirely.
        assert!(Arm64.accepts_subtype(0x8000_0002u32 as i32));
        assert!(Arm64.accepts_subtype(CPU_SUBTYPE_ARM64E));
        assert!(Arm64.accepts_subtype(CPU_SUBTYPE_ARM64_ALL));
        assert!(!Arm64.accepts_subtype(0x0000_0009));
    }

    #[test]
    fn prefers_baseline_over_arm64e() {
        assert!(
            Arm64.subtype_rank(CPU_SUBTYPE_ARM64_ALL)
                > Arm64.subtype_rank(0x8000_0002u32 as i32)
        );
    }
}
