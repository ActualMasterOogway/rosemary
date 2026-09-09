//! Where a command buffer's completion handlers run.
//!
//! Metal never calls a completion handler on the thread that committed: it calls
//! it from the framework's own queue once the work has finished. Games rely on
//! that. Roblox commits a command buffer while holding a renderer lock and takes
//! the same lock in the handler, which is a plain sequential program on Apple's
//! Metal and an instant self-deadlock if the handler runs inside `-commit`.
//!
//! One thread drains one queue, so handlers still run in submission order - a
//! stronger promise than Metal's, which orders them only within a command queue,
//! and one no correct guest can tell apart.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, Once};

use objc::object::Id;

/// A handler waiting to run, with the command buffer it is passed. Addresses
/// rather than `Id`, because a raw pointer is not `Send` and these two are:
/// ownership of one reference to each moves to the worker with the entry.
struct Pending {
    block: usize,
    command_buffer: usize,
}

struct Queue {
    pending: Mutex<VecDeque<Pending>>,
    ready: Condvar,
}

static QUEUE: Queue = Queue {
    pending: Mutex::new(VecDeque::new()),
    ready: Condvar::new(),
};
static WORKER: Once = Once::new();

/// Hand one completion handler to the worker, which invokes and releases it.
///
/// # Safety
///
/// `block` is a block this crate copied and now gives away, and
/// `command_buffer` a live `MTLCommandBuffer` the caller has not released.
pub(crate) unsafe fn deliver(block: Id, command_buffer: Id) {
    WORKER.call_once(|| {
        // Detached deliberately: it runs for the life of the process, and there
        // is no point in the process at which Metal is known to be finished.
        std::thread::Builder::new()
            .name("metal:completed".to_string())
            .spawn(worker)
            .expect("a thread for Metal completion handlers");
    });
    let entry = Pending {
        block: block as usize,
        // The guest may release the command buffer the moment `-commit` returns,
        // and the handler is handed it as its argument.
        command_buffer: objc::arc::retain(command_buffer) as usize,
    };
    let mut pending = QUEUE.pending.lock().expect("the completion queue");
    pending.push_back(entry);
    QUEUE.ready.notify_one();
}

fn worker() {
    loop {
        let entry = {
            let mut pending = QUEUE.pending.lock().expect("the completion queue");
            loop {
                match pending.pop_front() {
                    Some(entry) => break entry,
                    None => {
                        pending = QUEUE.ready.wait(pending).expect("the completion queue");
                    }
                }
            }
        };
        // SAFETY: one reference to each arrived with the entry.
        unsafe { run(entry) };
    }
}

/// # Safety
///
/// The entry carries one reference to a live block and one to a live command
/// buffer, both of which this consumes.
unsafe fn run(entry: Pending) {
    let block = entry.block as Id;
    let command_buffer = entry.command_buffer as Id;
    {
        // This thread is the runtime's, so Darwin code running on it needs the
        // thread-local array it reads `errno`, its stack canary and its
        // allocator identity out of. The dispatch pool does the same thing for
        // the same reason.
        let _darwin = darwin::tls::DarwinThread::enter();
        // A block's invoke pointer is its third word.
        // SAFETY: `block` was copied by `objc_retainBlock` and is still live.
        let invoke = unsafe { *((block as *const u8).add(16) as *const usize) };
        if invoke != 0 {
            // SAFETY: a completion handler is `void (^)(id<MTLCommandBuffer>)`.
            let call: unsafe extern "C" fn(Id, Id) = unsafe { std::mem::transmute(invoke) };
            // SAFETY: the guest's handler, given the buffer it was registered on.
            unsafe { call(block, command_buffer) };
        }
    }
    // The reference this entry owned, now spent.
    objc::block::release(block);
    objc::arc::release(command_buffer);
}
