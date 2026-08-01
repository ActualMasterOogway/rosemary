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
//! - **`dladdr`, `RTLD_NEXT`, and re-export chains.** Each is refused or
//!   answered narrowly and says so, rather than being approximated.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::ops::Range;
use std::sync::Mutex;

use machish::exports::ExportTable;
use machish::image::MachObject;
use machish::install_name::PathContext;
use machish::loader::Loader;
use machish::memory::{MmapSpace, Regions};
use machish::namespace::FsDylibSource;
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
static IMAGES: Mutex<Vec<&'static Image>> = Mutex::new(Vec::new());

/// The guest executable's path, for `@executable_path` and for the search that
/// finds a bundled dylib named without a directory.
static EXECUTABLE: Mutex<Option<String>> = Mutex::new(None);

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

/// Record where the guest executable came from, for `@executable_path`.
pub fn set_executable_path(path: &str) {
    let canonical = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    *EXECUTABLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(canonical);
}

/// The loaded images, as a snapshot. The lock is held only for the copy.
fn images() -> Vec<&'static Image> {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The import registered at `va` in any dynamically loaded image.
///
/// The widening described in the module note: what [`crate::Program::import_at`]
/// falls back to, so a call into a stub of an image the lifter never saw is
/// serviced rather than decoded.
pub fn import_at(va: u64) -> Option<&'static ImportSite> {
    images().into_iter().find_map(|i| i.imports.get(&va))
}

/// The import of any dynamically loaded image that *resolved to* `host`.
pub fn import_by_target(host: u64) -> Option<&'static ImportSite> {
    images()
        .into_iter()
        .find_map(|i| i.imports_by_target.get(&host))
}

/// Whether `va` lies inside a dynamically loaded image.
pub fn contains(va: u64) -> bool {
    images().into_iter().any(|i| region_of(i, va).is_some())
}

/// The region to lift in order to execute `va`, if it is in a loaded image.
pub fn lift_region(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    for image in images() {
        let Some((base, bytes)) = region_of(image, va) else {
            continue;
        };
        let function = image.functions.iter().find(|f| f.contains(&va)).cloned();
        let region = crate::region::region_for(bytes, base, va, function)?;
        let start = usize::try_from(region.range.start - base).ok()?;
        let end = usize::try_from(region.range.end - base)
            .ok()?
            .min(bytes.len());
        return Some((region, bytes.get(start..end)?.to_vec()));
    }
    None
}

fn region_of(image: &'static Image, va: u64) -> Option<(u64, &'static [u8])> {
    image
        .regions
        .iter()
        .find(|(base, bytes)| va >= *base && va < base + bytes.len() as u64)
        .copied()
}

/// How many images the guest has loaded.
pub fn loaded() -> usize {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner()).len()
}

// ------------------------------------------------------------- loading ------

/// Load `path` as a guest image, or return the handle it already has.
///
/// Idempotent by resolved path, which is what makes a program that `dlopen`s
/// the same library from two places get one image and one set of initializers.
pub fn open(path: &str) -> std::result::Result<u64, String> {
    let resolved = resolve_path(path).ok_or_else(|| format!("no such file: {path}"))?;

    if let Some(existing) = images().into_iter().find(|i| i.path == resolved) {
        return Ok(existing.handle);
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
    let mut source = FsDylibSource::new();
    let providers = HostProviders::new();
    let report = Loader::new(X86_64)
        .link_with(
            &bytes,
            &resolved,
            &mut memory,
            &mut source,
            &providers.chain(&[]),
        )
        .map_err(|e| format!("{resolved}: {e}"))?;

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
    let (imports, imports_by_target) = collect_imports(&report);

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
    IMAGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(leaked);

    run_initializers(leaked)?;
    Ok(leaked.handle)
}

/// Every address a call site in `report` might name, both directions.
///
/// The same two maps [`crate::Program`] builds, built the same way and for the
/// same reasons — including the unsatisfied imports, which are registered so a
/// call to one is a named symbol rather than `no guest code at 0x0`.
fn collect_imports(report: &LinkReport) -> (BTreeMap<u64, ImportSite>, BTreeMap<u64, ImportSite>) {
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
        if site.resolved != 0 {
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
    let executable = EXECUTABLE.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
    let scoped: Vec<&'static Image> = match images().into_iter().find(|i| i.handle == handle) {
        Some(image) => vec![image],
        // Every pseudo-handle — `RTLD_DEFAULT`, `RTLD_SELF`, `RTLD_NEXT`, and
        // the token `dlopen(NULL)` returns — is treated as a global search.
        // Distinguishing them needs a caller's image, which a shim standing
        // outside the guest's frame does not have.
        None => images(),
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
fn host_symbol(name: &str) -> Option<u64> {
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
    matches!(name, "dlopen" | "dlsym" | "dlclose" | "dlerror").then_some(serve)
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
    for name in ["dlopen", "dlsym", "dlclose", "dlerror"] {
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
