#![cfg(not(target_os = "macos"))]

pub mod addr;
pub mod control;
pub mod descriptor;
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
