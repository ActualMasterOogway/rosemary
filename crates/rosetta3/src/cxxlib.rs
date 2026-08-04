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
//! ## Why `libc++abi` is deliberately *not* vendored
//!
//! An Intel `libc++abi.dylib` sits beside the vendored `libc++` and is not used,
//! which is a decision rather than an omission.
//!
//! `libc++abi` defines `__cxa_throw`, `__cxa_begin_catch`, `__cxa_end_catch`,
//! `__cxa_rethrow`, `__cxa_allocate_exception` and `_Unwind_Resume` — which is
//! precisely the list [`crate::unwind`] **virtualizes**. It has to: a guest
//! frame is not a machine frame. The guest's code is Cranelift-compiled arm64
//! running on the host's stack, so there is nothing for a real unwinder to walk
//! — and the real unwinder here would be `libunwind`, which `libc++abi` in turn
//! depends on and which is not vendored either. Loading `libc++abi` as guest
//! code would take `__cxa_throw` away from the shim and give it to code that
//! then walks a stack whose frames do not exist, which is the one failure mode
//! worse than not implementing exceptions at all.
//!
//! So the C++ *runtime* is guest code and the C++ *unwinder* stays virtualized,
//! and the seam between them is exactly where it already was. What
//! `libc++abi` would otherwise have supplied — `std::exception` and its `what`,
//! `std::length_error`, `std::out_of_range`, `std::bad_array_new_length`,
//! `std::terminate`, and their `typeinfo` and `vtable` objects, thirteen symbols
//! in `steamclient.dylib`'s case — is answered by the host, for both the guest
//! *and* the vendored `libc++`, so the two agree with each other. Those are the
//! one part of the C++ runtime where agreement is plausible without checking:
//! an exception object is a vtable pointer and a `const char *`, with no ABI
//! switch on it, and [`crate::unwind`]'s `type_info` comparisons already work
//! against the host's copies.
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
        } else if is_libcxxabi(request.install_name) {
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
        // libc++abi is *not* vendored: the thirteen symbols it would supply are
        // deliberately answered by the host, and both the guest and the vendored
        // libc++ get the same answers. Claiming the name without the bytes would
        // be a load failure instead.
        assert!(!is_libcxx("/usr/lib/libc++abi.dylib"));
        assert!(!is_libcxx("/usr/lib/libSystem.B.dylib"));
        assert!(!is_libcxx("/usr/lib/libstdc++.6.dylib"));
    }
}
