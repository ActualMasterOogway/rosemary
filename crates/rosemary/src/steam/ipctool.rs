#![cfg(not(target_os = "macos"))]

use std::ffi::CString;

use darwin::mach::bootstrap::{BOOTSTRAP_PORT, bootstrap_check_in};
use darwin::mach::kern::{KERN_SUCCESS, MACH_MSG_SUCCESS};
use darwin::mach::msg::{MOVE_SEND, mach_msg, mach_msg_destroy};
use darwin::mach::space::{MACH_PORT_NULL, Name};

/// The name Steam publishes and `CIPCToolMachClient` looks up.
const SERVICE: &[u8] = b"com.valvesoftware.steam.ipctool\0";

/// `mach_msg` options, from `<mach/message.h>`.
const SEND_MSG: i32 = 0x0000_0001;
const RCV_MSG: i32 = 0x0000_0002;
const SEND_TIMEOUT: i32 = 0x0000_0010;

/// `mach_msg_header_t`: six words, and the offsets of the four fields a reply
/// has to set.
const HEADER: usize = 24;
const BITS: usize = 0;
const SIZE: usize = 4;
const REMOTE: usize = 8;
const LOCAL: usize = 12;
const ID: usize = 20;

/// The body, as `CSimpleMachClient` lays it out: a command word, then one word
/// of answer, then - for a path reply - the path.
const COMMAND: usize = HEADER;
const ANSWER: usize = HEADER + 4;
const PATH: usize = HEADER + 8;

/// The two commands that matter. `KILL` is the third, and is sent only to an
/// ipctool older than the caller - which this never claims to be.
const COMMAND_VERSION: u32 = 0x0001_86a1;
const COMMAND_KILL: u32 = 0x0001_86a0;
const COMMAND_STEAM_PATH: u32 = 0x0000_000e;

/// The protocol version this answers with.
///
/// `CIPCToolMachClient::Init` compares what comes back against the version the
/// caller was built with and, if the server's is **lower**, kills it and
/// expects a newer one to be started. Nothing here can be restarted, so this is
/// the highest version any shipped `libsteam_api` asks for; a caller built
/// against an older SDK is satisfied by it as well.
const VERSION: u32 = 0x68;

/// How large a reply is in each direction. The caller sizes its receive buffer
/// for exactly these and `mach_msg` refuses anything larger, so they are part
/// of the protocol rather than a choice.
///
/// A trailer of eight bytes goes on top of each, which is why the caller's
/// buffers are 0x28 and 0x228.
const VERSION_REPLY: usize = HEADER + 8;
const PATH_REPLY: usize = HEADER + 8 + PATH_BYTES;
/// The path field's own length, which is what is left of the reply.
const PATH_BYTES: usize = 512;

/// Room for the largest request plus its trailer, with slack: a request is 32
/// bytes and anything bigger is not one of these commands.
const REQUEST_BUFFER: usize = 256;

/// Publish the service, once, if this install has a bridge to point at.
///
/// Called before any guest code runs: `libsteam_api` looks the name up during
/// `SteamAPI_Init`, and a registration that lands after the lookup is a
/// registration that did not happen.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(dylib) = super::guest_dylib() else {
            return;
        };
        let Some(path) = steam_path(dylib) else {
            return;
        };
        let mut port = MACH_PORT_NULL;
        // SAFETY: a NUL-terminated service name and a live local for the port.
        let rc = unsafe { bootstrap_check_in(BOOTSTRAP_PORT, SERVICE.as_ptr().cast(), &mut port) };
        if rc != KERN_SUCCESS {
            // eprintln!(
            //     "rosemary: steam: could not publish the Steam IPC service ({rc}); a guest \
            //      that asks for Steam will be told it is not running"
            // );
            return;
        }
        std::thread::Builder::new()
            .name("rosemary-steam-ipc".into())
            .spawn(move || serve(port, path))
            .map(|_| ())
            .unwrap_or_else(|e| eprintln!("rosemary: steam: no IPC thread: {e}"));
    });
}

/// What to answer the "where is Steam installed" question with.
///
/// The caller replaces the last path component with `steamclient.dylib`, so
/// what has to be right is the *directory* - and the file this names is the one
/// that would then be opened. It must contain a separator, which the caller
/// checks for and complains about by name.
fn steam_path(dylib: &std::path::Path) -> Option<CString> {
    let dir = dylib.parent()?;
    let path = dir.join("steam");
    let bytes = path.as_os_str().as_encoded_bytes();
    // One short of the field, so the NUL the caller reads up to is inside it.
    if bytes.len() >= PATH_BYTES {
        eprintln!(
            "rosemary: steam: {} is too long to report to a guest",
            path.display()
        );
        return None;
    }
    CString::new(bytes).ok()
}

/// The process id the guest is told Steam is running as.
///
/// It is used for one thing on the other side: `kill(pid, 0)`, asked both by
/// `SteamAPI_Init` and by `SteamAPI_IsSteamRunning`. The host's own Steam is
/// the honest answer and the one that makes "is Steam running" mean what it
/// says, so it is preferred; where no pid file names a live process this
/// process stands in, and the truth then comes from the real client refusing to
/// hand out interfaces.
fn steam_pid() -> u32 {
    let files = std::env::var_os("HOME").into_iter().flat_map(|home| {
        let home = std::path::PathBuf::from(home);
        [
            home.join(".steam/steam.pid"),
            home.join(".local/share/Steam/steam.pid"),
        ]
    });
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(pid) = text.trim().parse::<i32>() else {
            continue;
        };
        // SAFETY: signal zero performs the permission and liveness checks and
        // sends nothing.
        if pid > 0 && unsafe { libc::kill(pid, 0) } == 0 {
            return pid as u32;
        }
    }
    unsafe { libc::getpid() as u32 }
}

/// Answer requests until the port goes away.
fn serve(port: Name, path: CString) {
    let mut request = [0u8; REQUEST_BUFFER];
    let mut failures = 0;
    loop {
        // SAFETY: the whole of `request` is writable, and that is the size
        // passed. No timeout, so this blocks until a message arrives.
        let rc = unsafe {
            mach_msg(
                request.as_mut_ptr().cast(),
                RCV_MSG,
                0,
                request.len() as u32,
                port,
                0,
                MACH_PORT_NULL,
            )
        };
        if rc != MACH_MSG_SUCCESS {
            // A receive that keeps failing is a port that is gone, and a loop
            // that keeps trying is a spin. Three is enough to tell one from a
            // message this build could not parse.
            failures += 1;
            if failures > 3 {
                eprintln!("rosemary: steam: the Steam IPC service stopped ({rc:#x})");
                return;
            }
            continue;
        }
        failures = 0;
        answer(&mut request, &path);
    }
}

/// Reply to one received request, in place.
fn answer(request: &mut [u8], path: &CString) {
    // A request is a header and two words; anything shorter is not one.
    if word(request, SIZE) < (HEADER + 8) as u32 {
        discard(request);
        return;
    }
    let reply_to = word(request, REMOTE);
    let id = word(request, ID);
    let command = word(request, COMMAND);
    if reply_to == MACH_PORT_NULL {
        discard(request);
        return;
    }

    let mut reply = [0u8; PATH_REPLY];
    let size = match command {
        COMMAND_VERSION => {
            put(&mut reply, ANSWER, VERSION);
            VERSION_REPLY
        }
        COMMAND_STEAM_PATH => {
            put(&mut reply, ANSWER, steam_pid());
            let bytes = path.as_bytes();
            reply[PATH..PATH + bytes.len()].copy_from_slice(bytes);
            PATH_REPLY
        }
        // Sent without waiting for a reply, and only to a server this one is
        // never mistaken for; the right that came with it still has to go.
        COMMAND_KILL => {
            discard(request);
            return;
        }
        _ => {
            eprintln!("rosemary: steam: the guest asked the Steam IPC service for {command:#x}");
            discard(request);
            return;
        }
    };

    // `MACH_MSGH_BITS(MOVE_SEND, 0)`: the reply port arrived as a send right
    // and is spent by replying to it. The id is echoed because the caller
    // receives in a loop until it sees its own back - that is how it tells its
    // reply from anything else queued on the port.
    put(&mut reply, BITS, MOVE_SEND);
    put(&mut reply, SIZE, size as u32);
    put(&mut reply, REMOTE, reply_to);
    put(&mut reply, LOCAL, MACH_PORT_NULL);
    put(&mut reply, ID, id);
    put(&mut reply, COMMAND, command);

    // SAFETY: `size` readable bytes of `reply`, which is at least that long.
    let rc = unsafe {
        mach_msg(
            reply.as_mut_ptr().cast(),
            SEND_MSG | SEND_TIMEOUT,
            size as u32,
            0,
            MACH_PORT_NULL,
            SEND_TIMEOUT_MS,
            MACH_PORT_NULL,
        )
    };
    if rc != MACH_MSG_SUCCESS {
        eprintln!("rosemary: steam: the Steam IPC reply could not be sent ({rc:#x})");
    }
}

/// Long enough that a guest thread descheduled mid-receive still gets its
/// answer, short enough that a guest that has gone away cannot wedge this one.
const SEND_TIMEOUT_MS: u32 = 1000;

/// Destroy the rights in a message that is not being replied to.
fn discard(request: &mut [u8]) {
    // SAFETY: a message this thread received, whose rights are still its own.
    unsafe { mach_msg_destroy(request.as_mut_ptr().cast()) };
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]))
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reply sizes are the caller's buffers less the trailer `mach_msg`
    /// appends, and a reply one byte over is refused rather than truncated.
    #[test]
    fn the_reply_sizes_are_the_ones_the_caller_sized_its_buffers_for() {
        const TRAILER: usize = 8;
        assert_eq!(VERSION_REPLY + TRAILER, 0x28);
        assert_eq!(PATH_REPLY + TRAILER, 0x228);
    }

    /// The path field must hold a NUL, because the caller `strncpy`s out of it
    /// with a length longer than the field.
    #[test]
    fn a_path_that_would_fill_the_field_is_refused() {
        let long = std::path::PathBuf::from("/".to_string() + &"x".repeat(PATH_BYTES));
        assert!(steam_path(&long.join("steamclient.dylib")).is_none());
        let ok = steam_path(std::path::Path::new("/opt/steam/steamclient.dylib")).unwrap();
        assert_eq!(ok.as_bytes(), b"/opt/steam/steam");
    }

    /// The pid has to name a live process, because that is the only thing the
    /// other side does with it.
    #[test]
    fn the_reported_pid_is_alive() {
        let pid = steam_pid();
        // SAFETY: signal zero sends nothing.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0, "pid {pid} is dead");
    }
}
