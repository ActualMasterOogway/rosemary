//! machish - a templatable Mach-O loader and dynamic linker, built for
//! high-level emulation of macOS binaries.
//!
//! It is the Mach-O counterpart to [`selfish`](https://github.com/V3L0C1T13S/selfish),
//! and shares its primary design: that a loader whose every interaction with
//! memory, symbol resolution, and control transfer goes through a trait can
//! serve a compatibility layer as readily as it serves native execution.
//!
//! Unlike a real dyld, linking here produces a transcript as well as a
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
pub mod exports;
pub mod extensions;
pub mod fat;
pub mod fixups;
pub mod image;
pub mod install_name;
pub mod loader;
pub mod memory;
pub mod namespace;
pub mod raw;
pub mod report;
pub mod resolve;
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
