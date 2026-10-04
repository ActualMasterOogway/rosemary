#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;

const PAM_SUCCESS: c_int = 0;
const PAM_SYSTEM_ERR: c_int = 4;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 6;
const PAM_AUTH_ERR: c_int = 9;
const PAM_NEW_AUTHTOK_REQD: c_int = 10;
const PAM_USER_UNKNOWN: c_int = 13;
const PAM_AUTHTOK_ERR: c_int = 20;
const PAM_NO_MODULE_DATA: c_int = 24;
const PAM_IGNORE: c_int = 25;
const PAM_SYMBOL_ERR: c_int = 2;

/// `pam_strerror`'s table, in OpenPAM's order and words.
const MESSAGES: [&CStr; 30] = [
    c"Success",
    c"Failed to load module",
    c"Invalid symbol",
    c"Error in service module",
    c"System error",
    c"Memory buffer error",
    c"Conversation failure",
    c"Permission denied",
    c"Maximum number of tries exceeded",
    c"Authentication error",
    c"New authentication token required",
    c"Insufficient credentials",
    c"Authentication information is unavailable",
    c"Unknown user",
    c"Failed to retrieve user credentials",
    c"User credentials have expired",
    c"Failed to set user credentials",
    c"User account has expired",
    c"Authentication token has expired",
    c"Session failure",
    c"Authentication token failure",
    c"Failed to recover old authentication token",
    c"Authentication token lock busy",
    c"Authentication token aging disabled",
    c"Module data not found",
    c"Ignore this module",
    c"General failure",
    c"Try again",
    c"Unknown module type",
    c"Unknown authentication domain",
];

const PAM_SERVICE: c_int = 1;
const PAM_USER: c_int = 2;
const PAM_CONV: c_int = 5;
const PAM_RUSER: c_int = 8;
const PAM_LAST_ITEM: c_int = 13;

/// `struct pam_conv`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Conv {
    conv: usize,
    appdata: *mut c_void,
}

/// `struct pam_message`.
#[repr(C)]
pub struct Message {
    style: c_int,
    msg: *const c_char,
}

/// `struct pam_response`.
#[repr(C)]
pub struct Response {
    resp: *mut c_char,
    retcode: c_int,
}

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Facility {
    Auth,
    Account,
    Session,
    Password,
}

impl Facility {
    fn parse(word: &str) -> Option<Facility> {
        Some(match word {
            "auth" => Facility::Auth,
            "account" => Facility::Account,
            "session" => Facility::Session,
            "password" => Facility::Password,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Required,
    Requisite,
    Sufficient,
    Binding,
    Optional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Primitive {
    Authenticate,
    SetCred,
    AcctMgmt,
    OpenSession,
    CloseSession,
    ChAuthTok,
}

impl Primitive {
    fn facility(self) -> Facility {
        match self {
            Primitive::Authenticate | Primitive::SetCred => Facility::Auth,
            Primitive::AcctMgmt => Facility::Account,
            Primitive::OpenSession | Primitive::CloseSession => Facility::Session,
            Primitive::ChAuthTok => Facility::Password,
        }
    }
}

type Module = fn(&Handle, Primitive, &[String]) -> c_int;

fn module(name: &str) -> Option<Module> {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    let base = leaf.split(".so").next().unwrap_or(leaf);
    Some(match base {
        "pam_rootok" => rootok,
        "pam_opendirectory" => opendirectory,
        "pam_group" => group,
        "pam_permit" => |_, _, _| PAM_SUCCESS,
        "pam_deny" => deny,
        "pam_launchd" => |_, _, _| PAM_SUCCESS,
        "pam_smartcard" | "pam_tid" | "pam_watchid" | "pam_reattach" => |_, _, _| PAM_IGNORE,
        _ => return None,
    })
}

#[derive(Clone)]
struct Entry {
    facility: Facility,
    control: Control,
    module: Module,
    args: Vec<String>,
}

fn policy_path(service: &str) -> PathBuf {
    prefix::ns::host_path(&format!("/etc/pam.d/{service}"))
}

const MAX_DEPTH: usize = 8;

fn load(service: &str, depth: usize) -> Result<Vec<Entry>, String> {
    if depth > MAX_DEPTH || service.contains('/') {
        return Err(format!("{service}: include loop or bad name"));
    }
    let path = policy_path(service);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if depth > 0 && e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("/etc/pam.d/{service}: {e}")),
    };
    parse(service, &text, depth)
}

fn parse(service: &str, text: &str, depth: usize) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let joined = text.replace("\\\n", " ");
    for (number, line) in joined.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("");
        let words: Vec<&str> = line.split_whitespace().collect();
        let [facility, control, module_name, args @ ..] = words.as_slice() else {
            if words.is_empty() {
                continue;
            }
            return Err(format!(
                "/etc/pam.d/{service}:{}: too few fields",
                number + 1
            ));
        };
        let at = format!("/etc/pam.d/{service}:{}", number + 1);
        let facility = Facility::parse(facility)
            .ok_or_else(|| format!("{at}: unknown facility {facility}"))?;
        let control = match *control {
            "include" => {
                entries.extend(
                    load(module_name, depth + 1)?
                        .into_iter()
                        .filter(|entry| entry.facility == facility),
                );
                continue;
            }
            "required" => Control::Required,
            "requisite" => Control::Requisite,
            "sufficient" => Control::Sufficient,
            "binding" => Control::Binding,
            "optional" => Control::Optional,
            other => return Err(format!("{at}: unknown control flag {other}")),
        };
        let module = module(module_name)
            .ok_or_else(|| format!("{at}: {module_name} is not a module this runtime has"))?;
        entries.push(Entry {
            facility,
            control,
            module,
            args: args.iter().map(|arg| arg.to_string()).collect(),
        });
    }
    Ok(entries)
}

/// `pam_handle_t`.
pub struct Handle {
    /// String items, by number; `PAM_CONV` is kept apart.
    items: Vec<Option<CString>>,
    conv: Option<Box<Conv>>,
    env: Vec<CString>,
    data: Vec<(CString, *mut c_void, Option<Cleanup>)>,
    stack: Vec<Entry>,
}

type Cleanup = unsafe extern "C" fn(*mut Handle, *mut c_void, c_int);

impl Handle {
    fn item(&self, item: c_int) -> Option<&CStr> {
        self.items.get(item as usize)?.as_deref()
    }

    /// Run `primitive` down the stack, by OpenPAM's `openpam_dispatch`: a
    /// sufficient or binding success ends it if nothing required has failed
    /// (except for `setcred`, which every module sees), a requisite failure
    /// ends it at once, and the answer is the first required failure - or the
    /// first failure of any kind, if only those failed and nothing required.
    fn dispatch(&self, primitive: Primitive) -> c_int {
        let facility = primitive.facility();
        let chain: Vec<&Entry> = self
            .stack
            .iter()
            .filter(|e| e.facility == facility)
            .collect();
        if chain.is_empty() {
            return PAM_SYSTEM_ERR;
        }
        let mut failed = false;
        let mut error = PAM_SUCCESS;
        for entry in chain {
            let result = (entry.module)(self, primitive, &entry.args);
            match result {
                PAM_IGNORE => continue,
                PAM_SUCCESS => {
                    let ends = matches!(entry.control, Control::Sufficient | Control::Binding);
                    if ends && !failed && primitive != Primitive::SetCred {
                        break;
                    }
                }
                _ => {
                    if error == PAM_SUCCESS {
                        error = result;
                    }
                    let required = matches!(entry.control, Control::Required | Control::Binding);
                    if required && !failed {
                        failed = true;
                        error = result;
                    }
                    if entry.control == Control::Requisite {
                        failed = true;
                        break;
                    }
                }
            }
        }
        if !failed && error != PAM_NEW_AUTHTOK_REQD {
            error = PAM_SUCCESS;
        }
        error
    }
}

/// The account the request is about, by the guest's account database.
fn target(handle: &Handle) -> Option<(CString, u32)> {
    let name = handle.item(PAM_USER)?.to_owned();
    let (uid, _) = crate::passwd::user_ids(&name)?;
    Some((name, uid))
}

fn rootok(_: &Handle, primitive: Primitive, _: &[String]) -> c_int {
    match primitive {
        Primitive::Authenticate if crate::credentials::current().ruid == 0 => PAM_SUCCESS,
        Primitive::Authenticate => PAM_AUTH_ERR,
        _ => PAM_SUCCESS,
    }
}

fn opendirectory(handle: &Handle, primitive: Primitive, _: &[String]) -> c_int {
    let Some((name, _)) = target(handle) else {
        return PAM_USER_UNKNOWN;
    };
    match primitive {
        Primitive::Authenticate if crate::accounts::password_required(&name) => {
            PAM_AUTH_ERR
        }
        Primitive::ChAuthTok => PAM_AUTHTOK_ERR,
        _ => PAM_SUCCESS,
    }
}

fn deny(_: &Handle, primitive: Primitive, _: &[String]) -> c_int {
    match primitive {
        Primitive::ChAuthTok => PAM_AUTHTOK_ERR,
        _ => PAM_AUTH_ERR,
    }
}

/// `pam_group`: `group=a,b`, checking `ruser` instead of the target with
/// `ruser`, only when the target is root with `root_only`, inverted with
/// `deny`. `fail_safe` and `no_warn` change nothing here: every group
/// `crate::accounts` names exists.
fn group(handle: &Handle, primitive: Primitive, args: &[String]) -> c_int {
    if primitive != Primitive::AcctMgmt && primitive != Primitive::Authenticate {
        return PAM_IGNORE;
    }
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let groups: Vec<&str> = args
        .iter()
        .filter_map(|arg| arg.strip_prefix("group="))
        .flat_map(|list| list.split(','))
        .collect();
    if has("root_only") && !target(handle).is_some_and(|(_, uid)| uid == 0) {
        return PAM_IGNORE;
    }
    let who = match has("ruser") {
        true => handle.item(PAM_RUSER).map(CStr::to_owned),
        false => handle.item(PAM_USER).map(CStr::to_owned),
    };
    let Some(who) = who else {
        return PAM_AUTH_ERR;
    };
    let Some((_, base)) = crate::passwd::user_ids(&who) else {
        return PAM_USER_UNKNOWN;
    };
    let mine = crate::accounts::groups_of(&who, base);
    let member = groups.iter().any(|name| {
        let Ok(name) = CString::new(*name) else {
            return false;
        };
        crate::passwd::group_id(&name).is_some_and(|gid| mine.contains(&gid))
    });
    match member != has("deny") {
        true => PAM_SUCCESS,
        false => PAM_AUTH_ERR,
    }
}

/// # Safety
///
/// `pamh` is null or a handle `pam_start` made and `pam_end` has not freed.
unsafe fn handle<'a>(pamh: *mut Handle) -> Option<&'a mut Handle> {
    unsafe { pamh.as_mut() }
}

/// `int pam_start(const char *service, const char *user,
/// const struct pam_conv *conv, pam_handle_t **pamh)`
///
/// # Safety
///
/// `service` is NUL-terminated, `user` is null or NUL-terminated, `conv` is
/// null or a readable `struct pam_conv`, and `pamh` is writable.
pub unsafe extern "C" fn pam_start(
    service: *const c_char,
    user: *const c_char,
    conv: *const Conv,
    pamh: *mut *mut Handle,
) -> c_int {
    if service.is_null() || pamh.is_null() {
        return PAM_SYSTEM_ERR;
    }
    let service = unsafe { CStr::from_ptr(service) }.to_owned();
    let stack = match load(&service.to_string_lossy(), 0) {
        Ok(stack) => stack,
        Err(why) => {
            eprintln!("rosemary: pam: {why}");
            return PAM_SYSTEM_ERR;
        }
    };
    let mut items = vec![None; PAM_LAST_ITEM as usize + 1];
    items[PAM_SERVICE as usize] = Some(service);
    if !user.is_null() {
        items[PAM_USER as usize] = Some(unsafe { CStr::from_ptr(user) }.to_owned());
    }
    let conv = (!conv.is_null()).then(|| Box::new(unsafe { *conv }));
    let handle = Box::new(Handle {
        items,
        conv,
        env: Vec::new(),
        data: Vec::new(),
        stack,
    });
    unsafe { *pamh = Box::into_raw(handle) };
    PAM_SUCCESS
}

/// `int pam_end(pam_handle_t *, int status)`
///
/// # Safety
///
/// `pamh` is a live handle, which this frees.
pub unsafe extern "C" fn pam_end(pamh: *mut Handle, status: c_int) -> c_int {
    if pamh.is_null() {
        return PAM_SYSTEM_ERR;
    }
    // SAFETY: made by `pam_start`, freed only here.
    let handle = unsafe { Box::from_raw(pamh) };
    for (_, data, cleanup) in &handle.data {
        if let Some(cleanup) = cleanup {
            // SAFETY: the guest's cleanup, given the handle it registered on
            // - still alive until this returns - and the data it gave.
            unsafe { crate::call_as_darwin!(cleanup(pamh, *data, status)) };
        }
    }
    drop(handle);
    PAM_SUCCESS
}

macro_rules! primitive {
    ($name:ident, $primitive:expr) => {
        /// # Safety
        ///
        /// `pamh` is a live handle.
        pub unsafe extern "C" fn $name(pamh: *mut Handle, _flags: c_int) -> c_int {
            match unsafe { handle(pamh) } {
                Some(handle) => handle.dispatch($primitive),
                None => PAM_SYSTEM_ERR,
            }
        }
    };
}

primitive!(pam_authenticate, Primitive::Authenticate);
primitive!(pam_setcred, Primitive::SetCred);
primitive!(pam_acct_mgmt, Primitive::AcctMgmt);
primitive!(pam_open_session, Primitive::OpenSession);
primitive!(pam_close_session, Primitive::CloseSession);
primitive!(pam_chauthtok, Primitive::ChAuthTok);

/// `int pam_get_item(const pam_handle_t *, int item, const void **value)`
///
/// # Safety
///
/// `pamh` is a live handle and `value` is writable.
pub unsafe extern "C" fn pam_get_item(
    pamh: *mut Handle,
    item: c_int,
    value: *mut *const c_void,
) -> c_int {
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return PAM_SYSTEM_ERR;
    };
    if value.is_null() || !(1..=PAM_LAST_ITEM).contains(&item) {
        return PAM_SYMBOL_ERR;
    }
    let pointer = match item {
        PAM_CONV => handle
            .conv
            .as_deref()
            .map_or(std::ptr::null(), |conv| (conv as *const Conv).cast()),
        _ => handle
            .item(item)
            .map_or(std::ptr::null(), |text| text.as_ptr().cast()),
    };
    unsafe { *value = pointer };
    PAM_SUCCESS
}

/// `int pam_set_item(pam_handle_t *, int item, const void *value)`
///
/// # Safety
///
/// `pamh` is a live handle, and `value` is null, a NUL-terminated string, or
/// - for `PAM_CONV` - a readable `struct pam_conv`.
pub unsafe extern "C" fn pam_set_item(
    pamh: *mut Handle,
    item: c_int,
    value: *const c_void,
) -> c_int {
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return PAM_SYSTEM_ERR;
    };
    if !(1..=PAM_LAST_ITEM).contains(&item) {
        return PAM_SYMBOL_ERR;
    }
    match item {
        PAM_CONV => {
            handle.conv = (!value.is_null()).then(|| Box::new(unsafe { *value.cast::<Conv>() }))
        }
        _ => {
            handle.items[item as usize] =
                (!value.is_null()).then(|| unsafe { CStr::from_ptr(value.cast()) }.to_owned())
        }
    }
    PAM_SUCCESS
}

/// The environment entry for `name`, by index.
fn env_index(handle: &Handle, name: &[u8]) -> Option<usize> {
    handle.env.iter().position(|entry| {
        let entry = entry.as_bytes();
        entry.len() > name.len() && entry.starts_with(name) && entry[name.len()] == b'='
    })
}

/// `const char *pam_getenv(pam_handle_t *, const char *name)`
///
/// # Safety
///
/// `pamh` is a live handle and `name` is NUL-terminated.
pub unsafe extern "C" fn pam_getenv(pamh: *mut Handle, name: *const c_char) -> *const c_char {
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return std::ptr::null();
    };
    if name.is_null() {
        return std::ptr::null();
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    match env_index(handle, name) {
        Some(at) => unsafe { handle.env[at].as_ptr().add(name.len() + 1) },
        None => std::ptr::null(),
    }
}

/// `int pam_putenv(pam_handle_t *, const char *namevalue)`
///
/// # Safety
///
/// `pamh` is a live handle and `entry` is NUL-terminated.
pub unsafe extern "C" fn pam_putenv(pamh: *mut Handle, entry: *const c_char) -> c_int {
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return PAM_SYSTEM_ERR;
    };
    if entry.is_null() {
        return PAM_SYSTEM_ERR;
    }
    let entry = unsafe { CStr::from_ptr(entry) }.to_owned();
    let Some(eq) = entry.as_bytes().iter().position(|b| *b == b'=') else {
        return PAM_SYSTEM_ERR;
    };
    match env_index(handle, &entry.as_bytes()[..eq]) {
        Some(at) => handle.env[at] = entry,
        None => handle.env.push(entry),
    }
    PAM_SUCCESS
}

/// `char **pam_getenvlist(pam_handle_t *)`: a copy the caller frees, each
/// string and then the list.
///
/// # Safety
///
/// `pamh` is a live handle.
pub unsafe extern "C" fn pam_getenvlist(pamh: *mut Handle) -> *mut *mut c_char {
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: room for every entry and a null, from the allocator the guest
    // frees with.
    let list = unsafe { crate::heap::malloc((handle.env.len() + 1) * size_of::<*mut c_char>()) }
        .cast::<*mut c_char>();
    if list.is_null() {
        return list;
    }
    for (i, entry) in handle.env.iter().enumerate() {
        let bytes = entry.as_bytes_with_nul();
        // SAFETY: a block of exactly the entry's bytes, filled at once, and a
        // slot inside `list`.
        unsafe {
            let copy = crate::heap::malloc(bytes.len()).cast::<c_char>();
            if !copy.is_null() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), copy, bytes.len());
            }
            *list.add(i) = copy;
        }
    }
    // SAFETY: the last slot.
    unsafe { *list.add(handle.env.len()) = std::ptr::null_mut() };
    list
}

/// `const char *pam_strerror(const pam_handle_t *, int error)`
pub extern "C" fn pam_strerror(_pamh: *mut Handle, error: c_int) -> *const c_char {
    match MESSAGES.get(error as usize) {
        Some(text) if error >= 0 => text.as_ptr(),
        _ => c"Unknown error".as_ptr(),
    }
}

/// `int pam_set_data(pam_handle_t *, const char *name, void *data,
/// void (*cleanup)(pam_handle_t *, void *, int))`
///
/// # Safety
///
/// `pamh` is a live handle, `name` is NUL-terminated, and `cleanup` is null
/// or callable as declared.
pub unsafe extern "C" fn pam_set_data(
    pamh: *mut Handle,
    name: *const c_char,
    data: *mut c_void,
    cleanup: Option<Cleanup>,
) -> c_int {
    // SAFETY: the caller's obligation.
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return PAM_SYSTEM_ERR;
    };
    if name.is_null() {
        return PAM_SYSTEM_ERR;
    }
    let name = unsafe { CStr::from_ptr(name) }.to_owned();
    if let Some(at) = handle.data.iter().position(|(key, _, _)| *key == name) {
        let (_, old, old_cleanup) = handle.data.remove(at);
        if let Some(old_cleanup) = old_cleanup {
            // SAFETY: the cleanup registered with the data it replaces.
            unsafe { crate::call_as_darwin!(old_cleanup(pamh, old, PAM_SUCCESS)) };
        }
    }
    handle.data.push((name, data, cleanup));
    PAM_SUCCESS
}

/// `int pam_get_data(const pam_handle_t *, const char *name, const void **data)`
///
/// # Safety
///
/// `pamh` is a live handle, `name` is NUL-terminated and `data` is writable.
pub unsafe extern "C" fn pam_get_data(
    pamh: *mut Handle,
    name: *const c_char,
    data: *mut *const c_void,
) -> c_int {
    let Some(handle) = (unsafe { handle(pamh) }) else {
        return PAM_SYSTEM_ERR;
    };
    if name.is_null() || data.is_null() {
        return PAM_SYSTEM_ERR;
    }
    let name = unsafe { CStr::from_ptr(name) };
    match handle
        .data
        .iter()
        .find(|(key, _, _)| key.as_c_str() == name)
    {
        Some((_, value, _)) => {
            // SAFETY: the caller's slot.
            unsafe { *data = *value };
            PAM_SUCCESS
        }
        None => PAM_NO_MODULE_DATA,
    }
}

/// Read one line from standard input, without echo when `echo` is false.
fn read_line(echo: bool) -> Option<Vec<u8>> {
    let fd = libc::STDIN_FILENO;
    // SAFETY: a zeroed `termios` for the host to fill; the descriptor is
    // stdin, which is restored before returning.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let quiet = !echo && unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if quiet {
        let mut silent = saved;
        silent.c_lflag &= !libc::ECHO;
        // SAFETY: a `termios` the host filled, less echo.
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &silent) };
    }
    let mut line = Vec::new();
    let mut byte = 0u8;
    let complete = loop {
        // SAFETY: one byte into a local.
        match unsafe { libc::read(fd, (&raw mut byte).cast(), 1) } {
            1 if byte == b'\n' => break true,
            1 => line.push(byte),
            0 => break !line.is_empty(),
            _ if crate::errno::get() == libc::EINTR => continue,
            _ => break false,
        }
    };
    if quiet {
        // SAFETY: as saved above.
        unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &saved) };
        write_out(libc::STDOUT_FILENO, b"\n");
    }
    complete.then_some(line)
}

fn write_out(fd: c_int, bytes: &[u8]) {
    // SAFETY: a buffer of the length given.
    unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

/// `int openpam_ttyconv(int n, const struct pam_message **msg,
/// struct pam_response **resp, void *data)`: the conversation function a
/// command-line program hands `pam_start`, on its standard input and output.
///
/// # Safety
///
/// `msg` holds `n` readable message pointers and `resp` is writable.
pub unsafe extern "C" fn openpam_ttyconv(
    n: c_int,
    msg: *const *const Message,
    resp: *mut *mut Response,
    _data: *mut c_void,
) -> c_int {
    if n <= 0 || msg.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    let n = n as usize;
    // SAFETY: zeroed responses, from the allocator the guest frees with.
    let responses = unsafe { crate::heap::malloc(n * size_of::<Response>()) }.cast::<Response>();
    if responses.is_null() {
        return PAM_BUF_ERR;
    }
    // SAFETY: the block just allocated, of `n` responses.
    unsafe { std::ptr::write_bytes(responses, 0, n) };
    let free_all = |filled: usize| {
        for i in 0..filled {
            // SAFETY: responses this call filled.
            unsafe {
                let text = (*responses.add(i)).resp;
                if !text.is_null() {
                    crate::heap::free(text.cast());
                }
            }
        }
        unsafe { crate::heap::free(responses.cast()) };
    };
    for i in 0..n {
        // SAFETY: the caller's array of `n` messages.
        let message = unsafe { &**msg.add(i) };
        let text = match message.msg.is_null() {
            true => &[][..],
            // SAFETY: the module's NUL-terminated message.
            false => unsafe { CStr::from_ptr(message.msg) }.to_bytes(),
        };
        let answer = match message.style {
            PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                write_out(libc::STDOUT_FILENO, text);
                match read_line(message.style == PAM_PROMPT_ECHO_ON) {
                    Some(line) => Some(line),
                    None => {
                        free_all(i);
                        return PAM_CONV_ERR;
                    }
                }
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO => {
                let fd = match message.style {
                    PAM_ERROR_MSG => libc::STDERR_FILENO,
                    _ => libc::STDOUT_FILENO,
                };
                write_out(fd, text);
                if !text.ends_with(b"\n") {
                    write_out(fd, b"\n");
                }
                None
            }
            _ => {
                free_all(i);
                return PAM_CONV_ERR;
            }
        };
        if let Some(line) = answer {
            // SAFETY: a block of the line and its NUL, filled at once.
            unsafe {
                let copy = crate::heap::malloc(line.len() + 1).cast::<u8>();
                if copy.is_null() {
                    free_all(i);
                    return PAM_BUF_ERR;
                }
                std::ptr::copy_nonoverlapping(line.as_ptr(), copy, line.len());
                *copy.add(line.len()) = 0;
                (*responses.add(i)).resp = copy.cast();
            }
        }
    }
    // SAFETY: the caller's slot.
    unsafe { *resp = responses };
    PAM_SUCCESS
}

exports::exports! {
    functions {
        "pam_start" => pam_start,
        "pam_end" => pam_end,
        "pam_authenticate" => pam_authenticate,
        "pam_setcred" => pam_setcred,
        "pam_acct_mgmt" => pam_acct_mgmt,
        "pam_open_session" => pam_open_session,
        "pam_close_session" => pam_close_session,
        "pam_chauthtok" => pam_chauthtok,
        "pam_get_item" => pam_get_item,
        "pam_set_item" => pam_set_item,
        "pam_getenv" => pam_getenv,
        "pam_putenv" => pam_putenv,
        "pam_getenvlist" => pam_getenvlist,
        "pam_strerror" => pam_strerror,
        "pam_set_data" => pam_set_data,
        "pam_get_data" => pam_get_data,
        "openpam_ttyconv" => openpam_ttyconv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle_for(policy: &str, user: &str, ruser: Option<&str>) -> Handle {
        let mut items = vec![None; PAM_LAST_ITEM as usize + 1];
        items[PAM_USER as usize] = Some(CString::new(user).unwrap());
        items[PAM_RUSER as usize] = ruser.map(|r| CString::new(r).unwrap());
        Handle {
            items,
            conv: None,
            env: Vec::new(),
            data: Vec::new(),
            stack: parse("test", policy, 0).unwrap(),
        }
    }

    fn me() -> String {
        crate::accounts::me().unwrap()
    }

    /// Apple's own `/etc/pam.d/su`.
    const SU: &str = "# su: auth account session
auth       sufficient     pam_rootok.so
auth       required       pam_opendirectory.so
account    required       pam_group.so no_warn group=admin,wheel ruser root_only fail_safe
account    required       pam_opendirectory.so no_check_shell
password   required       pam_opendirectory.so
session    required       pam_launchd.so
";

    #[test]
    fn an_administrator_may_su_to_root() {
        let handle = handle_for(SU, "root", Some(&me()));
        assert_eq!(handle.dispatch(Primitive::Authenticate), PAM_SUCCESS);
        assert_eq!(handle.dispatch(Primitive::AcctMgmt), PAM_SUCCESS);
        assert_eq!(handle.dispatch(Primitive::OpenSession), PAM_SUCCESS);
    }

    #[test]
    fn root_is_only_for_the_groups_named() {
        let policy = "account required pam_group.so group=wheel ruser root_only\n";
        let handle = handle_for(policy, "root", Some(&me()));
        assert_eq!(handle.dispatch(Primitive::AcctMgmt), PAM_AUTH_ERR);
        // Anybody but root is not this module's business.
        let handle = handle_for(policy, &me(), Some(&me()));
        assert_eq!(handle.dispatch(Primitive::AcctMgmt), PAM_SUCCESS);
    }

    #[test]
    fn an_unknown_user_is_unknown() {
        let handle = handle_for(SU, "no-such-user-here", Some(&me()));
        assert_eq!(handle.dispatch(Primitive::Authenticate), PAM_USER_UNKNOWN);
    }

    #[test]
    fn a_requisite_failure_stops_the_chain_and_sufficient_ends_it() {
        let handle = handle_for(
            "auth requisite pam_deny.so\nauth sufficient pam_permit.so\n",
            "root",
            None,
        );
        assert_eq!(handle.dispatch(Primitive::Authenticate), PAM_AUTH_ERR);
        let handle = handle_for(
            "auth sufficient pam_permit.so\nauth required pam_deny.so\n",
            "root",
            None,
        );
        assert_eq!(handle.dispatch(Primitive::Authenticate), PAM_SUCCESS);
        let handle = handle_for("auth optional pam_deny.so\n", "root", None);
        assert_eq!(handle.dispatch(Primitive::Authenticate), PAM_SUCCESS);
    }

    #[test]
    fn a_stack_with_a_module_this_runtime_lacks_is_refused_by_name() {
        let error = parse("t", "auth required pam_krb5.so\n", 0).err().unwrap();
        assert!(error.contains("pam_krb5.so"), "{error}");
    }

    #[test]
    fn a_facility_with_no_modules_is_an_error() {
        let handle = handle_for("auth required pam_permit.so\n", "root", None);
        assert_eq!(handle.dispatch(Primitive::OpenSession), PAM_SYSTEM_ERR);
    }

    #[test]
    fn items_and_the_environment_round_trip() {
        let mut handle = Box::new(handle_for("auth required pam_permit.so\n", "root", None));
        let pamh: *mut Handle = &raw mut *handle;
        // SAFETY: a live handle this test owns.
        unsafe {
            assert_eq!(
                pam_set_item(pamh, PAM_RUSER, c"me".as_ptr().cast()),
                PAM_SUCCESS
            );
            let mut value: *const c_void = std::ptr::null();
            assert_eq!(pam_get_item(pamh, PAM_RUSER, &mut value), PAM_SUCCESS);
            assert_eq!(CStr::from_ptr(value.cast()), c"me");
            assert_eq!(pam_putenv(pamh, c"A=1".as_ptr()), PAM_SUCCESS);
            assert_eq!(pam_putenv(pamh, c"A=2".as_ptr()), PAM_SUCCESS);
            assert_eq!(CStr::from_ptr(pam_getenv(pamh, c"A".as_ptr())), c"2");
            let list = pam_getenvlist(pamh);
            assert_eq!(CStr::from_ptr(*list), c"A=2");
            assert!((*list.add(1)).is_null());
            crate::heap::free((*list).cast());
            crate::heap::free(list.cast());
        }
        drop(handle);
    }

    #[test]
    fn strerror_speaks_openpams_words() {
        // SAFETY: static strings.
        unsafe {
            assert_eq!(
                CStr::from_ptr(pam_strerror(std::ptr::null_mut(), PAM_AUTH_ERR)),
                c"Authentication error"
            );
            assert_eq!(
                CStr::from_ptr(pam_strerror(std::ptr::null_mut(), 999)),
                c"Unknown error"
            );
        }
    }
}
