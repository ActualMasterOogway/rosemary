//! Interface names and IPv6 address constants owned by Darwin's networking ABI.

use std::ffi::c_char;

unsafe extern "C" fn index_to_name(index: u32, out: *mut c_char) -> *mut c_char {
    unsafe { libc::if_indextoname(index, out) }
}
unsafe extern "C" fn name_to_index(name: *const c_char) -> u32 {
    unsafe { libc::if_nametoindex(name) }
}

pub static ANY: [u8; 16] = [0; 16];
pub static LOOPBACK: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

pub const NAMES: &[&str] = &["if_indextoname", "if_nametoindex"];
pub fn symbol(name: &str) -> Option<u64> {
    let f = match name {
        "if_indextoname" => index_to_name as *const (),
        "if_nametoindex" => name_to_index as *const (),
        "in6addr_any" => ANY.as_ptr().cast(),
        "in6addr_loopback" => LOOPBACK.as_ptr().cast(),
        _ => return None,
    };
    Some(f as usize as u64)
}
