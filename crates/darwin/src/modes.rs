//! File-mode and file-flag strings: `strmode`, `setmode`/`getmode`,
//! `fflagstostr`.
//!
//! BSD's, which glibc never had. Ported from Apple's Libc (`string/FreeBSD/
//! strmode.c`, `gen/FreeBSD/setmode.c`, `gen/strtofflags.c`) in its x86-64
//! build, which is the non-`VARIANT_LEGACY` one: there `a` includes the sticky
//! bit and an octal mode may set it.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};

const S_IFMT: u32 = 0o170000;
const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFBLK: u32 = 0o060000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFSOCK: u32 = 0o140000;
const S_IFWHT: u32 = 0o160000;
const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
const S_ISTXT: u32 = 0o1000;
const S_IRWXU: u32 = 0o700;
const S_IRUSR: u32 = 0o400;
const S_IWUSR: u32 = 0o200;
const S_IXUSR: u32 = 0o100;
const S_IRWXG: u32 = 0o070;
const S_IRGRP: u32 = 0o040;
const S_IWGRP: u32 = 0o020;
const S_IXGRP: u32 = 0o010;
const S_IRWXO: u32 = 0o007;
const S_IROTH: u32 = 0o004;
const S_IWOTH: u32 = 0o002;
const S_IXOTH: u32 = 0o001;

/// `void strmode(mode_t mode, char *bp)`
///
/// Eleven characters and a NUL: the type, three permission triples, and a
/// trailing space that is where a Mac would put `+` for an ACL.
///
/// # Safety
///
/// `bp` is writable for twelve bytes.
pub unsafe extern "C" fn strmode(mode: c_int, bp: *mut c_char) {
    let out = mode_string(mode as u32);
    // SAFETY: the caller's obligation.
    unsafe { std::ptr::copy_nonoverlapping(out.as_ptr().cast(), bp, out.len()) };
}

fn mode_string(mode: u32) -> [u8; 12] {
    let kind = match mode & S_IFMT {
        S_IFDIR => b'd',
        S_IFCHR => b'c',
        S_IFBLK => b'b',
        S_IFREG => b'-',
        S_IFLNK => b'l',
        S_IFSOCK => b's',
        S_IFIFO => b'p',
        S_IFWHT => b'w',
        _ => b'?',
    };
    let bit = |mask: u32, c: u8| if mode & mask != 0 { c } else { b'-' };
    let exec = |x: u32, special: u32, set: u8, unset: u8| match (mode & x != 0, mode & special != 0)
    {
        (false, false) => b'-',
        (true, false) => b'x',
        (false, true) => unset,
        (true, true) => set,
    };
    [
        kind,
        bit(S_IRUSR, b'r'),
        bit(S_IWUSR, b'w'),
        exec(S_IXUSR, S_ISUID, b's', b'S'),
        bit(S_IRGRP, b'r'),
        bit(S_IWGRP, b'w'),
        exec(S_IXGRP, S_ISGID, b's', b'S'),
        bit(S_IROTH, b'r'),
        bit(S_IWOTH, b'w'),
        exec(S_IXOTH, S_ISTXT, b't', b'T'),
        b' ',
        0,
    ]
}

/// One step of a compiled mode; Apple's `BITCMD`, and the element of the array
/// `setmode` returns for the caller to `free`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct BitCmd {
    cmd: u8,
    cmd2: u8,
    bits: u16,
}

const CMD2_CLR: u8 = 0x01;
const CMD2_SET: u8 = 0x02;
const CMD2_GBITS: u8 = 0x04;
const CMD2_OBITS: u8 = 0x08;
const CMD2_UBITS: u8 = 0x10;

const STANDARD_BITS: u32 = S_ISUID | S_ISGID | S_IRWXU | S_IRWXG | S_IRWXO | S_ISTXT;

/// Apple's `addcmd`, pushing rather than writing through a cursor.
fn addcmd(set: &mut Vec<BitCmd>, op: u8, who: u32, oparg: u32, mask: u32) {
    match op {
        b'=' | b'+' | b'-' | b'X' => {
            let op = if op == b'=' {
                set.push(BitCmd {
                    cmd: b'-',
                    cmd2: 0,
                    bits: (if who != 0 { who } else { STANDARD_BITS }) as u16,
                });
                b'+'
            } else {
                op
            };
            set.push(BitCmd {
                cmd: op,
                cmd2: 0,
                bits: ((if who != 0 { who } else { mask }) & oparg) as u16,
            });
        }
        b'u' | b'g' | b'o' => {
            let (mut cmd2, bits) = match who {
                0 => (CMD2_UBITS | CMD2_GBITS | CMD2_OBITS, mask as u16),
                _ => (
                    (if who & S_IRUSR != 0 { CMD2_UBITS } else { 0 })
                        | (if who & S_IRGRP != 0 { CMD2_GBITS } else { 0 })
                        | (if who & S_IROTH != 0 { CMD2_OBITS } else { 0 }),
                    u16::MAX,
                ),
            };
            cmd2 |= match oparg as u8 {
                b'+' => CMD2_SET,
                b'-' => CMD2_CLR,
                b'=' => CMD2_SET | CMD2_CLR,
                _ => 0,
            };
            set.push(BitCmd {
                cmd: op,
                cmd2,
                bits,
            });
        }
        _ => {}
    }
}

/// Compile a mode string, or `None` where Apple's returns null.
fn compile(p: &[u8], umask: u32) -> Option<Vec<BitCmd>> {
    if p.is_empty() {
        return None;
    }
    // The bits the umask does not clear.
    let mask = !umask;
    let mut set = Vec::new();

    if p[0].is_ascii_digit() {
        let digits = p.iter().take_while(|b| (b'0'..=b'7').contains(b)).count();
        if digits != p.len() {
            return None;
        }
        let perm = p.iter().try_fold(0u64, |acc, d| {
            acc.checked_mul(8)?.checked_add(u64::from(d - b'0'))
        })?;
        if perm & !u64::from(STANDARD_BITS) != 0 {
            return None;
        }
        addcmd(&mut set, b'=', STANDARD_BITS, perm as u32, mask);
        return Some(set);
    }

    let at = |i: usize| p.get(i).copied().unwrap_or(0);
    let mut i = 0;
    let mut equalopdone = false;
    'clauses: loop {
        let mut who = 0;
        loop {
            match at(i) {
                b'a' => who |= STANDARD_BITS,
                b'u' => who |= S_ISUID | S_IRWXU,
                b'g' => who |= S_ISGID | S_IRWXG,
                b'o' => who |= S_IRWXO,
                _ => break,
            }
            i += 1;
        }
        'ops: loop {
            let op = at(i);
            i += 1;
            if op != b'+' && op != b'-' && op != b'=' {
                return None;
            }
            if op == b'=' {
                equalopdone = false;
            }
            let mut perm = 0;
            let mut perm_x = 0;
            loop {
                match at(i) {
                    b'r' => perm |= S_IRUSR | S_IRGRP | S_IROTH,
                    // Set-id and sticky mean nothing for "other" alone.
                    b's' if who == 0 || who & !S_IRWXO != 0 => perm |= S_ISUID | S_ISGID,
                    b't' if who == 0 || who & !S_IRWXO != 0 => perm |= S_ISTXT,
                    b's' | b't' => {}
                    b'w' => perm |= S_IWUSR | S_IWGRP | S_IWOTH,
                    b'X' => perm_x = S_IXUSR | S_IXGRP | S_IXOTH,
                    b'x' => perm |= S_IXUSR | S_IXGRP | S_IXOTH,
                    c @ (b'u' | b'g' | b'o') => {
                        if perm != 0 {
                            addcmd(&mut set, op, who, perm, mask);
                            perm = 0;
                        }
                        if op == b'=' {
                            equalopdone = true;
                        }
                        if op == b'+' && perm_x != 0 {
                            addcmd(&mut set, b'X', who, perm_x, mask);
                            perm_x = 0;
                        }
                        addcmd(&mut set, c, who, u32::from(op), mask);
                    }
                    _ => {
                        if perm != 0 || (op == b'=' && !equalopdone) {
                            if op == b'=' {
                                equalopdone = true;
                            }
                            addcmd(&mut set, op, who, perm, mask);
                        }
                        if perm_x != 0 {
                            addcmd(&mut set, b'X', who, perm_x, mask);
                        }
                        break;
                    }
                }
                i += 1;
            }
            match at(i) {
                0 => break 'clauses,
                b',' => {
                    i += 1;
                    continue 'clauses;
                }
                // Another operator for the same `who`, as in `u+r-w`.
                _ => continue 'ops,
            }
        }
    }
    Some(set)
}

/// The mode `set` makes of `omode`; Apple's `getmode` over a slice.
fn apply(set: &[BitCmd], omode: u32) -> u32 {
    let mut newmode = omode;
    for step in set {
        let bits = u32::from(step.bits);
        match step.cmd {
            b'u' | b'g' | b'o' => {
                let value = match step.cmd {
                    b'u' => (newmode & S_IRWXU) >> 6,
                    b'g' => (newmode & S_IRWXG) >> 3,
                    _ => newmode & S_IRWXO,
                };
                if step.cmd2 & CMD2_CLR != 0 {
                    let clrval = match step.cmd2 & CMD2_SET != 0 {
                        true => S_IRWXO,
                        false => value,
                    };
                    if step.cmd2 & CMD2_UBITS != 0 {
                        newmode &= !((clrval << 6) & bits);
                    }
                    if step.cmd2 & CMD2_GBITS != 0 {
                        newmode &= !((clrval << 3) & bits);
                    }
                    if step.cmd2 & CMD2_OBITS != 0 {
                        newmode &= !(clrval & bits);
                    }
                }
                if step.cmd2 & CMD2_SET != 0 {
                    if step.cmd2 & CMD2_UBITS != 0 {
                        newmode |= (value << 6) & bits;
                    }
                    if step.cmd2 & CMD2_GBITS != 0 {
                        newmode |= (value << 3) & bits;
                    }
                    if step.cmd2 & CMD2_OBITS != 0 {
                        newmode |= value & bits;
                    }
                }
            }
            b'+' => newmode |= bits,
            b'-' => newmode &= !bits,
            b'X' => {
                if omode & (S_IFDIR | S_IXUSR | S_IXGRP | S_IXOTH) != 0 {
                    newmode |= bits;
                }
            }
            _ => break,
        }
    }
    newmode & 0xffff
}

/// `void *setmode(const char *mode_str)`
///
/// The result is an array the caller releases with `free`, so it comes from
/// the allocator the guest frees with - see [`crate::heap`].
///
/// # Safety
///
/// `mode` is a NUL-terminated string.
pub unsafe extern "C" fn setmode(mode: *const c_char) -> *mut c_void {
    if mode.is_null() {
        crate::errno::set(libc::EINVAL);
        return std::ptr::null_mut();
    }
    // Apple reads the umask by setting it and putting it back, with every
    // signal blocked so a handler cannot observe the zero in between.
    // SAFETY: a full set, and the previous mask restored straight after.
    let umask = unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut old);
        let umask = libc::umask(0);
        libc::umask(umask);
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        umask as u32
    };
    // SAFETY: the caller's obligation.
    let text = unsafe { CStr::from_ptr(mode) }.to_bytes();
    let Some(mut set) = compile(text, umask) else {
        return std::ptr::null_mut();
    };
    set.push(BitCmd::default());
    let bytes = set.len() * size_of::<BitCmd>();
    // SAFETY: a block of the size the copy below fills.
    let out = unsafe { crate::heap::malloc(bytes) }.cast::<BitCmd>();
    if out.is_null() {
        crate::errno::set(libc::ENOMEM);
        return std::ptr::null_mut();
    }
    // SAFETY: `out` holds `set.len()` elements and does not overlap `set`.
    unsafe { std::ptr::copy_nonoverlapping(set.as_ptr(), out, set.len()) };
    out.cast()
}

/// `mode_t getmode(const void *set, mode_t mode)`
///
/// # Safety
///
/// `set` is an array [`setmode`] returned.
pub unsafe extern "C" fn getmode(set: *const c_void, mode: c_int) -> c_int {
    let set = set.cast::<BitCmd>();
    let mut len = 0;
    // SAFETY: [`setmode`] terminates its array with a zero command.
    unsafe {
        while (*set.add(len)).cmd != 0 {
            len += 1;
        }
    }
    // SAFETY: `len` elements, all read above.
    let steps = unsafe { std::slice::from_raw_parts(set, len) };
    apply(steps, mode as u32 & 0xffff) as c_int
}

const UF_NODUMP: c_ulong = 0x0000_0001;
const UF_IMMUTABLE: c_ulong = 0x0000_0002;
const UF_APPEND: c_ulong = 0x0000_0004;
const UF_OPAQUE: c_ulong = 0x0000_0008;
const UF_COMPRESSED: c_ulong = 0x0000_0020;
const UF_DATAVAULT: c_ulong = 0x0000_0080;
const UF_HIDDEN: c_ulong = 0x0000_8000;
const SF_ARCHIVED: c_ulong = 0x0001_0000;
const SF_IMMUTABLE: c_ulong = 0x0002_0000;
const SF_APPEND: c_ulong = 0x0004_0000;
const SF_RESTRICTED: c_ulong = 0x0008_0000;
const SF_NOUNLINK: c_ulong = 0x0010_0000;
const SF_DATALESS: c_ulong = 0x4000_0000;

/// Apple's `strtofflags.c` table: the shortest name per flag first, each
/// spelled with the `no` that `fflagstostr` strips unless `invert` is set.
const FLAG_NAMES: &[(&str, c_ulong, bool)] = &[
    ("nosappnd", SF_APPEND, false),
    ("nosappend", SF_APPEND, false),
    ("noarch", SF_ARCHIVED, false),
    ("noarchived", SF_ARCHIVED, false),
    ("noschg", SF_IMMUTABLE, false),
    ("noschange", SF_IMMUTABLE, false),
    ("nosimmutable", SF_IMMUTABLE, false),
    ("norestricted", SF_RESTRICTED, false),
    ("nosunlnk", SF_NOUNLINK, false),
    ("nosunlink", SF_NOUNLINK, false),
    ("nouappnd", UF_APPEND, false),
    ("nouappend", UF_APPEND, false),
    ("nouchg", UF_IMMUTABLE, false),
    ("nouchange", UF_IMMUTABLE, false),
    ("nouimmutable", UF_IMMUTABLE, false),
    ("nodump", UF_NODUMP, true),
    ("noopaque", UF_OPAQUE, false),
    ("nohidden", UF_HIDDEN, false),
    ("nocompressed", UF_COMPRESSED, false),
    ("nodatavault", UF_DATAVAULT, false),
    ("nodataless", SF_DATALESS, false),
];

fn flags_string(mut flags: c_ulong) -> String {
    let mut out = String::new();
    for (name, flag, invert) in FLAG_NAMES {
        if flags & flag != 0 {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(if *invert { name } else { &name[2..] });
            flags &= !flag;
        }
    }
    out
}

/// `char *fflagstostr(u_long flags)`
///
/// The caller frees the result, so it comes from the guest's allocator.
///
/// # Safety
///
/// Takes and touches nothing of the caller's.
pub unsafe extern "C" fn fflagstostr(flags: c_ulong) -> *mut c_char {
    let text = flags_string(flags);
    // SAFETY: room for the text and its terminator.
    let out = unsafe { crate::heap::malloc(text.len() + 1) }.cast::<c_char>();
    if out.is_null() {
        crate::errno::set(libc::ENOMEM);
        return out;
    }
    // SAFETY: `out` holds `len + 1` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr().cast(), out, text.len());
        *out.add(text.len()) = 0;
    }
    out
}

exports::exports! {
    functions {
        "strmode" => strmode,
        "setmode" => setmode,
        "getmode" => getmode,
        "fflagstostr" => fflagstostr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(mode: u32) -> String {
        String::from_utf8(mode_string(mode)[..11].to_vec()).unwrap()
    }

    #[test]
    fn strmode_spells_type_permissions_and_special_bits() {
        assert_eq!(text(S_IFREG | 0o644), "-rw-r--r-- ");
        assert_eq!(text(S_IFDIR | 0o755), "drwxr-xr-x ");
        assert_eq!(text(S_IFLNK | 0o777), "lrwxrwxrwx ");
        assert_eq!(text(S_IFREG | 0o4755), "-rwsr-xr-x ");
        assert_eq!(text(S_IFREG | 0o2644), "-rw-r-Sr-- ");
        assert_eq!(text(S_IFDIR | 0o1777), "drwxrwxrwt ");
        assert_eq!(text(S_IFDIR | 0o1776), "drwxrwxrwT ");
    }

    fn mode(spec: &str, umask: u32, from: u32) -> u32 {
        apply(&compile(spec.as_bytes(), umask).expect(spec), from)
    }

    #[test]
    fn numeric_modes_replace_every_standard_bit() {
        assert_eq!(mode("644", 0o022, 0o777), 0o644);
        assert_eq!(mode("1777", 0o022, 0), 0o1777);
        assert!(compile(b"8", 0).is_none());
        assert!(compile(b"10000", 0).is_none());
    }

    #[test]
    fn symbolic_modes_follow_the_umask_only_without_a_who() {
        assert_eq!(mode("+x", 0o022, 0o644), 0o755);
        assert_eq!(mode("+w", 0o022, 0o444), 0o644);
        assert_eq!(mode("a+w", 0o022, 0o444), 0o666);
        assert_eq!(mode("u=rwx,g=rx,o=", 0, 0o777), 0o750);
        assert_eq!(mode("go-w", 0, 0o666), 0o644);
        assert_eq!(mode("g=u", 0, 0o700), 0o770);
        assert_eq!(mode("u+r-w", 0, 0o200), 0o400);
    }

    #[test]
    fn capital_x_only_adds_execute_where_some_already_is() {
        assert_eq!(mode("a+X", 0, 0o644), 0o644);
        assert_eq!(mode("a+X", 0, 0o744), 0o755);
        assert_eq!(mode("a+X", 0, S_IFDIR | 0o644), S_IFDIR | 0o755);
    }

    #[test]
    fn malformed_modes_are_refused() {
        assert!(compile(b"", 0).is_none());
        assert!(compile(b"u", 0).is_none());
        assert!(compile(b"u+q", 0).is_none());
        assert!(compile(b"u+r,", 0).is_none());
    }

    #[test]
    fn setmode_and_getmode_round_trip_through_the_guest_heap() {
        // SAFETY: a literal mode, and the array handed straight back.
        unsafe {
            let set = setmode(c"u=rw,go=r".as_ptr());
            assert!(!set.is_null());
            assert_eq!(getmode(set, 0o777), 0o644);
            crate::heap::free(set);
        }
    }

    #[test]
    fn fflags_use_the_shortest_name_and_invert_nodump() {
        assert_eq!(flags_string(0), "");
        assert_eq!(flags_string(UF_HIDDEN), "hidden");
        assert_eq!(flags_string(UF_IMMUTABLE | UF_NODUMP), "uchg,nodump");
        assert_eq!(flags_string(SF_APPEND | UF_APPEND), "sappnd,uappnd");
    }
}
