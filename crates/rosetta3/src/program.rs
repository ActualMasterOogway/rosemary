//! A loaded guest program, ready to be lifted.

use std::collections::BTreeMap;
use std::fmt;

use machish::loader::Loader;
use machish::memory::{AddressSpace, BufferSpace};
use machish::namespace::FsDylibSource;
use machish::report::LinkReport;
use machish::resolve::{eager_binding_shims, NativeDylibProvider, SymbolProvider};
use machish::spec::arch::X86_64;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::exit::MemorySegment;
use microlift::core::LifterConfig;

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
/// ## Backed by a buffer, which is why it cannot yet execute
///
/// The guest is mapped into a [`BufferSpace`] — a plain allocation at a chosen
/// base, not memory at the guest's own addresses. That is right for inspecting
/// and auditing an image, and it is *insufficient for execution*: the host
/// pointers written into the guest's import slots only mean anything if the
/// guest's virtual addresses are this process's virtual addresses. Running the
/// guest requires an identity mapping, i.e. [`MmapSpace`], and threading the
/// address space out as a parameter. That is M3 work.
///
/// [`MmapSpace`]: machish::memory::MmapSpace
pub struct Program {
    /// The full linking transcript.
    pub report: LinkReport,
    /// The guest's mapped memory.
    pub memory: BufferSpace,
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

impl Program {
    /// Load and link `path` against the host, with no guest system libraries.
    ///
    /// `extra` is consulted *before* the native provider, which is how a symbol
    /// that must be virtualized rather than passed through gets intercepted —
    /// see [`machish::resolve`] for why ordering is the mechanism.
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
        let mut memory = BufferSpace::new(0x1_0000_0000);
        let mut source = FsDylibSource::new();

        let eager = eager_binding_shims();
        let native = NativeDylibProvider::system();
        let mut providers: Vec<&dyn SymbolProvider> = extra.to_vec();
        providers.push(&eager);
        providers.push(&native);

        let report =
            Loader::new(X86_64).link_with(bytes, path, &mut memory, &mut source, &providers)?;

        // The root image's `__text` is what the driver lifts. Re-parsing here
        // rather than threading a `MachObject` out of the loader keeps the
        // report the only thing that crosses the boundary.
        let obj = machish::image::MachObject::parse_any(bytes, X86_64)?;
        let text_sect = obj.section("__TEXT", "__text").ok_or(ProgramError::NoText)?;
        let slide = report.root().map_or(0, |i| i.slide);
        let text_va = text_sect.addr.wrapping_add(slide);

        let mut text = vec![0u8; text_sect.size as usize];
        memory.read(text_va, &mut text)?;

        let functions: Vec<std::ops::Range<u64>> =
            machish::image::function_ranges(&obj, text_sect.addr, text_sect.addr + text_sect.size)?
                .into_iter()
                .map(|r| r.start.wrapping_add(slide)..r.end.wrapping_add(slide))
                .collect();

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
                imports.insert(stub, ImportSite { is_stub: true, ..site });
            }
        }

        Ok(Program { report, memory, text: (text_va, text), functions, imports })
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
            config.segments.push(MemorySegment::new(base, bytes.to_vec(), false));
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
            return Err(AbiError::NoSignature { symbol: format!("<unregistered {va:#x}>") });
        };
        let (symbol, tag, resolved) = (site.symbol.clone(), site.tag, site.resolved);
        crate::abi::call_import(&symbol, tag, resolved, ctx, &mut self.memory)
    }

    /// The region a JIT should lift in order to execute `va`: the enclosing
    /// function, and the bytes to hand microlift.
    ///
    /// Function granularity, from `LC_FUNCTION_STARTS`, because microlift lifts
    /// a region with its internal control flow already resolved — so a whole
    /// function is one unit of work and one cache entry, while a basic block
    /// would fragment every loop.
    ///
    /// `None` when `va` is outside the root image's `__text`, which today covers
    /// three real cases a running JIT must handle and this does not yet: code in
    /// a bundled dylib, code in an image with no `LC_FUNCTION_STARTS`, and code
    /// the guest generated at runtime. All three are M3.
    pub fn lift_region(&self, va: u64) -> Option<(u64, &[u8])> {
        let (text_va, text) = &self.text;
        let f = self.functions.iter().find(|f| f.contains(&va))?;
        let start = usize::try_from(f.start.checked_sub(*text_va)?).ok()?;
        let end = usize::try_from(f.end.checked_sub(*text_va)?).ok()?.min(text.len());
        let bytes = text.get(start..end).filter(|b| !b.is_empty())?;
        Some((f.start, bytes))
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

    /// Every import address, in ascending order.
    pub fn import_addresses(&self) -> impl Iterator<Item = u64> + '_ {
        self.imports.keys().copied()
    }
}
