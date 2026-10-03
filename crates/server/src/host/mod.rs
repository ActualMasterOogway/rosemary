//! What the host kernel says about a process, in protocol terms.
//!
//! The server never invents a measurement: a host that cannot say something
//! leaves it zero, and one that cannot see a process at all answers `None`.

use crate::protocol::State;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod other;
#[cfg(not(target_os = "linux"))]
pub use other::*;

/// What makes a pid *this* process rather than a later one that reused the
/// number, or the same process after it `exec`ed something that is not a guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Identity {
    /// When it started, in the host's own units; unchanged by `exec`.
    pub start: u64,
    /// The device and inode of its executable, which `exec` does change.
    pub exe: (u64, u64),
}

/// The host's view of one process: what `kinfo_proc` is built from.
#[derive(Clone, Debug, Default)]
pub struct Process {
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    pub tty_major: i32,
    pub tty_minor: i32,
    pub tpgid: i32,
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub state: State,
    pub nice: i32,
    pub start_sec: i64,
    pub start_usec: i32,
}

/// Map a one-letter process state, which Linux and the BSDs spell alike for
/// the states they share.
pub fn state_from_letter(letter: u8) -> State {
    match letter {
        b'R' => State::Running,
        b'D' => State::Waiting,
        b'T' | b't' => State::Stopped,
        b'Z' | b'X' => State::Zombie,
        _ => State::Sleeping,
    }
}
