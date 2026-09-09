#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use static_init::dynamic;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[dynamic]
pub static HAS_FSGSBASE: bool = {
    /// Linux's `HWCAP2_FSGSBASE`, from `<asm/hwcap2.h>`.
    ///
    /// CPUID alone is not enough: the instructions fault unless the kernel also
    /// enabled CR4.FSGSBASE. `AT_HWCAP2` reports that userspace may actually use
    /// them.
    const HWCAP2_FSGSBASE: libc::c_ulong = 1 << 1;

    // SAFETY: `getauxval` reads the process's immutable ELF auxiliary vector.
    unsafe { libc::getauxval(libc::AT_HWCAP2) & HWCAP2_FSGSBASE != 0 }
};
