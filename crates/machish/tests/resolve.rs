#![cfg(all(feature = "std", feature = "arch-x86_64"))]
//! M1.7 acceptance: two-level resolution and the high-level-emulation path.
//!
//! The claim under test is the crate's central bet: that an Intel binary's
//! imports can be satisfied by *native arm64 host functions* already loaded in
//! this process, with no Intel library code translated and no system dylib on
//! disk. [`ls_links_against_the_host_and_every_address_is_real`] is where that
//! is checked rather than asserted - every resolved address is fed back to
//! `dladdr`, which answers only for addresses inside a genuinely loaded image.

mod common;

use std::ffi::CStr;

use machish::fixups::{Binder, Fixup, LibraryRef, StubMap, SymbolRef, decode_image};
use machish::image::{MachObject, map_segments};
use machish::memory::BufferSpace;
use machish::resolve::{
    NativeDylibProvider, Resolved, SymbolProvider, SyntheticDylib, eager_binding_shims,
};
use machish::spec::arch::X86_64;

const LS: &str = "/bin/ls";

/// Where `dladdr` says an address lives: the image path and the symbol name.
fn dladdr(addr: u64) -> Option<(String, Option<String>)> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `dladdr` reads the address as an opaque value and never
    // dereferences it, and `info` is a live, correctly-typed, zeroed
    // out-parameter. A zero return means "not in any loaded image".
    let ok = unsafe { libc::dladdr(addr as usize as *const libc::c_void, &mut info) };
    if ok == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: on success `dladdr` sets `dli_fname` to a NUL-terminated string
    // owned by the dynamic linker and valid for the lifetime of the image;
    // `dli_sname` is either null (checked) or the same.
    let fname = unsafe { CStr::from_ptr(info.dli_fname) }
        .to_string_lossy()
        .into_owned();
    let sname = if info.dli_sname.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(info.dli_sname) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    Some((fname, sname))
}

#[test]
fn ls_links_against_the_host_and_every_address_is_real() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();
    let stubs = StubMap::build(&obj).unwrap();

    // The denylist refuses `___stack_chk_guard` on purpose: it is the *host's*
    // stack canary, and handing the guest a pointer to it wires two unrelated
    // programs' stack-smashing detection together. Supplying our own is the
    // whole intended workflow - a refused symbol is a prompt to virtualize, and
    // this is what virtualizing the simplest one looks like.
    static GUEST_STACK_GUARD: u64 = 0x0000_dead_beef_0000;
    let mut virtualized = SyntheticDylib::new("<test virtualization>");
    virtualized.flat(true).define_data(
        "___stack_chk_guard",
        &raw const GUEST_STACK_GUARD as usize as u64,
    );

    let shims = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 3] = [&virtualized, &shims, &native];

    let outcome = Binder {
        slide: mapped.slide,
        providers: &providers,
        self_exports: None,
        main_executable: None,
    }
    .apply(&fixups, &mut mem, &stubs)
    .unwrap();

    // Anything left over is either a symbol needing virtualization (which the
    // embedder must shim, and the denylist deliberately surfaces) or a real
    // gap. Name them, so a failure says which.
    let names: Vec<&str> = outcome.missing.iter().map(|m| m.symbol.as_str()).collect();
    assert!(
        outcome.missing.is_empty(),
        "{} of {LS}'s imports went unresolved: {names:?}",
        outcome.missing.len()
    );
    assert!(!outcome.bindings.is_empty());

    let mut checked = 0;
    for b in &outcome.bindings {
        if !b.is_satisfied() {
            // A deliberately-absent shim, e.g. `dyld_stub_binder`.
            continue;
        }
        let (image, _) = dladdr(b.resolved).unwrap_or_else(|| {
            panic!(
                "{} resolved to {:#x}, which is in no loaded image",
                b.symbol, b.resolved
            )
        });
        assert!(!image.is_empty());
        checked += 1;
    }
    assert!(
        checked > 50,
        "expected {LS} to import many host functions, got {checked}"
    );
}

#[test]
fn resolved_addresses_are_the_hosts_own_implementations() {
    // The point of high-level emulation, stated as an assertion: the address
    // written into the guest's slot for `_strlen` is the *same address* this
    // process would call for `strlen`. True on either host, which is why the
    // assertions below are about the host's own C library rather than about macOS.
    let native = NativeDylibProvider::system();
    let sym = SymbolRef {
        name: "_strlen",
        library: LibraryRef::Flat,
        weak_import: false,
    };
    let r = native.resolve(&sym).expect("_strlen must resolve");

    let (image, name) = dladdr(r.addr).expect("resolved address is in no loaded image");
    // Not an equality check: macOS routes `strlen` through
    // `libsystem_platform`'s CPU-dispatched `_platform_strlen`, so the symbol
    // `dladdr` names is an implementation detail of the host's own libc. What
    // must hold is that we landed on *a* host strlen, in a system image.
    //
    // And glibc may not name it at all: `strlen` there is an IFUNC, so the
    // address resolves to a CPU-specific implementation that is a *local*
    // symbol and appears in no dynamic symbol table for `dladdr` to read back.
    // The image below still identifies it, which is the half of the claim that
    // matters - the address is the host's own.
    if let Some(name) = name {
        assert!(
            name.contains("strlen"),
            "resolved to {name} in {image}, which is not a strlen"
        );
    }
    // And it is genuinely the host's own C library, not something we loaded. Spelled
    // for both: macOS splits libc across `libsystem_*` under `dyld`, and musl and
    // glibc put it in one `libc`.
    assert!(
        ["libsystem", "libSystem", "dyld", "libc", "ld-musl", "ld-linux"]
            .iter()
            .any(|c| image.contains(c)),
        "unexpected host image for strlen: {image}"
    );
}

#[test]
fn a_synthetic_shim_layered_ahead_wins() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    // Pick a symbol the image actually imports, so the test cannot pass by
    // shimming something that is never bound.
    let (_, fixups) = decode_image(&obj).unwrap();
    let target = fixups
        .iter()
        .find_map(|f| match f {
            Fixup::Bind { symbol, .. } if symbol.name == "_strlen" => Some(symbol.name),
            _ => None,
        })
        .unwrap_or_else(|| skip_strlen());

    let mut shim = SyntheticDylib::new("shims");
    shim.flat(true)
        .define(target, Resolved::function(0x5150_0000).with_tag(99));
    let native = NativeDylibProvider::system();
    let eager = eager_binding_shims();
    let providers: [&dyn SymbolProvider; 3] = [&shim, &eager, &native];

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let stubs = StubMap::build(&obj).unwrap();
    let outcome = Binder {
        slide: mapped.slide,
        providers: &providers,
        self_exports: None,
        main_executable: None,
    }
    .apply(&fixups, &mut mem, &stubs)
    .unwrap();

    let bound: Vec<_> = outcome
        .bindings
        .iter()
        .filter(|b| b.symbol == target)
        .collect();
    assert!(!bound.is_empty());
    for b in bound {
        assert_eq!(b.resolved, 0x5150_0000, "the host's strlen beat the shim");
        assert_eq!(b.tag, 99);
    }
}

fn skip_strlen() -> &'static str {
    panic!("/bin/ls is expected to import _strlen");
}

#[test]
fn a_shim_claiming_the_wrong_install_name_does_not_satisfy_the_import() {
    // This is the whole reason `SymbolProvider` takes a `SymbolRef` rather than
    // a `&str`. Under a flat search this shim would win; under two-level
    // scoping it must not even be consulted.
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();

    // Confirm the import really is two-level, or the test proves nothing.
    let two_level = fixups.iter().any(|f| {
        matches!(f, Fixup::Bind { symbol, .. }
            if symbol.name == "_strlen"
                && matches!(symbol.library, LibraryRef::InstallName(_)))
    });
    assert!(two_level, "{LS} should import _strlen from a named library");

    let mut impostor = SyntheticDylib::new("@rpath/libnotlibsystem.dylib");
    impostor.define("_strlen", Resolved::function(0xbad0_0000));
    let eager = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 3] = [&impostor, &eager, &native];

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let stubs = StubMap::build(&obj).unwrap();
    let outcome = Binder {
        slide: mapped.slide,
        providers: &providers,
        self_exports: None,
        main_executable: None,
    }
    .apply(&fixups, &mut mem, &stubs)
    .unwrap();

    for b in outcome.bindings.iter().filter(|b| b.symbol == "_strlen") {
        assert_ne!(
            b.resolved, 0xbad0_0000,
            "a shim claiming a different install name satisfied a two-level import"
        );
        assert!(
            dladdr(b.resolved).is_some(),
            "_strlen should have fallen through to the host"
        );
    }
}

#[test]
fn virtualization_candidates_do_not_reach_the_host() {
    // The denylist's job: these must surface as unresolved so an embedder is
    // forced to shim them, rather than silently binding the guest to host state.
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 1] = [&native];
    for name in [
        "_environ",
        "_objc_msgSend",
        "__tlv_bootstrap",
        "__dyld_image_count",
    ] {
        let sym = SymbolRef {
            name,
            library: LibraryRef::Flat,
            weak_import: false,
        };
        assert_eq!(
            machish::resolve::resolve_in(&providers, &sym),
            None,
            "{name} must not pass through to the host"
        );
    }
}
