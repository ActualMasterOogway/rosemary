//! `mmap` and `mprotect`: a guest that generates its own code.

use microlift::arch::x86_64::context::RegisterContext;
use static_init::dynamic;

use crate::abi::{GuestMemory, Result};

use super::set_int_return;

/// `MAP_JIT`. Not in the `libc` crate's Darwin bindings, and it is the flag this
/// module exists to remove, so it is spelled out with the value from `mman.h`.
const MAP_JIT: i32 = 0x0800;

/// The ranges the guest asked to be executable.
///
/// ## Why this list is the right answer to "which bytes are code"
///
/// [The roadmap][r] states the problem in its smallest form — `__text` contains data,
/// and no static rule separates the two — and this runtime's answer has always been to
/// lift *from a PC*, demand-driven.
///
/// Append-only and never removed, matching [`crate::dyld`]'s images and for the same
/// reason.
static ARENAS: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// The lowest and highest arena address, for the question asked on the hot path.
static ARENA_LO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
static ARENA_HI: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record `[base, base + len)` as memory the guest means to execute.
fn remember_arena(base: u64, len: u64) {
    use std::sync::atomic::Ordering;
    if base == 0 || len == 0 {
        return;
    }
    let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
    if arenas
        .iter()
        .any(|&(b, l)| base >= b && base + len <= b + l)
    {
        return;
    }
    arenas.push((base, len));
    ARENA_LO.fetch_min(base, Ordering::Relaxed);
    ARENA_HI.fetch_max(base + len, Ordering::Relaxed);
}

/// The arena containing `va`, if any.
pub fn arena_at(va: u64) -> Option<(u64, u64)> {
    use std::sync::atomic::Ordering;
    if va < ARENA_LO.load(Ordering::Relaxed) || va >= ARENA_HI.load(Ordering::Relaxed) {
        return None;
    }
    ARENAS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .find(|&(base, len)| va >= base && va < base + len)
}

/// Whether `va` is in memory the guest declared executable.
pub fn contains(va: u64) -> bool {
    arena_at(va).is_some()
}

/// The shim for `name`, if this module has one.
pub fn shim_for(
    name: &str,
) -> Option<fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>> {
    match name {
        "mmap" => Some(mmap),
        "mprotect" => Some(mprotect),
        _ => None,
    }
}

/// The protection to actually request for a guest's `prot`.
pub(crate) fn host_prot(prot: i32) -> i32 {
    match prot & libc::PROT_EXEC {
        0 => prot,
        _ => (prot & !libc::PROT_EXEC) | libc::PROT_READ,
    }
}

/// The flags to actually request for a guest's `flags`.
pub(crate) fn host_flags(flags: i32) -> i32 {
    flags & !MAP_JIT
}

/// `void *mmap(void *addr, size_t len, int prot, int flags, int fd, off_t off)`
fn mmap(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let (addr, len) = (ctx.rdi as usize, ctx.rsi as usize);
    let (prot, flags) = (ctx.rdx as i32, ctx.rcx as i32);
    let (fd, off) = (ctx.r8 as i32, ctx.r9 as i64);
    let (want_prot, want_flags) = (host_prot(prot), host_flags(flags));

    // SAFETY: the guest's own request, with executable permission removed. The
    // address and length are the guest's to choose; the kernel judges both.
    let p = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            len,
            want_prot,
            want_flags,
            fd,
            off,
        )
    };

    // The guest said it means to execute these bytes; see [`ARENAS`].
    if prot & libc::PROT_EXEC != 0 && p != libc::MAP_FAILED {
        remember_arena(p as usize as u64, len as u64);
    }

    if tracing() {
        eprintln!(
            "rosetta3: mmap({addr:#x}, {len:#x}, {}, {}, {fd}, {off:#x}) as ({}, {}) -> {}",
            prot_name(prot),
            flags_name(flags),
            prot_name(want_prot),
            flags_name(want_flags),
            outcome(p as isize)
        );
    }
    ctx.rax = p as u64;
    Ok(())
}

/// `int mprotect(void *addr, size_t len, int prot)`
fn mprotect(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let (addr, len, prot) = (ctx.rdi as usize, ctx.rsi as usize, ctx.rdx as i32);
    let want_prot = host_prot(prot);

    // SAFETY: the guest's own request, with executable permission removed.
    let rc = unsafe { libc::mprotect(addr as *mut libc::c_void, len, want_prot) };

    if prot & libc::PROT_EXEC != 0 && rc == 0 {
        remember_arena(addr as u64, len as u64);
    }

    if tracing() {
        eprintln!(
            "rosetta3: mprotect({addr:#x}, {len:#x}, {}) as {} -> {}",
            prot_name(prot),
            prot_name(want_prot),
            outcome(rc as isize)
        );
    }
    set_int_return(ctx, rc);
    Ok(())
}

/// Whether `ROSETTA3_TRACE_VM` asked for the guest's use of memory to be reported:
///
/// Read once and cached, because some of its callers sit on the path taken for *every*
/// generated region and a `getenv` each time would be a cost paid by runs that never
/// look at the output.
#[inline(always)]
pub fn tracing() -> bool {
    #[dynamic]
    static ON: bool = std::env::var_os("ROSETTA3_TRACE_VM").is_some();
    *ON
}

/// A return value, with `errno` when it is a failure.
fn outcome(rc: isize) -> String {
    if rc == 0 || (rc != -1 && rc as usize != usize::MAX) {
        return format!("{rc:#x}");
    }
    // SAFETY: `__error` returns this thread's `errno` slot.
    let e = unsafe { *libc::__error() };
    format!("failed, errno {e}")
}

fn prot_name(prot: i32) -> String {
    if prot == 0 {
        return "PROT_NONE".into();
    }
    let mut parts = Vec::new();
    for (bit, name) in [
        (libc::PROT_READ, "READ"),
        (libc::PROT_WRITE, "WRITE"),
        (libc::PROT_EXEC, "EXEC"),
    ] {
        if prot & bit != 0 {
            parts.push(name);
        }
    }
    parts.join("|")
}

fn flags_name(flags: i32) -> String {
    let mut parts = Vec::new();
    for (bit, name) in [
        (libc::MAP_SHARED, "SHARED"),
        (libc::MAP_PRIVATE, "PRIVATE"),
        (libc::MAP_FIXED, "FIXED"),
        (libc::MAP_ANON, "ANON"),
        (MAP_JIT, "JIT"),
    ] {
        if flags & bit != 0 {
            parts.push(name);
        }
    }
    format!("{}({flags:#x})", parts.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_permission_becomes_readable_permission() {
        // The case that produced write-only memory the lifter could not decode.
        assert_eq!(
            host_prot(libc::PROT_WRITE | libc::PROT_EXEC),
            libc::PROT_WRITE | libc::PROT_READ
        );
        assert_eq!(
            host_prot(libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC),
            libc::PROT_READ | libc::PROT_WRITE
        );
        assert_eq!(host_prot(libc::PROT_EXEC), libc::PROT_READ);
    }

    #[test]
    fn a_request_without_executable_permission_is_passed_through_unchanged() {
        for prot in [
            libc::PROT_NONE,
            libc::PROT_READ,
            libc::PROT_WRITE,
            libc::PROT_READ | libc::PROT_WRITE,
        ] {
            assert_eq!(host_prot(prot), prot);
        }
    }

    #[test]
    fn map_jit_is_removed_and_nothing_else_is() {
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | MAP_JIT;
        assert_eq!(host_flags(flags), libc::MAP_PRIVATE | libc::MAP_ANON);
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED;
        assert_eq!(host_flags(flags), flags);
    }

    /// The fault this module exists for, reproduced and then not reproduced.
    #[test]
    fn the_rewritten_request_yields_memory_the_guest_can_write() {
        let (prot, flags) = (
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANON | MAP_JIT,
        );
        // SAFETY: an anonymous mapping of one host page, unmapped below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                0x4000,
                host_prot(prot),
                host_flags(flags),
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED, "rewritten mmap must succeed");
        // SAFETY: `p` is a page this test just mapped readable and writable.
        unsafe {
            std::ptr::write_bytes(p as *mut u8, 0xc3, 0x4000);
            assert_eq!(*(p as *const u8), 0xc3);
            libc::munmap(p, 0x4000);
        }
    }
}
