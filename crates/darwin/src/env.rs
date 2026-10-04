use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

/// `char **environ` as the guest sees it: the *variable*, whose address is what
/// is published and what `_NSGetEnviron` answers with.
static ENVIRON: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());

// Serialize our API calls, but keep the published vector authoritative:
// guests can replace it, edit its entries, or lend strings through `putenv`.
// Direct guest edits must not race environment API calls.
static INITIALIZED: Mutex<bool> = Mutex::new(false);

/// Build the block from this process's environment, once.
///
/// Called by every entry point below rather than from a start-up hook: a guest
/// can read `environ` before anything else runs, and a block that is empty
/// until something initialises it is a guest that sees no environment at all.
fn block() -> std::sync::MutexGuard<'static, bool> {
    let mut initialized = INITIALIZED.lock().unwrap_or_else(|e| e.into_inner());
    if *initialized {
        return initialized;
    }
    let mut entries = Vec::new();
    let mut has_path = false;
    for (name, value) in std::env::vars_os() {
        use std::os::unix::ffi::OsStrExt;
        // The hand-off from a parent guest, which is this runtime's and not
        // the guest's; see [`host_spelling`].
        if name.to_str().is_some_and(is_hand_off) {
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
            entries.push(entry.into_raw());
        }
    }
    // A prefixed guest has a `PATH` even when this process has none.
    if !has_path
        && let Some(path) = corrected(std::ffi::OsStr::new("PATH"))
        && let Ok(entry) = CString::new(format!("PATH={path}"))
    {
        entries.push(entry.into_raw());
    }
    publish(&entries);
    *initialized = true;
    initialized
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

/// The corrected variables whose guest value a parent guest hands to a child it
/// execs, each as `ROSEMARY_GUEST_<NAME>` - [`GUEST_PATH_VAR`] is `PATH`'s.
///
/// [`host_spelling`] gives the child runtime this machine's value back, which
/// is right for the runtime and the host libraries in it, and the child then
/// corrects it again for its guest. Without the hand-off what a guest set
/// itself would stop at the `exec`: `su -`'s `HOME=/var/root` and the
/// `SHELL` it names would become the user's again in every child.
const HANDED_OFF: &[&str] = &["PATH", "HOME", "SHELL"];

fn hand_off_var(name: &str) -> String {
    format!("ROSEMARY_GUEST_{name}")
}

/// Whether `name` is one of the hand-offs, which belong to this runtime.
pub fn is_hand_off(name: &str) -> bool {
    name.strip_prefix("ROSEMARY_GUEST_")
        .is_some_and(|rest| HANDED_OFF.contains(&rest))
}

/// What a parent guest handed over for `name`, if it did.
fn handed_off(name: &str) -> Option<String> {
    std::env::var(hand_off_var(name)).ok()
}

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
        "HOME" => Some(handed_off("HOME").unwrap_or_else(|| prefix::ns::home().to_string())),
        // A login shell the prefix has; see `guest_shell`.
        "SHELL" => Some(handed_off("SHELL").unwrap_or_else(|| {
            let host = std::env::var_os("SHELL").unwrap_or_default();
            let guest = guest_shell(host.as_encoded_bytes());
            String::from_utf8_lossy(&guest).into_owned()
        })),
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

/// The guest's spelling of a host account's login shell.
///
/// The host's is a host program - often somewhere a Mac has nothing, like
/// `/run/current-system/sw/bin/zsh` - and a guest that runs it (`su`, `sudo -s`,
/// `login`) would leave the prefix for a shell that is not Darwin's. A Mac's
/// account names `/bin/zsh` or `/bin/bash`, so the same shell in the prefix's
/// `/bin` is the answer, and `/bin/sh` when the prefix has none of that name.
/// Outside a prefix the host's path is the guest's, and is left alone.
pub fn guest_shell(host: &[u8]) -> Vec<u8> {
    if prefix::ns::current().is_none() || host.is_empty() {
        return host.to_vec();
    }
    let name = host.rsplit(|b| *b == b'/').next().unwrap_or(host);
    let candidate = [b"/bin/".as_slice(), name].concat();
    let present = std::str::from_utf8(&candidate)
        .ok()
        .is_some_and(|guest| prefix::ns::host_path(guest).is_file());
    match present {
        true => candidate,
        false => b"/bin/sh".to_vec(),
    }
}

/// Point `environ` at a fresh vector of the current entries.
///
/// The old vector is leaked: a guest that read `environ` into a local still
/// holds it, and C gives no way to tell it not to.
fn publish(entries: &[*mut c_char]) {
    let mut vector = entries.to_vec();
    vector.push(std::ptr::null_mut());
    let leaked = Box::leak(vector.into_boxed_slice());
    ENVIRON.store(leaked.as_mut_ptr(), Ordering::Release);
}

/// Read the live block while the environment lock is held.
fn pointers() -> Vec<*mut c_char> {
    let mut entries = Vec::new();
    let mut at = ENVIRON.load(Ordering::Acquire);
    if !at.is_null() {
        // SAFETY: the published block, including guest replacements, must be
        // null-terminated and live until the next environment mutation.
        unsafe {
            while !(*at).is_null() {
                entries.push(*at);
                at = at.add(1);
            }
        }
    }
    entries
}

/// The address of the guest's `environ` variable.
pub fn environ_slot() -> *mut *mut *mut c_char {
    // Built now if it has not been, so that the address is of a live block.
    drop(block());
    std::ptr::addr_of!(ENVIRON) as *mut *mut *mut c_char
}

/// The slot used by the selected guest libc, including native macOS libc.
pub fn guest_environ_slot() -> *mut *mut *mut c_char {
    #[cfg(target_os = "macos")]
    if !crate::libc_backend::using_builtin() {
        // SAFETY: Darwin's accessor returns a process-lifetime slot.
        return unsafe { libc::_NSGetEnviron() };
    }
    environ_slot()
}

/// The guest's environment as `NAME=VALUE` strings for its initial `envp`.
pub fn entries() -> Vec<CString> {
    let _guard = block();
    pointers()
        .iter()
        // SAFETY: each live entry is a NUL-terminated string.
        .map(|&entry| unsafe { CStr::from_ptr(entry) }.to_owned())
        .collect()
}

/// The index of `name` in `entries`, comparing up to the `=`.
fn position(entries: &[*mut c_char], name: &[u8]) -> Option<usize> {
    entries.iter().position(|&entry| {
        // SAFETY: each live entry is a NUL-terminated string.
        let bytes = unsafe { CStr::from_ptr(entry) }.to_bytes();
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
    // Darwin compares only the part before the first '='.
    let name = name.split(|&byte| byte == b'=').next().unwrap();
    let _guard = block();
    let block = pointers();
    let Some(at) = position(&block, name) else {
        return std::ptr::null_mut();
    };
    // The value, which is the entry past `NAME=` - the pointer C callers expect
    // and are entitled to hold on to.
    // SAFETY: the entry is at least `name.len() + 1` bytes and is live, so
    // the interior pointer remains valid while the entry does.
    unsafe { block[at].add(name.len() + 1) }
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
    let _guard = block();
    let mut block = pointers();
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
    let entry = entry.into_raw();
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
    let _guard = block();
    let mut block = pointers();
    let old_len = block.len();
    // Darwin removes every occurrence, including duplicates in guest blocks.
    while let Some(at) = position(&block, name) {
        block.remove(at);
    }
    if block.len() != old_len {
        publish(&block);
    }
    0
}

/// `int putenv(char *)`
///
/// Darwin retains the caller's buffer; later edits change the environment.
///
/// # Safety
///
/// `entry` is null or NUL-terminated. On success it must remain live while in
/// the environment, and edits must not race environment API calls.
pub unsafe extern "C" fn putenv(entry: *mut c_char) -> c_int {
    if entry.is_null() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let text = unsafe { CStr::from_ptr(entry) }.to_bytes();
    let Some(split) = text.iter().position(|b| *b == b'=') else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    let _guard = block();
    let mut block = pointers();
    match position(&block, &text[..split]) {
        Some(at) => block[at] = entry,
        None => block.push(entry),
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
        if let Ok(text) = std::str::from_utf8(name)
            && HANDED_OFF.contains(&text)
            && prefix::ns::current().is_some()
            && let Some(value) = bytes.get(name.len() + 1..)
            && let Ok(entry) = CString::new([hand_off_var(text).as_bytes(), b"=", value].concat())
        {
            // The guest's own value, for the child to take back; see
            // [`HANDED_OFF`].
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
        "SHELL" => std::env::var("SHELL").ok(),
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
        let entries: Vec<*mut c_char> = vec![
            CString::new("PATHX=wrong").unwrap().into_raw(),
            CString::new("PATH=right").unwrap().into_raw(),
        ];
        assert_eq!(position(&entries, b"PATH"), Some(1));
        assert_eq!(position(&entries, b"PATHX"), Some(0));
        assert_eq!(position(&entries, b"PAT"), None);
        // An entry that is exactly the name with no `=` is not that name.
        let bare: Vec<*mut c_char> = vec![CString::new("PATH").unwrap().into_raw()];
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

    /// Darwin rejects `putenv` without an equals sign and keeps the old value.
    #[test]
    fn putenv_without_an_equals_sign_keeps_the_value() {
        let name = unique("PUT");
        let entry = CString::new(format!("{}=value", name.to_str().unwrap())).expect("no NUL");
        // SAFETY: the entry stays live until `unsetenv` removes it.
        unsafe {
            assert_eq!(putenv(entry.as_ptr().cast_mut()), 0);
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"value");
            assert_eq!(putenv(name.as_ptr().cast_mut()), -1);
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"value");
            assert_eq!(unsetenv(name.as_ptr()), 0);
        }
    }

    #[test]
    fn putenv_buffer_child() {
        if std::env::var("ROSEMARY_ENV_TEST_CHILD").as_deref() != Ok("borrowed") {
            return;
        }
        let name = unique("BORROWED");
        let mut entry = format!("{}=first\0", name.to_str().unwrap()).into_bytes();
        let offset = name.to_bytes().len() + 1;
        // SAFETY: entry remains NUL-terminated and live until removal.
        unsafe {
            assert_eq!(putenv(entry.as_mut_ptr().cast()), 0);
            assert_eq!(getenv(name.as_ptr()), entry.as_mut_ptr().add(offset).cast());
            entry[offset..offset + 5].copy_from_slice(b"later");
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"later");
            let with_equals = CString::new(format!("{}=ignored", name.to_str().unwrap())).unwrap();
            assert_eq!(getenv(with_equals.as_ptr()), getenv(name.as_ptr()));
            assert_eq!(setenv(name.as_ptr(), c"owned".as_ptr(), 1), 0);
            entry[offset..offset + 5].copy_from_slice(b"again");
            assert_eq!(CStr::from_ptr(getenv(name.as_ptr())), c"owned");
            assert_eq!(unsetenv(name.as_ptr()), 0);
        }
    }

    #[test]
    fn putenv_keeps_the_callers_mutable_buffer() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "env::tests::putenv_buffer_child"])
            .env("ROSEMARY_ENV_TEST_CHILD", "borrowed")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn environment_replacement_child() {
        if std::env::var_os("ROSEMARY_ENV_TEST_CHILD").is_none() {
            return;
        }
        let slot = environ_slot();
        let first = CString::new("DUP=first").unwrap();
        let second = CString::new("DUP=second").unwrap();
        let mut replacement = [
            first.as_ptr().cast_mut(),
            second.as_ptr().cast_mut(),
            std::ptr::null_mut(),
        ];
        // SAFETY: this isolated process runs one test; the replacement and its
        // strings outlive all uses of the environment.
        unsafe {
            *slot = replacement.as_mut_ptr();
            assert_eq!(CStr::from_ptr(getenv(c"DUP".as_ptr())), c"first");
            assert_eq!(entries(), [first.clone(), second.clone()]);
            assert_eq!(unsetenv(c"DUP".as_ptr()), 0);
            assert!(getenv(c"DUP".as_ptr()).is_null());
            assert!(entries().is_empty());
            assert!((**slot).is_null());
            assert_eq!(setenv(c"ONLY".as_ptr(), c"one".as_ptr(), 1), 0);
            assert_eq!(entries(), [c"ONLY=one".to_owned()]);
            *slot = std::ptr::null_mut();
            assert!(entries().is_empty());
            assert_eq!(setenv(c"ONLY".as_ptr(), c"two".as_ptr(), 1), 0);
            assert_eq!(entries(), [c"ONLY=two".to_owned()]);
        }
    }

    #[test]
    fn guest_replacement_and_empty_blocks_stay_authoritative() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "env::tests::environment_replacement_child"])
            .env("ROSEMARY_ENV_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
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
