use super::*;

fn device_for_tests() -> Id {
    bootstrap();
    let target = Box::leak(Box::new(metal_translate::TargetProfile::vulkan_1_3()));
    object::make(
        c"R3MTLDevice",
        Device {
            ordinal: 1,
            name: foundation::string::from_str("Test Device"),
            target,
            gpu: None,
        },
    )
}

/// A device backed by a real Vulkan adapter, or `None` where there is none.
fn gpu_device() -> Option<Id> {
    bootstrap();
    // SAFETY: called through the same entry point a guest uses.
    let device = unsafe { MTLCreateSystemDefaultDevice() };
    (!device.is_null() && gpu_of(device).is_some()).then_some(device)
}

const SMOKE_KERNEL: &str =
    "kernel void clear(device uint* output [[buffer(0)]]) { output[0] = 42; }";

fn source_library(device: Id, source: &str) -> Id {
    let mut error = std::ptr::null_mut();
    // SAFETY: the same call the guest makes.
    unsafe {
        new_library_with_source(
            device,
            std::ptr::null(),
            foundation::string::from_str(source),
            std::ptr::null_mut(),
            &mut error,
        )
    }
}

#[test]
fn every_export_resolves_with_both_spellings() {
    for name in NAMES {
        assert_ne!(symbol(name), None);
        assert_eq!(symbol(&format!("_{name}")), symbol(name));
    }
}

#[test]
fn device_registers_the_metal_object_graph() {
    bootstrap();
    for (class, selector) in [
        (c"R3MTLDevice", "newLibraryWithSource:options:error:"),
        (
            c"R3MTLDevice",
            "newRenderPipelineStateWithDescriptor:error:",
        ),
        (
            c"R3MTLCommandEncoder",
            "drawPrimitives:vertexStart:vertexCount:instanceCount:",
        ),
        (c"MTLRenderPassDescriptor", "colorAttachments"),
        (c"MTLRenderPipelineDescriptor", "setVertexFunction:"),
        (c"MTLSamplerDescriptor", "setMinFilter:"),
        (c"MTLDepthStencilDescriptor", "setDepthWriteEnabled:"),
        (c"R3CAMetalDrawable", "texture"),
    ] {
        let class = objc::class::lookup(class).expect("Metal is bootstrapped");
        assert!(
            class.method(objc::sel::get(selector)).is_some(),
            "{selector} is not registered"
        );
    }
}

/// A device which answers "no" to every family and feature set reads as
/// unusable: a game asks before it decides which renderer to run.
#[test]
fn the_device_reports_the_mac_gpu_families() {
    bootstrap();
    // SAFETY: the same calls the guest makes; neither reads the receiver.
    unsafe {
        assert!(supports_family(
            std::ptr::null_mut(),
            std::ptr::null(),
            2002
        ));
        assert!(supports_family(
            std::ptr::null_mut(),
            std::ptr::null(),
            3001
        ));
        // An Apple silicon family promises tile memory and imageblocks.
        assert!(!supports_family(
            std::ptr::null_mut(),
            std::ptr::null(),
            1001
        ));
        assert!(supports_feature_set(
            std::ptr::null_mut(),
            std::ptr::null(),
            10000
        ));
        // An iOS feature set belongs to a different device.
        assert!(!supports_feature_set(
            std::ptr::null_mut(),
            std::ptr::null(),
            0
        ));
    }
}

#[test]
fn source_library_creation_reports_invalid_source() {
    let device = device_for_tests();
    let mut error = std::ptr::null_mut();
    // SAFETY: the same call the guest makes.
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
fn a_translated_library_exposes_its_entry_points() {
    let device = device_for_tests();
    let library = source_library(device, SMOKE_KERNEL);
    assert!(!library.is_null());
    // SAFETY: a library this test made.
    let names = unsafe { library_function_names(library, std::ptr::null()) };
    // SAFETY: the array `functionNames` just returned.
    assert_eq!(
        unsafe { corefoundation::api::CFArrayGetCount(names as CFTypeRef) },
        1
    );
    // SAFETY: as above.
    let name = unsafe { corefoundation::api::CFArrayGetValueAtIndex(names as CFTypeRef, 0) as Id };
    // SAFETY: a library and a name it just reported.
    let function = unsafe { new_function_with_name(library, std::ptr::null(), name) };
    assert!(!function.is_null());
    // SAFETY: a function this test made.
    assert_eq!(
        unsafe { foundation::string::bytes(function_name(function, std::ptr::null())) },
        b"clear"
    );
}

/// A device without a Vulkan device must refuse resource creation rather than
/// answer an object which cannot back any GPU work.
#[test]
fn a_device_without_vulkan_refuses_resources() {
    let device = device_for_tests();
    // SAFETY: the same calls the guest makes.
    unsafe {
        assert!(new_buffer(device, std::ptr::null(), 256, 0).is_null());
        assert!(new_command_queue(device, std::ptr::null()).is_null());
    }
}

/// The whole compute path on a real device: translate, create a pipeline,
/// encode a dispatch against a Vulkan buffer, and read the result back.
#[test]
fn a_translated_kernel_runs_on_the_gpu() {
    let Some(device) = gpu_device() else {
        return;
    };
    let library = source_library(device, SMOKE_KERNEL);
    assert!(!library.is_null(), "the smoke kernel must translate");
    // SAFETY: a library this test made.
    let function = unsafe {
        new_function_with_name(
            library,
            std::ptr::null(),
            foundation::string::from_str("clear"),
        )
    };
    assert!(!function.is_null());
    let mut error = std::ptr::null_mut();
    // SAFETY: a device and a function this test made.
    let pipeline = unsafe { new_compute_pipeline(device, std::ptr::null(), function, &mut error) };
    assert!(!pipeline.is_null(), "compute pipeline creation failed");
    assert!(error.is_null());

    // SAFETY: the same calls the guest makes.
    let (buffer, queue) = unsafe {
        (
            new_buffer(device, std::ptr::null(), 16, 0),
            new_command_queue(device, std::ptr::null()),
        )
    };
    assert!(!buffer.is_null() && !queue.is_null());
    // SAFETY: objects this test made, sent the selectors they implement.
    unsafe {
        let command = send(queue, "commandBuffer");
        let encoder = send(command, "computeCommandEncoder");
        send_with_object(encoder, "setComputePipelineState:", pipeline);
        set_buffer_at(encoder, buffer, 0, 0);
        dispatch(encoder, 1);
        send_void(encoder, "endEncoding");
        send_void(command, "commit");
        send_void(command, "waitUntilCompleted");

        let contents = object::body::<resource::Buffer>(buffer).inner.contents;
        assert_eq!(std::ptr::read(contents as *const u32), 42);
    }
}

// Small message-sending helpers: the tests drive the objects through the
// selectors a guest uses rather than through the Rust implementations, so a
// missing registration fails the test.

/// # Safety
///
/// `receiver` must implement `selector` as `id (id, SEL)`.
unsafe fn send(receiver: Id, selector: &str) -> Id {
    let sel = objc::sel::get(selector);
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(receiver) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the caller guarantees the signature.
    let call: unsafe extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel) }
}

/// # Safety
///
/// `receiver` must implement `selector` as `void (id, SEL)`.
unsafe fn send_void(receiver: Id, selector: &str) {
    let sel = objc::sel::get(selector);
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(receiver) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the caller guarantees the signature.
    let call: unsafe extern "C" fn(Id, Sel) =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel) };
}

/// # Safety
///
/// `receiver` must implement `selector` as `void (id, SEL, id)`.
unsafe fn send_with_object(receiver: Id, selector: &str, argument: Id) {
    let sel = objc::sel::get(selector);
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(receiver) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the caller guarantees the signature.
    let call: unsafe extern "C" fn(Id, Sel, Id) =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel, argument) };
}

/// # Safety
///
/// `encoder` must be an `R3MTLCommandEncoder`.
unsafe fn set_buffer_at(encoder: Id, buffer: Id, offset: u64, index: u64) {
    let sel = objc::sel::get("setBuffer:offset:atIndex:");
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(encoder) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    let call: unsafe extern "C" fn(Id, Sel, Id, u64, u64) =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(encoder, sel, buffer, offset, index) };
}

/// # Safety
///
/// `encoder` must be an `R3MTLCommandEncoder`.
unsafe fn dispatch(encoder: Id, groups: u64) {
    let sel = objc::sel::get("dispatchThreadgroups:threadsPerThreadgroup:");
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(encoder) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    let call: unsafe extern "C" fn(Id, Sel, encoder::MtlSize, encoder::MtlSize) =
        unsafe { std::mem::transmute(method.implementation()) };
    let size = encoder::MtlSize {
        width: groups,
        height: 1,
        depth: 1,
    };
    let one = encoder::MtlSize {
        width: 1,
        height: 1,
        depth: 1,
    };
    // SAFETY: as above.
    unsafe { call(encoder, sel, size, one) };
}

// ---------------------------------------------------------- render path

use metal_translate::ir;

/// A vertex/fragment pair built directly in the Metal IR.
///
/// The MSL frontend has no vertex path yet and the AIR fixtures in this tree
/// are compute kernels, so the render test states the program it draws with
/// rather than parsing one.
fn triangle_library() -> metal_translate::TranslatedLibrary {
    let float4 = ir::Type::Vector {
        element: Box::new(ir::Type::Float { bits: 32 }),
        lanes: 4,
    };
    let vertex = ir::Function {
        name: "triangle_vertex".to_string(),
        result: float4.clone(),
        parameters: vec![ir::Parameter {
            value: ir::ValueId(0),
            name: "position".to_string(),
            ty: float4.clone(),
        }],
        values: vec![float4.clone()],
        blocks: vec![ir::Block {
            id: ir::BlockId(0),
            name: "entry".to_string(),
            instructions: Vec::new(),
            terminator: ir::Terminator::Return(Some(ir::Operand::Value(ir::ValueId(0)))),
        }],
    };
    let fragment = ir::Function {
        name: "triangle_fragment".to_string(),
        result: float4.clone(),
        parameters: Vec::new(),
        values: Vec::new(),
        blocks: vec![ir::Block {
            id: ir::BlockId(0),
            name: "entry".to_string(),
            instructions: Vec::new(),
            terminator: ir::Terminator::Return(Some(ir::Operand::Composite {
                ty: float4.clone(),
                elements: vec![
                    ir::Operand::f32(1.0),
                    ir::Operand::f32(0.0),
                    ir::Operand::f32(0.0),
                    ir::Operand::f32(1.0),
                ],
            })),
        }],
    };
    let module = ir::Module {
        functions: vec![vertex, fragment],
        globals: Vec::new(),
        entries: vec![
            ir::EntryPoint {
                function: 0,
                stage: ir::Stage::Vertex,
                resources: Vec::new(),
                inputs: vec![ir::StageInput {
                    argument: 0,
                    binding: ir::StageBinding::Location {
                        location: 0,
                        interpolation: None,
                    },
                    name: Some("position".to_string()),
                }],
                outputs: vec![ir::StageOutput {
                    field: None,
                    binding: ir::StageBinding::Builtin(ir::Builtin::Position),
                    name: None,
                }],
            },
            ir::EntryPoint {
                function: 1,
                stage: ir::Stage::Fragment,
                resources: Vec::new(),
                inputs: Vec::new(),
                outputs: vec![ir::StageOutput {
                    field: None,
                    binding: ir::StageBinding::RenderTarget { index: 0 },
                    name: None,
                }],
            },
        ],
    };
    metal_translate::emit_spirv(&module, &metal_translate::TargetProfile::vulkan_1_3())
        .expect("the triangle program translates")
}

/// # Safety
///
/// `receiver` must implement `selector` as `void (id, SEL, NSUInteger)`.
unsafe fn send_number(receiver: Id, selector: &str, value: u64) {
    let sel = objc::sel::get(selector);
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(receiver) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the caller guarantees the signature.
    let call: unsafe extern "C" fn(Id, Sel, u64) =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel, value) };
}

/// # Safety
///
/// `receiver` must implement `objectAtIndexedSubscript:`.
unsafe fn element_at(receiver: Id, index: u64) -> Id {
    let sel = objc::sel::get("objectAtIndexedSubscript:");
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(receiver) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    let call: unsafe extern "C" fn(Id, Sel, u64) -> Id =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(receiver, sel, index) }
}

/// The whole render path on a real device: a translated vertex/fragment pair,
/// a render pipeline, a render pass with a clear, one draw, and a blit which
/// reads the result back.
#[test]
fn a_translated_triangle_draws_on_the_gpu() {
    let Some(device) = gpu_device() else {
        return;
    };
    const SIZE: u64 = 8;
    let library = retain_library(device, triangle_library());
    // SAFETY: a library this test made.
    let (vertex, fragment) = unsafe {
        (
            new_function_with_name(
                library,
                std::ptr::null(),
                foundation::string::from_str("triangle_vertex"),
            ),
            new_function_with_name(
                library,
                std::ptr::null(),
                foundation::string::from_str("triangle_fragment"),
            ),
        )
    };
    assert!(!vertex.is_null() && !fragment.is_null());

    // The vertex descriptor: one float4 stream at buffer index 0.
    let vertex_descriptor = object::make(
        c"MTLVertexDescriptor",
        resource::VertexDescriptor {
            layouts: [resource::VertexLayout::default(); resource::VERTEX_SLOTS],
            attributes: [resource::VertexAttribute::default(); resource::VERTEX_SLOTS],
            layout_objects: std::ptr::null_mut(),
            attribute_objects: std::ptr::null_mut(),
        },
    );
    // SAFETY: objects this test made, sent the selectors they implement.
    unsafe {
        let layouts = send(vertex_descriptor, "layouts");
        send_number(element_at(layouts, 0), "setStride:", 16);
        let attributes = send(vertex_descriptor, "attributes");
        let attribute = element_at(attributes, 0);
        send_number(attribute, "setFormat:", 31); // MTLVertexFormatFloat4
        send_number(attribute, "setOffset:", 0);
        send_number(attribute, "setBufferIndex:", 0);
    }

    let descriptor = object::make(
        c"MTLRenderPipelineDescriptor",
        pipeline::RenderPipelineDescriptor {
            vertex_function: vertex,
            fragment_function: fragment,
            vertex_descriptor,
            colors: [pipeline::ColorAttachment::fresh(); pipeline::COLOR_ATTACHMENTS],
            depth_format: 0,
            stencil_format: 0,
            sample_count: 1,
            alpha_to_coverage: false,
            color_objects: std::ptr::null_mut(),
        },
    );
    // SAFETY: a descriptor this test made.
    unsafe {
        let colors = send(descriptor, "colorAttachments");
        send_number(element_at(colors, 0), "setPixelFormat:", 80); // BGRA8Unorm
    }
    let mut error = std::ptr::null_mut();
    // SAFETY: a device and a descriptor this test made.
    let state = unsafe { new_render_pipeline(device, std::ptr::null(), descriptor, &mut error) };
    assert!(!state.is_null(), "render pipeline creation failed");

    // The colour attachment, the vertices and the buffer read back into.
    let target = resource::new_texture(
        gpu_of(device).expect("a Vulkan device"),
        resource::TextureDescriptor {
            texture_type: 2,
            pixel_format: 80,
            width: SIZE,
            height: SIZE,
            depth: 1,
            mipmap_level_count: 1,
            sample_count: 1,
            array_length: 1,
            storage_mode: 2,
            usage: gpu::USAGE_RENDER_TARGET,
        },
    );
    assert!(!target.is_null());
    // SAFETY: the same calls the guest makes.
    let (vertices, readback, queue) = unsafe {
        (
            new_buffer(device, std::ptr::null(), 48, 0),
            new_buffer(device, std::ptr::null(), (SIZE * SIZE * 4) as usize, 0),
            new_command_queue(device, std::ptr::null()),
        )
    };
    // A triangle which covers the whole target in Metal's clip space.
    // SAFETY: a buffer this test made, mapped over its whole length.
    unsafe {
        let contents = object::body::<resource::Buffer>(vertices).inner.contents as *mut f32;
        for (index, value) in [
            -1.0f32, -1.0, 0.0, 1.0, //
            3.0, -1.0, 0.0, 1.0, //
            -1.0, 3.0, 0.0, 1.0,
        ]
        .iter()
        .enumerate()
        {
            contents.add(index).write(*value);
        }
    }

    let pass = object::make(
        c"MTLRenderPassDescriptor",
        encoder::RenderPassDescriptor::default(),
    );
    // SAFETY: objects this test made, sent the selectors they implement.
    unsafe {
        let colors = send(pass, "colorAttachments");
        let attachment = element_at(colors, 0);
        send_with_object(attachment, "setTexture:", target);
        send_number(attachment, "setLoadAction:", 2); // Clear
        send_number(attachment, "setStoreAction:", 1); // Store
    }

    // SAFETY: objects this test made, sent the selectors they implement.
    unsafe {
        let command = send(queue, "commandBuffer");
        let render = render_encoder(command, pass);
        assert!(!render.is_null());
        send_with_object(render, "setRenderPipelineState:", state);
        set_buffer_named(render, "setVertexBuffer:offset:atIndex:", vertices, 0, 0);
        draw(render, 3);
        send_void(render, "endEncoding");

        let blit = send(command, "blitCommandEncoder");
        copy_texture_to_buffer(blit, target, readback, SIZE);
        send_void(blit, "endEncoding");
        send_void(command, "commit");

        let contents = object::body::<resource::Buffer>(readback).inner.contents;
        let middle = ((SIZE / 2) * SIZE + SIZE / 2) as usize * 4;
        let texel = std::slice::from_raw_parts(contents.add(middle), 4);
        // BGRA8Unorm: the fragment shader's opaque red is (0, 0, 255, 255).
        assert_eq!(texel, [0, 0, 255, 255], "the triangle did not draw red");
    }
}

/// # Safety
///
/// `command` must be an `R3MTLCommandBuffer` and `pass` a render pass
/// descriptor.
unsafe fn render_encoder(command: Id, pass: Id) -> Id {
    let sel = objc::sel::get("renderCommandEncoderWithDescriptor:");
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(command) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    let call: unsafe extern "C" fn(Id, Sel, Id) -> Id =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(command, sel, pass) }
}

/// # Safety
///
/// `encoder` must be an `R3MTLCommandEncoder`.
unsafe fn set_buffer_named(encoder: Id, selector: &str, buffer: Id, offset: u64, index: u64) {
    let sel = objc::sel::get(selector);
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(encoder) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    let call: unsafe extern "C" fn(Id, Sel, Id, u64, u64) =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { call(encoder, sel, buffer, offset, index) };
}

/// # Safety
///
/// `encoder` must be an `R3MTLCommandEncoder` inside a render pass.
unsafe fn draw(encoder: Id, vertices: u64) {
    let sel = objc::sel::get("drawPrimitives:vertexStart:vertexCount:instanceCount:");
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(encoder) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    let call: unsafe extern "C" fn(Id, Sel, u64, u64, u64, u64) =
        unsafe { std::mem::transmute(method.implementation()) };
    // MTLPrimitiveTypeTriangle is 3.
    // SAFETY: as above.
    unsafe { call(encoder, sel, 3, 0, vertices, 1) };
}

/// # Safety
///
/// `encoder` must be an `R3MTLCommandEncoder`.
unsafe fn copy_texture_to_buffer(encoder: Id, texture: Id, buffer: Id, size: u64) {
    let sel = objc::sel::get(
        "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toBuffer:destinationOffset:destinationBytesPerRow:destinationBytesPerImage:",
    );
    // SAFETY: an object the runtime allocated.
    let method = unsafe { objc::object::class_of(encoder) }
        .and_then(|class| class.method(sel))
        .expect("the selector is registered");
    // SAFETY: the registered signature.
    #[allow(clippy::type_complexity)]
    let call: unsafe extern "C" fn(
        Id,
        Sel,
        Id,
        u64,
        u64,
        encoder::MtlOrigin,
        encoder::MtlSize,
        Id,
        u64,
        u64,
        u64,
    ) = unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe {
        call(
            encoder,
            sel,
            texture,
            0,
            0,
            encoder::MtlOrigin { x: 0, y: 0, z: 0 },
            encoder::MtlSize {
                width: size,
                height: size,
                depth: 1,
            },
            buffer,
            0,
            size * 4,
            size * size * 4,
        )
    };
}
