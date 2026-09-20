//! The host's own Steam client, as this process's copy of it.
//!
//! `steamclient.so` is an ordinary ELF shared library that talks to the Steam
//! process already running on this machine. Loading it here is what makes the
//! bridge worth having: the guest's Steamworks calls reach the user's real
//! account, friends list and overlay, and no macOS Steam client has to exist.

use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};

/// The signatures `steamclient.so` publishes. Named rather than spelled inline
/// because each one is also the annotation on the cast that adopts it, and a
/// cast that names the wrong signature is a call made in the wrong registers.
pub type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut i32) -> *mut c_void;
pub type GetCallbackFn = unsafe extern "C" fn(i32, *mut c_void, *mut i32) -> i8;
pub type FreeLastCallbackFn = unsafe extern "C" fn(i32) -> i8;
pub type GetApiCallResultFn = unsafe extern "C" fn(i32, u64, *mut c_void, i32, i32, *mut i8) -> i8;
pub type ReleaseThreadLocalMemoryFn = unsafe extern "C" fn(i32);
pub type IsKnownInterfaceFn = unsafe extern "C" fn(*const c_char) -> i8;
pub type NotifyMissingInterfaceFn = unsafe extern "C" fn(i32, *const c_char);
pub type BreakpadInitFn = unsafe extern "C" fn(u32, *const c_char, *const c_char);
pub type BreakpadSetAppIdFn = unsafe extern "C" fn(u32);
pub type BreakpadSetSteamIdFn = unsafe extern "C" fn(u64) -> i32;
pub type BreakpadCommentFn = unsafe extern "C" fn(*const c_char) -> i32;
pub type BreakpadMiniDumpFn = unsafe extern "C" fn(i32, i32);

/// The entry points `steamclient.so` publishes, which are the same seven
/// Valve's own `libsteam_api` calls.
pub struct Client {
    /// Kept so the library stays mapped for the life of the process. Never
    /// closed: the guest holds interface pointers into it.
    _handle: *mut c_void,
    pub path: PathBuf,
    pub create_interface: CreateInterfaceFn,
    pub get_callback: GetCallbackFn,
    pub free_last_callback: FreeLastCallbackFn,
    pub get_api_call_result: GetApiCallResultFn,
    pub release_thread_local_memory: ReleaseThreadLocalMemoryFn,
    pub is_known_interface: IsKnownInterfaceFn,
    pub notify_missing_interface: NotifyMissingInterfaceFn,
    /// The crash reporter, which is optional in a way the seven above are not:
    /// a guest that never crashes never calls it, and a client build without it
    /// should still serve every other call.
    pub breakpad: Breakpad,
}

/// Valve's minidump hooks. Each is `None` where the host client does not export
/// it, and the bridge then answers the guest without forwarding.
#[derive(Default)]
pub struct Breakpad {
    pub minidump_init: Option<BreakpadInitFn>,
    pub set_appid: Option<BreakpadSetAppIdFn>,
    pub set_steamid: Option<BreakpadSetSteamIdFn>,
    pub write_minidump_set_comment: Option<BreakpadCommentFn>,
    pub write_minidump_with_build_id: Option<BreakpadMiniDumpFn>,
}

// SAFETY: the handle and the entry points are immutable once loaded, and the
// client is internally synchronised - the guest calls it from whatever thread
// it likes, exactly as a Linux game does.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

impl Client {
    /// Load the host client at `path`.
    pub fn load(path: &Path) -> Result<Client, String> {
        let c = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("{}: path contains a NUL", path.display()))?;
        // `RTLD_NOW`, as Proton loads it: a lazily bound entry point that turns
        // out to be missing would fault inside the client with no name
        // attached. `RTLD_LOCAL` so that its symbols do not join the pool
        // `NativeDylibProvider` resolves a guest import against - the guest
        // must reach the client through this bridge and nowhere else.
        // SAFETY: `c` is a NUL-terminated path and outlives the call.
        let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("{}: {}", path.display(), dlerror()));
        }
        let required = |name: &str| -> Result<*mut c_void, String> {
            symbol(handle, name).ok_or_else(|| {
                format!(
                    "{} exports no {name}, so it is not a Steam client this bridge can use",
                    path.display()
                )
            })
        };
        // SAFETY: every cast below names the signature Valve's client declares,
        // which is the one Proton's `lsteamclient` calls it with.
        unsafe {
            Ok(Client {
                create_interface: cast::<CreateInterfaceFn>(required("CreateInterface")?),
                get_callback: cast::<GetCallbackFn>(required("Steam_BGetCallback")?),
                free_last_callback: cast::<FreeLastCallbackFn>(required("Steam_FreeLastCallback")?),
                get_api_call_result: cast::<GetApiCallResultFn>(required(
                    "Steam_GetAPICallResult",
                )?),
                release_thread_local_memory: cast::<ReleaseThreadLocalMemoryFn>(required(
                    "Steam_ReleaseThreadLocalMemory",
                )?),
                is_known_interface: cast::<IsKnownInterfaceFn>(required("Steam_IsKnownInterface")?),
                notify_missing_interface: cast::<NotifyMissingInterfaceFn>(required(
                    "Steam_NotifyMissingInterface",
                )?),
                breakpad: Breakpad {
                    minidump_init: symbol(handle, "Breakpad_SteamMiniDumpInit")
                        .map(|f| cast::<BreakpadInitFn>(f)),
                    set_appid: symbol(handle, "Breakpad_SteamSetAppID")
                        .map(|f| cast::<BreakpadSetAppIdFn>(f)),
                    set_steamid: symbol(handle, "Breakpad_SteamSetSteamID")
                        .map(|f| cast::<BreakpadSetSteamIdFn>(f)),
                    write_minidump_set_comment: symbol(
                        handle,
                        "Breakpad_SteamWriteMiniDumpSetComment",
                    )
                    .map(|f| cast::<BreakpadCommentFn>(f)),
                    write_minidump_with_build_id: symbol(
                        handle,
                        "Breakpad_SteamWriteMiniDumpUsingExceptionInfoWithBuildId",
                    )
                    .map(|f| cast::<BreakpadMiniDumpFn>(f)),
                },
                path: path.to_path_buf(),
                _handle: handle,
            })
        }
    }
}

/// A `dlsym` result as the function it is.
///
/// # Safety
///
/// `f` is a live function of type `T`, which is a bare function pointer.
unsafe fn cast<T: Copy>(f: *mut c_void) -> T {
    const { assert!(size_of::<T>() == size_of::<*mut c_void>()) };
    // SAFETY: `T` is pointer-sized by the assertion above, and the caller
    // states the address is a function of that type.
    unsafe { *(&raw const f).cast::<T>() }
}

fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `handle` came from `dlopen` and `c` outlives the call.
    let found = unsafe { libc::dlsym(handle, c.as_ptr()) };
    (!found.is_null()).then_some(found)
}

fn dlerror() -> String {
    // SAFETY: `dlerror` returns a NUL-terminated string or null, and is called
    // straight after the failure it reports on.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "dlopen failed with no message".into();
    }
    // SAFETY: non-null, and owned by the loader for at least until the next
    // `dlerror` on this thread.
    unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// Where the host's Steam client is, in the order a Linux game finds it.
///
/// `~/.steam/sdk64` is the path Valve's own `libsteam_api.so` uses and the only
/// one Proton looks at; the rest are the install directories it is usually a
/// symlink into, checked because a machine can have the install without the
/// SDK link.
pub fn host_client_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(super::HOST_CLIENT_VAR) {
        return Some(PathBuf::from(explicit));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(compat) = std::env::var_os("STEAM_COMPAT_CLIENT_INSTALL_PATH") {
        roots.push(PathBuf::from(compat));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".steam/sdk64"));
        roots.push(home.join(".steam/root"));
        roots.push(home.join(".steam/steam"));
        roots.push(home.join(".local/share/Steam"));
    }
    roots
        .into_iter()
        .flat_map(|root| {
            // `sdk64` holds the library directly; an install directory has it
            // under the architecture.
            [
                root.join("steamclient.so"),
                root.join("linux64/steamclient.so"),
            ]
        })
        .find(|p| p.exists())
}
