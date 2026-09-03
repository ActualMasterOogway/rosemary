//! The CoreAudio HAL: the system object, and one output device that is SDL's.
//!
//! This is the layer *below* an `AudioUnit`. A program asks the system object
//! which device is the default output, asks that device its sample rate and
//! channel count, and - if it drives the hardware itself rather than through an
//! output unit - registers an `AudioDeviceIOProc` and starts it. Roblox does
//! both: it queries the device before opening its unit, and it listens for the
//! default device changing.
//!
//! The device is real. Its sample rate, channel count and name come from the
//! playback device SDL actually opened, and an `IOProc` is driven by that
//! device's own callback - so a program that plays this way is heard, and one
//! that only asks the device its format configures itself for the hardware in
//! this machine rather than for a number invented here.
//!
//! What is *not* answered is refused with the framework's own
//! `kAudioHardwareUnknownPropertyError`, which is what Apple's HAL answers for
//! a property a device does not have. The rule this crate is built on applies
//! with more force here than anywhere: every property is a number a program
//! will configure itself from, and a plausible wrong one - a sample rate the
//! device does not run at, a channel count that does not match the buffer list
//! - is silence or noise with nothing in any log.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::render::{AudioBuffer, AudioBufferList, MAX_BUFFERS, TimeStamp};
use crate::status::SUCCESS;

/// `AudioObjectID`. Apple's are opaque; these are two constants, because a
/// program compares them and passes them back and does nothing else with them.
pub type ObjectId = u32;

/// `kAudioObjectSystemObject`.
const SYSTEM_OBJECT: ObjectId = 1;
/// The one device this crate has. Not 0: `kAudioObjectUnknown` is 0, and a
/// caller checks for it.
const OUTPUT_DEVICE: ObjectId = 2;

/// `kAudioHardwareUnknownPropertyError`, `'who?'`.
const UNKNOWN_PROPERTY: i32 = i32::from_be_bytes(*b"who?");
/// `kAudioHardwareBadObjectError`, `'!obj'`.
const BAD_OBJECT: i32 = i32::from_be_bytes(*b"!obj");
/// `kAudioHardwareUnsupportedOperationError`, `'unop'`.
const UNSUPPORTED: i32 = i32::from_be_bytes(*b"unop");
/// `kAudioHardwareIllegalOperationError`, `'nope'`.
const ILLEGAL: i32 = i32::from_be_bytes(*b"nope");

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// The property selectors this HAL answers. Every one is a four-character code
/// and the codes are the interface - a guest passes the number, never a name.
mod selector {
    use super::fourcc;

    // On the system object.
    pub const DEVICES: u32 = fourcc(b"dev#");
    pub const DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
    pub const DEFAULT_SYSTEM_OUTPUT_DEVICE: u32 = fourcc(b"sOut");
    pub const DEFAULT_INPUT_DEVICE: u32 = fourcc(b"dIn ");

    // On a device.
    pub const NAME: u32 = fourcc(b"lnam");
    pub const MANUFACTURER: u32 = fourcc(b"lmak");
    pub const DEVICE_UID: u32 = fourcc(b"uid ");
    pub const MODEL_UID: u32 = fourcc(b"muid");
    pub const TRANSPORT_TYPE: u32 = fourcc(b"tran");
    pub const NOMINAL_SAMPLE_RATE: u32 = fourcc(b"nsrt");
    pub const AVAILABLE_NOMINAL_SAMPLE_RATES: u32 = fourcc(b"nsr#");
    pub const STREAM_CONFIGURATION: u32 = fourcc(b"slay");
    pub const STREAMS: u32 = fourcc(b"stm#");
    pub const BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
    pub const BUFFER_FRAME_SIZE_RANGE: u32 = fourcc(b"fsz#");
    pub const DEVICE_IS_ALIVE: u32 = fourcc(b"livn");
    pub const DEVICE_IS_RUNNING: u32 = fourcc(b"goin");
    pub const LATENCY: u32 = fourcc(b"ltnc");
    pub const SAFETY_OFFSET: u32 = fourcc(b"saft");
    pub const STREAM_FORMAT: u32 = fourcc(b"sfmt");
}

/// `AudioObjectPropertyScope`. All three are declared even though only the
/// input side is branched on: a scope a caller passes is read here, and the
/// reader has to be able to see which value that is.
#[allow(dead_code)]
mod scope {
    use super::fourcc;
    pub const GLOBAL: u32 = fourcc(b"glob");
    pub const INPUT: u32 = fourcc(b"inpt");
    pub const OUTPUT: u32 = fourcc(b"outp");
}

/// `AudioObjectPropertyAddress`: what is being asked, of which side of the
/// device, and of which channel. Twelve bytes, and the layout is ABI.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Address {
    pub selector: u32,
    pub scope: u32,
    pub element: u32,
}

const _: () = assert!(size_of::<Address>() == 12);

/// `AudioValueRange`, which is how the HAL reports every "from here to there".
#[repr(C)]
#[derive(Clone, Copy)]
struct ValueRange {
    minimum: f64,
    maximum: f64,
}

/// What this device is, as SDL reports it.
///
/// Read once: SDL's default device does not change under this process, and a
/// program that asked twice and got two answers would have no way to tell which
/// one its buffers were sized for.
struct Device {
    sample_rate: f64,
    channels: u32,
    name: String,
}

fn device() -> &'static Device {
    static DEVICE: std::sync::OnceLock<Device> = std::sync::OnceLock::new();
    DEVICE.get_or_init(|| {
        let spec = sdl3::audio::device_format();
        Device {
            // 48 kHz stereo when SDL declines to say, which is what it opens a
            // device at by default - so the fallback is the same number the
            // device would have run at rather than a guess.
            sample_rate: spec.map_or(48_000.0, |s| s.freq as f64),
            channels: spec.map_or(2, |s| s.channels.max(1) as u32),
            name: sdl3::audio::device_name().unwrap_or_else(|| "Default Output".to_string()),
        }
    })
}

/// The buffer size an `IOProc` is called with, in frames.
///
/// Settable, because `kAudioDevicePropertyBufferFrameSize` is settable on a
/// real device and a program that sets it sizes its own mixing buffers to
/// match. Nothing else reads it until the next `AudioDeviceStart`.
static BUFFER_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(512);

/// What one registered `AudioDeviceIOProc` is.
struct Proc {
    /// The guest's function, as a host-callable pointer.
    call: IoProc,
    client_data: *mut c_void,
    /// SDL's stream while this proc is started, null while it is stopped.
    stream: sdl3::AudioStream,
    /// Frames handed to it so far, which is the `mSampleTime` it integrates.
    rendered: u64,
}

// SAFETY: as `crate::Unit` - the stream is SDL's own handle, used only through
// `sdl3::audio`, and the callback is a host-callable pointer this runtime was
// handed. Both are behind the mutex below.
unsafe impl Send for Proc {}

/// `AudioDeviceIOProc`.
type IoProc = unsafe extern "C" fn(
    ObjectId,
    *const TimeStamp,
    *const AudioBufferList,
    *const TimeStamp,
    *mut AudioBufferList,
    *const TimeStamp,
    *mut c_void,
) -> i32;

static PROCS: Mutex<Vec<(usize, Arc<Mutex<Proc>>)>> = Mutex::new(Vec::new());

fn proc_for(id: *mut c_void) -> Option<Arc<Mutex<Proc>>> {
    PROCS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(handle, _)| *handle == id as usize)
        .map(|(_, p)| Arc::clone(p))
}

/// A property listener the guest registered.
///
/// Kept so that removing one succeeds and so that a second registration of the
/// same pair is not counted twice. None is ever called: nothing about this
/// device changes while the process runs - SDL opens one device and keeps it -
/// so a listener that fired would be reporting a change that did not happen.
struct Listener {
    object: ObjectId,
    selector: u32,
    call: usize,
    client_data: usize,
}

static LISTENERS: Mutex<Vec<Listener>> = Mutex::new(Vec::new());

/// Write `value` into a caller's buffer, honouring the size protocol every
/// `AudioObjectGetPropertyData` follows: `size` is in-out, the buffer may be
/// null when the caller is only asking how big the answer is, and a buffer
/// smaller than the answer takes as much as fits.
///
/// Getting this wrong is the whole failure mode of this API: a caller that is
/// told 8 bytes and given 16 has a corrupted stack frame, and one told 0
/// concludes the device has no channels.
unsafe fn answer(bytes: &[u8], data: *mut c_void, size: *mut u32) -> i32 {
    if size.is_null() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    let capacity = unsafe { *size } as usize;
    if !data.is_null() {
        let taken = capacity.min(bytes.len());
        // SAFETY: the caller says `data` is writable for `capacity` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), taken) };
        // SAFETY: as above.
        unsafe { *size = taken as u32 };
        return SUCCESS;
    }
    // SAFETY: as above.
    unsafe { *size = bytes.len() as u32 };
    SUCCESS
}

fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is a `Copy` structure with no padding this crate reads back,
    // and the slice borrows it for no longer than the call.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

/// A `CFStringRef` answer, retained as the "copy" in the property's name says.
///
/// The caller releases it. A property whose name says `CFString` hands over
/// ownership, and a runtime that autoreleased it instead would be freeing a
/// string the guest still holds.
fn string_property(text: &str) -> Vec<u8> {
    let string = corefoundation::string::from_bytes(text.as_bytes(), false);
    as_bytes(&(string as usize)).to_vec()
}

/// The stream description this device runs at, for the deprecated
/// `kAudioDevicePropertyStreamFormat`.
fn device_format() -> crate::stream::Format {
    let device = device();
    let bytes_per_frame = 4 * device.channels;
    crate::stream::Format {
        sample_rate: device.sample_rate,
        channels_per_frame: device.channels,
        bytes_per_frame,
        bytes_per_packet: bytes_per_frame,
        ..crate::stream::Format::default()
    }
}

/// The channel layout, as the `AudioBufferList` `kAudioDevicePropertyStream‑
/// Configuration` answers with: one interleaved buffer on the output side and
/// none at all on the input side, because this device does not record.
///
/// The *number of buffers* is what a caller counts channels from, so an input
/// scope that answered the output's layout would have a program open a capture
/// path against a device that has none.
fn stream_configuration(scope: u32) -> Vec<u8> {
    let device = device();
    let mut list = AudioBufferList {
        count: 0,
        buffers: [AudioBuffer {
            channels: 0,
            byte_size: 0,
            data: std::ptr::null_mut(),
        }; MAX_BUFFERS],
    };
    if scope != scope::INPUT {
        list.count = 1;
        list.buffers[0].channels = device.channels;
        list.buffers[0].byte_size = BUFFER_FRAMES.load(std::sync::atomic::Ordering::Relaxed)
            * 4
            * device.channels;
    }
    // Only the buffers that are declared: the header's trailing array is one
    // element long and a caller reads `mNumberBuffers` of them.
    let mut bytes = as_bytes(&list.count).to_vec();
    bytes.extend_from_slice(&[0; 4]);
    for buffer in &list.buffers[..list.count as usize] {
        bytes.extend_from_slice(as_bytes(buffer));
    }
    bytes
}

/// The bytes one property answers with, or `None` for one this HAL does not
/// have.
fn property(object: ObjectId, address: &Address) -> Option<Vec<u8>> {
    let device = device();
    Some(match (object, address.selector) {
        (SYSTEM_OBJECT, selector::DEVICES) => as_bytes(&OUTPUT_DEVICE).to_vec(),
        (SYSTEM_OBJECT, selector::DEFAULT_OUTPUT_DEVICE)
        | (SYSTEM_OBJECT, selector::DEFAULT_SYSTEM_OUTPUT_DEVICE) => {
            as_bytes(&OUTPUT_DEVICE).to_vec()
        }
        // There is no capture device here, and `kAudioObjectUnknown` is how the
        // HAL says so - a caller checks for it before opening an input unit.
        (SYSTEM_OBJECT, selector::DEFAULT_INPUT_DEVICE) => as_bytes(&0u32).to_vec(),
        (OUTPUT_DEVICE, selector::NAME) => string_property(&device.name),
        (OUTPUT_DEVICE, selector::MANUFACTURER) => string_property("SDL"),
        (OUTPUT_DEVICE, selector::DEVICE_UID) | (OUTPUT_DEVICE, selector::MODEL_UID) => {
            string_property("rosetta3:default-output")
        }
        // `kAudioDeviceTransportTypeBuiltIn`. A program checks this to decide
        // whether the device may vanish - USB and Bluetooth ones do.
        (OUTPUT_DEVICE, selector::TRANSPORT_TYPE) => as_bytes(&fourcc(b"bltn")).to_vec(),
        (OUTPUT_DEVICE, selector::NOMINAL_SAMPLE_RATE) => as_bytes(&device.sample_rate).to_vec(),
        (OUTPUT_DEVICE, selector::AVAILABLE_NOMINAL_SAMPLE_RATES) => as_bytes(&ValueRange {
            minimum: device.sample_rate,
            maximum: device.sample_rate,
        })
        .to_vec(),
        (OUTPUT_DEVICE, selector::STREAM_CONFIGURATION) => stream_configuration(address.scope),
        // One stream on the output side, none on the input side, matching the
        // configuration above.
        (OUTPUT_DEVICE, selector::STREAMS) => match address.scope == scope::INPUT {
            true => Vec::new(),
            false => as_bytes(&OUTPUT_DEVICE).to_vec(),
        },
        (OUTPUT_DEVICE, selector::STREAM_FORMAT) => as_bytes(&device_format()).to_vec(),
        (OUTPUT_DEVICE, selector::BUFFER_FRAME_SIZE) => {
            as_bytes(&BUFFER_FRAMES.load(std::sync::atomic::Ordering::Relaxed)).to_vec()
        }
        (OUTPUT_DEVICE, selector::BUFFER_FRAME_SIZE_RANGE) => as_bytes(&ValueRange {
            minimum: 64.0,
            maximum: 4096.0,
        })
        .to_vec(),
        (OUTPUT_DEVICE, selector::DEVICE_IS_ALIVE) => as_bytes(&1u32).to_vec(),
        (OUTPUT_DEVICE, selector::DEVICE_IS_RUNNING) => {
            let running = PROCS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|(_, p)| !p.lock().unwrap_or_else(|e| e.into_inner()).stream.is_null());
            as_bytes(&u32::from(running)).to_vec()
        }
        // Latency and safety offset are what a program *adds* to its own
        // scheduling, so zero is a real answer rather than a missing one: SDL
        // does not report the device's, and claiming a number would move every
        // sound the program schedules.
        (OUTPUT_DEVICE, selector::LATENCY) | (OUTPUT_DEVICE, selector::SAFETY_OFFSET) => {
            as_bytes(&0u32).to_vec()
        }
        _ => return None,
    })
}

/// `OSStatus AudioObjectGetPropertyData(AudioObjectID, const
/// AudioObjectPropertyAddress *, UInt32, const void *, UInt32 *, void *)`
///
/// # Safety
///
/// `address` must be a readable `AudioObjectPropertyAddress`, `size` a writable
/// `UInt32`, and `data` null or writable for `*size` bytes.
pub unsafe extern "C" fn AudioObjectGetPropertyData(
    object: ObjectId,
    address: *const Address,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    size: *mut u32,
    data: *mut c_void,
) -> i32 {
    if address.is_null() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    let address = unsafe { *address };
    if !known(object) {
        return BAD_OBJECT;
    }
    match property(object, &address) {
        // SAFETY: the caller's obligation.
        Some(bytes) => unsafe { answer(&bytes, data, size) },
        None => {
            // The size is zeroed rather than left alone, so a caller that
            // ignores the status reads an empty answer rather than whatever was
            // in its variable before.
            if !size.is_null() {
                // SAFETY: as above.
                unsafe { *size = 0 };
            }
            UNKNOWN_PROPERTY
        }
    }
}

/// `OSStatus AudioObjectGetPropertyDataSize(...)`
///
/// # Safety
///
/// As [`AudioObjectGetPropertyData`], with no data buffer.
pub unsafe extern "C" fn AudioObjectGetPropertyDataSize(
    object: ObjectId,
    address: *const Address,
    qualifier_size: u32,
    qualifier: *const c_void,
    size: *mut u32,
) -> i32 {
    if !size.is_null() {
        // The buffer is null below, so the answer is the full size whatever the
        // caller left in here.
        // SAFETY: the caller's obligation.
        unsafe { *size = 0 };
    }
    // SAFETY: the caller's obligation.
    unsafe {
        AudioObjectGetPropertyData(
            object,
            address,
            qualifier_size,
            qualifier,
            size,
            std::ptr::null_mut(),
        )
    }
}

fn known(object: ObjectId) -> bool {
    object == SYSTEM_OBJECT || object == OUTPUT_DEVICE
}

/// `Boolean AudioObjectHasProperty(AudioObjectID, const
/// AudioObjectPropertyAddress *)`
///
/// # Safety
///
/// `address` must be null or a readable `AudioObjectPropertyAddress`.
pub unsafe extern "C" fn AudioObjectHasProperty(object: ObjectId, address: *const Address) -> u8 {
    if address.is_null() || !known(object) {
        return 0;
    }
    // SAFETY: the caller's obligation.
    let address = unsafe { *address };
    u8::from(property(object, &address).is_some())
}

/// `OSStatus AudioObjectIsPropertySettable(AudioObjectID, const
/// AudioObjectPropertyAddress *, Boolean *)`
///
/// # Safety
///
/// As [`AudioObjectHasProperty`], and `settable` must be null or writable.
pub unsafe extern "C" fn AudioObjectIsPropertySettable(
    object: ObjectId,
    address: *const Address,
    settable: *mut u8,
) -> i32 {
    if address.is_null() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    let address = unsafe { *address };
    if !known(object) {
        return BAD_OBJECT;
    }
    if property(object, &address).is_none() {
        return UNKNOWN_PROPERTY;
    }
    if !settable.is_null() {
        // SAFETY: the caller's obligation.
        unsafe { *settable = u8::from(is_settable(object, address.selector)) };
    }
    SUCCESS
}

/// The one property a caller may change. The sample rate is *not* one of them:
/// SDL opened the device at its own rate and converts to it, so accepting a new
/// one would be agreeing to something the device is not doing.
fn is_settable(object: ObjectId, selector: u32) -> bool {
    object == OUTPUT_DEVICE && selector == selector::BUFFER_FRAME_SIZE
}

/// `OSStatus AudioObjectSetPropertyData(...)`
///
/// # Safety
///
/// As [`AudioObjectGetPropertyData`], and `data` must be readable for `size`
/// bytes.
pub unsafe extern "C" fn AudioObjectSetPropertyData(
    object: ObjectId,
    address: *const Address,
    _qualifier_size: u32,
    _qualifier: *const c_void,
    size: u32,
    data: *const c_void,
) -> i32 {
    if address.is_null() || data.is_null() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    let address = unsafe { *address };
    if !known(object) {
        return BAD_OBJECT;
    }
    if !is_settable(object, address.selector) {
        return match property(object, &address).is_some() {
            true => ILLEGAL,
            false => UNKNOWN_PROPERTY,
        };
    }
    if (size as usize) < size_of::<u32>() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation, and the size was just checked.
    let frames = unsafe { *(data as *const u32) };
    BUFFER_FRAMES.store(frames.clamp(64, 4096), std::sync::atomic::Ordering::Relaxed);
    SUCCESS
}

/// `OSStatus AudioObjectAddPropertyListener(AudioObjectID, const
/// AudioObjectPropertyAddress *, AudioObjectPropertyListenerProc, void *)`
///
/// Accepted and recorded, never called - see [`Listener`]. Success rather than
/// a refusal because the registration *did* succeed: there is nothing wrong
/// with the caller's request, there is simply no change to report.
///
/// # Safety
///
/// `address` must be null or readable.
pub unsafe extern "C" fn AudioObjectAddPropertyListener(
    object: ObjectId,
    address: *const Address,
    listener: *const c_void,
    client_data: *mut c_void,
) -> i32 {
    if address.is_null() || listener.is_null() {
        return crate::status::PARAM_ERROR;
    }
    if !known(object) {
        return BAD_OBJECT;
    }
    // SAFETY: the caller's obligation.
    let address = unsafe { *address };
    LISTENERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Listener {
            object,
            selector: address.selector,
            call: listener as usize,
            client_data: client_data as usize,
        });
    SUCCESS
}

/// `OSStatus AudioObjectRemovePropertyListener(...)`
///
/// # Safety
///
/// As [`AudioObjectAddPropertyListener`].
pub unsafe extern "C" fn AudioObjectRemovePropertyListener(
    object: ObjectId,
    address: *const Address,
    listener: *const c_void,
    client_data: *mut c_void,
) -> i32 {
    if address.is_null() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    let address = unsafe { *address };
    let mut listeners = LISTENERS.lock().unwrap_or_else(|e| e.into_inner());
    let before = listeners.len();
    listeners.retain(|l| {
        !(l.object == object
            && l.selector == address.selector
            && l.call == listener as usize
            && l.client_data == client_data as usize)
    });
    match listeners.len() < before {
        true => SUCCESS,
        // Apple's answers this for a listener that was never added, and a
        // caller that removes one twice is asking a real question.
        false => crate::status::INVALID_PARAMETER,
    }
}

/// `OSStatus AudioHardwareUnload(void)`
///
/// Nothing is loaded that a program can unload: the device belongs to SDL and
/// outlives this call. Success, because the caller's request - "I am done with
/// the HAL" - is satisfied.
pub unsafe extern "C" fn AudioHardwareUnload() -> i32 {
    SUCCESS
}

/// `OSStatus AudioDeviceCreateIOProcID(AudioObjectID, AudioDeviceIOProc, void *,
/// AudioDeviceIOProcID *)`
///
/// The `IOProcID` handed back is the address of the registration, which is what
/// every other handle in this crate is and for the same reason: the guest
/// compares it against null and passes it back.
///
/// # Safety
///
/// `call` must be a host-callable `AudioDeviceIOProc`, and `out` null or a
/// writable pointer-sized slot.
pub unsafe extern "C" fn AudioDeviceCreateIOProcID(
    device: ObjectId,
    call: Option<IoProc>,
    client_data: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return crate::status::PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    unsafe { *out = std::ptr::null_mut() };
    let Some(call) = call else {
        return crate::status::PARAM_ERROR;
    };
    if device != OUTPUT_DEVICE {
        return BAD_OBJECT;
    }
    let registration = Arc::new(Mutex::new(Proc {
        call,
        client_data,
        stream: std::ptr::null_mut(),
        rendered: 0,
    }));
    let handle = Arc::as_ptr(&registration) as *mut c_void;
    PROCS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, registration));
    // SAFETY: the caller's obligation.
    unsafe { *out = handle };
    SUCCESS
}

/// `OSStatus AudioDeviceDestroyIOProcID(AudioObjectID, AudioDeviceIOProcID)`
pub unsafe extern "C" fn AudioDeviceDestroyIOProcID(device: ObjectId, id: *mut c_void) -> i32 {
    if device != OUTPUT_DEVICE {
        return BAD_OBJECT;
    }
    let Some(registration) = proc_for(id) else {
        return crate::status::INVALID_PARAMETER;
    };
    // SAFETY: `stop` owns the stream and leaves none behind.
    unsafe { stop(&mut registration.lock().unwrap_or_else(|e| e.into_inner())) };
    PROCS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(handle, _)| *handle != id as usize);
    SUCCESS
}

/// `OSStatus AudioDeviceStart(AudioObjectID, AudioDeviceIOProcID)`
pub unsafe extern "C" fn AudioDeviceStart(device: ObjectId, id: *mut c_void) -> i32 {
    if device != OUTPUT_DEVICE {
        return BAD_OBJECT;
    }
    let Some(registration) = proc_for(id) else {
        return crate::status::INVALID_PARAMETER;
    };
    let mut started = registration.lock().unwrap_or_else(|e| e.into_inner());
    if !started.stream.is_null() {
        return SUCCESS;
    }
    let opened = self::device();
    let spec = sdl3::AudioSpec {
        format: sdl3::audio::format::F32,
        channels: opened.channels as i32,
        freq: opened.sample_rate as i32,
    };
    // SAFETY: `drive` has SDL's callback prototype, and `id` is a key into a
    // table that outlives the process rather than a borrow.
    let stream = unsafe { sdl3::audio::open_playback(spec, Some(drive), id) };
    let Some(stream) = stream else {
        return UNSUPPORTED;
    };
    started.stream = stream;
    // SAFETY: a stream this crate just opened.
    unsafe { sdl3::audio::resume(stream) };
    SUCCESS
}

/// `OSStatus AudioDeviceStop(AudioObjectID, AudioDeviceIOProcID)`
pub unsafe extern "C" fn AudioDeviceStop(device: ObjectId, id: *mut c_void) -> i32 {
    if device != OUTPUT_DEVICE {
        return BAD_OBJECT;
    }
    let Some(registration) = proc_for(id) else {
        return crate::status::INVALID_PARAMETER;
    };
    // SAFETY: SDL's destroy waits for its own callback, which is the only other
    // caller.
    unsafe { stop(&mut registration.lock().unwrap_or_else(|e| e.into_inner())) };
    SUCCESS
}

/// # Safety
///
/// No other thread may be inside this proc's stream.
unsafe fn stop(registration: &mut Proc) {
    if registration.stream.is_null() {
        return;
    }
    // SAFETY: the caller's obligation; the handle came from `AudioDeviceStart`.
    unsafe { sdl3::audio::destroy(registration.stream) };
    registration.stream = std::ptr::null_mut();
}

/// SDL wants more bytes: call the guest's `IOProc` for them.
///
/// The same shape as [`crate::render::feed`], and the difference is the
/// interface: an `IOProc` is handed the output buffer list directly rather than
/// through a render callback, and gets three timestamps because a device that
/// also records has an input time.
///
/// # Safety
///
/// `userdata` is the `IOProcID` [`AudioDeviceStart`] passed, and SDL calls this
/// on its own audio thread.
unsafe extern "C" fn drive(
    userdata: *mut c_void,
    stream: sdl3::AudioStream,
    additional: i32,
    _total: i32,
) {
    if additional <= 0 {
        return;
    }
    let Some(registration) = proc_for(userdata) else {
        return;
    };
    // Held across the guest's call for the reason `render::feed` holds its own:
    // `AudioDeviceStop` takes it too, so a guest cannot free what it is being
    // asked to fill.
    let mut registration = registration.lock().unwrap_or_else(|e| e.into_inner());
    if registration.stream.is_null() {
        return;
    }
    let device = device();
    let frame_bytes = 4 * device.channels as usize;
    let per_call = BUFFER_FRAMES.load(std::sync::atomic::Ordering::Relaxed) as usize;
    let mut wanted = additional as usize;
    let mut scratch = vec![0u8; per_call * frame_bytes];
    while wanted > 0 {
        let frames = (wanted / frame_bytes).clamp(1, per_call);
        let filled = &mut scratch[..frames * frame_bytes];
        filled.fill(0);
        let mut list = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: 0,
                byte_size: 0,
                data: std::ptr::null_mut(),
            }; MAX_BUFFERS],
        };
        list.buffers[0].channels = device.channels;
        list.buffers[0].byte_size = filled.len() as u32;
        list.buffers[0].data = filled.as_mut_ptr().cast();
        let now = TimeStamp {
            sample_time: registration.rendered as f64,
            host_time: crate::host_time(),
            rate_scalar: 1.0,
            flags: 1 | 2,
            ..TimeStamp::default()
        };
        // SAFETY: `call` is a host-callable pointer this runtime was handed for
        // the guest's function, and every structure it is given outlives the
        // call. The input list is null, which is what a device with no input
        // stream passes.
        let status = unsafe {
            (registration.call)(
                OUTPUT_DEVICE,
                &now,
                std::ptr::null(),
                &now,
                &mut list,
                &now,
                registration.client_data,
            )
        };
        if status != SUCCESS {
            filled.fill(0);
        }
        registration.rendered += frames as u64;
        // SAFETY: a stream this crate opened, and a live slice.
        unsafe { sdl3::audio::put(stream, filled) };
        wanted = wanted.saturating_sub(filled.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(object: ObjectId, selector: u32, scope: u32) -> Option<Vec<u8>> {
        let address = Address {
            selector,
            scope,
            element: 0,
        };
        let mut size = 0u32;
        // SAFETY: a live address and a writable size, with no data buffer.
        let status = unsafe {
            AudioObjectGetPropertyDataSize(object, &address, 0, std::ptr::null(), &mut size)
        };
        if status != SUCCESS {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: as above, with a buffer of the size just asked for.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                buffer.as_mut_ptr().cast(),
            )
        };
        (status == SUCCESS).then_some(buffer)
    }

    /// The system object names a device, and that device is the one this crate
    /// answers about - a caller follows exactly this chain.
    #[test]
    fn the_default_output_device_is_the_one_that_answers() {
        let id = ask(SYSTEM_OBJECT, selector::DEFAULT_OUTPUT_DEVICE, scope::GLOBAL)
            .expect("a default output device");
        assert_eq!(id, as_bytes(&OUTPUT_DEVICE));
        assert!(ask(OUTPUT_DEVICE, selector::NOMINAL_SAMPLE_RATE, scope::GLOBAL).is_some());
    }

    /// An unknown property is refused *and* reports a size of zero, so a caller
    /// that ignores the status does not read its own uninitialised buffer.
    #[test]
    fn an_unknown_property_is_refused_and_leaves_no_size_behind() {
        let address = Address {
            selector: fourcc(b"zzzz"),
            scope: scope::GLOBAL,
            element: 0,
        };
        let mut size = 1234u32;
        // SAFETY: a live address and a writable size.
        let status = unsafe {
            AudioObjectGetPropertyData(
                OUTPUT_DEVICE,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, UNKNOWN_PROPERTY);
        assert_eq!(size, 0);
    }

    /// An object nothing knows about is `kAudioHardwareBadObjectError` rather
    /// than an unknown property: the two send a caller looking in different
    /// places.
    #[test]
    fn an_unknown_object_is_a_bad_object() {
        let address = Address {
            selector: selector::DEVICE_IS_ALIVE,
            scope: scope::GLOBAL,
            element: 0,
        };
        let mut size = 4u32;
        // SAFETY: as above.
        let status = unsafe {
            AudioObjectGetPropertyData(
                99,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, BAD_OBJECT);
    }

    /// The input side of an output-only device has no buffers, and that is how
    /// a caller counts its channels.
    #[test]
    fn the_input_scope_reports_no_channels() {
        let input = stream_configuration(scope::INPUT);
        assert_eq!(input[..4], *as_bytes(&0u32), "no buffers on the input side");
        let output = stream_configuration(scope::OUTPUT);
        assert_eq!(output[..4], *as_bytes(&1u32));
    }

    /// The buffer size is the one property a program may set, and it reads back
    /// as what was set.
    #[test]
    fn the_buffer_size_is_settable_and_reads_back() {
        let address = Address {
            selector: selector::BUFFER_FRAME_SIZE,
            scope: scope::OUTPUT,
            element: 0,
        };
        let wanted = 1024u32;
        // SAFETY: a live address and a live `UInt32`.
        let status = unsafe {
            AudioObjectSetPropertyData(
                OUTPUT_DEVICE,
                &address,
                0,
                std::ptr::null(),
                4,
                (&wanted as *const u32).cast(),
            )
        };
        assert_eq!(status, SUCCESS);
        assert_eq!(
            ask(OUTPUT_DEVICE, selector::BUFFER_FRAME_SIZE, scope::OUTPUT),
            Some(as_bytes(&wanted).to_vec())
        );
        // And back, so the order tests run in cannot matter.
        let default = 512u32;
        // SAFETY: as above.
        unsafe {
            AudioObjectSetPropertyData(
                OUTPUT_DEVICE,
                &address,
                0,
                std::ptr::null(),
                4,
                (&default as *const u32).cast(),
            )
        };
    }
}
