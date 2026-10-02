use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use corefoundation::api as cf;
use corefoundation::base::CFTypeRef;

struct Registration {
    active: AtomicBool,
    owner: usize,
    poll: fn(usize),
    run_loop: usize,
    mode: String,
    source: usize,
    timer: usize,
}

/// Contexts remain allocated because a reentrant unschedule can occur in a
/// callback and the run loop may already have copied its list of sources.
#[derive(Default)]
pub(crate) struct Schedules(Mutex<Vec<&'static Registration>>);

impl Schedules {
    pub fn any(&self) -> bool {
        !self.0.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    pub fn current(&self) -> bool {
        let current = unsafe { cf::CFRunLoopGetCurrent() } as usize;
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|r| r.run_loop == current && r.active.load(Ordering::Acquire))
    }

    pub fn add(&self, owner: usize, poll: fn(usize), run_loop: CFTypeRef, mode: CFTypeRef) {
        let name = unsafe { corefoundation::string::as_string(mode) }
            .map(|s| String::from_utf8_lossy(&s.utf8()).into_owned())
            .unwrap_or_default();
        let mut registrations = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if run_loop.is_null()
            || registrations
                .iter()
                .any(|r| r.run_loop == run_loop as usize && r.mode == name)
        {
            return;
        }
        unsafe extern "C" fn perform(info: *mut c_void) {
            let r = unsafe { &*(info as *const Registration) };
            if r.active.load(Ordering::Acquire) {
                (r.poll)(r.owner);
            }
        }
        unsafe extern "C" fn wake(_timer: CFTypeRef, info: *mut c_void) {
            unsafe { perform(info) };
        }
        let mut registration = Box::new(Registration {
            active: AtomicBool::new(true),
            owner,
            poll,
            run_loop: run_loop as usize,
            mode: name,
            source: 0,
            timer: 0,
        });
        let info = std::ptr::from_mut(&mut *registration).cast::<c_void>();
        let context = cf::CFRunLoopSourceContext {
            version: 0,
            info,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
            equal: std::ptr::null(),
            hash: std::ptr::null(),
            schedule: std::ptr::null(),
            cancel: std::ptr::null(),
            perform: Some(perform),
        };
        let mut timer_context = cf::CFRunLoopTimerContext {
            version: 0,
            info,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        unsafe {
            let source = cf::CFRunLoopSourceCreate(std::ptr::null(), 0, &context);
            let timer = cf::CFRunLoopTimerCreate(
                std::ptr::null(),
                cf::CFAbsoluteTimeGetCurrent() + 1.0 / 60.0,
                1.0 / 60.0,
                0,
                0,
                Some(wake),
                &mut timer_context,
            );
            registration.source = source as usize;
            registration.timer = timer as usize;
            cf::CFRunLoopAddSource(run_loop, source, mode);
            cf::CFRunLoopAddTimer(run_loop, timer, mode);
            cf::CFRunLoopSourceSignal(source);
        }
        registrations.push(Box::leak(registration));
    }

    pub fn remove(&self, run_loop: CFTypeRef, mode: CFTypeRef) {
        let name = unsafe { corefoundation::string::as_string(mode) }
            .map(|s| String::from_utf8_lossy(&s.utf8()).into_owned())
            .unwrap_or_default();
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|r| {
                if r.run_loop != run_loop as usize || r.mode != name {
                    return true;
                }
                r.active.store(false, Ordering::Release);
                unsafe {
                    cf::CFRunLoopRemoveSource(run_loop, r.source as CFTypeRef, mode);
                    cf::CFRunLoopRemoveTimer(run_loop, r.timer as CFTypeRef, mode);
                    cf::CFRunLoopSourceInvalidate(r.source as CFTypeRef);
                    cf::CFRunLoopTimerInvalidate(r.timer as CFTypeRef);
                    cf::CFRelease(r.source as CFTypeRef);
                    cf::CFRelease(r.timer as CFTypeRef);
                }
                false
            });
    }

    pub fn clear(&self) {
        let registrations = std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()));
        for r in registrations {
            r.active.store(false, Ordering::Release);
            let mode = corefoundation::string::from_bytes(r.mode.as_bytes(), false);
            unsafe {
                cf::CFRunLoopRemoveSource(r.run_loop as CFTypeRef, r.source as CFTypeRef, mode);
                cf::CFRunLoopRemoveTimer(r.run_loop as CFTypeRef, r.timer as CFTypeRef, mode);
                cf::CFRunLoopSourceInvalidate(r.source as CFTypeRef);
                cf::CFRunLoopTimerInvalidate(r.timer as CFTypeRef);
                cf::CFRelease(r.source as CFTypeRef);
                cf::CFRelease(r.timer as CFTypeRef);
                cf::CFRelease(mode);
            }
        }
    }
}
