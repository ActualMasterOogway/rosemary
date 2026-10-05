use std::ffi::{CStr, c_char, c_int, c_long, c_void};
use std::ptr::null_mut;

use crate::stat::Stat;

const FTS_COMFOLLOW: c_int = 0x001;
const FTS_LOGICAL: c_int = 0x002;
const FTS_NOCHDIR: c_int = 0x004;
const FTS_NOSTAT: c_int = 0x008;
const FTS_PHYSICAL: c_int = 0x010;
const FTS_SEEDOT: c_int = 0x020;
const FTS_XDEV: c_int = 0x040;
const FTS_COMFOLLOWDIR: c_int = 0x400;
const FTS_NOSTAT_TYPE: c_int = 0x800;
const FTS_OPTIONMASK: c_int = 0xcff;
const FTS_NAMEONLY: c_int = 0x100;
const FTS_STOP: c_int = 0x200;

const FTS_ROOTPARENTLEVEL: i16 = -1;
const FTS_ROOTLEVEL: i16 = 0;
const FTS_MAXLEVEL: i16 = i16::MAX;

const FTS_D: u16 = 1;
const FTS_DC: u16 = 2;
const FTS_DEFAULT: u16 = 3;
const FTS_DNR: u16 = 4;
const FTS_DOT: u16 = 5;
const FTS_DP: u16 = 6;
const FTS_ERR: u16 = 7;
const FTS_F: u16 = 8;
const FTS_INIT: u16 = 9;
const FTS_NS: u16 = 10;
const FTS_NSOK: u16 = 11;
const FTS_SL: u16 = 12;
const FTS_SLNONE: u16 = 13;
const FTS_W: u16 = 14;

const FTS_AGAIN: c_int = 1;
const FTS_FOLLOW: c_int = 2;
const FTS_NOINSTR: c_int = 3;
const FTS_SKIP: c_int = 4;

/// What `fts_build` was called for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Build {
    Child,
    Names,
    Read,
}

/// Which entries `fts_build` stats; Apple's `F_*`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DoStat {
    Never,
    DirsAndLinks,
    Always,
    Dirs,
    DirsByType,
    DirsAndLinksByType,
}

type Compar = unsafe extern "C" fn(*const *const Ftsent, *const *const Ftsent) -> c_int;

/// Darwin's `FTS`. Opaque to a caller in practice, laid out as Darwin's anyway.
#[repr(C)]
pub struct Fts {
    cur: *mut Ftsent,
    child: *mut Ftsent,
    array: *mut *mut Ftsent,
    dev: i32,
    path: *mut c_char,
    rfd: c_int,
    pathlen: c_int,
    nitems: c_int,
    compar: Option<Compar>,
    options: c_int,
}

/// Darwin's `FTSENT`, the `$INODE64` layout.
#[repr(C)]
pub struct Ftsent {
    cycle: *mut Ftsent,
    parent: *mut Ftsent,
    link: *mut Ftsent,
    number: c_long,
    pointer: *mut c_void,
    accpath: *mut c_char,
    path: *mut c_char,
    errno: c_int,
    symfd: c_int,
    pathlen: u16,
    namelen: u16,
    ino: u64,
    dev: i32,
    nlink: u16,
    level: i16,
    info: u16,
    flags: u16,
    instr: u16,
    statp: *mut Stat,
    name: [c_char; 1],
}

const _: () = {
    assert!(std::mem::offset_of!(Ftsent, accpath) == 40);
    assert!(std::mem::offset_of!(Ftsent, pathlen) == 64);
    assert!(std::mem::offset_of!(Ftsent, ino) == 72);
    assert!(std::mem::offset_of!(Ftsent, dev) == 80);
    assert!(std::mem::offset_of!(Ftsent, level) == 86);
    assert!(std::mem::offset_of!(Ftsent, statp) == 96);
    assert!(std::mem::offset_of!(Ftsent, name) == 104);
    assert!(std::mem::offset_of!(Fts, options) == 64);
};

impl Fts {
    fn isset(&self, option: c_int) -> bool {
        self.options & option != 0
    }
}

/// `.` or `..`.
///
/// # Safety
///
/// `name` is NUL-terminated.
unsafe fn is_dot(name: *const c_char) -> bool {
    // SAFETY: the caller's obligation; each read stops at the NUL.
    unsafe {
        *name == b'.' as c_char
            && (*name.add(1) == 0 || (*name.add(1) == b'.' as c_char && *name.add(2) == 0))
    }
}

/// Where a child's name goes in the path buffer: after the parent's path,
/// except that a root of `/` does not become `//name`.
///
/// # Safety
///
/// `p` is a live entry whose `path` is the stream's buffer.
unsafe fn nappend(p: *const Ftsent) -> usize {
    unsafe {
        if (*p).level == FTS_ROOTLEVEL && (*p).pathlen == 1 && *(*p).path == b'/' as c_char {
            0
        } else {
            (*p).pathlen as usize
        }
    }
}

/// `FTS *fts_open(char * const *, int, int (*)(const FTSENT **, const FTSENT **))`
///
/// # Safety
///
/// `argv` is a null-terminated array of NUL-terminated paths, and `compar` is
/// null or callable from this thread.
pub unsafe extern "C" fn fts_open(
    argv: *const *const c_char,
    options: c_int,
    compar: Option<Compar>,
) -> *mut Fts {
    if options & !FTS_OPTIONMASK != 0 || argv.is_null() {
        crate::errno::set(libc::EINVAL);
        return null_mut();
    }
    let mut options = options;
    if options & FTS_NOSTAT_TYPE != 0 {
        options |= FTS_NOSTAT;
    }
    // See the module note: always, and not only for a logical walk.
    options |= FTS_NOCHDIR;

    // SAFETY: a zeroed `FTS` is a valid empty stream, and `calloc` either
    // returns one or null.
    let sp = unsafe { libc::calloc(1, size_of::<Fts>()) }.cast::<Fts>();
    if sp.is_null() {
        return null_mut();
    }
    // SAFETY: just allocated and zeroed.
    unsafe {
        (*sp).compar = compar;
        (*sp).options = options;
        (*sp).rfd = -1;
        open_stream(argv, sp)
    }
}

/// Apple's `__fts_open`.
///
/// # Safety
///
/// As [`fts_open`], with `sp` a fresh stream.
unsafe fn open_stream(mut argv: *const *const c_char, sp: *mut Fts) -> *mut Fts {
    // SAFETY: the caller's obligations throughout.
    unsafe {
        if palloc(sp, maxarglen(argv).max(libc::PATH_MAX as usize)) {
            libc::free(sp.cast());
            return null_mut();
        }
        let parent = alloc(sp, c"".as_ptr(), 0);
        if parent.is_null() {
            libc::free((*sp).path.cast());
            libc::free(sp.cast());
            return null_mut();
        }
        (*parent).level = FTS_ROOTPARENTLEVEL;

        let mut root: *mut Ftsent = null_mut();
        let mut tail: *mut Ftsent = null_mut();
        let mut nitems = 0;
        while !(*argv).is_null() {
            let len = libc::strlen(*argv);
            let p = alloc(sp, *argv, len);
            if p.is_null() {
                lfree(root);
                libc::free(parent.cast());
                libc::free((*sp).path.cast());
                libc::free(sp.cast());
                return null_mut();
            }
            (*p).level = FTS_ROOTLEVEL;
            (*p).parent = parent;
            (*p).accpath = (*p).name.as_mut_ptr();
            let follow = match (*sp).isset(FTS_COMFOLLOWDIR) {
                true => Follow::DirsOnly,
                false if (*sp).isset(FTS_COMFOLLOW) => Follow::Yes,
                false => Follow::No,
            };
            (*p).info = stat_entry(sp, p, follow);
            // Command-line "." and ".." are real directories.
            if (*p).info == FTS_DOT {
                (*p).info = FTS_D;
            }
            if (*sp).compar.is_some() {
                (*p).link = root;
                root = p;
            } else {
                (*p).link = null_mut();
                if root.is_null() {
                    root = p;
                } else {
                    (*tail).link = p;
                }
                tail = p;
            }
            argv = argv.add(1);
            nitems += 1;
        }
        if (*sp).compar.is_some() && nitems > 1 {
            root = sort(sp, root, nitems);
        }

        // A dummy "current" node that fts_read treats as the one before the
        // roots.
        (*sp).cur = alloc(sp, c"".as_ptr(), 0);
        if (*sp).cur.is_null() {
            lfree(root);
            libc::free(parent.cast());
            libc::free((*sp).path.cast());
            libc::free(sp.cast());
            return null_mut();
        }
        (*(*sp).cur).link = root;
        (*(*sp).cur).info = FTS_INIT;
        if nitems == 0 {
            libc::free(parent.cast());
        }
        sp
    }
}

/// Load the stream for the traversal of root `p`.
///
/// # Safety
///
/// `p` is a root entry of `sp`, whose buffer holds its path by construction.
unsafe fn load(sp: *mut Fts, p: *mut Ftsent) {
    unsafe {
        let len = (*p).namelen as usize;
        (*p).pathlen = (*p).namelen;
        std::ptr::copy((*p).name.as_ptr(), (*sp).path, len + 1);
        let name = (*p).name.as_mut_ptr();
        let slash = libc::strrchr(name, b'/' as c_int);
        if !slash.is_null() && (slash != name || *slash.add(1) != 0) {
            let tail = slash.add(1);
            let len = libc::strlen(tail);
            std::ptr::copy(tail, name, len + 1);
            (*p).namelen = len as u16;
        }
        (*p).path = (*sp).path;
        (*p).accpath = (*sp).path;
        (*sp).dev = (*p).dev;
    }
}

/// `int fts_close(FTS *)`
///
/// # Safety
///
/// `sp` is a stream [`fts_open`] returned, not already closed.
pub unsafe extern "C" fn fts_close(sp: *mut Fts) -> c_int {
    if sp.is_null() {
        return 0;
    }
    // SAFETY: the caller's obligation. The walk frees the current node and
    // everything after it on its level, then climbs, which reaches every node
    // still allocated.
    unsafe {
        if !(*sp).cur.is_null() {
            let mut p = (*sp).cur;
            while (*p).level >= FTS_ROOTLEVEL {
                let freep = p;
                p = match (*p).link.is_null() {
                    false => (*p).link,
                    true => (*p).parent,
                };
                libc::free(freep.cast());
            }
            libc::free(p.cast());
        }
        if !(*sp).child.is_null() {
            lfree((*sp).child);
        }
        libc::free((*sp).array.cast());
        libc::free((*sp).path.cast());
        libc::free(sp.cast());
    }
    0
}

/// `FTSENT *fts_read(FTS *)`
///
/// # Safety
///
/// `sp` is a live stream.
pub unsafe extern "C" fn fts_read(sp: *mut Fts) -> *mut Ftsent {
    if sp.is_null() {
        crate::errno::set(libc::EINVAL);
        return null_mut();
    }
    // SAFETY: the caller's obligation, and every pointer followed below is one
    // this stream allocated and still owns.
    unsafe {
        if (*sp).cur.is_null() || (*sp).isset(FTS_STOP) {
            return null_mut();
        }
        let mut p = (*sp).cur;
        let instr = (*p).instr as c_int;
        (*p).instr = FTS_NOINSTR as u16;

        // Any type of file may be re-visited; re-stat and re-turn.
        if instr == FTS_AGAIN {
            (*p).info = stat_entry(sp, p, Follow::No);
            return p;
        }
        if instr == FTS_FOLLOW && ((*p).info == FTS_SL || (*p).info == FTS_SLNONE) {
            (*p).info = stat_entry(sp, p, Follow::Yes);
            return p;
        }

        // Directory in pre-order.
        if (*p).info == FTS_D {
            // Skipped or crossed a mount point: do the post-order visit.
            if instr == FTS_SKIP || ((*sp).isset(FTS_XDEV) && (*p).dev != (*sp).dev) {
                if !(*sp).child.is_null() {
                    lfree((*sp).child);
                    (*sp).child = null_mut();
                }
                (*p).info = FTS_DP;
                return p;
            }
            // Rebuild if only the names were read and now traversing.
            if !(*sp).child.is_null() && (*sp).isset(FTS_NAMEONLY) {
                (*sp).options &= !FTS_NAMEONLY;
                lfree((*sp).child);
                (*sp).child = null_mut();
            }
            if (*sp).child.is_null() {
                (*sp).child = build(sp, Build::Read);
                if (*sp).child.is_null() {
                    if (*sp).isset(FTS_STOP) {
                        return null_mut();
                    }
                    return p;
                }
            }
            p = (*sp).child;
            (*sp).child = null_mut();
            return name(sp, p);
        }

        // Move to the next node on this level.
        loop {
            let tmp = p;
            p = (*p).link;
            if p.is_null() {
                // Move up to the parent node.
                let p = (*tmp).parent;
                libc::free(tmp.cast());
                if (*p).level == FTS_ROOTPARENTLEVEL {
                    // Done. errno is cleared so the caller can tell the end
                    // from an error.
                    libc::free(p.cast());
                    crate::errno::set(0);
                    (*sp).cur = null_mut();
                    return null_mut();
                }
                *(*sp).path.add((*p).pathlen as usize) = 0;
                (*p).info = match (*p).errno {
                    0 => FTS_DP,
                    _ => FTS_ERR,
                };
                (*sp).cur = p;
                return p;
            }
            libc::free(tmp.cast());

            // The top: load the paths for the next root.
            if (*p).level == FTS_ROOTLEVEL {
                load(sp, p);
                (*sp).cur = p;
                return p;
            }
            // The caller may have used fts_set on the node.
            if (*p).instr as c_int == FTS_SKIP {
                continue;
            }
            if (*p).instr as c_int == FTS_FOLLOW {
                (*p).info = stat_entry(sp, p, Follow::Yes);
                (*p).instr = FTS_NOINSTR as u16;
            }
            return name(sp, p);
        }
    }
}

/// Write `p`'s name after its parent's path and make it current.
///
/// # Safety
///
/// `p` is a live non-root entry of `sp`.
unsafe fn name(sp: *mut Fts, p: *mut Ftsent) -> *mut Ftsent {
    // SAFETY: the caller's obligation; `build` sized the buffer for the
    // longest child path.
    unsafe {
        let t = (*sp).path.add(nappend((*p).parent));
        *t = b'/' as c_char;
        std::ptr::copy((*p).name.as_ptr(), t.add(1), (*p).namelen as usize + 1);
        (*sp).cur = p;
        p
    }
}

/// `int fts_set(FTS *, FTSENT *, int)`
///
/// # Safety
///
/// `p` is a live entry of the stream.
pub unsafe extern "C" fn fts_set(_sp: *mut Fts, p: *mut Ftsent, instr: c_int) -> c_int {
    if instr != 0
        && instr != FTS_AGAIN
        && instr != FTS_FOLLOW
        && instr != FTS_NOINSTR
        && instr != FTS_SKIP
    {
        crate::errno::set(libc::EINVAL);
        return 1;
    }
    unsafe { (*p).instr = instr as u16 };
    0
}

/// `FTSENT *fts_children(FTS *, int)`
///
/// # Safety
///
/// `sp` is a live stream.
pub unsafe extern "C" fn fts_children(sp: *mut Fts, instr: c_int) -> *mut Ftsent {
    if instr != 0 && instr != FTS_NAMEONLY {
        crate::errno::set(libc::EINVAL);
        return null_mut();
    }
    unsafe {
        let p = (*sp).cur;
        // Cleared so the caller can tell an empty directory from an error.
        crate::errno::set(0);
        if (*sp).isset(FTS_STOP) {
            return null_mut();
        }
        // The logical hierarchy of the caller's arguments.
        if (*p).info == FTS_INIT {
            return (*p).link;
        }
        if (*p).info != FTS_D {
            return null_mut();
        }
        if !(*sp).child.is_null() {
            lfree((*sp).child);
        }
        let kind = match instr == FTS_NAMEONLY {
            true => {
                (*sp).options |= FTS_NAMEONLY;
                Build::Names
            }
            false => Build::Child,
        };
        (*sp).child = build(sp, kind);
        (*sp).child
    }
}

/// Read the current directory into a list of entries.
///
/// # Safety
///
/// `sp` is a live stream whose current node is a directory.
unsafe fn build(sp: *mut Fts, kind: Build) -> *mut Ftsent {
    // SAFETY: the caller's obligation, and every entry below is freshly
    // allocated by this stream.
    unsafe {
        let cur = (*sp).cur;
        let dir = crate::dirent::opendir((*cur).accpath);
        if dir.is_null() {
            if kind == Build::Read {
                (*cur).info = FTS_DNR;
                (*cur).errno = crate::errno::get();
            }
            return null_mut();
        }

        let dostat = if kind == Build::Names {
            DoStat::Never
        } else if (*sp).isset(FTS_NOSTAT_TYPE) {
            match (*sp).isset(FTS_PHYSICAL) {
                true => DoStat::DirsByType,
                false => DoStat::DirsAndLinksByType,
            }
        } else if (*sp).isset(FTS_NOSTAT) {
            match (*sp).isset(FTS_PHYSICAL) {
                true => DoStat::Dirs,
                false => DoStat::DirsAndLinks,
            }
        } else {
            DoStat::Always
        };

        // Each name is appended after the parent's path and a slash.
        let mut len = nappend(cur);
        let mut cp = (*sp).path.add(len);
        *cp = b'/' as c_char;
        cp = cp.add(1);
        len += 1;
        let mut maxlen = (*sp).pathlen as usize - len;

        let level = match (*cur).level < FTS_MAXLEVEL {
            true => (*cur).level + 1,
            false => (*cur).level,
        };

        let mut doadjust = false;
        let mut head: *mut Ftsent = null_mut();
        let mut tail: *mut Ftsent = null_mut();
        let mut nitems = 0;
        // The guest's view of the directory, mount points included; an error
        // ends the listing as the end of the directory does.
        while let Ok(Some(entry)) = crate::dirent::next_listed(dir) {
            let dname = entry.name.as_ptr();
            if !(*sp).isset(FTS_SEEDOT) && is_dot(dname) {
                continue;
            }
            let namlen = libc::strlen(dname);
            let p = alloc(sp, dname, namlen);
            // Out of memory or path: give up on the whole stream.
            macro_rules! fail {
                ($errno:expr) => {{
                    let errno = $errno;
                    libc::free(p.cast());
                    lfree(head);
                    crate::dirent::closedir(dir);
                    (*cur).info = FTS_ERR;
                    (*sp).options |= FTS_STOP;
                    crate::errno::set(errno);
                    return null_mut();
                }};
            }
            if p.is_null() {
                fail!(crate::errno::get());
            }
            if namlen >= maxlen {
                let oldaddr = (*sp).path;
                if palloc(sp, namlen + len + 1) {
                    fail!(crate::errno::get());
                }
                if oldaddr != (*sp).path {
                    doadjust = true;
                    cp = (*sp).path.add(len);
                }
                maxlen = (*sp).pathlen as usize - len;
            }
            if len + namlen >= u16::MAX as usize {
                fail!(libc::ENAMETOOLONG);
            }
            (*p).level = level;
            (*p).parent = (*sp).cur;
            (*p).pathlen = (len + namlen) as u16;

            let d_type = entry.kind;
            let stat_it = match dostat {
                DoStat::Never => None,
                DoStat::Always => Some(true),
                DoStat::Dirs => matches!(d_type, libc::DT_UNKNOWN | libc::DT_DIR).then_some(true),
                DoStat::DirsAndLinks => {
                    matches!(d_type, libc::DT_UNKNOWN | libc::DT_DIR | libc::DT_LNK).then_some(true)
                }
                DoStat::DirsByType | DoStat::DirsAndLinksByType => {
                    let links = dostat == DoStat::DirsAndLinksByType;
                    match d_type {
                        libc::DT_UNKNOWN | libc::DT_DIR => Some(true),
                        libc::DT_LNK if links => Some(true),
                        libc::DT_LNK => {
                            (*p).info = FTS_SL;
                            Some(false)
                        }
                        libc::DT_FIFO | libc::DT_CHR | libc::DT_BLK | libc::DT_SOCK => {
                            (*p).info = FTS_DEFAULT;
                            Some(false)
                        }
                        libc::DT_REG => {
                            (*p).info = FTS_F;
                            Some(false)
                        }
                        // DT_WHT, which no host filesystem here reports.
                        14 => {
                            (*p).info = FTS_W;
                            Some(false)
                        }
                        _ => None,
                    }
                }
            };
            (*p).accpath = (*p).path;
            match stat_it {
                Some(true) => {
                    std::ptr::copy(dname, cp, namlen + 1);
                    (*p).info = stat_entry(sp, p, Follow::No);
                }
                Some(false) => {}
                None => (*p).info = FTS_NSOK,
            }

            // Directory order, so "ls -f" is not upset.
            (*p).link = null_mut();
            if head.is_null() {
                head = p;
            } else {
                (*tail).link = p;
            }
            tail = p;
            nitems += 1;
        }
        crate::dirent::closedir(dir);

        if doadjust {
            padjust(sp, head);
        }
        // Put the path back the way it was.
        if len == (*sp).pathlen as usize || nitems == 0 {
            cp = cp.sub(1);
        }
        *cp = 0;

        if nitems == 0 {
            if kind == Build::Read {
                (*cur).info = FTS_DP;
            }
            return null_mut();
        }
        if (*sp).compar.is_some() && nitems > 1 {
            head = sort(sp, head, nitems);
        }
        head
    }
}

/// How [`stat_entry`] treats a symbolic link.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    No,
    Yes,
    /// `FTS_COMFOLLOWDIR`: follow, but keep the link's own answer unless it
    /// leads to a directory.
    DirsOnly,
}

/// Apple's `fts_stat`, which classifies an entry and fills its `fts_statp`.
///
/// # Safety
///
/// `p` is a live entry of `sp` whose `accpath` names it.
unsafe fn stat_entry(sp: *mut Fts, p: *mut Ftsent, follow: Follow) -> u16 {
    unsafe {
        let mut scratch = Stat::default();
        let sbp: *mut Stat = match (*sp).isset(FTS_NOSTAT) {
            true => &mut scratch,
            false => (*p).statp,
        };
        let path = (*p).accpath;
        let failed = |sbp: *mut Stat| {
            (*p).errno = crate::errno::get();
            *sbp = Stat::default();
            FTS_NS
        };

        if (*sp).isset(FTS_LOGICAL) || follow != Follow::No {
            if crate::stat::at_path(path, sbp, false) != 0 {
                let saved = crate::errno::get();
                if crate::stat::at_path(path, sbp, true) == 0 {
                    if saved == libc::ELOOP {
                        (*p).errno = libc::ELOOP;
                    }
                    crate::errno::set(0);
                    return FTS_SLNONE;
                }
                (*p).errno = saved;
                *sbp = Stat::default();
                return FTS_NS;
            }
            if follow == Follow::DirsOnly
                && libc::mode_t::from((*sbp).mode) & libc::S_IFMT != libc::S_IFDIR
                && crate::stat::at_path(path, sbp, true) != 0
            {
                return failed(sbp);
            }
        } else if crate::stat::at_path(path, sbp, true) != 0 {
            return failed(sbp);
        }

        match libc::mode_t::from((*sbp).mode) & libc::S_IFMT {
            libc::S_IFDIR => {
                (*p).dev = (*sbp).dev;
                (*p).ino = (*sbp).ino;
                (*p).nlink = (*sbp).nlink;
                if is_dot((*p).name.as_ptr()) {
                    return FTS_DOT;
                }
                let mut t = (*p).parent;
                while (*t).level >= FTS_ROOTLEVEL {
                    if (*sbp).ino == (*t).ino && (*sbp).dev == (*t).dev {
                        (*p).cycle = t;
                        return FTS_DC;
                    }
                    t = (*t).parent;
                }
                FTS_D
            }
            libc::S_IFLNK => FTS_SL,
            libc::S_IFREG => FTS_F,
            _ => FTS_DEFAULT,
        }
    }
}

/// Sort a list of `nitems` entries with the stream's comparison.
///
/// # Safety
///
/// `head` is a list of `nitems` live entries and the stream has a comparison.
unsafe fn sort(sp: *mut Fts, head: *mut Ftsent, nitems: usize) -> *mut Ftsent {
    unsafe {
        if nitems > (*sp).nitems as usize {
            let wanted = nitems + 40;
            let grown = libc::realloc((*sp).array.cast(), wanted * size_of::<*mut Ftsent>())
                .cast::<*mut Ftsent>();
            if grown.is_null() {
                libc::free((*sp).array.cast());
                (*sp).array = null_mut();
                (*sp).nitems = 0;
                return head;
            }
            (*sp).array = grown;
            (*sp).nitems = wanted as c_int;
        }
        let array = (*sp).array;
        let mut p = head;
        let mut i = 0;
        while !p.is_null() {
            *array.add(i) = p;
            i += 1;
            p = (*p).link;
        }
        let Some(compar) = (*sp).compar else {
            return head;
        };
        // SAFETY: `qsort` hands the comparison two pointers into the array,
        // which is exactly the `const FTSENT **` pair it is declared to take.
        let compar: unsafe extern "C" fn(*const c_void, *const c_void) -> c_int =
            std::mem::transmute(compar);
        libc::qsort(array.cast(), nitems, size_of::<*mut Ftsent>(), Some(compar));
        for i in 0..nitems - 1 {
            (**array.add(i)).link = *array.add(i + 1);
        }
        (**array.add(nitems - 1)).link = null_mut();
        *array
    }
}

/// A new entry named `name`, with room for its `struct stat` behind it.
///
/// # Safety
///
/// `name` is readable for `namelen` bytes.
unsafe fn alloc(sp: *mut Fts, name: *const c_char, namelen: usize) -> *mut Ftsent {
    let stat_at = (size_of::<Ftsent>() + namelen + 1).next_multiple_of(align_of::<Stat>());
    // SAFETY: the caller's obligation, and a size that covers the entry, its
    // name, its terminator and an aligned `struct stat`.
    unsafe {
        let nostat = (*sp).isset(FTS_NOSTAT);
        let size = match nostat {
            true => size_of::<Ftsent>() + namelen + 1,
            false => stat_at + size_of::<Stat>(),
        };
        let p = libc::calloc(1, size).cast::<Ftsent>();
        if p.is_null() {
            return null_mut();
        }
        (*p).path = (*sp).path;
        (*p).namelen = namelen as u16;
        (*p).instr = FTS_NOINSTR as u16;
        if !nostat {
            (*p).statp = p.cast::<u8>().add(stat_at).cast();
        }
        std::ptr::copy_nonoverlapping(name, (*p).name.as_mut_ptr(), namelen);
        p
    }
}

/// Free a list of entries.
///
/// # Safety
///
/// `head` is null or a list this stream allocated and no longer references.
unsafe fn lfree(mut head: *mut Ftsent) {
    while !head.is_null() {
        unsafe {
            let next = (*head).link;
            libc::free(head.cast());
            head = next;
        }
    }
}

/// Grow the path buffer by at least `more`. `true` on failure, as Apple's.
///
/// # Safety
///
/// `sp` is a live stream.
unsafe fn palloc(sp: *mut Fts, more: usize) -> bool {
    unsafe {
        let wanted = (*sp).pathlen as usize + more + 256;
        if wanted >= u16::MAX as usize {
            libc::free((*sp).path.cast());
            (*sp).path = null_mut();
            crate::errno::set(libc::ENAMETOOLONG);
            return true;
        }
        let grown = libc::realloc((*sp).path.cast(), wanted).cast::<c_char>();
        if grown.is_null() {
            libc::free((*sp).path.cast());
            (*sp).path = null_mut();
            return true;
        }
        (*sp).path = grown;
        (*sp).pathlen = wanted as c_int;
        false
    }
}

/// Point every live entry at the path buffer's new address.
///
/// # Safety
///
/// `head` is the list being built, and the rest of the tree hangs off it.
unsafe fn padjust(sp: *mut Fts, head: *mut Ftsent) {
    unsafe {
        let addr = (*sp).path;
        let adjust = |p: *mut Ftsent| {
            if (*p).accpath != (*p).name.as_mut_ptr() {
                (*p).accpath = addr.offset((*p).accpath.offset_from((*p).path));
            }
            (*p).path = addr;
        };
        let mut p = (*sp).child;
        while !p.is_null() {
            adjust(p);
            p = (*p).link;
        }
        let mut p = head;
        while !p.is_null() && (*p).level >= FTS_ROOTLEVEL {
            adjust(p);
            p = match (*p).link.is_null() {
                false => (*p).link,
                true => (*p).parent,
            };
        }
    }
}

/// The longest argument plus its terminator.
///
/// # Safety
///
/// `argv` is a null-terminated array of NUL-terminated strings.
unsafe fn maxarglen(mut argv: *const *const c_char) -> usize {
    let mut max = 0;
    unsafe {
        while !(*argv).is_null() {
            max = max.max(CStr::from_ptr(*argv).to_bytes().len());
            argv = argv.add(1);
        }
    }
    max + 1
}

exports::exports! {
    functions {
        "fts_open" | "fts_open$INODE64" => fts_open,
        "fts_read" | "fts_read$INODE64" => fts_read,
        "fts_children" | "fts_children$INODE64" => fts_children,
        "fts_set" | "fts_set$INODE64" => fts_set,
        "fts_close" | "fts_close$INODE64" => fts_close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// A small tree of `test`'s own, so tests running in parallel do not share one.
    fn tree(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("darwin-fts-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        std::fs::write(dir.join("sub/a.txt"), b"aa").unwrap();
        std::os::unix::fs::symlink("b.txt", dir.join("link")).unwrap();
        dir
    }

    unsafe extern "C" fn by_name(a: *const *const Ftsent, b: *const *const Ftsent) -> c_int {
        // SAFETY: qsort's two array slots, each a live entry.
        unsafe { libc::strcmp((**a).name.as_ptr(), (**b).name.as_ptr()) }
    }

    /// Pre-order, children in comparison order, post-order for directories -
    /// and each entry's path is the whole path while it is current.
    #[test]
    fn walks_a_tree_in_the_order_apples_does() {
        let dir = tree("walk");
        let root = CString::new(dir.to_str().unwrap()).unwrap();
        let argv = [root.as_ptr(), std::ptr::null()];
        let mut seen = Vec::new();
        // SAFETY: a null-terminated argv of live strings, and a comparison of
        // this test's own.
        unsafe {
            let sp = fts_open(argv.as_ptr(), FTS_PHYSICAL, Some(by_name));
            assert!(!sp.is_null());
            loop {
                let p = fts_read(sp);
                if p.is_null() {
                    break;
                }
                let path = CStr::from_ptr((*p).path).to_str().unwrap();
                let rel = path
                    .strip_prefix(dir.to_str().unwrap())
                    .unwrap()
                    .to_string();
                seen.push((rel, (*p).info, (*p).level));
            }
            assert_eq!(crate::errno::get(), 0, "the end is not an error");
            assert_eq!(fts_close(sp), 0);
        }
        let expected = [
            ("", FTS_D, 0),
            ("/b.txt", FTS_F, 1),
            ("/link", FTS_SL, 1),
            ("/sub", FTS_D, 1),
            ("/sub/a.txt", FTS_F, 2),
            ("/sub", FTS_DP, 1),
            ("", FTS_DP, 0),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(p, i, l)| (p.to_string(), *i, *l))
            .collect();
        assert_eq!(seen, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ls` without `-R`: read the root, take its children, and skip it.
    #[test]
    fn children_carry_a_darwin_stat_and_skip_prevents_descent() {
        let dir = tree("children");
        let root = CString::new(dir.to_str().unwrap()).unwrap();
        let argv = [root.as_ptr(), std::ptr::null()];
        unsafe {
            let sp = fts_open(argv.as_ptr(), FTS_PHYSICAL, Some(by_name));
            let top = fts_read(sp);
            assert_eq!((*top).info, FTS_D);
            let mut child = fts_children(sp, 0);
            let mut names = Vec::new();
            while !child.is_null() {
                let name = CStr::from_ptr((*child).name.as_ptr()).to_str().unwrap();
                if name == "b.txt" {
                    assert_eq!((*(*child).statp).size, 1);
                    assert_eq!((*child).info, FTS_F);
                }
                names.push(name.to_string());
                child = (*child).link;
            }
            assert_eq!(names, ["b.txt", "link", "sub"]);
            assert_eq!(fts_set(sp, top, FTS_SKIP), 0);
            let after = fts_read(sp);
            assert_eq!((*after).info, FTS_DP);
            assert!(fts_read(sp).is_null());
            fts_close(sp);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Children of `/` are named `/bin`, not `//bin`, while they are current.
    #[test]
    fn a_root_of_slash_does_not_double_its_separator() {
        let argv = [c"/".as_ptr(), std::ptr::null()];
        // SAFETY: as above, without a comparison.
        unsafe {
            let sp = fts_open(argv.as_ptr(), FTS_PHYSICAL | FTS_NOSTAT, None);
            let top = fts_read(sp);
            assert_eq!((*top).info, FTS_D);
            let child = fts_read(sp);
            assert!(!child.is_null());
            let path = CStr::from_ptr((*child).path).to_bytes();
            assert!(
                path.starts_with(b"/") && !path.starts_with(b"//"),
                "{path:?}"
            );
            fts_close(sp);
        }
    }

    #[test]
    fn unknown_options_are_refused() {
        let argv = [c"/".as_ptr(), std::ptr::null()];
        // SAFETY: a valid argv; the call must refuse before reading it.
        let sp = unsafe { fts_open(argv.as_ptr(), 0x1000, None) };
        assert!(sp.is_null());
        assert_eq!(crate::errno::get(), libc::EINVAL);
    }
}
