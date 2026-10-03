use std::ffi::c_int;

unsafe extern "C" fn res_9_init() -> c_int {
    unsafe { libc::res_init() }
}

unsafe extern "C" fn res_9_close() {
    // FIXME (ACCURACY): unimplemented!
}

exports::exports! {
    functions {
        "res_9_init" => res_9_init,
        "res_9_close" => res_9_close,
    }
}
