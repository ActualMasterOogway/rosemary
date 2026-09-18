//! The guards every module used to carry a copy of, kept once.
//!
//! Twenty-one modules had an `every_listed_name_resolves` of their own, because
//! their name list and their `match` were separate declarations. Here the two
//! come from one table, so the property is checked once against this fixture
//! rather than once per module against each.

use crate::{Exports, Module, NonNull, Recording};

extern "C" fn one() -> u32 {
    1
}

extern "C" fn two() -> u32 {
    2
}

mod leaf {
    use super::{one, two};

    crate::exports! {
        functions {
            "leaf_one" => one,
            /// An alias group: one implementation, several spellings a guest
            /// may import it by.
            "leaf_two" | "leaf_two_np" | "leaf_two_deprecated" => two,
            #[cfg(any())]
            "leaf_never" => one,
            #[cfg(not(any()))]
            "leaf_always" => one,
        }
        data {
            "_leaf_datum" => 0xdead_beef,
            "_OBJC_CLASS_$_Leaf" => 0x1000,
        }
    }
}

mod other {
    use super::two;

    crate::exports! {
        functions {
            "other_one" => two,
        }
    }
}

mod group {
    crate::group! { super::leaf, super::other }
}

#[test]
fn every_listed_name_resolves() {
    let missing: crate::Names = leaf::names()
        .into_iter()
        .filter(|n| leaf::symbol(n).is_none())
        .collect();
    assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
}

#[test]
fn an_alias_group_resolves_to_one_address() {
    let canonical = leaf::symbol("leaf_two").expect("the canonical spelling");
    for alias in ["leaf_two_np", "leaf_two_deprecated"] {
        assert_eq!(leaf::symbol(alias), Some(canonical), "{alias}");
    }
    assert_ne!(canonical, leaf::symbol("leaf_one").expect("a second entry"));
}

#[test]
fn a_cfg_gated_entry_is_absent_from_names_and_from_symbol() {
    assert!(!leaf::names().contains(&"leaf_never"));
    assert_eq!(leaf::symbol("leaf_never"), None);
    // The other side of the same gate, so that a gate which excluded everything
    // would not pass this test by accident.
    assert!(leaf::names().contains(&"leaf_always"));
    assert!(leaf::symbol("leaf_always").is_some());
}

#[test]
fn a_published_function_carries_the_mach_o_underscore() {
    let mut out = Recording::default();
    leaf::provide(&mut out);
    let published = out.symbols();
    assert!(published.contains(&"_leaf_one"), "{published:?}");
    assert!(!published.contains(&"leaf_one"), "{published:?}");
}

/// A data name is published exactly as written: `_OBJC_CLASS_$_Leaf` has no
/// bare C spelling to derive one from.
#[test]
fn a_published_data_symbol_keeps_the_spelling_it_was_given() {
    let mut out = Recording::default();
    leaf::provide(&mut out);
    let names: alloc::vec::Vec<&str> = out.data.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["_leaf_datum", "_OBJC_CLASS_$_Leaf"]);
}

/// Data is published but is not part of the function lookup a `dlsym` uses.
#[test]
fn a_data_symbol_is_not_a_name_symbol_answers() {
    assert_eq!(leaf::symbol("_leaf_datum"), None);
    assert!(!leaf::names().contains(&"_leaf_datum"));
}

#[test]
fn a_group_answers_every_members_name() {
    let missing: crate::Names = group::names()
        .into_iter()
        .filter(|n| group::symbol(n).is_none())
        .collect();
    assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    assert_eq!(group::symbol("leaf_one"), leaf::symbol("leaf_one"));
    assert_eq!(group::symbol("other_one"), other::symbol("other_one"));
    assert_eq!(group::symbol("nothing_defines_this"), None);
}

#[test]
fn a_group_publishes_every_members_symbols() {
    let mut out = Recording::default();
    group::provide(&mut out);
    let published = out.symbols();
    for expected in ["_leaf_one", "_leaf_datum", "_other_one"] {
        assert!(published.contains(&expected), "{expected}: {published:?}");
    }
}

#[test]
fn a_null_address_is_not_published() {
    let mut recorded = Recording::default();
    {
        let mut out = NonNull(&mut recorded);
        out.function("_present", 0x1000);
        out.function("_absent", 0);
        out.data("_absent_datum", 0);
    }
    assert_eq!(recorded.symbols(), ["_present"]);
}

#[test]
fn a_denied_symbol_is_not_published() {
    fn deny(name: &str) -> bool {
        name == "_withheld"
    }

    let mut recorded = Recording::default();
    {
        let mut out = crate::Denying {
            inner: &mut recorded,
            deny,
        };
        out.function("_kept", 0x1000);
        out.function("_withheld", 0x2000);
    }
    assert_eq!(recorded.symbols(), ["_kept"]);
}

/// The invariant the crates above depend on: the address a link gets and the
/// address a `dlsym` gets are the same one, because both come from this table.
#[test]
fn a_published_address_is_the_one_symbol_answers() {
    let mut out = Recording::default();
    group::provide(&mut out);
    for (mach_o, address) in &out.functions {
        let bare = mach_o.strip_prefix('_').expect("the Mach-O underscore");
        assert_eq!(group::symbol(bare), Some(*address), "{mach_o}");
    }
}

#[test]
fn a_module_row_answers_the_same_as_the_module() {
    let row: Module = leaf::MODULE;
    assert!(row.path.ends_with("leaf"), "{}", row.path);
    assert_eq!((row.symbol)("leaf_one"), leaf::symbol("leaf_one"));
    assert_eq!((row.names)(), leaf::names());

    // A group is a row too, which is what lets one nest inside another.
    let nested: Module = group::MODULE;
    assert_eq!((nested.symbol)("other_one"), other::symbol("other_one"));
}
