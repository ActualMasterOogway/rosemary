//! POSIX regular expressions, which are the same *API* on both hosts and not
//! the same ABI.
//!
//! Three things diverge, and each is silent on its own:
//!
//! * `regmatch_t` holds two `regoff_t`, and `regoff_t` is `off_t` - sixty-four
//!   bits - on Darwin and `int` under glibc. A guest that passes an array of
//!   them gets every second field written into the first half of the next
//!   entry, and reads a match offset that is a stack address.
//! * `regex_t` is four fields and thirty-two bytes on Darwin, and sixty-four
//!   bytes under glibc. A guest that declares one statically - which is what a
//!   compiled-once pattern is - has its *neighbours* overwritten by a host
//!   `regcomp`.
//! * `REG_NEWLINE` and `REG_NOSUB` have each other's values. A guest asking for
//!   one gets the other, and neither failure looks like a flag.
//!
//! So the guest's `regex_t` is this crate's structure, holding a host one
//! allocated beside it, and the matches are translated on the way back. The
//! matching itself is the host's: a second regular-expression engine would be a
//! different dialect, which is the one difference a caller would see in its
//! results rather than in a crash.

#![cfg(not(target_os = "macos"))]

use std::ffi::{c_char, c_int, c_void};

// The host's own `regcomp` family, declared here because the `libc` crate does
// not publish it for this target.
unsafe extern "C" {
    fn regcomp(preg: *mut c_void, pattern: *const c_char, cflags: c_int) -> c_int;
    fn regexec(
        preg: *const c_void,
        string: *const c_char,
        nmatch: usize,
        pmatch: *mut HostMatch,
        eflags: c_int,
    ) -> c_int;
    fn regfree(preg: *mut c_void);
}

/// `regmatch_t` as the host lays it out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HostMatch {
    rm_so: libc::regoff_t,
    rm_eo: libc::regoff_t,
}

/// `regmatch_t` as Darwin lays it out: two `off_t`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GuestMatch {
    rm_so: i64,
    rm_eo: i64,
}

const _: () = assert!(size_of::<GuestMatch>() == 16);

/// The host's `regex_t`, mirrored so that `re_nsub` - the one field POSIX makes
/// public and a guest reads - can be got at.
///
/// Checked against the `libc` crate's own declaration for this target, which is
/// what keeps the mirror from drifting.
#[cfg(target_env = "musl")]
#[repr(C)]
struct HostRegex {
    re_nsub: usize,
    opaque: *mut c_void,
    padding: [usize; 4],
    nsub2: usize,
    padding2: u8,
}

#[cfg(not(target_env = "musl"))]
#[repr(C)]
struct HostRegex {
    buffer: *mut c_void,
    allocated: usize,
    used: usize,
    syntax: u64,
    fastmap: *mut c_char,
    translate: *mut c_char,
    re_nsub: usize,
    bits: u8,
}

const _: () = assert!(size_of::<HostRegex>() == size_of::<libc::regex_t>());

/// Darwin's `regex_t`: `{ int re_magic; size_t re_nsub; const char *re_endp;
/// struct re_guts *re_g; }`. The guest declared this, so the layout is ABI.
///
/// `re_g` is opaque to the guest on Darwin too - it is the compiled program -
/// so this crate's host `regex_t` goes there and nothing is being smuggled into
/// a field a caller reads.
#[repr(C)]
pub struct GuestRegex {
    re_magic: i32,
    _pad: u32,
    re_nsub: usize,
    re_endp: *const c_char,
    re_g: *mut HostRegex,
}

const _: () = assert!(size_of::<GuestRegex>() == 32);

/// What this crate leaves in `re_magic`, so that a `regexec` on storage no
/// `regcomp` of ours filled is refused rather than followed into a wild
/// pointer. Not Darwin's value: nothing may depend on the number, and a
/// distinct one says which implementation compiled the pattern.
const MAGIC: i32 = 0x5233_5245;

/// Darwin's `<regex.h>` compile flags.
mod darwin {
    pub const EXTENDED: i32 = 0o001;
    pub const ICASE: i32 = 0o002;
    pub const NOSUB: i32 = 0o004;
    pub const NEWLINE: i32 = 0o010;
    pub const NOSPEC: i32 = 0o020;
    pub const PEND: i32 = 0o040;
    /// Everything above, so that a flag nothing here knows can be told from one
    /// that is merely unsupported.
    pub const KNOWN: i32 = EXTENDED | ICASE | NOSUB | NEWLINE | NOSPEC | PEND | 0o200;
}

/// The host's, which agree on the first two and swap the next two.
mod host {
    pub const EXTENDED: i32 = 1;
    pub const ICASE: i32 = 2;
    pub const NEWLINE: i32 = 4;
    pub const NOSUB: i32 = 8;
}

/// Darwin's error codes. The first thirteen are the host's as well; from
/// fourteen the two numberings part, which is why [`from_host_error`] exists.
pub const REG_NOMATCH: i32 = 1;
const REG_BADPAT: i32 = 2;
const REG_ESPACE: i32 = 12;
const REG_ASSERT: i32 = 15;
const REG_INVARG: i32 = 16;

/// `REG_STARTEND`, which both spell `4` and which makes `pmatch[0]` an *input*.
const REG_STARTEND: i32 = 4;

/// Translate a host error number into Darwin's.
///
/// One to thirteen are the same code with the same meaning. Above that glibc
/// has `REG_EEND`, `REG_ESIZE` and `REG_ERPAREN` where Darwin has `REG_EMPTY`,
/// `REG_ASSERT` and `REG_INVARG`, so the three are mapped by meaning rather
/// than passed through as numbers that name something else.
fn from_host_error(code: c_int) -> i32 {
    match code {
        0..=13 => code,
        // REG_EEND: "premature end", which is the host saying its own matcher
        // got into a state it does not describe.
        14 => REG_ASSERT,
        // REG_ESIZE: the compiled pattern is too large.
        15 => REG_ESPACE,
        // REG_ERPAREN.
        16 => 8,
        _ => REG_BADPAT,
    }
}

/// Translate compile flags, or say which one cannot be honoured.
fn compile_flags(cflags: i32) -> Result<c_int, &'static str> {
    if cflags & darwin::NOSPEC != 0 {
        return Err("REG_NOSPEC: a literal-text match, which the host's regcomp has no flag for");
    }
    if cflags & darwin::PEND != 0 {
        return Err("REG_PEND: a pattern bounded by re_endp rather than by a NUL");
    }
    if cflags & !darwin::KNOWN != 0 {
        return Err("a compile flag this crate does not know");
    }
    let mut out = 0;
    if cflags & darwin::EXTENDED != 0 {
        out |= host::EXTENDED;
    }
    if cflags & darwin::ICASE != 0 {
        out |= host::ICASE;
    }
    if cflags & darwin::NEWLINE != 0 {
        out |= host::NEWLINE;
    }
    if cflags & darwin::NOSUB != 0 {
        out |= host::NOSUB;
    }
    Ok(out)
}

/// `int regcomp(regex_t *preg, const char *pattern, int cflags)`
///
/// # Safety
///
/// `preg` is storage for a Darwin `regex_t` and `pattern` a C string.
pub unsafe extern "C" fn darwin_regcomp(
    preg: *mut GuestRegex,
    pattern: *const c_char,
    cflags: c_int,
) -> c_int {
    if preg.is_null() || pattern.is_null() {
        return REG_INVARG;
    }
    let cflags = match compile_flags(cflags) {
        Ok(flags) => flags,
        Err(why) => {
            eprintln!("rosetta3: darwin: regcomp: {why}");
            return REG_INVARG;
        }
    };
    // Zeroed, because the host's `regcomp` reads the fields it is about to fill
    // when the pattern is recompiled into the same structure.
    let host = Box::into_raw(Box::new(unsafe { std::mem::zeroed::<HostRegex>() }));
    // SAFETY: a box just made, and the caller's C string.
    let status = unsafe { regcomp(host.cast(), pattern, cflags) };
    if status != 0 {
        // SAFETY: nothing was compiled into it, so only the box is freed.
        drop(unsafe { Box::from_raw(host) });
        // SAFETY: the caller states `preg` is writable.
        unsafe { (*preg).re_g = std::ptr::null_mut() };
        return from_host_error(status);
    }
    // SAFETY: the caller states `preg` is a whole `regex_t`, and `host` is live.
    unsafe {
        (*preg).re_magic = MAGIC;
        (*preg)._pad = 0;
        (*preg).re_nsub = (*host).re_nsub;
        (*preg).re_endp = std::ptr::null();
        (*preg).re_g = host;
    }
    0
}

/// `int regexec(const regex_t *preg, const char *string, size_t nmatch,
/// regmatch_t pmatch[], int eflags)`
///
/// # Safety
///
/// `preg` is a compiled Darwin `regex_t`, `string` a C string, and `pmatch` is
/// null or writable for `nmatch` Darwin `regmatch_t`.
pub unsafe extern "C" fn darwin_regexec(
    preg: *const GuestRegex,
    string: *const c_char,
    nmatch: usize,
    pmatch: *mut GuestMatch,
    eflags: c_int,
) -> c_int {
    if preg.is_null() || string.is_null() {
        return REG_INVARG;
    }
    // SAFETY: the caller states `preg` is a `regex_t`.
    let (magic, host) = unsafe { ((*preg).re_magic, (*preg).re_g) };
    if magic != MAGIC || host.is_null() {
        // Storage no `regcomp` of this crate's filled in. Following `re_g`
        // would be a jump into whatever the guest left there.
        return REG_INVARG;
    }
    let wanted = match pmatch.is_null() {
        true => 0,
        false => nmatch,
    };
    let mut matches = vec![HostMatch::default(); wanted];
    if eflags & REG_STARTEND != 0 && wanted > 0 {
        // The one direction that is an input: `pmatch[0]` bounds the search.
        // SAFETY: the caller states `pmatch` holds `nmatch` entries.
        let first = unsafe { *pmatch };
        matches[0] = HostMatch {
            rm_so: first.rm_so as libc::regoff_t,
            rm_eo: first.rm_eo as libc::regoff_t,
        };
    }
    // SAFETY: a compiled host pattern, the caller's string, and a vector of
    // exactly the length passed with it.
    let status = unsafe {
        regexec(
            host.cast::<c_void>().cast_const(),
            string,
            wanted,
            matches.as_mut_ptr(),
            eflags,
        )
    };
    if status != 0 {
        return from_host_error(status);
    }
    for (at, found) in matches.iter().enumerate() {
        // SAFETY: `wanted` is the caller's own `nmatch`, and `pmatch` is
        // non-null whenever it is not zero.
        unsafe {
            *pmatch.add(at) = GuestMatch {
                rm_so: i64::from(found.rm_so),
                rm_eo: i64::from(found.rm_eo),
            };
        }
    }
    0
}

/// `void regfree(regex_t *preg)`
///
/// # Safety
///
/// `preg` is null or a `regex_t` [`darwin_regcomp`] filled in.
pub unsafe extern "C" fn darwin_regfree(preg: *mut GuestRegex) {
    if preg.is_null() {
        return;
    }
    // SAFETY: the caller states `preg` is a `regex_t`.
    let (magic, host) = unsafe { ((*preg).re_magic, (*preg).re_g) };
    if magic != MAGIC || host.is_null() {
        return;
    }
    // SAFETY: a pattern this crate compiled and has not freed - the magic is
    // cleared below, so a second `regfree` of the same structure does nothing.
    unsafe {
        regfree(host.cast());
        drop(Box::from_raw(host));
        (*preg).re_magic = 0;
        (*preg).re_g = std::ptr::null_mut();
        (*preg).re_nsub = 0;
    }
}

/// The message Darwin gives for each of its own error codes.
fn describe(code: i32) -> &'static str {
    match code {
        0 => "no errors detected",
        REG_NOMATCH => "regexec() failed to match",
        REG_BADPAT => "invalid regular expression",
        3 => "invalid collating element",
        4 => "invalid character class",
        5 => "trailing backslash (\\)",
        6 => "invalid backreference number",
        7 => "brackets ([ ]) not balanced",
        8 => "parentheses ( ) not balanced",
        9 => "braces { } not balanced",
        10 => "invalid repetition count(s)",
        11 => "invalid character range",
        REG_ESPACE => "out of memory",
        13 => "repetition-operator operand invalid",
        14 => "empty (sub)expression",
        REG_ASSERT => "\"can't happen\" -- you found a bug",
        REG_INVARG => "invalid argument to regex routine",
        _ => "*** unknown regexp error code ***",
    }
}

/// `size_t regerror(int errcode, const regex_t *preg, char *errbuf,
/// size_t errbuf_size)`
///
/// Answers the size the whole message needs, terminator included, and copies as
/// much of it as fits - which is what a caller sizing a second buffer reads.
///
/// # Safety
///
/// `errbuf` is null or writable for `size` bytes.
pub unsafe extern "C" fn darwin_regerror(
    code: c_int,
    _preg: *const GuestRegex,
    errbuf: *mut c_char,
    size: usize,
) -> usize {
    let message = describe(code);
    let needed = message.len() + 1;
    if errbuf.is_null() || size == 0 {
        return needed;
    }
    let fits = message.len().min(size - 1);
    // SAFETY: the caller states `errbuf` holds `size` bytes, and `fits` leaves
    // room for the terminator.
    unsafe {
        std::ptr::copy_nonoverlapping(message.as_ptr().cast::<c_char>(), errbuf, fits);
        *errbuf.add(fits) = 0;
    }
    needed
}

exports::exports! {
    functions {
        "regcomp" => darwin_regcomp,
        "regexec" => darwin_regexec,
        "regfree" => darwin_regfree,
        "regerror" => darwin_regerror,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    fn compile(pattern: &CStr, cflags: c_int) -> GuestRegex {
        let mut preg = GuestRegex {
            re_magic: 0,
            _pad: 0,
            re_nsub: 0,
            re_endp: std::ptr::null(),
            re_g: std::ptr::null_mut(),
        };
        // SAFETY: storage for a whole `regex_t`, and a C string.
        let status = unsafe { darwin_regcomp(&mut preg, pattern.as_ptr(), cflags) };
        assert_eq!(status, 0, "compiling {pattern:?}");
        preg
    }

    /// The bug this module exists for: the sub-match offsets land in the
    /// guest's own sixteen-byte entries, not in glibc's eight-byte ones.
    #[test]
    fn submatches_are_written_at_darwins_width() {
        let mut preg = compile(c"([a-z]+)=([0-9]+)", darwin::EXTENDED);
        assert_eq!(preg.re_nsub, 2);
        let mut found = [GuestMatch::default(); 3];
        // SAFETY: a compiled pattern, a C string, and three whole entries.
        let status = unsafe {
            darwin_regexec(
                &preg,
                c"  width=1280;".as_ptr(),
                found.len(),
                found.as_mut_ptr(),
                0,
            )
        };
        assert_eq!(status, 0);
        assert_eq!((found[0].rm_so, found[0].rm_eo), (2, 12));
        assert_eq!((found[1].rm_so, found[1].rm_eo), (2, 7));
        assert_eq!((found[2].rm_so, found[2].rm_eo), (8, 12));
        // SAFETY: a pattern this test compiled.
        unsafe { darwin_regfree(&mut preg) };
    }

    /// `REG_NEWLINE` and `REG_NOSUB` have each other's values on the two hosts,
    /// so the flag that reaches the host has to be the translated one.
    #[test]
    fn newline_is_newline_and_not_nosub() {
        let mut preg = compile(c"^b", darwin::EXTENDED | darwin::NEWLINE);
        let mut found = [GuestMatch::default(); 1];
        // SAFETY: a compiled pattern and one whole entry.
        let status = unsafe { darwin_regexec(&preg, c"a\nb".as_ptr(), 1, found.as_mut_ptr(), 0) };
        assert_eq!(status, 0, "REG_NEWLINE was not honoured");
        assert_eq!((found[0].rm_so, found[0].rm_eo), (2, 3));
        // SAFETY: a pattern this test compiled.
        unsafe { darwin_regfree(&mut preg) };
    }

    /// A pattern that does not match says so in Darwin's numbering, which is
    /// the same as the host's for this one.
    #[test]
    fn a_failure_to_match_is_reported() {
        let mut preg = compile(c"[0-9]+", darwin::EXTENDED);
        // SAFETY: a compiled pattern; a null `pmatch` with `nmatch` zero.
        let status =
            unsafe { darwin_regexec(&preg, c"none here".as_ptr(), 0, std::ptr::null_mut(), 0) };
        assert_eq!(status, REG_NOMATCH);
        // SAFETY: a pattern this test compiled.
        unsafe { darwin_regfree(&mut preg) };
    }

    /// Storage nothing compiled into is refused rather than followed - a guest
    /// that declares a `regex_t` statically starts with one full of zeroes.
    #[test]
    fn uncompiled_storage_is_refused() {
        let preg = GuestRegex {
            re_magic: 0,
            _pad: 0,
            re_nsub: 0,
            re_endp: std::ptr::null(),
            re_g: std::ptr::null_mut(),
        };
        // SAFETY: a readable `regex_t` and a C string.
        let status = unsafe { darwin_regexec(&preg, c"x".as_ptr(), 0, std::ptr::null_mut(), 0) };
        assert_eq!(status, REG_INVARG);
    }

    /// A flag with no host counterpart is refused by name rather than dropped,
    /// because a literal-text match done as a pattern matches something else.
    #[test]
    fn a_flag_with_no_counterpart_is_refused() {
        let mut preg = GuestRegex {
            re_magic: 0,
            _pad: 0,
            re_nsub: 0,
            re_endp: std::ptr::null(),
            re_g: std::ptr::null_mut(),
        };
        // SAFETY: storage for a whole `regex_t`, and a C string.
        let status = unsafe { darwin_regcomp(&mut preg, c"a.b".as_ptr(), darwin::NOSPEC) };
        assert_eq!(status, REG_INVARG);
    }

    /// The message is the whole length even when the buffer is not, which is
    /// what a caller that allocates from the answer depends on.
    #[test]
    fn the_error_message_reports_its_whole_length() {
        let mut small = [0_i8; 8];
        // SAFETY: a buffer with its own length.
        let needed = unsafe {
            darwin_regerror(
                REG_NOMATCH,
                std::ptr::null(),
                small.as_mut_ptr(),
                small.len(),
            )
        };
        assert_eq!(needed, describe(REG_NOMATCH).len() + 1);
        // SAFETY: the call above NUL-terminated it.
        let text = unsafe { CStr::from_ptr(small.as_ptr()) };
        assert_eq!(text.to_bytes(), b"regexec");
    }
}
