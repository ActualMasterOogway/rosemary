use std::any::TypeId;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::base::CFTypeRef;

/// `CFRunLoopSourceContext::perform` and the timer's callback: a C function this
/// process can call.
///
/// For a guest's own callback this is a trampoline `rosetta3` compiled - see
/// `shims::corefoundation` - so nothing here ever holds a guest address.
pub type Perform = unsafe extern "C" fn(*mut c_void);
pub type TimerCallback = unsafe extern "C" fn(CFTypeRef, *mut c_void);

/// A version-0 source: a flag, and what to do when it is set.
pub struct Source {
    signalled: AtomicBool,
    perform: Option<Perform>,
    info: usize,
    /// Set when the source is invalidated, after which the loop drops it.
    valid: AtomicBool,
}

// SAFETY: the callback is a code address that outlives the process and `info` is
// an opaque number this crate never dereferences - it is handed back to the
// callback, which is the only thing that knows what it is.
unsafe impl Send for Source {}
unsafe impl Sync for Source {}

impl Source {
    pub fn signal(&self) {
        self.signalled.store(true, Ordering::Release);
    }

    pub fn invalidate(&self) {
        self.valid.store(false, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    /// Run the callback if the source was signalled, clearing the flag first.
    ///
    /// Cleared *before* the callback, so that a callback which signals its own
    /// source runs again next time round rather than never.
    fn fire(&self) -> bool {
        if !self.signalled.swap(false, Ordering::AcqRel) || !self.is_valid() {
            return false;
        }
        let Some(perform) = self.perform else {
            return false;
        };
        // SAFETY: `perform` is a `void (*)(void *)` the creator supplied, and
        // `info` is the pointer it asked to be given back.
        unsafe { perform(self.info as *mut c_void) };
        true
    }
}

pub struct Timer {
    next: Mutex<Instant>,
    interval: Option<Duration>,
    callback: Option<TimerCallback>,
    info: usize,
    valid: AtomicBool,
}

// SAFETY: as [`Source`].
unsafe impl Send for Timer {}
unsafe impl Sync for Timer {}

impl Timer {
    pub fn invalidate(&self) {
        self.valid.store(false, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    /// When this timer is next due, or `None` if it never is.
    fn due(&self) -> Option<Instant> {
        self.is_valid()
            .then(|| *self.next.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Fire if due, and schedule the next one.
    ///
    /// A repeating timer whose interval has passed several times over is fired
    /// *once* and rescheduled from now, which is what Apple's does: catching up
    /// on a backlog of frames is never what a caller wanted.
    fn fire(self: &Arc<Timer>, now: Instant) -> bool {
        {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            if !self.is_valid() || now < *next {
                return false;
            }
            match self.interval {
                Some(interval) => *next = now + interval,
                None => {
                    self.valid.store(false, Ordering::Release);
                }
            }
        }
        let Some(callback) = self.callback else {
            return false;
        };
        let handle = Arc::as_ptr(self) as CFTypeRef;
        // SAFETY: `callback` is a `void (*)(CFRunLoopTimerRef, void *)` the
        // creator supplied, and `info` is the pointer it asked for.
        unsafe { callback(handle, self.info as *mut c_void) };
        true
    }
}

#[derive(Default)]
struct Mode {
    sources: Vec<Arc<Source>>,
    timers: Vec<Arc<Timer>>,
}

pub struct RunLoop {
    modes: Mutex<BTreeMap<String, Mode>>,
    stopped: AtomicBool,
    waker: (Mutex<bool>, Condvar),
}

pub const DEFAULT_MODE: &str = "kCFRunLoopDefaultMode";
pub const COMMON_MODES: &str = "kCFRunLoopCommonModes";

pub mod result {
    pub const FINISHED: i32 = 1;
    pub const STOPPED: i32 = 2;
    pub const TIMED_OUT: i32 = 3;
    pub const HANDLED_SOURCE: i32 = 4;
}

impl RunLoop {
    fn new() -> RunLoop {
        RunLoop {
            modes: Mutex::new(BTreeMap::new()),
            stopped: AtomicBool::new(false),
            waker: (Mutex::new(false), Condvar::new()),
        }
    }

    /// The modes a name refers to: itself, or every mode for the common alias.
    fn names(&self, mode: &str) -> Vec<String> {
        if mode != COMMON_MODES {
            return vec![mode.to_string()];
        }
        let modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
        // The default one always, so a source added to the common modes before
        // any mode exists is still there when the loop runs the default one.
        let mut names: Vec<String> = modes.keys().cloned().collect();
        if !names.iter().any(|n| n == DEFAULT_MODE) {
            names.push(DEFAULT_MODE.to_string());
        }
        names
    }

    pub fn add_source(&self, source: &Arc<Source>, mode: &str) {
        for name in self.names(mode) {
            let mut modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
            let entry = modes.entry(name).or_default();
            if !entry.sources.iter().any(|s| Arc::ptr_eq(s, source)) {
                entry.sources.push(Arc::clone(source));
            }
        }
        self.wake();
    }

    pub fn remove_source(&self, source: &Arc<Source>, mode: &str) {
        for name in self.names(mode) {
            let mut modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = modes.get_mut(&name) {
                entry.sources.retain(|s| !Arc::ptr_eq(s, source));
            }
        }
    }

    pub fn contains_source(&self, source: &Arc<Source>, mode: &str) -> bool {
        let modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
        self.names(mode).iter().any(|name| {
            modes
                .get(name)
                .is_some_and(|m| m.sources.iter().any(|s| Arc::ptr_eq(s, source)))
        })
    }

    pub fn add_timer(&self, timer: &Arc<Timer>, mode: &str) {
        for name in self.names(mode) {
            let mut modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
            let entry = modes.entry(name).or_default();
            if !entry.timers.iter().any(|t| Arc::ptr_eq(t, timer)) {
                entry.timers.push(Arc::clone(timer));
            }
        }
        self.wake();
    }

    pub fn remove_timer(&self, timer: &Arc<Timer>, mode: &str) {
        for name in self.names(mode) {
            let mut modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = modes.get_mut(&name) {
                entry.timers.retain(|t| !Arc::ptr_eq(t, timer));
            }
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake();
    }

    /// Interrupt a loop that is waiting, from any thread.
    pub fn wake(&self) {
        let (lock, condition) = &self.waker;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condition.notify_all();
    }

    /// Whether `mode` has anything at all to service.
    ///
    /// `CFRunLoopRunInMode` on an empty mode returns immediately rather than
    /// waiting out its timeout: a loop with no sources is one nothing can ever
    /// wake, and sleeping in it is indistinguishable from a hang.
    pub fn is_empty(&self, mode: &str) -> bool {
        let modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
        self.names(mode).iter().all(|name| {
            modes.get(name).is_none_or(|m| {
                m.sources.iter().all(|s| !s.is_valid()) && m.timers.iter().all(|t| !t.is_valid())
            })
        })
    }

    /// Everything `mode` holds, copied out so the lock is not held across a
    /// callback.
    ///
    /// A callback is entitled to add or remove sources - `[NSApp run]`'s does,
    /// every frame - and holding the lock across one is a deadlock the first
    /// time it happens.
    fn contents(&self, mode: &str) -> (Vec<Arc<Source>>, Vec<Arc<Timer>>) {
        let modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
        let mut sources = Vec::new();
        let mut timers = Vec::new();
        for name in self.names(mode) {
            if let Some(entry) = modes.get(&name) {
                sources.extend(entry.sources.iter().cloned());
                timers.extend(entry.timers.iter().cloned());
            }
        }
        (sources, timers)
    }

    /// Run `mode` for at most `seconds`, as `CFRunLoopRunInMode` does.
    pub fn run_in_mode(&self, mode: &str, seconds: f64, return_after_source: bool) -> i32 {
        self.stopped.store(false, Ordering::Release);
        let deadline = match seconds.is_finite() && seconds >= 0.0 {
            true => Instant::now().checked_add(Duration::from_secs_f64(seconds.min(1e9))),
            false => None,
        };

        loop {
            if self.stopped.swap(false, Ordering::AcqRel) {
                return result::STOPPED;
            }
            if self.is_empty(mode) {
                return result::FINISHED;
            }

            let now = Instant::now();
            let (sources, timers) = self.contents(mode);
            let mut handled = false;
            for timer in &timers {
                handled |= timer.fire(now);
            }
            for source in &sources {
                handled |= source.fire();
            }
            if handled && return_after_source {
                return result::HANDLED_SOURCE;
            }
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                return result::TIMED_OUT;
            }

            // Sleep until the next timer, the deadline, or a wake-up - whichever
            // comes first. A loop that spun here would be a busy wait in every
            // program that idles, which is most of them.
            let next_timer = timers.iter().filter_map(|t| t.due()).min();
            let wait_until = [next_timer, deadline]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or_else(|| Instant::now() + Duration::from_millis(50));
            self.wait_until(wait_until);
        }
    }

    fn wait_until(&self, deadline: Instant) {
        let (lock, condition) = &self.waker;
        let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
        if *woken {
            *woken = false;
            return;
        }
        let timeout = deadline.saturating_duration_since(Instant::now());
        let (mut woken, _) = condition
            .wait_timeout(woken, timeout)
            .unwrap_or_else(|e| e.into_inner());
        *woken = false;
    }
}

/// Build a version-0 source.
pub fn new_source(perform: Option<Perform>, info: usize) -> Arc<Source> {
    let source = Arc::new(Source {
        signalled: AtomicBool::new(false),
        perform,
        info,
        valid: AtomicBool::new(true),
    });
    note(Arc::as_ptr(&source));
    source
}

/// Every run loop, source and timer this module made, and which of the three it
/// is.
///
/// These are the one family of CoreFoundation handles that is *not* an object
/// with an isa - a `CFRunLoopSourceRef` here is a leaked `Arc` - so nothing
/// about the pointer says what it points at, and the isa check
/// [`crate::foreign`] uses cannot be asked. It has to be asked of something: a
/// program under the builtin frameworks still gets sources from Apple's own
/// libraries, IOKit's notification port among them, and one of those read as a
/// [`Source`] turns a field of a structure this crate did not lay out into the
/// callback the loop then calls.
static HANDLES: Mutex<BTreeMap<usize, TypeId>> = Mutex::new(BTreeMap::new());

/// Record a handle before it is handed to the C world.
pub fn note<T: 'static>(handle: *const T) {
    HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle as usize, TypeId::of::<T>());
}

/// Whether `handle` is a `T` this module handed out.
pub fn is_noted<T: 'static>(handle: usize) -> bool {
    HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&handle)
        == Some(&TypeId::of::<T>())
}

/// Build a timer, first due `after` from now and repeating every `interval`.
pub fn new_timer(
    after: Duration,
    interval: Option<Duration>,
    callback: Option<TimerCallback>,
    info: usize,
) -> Arc<Timer> {
    let timer = Arc::new(Timer {
        next: Mutex::new(Instant::now() + after),
        interval,
        callback,
        info,
        valid: AtomicBool::new(true),
    });
    note(Arc::as_ptr(&timer));
    timer
}

thread_local! {
    /// This thread's run loop, made on the first `CFRunLoopGetCurrent`.
    static CURRENT: std::cell::OnceCell<Arc<RunLoop>> = const { std::cell::OnceCell::new() };
}

/// The main thread's, which `CFRunLoopGetMain` answers from any thread.
static MAIN: Mutex<Option<Arc<RunLoop>>> = Mutex::new(None);

/// This thread's run loop, made if it has none.
///
/// The first caller's becomes the main one, which is right because the first
/// caller is the thread the program started on.
pub fn current() -> Arc<RunLoop> {
    CURRENT.with(|cell| {
        let loop_ = cell.get_or_init(|| {
            let loop_ = Arc::new(RunLoop::new());
            note(Arc::as_ptr(&loop_));
            loop_
        });
        let mut main = MAIN.lock().unwrap_or_else(|e| e.into_inner());
        if main.is_none() {
            *main = Some(Arc::clone(loop_));
        }
        Arc::clone(loop_)
    })
}

/// The main thread's run loop, or this thread's if nothing has claimed it.
pub fn main() -> Arc<RunLoop> {
    let existing = MAIN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    existing.unwrap_or_else(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A counter and a pair of callbacks, per test.
    ///
    /// Per test rather than shared: the suite runs them on several threads at
    /// once, and one counter between them makes every assertion here depend on
    /// which other test happened to be running.
    macro_rules! counter {
        () => {{
            static CALLS: AtomicUsize = AtomicUsize::new(0);
            unsafe extern "C" fn source(_info: *mut c_void) {
                CALLS.fetch_add(1, Ordering::SeqCst);
            }
            unsafe extern "C" fn timer(_timer: CFTypeRef, _info: *mut c_void) {
                CALLS.fetch_add(1, Ordering::SeqCst);
            }
            (
                || CALLS.load(Ordering::SeqCst),
                source as Perform,
                timer as TimerCallback,
            )
        }};
    }

    #[test]
    fn a_signalled_source_is_serviced_and_a_quiet_one_is_not() {
        let (calls, perform, _) = counter!();
        let loop_ = Arc::new(RunLoop::new());
        let source = new_source(Some(perform), 0);
        loop_.add_source(&source, DEFAULT_MODE);

        // Nothing signalled: the loop waits out its timeout and says so.
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 0.01, true),
            result::TIMED_OUT
        );
        assert_eq!(calls(), 0);

        source.signal();
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 1.0, true),
            result::HANDLED_SOURCE
        );
        assert_eq!(calls(), 1);
        // Signalled once, serviced once: the flag is cleared by the firing.
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 0.01, true),
            result::TIMED_OUT
        );
        assert_eq!(calls(), 1);
    }

    /// An empty mode returns at once rather than sleeping: a loop nothing can
    /// wake is indistinguishable from a hang.
    #[test]
    fn an_empty_mode_finishes_immediately() {
        let loop_ = Arc::new(RunLoop::new());
        let started = Instant::now();
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 30.0, false),
            result::FINISHED
        );
        assert!(started.elapsed() < Duration::from_secs(1), "it waited");
    }

    #[test]
    fn a_repeating_timer_fires_more_than_once_and_stops_when_invalidated() {
        let (calls, _, callback) = counter!();
        let loop_ = Arc::new(RunLoop::new());
        let timer = new_timer(
            Duration::from_millis(1),
            Some(Duration::from_millis(1)),
            Some(callback),
            0,
        );
        loop_.add_timer(&timer, DEFAULT_MODE);
        loop_.run_in_mode(DEFAULT_MODE, 0.05, false);
        let fired = calls();
        assert!(fired >= 2, "a repeating timer fired {fired} times");

        timer.invalidate();
        // With nothing valid left, the mode is empty.
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 1.0, false),
            result::FINISHED
        );
        assert_eq!(calls(), fired, "it fired after invalidation");
    }

    /// A one-shot timer fires once and invalidates itself.
    #[test]
    fn a_one_shot_timer_fires_once() {
        let (calls, _, callback) = counter!();
        let loop_ = Arc::new(RunLoop::new());
        let timer = new_timer(Duration::from_millis(1), None, Some(callback), 0);
        loop_.add_timer(&timer, DEFAULT_MODE);
        loop_.run_in_mode(DEFAULT_MODE, 0.05, false);
        assert_eq!(calls(), 1);
        assert!(!timer.is_valid());
    }

    /// Signalled from another thread while the loop is idle: the wake-up is what
    /// makes this finish in milliseconds rather than at the end of the timeout.
    #[test]
    fn a_source_signalled_from_another_thread_wakes_the_loop() {
        let (calls, perform, _) = counter!();
        let loop_ = Arc::new(RunLoop::new());
        let source = new_source(Some(perform), 0);
        loop_.add_source(&source, DEFAULT_MODE);

        let waker = Arc::clone(&loop_);
        let signal = Arc::clone(&source);
        let started = Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            signal.signal();
            waker.wake();
        });
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 30.0, true),
            result::HANDLED_SOURCE
        );
        assert_eq!(calls(), 1);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it slept the timeout out"
        );
    }

    /// `CFRunLoopStop` from inside a callback ends the loop.
    #[test]
    fn stopping_from_inside_a_callback_returns_stopped() {
        static LOOP: Mutex<Option<Arc<RunLoop>>> = Mutex::new(None);
        unsafe extern "C" fn stop_it(_info: *mut c_void) {
            if let Some(l) = LOOP.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                l.stop();
            }
        }
        let loop_ = Arc::new(RunLoop::new());
        *LOOP.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&loop_));
        let source = new_source(Some(stop_it), 0);
        loop_.add_source(&source, DEFAULT_MODE);
        source.signal();
        assert_eq!(loop_.run_in_mode(DEFAULT_MODE, 5.0, false), result::STOPPED);
        *LOOP.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// A source added to the common modes is run by the default one.
    #[test]
    fn the_common_modes_alias_reaches_the_default_mode() {
        let (calls, perform, _) = counter!();
        let loop_ = Arc::new(RunLoop::new());
        let source = new_source(Some(perform), 0);
        loop_.add_source(&source, COMMON_MODES);
        source.signal();
        assert_eq!(
            loop_.run_in_mode(DEFAULT_MODE, 1.0, true),
            result::HANDLED_SOURCE
        );
        assert_eq!(calls(), 1);
    }
}
