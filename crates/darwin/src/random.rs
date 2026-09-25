#![cfg(not(target_os = "macos"))]

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

exports::exports! {
    functions {
        "arc4random" => arc4random,
        "arc4random_buf" => arc4random_buf,
    }
}
