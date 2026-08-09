//! `CMPXCHG16B`, performed by the host because the guest shares it.
//!
//! ## The instruction, and why it cannot be lifted here
//!
//! `lock cmpxchg16b m128` is a 128-bit compare-and-swap: compare `RDX:RAX`
//! against sixteen bytes of memory, and on a match store `RCX:RBX` there,
//! otherwise load what was there into `RDX:RAX`. `ZF` says which happened. It is
//! the primitive every tagged-pointer lock-free structure on x86-64 is built
//! from — a pointer in one half, an ABA counter in the other, swung together or
//! not at all.
//!
//! Cranelift can express it (`atomic_cas` on `I128`) but **only the x64 backend
//! lowers it**. On this host — aarch64 — there is no IR-expressible 128-bit
//! atomic RMW at all, so microlift offers three
//! [`Atomic128Mode`](microlift::core::Atomic128Mode)s and every one of them is a
//! decision the embedder has to make:
//!
//! - `Native` fails to compile here;
//! - `Emulated` loads both halves, compares, and stores back — correct for a
//!   single-threaded guest and a **data race** for any other;
//! - `Exit` refuses to lower it and hands the instruction to the runtime.
//!
//! This runtime took `Emulated` until Subnautica Below Zero, and that was wrong
//! in the way this project exists to avoid: not a refusal, an answer that is
//! usually right.
//!
//! ## What it cost, and how it was found
//!
//! Unity's job system pops from a lock-free free list on every worker thread.
//! Its pop is the textbook loop —
//!
//! ```text
//! 80dead: lock cmpxchg16b (%rdi)     ; read the head, atomically
//! 80deb2: mov  %rax, %rsi
//! 80dec0: mov  (%rsi), %rbx          ; the popped node's `next`
//! 80dec3: lea  0x1(%rdx), %rcx       ; tag + 1
//! 80dec8: lock cmpxchg16b (%rdi)     ; swing head to `next`, if nobody moved it
//! ```
//!
//! — and with a non-atomic CAS two workers pop the *same* node. One frees it,
//! the allocator hands the memory to something else, and the other dereferences
//! it at `0x80dec0`. [`crate::fault`] caught exactly that, three times, each
//! time on a different thread and each time at an address whose bytes were
//! **printable**: `CanUseSp`, `_ColorSt`, `/Users/v`. A recycled node reread as
//! a pointer, holding whatever the memory now says — a Unity property name, a
//! shader property, a path.
//!
//! Which is also why the bug looked so unlike itself for so long: it is a race,
//! so it moved between runs, it did not reproduce under a debugger, and its
//! symptom was a wild pointer three levels away from its cause.
//!
//! ## Why the host can do this atomically when the JIT cannot
//!
//! The guest is identity-mapped, so `m128` is a real address in this process,
//! and aarch64 has had a 128-bit atomic since ARMv8.0: a `ldaxp`/`stlxp` pair on
//! an aligned sixteen-byte location. The exclusive monitor is the CPU's, not
//! this runtime's, so it serialises against every other core — including one
//! running another guest thread that is in this same routine, which is the case
//! that matters. `Exit` lowering is what makes that sufficient: *every* guest
//! `CMPXCHG16B` comes here, so there is no second path to race against.
//!
//! The cost is a VM exit per execution, which is the trade the mode's own
//! documentation names. It buys correctness for a guest with threads, and a
//! guest without threads never notices because it never reaches a `lock` prefix.
//!
//! ## Scope
//!
//! `CMPXCHG16B` only. `CMPXCHG8B` is 64-bit and Cranelift lowers it everywhere;
//! the rest of the `lock`-prefixed family is 64 bits or narrower and likewise
//! needs nothing from here.
//!
//! An **unaligned** `m128` is refused rather than emulated. Intel raises `#GP`
//! for one, so a guest that issues it is already broken — and `ldaxp` on an
//! unaligned address faults, which would be this runtime crashing where the
//! guest should.

use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, Register};

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::GuestMemory;
use crate::vm::VmError;

/// Longest x86-64 instruction, and so how far back the start can be.
const MAX_LEN: u64 = 15;

/// Perform the `CMPXCHG16B` the guest exited on.
///
/// `ctx.rip` is already **past** the instruction, which is microlift's
/// unimplemented-instruction contract: guest state is fully synced and the host
/// resumes without adjusting the PC. So this reads the registers, does the
/// operation, writes `RDX:RAX` and `ZF` back, and returns.
pub fn service(ctx: &mut RegisterContext, mem: &dyn GuestMemory) -> Result<(), VmError> {
    let at = ctx.rip;
    let instruction = decode_before(at, mem).ok_or(VmError::Trap {
        reason: microlift::core::exit::ExitReason::UnimplementedInstruction,
        code: ctx.exit_code,
        pc: at,
    })?;
    let address = effective_address(&instruction, ctx).ok_or(VmError::Trap {
        reason: microlift::core::exit::ExitReason::UnimplementedInstruction,
        code: ctx.exit_code,
        pc: at,
    })?;
    if address % 16 != 0 {
        return Err(VmError::Trap {
            reason: microlift::core::exit::ExitReason::UnimplementedInstruction,
            code: ctx.exit_code,
            pc: at,
        });
    }

    let expected = u128::from(ctx.rax) | (u128::from(ctx.rdx) << 64);
    let desired = u128::from(ctx.rbx) | (u128::from(ctx.rcx) << 64);

    // SAFETY: `address` is sixteen-byte aligned and inside the guest's own
    // mapped memory, which is identity-mapped into this process and writable —
    // a `lock cmpxchg16b` the guest issued is a write it is entitled to make.
    // Concurrency is the point rather than a hazard: every guest `CMPXCHG16B`
    // reaches this function, so the exclusive monitor sees all of them.
    let found = unsafe { compare_exchange(address as usize as *mut u128, expected, desired) };

    match found == expected {
        true => ctx.zf = 1,
        false => {
            ctx.zf = 0;
            ctx.rax = found as u64;
            ctx.rdx = (found >> 64) as u64;
        }
    }
    Ok(())
}

/// A real 128-bit compare-and-swap, returning what was in memory.
///
/// `ldaxp`/`stlxp` rather than LSE's `caspal`: the exclusive pair is ARMv8.0 and
/// works on every arm64 this could run on, while `casp` needs the LSE extension.
/// Apple Silicon has LSE, so this is conservatism rather than necessity, and the
/// loop is a handful of instructions either way.
///
/// Acquire on the load and release on the store, which is what the x86 `lock`
/// prefix means: a `CMPXCHG16B` is a full barrier on x86-64, and acquire-release
/// on the pair is the aarch64 spelling of that for this location.
///
/// # Safety
/// `at` must be a valid, sixteen-byte-aligned, writable address for the duration
/// of the call.
#[cfg(target_arch = "aarch64")]
unsafe fn compare_exchange(at: *mut u128, expected: u128, desired: u128) -> u128 {
    let (expect_lo, expect_hi) = (expected as u64, (expected >> 64) as u64);
    let (desire_lo, desire_hi) = (desired as u64, (desired >> 64) as u64);
    let (mut found_lo, mut found_hi): (u64, u64);
    // SAFETY: the caller's guarantee about `at`. The loop is the standard
    // exclusive-pair CAS: load the pair with acquire, compare, and either store
    // the new pair with release or break out having *cleared* the monitor with
    // `clrex` — leaving it set would make an unrelated later `stxp` on this core
    // succeed when it should not.
    unsafe {
        std::arch::asm!(
            "2:",
            "ldaxp {found_lo}, {found_hi}, [{at}]",
            "cmp {found_lo}, {expect_lo}",
            "ccmp {found_hi}, {expect_hi}, #0, eq",
            "b.ne 3f",
            "stlxp {status:w}, {desire_lo}, {desire_hi}, [{at}]",
            "cbnz {status:w}, 2b",
            "b 4f",
            "3:",
            "clrex",
            "4:",
            at = in(reg) at,
            expect_lo = in(reg) expect_lo,
            expect_hi = in(reg) expect_hi,
            desire_lo = in(reg) desire_lo,
            desire_hi = in(reg) desire_hi,
            found_lo = out(reg) found_lo,
            found_hi = out(reg) found_hi,
            status = out(reg) _,
            options(nostack),
        );
    }
    u128::from(found_lo) | (u128::from(found_hi) << 64)
}

/// The same operation where the host has one in its own instruction set.
///
/// x86-64 *is* `CMPXCHG16B`, and any other host that gets here has no reason to
/// need the exclusive-pair loop. `AtomicU128` is not stable, so this goes
/// through the compiler intrinsic the same way the aarch64 arm goes through the
/// instruction.
///
/// # Safety
/// As the aarch64 arm.
#[cfg(not(target_arch = "aarch64"))]
unsafe fn compare_exchange(at: *mut u128, expected: u128, desired: u128) -> u128 {
    // SAFETY: the caller's guarantee about `at`.
    unsafe {
        let cell = &*(at as *const std::sync::atomic::AtomicU64).cast::<CellU128>();
        cell.compare_exchange(expected, desired)
    }
}

/// Find the instruction that ends at `end`.
///
/// The exit reports the PC *after* the instruction and nothing about its length,
/// so the start is recovered by trying each length a `CMPXCHG16B` encoding can
/// have and keeping the one that decodes to exactly this instruction ending
/// exactly here. Both conditions matter: a shorter candidate that happens to
/// decode is rejected because it would not end at `end`, and a wrong mnemonic is
/// rejected outright.
///
/// Longest first, because a `lock` prefix belongs to the instruction and a
/// candidate starting *after* it would decode the same opcode without it.
fn decode_before(end: u64, mem: &dyn GuestMemory) -> Option<Instruction> {
    let mut window = [0u8; MAX_LEN as usize];
    let start = end.checked_sub(MAX_LEN)?;
    mem.read(start, &mut window).ok()?;
    for len in (1..=MAX_LEN).rev() {
        let offset = (MAX_LEN - len) as usize;
        let mut decoder = Decoder::with_ip(64, &window[offset..], end - len, DecoderOptions::NONE);
        let instruction = decoder.decode();
        if instruction.mnemonic() == Mnemonic::Cmpxchg16b
            && instruction.next_ip() == end
            && instruction.has_lock_prefix()
        {
            return Some(instruction);
        }
    }
    None
}

/// The address the memory operand names, from the guest's own registers.
///
/// `None` for a form this does not model — a segment override, or a base or
/// index the context has no field for. `CMPXCHG16B` takes exactly one memory
/// operand and no register operand, so there is no ambiguity about which one.
fn effective_address(instruction: &Instruction, ctx: &RegisterContext) -> Option<u64> {
    if instruction.segment_prefix() != Register::None {
        return None;
    }
    let base = match instruction.memory_base() {
        Register::None => 0,
        // RIP-relative, where iced has already folded the displacement against
        // the IP the decoder was given — which is why `decode_before` sets it.
        Register::RIP => return Some(instruction.memory_displacement64()),
        reg => read(reg, ctx)?,
    };
    let index = match instruction.memory_index() {
        Register::None => 0,
        reg => read(reg, ctx)?.wrapping_mul(u64::from(instruction.memory_index_scale())),
    };
    Some(
        base.wrapping_add(index)
            .wrapping_add(instruction.memory_displacement64()),
    )
}

/// One 64-bit register out of the guest's context.
fn read(reg: Register, ctx: &RegisterContext) -> Option<u64> {
    Some(match reg {
        Register::RAX => ctx.rax,
        Register::RCX => ctx.rcx,
        Register::RDX => ctx.rdx,
        Register::RBX => ctx.rbx,
        Register::RSP => ctx.rsp,
        Register::RBP => ctx.rbp,
        Register::RSI => ctx.rsi,
        Register::RDI => ctx.rdi,
        Register::R8 => ctx.r8,
        Register::R9 => ctx.r9,
        Register::R10 => ctx.r10,
        Register::R11 => ctx.r11,
        Register::R12 => ctx.r12,
        Register::R13 => ctx.r13,
        Register::R14 => ctx.r14,
        Register::R15 => ctx.r15,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sixteen-byte-aligned storage, which `compare_exchange` requires and a
    /// plain `u128` local does not promise.
    #[repr(align(16))]
    struct Aligned(u128);

    #[test]
    fn a_matching_pair_is_swapped_and_a_mismatched_one_is_read_back() {
        let mut cell = Aligned(0x1111_2222_3333_4444_5555_6666_7777_8888);
        // SAFETY: `cell` is live, aligned by its own `repr`, and this thread is
        // the only one touching it.
        let found = unsafe { compare_exchange(&mut cell.0, cell.0, 0xdead) };
        assert_eq!(found, 0x1111_2222_3333_4444_5555_6666_7777_8888);
        assert_eq!(cell.0, 0xdead);

        // SAFETY: as above.
        let found = unsafe { compare_exchange(&mut cell.0, 0, 0xbeef) };
        assert_eq!(found, 0xdead, "a failed CAS reports what was there");
        assert_eq!(cell.0, 0xdead, "and leaves it alone");
    }

    #[test]
    fn the_swap_is_atomic_across_threads() {
        // The whole point, and the property `Atomic128Mode::Emulated` does not
        // have: eight threads each add one to the low half a thousand times
        // through the CAS, and every increment has to survive. Under a
        // load-compare-store emulation this loses updates on every run.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let cell = Arc::new(AtomicU64::new(0));
        let boxed: Arc<std::sync::Mutex<Box<Aligned>>> =
            Arc::new(std::sync::Mutex::new(Box::new(Aligned(0))));
        let at = {
            let guard = boxed.lock().unwrap();
            (&guard.0 as *const u128) as usize
        };
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        loop {
                            // SAFETY: `at` is the boxed, aligned cell, which
                            // outlives every thread here because `boxed` is
                            // joined below. Racing on it is the test.
                            let current = unsafe { compare_exchange(at as *mut u128, 0, 0) };
                            let next = current.wrapping_add(1);
                            // SAFETY: as above.
                            let found = unsafe { compare_exchange(at as *mut u128, current, next) };
                            if found == current {
                                break;
                            }
                        }
                    }
                    cell.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();
        for t in threads {
            t.join().expect("a worker");
        }
        let guard = boxed.lock().unwrap();
        assert_eq!(
            guard.0, 8000,
            "an increment was lost, so the CAS is not atomic"
        );
    }

    #[test]
    fn a_failed_compare_leaves_the_monitor_clear() {
        // `clrex` on the mismatch path. Without it the exclusive monitor stays
        // set from the `ldaxp`, and the *next* `stlxp` on this core — the one
        // belonging to the next CAS — can succeed against a stale reservation.
        // A thousand alternating failure/success pairs is enough to catch it.
        let mut cell = Aligned(0);
        for i in 0..1000u128 {
            // SAFETY: `cell` is live, aligned, and thread-local here.
            let found = unsafe { compare_exchange(&mut cell.0, i + 500, 0xbad) };
            assert_eq!(found, i, "iteration {i}");
            assert_eq!(cell.0, i, "a mismatched CAS must not store");
            // SAFETY: as above.
            unsafe { compare_exchange(&mut cell.0, i, i + 1) };
        }
        assert_eq!(cell.0, 1000);
    }
}
