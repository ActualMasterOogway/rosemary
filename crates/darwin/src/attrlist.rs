#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int, c_void};

use crate::stat::Stat;

/// `struct attrlist`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AttrList {
    pub bitmapcount: u16,
    pub reserved: u16,
    pub commonattr: u32,
    pub volattr: u32,
    pub dirattr: u32,
    pub fileattr: u32,
    pub forkattr: u32,
}

const ATTR_BIT_MAP_COUNT: u16 = 5;

const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_CHGTIME: u32 = 0x0000_0800;
const ATTR_CMN_ACCTIME: u32 = 0x0000_1000;
const ATTR_CMN_OWNERID: u32 = 0x0000_8000;
const ATTR_CMN_GRPID: u32 = 0x0001_0000;
const ATTR_CMN_ACCESSMASK: u32 = 0x0002_0000;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

const ATTR_DIR_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_DIR_ENTRYCOUNT: u32 = 0x0000_0002;

const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
const ATTR_FILE_IOBLOCKSIZE: u32 = 0x0000_0008;
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;
const ATTR_FILE_DATAALLOCSIZE: u32 = 0x0000_0400;

const COMMON: u32 = ATTR_CMN_DEVID
    | ATTR_CMN_OBJTYPE
    | ATTR_CMN_MODTIME
    | ATTR_CMN_CHGTIME
    | ATTR_CMN_ACCTIME
    | ATTR_CMN_OWNERID
    | ATTR_CMN_GRPID
    | ATTR_CMN_ACCESSMASK
    | ATTR_CMN_FLAGS
    | ATTR_CMN_FILEID
    | ATTR_CMN_RETURNED_ATTRS;
const DIR: u32 = ATTR_DIR_LINKCOUNT | ATTR_DIR_ENTRYCOUNT;
const FILE: u32 = ATTR_FILE_LINKCOUNT
    | ATTR_FILE_TOTALSIZE
    | ATTR_FILE_ALLOCSIZE
    | ATTR_FILE_IOBLOCKSIZE
    | ATTR_FILE_DATALENGTH
    | ATTR_FILE_DATAALLOCSIZE;

const FSOPT_NOFOLLOW: u64 = 0x1;
const FSOPT_REPORT_FULLSIZE: u64 = 0x4;
const FSOPT_PACK_INVAL_ATTRS: u64 = 0x8;
const OPTIONS: u64 = FSOPT_NOFOLLOW | FSOPT_REPORT_FULLSIZE | FSOPT_PACK_INVAL_ATTRS;

/// `enum vtype`, which `ATTR_CMN_OBJTYPE` reports.
fn object_type(mode: u16) -> libc::mode_t {
    match libc::mode_t::from(mode) & libc::S_IFMT {
        libc::S_IFREG => 1,
        libc::S_IFDIR => 2,
        libc::S_IFBLK => 3,
        libc::S_IFCHR => 4,
        libc::S_IFLNK => 5,
        libc::S_IFSOCK => 6,
        libc::S_IFIFO => 7,
        _ => 0,
    }
}

/// Why a request was refused, before anything was looked up.
fn check(list: &AttrList, options: u64) -> Result<(), c_int> {
    if list.bitmapcount != ATTR_BIT_MAP_COUNT || options & !OPTIONS != 0 {
        return Err(libc::EINVAL);
    }
    let unsupported = [
        ("common", list.commonattr & !COMMON),
        ("volume", list.volattr),
        ("directory", list.dirattr & !DIR),
        ("file", list.fileattr & !FILE),
        ("fork", list.forkattr),
    ];
    if unsupported.iter().all(|(_, bits)| *bits == 0) {
        return Ok(());
    }
    let named: Vec<String> = unsupported
        .iter()
        .filter(|(_, bits)| *bits != 0)
        .map(|(what, bits)| format!("{what} {bits:#x}"))
        .collect();
    eprintln!(
        "rosemary: getattrlist: attributes not implemented: {}",
        named.join(", ")
    );
    Err(libc::ENOTSUP)
}

/// Lay out the answer for an object `st` describes. `entries` is the number
/// of directory entries, asked for only when it is wanted.
fn pack(list: &AttrList, options: u64, st: &Stat, entries: impl FnOnce() -> u32) -> Vec<u8> {
    let is_dir = object_type(st.mode) == 2;
    let pack_invalid = options & FSOPT_PACK_INVAL_ATTRS != 0;
    // Darwin returns directory attributes only for a directory and file
    // attributes only for anything else. The others are left out, or packed as
    // zero with FSOPT_PACK_INVAL_ATTRS so the layout is the one asked for.
    let (dir_valid, file_valid) = match is_dir {
        true => (list.dirattr, 0),
        false => (0, list.fileattr),
    };
    let (dir, file) = match pack_invalid {
        true => (list.dirattr, list.fileattr),
        false => (dir_valid, file_valid),
    };

    let mut out = vec![0u8; 4];
    let common = list.commonattr;
    if common & ATTR_CMN_RETURNED_ATTRS != 0 {
        // `attribute_set_t`: what is actually valid, then its own padding
        // word. The flag itself is not listed as returned.
        for bits in [
            common & !ATTR_CMN_RETURNED_ATTRS,
            0,
            dir_valid,
            file_valid,
            0,
        ] {
            out.extend_from_slice(&bits.to_ne_bytes());
        }
    }
    let time = |out: &mut Vec<u8>, t: crate::stat::Timespec| {
        out.extend_from_slice(&t.sec.to_ne_bytes());
        out.extend_from_slice(&t.nsec.to_ne_bytes());
    };
    let mode_t_ = |out: &mut Vec<u8>, v: libc::mode_t| out.extend_from_slice(&v.to_ne_bytes());
    let u32_ = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_ne_bytes());
    let i64_ = |out: &mut Vec<u8>, v: i64| out.extend_from_slice(&v.to_ne_bytes());
    if common & ATTR_CMN_DEVID != 0 {
        u32_(&mut out, st.dev as u32);
    }
    if common & ATTR_CMN_OBJTYPE != 0 {
        mode_t_(&mut out, object_type(st.mode));
    }
    if common & ATTR_CMN_MODTIME != 0 {
        time(&mut out, st.mtime);
    }
    if common & ATTR_CMN_CHGTIME != 0 {
        time(&mut out, st.ctime);
    }
    if common & ATTR_CMN_ACCTIME != 0 {
        time(&mut out, st.atime);
    }
    if common & ATTR_CMN_OWNERID != 0 {
        u32_(&mut out, st.uid);
    }
    if common & ATTR_CMN_GRPID != 0 {
        u32_(&mut out, st.gid);
    }
    if common & ATTR_CMN_ACCESSMASK != 0 {
        u32_(&mut out, u32::from(st.mode) & 0o7777);
    }
    if common & ATTR_CMN_FLAGS != 0 {
        u32_(&mut out, st.flags);
    }
    if common & ATTR_CMN_FILEID != 0 {
        i64_(&mut out, st.ino as i64);
    }
    if dir & ATTR_DIR_LINKCOUNT != 0 {
        // Hard links *to the directory*, not `st_nlink`: a Linux directory's
        // link count also counts its subdirectories' `..`, and `du` would take
        // every directory with a subdirectory for a hard-linked one. Neither
        // this host nor APFS can hard-link a directory, so the answer is 1.
        u32_(&mut out, u32::from(is_dir));
    }
    if dir & ATTR_DIR_ENTRYCOUNT != 0 {
        u32_(&mut out, if is_dir { entries() } else { 0 });
    }
    let alloc = st.blocks * 512;
    if file & ATTR_FILE_LINKCOUNT != 0 {
        u32_(&mut out, if is_dir { 0 } else { u32::from(st.nlink) });
    }
    if file & ATTR_FILE_TOTALSIZE != 0 {
        i64_(&mut out, if is_dir { 0 } else { st.size });
    }
    if file & ATTR_FILE_ALLOCSIZE != 0 {
        i64_(&mut out, if is_dir { 0 } else { alloc });
    }
    if file & ATTR_FILE_IOBLOCKSIZE != 0 {
        u32_(&mut out, if is_dir { 0 } else { st.blksize as u32 });
    }
    if file & ATTR_FILE_DATALENGTH != 0 {
        i64_(&mut out, if is_dir { 0 } else { st.size });
    }
    if file & ATTR_FILE_DATAALLOCSIZE != 0 {
        i64_(&mut out, if is_dir { 0 } else { alloc });
    }
    let len = out.len() as u32;
    out[..4].copy_from_slice(&len.to_ne_bytes());
    out
}

/// Entries in the directory at host path `path`, not counting `.` and `..`.
fn count_entries(path: &CStr) -> u32 {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
    std::fs::read_dir(path).map_or(0, |d| d.count() as u32)
}

/// Copy `answer` out, truncated to `size`, and report its length as Darwin
/// does: what was written, or the whole with FSOPT_REPORT_FULLSIZE.
///
/// # Safety
///
/// `buf` is writable for `size` bytes.
unsafe fn copy_out(mut answer: Vec<u8>, options: u64, buf: *mut c_void, size: usize) -> c_int {
    let n = answer.len().min(size);
    if options & FSOPT_REPORT_FULLSIZE == 0 && n >= 4 {
        answer[..4].copy_from_slice(&(n as u32).to_ne_bytes());
    }
    // SAFETY: `n <= size`, the caller's obligation.
    unsafe { std::ptr::copy_nonoverlapping(answer.as_ptr(), buf.cast::<u8>(), n) };
    0
}

/// `int getattrlist(const char *, struct attrlist *, void *, size_t, unsigned
/// int options)`. `options` is an `unsigned long` in the header the SDK
/// ships and an `unsigned int` in the man page; only its low bits mean anything.
///
/// # Safety
///
/// `path` is NUL-terminated, `list` is readable, and `buf` is writable for
/// `size` bytes.
pub unsafe extern "C" fn getattrlist(
    path: *const c_char,
    list: *const AttrList,
    buf: *mut c_void,
    size: usize,
    options: u64,
) -> c_int {
    let options = options & u64::from(u32::MAX);
    if path.is_null() || list.is_null() || buf.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let list = unsafe { list.read_unaligned() };
    if let Err(errno) = check(&list, options) {
        crate::errno::set(errno);
        return -1;
    }
    let found = crate::stat::with_fallback(path, |host| {
        // SAFETY: a zeroed `struct stat` for the host to fill, and the path
        // `with_fallback` translated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            match options & FSOPT_NOFOLLOW != 0 {
                true => libc::lstat(host, &mut st),
                false => libc::stat(host, &mut st),
            }
        };
        // SAFETY: `host` is the NUL-terminated path just used.
        let answer = (rc == 0).then(|| {
            let st = crate::stat::from_host(&st);
            pack(&list, options, &st, || {
                count_entries(unsafe { CStr::from_ptr(host) })
            })
        });
        (answer, rc == 0)
    });
    match found {
        Some(answer) => unsafe { copy_out(answer, options, buf, size) },
        None => -1,
    }
}

/// `int fgetattrlist(int, struct attrlist *, void *, size_t, unsigned int)`
///
/// # Safety
///
/// As [`getattrlist`], with `fd` in place of the path.
pub unsafe extern "C" fn fgetattrlist(
    fd: c_int,
    list: *const AttrList,
    buf: *mut c_void,
    size: usize,
    options: u64,
) -> c_int {
    let options = options & u64::from(u32::MAX);
    if list.is_null() || buf.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let list = unsafe { list.read_unaligned() };
    if let Err(errno) = check(&list, options) {
        crate::errno::set(errno);
        return -1;
    }
    // SAFETY: a zeroed `struct stat` for the host to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return -1;
    }
    let st = crate::stat::from_host(&st);
    let answer = pack(&list, options, &st, || {
        count_entries(&std::ffi::CString::new(format!("/proc/self/fd/{fd}")).unwrap_or_default())
    });
    unsafe { copy_out(answer, options, buf, size) }
}

exports::exports! {
    functions {
        "getattrlist" => getattrlist,
        "fgetattrlist" => fgetattrlist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(common: u32, dir: u32, file: u32) -> AttrList {
        AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: common,
            volattr: 0,
            dirattr: dir,
            fileattr: file,
            forkattr: 0,
        }
    }

    fn ask(path: &CStr, l: AttrList, options: u64) -> Result<Vec<u8>, c_int> {
        let mut buf = [0u8; 256];
        // SAFETY: live locals of the sizes passed.
        let rc = unsafe {
            getattrlist(
                path.as_ptr(),
                &l,
                buf.as_mut_ptr().cast(),
                buf.len(),
                options,
            )
        };
        if rc != 0 {
            return Err(crate::errno::get());
        }
        let len = u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
        Ok(buf[..len].to_vec())
    }

    fn word(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// `du`'s question: a directory is never reported as hard-linked, however
    /// many subdirectories give it a host link count above two.
    #[test]
    fn a_directory_has_one_link() {
        let dir = std::env::temp_dir().join(format!("attrlist-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let path = std::ffi::CString::new(dir.to_str().unwrap()).unwrap();
        let b = ask(
            &path,
            list(0, ATTR_DIR_LINKCOUNT | ATTR_DIR_ENTRYCOUNT, 0),
            0,
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(b.len(), 12);
        assert_eq!((word(&b, 4), word(&b, 8)), (1, 1));
    }

    #[test]
    fn attributes_are_packed_in_bit_order() {
        let mut st = Stat::default();
        st.mode = libc::S_IFREG as u16 | 0o644;
        st.nlink = 3;
        st.uid = 501;
        st.size = 1000;
        st.blocks = 8;
        let l = list(
            ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_OWNERID | ATTR_CMN_OBJTYPE,
            ATTR_DIR_LINKCOUNT,
            ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE,
        );
        let b = pack(&l, 0, &st, || unreachable!());
        // Length, returned set, OBJTYPE, OWNERID, FILE_LINKCOUNT, ALLOCSIZE;
        // the directory attribute is left out for a file.
        assert_eq!(b.len(), 4 + 20 + 4 + 4 + 4 + 8);
        assert_eq!(word(&b, 4), ATTR_CMN_OWNERID | ATTR_CMN_OBJTYPE);
        assert_eq!(word(&b, 12), 0);
        assert_eq!(word(&b, 16), ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE);
        assert_eq!((word(&b, 24), word(&b, 28), word(&b, 32)), (1, 501, 3));
        assert_eq!(i64::from_ne_bytes(b[36..44].try_into().unwrap()), 4096);
    }

    #[test]
    fn an_unimplemented_attribute_is_refused() {
        let root = c"/";
        assert_eq!(ask(root, list(0x1, 0, 0), 0), Err(libc::ENOTSUP));
        let mut l = list(ATTR_CMN_OBJTYPE, 0, 0);
        l.volattr = 0x8000_0000;
        assert_eq!(ask(root, l, 0), Err(libc::ENOTSUP));
        l = list(ATTR_CMN_OBJTYPE, 0, 0);
        l.bitmapcount = 4;
        assert_eq!(ask(root, l, 0), Err(libc::EINVAL));
    }
}
