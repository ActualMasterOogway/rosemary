//! Text Input Services: what a program asks about the keyboard layout.
//!
//! Carbon rather than AppKit, but it is the keyboard, so it lives beside
//! [`crate::keymap`]. A program reaches it for one of two reasons: to be told when the
//! user switches input source, or to get the `'uchr'` layout resource and translate
//! key codes itself through `UCKeyTranslate`.
//!
//! The second is not answered, and deliberately: this host has no `'uchr'` resource to
//! hand over, and a *fabricated* one would be a table of wrong characters that no
//! caller could tell from a right one. `TISGetInputSourceProperty` is documented to
//! return null for a property a source does not carry, so null is both honest and a
//! path every caller already has to handle - and [`UCKeyTranslate`] refuses rather
//! than reading whatever was passed instead.
//!
//! What a program gets instead is [`crate::keymap`], which is where this tree turns a
//! key code into a character.
//!
//! **What refusing costs, measured.** Roblox falls back to naming the key, so its
//! keyboard hints read `LeftBracket` where the key is `[`. That is a visible wrong
//! answer and it looks like this refusal is the thing to undo - but handing out a
//! layout is worse, and was tried: with `kTISPropertyUnicodeKeyLayoutData` answered
//! by an opaque `CFData` and `UCKeyTranslate` translating from [`crate::keymap`],
//! Roblox's whole keyboard-hint bar **stops appearing at all**. Empty data, a
//! sixteen-byte `UCKeyboardLayout` header, control characters left in the table or
//! left out - all four the same. So the guest reads the resource itself, and the only
//! answer better than null is a real `'uchr'`. Anything between is a subtree that
//! silently stops rendering, which is the expensive kind of wrong.

#![expect(non_snake_case, reason = "the C spelling is the interface")]

use corefoundation::base::CFTypeRef;

/// The value of `kTISNotifySelectedKeyboardInputSourceChanged`.
///
/// A distributed-notification name, so its *text* is what matters: a guest that
/// observes it and a framework that posts it have to name one string.
pub const NOTIFY_SELECTED_INPUT_SOURCE_CHANGED: &str =
    "AppleSelectedInputSourcesChangedNotification";

/// `kTISPropertyInputSourceLanguages`, and the value of the symbol of that name.
pub const PROPERTY_INPUT_SOURCE_LANGUAGES: &str = "TISPropertyInputSourceLanguages";

/// `kTISPropertyUnicodeKeyLayoutData`.
pub const PROPERTY_UNICODE_KEY_LAYOUT_DATA: &str = "TISPropertyUnicodeKeyLayoutData";

/// The one input source this host reports.
///
/// A `TISInputSourceRef` is opaque: a caller receives one and passes it back to
/// [`TISGetInputSourceProperty`], never reading it. A string standing in for the
/// object is therefore indistinguishable from a real one to any correct caller, and
/// is a real CoreFoundation object, so retaining and releasing it behave.
fn current_source() -> CFTypeRef {
    foundation::notification::name_object("com.apple.keylayout.US") as CFTypeRef
}

/// `TISInputSourceRef TISCopyCurrentKeyboardInputSource(void)`.
///
/// `Copy` in the name, so the caller owns a reference and will release it.
pub unsafe extern "C" fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef {
    let source = current_source();
    objc::arc::retain(source as objc::object::Id);
    source
}

/// `TISInputSourceRef TISCopyCurrentKeyboardLayoutInputSource(void)`.
///
/// The same source: this host has one keyboard layout, so the "current layout" and
/// the "current input source" cannot disagree.
pub unsafe extern "C" fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef {
    // SAFETY: takes no argument and returns a retained object.
    unsafe { TISCopyCurrentKeyboardInputSource() }
}

/// `void *TISGetInputSourceProperty(TISInputSourceRef, CFStringRef key)`.
///
/// Not retained - `Get` rather than `Copy` - so everything returned here has to
/// outlive the call on its own.
pub unsafe extern "C" fn TISGetInputSourceProperty(_source: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    if key.is_null() {
        return std::ptr::null();
    }
    let languages = foundation::notification::name_object(PROPERTY_INPUT_SOURCE_LANGUAGES);
    if key as u64 == languages {
        return language_list();
    }
    // Every other property, `kTISPropertyUnicodeKeyLayoutData` above all: null,
    // because there is no answer this host can give that a caller could trust. See
    // the module note.
    std::ptr::null()
}

/// `["en"]`, as `kTISPropertyInputSourceLanguages` is an array of language codes.
///
/// Built once and kept, because the caller does not own what it is handed.
fn language_list() -> CFTypeRef {
    static LIST: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
    let mut list = LIST.lock().unwrap_or_else(|e| e.into_inner());
    if *list == 0 {
        let english = foundation::notification::name_object("en");
        let values = [english];
        // SAFETY: a live array of one object pointer, described by the count that
        // follows it, with this tree's own type callbacks.
        let array = unsafe {
            corefoundation::api::CFArrayCreate(
                std::ptr::null(),
                values.as_ptr() as *const corefoundation::base::CFTypeRef,
                1,
                &corefoundation::callbacks::TYPE_VALUE_CALLBACKS,
            )
        };
        objc::arc::retain(array as objc::object::Id);
        *list = array as u64;
    }
    *list as CFTypeRef
}

/// `UInt8 LMGetKbdType(void)`.
///
/// `kKeyboardANSI`. The value only selects between the physical layouts that differ
/// in where a handful of keys sit, and ANSI is the one [`crate::keymap`] describes.
pub unsafe extern "C" fn LMGetKbdType() -> u8 {
    40
}

/// `OSStatus UCKeyTranslate(const UCKeyboardLayout *, UInt16 code, UInt16 action,
/// UInt32 modifiers, UInt32 kbdType, OptionBits options, UInt32 *deadKeyState,
/// UniCharCount maxLength, UniCharCount *actualLength, UniChar *string)`.
///
/// Refused with `paramErr`, which is what it returns for a layout it cannot read.
///
/// Reachable only through a layout that [`TISGetInputSourceProperty`] declined to
/// hand out, so a caller arriving here is one that ignored a null - and translating
/// from whatever it passed instead would produce characters out of unrelated memory.
/// The out-parameters are still written, because a caller that ignores the status
/// reads them.
pub unsafe extern "C" fn UCKeyTranslate(
    _layout: *const std::ffi::c_void,
    _code: u16,
    _action: u16,
    _modifiers: u32,
    _kbd_type: u32,
    _options: u32,
    dead_key_state: *mut u32,
    _max_length: usize,
    actual_length: *mut usize,
    _string: *mut u16,
) -> i32 {
    if !actual_length.is_null() {
        // SAFETY: the caller states this is a writable `UniCharCount *`.
        unsafe { *actual_length = 0 };
    }
    if !dead_key_state.is_null() {
        // SAFETY: as above. Cleared so a caller that loops on it terminates.
        unsafe { *dead_key_state = 0 };
    }
    // `paramErr`.
    -50
}

/// The `kTIS*` string globals, as `(symbol, the string it holds)`.
pub const STRING_GLOBALS: &[(&str, &str)] = &[
    (
        "_kTISNotifySelectedKeyboardInputSourceChanged",
        NOTIFY_SELECTED_INPUT_SOURCE_CHANGED,
    ),
    (
        "_kTISPropertyInputSourceLanguages",
        PROPERTY_INPUT_SOURCE_LANGUAGES,
    ),
    (
        "_kTISPropertyUnicodeKeyLayoutData",
        PROPERTY_UNICODE_KEY_LAYOUT_DATA,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The property key a caller passes has to be the same object this module
    /// compares against, or every lookup falls through to null.
    #[test]
    fn the_languages_property_answers_its_own_key() {
        crate::bootstrap();
        let key = foundation::notification::name_object(PROPERTY_INPUT_SOURCE_LANGUAGES);
        // SAFETY: a live source and key.
        let value = unsafe { TISGetInputSourceProperty(current_source(), key as CFTypeRef) };
        assert!(!value.is_null(), "the languages property must be answered");
    }

    /// The layout resource is declined rather than fabricated - see the module note.
    #[test]
    fn the_key_layout_is_declined() {
        crate::bootstrap();
        let key = foundation::notification::name_object(PROPERTY_UNICODE_KEY_LAYOUT_DATA);
        // SAFETY: a live source and key.
        let value = unsafe { TISGetInputSourceProperty(current_source(), key as CFTypeRef) };
        assert!(value.is_null(), "no uchr resource may be invented");
    }

    /// A caller that ignored the null layout must get an error and cleared
    /// out-parameters, not characters from whatever it passed.
    #[test]
    fn translating_without_a_layout_is_refused() {
        let mut length = 99usize;
        let mut dead = 7u32;
        // SAFETY: live out-parameters; the layout is never read.
        let status = unsafe {
            UCKeyTranslate(
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                &mut dead,
                4,
                &mut length,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, -50);
        assert_eq!(length, 0);
        assert_eq!(dead, 0);
    }
}
