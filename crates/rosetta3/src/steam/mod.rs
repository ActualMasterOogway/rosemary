use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod call;
mod client;
#[cfg(not(target_os = "macos"))]
mod ipctool;
mod table;

use table::class;

/// Turn the bridge off. Set to `0`, `off` or `false` and a guest that asks for
/// `steamclient.dylib` gets whatever is on disk, which is normally nothing.
pub const ENABLE_VAR: &str = "ROSETTA3_STEAM";

/// The generated guest-facing `steamclient.dylib`, when it is not where
/// [`guest_dylib`] would look.
pub const GUEST_CLIENT_VAR: &str = "ROSETTA3_STEAM_CLIENT";

/// The host's `steamclient.so`, when it is not where [`client::host_client_path`]
/// would look.
pub const HOST_CLIENT_VAR: &str = "ROSETTA3_STEAM_HOST_CLIENT";

/// Report every dispatched call on stderr.
pub const TRACE_VAR: &str = "ROSETTA3_STEAM_TRACE";

/// The name a guest opens the Steam client by, whatever directory it names.
const GUEST_CLIENT_LEAF: &str = "steamclient.dylib";

/// The symbol the generated dylib leaves undefined, and this module defines.
const DISPATCH_SYMBOL: &str = "_rosetta3_steamclient_dispatch";

/// What [`dispatch`] answers. The guest reads these from `rosetta3_steam.h`.
const OK: i32 = 0;
const UNKNOWN_METHOD: i32 = 1;
const NO_CLIENT: i32 = 2;

/// The ids the generator gives the entry points that are not interface methods.
/// The order is `BUILTIN_DISPATCH` in `gen_steamclient.py`.
mod builtin {
    pub const INIT: u32 = 0;
    pub const SHUTDOWN: u32 = 1;
    pub const CREATE_INTERFACE: u32 = 2;
    pub const IS_KNOWN_INTERFACE: u32 = 3;
    pub const NOTIFY_MISSING_INTERFACE: u32 = 4;
    pub const GET_CALLBACK: u32 = 5;
    pub const FREE_LAST_CALLBACK: u32 = 6;
    pub const GET_API_CALL_RESULT: u32 = 7;
    pub const RELEASE_THREAD_LOCAL_MEMORY: u32 = 8;
    pub const BREAKPAD_MINIDUMP_INIT: u32 = 9;
    pub const BREAKPAD_SET_APPID: u32 = 10;
    pub const BREAKPAD_SET_STEAMID: u32 = 11;
    pub const BREAKPAD_WRITE_MINIDUMP_SET_COMMENT: u32 = 12;
    pub const BREAKPAD_WRITE_MINIDUMP_WITH_BUILD_ID: u32 = 13;
}

/// The packed argument structs the non-method entry points send.
///
/// Field for field with the `steam_params_*` declarations in
/// `tools/steamclient/support/rosetta3_steam.h`, which are `#pragma pack(1)` -
/// so these are too, and a field read here is a field the guest wrote there.
mod params {
    use std::ffi::{c_char, c_void};

    #[repr(C, packed)]
    pub struct CreateInterface {
        pub name: *const c_char,
        pub return_code: *mut i32,
        pub ret: *mut c_void,
    }

    #[repr(C, packed)]
    pub struct NameOnly {
        pub name: *const c_char,
        pub ret: i8,
    }

    #[repr(C, packed)]
    pub struct PipeOnly {
        pub pipe: i32,
        pub ret: i8,
    }

    #[repr(C, packed)]
    pub struct NotifyMissing {
        pub pipe: i32,
        pub name: *const c_char,
    }

    #[repr(C, packed)]
    pub struct GetCallback {
        pub pipe: i32,
        pub msg: *mut c_void,
        pub ignored: *mut i32,
        pub ret: i8,
    }

    #[repr(C, packed)]
    pub struct GetApiCallResult {
        pub pipe: i32,
        pub call: u64,
        pub callback: *mut c_void,
        pub callback_size: i32,
        pub callback_expected: i32,
        pub failed: *mut i8,
        pub ret: i8,
    }

    #[repr(C, packed)]
    pub struct ReleaseTls {
        pub thread_exit: i32,
    }

    #[repr(C, packed)]
    pub struct BreakpadInit {
        pub a: u32,
        pub b: *const c_char,
        pub c: *const c_char,
    }

    #[repr(C, packed)]
    pub struct BreakpadAppId {
        pub appid: u32,
    }

    #[repr(C, packed)]
    pub struct BreakpadSteamId {
        pub steamid: u64,
        pub ret: i32,
    }

    #[repr(C, packed)]
    pub struct BreakpadComment {
        pub comment: *const c_char,
        pub ret: i32,
    }

    #[repr(C, packed)]
    pub struct BreakpadMiniDump {
        pub a: i32,
        pub b: i32,
    }
}

/// The host client and the table describing what may be asked of it.
struct Bridge {
    client: client::Client,
    table: table::Table,
}

/// Publish everything a guest needs to find the bridge before it runs.
///
/// One thing today: the Mach service `libsteam_api` asks where Steam is. See
/// [`ipctool`], and `docs/steam.md` for why a name server is involved at all.
pub fn install() {
    #[cfg(not(target_os = "macos"))]
    ipctool::install();
}

/// Whether the bridge may answer at all.
pub fn enabled() -> bool {
    !matches!(
        std::env::var(ENABLE_VAR).as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

fn tracing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(TRACE_VAR).is_some())
}

/// The generated guest-facing dylib, if this install has one.
///
/// Not checked in and not built by `cargo`: it is generated from the Steamworks
/// headers by `tools/steamclient/gen_steamclient.py`. So it is looked for where
/// an install puts it and where a checkout leaves it, and the bridge simply
/// does not exist when there is neither.
pub fn guest_dylib() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if !enabled() {
            return None;
        }
        if let Some(explicit) = std::env::var_os(GUEST_CLIENT_VAR) {
            let path = PathBuf::from(explicit);
            if !path.exists() {
                eprintln!(
                    "rosetta3: {GUEST_CLIENT_VAR}={}: no such file, so the Steam bridge is off",
                    path.display()
                );
                return None;
            }
            return Some(path);
        }
        let exe = std::env::current_exe().ok()?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let mut dir = exe.parent();
        // Beside the binary is how an install ships it. Walking up is for a
        // checkout, where the binary is in `target/<profile>/` and the
        // generator wrote into `tools/steamclient/generated/`.
        while let Some(at) = dir {
            for candidate in [
                at.join(GUEST_CLIENT_LEAF),
                at.join("tools/steamclient/generated")
                    .join(GUEST_CLIENT_LEAF),
            ] {
                if candidate.exists() {
                    return Some(candidate);
                }
            }
            dir = at.parent();
        }
        None
    })
    .as_deref()
}

/// The path a guest's `dlopen` of the Steam client should land on.
///
/// Consulted by [`crate::dyld::resolve_path`] before the filesystem search. A
/// game asks for whichever absolute path its Steam install would have had, and
/// none of them exist here; what has to open is the generated dylib.
pub fn redirect(path: &str) -> Option<String> {
    if path.rsplit('/').next().unwrap_or(path) != GUEST_CLIENT_LEAF {
        return None;
    }
    let ours = guest_dylib()?;
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    ANNOUNCED.call_once(|| {
        eprintln!(
            "rosetta3: steam: {path} -> {} (bridged to the host's Steam client)",
            ours.display()
        );
    });
    Some(ours.to_string_lossy().into_owned())
}

/// The dispatch entry point, as something the generated dylib can bind to.
///
/// One symbol, defined here rather than by the host's loader: the generated
/// dylib is linked `-undefined dynamic_lookup` and this is the only name it
/// leaves for Rosetta 3 to answer.
pub fn provider() -> machish::resolve::SyntheticDylib {
    let mut lib = machish::resolve::SyntheticDylib::new("<rosetta3 steam>");
    // Flat: the generated dylib records no library for the import, having been
    // linked against none.
    lib.flat(true);
    if guest_dylib().is_some() {
        lib.define_fn(DISPATCH_SYMBOL, dispatch as *const ());
    }
    lib
}

/// The host client and dispatch table, loaded on the guest's first call.
///
/// Deliberately not at start-up: most guests are not Steam games, and loading
/// the Steam client costs a `dlopen` and whatever it does to reach the Steam
/// process.
fn bridge() -> Option<&'static Bridge> {
    static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| match load() {
            Ok(bridge) => {
                eprintln!(
                    "rosetta3: steam: bridged to {} ({} dispatch ids)",
                    bridge.client.path.display(),
                    bridge.table.len()
                );
                Some(bridge)
            }
            Err(why) => {
                eprintln!("rosetta3: steam: {why}");
                None
            }
        })
        .as_ref()
}

fn load() -> Result<Bridge, String> {
    if !cfg!(target_arch = "x86_64") {
        return Err(unavailable().into());
    }
    let dylib = guest_dylib().ok_or("no generated steamclient.dylib, so nothing to bridge")?;
    let table_path = dylib.with_file_name("steamclient_dispatch.bin");
    let bytes = std::fs::read(&table_path).map_err(|e| {
        format!(
            "{}: {e}. It is written beside the dylib by \
             tools/steamclient/gen_steamclient.py; regenerate both together.",
            table_path.display()
        )
    })?;
    let table =
        table::Table::parse(&bytes).map_err(|e| format!("{}: {e}", table_path.display()))?;
    let host = client::host_client_path().ok_or_else(|| {
        format!(
            "no steamclient.so found. Install Steam, or name the library with \
             {HOST_CLIENT_VAR}."
        )
    })?;
    let client = client::Client::load(&host)?;
    Ok(Bridge { client, table })
}

/// Why there is no bridge on a host that is not x86-64.
///
/// A `steamclient.so` is an x86-64 ELF; nothing in this process can call one on
/// an aarch64 host, whatever the guest's architecture is.
fn unavailable() -> &'static str {
    "the host's Steam client is an x86-64 library and this host is not x86-64, \
     so Steamworks calls cannot be bridged"
}

/// Service one call from the generated dylib.
///
/// `method` is an id from the generated `steamclient_methods.h`; `params`
/// points at the packed struct the guest filled in, whose layout the dispatch
/// table describes. A non-zero answer means the call was not made, and the
/// guest reads back the zeroed return slot it arrived with.
///
/// # Safety
///
/// `params` is null, or points at the packed struct `method` names - which is
/// what the generated dylib always sends, both sides having been laid out by
/// the same generation.
pub unsafe extern "C" fn dispatch(method: u32, params: *mut c_void) -> i32 {
    if params.is_null() {
        return UNKNOWN_METHOD;
    }
    let Some(bridge) = bridge() else {
        return NO_CLIENT;
    };
    if method < builtin::BREAKPAD_WRITE_MINIDUMP_WITH_BUILD_ID + 1 {
        // SAFETY: the guest passes a struct of the shape this id names; see
        // [`params`], which mirrors the header both sides compile against.
        return unsafe { builtin_call(bridge, method, params) };
    }
    let Some((info, slots)) = bridge.table.method(method) else {
        eprintln!("rosetta3: steam: dispatch id {method} is not a method this build knows");
        return UNKNOWN_METHOD;
    };
    // SAFETY: as above - `params` is the packed struct the table describes, and
    // every read below is inside `params_size` bytes of it by construction.
    unsafe { method_call(method, info, slots, params) }
}

/// # Safety
///
/// `params` points at the packed struct `method` names.
unsafe fn builtin_call(bridge: &Bridge, method: u32, params: *mut c_void) -> i32 {
    let client = &bridge.client;
    // SAFETY: the caller states `params` has the shape this id names. Each arm
    // reads only the fields that shape declares.
    unsafe {
        match method {
            // Nothing to do: reaching this function has already loaded the
            // client, which is all these two ever asked for.
            builtin::INIT | builtin::SHUTDOWN => {}
            builtin::CREATE_INTERFACE => {
                let p = &mut *params.cast::<params::CreateInterface>();
                let name = p.name;
                let found = (client.create_interface)(name, p.return_code);
                p.ret = found;
                if tracing() {
                    eprintln!(
                        "rosetta3: steam: CreateInterface({}) -> {found:?}",
                        text(name)
                    );
                }
            }
            builtin::IS_KNOWN_INTERFACE => {
                let p = &mut *params.cast::<params::NameOnly>();
                p.ret = (client.is_known_interface)(p.name);
            }
            builtin::NOTIFY_MISSING_INTERFACE => {
                let p = &*params.cast::<params::NotifyMissing>();
                (client.notify_missing_interface)(p.pipe, p.name);
            }
            builtin::GET_CALLBACK => {
                let p = &mut *params.cast::<params::GetCallback>();
                p.ret = (client.get_callback)(p.pipe, p.msg, p.ignored);
            }
            builtin::FREE_LAST_CALLBACK => {
                let p = &mut *params.cast::<params::PipeOnly>();
                p.ret = (client.free_last_callback)(p.pipe);
            }
            builtin::GET_API_CALL_RESULT => {
                let p = &mut *params.cast::<params::GetApiCallResult>();
                p.ret = (client.get_api_call_result)(
                    p.pipe,
                    p.call,
                    p.callback,
                    p.callback_size,
                    p.callback_expected,
                    p.failed,
                );
            }
            builtin::RELEASE_THREAD_LOCAL_MEMORY => {
                let p = &*params.cast::<params::ReleaseTls>();
                (client.release_thread_local_memory)(p.thread_exit);
            }
            builtin::BREAKPAD_MINIDUMP_INIT => {
                if let Some(f) = client.breakpad.minidump_init {
                    let p = &*params.cast::<params::BreakpadInit>();
                    f(p.a, p.b, p.c);
                }
            }
            builtin::BREAKPAD_SET_APPID => {
                if let Some(f) = client.breakpad.set_appid {
                    let p = &*params.cast::<params::BreakpadAppId>();
                    f(p.appid);
                }
            }
            builtin::BREAKPAD_SET_STEAMID => {
                if let Some(f) = client.breakpad.set_steamid {
                    let p = &mut *params.cast::<params::BreakpadSteamId>();
                    p.ret = f(p.steamid);
                }
            }
            builtin::BREAKPAD_WRITE_MINIDUMP_SET_COMMENT => {
                if let Some(f) = client.breakpad.write_minidump_set_comment {
                    let p = &mut *params.cast::<params::BreakpadComment>();
                    p.ret = f(p.comment);
                }
            }
            builtin::BREAKPAD_WRITE_MINIDUMP_WITH_BUILD_ID => {
                if let Some(f) = client.breakpad.write_minidump_with_build_id {
                    let p = &*params.cast::<params::BreakpadMiniDump>();
                    f(p.a, p.b);
                }
            }
            _ => return UNKNOWN_METHOD,
        }
    }
    OK
}

/// Make one virtual call on the host interface the guest named.
///
/// # Safety
///
/// `params` points at the packed struct `info` and `slots` describe, whose
/// first field is a pointer the host's `CreateInterface` handed out.
unsafe fn method_call(
    method: u32,
    info: &table::Method,
    slots: &[table::Slot],
    params: *mut c_void,
) -> i32 {
    let base = params.cast::<u8>();
    // SAFETY: the interface pointer is the first field of every params struct.
    let iface = unsafe { base.cast::<*mut u8>().read_unaligned() };
    if iface.is_null() {
        eprintln!("rosetta3: steam: dispatch id {method} on a null interface");
        return NO_CLIENT;
    }
    // SAFETY: `iface` is a C++ object the host client created, so its first
    // word is its vtable pointer and the slot the table names is within it.
    let func = unsafe {
        let object = iface.add(info.iface_offset as usize);
        let vtable = object.cast::<*const *const c_void>().read();
        vtable.add(info.vtable_index as usize).read()
    };
    if func.is_null() {
        eprintln!("rosetta3: steam: dispatch id {method} has no vtable entry");
        return NO_CLIENT;
    }

    let mut args = call::Args::default();
    // A return value too large for registers is written through a hidden first
    // pointer; 64 bytes is the largest by-value type in any SDK signature, and
    // the generator refuses a signature naming one it does not know.
    let mut returned = Aligned([0u8; 64]);
    let large = info.ret.class == class::MEMORY;
    if large {
        args.int(returned.0.as_mut_ptr() as usize as u64);
    }
    args.int(iface as usize as u64);
    for slot in slots {
        // SAFETY: the offset and size come from the table the same generation
        // laid the struct out with, so the read is inside it.
        let at = unsafe { base.add(slot.offset as usize) };
        match slot.class {
            // Sub-word integers are zero-extended, which is what the callee's
            // own prologue would do with the register anyway.
            class::INTEGER => args.int(unsafe { word(at, slot.size) }),
            class::SSE => args.sse(unsafe { word(at, slot.size) }),
            class::INTEGER2 => args.int_pair(unsafe { word(at, 8) }, unsafe { word(at.add(8), 8) }),
            class::MEMORY => {
                let mut bytes = [0u8; 64];
                let size = slot.size as usize;
                unsafe { std::ptr::copy_nonoverlapping(at, bytes.as_mut_ptr(), size) };
                args.memory(&bytes[..size]);
            }
            other => {
                eprintln!("rosetta3: steam: dispatch id {method} names argument class {other}");
                return UNKNOWN_METHOD;
            }
        }
    }

    if tracing() {
        eprintln!(
            "rosetta3: steam: id {method} -> {iface:?} vtable[{}] ({} args)",
            info.vtable_index,
            slots.len()
        );
    }

    // SAFETY: `func` is the vtable slot the table names for this id, and the
    // arguments above were built from that same id's recorded signature.
    let got = unsafe { args.call(func) };

    if info.ret.class == class::VOID {
        return OK;
    }
    // SAFETY: the return slot is `ret.size` bytes at `ret.offset`, which the
    // generator laid out in the struct the guest allocated.
    let into = unsafe { base.add(info.ret.offset as usize) };
    let size = info.ret.size as usize;
    // SAFETY: each arm writes exactly the `size` bytes the slot holds.
    unsafe {
        match info.ret.class {
            class::INTEGER => store(into, &got.rax.to_le_bytes(), size),
            class::SSE => store(into, &got.xmm0.to_le_bytes(), size),
            class::INTEGER2 => {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&got.rax.to_le_bytes());
                bytes[8..].copy_from_slice(&got.rdx.to_le_bytes());
                store(into, &bytes, size);
            }
            class::MEMORY => store(into, &returned.0, size),
            _ => return UNKNOWN_METHOD,
        }
    }
    OK
}

/// A buffer for a returned aggregate, aligned as the largest of them needs.
#[repr(align(16))]
struct Aligned([u8; 64]);

/// `size` bytes at `at`, zero-extended into a register-sized word.
///
/// # Safety
///
/// `at` names `size` readable bytes, and `size` is at most eight.
unsafe fn word(at: *const u8, size: u8) -> u64 {
    let mut bytes = [0u8; 8];
    // SAFETY: the caller states the bytes are there; `min` keeps the write
    // inside `bytes` whatever the table said.
    unsafe {
        std::ptr::copy_nonoverlapping(at, bytes.as_mut_ptr(), (size as usize).min(8));
    }
    u64::from_le_bytes(bytes)
}

/// Write the first `size` bytes of `from` to `into`, which may be unaligned.
///
/// # Safety
///
/// `into` names `size` writable bytes and `from` is at least that long.
unsafe fn store(into: *mut u8, from: &[u8], size: usize) {
    // SAFETY: the caller states the destination is that large; `min` keeps the
    // read inside `from` whatever the table said.
    unsafe {
        std::ptr::copy_nonoverlapping(from.as_ptr(), into, size.min(from.len()));
    }
}

/// A C string, for a diagnostic.
fn text(p: *const c_char) -> String {
    if p.is_null() {
        return "(null)".into();
    }
    // SAFETY: non-null, and the guest passed it as a NUL-terminated name.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the Steam client's own name is redirected: a guest opening
    /// something else must still get what is on disk.
    #[test]
    fn redirects_nothing_but_the_steam_client() {
        assert!(redirect("/usr/lib/libz.dylib").is_none());
        assert!(redirect("steamclient.dylib.bak").is_none());
        // And the leaf is what matters, not the directory, because every Steam
        // install spells the path differently.
        assert_eq!(
            redirect("/Steam/steamclient.dylib").is_some(),
            guest_dylib().is_some()
        );
    }

    /// A null params pointer is the guest being wrong, and must be refused
    /// rather than dereferenced.
    #[test]
    fn a_null_params_pointer_is_refused() {
        // SAFETY: a null `params`, which is the case under test.
        let answer = unsafe { dispatch(builtin::CREATE_INTERFACE, std::ptr::null_mut()) };
        assert_eq!(answer, UNKNOWN_METHOD);
    }

    /// The structs this module reads must have the offsets the guest's header
    /// gives them: `#pragma pack(1)`, so every field follows the last.
    #[test]
    fn the_params_structs_are_packed_like_the_guest_header() {
        assert_eq!(std::mem::size_of::<params::CreateInterface>(), 24);
        assert_eq!(std::mem::size_of::<params::NameOnly>(), 9);
        assert_eq!(std::mem::size_of::<params::PipeOnly>(), 5);
        assert_eq!(std::mem::size_of::<params::NotifyMissing>(), 12);
        assert_eq!(std::mem::size_of::<params::GetCallback>(), 21);
        assert_eq!(std::mem::size_of::<params::GetApiCallResult>(), 37);
        assert_eq!(std::mem::size_of::<params::BreakpadInit>(), 20);
        assert_eq!(std::mem::size_of::<params::BreakpadSteamId>(), 12);
        assert_eq!(std::mem::size_of::<params::BreakpadComment>(), 12);
    }

    /// The provider defines the one symbol the generated dylib leaves for it,
    /// and only when there is a dylib to answer for.
    #[test]
    fn the_dispatch_symbol_is_published_with_the_dylib() {
        let lib = provider();
        let mut names = Vec::new();
        machish::resolve::SymbolProvider::enumerate(&lib, &mut |name, _| {
            names.push(name.to_string());
        });
        match guest_dylib().is_some() {
            true => assert_eq!(names, [DISPATCH_SYMBOL]),
            false => assert!(names.is_empty()),
        }
    }
}
