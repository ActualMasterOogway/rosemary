//! `AudioUnitGetProperty` and `AudioUnitSetProperty`.
//!
//! A handful of properties, and a name for everything else. A property this
//! does not know is `kAudioUnitErr_InvalidProperty`, which is the framework's
//! own answer for one the unit does not have - so a guest that checks gets
//! told, and one that does not is no worse off than it would be on Apple's.
//!
//! Three of them belong to the HAL output unit rather than to an `AudioUnit`
//! in general - `EnableIO`, `CurrentDevice` and `AudioChannelLayout`. A program
//! that wants a *particular* output device asks for the `ahal` subtype instead
//! of `def `, and then configures it through those three before it initializes.
//! There is one device here and it is always the one that opens, so what they
//! do is agree or refuse: the answer to "enable input" or "use that other
//! device" has to be an error, because the alternative is a program configured
//! for something this runtime will not do and no way for it to find out.

use std::ffi::c_void;

use crate::render::{Callback, Proc};
use crate::status::*;
use crate::stream::Format;

/// `kAudioUnitProperty_StreamFormat`.
const STREAM_FORMAT: u32 = 8;
/// `kAudioUnitProperty_SetRenderCallback`, and the older spelling that carries
/// the identical `AURenderCallbackStruct`.
const SET_RENDER_CALLBACK: u32 = 23;
const SET_INPUT_CALLBACK: u32 = 22;
/// `kAudioOutputUnitProperty_SetInputCallback`, which despite the similar name
/// is the *record* callback: the unit calls it when it has captured audio, and
/// it answers by calling `AudioUnitRender` on the input bus. Refused, because
/// this HAL has no capture device - installing it as the render callback would
/// have the playback stream pulling a function that expects to be handed
/// recorded frames.
const OUTPUT_SET_INPUT_CALLBACK: u32 = 2005;
/// `kAudioUnitProperty_MaximumFramesPerSlice`, which a guest reads to size its
/// own mixing buffer.
const MAXIMUM_FRAMES_PER_SLICE: u32 = 14;
/// `kAudioUnitProperty_AudioChannelLayout`: which speaker each channel of the
/// stream format is meant for.
const AUDIO_CHANNEL_LAYOUT: u32 = 19;
/// `kAudioOutputUnitProperty_CurrentDevice`, the HAL device an `ahal` unit
/// plays through.
const CURRENT_DEVICE: u32 = 2000;
/// `kAudioOutputUnitProperty_EnableIO`, one flag per scope.
const ENABLE_IO: u32 = 2003;

/// `AudioUnitScope`.
mod scope {
    pub const INPUT: u32 = 1;
    pub const OUTPUT: u32 = 2;
}

/// The largest slice this crate will ever ask for. It is a constant rather than
/// a tuning knob, because a guest allocates against the number it is told.
const MAXIMUM_FRAMES: u32 = crate::render::FRAMES_PER_PULL_MAX;

/// How many channels an `AudioChannelLayout` describes, or `None` for one too
/// short to read or naming no channels.
///
/// The structure is a tag, a bitmap and a count of trailing descriptions, and
/// which of the three carries the answer is what the tag says. Getting this
/// wrong is a layout accepted against a stream format with a different number
/// of channels, which plays as the wrong speakers rather than as an error.
///
/// # Safety
///
/// `data` must be readable for `size` bytes.
unsafe fn layout_channels(data: *const c_void, size: u32) -> Option<u32> {
    /// `kAudioChannelLayoutTag_UseChannelDescriptions`.
    const USE_DESCRIPTIONS: u32 = 0;
    /// `kAudioChannelLayoutTag_UseChannelBitmap`.
    const USE_BITMAP: u32 = 1 << 16;

    if (size as usize) < 3 * size_of::<u32>() {
        return None;
    }
    // SAFETY: the caller's obligation, and the header was just measured.
    let header = unsafe { std::slice::from_raw_parts(data.cast::<u32>(), 3) };
    let channels = match header[0] {
        USE_DESCRIPTIONS => header[2],
        USE_BITMAP => header[1].count_ones(),
        // Every other tag spells its channel count in its low half.
        tag => tag & 0xffff,
    };
    (channels != 0).then_some(channels)
}

fn carries_a_callback(id: u32) -> bool {
    matches!(id, SET_RENDER_CALLBACK | SET_INPUT_CALLBACK)
}

/// `OSStatus AudioUnitSetProperty(AudioUnit, AudioUnitPropertyID,
/// AudioUnitScope, AudioUnitElement, const void *inData, UInt32 inDataSize)`
///
/// # Safety
///
/// `data` must be null or `size` readable bytes, and for a callback property it
/// must hold an `AURenderCallbackStruct` whose function pointer is *host*
/// callable - the caller substitutes a guest's for a thunk before it gets here.
pub unsafe extern "C" fn AudioUnitSetProperty(
    handle: *mut c_void,
    id: u32,
    scope: u32,
    element: u32,
    data: *const c_void,
    size: u32,
) -> i32 {
    let Some(unit) = crate::unit_for(handle) else {
        // SAFETY: the caller's obligation, unchanged by the forward.
        return unsafe { crate::foreign::set_property(handle, id, scope, element, data, size) }
            .unwrap_or(INVALID_COMPONENT);
    };
    // SAFETY: the caller's obligation.
    let status = unsafe { set(&unit, id, scope, data, size) };
    crate::trace::say(
        format_args!("set property {id} scope {scope} element {element}"),
        status,
    );
    status
}

/// # Safety
///
/// As [`AudioUnitSetProperty`]'s.
unsafe fn set(
    unit: &std::sync::Mutex<crate::Unit>,
    id: u32,
    scope: u32,
    data: *const c_void,
    size: u32,
) -> i32 {
    if data.is_null() {
        return PARAM_ERROR;
    }
    let mut unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    match id {
        STREAM_FORMAT => {
            if (size as usize) < size_of::<Format>() {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: the caller's obligation, and the size was just checked.
            let format = unsafe { *(data as *const Format) };
            if crate::stream::sdl_spec(&format).is_none() {
                return FORMAT_NOT_SUPPORTED;
            }
            // A format set after the device is open would have the guest's
            // callback filling one format while SDL converted another.
            if !unit.stream.is_null() {
                return CANNOT_DO_IN_CURRENT_CONTEXT;
            }
            unit.format = format;
            SUCCESS
        }
        id if carries_a_callback(id) => {
            if (size as usize) < size_of::<[usize; 2]>() {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: as above; an `AURenderCallbackStruct` is a function
            // pointer and a cookie.
            let raw = unsafe { *(data as *const [*mut c_void; 2]) };
            unit.callback = (!raw[0].is_null()).then(|| Callback {
                // SAFETY: the caller's obligation - this pointer is host
                // callable with the `AURenderCallback` prototype.
                proc: unsafe { std::mem::transmute::<*mut c_void, Proc>(raw[0]) },
                ref_con: raw[1],
            });
            SUCCESS
        }
        MAXIMUM_FRAMES_PER_SLICE => {
            // Accepted and ignored: this crate never asks for more than
            // `MAXIMUM_FRAMES`, so a guest lowering the ceiling changes nothing
            // and a guest raising it gets what it asked for anyway.
            match size >= 4 {
                true => SUCCESS,
                false => INVALID_PROPERTY_VALUE,
            }
        }
        AUDIO_CHANNEL_LAYOUT => {
            // SAFETY: the caller's obligation, and the size is checked against
            // the header before any of it is read.
            let Some(channels) = (unsafe { layout_channels(data, size) }) else {
                return INVALID_PROPERTY_VALUE;
            };
            // Only the count is honoured, in the standard order SDL plays.
            // A layout naming the same channels in a different order would be
            // played in this one, which is the pair of speakers swapped and
            // nothing to say so.
            match channels == unit.format.channels_per_frame {
                true => SUCCESS,
                false => FORMAT_NOT_SUPPORTED,
            }
        }
        ENABLE_IO => {
            if size < 4 {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: the caller's obligation, and the size was just checked.
            let enabled = unsafe { *(data as *const u32) } != 0;
            match scope {
                // This unit is an output, and disabling that leaves a program
                // rendering into a unit that will never pull it.
                scope::OUTPUT => match enabled {
                    true => SUCCESS,
                    false => INVALID_PROPERTY_VALUE,
                },
                // There is no capture device behind this HAL, so enabling
                // input is refused here rather than at the first empty buffer.
                scope::INPUT => match enabled {
                    true => INVALID_PROPERTY_VALUE,
                    false => SUCCESS,
                },
                _ => INVALID_SCOPE,
            }
        }
        // As `ENABLE_IO` on the input scope: there is nothing to record from,
        // and this is where a program still checks.
        OUTPUT_SET_INPUT_CALLBACK => INVALID_PROPERTY,
        CURRENT_DEVICE => {
            if size < 4 {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: the caller's obligation, and the size was just checked.
            let device = unsafe { *(data as *const u32) };
            match device == crate::hal::OUTPUT_DEVICE {
                true => SUCCESS,
                false => INVALID_PROPERTY_VALUE,
            }
        }
        _ => INVALID_PROPERTY,
    }
}

/// `OSStatus AudioUnitGetProperty(AudioUnit, AudioUnitPropertyID,
/// AudioUnitScope, AudioUnitElement, void *outData, UInt32 *ioDataSize)`
///
/// # Safety
///
/// `out` must be null or writable for `*size` bytes, and `size` must be null or
/// a writable `UInt32`.
pub unsafe extern "C" fn AudioUnitGetProperty(
    handle: *mut c_void,
    id: u32,
    scope: u32,
    element: u32,
    out: *mut c_void,
    size: *mut u32,
) -> i32 {
    let Some(unit) = crate::unit_for(handle) else {
        // SAFETY: the caller's obligation, unchanged by the forward.
        return unsafe { crate::foreign::get_property(handle, id, scope, element, out, size) }
            .unwrap_or(INVALID_COMPONENT);
    };
    if size.is_null() {
        return PARAM_ERROR;
    }
    let unit = unit.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the caller's obligation.
    let capacity = unsafe { *size };
    match id {
        STREAM_FORMAT => {
            let wanted = size_of::<Format>() as u32;
            // SAFETY: the caller's obligation.
            unsafe { *size = wanted };
            if out.is_null() {
                // A null `outData` is how a guest asks only for the size.
                return SUCCESS;
            }
            if capacity < wanted {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: the caller's obligation, and the capacity was checked.
            unsafe { *(out as *mut Format) = unit.format };
            SUCCESS
        }
        MAXIMUM_FRAMES_PER_SLICE => {
            // SAFETY: the caller's obligation.
            unsafe { *size = 4 };
            if out.is_null() {
                return SUCCESS;
            }
            if capacity < 4 {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: as above.
            unsafe { *(out as *mut u32) = MAXIMUM_FRAMES };
            SUCCESS
        }
        // Read back so that a program can confirm what it set: this unit is an
        // output on the one device below it, whatever scope is asked about.
        ENABLE_IO | CURRENT_DEVICE => {
            let value = match id {
                CURRENT_DEVICE => crate::hal::OUTPUT_DEVICE,
                _ => u32::from(scope == scope::OUTPUT),
            };
            // SAFETY: the caller's obligation.
            unsafe { *size = 4 };
            if out.is_null() {
                return SUCCESS;
            }
            if capacity < 4 {
                return INVALID_PROPERTY_VALUE;
            }
            // SAFETY: as above, and the capacity was checked.
            unsafe { *(out as *mut u32) = value };
            SUCCESS
        }
        _ => INVALID_PROPERTY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> *mut c_void {
        let mut handle = std::ptr::null_mut();
        // SAFETY: a writable slot and this crate's own component.
        unsafe {
            crate::AudioComponentInstanceNew(crate::test_component(), &mut handle);
        }
        handle
    }

    #[test]
    fn the_stream_format_round_trips() {
        let handle = unit();
        let wanted = Format {
            sample_rate: 48000.0,
            ..Format::default()
        };
        // SAFETY: live structures of the sizes declared.
        unsafe {
            assert_eq!(
                AudioUnitSetProperty(
                    handle,
                    STREAM_FORMAT,
                    1,
                    0,
                    (&wanted as *const Format).cast(),
                    size_of::<Format>() as u32,
                ),
                SUCCESS
            );
            let mut read = Format::default();
            let mut size = size_of::<Format>() as u32;
            assert_eq!(
                AudioUnitGetProperty(
                    handle,
                    STREAM_FORMAT,
                    1,
                    0,
                    (&mut read as *mut Format).cast(),
                    &mut size,
                ),
                SUCCESS
            );
            assert_eq!(read, wanted);
            crate::AudioComponentInstanceDispose(handle);
        }
    }

    /// A format the device cannot be opened for is refused *here*, where the
    /// guest still checks, rather than at start where it no longer does.
    #[test]
    fn an_unsupported_format_is_refused_when_it_is_set() {
        let handle = unit();
        let bad = Format {
            bits_per_channel: 24,
            format_flags: 4,
            ..Format::default()
        };
        // SAFETY: a live structure of the size declared.
        unsafe {
            assert_eq!(
                AudioUnitSetProperty(
                    handle,
                    STREAM_FORMAT,
                    1,
                    0,
                    (&bad as *const Format).cast(),
                    size_of::<Format>() as u32,
                ),
                FORMAT_NOT_SUPPORTED
            );
            crate::AudioComponentInstanceDispose(handle);
        }
    }

    /// A property nothing here has is named rather than silently accepted: a
    /// guest that sets one and hears nothing would otherwise have no way to tell
    /// which of the two it was.
    #[test]
    fn an_unknown_property_is_an_invalid_property() {
        let handle = unit();
        let value = 0u32;
        // SAFETY: a live word.
        unsafe {
            assert_eq!(
                AudioUnitSetProperty(handle, 9999, 0, 0, (&value as *const u32).cast(), 4),
                INVALID_PROPERTY
            );
            crate::AudioComponentInstanceDispose(handle);
        }
    }

    /// The three properties a HAL output unit is configured through, which is
    /// what a program asking for the `ahal` subtype does before it initializes.
    /// Each of them has exactly one answer here, and the other answer has to be
    /// an error rather than a silent agreement: a program told it may record,
    /// or may play through some other device, has configured itself for
    /// something that will never happen.
    #[test]
    fn the_output_unit_is_configured_and_disagreement_is_refused() {
        let handle = unit();
        let word = |value: u32, id: u32, scope: u32| {
            // SAFETY: a live word of the size declared.
            unsafe { AudioUnitSetProperty(handle, id, scope, 0, (&value as *const u32).cast(), 4) }
        };
        assert_eq!(word(1, ENABLE_IO, scope::OUTPUT), SUCCESS);
        assert_eq!(word(0, ENABLE_IO, scope::INPUT), SUCCESS);
        assert_eq!(word(1, ENABLE_IO, scope::INPUT), INVALID_PROPERTY_VALUE);
        assert_eq!(word(0, ENABLE_IO, scope::OUTPUT), INVALID_PROPERTY_VALUE);
        assert_eq!(word(0, ENABLE_IO, 0), INVALID_SCOPE);

        assert_eq!(word(crate::hal::OUTPUT_DEVICE, CURRENT_DEVICE, 0), SUCCESS);
        assert_eq!(word(99, CURRENT_DEVICE, 0), INVALID_PROPERTY_VALUE);

        // `kAudioChannelLayoutTag_Stereo`, against the default format's two
        // channels, and a five-channel tag against the same.
        let layout = |tag: u32| {
            let value: [u32; 8] = [tag, 0, 0, 0, 0, 0, 0, 0];
            // SAFETY: a live structure of the size declared.
            unsafe {
                AudioUnitSetProperty(
                    handle,
                    AUDIO_CHANNEL_LAYOUT,
                    scope::INPUT,
                    0,
                    value.as_ptr().cast(),
                    size_of_val(&value) as u32,
                )
            }
        };
        assert_eq!(layout((101 << 16) | 2), SUCCESS);
        assert_eq!(layout((120 << 16) | 5), FORMAT_NOT_SUPPORTED);

        // SAFETY: a unit this test made.
        unsafe { crate::AudioComponentInstanceDispose(handle) };
    }

    #[test]
    fn the_render_callback_and_its_cookie_are_both_kept() {
        unsafe extern "C" fn never(
            _: *mut c_void,
            _: *mut u32,
            _: *const crate::render::TimeStamp,
            _: u32,
            _: u32,
            _: *mut crate::render::AudioBufferList,
        ) -> i32 {
            SUCCESS
        }
        let handle = unit();
        let installed: [*mut c_void; 2] = [never as *mut c_void, 0x1234 as *mut c_void];
        // SAFETY: a live pair of the size declared.
        unsafe {
            assert_eq!(
                AudioUnitSetProperty(
                    handle,
                    SET_RENDER_CALLBACK,
                    1,
                    0,
                    installed.as_ptr().cast(),
                    16,
                ),
                SUCCESS
            );
        }
        let unit = crate::unit_for(handle).expect("a unit");
        let callback = unit.lock().unwrap().callback.expect("installed");
        assert_eq!(callback.ref_con, 0x1234 as *mut c_void);
        // SAFETY: a unit this test made.
        unsafe { crate::AudioComponentInstanceDispose(handle) };
    }
}
