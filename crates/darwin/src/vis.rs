//! `<vis.h>`: `vis`, `strvis`, `unvis`.
//!
//! BSD's, which glibc never had; libedit saves and loads its history through
//! them. Ported from Apple's Libc (`gen/FreeBSD/vis.c`, NetBSD 1.62, and
//! `unvis.c`, NetBSD 1.41) with **Apple's** flag numbering, which is not
//! NetBSD's current one: `VIS_GLOB` is `0x100` and `UNVIS_END` is `0x1000`
//! there, and the guest was compiled against those.
//!
//! Characters are decoded with the host's `mbtowc`, which is the locale the
//! guest's own `setlocale` set: the point of the multibyte path is that a
//! character the locale can print is left alone.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int};

const VIS_OCTAL: c_int = 0x0001;
const VIS_CSTYLE: c_int = 0x0002;
const VIS_SP: c_int = 0x0004;
const VIS_TAB: c_int = 0x0008;
const VIS_NL: c_int = 0x0010;
const VIS_SAFE: c_int = 0x0020;
const VIS_NOSLASH: c_int = 0x0040;
const VIS_HTTPSTYLE: c_int = 0x0080;
const VIS_GLOB: c_int = 0x0100;
const VIS_MIMESTYLE: c_int = 0x0200;
const VIS_HTTP1866: c_int = 0x0400;
const VIS_NOESCAPE: c_int = 0x0800;
const UNVIS_END: c_int = 0x1000;

const UNVIS_VALID: c_int = 1;
const UNVIS_VALIDPUSH: c_int = 2;
const UNVIS_NOCHAR: c_int = 3;
const UNVIS_SYNBAD: c_int = -1;

/// Darwin's `MB_LEN_MAX`, the most `mbtowc` is asked to read.
const MB_LEN_MAX: usize = 6;

unsafe extern "C" {
    fn mbtowc(pwc: *mut libc::wchar_t, s: *const c_char, n: usize) -> c_int;
    fn wctomb(s: *mut c_char, wc: libc::wchar_t) -> c_int;
    fn iswgraph(c: u32) -> c_int;
    fn iswalnum(c: u32) -> c_int;
    fn iswspace(c: u32) -> c_int;
    fn iswcntrl(c: u32) -> c_int;
}

type Wide = u32;

fn graph(c: Wide) -> bool {
    // SAFETY: a pure classification of a value.
    unsafe { iswgraph(c) != 0 }
}

/// Apple's `do_mbyte`: one byte of a character that is being encoded.
fn mbyte(dst: &mut Vec<Wide>, c: Wide, flags: c_int, nextc: Wide, extra: bool) {
    let push2 = |dst: &mut Vec<Wide>, b: u8| {
        dst.push(b'\\' as Wide);
        dst.push(b as Wide);
    };
    if flags & VIS_CSTYLE != 0 {
        let short = match c {
            0x0a => Some(b'n'),
            0x0d => Some(b'r'),
            0x08 => Some(b'b'),
            0x07 => Some(b'a'),
            0x0b => Some(b'v'),
            0x09 => Some(b't'),
            0x0c => Some(b'f'),
            0x20 => Some(b's'),
            _ => None,
        };
        if let Some(b) = short {
            return push2(dst, b);
        }
        if c == 0 {
            push2(dst, b'0');
            if (b'0' as Wide..=b'7' as Wide).contains(&(nextc & 0xff)) {
                dst.push(b'0' as Wide);
                dst.push(b'0' as Wide);
            }
            return;
        }
        if graph(c) {
            dst.push(b'\\' as Wide);
            dst.push(c);
            return;
        }
    }
    if extra || (c & 0o177) == b' ' as Wide || flags & VIS_OCTAL != 0 {
        let b = c & 0xff;
        dst.push(b'\\' as Wide);
        dst.push(((b >> 6) & 0o3) + b'0' as Wide);
        dst.push(((b >> 3) & 0o7) + b'0' as Wide);
        dst.push((c & 0o7) + b'0' as Wide);
        return;
    }
    if flags & VIS_NOSLASH == 0 {
        dst.push(b'\\' as Wide);
    }
    let mut c = c;
    if c & 0o200 != 0 {
        c &= 0o177;
        dst.push(b'M' as Wide);
    }
    // SAFETY: a pure classification of a value.
    if unsafe { iswcntrl(c) } != 0 {
        dst.push(b'^' as Wide);
        dst.push(if c == 0o177 {
            b'?' as Wide
        } else {
            c + b'@' as Wide
        });
    } else {
        dst.push(b'-' as Wide);
        dst.push(c);
    }
}

/// Apple's `do_svis`.
fn svis(dst: &mut Vec<Wide>, c: Wide, flags: c_int, nextc: Wide, extra: &[Wide]) {
    // A NUL is always "extra": Apple's `wcschr` finds the list's terminator.
    let is_extra = c == 0 || extra.contains(&c);
    let white = c == b' ' as Wide || c == b'\t' as Wide || c == b'\n' as Wide;
    let safe = c == 0x08 || c == 0x07 || c == b'\r' as Wide;
    if !is_extra && (graph(c) || white || (flags & VIS_SAFE != 0 && safe)) {
        dst.push(c);
        return;
    }
    // Each significant byte of the character, most significant first.
    let mut wmsk: u64 = 0;
    for i in (0..8).rev() {
        let shift = i * 8;
        let bmsk = 0xffu64 << shift;
        wmsk |= bmsk;
        if (u64::from(c) & wmsk) != 0 || i == 0 {
            let byte = ((u64::from(c) & bmsk) >> shift) as Wide;
            mbyte(dst, byte, flags, nextc, is_extra);
        }
    }
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Apple's `do_hvis`, RFC 1808.
fn hvis(dst: &mut Vec<Wide>, c: Wide, flags: c_int, nextc: Wide, extra: &[Wide]) {
    // SAFETY: a pure classification of a value.
    let alnum = unsafe { iswalnum(c) } != 0;
    if alnum || b"$-_.+!*'(),".iter().any(|&b| c == b as Wide) {
        svis(dst, c, flags, nextc, extra);
    } else {
        dst.push(b'%' as Wide);
        dst.push(HEX_LOWER[((c >> 4) & 0xf) as usize] as Wide);
        dst.push(HEX_LOWER[(c & 0xf) as usize] as Wide);
    }
}

/// Apple's `do_mvis`, quoted-printable.
fn mvis(dst: &mut Vec<Wide>, c: Wide, flags: c_int, nextc: Wide, extra: &[Wide]) {
    // SAFETY: a pure classification of a value.
    let space = unsafe { iswspace(c) } != 0;
    let escape = c != b'\n' as Wide
        && ((space && (nextc == b'\r' as Wide || nextc == b'\n' as Wide))
            || (!space && (c < 33 || (c > 60 && c < 62) || c > 126))
            || b"#$@[\\]^`{|}~".iter().any(|&b| c == b as Wide));
    if escape {
        dst.push(b'=' as Wide);
        dst.push(HEX_UPPER[((c >> 4) & 0xf) as usize] as Wide);
        dst.push(HEX_UPPER[(c & 0xf) as usize] as Wide);
    } else {
        svis(dst, c, flags, nextc, extra);
    }
}

/// Apple's `istrsenvisx` with no output limit: encode `len` characters of
/// `src`, which holds at least one more byte for the look-ahead when `len` is 1.
fn encode(src: &[u8], len: usize, flags: c_int, extra: &[u8]) -> Vec<u8> {
    // Input: bytes to characters, falling back to bytes for good after the
    // first sequence the locale cannot decode.
    let mut wide = Vec::with_capacity(src.len());
    let mut failed = false;
    let mut at = 0;
    let mut remaining = if len == 1 { 2 } else { len } as isize;
    while remaining > 0 && at < src.len() {
        let mut clen: isize = -1;
        let mut w: libc::wchar_t = 0;
        if !failed {
            let n = MB_LEN_MAX.min(src.len() - at);
            // SAFETY: `n` bytes of `src` from `at`, and a local to write.
            clen = unsafe { mbtowc(&mut w, src.as_ptr().add(at).cast(), n) } as isize;
        }
        if failed || clen < 0 {
            w = src[at] as libc::wchar_t;
            clen = 1;
            failed = true;
        }
        if clen == 0 {
            clen = 1;
        }
        wide.push(w as Wide);
        at += clen as usize;
        remaining -= clen;
    }
    let count = wide.len().min(len);

    let mut extras: Vec<Wide> = extra.iter().map(|&b| b as Wide).collect();
    if flags & VIS_GLOB != 0 {
        extras.extend(b"*?[#".map(Wide::from));
    }
    if flags & VIS_SP != 0 {
        extras.push(b' ' as Wide);
    }
    if flags & VIS_TAB != 0 {
        extras.push(b'\t' as Wide);
    }
    if flags & VIS_NL != 0 {
        extras.push(b'\n' as Wide);
    }
    if flags & VIS_NOSLASH == 0 {
        extras.push(b'\\' as Wide);
    }

    let visit: fn(&mut Vec<Wide>, Wide, c_int, Wide, &[Wide]) = if flags & VIS_HTTPSTYLE != 0 {
        hvis
    } else if flags & VIS_MIMESTYLE != 0 {
        mvis
    } else {
        svis
    };
    let mut out = Vec::new();
    for i in 0..count {
        let next = wide.get(i + 1).copied().unwrap_or(0);
        visit(&mut out, wide[i], flags, next, &extras);
    }

    // Output: characters back to bytes, with the same fallback.
    let mut bytes = Vec::with_capacity(out.len());
    for &c in &out {
        let mut buf = [0 as c_char; 16];
        let mut clen: c_int = -1;
        if !failed {
            // SAFETY: a buffer larger than any host's MB_CUR_MAX.
            clen = unsafe { wctomb(buf.as_mut_ptr(), c as libc::wchar_t) };
        }
        if failed || clen < 0 {
            let mut wmsk: u64 = 0;
            for i in (0..8).rev() {
                let shift = i * 8;
                let bmsk = 0xffu64 << shift;
                wmsk |= bmsk;
                if (u64::from(c) & wmsk) != 0 || i == 0 {
                    bytes.push(((u64::from(c) & bmsk) >> shift) as u8);
                }
            }
            failed = true;
        } else {
            bytes.extend(buf[..clen as usize].iter().map(|&b| b as u8));
        }
    }
    bytes
}

/// Copy `bytes` and a terminator to `dst`, returning the terminator's address.
///
/// # Safety
///
/// `dst` is writable for `bytes.len() + 1`.
unsafe fn emit(dst: *mut c_char, bytes: &[u8]) -> *mut c_char {
    // SAFETY: the caller's obligation.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), dst, bytes.len());
        let end = dst.add(bytes.len());
        *end = 0;
        end
    }
}

/// `char *vis(char *dst, int c, int flag, int nextc)`
///
/// # Safety
///
/// `dst` has room for the encoding, which the header documents as four bytes
/// per input byte plus the terminator.
pub unsafe extern "C" fn vis(
    dst: *mut c_char,
    c: c_int,
    flags: c_int,
    nextc: c_int,
) -> *mut c_char {
    let cc = [c as u8, nextc as u8];
    let out = encode(&cc, 1, flags, b"");
    // SAFETY: the caller's obligation.
    unsafe { emit(dst, &out) }
}

/// `int strvis(char *dst, const char *src, int flag)`
///
/// # Safety
///
/// `src` is NUL-terminated and `dst` holds four times its length plus one.
pub unsafe extern "C" fn strvis(dst: *mut c_char, src: *const c_char, flags: c_int) -> c_int {
    let src = match src.is_null() {
        true => &[][..],
        // SAFETY: the caller's obligation.
        false => unsafe { CStr::from_ptr(src) }.to_bytes(),
    };
    let out = encode(src, src.len(), flags, b"");
    // SAFETY: the caller's obligation.
    unsafe { emit(dst, &out) };
    out.len() as c_int
}

const S_GROUND: u32 = 0;
const S_START: u32 = 1;
const S_META: u32 = 2;
const S_META1: u32 = 3;
const S_CTRL: u32 = 4;
const S_OCTAL2: u32 = 5;
const S_OCTAL3: u32 = 6;
const S_HEX: u32 = 7;
const S_HEX1: u32 = 8;
const S_HEX2: u32 = 9;
const S_MIME1: u32 = 10;
const S_MIME2: u32 = 11;
const S_EATCRNL: u32 = 12;
const S_AMP: u32 = 13;
const S_NUMBER: u32 = 14;
const S_STRING: u32 = 15;

/// RFC 1866 entity names, in Apple's order, which the string state walks.
const ENTITIES: &[(&str, u8)] = &[
    ("AElig", 198),
    ("Aacute", 193),
    ("Acirc", 194),
    ("Agrave", 192),
    ("Aring", 197),
    ("Atilde", 195),
    ("Auml", 196),
    ("Ccedil", 199),
    ("ETH", 208),
    ("Eacute", 201),
    ("Ecirc", 202),
    ("Egrave", 200),
    ("Euml", 203),
    ("Iacute", 205),
    ("Icirc", 206),
    ("Igrave", 204),
    ("Iuml", 207),
    ("Ntilde", 209),
    ("Oacute", 211),
    ("Ocirc", 212),
    ("Ograve", 210),
    ("Oslash", 216),
    ("Otilde", 213),
    ("Ouml", 214),
    ("THORN", 222),
    ("Uacute", 218),
    ("Ucirc", 219),
    ("Ugrave", 217),
    ("Uuml", 220),
    ("Yacute", 221),
    ("aacute", 225),
    ("acirc", 226),
    ("acute", 180),
    ("aelig", 230),
    ("agrave", 224),
    ("amp", 38),
    ("aring", 229),
    ("atilde", 227),
    ("auml", 228),
    ("brvbar", 166),
    ("ccedil", 231),
    ("cedil", 184),
    ("cent", 162),
    ("copy", 169),
    ("curren", 164),
    ("deg", 176),
    ("divide", 247),
    ("eacute", 233),
    ("ecirc", 234),
    ("egrave", 232),
    ("eth", 240),
    ("euml", 235),
    ("frac12", 189),
    ("frac14", 188),
    ("frac34", 190),
    ("gt", 62),
    ("iacute", 237),
    ("icirc", 238),
    ("iexcl", 161),
    ("igrave", 236),
    ("iquest", 191),
    ("iuml", 239),
    ("laquo", 171),
    ("lt", 60),
    ("macr", 175),
    ("micro", 181),
    ("middot", 183),
    ("nbsp", 160),
    ("not", 172),
    ("ntilde", 241),
    ("oacute", 243),
    ("ocirc", 244),
    ("ograve", 242),
    ("ordf", 170),
    ("ordm", 186),
    ("oslash", 248),
    ("otilde", 245),
    ("ouml", 246),
    ("para", 182),
    ("plusmn", 177),
    ("pound", 163),
    ("quot", 34),
    ("raquo", 187),
    ("reg", 174),
    ("sect", 167),
    ("shy", 173),
    ("sup1", 185),
    ("sup2", 178),
    ("sup3", 179),
    ("szlig", 223),
    ("thorn", 254),
    ("times", 215),
    ("uacute", 250),
    ("ucirc", 251),
    ("ugrave", 249),
    ("uml", 168),
    ("uuml", 252),
    ("yacute", 253),
    ("yen", 165),
    ("yuml", 255),
];

/// Byte `i` of entity `ia`'s name, NUL past its end as in Apple's `char[7]`.
fn entity_byte(ia: usize, i: usize) -> u8 {
    ENTITIES[ia].0.as_bytes().get(i).copied().unwrap_or(0)
}

fn xtod(c: u8) -> u8 {
    match c.is_ascii_digit() {
        true => c - b'0',
        false => c.to_ascii_lowercase() - b'a' + 10,
    }
}

/// Apple's `unvis`, on a Rust view of its three outputs.
fn step(cp: &mut u8, c: c_int, state: &mut u32, flag: c_int) -> c_int {
    let uc = c as u8;
    let st = *state & 0xff;
    let ss = |index: u32, s: u32| (index << 24) | s;
    let octal = (b'0'..=b'7').contains(&uc);

    if flag & UNVIS_END != 0 {
        return match st {
            S_OCTAL2 | S_OCTAL3 | S_HEX2 => {
                *state = ss(0, S_GROUND);
                UNVIS_VALID
            }
            S_GROUND => UNVIS_NOCHAR,
            _ => UNVIS_SYNBAD,
        };
    }

    let bad = |state: &mut u32| {
        *state = ss(0, S_GROUND);
        UNVIS_SYNBAD
    };

    match st {
        S_GROUND => {
            *cp = 0;
            if flag & VIS_NOESCAPE == 0 && uc == b'\\' {
                *state = ss(0, S_START);
                return UNVIS_NOCHAR;
            }
            if flag & VIS_HTTPSTYLE != 0 && uc == b'%' {
                *state = ss(0, S_HEX1);
                return UNVIS_NOCHAR;
            }
            if flag & VIS_HTTP1866 != 0 && uc == b'&' {
                *state = ss(0, S_AMP);
                return UNVIS_NOCHAR;
            }
            if flag & VIS_MIMESTYLE != 0 && uc == b'=' {
                *state = ss(0, S_MIME1);
                return UNVIS_NOCHAR;
            }
            *cp = uc;
            UNVIS_VALID
        }
        S_START => {
            let simple = match uc {
                b'\\' => Some(b'\\'),
                b'n' => Some(b'\n'),
                b'r' => Some(b'\r'),
                b'b' => Some(0x08),
                b'a' => Some(0x07),
                b'v' => Some(0x0b),
                b't' => Some(b'\t'),
                b'f' => Some(0x0c),
                b's' => Some(b' '),
                b'E' => Some(0x1b),
                _ => None,
            };
            if let Some(v) = simple {
                *cp = v;
                *state = ss(0, S_GROUND);
                return UNVIS_VALID;
            }
            match uc {
                b'0'..=b'7' => {
                    *cp = uc - b'0';
                    *state = ss(0, S_OCTAL2);
                }
                b'M' => {
                    *cp = 0o200;
                    *state = ss(0, S_META);
                }
                b'^' => *state = ss(0, S_CTRL),
                b'x' => *state = ss(0, S_HEX),
                // A hidden newline, and a hidden marker.
                b'\n' | b'$' => *state = ss(0, S_GROUND),
                _ => return bad(state),
            }
            UNVIS_NOCHAR
        }
        S_META => {
            match uc {
                b'-' => *state = ss(0, S_META1),
                b'^' => *state = ss(0, S_CTRL),
                _ => return bad(state),
            }
            UNVIS_NOCHAR
        }
        S_META1 => {
            *state = ss(0, S_GROUND);
            *cp |= uc;
            UNVIS_VALID
        }
        S_CTRL => {
            *cp |= if uc == b'?' { 0o177 } else { uc & 0o37 };
            *state = ss(0, S_GROUND);
            UNVIS_VALID
        }
        S_OCTAL2 => {
            if octal {
                *cp = (*cp << 3).wrapping_add(uc - b'0');
                *state = ss(0, S_OCTAL3);
                return UNVIS_NOCHAR;
            }
            *state = ss(0, S_GROUND);
            UNVIS_VALIDPUSH
        }
        S_OCTAL3 => {
            *state = ss(0, S_GROUND);
            if octal {
                *cp = (*cp << 3).wrapping_add(uc - b'0');
                return UNVIS_VALID;
            }
            UNVIS_VALIDPUSH
        }
        S_HEX | S_HEX1 => {
            if st == S_HEX && !uc.is_ascii_hexdigit() {
                return bad(state);
            }
            if uc.is_ascii_hexdigit() {
                *cp = xtod(uc);
                *state = ss(0, S_HEX2);
                return UNVIS_NOCHAR;
            }
            *state = ss(0, S_GROUND);
            UNVIS_VALIDPUSH
        }
        S_HEX2 => {
            *state = S_GROUND;
            if uc.is_ascii_hexdigit() {
                *cp = xtod(uc) | (*cp << 4);
                return UNVIS_VALID;
            }
            UNVIS_VALIDPUSH
        }
        S_MIME1 => {
            if uc == b'\n' || uc == b'\r' {
                *state = ss(0, S_EATCRNL);
                return UNVIS_NOCHAR;
            }
            if uc.is_ascii_hexdigit() && (uc.is_ascii_digit() || uc.is_ascii_uppercase()) {
                *cp = xtod(uc);
                *state = ss(0, S_MIME2);
                return UNVIS_NOCHAR;
            }
            bad(state)
        }
        S_MIME2 => {
            if uc.is_ascii_hexdigit() && (uc.is_ascii_digit() || uc.is_ascii_uppercase()) {
                *state = ss(0, S_GROUND);
                *cp = xtod(uc) | (*cp << 4);
                return UNVIS_VALID;
            }
            bad(state)
        }
        S_EATCRNL => match uc {
            b'\r' | b'\n' => UNVIS_NOCHAR,
            b'=' => {
                *state = ss(0, S_MIME1);
                UNVIS_NOCHAR
            }
            _ => {
                *cp = uc;
                *state = ss(0, S_GROUND);
                UNVIS_VALID
            }
        },
        S_AMP | S_STRING => {
            if st == S_AMP {
                *cp = 0;
                if uc == b'#' {
                    *state = ss(0, S_NUMBER);
                    return UNVIS_NOCHAR;
                }
                *state = ss(0, S_STRING);
            }
            let mut ia = *cp as usize;
            let is = (*state >> 24) as usize;
            let lc = if is == 0 { 0 } else { entity_byte(ia, is - 1) };
            let uc = if uc == b';' { 0 } else { uc };
            while ia < ENTITIES.len() {
                if is != 0 && entity_byte(ia, is - 1) != lc {
                    return bad(state);
                }
                if entity_byte(ia, is) == uc {
                    break;
                }
                ia += 1;
            }
            if ia == ENTITIES.len() {
                return bad(state);
            }
            if uc != 0 {
                *cp = ia as u8;
                *state = ss(is as u32 + 1, S_STRING);
                return UNVIS_NOCHAR;
            }
            *cp = ENTITIES[ia].1;
            *state = ss(0, S_GROUND);
            UNVIS_VALID
        }
        S_NUMBER => {
            if uc == b';' {
                return UNVIS_VALID;
            }
            if !uc.is_ascii_digit() {
                return bad(state);
            }
            *cp = cp.wrapping_add(cp.wrapping_mul(10)).wrapping_add(uc - b'0');
            UNVIS_NOCHAR
        }
        _ => bad(state),
    }
}

/// `int unvis(char *cp, int c, int *astate, int flag)`
///
/// # Safety
///
/// `cp` and `astate` are writable, and `astate` holds 0 or what an earlier
/// call left there.
pub unsafe extern "C" fn unvis(
    cp: *mut c_char,
    c: c_int,
    astate: *mut c_int,
    flag: c_int,
) -> c_int {
    // SAFETY: the caller's obligation.
    unsafe {
        let mut byte = *cp as u8;
        let mut state = *astate as u32;
        let rc = step(&mut byte, c, &mut state, flag);
        *cp = byte as c_char;
        *astate = state as c_int;
        rc
    }
}

exports::exports! {
    functions {
        "vis" => vis,
        "strvis" => strvis,
        "unvis" => unvis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIS_WHITE: c_int = VIS_SP | VIS_TAB | VIS_NL;

    fn strvis_of(s: &[u8], flags: c_int) -> Vec<u8> {
        encode(s, s.len(), flags, b"")
    }

    fn decode(s: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut state = 0;
        let mut c = 0u8;
        let mut i = 0;
        while i < s.len() {
            match step(&mut c, s[i] as c_int, &mut state, 0) {
                UNVIS_VALID => out.push(c),
                UNVIS_VALIDPUSH => {
                    out.push(c);
                    continue;
                }
                UNVIS_NOCHAR => {}
                other => panic!("unvis said {other} at {i}"),
            }
            i += 1;
        }
        if step(&mut c, 0, &mut state, UNVIS_END) == UNVIS_VALID {
            out.push(c);
        }
        out
    }

    /// What libedit writes to its history file, and reads back.
    #[test]
    fn history_lines_round_trip_through_vis_white() {
        let line = b"echo \"a b\"\tc\\d";
        let encoded = strvis_of(line, VIS_WHITE);
        assert_eq!(encoded, b"echo\\040\"a\\040b\"\\011c\\134d");
        assert_eq!(decode(&encoded), line);
    }

    #[test]
    fn control_and_meta_characters_use_the_caret_form() {
        assert_eq!(strvis_of(b"\x01", 0), b"\\^A");
        assert_eq!(strvis_of(b"\x7f", 0), b"\\^?");
        assert_eq!(strvis_of(b"\x01", VIS_CSTYLE | VIS_OCTAL), b"\\001");
        assert_eq!(strvis_of(b"\n", VIS_CSTYLE | VIS_NL), b"\\n");
        assert_eq!(decode(b"\\^A\\M-a\\n"), b"\x01\xe1\n");
    }

    #[test]
    fn unvis_end_finishes_a_pending_octal() {
        assert_eq!(decode(b"x\\1"), b"x\x01");
        assert_eq!(decode(b"\\101B"), b"AB");
    }

    #[test]
    fn a_trailing_backslash_is_a_syntax_error_at_the_end() {
        let mut c = 0u8;
        let mut state = 0;
        assert_eq!(step(&mut c, b'\\' as c_int, &mut state, 0), UNVIS_NOCHAR);
        assert_eq!(step(&mut c, 0, &mut state, UNVIS_END), UNVIS_SYNBAD);
    }

    #[test]
    fn vis_writes_one_character_and_returns_the_end() {
        let mut buf = [0 as c_char; 8];
        // SAFETY: eight bytes, more than one encoded character needs.
        let end = unsafe { vis(buf.as_mut_ptr(), b' ' as c_int, VIS_SP, 0) };
        // SAFETY: `vis` returned a pointer into `buf`.
        let len = unsafe { end.offset_from(buf.as_ptr()) } as usize;
        assert_eq!(
            &buf[..len].iter().map(|&b| b as u8).collect::<Vec<_>>(),
            b"\\040"
        );
    }

    #[test]
    fn http_and_entity_decoding_follow_their_flags() {
        let mut c = 0u8;
        let mut state = 0;
        let mut out = Vec::new();
        for &b in b"%41" {
            if step(&mut c, b as c_int, &mut state, VIS_HTTPSTYLE) == UNVIS_VALID {
                out.push(c);
            }
        }
        assert_eq!(out, b"A");
        let mut out = Vec::new();
        for &b in b"&amp;" {
            if step(&mut c, b as c_int, &mut state, VIS_HTTP1866) == UNVIS_VALID {
                out.push(c);
            }
        }
        assert_eq!(out, b"&");
    }
}
