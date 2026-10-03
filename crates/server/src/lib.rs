//! The process server, Rosemary's counterpart to `wineserver` and
//! `darlingserver`.
//!
//! Every guest is a host process of its own, and a process cannot see past
//! itself: `top`, `ps` and anything else that enumerates tasks need somebody
//! who knows every guest at once. That is this server - one per prefix,
//! started on demand by the first guest and gone a few seconds after the last.
//!
//! * [`client`] is a guest's side: check in once, then ask.
//! * [`daemon`] is the server: a process table ([`registry`]) kept honest by
//!   the host kernel (through `starnix`), answered over a private Unix socket
//!   ([`location`], [`protocol`], [`codec`]).
//!
//! See `docs/server.md` for the design and what is still out of scope.

pub mod client;
pub mod codec;
pub mod daemon;
pub mod location;
pub mod protocol;
pub mod registry;
