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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AdapterRank {
    device_class: u8,
    device_local_memory: u64,
    max_image_dimension_2d: u32,
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
    /// `VK_KHR_surface`, present only where the instance could be created
    /// with the extension - a headless host has no surface extension at all,
    /// and presentation falls back to the readback path there.
    surface_api: Option<ash::khr::surface::Instance>,
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
    let surface_extensions = surface_extensions(&entry);
    let enabled = surface_extensions
        .iter()
        .map(|name| name.as_ptr())
        .collect::<Vec<_>>();
    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&application)
        .enabled_extension_names(&enabled);
    // SAFETY: the create info, its application info and the extension name
    // pointers - which are static - live through the call, and the default
    // allocator is used.
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .map_err(|error| format!("vkCreateInstance failed with {error:?}"))?;
    let surface_api = surface_extensions
        .contains(&ash::khr::surface::NAME)
        .then(|| ash::khr::surface::Instance::new(&entry, &instance));

    // SAFETY: `instance` is live and ash owns its loaded instance functions.
    let physical = unsafe { instance.enumerate_physical_devices() }
        .map_err(|error| format!("vkEnumeratePhysicalDevices failed with {error:?}"))?;
    let mut adapters = physical
        .into_iter()
        .map(|handle| {
            // SAFETY: every handle was returned by this live instance.
            let properties = unsafe { instance.get_physical_device_properties(handle) };
            // SAFETY: the physical device belongs to this live instance.
            let features = unsafe { instance.get_physical_device_features(handle) };
            // SAFETY: the physical device belongs to this live instance.
            let memory = unsafe { instance.get_physical_device_memory_properties(handle) };
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
            (
                adapter_rank(&properties, &memory),
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
                },
            )
        })
        .collect::<Vec<_>>();
    // Vulkan does not promise any useful enumeration order. Metal's default
    // device is the first adapter below, so prefer dedicated graphics hardware
    // and use capacity as a deterministic performance proxy within each class.
    adapters.sort_by(|(left, _), (right, _)| right.cmp(left));
    let adapters = adapters
        .into_iter()
        .map(|(_, adapter)| adapter)
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
        surface_api,
        _entry: entry,
        adapters,
    })
}

fn adapter_rank(
    properties: &vk::PhysicalDeviceProperties,
    memory: &vk::PhysicalDeviceMemoryProperties,
) -> AdapterRank {
    let device_class = match properties.device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => 5,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 4,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 3,
        vk::PhysicalDeviceType::OTHER => 2,
        vk::PhysicalDeviceType::CPU => 1,
        _ => 0,
    };
    let device_local_memory = memory.memory_heaps[..memory.memory_heap_count as usize]
        .iter()
        .filter(|heap| heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .map(|heap| heap.size)
        .sum();
    AdapterRank {
        device_class,
        device_local_memory,
        max_image_dimension_2d: properties.limits.max_image_dimension2_d,
    }
}

/// The surface extensions to create the instance with: `VK_KHR_surface` and
/// every window-system one the loader advertises.
///
/// Presentation needs these enabled on the *instance*, and the instance is
/// created when a guest first asks for the adapter list - which is before it
/// has a window, and so before anything could ask SDL which extensions its
/// video backend wants. Asking the loader instead removes the ordering
/// question entirely: enabling an extension the loader reports is harmless,
/// the list is a superset of whatever SDL will need, and it costs nothing on
/// a host which has none of them.
fn surface_extensions(entry: &Entry) -> Vec<&'static std::ffi::CStr> {
    // SAFETY: the entry owns a loaded Vulkan library.
    let Ok(available) = (unsafe { entry.enumerate_instance_extension_properties(None) }) else {
        return Vec::new();
    };
    let has = |name: &std::ffi::CStr| {
        available.iter().any(|property| {
            property
                .extension_name_as_c_str()
                .is_ok_and(|found| found == name)
        })
    };
    if !has(ash::khr::surface::NAME) {
        return Vec::new();
    }
    let mut wanted = vec![ash::khr::surface::NAME];
    for name in [
        ash::khr::xlib_surface::NAME,
        ash::khr::xcb_surface::NAME,
        ash::khr::wayland_surface::NAME,
        ash::khr::win32_surface::NAME,
        ash::khr::android_surface::NAME,
        ash::ext::metal_surface::NAME,
        ash::mvk::macos_surface::NAME,
    ] {
        if has(name) {
            wanted.push(name);
        }
    }
    wanted
}

/// `VK_KHR_surface`'s instance functions, where the instance has it.
pub fn surface_api() -> Option<&'static ash::khr::surface::Instance> {
    STATE.get()?.as_ref().ok()?.surface_api.as_ref()
}

/// The raw `VkInstance`, for SDL to make a surface against.
pub fn instance_handle() -> Option<u64> {
    use vk::Handle;
    Some(STATE.get()?.as_ref().ok()?.instance.handle().as_raw())
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
    fn adapter_rank_prefers_discrete_graphics_over_more_shared_memory() {
        let discrete = properties(vk::PhysicalDeviceType::DISCRETE_GPU, 8192);
        let integrated = properties(vk::PhysicalDeviceType::INTEGRATED_GPU, 16384);
        let discrete_memory = memory(4 * 1024 * 1024 * 1024);
        let integrated_memory = memory(64 * 1024 * 1024 * 1024);

        assert!(
            adapter_rank(&discrete, &discrete_memory)
                > adapter_rank(&integrated, &integrated_memory)
        );
    }

    #[test]
    fn adapter_rank_uses_device_local_memory_within_a_device_class() {
        let properties = properties(vk::PhysicalDeviceType::DISCRETE_GPU, 8192);

        assert!(
            adapter_rank(&properties, &memory(16 * 1024 * 1024 * 1024))
                > adapter_rank(&properties, &memory(8 * 1024 * 1024 * 1024))
        );
    }

    fn properties(
        device_type: vk::PhysicalDeviceType,
        image_dimension: u32,
    ) -> vk::PhysicalDeviceProperties {
        let mut properties = vk::PhysicalDeviceProperties::default();
        properties.device_type = device_type;
        properties.limits.max_image_dimension2_d = image_dimension;
        properties
    }

    fn memory(device_local: u64) -> vk::PhysicalDeviceMemoryProperties {
        let mut memory = vk::PhysicalDeviceMemoryProperties::default();
        memory.memory_heap_count = 1;
        memory.memory_heaps[0].size = device_local;
        memory.memory_heaps[0].flags = vk::MemoryHeapFlags::DEVICE_LOCAL;
        memory
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
