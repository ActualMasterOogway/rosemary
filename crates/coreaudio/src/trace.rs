//! `ROSETTA3_TRACE_AUDIO`: name every property a program sets on its output
//! unit, and every one this HAL was asked for and does not have.
//!
//! A program that cannot open its output says so in one line of its own log and
//! nothing about which call failed. There are twelve of them in a typical
//! bring-up, they all return an `OSStatus` the program throws away, and the
//! only one that matters is the first non-zero - which is exactly what this
//! prints.

use static_init::dynamic;

#[inline]
pub fn on() -> bool {
    #[dynamic]
    static ON: bool = std::env::var_os("ROSETTA3_TRACE_AUDIO").is_some();
    *ON
}

/// A four-character code as its characters, for the HAL's selectors, or as a
/// number when it is not printable.
pub fn code(value: u32) -> String {
    let bytes = value.to_be_bytes();
    match bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        true => format!("'{}'", String::from_utf8_lossy(&bytes)),
        false => format!("{value}"),
    }
}

/// One call, its subject and what it answered.
pub fn say(what: std::fmt::Arguments<'_>, status: i32) {
    if !on() {
        return;
    }
    let outcome = match status {
        crate::status::SUCCESS => "ok".to_string(),
        other => format!("error {other}"),
    };
    eprintln!("audio: {what} -> {outcome}");
}
