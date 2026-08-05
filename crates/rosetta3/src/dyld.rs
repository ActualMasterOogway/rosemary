//! Guest-initiated `dlopen`/`dlsym`: loading an Intel image after the guest has
//! started running.
//!
//! ## Why the host's `dlopen` cannot be forwarded
//!
//! Every other libSystem entry point in this runtime is passed through, and the
//! reason it works is that the host's implementation operates on *memory* the
//! two sides share. `dlopen` operates on a **Mach-O file**, and the file is
//! x86-64. The host's loader looks at it, finds no arm64 slice, and refuses —
//! *"fat file, but missing compatible architecture (have 'i386,x86_64', need
//! 'arm64')"*. That is not a bug to work around; it is the correct answer to the
//! question the host was asked, and it is the wrong question. The image has to
//! be loaded the way the root image was: through [`machish`], into this process
//! at its own addresses, with its imports answered by the same provider chain.
//!
//! UNDERTALE's GameMaker runner is what forced this. It `dlopen`s
//! `libsteam_api.dylib` out of its own bundle, and when that fails it calls a
//! function pointer Steam never filled in — so the failure surfaces four frames
//! later as `no guest code at 0x0`, which says nothing at all about `dlopen`.
//!
//! ## What the lifter is not told, and why that is survivable
//!
//! A [`LifterConfig`](microlift::core::LifterConfig) is built once, before the
//! guest runs, and it carries the registered import addresses. An image loaded
//! *after* that has none of its call sites registered, and there is no
//! supported way to add them to translations that already exist.
//!
//! The consequence is precisely stated in the crate note on `stub_va`: an
//! unregistered call to a `__stubs` entry is followed *as guest code*, decoding
//! the six-byte trampoline, chasing its `jmp qword [rip+disp]` into the pointer
//! slot, and finding a native arm64 host address there to disassemble as
//! x86-64. So the registration has to happen somewhere.
//!
//! It happens at the exit instead of at lift time. An unregistered direct call
//! leaves microlift as `ExitReason::UnknownJump` with `TransferKind::DirectCall`,
//! and the runtime's loop already asks [`Program::import_at`] about the target
//! before lifting it — that question is what catches a tail `jmp` to an import.
//! Widening it to consult this module is the whole of the mechanism: a call into
//! a dynamically loaded image's stub is answered by the same marshaller an
//! `ImportedFunctionCall` exit is answered by, from an [`ImportSite`] that says
//! the same things. The cost is one VM exit per call that a registered import
//! would have inlined, which is the same trade a slow import already makes.
//!
//! [`Program::lift_region`] and [`Program::is_guest_address`] are widened for
//! the same reason and in the same direction, which is what makes a `dlsym`
//! result callable: a guest address, entered through the JIT, and turned into a
//! thunk by [`Callbacks::substitute`](crate::abi::Callbacks::substitute) on its
//! way out to a host argument.
//!
//! ## The registry is process-global
//!
//! For the reason [`crate::tls`]'s is. `dlopen` is reached from a shim, which is
//! handed a register context and guest memory and no VM; the VM is found on the
//! thread. But [`Program::lift_region`] is called with only a `&Program`, and
//! the image a `dlopen` produced is not in it and cannot be — a `Program` is
//! immutable once loaded, which is the claim `SharedProgram`'s `unsafe impl
//! Sync` rests on. A separate global registry keeps that claim intact rather
//! than quietly widening it.
//!
//! ## What is deliberately not done
//!
//! - **Unloading.** `dlclose` succeeds and does nothing. A translation cache
//!   with no invalidation cannot survive an image being unmapped and another
//!   landing on its addresses, and a leaked mapping is the honest cost of that.
//! - **The dynamically loaded image's Objective-C classes.**
//!   [`crate::objc::register`] realizes the *root* program's metadata. An image
//!   loaded later carrying its own classes would need the same pass run over its
//!   report; nothing does it yet, so a `+load` in a `dlopen`ed image does not
//!   run and a message to one of its classes fails by name in
//!   [`crate::objc::dispatch`] rather than silently.
//!
//!   Its **selector references** are the exception, and are uniqued at load by
//!   [`crate::objc::unique_loaded_selectors`]. That is not registration; it is
//!   the one part of it a loaded image cannot do without. A selector reference
//!   is a slot guest code *loads from* on its way into `objc_msgSend`, so until
//!   it is rewritten the image sends every message with a `char *` where a `SEL`
//!   belongs — and the host runtime answers, correctly and uselessly, that
//!   `NSProcessInfo` does not respond to `processInfo`.
//! - **`dladdr`, `RTLD_NEXT`, and re-export chains.** Each is refused or
//!   answered narrowly and says so, rather than being approximated.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use machish::exports::ExportTable;
use machish::image::MachObject;
use machish::install_name::PathContext;
use machish::loader::Loader;
use machish::memory::{MmapSpace, Regions};
use machish::report::LinkReport;
use machish::resolve::SyntheticDylib;
use machish::spec::arch::X86_64;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};
use crate::program::{HostProviders, ImportSite};

/// An image this runtime loaded on the guest's behalf.
///
/// Leaked once built, so every lookup can hand out a `&'static` reference while
/// the registry's lock is held only long enough to copy a pointer. That is the
/// same lifetime the mapping already has — see the scope note on unloading —
/// rather than a shortcut around one.
pub struct Image {
    /// The path actually opened, after `@`-prefix expansion.
    pub path: String,
    /// What `dlopen` returned. The image's base address, which is unique in this
    /// process by construction and is the one value a crash dump can be read
    /// against.
    pub handle: u64,
    pub report: LinkReport,
    /// The mapping, kept alive for the process. Leaked separately from the
    /// `Image` so that [`regions`](Image::regions) can borrow from it: the
    /// slices have to outlive the load, and they only can if the space does.
    _memory: &'static SharedSpace,
    /// The mapped extents, for [`contains`] and [`lift_region`].
    regions: Vec<(u64, &'static [u8])>,
    /// Every address a call site in this image might name — both `slot_va` and
    /// `stub_va` — exactly as [`crate::Program`] keys its own.
    imports: BTreeMap<u64, ImportSite>,
    /// The same sites by the address they resolved to; see
    /// [`Program::import_by_target`](crate::Program::import_by_target).
    imports_by_target: BTreeMap<u64, ImportSite>,
    /// Slid extents from `LC_FUNCTION_STARTS`, the hint [`lift_region`] uses.
    functions: Vec<Range<u64>>,
    exports: ExportTable,
    slide: u64,
}

/// An [`MmapSpace`] shared across the threads running the guest.
///
/// The same claim [`SharedProgram`](crate::vm::SharedProgram) makes, confined
/// the same way: what makes an `MmapSpace` `!Sync` automatically is the raw
/// pointers to its mappings, and those are read-only handles to memory that
/// lives as long as the process. The guest's races inside that memory are the
/// guest's own.
struct SharedSpace(MmapSpace);

// SAFETY: see the type note.
unsafe impl Sync for SharedSpace {}
unsafe impl Send for SharedSpace {}

/// Every image `dlopen` has produced, in load order.
///
/// Written under [`LOADING`] and read through [`VIEW`], which is what keeps the
/// read path off a lock entirely.
static IMAGES: Mutex<Vec<&'static Image>> = Mutex::new(Vec::new());

/// The published, immutable view of [`IMAGES`]: a leaked slice, swapped whole.
///
/// The read path takes no lock, and that is not premature. These lookups are
/// consulted at *every* import exit and every region entry — `sample` put
/// DELTARUNE's game loop mostly inside `pthread_mutex_lock` underneath
/// `import_at` — so the cost of answering "no" is the cost of the runtime.
///
/// Append-only and leaked makes the lock-free read sound: a `dlopen` builds a
/// **new** slice, leaks it, and swaps the pointer, so a reader either sees the
/// old slice or the new one and both stay valid forever. Nothing is ever freed,
/// which is already this module's position on unloading.
static VIEW: AtomicPtr<&'static [&'static Image]> = AtomicPtr::new(core::ptr::null_mut());

/// The guest executable's path and slide.
///
/// The path answers `@executable_path` and the search that finds a bundled dylib
/// named without a directory; the slide is what makes the executable's *exports*
/// usable, which a `dlopen`ed plugin needs. See [`MainExports`].
static EXECUTABLE: Mutex<Option<(String, u64)>> = Mutex::new(None);

/// The main executable's export table, parsed once, on first use.
static MAIN_EXPORTS: Mutex<Option<&'static MainExports>> = Mutex::new(None);

/// The main executable, as something a `dlopen`ed image can bind against.
///
/// dyld resolves a newly loaded image against **everything already loaded**, the
/// main executable included, and a plugin that calls back into its host depends
/// on it. `libYoYoGamepad.dylib` is exactly that: DELTARUNE `dlopen`s it, and it
/// imports `GMGamePad`'s methods — ninety of them — straight out of
/// `Mac_Runner`, which exports them for the purpose.
///
/// Without this the plugin links, loads, runs its initializers, and stops at its
/// first call back into the executable with "unsatisfied import".
///
/// The reference is spelled **flat** (`<flat-namespace>/__ZN9GMGamePad…`), which
/// is why [`SymbolProvider::provides`] is not what answers here — that predicate
/// is consulted only for a two-level reference. It is still implemented
/// faithfully, so an image naming the executable explicitly is answered too and
/// one naming `libSystem` is not.
struct MainExports {
    exports: ExportTable,
    slide: u64,
    /// The path the executable was loaded from, and its last component, which
    /// are the two spellings a two-level reference could use.
    path: String,
}

impl machish::resolve::SymbolProvider for MainExports {
    fn provides(&self, install_name: &str) -> bool {
        install_name == self.path
            || Some(install_name) == self.path.rsplit('/').next()
            || install_name == "@executable_path"
    }

    fn resolve(&self, sym: &machish::fixups::SymbolRef<'_>) -> Option<machish::resolve::Resolved> {
        let export = self.exports.get(sym.name)?;
        // A re-export forwards into another library and has no address of its
        // own; answering zero would bind the caller to nothing and report
        // success.
        let addr = export.address(self.slide)?;
        Some(machish::resolve::Resolved {
            addr,
            kind: match export.kind {
                machish::exports::ExportKind::ThreadLocal => {
                    machish::resolve::SymbolKind::ThreadLocal
                }
                _ => machish::resolve::SymbolKind::Function,
            },
            tag: 0,
        })
    }
}

/// The main executable's exports, parsed on first `dlopen` and kept.
///
/// Lazy rather than built at startup because a program that never calls
/// `dlopen` should not pay to re-read and parse its own executable.
fn main_exports() -> Option<&'static MainExports> {
    let mut slot = MAIN_EXPORTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = *slot {
        return Some(existing);
    }
    let (path, slide) = EXECUTABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let bytes = std::fs::read(&path).ok()?;
    let object = MachObject::parse_any(&bytes, X86_64).ok()?;
    let exports = ExportTable::parse(&object).ok()?;
    let leaked: &'static MainExports = Box::leak(Box::new(MainExports {
        exports,
        slide,
        path,
    }));
    *slot = Some(leaked);
    Some(leaked)
}

/// The message `dlerror` will report, and the buffer it last handed out.
///
/// Two slots because `dlerror` returns a `char *` the guest may hold across the
/// call that clears the error. Keeping the previous string alive until the next
/// `dlerror` is what the specification's "valid until the next call" means.
static ERROR: Mutex<Option<CString>> = Mutex::new(None);
static REPORTED: Mutex<Option<CString>> = Mutex::new(None);

/// A token for "the main executable and everything already loaded".
///
/// `dlopen(NULL, ...)` returns this, and `dlsym` treats it — and every negative
/// pseudo-handle — as a global search. The address of a `static` rather than a
/// small integer so it can never collide with an image base.
static GLOBAL_TOKEN: u8 = 0;

fn global_handle() -> u64 {
    &GLOBAL_TOKEN as *const u8 as usize as u64
}

/// Record where the guest executable came from, and where it landed.
pub fn set_executable_path(path: &str, slide: u64) {
    let canonical = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    *EXECUTABLE.lock().unwrap_or_else(|e| e.into_inner()) = Some((canonical, slide));
}

/// The path presented to guest process-introspection calls.
pub fn executable_path() -> Option<String> {
    EXECUTABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(path, _)| path.clone())
}

/// How many images are loaded, and the address range they span.
///
/// Read without the lock, and that is the entire point. These lookups sit on the
/// hot path — [`crate::Program::import_at`] and `import_by_target` are consulted
/// at *every* import exit, and `is_guest_address` at every one that resolved
/// somewhere — so what they cost when the answer is "no" is what the runtime
/// costs. The first version of this cloned the image vector under a mutex per
/// call, and a `sample` of DELTARUNE's game loop found the process spending most
/// of its time in `malloc` underneath `import_by_target`.
///
/// A program that never calls `dlopen` now pays one relaxed atomic load, and a
/// program that has pays that plus a range check before any lock is taken.
/// Relaxed is sufficient: a stale zero can only be read by a thread that has not
/// yet synchronised with the `dlopen` that stored it, and that thread cannot
/// hold an address from the image it has not seen.
static IMAGE_COUNT: AtomicUsize = AtomicUsize::new(0);
static REGION_LO: AtomicU64 = AtomicU64::new(u64::MAX);
static REGION_HI: AtomicU64 = AtomicU64::new(0);

/// Search the loaded images without copying the list.
///
/// Every `Image` is `&'static` — leaked at load, never freed — so a reference
/// found inside the lock outlives it, and the guard is dropped before the caller
/// does anything with the answer.
/// Every loaded image, copied out.
///
/// For `dlsym`, which searches them all and is called a handful of times in a
/// process's life. [`find`] is what the hot paths use.
fn snapshot() -> Vec<&'static Image> {
    let view = VIEW.load(Ordering::Acquire);
    if view.is_null() {
        return Vec::new();
    }
    // SAFETY: as in `find`.
    unsafe { *view }.to_vec()
}

fn find<R>(mut f: impl FnMut(&'static Image) -> Option<R>) -> Option<R> {
    let view = VIEW.load(Ordering::Acquire);
    if view.is_null() {
        return None;
    }
    // SAFETY: non-null only after a `dlopen` stored a leaked slice, which is
    // never freed and never mutated. See the note on `VIEW`.
    let images: &'static [&'static Image] = unsafe { *view };
    images.iter().copied().find_map(&mut f)
}

/// The import registered at `va` in any dynamically loaded image.
///
/// The widening described in the module note: what [`crate::Program::import_at`]
/// falls back to, so a call into a stub of an image the lifter never saw is
/// serviced rather than decoded.
pub fn import_at(va: u64) -> Option<&'static ImportSite> {
    // A slot or stub of a loaded image is *inside* that image, so an address
    // outside every dlopened mapping cannot be one. This is the question the run
    // loop asks before entering any region, which makes it the hottest of the
    // three.
    if va < REGION_LO.load(Ordering::Relaxed) || va >= REGION_HI.load(Ordering::Relaxed) {
        return None;
    }
    find(|i| i.imports.get(&va))
}

/// The import of any dynamically loaded image that *resolved to* `host`.
pub fn import_by_target(host: u64) -> Option<&'static ImportSite> {
    find(|i| i.imports_by_target.get(&host))
}

/// What `symbol` resolved to in any dynamically loaded image.
///
/// [`Program::resolved_symbol`](crate::Program::resolved_symbol) widened the
/// same way [`import_at`] is, and for a caller with the same need: an answer
/// read out of the *transcript* rather than looked up again. The case that
/// forces it is `operator new` — a library the guest `dlopen`ed imports it, the
/// main executable defines it, and so it appears in no import table but the
/// loaded image's. See [`crate::shims::cxxstring`].
pub fn resolved_symbol(symbol: &str) -> Option<u64> {
    find(|i| {
        i.imports
            .values()
            .find(|s| !s.is_stub && s.symbol == symbol && s.resolved != 0)
            .map(|s| s.resolved)
    })
}

/// Whether `va` lies inside a dynamically loaded image.
pub fn contains(va: u64) -> bool {
    // A host address is nowhere near a guest mapping, and this is the question
    // asked most often and answered "no" almost every time. Rejecting it on two
    // atomic loads keeps the lock off the path an ordinary import takes.
    if va < REGION_LO.load(Ordering::Relaxed) || va >= REGION_HI.load(Ordering::Relaxed) {
        return false;
    }
    find(|i| region_of(i, va).map(|_| ())).is_some()
}

/// The region to lift in order to execute `va`, if it is in a loaded image.
pub fn lift_region(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    find(|image| {
        let (base, bytes) = region_of(image, va)?;
        let function = image.functions.iter().find(|f| f.contains(&va)).cloned();
        let region = crate::region::region_for(bytes, base, va, function)?;
        let start = usize::try_from(region.range.start - base).ok()?;
        let end = usize::try_from(region.range.end - base)
            .ok()?
            .min(bytes.len());
        Some((region, bytes.get(start..end)?.to_vec()))
    })
}

fn region_of(image: &'static Image, va: u64) -> Option<(u64, &'static [u8])> {
    image
        .regions
        .iter()
        .find(|(base, bytes)| va >= *base && va < base + bytes.len() as u64)
        .copied()
}

/// `va` as `image+offset`, for an image the guest loaded. See
/// [`Program::locate`](crate::Program::locate).
pub fn locate(va: u64) -> Option<String> {
    find(|image| {
        for report in &image.report.images {
            for seg in &report.segments {
                if va >= seg.vmaddr && va < seg.vmaddr.wrapping_add(seg.vmsize) {
                    let name = report.name.rsplit('/').next().unwrap_or(&report.name);
                    return Some(format!("{name}+{:#x}", va.wrapping_sub(report.slide)));
                }
            }
        }
        None
    })
}

/// How many images the guest has loaded.
pub fn loaded() -> usize {
    IMAGE_COUNT.load(Ordering::Acquire)
}

// ------------------------------------------------------------- loading ------

/// Load `path` as a guest image, or return the handle it already has.
///
/// Idempotent by resolved path, which is what makes a program that `dlopen`s
/// the same library from two places get one image and one set of initializers.
pub fn open(path: &str) -> std::result::Result<u64, String> {
    let resolved = resolve_path(path).ok_or_else(|| format!("no such file: {path}"))?;

    if let Some(handle) = find(|i| (i.path == resolved).then_some(i.handle)) {
        return Ok(handle);
    }

    let bytes = std::fs::read(&resolved).map_err(|e| format!("{resolved}: {e}"))?;
    // `parse_any` narrows a fat file to the x86-64 slice, and fails when there
    // is none — which is the case a *native* arm64 dylib takes. That is not
    // something this path can load: its code is already this machine's, and
    // calling into it means describing every entry point the guest will reach.
    // Refused by name rather than half-loaded.
    let object = MachObject::parse_any(&bytes, X86_64)
        .map_err(|e| format!("{resolved}: not a loadable x86-64 Mach-O: {e}"))?;

    let mut memory = MmapSpace::new();
    // Vendored libc++ ahead of the filesystem; see [`crate::cxxlib`].
    let mut source = crate::cxxlib::source();
    let hosts = HostProviders::new();
    // The main executable ahead of everything: dyld resolves a newly loaded
    // image against what is *already loaded*, and a plugin's host is the first
    // thing that was. See [`MainExports`].
    let main = main_exports();
    let extra: Vec<&dyn machish::resolve::SymbolProvider> = main
        .map(|m| m as &dyn machish::resolve::SymbolProvider)
        .into_iter()
        .collect();
    let report = Loader::new(X86_64)
        .link_with(
            &bytes,
            &resolved,
            &mut memory,
            &mut source,
            &hosts.chain(&extra),
        )
        .map_err(|e| format!("{resolved}: {e}"))?;

    // Say what did not resolve, here, rather than letting the guest find out by
    // calling it. A `dlopen`ed image links against a chain assembled at run time
    // — the host, the main executable, images loaded before it — and an
    // unsatisfied symbol is a fact about that chain, which is exactly the thing
    // no later diagnostic can reconstruct.
    let unresolved: Vec<&str> = report
        .images
        .iter()
        .flat_map(|i| i.missing.iter())
        .filter(|m| !m.weak_import)
        .map(|m| m.symbol.as_str())
        .collect();
    if !unresolved.is_empty() {
        eprintln!(
            "rosetta3: dlopen({resolved}): {} unresolved: {}",
            unresolved.len(),
            unresolved
                .iter()
                .take(12)
                .copied()
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let root = report
        .root()
        .ok_or_else(|| format!("{resolved}: linked to no images"))?;
    let (base, slide) = (root.base, root.slide);

    let functions = match object.section("__TEXT", "__text") {
        Some(text) => machish::image::function_ranges(&object, text.addr, text.addr + text.size)
            .map_err(|e| format!("{resolved}: {e}"))?
            .into_iter()
            .map(|r| r.start.wrapping_add(slide)..r.end.wrapping_add(slide))
            .collect(),
        // No `__text` is not a refusal here the way it is for a program: a
        // resource-only bundle is a legal thing to `dlopen`, and a stripped
        // image simply has no hints to offer [`lift_region`].
        None => Vec::new(),
    };

    let exports = ExportTable::parse(&object).map_err(|e| format!("{resolved}: {e}"))?;
    // Everything mapped: this image, the ones loaded before it, and the program
    // that started it all. See [`collect_imports`] for why the distinction is
    // load-bearing.
    let vm = crate::thunk::current();
    let (imports, imports_by_target) = collect_imports(&report, |va| {
        memory.region_at(va).is_some()
            || contains(va)
            || vm.is_some_and(|vm| vm.program.is_guest_address(va))
    });

    // The space is leaked first so its regions really are `'static`; the image
    // then holds both the borrow and the thing borrowed from, which is legal
    // only because the space can no longer move.
    let memory: &'static SharedSpace = Box::leak(Box::new(SharedSpace(memory)));
    let leaked: &'static Image = Box::leak(Box::new(Image {
        path: resolved.clone(),
        handle: base,
        report,
        regions: memory.0.regions(),
        imports,
        imports_by_target,
        functions,
        exports,
        slide,
        _memory: memory,
    }));

    // Published *before* the initializers run: an initializer is guest code in
    // this image, and lifting it needs the image to be findable.
    {
        let mut images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
        images.push(leaked);
        for &(base, bytes) in &leaked.regions {
            REGION_LO.fetch_min(base, Ordering::Relaxed);
            REGION_HI.fetch_max(base + bytes.len() as u64, Ordering::Relaxed);
        }
        // The extents go up before the view does, so a reader that sees the
        // image also sees a range that contains it.
        let published: &'static [&'static Image] = images.clone().leak();
        let cell: &'static mut &'static [&'static Image] = Box::leak(Box::new(published));
        VIEW.store(cell as *mut _, Ordering::Release);
        IMAGE_COUNT.store(images.len(), Ordering::Release);
    }

    // A loaded image's thread-locals are its own, and nothing else registers
    // them: [`crate::tls::register`] runs over the *program's* images, so a
    // `_Thread_local` in a `dlopen`ed image had no descriptor in the registry and
    // `__tlv_bootstrap` answered null for it — correctly, by its own rule, and
    // fatally, because the guest dereferences what it is handed. The registry is
    // keyed by descriptor address and append-only precisely so that an image
    // mapped later can join it.
    crate::tls::register(&leaked.report);

    // Before any of this image's code runs, and after it is findable: rewrite
    // its `__objc_selrefs` slots from string pointers into the host runtime's
    // own selectors, which is what dyld's `libobjc` does when it maps an image.
    // An initializer that sends a message — `steamclient.dylib` has one that
    // asks `+[NSProcessInfo processInfo]` — loads a selector out of one of those
    // slots on the way in, so the rewrite has to happen first or the message is
    // sent with a `char *`. See [`crate::objc::unique_loaded_selectors`] for what
    // this deliberately does *not* do.
    if let Some(vm) = crate::thunk::current() {
        crate::objc::unique_loaded_selectors(vm, &leaked.report)
            .map_err(|e| format!("{resolved}: objc selectors: {e}"))?;
    }

    run_initializers(leaked)?;
    Ok(leaked.handle)
}

/// Every address a call site in `report` might name, both directions.
///
/// The same two maps [`crate::Program`] builds, built the same way and for the
/// same reasons — including the unsatisfied imports, which are registered so a
/// call to one is a named symbol rather than `no guest code at 0x0`.
fn collect_imports(
    report: &LinkReport,
    is_guest: impl Fn(u64) -> bool,
) -> (BTreeMap<u64, ImportSite>, BTreeMap<u64, ImportSite>) {
    let mut imports: BTreeMap<u64, ImportSite> = BTreeMap::new();
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
    let mut by_target: BTreeMap<u64, ImportSite> = BTreeMap::new();
    for site in by_symbol {
        // A site that resolved into the **guest** must never appear here, and
        // the reason is a two-billion-iteration livelock rather than a nicety.
        //
        // This map answers "the guest loaded a pointer and called it", so the
        // run loop consults it for every address it is about to *enter*. When
        // an import resolved to a guest function, that function's own address
        // is the key — so entering it looks like calling the import, which
        // transfers to it, which enters it again, with `rsp` never moving.
        // `libYoYoGamepad.dylib` importing `GMGamePad::SetGamePadCount` out of
        // `Mac_Runner` span there at 100% CPU with nothing to report.
        //
        // Nothing is lost by leaving it out: a guest address is directly
        // executable, so there is no interception to arrange. Only the call
        // *site* — the slot and the stub, in `imports` — needs one.
        if site.resolved != 0 && !is_guest(site.resolved) {
            by_target
                .entry(site.resolved)
                .or_insert_with(|| site.clone());
        }
    }
    (imports, by_target)
}

/// Run the image's constructors, as dyld does before `dlopen` returns.
///
/// Reverse load order, matching [`crate::startup::run_initializers`]:
/// dependencies before dependents. The arguments are the guest's own
/// `(argc, argv, envp, apple)`, which a constructor may legally take.
fn run_initializers(image: &'static Image) -> std::result::Result<(), String> {
    let Some(vm) = crate::thunk::current() else {
        return Err("no VM is running on this thread".to_string());
    };
    let (argc, argv, envp, apple) = vm.main_args();
    let initializers: Vec<u64> = image
        .report
        .images
        .iter()
        .rev()
        .flat_map(|i| i.initializers.iter().copied())
        .collect();
    for init in initializers {
        vm.call(init, &[argc, argv, envp, apple])
            .map_err(|e| format!("initializer at {init:#x}: {e}"))?;
    }
    Ok(())
}

/// Where a `dlopen` argument actually points.
///
/// The `@`-prefixed forms are Mach-O's own and are expanded by [`machish`]. A
/// bare or relative name is tried as given first — that is what the guest asked
/// for and what a real `dlopen` would try — and then beside the executable and
/// in the two places a `.app` puts a bundled dylib, because a runner launched
/// from anywhere still expects to find its own library.
fn resolve_path(path: &str) -> Option<String> {
    let executable = EXECUTABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .map(|(path, _)| path);
    let loader_dir = executable.as_deref().map(machish::loader::loader_directory);

    let mut candidates: Vec<String> = Vec::new();
    if path.starts_with('@') {
        let rpaths = executable.as_deref().map(rpaths_of).unwrap_or_default();
        let ctx = PathContext {
            executable: executable.as_deref(),
            loader: executable.as_deref(),
            rpaths: &rpaths,
        };
        candidates.extend(machish::install_name::expand(path, &ctx));
    } else {
        candidates.push(path.to_string());
        if !path.starts_with('/')
            && let Some(dir) = loader_dir
        {
            let name = path.rsplit('/').next().unwrap_or(path);
            candidates.push(format!("{dir}/{name}"));
            candidates.push(format!("{dir}/../Frameworks/{name}"));
            candidates.push(format!("{dir}/../Resources/{name}"));
        }
    }

    candidates
        .into_iter()
        .filter_map(|c| std::fs::canonicalize(&c).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .next()
}

/// The `LC_RPATH` entries of the image at `path`, for `@rpath` expansion.
///
/// Read from the file rather than kept on the [`crate::Program`], because this
/// is the only caller and a `dlopen` is already doing file I/O.
fn rpaths_of(path: &str) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(object) = MachObject::parse_any(&bytes, X86_64) else {
        return Vec::new();
    };
    object.rpaths().map(|r| r.to_string()).collect()
}

// --------------------------------------------------------------- dlsym ------

/// The address `name` has in `handle`, as `dlsym` reports it.
///
/// A guest address for a symbol one of our images defines, and the *host's*
/// answer otherwise — a program that `dlsym`s `"malloc"` out of the global
/// handle is asking about libSystem, and libSystem here is the host's.
fn lookup(handle: u64, name: &str) -> Option<u64> {
    // `dlsym` takes the C identifier; a Mach-O export carries the underscore.
    let mangled = format!("_{name}");
    let scoped: Vec<&'static Image> = match snapshot().into_iter().find(|i| i.handle == handle) {
        Some(image) => vec![image],
        // Every pseudo-handle — `RTLD_DEFAULT`, `RTLD_SELF`, `RTLD_NEXT`, and
        // the token `dlopen(NULL)` returns — is treated as a global search.
        // Distinguishing them needs a caller's image, which a shim standing
        // outside the guest's frame does not have.
        None => snapshot(),
    };
    for image in scoped {
        if let Some(export) = image.exports.get(&mangled)
            && let Some(addr) = export.address(image.slide)
        {
            return Some(addr);
        }
    }
    host_symbol(name)
}

/// `dlsym` against the host process, for a symbol no guest image defines.
///
/// Also how [`crate::shims`] finds a host entry point without `#[link]`ing the
/// library that exports it — see the note in [`crate::shims::audiounit`] about
/// what linking one does to the process the guest starts in.
pub(crate) fn host_symbol(name: &str) -> Option<u64> {
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c` outlives the call.
    // `dlsym` returns null for a name it does not know, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    match found.is_null() {
        true => None,
        false => Some(found as usize as u64),
    }
}

// --------------------------------------------------------------- shims ------

fn set_error(message: String) {
    *ERROR.lock().unwrap_or_else(|e| e.into_inner()) = CString::new(message).ok();
}

fn clear_error() {
    *ERROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<crate::jmpbuf::Shim> {
    matches!(
        name,
        "dlopen" | "dlsym" | "dlclose" | "dlerror" | "dladdr"
    )
    .then_some(serve)
}

fn serve(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    match symbol.strip_prefix('_').unwrap_or(symbol) {
        "dlopen" => dlopen(symbol, ctx, mem),
        "dlsym" => dlsym(symbol, ctx, mem),
        "dlclose" => {
            // No unloading; see the scope note. Zero is success, which is the
            // honest answer: the guest asked for the library to stop being
            // needed, and it has.
            clear_error();
            ctx.rax = 0;
            Ok(())
        }
        "dladdr" => dladdr(symbol, ctx, mem),
        "dlerror" => {
            let taken = ERROR.lock().unwrap_or_else(|e| e.into_inner()).take();
            let mut reported = REPORTED.lock().unwrap_or_else(|e| e.into_inner());
            *reported = taken;
            ctx.rax = reported.as_ref().map_or(0, |s| s.as_ptr() as usize as u64);
            Ok(())
        }
        other => Err(AbiError::NoShim {
            symbol: other.to_string(),
        }),
    }
}

fn dlopen(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    if ctx.rdi == 0 {
        clear_error();
        ctx.rax = global_handle();
        return Ok(());
    }
    let path = mem.read_cstr(ctx.rdi, symbol)?;
    let path = String::from_utf8_lossy(&path).into_owned();
    match open(&path) {
        Ok(handle) => {
            clear_error();
            ctx.rax = handle;
        }
        Err(reason) => {
            set_error(format!("dlopen({path}): {reason}"));
            ctx.rax = 0;
        }
    }
    Ok(())
}

fn dlsym(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let name = mem.read_cstr(ctx.rsi, symbol)?;
    let name = String::from_utf8_lossy(&name).into_owned();
    match lookup(ctx.rdi, &name) {
        Some(addr) => {
            clear_error();
            ctx.rax = addr;
        }
        None => {
            set_error(format!("dlsym({name}): symbol not found"));
            ctx.rax = 0;
        }
    }
    Ok(())
}

/// `int dladdr(const void *addr, Dl_info *info)`
///
/// The host's cannot answer this one at all for the addresses that matter. A
/// guest image is mapped by [`open`], not by dyld, so the host's loader has
/// never heard of it: asked about an address inside `steamclient.dylib` it
/// reports failure, or — worse — reports whichever *host* image happens to
/// surround that address, and the caller believes it.
///
/// So the registry answers for anything it recognises and the host answers for
/// everything else. `Dl_info` is four pointers, 32 bytes, laid out identically
/// on both architectures, so the structure is filled in guest memory in place.
///
/// `dli_sname`/`dli_saddr` are the nearest export at or below `addr`, which is
/// what `dladdr` documents them to be, and both are left null when the image
/// exports nothing below it — a legal answer that Darwin's own `dladdr` gives
/// for a static function. `dli_fname` points at the *guest image's* path string,
/// which this module keeps for the process's lifetime; that is the field Steam's
/// crash handler and `Sys_LoadModule` actually read.
fn dladdr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let (addr, info) = (ctx.rdi, ctx.rsi);
    let Some(found) = describe(addr) else {
        // Not ours. The host knows its own images, and this is exactly the
        // question it can answer.
        let Some(host) = host_symbol("dladdr") else {
            ctx.rax = 0;
            return Ok(());
        };
        // SAFETY: `dladdr` is `int (const void *, Dl_info *)`, and `info` is a
        // guest address — which is a real address in this process, since the
        // guest is identity-mapped. The host writes 32 bytes there, which is
        // what the guest allocated.
        let f: extern "C" fn(u64, u64) -> i32 =
            unsafe { core::mem::transmute::<u64, extern "C" fn(u64, u64) -> i32>(host) };
        crate::shims::set_int_return(ctx, f(addr, info));
        return Ok(());
    };
    if info == 0 {
        ctx.rax = 0;
        return Ok(());
    }
    let mut words = [0u8; 32];
    words[0..8].copy_from_slice(&found.fname.to_le_bytes());
    words[8..16].copy_from_slice(&found.fbase.to_le_bytes());
    words[16..24].copy_from_slice(&found.sname.to_le_bytes());
    words[24..32].copy_from_slice(&found.saddr.to_le_bytes());
    mem.write_bytes(info, &words, symbol)?;
    // Non-zero on success, unlike almost everything else in `<dlfcn.h>`.
    crate::shims::set_int_return(ctx, 1);
    Ok(())
}

/// What [`dladdr`] found: the four `Dl_info` fields, as host addresses.
struct Described {
    fname: u64,
    fbase: u64,
    sname: u64,
    saddr: u64,
}

/// `addr` as an image and a symbol, if this runtime is the one that mapped it.
///
/// Covers both kinds of guest image — the program itself and anything [`open`]
/// loaded — because a caller cannot tell them apart and neither should the
/// answer.
fn describe(addr: u64) -> Option<Described> {
    if addr == 0 {
        return None;
    }
    if let Some(found) = find(|i| {
        region_of(i, addr).map(|_| {
            let (sname, saddr) = nearest_export(&i.exports, i.slide, addr);
            Described {
                fname: path_cstr(&i.path),
                fbase: i.handle,
                sname,
                saddr,
            }
        })
    }) {
        return Some(found);
    }
    // The main executable, which is a guest image with no `Image` record.
    let vm = crate::thunk::current()?;
    if !vm.program.is_guest_address(addr) {
        return None;
    }
    let (path, slide) = EXECUTABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let (sname, saddr) = main_exports()
        .map(|m| nearest_export(&m.exports, slide, addr))
        .unwrap_or((0, 0));
    Some(Described {
        fname: path_cstr(&path),
        fbase: slide,
        sname,
        saddr,
    })
}

/// The export at or below `addr`, as `(name, address)`, or `(0, 0)`.
///
/// A linear scan: `ExportTable` is sorted by *name*, and this is the one
/// question that wants it sorted by address. It runs once per `dladdr` call,
/// which a program makes when it is reporting a crash or logging a module path,
/// so the scan is not on any path that repeats.
fn nearest_export(exports: &ExportTable, slide: u64, addr: u64) -> (u64, u64) {
    let mut best: Option<(&str, u64)> = None;
    for e in exports.entries() {
        let Some(at) = e.address(slide) else {
            continue;
        };
        if at <= addr && best.is_none_or(|(_, b)| at > b) {
            best = Some((e.name.as_str(), at));
        }
    }
    match best {
        Some((name, at)) => (symbol_cstr(name), at),
        None => (0, 0),
    }
}

/// A NUL-terminated copy of `text`, kept for the life of the process.
///
/// `dladdr` hands out pointers the caller may hold indefinitely — the
/// specification says they are valid until the image is unloaded, and nothing
/// here ever unloads — so these are interned rather than owned by the call.
fn interned(text: &str) -> u64 {
    static STRINGS: Mutex<Option<BTreeMap<String, &'static CString>>> = Mutex::new(None);
    let mut map = STRINGS.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(BTreeMap::new);
    if let Some(existing) = map.get(text) {
        return existing.as_ptr() as usize as u64;
    }
    let Ok(owned) = CString::new(text) else {
        return 0;
    };
    let leaked: &'static CString = Box::leak(Box::new(owned));
    map.insert(text.to_string(), leaked);
    leaked.as_ptr() as usize as u64
}

fn path_cstr(path: &str) -> u64 {
    interned(path)
}

/// The symbol name as C sees it: `dladdr` reports `main`, not `_main`.
fn symbol_cstr(name: &str) -> u64 {
    interned(name.strip_prefix('_').unwrap_or(name))
}

/// The `dl` family, claimed ahead of the host's.
///
/// The host exports all four and passing any of them through would be wrong for
/// the reason in the module note, so they are claimed here and the addresses are
/// never called. Each resolves to the host's own function anyway — an import
/// slot holding the honest value is one fewer thing to explain in a crash dump,
/// and it keeps the slot from being the null this project spells "unsatisfied".
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 dyld>");
    lib.flat(true);
    for name in ["dlopen", "dlsym", "dlclose", "dlerror", "dladdr"] {
        if let Some(addr) = host_symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    lib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_the_provider_claims_is_one_this_module_serves() {
        // Two lists that must agree: a symbol resolved here but not shimmed
        // would be *called* — the host's `dlopen`, handed a guest path, refusing
        // an x86-64 file for exactly the reason this module exists.
        use machish::resolve::SymbolProvider;
        let mut claimed = Vec::new();
        SymbolProvider::enumerate(&provider(), &mut |name, _| claimed.push(name.to_string()));
        assert!(!claimed.is_empty(), "the provider defines nothing");
        for name in claimed {
            let bare = name.strip_prefix('_').unwrap_or(&name);
            assert!(shim_for(bare).is_some(), "{name} is claimed and not served");
            assert!(
                crate::shims::is_shimmed(&name),
                "{name} would be promoted to an inlined host call"
            );
        }
    }

    #[test]
    fn the_global_handle_is_not_a_plausible_image_base() {
        // It is compared against `Image::handle`, which is a mapped base
        // address. A small integer would eventually be one.
        assert!(global_handle() > 0x1000);
    }

    #[test]
    fn a_missing_file_is_a_named_failure_rather_than_a_panic() {
        let err = open("/nonexistent/libnothing.dylib").unwrap_err();
        assert!(err.contains("libnothing.dylib"), "{err}");
    }

    #[test]
    fn a_native_arm64_image_is_refused_by_name() {
        // The mirror of the failure this module exists for: an image with no
        // x86-64 slice cannot be loaded as guest code, and saying so is better
        // than mapping half of it.
        let host_dylib = "/usr/lib/libSystem.B.dylib";
        if std::path::Path::new(host_dylib).exists() {
            let err = open(host_dylib).unwrap_err();
            assert!(
                err.contains("x86-64") || err.contains("no such file"),
                "{err}"
            );
        }
    }
}
