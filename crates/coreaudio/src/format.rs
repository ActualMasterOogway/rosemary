use std::ffi::c_void;

use crate::status::{PARAM_ERROR, SUCCESS};
use crate::stream::Format;

/// `kAudioFormatUnknownFormatError`, `'!fmt'`.
const UNKNOWN_FORMAT: i32 = i32::from_be_bytes(*b"!fmt");

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// `kAudioFormatProperty_FormatInfo`: complete a partly filled description.
const FORMAT_INFO: u32 = fourcc(b"fmti");

/// `kAudioFormatLinearPCM`.
const LINEAR_PCM: u32 = fourcc(b"lpcm");

/// `kAudioFormatFlagIsNonInterleaved`.
const NON_INTERLEAVED: u32 = 1 << 5;

/// Fill in what a caller left at zero.
///
/// The rules are the header's: one frame per packet for PCM, and a frame is
/// every channel's sample side by side - except for a non-interleaved format,
/// where `mBytesPerFrame` counts one channel only.
fn completed(mut format: Format) -> Option<Format> {
    if format.format_id != LINEAR_PCM || format.bits_per_channel == 0 {
        return None;
    }
    if format.channels_per_frame == 0 {
        return None;
    }
    if format.frames_per_packet == 0 {
        format.frames_per_packet = 1;
    }
    let per_channel = format.bits_per_channel.div_ceil(8);
    if format.bytes_per_frame == 0 {
        format.bytes_per_frame = match format.format_flags & NON_INTERLEAVED != 0 {
            true => per_channel,
            false => per_channel * format.channels_per_frame,
        };
    }
    if format.bytes_per_packet == 0 {
        format.bytes_per_packet = format.bytes_per_frame * format.frames_per_packet;
    }
    Some(format)
}

/// `OSStatus AudioFormatGetProperty(AudioFormatPropertyID, UInt32, const void *,
/// UInt32 *, void *)`
///
/// # Safety
///
/// `specifier` must be readable for `specifier_size` bytes, `size` a writable
/// `UInt32`, and `data` null or writable for `*size` bytes.
pub unsafe extern "C" fn AudioFormatGetProperty(
    property: u32,
    specifier_size: u32,
    specifier: *const c_void,
    size: *mut u32,
    data: *mut c_void,
) -> i32 {
    if size.is_null() {
        return PARAM_ERROR;
    }
    if property != FORMAT_INFO {
        unsafe { *size = 0 };
        return UNKNOWN_FORMAT;
    }
    // `kAudioFormatProperty_FormatInfo` reads the description out of the
    // *specifier* and writes the completed one back through `data` - a caller
    // usually passes the same address for both.
    let source = match (specifier.is_null(), data.is_null()) {
        (false, _) if specifier_size as usize >= size_of::<Format>() => specifier as *const Format,
        (true, false) if unsafe { *size } as usize >= size_of::<Format>() => data as *const Format,
        _ => {
            unsafe { *size = 0 };
            return PARAM_ERROR;
        }
    };
    // SAFETY: checked above to be readable for a whole description.
    let Some(format) = completed(unsafe { *source }) else {
        unsafe { *size = 0 };
        return UNKNOWN_FORMAT;
    };
    if data.is_null() {
        unsafe { *size = size_of::<Format>() as u32 };
        return SUCCESS;
    }
    // SAFETY: the caller says `data` is writable for `*size` bytes, which was
    // checked to be at least a whole description.
    unsafe {
        if (*size as usize) < size_of::<Format>() {
            *size = 0;
            return PARAM_ERROR;
        }
        *(data as *mut Format) = format;
        *size = size_of::<Format>() as u32;
    }
    SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case every caller uses: a rate, a channel count and a sample width,
    /// with the three derived fields left at zero.
    #[test]
    fn the_derived_fields_are_filled_in() {
        let sparse = Format {
            sample_rate: 44100.0,
            format_id: LINEAR_PCM,
            format_flags: 1,
            bits_per_channel: 32,
            channels_per_frame: 2,
            bytes_per_frame: 0,
            bytes_per_packet: 0,
            frames_per_packet: 0,
            reserved: 0,
        };
        let full = completed(sparse).expect("linear PCM completes");
        assert_eq!(full.frames_per_packet, 1);
        assert_eq!(full.bytes_per_frame, 8, "two channels of four bytes");
        assert_eq!(full.bytes_per_packet, 8);
    }

    /// A non-interleaved frame is *one* channel wide, which is the rule in the
    /// header that is easiest to get backwards.
    #[test]
    fn a_non_interleaved_frame_is_one_channel() {
        let planar = Format {
            format_flags: 1 | NON_INTERLEAVED,
            bytes_per_frame: 0,
            bytes_per_packet: 0,
            ..Format::default()
        };
        assert_eq!(completed(planar).unwrap().bytes_per_frame, 4);
    }

    /// A format with no codec here is refused rather than completed with
    /// arithmetic that only holds for PCM.
    #[test]
    fn a_compressed_format_has_no_derived_fields() {
        let aac = Format {
            format_id: fourcc(b"aac "),
            ..Format::default()
        };
        assert!(completed(aac).is_none());
    }
}
