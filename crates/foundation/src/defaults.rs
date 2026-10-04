use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use corefoundation::base::CFTypeRef;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

static VALUES: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
static LOADED: std::sync::Once = std::sync::Once::new();

fn values() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
    LOADED.call_once(|| {
        *VALUES.lock().unwrap_or_else(|e| e.into_inner()) = load();
    });
    VALUES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Consume key/value pairs after argv[0]. Later occurrences override earlier ones.
fn parse_arguments(arguments: &[std::ffi::OsString]) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let mut arguments = arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        let argument = argument.to_string_lossy();
        let Some(key) = argument.strip_prefix('-').filter(|key| !key.is_empty()) else {
            continue;
        };
        let Some(value) = arguments.next() else {
            break;
        };
        values.insert(key.to_owned(), value.to_string_lossy().into_owned());
    }
    values
}

fn argument_values() -> BTreeMap<String, String> {
    parse_arguments(&crate::processinfo::guest_arguments())
}

/// # Safety
///
/// `receiver` is one of our defaults objects; `key` is null or an NSString.
/// The suites `-addSuiteNamed:` put in each defaults object's search list,
/// searched after its own domain, in the order they were added.
static ADDED_SUITES: Mutex<BTreeMap<usize, Vec<String>>> = Mutex::new(BTreeMap::new());

unsafe fn value_for_key(receiver: Id, key: Id) -> Option<String> {
    let key = unsafe { key_text(key) }?;
    if key == corefoundation::preferences::INTERFACE_STYLE {
        return corefoundation::preferences::interface_style().map(str::to_owned);
    }
    argument_values().remove(&key).or_else(|| {
        let values = values();
        values
            .get(&format!("{}{key}", unsafe { prefix(receiver) }))
            .or_else(|| {
                let suites = ADDED_SUITES.lock().unwrap_or_else(|e| e.into_inner());
                suites
                    .get(&(receiver as usize))?
                    .iter()
                    .find_map(|suite| values.get(&format!("{suite}\u{1}{key}")))
            })
            .cloned()
    })
}

unsafe extern "C" fn add_suite_named(receiver: Id, _sel: Sel, name: Id) {
    // SAFETY: null or an `NSString`.
    let Some(name) = (unsafe { key_text(name) }) else {
        return;
    };
    let mut suites = ADDED_SUITES.lock().unwrap_or_else(|e| e.into_inner());
    let list = suites.entry(receiver as usize).or_default();
    if !list.contains(&name) {
        list.push(name);
    }
}

unsafe extern "C" fn remove_suite_named(receiver: Id, _sel: Sel, name: Id) {
    // SAFETY: null or an `NSString`.
    let Some(name) = (unsafe { key_text(name) }) else {
        return;
    };
    let mut suites = ADDED_SUITES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = suites.get_mut(&(receiver as usize)) {
        list.retain(|suite| *suite != name);
    }
}

/// Under the directory macOS keeps preferences in, so a second launch finds
/// what the first one wrote.
fn store_path() -> PathBuf {
    crate::host_home_directory().join("Library/Preferences/rosemary-defaults.tsv")
}

/// Tab-separated `key\tvalue`. A key or a value containing a tab or a newline
/// is not written and not read: escaping would make the file unreadable by
/// anything else, and no caller in this tree has stored one.
fn load() -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn save(values: &BTreeMap<String, String>) {
    let mut text = String::new();
    for (key, value) in values {
        if key.contains(['\t', '\n']) || value.contains(['\t', '\n']) {
            continue;
        }
        text.push_str(&format!("{key}\t{value}\n"));
    }
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

/// An `NSUserDefaults`. The suite name is a leaked `String` rather than an
/// inline one, because the object is created by the runtime's allocator and its
/// size has to be a constant.
#[repr(C)]
struct Defaults {
    isa: *const std::ffi::c_void,
    suite: *mut String,
}

/// The prefix every key of `receiver` carries.
///
/// A suite is a **separate** key space on macOS - `-initWithSuiteName:` is how
/// an app reads the defaults of an app group rather than its own - so a flat
/// store has to keep them apart or one suite's `"enabled"` is another's. The
/// separator is a byte no key contains, so no prefixed key can collide with an
/// unprefixed one.
///
/// # Safety
///
/// `receiver` must be null or an `NSUserDefaults` this module made.
unsafe fn prefix(receiver: Id) -> String {
    if receiver.is_null() {
        return String::new();
    }
    let suite = unsafe { (*(receiver as *const Defaults)).suite };
    if suite.is_null() {
        return String::new();
    }
    // SAFETY: a leaked `String` this module made.
    format!("{}\u{1}", unsafe { &*suite })
}

/// The stored key `key` names when `receiver` asks for it.
///
/// # Safety
///
/// `receiver` is an `NSUserDefaults` this module made; `key` is null or an
/// `NSString`.
unsafe fn scoped(receiver: Id, key: Id) -> Option<String> {
    let key = unsafe { key_text(key) }?;
    Some(format!("{}{key}", unsafe { prefix(receiver) }))
}

/// `-initWithSuiteName:`, which opens another key space through the same store.
///
/// A nil name is `+standardUserDefaults`'s own space, which is Apple's
/// behaviour and what a caller passing a nil group identifier means.
unsafe extern "C" fn init_with_suite_name(receiver: Id, _sel: Sel, name: Id) -> Id {
    if receiver.is_null() {
        return receiver;
    }
    // SAFETY: null or an `NSString`.
    let suite = unsafe { key_text(name) };
    // SAFETY: a fresh instance of this module's own class.
    unsafe {
        (*(receiver as *mut Defaults)).suite = match suite {
            Some(name) => Box::into_raw(Box::new(name)),
            None => std::ptr::null_mut(),
        };
    }
    receiver
}

/// The one defaults object, retained forever - the reason `NSLocale`'s
/// singleton is.
static SHARED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

unsafe extern "C" fn standard_defaults(class: Id, _sel: Sel) -> Id {
    *SHARED.get_or_init(|| {
        // SAFETY: `class` is `NSUserDefaults`, which this module registered.
        let defaults = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        objc::arc::retain(defaults);
        defaults as usize
    }) as Id
}

/// # Safety
///
/// `key` must be null or an `NSString`.
unsafe fn key_text(key: Id) -> Option<String> {
    if key.is_null() {
        return None;
    }
    let bytes = unsafe { crate::string::bytes(key) };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (!text.is_empty()).then_some(text)
}

/// `-objectForKey:`, which answers a string.
///
/// A string for everything, including what was stored as a number: a caller
/// that wants a number asks `-integerForKey:` or `-boolForKey:`, and one that
/// asks for the object and then sends it `-intValue` gets the right answer from
/// an `NSString` too.
unsafe extern "C" fn object_for_key(receiver: Id, _sel: Sel, key: Id) -> Id {
    // SAFETY: an `NSUserDefaults` this module made, and null or an `NSString`.
    let Some(value) = (unsafe { value_for_key(receiver, key) }) else {
        return std::ptr::null_mut();
    };
    let string = crate::string::from_str(&value);
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(string as CFTypeRef) as Id }
}

unsafe extern "C" fn set_object_for_key(receiver: Id, _sel: Sel, value: Id, key: Id) {
    // SAFETY: as [`object_for_key`].
    let Some(key) = (unsafe { scoped(receiver, key) }) else {
        return;
    };
    let mut values = values();
    // Apple's `-setObject:nil forKey:` is a removal, not a store of nil.
    match value.is_null() {
        true => {
            values.remove(&key);
        }
        false => {
            // SAFETY: whatever the caller stored, read as a string - which is
            // what `-description` would give for a number or a boolean too.
            let text = unsafe { crate::string::bytes(value) };
            values.insert(key, String::from_utf8_lossy(&text).into_owned());
        }
    }
    save(&values);
}

unsafe extern "C" fn remove_object_for_key(receiver: Id, _sel: Sel, key: Id) {
    // SAFETY: as [`object_for_key`].
    let Some(key) = (unsafe { scoped(receiver, key) }) else {
        return;
    };
    let mut values = values();
    values.remove(&key);
    save(&values);
}

/// The stored value as a number, or zero.
///
/// `"YES"` and `"true"` count as one, because `-setBool:forKey:` stores the
/// boolean's description and a later `-integerForKey:` on the same key has to
/// agree with the `-boolForKey:` that would read it.
fn number_for(receiver: Id, key: Id) -> i64 {
    // SAFETY: as [`object_for_key`].
    let Some(value) = (unsafe { value_for_key(receiver, key) }) else {
        return 0;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" => 1,
        "no" | "false" => 0,
        other => other.parse().unwrap_or(0),
    }
}

unsafe extern "C" fn bool_for_key(receiver: Id, _sel: Sel, key: Id) -> u8 {
    u8::from(number_for(receiver, key) != 0)
}

unsafe extern "C" fn integer_for_key(receiver: Id, _sel: Sel, key: Id) -> i64 {
    number_for(receiver, key)
}

unsafe extern "C" fn double_for_key(receiver: Id, _sel: Sel, key: Id) -> f64 {
    // SAFETY: as [`object_for_key`].
    unsafe { value_for_key(receiver, key) }
        .as_deref()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0)
}

unsafe extern "C" fn set_bool_for_key(receiver: Id, _sel: Sel, value: u8, key: Id) {
    store(
        receiver,
        key,
        match value != 0 {
            true => "YES",
            false => "NO",
        },
    );
}

unsafe extern "C" fn set_integer_for_key(receiver: Id, _sel: Sel, value: i64, key: Id) {
    store(receiver, key, &value.to_string());
}

unsafe extern "C" fn set_double_for_key(receiver: Id, _sel: Sel, value: f64, key: Id) {
    store(receiver, key, &value.to_string());
}

fn store(receiver: Id, key: Id, value: &str) {
    // SAFETY: as [`object_for_key`].
    let Some(key) = (unsafe { scoped(receiver, key) }) else {
        return;
    };
    let mut values = values();
    values.insert(key, value.to_string());
    save(&values);
}

/// `-stringForKey:` is `-objectForKey:` here, because everything is stored as
/// one.
unsafe extern "C" fn string_for_key(receiver: Id, sel: Sel, key: Id) -> Id {
    // SAFETY: the same arguments.
    unsafe { object_for_key(receiver, sel, key) }
}

/// This string-backed defaults store has no values whose persisted type is an
/// array, a dictionary or data, so each of those getters answers nil.
unsafe extern "C" fn string_array_for_key(_receiver: Id, _sel: Sel, _key: Id) -> Id {
    std::ptr::null_mut()
}

/// `-synchronize`, which has already happened: every write goes to disk as it
/// is made, so there is nothing deferred to flush. `YES`, because that is what
/// a caller checks and it is true.
unsafe extern "C" fn synchronize(_receiver: Id, _sel: Sel) -> u8 {
    1
}

/// `-registerDefaults:`, which supplies values for keys nothing has set.
///
/// Registered defaults are **not** persisted: Apple's live in a separate
/// volatile domain and are re-registered on every launch, so writing them to
/// the store would turn a default into a stored value the program can no longer
/// change by changing its own registration.
unsafe extern "C" fn register_defaults(receiver: Id, _sel: Sel, registration: Id) {
    // SAFETY: null or an `NSDictionary` this tree made.
    let Some(dictionary) =
        (unsafe { corefoundation::collections::dictionary(registration as CFTypeRef) })
    else {
        return;
    };
    let mut values = values();
    for (key, value) in dictionary.entries.iter() {
        // SAFETY: the dictionary's own keys and values.
        let (Some(key), value) = (unsafe { key_text(*key as Id) }, unsafe {
            crate::string::bytes(*value as Id)
        }) else {
            continue;
        };
        // SAFETY: an `NSUserDefaults` this module made.
        values
            .entry(format!("{}{key}", unsafe { prefix(receiver) }))
            .or_insert_with(|| String::from_utf8_lossy(&value).into_owned());
    }
}

/// An empty dictionary, autoreleased.
fn empty_dictionary() -> Id {
    let dictionary = corefoundation::collections::new_dictionary(
        &[],
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh dictionary this call owns.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// The argument domain is volatile and never enters the persistent store.
unsafe extern "C" fn volatile_domain_for_name(_receiver: Id, _sel: Sel, name: Id) -> Id {
    // SAFETY: the domain name is null or an NSString.
    if unsafe { key_text(name) }.as_deref() == Some("NSArgumentDomain") {
        dictionary_from_values(&argument_values())
    } else {
        empty_dictionary()
    }
}

/// Expose the system preference in the global persistent domain.
unsafe extern "C" fn domain_for_name(_receiver: Id, _sel: Sel, name: Id) -> Id {
    let mut domain = BTreeMap::new();
    if unsafe { key_text(name) }.as_deref() == Some("NSGlobalDomain") {
        add_interface_style(&mut domain);
    }
    dictionary_from_values(&domain)
}

/// `-dictionaryRepresentation`, which is every key this store has.
unsafe extern "C" fn dictionary_representation(_receiver: Id, _sel: Sel) -> Id {
    let mut domain = values().clone();
    add_interface_style(&mut domain);
    dictionary_from_values(&domain)
}

fn add_interface_style(values: &mut BTreeMap<String, String>) {
    values.remove(corefoundation::preferences::INTERFACE_STYLE);
    if let Some(style) = corefoundation::preferences::interface_style() {
        values.insert(
            corefoundation::preferences::INTERFACE_STYLE.to_owned(),
            style.to_owned(),
        );
    }
}

fn dictionary_from_values(values: &BTreeMap<String, String>) -> Id {
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = values
        .iter()
        .map(|(key, value)| {
            (
                crate::string::from_str(key) as CFTypeRef,
                crate::string::from_str(value) as CFTypeRef,
            )
        })
        .collect();
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (key, value) in pairs {
        // SAFETY: the dictionary retained both; these are this call's own
        // references.
        unsafe {
            corefoundation::api::CFRelease(key);
            corefoundation::api::CFRelease(value);
        }
    }
    // SAFETY: a fresh dictionary this call owns.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// `-setVolatileDomain:forName:`, `-setPersistentDomain:forName:` and
/// `-removePersistentDomainForName:`.
///
/// Domain mutation is not implemented; the argument domain comes from guest argv.
unsafe extern "C" fn set_domain(_receiver: Id, _sel: Sel, _domain: Id, _name: Id) {}

unsafe extern "C" fn remove_domain(_receiver: Id, _sel: Sel, _name: Id) {}

pub(crate) fn install() {
    let class =
        corefoundation::base::register(c"NSUserDefaults", None, size_of::<Defaults>(), false);
    class_methods!(class, {
        "standardUserDefaults" => standard_defaults, c"@16@0:8";
    });
    methods!(class, {
        "objectForKey:" => object_for_key, c"@24@0:8@16";
        "valueForKey:" => object_for_key, c"@24@0:8@16";
        "valueForKeyPath:" => object_for_key, c"@24@0:8@16";
        "stringForKey:" => string_for_key, c"@24@0:8@16";
        "stringArrayForKey:" => string_array_for_key, c"@24@0:8@16";
        "arrayForKey:" => string_array_for_key, c"@24@0:8@16";
        "dictionaryForKey:" => string_array_for_key, c"@24@0:8@16";
        "dataForKey:" => string_array_for_key, c"@24@0:8@16";
        "setObject:forKey:" => set_object_for_key, c"v32@0:8@16@24";
        "removeObjectForKey:" => remove_object_for_key, c"v24@0:8@16";
        "boolForKey:" => bool_for_key, c"c24@0:8@16";
        "integerForKey:" => integer_for_key, c"q24@0:8@16";
        "doubleForKey:" => double_for_key, c"d24@0:8@16";
        "setBool:forKey:" => set_bool_for_key, c"v28@0:8c16@20";
        "setInteger:forKey:" => set_integer_for_key, c"v32@0:8q16@24";
        "setDouble:forKey:" => set_double_for_key, c"v32@0:8d16@24";
        "synchronize" => synchronize, c"c16@0:8";
        "registerDefaults:" => register_defaults, c"v24@0:8@16";
        "initWithSuiteName:" => init_with_suite_name, c"@24@0:8@16";
        "addSuiteNamed:" => add_suite_named, c"v24@0:8@16";
        "removeSuiteNamed:" => remove_suite_named, c"v24@0:8@16";
        "volatileDomainForName:" => volatile_domain_for_name, c"@24@0:8@16";
        "persistentDomainForName:" => domain_for_name, c"@24@0:8@16";
        "dictionaryRepresentation" => dictionary_representation, c"@16@0:8";
        "setVolatileDomain:forName:" => set_domain, c"v32@0:8@16@24";
        "setPersistentDomain:forName:" => set_domain, c"v32@0:8@16@24";
        "removePersistentDomainForName:" => remove_domain, c"v24@0:8@16";
        "removeVolatileDomainForName:" => remove_domain, c"v24@0:8@16";
    });
}

#[cfg(test)]
mod tests {

    /// `-setBool:` then `-integerForKey:` has to agree, which is the whole
    /// reason the value is stored as text and read by conversion.
    #[test]
    fn a_boolean_reads_back_as_a_number() {
        assert_eq!(convert("YES"), 1);
        assert_eq!(convert("true"), 1);
        assert_eq!(convert("NO"), 0);
        assert_eq!(convert("42"), 42);
        assert_eq!(convert("not a number"), 0);
    }

    fn convert(stored: &str) -> i64 {
        match stored.trim().to_ascii_lowercase().as_str() {
            "yes" | "true" => 1,
            "no" | "false" => 0,
            other => other.parse().unwrap_or(0),
        }
    }
}
