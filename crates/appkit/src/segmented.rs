//! `NSSegmentedControl`: a row of segments, each with a label and a tag, of
//! which one (or any, or none) is selected.
//!
//! FIXME (ACCURACY): one look for every `NSSegmentStyle`, drawn like the
//! small-square style: a light bezel, with the selected segments darker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use foundation::geometry::{NSPoint, NSRect};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;
use crate::view;

#[derive(Clone, Default)]
struct Segment {
    label: String,
    tag: isize,
    /// Zero for "as wide as the room left allows".
    width: f64,
    selected: bool,
    disabled: bool,
}

#[derive(Clone, Default)]
struct Control {
    segments: Vec<Segment>,
    /// `NSSegmentSwitchTracking`: 0 selects one, 1 any, 2 is momentary.
    tracking: usize,
    /// Retained.
    font: usize,
}

const SELECT_ANY: usize = 1;
const MOMENTARY: usize = 2;

fn controls() -> &'static Mutex<HashMap<usize, Control>> {
    static CONTROLS: OnceLock<Mutex<HashMap<usize, Control>>> = OnceLock::new();
    CONTROLS.get_or_init(Default::default)
}

fn read(receiver: Id) -> Control {
    controls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or_default()
}

fn update<R>(receiver: Id, change: impl FnOnce(&mut Control) -> R) -> R {
    let result = change(
        controls()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(receiver as usize)
            .or_default(),
    );
    crate::backing::needs_display(receiver);
    result
}

/// Change segment `at`, growing the list to reach it as AppKit does not; a
/// segment past the count is ignored.
fn update_segment(receiver: Id, at: isize, change: impl FnOnce(&mut Segment, &mut Vec<Segment>)) {
    update(receiver, |control| {
        let Ok(at) = usize::try_from(at) else {
            return;
        };
        if at >= control.segments.len() {
            return;
        }
        let mut segment = control.segments[at].clone();
        change(&mut segment, &mut control.segments);
        control.segments[at] = segment;
    });
}

/// Drop the state of a control that is going away.
pub(crate) fn forget(receiver: Id) {
    let gone = controls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    if let Some(control) = gone {
        objc::arc::release(control.font as Id);
    }
}

fn text_of(string: Id) -> String {
    // SAFETY: a string, or nil.
    String::from_utf8_lossy(&unsafe { foundation::string::bytes(string) }).into_owned()
}

unsafe extern "C" fn set_segment_count(receiver: Id, _sel: Sel, count: isize) {
    update(receiver, |control| {
        control
            .segments
            .resize(count.max(0) as usize, Segment::default())
    });
}

unsafe extern "C" fn segment_count(receiver: Id, _sel: Sel) -> isize {
    read(receiver).segments.len() as isize
}

unsafe extern "C" fn set_label(receiver: Id, _sel: Sel, label: Id, at: isize) {
    let label = text_of(label);
    update_segment(receiver, at, |segment, _| segment.label = label);
}

unsafe extern "C" fn label(receiver: Id, _sel: Sel, at: isize) -> Id {
    let control = read(receiver);
    match usize::try_from(at)
        .ok()
        .and_then(|at| control.segments.get(at))
    {
        Some(segment) => objc::arc::autorelease(foundation::string::from_str(&segment.label)),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn set_tag(receiver: Id, _sel: Sel, tag: isize, at: isize) {
    update_segment(receiver, at, |segment, _| segment.tag = tag);
}

unsafe extern "C" fn tag(receiver: Id, _sel: Sel, at: isize) -> isize {
    let control = read(receiver);
    usize::try_from(at)
        .ok()
        .and_then(|at| control.segments.get(at))
        .map_or(0, |segment| segment.tag)
}

unsafe extern "C" fn set_width(receiver: Id, _sel: Sel, width: f64, at: isize) {
    update_segment(receiver, at, |segment, _| segment.width = width);
}

unsafe extern "C" fn width(receiver: Id, _sel: Sel, at: isize) -> f64 {
    let control = read(receiver);
    usize::try_from(at)
        .ok()
        .and_then(|at| control.segments.get(at))
        .map_or(0.0, |segment| segment.width)
}

/// `-setSelected:forSegment:`: under select-one tracking, selecting a
/// segment deselects the rest.
unsafe extern "C" fn set_selected(receiver: Id, _sel: Sel, on: u8, at: isize) {
    let tracking = read(receiver).tracking;
    update_segment(receiver, at, |segment, all| {
        if on != 0 && tracking != SELECT_ANY {
            all.iter_mut().for_each(|other| other.selected = false);
        }
        segment.selected = on != 0;
    });
}

unsafe extern "C" fn is_selected(receiver: Id, _sel: Sel, at: isize) -> u8 {
    let control = read(receiver);
    u8::from(
        usize::try_from(at)
            .ok()
            .and_then(|at| control.segments.get(at))
            .is_some_and(|segment| segment.selected),
    )
}

unsafe extern "C" fn set_enabled_for_segment(receiver: Id, _sel: Sel, on: u8, at: isize) {
    update_segment(receiver, at, |segment, _| segment.disabled = on == 0);
}

unsafe extern "C" fn is_enabled_for_segment(receiver: Id, _sel: Sel, at: isize) -> u8 {
    let control = read(receiver);
    u8::from(
        usize::try_from(at)
            .ok()
            .and_then(|at| control.segments.get(at))
            .is_some_and(|segment| !segment.disabled),
    )
}

/// `-selectedSegment`: the last selected, or -1 for none.
unsafe extern "C" fn selected_segment(receiver: Id, _sel: Sel) -> isize {
    read(receiver)
        .segments
        .iter()
        .rposition(|segment| segment.selected)
        .map_or(-1, |at| at as isize)
}

unsafe extern "C" fn set_selected_segment(receiver: Id, sel: Sel, at: isize) {
    if at < 0 {
        update(receiver, |control| {
            control
                .segments
                .iter_mut()
                .for_each(|segment| segment.selected = false)
        });
        return;
    }
    // SAFETY: the same control.
    unsafe { set_selected(receiver, sel, 1, at) };
}

unsafe extern "C" fn select_segment_with_tag(receiver: Id, sel: Sel, tag: isize) -> u8 {
    let Some(at) = read(receiver)
        .segments
        .iter()
        .position(|segment| segment.tag == tag)
    else {
        return 0;
    };
    // SAFETY: the same control.
    unsafe { set_selected(receiver, sel, 1, at as isize) };
    1
}

unsafe extern "C" fn set_tracking_mode(receiver: Id, _sel: Sel, mode: usize) {
    update(receiver, |control| control.tracking = mode);
}

unsafe extern "C" fn tracking_mode(receiver: Id, _sel: Sel) -> usize {
    read(receiver).tracking
}

unsafe extern "C" fn set_font(receiver: Id, _sel: Sel, font: Id) {
    let font = objc::arc::retain(font) as usize;
    let old = update(receiver, |control| {
        std::mem::replace(&mut control.font, font)
    });
    objc::arc::release(old as Id);
}

unsafe extern "C" fn font(receiver: Id, _sel: Sel) -> Id {
    read(receiver).font as Id
}

unsafe extern "C" fn ignored_integer(_receiver: Id, _sel: Sel, _value: isize) {}

/// Each segment's left edge and width across `total` points: fixed widths as
/// given, and the rest sharing what remains.
fn layout(control: &Control, total: f64) -> Vec<(f64, f64)> {
    let fixed: f64 = control.segments.iter().map(|segment| segment.width).sum();
    let flexible = control
        .segments
        .iter()
        .filter(|segment| segment.width <= 0.0)
        .count();
    let share = match flexible {
        0 => 0.0,
        n => ((total - fixed) / n as f64).max(0.0),
    };
    let mut x = 0.0;
    control
        .segments
        .iter()
        .map(|segment| {
            let width = if segment.width > 0.0 {
                segment.width
            } else {
                share
            };
            let at = (x, width);
            x += width;
            at
        })
        .collect()
}

/// `-mouseDown:`: the segment under the pointer is chosen and the action sent.
unsafe extern "C" fn mouse_down(receiver: Id, _sel: Sel, event: Id) {
    if !crate::controls::is_enabled(receiver) {
        return;
    }
    // SAFETY: `-locationInWindow` answers an `NSPoint`, and
    // `-convertPoint:fromView:` is the view's own.
    let point = unsafe {
        let location: unsafe extern "C" fn(Id, Sel) -> NSPoint =
            std::mem::transmute(objc::msg::entry());
        let convert: unsafe extern "C" fn(Id, Sel, NSPoint, Id) -> NSPoint =
            std::mem::transmute(objc::msg::entry());
        convert(
            receiver,
            objc::sel::get("convertPoint:fromView:"),
            location(event, objc::sel::get("locationInWindow")),
            std::ptr::null_mut(),
        )
    };
    let control = read(receiver);
    let bounds = view::bounds_of(receiver);
    let Some(at) = layout(&control, bounds.size.width)
        .iter()
        .position(|(x, width)| point.x >= *x && point.x < x + width)
    else {
        return;
    };
    if control.segments[at].disabled {
        return;
    }
    let sel = objc::sel::get("setSelected:forSegment:");
    match control.tracking {
        // SAFETY: the same control, at a segment it has.
        SELECT_ANY => unsafe {
            set_selected(
                receiver,
                sel,
                u8::from(!control.segments[at].selected),
                at as isize,
            )
        },
        // SAFETY: as above.
        _ => unsafe { set_selected(receiver, sel, 1, at as isize) },
    }
    crate::controls::send_action(receiver);
    if control.tracking == MOMENTARY {
        update(receiver, |control| {
            control.segments.iter_mut().for_each(|s| s.selected = false)
        });
    }
}

unsafe extern "C" fn draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    let bounds = view::bounds_of(receiver);
    let density = crate::backing::focus_density();
    let pixel_width = (bounds.size.width * density).ceil() as usize;
    let pixel_height = (bounds.size.height * density).ceil() as usize;
    if pixel_width == 0 || pixel_height == 0 {
        return;
    }
    let control = read(receiver);
    let mut pixels = vec![0u8; pixel_width * pixel_height * 4];
    let mut canvas = chrome::Canvas::new(
        &mut pixels,
        pixel_width,
        pixel_height,
        pixel_width * 4,
        density as f32,
    );
    let (w, h) = (bounds.size.width as f32, bounds.size.height as f32);
    let edge = chrome::Color::rgba(0, 0, 0, 0.35);
    canvas.fill_rounded_rect(
        chrome::Rect::new(0.5, 0.5, w - 1.0, h - 1.0),
        3.0,
        chrome::Color::rgba(245, 245, 245, 1.0),
    );
    let enabled = crate::controls::is_enabled(receiver);
    let font: Option<(Arc<chrome::Font>, f64)> = crate::label::chrome_font(control.font as Id);
    for (segment, (x, width)) in control
        .segments
        .iter()
        .zip(layout(&control, bounds.size.width))
    {
        let (x, width) = (x as f32, width as f32);
        if segment.selected {
            canvas.fill_rect(
                chrome::Rect::new(x + 1.0, 1.0, (width - 2.0).max(0.0), h - 2.0),
                chrome::Color::rgba(160, 160, 160, 1.0),
            );
        }
        if x > 0.0 {
            canvas.fill_rect(chrome::Rect::new(x, 1.0, 1.0, h - 2.0), edge);
        }
        if let Some((font, size)) = &font {
            let size = *size as f32;
            let line = font.truncate(&segment.label, size, (width - 4.0).max(0.0));
            let advance = font.measure(&line, size);
            let baseline = font.centered_baseline(size, 0.0, h);
            let alpha = if enabled && !segment.disabled {
                0.85
            } else {
                0.35
            };
            canvas.draw_text(
                &line,
                font,
                size,
                chrome::Point::new(x + (width - advance) / 2.0, baseline),
                chrome::Color::rgba(0, 0, 0, alpha),
            );
        }
    }
    canvas.stroke_rounded_rect(
        chrome::Rect::new(0.5, 0.5, w - 1.0, h - 1.0),
        3.0,
        1.0,
        edge,
    );
    crate::backing::draw_image(
        &pixels,
        pixel_width,
        pixel_height,
        bounds.size,
        bounds,
        NSRect {
            origin: NSPoint::default(),
            size: bounds.size,
        },
        // Source-over.
        2,
        1.0,
        true,
    );
}

/// What a nib's `NSSegmentItem` holds, for [`crate::decode`].
pub(crate) struct Archived {
    pub label: String,
    pub tag: isize,
    pub width: f64,
    pub selected: bool,
    pub disabled: bool,
}

/// Give a control decoded from a nib its segments.
pub(crate) fn set_archived(receiver: Id, segments: Vec<Archived>) {
    update(receiver, |control| {
        control.segments = segments
            .into_iter()
            .map(|s| Segment {
                label: s.label,
                tag: s.tag,
                width: s.width,
                selected: s.selected,
                disabled: s.disabled,
            })
            .collect();
    });
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSSegmentedControl").expect("declared in crate::classes");
    methods!(class, {
        "setSegmentCount:" => set_segment_count, c"v24@0:8q16";
        "segmentCount" => segment_count, c"q16@0:8";
        "setLabel:forSegment:" => set_label, c"v32@0:8@16q24";
        "labelForSegment:" => label, c"@24@0:8q16";
        "setTag:forSegment:" => set_tag, c"v32@0:8q16q24";
        "tagForSegment:" => tag, c"q24@0:8q16";
        "setWidth:forSegment:" => set_width, c"v32@0:8d16q24";
        "widthForSegment:" => width, c"d24@0:8q16";
        "setSelected:forSegment:" => set_selected, c"v28@0:8c16q20";
        "isSelectedForSegment:" => is_selected, c"c24@0:8q16";
        "setEnabled:forSegment:" => set_enabled_for_segment, c"v28@0:8c16q20";
        "isEnabledForSegment:" => is_enabled_for_segment, c"c24@0:8q16";
        "selectedSegment" => selected_segment, c"q16@0:8";
        "setSelectedSegment:" => set_selected_segment, c"v24@0:8q16";
        "selectSegmentWithTag:" => select_segment_with_tag, c"c24@0:8q16";
        "setTrackingMode:" => set_tracking_mode, c"v24@0:8Q16";
        "trackingMode" => tracking_mode, c"Q16@0:8";
        "setSegmentStyle:" => ignored_integer, c"v24@0:8q16";
        "setFont:" => set_font, c"v24@0:8@16";
        "font" => font, c"@16@0:8";
        "mouseDown:" => mouse_down, c"v24@0:8@16";
        "drawRect:" => draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });
}
