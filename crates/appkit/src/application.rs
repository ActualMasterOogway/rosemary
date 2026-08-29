use std::ffi::{c_char, c_void};

use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct Application {
    isa: *const c_void,
    delegate: Id,
    main_menu: Id,
    running: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Application>();

pub const WILL_FINISH_LAUNCHING: &str = "NSApplicationWillFinishLaunchingNotification";
pub const DID_FINISH_LAUNCHING: &str = "NSApplicationDidFinishLaunchingNotification";
pub const WILL_TERMINATE: &str = "NSApplicationWillTerminateNotification";
pub const DID_BECOME_ACTIVE: &str = "NSApplicationDidBecomeActiveNotification";
pub const WILL_RESIGN_ACTIVE: &str = "NSApplicationWillResignActiveNotification";

static SHARED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn shared() -> Id {
    SHARED.load(std::sync::atomic::Ordering::Acquire) as Id
}

/// `+sharedApplication`, on whichever class it was sent to.
///
/// The receiver rather than `NSApplication`: `NSApplicationMain` sends it to the
/// bundle's `NSPrincipalClass`, and a program whose principal class is
/// `YYApplication` expects `NSApp` to be a `YYApplication` - its own overrides
/// are the reason it declared one.
unsafe extern "C" fn shared_application(class: Id, _sel: Sel) -> Id {
    let existing = shared();
    if !existing.is_null() {
        return existing;
    }
    // SAFETY: the receiver of a class method is a class this runtime knows.
    let Some(class) = (unsafe { objc::class::from_ptr(class as *const Class) }) else {
        return std::ptr::null_mut();
    };
    let app = objc::object::create_instance(class, 0);
    // Retained forever: `NSApp` is a process-lifetime global, and an application
    // a guest can release is one it can make dangle.
    objc::arc::retain(app);
    SHARED.store(app as usize, std::sync::atomic::Ordering::Release);
    // SAFETY: the cell is this crate's own, and the guest reads it as `NSApp`.
    unsafe { *(crate::ns_app_cell() as *mut u64) = app as usize as u64 };
    app
}

unsafe extern "C" fn delegate(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an application.
    unsafe { (*(receiver as *const Application)).delegate }
}

/// `-setDelegate:`, which does not retain - Apple's rule, and the reason a
/// delegate outliving its application is the program's business rather than
/// this one's.
unsafe extern "C" fn set_delegate(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: an application.
    unsafe { (*(receiver as *mut Application)).delegate = value };
}

unsafe extern "C" fn main_menu(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an application.
    unsafe { (*(receiver as *const Application)).main_menu }
}

unsafe extern "C" fn set_main_menu(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: an application.
    unsafe { (*(receiver as *mut Application)).main_menu = objc::arc::retain(value) };
}

unsafe extern "C" fn is_running(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: an application.
    unsafe { ((*(receiver as *const Application)).running != 0) as u8 }
}

/// `-finishLaunching`: the two notifications, in the order Apple sends them.
///
/// The delegate is sent each directly as well as through the notification
/// center, because AppKit registers a delegate for the notifications it declares
/// the moment it is set, and a program that only implements the delegate method
/// never registers as an observer of anything.
unsafe extern "C" fn finish_launching(receiver: Id, _sel: Sel) {
    // SAFETY: an application.
    let delegate = unsafe { (*(receiver as *const Application)).delegate };
    for (name, selector) in [
        (WILL_FINISH_LAUNCHING, "applicationWillFinishLaunching:"),
        (DID_FINISH_LAUNCHING, "applicationDidFinishLaunching:"),
    ] {
        let notification = foundation::notification::make(name, receiver, std::ptr::null_mut());
        // SAFETY: a delegate method is `void (id, SEL, NSNotification *)`.
        unsafe { send_if_implemented(delegate, selector, notification) };
        foundation::notification::post(name, receiver, std::ptr::null_mut());
    }
}

/// `-run`.
///
/// No event loop: an event comes from a window, and a window is M2. What this
/// does have to do is service the run loop, because that is where a scheduled
/// `IOHIDManager` delivers from - so a program that reads a gamepad works
/// before a program that draws does.
///
/// The sleep is for the empty case. `CFRunLoopRunInMode` returns immediately
/// when a mode has nothing in it, and a bare loop around that is a spin at a
/// hundred per cent of a core for a program that is only waiting to be told to
/// stop.
unsafe extern "C" fn run(receiver: Id, sel: Sel) {
    // SAFETY: an application.
    unsafe { (*(receiver as *mut Application)).running = 1 };
    // SAFETY: as above; `-finishLaunching` is `void (id, SEL)`.
    unsafe { finish_launching(receiver, sel) };

    let loop_ = corefoundation::runloop::main();
    loop {
        // SAFETY: an application.
        if unsafe { (*(receiver as *const Application)).running } == 0 {
            break;
        }
        let result = loop_.run_in_mode(corefoundation::runloop::DEFAULT_MODE, 0.05, false);
        if result == corefoundation::runloop::result::FINISHED {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// `-stop:`, which ends the loop above after the current turn.
unsafe extern "C" fn stop(receiver: Id, _sel: Sel, _sender: Id) {
    // SAFETY: an application.
    unsafe { (*(receiver as *mut Application)).running = 0 };
    corefoundation::runloop::main().stop();
}

/// `-terminate:`.
///
/// Apple asks the delegate `applicationShouldTerminate:` first and may be told
/// to wait. That answer is an enumeration whose "later" arm needs a reply the
/// program sends back asynchronously, so it is not half-implemented here: the
/// delegate is asked nothing, and the process ends after the notification. A
/// program that wanted to veto its own exit gets no chance to, which is worth
/// saying out loud rather than discovering.
unsafe extern "C" fn terminate(receiver: Id, _sel: Sel, _sender: Id) {
    // SAFETY: an application.
    let delegate = unsafe { (*(receiver as *const Application)).delegate };
    let notification =
        foundation::notification::make(WILL_TERMINATE, receiver, std::ptr::null_mut());
    // SAFETY: a delegate method is `void (id, SEL, NSNotification *)`.
    unsafe { send_if_implemented(delegate, "applicationWillTerminate:", notification) };
    foundation::notification::post(WILL_TERMINATE, receiver, std::ptr::null_mut());
    std::process::exit(0);
}

/// `-activateIgnoringOtherApps:`, which has nothing to activate yet.
///
/// Present rather than absent because every Cocoa `main` calls it, and a refusal
/// there would stop a program on a line that does not matter until there is a
/// window to bring forward.
unsafe extern "C" fn activate(_receiver: Id, _sel: Sel, _flag: u8) {}

/// `-setActivationPolicy:`, which answers that it succeeded.
unsafe extern "C" fn set_activation_policy(_receiver: Id, _sel: Sel, _policy: isize) -> u8 {
    1
}

/// Send `selector` to `target` if it has one, and do nothing if it does not.
///
/// An *optional* protocol method, which `applicationDidFinishLaunching:` is - so
/// a delegate that does not implement one is a program making a choice, not a
/// program that is broken. Everything else in this tree that cannot find a
/// method reports it; this is the one place where not finding it is the answer.
///
/// # Safety
///
/// `target` must be null or live, and `selector` must name a method taking one
/// object and returning nothing.
unsafe fn send_if_implemented(target: Id, selector: &str, argument: Id) {
    if target.is_null() {
        return;
    }
    let sel = objc::sel::get(selector);
    // SAFETY: the caller's obligation.
    let Some(method) = (unsafe { objc::object::class_of(target) }).and_then(|c| c.method(sel))
    else {
        return;
    };
    // SAFETY: the caller's obligation.
    let f: unsafe extern "C" fn(Id, Sel, Id) = unsafe {
        std::mem::transmute::<*const c_void, unsafe extern "C" fn(Id, Sel, Id)>(
            method.implementation(),
        )
    };
    // SAFETY: as above.
    unsafe { f(target, sel, argument) };
}

/// `int NSApplicationMain(int argc, const char *argv[])`.
///
/// The arguments are ignored, as Apple's are: everything it needs is in the
/// bundle, and a program's own arguments reached `main` before this was called.
///
/// Never returns on Darwin - `-run` ends in `exit` - and never returns here for
/// the same reason, but the return type is `int` because that is what the guest
/// declared and a call site expects `eax` to have been written.
#[expect(non_snake_case, reason = "the C spelling is the interface")]
pub unsafe extern "C" fn NSApplicationMain(_argc: i32, _argv: *const *const c_char) -> i32 {
    crate::bootstrap();
    let bundle = foundation::bundle::main();
    let principal = bundle
        .and_then(|b| b.principal_class())
        .unwrap_or("NSApplication");
    let Some(class) = std::ffi::CString::new(principal)
        .ok()
        .and_then(|n| objc::class::lookup(&n))
    else {
        objc::fail!(
            "NSApplicationMain: the bundle's NSPrincipalClass is {principal}, which no image \
             declared"
        );
    };

    let app = {
        let class_object = objc::class::ptr(class) as Id;
        // SAFETY: `+sharedApplication` is `id (Class, SEL)`, and this crate
        // installed it on `NSApplication`, from which the principal class
        // descends - which is checked below rather than assumed.
        unsafe { shared_application(class_object, objc::sel::get("sharedApplication")) }
    };
    if app.is_null() {
        objc::fail!("NSApplicationMain: {principal} could not be instantiated");
    }
    let is_application = objc::class::lookup(c"NSApplication")
        .is_some_and(|a| class.chain().any(|c| std::ptr::eq(c, a)));
    if !is_application {
        objc::fail!(
            "NSApplicationMain: the bundle's NSPrincipalClass {principal} does not descend from \
             NSApplication"
        );
    }

    match bundle.and_then(|b| b.main_nib()) {
        Some(path) => match crate::nib::load(&path, app) {
            Ok(loaded) => loaded.report(&path),
            Err(e) => objc::fail!("NSApplicationMain: {}: {e}", path.display()),
        },
        // A program with no `NSMainNibFile` is a program that wires its delegate
        // in code, which is ordinary and not an error.
        None => {}
    }

    // SAFETY: `-run` is `void (id, SEL)` and `app` is an application.
    unsafe { run(app, objc::sel::get("run")) };
    0
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSApplication").expect("declared before its methods");
    class_methods!(class, {
        "sharedApplication" => shared_application, c"@16@0:8";
    });
    methods!(class, {
        "delegate" => delegate, c"@16@0:8";
        "setDelegate:" => set_delegate, c"v24@0:8@16";
        "mainMenu" => main_menu, c"@16@0:8";
        "setMainMenu:" => set_main_menu, c"v24@0:8@16";
        "isRunning" => is_running, c"c16@0:8";
        "finishLaunching" => finish_launching, c"v16@0:8";
        "run" => run, c"v16@0:8";
        "stop:" => stop, c"v24@0:8@16";
        "terminate:" => terminate, c"v24@0:8@16";
        "activateIgnoringOtherApps:" => activate, c"v20@0:8c16";
        "setActivationPolicy:" => set_activation_policy, c"c24@0:8q16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for the singleton, because a process has one of it.
    ///
    /// Two claims: `+sharedApplication` builds an instance of the class it was
    /// *sent to*, which is what makes a bundle's `YYApplication` principal class
    /// mean anything; and the `NSApp` cell holds that object, because a guest
    /// reads `NSApp` by loading through the symbol rather than using it.
    #[test]
    fn the_shared_application_is_the_class_it_was_asked_for_and_is_what_ns_app_holds() {
        crate::bootstrap();
        let subclass = corefoundation::base::register(
            c"R3TestApplicationSubclass",
            objc::class::lookup(c"NSApplication"),
            INSTANCE_SIZE,
            false,
        );
        // SAFETY: `+sharedApplication` is `id (Class, SEL)`.
        let app = unsafe {
            shared_application(
                objc::class::ptr(subclass) as Id,
                objc::sel::get("sharedApplication"),
            )
        };
        assert!(!app.is_null());
        // SAFETY: a live object this runtime allocated.
        let class = unsafe { objc::object::class_of(app) }.expect("a class");
        assert_eq!(class.name_str(), "R3TestApplicationSubclass");

        // The same object the second time, whatever it is sent to.
        // SAFETY: as above.
        let again = unsafe {
            shared_application(
                objc::class::ptr(objc::class::lookup(c"NSApplication").expect("NSApplication"))
                    as Id,
                objc::sel::get("sharedApplication"),
            )
        };
        assert_eq!(app, again);

        // SAFETY: the cell is a word this crate leaked.
        let held = unsafe { *(crate::ns_app_cell() as *const u64) };
        assert_eq!(held, app as usize as u64);
    }

    /// `-run` turns the run loop, and `-stop:` ends it.
    ///
    /// The claim the gamepad depends on: a scheduled `IOHIDManager` delivers
    /// from a run loop source, and until this milestone nothing was turning one
    /// after `NSApplicationMain`. Asserted with a plain source rather than a
    /// device, because what is in the loop is not this module's business - that
    /// it is serviced is.
    #[test]
    fn run_services_the_run_loop_until_it_is_stopped() {
        crate::bootstrap();
        static FIRED: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        static APP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        unsafe extern "C" fn perform(_info: *mut std::ffi::c_void) {
            FIRED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let app = APP.load(std::sync::atomic::Ordering::SeqCst) as Id;
            // SAFETY: `-stop:` is `void (id, SEL, id)` and `app` is the one the
            // test made. Stopping from inside the callback rather than after a
            // few turns, because a source fires once per signal and a test that
            // waited for a second one would wait forever.
            unsafe { stop(app, objc::sel::get("stop:"), std::ptr::null_mut()) };
        }

        let app = objc::object::create_instance(
            objc::class::lookup(c"NSApplication").expect("NSApplication"),
            0,
        );
        APP.store(app as usize, std::sync::atomic::Ordering::SeqCst);

        let source = corefoundation::runloop::new_source(Some(perform), 0);
        let loop_ = corefoundation::runloop::main();
        loop_.add_source(&source, corefoundation::runloop::DEFAULT_MODE);
        source.signal();

        // SAFETY: `-run` is `void (id, SEL)` and `app` is an application.
        unsafe { run(app, objc::sel::get("run")) };

        assert_eq!(
            FIRED.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the source was not serviced from inside -run"
        );
        loop_.remove_source(&source, corefoundation::runloop::DEFAULT_MODE);
    }
}
