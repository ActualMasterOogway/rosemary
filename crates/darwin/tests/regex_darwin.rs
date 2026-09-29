#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_int, c_void};

use darwin::regex::{
    GuestMatch, GuestRegex, darwin_regcomp, darwin_regerror, darwin_regexec, darwin_regfree,
    darwin_regncomp, darwin_regnexec, flags,
};

unsafe extern "C" {
    #[link_name = "regcomp"]
    fn native_comp(preg: *mut c_void, pattern: *const c_char, flags: c_int) -> c_int;
    #[link_name = "regexec"]
    fn native_exec(
        preg: *const c_void,
        string: *const c_char,
        nmatch: usize,
        pmatch: *mut GuestMatch,
        flags: c_int,
    ) -> c_int;
    #[link_name = "regfree"]
    fn native_free(preg: *mut c_void);
    #[link_name = "regerror"]
    fn native_error(code: c_int, preg: *const c_void, buffer: *mut c_char, size: usize) -> usize;
    #[link_name = "regncomp"]
    fn native_ncomp(preg: *mut c_void, pattern: *const c_char, len: usize, flags: c_int) -> c_int;
    #[link_name = "regnexec"]
    fn native_nexec(
        preg: *const c_void,
        string: *const c_char,
        len: usize,
        nmatch: usize,
        pmatch: *mut GuestMatch,
        flags: c_int,
    ) -> c_int;
}

#[test]
fn bounded_apis_agree_with_native() {
    let pattern = b"a\0b";
    let input = b"xa\0by";
    let mut native = [0_u64; 4];
    let mut builtin = GuestRegex {
        re_magic: 0,
        _pad: 0,
        re_nsub: 0,
        re_endp: std::ptr::null(),
        re_g: std::ptr::null_mut(),
    };
    assert_eq!(
        unsafe {
            native_ncomp(
                native.as_mut_ptr().cast(),
                pattern.as_ptr().cast(),
                pattern.len(),
                flags::EXTENDED,
            )
        },
        unsafe {
            darwin_regncomp(
                &mut builtin,
                pattern.as_ptr().cast(),
                pattern.len(),
                flags::EXTENDED,
            )
        }
    );
    let mut native_match = [GuestMatch::default(); 2];
    let mut builtin_match = [GuestMatch::default(); 2];
    assert_eq!(
        unsafe {
            native_nexec(
                native.as_ptr().cast(),
                input.as_ptr().cast(),
                input.len(),
                2,
                native_match.as_mut_ptr(),
                0,
            )
        },
        unsafe {
            darwin_regnexec(
                &builtin,
                input.as_ptr().cast(),
                input.len(),
                2,
                builtin_match.as_mut_ptr(),
                0,
            )
        }
    );
    assert_eq!(builtin_match, native_match);
    unsafe {
        native_free(native.as_mut_ptr().cast());
        darwin_regfree(&mut builtin);
    }
}

#[test]
fn pend_and_startend_agree_with_native() {
    let pattern = b"a\0b";
    let input = b"xa\0by";
    let mut native = [0_u64; 4];
    native[2] = (pattern.as_ptr() as usize + pattern.len()) as u64;
    let mut builtin = GuestRegex {
        re_magic: 0,
        _pad: 0,
        re_nsub: 0,
        re_endp: unsafe { pattern.as_ptr().add(pattern.len()) }.cast(),
        re_g: std::ptr::null_mut(),
    };
    assert_eq!(
        unsafe {
            native_comp(
                native.as_mut_ptr().cast(),
                pattern.as_ptr().cast(),
                flags::EXTENDED | flags::PEND,
            )
        },
        unsafe {
            darwin_regcomp(
                &mut builtin,
                pattern.as_ptr().cast(),
                flags::EXTENDED | flags::PEND,
            )
        }
    );
    let mut native_match = [GuestMatch { rm_so: 1, rm_eo: 4 }];
    let mut builtin_match = native_match;
    assert_eq!(
        unsafe {
            native_exec(
                native.as_ptr().cast(),
                input.as_ptr().cast(),
                1,
                native_match.as_mut_ptr(),
                flags::STARTEND,
            )
        },
        unsafe {
            darwin_regexec(
                &builtin,
                input.as_ptr().cast(),
                1,
                builtin_match.as_mut_ptr(),
                flags::STARTEND,
            )
        }
    );
    assert_eq!(builtin_match, native_match);
    unsafe {
        native_free(native.as_mut_ptr().cast());
        darwin_regfree(&mut builtin);
    }
}

#[test]
fn builtin_error_text_agrees_with_native() {
    for code in 0..=18 {
        for code in [code, code | flags::ITOA] {
            let mut native = [0_i8; 128];
            let mut builtin = [0_i8; 128];
            let native_size =
                unsafe { native_error(code, std::ptr::null(), native.as_mut_ptr(), native.len()) };
            let builtin_size = unsafe {
                darwin_regerror(code, std::ptr::null(), builtin.as_mut_ptr(), builtin.len())
            };
            assert_eq!(
                (builtin_size, builtin),
                (native_size, native),
                "error {code}"
            );
        }
    }
}

#[test]
fn builtin_matches_native_c_locale_matrix() {
    let cases: &[(&[u8], i32, &[u8], i32)] = &[
        (b"a|ab\0", flags::EXTENDED, b"abc\0", 0),
        (b"(a|aa)(a?)\0", flags::EXTENDED, b"aa\0", 0),
        (b"(a*)(a*)\0", flags::EXTENDED, b"aaa\0", 0),
        (b"[a-z]+\0", flags::EXTENDED | flags::ICASE, b"ABC\0", 0),
        (b"\\(ab\\)\\1\0", 0, b"abab\0", 0),
        (b"a\\|b\0", 0, b"a|b\0", 0),
        (b"([a-z]+)=([0-9]+)\0", flags::EXTENDED, b"x=12\0", 0),
        (
            b"^b\0",
            flags::EXTENDED | flags::NEWLINE,
            b"a\nb\0",
            flags::NOTBOL,
        ),
        (b"a.b\0", flags::EXTENDED | flags::NEWLINE, b"a\nb\0", 0),
        (b"a.b\0", flags::EXTENDED, b"a\nb\0", 0),
        (
            b"a+\0",
            flags::EXTENDED | flags::DUMP,
            b"aaaa\0",
            flags::TRACE | flags::LARGE | flags::BACKR,
        ),
        (b"a+\0", flags::EXTENDED | flags::MINIMAL, b"aaaa\0", 0),
        (b"a+\0", flags::EXTENDED | flags::NOSUB, b"aaaa\0", 0),
        (b"^b\0", flags::EXTENDED, b"b\0", flags::NOTBOL),
        (b"b$\0", flags::EXTENDED, b"b\0", flags::NOTEOL),
        (b"a.b\0", flags::NOSPEC, b"a.b\0", 0),
        (b"a.b\0", flags::NOSPEC | flags::ICASE, b"A.B\0", 0),
        (b"[\0", flags::EXTENDED, b"x\0", 0),
        (b"a(\0", flags::EXTENDED, b"x\0", 0),
        (b"a)\0", flags::EXTENDED, b"x\0", 0),
        (b"*a\0", flags::EXTENDED, b"x\0", 0),
        (b"a**\0", flags::EXTENDED, b"x\0", 0),
        (b"[z-a]\0", flags::EXTENDED, b"x\0", 0),
        (b"[[:<:]]x[[:>:]]\0", flags::EXTENDED, b"x\0", 0),
        (b"[]a]\0", flags::EXTENDED, b"]\0", 0),
        (b"[^]a]\0", flags::EXTENDED, b"b\0", 0),
        (b"()\0", flags::EXTENDED, b"x\0", 0),
        (b"a|\0", flags::EXTENDED, b"a\0", 0),
        (b"|a\0", flags::EXTENDED, b"a\0", 0),
        (b"[a-z]\0", flags::EXTENDED | flags::ICASE, b"A\0", 0),
        (b"a\\\0", flags::EXTENDED, b"x\0", 0),
        (b"[[:bogus:]]\0", flags::EXTENDED, b"x\0", 0),
        (
            b"a+\0",
            flags::EXTENDED | flags::ENHANCED | flags::MINIMAL,
            b"aaaa\0",
            0,
        ),
        (b"a|ab\0", flags::EXTENDED | flags::ENHANCED, b"abc\0", 0),
        (
            b"^b\0",
            flags::EXTENDED | flags::ENHANCED,
            b"b\0",
            flags::NOTBOL,
        ),
        (
            b"\\Q[a]+\\E\0",
            flags::EXTENDED | flags::ENHANCED,
            b"[a]+\0",
            0,
        ),
        (b"\\x41+\0", flags::EXTENDED | flags::ENHANCED, b"AA\0", 0),
        (b"a+?\0", flags::EXTENDED | flags::ENHANCED, b"aaaa\0", 0),
        (
            b"a+?\0",
            flags::EXTENDED | flags::ENHANCED | flags::MINIMAL,
            b"aaaa\0",
            0,
        ),
        (
            b"a?\0",
            flags::EXTENDED | flags::ENHANCED | flags::MINIMAL,
            b"aaaa\0",
            0,
        ),
        (
            b"a{1,3}\0",
            flags::EXTENDED | flags::ENHANCED | flags::MINIMAL,
            b"aaaa\0",
            0,
        ),
        (
            b"(?:ab)+\0",
            flags::EXTENDED | flags::ENHANCED,
            b"abab\0",
            0,
        ),
        (b"(?i:a)b\0", flags::EXTENDED | flags::ENHANCED, b"Ab\0", 0),
        (b"\\d+\0", flags::EXTENDED | flags::ENHANCED, b"a123\0", 0),
        (b"(a\0", flags::EXTENDED | flags::ENHANCED, b"a\0", 0),
        (b"a\\\0", flags::EXTENDED | flags::ENHANCED, b"a\0", 0),
        (b"[a\0", flags::EXTENDED | flags::ENHANCED, b"a\0", 0),
        (b"*a\0", flags::EXTENDED | flags::ENHANCED, b"a\0", 0),
        (b"\\1\0", flags::EXTENDED | flags::ENHANCED, b"a\0", 0),
    ];
    for &(pattern, compile_flags, input, exec_flags) in cases {
        let mut native = [0_u64; 4];
        let mut builtin = GuestRegex {
            re_magic: 0,
            _pad: 0,
            re_nsub: 0,
            re_endp: std::ptr::null(),
            re_g: std::ptr::null_mut(),
        };
        let native_status = unsafe {
            native_comp(
                native.as_mut_ptr().cast(),
                pattern.as_ptr().cast(),
                compile_flags,
            )
        };
        let builtin_status =
            unsafe { darwin_regcomp(&mut builtin, pattern.as_ptr().cast(), compile_flags) };
        assert_eq!(builtin_status, native_status, "compile {pattern:?}");
        if native_status != 0 {
            continue;
        }
        let mut native_matches = [GuestMatch::default(); 4];
        let mut builtin_matches = [GuestMatch::default(); 4];
        let native_result = unsafe {
            native_exec(
                native.as_ptr().cast(),
                input.as_ptr().cast(),
                4,
                native_matches.as_mut_ptr(),
                exec_flags,
            )
        };
        let builtin_result = unsafe {
            darwin_regexec(
                &builtin,
                input.as_ptr().cast(),
                4,
                builtin_matches.as_mut_ptr(),
                exec_flags,
            )
        };
        assert_eq!(builtin_result, native_result, "execute {pattern:?}");
        if native_result == 0 {
            assert_eq!(builtin_matches, native_matches, "captures {pattern:?}");
        }
        unsafe {
            native_free(native.as_mut_ptr().cast());
            darwin_regfree(&mut builtin);
        }
    }
}
