//! Retiring a submission once the GPU has finished with it.

use super::*;

pub(super) struct CommandCompletion {
    state: Mutex<CommandState>,
    changed: Condvar,
}

impl CommandCompletion {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(CommandState::default()),
            changed: Condvar::new(),
        }
    }

    pub(super) fn get(&self) -> u64 {
        self.state.lock().expect("Metal command state").status
    }

    pub(super) fn set(&self, status: u64) {
        self.state.lock().expect("Metal command state").status = status;
        self.changed.notify_all();
    }

    pub(super) fn finish(&self, status: u64, gpu_times: Option<(f64, f64)>) {
        let mut state = self.state.lock().expect("Metal command state");
        if let Some((start, end)) = gpu_times {
            state.gpu_start_time = start;
            state.gpu_end_time = end;
        }
        state.status = status;
        self.changed.notify_all();
    }

    pub(super) fn gpu_time(&self, end: bool) -> f64 {
        let state = self.state.lock().expect("Metal command state");
        if end {
            state.gpu_end_time
        } else {
            state.gpu_start_time
        }
    }

    pub(super) fn wait_for(&self, predicate: impl Fn(u64) -> bool) {
        let mut state = self.state.lock().expect("Metal command state");
        while !predicate(state.status) {
            state = self.changed.wait(state).expect("Metal command state");
        }
    }
}

#[derive(Default)]
pub(super) struct CommandState {
    status: u64,
    gpu_start_time: f64,
    gpu_end_time: f64,
}

/// Everything whose lifetime is tied to one Vulkan submission.
pub(crate) struct Submission {
    pub(super) gpu: &'static Gpu,
    pub(super) timestamps: vk::QueryPool,
    pub(super) pool: vk::CommandPool,
    pub(super) command_buffer: vk::CommandBuffer,
    pub(super) fence: vk::Fence,
    pub(super) descriptor_pools: Vec<vk::DescriptorPool>,
    pub(super) retained: Vec<usize>,
    pub(super) uploads: Vec<GpuBuffer>,
    pub(super) framebuffers: Vec<vk::Framebuffer>,
    pub(super) captures: Vec<crate::capture::Capture>,
    pub(super) completed_handlers: Vec<usize>,
    pub(super) presents: Vec<usize>,
    pub(super) object: usize,
    pub(super) completion: Arc<CommandCompletion>,
    /// Host time when Vulkan accepted this submission, used if this queue has
    /// no device timestamps or reading them fails.
    pub(super) scheduled_time: f64,
}

impl Submission {
    pub(crate) fn wait_and_finish(self) {
        let result = self.gpu.wait(self.fence);
        self.finish(result);
    }

    pub(super) fn finish(mut self, result: Result<(), String>) {
        if let Err(reason) = &result {
            gpu::trace(format!("command buffer completion failed: {reason}"));
        }
        let mut gpu_times = None;
        if result.is_ok() && self.timestamps != vk::QueryPool::null() {
            let mut ticks = [0u64; 2];
            // SAFETY: the submission completed, so both queries have results.
            let read = unsafe {
                self.gpu.device.get_query_pool_results(
                    self.timestamps,
                    0,
                    &mut ticks,
                    vk::QueryResultFlags::TYPE_64,
                )
            };
            if read.is_ok() {
                let elapsed = timestamp_delta(ticks[0], ticks[1], self.gpu.timestamp_valid_bits())
                    as f64
                    * self.gpu.timestamp_period()
                    / 1e9;
                let end = darwin::mach::media_time();
                gpu_times = Some((end - elapsed, end));
                if stats::enabled() {
                    stats::add(Counter::GpuMicros, (elapsed * 1e6) as u64);
                }
            }
        }
        if result.is_ok() && gpu_times.is_none() {
            let end = darwin::mach::media_time();
            gpu_times = Some((self.scheduled_time.min(end), end));
        }
        self.gpu.release_timestamp_pool(self.timestamps);
        for drawable in self.presents.drain(..).map(|drawable| drawable as Id) {
            crate::present::present(drawable);
            objc::arc::release(drawable);
        }
        // The work has completed, so everything the recording referred to and every
        // transient object it created can go.
        crate::capture::finish(self.gpu, std::mem::take(&mut self.captures));
        for framebuffer in self.framebuffers.drain(..) {
            // SAFETY: the submission finished before this point.
            unsafe { self.gpu.device.destroy_framebuffer(framebuffer, None) };
        }
        for pool in self.descriptor_pools.drain(..) {
            // The sets allocated from it are no longer in use.
            self.gpu.release_descriptor_pool(pool);
        }
        for chunk in self.uploads.drain(..) {
            self.gpu.release_upload_chunk(chunk);
        }
        for object in self.retained.drain(..).map(|object| object as Id) {
            objc::arc::release(object);
        }
        // The submission finished, or never reached the queue.
        self.gpu.release_context(CommandContext {
            pool: self.pool,
            command_buffer: self.command_buffer,
            fence: self.fence,
        });
        self.completion
            .finish(if result.is_ok() { 4 } else { 5 }, gpu_times);
        let object = self.object as Id;
        for block in self.completed_handlers.drain(..).map(|block| block as Id) {
            // SAFETY: `objc_retainBlock` returned a live block, whose reference this
            // gives away, and `object` is retained by this submission.
            unsafe { crate::completion::deliver(block, object) };
        }
        objc::arc::release(object);
    }
}

pub(super) fn timestamp_delta(start: u64, end: u64, valid_bits: u32) -> u64 {
    let mask = match valid_bits {
        64.. => u64::MAX,
        bits => (1u64 << bits) - 1,
    };
    end.wrapping_sub(start) & mask
}
