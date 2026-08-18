//! `mmap` and `mprotect`: a guest that generates its own code.
//!
//! ## Why these cannot be passthrough
//!
//! Everything else about memory mapping survives the seam untouched — a guest
//! allocation is a host allocation at the same address. Executable permission
//! does not, and the reason is the whole shape of this runtime: **guest code is
//! never executed by this CPU.** It is x86-64; it reaches the recompiler as
//! *data*, exactly as [the no-`MAP_JIT` decision] says of a loaded image. A page
//! the guest calls executable is a page this runtime will *read* and lift.
//!
//! [the no-`MAP_JIT` decision]: ../../../../docs/ROADMAP.md
//!
//! So `PROT_EXEC` is not merely unnecessary here, it is actively harmful, and
//! Terraria is where that stops being theory. It embeds Mono, whose JIT asks for
//! code memory twice:
//!
//! ```text
//! mmap(0, 0x4000,  WRITE|EXEC,      PRIVATE|ANON)      -> EACCES
//! mmap(0, 0x40000, READ|WRITE|EXEC, PRIVATE|ANON|JIT)  -> 0x10b67c000
//! ```
//!
//! The first is refused because Apple Silicon does not hand out W^X memory
//! without `MAP_JIT`. The second succeeds — and then the guest's very first
//! store into it takes `SIGBUS`, `KERN_PROTECTION_FAILURE`, with no diagnostic
//! naming any of this. A `MAP_JIT` region on this architecture starts
//! *write-protected* for the calling thread, and is made writable by
//! `pthread_jit_write_protect_np(0)` — which the **x86-64** Mono never calls,
//! because on the Intel hardware it was compiled for `MAP_JIT` memory is plainly
//! readable, writable and executable and the toggle is an arm64 intrinsic that
//! does not exist there. The guest is not wrong; it is running on a CPU it was
//! not built for, which is this project's entire premise.
//!
//! ## What the shim does
//!
//! Drops `PROT_EXEC` from the request and `MAP_JIT` from the flags, so the guest
//! is handed ordinary anonymous memory it can write without ceremony. Because a
//! page the guest means to execute is one this runtime must be able to *read*,
//! `PROT_EXEC` is translated to `PROT_READ` rather than simply erased — a
//! `WRITE|EXEC` request that would otherwise yield write-only memory the lifter
//! could not decode. Read and write permission are otherwise passed through
//! exactly as asked, so a guest that `mprotect`s its code read-only after
//! emitting it still gets memory it cannot write.
//!
//! With `MAP_JIT` gone the first request above succeeds on its own terms and the
//! fallback is never taken, which is the more direct answer as well as the
//! working one. It also makes the claim in
//! [`pthread_jit_write_protect_np`](super::process::pthread_jit_write_protect_np)
//! — "the guest has no separately protected JIT mapping to toggle" — true rather
//! than merely convenient: it is this shim that removes the mapping the guest
//! would otherwise have to toggle, and a guest toggle reaching the host would
//! unprotect *microlift's* pages, not the guest's.
//!
//! ## Scope
//!
//! What is *in* those pages afterwards is a separate question this shim does not
//! answer. The code cache never invalidates a translation, so a guest that
//! regenerates code at an address it already ran is served the stale one — see
//! the self-modifying-code note in the roadmap. This module is about being able
//! to write the bytes at all.
//!
//! `ROSETTA3_TRACE_VM=1` prints every request, its rewrite, and its result. The
//! trace is how the two lines above were obtained rather than guessed at, and a
//! JIT that stops working is the case where seeing them again is the whole
//! diagnosis.

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
/// [The roadmap][r] states the problem in its smallest form — `__text` contains
/// data, and no static rule separates the two — and this runtime's answer has
/// always been to lift *from a PC*, demand-driven. A guest that generates code
/// poses the same question one level out: given an address in anonymous memory,
/// is it a function pointer to lift or a host pointer to call?
///
/// [r]: ../../../../docs/ROADMAP.md
///
/// The guest answers it itself, and it does so before the question is asked. A
/// program that intends to execute a page requests `PROT_EXEC` for it — that is
/// what the permission *means*, and it is a statement of intent this runtime
/// intercepts anyway in order to strip. So the arenas are exactly the mappings
/// the guest declared executable: no heuristic, no address-range guess, and
/// nothing an ordinary data allocation can fall into. A guest that jumps into a
/// page it never asked to execute is still `UnknownIndirectCall` by name, which
/// is the right answer, because on real hardware it would have faulted.
///
/// Append-only and never removed, matching [`crate::dyld`]'s images and for the
/// same reason: the code cache holds translations keyed by address, so a range
/// that stopped being an arena would be one whose translations outlive it. See
/// the scope note in the module header.
static ARENAS: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// The lowest and highest arena address, for the question asked on the hot path.
///
/// [`crate::Program::is_guest_address`] is consulted at every indirect call, and
/// answers "no" almost every time. Two relaxed loads reject a host address
/// before any lock is taken — the same shape, and the same reasoning, as
/// [`crate::dyld`]'s extents.
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
///
/// `PROT_EXEC` becomes `PROT_READ` — the permission this runtime needs to lift
/// the page — and every other bit is passed through. Kept separate from the
/// calls so it can be tested without mapping anything.
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

/// Whether `ROSETTA3_TRACE_VM` asked for the guest's use of memory to be
/// reported: its mappings, the code it generates into them, the images it loads,
/// and the regions this runtime declines to compile.
///
/// Read once and cached, because some of its callers sit on the path taken for
/// *every* generated region and a `getenv` each time would be a cost paid by
/// runs that never look at the output.
///
/// It lives here rather than in a diagnostics module because this is where the
/// question starts: a JIT guest that goes wrong goes wrong about memory, and the
/// mapping trace is the first thing worth seeing.
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

    /// The fault this module exists for, reproduced and then not reproduced:
    /// a `MAP_JIT` region cannot be written by a thread that has not turned its
    /// write protection off, and the rewritten request yields memory that can.
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
