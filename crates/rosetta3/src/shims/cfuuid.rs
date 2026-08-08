//! `CFUUIDBytes`: sixteen bytes both ABIs agree about, and neither table can
//! name.
//!
//! ```c
//! CFUUIDBytes CFUUIDGetUUIDBytes(CFUUIDRef uuid);
//! CFUUIDRef   CFUUIDCreateFromUUIDBytes(CFAllocatorRef alloc, CFUUIDBytes bytes);
//! ```
//!
//! A `CFUUIDBytes` is sixteen `UInt8`s. SysV AMD64 classifies sixteen bytes of
//! integers as INTEGER,INTEGER — `rax`/`rdx` returning, and the next two
//! argument registers going out. AAPCS64 sees a sixteen-byte non-floating
//! aggregate and does the same in `x0`/`x1`. **The two ABIs agree**, which is
//! the unusual case here and the reason this is six lines rather than the
//! `NSRect` problem.
//!
//! What they agree about still cannot be written in [`crate::abi::table`],
//! whose entries are scalars: `sig!` has no way to say "two eightbytes returned
//! in a pair of registers", and an entry claiming `U64` would take the low half
//! and leave the high half whatever the last instruction wrote. That is the
//! same gap [`super::math`]'s `sincos` and [`super::coregraphics`]'s
//! `CGDisplayScreenSize` fall through, and this is the third instance of it —
//! the shape is now common enough to be worth naming rather than rediscovering.
//!
//! Reached by Unity's analytics, which identifies an install with a UUID and
//! asks for its bytes to hash. It is not optional: the call is on the path
//! between the window opening and the first frame.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

/// Sixteen bytes as the two ABIs carry them: a pair of doublewords.
///
/// `#[repr(C)]` and exactly two `u64`s, so Rust returns it in `x0`/`x1` and
/// passes it in the next two integer registers, which is what the host's own
/// declaration does with the `unsigned char [16]` it really is. Little-endian
/// is why the byte order needs no thought: the guest reads the same sixteen
/// bytes back out of `rax`/`rdx` in the order the host wrote them.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Bytes {
    lo: u64,
    hi: u64,
}

/// `CFUUIDBytes CFUUIDGetUUIDBytes(CFUUIDRef)`
fn get_uuid_bytes(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: the symbol [`shim_for`] matched, which is
    // `CFUUIDBytes (*)(CFUUIDRef)`, resolved out of a CoreFoundation this
    // process has loaded. The `CFUUIDRef` is a host object the guest received
    // from `CFUUIDCreate`.
    let f: extern "C" fn(u64) -> Bytes =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64) -> Bytes>(host) };
    let bytes = f(ctx.rdi);
    ctx.rax = bytes.lo;
    ctx.rdx = bytes.hi;
    Ok(())
}

/// `CFUUIDRef CFUUIDCreateFromUUIDBytes(CFAllocatorRef, CFUUIDBytes)`
///
/// The mirror image: the allocator is the first integer argument and the
/// sixteen bytes are the second and third, on both sides.
fn create_from_uuid_bytes(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    let host = host_or_refuse(symbol)?;
    // SAFETY: as above, for `CFUUIDRef (*)(CFAllocatorRef, CFUUIDBytes)`.
    let f: extern "C" fn(u64, Bytes) -> u64 =
        unsafe { core::mem::transmute::<u64, extern "C" fn(u64, Bytes) -> u64>(host) };
    ctx.rax = f(
        ctx.rdi,
        Bytes {
            lo: ctx.rsi,
            hi: ctx.rdx,
        },
    );
    Ok(())
}

/// The host's address for one of this module's names.
fn host_or_refuse(symbol: &str) -> Result<u64> {
    crate::dyld::host_symbol(symbol.strip_prefix('_').unwrap_or(symbol)).ok_or(AbiError::NoShim {
        symbol: symbol.to_string(),
    })
}

pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "CFUUIDGetUUIDBytes" => Some(get_uuid_bytes),
        "CFUUIDCreateFromUUIDBytes" => Some(create_from_uuid_bytes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip, through the host's own CoreFoundation.
    ///
    /// Checked rather than asserted, because the whole claim is that sixteen
    /// bytes survive a pair of registers in both directions: a wrong half would
    /// still produce a `CFUUIDRef`, and only comparing the bytes back out
    /// catches it.
    #[test]
    fn sixteen_bytes_survive_the_round_trip() {
        let Some(create) = crate::dyld::host_symbol("CFUUIDCreateFromUUIDBytes") else {
            return;
        };
        let Some(get) = crate::dyld::host_symbol("CFUUIDGetUUIDBytes") else {
            return;
        };
        let want = Bytes {
            lo: 0x0011_2233_4455_6677,
            hi: 0x8899_aabb_ccdd_eeff,
        };
        // SAFETY: both are the CoreFoundation entry points named, resolved from
        // this process's own image list.
        let (create, get) = unsafe {
            (
                core::mem::transmute::<u64, extern "C" fn(u64, Bytes) -> u64>(create),
                core::mem::transmute::<u64, extern "C" fn(u64) -> Bytes>(get),
            )
        };
        let uuid = create(0, want);
        assert_ne!(uuid, 0, "CoreFoundation made no UUID");
        let back = get(uuid);
        assert_eq!(back.lo, want.lo, "the low eight bytes changed");
        assert_eq!(back.hi, want.hi, "the high eight bytes changed");
    }
}
