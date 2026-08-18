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

use crate::abi::{AbiError, Classification, Lookup};

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
    /// The signature to inline this site's call with, and the address to inline a call
    /// to.
    fn promotion_for(self, site: &ImportSite) -> Option<crate::objc::fast::Promotion> {
        if self == ImportPolicy::AlwaysSlow || site.resolved == 0 {
            return None;
        }
        // A traced symbol is never promoted.
        if crate::trace::watching(&site.symbol) {
            return None;
        }
        // A shimmed symbol must never be promoted: the whole reason it has a
        // shim is that calling the host function directly would be wrong.
        if crate::shims::is_shimmed(&site.symbol) {
            let promotion = crate::objc::fast::promotion(&site.symbol, site.resolved)?;
            // Classified as well as described, so a promoted entry passes the
            // same marshallability test every other fast import passes rather
            // than being taken on the table's word.
            return (crate::abi::classify(site.tag, &site.symbol) == Classification::Fast)
                .then_some(promotion);
        }
        if crate::abi::classify(site.tag, &site.symbol) != Classification::Fast {
            return None;
        }
        match crate::abi::table::lookup(site.tag, &site.symbol) {
            Lookup::Known(abi) => Some(crate::objc::fast::Promotion {
                abi,
                address: site.resolved,
            }),
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
/// [`MmapSpace`]: machish::memory::MmapSpace
pub struct Program<M: Regions = BufferSpace> {
    /// The full linking transcript.
    pub report: LinkReport,
    /// The guest's mapped memory.
    pub memory: M,
    /// `(virtual address, bytes)` of the root image's `__TEXT,__text`.
    pub text: (u64, Vec<u8>),
    /// Slid extents of each function in `__text`, from `LC_FUNCTION_STARTS`.
    pub functions: Vec<std::ops::Range<u64>>,
    /// Every address a call site might name, mapped to the host address behind
    /// it: both `slot_va` and `stub_va` for each binding.
    pub imports: BTreeMap<u64, ImportSite>,
    /// How to leave each function of the root image, from `__unwind_info`.
    ///
    /// Read at load for the same reason [`functions`](Program::functions) is:
    pub unwind: machish::image::UnwindInfo,
    /// The same sites, keyed by the address they *resolved to*.
    ///
    /// A `HashMap` where [`imports`] is a `BTreeMap`, and the difference is measured.
    imports_by_target: AddrMap<ImportSite>,
}

/// A hasher for maps keyed by a guest or host **address**.
///
/// Rust's default is SipHash-1-3, chosen to make `HashMap` safe against an attacker who
/// controls the keys.
///
/// The mixing is Fibonacci hashing: one multiply by the odd constant nearest 2^64/φ,
/// then a shift to bring the high bits down, which is what `HashMap` reads.
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
    /// `extra` is consulted *before* the native provider, which is how a symbol that
    /// must be virtualized rather than passed through gets intercepted — see
    /// [`machish::resolve`] for why ordering is the mechanism.
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
    /// The mapping is never executable. The guest is x86-64; this CPU cannot run a byte
    /// of it under any protection, and the recompiler reads it as data.
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

/// The provider chain every link in this runtime uses, in the order that makes it safe.
///
/// Order *is* the mechanism — see [`machish::resolve`].
///
/// It lives here, in one value, because there are now two callers: loading the program,
/// and [`crate::dyld`] loading an image the *guest* asked for.
pub(crate) struct HostProviders {
    /// `___stack_chk_guard` and `__tlv_bootstrap`: symbols the host declines to export
    /// and every guest with an array on its stack dereferences in its own prologue.
    runtime: machish::resolve::SyntheticDylib,
    /// The Objective-C runtime. The native provider denies the `_objc_`, `_class_`,
    /// `_sel_` and `_protocol_` families precisely so a shim can claim them:
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

        // Before anything reads the report:
        crate::cxxlib::bind_unwinder_to_the_vendored_one(&mut report, &mut memory)?;

        // And the same for the C++ ABI: a binary linked against `libstdc++`
        // names *that* for `__cxa_guard_acquire` and the `type_info` vtables,
        // which native passthrough answers with the host's.
        crate::cxxlib::bind_cxxabi_to_the_vendored_one(&mut report, &mut memory)?;

        // And last, because it is stated in terms of what everything above
        // settled on: a guest image may declare `__DATA,__interpose` and take
        // over a function the rest of the program calls.
        crate::interpose::apply(&mut report, &mut memory)?;

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
        // Not for completeness: for the diagnostic.
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
            // Guest-resolved sites are excluded for the reason [`crate::dyld`]
            // spells out:
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
    /// **The guest's mapped images**, as a hint for microlift's value-set analysis:
    pub fn lifter_config(&self) -> LifterConfig {
        self.lifter_config_with(ImportPolicy::FastWhenKnown)
    }

    /// [`Program::lifter_config`], with the promotion decision made explicitly.
    ///
    /// [`ImportPolicy::AlwaysSlow`] exists because it is the falsifiable half of the
    /// claim:
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
        // `CMPXCHG16B`, and the one setting here that is a correctness decision
        // rather than a tuning one.
        let atomic128 = {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                if std::arch::is_x86_feature_detected!("cmpxchg16b") {
                    Atomic128Mode::Native
                } else {
                    Atomic128Mode::Exit
                }
            }
            #[cfg(target_arch = "aarch64")]
            {
                if std::arch::is_aarch64_feature_detected!("lse") {
                    Atomic128Mode::Native
                } else {
                    Atomic128Mode::Exit
                }
            }
            #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
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
            // Let a region resolve its own successor out of this thread's
            // window instead of returning to the run loop to have the same
            // address looked up.
            block_link: crate::jit::block_linking().then(crate::thread::link_config),
            // `exit_trampoline_address` is left at its default `None`, and that
            // is a decision rather than an omission:
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
        // Set rather than pushed one by one, because the field is shared:
        config.set_segments(segments);
        for (&va, site) in &self.imports {
            // A fast import is an **absolute native call** the lifter bakes
            // into the translation, so the address had better be native code.
            let promoted = policy
                .promotion_for(site)
                .filter(|_| !self.is_guest_address(site.resolved));
            match promoted {
                Some(p) => config.register_fast_import(
                    va,
                    crate::abi::dynamic_symbol(p.abi, p.address),
                    crate::abi::guest_call_abi(),
                ),
                // Everything a fast import cannot express — a variadic, a
                // callback taker, a shim, a signature the marshaller refuses —
                None if policy != ImportPolicy::AlwaysSlow => {
                    config.register_bridge_import(va, crate::bridge::import_bridge())
                }
                None => config.register_slow_import(va),
            }
        }

        // Only under the fast policy, because [`ImportPolicy::AlwaysSlow`] is
        // the baseline the rest is measured against and a baseline with two
        // kinds of inline call in it measures nothing.
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
    /// The acceptance question in one call:
    pub fn classify_imports(&self) -> BTreeMap<&str, Classification> {
        let mut out = BTreeMap::new();
        for b in self.report.bindings() {
            out.insert(b.symbol.as_str(), crate::abi::classify(b.tag, &b.symbol));
        }
        out
    }

    /// Service an `ExitReason::ImportedFunctionCall`.
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

    /// The region a JIT should lift in order to execute `va`: which bytes are code, and
    /// the slice to hand microlift.
    ///
    /// `None` when `va` is not in mapped guest memory, or when nothing there decodes.
    pub fn lift_region(&self, va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
        let Some((base, bytes)) = self.memory.region_at(va) else {
            // Or an image the *guest* loaded, which is not in this `Program` and
            // cannot be: a `Program` is immutable once linked, which is what
            // `SharedProgram`'s `unsafe impl Sync` rests on. See [`crate::dyld`].
            return crate::dyld::lift_region(va)
                // Or code the guest *wrote*, which is in no image at all — see
                // [`crate::shims::vmmap`].
                .or_else(|| lift_generated(va))
                // Or code *this runtime* wrote, which is in no image either.
                .or_else(|| crate::trampoline::lift_region(va))
                // Including the one instruction that ends a call: the return
                // address every entry into guest code pushes. See
                // [`crate::retstub`].
                .or_else(|| crate::retstub::lift_region(va));
        };
        // A loaded image is not memory the guest writes, so the Mono call rule
        // does not apply to it. See [`crate::region`].
        let region = crate::region::region_for(bytes, base, va, false)?;
        let code = slice_of(bytes, base, &region)?;
        Some((region, code))
    }

    /// The recorded function starts inside `range`, ascending.
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
    /// The question every `ExitReason::IndirectCall` has to answer.
    pub fn is_guest_address(&self, va: u64) -> bool {
        self.memory.region_at(va).is_some()
            || crate::dyld::contains(va)
            // Or memory the guest *generated* code into, which is in no image at
            // all. See [`crate::shims::vmmap`] for why the set of such addresses
            // is exactly the set the guest asked to be executable.
            || crate::shims::vmmap::contains(va)
            // Or one of this runtime's own five-byte stubs, which is guest code
            // by construction: it exists to be lifted. See [`crate::trampoline`].
            || crate::trampoline::contains(va)
            // Or the return stub, which is guest code for the same reason: a
            // `ret` transfers to it and the run loop lifts it. See
            // [`crate::retstub`].
            || crate::retstub::contains(va)
    }

    /// `va` as `image+offset`, in the image's *own* address space.
    ///
    /// Every address this runtime reports is slid by whatever `mmap` chose that run, so
    /// two runs of the same failure name two different numbers and neither can be
    /// looked up in a disassembler.
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
            .or_else(|| crate::trampoline::locate(va))
            .or_else(|| crate::retstub::locate(va))
    }

    /// Point this program's import slots at [trampolines](crate::trampoline).
    ///
    /// The **relocation** moves and the registration does not:
    ///
    /// Not done in [`Program::load_into`], and the reason is
    /// [`ImportPolicy::AlwaysSlow`]:
    pub fn install_trampolines(&mut self) -> usize {
        // Collected first, because deciding needs `&self` (for
        // `is_guest_address`) and writing needs `&mut self.memory`. A program
        // has thousands of imports and this runs once.
        let eligible: Vec<(u64, ImportSite)> = self
            .imports
            .iter()
            .filter(|(_, site)| !site.is_stub && !self.is_guest_address(site.resolved))
            .map(|(&va, site)| (va, site.clone()))
            .collect();
        crate::trampoline::install(
            eligible.iter().map(|(va, site)| (*va, site)),
            // Already filtered above, where the borrow was available.
            &|_| false,
            &mut self.memory,
        )
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

    /// Whether the root image's entry point is an `LC_UNIXTHREAD` one — which
    /// is to say whether it reads the process arguments off its own stack
    /// instead of taking them in registers. See [`crate::stack`].
    pub fn entry_reads_stack(&self) -> bool {
        matches!(
            self.report.root().and_then(|i| i.entry_command),
            Some(machish::image::EntryPoint::UnixThread { .. })
        )
    }

    /// How to leave the function containing `va`.
    pub fn unwind_entry(&self, va: u64) -> Option<machish::image::UnwindEntry> {
        self.unwind.lookup(va).copied()
    }

    /// The import registered at `va`, if any.
    ///
    /// `va` is whatever an exit named:
    pub fn import_at(&self, va: u64) -> Option<&ImportSite> {
        self.imports
            .get(&va)
            .or_else(|| crate::dyld::import_at(va))
            // And the synthetic addresses a trampoline jumps to.
            .or_else(|| crate::trampoline::import_at(va))
    }

    /// What an imported symbol resolved to, by name.
    ///
    /// The reverse of the usual question, and it has one caller:
    pub fn resolved_symbol(&self, symbol: &str) -> Option<u64> {
        self.imports
            .values()
            .find(|s| !s.is_stub && s.symbol == symbol && s.resolved != 0)
            .map(|s| s.resolved)
    }

    /// The import that *resolved to* `host`, if any.
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
fn slice_of(bytes: &[u8], base: u64, region: &crate::region::Region) -> Option<Vec<u8>> {
    let start = usize::try_from(region.range.start.checked_sub(base)?).ok()?;
    let end = usize::try_from(region.range.end.checked_sub(base)?)
        .ok()?
        .min(bytes.len());
    Some(bytes.get(start..end)?.to_vec())
}

/// The region to lift in order to execute `va`, when `va` is code the guest *wrote*
/// rather than code it loaded.
///
/// ## What is different about an arena
///
/// Only one thing, and it is the reason this function exists rather than being a call
/// site of the image path:
///
/// ## The bytes are copied, deliberately
///
/// The arena is live guest memory, and the guest's own JIT is still writing to it —
/// possibly on another thread.
///
/// ## Scope
///
/// The code cache never invalidates, so a guest that frees a method and emits a
/// different one at the same address is served the first translation.
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
