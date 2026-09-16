//! The lifecycle half of FSEvents.
//!
//! A portable host watcher is deliberately not invented here. Clients still
//! need a stream they can schedule and tear down while startup proceeds; an
//! inert stream gives them that contract without reporting events that did not
//! come from Darwin's filesystem service.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::base::{CFIndex, CFTypeRef};

type Callback =
    unsafe extern "C" fn(*const Stream, *mut c_void, usize, *mut c_void, *const u64, *const u32);

#[repr(C)]
pub struct Context {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: *const c_void,
}

pub struct Stream {
    _callback: Option<Callback>,
    info: *const c_void,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    valid: AtomicBool,
    started: AtomicBool,
}

pub unsafe extern "C" fn create(
    _allocator: CFTypeRef,
    callback: Option<Callback>,
    context: *const Context,
    _paths: CFTypeRef,
    _since_when: u64,
    _latency: f64,
    _flags: u32,
) -> *const Stream {
    let (info, release) = if context.is_null() {
        (std::ptr::null(), None)
    } else {
        // SAFETY: the caller supplied a readable FSEventStreamContext.
        let context = unsafe { &*context };
        let info = match context.retain {
            // SAFETY: retain is the ownership callback the caller supplied for info.
            Some(retain) => unsafe { retain(context.info) },
            None => context.info,
        };
        (info, context.release)
    };
    Box::into_raw(Box::new(Stream {
        _callback: callback,
        info,
        release,
        valid: AtomicBool::new(true),
        started: AtomicBool::new(false),
    }))
}

pub unsafe extern "C" fn set_dispatch_queue(_stream: *const Stream, _queue: *mut c_void) {}

pub unsafe extern "C" fn start(stream: *const Stream) -> u8 {
    // SAFETY: FSEvents requires a live stream reference.
    let Some(stream) = (unsafe { stream.as_ref() }) else {
        return 0;
    };
    if !stream.valid.load(Ordering::Acquire) {
        return 0;
    }
    stream.started.store(true, Ordering::Release);
    1
}

pub unsafe extern "C" fn stop(stream: *const Stream) {
    // SAFETY: null is tolerated; otherwise FSEvents requires a live reference.
    if let Some(stream) = unsafe { stream.as_ref() } {
        stream.started.store(false, Ordering::Release);
    }
}

pub unsafe extern "C" fn flush_sync(_stream: *const Stream) {}

pub unsafe extern "C" fn invalidate(stream: *const Stream) {
    // SAFETY: null is tolerated; otherwise FSEvents requires a live reference.
    if let Some(stream) = unsafe { stream.as_ref() } {
        stream.started.store(false, Ordering::Release);
        stream.valid.store(false, Ordering::Release);
    }
}

pub unsafe extern "C" fn release(stream: *const Stream) {
    if stream.is_null() {
        return;
    }
    // SAFETY: FSEventStreamRelease consumes one create-rule stream reference.
    let stream = unsafe { Box::from_raw(stream.cast_mut()) };
    if let Some(release) = stream.release {
        // SAFETY: this balances the context retain performed by create.
        unsafe { release(stream.info) };
    }
}

pub const NAMES: &[&str] = &[
    "FSEventStreamCreate",
    "FSEventStreamSetDispatchQueue",
    "FSEventStreamStart",
    "FSEventStreamStop",
    "FSEventStreamFlushSync",
    "FSEventStreamInvalidate",
    "FSEventStreamRelease",
];

pub fn symbol(name: &str) -> Option<u64> {
    let function: *const () = match name {
        "FSEventStreamCreate" => create as *const (),
        "FSEventStreamSetDispatchQueue" => set_dispatch_queue as *const (),
        "FSEventStreamStart" => start as *const (),
        "FSEventStreamStop" => stop as *const (),
        "FSEventStreamFlushSync" => flush_sync as *const (),
        "FSEventStreamInvalidate" => invalidate as *const (),
        "FSEventStreamRelease" => release as *const (),
        _ => return None,
    };
    Some(function as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static RETAINS: AtomicUsize = AtomicUsize::new(0);
    static RELEASES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn retain(info: *const c_void) -> *const c_void {
        RETAINS.fetch_add(1, Ordering::Relaxed);
        info
    }

    unsafe extern "C" fn release_info(_info: *const c_void) {
        RELEASES.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn stream_lifecycle_balances_context_ownership() {
        RETAINS.store(0, Ordering::Relaxed);
        RELEASES.store(0, Ordering::Relaxed);
        let context = Context {
            version: 0,
            info: 1usize as *mut c_void,
            retain: Some(retain),
            release: Some(release_info),
            copy_description: std::ptr::null(),
        };

        // SAFETY: the context and returned stream remain live for every call.
        unsafe {
            let stream = create(
                std::ptr::null(),
                None,
                &context,
                std::ptr::null(),
                0,
                0.0,
                0,
            );
            assert_eq!(RETAINS.load(Ordering::Relaxed), 1);
            assert_eq!(start(stream), 1);
            invalidate(stream);
            assert_eq!(start(stream), 0);
            release(stream);
        }
        assert_eq!(RELEASES.load(Ordering::Relaxed), 1);
    }
}
