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

use exports::Exports;
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
/// A framework this tree implements, and everything the three published views of
/// it are derived from.
///
/// They used to be three hand-kept lists - the paths a `dlopen` answers, the
/// loop that publishes, the chain a `dlsym` walks - and a framework in one and
/// not another is a name that binds and cannot be found, or a path that returns
/// null. `AudioToolbox` had been missing from the first.
struct Framework {
    /// The leaf, which is the framework directory's name and the spelling a
    /// `dlopen("AppKit")` uses.
    leaf: &'static str,
    /// The `Versions/` letter a real install carries: C for the two frameworks
    /// old enough to have one, A for everything younger.
    version: char,
    /// Umbrellas that re-export it. A program that links Cocoa imports AppKit's
    /// symbols through that name.
    umbrellas: &'static [&'static str],
    /// Its entry points, which the generated table publishes.
    provide: Option<fn(&mut dyn exports::Exports)>,
    /// Its data symbols, which are objects built at run time rather than a list.
    data: Option<fn() -> Vec<(String, u64)>>,
    /// What a `dlsym` of a bare C name answers. `None` where the framework
    /// publishes only class objects, which a guest binds at link and never asks
    /// for by name - see `contacts`.
    symbol: Option<fn(&str) -> Option<u64>>,
}

/// In the order they were published in before this was one list, because the
/// library underneath is a map and the last definition of a name would win.
/// `no_framework_defines_a_symbol_another_one_does` is what says the order
/// cannot matter.
static FRAMEWORKS: &[Framework] = &[
    Framework {
        leaf: "CoreFoundation",
        version: 'A',
        umbrellas: &[],
        provide: Some(corefoundation::provide),
        data: Some(corefoundation::data_symbols),
        symbol: Some(corefoundation::symbol),
    },
    Framework {
        leaf: "Foundation",
        version: 'C',
        umbrellas: &[],
        provide: Some(foundation::provide),
        data: Some(foundation::data_symbols),
        symbol: Some(foundation::symbol),
    },
    Framework {
        leaf: "IOKit",
        version: 'A',
        umbrellas: &[],
        provide: Some(iokit::provide),
        data: Some(iokit::data_symbols),
        symbol: Some(iokit::symbol),
    },
    Framework {
        leaf: "ForceFeedback",
        version: 'A',
        umbrellas: &[],
        provide: Some(forcefeedback::provide),
        data: None,
        symbol: Some(forcefeedback::symbol),
    },
    Framework {
        leaf: "AppKit",
        version: 'C',
        umbrellas: &["Cocoa"],
        provide: Some(appkit::provide),
        data: Some(appkit::data_symbols),
        symbol: Some(appkit::symbol),
    },
    Framework {
        leaf: "ApplicationServices",
        version: 'A',
        umbrellas: &[],
        provide: Some(applicationservices::provide),
        data: None,
        symbol: Some(applicationservices::symbol),
    },
    Framework {
        leaf: "AudioToolbox",
        version: 'A',
        umbrellas: &[],
        provide: Some(audiotoolbox::provide),
        data: None,
        symbol: Some(audiotoolbox::symbol),
    },
    Framework {
        leaf: "CoreVideo",
        version: 'A',
        umbrellas: &[],
        provide: Some(corevideo::provide),
        data: None,
        symbol: Some(corevideo::symbol),
    },
    Framework {
        leaf: "CoreAudio",
        version: 'A',
        umbrellas: &[],
        provide: Some(coreaudio::provide),
        data: None,
        symbol: Some(coreaudio::symbol),
    },
    Framework {
        leaf: "Contacts",
        version: 'A',
        umbrellas: &[],
        provide: None,
        data: Some(contacts::data_symbols),
        symbol: None,
    },
    Framework {
        leaf: "Security",
        version: 'A',
        umbrellas: &[],
        provide: Some(security::provide),
        data: Some(security::data_symbols),
        symbol: Some(security::symbol),
    },
    Framework {
        leaf: "GameController",
        version: 'A',
        umbrellas: &[],
        provide: None,
        data: Some(gamecontroller::data_symbols),
        symbol: Some(gamecontroller::symbol),
    },
    Framework {
        leaf: "WebKit",
        version: 'A',
        umbrellas: &[],
        provide: None,
        data: Some(webkit::data_symbols),
        symbol: Some(webkit::symbol),
    },
    Framework {
        leaf: "Metal",
        version: 'A',
        umbrellas: &[],
        provide: Some(metal::provide),
        data: Some(metal::data_symbols),
        symbol: Some(metal::symbol),
    },
];

/// The spellings a guest opens `framework` by.
///
/// Both the install name and the versioned path an `LC_LOAD_DYLIB` records, and
/// the bare leaf - `dyld::synthetic::find` matches a query against a registered
/// name by the whole path or by the query's leaf, never by the leaf of the
/// registered name, so a framework without its bare spelling is one a
/// `dlopen("AppKit")` cannot reach.
fn paths_of(framework: &Framework) -> Vec<String> {
    let mut out = Vec::new();
    for leaf in std::iter::once(framework.leaf).chain(framework.umbrellas.iter().copied()) {
        // An umbrella is always Versions/A; only what it re-exports is old
        // enough to be C.
        let version = if leaf == framework.leaf {
            framework.version
        } else {
            'A'
        };
        out.push(format!(
            "/System/Library/Frameworks/{leaf}.framework/Versions/{version}/{leaf}"
        ));
        out.push(format!(
            "/System/Library/Frameworks/{leaf}.framework/{leaf}"
        ));
        out.push(leaf.to_string());
    }
    out
}

/// Every library in this tree, and the paths a guest opens it by.
///
/// A guest that `dlopen`s a framework this runtime implements must get a live
/// handle rather than null: the frameworks have no file on disk, so the
/// filesystem search in `dyld::resolve_path` can never find one. It also stops
/// the *host's* framework being loaded in its place, which under the builtin
/// runtime would put Apple's classes in front of a guest that has ours.
fn register_dlopen_names() {
    if !builtin() {
        return;
    }
    for framework in FRAMEWORKS {
        let paths = paths_of(framework);
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        crate::dyld::register_synthetic(&borrowed, symbol);
    }
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
    crate::dyld::register_synthetic(crate::opengl::INSTALL_PATHS, crate::opengl::symbol);
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
    let out = &mut exports::Denying {
        inner: &mut lib,
        deny: withheld,
    };
    for framework in FRAMEWORKS {
        if let Some(provide) = framework.provide {
            provide(out);
        }
        if let Some(data) = framework.data {
            for (name, addr) in data() {
                out.data(&name, addr);
            }
        }
    }
    for (name, addr) in media_constants() {
        out.data(&name, addr);
    }
    for (name, addr) in shimmed() {
        out.function(&format!("_{name}"), addr as usize as u64);
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
    corefoundation::base::string_globals(MEDIA_CONSTANTS)
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
    // A dlsym answer must be the same address the flat provider binds, which is
    // why both come from `FRAMEWORKS`.
    FRAMEWORKS
        .iter()
        .filter_map(|f| f.symbol)
        .find_map(|symbol| symbol(name))
        // libclosure, which a shim forwards to by name - see
        // `crate::shims::block`. Nothing on a host off Darwin defines it, and it
        // has no framework of its own to be a row.
        .or_else(|| objc::block::symbol(name))
        // The host's GL, reached by `dlsym` rather than enumerated - see
        // `crate::opengl`, which publishes it through its own lazy provider.
        .or_else(|| crate::opengl::symbol(name))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the provider publishes, in order.
    fn published() -> exports::Recording {
        let mut out = exports::Recording::default();
        foundation::bootstrap();
        for framework in FRAMEWORKS {
            if let Some(provide) = framework.provide {
                provide(&mut out);
            }
            if let Some(data) = framework.data {
                for (name, addr) in data() {
                    out.data(&name, addr);
                }
            }
        }
        out
    }

    /// The library underneath is a map, so a name two frameworks define is one
    /// whose address depends on the order they are listed in - while `symbol`
    /// answers with the first that knows it, which may be the other one. Both
    /// `foundation` and `appkit` had been defining `_CGPointZero`.
    #[test]
    fn no_framework_defines_a_symbol_another_one_does() {
        if !builtin() {
            return;
        }
        let out = published();
        let mut seen: Vec<&str> = Vec::new();
        let mut twice: Vec<&str> = Vec::new();
        for name in out.symbols() {
            if seen.contains(&name) {
                twice.push(name);
            } else {
                seen.push(name);
            }
        }
        assert!(twice.is_empty(), "defined more than once: {twice:?}");
    }

    /// A framework with no bare spelling is one a `dlopen("AudioToolbox")`
    /// cannot reach, because `dyld::synthetic::find` never takes the leaf of a
    /// registered name. Deriving the paths is what makes that unrepresentable.
    #[test]
    fn every_framework_answers_a_dlopen_by_its_leaf() {
        for framework in FRAMEWORKS {
            let paths = paths_of(framework);
            for leaf in std::iter::once(framework.leaf).chain(framework.umbrellas.iter().copied()) {
                assert!(paths.iter().any(|p| p == leaf), "{leaf}");
                assert!(
                    paths.iter().any(
                        |p| p == &format!("/System/Library/Frameworks/{leaf}.framework/{leaf}")
                    ),
                    "{leaf}: the install name"
                );
            }
        }
    }

    /// The invariant the three lists could not state when they were three: the
    /// address a link binds and the address a `dlsym` answers are one.
    #[test]
    fn a_published_function_is_the_one_dlsym_answers() {
        if !builtin() {
            return;
        }
        for framework in FRAMEWORKS {
            let (Some(provide), Some(answer)) = (framework.provide, framework.symbol) else {
                continue;
            };
            let mut out = exports::Recording::default();
            provide(&mut out);
            for (mach_o, address) in &out.functions {
                let bare = mach_o.strip_prefix('_').expect("the Mach-O underscore");
                assert_eq!(answer(bare), Some(*address), "{}: {mach_o}", framework.leaf);
            }
        }
    }
}
