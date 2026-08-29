//! SDL scancodes as macOS virtual key codes.
//!
//! A guest reads `-[NSEvent keyCode]` and looks the number up in a table of its
//! own - Deltarune's `getVirtualKeyEquivalent:` is exactly that - so the number
//! has to be the one Apple's `kVK_` constants name and not a number this
//! runtime finds convenient. They are the old ADB codes and have no pattern:
//! `A` is 0, `S` is 1, `Z` is 6.
//!
//! SDL's scancodes are the USB HID keyboard usage IDs, which do have a pattern
//! and are not these. The table below is the join, and it is the whole module.

/// `SDL_Scancode` -> `kVK_*`, for every key a keyboard has that this tree can
/// name. A scancode absent from it has no macOS code, and the event carries
/// [`UNKNOWN`] rather than a neighbour's.
static VIRTUAL_KEYS: &[(u32, u16)] = &[
    // Letters, in USB order, which is alphabetical.
    (4, 0x00),  // a
    (5, 0x0B),  // b
    (6, 0x08),  // c
    (7, 0x02),  // d
    (8, 0x0E),  // e
    (9, 0x03),  // f
    (10, 0x05), // g
    (11, 0x04), // h
    (12, 0x22), // i
    (13, 0x26), // j
    (14, 0x28), // k
    (15, 0x25), // l
    (16, 0x2E), // m
    (17, 0x2D), // n
    (18, 0x1F), // o
    (19, 0x23), // p
    (20, 0x0C), // q
    (21, 0x0F), // r
    (22, 0x01), // s
    (23, 0x11), // t
    (24, 0x20), // u
    (25, 0x09), // v
    (26, 0x0D), // w
    (27, 0x07), // x
    (28, 0x10), // y
    (29, 0x06), // z
    // The number row, `1` through `0`.
    (30, 0x12),
    (31, 0x13),
    (32, 0x14),
    (33, 0x15),
    (34, 0x17),
    (35, 0x16),
    (36, 0x1A),
    (37, 0x1C),
    (38, 0x19),
    (39, 0x1D),
    (40, 0x24), // return
    (41, 0x35), // escape
    (42, 0x33), // backspace
    (43, 0x30), // tab
    (44, 0x31), // space
    (45, 0x1B), // minus
    (46, 0x18), // equals
    (47, 0x21), // left bracket
    (48, 0x1E), // right bracket
    (49, 0x2A), // backslash
    (51, 0x29), // semicolon
    (52, 0x27), // apostrophe
    (53, 0x32), // grave
    (54, 0x2B), // comma
    (55, 0x2F), // period
    (56, 0x2C), // slash
    (57, 0x39), // caps lock
    // The function keys, which are not consecutive in ADB order.
    (58, 0x7A), // F1
    (59, 0x78),
    (60, 0x63),
    (61, 0x76),
    (62, 0x60),
    (63, 0x61),
    (64, 0x62),
    (65, 0x64),
    (66, 0x65),
    (67, 0x6D),
    (68, 0x67),
    (69, 0x6F), // F12
    (73, 0x72), // insert, which macOS spells help
    (74, 0x73), // home
    (75, 0x74), // page up
    (76, 0x75), // forward delete
    (77, 0x77), // end
    (78, 0x79), // page down
    (79, 0x7C), // right
    (80, 0x7B), // left
    (81, 0x7D), // down
    (82, 0x7E), // up
    (83, 0x47), // num lock, which macOS spells clear
    (84, 0x4B), // keypad divide
    (85, 0x43), // keypad multiply
    (86, 0x4E), // keypad minus
    (87, 0x45), // keypad plus
    (88, 0x4C), // keypad enter
    (89, 0x53), // keypad 1
    (90, 0x54),
    (91, 0x55),
    (92, 0x56),
    (93, 0x57),
    (94, 0x58),
    (95, 0x59),
    (96, 0x5B),
    (97, 0x5C), // keypad 9
    (98, 0x52), // keypad 0
    (99, 0x41), // keypad period
    (103, 0x51), // keypad equals
    (224, 0x3B), // left control
    (225, 0x38), // left shift
    (226, 0x3A), // left option
    (227, 0x37), // left command
    (228, 0x3E), // right control
    (229, 0x3C), // right shift
    (230, 0x3D), // right option
    (231, 0x36), // right command
];

/// The key code an event carries for a scancode nothing here names.
///
/// Not zero: zero is `A`, and a key that pressed `A` because this table did not
/// know it would be a bug with nothing to say so.
pub const UNKNOWN: u16 = 0xFFFF;

pub fn virtual_key(scancode: u32) -> u16 {
    VIRTUAL_KEYS
        .iter()
        .find(|(sdl, _)| *sdl == scancode)
        .map_or(UNKNOWN, |(_, vk)| *vk)
}

/// `SDL_Keymod` bits, which are not `NSEventModifierFlags` bits.
mod sdl {
    pub const SHIFT: u16 = 0x0001 | 0x0002;
    pub const CTRL: u16 = 0x0040 | 0x0080;
    pub const ALT: u16 = 0x0100 | 0x0200;
    pub const GUI: u16 = 0x0400 | 0x0800;
    pub const CAPS: u16 = 0x2000;
    pub const NUM: u16 = 0x1000;
}

/// `NSEventModifierFlags`.
pub mod flags {
    pub const CAPS_LOCK: u64 = 1 << 16;
    pub const SHIFT: u64 = 1 << 17;
    pub const CONTROL: u64 = 1 << 18;
    pub const OPTION: u64 = 1 << 19;
    pub const COMMAND: u64 = 1 << 20;
    pub const NUMERIC_PAD: u64 = 1 << 21;
    pub const FUNCTION: u64 = 1 << 23;
}

pub fn modifiers(sdl_modifiers: u16) -> u64 {
    let mut out = 0;
    for (from, to) in [
        (sdl::SHIFT, flags::SHIFT),
        (sdl::CTRL, flags::CONTROL),
        (sdl::ALT, flags::OPTION),
        (sdl::GUI, flags::COMMAND),
        (sdl::CAPS, flags::CAPS_LOCK),
        (sdl::NUM, flags::NUMERIC_PAD),
    ] {
        if sdl_modifiers & from != 0 {
            out |= to;
        }
    }
    out
}

/// The text `-characters` reports, from SDL's keycode.
///
/// SDL's keycode for a printable key *is* its Unicode scalar, so those need no
/// table. The rest are the private-use scalars AppKit puts in `-characters` for
/// a function key, because a guest walking the string sees the same length and
/// the same values it would on Apple's - and a key with neither is the empty
/// string, which is what AppKit gives for a modifier.
pub fn characters(scancode: u32, keycode: u32) -> String {
    if let Some(c) = char::from_u32(keycode).filter(|c| !c.is_control()) {
        return c.to_string();
    }
    let private = match scancode {
        82 => 0xF700,  // up
        81 => 0xF701,  // down
        80 => 0xF702,  // left
        79 => 0xF703,  // right
        74 => 0xF729,  // home
        77 => 0xF72B,  // end
        75 => 0xF72C,  // page up
        78 => 0xF72D,  // page down
        76 => 0xF728,  // forward delete
        73 => 0xF727,  // insert
        58..=69 => 0xF704 + (scancode - 58), // F1 through F12
        40 | 88 => 0x000D,                   // return and keypad enter
        42 => 0x0008,                        // backspace
        41 => 0x001B,                        // escape
        43 => 0x0009,                        // tab
        _ => return String::new(),
    };
    char::from_u32(private).map(String::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two keys with the same virtual code would be one key as far as a guest's
    /// lookup table is concerned.
    #[test]
    fn no_two_scancodes_share_a_virtual_key() {
        let mut seen: Vec<u16> = VIRTUAL_KEYS.iter().map(|(_, vk)| *vk).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "a virtual key code appears twice");
    }

    /// The four Apple documents by name, as the anchor for the rest.
    #[test]
    fn the_documented_codes_are_the_documented_codes() {
        assert_eq!(virtual_key(4), 0x00, "kVK_ANSI_A");
        assert_eq!(virtual_key(29), 0x06, "kVK_ANSI_Z");
        assert_eq!(virtual_key(44), 0x31, "kVK_Space");
        assert_eq!(virtual_key(82), 0x7E, "kVK_UpArrow");
    }

    #[test]
    fn a_scancode_with_no_macos_code_is_not_the_a_key() {
        assert_eq!(virtual_key(0), UNKNOWN);
        assert_eq!(virtual_key(400), UNKNOWN);
    }

    #[test]
    fn characters_are_the_scalar_for_text_and_apples_private_use_for_the_rest() {
        assert_eq!(characters(4, 'a' as u32), "a");
        assert_eq!(characters(82, 0x40000052), "\u{F700}", "up arrow");
        assert_eq!(characters(225, 0x400000E1), "", "a modifier types nothing");
    }

    #[test]
    fn sdl_modifiers_become_appkit_flags() {
        assert_eq!(modifiers(0x0002), flags::SHIFT, "right shift is shift");
        assert_eq!(modifiers(0x0040 | 0x0100), flags::CONTROL | flags::OPTION);
        assert_eq!(modifiers(0), 0);
    }
}
