//! The small CoreText surface AppKit's clients need before font rasterization.
//!
//! A font descriptor is an immutable attributes dictionary.  Keeping the font
//! itself in that representation is sufficient for callers which only inspect
//! its descriptor; drawing and metrics can grow a real host-font backing when
//! they are needed.

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;
use corefoundation::callbacks::Policy;
use corefoundation::collections::{self, Number};
use foundation::geometry::{NSPoint, NSRect, NSSize};

const ATTRIBUTE_GLOBALS: &[(&str, &str)] = &[
    ("_kCTFontCopyrightNameKey", "CTFontCopyrightNameKey"),
    ("_kCTFontAttributeName", "CTFontAttributeName"),
    ("_kCTFontFamilyNameAttribute", "CTFontFamilyNameAttribute"),
    ("_kCTFontLanguagesAttribute", "CTFontLanguagesAttribute"),
    ("_kCTFontSizeAttribute", "CTFontSizeAttribute"),
    ("_kCTFontSlantTrait", "CTFontSlantTrait"),
    ("_kCTFontStyleNameAttribute", "CTFontStyleNameAttribute"),
    ("_kCTFontSymbolicTrait", "CTFontSymbolicTrait"),
    ("_kCTFontTraitsAttribute", "CTFontTraitsAttribute"),
    ("_kCTFontURLAttribute", "CTFontURLAttribute"),
    ("_kCTFontUniqueNameKey", "CTFontUniqueNameKey"),
    ("_kCTFontWeightTrait", "CTFontWeightTrait"),
    (
        "_kCTForegroundColorAttributeName",
        "CTForegroundColorAttributeName",
    ),
];

fn key(text: &'static str) -> CFTypeRef {
    foundation::notification::name_object(text) as CFTypeRef
}
fn type_policy() -> Policy {
    Policy::from_type_callbacks()
}

fn make_font_with_name(family: CFTypeRef, size: f64) -> CFTypeRef {
    let style = foundation::string::from_str("Regular") as CFTypeRef;
    let size = collections::new_number(Number::Float(if size > 0.0 { size } else { 13.0 }));
    let weight = collections::new_number(Number::Float(0.0));
    let slant = collections::new_number(Number::Float(0.0));
    let symbolic = collections::new_number(Number::Integer(0));
    let traits = collections::new_dictionary(
        &[
            (key("CTFontWeightTrait"), weight),
            (key("CTFontSlantTrait"), slant),
            (key("CTFontSymbolicTrait"), symbolic),
        ],
        Policy::from_key_callbacks(),
        type_policy(),
        false,
    );
    let font = collections::new_dictionary(
        &[
            (key("CTFontFamilyNameAttribute"), family),
            (key("CTFontUniqueNameKey"), family),
            (key("CTFontStyleNameAttribute"), style),
            (key("CTFontSizeAttribute"), size),
            (key("CTFontTraitsAttribute"), traits),
        ],
        Policy::from_key_callbacks(),
        type_policy(),
        false,
    );
    // The dictionaries retained their contents.
    unsafe {
        corefoundation::api::CFRelease(style);
        corefoundation::api::CFRelease(size);
        corefoundation::api::CFRelease(weight);
        corefoundation::api::CFRelease(slant);
        corefoundation::api::CFRelease(symbolic);
        corefoundation::api::CFRelease(traits);
    }
    font
}

fn make_font(size: f64) -> CFTypeRef {
    let family = foundation::string::from_str("Sans") as CFTypeRef;
    let font = make_font_with_name(family, size);
    // The font dictionary retained the family name.
    unsafe { corefoundation::api::CFRelease(family) };
    font
}

pub unsafe extern "C" fn CTFontCreateUIFontForLanguage(
    _kind: u32,
    size: f64,
    _language: CFTypeRef,
) -> CFTypeRef {
    make_font(size)
}

pub unsafe extern "C" fn CTFontCreateWithName(
    name: CFTypeRef,
    size: f64,
    _matrix: *const c_void,
) -> CFTypeRef {
    if name.is_null() {
        return std::ptr::null();
    }
    make_font_with_name(name, size)
}

pub unsafe extern "C" fn CTFontCopyFontDescriptor(font: CFTypeRef) -> CFTypeRef {
    if font.is_null() {
        return std::ptr::null();
    }
    unsafe { corefoundation::api::CFRetain(font) }
}

/// The fallback font object also serves as its graphics-font backing.
pub unsafe extern "C" fn CTFontCopyGraphicsFont(
    font: CFTypeRef,
    attributes: *mut CFTypeRef,
) -> CFTypeRef {
    if font.is_null() {
        return std::ptr::null();
    }
    if !attributes.is_null() {
        // SAFETY: the caller supplied the documented optional output pointer.
        unsafe { *attributes = corefoundation::api::CFRetain(font) };
    }
    unsafe { corefoundation::api::CFRetain(font) }
}

unsafe fn copy_font_string(font: CFTypeRef, attribute: CFTypeRef) -> CFTypeRef {
    if !font.is_null() {
        let value = unsafe { corefoundation::api::CFDictionaryGetValue(font, attribute) };
        if !value.is_null() {
            return unsafe { corefoundation::api::CFRetain(value) };
        }
        let family = unsafe {
            corefoundation::api::CFDictionaryGetValue(font, key("CTFontFamilyNameAttribute"))
        };
        if !family.is_null() {
            return unsafe { corefoundation::api::CFRetain(family) };
        }
    }
    foundation::string::from_str("Sans") as CFTypeRef
}

pub unsafe extern "C" fn CTFontCopyName(font: CFTypeRef, name_key: CFTypeRef) -> CFTypeRef {
    unsafe { copy_font_string(font, name_key) }
}

pub unsafe extern "C" fn CTFontCopyFamilyName(font: CFTypeRef) -> CFTypeRef {
    unsafe { copy_font_string(font, key("CTFontFamilyNameAttribute")) }
}

pub unsafe extern "C" fn CTFontCopyPostScriptName(font: CFTypeRef) -> CFTypeRef {
    unsafe { copy_font_string(font, key("CTFontUniqueNameKey")) }
}

/// The fallback font is regular, with none of CoreText's symbolic trait bits.
pub unsafe extern "C" fn CTFontGetSymbolicTraits(_font: CFTypeRef) -> u32 {
    0
}

pub unsafe extern "C" fn CTFontGetUnitsPerEm(_font: CFTypeRef) -> u32 {
    1000
}

unsafe fn font_size(font: CFTypeRef) -> f64 {
    if font.is_null() {
        return 0.0;
    }
    let value =
        unsafe { corefoundation::api::CFDictionaryGetValue(font, key("CTFontSizeAttribute")) };
    unsafe { collections::number(value) }.map_or(0.0, |number| number.as_f64())
}

pub unsafe extern "C" fn CTFontGetSize(font: CFTypeRef) -> f64 {
    unsafe { font_size(font) }
}

pub unsafe extern "C" fn CTFontGetAscent(font: CFTypeRef) -> f64 {
    (unsafe { font_size(font) }) * 0.8
}

pub unsafe extern "C" fn CTFontGetDescent(font: CFTypeRef) -> f64 {
    (unsafe { font_size(font) }) * 0.2
}

pub unsafe extern "C" fn CTFontGetLeading(_font: CFTypeRef) -> f64 {
    0.0
}

pub unsafe extern "C" fn CTFontGetCapHeight(font: CFTypeRef) -> f64 {
    (unsafe { font_size(font) }) * 0.7
}

pub unsafe extern "C" fn CTFontGetXHeight(font: CFTypeRef) -> f64 {
    (unsafe { font_size(font) }) * 0.5
}

pub unsafe extern "C" fn CTFontGetSlantAngle(_font: CFTypeRef) -> f64 {
    0.0
}

pub unsafe extern "C" fn CTFontGetUnderlinePosition(font: CFTypeRef) -> f64 {
    (unsafe { font_size(font) }) * -0.1
}

pub unsafe extern "C" fn CTFontGetUnderlineThickness(font: CFTypeRef) -> f64 {
    (unsafe { font_size(font) }) * 0.05
}

unsafe fn glyph_bounds(font: CFTypeRef) -> NSRect {
    let size = unsafe { font_size(font) };
    NSRect {
        origin: NSPoint {
            x: 0.0,
            y: size * -0.2,
        },
        size: NSSize {
            width: size * 0.6,
            height: size,
        },
    }
}

pub unsafe extern "C" fn CTFontGetBoundingBox(font: CFTypeRef) -> NSRect {
    unsafe { glyph_bounds(font) }
}

pub unsafe extern "C" fn CTFontGetBoundingRectsForGlyphs(
    font: CFTypeRef,
    _orientation: u32,
    _glyphs: *const u16,
    bounds: *mut NSRect,
    count: isize,
) -> NSRect {
    let rect = unsafe { glyph_bounds(font) };
    if !bounds.is_null() {
        for at in 0..count.max(0) as usize {
            // SAFETY: the caller supplies `count` writable rectangles.
            unsafe { bounds.add(at).write(rect) };
        }
    }
    rect
}

pub unsafe extern "C" fn CTFontGetAdvancesForGlyphs(
    font: CFTypeRef,
    _orientation: u32,
    _glyphs: *const u16,
    advances: *mut NSSize,
    count: isize,
) -> f64 {
    let count = count.max(0) as usize;
    let advance = NSSize {
        width: unsafe { font_size(font) } * 0.6,
        height: 0.0,
    };
    if !advances.is_null() {
        for at in 0..count {
            // SAFETY: the caller supplies `count` writable advances.
            unsafe { advances.add(at).write(advance) };
        }
    }
    advance.width * count as f64
}

/// Map UTF-16 code units directly into the fallback font's synthetic glyph set.
pub unsafe extern "C" fn CTFontGetGlyphsForCharacters(
    _font: CFTypeRef,
    characters: *const u16,
    glyphs: *mut u16,
    count: isize,
) -> u8 {
    let Ok(count) = usize::try_from(count) else {
        return 0;
    };
    if count == 0 {
        return 1;
    }
    if characters.is_null() || glyphs.is_null() {
        return 0;
    }
    // SAFETY: the caller supplies `count` readable characters and writable glyphs.
    unsafe { std::ptr::copy_nonoverlapping(characters, glyphs, count) };
    1
}

pub unsafe extern "C" fn CTFontDrawGlyphs(
    _font: CFTypeRef,
    _glyphs: *const u16,
    _positions: *const c_void,
    _count: usize,
    _context: CFTypeRef,
) {
}

pub unsafe extern "C" fn CTFontDescriptorCopyAttribute(
    descriptor: CFTypeRef,
    attribute: CFTypeRef,
) -> CFTypeRef {
    if descriptor.is_null() || attribute.is_null() {
        return std::ptr::null();
    }
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(descriptor, attribute) };
    if value.is_null() {
        value
    } else {
        unsafe { corefoundation::api::CFRetain(value) }
    }
}

pub unsafe extern "C" fn CTFontCopyAttribute(font: CFTypeRef, attribute: CFTypeRef) -> CFTypeRef {
    unsafe { CTFontDescriptorCopyAttribute(font, attribute) }
}

pub unsafe extern "C" fn CTFontCopyTraits(font: CFTypeRef) -> CFTypeRef {
    unsafe { CTFontDescriptorCopyAttribute(font, key("CTFontTraitsAttribute")) }
}

/// A descriptor is the immutable attributes dictionary in this CoreText subset.
pub unsafe extern "C" fn CTFontDescriptorCreateWithAttributes(attributes: CFTypeRef) -> CFTypeRef {
    if attributes.is_null() {
        return std::ptr::null();
    }
    unsafe { corefoundation::api::CFRetain(attributes) }
}

/// Return the input descriptor as the single match in the fallback database.
pub unsafe extern "C" fn CTFontDescriptorCreateMatchingFontDescriptor(
    descriptor: CFTypeRef,
    _mandatory_attributes: CFTypeRef,
) -> CFTypeRef {
    if descriptor.is_null() {
        return std::ptr::null();
    }
    unsafe { corefoundation::api::CFRetain(descriptor) }
}

pub unsafe extern "C" fn CTFontCreateWithFontDescriptor(
    descriptor: CFTypeRef,
    _size: f64,
    _matrix: *const c_void,
) -> CFTypeRef {
    if descriptor.is_null() {
        return std::ptr::null();
    }
    unsafe { corefoundation::api::CFRetain(descriptor) }
}

pub unsafe extern "C" fn CTFontCopyTable(
    _font: CFTypeRef,
    _table: u32,
    _options: u32,
) -> CFTypeRef {
    std::ptr::null()
}

/// There are no secondary families behind this runtime's fallback font.
pub unsafe extern "C" fn CTFontCopyDefaultCascadeListForLanguages(
    _font: CFTypeRef,
    _languages: CFTypeRef,
) -> CFTypeRef {
    collections::new_array(&[], type_policy(), false)
}

/// The family used by [`CTFontCreateUIFontForLanguage`], as a Copy-owned array.
pub unsafe extern "C" fn CTFontManagerCopyAvailableFontFamilyNames() -> CFTypeRef {
    let family = foundation::string::from_str("Sans") as CFTypeRef;
    let families = collections::new_array(&[family], type_policy(), false);
    // The array retained the string and the caller owns the array by the Copy
    // rule in the function's name.
    unsafe { corefoundation::api::CFRelease(family) };
    families
}

/// Describe an application font by its file name. The synthetic CoreText
/// backing does not parse OpenType tables, but returning a stable descriptor
/// lets Qt register and select the bundled family instead of rejecting it.
pub unsafe extern "C" fn CTFontManagerCreateFontDescriptorsFromURL(url: CFTypeRef) -> CFTypeRef {
    let Some(path) = (unsafe { corefoundation::url::as_url(url) }).and_then(|url| url.file_path())
    else {
        return std::ptr::null();
    };
    if !std::path::Path::new(&path).is_file() {
        return std::ptr::null();
    }
    let Some(name) = std::path::Path::new(&path)
        .file_stem()
        .and_then(|name| name.to_str())
    else {
        return std::ptr::null();
    };
    let name = foundation::string::from_str(name) as CFTypeRef;
    let descriptor = make_font_with_name(name, 0.0);
    let descriptors = collections::new_array(&[descriptor], type_policy(), false);
    unsafe {
        corefoundation::api::CFRelease(name);
        corefoundation::api::CFRelease(descriptor);
    }
    descriptors
}

pub unsafe extern "C" fn CTFontManagerCreateFontDescriptorFromData(data: CFTypeRef) -> CFTypeRef {
    if data.is_null() {
        return std::ptr::null();
    }
    make_font(0.0)
}

pub const NAMES: &[&str] = &[
    "CTFontCreateUIFontForLanguage",
    "CTFontCreateWithName",
    "CTFontCopyFontDescriptor",
    "CTFontCopyGraphicsFont",
    "CTFontCopyName",
    "CTFontCopyFamilyName",
    "CTFontCopyPostScriptName",
    "CTFontGetSymbolicTraits",
    "CTFontGetUnitsPerEm",
    "CTFontGetSize",
    "CTFontGetAscent",
    "CTFontGetDescent",
    "CTFontGetLeading",
    "CTFontGetCapHeight",
    "CTFontGetXHeight",
    "CTFontGetSlantAngle",
    "CTFontGetUnderlinePosition",
    "CTFontGetUnderlineThickness",
    "CTFontGetBoundingBox",
    "CTFontGetBoundingRectsForGlyphs",
    "CTFontGetAdvancesForGlyphs",
    "CTFontGetGlyphsForCharacters",
    "CTFontDrawGlyphs",
    "CTFontDescriptorCopyAttribute",
    "CTFontCopyAttribute",
    "CTFontCopyTraits",
    "CTFontDescriptorCreateWithAttributes",
    "CTFontDescriptorCreateMatchingFontDescriptor",
    "CTFontCreateWithFontDescriptor",
    "CTFontCopyTable",
    "CTFontCopyDefaultCascadeListForLanguages",
    "CTFontManagerCopyAvailableFontFamilyNames",
    "CTFontManagerCreateFontDescriptorsFromURL",
    "CTFontManagerCreateFontDescriptorFromData",
];

pub fn symbol(name: &str) -> Option<u64> {
    macro_rules! at {
        ($function:expr) => {
            $function as *const () as usize as u64
        };
    }
    Some(match name {
        "CTFontCreateUIFontForLanguage" => at!(CTFontCreateUIFontForLanguage),
        "CTFontCreateWithName" => at!(CTFontCreateWithName),
        "CTFontCopyFontDescriptor" => at!(CTFontCopyFontDescriptor),
        "CTFontCopyGraphicsFont" => at!(CTFontCopyGraphicsFont),
        "CTFontCopyName" => at!(CTFontCopyName),
        "CTFontCopyFamilyName" => at!(CTFontCopyFamilyName),
        "CTFontCopyPostScriptName" => at!(CTFontCopyPostScriptName),
        "CTFontGetSymbolicTraits" => at!(CTFontGetSymbolicTraits),
        "CTFontGetUnitsPerEm" => at!(CTFontGetUnitsPerEm),
        "CTFontGetSize" => at!(CTFontGetSize),
        "CTFontGetAscent" => at!(CTFontGetAscent),
        "CTFontGetDescent" => at!(CTFontGetDescent),
        "CTFontGetLeading" => at!(CTFontGetLeading),
        "CTFontGetCapHeight" => at!(CTFontGetCapHeight),
        "CTFontGetXHeight" => at!(CTFontGetXHeight),
        "CTFontGetSlantAngle" => at!(CTFontGetSlantAngle),
        "CTFontGetUnderlinePosition" => at!(CTFontGetUnderlinePosition),
        "CTFontGetUnderlineThickness" => at!(CTFontGetUnderlineThickness),
        "CTFontGetBoundingBox" => at!(CTFontGetBoundingBox),
        "CTFontGetBoundingRectsForGlyphs" => at!(CTFontGetBoundingRectsForGlyphs),
        "CTFontGetAdvancesForGlyphs" => at!(CTFontGetAdvancesForGlyphs),
        "CTFontGetGlyphsForCharacters" => at!(CTFontGetGlyphsForCharacters),
        "CTFontDrawGlyphs" => at!(CTFontDrawGlyphs),
        "CTFontDescriptorCopyAttribute" => at!(CTFontDescriptorCopyAttribute),
        "CTFontCopyAttribute" => at!(CTFontCopyAttribute),
        "CTFontCopyTraits" => at!(CTFontCopyTraits),
        "CTFontDescriptorCreateWithAttributes" => at!(CTFontDescriptorCreateWithAttributes),
        "CTFontDescriptorCreateMatchingFontDescriptor" => {
            at!(CTFontDescriptorCreateMatchingFontDescriptor)
        }
        "CTFontCreateWithFontDescriptor" => at!(CTFontCreateWithFontDescriptor),
        "CTFontCopyTable" => at!(CTFontCopyTable),
        "CTFontCopyDefaultCascadeListForLanguages" => {
            at!(CTFontCopyDefaultCascadeListForLanguages)
        }
        "CTFontManagerCopyAvailableFontFamilyNames" => {
            at!(CTFontManagerCopyAvailableFontFamilyNames)
        }
        "CTFontManagerCreateFontDescriptorsFromURL" => {
            at!(CTFontManagerCreateFontDescriptorsFromURL)
        }
        "CTFontManagerCreateFontDescriptorFromData" => {
            at!(CTFontManagerCreateFontDescriptorFromData)
        }
        _ => return None,
    })
}

pub fn data_symbols() -> Vec<(String, u64)> {
    ATTRIBUTE_GLOBALS
        .iter()
        .map(|(symbol, text)| {
            (
                (*symbol).to_string(),
                corefoundation::base::global_cell(symbol, key(text) as usize as u64),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_created_from_attributes_exposes_them() {
        crate::bootstrap();
        let family = foundation::string::from_str("Sans") as CFTypeRef;
        let attributes = collections::new_dictionary(
            &[(key("CTFontFamilyNameAttribute"), family)],
            Policy::from_key_callbacks(),
            type_policy(),
            false,
        );
        // SAFETY: all arguments are live Core Foundation objects.
        unsafe {
            let descriptor = CTFontDescriptorCreateWithAttributes(attributes);
            let matching =
                CTFontDescriptorCreateMatchingFontDescriptor(descriptor, std::ptr::null());
            let mut graphics_attributes = std::ptr::null();
            let graphics = CTFontCopyGraphicsFont(matching, &mut graphics_attributes);
            assert_eq!(graphics, matching);
            assert_eq!(graphics_attributes, matching);
            let result = CTFontDescriptorCopyAttribute(matching, key("CTFontFamilyNameAttribute"));
            assert_eq!(foundation::string::bytes(result as _), b"Sans");
            let font_result = CTFontCopyAttribute(matching, key("CTFontFamilyNameAttribute"));
            assert_eq!(foundation::string::bytes(font_result as _), b"Sans");
            corefoundation::api::CFRelease(font_result);
            let unique = CTFontCopyName(matching, key("CTFontUniqueNameKey"));
            assert_eq!(foundation::string::bytes(unique as _), b"Sans");
            corefoundation::api::CFRelease(unique);
            corefoundation::api::CFRelease(result);
            corefoundation::api::CFRelease(graphics);
            corefoundation::api::CFRelease(graphics_attributes);
            corefoundation::api::CFRelease(matching);
            corefoundation::api::CFRelease(descriptor);
            corefoundation::api::CFRelease(attributes);
            corefoundation::api::CFRelease(family);
        }
    }

    #[test]
    fn fallback_font_has_no_cascade_families() {
        crate::bootstrap();
        // SAFETY: the nullable arguments are accepted and the result is a CFArray.
        unsafe {
            let cascade =
                CTFontCopyDefaultCascadeListForLanguages(std::ptr::null(), std::ptr::null());
            assert_eq!(corefoundation::api::CFArrayGetCount(cascade), 0);
            corefoundation::api::CFRelease(cascade);
        }
    }

    #[test]
    fn fallback_font_has_regular_symbolic_traits() {
        crate::bootstrap();
        let font = make_font(13.0);
        // SAFETY: `font` is a live fallback font object.
        unsafe {
            assert_eq!(CTFontGetSymbolicTraits(font), 0);
            assert_eq!(CTFontGetUnitsPerEm(font), 1000);
            assert_eq!(CTFontGetSize(font), 13.0);
            assert_eq!(CTFontGetAscent(font) + CTFontGetDescent(font), 13.0);
            let mut bounds = NSRect::default();
            let aggregate =
                CTFontGetBoundingRectsForGlyphs(font, 0, std::ptr::null(), &mut bounds, 1);
            assert_eq!(bounds, aggregate);
            let mut advance = NSSize::default();
            let total = CTFontGetAdvancesForGlyphs(font, 0, std::ptr::null(), &mut advance, 1);
            assert_eq!(total, advance.width);
            let traits = CTFontCopyTraits(font);
            assert!(!traits.is_null());
            assert_eq!(
                corefoundation::api::CFDictionaryContainsKey(traits, key("missing")),
                0
            );
            corefoundation::api::CFRelease(traits);
            corefoundation::api::CFRelease(font);
        }
    }

    #[test]
    fn font_created_with_name_preserves_its_names() {
        crate::bootstrap();
        let name = foundation::string::from_str("Studio Sans") as CFTypeRef;
        // SAFETY: `name` is a live string and a null transform is documented.
        unsafe {
            let font = CTFontCreateWithName(name, 17.0, std::ptr::null());
            let family = CTFontCopyFamilyName(font);
            let postscript = CTFontCopyPostScriptName(font);
            assert_eq!(foundation::string::bytes(family as _), b"Studio Sans");
            assert_eq!(foundation::string::bytes(postscript as _), b"Studio Sans");
            corefoundation::api::CFRelease(family);
            corefoundation::api::CFRelease(postscript);
            corefoundation::api::CFRelease(font);
            corefoundation::api::CFRelease(name);
        }
    }

    #[test]
    fn fallback_font_maps_characters_to_stable_glyphs() {
        let characters = [b'A' as u16, 0x03bb];
        let mut glyphs = [0; 2];
        // SAFETY: both arrays contain the requested two entries.
        unsafe {
            assert_eq!(
                CTFontGetGlyphsForCharacters(
                    std::ptr::null(),
                    characters.as_ptr(),
                    glyphs.as_mut_ptr(),
                    2,
                ),
                1
            );
        }
        assert_eq!(glyphs, characters);
    }
}
