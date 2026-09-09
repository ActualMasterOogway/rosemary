//! Where a frame's time goes.
//!
//! A runtime which is slow and a runtime which is doing too much work look the
//! same from outside, so the counters a frame moves are collected here and
//! reported once a second under `ROSETTA3_METAL_STATS=1`. Nothing here is on a
//! path a correct run depends on: every counter is a relaxed atomic and the
//! report is written by whichever thread presents the frame which crosses the
//! interval.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

macro_rules! counters {
    ($($name:ident = $label:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum Counter { $($name),* }
        const COUNTERS: usize = [$(Counter::$name),*].len();
        const NAMES: [&str; COUNTERS] = [$($label),*];
    };
}

counters!(
    CommandBuffers = "command_buffers",
    Draws = "draws",
    Dispatches = "dispatches",
    Blits = "blits",
    Encoders = "encoders",
    DescriptorSets = "descriptor_sets",
    BuffersCreated = "buffers_created",
    BufferBytes = "buffer_bytes",
    SetBytesCalls = "set_bytes_calls",
    SetBytesBytes = "set_bytes_bytes",
    TexturesCreated = "textures_created",
    PipelinesCreated = "pipelines_created",
    SubmitMicros = "submit_micros",
    PresentMicros = "present_micros",
    ReadbackMicros = "readback_micros",
    BlitMicros = "blit_micros",
    Frames = "frames",
    BufferMicros = "buffer_micros",
    BufferPeakBytes = "buffer_peak_bytes",
    BufferZeroMicros = "buffer_zero_micros",
    BufferBlockMicros = "buffer_block_micros",
    BindMicros = "bind_micros",
    PipelineMicros = "pipeline_micros",
    QueueMicros = "queue_micros",
    GpuMicros = "gpu_micros",
);

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static VALUES: [AtomicU64; COUNTERS] = [ZERO; COUNTERS];

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ROSETTA3_METAL_STATS").is_some_and(|value| value != "0"))
}

pub fn add(counter: Counter, amount: u64) {
    if enabled() {
        VALUES[counter as usize].fetch_add(amount, Ordering::Relaxed);
    }
}

pub fn count(counter: Counter) {
    add(counter, 1);
}

/// Keep the largest value a counter has been offered this interval.
pub fn peak(counter: Counter, amount: u64) {
    if enabled() {
        VALUES[counter as usize].fetch_max(amount, Ordering::Relaxed);
    }
}

/// Time `body`, adding its duration in microseconds to `counter`.
pub fn timed<T>(counter: Counter, body: impl FnOnce() -> T) -> T {
    if !enabled() {
        return body();
    }
    let start = Instant::now();
    let value = body();
    add(counter, start.elapsed().as_micros() as u64);
    value
}

/// Report and reset the counters, once per interval.
pub fn tick() {
    if !enabled() {
        return;
    }
    static SINCE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let mut since = SINCE.lock().expect("statistics interval");
    let start = *since.get_or_insert_with(Instant::now);
    let elapsed = start.elapsed();
    if elapsed < Duration::from_secs(1) {
        return;
    }
    *since = Some(Instant::now());
    let seconds = elapsed.as_secs_f64();
    let line = NAMES
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let value = VALUES[index].swap(0, Ordering::Relaxed);
            if value == 0 {
                return None;
            }
            Some(match name.ends_with("_peak_bytes") {
                true => format!("{name}={value}"),
                false => format!("{name}={:.0}/s", value as f64 / seconds),
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("rosetta3: Metal: stats {line}");
}
