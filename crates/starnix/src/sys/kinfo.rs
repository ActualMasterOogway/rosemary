//! FreeBSD's packed `kinfo_file` table, parsed by offset.
//!
//! Built on every host so its test runs where the tree is usually tested; only
//! the FreeBSD backend calls it.

use std::io;

use crate::process::{Descriptor, DescriptorKind};

/// `KF_TYPE_*` from `<sys/user.h>`.
const KF_TYPE_SOCKET: i32 = 2;
const KF_TYPE_PIPE: i32 = 3;
const KF_TYPE_FIFO: i32 = 4;
const KF_TYPE_KQUEUE: i32 = 5;

/// `KERN_PROC_FILEDESC`'s answer: records of `kf_structsize`, each opening with
/// the size, the type and the descriptor.
pub fn descriptors(bytes: &[u8]) -> io::Result<Vec<Descriptor>> {
    let invalid = || io::Error::from_raw_os_error(libc::EINVAL);
    let word = |at: usize| i32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
    let mut fds = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < 12 {
            return Err(invalid());
        }
        let (size, kind, fd) = (word(offset), word(offset + 4), word(offset + 8));
        if size < 12 || size as usize > bytes.len() - offset {
            return Err(invalid());
        }
        // The table also holds pseudo descriptors for cwd, root and the like.
        if fd >= 0 {
            let kind = match kind {
                KF_TYPE_SOCKET => DescriptorKind::Socket,
                KF_TYPE_PIPE | KF_TYPE_FIFO => DescriptorKind::Pipe,
                KF_TYPE_KQUEUE => DescriptorKind::Queue,
                _ => DescriptorKind::File,
            };
            fds.push(Descriptor { fd, kind });
        }
        offset += size as usize;
    }
    Ok(fds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_records_skip_pseudo_fds_and_use_record_lengths() {
        let mut bytes = Vec::new();
        for (length, kind, fd) in [(16i32, 1i32, -1i32), (24, 2, 7), (16, 3, 4)] {
            bytes.extend_from_slice(&length.to_ne_bytes());
            bytes.extend_from_slice(&kind.to_ne_bytes());
            bytes.extend_from_slice(&fd.to_ne_bytes());
            bytes.resize(bytes.len() + length as usize - 12, 0);
        }
        let fds = descriptors(&bytes).unwrap();
        assert_eq!(
            fds,
            [
                Descriptor {
                    fd: 7,
                    kind: DescriptorKind::Socket
                },
                Descriptor {
                    fd: 4,
                    kind: DescriptorKind::Pipe
                },
            ]
        );
        bytes[16..20].copy_from_slice(&100i32.to_ne_bytes());
        assert!(descriptors(&bytes).is_err());
    }
}
