//! The process-wide `NSWorkspace` facade.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

static SHARED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
static CURRENT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

pub const WILL_SLEEP: &str = "NSWorkspaceWillSleepNotification";
pub const DID_WAKE: &str = "NSWorkspaceDidWakeNotification";
pub const SCREENS_DID_SLEEP: &str = "NSWorkspaceScreensDidSleepNotification";
pub const SCREENS_DID_WAKE: &str = "NSWorkspaceScreensDidWakeNotification";

/// `+[NSWorkspace sharedWorkspace]`.
///
/// The workspace is a process-lifetime singleton. Its individual services are
/// installed as they acquire host equivalents; callers must still be able to
/// obtain the object before asking for one of them.
unsafe extern "C" fn shared_workspace(class: Id, _sel: Sel) -> Id {
    *SHARED.get_or_init(|| {
        // SAFETY: the receiver is the `NSWorkspace` class this crate declared.
        let workspace = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        // A singleton is not transferred to the caller and must survive a
        // guest incorrectly balancing it with `release`.
        objc::arc::retain(workspace);
        workspace as usize
    }) as Id
}

/// `-URLForApplicationToOpenURL:`.
///
/// Host desktop applications are not Mach-O bundles loaded by this runtime, so
/// there is no truthful application URL to return. Cocoa callers use nil to
/// mean that Launch Services has no registered handler and are required to
/// handle that result.
unsafe extern "C" fn application_url_for_url(_receiver: Id, _sel: Sel, _url: Id) -> Id {
    std::ptr::null_mut()
}

/// `-[NSWorkspace notificationCenter]`.
///
/// Workspace notifications share Foundation's process-wide center in this
/// runtime. It provides the same observer semantics while host sleep and wake
/// events remain unavailable.
unsafe extern "C" fn notification_center(_receiver: Id, _sel: Sel) -> Id {
    foundation::notification::default_center_object()
}

/// `-[NSWorkspace frontmostApplication]`: this process.
///
/// The same answer as `-[NSApplication isActive]`, which never resigns because
/// no other application here can be in front. A program comparing this against
/// its own process to decide whether it has focus must not find a stranger.
unsafe extern "C" fn frontmost_application(_receiver: Id, _sel: Sel) -> Id {
    current()
}

/// The `NSRunningApplication` for this process, a process-lifetime singleton.
fn current() -> Id {
    *CURRENT.get_or_init(|| {
        let class =
            objc::class::lookup(c"NSRunningApplication").expect("declared before its methods");
        let object = objc::object::create_instance(class, 0);
        objc::arc::retain(object);
        object as usize
    }) as Id
}

/// `+[NSRunningApplication currentApplication]`.
unsafe extern "C" fn current_application(_class: Id, _sel: Sel) -> Id {
    current()
}

fn autoreleased(text: &str) -> Id {
    let string = foundation::string::from_str(text);
    // SAFETY: a fresh CFString, toll-free bridged to NSString.
    unsafe { corefoundation::api::CFAutorelease(string as _) as Id }
}

/// `-processIdentifier`. Only this process is ever represented.
unsafe extern "C" fn process_identifier(_receiver: Id, _sel: Sel) -> i32 {
    std::process::id() as i32
}

/// `-bundleIdentifier`: nil for an executable outside a bundle, as on macOS.
unsafe extern "C" fn bundle_identifier(_receiver: Id, _sel: Sel) -> Id {
    match foundation::bundle::main().and_then(|b| b.identifier()) {
        Some(identifier) => autoreleased(identifier),
        None => std::ptr::null_mut(),
    }
}

/// `-localizedName`: the bundle's display name, else the executable's name.
unsafe extern "C" fn localized_name(_receiver: Id, _sel: Sel) -> Id {
    if let Some(name) = foundation::bundle::main().and_then(|b| b.display_name()) {
        return autoreleased(name);
    }
    let name = std::env::args_os().next().and_then(|arg| {
        std::path::Path::new(&arg)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    });
    match name {
        Some(name) => autoreleased(&name),
        None => std::ptr::null_mut(),
    }
}

/// `-isActive`: always, for the reason given at [`frontmost_application`].
unsafe extern "C" fn running_is_active(_receiver: Id, _sel: Sel) -> u8 {
    1
}

/// `-activateWithOptions:`: already active, which Apple reports as success.
unsafe extern "C" fn activate_with_options(_receiver: Id, _sel: Sel, _options: usize) -> u8 {
    1
}

/// `+[NSRunningApplication runningApplicationsWithBundleIdentifier:]`.
///
/// Host processes are not registered applications in this Darwin workspace.
unsafe extern "C" fn running_applications(_class: Id, _sel: Sel, _identifier: Id) -> Id {
    let array = corefoundation::collections::new_array(
        &[],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh CFArray, toll-free bridged to NSArray.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSWorkspace").expect("declared before its methods");
    class_methods!(class, {
        "sharedWorkspace" => shared_workspace, c"@16@0:8";
    });
    methods!(class, {
        "URLForApplicationToOpenURL:" => application_url_for_url, c"@24@0:8@16";
        "notificationCenter" => notification_center, c"@16@0:8";
        "frontmostApplication" => frontmost_application, c"@16@0:8";
    });

    let running =
        objc::class::lookup(c"NSRunningApplication").expect("declared before its methods");
    class_methods!(running, {
        "runningApplicationsWithBundleIdentifier:" => running_applications, c"@24@0:8@16";
        "currentApplication" => current_application, c"@16@0:8";
    });
    methods!(running, {
        "processIdentifier" => process_identifier, c"i16@0:8";
        "bundleIdentifier" => bundle_identifier, c"@16@0:8";
        "localizedName" => localized_name, c"@16@0:8";
        "isActive" => running_is_active, c"c16@0:8";
        "activateWithOptions:" => activate_with_options, c"c24@0:8Q16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_workspace_is_one_process_lifetime_object() {
        crate::bootstrap();
        let class =
            objc::class::ptr(objc::class::lookup(c"NSWorkspace").expect("NSWorkspace")) as Id;
        // SAFETY: `+sharedWorkspace` is `id (Class, SEL)`.
        let first = unsafe { shared_workspace(class, objc::sel::get("sharedWorkspace")) };
        let second = unsafe { shared_workspace(class, objc::sel::get("sharedWorkspace")) };
        assert!(!first.is_null());
        assert_eq!(first, second);
        // SAFETY: `-notificationCenter` takes no explicit arguments.
        let center = unsafe { notification_center(first, objc::sel::get("notificationCenter")) };
        assert!(!center.is_null());
        assert_eq!(center, foundation::notification::default_center_object());
    }

    #[test]
    fn frontmost_application_is_this_process() {
        crate::bootstrap();
        // SAFETY: both take no explicit arguments.
        let frontmost = unsafe {
            frontmost_application(std::ptr::null_mut(), objc::sel::get("frontmostApplication"))
        };
        let current = unsafe {
            current_application(std::ptr::null_mut(), objc::sel::get("currentApplication"))
        };
        assert!(!frontmost.is_null());
        assert_eq!(frontmost, current);
        // SAFETY: `-processIdentifier` takes no explicit arguments.
        let pid = unsafe { process_identifier(frontmost, objc::sel::get("processIdentifier")) };
        assert_eq!(pid, std::process::id() as i32);
    }
}
