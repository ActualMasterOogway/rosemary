//! `MTLLibrary` and `MTLFunction`, and reporting a library this build cannot translate.

use super::*;

pub(super) unsafe extern "C" fn new_library_with_data(
    receiver: Id,
    _sel: Sel,
    data: u64,
    _error: *mut Id,
) -> Id {
    let Some((bytes, length)) = darwin::dispatch::data::bytes(data) else {
        report_library_error("newLibraryWithData:error: received non-dispatch data");
        return std::ptr::null_mut();
    };
    // SAFETY: dispatch reported this range as the data's bytes.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, length) };
    let air = match metal_translate::metallib_air(bytes) {
        Ok(air) => air,
        Err(error) => {
            report_library_error(error);
            return std::ptr::null_mut();
        }
    };
    // SAFETY: an `R3MTLDevice`.
    let target = unsafe { object::body::<Device>(receiver) }.target;
    // SAFETY: the profile belongs to the process-wide adapter list.
    let Some(target) = (unsafe { target.as_ref() }) else {
        report_library_error("Metal device has no Vulkan target profile");
        return std::ptr::null_mut();
    };
    let translated =
        match metal_translate::translate_cached(metal_translate::AirInput::Bitcode(air), target) {
            Ok(translated) => translated,
            Err(error) => {
                report_library_error(error);
                return std::ptr::null_mut();
            }
        };
    retain_library(receiver, translated, Some(air), Some(target))
}

/// Wrap a translated library in the `MTLLibrary` object the guest holds.
pub(super) fn retain_library(
    device: Id,
    translated: metal_translate::TranslatedLibrary,
    air: Option<&[u8]>,
    target: Option<&'static metal_translate::TargetProfile>,
) -> Id {
    // Only a library with function constants is ever translated again.
    let air = air
        .filter(|_| !translated.function_constants.is_empty())
        .map(<[u8]>::to_vec);
    let library = object::make(
        c"R3MTLLibrary",
        Library {
            translated,
            air,
            target,
        },
    );
    if library.is_null() {
        report_library_error("could not allocate MTL library");
        return std::ptr::null_mut();
    }
    let _ = device;
    library
}

pub(super) unsafe extern "C" fn new_library_with_source(
    receiver: Id,
    _sel: Sel,
    source: Id,
    _options: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe { *error = std::ptr::null_mut() };
    }
    if source.is_null() {
        return fail_library(error, "newLibraryWithSource: requires an NSString source");
    }
    // SAFETY: an `R3MTLDevice`.
    let target = unsafe { object::body::<Device>(receiver) }.target;
    // SAFETY: the profile belongs to the process-wide adapter list.
    let Some(target) = (unsafe { target.as_ref() }) else {
        return fail_library(error, "Metal device has no Vulkan target profile");
    };

    // SAFETY: a guest `NSString`.
    let source = unsafe { foundation::string::bytes(source) };
    if source.is_empty() {
        return fail_library(
            error,
            "newLibraryWithSource: received an empty or invalid NSString",
        );
    }
    let Ok(source) = std::str::from_utf8(&source) else {
        return fail_library(error, "Metal source is not valid UTF-8");
    };
    report_source_library(source);
    dump_source_library(source);
    let translated = match metal_translate::translate_msl_cached(source, target) {
        Ok(translated) => translated,
        Err(translation_error) => return fail_library(error, &translation_error.to_string()),
    };
    let library = retain_library(receiver, translated, None, Some(target));
    if library.is_null() {
        return fail_library(error, "could not allocate MTL source library");
    }
    library
}

pub(super) unsafe extern "C" fn new_function_with_name(receiver: Id, _sel: Sel, name: Id) -> Id {
    // SAFETY: a guest `NSString`.
    let name = unsafe { foundation::string::bytes(name) };
    let Ok(name) = std::str::from_utf8(&name) else {
        return std::ptr::null_mut();
    };
    // SAFETY: an `R3MTLLibrary`.
    let library = unsafe { object::body::<Library>(receiver) };
    let Some(entry) = library
        .translated
        .entries
        .iter()
        .position(|entry| entry.name == name)
    else {
        return std::ptr::null_mut();
    };
    object::make(
        c"R3MTLFunction",
        Function {
            library: objc::arc::retain(receiver),
            entry,
            specialized: None,
        },
    )
}

/// `-[MTLLibrary newFunctionWithName:constantValues:error:]`.
///
/// The library is translated again with the values, through the shader cache,
/// which keys an entry by them; specialization comes before the IR passes, so
/// the code a constant switches off is folded away before emission.
pub(super) unsafe extern "C" fn new_function_with_constants(
    receiver: Id,
    _sel: Sel,
    name: Id,
    constants: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe { *error = std::ptr::null_mut() };
    }
    // SAFETY: a guest `NSString`.
    let name = unsafe { foundation::string::bytes(name) };
    let Ok(name) = std::str::from_utf8(&name) else {
        return fail_library(
            error,
            "newFunctionWithName: names a function in invalid UTF-8",
        );
    };
    // SAFETY: an `R3MTLLibrary`.
    let library = unsafe { object::body::<Library>(receiver) };
    let Some(entry) = library
        .translated
        .entries
        .iter()
        .position(|entry| entry.name == name)
    else {
        return fail_library(error, &format!("the library has no function named {name}"));
    };
    // A library which declares no function constants has nothing to
    // specialize: Metal ignores the values, and so does this.
    let specialized = match (&library.air, library.target) {
        (Some(air), Some(target)) => {
            let values = if object::is_kind(constants, c"MTLFunctionConstantValues") {
                // SAFETY: checked above.
                unsafe { object::body::<ConstantValues>(constants) }
                    .values
                    .clone()
            } else {
                metal_translate::FunctionConstantValues::new()
            };
            let translated = metal_translate::translate_specialized_cached(
                metal_translate::AirInput::Bitcode(air),
                target,
                metal_translate::PassPipeline::from_env(),
                &values,
            );
            match translated {
                Ok(mut translated) => {
                    let Some(position) = translated
                        .entries
                        .iter()
                        .position(|entry| entry.name == name)
                    else {
                        return fail_library(error, "specialization lost the function");
                    };
                    Some(translated.entries.swap_remove(position))
                }
                Err(translation) => return fail_library(error, &translation.to_string()),
            }
        }
        _ => None,
    };
    object::make(
        c"R3MTLFunction",
        Function {
            library: objc::arc::retain(receiver),
            entry,
            specialized,
        },
    )
}

/// `-[MTLLibrary functionNames]`: the entry points the translation produced,
/// in the order the library declares them.
pub(super) unsafe extern "C" fn library_function_names(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `R3MTLLibrary`.
    let library = unsafe { object::body::<Library>(receiver) };
    let names: Vec<CFTypeRef> = library
        .translated
        .entries
        .iter()
        .map(|entry| foundation::string::from_str(&entry.name) as CFTypeRef)
        .collect();
    // SAFETY: a fresh array over freshly created strings.
    unsafe {
        corefoundation::api::CFAutorelease(corefoundation::collections::new_array(
            &names,
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

pub(super) unsafe extern "C" fn library_device(_receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the process has one Metal device per adapter and a library is
    // only made by one of them.
    unsafe { MTLCreateSystemDefaultDevice() }
}

pub(super) unsafe extern "C" fn function_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `R3MTLFunction`.
    let function = unsafe { object::body::<Function>(receiver) };
    // SAFETY: the library which vended the function.
    let library = unsafe { object::body::<Library>(function.library) };
    foundation::string::from_str(&library.translated.entries[function.entry].name)
}

pub(super) fn fail_library(error: *mut Id, message: &str) -> Id {
    report_library_error(message);
    if !error.is_null() {
        // SAFETY: the guest passed a writable `NSError **`.
        unsafe {
            *error = foundation::error::make("MTLLibraryErrorDomain", 3, message);
        }
    }
    std::ptr::null_mut()
}

pub(super) fn report_library_error(message: impl std::fmt::Display) {
    if std::env::var_os("ROSETTA3_TRACE_METAL").is_none() {
        return;
    }
    let message = message.to_string();
    let mut seen = REPORTED_LIBRARY_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if seen.insert(message.clone()) {
        eprintln!("rosetta3: Metal: shader library translation stopped: {message}");
    }
}

/// Report just enough information to identify the first guest source library
/// without placing proprietary game shaders in a log file.
pub(super) fn report_source_library(source: &str) {
    if std::env::var_os("ROSETTA3_TRACE_METAL").is_some()
        && !REPORTED_SOURCE_LIBRARY.swap(true, Ordering::Relaxed)
    {
        eprintln!(
            "rosetta3: Metal: source library: {} bytes, fnv1a64={:016x}",
            source.len(),
            fnv1a64(source.as_bytes()),
        );
    }
}

pub(super) fn dump_source_library(source: &str) {
    let Some(directory) = std::env::var_os("ROSETTA3_MSL_OUT") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let name = format!("msl-{:016x}.metal", fnv1a64(source.as_bytes()));
    let _ = std::fs::write(directory.join(name), source);
}

pub(super) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
