//! machish - a templatable Mach-O loader and dynamic linker, built for
//! high-level emulation of macOS binaries.
//!
//! It is the Mach-O counterpart to [`selfish`](https://github.com/V3L0C1T13S/selfish),
//! and shares its central bet: that a loader whose *every* interaction with
//! memory, symbol resolution, and control transfer goes through a trait can
//! serve a compatibility layer as readily as it serves native execution. The
//! immediate consumer is Rosetta 3, which runs Intel macOS applications on
//! Apple Silicon — so the guest image is x86-64 data being fed to a recompiler,
//! never code this CPU executes, and an "imported function" resolves to a
//! *native arm64 host pointer*.
//!
//! Three seams carry that design:
//!
//! - [`memory::AddressSpace`] — all mapping and fixup application. Host memory,
//!   a plain buffer, or anything else.
//! - [`resolve::SymbolProvider`] — what an import *means*. Answering with a host
//!   function pointer is what makes this high-level emulation rather than
//!   translation.
//! - [`exec::Executor`] — how a resolved address is called. Natively, or by
//!   handing it to an emulator.
//!
//! Unlike a real dyld, linking here produces a **transcript** as well as a
//! result: [`report::LinkReport`] records every binding's slot address, stub
//! address, source library, and resolved value, which is what an embedder needs
//! to install native call-site intercepts.
//!
//! The core is `no_std` (`alloc` only). The `std` feature adds the host
//! frontend: an mmap-backed address space, a native executor, filesystem
//! dylib lookup, and `dlopen`/`dlsym`-based symbol resolution.

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod error;
pub mod fat;
pub mod image;
pub mod memory;
pub mod raw;
pub mod spec;

pub use error::{Error, Result};

pub mod prelude {
    pub use crate::error::{Error, Result};
    pub use crate::memory::{AddressSpace, Placement, Prot, ProtectionPolicy};
    pub use crate::spec::{MachSpec, PageSized};

    #[cfg(feature = "arch-arm64")]
    pub use crate::spec::arch::Arm64;
    #[cfg(feature = "arch-x86_64")]
    pub use crate::spec::arch::X86_64;

    #[cfg(feature = "std")]
    pub use crate::memory::MmapSpace;
}
