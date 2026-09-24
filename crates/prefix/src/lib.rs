//! Prefixes: the Darwin userland a guest runs against, and the namespace that
//! puts it where the guest looks for it.
//!
//! A *prefix* is a directory holding a Darwin filesystem - a verified MiniDarwin
//! base plus whatever the guest has written since. It is deliberately two
//! things layered: the base is immutable, content-addressed and shared between
//! prefixes, and the prefix is the mutable state on top of it. Repair is then
//! "put the base back", which needs no network.
//!
//! [`ns`] is the half every other crate touches: the translation between the
//! paths a guest speaks and the paths this host has.

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
