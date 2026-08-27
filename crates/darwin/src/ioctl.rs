#[cfg(target_os = "macos")]
pub fn request(request: u64) -> libc::c_ulong {
    request as libc::c_ulong
}

#[cfg(not(target_os = "macos"))]
pub fn request(request: u64) -> libc::Ioctl {
    request as libc::Ioctl
}
