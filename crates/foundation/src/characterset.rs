//! `NSCharacterSet` values used by URL percent encoding.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::class_methods;

#[repr(C)]
struct Body {
    isa: *const objc::class::Class,
    kind: usize,
}

const URL_PATH: usize = 1;

pub(crate) unsafe extern "C" fn url_path_allowed_character_set(_class: Id, _sel: Sel) -> Id {
    static SET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SET.get_or_init(|| {
        let class = objc::class::lookup(c"NSCharacterSet").expect("installed NSCharacterSet");
        // SAFETY: `Body` begins with `isa` and is the class's registered size.
        let object: *mut Body = unsafe { corefoundation::base::allocate(class) };
        // SAFETY: a newly allocated instance whose payload belongs to us.
        unsafe { (*object).kind = URL_PATH };
        // Process-lifetime singleton, as Foundation's standard sets are.
        objc::arc::retain(object as Id);
        object as usize
    }) as Id
}

pub(crate) unsafe fn contains(set: Id, byte: u8) -> bool {
    let Some(class) = objc::class::lookup(c"NSCharacterSet") else {
        return false;
    };
    // SAFETY: the caller supplies a live character-set object.
    if !unsafe { corefoundation::base::is_kind_of(set.cast(), class) } {
        return false;
    }
    // SAFETY: instances of the class have `Body`'s layout.
    match unsafe { (*(set as *const Body)).kind } {
        URL_PATH => byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte),
        _ => false,
    }
}

pub(crate) fn install() {
    let class = corefoundation::base::register(c"NSCharacterSet", None, size_of::<Body>(), false);
    class_methods!(class, {
        "URLPathAllowedCharacterSet" => url_path_allowed_character_set, c"@16@0:8"
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_url_path_set_is_a_stable_object() {
        crate::bootstrap();
        // SAFETY: a nullary class method returning an object.
        let first = unsafe {
            url_path_allowed_character_set(
                objc::class::ptr(objc::class::lookup(c"NSCharacterSet").unwrap()) as Id,
                objc::sel::get("URLPathAllowedCharacterSet"),
            )
        };
        let second = unsafe {
            url_path_allowed_character_set(
                objc::class::ptr(objc::class::lookup(c"NSCharacterSet").unwrap()) as Id,
                objc::sel::get("URLPathAllowedCharacterSet"),
            )
        };
        assert!(!first.is_null());
        assert_eq!(first, second);
    }
}
