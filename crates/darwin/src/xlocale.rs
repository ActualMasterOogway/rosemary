//! Darwin's `xlocale.h`, on a host that has only half of it.
//!
//! Darwin gives nearly every libc function a `_l` variant taking an explicit
//! `locale_t`. glibc gives that treatment to about a third of them - `strtod_l`,
//! `strcoll_l`, `strftime_l` - and expects the rest to be reached by *switching*
//! the thread's locale with `uselocale`. So each entry here is the plain
//! function between a `uselocale` pair, and the module exists because the
//! vendored `libc++` imports the whole family.
//!
//! A null `locale_t` means the caller's current locale, which is what Darwin's
//! `xlocale.h` documents - so that is the one case that switches nothing.
//!
//! **The `mbstate_t` is not converted.** Darwin's is 128 bytes and glibc's is
//! eight, and a guest hands its own across. That is safe in the direction it
//! matters - the host writes eight bytes into a buffer with room for a hundred
//! and twenty-eight - and correct as long as the *same* pointer keeps coming
//! back, which it does: the state is opaque to the guest, all-zero means
//! "initial" on both, and every call that advances it is this one. What it would
//! not survive is a guest that copied a partially-consumed state between two
//! objects, and nothing in a C++ standard library does.

#![cfg(not(target_os = "macos"))]

use std::ffi::{c_char, c_int, c_uint, c_void};

use machish::resolve::SyntheticDylib;

/// The thread's locale, put back when this is dropped.
///
/// A guard rather than a pair of calls, because every wrapper below has an early
/// return in it and a restore that is skipped leaves the *thread* in the guest's
/// locale for good.
struct Switched(libc::locale_t);

impl Drop for Switched {
    fn drop(&mut self) {
        // SAFETY: `self.0` is whatever `uselocale` reported before the switch,
        // which is either a live locale or `LC_GLOBAL_LOCALE`.
        unsafe { libc::uselocale(self.0) };
    }
}

/// Make `loc` the thread's locale for as long as the guard lives.
///
/// `None` for a null `loc`, which asks for the locale already in effect.
fn switch(loc: libc::locale_t) -> Option<Switched> {
    if loc.is_null() {
        return None;
    }
    // SAFETY: `loc` is a `locale_t` this process obtained - a guest's came from
    // `newlocale`, which is passed through. A null result means the switch did
    // not happen, and the guard then restores a null, which `uselocale` reads as
    // "the global locale" - the state a thread that never switched is in.
    Some(Switched(unsafe { libc::uselocale(loc) }))
}

/// The plain functions each wrapper reaches, declared here because the `libc`
/// crate does not carry all of them and half a family from two sources is how a
/// prototype comes to disagree with its definition.
///
/// `wchar_t` is `int` and `wint_t` is `unsigned int` on every host this builds
/// for; `mbstate_t` is opaque - see the module note on why it is not converted.
mod plain {
    use super::{Mbstate, c_char, c_int, c_uint};

    unsafe extern "C" {
        pub fn mbtowc(pwc: *mut c_int, s: *const c_char, n: usize) -> c_int;
        pub fn btowc(c: c_int) -> c_uint;
        pub fn wctob(c: c_uint) -> c_int;
        pub fn mbrlen(s: *const c_char, n: usize, ps: Mbstate) -> usize;
        pub fn mbrtowc(pwc: *mut c_int, s: *const c_char, n: usize, ps: Mbstate) -> usize;
        pub fn mbsrtowcs(
            dst: *mut c_int,
            src: *mut *const c_char,
            len: usize,
            ps: Mbstate,
        ) -> usize;
        pub fn mbsnrtowcs(
            dst: *mut c_int,
            src: *mut *const c_char,
            nms: usize,
            len: usize,
            ps: Mbstate,
        ) -> usize;
        pub fn wcrtomb(s: *mut c_char, wc: c_int, ps: Mbstate) -> usize;
        pub fn wcsnrtombs(
            dst: *mut c_char,
            src: *mut *const c_int,
            nwc: usize,
            len: usize,
            ps: Mbstate,
        ) -> usize;
        pub fn wcwidth(wc: c_int) -> c_int;
        pub fn tolower(c: c_int) -> c_int;
        pub fn toupper(c: c_int) -> c_int;
        pub fn localeconv() -> *mut c_void;
        pub fn isalpha(c: c_int) -> c_int;
        pub fn iscntrl(c: c_int) -> c_int;
        pub fn isdigit(c: c_int) -> c_int;
        pub fn isgraph(c: c_int) -> c_int;
        pub fn islower(c: c_int) -> c_int;
        pub fn ispunct(c: c_int) -> c_int;
        pub fn isspace(c: c_int) -> c_int;
        pub fn isupper(c: c_int) -> c_int;
        pub fn isxdigit(c: c_int) -> c_int;
        pub fn isblank(c: c_int) -> c_int;
        pub fn isprint(c: c_int) -> c_int;
        pub fn __ctype_get_mb_cur_max() -> usize;
    }

    use super::c_void;
}

/// A `mbstate_t *`, opaque on purpose.
pub type Mbstate = *mut c_void;

/// Wrap a call in a locale switch, as one `_l` entry point.
///
/// A macro because the alternative is a dozen functions whose bodies differ only
/// in the call in the middle, which is exactly the shape where a near-copy hides
/// the wrong argument order.
macro_rules! localised {
    ($(
        $(#[$meta:meta])*
        $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty = $plain:ident;
    )*) => {$(
        $(#[$meta])*
        /// # Safety
        ///
        /// Every argument is the one the plain function takes, and `loc` is a
        /// `locale_t` this process obtained or null.
        pub unsafe extern "C" fn $name($($arg: $ty),*, loc: libc::locale_t) -> $ret {
            let _switched = switch(loc);
            // SAFETY: the caller's obligation, and the arguments are passed
            // through untouched.
            unsafe { plain::$plain($($arg),*) }
        }
    )*};
}

localised! {
    /// `int mbtowc_l(wchar_t *, const char *, size_t, locale_t)`
    mbtowc_l(pwc: *mut c_int, s: *const c_char, n: usize) -> c_int = mbtowc;
    /// `wint_t btowc_l(int, locale_t)`
    btowc_l(c: c_int) -> c_uint = btowc;
    /// `int wctob_l(wint_t, locale_t)`
    wctob_l(c: c_uint) -> c_int = wctob;
    /// `size_t mbrlen_l(const char *, size_t, mbstate_t *, locale_t)`
    mbrlen_l(s: *const c_char, n: usize, ps: Mbstate) -> usize = mbrlen;
    /// `size_t mbrtowc_l(wchar_t *, const char *, size_t, mbstate_t *, locale_t)`
    mbrtowc_l(pwc: *mut c_int, s: *const c_char, n: usize, ps: Mbstate) -> usize = mbrtowc;
    /// `size_t mbsrtowcs_l(wchar_t *, const char **, size_t, mbstate_t *, locale_t)`
    mbsrtowcs_l(
        dst: *mut c_int,
        src: *mut *const c_char,
        len: usize,
        ps: Mbstate
    ) -> usize = mbsrtowcs;
    /// `size_t mbsnrtowcs_l(wchar_t *, const char **, size_t, size_t, mbstate_t *, locale_t)`
    mbsnrtowcs_l(
        dst: *mut c_int,
        src: *mut *const c_char,
        nms: usize,
        len: usize,
        ps: Mbstate
    ) -> usize = mbsnrtowcs;
    /// `size_t wcrtomb_l(char *, wchar_t, mbstate_t *, locale_t)`
    wcrtomb_l(s: *mut c_char, wc: c_int, ps: Mbstate) -> usize = wcrtomb;
    /// `size_t wcsnrtombs_l(char *, const wchar_t **, size_t, size_t, mbstate_t *, locale_t)`
    wcsnrtombs_l(
        dst: *mut c_char,
        src: *mut *const c_int,
        nwc: usize,
        len: usize,
        ps: Mbstate
    ) -> usize = wcsnrtombs;
    /// `int wcwidth_l(wchar_t, locale_t)`
    wcwidth_l(wc: c_int) -> c_int = wcwidth;
    /// `int tolower_l(int, locale_t)`, which Darwin's `<ctype.h>` also inlines as
    /// `__tolower_l`.
    tolower_l(c: c_int) -> c_int = tolower;
    /// `int toupper_l(int, locale_t)`, likewise.
    toupper_l(c: c_int) -> c_int = toupper;
}

/// The `_CTYPE_*` bits Darwin's `<ctype.h>` masks a rune type with.
///
/// They are ABI: a guest compiled against that header has the constants baked
/// into its own code and hands one to `__maskrune`, so a runtime that numbered
/// them differently would answer a question nobody asked.
mod ctype {
    pub const ALPHA: u32 = 0x0000_0100;
    pub const CONTROL: u32 = 0x0000_0200;
    pub const DIGIT: u32 = 0x0000_0400;
    pub const GRAPH: u32 = 0x0000_0800;
    pub const LOWER: u32 = 0x0000_1000;
    pub const PUNCT: u32 = 0x0000_2000;
    pub const SPACE: u32 = 0x0000_4000;
    pub const UPPER: u32 = 0x0000_8000;
    pub const XDIGIT: u32 = 0x0001_0000;
    pub const BLANK: u32 = 0x0002_0000;
    pub const PRINT: u32 = 0x0004_0000;
}

/// The rune type word for `c`, assembled from the host's `<ctype.h>`.
///
/// Assembled rather than read out of a table, because the table is Darwin's
/// `_DefaultRuneLocale` and this host has no such object. Every bit below is a
/// question the host answers directly; the ones it cannot - `_CTYPE_I`,
/// `_CTYPE_T`, `_CTYPE_Q`, and the character-width field - are left clear, which
/// is what they are for every character in a C or UTF-8 locale anyway.
fn runetype(c: c_int) -> u32 {
    let mut kinds = 0;
    // SAFETY: every one of these takes an `int` and touches no memory.
    unsafe {
        for (test, bit) in [
            (plain::isalpha as unsafe extern "C" fn(c_int) -> c_int, ctype::ALPHA),
            (plain::iscntrl, ctype::CONTROL),
            (plain::isdigit, ctype::DIGIT),
            (plain::isgraph, ctype::GRAPH),
            (plain::islower, ctype::LOWER),
            (plain::ispunct, ctype::PUNCT),
            (plain::isspace, ctype::SPACE),
            (plain::isupper, ctype::UPPER),
            (plain::isxdigit, ctype::XDIGIT),
            (plain::isblank, ctype::BLANK),
            (plain::isprint, ctype::PRINT),
        ] {
            if test(c) != 0 {
                kinds |= bit;
            }
        }
    }
    kinds
}

/// `int __maskrune(__darwin_ct_rune_t, unsigned long)`
///
/// Darwin's `<ctype.h>` inlines `isalpha(c)` to a lookup in
/// `_DefaultRuneLocale` for an ASCII `c` and a call to this for anything else,
/// so a guest reaches it for every non-ASCII character it classifies.
pub unsafe extern "C" fn maskrune(c: c_int, mask: u64) -> c_int {
    (runetype(c) & mask as u32) as c_int
}

/// `int __maskrune_l(__darwin_ct_rune_t, unsigned long, locale_t)`
///
/// # Safety
///
/// `loc` is a `locale_t` this process obtained, or null.
pub unsafe extern "C" fn maskrune_l(c: c_int, mask: u64, loc: libc::locale_t) -> c_int {
    let _switched = switch(loc);
    // SAFETY: touches no memory.
    unsafe { maskrune(c, mask) }
}

/// `int __tolower(__darwin_ct_rune_t)` and `int __toupper(...)`, which Darwin's
/// `<ctype.h>` inlines to for a character outside the cached ASCII range.
pub unsafe extern "C" fn tolower_(c: c_int) -> c_int {
    // SAFETY: takes an `int` and touches no memory.
    unsafe { plain::tolower(c) }
}

pub unsafe extern "C" fn toupper_(c: c_int) -> c_int {
    // SAFETY: as above.
    unsafe { plain::toupper(c) }
}

/// `struct lconv *localeconv_l(locale_t)`
///
/// The pointer is libc's own and is valid until the thread's locale changes
/// again - which the guard below does before returning. That is the same
/// lifetime Darwin's gives, and the caller of a `localeconv` has always had to
/// copy what it wants out before doing anything else.
///
/// # Safety
///
/// `loc` is a `locale_t` this process obtained, or null.
pub unsafe extern "C" fn localeconv_l(loc: libc::locale_t) -> *mut c_void {
    let _switched = switch(loc);
    // SAFETY: takes no argument and returns libc's own storage.
    unsafe { plain::localeconv() }
}

/// `int __mb_cur_max_l(locale_t)`: how many bytes one character can take.
///
/// glibc's `MB_CUR_MAX` is a macro over `__ctype_get_mb_cur_max()`, which is the
/// function this reaches for.
///
/// # Safety
///
/// As [`localeconv_l`].
pub unsafe extern "C" fn mb_cur_max_l(loc: libc::locale_t) -> c_int {
    let _switched = switch(loc);
    // SAFETY: takes no argument.
    unsafe { plain::__ctype_get_mb_cur_max() as c_int }
}

/// Every name this module answers, in the Mach-O spelling.
///
/// Two pairs are the same function under two names: Darwin's `<ctype.h>` inlines
/// `tolower_l` to `__tolower_l`, and a guest compiled against that header
/// imports the underscored one.
pub const NAMES: &[&str] = &[
    "mbtowc_l",
    "btowc_l",
    "wctob_l",
    "mbrlen_l",
    "mbrtowc_l",
    "mbsrtowcs_l",
    "mbsnrtowcs_l",
    "wcrtomb_l",
    "wcsnrtombs_l",
    "wcwidth_l",
    "localeconv_l",
    "__mb_cur_max_l",
    "tolower_l",
    "toupper_l",
    "__tolower_l",
    "__toupper_l",
    "__maskrune",
    "__maskrune_l",
    "__tolower",
    "__toupper",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "mbtowc_l" => mbtowc_l as *const (),
        "btowc_l" => btowc_l as *const (),
        "wctob_l" => wctob_l as *const (),
        "mbrlen_l" => mbrlen_l as *const (),
        "mbrtowc_l" => mbrtowc_l as *const (),
        "mbsrtowcs_l" => mbsrtowcs_l as *const (),
        "mbsnrtowcs_l" => mbsnrtowcs_l as *const (),
        "wcrtomb_l" => wcrtomb_l as *const (),
        "wcsnrtombs_l" => wcsnrtombs_l as *const (),
        "wcwidth_l" => wcwidth_l as *const (),
        "localeconv_l" => localeconv_l as *const (),
        "__mb_cur_max_l" => mb_cur_max_l as *const (),
        "tolower_l" | "__tolower_l" => tolower_l as *const (),
        "toupper_l" | "__toupper_l" => toupper_l as *const (),
        "__maskrune" => maskrune as *const (),
        "__maskrune_l" => maskrune_l as *const (),
        "__tolower" => tolower_ as *const (),
        "__toupper" => toupper_ as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap_or(0) as *const ());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switch is undone even when the wrapped call fails, which is the whole
    /// reason it is a guard: a thread left in the guest's locale formats every
    /// number this runtime prints afterwards.
    #[test]
    fn a_switch_is_undone() {
        // SAFETY: a locale this test makes and frees, and `uselocale` with a
        // null argument only reports.
        unsafe {
            let before = libc::uselocale(std::ptr::null_mut());
            let c = libc::newlocale(libc::LC_ALL_MASK, c"C".as_ptr(), std::ptr::null_mut());
            if c.is_null() {
                return;
            }
            {
                let _switched = switch(c);
                assert_eq!(libc::uselocale(std::ptr::null_mut()), c);
            }
            assert_eq!(libc::uselocale(std::ptr::null_mut()), before);
            libc::freelocale(c);
        }
    }

    /// A null locale switches nothing, which is what Darwin's `_l` functions
    /// take it to mean.
    /// Every name the list claims resolves, so `provide` cannot publish a null.
    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&&str> = NAMES.iter().filter(|n| symbol(n).is_none()).collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    #[test]
    fn a_null_locale_switches_nothing() {
        assert!(switch(std::ptr::null_mut()).is_none());
    }

    /// The bits are the header's, and a character has the ones it has.
    #[test]
    fn a_rune_type_says_what_a_character_is() {
        let kinds = runetype('A' as c_int);
        assert_ne!(kinds & ctype::ALPHA, 0);
        assert_ne!(kinds & ctype::UPPER, 0);
        assert_ne!(kinds & ctype::XDIGIT, 0);
        assert_eq!(kinds & ctype::DIGIT, 0);
        assert_eq!(kinds & ctype::SPACE, 0);

        // SAFETY: touches no memory.
        unsafe {
            assert_ne!(maskrune('7' as c_int, ctype::DIGIT as u64), 0);
            assert_eq!(maskrune('7' as c_int, ctype::ALPHA as u64), 0);
        }
    }

    #[test]
    fn one_character_converts() {
        let mut wc: c_int = 0;
        // SAFETY: a writable wide character and a NUL-terminated byte.
        let used = unsafe { mbtowc_l(&mut wc, c"a".as_ptr(), 1, std::ptr::null_mut()) };
        assert_eq!((used, wc), (1, 'a' as c_int));
    }
}
