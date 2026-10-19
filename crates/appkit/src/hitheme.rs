//! The Appearance Manager's metrics and HIToolbox's theme drawing calls.
//!
//! A widget toolkit sizes and draws native-looking controls with these rather
//! than through `NSCell`: it asks how wide a scroll bar is, then hands a rect
//! and a state to `HIThemeDraw...` with its own `CGContext`.

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;
use foundation::geometry::{NSPoint, NSRect, NSSize};

type OSStatus = i32;

const NO_ERR: OSStatus = 0;
/// `paramErr`, for a metric this table does not have.
const PARAM_ERR: OSStatus = -50;

/// `ThemeMetric` values and Aqua's answer for each, in points.
///
/// Only the metrics a program has been seen to ask for; the numbers are the
/// regular-size Aqua controls'.
const METRICS: &[(u32, i32)] = &[
    // kThemeMetricScrollBarWidth, kThemeMetricSmallScrollBarWidth
    (0, 15),
    (1, 11),
    // kThemeMetricCheckBoxHeight, kThemeMetricRadioButtonHeight
    (2, 18),
    (3, 18),
    // kThemeMetricEditTextWhitespace, kThemeMetricEditTextFrameOutset,
    // kThemeMetricListBoxFrameOutset, kThemeMetricFocusRectOutset
    (4, 3),
    (5, 1),
    (6, 1),
    (7, 4),
    // kThemeMetricDisclosureButtonHeight
    (17, 21),
    // kThemeMetricPushButtonHeight, kThemeMetricListHeaderHeight
    (19, 20),
    (20, 17),
    // kThemeMetricDisclosureButtonWidth
    (22, 21),
    // kThemeMetricLittleArrowsHeight, kThemeMetricLittleArrowsWidth
    (27, 23),
    (28, 13),
    // kThemeMetricPopupButtonHeight
    (30, 20),
    // kThemeMetricHSliderHeight, kThemeMetricVSliderWidth
    (41, 18),
    (45, 18),
    // kThemeMetricCheckBoxWidth, kThemeMetricRadioButtonWidth
    (50, 18),
    (52, 18),
    // kThemeMetricNormalProgressBarThickness
    (58, 14),
];

/// `OSStatus GetThemeMetric(ThemeMetric, SInt32 *)`
///
/// A metric not in the table is refused with zero written, so a caller that
/// ignores the status reads a defined value rather than its stack.
pub unsafe extern "C" fn GetThemeMetric(metric: u32, out: *mut i32) -> OSStatus {
    if out.is_null() {
        return PARAM_ERR;
    }
    let found = METRICS.iter().find(|(m, _)| *m == metric).map(|(_, v)| *v);
    // SAFETY: writable, per the caller.
    unsafe { out.write(found.unwrap_or(0)) };
    match found {
        Some(_) => NO_ERR,
        None => PARAM_ERR,
    }
}

// FIXME (ACCURACY): the `HIThemeDraw...` calls below accept every widget and
// draw nothing yet. Their target is the caller's `CGContext`, and that context
// cannot yet fill or stroke a path; until it can, these keep a toolkit's native
// theme running with its controls unpainted.

pub unsafe extern "C" fn HIThemeDrawButton(
    bounds: *const NSRect,
    _info: *const c_void,
    _context: CFTypeRef,
    _orientation: u32,
    label: *mut NSRect,
) -> OSStatus {
    if !label.is_null() && !bounds.is_null() {
        // SAFETY: both supplied by the caller. The label fills the button.
        unsafe { label.write(*bounds) };
    }
    NO_ERR
}

pub unsafe extern "C" fn HIThemeDrawFocusRect(
    _bounds: *const NSRect,
    _focused: u8,
    _context: CFTypeRef,
    _orientation: u32,
) -> OSStatus {
    NO_ERR
}

/// Every call shaped `(const HIRect *, const void *info, CGContextRef,
/// HIThemeOrientation)`.
pub unsafe extern "C" fn draw_in_rect(
    _bounds: *const NSRect,
    _info: *const c_void,
    _context: CFTypeRef,
    _orientation: u32,
) -> OSStatus {
    NO_ERR
}

/// `HIThemeDrawGrowBox(const HIPoint *, const HIThemeGrowBoxDrawInfo *,
/// CGContextRef, HIThemeOrientation)`
pub unsafe extern "C" fn HIThemeDrawGrowBox(
    _origin: *const NSPoint,
    _info: *const c_void,
    _context: CFTypeRef,
    _orientation: u32,
) -> OSStatus {
    NO_ERR
}

pub unsafe extern "C" fn HIThemeDrawMenuItem(
    _menu: *const NSRect,
    item: *const NSRect,
    _info: *const c_void,
    _context: CFTypeRef,
    _orientation: u32,
    content: *mut NSRect,
) -> OSStatus {
    if !content.is_null() && !item.is_null() {
        // SAFETY: both supplied by the caller.
        unsafe { content.write(*item) };
    }
    NO_ERR
}

pub unsafe extern "C" fn HIThemeDrawMenuSeparator(
    _menu: *const NSRect,
    _item: *const NSRect,
    _info: *const c_void,
    _context: CFTypeRef,
    _orientation: u32,
) -> OSStatus {
    NO_ERR
}

/// `HIThemeDrawTrack(const HIThemeTrackDrawInfo *, const HIRect *ghost,
/// CGContextRef, HIThemeOrientation)`
pub unsafe extern "C" fn HIThemeDrawTrack(
    _info: *const c_void,
    _ghost: *const NSRect,
    _context: CFTypeRef,
    _orientation: u32,
) -> OSStatus {
    NO_ERR
}

/// `HIThemeGetGrowBoxBounds(const HIPoint *, const HIThemeGrowBoxDrawInfo *,
/// HIRect *)`: Aqua's grow box is fifteen points square, its corner at the
/// origin.
pub unsafe extern "C" fn HIThemeGetGrowBoxBounds(
    origin: *const NSPoint,
    _info: *const c_void,
    bounds: *mut NSRect,
) -> OSStatus {
    if origin.is_null() || bounds.is_null() {
        return PARAM_ERR;
    }
    // SAFETY: both supplied by the caller.
    unsafe {
        bounds.write(NSRect {
            origin: *origin,
            size: NSSize {
                width: 15.0,
                height: 15.0,
            },
        })
    };
    NO_ERR
}

/// `HIThemeSetFill(ThemeBrush, void *, CGContextRef, HIThemeOrientation)`:
/// the brush's colour as the context's fill. Every brush is the window
/// background here, the one colour a toolkit fills with this.
pub unsafe extern "C" fn HIThemeSetFill(
    _brush: i16,
    _info: *mut c_void,
    context: CFTypeRef,
    _orientation: u32,
) -> OSStatus {
    // SAFETY: the caller's context, or null, which the call ignores.
    unsafe { crate::cgcontext::CGContextSetRGBFillColor(context, 0.93, 0.93, 0.93, 1.0) };
    NO_ERR
}

/// `cmProfileError`: the ColorSync calls below have no profile to answer
/// with, the display's colour being whatever the compositor makes of sRGB.
const CM_PROFILE_ERROR: OSStatus = -170;

/// `CMError CMGetProfileByAVID(CMDisplayIDType, CMProfileRef *)` and
/// `CMGetSystemProfile(CMProfileRef *)`: no profile, and a caller that treats
/// failure as "no colour management" is right to.
pub unsafe extern "C" fn CMGetProfileByAVID(_display: u32, profile: *mut *mut c_void) -> OSStatus {
    if !profile.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { profile.write(std::ptr::null_mut()) };
    }
    CM_PROFILE_ERROR
}

pub unsafe extern "C" fn CMGetSystemProfile(profile: *mut *mut c_void) -> OSStatus {
    // SAFETY: as above.
    unsafe { CMGetProfileByAVID(0, profile) }
}

/// `CMError NCMGetProfileLocation(CMProfileRef, CMProfileLocation *, UInt32 *)`
pub unsafe extern "C" fn NCMGetProfileLocation(
    _profile: *mut c_void,
    _location: *mut c_void,
    _size: *mut u32,
) -> OSStatus {
    CM_PROFILE_ERROR
}

/// `CMError CMCloseProfile(CMProfileRef)`: nothing was opened.
pub unsafe extern "C" fn CMCloseProfile(_profile: *mut c_void) -> OSStatus {
    NO_ERR
}

/// `void CUIDraw(CUIRendererRef, CGRect, CGContextRef, CFDictionaryRef
/// options, CFDictionaryRef *result)`, CoreUI's private widget renderer.
///
/// FIXME (ACCURACY): nothing is drawn, for the reason the `HIThemeDraw...`
/// calls above give; the options name an Aqua artwork asset there is no copy of.
pub unsafe extern "C" fn CUIDraw(
    _renderer: *mut c_void,
    _rect: NSRect,
    _context: CFTypeRef,
    _options: CFTypeRef,
    result: *mut CFTypeRef,
) {
    if !result.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { result.write(std::ptr::null()) };
    }
}

/// `+[NSWindow coreUIRenderer]`, private: the renderer `CUIDraw` takes, which
/// is never looked at here.
unsafe extern "C" fn core_ui_renderer(
    _class: objc::object::Id,
    _sel: objc::sel::Sel,
) -> objc::object::Id {
    std::ptr::null_mut()
}

pub(crate) fn install() {
    let window = objc::class::lookup(c"NSWindow").expect("declared before its methods");
    crate::define::class_methods!(window, {
        "coreUIRenderer" => core_ui_renderer, c"^v16@0:8";
    });
}

exports::exports! {
    functions {
        "GetThemeMetric" => GetThemeMetric,
        "HIThemeDrawButton" => HIThemeDrawButton,
        "HIThemeDrawFocusRect" => HIThemeDrawFocusRect,
        "HIThemeDrawFrame" | "HIThemeDrawGroupBox" | "HIThemeDrawHeader" | "HIThemeDrawMenuBackground" | "HIThemeDrawSeparator" | "HIThemeDrawTabPane" => draw_in_rect,
        "HIThemeDrawGrowBox" => HIThemeDrawGrowBox,
        "HIThemeDrawMenuItem" => HIThemeDrawMenuItem,
        "HIThemeDrawMenuSeparator" => HIThemeDrawMenuSeparator,
        "HIThemeDrawTrack" => HIThemeDrawTrack,
        "HIThemeGetGrowBoxBounds" => HIThemeGetGrowBoxBounds,
        "HIThemeSetFill" => HIThemeSetFill,
        "CMGetProfileByAVID" => CMGetProfileByAVID,
        "CMGetSystemProfile" => CMGetSystemProfile,
        "NCMGetProfileLocation" => NCMGetProfileLocation,
        "CMCloseProfile" => CMCloseProfile,
        "CUIDraw" => CUIDraw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_metric_answers_and_an_unknown_one_writes_zero() {
        let mut out = -1;
        // SAFETY: a writable SInt32.
        assert_eq!(unsafe { GetThemeMetric(0, &mut out) }, NO_ERR);
        assert_eq!(out, 15);
        out = -1;
        // SAFETY: as above.
        assert_eq!(unsafe { GetThemeMetric(9999, &mut out) }, PARAM_ERR);
        assert_eq!(out, 0);
    }
}
