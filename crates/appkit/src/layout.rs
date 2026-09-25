use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Mutex;

use foundation::geometry::{NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};
use crate::view;

#[repr(C)]
pub struct Constraint {
    isa: *const c_void,
    /// Unretained, as AppKit's are: a constraint does not keep its views alive,
    /// and [`forget`] drops it from the active set when one of them goes.
    first: Id,
    second: Id,
    first_attribute: isize,
    second_attribute: isize,
    relation: isize,
    multiplier: f64,
    constant: f64,
    priority: f32,
    active: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Constraint>();

const REQUIRED: f32 = 1000.0;

/// `NSLayoutRelation`.
const LESS_OR_EQUAL: isize = -1;
const EQUAL: isize = 0;

/// The active constraints, each retained, as the views they constrain would
/// retain them in AppKit.
static ACTIVE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

thread_local! {
    static SOLVING: Cell<bool> = const { Cell::new(false) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
}

fn constraint_of<'a>(receiver: Id) -> &'a mut Constraint {
    // SAFETY: a constraint of this module's, which is what every method here is
    // sent to.
    unsafe { &mut *(receiver as *mut Constraint) }
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    X,
    Y,
}

/// Which quantity along an axis an attribute names; the index into
/// [`Known::values`].
const MIN: usize = 0;
const MAX: usize = 1;
const CENTER: usize = 2;
const SIZE: usize = 3;

/// `NSLayoutAttribute`, along an axis of a view whose superview is `flipped`.
///
/// Margins and baselines are the edges they sit nearest: an AppKit view has no
/// layout margins of its own, and a view here has no text to measure a
/// baseline from.
fn classify(attribute: isize, flipped: bool) -> Option<(Axis, usize)> {
    let (top, bottom) = if flipped { (MIN, MAX) } else { (MAX, MIN) };
    Some(match attribute {
        1 | 5 | 13 | 17 => (Axis::X, MIN),
        2 | 6 | 14 | 18 => (Axis::X, MAX),
        9 | 19 => (Axis::X, CENTER),
        7 => (Axis::X, SIZE),
        3 | 12 | 15 => (Axis::Y, top),
        4 | 11 | 16 => (Axis::Y, bottom),
        10 | 20 => (Axis::Y, CENTER),
        8 => (Axis::Y, SIZE),
        _ => return None,
    })
}

fn along(rect: NSRect, axis: Axis) -> (f64, f64) {
    match axis {
        Axis::X => (rect.origin.x, rect.size.width),
        Axis::Y => (rect.origin.y, rect.size.height),
    }
}

fn measure(rect: NSRect, axis: Axis, what: usize) -> f64 {
    let (origin, size) = along(rect, axis);
    match what {
        MIN => origin,
        MAX => origin + size,
        CENTER => origin + size / 2.0,
        _ => size,
    }
}

/// `item`'s rectangle in the coordinates of `space`, a view's superview.
fn rect_in(item: Id, space: Id) -> Option<NSRect> {
    if !view::is_view(item) {
        return None;
    }
    let bounds = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: view::frame_of(item).size,
    };
    if item == space {
        return Some(bounds);
    }
    Some(view::convert_rect(bounds, item, space))
}

pub(crate) fn is_flipped(view: Id) -> bool {
    let sel = objc::sel::get("isFlipped");
    // SAFETY: `-isFlipped` is `BOOL (id, SEL)`, sent so a guest override answers.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> u8 = std::mem::transmute(objc::msg::entry());
        f(view, sel) != 0
    }
}

/// What the constraints say about one axis of one view.
#[derive(Default)]
struct Known {
    values: [Option<f64>; 4],
    smallest: Option<f64>,
    largest: Option<f64>,
}

impl Known {
    /// The view's new origin and size along this axis, from `current`.
    fn place(&self, current: (f64, f64)) -> (f64, f64) {
        let [min, max, center, size] = self.values;
        let mut size = size
            .or(min.zip(max).map(|(a, b)| b - a))
            .or(min.zip(center).map(|(a, c)| 2.0 * (c - a)))
            .or(max.zip(center).map(|(b, c)| 2.0 * (b - c)))
            .unwrap_or(current.1);
        if let Some(largest) = self.largest {
            size = size.min(largest);
        }
        if let Some(smallest) = self.smallest {
            size = size.max(smallest);
        }
        let size = size.max(0.0);
        let origin = min
            .or(max.map(|b| b - size))
            .or(center.map(|c| c - size / 2.0))
            .unwrap_or(current.0);
        (origin, size)
    }
}

/// Where `target`'s constraints put it, in its superview's coordinates.
fn place(target: Id, constraints: &[&Constraint]) -> Option<NSRect> {
    // SAFETY: a view of this crate's, as every constrained item is.
    let space = unsafe { view::superview_of(target) };
    if !view::is_view(space) {
        return None;
    }
    let flipped = is_flipped(space);
    let current = view::frame_of(target);
    let mut x = Known::default();
    let mut y = Known::default();
    for c in constraints {
        // The side `target` is on, and the value the other side gives it.
        let (attribute, relation, value) = if c.first == target {
            let value = if c.second.is_null() || c.second_attribute == 0 {
                c.constant
            } else {
                let Some((axis, what)) = classify(c.second_attribute, flipped) else {
                    continue;
                };
                let rect = if c.second == target {
                    Some(current)
                } else {
                    rect_in(c.second, space)
                };
                let Some(rect) = rect else {
                    continue;
                };
                c.multiplier * measure(rect, axis, what) + c.constant
            };
            (c.first_attribute, c.relation, value)
        } else if c.second == target && c.relation == EQUAL && c.multiplier != 0.0 {
            let Some((axis, what)) = classify(c.first_attribute, flipped) else {
                continue;
            };
            let Some(rect) = rect_in(c.first, space) else {
                continue;
            };
            let value = (measure(rect, axis, what) - c.constant) / c.multiplier;
            (c.second_attribute, EQUAL, value)
        } else {
            continue;
        };
        let Some((axis, what)) = classify(attribute, flipped) else {
            continue;
        };
        let known = if axis == Axis::X { &mut x } else { &mut y };
        match (relation, what) {
            (EQUAL, _) => known.values[what] = Some(value),
            (LESS_OR_EQUAL, SIZE) => known.largest = Some(value),
            (_, SIZE) => known.smallest = Some(value),
            _ => {}
        }
    }
    let (ox, width) = x.place(along(current, Axis::X));
    let (oy, height) = y.place(along(current, Axis::Y));
    Some(NSRect {
        origin: NSPoint { x: ox, y: oy },
        size: NSSize { width, height },
    })
}

fn differs(a: NSRect, b: NSRect) -> bool {
    [
        a.origin.x - b.origin.x,
        a.origin.y - b.origin.y,
        a.size.width - b.size.width,
        a.size.height - b.size.height,
    ]
    .iter()
    .any(|d| d.abs() > 0.001)
}

pub(crate) fn set_frame(target: Id, frame: NSRect) -> bool {
    if !differs(view::frame_of(target), frame) {
        return false;
    }
    // Sent, so a view that tracks its own size - a web view resizing its
    // engine - hears about it.
    // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
        f(target, objc::sel::get("setFrame:"), frame);
    }
    true
}

/// One pass over every constrained view; whether any of them moved.
fn pass() -> bool {
    let snapshot: Vec<usize> = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // Held for the pass: a guest's `-setFrame:` may deactivate one.
    for c in &snapshot {
        objc::arc::retain(*c as Id);
    }
    let mut constraints: Vec<&Constraint> =
        snapshot.iter().map(|c| &*constraint_of(*c as Id)).collect();
    // Stable, so that among equal priorities the later constraint wins.
    constraints.sort_by(|a, b| a.priority.total_cmp(&b.priority));
    let mut targets: Vec<Id> = Vec::new();
    for c in &constraints {
        for item in [c.first, c.second] {
            if view::is_constrained(item) && !targets.contains(&item) {
                targets.push(item);
            }
        }
    }
    let mut moved = false;
    for target in targets {
        let related: Vec<&Constraint> = constraints
            .iter()
            .copied()
            .filter(|c| c.first == target || c.second == target)
            .collect();
        if let Some(frame) = place(target, &related) {
            moved |= set_frame(target, frame);
        }
    }
    moved |= crate::controls::arrange_stacks();
    for c in snapshot {
        objc::arc::release(c as Id);
    }
    moved
}

/// Bring every constrained view to where its constraints put it.
///
/// Called on every geometry change. Placing one view changes others', which
/// calls back in; that re-entry only marks the running solve dirty.
pub(crate) fn solve() {
    if ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).is_empty() && !crate::controls::has_stacks()
    {
        return;
    }
    if SOLVING.get() {
        DIRTY.set(true);
        return;
    }
    SOLVING.set(true);
    // Bounded, because a contradictory set of equalities never settles.
    for _ in 0..16 {
        DIRTY.set(false);
        if !pass() && !DIRTY.get() {
            break;
        }
    }
    SOLVING.set(false);
}

/// Drop every active constraint on a view that is going away.
pub(crate) fn forget(view: Id) {
    let gone: Vec<usize> = {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (gone, kept) = active.drain(..).partition(|c| {
            let c = constraint_of(*c as Id);
            c.first == view || c.second == view
        });
        *active = kept;
        gone
    };
    for c in gone {
        constraint_of(c as Id).active = 0;
        objc::arc::release(c as Id);
    }
    crate::controls::forget(view);
}

fn set_active(constraint: Id, on: bool) -> bool {
    if constraint.is_null() {
        return false;
    }
    let c = constraint_of(constraint);
    if (c.active != 0) == on {
        return false;
    }
    c.active = usize::from(on);
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if on {
        active.push(objc::arc::retain(constraint) as usize);
    } else if let Some(at) = active.iter().position(|x| *x == constraint as usize) {
        active.remove(at);
        drop(active);
        objc::arc::release(constraint);
    }
    true
}

/// `+constraintWithItem:attribute:relatedBy:toItem:attribute:multiplier:constant:`.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn constraint_with_item(
    class: Id,
    _sel: Sel,
    first: Id,
    first_attribute: isize,
    relation: isize,
    second: Id,
    second_attribute: isize,
    multiplier: f64,
    constant: f64,
) -> Id {
    // SAFETY: the receiver is a class, as a class method's is.
    let Some(class) = (unsafe { objc::class::from_ptr(class as *const objc::class::Class) }) else {
        return std::ptr::null_mut();
    };
    let object = objc::object::create_instance(class, 0);
    let c = constraint_of(object);
    c.first = first;
    c.second = second;
    c.first_attribute = first_attribute;
    c.second_attribute = second_attribute;
    c.relation = relation;
    c.multiplier = multiplier;
    c.constant = constant;
    c.priority = REQUIRED;
    objc::arc::autorelease(object)
}

fn each_in(array: Id, mut f: impl FnMut(Id)) {
    // SAFETY: null or an array of this runtime's.
    let Some(array) = (unsafe { corefoundation::collections::array(array as *const c_void) })
    else {
        return;
    };
    let values: Vec<Id> = array.values.iter().map(|v| *v as Id).collect();
    for value in values {
        f(value);
    }
}

unsafe extern "C" fn activate_constraints(_class: Id, _sel: Sel, constraints: Id) {
    each_in(constraints, |c| {
        set_active(c, true);
    });
    solve();
}

unsafe extern "C" fn deactivate_constraints(_class: Id, _sel: Sel, constraints: Id) {
    each_in(constraints, |c| {
        set_active(c, false);
    });
    solve();
}

unsafe extern "C" fn set_active_method(receiver: Id, _sel: Sel, on: u8) {
    if set_active(receiver, on != 0) {
        solve();
    }
}

unsafe extern "C" fn is_active(receiver: Id, _sel: Sel) -> u8 {
    u8::from(constraint_of(receiver).active != 0)
}

unsafe extern "C" fn first_item(receiver: Id, _sel: Sel) -> Id {
    constraint_of(receiver).first
}

unsafe extern "C" fn second_item(receiver: Id, _sel: Sel) -> Id {
    constraint_of(receiver).second
}

unsafe extern "C" fn first_attribute(receiver: Id, _sel: Sel) -> isize {
    constraint_of(receiver).first_attribute
}

unsafe extern "C" fn second_attribute(receiver: Id, _sel: Sel) -> isize {
    constraint_of(receiver).second_attribute
}

unsafe extern "C" fn relation(receiver: Id, _sel: Sel) -> isize {
    constraint_of(receiver).relation
}

unsafe extern "C" fn multiplier(receiver: Id, _sel: Sel) -> f64 {
    constraint_of(receiver).multiplier
}

unsafe extern "C" fn constant(receiver: Id, _sel: Sel) -> f64 {
    constraint_of(receiver).constant
}

unsafe extern "C" fn set_constant(receiver: Id, _sel: Sel, value: f64) {
    constraint_of(receiver).constant = value;
    if constraint_of(receiver).active != 0 {
        solve();
    }
}

unsafe extern "C" fn priority(receiver: Id, _sel: Sel) -> f32 {
    constraint_of(receiver).priority
}

unsafe extern "C" fn set_priority(receiver: Id, _sel: Sel, value: f32) {
    constraint_of(receiver).priority = value;
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSLayoutConstraint").expect("declared before its methods");
    class_methods!(class, {
        "constraintWithItem:attribute:relatedBy:toItem:attribute:multiplier:constant:" =>
            constraint_with_item, c"@72@0:8@16q24q32@40q48d56d64";
        "activateConstraints:" => activate_constraints, c"v24@0:8@16";
        "deactivateConstraints:" => deactivate_constraints, c"v24@0:8@16";
    });
    methods!(class, {
        "isActive" => is_active, c"c16@0:8";
        "setActive:" => set_active_method, c"v20@0:8c16";
        "firstItem" => first_item, c"@16@0:8";
        "secondItem" => second_item, c"@16@0:8";
        "firstAttribute" => first_attribute, c"q16@0:8";
        "secondAttribute" => second_attribute, c"q16@0:8";
        "relation" => relation, c"q16@0:8";
        "multiplier" => multiplier, c"d16@0:8";
        "constant" => constant, c"d16@0:8";
        "setConstant:" => set_constant, c"v24@0:8d16";
        "priority" => priority, c"f16@0:8";
        "setPriority:" => set_priority, c"v20@0:8f16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(frame: NSRect) -> Id {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSView").expect("NSView");
        let object = objc::object::create_instance(class, 0);
        let sel = objc::sel::get("initWithFrame:");
        // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, NSRect) -> Id =
                std::mem::transmute(objc::msg::entry());
            f(object, sel, frame)
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }
    }

    fn pin(first: Id, attribute: isize, second: Id, constant: f64) -> Id {
        let class = objc::class::lookup(c"NSLayoutConstraint").expect("NSLayoutConstraint");
        // SAFETY: direct calls with the signature installed above.
        unsafe {
            constraint_with_item(
                objc::class::ptr(class) as Id,
                objc::sel::get("constraintWithItem:"),
                first,
                attribute,
                EQUAL,
                second,
                attribute,
                1.0,
                constant,
            )
        }
    }

    fn send(receiver: Id, selector: &str, argument: Id) {
        // SAFETY: every selector used here is `void (id, SEL, id)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
            f(receiver, objc::sel::get(selector), argument);
        }
    }

    #[test]
    fn a_view_pinned_to_its_superviews_edges_follows_it() {
        let parent = view(rect(0.0, 0.0, 400.0, 300.0));
        let child = view(rect(0.0, 0.0, 0.0, 0.0));
        send(parent, "addSubview:", child);
        // SAFETY: `-setTranslatesAutoresizingMaskIntoConstraints:` takes a BOOL.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
            f(
                child,
                objc::sel::get("setTranslatesAutoresizingMaskIntoConstraints:"),
                0,
            );
        }
        let constraints = [
            pin(child, 1, parent, 10.0),
            pin(child, 2, parent, -10.0),
            pin(child, 3, parent, -20.0),
            pin(child, 4, parent, 0.0),
        ];
        let values: Vec<*const c_void> = constraints.iter().map(|c| *c as *const c_void).collect();
        let array = corefoundation::collections::new_array(
            &values,
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        ) as Id;
        let class = objc::class::lookup(c"NSLayoutConstraint").expect("NSLayoutConstraint");
        send(objc::class::ptr(class) as Id, "activateConstraints:", array);
        assert_eq!(view::frame_of(child), rect(10.0, 0.0, 380.0, 280.0));

        // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
            f(
                parent,
                objc::sel::get("setFrame:"),
                rect(0.0, 0.0, 200.0, 100.0),
            );
        }
        assert_eq!(view::frame_of(child), rect(10.0, 0.0, 180.0, 80.0));

        send(
            objc::class::ptr(class) as Id,
            "deactivateConstraints:",
            array,
        );
    }
}
