#![cfg(all(feature = "std", feature = "arch-x86_64"))]
//! M1.9 acceptance: initializers, terminators, and thread-local descriptors.
//!
//! Every assertion here is about what the loader *reports*, never about what it
//! runs. Nothing in this file calls a guest function, and nothing allocates a
//! thread block - the milestone's whole claim is that the loader can hand a
//! driver an exact list and stop.

mod common;

use machish::extensions::Extension;
use machish::extensions::tlv::TlvExtension;
use machish::fixups::{Fixup, LibraryRef, decode_image};
use machish::image::MachObject;
use machish::loader::Loader;
use machish::memory::BufferSpace;
use machish::resolve::{NativeDylibProvider, SymbolProvider, SyntheticDylib, eager_binding_shims};
use machish::spec::arch::X86_64;

/// Three constructors with explicit priorities, a destructor, and two
/// thread-locals - enough that ordering and count are both checkable.
const SRC: &str = r#"
#include <stdio.h>

int trace[8];
int n = 0;

__attribute__((constructor(101))) static void first(void)  { trace[n++] = 1; }
__attribute__((constructor(102))) static void second(void) { trace[n++] = 2; }
__attribute__((constructor(103))) static void third(void)  { trace[n++] = 3; }
__attribute__((destructor))       static void last(void)   { trace[n++] = 9; }

_Thread_local int tls_counter = 0;
_Thread_local double tls_scale = 1.5;

int bump(void) { tls_counter++; return (int)(tls_counter * tls_scale); }
int main(void) { printf("%d %d\n", bump(), trace[0]); return 0; }
"#;

fn link(name: &str, extra: &[&str]) -> Option<(Vec<u8>, machish::report::LinkReport)> {
    let bytes = common::build_x86_64(name, SRC, extra)?;
    let mut mem = BufferSpace::new(0x1_0000_0000);
    let shims = eager_binding_shims();
    // `__tlv_bootstrap` is denied by the native provider on purpose, so the
    // shim is not optional - it is the intended way to supply one.
    let mut tlv_shim = SyntheticDylib::new("<tlv shims>");
    tlv_shim
        .flat(true)
        .define_fn("__tlv_bootstrap", 0x7f00_0000 as *const ());
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 3] = [&tlv_shim, &shims, &native];

    let report = Loader::new(X86_64)
        .link(&bytes, &format!("/tmp/{name}"), &mut mem, &providers)
        .unwrap();
    Some((bytes, report))
}

#[test]
fn initializers_are_collected_slid_and_in_section_order() {
    let Some((bytes, report)) = link("ext_init", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let img = report.root().unwrap();

    // Either encoding is acceptable - which one appears depends on the
    // toolchain's vintage, and both must be read.
    let sect = obj
        .sections
        .iter()
        .find(|s| s.is_initializers())
        .expect("the fixture must have an initializer section");
    let stride = if sect.is_mod_init() { 8 } else { 4 };
    assert_eq!(
        img.initializers.len() as u64,
        sect.size / stride,
        "one initializer per {} entry",
        sect.sect_name
    );

    // Every entry must point into this image's `__TEXT`, slid. Structural, not
    // literal: the addresses are whatever the linker chose.
    let text: Vec<_> = obj
        .sections
        .iter()
        .filter(|s| s.seg_name == "__TEXT")
        .collect();
    for &init in &img.initializers {
        let unslid = init.wrapping_sub(img.slide);
        assert!(
            text.iter().any(|s| s.contains(unslid)),
            "initializer {init:#x} is not in __TEXT"
        );
    }

    // Priorities 101/102/103 make the linker emit them in that order, and the
    // report must not have reordered them.
    assert!(img.initializers.len() >= 3);
    let mut sorted = img.initializers.clone();
    sorted.sort_unstable();
    assert_eq!(
        img.initializers, sorted,
        "initializers must stay in section order"
    );

    // And nothing was called: `main` was never entered, so the entry point is
    // merely reported.
    assert!(img.entry.is_some());
}

#[test]
fn terminators_match_the_section_and_never_leak_into_initializers() {
    let Some((bytes, report)) = link("ext_term", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let img = report.root().unwrap();

    // Deliberately not "the fixture has a destructor, so this is non-empty".
    // Clang registers `__attribute__((destructor))` functions with
    // `__cxa_atexit` from a constructor rather than emitting `__mod_term_func`,
    // so the honest claim is that the report matches whatever the image
    // actually contains - which for a current toolchain is nothing.
    let expected: u64 = obj
        .sections
        .iter()
        .filter(|s| s.is_mod_term())
        .map(|s| s.size / 8)
        .sum();
    assert_eq!(img.terminators.len() as u64, expected);

    for t in &img.terminators {
        assert!(
            !img.initializers.contains(t),
            "a terminator leaked into the initializer list"
        );
    }
}

#[test]
fn thread_local_descriptors_are_enumerated_with_their_three_words() {
    let Some((bytes, report)) = link("ext_tlv", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let img = report.root().unwrap();

    let sect = obj
        .sections
        .iter()
        .find(|s| s.is_thread_local_variables())
        .expect("the fixture must have a __thread_vars section");
    assert_eq!(
        img.thread_locals.len() as u64,
        sect.size / 24,
        "one descriptor per 24-byte tlv_descriptor"
    );
    assert!(
        img.thread_locals.len() >= 2,
        "the fixture declares two thread-locals"
    );

    for d in &img.thread_locals {
        // The thunk is a bound import; after linking it is the shim's address.
        assert_eq!(
            d.thunk, 0x7f00_0000,
            "the tlv thunk was not bound to the shim"
        );
        // The key is allocated by a runtime we deliberately do not provide.
        assert_eq!(d.key, 0, "no pthread key should have been allocated");
        assert!(sect.contains(d.at.wrapping_sub(img.slide)));
    }

    // The offsets must be distinct - two variables in one thread block.
    let mut offsets: Vec<u64> = img.thread_locals.iter().map(|d| d.offset).collect();
    offsets.sort_unstable();
    offsets.dedup();
    assert_eq!(
        offsets.len(),
        img.thread_locals.len(),
        "two variables share an offset"
    );
}

/// The fact the whole thread-local block layout rests on, checked against the
/// link editor rather than assumed from its documentation.
///
/// A descriptor says "offset 0x8" and nothing about what it is an offset from.
/// The claim is that it is relative to the start of `__thread_data` - so for
/// each thread-local, `template.initial_va + offset` must be the address `ld64`
/// gave that variable's storage, which it records under the name
/// `_<var>$tlv$init`. Getting this wrong by one section would produce a runtime
/// that reads and writes plausible neighbouring variables.
#[test]
fn a_descriptor_offset_is_relative_to_the_start_of_the_thread_block() {
    let Some((bytes, report)) = link("ext_tlv_layout", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let img = report.root().unwrap();
    let template = img
        .tlv_template
        .expect("the fixture declares thread-locals");

    // Where `ld64` actually put each variable's initial storage.
    let table = machish::image::SymbolTable::new(
        *obj.view(),
        obj.symtab().expect("a symbol table"),
        obj.dysymtab(),
    )
    .unwrap();
    let mut storage: Vec<u64> = table
        .iter()
        .filter_map(|(_, sym)| {
            let name = table.name(&sym).ok()?;
            (!sym.is_stab() && name.ends_with("$tlv$init")).then_some(sym.n_value)
        })
        .collect();
    storage.sort_unstable();
    assert!(storage.len() >= 2, "the fixture declares two thread-locals");

    let mut described: Vec<u64> = img
        .thread_locals
        .iter()
        .map(|d| {
            template
                .initial_va
                .wrapping_sub(img.slide)
                .wrapping_add(d.offset)
        })
        .collect();
    described.sort_unstable();
    assert_eq!(
        described, storage,
        "a descriptor's offset is not relative to the start of __thread_data"
    );

    // The block has to hold the last variable whole, and the initialised prefix
    // is `__thread_data` alone - everything past it is zero, which is what lets
    // a bootstrap copy the template and zero the rest without re-reading the
    // section table.
    let data = obj
        .sections
        .iter()
        .find(|s| s.is_thread_local_data())
        .unwrap();
    let bss = obj
        .sections
        .iter()
        .find(|s| s.is_thread_local_bss())
        .unwrap();
    assert_eq!(template.initial_va.wrapping_sub(img.slide), data.addr);
    assert_eq!(template.initial_size, data.size);
    assert_eq!(template.total_size, bss.addr + bss.size - data.addr);
    assert!(template.total_size >= template.initial_size);
}

#[test]
fn descriptors_are_read_correctly_from_a_chained_fixups_image_too() {
    // The reason `observe` runs against memory rather than the file: in a
    // chained image the descriptor's first word on disk is a packed chain link,
    // not a pointer. Both encodings must give the same shape.
    let Some((_, chained)) = link("ext_tlv_chained", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let Some((_, opcodes)) = link("ext_tlv_opcodes", &["-Wl,-no_fixup_chains"]) else {
        skip!("cc -arch x86_64 unavailable")
    };

    let a = chained.root().unwrap();
    let b = opcodes.root().unwrap();
    assert_eq!(a.thread_locals.len(), b.thread_locals.len());
    assert_eq!(a.initializers.len(), b.initializers.len());
    for d in a.thread_locals.iter().chain(&b.thread_locals) {
        assert_eq!(d.thunk, 0x7f00_0000, "a descriptor's thunk was misread");
    }
}

#[test]
fn the_tlv_extension_redirects_bootstrap_imports_to_a_flat_lookup() {
    let Some(bytes) = common::build_x86_64("ext_tlv_redirect", SRC, &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let (_, mut fixups) = decode_image(&obj).unwrap();

    // Before: the bootstrap import names a specific library.
    let named_before = fixups.iter().any(|f| {
        matches!(f, Fixup::Bind { symbol, .. }
            if symbol.name == "__tlv_bootstrap"
                && matches!(symbol.library, LibraryRef::InstallName(_)))
    });
    assert!(
        named_before,
        "the fixture should import __tlv_bootstrap two-level"
    );

    TlvExtension::new()
        .transform_fixups(&obj, &mut fixups)
        .unwrap();

    // After: it is flat, so one shim can serve every image without knowing
    // which library each of them named.
    for f in &fixups {
        if let Fixup::Bind { symbol, .. } = f
            && symbol.name == "__tlv_bootstrap"
        {
            assert_eq!(symbol.library, LibraryRef::Flat);
        }
    }
}

#[test]
fn the_tlv_extension_can_rename_the_import() {
    let Some(bytes) = common::build_x86_64("ext_tlv_rename", SRC, &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let (_, mut fixups) = decode_image(&obj).unwrap();

    let mut ext = TlvExtension::new();
    ext.rename_to("_machish_tlv_bootstrap");
    ext.transform_fixups(&obj, &mut fixups).unwrap();

    let renamed = fixups
        .iter()
        .filter(
            |f| matches!(f, Fixup::Bind { symbol, .. } if symbol.name == "_machish_tlv_bootstrap"),
        )
        .count();
    assert!(renamed > 0, "no import was renamed");
    assert!(
        !fixups.iter().any(|f| matches!(f, Fixup::Bind { symbol, .. }
            if symbol.name == "__tlv_bootstrap")),
        "the original name survived the rename"
    );
}

#[test]
fn a_bare_loader_reports_nothing_optional() {
    let Some(bytes) = common::build_x86_64("ext_bare", SRC, &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let mut mem = BufferSpace::new(0x1_0000_0000);
    let shims = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 2] = [&shims, &native];

    let report = Loader::bare(X86_64)
        .link(&bytes, "/tmp/ext_bare", &mut mem, &providers)
        .unwrap();
    let img = report.root().unwrap();
    assert!(img.initializers.is_empty());
    assert!(img.thread_locals.is_empty());
    // Binding still happened; only the optional passes were skipped.
    assert!(!img.bindings.is_empty());
}
