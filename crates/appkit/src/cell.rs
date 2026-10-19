//! `-[NSControl cell]` and the `NSCell` family.
//!
//! Every control here keeps its own state, where Apple's keeps it in a cell and
//! forwards to it. A cell is therefore the reverse: a small object that names
//! its control and forwards the shared accessors to it, so a program that
//! reaches through `-cell` reads and writes the same state the control shows.
//! A button cell a program allocates itself is given a button of its own to
//! keep that state, which it owns rather than the other way round.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// `isa`, then the control. Not retained unless `owns_control` is set: a
/// control owns its cell, and the cell is dropped in [`forget`] when the
/// control goes.
#[repr(C)]
struct Cell {
    isa: *const std::ffi::c_void,
    control: Id,
    /// Nonzero for a cell a program made, which made its control to back it.
    owns_control: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Cell>();

/// The cell class for each control class, most specific first.
const KINDS: &[(&std::ffi::CStr, &std::ffi::CStr)] = &[
    (c"NSPopUpButton", c"NSPopUpButtonCell"),
    (c"NSButton", c"NSButtonCell"),
    (c"NSSearchField", c"NSSearchFieldCell"),
    (c"NSTextField", c"NSTextFieldCell"),
    (c"NSSegmentedControl", c"NSSegmentedCell"),
    (c"NSImageView", c"NSImageCell"),
    (c"NSControl", c"NSActionCell"),
];

/// What a cell answers for its control. Only methods returning in registers:
/// the forwarder cannot move a struct-return buffer's receiver.
const FORWARDED: &[&str] = &[
    "tag",
    "setTag:",
    "stringValue",
    "setStringValue:",
    "objectValue",
    "setObjectValue:",
    "intValue",
    "setIntValue:",
    "integerValue",
    "setIntegerValue:",
    "doubleValue",
    "setDoubleValue:",
    "floatValue",
    "setFloatValue:",
    "attributedStringValue",
    "setAttributedStringValue:",
    "title",
    "setTitle:",
    "attributedTitle",
    "setAttributedTitle:",
    "font",
    "setFont:",
    "alignment",
    "setAlignment:",
    "textColor",
    "setTextColor:",
    "backgroundColor",
    "setBackgroundColor:",
    "drawsBackground",
    "setDrawsBackground:",
    "isBordered",
    "setBordered:",
    "isEnabled",
    "setEnabled:",
    "isEditable",
    "setEditable:",
    "isSelectable",
    "setSelectable:",
    "lineBreakMode",
    "setLineBreakMode:",
    "state",
    "setState:",
    "image",
    "setImage:",
    "alternateImage",
    "setAlternateImage:",
    "keyEquivalent",
    "setKeyEquivalent:",
    "target",
    "setTarget:",
    "action",
    "setAction:",
    "formatter",
    "setFormatter:",
    "isHighlighted",
    "setHighlighted:",
    "bezelStyle",
    "setBezelStyle:",
    "segmentCount",
    "setSegmentCount:",
    "selectedSegment",
    "setSelectedSegment:",
    "selectSegmentWithTag:",
    "tagForSegment:",
    "setTag:forSegment:",
    "labelForSegment:",
    "setLabel:forSegment:",
    "isSelectedForSegment:",
    "setSelected:forSegment:",
    "isEnabledForSegment:",
    "setEnabled:forSegment:",
    "trackingMode",
    "setTrackingMode:",
    "buttonType",
    "setButtonType:",
    "highlightsBy",
    "setHighlightsBy:",
    "showsStateBy",
    "setShowsStateBy:",
    "imagePosition",
    "setImagePosition:",
    "imageScaling",
    "setImageScaling:",
    "allowsMixedState",
    "setAllowsMixedState:",
    "nextState",
    "setNextState",
    "alternateTitle",
    "setAlternateTitle:",
    "isTransparent",
    "setTransparent:",
    "keyEquivalentModifierMask",
    "setKeyEquivalentModifierMask:",
    "performClick:",
];

fn cells() -> &'static Mutex<HashMap<usize, usize>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

fn kind_of(control: Id) -> &'static objc::class::Class {
    // SAFETY: a control this crate's classes describe, or a program's subclass.
    let chain = unsafe { objc::object::class_of(control) };
    let class_name = KINDS
        .iter()
        .find(|(control_class, _)| {
            let Some(wanted) = objc::class::lookup(control_class) else {
                return false;
            };
            chain.is_some_and(|c| c.chain().any(|k| std::ptr::eq(k, wanted)))
        })
        .map_or(c"NSActionCell", |(_, cell)| *cell);
    objc::class::lookup(class_name).expect("declared in crate::classes")
}

/// The control's cell, made on first use and owned by the control.
pub(crate) fn cell_of(control: Id) -> Id {
    if control.is_null() {
        return std::ptr::null_mut();
    }
    let mut table = cells().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&cell) = table.get(&(control as usize)) {
        return cell as Id;
    }
    let cell = objc::object::create_instance(kind_of(control), 0);
    // SAFETY: a fresh instance of a class laid out as `Cell`.
    unsafe { (*(cell as *mut Cell)).control = control };
    table.insert(control as usize, cell as usize);
    cell
}

/// Drop the cell of a control that is going away.
pub(crate) fn forget(control: Id) {
    let cell = cells()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(control as usize));
    if let Some(cell) = cell {
        // SAFETY: the cell this module made; nothing else points at its control.
        unsafe { (*(cell as *mut Cell)).control = std::ptr::null_mut() };
        objc::arc::release(cell as Id);
    }
}

/// Give a cell a program allocated a control of its kind to keep its state,
/// so that what it is sent is kept, and so that it can be the prototype a
/// matrix copies its cells from.
fn back_with_control(cell: Id) -> Id {
    if cell.is_null() {
        return cell;
    }
    // SAFETY: an instance of a class laid out as `Cell`.
    if unsafe { !(*(cell as *const Cell)).control.is_null() } {
        return cell;
    }
    // SAFETY: an object this runtime allocated.
    let chain = unsafe { objc::object::class_of(cell) };
    let Some(control_class) = KINDS
        .iter()
        .find(|(_, cell_class)| {
            objc::class::lookup(cell_class).is_some_and(|wanted| {
                chain.is_some_and(|c| c.chain().any(|k| std::ptr::eq(k, wanted)))
            })
        })
        .and_then(|(control_class, _)| objc::class::lookup(control_class))
    else {
        return cell;
    };
    let control = objc::object::create_instance(control_class, 0);
    // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)` on every view.
    let control = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, foundation::geometry::NSRect) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(
            control,
            objc::sel::get("initWithFrame:"),
            foundation::geometry::NSRect::default(),
        )
    };
    if control.is_null() {
        return cell;
    }
    // SAFETY: as above; the cell keeps the reference `-initWithFrame:` gave.
    unsafe {
        (*(cell as *mut Cell)).control = control;
        (*(cell as *mut Cell)).owns_control = 1;
    }
    cells()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(control as usize, cell as usize);
    cell
}

unsafe extern "C" fn init(receiver: Id, _sel: Sel) -> Id {
    back_with_control(receiver)
}

/// `-initTextCell:`: a button's title is the string, and any other cell's
/// value is.
unsafe extern "C" fn init_text_cell(receiver: Id, _sel: Sel, text: Id) -> Id {
    let cell = back_with_control(receiver);
    let setter = match is_button_cell(cell) {
        true => "setTitle:",
        false => "setStringValue:",
    };
    if responds(cell, setter) {
        // SAFETY: both setters take one string.
        unsafe { send_object(cell, setter, text) };
    }
    cell
}

fn is_button_cell(cell: Id) -> bool {
    let Some(button) = objc::class::lookup(c"NSButtonCell") else {
        return false;
    };
    // SAFETY: an object this runtime allocated.
    unsafe { objc::object::class_of(cell) }
        .is_some_and(|c| c.chain().any(|k| std::ptr::eq(k, button)))
}

fn responds(object: Id, selector: &str) -> bool {
    // SAFETY: an object this runtime allocated.
    unsafe { objc::object::class_of(object) }
        .is_some_and(|c| c.method(objc::sel::get(selector)).is_some())
}

/// `-initImageCell:`: an image and no title.
unsafe extern "C" fn init_image_cell(receiver: Id, _sel: Sel, image: Id) -> Id {
    let cell = back_with_control(receiver);
    if responds(cell, "setImage:") {
        // SAFETY: `-setImage:` takes one image.
        unsafe { send_object(cell, "setImage:", image) };
    }
    cell
}

/// # Safety
///
/// `selector` must name a method taking one object and returning nothing.
unsafe fn send_object(receiver: Id, selector: &str, value: Id) {
    // SAFETY: the caller's obligation.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), value);
    }
}

/// `-dealloc`: a cell that made its control releases it.
unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    forget_state(receiver);
    // SAFETY: an instance of a class laid out as `Cell`.
    let cell = unsafe { &mut *(receiver as *mut Cell) };
    if cell.owns_control != 0 && !cell.control.is_null() {
        let control = std::mem::replace(&mut cell.control, std::ptr::null_mut());
        cell.owns_control = 0;
        {
            let mut table = cells().lock().unwrap_or_else(|e| e.into_inner());
            if table.get(&(control as usize)) == Some(&(receiver as usize)) {
                table.remove(&(control as usize));
            }
        }
        // Something else - a matrix keeping it as a prototype - may still
        // hold the control; it is then given a new cell when one is asked for.
        objc::arc::release(control);
    }
    objc::arc::finalize(receiver);
}

unsafe extern "C" fn cell(receiver: Id, _sel: Sel) -> Id {
    cell_of(receiver)
}

unsafe extern "C" fn control_view(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a cell this module made.
    unsafe { (*(receiver as *const Cell)).control }
}

/// `-accessibilitySetOverrideValue:forAttribute:`, an `NSObject` category.
/// Nothing here serves accessibility, so the override has no reader.
unsafe extern "C" fn accessibility_set_override(
    _receiver: Id,
    _sel: Sel,
    _value: Id,
    _attribute: Id,
) -> u8 {
    1
}

/// State a cell keeps itself rather than in a control, by `(cell, slot)`.
///
/// Mostly what only a cell has - control size, the first-responder ring, a
/// text cell's bezel - which a program sets on a standalone cell it draws with.
/// A cell backed by a control forwards what the control has (see
/// [`FORWARDED`]); these answer for the rest, and for a program's own `NSCell`
/// subclass, which has no control.
fn state() -> &'static Mutex<HashMap<(usize, Slot), usize>> {
    static TABLE: OnceLock<Mutex<HashMap<(usize, Slot), usize>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    ControlSize,
    ShowsFirstResponder,
    Bezeled,
    BezelStyle,
    FocusRingType,
    ControlTint,
    /// Stored inverted, so a fresh cell is enabled.
    Disabled,
    State,
    Highlighted,
    /// An `f64`'s bits.
    DoubleValue,
    Editable,
    ArrowPosition,
    UsesItemFromMenu,
    /// Retained.
    MenuItem,
}

fn get(cell: Id, slot: Slot) -> usize {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(cell as usize, slot))
        .copied()
        .unwrap_or(0)
}

fn set(cell: Id, slot: Slot, value: usize) {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((cell as usize, slot), value);
}

/// Drop everything [`state`] holds for a cell, releasing its menu item.
fn forget_state(cell: Id) {
    let item = {
        let mut table = state().lock().unwrap_or_else(|e| e.into_inner());
        table.retain(|(owner, slot), _| *owner != cell as usize || *slot == Slot::MenuItem);
        table.remove(&(cell as usize, Slot::MenuItem))
    };
    if let Some(item) = item {
        objc::arc::release(item as Id);
    }
}

macro_rules! cell_accessors {
    ($($get:ident / $set:ident : $slot:expr;)*) => {$(
        unsafe extern "C" fn $get(receiver: Id, _sel: Sel) -> usize {
            get(receiver, $slot)
        }
        unsafe extern "C" fn $set(receiver: Id, _sel: Sel, value: usize) {
            set(receiver, $slot, value);
        }
    )*};
}

cell_accessors! {
    control_size / set_control_size: Slot::ControlSize;
    bezel_style / set_bezel_style: Slot::BezelStyle;
    focus_ring_type / set_focus_ring_type: Slot::FocusRingType;
    control_tint / set_control_tint: Slot::ControlTint;
    cell_state / set_cell_state: Slot::State;
    arrow_position / set_arrow_position: Slot::ArrowPosition;
}

macro_rules! cell_flags {
    ($($get:ident / $set:ident : $slot:expr;)*) => {$(
        unsafe extern "C" fn $get(receiver: Id, _sel: Sel) -> u8 {
            u8::from(get(receiver, $slot) != 0)
        }
        unsafe extern "C" fn $set(receiver: Id, _sel: Sel, value: u8) {
            set(receiver, $slot, usize::from(value != 0));
        }
    )*};
}

cell_flags! {
    shows_first_responder / set_shows_first_responder: Slot::ShowsFirstResponder;
    is_bezeled / set_bezeled: Slot::Bezeled;
    is_highlighted / set_highlighted: Slot::Highlighted;
    is_editable / set_editable: Slot::Editable;
    uses_item_from_menu / set_uses_item_from_menu: Slot::UsesItemFromMenu;
}

unsafe extern "C" fn is_enabled(receiver: Id, _sel: Sel) -> u8 {
    u8::from(get(receiver, Slot::Disabled) == 0)
}

unsafe extern "C" fn set_enabled(receiver: Id, _sel: Sel, value: u8) {
    set(receiver, Slot::Disabled, usize::from(value == 0));
}

unsafe extern "C" fn double_value(receiver: Id, _sel: Sel) -> f64 {
    f64::from_bits(get(receiver, Slot::DoubleValue) as u64)
}

unsafe extern "C" fn set_double_value(receiver: Id, _sel: Sel, value: f64) {
    set(receiver, Slot::DoubleValue, value.to_bits() as usize);
}

unsafe extern "C" fn menu_item(receiver: Id, _sel: Sel) -> Id {
    get(receiver, Slot::MenuItem) as Id
}

unsafe extern "C" fn set_menu_item(receiver: Id, _sel: Sel, item: Id) {
    let old = get(receiver, Slot::MenuItem) as Id;
    set(receiver, Slot::MenuItem, objc::arc::retain(item) as usize);
    objc::arc::release(old);
}

/// `-drawWithFrame:inView:` and `-drawInteriorWithFrame:inView:`.
///
/// FIXME (ACCURACY): a standalone cell draws nothing yet. A control here paints
/// itself into the focused view's store; a cell is asked to paint into whatever
/// context is current, which for a program drawing native widgets is its own
/// bitmap context.
unsafe extern "C" fn draw_with_frame(
    _receiver: Id,
    _sel: Sel,
    _frame: foundation::geometry::NSRect,
    _view: Id,
) {
}

/// `-[NSPopUpButtonCell initTextCell:pullsDown:]`
unsafe extern "C" fn init_text_cell_pulls_down(
    receiver: Id,
    sel: Sel,
    text: Id,
    pulls_down: u8,
) -> Id {
    // SAFETY: the same receiver and string.
    let cell = unsafe { init_text_cell(receiver, sel, text) };
    if responds(cell, "setPullsDown:") {
        // SAFETY: `-setPullsDown:` takes one BOOL.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
            f(cell, objc::sel::get("setPullsDown:"), pulls_down);
        }
    }
    cell
}

pub(crate) fn install() {
    let control = objc::class::lookup(c"NSControl").expect("declared before its methods");
    methods!(control, {
        "cell" => cell, c"@16@0:8";
    });
    let object = objc::class::lookup(c"NSObject").expect("the runtime's own");
    methods!(object, {
        "accessibilitySetOverrideValue:forAttribute:" => accessibility_set_override, c"c32@0:8@16@24";
    });
    let cell_class = objc::class::lookup(c"NSCell").expect("declared before its methods");
    methods!(cell_class, {
        "init" => init, c"@16@0:8";
        "initTextCell:" => init_text_cell, c"@24@0:8@16";
        "initImageCell:" => init_image_cell, c"@24@0:8@16";
        "dealloc" => dealloc, c"v16@0:8";
        "controlSize" => control_size, c"Q16@0:8";
        "setControlSize:" => set_control_size, c"v24@0:8Q16";
        "bezelStyle" => bezel_style, c"Q16@0:8";
        "setBezelStyle:" => set_bezel_style, c"v24@0:8Q16";
        "focusRingType" => focus_ring_type, c"Q16@0:8";
        "setFocusRingType:" => set_focus_ring_type, c"v24@0:8Q16";
        "controlTint" => control_tint, c"Q16@0:8";
        "setControlTint:" => set_control_tint, c"v24@0:8Q16";
        "state" => cell_state, c"q16@0:8";
        "setState:" => set_cell_state, c"v24@0:8q16";
        "showsFirstResponder" => shows_first_responder, c"c16@0:8";
        "setShowsFirstResponder:" => set_shows_first_responder, c"v20@0:8c16";
        "isBezeled" => is_bezeled, c"c16@0:8";
        "setBezeled:" => set_bezeled, c"v20@0:8c16";
        "isHighlighted" => is_highlighted, c"c16@0:8";
        "setHighlighted:" => set_highlighted, c"v20@0:8c16";
        "isEditable" => is_editable, c"c16@0:8";
        "setEditable:" => set_editable, c"v20@0:8c16";
        "isEnabled" => is_enabled, c"c16@0:8";
        "setEnabled:" => set_enabled, c"v20@0:8c16";
        "doubleValue" => double_value, c"d16@0:8";
        "setDoubleValue:" => set_double_value, c"v24@0:8d16";
        "drawWithFrame:inView:" => draw_with_frame, c"v56@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16@48";
        "drawInteriorWithFrame:inView:" => draw_with_frame, c"v56@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16@48";
    });
    let popup_cell =
        objc::class::lookup(c"NSPopUpButtonCell").expect("declared before its methods");
    methods!(popup_cell, {
        "initTextCell:pullsDown:" => init_text_cell_pulls_down, c"@28@0:8@16c24";
        "arrowPosition" => arrow_position, c"Q16@0:8";
        "setArrowPosition:" => set_arrow_position, c"v24@0:8Q16";
        "usesItemFromMenu" => uses_item_from_menu, c"c16@0:8";
        "setUsesItemFromMenu:" => set_uses_item_from_menu, c"v20@0:8c16";
    });
    methods!(cell_class, {
        "menuItem" => menu_item, c"@16@0:8";
        "setMenuItem:" => set_menu_item, c"v24@0:8@16";
    });
    let forward = objc::msg::entry_forward_to_first_word();
    for (control_class, cell_class) in KINDS {
        let (Some(control_class), Some(cell_class)) = (
            objc::class::lookup(control_class),
            objc::class::lookup(cell_class),
        ) else {
            continue;
        };
        methods!(cell_class, {
            "controlView" => control_view, c"@16@0:8";
        });
        for name in FORWARDED {
            let sel = objc::sel::get(name);
            let Some(method) = control_class.method(sel) else {
                continue;
            };
            // SAFETY: a registered method's types are a NUL-terminated string.
            let types = unsafe { std::ffi::CStr::from_ptr(method.types()) };
            if types.to_bytes().first() == Some(&b'{') {
                continue;
            }
            cell_class.replace_method(sel, forward, types);
        }
    }
}

#[cfg(test)]
mod tests {
    use objc::object::Id;
    use objc::sel::Sel;

    unsafe fn send(receiver: Id, selector: &str) -> usize {
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(objc::msg::entry());
            f(receiver, objc::sel::get(selector))
        }
    }

    unsafe fn send_word(receiver: Id, selector: &str, value: usize) -> usize {
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, usize) -> usize =
                std::mem::transmute(objc::msg::entry());
            f(receiver, objc::sel::get(selector), value)
        }
    }

    fn new_button_cell(title: &str) -> Id {
        let class = objc::class::lookup(c"NSButtonCell").expect("NSButtonCell");
        let cell = objc::object::create_instance(class, 0);
        let title = foundation::string::from_str(title);
        // SAFETY: `-initTextCell:` takes one string.
        let cell = unsafe { send_word(cell, "initTextCell:", title as usize) } as Id;
        objc::arc::release(title);
        cell
    }

    #[test]
    fn a_program_made_button_cell_keeps_its_button_type() {
        crate::bootstrap();
        let cell = new_button_cell("Remember me");
        // SAFETY: each selector is one the button cell answers, with the
        // argument and return types used here.
        unsafe {
            assert!(!(send(cell, "controlView") as Id).is_null());
            assert_eq!(send(cell, "highlightsBy"), 6);
            send_word(cell, "setButtonType:", 3);
            assert_eq!(send(cell, "buttonType"), 3);
            assert_eq!(send(cell, "highlightsBy"), 1);
            assert_eq!(send(cell, "showsStateBy"), 1);
            assert_eq!(send(cell, "imagePosition"), 2);
            assert_eq!(send(cell, "isBordered") as u8, 0);
            let title = send(cell, "title") as Id;
            assert_eq!(foundation::string::bytes(title), b"Remember me");
        }
        objc::arc::release(cell);
    }

    #[test]
    fn a_button_cell_allowing_mixed_state_cycles_through_it() {
        crate::bootstrap();
        let cell = new_button_cell("");
        // SAFETY: as above.
        unsafe {
            send_word(cell, "setButtonType:", 3);
            assert_eq!(send(cell, "nextState") as isize, 1);
            send_word(cell, "setAllowsMixedState:", 1);
            assert_eq!(send(cell, "allowsMixedState") as u8, 1);
            send(cell, "setNextState");
            assert_eq!(send(cell, "state") as isize, -1);
            send(cell, "setNextState");
            assert_eq!(send(cell, "state") as isize, 1);
            send(cell, "setNextState");
            assert_eq!(send(cell, "state") as isize, 0);
        }
        objc::arc::release(cell);
    }

    #[test]
    fn a_button_cell_set_as_a_prototype_shapes_new_matrix_cells() {
        crate::bootstrap();
        let cell = new_button_cell("Option");
        // SAFETY: as above, and `-setPrototype:` takes one cell.
        let made = unsafe {
            send_word(cell, "setButtonType:", 4);
            let class = objc::class::lookup(c"NSMatrix").expect("NSMatrix");
            let matrix = objc::object::create_instance(class, 0);
            let f: unsafe extern "C" fn(Id, Sel, foundation::geometry::NSRect) -> Id =
                std::mem::transmute(objc::msg::entry());
            let matrix = f(
                matrix,
                objc::sel::get("initWithFrame:"),
                foundation::geometry::NSRect::default(),
            );
            send_word(matrix, "setPrototype:", cell as usize);
            assert_eq!(send(matrix, "prototype") as Id, cell);
            let made = crate::matrix::new_cell(matrix);
            objc::arc::release(cell);
            objc::arc::release(matrix);
            made
        };
        // SAFETY: a button.
        unsafe {
            assert_eq!(send(made, "buttonType"), 4);
            assert_eq!(
                foundation::string::bytes(send(made, "title") as Id),
                b"Option"
            );
        }
        objc::arc::release(made);
    }

    #[test]
    fn a_text_field_cell_answers_what_its_field_does() {
        crate::bootstrap();
        let cell = objc::class::lookup(c"NSTextFieldCell").expect("NSTextFieldCell");
        for name in [
            "setFont:",
            "font",
            "setAlignment:",
            "setDrawsBackground:",
            "setEditable:",
        ] {
            assert!(cell.method(objc::sel::get(name)).is_some(), "{name}");
        }
    }
}
