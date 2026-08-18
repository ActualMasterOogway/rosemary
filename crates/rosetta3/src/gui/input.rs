//! Taking the mouse, but only when asked.

use std::ffi::{c_ulong, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use super::objc::{Id, send0, send1, send2};
use super::sys;

/// `NSEventType`, from `<AppKit/NSEvent.h>`. A mask bit is `1 << type`.
const TYPE_LEFT_MOUSE_DOWN: u64 = 1;
const TYPE_LEFT_MOUSE_UP: u64 = 2;
const TYPE_LEFT_MOUSE_DRAGGED: u64 = 6;
const TYPE_SCROLL_WHEEL: u64 = 22;

/// The events the monitor asks for: a scroll for the panel's contents, and the
/// press-drag-release that moves or resizes it.
const MASK: u64 = (1 << TYPE_LEFT_MOUSE_DOWN)
    | (1 << TYPE_LEFT_MOUSE_UP)
    | (1 << TYPE_LEFT_MOUSE_DRAGGED)
    | (1 << TYPE_SCROLL_WHEEL);

/// `BLOCK_IS_GLOBAL` and `BLOCK_HAS_SIGNATURE`, from `Block_private.h`.
const BLOCK_IS_GLOBAL: i32 = 1 << 28;
const BLOCK_HAS_SIGNATURE: i32 = 1 << 30;

/// The handler's type encoding: returns `id`, takes the block and an `id`, in
/// sixteen bytes of arguments.
const SIGNATURE: &[u8] = b"@16@?0@8\0";

/// A block literal with no captures.
#[repr(C)]
struct Block {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: extern "C" fn(*mut c_void, Id) -> Id,
    descriptor: *const Descriptor,
}

/// Its descriptor. No copy or dispose helpers: nothing is captured, so there is
/// nothing to copy.
#[repr(C)]
struct Descriptor {
    reserved: c_ulong,
    size: c_ulong,
    signature: *const u8,
}

// SAFETY: both structures are plain data whose pointers are to process-lifetime
// objects — a class object from libSystem, a leaked descriptor, and a `'static`
// byte string. They are written once, before publication, and only read after.
unsafe impl Sync for Block {}
unsafe impl Sync for Descriptor {}


/// Whether the overlay is taking the mouse.
static FOCUSED: AtomicBool = AtomicBool::new(false);

/// How far the panel's contents are scrolled, and where the panel sits.
static STATE: Mutex<State> = Mutex::new(State {
    scroll: 0.0,
    x: 12.0,
    y: 12.0,
    width: 0.0,
    height: 0.0,
    reach: 0.0,
    rect: (0.0, 0.0, 0.0, 0.0),
    holding: None,
});

struct State {
    scroll: f32,
    x: f32,
    y: f32,
    /// The size the panel has been dragged to, or zero for "not yet sized".
    ///
    /// Zero rather than an `Option` because the panel is the only thing that can
    /// work out a sensible first size — it depends on the font and the surface —
    /// and it does that on its first layout through [`size_defaults`].
    width: f32,
    height: f32,
    /// How far the panel *can* be scrolled, which only the panel knows — it is the
    /// content's height less the room there is for it.
    reach: f32,
    /// Where the panel was last actually drawn, as `(x, y, w, h)`.
    rect: (f32, f32, f32, f32),
    /// What the current drag is doing, decided once when the button went down.
    holding: Option<Grip>,
}

/// What part of the panel a press landed on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grip {
    /// The body: the whole panel moves.
    Move,
    /// An edge or a corner: the panel resizes, in the directions named.
    Edge {
        left: bool,
        right: bool,
        top: bool,
        bottom: bool,
    },
}

/// How close to an edge counts as grabbing it.
const GRIP: f32 = 8.0;

/// The smallest the panel may be dragged to.
const MIN_WIDTH: f32 = 120.0;
const MIN_HEIGHT: f32 = 48.0;

/// Whether the mouse belongs to the overlay right now.
pub fn focused() -> bool {
    FOCUSED.load(Relaxed)
}

/// Take the mouse, or give it back. Bound to shift+F8.
pub fn toggle_focus() -> bool {
    let focused = !FOCUSED.fetch_xor(true, Relaxed);
    if focused {
        install();
    }
    focused
}

/// Where the panel wants to be, how big, and how far scrolled.
pub fn view() -> (f32, f32, f32, f32, f32) {
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    (state.x, state.y, state.width, state.height, state.scroll)
}

/// Give the panel a size, if it has never had one.
///
/// The panel works this out from its font and the surface on its first layout;
/// after that the numbers belong to whoever last dragged an edge.
pub fn size_defaults(width: f32, height: f32) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.width <= 0.0 {
        state.width = width;
    }
    if state.height <= 0.0 {
        state.height = height;
    }
}

/// Record where the panel actually ended up, for hit testing.
pub fn set_rect(x: f32, y: f32, width: f32, height: f32) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.rect = (x, y, width, height);
}

/// Tell the input layer how much there is to scroll through.
pub fn set_reach(reach: f32) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.reach = reach.max(0.0);
    state.scroll = state.scroll.clamp(0.0, state.reach);
}

impl State {
    /// Which part of the panel `(x, y)` is over, if any.
    fn grip_at(&self, x: f32, y: f32) -> Option<Grip> {
        let (px, py, pw, ph) = self.rect;
        if pw <= 0.0 || ph <= 0.0 {
            return None;
        }
        // A margin outside the panel counts too, so that the edge of a panel
        // flush against the window edge is still grabbable.
        if x < px - GRIP || x > px + pw + GRIP || y < py - GRIP || y > py + ph + GRIP {
            return None;
        }
        let (left, right) = (x <= px + GRIP, x >= px + pw - GRIP);
        let (top, bottom) = (y <= py + GRIP, y >= py + ph - GRIP);
        match left || right || top || bottom {
            true => Some(Grip::Edge {
                left,
                right,
                top,
                bottom,
            }),
            false => Some(Grip::Move),
        }
    }

    /// Apply one drag step.
    fn drag(&mut self, dx: f32, dy: f32) {
        match self.holding {
            Some(Grip::Move) => {
                self.x += dx;
                self.y += dy;
            }
            Some(Grip::Edge {
                left,
                right,
                top,
                bottom,
            }) => {
                // A left or top edge moves the origin and takes the same amount
                // off the size, which is what makes the *opposite* edge stay
                // where it is — the property that distinguishes resizing from
                // moving-and-growing.
                if left {
                    let width = (self.width - dx).max(MIN_WIDTH);
                    self.x += self.width - width;
                    self.width = width;
                }
                if right {
                    self.width = (self.width + dx).max(MIN_WIDTH);
                }
                if top {
                    let height = (self.height - dy).max(MIN_HEIGHT);
                    self.y += self.height - height;
                    self.height = height;
                }
                if bottom {
                    self.height = (self.height + dy).max(MIN_HEIGHT);
                }
            }
            None => {}
        }
    }
}


/// The handler, called by AppKit for every event in [`MASK`].
///
/// Returns the event to let it through, or nil to swallow it. Never unwinds.
extern "C" fn on_event(_block: *mut c_void, event: Id) -> Id {
    if event.is_null() || !focused() {
        return event;
    }
    let mut consumed = false;
    super::guarded(|| consumed = handle(event));
    match consumed {
        true => std::ptr::null_mut(),
        false => event,
    }
}

/// Where the pointer is, in the pixels the draw list uses.
fn cursor(event: Id) -> Option<(f32, f32)> {
    let sel = super::objc::sel;
    // SAFETY: each is a documented `NSEvent`, `NSWindow` or `NSView` method
    // with the prototype its header gives. `NSPoint` and `NSRect` are
    // homogeneous aggregates of `CGFloat`, which AAPCS64 returns in the vector
    // registers — Rust's `extern "C"` does that correctly for a `repr(C)`
    // struct of `f64`, which is why there is no `_stret` question here.
    unsafe {
        let window = send0::<Id>(event, sel("window"));
        if window.is_null() {
            return None;
        }
        let view = send0::<Id>(window, sel("contentView"));
        if view.is_null() {
            return None;
        }
        let location = send0::<sys::Point>(event, sel("locationInWindow"));
        let in_view = send2::<sys::Point, sys::Point, Id>(
            view,
            sel("convertPoint:fromView:"),
            location,
            std::ptr::null_mut(),
        );
        let backing = send1::<sys::Point, sys::Point>(view, sel("convertPointToBacking:"), in_view);
        let bounds = send0::<sys::Rect>(view, sel("bounds"));
        let bounds_backing =
            send1::<sys::Rect, sys::Rect>(view, sel("convertRectToBacking:"), bounds);
        Some((
            backing.x as f32,
            (bounds_backing.size.height - backing.y) as f32,
        ))
    }
}

/// Act on one event. Returns whether the guest should be denied it.
fn handle(event: Id) -> bool {
    // SAFETY: `-type` is `NSEvent`'s, takes nothing and returns an
    // `NSEventType`, which is an `NSUInteger`. The receiver is the event AppKit
    // handed to the monitor.
    let kind = unsafe { send0::<u64>(event, super::objc::sel("type")) };
    let at = cursor(event);
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());

    // A press decides what the drag is; a release ends it.
    match kind {
        TYPE_LEFT_MOUSE_DOWN => {
            state.holding = at.and_then(|(x, y)| state.grip_at(x, y));
            return state.holding.is_some();
        }
        TYPE_LEFT_MOUSE_UP => {
            return state.holding.take().is_some();
        }
        TYPE_LEFT_MOUSE_DRAGGED if state.holding.is_none() => return false,
        // A wheel outside the panel belongs to the guest.
        TYPE_SCROLL_WHEEL if !at.is_some_and(|(x, y)| state.grip_at(x, y).is_some()) => {
            return false;
        }
        _ => {}
    }

    match kind {
        TYPE_SCROLL_WHEEL => {
            // SAFETY: `-scrollingDeltaY` and `-hasPreciseScrollingDeltas` are
            // `NSEvent`'s, taking nothing and returning a `CGFloat` and a
            // `BOOL`.
            let (delta, precise) = unsafe {
                (
                    send0::<f64>(event, super::objc::sel("scrollingDeltaY")) as f32,
                    send0::<u8>(event, super::objc::sel("hasPreciseScrollingDeltas")) != 0,
                )
            };
            // A precise delta is in points and a coarse one is in *lines*, and
            // treating them alike makes a trackpad a hundred times faster than
            // a wheel.
            let row = ROW_HEIGHT.load(Relaxed).max(1) as f32;
            let pixels = match precise {
                true => delta * row / 14.0,
                false => delta * row * 3.0,
            };
            // Scrolling *down* — a negative delta — reveals content further
            // down, which is a larger offset.
            state.scroll = (state.scroll - pixels).clamp(0.0, state.reach);
            true
        }
        TYPE_LEFT_MOUSE_DRAGGED => {
            // SAFETY: `-deltaX` and `-deltaY` are `NSEvent`'s, taking nothing
            // and returning `CGFloat`. Deltas rather than the absolute cursor,
            // even though [`cursor`] now provides one: a drag driven by deltas
            // cannot drift away from where the grab started, and a resize driven
            // by absolute positions would jump the first time a conversion was
            // a pixel out.
            let (dx, dy) = unsafe {
                (
                    send0::<f64>(event, super::objc::sel("deltaX")) as f32,
                    send0::<f64>(event, super::objc::sel("deltaY")) as f32,
                )
            };
            // The deltas are in points and the panel is in pixels. The ratio is
            // whatever the drawable's is, which the row height carries.
            let scale = (ROW_HEIGHT.load(Relaxed) as f32 / 16.0).clamp(1.0, 3.0);
            state.drag(dx * scale, dy * scale);
            true
        }
        _ => false,
    }
}

/// The panel's row height in pixels, so the wheel can scroll in rows.
static ROW_HEIGHT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(16);

/// Tell the input layer how tall a row is, for the wheel's benefit.
pub fn set_row_height(pixels: f32) {
    ROW_HEIGHT.store(pixels.max(1.0) as u32, Relaxed);
}

/// Put the monitor in, once, from the main thread.
fn install() {
    static ASKED: AtomicBool = AtomicBool::new(false);
    if ASKED.swap(true, Relaxed) {
        return;
    }
    type AsyncF = unsafe extern "C" fn(*mut c_void, *mut c_void, extern "C" fn(*mut c_void));
    // SAFETY: `dispatch_async_f`'s prototype from `<dispatch/queue.h>`, and
    // `_dispatch_main_q` is the object `dispatch_get_main_queue()` is a macro
    // for — its *address* is the queue.
    let (Some(async_f), Some(main_queue)) = (unsafe {
        (
            sys::function::<AsyncF>("dispatch_async_f"),
            sys::symbol("_dispatch_main_q"),
        )
    }) else {
        super::say_once("no libdispatch: the overlay cannot take the mouse");
        return;
    };
    // SAFETY: a real queue and an `extern "C"` function of the right shape. The
    // context is null because `install_on_main` captures nothing.
    unsafe { async_f(main_queue, std::ptr::null_mut(), install_on_main) };
}

/// The half that has to be on the main thread.
extern "C" fn install_on_main(_context: *mut c_void) {
    super::guarded(|| {
        let class = super::objc::class("NSEvent");
        if class.is_null() {
            super::say_once("no NSEvent: this guest has not loaded AppKit, so there is no mouse");
            return;
        }
        let descriptor = Box::leak(Box::new(Descriptor {
            reserved: 0,
            size: size_of::<Block>() as c_ulong,
            signature: SIGNATURE.as_ptr(),
        }));
        let Some(isa) = sys::symbol("_NSConcreteGlobalBlock") else {
            super::say_once("no _NSConcreteGlobalBlock: the mouse handler cannot be built");
            return;
        };
        let block = Box::leak(Box::new(Block {
            isa,
            flags: BLOCK_IS_GLOBAL | BLOCK_HAS_SIGNATURE,
            reserved: 0,
            invoke: on_event,
            descriptor,
        }));
        // SAFETY: `+addLocalMonitorForEventsMatchingMask:handler:` is
        // `NSEvent`'s, takes an `NSEventMask` and a block, and returns the
        // monitor object. The block is a correctly formed global block, leaked,
        // so it outlives the monitor — which is never removed, because the
        // overlay lives as long as the process.
        let monitor = unsafe {
            send2::<Id, u64, *const Block>(
                class,
                super::objc::sel("addLocalMonitorForEventsMatchingMask:handler:"),
                MASK,
                block,
            )
        };
        match monitor.is_null() {
            true => super::say_once("AppKit refused the overlay's mouse monitor"),
            false => eprintln!(
                "rosetta3: gui  the overlay has the mouse: scroll to scroll, drag to move"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> State {
        State {
            scroll: 0.0,
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
            reach: 0.0,
            rect: (100.0, 100.0, 400.0, 300.0),
            holding: None,
        }
    }

    /// Where a press lands decides what a drag does, and the boundaries are the whole
    /// of it.
    #[test]
    fn a_press_is_classified_by_where_it_lands() {
        let state = panel();
        assert!(
            state.grip_at(300.0, 250.0) == Some(Grip::Move),
            "the middle"
        );
        assert!(state.grip_at(1000.0, 1000.0).is_none(), "well outside");

        let Some(Grip::Edge { right, bottom, .. }) = state.grip_at(499.0, 399.0) else {
            panic!("the bottom-right corner is not a resize grip");
        };
        assert!(right && bottom, "the corner resizes in one direction only");

        let Some(Grip::Edge {
            left,
            top,
            right,
            bottom,
        }) = state.grip_at(101.0, 250.0)
        else {
            panic!("the left edge is not a resize grip");
        };
        assert!(left && !right && !top && !bottom, "the left edge alone");
    }

    /// **The property that makes a resize a resize.** Dragging the left edge
    /// must leave the *right* edge exactly where it was; a version that only
    /// changed the width would move the panel instead, and a version that only
    /// changed the origin would drag it.
    #[test]
    fn resizing_an_edge_leaves_the_opposite_edge_alone() {
        for (grip, dx, dy) in [
            (
                Grip::Edge {
                    left: true,
                    right: false,
                    top: false,
                    bottom: false,
                },
                40.0,
                0.0,
            ),
            (
                Grip::Edge {
                    left: false,
                    right: false,
                    top: true,
                    bottom: false,
                },
                0.0,
                40.0,
            ),
        ] {
            let mut state = panel();
            let (right, bottom) = (state.x + state.width, state.y + state.height);
            state.holding = Some(grip);
            state.drag(dx, dy);
            assert!(
                (state.x + state.width - right).abs() < 1e-3,
                "the right edge moved from {right} to {}",
                state.x + state.width,
            );
            assert!(
                (state.y + state.height - bottom).abs() < 1e-3,
                "the bottom edge moved from {bottom} to {}",
                state.y + state.height,
            );
        }
    }

    /// A panel cannot be dragged smaller than it can be grabbed again.
    #[test]
    fn a_resize_stops_at_the_minimum() {
        let mut state = panel();
        state.holding = Some(Grip::Edge {
            left: false,
            right: true,
            top: false,
            bottom: true,
        });
        state.drag(-10_000.0, -10_000.0);
        assert_eq!(state.width, MIN_WIDTH);
        assert_eq!(state.height, MIN_HEIGHT);
    }

    /// Moving changes neither dimension.
    #[test]
    fn moving_does_not_resize() {
        let mut state = panel();
        state.holding = Some(Grip::Move);
        state.drag(25.0, -15.0);
        assert_eq!((state.x, state.y), (125.0, 85.0));
        assert_eq!((state.width, state.height), (400.0, 300.0));
    }
}
