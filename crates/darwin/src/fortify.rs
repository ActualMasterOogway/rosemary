#![cfg(not(target_os = "macos"))]

use std::ffi::{c_char, c_int, c_void};

#[inline(always)]
fn checked(len: usize, capacity: usize) {
    if len > capacity {
        unsafe { libc::abort() }
    }
}

pub unsafe extern "C" fn memcpy_chk(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    capacity: usize,
) -> *mut c_void {
    checked(len, capacity);
    unsafe { std::ptr::copy_nonoverlapping(src.cast::<u8>(), dst.cast::<u8>(), len) };
    dst
}

pub unsafe extern "C" fn memmove_chk(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    capacity: usize,
) -> *mut c_void {
    checked(len, capacity);
    unsafe { std::ptr::copy(src.cast::<u8>(), dst.cast::<u8>(), len) };
    dst
}

pub unsafe extern "C" fn memset_chk(
    dst: *mut c_void,
    byte: c_int,
    len: usize,
    capacity: usize,
) -> *mut c_void {
    checked(len, capacity);
    unsafe { std::ptr::write_bytes(dst, byte as u8, len) };
    dst
}

unsafe fn c_len(s: *const c_char) -> usize {
    unsafe { libc::strlen(s) }
}

pub unsafe extern "C" fn strcpy_chk(
    dst: *mut c_char,
    src: *const c_char,
    capacity: usize,
) -> *mut c_char {
    let len = unsafe { c_len(src) } + 1;
    checked(len, capacity);
    unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
    dst
}

pub unsafe extern "C" fn strcat_chk(
    dst: *mut c_char,
    src: *const c_char,
    capacity: usize,
) -> *mut c_char {
    let used = unsafe { c_len(dst) };
    let added = unsafe { c_len(src) };
    checked(used.saturating_add(added).saturating_add(1), capacity);
    unsafe { std::ptr::copy_nonoverlapping(src, dst.add(used), added + 1) };
    dst
}

pub unsafe extern "C" fn strlcpy_chk(
    dst: *mut c_char,
    src: *const c_char,
    size: usize,
    capacity: usize,
) -> usize {
    checked(size, capacity);
    let len = unsafe { c_len(src) };
    if size != 0 {
        let copied = len.min(size - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst, copied);
            *dst.add(copied) = 0;
        }
    }
    len
}

pub unsafe extern "C" fn strlcat_chk(
    dst: *mut c_char,
    src: *const c_char,
    size: usize,
    capacity: usize,
) -> usize {
    checked(size, capacity);
    let src_len = unsafe { c_len(src) };
    let dst_len = unsafe { libc::strnlen(dst, size) };
    if dst_len == size {
        return size.saturating_add(src_len);
    }
    let available = size - dst_len;
    if available != 0 {
        let copied = src_len.min(available - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.add(dst_len), copied);
            *dst.add(dst_len + copied) = 0;
        }
    }
    dst_len + src_len
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct VaList {
    gp_offset: u32,
    fp_offset: u32,
    overflow_arg_area: *mut c_void,
    reg_save_area: *mut c_void,
}

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn vsnprintf(dst: *mut c_char, size: usize, format: *const c_char, ap: *mut VaList) -> c_int;
}

#[cfg(target_arch = "x86_64")]
unsafe extern "C" fn sprintf_chk_body(
    dst: *mut c_char,
    _flag: c_int,
    capacity: usize,
    format: *const c_char,
    ap: *mut VaList,
) -> c_int {
    let written = unsafe { vsnprintf(dst, capacity, format, ap) };
    if written >= 0 && written as usize >= capacity {
        unsafe { libc::abort() }
    }
    written
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".text",
    ".globl rosetta3_sprintf_chk",
    ".hidden rosetta3_sprintf_chk",
    ".type rosetta3_sprintf_chk, @function",
    "rosetta3_sprintf_chk:",
    "push rbp",
    "mov rbp, rsp",
    "sub rsp, 208",
    "mov [rsp], rdi",
    "mov [rsp + 8], rsi",
    "mov [rsp + 16], rdx",
    "mov [rsp + 24], rcx",
    "mov [rsp + 32], r8",
    "mov [rsp + 40], r9",
    "movaps [rsp + 48], xmm0",
    "movaps [rsp + 64], xmm1",
    "movaps [rsp + 80], xmm2",
    "movaps [rsp + 96], xmm3",
    "movaps [rsp + 112], xmm4",
    "movaps [rsp + 128], xmm5",
    "movaps [rsp + 144], xmm6",
    "movaps [rsp + 160], xmm7",
    "mov dword ptr [rsp + 176], 32",
    "mov dword ptr [rsp + 180], 48",
    "lea rax, [rbp + 16]",
    "mov [rsp + 184], rax",
    "mov rax, rsp",
    "mov [rsp + 192], rax",
    "lea r8, [rsp + 176]",
    "call {body}",
    "mov rsp, rbp",
    "pop rbp",
    "ret",
    ".size rosetta3_sprintf_chk, . - rosetta3_sprintf_chk",
    body = sym sprintf_chk_body,
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn rosetta3_sprintf_chk();
}

#[cfg(not(target_arch = "x86_64"))]
unsafe extern "C" fn rosetta3_sprintf_chk() {
    // Native Darwin supplies its own variadic entry. Other architectures only
    // reach this address if the emulator failed to intercept the import.
    unsafe { libc::abort() }
}

exports::exports! {
    functions {
        "__memcpy_chk" => memcpy_chk,
        "__memmove_chk" => memmove_chk,
        "__memset_chk" => memset_chk,
        "__sprintf_chk" => rosetta3_sprintf_chk,
        "__strcat_chk" => strcat_chk,
        "__strcpy_chk" => strcpy_chk,
        "__strlcat_chk" => strlcat_chk,
        "__strlcpy_chk" => strlcpy_chk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_string_functions_have_darwin_results() {
        let mut dst = [0i8; 8];
        unsafe {
            assert_eq!(strlcpy_chk(dst.as_mut_ptr(), c"abcdef".as_ptr(), 4, 8), 6);
            assert_eq!(std::ffi::CStr::from_ptr(dst.as_ptr()).to_bytes(), b"abc");
            assert_eq!(strlcat_chk(dst.as_mut_ptr(), c"XYZ".as_ptr(), 8, 8), 6);
            assert_eq!(std::ffi::CStr::from_ptr(dst.as_ptr()).to_bytes(), b"abcXYZ");
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn sprintf_chk_forwards_variadic_arguments() {
        type Sprintf = unsafe extern "C" fn(*mut c_char, c_int, usize, *const c_char, ...) -> c_int;
        let function: Sprintf =
            unsafe { std::mem::transmute(symbol("__sprintf_chk").expect("published") as usize) };
        let mut dst = [0i8; 32];
        let written = unsafe {
            function(
                dst.as_mut_ptr(),
                0,
                dst.len(),
                c"%s:%d".as_ptr(),
                c"ok".as_ptr(),
                42,
            )
        };
        assert_eq!(written, 5);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(dst.as_ptr()) }.to_bytes(),
            b"ok:42"
        );
    }
}
