//! The integer conversions and absolute values whose overflow Darwin defines
//! and a host libc may not.
//!
//! musl's `atoi` accumulates in an `int` and negates it, and its `abs` is
//! `a > 0 ? a : -a`, so an out-of-range string or `INT_MIN` overflows. A host
//! built with `-fsanitize=signed-integer-overflow` in trap mode (Chimera Linux
//! is one) answers that with `SIGILL`. Darwin's `atoi` is `(int)strtol(s, NULL,
//! 10)` and its `abs` wraps, and guests count on both.

use std::ffi::{c_char, c_int, c_long, c_longlong};

/// `int atoi(const char *str)`: the `long` [`atol`] reads, truncated.
///
/// # Safety
///
/// `str` is NUL-terminated.
pub unsafe extern "C" fn atoi(str: *const c_char) -> c_int {
    // SAFETY: the caller's string.
    unsafe { atol(str) as c_int }
}

/// `long atol(const char *str)`: `strtol`, clamped as it clamps.
///
/// # Safety
///
/// `str` is NUL-terminated.
pub unsafe extern "C" fn atol(str: *const c_char) -> c_long {
    // SAFETY: the caller's string; no end pointer is asked for.
    unsafe { libc::strtol(str, std::ptr::null_mut(), 10) }
}

/// `long long atoll(const char *str)`
///
/// # Safety
///
/// `str` is NUL-terminated.
pub unsafe extern "C" fn atoll(str: *const c_char) -> c_longlong {
    // SAFETY: the caller's string; no end pointer is asked for.
    unsafe { libc::strtoll(str, std::ptr::null_mut(), 10) }
}

/// `int abs(int i)`
pub extern "C" fn abs(i: c_int) -> c_int {
    i.wrapping_abs()
}

/// `long labs(long i)`
pub extern "C" fn labs(i: c_long) -> c_long {
    i.wrapping_abs()
}

/// `long long llabs(long long i)`
pub extern "C" fn llabs(i: c_longlong) -> c_longlong {
    i.wrapping_abs()
}

exports::exports! {
    functions {
        "atoi" => atoi,
        "atol" => atol,
        "atoll" => atoll,
        "abs" => abs,
        "labs" => labs,
        "llabs" => llabs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_out_of_range_string_is_strtol_truncated() {
        // SAFETY: literals.
        unsafe {
            assert_eq!(atoi(c"-42".as_ptr()), -42);
            assert_eq!(atoi(c"  2147483647x".as_ptr()), i32::MAX);
            assert_eq!(atoi(c"2147483648".as_ptr()), i32::MIN);
            assert_eq!(atoi(c"-2147483649".as_ptr()), i32::MAX);
            assert_eq!(atoi(c"99999999999999999999".as_ptr()), -1);
            assert_eq!(atol(c"99999999999999999999".as_ptr()), c_long::MAX);
            assert_eq!(atoll(c"-99999999999999999999".as_ptr()), c_longlong::MIN);
        }
    }

    #[test]
    fn the_minimum_is_its_own_absolute_value() {
        assert_eq!(abs(-7), 7);
        assert_eq!(abs(i32::MIN), i32::MIN);
        assert_eq!(labs(c_long::MIN), c_long::MIN);
        assert_eq!(llabs(c_longlong::MIN), c_longlong::MIN);
    }
}
