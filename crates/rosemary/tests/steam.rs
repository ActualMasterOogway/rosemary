use std::ffi::CString;
use std::path::PathBuf;
use std::process::Command;

mod common;

use common::steam::{call, create_interface, generated, method_id, params_with};

const FAKE_CLIENT: &str = r#"
#include <cstdint>
#include <cstring>

namespace {

struct CSteamID { uint64_t v; };

struct ISteamUser
{
    virtual int32_t GetHSteamUser() { return 0x5eed; }
    virtual bool BLoggedOn() { return true; }
    virtual CSteamID GetSteamID() { return CSteamID{0x2721ull}; }
    virtual int32_t InitiateGameConnection(void *blob, int32_t max, CSteamID id,
                                           uint32_t ip, uint16_t port, bool secure)
    {
        if (!blob || memcmp(blob, "auth", 4) != 0) return -1;
        if (max != 99) return -2;
        if (id.v != 0x67ull) return -3;
        if (ip != 0x7f000001u) return -4;
        if (port != 27015) return -5;
        if (!secure) return -6;
        return 0x600d;
    }
};

struct ISteamUserStats
{
    virtual bool RequestCurrentStats() { return true; }
    virtual bool GetStatInt(const char *, int32_t *) { return false; }
    virtual bool GetStatFloat(const char *, float *) { return false; }
    virtual bool SetStatInt(const char *, int32_t) { return false; }
    virtual bool SetStatFloat(const char *name, float value)
    {
        return name && strcmp(name, "speed") == 0 && value == 2.5f;
    }
    virtual bool UpdateAvgRateStat(const char *name, float count, double session)
    {
        return name && strcmp(name, "rate") == 0 && count == 0.5f && session == 4.25;
    }
    virtual bool GetAchievement(const char *, bool *) { return false; }
    virtual bool SetAchievement(const char *) { return false; }
    virtual bool ClearAchievement(const char *) { return false; }
    virtual bool GetAchievementAndUnlockTime(const char *, bool *, uint32_t *) { return false; }
    virtual bool StoreStats() { return false; }
    virtual int32_t GetAchievementIcon(const char *name)
    {
        return name ? (int32_t)strlen(name) * 7 : -1;
    }
};

ISteamUser user;
ISteamUserStats stats;

} // namespace

extern "C" {

void *CreateInterface(const char *name, int *return_code)
{
    if (return_code) *return_code = 0;
    if (!strcmp(name, "SteamUser023")) return &user;
    if (!strcmp(name, "STEAMUSERSTATS_INTERFACE_VERSION012")) return &stats;
    if (return_code) *return_code = 1;
    return nullptr;
}

/* The rest of what the bridge insists a Steam client exports. */
int8_t Steam_BGetCallback(int32_t, void *, int32_t *) { return 0; }
int8_t Steam_FreeLastCallback(int32_t) { return 0; }
int8_t Steam_GetAPICallResult(int32_t, uint64_t, void *, int, int, int8_t *) { return 0; }
void Steam_ReleaseThreadLocalMemory(int) {}
bool Steam_IsKnownInterface(const char *name) { return !strcmp(name, "SteamUser023"); }
void Steam_NotifyMissingInterface(int32_t, const char *) {}

}
"#;

/// Build the stand-in client, or `None` where no host C++ compiler will.
fn build_fake_client() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("steam");
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("fake_steamclient.cpp");
    let out = dir.join("fake_steamclient.so");
    std::fs::write(&src, FAKE_CLIENT).ok()?;
    for compiler in ["c++", "clang++", "g++"] {
        let built = Command::new(compiler)
            .args(["-shared", "-fPIC", "-O1", "-o"])
            .arg(&out)
            .arg(&src)
            .status();
        if matches!(built, Ok(status) if status.success()) {
            return Some(out);
        }
    }
    None
}

#[test]
fn bridges_guest_calls_onto_a_host_steam_client() {
    let Some(generated) = generated() else {
        skip!("tools/steamclient/generated has not been generated");
    };
    let Some(fake) = build_fake_client() else {
        skip!("no host C++ compiler to build the stand-in client");
    };
    let header = std::fs::read_to_string(generated.join("steamclient_methods.h")).unwrap();

    // SAFETY: single-threaded, and before anything in `steam` has read either
    // variable - the bridge loads on its first dispatch, which is below.
    unsafe {
        std::env::set_var("ROSEMARY_STEAM_HOST_CLIENT", &fake);
        std::env::set_var("ROSEMARY_STEAM_CLIENT", generated.join("steamclient.dylib"));
    }

    // `CreateInterface`, which is where every guest starts.
    let (user, code) = create_interface("SteamUser023");
    assert!(!user.is_null(), "the stand-in client returned no interface");
    assert_eq!(code, 0);

    // A scalar return, from vtable slot zero.
    let mut params = params_with(user, &[0u8; 4]);
    let id = method_id(&header, "ISteamUser_SteamUser023_GetHSteamUser");
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(
        i32::from_le_bytes(params[8..12].try_into().unwrap()),
        0x5eed
    );

    // A by-value return: eight bytes, so it comes back in a register.
    let mut params = params_with(user, &[0u8; 8]);
    let id = method_id(&header, "ISteamUser_SteamUser023_GetSteamID");
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(
        u64::from_le_bytes(params[8..16].try_into().unwrap()),
        0x2721
    );

    // Six arguments, one of them a by-value CSteamID and two of them narrower
    // than a register. The stand-in checks every one and says which was wrong.
    let blob = *b"auth";
    let mut args = Vec::new();
    args.extend(0i32.to_le_bytes()); // the return slot
    args.extend((blob.as_ptr() as usize as u64).to_le_bytes());
    args.extend(99i32.to_le_bytes());
    args.extend(0x67u64.to_le_bytes());
    args.extend(0x7f000001u32.to_le_bytes());
    args.extend(27015u16.to_le_bytes());
    args.push(1); // bSecure
    let mut params = params_with(user, &args);
    let id = method_id(
        &header,
        "ISteamUser_SteamUser023_InitiateGameConnection_DEPRECATED",
    );
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(
        i32::from_le_bytes(params[8..12].try_into().unwrap()),
        0x600d,
        "an argument did not arrive as the stand-in expected it"
    );

    // And the second interface, for the arguments that travel in vector
    // registers rather than general-purpose ones.
    let (stats, _) = create_interface("STEAMUSERSTATS_INTERFACE_VERSION012");
    assert!(!stats.is_null());

    let stat = CString::new("speed").unwrap();
    let mut args = vec![0u8]; // the bool return slot
    args.extend((stat.as_ptr() as usize as u64).to_le_bytes());
    args.extend(2.5f32.to_le_bytes());
    let mut params = params_with(stats, &args);
    let id = method_id(
        &header,
        "ISteamUserStats_STEAMUSERSTATS_INTERFACE_VERSION012_SetStat_2",
    );
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(params[8], 1, "the float argument did not arrive");

    let stat = CString::new("rate").unwrap();
    let mut args = vec![0u8];
    args.extend((stat.as_ptr() as usize as u64).to_le_bytes());
    args.extend(0.5f32.to_le_bytes());
    args.extend(4.25f64.to_le_bytes());
    let mut params = params_with(stats, &args);
    let id = method_id(
        &header,
        "ISteamUserStats_STEAMUSERSTATS_INTERFACE_VERSION012_UpdateAvgRateStat",
    );
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(params[8], 1, "a float and a double did not both arrive");

    // A pointer argument at a vtable index well past the first, which is the
    // ordinary shape of every other call a game makes.
    let achievement = CString::new("FIRST_BLOOD").unwrap();
    let mut args = Vec::new();
    args.extend(0i32.to_le_bytes());
    args.extend((achievement.as_ptr() as usize as u64).to_le_bytes());
    let mut params = params_with(stats, &args);
    let id = method_id(
        &header,
        "ISteamUserStats_STEAMUSERSTATS_INTERFACE_VERSION012_GetAchievementIcon",
    );
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(
        i32::from_le_bytes(params[8..12].try_into().unwrap()),
        11 * 7,
        "the vtable index or the pointer argument was wrong"
    );

    // The name server half, which is how a guest gets this far at all.
    #[cfg(not(target_os = "macos"))]
    answers_the_questions_libsteam_api_asks_the_name_server(&generated);

    // An id past the end of the table is the guest being wrong, and must be
    // reported rather than indexed with.
    let mut params = [0u8; 32];
    assert_ne!(call(u32::MAX, &mut params), 0);

    // The client is now loaded, so the loader redirect is live: the Steam
    // client's own name, from whatever directory a guest spells it with, and
    // nothing else.
    assert!(rosemary::steam::redirect("/usr/lib/libSystem.B.dylib").is_none());
    assert_eq!(
        rosemary::steam::redirect("/Users/me/Library/Application Support/Steam/steamclient.dylib"),
        Some(
            generated
                .join("steamclient.dylib")
                .to_string_lossy()
                .into_owned()
        ),
    );
}

#[cfg(not(target_os = "macos"))]
fn answers_the_questions_libsteam_api_asks_the_name_server(generated: &std::path::Path) {
    use darwin::mach::bootstrap::{BOOTSTRAP_PORT, bootstrap_look_up};
    use darwin::mach::msg::{COPY_SEND, MAKE_SEND, mach_msg};
    use darwin::mach::space::{MACH_PORT_NULL, Name, RIGHT_RECEIVE, TASK_SELF, mach_port_allocate};

    const SEND: i32 = 0x1 | 0x10;
    const RECEIVE: i32 = 0x2 | 0x100;
    const VERSION: u32 = 0x68;

    rosemary::steam::install();

    let service_name = c"com.valvesoftware.steam.ipctool";
    let mut service: Name = MACH_PORT_NULL;
    // SAFETY: a NUL-terminated name and a live local for the port.
    let rc = unsafe { bootstrap_look_up(BOOTSTRAP_PORT, service_name.as_ptr(), &mut service) };
    assert_eq!(rc, 0, "the Steam IPC service was not published");

    let mut reply_port: Name = MACH_PORT_NULL;
    // SAFETY: a live local for the port.
    let rc = unsafe { mach_port_allocate(TASK_SELF, RIGHT_RECEIVE, &mut reply_port) };
    assert_eq!(rc, 0);

    let ask = |command: u32, argument: u32, reply: &mut [u8]| {
        const REQUEST: usize = 32;
        let mut request = [0u8; REQUEST];
        let put = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
        // `MACH_MSGH_BITS(COPY_SEND, MAKE_SEND)`, which is what `Send` ors in.
        put(&mut request, 0, COPY_SEND | (MAKE_SEND << 8));
        put(&mut request, 4, REQUEST as u32);
        put(&mut request, 8, service);
        put(&mut request, 12, reply_port);
        put(&mut request, 20, VERSION);
        put(&mut request, 24, command);
        put(&mut request, 28, argument);
        // SAFETY: `request` is exactly the size passed, and nothing is received.
        let rc = unsafe {
            mach_msg(
                request.as_mut_ptr().cast(),
                SEND,
                request.len() as u32,
                0,
                MACH_PORT_NULL,
                1000,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(rc, 0, "command {command:#x} could not be sent");
        // SAFETY: `reply` is writable for the size passed.
        let rc = unsafe {
            mach_msg(
                reply.as_mut_ptr().cast(),
                RECEIVE,
                0,
                reply.len() as u32,
                reply_port,
                5000,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(rc, 0, "command {command:#x} went unanswered");
        // The caller receives in a loop until it sees its own version back.
        assert_eq!(
            u32::from_ne_bytes(reply[20..24].try_into().unwrap()),
            VERSION,
            "the reply to {command:#x} is not the one the caller waits for"
        );
        u32::from_ne_bytes(reply[28..32].try_into().unwrap())
    };

    // The version. Anything lower than the caller's and it kills the server and
    // waits for a newer one, which nothing here could start.
    let mut reply = [0u8; 0x28];
    assert!(
        ask(0x0001_86a1, VERSION, &mut reply) >= VERSION,
        "an older version would have this service killed rather than used"
    );

    // Where Steam is, and whether it is alive. The caller replaces the last
    // path component with `steamclient.dylib`, so what has to come back is the
    // directory holding the generated one.
    let mut reply = [0u8; 0x228];
    let pid = ask(0x0000_000e, 0, &mut reply);
    // SAFETY: signal zero sends nothing; a dead pid is what the caller rejects.
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0, "pid {pid} is dead");

    let path = std::ffi::CStr::from_bytes_until_nul(&reply[32..]).expect("an unterminated path");
    let path = std::path::Path::new(std::str::from_utf8(path.to_bytes()).unwrap());
    assert_eq!(
        path.parent(),
        Some(generated),
        "the guest would build {}/steamclient.dylib, which is not the generated one",
        path.parent().unwrap_or(path).display()
    );
}
