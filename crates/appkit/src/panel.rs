//! `NSOpenPanel` and `NSSavePanel`, which are answered "cancelled".
//!
//! A file panel is AppKit's own browser: a window full of AppKit's controls,
//! with a filesystem view and a sidebar. Nothing in this tree draws one - the
//! same reason `crate::alert` gives - and there is no host dialog to stand in
//! for it either, because the paths a guest sees are not this host's view of
//! the filesystem.
//!
//! So the panel is reported on stderr, where whoever is running the program can
//! see what it wanted to be handed, and `-runModal` answers
//! `NSModalResponseCancel`. Cancelling is the one answer that is true: nobody
//! chose a file. A program reads that as "the person changed their mind", which
//! is a path it already has, rather than being handed a file that was never
//! picked.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// `NSModalResponseCancel`, which is also `NSFileHandlingPanelCancelButton`.
const CANCEL: i64 = 0;

/// `+openPanel` / `+savePanel`: an autoreleased panel of the receiving class.
///
/// The receiver is the class the message was sent to, so one function serves
/// both and an `NSOpenPanel` comes back for `+openPanel`.
unsafe extern "C" fn panel(class: Id, _sel: Sel) -> Id {
    // SAFETY: `class` is the class object the message was sent to.
    let Some(class) = (unsafe { objc::class::from_ptr(class as *const objc::class::Class) }) else {
        return std::ptr::null_mut();
    };
    objc::arc::autorelease(objc::object::create_instance(class, 0))
}

/// `-runModal`, and the `-beginSheetModalForWindow:completionHandler:`-free
/// half of the family: nothing was chosen.
unsafe extern "C" fn run_modal(receiver: Id, _sel: Sel) -> i64 {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        eprintln!(
            "rosetta3: appkit: the guest asked for a file panel, and nothing here draws one - \
             so it is answered as cancelled"
        );
    });
    let _ = receiver;
    CANCEL
}

/// `-URLs`, `-URL`, `-filenames` and `-filename`: nothing was chosen, so there
/// is nothing to hand back.
///
/// An empty array rather than nil for the plural forms, because a caller that
/// checks the response and then counts is entitled to count something.
unsafe extern "C" fn urls(_receiver: Id, _sel: Sel) -> Id {
    let empty = corefoundation::collections::new_array(
        &[],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array this call made.
    unsafe { corefoundation::api::CFAutorelease(empty) as Id }
}

unsafe extern "C" fn nothing(_receiver: Id, _sel: Sel) -> Id {
    std::ptr::null_mut()
}

/// The setters a program calls before it runs the panel. Each records nothing:
/// there is no panel for them to configure.
unsafe extern "C" fn ignored_object(_receiver: Id, _sel: Sel, _value: Id) {}

unsafe extern "C" fn ignored_flag(_receiver: Id, _sel: Sel, _value: u8) {}

pub(crate) fn install() {
    for name in [c"NSOpenPanel", c"NSSavePanel"] {
        let class = objc::class::lookup(name).expect("declared in `classes`");
        class_methods!(class, {
            "openPanel" => panel, c"@16@0:8";
            "savePanel" => panel, c"@16@0:8";
        });
        methods!(class, {
            "runModal" => run_modal, c"q16@0:8";
            "URLs" => urls, c"@16@0:8";
            "filenames" => urls, c"@16@0:8";
            "URL" => nothing, c"@16@0:8";
            "filename" => nothing, c"@16@0:8";
            "directoryURL" => nothing, c"@16@0:8";
            "setDirectoryURL:" => ignored_object, c"v24@0:8@16";
            "setAllowedFileTypes:" => ignored_object, c"v24@0:8@16";
            "setNameFieldStringValue:" => ignored_object, c"v24@0:8@16";
            "setTitle:" => ignored_object, c"v24@0:8@16";
            "setMessage:" => ignored_object, c"v24@0:8@16";
            "setPrompt:" => ignored_object, c"v24@0:8@16";
            "setCanChooseFiles:" => ignored_flag, c"v20@0:8c16";
            "setCanChooseDirectories:" => ignored_flag, c"v20@0:8c16";
            "setAllowsMultipleSelection:" => ignored_flag, c"v20@0:8c16";
            "setCanCreateDirectories:" => ignored_flag, c"v20@0:8c16";
            "setResolvesAliases:" => ignored_flag, c"v20@0:8c16";
            "setShowsHiddenFiles:" => ignored_flag, c"v20@0:8c16";
            "setTreatsFilePackagesAsDirectories:" => ignored_flag, c"v20@0:8c16";
        });
    }
}
