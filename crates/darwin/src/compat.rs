#![cfg(not(target_os = "macos"))]

use std::ffi::c_void;

unsafe extern "C" fn bzero(dst: *mut c_void, len: usize) {
    unsafe { std::ptr::write_bytes(dst, 0, len) };
}

exports::exports! {
    functions {
        "bzero" | "__bzero" => bzero,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_of_bzero_are_published() {
        assert_eq!(symbol("bzero"), symbol("__bzero"));
        assert!(symbol("bzero").is_some());
    }

    #[test]
    fn bzero_clears_exactly_its_length() {
        let mut buffer = [0xffu8; 8];
        // SAFETY: four writable bytes at the front of a live array.
        unsafe { bzero(buffer.as_mut_ptr().cast(), 4) };
        assert_eq!(buffer, [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    }
}
