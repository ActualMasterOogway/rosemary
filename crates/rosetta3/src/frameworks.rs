//! The frameworks this tree implements, and whether they are the ones in use.
//!
//! Each is selected the same way [`crate::objc::host`] selects a runtime, and
//! for the same reason: a framework written here can then be exercised on macOS,
//! where its answers can be compared against Apple's, instead of only on the
//! host that has no alternative.
//!
//! The selection is not free-standing. Every CoreFoundation object is an object
//! of *this tree's* Objective-C runtime, so a builtin CoreFoundation over the
//! host's libobjc would hand Apple's runtime structures it would read as its
//! own. The frameworks therefore follow `ROSETTA3_OBJC` rather than carrying a
//! switch each.

use machish::resolve::{NativeDylibProvider, SyntheticDylib};

use crate::objc::host::{Backend, backend};

/// Whether this process is using the frameworks in this tree.
pub fn builtin() -> bool {
    backend() == Backend::Builtin
}

/// Tell `darwin` where to find these, so that a `dlsym` and the loader agree.
///
/// Called from everything below rather than from start-up: `darwin::symbol` is
/// asked during linking, which happens before `startup::run`, and a registration
/// that lands after the first question is a registration that did not happen.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        darwin::register_provider(symbol);
        register_dlopen_names();
        install_block_runtime();
        push_main_pool();
    });
}

/// Tell `darwin`'s libdispatch which block runtime this process is using.
///
/// `dispatch_async` has to copy the block it is given, because the block outlives its
/// caller's frame - but a block carries its runtime's `_NSConcreteMallocBlock` in its
/// `isa`, so whichever runtime copies it has to be the one that later releases it.
/// `darwin` therefore implements neither and asks here, where the selection that
/// [`crate::objc::host`] already made is known.
fn install_block_runtime() {
    let (Some(copy), Some(release)) = (
        crate::objc::host::symbol("_Block_copy"),
        crate::objc::host::symbol("_Block_release"),
    ) else {
        // Not an error to leave unset: on macOS nothing binds `darwin`'s libdispatch
        // at all. Where something does, a block-taking entry point refuses rather
        // than queueing a block it cannot keep alive.
        return;
    };
    darwin::dispatch::block::set_runtime(copy, release);
}

/// Every library in this tree, and the paths a guest opens it by.
///
/// A guest that `dlopen`s a framework this runtime implements must get a live
/// handle rather than null: the frameworks have no file on disk, so the
/// filesystem search in `dyld::resolve_path` can never find one. It also stops
/// the *host's* framework being loaded in its place, which under the builtin
/// runtime would put Apple's classes in front of a guest that has ours.
///
/// Both spellings of each: the install name, and the versioned path an
/// `LC_LOAD_DYLIB` records.
fn register_dlopen_names() {
    if !builtin() {
        return;
    }
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
            "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
            "CoreFoundation",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/Metal.framework/Versions/A/Metal",
            "/System/Library/Frameworks/Metal.framework/Metal",
            "Metal",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/ForceFeedback.framework/Versions/A/ForceFeedback",
            "/System/Library/Frameworks/ForceFeedback.framework/ForceFeedback",
            "ForceFeedback",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation",
            "/System/Library/Frameworks/Foundation.framework/Foundation",
            "Foundation",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/IOKit.framework/Versions/A/IOKit",
            "/System/Library/Frameworks/IOKit.framework/IOKit",
            "IOKit",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/AppKit.framework/Versions/C/AppKit",
            "/System/Library/Frameworks/AppKit.framework/AppKit",
            "AppKit",
            // Cocoa is the umbrella, and a program that links it imports
            // AppKit's symbols through that name.
            "/System/Library/Frameworks/Cocoa.framework/Versions/A/Cocoa",
            "/System/Library/Frameworks/Cocoa.framework/Cocoa",
            "Cocoa",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/CoreVideo.framework/Versions/A/CoreVideo",
            "/System/Library/Frameworks/CoreVideo.framework/CoreVideo",
            "CoreVideo",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/Contacts.framework/Versions/A/Contacts",
            "/System/Library/Frameworks/Contacts.framework/Contacts",
            "Contacts",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/GameController.framework/Versions/A/GameController",
            "/System/Library/Frameworks/GameController.framework/GameController",
            "GameController",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/WebKit.framework/Versions/A/WebKit",
            "/System/Library/Frameworks/WebKit.framework/WebKit",
            "WebKit",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/ApplicationServices.framework/Versions/A/ApplicationServices",
            "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices",
        ],
        symbol,
    );
    crate::dyld::register_synthetic(
        &[
            "/System/Library/Frameworks/AudioToolbox.framework/Versions/A/AudioToolbox",
            "/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox",
        ],
        symbol,
    );
    register_opengl_framework();
}

/// `OpenGL.framework`, as a handle over the GL library SDL is using.
///
/// Without a handle for that path the loader returns null, every `dlsym` after
/// it answers null, and the program reports **"Missing GL version"** and then
/// "OpenGL 3.2 support or higher required" - a version check that failed
/// because there was no library to ask, not because the driver is old.
///
/// The other half of [`crate::opengl`]: the same answers a link gets, so that a
/// program which resolves half its GL by import and half by `dlsym` draws
/// through one library either way.
fn register_opengl_framework() {
    if !crate::opengl::interposed() {
        return;
    }
    crate::dyld::register_synthetic(crate::opengl::NAMES, crate::opengl::symbol);
}

/// Push the outermost autorelease pool, the one a process is born with.
///
/// A real Darwin process reaches `main` inside a pool the runtime pushed, and
/// `NSApplicationMain` pushes another per event. Without the outer one, the
/// first `-autorelease` before the guest makes a pool of its own has nowhere to
/// go, and this runtime says so and leaks it - which is Apple's behaviour and
/// still the wrong thing to reproduce here, because it happens on the way in
/// rather than in the guest's own code.
///
/// Here rather than in `startup::run`, because class realization autoreleases
/// and that happens while the program is still being loaded. On the thread that
/// first builds the provider chain, which is the one the program starts on.
/// Never popped: it is the process's, and the process ending is what drains it.
fn push_main_pool() {
    if !builtin() {
        return;
    }
    objc::arc::pool_push();
}

/// Whether `ROSETTA3_WITHHOLD` names this symbol, so the loader leaves it unbound.
///
/// The A/B this tree could not otherwise run. A framework answer that is subtly
/// wrong is invisible - the guest takes a different branch and something stops
/// working three layers away - and the way to find one is to compare against a
/// runtime that does not answer at all. Withholding a name here reproduces that
/// side of the comparison without a rebuild.
///
/// What it costs is the difference `appkit::NOTIFICATION_NAMES` states: a
/// withheld **data** symbol binds to zero and a message to it answers nil, which
/// is survivable and is the interesting case; a withheld **function** faults at
/// the call. So this is a diagnostic and not a mode, and it says what it did.
fn withheld(name: &str) -> bool {
    #[static_init::dynamic]
    static NAMES: Vec<String> = std::env::var("ROSETTA3_WITHHOLD")
        .unwrap_or_default()
        .split(',')
        .map(|n| n.trim().trim_start_matches('_').to_string())
        .filter(|n| !n.is_empty())
        .collect();

    let bare = name.trim_start_matches('_');
    let out = NAMES.iter().any(|n| n == bare);
    if out {
        eprintln!("rosetta3: {name}: withheld (ROSETTA3_WITHHOLD names it)");
    }
    out
}

fn define_fn(lib: &mut SyntheticDylib, name: &str, address: *const ()) {
    if !withheld(name) {
        lib.define_fn(name, address);
    }
}

fn define_data(lib: &mut SyntheticDylib, name: &str, address: u64) {
    if !withheld(name) {
        lib.define_data(name, address);
    }
}

/// The frameworks, as a library the loader can bind against.
///
/// Flat, like the rest of this runtime's providers: which framework an image
/// thinks `CFRelease` comes from is not the question.
pub fn provider() -> SyntheticDylib {
    install();
    let mut lib = SyntheticDylib::new("<rosetta3 frameworks>");
    lib.flat(true);
    if !builtin() {
        return lib;
    }
    // Before anything is published: `kCFNull` is the `NSNull` singleton, and
    // `NSNull` is Foundation's class. A provider built before Foundation had
    // registered it would publish a null for that symbol, which is the very
    // failure the globals exist to avoid.
    foundation::bootstrap();
    for name in corefoundation::names() {
        if let Some(addr) = corefoundation::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in corefoundation::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    // The class objects, which are data: a guest's `[NSMutableArray array]`
    // binds `_OBJC_CLASS_$_NSMutableArray` and sends to whatever it gets.
    for (name, addr) in foundation::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    for name in foundation::names() {
        if let Some(addr) = foundation::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // IOKit's HID half, on SDL3. Published even where there is no SDL3: a guest
    // then gets a manager that declines to open, which is a diagnosis, where an
    // unresolved import would only say the symbol was missing.
    for name in iokit::names() {
        if let Some(addr) = iokit::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in iokit::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    // ForceFeedback translates the legacy effect API into SDL3 joystick rumble.
    for name in forcefeedback::NAMES {
        if let Some(addr) = forcefeedback::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // AppKit up to the point a window would be needed. Its classes are here
    // even though none of them draws: a guest's `YYGLView : NSView` binds
    // `_OBJC_CLASS_$_NSView` and is refused whole without it, where a declared
    // superclass turns that into one refusal per selector actually sent.
    for name in appkit::names() {
        if let Some(addr) = appkit::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // ApplicationServices
    for name in applicationservices::names() {
        if let Some(addr) = applicationservices::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // AudioToolbox
    for name in audiotoolbox::names() {
        if let Some(addr) = audiotoolbox::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // CoreVideo, which is one thing: the display link a Mac game's frame loop
    // hangs on. Its callback is guest code, and reaches this runtime as a thunk
    // because `abi::table` names that parameter a callback.
    for name in corevideo::NAMES {
        if let Some(addr) = corevideo::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // CoreAudio's output half, on SDL3. Published even where there is no SDL3,
    // for the reason IOKit's is: a unit that declines to open is a diagnosis,
    // where an unresolved import only says the symbol was missing.
    for name in coreaudio::NAMES {
        if let Some(addr) = coreaudio::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    // Contacts has no backing store. Its one class reports that access is
    // restricted, which lets applications disable contact-dependent features.
    for (name, addr) in contacts::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    for (name, addr) in gamecontroller::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    for (name, addr) in webkit::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    for (name, addr) in appkit::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    for (name, addr) in media_constants() {
        define_data(&mut lib, &name, addr);
    }
    for (name, addr) in metal::data_symbols() {
        define_data(&mut lib, &name, addr);
    }
    for name in metal::NAMES {
        if let Some(addr) = metal::symbol(name) {
            define_fn(&mut lib, &format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in shimmed() {
        define_fn(&mut lib, &format!("_{name}"), addr);
    }
    lib
}

/// String globals belonging to frameworks this tree does not implement at all.
///
/// Published anyway, and only because of the difference AppKit's
/// `NOTIFICATION_NAMES` states: an unresolved *function* import is refused by
/// name at the call, while an unresolved **data** import binds to zero and the
/// guest dereferences it. A program that registers for
/// `AVCaptureDeviceWasConnectedNotification` before it ever touches a camera
/// therefore segfaults on the `objc_msgSend` that passes the name, with nothing
/// to say which symbol it was.
///
/// Nothing posts any of these, which is the honest state: there is no capture
/// session here. A name nothing posts is a gap the program survives.
///
/// The values are Apple's own, not the symbol names. Three of them differ, and
/// they are the ones a program compares rather than merely passes on:
/// `AVMediaTypeVideo` is the four-character code `vide`.
const MEDIA_CONSTANTS: &[(&str, &str)] = &[
    (
        "_AVCaptureDeviceWasConnectedNotification",
        "AVCaptureDeviceWasConnectedNotification",
    ),
    (
        "_AVCaptureDeviceWasDisconnectedNotification",
        "AVCaptureDeviceWasDisconnectedNotification",
    ),
    (
        "_AVCaptureSessionDidStartRunningNotification",
        "AVCaptureSessionDidStartRunningNotification",
    ),
    (
        "_AVCaptureSessionDidStopRunningNotification",
        "AVCaptureSessionDidStopRunningNotification",
    ),
    (
        "_AVCaptureSessionRuntimeErrorNotification",
        "AVCaptureSessionRuntimeErrorNotification",
    ),
    ("_AVCaptureSessionErrorKey", "AVCaptureSessionErrorKey"),
    ("_AVMediaTypeVideo", "vide"),
    ("_AVMediaTypeAudio", "soun"),
    ("_AVMediaTypeText", "text"),
];

/// Each of [`MEDIA_CONSTANTS`] as a cell holding an `NSString *`.
///
/// A cell rather than the string itself, for the reason `foundation`'s own
/// globals go through one: the symbol names a *variable* holding the pointer,
/// and a guest loads it before dereferencing.
fn media_constants() -> Vec<(String, u64)> {
    MEDIA_CONSTANTS
        .iter()
        .map(|(symbol, text)| {
            (
                (*symbol).to_string(),
                corefoundation::base::global_cell(
                    symbol,
                    foundation::notification::name_object(text),
                ),
            )
        })
        .collect()
}

/// Names this tree answers only as a shim, and the address the loader binds.
///
/// A shim is reached by *name* on the way through an intercepted call, so under
/// the emulated backend the address only has to resolve here rather than
/// somewhere else - see the roadmap's "a shim needs its import to resolve
/// first". The native backend intercepts nothing, so the address is the whole
/// implementation there, and a stub is what says which one is still missing.
fn shimmed() -> Vec<(&'static str, *const ())> {
    vec![
        ("NSLog", crate::shims::nslog::native::nslog() as *const ()),
        (
            "NSRunAlertPanel",
            crate::shims::nslog::native::run_alert_panel() as *const (),
        ),
    ]
}

pub fn symbol(name: &str) -> Option<u64> {
    if !builtin() {
        return None;
    }
    foundation::bootstrap();
    // A dlsym answer must be the same address the flat provider binds.
    corefoundation::symbol(name)
        // libclosure, which a shim forwards to by name - see
        // `crate::shims::block`. Nothing on a host off Darwin defines it.
        .or_else(|| objc::block::symbol(name))
        .or_else(|| foundation::symbol(name))
        .or_else(|| iokit::symbol(name))
        .or_else(|| forcefeedback::symbol(name))
        .or_else(|| appkit::symbol(name))
        .or_else(|| applicationservices::symbol(name))
        .or_else(|| corevideo::symbol(name))
        .or_else(|| crate::opengl::symbol(name))
        .or_else(|| coreaudio::symbol(name))
        .or_else(|| gamecontroller::symbol(name))
        .or_else(|| webkit::symbol(name))
        .or_else(|| metal::symbol(name))
}

/// Tell this tree's `NSBundle` which `.app` the guest is.
///
/// Nothing here reads the *host* process's path, which is why the builtin path
/// needs no re-exec: see `rosetta3::bundle` for the hack this replaces, and
/// `foundation::bundle::set_main` for what is told.
///
/// A loose binary is not an error - most guests are one - so a `false` here is
/// only reported under the trace.
pub fn set_main_bundle(executable: &std::path::Path) {
    install();
    if !builtin() {
        return;
    }
    let found = foundation::bundle::set_main(executable);
    // Before anything brings SDL up, which is what makes it reach the audio
    // server and the compositor: see `appkit::metadata`.
    appkit::metadata::publish(executable);
    if !found && std::env::var_os("ROSETTA3_TRACE_VM").is_some() {
        eprintln!(
            "rosetta3: bundle: {} is not inside a .app",
            executable.display()
        );
    }
}

/// Keep the host's own framework symbols away from a guest using these.
///
/// The same argument as [`crate::objc::host::deny_foreign_classes`]: an object
/// from Apple's CoreFoundation handed to this one is a foreign structure read as
/// a native one. `darwin::divergent` already denies the `CF` and `NS` families
/// off macOS; this is what denies them *on* macOS when the builtin frameworks
/// are the ones in use.
pub fn deny_host_frameworks(provider: &mut NativeDylibProvider) {
    install();
    if !builtin() {
        return;
    }
    // `_SC` and Contacts with them, and for the same reason one step removed:
    // SystemConfiguration is Apple's own and takes CoreFoundation objects, so
    // `SCDynamicStoreCreate` handed one of this tree's strings is Apple's
    // CoreFoundation reading a structure this runtime laid out. Denied whole
    // rather than per name, so that an entry point `corefoundation::sysconfig`
    // does not implement is refused by name instead of corrupting memory. An
    // an unimplemented Contacts, GameController or WebKit call likewise must
    // not hand an object from this tree to Apple's corresponding framework.
    for prefix in [
        "_CF", "_NS", "_kCF", "_kNS", "_SC", "_kSC", "_CN", "_kCN", "_GC", "_kGC", "_Web",
    ] {
        provider.deny_prefix(prefix);
    }
}
