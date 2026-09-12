//! Metal.framework, backed by Vulkan.
//!
//! The device boundary, shader libraries and the object graph a game holds
//! live here; the Vulkan objects underneath them live in `gpu`, and the
//! resource, pipeline, encoding and presentation halves of the API each have
//! their own module.
//!
//! The Objective-C entry points themselves are grouped by the class they
//! belong to: [`device`], [`library`], [`constants`], and the dynamic
//! symbols in [`exports`].

#![allow(non_snake_case)]

mod capture;
mod completion;
mod constants;
mod device;
mod encoder;
mod exports;
mod gpu;
mod heap;
mod library;
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

use constants::*;
use device::*;
use gpu::Gpu;
use library::*;
use object::methods;

pub use exports::{
    CGDirectDisplayCopyCurrentMetalDevice, MTLCopyAllDevices, MTLCreateSystemDefaultDevice, NAMES,
    data_symbols, symbol,
};

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

#[cfg(test)]
mod tests;
