#![allow(clippy::manual_dangling_ptr, clippy::missing_safety_doc)]

//! `CVDisplayLink`, on a thread of its own.
//!
//! A display link is a callback at the refresh rate, and it is how a Mac game
//! that does not use `NSTimer` gets a frame loop. Deltarune is one: after
//! `applicationDidFinishLaunching:` returns, *nothing else* would ever run its
//! code again.
//!
//! The thread is not an implementation detail. This was a run loop timer first,
//! on the argument that a callback drawing into an `NSOpenGLContext` needs the
//! thread that context is current on - and Deltarune deadlocked on it. Its frame
//! function spins on a counter that the *callback* increments, so a callback
//! that waits for the main thread to come back round waits for a main thread
//! waiting for it. Apple's runs on a dedicated thread, guests are written
//! against that, and matching it is the only thing that terminates.
//!
//! What a guest may do in the callback follows from where it runs: a GL context
//! is current per thread, so a callback that draws has to make one current on
//! this thread first, exactly as it would on Apple's.

// The C spellings are the interface, as they are in `objc::api`; renaming them
// would only hide it.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// `CVReturn`. Zero is success, and the one failure this reports is
/// `kCVReturnInvalidArgument`.
const SUCCESS: i32 = 0;
const INVALID_ARGUMENT: i32 = -6661;

/// `CVTimeStamp`, which the callback receives twice - "now" and "when this frame
/// will be shown".
///
/// Eighty bytes, and the layout is ABI: a guest reads `videoTime` and
/// `videoTimeScale` out of it to work out how much time has passed. Checked
/// against the header rather than counted.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TimeStamp {
    pub version: u32,
    pub video_time_scale: i32,
    pub video_time: i64,
    pub host_time: u64,
    pub rate_scalar: f64,
    pub video_refresh_period: i64,
    /// `CVSMPTETime`, which nothing here fills in.
    smpte_time: [u8; 24],
    pub flags: u64,
    pub reserved: u64,
}

const _: () = {
    assert!(size_of::<TimeStamp>() == 80);
    assert!(std::mem::offset_of!(TimeStamp, video_time) == 8);
    assert!(std::mem::offset_of!(TimeStamp, host_time) == 16);
    assert!(std::mem::offset_of!(TimeStamp, rate_scalar) == 24);
    assert!(std::mem::offset_of!(TimeStamp, video_refresh_period) == 32);
    assert!(std::mem::offset_of!(TimeStamp, flags) == 64);
};

/// `CVDisplayLinkOutputCallback`.
type OutputCallback = unsafe extern "C" fn(
    *mut c_void,
    *const TimeStamp,
    *const TimeStamp,
    u64,
    *mut u64,
    *mut c_void,
) -> i32;

/// One display link.
struct Link {
    callback: Option<OutputCallback>,
    context: *mut c_void,
    /// Set while the link's thread should keep calling back. Cleared by
    /// [`CVDisplayLinkStop`], which is the thread's only stop signal.
    running: Option<Arc<AtomicBool>>,
    /// Seconds between frames, from the display SDL reports.
    period: f64,
    /// How many times the callback has run, which is the `videoTime` a guest
    /// integrates to get elapsed time.
    frame: i64,
    started: std::time::Instant,
}

// SAFETY: the context is the guest's own pointer and is only ever handed back to
// the guest's callback; the callback is a thunk this runtime made.
unsafe impl Send for Link {}

/// Every link this process has made, by the handle the guest holds.
///
/// The handle is the address of the `Arc`'s contents, which is stable and unique
/// for as long as the link is alive - so it is the `CVDisplayLinkRef` itself
/// rather than an index a guest could fabricate.
static LINKS: Mutex<Vec<(usize, Arc<Mutex<Link>>)>> = Mutex::new(Vec::new());

fn link_for(handle: *mut c_void) -> Option<Arc<Mutex<Link>>> {
    LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle as usize)
        .map(|(_, link)| Arc::clone(link))
}

/// The refresh period of the display a link is on, in seconds.
///
/// SDL cannot report a refresh rate through the entry points this tree binds, so
/// sixty per second is the assumption - which is what a display link on a
/// sixty-hertz display gives, and what every other rate is a multiple or a
/// fraction of. A guest that integrates `videoTime` gets a consistent clock
/// either way, because the timestamps this module hands out are built from the
/// same number it drives the timer with.
const DEFAULT_PERIOD: f64 = 1.0 / 60.0;

/// `CVReturn CVDisplayLinkCreateWithCGDisplay(CGDirectDisplayID, CVDisplayLinkRef *)`.
///
/// # Safety
///
/// `out` must be null or a writable pointer-sized slot.
pub unsafe extern "C" fn CVDisplayLinkCreateWithCGDisplay(
    _display: u32,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    let link = Arc::new(Mutex::new(Link {
        callback: None,
        context: std::ptr::null_mut(),
        running: None,
        period: DEFAULT_PERIOD,
        frame: 0,
        started: std::time::Instant::now(),
    }));
    // The handle is the `Mutex`'s address, which the `Arc` in the table keeps
    // alive for as long as the link exists.
    let handle = Arc::as_ptr(&link) as *mut c_void;
    LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((handle as usize, link));
    // SAFETY: the caller's obligation.
    unsafe { *out = handle };
    SUCCESS
}

/// As above, for the "all active displays" spelling. One display link either
/// way: this runtime draws into one window.
///
/// # Safety
///
/// As [`CVDisplayLinkCreateWithCGDisplay`].
pub unsafe extern "C" fn CVDisplayLinkCreateWithActiveCGDisplays(out: *mut *mut c_void) -> i32 {
    // SAFETY: the caller's obligation.
    unsafe { CVDisplayLinkCreateWithCGDisplay(0, out) }
}

/// # Safety
///
/// `callback` must be a host-callable function of the display link's prototype -
/// which for a guest's own function is the thunk `rosetta3`'s ABI layer made,
/// because the table entry names the parameter a callback.
pub unsafe extern "C" fn CVDisplayLinkSetOutputCallback(
    handle: *mut c_void,
    callback: *mut c_void,
    context: *mut c_void,
) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    let mut link = link.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the caller's obligation.
    link.callback = (!callback.is_null())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, OutputCallback>(callback) });
    link.context = context;
    SUCCESS
}

pub unsafe extern "C" fn CVDisplayLinkSetCurrentCGDisplay(
    handle: *mut c_void,
    _display: u32,
) -> i32 {
    match link_for(handle).is_some() {
        true => SUCCESS,
        false => INVALID_ARGUMENT,
    }
}

pub unsafe extern "C" fn CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext(
    handle: *mut c_void,
    _context: *mut c_void,
    _format: *mut c_void,
) -> i32 {
    match link_for(handle).is_some() {
        true => SUCCESS,
        false => INVALID_ARGUMENT,
    }
}

pub unsafe extern "C" fn CVDisplayLinkGetActualOutputVideoRefreshPeriod(
    handle: *mut c_void,
) -> f64 {
    link_for(handle).map_or(0.0, |link| {
        link.lock().unwrap_or_else(|e| e.into_inner()).period
    })
}

/// `CVDisplayLinkStart`: start the thread that calls the guest back.
pub unsafe extern "C" fn CVDisplayLinkStart(handle: *mut c_void) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    let (period, running) = {
        let mut guard = link.lock().unwrap_or_else(|e| e.into_inner());
        if guard.running.is_some() {
            return SUCCESS;
        }
        guard.started = std::time::Instant::now();
        let running = Arc::new(AtomicBool::new(true));
        guard.running = Some(Arc::clone(&running));
        (guard.period, running)
    };

    let period = std::time::Duration::from_secs_f64(period);
    let at = handle as usize;
    // Detached. A guest may stop its link from inside the callback, and a
    // `Stop` that joined would then be the thread waiting for itself.
    std::thread::Builder::new()
        .name("CVDisplayLink".to_string())
        .spawn(move || {
            let mut next = std::time::Instant::now() + period;
            while running.load(Ordering::Acquire) {
                let now = std::time::Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    // Behind: give up the frames that were missed rather than
                    // calling back in a burst to catch up.
                    next = now;
                }
                next += period;
                if !running.load(Ordering::Acquire) {
                    break;
                }
                // SAFETY: `at` is a handle this module made, and the table keeps
                // it alive.
                unsafe { fire(std::ptr::null(), at as *mut c_void) };
            }
        })
        .map_or(INVALID_ARGUMENT, |_| SUCCESS)
}

pub unsafe extern "C" fn CVDisplayLinkStop(handle: *mut c_void) -> i32 {
    let Some(link) = link_for(handle) else {
        return INVALID_ARGUMENT;
    };
    let running = link
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running
        .take();
    if let Some(running) = running {
        running.store(false, Ordering::Release);
    }
    SUCCESS
}

pub unsafe extern "C" fn CVDisplayLinkRetain(handle: *mut c_void) -> *mut c_void {
    handle
}

pub unsafe extern "C" fn CVDisplayLinkRelease(_handle: *mut c_void) {}

pub unsafe extern "C" fn CVDisplayLinkIsRunning(handle: *mut c_void) -> u8 {
    link_for(handle).is_some_and(|link| {
        link.lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
            .is_some()
    }) as u8
}

/// One turn of the link: build the two timestamps and call the guest.
///
/// `info` is the link's handle, which is how one function serves them all.
unsafe extern "C" fn fire(_timer: *const c_void, info: *mut c_void) {
    let Some(link) = link_for(info) else {
        return;
    };
    let (callback, context, now, output) = {
        let mut guard = link.lock().unwrap_or_else(|e| e.into_inner());
        let Some(callback) = guard.callback else {
            return;
        };
        guard.frame += 1;
        let scale = 1_000_000_000i32;
        let elapsed = guard.started.elapsed().as_secs_f64();
        let now = TimeStamp {
            version: 0,
            video_time_scale: scale,
            video_time: (elapsed * scale as f64) as i64,
            host_time: elapsed.to_bits(),
            rate_scalar: 1.0,
            video_refresh_period: (guard.period * scale as f64) as i64,
            smpte_time: [0; 24],
            flags: 0,
            reserved: 0,
        };
        // The frame after this one, which is what "output time" means.
        let output = TimeStamp {
            video_time: now.video_time + now.video_refresh_period,
            ..now
        };
        (callback, guard.context, now, output)
    };
    // Called with the lock released: the guest's callback may stop the link, and
    // `CVDisplayLinkStop` takes the same lock.
    let mut flags_out = 0u64;
    // SAFETY: `callback` is the thunk the guest's function was registered
    // through, and the two timestamps outlive the call.
    unsafe { callback(info, &now, &output, 0, &mut flags_out, context) };
}

/// One entry point, as an address.
macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "CVDisplayLinkCreateWithCGDisplay" => at!(CVDisplayLinkCreateWithCGDisplay),
        "CVDisplayLinkCreateWithActiveCGDisplays" => at!(CVDisplayLinkCreateWithActiveCGDisplays),
        "CVDisplayLinkSetOutputCallback" => at!(CVDisplayLinkSetOutputCallback),
        "CVDisplayLinkSetCurrentCGDisplay" => at!(CVDisplayLinkSetCurrentCGDisplay),
        "CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext" => {
            at!(CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext)
        }
        "CVDisplayLinkGetActualOutputVideoRefreshPeriod" => {
            at!(CVDisplayLinkGetActualOutputVideoRefreshPeriod)
        }
        "CVDisplayLinkStart" => at!(CVDisplayLinkStart),
        "CVDisplayLinkStop" => at!(CVDisplayLinkStop),
        "CVDisplayLinkRetain" => at!(CVDisplayLinkRetain),
        "CVDisplayLinkRelease" => at!(CVDisplayLinkRelease),
        "CVDisplayLinkIsRunning" => at!(CVDisplayLinkIsRunning),
        _ => return None,
    })
}

/// Every name [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "CVDisplayLinkCreateWithCGDisplay",
    "CVDisplayLinkCreateWithActiveCGDisplays",
    "CVDisplayLinkSetOutputCallback",
    "CVDisplayLinkSetCurrentCGDisplay",
    "CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext",
    "CVDisplayLinkGetActualOutputVideoRefreshPeriod",
    "CVDisplayLinkStart",
    "CVDisplayLinkStop",
    "CVDisplayLinkRetain",
    "CVDisplayLinkRelease",
    "CVDisplayLinkIsRunning",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIRED: AtomicUsize = AtomicUsize::new(0);
    static SEEN_CONTEXT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn record(
        _link: *mut c_void,
        now: *const TimeStamp,
        output: *const TimeStamp,
        _flags: u64,
        _flags_out: *mut u64,
        context: *mut c_void,
    ) -> i32 {
        FIRED.fetch_add(1, Ordering::SeqCst);
        SEEN_CONTEXT.store(context as usize, Ordering::SeqCst);
        // SAFETY: two timestamps the caller built on its own frame.
        unsafe {
            assert!((*output).video_time > (*now).video_time, "output is later");
        }
        SUCCESS
    }

    #[test]
    fn a_started_link_calls_back_on_its_own_thread() {
        FIRED.store(0, Ordering::SeqCst);
        let mut handle = std::ptr::null_mut();
        // SAFETY: a writable slot.
        assert_eq!(
            unsafe { CVDisplayLinkCreateWithCGDisplay(0, &mut handle) },
            SUCCESS
        );
        assert!(!handle.is_null());

        // SAFETY: `record` has the callback's prototype.
        assert_eq!(
            unsafe {
                CVDisplayLinkSetOutputCallback(handle, record as *mut c_void, 0x1234 as *mut c_void)
            },
            SUCCESS
        );
        // SAFETY: a link this test made.
        assert_eq!(unsafe { CVDisplayLinkStart(handle) }, SUCCESS);
        // SAFETY: as above.
        assert_eq!(unsafe { CVDisplayLinkIsRunning(handle) }, 1);

        // Nothing is turned here, which is the whole point: this thread does
        // not serve the link, so a guest may block on it.
        for _ in 0..100 {
            if FIRED.load(Ordering::SeqCst) >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            FIRED.load(Ordering::SeqCst) >= 2,
            "the link did not call back twice"
        );
        assert_eq!(
            SEEN_CONTEXT.load(Ordering::SeqCst),
            0x1234,
            "the context arrives"
        );

        // SAFETY: a link this test made.
        assert_eq!(unsafe { CVDisplayLinkStop(handle) }, SUCCESS);
        // SAFETY: as above.
        assert_eq!(unsafe { CVDisplayLinkIsRunning(handle) }, 0);

        // And a stopped link stops: the thread notices the flag within a frame.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let settled = FIRED.load(Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            FIRED.load(Ordering::SeqCst),
            settled,
            "the thread kept firing"
        );
    }

    /// A handle nothing made is refused rather than dereferenced.
    #[test]
    fn an_unknown_handle_is_an_invalid_argument() {
        // SAFETY: a pointer this module never handed out.
        unsafe {
            assert_eq!(CVDisplayLinkStart(0x1 as *mut c_void), INVALID_ARGUMENT);
            assert_eq!(
                CVDisplayLinkSetOutputCallback(
                    0x1 as *mut c_void,
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                INVALID_ARGUMENT
            );
        }
    }

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|n| symbol(n).is_none())
            .collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }
}
