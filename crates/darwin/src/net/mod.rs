//! The socket API, where almost nothing matches and almost nothing fails.
//!
//! This is the largest single divergence between the two platforms, and the one
//! whose symptoms point furthest from the cause. Every layer disagrees:
//!
//! - [`addr`] - `struct sockaddr`'s first two bytes, and the `AF_*` numbers.
//! - [`socket`] - the `MSG_*` flags, and `struct msghdr`'s width.
//! - [`sockopt`] - `SOL_SOCKET` itself, and every option name under it.
//! - [`resolve`] - `struct addrinfo`'s field order, the `AI_*` and `NI_*` bits,
//!   and the sign of every `EAI_*`.
//! - [`ifaddrs`] - the addresses inside a matching structure.
//!
//! What makes this class of bug expensive is that none of it is a crash. A
//! socket opens, a `connect` returns `EAFNOSUPPORT` or `ENETUNREACH`, an option
//! is set on something else, a lookup reports the wrong reason for failing. The
//! guest reports a network problem and the network is fine.
//!
//! `listen`, `shutdown`, `inet_addr`, `inet_aton`, `inet_ntoa`, `htons` and
//! their family are deliberately absent: their arguments are integers or byte
//! strings that carry no platform constant, so the host's own are the answer.

#![cfg(not(target_os = "macos"))]

use machish::resolve::SyntheticDylib;

pub mod addr;
pub mod control;
pub mod descriptor;
pub mod ifaddrs;
pub mod interface;
pub mod resolve;
pub mod socket;
pub mod sockopt;

/// Every name this module answers, across its parts.
pub fn names() -> impl Iterator<Item = &'static str> {
    descriptor::NAMES
        .iter()
        .chain(socket::NAMES)
        .chain(sockopt::NAMES)
        .chain(resolve::NAMES)
        .chain(ifaddrs::NAMES)
        .chain(interface::NAMES)
        .copied()
}

pub fn symbol(name: &str) -> Option<u64> {
    descriptor::symbol(name)
        .or_else(|| socket::symbol(name))
        .or_else(|| sockopt::symbol(name))
        .or_else(|| resolve::symbol(name))
        .or_else(|| ifaddrs::symbol(name))
        .or_else(|| interface::symbol(name))
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in names() {
        crate::define_fn(
            lib,
            &format!("_{name}"),
            symbol(name).unwrap_or(0) as *const (),
        );
    }
    crate::define_data(lib, "_in6addr_any", interface::ANY.as_ptr() as u64);
    crate::define_data(
        lib,
        "_in6addr_loopback",
        interface::LOOPBACK.as_ptr() as u64,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name this module claims is one it can actually answer, through the
    /// same lookup the loader uses.
    #[test]
    fn every_published_name_resolves() {
        for name in names() {
            assert!(symbol(name).is_some(), "{name} is published and absent");
        }
    }
}
