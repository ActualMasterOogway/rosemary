//! Darwin's `arc4random` family, backed by the host kernel's random source.

#![cfg(not(target_os = "macos"))]

use machish::resolve::SyntheticDylib;

unsafe extern "C" fn arc4random() -> u32 {
    let mut bytes = [0; 4];
    if getrandom::fill(&mut bytes).is_err() {
        crate::errno::set(libc::EIO);
        return 0;
    }
    u32::from_ne_bytes(bytes)
}
unsafe extern "C" fn arc4random_buf(out: *mut u8, len: usize) {
    if len == 0 || out.is_null() {
        return;
    }
    if getrandom::fill(unsafe { std::slice::from_raw_parts_mut(out, len) }).is_err() {
        crate::errno::set(libc::EIO);
    }
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "arc4random" => arc4random as *const () as usize as u64,
        "arc4random_buf" => arc4random_buf as *const () as usize as u64,
        _ => return None,
    })
}
pub fn provide(lib: &mut SyntheticDylib) {
    for name in ["arc4random", "arc4random_buf"] {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
}
