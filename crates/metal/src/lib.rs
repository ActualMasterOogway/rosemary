//! Metal.framework, backed by Vulkan.
//!
//! The device boundary, shader libraries and the object graph a game holds
//! live here; the Vulkan objects underneath them live in `gpu`, and the
//! resource, pipeline, encoding and presentation halves of the API each have
//! their own module.

#![allow(non_snake_case)]

mod completion;
mod encoder;
mod gpu;
mod object;
mod pipeline;
mod present;
mod resource;
mod vulkan;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use corefoundation::base::CFTypeRef;
use objc::object::Id;
use objc::sel::Sel;

use gpu::Gpu;
use object::methods;

struct Device {
    ordinal: u64,
    name: Id,
    target: *const metal_translate::TargetProfile,
    /// `None` when the adapter has no logical device: every resource entry
    /// point then fails through the Metal API rather than pretending.
    gpu: Option<&'static Gpu>,
}

struct Library {
    translated: metal_translate::TranslatedLibrary,
}

struct Function {
    library: Id,
    entry: usize,
}

static ALLOCATED_SIZE: AtomicU64 = AtomicU64::new(0);
static REPORTED_LIBRARY_ERROR: AtomicBool = AtomicBool::new(false);
static REPORTED_SOURCE_LIBRARY: AtomicBool = AtomicBool::new(false);

pub(crate) fn account(bytes: u64) {
    ALLOCATED_SIZE.fetch_add(bytes, Ordering::Relaxed);
}

pub(crate) unsafe extern "C" fn no_op(_receiver: Id, _sel: Sel) {}

/// The logical device behind an `MTLDevice`, or `None` when Vulkan gave none.
fn gpu_of(receiver: Id) -> Option<&'static Gpu> {
    // SAFETY: every caller is a method registered on `R3MTLDevice`.
    unsafe { object::body::<Device>(receiver) }.gpu
}

/// As `gpu_of`, for a caller holding an object which may not be a device.
pub(crate) fn gpu_of_device(device: Id) -> Option<&'static Gpu> {
    object::is_kind(device, c"R3MTLDevice").then(|| gpu_of(device))?
}

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        let class = object::register::<Device>(c"R3MTLDevice");
        let library_class = object::register::<Library>(c"R3MTLLibrary");
        let function_class = object::register::<Function>(c"R3MTLFunction");
        object::register::<pipeline::ComputePipeline>(c"R3MTLComputePipelineState");
        object::register::<pipeline::RenderPipeline>(c"R3MTLRenderPipelineState");

        methods!(class, {
            "name" => name, c"@16@0:8";
            "registryID" => registry_id, c"Q16@0:8";
            "isLowPower" => no, c"B16@0:8";
            "isHeadless" => no, c"B16@0:8";
            "isRemovable" => no, c"B16@0:8";
            "hasUnifiedMemory" => no, c"B16@0:8";
            "isDepth24Stencil8PixelFormatSupported" => no, c"B16@0:8";
            "recommendedMaxWorkingSetSize" => recommended_working_set, c"Q16@0:8";
            "maxBufferLength" => max_buffer_length, c"Q16@0:8";
            "maxThreadgroupMemoryLength" => max_threadgroup_memory, c"Q16@0:8";
            "supportsFeatureSet:" => supports_feature_set, c"B24@0:8Q16";
            "supportsFamily:" => supports_family, c"B24@0:8Q16";
            "argumentBuffersSupport" => argument_buffers_tier, c"Q16@0:8";
            "argumentBuffersTier" => argument_buffers_tier, c"Q16@0:8";
            "supportsShaderBarycentricCoordinates" => no, c"B16@0:8";
            "areBarycentricCoordsSupported" => no, c"B16@0:8";
            "supportsRaytracing" => no, c"B16@0:8";
            "supportsFunctionPointers" => no, c"B16@0:8";
            "supportsDynamicLibraries" => no, c"B16@0:8";
            "supports32BitFloatFiltering" => yes, c"B16@0:8";
            "supportsBCTextureCompression" => supports_bc, c"B16@0:8";
            "supportsPullModelInterpolation" => no, c"B16@0:8";
            "supportsTextureSampleCount:" => supports_sample_count, c"B24@0:8Q16";
            "newCommandQueue" => new_command_queue, c"@16@0:8";
            "newCommandQueueWithMaxCommandBufferCount:" => new_command_queue_with_count,
                c"@24@0:8Q16";
            "newBufferWithLength:options:" => new_buffer, c"@32@0:8Q16Q24";
            "newBufferWithBytes:length:options:" => new_buffer_with_bytes, c"@40@0:8^v16Q24Q32";
            "newTextureWithDescriptor:" => new_texture, c"@24@0:8@16";
            "newSamplerStateWithDescriptor:" => new_sampler_state, c"@24@0:8@16";
            "newDepthStencilStateWithDescriptor:" => new_depth_stencil_state, c"@24@0:8@16";
            "newLibraryWithData:error:" => new_library_with_data, c"@32@0:8@16^@24";
            "newLibraryWithSource:options:error:" => new_library_with_source, c"@40@0:8@16@24^@32";
            "newComputePipelineStateWithFunction:error:" => new_compute_pipeline,
                c"@32@0:8@16^@24";
            "newRenderPipelineStateWithDescriptor:error:" => new_render_pipeline,
                c"@32@0:8@16^@24";
            "currentAllocatedSize" => current_allocated_size, c"Q16@0:8";
        });
        methods!(library_class, {
            "newFunctionWithName:" => new_function_with_name, c"@24@0:8@16";
            "functionNames" => library_function_names, c"@16@0:8";
            "device" => library_device, c"@16@0:8";
            "setLabel:" => no_op, c"v24@0:8@16";
        });
        methods!(function_class, {
            "name" => function_name, c"@16@0:8";
            "setLabel:" => no_op, c"v24@0:8@16";
        });

        resource::install();
        encoder::install();
        pipeline::install();
        present::install();
    });
}

// -------------------------------------------------------------- device

unsafe extern "C" fn new_command_queue(receiver: Id, _sel: Sel) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    encoder::new_command_queue(gpu, receiver)
}

unsafe extern "C" fn new_command_queue_with_count(receiver: Id, sel: Sel, _count: u64) -> Id {
    // SAFETY: forwards to the unbounded form; nothing here bounds the number
    // of command buffers in flight.
    unsafe { new_command_queue(receiver, sel) }
}

unsafe extern "C" fn new_buffer(receiver: Id, _sel: Sel, length: usize, options: u64) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    resource::new_buffer(gpu, length, options)
}

unsafe extern "C" fn new_buffer_with_bytes(
    receiver: Id,
    _sel: Sel,
    bytes: *const u8,
    length: usize,
    options: u64,
) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    let buffer = resource::new_buffer(gpu, length, options);
    if !buffer.is_null() && !bytes.is_null() {
        // SAFETY: a buffer this call just made, mapped over `length` bytes,
        // and `length` readable bytes the guest promised.
        unsafe {
            let contents = object::body::<resource::Buffer>(buffer).inner.contents;
            std::ptr::copy_nonoverlapping(bytes, contents, length);
        }
    }
    buffer
}

unsafe extern "C" fn new_texture(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `MTLTextureDescriptor` is this crate's class.
    let descriptor = *unsafe { object::body::<resource::TextureDescriptor>(descriptor) };
    resource::new_texture(gpu, descriptor)
}

unsafe extern "C" fn new_sampler_state(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `MTLSamplerDescriptor` is this crate's class.
    let descriptor = *unsafe { object::body::<resource::SamplerDescriptor>(descriptor) };
    resource::new_sampler(gpu, &descriptor)
}

unsafe extern "C" fn new_depth_stencil_state(_receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `MTLDepthStencilDescriptor` is this crate's class.
    let state = *unsafe { object::body::<resource::DepthStencilState>(descriptor) };
    resource::new_depth_stencil_state(state)
}

unsafe extern "C" fn new_compute_pipeline(
    receiver: Id,
    _sel: Sel,
    function: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe { *error = std::ptr::null_mut() };
    }
    let fail = |message: &str| -> Id {
        if !error.is_null() {
            // SAFETY: as above.
            unsafe {
                *error = foundation::error::make("MTLComputePipelineErrorDomain", 3, message)
            };
        }
        gpu::trace(message);
        std::ptr::null_mut()
    };
    if function.is_null() || !object::is_kind(function, c"R3MTLFunction") {
        return fail("newComputePipelineStateWithFunction:error: requires an MTLFunction");
    }
    let Some(gpu) = gpu_of(receiver) else {
        return fail("Metal device has no Vulkan device");
    };
    // SAFETY: an `R3MTLFunction`, checked above.
    let entry = unsafe { object::body::<Function>(function) };
    // SAFETY: a function holds the library which vended it.
    let library = unsafe { object::body::<Library>(entry.library) };
    let artifact = &library.translated.entries[entry.entry];
    match pipeline::ComputePipeline::new(gpu, receiver, artifact) {
        Ok(state) => {
            objc::arc::retain(receiver);
            object::make(c"R3MTLComputePipelineState", state)
        }
        Err(reason) => fail(&reason),
    }
}

unsafe extern "C" fn new_render_pipeline(
    receiver: Id,
    _sel: Sel,
    descriptor: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe { *error = std::ptr::null_mut() };
    }
    let fail = |message: &str| -> Id {
        if !error.is_null() {
            // SAFETY: as above.
            unsafe { *error = foundation::error::make("MTLRenderPipelineErrorDomain", 3, message) };
        }
        gpu::trace(message);
        std::ptr::null_mut()
    };
    if descriptor.is_null() {
        return fail("newRenderPipelineStateWithDescriptor:error: requires a descriptor");
    }
    let Some(gpu) = gpu_of(receiver) else {
        return fail("Metal device has no Vulkan device");
    };
    // SAFETY: `MTLRenderPipelineDescriptor` is this crate's class.
    let described = unsafe { object::body::<pipeline::RenderPipelineDescriptor>(descriptor) };
    let Some(vertex) = (unsafe { artifact_of(described.vertex_function) }) else {
        return fail("render pipeline has no translated vertex function");
    };
    let Some(fragment) = (unsafe { artifact_of(described.fragment_function) }) else {
        return fail("render pipeline has no translated fragment function");
    };
    let (layouts, attributes) = if described.vertex_descriptor.is_null() {
        (
            [resource::VertexLayout::default(); resource::VERTEX_SLOTS],
            [resource::VertexAttribute::default(); resource::VERTEX_SLOTS],
        )
    } else {
        // SAFETY: `MTLVertexDescriptor` is this crate's class.
        let vertex_descriptor =
            unsafe { object::body::<resource::VertexDescriptor>(described.vertex_descriptor) };
        (vertex_descriptor.layouts, vertex_descriptor.attributes)
    };
    match pipeline::RenderPipeline::new(gpu, vertex, fragment, described, &layouts, &attributes) {
        Ok(state) => object::make(c"R3MTLRenderPipelineState", state),
        Err(reason) => fail(&reason),
    }
}

/// The translated entry point behind an `MTLFunction`.
///
/// # Safety
///
/// `function` must be null or an `R3MTLFunction`.
unsafe fn artifact_of(function: Id) -> Option<&'static metal_translate::EntryArtifact> {
    if function.is_null() || !object::is_kind(function, c"R3MTLFunction") {
        return None;
    }
    // SAFETY: the caller guarantees the class.
    let entry = unsafe { object::body::<Function>(function) };
    // SAFETY: a function holds the library which vended it, and a library
    // outlives every function it made.
    let library = unsafe { object::body::<Library>(entry.library) };
    library.translated.entries.get(entry.entry)
}

unsafe extern "C" fn current_allocated_size(_receiver: Id, _sel: Sel) -> u64 {
    ALLOCATED_SIZE.load(Ordering::Relaxed)
}

unsafe extern "C" fn name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `R3MTLDevice`.
    unsafe { object::body::<Device>(receiver) }.name
}

unsafe extern "C" fn registry_id(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: as above.
    unsafe { object::body::<Device>(receiver) }.ordinal
}

unsafe extern "C" fn no(_receiver: Id, _sel: Sel) -> bool {
    false
}

unsafe extern "C" fn recommended_working_set(_receiver: Id, _sel: Sel) -> u64 {
    1024 * 1024 * 1024
}

unsafe extern "C" fn max_buffer_length(receiver: Id, _sel: Sel) -> u64 {
    gpu_of(receiver).map_or(256 << 20, |gpu| {
        u64::from(gpu.limits.max_storage_buffer_range)
    })
}

unsafe extern "C" fn max_threadgroup_memory(receiver: Id, _sel: Sel) -> u64 {
    gpu_of(receiver).map_or(32 << 10, |gpu| {
        u64::from(gpu.limits.max_compute_shared_memory_size)
    })
}

/// `-supportsFamily:`.
///
/// A Vulkan device on a desktop driver is the Mac family, never an Apple
/// silicon one: the Apple families promise tile memory, imageblocks and
/// programmable blending, which this translator does not lower. A game which
/// asks and is told "no family at all" concludes the device is unusable.
unsafe extern "C" fn supports_family(_receiver: Id, _sel: Sel, family: u64) -> bool {
    matches!(
        family,
        // MTLGPUFamilyMac1 and Mac2.
        2001 | 2002
        // MTLGPUFamilyCommon1 to Common3.
        | 3001
            ..=3003
        // MTLGPUFamilyMetal3.
        | 5001
    )
}

/// `-supportsFeatureSet:`, the older spelling of the same question. Only the
/// macOS feature sets are answered; iOS and tvOS sets belong to other devices.
unsafe extern "C" fn supports_feature_set(_receiver: Id, _sel: Sel, feature_set: u64) -> bool {
    // MTLFeatureSet_macOS_GPUFamily1_v1 through _GPUFamily2_v1.
    matches!(feature_set, 10000..=10005)
}

unsafe extern "C" fn yes(_receiver: Id, _sel: Sel) -> bool {
    true
}

/// `-supportsBCTextureCompression`, which the pixel-format table needs in
/// order to answer a BC format at all.
unsafe extern "C" fn supports_bc(receiver: Id, _sel: Sel) -> bool {
    gpu_of(receiver).is_some()
}

/// `-argumentBuffersSupport`: `MTLArgumentBuffersTier1`, which is the tier
/// whose argument buffers this runtime could lower to descriptor sets.
unsafe extern "C" fn argument_buffers_tier(_receiver: Id, _sel: Sel) -> u64 {
    0
}

unsafe extern "C" fn supports_sample_count(_receiver: Id, _sel: Sel, count: u64) -> bool {
    matches!(count, 1 | 2 | 4)
}

// ------------------------------------------------------------- libraries

unsafe extern "C" fn new_library_with_data(
    receiver: Id,
    _sel: Sel,
    data: u64,
    _error: *mut Id,
) -> Id {
    let Some((bytes, length)) = darwin::dispatch::data::bytes(data) else {
        report_library_error("newLibraryWithData:error: received non-dispatch data");
        return std::ptr::null_mut();
    };
    // SAFETY: dispatch reported this range as the data's bytes.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, length) };
    let air = match metal_translate::metallib_air(bytes) {
        Ok(air) => air,
        Err(error) => {
            report_library_error(error);
            return std::ptr::null_mut();
        }
    };
    // SAFETY: an `R3MTLDevice`.
    let target = unsafe { object::body::<Device>(receiver) }.target;
    // SAFETY: the profile belongs to the process-wide adapter list.
    let Some(target) = (unsafe { target.as_ref() }) else {
        report_library_error("Metal device has no Vulkan target profile");
        return std::ptr::null_mut();
    };
    let translated =
        match metal_translate::translate(metal_translate::AirInput::Bitcode(air), target) {
            Ok(translated) => translated,
            Err(error) => {
                report_library_error(error);
                return std::ptr::null_mut();
            }
        };
    retain_library(receiver, translated)
}

/// Wrap a translated library in the `MTLLibrary` object the guest holds.
fn retain_library(device: Id, translated: metal_translate::TranslatedLibrary) -> Id {
    let library = object::make(c"R3MTLLibrary", Library { translated });
    if library.is_null() {
        report_library_error("could not allocate MTL library");
        return std::ptr::null_mut();
    }
    let _ = device;
    library
}

unsafe extern "C" fn new_library_with_source(
    receiver: Id,
    _sel: Sel,
    source: Id,
    _options: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe { *error = std::ptr::null_mut() };
    }
    if source.is_null() {
        return fail_library(error, "newLibraryWithSource: requires an NSString source");
    }
    // SAFETY: an `R3MTLDevice`.
    let target = unsafe { object::body::<Device>(receiver) }.target;
    // SAFETY: the profile belongs to the process-wide adapter list.
    let Some(target) = (unsafe { target.as_ref() }) else {
        return fail_library(error, "Metal device has no Vulkan target profile");
    };

    // SAFETY: a guest `NSString`.
    let source = unsafe { foundation::string::bytes(source) };
    if source.is_empty() {
        return fail_library(
            error,
            "newLibraryWithSource: received an empty or invalid NSString",
        );
    }
    let Ok(source) = std::str::from_utf8(&source) else {
        return fail_library(error, "Metal source is not valid UTF-8");
    };
    report_source_library(source);
    let translated = match metal_translate::translate_msl(source, target) {
        Ok(translated) => translated,
        Err(translation_error) => return fail_library(error, &translation_error.to_string()),
    };
    let library = retain_library(receiver, translated);
    if library.is_null() {
        return fail_library(error, "could not allocate MTL source library");
    }
    library
}

unsafe extern "C" fn new_function_with_name(receiver: Id, _sel: Sel, name: Id) -> Id {
    // SAFETY: a guest `NSString`.
    let name = unsafe { foundation::string::bytes(name) };
    let Ok(name) = std::str::from_utf8(&name) else {
        return std::ptr::null_mut();
    };
    // SAFETY: an `R3MTLLibrary`.
    let library = unsafe { object::body::<Library>(receiver) };
    let Some(entry) = library
        .translated
        .entries
        .iter()
        .position(|entry| entry.name == name)
    else {
        return std::ptr::null_mut();
    };
    object::make(
        c"R3MTLFunction",
        Function {
            library: objc::arc::retain(receiver),
            entry,
        },
    )
}

/// `-[MTLLibrary functionNames]`: the entry points the translation produced,
/// in the order the library declares them.
unsafe extern "C" fn library_function_names(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `R3MTLLibrary`.
    let library = unsafe { object::body::<Library>(receiver) };
    let names: Vec<CFTypeRef> = library
        .translated
        .entries
        .iter()
        .map(|entry| foundation::string::from_str(&entry.name) as CFTypeRef)
        .collect();
    // SAFETY: a fresh array over freshly created strings.
    unsafe {
        corefoundation::api::CFAutorelease(corefoundation::collections::new_array(
            &names,
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

unsafe extern "C" fn library_device(_receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the process has one Metal device per adapter and a library is
    // only made by one of them.
    unsafe { MTLCreateSystemDefaultDevice() }
}

unsafe extern "C" fn function_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `R3MTLFunction`.
    let function = unsafe { object::body::<Function>(receiver) };
    // SAFETY: the library which vended the function.
    let library = unsafe { object::body::<Library>(function.library) };
    foundation::string::from_str(&library.translated.entries[function.entry].name)
}

fn fail_library(error: *mut Id, message: &str) -> Id {
    report_library_error(message);
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe {
            *error = foundation::error::make("MTLLibraryErrorDomain", 3, message);
        }
    }
    std::ptr::null_mut()
}

fn report_library_error(message: impl std::fmt::Display) {
    if std::env::var_os("ROSETTA3_TRACE_METAL").is_some()
        && !REPORTED_LIBRARY_ERROR.swap(true, Ordering::Relaxed)
    {
        eprintln!("rosetta3: Metal: shader library translation stopped: {message}");
    }
}

/// Report just enough information to identify the first guest source library
/// without placing proprietary game shaders in a log file.
fn report_source_library(source: &str) {
    if std::env::var_os("ROSETTA3_TRACE_METAL").is_some()
        && !REPORTED_SOURCE_LIBRARY.swap(true, Ordering::Relaxed)
    {
        eprintln!(
            "rosetta3: Metal: source library: {} bytes, fnv1a64={:016x}",
            source.len(),
            fnv1a64(source.as_bytes()),
        );
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
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
            .map(|(ordinal, adapter)| {
                let gpu = match adapter.gpu() {
                    Ok(gpu) => Some(gpu),
                    Err(reason) => {
                        eprintln!(
                            "rosetta3: Metal: {} has no logical device: {reason}",
                            adapter.name
                        );
                        None
                    }
                };
                object::make(
                    c"R3MTLDevice",
                    Device {
                        ordinal: ordinal as u64 + 1,
                        name: foundation::string::from_str(&adapter.name),
                        target: &adapter.target,
                        gpu,
                    },
                ) as usize
            })
            .filter(|device| *device != 0)
            .collect()
    })
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
    // SAFETY: called through the framework ABI.
    let device = unsafe { MTLCreateSystemDefaultDevice() };
    objc::arc::retain(device)
}

pub const NAMES: &[&str] = &[
    "MTLCreateSystemDefaultDevice",
    "MTLCopyAllDevices",
    "CGDirectDisplayCopyCurrentMetalDevice",
];

pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    [
        "MTLTextureDescriptor",
        "MTLVertexDescriptor",
        "MTLSamplerDescriptor",
        "MTLDepthStencilDescriptor",
        "MTLStencilDescriptor",
        "MTLRenderPipelineDescriptor",
        "MTLRenderPassDescriptor",
        "MTLCompileOptions",
    ]
    .into_iter()
    .filter_map(|name| {
        let name_c = std::ffi::CString::new(name).ok()?;
        let class = objc::class::lookup(&name_c)?;
        Some((
            format!("_OBJC_CLASS_$_{name}"),
            objc::class::ptr(class) as usize as u64,
        ))
    })
    .collect()
}

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
mod tests;
