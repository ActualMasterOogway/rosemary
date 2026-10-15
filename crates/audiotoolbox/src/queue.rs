//! `AudioQueue` playback, on an SDL stream.
//!
//! A queue owns buffers the program fills and enqueues; the device consumes
//! them in order and hands each back through the output callback to be filled
//! again. Here a buffer is consumed when SDL's stream takes its bytes, so the
//! callback runs on SDL's audio thread rather than the run loop the program
//! named. SDL 3's own CoreAudio backend is the program this was written for,
//! and its callback only needs to be serialized, which SDL's thread is.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

const SUCCESS: i32 = 0;
const PARAM_ERR: i32 = -50;
/// `kAudioQueueErr_InvalidBuffer`.
const INVALID_BUFFER: i32 = -66687;
/// `kAudioQueueErr_BufferEmpty`.
const BUFFER_EMPTY: i32 = -66686;
/// `kAudioQueueErr_InvalidProperty`.
const INVALID_PROPERTY: i32 = -66684;
/// `kAudioQueueErr_CannotStart`.
const CANNOT_START: i32 = -66681;
/// `kAudioQueueErr_InvalidDevice`.
const INVALID_DEVICE: i32 = -66680;
/// `kAudioFormatUnsupportedDataFormatError`, `'fmt?'`.
const UNSUPPORTED_FORMAT: i32 = i32::from_be_bytes(*b"fmt?");

/// `kAudioQueueProperty_CurrentDevice`.
const PROPERTY_CURRENT_DEVICE: u32 = u32::from_be_bytes(*b"aqcd");
/// `kAudioQueueProperty_ChannelLayout`.
const PROPERTY_CHANNEL_LAYOUT: u32 = u32::from_be_bytes(*b"aqcl");

/// `kAudioFormatLinearPCM` and the `AudioFormatFlags` that matter for it.
const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const IS_FLOAT: u32 = 1 << 0;
const IS_BIG_ENDIAN: u32 = 1 << 1;
const IS_SIGNED_INTEGER: u32 = 1 << 2;
const IS_NON_INTERLEAVED: u32 = 1 << 5;

/// `AudioStreamBasicDescription`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Format {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

const _: () = assert!(size_of::<Format>() == 40);

/// `AudioQueueBuffer`. The program writes `mAudioData` and `mAudioDataByteSize`
/// and reads the rest, so the layout is ABI.
#[repr(C)]
pub struct Buffer {
    pub capacity: u32,
    pub data: *mut c_void,
    pub byte_size: u32,
    pub user_data: *mut c_void,
    pub packet_description_capacity: u32,
    pub packet_descriptions: *mut c_void,
    pub packet_description_count: u32,
}

const _: () = assert!(size_of::<Buffer>() == 56);

/// `AudioQueueOutputCallback`: `(inUserData, inAQ, inBuffer)`.
pub type OutputCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut Buffer);

/// The SDL spec a description asks for, or `None` for one SDL cannot name.
///
/// Refused rather than converted: a layout read as another plays as noise.
fn sdl_spec(format: &Format) -> Option<sdl3::AudioSpec> {
    if format.format_id != FORMAT_LINEAR_PCM
        || format.sample_rate <= 0.0
        || format.channels_per_frame == 0
    {
        return None;
    }
    let flags = format.format_flags;
    // One interleaved buffer is all `AudioQueueBuffer` has room for.
    if flags & IS_NON_INTERLEAVED != 0 && format.channels_per_frame > 1 {
        return None;
    }
    if format.bytes_per_frame != format.channels_per_frame * format.bits_per_channel / 8 {
        return None;
    }
    use sdl3::audio::format::*;
    let big = flags & IS_BIG_ENDIAN != 0;
    let sdl_format = match (
        flags & IS_FLOAT != 0,
        flags & IS_SIGNED_INTEGER != 0,
        format.bits_per_channel,
    ) {
        (true, _, 32) => [F32, F32BE][big as usize],
        (false, true, 32) => [S32, S32BE][big as usize],
        (false, true, 16) => [S16, S16BE][big as usize],
        (false, true, 8) => S8,
        (false, false, 8) => U8,
        _ => return None,
    };
    Some(sdl3::AudioSpec {
        format: sdl_format,
        channels: format.channels_per_frame as i32,
        freq: format.sample_rate as i32,
    })
}

struct Queue {
    callback: OutputCallback,
    user_data: *mut c_void,
    stream: sdl3::AudioStream,
    /// Every buffer allocated on this queue, freed with it.
    buffers: Vec<*mut Buffer>,
    /// Buffers waiting for the device, in the order they were enqueued.
    enqueued: VecDeque<*mut Buffer>,
    running: bool,
}

// SAFETY: the stream is SDL's handle, the buffers are this module's own
// allocations, and the callback is a host-callable pointer; all are behind the
// queue's mutex.
unsafe impl Send for Queue {}

static QUEUES: Mutex<Vec<(usize, Arc<Mutex<Queue>>)>> = Mutex::new(Vec::new());

fn queue_for(handle: *mut c_void) -> Option<Arc<Mutex<Queue>>> {
    QUEUES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, q)| Arc::clone(q))
}

fn lock(queue: &Mutex<Queue>) -> std::sync::MutexGuard<'_, Queue> {
    queue.lock().unwrap_or_else(|e| e.into_inner())
}

/// SDL's stream callback: hand it enqueued buffers until it has what it asked
/// for, giving each back to the program as it goes.
///
/// # Safety
///
/// `userdata` is the queue handle `AudioQueueNewOutput` opened the stream with.
unsafe extern "C" fn feed(
    userdata: *mut c_void,
    stream: sdl3::AudioStream,
    additional: i32,
    _total: i32,
) {
    let Some(queue) = queue_for(userdata) else {
        return;
    };
    let mut wanted = additional.max(0) as usize;
    while wanted > 0 {
        let (buffer, callback, user_data) = {
            let mut q = lock(&queue);
            if !q.running {
                return;
            }
            // Nothing enqueued is an underrun, and SDL plays silence for it.
            let Some(buffer) = q.enqueued.pop_front() else {
                return;
            };
            (buffer, q.callback, q.user_data)
        };
        // SAFETY: an allocation of this module's, live until the queue is
        // disposed, which destroys this stream - and so waits for this call -
        // first.
        let bytes = unsafe {
            let b = &*buffer;
            std::slice::from_raw_parts(b.data as *const u8, b.byte_size as usize)
        };
        // SAFETY: the stream this callback was registered on.
        unsafe { sdl3::audio::put(stream, bytes) };
        wanted = wanted.saturating_sub(bytes.len());
        // The callback re-enqueues, which takes the queue's lock: not held here.
        let _darwin = darwin::tls::DarwinThread::enter();
        // SAFETY: the program's callback, with the arguments it registered.
        unsafe { callback(user_data, userdata, buffer) };
    }
}

/// `OSStatus AudioQueueNewOutput(const AudioStreamBasicDescription *,
/// AudioQueueOutputCallback, void *, CFRunLoopRef, CFStringRef, UInt32,
/// AudioQueueRef *)`
///
/// # Safety
///
/// `format` must be readable and `out` writable.
pub unsafe extern "C" fn AudioQueueNewOutput(
    format: *const Format,
    callback: Option<OutputCallback>,
    user_data: *mut c_void,
    _run_loop: *mut c_void,
    _run_loop_mode: *mut c_void,
    _flags: u32,
    out: *mut *mut c_void,
) -> i32 {
    let (Some(callback), false, false) = (callback, format.is_null(), out.is_null()) else {
        return PARAM_ERR;
    };
    let format = unsafe { *format };
    let Some(spec) = sdl_spec(&format) else {
        coreaudio::trace(
            format_args!("AudioQueueNewOutput: unsupported format {format:?}"),
            UNSUPPORTED_FORMAT,
        );
        return UNSUPPORTED_FORMAT;
    };
    let queue = Arc::new(Mutex::new(Queue {
        callback,
        user_data,
        stream: std::ptr::null_mut(),
        buffers: Vec::new(),
        enqueued: VecDeque::new(),
        running: false,
    }));
    let handle = Arc::as_ptr(&queue) as *mut c_void;
    // SAFETY: `feed` has the callback's prototype; `handle` is a registry key
    // that `AudioQueueDispose` retires only after destroying the stream.
    let Some(stream) = (unsafe { sdl3::audio::open_playback(spec, Some(feed), handle) }) else {
        return INVALID_DEVICE;
    };
    lock(&queue).stream = stream;
    QUEUES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, queue));
    coreaudio::trace(
        format_args!("AudioQueueNewOutput {handle:p} {format:?}"),
        SUCCESS,
    );
    unsafe { *out = handle };
    SUCCESS
}

/// `AudioQueueNewInput`. Recording through a queue is not implemented, and a
/// program told there is no device carries on without one.
pub unsafe extern "C" fn AudioQueueNewInput(
    _format: *const Format,
    _callback: *mut c_void,
    _user_data: *mut c_void,
    _run_loop: *mut c_void,
    _run_loop_mode: *mut c_void,
    _flags: u32,
    _out: *mut *mut c_void,
) -> i32 {
    coreaudio::trace(
        format_args!("AudioQueueNewInput: unsupported"),
        INVALID_DEVICE,
    );
    INVALID_DEVICE
}

/// `OSStatus AudioQueueAllocateBuffer(AudioQueueRef, UInt32, AudioQueueBufferRef *)`
pub unsafe extern "C" fn AudioQueueAllocateBuffer(
    handle: *mut c_void,
    capacity: u32,
    out: *mut *mut Buffer,
) -> i32 {
    let Some(queue) = queue_for(handle) else {
        return PARAM_ERR;
    };
    if out.is_null() {
        return PARAM_ERR;
    }
    let data = Box::into_raw(vec![0u8; capacity as usize].into_boxed_slice()) as *mut u8;
    let buffer = Box::into_raw(Box::new(Buffer {
        capacity,
        data: data.cast(),
        byte_size: 0,
        user_data: std::ptr::null_mut(),
        packet_description_capacity: 0,
        packet_descriptions: std::ptr::null_mut(),
        packet_description_count: 0,
    }));
    lock(&queue).buffers.push(buffer);
    unsafe { *out = buffer };
    SUCCESS
}

/// # Safety
///
/// `buffer` must have come from [`AudioQueueAllocateBuffer`] and be in no
/// queue's lists any more.
unsafe fn free_buffer(buffer: *mut Buffer) {
    let b = unsafe { Box::from_raw(buffer) };
    let data = std::ptr::slice_from_raw_parts_mut(b.data as *mut u8, b.capacity as usize);
    drop(unsafe { Box::from_raw(data) });
}

/// `OSStatus AudioQueueFreeBuffer(AudioQueueRef, AudioQueueBufferRef)`
pub unsafe extern "C" fn AudioQueueFreeBuffer(handle: *mut c_void, buffer: *mut Buffer) -> i32 {
    let Some(queue) = queue_for(handle) else {
        return PARAM_ERR;
    };
    let mut q = lock(&queue);
    let Some(index) = q.buffers.iter().position(|b| *b == buffer) else {
        return INVALID_BUFFER;
    };
    q.buffers.remove(index);
    q.enqueued.retain(|b| *b != buffer);
    unsafe { free_buffer(buffer) };
    SUCCESS
}

/// `OSStatus AudioQueueEnqueueBuffer(AudioQueueRef, AudioQueueBufferRef, UInt32,
/// const AudioStreamPacketDescription *)`
///
/// Packet descriptions describe compressed formats, which no queue here takes.
pub unsafe extern "C" fn AudioQueueEnqueueBuffer(
    handle: *mut c_void,
    buffer: *mut Buffer,
    _packet_count: u32,
    _packets: *const c_void,
) -> i32 {
    let Some(queue) = queue_for(handle) else {
        return PARAM_ERR;
    };
    let mut q = lock(&queue);
    if !q.buffers.contains(&buffer) {
        return INVALID_BUFFER;
    }
    // SAFETY: one of this queue's buffers.
    let b = unsafe { &*buffer };
    if b.byte_size == 0 {
        return BUFFER_EMPTY;
    }
    if b.byte_size > b.capacity {
        return PARAM_ERR;
    }
    q.enqueued.push_back(buffer);
    SUCCESS
}

/// `OSStatus AudioQueueStart(AudioQueueRef, const AudioTimeStamp *)`
pub unsafe extern "C" fn AudioQueueStart(handle: *mut c_void, _start: *const c_void) -> i32 {
    let Some(queue) = queue_for(handle) else {
        return PARAM_ERR;
    };
    let mut q = lock(&queue);
    q.running = true;
    // SAFETY: the queue's own stream.
    match unsafe { sdl3::audio::resume(q.stream) } {
        true => SUCCESS,
        false => {
            q.running = false;
            CANNOT_START
        }
    }
}

/// `OSStatus AudioQueuePause(AudioQueueRef)`
pub unsafe extern "C" fn AudioQueuePause(handle: *mut c_void) -> i32 {
    let Some(queue) = queue_for(handle) else {
        return PARAM_ERR;
    };
    let mut q = lock(&queue);
    q.running = false;
    unsafe { sdl3::audio::pause(q.stream) };
    SUCCESS
}

/// `OSStatus AudioQueueStop(AudioQueueRef, Boolean inImmediate)`
///
/// FIXME (ACCURACY): a deferred stop should play what is enqueued first; both
/// stop at once and drop it, which only shortens the tail of what was playing.
pub unsafe extern "C" fn AudioQueueStop(handle: *mut c_void, _immediate: u8) -> i32 {
    let Some(queue) = queue_for(handle) else {
        return PARAM_ERR;
    };
    let mut q = lock(&queue);
    q.running = false;
    q.enqueued.clear();
    unsafe {
        sdl3::audio::pause(q.stream);
        sdl3::audio::clear(q.stream);
    }
    SUCCESS
}

/// `OSStatus AudioQueueFlush(AudioQueueRef)`. A flush pushes a codec's
/// remaining frames out; linear PCM has none held back.
pub unsafe extern "C" fn AudioQueueFlush(handle: *mut c_void) -> i32 {
    match queue_for(handle) {
        Some(_) => SUCCESS,
        None => PARAM_ERR,
    }
}

/// `OSStatus AudioQueueDispose(AudioQueueRef, Boolean inImmediate)`
pub unsafe extern "C" fn AudioQueueDispose(handle: *mut c_void, _immediate: u8) -> i32 {
    let queue = {
        let mut queues = QUEUES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = queues.iter().position(|(h, _)| *h == handle as usize) else {
            return PARAM_ERR;
        };
        queues.remove(index).1
    };
    let stream = std::mem::replace(&mut lock(&queue).stream, std::ptr::null_mut());
    // Without the queue's lock: destroying waits for a running `feed`, whose
    // callback may be enqueueing.
    unsafe { sdl3::audio::destroy(stream) };
    let mut q = lock(&queue);
    q.enqueued.clear();
    for buffer in std::mem::take(&mut q.buffers) {
        unsafe { free_buffer(buffer) };
    }
    SUCCESS
}

/// `OSStatus AudioQueueSetProperty(AudioQueueRef, AudioQueuePropertyID, const
/// void *, UInt32)`
///
/// The device and the channel layout are accepted and not acted on: a queue
/// plays to the host's default device, in the channel order SDL defines, which
/// is the one the program's interleaved data is in for mono and stereo.
pub unsafe extern "C" fn AudioQueueSetProperty(
    handle: *mut c_void,
    id: u32,
    _data: *const c_void,
    _size: u32,
) -> i32 {
    if queue_for(handle).is_none() {
        return PARAM_ERR;
    }
    let status = match id {
        PROPERTY_CURRENT_DEVICE | PROPERTY_CHANNEL_LAYOUT => SUCCESS,
        _ => INVALID_PROPERTY,
    };
    coreaudio::trace(
        format_args!(
            "AudioQueueSetProperty {handle:p} {}",
            coreaudio::trace_code(id)
        ),
        status,
    );
    status
}

exports::exports! {
    functions {
        "AudioQueueNewOutput" => AudioQueueNewOutput,
        "AudioQueueNewInput" => AudioQueueNewInput,
        "AudioQueueAllocateBuffer" => AudioQueueAllocateBuffer,
        "AudioQueueFreeBuffer" => AudioQueueFreeBuffer,
        "AudioQueueEnqueueBuffer" => AudioQueueEnqueueBuffer,
        "AudioQueueStart" => AudioQueueStart,
        "AudioQueuePause" => AudioQueuePause,
        "AudioQueueStop" => AudioQueueStop,
        "AudioQueueFlush" => AudioQueueFlush,
        "AudioQueueDispose" => AudioQueueDispose,
        "AudioQueueSetProperty" => AudioQueueSetProperty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interleaved(flags: u32, bits: u32, channels: u32) -> Format {
        Format {
            sample_rate: 48000.0,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: flags,
            bytes_per_packet: channels * bits / 8,
            frames_per_packet: 1,
            bytes_per_frame: channels * bits / 8,
            channels_per_frame: channels,
            bits_per_channel: bits,
            reserved: 0,
        }
    }

    #[test]
    fn maps_the_formats_sdl_offers_a_queue() {
        use sdl3::audio::format::*;
        let cases = [
            (IS_FLOAT, 32, F32),
            (IS_FLOAT | IS_BIG_ENDIAN, 32, F32BE),
            (IS_SIGNED_INTEGER, 32, S32),
            (IS_SIGNED_INTEGER, 16, S16),
            (IS_SIGNED_INTEGER | IS_BIG_ENDIAN, 16, S16BE),
            (IS_SIGNED_INTEGER, 8, S8),
            (0, 8, U8),
        ];
        for (flags, bits, want) in cases {
            let spec = sdl_spec(&interleaved(flags, bits, 2)).expect("mapped");
            assert_eq!((spec.format, spec.channels, spec.freq), (want, 2, 48000));
        }
    }

    #[test]
    fn refuses_layouts_a_queue_buffer_cannot_hold() {
        let planar = interleaved(IS_FLOAT | IS_NON_INTERLEAVED, 32, 2);
        assert!(sdl_spec(&planar).is_none());
        let unpacked = Format {
            bytes_per_frame: 12,
            ..interleaved(IS_SIGNED_INTEGER, 16, 2)
        };
        assert!(sdl_spec(&unpacked).is_none());
        let aac = Format {
            format_id: u32::from_be_bytes(*b"aac "),
            ..interleaved(IS_FLOAT, 32, 2)
        };
        assert!(sdl_spec(&aac).is_none());
    }
}
