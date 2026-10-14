//! `NSMatrix`: a grid of cells, of which (in radio mode) one is chosen.
//!
//! A cell here is a proxy for a control (see `crate::cell`), so each of the
//! matrix's cells is a button laid out as its subview, and `-cellAtRow:column:`
//! answers that button's cell. A program that reaches through the cell to set a
//! title or a state is then changing what is drawn.
//!
//! FIXME (ACCURACY): highlight, list and track modes behave as radio mode does
//! for the selection they report, but a click selects one cell in each, and
//! nothing is ever chosen by dragging across cells.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use foundation::geometry::{NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// `NSMatrixMode`.
pub(crate) mod mode {
    pub const RADIO: usize = 0;
    pub const HIGHLIGHT: usize = 1;
    pub const LIST: usize = 2;
    pub const TRACK: usize = 3;
}

#[derive(Clone)]
struct Matrix {
    rows: usize,
    columns: usize,
    cell_size: NSSize,
    spacing: NSSize,
    mode: usize,
    allows_empty_selection: bool,
    /// Buttons, row by row, each retained here and also a subview.
    cells: Vec<usize>,
    /// The button new cells are copied from, retained and in no view tree.
    prototype: usize,
    /// The cell chosen outside radio mode, where no state records it.
    selected: Option<usize>,
}

impl Default for Matrix {
    fn default() -> Self {
        Self {
            rows: 0,
            columns: 0,
            cell_size: NSSize {
                width: 100.0,
                height: 17.0,
            },
            spacing: NSSize {
                width: 1.0,
                height: 1.0,
            },
            mode: mode::RADIO,
            allows_empty_selection: false,
            cells: Vec::new(),
            prototype: 0,
            selected: None,
        }
    }
}

fn matrices() -> &'static Mutex<HashMap<usize, Matrix>> {
    static MATRICES: OnceLock<Mutex<HashMap<usize, Matrix>>> = OnceLock::new();
    MATRICES.get_or_init(Default::default)
}

fn read(receiver: Id) -> Matrix {
    matrices()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .cloned()
        .unwrap_or_default()
}

fn update<R>(receiver: Id, change: impl FnOnce(&mut Matrix) -> R) -> R {
    change(
        matrices()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(receiver as usize)
            .or_default(),
    )
}

/// Drop the cells of a matrix that is going away.
pub(crate) fn forget(receiver: Id) {
    let gone = matrices()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    if let Some(matrix) = gone {
        for cell in matrix.cells {
            objc::arc::release(cell as Id);
        }
        objc::arc::release(matrix.prototype as Id);
    }
}

/// # Safety
///
/// `selector` must name a method taking nothing and returning a word.
unsafe fn get(receiver: Id, selector: &str) -> usize {
    // SAFETY: the caller's obligation.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector))
    }
}

/// # Safety
///
/// `selector` must name a method taking one word and returning nothing.
unsafe fn set(receiver: Id, selector: &str, value: usize) {
    // SAFETY: the caller's obligation.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, usize) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), value);
    }
}

fn set_frame(view: Id, frame: NSRect) {
    // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)` on every view.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
        f(view, objc::sel::get("setFrame:"), frame);
    }
}

fn state_of(button: Id) -> isize {
    // SAFETY: `-state` answers an `NSInteger` on every button.
    unsafe { get(button, "state") as isize }
}

fn set_state(button: Id, state: isize) {
    // SAFETY: `-setState:` takes one `NSInteger` on every button.
    unsafe { set(button, "setState:", state as usize) };
}

/// A new button to be a cell, by the prototype when there is one.
///
/// Its action is the matrix's private one, so a click reaches the matrix,
/// which keeps the selection and sends its own action.
pub(crate) fn new_cell(matrix: Id) -> Id {
    let class = objc::class::lookup(c"NSButton").expect("declared in crate::classes");
    let button = objc::object::create_instance(class, 0);
    // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)` on every view.
    let button = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(button, objc::sel::get("initWithFrame:"), NSRect::default())
    };
    let prototype = match matrix.is_null() {
        true => 0,
        false => read(matrix).prototype,
    } as Id;
    if !prototype.is_null() {
        // SAFETY: each pair is a getter and a setter of the same type, on
        // two buttons.
        unsafe {
            for (getter, setter) in [
                ("buttonType", "setButtonType:"),
                ("title", "setTitle:"),
                ("font", "setFont:"),
                ("isBordered", "setBordered:"),
                ("alignment", "setAlignment:"),
            ] {
                let value = get(prototype, getter);
                match getter {
                    "isBordered" => {
                        let f: unsafe extern "C" fn(Id, Sel, u8) =
                            std::mem::transmute(objc::msg::entry());
                        f(button, objc::sel::get(setter), value as u8);
                    }
                    _ => set(button, setter, value),
                }
            }
        }
    }
    button
}

/// Make `button` one of the matrix's cells: its target is the matrix.
fn adopt(matrix: Id, button: Id) {
    // SAFETY: `-setTarget:` takes an object and `-setAction:` a selector.
    unsafe {
        set(button, "setTarget:", matrix as usize);
        set(
            button,
            "setAction:",
            objc::sel::get("_rosemaryCellClicked:") as usize,
        );
        set(matrix, "addSubview:", button as usize);
    }
}

fn discard(button: Id) {
    // SAFETY: `-removeFromSuperview` is `void (id, SEL)` on every view.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
        f(button, objc::sel::get("removeFromSuperview"));
    }
    objc::arc::release(button);
}

/// Place each cell in its row and column. The matrix is flipped, so row zero
/// is at the top whatever height the matrix has.
fn lay_out(receiver: Id) {
    let matrix = read(receiver);
    let columns = matrix.columns.max(1);
    for (at, cell) in matrix.cells.iter().enumerate() {
        let (row, column) = (at / columns, at % columns);
        set_frame(
            *cell as Id,
            NSRect {
                origin: NSPoint {
                    x: column as f64 * (matrix.cell_size.width + matrix.spacing.width),
                    y: row as f64 * (matrix.cell_size.height + matrix.spacing.height),
                },
                size: matrix.cell_size,
            },
        );
    }
    crate::backing::needs_display(receiver);
}

/// The chosen cell's index: in radio mode the one that is on, which is what
/// a program setting a cell's state directly changes; otherwise the last one
/// clicked or selected.
fn selected_index(matrix: &Matrix) -> Option<usize> {
    match matrix.mode {
        mode::RADIO => matrix
            .cells
            .iter()
            .position(|cell| state_of(*cell as Id) != 0),
        _ => matrix.selected.filter(|at| *at < matrix.cells.len()),
    }
}

fn select_index(receiver: Id, at: Option<usize>) {
    let matrix = read(receiver);
    if matrix.mode == mode::RADIO {
        for (index, cell) in matrix.cells.iter().enumerate() {
            set_state(*cell as Id, isize::from(Some(index) == at));
        }
    }
    update(receiver, |m| m.selected = at);
    crate::backing::needs_display(receiver);
}

fn index_of(matrix: &Matrix, row: isize, column: isize) -> Option<usize> {
    let (row, column) = (usize::try_from(row).ok()?, usize::try_from(column).ok()?);
    if row >= matrix.rows || column >= matrix.columns {
        return None;
    }
    let at = row * matrix.columns + column;
    (at < matrix.cells.len()).then_some(at)
}

fn cell_at_index(matrix: &Matrix, at: Option<usize>) -> Id {
    match at.and_then(|at| matrix.cells.get(at)) {
        Some(button) => crate::cell::cell_of(*button as Id),
        None => std::ptr::null_mut(),
    }
}

/// The button behind a cell this module handed out, or the argument itself
/// when it is the button.
fn button_of(cell: Id) -> Id {
    if cell.is_null() || crate::view::is_view(cell) {
        return cell;
    }
    // SAFETY: `-controlView` is `id (id, SEL)` on every cell here.
    unsafe { get(cell, "controlView") as Id }
}

unsafe extern "C" fn cell_clicked(receiver: Id, _sel: Sel, sender: Id) {
    let matrix = read(receiver);
    let Some(at) = matrix.cells.iter().position(|c| *c == sender as usize) else {
        return;
    };
    select_index(receiver, Some(at));
    crate::controls::send_action(receiver);
}

unsafe extern "C" fn is_flipped(_receiver: Id, _sel: Sel) -> u8 {
    1
}

unsafe extern "C" fn number_of_rows(receiver: Id, _sel: Sel) -> isize {
    read(receiver).rows as isize
}

unsafe extern "C" fn number_of_columns(receiver: Id, _sel: Sel) -> isize {
    read(receiver).columns as isize
}

unsafe extern "C" fn get_number_of_rows(
    receiver: Id,
    _sel: Sel,
    rows: *mut isize,
    columns: *mut isize,
) {
    let matrix = read(receiver);
    // SAFETY: the caller's out-pointers, either of which may be null.
    unsafe {
        if !rows.is_null() {
            *rows = matrix.rows as isize;
        }
        if !columns.is_null() {
            *columns = matrix.columns as isize;
        }
    }
}

unsafe extern "C" fn cell_at(receiver: Id, _sel: Sel, row: isize, column: isize) -> Id {
    let matrix = read(receiver);
    cell_at_index(&matrix, index_of(&matrix, row, column))
}

unsafe extern "C" fn cells(receiver: Id, _sel: Sel) -> Id {
    let values: Vec<*const std::ffi::c_void> = read(receiver)
        .cells
        .iter()
        .map(|button| crate::cell::cell_of(*button as Id) as *const _)
        .collect();
    let array = corefoundation::collections::new_array(
        &values,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn cell_with_tag(receiver: Id, _sel: Sel, tag: isize) -> Id {
    let matrix = read(receiver);
    // SAFETY: `-tag` answers an `NSInteger` on every control.
    let at = matrix
        .cells
        .iter()
        .position(|cell| unsafe { get(*cell as Id, "tag") } as isize == tag);
    cell_at_index(&matrix, at)
}

unsafe extern "C" fn selected_cell(receiver: Id, _sel: Sel) -> Id {
    let matrix = read(receiver);
    cell_at_index(&matrix, selected_index(&matrix))
}

unsafe extern "C" fn selected_row(receiver: Id, _sel: Sel) -> isize {
    let matrix = read(receiver);
    selected_index(&matrix).map_or(-1, |at| (at / matrix.columns.max(1)) as isize)
}

unsafe extern "C" fn selected_column(receiver: Id, _sel: Sel) -> isize {
    let matrix = read(receiver);
    selected_index(&matrix).map_or(-1, |at| (at % matrix.columns.max(1)) as isize)
}

/// `-selectCellAtRow:column:`: -1 for either deselects, where empty selection
/// is allowed.
unsafe extern "C" fn select_cell_at(receiver: Id, _sel: Sel, row: isize, column: isize) {
    let matrix = read(receiver);
    match index_of(&matrix, row, column) {
        Some(at) => select_index(receiver, Some(at)),
        None if matrix.allows_empty_selection || matrix.mode != mode::RADIO => {
            select_index(receiver, None)
        }
        None => {}
    }
}

unsafe extern "C" fn select_cell_with_tag(receiver: Id, sel: Sel, tag: isize) -> u8 {
    let matrix = read(receiver);
    // SAFETY: `-tag` answers an `NSInteger` on every control.
    let Some(at) = matrix
        .cells
        .iter()
        .position(|cell| unsafe { get(*cell as Id, "tag") } as isize == tag)
    else {
        return 0;
    };
    let columns = matrix.columns.max(1);
    // SAFETY: the same matrix.
    unsafe {
        select_cell_at(
            receiver,
            sel,
            (at / columns) as isize,
            (at % columns) as isize,
        )
    };
    1
}

unsafe extern "C" fn select_cell(receiver: Id, _sel: Sel, cell: Id) {
    let button = button_of(cell) as usize;
    let at = read(receiver).cells.iter().position(|c| *c == button);
    if at.is_some() {
        select_index(receiver, at);
    }
}

unsafe extern "C" fn deselect_all_cells(receiver: Id, _sel: Sel) {
    let matrix = read(receiver);
    if matrix.mode == mode::RADIO && !matrix.allows_empty_selection {
        return;
    }
    select_index(receiver, None);
}

unsafe extern "C" fn set_state_at(receiver: Id, _sel: Sel, state: isize, row: isize, column: isize) {
    let matrix = read(receiver);
    let Some(at) = index_of(&matrix, row, column) else {
        return;
    };
    match matrix.mode {
        mode::RADIO if state != 0 => select_index(receiver, Some(at)),
        _ => set_state(matrix.cells[at] as Id, state),
    }
}

/// `-insertRow:`: a row of new cells, copied from the prototype.
unsafe extern "C" fn insert_row(receiver: Id, _sel: Sel, row: isize) {
    let matrix = read(receiver);
    let row = usize::try_from(row).unwrap_or(0).min(matrix.rows);
    // A matrix with no columns gains one, as AppKit's does.
    let columns = matrix.columns.max(1);
    let fresh: Vec<usize> = (0..columns)
        .map(|_| {
            let button = new_cell(receiver);
            adopt(receiver, button);
            button as usize
        })
        .collect();
    update(receiver, |m| {
        m.columns = columns;
        m.rows += 1;
        let at = (row * columns).min(m.cells.len());
        m.cells.splice(at..at, fresh);
        if let Some(selected) = m.selected.as_mut()
            && *selected >= at
        {
            *selected += columns;
        }
    });
    lay_out(receiver);
}

unsafe extern "C" fn add_row(receiver: Id, sel: Sel) {
    let rows = read(receiver).rows as isize;
    // SAFETY: the same matrix.
    unsafe { insert_row(receiver, sel, rows) };
}

unsafe extern "C" fn remove_row(receiver: Id, _sel: Sel, row: isize) {
    let Ok(row) = usize::try_from(row) else {
        return;
    };
    let removed = update(receiver, |m| {
        if row >= m.rows {
            return Vec::new();
        }
        let columns = m.columns.max(1);
        let start = (row * columns).min(m.cells.len());
        let end = (start + columns).min(m.cells.len());
        m.rows -= 1;
        m.selected = match m.selected {
            Some(s) if (start..end).contains(&s) => None,
            Some(s) if s >= end => Some(s - (end - start)),
            other => other,
        };
        m.cells.drain(start..end).collect()
    });
    for button in removed {
        discard(button as Id);
    }
    lay_out(receiver);
}

/// `-renewRows:columns:`: the grid resized, keeping the cells it still has in
/// order and making new ones for the rest.
unsafe extern "C" fn renew_rows(receiver: Id, _sel: Sel, rows: isize, columns: isize) {
    let (rows, columns) = (rows.max(0) as usize, columns.max(0) as usize);
    let wanted = rows * columns;
    let have = read(receiver).cells.len();
    let fresh: Vec<usize> = (have..wanted)
        .map(|_| {
            let button = new_cell(receiver);
            adopt(receiver, button);
            button as usize
        })
        .collect();
    let removed = update(receiver, |m| {
        m.rows = rows;
        m.columns = columns;
        m.cells.extend(fresh);
        let removed: Vec<usize> = m.cells.drain(wanted.min(m.cells.len())..).collect();
        m.selected = m.selected.filter(|s| *s < wanted);
        removed
    });
    for button in removed {
        discard(button as Id);
    }
    lay_out(receiver);
}

unsafe extern "C" fn size_to_cells(receiver: Id, _sel: Sel) {
    let matrix = read(receiver);
    let span = |count: usize, cell: f64, gap: f64| match count {
        0 => 0.0,
        n => n as f64 * cell + (n - 1) as f64 * gap,
    };
    // SAFETY: `-setFrameSize:` is `void (id, SEL, NSSize)` on every view.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSSize) = std::mem::transmute(objc::msg::entry());
        f(
            receiver,
            objc::sel::get("setFrameSize:"),
            NSSize {
                width: span(
                    matrix.columns,
                    matrix.cell_size.width,
                    matrix.spacing.width,
                ),
                height: span(matrix.rows, matrix.cell_size.height, matrix.spacing.height),
            },
        );
    }
}

unsafe extern "C" fn set_mode(receiver: Id, _sel: Sel, value: usize) {
    update(receiver, |m| m.mode = value);
}

unsafe extern "C" fn get_mode(receiver: Id, _sel: Sel) -> usize {
    read(receiver).mode
}

unsafe extern "C" fn set_allows_empty_selection(receiver: Id, _sel: Sel, value: u8) {
    update(receiver, |m| m.allows_empty_selection = value != 0);
}

unsafe extern "C" fn allows_empty_selection(receiver: Id, _sel: Sel) -> u8 {
    u8::from(read(receiver).allows_empty_selection)
}

unsafe extern "C" fn set_cell_size(receiver: Id, _sel: Sel, size: NSSize) {
    update(receiver, |m| m.cell_size = size);
    lay_out(receiver);
}

unsafe extern "C" fn cell_size(receiver: Id, _sel: Sel) -> NSSize {
    read(receiver).cell_size
}

unsafe extern "C" fn set_intercell_spacing(receiver: Id, _sel: Sel, size: NSSize) {
    update(receiver, |m| m.spacing = size);
    lay_out(receiver);
}

unsafe extern "C" fn intercell_spacing(receiver: Id, _sel: Sel) -> NSSize {
    read(receiver).spacing
}

unsafe extern "C" fn prototype(receiver: Id, _sel: Sel) -> Id {
    match read(receiver).prototype as Id {
        p if p.is_null() => p,
        p => crate::cell::cell_of(p),
    }
}

/// `-setPrototype:`: a cell this crate made stands for its button. A cell
/// with no button - one a program allocated itself - names nothing new cells
/// could be copied from, and leaves the prototype as it was.
unsafe extern "C" fn set_prototype(receiver: Id, _sel: Sel, cell: Id) {
    let button = button_of(cell);
    if button.is_null() {
        return;
    }
    objc::arc::retain(button);
    let old = update(receiver, |m| std::mem::replace(&mut m.prototype, button as usize));
    objc::arc::release(old as Id);
}

/// What a nib's `NSMatrix` holds, for [`crate::decode`].
pub(crate) struct Archived {
    pub rows: usize,
    pub columns: usize,
    pub cell_size: Option<NSSize>,
    pub spacing: Option<NSSize>,
    pub mode: usize,
    pub allows_empty_selection: bool,
    /// Buttons made by [`new_cell`], whose references pass to the matrix.
    pub cells: Vec<Id>,
    pub prototype: Id,
    pub selected: Option<(usize, usize)>,
}

/// Give a matrix decoded from a nib its grid and cells.
pub(crate) fn set_archived(receiver: Id, archived: Archived) {
    for cell in &archived.cells {
        adopt(receiver, *cell);
    }
    update(receiver, |m| {
        m.rows = archived.rows;
        m.columns = archived.columns;
        if let Some(size) = archived.cell_size {
            m.cell_size = size;
        }
        if let Some(spacing) = archived.spacing {
            m.spacing = spacing;
        }
        m.mode = archived.mode;
        m.allows_empty_selection = archived.allows_empty_selection;
        m.cells = archived.cells.iter().map(|c| *c as usize).collect();
        m.prototype = archived.prototype as usize;
        m.selected = archived
            .selected
            .map(|(row, column)| row * archived.columns.max(1) + column);
    });
    lay_out(receiver);
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSMatrix").expect("declared in crate::classes");
    methods!(class, {
        "isFlipped" => is_flipped, c"c16@0:8";
        "_rosemaryCellClicked:" => cell_clicked, c"v24@0:8@16";
        "numberOfRows" => number_of_rows, c"q16@0:8";
        "numberOfColumns" => number_of_columns, c"q16@0:8";
        "getNumberOfRows:columns:" => get_number_of_rows, c"v32@0:8^q16^q24";
        "cellAtRow:column:" => cell_at, c"@32@0:8q16q24";
        "cells" => cells, c"@16@0:8";
        "cellWithTag:" => cell_with_tag, c"@24@0:8q16";
        "selectedCell" => selected_cell, c"@16@0:8";
        "selectedRow" => selected_row, c"q16@0:8";
        "selectedColumn" => selected_column, c"q16@0:8";
        "selectCellAtRow:column:" => select_cell_at, c"v32@0:8q16q24";
        "selectCellWithTag:" => select_cell_with_tag, c"c24@0:8q16";
        "selectCell:" => select_cell, c"v24@0:8@16";
        "deselectAllCells" => deselect_all_cells, c"v16@0:8";
        "setState:atRow:column:" => set_state_at, c"v40@0:8q16q24q32";
        "insertRow:" => insert_row, c"v24@0:8q16";
        "addRow" => add_row, c"v16@0:8";
        "removeRow:" => remove_row, c"v24@0:8q16";
        "renewRows:columns:" => renew_rows, c"v32@0:8q16q24";
        "sizeToCells" => size_to_cells, c"v16@0:8";
        "setMode:" => set_mode, c"v24@0:8Q16";
        "mode" => get_mode, c"Q16@0:8";
        "setAllowsEmptySelection:" => set_allows_empty_selection, c"v20@0:8c16";
        "allowsEmptySelection" => allows_empty_selection, c"c16@0:8";
        "setCellSize:" => set_cell_size, c"v32@0:8{CGSize=dd}16";
        "cellSize" => cell_size, c"{CGSize=dd}16@0:8";
        "setIntercellSpacing:" => set_intercell_spacing, c"v32@0:8{CGSize=dd}16";
        "intercellSpacing" => intercell_spacing, c"{CGSize=dd}16@0:8";
        "prototype" => prototype, c"@16@0:8";
        "setPrototype:" => set_prototype, c"v24@0:8@16";
    });
}
