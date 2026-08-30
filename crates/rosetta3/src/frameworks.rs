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
        push_main_pool();
    });
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
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in corefoundation::data_symbols() {
        lib.define_data(&name, addr);
    }
    // The class objects, which are data: a guest's `[NSMutableArray array]`
    // binds `_OBJC_CLASS_$_NSMutableArray` and sends to whatever it gets.
    for (name, addr) in foundation::data_symbols() {
        lib.define_data(&name, addr);
    }
    for name in foundation::names() {
        if let Some(addr) = foundation::symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    // IOKit's HID half, on SDL3. Published even where there is no SDL3: a guest
    // then gets a manager that declines to open, which is a diagnosis, where an
    // unresolved import would only say the symbol was missing.
    for name in iokit::NAMES {
        if let Some(addr) = iokit::symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in iokit::data_symbols() {
        lib.define_data(&name, addr);
    }
    // AppKit up to the point a window would be needed. Its classes are here
    // even though none of them draws: a guest's `YYGLView : NSView` binds
    // `_OBJC_CLASS_$_NSView` and is refused whole without it, where a declared
    // superclass turns that into one refusal per selector actually sent.
    for name in appkit::NAMES {
        if let Some(addr) = appkit::symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    // CoreVideo, which is one thing: the display link a Mac game's frame loop
    // hangs on. Its callback is guest code, and reaches this runtime as a thunk
    // because `abi::table` names that parameter a callback.
    for name in corevideo::NAMES {
        if let Some(addr) = corevideo::symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    // CoreAudio's output half, on SDL3. Published even where there is no SDL3,
    // for the reason IOKit's is: a unit that declines to open is a diagnosis,
    // where an unresolved import only says the symbol was missing.
    for name in coreaudio::NAMES {
        if let Some(addr) = coreaudio::symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in appkit::data_symbols() {
        lib.define_data(&name, addr);
    }
    for (name, addr) in shimmed() {
        lib.define_fn(&format!("_{name}"), addr);
    }
    lib
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
        ("NSRunAlertPanel", unintercepted_alert as *const ()),
    ]
}

unsafe extern "C" fn unintercepted_alert() -> u64 {
    objc::fail!("NSRunAlertPanel was called rather than intercepted")
}

pub fn symbol(name: &str) -> Option<u64> {
    if !builtin() {
        return None;
    }
    foundation::bootstrap();
    corefoundation::symbol(name)
        .or_else(|| foundation::symbol(name))
        .or_else(|| iokit::symbol(name))
        .or_else(|| appkit::symbol(name))
        .or_else(|| corevideo::symbol(name))
        .or_else(|| coreaudio::symbol(name))
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
    for prefix in ["_CF", "_NS", "_kCF", "_kNS"] {
        provider.deny_prefix(prefix);
    }
}
