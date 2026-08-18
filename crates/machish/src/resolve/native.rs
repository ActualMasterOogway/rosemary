//! `dlopen`/`dlsym`-backed resolution

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::fixups::SymbolRef;
use crate::resolve::{Resolved, SymbolKind, SymbolProvider};

/// Symbols never passed through to the host, and why.
///
/// Exact names; see [`DENY_PREFIXES`] for the families.
const DENY_EXACT: &[&str] = &[
    // Host process state a guest must not observe directly.
    "_environ",
    "__NSGetEnviron",
    "_mach_task_self_",
    "_mach_host_self",
    "___stack_chk_guard",
    "___progname",
    "__NSGetArgc",
    "__NSGetArgv",
    "__NSGetProgname",
    "__NSGetExecutablePath",
    // The image list is the host's, and answering with it makes the guest
    // believe it is running as this process.
    "__dyld_get_image_header",
    "__dyld_image_count",
];

/// Symbol families never passed through.
const DENY_PREFIXES: &[&str] = &[
    // dyld introspection and registration.
    "_dyld_",
    "__dyld_",
    // The Objective-C runtime needs class realization, selector uniquing, and
    // category attachment that this crate does not do. Passing these through
    // registers *guest* class structures with the *host* runtime, which is the
    // single fastest way to a crash inside `objc_msgSend`.
    "_objc_",
    "_class_",
    "_sel_",
    "_protocol_",
    ".objc_class_name",
    // Thread-local bootstrap. Descriptors are enumerated and stop there;
    // handing the guest the host's TLV machinery would allocate host pthread
    // keys for guest variables.
    "__tlv_",
];

/// Suffixes an x86-64 macOS symbol may carry that the arm64 ABI does not.
///
/// These are the platform's own compatibility mangling, and they are exactly
/// the sort of thing high-level emulation has to notice. `/bin/ls` imports
/// `_fts_open$INODE64` because on x86-64 macOS `fts_open` came in two flavours,
/// one with a 32-bit `ino_t` and one with 64. arm64 has only ever had the
/// 64-bit one, so it exports the plain name and `dlsym("fts_open$INODE64")`
/// returns null — five of `/bin/ls`'s imports, unresolvable, over a suffix.
///
/// Stripping is a *fallback*, tried only after the exact name misses, because
/// `$NOCANCEL` variants genuinely do exist on arm64 and the exact match is
/// always the better answer when it is available.
const LEGACY_VARIANT_SUFFIXES: &[&str] = &[
    // 64-bit `ino_t` variants of the `fts`/`readdir`/`stat` family.
    "$INODE64",
    // SUSv3 conformance variants from the 10.4/10.5 era.
    "$UNIX2003",
    "$1050",
];

/// Resolution against the images already loaded in this process.
pub struct NativeDylibProvider {
    /// `None` means `RTLD_DEFAULT`: search every loaded image in load order.
    handle: Option<Handle>,
    /// Install names this provider stands for. Empty plus `flat` means "any".
    install_names: Vec<String>,
    flat: bool,
    deny_exact: BTreeSet<String>,
    deny_prefixes: Vec<String>,
    /// Whether to `dlopen` a two-level import's library when the symbol is not
    /// already loaded. See [`autoload`](Self::autoload).
    autoload: bool,
    /// Install names already passed to `dlopen`, successfully or not, so a
    /// library that cannot be opened is not retried once per import.
    attempted: RefCell<BTreeSet<String>>,
    /// Bind streams repeat names heavily — `/bin/ls` asks for `_strlen` from
    /// several slots — and `dlsym` walks the whole image list each time.
    ///
    /// Keyed by name alone, which is correct for the two configurations this
    /// type offers: [`system`](Self::system) answers flatly, and
    /// [`open`](Self::open) is scoped to a single library, so in neither case
    /// does the answer depend on which library was asked.
    cache: RefCell<BTreeMap<String, Option<Resolved>>>,
}

/// A `dlopen` handle, closed on drop.
struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `dlopen` and has not been
        // closed; `Handle` is not `Copy` or `Clone`, so this runs exactly once.
        unsafe {
            libc::dlclose(self.0);
        }
    }
}

impl NativeDylibProvider {
    /// Resolve against every image loaded in this process (`RTLD_DEFAULT`),
    /// answering for any install name.
    ///
    /// This is the configuration that makes `/usr/lib/*` imports work without a
    /// single system dylib on disk: arm64 libSystem is already mapped here, and
    /// it exports the same C names the Intel guest wants.
    pub fn system() -> Self {
        NativeDylibProvider {
            handle: None,
            install_names: Vec::new(),
            flat: true,
            deny_exact: DENY_EXACT.iter().map(|s| s.to_string()).collect(),
            deny_prefixes: DENY_PREFIXES.iter().map(|s| s.to_string()).collect(),
            autoload: true,
            attempted: RefCell::new(BTreeSet::new()),
            cache: RefCell::new(BTreeMap::new()),
        }
    }

    /// Whether an unresolved two-level import may `dlopen` the library it
    /// names. On by default.
    ///
    /// `RTLD_DEFAULT` searches images *already loaded*, and the host process is
    /// not the guest: nothing has loaded `libncurses` or `libutil` just because
    /// the guest links against them. Six of `/bin/ls`'s imports are
    /// unresolvable without this, and each would look like a missing shim
    /// rather than an unopened library.
    ///
    /// The library is opened `RTLD_GLOBAL` and deliberately **never closed**.
    /// That is not a leak so much as a requirement: the loader has written host
    /// pointers into the guest's slots, and unloading the image those point
    /// into would turn every one of them into a dangling call target.
    pub fn autoload(&mut self, autoload: bool) -> &mut Self {
        self.autoload = autoload;
        self
    }

    /// `dlopen` a specific library and answer only for `install_name`.
    ///
    /// Returns `None` if the library cannot be opened, which on modern macOS is
    /// the expected answer for most of `/usr/lib` — those paths resolve inside
    /// the shared cache, and a `dlopen` of the *guest's* copy would be the
    /// wrong architecture anyway.
    pub fn open(path: &str, install_name: &str) -> Option<Self> {
        let c = std::ffi::CString::new(path).ok()?;
        // SAFETY: `c` is a valid NUL-terminated C string that outlives the
        // call. `dlopen` returns null on failure, which is checked.
        let raw = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if raw.is_null() {
            return None;
        }
        let mut p = Self::system();
        p.handle = Some(Handle(raw));
        p.flat = false;
        p.install_names.push(install_name.to_string());
        Some(p)
    }

    /// Also answer for `install_name`.
    pub fn alias(&mut self, install_name: &str) -> &mut Self {
        self.install_names.push(install_name.to_string());
        self
    }

    /// Refuse `symbol`, so an earlier provider must supply it.
    pub fn deny(&mut self, symbol: &str) -> &mut Self {
        self.deny_exact.insert(symbol.to_string());
        self
    }

    /// Refuse every symbol starting with `prefix`.
    pub fn deny_prefix(&mut self, prefix: &str) -> &mut Self {
        self.deny_prefixes.push(prefix.to_string());
        self
    }

    /// Drop the default denylist.
    ///
    /// Only correct when an earlier provider in the chain already claims every
    /// symbol that needs virtualizing — otherwise this is how a guest ends up
    /// reading the host's environment. Provided because "I have a complete shim
    /// table and want the default out of my way" is a legitimate position, and
    /// hiding it would just get the field edited.
    pub fn allow_everything(&mut self) -> &mut Self {
        self.deny_exact.clear();
        self.deny_prefixes.clear();
        self
    }

    /// Whether `name` is refused by policy rather than merely absent.
    pub fn is_denied(&self, name: &str) -> bool {
        self.deny_exact.contains(name)
            || self
                .deny_prefixes
                .iter()
                .any(|p| name.starts_with(p.as_str()))
    }

    /// Try `dlsym` for `name`, then again with a legacy x86-only variant
    /// suffix stripped. See [`LEGACY_VARIANT_SUFFIXES`].
    fn lookup(&self, name: &str) -> Option<Resolved> {
        if let Some(found) = self.dlsym(name) {
            return Some(found);
        }
        for suffix in LEGACY_VARIANT_SUFFIXES {
            if let Some(base) = name.strip_suffix(suffix)
                && let Some(found) = self.dlsym(base)
            {
                return Some(found);
            }
        }
        None
    }

    /// `dlopen` `install_name` into the global namespace, once.
    ///
    /// Returns whether this call is the one that attempted it, so the caller
    /// knows whether retrying the `dlsym` could possibly help.
    fn autoload_library(&self, install_name: &str) -> bool {
        if !self.autoload || !self.attempted.borrow_mut().insert(install_name.to_string()) {
            return false;
        }
        let Ok(c) = std::ffi::CString::new(install_name) else {
            return false;
        };
        // SAFETY: `c` is a valid NUL-terminated C string outliving the call.
        // The handle is intentionally never closed — see `autoload` — so there
        // is nothing to own and nothing to free.
        unsafe {
            libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
        }
        true
    }

    /// `dlsym` for `name`, with the leading underscore stripped.
    fn dlsym(&self, name: &str) -> Option<Resolved> {
        // A name that is *only* an underscore, or does not start with one, is
        // not something a C-ABI `dlsym` can answer.
        let bare = name.strip_prefix('_')?;
        if bare.is_empty() {
            return None;
        }
        let c = std::ffi::CString::new(bare).ok()?;

        // `RTLD_DEFAULT` is `(void *)-2` on macOS: search all loaded images.
        let handle = self
            .handle
            .as_ref()
            .map_or(-2isize as *mut core::ffi::c_void, |h| h.0);

        // SAFETY: `handle` is either a live `dlopen` result owned by `self` or
        // the `RTLD_DEFAULT` sentinel, and `c` is a valid NUL-terminated string
        // that outlives the call. `dlsym` returns null when the symbol is
        // absent, which is checked rather than dereferenced.
        let ptr = unsafe { libc::dlsym(handle, c.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        // `dlsym` cannot say whether an address is code or data, and guessing
        // would be worse than declining to: `Function` is what a bind stream
        // overwhelmingly means, and an embedder that needs the distinction has
        // `dladdr` and the tag.
        Some(Resolved {
            addr: ptr as usize as u64,
            kind: SymbolKind::Function,
            tag: 0,
        })
    }
}

impl SymbolProvider for NativeDylibProvider {
    fn provides(&self, install_name: &str) -> bool {
        self.flat || self.install_names.iter().any(|n| n == install_name)
    }

    fn resolve(&self, sym: &SymbolRef<'_>) -> Option<Resolved> {
        if self.is_denied(sym.name) {
            return None;
        }
        if let Some(hit) = self.cache.borrow().get(sym.name) {
            return *hit;
        }

        let mut found = self.lookup(sym.name);
        // Only now, on a genuine miss, consider that the library may simply not
        // be loaded yet. Doing it eagerly would `dlopen` every dependency of
        // every image for symbols that were going to resolve anyway.
        if found.is_none()
            && let Some(install_name) = sym.library.install_name()
            && self.autoload_library(install_name)
        {
            found = self.lookup(sym.name);
        }

        self.cache.borrow_mut().insert(sym.name.to_owned(), found);
        found
    }

    // `enumerate` is deliberately not implemented. `dlsym` answers questions
    // but cannot be asked to list, and a plausible-looking implementation that
    // returned nothing would be indistinguishable from a broken one.
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

    #[test]
    fn resolves_a_libsystem_symbol_to_a_host_pointer() {
        let p = NativeDylibProvider::system();
        let r = p
            .resolve(&sym("_strlen"))
            .expect("_strlen must resolve via dlsym");
        assert_ne!(r.addr, 0);
        // Sanity: the same query twice must agree, which also exercises the
        // cache path rather than only the `dlsym` one.
        assert_eq!(p.resolve(&sym("_strlen")), Some(r));
    }

    #[test]
    fn denied_symbols_are_refused_even_though_the_host_has_them() {
        let p = NativeDylibProvider::system();
        // The host genuinely exports `_environ`; refusing it is policy.
        assert!(p.is_denied("_environ"));
        assert_eq!(p.resolve(&sym("_environ")), None);
        assert_eq!(p.resolve(&sym("_objc_msgSend")), None);
        assert_eq!(p.resolve(&sym("__tlv_bootstrap")), None);
    }

    #[test]
    fn allow_everything_reopens_the_denied_set() {
        let mut p = NativeDylibProvider::system();
        p.allow_everything();
        assert!(p.resolve(&sym("_environ")).is_some());
    }

    #[test]
    fn a_name_without_an_underscore_is_not_guessed_at() {
        let p = NativeDylibProvider::system();
        assert_eq!(p.resolve(&sym("strlen")), None);
        assert_eq!(p.resolve(&sym("_")), None);
    }

    #[test]
    fn a_legacy_x86_variant_suffix_falls_back_to_the_plain_name() {
        // `/bin/ls` really does import this name, and arm64 really does not
        // export it — the whole point of the fallback.
        let p = NativeDylibProvider::system();
        let plain = p
            .resolve(&sym("_fts_open"))
            .expect("_fts_open must resolve");
        let variant = p
            .resolve(&sym("_fts_open$INODE64"))
            .expect("the $INODE64 variant must fall back");
        assert_eq!(
            variant, plain,
            "the fallback must land on the same implementation"
        );
    }

    #[test]
    fn an_exact_match_beats_the_suffix_fallback() {
        // `$NOCANCEL` variants do exist on arm64, so stripping must never
        // happen when the full name resolves.
        let p = NativeDylibProvider::system();
        let Some(exact) = p.dlsym("_read$NOCANCEL") else {
            // Not present on every OS version; the ordering claim is still
            // covered by the unconditional fallback ordering in `lookup`.
            return;
        };
        assert_eq!(p.resolve(&sym("_read$NOCANCEL")), Some(exact));
        assert_ne!(p.resolve(&sym("_read")), Some(exact));
    }

    #[test]
    fn autoload_opens_a_library_the_host_has_not_loaded() {
        // Nothing in this test process links `libncurses`, so `RTLD_DEFAULT`
        // cannot see `_tputs` until the library named by the import is opened.
        let p = NativeDylibProvider::system();
        assert_eq!(
            p.dlsym("_tputs"),
            None,
            "libncurses should not be loaded yet"
        );

        let two_level = SymbolRef {
            name: "_tputs",
            library: LibraryRef::InstallName("/usr/lib/libncurses.5.4.dylib"),
            weak_import: false,
        };
        assert!(
            p.resolve(&two_level).is_some(),
            "autoload should have opened libncurses"
        );
    }

    #[test]
    fn autoload_can_be_turned_off() {
        let mut p = NativeDylibProvider::system();
        p.autoload(false);
        let two_level = SymbolRef {
            name: "_machish_not_a_symbol",
            library: LibraryRef::InstallName("/usr/lib/libmachish_not_a_library.dylib"),
            weak_import: false,
        };
        assert_eq!(p.resolve(&two_level), None);
        assert!(
            p.attempted.borrow().is_empty(),
            "autoload(false) must not dlopen anything"
        );
    }

    #[test]
    fn absent_symbols_resolve_to_nothing() {
        let p = NativeDylibProvider::system();
        assert_eq!(
            p.resolve(&sym("_machish_definitely_not_a_real_symbol")),
            None
        );
    }
}
