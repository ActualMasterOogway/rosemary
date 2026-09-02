//! `NSText` and `NSTextView`, as far as being *configured* goes.
//!
//! There is no text system here: nothing lays out glyphs, nothing draws a
//! caret, and no key event reaches an editor. What a program still does before
//! any of that could matter is configure one - a Cocoa program's input-method
//! handler is an `NSTextView` subclass, and it sets its flags in its own
//! `-init` long before a window exists.
//!
//! So each flag is *stored and read back* rather than ignored. That is the
//! difference between this and the ignored setters on `NSView`: a view that
//! drops `-setNeedsDisplay:` has answered the question, and a text view that
//! drops `-setFieldEditor:` and then answers `-isFieldEditor` with NO has told
//! its caller the opposite of what it was asked.
//!
//! The flags live in a table beside the objects rather than in an ivar, because
//! the class a guest actually instantiates is its **own** subclass and its ivar
//! offsets were fixed against Apple's `NSTextView`, not this one.

use std::collections::HashMap;
use std::sync::Mutex;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// What a text view has been configured with. One entry per object that was
/// ever configured; nothing removes an entry, because `-dealloc` on a guest's
/// own subclass is not this crate's call to see and a handful of text views is
/// what a program has.
#[derive(Clone, Copy, Default)]
struct Flags {
    field_editor: bool,
    editable: bool,
    selectable: bool,
    /// Unretained, as every AppKit delegate is: the delegate owns the view.
    delegate: usize,
}

fn flags() -> &'static Mutex<HashMap<usize, Flags>> {
    static FLAGS: std::sync::OnceLock<Mutex<HashMap<usize, Flags>>> = std::sync::OnceLock::new();
    FLAGS.get_or_init(Default::default)
}

fn get(receiver: Id) -> Flags {
    flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .copied()
        .unwrap_or_default()
}

fn set(receiver: Id, with: impl FnOnce(&mut Flags)) {
    let mut table = flags().lock().unwrap_or_else(|e| e.into_inner());
    with(table.entry(receiver as usize).or_default());
}

unsafe extern "C" fn set_field_editor(receiver: Id, _sel: Sel, on: u8) {
    set(receiver, |f| f.field_editor = on != 0);
}

unsafe extern "C" fn is_field_editor(receiver: Id, _sel: Sel) -> u8 {
    get(receiver).field_editor as u8
}

unsafe extern "C" fn set_editable(receiver: Id, _sel: Sel, on: u8) {
    // AppKit's `-setEditable:YES` implies selectable, which is the one
    // dependency between these flags a caller relies on.
    set(receiver, |f| {
        f.editable = on != 0;
        f.selectable |= on != 0;
    });
}

unsafe extern "C" fn is_editable(receiver: Id, _sel: Sel) -> u8 {
    get(receiver).editable as u8
}

unsafe extern "C" fn set_selectable(receiver: Id, _sel: Sel, on: u8) {
    set(receiver, |f| {
        f.selectable = on != 0;
        f.editable &= on != 0;
    });
}

unsafe extern "C" fn is_selectable(receiver: Id, _sel: Sel) -> u8 {
    get(receiver).selectable as u8
}

unsafe extern "C" fn set_delegate(receiver: Id, _sel: Sel, delegate: Id) {
    set(receiver, |f| f.delegate = delegate as usize);
}

unsafe extern "C" fn delegate(receiver: Id, _sel: Sel) -> Id {
    get(receiver).delegate as Id
}

/// The appearance flags, which are only ever set. Nothing draws, so there is
/// nothing for a caller to read back that this crate could be wrong about.
unsafe extern "C" fn ignored_flag(_receiver: Id, _sel: Sel, _on: u8) {}

pub(crate) fn install() {
    let text = objc::class::lookup(c"NSText").expect("declared before its methods");
    methods!(text, {
        "setFieldEditor:" => set_field_editor, c"v20@0:8c16";
        "isFieldEditor" => is_field_editor, c"c16@0:8";
        "setEditable:" => set_editable, c"v20@0:8c16";
        "isEditable" => is_editable, c"c16@0:8";
        "setSelectable:" => set_selectable, c"v20@0:8c16";
        "isSelectable" => is_selectable, c"c16@0:8";
        "setDelegate:" => set_delegate, c"v24@0:8@16";
        "delegate" => delegate, c"@16@0:8";
        "setRichText:" => ignored_flag, c"v20@0:8c16";
        "setImportsGraphics:" => ignored_flag, c"v20@0:8c16";
        "setUsesFontPanel:" => ignored_flag, c"v20@0:8c16";
        "setDrawsBackground:" => ignored_flag, c"v20@0:8c16";
        "setHorizontallyResizable:" => ignored_flag, c"v20@0:8c16";
        "setVerticallyResizable:" => ignored_flag, c"v20@0:8c16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_view() -> Id {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSTextView").expect("NSTextView");
        objc::object::create_instance(class, 0)
    }

    /// The whole point of storing rather than ignoring: what was set is what is
    /// read back, and two views do not share an answer.
    #[test]
    fn a_flag_reads_back_per_object() {
        let (a, b) = (text_view(), text_view());
        // SAFETY: `-setFieldEditor:` is `void (id, SEL, BOOL)` and its getter
        // `BOOL (id, SEL)`.
        unsafe {
            let sel = objc::sel::get("setFieldEditor:");
            set_field_editor(a, sel, 1);
            assert_eq!(is_field_editor(a, sel), 1);
            assert_eq!(is_field_editor(b, sel), 0);
        }
    }

    /// Editable implies selectable, and turning selection off turns editing off
    /// with it - which is AppKit's rule and not a simplification.
    #[test]
    fn editable_and_selectable_move_together() {
        let view = text_view();
        // SAFETY: as above.
        unsafe {
            let sel = objc::sel::get("setEditable:");
            set_editable(view, sel, 1);
            assert_eq!(is_selectable(view, sel), 1);
            set_selectable(view, sel, 0);
            assert_eq!(is_editable(view, sel), 0);
        }
    }
}
