#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

/// `char **environ` as the guest sees it: the *variable*, whose address is what
/// is published and what `_NSGetEnviron` answers with.
static ENVIRON: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());

/// The entries, as `NAME=VALUE`.
///
/// `&'static CStr` rather than a raw pointer so that the block is `Send`: each
/// one is a leaked `CString`, which is exactly as long-lived as that claims.
static BLOCK: Mutex<Vec<&'static CStr>> = Mutex::new(Vec::new());

/// Build the block from this process's environment, once.
///
/// Called by every entry point below rather than from a start-up hook: a guest
/// can read `environ` before anything else runs, and a block that is empty
/// until something initialises it is a guest that sees no environment at all.
fn block() -> std::sync::MutexGuard<'static, Vec<&'static CStr>> {
    let mut block = BLOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !block.is_empty() {
        return block;
    }
    let mut has_path = false;
    for (name, value) in std::env::vars_os() {
        use std::os::unix::ffi::OsStrExt;
        // The hand-off from a parent guest, which is this runtime's and not
        // the guest's; see [`host_spelling`].
        if name == GUEST_PATH_VAR {
            continue;
        }
        has_path |= name == "PATH";
        let value = match corrected(&name) {
            Some(replacement) => std::ffi::OsString::from(replacement),
            None => value,
        };
        let mut entry = name.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        // A variable whose name or value holds a NUL cannot be spelled in a
        // block at all, and is dropped rather than truncated.
        if let Ok(entry) = CString::new(entry) {
            block.push(Box::leak(entry.into_boxed_c_str()));
        }
    }
    // A prefixed guest has a `PATH` even when this process has none.
    if !has_path
        && let Some(path) = corrected(std::ffi::OsStr::new("PATH"))
        && let Ok(entry) = CString::new(format!("PATH={path}"))
    {
        block.push(Box::leak(entry.into_boxed_c_str()));
    }
    publish(&block);
    block
}

/// A Mac's default `PATH`: `/etc/paths`, in its order.
pub const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Where a parent guest leaves its own `PATH` for a child it execs.
///
/// The child is another copy of this runtime, and [`host_spelling`] gives it
/// this machine's `PATH` - host libraries in it may launch host helpers - so
/// the guest's value, which may be one the guest set itself, travels here and
/// becomes the child guest's `PATH` in place of [`DEFAULT_PATH`].
pub const GUEST_PATH_VAR: &str = "ROSEMARY_GUEST_PATH";

/// The `PATH` a prefixed guest starts with: its parent's, if it has one.
fn guest_path(inherited: Option<String>) -> String {
    inherited.unwrap_or_else(|| DEFAULT_PATH.to_string())
}

/// The value this runtime gives `name` instead of the host's, if any.
///
/// `None` in identity mode for every name: with no prefix the guest's paths are
/// this host's, and correcting them would only make them wrong.
fn corrected(name: &std::ffi::OsStr) -> Option<String> {
    prefix::ns::current()?;
    match name.to_str()? {
        // Where the guest's files go, and the reason this module exists.
        "HOME" => Some(prefix::ns::home().to_string()),
        // Darwin's is per-session and ends in a separator, which a program that
        // concatenates rather than joining depends on.
        "TMPDIR" => Some("/tmp/".to_string()),
        // Directories in the guest's namespace, not this machine's.
        "PATH" => Some(guest_path(std::env::var(GUEST_PATH_VAR).ok())),
        // A shell sets this to the directory it started the program in. It is
        // the same directory `getcwd` reports, so it gets the same spelling.
        "PWD" => prefix::ns::to_guest(&std::env::current_dir().ok()?)
            .map(|path| path.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// Point `environ` at a fresh vector of the current entries.
///
/// The old vector is leaked: a guest that read `environ` into a local still
/// holds it, and C gives no way to tell it not to.
fn publish(entries: &[&'static CStr]) {
    let mut vector: Vec<*mut c_char> = entries
        .iter()
        .map(|entry| entry.as_ptr().cast_mut())
        .collect();
    vector.push(std::ptr::null_mut());
    let leaked = Box::leak(vector.into_boxed_slice());
    ENVIRON.store(leaked.as_mut_ptr(), Ordering::Release);
}

/// The address of the guest's `environ` variable.
pub fn environ_slot() -> *mut *mut *mut c_char {
    // Built now if it has not been, so that the address is of a live block.
    drop(block());
    std::ptr::addr_of!(ENVIRON) as *mut *mut *mut c_char
}

/// The guest's environment as `NAME=VALUE` strings: what a new guest's `main`
/// is handed as `envp`. It has to be this block and not the host's, or a
/// program that reads `envp` - bash builds its variables from it - sees a
/// different `PATH` than one that calls `getenv`.
pub fn entries() -> Vec<CString> {
    block().iter().map(|entry| (*entry).to_owned()).collect()
}

/// The index of `name` in `entries`, comparing up to the `=`.
fn position(entries: &[&'static CStr], name: &[u8]) -> Option<usize> {
    entries.iter().position(|entry| {
        let bytes = entry.to_bytes();
        bytes.len() > name.len() && bytes[name.len()] == b'=' && &bytes[..name.len()] == name
    })
}

/// `char *getenv(const char *)`
///
/// # Safety
///
/// `name` is null or a NUL-terminated string.
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    if name.is_empty() || name.contains(&b'=') {
        return std::ptr::null_mut();
    }
    let block = block();
    let Some(at) = position(&block, name) else {
        return std::ptr::null_mut();
    };
    // The value, which is the entry past `NAME=` - the pointer C callers expect
    // and are entitled to hold on to.
    // SAFETY: the entry is at least `name.len() + 1` bytes and is leaked, so
    // the interior pointer is valid for the life of the process.
    unsafe { block[at].as_ptr().add(name.len() + 1).cast_mut() }
}

/// `int setenv(const char *, const char *, int)`
///
/// # Safety
///
/// Both strings are null or NUL-terminated.
pub unsafe extern "C" fn setenv(
    name: *const c_char,
    value: *const c_char,
    overwrite: c_int,
) -> c_int {
    if name.is_null() || value.is_null() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let (name, value) = unsafe { (CStr::from_ptr(name), CStr::from_ptr(value)) };
    let name = name.to_bytes();
    if name.is_empty() || name.contains(&b'=') {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let mut block = block();
    let at = position(&block, name);
    if at.is_some() && overwrite == 0 {
        return 0;
    }
    let mut entry = name.to_vec();
    entry.push(b'=');
    entry.extend_from_slice(value.to_bytes());
    let Ok(entry) = CString::new(entry) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    let entry: &'static CStr = Box::leak(entry.into_boxed_c_str());
    match at {
        Some(at) => block[at] = entry,
        None => block.push(entry),
    }
    publish(&block);
    0
}

/// `int unsetenv(const char *)`
///
/// # Safety
///
/// `name` is null or NUL-terminated.
pub unsafe extern "C" fn unsetenv(name: *const c_char) -> c_int {
    if name.is_null() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    if name.is_empty() || name.contains(&b'=') {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let mut block = block();
    if let Some(at) = position(&block, name) {
        block.remove(at);
        publish(&block);
    }
    0
}

/// `int putenv(char *)`
///
/// The string is *copied*, which is where this parts from Darwin: there the
/// caller's own buffer becomes the entry, and a later write through it changes
/// the environment. Copying is the safe half of that bargain - a guest that
/// frees the buffer cannot make this block dangle - and the behaviour a program
/// relies on is `getenv` seeing what it just put, which is kept.
///
/// # Safety
///
/// `entry` is null or a NUL-terminated `NAME=VALUE` string.
pub unsafe extern "C" fn putenv(entry: *mut c_char) -> c_int {
    if entry.is_null() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let text = unsafe { CStr::from_ptr(entry) }.to_bytes();
    let Some(split) = text.iter().position(|b| *b == b'=') else {
        // No `=` means "remove this name", which is what a Darwin `putenv`
        // does with one.
        // SAFETY: the pointer the caller gave, still NUL-terminated.
        return unsafe { unsetenv(entry) };
    };
    if split == 0 {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let Ok(copy) = CString::new(text) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    let copy: &'static CStr = Box::leak(copy.into_boxed_c_str());
    let mut block = block();
    match position(&block, &text[..split]) {
        Some(at) => block[at] = copy,
        None => block.push(copy),
    }
    publish(&block);
    0
}

/// A guest environment block with this runtime's corrections undone.
///
/// For the one place the two environments meet: a guest that `exec`s another
/// Mach-O is rewritten into `rosemary run ...`, and the child is a *host*
/// process that resolves its own prefix from `HOME` before it builds a guest
/// block of its own. Handing it the guest's `HOME` would have it look for the
/// prefix inside the prefix.
///
/// Variables the guest set itself are carried through untouched, which is the
/// reason this undoes the corrections rather than simply using the host block.
///
/// # Safety
///
/// `envp` is null or a null-terminated array of NUL-terminated strings.
pub unsafe fn host_spelling(envp: *const *const c_char) -> Vec<CString> {
    let mut out = Vec::new();
    if envp.is_null() {
        return out;
    }
    let mut at = envp;
    loop {
        // SAFETY: the array is null-terminated.
        let entry = unsafe { *at };
        if entry.is_null() {
            break;
        }
        // SAFETY: as above, each entry is NUL-terminated.
        let bytes = unsafe { CStr::from_ptr(entry) }.to_bytes();
        let name = match bytes.iter().position(|b| *b == b'=') {
            Some(split) => &bytes[..split],
            None => bytes,
        };
        if name == b"PATH"
            && prefix::ns::current().is_some()
            && let Some(value) = bytes.get(b"PATH=".len()..)
            && let Ok(entry) = CString::new([GUEST_PATH_VAR.as_bytes(), b"=", value].concat())
        {
            // The guest's own `PATH`, for the child to take back; see
            // [`GUEST_PATH_VAR`].
            out.push(entry);
        }
        let restored = std::str::from_utf8(name)
            .ok()
            .filter(|name| corrected(std::ffi::OsStr::new(name)).is_some())
            .and_then(|name| {
                let value = host_value(name)?;
                let mut entry = name.as_bytes().to_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                CString::new(entry).ok()
            });
        if let Some(entry) = restored.or_else(|| CString::new(bytes).ok()) {
            out.push(entry);
        }
        // SAFETY: not past the null, which ends the loop above.
        at = unsafe { at.add(1) };
    }
    out
}

/// This process's own value for a name [`corrected`] replaces.
fn host_value(name: &str) -> Option<String> {
    match name {
        "HOME" => Some(prefix::ns::host_home_directory()),
        "TMPDIR" => std::env::var("TMPDIR").ok(),
        "PATH" => std::env::var("PATH").ok(),
        "PWD" => std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        _ => None,
    }
}

exports::exports! {
    functions {
        "getenv" => getenv,
        "setenv" => setenv,
        "unsetenv" => unsetenv,
        "putenv" => putenv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block is one process-wide object, so each test uses a name of its
    /// own rather than a shared one.
    fn unique(what: &str) -> CString {
        CString::new(format!("ROSEMARY_TEST_{what}_{}", std::process::id())).expect("no NUL")
    }

    #[test]
    fn a_guest_path_is_the_parents_or_a_macs_default() {
        assert_eq!(guest_path(None), DEFAULT_PATH);
        assert_eq!(guest_path(Some("/opt/bin".into())), "/opt/bin");
        assert!(DEFAULT_PATH.split(':').any(|d| d == "/usr/local/bin"));
    }

    /// `PATH` is not `PATHX`. Comparing the prefix alone would have `getenv`
    /// answer about whichever entry happened to come first.
    #[test]
    fn a_name_is_matched_up_to_the_equals() {
        let entries: Vec<&'static CStr> = vec![
            Box::leak(CString::new("PATHX=wrong").unwrap().into_boxed_c_str()),
            Box::leak(CString::new("PATH=right").unwrap().into_boxed_c_str()),
        ];
        assert_eq!(position(&entries, b"PATH"), Some(1));
        assert_eq!(position(&entries, b"PATHX"), Some(0));
        assert_eq!(position(&entries, b"PAT"), None);
        // An entry that is exactly the name with no `=` is not that name.
        let bare: Vec<&'static CStr> =
            vec![Box::leak(CString::new("PATH").unwrap().into_boxed_c_str())];
        assert_eq!(position(&bare, b"PATH"), None);
    }

    #[test]
    fn a_value_survives_being_set_and_read_back() {
        let name = unique("SET");
        // SAFETY: two NUL-terminated strings this test owns.
        unsafe {
            assert!(getenv(name.as_ptr()).is_null(), "not set yet");
            assert_eq!(setenv(name.as_ptr(), c"first".as_ptr(), 1), 0);
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"first");
            // Without `overwrite` the first value stands.
            assert_eq!(setenv(name.as_ptr(), c"second".as_ptr(), 0), 0);
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"first");
            assert_eq!(setenv(name.as_ptr(), c"second".as_ptr(), 1), 0);
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"second");
            assert_eq!(unsetenv(name.as_ptr()), 0);
            assert!(getenv(name.as_ptr()).is_null(), "removed");
        }
    }

    /// The guest's block is this runtime's, so a variable it sets is *not* in
    /// this process's own environment - which is the whole point.
    #[test]
    fn what_the_guest_sets_does_not_reach_this_process() {
        let name = unique("ISOLATED");
        // SAFETY: NUL-terminated strings this test owns.
        unsafe { assert_eq!(setenv(name.as_ptr(), c"guest".as_ptr(), 1), 0) };
        assert!(
            std::env::var_os(name.to_str().expect("utf-8")).is_none(),
            "the host's environment was written too"
        );
        unsafe { unsetenv(name.as_ptr()) };
    }

    /// `putenv` with no `=` removes the name, which is what Darwin's does.
    #[test]
    fn putenv_without_a_value_removes_the_name() {
        let name = unique("PUT");
        let entry = CString::new(format!("{}=value", name.to_str().unwrap())).expect("no NUL");
        // SAFETY: NUL-terminated strings this test owns; `putenv` copies.
        unsafe {
            assert_eq!(putenv(entry.as_ptr().cast_mut()), 0);
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"value");
            assert_eq!(putenv(name.as_ptr().cast_mut()), 0);
            assert!(getenv(name.as_ptr()).is_null());
        }
    }

    /// `environ` is null-terminated and holds what the block holds, so a guest
    /// walking it sees the same variables `getenv` answers.
    #[test]
    fn environ_is_a_null_terminated_view_of_the_block() {
        let name = unique("ENVIRON");
        // SAFETY: NUL-terminated strings this test owns.
        unsafe { assert_eq!(setenv(name.as_ptr(), c"here".as_ptr(), 1), 0) };
        let wanted = CString::new(format!("{}=here", name.to_str().unwrap())).expect("no NUL");
        // SAFETY: the slot holds a null-terminated vector this runtime owns.
        let mut at = unsafe { *environ_slot() };
        let mut found = false;
        // SAFETY: walked only as far as the null that ends it.
        unsafe {
            while !(*at).is_null() {
                found |= CStr::from_ptr(*at) == wanted.as_c_str();
                at = at.add(1);
            }
        }
        assert!(found, "the block's entry is not in `environ`");
        unsafe { unsetenv(name.as_ptr()) };
    }

    /// The child of a rewritten `exec` is another copy of this runtime, and it
    /// resolves its own prefix from `HOME` before it has a guest block. So the
    /// corrections are undone on the way out - and everything the guest set
    /// itself is carried through untouched.
    #[test]
    fn a_rewritten_exec_gets_the_hosts_spelling_back() {
        let entries = [
            CString::new("HOME=/Users/user").unwrap(),
            CString::new("GUEST_ONLY=kept").unwrap(),
        ];
        let mut envp: Vec<*const c_char> = entries.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());
        // SAFETY: a null-terminated array of NUL-terminated strings.
        let restored = unsafe { host_spelling(envp.as_ptr()) };
        let restored: Vec<&str> = restored.iter().filter_map(|e| e.to_str().ok()).collect();
        assert!(
            restored.contains(&"GUEST_ONLY=kept"),
            "a variable the guest set was dropped: {restored:?}"
        );
        let home = restored
            .iter()
            .find(|e| e.starts_with("HOME="))
            .expect("HOME is still there");
        match prefix::ns::current() {
            // Under a prefix it is put back to this host's.
            Some(_) => assert_eq!(*home, format!("HOME={}", prefix::ns::host_home_directory())),
            // With no prefix nothing was corrected, so nothing is undone.
            None => assert_eq!(*home, "HOME=/Users/user"),
        }
    }
}
