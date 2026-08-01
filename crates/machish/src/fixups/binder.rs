//! Applying a decoded fixup stream: resolve, write, record.
//!
//! A struct rather than a trait, unlike almost everything else here, because
//! there is exactly one correct policy. A rebase adds the slide; a bind writes
//! the resolved address plus the addend. There is no architecture-specific
//! arithmetic to template over — that is precisely what
//! [`crate::fixups::FixupDecoder`] already normalized away — so a trait would
//! offer an extension point with nothing to extend and one implementation to
//! justify it.
//!
//! Binding is **eager**, including for `__la_symbol_ptr` slots dyld would have
//! left for `dyld_stub_binder`. That removes a whole class of problem rather
//! than deferring it: a lazy bind resolved on first call would re-enter this
//! linker from *inside recompiled guest code*, mid-translation, which is a
//! re-entrancy hazard with no good answer. The cost is that programs able to
//! observe binding laziness will differ; see the crate's scope notes.

use alloc::string::ToString;
use alloc::vec::Vec;

use crate::error::Result;
use crate::exports::{ExportTable, ExportValue};
use crate::fixups::{BindKind, Fixup, LibraryRef, StubMap, SymbolRef};
use crate::memory::AddressSpace;
use crate::raw::Word;
use crate::report::{BindingRecord, MissingSymbol};
use crate::resolve::{Resolved, SymbolKind, SymbolProvider, resolve_in};

/// What applying a fixup stream produced.
#[derive(Clone, Debug, Default)]
pub struct BindOutcome {
    pub bindings: Vec<BindingRecord>,
    pub missing: Vec<MissingSymbol>,
    /// Pointers slid.
    pub rebases: usize,
}

/// Resolves and writes a decoded fixup stream into an address space.
pub struct Binder<'s> {
    /// Added to every unslid address in the stream. Zero when the image landed
    /// on its preferred base, which is the case the recompiler's identity
    /// mapping wants.
    pub slide: Word,
    /// Consulted in order; see [`crate::resolve`] for why order is the safety
    /// mechanism rather than a convenience.
    pub providers: &'s [&'s dyn SymbolProvider],
    /// This image's own exports, for `BIND_SPECIAL_DYLIB_SELF`. A self-bind
    /// that cannot find its own definition would otherwise fall through to the
    /// providers and pick up the *host's* copy of the symbol, which is a
    /// silently wrong binding rather than a loud failure.
    pub self_exports: Option<&'s ExportTable>,
    /// The main executable's exports and slide, for
    /// `BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE` — how a plugin bundle calls back
    /// into the app that loaded it.
    pub main_executable: Option<(&'s ExportTable, Word)>,
}

impl<'s> Binder<'s> {
    /// A binder with no providers and no exports; useful for applying a
    /// rebase-only stream.
    pub fn new(slide: Word) -> Self {
        Binder {
            slide,
            providers: &[],
            self_exports: None,
            main_executable: None,
        }
    }

    /// Resolve and write every fixup in `stream`.
    ///
    /// `stubs` supplies [`BindingRecord::stub_va`]; pass
    /// [`StubMap::default`] when the image has none.
    pub fn apply<M: AddressSpace>(
        &self,
        stream: &[Fixup<'_>],
        mem: &mut M,
        stubs: &StubMap,
    ) -> Result<BindOutcome> {
        let mut out = BindOutcome::default();

        for fixup in stream {
            match fixup {
                Fixup::Rebase { at } => {
                    self.rebase(*at, mem)?;
                    out.rebases += 1;
                }
                Fixup::RebaseTo { at, target } => {
                    let value = target.wrapping_add(self.slide);
                    mem.write(at.wrapping_add(self.slide), &value.to_le_bytes())?;
                    out.rebases += 1;
                }
                Fixup::Bind {
                    at,
                    symbol,
                    addend,
                    kind,
                } => {
                    self.bind(*at, symbol, *addend, *kind, mem, stubs, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    /// Add the slide to the pointer already stored at `at`.
    fn rebase<M: AddressSpace>(&self, at: Word, mem: &mut M) -> Result<()> {
        let addr = at.wrapping_add(self.slide);
        if self.slide == 0 {
            // The stored value is already correct. Skipping the read-modify-
            // write is not just an optimization: it means an image loaded at
            // its preferred base needs no write permission on the pages it
            // rebases, which is the common case for an identity-mapped guest.
            return Ok(());
        }
        let mut buf = [0u8; 8];
        mem.read(addr, &mut buf)?;
        let value = u64::from_le_bytes(buf).wrapping_add(self.slide);
        mem.write(addr, &value.to_le_bytes())
    }

    #[allow(clippy::too_many_arguments)]
    fn bind<M: AddressSpace>(
        &self,
        at: Word,
        symbol: &SymbolRef<'_>,
        addend: i64,
        kind: BindKind,
        mem: &mut M,
        stubs: &StubMap,
        out: &mut BindOutcome,
    ) -> Result<()> {
        let slot_va = at.wrapping_add(self.slide);
        let stub_va = stubs.stub_for(at).map(|s| s.wrapping_add(self.slide));
        let library = symbol.library.install_name().map(|n| n.to_string());

        let resolved = self.lookup(symbol);

        let Some(r) = resolved else {
            // A weak-lookup bind is conditional by definition: it applies only
            // if some *other* image already defines the name. Unresolved, the
            // correct action is to leave the slot exactly as the linker wrote
            // it — or as the ordinary bind stream already wrote it, since a
            // coalesced symbol is commonly bound twice — and to say nothing.
            // It is not a missing symbol; it is a symbol nobody overrode.
            if symbol.library == LibraryRef::WeakLookup {
                return Ok(());
            }
            let value: u64 = 0;
            mem.write(slot_va, &value.to_le_bytes())?;
            out.missing.push(MissingSymbol {
                symbol: symbol.name.to_string(),
                library,
                slot_va,
                stub_va,
                weak_import: symbol.weak_import,
            });
            return Ok(());
        };

        let value = r.addr.wrapping_add(addend as u64);
        mem.write(slot_va, &value.to_le_bytes())?;

        out.bindings.push(BindingRecord {
            slot_va,
            stub_va,
            library,
            symbol: symbol.name.to_string(),
            resolved: value,
            kind,
            weak_import: symbol.weak_import,
            tag: r.tag,
        });
        Ok(())
    }

    /// Where a reference is looked up, which depends on which image it names.
    fn lookup(&self, symbol: &SymbolRef<'_>) -> Option<Resolved> {
        match symbol.library {
            LibraryRef::SelfImage => lookup_export(self.self_exports?, symbol.name, self.slide),
            LibraryRef::MainExecutable => {
                let (exports, slide) = self.main_executable?;
                lookup_export(exports, symbol.name, slide)
            }
            _ => resolve_in(self.providers, symbol),
        }
    }
}

/// Resolve a name against an already-loaded image's export table.
///
/// A re-export yields `None` rather than an address: it is a forwarding edge
/// into another library, and following it needs the dependency graph, which
/// lives a layer up in [`crate::namespace`]. Answering zero here would bind the
/// caller to nothing and report success.
fn lookup_export(exports: &ExportTable, name: &str, slide: Word) -> Option<Resolved> {
    let e = exports.get(name)?;
    if matches!(e.value, ExportValue::Reexport { .. }) {
        return None;
    }
    let addr = e.address(slide)?;
    Some(Resolved {
        addr,
        kind: match e.kind {
            crate::exports::ExportKind::ThreadLocal => SymbolKind::ThreadLocal,
            _ => SymbolKind::Function,
        },
        tag: 0,
    })
}
