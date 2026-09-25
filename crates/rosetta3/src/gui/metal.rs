use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use super::draw::DrawList;
use super::objc::{Id, Sel, class, pool_pop, pool_push, send0, send1, send2, send3, send4};

// Metal enumerators, from `<Metal/MTLRenderPass.h>`,
// `<Metal/MTLPixelFormat.h>`, `<Metal/MTLRenderPipeline.h>` and
// `<Metal/MTLRenderCommandEncoder.h>`.

/// `MTLLoadActionLoad`: keep what the guest already drew. The whole point.
const LOAD_ACTION_LOAD: u64 = 1;
/// `MTLStoreActionStore`.
const STORE_ACTION_STORE: u64 = 1;
/// `MTLPixelFormatR8Unorm`, which is what an 8-bit coverage atlas is.
const PIXEL_FORMAT_R8: u64 = 10;
/// `MTLPrimitiveTypeTriangle`.
const PRIMITIVE_TRIANGLE: u64 = 3;
/// `MTLBlendOperationAdd`.
const BLEND_ADD: u64 = 0;
/// `MTLBlendFactorSourceAlpha` and `MTLBlendFactorOneMinusSourceAlpha`: ordinary
/// straight-alpha compositing over whatever the guest rendered.
const BLEND_SRC_ALPHA: u64 = 4;
const BLEND_ONE_MINUS_SRC_ALPHA: u64 = 5;
/// `MTLResourceStorageModeShared`, which on Apple silicon is the only kind of
/// memory there is and costs nothing to ask for.
const STORAGE_SHARED: u64 = 0;

/// Buffers in the ring. Metal's own recommendation, and the reason is exactly the
/// situation here.
const RING: usize = 3;

/// `MTLRegion`.
#[repr(C)]
struct Region {
    x: u64,
    y: u64,
    z: u64,
    width: u64,
    height: u64,
    depth: u64,
}

/// The shader pair, in one string.
const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Vertex {
    float2 position;
    float2 uv;
    float4 color;
};

struct Varying {
    float4 position [[position]];
    float2 uv;
    float4 color;
};

vertex Varying rosetta3_overlay_vertex(
    uint id [[vertex_id]],
    const device Vertex *vertices [[buffer(0)]],
    constant float2 &surface [[buffer(1)]])
{
    Varying out;
    float2 unit = vertices[id].position / surface * 2.0 - 1.0;
    out.position = float4(unit.x, -unit.y, 0.0, 1.0);
    out.uv = vertices[id].uv;
    out.color = vertices[id].color;
    return out;
}

fragment float4 rosetta3_overlay_fragment(
    Varying in [[stage_in]],
    texture2d<float> atlas [[texture(0)]])
{
    constexpr sampler nearest(filter::nearest, address::clamp_to_edge);
    float coverage = atlas.sample(nearest, in.uv).r;
    return float4(in.color.rgb, in.color.a * coverage);
}
"#;

/// Everything built once per device.
struct Resources {
    pipeline: Id,
    atlas: Id,
    /// The size of the atlas the texture holds, so a font rebuilt at a new scale
    /// is noticed and re-uploaded rather than sampled at the wrong coordinates.
    atlas_key: (usize, usize),
    ring: [Option<(Id, usize)>; RING],
    next: usize,
}

/// Resources by `(device, pixel format)`.
struct Cache(HashMap<(usize, u64), Resources>);

// SAFETY: the values are Metal object pointers. Metal's own documented rule is
// that its objects are safe to use from multiple threads once created, and this
// module additionally only ever touches them from whichever thread is
// presenting. What crosses threads is the pointer, not a Rust reference to
// anything the compiler is reasoning about.
unsafe impl Send for Cache {}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// The guest's own command queue, kept so that work of ours can be ordered against work
/// of its.
static QUEUE: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Remember the queue a command buffer belongs to, once.
///
/// # Safety
///
/// `command_buffer` must be a live object pointer, on a thread that may message it.
pub unsafe fn note_queue(command_buffer: Id) {
    use std::sync::atomic::Ordering::{Acquire, Release};
    if command_buffer.is_null() || !QUEUE.load(Acquire).is_null() {
        return;
    }
    let selector = se("commandQueue");
    // SAFETY: the caller's obligation is exactly this function's - a live object
    // pointer - and `responds` asks the runtime rather than the object.
    if !unsafe { super::objc::responds(command_buffer, selector) } {
        return;
    }
    // SAFETY: the receiver implements `commandQueue`, whose declaration in
    // `<Metal/MTLCommandBuffer.h>` takes nothing and returns the queue.
    let queue = unsafe { send0::<Id>(command_buffer, selector) };
    if queue.is_null() {
        return;
    }
    // Retained, because this outlives the guest's own reference to it and a
    // dangling queue pointer is a fault inside Metal with this runtime nowhere
    // in the backtrace.
    //
    // SAFETY: `retain` is `NSObject`'s and every Metal object is one.
    let queue = unsafe { send0::<Id>(queue, se("retain")) };
    QUEUE.store(queue, Release);
    if super::debugging() {
        eprintln!("rosetta3: gui  found the guest's Metal command queue");
    }
}

/// Draw the overlay for a guest that presents the drawable *itself*, with no command
/// buffer in the message.
///
/// # Safety
///
/// `drawable` must be a live object pointer on a thread that may message it.
pub unsafe fn present_standalone(drawable: Id) {
    use std::sync::atomic::Ordering::Acquire;
    // SAFETY: as above - the caller states the pointer is live, and whether it
    // is a drawable is what this asks.
    if drawable.is_null() || !unsafe { super::objc::responds(drawable, se("texture")) } {
        return;
    }
    let queue = QUEUE.load(Acquire);
    if queue.is_null() {
        super::say_once(
            "the guest presents its drawable directly and no Metal command queue has been \
             seen yet, so there is nothing to order the overlay's own work against",
        );
        return;
    }
    // SAFETY: `-commandBuffer` is `MTLCommandQueue`'s, takes nothing, and
    // returns an autoreleased command buffer. The pool is this function's own
    // because the guest's presenting thread need not have one.
    let pool = unsafe { pool_push() };
    let command_buffer = unsafe { send0::<Id>(queue, se("commandBuffer")) };
    if !command_buffer.is_null() {
        // SAFETY: a freshly created command buffer, and the drawable the guest
        // is presenting. `present` encodes and commits; the queue orders it
        // after everything the guest has already committed.
        unsafe { present(command_buffer, drawable) };
        // SAFETY: `-commit` is `MTLCommandBuffer`'s and takes nothing. Not
        // waited on: a wait here would stall the guest's presenting thread on
        // the GPU, which is the one thing an overlay must never do.
        unsafe { send0::<()>(command_buffer, se("commit")) };
    }
    // SAFETY: `pool` is this thread's token, popped exactly once.
    unsafe { pool_pop(pool) };
}

/// Draw the overlay into the drawable `command_buffer` is about to present.
///
/// # Safety
///
/// `command_buffer` must be a live, un-committed `MTLCommandBuffer` and `drawable` a
/// `CAMetalDrawable`, on the thread that is presenting. Two call sites satisfy that.
pub unsafe fn present(command_buffer: Id, drawable: Id) {
    if command_buffer.is_null() || drawable.is_null() {
        return;
    }
    // SAFETY of every send in this function: each selector is a documented
    // Metal or Foundation method and each prototype is transcribed from its
    // header. The receivers are the objects the guest handed to
    // `presentDrawable:`, which are a command buffer and a drawable by that
    // method's own declaration.
    let texture = unsafe { send0::<Id>(drawable, se("texture")) };
    if texture.is_null() {
        super::say_once("the drawable being presented has no texture to draw into");
        return;
    }
    let device = unsafe { send0::<Id>(texture, se("device")) };
    let width = unsafe { send0::<u64>(texture, se("width")) };
    let height = unsafe { send0::<u64>(texture, se("height")) };
    let format = unsafe { send0::<u64>(texture, se("pixelFormat")) };
    if device.is_null() || width == 0 || height == 0 {
        super::say_once("the drawable's texture reports no device or no size");
        return;
    }
    announce(format, width, height);

    let Some(list) = super::compose(width as f32, height as f32) else {
        return;
    };

    // A pool of our own around every object created below. See
    // [`super::objc::pool_push`] for why the guest's cannot be assumed.
    //
    // SAFETY: pushed and popped on this thread, once, with nothing between them
    // that can unwind - every call in the body is an `extern "C"` message send.
    let pool = unsafe { pool_push() };
    encode(command_buffer, texture, device, format, &list);
    super::drew("Metal", list.width, list.height, list.vertices.len());
    // SAFETY: `pool` is this thread's token, popped exactly once.
    unsafe { pool_pop(pool) };
}

/// Say which drawable the overlay found, once, under `ROSETTA3_GUI_DEBUG`.
fn announce(format: u64, width: u64, height: u64) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !super::debugging() || SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "rosetta3: gui  Metal drawable {width}x{height}, MTLPixelFormat {format}          -> encoding into the guest's command buffer"
    );
}

fn encode(command_buffer: Id, texture: Id, device: Id, format: u64, list: &DrawList) {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = &mut guard.get_or_insert_with(|| Cache(HashMap::new())).0;
    let key = (device as usize, format);
    if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(key) {
        let Some(built) = build(device, format) else {
            // Built once and failed once.
            return;
        };
        slot.insert(built);
    }
    let Some(resources) = cache.get_mut(&key) else {
        return;
    };

    upload_atlas(device, resources, list);
    let Some(vertices) = upload_vertices(device, resources, list) else {
        return;
    };

    // The render pass: load what is there, draw, store. `renderPassDescriptor`
    // is a class method returning an autoreleased descriptor, which is why this
    // whole function runs inside a pool.
    //
    // SAFETY of the sends below: each is a documented Metal method with the
    // prototype its header gives.
    let pass = unsafe { send0::<Id>(class("MTLRenderPassDescriptor"), se("renderPassDescriptor")) };
    if pass.is_null() {
        super::say_once("MTLRenderPassDescriptor is not a class this process has");
        return;
    }
    let attachments = unsafe { send0::<Id>(pass, se("colorAttachments")) };
    let color = unsafe { send1::<Id, u64>(attachments, se("objectAtIndexedSubscript:"), 0) };
    unsafe {
        send1::<(), Id>(color, se("setTexture:"), texture);
        send1::<(), u64>(color, se("setLoadAction:"), LOAD_ACTION_LOAD);
        send1::<(), u64>(color, se("setStoreAction:"), STORE_ACTION_STORE);
    }

    let encoder = unsafe {
        send1::<Id, Id>(
            command_buffer,
            se("renderCommandEncoderWithDescriptor:"),
            pass,
        )
    };
    if encoder.is_null() {
        super::say_once(
            "the guest's command buffer would not give a render encoder: it is already committed, \
             so the overlay has nothing to encode into",
        );
        return;
    }

    let surface = [list.width, list.height];
    let count = list.vertices.len() as u64;
    // SAFETY: the encoder is live until `endEncoding`, the pipeline came from
    // this device, the buffer holds `count` vertices in the layout the shader
    // declares, and `setVertexBytes:length:atIndex:` copies its argument - which
    // is why a stack local is a legal source for it.
    unsafe {
        send1::<(), Id>(encoder, se("setRenderPipelineState:"), resources.pipeline);
        send3::<(), Id, u64, u64>(
            encoder,
            se("setVertexBuffer:offset:atIndex:"),
            vertices,
            0,
            0,
        );
        send3::<(), *const c_void, u64, u64>(
            encoder,
            se("setVertexBytes:length:atIndex:"),
            surface.as_ptr().cast(),
            size_of_val(&surface) as u64,
            1,
        );
        send2::<(), Id, u64>(
            encoder,
            se("setFragmentTexture:atIndex:"),
            resources.atlas,
            0,
        );
        send3::<(), u64, u64, u64>(
            encoder,
            se("drawPrimitives:vertexStart:vertexCount:"),
            PRIMITIVE_TRIANGLE,
            0,
            count,
        );
        send0::<()>(encoder, se("endEncoding"));
    }
}

/// Compile the shaders and build the pipeline state for one device.
fn build(device: Id, format: u64) -> Option<Resources> {
    let source = super::objc::ns_string(SHADER);
    if source.is_null() {
        return None;
    }
    let mut error: Id = std::ptr::null_mut();
    // SAFETY: `-newLibraryWithSource:options:error:` takes an `NSString`, a
    // nullable `MTLCompileOptions` and an out-parameter for an `NSError`.
    let library = unsafe {
        send3::<Id, Id, Id, *mut Id>(
            device,
            se("newLibraryWithSource:options:error:"),
            source,
            std::ptr::null_mut(),
            &mut error,
        )
    };
    if library.is_null() {
        super::say_once("the overlay's Metal shaders would not compile");
        return None;
    }

    // SAFETY: `-newFunctionWithName:` takes an `NSString` and returns a
    // function or nil.
    let vertex = unsafe {
        send1::<Id, Id>(
            library,
            se("newFunctionWithName:"),
            super::objc::ns_string("rosetta3_overlay_vertex"),
        )
    };
    let fragment = unsafe {
        send1::<Id, Id>(
            library,
            se("newFunctionWithName:"),
            super::objc::ns_string("rosetta3_overlay_fragment"),
        )
    };
    if vertex.is_null() || fragment.is_null() {
        return None;
    }

    let descriptor_class = class("MTLRenderPipelineDescriptor");
    // SAFETY: `+alloc`/`-init` on a Metal descriptor class.
    let descriptor = unsafe { send0::<Id>(send0::<Id>(descriptor_class, se("alloc")), se("init")) };
    if descriptor.is_null() {
        return None;
    }
    // SAFETY: the setters below are the descriptor's own, with the types its
    // header declares. The colour attachment is index zero because the render
    // pass above binds exactly one.
    let pipeline = unsafe {
        send1::<(), Id>(descriptor, se("setVertexFunction:"), vertex);
        send1::<(), Id>(descriptor, se("setFragmentFunction:"), fragment);
        let attachments = send0::<Id>(descriptor, se("colorAttachments"));
        let color = send1::<Id, u64>(attachments, se("objectAtIndexedSubscript:"), 0);
        send1::<(), u64>(color, se("setPixelFormat:"), format);
        // Straight alpha over the guest's frame, which is what the draw list
        // produces and what the fragment shader hands back.
        send1::<(), u8>(color, se("setBlendingEnabled:"), 1);
        send1::<(), u64>(color, se("setRgbBlendOperation:"), BLEND_ADD);
        send1::<(), u64>(color, se("setAlphaBlendOperation:"), BLEND_ADD);
        send1::<(), u64>(color, se("setSourceRGBBlendFactor:"), BLEND_SRC_ALPHA);
        send1::<(), u64>(color, se("setSourceAlphaBlendFactor:"), BLEND_SRC_ALPHA);
        send1::<(), u64>(
            color,
            se("setDestinationRGBBlendFactor:"),
            BLEND_ONE_MINUS_SRC_ALPHA,
        );
        send1::<(), u64>(
            color,
            se("setDestinationAlphaBlendFactor:"),
            BLEND_ONE_MINUS_SRC_ALPHA,
        );
        let mut error: Id = std::ptr::null_mut();
        send2::<Id, Id, *mut Id>(
            device,
            se("newRenderPipelineStateWithDescriptor:error:"),
            descriptor,
            &mut error,
        )
    };
    if pipeline.is_null() {
        super::say_once("the overlay's Metal pipeline state would not build");
        return None;
    }

    Some(Resources {
        pipeline,
        atlas: std::ptr::null_mut(),
        atlas_key: (0, 0),
        ring: [None; RING],
        next: 0,
    })
}

/// Upload the font atlas, if the one in the texture is not the one being drawn
/// with.
fn upload_atlas(device: Id, resources: &mut Resources, list: &DrawList) {
    let atlas = list.atlas();
    let key = (atlas.width, atlas.height);
    if resources.atlas_key == key && !resources.atlas.is_null() {
        return;
    }

    // SAFETY: `+texture2DDescriptorWithPixelFormat:width:height:mipmapped:` is
    // `MTLTextureDescriptor`'s, and returns an autoreleased descriptor whose
    // default usage is shader read - which is the only use here.
    let descriptor = unsafe {
        send4::<Id, u64, u64, u64, u8>(
            class("MTLTextureDescriptor"),
            se("texture2DDescriptorWithPixelFormat:width:height:mipmapped:"),
            PIXEL_FORMAT_R8,
            atlas.width as u64,
            atlas.height as u64,
            0,
        )
    };
    if descriptor.is_null() {
        return;
    }
    // SAFETY: `-newTextureWithDescriptor:` returns a texture owned by us, kept
    // for the life of the process.
    let texture = unsafe { send1::<Id, Id>(device, se("newTextureWithDescriptor:"), descriptor) };
    if texture.is_null() {
        return;
    }
    let region = Region {
        x: 0,
        y: 0,
        z: 0,
        width: atlas.width as u64,
        height: atlas.height as u64,
        depth: 1,
    };
    // SAFETY: the region is the whole texture, the buffer is
    // `width * height` bytes at one byte per texel, and the row length says so.
    unsafe {
        send4::<(), Region, u64, *const c_void, u64>(
            texture,
            se("replaceRegion:mipmapLevel:withBytes:bytesPerRow:"),
            region,
            0,
            atlas.pixels.as_ptr().cast(),
            atlas.width as u64,
        )
    };
    resources.atlas = texture;
    resources.atlas_key = key;
}

/// Put this frame's vertices in the next buffer of the ring, growing it if the
/// panel got taller.
fn upload_vertices(device: Id, resources: &mut Resources, list: &DrawList) -> Option<Id> {
    let bytes = std::mem::size_of_val(&list.vertices[..]);
    let slot = resources.next;
    resources.next = (resources.next + 1) % RING;

    let needed = bytes.next_power_of_two();
    let reuse = matches!(resources.ring[slot], Some((buffer, size))
        if size >= bytes && !buffer.is_null());
    if !reuse {
        // SAFETY: `-newBufferWithLength:options:` returns a buffer owned by us.
        // The old one in this slot is dropped without a release, deliberately:
        // the GPU may still be reading it, there is no completion handler here
        // to release it from, and a buffer is reallocated at most a handful of
        // times in a run - when the panel first opens, and if it grows.
        let buffer = unsafe {
            send2::<Id, u64, u64>(
                device,
                se("newBufferWithLength:options:"),
                needed as u64,
                STORAGE_SHARED,
            )
        };
        if buffer.is_null() {
            return None;
        }
        resources.ring[slot] = Some((buffer, needed));
    }
    let (buffer, _) = resources.ring[slot]?;

    // SAFETY: `-contents` is the buffer's own mapping, which for shared storage
    // is CPU-writable for the buffer's whole length; `bytes` is at most that
    // length by the check above, and the source is a live slice.
    let contents = unsafe { send0::<*mut c_void>(buffer, se("contents")) };
    if contents.is_null() {
        return None;
    }
    // SAFETY: as above. `Vertex` is `repr(C)` and the shader's `Vertex` has the
    // same layout, which is the one thing this copy depends on.
    unsafe {
        std::ptr::copy_nonoverlapping(
            list.vertices.as_ptr().cast::<u8>(),
            contents.cast::<u8>(),
            bytes,
        )
    };
    Some(buffer)
}

/// A selector, uniqued once per call site.
fn se(name: &'static str) -> Sel {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Mutex<HashMap<&'static str, usize>>> = OnceLock::new();
    let table = TABLE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = table.lock().unwrap_or_else(|e| e.into_inner());
    *guard
        .entry(name)
        .or_insert_with(|| super::objc::sel(name) as usize) as Sel
}
