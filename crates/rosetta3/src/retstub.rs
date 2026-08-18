//! One byte of guest code that means "the call this runtime made is finished".

use static_init::dynamic;

/// The one-byte x86-64 breakpoint, which is the whole of the stub.
const INT3: u8 = 0xCC;

/// The page, mapped once on the first call into guest code.
#[dynamic]
static STUB: u64 = map().expect("rosetta3: could not map the guest return stub");

/// The address every entry into guest code pushes as its return address.
///
/// Mapped on first use and never unmapped:
///
/// Panics if the page cannot be mapped, which is a process that cannot run a single
/// guest call — there is no return address to push and nothing further to try.
pub fn address() -> u64 {
    *STUB
}

/// Whether `va` is the return stub — that is, whether a breakpoint there is the
/// end of a call this runtime made.
pub fn is_exit(va: u64) -> bool {
    *STUB == va
}

/// Whether `va` is guest code this module owns.
pub fn contains(va: u64) -> bool {
    is_exit(va)
}

/// The region to lift in order to execute the stub at `va`.
///
/// One byte, copied out rather than borrowed, so the walk's upper bound is the
/// instruction itself and nothing follows it. Every other address in the page is
/// refused:
pub fn lift_region(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    if !is_exit(va) {
        return None;
    }
    let bytes = [INT3];
    let region = crate::region::region_for(&bytes, va, va, false)?;
    Some((region, bytes.to_vec()))
}

/// `va` as a name, for a diagnostic. See [`crate::program::Program::locate`].
pub fn locate(va: u64) -> Option<String> {
    is_exit(va).then(|| "<return stub>".to_string())
}

/// Map the page and fill it with breakpoints.
///
/// Readable and writable by this runtime and never executable, which is the decision
/// every guest mapping is made under:
fn map() -> Option<u64> {
    let size = machish::memory::host_page_size() as usize;
    // SAFETY: an anonymous private mapping of one page at an address of the
    // kernel's choosing. Nothing is assumed about where it lands.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: the page just mapped, writable and owned by this module. Written
    // before the address is published, so nothing can read it half-filled.
    unsafe { std::ptr::write_bytes(base as *mut u8, INT3, size) };
    Some(base as usize as u64)
}

#[cfg(test)]
mod tests {
    /// The address is stable and the byte behind it is the breakpoint every part of
    /// this rests on:
    #[test]
    fn the_stub_is_one_breakpoint_at_a_stable_address() {
        let va = super::address();
        assert_eq!(va, super::address(), "the stub moved between calls");
        assert!(super::is_exit(va));
        assert!(!super::is_exit(va + 1), "only the first byte is the stub");

        let (region, bytes) = super::lift_region(va).expect("the stub is liftable");
        assert_eq!(bytes, vec![super::INT3]);
        assert_eq!(
            region.range,
            va..va + 1,
            "the region is the one instruction"
        );
        assert!(super::lift_region(va + 1).is_none());
    }
}
