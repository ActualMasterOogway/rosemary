//! The worker which retires asynchronous Vulkan submissions.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, Once};

use crate::encoder::Submission;

struct Queue {
    pending: Mutex<VecDeque<Submission>>,
    ready: Condvar,
}

static QUEUE: Queue = Queue {
    pending: Mutex::new(VecDeque::new()),
    ready: Condvar::new(),
};
static WORKER: Once = Once::new();

pub(crate) fn submit(submission: Submission) {
    WORKER.call_once(|| {
        std::thread::Builder::new()
            .name("metal:submission".to_string())
            .spawn(worker)
            .expect("a thread for Metal submissions");
    });
    let mut pending = QUEUE.pending.lock().expect("the submission queue");
    pending.push_back(submission);
    QUEUE.ready.notify_one();
}

fn worker() {
    loop {
        let submission = {
            let mut pending = QUEUE.pending.lock().expect("the submission queue");
            loop {
                match pending.pop_front() {
                    Some(submission) => break submission,
                    None => {
                        pending = QUEUE.ready.wait(pending).expect("the submission queue");
                    }
                }
            }
        };
        submission.wait_and_finish();
    }
}
