pub mod api;
pub mod hid;
pub mod manager;
pub mod value;

/// This crate answers nothing without SDL3 to answer from.
pub fn available() -> bool {
    sdl3::available()
}

/// One entry point, as an address.
macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

/// The address `name` has here, or `None`.
pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "IOHIDManagerCreate" => at!(api::IOHIDManagerCreate),
        "IOHIDManagerOpen" => at!(api::IOHIDManagerOpen),
        "IOHIDManagerClose" => at!(api::IOHIDManagerClose),
        "IOHIDManagerSetDeviceMatching" => at!(api::IOHIDManagerSetDeviceMatching),
        "IOHIDManagerSetDeviceMatchingMultiple" => at!(api::IOHIDManagerSetDeviceMatchingMultiple),
        "IOHIDManagerRegisterDeviceMatchingCallback" => {
            at!(api::IOHIDManagerRegisterDeviceMatchingCallback)
        }
        "IOHIDManagerRegisterDeviceRemovalCallback" => {
            at!(api::IOHIDManagerRegisterDeviceRemovalCallback)
        }
        "IOHIDManagerScheduleWithRunLoop" => at!(api::IOHIDManagerScheduleWithRunLoop),
        "IOHIDManagerUnscheduleFromRunLoop" => at!(api::IOHIDManagerUnscheduleFromRunLoop),

        "IOHIDDeviceRegisterInputValueCallback" => at!(api::IOHIDDeviceRegisterInputValueCallback),
        "IOHIDDeviceGetProperty" => at!(api::IOHIDDeviceGetProperty),
        "IOHIDDeviceCopyMatchingElements" => at!(api::IOHIDDeviceCopyMatchingElements),
        "IOHIDDeviceOpen" => at!(api::IOHIDDeviceOpen),
        "IOHIDDeviceClose" => at!(api::IOHIDDeviceClose),

        "IOHIDElementGetCookie" => at!(api::IOHIDElementGetCookie),
        "IOHIDElementGetUsagePage" => at!(api::IOHIDElementGetUsagePage),
        "IOHIDElementGetUsage" => at!(api::IOHIDElementGetUsage),
        "IOHIDElementGetType" => at!(api::IOHIDElementGetType),
        "IOHIDElementGetLogicalMin" => at!(api::IOHIDElementGetLogicalMin),
        "IOHIDElementGetLogicalMax" => at!(api::IOHIDElementGetLogicalMax),
        "IOHIDElementGetChildren" => at!(api::IOHIDElementGetChildren),

        "IOHIDValueGetElement" => at!(api::IOHIDValueGetElement),
        "IOHIDValueGetIntegerValue" => at!(api::IOHIDValueGetIntegerValue),
        "IOHIDValueGetLength" => at!(api::IOHIDValueGetLength),
        "IOHIDValueGetScaledValue" => at!(api::IOHIDValueGetScaledValue),

        _ => return None,
    })
}

/// Every name [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "IOHIDManagerCreate",
    "IOHIDManagerOpen",
    "IOHIDManagerClose",
    "IOHIDManagerSetDeviceMatching",
    "IOHIDManagerSetDeviceMatchingMultiple",
    "IOHIDManagerRegisterDeviceMatchingCallback",
    "IOHIDManagerRegisterDeviceRemovalCallback",
    "IOHIDManagerScheduleWithRunLoop",
    "IOHIDManagerUnscheduleFromRunLoop",
    "IOHIDDeviceRegisterInputValueCallback",
    "IOHIDDeviceGetProperty",
    "IOHIDDeviceCopyMatchingElements",
    "IOHIDDeviceOpen",
    "IOHIDDeviceClose",
    "IOHIDElementGetCookie",
    "IOHIDElementGetUsagePage",
    "IOHIDElementGetUsage",
    "IOHIDElementGetType",
    "IOHIDElementGetLogicalMin",
    "IOHIDElementGetLogicalMax",
    "IOHIDElementGetChildren",
    "IOHIDValueGetElement",
    "IOHIDValueGetIntegerValue",
    "IOHIDValueGetLength",
    "IOHIDValueGetScaledValue",
];

/// The `kIOHID*Key` globals, as `(Mach-O name, address)`.
///
/// Each is a `CFStringRef` a guest reads once and uses as a dictionary key, so
/// it has to be a real string object and the same one every time.
pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    api::key_strings()
        .into_iter()
        .map(|(symbol, text)| (symbol.to_string(), key_object(text)))
        .collect()
}

fn key_object(text: &'static str) -> u64 {
    static KEYS: std::sync::Mutex<Vec<(&'static str, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, addr)) = keys.iter().find(|(t, _)| *t == text) {
        return *addr;
    }
    let object = corefoundation::string::from_bytes(text.as_bytes(), false);
    // Retained forever: a global a guest may release is one it can make dangle.
    objc::arc::retain(object as objc::object::Id);
    let addr = object as usize as u64;
    keys.push((text, addr));
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES.iter().copied().filter(|n| symbol(n).is_none()).collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    /// Every key a guest builds a matching dictionary from is the *same object*
    /// each time it is read, which is what makes it work as a key at all.
    #[test]
    fn a_key_global_is_one_object() {
        let first = data_symbols();
        let second = data_symbols();
        assert_eq!(first, second);
        assert!(first.iter().all(|(_, addr)| *addr != 0));
    }
}
