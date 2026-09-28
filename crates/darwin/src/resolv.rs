use std::ffi::c_int;

unsafe extern "C" fn res_9_init() -> c_int {
    #[cfg(target_os = "macos")]
    {
        unsafe { host_res_init() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { libc::res_init() }
    }
}

unsafe extern "C" fn res_9_close() {
    #[cfg(target_os = "macos")]
    unsafe {
        host_res_close()
    }
}

#[cfg(target_os = "macos")]
#[link(name = "resolv")]
unsafe extern "C" {
    #[link_name = "res_9_init"]
    fn host_res_init() -> c_int;
    #[link_name = "res_9_close"]
    fn host_res_close();
}

exports::exports! {
    functions {
        "res_9_init" => res_9_init,
        "res_9_close" => res_9_close,
    }
}
