use std::ffi::c_int;

unsafe extern "C" fn res_9_init() -> c_int {
    unsafe { libc::res_init() }
}

// FIXME (ACCURACY): stub!
unsafe extern "C" fn res_9_close() -> c_int {
    0
}

exports::exports! {
    functions {
        "res_9_init" => res_9_init,
        "res_9_close" => res_9_close,
    }
}
