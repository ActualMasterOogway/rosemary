use objc::object::Id;
use objc::sel::Sel;

use crate::define::class_methods;

#[repr(C)]
struct Body {
    isa: *const objc::class::Class,
    kind: usize,
}

const URL_PATH: usize = 1;
const WHITESPACE: usize = 2;
const NEWLINE: usize = 3;
const WHITESPACE_AND_NEWLINE: usize = 4;

/// One of Foundation's standard sets, made once for the process as Apple's are.
fn standard(kind: usize) -> Id {
    static SETS: std::sync::Mutex<[usize; 5]> = std::sync::Mutex::new([0; 5]);
    let mut sets = SETS.lock().unwrap_or_else(|e| e.into_inner());
    if sets[kind] == 0 {
        let class = objc::class::lookup(c"NSCharacterSet").expect("installed NSCharacterSet");
        // SAFETY: `Body` begins with `isa` and is the class's registered size.
        let object: *mut Body = unsafe { corefoundation::base::allocate(class) };
        // SAFETY: a newly allocated instance whose payload belongs to us.
        unsafe { (*object).kind = kind };
        objc::arc::retain(object as Id);
        sets[kind] = object as usize;
    }
    sets[kind] as Id
}

pub(crate) unsafe extern "C" fn url_path_allowed_character_set(_class: Id, _sel: Sel) -> Id {
    standard(URL_PATH)
}

unsafe extern "C" fn whitespace_character_set(_class: Id, _sel: Sel) -> Id {
    standard(WHITESPACE)
}

unsafe extern "C" fn newline_character_set(_class: Id, _sel: Sel) -> Id {
    standard(NEWLINE)
}

unsafe extern "C" fn whitespace_and_newline_character_set(_class: Id, _sel: Sel) -> Id {
    standard(WHITESPACE_AND_NEWLINE)
}

/// Whether a byte of a UTF-8 string is a member of `set`.
///
/// FIXME (ACCURACY): the sets hold only their ASCII members. The Unicode
/// spaces and line separators Apple's sets also hold (U+00A0, U+0085, U+2028,
/// ...) are never matched. Matching bytes is safe for ASCII members, since no
/// byte of a multibyte character is below 0x80.
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
        WHITESPACE => byte == b' ' || byte == b'\t',
        NEWLINE => (0x0a..=0x0d).contains(&byte),
        WHITESPACE_AND_NEWLINE => byte == b' ' || byte == b'\t' || (0x0a..=0x0d).contains(&byte),
        _ => false,
    }
}

pub(crate) fn install() {
    let class = corefoundation::base::register(c"NSCharacterSet", None, size_of::<Body>(), false);
    class_methods!(class, {
        "URLPathAllowedCharacterSet" => url_path_allowed_character_set, c"@16@0:8";
        "whitespaceCharacterSet" => whitespace_character_set, c"@16@0:8";
        "newlineCharacterSet" => newline_character_set, c"@16@0:8";
        "whitespaceAndNewlineCharacterSet" => whitespace_and_newline_character_set, c"@16@0:8";
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

    #[test]
    fn newline_and_whitespace_sets_hold_their_ascii_members() {
        crate::bootstrap();
        let (newline, whitespace) = (standard(NEWLINE), standard(WHITESPACE));
        // SAFETY: standard sets.
        unsafe {
            assert!(contains(newline, b'\n') && contains(newline, b'\r'));
            assert!(!contains(newline, b' '));
            assert!(contains(whitespace, b' ') && contains(whitespace, b'\t'));
            assert!(!contains(whitespace, b'\n'));
        }
    }
}
