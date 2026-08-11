//! A loaded guest program, ready to be lifted.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::Range;

use machish::loader::Loader;
use machish::memory::{BufferSpace, Regions};
use machish::report::LinkReport;
use machish::resolve::{NativeDylibProvider, SymbolProvider, eager_binding_shims};
use machish::spec::arch::X86_64;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::exit::MemorySegment;
use microlift::core::specialize::Specialization;
use microlift::core::{AddressMode, Atomic128Mode, LifterConfig};

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
        // Nor a traced one. An inlined call never reaches the seam, so
        // promoting a symbol the user asked to watch would silently trace
        // nothing — and the symbols worth watching are exactly the ordinary,
        // fully-signatured ones this would otherwise inline. See
        // [`crate::trace`].
        if crate::trace::watching(&site.symbol) {
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
    /// A `HashMap` where [`imports`] is a `BTreeMap`, and the difference is
    /// measured. The run loop asks this one about **every address it is about to
    /// enter** and the answer is "no" almost every time, so what it costs is a
    /// *miss*: a `BTreeMap` miss over the thousands of imports a real program
    /// has is a dozen dependent loads with no locality, and a `sample` of
    /// Terraria's start-up put 13% of the whole process in it. A hash miss is
    /// one multiply and one cache line.
    ///
    /// A range rejection was tried first — two compares against the lowest and
    /// highest key, which is what [`crate::dyld::import_at`] uses — and rejects
    /// **nothing**, for a reason worth writing down: a key here is a host
    /// address, but "host" includes this runtime's own shims, and the runtime is
    /// mapped in the same part of the address space as the guest images. The two
    /// populations interleave rather than separating.
    ///
    /// [`imports`]: Program::imports
    imports_by_target: AddrMap<ImportSite>,
}

/// A hasher for maps keyed by a guest or host **address**.
///
/// Rust's default is SipHash-1-3, chosen to make `HashMap` safe against an
/// attacker who controls the keys. That is the wrong trade for the two maps in
/// this runtime that are consulted on the hottest path there is — once per
/// region entry, by [`Program::import_by_target`] and its counterpart in
/// [`crate::dyld`] — where the keys are addresses this runtime itself computed
/// and every lookup is a miss. Replacing SipHash here took `dyld`'s share of a
/// Terraria profile from 9.5% to noise.
///
/// The mixing is Fibonacci hashing: one multiply by the odd constant nearest
/// 2^64/φ, then a shift to bring the high bits down, which is what
/// `HashMap` reads. It is *not* a general-purpose hash and must not be used for
/// anything an outside party chooses the keys of.
#[derive(Clone, Copy, Default)]
pub struct AddrHasher(u64);

impl std::hash::Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Only reached if someone keys one of these maps by something other
        // than an integer. Correct rather than fast, so that doing so is a
        // performance mistake instead of a correctness one.
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.0 ^= self.0 >> 32;
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// [`AddrHasher`] as a `HashMap` may ask for one.
pub type AddrHash = std::hash::BuildHasherDefault<AddrHasher>;

/// A map from an address to whatever it names.
pub type AddrMap<V> = HashMap<u64, V, AddrHash>;

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

/// The provider chain every link in this runtime uses, in the order that makes
/// it safe.
///
/// Order *is* the mechanism — see [`machish::resolve`]. Hand-written shims come
/// ahead of the host's `dlsym` because the symbols they claim must be
/// virtualized rather than passed through, and there is nothing in a name to say
/// which those are.
///
/// It lives here, in one value, because there are now two callers: loading the
/// program, and [`crate::dyld`] loading an image the *guest* asked for. Those
/// two links must answer an import identically, and the only way to guarantee
/// that is for them to be the same list.
pub(crate) struct HostProviders {
    /// `___stack_chk_guard` and `__tlv_bootstrap`: symbols the host declines to
    /// export and every guest with an array on its stack dereferences in its
    /// own prologue. See [`crate::shims::runtime`].
    runtime: machish::resolve::SyntheticDylib,
    /// The Objective-C runtime. The native provider denies the `_objc_`,
    /// `_class_`, `_sel_` and `_protocol_` families precisely so a shim can
    /// claim them: passing one through unexamined would hand the host runtime a
    /// guest class. See [`crate::objc::dispatch`].
    objc: machish::resolve::SyntheticDylib,
    /// The `dl` family, which the host exports and cannot usefully answer for an
    /// x86-64 file. See [`crate::dyld`].
    dyld: machish::resolve::SyntheticDylib,
    eager: machish::resolve::SyntheticDylib,
    native: NativeDylibProvider,
}

impl HostProviders {
    pub(crate) fn new() -> Self {
        HostProviders {
            runtime: crate::shims::runtime::provider(),
            objc: crate::objc::dispatch::provider(),
            dyld: crate::dyld::provider(),
            eager: eager_binding_shims(),
            native: NativeDylibProvider::system(),
        }
    }

    /// The chain, with `extra` consulted ahead of everything.
    pub(crate) fn chain<'a>(
        &'a self,
        extra: &[&'a dyn SymbolProvider],
    ) -> Vec<&'a dyn SymbolProvider> {
        let mut providers: Vec<&dyn SymbolProvider> = extra.to_vec();
        providers.push(&self.runtime);
        providers.push(&self.objc);
        providers.push(&self.dyld);
        providers.push(&self.eager);
        providers.push(&self.native);
        providers
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
        // Vendored libc++ ahead of the filesystem; see [`crate::cxxlib`].
        let mut source = crate::cxxlib::source();
        let hosts = HostProviders::new();
        let providers = hosts.chain(extra);

        let mut report =
            Loader::new(X86_64).link_with(bytes, path, &mut memory, &mut source, &providers)?;

        // Before anything reads the report: the guest's landing pads call
        // `_Unwind_Resume` through libSystem, which native passthrough answers
        // with the *host's* unwinder, while `__cxa_throw` names libc++ and
        // reaches the vendored guest one. One unwind, two unwinders. See
        // [`crate::cxxlib::bind_unwinder_to_the_vendored_one`].
        crate::cxxlib::bind_unwinder_to_the_vendored_one(&mut report, &mut memory)?;

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
        let mut imports_by_target: AddrMap<ImportSite> = AddrMap::default();
        for site in by_symbol {
            // Guest-resolved sites are excluded for the reason
            // [`crate::dyld`] spells out: this map is consulted for every
            // address the run loop is about to *enter*, so a guest function
            // that is also some image's import would transfer to itself
            // forever. A bundled dylib dependency is the case that reaches it
            // here.
            if site.resolved != 0 && memory.region_at(site.resolved).is_none() {
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
    /// constant at lift time instead of becoming a runtime memory access. Each
    /// piece carries the writability of the segment it comes from, because that
    /// is the whole safety condition — the lifter must not fold a value the
    /// guest can later store over, and a `__DATA` page handed over as read-only
    /// is exactly that mistake. See [`Program::writable_ranges`].
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
        // Host-specific, so it is the half of the optimisation setting that
        // belongs to the lift rather than to the lowering — see [`crate::jit`].
        // Below `all` the guest's vector registers stay scalar lo/hi pairs.
        let use_vector_xmm = crate::jit::opt_level().host_specific() && {
            #[cfg(target_arch = "aarch64")]
            {
                std::arch::is_aarch64_feature_detected!("neon")
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                std::arch::is_x86_feature_detected!("sse2")
            }
            #[cfg(not(any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")))]
            {
                false
            }
        };
        // `CMPXCHG16B`, and the one setting here that is a correctness
        // decision rather than a tuning one. `Emulated` is a load, a compare
        // and a store — right for a single-threaded guest and a data race for
        // any other — and Unity's job system pops a lock-free free list with it
        // on every worker. `Exit` hands the instruction to
        // [`crate::atomic128`], which does it atomically with the host's own
        // exclusive pair. See that module for what the race cost.
        let atomic128 = {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                if std::arch::is_x86_feature_detected!("cmpxchg16b") {
                    Atomic128Mode::Native
                } else {
                    Atomic128Mode::Exit
                }
            }
            #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
            {
                Atomic128Mode::Exit
            }
        };
        let specialization = if crate::jit::opt_level().host_specific() {
            Specialization::host()
        } else {
            Specialization::generic()
        };
        let mut config = LifterConfig {
            interpreter_fallback: true,
            use_vector_xmm,
            outline_helpers: true,
            address_mode: AddressMode::Direct,
            atomic128,
            specialization,
            eliminate_dead_flags: crate::jit::eliminate_dead_flags(),
            ..LifterConfig::default()
        };

        // A mapped region is a whole image's reservation, spanning segments of
        // both kinds, so it is cut at the writable segments' edges rather than
        // handed over whole.
        let writable = self.writable_ranges();
        let mut segments: Vec<MemorySegment> = Vec::new();
        for (base, bytes) in self.memory.regions() {
            let end = base.wrapping_add(bytes.len() as u64);
            let mut piece = |from: u64, to: u64, is_writable: bool| {
                let (lo, hi) = ((from - base) as usize, (to - base) as usize);
                segments.push(MemorySegment::new(
                    from,
                    bytes[lo..hi].to_vec(),
                    is_writable,
                ));
            };
            let mut at = base;
            for range in writable.iter().filter(|r| r.start < end && r.end > base) {
                let (start, stop) = (range.start.max(base), range.end.min(end));
                if at < start {
                    piece(at, start, false);
                }
                piece(start, stop, true);
                at = stop;
            }
            if at < end {
                piece(at, end, false);
            }
        }
        // Set rather than pushed one by one, because the field is shared: a
        // `MemorySegment` owns a copy of the bytes it describes, so this is the
        // program's whole `__TEXT` and `__DATA`, and every lift clones the
        // config it lives in. See `LifterConfig::segments`.
        config.set_segments(segments);
        for (&va, site) in &self.imports {
            // A fast import is an **absolute native call** the lifter bakes into
            // the translation, so the address had better be native code. Asked
            // here rather than in `promotion_for` because it is the one question
            // that needs the address space, and only a `Program` has it.
            //
            // What it catches: a symbol that resolved into *guest* memory. Guest
            // pages are mapped `r--` and never executable — that is the
            // no-`MAP_JIT` decision in the table above — so an inlined call to
            // one faults on instruction fetch with the PC *equal to* the fault
            // address, several frames below anything named, and only when that
            // call site is finally reached. Everywhere else in the runtime the
            // same case is handled: `Vm::call_site` transfers to guest code
            // rather than marshalling it, and `dispatch_indirect` asks
            // `is_guest_address` before treating a pointer as native. Promotion
            // was the one path that did not ask, and `--slow` hid it by never
            // promoting anything.
            let promoted = policy
                .promotion_for(site)
                .filter(|_| !self.is_guest_address(site.resolved));
            match promoted {
                Some(abi) => config.register_fast_import(
                    va,
                    crate::abi::dynamic_symbol(abi, site.resolved),
                    crate::abi::guest_call_abi(),
                ),
                // Everything a fast import cannot express — a variadic, a
                // callback taker, a shim, a signature the marshaller refuses —
                // used to be a VM exit and is now a bridged call. The work is
                // identical; what goes away is the round trip. See
                // [`crate::bridge`].
                None if policy != ImportPolicy::AlwaysSlow => {
                    config.register_bridge_import(va, crate::bridge::import_bridge())
                }
                None => config.register_slow_import(va),
            }
        }

        // Only under the fast policy, because [`ImportPolicy::AlwaysSlow`] is
        // the baseline the rest is measured against and a baseline with two
        // kinds of inline call in it measures nothing. Both of these are
        // *additions* to what the run loop already answers, not replacements:
        // an address neither one recognises still arrives as the exit it always
        // did.
        if policy != ImportPolicy::AlwaysSlow {
            // Sites that did not exist when this config was built — see
            // [`crate::bridge::DynamicImports`] and [`crate::dyld`].
            config.set_import_registry(crate::bridge::registry());
            // And the calls whose target no lift-time question can answer: a
            // `dlsym` result, a stored `objc_msgSend`, a `__tlv_bootstrap`.
            config.set_indirect_call_bridge(crate::bridge::indirect_bridge());
        }
        config
    }

    /// Every VA range the guest may store to, merged and in address order.
    ///
    /// The loader records a segment's `initprot`, so this is the guest's own
    /// answer rather than a guess from segment names: `__DATA`, `__DATA_DIRTY`
    /// and a `__DATA_CONST` that chained fixups made writable all land here,
    /// and so does anything an image invents. Overlaps are merged because two
    /// images' segments can share a host page, and a range that is writable for
    /// either one is writable.
    fn writable_ranges(&self) -> Vec<Range<u64>> {
        let mut ranges: Vec<Range<u64>> = self
            .report
            .images
            .iter()
            .flat_map(|image| &image.segments)
            .filter(|seg| seg.writable)
            .map(|seg| seg.vmaddr..seg.vmaddr.wrapping_add(seg.vmsize))
            .filter(|r| r.start < r.end)
            .collect();
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
        for r in ranges {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
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

    /// The region a JIT should lift in order to execute `va`: which bytes are
    /// code, and the slice to hand microlift.
    ///
    /// The region begins at `va` and its extent is computed by walking the
    /// guest's control flow from there — `LC_FUNCTION_STARTS` is not consulted,
    /// because a recorded extent is a claim about which bytes are code and the
    /// walk answers that question better. See [`crate::region`].
    ///
    /// Any mapped image answers, not only the root's `__text`: a bundled dylib,
    /// an image whose function starts were stripped, and code the guest wrote at
    /// run time all reach this the same way.
    ///
    /// `None` when `va` is not in mapped guest memory, or when nothing there
    /// decodes.
    pub fn lift_region(&self, va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
        let Some((base, bytes)) = self.memory.region_at(va) else {
            // Or an image the *guest* loaded, which is not in this `Program` and
            // cannot be: a `Program` is immutable once linked, which is what
            // `SharedProgram`'s `unsafe impl Sync` rests on. See [`crate::dyld`].
            return crate::dyld::lift_region(va)
                // Or code the guest *wrote*, which is in no image at all — see
                // [`crate::shims::vmmap`].
                .or_else(|| lift_generated(va));
        };
        // A loaded image is not memory the guest writes, so the Mono call rule
        // does not apply to it. See [`crate::region`].
        let region = crate::region::region_for(bytes, base, va, false)?;
        let code = slice_of(bytes, base, &region)?;
        Some((region, code))
    }

    /// The recorded function starts inside `range`, ascending.
    ///
    /// What a translation needs in order to be enterable at an address the guest
    /// only ever reaches through a *pointer* — a vtable slot, a callback handed
    /// to `qsort`, an Objective-C method. Such an address has no static
    /// predecessor, so nothing in a lift discovers it, and a region that spans it
    /// without offering it as a resume point forces a second translation of the
    /// same bytes. See [`crate::cache`] on why the set of resume points is
    /// declared rather than made universal.
    ///
    /// Only the root program's own records; an image the guest loaded answers
    /// through [`crate::dyld`], and generated code has no records at all.
    pub fn function_starts_in(&self, range: std::ops::Range<u64>) -> Vec<u64> {
        let mut starts: Vec<u64> = self
            .functions
            .iter()
            .map(|f| f.start)
            .filter(|s| range.contains(s))
            .chain(crate::dyld::function_starts_in(range.clone()))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        starts
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
            || crate::dyld::contains(va)
            // Or memory the guest *generated* code into, which is in no image at
            // all. See [`crate::shims::vmmap`] for why the set of such addresses
            // is exactly the set the guest asked to be executable.
            || crate::shims::vmmap::contains(va)
    }

    /// `va` as `image+offset`, in the image's *own* address space.
    ///
    /// Every address this runtime reports is slid by whatever `mmap` chose that
    /// run, so two runs of the same failure name two different numbers and
    /// neither can be looked up in a disassembler. This is the form that
    /// survives: `Mac_Runner+0x1a3c50` is the same place every time, and it is
    /// what `otool -arch x86_64 -tV` and a debugger both speak.
    ///
    /// Searched by segment rather than by mapped region, so the answer names the
    /// image the address is *in* rather than whichever reservation happens to
    /// contain it.
    pub fn locate(&self, va: u64) -> Option<String> {
        for image in &self.report.images {
            for seg in &image.segments {
                let start = seg.vmaddr;
                if va >= start && va < start.wrapping_add(seg.vmsize) {
                    let name = image.name.rsplit('/').next().unwrap_or(&image.name);
                    return Some(format!("{name}+{:#x}", va.wrapping_sub(image.slide)));
                }
            }
        }
        crate::dyld::locate(va)
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
    /// An image the guest loaded contributes its sites here too, and that is
    /// what makes a call into one survivable. Its call sites were never
    /// registered with the lifter — the config is built before the guest runs —
    /// so a call to one of its `__stubs` entries arrives as an `UnknownJump`
    /// rather than an `ImportedFunctionCall`, and answering it *here* is what
    /// stops the recompiler decoding the trampoline and following it into the
    /// host. See [`crate::dyld`].
    pub fn import_at(&self, va: u64) -> Option<&ImportSite> {
        self.imports.get(&va).or_else(|| crate::dyld::import_at(va))
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
        self.imports_by_target
            .get(&host)
            .or_else(|| crate::dyld::import_by_target(host))
    }

    /// Every import address, in ascending order.
    pub fn import_addresses(&self) -> impl Iterator<Item = u64> + '_ {
        self.imports.keys().copied()
    }
}

/// The bytes of a region, cut out of the mapping it was found in.
///
/// The slice spans the region's blocks and the holes between them; the lifter is
/// told which is which separately (`LifterConfig::block_ranges`), and reads only
/// the blocks. Handing it the span rather than the blocks keeps every guest
/// address an index into one buffer, which is what makes a branch target mean
/// the same thing to both sides.
fn slice_of(bytes: &[u8], base: u64, region: &crate::region::Region) -> Option<Vec<u8>> {
    let start = usize::try_from(region.range.start.checked_sub(base)?).ok()?;
    let end = usize::try_from(region.range.end.checked_sub(base)?)
        .ok()?
        .min(bytes.len());
    Some(bytes.get(start..end)?.to_vec())
}

/// The region to lift in order to execute `va`, when `va` is code the guest
/// *wrote* rather than code it loaded.
///
/// ## What is different about an arena
///
/// Only one thing, and it is the reason this function exists rather than being
/// a call site of the image path: these bytes are **writable**, so the guest can
/// and does rewrite them. That selects [`crate::region`]'s Mono rule — a call in
/// writable executable memory ends its block and its resume point is not
/// followed — which answers both of the things Mono does to a region walk: the
/// inline argument it puts after a call, and the backpatching that makes any
/// translation reaching past a call site stale the moment the call is patched.
///
/// Everything else is the same. The walk needs no function record here and asks
/// for none anywhere; the arena's end is the only upper bound.
///
/// ## The bytes are copied, deliberately
///
/// The arena is live guest memory, and the guest's own JIT is still writing to
/// it — possibly on another thread. Lifting from a borrowed slice would mean
/// decoding bytes that change underneath the decoder. Copying the region's
/// extent gives the translation a fixed input, which is the only way its
/// instruction grid means anything afterwards.
///
/// ## Scope
///
/// The code cache never invalidates, so a guest that frees a method and emits a
/// different one at the same address is served the first translation. Mono's
/// code manager does reuse space, which makes this the next thing this path will
/// need and not a theoretical concern — closing it means write-protecting arena
/// pages and flushing the cache on fault, which is the standing self-modifying-code
/// item in the roadmap.
fn lift_generated(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    let (base, len) = crate::shims::vmmap::arena_at(va)?;
    let offset = usize::try_from(va - base).ok()?;
    let len = usize::try_from(len).ok()?;

    // SAFETY: `base` came from a successful `mmap` this runtime made on the
    // guest's behalf and never unmapped — see [`crate::shims::vmmap`], where the
    // arena is recorded — so the whole range is mapped and readable. The guest
    // may be writing into it concurrently, which is why the bytes are copied
    // here rather than borrowed: a torn read produces an instruction stream that
    // fails to decode, not a dangling reference.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(base as usize as *const u8, len) };

    let region = crate::region::region_for(bytes, base, va, true)?;
    if crate::shims::vmmap::tracing() {
        let head = &bytes[offset..(offset + 64).min(len)];
        eprintln!(
            "rosetta3: lift generated {va:#x} in arena {base:#x}+{len:#x}, region {:#x}..{:#x}, {} starts, bytes {}",
            region.range.start,
            region.range.end,
            region.starts.len(),
            head.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let code = slice_of(bytes, base, &region)?;
    Some((region, code))
}
