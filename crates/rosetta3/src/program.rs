//! A loaded guest program, ready to be lifted.

use std::collections::BTreeMap;
use std::fmt;

use machish::loader::Loader;
use machish::memory::{BufferSpace, Regions};
use machish::namespace::FsDylibSource;
use machish::report::LinkReport;
use machish::resolve::{NativeDylibProvider, SymbolProvider, eager_binding_shims};
use machish::spec::arch::X86_64;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::LifterConfig;
use microlift::core::exit::MemorySegment;

use crate::abi::{AbiError, Classification, FunctionAbi, Lookup};

/// Whether an import with a known signature is inlined by the lifter or left to
/// a VM exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImportPolicy {
    /// Inline every call whose signature is known and marshallable; everything
    /// else — variadics, refusals, unknowns — stays a slow exit.
    #[default]
    FastWhenKnown,
    /// Every import is a slow exit. Correct, and the baseline the fast path is
    /// measured against.
    AlwaysSlow,
}

impl ImportPolicy {
    /// The signature to inline this site's call with, if any.
    fn promotion_for(self, site: &ImportSite) -> Option<&'static FunctionAbi> {
        if self == ImportPolicy::AlwaysSlow || site.resolved == 0 {
            return None;
        }
        // A shimmed symbol must never be promoted: the whole reason it has a
        // shim is that calling the host function directly would be wrong.
        if crate::shims::is_shimmed(&site.symbol) {
            return None;
        }
        if crate::abi::classify(site.tag, &site.symbol) != Classification::Fast {
            return None;
        }
        match crate::abi::table::lookup(site.tag, &site.symbol) {
            Lookup::Known(abi) => Some(abi),
            _ => None,
        }
    }
}

/// Anything that can go wrong loading a program.
#[derive(Debug)]
pub enum ProgramError {
    Io(std::io::Error),
    Link(machish::Error),
    /// The image has no `__TEXT,__text`, so there is nothing to lift.
    NoText,
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProgramError::Io(e) => write!(f, "{e}"),
            ProgramError::Link(e) => write!(f, "{e}"),
            ProgramError::NoText => write!(f, "image has no __TEXT,__text section"),
        }
    }
}

impl std::error::Error for ProgramError {}

impl From<std::io::Error> for ProgramError {
    fn from(e: std::io::Error) -> Self {
        ProgramError::Io(e)
    }
}

impl From<machish::Error> for ProgramError {
    fn from(e: machish::Error) -> Self {
        ProgramError::Link(e)
    }
}

/// An Intel Mach-O loaded, linked against the host, and ready for the lifter.
///
/// ## Which address space, and why it is a parameter
///
/// A [`BufferSpace`]-backed program is a plain allocation at a chosen base: the
/// right thing for inspecting and auditing an image, and *insufficient for
/// execution*, because the host pointers written into the guest's import slots
/// only mean anything if the guest's virtual addresses are this process's
/// virtual addresses. Running the guest needs an identity mapping —
/// [`MmapSpace`] — which is why the space is a type parameter rather than a
/// fixed choice: [`Program::load`] gives the inspectable form and
/// [`Program::load_executable`] the runnable one, and everything between them
/// is the same code.
///
/// [`MmapSpace`]: machish::memory::MmapSpace
pub struct Program<M: Regions = BufferSpace> {
    /// The full linking transcript.
    pub report: LinkReport,
    /// The guest's mapped memory.
    pub memory: M,
    /// `(virtual address, bytes)` of the root image's `__TEXT,__text`.
    pub text: (u64, Vec<u8>),
    /// Slid extents of each function in `__text`, from `LC_FUNCTION_STARTS`.
    ///
    /// The unit a JIT lifts at a time. Not a translation plan: nothing here is
    /// lifted until control is about to reach it. See [`Program::lift_region`].
    pub functions: Vec<std::ops::Range<u64>>,
    /// Every address a call site might name, mapped to the host address behind
    /// it: both `slot_va` and `stub_va` for each binding.
    pub imports: BTreeMap<u64, ImportSite>,
    /// How to leave each function of the root image, from `__unwind_info`.
    ///
    /// Read at load for the same reason [`functions`](Program::functions) is:
    /// it is a table in the image, and the alternative is re-parsing the Mach-O
    /// at the moment an exception is being thrown. Empty for an image without
    /// the section, which is a C program that never unwinds.
    pub unwind: machish::image::UnwindInfo,
    /// The same sites, keyed by the address they *resolved to*.
    ///
    /// The other direction, and it answers a different question. [`imports`]
    /// answers "the guest called this address; what is it" for an address the
    /// lifter recognised at a call site. This answers "the guest loaded a
    /// pointer, and here is its value" — the case where nothing was recognisable
    /// at lift time because the call is `callq *(%rdi)`. Reading a
    /// `_Thread_local` is compiled exactly that way, which is what makes this
    /// load-bearing rather than a convenience.
    ///
    /// Deduplicated by symbol name so the entry for an address two symbols
    /// resolve to (`bzero` and `__bzero` are one function) is the same on every
    /// run, rather than whichever binding the decoder happened to reach first.
    ///
    /// [`imports`]: Program::imports
    imports_by_target: BTreeMap<u64, ImportSite>,
}

/// One address microlift should treat as a call out of the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportSite {
    pub symbol: String,
    pub library: Option<String>,
    /// The host address the guest call should end up at.
    pub resolved: u64,
    /// Provider-defined; an index into the embedder's shim table. See the crate
    /// note on why the seam is a `u64` and not a typed ABI.
    pub tag: u64,
    /// Whether this address is the `__stubs` entry rather than the pointer slot.
    pub is_stub: bool,
}

impl Program<BufferSpace> {
    /// Load and link `path` against the host, with no guest system libraries.
    ///
    /// `extra` is consulted *before* the native provider, which is how a symbol
    /// that must be virtualized rather than passed through gets intercepted —
    /// see [`machish::resolve`] for why ordering is the mechanism.
    ///
    /// The result is *inspectable*, not runnable: see
    /// [`Program::load_executable`].
    pub fn load(path: &str, extra: &[&dyn SymbolProvider]) -> Result<Self, ProgramError> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes, path, extra)
    }

    /// Load and link an image already in memory.
    pub fn from_bytes(
        bytes: &[u8],
        path: &str,
        extra: &[&dyn SymbolProvider],
    ) -> Result<Self, ProgramError> {
        Self::load_into(bytes, path, extra, BufferSpace::new(0x1_0000_0000))
    }
}

impl Program<machish::memory::MmapSpace> {
    /// Load and link `path` into **host memory at the guest's own addresses**.
    ///
    /// The identity mapping is what makes execution possible at all: a host
    /// function pointer bound into a guest import slot is directly callable,
    /// and a guest pointer handed to a host function is directly
    /// dereferenceable. Its price is that a pointer no longer says which side
    /// it came from — see [`Program::is_guest_address`], the question every
    /// indirect call has to answer.
    ///
    /// The mapping is never executable. The guest is x86-64; this CPU cannot
    /// run a byte of it under any protection, and the recompiler reads it as
    /// data.
    pub fn load_executable(
        path: &str,
        extra: &[&dyn SymbolProvider],
    ) -> Result<Self, ProgramError> {
        let bytes = std::fs::read(path)?;
        Self::load_into(
            bytes.as_slice(),
            path,
            extra,
            machish::memory::MmapSpace::new(),
        )
    }
}

impl<M: Regions> Program<M> {
    /// Load and link into a caller-supplied address space.
    pub fn load_into(
        bytes: &[u8],
        path: &str,
        extra: &[&dyn SymbolProvider],
        mut memory: M,
    ) -> Result<Self, ProgramError> {
        let mut source = FsDylibSource::new();

        let eager = eager_binding_shims();
        let native = NativeDylibProvider::system();
        // Ahead of the host: `___stack_chk_guard` is a symbol the host declines
        // to export and every guest with an array on its stack dereferences in
        // its own prologue. See `crate::shims::runtime`.
        let runtime = crate::shims::runtime::provider();
        // And the Objective-C runtime, for the same reason one step further out:
        // the native provider denies the `_objc_`, `_class_`, `_sel_` and
        // `_protocol_` families precisely so a shim can claim them, and passing
        // one through unexamined would hand the host runtime a guest class. See
        // [`crate::objc::dispatch`].
        let objc = crate::objc::dispatch::provider();
        let mut providers: Vec<&dyn SymbolProvider> = extra.to_vec();
        providers.push(&runtime);
        providers.push(&objc);
        providers.push(&eager);
        providers.push(&native);

        let report =
            Loader::new(X86_64).link_with(bytes, path, &mut memory, &mut source, &providers)?;

        // The root image's `__text` is what the driver lifts. Re-parsing here
        // rather than threading a `MachObject` out of the loader keeps the
        // report the only thing that crosses the boundary.
        let obj = machish::image::MachObject::parse_any(bytes, X86_64)?;
        let text_sect = obj
            .section("__TEXT", "__text")
            .ok_or(ProgramError::NoText)?;
        let slide = report.root().map_or(0, |i| i.slide);
        let text_va = text_sect.addr.wrapping_add(slide);

        let mut text = vec![0u8; text_sect.size as usize];
        memory.read(text_va, &mut text)?;

        let functions: Vec<std::ops::Range<u64>> =
            machish::image::function_ranges(&obj, text_sect.addr, text_sect.addr + text_sect.size)?
                .into_iter()
                .map(|r| r.start.wrapping_add(slide)..r.end.wrapping_add(slide))
                .collect();

        let unwind = machish::image::UnwindInfo::parse(&obj, slide)?;

        let mut imports = BTreeMap::new();
        for b in report.bindings() {
            let site = ImportSite {
                symbol: b.symbol.clone(),
                library: b.library.clone(),
                resolved: b.resolved,
                tag: b.tag,
                is_stub: false,
            };
            imports.insert(b.slot_va, site.clone());
            if let Some(stub) = b.stub_va {
                imports.insert(
                    stub,
                    ImportSite {
                        is_stub: true,
                        ..site
                    },
                );
            }
        }

        // Imports **nothing satisfied** are registered too, resolved to zero.
        //
        // Not for completeness: for the diagnostic. An unregistered call site is
        // followed as guest code, so a call to an unsatisfied import decodes the
        // `__stubs` trampoline, follows its `jmp qword [rip+disp]` to the slot
        // the binder zeroed, and stops with "no guest code at 0x0" — a report
        // about an address, for a failure whose whole content is a *name*.
        // Registered, the same call is `AbiError::Unsatisfied` naming the symbol,
        // which is a work item. See [`crate::abi::call_import_with`], which
        // refuses a zero host address before anything else.
        for image in &report.images {
            for m in &image.missing {
                let site = ImportSite {
                    symbol: m.symbol.clone(),
                    library: m.library.clone(),
                    resolved: 0,
                    tag: 0,
                    is_stub: false,
                };
                imports.insert(m.slot_va, site.clone());
                if let Some(stub) = m.stub_va {
                    imports.insert(
                        stub,
                        ImportSite {
                            is_stub: true,
                            ..site
                        },
                    );
                }
            }
        }

        let mut by_symbol: Vec<&ImportSite> = imports.values().filter(|s| !s.is_stub).collect();
        by_symbol.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        let mut imports_by_target: BTreeMap<u64, ImportSite> = BTreeMap::new();
        for site in by_symbol {
            if site.resolved != 0 {
                imports_by_target
                    .entry(site.resolved)
                    .or_insert_with(|| site.clone());
            }
        }

        Ok(Program {
            report,
            memory,
            text: (text_va, text),
            functions,
            unwind,
            imports,
            imports_by_target,
        })
    }

    /// The shared [`LifterConfig`] every on-demand lift is performed with.
    ///
    /// Two things go in, and neither is a translation plan.
    ///
    /// **Import addresses**, so a call to one is recognised as leaving the guest
    /// rather than being followed into a `__stubs` entry — see the crate note
    /// for what happens otherwise. They are registered **slow**: each call
    /// produces an `ExitReason::ImportedFunctionCall` exit the runtime services
    /// by calling the host function itself. Promoting one to a fast inline call
    /// needs its calling convention, whose only honest source is the embedder's
    /// shim table, reached through [`ImportSite::tag`]. A slow import is
    /// *correct*; a fast one is merely faster. That is M2.
    ///
    /// **The guest's mapped images**, as a hint for microlift's value-set
    /// analysis: it lets a RIP-relative load from read-only data be folded to a
    /// constant at lift time instead of becoming a runtime memory access.
    /// Marked non-writable for exactly that reason — the lifter must not fold a
    /// value the guest can later store over.
    pub fn lifter_config(&self) -> LifterConfig {
        self.lifter_config_with(ImportPolicy::FastWhenKnown)
    }

    /// [`Program::lifter_config`], with the promotion decision made explicitly.
    ///
    /// [`ImportPolicy::AlwaysSlow`] exists because it is the falsifiable half
    /// of the M2 claim: fast is an *optimisation* of slow, so lifting a program
    /// both ways must differ in the number of VM exits and in nothing else. A
    /// test that can only run the fast path cannot check that.
    pub fn lifter_config_with(&self, policy: ImportPolicy) -> LifterConfig {
        let mut config = LifterConfig::default();

        for (base, bytes) in self.memory.regions() {
            config
                .segments
                .push(MemorySegment::new(base, bytes.to_vec(), false));
        }
        for (&va, site) in &self.imports {
            match policy.promotion_for(site) {
                Some(abi) => config.register_fast_import(
                    va,
                    crate::abi::dynamic_symbol(abi, site.resolved),
                    crate::abi::guest_call_abi(),
                ),
                None => config.register_slow_import(va),
            }
        }
        config
    }

    /// How every import of this program will be reached, by symbol.
    ///
    /// The M2 acceptance question in one call: an entry classified
    /// [`Classification::Unknown`] is a symbol nobody has described, which is
    /// the one outcome that becomes a wrong value in a register at run time
    /// rather than a diagnostic.
    pub fn classify_imports(&self) -> BTreeMap<&str, Classification> {
        let mut out = BTreeMap::new();
        for b in self.report.bindings() {
            out.insert(b.symbol.as_str(), crate::abi::classify(b.tag, &b.symbol));
        }
        out
    }

    /// Service an `ExitReason::ImportedFunctionCall`.
    ///
    /// `ctx.imported_function_va` names the address the guest called — a slot
    /// or a stub, both of which are registered — and the guest resumes at
    /// `ctx.rip`, which microlift already set to the instruction after the
    /// `CALL`. Nothing here touches `rip` or `rsp`: the call was intercepted
    /// *before* it executed, so there is no return address to pop.
    pub fn service_import_call(&mut self, ctx: &mut RegisterContext) -> Result<(), AbiError> {
        let va = ctx.imported_function_va;
        let Some(site) = self.imports.get(&va) else {
            return Err(AbiError::NoSignature {
                symbol: format!("<unregistered {va:#x}>"),
            });
        };
        let (symbol, tag, resolved) = (site.symbol.clone(), site.tag, site.resolved);
        crate::abi::call_import(&symbol, tag, resolved, ctx, &mut self.memory)
    }

    /// The region a JIT should lift in order to execute `va`: where it starts,
    /// and the bytes to hand microlift.
    ///
    /// `LC_FUNCTION_STARTS` supplies a *hint* — the enclosing function's start,
    /// so that every call site reaching one function shares one translation —
    /// and nothing more. The extent is computed by walking the guest's control
    /// flow from that start; see [`crate::region`] for why a function's recorded
    /// extent cannot be handed to the lifter as-is, and what happens to the code
    /// on the far side of an embedded jump table.
    ///
    /// Any mapped image answers, not only the root's `__text`: a bundled dylib,
    /// an image whose function starts were stripped, and code the guest wrote at
    /// run time all reach this the same way — the first by hint, the other two
    /// with none.
    ///
    /// `None` when `va` is not in mapped guest memory, or when nothing there
    /// decodes.
    pub fn lift_region(&self, va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
        let (base, bytes) = self.memory.region_at(va)?;
        let function = self.functions.iter().find(|f| f.contains(&va)).cloned();
        let region = crate::region::region_for(bytes, base, va, function)?;
        let start = usize::try_from(region.range.start - base).ok()?;
        let end = usize::try_from(region.range.end - base)
            .ok()?
            .min(bytes.len());
        let code = bytes.get(start..end)?.to_vec();
        Some((region, code))
    }

    /// Whether `va` lies inside a mapped guest image.
    ///
    /// The question every `ExitReason::IndirectCall` has to answer. Guest and
    /// host share one address space by construction, so a function pointer
    /// carries no tag saying which it is: a guest address must be lifted and
    /// entered, a host address must be called natively as arm64. Getting this
    /// backwards in either direction is a crash with no useful diagnostic.
    pub fn is_guest_address(&self, va: u64) -> bool {
        self.memory.region_at(va).is_some()
    }

    /// The extent of `__text`, which is what counts as "inside the guest" when
    /// classifying a call target.
    pub fn text_range(&self) -> std::ops::Range<u64> {
        let (va, bytes) = &self.text;
        *va..va.wrapping_add(bytes.len() as u64)
    }

    /// The root image's entry point, slid.
    pub fn entry(&self) -> Option<u64> {
        self.report.root().and_then(|i| i.entry)
    }

    /// How to leave the function containing `va`.
    ///
    /// The unwinder's only question about the image, and it is a lookup rather
    /// than a search because `__unwind_info` is sorted by address — see
    /// [`crate::unwind`] for what is done with the answer.
    pub fn unwind_entry(&self, va: u64) -> Option<machish::image::UnwindEntry> {
        self.unwind.lookup(va).copied()
    }

    /// The import registered at `va`, if any.
    ///
    /// `va` is whatever an exit named: a pointer slot or a `__stubs` entry,
    /// both of which are registered, which is why this is a lookup rather than
    /// a search through the bindings.
    pub fn import_at(&self, va: u64) -> Option<&ImportSite> {
        self.imports.get(&va)
    }

    /// What an imported symbol resolved to, by name.
    ///
    /// The reverse of the usual question, and it has one caller: the unwinder
    /// needs the addresses of libc++abi's `type_info` **vtables** in order to
    /// tell a class with no bases from one with a single base. Those are data
    /// imports the guest binds like any other, so the answer is already in the
    /// transcript — no host header, no `dlsym`, and nothing assumed about
    /// libc++'s internals beyond the layout its ABI documents.
    pub fn resolved_symbol(&self, symbol: &str) -> Option<u64> {
        self.imports
            .values()
            .find(|s| !s.is_stub && s.symbol == symbol && s.resolved != 0)
            .map(|s| s.resolved)
    }

    /// The import that *resolved to* `host`, if any.
    ///
    /// What an indirect call through a loaded function pointer needs: see
    /// [`Program::imports_by_target`](Program#structfield.imports_by_target).
    pub fn import_by_target(&self, host: u64) -> Option<&ImportSite> {
        self.imports_by_target.get(&host)
    }

    /// Every import address, in ascending order.
    pub fn import_addresses(&self) -> impl Iterator<Item = u64> + '_ {
        self.imports.keys().copied()
    }
}
