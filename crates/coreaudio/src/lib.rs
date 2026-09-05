//! `AudioUnit` output, over SDL3.
//!
//! One thing, done the way a Mac game does it: find the default output
//! component, make an instance, tell it a stream format, give it a render
//! callback, initialize, start. Everything else an `AudioUnit` can be - a
//! mixer, a converter, an effect - is not here, and a component description
//! asking for one finds nothing rather than getting an output unit under
//! another name.
//!
//! **The render callback is the guest's, and it is pulled on SDL's audio
//! thread.** That is the same shape Apple's has, and the same shape SDL's
//! stream callback has, so there is no queue in the middle: SDL asks for more
//! bytes, this asks the guest for frames, and the frames go straight in.
//!
//! What this does *not* do is convert. The stream format the guest sets is
//! handed to SDL, which converts to whatever the device runs at - so a guest
//! asking for 44100 Hz stereo float gets exactly that in its buffers, whatever
//! the hardware is doing. A format SDL cannot name is refused when it is set,
//! by returning the framework's own `kAudioUnitErr_FormatNotSupported`, rather
//! than accepted and then played as noise.

// The C spellings are the interface, as they are in `corevideo`.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

mod converter;
mod foreign;
mod format;
mod hal;
mod render;
mod status;
mod stream;
pub(crate) mod trace;

pub use status::*;
use stream::{Format, sdl_spec};

/// `AudioComponentDescription`, which is five four-byte fields.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ComponentDescription {
    pub kind: u32,
    pub subtype: u32,
    pub manufacturer: u32,
    pub flags: u32,
    pub flags_mask: u32,
}

/// A four-character code, as the headers write them.
const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// `kAudioUnitType_Output`.
const TYPE_OUTPUT: u32 = fourcc(b"auou");
/// The three output subtypes that mean "the default device": `def `, `ahal`
/// and, on iOS-derived code paths, `rioc`.
const SUBTYPE_DEFAULT_OUTPUT: u32 = fourcc(b"def ");
const SUBTYPE_HAL_OUTPUT: u32 = fourcc(b"ahal");
const SUBTYPE_SYSTEM_OUTPUT: u32 = fourcc(b"sys ");
const SUBTYPE_REMOTE_IO: u32 = fourcc(b"rioc");
/// `kAudioUnitManufacturer_Apple`.
const MANUFACTURER_APPLE: u32 = fourcc(b"appl");

/// The one component this crate has, as an opaque handle.
///
/// The address of a static rather than an index: a guest compares the handle
/// against null and passes it back, and nothing else.
static OUTPUT_COMPONENT: u8 = 0;

fn output_component() -> *mut c_void {
    &OUTPUT_COMPONENT as *const u8 as *mut c_void
}

/// Whether `description` asks for the default output unit.
///
/// A zero field is a wildcard, which is what `AudioComponentFindNext`'s
/// contract says and what a guest that only fills in the type relies on.
fn describes_default_output(d: &ComponentDescription) -> bool {
    let matches = |field: u32, wanted: &[u32]| field == 0 || wanted.contains(&field);
    matches(d.kind, &[TYPE_OUTPUT])
        && matches(
            d.subtype,
            &[
                SUBTYPE_DEFAULT_OUTPUT,
                SUBTYPE_HAL_OUTPUT,
                SUBTYPE_SYSTEM_OUTPUT,
                SUBTYPE_REMOTE_IO,
            ],
        )
        && matches(d.manufacturer, &[MANUFACTURER_APPLE])
}

/// One output unit.
pub struct Unit {
    /// The format the guest set on the input scope, which is the format its
    /// render callback will be asked to fill.
    format: Format,
    /// The render callback and its cookie, as
    /// `kAudioUnitProperty_SetRenderCallback` gave them.
    callback: Option<render::Callback>,
    /// SDL's stream, once `AudioUnitInitialize` has opened the device.
    stream: sdl3::AudioStream,
    running: bool,
    /// Frames pulled so far, which is the `mSampleTime` a guest integrates.
    rendered: u64,
}

// SAFETY: `stream` is SDL's own handle, used only through `sdl3::audio`, and the
// callback is a host-callable pointer this runtime was handed. Both are behind
// the mutex below.
unsafe impl Send for Unit {}

/// Every instance handed out, by the handle the guest holds - the address of
/// the `Mutex`, which is stable and unique for as long as the `Arc` lives.
static UNITS: Mutex<Vec<(usize, Arc<Mutex<Unit>>)>> = Mutex::new(Vec::new());

fn unit_for(handle: *mut c_void) -> Option<Arc<Mutex<Unit>>> {
    UNITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, u)| Arc::clone(u))
}

/// `AudioComponent AudioComponentFindNext(AudioComponent, const AudioComponentDescription *)`
///
/// # Safety
///
/// `description` must be null or a readable `AudioComponentDescription`.
pub unsafe extern "C" fn AudioComponentFindNext(
    after: *mut c_void,
    description: *const ComponentDescription,
) -> *mut c_void {
    // There is exactly one component, so "the next one after it" is none.
    if !after.is_null() || description.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's obligation.
    let wanted = unsafe { *description };
    match describes_default_output(&wanted) {
        true => output_component(),
        false => std::ptr::null_mut(),
    }
}

/// `OSStatus AudioComponentInstanceNew(AudioComponent, AudioComponentInstance *)`
///
/// # Safety
///
/// `out` must be null or a writable pointer-sized slot.
pub unsafe extern "C" fn AudioComponentInstanceNew(
    component: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return PARAM_ERROR;
    }
    if component != output_component() {
        return INVALID_COMPONENT;
    }
    let unit = Arc::new(Mutex::new(Unit {
        format: Format::default(),
        callback: None,
        stream: std::ptr::null_mut(),
        running: false,
        rendered: 0,
    }));
    let handle = Arc::as_ptr(&unit) as *mut c_void;
    UNITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, unit));
    // SAFETY: the caller's obligation.
    unsafe { *out = handle };
    SUCCESS
}

/// `OSStatus AudioComponentInstanceDispose(AudioComponentInstance)`
pub unsafe extern "C" fn AudioComponentInstanceDispose(handle: *mut c_void) -> i32 {
    let Some(unit) = unit_for(handle) else {
        return foreign::status(c"AudioComponentInstanceDispose", handle)
            .unwrap_or(INVALID_COMPONENT);
    };
    // SAFETY: `close` owns the stream and leaves none behind.
    unsafe { stream::close(&mut unit.lock().unwrap_or_else(|e| e.into_inner())) };
    UNITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(h, _)| *h != handle as usize);
    SUCCESS
}

/// `OSStatus AudioUnitInitialize(AudioUnit)`: open the device.
///
/// Opened here rather than at `AudioOutputUnitStart`, because that is where
/// Apple's fails for a format the hardware cannot take - and a guest that has
/// already started is past the point where it checks.
pub unsafe extern "C" fn AudioUnitInitialize(handle: *mut c_void) -> i32 {
    let Some(unit) = unit_for(handle) else {
        return foreign::status(c"AudioUnitInitialize", handle).unwrap_or(INVALID_COMPONENT);
    };
    let mut unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    if !unit.stream.is_null() {
        return SUCCESS;
    }
    let Some(spec) = sdl_spec(&unit.format) else {
        trace::say(
            format_args!("initialize {:?}", unit.format),
            FORMAT_NOT_SUPPORTED,
        );
        return FORMAT_NOT_SUPPORTED;
    };
    let status = match stream::open(&mut unit, spec, handle) {
        true => SUCCESS,
        false => CANNOT_DO_IN_CURRENT_CONTEXT,
    };
    trace::say(
        format_args!("initialize {} Hz, {} channels", spec.freq, spec.channels),
        status,
    );
    status
}

/// `OSStatus AudioUnitUninitialize(AudioUnit)`
pub unsafe extern "C" fn AudioUnitUninitialize(handle: *mut c_void) -> i32 {
    let Some(unit) = unit_for(handle) else {
        return foreign::status(c"AudioUnitUninitialize", handle).unwrap_or(INVALID_COMPONENT);
    };
    // SAFETY: as `AudioComponentInstanceDispose`.
    unsafe { stream::close(&mut unit.lock().unwrap_or_else(|e| e.into_inner())) };
    SUCCESS
}

/// `OSStatus AudioOutputUnitStart(AudioOutputUnit)`
pub unsafe extern "C" fn AudioOutputUnitStart(handle: *mut c_void) -> i32 {
    let Some(unit) = unit_for(handle) else {
        return foreign::status(c"AudioOutputUnitStart", handle).unwrap_or(INVALID_COMPONENT);
    };
    let mut unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    if unit.stream.is_null() {
        return UNINITIALIZED;
    }
    // SAFETY: a stream this crate opened.
    unsafe { sdl3::audio::resume(unit.stream) };
    unit.running = true;
    trace::say(format_args!("start"), SUCCESS);
    SUCCESS
}

/// `OSStatus AudioOutputUnitStop(AudioOutputUnit)`
pub unsafe extern "C" fn AudioOutputUnitStop(handle: *mut c_void) -> i32 {
    let Some(unit) = unit_for(handle) else {
        return foreign::status(c"AudioOutputUnitStop", handle).unwrap_or(INVALID_COMPONENT);
    };
    let mut unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    if !unit.stream.is_null() {
        // SAFETY: a stream this crate opened.
        unsafe { sdl3::audio::pause(unit.stream) };
    }
    unit.running = false;
    SUCCESS
}

/// The clock `AudioGetCurrentHostTime` reports, in nanoseconds.
///
/// It must be the *same* clock `mach_absolute_time` is, because a program
/// compares an `AudioTimeStamp`'s `mHostTime` against one it read from Mach -
/// scheduling a sound "half a second from now" is exactly that subtraction.
/// `darwin::mach` answers `CLOCK_MONOTONIC` nanoseconds off Darwin and reports
/// a 1/1 timebase, so this reads the same clock and
/// [`AudioConvertHostTimeToNanos`] is the identity.
pub(crate) fn host_time() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes the `timespec` it is given and nothing
    // else.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return 0;
    }
    (now.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(now.tv_nsec as u64)
}

/// `UInt64 AudioGetCurrentHostTime(void)`
pub unsafe extern "C" fn AudioGetCurrentHostTime() -> u64 {
    host_time()
}

/// `UInt64 AudioConvertHostTimeToNanos(UInt64)`, and its inverse: both the
/// identity here, for the reason [`host_time`] gives.
pub unsafe extern "C" fn AudioConvertHostTimeToNanos(host_time: u64) -> u64 {
    host_time
}

pub unsafe extern "C" fn AudioConvertNanosToHostTime(nanos: u64) -> u64 {
    nanos
}

/// `OSStatus AudioUnitRender(AudioUnit, AudioUnitRenderActionFlags *, const
/// AudioTimeStamp *, UInt32, UInt32, AudioBufferList *)`
///
/// Pull this unit's render callback into the caller's buffer list. A program
/// calls it when it drives the graph itself rather than letting the output unit
/// pull - a mixer that wants a block of its input, or a tap recording what is
/// about to be played.
///
/// # Safety
///
/// `list` must be a writable `AudioBufferList` whose buffers are writable for
/// their own `mDataByteSize`, and `timestamp` null or readable.
pub unsafe extern "C" fn AudioUnitRender(
    handle: *mut c_void,
    flags: *mut u32,
    timestamp: *const render::TimeStamp,
    bus: u32,
    frames: u32,
    list: *mut render::AudioBufferList,
) -> i32 {
    if list.is_null() {
        return PARAM_ERROR;
    }
    let Some(unit) = unit_for(handle) else {
        return foreign::status(c"AudioUnitRender", handle).unwrap_or(INVALID_COMPONENT);
    };
    let mut unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    let Some(callback) = unit.callback else {
        // A unit with no callback has nothing to render, and the buffers are
        // left as they are: an `AudioUnitRender` that failed has not promised
        // anything about them.
        return UNINITIALIZED;
    };
    let now = match timestamp.is_null() {
        true => render::TimeStamp {
            sample_time: unit.rendered as f64,
            host_time: host_time(),
            rate_scalar: 1.0,
            flags: 1 | 2,
            ..render::TimeStamp::default()
        },
        // SAFETY: the caller's obligation.
        false => unsafe { *timestamp },
    };
    let mut action = 0u32;
    // SAFETY: `callback.proc` is a host-callable pointer this runtime was
    // handed for the guest's function, and every structure it is given outlives
    // the call.
    let status = unsafe {
        (callback.proc)(
            callback.ref_con,
            match flags.is_null() {
                true => &mut action,
                false => flags,
            },
            &now,
            bus,
            frames,
            list,
        )
    };
    if status == SUCCESS {
        unit.rendered += u64::from(frames);
    }
    status
}

/// One entry point, as an address.
macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "AudioComponentFindNext" => at!(AudioComponentFindNext),
        "AudioComponentInstanceNew" => at!(AudioComponentInstanceNew),
        "AudioComponentInstanceDispose" => at!(AudioComponentInstanceDispose),
        "AudioUnitInitialize" => at!(AudioUnitInitialize),
        "AudioUnitUninitialize" => at!(AudioUnitUninitialize),
        "AudioOutputUnitStart" => at!(AudioOutputUnitStart),
        "AudioOutputUnitStop" => at!(AudioOutputUnitStop),
        "AudioUnitGetProperty" => at!(property::AudioUnitGetProperty),
        "AudioUnitSetProperty" => at!(property::AudioUnitSetProperty),
        "AudioUnitRender" => at!(AudioUnitRender),
        "AudioGetCurrentHostTime" => at!(AudioGetCurrentHostTime),
        "AudioConvertHostTimeToNanos" => at!(AudioConvertHostTimeToNanos),
        "AudioConvertNanosToHostTime" => at!(AudioConvertNanosToHostTime),
        "AudioObjectGetPropertyData" => at!(hal::AudioObjectGetPropertyData),
        "AudioObjectGetPropertyDataSize" => at!(hal::AudioObjectGetPropertyDataSize),
        "AudioObjectHasProperty" => at!(hal::AudioObjectHasProperty),
        "AudioObjectIsPropertySettable" => at!(hal::AudioObjectIsPropertySettable),
        "AudioObjectSetPropertyData" => at!(hal::AudioObjectSetPropertyData),
        "AudioObjectAddPropertyListener" => at!(hal::AudioObjectAddPropertyListener),
        "AudioObjectRemovePropertyListener" => at!(hal::AudioObjectRemovePropertyListener),
        "AudioHardwareUnload" => at!(hal::AudioHardwareUnload),
        "AudioDeviceCreateIOProcID" => at!(hal::AudioDeviceCreateIOProcID),
        "AudioDeviceDestroyIOProcID" => at!(hal::AudioDeviceDestroyIOProcID),
        "AudioDeviceStart" => at!(hal::AudioDeviceStart),
        "AudioDeviceStop" => at!(hal::AudioDeviceStop),
        "AudioConverterNew" => at!(converter::AudioConverterNew),
        "AudioConverterNewSpecific" => at!(converter::AudioConverterNewSpecific),
        "AudioConverterDispose" => at!(converter::AudioConverterDispose),
        "AudioConverterReset" => at!(converter::AudioConverterReset),
        "AudioConverterGetProperty" => at!(converter::AudioConverterGetProperty),
        "AudioConverterGetPropertyInfo" => at!(converter::AudioConverterGetPropertyInfo),
        "AudioConverterSetProperty" => at!(converter::AudioConverterSetProperty),
        "AudioConverterFillComplexBuffer" => at!(converter::AudioConverterFillComplexBuffer),
        "AudioFormatGetProperty" => at!(format::AudioFormatGetProperty),
        _ => return None,
    })
}

/// Every name [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "AudioComponentFindNext",
    "AudioComponentInstanceNew",
    "AudioComponentInstanceDispose",
    "AudioUnitInitialize",
    "AudioUnitUninitialize",
    "AudioOutputUnitStart",
    "AudioOutputUnitStop",
    "AudioUnitGetProperty",
    "AudioUnitSetProperty",
    "AudioUnitRender",
    "AudioGetCurrentHostTime",
    "AudioConvertHostTimeToNanos",
    "AudioConvertNanosToHostTime",
    "AudioObjectGetPropertyData",
    "AudioObjectGetPropertyDataSize",
    "AudioObjectHasProperty",
    "AudioObjectIsPropertySettable",
    "AudioObjectSetPropertyData",
    "AudioObjectAddPropertyListener",
    "AudioObjectRemovePropertyListener",
    "AudioHardwareUnload",
    "AudioDeviceCreateIOProcID",
    "AudioDeviceDestroyIOProcID",
    "AudioDeviceStart",
    "AudioDeviceStop",
    "AudioConverterNew",
    "AudioConverterNewSpecific",
    "AudioConverterDispose",
    "AudioConverterReset",
    "AudioConverterGetProperty",
    "AudioConverterGetPropertyInfo",
    "AudioConverterSetProperty",
    "AudioConverterFillComplexBuffer",
    "AudioFormatGetProperty",
];

pub mod property;

/// The component handle, for this crate's own tests.
#[cfg(test)]
fn test_component() -> *mut c_void {
    output_component()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_default_output_component_is_found() {
        let output = ComponentDescription {
            kind: TYPE_OUTPUT,
            subtype: SUBTYPE_DEFAULT_OUTPUT,
            manufacturer: MANUFACTURER_APPLE,
            ..ComponentDescription::default()
        };
        // SAFETY: a live description.
        assert_eq!(
            unsafe { AudioComponentFindNext(std::ptr::null_mut(), &output) },
            output_component()
        );

        // A mixer is a real AudioUnit and is not this one. Answering with the
        // output unit would be a guest rendering into something that never
        // plays.
        let mixer = ComponentDescription {
            kind: fourcc(b"aumx"),
            ..output
        };
        // SAFETY: as above.
        assert!(unsafe { AudioComponentFindNext(std::ptr::null_mut(), &mixer) }.is_null());

        // And "the next one after this" is nothing, which is how a guest's
        // enumeration loop terminates.
        // SAFETY: as above.
        assert!(unsafe { AudioComponentFindNext(output_component(), &output) }.is_null());
    }

    /// A wildcard field matches, which is what a guest filling in only the type
    /// relies on.
    #[test]
    fn a_zero_field_is_a_wildcard() {
        let sparse = ComponentDescription {
            kind: TYPE_OUTPUT,
            ..ComponentDescription::default()
        };
        assert!(describes_default_output(&sparse));
    }

    #[test]
    fn an_instance_is_its_own_handle_and_is_disposed_once() {
        let mut handle = std::ptr::null_mut();
        // SAFETY: a writable slot and this crate's own component.
        unsafe {
            assert_eq!(
                AudioComponentInstanceNew(output_component(), &mut handle),
                SUCCESS
            );
            assert!(!handle.is_null());
            assert_eq!(AudioComponentInstanceDispose(handle), SUCCESS);
            // Twice is a guest bug, and it is named rather than crashing.
            assert_eq!(AudioComponentInstanceDispose(handle), INVALID_COMPONENT);
        }
    }

    /// Starting a unit nothing initialized is the framework's own error, not a
    /// silent no-op: a guest that ignores it and then hears nothing has no way
    /// to tell that from a device with the volume down.
    #[test]
    fn starting_an_uninitialised_unit_says_so() {
        let mut handle = std::ptr::null_mut();
        // SAFETY: as above.
        unsafe {
            AudioComponentInstanceNew(output_component(), &mut handle);
            assert_eq!(AudioOutputUnitStart(handle), UNINITIALIZED);
            AudioComponentInstanceDispose(handle);
        }
    }
}
