//! Prefixes: the Darwin userland a guest runs against, and the namespace that
//! puts it where the guest looks for it.
//!
//! A prefix layers mutable guest state over a shared, verified MiniDarwin base.
//! [`ns`] maps guest paths to host paths. Repair restores the base offline.

mod errno;

pub mod bundle;
pub mod cache;
pub mod digest;
pub mod fetch;
pub mod format;
pub mod layout;
pub mod ns;
pub mod pinned;
pub mod provision;
pub mod repair;
pub mod state;
pub mod system;
pub mod unpack;
pub mod verify;

pub use layout::Prefix;
