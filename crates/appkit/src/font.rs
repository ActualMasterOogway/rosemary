//! Minimal AppKit font-manager facade.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

static SHARED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// `+[NSFontManager sharedFontManager]`.
unsafe extern "C" fn shared_font_manager(class: Id, _sel: Sel) -> Id {
    *SHARED.get_or_init(|| {
        // SAFETY: the receiver is the NSFontManager class declared by AppKit.
        let manager = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        // AppKit owns the process-wide singleton.
        objc::arc::retain(manager);
        manager as usize
    }) as Id
}

/// `-[NSFontManager localizedNameForFamily:face:]`.
///
/// The fallback family exposed by CoreText has no separate localized names.
unsafe extern "C" fn localized_name(_receiver: Id, _sel: Sel, family: Id, face: Id) -> Id {
    if family.is_null() { face } else { family }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSFontManager").expect("declared before its methods");
    class_methods!(class, {
        "sharedFontManager" => shared_font_manager, c"@16@0:8";
    });
    methods!(class, {
        "localizedNameForFamily:face:" => localized_name, c"@32@0:8@16@24";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_font_manager_is_stable() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSFontManager").unwrap();
        let class = objc::class::ptr(class).cast();
        // SAFETY: direct calls with the Objective-C signature installed above.
        let first = unsafe { shared_font_manager(class, objc::sel::get("sharedFontManager")) };
        let second = unsafe { shared_font_manager(class, objc::sel::get("sharedFontManager")) };
        assert!(!first.is_null());
        assert_eq!(first, second);
    }

    #[test]
    fn localized_font_name_uses_the_family() {
        crate::bootstrap();
        let family = foundation::string::from_str("Sans") as Id;
        let face = foundation::string::from_str("Regular") as Id;
        // SAFETY: direct call with the Objective-C signature installed above.
        let result = unsafe {
            localized_name(
                std::ptr::null_mut(),
                objc::sel::get("localizedNameForFamily:face:"),
                family,
                face,
            )
        };
        assert_eq!(result, family);
        unsafe {
            corefoundation::api::CFRelease(family.cast());
            corefoundation::api::CFRelease(face.cast());
        }
    }
}
