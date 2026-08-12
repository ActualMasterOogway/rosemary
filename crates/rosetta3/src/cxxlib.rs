//! The guest's C++ runtime, run as **guest code** rather than emulated.
//!
//! ## The decision, and what it replaces
//!
//! Every other `/usr/lib` import in this runtime is native passthrough: the
//! guest calls `strlen` and reaches the host's arm64 `strlen`. That works
//! because a C library's interface is scalars and pointers to structures the
//! two architectures agree about — a `pthread_mutex_t`, a `struct sigaction`, a
//! `struct kevent` — and where they do not agree, a signature can say so.
//!
//! **The C++ standard library is not like that**, and `std::string` is the proof
//! ([M7.2](../../docs/ROADMAP.md)). libc++ selects
//! `_LIBCPP_ABI_ALTERNATE_STRING_LAYOUT` on arm64 and not on x86-64, so the same
//! twenty-four bytes mean different things on the two sides: the data pointer is
//! at `+16` for the guest and `+0` for the host, the size of a short string is
//! in the first byte doubled for the guest and in the last byte for the host.
//! An import described *correctly* — every argument really is a `this` pointer
//! and a scalar — still handed the object to code that read different fields out
//! of it, and the failure surfaced as a `SIGSEGV` inside protobuf with nothing
//! anywhere naming a string.
//!
//! That was fixed once by writing `basic_string` out by hand against the guest's
//! layout. It worked, and it does not scale: `basic_string` is one type. Behind
//! it are `basic_streambuf` and its vtable, `ios_base`'s state and its callback
//! list, `locale` and its facet table, `std::function`, `shared_ptr`'s control
//! block — a library whose surface is *objects with layouts and virtual
//! dispatch*, not functions with signatures. Reimplementing that faithfully is a
//! project, and every piece of it is a place to be subtly and silently wrong.
//!
//! So the model changes here: when a guest asks for `/usr/lib/libc++`, it gets a
//! **real Intel libc++**, loaded and lifted like any other guest image. Its
//! `basic_string` is the guest's `basic_string` because it *is* the same code
//! the guest was compiled against. Nothing is described, nothing is marshalled,
//! and the ABI question stops being asked.
//!
//! ## Why this is not the shared cache
//!
//! The roadmap rules out reading the x86-64 dyld shared cache, and that still
//! holds — the objection was never "x86-64 library code is bad", it was that
//! cache-resident *libSystem* issues raw `syscall`s and touches commpage state
//! this runtime does not model. libc++ does none of that. It is a leaf library
//! that calls libSystem through ordinary imports, which native passthrough
//! answers exactly as it does for any other guest image. That is what makes this
//! library specifically safe to run as guest code when libSystem is not.
//!
//! ## `libc++abi` and `libunwind` are vendored too, and that is the later answer
//!
//! This module first vendored `libc++` alone, on the reasoning that
//! `libc++abi` defines the `__cxa_*` family [`crate::unwind`] virtualizes and
//! that a real unwinder would walk a stack whose frames do not exist. Both are
//! vendored now, and the C++ runtime is guest code end to end.
//!
//! ## One unwind, one unwinder — and the linker splits them
//!
//! Vendoring the unwinder exposes a problem that vendoring the library did not,
//! and it is a fact about how a C++ program is *linked* rather than about
//! either ABI. Two symbols, one exception:
//!
//! ```text
//! (undefined) external ___cxa_throw    (from libc++)
//! (undefined) external __Unwind_Resume (from libSystem)
//! ```
//!
//! `__cxa_throw` names libc++, so it resolves into the vendored guest
//! `libc++abi`. `_Unwind_Resume` — which the compiler emits at the bottom of
//! every landing pad, in the *program's own* code — names **libSystem**, because
//! that is where macOS re-exports libunwind from. Native passthrough answers
//! libSystem, so it resolves to the **host's** unwinder.
//!
//! That splits one unwind across two implementations. The guest's `libc++abi`
//! begins it by calling the guest `libunwind`'s `_Unwind_RaiseException`, which
//! builds a `_Unwind_Exception` and walks the guest's frames; the landing pad
//! then hands that object to the *host's* `_Unwind_Resume`, which reads a
//! structure its own libunwind did not create and continues along a stack it has
//! never seen. Nothing about that is detectable from a name, and it fails at the
//! moment an exception is thrown.
//!
//! So [`bind_unwinder_to_the_vendored_one`] rewrites those slots after the link:
//! any `_Unwind_*` binding that resolved outside the guest is pointed at the
//! vendored `libunwind`'s own export. Which unwinder a program uses is decided
//! by whoever raised the exception, and that is the guest's.
//!
//! ## `libstdc++` names the same ABI, and vendoring by install name misses it
//!
//! The split above is about *which library a symbol names*, and there is a
//! second library that names the C++ ABI. A binary built before libc++ became
//! the default — A Hat in Time is one, and so is anything else linked with the
//! 10.6-era toolchain — links `/usr/lib/libstdc++.6.dylib` and takes eleven
//! symbols from it:
//!
//! ```text
//! ___cxa_guard_acquire  ___cxa_begin_catch  ___gxx_personality_v0
//! ___cxa_guard_release  ___cxa_end_catch    __ZSt9terminatev
//! ___cxa_guard_abort    ___cxa_pure_virtual __ZTVN10__cxxabiv117__class_type_infoE
//! ___cxa_call_unexpected                    __ZTVN10__cxxabiv120__si_class_type_infoE
//! ```
//!
//! Every one of those is the Itanium C++ ABI, not libstdc++'s own library —
//! Apple's `libstdc++` gets them from `libc++abi` itself. But nothing in the
//! list above names `libc++abi`, so the vendoring keyed on install names never
//! saw the request, native passthrough answered `libstdc++`, and the C++ ABI
//! went back to being the host's for exactly the programs that have no libc++ at
//! all. It surfaced as `___cxa_guard_acquire: no signature` — a host call the
//! ABI table has no entry for, because the entry was removed when this file
//! made the answer guest code.
//!
//! Two things follow, and both are narrow. A request for `libstdc++` loads the
//! vendored `libc++abi`, so the image is *present* in a link that mentions no
//! libc++ — but it answers to its own name, so it does not silently claim
//! libstdc++'s. And [`bind_cxxabi_to_the_vendored_one`] then moves the bindings
//! in [`is_cxx_abi_symbol`]'s family, and only those: the containers, the
//! iostreams, and the standard exception *types* still resolve to the host,
//! because those are the parts where `libstdc++` and `libc++abi` are genuinely
//! different libraries rather than two spellings of one interface.
//!
//! ## What it costs
//!
//! - **A copy per link unit.** [`crate::dyld::open`] links each `dlopen`ed image
//!   against the host and the main executable, not against other loaded guest
//!   images, so each link that wants libc++ maps its own — with its own
//!   `std::cout`, its own locale table, and its own `std::mutex` statics. Under
//!   UNDERTALE that is two: the `Mac_Runner` link, and the `steamclient.dylib` +
//!   `libtier0_s.dylib` link. Nothing observed depends on them being one, and
//!   nothing would look like a loading problem the day something does — a
//!   `std::string` built by one and destroyed by the other is fine (same layout,
//!   same `operator new`), but a `static` inside libc++ is not shared.
//!
//! - **Every import that resolves into it is a guest→guest call**, and the
//!   runtime has to know that everywhere rather than in most places. It did not:
//!   the lifter's fast-import promotion baked an absolute native `call` to
//!   whatever address an import resolved to, and had never been handed a guest
//!   one before, because before this every `/usr/lib` import was a host
//!   function. See `no_inlined_import_targets_guest_memory` in
//!   `tests/cxxstring.rs`, which is the regression that costs a few seconds of
//!   DELTARUNE and a backtrace with no names in it.
//! - **Lifting it.** It is a megabyte of Intel code that used to be zero. The
//!   JIT translates what control reaches, so the cost is proportional to how
//!   much of libc++ the guest actually uses.
//!
//! ## Why the bytes are embedded
//!
//! [`crate::bundle`] re-executes this binary from a mirror in the user's cache
//! directory, so anything found by a path relative to the executable is found in
//! a different place after the re-exec, and anything found relative to the
//! source tree is not found at all once the binary is installed. Embedding is
//! the only form of "always present" that survives both, and it means there is
//! no fallback path in which a guest silently gets the host's libc++ back —
//! which matters, because that fallback is the original bug.

use machish::error::Result;
use machish::namespace::{DylibRequest, DylibSource, FetchedDylib};
use machish::spec::arch::X86_64;

/// A real x86-64 `libc++.1.dylib`, built for the ABI Apple's is built for.
///
/// Checked rather than assumed to be the right vintage: it exports
/// `basic_string::__grow_by` and `__basic_string_common<true>::__throw_length_error`,
/// both of which newer libc++ deleted, so it is an old-ABI build of the kind a
/// 2015-2020 Intel binary was linked against. `vendored_libcxx_is_the_abi_the_guest_expects`
/// is that check.
const LIBCXX: &[u8] = include_bytes!("../vendor/libs/intel/libc++.1.dylib");
const LIBCXXABI: &[u8] = include_bytes!("../vendor/libs/intel/libc++abi.1.dylib");
const LIBUNWIND: &[u8] = include_bytes!("../vendor/libs/intel/libunwind.1.dylib");

/// The install names Apple's `libc++` answers to, and which this stands in for.
///
/// Matched on the **install name the image recorded**, not on an expanded
/// candidate path, for the same reason [`machish::namespace::FsDylibSource`]
/// does: that string is what the guest actually asked for.
///
/// `@rpath/libc++.1.dylib` is in the list because it is the vendored library's
/// *own* `LC_ID_DYLIB`, so an image linked against a relocatable libc++ — or the
/// vendored library appearing in its own dependency graph — asks for it by that
/// name.
const NAMES: &[&str] = &[
    "/usr/lib/libc++.1.dylib",
    "/usr/lib/libc++.dylib",
    "/usr/lib/libc++.1.0.dylib",
    "@rpath/libc++.1.dylib",
];

const CXXABI_NAMES: &[&str] = &[
    "/usr/lib/libc++abi.1.dylib",
    "/usr/lib/libc++abi.dylib",
    "/usr/lib/libc++abi.1.0.dylib",
    "@rpath/libc++abi.1.dylib",
];

const UNWIND_NAMES: &[&str] = &[
    "/usr/lib/libunwind.1.dylib",
    "/usr/lib/libunwind.dylib",
    "/usr/lib/libunwind.1.0.dylib",
    "@rpath/libunwind.1.dylib",
];

/// The GNU C++ runtime a pre-2011 Intel binary was linked against.
///
/// It matters here for one reason: it is where such a binary's *C++ ABI*
/// imports come from. See the module note on the second library that names the
/// ABI.
const LIBSTDCXX_NAMES: &[&str] = &[
    "/usr/lib/libstdc++.6.dylib",
    "/usr/lib/libstdc++.dylib",
    "/usr/lib/libstdc++.6.0.9.dylib",
];

/// The path the vendored image is known by once loaded.
///
/// The name the guest asked for rather than where the bytes came from, because
/// that is the string every other image's dependency records and the one
/// [`machish::namespace::NamespaceImage::answers_to`] has to match — and because
/// a diagnostic saying `/usr/lib/libc++.1.dylib` describes what this *is* better
/// than a path inside this repository would.
const AS_PATH: &str = "/usr/lib/libc++.1.dylib";
const CXXABI_PATH: &str = "/usr/lib/libc++abi.1.dylib";
const UNWIND_PATH: &str = "/usr/lib/libunwind.1.dylib";

/// Whether `install_name` is a request for the C++ runtime.
pub fn is_libcxx(install_name: &str) -> bool {
    NAMES.contains(&install_name)
}

pub fn is_libcxxabi(install_name: &str) -> bool {
    CXXABI_NAMES.contains(&install_name)
}

pub fn is_libunwind(install_name: &str) -> bool {
    UNWIND_NAMES.contains(&install_name)
}

pub fn is_libstdcxx(install_name: &str) -> bool {
    LIBSTDCXX_NAMES.contains(&install_name)
}

/// A [`DylibSource`] that answers `/usr/lib/libc++` with a real Intel libc++,
/// and delegates everything else.
///
/// A wrapper rather than a replacement because the two decisions are
/// independent: `FsDylibSource` decides where a *bundled* library comes from,
/// and this decides that one system library is not passed through. Chaining
/// them keeps either one readable on its own.
pub struct VendoredCxx<S> {
    inner: S,
}

impl<S> VendoredCxx<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: DylibSource> DylibSource for VendoredCxx<S> {
    fn fetch(&mut self, request: &DylibRequest<'_>) -> Result<Option<FetchedDylib>> {
        if is_libcxx(request.install_name) {
            return Ok(Some(FetchedDylib {
                path: AS_PATH.to_string(),
                bytes: LIBCXX.to_vec(),
            }));
        } else if is_libcxxabi(request.install_name) || is_libstdcxx(request.install_name) {
            return Ok(Some(FetchedDylib {
                path: CXXABI_PATH.to_string(),
                bytes: LIBCXXABI.to_vec(),
            }));
        } else if is_libunwind(request.install_name) {
            return Ok(Some(FetchedDylib {
                path: UNWIND_PATH.to_string(),
                bytes: LIBUNWIND.to_vec(),
            }));
        }
        self.inner.fetch(request)
    }
}

/// Point every `_Unwind_*` binding at the **vendored** `libunwind`.
///
/// See the module note on why the linker splits one unwind across two
/// unwinders. This is the repair, and it is a rebind rather than a provider for
/// a reason worth stating: a [`SymbolProvider`] is built before
/// [`Loader::link_with`] runs and cannot know where the loader will map
/// `libunwind`, because the loader chooses that during mapping — which happens
/// after the providers are assembled and before the bindings they answer are
/// written. The address only exists once the link is done.
///
/// Slots are rewritten in guest memory *and* in the report, so everything built
/// from the report afterwards — [`crate::Program`]'s import maps, the audit —
/// describes what the slot actually holds.
///
/// A binding that already resolved into the guest is left alone: it named
/// libc++ or libc++abi and reached the vendored image the ordinary way — and
/// "into the guest" is decided by the mapped ranges rather than by comparing
/// addresses with the export, for the reason [`rebind_to_vendored`] gives. So is
/// one this vendored `libunwind` does not export, which is how a name that only
/// looks like the family stays the host's.
///
/// [`SymbolProvider`]: machish::resolve::SymbolProvider
/// [`Loader::link_with`]: machish::loader::Loader::link_with
pub fn bind_unwinder_to_the_vendored_one<M: machish::memory::Regions>(
    report: &mut machish::report::LinkReport,
    memory: &mut M,
) -> Result<usize> {
    // The Mach-O spelling carries a leading underscore, so the C identifier
    // `_Unwind_Resume` arrives as `__Unwind_Resume`.
    rebind_to_vendored(report, memory, LIBUNWIND, &is_libunwind, |symbol| {
        symbol.starts_with("__Unwind_")
    })
}

/// Point every C++ ABI binding at the **vendored** `libc++abi`.
///
/// The same repair as [`bind_unwinder_to_the_vendored_one`] and for the same
/// reason, applied to the other library that gets split — see the module note on
/// `libstdc++`. A binding that named `libc++abi` already reached the vendored
/// image through the namespace and is left alone; one that named `libstdc++`
/// resolved to the host, and this is what moves it.
///
/// [`is_cxx_abi_symbol`] is deliberately a family test rather than "anything the
/// vendored `libc++abi` exports".
pub fn bind_cxxabi_to_the_vendored_one<M: machish::memory::Regions>(
    report: &mut machish::report::LinkReport,
    memory: &mut M,
) -> Result<usize> {
    rebind_to_vendored(report, memory, LIBCXXABI, &is_libcxxabi, is_cxx_abi_symbol)
}

/// Whether `symbol` is part of the Itanium C++ ABI *runtime interface* — the
/// half of `libstdc++` that the vendored `libc++abi` is a drop-in for.
///
/// The three prefixes are the ABI's own namespaces: `___cxa_*` is the runtime
/// entry points (`__cxa_throw`, the function-local static guards,
/// `__cxa_pure_virtual`), `__ZN10__cxxabiv1*` and its `__ZT[VIS]N10__cxxabiv1*`
/// data counterparts are the `type_info` hierarchy the personality routine
/// walks, and `___gxx_personality_*` is that routine. `TERMINATE_HANDLERS` is
/// the `std::` spelling of the same interface — `std::terminate` is called by
/// `__cxa_throw` when there is no handler, so it has to be the same
/// implementation that raised.
///
/// What is **excluded** is the point: the vendored `libc++abi` also exports the
/// standard exception *types* (`std::runtime_error` and friends), and those are
/// not interchangeable with `libstdc++`'s. `libstdc++` builds a
/// `std::runtime_error` around a COW `std::string`; `libc++abi` builds one
/// around a `char*`. Rebinding a destructor for an object some host `libstdc++`
/// constructor made would be exactly the class of silent layout bug
/// [`crate::cxxlib`] exists to remove, so a name outside this family stays
/// wherever it resolved — both halves of such a type's lifetime then belong to
/// one library.
fn is_cxx_abi_symbol(symbol: &str) -> bool {
    /// The handler interface, which is spelled in `std::` rather than
    /// `__cxxabiv1::` but is defined by the ABI all the same.
    const TERMINATE_HANDLERS: &[&str] = &[
        "__ZSt9terminatev",
        "__ZSt10unexpectedv",
        "__ZSt13get_terminatev",
        "__ZSt14get_unexpectedv",
        "__ZSt13set_terminatePFvvE",
        "__ZSt14set_unexpectedPFvvE",
        "__ZSt18uncaught_exceptionv",
        "__ZSt19uncaught_exceptionsv",
    ];

    symbol.starts_with("___cxa_")
        || symbol.starts_with("___gxx_personality_")
        || symbol.starts_with("__ZN10__cxxabiv1")
        || symbol.starts_with("__ZTVN10__cxxabiv1")
        || symbol.starts_with("__ZTIN10__cxxabiv1")
        || symbol.starts_with("__ZTSN10__cxxabiv1")
        || TERMINATE_HANDLERS.contains(&symbol)
}

/// The shared body of the two rebinds above.
///
/// `bytes` is the vendored image whose exports are the new answer, `is_image`
/// finds where the loader put it, and `wanted` selects the symbols to move.
fn rebind_to_vendored<M: machish::memory::Regions>(
    report: &mut machish::report::LinkReport,
    memory: &mut M,
    bytes: &[u8],
    is_image: &dyn Fn(&str) -> bool,
    wanted: impl Fn(&str) -> bool,
) -> Result<usize> {
    // Where the vendored image landed, if this link wanted one at all. A C
    // program does not, and neither does a C++ program that never throws.
    let Some((base, slide)) = report
        .images
        .iter()
        .find(|i| is_image(&i.name))
        .map(|i| (i.base, i.slide))
    else {
        return Ok(0);
    };
    let object = machish::image::MachObject::parse_any(bytes, X86_64)?;
    let exports = machish::exports::ExportTable::parse(&object)?;

    // Every address the loader mapped, which is what "already in the guest"
    // means. Asking the ranges rather than comparing against the export's own
    // address is the load-bearing part: a **data** binding carries an addend,
    // and the C++ ABI's `type_info` vtables are bound at `+16` because that is
    // where a vtable pointer points. An equality test against the export sees
    // those sixteen bytes as a wrong answer and "corrects" a binding that was
    // already right, which moves every guest `type_info`'s vtable pointer to
    // the head of the vtable object — and the crash is inside the vendored
    // `libc++abi` comparing types, several frames from anything naming a bind.
    let guest: Vec<(u64, u64)> = report
        .images
        .iter()
        .flat_map(|i| &i.segments)
        .map(|s| (s.vmaddr, s.vmaddr.wrapping_add(s.vmsize)))
        .collect();
    let in_guest = |addr: u64| guest.iter().any(|(lo, hi)| addr >= *lo && addr < *hi);

    let mut rebound = 0;
    for image in &mut report.images {
        // The vendored image's own bindings are not rewritten: it is the thing
        // being bound *to*, and its internal references are already its own.
        if image.base == base {
            continue;
        }
        for binding in &mut image.bindings {
            if !wanted(&binding.symbol) {
                continue;
            }
            if in_guest(binding.resolved) {
                continue;
            }
            let Some(export) = exports.get(&binding.symbol) else {
                continue;
            };
            let Some(addr) = export.address(slide) else {
                continue;
            };
            memory.write(binding.slot_va, &addr.to_le_bytes())?;
            binding.resolved = addr;
            // The tag is the *provider's*, and the provider that answered was
            // the host's. Clearing it stops a stale index selecting a signature
            // for a call that is now guest-to-guest.
            binding.tag = 0;
            rebound += 1;
        }
    }
    Ok(rebound)
}

/// The source every link in this crate uses: vendored libc++ in front of the
/// filesystem.
pub fn source() -> VendoredCxx<machish::namespace::FsDylibSource> {
    VendoredCxx::new(machish::namespace::FsDylibSource::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use machish::exports::ExportTable;
    use machish::image::MachObject;
    use machish::spec::arch::X86_64;

    fn parsed() -> MachObject<'static, X86_64> {
        MachObject::parse_any(LIBCXX, X86_64).expect("the vendored libc++ is an x86-64 Mach-O")
    }

    /// It has to be the *guest's* architecture, or none of this means anything.
    #[test]
    fn the_vendored_library_is_intel() {
        let obj = parsed();
        assert!(obj.section("__TEXT", "__text").is_some(), "no __text");
    }

    /// The vintage check.
    ///
    /// Both of these were deleted from libc++ years ago — `__grow_by` when the
    /// growth path was reworked, `__basic_string_common` when the throwing
    /// helpers moved into the header. A build that still exports them is one
    /// whose `basic_string` is the layout a 2015-2020 Intel binary was compiled
    /// against, which is the entire reason this file exists. A "newer is better"
    /// upgrade of the vendored library would fail here rather than silently
    /// leave `steamclient.dylib`'s imports unresolved.
    #[test]
    fn vendored_libcxx_is_the_abi_the_guest_expects() {
        let exports = ExportTable::parse(&parsed()).expect("exports");
        for symbol in [
            "__ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE9__grow_byEmmmmmm",
            "__ZNKSt3__121__basic_string_commonILb1EE20__throw_length_errorEv",
            "__ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6__initEPKcm",
            "__ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEaSERKS5_",
            "__ZNSt3__18ios_base4initEPv",
        ] {
            assert!(exports.contains(symbol), "vendored libc++ lacks {symbol}");
        }
    }

    /// The names it stands in for, and one it must not claim.
    #[test]
    fn it_answers_for_libcxx_and_nothing_else() {
        assert!(is_libcxx("/usr/lib/libc++.1.dylib"));
        assert!(is_libcxx("/usr/lib/libc++.dylib"));
        assert!(is_libcxx("@rpath/libc++.1.dylib"));
        // libc++abi and libstdc++ are answered too, but by their own arms and
        // with their own bytes — `is_libcxx` deciding either would hand a
        // C++-ABI request the library instead.
        assert!(!is_libcxx("/usr/lib/libc++abi.dylib"));
        assert!(is_libcxxabi("/usr/lib/libc++abi.dylib"));
        assert!(!is_libcxx("/usr/lib/libSystem.B.dylib"));
        assert!(!is_libcxx("/usr/lib/libstdc++.6.dylib"));
        assert!(is_libstdcxx("/usr/lib/libstdc++.6.dylib"));
    }

    /// A `libstdc++` request loads the vendored `libc++abi`, **under its own
    /// name**.
    ///
    /// Both halves matter and they pull in opposite directions. The bytes have to
    /// arrive, or a program with no libc++ has no `libc++abi` in the link for
    /// [`bind_cxxabi_to_the_vendored_one`] to bind to. The *path* has to stay
    /// `libc++abi`'s, or [`machish::namespace::NamespaceImage::answers_to`]
    /// matches it for `/usr/lib/libstdc++.6.dylib` and every symbol the vendored
    /// image happens to export — `std::runtime_error`'s destructor included —
    /// silently answers a two-level `libstdc++` import.
    #[test]
    fn a_libstdcxx_request_gets_the_cxxabi_bytes_under_the_cxxabi_name() {
        let mut source = VendoredCxx::new(machish::namespace::NoDylibSource);
        let fetched = source
            .fetch(&DylibRequest {
                install_name: "/usr/lib/libstdc++.6.dylib",
                candidates: &[],
                kind: machish::image::DependencyKind::Load,
                loader_path: None,
            })
            .expect("the vendored source does not fail")
            .expect("libstdc++ is answered");
        assert_eq!(fetched.path, CXXABI_PATH);
        assert_eq!(fetched.bytes.len(), LIBCXXABI.len());
    }

    /// The vendored `libc++abi` really does define every C++ ABI symbol a
    /// `libstdc++`-linked binary takes from `libstdc++`.
    ///
    /// This is the list A Hat in Time imports, which is the whole of the ABI
    /// half. If a vendored-library upgrade dropped one of them the rebind would
    /// silently leave that binding on the host — the bug this file is about —
    /// rather than fail, so it is checked here instead.
    #[test]
    fn the_vendored_cxxabi_covers_what_libstdcxx_supplied() {
        let object = MachObject::parse_any(LIBCXXABI, X86_64).expect("x86-64 Mach-O");
        let exports = ExportTable::parse(&object).expect("exports");
        for symbol in [
            "___cxa_begin_catch",
            "___cxa_call_unexpected",
            "___cxa_end_catch",
            "___cxa_guard_abort",
            "___cxa_guard_acquire",
            "___cxa_guard_release",
            "___cxa_pure_virtual",
            "___gxx_personality_v0",
            "__ZSt9terminatev",
            "__ZTVN10__cxxabiv117__class_type_infoE",
            "__ZTVN10__cxxabiv120__si_class_type_infoE",
        ] {
            assert!(
                exports.contains(symbol),
                "vendored libc++abi lacks {symbol}"
            );
            assert!(is_cxx_abi_symbol(symbol), "{symbol} is not in the family");
        }
    }

    /// What the family test must *not* claim.
    ///
    /// `std::runtime_error`'s destructor is exported by the vendored `libc++abi`
    /// and by `libstdc++`, and they are not the same object — see
    /// [`is_cxx_abi_symbol`]. `__ZNSt3__1*` is libc++'s own namespace, which
    /// reaches the vendored library by name and needs no rebind.
    #[test]
    fn the_family_excludes_the_exception_types_and_the_containers() {
        assert!(!is_cxx_abi_symbol("__ZNSt13runtime_errorD1Ev"));
        assert!(!is_cxx_abi_symbol("__ZNKSt9exception4whatEv"));
        assert!(!is_cxx_abi_symbol("__ZNSt9bad_allocC1Ev"));
        assert!(!is_cxx_abi_symbol(
            "__ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6__initEPKcm"
        ));
        // Not the ABI's `__cxa_atexit`: that one is shimmed, because the shim
        // has to substitute the DSO handle as well. It names libSystem and is
        // not exported by the vendored libc++abi, so the family test never sees
        // it — but the intent is worth pinning.
        assert!(
            !ExportTable::parse(&MachObject::parse_any(LIBCXXABI, X86_64).unwrap())
                .unwrap()
                .contains("___cxa_atexit")
        );
    }
}
