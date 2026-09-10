//! `ROSETTA3_PASS_OUT`: the texels every render pass left in its attachments.
//!
//! A frame that comes out wrong says only that some pass in it is wrong, and a
//! frame is twenty-five of them writing half-resolution intermediates nothing
//! ever presents. The log cannot say which one, and neither can the presented
//! frame: by then every pass has run. So each pass's attachments are copied
//! into a host buffer inside the same recording, and written out once the
//! submission completes.
//!
//! Capture is armed by creating a file named `go` in the directory, and stops
//! after `ROSETTA3_PASS_OUT_COUNT` attachments; a run which captured every pass
//! of every frame would write gigabytes a second. Texels are written raw, with
//! the size and the Vulkan format in the file name, because the formats a game
//! renders into - `A2B10G10R10`, `B10G11R11`, half floats - have no image file
//! that carries them and decoding belongs outside a hot path.

use crate::gpu::{Gpu, GpuBuffer};
use ash::vk;
use std::sync::atomic::{AtomicU64, Ordering};

/// One attachment of one pass, named while the pass is recorded and read back
/// once the submission that wrote it completes.
pub(crate) struct Capture {
    pub buffer: GpuBuffer,
    pub name: String,
}

// SAFETY: the mapping a `GpuBuffer` holds is valid on any thread, and a
// capture is only ever read on the thread that retires its submission.
unsafe impl Send for Capture {}

/// What `begin_render_pass` saw, kept on the encoder until the pass ends.
pub(crate) struct Target {
    pub image: vk::Image,
    pub aspect: vk::ImageAspectFlags,
    pub level: u32,
    pub slice: u32,
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
    pub samples: vk::SampleCountFlags,
    /// `c0`, `r0`, `d`: which attachment of the pass this is.
    pub role: String,
}

fn directory() -> Option<&'static std::path::Path> {
    static DIRECTORY: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIRECTORY
        .get_or_init(|| std::env::var_os("ROSETTA3_PASS_OUT").map(std::path::PathBuf::from))
        .as_deref()
}

/// Whether this pass should be captured, and the serial number to name it by.
///
/// The trigger file is removed as soon as it is seen, so one `touch` captures
/// one burst however long the run continues.
pub(crate) fn claim() -> Option<u64> {
    static REMAINING: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let directory = directory()?;
    let mut remaining = REMAINING.lock().unwrap_or_else(|error| error.into_inner());
    if *remaining == 0 {
        let trigger = directory.join("go");
        if !trigger.exists() {
            return None;
        }
        let _ = std::fs::remove_file(&trigger);
        *remaining = std::env::var("ROSETTA3_PASS_OUT_COUNT")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(64);
    }
    *remaining -= 1;
    Some(SERIAL.fetch_add(1, Ordering::Relaxed))
}

/// Bytes one texel of `format` occupies, for the formats a pass renders into.
fn texel_bytes(format: vk::Format) -> Option<u32> {
    Some(match format {
        vk::Format::R8_UNORM => 1,
        vk::Format::R16_SFLOAT | vk::Format::R16_UNORM | vk::Format::R8G8_UNORM => 2,
        vk::Format::R16G16_SFLOAT
        | vk::Format::R16G16_UNORM
        | vk::Format::R8G8B8A8_UNORM
        | vk::Format::R8G8B8A8_SRGB
        | vk::Format::B8G8R8A8_UNORM
        | vk::Format::B8G8R8A8_SRGB
        | vk::Format::A2B10G10R10_UNORM_PACK32
        | vk::Format::B10G11R11_UFLOAT_PACK32
        | vk::Format::D32_SFLOAT
        | vk::Format::R32_SFLOAT
        | vk::Format::R32_SINT
        | vk::Format::R32_UINT => 4,
        vk::Format::R16G16B16A16_SFLOAT | vk::Format::R32G32_SFLOAT => 8,
        vk::Format::R32G32B32A32_SFLOAT => 16,
        _ => return None,
    })
}

/// Record the copy of one attachment into a fresh host buffer.
pub(crate) fn record(
    gpu: &'static Gpu,
    command_buffer: vk::CommandBuffer,
    pass: u64,
    when: &str,
    target: &Target,
) -> Option<Capture> {
    if target.samples != vk::SampleCountFlags::TYPE_1 {
        return None;
    }
    let bytes = texel_bytes(target.format)?;
    let length = u64::from(bytes) * u64::from(target.width) * u64::from(target.height);
    let buffer = gpu.create_buffer(length).ok()?;
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(target.width)
        .buffer_image_height(target.height)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: target.aspect,
            mip_level: target.level,
            base_array_layer: target.slice,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width: target.width,
            height: target.height,
            depth: 1,
        });
    // SAFETY: recording into this command buffer, outside a render pass; every
    // image this runtime makes stays in GENERAL.
    unsafe {
        gpu.device.cmd_copy_image_to_buffer(
            command_buffer,
            target.image,
            vk::ImageLayout::GENERAL,
            buffer.buffer,
            &[copy],
        )
    };
    Some(Capture {
        buffer,
        name: format!(
            "pass{pass:04}-{when}-{}-img{:x}-{}x{}-{:?}.raw",
            target.role,
            ash::vk::Handle::as_raw(target.image),
            target.width,
            target.height,
            target.format
        ),
    })
}

/// Write out and free everything one submission captured.
pub(crate) fn finish(gpu: &'static Gpu, captures: Vec<Capture>) {
    let Some(directory) = directory() else {
        return;
    };
    let _ = std::fs::create_dir_all(directory);
    for capture in captures {
        // SAFETY: the submission completed and the mapping covers the buffer.
        let pixels = unsafe {
            std::slice::from_raw_parts(capture.buffer.contents, capture.buffer.length as usize)
        };
        let _ = std::fs::write(directory.join(&capture.name), pixels);
        gpu.destroy_buffer(capture.buffer);
    }
}
