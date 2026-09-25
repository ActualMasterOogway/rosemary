use super::*;

pub(crate) fn new_command_queue(gpu: &'static Gpu, device: Id) -> Id {
    object::make(
        c"R3MTLCommandQueue",
        CommandQueue {
            gpu,
            device: objc::arc::retain(device),
        },
    )
}

/// `-[MTLCommandQueue dealloc]`.
pub(super) unsafe extern "C" fn queue_dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: a command queue of this crate's.
    let queue = unsafe { object::body::<CommandQueue>(receiver) };
    objc::arc::release(std::mem::replace(&mut queue.device, std::ptr::null_mut()));
    objc::arc::finalize(receiver);
}

pub(super) unsafe extern "C" fn queue_device(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a command queue of this crate's.
    unsafe { object::body::<CommandQueue>(receiver) }.device
}
