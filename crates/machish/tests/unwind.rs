#![cfg(all(feature = "std", feature = "arch-x86_64"))]

mod common;

use machish::image::{MachObject, UnwindInfo, function_starts};
use machish::spec::arch::X86_64;

/// A C++ program with a `try` block, which is what puts a personality routine
/// and an LSDA into the table at all.
const THROWS: &str = r#"
#include <cstdio>
struct Boom { int v; };
static int deep(int n) { if (n == 0) throw Boom{7}; return deep(n - 1); }
int main() {
    try { deep(3); } catch (const Boom &b) { printf("%d\n", b.v); }
    return 0;
}
"#;

fn cpp_fixture() -> Option<Vec<u8>> {
    common::build_x86_64("unwind_cpp", THROWS, &["-x", "c++", "-lc++"])
}

#[test]
fn every_entry_names_a_real_function_of_a_real_binary() {
    let Some(bytes) = common::system_binary("/bin/ls") else {
        skip!("/bin/ls unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).expect("parse");
    let info = UnwindInfo::parse(&obj, 0).expect("unwind info");
    let text = obj.section("__TEXT", "__text").expect("__text");

    assert!(!info.is_empty(), "/bin/ls has no __unwind_info");
    let starts: std::collections::BTreeSet<u64> = function_starts(&obj)
        .expect("function starts")
        .into_iter()
        .collect();

    for e in info.entries() {
        assert!(
            e.function >= text.addr && e.function < text.addr + text.size,
            "{:#x} is outside __text",
            e.function
        );
        // Two independent tables produced by the same linker from the same
        // facts: agreement is evidence neither decoder can give alone.
        assert!(
            starts.contains(&e.function),
            "{:#x} is not a function start",
            e.function
        );
    }
}

#[test]
fn entries_are_sorted_and_each_one_finds_itself() {
    let Some(bytes) = common::system_binary("/bin/ls") else {
        skip!("/bin/ls unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).expect("parse");
    let info = UnwindInfo::parse(&obj, 0).expect("unwind info");

    for pair in info.entries().windows(2) {
        assert!(
            pair[0].function < pair[1].function,
            "entries are not sorted"
        );
    }
    for e in info.entries() {
        assert_eq!(info.lookup(e.function), Some(e));
        // One byte into the function is still that function: the table records
        // starts, and a lookup that only matched exactly would answer nothing
        // for every address a program actually unwinds from.
        assert_eq!(info.lookup(e.function + 1), Some(e));
    }
}

#[test]
fn an_address_outside_the_covered_range_has_no_entry() {
    let Some(bytes) = common::system_binary("/bin/ls") else {
        skip!("/bin/ls unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).expect("parse");
    let info = UnwindInfo::parse(&obj, 0).expect("unwind info");
    let first = info.entries().first().expect("at least one entry").function;

    assert_eq!(
        info.lookup(first - 1),
        None,
        "an address below the first function"
    );
    assert_eq!(
        info.lookup(0xffff_ffff_0000),
        None,
        "an address in nothing at all"
    );
}

#[test]
fn the_slide_moves_every_address_and_nothing_else() {
    let Some(bytes) = common::system_binary("/bin/ls") else {
        skip!("/bin/ls unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).expect("parse");
    let unslid = UnwindInfo::parse(&obj, 0).expect("unwind info");
    let slid = UnwindInfo::parse(&obj, 0x1_0000).expect("unwind info");

    assert_eq!(unslid.entries().len(), slid.entries().len());
    for (a, b) in unslid.entries().iter().zip(slid.entries()) {
        assert_eq!(a.function + 0x1_0000, b.function);
        assert_eq!(a.encoding, b.encoding, "an encoding is not an address");
        assert_eq!(a.personality_slot.map(|p| p + 0x1_0000), b.personality_slot);
        assert_eq!(a.lsda.map(|l| l + 0x1_0000), b.lsda);
        // Slid or not, an entry that can catch something still can.
        assert_eq!(a.can_handle(), b.can_handle());
    }
}

#[test]
fn a_cpp_binary_carries_a_personality_and_an_lsda_and_a_c_one_does_not() {
    let Some(bytes) = cpp_fixture() else {
        skip!("cannot build an x86_64 C++ fixture")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).expect("parse");
    let info = UnwindInfo::parse(&obj, 0).expect("unwind info");

    let handlers: Vec<_> = info.entries().iter().filter(|e| e.can_handle()).collect();
    assert!(
        !handlers.is_empty(),
        "a program with a try block has no LSDA"
    );
    let except = obj
        .section("__TEXT", "__gcc_except_tab")
        .expect("__gcc_except_tab");
    for e in &handlers {
        let lsda = e.lsda.expect("can_handle implies an LSDA");
        assert!(
            lsda >= except.addr && lsda < except.addr + except.size,
            "LSDA {lsda:#x} is outside __gcc_except_tab"
        );
    }

    // The negative half: `/bin/ls` is C, so nothing in it catches anything, and
    // a decoder that invented an LSDA index would show up here rather than in a
    // fixture written to have one.
    let Some(ls) = common::system_binary("/bin/ls") else {
        return;
    };
    let ls = MachObject::parse_any(&ls, X86_64).expect("parse");
    let ls_info = UnwindInfo::parse(&ls, 0).expect("unwind info");
    assert!(
        ls_info.entries().iter().all(|e| e.lsda.is_none()),
        "/bin/ls catches something?"
    );
}

#[test]
fn a_personality_slot_is_a_pointer_the_linker_binds() {
    // The field is the address *of the pointer*, not of the routine: the slot
    // lives in `__DATA_CONST,__got` and holds `___gxx_personality_v0` only
    // after binding. A decoder reporting the routine's address would have
    // nothing to report at all in an unbound image.
    let Some(bytes) = cpp_fixture() else {
        skip!("cannot build an x86_64 C++ fixture")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).expect("parse");
    let info = UnwindInfo::parse(&obj, 0).expect("unwind info");

    let slot = info
        .entries()
        .iter()
        .find_map(|e| e.personality_slot)
        .expect("a C++ binary has a personality routine");
    let sect = obj.section_at(slot).expect("the slot is in a section");
    assert!(
        sect.is_non_lazy_pointers(),
        "a personality slot should be a bound pointer, not {}",
        sect.sect_name
    );
}
