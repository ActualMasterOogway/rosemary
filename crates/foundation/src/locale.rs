//! `NSLocale`, from the environment.
//!
//! The host's answer is the POSIX locale a guest's own `setlocale` would see -
//! `LC_ALL`, then `LC_MESSAGES`, then `LANG` - because that is the one answer
//! both hosts have. macOS's real `NSLocale` reads `AppleLanguages` out of the
//! user's defaults, which is a preferences database this tree does not have; a
//! program asking for a language wants to know which strings to load, and the
//! environment says that on either host.
//!
//! The keys are published as data as well as answered, because
//! `NSLocaleCountryCode` is a `NSString *` **variable**: unresolved it binds to
//! zero and the guest's `objectForKey:` argument is address zero.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// The locale name this process runs under, as `language_REGION`.
fn posix() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .unwrap_or_default()
}

/// A locale name split into an ISO 639 language and an ISO 3166 country.
///
/// The encoding suffix and the `@modifier` are dropped: `en_US.UTF-8` and
/// `en_US` name the same language and the same country, and nothing here asks
/// anything else. A name with no country - or none at all, which is what the
/// `C` locale and an unset environment amount to - is `en_US`, because a
/// program uses this to pick a localisation and there has to be one.
fn split(name: &str) -> (String, String) {
    let base = name.split(['.', '@']).next().unwrap_or_default();
    let mut parts = base.split(['_', '-']);
    let language = parts.next().filter(|p| !p.is_empty()).unwrap_or("en");
    let country = parts.next().filter(|p| !p.is_empty()).unwrap_or("US");
    (language.to_ascii_lowercase(), country.to_ascii_uppercase())
}

fn language() -> String {
    split(&posix()).0
}

fn country() -> String {
    split(&posix()).1
}

/// `language-REGION`, which is how a BCP 47 tag spells what `posix` returns.
fn tag() -> String {
    format!("{}-{}", language(), country())
}

/// `language_REGION`, the identifier `-localeIdentifier` answers with.
fn identifier() -> String {
    format!("{}_{}", language(), country())
}

/// The one locale, which the three class methods all return.
static CURRENT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

unsafe extern "C" fn current_locale(class: Id, _sel: Sel) -> Id {
    *CURRENT.get_or_init(|| {
        // SAFETY: `class` is `NSLocale`, which this module registered.
        let locale = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        // Retained forever: a singleton a guest can release is one it can make
        // dangle by releasing what it never owned.
        objc::arc::retain(locale);
        locale as usize
    }) as Id
}

/// `+preferredLanguages`, which is the one language this process has.
///
/// One element rather than a guessed-at fallback chain: a program walks the
/// list looking for a localisation it has, and an invented second entry would
/// send it to a language the user never asked for.
unsafe extern "C" fn preferred_languages(_class: Id, _sel: Sel) -> Id {
    let tag = crate::string::from_str(&tag());
    let array = corefoundation::collections::new_array(
        &[tag as corefoundation::base::CFTypeRef],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: both are fresh objects this call owns.
    unsafe {
        corefoundation::api::CFRelease(tag as corefoundation::base::CFTypeRef);
        corefoundation::api::CFAutorelease(array) as Id
    }
}

/// The locale components, by the key the constants below name.
///
/// A key this does not know answers nil, which is what Apple's locale does for
/// a component it has no value for.
unsafe extern "C" fn object_for_key(_receiver: Id, _sel: Sel, key: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let key = unsafe { crate::string::bytes(key) };
    let value = match key.as_slice() {
        b"kCFLocaleIdentifier" => identifier(),
        b"kCFLocaleLanguageCode" => language(),
        b"kCFLocaleCountryCode" => country(),
        _ => return std::ptr::null_mut(),
    };
    autoreleased(&value)
}

unsafe extern "C" fn locale_identifier(_receiver: Id, _sel: Sel) -> Id {
    autoreleased(&identifier())
}

fn autoreleased(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
}

/// The `NSString *` globals naming a locale component, and their documented
/// values - which are the CoreFoundation spellings, because Foundation's
/// constants *are* CoreFoundation's.
pub const KEYS: &[(&str, &str)] = &[
    ("_NSLocaleIdentifier", "kCFLocaleIdentifier"),
    ("_NSLocaleLanguageCode", "kCFLocaleLanguageCode"),
    ("_NSLocaleCountryCode", "kCFLocaleCountryCode"),
    ("_kCFLocaleIdentifier", "kCFLocaleIdentifier"),
    ("_kCFLocaleLanguageCode", "kCFLocaleLanguageCode"),
    ("_kCFLocaleCountryCode", "kCFLocaleCountryCode"),
];

pub(crate) fn install() {
    let class =
        corefoundation::base::register(c"NSLocale", None, std::mem::size_of::<usize>(), false);
    class_methods!(class, {
        "currentLocale" => current_locale, c"@16@0:8";
        "autoupdatingCurrentLocale" => current_locale, c"@16@0:8";
        "systemLocale" => current_locale, c"@16@0:8";
        "preferredLanguages" => preferred_languages, c"@16@0:8";
    });
    methods!(class, {
        "objectForKey:" => object_for_key, c"@24@0:8@16";
        "localeIdentifier" => locale_identifier, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posix_name_splits_into_a_language_and_a_country() {
        assert_eq!(split("fr_CA.UTF-8"), ("fr".into(), "CA".into()));
        assert_eq!(split("en_GB@euro"), ("en".into(), "GB".into()));
        assert_eq!(split("de"), ("de".into(), "US".into()));
        assert_eq!(split(""), ("en".into(), "US".into()));
    }
}
