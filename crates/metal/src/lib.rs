//! Metal.framework, backed by Vulkan.
//!
//! The device boundary, shader libraries and the object graph a game holds
//! live here; the Vulkan objects underneath them live in `gpu`, and the
//! resource, pipeline, encoding and presentation halves of the API each have
//! their own module.

#![allow(non_snake_case)]

mod capture;
mod completion;
mod encoder;
mod gpu;
mod heap;
mod object;
mod pipeline;
mod present;
mod resource;
pub mod stats;
mod submission;
mod swapchain;
mod vulkan;

use std::collections::BTreeSet;
use std::sync::Mutex;
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
    /// The AIR the library was made from, kept only where it declares function
    /// constants: each set of values a function is made with is a translation
    /// of its own.
    air: Option<Vec<u8>>,
    /// The profile of the device which made the library.
    target: Option<&'static metal_translate::TargetProfile>,
}

struct Function {
    library: Id,
    entry: usize,
    /// The entry point translated for the function constants this function
    /// was made with, in place of the library's own.
    specialized: Option<metal_translate::EntryArtifact>,
}

impl Function {
    /// The translated entry point this function runs.
    ///
    /// # Safety
    ///
    /// The function's library must be alive, which it is for as long as the
    /// function holds it.
    unsafe fn artifact(&self) -> Option<&'static metal_translate::EntryArtifact> {
        if let Some(artifact) = &self.specialized {
            // SAFETY: the artifact lives in this function's body, which
            // outlives every pipeline made from it.
            return Some(unsafe { &*(artifact as *const metal_translate::EntryArtifact) });
        }
        // SAFETY: the caller's obligation.
        let library = unsafe { object::body::<Library>(self.library) };
        library.translated.entries.get(self.entry)
    }
}

/// `MTLFunctionConstantValues`: the values a guest specializes a function
/// with.
struct ConstantValues {
    values: metal_translate::FunctionConstantValues,
}

static ALLOCATED_SIZE: AtomicU64 = AtomicU64::new(0);

/// Diagnostics [`report_library_error`] has already printed.
///
/// A rejected library is reported by the shape it was rejected for rather than
/// once per library: a game loads thousands, and the same gap in the translator
/// accounts for a run of them. Keyed by the message so that a run collapses to
/// one line and a new gap is still visible.
static REPORTED_LIBRARY_ERRORS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static REPORTED_SOURCE_LIBRARY: AtomicBool = AtomicBool::new(false);

/// Add to, or with a negative amount subtract from, what
/// `-[MTLDevice currentAllocatedSize]` reports.
pub(crate) fn account(bytes: i64) {
    match bytes < 0 {
        true => ALLOCATED_SIZE.fetch_sub(bytes.unsigned_abs(), Ordering::Relaxed),
        false => ALLOCATED_SIZE.fetch_add(bytes as u64, Ordering::Relaxed),
    };
}

/// The number of live `MTLBuffer`s after this change, for the buffer trace.
pub(crate) fn live_buffers(delta: i64) -> i64 {
    static LIVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    LIVE.fetch_add(delta, Ordering::Relaxed) + delta
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
        let constant_values_class =
            object::register::<ConstantValues>(c"MTLFunctionConstantValues");
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
            "newFunctionWithName:constantValues:error:" => new_function_with_constants,
                c"@40@0:8@16@24^@32";
            "functionNames" => library_function_names, c"@16@0:8";
            "device" => library_device, c"@16@0:8";
            "setLabel:" => no_op, c"v24@0:8@16";
        });
        methods!(function_class, {
            "name" => function_name, c"@16@0:8";
            "setLabel:" => no_op, c"v24@0:8@16";
        });
        methods!(constant_values_class, {
            "init" => constant_values_init, c"@16@0:8";
            "reset" => constant_values_reset, c"v16@0:8";
            "dealloc" => constant_values_dealloc, c"v16@0:8";
            "setConstantValue:type:atIndex:" => set_constant_at_index, c"v40@0:8r^v16Q24Q32";
            "setConstantValue:type:withName:" => set_constant_with_name, c"v40@0:8r^v16Q24@32";
            "setConstantValues:type:withRange:" => set_constants_in_range,
                c"v48@0:8r^v16Q24{_NSRange=QQ}32";
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
    let Some(artifact) = (unsafe { entry.artifact() }) else {
        return fail("the MTLFunction has no translated entry point");
    };
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
    unsafe { entry.artifact() }
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
        match metal_translate::translate_cached(metal_translate::AirInput::Bitcode(air), target) {
            Ok(translated) => translated,
            Err(error) => {
                report_library_error(error);
                return std::ptr::null_mut();
            }
        };
    retain_library(receiver, translated, Some(air), Some(target))
}

/// Wrap a translated library in the `MTLLibrary` object the guest holds.
fn retain_library(
    device: Id,
    translated: metal_translate::TranslatedLibrary,
    air: Option<&[u8]>,
    target: Option<&'static metal_translate::TargetProfile>,
) -> Id {
    // Only a library with function constants is ever translated again.
    let air = air
        .filter(|_| !translated.function_constants.is_empty())
        .map(<[u8]>::to_vec);
    let library = object::make(
        c"R3MTLLibrary",
        Library {
            translated,
            air,
            target,
        },
    );
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
    dump_source_library(source);
    let translated = match metal_translate::translate_msl_cached(source, target) {
        Ok(translated) => translated,
        Err(translation_error) => return fail_library(error, &translation_error.to_string()),
    };
    let library = retain_library(receiver, translated, None, Some(target));
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
            specialized: None,
        },
    )
}

/// `-[MTLLibrary newFunctionWithName:constantValues:error:]`.
///
/// The library is translated again with the values, through the shader cache,
/// which keys an entry by them; specialization comes before the IR passes, so
/// the code a constant switches off is folded away before emission.
unsafe extern "C" fn new_function_with_constants(
    receiver: Id,
    _sel: Sel,
    name: Id,
    constants: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe { *error = std::ptr::null_mut() };
    }
    // SAFETY: a guest `NSString`.
    let name = unsafe { foundation::string::bytes(name) };
    let Ok(name) = std::str::from_utf8(&name) else {
        return fail_library(
            error,
            "newFunctionWithName: names a function in invalid UTF-8",
        );
    };
    // SAFETY: an `R3MTLLibrary`.
    let library = unsafe { object::body::<Library>(receiver) };
    let Some(entry) = library
        .translated
        .entries
        .iter()
        .position(|entry| entry.name == name)
    else {
        return fail_library(error, &format!("the library has no function named {name}"));
    };
    // A library which declares no function constants has nothing to
    // specialize: Metal ignores the values, and so does this.
    let specialized = match (&library.air, library.target) {
        (Some(air), Some(target)) => {
            let values = if object::is_kind(constants, c"MTLFunctionConstantValues") {
                // SAFETY: checked above.
                unsafe { object::body::<ConstantValues>(constants) }
                    .values
                    .clone()
            } else {
                metal_translate::FunctionConstantValues::new()
            };
            let translated = metal_translate::translate_specialized_cached(
                metal_translate::AirInput::Bitcode(air),
                target,
                metal_translate::PassPipeline::from_env(),
                &values,
            );
            match translated {
                Ok(mut translated) => {
                    let Some(position) = translated
                        .entries
                        .iter()
                        .position(|entry| entry.name == name)
                    else {
                        return fail_library(error, "specialization lost the function");
                    };
                    Some(translated.entries.swap_remove(position))
                }
                Err(translation) => return fail_library(error, &translation.to_string()),
            }
        }
        _ => None,
    };
    object::make(
        c"R3MTLFunction",
        Function {
            library: objc::arc::retain(receiver),
            entry,
            specialized,
        },
    )
}

// ------------------------------------------------- function constant values

unsafe extern "C" fn constant_values_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `MTLFunctionConstantValues` straight from `+alloc`, whose
    // body is this call's to establish.
    unsafe {
        object::init(
            receiver,
            ConstantValues {
                values: metal_translate::FunctionConstantValues::new(),
            },
        )
    };
    receiver
}

unsafe extern "C" fn constant_values_reset(receiver: Id, _sel: Sel) {
    // SAFETY: an initialized `MTLFunctionConstantValues`.
    unsafe { object::body::<ConstantValues>(receiver) }.values =
        metal_translate::FunctionConstantValues::new();
}

unsafe extern "C" fn constant_values_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: the body `-init` established, dropped once as the object goes.
    unsafe { std::ptr::drop_in_place(object::body::<ConstantValues>(receiver)) };
    objc::arc::finalize(receiver);
}

/// Read one value of an `MTLDataType` from where the guest keeps it.
///
/// # Safety
///
/// `value` must be null or point at a value of `data_type`.
unsafe fn constant_bytes<'a>(value: *const u8, data_type: u64) -> Option<&'a [u8]> {
    let Some(size) = metal_translate::metal_data_type_size(data_type) else {
        report_library_error(format!(
            "MTLFunctionConstantValues was given MTLDataType {data_type}, which no function constant has"
        ));
        return None;
    };
    if value.is_null() {
        return None;
    }
    // SAFETY: the caller's obligation, for exactly the value's size.
    Some(unsafe { std::slice::from_raw_parts(value, size) })
}

unsafe extern "C" fn set_constant_at_index(
    receiver: Id,
    _sel: Sel,
    value: *const u8,
    data_type: u64,
    index: u64,
) {
    // SAFETY: Metal reads one value of `data_type` from `value`.
    let Some(bytes) = (unsafe { constant_bytes(value, data_type) }) else {
        return;
    };
    // SAFETY: an initialized `MTLFunctionConstantValues`.
    let body = unsafe { object::body::<ConstantValues>(receiver) };
    if let Err(message) = body.values.set_index(index as u32, data_type, bytes) {
        report_library_error(message);
    }
}

unsafe extern "C" fn set_constant_with_name(
    receiver: Id,
    _sel: Sel,
    value: *const u8,
    data_type: u64,
    name: Id,
) {
    // SAFETY: Metal reads one value of `data_type` from `value`.
    let Some(bytes) = (unsafe { constant_bytes(value, data_type) }) else {
        return;
    };
    // SAFETY: a guest `NSString`.
    let name = unsafe { foundation::string::bytes(name) };
    let Ok(name) = std::str::from_utf8(&name) else {
        return;
    };
    // SAFETY: an initialized `MTLFunctionConstantValues`.
    let body = unsafe { object::body::<ConstantValues>(receiver) };
    if let Err(message) = body.values.set_name(name, data_type, bytes) {
        report_library_error(message);
    }
}

/// `-setConstantValues:type:withRange:`: an array of values for consecutive
/// indices. An `NSRange` is two words and arrives in two registers.
unsafe extern "C" fn set_constants_in_range(
    receiver: Id,
    sel: Sel,
    values: *const u8,
    data_type: u64,
    location: u64,
    length: u64,
) {
    let Some(size) = metal_translate::metal_data_type_size(data_type) else {
        report_library_error(format!(
            "MTLFunctionConstantValues was given MTLDataType {data_type}, which no function constant has"
        ));
        return;
    };
    // A C array of a three-lane vector is laid out at four lanes, like every
    // Metal vector type; each size here rounds up to the type's stride.
    let stride = size.next_power_of_two();
    for offset in 0..length {
        // SAFETY: the guest's array holds `length` values of `data_type`.
        let value = unsafe { values.add(offset as usize * stride) };
        // SAFETY: as above.
        unsafe { set_constant_at_index(receiver, sel, value, data_type, location + offset) };
    }
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
    if std::env::var_os("ROSETTA3_TRACE_METAL").is_none() {
        return;
    }
    let message = message.to_string();
    let mut seen = REPORTED_LIBRARY_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if seen.insert(message.clone()) {
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

fn dump_source_library(source: &str) {
    let Some(directory) = std::env::var_os("ROSETTA3_MSL_OUT") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let name = format!("msl-{:016x}.metal", fnv1a64(source.as_bytes()));
    let _ = std::fs::write(directory.join(name), source);
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
