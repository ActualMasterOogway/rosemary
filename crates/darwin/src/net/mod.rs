//! Darwin's sockets, resolver and interface list over whichever host runs them.
//!
//! Every host translates, macOS included, where most of it is the identity: the
//! code path is the same everywhere, so a fault can be told apart as one host's
//! or the translation's. What the hosts spell differently is in [`host`].

pub mod addr;
pub mod control;
pub mod descriptor;
pub mod host;
pub mod ifaddrs;
pub mod interface;
pub mod resolve;
pub mod socket;
pub mod sockopt;

exports::group! {
    descriptor,
    socket,
    sockopt,
    resolve,
    ifaddrs,
    interface,
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
