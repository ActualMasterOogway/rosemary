//! What an import *means*: symbol providers and two-level lookup.
//!
//! This is the seam that makes the crate a high-level emulation layer rather
//! than a translator. A provider answers an import with a **native arm64 host
//! function pointer**, so a guest `call _strlen` ends up at the host's real
//! `strlen` instead of at translated Intel library code. Nothing else in the
//! loader has to know that happened.
//!
//! ## Why the trait is not `resolve(&str) -> Option<Word>`
//!
//! That is `selfish`'s signature (`selfish/src/resolve/mod.rs:16-21`) and it is
//! right for ELF, whose namespace is flat: a name has one meaning per process.
//! Mach-O's is not. Under a two-level namespace, `_write` from
//! `/usr/lib/libSystem.B.dylib` and `_write` from a bundled `libmine.dylib` are
//! *different bindings*, and a provider list searched first-match-wins would
//! quietly cross-wire them — the bug would be an app calling the wrong
//! function, with no error anywhere.
//!
//! So the query carries the library ([`SymbolRef::library`]), and providers get
//! a [`provides`](SymbolProvider::provides) predicate checked *before*
//! `resolve` for a two-level reference. A flat reference skips it, which is
//! exactly the pre-10.1 behaviour that flag is asking for.
//!
//! ## Ordering is the safety mechanism
//!
//! Blanket `dlsym` is illegal here. `_environ`, `___stack_chk_guard`,
//! `_mach_task_self_`, the `_dyld_*` family, and the entire Objective-C runtime
//! need *virtualization*, not passthrough: handing the guest the host's copy of
//! them wires guest state to host state in ways nothing later can untangle.
//! There is no way to detect that from the name alone, so the mechanism is
//! provider order — hand-written shims first, `dlsym` last:
//!
//! ```ignore
//! let providers: [&dyn SymbolProvider; 3] = [
//!     &hand_written_shims,  // SyntheticDylib: must be virtualized
//!     &tlv_shims,           // SyntheticDylib: _tlv_bootstrap and friends
//!     &native,              // NativeDylibProvider: everything else
//! ];
//! ```

mod synthetic;

#[cfg(feature = "std")]
mod native;

pub use synthetic::SyntheticDylib;

#[cfg(feature = "std")]
pub use native::NativeDylibProvider;

use crate::fixups::{LibraryRef, SymbolRef};
use crate::raw::Word;

/// What a resolved symbol names, which is what an embedder needs to decide
/// whether a call site can be intercepted at all.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum SymbolKind {
    #[default]
    Function,
    Data,
    /// A thread-local variable's descriptor, not any thread's copy.
    ThreadLocal,
    /// Deliberately not present. Distinct from an unresolved symbol: this is a
    /// provider saying "this name exists and its address is zero", which is the
    /// correct answer for a satisfied-but-absent weak import.
    Absent,
}

/// A provider's answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Resolved {
    /// The address to store in the slot. For high-level emulation this is a
    /// **host** pointer, valid in the host's address space, which works only
    /// because the guest image is identity-mapped.
    pub addr: Word,
    pub kind: SymbolKind,
    /// Opaque, provider-defined, echoed verbatim into
    /// [`crate::report::BindingRecord::tag`].
    ///
    /// This is how ABI metadata crosses the crate boundary without a
    /// dependency. The embedder needs to know a shim's calling convention to
    /// register it as a fast import, and the two candidate ways to say so are
    /// both worse: depending on `function_traits` would drag
    /// calling-convention vocabulary into a `no_std` loader, and a parallel
    /// enum defined here would drift from the real one. An index into the
    /// embedder's own shim table costs nothing and keeps the dependency graph a
    /// DAG.
    pub tag: u64,
}

impl Resolved {
    /// A function at `addr`, untagged.
    pub fn function(addr: Word) -> Self {
        Resolved {
            addr,
            kind: SymbolKind::Function,
            tag: 0,
        }
    }

    /// A datum at `addr`, untagged.
    pub fn data(addr: Word) -> Self {
        Resolved {
            addr,
            kind: SymbolKind::Data,
            tag: 0,
        }
    }

    /// Attach a provider-defined tag.
    pub fn with_tag(mut self, tag: u64) -> Self {
        self.tag = tag;
        self
    }
}

/// Something that can say what an import resolves to.
pub trait SymbolProvider {
    /// Whether this provider stands for the image called `install_name`.
    ///
    /// Consulted *before* [`resolve`](Self::resolve) for a two-level reference,
    /// and skipped entirely for a flat one. Matching should accept the whole
    /// install name as recorded (`/usr/lib/libSystem.B.dylib`); see
    /// [`SyntheticDylib::alias`] for the umbrella case, where one provider
    /// legitimately stands for several names.
    fn provides(&self, install_name: &str) -> bool;

    /// The address `sym` names, or `None` if this provider does not define it.
    fn resolve(&self, sym: &SymbolRef<'_>) -> Option<Resolved>;

    /// Offer every symbol this provider knows, for diagnostics. Providers
    /// backed by `dlsym` cannot enumerate and correctly do nothing here.
    fn enumerate(&self, _out: &mut dyn FnMut(&str, Resolved)) {}
}

/// The symbols eager binding makes vestigial, defined as deliberately absent.
///
/// `dyld_stub_binder` is the one every classic image imports and nothing here
/// can supply: it is dyld's lazy-binding trampoline, called from
/// `__stub_helper` when a `__la_symbol_ptr` slot is still unbound. Because this
/// crate binds every lazy slot at load time, `__stub_helper` is dead code and
/// the pointer is never read — so the honest binding is a defined zero rather
/// than either an unresolved import (which would report a failure that is not
/// one) or the host's real `dyld_stub_binder` (which would hand guest code a
/// pointer into the host dynamic linker).
///
/// Note the missing leading underscore. That is not a typo: `dyld_stub_binder`
/// is the one symbol Mach-O spells without one.
///
/// Place it anywhere in the provider chain; it defines nothing else.
pub fn eager_binding_shims() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<machish eager-binding shims>");
    lib.flat(true).define_absent("dyld_stub_binder");
    lib
}

/// Resolve `sym` against `providers`, honouring two-level scoping.
///
/// A two-level reference consults only providers that claim its library; a flat
/// or special reference consults all of them in order, first match winning.
pub fn resolve_in(providers: &[&dyn SymbolProvider], sym: &SymbolRef<'_>) -> Option<Resolved> {
    match sym.library {
        LibraryRef::InstallName(name) => providers
            .iter()
            .filter(|p| p.provides(name))
            .find_map(|p| p.resolve(sym)),
        // Everything else — flat lookup, weak lookup, an ordinal we could not
        // match to a load command — is a search across all providers. For an
        // unknown ordinal that is a deliberate softening: the alternative is
        // failing an import over a malformed number when the name alone is
        // very likely enough to find it.
        _ => providers.iter().find_map(|p| p.resolve(sym)),
    }
}
