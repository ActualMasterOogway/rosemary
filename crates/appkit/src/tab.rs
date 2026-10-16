//! `NSTabView` and `NSTabViewItem`: a row of tabs over a box, showing the
//! chosen item's view in the box.
//!
//! FIXME (ACCURACY): every `NSTabViewType` with tabs is drawn with them on top,
//! as a strip of segments centred on the box's upper edge.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use foundation::geometry::{NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;
use crate::view;

#[repr(C)]
struct Item {
    isa: *const c_void,
    /// Retained, like `view` and `identifier`.
    label: Id,
    view: Id,
    identifier: Id,
    /// Not retained: the tab view holds its items.
    tab_view: Id,
}

pub const ITEM_INSTANCE_SIZE: usize = size_of::<Item>();

fn item_of<'a>(item: Id) -> &'a mut Item {
    // SAFETY: an item of this crate's class, which is what every method here
    // is sent to.
    unsafe { &mut *(item as *mut Item) }
}

/// `NSTabViewType`: the kinds from 4 on have no tabs, and 6 not even a box.
const NO_TABS_BEZEL_BORDER: usize = 4;
const NO_TABS_NO_BORDER: usize = 6;

/// The strip's height, and how far its top sits below the view's.
const STRIP_HEIGHT: f64 = 22.0;
const STRIP_TOP: f64 = 6.0;
const TAB_PADDING: f64 = 12.0;

#[derive(Clone, Default)]
struct Tabs {
    /// Retained.
    items: Vec<usize>,
    selected: Option<usize>,
    /// Not retained, as a delegate never is.
    delegate: usize,
    /// Retained.
    font: usize,
    kind: usize,
}

fn tab_views() -> &'static Mutex<HashMap<usize, Tabs>> {
    static TABS: OnceLock<Mutex<HashMap<usize, Tabs>>> = OnceLock::new();
    TABS.get_or_init(Default::default)
}

fn read(receiver: Id) -> Tabs {
    tab_views()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or_default()
}

fn update<R>(receiver: Id, change: impl FnOnce(&mut Tabs) -> R) -> R {
    change(
        tab_views()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(receiver as usize)
            .or_default(),
    )
}

/// Drop the items of a tab view that is going away.
pub(crate) fn forget(receiver: Id) {
    let gone = tab_views()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    if let Some(tabs) = gone {
        for item in tabs.items {
            item_of(item as Id).tab_view = std::ptr::null_mut();
            objc::arc::release(item as Id);
        }
        objc::arc::release(tabs.font as Id);
    }
}

fn send_view(receiver: Id, selector: &str, argument: Id) {
    // SAFETY: each selector this module sends takes one object.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), argument);
    }
}

fn text_of(string: Id) -> String {
    // SAFETY: a string, or nil.
    String::from_utf8_lossy(&unsafe { foundation::string::bytes(string) }).into_owned()
}

fn responds(object: Id, selector: &str) -> bool {
    // SAFETY: an object, or nil.
    !object.is_null()
        && unsafe { objc::object::class_of(object) }
            .is_some_and(|class| class.method(objc::sel::get(selector)).is_some())
}

/// The box the items' views are shown in, inside the border.
fn content_rect(receiver: Id, kind: usize) -> NSRect {
    let bounds = view::bounds_of(receiver);
    let (top, side, bottom) = match kind {
        NO_TABS_NO_BORDER => (0.0, 0.0, 0.0),
        k if k >= NO_TABS_BEZEL_BORDER => (10.0, 10.0, 13.0),
        _ => (33.0, 10.0, 13.0),
    };
    NSRect {
        origin: NSPoint {
            x: bounds.origin.x + side,
            y: bounds.origin.y + top,
        },
        size: NSSize {
            width: (bounds.size.width - 2.0 * side).max(0.0),
            height: (bounds.size.height - top - bottom).max(0.0),
        },
    }
}

fn font_of(tabs: &Tabs) -> Option<(Arc<chrome::Font>, f64)> {
    crate::label::chrome_font(tabs.font as Id)
}

/// Each tab's left edge and width, centred across the view.
fn tab_layout(receiver: Id, tabs: &Tabs) -> Vec<(f64, f64)> {
    let font = font_of(tabs);
    let widths: Vec<f64> = tabs
        .items
        .iter()
        .map(|item| {
            let label = text_of(item_of(*item as Id).label);
            let text = font
                .as_ref()
                .map_or(0.0, |(f, size)| f64::from(f.measure(&label, *size as f32)));
            (text + 2.0 * TAB_PADDING).ceil()
        })
        .collect();
    let total: f64 = widths.iter().sum();
    let mut x = ((view::bounds_of(receiver).size.width - total) / 2.0).floor();
    widths
        .into_iter()
        .map(|width| {
            let at = (x, width);
            x += width;
            at
        })
        .collect()
}

/// Show item `at`, asking the delegate first and telling it after.
fn select(receiver: Id, at: usize) {
    let tabs = read(receiver);
    let Some(&item) = tabs.items.get(at) else {
        return;
    };
    let item = item as Id;
    let delegate = tabs.delegate as Id;
    let ask = |selector: &str| -> bool {
        if !responds(delegate, selector) {
            return true;
        }
        // SAFETY: the delegate methods are `BOOL (id, SEL, id, id)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> u8 =
                std::mem::transmute(objc::msg::entry());
            f(delegate, objc::sel::get(selector), receiver, item) != 0
        }
    };
    let tell = |selector: &str| {
        if responds(delegate, selector) {
            // SAFETY: the delegate methods are `void (id, SEL, id, id)`.
            unsafe {
                let f: unsafe extern "C" fn(Id, Sel, Id, Id) =
                    std::mem::transmute(objc::msg::entry());
                f(delegate, objc::sel::get(selector), receiver, item);
            }
        }
    };
    if tabs.selected == Some(at) || !ask("tabView:shouldSelectTabViewItem:") {
        return;
    }
    tell("tabView:willSelectTabViewItem:");
    if let Some(old) = tabs.selected.and_then(|old| tabs.items.get(old)) {
        let old = item_of(*old as Id).view;
        if !old.is_null() {
            // SAFETY: `-removeFromSuperview` is `void (id, SEL)`.
            unsafe {
                let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
                f(old, objc::sel::get("removeFromSuperview"));
            }
        }
    }
    update(receiver, |t| t.selected = Some(at));
    show_selected(receiver);
    tell("tabView:didSelectTabViewItem:");
}

/// Put the selected item's view in the box.
fn show_selected(receiver: Id) {
    let tabs = read(receiver);
    if let Some(item) = tabs.selected.and_then(|at| tabs.items.get(at)) {
        let content = item_of(*item as Id).view;
        if !content.is_null() {
            // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
            unsafe {
                let f: unsafe extern "C" fn(Id, Sel, NSRect) =
                    std::mem::transmute(objc::msg::entry());
                f(
                    content,
                    objc::sel::get("setFrame:"),
                    content_rect(receiver, tabs.kind),
                );
            }
            // SAFETY: a view of this crate's.
            if unsafe { view::superview_of(content) } != receiver {
                send_view(receiver, "addSubview:", content);
            }
        }
    }
    crate::backing::needs_display(receiver);
}

fn index_of(receiver: Id, item: Id) -> Option<usize> {
    read(receiver)
        .items
        .iter()
        .position(|i| *i == item as usize)
}

unsafe extern "C" fn is_flipped(_receiver: Id, _sel: Sel) -> u8 {
    1
}

unsafe extern "C" fn insert_item(receiver: Id, _sel: Sel, item: Id, at: isize) {
    if item.is_null() {
        return;
    }
    objc::arc::retain(item);
    item_of(item).tab_view = receiver;
    let first = update(receiver, |t| {
        let at = usize::try_from(at).unwrap_or(0).min(t.items.len());
        t.items.insert(at, item as usize);
        if let Some(selected) = t.selected.as_mut()
            && *selected >= at
        {
            *selected += 1;
        }
        t.items.len() == 1
    });
    // The first item added is shown, as AppKit shows it.
    if first {
        select(receiver, 0);
    }
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn add_item(receiver: Id, sel: Sel, item: Id) {
    let count = read(receiver).items.len() as isize;
    // SAFETY: the same tab view.
    unsafe { insert_item(receiver, sel, item, count) };
}

unsafe extern "C" fn remove_item(receiver: Id, _sel: Sel, item: Id) {
    let Some(at) = index_of(receiver, item) else {
        return;
    };
    let was_selected = read(receiver).selected == Some(at);
    if was_selected && !item_of(item).view.is_null() {
        // SAFETY: `-removeFromSuperview` is `void (id, SEL)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
            f(item_of(item).view, objc::sel::get("removeFromSuperview"));
        }
    }
    let remaining = update(receiver, |t| {
        t.items.remove(at);
        t.selected = match t.selected {
            Some(s) if s == at => None,
            Some(s) if s > at => Some(s - 1),
            other => other,
        };
        t.items.len()
    });
    item_of(item).tab_view = std::ptr::null_mut();
    objc::arc::release(item);
    if was_selected && remaining > 0 {
        select(receiver, at.min(remaining - 1));
    }
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn items(receiver: Id, _sel: Sel) -> Id {
    let values: Vec<*const c_void> = read(receiver)
        .items
        .iter()
        .map(|i| *i as *const _)
        .collect();
    let array = corefoundation::collections::new_array(
        &values,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn number_of_items(receiver: Id, _sel: Sel) -> isize {
    read(receiver).items.len() as isize
}

unsafe extern "C" fn item_at(receiver: Id, _sel: Sel, at: isize) -> Id {
    usize::try_from(at)
        .ok()
        .and_then(|at| read(receiver).items.get(at).copied())
        .unwrap_or(0) as Id
}

/// `NSNotFound` for an item the tab view does not hold.
unsafe extern "C" fn index_of_item(receiver: Id, _sel: Sel, item: Id) -> isize {
    index_of(receiver, item).map_or(isize::MAX, |at| at as isize)
}

fn index_with_identifier(receiver: Id, identifier: Id) -> Option<usize> {
    read(receiver).items.iter().position(|item| {
        let own = item_of(*item as Id).identifier;
        own == identifier
            || (!own.is_null()
                && !identifier.is_null()
                // SAFETY: `-isEqual:` is `BOOL (id, SEL, id)` on every object.
                && unsafe {
                    let f: unsafe extern "C" fn(Id, Sel, Id) -> u8 =
                        std::mem::transmute(objc::msg::entry());
                    f(own, objc::sel::get("isEqual:"), identifier) != 0
                })
    })
}

unsafe extern "C" fn index_of_item_with_identifier(
    receiver: Id,
    _sel: Sel,
    identifier: Id,
) -> isize {
    index_with_identifier(receiver, identifier).map_or(isize::MAX, |at| at as isize)
}

unsafe extern "C" fn selected_item(receiver: Id, _sel: Sel) -> Id {
    let tabs = read(receiver);
    tabs.selected
        .and_then(|at| tabs.items.get(at).copied())
        .unwrap_or(0) as Id
}

unsafe extern "C" fn select_item(receiver: Id, _sel: Sel, item: Id) {
    if let Some(at) = index_of(receiver, item) {
        select(receiver, at);
    }
}

unsafe extern "C" fn select_item_at(receiver: Id, _sel: Sel, at: isize) {
    if let Ok(at) = usize::try_from(at) {
        select(receiver, at);
    }
}

unsafe extern "C" fn select_item_with_identifier(receiver: Id, _sel: Sel, identifier: Id) {
    if let Some(at) = index_with_identifier(receiver, identifier) {
        select(receiver, at);
    }
}

unsafe extern "C" fn select_first(receiver: Id, _sel: Sel, _sender: Id) {
    select(receiver, 0);
}

unsafe extern "C" fn select_last(receiver: Id, _sel: Sel, _sender: Id) {
    let count = read(receiver).items.len();
    if count > 0 {
        select(receiver, count - 1);
    }
}

unsafe extern "C" fn select_next(receiver: Id, _sel: Sel, _sender: Id) {
    let tabs = read(receiver);
    if let Some(at) = tabs.selected.filter(|at| at + 1 < tabs.items.len()) {
        select(receiver, at + 1);
    }
}

unsafe extern "C" fn select_previous(receiver: Id, _sel: Sel, _sender: Id) {
    if let Some(at) = read(receiver).selected.filter(|at| *at > 0) {
        select(receiver, at - 1);
    }
}

unsafe extern "C" fn set_delegate(receiver: Id, _sel: Sel, delegate: Id) {
    update(receiver, |t| t.delegate = delegate as usize);
}

unsafe extern "C" fn delegate(receiver: Id, _sel: Sel) -> Id {
    read(receiver).delegate as Id
}

unsafe extern "C" fn set_font(receiver: Id, _sel: Sel, font: Id) {
    let font = objc::arc::retain(font) as usize;
    let old = update(receiver, |t| std::mem::replace(&mut t.font, font));
    objc::arc::release(old as Id);
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn font(receiver: Id, _sel: Sel) -> Id {
    read(receiver).font as Id
}

unsafe extern "C" fn set_kind(receiver: Id, _sel: Sel, kind: usize) {
    update(receiver, |t| t.kind = kind);
    show_selected(receiver);
}

unsafe extern "C" fn kind(receiver: Id, _sel: Sel) -> usize {
    read(receiver).kind
}

unsafe extern "C" fn get_content_rect(receiver: Id, _sel: Sel) -> NSRect {
    content_rect(receiver, read(receiver).kind)
}

unsafe extern "C" fn mouse_down(receiver: Id, _sel: Sel, event: Id) {
    let tabs = read(receiver);
    if tabs.kind >= NO_TABS_BEZEL_BORDER {
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
    if point.y < STRIP_TOP || point.y >= STRIP_TOP + STRIP_HEIGHT {
        return;
    }
    if let Some(at) = tab_layout(receiver, &tabs)
        .iter()
        .position(|(x, width)| point.x >= *x && point.x < x + width)
    {
        select(receiver, at);
    }
}

unsafe extern "C" fn draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    let tabs = read(receiver);
    if tabs.kind == NO_TABS_NO_BORDER {
        return;
    }
    let bounds = view::bounds_of(receiver);
    let density = crate::backing::focus_density();
    let pixel_width = (bounds.size.width * density).ceil() as usize;
    let pixel_height = (bounds.size.height * density).ceil() as usize;
    if pixel_width == 0 || pixel_height == 0 {
        return;
    }
    let mut pixels = vec![0u8; pixel_width * pixel_height * 4];
    let mut canvas = chrome::Canvas::new(
        &mut pixels,
        pixel_width,
        pixel_height,
        pixel_width * 4,
        density as f32,
    );
    let edge = chrome::Color::rgba(0, 0, 0, 0.2);
    let content = content_rect(receiver, tabs.kind);
    let has_tabs = tabs.kind < NO_TABS_BEZEL_BORDER;
    // The box sits around the content, its top under the middle of the strip.
    let top = match has_tabs {
        true => STRIP_TOP + STRIP_HEIGHT / 2.0,
        false => content.origin.y - 3.0,
    } as f32;
    let left = (content.origin.x - 3.0) as f32;
    let right = (content.origin.x + content.size.width + 3.0) as f32;
    let bottom = (content.origin.y + content.size.height + 3.0) as f32;
    let frame = chrome::Rect::new(
        left + 0.5,
        top + 0.5,
        right - left - 1.0,
        bottom - top - 1.0,
    );
    canvas.fill_rounded_rect(frame, 4.0, chrome::Color::rgba(0, 0, 0, 0.04));
    canvas.stroke_rounded_rect(frame, 4.0, 1.0, edge);
    if has_tabs {
        let font = font_of(&tabs);
        let layout = tab_layout(receiver, &tabs);
        if let (Some((first, _)), Some((last, width))) = (layout.first(), layout.last()) {
            let strip = chrome::Rect::new(
                *first as f32 + 0.5,
                STRIP_TOP as f32 + 0.5,
                (last + width - first) as f32 - 1.0,
                STRIP_HEIGHT as f32 - 1.0,
            );
            canvas.fill_rounded_rect(strip, 4.0, chrome::Color::rgba(250, 250, 250, 1.0));
            for (at, (item, (x, width))) in tabs.items.iter().zip(&layout).enumerate() {
                let (x, width) = (*x as f32, *width as f32);
                if tabs.selected == Some(at) {
                    canvas.fill_rect(
                        chrome::Rect::new(
                            x + 1.0,
                            STRIP_TOP as f32 + 1.0,
                            width - 2.0,
                            STRIP_HEIGHT as f32 - 2.0,
                        ),
                        chrome::Color::rgba(0, 122, 255, 1.0),
                    );
                }
                if at > 0 {
                    canvas.fill_rect(
                        chrome::Rect::new(
                            x,
                            STRIP_TOP as f32 + 1.0,
                            1.0,
                            STRIP_HEIGHT as f32 - 2.0,
                        ),
                        edge,
                    );
                }
                if let Some((font, size)) = &font {
                    let size = *size as f32;
                    let label = text_of(item_of(*item as Id).label);
                    let line = font.truncate(&label, size, (width - 4.0).max(0.0));
                    let advance = font.measure(&line, size);
                    let baseline =
                        font.centered_baseline(size, STRIP_TOP as f32, STRIP_HEIGHT as f32);
                    let color = match tabs.selected == Some(at) {
                        true => chrome::Color::rgba(255, 255, 255, 1.0),
                        false => chrome::Color::rgba(0, 0, 0, 0.85),
                    };
                    canvas.draw_text(
                        &line,
                        font,
                        size,
                        chrome::Point::new(x + (width - advance) / 2.0, baseline),
                        color,
                    );
                }
            }
            canvas.stroke_rounded_rect(strip, 4.0, 1.0, chrome::Color::rgba(0, 0, 0, 0.3));
        }
    }
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

/// Give a tab view decoded from a nib its items, and show the chosen one.
pub(crate) fn set_archived(receiver: Id, items: Vec<Id>, selected: Option<usize>, kind: usize) {
    // A view the nib filed among the tab view's subviews belongs to an item,
    // and is shown again below only if its item is the one chosen.
    for item in &items {
        let content = item_of(*item).view;
        // SAFETY: a view of this crate's, or nil.
        if !content.is_null() && unsafe { view::superview_of(content) } == receiver {
            // SAFETY: `-removeFromSuperview` is `void (id, SEL)`.
            unsafe {
                let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
                f(content, objc::sel::get("removeFromSuperview"));
            }
        }
        objc::arc::retain(*item);
        item_of(*item).tab_view = receiver;
    }
    let count = items.len();
    update(receiver, |t| {
        t.items = items.iter().map(|i| *i as usize).collect();
        t.kind = kind;
        t.selected = selected
            .filter(|at| *at < count)
            .or((count > 0).then_some(0));
    });
    show_selected(receiver);
}

unsafe extern "C" fn item_init_with_identifier(receiver: Id, sel: Sel, identifier: Id) -> Id {
    // SAFETY: the same item.
    unsafe { item_set_identifier(receiver, sel, identifier) };
    receiver
}

macro_rules! item_object {
    ($($set:ident, $get:ident => $field:ident;)*) => {$(
        unsafe extern "C" fn $set(receiver: Id, _sel: Sel, value: Id) {
            let item = item_of(receiver);
            let old = std::mem::replace(&mut item.$field, objc::arc::retain(value));
            objc::arc::release(old);
            let tab_view = item.tab_view;
            if !tab_view.is_null() {
                crate::backing::needs_display(tab_view);
            }
        }

        unsafe extern "C" fn $get(receiver: Id, _sel: Sel) -> Id {
            item_of(receiver).$field
        }
    )*};
}

item_object! {
    item_set_label, item_label => label;
    item_set_view, item_view => view;
    item_set_identifier, item_identifier => identifier;
}

unsafe extern "C" fn item_tab_view(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).tab_view
}

unsafe extern "C" fn item_dealloc(receiver: Id, _sel: Sel) {
    let item = item_of(receiver);
    for field in [&mut item.label, &mut item.view, &mut item.identifier] {
        objc::arc::release(std::mem::replace(field, std::ptr::null_mut()));
    }
    objc::arc::finalize(receiver);
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSTabView").expect("declared in crate::classes");
    methods!(class, {
        "isFlipped" => is_flipped, c"c16@0:8";
        "addTabViewItem:" => add_item, c"v24@0:8@16";
        "insertTabViewItem:atIndex:" => insert_item, c"v32@0:8@16q24";
        "removeTabViewItem:" => remove_item, c"v24@0:8@16";
        "tabViewItems" => items, c"@16@0:8";
        "numberOfTabViewItems" => number_of_items, c"q16@0:8";
        "tabViewItemAtIndex:" => item_at, c"@24@0:8q16";
        "indexOfTabViewItem:" => index_of_item, c"q24@0:8@16";
        "indexOfTabViewItemWithIdentifier:" => index_of_item_with_identifier, c"q24@0:8@16";
        "selectedTabViewItem" => selected_item, c"@16@0:8";
        "selectTabViewItem:" => select_item, c"v24@0:8@16";
        "selectTabViewItemAtIndex:" => select_item_at, c"v24@0:8q16";
        "selectTabViewItemWithIdentifier:" => select_item_with_identifier, c"v24@0:8@16";
        "selectFirstTabViewItem:" => select_first, c"v24@0:8@16";
        "selectLastTabViewItem:" => select_last, c"v24@0:8@16";
        "selectNextTabViewItem:" => select_next, c"v24@0:8@16";
        "selectPreviousTabViewItem:" => select_previous, c"v24@0:8@16";
        "setDelegate:" => set_delegate, c"v24@0:8@16";
        "delegate" => delegate, c"@16@0:8";
        "setFont:" => set_font, c"v24@0:8@16";
        "font" => font, c"@16@0:8";
        "setTabViewType:" => set_kind, c"v24@0:8Q16";
        "tabViewType" => kind, c"Q16@0:8";
        "contentRect" => get_content_rect, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "mouseDown:" => mouse_down, c"v24@0:8@16";
        "drawRect:" => draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });
    let class = objc::class::lookup(c"NSTabViewItem").expect("declared in crate::classes");
    methods!(class, {
        "initWithIdentifier:" => item_init_with_identifier, c"@24@0:8@16";
        "setLabel:" => item_set_label, c"v24@0:8@16";
        "label" => item_label, c"@16@0:8";
        "setView:" => item_set_view, c"v24@0:8@16";
        "view" => item_view, c"@16@0:8";
        "setIdentifier:" => item_set_identifier, c"v24@0:8@16";
        "identifier" => item_identifier, c"@16@0:8";
        "tabView" => item_tab_view, c"@16@0:8";
        "dealloc" => item_dealloc, c"v16@0:8";
    });
}
