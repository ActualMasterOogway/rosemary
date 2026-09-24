//! The `'uchr'` keyboard layout resource, and the translation of one.
//!
//! A guest asking for `kTISPropertyUnicodeKeyLayoutData` reads the bytes itself
//! as readily as it passes them back to `UCKeyTranslate`, so an opaque stand-in
//! is worse than nothing: the guest takes the "there is a layout" branch and
//! finds no tables in it. What is built here is therefore a real resource in the
//! format `<CarbonCore/UnicodeUtilities.h>` describes - US ANSI, the layout
//! [`crate::keymap`] already assumes, with the five dead keys the real one
//! carries.
//!
//! [`translate`] parses whatever layout it is handed rather than answering from
//! the tables below, so a guest that built its own resource, or copied this one,
//! gets what its own bytes say.

use std::sync::OnceLock;

/// `kUCKeyLayoutHeaderFormat` and the per-section format tags.
mod format {
    pub const HEADER: u16 = 0x1002;
    pub const FEATURE_INFO: u16 = 0x2001;
    pub const MODIFIERS_TO_TABLE_NUM: u16 = 0x3001;
    pub const KEY_TO_CHAR_TABLE_INDEX: u16 = 0x4001;
    pub const STATE_RECORDS_INDEX: u16 = 0x5001;
    pub const STATE_TERMINATORS: u16 = 0x6001;
    /// The terminal entry format is the only one written here; the range format
    /// exists for a layout whose states map whole runs of key codes, and is read
    /// because a guest may hand one over.
    pub const STATE_ENTRY_TERMINAL: u16 = 0x0001;
    pub const STATE_ENTRY_RANGE: u16 = 0x0002;
    pub const SEQUENCE_DATA_INDEX: u16 = 0x7001;
}

/// The `UCKeyOutput` tag bits. An entry whose bits 14-15 are `STATE_INDEX` is an
/// index into the state records, `SEQUENCE_INDEX` an index into the sequence
/// list, and anything else is the `UniChar` itself.
mod output {
    pub const STATE_INDEX: u16 = 0x4000;
    pub const SEQUENCE_INDEX: u16 = 0x8000;
    pub const TEST_FOR_INDEX: u16 = 0xC000;
    pub const GET_INDEX: u16 = 0x3FFF;
    /// `kUCKeyOutputNoOutput`, the two values that are not characters.
    pub const NONE: u16 = 0xFFFF;
    pub const NONE_ALT: u16 = 0xFFFE;
}

/// `UCKeyTranslate`'s `modifierKeyState`, which is the classic event modifiers
/// shifted down by eight.
pub mod modifier {
    pub const COMMAND: u8 = 0x01;
    pub const SHIFT: u8 = 0x02;
    pub const ALPHA_LOCK: u8 = 0x04;
    pub const OPTION: u8 = 0x08;
    pub const CONTROL: u8 = 0x10;
    pub const RIGHT_SHIFT: u8 = 0x20;
    pub const RIGHT_OPTION: u8 = 0x40;
    pub const RIGHT_CONTROL: u8 = 0x80;
}

/// `kUCKeyTranslateNoDeadKeysMask`, and the key action that means the same
/// thing: a caller displaying a key equivalent wants the accent, not a state.
pub const NO_DEAD_KEYS: u32 = 1;
pub const ACTION_DISPLAY: u16 = 3;

/// `paramErr` and `kUCOutputBufferTooSmall`.
pub const PARAM_ERR: i32 = -50;
pub const BUFFER_TOO_SMALL: i32 = -25340;

/// The number of virtual key codes a table describes. `kVK_` codes stop below
/// this, and a code at or above it has no entry rather than a neighbour's.
const TABLE_SIZE: usize = 128;

/// A key's output before offsets exist for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    None,
    Char(char),
    /// Enters the named dead-key state and emits nothing.
    Dead(u16),
    /// A character that also composes with a dead key, so it needs a state
    /// record rather than a bare `UniChar`.
    Compose(char),
}

/// The option-key column, which is the only one holding dead keys.
#[derive(Clone, Copy)]
enum Opt {
    Ch(char),
    Dead(u16),
}

use Opt::{Ch, Dead};

/// The dead-key states, numbered from one because zero means "no state".
mod dead {
    pub const ACUTE: u16 = 1;
    pub const GRAVE: u16 = 2;
    pub const CIRCUMFLEX: u16 = 3;
    pub const TILDE: u16 = 4;
    pub const DIAERESIS: u16 = 5;
}

/// What each dead state produces when the key after it does not compose - and
/// what the accent key itself produces with dead keys turned off. Indexed by
/// state minus one.
const TERMINATORS: [char; 5] = ['\u{00B4}', '\u{0060}', '\u{02C6}', '\u{02DC}', '\u{00A8}'];

/// `(virtual key, unshifted, shifted, option, shift+option)` for the keys that
/// type something. The option columns are US ANSI's, which is where the layout
/// stops being a rearrangement of ASCII and has to be transcribed.
#[rustfmt::skip]
static PRINTABLE: &[(u16, char, char, Opt, char)] = &[
    (vk::ANSI_A, 'a', 'A', Ch('å'), 'Å'),
    (vk::ANSI_B, 'b', 'B', Ch('∫'), 'ı'),
    (vk::ANSI_C, 'c', 'C', Ch('ç'), 'Ç'),
    (vk::ANSI_D, 'd', 'D', Ch('∂'), 'Î'),
    (vk::ANSI_E, 'e', 'E', Dead(dead::ACUTE), '\u{00B4}'),
    (vk::ANSI_F, 'f', 'F', Ch('ƒ'), 'Ï'),
    (vk::ANSI_G, 'g', 'G', Ch('©'), '\u{02DD}'),
    (vk::ANSI_H, 'h', 'H', Ch('\u{02D9}'), 'Ó'),
    (vk::ANSI_I, 'i', 'I', Dead(dead::CIRCUMFLEX), '\u{02C6}'),
    (vk::ANSI_J, 'j', 'J', Ch('∆'), 'Ô'),
    (vk::ANSI_K, 'k', 'K', Ch('\u{02DA}'), '\u{F8FF}'),
    (vk::ANSI_L, 'l', 'L', Ch('¬'), 'Ò'),
    (vk::ANSI_M, 'm', 'M', Ch('µ'), 'Â'),
    (vk::ANSI_N, 'n', 'N', Dead(dead::TILDE), '\u{02DC}'),
    (vk::ANSI_O, 'o', 'O', Ch('ø'), 'Ø'),
    (vk::ANSI_P, 'p', 'P', Ch('π'), '∏'),
    (vk::ANSI_Q, 'q', 'Q', Ch('œ'), 'Œ'),
    (vk::ANSI_R, 'r', 'R', Ch('®'), '‰'),
    (vk::ANSI_S, 's', 'S', Ch('ß'), 'Í'),
    (vk::ANSI_T, 't', 'T', Ch('†'), '\u{02C7}'),
    (vk::ANSI_U, 'u', 'U', Dead(dead::DIAERESIS), '\u{00A8}'),
    (vk::ANSI_V, 'v', 'V', Ch('√'), '◊'),
    (vk::ANSI_W, 'w', 'W', Ch('∑'), '„'),
    (vk::ANSI_X, 'x', 'X', Ch('≈'), '\u{02DB}'),
    (vk::ANSI_Y, 'y', 'Y', Ch('¥'), 'Á'),
    (vk::ANSI_Z, 'z', 'Z', Ch('Ω'), '¸'),
    (vk::ANSI_1, '1', '!', Ch('¡'), '⁄'),
    (vk::ANSI_2, '2', '@', Ch('™'), '€'),
    (vk::ANSI_3, '3', '#', Ch('£'), '‹'),
    (vk::ANSI_4, '4', '$', Ch('¢'), '›'),
    (vk::ANSI_5, '5', '%', Ch('∞'), 'ﬁ'),
    (vk::ANSI_6, '6', '^', Ch('§'), 'ﬂ'),
    (vk::ANSI_7, '7', '&', Ch('¶'), '‡'),
    (vk::ANSI_8, '8', '*', Ch('•'), '°'),
    (vk::ANSI_9, '9', '(', Ch('ª'), '·'),
    (vk::ANSI_0, '0', ')', Ch('º'), '‚'),
    (vk::ANSI_MINUS, '-', '_', Ch('–'), '—'),
    (vk::ANSI_EQUAL, '=', '+', Ch('≠'), '±'),
    (vk::ANSI_LEFT_BRACKET, '[', '{', Ch('“'), '”'),
    (vk::ANSI_RIGHT_BRACKET, ']', '}', Ch('‘'), '’'),
    (vk::ANSI_BACKSLASH, '\\', '|', Ch('«'), '»'),
    (vk::ANSI_SEMICOLON, ';', ':', Ch('…'), 'Ú'),
    (vk::ANSI_QUOTE, '\'', '"', Ch('æ'), 'Æ'),
    (vk::ANSI_GRAVE, '`', '~', Dead(dead::GRAVE), '\u{0060}'),
    (vk::ANSI_COMMA, ',', '<', Ch('≤'), '\u{00AF}'),
    (vk::ANSI_PERIOD, '.', '>', Ch('≥'), '\u{02D8}'),
    (vk::ANSI_SLASH, '/', '?', Ch('÷'), '¿'),
    (vk::SPACE, ' ', ' ', Ch('\u{00A0}'), '\u{00A0}'),
    (vk::ISO_SECTION, '§', '±', Ch('§'), '±'),
];

/// Keys whose output no modifier changes. The arrow and editing keys are the
/// classic control characters Apple's own layout gives them, not the private-use
/// scalars `-[NSEvent characters]` reports - a caller reaching `UCKeyTranslate`
/// is asking what the layout says, and this is what it says.
#[rustfmt::skip]
static FIXED: &[(u16, char)] = &[
    (vk::RETURN, '\u{000D}'),
    (vk::KEYPAD_ENTER, '\u{0003}'),
    (vk::TAB, '\u{0009}'),
    (vk::ESCAPE, '\u{001B}'),
    (vk::DELETE, '\u{0008}'),
    (vk::FORWARD_DELETE, '\u{007F}'),
    (vk::HELP, '\u{0005}'),
    (vk::HOME, '\u{0001}'),
    (vk::END, '\u{0004}'),
    (vk::PAGE_UP, '\u{000B}'),
    (vk::PAGE_DOWN, '\u{000C}'),
    (vk::LEFT_ARROW, '\u{001C}'),
    (vk::RIGHT_ARROW, '\u{001D}'),
    (vk::UP_ARROW, '\u{001E}'),
    (vk::DOWN_ARROW, '\u{001F}'),
    (vk::ANSI_KEYPAD_CLEAR, '\u{001B}'),
    (vk::ANSI_KEYPAD_0, '0'), (vk::ANSI_KEYPAD_1, '1'), (vk::ANSI_KEYPAD_2, '2'),
    (vk::ANSI_KEYPAD_3, '3'), (vk::ANSI_KEYPAD_4, '4'), (vk::ANSI_KEYPAD_5, '5'),
    (vk::ANSI_KEYPAD_6, '6'), (vk::ANSI_KEYPAD_7, '7'), (vk::ANSI_KEYPAD_8, '8'),
    (vk::ANSI_KEYPAD_9, '9'),
    (vk::ANSI_KEYPAD_DECIMAL, '.'),
    (vk::ANSI_KEYPAD_MULTIPLY, '*'),
    (vk::ANSI_KEYPAD_PLUS, '+'),
    (vk::ANSI_KEYPAD_MINUS, '-'),
    (vk::ANSI_KEYPAD_DIVIDE, '/'),
    (vk::ANSI_KEYPAD_EQUALS, '='),
];

/// The function keys, which every table answers with `kFunctionKeyCharCode`.
#[rustfmt::skip]
static FUNCTION_KEYS: &[u16] = &[
    vk::F1, vk::F2, vk::F3, vk::F4, vk::F5, vk::F6, vk::F7, vk::F8, vk::F9,
    vk::F10, vk::F11, vk::F12, vk::F13, vk::F14, vk::F15, vk::F16, vk::F17,
    vk::F18, vk::F19, vk::F20,
];

/// The control table's own entries. A key absent from it keeps what it types
/// unmodified, which is what the real layout does for the digits.
#[rustfmt::skip]
static CONTROL: &[(u16, char)] = &[
    (vk::ANSI_LEFT_BRACKET, '\u{001B}'),
    (vk::ANSI_BACKSLASH, '\u{001C}'),
    (vk::ANSI_RIGHT_BRACKET, '\u{001D}'),
    (vk::ANSI_6, '\u{001E}'),
    (vk::ANSI_MINUS, '\u{001F}'),
    (vk::ANSI_SLASH, '\u{007F}'),
    (vk::ANSI_2, '\u{0000}'),
    (vk::SPACE, '\u{0000}'),
    (vk::ANSI_GRAVE, '\u{0000}'),
];

/// What a base character becomes after each dead-key state. Space is here too:
/// it is how a US layout types a bare accent, and without it space would emit
/// the terminator *and* a space.
#[rustfmt::skip]
static COMPOSE: &[(char, &[(u16, char)])] = &[
    (' ', &[(dead::ACUTE, '\u{00B4}'), (dead::GRAVE, '\u{0060}'),
            (dead::CIRCUMFLEX, '\u{02C6}'), (dead::TILDE, '\u{02DC}'),
            (dead::DIAERESIS, '\u{00A8}')]),
    ('a', &[(dead::ACUTE, 'á'), (dead::GRAVE, 'à'), (dead::CIRCUMFLEX, 'â'),
            (dead::TILDE, 'ã'), (dead::DIAERESIS, 'ä')]),
    ('A', &[(dead::ACUTE, 'Á'), (dead::GRAVE, 'À'), (dead::CIRCUMFLEX, 'Â'),
            (dead::TILDE, 'Ã'), (dead::DIAERESIS, 'Ä')]),
    ('e', &[(dead::ACUTE, 'é'), (dead::GRAVE, 'è'), (dead::CIRCUMFLEX, 'ê'),
            (dead::DIAERESIS, 'ë')]),
    ('E', &[(dead::ACUTE, 'É'), (dead::GRAVE, 'È'), (dead::CIRCUMFLEX, 'Ê'),
            (dead::DIAERESIS, 'Ë')]),
    ('i', &[(dead::ACUTE, 'í'), (dead::GRAVE, 'ì'), (dead::CIRCUMFLEX, 'î'),
            (dead::DIAERESIS, 'ï')]),
    ('I', &[(dead::ACUTE, 'Í'), (dead::GRAVE, 'Ì'), (dead::CIRCUMFLEX, 'Î'),
            (dead::DIAERESIS, 'Ï')]),
    ('n', &[(dead::TILDE, 'ñ')]),
    ('N', &[(dead::TILDE, 'Ñ')]),
    ('o', &[(dead::ACUTE, 'ó'), (dead::GRAVE, 'ò'), (dead::CIRCUMFLEX, 'ô'),
            (dead::TILDE, 'õ'), (dead::DIAERESIS, 'ö')]),
    ('O', &[(dead::ACUTE, 'Ó'), (dead::GRAVE, 'Ò'), (dead::CIRCUMFLEX, 'Ô'),
            (dead::TILDE, 'Õ'), (dead::DIAERESIS, 'Ö')]),
    ('u', &[(dead::ACUTE, 'ú'), (dead::GRAVE, 'ù'), (dead::CIRCUMFLEX, 'û'),
            (dead::DIAERESIS, 'ü')]),
    ('U', &[(dead::ACUTE, 'Ú'), (dead::GRAVE, 'Ù'), (dead::CIRCUMFLEX, 'Û'),
            (dead::DIAERESIS, 'Ü')]),
    ('y', &[(dead::ACUTE, 'ý'), (dead::DIAERESIS, 'ÿ')]),
    ('Y', &[(dead::ACUTE, 'Ý'), (dead::DIAERESIS, 'Ÿ')]),
];

use crate::keymap::vk;

/// The tables, in the order they are written and numbered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Plain,
    Shift,
    Caps,
    CapsShift,
    Option,
    ShiftOption,
    CapsOption,
    CapsShiftOption,
    Control,
}

const TABLES: [Table; 9] = [
    Table::Plain,
    Table::Shift,
    Table::Caps,
    Table::CapsShift,
    Table::Option,
    Table::ShiftOption,
    Table::CapsOption,
    Table::CapsShiftOption,
    Table::Control,
];

/// Which table a modifier byte selects.
///
/// Caps lock shifts the alphabetic keys and leaves the rest alone, which is why
/// it needs tables of its own rather than reusing shift's. Command suppresses
/// option so that a key equivalent is the letter the key is printed with, and
/// control wins over everything, as it does on the real layout.
fn table_for(modifiers: u8) -> u8 {
    let shift = modifiers & (modifier::SHIFT | modifier::RIGHT_SHIFT) != 0;
    let option = modifiers & (modifier::OPTION | modifier::RIGHT_OPTION) != 0;
    let control = modifiers & (modifier::CONTROL | modifier::RIGHT_CONTROL) != 0;
    let caps = modifiers & modifier::ALPHA_LOCK != 0;
    let command = modifiers & modifier::COMMAND != 0;
    let table = match (control, command, option, shift, caps) {
        (true, ..) => Table::Control,
        (_, true, _, true, _) => Table::Shift,
        (_, true, ..) => Table::Plain,
        (_, _, true, true, true) => Table::CapsShiftOption,
        (_, _, true, true, false) => Table::ShiftOption,
        (_, _, true, false, true) => Table::CapsOption,
        (_, _, true, false, false) => Table::Option,
        (_, _, false, true, true) => Table::CapsShift,
        (_, _, false, true, false) => Table::Shift,
        (_, _, false, false, true) => Table::Caps,
        (_, _, false, false, false) => Table::Plain,
    };
    TABLES.iter().position(|t| *t == table).unwrap_or(0) as u8
}

fn composes(c: char) -> bool {
    COMPOSE.iter().any(|(base, _)| *base == c)
}

fn cell(c: char) -> Cell {
    match composes(c) {
        true => Cell::Compose(c),
        false => Cell::Char(c),
    }
}

/// The modifier state a table stands for: `(shift, caps lock, option)`.
/// The control table is the exception and is built from its own list.
fn state_of(table: Table) -> (bool, bool, bool) {
    match table {
        Table::Plain | Table::Control => (false, false, false),
        Table::Shift => (true, false, false),
        Table::Caps => (false, true, false),
        Table::CapsShift => (true, true, false),
        Table::Option => (false, false, true),
        Table::ShiftOption => (true, false, true),
        Table::CapsOption => (false, true, true),
        Table::CapsShiftOption => (true, true, true),
    }
}

/// One table's 128 entries.
fn build_table(table: Table) -> Vec<Cell> {
    let (shift_on, caps_on, option_on) = state_of(table);
    let mut cells = vec![Cell::None; TABLE_SIZE];
    for (key, plain, shift, option, shift_option) in PRINTABLE {
        // Caps lock shifts the keys that type a letter and no others.
        let shifted = shift_on != (caps_on && plain.is_ascii_alphabetic());
        let out = match (table, option_on, shifted) {
            (Table::Control, ..) => match CONTROL.iter().find(|(k, _)| k == key) {
                Some((_, c)) => Cell::Char(*c),
                None if plain.is_ascii_lowercase() => Cell::Char((*plain as u8 - b'a' + 1) as char),
                None => Cell::Char(*plain),
            },
            (_, true, false) => match option {
                Ch(c) => cell(*c),
                Dead(state) => Cell::Dead(*state),
            },
            (_, true, true) => cell(*shift_option),
            (_, false, true) => cell(*shift),
            (_, false, false) => cell(*plain),
        };
        cells[*key as usize] = out;
    }
    for (key, c) in FIXED {
        cells[*key as usize] = Cell::Char(*c);
    }
    for key in FUNCTION_KEYS {
        cells[*key as usize] = Cell::Char('\u{0010}');
    }
    cells
}

/// A `UCKeyOutput` for a character, which is the scalar itself for everything a
/// keyboard layout can name. A dead key or a composed character is an index
/// instead, tagged in the top two bits.
fn char_output(c: char) -> u16 {
    match u16::try_from(c as u32) {
        Ok(scalar) if scalar < output::NONE_ALT => scalar,
        // Above the BMP a layout has to use a sequence, and nothing here does.
        _ => output::NONE,
    }
}

/// One entry of the state-record list: a dead key has no entries and a next
/// state, a composing character has entries and none.
struct Record {
    zero_char: u16,
    next_state: u16,
    entries: Vec<(u16, u16)>,
}

fn record_for(cell: Cell) -> Option<Record> {
    match cell {
        Cell::Dead(state) => Some(Record {
            zero_char: output::NONE,
            next_state: state,
            entries: Vec::new(),
        }),
        Cell::Compose(base) => {
            let entries = COMPOSE.iter().find(|(c, _)| *c == base)?.1;
            Some(Record {
                zero_char: char_output(base),
                next_state: 0,
                entries: entries
                    .iter()
                    .map(|(state, c)| (*state, char_output(*c)))
                    .collect(),
            })
        }
        _ => None,
    }
}

/// A resource under construction. Offsets are `UInt32` from the start of the
/// whole thing, so every section is written first and its offset patched in.
struct Blob(Vec<u8>);

impl Blob {
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn here(&self) -> u32 {
        self.0.len() as u32
    }

    fn patch(&mut self, at: usize, value: u32) {
        self.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Every section starts on a four-byte boundary, because the offsets in a
    /// header are read as aligned `UInt32` by whatever parses this.
    fn align(&mut self) {
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
    }
}

/// The US ANSI resource, built once.
///
/// Little-endian, because a `'uchr'` handed out by Text Input Services is in the
/// byte order of the process that reads it and every architecture this runtime
/// serves is little-endian. It is not the big-endian resource fork of the same
/// name.
pub fn layout() -> &'static [u8] {
    static LAYOUT: OnceLock<Vec<u8>> = OnceLock::new();
    LAYOUT.get_or_init(build)
}

fn build() -> Vec<u8> {
    let tables: Vec<Vec<Cell>> = TABLES.iter().map(|t| build_table(*t)).collect();

    let mut keys: Vec<Cell> = Vec::new();
    for cell in tables.iter().flatten() {
        if matches!(cell, Cell::Dead(_) | Cell::Compose(_)) && !keys.contains(cell) {
            keys.push(*cell);
        }
    }
    let records: Vec<Record> = keys.iter().filter_map(|c| record_for(*c)).collect();

    let mut blob = Blob(Vec::new());
    blob.u16(format::HEADER);
    blob.u16(0x0100);
    let feature_offset = blob.here() as usize;
    blob.u32(0);
    blob.u32(1);

    // `UCKeyboardTypeHeader`: one entry, covering every keyboard type, because
    // this host reports one keyboard.
    blob.u32(0);
    blob.u32(u32::MAX);
    let modifiers_offset = blob.here() as usize;
    blob.u32(0);
    let table_index_offset = blob.here() as usize;
    blob.u32(0);
    let records_offset = blob.here() as usize;
    blob.u32(0);
    let terminators_offset = blob.here() as usize;
    blob.u32(0);
    // No sequence list: every output here is a single scalar.
    blob.u32(0);

    blob.align();
    let at = blob.here();
    blob.patch(feature_offset, at);
    blob.u16(format::FEATURE_INFO);
    blob.u16(0);
    // A terminator and the character after it, which is the longest a
    // translation gets without sequences.
    blob.u32(2);

    blob.align();
    let at = blob.here();
    blob.patch(modifiers_offset, at);
    blob.u16(format::MODIFIERS_TO_TABLE_NUM);
    blob.u16(0);
    blob.u32(256);
    for modifiers in 0..=u8::MAX {
        blob.0.push(table_for(modifiers));
    }

    blob.align();
    let at = blob.here();
    blob.patch(table_index_offset, at);
    blob.u16(format::KEY_TO_CHAR_TABLE_INDEX);
    blob.u16(TABLE_SIZE as u16);
    blob.u32(tables.len() as u32);
    let table_offsets = blob.here() as usize;
    for _ in &tables {
        blob.u32(0);
    }
    for (index, cells) in tables.iter().enumerate() {
        blob.align();
        let at = blob.here();
        blob.patch(table_offsets + index * 4, at);
        for cell in cells {
            let out = match cell {
                Cell::None => output::NONE,
                Cell::Char(c) => char_output(*c),
                Cell::Dead(_) | Cell::Compose(_) => {
                    let index = keys.iter().position(|k| k == cell).unwrap_or(0);
                    output::STATE_INDEX | index as u16
                }
            };
            blob.u16(out);
        }
    }

    blob.align();
    let at = blob.here();
    blob.patch(records_offset, at);
    blob.u16(format::STATE_RECORDS_INDEX);
    blob.u16(records.len() as u16);
    let record_offsets = blob.here() as usize;
    for _ in &records {
        blob.u32(0);
    }
    for (index, record) in records.iter().enumerate() {
        blob.align();
        let at = blob.here();
        blob.patch(record_offsets + index * 4, at);
        blob.u16(record.zero_char);
        blob.u16(record.next_state);
        blob.u16(record.entries.len() as u16);
        blob.u16(format::STATE_ENTRY_TERMINAL);
        for (state, out) in &record.entries {
            blob.u16(*state);
            blob.u16(*out);
        }
    }

    blob.align();
    let at = blob.here();
    blob.patch(terminators_offset, at);
    blob.u16(format::STATE_TERMINATORS);
    blob.u16(TERMINATORS.len() as u16);
    for c in TERMINATORS {
        blob.u16(char_output(c));
    }

    blob.0
}

/// A layout being read, which is a bare pointer: `UCKeyTranslate` is handed no
/// length. Reads are capped at [`Reader::LIMIT`] so a resource with a corrupt
/// offset ends as an error rather than a read of unrelated memory - except for
/// this tree's own layout, whose real length is known and used.
struct Reader {
    base: *const u8,
    limit: usize,
}

impl Reader {
    /// Larger than any keyboard layout Apple ships; Chinese and Japanese
    /// layouts are input methods rather than `'uchr'` resources.
    const LIMIT: usize = 1 << 20;

    fn new(base: *const u8) -> Self {
        let ours = layout();
        let limit = match std::ptr::eq(base, ours.as_ptr()) {
            true => ours.len(),
            false => Self::LIMIT,
        };
        Reader { base, limit }
    }

    fn u8(&self, at: usize) -> Option<u8> {
        // SAFETY: within the length this layout is known or assumed to have.
        (at < self.limit).then(|| unsafe { self.base.add(at).read() })
    }

    fn u16(&self, at: usize) -> Option<u16> {
        (at + 2 <= self.limit).then(|| {
            // SAFETY: within the length this layout is known or assumed to have,
            // and read unaligned because a section may sit on an odd offset.
            unsafe { self.base.add(at).cast::<u16>().read_unaligned() }
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        (at + 4 <= self.limit).then(|| {
            // SAFETY: as above.
            unsafe { self.base.add(at).cast::<u32>().read_unaligned() }
        })
    }
}

/// The sections of a parsed layout, resolved for one keyboard type.
struct Parsed {
    reader: Reader,
    table: usize,
    table_size: u16,
    records: usize,
    terminators: usize,
    sequences: usize,
}

fn section(reader: &Reader, at: usize, expected: u16) -> Option<usize> {
    match at != 0 && reader.u16(at)? == expected {
        true => Some(at),
        false => None,
    }
}

impl Parsed {
    fn new(reader: Reader, kbd_type: u32, modifiers: u8) -> Option<Parsed> {
        if reader.u16(0)? != format::HEADER {
            return None;
        }
        let type_count = reader.u32(8)? as usize;
        // 28 bytes per `UCKeyboardTypeHeader`, the first at offset 12. A type
        // outside every range falls back to the first, which is what a layout
        // describing one keyboard means by it.
        let mut chosen = 12;
        for index in 0..type_count.min(64) {
            let at = 12 + index * 28;
            let first = reader.u32(at)?;
            let last = reader.u32(at + 4)?;
            if (first..=last).contains(&kbd_type) {
                chosen = at;
                break;
            }
        }
        let modifiers_at = reader.u32(chosen + 8)? as usize;
        let index_at = reader.u32(chosen + 12)? as usize;
        let records = reader.u32(chosen + 16)? as usize;
        let terminators = reader.u32(chosen + 20)? as usize;
        let sequences = reader.u32(chosen + 24)? as usize;

        let modifiers_at = section(&reader, modifiers_at, format::MODIFIERS_TO_TABLE_NUM)?;
        let default_table = reader.u16(modifiers_at + 2)?;
        let count = reader.u32(modifiers_at + 4)? as usize;
        let table_num = match (modifiers as usize) < count {
            true => reader.u8(modifiers_at + 8 + modifiers as usize)?,
            false => default_table as u8,
        } as usize;

        let index_at = section(&reader, index_at, format::KEY_TO_CHAR_TABLE_INDEX)?;
        let table_size = reader.u16(index_at + 2)?;
        let table_count = reader.u32(index_at + 4)? as usize;
        let table_num = match table_num < table_count {
            true => table_num,
            false => 0,
        };
        let table = reader.u32(index_at + 8 + table_num * 4)? as usize;
        let records = section(&reader, records, format::STATE_RECORDS_INDEX).unwrap_or(0);
        let terminators = section(&reader, terminators, format::STATE_TERMINATORS).unwrap_or(0);

        Some(Parsed {
            reader,
            table,
            table_size,
            records,
            terminators,
            sequences,
        })
    }

    /// The `UCKeyOutput` for a key code, or none for a code the table does not
    /// describe.
    fn output_for(&self, code: u16) -> Option<u16> {
        match code < self.table_size {
            true => self.reader.u16(self.table + code as usize * 2),
            false => None,
        }
    }

    fn terminator(&self, state: u32) -> u16 {
        let index = match state.checked_sub(1) {
            Some(index) => index as usize,
            None => return output::NONE,
        };
        let count = self.reader.u16(self.terminators + 2).unwrap_or(0) as usize;
        match self.terminators != 0 && index < count {
            true => self
                .reader
                .u16(self.terminators + 4 + index * 2)
                .unwrap_or(output::NONE),
            false => output::NONE,
        }
    }

    /// A `UCKeyCharSeq`, which is a character unless its top bits say it indexes
    /// the sequence list.
    fn push(&self, out: u16, chars: &mut Vec<u16>) {
        if out == output::NONE || out == output::NONE_ALT {
            return;
        }
        if out & output::TEST_FOR_INDEX != output::SEQUENCE_INDEX {
            chars.push(out);
            return;
        }
        let at = self.sequences;
        if section(&self.reader, at, format::SEQUENCE_DATA_INDEX).is_none() {
            return;
        }
        let index = (out & output::GET_INDEX) as usize;
        let count = self.reader.u16(at + 2).unwrap_or(0) as usize;
        if index >= count {
            return;
        }
        // `charSequenceOffsets` holds one more offset than there are sequences,
        // so a sequence is the characters between its offset and the next.
        let start = self.reader.u16(at + 4 + index * 2).unwrap_or(0) as usize;
        let end = self.reader.u16(at + 6 + index * 2).unwrap_or(0) as usize;
        for offset in (start..end).step_by(2) {
            if let Some(c) = self.reader.u16(at + offset) {
                chars.push(c);
            }
        }
    }

    /// The record's answer for the state the translation is in, as
    /// `(output, next state)`.
    fn entry(&self, record: usize, state: u32) -> Option<(u16, u16)> {
        let count = self.reader.u16(record + 4)? as usize;
        let entries = record + 8;
        match self.reader.u16(record + 6)? {
            format::STATE_ENTRY_TERMINAL => (0..count).find_map(|index| {
                let at = entries + index * 4;
                (self.reader.u16(at)? as u32 == state).then_some((self.reader.u16(at + 2)?, 0))
            }),
            format::STATE_ENTRY_RANGE => (0..count).find_map(|index| {
                let at = entries + index * 8;
                let start = self.reader.u16(at)? as u32;
                let range = self.reader.u8(at + 2)? as u32;
                let multiplier = self.reader.u8(at + 3)? as u32;
                let char_data = self.reader.u16(at + 4)?;
                let next = self.reader.u16(at + 6)?;
                let step = state.checked_sub(start).filter(|step| *step <= range)?;
                Some((char_data.wrapping_add((step * multiplier) as u16), next))
            }),
            _ => None,
        }
    }

    fn record(&self, index: u16) -> Option<usize> {
        let count = self.reader.u16(self.records + 2)? as usize;
        match self.records != 0 && (index as usize) < count {
            true => Some(self.reader.u32(self.records + 4 + index as usize * 4)? as usize),
            false => None,
        }
    }

    /// The state machine: a key either composes with the state the translation
    /// is in, enters a state of its own, or ends the one it found.
    fn emit(&self, out: u16, state: &mut u32, no_dead: bool, chars: &mut Vec<u16>) {
        if out == output::NONE || out == output::NONE_ALT {
            return;
        }
        if out & output::TEST_FOR_INDEX != output::STATE_INDEX {
            if *state != 0 {
                self.push(self.terminator(*state), chars);
                *state = 0;
            }
            self.push(out, chars);
            return;
        }
        let Some(record) = self.record(out & output::GET_INDEX) else {
            return;
        };
        let (zero_char, next) = match (self.reader.u16(record), self.reader.u16(record + 2)) {
            (Some(zero_char), Some(next)) => (zero_char, next),
            _ => return,
        };
        if *state == 0 {
            match (no_dead, next) {
                // A caller that asked for no dead keys wants the accent itself,
                // which is the state's terminator rather than nothing.
                (true, next) if next != 0 => match zero_char {
                    output::NONE | output::NONE_ALT => {
                        self.push(self.terminator(next as u32), chars)
                    }
                    _ => self.push(zero_char, chars),
                },
                _ => {
                    self.push(zero_char, chars);
                    *state = next as u32;
                }
            }
            return;
        }
        match self.entry(record, *state) {
            Some((composed, next)) => {
                *state = next as u32;
                self.push(composed, chars);
            }
            // Nothing composes: the accent stands on its own and the key is
            // translated again as if it had been pressed first.
            None => {
                self.push(self.terminator(*state), chars);
                *state = 0;
                self.emit(out, state, no_dead, chars);
            }
        }
    }
}

/// `UCKeyTranslate`'s work, as characters rather than an out-parameter.
///
/// # Safety
///
/// `layout` is null or points at a `UCKeyboardLayout`.
pub unsafe fn translate(
    layout: *const u8,
    code: u16,
    action: u16,
    modifiers: u32,
    kbd_type: u32,
    options: u32,
    state: &mut u32,
) -> Result<Vec<u16>, i32> {
    if layout.is_null() {
        return Err(PARAM_ERR);
    }
    let modifiers = (modifiers & 0xFF) as u8;
    let parsed = Parsed::new(Reader::new(layout), kbd_type, modifiers).ok_or(PARAM_ERR)?;
    let mut chars = Vec::new();
    let no_dead = options & NO_DEAD_KEYS != 0 || action == ACTION_DISPLAY;
    if let Some(out) = parsed.output_for(code) {
        parsed.emit(out, state, no_dead, &mut chars);
    }
    Ok(chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Translate against the built layout, as the guest does.
    fn typed_with(code: u16, modifiers: u8, options: u32, state: &mut u32) -> String {
        // SAFETY: this module's own resource.
        let chars = unsafe {
            translate(
                layout().as_ptr(),
                code,
                0,
                modifiers as u32,
                40,
                options,
                state,
            )
        };
        String::from_utf16_lossy(&chars.expect("this layout translates"))
    }

    fn typed(code: u16, modifiers: u8) -> String {
        typed_with(code, modifiers, 0, &mut 0)
    }

    #[test]
    fn the_unshifted_keys_type_what_they_are_printed_with() {
        assert_eq!(typed(vk::ANSI_A, 0), "a");
        assert_eq!(typed(vk::ANSI_LEFT_BRACKET, 0), "[");
        assert_eq!(typed(vk::ANSI_1, 0), "1");
        assert_eq!(typed(vk::SPACE, 0), " ");
        assert_eq!(typed(vk::RETURN, 0), "\r");
    }

    #[test]
    fn shift_and_caps_lock_disagree_about_the_keys_that_are_not_letters() {
        assert_eq!(typed(vk::ANSI_A, modifier::SHIFT), "A");
        assert_eq!(typed(vk::ANSI_A, modifier::ALPHA_LOCK), "A");
        // Caps lock is not shift: it leaves a digit alone, and shift with it on
        // types the lowercase letter.
        assert_eq!(typed(vk::ANSI_1, modifier::ALPHA_LOCK), "1");
        assert_eq!(typed(vk::ANSI_1, modifier::SHIFT), "!");
        assert_eq!(
            typed(vk::ANSI_A, modifier::SHIFT | modifier::ALPHA_LOCK),
            "a"
        );
        assert_eq!(
            typed(vk::ANSI_1, modifier::SHIFT | modifier::ALPHA_LOCK),
            "!"
        );
    }

    #[test]
    fn the_right_hand_modifiers_are_the_same_modifiers() {
        assert_eq!(typed(vk::ANSI_A, modifier::RIGHT_SHIFT), "A");
        assert_eq!(typed(vk::ANSI_A, modifier::RIGHT_OPTION), "å");
    }

    #[test]
    fn option_types_the_us_layouts_second_character() {
        assert_eq!(typed(vk::ANSI_A, modifier::OPTION), "å");
        assert_eq!(typed(vk::ANSI_A, modifier::OPTION | modifier::SHIFT), "Å");
        assert_eq!(typed(vk::ANSI_2, modifier::OPTION), "™");
    }

    /// Control produces the control character, which is the one thing a guest
    /// cannot work out from `-[NSEvent characters]`.
    #[test]
    fn control_types_control_characters() {
        assert_eq!(typed(vk::ANSI_A, modifier::CONTROL), "\u{0001}");
        assert_eq!(typed(vk::ANSI_LEFT_BRACKET, modifier::CONTROL), "\u{001B}");
        // A digit has no control character and keeps typing itself.
        assert_eq!(typed(vk::ANSI_1, modifier::CONTROL), "1");
    }

    /// A key equivalent is the letter on the key, so command has to suppress
    /// option rather than compose with it.
    #[test]
    fn command_types_the_key_it_is_printed_with() {
        assert_eq!(typed(vk::ANSI_A, modifier::COMMAND | modifier::OPTION), "a");
        assert_eq!(
            typed(
                vk::ANSI_A,
                modifier::COMMAND | modifier::OPTION | modifier::SHIFT
            ),
            "A"
        );
    }

    #[test]
    fn a_dead_key_emits_nothing_and_composes_with_the_next() {
        let mut state = 0;
        assert_eq!(typed_with(vk::ANSI_E, modifier::OPTION, 0, &mut state), "");
        assert_ne!(state, 0, "the acute state has to be entered");
        assert_eq!(typed_with(vk::ANSI_E, 0, 0, &mut state), "é");
        assert_eq!(state, 0, "composing leaves the state");
    }

    #[test]
    fn a_dead_key_that_composes_with_nothing_stands_on_its_own() {
        let mut state = 0;
        typed_with(vk::ANSI_E, modifier::OPTION, 0, &mut state);
        // Space is how a US layout types a bare accent.
        assert_eq!(typed_with(vk::SPACE, 0, 0, &mut state), "´");
        assert_eq!(state, 0);

        typed_with(vk::ANSI_N, modifier::OPTION, 0, &mut state);
        // A key that does not compose emits the accent and then itself.
        assert_eq!(typed_with(vk::ANSI_Q, 0, 0, &mut state), "˜q");
        assert_eq!(state, 0);
    }

    /// Two dead keys in a row: the first ends and the second begins, which is
    /// the path that recurses.
    #[test]
    fn a_dead_key_after_a_dead_key_ends_the_first() {
        let mut state = 0;
        typed_with(vk::ANSI_E, modifier::OPTION, 0, &mut state);
        assert_eq!(typed_with(vk::ANSI_I, modifier::OPTION, 0, &mut state), "´");
        assert_ne!(state, 0, "the circumflex state has to be entered");
        assert_eq!(typed_with(vk::ANSI_O, 0, 0, &mut state), "ô");
    }

    /// A caller asking for no dead keys wants the accent, not silence - which is
    /// what a menu drawing a key equivalent depends on.
    #[test]
    fn no_dead_keys_types_the_accent_and_enters_no_state() {
        let mut state = 0;
        assert_eq!(
            typed_with(vk::ANSI_E, modifier::OPTION, NO_DEAD_KEYS, &mut state),
            "´"
        );
        assert_eq!(state, 0);
        // The display action means the same thing.
        let chars = unsafe {
            translate(
                layout().as_ptr(),
                vk::ANSI_E,
                ACTION_DISPLAY,
                modifier::OPTION as u32,
                40,
                0,
                &mut state,
            )
        };
        assert_eq!(String::from_utf16_lossy(&chars.unwrap()), "´");
        assert_eq!(state, 0);
    }

    #[test]
    fn a_key_the_layout_does_not_describe_types_nothing() {
        assert_eq!(typed(vk::SHIFT, 0), "", "a modifier types nothing");
        assert_eq!(typed(200, 0), "", "and neither does a code out of range");
    }

    /// Every function key is `kFunctionKeyCharCode`, as on the real layout.
    #[test]
    fn the_function_keys_are_one_character() {
        assert_eq!(typed(vk::F1, 0), "\u{0010}");
        assert_eq!(typed(vk::F13, modifier::SHIFT), "\u{0010}");
    }

    /// A layout that is not one is refused rather than read.
    #[test]
    fn a_resource_without_the_header_format_is_refused() {
        let junk = [0u8; 64];
        // SAFETY: a live buffer that is not a layout.
        let status = unsafe { translate(junk.as_ptr(), 0, 0, 0, 40, 0, &mut 0) };
        assert_eq!(status.err(), Some(PARAM_ERR));
        // SAFETY: null is the documented refusal.
        let status = unsafe { translate(std::ptr::null(), 0, 0, 0, 40, 0, &mut 0) };
        assert_eq!(status.err(), Some(PARAM_ERR));
    }

    /// Every offset in the resource has to land inside it, or a guest parsing
    /// the tables itself reads whatever follows.
    #[test]
    fn every_offset_is_inside_the_resource() {
        let bytes = layout();
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(u16_at(0), format::HEADER);
        assert_eq!(u32_at(8), 1, "one keyboard type");
        for at in [4, 20, 24, 28, 32] {
            let offset = u32_at(at);
            assert!(offset > 0 && offset < bytes.len(), "offset at {at}");
            assert_eq!(offset % 4, 0, "offset at {at} is unaligned");
        }
        assert_eq!(u32_at(36), 0, "no sequence list is written");

        let tables = u32_at(24);
        assert_eq!(u16_at(tables), format::KEY_TO_CHAR_TABLE_INDEX);
        assert_eq!(u16_at(tables + 2) as usize, TABLE_SIZE);
        let count = u32_at(tables + 4);
        assert_eq!(count, TABLES.len());
        for index in 0..count {
            let table = u32_at(tables + 8 + index * 4);
            assert!(
                table + TABLE_SIZE * 2 <= bytes.len(),
                "table {index} runs past the end"
            );
        }
    }

    /// Every modifier combination selects a table that exists.
    #[test]
    fn the_modifier_table_names_a_real_table() {
        for modifiers in 0..=u8::MAX {
            assert!(
                (table_for(modifiers) as usize) < TABLES.len(),
                "modifiers {modifiers:#04x}"
            );
        }
    }

    /// No character in the tables collides with the tag bits that would make it
    /// an index instead.
    #[test]
    fn no_character_is_mistaken_for_an_index() {
        for table in TABLES {
            for cell in build_table(table) {
                if let Cell::Char(c) | Cell::Compose(c) = cell {
                    let out = char_output(c);
                    assert_ne!(out, output::NONE, "{c:?} is not a single scalar");
                    assert_ne!(
                        out & output::TEST_FOR_INDEX,
                        output::STATE_INDEX,
                        "{c:?} would read as a state index"
                    );
                    assert_ne!(
                        out & output::TEST_FOR_INDEX,
                        output::SEQUENCE_INDEX,
                        "{c:?} would read as a sequence index"
                    );
                }
            }
        }
    }
}
