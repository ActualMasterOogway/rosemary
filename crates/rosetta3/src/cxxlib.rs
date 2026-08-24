//! The guest's C++ runtime, run as **guest code** rather than emulated.

use machish::error::Result;
use machish::namespace::{DylibRequest, DylibSource, FetchedDylib};
use machish::spec::arch::X86_64;

/// A real x86-64 `libc++.1.dylib`, built for the ABI Apple's is built for.
const LIBCXX: &[u8] = include_bytes!("../vendor/libs/intel/libc++.1.dylib");
const LIBCXXABI: &[u8] = include_bytes!("../vendor/libs/intel/libc++abi.dylib");
const LIBUNWIND: &[u8] = include_bytes!("../vendor/libs/intel/libunwind.dylib");

/// The install names Apple's `libc++` answers to, and which this stands in for.
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
/// It matters here for one reason: it is where such a binary's *C++ ABI* imports come
/// from. See the module note on the second library that names the ABI.
const LIBSTDCXX_NAMES: &[&str] = &[
    "/usr/lib/libstdc++.6.dylib",
    "/usr/lib/libstdc++.dylib",
    "/usr/lib/libstdc++.6.0.9.dylib",
];

/// The path the vendored image is known by once loaded.
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

/// A [`DylibSource`] that answers `/usr/lib/libc++` with a real Intel libc++, and
/// delegates everything else.
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
/// Slots are rewritten in guest memory *and* in the report, so everything built from
/// the report afterwards - [`crate::Program`]'s import maps, the audit - describes what
/// the slot actually holds.
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
/// [`is_cxx_abi_symbol`] is deliberately a family test rather than "anything the
/// vendored `libc++abi` exports".
pub fn bind_cxxabi_to_the_vendored_one<M: machish::memory::Regions>(
    report: &mut machish::report::LinkReport,
    memory: &mut M,
) -> Result<usize> {
    rebind_to_vendored(report, memory, LIBCXXABI, &is_libcxxabi, is_cxx_abi_symbol)
}

/// Whether `symbol` is part of the Itanium C++ ABI *runtime interface* - the half of
/// `libstdc++` that the vendored `libc++abi` is a drop-in for.
///
/// The three prefixes are the ABI's own namespaces.
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
    // means.
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
            // the host's.
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
        // with their own bytes - `is_libcxx` deciding either would hand a
        // C++-ABI request the library instead.
        assert!(!is_libcxx("/usr/lib/libc++abi.dylib"));
        assert!(is_libcxxabi("/usr/lib/libc++abi.dylib"));
        assert!(!is_libcxx("/usr/lib/libSystem.B.dylib"));
        assert!(!is_libcxx("/usr/lib/libstdc++.6.dylib"));
        assert!(is_libstdcxx("/usr/lib/libstdc++.6.dylib"));
    }

    /// A `libstdc++` request loads the vendored `libc++abi`, **under its own name**.
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
    /// This is the list A Hat in Time imports, which is the whole of the ABI half.
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
    /// `std::runtime_error`'s destructor is exported by the vendored `libc++abi` and by
    /// `libstdc++`, and they are not the same object - see [`is_cxx_abi_symbol`].
    #[test]
    fn the_family_excludes_the_exception_types_and_the_containers() {
        assert!(!is_cxx_abi_symbol("__ZNSt13runtime_errorD1Ev"));
        assert!(!is_cxx_abi_symbol("__ZNKSt9exception4whatEv"));
        assert!(!is_cxx_abi_symbol("__ZNSt9bad_allocC1Ev"));
        assert!(!is_cxx_abi_symbol(
            "__ZNSt3__112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6__initEPKcm"
        ));
        // Not the ABI's `__cxa_atexit`: that one is shimmed, because the shim
        // has to substitute the DSO handle as well.
        assert!(
            !ExportTable::parse(&MachObject::parse_any(LIBCXXABI, X86_64).unwrap())
                .unwrap()
                .contains("___cxa_atexit")
        );
    }
}
