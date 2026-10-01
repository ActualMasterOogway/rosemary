use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use foundation::geometry::{NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};
use crate::view;

/// `NSEdgeInsets`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct NSEdgeInsets {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

/// `NSUserInterfaceLayoutOrientation`.
const VERTICAL: isize = 1;
/// `NSStackViewDistribution`.
const FILL_EQUALLY: isize = 1;
/// `NSStackViewGravity`: leading and top are 1, centre 2, trailing and bottom 3.
const GRAVITY_CENTER: isize = 2;
const GRAVITY_TRAILING: isize = 3;

#[derive(Clone)]
struct Stack {
    orientation: isize,
    distribution: isize,
    alignment: isize,
    spacing: f64,
    insets: NSEdgeInsets,
    detaches_hidden_views: bool,
    /// Arranged subviews and their gravities, unretained: the subview list
    /// holds each one, and [`forget`] drops it when it goes.
    views: Vec<(usize, isize)>,
}

impl Default for Stack {
    fn default() -> Self {
        Self {
            orientation: 0,
            // `NSStackViewDistributionGravityAreas`.
            distribution: -1,
            // `NSLayoutAttributeCenterY`, for the default horizontal stack.
            alignment: 10,
            spacing: 8.0,
            insets: NSEdgeInsets::default(),
            detaches_hidden_views: true,
            views: Vec::new(),
        }
    }
}

fn stacks() -> &'static Mutex<HashMap<usize, Stack>> {
    static STACKS: OnceLock<Mutex<HashMap<usize, Stack>>> = OnceLock::new();
    STACKS.get_or_init(Default::default)
}

fn stack(receiver: Id) -> Stack {
    stacks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or_default()
}

fn update_stack(receiver: Id, with: impl FnOnce(&mut Stack)) {
    with(
        stacks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(receiver as usize)
            .or_default(),
    );
    crate::layout::solve();
}

pub(crate) fn has_stacks() -> bool {
    stacks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .any(|s| !s.views.is_empty())
}

/// Drop a view that is going away, as a stack or as something arranged in one.
pub(crate) fn forget(object: Id) {
    let mut table = stacks().lock().unwrap_or_else(|e| e.into_inner());
    table.remove(&(object as usize));
    for stack in table.values_mut() {
        stack.views.retain(|(v, _)| *v != object as usize);
    }
    drop(table);
    let key = object as usize;
    for (table, objects) in [
        (boxes(), &slot::BOX_OBJECTS[..]),
        (indicators(), &[][..]),
        (buttons(), &slot::BUTTON_OBJECTS[..]),
        (image_views(), &slot::IMAGE_VIEW_OBJECTS[..]),
        (scrollers(), &[][..]),
    ] {
        let gone = table.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Some(row) = gone {
            for at in objects {
                objc::arc::release(row[*at] as Id);
            }
        }
    }
}

/// The size a stack gives a view it arranges.
///
/// A label is as big as its text and a stack as big as what it arranges, as
/// their intrinsic sizes make them in AppKit; anything else keeps the size its
/// frame or its constraints gave it.
fn fitting_size(v: Id, depth: usize) -> NSSize {
    if let Some(size) = crate::label::intrinsic_size(v) {
        return size;
    }
    let nested = stacks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(v as usize))
        .cloned();
    let Some(stack) = nested.filter(|s| !s.views.is_empty() && depth < 16) else {
        return view::frame_of(v).size;
    };
    let vertical = stack.orientation == VERTICAL;
    let sizes: Vec<NSSize> = stack
        .views
        .iter()
        .map(|(child, _)| *child as Id)
        // SAFETY: views this crate made, still in the list `forget` keeps.
        .filter(|child| unsafe { view::superview_of(*child) } == v)
        .filter(|child| {
            !(stack.detaches_hidden_views && view::is_hidden_or_has_hidden_ancestor(*child))
        })
        .map(|child| fitting_size(child, depth + 1))
        .collect();
    let gaps = stack.spacing * sizes.len().saturating_sub(1) as f64;
    let insets = stack.insets;
    let (along, across) = sizes
        .iter()
        .fold((gaps, 0.0f64), |(along, across), s| match vertical {
            true => (along + s.height, across.max(s.width)),
            false => (along + s.width, across.max(s.height)),
        });
    match vertical {
        true => NSSize {
            width: across + insets.left + insets.right,
            height: along + insets.top + insets.bottom,
        },
        false => NSSize {
            width: along + insets.left + insets.right,
            height: across + insets.top + insets.bottom,
        },
    }
}

/// Place one stack's arranged subviews; whether any of them moved.
fn arrange(receiver: Id, stack: &Stack) -> bool {
    let size = view::frame_of(receiver).size;
    let vertical = stack.orientation == VERTICAL;
    let flipped = crate::layout::is_flipped(receiver);
    let insets = stack.insets;
    let views: Vec<(Id, isize)> = stack
        .views
        .iter()
        .map(|(v, g)| (*v as Id, *g))
        // SAFETY: views this crate made, still in the list `forget` keeps.
        .filter(|(v, _)| unsafe { view::superview_of(*v) } == receiver)
        .filter(|(v, _)| {
            !(stack.detaches_hidden_views && view::is_hidden_or_has_hidden_ancestor(*v))
        })
        .collect();
    if views.is_empty() {
        return false;
    }
    let (length, lead, trail) = if vertical {
        (size.height, insets.top, insets.bottom)
    } else {
        (size.width, insets.left, insets.right)
    };
    let (cross, cross_lead, cross_trail) = if vertical {
        (size.width, insets.left, insets.right)
    } else {
        (size.height, insets.bottom, insets.top)
    };
    let extent = |v: Id| {
        let s = fitting_size(v, 0);
        if vertical {
            (s.height, s.width)
        } else {
            (s.width, s.height)
        }
    };
    // Each view's distance from the leading edge and its length along the axis.
    let mut spans: Vec<(Id, f64, f64)> = Vec::new();
    let spacing = stack.spacing;
    if stack.distribution == FILL_EQUALLY {
        let n = views.len() as f64;
        let each = ((length - lead - trail - spacing * (n - 1.0)) / n).max(0.0);
        for (i, (v, _)) in views.iter().enumerate() {
            spans.push((*v, lead + i as f64 * (each + spacing), each));
        }
    } else {
        let group = |gravity: &dyn Fn(isize) -> bool| -> Vec<(Id, f64)> {
            views
                .iter()
                .filter(|(_, g)| gravity(*g))
                .map(|(v, _)| (*v, extent(*v).0))
                .collect()
        };
        let total = |g: &[(Id, f64)]| {
            g.iter().map(|(_, l)| *l).sum::<f64>() + spacing * (g.len().max(1) - 1) as f64
        };
        let leading = group(&|g| g != GRAVITY_CENTER && g != GRAVITY_TRAILING);
        let center = group(&|g| g == GRAVITY_CENTER);
        let trailing = group(&|g| g == GRAVITY_TRAILING);
        for (views, start) in [
            (&leading, lead),
            (&center, (length - total(&center)) / 2.0),
            (&trailing, length - trail - total(&trailing)),
        ] {
            let mut at = start;
            for (v, l) in views {
                spans.push((*v, at, *l));
                at += l + spacing;
            }
        }
    }
    let mut moved = false;
    for (v, at, along) in spans {
        let (_, across) = extent(v);
        // Distance from the cross axis's bottom or left edge. Edge alignments
        // across the stack's axis hug that edge; every other one centres.
        let offset = match stack.alignment {
            1 | 5 if vertical => cross_lead,
            2 | 6 if vertical => cross - cross_trail - across,
            3 if !vertical => cross - cross_trail - across,
            4 if !vertical => cross_lead,
            _ => cross_lead + (cross - cross_lead - cross_trail - across) / 2.0,
        };
        let frame = if vertical {
            let y = if flipped {
                at
            } else {
                size.height - at - along
            };
            NSRect {
                origin: NSPoint { x: offset, y },
                size: NSSize {
                    width: across,
                    height: along,
                },
            }
        } else {
            let y = if flipped {
                cross - offset - across
            } else {
                offset
            };
            NSRect {
                origin: NSPoint { x: at, y },
                size: NSSize {
                    width: along,
                    height: across,
                },
            }
        };
        moved |= crate::layout::set_frame(v, frame);
    }
    moved
}

/// Place every stack's arranged subviews; whether any of them moved.
pub(crate) fn arrange_stacks() -> bool {
    let snapshot: Vec<(usize, Stack)> = stacks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(_, s)| !s.views.is_empty())
        .map(|(k, s)| (*k, s.clone()))
        .collect();
    let mut moved = false;
    for (receiver, stack) in snapshot {
        moved |= arrange(receiver as Id, &stack);
    }
    moved
}

unsafe extern "C" fn stack_set_orientation(receiver: Id, _sel: Sel, value: isize) {
    update_stack(receiver, |s| {
        // The default alignment follows the orientation, as AppKit's does.
        if s.alignment == 10 && value == VERTICAL {
            s.alignment = 9;
        } else if s.alignment == 9 && value != VERTICAL {
            s.alignment = 10;
        }
        s.orientation = value;
    });
}

unsafe extern "C" fn stack_orientation(receiver: Id, _sel: Sel) -> isize {
    stack(receiver).orientation
}

unsafe extern "C" fn stack_set_distribution(receiver: Id, _sel: Sel, value: isize) {
    update_stack(receiver, |s| s.distribution = value);
}

unsafe extern "C" fn stack_distribution(receiver: Id, _sel: Sel) -> isize {
    stack(receiver).distribution
}

unsafe extern "C" fn stack_set_alignment(receiver: Id, _sel: Sel, value: isize) {
    update_stack(receiver, |s| s.alignment = value);
}

unsafe extern "C" fn stack_alignment(receiver: Id, _sel: Sel) -> isize {
    stack(receiver).alignment
}

unsafe extern "C" fn stack_set_spacing(receiver: Id, _sel: Sel, value: f64) {
    update_stack(receiver, |s| s.spacing = value);
}

unsafe extern "C" fn stack_spacing(receiver: Id, _sel: Sel) -> f64 {
    stack(receiver).spacing
}

unsafe extern "C" fn stack_set_edge_insets(receiver: Id, _sel: Sel, value: NSEdgeInsets) {
    update_stack(receiver, |s| s.insets = value);
}

unsafe extern "C" fn stack_edge_insets(receiver: Id, _sel: Sel) -> NSEdgeInsets {
    stack(receiver).insets
}

unsafe extern "C" fn stack_set_detaches_hidden_views(receiver: Id, _sel: Sel, value: u8) {
    update_stack(receiver, |s| s.detaches_hidden_views = value != 0);
}

unsafe extern "C" fn stack_detaches_hidden_views(receiver: Id, _sel: Sel) -> u8 {
    u8::from(stack(receiver).detaches_hidden_views)
}

fn send_object(receiver: Id, selector: &str, argument: Id) {
    // SAFETY: every selector this is used with is `void (id, SEL, id)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), argument);
    }
}

fn arrange_view(receiver: Id, subview: Id, gravity: isize) {
    if !view::is_view(subview) {
        return;
    }
    // AppKit's own: an arranged subview is placed by the stack, not its mask.
    // SAFETY: `-setTranslatesAutoresizingMaskIntoConstraints:` takes a BOOL.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
        f(
            subview,
            objc::sel::get("setTranslatesAutoresizingMaskIntoConstraints:"),
            0,
        );
    }
    send_object(receiver, "addSubview:", subview);
    update_stack(receiver, |s| {
        s.views.retain(|(v, _)| *v != subview as usize);
        s.views.push((subview as usize, gravity));
    });
}

unsafe extern "C" fn stack_add_view_in_gravity(
    receiver: Id,
    _sel: Sel,
    subview: Id,
    gravity: isize,
) {
    arrange_view(receiver, subview, gravity);
}

unsafe extern "C" fn stack_add_arranged_subview(receiver: Id, _sel: Sel, subview: Id) {
    arrange_view(receiver, subview, 1);
}

unsafe extern "C" fn stack_remove_arranged_subview(receiver: Id, _sel: Sel, subview: Id) {
    update_stack(receiver, |s| {
        s.views.retain(|(v, _)| *v != subview as usize)
    });
}

unsafe extern "C" fn stack_arranged_subviews(receiver: Id, _sel: Sel) -> Id {
    let values: Vec<*const std::ffi::c_void> = stack(receiver)
        .views
        .iter()
        .map(|(v, _)| *v as *const std::ffi::c_void)
        .collect();
    let array = corefoundation::collections::new_array(
        &values,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

fn boxes() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

fn indicators() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

fn buttons() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

fn scrollers() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

fn image_views() -> &'static Mutex<HashMap<usize, Vec<usize>>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, Vec<usize>>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

/// Slots in each control's row. Object slots are retained; the rest are plain
/// values stored as words. The views are the rows' keys, so [`forget`] is what
/// frees them.
mod slot {
    pub const BOX_TYPE: usize = 0;
    pub const FILL_COLOR: usize = 1;
    pub const BORDER_COLOR: usize = 2;
    pub const TRANSPARENT: usize = 3;
    /// An `f64`'s bits.
    pub const BORDER_WIDTH: usize = 4;
    /// `-contentViewMargins`, each an `f64`'s bits.
    pub const MARGIN_WIDTH: usize = 5;
    pub const MARGIN_HEIGHT: usize = 6;
    pub const BOX_OBJECTS: [usize; 2] = [FILL_COLOR, BORDER_COLOR];

    pub const INDETERMINATE: usize = 0;
    pub const STYLE: usize = 1;
    pub const ANIMATING: usize = 2;
    pub const VALUE: usize = 3;

    pub const IMAGE: usize = 0;
    pub const TARGET: usize = 1;
    pub const ACTION: usize = 2;
    pub const TITLE: usize = 3;
    pub const BUTTON_TYPE: usize = 4;
    pub const BUTTON_STATE: usize = 5;
    pub const ALTERNATE_IMAGE: usize = 6;
    /// Stored inverted: a fresh button is bordered.
    pub const NOT_BORDERED: usize = 7;
    pub const BUTTON_TRANSPARENT: usize = 8;
    pub const HIGHLIGHTED: usize = 9;
    pub const BUTTON_OBJECTS: [usize; 3] = [IMAGE, TITLE, ALTERNATE_IMAGE];

    pub const VIEW_IMAGE: usize = 0;
    pub const SCALING: usize = 1;
    pub const ALIGNMENT: usize = 2;
    pub const IMAGE_VIEW_OBJECTS: [usize; 1] = [VIEW_IMAGE];

    /// Both `f64` bits.
    pub const SCROLLER_VALUE: usize = 0;
    pub const KNOB_PROPORTION: usize = 1;
}

const ROW: usize = 10;

fn read(table: &Mutex<HashMap<usize, Vec<usize>>>, receiver: Id, at: usize) -> usize {
    table
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .and_then(|row| row.get(at).copied())
        .unwrap_or(0)
}

fn write(table: &Mutex<HashMap<usize, Vec<usize>>>, receiver: Id, at: usize, value: usize) {
    let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
    table
        .entry(receiver as usize)
        .or_insert_with(|| vec![0; ROW])[at] = value;
}

/// Store a retained object, releasing the one it replaces.
fn write_object(table: &Mutex<HashMap<usize, Vec<usize>>>, receiver: Id, at: usize, value: Id) {
    let old = read(table, receiver, at);
    write(table, receiver, at, objc::arc::retain(value) as usize);
    objc::arc::release(old as Id);
}

unsafe extern "C" fn box_set_box_type(receiver: Id, _sel: Sel, value: usize) {
    write(boxes(), receiver, slot::BOX_TYPE, value);
}

unsafe extern "C" fn box_box_type(receiver: Id, _sel: Sel) -> usize {
    read(boxes(), receiver, slot::BOX_TYPE)
}

/// `-setContentViewMargins:`. Only read back: a box here has no content view
/// of its own to inset, and what a program puts in it places itself.
unsafe extern "C" fn box_set_content_view_margins(receiver: Id, _sel: Sel, margins: NSSize) {
    write(
        boxes(),
        receiver,
        slot::MARGIN_WIDTH,
        margins.width.to_bits() as usize,
    );
    write(
        boxes(),
        receiver,
        slot::MARGIN_HEIGHT,
        margins.height.to_bits() as usize,
    );
}

unsafe extern "C" fn box_content_view_margins(receiver: Id, _sel: Sel) -> NSSize {
    NSSize {
        width: f64::from_bits(read(boxes(), receiver, slot::MARGIN_WIDTH) as u64),
        height: f64::from_bits(read(boxes(), receiver, slot::MARGIN_HEIGHT) as u64),
    }
}

unsafe extern "C" fn box_set_fill_color(receiver: Id, _sel: Sel, value: Id) {
    write_object(boxes(), receiver, slot::FILL_COLOR, value);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn box_fill_color(receiver: Id, _sel: Sel) -> Id {
    read(boxes(), receiver, slot::FILL_COLOR) as Id
}

unsafe extern "C" fn box_set_border_color(receiver: Id, _sel: Sel, value: Id) {
    write_object(boxes(), receiver, slot::BORDER_COLOR, value);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn box_border_color(receiver: Id, _sel: Sel) -> Id {
    read(boxes(), receiver, slot::BORDER_COLOR) as Id
}

unsafe extern "C" fn box_set_transparent(receiver: Id, _sel: Sel, value: u8) {
    write(
        boxes(),
        receiver,
        slot::TRANSPARENT,
        usize::from(value != 0),
    );
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn box_is_transparent(receiver: Id, _sel: Sel) -> u8 {
    u8::from(read(boxes(), receiver, slot::TRANSPARENT) != 0)
}

unsafe extern "C" fn box_set_border_width(receiver: Id, _sel: Sel, value: f64) {
    write(
        boxes(),
        receiver,
        slot::BORDER_WIDTH,
        value.to_bits() as usize,
    );
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn box_border_width(receiver: Id, _sel: Sel) -> f64 {
    f64::from_bits(read(boxes(), receiver, slot::BORDER_WIDTH) as u64)
}

/// `NSBoxCustom`, the type whose look is entirely its fill and border colours.
const BOX_CUSTOM: usize = 4;

/// `-drawRect:` for a custom box: the fill over its bounds, then a border of
/// its width inside them. Other box types keep their earlier look, which here
/// is nothing drawn.
unsafe extern "C" fn box_draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    if read(boxes(), receiver, slot::BOX_TYPE) != BOX_CUSTOM
        || read(boxes(), receiver, slot::TRANSPARENT) != 0
    {
        return;
    }
    let bounds = view::bounds_of(receiver);
    let fill = |color: Id, rect: NSRect| {
        if color.is_null() || rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            return;
        }
        // SAFETY: `-setFill` takes no arguments.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
            f(color, objc::sel::get("setFill"));
        }
        crate::color::NSRectFill(rect);
    };
    fill(read(boxes(), receiver, slot::FILL_COLOR) as Id, bounds);
    let width = f64::from_bits(read(boxes(), receiver, slot::BORDER_WIDTH) as u64);
    let border = read(boxes(), receiver, slot::BORDER_COLOR) as Id;
    if width > 0.0 {
        let NSRect { origin, size } = bounds;
        let edge = |x: f64, y: f64, w: f64, h: f64| NSRect {
            origin: NSPoint { x, y },
            size: NSSize {
                width: w,
                height: h,
            },
        };
        fill(border, edge(origin.x, origin.y, size.width, width));
        fill(
            border,
            edge(origin.x, origin.y + size.height - width, size.width, width),
        );
        fill(border, edge(origin.x, origin.y, width, size.height));
        fill(
            border,
            edge(origin.x + size.width - width, origin.y, width, size.height),
        );
    }
}

unsafe extern "C" fn indicator_set_indeterminate(receiver: Id, _sel: Sel, value: u8) {
    // Stored inverted: a fresh indicator is indeterminate.
    write(
        indicators(),
        receiver,
        slot::INDETERMINATE,
        usize::from(value == 0),
    );
}

unsafe extern "C" fn indicator_is_indeterminate(receiver: Id, _sel: Sel) -> u8 {
    u8::from(read(indicators(), receiver, slot::INDETERMINATE) == 0)
}

unsafe extern "C" fn indicator_set_style(receiver: Id, _sel: Sel, value: usize) {
    write(indicators(), receiver, slot::STYLE, value);
}

unsafe extern "C" fn indicator_style(receiver: Id, _sel: Sel) -> usize {
    read(indicators(), receiver, slot::STYLE)
}

/// `-startAnimation:`. Nothing spins, since nothing draws; the state is kept
/// for `-isAnimating`-style reads a program makes of its own indicator.
unsafe extern "C" fn indicator_start_animation(receiver: Id, _sel: Sel, _sender: Id) {
    write(indicators(), receiver, slot::ANIMATING, 1);
}

unsafe extern "C" fn indicator_stop_animation(receiver: Id, _sel: Sel, _sender: Id) {
    write(indicators(), receiver, slot::ANIMATING, 0);
}

unsafe extern "C" fn indicator_set_double_value(receiver: Id, _sel: Sel, value: f64) {
    write(
        indicators(),
        receiver,
        slot::VALUE,
        value.to_bits() as usize,
    );
}

unsafe extern "C" fn indicator_double_value(receiver: Id, _sel: Sel) -> f64 {
    f64::from_bits(read(indicators(), receiver, slot::VALUE) as u64)
}

unsafe extern "C" fn button_set_image(receiver: Id, _sel: Sel, value: Id) {
    write_object(buttons(), receiver, slot::IMAGE, value);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn button_set_alternate_image(receiver: Id, _sel: Sel, value: Id) {
    write_object(buttons(), receiver, slot::ALTERNATE_IMAGE, value);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn button_alternate_image(receiver: Id, _sel: Sel) -> Id {
    read(buttons(), receiver, slot::ALTERNATE_IMAGE) as Id
}

unsafe extern "C" fn button_set_bordered(receiver: Id, _sel: Sel, value: u8) {
    write(
        buttons(),
        receiver,
        slot::NOT_BORDERED,
        usize::from(value == 0),
    );
}

unsafe extern "C" fn button_is_bordered(receiver: Id, _sel: Sel) -> u8 {
    u8::from(read(buttons(), receiver, slot::NOT_BORDERED) == 0)
}

unsafe extern "C" fn button_set_transparent(receiver: Id, _sel: Sel, value: u8) {
    write(
        buttons(),
        receiver,
        slot::BUTTON_TRANSPARENT,
        usize::from(value != 0),
    );
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn button_is_transparent(receiver: Id, _sel: Sel) -> u8 {
    read(buttons(), receiver, slot::BUTTON_TRANSPARENT) as u8
}

fn set_highlighted(receiver: Id, on: bool) {
    if (read(buttons(), receiver, slot::HIGHLIGHTED) != 0) != on {
        write(buttons(), receiver, slot::HIGHLIGHTED, usize::from(on));
        crate::backing::needs_display(receiver);
    }
}

unsafe extern "C" fn button_set_highlighted(receiver: Id, _sel: Sel, value: u8) {
    set_highlighted(receiver, value != 0);
}

unsafe extern "C" fn button_is_highlighted(receiver: Id, _sel: Sel) -> u8 {
    read(buttons(), receiver, slot::HIGHLIGHTED) as u8
}

/// `-drawRect:`: the image, or the alternate image while pressed.
///
/// No bezel or title is drawn: the buttons that reach here are image buttons,
/// and a program that wants a bezel draws its own.
unsafe extern "C" fn button_draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    if read(buttons(), receiver, slot::BUTTON_TRANSPARENT) != 0 {
        return;
    }
    let alternate = read(buttons(), receiver, slot::ALTERNATE_IMAGE) as Id;
    let image = match read(buttons(), receiver, slot::HIGHLIGHTED) != 0 && !alternate.is_null() {
        true => alternate,
        false => read(buttons(), receiver, slot::IMAGE) as Id,
    };
    if image.is_null() {
        return;
    }
    // SAFETY: `-size` answers an `NSSize`, and the draw call is the prototype
    // `NSImage` declares for it.
    unsafe {
        let size: unsafe extern "C" fn(Id, Sel) -> NSSize = std::mem::transmute(objc::msg::entry());
        let natural = size(image, objc::sel::get("size"));
        if natural.width <= 0.0 || natural.height <= 0.0 {
            return;
        }
        // Proportionally down and centred, `NSButton`'s defaults.
        let destination = image_placement(view::bounds_of(receiver), natural, 0, 0);
        let draw: unsafe extern "C" fn(Id, Sel, NSRect, NSRect, usize, f64) =
            std::mem::transmute(objc::msg::entry());
        draw(
            image,
            objc::sel::get("drawInRect:fromRect:operation:fraction:"),
            destination,
            NSRect::default(),
            2,
            1.0,
        );
    }
}

/// `-mouseDown:`: pressed until the button is released.
unsafe extern "C" fn button_mouse_down(receiver: Id, _sel: Sel, _event: Id) {
    set_highlighted(receiver, true);
}

/// `-mouseUp:`: a click, if the press began here. Events are delivered to the
/// view under the pointer, so a release elsewhere never reaches this and the
/// press is undone by the next one.
unsafe extern "C" fn button_mouse_up(receiver: Id, _sel: Sel, _event: Id) {
    if read(buttons(), receiver, slot::HIGHLIGHTED) == 0 {
        return;
    }
    set_highlighted(receiver, false);
    let action = read(buttons(), receiver, slot::ACTION) as Sel;
    let app = crate::application::shared();
    if action.is_null() || app.is_null() {
        return;
    }
    let target = read(buttons(), receiver, slot::TARGET) as Id;
    // SAFETY: `-sendAction:to:from:` is `BOOL (id, SEL, SEL, id, id)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Sel, Id, Id) -> u8 =
            std::mem::transmute(objc::msg::entry());
        f(
            app,
            objc::sel::get("sendAction:to:from:"),
            action,
            target,
            receiver,
        );
    }
}

unsafe extern "C" fn button_image(receiver: Id, _sel: Sel) -> Id {
    read(buttons(), receiver, slot::IMAGE) as Id
}

unsafe extern "C" fn button_set_title(receiver: Id, _sel: Sel, value: Id) {
    write_object(buttons(), receiver, slot::TITLE, value);
}

unsafe extern "C" fn button_title(receiver: Id, _sel: Sel) -> Id {
    read(buttons(), receiver, slot::TITLE) as Id
}

unsafe extern "C" fn button_set_type(receiver: Id, _sel: Sel, value: usize) {
    write(buttons(), receiver, slot::BUTTON_TYPE, value);
}

unsafe extern "C" fn button_type(receiver: Id, _sel: Sel) -> usize {
    read(buttons(), receiver, slot::BUTTON_TYPE)
}

unsafe extern "C" fn button_set_state(receiver: Id, _sel: Sel, value: isize) {
    write(buttons(), receiver, slot::BUTTON_STATE, value as usize);
}

unsafe extern "C" fn button_state(receiver: Id, _sel: Sel) -> isize {
    read(buttons(), receiver, slot::BUTTON_STATE) as isize
}

pub(crate) fn accessory_checkbox(receiver: Id) -> Option<(String, bool)> {
    let class = unsafe { objc::object::class_of(receiver) }?;
    let button = objc::class::lookup(c"NSButton")?;
    if !class.chain().any(|ancestor| std::ptr::eq(ancestor, button))
        || read(buttons(), receiver, slot::BUTTON_TYPE) != 3
    {
        return None;
    }
    let title = read(buttons(), receiver, slot::TITLE) as Id;
    let title = if title.is_null() {
        String::new()
    } else {
        String::from_utf8_lossy(&unsafe { foundation::string::bytes(title) }).into_owned()
    };
    Some((title, read(buttons(), receiver, slot::BUTTON_STATE) != 0))
}

pub(crate) fn set_accessory_checkbox(receiver: Id, checked: bool) {
    write(
        buttons(),
        receiver,
        slot::BUTTON_STATE,
        usize::from(checked),
    );
}

unsafe extern "C" fn image_view_set_image(receiver: Id, _sel: Sel, image: Id) {
    write_object(image_views(), receiver, slot::VIEW_IMAGE, image);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn image_view_image(receiver: Id, _sel: Sel) -> Id {
    read(image_views(), receiver, slot::VIEW_IMAGE) as Id
}

unsafe extern "C" fn image_view_set_scaling(receiver: Id, _sel: Sel, value: usize) {
    write(image_views(), receiver, slot::SCALING, value);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn image_view_scaling(receiver: Id, _sel: Sel) -> usize {
    read(image_views(), receiver, slot::SCALING)
}

unsafe extern "C" fn image_view_set_alignment(receiver: Id, _sel: Sel, value: usize) {
    write(image_views(), receiver, slot::ALIGNMENT, value);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn image_view_alignment(receiver: Id, _sel: Sel) -> usize {
    read(image_views(), receiver, slot::ALIGNMENT)
}

/// `NSImageScaling`. Proportionally down, 0, is the default arm below.
mod scaling {
    pub const AXES_INDEPENDENTLY: usize = 1;
    pub const NONE: usize = 2;
    pub const PROPORTIONALLY_UP_OR_DOWN: usize = 3;
}

/// Where an image of `image` size goes in `bounds`, for an image view's
/// `NSImageScaling` and `NSImageAlignment`.
fn image_placement(bounds: NSRect, image: NSSize, scaling: usize, alignment: usize) -> NSRect {
    let fit = (bounds.size.width / image.width).min(bounds.size.height / image.height);
    let size = match scaling {
        scaling::AXES_INDEPENDENTLY => bounds.size,
        scaling::NONE => image,
        scaling::PROPORTIONALLY_UP_OR_DOWN => NSSize {
            width: image.width * fit,
            height: image.height * fit,
        },
        // Proportionally down, and the default for a value this does not know.
        _ => NSSize {
            width: image.width * fit.min(1.0),
            height: image.height * fit.min(1.0),
        },
    };
    // `NSImageAlignment`: center, top, top left, top right, left, bottom,
    // bottom left, bottom right, right - as fractions of the room left over,
    // with y measured up from the bottom.
    let (x, y) = match alignment {
        1 => (0.5, 1.0),
        2 => (0.0, 1.0),
        3 => (1.0, 1.0),
        4 => (0.0, 0.5),
        5 => (0.5, 0.0),
        6 => (0.0, 0.0),
        7 => (1.0, 0.0),
        8 => (1.0, 0.5),
        _ => (0.5, 0.5),
    };
    NSRect {
        origin: NSPoint {
            x: bounds.origin.x + (bounds.size.width - size.width) * x,
            y: bounds.origin.y + (bounds.size.height - size.height) * y,
        },
        size,
    }
}

unsafe extern "C" fn image_view_draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    let image = read(image_views(), receiver, slot::VIEW_IMAGE) as Id;
    if image.is_null() {
        return;
    }
    // SAFETY: `-size` answers an `NSSize`, and the draw call is the prototype
    // `NSImage` declares for it.
    unsafe {
        let size: unsafe extern "C" fn(Id, Sel) -> NSSize = std::mem::transmute(objc::msg::entry());
        let natural = size(image, objc::sel::get("size"));
        if natural.width <= 0.0 || natural.height <= 0.0 {
            return;
        }
        let bounds = view::bounds_of(receiver);
        let destination = image_placement(
            bounds,
            natural,
            read(image_views(), receiver, slot::SCALING),
            read(image_views(), receiver, slot::ALIGNMENT),
        );
        let draw: unsafe extern "C" fn(Id, Sel, NSRect, NSRect, usize, f64) =
            std::mem::transmute(objc::msg::entry());
        // A zero source rectangle is the whole image; 2 is source-over.
        draw(
            image,
            objc::sel::get("drawInRect:fromRect:operation:fraction:"),
            destination,
            NSRect::default(),
            2,
            1.0,
        );
    }
}

/// `-setTarget:`, unretained as a control's target is.
unsafe extern "C" fn button_set_target(receiver: Id, _sel: Sel, value: Id) {
    write(buttons(), receiver, slot::TARGET, value as usize);
}

unsafe extern "C" fn button_target(receiver: Id, _sel: Sel) -> Id {
    read(buttons(), receiver, slot::TARGET) as Id
}

unsafe extern "C" fn button_set_action(receiver: Id, _sel: Sel, value: Sel) {
    write(buttons(), receiver, slot::ACTION, value as usize);
}

unsafe extern "C" fn button_action(receiver: Id, _sel: Sel) -> Sel {
    read(buttons(), receiver, slot::ACTION) as Sel
}

/// A fresh, autoreleased view of the receiving class with a zero frame.
fn new_view(class: Id) -> Option<Id> {
    // SAFETY: the receiver of a class method is a class.
    let class = unsafe { objc::class::from_ptr(class as *const objc::class::Class) }?;
    let object = objc::object::create_instance(class, 0);
    let frame = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 0.0,
            height: 0.0,
        },
    };
    // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`, sent so a subclass's
    // override runs.
    let object = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(object, objc::sel::get("initWithFrame:"), frame)
    };
    (!object.is_null()).then(|| objc::arc::autorelease(object))
}

/// `+buttonWithImage:target:action:`, sized to the image as AppKit sizes it to
/// fit.
unsafe extern "C" fn button_with_image(
    class: Id,
    _sel: Sel,
    image: Id,
    target: Id,
    action: Sel,
) -> Id {
    let Some(button) = new_view(class) else {
        return std::ptr::null_mut();
    };
    send_object(button, "setImage:", image);
    send_object(button, "setTarget:", target);
    // SAFETY: `-setAction:` is `void (id, SEL, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Sel) = std::mem::transmute(objc::msg::entry());
        f(button, objc::sel::get("setAction:"), action);
    }
    if !image.is_null() {
        // SAFETY: `-size` is `NSSize (id, SEL)`, and `-setFrameSize:` its mirror.
        unsafe {
            let size: unsafe extern "C" fn(Id, Sel) -> NSSize =
                std::mem::transmute(objc::msg::entry());
            let size = size(image, objc::sel::get("size"));
            let set: unsafe extern "C" fn(Id, Sel, NSSize) =
                std::mem::transmute(objc::msg::entry());
            set(button, objc::sel::get("setFrameSize:"), size);
        }
    }
    button
}

/// `+labelWithString:`: a text field that can be neither edited nor selected.
unsafe extern "C" fn label_with_string(class: Id, _sel: Sel, string: Id) -> Id {
    let Some(label) = new_view(class) else {
        return std::ptr::null_mut();
    };
    send_object(label, "setStringValue:", string);
    for selector in ["setEditable:", "setSelectable:"] {
        // SAFETY: both are `void (id, SEL, BOOL)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
            f(label, objc::sel::get(selector), 0);
        }
    }
    label
}

/// `+[NSScroller scrollerWidth]`, the breadth of a scroller in points.
///
/// The legacy metric, which is the one a program that lays a scroller out by
/// hand is asking for: it subtracts this from a content width. Nothing here
/// draws a scroller, so this is a number rather than a measurement - but it has
/// to be *the* number, because the guest's arithmetic has to agree with the
/// frame [`scroller_init_with_frame`] hands back.
const SCROLLER_WIDTH: f64 = 15.0;

/// The small control size's scroller, which is the one size that differs.
const SMALL_SCROLLER_WIDTH: f64 = 11.0;

unsafe extern "C" fn scroller_width(_class: Id, _sel: Sel) -> f64 {
    SCROLLER_WIDTH
}

/// `NSControlSize`: regular is 0, small 1, mini 2.
unsafe extern "C" fn scroller_width_for_control_size(_class: Id, _sel: Sel, size: usize) -> f64 {
    match size {
        0 => SCROLLER_WIDTH,
        _ => SMALL_SCROLLER_WIDTH,
    }
}

/// `-[NSScroller initWithFrame:]`, which is not the inherited one: a scroller
/// constrains the frame it is given.
///
/// Its long side is the direction it scrolls - a frame wider than it is tall is
/// a horizontal scroller - and its short side is forced to [`SCROLLER_WIDTH`],
/// because a scroller has one breadth and AppKit does not let a caller pick
/// another. A program reads `-frame` back to lay out the view beside it, so the
/// constraint is the observable part of this method.
unsafe extern "C" fn scroller_init_with_frame(receiver: Id, sel: Sel, frame: NSRect) -> Id {
    let mut frame = frame;
    match frame.size.width > frame.size.height {
        true => frame.size.height = SCROLLER_WIDTH,
        false => frame.size.width = SCROLLER_WIDTH,
    }
    // `NSView`'s designated initialiser, reached by its class rather than by
    // sending: a send would find this override again.
    let Some(method) = objc::class::lookup(c"NSView")
        .and_then(|class| class.method(objc::sel::get("initWithFrame:")))
    else {
        return std::ptr::null_mut();
    };
    // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) -> Id =
            std::mem::transmute(method.implementation());
        f(receiver, sel, frame)
    }
}

/// The shortest knob a scroller draws, so that a huge document still leaves
/// something to grab.
const MIN_KNOB_LENGTH: f64 = 18.0;

/// `NSScrollerPart`.
mod part {
    pub const DECREMENT_PAGE: usize = 1;
    pub const KNOB: usize = 2;
    pub const INCREMENT_PAGE: usize = 3;
    pub const KNOB_SLOT: usize = 6;
}

fn scroller_f64(receiver: Id, at: usize) -> f64 {
    f64::from_bits(read(scrollers(), receiver, at) as u64)
}

/// AppKit clamps both the position and the proportion into `0..=1`, and a
/// program reads the clamped value back.
fn set_scroller_f64(receiver: Id, at: usize, value: f64) {
    let value = match value.is_nan() {
        true => 0.0,
        false => value.clamp(0.0, 1.0),
    };
    write(scrollers(), receiver, at, value.to_bits() as usize);
}

unsafe extern "C" fn scroller_set_double_value(receiver: Id, _sel: Sel, value: f64) {
    set_scroller_f64(receiver, slot::SCROLLER_VALUE, value);
}

unsafe extern "C" fn scroller_double_value(receiver: Id, _sel: Sel) -> f64 {
    scroller_f64(receiver, slot::SCROLLER_VALUE)
}

unsafe extern "C" fn scroller_set_float_value(receiver: Id, _sel: Sel, value: f32) {
    set_scroller_f64(receiver, slot::SCROLLER_VALUE, f64::from(value));
}

unsafe extern "C" fn scroller_float_value(receiver: Id, _sel: Sel) -> f32 {
    scroller_f64(receiver, slot::SCROLLER_VALUE) as f32
}

unsafe extern "C" fn scroller_set_knob_proportion(receiver: Id, _sel: Sel, value: f64) {
    set_scroller_f64(receiver, slot::KNOB_PROPORTION, value);
}

unsafe extern "C" fn scroller_knob_proportion(receiver: Id, _sel: Sel) -> f64 {
    scroller_f64(receiver, slot::KNOB_PROPORTION)
}

/// `-[NSScroller rectForPart:]`, in the scroller's own flipped coordinates.
///
/// A scroller since 10.7 has no arrows, so the slot is the whole bounds and
/// the line parts are empty. The knob is the proportion's share of the slot,
/// placed by the value along what is left; with nothing to scroll (a
/// proportion of 0 or 1) there is no knob, and so no page parts either.
/// Qt's style lays out and hit-tests its scroll bars from these rectangles.
unsafe extern "C" fn scroller_rect_for_part(receiver: Id, _sel: Sel, which: usize) -> NSRect {
    let size = view::frame_of(receiver).size;
    scroller_part(
        size,
        scroller_f64(receiver, slot::SCROLLER_VALUE),
        scroller_f64(receiver, slot::KNOB_PROPORTION),
        which,
    )
}

fn scroller_part(size: NSSize, value: f64, proportion: f64, which: usize) -> NSRect {
    let bounds = NSRect {
        origin: NSPoint::default(),
        size,
    };
    // The same test `-initWithFrame:` makes: wider than tall is horizontal.
    let horizontal = size.width > size.height;
    let length = match horizontal {
        true => size.width,
        false => size.height,
    };
    // A span `(start, extent)` along the scrolling axis, full breadth across.
    let span = |start: f64, extent: f64| match horizontal {
        true => NSRect {
            origin: NSPoint { x: start, y: 0.0 },
            size: NSSize {
                width: extent,
                height: size.height,
            },
        },
        false => NSRect {
            origin: NSPoint { x: 0.0, y: start },
            size: NSSize {
                width: size.width,
                height: extent,
            },
        },
    };
    let knob = (proportion > 0.0 && proportion < 1.0 && length >= MIN_KNOB_LENGTH).then(|| {
        let extent = (length * proportion).max(MIN_KNOB_LENGTH);
        ((length - extent) * value, extent)
    });
    match (which, knob) {
        (part::KNOB_SLOT, _) => bounds,
        (part::KNOB, Some((start, extent))) => span(start, extent),
        (part::DECREMENT_PAGE, Some((start, _))) => span(0.0, start),
        (part::INCREMENT_PAGE, Some((start, extent))) => {
            span(start + extent, length - start - extent)
        }
        _ => NSRect::default(),
    }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSStackView").expect("declared before its methods");
    methods!(class, {
        "setOrientation:" => stack_set_orientation, c"v24@0:8q16";
        "orientation" => stack_orientation, c"q16@0:8";
        "setDistribution:" => stack_set_distribution, c"v24@0:8q16";
        "distribution" => stack_distribution, c"q16@0:8";
        "setAlignment:" => stack_set_alignment, c"v24@0:8q16";
        "alignment" => stack_alignment, c"q16@0:8";
        "setSpacing:" => stack_set_spacing, c"v24@0:8d16";
        "spacing" => stack_spacing, c"d16@0:8";
        "setEdgeInsets:" => stack_set_edge_insets, c"v48@0:8{NSEdgeInsets=dddd}16";
        "edgeInsets" => stack_edge_insets, c"{NSEdgeInsets=dddd}16@0:8";
        "setDetachesHiddenViews:" => stack_set_detaches_hidden_views, c"v20@0:8c16";
        "detachesHiddenViews" => stack_detaches_hidden_views, c"c16@0:8";
        "addView:inGravity:" => stack_add_view_in_gravity, c"v32@0:8@16q24";
        "addArrangedSubview:" => stack_add_arranged_subview, c"v24@0:8@16";
        "removeArrangedSubview:" => stack_remove_arranged_subview, c"v24@0:8@16";
        "arrangedSubviews" => stack_arranged_subviews, c"@16@0:8";
    });

    let class = objc::class::lookup(c"NSBox").expect("declared before its methods");
    methods!(class, {
        "setBoxType:" => box_set_box_type, c"v24@0:8Q16";
        "boxType" => box_box_type, c"Q16@0:8";
        "setFillColor:" => box_set_fill_color, c"v24@0:8@16";
        "fillColor" => box_fill_color, c"@16@0:8";
        "setBorderColor:" => box_set_border_color, c"v24@0:8@16";
        "borderColor" => box_border_color, c"@16@0:8";
        "setTransparent:" => box_set_transparent, c"v20@0:8c16";
        "isTransparent" => box_is_transparent, c"c16@0:8";
        "setBorderWidth:" => box_set_border_width, c"v24@0:8d16";
        "setContentViewMargins:" => box_set_content_view_margins, c"v32@0:8{CGSize=dd}16";
        "contentViewMargins" => box_content_view_margins, c"{CGSize=dd}16@0:8";
        "borderWidth" => box_border_width, c"d16@0:8";
        "drawRect:" => box_draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });

    let class = objc::class::lookup(c"NSProgressIndicator").expect("declared before its methods");
    methods!(class, {
        "setIndeterminate:" => indicator_set_indeterminate, c"v20@0:8c16";
        "isIndeterminate" => indicator_is_indeterminate, c"c16@0:8";
        "setStyle:" => indicator_set_style, c"v24@0:8Q16";
        "style" => indicator_style, c"Q16@0:8";
        "startAnimation:" => indicator_start_animation, c"v24@0:8@16";
        "stopAnimation:" => indicator_stop_animation, c"v24@0:8@16";
        "setDoubleValue:" => indicator_set_double_value, c"v24@0:8d16";
        "doubleValue" => indicator_double_value, c"d16@0:8";
    });

    let class = objc::class::lookup(c"NSButton").expect("declared before its methods");
    class_methods!(class, {
        "buttonWithImage:target:action:" => button_with_image, c"@40@0:8@16@24:32";
    });
    methods!(class, {
        "setImage:" => button_set_image, c"v24@0:8@16";
        "image" => button_image, c"@16@0:8";
        "setTitle:" => button_set_title, c"v24@0:8@16";
        "title" => button_title, c"@16@0:8";
        "setButtonType:" => button_set_type, c"v24@0:8Q16";
        "buttonType" => button_type, c"Q16@0:8";
        "setState:" => button_set_state, c"v24@0:8q16";
        "state" => button_state, c"q16@0:8";
        "setTarget:" => button_set_target, c"v24@0:8@16";
        "target" => button_target, c"@16@0:8";
        "setAction:" => button_set_action, c"v24@0:8:16";
        "action" => button_action, c":16@0:8";
        "setAlternateImage:" => button_set_alternate_image, c"v24@0:8@16";
        "alternateImage" => button_alternate_image, c"@16@0:8";
        "setBordered:" => button_set_bordered, c"v20@0:8c16";
        "isBordered" => button_is_bordered, c"c16@0:8";
        "setTransparent:" => button_set_transparent, c"v20@0:8c16";
        "isTransparent" => button_is_transparent, c"c16@0:8";
        "setHighlighted:" => button_set_highlighted, c"v20@0:8c16";
        "highlight:" => button_set_highlighted, c"v20@0:8c16";
        "isHighlighted" => button_is_highlighted, c"c16@0:8";
        "drawRect:" => button_draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "mouseDown:" => button_mouse_down, c"v24@0:8@16";
        "mouseUp:" => button_mouse_up, c"v24@0:8@16";
    });

    let class = objc::class::lookup(c"NSImageView").expect("declared before its methods");
    methods!(class, {
        "setImage:" => image_view_set_image, c"v24@0:8@16";
        "image" => image_view_image, c"@16@0:8";
        "setImageScaling:" => image_view_set_scaling, c"v24@0:8Q16";
        "imageScaling" => image_view_scaling, c"Q16@0:8";
        "setImageAlignment:" => image_view_set_alignment, c"v24@0:8Q16";
        "imageAlignment" => image_view_alignment, c"Q16@0:8";
        "drawRect:" => image_view_draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });

    let class = objc::class::lookup(c"NSTextField").expect("declared before its methods");
    class_methods!(class, {
        "labelWithString:" => label_with_string, c"@24@0:8@16";
    });

    let class = objc::class::lookup(c"NSScroller").expect("declared before its methods");
    class_methods!(class, {
        "scrollerWidth" => scroller_width, c"d16@0:8";
        "scrollerWidthForControlSize:" => scroller_width_for_control_size, c"d24@0:8Q16";
    });
    methods!(class, {
        "initWithFrame:" => scroller_init_with_frame,
            c"@48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "setDoubleValue:" => scroller_set_double_value, c"v24@0:8d16";
        "doubleValue" => scroller_double_value, c"d16@0:8";
        "setFloatValue:" => scroller_set_float_value, c"v20@0:8f16";
        "floatValue" => scroller_float_value, c"f16@0:8";
        "setKnobProportion:" => scroller_set_knob_proportion, c"v24@0:8d16";
        "knobProportion" => scroller_knob_proportion, c"d16@0:8";
        "rectForPart:" => scroller_rect_for_part,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}24@0:8Q16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_title_is_retained_and_read_back() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSButton").expect("NSButton");
        let button = objc::object::create_instance(class, 0);
        let title = foundation::string::from_str("Install");
        // SAFETY: the receiver is a button and the argument is an NSString.
        unsafe { button_set_title(button, objc::sel::get("setTitle:"), title) };
        objc::arc::release(title);
        // SAFETY: the receiver is the same live button.
        let stored = unsafe { button_title(button, objc::sel::get("title")) };
        assert_eq!(unsafe { foundation::string::bytes(stored) }, b"Install");
        objc::arc::release(button);
    }

    #[test]
    fn accessory_checkbox_state_is_read_back_from_button() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSButton").expect("NSButton");
        let button = objc::object::create_instance(class, 0);
        let title = foundation::string::from_str("Show details");
        unsafe {
            button_set_title(button, objc::sel::get("setTitle:"), title);
            button_set_type(button, objc::sel::get("setButtonType:"), 3);
        }
        objc::arc::release(title);
        assert_eq!(
            accessory_checkbox(button),
            Some(("Show details".into(), false))
        );
        set_accessory_checkbox(button, true);
        assert_eq!(unsafe { button_state(button, objc::sel::get("state")) }, 1);
        objc::arc::release(button);
    }

    #[test]
    fn an_image_view_centres_its_image_and_only_shrinks_it() {
        const PROPORTIONALLY_DOWN: usize = 0;
        let bounds = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 300.0,
                height: 200.0,
            },
        };
        let small = NSSize {
            width: 100.0,
            height: 50.0,
        };
        let placed = image_placement(bounds, small, PROPORTIONALLY_DOWN, 0);
        assert_eq!((placed.origin.x, placed.origin.y), (100.0, 75.0));
        assert_eq!(placed.size.width, 100.0);
        let large = NSSize {
            width: 600.0,
            height: 200.0,
        };
        let placed = image_placement(bounds, large, PROPORTIONALLY_DOWN, 0);
        assert_eq!((placed.size.width, placed.size.height), (300.0, 100.0));
        assert_eq!(placed.origin.y, 50.0);
    }

    fn scroller(frame: NSRect) -> Id {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSScroller").expect("NSScroller");
        let object = objc::object::create_instance(class, 0);
        // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`.
        unsafe { scroller_init_with_frame(object, objc::sel::get("initWithFrame:"), frame) }
    }

    /// A scroller keeps the length it was given and takes the breadth AppKit
    /// gives it, in whichever direction the frame said it runs.
    #[test]
    fn a_scroller_is_constrained_to_one_breadth() {
        let rect = |width: f64, height: f64| NSRect {
            origin: NSPoint { x: 4.0, y: 7.0 },
            size: NSSize { width, height },
        };
        let vertical = scroller(rect(40.0, 300.0));
        assert!(!vertical.is_null());
        assert_eq!(
            view::frame_of(vertical),
            rect(SCROLLER_WIDTH, 300.0),
            "a tall frame is a vertical scroller, narrowed"
        );
        let horizontal = scroller(rect(300.0, 40.0));
        assert_eq!(
            view::frame_of(horizontal),
            rect(300.0, SCROLLER_WIDTH),
            "a wide frame is a horizontal scroller, flattened"
        );
        // A square is vertical, as it is on a Mac: horizontal needs width
        // *greater* than height.
        assert_eq!(
            view::frame_of(scroller(rect(40.0, 40.0))).size.width,
            SCROLLER_WIDTH
        );
        objc::arc::release(vertical);
        objc::arc::release(horizontal);
    }

    /// A scroller is a control, and a view - which is what lets a program put
    /// one in its subview tree at all.
    #[test]
    fn a_scroller_is_a_control() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSScroller").expect("NSScroller");
        for name in [c"NSControl", c"NSView", c"NSResponder"] {
            let ancestor = objc::class::lookup(name).expect("an ancestor");
            assert!(
                class.chain().any(|c| std::ptr::eq(c, ancestor)),
                "a scroller is not a {name:?}"
            );
        }
    }

    /// The breadth a program lays out against is the breadth it gets.
    #[test]
    fn the_published_width_is_the_constraint() {
        crate::bootstrap();
        // SAFETY: both are class methods of this module's.
        unsafe {
            let sel = objc::sel::get("scrollerWidth");
            assert_eq!(scroller_width(std::ptr::null_mut(), sel), SCROLLER_WIDTH);
            let sel = objc::sel::get("scrollerWidthForControlSize:");
            assert_eq!(
                scroller_width_for_control_size(std::ptr::null_mut(), sel, 0),
                SCROLLER_WIDTH
            );
            assert_eq!(
                scroller_width_for_control_size(std::ptr::null_mut(), sel, 1),
                SMALL_SCROLLER_WIDTH
            );
        }
    }

    /// The knob takes its proportion of the slot and slides along the rest;
    /// the page parts are what lies either side of it.
    #[test]
    fn a_scroller_places_its_knob_by_value_and_proportion() {
        let size = NSSize {
            width: SCROLLER_WIDTH,
            height: 200.0,
        };
        let knob = scroller_part(size, 0.5, 0.25, part::KNOB);
        assert_eq!((knob.origin.y, knob.size.height), (75.0, 50.0));
        assert_eq!(knob.size.width, SCROLLER_WIDTH);
        let before = scroller_part(size, 0.5, 0.25, part::DECREMENT_PAGE);
        assert_eq!((before.origin.y, before.size.height), (0.0, 75.0));
        let after = scroller_part(size, 0.5, 0.25, part::INCREMENT_PAGE);
        assert_eq!((after.origin.y, after.size.height), (125.0, 75.0));
        let slot = scroller_part(size, 0.5, 0.25, part::KNOB_SLOT);
        assert_eq!(slot.size, size);
        // A tiny proportion still leaves a knob to grab.
        let small = scroller_part(size, 1.0, 0.001, part::KNOB);
        assert_eq!(
            (small.origin.y, small.size.height),
            (182.0, MIN_KNOB_LENGTH)
        );
        // Nothing to scroll: no knob.
        assert_eq!(scroller_part(size, 0.0, 1.0, part::KNOB), NSRect::default());
    }

    #[test]
    fn a_scroller_clamps_what_it_is_given() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSScroller").expect("NSScroller");
        let object = objc::object::create_instance(class, 0);
        // SAFETY: the receiver is a live scroller.
        unsafe {
            scroller_set_float_value(object, objc::sel::get("setFloatValue:"), 1.5);
            assert_eq!(
                scroller_double_value(object, objc::sel::get("doubleValue")),
                1.0
            );
            scroller_set_knob_proportion(object, objc::sel::get("setKnobProportion:"), 0.3);
            assert_eq!(
                scroller_knob_proportion(object, objc::sel::get("knobProportion")),
                0.3
            );
        }
        forget(object);
        objc::arc::release(object);
    }
}
