//! What a window shows of the views that draw on the CPU.
//!
//! AppKit gives every window a backing store which `-drawRect:` draws into
//! with the view's focus locked, and the window server composites that store
//! with the window's layers. Qt's raster widgets reach the screen exactly this
//! way: `QNSWindowBackingStore::flush` locks focus on its view and draws the
//! widget image with `-[NSImage drawInRect:fromRect:...]`.
//!
//! Here a store is premultiplied B, G, R, A pixels at the window's pixel size,
//! top row first. Presenting one goes through [`set_presenter`] when something
//! has claimed the window's surface - Metal composites the store under its
//! layers, as the window server would - and through SDL's window surface
//! otherwise.
//!
//! FIXME (ACCURACY): drawing is scaled nearest-neighbour and clipped to the
//! focused view's rectangle only, not to its superviews'.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use foundation::geometry::{NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

pub struct Store {
    pub width: u32,
    pub height: u32,
    /// Premultiplied B, G, R, A, top row first.
    pub pixels: Vec<u8>,
    dirty: bool,
}

/// Keyed by the SDL window's address.
static STORES: Mutex<Option<HashMap<usize, Store>>> = Mutex::new(None);

/// Views sent `-setNeedsDisplay:`, by address, drawn at the next pump.
static NEEDS_DISPLAY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Presents a store for a window; `false` where it cannot, so that SDL's
/// window surface is used instead.
pub type Presenter = fn(sdl3::Window, &Store) -> bool;

static PRESENTER: Mutex<Option<Presenter>> = Mutex::new(None);

/// Let a GPU backend present stores, so that a window it also presents layers
/// to has one owner of its surface.
pub fn set_presenter(presenter: Presenter) {
    *PRESENTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(presenter);
}

#[derive(Clone, Copy)]
struct Focus {
    view: Id,
    window: sdl3::Window,
    /// The view's top-left corner and size in window pixels.
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    /// Window pixels per point.
    density: f64,
    flipped: bool,
}

thread_local! {
    static FOCUS: RefCell<Vec<Focus>> = const { RefCell::new(Vec::new()) };
}

fn send_flag(receiver: Id, selector: &str) -> bool {
    // SAFETY: a live object; the selector is `BOOL (id, SEL)`.
    unsafe {
        let send: unsafe extern "C" fn(Id, Sel) -> u8 = std::mem::transmute(objc::msg::entry());
        send(receiver, objc::sel::get(selector)) != 0
    }
}

fn focus_on(view: Id) -> Option<Focus> {
    // SAFETY: null or an object this runtime allocated.
    let frame = unsafe { crate::view::presentation_frame(view) }?;
    let (points, _) = sdl3::video::window_size(frame.window)?;
    let density = match points > 0 {
        true => f64::from(frame.window_width) / f64::from(points),
        false => 1.0,
    };
    Some(Focus {
        view,
        window: frame.window,
        x: f64::from(frame.x),
        y: f64::from(frame.y),
        width: f64::from(frame.width),
        height: f64::from(frame.height),
        density: if density > 0.0 { density } else { 1.0 },
        flipped: send_flag(view, "isFlipped"),
    })
}

fn push_focus(view: Id) -> bool {
    let Some(focus) = focus_on(view) else {
        return false;
    };
    FOCUS.with(|stack| stack.borrow_mut().push(focus));
    true
}

fn pop_focus() {
    FOCUS.with(|stack| stack.borrow_mut().pop());
}

fn current_focus() -> Option<Focus> {
    FOCUS.with(|stack| stack.borrow().last().copied())
}

fn with_store<R>(window: sdl3::Window, body: impl FnOnce(&mut Store) -> R) -> Option<R> {
    let (width, height) = sdl3::video::window_size_in_pixels(window)?;
    let (width, height) = (width.max(1) as u32, height.max(1) as u32);
    let mut stores = STORES.lock().unwrap_or_else(|e| e.into_inner());
    let store = stores
        .get_or_insert_with(HashMap::new)
        .entry(window as usize)
        .or_insert_with(|| Store {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
            dirty: false,
        });
    if (store.width, store.height) != (width, height) {
        let mut pixels = vec![0; width as usize * height as usize * 4];
        let columns = store.width.min(width) as usize;
        for row in 0..store.height.min(height) as usize {
            let from = row * store.width as usize * 4;
            let to = row * width as usize * 4;
            pixels[to..to + columns * 4].copy_from_slice(&store.pixels[from..from + columns * 4]);
        }
        *store = Store {
            width,
            height,
            pixels,
            dirty: true,
        };
    }
    Some(body(store))
}

/// `NSCompositingOperation` values this draws with; any other is drawn as
/// source-over.
const COMPOSITE_COPY: usize = 1;

/// Draw premultiplied BGRA `source` pixels into the focused view.
///
/// `destination` is in the view's coordinates and `from` in the image's,
/// whose origin is its bottom-left corner. `points` is the image's size in
/// points, which scales `from` into the source's pixels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_image(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    points: NSSize,
    destination: NSRect,
    from: NSRect,
    operation: usize,
    fraction: f64,
    respect_flipped: bool,
) {
    let Some(focus) = current_focus() else {
        return;
    };
    with_store(focus.window, |store| {
        draw_into(
            store,
            source,
            source_width,
            source_height,
            points,
            destination,
            from,
            operation,
            fraction,
            respect_flipped,
        )
    });
}

/// [`draw_image`], into a store already in hand.
#[allow(clippy::too_many_arguments)]
fn draw_into(
    store: &mut Store,
    source: &[u8],
    source_width: usize,
    source_height: usize,
    points: NSSize,
    destination: NSRect,
    from: NSRect,
    operation: usize,
    fraction: f64,
    respect_flipped: bool,
) {
    let Some(focus) = current_focus() else {
        return;
    };
    if source_width == 0
        || source_height == 0
        || fraction <= 0.0
        || source.len() < source_width * source_height * 4
    {
        return;
    }
    let from = match from.size.width <= 0.0 || from.size.height <= 0.0 {
        true => NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: points,
        },
        false => from,
    };
    let scale_x = source_width as f64 / points.width.max(f64::MIN_POSITIVE);
    let scale_y = source_height as f64 / points.height.max(f64::MIN_POSITIVE);
    let source_left = from.origin.x * scale_x;
    let source_top = source_height as f64 - (from.origin.y + from.size.height) * scale_y;
    let source_w = from.size.width * scale_x;
    let source_h = from.size.height * scale_y;

    let view_height = focus.height / focus.density;
    let top = match focus.flipped {
        true => destination.origin.y,
        false => view_height - (destination.origin.y + destination.size.height),
    };
    let left = focus.x + destination.origin.x * focus.density;
    let top = focus.y + top * focus.density;
    let width = destination.size.width * focus.density;
    let height = destination.size.height * focus.density;
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    // A flipped context draws an image upside down unless asked not to.
    let mirrored = focus.flipped && !respect_flipped;
    let opacity = (fraction.min(1.0) * 256.0) as u32;

    let clip_left = focus.x.max(0.0);
    let clip_top = focus.y.max(0.0);
    let clip_right = (focus.x + focus.width).min(f64::from(store.width));
    let clip_bottom = (focus.y + focus.height).min(f64::from(store.height));
    let x0 = left.max(clip_left).round() as i64;
    let x1 = (left + width).min(clip_right).round() as i64;
    let y0 = top.max(clip_top).round() as i64;
    let y1 = (top + height).min(clip_bottom).round() as i64;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let columns: Vec<usize> = (x0..x1)
        .map(|x| {
            let u = (x as f64 + 0.5 - left) / width;
            ((source_left + u * source_w).floor().max(0.0) as usize).min(source_width - 1)
        })
        .collect();
    for y in y0..y1 {
        let mut v = (y as f64 + 0.5 - top) / height;
        if mirrored {
            v = 1.0 - v;
        }
        let row = ((source_top + v * source_h).floor().max(0.0) as usize).min(source_height - 1);
        let from_row = &source[row * source_width * 4..(row + 1) * source_width * 4];
        let base = y as usize * store.width as usize * 4;
        let to_row = &mut store.pixels[base + x0 as usize * 4..base + x1 as usize * 4];
        for (target, &column) in to_row.chunks_exact_mut(4).zip(&columns) {
            let texel = &from_row[column * 4..column * 4 + 4];
            if operation == COMPOSITE_COPY && opacity >= 256 {
                target.copy_from_slice(texel);
                continue;
            }
            let alpha = texel[3] as u32 * opacity / 256;
            for channel in 0..4 {
                let s = texel[channel] as u32 * opacity / 256;
                let d = target[channel] as u32;
                target[channel] = match operation == COMPOSITE_COPY {
                    true => s as u8,
                    false => (s + d * (255 - alpha) / 255).min(255) as u8,
                };
            }
        }
    }
    store.dirty = true;
}

/// Mark a view for `-drawRect:` at the next pump.
pub(crate) fn needs_display(view: Id) {
    let mut pending = NEEDS_DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    if !pending.contains(&(view as usize)) {
        pending.push(view as usize);
    }
}

/// Draw `view` now, with its focus locked, as `-display` does.
pub(crate) fn display_view(view: Id) {
    let sel_draw = objc::sel::get("drawRect:");
    // SAFETY: a live object this runtime allocated.
    let responds =
        unsafe { objc::object::class_of(view) }.is_some_and(|c| c.method(sel_draw).is_some());
    if !responds || !push_focus(view) {
        return;
    }
    let bounds = current_focus().map_or(NSRect::default(), |focus| NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: focus.width / focus.density,
            height: focus.height / focus.density,
        },
    });
    // SAFETY: `-drawRect:` is `void (id, SEL, NSRect)`.
    unsafe {
        let send: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
        send(view, sel_draw, bounds);
    }
    pop_focus();
}

/// Draw everything marked for display and present every store that changed.
pub fn pump() {
    for window in crate::window::all() {
        if !crate::window::is_shown(window) {
            continue;
        }
        // SAFETY: a window this crate made.
        let sdl = unsafe { crate::window::sdl_window(window) };
        let Some((width, height)) = sdl3::video::window_size_in_pixels(sdl) else {
            continue;
        };
        let current = STORES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|stores| stores.get(&(sdl as usize)))
            .map(|store| (store.width as i32, store.height as i32));
        // A window newly shown, or resized, has nothing valid in its store:
        // AppKit redraws the whole window then.
        if current != Some((width.max(1), height.max(1))) {
            let content = crate::window::content_view_of(window);
            if !content.is_null() {
                needs_display(content);
            }
        }
    }
    let pending = std::mem::take(&mut *NEEDS_DISPLAY.lock().unwrap_or_else(|e| e.into_inner()));
    for view in pending {
        let view = view as Id;
        // SAFETY: a view this crate recorded while it was live.
        let window = unsafe { crate::view::window_of(view) };
        if crate::window::is_shown(window) && !crate::view::is_hidden_or_has_hidden_ancestor(view) {
            display_view(view);
        }
    }
    present_dirty();
}

fn present_dirty() {
    let presenter = *PRESENTER.lock().unwrap_or_else(|e| e.into_inner());
    let mut stores = STORES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(stores) = stores.as_mut() else {
        return;
    };
    for (&window, store) in stores
        .iter_mut()
        .filter(|(window, store)| store.dirty || dump_overdue(**window as sdl3::Window))
    {
        let window = window as sdl3::Window;
        store.dirty = false;
        let composed = compose(window, store);
        let store = composed.as_ref().unwrap_or(store);
        dump(window, store);
        if presenter.is_some_and(|present| present(window, store)) {
            continue;
        }
        sdl3::video::present_window(window, |surface| {
            let rows = store.height.min(surface.height.max(0) as u32) as usize;
            let columns = store.width.min(surface.width.max(0) as u32) as usize;
            for row in 0..rows {
                let from = row * store.width as usize * 4;
                // SAFETY: SDL reported `pitch` bytes per row for `height` rows.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        store.pixels[from..from + columns * 4].as_ptr(),
                        surface.pixels.add(row * surface.pitch as usize),
                        columns * 4,
                    )
                };
            }
            rows > 0 && columns > 0
        });
    }
}

/// Pixels a view shows above its window's store.
///
/// A view whose content comes from outside the window's drawing - a web view,
/// whose page another toolkit renders - is a layer the window server stacks
/// above the views that draw into the store. Writing it into the store instead
/// would lose it at the next flush of the view underneath, which repaints its
/// whole rectangle without knowing a subview is there.
pub struct Overlay {
    pub width: u32,
    pub height: u32,
    /// Premultiplied B, G, R, A, top row first.
    pub pixels: Vec<u8>,
}

/// Keyed by the view's address, in the order the overlays were first set.
static OVERLAYS: Mutex<Vec<(usize, Overlay)>> = Mutex::new(Vec::new());

/// Show `overlay` over `view`'s rectangle from the next present on.
///
/// The view must call [`remove_overlay`] before it is deallocated.
pub fn set_overlay(view: Id, overlay: Overlay) {
    {
        let mut overlays = OVERLAYS.lock().unwrap_or_else(|e| e.into_inner());
        match overlays.iter_mut().find(|(key, _)| *key == view as usize) {
            Some((_, existing)) => *existing = overlay,
            None => overlays.push((view as usize, overlay)),
        }
    }
    mark_window_of(view);
}

pub fn remove_overlay(view: Id) {
    let removed = {
        let mut overlays = OVERLAYS.lock().unwrap_or_else(|e| e.into_inner());
        let before = overlays.len();
        overlays.retain(|(key, _)| *key != view as usize);
        overlays.len() != before
    };
    if removed {
        mark_window_of(view);
    }
}

/// Have the store of `view`'s window presented again, creating it if the
/// window has none: an overlay on a window nothing else draws into still has
/// to reach the screen.
fn mark_window_of(view: Id) {
    // SAFETY: a live view, which both callers are passed.
    let Some(frame) = (unsafe { crate::view::presentation_frame(view) }) else {
        return;
    };
    with_store(frame.window, |store| store.dirty = true);
}

/// The store with every visible overlay of `window` drawn over it, or `None`
/// when there is nothing to draw and the store can be presented as it is.
fn compose(window: sdl3::Window, store: &Store) -> Option<Store> {
    let overlays = OVERLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let mut composed: Option<Store> = None;
    for (view, overlay) in overlays.iter() {
        let view = *view as Id;
        // SAFETY: an overlay is removed before its view is deallocated.
        let shown = unsafe { crate::view::window_of(view) };
        if !crate::window::is_shown(shown) || crate::view::is_hidden_or_has_hidden_ancestor(view) {
            continue;
        }
        // SAFETY: as above.
        let Some(frame) = (unsafe { crate::view::presentation_frame(view) }) else {
            continue;
        };
        if frame.window != window {
            continue;
        }
        let target = composed.get_or_insert_with(|| Store {
            width: store.width,
            height: store.height,
            pixels: store.pixels.clone(),
            dirty: false,
        });
        blend_overlay(target, overlay, frame.x, frame.y, frame.width, frame.height);
    }
    composed
}

/// Draw `overlay` scaled into the rectangle at `left`, `top` of `store`,
/// source-over, nearest-neighbour.
fn blend_overlay(
    store: &mut Store,
    overlay: &Overlay,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
) {
    let (source_width, source_height) = (overlay.width as usize, overlay.height as usize);
    if source_width == 0
        || source_height == 0
        || width == 0
        || height == 0
        || overlay.pixels.len() < source_width * source_height * 4
    {
        return;
    }
    let x0 = left.max(0) as usize;
    let y0 = top.max(0) as usize;
    let x1 = (i64::from(left) + i64::from(width)).clamp(0, i64::from(store.width)) as usize;
    let y1 = (i64::from(top) + i64::from(height)).clamp(0, i64::from(store.height)) as usize;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let columns: Vec<usize> = (x0..x1)
        .map(|x| {
            ((x as i64 - i64::from(left)) as usize * source_width / width as usize)
                .min(source_width - 1)
        })
        .collect();
    for y in y0..y1 {
        let row = ((y as i64 - i64::from(top)) as usize * source_height / height as usize)
            .min(source_height - 1);
        let from_row = &overlay.pixels[row * source_width * 4..(row + 1) * source_width * 4];
        let base = y * store.width as usize * 4;
        let to_row = &mut store.pixels[base + x0 * 4..base + x1 * 4];
        for (target, &column) in to_row.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            let texel = &from_row[column * 4..column * 4 + 4];
            let alpha = u32::from(texel[3]);
            if alpha == 255 {
                target.copy_from_slice(texel);
                continue;
            }
            for channel in 0..4 {
                let blended =
                    u32::from(texel[channel]) + u32::from(target[channel]) * (255 - alpha) / 255;
                target[channel] = blended.min(255) as u8;
            }
        }
    }
}

/// `ROSETTA3_STORE_OUT=DIR`: write each window's store as a binary PPM when it
/// is presented, at most once a second per window, overwriting the last.
///
/// The Metal frame dump sees only layers; this is the half of a window that
/// views drew on the CPU, for a session with no screen to look at.
/// When each window's store was last written, and whether a present since
/// then went unwritten.
static DUMPED: Mutex<Vec<(usize, std::time::Instant, bool)>> = Mutex::new(Vec::new());

/// Whether a store should be presented again only so that its last frame is
/// written: without this, a window that stops changing within a second of
/// its last dump leaves a stale picture of itself behind.
fn dump_overdue(window: sdl3::Window) -> bool {
    std::env::var_os("ROSETTA3_STORE_OUT").is_some()
        && DUMPED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(key, at, skipped)| {
                *key == window as usize && *skipped && at.elapsed().as_secs_f64() >= 1.0
            })
}

fn dump(window: sdl3::Window, store: &Store) {
    use std::time::Instant;
    let Some(directory) = std::env::var_os("ROSETTA3_STORE_OUT") else {
        return;
    };
    let mut last = DUMPED.lock().unwrap_or_else(|e| e.into_inner());
    match last.iter_mut().find(|(key, _, _)| *key == window as usize) {
        Some((_, at, skipped)) if at.elapsed().as_secs_f64() < 1.0 => {
            *skipped = true;
            return;
        }
        Some((_, at, skipped)) => {
            *at = Instant::now();
            *skipped = false;
        }
        None => last.push((window as usize, Instant::now(), false)),
    }
    let mut out = format!("P6\n{} {}\n255\n", store.width, store.height).into_bytes();
    for texel in store.pixels.chunks_exact(4) {
        out.extend_from_slice(&[texel[2], texel[1], texel[0]]);
    }
    let name = format!("store-{:x}.ppm", window as usize);
    let _ = std::fs::write(std::path::Path::new(&directory).join(name), out);
}

/// Whether a store has been drawn into for `window`, for a presenter deciding
/// whether a window has CPU-drawn content under its layers.
pub fn has_store(window: sdl3::Window) -> bool {
    STORES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|stores| stores.contains_key(&(window as usize)))
}

unsafe extern "C" fn lock_focus_if_can_draw(receiver: Id, _sel: Sel) -> u8 {
    if crate::view::is_hidden_or_has_hidden_ancestor(receiver) {
        return 0;
    }
    push_focus(receiver) as u8
}

unsafe extern "C" fn lock_focus(receiver: Id, _sel: Sel) {
    push_focus(receiver);
}

unsafe extern "C" fn unlock_focus(_receiver: Id, _sel: Sel) {
    pop_focus();
}

unsafe extern "C" fn focus_view(_class: Id, _sel: Sel) -> Id {
    current_focus().map_or(std::ptr::null_mut(), |focus| focus.view)
}

/// `-getRectsBeingDrawn:count:`: the whole view, which is what [`display_view`]
/// draws.
unsafe extern "C" fn get_rects_being_drawn(
    receiver: Id,
    _sel: Sel,
    rects: *mut *const NSRect,
    count: *mut isize,
) {
    thread_local! {
        static DRAWN: RefCell<NSRect> = const { RefCell::new(NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize { width: 0.0, height: 0.0 },
        }) };
    }
    let size = current_focus()
        .filter(|focus| focus.view == receiver)
        .map_or(NSSize::default(), |focus| NSSize {
            width: focus.width / focus.density,
            height: focus.height / focus.density,
        });
    DRAWN.with(|drawn| {
        *drawn.borrow_mut() = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size,
        };
        if !rects.is_null() {
            // SAFETY: the caller's out-pointer; the rect lives as long as the
            // thread, past the `-drawRect:` it is read in.
            unsafe { *rects = drawn.as_ptr() };
        }
    });
    if !count.is_null() {
        // SAFETY: the caller's out-pointer.
        unsafe { *count = 1 };
    }
}

unsafe extern "C" fn current_context(_class: Id, _sel: Sel) -> Id {
    if current_focus().is_none() {
        return std::ptr::null_mut();
    }
    shared_instance(c"NSGraphicsContext")
}

fn shared_instance(name: &'static std::ffi::CStr) -> Id {
    static INSTANCES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    let class = objc::class::lookup(name).expect("declared during bootstrap");
    let key = objc::class::ptr(class) as usize;
    let mut instances = INSTANCES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, object)) = instances.iter().find(|(k, _)| *k == key) {
        return *object as Id;
    }
    let object = objc::object::create_instance(class, 0);
    objc::arc::retain(object);
    instances.push((key, object as usize));
    object
}

unsafe extern "C" fn context_is_flipped(_receiver: Id, _sel: Sel) -> u8 {
    current_focus().is_some_and(|focus| focus.flipped) as u8
}

unsafe extern "C" fn nil(_receiver: Id, _sel: Sel) -> Id {
    std::ptr::null_mut()
}

unsafe extern "C" fn ignored(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn ignored_integer(_receiver: Id, _sel: Sel, _value: isize) {}

unsafe extern "C" fn zero(_receiver: Id, _sel: Sel) -> isize {
    0
}

unsafe extern "C" fn shared_color_space(_receiver: Id, _sel: Sel) -> Id {
    shared_instance(c"NSColorSpace")
}

/// `-[NSColorSpace CGColorSpace]`, which the caller does not own.
unsafe extern "C" fn cg_color_space(_receiver: Id, _sel: Sel) -> *const c_void {
    static SPACE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // SAFETY: creates an object this runtime keeps for the process.
    *SPACE.get_or_init(|| unsafe { crate::graphics::CGColorSpaceCreateDeviceRGB() } as usize)
        as *const c_void
}

pub(crate) fn install() {
    let view = objc::class::lookup(c"NSView").expect("declared before its methods");
    methods!(view, {
        "lockFocusIfCanDraw" => lock_focus_if_can_draw, c"c16@0:8";
        "lockFocus" => lock_focus, c"v16@0:8";
        "unlockFocus" => unlock_focus, c"v16@0:8";
        "getRectsBeingDrawn:count:" => get_rects_being_drawn, c"v32@0:8^^{CGRect}16^q24";
    });
    class_methods!(view, {
        "focusView" => focus_view, c"@16@0:8";
    });

    let context = objc::class::lookup(c"NSGraphicsContext").expect("declared");
    class_methods!(context, {
        "currentContext" => current_context, c"@16@0:8";
        "saveGraphicsState" => ignored, c"v16@0:8";
        "restoreGraphicsState" => ignored, c"v16@0:8";
    });
    methods!(context, {
        "isFlipped" => context_is_flipped, c"c16@0:8";
        "CGContext" => nil, c"^v16@0:8";
        "graphicsPort" => nil, c"^v16@0:8";
        "saveGraphicsState" => ignored, c"v16@0:8";
        "restoreGraphicsState" => ignored, c"v16@0:8";
        "flushGraphics" => ignored, c"v16@0:8";
        "compositingOperation" => zero, c"q16@0:8";
        "setCompositingOperation:" => ignored_integer, c"v24@0:8q16";
    });

    let space = objc::class::lookup(c"NSColorSpace").expect("declared");
    class_methods!(space, {
        "sRGBColorSpace" => shared_color_space, c"@16@0:8";
        "deviceRGBColorSpace" => shared_color_space, c"@16@0:8";
        "genericRGBColorSpace" => shared_color_space, c"@16@0:8";
    });
    methods!(space, {
        "CGColorSpace" => cg_color_space, c"^v16@0:8";
    });
    let window = objc::class::lookup(c"NSWindow").expect("declared");
    methods!(window, {
        "colorSpace" => shared_color_space, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_of(width: u32, height: u32) -> Store {
        Store {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
            dirty: false,
        }
    }

    #[test]
    fn copies_a_flipped_view_upright() {
        // Two rows: red on top, blue below, as Qt's image holds them.
        let source = [0, 0, 255, 255, 255, 0, 0, 255];
        let focus = Focus {
            view: std::ptr::null_mut(),
            window: std::ptr::null_mut(),
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 2.0,
            density: 1.0,
            flipped: true,
        };
        let size = NSSize {
            width: 1.0,
            height: 2.0,
        };
        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size,
        };
        FOCUS.with(|stack| stack.borrow_mut().push(focus));
        let mut store = store_of(1, 2);
        draw_into(&mut store, &source, 1, 2, size, rect, rect, 1, 1.0, true);
        assert_eq!(store.pixels, source);
        let mut mirrored = store_of(1, 2);
        draw_into(
            &mut mirrored,
            &source,
            1,
            2,
            size,
            rect,
            rect,
            1,
            1.0,
            false,
        );
        assert_eq!(mirrored.pixels, [255, 0, 0, 255, 0, 0, 255, 255]);
        FOCUS.with(|stack| stack.borrow_mut().clear());
    }

    #[test]
    fn an_overlay_is_scaled_into_its_rectangle_over_the_store() {
        let mut store = store_of(4, 2);
        store.pixels = vec![9; 4 * 2 * 4];
        // One opaque texel and one fully transparent one, stretched over two
        // store pixels each.
        let overlay = Overlay {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 255, 0, 0, 0, 0],
        };
        blend_overlay(&mut store, &overlay, 0, 1, 4, 1);
        assert_eq!(&store.pixels[..16], &[9; 16]);
        assert_eq!(
            &store.pixels[16..],
            &[1, 2, 3, 255, 1, 2, 3, 255, 9, 9, 9, 9, 9, 9, 9, 9]
        );
    }

    #[test]
    fn an_overlay_past_the_store_edge_is_clipped() {
        let mut store = store_of(2, 2);
        let overlay = Overlay {
            width: 1,
            height: 1,
            pixels: vec![7, 7, 7, 255],
        };
        blend_overlay(&mut store, &overlay, 1, -1, 4, 2);
        assert_eq!(
            store.pixels,
            [0, 0, 0, 0, 7, 7, 7, 255, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn source_over_blends_premultiplied_pixels() {
        let focus = Focus {
            view: std::ptr::null_mut(),
            window: std::ptr::null_mut(),
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            density: 1.0,
            flipped: true,
        };
        FOCUS.with(|stack| stack.borrow_mut().push(focus));
        let size = NSSize {
            width: 1.0,
            height: 1.0,
        };
        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size,
        };
        let mut store = store_of(1, 1);
        store.pixels = vec![200, 200, 200, 255];
        draw_into(
            &mut store,
            &[0, 0, 128, 128],
            1,
            1,
            size,
            rect,
            rect,
            2,
            1.0,
            true,
        );
        assert_eq!(store.pixels, [99, 99, 227, 255]);
        FOCUS.with(|stack| stack.borrow_mut().clear());
    }
}
