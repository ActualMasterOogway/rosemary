use std::ffi::c_char;

/// `sysdir_search_path_domain_mask_t` bits.
const USER: u32 = 1;
const LOCAL: u32 = 2;
const NETWORK: u32 = 4;
const SYSTEM: u32 = 8;
const DOMAINS: u32 = USER | LOCAL | NETWORK | SYSTEM;

const ALL_APPLICATIONS: u32 = 100;
const ALL_LIBRARIES: u32 = 101;

/// `(domain, path)` rows for one `sysdir_search_path_directory_t`, in the order
/// a Mac enumerates them: user, local, network, system.
///
/// Paths keep their leading `~`, which is the caller's to expand, as it is
/// from Apple's own.
fn rows(directory: u32) -> &'static [(u32, &'static str)] {
    match directory {
        1 => &[
            (USER, "~/Applications"),
            (LOCAL, "/Applications"),
            (NETWORK, "/Network/Applications"),
            (SYSTEM, "/System/Applications"),
        ],
        2 => &[
            (USER, "~/Applications/Demos"),
            (LOCAL, "/Applications/Demos"),
            (NETWORK, "/Network/Applications/Demos"),
        ],
        3 => &[
            (USER, "~/Developer/Applications"),
            (LOCAL, "/Developer/Applications"),
            (NETWORK, "/Network/Developer/Applications"),
        ],
        4 => &[
            (LOCAL, "/Applications/Utilities"),
            (NETWORK, "/Network/Applications/Utilities"),
            (SYSTEM, "/System/Library/CoreServices/Applications"),
        ],
        5 => &[
            (USER, "~/Library"),
            (LOCAL, "/Library"),
            (NETWORK, "/Network/Library"),
            (SYSTEM, "/System/Library"),
        ],
        6 => &[
            (USER, "~/Developer"),
            (LOCAL, "/Developer"),
            (NETWORK, "/Network/Developer"),
        ],
        7 => &[(LOCAL, "/Users"), (NETWORK, "/Network/Users")],
        8 => &[
            (USER, "~/Library/Documentation"),
            (LOCAL, "/Library/Documentation"),
            (NETWORK, "/Network/Library/Documentation"),
            (SYSTEM, "/System/Library/Documentation"),
        ],
        9 => &[(USER, "~/Documents")],
        10 => &[(SYSTEM, "/System/Library/CoreServices")],
        11 => &[(USER, "~/Library/Autosave Information")],
        12 => &[(USER, "~/Desktop")],
        13 => &[
            (USER, "~/Library/Caches"),
            (LOCAL, "/Library/Caches"),
            (SYSTEM, "/System/Library/Caches"),
        ],
        14 => &[
            (USER, "~/Library/Application Support"),
            (LOCAL, "/Library/Application Support"),
            (NETWORK, "/Network/Library/Application Support"),
        ],
        15 => &[(USER, "~/Downloads")],
        16 => &[
            (USER, "~/Library/Input Methods"),
            (LOCAL, "/Library/Input Methods"),
            (NETWORK, "/Network/Library/Input Methods"),
            (SYSTEM, "/System/Library/Input Methods"),
        ],
        17 => &[(USER, "~/Movies")],
        18 => &[(USER, "~/Music")],
        19 => &[(USER, "~/Pictures")],
        20 => &[(SYSTEM, "/System/Library/Printers/PPDs")],
        21 => &[(USER, "~/Public")],
        22 => &[
            (USER, "~/Library/PreferencePanes"),
            (LOCAL, "/Library/PreferencePanes"),
            (SYSTEM, "/System/Library/PreferencePanes"),
        ],
        _ => &[],
    }
}

/// Every path `directory` names within `mask`, in enumeration order.
///
/// The two `ALL_*` values are the concatenation of their members, each in its
/// own order.
///
/// FIXME (ACCURACY): `SYSDIR_DIRECTORY_ALL_LIBRARIES` is only `LIBRARY`; the
/// developer library directories Apple adds are not listed.
fn paths(directory: u32, mask: u32) -> Vec<&'static str> {
    let members: &[u32] = match directory {
        ALL_APPLICATIONS => &[1, 2, 3, 4],
        ALL_LIBRARIES => &[5],
        other => &[other],
    };
    members
        .iter()
        .flat_map(|d| rows(*d))
        .filter(|(domain, _)| mask & domain != 0)
        .map(|(_, path)| *path)
        .collect()
}

// The state is opaque to callers: bit 31 keeps it non-zero (zero ends a loop),
// then the position in the list, the domain mask and the directory.
const LIVE: u32 = 1 << 31;

fn pack(directory: u32, mask: u32, index: u32) -> u32 {
    LIVE | (index & 0xff) << 16 | (mask & DOMAINS) << 8 | (directory & 0xff)
}

/// `sysdir_search_path_enumeration_state sysdir_start_search_path_enumeration(
/// sysdir_search_path_directory_t, sysdir_search_path_domain_mask_t)`
pub extern "C" fn start_search_path_enumeration(directory: u32, mask: u32) -> u32 {
    // The mask's low bits are the four domains; `ALL` is `0x0ffff`.
    pack(directory, mask, 0)
}

/// `sysdir_search_path_enumeration_state sysdir_get_next_search_path_enumeration(
/// sysdir_search_path_enumeration_state, char *path)`
///
/// Zero, with `path` untouched, when there is nothing left.
///
/// # Safety
///
/// `path` is null or writable for `PATH_MAX` bytes.
pub unsafe extern "C" fn get_next_search_path_enumeration(state: u32, path: *mut c_char) -> u32 {
    if state & LIVE == 0 || path.is_null() {
        return 0;
    }
    let (directory, mask, index) = (state & 0xff, (state >> 8) & 0xf, (state >> 16) & 0xff);
    let Some(next) = paths(directory, mask).get(index as usize).copied() else {
        return 0;
    };
    // SAFETY: the caller's buffer is `PATH_MAX`, and every path is far shorter.
    unsafe {
        std::ptr::copy_nonoverlapping(next.as_ptr().cast::<c_char>(), path, next.len());
        *path.add(next.len()) = 0;
    }
    pack(directory, mask, index + 1)
}

exports::exports! {
    functions {
        "sysdir_start_search_path_enumeration" => start_search_path_enumeration,
        "sysdir_get_next_search_path_enumeration" => get_next_search_path_enumeration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(directory: u32, mask: u32) -> Vec<String> {
        let mut out = Vec::new();
        let mut buffer = [0 as c_char; 1024];
        let mut state = start_search_path_enumeration(directory, mask);
        // SAFETY: a buffer of PATH_MAX or more.
        while {
            state = unsafe { get_next_search_path_enumeration(state, buffer.as_mut_ptr()) };
            state != 0
        } {
            let text = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
            out.push(text.to_string_lossy().into_owned());
        }
        out
    }

    #[test]
    fn library_enumerates_user_local_network_system() {
        assert_eq!(
            all(5, 0xffff),
            [
                "~/Library",
                "/Library",
                "/Network/Library",
                "/System/Library"
            ]
        );
    }

    #[test]
    fn the_domain_mask_filters() {
        assert_eq!(all(5, USER | SYSTEM), ["~/Library", "/System/Library"]);
        assert!(all(9, LOCAL).is_empty());
    }

    #[test]
    fn all_applications_concatenates_its_members() {
        let all = all(ALL_APPLICATIONS, USER);
        assert_eq!(all[0], "~/Applications");
        assert!(all.contains(&"~/Developer/Applications".to_string()));
    }

    #[test]
    fn an_unknown_directory_ends_at_once() {
        assert!(all(999, 0xffff).is_empty());
    }
}
