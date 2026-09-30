//! Mac keys as the Linux evdev codes GTK's keyboard reads.
//!
//! The page gets physical keys, and the compositor's xkb keymap turns them
//! into characters, so a web view types in that keymap's layout.

use appkit::keymap::flags;

/// `KEY_LEFTCTRL`, which Command becomes: GTK's and WebKitGTK's editing
/// shortcuts are Control ones, and a Mac user reaches them with Command.
const COMMAND_LEFT: u32 = 29;
const COMMAND_RIGHT: u32 = 97;
/// `KEY_LEFTMETA`, which Control becomes, so that it does not collide with
/// Command.
const CONTROL_LEFT: u32 = 125;
const CONTROL_RIGHT: u32 = 126;
const CAPS_LOCK: u32 = 58;

/// The evdev code of a Mac virtual key (`kVK_*`), or `None` for a key Linux
/// has no counterpart for.
pub fn evdev(mac_key_code: u16) -> Option<u32> {
    Some(match mac_key_code {
        0x00 => 30, // A
        0x01 => 31, // S
        0x02 => 32, // D
        0x03 => 33, // F
        0x04 => 35, // H
        0x05 => 34, // G
        0x06 => 44, // Z
        0x07 => 45, // X
        0x08 => 46, // C
        0x09 => 47, // V
        0x0A => 86, // ISO section: KEY_102ND
        0x0B => 48, // B
        0x0C => 16, // Q
        0x0D => 17, // W
        0x0E => 18, // E
        0x0F => 19, // R
        0x10 => 21, // Y
        0x11 => 20, // T
        0x12 => 2,  // 1
        0x13 => 3,  // 2
        0x14 => 4,  // 3
        0x15 => 5,  // 4
        0x16 => 7,  // 6
        0x17 => 6,  // 5
        0x18 => 13, // =
        0x19 => 10, // 9
        0x1A => 8,  // 7
        0x1B => 12, // -
        0x1C => 9,  // 8
        0x1D => 11, // 0
        0x1E => 27, // ]
        0x1F => 24, // O
        0x20 => 22, // U
        0x21 => 26, // [
        0x22 => 23, // I
        0x23 => 25, // P
        0x24 => 28, // Return
        0x25 => 38, // L
        0x26 => 36, // J
        0x27 => 40, // '
        0x28 => 37, // K
        0x29 => 39, // ;
        0x2A => 43, // backslash
        0x2B => 51, // ,
        0x2C => 53, // /
        0x2D => 49, // N
        0x2E => 50, // M
        0x2F => 52, // .
        0x30 => 15, // Tab
        0x31 => 57, // Space
        0x32 => 41, // `
        0x33 => 14, // Delete: KEY_BACKSPACE
        0x34 => 96, // PowerBook Enter: KEY_KPENTER
        0x35 => 1,  // Escape
        0x36 => COMMAND_RIGHT,
        0x37 => COMMAND_LEFT,
        0x38 => 42, // Shift
        0x39 => CAPS_LOCK,
        0x3A => 56, // Option: KEY_LEFTALT
        0x3B => CONTROL_LEFT,
        0x3C => 54,  // right Shift
        0x3D => 100, // right Option: KEY_RIGHTALT
        0x3E => CONTROL_RIGHT,
        0x40 => 187, // F17
        0x41 => 83,  // keypad .
        0x43 => 55,  // keypad *
        0x45 => 78,  // keypad +
        0x47 => 69,  // keypad Clear: KEY_NUMLOCK
        0x48 => 115, // volume up
        0x49 => 114, // volume down
        0x4A => 113, // mute
        0x4B => 98,  // keypad /
        0x4C => 96,  // keypad Enter
        0x4E => 74,  // keypad -
        0x4F => 188, // F18
        0x50 => 189, // F19
        0x51 => 117, // keypad =
        0x52 => 82,  // keypad 0
        0x53 => 79,  // keypad 1
        0x54 => 80,  // keypad 2
        0x55 => 81,  // keypad 3
        0x56 => 75,  // keypad 4
        0x57 => 76,  // keypad 5
        0x58 => 77,  // keypad 6
        0x59 => 71,  // keypad 7
        0x5A => 190, // F20
        0x5B => 72,  // keypad 8
        0x5C => 73,  // keypad 9
        0x5D => 124, // JIS yen
        0x5E => 89,  // JIS underscore: KEY_RO
        0x5F => 121, // JIS keypad comma
        0x60 => 63,  // F5
        0x61 => 64,  // F6
        0x62 => 65,  // F7
        0x63 => 61,  // F3
        0x64 => 66,  // F8
        0x65 => 67,  // F9
        0x66 => 94,  // JIS eisu: KEY_MUHENKAN
        0x67 => 87,  // F11
        0x68 => 92,  // JIS kana: KEY_HENKAN
        0x69 => 183, // F13
        0x6A => 186, // F16
        0x6B => 184, // F14
        0x6D => 68,  // F10
        0x6F => 88,  // F12
        0x71 => 185, // F15
        0x72 => 110, // Help, where a PC keyboard has Insert
        0x73 => 102, // Home
        0x74 => 104, // Page Up
        0x75 => 111, // forward delete: KEY_DELETE
        0x76 => 62,  // F4
        0x77 => 107, // End
        0x78 => 60,  // F2
        0x79 => 109, // Page Down
        0x7A => 59,  // F1
        0x7B => 105, // left
        0x7C => 106, // right
        0x7D => 108, // down
        0x7E => 103, // up
        _ => return None,
    })
}

/// The modifier keys `modifier_flags` says are held, by the side-specific
/// bits AppKit reports beside the usual ones.
pub fn held_modifiers(modifier_flags: u64) -> Vec<u32> {
    [
        (flags::DEVICE_LSHIFT, 42),
        (flags::DEVICE_RSHIFT, 54),
        (flags::DEVICE_LCTL, CONTROL_LEFT),
        (flags::DEVICE_RCTL, CONTROL_RIGHT),
        (flags::DEVICE_LALT, 56),
        (flags::DEVICE_RALT, 100),
        (flags::DEVICE_LCMD, COMMAND_LEFT),
        (flags::DEVICE_RCMD, COMMAND_RIGHT),
    ]
    .into_iter()
    .filter(|(bit, _)| modifier_flags & bit != 0)
    .map(|(_, key)| key)
    .collect()
}

/// The key presses and releases that take the keyboard from the modifiers
/// in `before` to those in `after`: `(evdev, pressed)`.
///
/// Caps Lock toggles, so a change in its flag is one press and release.
pub fn modifier_changes(before: u64, after: u64) -> Vec<(u32, bool)> {
    let (was, now) = (held_modifiers(before), held_modifiers(after));
    let mut changes: Vec<(u32, bool)> = was
        .iter()
        .filter(|key| !now.contains(key))
        .map(|key| (*key, false))
        .collect();
    changes.extend(
        now.iter()
            .filter(|key| !was.contains(key))
            .map(|key| (*key, true)),
    );
    if (before ^ after) & flags::CAPS_LOCK != 0 {
        changes.extend([(CAPS_LOCK, true), (CAPS_LOCK, false)]);
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_keys_map_to_their_evdev_positions() {
        assert_eq!(evdev(0x00), Some(30));
        assert_eq!(evdev(0x33), Some(14));
        assert_eq!(evdev(0x7E), Some(103));
        assert_eq!(evdev(0x3F), None);
    }

    #[test]
    fn command_is_control_and_control_is_meta() {
        assert_eq!(evdev(0x37), Some(29));
        assert_eq!(evdev(0x3B), Some(125));
    }

    #[test]
    fn modifier_changes_press_and_release_by_side() {
        let command = flags::COMMAND | flags::DEVICE_LCMD;
        let shift = flags::SHIFT | flags::DEVICE_RSHIFT;
        assert_eq!(modifier_changes(0, command), [(29, true)]);
        assert_eq!(modifier_changes(command, shift), [(29, false), (54, true)]);
        assert_eq!(
            modifier_changes(0, flags::CAPS_LOCK),
            [(58, true), (58, false)]
        );
    }
}
