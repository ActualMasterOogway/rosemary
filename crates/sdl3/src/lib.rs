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
pub type Window = *mut c_void;
pub type GlContext = *mut c_void;

/// `SDL_Rect`: a display's bounds, in the desktop's own coordinates.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: c_int,
    pub y: c_int,
    pub w: c_int,
    pub h: c_int,
}

/// `SDL_Event`, as bytes.
///
/// The union is a hundred and twenty-eight bytes of variants this tree does not
/// need to name: the fields it reads are at fixed offsets in the few structs it
/// cares about, and declaring the rest would be a copy of a header that changes
/// without this crate hearing about it. [`video::Event`] is the decoded form.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct Event {
    pub bytes: [u8; 128],
}

impl Default for Event {
    fn default() -> Self {
        Event { bytes: [0; 128] }
    }
}

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

    SDL_CreateWindow: Option<unsafe extern "C" fn(*const c_char, c_int, c_int, u64) -> Window>,
    SDL_DestroyWindow: Option<unsafe extern "C" fn(Window)>,
    SDL_SetWindowTitle: Option<unsafe extern "C" fn(Window, *const c_char) -> bool>,
    SDL_GetWindowSize: Option<unsafe extern "C" fn(Window, *mut c_int, *mut c_int) -> bool>,
    SDL_SetWindowSize: Option<unsafe extern "C" fn(Window, c_int, c_int) -> bool>,
    SDL_GetWindowPosition: Option<unsafe extern "C" fn(Window, *mut c_int, *mut c_int) -> bool>,
    SDL_SetWindowPosition: Option<unsafe extern "C" fn(Window, c_int, c_int) -> bool>,
    SDL_ShowWindow: Option<unsafe extern "C" fn(Window) -> bool>,
    SDL_HideWindow: Option<unsafe extern "C" fn(Window) -> bool>,
    SDL_RaiseWindow: Option<unsafe extern "C" fn(Window) -> bool>,
    SDL_SetWindowFullscreen: Option<unsafe extern "C" fn(Window, bool) -> bool>,
    SDL_SetWindowResizable: Option<unsafe extern "C" fn(Window, bool) -> bool>,
    SDL_GetWindowPixelDensity: Option<unsafe extern "C" fn(Window) -> f32>,

    SDL_GL_CreateContext: Option<unsafe extern "C" fn(Window) -> GlContext>,
    SDL_GL_DestroyContext: Option<unsafe extern "C" fn(GlContext) -> bool>,
    SDL_GL_MakeCurrent: Option<unsafe extern "C" fn(Window, GlContext) -> bool>,
    SDL_GL_SwapWindow: Option<unsafe extern "C" fn(Window) -> bool>,
    SDL_GL_SetAttribute: Option<unsafe extern "C" fn(c_int, c_int) -> bool>,
    SDL_GL_SetSwapInterval: Option<unsafe extern "C" fn(c_int) -> bool>,
    SDL_GL_GetProcAddress: Option<unsafe extern "C" fn(*const c_char) -> *mut c_void>,

    SDL_GetPrimaryDisplay: Option<unsafe extern "C" fn() -> u32>,
    SDL_GetDisplays: Option<unsafe extern "C" fn(*mut c_int) -> *mut u32>,
    SDL_GetDisplayBounds: Option<unsafe extern "C" fn(u32, *mut Rect) -> bool>,
    SDL_GetDisplayUsableBounds: Option<unsafe extern "C" fn(u32, *mut Rect) -> bool>,
    SDL_GetDisplayContentScale: Option<unsafe extern "C" fn(u32) -> f32>,

    SDL_PollEvent: Option<unsafe extern "C" fn(*mut Event) -> bool>,
    SDL_HideCursor: Option<unsafe extern "C" fn() -> bool>,
    SDL_ShowCursor: Option<unsafe extern "C" fn() -> bool>,
    SDL_GetGlobalMouseState: Option<unsafe extern "C" fn(*mut f32, *mut f32) -> u32>,
    SDL_GetMouseState: Option<unsafe extern "C" fn(*mut f32, *mut f32) -> u32>,
    SDL_WarpMouseGlobal: Option<unsafe extern "C" fn(f32, f32) -> bool>,
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
            SDL_CreateWindow,
            SDL_DestroyWindow,
            SDL_SetWindowTitle,
            SDL_GetWindowSize,
            SDL_SetWindowSize,
            SDL_GetWindowPosition,
            SDL_SetWindowPosition,
            SDL_ShowWindow,
            SDL_HideWindow,
            SDL_RaiseWindow,
            SDL_SetWindowFullscreen,
            SDL_SetWindowResizable,
            SDL_GetWindowPixelDensity,
            SDL_GL_CreateContext,
            SDL_GL_DestroyContext,
            SDL_GL_MakeCurrent,
            SDL_GL_SwapWindow,
            SDL_GL_SetAttribute,
            SDL_GL_SetSwapInterval,
            SDL_GL_GetProcAddress,
            SDL_GetPrimaryDisplay,
            SDL_GetDisplays,
            SDL_GetDisplayBounds,
            SDL_GetDisplayUsableBounds,
            SDL_GetDisplayContentScale,
            SDL_PollEvent,
            SDL_HideCursor,
            SDL_ShowCursor,
            SDL_GetGlobalMouseState,
            SDL_GetMouseState,
            SDL_WarpMouseGlobal,
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

/// The window, the GL context, the displays, and the event queue.
///
/// A thin layer: every function here is one SDL call and a null check, because
/// the decisions belong to `crates/appkit` - what a window's coordinates mean,
/// which events become an `NSEvent`, when a context is current. What is *not*
/// thin is [`Event`](video::Event), which is decoded here: `SDL_Event` is a
/// union whose members are this crate's business to know and nobody else's.
pub mod video {
    use super::*;

    /// `SDL_WindowFlags`.
    pub mod flags {
        pub const FULLSCREEN: u64 = 0x0000_0001;
        pub const OPENGL: u64 = 0x0000_0002;
        pub const HIDDEN: u64 = 0x0000_0008;
        pub const BORDERLESS: u64 = 0x0000_0010;
        pub const RESIZABLE: u64 = 0x0000_0020;
        pub const HIGH_PIXEL_DENSITY: u64 = 0x0000_2000;
    }

    /// `SDL_GLAttr`, in declaration order - the enum has no assigned values, so
    /// the ordinal *is* the interface.
    pub mod attr {
        pub const RED_SIZE: i32 = 0;
        pub const GREEN_SIZE: i32 = 1;
        pub const BLUE_SIZE: i32 = 2;
        pub const ALPHA_SIZE: i32 = 3;
        pub const BUFFER_SIZE: i32 = 4;
        pub const DOUBLEBUFFER: i32 = 5;
        pub const DEPTH_SIZE: i32 = 6;
        pub const STENCIL_SIZE: i32 = 7;
        pub const STEREO: i32 = 12;
        pub const MULTISAMPLEBUFFERS: i32 = 13;
        pub const MULTISAMPLESAMPLES: i32 = 14;
        pub const ACCELERATED_VISUAL: i32 = 15;
        pub const CONTEXT_MAJOR_VERSION: i32 = 17;
        pub const CONTEXT_MINOR_VERSION: i32 = 18;
        pub const CONTEXT_PROFILE_MASK: i32 = 20;
    }

    /// `SDL_GLProfile`.
    pub mod profile {
        pub const CORE: i32 = 0x0001;
        pub const COMPATIBILITY: i32 = 0x0002;
    }

    /// The `SDL_EventType` values this tree acts on.
    pub mod event_type {
        pub const QUIT: u32 = 0x100;
        pub const WINDOW_RESIZED: u32 = 0x206;
        pub const WINDOW_FOCUS_GAINED: u32 = 0x20e;
        pub const WINDOW_FOCUS_LOST: u32 = 0x20f;
        pub const WINDOW_CLOSE_REQUESTED: u32 = 0x210;
        pub const KEY_DOWN: u32 = 0x300;
        pub const KEY_UP: u32 = 0x301;
        pub const TEXT_INPUT: u32 = 0x303;
        pub const MOUSE_MOTION: u32 = 0x400;
        pub const MOUSE_BUTTON_DOWN: u32 = 0x401;
        pub const MOUSE_BUTTON_UP: u32 = 0x402;
        pub const MOUSE_WHEEL: u32 = 0x403;
    }

    /// `SDL_WindowEvent`, whose head every other event shares.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct WindowEvent {
        kind: u32,
        reserved: u32,
        timestamp: u64,
        window: u32,
        data1: i32,
        data2: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyboardEvent {
        kind: u32,
        reserved: u32,
        timestamp: u64,
        window: u32,
        which: u32,
        scancode: u32,
        key: u32,
        modifiers: u16,
        raw: u16,
        down: bool,
        repeat: bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseMotionEvent {
        kind: u32,
        reserved: u32,
        timestamp: u64,
        window: u32,
        which: u32,
        state: u32,
        x: f32,
        y: f32,
        xrel: f32,
        yrel: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseButtonEvent {
        kind: u32,
        reserved: u32,
        timestamp: u64,
        window: u32,
        which: u32,
        button: u8,
        down: bool,
        clicks: u8,
        padding: u8,
        x: f32,
        y: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseWheelEvent {
        kind: u32,
        reserved: u32,
        timestamp: u64,
        window: u32,
        which: u32,
        x: f32,
        y: f32,
        direction: u32,
        mouse_x: f32,
        mouse_y: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TextInputEvent {
        kind: u32,
        reserved: u32,
        timestamp: u64,
        window: u32,
        text: *const c_char,
    }

    // The offsets these structures put their fields at, checked against the
    // ones SDL's own headers compute. A member read at the wrong offset is a
    // coordinate or a keycode that is merely *wrong*, which is the failure this
    // whole file is arranged to avoid - and the window-event tags above were
    // three off by one when they were counted by hand rather than printed.
    const _: () = {
        assert!(size_of::<super::Event>() == 128);
        assert!(std::mem::offset_of!(KeyboardEvent, scancode) == 0x18);
        assert!(std::mem::offset_of!(KeyboardEvent, down) == 0x24);
        assert!(size_of::<KeyboardEvent>() == 0x28);
        assert!(std::mem::offset_of!(MouseButtonEvent, x) == 0x1c);
        assert!(std::mem::offset_of!(MouseMotionEvent, xrel) == 0x24);
        assert!(std::mem::offset_of!(TextInputEvent, text) == 0x18);
        // `SDL_MouseWheelEvent` grew `integer_x`/`integer_y` in 3.2.12, which is
        // why this one is shorter than SDL's: the head is what is read.
        assert!(std::mem::offset_of!(MouseWheelEvent, y) == 0x1c);
    };

    /// One event, decoded.
    ///
    /// Every window-relative coordinate is in SDL's own frame: y down from the
    /// top-left. Flipping is AppKit's, and doing it here would flip it twice for
    /// the one caller that wants the desktop's.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Event {
        Quit,
        WindowResized { window: u32, width: i32, height: i32 },
        WindowClosed { window: u32 },
        WindowFocus { window: u32, gained: bool },
        Key { window: u32, scancode: u32, key: u32, modifiers: u16, down: bool, repeat: bool },
        Text { window: u32, text: String },
        MouseMotion { window: u32, x: f32, y: f32, dx: f32, dy: f32, buttons: u32 },
        MouseButton { window: u32, button: u8, down: bool, clicks: u8, x: f32, y: f32 },
        MouseWheel { window: u32, x: f32, y: f32 },
        /// One SDL delivers that nothing here acts on, kept so a caller can see
        /// that the queue moved.
        Other(u32),
    }

    /// Read `T` out of a raw event.
    ///
    /// # Safety
    ///
    /// `T` must be the SDL structure the event's type tag names.
    unsafe fn decode<T: Copy>(raw: &super::Event) -> T {
        const _: () = assert!(size_of::<super::Event>() == 128);
        assert!(size_of::<T>() <= 128, "an event member is inside the union");
        // SAFETY: the caller's obligation, and the size was just checked.
        unsafe { *(raw.bytes.as_ptr() as *const T) }
    }

    fn kind(raw: &super::Event) -> u32 {
        u32::from_ne_bytes([raw.bytes[0], raw.bytes[1], raw.bytes[2], raw.bytes[3]])
    }

    /// The next event, or `None` when the queue is empty.
    pub fn poll() -> Option<Event> {
        let poll = api()?.SDL_PollEvent?;
        let mut raw = super::Event::default();
        // SAFETY: a writable event of SDL's own size.
        if !unsafe { poll(&mut raw) } {
            return None;
        }
        let tag = kind(&raw);
        // SAFETY: each arm names the structure SDL documents for that tag.
        Some(unsafe {
            match tag {
                event_type::QUIT => Event::Quit,
                event_type::WINDOW_RESIZED => {
                    let e: WindowEvent = decode(&raw);
                    Event::WindowResized { window: e.window, width: e.data1, height: e.data2 }
                }
                event_type::WINDOW_CLOSE_REQUESTED => {
                    let e: WindowEvent = decode(&raw);
                    Event::WindowClosed { window: e.window }
                }
                event_type::WINDOW_FOCUS_GAINED | event_type::WINDOW_FOCUS_LOST => {
                    let e: WindowEvent = decode(&raw);
                    Event::WindowFocus {
                        window: e.window,
                        gained: tag == event_type::WINDOW_FOCUS_GAINED,
                    }
                }
                event_type::KEY_DOWN | event_type::KEY_UP => {
                    let e: KeyboardEvent = decode(&raw);
                    Event::Key {
                        window: e.window,
                        scancode: e.scancode,
                        key: e.key,
                        modifiers: e.modifiers,
                        down: e.down,
                        repeat: e.repeat,
                    }
                }
                event_type::TEXT_INPUT => {
                    let e: TextInputEvent = decode(&raw);
                    let text = match e.text.is_null() {
                        true => String::new(),
                        // SAFETY: SDL owns the string until the next poll, which
                        // is after this copy.
                        false => CStr::from_ptr(e.text).to_string_lossy().into_owned(),
                    };
                    Event::Text { window: e.window, text }
                }
                event_type::MOUSE_MOTION => {
                    let e: MouseMotionEvent = decode(&raw);
                    Event::MouseMotion {
                        window: e.window,
                        x: e.x,
                        y: e.y,
                        dx: e.xrel,
                        dy: e.yrel,
                        buttons: e.state,
                    }
                }
                event_type::MOUSE_BUTTON_DOWN | event_type::MOUSE_BUTTON_UP => {
                    let e: MouseButtonEvent = decode(&raw);
                    Event::MouseButton {
                        window: e.window,
                        button: e.button,
                        down: e.down,
                        clicks: e.clicks,
                        x: e.x,
                        y: e.y,
                    }
                }
                event_type::MOUSE_WHEEL => {
                    let e: MouseWheelEvent = decode(&raw);
                    Event::MouseWheel { window: e.window, x: e.x, y: e.y }
                }
                other => Event::Other(other),
            }
        })
    }

    /// Start the video subsystem, once.
    ///
    /// SDL refuses to create a window without it, and there is no earlier point
    /// to do it from: a program that never opens a window should not have paid
    /// for a display connection.
    fn init_video() -> bool {
        static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *READY.get_or_init(|| super::init(super::init::VIDEO).is_ok())
    }

    pub fn create_window(title: &CStr, width: i32, height: i32, flags: u64) -> Option<Window> {
        if !init_video() {
            return None;
        }
        let create = api()?.SDL_CreateWindow?;
        // SAFETY: a NUL-terminated title and SDL's own flags.
        let window = unsafe { create(title.as_ptr(), width, height, flags) };
        (!window.is_null()).then_some(window)
    }

    /// One-argument window calls that answer whether they worked.
    macro_rules! window_call {
        ($($name:ident => $entry:ident),* $(,)?) => {
            $(
                pub fn $name(window: Window) -> bool {
                    let Some(f) = api().and_then(|a| a.$entry) else {
                        return false;
                    };
                    // SAFETY: a window this crate created.
                    unsafe { f(window) }
                }
            )*
        };
    }

    window_call! {
        show_window => SDL_ShowWindow,
        hide_window => SDL_HideWindow,
        raise_window => SDL_RaiseWindow,
        swap_window => SDL_GL_SwapWindow,
    }

    pub fn destroy_window(window: Window) {
        let Some(f) = api().and_then(|a| a.SDL_DestroyWindow) else {
            return;
        };
        // SAFETY: a window this crate created.
        unsafe { f(window) };
    }

    pub fn set_window_title(window: Window, title: &CStr) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_SetWindowTitle) else {
            return false;
        };
        // SAFETY: a window and a NUL-terminated title.
        unsafe { f(window, title.as_ptr()) }
    }

    pub fn window_size(window: Window) -> Option<(i32, i32)> {
        let f = api()?.SDL_GetWindowSize?;
        let (mut w, mut h) = (0, 0);
        // SAFETY: a window and two writable ints.
        unsafe { f(window, &mut w, &mut h) }.then_some((w, h))
    }

    pub fn set_window_size(window: Window, width: i32, height: i32) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_SetWindowSize) else {
            return false;
        };
        // SAFETY: a window this crate created.
        unsafe { f(window, width, height) }
    }

    pub fn window_position(window: Window) -> Option<(i32, i32)> {
        let f = api()?.SDL_GetWindowPosition?;
        let (mut x, mut y) = (0, 0);
        // SAFETY: a window and two writable ints.
        unsafe { f(window, &mut x, &mut y) }.then_some((x, y))
    }

    pub fn set_window_position(window: Window, x: i32, y: i32) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_SetWindowPosition) else {
            return false;
        };
        // SAFETY: a window this crate created.
        unsafe { f(window, x, y) }
    }

    pub fn set_window_fullscreen(window: Window, on: bool) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_SetWindowFullscreen) else {
            return false;
        };
        // SAFETY: a window this crate created.
        unsafe { f(window, on) }
    }

    pub fn set_window_resizable(window: Window, on: bool) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_SetWindowResizable) else {
            return false;
        };
        // SAFETY: a window this crate created.
        unsafe { f(window, on) }
    }

    /// Backing pixels per point, which is what `-backingScaleFactor` answers.
    ///
    /// One when SDL cannot say: a scale of zero would divide a window's size to
    /// nothing, and a guest sizing a framebuffer by it would ask for none.
    pub fn window_pixel_density(window: Window) -> f32 {
        let Some(f) = api().and_then(|a| a.SDL_GetWindowPixelDensity) else {
            return 1.0;
        };
        // SAFETY: a window this crate created.
        let density = unsafe { f(window) };
        match density > 0.0 {
            true => density,
            false => 1.0,
        }
    }

    pub fn gl_create_context(window: Window) -> Option<GlContext> {
        let create = api()?.SDL_GL_CreateContext?;
        // SAFETY: a window created with `flags::OPENGL`.
        let context = unsafe { create(window) };
        (!context.is_null()).then_some(context)
    }

    pub fn gl_destroy_context(context: GlContext) {
        let Some(f) = api().and_then(|a| a.SDL_GL_DestroyContext) else {
            return;
        };
        // SAFETY: a context this crate created.
        unsafe { f(context) };
    }

    pub fn gl_make_current(window: Window, context: GlContext) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_GL_MakeCurrent) else {
            return false;
        };
        // SAFETY: a window and a context this crate created.
        unsafe { f(window, context) }
    }

    pub fn gl_set_attribute(attribute: i32, value: i32) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_GL_SetAttribute) else {
            return false;
        };
        // SAFETY: SDL's own enumerator and an integer.
        unsafe { f(attribute, value) }
    }

    pub fn gl_set_swap_interval(interval: i32) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_GL_SetSwapInterval) else {
            return false;
        };
        // SAFETY: an integer.
        unsafe { f(interval) }
    }

    /// A GL entry point by name, which is how the `gl*` imports are satisfied.
    pub fn gl_proc_address(name: &CStr) -> Option<*mut c_void> {
        let f = api()?.SDL_GL_GetProcAddress?;
        // SAFETY: a NUL-terminated name.
        let found = unsafe { f(name.as_ptr()) };
        (!found.is_null()).then_some(found)
    }

    pub fn primary_display() -> u32 {
        let Some(f) = init_video().then(|| api().and_then(|a| a.SDL_GetPrimaryDisplay)).flatten()
        else {
            return 0;
        };
        // SAFETY: no arguments.
        unsafe { f() }
    }

    /// Every display, primary first - which is the order `+[NSScreen screens]`
    /// promises and the reason this is not just what SDL hands back.
    pub fn displays() -> Vec<u32> {
        let Some(api) = init_video().then(api).flatten() else {
            return Vec::new();
        };
        let (Some(get), Some(free)) = (api.SDL_GetDisplays, api.SDL_free) else {
            return Vec::new();
        };
        let mut count = 0;
        // SAFETY: a writable int; the result is SDL's to free and is freed below.
        let ids = unsafe { get(&mut count) };
        if ids.is_null() || count <= 0 {
            return Vec::new();
        }
        // SAFETY: `count` ids, as SDL just reported.
        let mut out = unsafe { std::slice::from_raw_parts(ids, count as usize) }.to_vec();
        // SAFETY: SDL allocated it.
        unsafe { free(ids as *mut c_void) };
        let primary = primary_display();
        if let Some(at) = out.iter().position(|id| *id == primary) {
            out.swap(0, at);
        }
        out
    }

    pub fn display_bounds(display: u32) -> Option<Rect> {
        let f = api()?.SDL_GetDisplayBounds?;
        let mut rect = Rect::default();
        // SAFETY: a display id and a writable rect.
        unsafe { f(display, &mut rect) }.then_some(rect)
    }

    pub fn display_usable_bounds(display: u32) -> Option<Rect> {
        let f = api()?.SDL_GetDisplayUsableBounds?;
        let mut rect = Rect::default();
        // SAFETY: a display id and a writable rect.
        unsafe { f(display, &mut rect) }.then_some(rect)
    }

    /// As [`window_pixel_density`], for a display with no window on it.
    pub fn display_content_scale(display: u32) -> f32 {
        let Some(f) = api().and_then(|a| a.SDL_GetDisplayContentScale) else {
            return 1.0;
        };
        // SAFETY: a display id.
        let scale = unsafe { f(display) };
        match scale > 0.0 {
            true => scale,
            false => 1.0,
        }
    }

    pub fn set_cursor_visible(visible: bool) -> bool {
        let Some(api) = api() else {
            return false;
        };
        let f = match visible {
            true => api.SDL_ShowCursor,
            false => api.SDL_HideCursor,
        };
        // SAFETY: no arguments.
        f.is_some_and(|f| unsafe { f() })
    }

    /// The pointer in desktop coordinates, and which buttons are down.
    pub fn global_mouse_state() -> Option<(f32, f32, u32)> {
        let f = api()?.SDL_GetGlobalMouseState?;
        let (mut x, mut y) = (0.0, 0.0);
        // SAFETY: two writable floats.
        let buttons = unsafe { f(&mut x, &mut y) };
        Some((x, y, buttons))
    }

    pub fn warp_mouse_global(x: f32, y: f32) -> bool {
        let Some(f) = api().and_then(|a| a.SDL_WarpMouseGlobal) else {
            return false;
        };
        // SAFETY: two coordinates.
        unsafe { f(x, y) }
    }
}
