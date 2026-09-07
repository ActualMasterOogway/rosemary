//! Metal.framework's device boundary, backed by Vulkan adapter discovery.

#![allow(non_snake_case)]

mod vulkan;

use std::ffi::c_void;
use std::sync::OnceLock;

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

#[repr(C)]
struct Device {
    isa: *const Class,
    physical: usize,
    ordinal: u64,
    name: Id,
    target: *const metal_translate::TargetProfile,
}

fn device_class() -> &'static Class {
    objc::class::lookup(c"R3MTLDevice").expect("Metal is bootstrapped")
}

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        let class =
            corefoundation::base::register(c"R3MTLDevice", None, size_of::<Device>(), false);
        for (selector, implementation, types) in [
            ("name", name as *const c_void, c"@16@0:8" as &std::ffi::CStr),
            ("registryID", registry_id as *const c_void, c"Q16@0:8"),
            ("isLowPower", no as *const c_void, c"B16@0:8"),
            ("isHeadless", no as *const c_void, c"B16@0:8"),
            ("isRemovable", no as *const c_void, c"B16@0:8"),
            ("hasUnifiedMemory", no as *const c_void, c"B16@0:8"),
            (
                "recommendedMaxWorkingSetSize",
                recommended_working_set as *const c_void,
                c"Q16@0:8",
            ),
            (
                "supportsFeatureSet:",
                supports_u64 as *const c_void,
                c"B24@0:8Q16",
            ),
            (
                "supportsFamily:",
                supports_u64 as *const c_void,
                c"B24@0:8Q16",
            ),
            (
                "supportsTextureSampleCount:",
                supports_sample_count as *const c_void,
                c"B24@0:8Q16",
            ),
        ] {
            class.replace_method(objc::sel::get(selector), implementation, types);
        }
    });
}

fn devices() -> &'static [usize] {
    static DEVICES: OnceLock<Vec<usize>> = OnceLock::new();
    DEVICES.get_or_init(|| {
        bootstrap();
        let adapters = match vulkan::adapters() {
            Ok(adapters) => adapters,
            Err(reason) => {
                eprintln!("rosetta3: Metal: {reason}");
                return Vec::new();
            }
        };
        adapters
            .iter()
            .enumerate()
            .filter_map(|(ordinal, adapter)| {
                // SAFETY: this is the layout registered for R3MTLDevice.
                let object = unsafe { corefoundation::base::allocate::<Device>(device_class()) };
                if object.is_null() {
                    return None;
                }
                // SAFETY: a fresh instance whose fields after isa are ours.
                unsafe {
                    (*object).physical = adapter.handle;
                    (*object).ordinal = ordinal as u64 + 1;
                    (*object).name = foundation::string::from_str(&adapter.name);
                    (*object).target = &adapter.target;
                }
                Some(object as usize)
            })
            .collect()
    })
}

unsafe extern "C" fn name(receiver: Id, _sel: Sel) -> Id {
    unsafe { (*(receiver as *const Device)).name }
}

unsafe extern "C" fn registry_id(receiver: Id, _sel: Sel) -> u64 {
    unsafe { (*(receiver as *const Device)).ordinal }
}

unsafe extern "C" fn no(_receiver: Id, _sel: Sel) -> bool {
    false
}

unsafe extern "C" fn recommended_working_set(_receiver: Id, _sel: Sel) -> u64 {
    1024 * 1024 * 1024
}

unsafe extern "C" fn supports_u64(_receiver: Id, _sel: Sel, _value: u64) -> bool {
    false
}

unsafe extern "C" fn supports_sample_count(_receiver: Id, _sel: Sel, count: u64) -> bool {
    matches!(count, 1 | 2 | 4)
}

/// Return the first Vulkan-backed Metal device, or nil when Vulkan is unavailable.
///
/// # Safety
///
/// Called through the Metal framework ABI.
pub unsafe extern "C" fn MTLCreateSystemDefaultDevice() -> Id {
    devices()
        .first()
        .copied()
        .map_or(std::ptr::null_mut(), |device| device as Id)
}

/// Return a retained array containing every Vulkan-backed Metal device.
///
/// # Safety
///
/// Called through the Metal framework ABI.
pub unsafe extern "C" fn MTLCopyAllDevices() -> Id {
    bootstrap();
    let values = devices()
        .iter()
        .map(|device| *device as CFTypeRef)
        .collect::<Vec<_>>();
    corefoundation::collections::new_array(
        &values,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    ) as Id
}

/// Return a retained device for the requested display.
///
/// # Safety
///
/// Called through the CoreGraphics framework ABI.
pub unsafe extern "C" fn CGDirectDisplayCopyCurrentMetalDevice(_display: u32) -> Id {
    let device = unsafe { MTLCreateSystemDefaultDevice() };
    objc::arc::retain(device)
}

pub const NAMES: &[&str] = &[
    "MTLCreateSystemDefaultDevice",
    "MTLCopyAllDevices",
    "CGDirectDisplayCopyCurrentMetalDevice",
];

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name.trim_start_matches('_') {
        "MTLCreateSystemDefaultDevice" => MTLCreateSystemDefaultDevice as *const (),
        "MTLCopyAllDevices" => MTLCopyAllDevices as *const (),
        "CGDirectDisplayCopyCurrentMetalDevice" => {
            CGDirectDisplayCopyCurrentMetalDevice as *const ()
        }
        _ => return None,
    } as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_export_resolves_with_both_spellings() {
        for name in NAMES {
            assert_ne!(symbol(name), None);
            assert_eq!(symbol(&format!("_{name}")), symbol(name));
        }
    }

    #[test]
    fn configured_vulkan_adapters_become_metal_devices() {
        if std::env::var_os("ROSETTA3_TEST_VULKAN").is_none() {
            return;
        }
        unsafe {
            let device = MTLCreateSystemDefaultDevice();
            assert!(!device.is_null());
            let all = MTLCopyAllDevices();
            assert!(corefoundation::api::CFArrayGetCount(all as CFTypeRef) >= 1);
            corefoundation::api::CFRelease(all as CFTypeRef);
            let copied = CGDirectDisplayCopyCurrentMetalDevice(0);
            assert_eq!(copied, device);
            objc::arc::release(copied);
        }
    }
}
