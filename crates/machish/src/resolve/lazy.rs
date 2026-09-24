//! A library whose symbol table is not built until something asks for one.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::fixups::SymbolRef;
use crate::resolve::{Resolved, SymbolProvider};

/// A library that is opened on the first symbol asked of it, and that answers
/// by name rather than from a table.
///
/// [`SyntheticDylib`](crate::resolve::SyntheticDylib) wants every definition up
/// front, which two kinds of library cannot give: one whose entry points are
/// discoverable only by name - an OpenGL driver publishes no list - and one that
/// must not be brought up at all in a process that never uses it. `open` runs
/// once, before the first lookup and never in a link that asks for nothing here;
/// `lookup` answers one name, and its answer is remembered.
///
/// An `open` that fails makes the library define nothing, so the chain carries
/// on to whatever comes after.
pub struct LazyDylib {
    install_name: String,
    aliases: Vec<String>,
    flat: bool,
    claims: fn(&str) -> bool,
    open: fn() -> bool,
    lookup: fn(&str) -> Option<Resolved>,
    opened: Cell<Option<bool>>,
    cache: RefCell<BTreeMap<String, Option<Resolved>>>,
}

impl LazyDylib {
    pub fn new(
        install_name: &str,
        open: fn() -> bool,
        lookup: fn(&str) -> Option<Resolved>,
    ) -> Self {
        LazyDylib {
            install_name: install_name.to_string(),
            aliases: Vec::new(),
            flat: false,
            claims: |_| true,
            open,
            lookup,
            opened: Cell::new(None),
            cache: RefCell::new(BTreeMap::new()),
        }
    }

    /// Also answer to `install_name`. See [`SyntheticDylib::alias`](crate::resolve::SyntheticDylib::alias).
    pub fn alias(&mut self, install_name: &str) -> &mut Self {
        self.aliases.push(install_name.to_string());
        self
    }

    /// Whether to answer regardless of which library an import names. See
    /// [`SyntheticDylib::flat`](crate::resolve::SyntheticDylib::flat).
    pub fn flat(&mut self, flat: bool) -> &mut Self {
        self.flat = flat;
        self
    }

    /// Only names `claims` accepts are this library's; any other is declined
    /// without opening it. A flat library needs this, or it is opened by the
    /// first import of any kind that passes through it.
    pub fn claims(&mut self, claims: fn(&str) -> bool) -> &mut Self {
        self.claims = claims;
        self
    }

    /// Whether the backing library has been opened, and whether that worked.
    /// `None` before anything has asked.
    pub fn opened(&self) -> Option<bool> {
        self.opened.get()
    }

    fn ensure_open(&self) -> bool {
        if let Some(known) = self.opened.get() {
            return known;
        }
        let ok = (self.open)();
        self.opened.set(Some(ok));
        ok
    }
}

impl SymbolProvider for LazyDylib {
    fn provides(&self, install_name: &str) -> bool {
        self.flat
            || self.install_name == install_name
            || self.aliases.iter().any(|a| a == install_name)
    }

    fn resolve(&self, sym: &SymbolRef<'_>) -> Option<Resolved> {
        if let Some(hit) = self.cache.borrow().get(sym.name) {
            return *hit;
        }
        if !(self.claims)(sym.name) || !self.ensure_open() {
            return None;
        }
        let found = (self.lookup)(sym.name);
        self.cache.borrow_mut().insert(sym.name.to_string(), found);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixups::LibraryRef;

    fn sym(name: &str) -> SymbolRef<'_> {
        SymbolRef {
            name,
            library: LibraryRef::Flat,
            weak_import: false,
        }
    }

    fn open_ok() -> bool {
        true
    }

    fn lookup(name: &str) -> Option<Resolved> {
        (name == "_present").then(|| Resolved::function(0x1234))
    }

    #[test]
    fn nothing_opens_the_library_until_a_symbol_is_asked_for() {
        let lib = LazyDylib::new("<test>", open_ok, lookup);
        assert_eq!(lib.opened(), None);
        assert!(lib.provides("<test>"));
        assert_eq!(lib.opened(), None, "matching a name is not a lookup");

        assert_eq!(lib.resolve(&sym("_present")).map(|r| r.addr), Some(0x1234));
        assert_eq!(lib.opened(), Some(true));
        assert_eq!(lib.resolve(&sym("_absent")), None);
    }

    #[test]
    fn a_library_that_will_not_open_defines_nothing() {
        fn open_fails() -> bool {
            false
        }
        let lib = LazyDylib::new("<test>", open_fails, lookup);
        assert_eq!(lib.resolve(&sym("_present")), None);
        assert_eq!(lib.opened(), Some(false));
    }

    #[test]
    fn a_name_the_library_does_not_claim_leaves_it_closed() {
        let mut lib = LazyDylib::new("<test>", open_ok, lookup);
        lib.flat(true).claims(|name| name.starts_with("_pre"));
        assert_eq!(lib.resolve(&sym("_write")), None);
        assert_eq!(lib.opened(), None);
        assert_eq!(lib.resolve(&sym("_present")).map(|r| r.addr), Some(0x1234));
        assert_eq!(lib.opened(), Some(true));
    }
}
