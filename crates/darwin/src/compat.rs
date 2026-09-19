//! Small libSystem entry points which are absent (most of these are for musl), fortified differently, or
//! carry a Darwin structure on other C libraries.

#![cfg(not(target_os = "macos"))]

use std::ffi::{c_char, c_int, c_void};

#[inline(always)]
fn checked(len: usize, capacity: usize) {
    if len > capacity {
        // Darwin's checked entry points terminate the process rather than
        // returning a partially written result.
        unsafe { libc::abort() }
    }
}

pub unsafe extern "C" fn bzero(dst: *mut c_void, len: usize) {
    unsafe { std::ptr::write_bytes(dst, 0, len) };
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

/// Darwin's public `glob_t` layout.
#[repr(C)]
pub struct Glob {
    pub gl_pathc: usize,
    pub gl_matchc: c_int,
    pub gl_offs: usize,
    pub gl_flags: c_int,
    pub gl_pathv: *mut *mut c_char,
    reserved: [*mut c_void; 5],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct HostGlob {
    pathc: usize,
    pathv: *mut *mut c_char,
    offs: usize,
    flags: c_int,
    reserved: [*mut c_void; 5],
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "glob"]
    fn host_glob(
        pattern: *const c_char,
        flags: c_int,
        err: Option<unsafe extern "C" fn(*const c_char, c_int) -> c_int>,
        out: *mut HostGlob,
    ) -> c_int;
}

#[cfg(target_os = "linux")]
pub unsafe extern "C" fn glob(
    pattern: *const c_char,
    flags: c_int,
    err: Option<unsafe extern "C" fn(*const c_char, c_int) -> c_int>,
    out: *mut Glob,
) -> c_int {
    // Darwin and Linux assign different bits to all but NOCHECK.
    let pairs = [
        (1, 32),
        (2, 8),
        (4, 1),
        (8, 2),
        (16, 16),
        (32, 4),
        (0x2000, 64),
    ];
    let host_flags = pairs.iter().fold(0, |v, (darwin, host)| {
        v | if flags & darwin != 0 { *host } else { 0 }
    });
    let previous = if flags & 1 != 0 {
        unsafe { (*out).gl_pathc }
    } else {
        0
    };
    let mut host = HostGlob {
        pathc: previous,
        pathv: if flags & 1 != 0 {
            unsafe { (*out).gl_pathv }
        } else {
            std::ptr::null_mut()
        },
        offs: unsafe { (*out).gl_offs },
        flags: 0,
        reserved: if flags & 1 != 0 {
            unsafe { (*out).reserved }
        } else {
            [std::ptr::null_mut(); 5]
        },
    };
    let status = unsafe { host_glob(pattern, host_flags, err, &raw mut host) };
    unsafe {
        (*out).gl_pathc = host.pathc;
        (*out).gl_matchc = host.pathc.saturating_sub(previous) as c_int;
        (*out).gl_offs = host.offs;
        (*out).gl_flags = flags;
        (*out).gl_pathv = host.pathv;
        (*out).reserved = host.reserved;
    }
    match status {
        1 => -1,
        2 => -2,
        3 => -3,
        other => other,
    }
}

#[cfg(not(target_os = "linux"))]
pub unsafe extern "C" fn glob(
    pattern: *const c_char,
    flags: c_int,
    err: Option<unsafe extern "C" fn(*const c_char, c_int) -> c_int>,
    out: *mut Glob,
) -> c_int {
    unsafe { libc::glob(pattern, flags, err, out.cast()) }
}

/// Darwin's `termios`, whose speeds follow the control-character array.
#[repr(C)]
pub struct Termios {
    pub c_iflag: u64,
    pub c_oflag: u64,
    pub c_cflag: u64,
    pub c_lflag: u64,
    pub c_cc: [u8; 20],
    pub c_ispeed: u64,
    pub c_ospeed: u64,
}

#[cfg(target_os = "linux")]
fn map_to_darwin(host: libc::tcflag_t, pairs: &[(u64, libc::tcflag_t)]) -> u64 {
    pairs.iter().fold(0, |out, (darwin, linux)| {
        out | if host & linux != 0 { *darwin } else { 0 }
    })
}

#[cfg(target_os = "linux")]
fn map_to_host(darwin: u64, pairs: &[(u64, libc::tcflag_t)]) -> libc::tcflag_t {
    pairs.iter().fold(0, |out, (darwin_bit, linux)| {
        out | if darwin & darwin_bit != 0 { *linux } else { 0 }
    })
}

#[cfg(target_os = "linux")]
const IFLAGS: &[(u64, libc::tcflag_t)] = &[
    (0x1, libc::IGNBRK),
    (0x2, libc::BRKINT),
    (0x4, libc::IGNPAR),
    (0x8, libc::PARMRK),
    (0x10, libc::INPCK),
    (0x20, libc::ISTRIP),
    (0x40, libc::INLCR),
    (0x80, libc::IGNCR),
    (0x100, libc::ICRNL),
    (0x200, libc::IXON),
    (0x400, libc::IXOFF),
    (0x800, libc::IXANY),
    (0x2000, libc::IMAXBEL),
];
#[cfg(target_os = "linux")]
const OFLAGS: &[(u64, libc::tcflag_t)] = &[(0x1, libc::OPOST), (0x2, libc::ONLCR)];
#[cfg(target_os = "linux")]
const CFLAGS: &[(u64, libc::tcflag_t)] = &[
    (0x400, libc::CSTOPB),
    (0x800, libc::CREAD),
    (0x1000, libc::PARENB),
    (0x2000, libc::PARODD),
    (0x4000, libc::HUPCL),
    (0x8000, libc::CLOCAL),
];
#[cfg(target_os = "linux")]
const LFLAGS: &[(u64, libc::tcflag_t)] = &[
    (0x1, libc::ECHOKE),
    (0x2, libc::ECHOE),
    (0x4, libc::ECHOK),
    (0x8, libc::ECHO),
    (0x10, libc::ECHONL),
    (0x20, libc::ECHOPRT),
    (0x40, libc::ECHOCTL),
    (0x80, libc::ISIG),
    (0x100, libc::ICANON),
    (0x400, libc::IEXTEN),
    (0x00400000, libc::TOSTOP),
    (0x00800000, libc::FLUSHO),
    (0x20000000, libc::PENDIN),
    (0x80000000, libc::NOFLSH),
];

#[cfg(target_os = "linux")]
fn speed_to_darwin(speed: libc::speed_t) -> u64 {
    for (darwin, linux) in SPEEDS {
        if speed == *linux {
            return *darwin;
        }
    }
    speed as u64
}

#[cfg(target_os = "linux")]
fn speed_to_host(speed: u64) -> libc::speed_t {
    for (darwin, linux) in SPEEDS {
        if speed == *darwin {
            return *linux;
        }
    }
    speed as libc::speed_t
}

#[cfg(target_os = "linux")]
const SPEEDS: &[(u64, libc::speed_t)] = &[
    (0, libc::B0),
    (50, libc::B50),
    (75, libc::B75),
    (110, libc::B110),
    (134, libc::B134),
    (150, libc::B150),
    (200, libc::B200),
    (300, libc::B300),
    (600, libc::B600),
    (1200, libc::B1200),
    (1800, libc::B1800),
    (2400, libc::B2400),
    (4800, libc::B4800),
    (9600, libc::B9600),
    (19200, libc::B19200),
    (38400, libc::B38400),
    (57600, libc::B57600),
    (115200, libc::B115200),
    (230400, libc::B230400),
];

#[cfg(target_os = "linux")]
const CC: &[(usize, usize)] = &[
    (0, libc::VEOF),
    (1, libc::VEOL),
    (2, libc::VEOL2),
    (3, libc::VERASE),
    (4, libc::VWERASE),
    (5, libc::VKILL),
    (6, libc::VREPRINT),
    (8, libc::VINTR),
    (9, libc::VQUIT),
    (10, libc::VSUSP),
    (12, libc::VSTART),
    (13, libc::VSTOP),
    (14, libc::VLNEXT),
    (15, libc::VDISCARD),
    (16, libc::VMIN),
    (17, libc::VTIME),
];

#[cfg(target_os = "linux")]
pub unsafe extern "C" fn tcgetattr(fd: c_int, out: *mut Termios) -> c_int {
    let mut host = std::mem::MaybeUninit::<libc::termios>::uninit();
    let status = unsafe { libc::tcgetattr(fd, host.as_mut_ptr()) };
    if status != 0 {
        return status;
    }
    let host = unsafe { host.assume_init() };
    let mut cc = [0; 20];
    for (darwin, linux) in CC {
        cc[*darwin] = host.c_cc[*linux];
    }
    let char_size = host.c_cflag & libc::CSIZE;
    let darwin_char_size = match char_size {
        libc::CS6 => 0x100,
        libc::CS7 => 0x200,
        libc::CS8 => 0x300,
        _ => 0,
    };
    unsafe {
        out.write(Termios {
            c_iflag: map_to_darwin(host.c_iflag, IFLAGS),
            c_oflag: map_to_darwin(host.c_oflag, OFLAGS),
            c_cflag: map_to_darwin(host.c_cflag, CFLAGS) | darwin_char_size,
            c_lflag: map_to_darwin(host.c_lflag, LFLAGS),
            c_cc: cc,
            c_ispeed: speed_to_darwin(libc::cfgetispeed(&host)),
            c_ospeed: speed_to_darwin(libc::cfgetospeed(&host)),
        })
    };
    0
}

#[cfg(target_os = "linux")]
pub unsafe extern "C" fn tcsetattr(fd: c_int, action: c_int, input: *const Termios) -> c_int {
    let input = unsafe { &*input };
    let mut host = unsafe { std::mem::MaybeUninit::<libc::termios>::zeroed().assume_init() };
    host.c_iflag = map_to_host(input.c_iflag, IFLAGS);
    host.c_oflag = map_to_host(input.c_oflag, OFLAGS);
    host.c_cflag = map_to_host(input.c_cflag, CFLAGS)
        | match input.c_cflag & 0x300 {
            0x100 => libc::CS6,
            0x200 => libc::CS7,
            0x300 => libc::CS8,
            _ => libc::CS5,
        };
    host.c_lflag = map_to_host(input.c_lflag, LFLAGS);
    for (darwin, linux) in CC {
        host.c_cc[*linux] = input.c_cc[*darwin];
    }
    unsafe {
        libc::cfsetispeed(&mut host, speed_to_host(input.c_ispeed));
        libc::cfsetospeed(&mut host, speed_to_host(input.c_ospeed));
    }
    let host_action = match action {
        0 => libc::TCSANOW,
        1 => libc::TCSADRAIN,
        2 => libc::TCSAFLUSH,
        _ => action,
    };
    unsafe { libc::tcsetattr(fd, host_action, &host) }
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

#[cfg(not(target_os = "linux"))]
pub unsafe extern "C" fn tcgetattr(fd: c_int, out: *mut Termios) -> c_int {
    unsafe { libc::tcgetattr(fd, out.cast()) }
}

#[cfg(not(target_os = "linux"))]
pub unsafe extern "C" fn tcsetattr(fd: c_int, action: c_int, input: *const Termios) -> c_int {
    unsafe { libc::tcsetattr(fd, action, input.cast()) }
}

exports::exports! {
    functions {
        "__bzero" => bzero,
        "__memcpy_chk" => memcpy_chk,
        "__memmove_chk" => memmove_chk,
        "__memset_chk" => memset_chk,
        "__sprintf_chk" => rosetta3_sprintf_chk,
        "__strcat_chk" => strcat_chk,
        "__strcpy_chk" => strcpy_chk,
        "__strlcat_chk" => strlcat_chk,
        "__strlcpy_chk" => strlcpy_chk,
        "glob" => glob,
        "tcgetattr" => tcgetattr,
        "tcsetattr" => tcsetattr,
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
    fn every_compatibility_entry_is_published() {
        for name in [
            "bzero",
            "__memcpy_chk",
            "__memmove_chk",
            "__memset_chk",
            "__sprintf_chk",
            "__strcat_chk",
            "__strcpy_chk",
            "__strlcat_chk",
            "__strlcpy_chk",
            "glob",
            "tcgetattr",
            "tcsetattr",
        ] {
            assert!(symbol(name).is_some(), "{name}");
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
