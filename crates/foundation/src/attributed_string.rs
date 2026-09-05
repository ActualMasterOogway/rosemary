//! The immutable, attribute-free part of `NSAttributedString`.
//!
//! Text input clients return attributed substrings even when they do not use
//! attributes. Keeping the string is enough for callers that measure the
//! returned segment, and is materially different from returning `nil`.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

fn class() -> &'static objc::class::Class {
    objc::class::lookup(c"NSAttributedString").expect("NSAttributedString is registered")
}

unsafe extern "C" fn init_with_string(receiver: Id, _sel: Sel, value: Id) -> Id {
    // SAFETY: `value` is null or an NSString supplied by the caller.
    let text = String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned();
    // SAFETY: this class is registered with `String` boxed storage.
    unsafe { crate::boxed::fill(receiver, text) };
    receiver
}

unsafe extern "C" fn attributed_string_with_string(_receiver: Id, _sel: Sel, value: Id) -> Id {
    // SAFETY: `value` is null or an NSString supplied by the caller.
    let text = String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned();
    let object = crate::boxed::make(class(), text);
    // SAFETY: a fresh object owned by this method.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

unsafe extern "C" fn string(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an instance of the class registered below.
    let text = unsafe { crate::boxed::contents::<String>(receiver, class()) }
        .map_or("", |text| text.as_str());
    objc::arc::autorelease(crate::string::from_str(text))
}

pub(crate) fn make(text: &str) -> Id {
    let object = crate::boxed::make(class(), text.to_owned());
    // SAFETY: a fresh object owned by this function.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

pub(crate) fn install() {
    let class = crate::boxed::register::<String>(c"NSAttributedString", None);
    methods!(class, {
        "initWithString:" => init_with_string, c"@24@0:8@16";
        "string" => string, c"@16@0:8";
    });
    class_methods!(class, {
        "attributedStringWithString:" => attributed_string_with_string, c"@24@0:8@16";
    });
}
