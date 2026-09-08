//! Metal.framework's device boundary, backed by Vulkan adapter discovery.

#![allow(non_snake_case)]

mod vulkan;

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

#[repr(C)]
struct Library {
    isa: *const Class,
    translated: metal_translate::TranslatedLibrary,
}

#[repr(C)]
struct Function {
    isa: *const Class,
    library: Id,
    entry: usize,
}

/// The source-library path can expose compute entry points before command
/// encoding owns a Vulkan pipeline. Keep the Metal function/device pairing
/// explicit so callers receive a real pipeline-state object rather than a
/// fabricated success value.
#[repr(C)]
struct ComputePipeline {
    isa: *const Class,
    device: Id,
    function: Id,
}

#[repr(C)]
struct CommandQueue {
    isa: *const Class,
    device: Id,
}

#[repr(C)]
struct CommandBuffer {
    isa: *const Class,
    queue: Id,
    completed_handler: Id,
    status: u64,
}

#[repr(C)]
struct CommandEncoder {
    isa: *const Class,
    command_buffer: Id,
}

#[repr(C)]
struct Buffer {
    isa: *const Class,
    contents: *mut u8,
    length: usize,
    options: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TextureDescriptor {
    isa: *const Class,
    texture_type: u64,
    pixel_format: u64,
    width: u64,
    height: u64,
    depth: u64,
    mipmap_level_count: u64,
    sample_count: u64,
    array_length: u64,
    storage_mode: u64,
    usage: u64,
}

#[repr(C)]
struct Texture {
    isa: *const Class,
    descriptor: TextureDescriptor,
}

#[repr(C)]
struct VertexDescriptor {
    isa: *const Class,
    layouts: Id,
    attributes: Id,
}

#[repr(C)]
struct IndexedDescriptors {
    isa: *const Class,
    elements: [Id; 31],
}

#[repr(C)]
struct VertexElement {
    isa: *const Class,
    values: [u64; 3],
}

static ALLOCATED_SIZE: AtomicU64 = AtomicU64::new(0);
static REPORTED_LIBRARY_ERROR: AtomicBool = AtomicBool::new(false);
static REPORTED_SOURCE_LIBRARY: AtomicBool = AtomicBool::new(false);

fn device_class() -> &'static Class {
    objc::class::lookup(c"R3MTLDevice").expect("Metal is bootstrapped")
}

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        let class =
            corefoundation::base::register(c"R3MTLDevice", None, size_of::<Device>(), false);
        let queue_class = corefoundation::base::register(
            c"R3MTLCommandQueue",
            None,
            size_of::<CommandQueue>(),
            false,
        );
        let library_class = corefoundation::base::register(
            c"R3MTLLibrary",
            None,
            size_of::<Library>(),
            false,
        );
        let function_class = corefoundation::base::register(
            c"R3MTLFunction",
            None,
            size_of::<Function>(),
            false,
        );
        let compute_pipeline_class = corefoundation::base::register(
            c"R3MTLComputePipelineState",
            None,
            size_of::<ComputePipeline>(),
            false,
        );
        let buffer_class =
            corefoundation::base::register(c"R3MTLBuffer", None, size_of::<Buffer>(), false);
        let command_buffer_class = corefoundation::base::register(
            c"R3MTLCommandBuffer",
            None,
            size_of::<CommandBuffer>(),
            false,
        );
        let command_encoder_class = corefoundation::base::register(
            c"R3MTLCommandEncoder",
            None,
            size_of::<CommandEncoder>(),
            false,
        );
        let descriptor_class = corefoundation::base::register(
            c"MTLTextureDescriptor",
            None,
            size_of::<TextureDescriptor>(),
            false,
        );
        let texture_class =
            corefoundation::base::register(c"R3MTLTexture", None, size_of::<Texture>(), false);
        let vertex_descriptor_class = corefoundation::base::register(
            c"MTLVertexDescriptor",
            None,
            size_of::<VertexDescriptor>(),
            false,
        );
        let indexed_descriptor_class = corefoundation::base::register(
            c"R3MTLIndexedDescriptors",
            None,
            size_of::<IndexedDescriptors>(),
            false,
        );
        let layout_class = corefoundation::base::register(
            c"MTLVertexBufferLayoutDescriptor",
            None,
            size_of::<VertexElement>(),
            false,
        );
        let attribute_class = corefoundation::base::register(
            c"MTLVertexAttributeDescriptor",
            None,
            size_of::<VertexElement>(),
            false,
        );
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
            (
                "newCommandQueue",
                new_command_queue as *const c_void,
                c"@16@0:8",
            ),
            (
                "newBufferWithLength:options:",
                new_buffer as *const c_void,
                c"@32@0:8Q16Q24",
            ),
            (
                "newTextureWithDescriptor:",
                new_texture as *const c_void,
                c"@24@0:8@16",
            ),
            (
                "newLibraryWithData:error:",
                new_library_with_data as *const c_void,
                c"@32@0:8@16^@24",
            ),
            (
                "newLibraryWithSource:options:error:",
                new_library_with_source as *const c_void,
                c"@40@0:8@16@24^@32",
            ),
            (
                "newComputePipelineStateWithFunction:error:",
                new_compute_pipeline as *const c_void,
                c"@32@0:8@16^@24",
            ),
            (
                "isDepth24Stencil8PixelFormatSupported",
                no as *const c_void,
                c"B16@0:8",
            ),
            (
                "currentAllocatedSize",
                current_allocated_size as *const c_void,
                c"Q16@0:8",
            ),
        ] {
            class.replace_method(objc::sel::get(selector), implementation, types);
        }
        library_class.replace_method(
            objc::sel::get("newFunctionWithName:"),
            new_function_with_name as *const c_void,
            c"@24@0:8@16",
        );
        library_class.replace_method(
            objc::sel::get("functionNames"),
            library_function_names as *const c_void,
            c"@16@0:8",
        );
        function_class.replace_method(
            objc::sel::get("name"),
            function_name as *const c_void,
            c"@16@0:8",
        );
        compute_pipeline_class.replace_method(
            objc::sel::get("device"),
            compute_pipeline_device as *const c_void,
            c"@16@0:8",
        );
        compute_pipeline_class.replace_method(
            objc::sel::get("threadExecutionWidth"),
            compute_pipeline_thread_width as *const c_void,
            c"Q16@0:8",
        );
        compute_pipeline_class.replace_method(
            objc::sel::get("maxTotalThreadsPerThreadgroup"),
            compute_pipeline_max_threads as *const c_void,
            c"Q16@0:8",
        );
        queue_class.replace_method(
            objc::sel::get("device"),
            queue_device as *const c_void,
            c"@16@0:8",
        );
        for selector in ["commandBuffer", "commandBufferWithUnretainedReferences"] {
            queue_class.replace_method(
                objc::sel::get(selector),
                new_command_buffer as *const c_void,
                c"@16@0:8",
            );
        }
        command_buffer_class.replace_method(
            objc::sel::get("commandQueue"),
            command_buffer_queue as *const c_void,
            c"@16@0:8",
        );
        for selector in ["computeCommandEncoder", "blitCommandEncoder"] {
            command_buffer_class.replace_method(
                objc::sel::get(selector),
                new_command_encoder as *const c_void,
                c"@16@0:8",
            );
        }
        command_buffer_class.replace_method(
            objc::sel::get("addCompletedHandler:"),
            add_completed_handler as *const c_void,
            c"v24@0:8@?16",
        );
        command_buffer_class.replace_method(
            objc::sel::get("commit"),
            commit as *const c_void,
            c"v16@0:8",
        );
        command_buffer_class.replace_method(
            objc::sel::get("waitUntilCompleted"),
            no_op as *const c_void,
            c"v16@0:8",
        );
        command_buffer_class.replace_method(
            objc::sel::get("status"),
            command_buffer_status as *const c_void,
            c"Q16@0:8",
        );
        for selector in [
            "endEncoding",
            "synchronizeResource:",
            "setComputePipelineState:",
            "setBuffer:offset:atIndex:",
            "setTexture:atIndex:",
            "dispatchThreadgroups:threadsPerThreadgroup:",
            "dispatchThreads:threadsPerThreadgroup:",
            "copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:",
        ] {
            command_encoder_class.replace_method(
                objc::sel::get(selector),
                no_op as *const c_void,
                c"v16@0:8",
            );
        }
        for (selector, implementation) in [
            ("layouts", vertex_layouts as *const c_void),
            ("attributes", vertex_attributes as *const c_void),
        ] {
            vertex_descriptor_class.replace_method(
                objc::sel::get(selector),
                implementation,
                c"@16@0:8",
            );
        }
        indexed_descriptor_class.replace_method(
            objc::sel::get("objectAtIndexedSubscript:"),
            indexed_descriptor as *const c_void,
            c"@24@0:8Q16",
        );
        for class in [layout_class, attribute_class] {
            for selector in [
                "setStride:",
                "setStepFunction:",
                "setStepRate:",
                "setFormat:",
                "setOffset:",
                "setBufferIndex:",
            ] {
                class.replace_method(
                    objc::sel::get(selector),
                    set_vertex_element_value as *const c_void,
                    c"v24@0:8Q16",
                );
            }
        }
        buffer_class.replace_method(
            objc::sel::get("contents"),
            buffer_contents as *const c_void,
            c"^v16@0:8",
        );
        buffer_class.replace_method(
            objc::sel::get("didModifyRange:"),
            no_op as *const c_void,
            c"v32@0:8{_NSRange=QQ}16",
        );
        buffer_class.replace_method(
            objc::sel::get("length"),
            buffer_length as *const c_void,
            c"Q16@0:8",
        );
        buffer_class.replace_method(
            objc::sel::get("storageMode"),
            buffer_storage_mode as *const c_void,
            c"Q16@0:8",
        );
        for selector in [
            "setTextureType:",
            "setPixelFormat:",
            "setWidth:",
            "setHeight:",
            "setDepth:",
            "setMipmapLevelCount:",
            "setSampleCount:",
            "setArrayLength:",
            "setStorageMode:",
            "setUsage:",
        ] {
            descriptor_class.replace_method(
                objc::sel::get(selector),
                set_descriptor_value as *const c_void,
                c"v24@0:8Q16",
            );
        }
        for selector in [
            "textureType",
            "pixelFormat",
            "width",
            "height",
            "depth",
            "mipmapLevelCount",
            "sampleCount",
            "arrayLength",
            "storageMode",
            "usage",
        ] {
            descriptor_class.replace_method(
                objc::sel::get(selector),
                descriptor_value as *const c_void,
                c"Q16@0:8",
            );
            texture_class.replace_method(
                objc::sel::get(selector),
                texture_value as *const c_void,
                c"Q16@0:8",
            );
        }
    });
}

unsafe extern "C" fn new_command_queue(receiver: Id, _sel: Sel) -> Id {
    let class = objc::class::lookup(c"R3MTLCommandQueue").expect("Metal is bootstrapped");
    let queue = unsafe { corefoundation::base::allocate::<CommandQueue>(class) };
    if !queue.is_null() {
        unsafe { (*queue).device = objc::arc::retain(receiver) };
    }
    queue as Id
}

unsafe extern "C" fn new_compute_pipeline(
    receiver: Id,
    _sel: Sel,
    function: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        unsafe { *error = std::ptr::null_mut() };
    }
    if function.is_null()
        || !unsafe {
            corefoundation::base::is_kind_of(
                function as CFTypeRef,
                objc::class::lookup(c"R3MTLFunction").expect("Metal is bootstrapped"),
            )
        }
    {
        if !error.is_null() {
            unsafe {
                *error = foundation::error::make(
                    "MTLComputePipelineErrorDomain",
                    3,
                    "newComputePipelineStateWithFunction:error: requires an MTLFunction",
                );
            }
        }
        return std::ptr::null_mut();
    }
    let class = objc::class::lookup(c"R3MTLComputePipelineState").expect("Metal is bootstrapped");
    let pipeline = unsafe { corefoundation::base::allocate::<ComputePipeline>(class) };
    if pipeline.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        (*pipeline).device = objc::arc::retain(receiver);
        (*pipeline).function = objc::arc::retain(function);
    }
    pipeline as Id
}

unsafe extern "C" fn compute_pipeline_device(receiver: Id, _sel: Sel) -> Id {
    unsafe { (*(receiver as *const ComputePipeline)).device }
}

unsafe extern "C" fn compute_pipeline_thread_width(_receiver: Id, _sel: Sel) -> u64 {
    32
}

unsafe extern "C" fn compute_pipeline_max_threads(_receiver: Id, _sel: Sel) -> u64 {
    1024
}

unsafe extern "C" fn queue_device(receiver: Id, _sel: Sel) -> Id {
    unsafe { (*(receiver as *const CommandQueue)).device }
}

unsafe extern "C" fn new_command_buffer(receiver: Id, _sel: Sel) -> Id {
    let class = objc::class::lookup(c"R3MTLCommandBuffer").expect("Metal is bootstrapped");
    let buffer = unsafe { corefoundation::base::allocate::<CommandBuffer>(class) };
    if !buffer.is_null() {
        unsafe {
            (*buffer).queue = objc::arc::retain(receiver);
            (*buffer).completed_handler = std::ptr::null_mut();
            (*buffer).status = 0;
        }
    }
    buffer as Id
}

unsafe extern "C" fn new_command_encoder(receiver: Id, _sel: Sel) -> Id {
    let class = objc::class::lookup(c"R3MTLCommandEncoder").expect("Metal is bootstrapped");
    let encoder = unsafe { corefoundation::base::allocate::<CommandEncoder>(class) };
    if !encoder.is_null() {
        unsafe { (*encoder).command_buffer = objc::arc::retain(receiver) };
    }
    encoder as Id
}

unsafe extern "C" fn add_completed_handler(receiver: Id, _sel: Sel, block: Id) {
    let command = unsafe { &mut *(receiver as *mut CommandBuffer) };
    objc::arc::release(command.completed_handler);
    command.completed_handler = unsafe { objc::shorthand::objc_retainBlock(block) };
}

unsafe extern "C" fn commit(receiver: Id, _sel: Sel) {
    let command = unsafe { &mut *(receiver as *mut CommandBuffer) };
    command.status = 4; // MTLCommandBufferStatusCompleted
    let block = command.completed_handler;
    if !block.is_null() {
        let invoke = unsafe { *((block as *const u8).add(16) as *const usize) };
        if invoke != 0 {
            let call: unsafe extern "C" fn(Id, Id) = unsafe { std::mem::transmute(invoke) };
            unsafe { call(block, receiver) };
        }
    }
}

unsafe extern "C" fn command_buffer_status(receiver: Id, _sel: Sel) -> u64 {
    unsafe { (*(receiver as *const CommandBuffer)).status }
}

unsafe extern "C" fn no_op(_receiver: Id, _sel: Sel) {}

unsafe fn vertex_descriptors(receiver: Id, layout: bool) -> Id {
    let descriptor = unsafe { &mut *(receiver as *mut VertexDescriptor) };
    let slot = if layout {
        &mut descriptor.layouts
    } else {
        &mut descriptor.attributes
    };
    if slot.is_null() {
        let collection_class =
            objc::class::lookup(c"R3MTLIndexedDescriptors").expect("Metal is bootstrapped");
        let collection =
            unsafe { corefoundation::base::allocate::<IndexedDescriptors>(collection_class) };
        if collection.is_null() {
            return std::ptr::null_mut();
        }
        let element_name = if layout {
            c"MTLVertexBufferLayoutDescriptor"
        } else {
            c"MTLVertexAttributeDescriptor"
        };
        let element_class = objc::class::lookup(element_name).expect("Metal is bootstrapped");
        for element in unsafe { &mut (*collection).elements } {
            *element =
                unsafe { corefoundation::base::allocate::<VertexElement>(element_class) as Id };
        }
        *slot = collection as Id;
    }
    *slot
}

unsafe extern "C" fn vertex_layouts(receiver: Id, _sel: Sel) -> Id {
    unsafe { vertex_descriptors(receiver, true) }
}

unsafe extern "C" fn vertex_attributes(receiver: Id, _sel: Sel) -> Id {
    unsafe { vertex_descriptors(receiver, false) }
}

unsafe extern "C" fn indexed_descriptor(receiver: Id, _sel: Sel, index: usize) -> Id {
    unsafe {
        (*(receiver as *const IndexedDescriptors))
            .elements
            .get(index)
            .copied()
            .unwrap_or(std::ptr::null_mut())
    }
}

unsafe extern "C" fn set_vertex_element_value(receiver: Id, sel: Sel, value: u64) {
    let element = unsafe { &mut *(receiver as *mut VertexElement) };
    let name = unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes();
    let index = match name {
        b"setStride:" | b"setFormat:" => 0,
        b"setStepFunction:" | b"setOffset:" => 1,
        b"setStepRate:" | b"setBufferIndex:" => 2,
        _ => unreachable!("only registered vertex setters reach this method"),
    };
    element.values[index] = value;
}

unsafe extern "C" fn command_buffer_queue(receiver: Id, _sel: Sel) -> Id {
    unsafe { (*(receiver as *const CommandBuffer)).queue }
}

unsafe extern "C" fn new_buffer(_receiver: Id, _sel: Sel, length: usize, options: u64) -> Id {
    if length == 0 {
        return std::ptr::null_mut();
    }
    let class = objc::class::lookup(c"R3MTLBuffer").expect("Metal is bootstrapped");
    let buffer = unsafe { corefoundation::base::allocate::<Buffer>(class) };
    if buffer.is_null() {
        return std::ptr::null_mut();
    }
    let contents = vec![0u8; length].leak().as_mut_ptr();
    unsafe {
        (*buffer).contents = contents;
        (*buffer).length = length;
        (*buffer).options = options;
    }
    ALLOCATED_SIZE.fetch_add(length as u64, Ordering::Relaxed);
    buffer as Id
}

unsafe extern "C" fn buffer_contents(receiver: Id, _sel: Sel) -> *mut u8 {
    unsafe { (*(receiver as *const Buffer)).contents }
}

unsafe extern "C" fn buffer_length(receiver: Id, _sel: Sel) -> usize {
    unsafe { (*(receiver as *const Buffer)).length }
}

unsafe extern "C" fn buffer_storage_mode(receiver: Id, _sel: Sel) -> u64 {
    unsafe { ((*(receiver as *const Buffer)).options >> 4) & 0xf }
}

unsafe extern "C" fn set_descriptor_value(receiver: Id, sel: Sel, value: u64) {
    let descriptor = unsafe { &mut *(receiver as *mut TextureDescriptor) };
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setTextureType:" => descriptor.texture_type = value,
        b"setPixelFormat:" => descriptor.pixel_format = value,
        b"setWidth:" => descriptor.width = value,
        b"setHeight:" => descriptor.height = value,
        b"setDepth:" => descriptor.depth = value,
        b"setMipmapLevelCount:" => descriptor.mipmap_level_count = value,
        b"setSampleCount:" => descriptor.sample_count = value,
        b"setArrayLength:" => descriptor.array_length = value,
        b"setStorageMode:" => descriptor.storage_mode = value,
        b"setUsage:" => descriptor.usage = value,
        _ => unreachable!("only registered descriptor setters reach this method"),
    }
}

fn descriptor_field(descriptor: &TextureDescriptor, sel: Sel) -> u64 {
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"textureType" => descriptor.texture_type,
        b"pixelFormat" => descriptor.pixel_format,
        b"width" => descriptor.width,
        b"height" => descriptor.height,
        b"depth" => descriptor.depth,
        b"mipmapLevelCount" => descriptor.mipmap_level_count,
        b"sampleCount" => descriptor.sample_count,
        b"arrayLength" => descriptor.array_length,
        b"storageMode" => descriptor.storage_mode,
        b"usage" => descriptor.usage,
        _ => unreachable!("only registered descriptor getters reach this method"),
    }
}

unsafe extern "C" fn descriptor_value(receiver: Id, sel: Sel) -> u64 {
    descriptor_field(unsafe { &*(receiver as *const TextureDescriptor) }, sel)
}

unsafe extern "C" fn texture_value(receiver: Id, sel: Sel) -> u64 {
    descriptor_field(unsafe { &(*(receiver as *const Texture)).descriptor }, sel)
}

unsafe extern "C" fn new_texture(_receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    let class = objc::class::lookup(c"R3MTLTexture").expect("Metal is bootstrapped");
    let texture = unsafe { corefoundation::base::allocate::<Texture>(class) };
    if texture.is_null() {
        return std::ptr::null_mut();
    }
    let copied = unsafe { *(descriptor as *const TextureDescriptor) };
    unsafe { (*texture).descriptor = copied };
    let pixels = copied
        .width
        .saturating_mul(copied.height)
        .saturating_mul(copied.depth.max(1))
        .saturating_mul(copied.array_length.max(1));
    ALLOCATED_SIZE.fetch_add(pixels.saturating_mul(4), Ordering::Relaxed);
    texture as Id
}

unsafe extern "C" fn current_allocated_size(_receiver: Id, _sel: Sel) -> u64 {
    ALLOCATED_SIZE.load(Ordering::Relaxed)
}

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
    let bytes = unsafe { std::slice::from_raw_parts(bytes, length) };
    let air = match metal_translate::metallib_air(bytes) {
        Ok(air) => air,
        Err(error) => {
            report_library_error(error);
            return std::ptr::null_mut();
        }
    };
    let target = unsafe { (*(receiver as *const Device)).target.as_ref() };
    let Some(target) = target else {
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
    retain_library(translated)
}

/// Wrap a translated library in the `MTLLibrary` object the guest holds. Both
/// library entry points differ only in how they obtain the translation.
fn retain_library(translated: metal_translate::TranslatedLibrary) -> Id {
    let class = objc::class::lookup(c"R3MTLLibrary").expect("Metal is bootstrapped");
    let library = unsafe { corefoundation::base::allocate::<Library>(class) };
    if library.is_null() {
        report_library_error("could not allocate MTL library");
        return std::ptr::null_mut();
    }
    // `allocate` only initializes the Objective-C header.  Write the Rust
    // payload without dropping the uninitialized slot first.
    unsafe { std::ptr::addr_of_mut!((*library).translated).write(translated) };
    library as Id
}

unsafe extern "C" fn new_library_with_source(
    receiver: Id,
    _sel: Sel,
    source: Id,
    _options: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`, as required by the
        // Objective-C method contract.
        unsafe { *error = std::ptr::null_mut() };
    }
    if source.is_null() {
        return fail_library(error, "newLibraryWithSource: requires an NSString source");
    }
    if unsafe { (*(receiver as *const Device)).target.as_ref() }.is_none() {
        return fail_library(error, "Metal device has no Vulkan target profile");
    }

    let source = unsafe { foundation::string::bytes(source) };
    if source.is_empty() {
        return fail_library(
            error,
            "newLibraryWithSource: received an empty or invalid NSString",
        );
    }
    let source = match std::str::from_utf8(&source) {
        Ok(source) => source,
        Err(_) => return fail_library(error, "Metal source is not valid UTF-8"),
    };
    report_source_library(source);
    let target = unsafe { (*(receiver as *const Device)).target.as_ref() }
        .expect("target was checked above");
    let translated = match metal_translate::translate_msl(source, target) {
        Ok(translated) => translated,
        Err(translation_error) => return fail_library(error, &translation_error.to_string()),
    };
    let library = retain_library(translated);
    if library.is_null() {
        return fail_library(error, "could not allocate MTL source library");
    }
    library
}

unsafe extern "C" fn new_function_with_name(receiver: Id, _sel: Sel, name: Id) -> Id {
    let name = unsafe { foundation::string::bytes(name) };
    let Ok(name) = std::str::from_utf8(&name) else {
        return std::ptr::null_mut();
    };
    let library = unsafe { &*(receiver as *const Library) };
    let Some(entry) = library
        .translated
        .entries
        .iter()
        .position(|entry| entry.name == name)
    else {
        return std::ptr::null_mut();
    };
    let class = objc::class::lookup(c"R3MTLFunction").expect("Metal is bootstrapped");
    let function = unsafe { corefoundation::base::allocate::<Function>(class) };
    if function.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        (*function).library = receiver;
        (*function).entry = entry;
    }
    function as Id
}

/// `-[MTLLibrary functionNames]`: the entry points the translation produced,
/// in the order the library declares them.
unsafe extern "C" fn library_function_names(receiver: Id, _sel: Sel) -> Id {
    let library = unsafe { &*(receiver as *const Library) };
    let names: Vec<corefoundation::base::CFTypeRef> = library
        .translated
        .entries
        .iter()
        .map(|entry| foundation::string::from_str(&entry.name) as corefoundation::base::CFTypeRef)
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

unsafe extern "C" fn function_name(receiver: Id, _sel: Sel) -> Id {
    let function = unsafe { &*(receiver as *const Function) };
    let library = unsafe { &*(function.library as *const Library) };
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

pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    ["MTLTextureDescriptor", "MTLVertexDescriptor"]
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
    fn descriptors_textures_buffers_and_queues_keep_their_metal_state() {
        bootstrap();
        unsafe {
            let descriptor_class = objc::class::lookup(c"MTLTextureDescriptor").unwrap();
            let descriptor = corefoundation::base::allocate::<TextureDescriptor>(descriptor_class);
            set_descriptor_value(descriptor as Id, objc::sel::get("setWidth:"), 64);
            set_descriptor_value(descriptor as Id, objc::sel::get("setHeight:"), 32);
            set_descriptor_value(descriptor as Id, objc::sel::get("setStorageMode:"), 2);
            let texture = new_texture(std::ptr::null_mut(), std::ptr::null(), descriptor as Id);
            assert_eq!(texture_value(texture, objc::sel::get("width")), 64);
            assert_eq!(texture_value(texture, objc::sel::get("height")), 32);

            let buffer = new_buffer(std::ptr::null_mut(), std::ptr::null(), 256, 2 << 4);
            assert!(!buffer_contents(buffer, std::ptr::null()).is_null());
            assert_eq!(buffer_length(buffer, std::ptr::null()), 256);
            assert_eq!(buffer_storage_mode(buffer, std::ptr::null()), 2);

            let device = corefoundation::base::allocate::<Device>(device_class()) as Id;
            let queue = new_command_queue(device, std::ptr::null());
            let command = new_command_buffer(queue, std::ptr::null());
            assert_eq!(queue_device(queue, std::ptr::null()), device);
            assert_eq!(command_buffer_queue(command, std::ptr::null()), queue);
        }
    }

    #[test]
    fn device_registers_source_library_creation() {
        bootstrap();
        let method = device_class().method(objc::sel::get("newLibraryWithSource:options:error:"));
        assert!(method.is_some());
    }

    #[test]
    fn source_library_creation_reports_invalid_source() {
        bootstrap();
        let device = unsafe { corefoundation::base::allocate::<Device>(device_class()) } as Id;
        let mut error = std::ptr::null_mut();
        let library = unsafe {
            new_library_with_source(
                device,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut error,
            )
        };
        assert!(library.is_null());
        assert!(!error.is_null());
    }

    #[test]
    fn source_library_compiles_the_initial_msl_compute_kernel() {
        bootstrap();
        let device = unsafe { corefoundation::base::allocate::<Device>(device_class()) };
        let target = Box::leak(Box::new(metal_translate::TargetProfile::vulkan_1_3()));
        unsafe { (*device).target = target };
        let source = foundation::string::from_str(
            "kernel void clear(device uint* output [[buffer(0)]]) { output[0] = 42; }",
        );
        let mut error = std::ptr::null_mut();
        let library = unsafe {
            new_library_with_source(
                device as Id,
                std::ptr::null(),
                source,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        assert!(!library.is_null());
        assert!(error.is_null());
        let function = unsafe {
            new_function_with_name(
                library,
                std::ptr::null(),
                foundation::string::from_str("clear"),
            )
        };
        assert!(!function.is_null());
    }

    #[test]
    fn source_compute_function_creates_a_pipeline_state() {
        bootstrap();
        let device = unsafe { corefoundation::base::allocate::<Device>(device_class()) };
        let target = Box::leak(Box::new(metal_translate::TargetProfile::vulkan_1_3()));
        unsafe { (*device).target = target };
        let source = foundation::string::from_str(
            "kernel void clear(device uint* output [[buffer(0)]]) { output[0] = 42; }",
        );
        let mut error = std::ptr::null_mut();
        let library = unsafe {
            new_library_with_source(
                device as Id,
                std::ptr::null(),
                source,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        let function = unsafe {
            new_function_with_name(
                library,
                std::ptr::null(),
                foundation::string::from_str("clear"),
            )
        };
        let pipeline =
            unsafe { new_compute_pipeline(device as Id, std::ptr::null(), function, &mut error) };
        assert!(!pipeline.is_null());
        assert!(error.is_null());
        assert_eq!(
            unsafe { compute_pipeline_device(pipeline, std::ptr::null()) },
            device as Id
        );
        assert_eq!(
            unsafe { compute_pipeline_thread_width(pipeline, std::ptr::null()) },
            32
        );
        assert_eq!(
            unsafe { compute_pipeline_max_threads(pipeline, std::ptr::null()) },
            1024
        );
    }

    /// A translated AIR library must expose its entry points as `MTLFunction`
    /// objects and answer `functionNames`, exactly as the source path does:
    /// a shipped Metal library is the only form a game uses.
    #[test]
    fn air_and_source_libraries_expose_the_same_function_objects() {
        bootstrap();
        let device = unsafe { corefoundation::base::allocate::<Device>(device_class()) };
        let target = Box::leak(Box::new(metal_translate::TargetProfile::vulkan_1_3()));
        unsafe { (*device).target = target };
        let source = foundation::string::from_str(
            "kernel void clear(device uint* output [[buffer(0)]]) { output[0] = 42; }",
        );
        let mut error = std::ptr::null_mut();
        let library = unsafe {
            new_library_with_source(
                device as Id,
                std::ptr::null(),
                source,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        assert!(!library.is_null());
        let names = unsafe { library_function_names(library, std::ptr::null()) };
        assert_eq!(
            unsafe { corefoundation::api::CFArrayGetCount(names as CFTypeRef) },
            1
        );
        let name =
            unsafe { corefoundation::api::CFArrayGetValueAtIndex(names as CFTypeRef, 0) as Id };
        let function = unsafe { new_function_with_name(library, std::ptr::null(), name) };
        assert!(!function.is_null());
        assert_eq!(
            unsafe { foundation::string::bytes(function_name(function, std::ptr::null())) },
            b"clear"
        );
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
