//! Entry-point extraction.
//!
//! Mach-O has two incompatible encodings, and the difference is not cosmetic:
//! `LC_MAIN` gives a **file offset** that must be added to the image's base,
//! while `LC_UNIXTHREAD` gives a complete initial register set whose `rip` is
//! already a virtual address. Conflating them puts the entry point off by the
//! header size, which fails in a confusing way much later.

use crate::error::{Error, Result};
use crate::raw::{View, Word};

/// `x86_THREAD_STATE64`.
const X86_THREAD_STATE64: u32 = 4;
/// `ARM_THREAD_STATE64`.
const ARM_THREAD_STATE64: u32 = 6;

/// Index of `rsp` in `x86_thread_state64_t`, counting in 64-bit words:
/// rax rbx rcx rdx rdi rsi rbp **rsp** r8..r15 rip rflags cs fs gs.
const X86_64_RSP_INDEX: usize = 7;
/// Index of `rip` in the same structure.
const X86_64_RIP_INDEX: usize = 16;
/// Index of `sp` in `arm_thread_state64_t`: x0..x28, fp, lr, **sp**, pc, cpsr.
const ARM64_SP_INDEX: usize = 31;
/// Index of `pc` in the same structure.
const ARM64_PC_INDEX: usize = 32;

/// How an image says where to start.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EntryPoint {
    /// `LC_MAIN`. `entryoff` is an offset from the start of the *file*, which
    /// for a loaded image means an offset from its base address — the Mach-O
    /// header is mapped, so file offset and image offset coincide.
    ///
    /// An `LC_MAIN` image expects libSystem's initializer to have run first:
    /// the real entry sequence is dyld → `libSystem` → `main`.
    Main {
        entryoff: u64,
        /// Requested main-thread stack size, or 0 for the default.
        stacksize: u64,
    },
    /// `LC_UNIXTHREAD`. A full initial register state; `pc` is a virtual
    /// address that already includes the image's preferred base. Used by
    /// pre-10.8 executables and by `dyld` itself.
    UnixThread { pc: Word, sp: Word },
}

impl EntryPoint {
    /// Resolve to an absolute address given the image's base and slide.
    ///
    /// `base` is where the image was mapped; `slide` is `base - preferred_base`.
    pub fn address(&self, base: Word, slide: Word) -> Word {
        match *self {
            EntryPoint::Main { entryoff, .. } => base.wrapping_add(entryoff),
            EntryPoint::UnixThread { pc, .. } => pc.wrapping_add(slide),
        }
    }

    /// Parse an `LC_MAIN` payload at `off` (the command's own offset).
    pub fn parse_main(view: &View<'_>, off: usize) -> Result<Self> {
        Ok(EntryPoint::Main {
            entryoff: view.u64(off + 8)?,
            stacksize: view.u64(off + 16)?,
        })
    }

    /// Parse an `LC_UNIXTHREAD` payload at `off`.
    ///
    /// The command carries `(flavor, count, state...)` triples. Only the first
    /// recognized thread-state flavor is read — no real executable carries more
    /// than one, and the flavor is self-describing, so this does not need the
    /// architecture threaded in from the spec.
    pub fn parse_unix_thread(view: &View<'_>, off: usize, cmdsize: usize) -> Result<Self> {
        let end = off + cmdsize;
        let mut p = off + 8;
        while p + 8 <= end {
            let flavor = view.u32(p)?;
            let count = view.u32(p + 4)? as usize;
            let state = p + 8;
            // `count` is in 32-bit words.
            let state_bytes = count.checked_mul(4).ok_or(Error::BadLoadCommand {
                why: "thread state count overflow",
            })?;
            if state + state_bytes > end {
                return Err(Error::BadLoadCommand { why: "thread state past command" });
            }

            let word = |i: usize| view.u64(state + i * 8);
            match flavor {
                X86_THREAD_STATE64 if count >= (X86_64_RIP_INDEX + 1) * 2 => {
                    return Ok(EntryPoint::UnixThread {
                        pc: word(X86_64_RIP_INDEX)?,
                        sp: word(X86_64_RSP_INDEX)?,
                    });
                }
                ARM_THREAD_STATE64 if count >= (ARM64_PC_INDEX + 1) * 2 => {
                    return Ok(EntryPoint::UnixThread {
                        pc: word(ARM64_PC_INDEX)?,
                        sp: word(ARM64_SP_INDEX)?,
                    });
                }
                _ => {}
            }
            p = state + state_bytes;
        }
        Err(Error::BadLoadCommand { why: "no usable thread state" })
    }
}
