//! Where AppKit's coordinates are and where SDL's are.
//!
//! AppKit measures a window from the bottom-left of the primary display with
//! y increasing upwards. SDL measures from the top-left of the desktop with y
//! increasing downwards. Nothing warns about the difference: a window placed
//! without converting appears mirrored about the middle of the screen, and a
//! mouse coordinate read without converting is wrong by twice its distance from
//! the centre - both of which look like a program bug rather than a runtime one.

use foundation::geometry::{NSPoint, NSRect, NSSize};

/// The height of the primary display, which is the origin AppKit measures from.
///
/// Zero when there is no display - a headless host, or SDL absent - and a zero
/// here makes the conversion the identity, which is the only answer that cannot
/// be mistaken for a real position.
pub fn primary_height() -> f64 {
    sdl3::video::display_bounds(sdl3::video::primary_display()).map_or(0.0, |b| b.h as f64)
}

/// An SDL rectangle, top-left and y down, as an AppKit one.
pub fn rect_from_sdl(x: i32, y: i32, width: i32, height: i32) -> NSRect {
    NSRect {
        origin: NSPoint {
            x: x as f64,
            y: primary_height() - (y as f64) - (height as f64),
        },
        size: NSSize {
            width: width as f64,
            height: height as f64,
        },
    }
}

/// The reverse: an AppKit rectangle as SDL's `(x, y, width, height)`.
///
/// The same expression as [`rect_from_sdl`], which is not a coincidence - the
/// transform is its own inverse - and is written out rather than shared so that
/// each direction reads as what it does.
pub fn rect_to_sdl(rect: NSRect) -> (i32, i32, i32, i32) {
    let y = primary_height() - rect.origin.y - rect.size.height;
    (
        rect.origin.x as i32,
        y as i32,
        rect.size.width as i32,
        rect.size.height as i32,
    )
}

/// A desktop point as an AppKit screen point.
pub fn point_from_sdl(x: f64, y: f64) -> NSPoint {
    NSPoint {
        x,
        y: primary_height() - y,
    }
}

/// A point inside a window, from SDL's top-left frame to AppKit's bottom-left.
///
/// `height` is the window's, in points: this is the conversion `-locationInWindow`
/// needs, and it is relative to the window rather than to the screen.
pub fn point_in_window(x: f64, y: f64, height: f64) -> NSPoint {
    NSPoint { x, y: height - y }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion is its own inverse, whatever the display height is -
    /// which is the property that makes a round trip through SDL and back leave
    /// a window where it started.
    #[test]
    fn a_rectangle_survives_the_round_trip() {
        let rect = NSRect {
            origin: NSPoint { x: 100.0, y: 200.0 },
            size: NSSize {
                width: 640.0,
                height: 480.0,
            },
        };
        let (x, y, w, h) = rect_to_sdl(rect);
        assert_eq!(rect_from_sdl(x, y, w, h), rect);
    }

    /// And a point at the bottom of the screen is at the top of SDL's.
    #[test]
    fn the_origin_is_the_other_corner() {
        let height = primary_height();
        assert_eq!(point_from_sdl(0.0, height), NSPoint { x: 0.0, y: 0.0 });
    }
}
