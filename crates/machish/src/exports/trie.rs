//! The trie is a prefix tree over symbol names. Each node holds an optional
//! terminal payload (this node's own name is an export) followed by a child
//! count and, per child, a name suffix and the absolute offset of the child
//! node within the trie. A symbol's name is the concatenation of the edge
//! labels from the root, which is why `_foo` and `_foobar` share one node's
//! worth of storage.
//!
//! The walk is iterative with an explicit stack rather than recursive.
//! Since child offsets might be attacker-controlled `uleb128`s
//! that may point anywhere in the trie, including at an ancestor, a
//! recursive walker on a hostile file overflows the host stack. For this reason,
//! we harden extensively against hostile input with the following mitigations:
//!
//! - a visited set, which is what actually rejects a back-edge. A trie has
//!   exactly one path to each node, so any second visit to an offset means the
//!   structure is not a trie;
//! - a depth cap, which bounds a legitimately deep but degenerate chain;
//! - an export cap, so a wide file cannot make us allocate without bound.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::exports::{Export, ExportKind, ExportValue};
use crate::raw::{Cursor, View, Word};
use crate::spec::constants as c;

/// Longest symbol name, in edge labels, we will follow. Real C++ mangling gets
/// long but never gets *deep*: this is dozens of times the observed maximum.
const MAX_TRIE_DEPTH: usize = 128;

/// Upper bound on exports from a single image. `libSystem`'s re-export umbrella
/// is in the low tens of thousands.
const MAX_EXPORTS: usize = 1 << 20;

/// Walk an export trie, returning every symbol it defines.
///
/// `image_base` is the image's preferred base (the address of its `mach_header`).
/// Trie nodes store offsets from there; the returned [`Export::vmaddr`] is the
/// unslid virtual address, so both export parsers speak the same units.
pub fn parse_trie(bytes: &[u8], image_base: Word) -> Result<Vec<Export>> {
    let view = View::new(bytes);
    let mut out = Vec::new();
    if bytes.is_empty() {
        return Ok(out);
    }

    let mut visited = BTreeSet::new();
    // (node offset, accumulated name, depth). Depth is carried per-entry
    // because a stack walk visits siblings out of order, so the stack's own
    // length is not the depth of the node being processed.
    let mut stack: Vec<(usize, String, usize)> = alloc::vec![(0usize, String::new(), 0usize)];

    while let Some((node, prefix, depth)) = stack.pop() {
        if depth > MAX_TRIE_DEPTH {
            return Err(Error::TooDeep("export trie"));
        }
        if !visited.insert(node) {
            // A repeated node offset is a cycle or a diamond; neither can occur
            // in a well-formed trie, and following it would not terminate.
            return Err(Error::TooDeep("export trie cycle"));
        }

        let mut cur = Cursor::at(view, node);
        let terminal_size = cur.uleb128()?;
        let terminal_size = usize::try_from(terminal_size).map_err(|_| Error::Truncated {
            what: "export node",
        })?;

        // The children always start past the terminal payload, whether or not
        // we understood it - that framing is what lets dyld skip flag bits it
        // does not know, and we honour it for the same reason.
        let children_at = cur
            .pos()
            .checked_add(terminal_size)
            .ok_or(Error::Truncated {
                what: "export node",
            })?;

        if terminal_size != 0
            && let Some(export) = parse_terminal(&mut cur, &prefix, image_base)?
        {
            if out.len() >= MAX_EXPORTS {
                return Err(Error::TooDeep("export count"));
            }
            out.push(export);
        }

        let mut cur = Cursor::at(view, children_at);
        let nchildren = cur.u8()?;
        for _ in 0..nchildren {
            let suffix = cur.cstr()?;
            let child = cur.uleb128()?;
            let child = usize::try_from(child).map_err(|_| Error::Truncated {
                what: "export child",
            })?;
            if child >= bytes.len() {
                return Err(Error::Truncated {
                    what: "export child",
                });
            }
            let mut name = String::with_capacity(prefix.len() + suffix.len());
            name.push_str(&prefix);
            name.push_str(suffix);
            stack.push((child, name, depth + 1));
        }
    }

    Ok(out)
}

/// Decode a node's terminal payload, the cursor sitting just past the
/// `terminal_size` field.
///
/// Returns `None` for the root's empty name, which a few linkers emit as a
/// terminal node and which is not a symbol anyone can import.
fn parse_terminal(cur: &mut Cursor<'_>, name: &str, image_base: Word) -> Result<Option<Export>> {
    let flags = cur.uleb128()?;
    let flags = u32::try_from(flags).map_err(|_| Error::UnknownFixup {
        format: "export trie",
        value: u32::MAX,
    })?;

    let kind = match flags & c::EXPORT_SYMBOL_FLAGS_KIND_MASK {
        c::EXPORT_SYMBOL_FLAGS_KIND_REGULAR => ExportKind::Regular,
        c::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL => ExportKind::ThreadLocal,
        c::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE => ExportKind::Absolute,
        other => {
            return Err(Error::UnknownFixup {
                format: "export symbol kind",
                value: other,
            });
        }
    };

    let (vmaddr, value) = if flags & c::EXPORT_SYMBOL_FLAGS_REEXPORT != 0 {
        let ordinal = cur.uleb128()?;
        let ordinal = u16::try_from(ordinal)
            .map_err(|_| Error::BadLibraryOrdinal(i32::try_from(ordinal).unwrap_or(-1)))?;
        // An empty string means "same name in the other library"; a non-empty
        // one is a rename, which is how umbrellas republish under new names.
        let imported = cur.cstr()?;
        let imported_name = (!imported.is_empty()).then(|| imported.to_owned());
        (
            0,
            ExportValue::Reexport {
                ordinal,
                imported_name,
            },
        )
    } else if flags & c::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER != 0 {
        let stub = cur.uleb128()?;
        let resolver = cur.uleb128()?;
        (
            rebase(stub, kind, image_base),
            ExportValue::StubAndResolver {
                resolver: rebase(resolver, kind, image_base),
            },
        )
    } else {
        (
            rebase(cur.uleb128()?, kind, image_base),
            ExportValue::Direct,
        )
    };

    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(Export {
        name: name.to_owned(),
        flags,
        kind,
        vmaddr,
        value,
    }))
}

/// Convert a trie-relative offset to an unslid VA. An absolute symbol's value
/// is not an offset into anything and must be passed through untouched.
fn rebase(offset: u64, kind: ExportKind, image_base: Word) -> Word {
    match kind {
        ExportKind::Absolute => offset,
        _ => image_base.wrapping_add(offset),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a trie by hand. `nodes[i]` is the encoded body of node `i`; the
    /// helper lays them out and patches nothing, so tests state child offsets
    /// literally, which is what makes a deliberate back-edge expressible.
    fn node(terminal: &[u8], children: &[(&str, usize)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(terminal.len() as u8);
        v.extend_from_slice(terminal);
        v.push(children.len() as u8);
        for (label, off) in children {
            v.extend_from_slice(label.as_bytes());
            v.push(0);
            v.push(*off as u8); // single-byte uleb; tests stay under 128
        }
        v
    }

    #[test]
    fn empty_trie_yields_nothing() {
        assert!(parse_trie(&[], 0).unwrap().is_empty());
    }

    /// Root with a single labelled edge to one terminal leaf - the shape every
    /// terminal-decoding test below wants, with the child offset patched to
    /// wherever the leaf actually landed.
    fn one_export(label: &str, terminal: &[u8]) -> Vec<u8> {
        let mut trie = node(&[], &[(label, 0)]);
        let leaf_at = trie.len();
        let last = trie.len() - 1;
        trie[last] = leaf_at as u8;
        trie.extend_from_slice(&node(terminal, &[]));
        trie
    }

    #[test]
    fn single_export_carries_name_and_address() {
        // root -"_f"-> [terminal: flags=0, addr=0x1234]
        let trie = one_export("_f", &[0x00, 0xb4, 0x24]);

        let exports = parse_trie(&trie, 0x1_0000_0000).unwrap();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "_f");
        assert_eq!(exports[0].vmaddr, 0x1_0000_1234);
        assert_eq!(exports[0].value, ExportValue::Direct);
        assert_eq!(exports[0].address(0x1000), Some(0x1_0000_2234));
    }

    #[test]
    fn absolute_export_is_not_slid() {
        let trie = one_export("_a", &[c::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE as u8, 0x2a]);

        let e = &parse_trie(&trie, 0x1_0000_0000).unwrap()[0];
        assert_eq!(e.kind, ExportKind::Absolute);
        assert_eq!(e.vmaddr, 0x2a);
        assert_eq!(
            e.address(0x8000),
            Some(0x2a),
            "an absolute symbol must not slide"
        );
    }

    #[test]
    fn reexport_has_no_address() {
        // flags=REEXPORT, ordinal=2, renamed to "_other".
        let mut terminal = alloc::vec![c::EXPORT_SYMBOL_FLAGS_REEXPORT as u8, 0x02];
        terminal.extend_from_slice(b"_other\0");
        let trie = one_export("_r", &terminal);

        let e = &parse_trie(&trie, 0x1_0000_0000).unwrap()[0];
        assert!(e.is_reexport());
        assert_eq!(
            e.address(0x1000),
            None,
            "a re-export must not look like address zero"
        );
        match &e.value {
            ExportValue::Reexport {
                ordinal,
                imported_name,
            } => {
                assert_eq!(*ordinal, 2);
                assert_eq!(imported_name.as_deref(), Some("_other"));
            }
            other => panic!("expected a re-export, got {other:?}"),
        }
    }

    #[test]
    fn back_edge_is_rejected_rather_than_looped() {
        // A single node whose only child points back at the root.
        let trie = node(&[], &[("x", 0)]);
        assert_eq!(
            parse_trie(&trie, 0),
            Err(Error::TooDeep("export trie cycle"))
        );
    }

    #[test]
    fn child_offset_past_the_end_is_truncated_not_panicking() {
        let trie = node(&[], &[("x", 0x7f)]);
        assert!(matches!(parse_trie(&trie, 0), Err(Error::Truncated { .. })));
    }

    #[test]
    fn truncation_at_every_prefix_is_an_error_not_a_panic() {
        let trie = one_export("_f", &[0x00, 0x10]);

        for n in 1..trie.len() {
            let _ = parse_trie(&trie[..n], 0);
        }
    }
}
