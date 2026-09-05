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

use foundation::geometry::NSRange;
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

/// What a text view holds, which is the half of it that is not configuration.
#[derive(Clone, Default)]
struct Contents {
    text: String,
    /// The insertion point or selection, in UTF-16 code units, as `NSRange`
    /// and `NSString` define it.
    selected: NSRange,
    /// The composition an input method has in progress, if any. Marked text is
    /// *provisional*: it is in `text` and replaced wholesale by the next
    /// `-setMarkedText:...`, which is what makes a dead key or a CJK candidate
    /// list behave.
    marked: Option<NSRange>,
}

fn contents() -> &'static Mutex<HashMap<usize, Contents>> {
    static CONTENTS: std::sync::OnceLock<Mutex<HashMap<usize, Contents>>> =
        std::sync::OnceLock::new();
    CONTENTS.get_or_init(Default::default)
}

/// Responders whose most recent key-down entered the inherited text system.
///
/// SDL reports resolved text separately from the physical key. A guest may
/// remain first responder while choosing not to call `[super keyDown:]`; that
/// choice must suppress the later text event or game keys leak into its hidden
/// editor.
fn interpreting() -> &'static Mutex<HashMap<usize, bool>> {
    static INTERPRETING: std::sync::OnceLock<Mutex<HashMap<usize, bool>>> =
        std::sync::OnceLock::new();
    INTERPRETING.get_or_init(Default::default)
}

pub(crate) fn begin_key_interpretation(receiver: Id) {
    if !receiver.is_null() {
        interpreting()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(receiver as usize, false);
    }
}

fn accept_key_interpretation(receiver: Id) {
    interpreting()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(receiver as usize, true);
}

pub(crate) fn accepts_host_text(receiver: Id) -> bool {
    !receiver.is_null()
        && interpreting()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(receiver as usize))
            .copied()
            .unwrap_or(false)
}

fn read(receiver: Id) -> Contents {
    contents()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or_default()
}

pub(crate) const DID_CHANGE: &str = "NSTextDidChangeNotification";
pub(crate) const DID_CHANGE_SELECTION: &str = "NSTextViewDidChangeSelectionNotification";

fn delegate_method(receiver: Id, name: &str) -> Option<(Id, Sel, &'static objc::class::Method)> {
    let delegate = get(receiver).delegate as Id;
    let sel = objc::sel::get(name);
    // SAFETY: null or a live object this runtime allocated.
    let method = unsafe { objc::object::class_of(delegate) }?.method(sel)?;
    Some((delegate, sel, method))
}

fn will_change_selection(receiver: Id, old: NSRange, proposed: NSRange) -> NSRange {
    let Some((delegate, sel, method)) = delegate_method(
        receiver,
        "textView:willChangeSelectionFromCharacterRange:toCharacterRange:",
    ) else {
        return proposed;
    };
    // SAFETY: the delegate method returns NSRange and takes the view and two ranges.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id, NSRange, NSRange) -> NSRange =
            std::mem::transmute(method.implementation());
        f(delegate, sel, receiver, old, proposed)
    }
}

fn did_change_selection(receiver: Id) {
    foundation::notification::post(DID_CHANGE_SELECTION, receiver, std::ptr::null_mut());
    let Some((delegate, sel, method)) = delegate_method(receiver, "textViewDidChangeSelection:")
    else {
        return;
    };
    let notification =
        foundation::notification::make(DID_CHANGE_SELECTION, receiver, std::ptr::null_mut());
    // SAFETY: `-textViewDidChangeSelection:` is `void (id, SEL, NSNotification *)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(method.implementation());
        f(delegate, sel, notification);
    }
}

fn notify_text_changed(receiver: Id) {
    foundation::notification::post(DID_CHANGE, receiver, std::ptr::null_mut());
    let Some((delegate, sel, method)) = delegate_method(receiver, "textDidChange:") else {
        return;
    };
    let notification = foundation::notification::make(DID_CHANGE, receiver, std::ptr::null_mut());
    // SAFETY: `-textDidChange:` is `void (id, SEL, NSNotification *)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(method.implementation());
        f(delegate, sel, notification);
    }
}

/// Edit a view's contents and tell its delegate, as AppKit does.
///
/// The notification is not decoration: a program that keeps its own copy of the
/// field learns about a keystroke this way and no other, and one that polls
/// `-string` is unaffected by it either way.
fn edit(receiver: Id, with: impl FnOnce(&mut Contents)) {
    let (old, proposed) = {
        let mut table = contents().lock().unwrap_or_else(|e| e.into_inner());
        let state = table.entry(receiver as usize).or_default();
        let old = state.selected;
        with(state);
        (old, state.selected)
    };
    if old != proposed {
        let accepted = will_change_selection(receiver, old, proposed);
        let mut table = contents().lock().unwrap_or_else(|e| e.into_inner());
        let state = table.entry(receiver as usize).or_default();
        if state.selected == proposed {
            state.selected = clamp_range(&state.text, accepted);
        }
        drop(table);
        did_change_selection(receiver);
    }
    notify_text_changed(receiver);
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `NSString` ranges count UTF-16 code units and Rust strings count bytes.
/// Clamp an offset inside a surrogate pair to the scalar's leading boundary.
fn byte_index(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, character) in text.char_indices() {
        let next = units + character.len_utf16();
        if offset < next {
            return byte;
        }
        units = next;
    }
    text.len()
}

/// Join an `NSRange` to Rust's byte ranges without slicing a scalar or running
/// beyond the string.
fn byte_range(text: &str, range: NSRange) -> std::ops::Range<usize> {
    let start = byte_index(text, range.location);
    let end = byte_index(text, range.location.saturating_add(range.length));
    start..end.max(start)
}

fn clamp_range(text: &str, range: NSRange) -> NSRange {
    let bytes = byte_range(text, range);
    NSRange {
        location: utf16_len(&text[..bytes.start]),
        length: utf16_len(&text[bytes]),
    }
}

/// Replace `range` with `insertion`, leaving the insertion point after it.
fn replace(state: &mut Contents, range: NSRange, insertion: &str) {
    let bytes = byte_range(&state.text, range);
    let location = utf16_len(&state.text[..bytes.start]);
    state.text.replace_range(bytes, insertion);
    state.selected = NSRange {
        location: location + utf16_len(insertion),
        length: 0,
    };
}

/// `NSNotFound`, which is what a range with nothing in it reports.
const NOT_FOUND: usize = isize::MAX as usize;

unsafe extern "C" fn string(receiver: Id, _sel: Sel) -> Id {
    objc::arc::autorelease(foundation::string::from_str(&read(receiver).text))
}

unsafe extern "C" fn set_string(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: null or an `NSString`.
    let text = String::from_utf8_lossy(&unsafe { foundation::string::bytes(value) }).into_owned();
    edit(receiver, |state| {
        state.selected = NSRange {
            location: utf16_len(&text),
            length: 0,
        };
        state.marked = None;
        state.text = text;
    });
}

unsafe extern "C" fn selected_range(receiver: Id, _sel: Sel) -> NSRange {
    read(receiver).selected
}

unsafe extern "C" fn set_selected_range(receiver: Id, _sel: Sel, range: NSRange) {
    change_selection(receiver, range);
}

fn change_selection(receiver: Id, range: NSRange) {
    let state = read(receiver);
    let proposed = clamp_range(&state.text, range);
    if state.selected == proposed {
        return;
    }
    let accepted = clamp_range(
        &state.text,
        will_change_selection(receiver, state.selected, proposed),
    );
    let mut table = contents().lock().unwrap_or_else(|e| e.into_inner());
    table.entry(receiver as usize).or_default().selected = accepted;
    drop(table);
    did_change_selection(receiver);
}

unsafe extern "C" fn marked_range(receiver: Id, _sel: Sel) -> NSRange {
    read(receiver).marked.unwrap_or(NSRange {
        location: NOT_FOUND,
        length: 0,
    })
}

unsafe extern "C" fn has_marked_text(receiver: Id, _sel: Sel) -> u8 {
    read(receiver).marked.is_some() as u8
}

unsafe extern "C" fn unmark_text(receiver: Id, _sel: Sel) {
    let mut table = contents().lock().unwrap_or_else(|e| e.into_inner());
    table.entry(receiver as usize).or_default().marked = None;
}

/// `-insertText:replacementRange:`, and `-insertText:` through the same body:
/// the older selector is the same operation with the replacement range left to
/// the current selection, which is what a null range means anyway.
unsafe extern "C" fn insert_text(receiver: Id, _sel: Sel, value: Id, replacement: NSRange) {
    // SAFETY: null or an `NSString`, which is what both selectors pass.
    let insertion =
        String::from_utf8_lossy(&unsafe { foundation::string::bytes(value) }).into_owned();
    edit(receiver, |state| {
        let range = match replacement.location {
            NOT_FOUND => state.marked.unwrap_or(state.selected),
            _ => replacement,
        };
        replace(state, range, &insertion);
        state.marked = None;
    });
}

/// The pre-`NSTextInputClient` spelling of `-insertText:replacementRange:`.
///
/// `InputMethodHandler` still uses this form when the active input source does
/// not need to replace a range explicitly. It replaces the current selection
/// (or outstanding marked text), exactly as the modern spelling does when its
/// range is `NSNotFound`.
unsafe extern "C" fn insert_text_legacy(receiver: Id, sel: Sel, value: Id) {
    // SAFETY: `insert_text` has the same receiver and string requirements, and
    // `NSNotFound` names the current replacement range.
    unsafe {
        insert_text(
            receiver,
            sel,
            value,
            NSRange {
                location: NOT_FOUND,
                length: 0,
            },
        )
    };
}

/// `-replaceCharactersInRange:withString:`, used by a text input handler to
/// commit an edit it has already measured against its buffer.
unsafe extern "C" fn replace_characters(receiver: Id, _sel: Sel, range: NSRange, value: Id) {
    // SAFETY: null or an `NSString`.
    let insertion =
        String::from_utf8_lossy(&unsafe { foundation::string::bytes(value) }).into_owned();
    edit(receiver, |state| {
        replace(state, range, &insertion);
        // Replacing a range is a committed edit, never a continuation of a
        // composition that happened to occupy the same bytes.
        state.marked = None;
    });
}

unsafe extern "C" fn set_marked_text(
    receiver: Id,
    _sel: Sel,
    value: Id,
    selected: NSRange,
    replacement: NSRange,
) {
    // SAFETY: null or an `NSString`.
    let insertion =
        String::from_utf8_lossy(&unsafe { foundation::string::bytes(value) }).into_owned();
    edit(receiver, |state| {
        let range = match replacement.location {
            NOT_FOUND => state.marked.unwrap_or(state.selected),
            _ => replacement,
        };
        replace(state, range, &insertion);
        let location = state
            .selected
            .location
            .saturating_sub(utf16_len(&insertion));
        let length = utf16_len(&insertion);
        state.marked = (length > 0).then_some(NSRange { location, length });
        let selected_location = selected.location.min(length);
        state.selected = NSRange {
            location: location + selected_location,
            length: selected.length.min(length - selected_location),
        };
    });
}

/// The older `NSTextInput` spelling, whose omitted replacement range means the
/// current selection or marked text.
unsafe extern "C" fn set_marked_text_legacy(receiver: Id, sel: Sel, value: Id, selected: NSRange) {
    // SAFETY: `set_marked_text` has the same receiver and string requirements.
    unsafe {
        set_marked_text(
            receiver,
            sel,
            value,
            selected,
            NSRange {
                location: NOT_FOUND,
                length: 0,
            },
        )
    };
}

/// `-deleteBackward:`, the command an input method sends instead of inserting
/// a backspace character.
unsafe extern "C" fn delete_backward(receiver: Id, _sel: Sel, _sender: Id) {
    edit(receiver, |state| {
        let range = backward_range(state);
        replace(state, range, "");
        state.marked = None;
    });
}

fn backward_range(state: &Contents) -> NSRange {
    if state.selected.length != 0 {
        return state.selected;
    }
    let caret = byte_index(&state.text, state.selected.location);
    let previous = state.text[..caret]
        .chars()
        .next_back()
        .map_or(0, char::len_utf16);
    NSRange {
        location: state.selected.location.saturating_sub(previous),
        length: previous,
    }
}

unsafe extern "C" fn delete_forward(receiver: Id, _sel: Sel, _sender: Id) {
    edit(receiver, |state| {
        let range = match state.selected.length {
            0 => {
                let caret = byte_index(&state.text, state.selected.location);
                NSRange {
                    location: state.selected.location,
                    length: state.text[caret..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf16),
                }
            }
            _ => state.selected,
        };
        replace(state, range, "");
        state.marked = None;
    });
}

/// `-shouldChangeTextInRange:replacementString:`. The in-memory text buffer
/// has no separate validation delegate, so every edit is permitted.
unsafe extern "C" fn should_change_text(
    _receiver: Id,
    _sel: Sel,
    _range: NSRange,
    _replacement: Id,
) -> u8 {
    1
}

/// `-keyDown:`, which is the whole reason the rest of this exists.
///
/// AppKit's `NSTextView` runs printable input through the input manager, which
/// is represented by SDL's separate text-input event. The key event handles
/// editing commands only; inserting its `-characters` here as well would type
/// every printable key twice.
unsafe extern "C" fn key_down(receiver: Id, _sel: Sel, event: Id) {
    accept_key_interpretation(receiver);
    let sel = objc::sel::get("characters");
    // SAFETY: `-characters` is `id (id, SEL)` on an event this crate made.
    let characters = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(event, sel)
    };
    // SAFETY: null or an `NSString`.
    let text =
        String::from_utf8_lossy(&unsafe { foundation::string::bytes(characters) }).into_owned();
    let Some(character) = text.chars().next() else {
        return;
    };
    match character {
        // Backspace, which is `-deleteBackward:` on a Mac.
        '\u{8}' | '\u{7f}' => unsafe {
            delete_backward(receiver, objc::sel::get("deleteBackward:"), receiver)
        },
        // A control character is a command - return, tab, an arrow - and none
        // of them types anything. Inserting one would put a control code in a
        // password.
        c if c.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&c) => {}
        _ => {}
    }
}

/// The three `NSTextInputClient` answers that are not about the buffer.
///
/// An input method asks where the text is on screen so it can put its candidate
/// window beside it, and which character a point is over. Nothing here draws
/// text, so there is no honest answer to either: the rectangle is empty and the
/// index is `NSNotFound`, which is what a client with no visible text reports.
unsafe extern "C" fn first_rect_for_character_range(
    receiver: Id,
    _sel: Sel,
    range: NSRange,
    actual: *mut NSRange,
) -> foundation::geometry::NSRect {
    if !actual.is_null() {
        // SAFETY: the caller supplied the optional out-parameter.
        unsafe { *actual = clamp_range(&read(receiver).text, range) };
    }
    foundation::geometry::NSRect::default()
}

unsafe extern "C" fn character_index_for_point(
    _receiver: Id,
    _sel: Sel,
    _point: foundation::geometry::NSPoint,
) -> usize {
    NOT_FOUND
}

unsafe extern "C" fn attributed_substring(
    receiver: Id,
    _sel: Sel,
    range: NSRange,
    actual: *mut NSRange,
) -> Id {
    let state = read(receiver);
    let range = clamp_range(&state.text, range);
    if !actual.is_null() {
        // SAFETY: the caller supplied the optional out-parameter.
        unsafe { *actual = range };
    }
    let bytes = byte_range(&state.text, range);
    foundation::attributed_string(&state.text[bytes])
}

unsafe extern "C" fn valid_attributes(_receiver: Id, _sel: Sel) -> Id {
    let array = corefoundation::collections::new_array(
        &[],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn select_all(receiver: Id, _sel: Sel, _sender: Id) {
    let length = utf16_len(&read(receiver).text);
    change_selection(
        receiver,
        NSRange {
            location: 0,
            length,
        },
    );
}

unsafe extern "C" fn move_left(receiver: Id, _sel: Sel, _sender: Id) {
    let state = read(receiver);
    let selected = match state.selected.length {
        0 => {
            let caret = byte_index(&state.text, state.selected.location);
            let units = state.text[..caret]
                .chars()
                .next_back()
                .map_or(0, char::len_utf16);
            NSRange {
                location: state.selected.location.saturating_sub(units),
                length: 0,
            }
        }
        _ => NSRange {
            location: state.selected.location,
            length: 0,
        },
    };
    change_selection(receiver, selected);
}

unsafe extern "C" fn move_right(receiver: Id, _sel: Sel, _sender: Id) {
    let state = read(receiver);
    let location = match state.selected.length {
        0 => {
            let caret = byte_index(&state.text, state.selected.location);
            state.selected.location
                + state.text[caret..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf16)
        }
        _ => state
            .selected
            .location
            .saturating_add(state.selected.length),
    };
    change_selection(
        receiver,
        NSRange {
            location: location.min(utf16_len(&state.text)),
            length: 0,
        },
    );
}

unsafe extern "C" fn do_command_by_selector(receiver: Id, _sel: Sel, command: Sel) {
    // The selectors an input manager produces for the commands that alter an
    // otherwise invisible field. Unknown commands are intentionally ignored.
    // SAFETY: each target has the uniform text-command shape `(id, SEL, id)`.
    unsafe {
        if command == objc::sel::get("deleteBackward:") {
            delete_backward(receiver, command, receiver);
        } else if command == objc::sel::get("deleteForward:") {
            delete_forward(receiver, command, receiver);
        } else if command == objc::sel::get("selectAll:") {
            select_all(receiver, command, receiver);
        } else if command == objc::sel::get("moveLeft:") {
            move_left(receiver, command, receiver);
        } else if command == objc::sel::get("moveRight:") {
            move_right(receiver, command, receiver);
        }
    }
}

/// Whether an object is an `NSTextInputClient` this bridge can feed.
pub(crate) fn is_input_client(receiver: Id) -> bool {
    if receiver.is_null() {
        return false;
    }
    let insert = objc::sel::get("insertText:replacementRange:");
    let legacy = objc::sel::get("insertText:");
    // SAFETY: a non-null live responder from a window this crate owns.
    unsafe { objc::object::class_of(receiver) }
        .is_some_and(|class| class.method(insert).is_some() || class.method(legacy).is_some())
}

/// Deliver one text-input commit to the focused input client.
pub(crate) fn commit(receiver: Id, text: &str) {
    if text.is_empty() {
        return;
    }
    let value = objc::arc::autorelease(foundation::string::from_str(text));
    let modern = objc::sel::get("insertText:replacementRange:");
    // SAFETY: a live first responder, when present.
    let method = unsafe { objc::object::class_of(receiver) }.and_then(|class| class.method(modern));
    unsafe {
        if let Some(method) = method {
            let f: unsafe extern "C" fn(Id, Sel, Id, NSRange) =
                std::mem::transmute(method.implementation());
            f(
                receiver,
                modern,
                value,
                NSRange {
                    location: NOT_FOUND,
                    length: 0,
                },
            );
        } else if let Some(method) = objc::object::class_of(receiver)
            .and_then(|class| class.method(objc::sel::get("insertText:")))
        {
            let sel = objc::sel::get("insertText:");
            let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(method.implementation());
            f(receiver, sel, value);
        }
    }
}

/// Deliver an in-progress host IME composition to the focused input client.
pub(crate) fn compose(receiver: Id, text: &str, start: i32, length: i32) {
    let value = objc::arc::autorelease(foundation::string::from_str(text));
    let character_range = NSRange {
        location: usize::try_from(start).unwrap_or(0),
        length: usize::try_from(length).unwrap_or(0),
    };
    let bytes = |characters: usize| {
        text.char_indices()
            .nth(characters)
            .map_or(text.len(), |(at, _)| at)
    };
    let from = bytes(character_range.location);
    let to = bytes(
        character_range
            .location
            .saturating_add(character_range.length),
    );
    let selected = NSRange {
        location: utf16_len(&text[..from]),
        length: utf16_len(&text[from..to.max(from)]),
    };
    let modern = objc::sel::get("setMarkedText:selectedRange:replacementRange:");
    let legacy = objc::sel::get("setMarkedText:selectedRange:");
    // SAFETY: the selector is either the modern five-argument shape or its
    // older spelling without a replacement range.
    unsafe {
        let class = objc::object::class_of(receiver);
        if let Some(method) = class.and_then(|class| class.method(modern)) {
            let f: unsafe extern "C" fn(Id, Sel, Id, NSRange, NSRange) =
                std::mem::transmute(method.implementation());
            f(
                receiver,
                modern,
                value,
                selected,
                NSRange {
                    location: NOT_FOUND,
                    length: 0,
                },
            );
        } else if let Some(method) = class.and_then(|class| class.method(legacy)) {
            let f: unsafe extern "C" fn(Id, Sel, Id, NSRange) =
                std::mem::transmute(method.implementation());
            f(receiver, legacy, value, selected);
        }
    }
}

/// Whether an implementation is this crate's own text-view key handling.
///
/// The event router asks, because a text view is where a keystroke goes only
/// when nothing else wants it: see [`crate::event::chain`].
pub(crate) fn is_default_key_handling(implementation: *const ()) -> bool {
    [key_down as *const (), key_up as *const ()].contains(&implementation)
}

/// A release types nothing, and a text view that did not answer it would abort
/// the program the first time a key came back up.
unsafe extern "C" fn key_up(_receiver: Id, _sel: Sel, _event: Id) {}

/// A text view is what the keyboard talks to, so it takes first responder.
unsafe extern "C" fn accepts_first_responder(_receiver: Id, _sel: Sel) -> u8 {
    1
}

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
        "string" => string, c"@16@0:8";
        "setString:" => set_string, c"v24@0:8@16";
        "selectedRange" => selected_range, c"{_NSRange=QQ}16@0:8";
        "setSelectedRange:" => set_selected_range, c"v32@0:8{_NSRange=QQ}16";
        "markedRange" => marked_range, c"{_NSRange=QQ}16@0:8";
        "hasMarkedText" => has_marked_text, c"c16@0:8";
        "unmarkText" => unmark_text, c"v16@0:8";
        "insertText:" => insert_text_legacy, c"v24@0:8@16";
        "insertText:replacementRange:" => insert_text, c"v40@0:8@16{_NSRange=QQ}24";
        "replaceCharactersInRange:withString:" => replace_characters,
            c"v40@0:8{_NSRange=QQ}16@32";
        "setMarkedText:selectedRange:" => set_marked_text_legacy, c"v40@0:8@16{_NSRange=QQ}24";
        "setMarkedText:selectedRange:replacementRange:" => set_marked_text,
            c"v56@0:8@16{_NSRange=QQ}24{_NSRange=QQ}40";
        "deleteBackward:" => delete_backward, c"v24@0:8@16";
        "deleteForward:" => delete_forward, c"v24@0:8@16";
        "selectAll:" => select_all, c"v24@0:8@16";
        "moveLeft:" => move_left, c"v24@0:8@16";
        "moveRight:" => move_right, c"v24@0:8@16";
        "shouldChangeTextInRange:replacementString:" => should_change_text,
            c"c40@0:8{_NSRange=QQ}16@32";
        "keyDown:" => key_down, c"v24@0:8@16";
        "keyUp:" => key_up, c"v24@0:8@16";
        "firstRectForCharacterRange:actualRange:" => first_rect_for_character_range,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}40@0:8{_NSRange=QQ}16^{_NSRange=QQ}32";
        "characterIndexForPoint:" => character_index_for_point, c"Q32@0:8{CGPoint=dd}16";
        "attributedSubstringForProposedRange:actualRange:" => attributed_substring,
            c"@40@0:8{_NSRange=QQ}16^{_NSRange=QQ}32";
        "validAttributesForMarkedText" => valid_attributes, c"@16@0:8";
        "doCommandBySelector:" => do_command_by_selector, c"v24@0:8:16";
        "acceptsFirstResponder" => accepts_first_responder, c"c16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    static SELECTION_EVENTS: Mutex<Vec<(&'static str, String, NSRange)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record_will_change(
        receiver: Id,
        _sel: Sel,
        _view: Id,
        _old: NSRange,
        proposed: NSRange,
    ) -> NSRange {
        let state = read(receiver);
        SELECTION_EVENTS
            .lock()
            .unwrap()
            .push(("will", state.text, state.selected));
        proposed
    }

    unsafe extern "C" fn record_did_change(receiver: Id, _sel: Sel, _notification: Id) {
        let state = read(receiver);
        SELECTION_EVENTS
            .lock()
            .unwrap()
            .push(("did", state.text, state.selected));
    }

    fn delegate_text_view() -> Id {
        crate::bootstrap();
        let text = objc::class::lookup(c"NSTextView").expect("NSTextView");
        let class = objc::class::lookup(c"Rosetta3TextSelectionDelegate").unwrap_or_else(|| {
            let class = objc::class::allocate_pair(Some(text), c"Rosetta3TextSelectionDelegate", 0)
                .expect("a fresh test class");
            assert!(class.add_method(
                objc::sel::get("textView:willChangeSelectionFromCharacterRange:toCharacterRange:"),
                record_will_change as objc::class::Imp,
                c"{_NSRange=QQ}56@0:8@16{_NSRange=QQ}24{_NSRange=QQ}40",
            ));
            assert!(class.add_method(
                objc::sel::get("textViewDidChangeSelection:"),
                record_did_change as objc::class::Imp,
                c"v24@0:8@16",
            ));
            objc::class::register_pair(class);
            class
        });
        let view = objc::object::create_instance(class, 0);
        // SAFETY: the live view is its own delegate.
        unsafe { set_delegate(view, objc::sel::get("setDelegate:"), view) };
        view
    }

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

    #[test]
    fn a_text_input_handler_can_replace_commit_and_delete_text() {
        let view = text_view();
        let initial = foundation::string::from_str("café");
        let replacement = foundation::string::from_str("ee");
        // SAFETY: the strings are this runtime's and the view is a live
        // `NSTextView`, which inherits these `NSText` implementations.
        unsafe {
            set_string(view, objc::sel::get("setString:"), initial);
            replace_characters(
                view,
                objc::sel::get("replaceCharactersInRange:withString:"),
                NSRange {
                    location: 1,
                    length: 2,
                },
                replacement,
            );
            assert_eq!(
                foundation::string::bytes(string(view, objc::sel::get("string"))),
                b"cee\xc3\xa9"
            );
            delete_backward(
                view,
                objc::sel::get("deleteBackward:"),
                std::ptr::null_mut(),
            );
            assert_eq!(
                foundation::string::bytes(string(view, objc::sel::get("string"))),
                b"ce\xc3\xa9"
            );
        }
    }

    #[test]
    fn committed_host_text_reaches_the_focused_client_immediately() {
        let view = text_view();
        commit(view, "typed");
        // SAFETY: a live text view and the string it owns.
        assert_eq!(
            unsafe { foundation::string::bytes(string(view, objc::sel::get("string"))) },
            b"typed"
        );
        assert_eq!(read(view).selected.location, 5);
    }

    #[test]
    fn an_edit_publishes_text_and_selection_before_delegate_callbacks() {
        SELECTION_EVENTS.lock().unwrap().clear();
        let view = delegate_text_view();
        commit(view, "\u{1f600}");
        assert_eq!(
            *SELECTION_EVENTS.lock().unwrap(),
            vec![
                (
                    "will",
                    "\u{1f600}".to_owned(),
                    NSRange {
                        location: 2,
                        length: 0
                    }
                ),
                (
                    "did",
                    "\u{1f600}".to_owned(),
                    NSRange {
                        location: 2,
                        length: 0
                    }
                ),
            ]
        );
    }

    #[test]
    fn attributed_substring_returns_the_utf16_intersection() {
        let view = text_view();
        // SAFETY: live text objects and correctly typed method calls.
        unsafe {
            set_string(
                view,
                objc::sel::get("setString:"),
                foundation::string::from_str("a\u{1f600}\u{e9}b"),
            );
            let mut actual = NSRange::default();
            let attributed = attributed_substring(
                view,
                objc::sel::get("attributedSubstringForProposedRange:actualRange:"),
                NSRange {
                    location: 1,
                    length: 3,
                },
                &mut actual,
            );
            assert!(!attributed.is_null());
            assert_eq!(
                actual,
                NSRange {
                    location: 1,
                    length: 3
                }
            );
            let method = objc::object::class_of(attributed)
                .and_then(|class| class.method(objc::sel::get("string")))
                .expect("NSAttributedString answers -string");
            let get_string: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(method.implementation());
            assert_eq!(
                foundation::string::bytes(get_string(attributed, objc::sel::get("string"))),
                "\u{1f600}\u{e9}".as_bytes()
            );
        }
    }

    #[test]
    fn ns_not_found_is_ns_integer_max() {
        assert_eq!(NOT_FOUND, 0x7fff_ffff_ffff_ffff);
    }

    #[test]
    fn host_text_requires_the_responder_to_enter_super_key_down() {
        let view = text_view();
        begin_key_interpretation(view);
        assert!(!accepts_host_text(view));
        accept_key_interpretation(view);
        assert!(accepts_host_text(view));
        begin_key_interpretation(view);
        assert!(!accepts_host_text(view));
    }

    #[test]
    fn composition_is_replaced_by_the_commit() {
        let view = text_view();
        compose(view, "caf\u{e9}", 3, 1);
        assert_eq!(
            read(view).marked,
            Some(NSRange {
                location: 0,
                length: 4,
            })
        );
        assert_eq!(
            read(view).selected,
            NSRange {
                location: 3,
                length: 1
            }
        );
        commit(view, "coffee");
        assert_eq!(read(view).text, "coffee");
        assert_eq!(read(view).marked, None);
    }

    #[test]
    fn ranges_and_deletion_count_utf16_code_units() {
        let view = text_view();
        // SAFETY: a live view and string.
        unsafe {
            set_string(
                view,
                objc::sel::get("setString:"),
                foundation::string::from_str("a\u{1f600}b"),
            );
            assert_eq!(read(view).selected.location, 4);
            delete_backward(view, objc::sel::get("deleteBackward:"), view);
            delete_backward(view, objc::sel::get("deleteBackward:"), view);
        }
        assert_eq!(read(view).text, "a");
        assert_eq!(read(view).selected.location, 1);
    }
}
