use std::ffi::c_char;

unsafe extern "C" fn index_to_name(index: u32, out: *mut c_char) -> *mut c_char {
    unsafe { libc::if_indextoname(index, out) }
}
unsafe extern "C" fn name_to_index(name: *const c_char) -> u32 {
    unsafe { libc::if_nametoindex(name) }
}

pub static ANY: [u8; 16] = [0; 16];
pub static LOOPBACK: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

exports::exports! {
    functions {
        "if_indextoname" => index_to_name,
        "if_nametoindex" => name_to_index,
    }
    constants {
        "in6addr_any" => ANY.as_ptr() as usize as u64,
        "in6addr_loopback" => LOOPBACK.as_ptr() as usize as u64,
    }
}
