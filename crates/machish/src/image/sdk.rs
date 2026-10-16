//! The SDK an image was built against, which the frameworks consult for
//! behaviour that changed between releases.

use crate::raw::View;
use crate::spec::constants::{self as c, LOAD_COMMAND_SIZE, MACH_HEADER_64_SIZE};

/// The SDK version a 64-bit image's load commands record, packed as
/// `xxxx.yy.zz` in nibbles the way `dyld_get_program_sdk_version` answers it:
/// 10.13 is `0x000A_0D00`.
///
/// `LC_BUILD_VERSION` is what current linkers write; `LC_VERSION_MIN_MACOSX`
/// is what older ones did, with the SDK in the same place relative to the
/// command's own fields. `None` for an image with neither, or one whose
/// commands do not fit in `header`.
pub fn sdk_version(header: &[u8]) -> Option<u32> {
    let view = View::new(header);
    if view.u32(0).ok()? != c::MH_MAGIC_64 {
        return None;
    }
    let ncmds = view.u32(16).ok()?;
    let end = MACH_HEADER_64_SIZE.checked_add(view.u32(20).ok()? as usize)?;
    let mut off = MACH_HEADER_64_SIZE;
    for _ in 0..ncmds {
        if off + LOAD_COMMAND_SIZE > end {
            return None;
        }
        let cmd = view.u32(off).ok()?;
        let size = view.u32(off + 4).ok()? as usize;
        if size < LOAD_COMMAND_SIZE {
            return None;
        }
        match cmd {
            // `platform`, `minos`, then `sdk`.
            c::LC_BUILD_VERSION => return view.u32(off + 16).ok(),
            // `version`, then `sdk`.
            c::LC_VERSION_MIN_MACOSX => return view.u32(off + 12).ok(),
            _ => off += size,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn image(commands: &[(u32, &[u32])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (cmd, fields) in commands {
            let size = 8 + 4 * fields.len() as u32;
            body.extend_from_slice(&cmd.to_le_bytes());
            body.extend_from_slice(&size.to_le_bytes());
            for field in *fields {
                body.extend_from_slice(&field.to_le_bytes());
            }
        }
        let mut bytes = Vec::new();
        for word in [
            c::MH_MAGIC_64,
            0,
            0,
            c::MH_EXECUTE,
            commands.len() as u32,
            body.len() as u32,
            0,
            0,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.extend_from_slice(&body);
        bytes
    }

    #[test]
    fn reads_the_sdk_from_a_version_min_command() {
        let bytes = image(&[
            (c::LC_UUID, &[0; 4]),
            (c::LC_VERSION_MIN_MACOSX, &[0x000A_0900, 0x000A_0D00]),
        ]);
        assert_eq!(sdk_version(&bytes), Some(0x000A_0D00));
    }

    #[test]
    fn reads_the_sdk_from_a_build_version_command() {
        let bytes = image(&[(c::LC_BUILD_VERSION, &[1, 0x000B_0000, 0x000E_0000, 0])]);
        assert_eq!(sdk_version(&bytes), Some(0x000E_0000));
    }

    #[test]
    fn an_image_without_a_version_command_has_no_sdk() {
        assert_eq!(sdk_version(&image(&[(c::LC_UUID, &[0; 4])])), None);
        assert_eq!(sdk_version(&[0; 8]), None);
    }
}
