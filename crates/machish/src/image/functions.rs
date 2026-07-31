//! `LC_FUNCTION_STARTS`: where each function in `__TEXT` begins.
//!
//! A ULEB128 delta list — the first entry is an offset from the image base, each
//! subsequent one an offset from the previous function's start, terminated by a
//! zero delta. It exists for the unwinder and the debugger, and it is the
//! cheapest exact answer to a question a recompiler has to ask: *where does one
//! function end and the next begin?*
//!
//! That matters more here than it would in a native loader. `__text` is not one
//! function; it is dozens, separated by alignment padding and sometimes by
//! read-only data. Handing a lifter the whole section and a single start address
//! makes it decode straight through those gaps, where alignment bytes decode as
//! real instructions — `0xfb` is `sti`, `0x00 0x00` is `add [rax], al` — and the
//! failure surfaces as a bogus "unsupported instruction" far from the actual
//! problem. Lifting function by function avoids the question entirely.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::image::MachObject;
use crate::raw::{Cursor, Word};
use crate::spec::MachSpec;

/// A guard against a corrupt delta list. Large binaries reach six figures.
const MAX_FUNCTIONS: usize = 1 << 22;

/// Unslid virtual addresses of every function start, in ascending order.
///
/// Empty — not an error — for an image with no `LC_FUNCTION_STARTS`, which is a
/// stripped binary rather than a malformed one.
pub fn function_starts<S: MachSpec>(obj: &MachObject<'_, S>) -> Result<Vec<Word>> {
    let Some(data) = obj.function_starts_data() else { return Ok(Vec::new()) };
    let Some(range) = data.range() else { return Ok(Vec::new()) };

    let view = obj.view().subview(range.start, range.end - range.start, "function starts")?;
    let mut cur = Cursor::new(view);
    let mut addr = obj.preferred_base()?;
    let mut out = Vec::new();

    while !cur.at_end() {
        let delta = cur.uleb128()?;
        // A zero delta terminates the list. It cannot mean "another function at
        // the same address", so treating it as data rather than as a terminator
        // would loop.
        if delta == 0 {
            break;
        }
        addr = addr.checked_add(delta).ok_or(Error::Truncated { what: "function starts" })?;
        if out.len() >= MAX_FUNCTIONS {
            return Err(Error::TooDeep("function starts"));
        }
        out.push(addr);
    }
    Ok(out)
}

/// Function starts inside `[start, end)`, paired with each function's extent.
///
/// The last function in the range ends at `end`, since the delta list records
/// starts only. That is exact for a function that runs to the end of `__text`
/// and an over-estimate for one followed by padding — which is harmless, since
/// a lifter stops at the function's `ret` either way and never reaches the slack.
pub fn function_ranges<S: MachSpec>(
    obj: &MachObject<'_, S>,
    start: Word,
    end: Word,
) -> Result<Vec<core::ops::Range<Word>>> {
    let starts: Vec<Word> =
        function_starts(obj)?.into_iter().filter(|&a| a >= start && a < end).collect();

    Ok(starts
        .iter()
        .enumerate()
        .map(|(i, &a)| a..starts.get(i + 1).copied().unwrap_or(end))
        .collect())
}
