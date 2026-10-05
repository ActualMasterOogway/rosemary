use std::ffi::c_int;

pub extern "C" fn csr_check(_mask: u32) -> c_int {
    0
}

pub extern "C" fn rootless_restricted_environment() -> c_int {
    0
}

pub extern "C" fn rootless_check_trusted_fd(fd: c_int) -> c_int {
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return -1;
    }
    1
}

exports::exports! {
    functions {
        "csr_check" => csr_check,
        "rootless_restricted_environment" => rootless_restricted_environment,
        "rootless_check_trusted_fd" => rootless_check_trusted_fd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_protected() {
        assert_eq!(csr_check(1 << 1), 0);
        assert_eq!(rootless_restricted_environment(), 0);
        assert_eq!(rootless_check_trusted_fd(2), 1);
        assert_eq!(rootless_check_trusted_fd(-1), -1);
    }
}
