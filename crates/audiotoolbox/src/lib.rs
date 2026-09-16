use std::ffi::c_int;

#[allow(non_upper_case_globals)]
const kAUGraphErr_InvalidAudioUnit: c_int = -10864;

#[allow(non_snake_case)]
extern "C" fn NewAUGraph() -> c_int {
    kAUGraphErr_InvalidAudioUnit
}

const NAMES: &[&str] = &["NewAUGraph"];

pub fn names() -> Vec<&'static str> {
    NAMES.iter().copied().collect()
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "NewAUGraph" => NewAUGraph as *const () as u64,
        _ => return None,
    })
}
