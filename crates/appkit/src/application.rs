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
    icon: Id,
    running: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Application>();

pub const WILL_FINISH_LAUNCHING: &str = "NSApplicationWillFinishLaunchingNotification";
pub const DID_FINISH_LAUNCHING: &str = "NSApplicationDidFinishLaunchingNotification";
pub const WILL_TERMINATE: &str = "NSApplicationWillTerminateNotification";
pub const DID_BECOME_ACTIVE: &str = "NSApplicationDidBecomeActiveNotification";
pub const WILL_RESIGN_ACTIVE: &str = "NSApplicationWillResignActiveNotification";
/// Posted when a display is added, removed or resized.
pub const DID_CHANGE_SCREEN_PARAMETERS: &str = "NSApplicationDidChangeScreenParametersNotification";

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

/// `-applicationIconImage`.
///
/// What the program itself set, and nil otherwise. Apple would answer with the
/// bundle's icon here; this does not, because the icon is decoded straight into
/// a window rather than into an `NSImage`, and an image made only to answer this
/// would be one nothing else in the runtime can draw.
unsafe extern "C" fn application_icon_image(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an application.
    unsafe { (*(receiver as *const Application)).icon }
}

/// `-setApplicationIconImage:`, which is the Dock tile on macOS and the window
/// icon elsewhere - see [`crate::icon`].
unsafe extern "C" fn set_application_icon_image(receiver: Id, _sel: Sel, image: Id) {
    // SAFETY: an application.
    unsafe { (*(receiver as *mut Application)).icon = objc::arc::retain(image) };
    crate::icon::replace(crate::image::bitmap(image));
}

unsafe extern "C" fn is_running(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: an application.
    unsafe { ((*(receiver as *const Application)).running != 0) as u8 }
}

/// `-isActive`: whether this is the frontmost application.
///
/// The same state as [`is_running`], and deliberately: nothing here ever
/// resigns active, because there is no other application for this one to be
/// behind. A game asks this to decide whether to keep rendering, so answering
/// NO is not the cautious choice - it pauses the program for good.
unsafe extern "C" fn is_active(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: an application.
    unsafe { ((*(receiver as *const Application)).running != 0) as u8 }
}

/// `-effectiveAppearance`, the light appearance this runtime renders with.
///
/// Semantic AppKit colours are Aqua throughout this runtime, so reporting the
/// same appearance keeps an application choosing assets by appearance in step
/// with the colours it receives.
unsafe extern "C" fn effective_appearance(_receiver: Id, _sel: Sel) -> Id {
    crate::appearance::aqua()
}

/// `-finishLaunching`: the two notifications, in the order Apple sends them,
/// with the launch Apple Event between them.
///
/// The delegate is sent each directly as well as through the notification
/// center, because AppKit registers a delegate for the notifications it declares
/// the moment it is set, and a program that only implements the delegate method
/// never registers as an observer of anything.
///
/// `aevt`/`oapp` is raised where Apple raises it - after the program has had
/// `applicationWillFinishLaunching:` to install its handlers, and before it is
/// told launching finished. It is the one Apple Event every application on a
/// real Mac receives, and a program that opens its untitled document from the
/// handler opens nothing without it.
unsafe extern "C" fn finish_launching(receiver: Id, _sel: Sel) {
    // SAFETY: an application.
    let delegate = unsafe { (*(receiver as *const Application)).delegate };
    for (name, selector) in [
        (WILL_FINISH_LAUNCHING, "applicationWillFinishLaunching:"),
        (DID_FINISH_LAUNCHING, "applicationDidFinishLaunching:"),
    ] {
        if name == DID_FINISH_LAUNCHING {
            // SAFETY: an application, and a delegate the program set.
            unsafe { open_launch_urls(receiver, delegate) };
            foundation::appleevent::raise(
                foundation::appleevent::CORE_EVENT_CLASS,
                foundation::appleevent::OPEN_APPLICATION,
            );
        }
        let notification = foundation::notification::make(name, receiver, std::ptr::null_mut());
        // SAFETY: a delegate method is `void (id, SEL, NSNotification *)`.
        unsafe { send_if_implemented(delegate, selector, notification) };
        foundation::notification::post(name, receiver, std::ptr::null_mut());
    }
}

/// The URLs this launch was asked to open, in the order they were given.
fn launch_urls() -> Vec<String> {
    fn scheme(argument: &str) -> bool {
        let Some((scheme, rest)) = argument.split_once(':') else {
            return false;
        };
        // Two characters at least, so a lone drive letter or a stray `x:` is
        // not mistaken for one, and a body.
        scheme.len() >= 2
            && !rest.is_empty()
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    }
    foundation::processinfo::guest_arguments()
        .iter()
        .skip(1)
        .filter_map(|argument| argument.to_str().map(str::to_owned))
        .filter(|argument| scheme(argument))
        .collect()
}

/// `-[delegate application:openURLs:]`, where Apple sends it: after the program
/// has had `applicationWillFinishLaunching:` to build whatever handles a URL,
/// and before it is told launching finished, because a delegate that defers the
/// URL to "after initialization" is waiting on the notification that has not
/// been sent yet.
///
/// Nothing is sent for a launch with no URLs, which is the ordinary case - Apple
/// does not send the method either, and a delegate is entitled to treat being
/// called at all as meaning there is a URL.
///
/// # Safety
///
/// `receiver` is an application and `delegate` is null or live.
unsafe fn open_launch_urls(receiver: Id, delegate: Id) {
    let urls = launch_urls();
    if urls.is_empty() || delegate.is_null() {
        return;
    }
    let objects: Vec<corefoundation::base::CFTypeRef> = urls
        .iter()
        .map(|url| corefoundation::url::new(url))
        .collect();
    let array = corefoundation::collections::new_array(
        &objects,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // The array retained each URL, so this call's own references are done with.
    for object in &objects {
        // SAFETY: a URL this call just made.
        unsafe { corefoundation::api::CFRelease(*object) };
    }
    let sel = objc::sel::get("application:openURLs:");
    // SAFETY: the caller's obligation.
    let method = (unsafe { objc::object::class_of(delegate) }).and_then(|c| c.method(sel));
    if let Some(method) = method {
        // SAFETY: `-application:openURLs:` is `void (id, SEL, NSApplication *,
        // NSArray<NSURL *> *)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, Id, Id) = std::mem::transmute::<
                *const c_void,
                unsafe extern "C" fn(Id, Sel, Id, Id),
            >(method.implementation());
            f(delegate, sel, receiver, array as Id);
        }
    }
    // SAFETY: the array this call made, which the delegate retains if it keeps it.
    unsafe { corefoundation::api::CFRelease(array) };
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
        // The window system first, and every turn. A window that is never
        // pumped is one the platform has not been told to show: on macOS SDL's
        // Cocoa backend needs its events drained for the window to appear at
        // all, so this is not only about delivering input.
        crate::event::pump(receiver);
        // The main queue is drained here because this is the main thread: a block
        // sent to it with `dispatch_async` runs nowhere else, so a run loop that
        // never drained would leave every such block queued forever.
        darwin::dispatch::queue::drain_main();
        // A short slice rather than a long one: the run loop's timers are what
        // drive a frame - see `corevideo` - and a turn that blocked for a tenth
        // of a second would cap the frame rate at ten.
        let result = loop_.run_in_mode(corefoundation::runloop::DEFAULT_MODE, 0.002, false);
        if result == corefoundation::runloop::result::FINISHED {
            // Nothing scheduled: a program with no display link and no source is
            // waiting to be told to stop, and spinning on that would be a core
            // at a hundred per cent for a window that is only being looked at.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Seconds until a deadline, or zero for a date this runtime cannot read.
fn wait_interval(until: Id) -> f64 {
    if until.is_null() {
        return 0.0;
    }
    // SAFETY: `-timeIntervalSinceNow` is `double (id, SEL)` on an `NSDate`.
    let seconds = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> f64 = std::mem::transmute(objc::msg::entry());
        f(until, objc::sel::get("timeIntervalSinceNow"))
    };
    seconds.max(0.0)
}

/// `-nextEventMatchingMask:untilDate:inMode:dequeue:`, which is always nil.
///
/// A program that drives its own loop - UE3 does - calls this instead of
/// `-run`, so the pump has to happen here too or its window is never serviced.
/// What it does not get is an event *object*: this tree turns a window-system
/// event straight into the message to the responder it names (`event::deliver`)
/// rather than keeping a queue of `NSEvent`s, so there is nothing to hand back.
/// Input still arrives; forwarding it by hand is what is missing.
unsafe extern "C" fn next_event(
    receiver: Id,
    _sel: Sel,
    _mask: u64,
    until: Id,
    _mode: Id,
    _dequeue: u8,
) -> Id {
    crate::event::pump(receiver);
    // Apple blocks until the deadline when there is no event. Nothing here can
    // wake, so the wait is a short sleep: a loop asking for the next event with
    // a distant deadline and answered nil at once would spin a core.
    if wait_interval(until) > 0.0 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::ptr::null_mut()
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

/// `NSApplicationPresentationOptions`: whether the menu bar and Dock are hidden.
///
/// Kept and reported rather than acted on. There is no menu bar and no Dock in
/// an SDL window, so the *effect* of hiding them is already the case; what a
/// program needs is for the value it set to be the value it reads back, because
/// a game toggling full screen compares the two.
static PRESENTATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn presentation_options(_receiver: Id, _sel: Sel) -> usize {
    PRESENTATION.load(std::sync::atomic::Ordering::Acquire)
}

unsafe extern "C" fn set_presentation_options(_receiver: Id, _sel: Sel, options: usize) {
    PRESENTATION.store(options, std::sync::atomic::Ordering::Release);
}

/// The windows this runtime has made, in the order they were made.
unsafe extern "C" fn windows(_receiver: Id, _sel: Sel) -> Id {
    let objects: Vec<corefoundation::base::CFTypeRef> = crate::window::all()
        .into_iter()
        .map(|w| w as corefoundation::base::CFTypeRef)
        .collect();
    let array = corefoundation::collections::new_array(
        &objects,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn key_window(_receiver: Id, _sel: Sel) -> Id {
    crate::window::key_window()
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
        "applicationIconImage" => application_icon_image, c"@16@0:8";
        "setApplicationIconImage:" => set_application_icon_image, c"v24@0:8@16";
        "isRunning" => is_running, c"c16@0:8";
        "isActive" => is_active, c"c16@0:8";
        "effectiveAppearance" => effective_appearance, c"@16@0:8";
        "finishLaunching" => finish_launching, c"v16@0:8";
        "run" => run, c"v16@0:8";
        "stop:" => stop, c"v24@0:8@16";
        "terminate:" => terminate, c"v24@0:8@16";
        "activateIgnoringOtherApps:" => activate, c"v20@0:8c16";
        "setActivationPolicy:" => set_activation_policy, c"c24@0:8q16";
        "currentSystemPresentationOptions" => presentation_options, c"Q16@0:8";
        "presentationOptions" => presentation_options, c"Q16@0:8";
        "setPresentationOptions:" => set_presentation_options, c"v24@0:8Q16";
        "nextEventMatchingMask:untilDate:inMode:dequeue:" => next_event, c"@48@0:8Q16@24@32B40";
        "windows" => windows, c"@16@0:8";
        "keyWindow" => key_window, c"@16@0:8";
        "mainWindow" => key_window, c"@16@0:8";
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

    #[test]
    fn only_scheme_shaped_arguments_are_launch_urls() {
        crate::bootstrap();
        foundation::processinfo::set_arguments(
            &[
                "roblox://not-argv-zero",
                "-protocolString",
                "roblox://experiences/start?placeId=1",
                "--place",
                "/Users/me/a:file",
                "roblox-player:1+launchmode:play",
                "x:",
                "a:b",
            ]
            .map(Into::into),
        );
        assert_eq!(
            launch_urls(),
            [
                "roblox://experiences/start?placeId=1",
                "roblox-player:1+launchmode:play",
            ]
        );
        foundation::processinfo::set_arguments(&["guest".into()]);
        assert!(launch_urls().is_empty());
    }

    /// Where the URL arrives, which is the whole of the contract: a delegate
    /// that defers a cold-launch URL until initialization has finished is
    /// waiting for `applicationDidFinishLaunching:`, so a URL delivered after
    /// it is a URL delivered too late.
    #[test]
    fn a_launch_url_reaches_the_delegate_between_the_two_notifications() {
        crate::bootstrap();
        static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        unsafe extern "C" fn will_finish(_receiver: Id, _sel: Sel, _notification: Id) {
            SEEN.lock().expect("no panic").push(String::from("will"));
        }
        unsafe extern "C" fn did_finish(_receiver: Id, _sel: Sel, _notification: Id) {
            SEEN.lock().expect("no panic").push(String::from("did"));
        }
        unsafe extern "C" fn open_urls(_receiver: Id, _sel: Sel, _app: Id, urls: Id) {
            // SAFETY: an `NSArray` of `NSURL`, which is what is sent.
            let first = unsafe {
                let f: unsafe extern "C" fn(Id, Sel) -> Id =
                    std::mem::transmute(objc::msg::entry());
                f(
                    f(urls, objc::sel::get("firstObject")),
                    objc::sel::get("absoluteString"),
                )
            };
            // SAFETY: an `NSString` the URL answered with.
            let bytes = unsafe { foundation::string::bytes(first) };
            let text = String::from_utf8_lossy(&bytes).into_owned();
            SEEN.lock().expect("no panic").push(text);
        }

        let delegate_class = corefoundation::base::register(
            c"R3TestOpenUrlsDelegate",
            objc::class::lookup(c"NSObject"),
            size_of::<usize>(),
            false,
        );
        for (name, imp) in [
            (
                "applicationWillFinishLaunching:",
                will_finish as *const c_void,
            ),
            (
                "applicationDidFinishLaunching:",
                did_finish as *const c_void,
            ),
        ] {
            delegate_class.add_method(objc::sel::get(name), imp, c"v24@0:8@16");
        }
        delegate_class.add_method(
            objc::sel::get("application:openURLs:"),
            open_urls as *const c_void,
            c"v32@0:8@16@24",
        );
        let delegate = objc::object::create_instance(delegate_class, 0);

        let class = objc::class::lookup(c"NSApplication").expect("NSApplication");
        let app = objc::object::create_instance(class, INSTANCE_SIZE);
        // SAFETY: an application this test made.
        unsafe { set_delegate(app, objc::sel::get("setDelegate:"), delegate) };
        foundation::processinfo::set_arguments(
            &[
                "guest",
                "-protocolString",
                "roblox://experiences/start?placeId=1",
            ]
            .map(Into::into),
        );
        // SAFETY: `-finishLaunching` is `void (id, SEL)`.
        unsafe { finish_launching(app, objc::sel::get("finishLaunching")) };
        foundation::processinfo::set_arguments(&["guest".into()]);

        assert_eq!(
            *SEEN.lock().expect("no panic"),
            ["will", "roblox://experiences/start?placeId=1", "did"]
        );
    }

    #[test]
    fn effective_appearance_is_the_named_aqua_appearance() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSApplication").expect("NSApplication");
        let app = objc::object::create_instance(class, 0);
        let appearance =
            unsafe { effective_appearance(app, objc::sel::get("effectiveAppearance")) };
        let name = unsafe { crate::appearance::name(appearance, objc::sel::get("name")) };
        assert_eq!(
            unsafe { foundation::string::bytes(name) },
            b"NSAppearanceNameAqua"
        );
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
        static FIRED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
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
