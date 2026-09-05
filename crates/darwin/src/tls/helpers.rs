#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use static_init::dynamic;

#[cfg(target_arch = "x86_64")]
#[dynamic]
pub static HAS_FSGSBASE: bool = {
    use std::arch::x86_64::{__cpuid, __cpuid_count};

    let max_leaf = __cpuid(0).eax;
    max_leaf >= 7 && (__cpuid_count(7, 0).ebx & (1 << 0)) != 0
};
