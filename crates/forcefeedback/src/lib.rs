#![allow(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    non_snake_case
)]

//! The ForceFeedback C API, implemented with SDL3's portable two-motor rumble.
//!
//! SDL cannot expose the rich device-specific effect programs available on old
//! Mac hardware. It does expose the part games use for gamepads: a pair of
//! motors with a duration. Effects are therefore retained as API objects and
//! reduced to a motor magnitude when started.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

type HResult = i32;
const OK: HResult = 0;
const INVALID_PARAM: HResult = -2147024809;
const DEVICE_LOST: HResult = -2147024866;
const NOT_DOWNLOADED: HResult = -2147024894;

const INFINITE: u32 = u32::MAX;
const FFCAP_ET_ALL: u32 = 0x0fff;
const FFCAP_ST_VIBRATION: u32 = 2;
const FFPROP_FFGAIN: u32 = 1;
const FFPROP_AUTOCENTER: u32 = 3;
const FFSFFC_RESET: u32 = 1;
const FFSFFC_STOPALL: u32 = 2;
const FFSFFC_PAUSE: u32 = 4;
const FFSFFC_CONTINUE: u32 = 8;
const FFSFFC_SETACTUATORSON: u32 = 16;
const FFSFFC_SETACTUATORSOFF: u32 = 32;
const FFEGES_PLAYING: u32 = 1;
const FFEGES_EMULATED: u32 = 2;
const FFEP_DURATION: u32 = 1;
const FFEP_GAIN: u32 = 4;
const FFEP_TYPESPECIFICPARAMS: u32 = 256;
const FFEP_ALLPARAMS: u32 = 1023;

/// The public ABI layout from `ForceFeedback.h`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FFEFFECT {
    pub dwSize: u32,
    pub dwFlags: u32,
    pub dwDuration: u32,
    pub dwSamplePeriod: u32,
    pub dwGain: u32,
    pub dwTriggerButton: u32,
    pub dwTriggerRepeatInterval: u32,
    pub cAxes: u32,
    pub rgdwAxes: *mut u32,
    pub rglDirection: *mut i32,
    pub lpEnvelope: *mut c_void,
    pub cbTypeSpecificParams: u32,
    pub lpvTypeSpecificParams: *mut c_void,
    pub dwStartDelay: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FFCAPABILITIES {
    pub ffSpecVer: u32,
    pub supportedEffects: u32,
    pub emulatedEffects: u32,
    pub subType: u32,
    pub numFfAxes: u32,
    pub ffAxes: [u8; 32],
    pub storageCapacity: u32,
    pub playbackCapacity: u32,
    pub firmwareVer: u32,
    pub hardwareVer: u32,
    pub driverVer: u32,
}

#[derive(Clone, Copy, Default)]
struct Definition {
    duration: u32,
    gain: u32,
    magnitude: u32,
}

struct Device {
    joystick: sdl3::Joystick,
    gain: u32,
    paused: bool,
    actuators: bool,
}
unsafe impl Send for Device {}

struct Effect {
    device: Arc<Mutex<Device>>,
    definition: Definition,
    playing: bool,
}

static DEVICES: Mutex<Vec<(usize, Arc<Mutex<Device>>)>> = Mutex::new(Vec::new());
static EFFECTS: Mutex<Vec<(usize, Arc<Mutex<Effect>>)>> = Mutex::new(Vec::new());

fn device(handle: *mut c_void) -> Option<Arc<Mutex<Device>>> {
    DEVICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(id, _)| *id == handle as usize)
        .map(|(_, d)| Arc::clone(d))
}
fn effect(handle: *mut c_void) -> Option<Arc<Mutex<Effect>>> {
    EFFECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(id, _)| *id == handle as usize)
        .map(|(_, e)| Arc::clone(e))
}

fn definition(value: *const FFEFFECT) -> Option<Definition> {
    if value.is_null() {
        return Some(Definition {
            duration: 0,
            gain: 10_000,
            magnitude: 10_000,
        });
    }
    // SAFETY: ForceFeedback callers supply a pointer to the public FFEFFECT layout.
    let effect = unsafe { &*value };
    if effect.dwSize != 0 && effect.dwSize < std::mem::size_of::<FFEFFECT>() as u32 {
        return None;
    }
    let mut magnitude = 10_000;
    if !effect.lpvTypeSpecificParams.is_null() && effect.cbTypeSpecificParams >= 4 {
        // All supported FF type-specific records begin with a signed or unsigned
        // 32-bit force magnitude. Absolute value makes directional forces rumble.
        let raw = unsafe { (effect.lpvTypeSpecificParams as *const i32).read_unaligned() };
        magnitude = raw.unsigned_abs().min(10_000);
    }
    Some(Definition {
        duration: effect.dwDuration,
        gain: effect.dwGain.min(10_000),
        magnitude,
    })
}

fn duration_ms(duration: u32, iterations: u32) -> u32 {
    if duration == INFINITE {
        return INFINITE;
    }
    let repeats = iterations.max(1) as u64;
    u64::from(duration)
        .saturating_mul(repeats)
        .div_ceil(1000)
        .min(u64::from(u32::MAX)) as u32
}

fn stop(device: &Device) {
    unsafe {
        let _ = sdl3::rumble(device.joystick, 0, 0, 0);
    }
}

pub unsafe extern "C" fn FFIsForceFeedback(service: u32) -> HResult {
    sdl3::open(service).map_or(DEVICE_LOST, |joystick| {
        unsafe { sdl3::close(joystick) };
        OK
    })
}

pub unsafe extern "C" fn FFCreateDevice(service: u32, out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return INVALID_PARAM;
    }
    let Some(joystick) = sdl3::open(service) else {
        return DEVICE_LOST;
    };
    let device = Arc::new(Mutex::new(Device {
        joystick,
        gain: 10_000,
        paused: false,
        actuators: true,
    }));
    let handle = Arc::as_ptr(&device) as *mut c_void;
    DEVICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, device));
    unsafe {
        out.write(handle);
    }
    OK
}

pub unsafe extern "C" fn FFReleaseDevice(handle: *mut c_void) -> HResult {
    let mut devices = DEVICES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(index) = devices.iter().position(|(id, _)| *id == handle as usize) else {
        return INVALID_PARAM;
    };
    let (_, device) = devices.remove(index);
    let device = device.lock().unwrap_or_else(|e| e.into_inner());
    stop(&device);
    unsafe {
        sdl3::close(device.joystick);
    }
    OK
}

pub unsafe extern "C" fn FFDeviceCreateEffect(
    device_handle: *mut c_void,
    _uuid: *mut c_void,
    value: *const FFEFFECT,
    out: *mut *mut c_void,
) -> HResult {
    if out.is_null() {
        return INVALID_PARAM;
    }
    let Some(device) = device(device_handle) else {
        return INVALID_PARAM;
    };
    let Some(definition) = definition(value) else {
        return INVALID_PARAM;
    };
    let effect = Arc::new(Mutex::new(Effect {
        device,
        definition,
        playing: false,
    }));
    let handle = Arc::as_ptr(&effect) as *mut c_void;
    EFFECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, effect));
    unsafe {
        out.write(handle);
    }
    OK
}

pub unsafe extern "C" fn FFDeviceReleaseEffect(
    device_handle: *mut c_void,
    effect_handle: *mut c_void,
) -> HResult {
    let Some(device) = device(device_handle) else {
        return INVALID_PARAM;
    };
    let mut effects = EFFECTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(index) = effects
        .iter()
        .position(|(id, _)| *id == effect_handle as usize)
    else {
        return INVALID_PARAM;
    };
    if !Arc::ptr_eq(
        &device,
        &effects[index]
            .1
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .device,
    ) {
        return INVALID_PARAM;
    }
    let (_, effect) = effects.remove(index);
    let effect = effect.lock().unwrap_or_else(|e| e.into_inner());
    let device = effect.device.lock().unwrap_or_else(|e| e.into_inner());
    stop(&device);
    OK
}

pub unsafe extern "C" fn FFDeviceGetForceFeedbackCapabilities(
    handle: *mut c_void,
    out: *mut FFCAPABILITIES,
) -> HResult {
    if device(handle).is_none() || out.is_null() {
        return INVALID_PARAM;
    }
    let mut axes = [0; 32];
    axes[0] = 0;
    axes[1] = 1;
    unsafe {
        out.write(FFCAPABILITIES {
            ffSpecVer: 0x0100_0080,
            supportedEffects: FFCAP_ET_ALL,
            emulatedEffects: FFCAP_ET_ALL,
            subType: FFCAP_ST_VIBRATION,
            numFfAxes: 2,
            ffAxes: axes,
            storageCapacity: u32::MAX,
            playbackCapacity: u32::MAX,
            firmwareVer: 0,
            hardwareVer: 0,
            driverVer: 0x0100_0080,
        });
    }
    OK
}

pub unsafe extern "C" fn FFDeviceGetForceFeedbackProperty(
    handle: *mut c_void,
    property: u32,
    out: *mut c_void,
    size: usize,
) -> HResult {
    let Some(device) = device(handle) else {
        return INVALID_PARAM;
    };
    if out.is_null() || size < 4 {
        return INVALID_PARAM;
    }
    let device = device.lock().unwrap_or_else(|e| e.into_inner());
    let value = match property {
        FFPROP_FFGAIN => device.gain,
        FFPROP_AUTOCENTER => 0,
        _ => return INVALID_PARAM,
    };
    unsafe {
        (out as *mut u32).write_unaligned(value);
    }
    OK
}

pub unsafe extern "C" fn FFDeviceSetForceFeedbackProperty(
    handle: *mut c_void,
    property: u32,
    value: *const c_void,
) -> HResult {
    let Some(device) = device(handle) else {
        return INVALID_PARAM;
    };
    if value.is_null() {
        return INVALID_PARAM;
    }
    let value = unsafe { (value as *const u32).read_unaligned() };
    let mut device = device.lock().unwrap_or_else(|e| e.into_inner());
    match property {
        FFPROP_FFGAIN => device.gain = value.min(10_000),
        FFPROP_AUTOCENTER => {}
        _ => return INVALID_PARAM,
    }
    OK
}

pub unsafe extern "C" fn FFDeviceSendForceFeedbackCommand(
    handle: *mut c_void,
    command: u32,
) -> HResult {
    let Some(device) = device(handle) else {
        return INVALID_PARAM;
    };
    let mut device = device.lock().unwrap_or_else(|e| e.into_inner());
    if command & (FFSFFC_RESET | FFSFFC_STOPALL | FFSFFC_PAUSE | FFSFFC_SETACTUATORSOFF) != 0 {
        stop(&device);
    }
    if command & FFSFFC_RESET != 0 {
        device.gain = 10_000;
        device.paused = false;
        device.actuators = true;
    }
    if command & FFSFFC_PAUSE != 0 {
        device.paused = true;
    }
    if command & FFSFFC_CONTINUE != 0 {
        device.paused = false;
    }
    if command & FFSFFC_SETACTUATORSON != 0 {
        device.actuators = true;
    }
    if command & FFSFFC_SETACTUATORSOFF != 0 {
        device.actuators = false;
    }
    OK
}

pub unsafe extern "C" fn FFEffectGetEffectStatus(handle: *mut c_void, out: *mut u32) -> HResult {
    let Some(effect) = effect(handle) else {
        return INVALID_PARAM;
    };
    if out.is_null() {
        return INVALID_PARAM;
    }
    let effect = effect.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        out.write(if effect.playing {
            FFEGES_PLAYING | FFEGES_EMULATED
        } else {
            0
        });
    }
    OK
}

pub unsafe extern "C" fn FFEffectSetParameters(
    handle: *mut c_void,
    value: *const FFEFFECT,
    flags: u32,
) -> HResult {
    let Some(effect) = effect(handle) else {
        return INVALID_PARAM;
    };
    let Some(next) = definition(value) else {
        return INVALID_PARAM;
    };
    let mut effect = effect.lock().unwrap_or_else(|e| e.into_inner());
    if flags == 0 || flags & FFEP_ALLPARAMS == FFEP_ALLPARAMS {
        effect.definition = next;
        return OK;
    }
    if flags & FFEP_DURATION != 0 {
        effect.definition.duration = next.duration;
    }
    if flags & FFEP_GAIN != 0 {
        effect.definition.gain = next.gain;
    }
    if flags & FFEP_TYPESPECIFICPARAMS != 0 {
        effect.definition.magnitude = next.magnitude;
    }
    OK
}

pub unsafe extern "C" fn FFEffectStart(
    handle: *mut c_void,
    iterations: u32,
    _flags: u32,
) -> HResult {
    let Some(effect) = effect(handle) else {
        return INVALID_PARAM;
    };
    let mut effect = effect.lock().unwrap_or_else(|e| e.into_inner());
    let device = effect.device.lock().unwrap_or_else(|e| e.into_inner());
    if device.paused || !device.actuators {
        return NOT_DOWNLOADED;
    }
    let strength = u64::from(effect.definition.magnitude)
        .saturating_mul(u64::from(effect.definition.gain))
        .saturating_mul(u64::from(device.gain))
        / 100_000_000;
    let strength = (strength * u64::from(u16::MAX) / 10_000).min(u64::from(u16::MAX)) as u16;
    let started = unsafe {
        sdl3::rumble(
            device.joystick,
            strength,
            strength,
            duration_ms(effect.definition.duration, iterations),
        )
    };
    drop(device);
    if started {
        effect.playing = true;
        OK
    } else {
        DEVICE_LOST
    }
}

pub unsafe extern "C" fn FFEffectStop(handle: *mut c_void) -> HResult {
    let Some(effect) = effect(handle) else {
        return INVALID_PARAM;
    };
    let mut effect = effect.lock().unwrap_or_else(|e| e.into_inner());
    let device = effect.device.lock().unwrap_or_else(|e| e.into_inner());
    stop(&device);
    drop(device);
    effect.playing = false;
    OK
}

exports::exports! {
    functions {
        "FFIsForceFeedback" => FFIsForceFeedback,
        "FFCreateDevice" => FFCreateDevice,
        "FFReleaseDevice" => FFReleaseDevice,
        "FFDeviceCreateEffect" => FFDeviceCreateEffect,
        "FFDeviceReleaseEffect" => FFDeviceReleaseEffect,
        "FFDeviceGetForceFeedbackCapabilities" => FFDeviceGetForceFeedbackCapabilities,
        "FFDeviceGetForceFeedbackProperty" => FFDeviceGetForceFeedbackProperty,
        "FFDeviceSetForceFeedbackProperty" => FFDeviceSetForceFeedbackProperty,
        "FFDeviceSendForceFeedbackCommand" => FFDeviceSendForceFeedbackCommand,
        "FFEffectGetEffectStatus" => FFEffectGetEffectStatus,
        "FFEffectSetParameters" => FFEffectSetParameters,
        "FFEffectStart" => FFEffectStart,
        "FFEffectStop" => FFEffectStop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_layouts_match_the_headers() {
        assert_eq!(std::mem::size_of::<FFEFFECT>(), 80);
        assert_eq!(std::mem::offset_of!(FFEFFECT, lpvTypeSpecificParams), 64);
        assert_eq!(std::mem::size_of::<FFCAPABILITIES>(), 72);
        assert_eq!(std::mem::offset_of!(FFCAPABILITIES, storageCapacity), 52);
    }

    #[test]
    fn translates_microseconds_and_infinite_duration() {
        assert_eq!(duration_ms(1, 1), 1);
        assert_eq!(duration_ms(1_001, 2), 3);
        assert_eq!(duration_ms(INFINITE, 10), INFINITE);
    }
}
