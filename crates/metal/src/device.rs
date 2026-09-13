//! `MTLDevice`: the entry points a guest makes everything else through.

use super::*;

pub(super) unsafe extern "C" fn new_command_queue(receiver: Id, _sel: Sel) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    encoder::new_command_queue(gpu, receiver)
}

pub(super) unsafe extern "C" fn new_command_queue_with_count(
    receiver: Id,
    sel: Sel,
    _count: u64,
) -> Id {
    // SAFETY: forwards to the unbounded form; nothing here bounds the number
    // of command buffers in flight.
    unsafe { new_command_queue(receiver, sel) }
}

pub(super) unsafe extern "C" fn new_buffer(
    receiver: Id,
    _sel: Sel,
    length: usize,
    options: u64,
) -> Id {
    let Some(gpu) = gpu_of(receiver) else {
        return std::ptr::null_mut();
    };
    resource::new_buffer(gpu, length, options)
}

pub(super) unsafe extern "C" fn new_buffer_with_bytes(
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

pub(super) unsafe extern "C" fn new_texture(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
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

pub(super) unsafe extern "C" fn new_sampler_state(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
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

pub(super) unsafe extern "C" fn new_depth_stencil_state(
    _receiver: Id,
    _sel: Sel,
    descriptor: Id,
) -> Id {
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `MTLDepthStencilDescriptor` is this crate's class.
    let state = *unsafe { object::body::<resource::DepthStencilState>(descriptor) };
    resource::new_depth_stencil_state(state)
}

pub(super) unsafe extern "C" fn new_compute_pipeline(
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

pub(super) unsafe extern "C" fn new_render_pipeline(
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
pub(super) unsafe fn artifact_of(function: Id) -> Option<&'static metal_translate::EntryArtifact> {
    if function.is_null() || !object::is_kind(function, c"R3MTLFunction") {
        return None;
    }
    // SAFETY: the caller guarantees the class.
    let entry = unsafe { object::body::<Function>(function) };
    // SAFETY: a function holds the library which vended it, and a library
    // outlives every function it made.
    unsafe { entry.artifact() }
}

pub(super) unsafe extern "C" fn current_allocated_size(_receiver: Id, _sel: Sel) -> u64 {
    ALLOCATED_SIZE.load(Ordering::Relaxed)
}

pub(super) unsafe extern "C" fn name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `R3MTLDevice`.
    unsafe { object::body::<Device>(receiver) }.name
}

pub(super) unsafe extern "C" fn registry_id(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: as above.
    unsafe { object::body::<Device>(receiver) }.ordinal
}

pub(super) unsafe extern "C" fn no(_receiver: Id, _sel: Sel) -> bool {
    false
}

pub(super) unsafe extern "C" fn recommended_working_set(_receiver: Id, _sel: Sel) -> u64 {
    1024 * 1024 * 1024
}

pub(super) unsafe extern "C" fn max_buffer_length(receiver: Id, _sel: Sel) -> u64 {
    gpu_of(receiver).map_or(256 << 20, |gpu| {
        u64::from(gpu.limits.max_storage_buffer_range)
    })
}

pub(super) unsafe extern "C" fn max_threadgroup_memory(receiver: Id, _sel: Sel) -> u64 {
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
pub(super) unsafe extern "C" fn supports_family(_receiver: Id, _sel: Sel, family: u64) -> bool {
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
pub(super) unsafe extern "C" fn supports_feature_set(
    _receiver: Id,
    _sel: Sel,
    feature_set: u64,
) -> bool {
    // MTLFeatureSet_macOS_GPUFamily1_v1 through _GPUFamily2_v1.
    matches!(feature_set, 10000..=10005)
}

pub(super) unsafe extern "C" fn yes(_receiver: Id, _sel: Sel) -> bool {
    true
}

/// `-supportsBCTextureCompression`, which the pixel-format table needs in
/// order to answer a BC format at all.
pub(super) unsafe extern "C" fn supports_bc(receiver: Id, _sel: Sel) -> bool {
    gpu_of(receiver).is_some()
}

/// `-argumentBuffersSupport`: `MTLArgumentBuffersTier1`, which is the tier
/// whose argument buffers this runtime could lower to descriptor sets.
pub(super) unsafe extern "C" fn argument_buffers_tier(_receiver: Id, _sel: Sel) -> u64 {
    0
}

pub(super) unsafe extern "C" fn supports_sample_count(receiver: Id, _sel: Sel, count: u64) -> bool {
    let Some(gpu) = gpu_of(receiver) else {
        return false;
    };
    let Some(samples) = gpu::sample_count(count) else {
        return false;
    };
    gpu.limits.framebuffer_color_sample_counts.contains(samples)
}
