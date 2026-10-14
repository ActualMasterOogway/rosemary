//! `NSTableView` and `NSTableColumn`, as a read-only list a data source
//! fills: rows of each column's value, drawn in its font and alignment.
//!
//! FIXME (ACCURACY): selection is tracked but not drawn; no editing, headers or grid are drawn, and a
//! table that does not fit its clip view shows the rows `-scrollRowToVisible:`
//! last asked for rather than scrolling.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use foundation::geometry::{NSPoint, NSRect};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;
use crate::view;

#[derive(Clone, Default)]
struct Table {
    /// Unretained, as AppKit's are.
    data_source: usize,
    delegate: usize,
    /// Retained columns, in order.
    columns: Vec<usize>,
    row_height: f64,
    spacing_height: f64,
    /// Retained colour, or zero for white.
    background: usize,
    /// The row last asked to be visible.
    visible_row: Option<isize>,
    /// Selected rows, ascending.
    selected: Vec<isize>,
}

#[derive(Clone, Default)]
struct Column {
    /// Retained string, or zero.
    identifier: usize,
    width: f64,
    /// Retained font, or zero for the system font.
    font: usize,
    /// `NSTextAlignment`.
    alignment: isize,
}

fn tables() -> &'static Mutex<HashMap<usize, Table>> {
    static TABLES: OnceLock<Mutex<HashMap<usize, Table>>> = OnceLock::new();
    TABLES.get_or_init(Default::default)
}

fn columns() -> &'static Mutex<HashMap<usize, Column>> {
    static COLUMNS: OnceLock<Mutex<HashMap<usize, Column>>> = OnceLock::new();
    COLUMNS.get_or_init(Default::default)
}

fn table(receiver: Id) -> Table {
    tables()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or(Table {
            row_height: 17.0,
            spacing_height: 2.0,
            ..Default::default()
        })
}

fn update_table(receiver: Id, change: impl FnOnce(&mut Table)) {
    let current = table(receiver);
    let mut tables = tables().lock().unwrap_or_else(|e| e.into_inner());
    change(tables.entry(receiver as usize).or_insert(current));
    drop(tables);
    crate::backing::needs_display(receiver);
}

fn column(receiver: Id) -> Column {
    columns()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or(Column {
            width: 100.0,
            ..Default::default()
        })
}

fn update_column(receiver: Id, change: impl FnOnce(&mut Column)) {
    let current = column(receiver);
    let mut columns = columns().lock().unwrap_or_else(|e| e.into_inner());
    change(columns.entry(receiver as usize).or_insert(current));
}

/// Store a retained object in `slot`, releasing what it held.
fn swap_retained(slot: &mut usize, value: Id) -> Id {
    std::mem::replace(slot, objc::arc::retain(value) as usize) as Id
}

/// Drop the state of a table that is going away.
pub(crate) fn forget(receiver: Id) {
    let gone = tables()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    if let Some(table) = gone {
        table
            .columns
            .into_iter()
            .for_each(|c| objc::arc::release(c as Id));
        objc::arc::release(table.background as Id);
    }
}

unsafe extern "C" fn set_data_source(receiver: Id, _sel: Sel, source: Id) {
    update_table(receiver, |t| t.data_source = source as usize);
}

unsafe extern "C" fn data_source(receiver: Id, _sel: Sel) -> Id {
    table(receiver).data_source as Id
}

unsafe extern "C" fn set_delegate(receiver: Id, _sel: Sel, delegate: Id) {
    update_table(receiver, |t| t.delegate = delegate as usize);
}

unsafe extern "C" fn delegate(receiver: Id, _sel: Sel) -> Id {
    table(receiver).delegate as Id
}

unsafe extern "C" fn set_row_height(receiver: Id, _sel: Sel, height: f64) {
    update_table(receiver, |t| t.row_height = height);
}

unsafe extern "C" fn row_height(receiver: Id, _sel: Sel) -> f64 {
    table(receiver).row_height
}

unsafe extern "C" fn set_background_color(receiver: Id, _sel: Sel, color: Id) {
    let mut old = std::ptr::null_mut();
    update_table(receiver, |t| old = swap_retained(&mut t.background, color));
    objc::arc::release(old);
}

unsafe extern "C" fn background_color(receiver: Id, _sel: Sel) -> Id {
    table(receiver).background as Id
}

unsafe extern "C" fn add_table_column(receiver: Id, _sel: Sel, added: Id) {
    if added.is_null() {
        return;
    }
    objc::arc::retain(added);
    update_table(receiver, |t| t.columns.push(added as usize));
}

unsafe extern "C" fn table_columns(receiver: Id, _sel: Sel) -> Id {
    let values: Vec<*const std::ffi::c_void> = table(receiver)
        .columns
        .iter()
        .map(|c| *c as *const _)
        .collect();
    let array = corefoundation::collections::new_array(
        &values,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn number_of_columns(receiver: Id, _sel: Sel) -> isize {
    table(receiver).columns.len() as isize
}

/// `-numberOfRows`: the data source's `-numberOfRowsInTableView:`.
unsafe extern "C" fn number_of_rows(receiver: Id, _sel: Sel) -> isize {
    rows(receiver)
}

fn rows(receiver: Id) -> isize {
    let source = table(receiver).data_source as Id;
    let sel = objc::sel::get("numberOfRowsInTableView:");
    if !responds(source, sel) {
        return 0;
    }
    // SAFETY: `NSInteger (id, SEL, NSTableView *)`, as the protocol declares.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> isize = std::mem::transmute(objc::msg::entry());
        f(source, sel, receiver)
    }
}

fn responds(object: Id, sel: Sel) -> bool {
    // SAFETY: a live object, or nil.
    unsafe { objc::object::class_of(object) }.is_some_and(|c| c.method(sel).is_some())
}

/// The value at `row` of `column`, as the text the cell would show.
fn value(receiver: Id, column: Id, row: isize) -> String {
    let source = table(receiver).data_source as Id;
    let sel = objc::sel::get("tableView:objectValueForTableColumn:row:");
    if !responds(source, sel) {
        return String::new();
    }
    // SAFETY: `id (id, SEL, NSTableView *, NSTableColumn *, NSInteger)`.
    let object = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id, Id, isize) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(source, sel, receiver, column, row)
    };
    if object.is_null() {
        return String::new();
    }
    // SAFETY: `-description` is `id (id, SEL)` on every object.
    let text = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(object, objc::sel::get("description"))
    };
    // SAFETY: a string, or nil.
    String::from_utf8_lossy(&unsafe { foundation::string::bytes(text) }).into_owned()
}

unsafe extern "C" fn reload_data(receiver: Id, _sel: Sel) {
    crate::backing::needs_display(receiver);
}

unsafe extern "C" fn scroll_row_to_visible(receiver: Id, _sel: Sel, row: isize) {
    update_table(receiver, |t| t.visible_row = Some(row));
}

unsafe extern "C" fn selected_row(receiver: Id, _sel: Sel) -> isize {
    table(receiver).selected.last().copied().unwrap_or(-1)
}

unsafe extern "C" fn number_of_selected_rows(receiver: Id, _sel: Sel) -> isize {
    table(receiver).selected.len() as isize
}

unsafe extern "C" fn is_row_selected(receiver: Id, _sel: Sel, row: isize) -> u8 {
    table(receiver).selected.contains(&row) as u8
}

/// The members of an `NSIndexSet`, walked with `-firstIndex` and
/// `-indexGreaterThanIndex:`.
fn index_set_members(set: Id) -> Vec<isize> {
    let mut rows = Vec::new();
    if set.is_null() {
        return rows;
    }
    let first = objc::sel::get("firstIndex");
    let next = objc::sel::get("indexGreaterThanIndex:");
    if !responds(set, first) || !responds(set, next) {
        return rows;
    }
    // SAFETY: `NSUInteger (id, SEL)` and `NSUInteger (id, SEL, NSUInteger)`.
    unsafe {
        let get_first: unsafe extern "C" fn(Id, Sel) -> usize =
            std::mem::transmute(objc::msg::entry());
        let get_next: unsafe extern "C" fn(Id, Sel, usize) -> usize =
            std::mem::transmute(objc::msg::entry());
        let mut index = get_first(set, first);
        while index < isize::MAX as usize && rows.len() < 1 << 20 {
            rows.push(index as isize);
            index = get_next(set, next, index);
        }
    }
    rows
}

unsafe extern "C" fn select_row_indexes(receiver: Id, _sel: Sel, set: Id, extend: u8) {
    let rows = index_set_members(set);
    update_table(receiver, |t| {
        if extend == 0 {
            t.selected.clear();
        }
        t.selected.extend(rows);
        t.selected.sort_unstable();
        t.selected.dedup();
    });
}

unsafe extern "C" fn selected_row_indexes(receiver: Id, _sel: Sel) -> Id {
    let class = objc::class::lookup(c"NSMutableIndexSet").expect("declared by Foundation");
    let index_set = objc::sel::get("indexSet");
    let add = objc::sel::get("addIndex:");
    // SAFETY: `+indexSet` is `id (Class, SEL)`, `-addIndex:` is
    // `void (id, SEL, NSUInteger)`.
    unsafe {
        let make: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        let set = make(objc::class::ptr(class) as Id, index_set);
        let add_index: unsafe extern "C" fn(Id, Sel, usize) =
            std::mem::transmute(objc::msg::entry());
        for row in table(receiver).selected {
            add_index(set, add, row as usize);
        }
        set
    }
}

unsafe extern "C" fn deselect_all(receiver: Id, _sel: Sel, _sender: Id) {
    update_table(receiver, |t| t.selected.clear());
}

unsafe extern "C" fn deselect_row(receiver: Id, _sel: Sel, row: isize) {
    update_table(receiver, |t| t.selected.retain(|r| *r != row));
}

unsafe extern "C" fn ignored(_receiver: Id, _sel: Sel, _argument: Id) {}

/// The vertical offset that puts `visible_row` inside a clip view of
/// `visible` points, for a table `total` points tall.
fn scroll_offset(row: Option<isize>, pitch: f64, total: f64, visible: f64) -> f64 {
    let Some(row) = row.filter(|row| *row >= 0) else {
        return 0.0;
    };
    let bottom = (row as f64 + 1.0) * pitch;
    (bottom - visible).clamp(0.0, (total - visible).max(0.0))
}

unsafe extern "C" fn draw_rect(receiver: Id, _sel: Sel, _dirty: NSRect) {
    let bounds = view::bounds_of(receiver);
    let density = crate::backing::focus_density();
    let width = (bounds.size.width * density).ceil() as usize;
    let height = (bounds.size.height * density).ceil() as usize;
    if width == 0 || height == 0 {
        return;
    }
    let state = table(receiver);
    let mut pixels = vec![0u8; width * height * 4];
    let mut canvas = chrome::Canvas::new(&mut pixels, width, height, width * 4, density as f32);
    let (w, h) = (bounds.size.width as f32, bounds.size.height as f32);
    let fill = crate::label::color_of(state.background, chrome::Color::rgba(255, 255, 255, 1.0));
    canvas.fill_rect(chrome::Rect::new(0.0, 0.0, w, h), fill);
    let pitch = state.row_height + state.spacing_height;
    let count = rows(receiver);
    // SAFETY: a view of this crate's.
    let visible = match unsafe { view::superview_of(receiver) } {
        clip if !clip.is_null() => view::bounds_of(clip).size.height,
        _ => bounds.size.height,
    }
    .min(bounds.size.height);
    let offset = scroll_offset(state.visible_row, pitch, count as f64 * pitch, visible) as f32;
    let mut x = 0.0f32;
    for column_object in &state.columns {
        let column_state = column(*column_object as Id);
        let column_width = column_state.width as f32;
        if let Some((font, size)) = crate::label::chrome_font(column_state.font as Id) {
            let size = size as f32;
            for row in 0..count {
                let top = row as f32 * pitch as f32 - offset;
                if top > h || top + pitch as f32 <= 0.0 {
                    continue;
                }
                let text = value(receiver, *column_object as Id, row);
                let line = font.truncate(&text, size, (column_width - 4.0).max(0.0));
                let advance = font.measure(&line, size);
                let left = match column_state.alignment {
                    1 => x + column_width - 2.0 - advance,
                    2 => x + (column_width - advance) / 2.0,
                    _ => x + 2.0,
                };
                let baseline = font.centered_baseline(size, top, state.row_height as f32);
                canvas.draw_text(
                    &line,
                    &font,
                    size,
                    chrome::Point::new(left, baseline),
                    chrome::Color::rgba(0, 0, 0, 0.85),
                );
            }
        }
        x += column_width + 3.0;
    }
    crate::backing::draw_image(
        &pixels,
        width,
        height,
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

unsafe extern "C" fn column_set_identifier(receiver: Id, _sel: Sel, identifier: Id) {
    let mut old = std::ptr::null_mut();
    update_column(receiver, |c| {
        old = swap_retained(&mut c.identifier, identifier)
    });
    objc::arc::release(old);
}

unsafe extern "C" fn column_identifier(receiver: Id, _sel: Sel) -> Id {
    column(receiver).identifier as Id
}

unsafe extern "C" fn column_init_with_identifier(receiver: Id, sel: Sel, identifier: Id) -> Id {
    // SAFETY: the same column.
    unsafe { column_set_identifier(receiver, sel, identifier) };
    receiver
}

unsafe extern "C" fn column_set_width(receiver: Id, _sel: Sel, width: f64) {
    update_column(receiver, |c| c.width = width);
}

unsafe extern "C" fn column_width(receiver: Id, _sel: Sel) -> f64 {
    column(receiver).width
}

/// What a nib's `NSTableColumn` and its data cell hold, for
/// [`crate::decode`].
pub(crate) fn set_archived_column(receiver: Id, width: f64, font: Id, alignment: isize) {
    let font = objc::arc::retain(font) as usize;
    update_column(receiver, |c| {
        c.width = width;
        c.font = font;
        c.alignment = alignment;
    });
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSTableView").expect("declared in crate::classes");
    methods!(class, {
        "setDataSource:" => set_data_source, c"v24@0:8@16";
        "dataSource" => data_source, c"@16@0:8";
        "setDelegate:" => set_delegate, c"v24@0:8@16";
        "delegate" => delegate, c"@16@0:8";
        "setRowHeight:" => set_row_height, c"v24@0:8d16";
        "rowHeight" => row_height, c"d16@0:8";
        "setBackgroundColor:" => set_background_color, c"v24@0:8@16";
        "backgroundColor" => background_color, c"@16@0:8";
        "addTableColumn:" => add_table_column, c"v24@0:8@16";
        "tableColumns" => table_columns, c"@16@0:8";
        "numberOfColumns" => number_of_columns, c"q16@0:8";
        "numberOfRows" => number_of_rows, c"q16@0:8";
        "reloadData" => reload_data, c"v16@0:8";
        "noteNumberOfRowsChanged" => reload_data, c"v16@0:8";
        "scrollRowToVisible:" => scroll_row_to_visible, c"v24@0:8q16";
        "selectedRow" => selected_row, c"q16@0:8";
        "numberOfSelectedRows" => number_of_selected_rows, c"q16@0:8";
        "isRowSelected:" => is_row_selected, c"c24@0:8q16";
        "selectRowIndexes:byExtendingSelection:" => select_row_indexes, c"v28@0:8@16c24";
        "selectedRowIndexes" => selected_row_indexes, c"@16@0:8";
        "deselectAll:" => deselect_all, c"v24@0:8@16";
        "deselectRow:" => deselect_row, c"v24@0:8q16";
        "setTarget:" => ignored, c"v24@0:8@16";
        "setDoubleAction:" => ignored, c"v24@0:8:16";
        "drawRect:" => draw_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });
    let column_class = objc::class::lookup(c"NSTableColumn").expect("declared in crate::classes");
    methods!(column_class, {
        "initWithIdentifier:" => column_init_with_identifier, c"@24@0:8@16";
        "setIdentifier:" => column_set_identifier, c"v24@0:8@16";
        "identifier" => column_identifier, c"@16@0:8";
        "setWidth:" => column_set_width, c"v24@0:8d16";
        "width" => column_width, c"d16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_to_a_row_shows_it_at_the_bottom_when_the_table_overflows() {
        assert_eq!(scroll_offset(None, 20.0, 200.0, 80.0), 0.0);
        assert_eq!(scroll_offset(Some(2), 20.0, 200.0, 80.0), 0.0);
        assert_eq!(scroll_offset(Some(9), 20.0, 200.0, 80.0), 120.0);
        assert_eq!(scroll_offset(Some(9), 20.0, 60.0, 80.0), 0.0);
    }
}
