pub mod api;
pub mod hid;
pub mod manager;
pub mod power;
pub mod registry;
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
        "IOHIDManagerGetTypeID" => at!(api::IOHIDManagerGetTypeID),
        "IOHIDManagerSetDeviceMatching" => at!(api::IOHIDManagerSetDeviceMatching),
        "IOHIDManagerSetDeviceMatchingMultiple" => at!(api::IOHIDManagerSetDeviceMatchingMultiple),
        "IOHIDManagerRegisterDeviceMatchingCallback" => {
            at!(api::IOHIDManagerRegisterDeviceMatchingCallback)
        }
        "IOHIDManagerRegisterDeviceRemovalCallback" => {
            at!(api::IOHIDManagerRegisterDeviceRemovalCallback)
        }
        "IOHIDManagerRegisterInputValueCallback" => {
            at!(api::IOHIDManagerRegisterInputValueCallback)
        }
        "IOHIDManagerScheduleWithRunLoop" => at!(api::IOHIDManagerScheduleWithRunLoop),
        "IOHIDManagerUnscheduleFromRunLoop" => at!(api::IOHIDManagerUnscheduleFromRunLoop),

        "IOHIDDeviceRegisterInputValueCallback" => at!(api::IOHIDDeviceRegisterInputValueCallback),
        "IOHIDDeviceGetProperty" => at!(api::IOHIDDeviceGetProperty),
        "IOHIDDeviceCopyMatchingElements" => at!(api::IOHIDDeviceCopyMatchingElements),
        "IOHIDDeviceOpen" => at!(api::IOHIDDeviceOpen),
        "IOHIDDeviceClose" => at!(api::IOHIDDeviceClose),
        "IOHIDDeviceGetTypeID" => at!(api::IOHIDDeviceGetTypeID),
        "IOHIDDeviceGetValue" => at!(api::IOHIDDeviceGetValue),

        "IOHIDElementGetCookie" => at!(api::IOHIDElementGetCookie),
        "IOHIDElementGetUsagePage" => at!(api::IOHIDElementGetUsagePage),
        "IOHIDElementGetUsage" => at!(api::IOHIDElementGetUsage),
        "IOHIDElementGetType" => at!(api::IOHIDElementGetType),
        "IOHIDElementGetLogicalMin" => at!(api::IOHIDElementGetLogicalMin),
        "IOHIDElementGetLogicalMax" => at!(api::IOHIDElementGetLogicalMax),
        "IOHIDElementGetChildren" => at!(api::IOHIDElementGetChildren),
        "IOHIDElementGetTypeID" => at!(api::IOHIDElementGetTypeID),

        "IOHIDValueGetElement" => at!(api::IOHIDValueGetElement),
        "IOHIDValueGetIntegerValue" => at!(api::IOHIDValueGetIntegerValue),
        "IOHIDValueGetLength" => at!(api::IOHIDValueGetLength),
        "IOHIDValueGetScaledValue" => at!(api::IOHIDValueGetScaledValue),
        "IOHIDValueGetTypeID" => at!(api::IOHIDValueGetTypeID),

        "IOHIDManagerCopyDevices" => at!(api::IOHIDManagerCopyDevices),
        "IOHIDDeviceGetService" => at!(api::IOHIDDeviceGetService),
        "IOHIDDeviceScheduleWithRunLoop" => at!(api::IOHIDDeviceScheduleWithRunLoop),
        "IOHIDDeviceUnscheduleFromRunLoop" => at!(api::IOHIDDeviceUnscheduleFromRunLoop),
        "IOHIDDeviceRegisterRemovalCallback" => at!(api::IOHIDDeviceRegisterRemovalCallback),
        "IOHIDDeviceGetReport" => at!(api::IOHIDDeviceGetReport),
        "IOHIDDeviceSetReport" => at!(api::IOHIDDeviceSetReport),
        "IOHIDDeviceRegisterInputReportCallback" => at!(api::IOHIDDeviceRegisterInputReportCallback),

        _ => return power::symbol(name).or_else(|| registry::symbol(name)),
    })
}

/// Every name [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "IOHIDManagerCreate",
    "IOHIDManagerOpen",
    "IOHIDManagerClose",
    "IOHIDManagerGetTypeID",
    "IOHIDManagerSetDeviceMatching",
    "IOHIDManagerSetDeviceMatchingMultiple",
    "IOHIDManagerRegisterDeviceMatchingCallback",
    "IOHIDManagerRegisterDeviceRemovalCallback",
    "IOHIDManagerRegisterInputValueCallback",
    "IOHIDManagerScheduleWithRunLoop",
    "IOHIDManagerUnscheduleFromRunLoop",
    "IOHIDDeviceRegisterInputValueCallback",
    "IOHIDDeviceGetProperty",
    "IOHIDDeviceCopyMatchingElements",
    "IOHIDDeviceOpen",
    "IOHIDDeviceClose",
    "IOHIDDeviceGetTypeID",
    "IOHIDDeviceGetValue",
    "IOHIDElementGetCookie",
    "IOHIDElementGetUsagePage",
    "IOHIDElementGetUsage",
    "IOHIDElementGetType",
    "IOHIDElementGetLogicalMin",
    "IOHIDElementGetLogicalMax",
    "IOHIDElementGetChildren",
    "IOHIDElementGetTypeID",
    "IOHIDValueGetElement",
    "IOHIDValueGetIntegerValue",
    "IOHIDValueGetLength",
    "IOHIDValueGetScaledValue",
    "IOHIDValueGetTypeID",
    "IOHIDManagerCopyDevices",
    "IOHIDDeviceGetService",
    "IOHIDDeviceScheduleWithRunLoop",
    "IOHIDDeviceUnscheduleFromRunLoop",
    "IOHIDDeviceRegisterRemovalCallback",
    "IOHIDDeviceGetReport",
    "IOHIDDeviceSetReport",
    "IOHIDDeviceRegisterInputReportCallback",
];

/// Every name this crate answers: the HID family above, the power sources in
/// [`power`], and the registry in [`registry`].
pub fn names() -> Vec<&'static str> {
    NAMES
        .iter()
        .copied()
        .chain(power::NAMES.iter().copied())
        .chain(registry::NAMES.iter().copied())
        .collect()
}

/// The `kIOHID*Key` globals and [`registry`]'s master port, as
/// `(Mach-O name, address)`.
///
/// Each key is a `CFStringRef` **variable**: the guest loads the symbol's
/// address and then loads the string out of it, so the symbol is a cell holding
/// the object rather than the object. The string itself still has to be the
/// same one every time, because a matching dictionary compares its keys by
/// identity.
pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    let mut out = registry::data_symbols();
    out.extend(api::key_strings().into_iter().map(|(symbol, text)| {
        (
            symbol.to_string(),
            corefoundation::base::global_cell(symbol, key_object(text)),
        )
    }));
    out
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
        let missing: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|n| symbol(n).is_none())
            .collect();
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
