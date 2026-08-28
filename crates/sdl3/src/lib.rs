use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

pub mod init {
    pub const VIDEO: u32 = 0x0000_0020;
    pub const AUDIO: u32 = 0x0000_0010;
    pub const JOYSTICK: u32 = 0x0000_0200;
    pub const GAMEPAD: u32 = 0x0000_2000;
}

pub type Joystick = *mut c_void;
pub type JoystickId = u32;

/// The entry points this tree calls.
///
/// Each is `Option`, and a missing one is a name this build of SDL does not
/// have - which is reported rather than called, since a null call would be a
/// crash with nothing to say which symbol was absent.
#[allow(non_snake_case)]
struct Api {
    SDL_InitSubSystem: Option<unsafe extern "C" fn(u32) -> bool>,
    SDL_GetError: Option<unsafe extern "C" fn() -> *const c_char>,
    SDL_PumpEvents: Option<unsafe extern "C" fn()>,

    SDL_GetJoysticks: Option<unsafe extern "C" fn(*mut c_int) -> *mut JoystickId>,
    SDL_OpenJoystick: Option<unsafe extern "C" fn(JoystickId) -> Joystick>,
    SDL_CloseJoystick: Option<unsafe extern "C" fn(Joystick)>,
    SDL_GetJoystickName: Option<unsafe extern "C" fn(Joystick) -> *const c_char>,
    SDL_GetJoystickVendor: Option<unsafe extern "C" fn(Joystick) -> u16>,
    SDL_GetJoystickProduct: Option<unsafe extern "C" fn(Joystick) -> u16>,
    SDL_GetJoystickProductVersion: Option<unsafe extern "C" fn(Joystick) -> u16>,
    SDL_GetNumJoystickAxes: Option<unsafe extern "C" fn(Joystick) -> c_int>,
    SDL_GetNumJoystickButtons: Option<unsafe extern "C" fn(Joystick) -> c_int>,
    SDL_GetNumJoystickHats: Option<unsafe extern "C" fn(Joystick) -> c_int>,
    SDL_GetJoystickAxis: Option<unsafe extern "C" fn(Joystick, c_int) -> i16>,
    SDL_GetJoystickButton: Option<unsafe extern "C" fn(Joystick, c_int) -> bool>,
    SDL_GetJoystickHat: Option<unsafe extern "C" fn(Joystick, c_int) -> u8>,
    SDL_free: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// The library, opened once.
///
/// Several sonames: a host may have any of them, and asking for each in turn is
/// cheaper than requiring one.
fn library() -> Option<*mut c_void> {
    static HANDLE: OnceLock<usize> = OnceLock::new();
    let handle = *HANDLE.get_or_init(|| {
        for name in [
            c"libSDL3.so.0",
            c"libSDL3.so",
            c"libSDL3.0.dylib",
            c"libSDL3.dylib",
            c"/opt/homebrew/lib/libSDL3.dylib",
            c"/usr/local/lib/libSDL3.dylib",
        ] {
            // SAFETY: a NUL-terminated soname. `RTLD_GLOBAL` and never closed,
            // for the reason every other `dlopen` in this tree is: a pointer
            // already handed out must not dangle.
            let handle =
                unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
            if !handle.is_null() {
                return handle as usize;
            }
        }
        0
    });
    (handle != 0).then_some(handle as *mut c_void)
}

/// Resolve one entry point by name.
///
/// # Safety
///
/// `T` must be the prototype SDL declares for `name`.
unsafe fn entry<T: Copy>(handle: *mut c_void, name: &str) -> Option<T> {
    const _: () = assert!(size_of::<*const c_void>() == 8);
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: a handle from `dlopen` and a NUL-terminated name.
    let found = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if found.is_null() {
        return None;
    }
    assert_eq!(
        size_of::<T>(),
        size_of::<*const c_void>(),
        "an entry point is a bare function pointer"
    );
    // SAFETY: the caller's obligation, and the size was just checked.
    Some(unsafe { *(&found as *const *mut c_void as *const T) })
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let handle = library()?;
        /// Resolve every field by its own name, which is SDL's spelling.
        macro_rules! resolve {
            ($($name:ident),* $(,)?) => {
                Api {
                    // SAFETY: each field's type above is the prototype SDL3's
                    // headers declare for that name.
                    $($name: unsafe { entry(handle, stringify!($name)) },)*
                }
            };
        }
        Some(resolve!(
            SDL_InitSubSystem,
            SDL_GetError,
            SDL_PumpEvents,
            SDL_GetJoysticks,
            SDL_OpenJoystick,
            SDL_CloseJoystick,
            SDL_GetJoystickName,
            SDL_GetJoystickVendor,
            SDL_GetJoystickProduct,
            SDL_GetJoystickProductVersion,
            SDL_GetNumJoystickAxes,
            SDL_GetNumJoystickButtons,
            SDL_GetNumJoystickHats,
            SDL_GetJoystickAxis,
            SDL_GetJoystickButton,
            SDL_GetJoystickHat,
            SDL_free,
        ))
    })
    .as_ref()
}

pub fn available() -> bool {
    api().is_some()
}

pub fn refusal() -> &'static str {
    "this host has no SDL3: install it from your package manager"
}

/// SDL's last error, for a diagnostic.
pub fn error() -> String {
    let Some(api) = api() else {
        return refusal().to_string();
    };
    let Some(get) = api.SDL_GetError else {
        return String::new();
    };
    // SAFETY: `SDL_GetError` returns a NUL-terminated string SDL owns.
    let text = unsafe { get() };
    match text.is_null() {
        true => String::new(),
        // SAFETY: as above.
        false => unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned(),
    }
}

/// Bring up one or more subsystems, or say why not.
///
/// Idempotent per subsystem: SDL counts them, and this tree may want the
/// joystick one from IOKit and the video one from AppKit without either knowing
/// about the other.
pub fn init(flags: u32) -> Result<(), String> {
    let Some(api) = api() else {
        return Err(refusal().to_string());
    };
    let Some(sub) = api.SDL_InitSubSystem else {
        return Err("this SDL3 has no SDL_InitSubSystem".to_string());
    };
    // SAFETY: takes a flag word and nothing else. `SDL_InitSubSystem` brings
    // SDL up on its own if nothing has yet, and counts a subsystem already
    // running rather than restarting it - which is what lets IOKit ask for the
    // joysticks and AppKit for the video without either knowing about the other.
    let ok = unsafe { sub(flags) };
    match ok {
        true => Ok(()),
        false => Err(error()),
    }
}

/// Service SDL's own event queue, which is what makes the state below change.
pub fn pump() {
    if let Some(pump) = api().and_then(|a| a.SDL_PumpEvents) {
        // SAFETY: takes nothing, and must be called on the thread that
        // initialised the subsystem - which is this tree's main thread.
        unsafe { pump() };
    }
}

/// The joystick instance ids attached right now.
pub fn joysticks() -> Vec<JoystickId> {
    let Some(api) = api() else {
        return Vec::new();
    };
    let (Some(get), Some(free)) = (api.SDL_GetJoysticks, api.SDL_free) else {
        return Vec::new();
    };
    let mut count: c_int = 0;
    // SAFETY: `SDL_GetJoysticks` writes the count and returns an array of that
    // many ids, allocated with SDL's own allocator and freed below.
    unsafe {
        let ids = get(&mut count);
        if ids.is_null() || count <= 0 {
            if !ids.is_null() {
                free(ids as *mut c_void);
            }
            return Vec::new();
        }
        let out = std::slice::from_raw_parts(ids, count as usize).to_vec();
        free(ids as *mut c_void);
        out
    }
}

/// Open a joystick, or `None`.
pub fn open(id: JoystickId) -> Option<Joystick> {
    let open = api()?.SDL_OpenJoystick?;
    // SAFETY: an id `SDL_GetJoysticks` reported.
    let joystick = unsafe { open(id) };
    (!joystick.is_null()).then_some(joystick)
}

/// # Safety
///
/// `joystick` must be one [`open`] returned and not yet closed.
pub unsafe fn close(joystick: Joystick) {
    if let Some(close) = api().and_then(|a| a.SDL_CloseJoystick) {
        // SAFETY: the caller's obligation.
        unsafe { close(joystick) };
    }
}

/// A joystick's name, or empty.
///
/// # Safety
///
/// As [`close`].
pub unsafe fn name(joystick: Joystick) -> String {
    let Some(get) = api().and_then(|a| a.SDL_GetJoystickName) else {
        return String::new();
    };
    // SAFETY: the caller's obligation; the string is SDL's and NUL-terminated.
    let text = unsafe { get(joystick) };
    match text.is_null() {
        true => String::new(),
        // SAFETY: as above.
        false => unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned(),
    }
}

/// Build a getter that takes a joystick and answers a number.
macro_rules! getter {
    ($(#[$meta:meta])* $name:ident, $field:ident, $ty:ty, $default:expr) => {
        $(#[$meta])*
        ///
        /// # Safety
        ///
        /// As [`close`].
        pub unsafe fn $name(joystick: Joystick) -> $ty {
            match api().and_then(|a| a.$field) {
                // SAFETY: the caller's obligation.
                Some(f) => unsafe { f(joystick) },
                None => $default,
            }
        }
    };
}

getter!(/// The USB vendor id, or zero for a device that reports none.
    vendor, SDL_GetJoystickVendor, u16, 0);
getter!(/// The USB product id.
    product, SDL_GetJoystickProduct, u16, 0);
getter!(/// The device's version, which is the third word of a HID GUID.
    version, SDL_GetJoystickProductVersion, u16, 0);
getter!(/// How many axes it has.
    axes, SDL_GetNumJoystickAxes, c_int, 0);
getter!(/// How many buttons it has.
    buttons, SDL_GetNumJoystickButtons, c_int, 0);
getter!(/// How many hats it has.
    hats, SDL_GetNumJoystickHats, c_int, 0);

/// One axis, in SDL's own `-32768..32767`.
///
/// # Safety
///
/// As [`close`].
pub unsafe fn axis(joystick: Joystick, index: c_int) -> i16 {
    match api().and_then(|a| a.SDL_GetJoystickAxis) {
        // SAFETY: the caller's obligation.
        Some(f) => unsafe { f(joystick, index) },
        None => 0,
    }
}

/// One button, pressed or not.
///
/// # Safety
///
/// As [`close`].
pub unsafe fn button(joystick: Joystick, index: c_int) -> bool {
    match api().and_then(|a| a.SDL_GetJoystickButton) {
        // SAFETY: the caller's obligation.
        Some(f) => unsafe { f(joystick, index) },
        None => false,
    }
}

/// One hat, as SDL's bitfield: 1 up, 2 right, 4 down, 8 left.
///
/// # Safety
///
/// As [`close`].
pub unsafe fn hat(joystick: Joystick, index: c_int) -> u8 {
    match api().and_then(|a| a.SDL_GetJoystickHat) {
        // SAFETY: the caller's obligation.
        Some(f) => unsafe { f(joystick, index) },
        None => 0,
    }
}

pub mod virtual_joystick {
    use super::*;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct Desc {
        pub version: u32,
        pub kind: u16,
        pub padding: u16,
        pub vendor_id: u16,
        pub product_id: u16,
        pub naxes: u16,
        pub nbuttons: u16,
        pub nballs: u16,
        pub nhats: u16,
        pub ntouchpads: u16,
        pub nsensors: u16,
        pub padding2: [u16; 2],
        pub button_mask: u32,
        pub axis_mask: u32,
        pub name: *const c_char,
        pub touchpads: *const c_void,
        pub sensors: *const c_void,
        pub userdata: *mut c_void,
        pub update: Option<unsafe extern "C" fn(*mut c_void)>,
        pub set_player_index: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
        pub rumble: Option<unsafe extern "C" fn(*mut c_void, u16, u16) -> bool>,
        pub rumble_triggers: Option<unsafe extern "C" fn(*mut c_void, u16, u16) -> bool>,
        pub set_led: Option<unsafe extern "C" fn(*mut c_void, u8, u8, u8) -> bool>,
        pub send_effect: Option<unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> bool>,
        pub set_sensors_enabled: Option<unsafe extern "C" fn(*mut c_void, bool) -> bool>,
        pub cleanup: Option<unsafe extern "C" fn(*mut c_void)>,
    }

    pub fn attach(desc: &Desc) -> Option<JoystickId> {
        let handle = library()?;
        let mut desc = Desc {
            version: size_of::<Desc>() as u32,
            ..*desc
        };
        let desc = &mut desc;
        // SAFETY: the prototype SDL3 declares for this name.
        let attach: unsafe extern "C" fn(*const Desc) -> JoystickId =
            unsafe { entry(handle, "SDL_AttachVirtualJoystick")? };
        // SAFETY: a descriptor this caller owns, which SDL copies.
        let id = unsafe { attach(desc) };
        (id != 0).then_some(id)
    }

    pub fn detach(id: JoystickId) -> bool {
        let Some(handle) = library() else {
            return false;
        };
        // SAFETY: as above.
        let Some(detach) = (unsafe {
            entry::<unsafe extern "C" fn(JoystickId) -> bool>(handle, "SDL_DetachVirtualJoystick")
        }) else {
            return false;
        };
        // SAFETY: an id `attach` returned.
        unsafe { detach(id) }
    }

    /// Set one axis of a virtual joystick.
    ///
    /// # Safety
    ///
    /// `joystick` must be an open virtual joystick.
    pub unsafe fn set_axis(joystick: Joystick, axis: c_int, value: i16) -> bool {
        let Some(handle) = library() else {
            return false;
        };
        // SAFETY: the prototype SDL3 declares.
        let Some(set) = (unsafe {
            entry::<unsafe extern "C" fn(Joystick, c_int, i16) -> bool>(
                handle,
                "SDL_SetJoystickVirtualAxis",
            )
        }) else {
            return false;
        };
        // SAFETY: the caller's obligation.
        unsafe { set(joystick, axis, value) }
    }

    /// Set one button.
    ///
    /// # Safety
    ///
    /// As [`set_axis`].
    pub unsafe fn set_button(joystick: Joystick, button: c_int, down: bool) -> bool {
        let Some(handle) = library() else {
            return false;
        };
        // SAFETY: the prototype SDL3 declares.
        let Some(set) = (unsafe {
            entry::<unsafe extern "C" fn(Joystick, c_int, bool) -> bool>(
                handle,
                "SDL_SetJoystickVirtualButton",
            )
        }) else {
            return false;
        };
        // SAFETY: the caller's obligation.
        unsafe { set(joystick, button, down) }
    }

    /// SDL asserts this size at compile time, so this tree asserts it too: a
    /// field out of place would attach a joystick with the wrong shape, and
    /// every assertion downstream would be about the wrong thing.
    const _: () = assert!(size_of::<Desc>() == 136 || size_of::<*const c_void>() == 4);

    /// Whether this SDL has the virtual-joystick API at all.
    pub fn available() -> bool {
        library().is_some_and(|handle| {
            // SAFETY: only the presence of the name is read.
            unsafe {
                entry::<unsafe extern "C" fn(*const Desc) -> JoystickId>(
                    handle,
                    "SDL_AttachVirtualJoystick",
                )
            }
            .is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_absence_of_sdl_is_an_answer_rather_than_a_crash() {
        if !available() {
            assert!(joysticks().is_empty());
            assert!(init(init::JOYSTICK).is_err());
            assert_eq!(error(), refusal());
            return;
        }
        // Present: the joystick subsystem comes up, and enumerating finds
        // whatever is attached - which on a build machine is nothing.
        init(init::JOYSTICK).expect("the joystick subsystem");
        pump();
        let _ = joysticks();
    }

    #[test]
    fn a_virtual_joystick_attaches_and_reads_back() {
        if !available() || !virtual_joystick::available() {
            return;
        }
        init(init::JOYSTICK).expect("the joystick subsystem");

        let label = c"rosetta3 test pad";
        let desc = virtual_joystick::Desc {
            kind: 1,
            vendor_id: 0x1234,
            product_id: 0x5678,
            naxes: 6,
            nbuttons: 15,
            nhats: 1,
            name: label.as_ptr(),
            ..Default::default()
        };
        let Some(id) = virtual_joystick::attach(&desc) else {
            panic!("SDL declined to attach a virtual joystick: {}", error());
        };
        pump();
        assert!(joysticks().contains(&id), "the attached joystick is not listed");

        let joystick = open(id).expect("the joystick opens");
        // SAFETY: a joystick just opened, closed at the end of this test.
        unsafe {
            assert_eq!(name(joystick), "rosetta3 test pad");
            assert_eq!(vendor(joystick), 0x1234);
            assert_eq!(product(joystick), 0x5678);
            assert_eq!(axes(joystick), 6);
            assert_eq!(buttons(joystick), 15);
            assert_eq!(hats(joystick), 1);

            assert!(virtual_joystick::set_axis(joystick, 0, 12_345));
            assert!(virtual_joystick::set_button(joystick, 3, true));
            pump();
            assert_eq!(axis(joystick, 0), 12_345);
            assert!(button(joystick, 3));
            assert!(!button(joystick, 4));

            close(joystick);
        }
        virtual_joystick::detach(id);
    }
}
