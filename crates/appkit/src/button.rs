//! SDL mouse buttons as AppKit's.

use crate::nsevent::kind;
use sdl3::mouse;

/// `-[NSEvent buttonNumber]` for an SDL button.
///
/// A button past the fifth is SDL's alone; it keeps SDL's order there, since
/// AppKit has no name for it either and the guest's own numbering is all that
/// is left.
pub fn number(button: u8) -> i64 {
    match button {
        mouse::LEFT => 0,
        mouse::RIGHT => 1,
        mouse::MIDDLE => 2,
        other => i64::from(other) - 1,
    }
}

/// The `NSEventType` a press or release of this button is.
pub fn event_kind(button: u8, down: bool) -> u64 {
    match (button, down) {
        (mouse::LEFT, true) => kind::LEFT_MOUSE_DOWN,
        (mouse::LEFT, false) => kind::LEFT_MOUSE_UP,
        (mouse::RIGHT, true) => kind::RIGHT_MOUSE_DOWN,
        (mouse::RIGHT, false) => kind::RIGHT_MOUSE_UP,
        (_, true) => kind::OTHER_MOUSE_DOWN,
        (_, false) => kind::OTHER_MOUSE_UP,
    }
}

/// The `NSEventType` a move with this button held is, or `MOUSE_MOVED` for a
/// move with none.
pub fn dragged_kind(button: Option<u8>) -> u64 {
    match button {
        Some(mouse::LEFT) => kind::LEFT_MOUSE_DRAGGED,
        Some(mouse::RIGHT) => kind::RIGHT_MOUSE_DRAGGED,
        Some(_) => kind::OTHER_MOUSE_DRAGGED,
        None => kind::MOUSE_MOVED,
    }
}

/// The lowest button held in an `SDL_GetMouseState` mask, as SDL numbers them.
///
/// A drag carries one button, and AppKit's choice for several at once is the
/// first pressed; the mask does not record that order, so this takes the lowest
/// - which for the usual case of a single button is the same answer.
pub fn held(mask: u32) -> Option<u8> {
    (mouse::LEFT..=mouse::X2).find(|b| mask & mouse::mask(*b) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The swap this module exists for.
    #[test]
    fn appkits_right_button_is_one_and_its_middle_is_two() {
        assert_eq!(number(mouse::LEFT), 0);
        assert_eq!(number(mouse::RIGHT), 1);
        assert_eq!(number(mouse::MIDDLE), 2);
        assert_eq!(number(mouse::X1), 3);
        assert_eq!(number(mouse::X2), 4);
    }

    #[test]
    fn a_press_is_the_event_type_for_its_own_button() {
        assert_eq!(event_kind(mouse::LEFT, true), kind::LEFT_MOUSE_DOWN);
        assert_eq!(event_kind(mouse::RIGHT, false), kind::RIGHT_MOUSE_UP);
        assert_eq!(event_kind(mouse::MIDDLE, true), kind::OTHER_MOUSE_DOWN);
        assert_eq!(event_kind(mouse::X1, true), kind::OTHER_MOUSE_DOWN);
    }

    #[test]
    fn a_drag_is_the_event_type_for_the_held_button() {
        assert_eq!(dragged_kind(None), kind::MOUSE_MOVED);
        assert_eq!(dragged_kind(Some(mouse::LEFT)), kind::LEFT_MOUSE_DRAGGED);
        assert_eq!(dragged_kind(Some(mouse::RIGHT)), kind::RIGHT_MOUSE_DRAGGED);
        assert_eq!(dragged_kind(Some(mouse::MIDDLE)), kind::OTHER_MOUSE_DRAGGED);
    }

    /// The mask is by bit, and the bit is one less than the button.
    #[test]
    fn the_held_button_comes_out_of_sdls_mask() {
        assert_eq!(held(0), None);
        assert_eq!(held(mouse::mask(mouse::RIGHT)), Some(mouse::RIGHT));
        assert_eq!(
            held(mouse::mask(mouse::MIDDLE) | mouse::mask(mouse::X2)),
            Some(mouse::MIDDLE)
        );
    }
}
