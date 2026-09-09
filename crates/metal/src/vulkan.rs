//! The Vulkan lifetime underneath Metal objects.

use std::sync::OnceLock;

use ash::{Entry, Instance, vk};
use vk::Handle;

use crate::gpu::Gpu;

pub struct Adapter {
    pub handle: usize,
    pub name: String,
    pub target: metal_translate::TargetProfile,
    /// The logical device, created the first time a Metal object needs one.
    /// Enumerating adapters must stay cheap: a guest asks for the device list
    /// long before it renders, and one of the adapters may never be used.
    gpu: OnceLock<Result<Gpu, String>>,
}

impl Adapter {
    pub fn gpu(&self) -> Result<&Gpu, &str> {
        match self.gpu.get_or_init(|| {
            let state = STATE.get().expect("adapters were enumerated first");
            let instance = &state.as_ref().expect("this adapter came from it").instance;
            Gpu::create(instance, vk::PhysicalDevice::from_raw(self.handle as u64))
        }) {
            Ok(gpu) => Ok(gpu),
            Err(reason) => Err(reason),
        }
    }
}

struct State {
    instance: Instance,
    _entry: Entry,
    adapters: Vec<Adapter>,
}

impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: the instance was created by this state and all objects which
        // depend on it are dropped before the process-wide state.
        unsafe { self.instance.destroy_instance(None) };
    }
}

static STATE: OnceLock<Result<State, String>> = OnceLock::new();

pub fn adapters() -> Result<&'static [Adapter], &'static str> {
    match STATE.get_or_init(initialize) {
        Ok(state) => Ok(&state.adapters),
        Err(reason) => Err(reason),
    }
}

fn initialize() -> Result<State, String> {
    let entry = load_entry()?;
    let application = vk::ApplicationInfo::default()
        .application_name(c"rosetta3")
        .application_version(1)
        .engine_name(c"metal_translate")
        .engine_version(1)
        .api_version(vk::API_VERSION_1_1);
    let create_info = vk::InstanceCreateInfo::default().application_info(&application);
    // SAFETY: the create info and its application info live through the call,
    // and the default allocator is used.
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .map_err(|error| format!("vkCreateInstance failed with {error:?}"))?;

    // SAFETY: `instance` is live and ash owns its loaded instance functions.
    let physical = unsafe { instance.enumerate_physical_devices() }
        .map_err(|error| format!("vkEnumeratePhysicalDevices failed with {error:?}"))?;
    let adapters = physical
        .into_iter()
        .map(|handle| {
            // SAFETY: every handle was returned by this live instance.
            let properties = unsafe { instance.get_physical_device_properties(handle) };
            // SAFETY: the physical device belongs to this live instance.
            let features = unsafe { instance.get_physical_device_features(handle) };
            // Variable pointers is a Vulkan 1.1 feature with a structure of
            // its own; a shader which phis or selects a pointer needs it, and
            // the translator rejects such a shader where it is absent.
            let mut pointers = vk::PhysicalDeviceVariablePointersFeatures::default();
            let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut pointers);
            // SAFETY: as above; `features2` and its chained structure live
            // across the call.
            unsafe { instance.get_physical_device_features2(handle, &mut features2) };
            let name = properties.device_name_as_c_str().map_or_else(
                |_| "Vulkan Device".to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            let version = (
                vk::api_version_major(properties.api_version) as u8,
                vk::api_version_minor(properties.api_version) as u8,
            );
            Adapter {
                handle: handle.as_raw() as usize,
                name,
                target: metal_translate::TargetProfile {
                    shader_int64: features.shader_int64 == vk::TRUE,
                    variable_pointers_storage_buffer: pointers.variable_pointers_storage_buffer
                        == vk::TRUE,
                    variable_pointers: pointers.variable_pointers == vk::TRUE,
                    ..target_profile(version)
                },
                gpu: OnceLock::new(),
            }
        })
        .collect::<Vec<_>>();
    if adapters.is_empty() {
        // SAFETY: no child objects were created from the instance.
        unsafe { instance.destroy_instance(None) };
        return Err("Vulkan reported no physical devices".to_string());
    }
    trace(format!(
        "created Vulkan 1.1 instance with {} adapter(s)",
        adapters.len()
    ));
    Ok(State {
        instance,
        _entry: entry,
        adapters,
    })
}

fn load_entry() -> Result<Entry, String> {
    if let Some(path) = std::env::var_os("ROSETTA3_VULKAN_LIBRARY") {
        // SAFETY: the entry and its library remain owned by the process-wide state.
        return unsafe { Entry::load_from(&path) }.map_err(|error| {
            format!(
                "the Vulkan loader at {:?} could not be opened: {error}",
                path
            )
        });
    }
    // SAFETY: the entry and its library remain owned by the process-wide state.
    unsafe { Entry::load() }
        .map_err(|error| format!("the Vulkan loader could not be opened: {error}"))
}

fn target_profile(version: (u8, u8)) -> metal_translate::TargetProfile {
    let spirv_version = match version {
        (major, minor) if major > 1 || minor >= 3 => (1, 6),
        (1, 2) => (1, 5),
        _ => (1, 3),
    };
    metal_translate::TargetProfile {
        vulkan_version: version,
        spirv_version,
        max_bound_descriptor_sets: 4,
        shader: true,
        shader_int64: false,
        variable_pointers_storage_buffer: false,
        variable_pointers: false,
    }
}

fn trace(message: impl std::fmt::Display) {
    if std::env::var_os("ROSETTA3_TRACE_METAL").is_some() {
        eprintln!("rosetta3: Metal: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ash_vulkan_version_is_exact() {
        assert_eq!(vk::API_VERSION_1_1, 0x0040_1000);
    }

    #[test]
    fn translator_profile_tracks_the_vulkan_version() {
        assert_eq!(target_profile((1, 1)).spirv_version, (1, 3));
        assert_eq!(target_profile((1, 2)).spirv_version, (1, 5));
        assert_eq!(target_profile((1, 3)).spirv_version, (1, 6));
    }

    #[test]
    fn configured_vulkan_runtime_enumerates_an_adapter() {
        if std::env::var_os("ROSETTA3_TEST_VULKAN").is_none() {
            return;
        }
        let adapters = adapters().expect("configured Vulkan runtime");
        assert!(!adapters.is_empty());
        assert!(adapters[0].target.vulkan_version >= (1, 1));
        assert!(adapters[0].target.max_bound_descriptor_sets >= 4);
    }
}
