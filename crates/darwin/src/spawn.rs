//! Launching another program: `execve`, `posix_spawn`, and what they mean when the
//! program being launched is a Mach-O.
//!
//! A Darwin application is often several programs. It re-launches itself to install an
//! update, spawns a crash reporter it expects to outlive it, spawns a helper for the
//! menu bar. Every one of those is a Mach-O, and this host's kernel cannot execute a
//! Mach-O: the call fails with `ENOEXEC`, or - worse on a host that has an ELF of the
//! same name - runs something else entirely.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use crate::signal::signal_to_host;

/// Darwin's `POSIX_SPAWN_*`. The first four agree with Linux; the last three do not,
/// and two of them *collide* with a different Linux flag of the same value - Darwin's
/// `SETEXEC` is Linux's `USEVFORK`, and `START_SUSPENDED` is `SETSID`. Passing the
/// word through unchanged would silently ask for the wrong thing.
/// The whole set is written out even though the last two are not acted on, because
/// the set is what makes the collisions visible: a name missing here is a bit that
/// would be forwarded by accident.
#[allow(dead_code)]
mod flag {
    pub const RESETIDS: u32 = 0x01;
    pub const SETPGROUP: u32 = 0x02;
    pub const SETSIGDEF: u32 = 0x04;
    pub const SETSIGMASK: u32 = 0x08;
    /// Replace the calling process rather than making a child.
    pub const SETEXEC: u32 = 0x40;
    /// Start the child stopped, for a debugger to attach to. Not honoured: nothing
    /// here is being debugged through a guest's spawn, and starting a crash reporter
    /// suspended would hang the program waiting for it.
    pub const START_SUSPENDED: u32 = 0x80;
    /// Close every descriptor across the exec except those the file actions name.
    /// Not honoured: the effect is to leak descriptors into the child that Darwin
    /// would have closed, which is visible only to a child that goes looking.
    pub const CLOEXEC_DEFAULT: u32 = 0x4000;
}

/// One entry of a `posix_spawn_file_actions_t`, in the order it was added.
enum Action {
    Open {
        fd: c_int,
        path: CString,
        oflag: c_int,
        mode: libc::mode_t,
    },
    Dup2 {
        from: c_int,
        to: c_int,
    },
    Close(c_int),
    Chdir(CString),
    Fchdir(c_int),
    /// `addinherit_np`: keep this descriptor open across the spawn. The host's
    /// equivalent is simply not marking it close-on-exec, which a `dup2` onto itself
    /// achieves - see [`FileActions::to_host`].
    Inherit(c_int),
}

#[derive(Default)]
struct FileActions {
    actions: Vec<Action>,
}

#[derive(Default)]
struct Attributes {
    flags: u32,
    pgroup: libc::pid_t,
    /// Darwin's `sigset_t` is one 32-bit word, so both of these are held as the
    /// guest's word and widened only when a host object is built.
    sigdefault: u32,
    sigmask: u32,
}

/// Read the pointer a guest's `posix_spawn*_t` holds.
///
/// Darwin's type is `void *`, which is what makes this a plain load: the guest's eight
/// bytes hold whichever object `..._init` allocated, and never anything else.
///
/// # Safety
///
/// `at` is null or a readable `void *`.
unsafe fn object<T>(at: *mut c_void) -> Option<&'static mut T> {
    if at.is_null() {
        return None;
    }
    // SAFETY: the caller's guarantee.
    let stored = unsafe { *(at as *const u64) };
    if stored == 0 {
        return None;
    }
    // SAFETY: the pointer this module's `..._init` stored, still owned by the guest's
    // variable and not yet destroyed.
    Some(unsafe { &mut *(stored as *mut T) })
}

/// Allocate `T` and store it in the guest's `void *`.
///
/// # Safety
///
/// `at` is a writable `void *`.
unsafe fn init<T: Default>(at: *mut c_void) -> c_int {
    if at.is_null() {
        return libc::EINVAL;
    }
    let boxed = Box::into_raw(Box::new(T::default())) as u64;
    // SAFETY: the caller's guarantee.
    unsafe { *(at as *mut u64) = boxed };
    0
}

/// Free what [`init`] allocated and leave the guest's word null.
///
/// # Safety
///
/// As [`init`], and the object is this module's own.
unsafe fn destroy<T>(at: *mut c_void) -> c_int {
    if at.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let stored = unsafe { *(at as *const u64) };
    if stored == 0 {
        return libc::EINVAL;
    }
    // SAFETY: this module allocated it, and the store below stops a second free.
    drop(unsafe { Box::from_raw(stored as *mut T) });
    // SAFETY: the caller's guarantee.
    unsafe { *(at as *mut u64) = 0 };
    0
}

// ---------------------------------------------------------------------------
// Is this a Mach-O?
// ---------------------------------------------------------------------------

/// Whether `path` names a Mach-O - thin or fat, either byte order.
///
/// Read from the file rather than inferred from the name: the whole point is to tell a
/// Mach-O helper from the host's own `/bin/sh`, and only the magic says which is which.
fn is_mach_o(path: &CStr) -> bool {
    let Ok(text) = path.to_str() else {
        return false;
    };
    let Ok(mut file) = std::fs::File::open(text) else {
        return false;
    };
    use std::io::Read;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        u32::from_le_bytes(magic),
        // 64- and 32-bit thin images, and the same two byte-swapped.
        0xfeed_facf | 0xfeed_face | 0xcffa_edfe | 0xcefa_edfe
        // A fat archive, whose header is big-endian by definition.
        | 0xcafe_babe | 0xbebafeca | 0xcafe_babf | 0xbfba_feca
    )
}

/// This runtime's own executable, which is what a Mach-O spawn is re-pointed at.
fn runtime_path() -> Option<CString> {
    let exe = std::fs::read_link("/proc/self/exe")
        .ok()
        .or_else(|| std::env::current_exe().ok())?;
    CString::new(exe.into_os_string().into_encoded_bytes()).ok()
}

/// A NUL-terminated argument vector as a `Vec`, keeping the guest's strings.
///
/// # Safety
///
/// `argv` is null or a null-terminated array of C strings.
unsafe fn argv_of(argv: *const *const c_char) -> Vec<CString> {
    let mut out = Vec::new();
    if argv.is_null() {
        return out;
    }
    let mut index = 0;
    loop {
        // SAFETY: the caller states the array is null-terminated.
        let entry = unsafe { *argv.add(index) };
        if entry.is_null() {
            return out;
        }
        // SAFETY: an element of a valid argv is a NUL-terminated string.
        out.push(unsafe { CStr::from_ptr(entry) }.to_owned());
        index += 1;
    }
}

/// The command a spawn of `path` should really run.
///
/// `None` leaves the call alone, which is the answer for everything that is not a
/// Mach-O. Otherwise the runtime's own path, and an argv with `run <path>` in front of
/// the guest's arguments.
///
/// The guest's `argv[0]` is dropped rather than preserved: this runtime derives the
/// guest's `argv[0]` from the image it is told to run, so passing the original through
/// as well would put it in front of the program's real first argument.
fn rewrite(path: &CStr, argv: &[CString]) -> Option<(CString, Vec<CString>)> {
    if !is_mach_o(path) {
        return None;
    }
    let runtime = runtime_path()?;
    let mut rewritten = vec![runtime.clone(), c"run".to_owned(), path.to_owned()];
    rewritten.extend(argv.iter().skip(1).cloned());
    Some((runtime, rewritten))
}

/// Borrow a `Vec<CString>` as the null-terminated `char *const *` an exec wants.
fn as_argv(strings: &[CString]) -> Vec<*const c_char> {
    let mut raw: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    raw.push(std::ptr::null());
    raw
}

// ---------------------------------------------------------------------------
// The exec family.
// ---------------------------------------------------------------------------

/// `int execve(const char *path, char *const argv[], char *const envp[])`.
///
/// # Safety
///
/// The three arguments are as `execve` declares them.
pub unsafe extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if path.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    // SAFETY: the caller's guarantee.
    let (given, arguments) = unsafe { (CStr::from_ptr(path), argv_of(argv)) };
    let Some((runtime, rewritten)) = rewrite(given, &arguments) else {
        // SAFETY: not a Mach-O, so the host's own `execve` is the right call and the
        // caller's arguments are already in the shape it wants.
        return unsafe { libc::execve(path, argv, envp) };
    };
    let raw = as_argv(&rewritten);
    // SAFETY: `runtime` and every string in `rewritten` outlive the call, and `raw` is
    // null-terminated. On success this does not return.
    unsafe {
        libc::execve(runtime.as_ptr(), raw.as_ptr(), envp)
    }
}

/// `int execv(const char *path, char *const argv[])`.
///
/// # Safety
///
/// As [`execve`].
pub unsafe extern "C" fn execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    // SAFETY: `environ` is this process's own, and the caller's guarantee for the rest.
    unsafe { execve(path, argv, environ()) }
}

/// `int execvp(const char *file, char *const argv[])`, which searches `PATH`.
///
/// # Safety
///
/// As [`execve`].
pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    if file.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    // SAFETY: the caller's guarantee.
    let name = unsafe { CStr::from_ptr(file) };
    match search_path(name) {
        // Resolved to a Mach-O, so it has to go through the rewrite; the host's
        // `execvp` would find the same file and fail to execute it.
        Some(found) if is_mach_o(&found) => {
            // SAFETY: `found` outlives the call, and the caller's guarantee for argv.
            unsafe { execve(found.as_ptr(), argv, environ()) }
        }
        // SAFETY: not a Mach-O; the host's own search is the right one.
        _ => unsafe { libc::execvp(file, argv) },
    }
}

/// `PATH`'s answer for `name`, or `None` if the name is already a path.
fn search_path(name: &CStr) -> Option<CString> {
    let text = name.to_str().ok()?;
    if text.contains('/') {
        return CString::new(text).ok();
    }
    let path = std::env::var("PATH").ok()?;
    for directory in path.split(':') {
        let candidate = format!("{}/{text}", directory.trim_end_matches('/'));
        if std::fs::metadata(&candidate).is_ok() {
            return CString::new(candidate).ok();
        }
    }
    None
}

/// This process's environment, as the exec family's implicit third argument.
fn environ() -> *const *const c_char {
    unsafe extern "C" {
        static environ: *const *const c_char;
    }
    // SAFETY: `environ` is defined by the C library and read, never written.
    unsafe { environ }
}

// ---------------------------------------------------------------------------
// posix_spawn.
// ---------------------------------------------------------------------------

impl FileActions {
    /// Build the host object these actions describe.
    ///
    /// Returned by value with its own destructor, because a host
    /// `posix_spawn_file_actions_t` owns heap the caller has to give back.
    fn to_host(&self) -> Option<HostActions> {
        let mut host = HostActions::new()?;
        for action in &self.actions {
            // SAFETY: `host.0` is initialised, and every path outlives the call.
            let rc = unsafe {
                match action {
                    Action::Open {
                        fd,
                        path,
                        oflag,
                        mode,
                    } => {
                        // The guest's `O_*` are Darwin's, which are not the host's.
                        let Ok(oflag) = crate::fcntl::flags_to_host(*oflag) else {
                            return None;
                        };
                        libc::posix_spawn_file_actions_addopen(
                            &mut host.0,
                            *fd,
                            path.as_ptr(),
                            oflag,
                            *mode,
                        )
                    }
                    Action::Dup2 { from, to } => {
                        libc::posix_spawn_file_actions_adddup2(&mut host.0, *from, *to)
                    }
                    Action::Close(fd) => {
                        libc::posix_spawn_file_actions_addclose(&mut host.0, *fd)
                    }
                    #[cfg(not(target_os = "macos"))]
                    Action::Chdir(path) => {
                        libc::posix_spawn_file_actions_addchdir_np(&mut host.0, path.as_ptr())
                    }
                    #[cfg(not(target_os = "macos"))]
                    Action::Fchdir(fd) => {
                        libc::posix_spawn_file_actions_addfchdir_np(&mut host.0, *fd)
                    }
                    // A descriptor duplicated onto itself survives the exec with its
                    // close-on-exec bit cleared, which is what "inherit" asks for.
                    Action::Inherit(fd) => {
                        libc::posix_spawn_file_actions_adddup2(&mut host.0, *fd, *fd)
                    }
                    #[cfg(target_os = "macos")]
                    _ => unimplemented!()
                }
            };
            if rc != 0 {
                return None;
            }
        }
        Some(host)
    }
}

/// A host `posix_spawn_file_actions_t` that destroys itself.
struct HostActions(libc::posix_spawn_file_actions_t);

impl HostActions {
    fn new() -> Option<HostActions> {
        // SAFETY: `posix_spawn_file_actions_init` fills the object it is given.
        let mut raw = unsafe { std::mem::zeroed::<libc::posix_spawn_file_actions_t>() };
        // SAFETY: a live local of the right type.
        match unsafe { libc::posix_spawn_file_actions_init(&mut raw) } {
            0 => Some(HostActions(raw)),
            _ => None,
        }
    }
}

impl Drop for HostActions {
    fn drop(&mut self) {
        // SAFETY: initialised by `new` and destroyed exactly once.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
    }
}

/// A host `posix_spawnattr_t` that destroys itself.
struct HostAttr(libc::posix_spawnattr_t);

impl HostAttr {
    fn new() -> Option<HostAttr> {
        // SAFETY: `posix_spawnattr_init` fills the object it is given.
        let mut raw = unsafe { std::mem::zeroed::<libc::posix_spawnattr_t>() };
        // SAFETY: a live local of the right type.
        match unsafe { libc::posix_spawnattr_init(&mut raw) } {
            0 => Some(HostAttr(raw)),
            _ => None,
        }
    }
}

impl Drop for HostAttr {
    fn drop(&mut self) {
        // SAFETY: initialised by `new` and destroyed exactly once.
        unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
    }
}

/// A Darwin `sigset_t` - one 32-bit word - as the host's.
fn host_sigset(darwin: u32) -> libc::sigset_t {
    // SAFETY: a `sigset_t` is a plain bit set; `sigemptyset` initialises it below.
    let mut set = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    // SAFETY: a live local.
    unsafe { libc::sigemptyset(&mut set) };
    for bit in 0..32 {
        if darwin & (1 << bit) == 0 {
            continue;
        }
        // Darwin numbers signals from one, and bit zero is signal one.
        if let Some(host) = signal_to_host(bit + 1) {
            // SAFETY: a live local and a signal number the translation vouched for.
            unsafe { libc::sigaddset(&mut set, host) };
        }
    }
    set
}

impl Attributes {
    fn to_host(&self) -> Option<HostAttr> {
        let mut host = HostAttr::new()?;
        // Only the four flags that mean the same thing on both are forwarded. The
        // rest are handled here or dropped - see [`flag`] for why forwarding the word
        // would ask for something else entirely.
        let mut host_flags: libc::c_short = 0;
        for (darwin, linux) in [
            (flag::RESETIDS, libc::POSIX_SPAWN_RESETIDS),
            (flag::SETPGROUP, libc::POSIX_SPAWN_SETPGROUP),
            (flag::SETSIGDEF, libc::POSIX_SPAWN_SETSIGDEF),
            (flag::SETSIGMASK, libc::POSIX_SPAWN_SETSIGMASK),
        ] {
            if self.flags & darwin != 0 {
                host_flags |= linux as libc::c_short;
            }
        }
        // SAFETY: `host.0` is initialised and every argument is a live local.
        unsafe {
            if libc::posix_spawnattr_setflags(&mut host.0, host_flags) != 0 {
                return None;
            }
            if self.flags & flag::SETPGROUP != 0
                && libc::posix_spawnattr_setpgroup(&mut host.0, self.pgroup) != 0
            {
                return None;
            }
            if self.flags & flag::SETSIGMASK != 0 {
                let set = host_sigset(self.sigmask);
                if libc::posix_spawnattr_setsigmask(&mut host.0, &set) != 0 {
                    return None;
                }
            }
            if self.flags & flag::SETSIGDEF != 0 {
                let set = host_sigset(self.sigdefault);
                if libc::posix_spawnattr_setsigdefault(&mut host.0, &set) != 0 {
                    return None;
                }
            }
        }
        Some(host)
    }
}

/// `int posix_spawn(pid_t *, const char *, const posix_spawn_file_actions_t *,
/// const posix_spawnattr_t *, char *const argv[], char *const envp[])`.
///
/// # Safety
///
/// Every argument is as `posix_spawn` declares it.
pub unsafe extern "C" fn posix_spawn(
    pid: *mut libc::pid_t,
    path: *const c_char,
    file_actions: *mut c_void,
    attrp: *mut c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if path.is_null() {
        return libc::EFAULT;
    }
    // SAFETY: the caller's guarantee.
    let (given, arguments) = unsafe { (CStr::from_ptr(path), argv_of(argv)) };
    // SAFETY: both are null or this module's own objects.
    let (actions, attributes) = unsafe {
        (
            object::<FileActions>(file_actions),
            object::<Attributes>(attrp),
        )
    };

    // `POSIX_SPAWN_SETEXEC` asks for this process to be replaced rather than a child
    // made, which is an `execve` however the guest spelled it.
    if attributes.as_ref().is_some_and(|a| a.flags & flag::SETEXEC != 0) {
        // SAFETY: the caller's guarantee.
        unsafe { execve(path, argv, envp) };
        return crate::errno::get();
    }

    let host_actions = match actions {
        Some(actions) => match actions.to_host() {
            Some(host) => Some(host),
            None => return libc::ENOMEM,
        },
        None => None,
    };
    let host_attr = match attributes {
        Some(attributes) => match attributes.to_host() {
            Some(host) => Some(host),
            None => return libc::ENOMEM,
        },
        None => None,
    };
    let actions_ptr = host_actions
        .as_ref()
        .map(|a| &a.0 as *const _)
        .unwrap_or(std::ptr::null());
    let attr_ptr = host_attr
        .as_ref()
        .map(|a| &a.0 as *const _)
        .unwrap_or(std::ptr::null());

    match rewrite(given, &arguments) {
        None => {
            // SAFETY: not a Mach-O, so the host runs it directly; the objects are
            // this module's translations and outlive the call.
            unsafe {
                libc::posix_spawn(
                    pid,
                    path,
                    actions_ptr,
                    attr_ptr,
                    argv as *const *mut c_char,
                    envp as *const *mut c_char,
                )
            }
        }
        Some((runtime, rewritten)) => {
            let raw = as_argv(&rewritten);
            // SAFETY: as above, and `runtime` and `raw` outlive the call.
            unsafe {
                libc::posix_spawn(
                    pid,
                    runtime.as_ptr(),
                    actions_ptr,
                    attr_ptr,
                    raw.as_ptr() as *const *mut c_char,
                    envp as *const *mut c_char,
                )
            }
        }
    }
}

/// `int posix_spawnp(...)`, which searches `PATH` for the executable.
///
/// # Safety
///
/// As [`posix_spawn`].
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut libc::pid_t,
    file: *const c_char,
    file_actions: *mut c_void,
    attrp: *mut c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if file.is_null() {
        return libc::EFAULT;
    }
    // SAFETY: the caller's guarantee.
    let name = unsafe { CStr::from_ptr(file) };
    // Resolved here so the Mach-O test has a real file to read. A name that resolves
    // to nothing is left to the host, which reports it.
    let resolved = search_path(name);
    let path = resolved.as_deref().unwrap_or(name);
    // SAFETY: `path` outlives the call, and the caller's guarantee for the rest.
    unsafe { posix_spawn(pid, path.as_ptr(), file_actions, attrp, argv, envp) }
}

// ---------------------------------------------------------------------------
// The attribute objects.
// ---------------------------------------------------------------------------

/// # Safety
///
/// Each takes the guest's `posix_spawn*_t`, which Darwin declares as a `void *`.
pub unsafe extern "C" fn file_actions_init(at: *mut c_void) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { init::<FileActions>(at) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn file_actions_destroy(at: *mut c_void) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { destroy::<FileActions>(at) }
}

/// Add `action` to the object at `at`.
///
/// # Safety
///
/// As above.
unsafe fn add(at: *mut c_void, action: Action) -> c_int {
    // SAFETY: the caller's guarantee.
    match unsafe { object::<FileActions>(at) } {
        Some(actions) => {
            actions.actions.push(action);
            0
        }
        None => libc::EINVAL,
    }
}

/// # Safety
///
/// As above, and `path` is a NUL-terminated string.
pub unsafe extern "C" fn file_actions_addopen(
    at: *mut c_void,
    fd: c_int,
    path: *const c_char,
    oflag: c_int,
    mode: libc::mode_t,
) -> c_int {
    if path.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let path = unsafe { CStr::from_ptr(path) }.to_owned();
    // SAFETY: the caller's guarantee.
    unsafe {
        add(
            at,
            Action::Open {
                fd,
                path,
                oflag,
                mode,
            },
        )
    }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn file_actions_adddup2(at: *mut c_void, from: c_int, to: c_int) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { add(at, Action::Dup2 { from, to }) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn file_actions_addclose(at: *mut c_void, fd: c_int) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { add(at, Action::Close(fd)) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn file_actions_addinherit_np(at: *mut c_void, fd: c_int) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { add(at, Action::Inherit(fd)) }
}

/// # Safety
///
/// As above, and `path` is a NUL-terminated string.
pub unsafe extern "C" fn file_actions_addchdir_np(at: *mut c_void, path: *const c_char) -> c_int {
    if path.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: the caller's guarantee.
    let path = unsafe { CStr::from_ptr(path) }.to_owned();
    // SAFETY: the caller's guarantee.
    unsafe { add(at, Action::Chdir(path)) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn file_actions_addfchdir_np(at: *mut c_void, fd: c_int) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { add(at, Action::Fchdir(fd)) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn attr_init(at: *mut c_void) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { init::<Attributes>(at) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn attr_destroy(at: *mut c_void) -> c_int {
    // SAFETY: the caller's guarantee.
    unsafe { destroy::<Attributes>(at) }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn attr_setflags(at: *mut c_void, flags: u16) -> c_int {
    // SAFETY: the caller's guarantee.
    match unsafe { object::<Attributes>(at) } {
        Some(attributes) => {
            attributes.flags = flags as u32;
            0
        }
        None => libc::EINVAL,
    }
}

/// # Safety
///
/// As above, and `out` is a writable `short *`.
pub unsafe extern "C" fn attr_getflags(at: *mut c_void, out: *mut u16) -> c_int {
    // SAFETY: the caller's guarantee.
    match unsafe { object::<Attributes>(at) } {
        Some(attributes) if !out.is_null() => {
            // SAFETY: the caller states `out` is writable.
            unsafe { *out = attributes.flags as u16 };
            0
        }
        _ => libc::EINVAL,
    }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn attr_setpgroup(at: *mut c_void, pgroup: libc::pid_t) -> c_int {
    // SAFETY: the caller's guarantee.
    match unsafe { object::<Attributes>(at) } {
        Some(attributes) => {
            attributes.pgroup = pgroup;
            0
        }
        None => libc::EINVAL,
    }
}

/// # Safety
///
/// As above, and `set` is a readable Darwin `sigset_t` - one 32-bit word.
pub unsafe extern "C" fn attr_setsigmask(at: *mut c_void, set: *const u32) -> c_int {
    // SAFETY: the caller's guarantee.
    match unsafe { object::<Attributes>(at) } {
        Some(attributes) if !set.is_null() => {
            // SAFETY: the caller states `set` is a readable Darwin `sigset_t`.
            attributes.sigmask = unsafe { *set };
            0
        }
        _ => libc::EINVAL,
    }
}

/// # Safety
///
/// As above.
pub unsafe extern "C" fn attr_setsigdefault(at: *mut c_void, set: *const u32) -> c_int {
    // SAFETY: the caller's guarantee.
    match unsafe { object::<Attributes>(at) } {
        Some(attributes) if !set.is_null() => {
            // SAFETY: as above.
            attributes.sigdefault = unsafe { *set };
            0
        }
        _ => libc::EINVAL,
    }
}

/// `int posix_spawnattr_setbinpref_np(posix_spawnattr_t *, size_t, cpu_type_t *,
/// size_t *)`.
///
/// Accepted and ignored: it chooses which slice of a fat Mach-O to run, and this
/// runtime makes that choice itself when it loads the image. The out-parameter is
/// still filled, because a caller checks how many preferences were taken.
///
/// # Safety
///
/// As above, and `taken` is null or a writable `size_t`.
pub unsafe extern "C" fn attr_setbinpref_np(
    _at: *mut c_void,
    count: usize,
    _prefs: *mut u32,
    taken: *mut usize,
) -> c_int {
    if !taken.is_null() {
        // SAFETY: the caller states `taken` is writable.
        unsafe { *taken = count };
    }
    0
}

/// Empty on macOS, where the host's own spawn runs a Mach-O correctly.
#[cfg(target_os = "macos")]
pub fn symbol(_name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn symbol(name: &str) -> Option<u64> {
    macro_rules! at {
        ($f:expr) => {
            $f as *const () as usize as u64
        };
    }
    let address = match name {
        "execv" => at!(execv),
        "execve" => at!(execve),
        "execvp" => at!(execvp),
        "posix_spawn" => at!(posix_spawn),
        "posix_spawnp" => at!(posix_spawnp),
        "posix_spawn_file_actions_init" => at!(file_actions_init),
        "posix_spawn_file_actions_destroy" => at!(file_actions_destroy),
        "posix_spawn_file_actions_addopen" => at!(file_actions_addopen),
        "posix_spawn_file_actions_adddup2" => at!(file_actions_adddup2),
        "posix_spawn_file_actions_addclose" => at!(file_actions_addclose),
        "posix_spawn_file_actions_addinherit_np" => at!(file_actions_addinherit_np),
        "posix_spawn_file_actions_addchdir_np" => at!(file_actions_addchdir_np),
        "posix_spawn_file_actions_addfchdir_np" => at!(file_actions_addfchdir_np),
        "posix_spawnattr_init" => at!(attr_init),
        "posix_spawnattr_destroy" => at!(attr_destroy),
        "posix_spawnattr_setflags" => at!(attr_setflags),
        "posix_spawnattr_getflags" => at!(attr_getflags),
        "posix_spawnattr_setpgroup" => at!(attr_setpgroup),
        "posix_spawnattr_setsigmask" => at!(attr_setsigmask),
        "posix_spawnattr_setsigdefault" => at!(attr_setsigdefault),
        "posix_spawnattr_setbinpref_np" => at!(attr_setbinpref_np),
        _ => return None,
    };
    Some(address)
}

#[cfg(target_os = "macos")]
pub fn provide(_lib: &mut machish::resolve::SyntheticDylib) {}

#[cfg(not(target_os = "macos"))]
pub fn provide(lib: &mut machish::resolve::SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
}

/// Everything [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "execv",
    "execve",
    "execvp",
    "posix_spawn",
    "posix_spawnp",
    "posix_spawn_file_actions_init",
    "posix_spawn_file_actions_destroy",
    "posix_spawn_file_actions_addopen",
    "posix_spawn_file_actions_adddup2",
    "posix_spawn_file_actions_addclose",
    "posix_spawn_file_actions_addinherit_np",
    "posix_spawn_file_actions_addchdir_np",
    "posix_spawn_file_actions_addfchdir_np",
    "posix_spawnattr_init",
    "posix_spawnattr_destroy",
    "posix_spawnattr_setflags",
    "posix_spawnattr_getflags",
    "posix_spawnattr_setpgroup",
    "posix_spawnattr_setsigmask",
    "posix_spawnattr_setsigdefault",
    "posix_spawnattr_setbinpref_np",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The test that decides everything else: a Mach-O is recognised by its magic,
    /// and the host's own binaries are not.
    #[test]
    fn a_mach_o_is_told_from_an_elf() {
        let dir = std::env::temp_dir().join(format!("rosetta3-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");

        let mach_o = dir.join("thin");
        std::fs::write(&mach_o, 0xfeed_facfu32.to_le_bytes()).expect("write");
        let fat = dir.join("fat");
        std::fs::write(&fat, 0xcafe_babeu32.to_le_bytes()).expect("write");
        let elf = dir.join("elf");
        std::fs::write(&elf, b"\x7fELF____").expect("write");

        let c = |p: &std::path::Path| CString::new(p.to_str().unwrap()).unwrap();
        assert!(is_mach_o(&c(&mach_o)), "a thin 64-bit image");
        assert!(is_mach_o(&c(&fat)), "a fat archive");
        assert!(!is_mach_o(&c(&elf)), "an ELF must be left alone");
        assert!(!is_mach_o(c"/nonexistent-path-for-this-test"));
        // The host's own shell is the case that must never be rewritten.
        assert!(!is_mach_o(c"/bin/sh"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The rewrite puts `run <path>` in front of the guest's arguments, and drops the
    /// guest's `argv[0]` - which this runtime derives from the image instead.
    #[test]
    fn a_mach_o_spawn_is_repointed_at_this_runtime() {
        let dir = std::env::temp_dir().join(format!("rosetta3-rewrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let helper = dir.join("RobloxCrashHandler");
        std::fs::write(&helper, 0xfeed_facfu32.to_le_bytes()).expect("write");
        let path = CString::new(helper.to_str().unwrap()).unwrap();

        let argv = vec![c"RobloxCrashHandler".to_owned(), c"--pipe=3".to_owned()];
        let (runtime, rewritten) = rewrite(&path, &argv).expect("a Mach-O is rewritten");

        assert_eq!(runtime, rewritten[0], "argv[0] is the runtime itself");
        assert_eq!(rewritten[1], c"run".to_owned());
        assert_eq!(rewritten[2], path);
        assert_eq!(rewritten[3], c"--pipe=3".to_owned());
        assert_eq!(rewritten.len(), 4, "the guest's argv[0] is not passed on");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Anything that is not a Mach-O is left entirely alone.
    #[test]
    fn a_host_program_is_not_rewritten() {
        assert!(rewrite(c"/bin/sh", &[c"sh".to_owned()]).is_none());
    }

    /// Darwin's attribute types are `void *`, so the guest's eight bytes must hold
    /// this module's object and be left null once destroyed.
    #[test]
    fn an_attribute_object_lives_in_the_guest_s_pointer() {
        let mut slot: *mut c_void = std::ptr::null_mut();
        let at = &mut slot as *mut _ as *mut c_void;
        // SAFETY: `at` is a writable `void *`.
        unsafe {
            assert_eq!(file_actions_init(at), 0);
            assert!(!slot.is_null(), "init stores an object");
            assert_eq!(file_actions_adddup2(at, 3, 1), 0);
            assert_eq!(file_actions_addclose(at, 4), 0);
            assert_eq!(object::<FileActions>(at).unwrap().actions.len(), 2);
            assert_eq!(file_actions_destroy(at), 0);
            assert!(slot.is_null(), "destroy clears the guest's word");
            // And a second destroy is refused rather than freeing twice.
            assert_eq!(file_actions_destroy(at), libc::EINVAL);
        }
    }

    /// The flags that collide are the reason this translation exists: Darwin's
    /// `SETEXEC` is Linux's `USEVFORK`, so the word must never be passed through.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_the_agreeing_flags_are_forwarded() {
        let attributes = Attributes {
            flags: flag::SETSIGMASK | flag::START_SUSPENDED,
            sigmask: 1 << 1,
            ..Default::default()
        };
        let host = attributes.to_host().expect("a host attribute object");
        let mut flags: libc::c_short = 0;
        // SAFETY: an initialised host object and a live local.
        unsafe { libc::posix_spawnattr_getflags(&host.0, &mut flags) };
        assert_eq!(
            flags & libc::POSIX_SPAWN_SETSIGMASK as libc::c_short,
            libc::POSIX_SPAWN_SETSIGMASK as libc::c_short
        );
        assert_eq!(
            flags & libc::POSIX_SPAWN_SETSID as libc::c_short,
            0,
            "START_SUSPENDED must not be forwarded as SETSID"
        );
    }

    /// A Darwin signal number is translated on the way into a host mask.
    #[test]
    fn a_signal_mask_is_translated_rather_than_copied() {
        // Bit 1 is signal 2, `SIGINT`, which both platforms number the same.
        let set = host_sigset(1 << 1);
        // SAFETY: a live local and a valid signal.
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGINT) }, 1);
        // SAFETY: as above; nothing else was asked for.
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGQUIT) }, 0);
    }
}
