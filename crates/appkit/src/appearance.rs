//! `NSAppearance`, fixed to the runtime's light Aqua appearance.
//!
//! The renderer and semantic colours in this runtime are light, so returning a
//! matching appearance is more useful than a nil property: applications often
//! inspect its name to select their colour assets.

use std::sync::atomic::{AtomicUsize, Ordering};

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

static AQUA: AtomicUsize = AtomicUsize::new(0);

/// The process-wide `NSAppearanceNameAqua` object.
pub fn aqua() -> Id {
    let existing = AQUA.load(Ordering::Acquire);
    if existing != 0 {
        return existing as Id;
    }

    let class = objc::class::lookup(c"NSAppearance").expect("declared before its methods");
    let appearance = objc::object::create_instance(class, 0);
    // This is the appearance AppKit hands out for the lifetime of the process.
    // Keep it alive even if a non-ARC guest incorrectly releases its result.
    objc::arc::retain(appearance);
    match AQUA.compare_exchange(0, appearance as usize, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => appearance,
        Err(existing) => {
            objc::arc::release(appearance);
            existing as Id
        }
    }
}

/// `-name`, the documented name used to compare appearances.
pub(crate) unsafe extern "C" fn name(_receiver: Id, _sel: Sel) -> Id {
    foundation::notification::name_object("NSAppearanceNameAqua") as Id
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSAppearance").expect("declared before its methods");
    methods!(class, {
        "name" => name, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aqua_is_a_stable_named_appearance() {
        crate::bootstrap();
        let appearance = aqua();
        assert_eq!(appearance, aqua());
        assert_eq!(
            unsafe { foundation::string::bytes(name(appearance, objc::sel::get("name"))) },
            b"NSAppearanceNameAqua"
        );
    }
}
