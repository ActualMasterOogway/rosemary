use std::ffi::{CStr, c_char};

use darwin::regex::{
    GuestMatch, GuestRegex, darwin_regcomp, darwin_regerror, darwin_regexec, darwin_regfree,
    darwin_regncomp, darwin_regnexec, flags,
};

fn storage() -> GuestRegex {
    GuestRegex {
        re_magic: 0,
        _pad: 0,
        re_nsub: 0,
        re_endp: std::ptr::null(),
        re_g: std::ptr::null_mut(),
    }
}

fn run(pattern: &[u8], cflags: i32, input: &[u8], eflags: i32) -> (i32, usize, [GuestMatch; 4]) {
    let mut regex = storage();
    let mut found = [GuestMatch::default(); 4];
    let status = unsafe {
        darwin_regncomp(
            &mut regex,
            pattern.as_ptr().cast::<c_char>(),
            pattern.len(),
            cflags,
        )
    };
    assert_eq!(status, 0, "compiling {pattern:?}");
    let result = unsafe {
        darwin_regnexec(
            &regex,
            input.as_ptr().cast::<c_char>(),
            input.len(),
            found.len(),
            found.as_mut_ptr(),
            eflags,
        )
    };
    let groups = regex.re_nsub;
    unsafe { darwin_regfree(&mut regex) };
    (result, groups, found)
}

#[test]
fn posix_longest_and_darwin_offsets() {
    let (status, groups, found) = run(b"(a|ab)(c)", flags::EXTENDED, b"abc", 0);
    assert_eq!(status, 0);
    assert_eq!(groups, 2);
    assert_eq!(found[0], GuestMatch { rm_so: 0, rm_eo: 3 });
    assert_eq!(found[1], GuestMatch { rm_so: 0, rm_eo: 2 });
    assert_eq!(found[2], GuestMatch { rm_so: 2, rm_eo: 3 });
    assert_eq!(
        found[3],
        GuestMatch {
            rm_so: -1,
            rm_eo: -1
        }
    );
}

#[test]
fn icase_does_not_fold_punctuation() {
    let (status, _, _) = run(b"[", flags::NOSPEC | flags::ICASE, b"{", 0);
    assert_eq!(status, darwin::regex::REG_NOMATCH);
}

#[test]
fn newline_and_nosub_are_distinct() {
    let (status, _, found) = run(b"^b", flags::EXTENDED | flags::NEWLINE, b"a\nb", 0);
    assert_eq!(status, 0);
    assert_eq!(found[0], GuestMatch { rm_so: 2, rm_eo: 3 });
    let (status, _, _) = run(b"a.b", flags::EXTENDED | flags::NEWLINE, b"a\nb", 0);
    assert_eq!(status, darwin::regex::REG_NOMATCH);
}

#[test]
fn bounded_calls_include_nul_and_startend_with_zero_output() {
    let (status, _, found) = run(b"a\0b", flags::EXTENDED, b"xa\0by", 0);
    assert_eq!(status, 0);
    assert_eq!(found[0], GuestMatch { rm_so: 1, rm_eo: 4 });
    let mut regex = storage();
    assert_eq!(unsafe { darwin_regcomp(&mut regex, c"b".as_ptr(), 0) }, 0);
    let mut bound = GuestMatch { rm_so: 2, rm_eo: 3 };
    assert_eq!(
        unsafe {
            darwin_regexec(
                &regex,
                b"a\0bZ".as_ptr().cast(),
                0,
                &mut bound,
                flags::STARTEND,
            )
        },
        0
    );
    assert_eq!(bound, GuestMatch { rm_so: 2, rm_eo: 3 });
    unsafe { darwin_regfree(&mut regex) };
}

#[test]
fn enhanced_minimal_and_shortcuts() {
    let (status, _, found) = run(
        b"a+",
        flags::EXTENDED | flags::ENHANCED | flags::MINIMAL,
        b"aaaa",
        0,
    );
    assert_eq!(status, 0);
    assert_eq!(found[0], GuestMatch { rm_so: 0, rm_eo: 1 });
    let (status, _, found) = run(br"\d+", flags::EXTENDED | flags::ENHANCED, b"a123", 0);
    assert_eq!(status, 0);
    assert_eq!(found[0], GuestMatch { rm_so: 1, rm_eo: 4 });
}

#[test]
fn regerror_converts_names_and_reports_full_length() {
    let mut text = [0_i8; 64];
    let size = unsafe {
        darwin_regerror(
            1 | flags::ITOA,
            std::ptr::null(),
            text.as_mut_ptr(),
            text.len(),
        )
    };
    assert_eq!(size, b"REG_NOMATCH".len() + 1);
    assert_eq!(
        unsafe { CStr::from_ptr(text.as_ptr()) }.to_bytes(),
        b"REG_NOMATCH"
    );
    let mut regex = storage();
    regex.re_endp = c"REG_BADPAT".as_ptr();
    unsafe { darwin_regerror(flags::ATOI, &regex, text.as_mut_ptr(), text.len()) };
    assert_eq!(unsafe { CStr::from_ptr(text.as_ptr()) }.to_bytes(), b"2");
}
