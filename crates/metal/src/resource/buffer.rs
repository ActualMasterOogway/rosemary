use super::*;

/// Every `MTLBuffer` made and destroyed, with its
/// length, its `MTLResourceOptions` and how many are live.
#[inline(always)]
pub(super) fn tracing_buffers() -> bool {
    #[dynamic]
    static ON: bool = std::env::var_os("ROSEMARY_TRACE_BUFFERS").is_some();

    *ON
}

/// `MTLStorageModePrivate`, as `MTLResourceOptions` carries it in bits 4-7.
const STORAGE_MODE_PRIVATE: u64 = 2;

pub(crate) fn new_buffer(gpu: &'static Gpu, length: usize, options: u64) -> Id {
    if length == 0 {
        return std::ptr::null_mut();
    }
    crate::stats::peak(crate::stats::Counter::BufferPeakBytes, length as u64);
    if tracing_buffers() {
        eprintln!(
            "rosemary: Metal: newBuffer {length} options {options:#x} live {}",
            crate::live_buffers(1)
        );
    }
    let private = (options >> 4) & 0xf == STORAGE_MODE_PRIVATE;
    let inner = match crate::stats::timed(crate::stats::Counter::BufferMicros, || match private {
        true => gpu.create_private_buffer(length as u64),
        false => gpu.create_buffer(length as u64),
    }) {
        Ok(inner) => inner,
        Err(reason) => {
            gpu::trace(format!("newBufferWithLength: failed: {reason}"));
            return std::ptr::null_mut();
        }
    };
    crate::account(length as i64);
    if !private {
        crate::add_shared_buffer_bytes(length as u64);
    }
    crate::stats::count(crate::stats::Counter::BuffersCreated);
    crate::stats::add(crate::stats::Counter::BufferBytes, length as u64);
    object::make(
        c"RosemaryMTLBuffer",
        Buffer {
            gpu,
            inner,
            options,
        },
    )
}

pub(super) unsafe extern "C" fn buffer_contents(receiver: Id, _sel: Sel) -> *mut u8 {
    // SAFETY: a buffer of this crate's.
    unsafe { object::body::<Buffer>(receiver) }.inner.contents
}

pub(super) unsafe extern "C" fn buffer_length(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: a buffer of this crate's.
    unsafe { object::body::<Buffer>(receiver) }.inner.length as usize
}

pub(super) unsafe extern "C" fn buffer_storage_mode(receiver: Id, _sel: Sel) -> u64 {
    // SAFETY: a buffer of this crate's.
    (unsafe { object::body::<Buffer>(receiver) }.options >> 4) & 0xf
}

/// `-[MTLBuffer dealloc]`.
///
/// Without this every buffer a guest makes is a `VkBuffer` and a range of a
/// heap block held for the life of the process. A game which streams its world
/// in makes gigabytes of them in a second, so the leak is not a slow one: it
/// exhausts the adapter's memory during the first minute of play.
///
/// The last reference is gone, so nothing can bind it again, and a command
/// buffer which did bind it holds a reference of its own until the work it was
/// recorded into has completed.
/// TEMPORARY: `ROSEMARY_METAL_LEAK=1` keeps every device resource alive, to tell
/// a fault caused by freeing one too early from every other kind.
#[inline(always)]
pub(crate) fn leaking() -> bool {
    #[dynamic]
    static LEAK: bool = std::env::var_os("ROSEMARY_METAL_LEAK").is_some();

    *LEAK
}

pub(super) unsafe extern "C" fn buffer_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a buffer of this crate's.
    let buffer = unsafe { object::body::<Buffer>(receiver) };
    if tracing_buffers() {
        eprintln!(
            "rosemary: Metal: freeBuffer {} live {}",
            buffer.inner.length,
            crate::live_buffers(-1)
        );
    }
    crate::account(-(buffer.inner.length as i64));
    if !buffer.inner.private {
        crate::sub_shared_buffer_bytes(buffer.inner.length);
    }
    heap::leave(receiver);
    let inner = std::mem::replace(&mut buffer.inner, GpuBuffer::none());
    if !leaking() {
        buffer.gpu.destroy_buffer(inner);
    }
    objc::arc::finalize(receiver);
}
