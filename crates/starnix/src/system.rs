//! The machine as a whole.

use crate::sys;

/// Physical memory by what it is doing, in bytes.
///
/// A host that does not keep a counter leaves it zero rather than
/// approximating it from a neighbour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Memory {
    pub free: u64,
    pub active: u64,
    pub inactive: u64,
    /// Memory the kernel may not page out.
    pub wired: u64,
    /// File-backed pages, which can be dropped and read back.
    pub file_backed: u64,
    /// Anonymous pages, which can only be swapped.
    pub anonymous: u64,
}

/// Ticks every CPU has spent in each state since boot, summed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CpuTicks {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
}

pub fn page_size() -> u64 {
    // SAFETY: `sysconf` only reads its argument.
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(4096) as u64
}

pub fn memory() -> Memory {
    sys::memory()
}

pub fn cpu_ticks() -> CpuTicks {
    sys::cpu_ticks()
}

/// Physical cores, where the host can tell them from SMT threads.
pub fn physical_cores() -> Option<u32> {
    sys::physical_cores()
}

/// The CPU's marketing name.
pub fn cpu_brand() -> Option<String> {
    sys::cpu_brand()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_has_free_memory_and_has_been_busy() {
        assert!(memory().free > 0);
        let ticks = cpu_ticks();
        assert!(ticks.user + ticks.system + ticks.idle > 0);
    }

    #[test]
    fn there_is_at_least_one_core() {
        assert!(physical_cores().is_none_or(|cores| cores >= 1));
    }
}
