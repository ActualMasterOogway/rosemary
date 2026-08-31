//! `AudioUnitGetProperty` and `AudioUnitSetProperty`.
//!
//! Three properties, and a name for everything else. A property this does not
//! know is `kAudioUnitErr_InvalidProperty`, which is the framework's own answer
//! for one the unit does not have - so a guest that checks gets told, and one
//! that does not is no worse off than it would be on Apple's.

use std::ffi::c_void;

use crate::render::{Callback, Proc};
use crate::status::*;
use crate::stream::Format;

/// `kAudioUnitProperty_StreamFormat`.
const STREAM_FORMAT: u32 = 8;
/// `kAudioUnitProperty_SetRenderCallback`, and the two older spellings that
/// carry the identical `AURenderCallbackStruct`.
const SET_RENDER_CALLBACK: u32 = 23;
const SET_INPUT_CALLBACK: u32 = 22;
const OUTPUT_SET_INPUT_CALLBACK: u32 = 2005;
/// `kAudioUnitProperty_MaximumFramesPerSlice`, which a guest reads to size its
/// own mixing buffer.
const MAXIMUM_FRAMES_PER_SLICE: u32 = 14;

/// The largest slice this crate will ever ask for. It is a constant rather than
/// a tuning knob, because a guest allocates against the number it is told.
const MAXIMUM_FRAMES: u32 = crate::render::FRAMES_PER_PULL_MAX;

fn carries_a_callback(id: u32) -> bool {
    matches!(
        id,
        SET_RENDER_CALLBACK | SET_INPUT_CALLBACK | OUTPUT_SET_INPUT_CALLBACK
    )
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
    _scope: u32,
    _element: u32,
    data: *const c_void,
    size: u32,
) -> i32 {
    let Some(unit) = crate::unit_for(handle) else {
        // SAFETY: the caller's obligation, unchanged by the forward.
        return unsafe { crate::foreign::set_property(handle, id, _scope, _element, data, size) }
            .unwrap_or(INVALID_COMPONENT);
    };
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
    _scope: u32,
    _element: u32,
    out: *mut c_void,
    size: *mut u32,
) -> i32 {
    let Some(unit) = crate::unit_for(handle) else {
        // SAFETY: the caller's obligation, unchanged by the forward.
        return unsafe { crate::foreign::get_property(handle, id, _scope, _element, out, size) }
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
