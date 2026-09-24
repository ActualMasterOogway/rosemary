//! Mach-O loader and dynamic linker for macOS emulation.
//!
//! Memory, symbol resolution, and execution are supplied through traits.
//! [`report::LinkReport`] records bindings and stub addresses for call-site
//! interception. The core uses `no_std`; the `std` feature adds host services.

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
