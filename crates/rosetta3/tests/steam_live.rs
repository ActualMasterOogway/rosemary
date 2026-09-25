use std::ffi::{CStr, CString, c_void};

mod common;

use common::steam::{call, create_interface, generated, method_id, params_with};

/// Set to run this. Off by default: the suite must not depend on a signed-in
/// Steam, and a failure here would otherwise be about the machine rather than
/// about the tree.
const LIVE_VAR: &str = "ROSETTA3_STEAM_LIVE_TESTS";

#[test]
fn reaches_the_signed_in_user_through_the_real_steam_client() {
    if std::env::var_os(LIVE_VAR).is_none() {
        skip!("{LIVE_VAR} is not set");
    }
    let Some(generated) = generated() else {
        skip!("tools/steamclient/generated has not been generated");
    };
    let header = std::fs::read_to_string(generated.join("steamclient_methods.h")).unwrap();
    // SAFETY: single-threaded, and before the bridge has read the variable -
    // it loads on the first dispatch, which is below.
    unsafe {
        std::env::set_var("ROSETTA3_STEAM_CLIENT", generated.join("steamclient.dylib"));
    }

    let (client, code) = create_interface("SteamClient020");
    assert!(
        !client.is_null(),
        "the host client returned no SteamClient020 (return code {code})"
    );

    // `CreateSteamPipe` then `ConnectToGlobalUser`, which is the pair every
    // `SteamAPI_Init` makes before it asks for anything else.
    let mut params = params_with(client, &[0u8; 4]);
    let id = method_id(&header, "ISteamClient_SteamClient020_CreateSteamPipe");
    assert_eq!(call(id, &mut params), 0);
    let pipe = i32::from_le_bytes(params[8..12].try_into().unwrap());
    assert_ne!(
        pipe, 0,
        "the host client handed out no pipe: is Steam running?"
    );

    let mut rest = [0u8; 8];
    rest[4..].copy_from_slice(&pipe.to_le_bytes());
    let mut params = params_with(client, &rest);
    let id = method_id(&header, "ISteamClient_SteamClient020_ConnectToGlobalUser");
    assert_eq!(call(id, &mut params), 0);
    let user = i32::from_le_bytes(params[8..12].try_into().unwrap());
    assert_ne!(user, 0, "no signed-in user on pipe {pipe}");

    // An interface reached through another interface, which is the shape the
    // guest's own wrapper has to unpick.
    let version = CString::new("SteamUser023").unwrap();
    let mut rest = Vec::from([0u8; 8]);
    rest.extend(user.to_le_bytes());
    rest.extend(pipe.to_le_bytes());
    rest.extend((version.as_ptr() as usize as u64).to_le_bytes());
    let mut params = params_with(client, &rest);
    let id = method_id(&header, "ISteamClient_SteamClient020_GetISteamUser");
    assert_eq!(call(id, &mut params), 0);
    let steam_user = u64::from_le_bytes(params[8..16].try_into().unwrap()) as *mut c_void;
    assert!(!steam_user.is_null(), "no ISteamUser for user {user}");

    // A by-value return from the real client: eight bytes in a register.
    let mut params = params_with(steam_user, &[0u8; 8]);
    let id = method_id(&header, "ISteamUser_SteamUser023_GetSteamID");
    assert_eq!(call(id, &mut params), 0);
    let steam_id = u64::from_le_bytes(params[8..16].try_into().unwrap());
    assert_ne!(
        steam_id, 0,
        "the client reported no SteamID: is Steam signed in?"
    );
    // The universe and account-type nibbles of an individual account, which is
    // what says the value is a SteamID rather than whatever was in the register.
    assert_eq!(
        steam_id >> 52,
        0x011,
        "{steam_id:#x} is not an individual SteamID"
    );

    let mut params = params_with(steam_user, &[0u8; 1]);
    let id = method_id(&header, "ISteamUser_SteamUser023_BLoggedOn");
    assert_eq!(call(id, &mut params), 0);
    assert_eq!(params[8], 1, "the client says it is not logged on");

    // And a string back out of a second interface, so the answer is one a
    // person can recognise rather than a handle.
    let version = CString::new("SteamFriends017").unwrap();
    let mut rest = Vec::from([0u8; 8]);
    rest.extend(user.to_le_bytes());
    rest.extend(pipe.to_le_bytes());
    rest.extend((version.as_ptr() as usize as u64).to_le_bytes());
    let mut params = params_with(client, &rest);
    let id = method_id(&header, "ISteamClient_SteamClient020_GetISteamFriends");
    assert_eq!(call(id, &mut params), 0);
    let friends = u64::from_le_bytes(params[8..16].try_into().unwrap()) as *mut c_void;
    assert!(!friends.is_null());

    let mut params = params_with(friends, &[0u8; 8]);
    let id = method_id(&header, "ISteamFriends_SteamFriends017_GetPersonaName");
    assert_eq!(call(id, &mut params), 0);
    let name = u64::from_le_bytes(params[8..16].try_into().unwrap()) as *const i8;
    assert!(!name.is_null(), "no persona name");
    // SAFETY: the client returns a NUL-terminated string it owns.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    assert!(!name.is_empty(), "the persona name is empty");
    eprintln!("bridged to {name} ({steam_id:#x}) on pipe {pipe}");
}
