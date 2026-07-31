//! Thread-local variables: `S_THREAD_LOCAL_VARIABLES` descriptors.
//!
//! A `__thread` variable in a Mach-O compiles to a three-word `tlv_descriptor`
//! in `__thread_vars`:
//!
//! ```text
//! struct tlv_descriptor {
//!     void *(*thunk)(struct tlv_descriptor *);  // an import: __tlv_bootstrap
//!     unsigned long key;                        // pthread key, 0 until set up
//!     unsigned long offset;                     // offset in the thread block
//! };
//! ```
//!
//! Reading the variable compiles to loading `thunk` and calling it with the
//! descriptor's own address. dyld binds `thunk` to `__tlv_bootstrap`, which on
//! first use allocates the thread's block, replaces `thunk` with a fast path,
//! and returns the address.
//!
//! ## Scope: enumerate and stop
//!
//! This crate parses the descriptors and does nothing else — no per-thread
//! blocks, no `pthread_key_create`, no `__tlv_bootstrap` implementation. That is
//! exactly the posture `selfish` takes with `PT_TLS`, which it parses into a
//! `TlsImage` and stops (`selfish/src/extensions/tls.rs:16-40`), and for the
//! same reason: making it work requires deciding how *guest* threads map onto
//! *host* threads, and that is a question about the execution model rather than
//! about the file format. It is milestone 3.
//!
//! Meanwhile [`TlvExtension`] does the one thing the loader can usefully do
//! now: rewrite the bootstrap imports so an embedder's shim can claim them
//! without having to know which library each image imported them from.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::error::Result;
use crate::extensions::{read_words, Extension};
use crate::fixups::{Fixup, LibraryRef};
use crate::image::MachObject;
use crate::memory::AddressSpace;
use crate::raw::Word;
use crate::report::{ImageReport, ThreadLocalDescriptor};
use crate::spec::MachSpec;

/// Words per `tlv_descriptor`.
const DESCRIPTOR_WORDS: u64 = 3;
/// Bytes per `tlv_descriptor`.
const DESCRIPTOR_SIZE: u64 = DESCRIPTOR_WORDS * 8;

/// The bootstrap thunks a `__thread` access goes through. Both spellings are
/// emitted, depending on toolchain vintage.
pub const BOOTSTRAP_SYMBOLS: &[&str] = &["__tlv_bootstrap", "_tlv_bootstrap"];

/// Enumerates thread-local descriptors and makes their bootstrap imports
/// claimable by a flat shim.
#[derive(Clone, Debug)]
pub struct TlvExtension {
    /// Symbols whose bind is rewritten to a flat lookup.
    redirect: Vec<String>,
    /// Rename each redirected import to this, if set.
    rename_to: Option<String>,
}

impl Default for TlvExtension {
    fn default() -> Self {
        Self::new()
    }
}

impl TlvExtension {
    /// Redirect the standard bootstrap symbols.
    pub fn new() -> Self {
        TlvExtension {
            redirect: BOOTSTRAP_SYMBOLS.iter().map(|s| s.to_string()).collect(),
            rename_to: None,
        }
    }

    /// Enumerate descriptors but leave the fixup stream alone.
    pub fn observe_only() -> Self {
        TlvExtension { redirect: Vec::new(), rename_to: None }
    }

    /// Also redirect `symbol`.
    pub fn redirect(&mut self, symbol: &str) -> &mut Self {
        self.redirect.push(symbol.to_string());
        self
    }

    /// Rewrite every redirected import to ask for `name` instead.
    ///
    /// Renaming, rather than substituting an address, is what keeps this a
    /// *transformation* of the stream: the loader still resolves the result
    /// through the normal provider chain, so an embedder's shim table remains
    /// the single place addresses come from.
    pub fn rename_to(&mut self, name: &str) -> &mut Self {
        self.rename_to = Some(name.to_string());
        self
    }

    fn wants(&self, name: &str) -> bool {
        self.redirect.iter().any(|s| s == name)
    }
}

impl<S: MachSpec> Extension<S> for TlvExtension {
    fn name(&self) -> &'static str {
        "tlv"
    }

    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        obj.sections.iter().any(|s| s.is_thread_local())
    }

    fn transform_fixups<'a>(
        &self,
        _obj: &MachObject<'a, S>,
        fixups: &mut Vec<Fixup<'a>>,
    ) -> Result<()> {
        if self.redirect.is_empty() {
            return Ok(());
        }
        for f in fixups.iter_mut() {
            let Fixup::Bind { symbol, .. } = f else { continue };
            if !self.wants(symbol.name) {
                continue;
            }
            // A flat reference is consulted against every provider rather than
            // only the one whose install name matches, which is what lets one
            // shim serve every image without knowing where each of them thinks
            // `__tlv_bootstrap` lives.
            symbol.library = LibraryRef::Flat;
            // The rename cannot borrow from the image — the new name is not in
            // the file — so it is only applied when the caller supplied a
            // `'static` one via `rename_to`.
            if let Some(new) = &self.rename_to {
                symbol.name = leak_static(new);
            }
        }
        Ok(())
    }

    fn observe(
        &self,
        obj: &MachObject<'_, S>,
        mem: &dyn AddressSpace,
        slide: Word,
        report: &mut ImageReport,
    ) -> Result<()> {
        report.thread_locals = collect_thread_locals(obj, mem, slide)?;
        Ok(())
    }
}

/// Interned rename targets.
///
/// `SymbolRef::name` borrows the *image*, and a rename introduces a name that is
/// not in the image, so it needs to live at least as long as the fixup stream.
/// Interning is the honest way to get that: renames come from configuration and
/// are a handful of distinct strings for the process's whole life, so the set
/// never grows with input. The alternative — an owned `String` in `SymbolRef` —
/// would allocate on every one of an image's tens of thousands of binds to
/// serve a feature almost nobody uses.
fn leak_static(name: &str) -> &'static str {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicPtr, Ordering};

    // A tiny append-only list, because `no_std` has no `OnceLock` and this is
    // touched a few times per process.
    struct Node {
        name: &'static str,
        next: *mut Node,
    }
    static HEAD: AtomicPtr<Node> = AtomicPtr::new(core::ptr::null_mut());

    let mut cur = HEAD.load(Ordering::Acquire);
    while !cur.is_null() {
        // SAFETY: nodes are heap-allocated once, published with `Release`, and
        // never freed or mutated after publication, so the pointer is valid.
        let node = unsafe { &*cur };
        if node.name == name {
            return node.name;
        }
        cur = node.next;
    }

    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    let node = Box::into_raw(Box::new(Node { name: leaked, next: HEAD.load(Ordering::Acquire) }));
    // A lost race duplicates one small allocation and is otherwise harmless:
    // both copies are valid `'static` strs with equal contents.
    loop {
        // SAFETY: `node` is a live, uniquely-owned allocation being published.
        let head = HEAD.load(Ordering::Acquire);
        unsafe { (*node).next = head };
        if HEAD
            .compare_exchange(head, node, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return leaked;
        }
    }
}

/// Every `tlv_descriptor` in the image, read as it stands in memory.
pub fn collect_thread_locals<S: MachSpec>(
    obj: &MachObject<'_, S>,
    mem: &dyn AddressSpace,
    slide: Word,
) -> Result<Vec<ThreadLocalDescriptor>> {
    let mut out = Vec::new();
    for sect in obj.sections.iter().filter(|s| s.is_thread_local_variables()) {
        let count = usize::try_from(sect.entry_count(DESCRIPTOR_SIZE)).unwrap_or(0);
        let base = sect.addr.wrapping_add(slide);
        let words = read_words(mem, base, count * DESCRIPTOR_WORDS as usize)?;
        for (i, chunk) in words.chunks_exact(DESCRIPTOR_WORDS as usize).enumerate() {
            out.push(ThreadLocalDescriptor {
                at: base.wrapping_add(i as u64 * DESCRIPTOR_SIZE),
                thunk: chunk[0],
                key: chunk[1],
                offset: chunk[2],
            });
        }
    }
    Ok(out)
}
