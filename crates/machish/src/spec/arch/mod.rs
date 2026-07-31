//! Built-in architecture specs.

#[cfg(feature = "arch-arm64")]
mod arm64;
#[cfg(feature = "arch-x86_64")]
mod x86_64;

#[cfg(feature = "arch-arm64")]
pub use arm64::Arm64;
#[cfg(feature = "arch-x86_64")]
pub use x86_64::X86_64;
