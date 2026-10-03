//! Hosts without a `/proc` this module reads. A process is known to exist and
//! nothing more, which is enough for the list to be right and every
//! measurement to read zero.

use super::{Identity, Process};
use crate::protocol::{Thread, Usage};

pub fn identity(pid: i32) -> Option<Identity> {
    alive(pid, &Identity::default()).then(Identity::default)
}

pub fn alive(pid: i32, _identity: &Identity) -> bool {
    // SAFETY: signal zero only checks that the pid can be signalled.
    unsafe { libc::kill(pid, 0) == 0 }
    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn process(pid: i32) -> Option<Process> {
    identity(pid).map(|_| Process::default())
}

pub fn usage(pid: i32) -> Option<Usage> {
    identity(pid).map(|_| Usage::default())
}

pub fn threads(pid: i32) -> Option<Vec<Thread>> {
    identity(pid).map(|_| Vec::new())
}
