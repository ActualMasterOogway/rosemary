//! `struct dirent`, which is not the host's, and the calls that hand one out.
//!
//! Darwin's 64-bit `dirent` leads with a 64-bit inode *and* a 64-bit seek
//! offset, then a record length, a **name length** glibc has no counterpart
//! for, the type byte, and a 1024-byte name:
//!
//! | member | Darwin | this host |
//! |---|---|---|
//! | `d_ino` | 0 | 0 |
//! | `d_seekoff` / `d_off` | 8 | 8 |
//! | `d_reclen` | 16 | 16 |
//! | `d_namlen` | **18** | - |
//! | `d_type` | **20** | 18 |
//! | `d_name` | **21** | 19 |
//!
//! So the name a guest reads out of a glibc entry starts two bytes early, on
//! the type byte and one byte of the record length - a name that is not the
//! file's and is not empty either, which is how this reaches a caller as a
//! wrong answer rather than as a failure.
//!
//! `DIR` itself stays the **host's**: it is opaque on both platforms and the
//! guest never looks inside one, so `opendir`, `dirfd`, `telldir`, `seekdir`
//! and `rewinddir` pass through untouched and only the calls that expose an
//! entry are rewritten. That keeps a handle usable by anything here that has
//! not been thought about, which a wrapper structure would not.
//!
//! `readdir` promises storage that lives until the next call on the same
//! handle, and Darwin keeps it inside the `DIR`. There is nowhere to put it in
//! the host's, so [`ENTRIES`] holds one per open handle and [`closedir`] - the
//! only reason that call is here - drops it.

#![cfg(not(target_os = "macos"))]

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int};
use std::sync::Mutex;

/// Darwin's `struct dirent`, the `_DARWIN_FEATURE_64_BIT_INODE` layout.
///
/// `#[repr(C)]` because the guest reads it by offset; the trailing name is not
/// aligned, which is why the structure is 1048 bytes and not 1053.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dirent {
    pub ino: u64,
    pub seekoff: u64,
    pub reclen: u16,
    pub namlen: u16,
    pub kind: u8,
    pub name: [c_char; 1024],
}

const _: () = {
    assert!(size_of::<Dirent>() == 1048);
    assert!(std::mem::offset_of!(Dirent, namlen) == 18);
    assert!(std::mem::offset_of!(Dirent, kind) == 20);
    assert!(std::mem::offset_of!(Dirent, name) == 21);
};

impl Default for Dirent {
    fn default() -> Self {
        Dirent {
            ino: 0,
            seekoff: 0,
            reclen: size_of::<Dirent>() as u16,
            namlen: 0,
            kind: 0,
            name: [0; 1024],
        }
    }
}

/// The `DT_*` values are the same numbers on both platforms, so the type byte
/// is copied rather than translated. Asserted rather than assumed.
const _: () = {
    assert!(libc::DT_DIR == 4 && libc::DT_REG == 8 && libc::DT_LNK == 10);
};

/// One published entry per open `DIR *`.
///
/// Keyed by the handle rather than held inside it, because the handle is the
/// host's own and has no room. Removed by [`closedir`]; a guest that leaks a
/// handle leaks its entry with it, which is the same leak either way.
static ENTRIES: Mutex<Option<HashMap<usize, Box<Dirent>>>> = Mutex::new(None);

fn entries<T>(with: impl FnOnce(&mut HashMap<usize, Box<Dirent>>) -> T) -> T {
    let mut guard = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    with(guard.get_or_insert_with(HashMap::new))
}

/// A host entry as Darwin's.
///
/// # Safety
///
/// `host` is a live `struct dirent` this process's libc returned.
unsafe fn convert(host: *const libc::dirent) -> Dirent {
    // SAFETY: the caller's guarantee.
    let host = unsafe { &*host };
    let mut out = Dirent {
        ino: host.d_ino,
        seekoff: host.d_off as u64,
        kind: host.d_type,
        ..Dirent::default()
    };
    // SAFETY: `d_name` is NUL-terminated within the entry libc returned.
    let name = unsafe { CStr::from_ptr(host.d_name.as_ptr()) }.to_bytes();
    let len = name.len().min(out.name.len() - 1);
    // SAFETY: `len` is inside both, and the two do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr().cast(), out.name.as_mut_ptr(), len) };
    out.namlen = len as u16;
    // Darwin directory records contain only the used name bytes and are padded
    // to a 4-byte boundary. `sizeof(struct dirent)` is merely the maximum.
    out.reclen = (std::mem::offset_of!(Dirent, name) + len + 1).next_multiple_of(4) as u16;
    out
}

/// Read one entry from `dir`, telling the end of the directory from an error.
///
/// Both are a null return, and `errno` is what separates them - so it is
/// cleared first, which is what the host's own `readdir` documents.
///
/// # Safety
///
/// `dir` is a handle `opendir` returned and has not been closed.
unsafe fn next(dir: *mut libc::DIR) -> Result<Option<Dirent>, c_int> {
    let saved = crate::errno::get();
    crate::errno::set(0);
    // SAFETY: the caller's obligation.
    let host = unsafe { libc::readdir(dir) };
    if host.is_null() {
        return match crate::errno::get() {
            0 => {
                crate::errno::set(saved);
                Ok(None)
            }
            error => Err(error),
        };
    }
    crate::errno::set(saved);
    // SAFETY: non-null, and libc's own entry.
    Ok(Some(unsafe { convert(host) }))
}

/// `struct dirent *readdir(DIR *)`
///
/// # Safety
///
/// `dir` is a handle `opendir` returned and has not been closed.
pub unsafe extern "C" fn readdir(dir: *mut libc::DIR) -> *mut Dirent {
    if dir.is_null() {
        crate::errno::set(libc::EBADF);
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's obligation.
    let Ok(Some(entry)) = (unsafe { next(dir) }) else {
        return std::ptr::null_mut();
    };
    entries(|open| {
        let slot = open
            .entry(dir as usize)
            .or_insert_with(|| Box::new(Dirent::default()));
        **slot = entry;
        (&raw mut **slot).cast()
    })
}

/// `int readdir_r(DIR *, struct dirent *, struct dirent **)`
///
/// Returns an error *number*, so it goes through [`crate::errno::from_host`] -
/// the guest compares it against Darwin's, where 35 is `EAGAIN` and this host
/// spells that 11.
///
/// # Safety
///
/// `dir` is an open handle, and `entry` and `result` are writable.
pub unsafe extern "C" fn readdir_r(
    dir: *mut libc::DIR,
    entry: *mut Dirent,
    result: *mut *mut Dirent,
) -> c_int {
    if result.is_null() {
        return crate::errno::from_host(libc::EINVAL);
    }
    // SAFETY: the caller states it is writable. Cleared first: a caller that
    // ignores the return value and tests `*result` must see the end of the
    // directory rather than what was in its variable before.
    unsafe { *result = std::ptr::null_mut() };
    if dir.is_null() || entry.is_null() {
        return crate::errno::from_host(libc::EINVAL);
    }
    // SAFETY: the caller's obligation.
    match unsafe { next(dir) } {
        Ok(None) => 0,
        Ok(Some(read)) => {
            // SAFETY: the caller states both are writable.
            unsafe {
                *entry = read;
                *result = entry;
            }
            0
        }
        Err(error) => crate::errno::from_host(error),
    }
}

/// `int closedir(DIR *)`
///
/// Here only to drop the entry [`readdir`] published for this handle. The call
/// itself is the host's.
///
/// # Safety
///
/// `dir` is an open handle, and is not used again.
pub unsafe extern "C" fn closedir(dir: *mut libc::DIR) -> c_int {
    if dir.is_null() {
        crate::errno::set(libc::EBADF);
        return -1;
    }
    entries(|open| open.remove(&(dir as usize)));
    // SAFETY: the caller's obligation.
    unsafe { libc::closedir(dir) }
}

/// `DIR *opendir(const char *)`, with the case-insensitive fallback.
///
/// The handle is the host's; see the module note. This entry point exists for
/// the same reason [`crate::stat::stat`] does - a Mac filesystem is
/// case-insensitive and Mac software leans on that without meaning to.
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut libc::DIR {
    if path.is_null() {
        crate::errno::set(libc::EFAULT);
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's obligation.
    let dir = unsafe { libc::opendir(path) };
    if !dir.is_null() || crate::errno::get() != libc::ENOENT {
        return dir;
    }
    // SAFETY: as above.
    let Some(found) = crate::path::insensitive(unsafe { CStr::from_ptr(path) }) else {
        return dir;
    };
    // SAFETY: a NUL-terminated path this crate just built.
    unsafe { libc::opendir(found.as_ptr()) }
}

/// `int scandir(const char *, struct dirent ***, int (*)(const struct dirent *),
/// int (*)(const struct dirent **, const struct dirent **))`
///
/// Not forwarded, for two reasons at once: the host's would call the guest's
/// `select` with a *host* entry, and it would allocate the array out of the
/// host's `malloc` for the guest to `free` - see [`crate::heap`] for what that
/// costs. So the walk is done here, and the array the caller frees comes from
/// the allocator the caller frees with.
///
/// # Safety
///
/// `path` is NUL-terminated, `list` is writable, and the two callbacks are
/// callable from this thread or null.
pub unsafe extern "C" fn scandir(
    path: *const c_char,
    list: *mut *mut *mut Dirent,
    select: Option<unsafe extern "C" fn(*const Dirent) -> c_int>,
    compare: Option<unsafe extern "C" fn(*const *const Dirent, *const *const Dirent) -> c_int>,
) -> c_int {
    if list.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    // SAFETY: the caller's obligation.
    let dir = unsafe { opendir(path) };
    if dir.is_null() {
        return -1;
    }

    let mut kept: Vec<*mut Dirent> = Vec::new();
    // SAFETY: `dir` is open, and each block is handed to the caller below.
    let failure = unsafe {
        loop {
            match next(dir) {
                Ok(None) => break None,
                Err(error) => break Some(error),
                Ok(Some(entry)) => {
                    if let Some(select) = select
                        && select(&raw const entry) == 0
                    {
                        continue;
                    }
                    let block = crate::heap::malloc(size_of::<Dirent>()).cast::<Dirent>();
                    if block.is_null() {
                        break Some(libc::ENOMEM);
                    }
                    block.write(entry);
                    kept.push(block);
                }
            }
        }
    };
    // SAFETY: opened above and not used again.
    unsafe { closedir(dir) };

    if let Some(error) = failure {
        // SAFETY: every block came from `crate::heap::malloc` just above.
        for block in kept {
            unsafe { crate::heap::free(block.cast()) };
        }
        crate::errno::set(error);
        return -1;
    }

    if let Some(compare) = compare {
        // The comparator's argument is a *pointer to* an entry pointer on both
        // platforms, which is why the array is sorted rather than the entries.
        kept.sort_by(|a, b| {
            // SAFETY: the caller's comparator, given two live array slots.
            let order = unsafe { compare(&raw const *a as *const _, &raw const *b as *const _) };
            order.cmp(&0)
        });
    }

    // The array is NULL-terminated as well as counted: Darwin's is, and a
    // caller that walks it looking for the terminator would run off the end.
    // SAFETY: sized for what was kept, and freed by the caller.
    let array = unsafe {
        crate::heap::malloc((kept.len() + 1) * size_of::<*mut Dirent>()).cast::<*mut Dirent>()
    };
    if array.is_null() {
        // SAFETY: as above.
        for block in kept {
            unsafe { crate::heap::free(block.cast()) };
        }
        crate::errno::set(libc::ENOMEM);
        return -1;
    }
    for (i, block) in kept.iter().enumerate() {
        // SAFETY: `array` holds `kept.len() + 1` pointers.
        unsafe { array.add(i).write(*block) };
    }
    // SAFETY: as above.
    unsafe { array.add(kept.len()).write(std::ptr::null_mut()) };
    // SAFETY: the caller states `list` is writable.
    unsafe { *list = array };
    kept.len() as c_int
}

/// `int alphasort(const struct dirent **, const struct dirent **)`
///
/// The default comparator [`scandir`] is usually called with, and it cannot be
/// the host's: that one reads the name at the host's offset.
///
/// # Safety
///
/// Both arguments point at a live entry pointer.
pub unsafe extern "C" fn alphasort(a: *const *const Dirent, b: *const *const Dirent) -> c_int {
    if a.is_null() || b.is_null() {
        return 0;
    }
    // Darwin defines `alphasort` in terms of `strcoll`, not bytewise string
    // order. This matters as soon as the process selects a non-C locale.
    // SAFETY: the caller's obligation, and each `d_name` is NUL-terminated
    // within its entry.
    unsafe { libc::strcoll((**a).name.as_ptr(), (**b).name.as_ptr()) }
}

/// `getdirentries` is **not** here, and stays denied.
///
/// It hands back the kernel's raw directory block for the caller to walk with
/// `d_reclen`, so honouring it means re-packing every entry into Darwin's
/// layout inside a buffer the caller sized for the host's - a different piece
/// of work, and one nothing has needed. The name stays on
/// [`crate::divergent`]'s list, which is what stops the host's being bound.
pub const REFUSED: &[&str] = &["getdirentries"];

exports::exports! {
    functions {
        "opendir" => opendir,
        "readdir" => readdir,
        "readdir_r" => readdir_r,
        "closedir" => closedir,
        "scandir" => scandir,
        "alphasort" => alphasort,
    }
}

/// A path this process can enumerate, as a C string.
#[cfg(test)]
mod tests {
    use super::*;

    /// A path this process can enumerate, as a C string.
    fn c_path(path: &std::path::Path) -> std::ffi::CString {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path with no NUL")
    }

    /// The offsets are the guest's. `d_name` is the one that matters: two bytes
    /// later than the host's, so a name read at the host's offset starts on the
    /// type byte.
    #[test]
    fn the_layout_is_darwins_and_not_this_hosts() {
        assert_eq!(std::mem::offset_of!(Dirent, name), 21);
        assert_eq!(std::mem::offset_of!(libc::dirent, d_name), 19);
        assert_eq!(size_of::<Dirent>(), 1048);
    }

    #[test]
    fn the_record_length_covers_the_used_name_and_four_byte_padding() {
        let mut host: libc::dirent = unsafe { core::mem::zeroed() };
        host.d_name[0] = b'a' as c_char;
        host.d_name[1] = 0;
        // SAFETY: a live host dirent with a terminated name.
        let guest = unsafe { convert(&raw const host) };
        assert_eq!(guest.namlen, 1);
        assert_eq!(guest.reclen, 24);
    }

    /// A directory of three files, named per test so the suite can run in
    /// parallel.
    fn fixture(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("r3-dirent-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        for name in ["beta", "alpha", "gamma"] {
            std::fs::write(dir.join(name), b"x").expect("a file");
        }
        dir
    }

    fn read(entry: *const Dirent) -> (String, u8) {
        // SAFETY: a live entry, whose name is NUL-terminated.
        unsafe {
            (
                CStr::from_ptr((*entry).name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
                (*entry).kind,
            )
        }
    }

    #[test]
    fn readdir_names_every_file_and_frees_its_entry_at_close() {
        let dir = fixture("readdir");
        let path = c_path(&dir);
        // SAFETY: a path this test made, and the handle is closed below.
        unsafe {
            let handle = opendir(path.as_ptr());
            assert!(!handle.is_null());
            let mut seen = Vec::new();
            loop {
                let entry = readdir(handle);
                if entry.is_null() {
                    break;
                }
                let (name, kind) = read(entry);
                assert_eq!((*entry).namlen as usize, name.len());
                if !name.starts_with('.') {
                    assert_eq!(kind, libc::DT_REG, "{name}");
                    seen.push(name);
                }
            }
            seen.sort();
            assert_eq!(seen, ["alpha", "beta", "gamma"]);
            assert!(entries(|open| open.contains_key(&(handle as usize))));
            assert_eq!(closedir(handle), 0);
            assert!(!entries(|open| open.contains_key(&(handle as usize))));
        }
        std::fs::remove_dir_all(&dir).expect("the fixture is removed");
    }

    /// The reentrant form fills the caller's storage, and says "end of
    /// directory" by returning zero with a null `result` rather than an error.
    #[test]
    fn readdir_r_uses_the_callers_storage() {
        let dir = fixture("readdir-r");
        let path = c_path(&dir);
        // SAFETY: as above.
        unsafe {
            let handle = opendir(path.as_ptr());
            assert!(!handle.is_null());
            let mut count = 0;
            loop {
                let mut entry = Dirent::default();
                let mut result = std::ptr::null_mut();
                assert_eq!(readdir_r(handle, &raw mut entry, &raw mut result), 0);
                if result.is_null() {
                    break;
                }
                assert_eq!(result, &raw mut entry);
                count += 1;
            }
            assert!(count >= 3, "three files, and `.` and `..`");
            assert_eq!(closedir(handle), 0);
        }
        std::fs::remove_dir_all(&dir).expect("the fixture is removed");
    }

    #[test]
    fn reaching_the_end_of_a_directory_preserves_errno() {
        let dir = fixture("errno");
        let path = c_path(&dir);
        // SAFETY: a path this test owns and a handle closed below.
        unsafe {
            let handle = opendir(path.as_ptr());
            assert!(!handle.is_null());
            while !readdir(handle).is_null() {}
            crate::errno::set(libc::EINTR);
            assert!(readdir(handle).is_null());
            assert_eq!(crate::errno::get(), libc::EINTR);
            closedir(handle);
        }
        std::fs::remove_dir_all(dir).expect("the fixture is removed");
    }

    /// `scandir` selects, sorts and hands back an array the caller frees.
    #[test]
    fn scandir_selects_and_sorts() {
        unsafe extern "C" fn visible(entry: *const Dirent) -> c_int {
            // SAFETY: a live entry from `scandir`'s own walk.
            let name = unsafe { CStr::from_ptr((*entry).name.as_ptr()) };
            c_int::from(!name.to_bytes().starts_with(b"."))
        }

        let dir = fixture("scandir");
        let path = c_path(&dir);
        let mut list: *mut *mut Dirent = std::ptr::null_mut();
        // SAFETY: a path this test made; the array is freed below.
        unsafe {
            let n = scandir(path.as_ptr(), &raw mut list, Some(visible), Some(alphasort));
            assert_eq!(n, 3);
            assert!(!list.is_null());
            let names: Vec<String> = (0..n as usize).map(|i| read(*list.add(i)).0).collect();
            assert_eq!(names, ["alpha", "beta", "gamma"]);
            assert!((*list.add(n as usize)).is_null(), "NULL-terminated too");
            for i in 0..n as usize {
                crate::heap::free((*list.add(i)).cast());
            }
            crate::heap::free(list.cast());
        }
        std::fs::remove_dir_all(&dir).expect("the fixture is removed");
    }

    #[test]
    fn alphasort_has_strcoll_semantics() {
        let mut a = Dirent::default();
        let mut b = Dirent::default();
        a.name[..2].copy_from_slice(&[b'b' as c_char, 0]);
        b.name[..2].copy_from_slice(&[b'a' as c_char, 0]);
        let (a, b) = (&raw const a, &raw const b);
        // SAFETY: two live entry pointers with terminated names.
        let sorted = unsafe { alphasort(&raw const a, &raw const b) };
        // SAFETY: the same inputs passed directly to the function Darwin
        // specifies as alphasort's comparator.
        let expected = unsafe { libc::strcoll((*a).name.as_ptr(), (*b).name.as_ptr()) };
        assert_eq!(sorted.signum(), expected.signum());
    }

    /// A directory that is not there is a failure the caller can read, not a
    /// zero-length listing.
    #[test]
    fn a_missing_directory_fails_rather_than_reading_empty() {
        let path = c"/nonexistent-rosetta3-dirent-test";
        let mut list: *mut *mut Dirent = std::ptr::null_mut();
        // SAFETY: a static path, and a live out-parameter.
        unsafe {
            assert!(opendir(path.as_ptr()).is_null());
            assert_eq!(scandir(path.as_ptr(), &raw mut list, None, None), -1);
        }
    }
}
