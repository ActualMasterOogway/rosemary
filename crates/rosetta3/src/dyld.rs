//! Guest-initiated `dlopen`/`dlsym`: loading an Intel image after the guest has started
//! running.

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
use crate::program::{AddrMap, HostProviders, ImportSite};

/// An image this runtime loaded on the guest's behalf.
pub struct Image {
    /// The path actually opened, after `@`-prefix expansion.
    pub path: String,
    /// What `dlopen` returned. The image's base address, which is unique in this
    /// process by construction and is the one value a crash dump can be read against.
    pub handle: u64,
    pub report: LinkReport,
    /// The mapping, kept alive for the process. Leaked separately from the `Image` so
    /// that [`regions`](Image::regions) can borrow from it.
    _memory: &'static SharedSpace,
    /// The mapped extents, for [`contains`] and [`lift_region`].
    regions: Vec<(u64, &'static [u8])>,
    /// Every address a call site in this image might name - both `slot_va` and
    /// `stub_va` - exactly as [`crate::Program`] keys its own.
    imports: BTreeMap<u64, ImportSite>,
    /// The same sites by the address they resolved to; see
    /// [`Program::import_by_target`](crate::Program::import_by_target), which also
    /// explains why this one is hashed and [`imports`](Image::imports) is not.
    imports_by_target: AddrMap<ImportSite>,
    /// Slid extents from `LC_FUNCTION_STARTS`, the hint [`lift_region`] uses.
    functions: Vec<Range<u64>>,
    exports: ExportTable,
    slide: u64,
}

/// An [`MmapSpace`] shared across the threads running the guest.
struct SharedSpace(MmapSpace);

// SAFETY: see the type note.
unsafe impl Sync for SharedSpace {}
unsafe impl Send for SharedSpace {}

/// Every image `dlopen` has produced, in load order.
static IMAGES: Mutex<Vec<&'static Image>> = Mutex::new(Vec::new());

/// The published, immutable view of [`IMAGES`]: a leaked slice, swapped whole.
static VIEW: AtomicPtr<&'static [&'static Image]> = AtomicPtr::new(core::ptr::null_mut());

/// The guest executable's path and slide.
static EXECUTABLE: Mutex<Option<(String, u64)>> = Mutex::new(None);

/// The main executable's export table, parsed once, on first use.
static MAIN_EXPORTS: Mutex<Option<&'static MainExports>> = Mutex::new(None);

/// The main executable, as something a `dlopen`ed image can bind against.
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

/// Every image the guest has already loaded, as something a newly loaded one can bind
/// against.
///
/// **Loading it again would be the wrong repair**, and is what a `DylibSource` that
/// guessed harder would do:
///
/// ## Matching
///
/// By **leaf name**, in both directions.
struct LoadedImages;

/// The file name of an install name or path, which is what both sides agree on.
fn leaf(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

impl machish::resolve::SymbolProvider for LoadedImages {
    fn provides(&self, install_name: &str) -> bool {
        let wanted = leaf(install_name);
        find(|i| (leaf(&i.path) == wanted).then_some(())).is_some()
    }

    fn resolve(&self, sym: &machish::fixups::SymbolRef<'_>) -> Option<machish::resolve::Resolved> {
        // A two-level reference is a claim about *which* image defines the
        // symbol, and honouring it is the whole reason `machish` carries the
        // library through to here.
        let named = sym.library.install_name().map(leaf);
        find(|image| {
            if named.is_some_and(|n| n != leaf(&image.path)) {
                return None;
            }
            let export = image.exports.get(sym.name)?;
            // A re-export has no address of its own; see [`MainExports`].
            let addr = export.address(image.slide)?;
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
static ERROR: Mutex<Option<CString>> = Mutex::new(None);
static REPORTED: Mutex<Option<CString>> = Mutex::new(None);

/// A token for "the main executable and everything already loaded".
static GLOBAL_TOKEN: u8 = 0;

fn global_handle() -> u64 {
    &GLOBAL_TOKEN as *const u8 as usize as u64
}

/// Return the guest image at a dyld image-list index.
fn indexed_image(index: u32) -> Option<(u64, u64, String)> {
    if index == 0 {
        let (path, slide) = EXECUTABLE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let header = MAIN_HEADER.load(Ordering::Acquire);
        return (header != 0).then_some((header, slide, path));
    }
    snapshot()
        .get(index as usize - 1)
        .map(|image| (image.handle, image.slide, image.path.clone()))
}

fn image_count() -> u32 {
    u32::from(MAIN_HEADER.load(Ordering::Acquire) != 0)
        .saturating_add(IMAGE_COUNT.load(Ordering::Acquire) as u32)
}

fn image_count_shim(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    ctx.rax = image_count() as u64;
    Ok(())
}

fn image_header_shim(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    ctx.rax = indexed_image(ctx.rdi as u32).map_or(0, |(header, _, _)| header);
    Ok(())
}

fn image_slide_shim(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    ctx.rax = indexed_image(ctx.rdi as u32).map_or(0, |(_, slide, _)| slide);
    Ok(())
}

fn image_name_shim(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    ctx.rax = indexed_image(ctx.rdi as u32).map_or(0, |(_, _, path)| path_cstr(&path));
    Ok(())
}

fn image_containing_shim(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    ctx.rax = guest_image_at(ctx.rdi).map_or(0, |(header, _)| header);
    Ok(())
}

/// The main executable's `mach_header` as the guest can address it.
static MAIN_HEADER: AtomicU64 = AtomicU64::new(0);

/// Record the main executable's mach header address.
pub fn set_executable_header(va: u64) {
    MAIN_HEADER.store(va, Ordering::Release);
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
/// A program that never calls `dlopen` now pays one relaxed atomic load, and a program
/// that has pays that plus a range check before any lock is taken.
static IMAGE_COUNT: AtomicUsize = AtomicUsize::new(0);
static REGION_LO: AtomicU64 = AtomicU64::new(u64::MAX);
static REGION_HI: AtomicU64 = AtomicU64::new(0);

static IMPORT_FILTER: AtomicPtr<&'static [u64]> = AtomicPtr::new(core::ptr::null_mut());

/// The same, over [`Image::imports_by_target`]'s keys.
static TARGET_FILTER: AtomicPtr<&'static [u64]> = AtomicPtr::new(core::ptr::null_mut());

/// Whether `filter` has heard of `va`. A yes may be a collision; a no is final.
/// Null - nothing published yet - answers yes, which costs a probe, not an answer.
fn heard_of(filter: &AtomicPtr<&'static [u64]>, va: u64) -> bool {
    let published = filter.load(Ordering::Acquire);
    if published.is_null() {
        return true;
    }
    // SAFETY: as `find` - leaked by `publish_filters`, never freed, never mutated.
    crate::program::filter_has(unsafe { *published }, va)
}

/// Rebuild both filters from `images`. Called under the [`IMAGES`] lock.
fn publish_filters(images: &[&'static Image]) {
    let publish = |filter: &AtomicPtr<&'static [u64]>, built: Box<[u64]>| {
        let leaked: &'static [u64] = Box::leak(built);
        filter.store(Box::leak(Box::new(leaked)) as *mut _, Ordering::Release);
    };
    let imports = images.iter().map(|i| i.imports.len()).sum();
    publish(
        &IMPORT_FILTER,
        crate::program::build_import_filter(
            images.iter().flat_map(|i| i.imports.keys().copied()),
            imports,
        ),
    );
    let targets = images.iter().map(|i| i.imports_by_target.len()).sum();
    publish(
        &TARGET_FILTER,
        crate::program::build_import_filter(
            images
                .iter()
                .flat_map(|i| i.imports_by_target.keys().copied()),
            targets,
        ),
    );
}

/// Search the loaded images without copying the list.
///
/// Every `Image` is `&'static` - leaked at load, never freed - so a reference found
/// inside the lock outlives it, and the guard is dropped before the caller does
/// anything with the answer.
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
pub fn import_at(va: u64) -> Option<&'static ImportSite> {
    // A slot or stub of a loaded image is *inside* that image, so an address
    // outside every dlopened mapping cannot be one.
    if va < REGION_LO.load(Ordering::Relaxed) || va >= REGION_HI.load(Ordering::Relaxed) {
        return None;
    }
    if !heard_of(&IMPORT_FILTER, va) {
        return None;
    }
    find(|i| i.imports.get(&va))
}

/// The import of any dynamically loaded image that *resolved to* `host`.
pub fn import_by_target(host: u64) -> Option<&'static ImportSite> {
    // The same two-compare rejection [`import_at`] makes, for a colder version
    // of the same problem.
    if IMAGE_COUNT.load(Ordering::Relaxed) == 0 || !heard_of(&TARGET_FILTER, host) {
        return native_symbol_at(host);
    }
    find(|i| i.imports_by_target.get(&host)).or_else(|| native_symbol_at(host))
}

/// What `symbol` resolved to in any dynamically loaded image.
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
    // asked most often and answered "no" almost every time.
    if va < REGION_LO.load(Ordering::Relaxed) || va >= REGION_HI.load(Ordering::Relaxed) {
        return false;
    }
    find(|i| region_of(i, va).map(|_| ())).is_some()
}

/// The recorded function starts inside `range`, for an image the *guest* loaded.
pub fn function_starts_in(range: std::ops::Range<u64>) -> Vec<u64> {
    // An address is in one image, so the first image with anything in `range` is
    // the answer - and `starts_within` finds it by binary search rather than by
    // scanning that image's function table. See
    // [`crate::program::Program::function_starts_in`].
    find(|image| {
        let starts: Vec<u64> =
            crate::program::starts_within(&image.functions, &range).collect();
        (!starts.is_empty()).then_some(starts)
    })
    .unwrap_or_default()
}

/// The region to lift in order to execute `va`, if it is in a loaded image.
pub fn lift_region(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    find(|image| {
        let (base, bytes) = region_of(image, va)?;
        // A loaded image, so not memory the guest writes; see [`crate::region`].
        let region = crate::region::region_for(bytes, base, va, false)?;
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


/// Load `path` as a guest image, or return the handle it already has.
pub fn open(path: &str) -> std::result::Result<u64, String> {
    let resolved = resolve_path(path).ok_or_else(|| format!("no such file: {path}"))?;

    if let Some(handle) = find(|i| (i.path == resolved).then_some(i.handle)) {
        return Ok(handle);
    }

    let bytes = std::fs::read(&resolved).map_err(|e| format!("{resolved}: {e}"))?;
    // `parse_any` narrows a fat file to the x86-64 slice, and fails when there
    // is none - which is the case a *native* arm64 dylib takes.
    let object = MachObject::parse_any(&bytes, X86_64)
        .map_err(|e| format!("{resolved}: not a loadable x86-64 Mach-O: {e}"))?;

    let mut memory = MmapSpace::new();
    // Vendored libc++ ahead of the filesystem; see [`crate::cxxlib`].
    let mut source = crate::cxxlib::source();
    let hosts = HostProviders::new();
    // Everything already loaded, which is what dyld resolves a new image
    // against.
    let main = main_exports();
    let loaded = LoadedImages;
    let mut extra: Vec<&dyn machish::resolve::SymbolProvider> = main
        .map(|m| m as &dyn machish::resolve::SymbolProvider)
        .into_iter()
        .collect();
    extra.push(&loaded as &dyn machish::resolve::SymbolProvider);
    let mut report = Loader::new(X86_64)
        .link_with(
            &bytes,
            &resolved,
            &mut memory,
            &mut source,
            &hosts.chain(&extra),
        )
        .map_err(|e| format!("{resolved}: {e}"))?;

    // The same repair the program's own link needs, for the same reason.
    crate::cxxlib::bind_unwinder_to_the_vendored_one(&mut report, &mut memory)
        .map_err(|e| format!("{resolved}: unwinder: {e}"))?;
    crate::cxxlib::bind_cxxabi_to_the_vendored_one(&mut report, &mut memory)
        .map_err(|e| format!("{resolved}: cxxabi: {e}"))?;

    // Say what did not resolve, here, rather than letting the guest find out by
    // calling it.
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
                .take(20)
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
        // No `__text` is not a refusal here the way it is for a program.
        None => Vec::new(),
    };

    let exports = ExportTable::parse(&object).map_err(|e| format!("{resolved}: {e}"))?;
    // Everything mapped: this image, the ones loaded before it, and the program
    // that started it all.
    let vm = crate::thunk::current();
    let is_guest = |va: u64| {
        memory.region_at(va).is_some()
            || contains(va)
            || vm.is_some_and(|vm| vm.program.is_guest_address(va))
    };
    let (imports, imports_by_target) = collect_imports(&report, is_guest);

    // The same substitution the program's own slots get - see
    // [`crate::trampoline`] - and a `dlopen`ed plugin is where it pays most.
    let eligible: Vec<(u64, ImportSite)> = imports
        .iter()
        .filter(|(_, site)| !site.is_stub && !is_guest(site.resolved))
        .map(|(&va, site)| (va, site.clone()))
        .collect();
    crate::trampoline::install(
        eligible.iter().map(|(va, site)| (*va, site)),
        &|_| false,
        &mut memory,
    );

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
        // The extents and the filters go up before the view does, so a reader
        // that sees the image also sees a range and a filter that contain it.
        publish_filters(&images);
        let published: &'static [&'static Image] = images.clone().leak();
        let cell: &'static mut &'static [&'static Image] = Box::leak(Box::new(published));
        VIEW.store(cell as *mut _, Ordering::Release);
        IMAGE_COUNT.store(images.len(), Ordering::Release);
        // A whole image's worth of addresses that meant nothing a moment ago,
        // imports among them. See [`crate::dispatch::invalidate`].
        crate::dispatch::invalidate();
    }

    // A loaded image's thread-locals are its own, and nothing else registers
    // them.
    crate::tls::register(&leaked.report);

    // Before any of this image's code runs, and after it is findable.
    let vm = crate::thunk::current();
    // Realizing a loaded image's classes needs a host Objective-C runtime to realize
    // them *into*, which is a thing only macOS has. An image that has classes and is
    // loaded here has already been refused at its imports.
    #[cfg(target_os = "macos")]
    if let Some(vm) = vm {
        match vm.objc() {
            // Registered classes: extend that runtime, selectors included.
            Some(_) => {
                vm.register_loaded_objc(&leaked.report)
                    .map_err(|e| format!("{resolved}: objc: {e}"))?;
            }
            // A program with no Objective-C of its own never registered, so
            // there is no runtime to extend and no class of the program's a
            // loaded image could subclass. The selectors are still its own.
            None => {
                crate::objc::unique_loaded_selectors(vm, &leaked.report)
                    .map_err(|e| format!("{resolved}: objc selectors: {e}"))?;
            }
        }
    }

    // Every `_dyld_register_func_for_add_image` callback, before this image's
    // own code runs. See [`announce`].
    announce(leaked.handle, leaked.slide);

    // Now the image's own code, in dyld's order.
    #[cfg(target_os = "macos")]
    if let Some(vm) = vm {
        vm.run_loaded_load_methods(&leaked.report)
            .map_err(|e| format!("{resolved}: objc +load: {e}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = vm;

    run_initializers(leaked)?;
    Ok(leaked.handle)
}

/// Every address a call site in `report` might name, both directions.
fn collect_imports(
    report: &LinkReport,
    is_guest: impl Fn(u64) -> bool,
) -> (BTreeMap<u64, ImportSite>, AddrMap<ImportSite>) {
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
    let mut by_target: AddrMap<ImportSite> = AddrMap::default();
    for site in by_symbol {
        // A site that resolved into the **guest** must never appear here, and
        // the reason is a two-billion-iteration livelock rather than a nicety.
        if site.resolved != 0 && !is_guest(site.resolved) {
            by_target
                .entry(site.resolved)
                .or_insert_with(|| site.clone());
        }
    }
    (imports, by_target)
}

/// Run the image's constructors, as dyld does before `dlopen` returns.
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
fn search_path(var: &str, name: &str) -> Vec<String> {
    let Some(value) = std::env::var_os(var) else {
        return Vec::new();
    };
    value
        .to_string_lossy()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| format!("{}/{name}", dir.trim_end_matches('/')))
        .collect()
}

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
        // `DYLD_LIBRARY_PATH` first, and `DYLD_FALLBACK_LIBRARY_PATH` after the
        // ordinary candidates - the order the host's own loader uses.
        let name = path.rsplit('/').next().unwrap_or(path);
        candidates.extend(search_path("DYLD_LIBRARY_PATH", name));
        candidates.push(path.to_string());
        if !path.starts_with('/')
            && let Some(dir) = loader_dir
        {
            candidates.push(format!("{dir}/{name}"));
            candidates.push(format!("{dir}/../Frameworks/{name}"));
            candidates.push(format!("{dir}/../Resources/{name}"));
        }
        // The executable's own `LC_RPATH` directories, searched by leaf name.
        if let Some(exe) = executable.as_deref() {
            let rpaths = rpaths_of(exe);
            let ctx = PathContext {
                executable: Some(exe),
                loader: Some(exe),
                rpaths: &rpaths,
            };
            candidates.extend(machish::install_name::expand(
                &format!("@rpath/{name}"),
                &ctx,
            ));
        }
        candidates.extend(search_path("DYLD_FALLBACK_LIBRARY_PATH", name));
    }

    candidates
        .into_iter()
        .filter_map(|c| std::fs::canonicalize(&c).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .next()
}

/// The `LC_RPATH` entries of the image at `path`, for `@rpath` expansion.
fn rpaths_of(path: &str) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(object) = MachObject::parse_any(&bytes, X86_64) else {
        return Vec::new();
    };
    object.rpaths().map(|r| r.to_string()).collect()
}


/// The address `name` has in `handle`, as `dlsym` reports it.
fn lookup(handle: u64, name: &str) -> Option<u64> {
    // A handle from the host's loader.
    if let Some(path) = native_handle(handle) {
        let c = CString::new(name).ok()?;
        // SAFETY: a handle this module obtained from `dlopen` and never closed,
        // and a null-terminated name.
        let found = unsafe { libc::dlsym(handle as usize as *mut libc::c_void, c.as_ptr()) };
        if found.is_null() {
            return None;
        }
        return Some(remember_native_symbol(
            found as usize as u64,
            name,
            Some(path),
        ));
    }

    // `dlsym` takes the C identifier; a Mach-O export carries the underscore.
    let mangled = format!("_{name}");
    let scoped: Vec<&'static Image> = match snapshot().into_iter().find(|i| i.handle == handle) {
        Some(image) => vec![image],
        // Every pseudo-handle - `RTLD_DEFAULT`, `RTLD_SELF`, `RTLD_NEXT`, and
        // the token `dlopen(NULL)` returns - is treated as a global search.
        None => snapshot(),
    };
    for image in scoped {
        if let Some(export) = image.exports.get(&mangled)
            && let Some(addr) = export.address(image.slide)
        {
            return Some(addr);
        }
    }
    // The **main executable's** own exports, which a global search must include
    // and which nothing above covers.
    if let Some(main) = main_exports()
        && let Some(export) = main.exports.get(&mangled)
        && let Some(addr) = export.address(main.slide)
    {
        return Some(addr);
    }

    // The global fallback, which is a *host* search.
    let addr = match crate::shims::openat::host_substitute(name) {
        Some(wrapper) => wrapper,
        None => crate::host::symbol(name)?,
    };
    Some(remember_native_symbol(addr, name, None))
}

/// Handles the *host's* `dlopen` returned, and the paths they were asked for.
static NATIVE: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

/// The import sites a native `dlsym` result has been given.
static NATIVE_SYMBOLS: Mutex<BTreeMap<u64, &'static ImportSite>> = Mutex::new(BTreeMap::new());

/// How many entries [`NATIVE_SYMBOLS`] has, read without the lock.
static NATIVE_SYMBOL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Open `path` with the host's loader, as a native arm64 library.
///
/// `RTLD_GLOBAL` and never closed, for the reason the native provider's own fallback
/// is: closing it would dangle every pointer already written into a guest slot.
fn open_native(path: &str) -> Option<u64> {
    let c = CString::new(path).ok()?;
    // SAFETY: a null-terminated path and the host's own loader. The handle is
    // kept for the process; see the note above on why it is never closed.
    let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return None;
    }
    let handle = handle as usize as u64;
    let mut native = NATIVE.lock().unwrap_or_else(|e| e.into_inner());
    if !native.iter().any(|(h, _)| *h == handle) {
        native.push((handle, path.to_string()));
    }
    Some(handle)
}

/// Whether `handle` came from [`open_native`], and the path it was opened as.
fn native_handle(handle: u64) -> Option<String> {
    NATIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(h, _)| *h == handle)
        .map(|(_, path)| path.clone())
}

/// Record a host address handed to the guest by `dlsym`, so it can be called.
fn remember_native_symbol(addr: u64, name: &str, library: Option<String>) -> u64 {
    let mut map = NATIVE_SYMBOLS.lock().unwrap_or_else(|e| e.into_inner());
    let before = map.len();
    let site = *map.entry(addr).or_insert_with(|| {
        Box::leak(Box::new(ImportSite {
            // The Mach-O spelling, because that is what the signature table is
            // keyed by - `dlsym` takes the C identifier and drops the underscore.
            symbol: format!("_{name}"),
            library,
            resolved: addr,
            tag: 0,
            is_stub: false,
        }))
    });
    if map.len() != before {
        // Publish the snapshot first, then the count, and both while the lock
        // is still held.
        let view: Vec<(u64, &'static ImportSite)> = map.iter().map(|(a, s)| (*a, *s)).collect();
        NATIVE_SYMBOL_VIEW.store(Box::leak(Box::new(view)), Ordering::Release);
        NATIVE_SYMBOL_COUNT.store(map.len(), Ordering::Release);
    }
    drop(map);

    // And the whole reason this passthrough is worth optimising at all.
    crate::trampoline::for_site(site).unwrap_or(addr)
}

/// The published, immutable half of [`NATIVE_SYMBOLS`]: a leaked, sorted slice of
/// `(address, site)`, swapped whole.
static NATIVE_SYMBOL_VIEW: AtomicPtr<Vec<(u64, &'static ImportSite)>> =
    AtomicPtr::new(core::ptr::null_mut());

/// The import site for a host address the guest got from `dlsym`, if any.
pub fn native_symbol_at(host: u64) -> Option<&'static ImportSite> {
    if NATIVE_SYMBOL_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let published = NATIVE_SYMBOL_VIEW.load(Ordering::Acquire);
    if published.is_null() {
        return None;
    }
    // SAFETY: leaked by `remember_native_symbol` and never freed, so the slice
    // stays valid for the life of the process - the same claim `VIEW` makes.
    //
    // A snapshot one `dlsym` behind can miss a site another thread has just
    // recorded, and unlike a thunk that is harmless: the caller falls through to
    // `UnknownIndirectCall`, which is what it would have got a microsecond
    // earlier anyway, and the guest cannot have the address yet - `dlsym` has
    // not returned it.
    let view = unsafe { &*published };
    // Sorted, so the two ends bound it: the run loop asks this of a *guest* pc on
    // every window miss, and a guest pc is nowhere near a host symbol.
    let (Some(first), Some(last)) = (view.first(), view.last()) else {
        return None;
    };
    if host < first.0 || host > last.0 {
        return None;
    }
    view.binary_search_by_key(&host, |(a, _)| *a)
        .ok()
        .map(|i| view[i].1)
}


/// The `add image` callbacks the guest has registered, as **thunks**.
static ADD_IMAGE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Tell one callback about one image.
///
/// The two arguments are dyld's: the image's `mach_header` and its slide.
fn notify(thunk: u64, header: u64, slide: u64) {
    if thunk == 0 || header == 0 {
        return;
    }
    if crate::shims::vmmap::tracing() {
        eprintln!("rosetta3: add-image notice: header {header:#x}, slide {slide:#x}");
    }
    // SAFETY: `thunk` is a native arm64 trampoline this runtime compiled for a
    // guest function of dyld's callback signature - two integer arguments, no
    // return - which is inside the shape [`crate::thunk`] can carry.
    let f: extern "C" fn(u64, u64) = unsafe { std::mem::transmute::<u64, _>(thunk) };
    f(header, slide);
}

/// Announce `image` to every callback registered so far.
fn announce(header: u64, slide: u64) {
    let callbacks = ADD_IMAGE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for thunk in callbacks {
        notify(thunk, header, slide);
    }
}

/// `void _dyld_register_func_for_add_image(void (*f)(const struct mach_header *,
/// intptr_t))`
///
/// ## Scope
///
/// The images this runtime *maps* are reported.
fn register_func_for_add_image(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    callbacks: &dyn crate::abi::Callbacks,
) -> Result<()> {
    let thunk = match ctx.rdi {
        0 => return Ok(()),
        va => callbacks.substitute(symbol, va)?,
    };

    // Registered before the retrospective notices go out, so an image loaded by
    // one of those notices - an initializer that `dlopen`s, which is exactly
    // what a plugin host does - is announced rather than missed.
    ADD_IMAGE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(thunk);

    let mut already: Vec<(u64, u64)> = Vec::new();
    if let Some((_, slide)) = EXECUTABLE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        already.push((MAIN_HEADER.load(Ordering::Acquire), slide));
    }
    already.extend(snapshot().iter().map(|i| (i.handle, i.slide)));
    for (header, slide) in already {
        notify(thunk, header, slide);
    }
    Ok(())
}

/// `void _dyld_register_func_for_remove_image(...)`
///
/// Recorded and never called, because nothing is ever unloaded - see the scope note on
/// `dlclose`. Accepting it is honest in a way refusing would not be.
fn register_func_for_remove_image(
    _symbol: &str,
    _ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    _callbacks: &dyn crate::abi::Callbacks,
) -> Result<()> {
    Ok(())
}

/// Whether `name` is one of this module's callback-taking shims.
///
pub fn shim_for_callbacks(name: &str) -> Option<crate::signal::Shim> {
    Some(match name {
        "_dyld_register_func_for_add_image" => register_func_for_add_image,
        "_dyld_register_func_for_remove_image" => register_func_for_remove_image,
        _ => return None,
    })
}


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
        "dlopen"
            | "dlsym"
            | "dlclose"
            | "dlerror"
            | "dladdr"
            | "_dyld_image_count"
            | "_dyld_get_image_header"
            | "_dyld_get_image_vmaddr_slide"
            | "_dyld_get_image_name"
            | "_dyld_image_containing_address"
            | "_dyld_find_unwind_sections"
    )
    .then_some(
        |symbol, ctx, mem| match symbol.strip_prefix('_').unwrap_or(symbol) {
            "dyld_image_count" => image_count_shim(symbol, ctx, mem),
            "dyld_get_image_header" => image_header_shim(symbol, ctx, mem),
            "dyld_get_image_vmaddr_slide" => image_slide_shim(symbol, ctx, mem),
            "dyld_get_image_name" => image_name_shim(symbol, ctx, mem),
            "dyld_image_containing_address" => image_containing_shim(symbol, ctx, mem),
            _ => serve(symbol, ctx, mem),
        },
    )
}

fn serve(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    match symbol.strip_prefix('_').unwrap_or(symbol) {
        "dlopen" => dlopen(symbol, ctx, mem),
        "dlsym" => dlsym(symbol, ctx, mem),
        "dlclose" => {
            // No unloading; see the scope note. Zero is success, which is the
            // honest answer.
            clear_error();
            ctx.rax = 0;
            Ok(())
        }
        "dladdr" => dladdr(symbol, ctx, mem),
        // Spelled with one fewer underscore than [`shim_for`] matches on.
        "dyld_find_unwind_sections" => find_unwind_sections(symbol, ctx, mem),
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
    if crate::shims::vmmap::tracing() {
        eprintln!("rosetta3: dlopen({path:?}, flags {:#x})", ctx.rsi as i32);
    }
    match open(&path) {
        Ok(handle) => {
            clear_error();
            ctx.rax = handle;
        }
        // No x86-64 Mach-O on disk under that name.
        Err(reason) => match open_native(&path) {
            Some(handle) => {
                clear_error();
                ctx.rax = handle;
            }
            None => {
                set_error(format!("dlopen({path}): {reason}"));
                ctx.rax = 0;
            }
        },
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
            let err = format!("dlsym({name}): symbol not found");
            eprintln!("symbol lookup failed: {err}");
            set_error(err);
            ctx.rax = 0;
        }
    }
    Ok(())
}

/// `int dladdr(const void *addr, Dl_info *info)`
///
/// The host's cannot answer this one at all for the addresses that matter.
fn dladdr(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let (addr, info) = (ctx.rdi, ctx.rsi);
    let Some(found) = describe(addr) else {
        // Not ours. The host knows its own images, and this is exactly the
        // question it can answer.
        let Some(host) = crate::host::symbol("dladdr") else {
            ctx.rax = 0;
            return Ok(());
        };
        // SAFETY: `dladdr` is `int (const void *, Dl_info *)`, and `info` is a
        // guest address - which is a real address in this process, since the
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

/// `bool _dyld_find_unwind_sections(void *addr, struct dyld_unwind_sections *)`
///
/// ## The structure, read out of the caller
///
/// `dyld_priv.h` is not in any SDK on this system, so the layout comes from the
/// vendored `libunwind` that calls it - the same method [`crate::abi::table`]'s
/// `KBGetLayoutType` entry uses, and the same one this project uses to decide which
/// bytes are code:
///
/// ```text
/// leaq -0x300(%rbp), %rsi   ; the structure
/// callq __dyld_find_unwind_sections
/// testb %al, %al            ; bool, in al
/// movq -0x300(%rbp), ...    ; +0x00  mach_header *
/// movq -0x2f8(%rbp), ...    ; +0x08  dwarf_section
/// movq -0x2f0(%rbp), ...    ; +0x10  dwarf_section_length
/// movq -0x2e8(%rbp), ...    ; +0x18  compact_unwind_section
/// movq -0x2e0(%rbp), ...    ; +0x20  compact_unwind_section_length
/// ```
///
/// ## Where the section bounds come from
///
/// The guest's own `mach_header`, walked by the **host's** libmacho.
fn find_unwind_sections(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let (addr, out) = (ctx.rdi, ctx.rsi);
    let Some((header, slide)) = guest_image_at(addr) else {
        // Not ours.
        let Some(host) = crate::host::symbol("_dyld_find_unwind_sections") else {
            crate::shims::set_int_return(ctx, 0);
            return Ok(());
        };
        // SAFETY: `bool (*)(void *, struct dyld_unwind_sections *)`, and `out`
        // is a guest address, which is a real address in this process. The host
        // writes forty bytes there, which is what the guest reserved.
        let f: extern "C" fn(u64, u64) -> bool =
            unsafe { core::mem::transmute::<u64, extern "C" fn(u64, u64) -> bool>(host) };
        crate::shims::set_int_return(ctx, i32::from(f(addr, out)));
        return Ok(());
    };
    if out == 0 {
        crate::shims::set_int_return(ctx, 0);
        return Ok(());
    }

    let mut words = [0u8; 40];
    words[0..8].copy_from_slice(&header.to_le_bytes());
    for (i, section) in ["__eh_frame", "__unwind_info"].into_iter().enumerate() {
        let (at, len) = section_in(header, section)
            .map_or((0, 0), |(addr, size)| (addr.wrapping_add(slide), size));
        let base = 8 + i * 16;
        words[base..base + 8].copy_from_slice(&at.to_le_bytes());
        words[base + 8..base + 16].copy_from_slice(&len.to_le_bytes());
    }
    mem.write_bytes(out, &words, symbol)?;
    // True even when both sections are absent, which is a real answer rather
    // than a failure.
    crate::shims::set_int_return(ctx, 1);
    Ok(())
}

/// `(mach_header VA, slide)` for the guest image containing `va`.
fn guest_image_at(va: u64) -> Option<(u64, u64)> {
    fn containing(report: &LinkReport, va: u64) -> Option<(u64, u64)> {
        report
            .images
            .iter()
            .find(|i| {
                i.segments
                    .iter()
                    .any(|s| va >= s.vmaddr && va < s.vmaddr.wrapping_add(s.vmsize))
            })
            .map(|i| (i.base, i.slide))
    }
    if let Some(found) = find(|i| containing(&i.report, va)) {
        return Some(found);
    }
    containing(&crate::thunk::current()?.program.report, va)
}

/// `(unslid addr, size)` of a section in the guest image at `header`.
fn section_in(header: u64, section: &str) -> Option<(u64, u64)> {
    let host = crate::host::symbol("getsectbynamefromheader_64")?;
    let (seg, sect) = (CString::new("__TEXT").ok()?, CString::new(section).ok()?);
    // SAFETY: `const struct section_64 *(*)(const struct mach_header_64 *,
    // const char *, const char *)`. `header` is a guest image's mapped header,
    // which is identity-mapped and therefore a real address here, and both
    // strings outlive the call.
    let f: extern "C" fn(u64, *const libc::c_char, *const libc::c_char) -> *const u8 =
        unsafe { core::mem::transmute::<u64, _>(host) };
    let found = f(header, seg.as_ptr(), sect.as_ptr());
    if found.is_null() {
        return None;
    }
    // `struct section_64`: two 16-byte names, then `addr` and `size`.
    // SAFETY: libmacho returned a `section_64` inside the guest's own mapped
    // load commands, so the thirty-two bytes at +32 are the image's.
    let (addr, size) = unsafe {
        (
            found.add(32).cast::<u64>().read_unaligned(),
            found.add(40).cast::<u64>().read_unaligned(),
        )
    };
    Some((addr, size))
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
/// Covers both kinds of guest image - the program itself and anything [`open`] loaded -
/// because a caller cannot tell them apart and neither should the answer.
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
/// The host exports all four and passing any of them through would be wrong for the
/// reason in the module note, so they are claimed here and the addresses are never
/// called.
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 dyld>");
    lib.flat(true);
    for name in [
        "dlopen",
        "dlsym",
        "dlclose",
        "dlerror",
        "dladdr",
        "_dyld_image_count",
        "_dyld_get_image_header",
        "_dyld_get_image_vmaddr_slide",
        "_dyld_get_image_name",
        "_dyld_image_containing_address",
        // Spelled with the underscore the C identifier already carries.
        "_dyld_register_func_for_add_image",
        "_dyld_register_func_for_remove_image",
        // On `machish`'s `_dyld_` denylist like the two above, and the reason
        // `tests/unwind.rs` has been red.
        "_dyld_find_unwind_sections",
    ] {
        if let Some(addr) = crate::host::symbol(name) {
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
        // Two lists that must agree.
        use machish::resolve::SymbolProvider;
        let mut claimed = Vec::new();
        SymbolProvider::enumerate(&provider(), &mut |name, _| claimed.push(name.to_string()));
        assert!(!claimed.is_empty(), "the provider defines nothing");
        for name in claimed {
            let bare = name.strip_prefix('_').unwrap_or(&name);
            assert!(
                shim_for(bare).is_some() || shim_for_callbacks(bare).is_some(),
                "{name} is claimed and not served"
            );
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
        // The mirror of the failure this module exists for.
        let host_dylib = "/usr/lib/libSystem.B.dylib";
        if std::path::Path::new(host_dylib).exists() {
            let err = open(host_dylib).unwrap_err();
            assert!(
                err.contains("x86-64") || err.contains("no such file"),
                "{err}"
            );
        }
    }

    #[test]
    fn guest_image_apis_are_shims_not_host_calls() {
        for name in [
            "__dyld_image_count",
            "__dyld_get_image_header",
            "__dyld_get_image_vmaddr_slide",
            "__dyld_get_image_name",
            "__dyld_image_containing_address",
        ] {
            assert!(
                shim_for(name.strip_prefix('_').unwrap()).is_some(),
                "{name}"
            );
            assert!(crate::shims::is_shimmed(name), "{name} may be inlined");
        }
    }
}
