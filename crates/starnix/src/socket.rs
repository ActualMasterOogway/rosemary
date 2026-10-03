//! Unix-domain sockets.

use std::os::fd::AsRawFd;

use crate::sys;

/// Who is on the other end of a connected Unix socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    /// `None` where the host cannot say.
    pub pid: Option<i32>,
    pub uid: u32,
}

pub fn peer(socket: &impl AsRawFd) -> Option<Peer> {
    sys::peer(socket.as_raw_fd())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_pair_is_this_user_at_both_ends() {
        let (one, _other) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = peer(&one).unwrap();
        // SAFETY: `getuid` has no preconditions.
        assert_eq!(peer.uid, unsafe { libc::getuid() });
        assert!(peer.pid.is_none_or(|pid| pid == crate::process::me()));
    }
}
