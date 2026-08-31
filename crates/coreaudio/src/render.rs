//! Pulling the guest's render callback, on SDL's audio thread.
//!
//! The `AudioBufferList` and the `AudioTimeStamp` are built here because the
//! callback's whole interface is those two structures: a guest reads
//! `mNumberBuffers` and writes through `mData`, and gets the frame it is filling
//! from `mSampleTime`. Both layouts are ABI.

use std::ffi::c_void;

use crate::status::SUCCESS;
use crate::stream::Format;

/// `AURenderCallback`.
pub type Proc = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const TimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> i32;

/// What `kAudioUnitProperty_SetRenderCallback` installs: the function and the
/// cookie the guest wants handed back to it.
#[derive(Clone, Copy)]
pub struct Callback {
    pub proc: Proc,
    pub ref_con: *mut c_void,
}

/// `AudioTimeStamp`. Sixty-four bytes, and a guest reads `mSampleTime` out of
/// it to know how far into the stream it is.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TimeStamp {
    pub sample_time: f64,
    pub host_time: u64,
    pub rate_scalar: f64,
    pub word_clock_time: u64,
    /// `SMPTETime`, which nothing here fills in.
    smpte: [u8; 24],
    pub flags: u32,
    pub reserved: u32,
}

const _: () = {
    assert!(size_of::<TimeStamp>() == 64);
    assert!(std::mem::offset_of!(TimeStamp, flags) == 56);
};

/// `kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid`.
const TIME_VALID: u32 = 1 | 2;

/// `AudioBuffer`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioBuffer {
    pub channels: u32,
    pub byte_size: u32,
    pub data: *mut c_void,
}

/// `AudioBufferList`, whose `mBuffers` is a trailing array the header declares
/// as one element.
#[repr(C)]
pub struct AudioBufferList {
    pub count: u32,
    pub buffers: [AudioBuffer; MAX_BUFFERS],
}

const _: () = assert!(std::mem::offset_of!(AudioBufferList, buffers) == 8);

/// How many channels a non-interleaved format may have before this refuses to
/// build a list for it.
///
/// Eight, because that is a 7.1 stream and a game is one or two. A format
/// asking for more is silence rather than a heap allocation on the audio
/// thread.
pub const MAX_BUFFERS: usize = 8;

/// Frames pulled per call, which is also the size of the scratch below.
///
/// SDL asks in bytes and an `AURenderCallback` answers in frames, so one of the
/// two has to be chopped into fixed pieces. 512 is a hundredth of a second at
/// 48 kHz: small enough that a guest mixing per-buffer stays responsive, large
/// enough that the per-call cost of re-entering guest code is not the work.
const FRAMES_PER_PULL: usize = 512;

/// [`FRAMES_PER_PULL`] as the number `kAudioUnitProperty_MaximumFramesPerSlice`
/// reports, which a guest allocates its own mixing buffer against.
pub const FRAMES_PER_PULL_MAX: u32 = FRAMES_PER_PULL as u32;

/// Silence for a unit that cannot render, in the same units as the scratch.
///
/// A stream fed nothing underruns, and SDL reports an underrun as a gap the
/// device fills with whatever was there. Zeroes are the honest answer for "the
/// guest has no callback installed yet".
fn feed_silence(stream: sdl3::AudioStream, bytes: usize) {
    let zeros = vec![0u8; bytes.min(FRAMES_PER_PULL * MAX_BUFFERS * 4)];
    // SAFETY: a stream this crate opened, and a live slice.
    unsafe { sdl3::audio::put(stream, &zeros) };
}

/// SDL wants `additional` more bytes: pull them from the guest.
///
/// # Safety
///
/// `userdata` is the unit handle `stream::open` passed, and SDL calls this on
/// its own audio thread with a stream this crate opened.
pub unsafe extern "C" fn feed(
    userdata: *mut c_void,
    stream: sdl3::AudioStream,
    additional: i32,
    _total: i32,
) {
    if additional <= 0 {
        return;
    }
    let Some(unit) = crate::unit_for(userdata) else {
        return;
    };
    // The lock is held across the guest's callback deliberately: `AudioUnitStop`
    // and `AudioComponentInstanceDispose` take it too, so a guest cannot free
    // what it is being asked to fill.
    let mut unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    if !unit.running {
        return;
    }
    let (format, callback) = (unit.format, unit.callback);
    let Some(callback) = callback else {
        feed_silence(stream, additional as usize);
        return;
    };

    let frame_bytes = match format.non_interleaved() {
        true => format.bytes_per_frame() * format.channels_per_frame as usize,
        false => format.bytes_per_frame(),
    };
    if frame_bytes == 0 {
        return;
    }

    let mut wanted = additional as usize;
    let mut scratch = vec![0u8; FRAMES_PER_PULL * frame_bytes];
    while wanted > 0 {
        let frames = (wanted / frame_bytes).clamp(1, FRAMES_PER_PULL);
        let filled = &mut scratch[..frames * frame_bytes];
        filled.fill(0);
        if !pull(&mut unit, callback, format, frames, filled) {
            feed_silence(stream, filled.len());
            return;
        }
        // SAFETY: a stream this crate opened, and a live slice.
        unsafe { sdl3::audio::put(stream, filled) };
        wanted = wanted.saturating_sub(filled.len());
    }
}

/// One call into the guest, filling `into`.
///
/// `false` when the guest's callback reports an error, which is the one thing
/// this cannot paper over: an `AURenderCallback` that fails has left the buffer
/// in whatever state it likes.
fn pull(
    unit: &mut crate::Unit,
    callback: Callback,
    format: Format,
    frames: usize,
    into: &mut [u8],
) -> bool {
    let channels = format.channels_per_frame as usize;
    let mut list = AudioBufferList {
        count: 1,
        buffers: [AudioBuffer {
            channels: 0,
            byte_size: 0,
            data: std::ptr::null_mut(),
        }; MAX_BUFFERS],
    };
    match format.non_interleaved() {
        // One buffer per channel, each a contiguous run of the scratch. The
        // guest writes them separately and `interleave` puts them back together,
        // because SDL takes one interleaved stream.
        true => {
            if channels == 0 || channels > MAX_BUFFERS {
                return false;
            }
            let per = frames * format.bytes_per_frame();
            list.count = channels as u32;
            for (i, buffer) in list.buffers[..channels].iter_mut().enumerate() {
                buffer.channels = 1;
                buffer.byte_size = per as u32;
                buffer.data = into[i * per..].as_mut_ptr().cast();
            }
        }
        false => {
            list.buffers[0].channels = channels as u32;
            list.buffers[0].byte_size = into.len() as u32;
            list.buffers[0].data = into.as_mut_ptr().cast();
        }
    }

    let now = TimeStamp {
        sample_time: unit.rendered as f64,
        host_time: 0,
        rate_scalar: 1.0,
        flags: TIME_VALID,
        ..TimeStamp::default()
    };
    let mut action_flags = 0u32;
    // SAFETY: `callback.proc` is a host-callable pointer this runtime was handed
    // for the guest's function, and every structure it is given outlives the
    // call.
    let status = unsafe {
        (callback.proc)(
            callback.ref_con,
            &mut action_flags,
            &now,
            0,
            frames as u32,
            &mut list,
        )
    };
    if status != SUCCESS {
        return false;
    }
    unit.rendered += frames as u64;
    if format.non_interleaved() {
        interleave(into, channels, frames, format.bytes_per_frame());
    }
    true
}

/// Turn `channels` contiguous per-channel runs into one interleaved stream, in
/// place.
///
/// Through a copy of the input rather than a swap chain: the permutation is not
/// a rotation, and doing it in place without one is the kind of cleverness that
/// is wrong for three channels and right for two.
fn interleave(buffer: &mut [u8], channels: usize, frames: usize, sample: usize) {
    let planar = buffer.to_vec();
    for frame in 0..frames {
        for channel in 0..channels {
            let from = channel * frames * sample + frame * sample;
            let to = (frame * channels + channel) * sample;
            buffer[to..to + sample].copy_from_slice(&planar[from..from + sample]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two planar channels become one interleaved stream, sample by sample.
    #[test]
    fn planar_channels_interleave() {
        // Four frames of two channels, one byte each: `LLLLRRRR`.
        let mut buffer = *b"1234abcd";
        interleave(&mut buffer, 2, 4, 1);
        assert_eq!(&buffer, b"1a2b3c4d");
    }

    /// And with a wider sample, because the stride is the thing that is easy to
    /// get wrong.
    #[test]
    fn planar_channels_interleave_at_two_bytes_a_sample() {
        let mut buffer = *b"L1L2R1R2";
        interleave(&mut buffer, 2, 2, 2);
        assert_eq!(&buffer, b"L1R1L2R2");
    }
}
