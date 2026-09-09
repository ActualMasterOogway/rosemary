//! Command queues, command buffers and the encoders which record into them.
//!
//! One Metal command buffer is one Vulkan command buffer with its own pool,
//! fence and descriptor pools, so that recording on several guest threads
//! needs no cross-thread synchronization. Metal binds resources to an encoder
//! and Vulkan binds descriptor sets to a pipeline, so the encoder keeps
//! Metal's binding tables and materializes descriptor sets at each dispatch
//! and draw.

use ash::vk;
use metal_translate::ir::{ResourceKind, Stage};
use objc::object::Id;
use objc::sel::Sel;

use crate::gpu::{self, Gpu, RenderPassKey};
use crate::object::{self, methods};
use crate::pipeline::{
    ComputePipeline, DepthStencilKey, RenderPipeline, RenderVariantKey, SETS, descriptor_type,
};
use crate::resource::{Buffer, DepthStencilState, SamplerState, Texture};
use crate::stats::{self, Counter};

/// Metal's per-stage binding tables. Index by `Stage::ordinal`.
const STAGES: usize = 3;
const SLOTS: usize = 32;

#[derive(Clone, Copy)]
struct BufferBinding {
    object: Id,
    offset: u64,
}

impl Default for BufferBinding {
    fn default() -> Self {
        Self {
            object: std::ptr::null_mut(),
            offset: 0,
        }
    }
}

pub(crate) struct CommandQueue {
    pub gpu: &'static Gpu,
    pub device: Id,
}

pub(crate) struct CommandBuffer {
    pub gpu: &'static Gpu,
    /// Two timestamps around the whole recording, under `ROSETTA3_METAL_STATS`.
    /// Whether a frame is waiting for the device or for this runtime is a
    /// question only the device can answer.
    timestamps: vk::QueryPool,
    pub queue: Id,
    pub pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub fence: vk::Fence,
    descriptor_pools: Vec<vk::DescriptorPool>,
    /// Objects the recording refers to, retained until the work completes.
    retained: Vec<Id>,
    framebuffers: Vec<vk::Framebuffer>,
    scheduled_handlers: Vec<Id>,
    completed_handlers: Vec<Id>,
    /// Drawables `-presentDrawable:` asked for, presented once the work is done.
    presents: Vec<Id>,
    pub status: u64,
    recording: bool,
}

pub(crate) struct Encoder {
    pub command_buffer: Id,
    pub kind: Kind,
    compute_pipeline: Id,
    render_pipeline: Id,
    depth_state: Id,
    buffers: [[BufferBinding; SLOTS]; STAGES],
    textures: [[Id; SLOTS]; STAGES],
    samplers: [[Id; SLOTS]; STAGES],
    stencil_reference: u32,
    cull_mode: u64,
    winding: u64,
    fill_mode: u64,
    depth_clip: u64,
    in_render_pass: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Kind {
    Compute,
    Blit,
    Render,
}

// ------------------------------------------------------------- structures
// Metal passes these by value, and the guest ABI puts anything over sixteen
// bytes in memory, so each must be one `repr(C)` argument.

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MtlSize {
    pub width: u64,
    pub height: u64,
    pub depth: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MtlOrigin {
    pub x: u64,
    pub y: u64,
    pub z: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Viewport {
    pub origin_x: f64,
    pub origin_y: f64,
    pub width: f64,
    pub height: f64,
    pub znear: f64,
    pub zfar: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ScissorRect {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ClearColor {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

// --------------------------------------------------------------- queue

pub(crate) fn new_command_queue(gpu: &'static Gpu, device: Id) -> Id {
    object::make(
        c"R3MTLCommandQueue",
        CommandQueue {
            gpu,
            device: objc::arc::retain(device),
        },
    )
}

unsafe extern "C" fn queue_device(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a command queue of this crate's.
    unsafe { object::body::<CommandQueue>(receiver) }.device
}

unsafe extern "C" fn new_command_buffer(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a command queue of this crate's.
    let queue = unsafe { object::body::<CommandQueue>(receiver) };
    let gpu = queue.gpu;
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(gpu.queue_family)
        .flags(vk::CommandPoolCreateFlags::TRANSIENT);
    // SAFETY: a well-formed create info on this live device.
    let pool = match unsafe { gpu.device.create_command_pool(&pool_info, None) } {
        Ok(pool) => pool,
        Err(error) => {
            gpu::trace(format!("vkCreateCommandPool failed with {error:?}"));
            return std::ptr::null_mut();
        }
    };
    let allocate = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: the pool was just created by this device.
    let command_buffer = match unsafe { gpu.device.allocate_command_buffers(&allocate) } {
        Ok(buffers) => buffers[0],
        Err(error) => {
            gpu::trace(format!("vkAllocateCommandBuffers failed with {error:?}"));
            return std::ptr::null_mut();
        }
    };
    // SAFETY: a well-formed create info on this live device.
    let fence = match unsafe {
        gpu.device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    } {
        Ok(fence) => fence,
        Err(error) => {
            gpu::trace(format!("vkCreateFence failed with {error:?}"));
            return std::ptr::null_mut();
        }
    };
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the command buffer is fresh and unrecorded.
    if let Err(error) = unsafe { gpu.device.begin_command_buffer(command_buffer, &begin) } {
        gpu::trace(format!("vkBeginCommandBuffer failed with {error:?}"));
        return std::ptr::null_mut();
    }
    let timestamps = gpu.timestamp_pool();
    if timestamps != vk::QueryPool::null() {
        // SAFETY: recording into the command buffer opened above.
        unsafe {
            gpu.device
                .cmd_reset_query_pool(command_buffer, timestamps, 0, 2);
            gpu.device.cmd_write_timestamp(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                timestamps,
                0,
            );
        }
    }
    stats::count(Counter::CommandBuffers);
    object::make(
        c"R3MTLCommandBuffer",
        CommandBuffer {
            gpu,
            timestamps,
            queue: objc::arc::retain(receiver),
            pool,
            command_buffer,
            fence,
            descriptor_pools: Vec::new(),
            retained: Vec::new(),
            framebuffers: Vec::new(),
            scheduled_handlers: Vec::new(),
            completed_handlers: Vec::new(),
            presents: Vec::new(),
            status: 0,
            recording: true,
        },
    )
}

// -------------------------------------------------------- command buffer

impl CommandBuffer {
    fn retain(&mut self, object: Id) {
        if !object.is_null() {
            self.retained.push(objc::arc::retain(object));
        }
    }

    /// A descriptor set for `layout`, from a pool this command buffer owns.
    fn allocate_set(&mut self, layout: vk::DescriptorSetLayout) -> Option<vk::DescriptorSet> {
        for attempt in 0..2 {
            if let Some(pool) = self.descriptor_pools.last().copied() {
                let layouts = [layout];
                let info = vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts);
                // SAFETY: the pool and layout belong to this live device.
                match unsafe { self.gpu.device.allocate_descriptor_sets(&info) } {
                    Ok(sets) => {
                        stats::count(Counter::DescriptorSets);
                        return sets.first().copied();
                    }
                    // A full pool is expected: allocate another and retry.
                    Err(_) if attempt == 0 => {}
                    Err(error) => {
                        gpu::trace(format!("vkAllocateDescriptorSets failed with {error:?}"));
                        return None;
                    }
                }
            }
            const CAPACITY: u32 = 512;
            let sizes = [
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(CAPACITY * 4),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(CAPACITY * 4),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(CAPACITY),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::SAMPLER)
                    .descriptor_count(CAPACITY * 2),
            ];
            let info = vk::DescriptorPoolCreateInfo::default()
                .max_sets(CAPACITY)
                .pool_sizes(&sizes);
            // SAFETY: a well-formed create info on this live device.
            match unsafe { self.gpu.device.create_descriptor_pool(&info, None) } {
                Ok(pool) => self.descriptor_pools.push(pool),
                Err(error) => {
                    gpu::trace(format!("vkCreateDescriptorPool failed with {error:?}"));
                    return None;
                }
            }
        }
        None
    }
}

unsafe extern "C" fn command_buffer_queue(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a command buffer of this crate's.
    unsafe { object::body::<CommandBuffer>(receiver) }.queue
}

unsafe extern "C" fn command_buffer_status(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: as above.
    unsafe { object::body::<CommandBuffer>(receiver) }.status
}

unsafe extern "C" fn add_handler(receiver: Id, sel: Sel, block: Id) {
    // SAFETY: as above.
    let command = unsafe { object::body::<CommandBuffer>(receiver) };
    // SAFETY: the guest passed a block.
    let retained = unsafe { objc::shorthand::objc_retainBlock(block) };
    if retained.is_null() {
        return;
    }
    // SAFETY: a selector the runtime owns.
    let scheduled = unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() == b"addScheduledHandler:";
    match scheduled {
        true => command.scheduled_handlers.push(retained),
        false => command.completed_handlers.push(retained),
    }
}

/// `-[MTLCommandBuffer commit]`: end recording, submit, and wait.
unsafe extern "C" fn commit(receiver: Id, _sel: Sel) {
    // SAFETY: a command buffer of this crate's.
    let command = unsafe { object::body::<CommandBuffer>(receiver) };
    if !command.recording {
        return;
    }
    command.recording = false;
    let gpu = command.gpu;
    if command.timestamps != vk::QueryPool::null() {
        // SAFETY: recording into this command buffer, which has not ended.
        unsafe {
            gpu.device.cmd_write_timestamp(
                command.command_buffer,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                command.timestamps,
                1,
            )
        };
    }
    // SAFETY: recording began when the command buffer was made.
    if let Err(error) = unsafe { gpu.device.end_command_buffer(command.command_buffer) } {
        gpu::trace(format!("vkEndCommandBuffer failed with {error:?}"));
        command.status = 5; // MTLCommandBufferStatusError
        return;
    }
    match stats::timed(Counter::SubmitMicros, || {
        gpu.submit_and_wait(command.command_buffer, command.fence)
    }) {
        Ok(()) => command.status = 4, // MTLCommandBufferStatusCompleted
        Err(reason) => {
            gpu::trace(format!("command buffer submission failed: {reason}"));
            command.status = 5;
        }
    }
    if command.timestamps != vk::QueryPool::null() {
        let mut ticks = [0u64; 2];
        // SAFETY: the submission completed, so both queries have results.
        let read = unsafe {
            gpu.device.get_query_pool_results(
                command.timestamps,
                0,
                &mut ticks,
                vk::QueryResultFlags::TYPE_64,
            )
        };
        if read.is_ok() {
            let nanoseconds = ticks[1].saturating_sub(ticks[0]) as f64 * gpu.timestamp_period();
            stats::add(Counter::GpuMicros, (nanoseconds / 1000.0) as u64);
        }
        gpu.release_timestamp_pool(command.timestamps);
        command.timestamps = vk::QueryPool::null();
    }
    let presents = std::mem::take(&mut command.presents);
    for drawable in presents {
        crate::present::present(drawable);
        objc::arc::release(drawable);
    }
    // The work has completed, so everything the recording referred to and every
    // transient object it created can go.
    for framebuffer in std::mem::take(&mut command.framebuffers) {
        // SAFETY: the submission finished before this point.
        unsafe { gpu.device.destroy_framebuffer(framebuffer, None) };
    }
    for pool in std::mem::take(&mut command.descriptor_pools) {
        // SAFETY: as above; the sets allocated from it are no longer in use.
        unsafe { gpu.device.destroy_descriptor_pool(pool, None) };
    }
    for object in std::mem::take(&mut command.retained) {
        objc::arc::release(object);
    }
    // SAFETY: as above.
    unsafe {
        gpu.device
            .free_command_buffers(command.pool, &[command.command_buffer]);
        gpu.device.destroy_command_pool(command.pool, None);
        gpu.device.destroy_fence(command.fence, None);
    }
    // Not called here: Metal calls a completion handler from its own queue, and
    // a guest which commits under a lock its handler also takes - which Roblox
    // does - deadlocks against itself the moment one runs on the committing
    // thread. See `crate::completion`.
    // A scheduled handler runs when the work reaches the device and a
    // completed handler when it finishes. This submission waits, so the two
    // moments are one; the order between them is still Metal's.
    for block in std::mem::take(&mut command.scheduled_handlers)
        .into_iter()
        .chain(std::mem::take(&mut command.completed_handlers))
    {
        // SAFETY: `objc_retainBlock` returned a live block, whose reference this
        // gives away, and `receiver` is the live command buffer it belongs to.
        unsafe { crate::completion::deliver(block, receiver) };
    }
}

unsafe extern "C" fn present_drawable(receiver: Id, _sel: Sel, drawable: Id) {
    if drawable.is_null() {
        return;
    }
    // SAFETY: a command buffer of this crate's.
    let command = unsafe { object::body::<CommandBuffer>(receiver) };
    command.presents.push(objc::arc::retain(drawable));
}

unsafe extern "C" fn present_drawable_at_time(receiver: Id, sel: Sel, drawable: Id, _at: f64) {
    // SAFETY: forwards to the untimed form; the runtime presents immediately.
    unsafe { present_drawable(receiver, sel, drawable) }
}

// --------------------------------------------------------------- encoders

pub(crate) fn new_encoder(command_buffer: Id, kind: Kind) -> Id {
    stats::count(Counter::Encoders);
    object::make(
        c"R3MTLCommandEncoder",
        Encoder {
            command_buffer,
            kind,
            compute_pipeline: std::ptr::null_mut(),
            render_pipeline: std::ptr::null_mut(),
            depth_state: std::ptr::null_mut(),
            buffers: [[BufferBinding::default(); SLOTS]; STAGES],
            textures: [[std::ptr::null_mut(); SLOTS]; STAGES],
            samplers: [[std::ptr::null_mut(); SLOTS]; STAGES],
            stencil_reference: 0,
            cull_mode: 0,
            winding: 0,
            fill_mode: 0,
            depth_clip: 0,
            in_render_pass: false,
        },
    )
}

fn stage_slot(stage: Stage) -> usize {
    match stage {
        Stage::Vertex => 1,
        Stage::Fragment => 2,
        _ => 0,
    }
}

impl Encoder {
    fn command<'a>(&self) -> &'a mut CommandBuffer {
        // SAFETY: the encoder holds the command buffer which made it.
        unsafe { object::body::<CommandBuffer>(self.command_buffer) }
    }

    /// Write the descriptor sets one pipeline's resources need and bind them.
    fn bind_resources(
        &mut self,
        resources: &[metal_translate::ResourceBinding],
        layouts: &[vk::DescriptorSetLayout; SETS],
        pipeline_layout: vk::PipelineLayout,
        bind_point: vk::PipelineBindPoint,
    ) {
        let start = stats::enabled().then(std::time::Instant::now);
        self.bind_resources_inner(resources, layouts, pipeline_layout, bind_point);
        if let Some(start) = start {
            stats::add(Counter::BindMicros, start.elapsed().as_micros() as u64);
        }
    }

    fn bind_resources_inner(
        &mut self,
        resources: &[metal_translate::ResourceBinding],
        layouts: &[vk::DescriptorSetLayout; SETS],
        pipeline_layout: vk::PipelineLayout,
        bind_point: vk::PipelineBindPoint,
    ) {
        let command = self.command();
        let gpu = command.gpu;
        let mut sets = [vk::DescriptorSet::null(); SETS];
        let mut used = [false; SETS];
        for binding in resources {
            let set = binding.descriptor_set as usize;
            if set < SETS {
                used[set] = true;
            }
        }
        for (index, present) in used.iter().enumerate() {
            if *present {
                let Some(set) = command.allocate_set(layouts[index]) else {
                    return;
                };
                sets[index] = set;
            }
        }

        let mut buffer_infos = Vec::new();
        let mut image_infos = Vec::new();
        let mut writes: Vec<(usize, u32, vk::DescriptorType, Which)> = Vec::new();
        enum Which {
            Buffer(usize),
            Image(usize),
        }
        for binding in resources {
            let set = binding.descriptor_set as usize;
            if set >= SETS {
                continue;
            }
            let slot = stage_slot(binding.stage);
            let index = binding.metal_index as usize;
            if index >= SLOTS {
                continue;
            }
            let Some(kind) = descriptor_type(binding) else {
                continue;
            };
            match binding.kind {
                ResourceKind::Buffer => {
                    let bound = self.buffers[slot][index];
                    if bound.object.is_null() {
                        continue;
                    }
                    // SAFETY: only a buffer of this crate's is ever bound.
                    let buffer = unsafe { object::body::<Buffer>(bound.object) };
                    buffer_infos.push(
                        vk::DescriptorBufferInfo::default()
                            .buffer(buffer.inner.buffer)
                            .offset(bound.offset.min(buffer.inner.length))
                            .range(vk::WHOLE_SIZE),
                    );
                    writes.push((
                        set,
                        binding.binding,
                        kind,
                        Which::Buffer(buffer_infos.len() - 1),
                    ));
                }
                ResourceKind::Texture => {
                    let bound = self.textures[slot][index];
                    if bound.is_null() {
                        continue;
                    }
                    // SAFETY: only a texture of this crate's is ever bound.
                    let texture = unsafe { object::body::<Texture>(bound) };
                    image_infos.push(
                        vk::DescriptorImageInfo::default()
                            .image_view(texture.image.view)
                            .image_layout(vk::ImageLayout::GENERAL),
                    );
                    writes.push((
                        set,
                        binding.binding,
                        kind,
                        Which::Image(image_infos.len() - 1),
                    ));
                }
                ResourceKind::Sampler => {
                    let bound = self.samplers[slot][index];
                    if bound.is_null() {
                        continue;
                    }
                    // SAFETY: only a sampler state of this crate's is bound.
                    let sampler = unsafe { object::body::<SamplerState>(bound) };
                    image_infos.push(vk::DescriptorImageInfo::default().sampler(sampler.sampler));
                    writes.push((
                        set,
                        binding.binding,
                        kind,
                        Which::Image(image_infos.len() - 1),
                    ));
                }
                _ => continue,
            }
        }

        let descriptor_writes = writes
            .iter()
            .map(|(set, binding, kind, which)| {
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(sets[*set])
                    .dst_binding(*binding)
                    .descriptor_type(*kind);
                match which {
                    Which::Buffer(at) => {
                        write.buffer_info(std::slice::from_ref(&buffer_infos[*at]))
                    }
                    Which::Image(at) => write.image_info(std::slice::from_ref(&image_infos[*at])),
                }
            })
            .collect::<Vec<_>>();
        if !descriptor_writes.is_empty() {
            // SAFETY: every referenced info lives across the call, and the sets
            // are not in use by a submitted command buffer.
            unsafe { gpu.device.update_descriptor_sets(&descriptor_writes, &[]) };
        }
        for (index, present) in used.iter().enumerate() {
            if *present {
                // SAFETY: recording into this command buffer.
                unsafe {
                    gpu.device.cmd_bind_descriptor_sets(
                        command.command_buffer,
                        bind_point,
                        pipeline_layout,
                        index as u32,
                        &[sets[index]],
                        &[],
                    )
                };
            }
        }
    }

    /// Metal orders the work of one encoder against the next; nothing here
    /// tracks which resource a later encoder reads, so one full barrier
    /// between encoders is what keeps the order Metal promises.
    ///
    /// It is not what makes a frame slow: removing it entirely takes the Roblox
    /// home page from 6.65 ms of device time a frame to 6.45 ms.
    fn barrier(&self) {
        let command = self.command();
        let memory = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);
        // SAFETY: recording into this command buffer.
        unsafe {
            command.gpu.device.cmd_pipeline_barrier(
                command.command_buffer,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[memory],
                &[],
                &[],
            )
        };
    }
}

unsafe extern "C" fn end_encoding(receiver: Id, _sel: Sel) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    if encoder.in_render_pass {
        let command = encoder.command();
        // SAFETY: a render pass began when the encoder was made.
        unsafe {
            command
                .gpu
                .device
                .cmd_end_render_pass(command.command_buffer)
        };
        encoder.in_render_pass = false;
    }
    encoder.barrier();
}

unsafe extern "C" fn set_compute_pipeline(receiver: Id, _sel: Sel, pipeline: Id) {
    // SAFETY: an encoder of this crate's.
    unsafe { object::body::<Encoder>(receiver) }.compute_pipeline = pipeline;
}

unsafe extern "C" fn set_render_pipeline(receiver: Id, _sel: Sel, pipeline: Id) {
    // SAFETY: as above.
    unsafe { object::body::<Encoder>(receiver) }.render_pipeline = pipeline;
}

unsafe extern "C" fn set_depth_stencil_state(receiver: Id, _sel: Sel, state: Id) {
    // SAFETY: as above.
    unsafe { object::body::<Encoder>(receiver) }.depth_state = state;
}

/// The stage a Metal binding selector names.
fn selector_stage(name: &[u8]) -> usize {
    if name.starts_with(b"setVertex") {
        1
    } else if name.starts_with(b"setFragment") {
        2
    } else {
        0
    }
}

unsafe extern "C" fn set_buffer(receiver: Id, sel: Sel, buffer: Id, offset: u64, index: u64) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // SAFETY: a selector the runtime owns.
    let stage = selector_stage(unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes());
    if let Some(slot) = encoder.buffers[stage].get_mut(index as usize) {
        *slot = BufferBinding {
            object: buffer,
            offset,
        };
    }
    // Metal retains what an encoder binds, and a guest is entitled to release
    // its own reference the moment the call returns. The command buffer's
    // reference outlives the work the binding is recorded into.
    encoder.command().retain(buffer);
}

/// `-setBytes:length:atIndex:` and its per-stage spellings.
///
/// Metal's small inline constants, which have no `MTLBuffer` of their own. One
/// is made here and retained by the command buffer, so the copy outlives the
/// recording exactly as a bound buffer does. Metal's own limit is four
/// kilobytes; a larger one is refused rather than quietly turned into an
/// allocation the guest never asked for.
unsafe extern "C" fn set_bytes(receiver: Id, sel: Sel, bytes: *const u8, length: u64, index: u64) {
    const LIMIT: u64 = 4096;
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    if bytes.is_null() || length == 0 || length > LIMIT {
        gpu::trace(format!("setBytes:length:atIndex: refused {length} byte(s)"));
        return;
    }
    stats::count(Counter::SetBytesCalls);
    stats::add(Counter::SetBytesBytes, length);
    let gpu = encoder.command().gpu;
    let buffer = crate::resource::new_buffer(gpu, length as usize, 0);
    if buffer.is_null() {
        return;
    }
    // SAFETY: a buffer this call just made, whose mapping is `length` bytes.
    let contents = unsafe { crate::resource::contents_of(buffer) };
    if !contents.is_null() {
        // SAFETY: the guest's `length` bytes into a mapping of that size.
        unsafe { std::ptr::copy_nonoverlapping(bytes, contents, length as usize) };
    }
    // SAFETY: a selector the runtime owns.
    let stage = selector_stage(unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes());
    if let Some(slot) = encoder.buffers[stage].get_mut(index as usize) {
        *slot = BufferBinding {
            object: buffer,
            offset: 0,
        };
    }
    // The encoder's own reference; the command buffer keeps it alive until the
    // work it was recorded into has finished.
    encoder.command().retain(buffer);
    objc::arc::release(buffer);
}

unsafe extern "C" fn set_buffer_offset(receiver: Id, sel: Sel, offset: u64, index: u64) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // SAFETY: a selector the runtime owns.
    let stage = selector_stage(unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes());
    if let Some(slot) = encoder.buffers[stage].get_mut(index as usize) {
        slot.offset = offset;
    }
}

unsafe extern "C" fn set_texture(receiver: Id, sel: Sel, texture: Id, index: u64) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // SAFETY: a selector the runtime owns.
    let stage = selector_stage(unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes());
    encoder.command().retain(texture);
    if let Some(slot) = encoder.textures[stage].get_mut(index as usize) {
        *slot = texture;
    }
}

unsafe extern "C" fn set_sampler(receiver: Id, sel: Sel, sampler: Id, index: u64) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // SAFETY: a selector the runtime owns.
    let stage = selector_stage(unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes());
    encoder.command().retain(sampler);
    if let Some(slot) = encoder.samplers[stage].get_mut(index as usize) {
        *slot = sampler;
    }
}

unsafe extern "C" fn set_stencil_reference(receiver: Id, _sel: Sel, value: u32) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    encoder.stencil_reference = value;
    let command = encoder.command();
    // SAFETY: recording into this command buffer.
    unsafe {
        command.gpu.device.cmd_set_stencil_reference(
            command.command_buffer,
            vk::StencilFaceFlags::FRONT_AND_BACK,
            value,
        )
    };
}

unsafe extern "C" fn set_raster_value(receiver: Id, sel: Sel, value: u64) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setCullMode:" => encoder.cull_mode = value,
        b"setFrontFacingWinding:" => encoder.winding = value,
        b"setTriangleFillMode:" => encoder.fill_mode = value,
        b"setDepthClipMode:" => encoder.depth_clip = value,
        _ => (),
    }
}

unsafe extern "C" fn set_viewport(receiver: Id, _sel: Sel, viewport: Viewport) {
    // SAFETY: an encoder of this crate's.
    let command = unsafe { object::body::<Encoder>(receiver) }.command();
    // Metal's clip space has Y up and its framebuffer origin is top left;
    // Vulkan's clip space has Y down. A negative viewport height, core since
    // Vulkan 1.1, is the exact correction.
    let flipped = vk::Viewport {
        x: viewport.origin_x as f32,
        y: (viewport.origin_y + viewport.height) as f32,
        width: viewport.width as f32,
        height: -(viewport.height as f32),
        min_depth: viewport.znear as f32,
        max_depth: viewport.zfar as f32,
    };
    // SAFETY: recording into this command buffer.
    unsafe {
        command
            .gpu
            .device
            .cmd_set_viewport(command.command_buffer, 0, &[flipped])
    };
}

unsafe extern "C" fn set_scissor(receiver: Id, _sel: Sel, rect: ScissorRect) {
    // SAFETY: an encoder of this crate's.
    let command = unsafe { object::body::<Encoder>(receiver) }.command();
    let scissor = vk::Rect2D {
        offset: vk::Offset2D {
            x: rect.x as i32,
            y: rect.y as i32,
        },
        extent: vk::Extent2D {
            width: rect.width as u32,
            height: rect.height as u32,
        },
    };
    // SAFETY: recording into this command buffer.
    unsafe {
        command
            .gpu
            .device
            .cmd_set_scissor(command.command_buffer, 0, &[scissor])
    };
}

unsafe extern "C" fn set_depth_bias(
    receiver: Id,
    _sel: Sel,
    bias: f32,
    slope_scale: f32,
    clamp: f32,
) {
    // SAFETY: an encoder of this crate's.
    let command = unsafe { object::body::<Encoder>(receiver) }.command();
    // SAFETY: recording into this command buffer.
    unsafe {
        command
            .gpu
            .device
            .cmd_set_depth_bias(command.command_buffer, bias, clamp, slope_scale)
    };
}

unsafe extern "C" fn set_blend_color(
    receiver: Id,
    _sel: Sel,
    red: f32,
    green: f32,
    blue: f32,
    alpha: f32,
) {
    // SAFETY: an encoder of this crate's.
    let command = unsafe { object::body::<Encoder>(receiver) }.command();
    // SAFETY: recording into this command buffer.
    unsafe {
        command
            .gpu
            .device
            .cmd_set_blend_constants(command.command_buffer, &[red, green, blue, alpha])
    };
}

// ------------------------------------------------------------- dispatch

unsafe extern "C" fn dispatch_threadgroups(
    receiver: Id,
    sel: Sel,
    groups: MtlSize,
    per_group: MtlSize,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    if encoder.kind != Kind::Compute || encoder.compute_pipeline.is_null() {
        return;
    }
    // SAFETY: only a compute pipeline state of this crate's is ever bound.
    let state = unsafe { object::body::<ComputePipeline>(encoder.compute_pipeline) };
    let size = [
        per_group.width.max(1) as u32,
        per_group.height.max(1) as u32,
        per_group.depth.max(1) as u32,
    ];
    let pipeline = match state.variant(size) {
        Ok(pipeline) => pipeline,
        Err(reason) => return gpu::trace(format!("compute pipeline creation failed: {reason}")),
    };
    // `-dispatchThreads:` counts threads; `-dispatchThreadgroups:` counts
    // groups. Metal rounds the thread form up to whole threadgroups.
    // SAFETY: a selector the runtime owns.
    let threads = unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes()
        == b"dispatchThreads:threadsPerThreadgroup:";
    let counts = if threads {
        [
            (groups.width.div_ceil(per_group.width.max(1))) as u32,
            (groups.height.div_ceil(per_group.height.max(1))) as u32,
            (groups.depth.div_ceil(per_group.depth.max(1))) as u32,
        ]
    } else {
        [
            groups.width as u32,
            groups.height as u32,
            groups.depth as u32,
        ]
    };
    let command = encoder.command();
    let device = command.gpu.device.clone();
    let vk_command = command.command_buffer;
    command.retain(encoder.compute_pipeline);
    // SAFETY: recording into this command buffer.
    unsafe { device.cmd_bind_pipeline(vk_command, vk::PipelineBindPoint::COMPUTE, pipeline) };
    let resources = state.resources.clone();
    let sets = state.layouts.sets;
    let layout = state.layouts.pipeline;
    encoder.bind_resources(&resources, &sets, layout, vk::PipelineBindPoint::COMPUTE);
    gpu::milestone("compute dispatch");
    stats::count(Counter::Dispatches);
    // SAFETY: as above.
    unsafe { device.cmd_dispatch(vk_command, counts[0], counts[1], counts[2]) };
}

// ----------------------------------------------------------------- draw

impl Encoder {
    /// Bind the pipeline variant and the resources one draw needs.
    fn prepare_draw(&mut self, primitive: u64) -> Option<(ash::Device, vk::CommandBuffer)> {
        if self.kind != Kind::Render || self.render_pipeline.is_null() || !self.in_render_pass {
            return None;
        }
        // SAFETY: only a render pipeline state of this crate's is ever bound.
        let state = unsafe { object::body::<RenderPipeline>(self.render_pipeline) };
        let depth = if self.depth_state.is_null() {
            DepthStencilState::default()
        } else {
            // SAFETY: only a depth/stencil state of this crate's is bound.
            *unsafe { object::body::<DepthStencilState>(self.depth_state) }
        };
        let key = RenderVariantKey {
            topology: primitive,
            cull_mode: self.cull_mode,
            winding: self.winding,
            fill_mode: self.fill_mode,
            depth_clip: self.depth_clip,
            depth: DepthStencilKey::from(&depth),
        };
        let pipeline = match state.variant(&key) {
            Ok(pipeline) => pipeline,
            Err(reason) => {
                gpu::trace(format!("render pipeline creation failed: {reason}"));
                return None;
            }
        };
        let command = self.command();
        let device = command.gpu.device.clone();
        let vk_command = command.command_buffer;
        command.retain(self.render_pipeline);
        // SAFETY: recording into this command buffer.
        unsafe { device.cmd_bind_pipeline(vk_command, vk::PipelineBindPoint::GRAPHICS, pipeline) };

        // Vertex streams are the buffer indices the vertex descriptor claims;
        // every other bound buffer is a descriptor resource.
        for stream in &state.vertex_streams {
            let bound = self.buffers[1][*stream as usize % SLOTS];
            if bound.object.is_null() {
                continue;
            }
            // SAFETY: only a buffer of this crate's is ever bound.
            let buffer = unsafe { object::body::<Buffer>(bound.object) };
            // SAFETY: recording into this command buffer.
            unsafe {
                device.cmd_bind_vertex_buffers(
                    vk_command,
                    *stream,
                    &[buffer.inner.buffer],
                    &[bound.offset.min(buffer.inner.length)],
                )
            };
            self.command().retain(bound.object);
        }
        let resources = state.resources.clone();
        let sets = state.layouts.sets;
        let layout = state.layouts.pipeline;
        self.bind_resources(&resources, &sets, layout, vk::PipelineBindPoint::GRAPHICS);
        gpu::milestone("draw");
        stats::count(Counter::Draws);
        Some((device, vk_command))
    }
}

unsafe extern "C" fn draw_primitives(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
    instance_count: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let Some((device, command)) = encoder.prepare_draw(primitive) else {
        return;
    };
    // SAFETY: recording into this command buffer.
    unsafe {
        device.cmd_draw(
            command,
            vertex_count as u32,
            instance_count.max(1) as u32,
            vertex_start as u32,
            0,
        )
    };
}

unsafe extern "C" fn draw_primitives_base(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
    instance_count: u64,
    base_instance: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let Some((device, command)) = encoder.prepare_draw(primitive) else {
        return;
    };
    // SAFETY: recording into this command buffer.
    unsafe {
        device.cmd_draw(
            command,
            vertex_count as u32,
            instance_count.max(1) as u32,
            vertex_start as u32,
            base_instance as u32,
        )
    };
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn draw_indexed_primitives(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    unsafe {
        draw_indexed(
            encoder,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            instance_count,
            0,
            0,
        )
    };
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn draw_indexed_primitives_base(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
    base_vertex: i64,
    base_instance: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    unsafe {
        draw_indexed(
            encoder,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            instance_count,
            base_vertex,
            base_instance,
        )
    };
}

/// # Safety
///
/// `index_buffer` must be null or an `MTLBuffer` of this crate's.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_indexed(
    encoder: &mut Encoder,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
    base_vertex: i64,
    base_instance: u64,
) {
    if index_buffer.is_null() {
        return;
    }
    let Some((device, command)) = encoder.prepare_draw(primitive) else {
        return;
    };
    // SAFETY: the caller guarantees the buffer's class.
    let buffer = unsafe { object::body::<Buffer>(index_buffer) };
    encoder.command().retain(index_buffer);
    // SAFETY: recording into this command buffer.
    unsafe {
        device.cmd_bind_index_buffer(
            command,
            buffer.inner.buffer,
            index_offset,
            match index_type {
                1 => vk::IndexType::UINT32,
                _ => vk::IndexType::UINT16,
            },
        );
        device.cmd_draw_indexed(
            command,
            index_count as u32,
            instance_count.max(1) as u32,
            0,
            base_vertex as i32,
            base_instance as u32,
        );
    };
}

// ------------------------------------------------------------------ blit

unsafe extern "C" fn copy_buffer_to_buffer(
    receiver: Id,
    _sel: Sel,
    source: Id,
    source_offset: u64,
    destination: Id,
    destination_offset: u64,
    size: u64,
) {
    if source.is_null() || destination.is_null() || size == 0 {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only buffers of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Buffer>(source),
            object::body::<Buffer>(destination),
        )
    };
    let region = vk::BufferCopy::default()
        .src_offset(source_offset)
        .dst_offset(destination_offset)
        .size(size);
    // SAFETY: recording into this command buffer.
    unsafe {
        command.gpu.device.cmd_copy_buffer(
            command.command_buffer,
            from.inner.buffer,
            to.inner.buffer,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn copy_buffer_to_texture(
    receiver: Id,
    _sel: Sel,
    source: Id,
    source_offset: u64,
    bytes_per_row: u64,
    bytes_per_image: u64,
    size: MtlSize,
    destination: Id,
    slice: u64,
    level: u64,
    origin: MtlOrigin,
) {
    if source.is_null() || destination.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only resources of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Buffer>(source),
            object::body::<Texture>(destination),
        )
    };
    let texel = u64::from(gpu::texel_size(to.image.format));
    let (block_width, block_height) = gpu::block_extent(to.image.format);
    let row_texels = if bytes_per_row == 0 {
        size.width
    } else {
        bytes_per_row / texel * u64::from(block_width)
    };
    let rows = if bytes_per_image == 0 || bytes_per_row == 0 {
        size.height
    } else {
        bytes_per_image / bytes_per_row * u64::from(block_height)
    };
    let region = vk::BufferImageCopy::default()
        .buffer_offset(source_offset)
        .buffer_row_length(row_texels as u32)
        .buffer_image_height(rows as u32)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: to.image.aspect,
            mip_level: level as u32,
            base_array_layer: slice as u32,
            layer_count: 1,
        })
        .image_offset(vk::Offset3D {
            x: origin.x as i32,
            y: origin.y as i32,
            z: origin.z as i32,
        })
        .image_extent(vk::Extent3D {
            width: size.width.max(1) as u32,
            height: size.height.max(1) as u32,
            depth: size.depth.max(1) as u32,
        });
    // SAFETY: recording into this command buffer; the image is in GENERAL.
    unsafe {
        command.gpu.device.cmd_copy_buffer_to_image(
            command.command_buffer,
            from.inner.buffer,
            to.image.image,
            vk::ImageLayout::GENERAL,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn copy_texture_to_buffer(
    receiver: Id,
    _sel: Sel,
    source: Id,
    slice: u64,
    level: u64,
    origin: MtlOrigin,
    size: MtlSize,
    destination: Id,
    destination_offset: u64,
    bytes_per_row: u64,
    bytes_per_image: u64,
) {
    if source.is_null() || destination.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only resources of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Texture>(source),
            object::body::<Buffer>(destination),
        )
    };
    let texel = u64::from(gpu::texel_size(from.image.format));
    let (block_width, block_height) = gpu::block_extent(from.image.format);
    let row_texels = if bytes_per_row == 0 {
        size.width
    } else {
        bytes_per_row / texel * u64::from(block_width)
    };
    let rows = if bytes_per_image == 0 || bytes_per_row == 0 {
        size.height
    } else {
        bytes_per_image / bytes_per_row * u64::from(block_height)
    };
    let region = vk::BufferImageCopy::default()
        .buffer_offset(destination_offset)
        .buffer_row_length(row_texels as u32)
        .buffer_image_height(rows as u32)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: from.image.aspect,
            mip_level: level as u32,
            base_array_layer: slice as u32,
            layer_count: 1,
        })
        .image_offset(vk::Offset3D {
            x: origin.x as i32,
            y: origin.y as i32,
            z: origin.z as i32,
        })
        .image_extent(vk::Extent3D {
            width: size.width.max(1) as u32,
            height: size.height.max(1) as u32,
            depth: size.depth.max(1) as u32,
        });
    // SAFETY: recording into this command buffer; the image is in GENERAL.
    unsafe {
        command.gpu.device.cmd_copy_image_to_buffer(
            command.command_buffer,
            from.image.image,
            vk::ImageLayout::GENERAL,
            to.inner.buffer,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn copy_texture_to_texture(
    receiver: Id,
    _sel: Sel,
    source: Id,
    source_slice: u64,
    source_level: u64,
    source_origin: MtlOrigin,
    size: MtlSize,
    destination: Id,
    destination_slice: u64,
    destination_level: u64,
    destination_origin: MtlOrigin,
) {
    if source.is_null() || destination.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only textures of this crate's reach a blit encoder.
    let (from, to) = unsafe {
        (
            object::body::<Texture>(source),
            object::body::<Texture>(destination),
        )
    };
    let region = vk::ImageCopy::default()
        .src_subresource(vk::ImageSubresourceLayers {
            aspect_mask: from.image.aspect,
            mip_level: source_level as u32,
            base_array_layer: source_slice as u32,
            layer_count: 1,
        })
        .src_offset(vk::Offset3D {
            x: source_origin.x as i32,
            y: source_origin.y as i32,
            z: source_origin.z as i32,
        })
        .dst_subresource(vk::ImageSubresourceLayers {
            aspect_mask: to.image.aspect,
            mip_level: destination_level as u32,
            base_array_layer: destination_slice as u32,
            layer_count: 1,
        })
        .dst_offset(vk::Offset3D {
            x: destination_origin.x as i32,
            y: destination_origin.y as i32,
            z: destination_origin.z as i32,
        })
        .extent(vk::Extent3D {
            width: size.width.max(1) as u32,
            height: size.height.max(1) as u32,
            depth: size.depth.max(1) as u32,
        });
    // SAFETY: recording into this command buffer; both images are in GENERAL.
    unsafe {
        command.gpu.device.cmd_copy_image(
            command.command_buffer,
            from.image.image,
            vk::ImageLayout::GENERAL,
            to.image.image,
            vk::ImageLayout::GENERAL,
            &[region],
        )
    };
    command.retain(source);
    command.retain(destination);
}

/// `-generateMipmapsForTexture:`: a chain of halving blits, which is what
/// Metal's own implementation does for a colour texture.
unsafe extern "C" fn generate_mipmaps(receiver: Id, _sel: Sel, texture: Id) {
    if texture.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let command = encoder.command();
    // SAFETY: only a texture of this crate's reaches a blit encoder.
    let image = unsafe { object::body::<Texture>(texture) };
    let device = command.gpu.device.clone();
    let vk_command = command.command_buffer;
    let mut width = image.image.width as i32;
    let mut height = image.image.height as i32;
    for level in 1..image.image.levels {
        let next_width = (width / 2).max(1);
        let next_height = (height / 2).max(1);
        let blit = vk::ImageBlit::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: image.image.aspect,
                mip_level: level - 1,
                base_array_layer: 0,
                layer_count: image.image.layers,
            })
            .src_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: width,
                    y: height,
                    z: 1,
                },
            ])
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: image.image.aspect,
                mip_level: level,
                base_array_layer: 0,
                layer_count: image.image.layers,
            })
            .dst_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: next_width,
                    y: next_height,
                    z: 1,
                },
            ]);
        // SAFETY: recording into this command buffer; the image is in GENERAL.
        unsafe {
            device.cmd_blit_image(
                vk_command,
                image.image.image,
                vk::ImageLayout::GENERAL,
                image.image.image,
                vk::ImageLayout::GENERAL,
                &[blit],
                vk::Filter::LINEAR,
            )
        };
        encoder.barrier();
        width = next_width;
        height = next_height;
    }
    command.retain(texture);
}

// ------------------------------------------------------------ render pass

/// One `MTLRenderPassAttachmentDescriptor`.
#[derive(Clone, Copy)]
pub(crate) struct AttachmentDescriptor {
    pub texture: Id,
    pub level: u64,
    pub slice: u64,
    pub load_action: u64,
    pub store_action: u64,
    pub clear_color: [f64; 4],
    pub clear_depth: f64,
    pub clear_stencil: u32,
    pub resolve_texture: Id,
}

impl Default for AttachmentDescriptor {
    fn default() -> Self {
        Self {
            texture: std::ptr::null_mut(),
            level: 0,
            slice: 0,
            load_action: 0,
            store_action: 0,
            clear_color: [0.0; 4],
            clear_depth: 1.0,
            clear_stencil: 0,
            resolve_texture: std::ptr::null_mut(),
        }
    }
}

pub(crate) const RENDER_TARGETS: usize = 8;

pub(crate) struct RenderPassDescriptor {
    pub colors: [AttachmentDescriptor; RENDER_TARGETS],
    pub depth: AttachmentDescriptor,
    pub stencil: AttachmentDescriptor,
    pub color_objects: Id,
    pub depth_object: Id,
    pub stencil_object: Id,
    /// FIXME (ACCURACY): kept so that a guest reading the property back sees
    /// what it set, and never written to. Occlusion queries are not
    /// implemented, so a guest which reads the counts finds whatever the
    /// buffer already held.
    pub visibility_result_buffer: Id,
}

impl Default for RenderPassDescriptor {
    fn default() -> Self {
        Self {
            colors: [AttachmentDescriptor::default(); RENDER_TARGETS],
            depth: AttachmentDescriptor::default(),
            stencil: AttachmentDescriptor::default(),
            color_objects: std::ptr::null_mut(),
            depth_object: std::ptr::null_mut(),
            stencil_object: std::ptr::null_mut(),
            visibility_result_buffer: std::ptr::null_mut(),
        }
    }
}

/// `ROSETTA3_TRACE_PASSES=1`: each distinct render pass shape, once.
///
/// A frame's device time is spent in its passes, and a log which says only how
/// many there were cannot say which one to look at. The size, the sample count
/// and the load and store actions are what decide the cost of a pass before any
/// shader runs.
fn trace_pass(key: &RenderPassKey, width: u32, height: u32) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    if std::env::var_os("ROSETTA3_TRACE_PASSES").is_none() {
        return;
    }
    let line = format!(
        "render pass {width}x{height} samples {:?} colors {:?} depth {:?}",
        key.samples, key.colors, key.depth
    );
    let mut seen = SEEN.lock().expect("render pass trace");
    if !seen.contains(&line) {
        seen.push(line.clone());
        gpu::trace(line);
    }
}

/// One dimension of a mip level, which is the level-zero one halved that many
/// times and never zero.
fn level_extent(base: u32, level: u32) -> u32 {
    (base >> level.min(31)).max(1)
}

fn load_op(metal: u64) -> vk::AttachmentLoadOp {
    match metal {
        1 => vk::AttachmentLoadOp::LOAD,
        2 => vk::AttachmentLoadOp::CLEAR,
        _ => vk::AttachmentLoadOp::DONT_CARE,
    }
}

fn store_op(metal: u64) -> vk::AttachmentStoreOp {
    match metal {
        0 => vk::AttachmentStoreOp::DONT_CARE,
        _ => vk::AttachmentStoreOp::STORE,
    }
}

/// Begin the Vulkan render pass one `MTLRenderPassDescriptor` describes.
pub(crate) fn begin_render_pass(encoder_object: Id, descriptor: &RenderPassDescriptor) {
    // SAFETY: an encoder this crate just made.
    let encoder = unsafe { object::body::<Encoder>(encoder_object) };
    let command = encoder.command();
    let gpu = command.gpu;

    let mut key = RenderPassKey {
        colors: Vec::new(),
        depth: None,
        stencil: None,
        samples: vk::SampleCountFlags::TYPE_1,
    };
    let mut views = Vec::new();
    let mut clears = Vec::new();
    let mut width = u32::MAX;
    let mut height = u32::MAX;
    let mut textures = Vec::new();

    for color in &descriptor.colors {
        if color.texture.is_null() {
            continue;
        }
        // SAFETY: only a texture of this crate's is attached.
        let texture = unsafe { object::body::<Texture>(color.texture) };
        key.colors.push((
            texture.image.format,
            load_op(color.load_action),
            store_op(color.store_action),
        ));
        key.samples = texture.image.samples;
        let level = color.level as u32;
        views.push(texture.attachment_view(level, color.slice as u32));
        clears.push(vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [
                    color.clear_color[0] as f32,
                    color.clear_color[1] as f32,
                    color.clear_color[2] as f32,
                    color.clear_color[3] as f32,
                ],
            },
        });
        width = width.min(level_extent(texture.image.width, level));
        height = height.min(level_extent(texture.image.height, level));
        textures.push(color.texture);
    }
    let depth_attachment = if !descriptor.depth.texture.is_null() {
        Some(&descriptor.depth)
    } else if !descriptor.stencil.texture.is_null() {
        Some(&descriptor.stencil)
    } else {
        None
    };
    if let Some(attachment) = depth_attachment {
        // SAFETY: only a texture of this crate's is attached.
        let texture = unsafe { object::body::<Texture>(attachment.texture) };
        key.depth = Some((
            texture.image.format,
            load_op(attachment.load_action),
            store_op(attachment.store_action),
        ));
        if !descriptor.stencil.texture.is_null() {
            key.stencil = Some((
                load_op(descriptor.stencil.load_action),
                store_op(descriptor.stencil.store_action),
            ));
        }
        key.samples = texture.image.samples;
        let level = attachment.level as u32;
        views.push(texture.attachment_view(level, attachment.slice as u32));
        clears.push(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: descriptor.depth.clear_depth as f32,
                stencil: descriptor.stencil.clear_stencil,
            },
        });
        width = width.min(level_extent(texture.image.width, level));
        height = height.min(level_extent(texture.image.height, level));
        textures.push(attachment.texture);
    }
    if views.is_empty() {
        gpu::trace("renderCommandEncoderWithDescriptor: has no attachments");
        return;
    }
    let width = width.max(1);
    let height = height.max(1);

    let pass = match gpu.render_pass(&key) {
        Ok(pass) => pass,
        Err(reason) => return gpu::trace(format!("render pass creation failed: {reason}")),
    };
    let framebuffer = match gpu.create_framebuffer(pass, &views, width, height, 1) {
        Ok(framebuffer) => framebuffer,
        Err(reason) => return gpu::trace(format!("framebuffer creation failed: {reason}")),
    };
    for texture in textures {
        command.retain(texture);
    }
    command.framebuffers.push(framebuffer);
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width, height },
    };
    trace_pass(&key, width, height);
    let begin = vk::RenderPassBeginInfo::default()
        .render_pass(pass)
        .framebuffer(framebuffer)
        .render_area(area)
        .clear_values(&clears);
    // SAFETY: recording into this command buffer.
    unsafe {
        gpu.device.cmd_begin_render_pass(
            command.command_buffer,
            &begin,
            vk::SubpassContents::INLINE,
        );
        // Metal's default viewport and scissor cover the attachments, and the
        // viewport is flipped for the same reason `-setViewport:` flips.
        gpu.device.cmd_set_viewport(
            command.command_buffer,
            0,
            &[vk::Viewport {
                x: 0.0,
                y: height as f32,
                width: width as f32,
                height: -(height as f32),
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        gpu.device
            .cmd_set_scissor(command.command_buffer, 0, &[area]);
        gpu.device
            .cmd_set_depth_bias(command.command_buffer, 0.0, 0.0, 0.0);
        gpu.device
            .cmd_set_blend_constants(command.command_buffer, &[0.0; 4]);
        gpu.device.cmd_set_stencil_reference(
            command.command_buffer,
            vk::StencilFaceFlags::FRONT_AND_BACK,
            0,
        );
    };
    gpu::milestone("render pass");
    encoder.in_render_pass = true;
}

// ------------------------------------- MTLRenderPassDescriptor and encoders

unsafe extern "C" fn render_pass_descriptor_init(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a freshly allocated render pass descriptor.
    *unsafe { object::body::<RenderPassDescriptor>(receiver) } = RenderPassDescriptor::default();
    receiver
}

unsafe extern "C" fn pass_visibility_result_buffer(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's.
    unsafe { object::body::<RenderPassDescriptor>(receiver) }.visibility_result_buffer
}

unsafe extern "C" fn set_pass_visibility_result_buffer(receiver: Id, _sel: Sel, buffer: Id) {
    // SAFETY: as above.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    let previous = std::mem::replace(
        &mut descriptor.visibility_result_buffer,
        objc::arc::retain(buffer),
    );
    objc::arc::release(previous);
}

/// `+[MTLRenderPassDescriptor renderPassDescriptor]`.
unsafe extern "C" fn new_render_pass_descriptor(_class: Id, _sel: Sel) -> Id {
    let object = object::make(c"MTLRenderPassDescriptor", RenderPassDescriptor::default());
    // SAFETY: a fresh object this call owns.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

/// Which attachment of a render pass descriptor one element names.
#[derive(Clone, Copy)]
pub(crate) struct AttachmentElement {
    pub owner: Id,
    pub slot: AttachmentSlot,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum AttachmentSlot {
    Color(usize),
    Depth,
    Stencil,
}

fn attachment_of<'a>(element: Id) -> &'a mut AttachmentDescriptor {
    // SAFETY: an attachment element of this crate's.
    let element = *unsafe { object::body::<AttachmentElement>(element) };
    // SAFETY: the element holds the descriptor which made it.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(element.owner) };
    match element.slot {
        AttachmentSlot::Color(index) => &mut descriptor.colors[index],
        AttachmentSlot::Depth => &mut descriptor.depth,
        AttachmentSlot::Stencil => &mut descriptor.stencil,
    }
}

unsafe extern "C" fn pass_color_attachments(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    if !descriptor.color_objects.is_null() {
        return descriptor.color_objects;
    }
    let elements = (0..RENDER_TARGETS)
        .map(|index| {
            object::make(
                c"R3MTLRenderPassAttachmentDescriptor",
                AttachmentElement {
                    owner: receiver,
                    slot: AttachmentSlot::Color(index),
                },
            )
        })
        .collect::<Vec<_>>();
    let collection = object::make(
        c"R3MTLIndexedCollection",
        crate::resource::IndexedCollection { elements },
    );
    // SAFETY: as above.
    unsafe { object::body::<RenderPassDescriptor>(receiver) }.color_objects = collection;
    collection
}

unsafe extern "C" fn pass_single_attachment(receiver: Id, sel: Sel) -> Id {
    // SAFETY: a render pass descriptor of this crate's.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    // SAFETY: a selector the runtime owns.
    let depth = unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() == b"depthAttachment";
    let existing = if depth {
        descriptor.depth_object
    } else {
        descriptor.stencil_object
    };
    if !existing.is_null() {
        return existing;
    }
    let element = object::make(
        c"R3MTLRenderPassAttachmentDescriptor",
        AttachmentElement {
            owner: receiver,
            slot: if depth {
                AttachmentSlot::Depth
            } else {
                AttachmentSlot::Stencil
            },
        },
    );
    // SAFETY: as above.
    let descriptor = unsafe { object::body::<RenderPassDescriptor>(receiver) };
    if depth {
        descriptor.depth_object = element;
    } else {
        descriptor.stencil_object = element;
    }
    element
}

unsafe extern "C" fn set_attachment_object(receiver: Id, sel: Sel, value: Id) {
    let attachment = attachment_of(receiver);
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setTexture:" => attachment.texture = objc::arc::retain(value),
        b"setResolveTexture:" => attachment.resolve_texture = objc::arc::retain(value),
        _ => (),
    }
}

unsafe extern "C" fn set_attachment_number(receiver: Id, sel: Sel, value: u64) {
    let attachment = attachment_of(receiver);
    // SAFETY: a selector the runtime owns.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"setLevel:" => attachment.level = value,
        b"setSlice:" => attachment.slice = value,
        b"setLoadAction:" => attachment.load_action = value,
        b"setStoreAction:" => attachment.store_action = value,
        b"setClearStencil:" => attachment.clear_stencil = value as u32,
        b"setDepthPlane:" | b"setResolveLevel:" | b"setResolveSlice:" => (),
        _ => (),
    }
}

unsafe extern "C" fn set_attachment_clear_color(receiver: Id, _sel: Sel, color: ClearColor) {
    attachment_of(receiver).clear_color = [color.red, color.green, color.blue, color.alpha];
}

unsafe extern "C" fn set_attachment_clear_depth(receiver: Id, _sel: Sel, depth: f64) {
    attachment_of(receiver).clear_depth = depth;
}

unsafe extern "C" fn new_compute_encoder(receiver: Id, _sel: Sel) -> Id {
    new_encoder(receiver, Kind::Compute)
}

unsafe extern "C" fn new_blit_encoder(receiver: Id, _sel: Sel) -> Id {
    new_encoder(receiver, Kind::Blit)
}

unsafe extern "C" fn new_render_encoder(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    let encoder = new_encoder(receiver, Kind::Render);
    if encoder.is_null() {
        return encoder;
    }
    // SAFETY: `MTLRenderPassDescriptor` is this crate's class.
    let described = unsafe { object::body::<RenderPassDescriptor>(descriptor) };
    begin_render_pass(encoder, described);
    encoder
}

pub(crate) fn install() {
    object::register::<CommandQueue>(c"R3MTLCommandQueue");
    object::register::<CommandBuffer>(c"R3MTLCommandBuffer");
    object::register::<Encoder>(c"R3MTLCommandEncoder");

    let queue = objc::class::lookup(c"R3MTLCommandQueue").expect("just registered");
    methods!(queue, {
        "device" => queue_device, c"@16@0:8";
        "commandBuffer" => new_command_buffer, c"@16@0:8";
        "commandBufferWithUnretainedReferences" => new_command_buffer, c"@16@0:8";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });

    let command = objc::class::lookup(c"R3MTLCommandBuffer").expect("just registered");
    methods!(command, {
        "commandQueue" => command_buffer_queue, c"@16@0:8";
        "status" => command_buffer_status, c"Q16@0:8";
        "addScheduledHandler:" => add_handler, c"v24@0:8@?16";
        "addCompletedHandler:" => add_handler, c"v24@0:8@?16";
        "commit" => commit, c"v16@0:8";
        "waitUntilCompleted" => crate::no_op, c"v16@0:8";
        "waitUntilScheduled" => crate::no_op, c"v16@0:8";
        "enqueue" => crate::no_op, c"v16@0:8";
        "presentDrawable:" => present_drawable, c"v24@0:8@16";
        "presentDrawable:atTime:" => present_drawable_at_time, c"v32@0:8@16d24";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
    });

    let encoder = objc::class::lookup(c"R3MTLCommandEncoder").expect("just registered");
    methods!(encoder, {
        "endEncoding" => end_encoding, c"v16@0:8";
        "setComputePipelineState:" => set_compute_pipeline, c"v24@0:8@16";
        "setRenderPipelineState:" => set_render_pipeline, c"v24@0:8@16";
        "setDepthStencilState:" => set_depth_stencil_state, c"v24@0:8@16";
        "setStencilReferenceValue:" => set_stencil_reference, c"v20@0:8I16";
        "setViewport:" => set_viewport, c"v64@0:8{MTLViewport=dddddd}16";
        "setScissorRect:" => set_scissor, c"v48@0:8{MTLScissorRect=QQQQ}16";
        "setDepthBias:slopeScale:clamp:" => set_depth_bias, c"v28@0:8f16f20f24";
        "setBlendColorRed:green:blue:alpha:" => set_blend_color, c"v32@0:8f16f20f24f28";
        "dispatchThreadgroups:threadsPerThreadgroup:" => dispatch_threadgroups,
            c"v64@0:8{MTLSize=QQQ}16{MTLSize=QQQ}40";
        "dispatchThreads:threadsPerThreadgroup:" => dispatch_threadgroups,
            c"v64@0:8{MTLSize=QQQ}16{MTLSize=QQQ}40";
        "drawPrimitives:vertexStart:vertexCount:instanceCount:" => draw_primitives,
            c"v48@0:8Q16Q24Q32Q40";
        "drawPrimitives:vertexStart:vertexCount:instanceCount:baseInstance:"
            => draw_primitives_base, c"v56@0:8Q16Q24Q32Q40Q48";
        "drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:"
            => draw_indexed_primitives, c"v64@0:8Q16Q24Q32@40Q48Q56";
        "drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:baseVertex:baseInstance:"
            => draw_indexed_primitives_base, c"v80@0:8Q16Q24Q32@40Q48Q56q64Q72";
        "copyFromBuffer:sourceOffset:toBuffer:destinationOffset:size:"
            => copy_buffer_to_buffer, c"v56@0:8@16Q24@32Q40Q48";
        "copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:"
            => copy_buffer_to_texture, c"v120@0:8@16Q24Q32Q40{MTLSize=QQQ}48@72Q80Q88{MTLOrigin=QQQ}96";
        "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toBuffer:destinationOffset:destinationBytesPerRow:destinationBytesPerImage:"
            => copy_texture_to_buffer, c"v120@0:8@16Q24Q32{MTLOrigin=QQQ}40{MTLSize=QQQ}64@88Q96Q104Q112";
        "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:"
            => copy_texture_to_texture, c"v144@0:8@16Q24Q32{MTLOrigin=QQQ}40{MTLSize=QQQ}64@88Q96Q104{MTLOrigin=QQQ}112";
        "generateMipmapsForTexture:" => generate_mipmaps, c"v24@0:8@16";
        "synchronizeResource:" => crate::no_op, c"v24@0:8@16";
        "synchronizeTexture:slice:level:" => crate::no_op, c"v40@0:8@16Q24Q32";
        "updateFence:" => crate::no_op, c"v24@0:8@16";
        "waitForFence:" => crate::no_op, c"v24@0:8@16";
        "pushDebugGroup:" => crate::no_op, c"v24@0:8@16";
        "popDebugGroup" => crate::no_op, c"v16@0:8";
        "insertDebugSignpost:" => crate::no_op, c"v24@0:8@16";
        "setLabel:" => crate::no_op, c"v24@0:8@16";
        "useResource:usage:" => crate::no_op, c"v32@0:8@16Q24";
        "setVisibilityResultMode:offset:" => crate::no_op, c"v32@0:8Q16Q24";
    });

    let pass = object::register::<RenderPassDescriptor>(c"MTLRenderPassDescriptor");
    let attachment = object::register::<AttachmentElement>(c"R3MTLRenderPassAttachmentDescriptor");
    let metaclass = pass.metaclass();
    metaclass.replace_method(
        objc::sel::get("renderPassDescriptor"),
        new_render_pass_descriptor as *const std::ffi::c_void,
        c"@16@0:8",
    );
    methods!(pass, {
        "init" => render_pass_descriptor_init, c"@16@0:8";
        "colorAttachments" => pass_color_attachments, c"@16@0:8";
        "depthAttachment" => pass_single_attachment, c"@16@0:8";
        "stencilAttachment" => pass_single_attachment, c"@16@0:8";
        "visibilityResultBuffer" => pass_visibility_result_buffer, c"@16@0:8";
        "setVisibilityResultBuffer:" => set_pass_visibility_result_buffer, c"v24@0:8@16";
        "setRenderTargetWidth:" => crate::no_op, c"v24@0:8Q16";
        "setRenderTargetHeight:" => crate::no_op, c"v24@0:8Q16";
        "setDefaultRasterSampleCount:" => crate::no_op, c"v24@0:8Q16";
    });
    methods!(attachment, {
        "setClearColor:" => set_attachment_clear_color, c"v48@0:8{MTLClearColor=dddd}16";
        "setClearDepth:" => set_attachment_clear_depth, c"v24@0:8d16";
    });
    for selector in ["setTexture:", "setResolveTexture:"] {
        attachment.replace_method(
            objc::sel::get(selector),
            set_attachment_object as *const std::ffi::c_void,
            c"v24@0:8@16",
        );
    }
    for selector in [
        "setLevel:",
        "setSlice:",
        "setDepthPlane:",
        "setLoadAction:",
        "setStoreAction:",
        "setClearStencil:",
        "setResolveLevel:",
        "setResolveSlice:",
    ] {
        attachment.replace_method(
            objc::sel::get(selector),
            set_attachment_number as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }

    methods!(command, {
        "computeCommandEncoder" => new_compute_encoder, c"@16@0:8";
        "blitCommandEncoder" => new_blit_encoder, c"@16@0:8";
        "renderCommandEncoderWithDescriptor:" => new_render_encoder, c"@24@0:8@16";
    });
    for selector in [
        "setBuffer:offset:atIndex:",
        "setVertexBuffer:offset:atIndex:",
        "setFragmentBuffer:offset:atIndex:",
    ] {
        encoder.replace_method(
            objc::sel::get(selector),
            set_buffer as *const std::ffi::c_void,
            c"v40@0:8@16Q24Q32",
        );
    }
    for selector in [
        "setBytes:length:atIndex:",
        "setVertexBytes:length:atIndex:",
        "setFragmentBytes:length:atIndex:",
    ] {
        encoder.replace_method(
            objc::sel::get(selector),
            set_bytes as *const std::ffi::c_void,
            c"v40@0:8r^v16Q24Q32",
        );
    }
    for selector in [
        "setBufferOffset:atIndex:",
        "setVertexBufferOffset:atIndex:",
        "setFragmentBufferOffset:atIndex:",
    ] {
        encoder.replace_method(
            objc::sel::get(selector),
            set_buffer_offset as *const std::ffi::c_void,
            c"v32@0:8Q16Q24",
        );
    }
    for selector in [
        "setTexture:atIndex:",
        "setVertexTexture:atIndex:",
        "setFragmentTexture:atIndex:",
    ] {
        encoder.replace_method(
            objc::sel::get(selector),
            set_texture as *const std::ffi::c_void,
            c"v32@0:8@16Q24",
        );
    }
    for selector in [
        "setSamplerState:atIndex:",
        "setVertexSamplerState:atIndex:",
        "setFragmentSamplerState:atIndex:",
    ] {
        encoder.replace_method(
            objc::sel::get(selector),
            set_sampler as *const std::ffi::c_void,
            c"v32@0:8@16Q24",
        );
    }
    for selector in [
        "setCullMode:",
        "setFrontFacingWinding:",
        "setTriangleFillMode:",
        "setDepthClipMode:",
    ] {
        encoder.replace_method(
            objc::sel::get(selector),
            set_raster_value as *const std::ffi::c_void,
            c"v24@0:8Q16",
        );
    }
}
