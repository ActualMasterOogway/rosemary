//! The stream format a guest sets, and the SDL device it becomes.

use std::ffi::c_void;

use crate::Unit;

/// `kAudioFormatLinearPCM`.
const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

/// `AudioFormatFlags`, the four an output stream can carry.
mod flags {
    pub const IS_FLOAT: u32 = 1 << 0;
    pub const IS_BIG_ENDIAN: u32 = 1 << 1;
    pub const IS_SIGNED_INTEGER: u32 = 1 << 2;
    pub const IS_NON_INTERLEAVED: u32 = 1 << 5;
}

/// `AudioStreamBasicDescription`. Forty bytes, and the layout is ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// What a unit reports before a guest sets anything: the canonical output
/// format, which is what Apple's default output unit starts at.
impl Default for Format {
    fn default() -> Self {
        Format {
            sample_rate: 44100.0,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: flags::IS_FLOAT,
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels_per_frame: 2,
            bits_per_channel: 32,
            reserved: 0,
        }
    }
}

impl Format {
    /// Whether the callback fills one buffer per channel rather than one
    /// interleaved buffer, which changes what `render` has to build.
    pub fn non_interleaved(&self) -> bool {
        self.format_flags & flags::IS_NON_INTERLEAVED != 0
    }

    /// Bytes in one frame of the buffer the callback fills.
    ///
    /// For a non-interleaved format `mBytesPerFrame` counts *one* channel, which
    /// is the header's rule and not a quirk of this runtime.
    pub fn bytes_per_frame(&self) -> usize {
        self.bytes_per_frame as usize
    }
}

/// The SDL spec a format asks for, or `None` for one SDL cannot name.
///
/// Refused rather than approximated. A format this converted silently - a
/// big-endian stream, or 24-bit samples - would play as noise, and the guest
/// checked the return value of `AudioUnitSetProperty` precisely so that it
/// would not have to.
pub fn sdl_spec(format: &Format) -> Option<sdl3::AudioSpec> {
    if format.format_id != FORMAT_LINEAR_PCM {
        return None;
    }
    if format.format_flags & flags::IS_BIG_ENDIAN != 0 {
        return None;
    }
    if format.sample_rate <= 0.0 || format.channels_per_frame == 0 {
        return None;
    }
    let sdl_format = match (
        format.format_flags & flags::IS_FLOAT != 0,
        format.bits_per_channel,
    ) {
        (true, 32) => sdl3::audio::format::F32,
        (false, 16) if format.format_flags & flags::IS_SIGNED_INTEGER != 0 => {
            sdl3::audio::format::S16
        }
        _ => return None,
    };
    Some(sdl3::AudioSpec {
        format: sdl_format,
        channels: format.channels_per_frame as i32,
        freq: format.sample_rate as i32,
    })
}

/// Open the device for `unit`, paused.
///
/// `handle` is the unit's own, and is what SDL hands back to the callback - so
/// one callback serves every unit and none of them has to outlive the table.
pub fn open(unit: &mut Unit, spec: sdl3::AudioSpec, handle: *mut c_void) -> bool {
    // SAFETY: `crate::render::feed` has the callback's prototype, and `handle`
    // is a key into a table that outlives the process rather than a borrow.
    let stream = unsafe { sdl3::audio::open_playback(spec, Some(crate::render::feed), handle) };
    match stream {
        Some(stream) => {
            unit.stream = stream;
            true
        }
        None => false,
    }
}

/// Close a unit's device, if it has one.
///
/// # Safety
///
/// No other thread may be inside this unit's stream. SDL's own destroy waits for
/// its callback, which is the only other caller.
pub unsafe fn close(unit: &mut Unit) {
    if unit.stream.is_null() {
        return;
    }
    // SAFETY: the caller's obligation; the handle came from `open`.
    unsafe { sdl3::audio::destroy(unit.stream) };
    unit.stream = std::ptr::null_mut();
    unit.running = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canonical_formats_map_and_the_rest_are_refused() {
        let canonical = Format::default();
        assert_eq!(
            sdl_spec(&canonical),
            Some(sdl3::AudioSpec {
                format: sdl3::audio::format::F32,
                channels: 2,
                freq: 44100,
            })
        );

        let s16 = Format {
            format_flags: flags::IS_SIGNED_INTEGER,
            bits_per_channel: 16,
            bytes_per_frame: 4,
            bytes_per_packet: 4,
            ..canonical
        };
        assert_eq!(sdl_spec(&s16).map(|s| s.format), Some(sdl3::audio::format::S16));

        // Each of these plays as noise if it is converted rather than refused.
        for bad in [
            Format { format_flags: flags::IS_FLOAT | flags::IS_BIG_ENDIAN, ..canonical },
            Format { bits_per_channel: 24, format_flags: flags::IS_SIGNED_INTEGER, ..canonical },
            Format { format_id: u32::from_be_bytes(*b"aac "), ..canonical },
            Format { sample_rate: 0.0, ..canonical },
            Format { channels_per_frame: 0, ..canonical },
        ] {
            assert_eq!(sdl_spec(&bad), None, "{bad:?}");
        }
    }
}
