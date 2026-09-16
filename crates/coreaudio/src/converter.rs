//! `AudioConverter`, on SDL's audio stream.
//!
//! A converter is the one piece of CoreAudio that is pure computation: bytes in
//! one linear-PCM format in, the same audio in another out, with a sample-rate
//! change in the middle. `SDL_AudioStream` is exactly that object, so this is a
//! translation of one interface into another rather than a signal processor:
//! the resampler is SDL's.
//!
//! **A compressed format is refused, not approximated.** Apple's converter also
//! decodes AAC and the rest; there is no decoder here, and a converter that
//! accepted `'aac '` and then produced silence would be a program that plays
//! nothing with nothing in any log. `kAudioConverterErr_FormatNotSupported` is
//! the answer a caller is written for.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::render::{AudioBufferList, MAX_BUFFERS};
use crate::status::{PARAM_ERROR, SUCCESS};
use crate::stream::{Format, sdl_spec};

/// `kAudioConverterErr_FormatNotSupported`, `'fmt?'`.
const FORMAT_NOT_SUPPORTED: i32 = i32::from_be_bytes(*b"fmt?");
/// `kAudioConverterErr_PropertyNotSupported`, `'prop'`.
const PROPERTY_NOT_SUPPORTED: i32 = i32::from_be_bytes(*b"prop");
/// `kAudioConverterErr_UnspecifiedError`, `'what'`.
const UNSPECIFIED: i32 = i32::from_be_bytes(*b"what");

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// The `AudioConverterPropertyID`s this answers.
mod property {
    use super::fourcc;
    pub const CURRENT_INPUT_FORMAT: u32 = fourcc(b"ciff");
    pub const CURRENT_OUTPUT_FORMAT: u32 = fourcc(b"cofm");
    /// `kAudioConverterPropertyMaximumOutputPacketSize`.
    pub const MAXIMUM_OUTPUT_PACKET_SIZE: u32 = fourcc(b"xops");
    /// `kAudioConverterPropertyCalculateInputBufferSize`.
    pub const CALCULATE_INPUT_BUFFER_SIZE: u32 = fourcc(b"cibs");
    /// `kAudioConverterPropertyCalculateOutputBufferSize`.
    pub const CALCULATE_OUTPUT_BUFFER_SIZE: u32 = fourcc(b"cobs");
    /// `kAudioConverterSampleRateConverterQuality`, which SDL has no dial for.
    pub const SAMPLE_RATE_CONVERTER_QUALITY: u32 = fourcc(b"srcq");
}

struct Converter {
    source: Format,
    destination: Format,
    /// SDL's converting stream, attached to no device.
    stream: sdl3::AudioStream,
    /// The bytes the input callback last handed over, kept alive until the next
    /// call: Apple's contract lets a caller point `mData` at storage it reuses
    /// only after the *next* invocation, and SDL has already copied by then.
    held: Vec<u8>,
    /// Converted frames on their way out, interleaved as SDL produces them.
    /// Kept on the converter rather than allocated per call, because every one
    /// of those calls is on an audio thread.
    scratch: Vec<u8>,
}

// SAFETY: `stream` is SDL's own handle, used only through `sdl3::audio`, and it
// is behind the mutex below.
unsafe impl Send for Converter {}

static CONVERTERS: Mutex<Vec<(usize, Arc<Mutex<Converter>>)>> = Mutex::new(Vec::new());

fn converter_for(handle: *mut c_void) -> Option<Arc<Mutex<Converter>>> {
    CONVERTERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, c)| Arc::clone(c))
}

/// `OSStatus AudioConverterNew(const AudioStreamBasicDescription *, const
/// AudioStreamBasicDescription *, AudioConverterRef *)`
///
/// # Safety
///
/// Both descriptions must be readable, and `out` null or a writable
/// pointer-sized slot.
pub unsafe extern "C" fn AudioConverterNew(
    source: *const Format,
    destination: *const Format,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    unsafe { *out = std::ptr::null_mut() };
    if source.is_null() || destination.is_null() {
        return PARAM_ERROR;
    }
    // SAFETY: the caller's obligation.
    let (source, destination) = unsafe { (*source, *destination) };
    let (Some(from), Some(to)) = (sdl_spec(&source), sdl_spec(&destination)) else {
        return FORMAT_NOT_SUPPORTED;
    };
    let Some(stream) = sdl3::audio::create_converter(from, to) else {
        return UNSPECIFIED;
    };
    let converter = Arc::new(Mutex::new(Converter {
        source,
        destination,
        stream,
        held: Vec::new(),
        scratch: Vec::new(),
    }));
    let handle = Arc::as_ptr(&converter) as *mut c_void;
    CONVERTERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, converter));
    // SAFETY: the caller's obligation.
    unsafe { *out = handle };
    SUCCESS
}

/// `OSStatus AudioConverterNewSpecific(..., UInt32, const AudioClassDescription
/// *, AudioConverterRef *)`
///
/// The class list names *which codec implementation* to use - hardware or
/// software - and there is one converter here either way, so it is ignored
/// rather than refused: a caller asking for the software AAC decoder and
/// getting linear PCM conversion is refused by the formats, one line down.
///
/// # Safety
///
/// As [`AudioConverterNew`].
pub unsafe extern "C" fn AudioConverterNewSpecific(
    source: *const Format,
    destination: *const Format,
    _class_count: u32,
    _classes: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    // SAFETY: the caller's obligation.
    unsafe { AudioConverterNew(source, destination, out) }
}

/// `OSStatus AudioConverterDispose(AudioConverterRef)`
pub unsafe extern "C" fn AudioConverterDispose(handle: *mut c_void) -> i32 {
    let Some(converter) = converter_for(handle) else {
        return PARAM_ERROR;
    };
    {
        let mut converter = converter.lock().unwrap_or_else(|e| e.into_inner());
        if !converter.stream.is_null() {
            // SAFETY: a stream this crate made, with no device and so no other
            // thread inside it.
            unsafe { sdl3::audio::destroy(converter.stream) };
            converter.stream = std::ptr::null_mut();
        }
    }
    CONVERTERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(h, _)| *h != handle as usize);
    SUCCESS
}

/// `OSStatus AudioConverterReset(AudioConverterRef)`: drop whatever is in
/// flight, which is what a caller does after a seek.
pub unsafe extern "C" fn AudioConverterReset(handle: *mut c_void) -> i32 {
    let Some(converter) = converter_for(handle) else {
        return PARAM_ERROR;
    };
    let mut converter = converter.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a stream this crate made.
    unsafe { sdl3::audio::clear(converter.stream) };
    converter.held.clear();
    SUCCESS
}

/// The bytes a property answers with, or `None` for one this converter has not
/// got.
fn property(converter: &Converter, id: u32) -> Option<Vec<u8>> {
    let bytes = |value: &[u8]| value.to_vec();
    Some(match id {
        property::CURRENT_INPUT_FORMAT => bytes(as_bytes(&converter.source)),
        property::CURRENT_OUTPUT_FORMAT => bytes(as_bytes(&converter.destination)),
        // One packet is one frame for linear PCM, which is the only thing this
        // converter takes.
        property::MAXIMUM_OUTPUT_PACKET_SIZE => {
            bytes(as_bytes(&converter.destination.bytes_per_packet))
        }
        _ => return None,
    })
}

fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: `T` is a `Copy` structure and the slice borrows it for no longer
    // than the call.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

/// `OSStatus AudioConverterGetProperty(AudioConverterRef,
/// AudioConverterPropertyID, UInt32 *, void *)`
///
/// # Safety
///
/// `size` must be a writable `UInt32` and `data` null or writable for `*size`.
pub unsafe extern "C" fn AudioConverterGetProperty(
    handle: *mut c_void,
    id: u32,
    size: *mut u32,
    data: *mut c_void,
) -> i32 {
    let Some(converter) = converter_for(handle) else {
        return PARAM_ERROR;
    };
    if size.is_null() {
        return PARAM_ERROR;
    }
    let converter = converter.lock().unwrap_or_else(|e| e.into_inner());
    let Some(bytes) = property(&converter, id) else {
        // SAFETY: the caller's obligation.
        unsafe { *size = 0 };
        return PROPERTY_NOT_SUPPORTED;
    };
    // SAFETY: the caller's obligation.
    let capacity = unsafe { *size } as usize;
    if !data.is_null() {
        let taken = capacity.min(bytes.len());
        // SAFETY: the caller says `data` is writable for `capacity` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), taken) };
        // SAFETY: as above.
        unsafe { *size = taken as u32 };
    } else {
        // SAFETY: as above.
        unsafe { *size = bytes.len() as u32 };
    }
    SUCCESS
}

/// `OSStatus AudioConverterGetPropertyInfo(AudioConverterRef,
/// AudioConverterPropertyID, UInt32 *, Boolean *)`
///
/// # Safety
///
/// Both out-parameters must be null or writable.
pub unsafe extern "C" fn AudioConverterGetPropertyInfo(
    handle: *mut c_void,
    id: u32,
    size: *mut u32,
    writable: *mut u8,
) -> i32 {
    let Some(converter) = converter_for(handle) else {
        return PARAM_ERROR;
    };
    let converter = converter.lock().unwrap_or_else(|e| e.into_inner());
    let found = property(&converter, id);
    if !size.is_null() {
        // SAFETY: the caller's obligation.
        unsafe { *size = found.as_ref().map_or(0, |b| b.len() as u32) };
    }
    if !writable.is_null() {
        // Nothing here is settable after the converter is made: the formats
        // were fixed when SDL's stream was created.
        // SAFETY: the caller's obligation.
        unsafe { *writable = 0 };
    }
    match found.is_some() {
        true => SUCCESS,
        false => PROPERTY_NOT_SUPPORTED,
    }
}

/// `OSStatus AudioConverterSetProperty(AudioConverterRef,
/// AudioConverterPropertyID, UInt32, const void *)`
///
/// The quality dials are accepted and dropped, because SDL's resampler has one
/// quality and a caller that asked for a better one is not owed an error - it
/// gets conversion either way. Everything else is refused rather than silently
/// ignored: a channel map or a priming setting that did nothing would change
/// what comes out.
pub unsafe extern "C" fn AudioConverterSetProperty(
    handle: *mut c_void,
    id: u32,
    _size: u32,
    _data: *const c_void,
) -> i32 {
    if converter_for(handle).is_none() {
        return PARAM_ERROR;
    }
    match id {
        property::SAMPLE_RATE_CONVERTER_QUALITY
        | property::CALCULATE_INPUT_BUFFER_SIZE
        | property::CALCULATE_OUTPUT_BUFFER_SIZE => SUCCESS,
        _ => PROPERTY_NOT_SUPPORTED,
    }
}

/// `AudioConverterComplexInputDataProc`: the callback that supplies input.
type InputProc = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *mut AudioBufferList,
    *mut *mut c_void,
    *mut c_void,
) -> i32;

/// `OSStatus AudioConverterFillComplexBuffer(AudioConverterRef,
/// AudioConverterComplexInputDataProc, void *, UInt32 *, AudioBufferList *,
/// AudioStreamPacketDescription *)`
///
/// The pull loop, and the shape of it is the interface: the caller says how
/// many output packets it wants, this asks the callback for input until SDL has
/// produced that much, and reports back how many packets were actually
/// produced. A callback that supplies nothing ends the loop - that is how a
/// caller signals end of stream - and the count written back is what it reads
/// to know the stream is over.
///
/// # Safety
///
/// `packets` must be a writable `UInt32`, `output` a writable `AudioBufferList`
/// whose buffers are writable for their own `mDataByteSize`, and `input` a
/// host-callable input procedure.
pub unsafe extern "C" fn AudioConverterFillComplexBuffer(
    handle: *mut c_void,
    input: Option<InputProc>,
    input_data: *mut c_void,
    packets: *mut u32,
    output: *mut AudioBufferList,
    _descriptions: *mut c_void,
) -> i32 {
    if packets.is_null() || output.is_null() {
        return PARAM_ERROR;
    }
    let Some(converter) = converter_for(handle) else {
        return PARAM_ERROR;
    };
    let Some(input) = input else {
        // SAFETY: the caller's obligation.
        unsafe { *packets = 0 };
        return PARAM_ERROR;
    };
    let mut converter = converter.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY: the caller's obligation.
    let out = unsafe { &mut *output };
    let destination = converter.destination;
    let channels = destination.channels_per_frame.max(1) as usize;
    // How many buffers the output is spread over, and how much of a frame each
    // one holds: a non-interleaved format puts one channel in each, and an
    // interleaved one puts the whole frame in the first.
    let planes = match destination.non_interleaved() {
        true => channels.min(out.count as usize),
        false => 1,
    };
    let plane_frame = destination.bytes_per_frame.max(1) as usize;
    if out.count == 0 || out.count as usize > MAX_BUFFERS || planes == 0 {
        // SAFETY: as above.
        unsafe { *packets = 0 };
        return PARAM_ERROR;
    }
    if out.buffers[..planes].iter().any(|b| b.data.is_null()) {
        // SAFETY: as above.
        unsafe { *packets = 0 };
        return PARAM_ERROR;
    }

    // In frames, which for linear PCM is also in packets: every buffer has to
    // hold the whole of what is produced, so the smallest of them is the room.
    // SAFETY: the caller's obligation.
    let mut room = unsafe { *packets } as usize;
    for buffer in &out.buffers[..planes] {
        room = room.min(buffer.byte_size as usize / plane_frame);
    }

    // What SDL hands back is interleaved whatever the destination's layout is,
    // so it is taken into this scratch and dealt out to the buffers below.
    let stream_frame = match destination.non_interleaved() {
        true => plane_frame * channels,
        false => plane_frame,
    };
    converter.scratch.clear();
    converter.scratch.resize(room * stream_frame, 0);

    let mut produced = 0usize;
    while produced < converter.scratch.len() {
        // SAFETY: a stream this crate made.
        if unsafe { sdl3::audio::available(converter.stream) } == 0
            && !feed(&mut converter, input, input_data)
        {
            break;
        }
        let taken =
            unsafe { sdl3::audio::get(converter.stream, &mut converter.scratch[produced..]) };
        if taken == 0 {
            break;
        }
        produced += taken;
    }
    // A partial frame is no frame: half of one written into a caller's buffer
    // is a click, and the count reported back would not account for it.
    let frames = produced / stream_frame;

    for (plane, buffer) in out.buffers[..planes].iter_mut().enumerate() {
        // SAFETY: the caller says the buffer is writable for `mDataByteSize`,
        // and `room` was measured against that.
        let into = unsafe {
            std::slice::from_raw_parts_mut(buffer.data.cast::<u8>(), frames * plane_frame)
        };
        match destination.non_interleaved() {
            true => unweave(&converter.scratch, into, plane, plane_frame, stream_frame),
            false => into.copy_from_slice(&converter.scratch[..frames * plane_frame]),
        }
        buffer.byte_size = (frames * plane_frame) as u32;
    }

    // SAFETY: the caller's obligation.
    unsafe { *packets = frames as u32 };
    SUCCESS
}

/// Lay one channel's own buffer into its place in an interleaved stream.
///
/// `plane_frame` is what one frame takes in the channel's buffer and
/// `stream_frame` what it takes in the stream, so the channel's slot is the
/// `plane`th of them. The two are what a non-interleaved description means:
/// `mBytesPerFrame` counts one channel, and the stream SDL takes counts all of
/// them.
fn weave(from: &[u8], into: &mut [u8], plane: usize, plane_frame: usize, stream_frame: usize) {
    let at = plane * plane_frame;
    for (frame, sample) in from.chunks_exact(plane_frame).enumerate() {
        let to = frame * stream_frame + at;
        if to + plane_frame > into.len() {
            return;
        }
        into[to..to + plane_frame].copy_from_slice(sample);
    }
}

/// Take one channel back out of an interleaved stream: [`weave`] the other way,
/// for the buffers a caller hands a converter to fill.
fn unweave(from: &[u8], into: &mut [u8], plane: usize, plane_frame: usize, stream_frame: usize) {
    let at = plane * plane_frame;
    for (frame, sample) in into.chunks_exact_mut(plane_frame).enumerate() {
        let take = frame * stream_frame + at;
        if take + plane_frame > from.len() {
            return;
        }
        sample.copy_from_slice(&from[take..take + plane_frame]);
    }
}

/// Ask the caller's input procedure for more, and give it to SDL.
///
/// `false` when it supplies nothing, which is end of stream: SDL is flushed so
/// the resampler hands back its tail rather than holding samples for input that
/// is never coming.
fn feed(converter: &mut Converter, input: InputProc, input_data: *mut c_void) -> bool {
    let mut wanted = 4096u32;
    let mut list = AudioBufferList {
        count: 1,
        buffers: [crate::render::AudioBuffer {
            channels: converter.source.channels_per_frame,
            byte_size: 0,
            data: std::ptr::null_mut(),
        }; MAX_BUFFERS],
    };
    // SAFETY: `input` is a host-callable pointer this runtime was handed for
    // the guest's procedure, and both structures outlive the call. The packet
    // description list is null, which is what a caller of a PCM converter
    // passes.
    let status = unsafe {
        input(
            input_data,
            &mut wanted,
            &mut list,
            std::ptr::null_mut(),
            input_data,
        )
    };
    if status != SUCCESS || list.count == 0 {
        // SAFETY: a stream this crate made.
        unsafe { sdl3::audio::flush(converter.stream) };
        return false;
    }
    let channels = converter.source.channels_per_frame.max(1) as usize;
    let planes = match converter.source.non_interleaved() {
        true => channels.min(list.count as usize),
        false => 1,
    };
    let plane_frame = converter.source.bytes_per_frame.max(1) as usize;
    // Frames the procedure supplied, which is the smallest of what it put in
    // each plane and of what it says it supplied in packets.
    let mut frames = wanted as usize * converter.source.frames_per_packet.max(1) as usize;
    for buffer in &list.buffers[..planes] {
        if buffer.data.is_null() {
            frames = 0;
            break;
        }
        frames = frames.min(buffer.byte_size as usize / plane_frame);
    }
    if frames == 0 {
        // SAFETY: as above.
        unsafe { sdl3::audio::flush(converter.stream) };
        return false;
    }

    // SDL takes one interleaved stream, so a planar supply is woven together
    // here rather than put a channel at a time - which would play as the left
    // channel followed by the right.
    let stream_frame = match converter.source.non_interleaved() {
        true => plane_frame * channels,
        false => plane_frame,
    };
    converter.held.clear();
    converter.held.resize(frames * stream_frame, 0);
    for (plane, buffer) in list.buffers[..planes].iter().enumerate() {
        // SAFETY: the guest's procedure says this buffer holds `mDataByteSize`
        // readable bytes, and `frames` was measured against that.
        let from =
            unsafe { std::slice::from_raw_parts(buffer.data.cast::<u8>(), frames * plane_frame) };
        match converter.source.non_interleaved() {
            true => weave(from, &mut converter.held, plane, plane_frame, stream_frame),
            false => converter.held.copy_from_slice(from),
        }
    }
    // SAFETY: a stream this crate made, and a live slice.
    unsafe { sdl3::audio::put(converter.stream, &converter.held) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(rate: f64, channels: u32) -> Format {
        Format {
            sample_rate: rate,
            channels_per_frame: channels,
            bytes_per_frame: 4 * channels,
            bytes_per_packet: 4 * channels,
            ..Format::default()
        }
    }

    /// A format SDL cannot name is refused at creation rather than accepted and
    /// then silently producing nothing.
    #[test]
    fn a_compressed_format_is_refused() {
        let mut handle = std::ptr::null_mut();
        let aac = Format {
            format_id: fourcc(b"aac "),
            ..format(44100.0, 2)
        };
        let pcm = format(48000.0, 2);
        // SAFETY: two live descriptions and a writable slot.
        assert_eq!(
            unsafe { AudioConverterNew(&aac, &pcm, &mut handle) },
            FORMAT_NOT_SUPPORTED
        );
        assert!(handle.is_null());
    }

    /// Two planar channels become one interleaved stream and come back out of
    /// it, which is the whole of what a non-interleaved format asks for.
    #[test]
    fn a_planar_channel_weaves_into_its_own_slot_and_back() {
        let (left, right) = (*b"L0L1L2L3", *b"R0R1R2R3");
        let mut stream = [0u8; 16];
        weave(&left, &mut stream, 0, 2, 4);
        weave(&right, &mut stream, 1, 2, 4);
        assert_eq!(&stream, b"L0R0L1R1L2R2L3R3");

        let (mut back_left, mut back_right) = ([0u8; 8], [0u8; 8]);
        unweave(&stream, &mut back_left, 0, 2, 4);
        unweave(&stream, &mut back_right, 1, 2, 4);
        assert_eq!(&back_left, &left);
        assert_eq!(&back_right, &right);
    }

    /// A stream shorter than the buffer it is dealt into stops rather than
    /// reading past it: what SDL had is what the caller is told it produced.
    #[test]
    fn weaving_stops_at_the_end_of_what_there_is() {
        let mut half = [b'.'; 8];
        unweave(b"L0R0L1R1", &mut half, 0, 2, 4);
        assert_eq!(&half, b"L0L1....");
    }

    /// A handle this crate did not make is a parameter error rather than a
    /// dereference of whatever it is.
    #[test]
    fn a_foreign_handle_is_refused() {
        let mut bogus = 0u8;
        let handle = (&mut bogus as *mut u8).cast::<c_void>();
        // SAFETY: a pointer that is not a converter, which is the case being
        // tested.
        unsafe {
            assert_eq!(AudioConverterDispose(handle), PARAM_ERROR);
            assert_eq!(AudioConverterReset(handle), PARAM_ERROR);
        }
    }
}
