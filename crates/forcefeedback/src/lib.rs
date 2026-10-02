#![allow(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    non_snake_case
)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type HResult = i32;
const OK: HResult = 0;
const INVALID_PARAM: HResult = 0x80070057u32 as i32;
const NO_INTERFACE: HResult = 0x80004002u32 as i32;
const UNSUPPORTED: HResult = 0x80004001u32 as i32;
const DEVICE_LOST: HResult = 0x80040209u32 as i32;
const DEVICE_FULL: HResult = 0x80040201u32 as i32;
const NOT_DOWNLOADED: HResult = 0x80040203u32 as i32;
const INCOMPLETE_EFFECT: HResult = 0x80040206u32 as i32;
const DEVICE_PAUSED: HResult = 0x80040301u32 as i32;
const EFFECT_UNSUPPORTED: HResult = 0x80040306u32 as i32;
const INFINITE: u32 = u32::MAX;
const FFCAP_SUPPORTED: u32 = 0x7f;
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
const FFEP_START: u32 = 0x20000000;
const FFEP_NORESTART: u32 = 0x40000000;
const FFEP_NODOWNLOAD: u32 = 0x80000000;
const FFES_SOLO: u32 = 1;
const FFES_NODOWNLOAD: u32 = 0x80000000;

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

#[derive(Clone, Copy, Debug)]
enum Kind {
    Constant,
    Ramp,
    Periodic,
}

fn kind(uuid: *const c_void) -> Option<Kind> {
    let bytes = unsafe { corefoundation::uuid::bytes_of(uuid) }?;
    let mut base = [
        0xe5, 0x59, 0xc4, 0x60, 0xc5, 0xcd, 0x11, 0xd6, 0x8a, 0x1c, 0, 3, 0x93, 0x53, 0xbd, 0,
    ];
    base[3] = bytes[3];
    if bytes != base {
        return None;
    }
    match bytes[3] {
        0x60 => Some(Kind::Constant),
        0x61 => Some(Kind::Ramp),
        0x62..=0x66 => Some(Kind::Periodic),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Definition {
    duration: u32,
    gain: u32,
    magnitude: u32,
    defined: u32,
}
impl Default for Definition {
    fn default() -> Self {
        Self {
            duration: 0,
            gain: 10_000,
            magnitude: 0,
            defined: 0,
        }
    }
}

struct Effect {
    kind: Kind,
    definition: Definition,
    uploaded: Option<Definition>,
}
struct Playback {
    effect: usize,
    duration: Option<Duration>,
    elapsed: Duration,
    since: Option<Instant>,
    iterations: u32,
    definition: Definition,
}
impl Playback {
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.duration.map(|r| {
            r.saturating_sub(
                self.elapsed
                    + self
                        .since
                        .map_or(Duration::ZERO, |s| now.saturating_duration_since(s)),
            )
        })
    }
}

struct Device {
    joystick: usize,
    gain: u32,
    paused: bool,
    actuators: bool,
    effects: BTreeMap<usize, Effect>,
    active: Option<Playback>,
}

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(1);
static DEVICES: Mutex<BTreeMap<usize, Arc<Mutex<Device>>>> = Mutex::new(BTreeMap::new());
static EFFECTS: Mutex<BTreeMap<usize, Arc<Mutex<Device>>>> = Mutex::new(BTreeMap::new());

fn device(handle: *mut c_void) -> Option<Arc<Mutex<Device>>> {
    DEVICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(handle as usize))
        .cloned()
}
fn effect(handle: *mut c_void) -> Option<Arc<Mutex<Device>>> {
    EFFECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(handle as usize))
        .cloned()
}

impl Device {
    fn connected(&self) -> bool {
        self.joystick != 0 && unsafe { sdl3::joystick_connected(self.joystick as sdl3::Joystick) }
    }
    fn stop(&self) {
        if self.joystick != 0 {
            unsafe {
                sdl3::rumble(self.joystick as sdl3::Joystick, 0, 0, 0);
            }
        }
    }
    fn refresh(&mut self, now: Instant) {
        if self
            .active
            .as_ref()
            .is_some_and(|p| p.remaining(now) == Some(Duration::ZERO))
        {
            self.active = None;
            self.stop();
        }
    }
    fn output(&mut self, now: Instant) -> bool {
        self.refresh(now);
        let Some(playback) = &self.active else {
            self.stop();
            return true;
        };
        if self.paused || !self.actuators {
            self.stop();
            return true;
        }
        let d = playback.definition;
        let strength = (u64::from(d.magnitude)
            * u64::from(d.gain)
            * u64::from(self.gain)
            * u64::from(u16::MAX)
            / 1_000_000_000_000) as u16;
        let duration = duration_ms(playback.remaining(now));
        unsafe {
            sdl3::rumble(
                self.joystick as sdl3::Joystick,
                strength,
                strength,
                duration,
            )
        }
    }
    fn update_playback(&mut self, handle: usize, next: Definition) -> HResult {
        if let Some(p) = &mut self.active
            && p.effect == handle
        {
            p.duration = if next.duration == INFINITE || p.iterations == INFINITE {
                None
            } else {
                Some(Duration::from_micros(
                    u64::from(next.duration) * u64::from(p.iterations),
                ))
            };
            p.definition = next;
            if !self.output(Instant::now()) {
                return DEVICE_LOST;
            }
        }
        OK
    }
    fn start(&mut self, handle: usize, iterations: u32, flags: u32) -> HResult {
        if !self.connected() {
            return DEVICE_LOST;
        }
        if self.paused {
            return DEVICE_PAUSED;
        }
        if iterations == 0 || flags & !(FFES_SOLO | FFES_NODOWNLOAD) != 0 {
            return INVALID_PARAM;
        }
        self.refresh(Instant::now());
        let Some(effect) = self.effects.get_mut(&handle) else {
            return INVALID_PARAM;
        };
        if effect.definition.defined & (FFEP_DURATION | FFEP_TYPESPECIFICPARAMS)
            != FFEP_DURATION | FFEP_TYPESPECIFICPARAMS
        {
            return INCOMPLETE_EFFECT;
        }
        if flags & FFES_NODOWNLOAD != 0 && effect.uploaded.is_none() {
            return NOT_DOWNLOADED;
        }
        if self.active.as_ref().is_some_and(|p| p.effect != handle) && flags & FFES_SOLO == 0 {
            return DEVICE_FULL;
        }
        if flags & FFES_NODOWNLOAD == 0 {
            effect.uploaded = Some(effect.definition);
        }
        let definition = effect.uploaded.unwrap();
        let duration = definition.duration;
        let remaining = if duration == INFINITE || iterations == INFINITE {
            None
        } else {
            Some(Duration::from_micros(
                u64::from(duration) * u64::from(iterations),
            ))
        };
        self.active = Some(Playback {
            effect: handle,
            duration: remaining,
            elapsed: Duration::ZERO,
            since: Some(Instant::now()),
            iterations,
            definition,
        });
        if self.output(Instant::now()) {
            OK
        } else {
            self.active = None;
            DEVICE_LOST
        }
    }
}

fn definition(
    value: *const FFEFFECT,
    kind: Kind,
    mut next: Definition,
    flags: u32,
) -> Result<Definition, HResult> {
    if value.is_null() {
        return Err(INVALID_PARAM);
    }
    // Read dwSize before accessing the rest of a potentially older record.
    if unsafe { std::ptr::addr_of!((*value).dwSize).read_unaligned() }
        < size_of::<FFEFFECT>() as u32
    {
        return Err(INVALID_PARAM);
    }
    let value = unsafe { value.read_unaligned() };
    if flags & FFEP_DURATION != 0 {
        next.duration = value.dwDuration;
        next.defined |= FFEP_DURATION;
    }
    if flags & FFEP_GAIN != 0 {
        next.gain = value.dwGain.min(10_000);
    }
    if flags & 0x80 != 0 && !value.lpEnvelope.is_null() {
        return Err(UNSUPPORTED);
    }
    if flags & 0x200 != 0 && value.dwStartDelay != 0 {
        return Err(UNSUPPORTED);
    }
    if flags & 8 != 0 && value.dwTriggerButton != 0 && value.dwTriggerButton != INFINITE {
        return Err(UNSUPPORTED);
    }
    if flags & 0x20 != 0 && value.cAxes != 0 {
        if value.cAxes > 2 || value.rgdwAxes.is_null() {
            return Err(INVALID_PARAM);
        }
        for axis in 0..value.cAxes as usize {
            if ![0, 4].contains(&unsafe { value.rgdwAxes.add(axis).read_unaligned() }) {
                return Err(0x80040304u32 as i32);
            }
        }
    }
    if flags & FFEP_TYPESPECIFICPARAMS != 0 {
        let size = match kind {
            Kind::Constant => 4,
            Kind::Ramp => 8,
            Kind::Periodic => 16,
        };
        if value.lpvTypeSpecificParams.is_null() || value.cbTypeSpecificParams < size {
            return Err(INCOMPLETE_EFFECT);
        }
        let data = value.lpvTypeSpecificParams as *const i32;
        let first = unsafe { data.read_unaligned() };
        next.magnitude = match kind {
            Kind::Constant => first.unsigned_abs(),
            // SDL rumble approximates a ramp by its mean absolute magnitude.
            Kind::Ramp => ((u64::from(first.unsigned_abs())
                + u64::from(unsafe { data.add(1).read_unaligned() }.unsigned_abs()))
                / 2)
            .min(10_000) as u32,
            Kind::Periodic => (u64::from(first as u32)
                + u64::from(unsafe { data.add(1).read_unaligned() }.unsigned_abs()))
            .min(10_000) as u32,
        }
        .min(10_000);
        next.defined |= FFEP_TYPESPECIFICPARAMS;
    }
    Ok(next)
}

fn duration_ms(duration: Option<Duration>) -> u32 {
    duration.map_or(INFINITE, |d| {
        d.as_micros().div_ceil(1000).min(u32::MAX as u128) as u32
    })
}

pub unsafe extern "C" fn FFIsForceFeedback(service: u32) -> HResult {
    if sdl3::init_controllers().is_err() {
        return INVALID_PARAM;
    }
    let Some(joystick) = sdl3::open(service) else {
        return INVALID_PARAM;
    };
    let supported = unsafe { sdl3::joystick_has_rumble(joystick) };
    unsafe {
        sdl3::close(joystick);
    }
    if supported { OK } else { NO_INTERFACE }
}

pub unsafe extern "C" fn FFCreateDevice(service: u32, out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return INVALID_PARAM;
    }
    unsafe {
        out.write(std::ptr::null_mut());
    }
    if sdl3::init_controllers().is_err() {
        return DEVICE_LOST;
    }
    let Some(joystick) = sdl3::open(service) else {
        return INVALID_PARAM;
    };
    if !unsafe { sdl3::joystick_has_rumble(joystick) } {
        unsafe {
            sdl3::close(joystick);
        }
        return NO_INTERFACE;
    }
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    DEVICES.lock().unwrap_or_else(|e| e.into_inner()).insert(
        handle,
        Arc::new(Mutex::new(Device {
            joystick: joystick as usize,
            gain: 10_000,
            paused: false,
            actuators: true,
            effects: BTreeMap::new(),
            active: None,
        })),
    );
    unsafe {
        out.write(handle as *mut c_void);
    }
    OK
}

pub unsafe extern "C" fn FFReleaseDevice(handle: *mut c_void) -> HResult {
    let Some(device) = DEVICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(handle as usize))
    else {
        return INVALID_PARAM;
    };
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    d.stop();
    let joystick = std::mem::take(&mut d.joystick);
    d.active = None;
    d.effects.clear();
    EFFECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, owner| !Arc::ptr_eq(owner, &device));
    if joystick != 0 {
        unsafe {
            sdl3::close(joystick as sdl3::Joystick);
        }
    }
    OK
}

pub unsafe extern "C" fn FFDeviceCreateEffect(
    device_handle: *mut c_void,
    uuid: *mut c_void,
    value: *const FFEFFECT,
    out: *mut *mut c_void,
) -> HResult {
    if out.is_null() {
        return INVALID_PARAM;
    }
    unsafe {
        out.write(std::ptr::null_mut());
    }
    let Some(device) = device(device_handle) else {
        return INVALID_PARAM;
    };
    let Some(kind) = kind(uuid) else {
        return EFFECT_UNSUPPORTED;
    };
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    if d.paused {
        return DEVICE_PAUSED;
    }
    let definition = if value.is_null() {
        Definition::default()
    } else {
        match definition(value, kind, Definition::default(), FFEP_ALLPARAMS) {
            Ok(d) => d,
            Err(e) => return e,
        }
    };
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    d.effects.insert(
        handle,
        Effect {
            kind,
            definition,
            uploaded: (!value.is_null()).then_some(definition),
        },
    );
    EFFECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, Arc::clone(&device));
    unsafe {
        out.write(handle as *mut c_void);
    }
    OK
}

pub unsafe extern "C" fn FFDeviceReleaseEffect(
    device_handle: *mut c_void,
    effect_handle: *mut c_void,
) -> HResult {
    let (Some(device), Some(owner)) = (device(device_handle), effect(effect_handle)) else {
        return INVALID_PARAM;
    };
    if !Arc::ptr_eq(&device, &owner) {
        return INVALID_PARAM;
    }
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if d.effects.remove(&(effect_handle as usize)).is_none() {
        return INVALID_PARAM;
    }
    if d.active
        .as_ref()
        .is_some_and(|p| p.effect == effect_handle as usize)
    {
        d.active = None;
        d.stop();
    }
    EFFECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(effect_handle as usize));
    OK
}

pub unsafe extern "C" fn FFDeviceGetForceFeedbackCapabilities(
    handle: *mut c_void,
    out: *mut FFCAPABILITIES,
) -> HResult {
    let Some(device) = device(handle) else {
        return INVALID_PARAM;
    };
    if out.is_null() {
        return INVALID_PARAM;
    }
    if !device.lock().unwrap_or_else(|e| e.into_inner()).connected() {
        return DEVICE_LOST;
    }
    let mut axes = [0; 32];
    axes[1] = 4;
    unsafe {
        out.write(FFCAPABILITIES {
            ffSpecVer: 0x00800001,
            supportedEffects: FFCAP_SUPPORTED,
            emulatedEffects: FFCAP_SUPPORTED,
            subType: 2,
            numFfAxes: 2,
            ffAxes: axes,
            storageCapacity: u32::MAX,
            playbackCapacity: 1,
            firmwareVer: 0,
            hardwareVer: 0,
            driverVer: 0x00800001,
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
    let d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    let value = match property {
        FFPROP_FFGAIN => d.gain,
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
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    match property {
        FFPROP_FFGAIN => {
            d.gain = value.min(10_000);
            if !d.output(Instant::now()) {
                return DEVICE_LOST;
            }
        }
        FFPROP_AUTOCENTER if value == 0 => {}
        FFPROP_AUTOCENTER => return UNSUPPORTED,
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
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    if ![
        FFSFFC_RESET,
        FFSFFC_STOPALL,
        FFSFFC_PAUSE,
        FFSFFC_CONTINUE,
        FFSFFC_SETACTUATORSON,
        FFSFFC_SETACTUATORSOFF,
    ]
    .contains(&command)
    {
        return INVALID_PARAM;
    }
    let now = Instant::now();
    d.refresh(now);
    match command {
        FFSFFC_RESET => {
            d.stop();
            d.active = None;
            d.effects.clear();
            d.paused = false;
            d.actuators = false;
            d.gain = 10_000;
            EFFECTS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, owner| !Arc::ptr_eq(owner, &device));
        }
        FFSFFC_STOPALL => {
            d.stop();
            d.active = None;
            d.paused = false;
        }
        FFSFFC_PAUSE => {
            if let Some(p) = &mut d.active {
                p.elapsed += p
                    .since
                    .map_or(Duration::ZERO, |s| now.saturating_duration_since(s));
                p.since = None;
            }
            d.paused = true;
            d.stop();
        }
        FFSFFC_CONTINUE => {
            if !d.paused {
                return INVALID_PARAM;
            }
            d.paused = false;
            if let Some(p) = &mut d.active {
                p.since = Some(now);
            }
        }
        FFSFFC_SETACTUATORSON => d.actuators = true,
        FFSFFC_SETACTUATORSOFF => {
            d.actuators = false;
            d.stop();
        }
        _ => unreachable!(),
    }
    if d.output(now) { OK } else { DEVICE_LOST }
}

pub unsafe extern "C" fn FFDeviceGetForceFeedbackState(
    handle: *mut c_void,
    out: *mut u32,
) -> HResult {
    let Some(device) = device(handle) else {
        return INVALID_PARAM;
    };
    if out.is_null() {
        return INVALID_PARAM;
    }
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    d.refresh(Instant::now());
    let mut state = if d.actuators { 0x10 } else { 0x20 };
    if !d.connected() {
        state |= 0x80000000;
    }
    if !d.effects.values().any(|e| e.uploaded.is_some()) {
        state |= 1;
    }
    if d.active.is_none() {
        state |= 2;
    }
    if d.paused {
        state |= 4;
    }
    unsafe {
        out.write(state);
    }
    OK
}

pub unsafe extern "C" fn FFEffectGetEffectStatus(handle: *mut c_void, out: *mut u32) -> HResult {
    let Some(device) = effect(handle) else {
        return INVALID_PARAM;
    };
    if out.is_null() {
        return INVALID_PARAM;
    }
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    if !d.effects.contains_key(&(handle as usize)) {
        return INVALID_PARAM;
    }
    d.refresh(Instant::now());
    let status = FFEGES_EMULATED
        | if d
            .active
            .as_ref()
            .is_some_and(|p| p.effect == handle as usize)
            && !d.paused
        {
            FFEGES_PLAYING
        } else {
            0
        };
    unsafe {
        out.write(status);
    }
    OK
}

pub unsafe extern "C" fn FFEffectSetParameters(
    handle: *mut c_void,
    value: *const FFEFFECT,
    flags: u32,
) -> HResult {
    let Some(device) = effect(handle) else {
        return INVALID_PARAM;
    };
    if flags & !(FFEP_ALLPARAMS | FFEP_START | FFEP_NORESTART | FFEP_NODOWNLOAD) != 0
        || (flags & (FFEP_START | FFEP_NORESTART | FFEP_NODOWNLOAD)).count_ones() > 1
    {
        return INVALID_PARAM;
    }
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    if d.paused {
        return DEVICE_PAUSED;
    }
    let Some(effect) = d.effects.get_mut(&(handle as usize)) else {
        return INVALID_PARAM;
    };
    let next = match definition(value, effect.kind, effect.definition, flags) {
        Ok(d) => d,
        Err(e) => return e,
    };
    effect.definition = next;
    if flags & FFEP_NODOWNLOAD != 0 {
        return 3;
    }
    if next.defined & (FFEP_DURATION | FFEP_TYPESPECIFICPARAMS)
        != FFEP_DURATION | FFEP_TYPESPECIFICPARAMS
    {
        return INCOMPLETE_EFFECT;
    }
    effect.uploaded = Some(next);
    if flags & FFEP_START != 0 {
        return d.start(handle as usize, 1, 0);
    }
    d.update_playback(handle as usize, next)
}

pub unsafe extern "C" fn FFEffectStart(
    handle: *mut c_void,
    iterations: u32,
    flags: u32,
) -> HResult {
    let Some(device) = effect(handle) else {
        return INVALID_PARAM;
    };
    device
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .start(handle as usize, iterations, flags)
}

pub unsafe extern "C" fn FFEffectStop(handle: *mut c_void) -> HResult {
    let Some(device) = effect(handle) else {
        return INVALID_PARAM;
    };
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    if !d.effects.contains_key(&(handle as usize)) {
        return INVALID_PARAM;
    }
    if d.active
        .as_ref()
        .is_some_and(|p| p.effect == handle as usize)
    {
        d.active = None;
        d.stop();
    }
    OK
}

pub unsafe extern "C" fn FFEffectDownload(handle: *mut c_void) -> HResult {
    let Some(device) = effect(handle) else {
        return INVALID_PARAM;
    };
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    if d.paused {
        return DEVICE_PAUSED;
    }
    let Some(effect) = d.effects.get_mut(&(handle as usize)) else {
        return INVALID_PARAM;
    };
    if effect.definition.defined & (FFEP_DURATION | FFEP_TYPESPECIFICPARAMS)
        != FFEP_DURATION | FFEP_TYPESPECIFICPARAMS
    {
        return INCOMPLETE_EFFECT;
    }
    let definition = effect.definition;
    effect.uploaded = Some(definition);
    d.update_playback(handle as usize, definition)
}

pub unsafe extern "C" fn FFEffectUnload(handle: *mut c_void) -> HResult {
    let Some(device) = effect(handle) else {
        return INVALID_PARAM;
    };
    let mut d = device.lock().unwrap_or_else(|e| e.into_inner());
    if !d.connected() {
        return DEVICE_LOST;
    }
    let Some(effect) = d.effects.get_mut(&(handle as usize)) else {
        return INVALID_PARAM;
    };
    if effect.uploaded.is_none() {
        return NOT_DOWNLOADED;
    }
    effect.uploaded = None;
    if d.active
        .as_ref()
        .is_some_and(|p| p.effect == handle as usize)
    {
        d.active = None;
        d.stop();
    }
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
        "FFDeviceGetForceFeedbackState" => FFDeviceGetForceFeedbackState,
        "FFEffectGetEffectStatus" => FFEffectGetEffectStatus,
        "FFEffectSetParameters" => FFEffectSetParameters,
        "FFEffectStart" => FFEffectStart,
        "FFEffectStop" => FFEffectStop,
        "FFEffectDownload" => FFEffectDownload,
        "FFEffectUnload" => FFEffectUnload,
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
        assert_eq!(duration_ms(Some(Duration::from_micros(1))), 1);
        assert_eq!(duration_ms(Some(Duration::from_micros(2_002))), 3);
        assert_eq!(duration_ms(None), INFINITE);
    }
}
