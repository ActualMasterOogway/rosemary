use core::ffi::{c_int, c_void};

use crate::signal::{Code, Context};

pub const SIGEMT: Option<c_int> = None;
pub const SIGINFO: Option<c_int> = None;

pub fn code(_signo: c_int, _code: c_int) -> Code {
    Code::Unknown
}

pub unsafe fn context(_: *const c_void) -> Option<Context> {
    None
}

pub unsafe fn set_context(_: *mut c_void, _: &Context) {}
