//! Objective-C exceptions, as C++ ones.
//!
//! On 64-bit macOS an `@throw` *is* a C++ throw: clang emits the same landing
//! pads, the same LSDA, and a personality routine that is `__gxx_personality_v0`
//! under another name. libobjc's part is three pieces of glue and one data
//! structure, and this module is those.
//!
//! The data structure is the reason it lives here rather than in
//! [`crate::unwind`]. `_OBJC_EHTYPE_$_NSException` is a `type_info`, and matching
//! it is what the vendored `libc++abi` already does - so the record this
//! publishes is a real `__si_class_type_info` whose `__base_type` is the
//! superclass's record. `@catch (NSException *e)` then takes a subclass because
//! libc++abi walks base classes, not because anything here re-implements
//! `isKindOfClass:`.
//!
//! What it replaces is worse than a gap: an unresolved `_OBJC_EHTYPE_$_Foo`
//! binds to zero, and a zero entry in the LSDA's type table is `catch (...)`. So
//! the guest's `@catch (NSException *)` was silently catching C++ exceptions.

use std::ffi::{CString, c_char};
use std::sync::Mutex;

use machish::error::Result;
use machish::exports::ExportTable;
use machish::image::MachObject;
use machish::spec::arch::X86_64;

/// `__cxxabiv1::__si_class_type_info`, which is `{ vtable, name, base }`.
///
/// A root class uses the same three words with `base` zero and the *no-bases*
/// vtable, which reads only the first two - so one layout serves both.
#[repr(C)]
struct TypeInfo {
    /// Set by [`bind`], because it is an address in the vendored `libc++abi`
    /// and that image has not been placed when the record is made.
    vptr: u64,
    name: *const c_char,
    base: u64,
}

// SAFETY: `name` points at a leaked `CString` and is never written after the
// record is made; `vptr` is written once, before any guest code runs.
unsafe impl Send for TypeInfo {}

/// Every record made so far, by class name, so that two images asking for
/// `_OBJC_EHTYPE_$_NSException` get the same address - which is what makes
/// libc++abi's pointer comparison of two `type_info`s correct.
static RECORDS: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// The `type_info` for `class`, making it and its superclasses' if needed.
///
/// `None` for a class this runtime does not have: a record whose base chain
/// stops short would answer "not a subclass" for a class that is one, which is a
/// wrong catch rather than a missing one.
fn record_for(class: &'static objc::class::Class) -> Option<u64> {
    let name = class.name_str().to_string();
    if let Some((_, addr)) = RECORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(n, _)| *n == name)
    {
        return Some(*addr);
    }
    let base = match class.superclass() {
        Some(parent) => record_for(parent)?,
        None => 0,
    };
    let text = CString::new(name.as_str()).ok()?;
    let record = Box::leak(Box::new(TypeInfo {
        vptr: 0,
        name: Box::leak(text.into_boxed_c_str()).as_ptr(),
        base,
    }));
    let addr = record as *const TypeInfo as usize as u64;
    RECORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name, addr));
    Some(addr)
}

/// Point every `_OBJC_EHTYPE_$_` binding at a record, and
/// `___objc_personality_v0` at the vendored `__gxx_personality_v0`.
///
/// Runs after [`crate::cxxlib::bind_cxxabi_to_the_vendored_one`], because the
/// vtable addresses it needs are that image's.
pub fn bind<M: machish::memory::Regions>(
    report: &mut machish::report::LinkReport,
    memory: &mut M,
    libcxxabi: &[u8],
    is_libcxxabi: &dyn Fn(&str) -> bool,
) -> Result<usize> {
    let Some(slide) = report
        .images
        .iter()
        .find(|i| is_libcxxabi(&i.name))
        .map(|i| i.slide)
    else {
        // No C++ runtime is no exceptions, so there is nothing for a type record
        // to be compared against.
        return Ok(0);
    };
    let object = MachObject::parse_any(libcxxabi, X86_64)?;
    let exports = ExportTable::parse(&object)?;
    let at = |symbol: &str| exports.get(symbol).and_then(|e| e.address(slide));

    // Two words in, which is where a C++ vtable *pointer* points: past the
    // offset-to-top and the type_info pointer every vtable begins with.
    let (Some(no_bases), Some(one_base), Some(personality)) = (
        at("__ZTVN10__cxxabiv117__class_type_infoE").map(|a| a.wrapping_add(16)),
        at("__ZTVN10__cxxabiv120__si_class_type_infoE").map(|a| a.wrapping_add(16)),
        at("___gxx_personality_v0"),
    ) else {
        return Ok(0);
    };

    // The libobjc entry points that are their C++ counterpart and nothing else.
    // They are bound here rather than served as shims because a shim would be a
    // host frame in the middle of an unwind. `objc_begin_catch` is not among
    // them: it returns the object rather than the exception buffer, which is one
    // dereference and therefore not an alias.
    let aliases = [
        ("_objc_end_catch", "___cxa_end_catch"),
        ("_objc_exception_rethrow", "___cxa_rethrow"),
    ];

    let mut bound = 0;
    for image in &mut report.images {
        for binding in &mut image.bindings {
            let addr = if binding.symbol == "___objc_personality_v0" {
                personality
            } else if let Some((_, cxx)) =
                aliases.iter().find(|(objc, _)| *objc == binding.symbol)
            {
                // Unconditionally, over whatever provider answered: this
                // runtime publishes these names so that a call to one is
                // refused by name rather than silently unresolved, and the
                // vendored library is the real implementation.
                match at(cxx) {
                    Some(addr) => addr,
                    None => continue,
                }
            } else if let Some(class) = binding.symbol.strip_prefix("_OBJC_EHTYPE_$_") {
                // Already satisfied means the guest defines the class itself,
                // and its own record is the one its `@catch` names.
                if binding.resolved != 0 {
                    continue;
                }
                let Some(record) = std::ffi::CString::new(class)
                    .ok()
                    .and_then(|n| objc::class::lookup(&n))
                    .and_then(record_for)
                else {
                    continue;
                };
                // SAFETY: the address came out of the table above, so it is a
                // record this module leaked.
                unsafe {
                    let r = record as *mut TypeInfo;
                    (*r).vptr = match (*r).base {
                        0 => no_bases,
                        _ => one_base,
                    };
                }
                record
            } else {
                continue;
            };
            memory.write(binding.slot_va, &addr.to_le_bytes())?;
            binding.resolved = addr;
            binding.tag = 0;
            bound += 1;
        }
    }
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record for a class is the record for that class every time, and its
    /// base is its superclass's - which is the whole of what libc++abi walks.
    #[test]
    fn a_chain_of_records_follows_the_class_chain() {
        foundation::bootstrap();
        let string = objc::class::lookup(c"NSString").expect("NSString");
        let first = record_for(string).expect("a record");
        assert_eq!(record_for(string), Some(first), "not interned");

        // SAFETY: an address this module leaked.
        let base = unsafe { (*(first as *const TypeInfo)).base };
        let object = objc::class::lookup(c"NSObject").expect("NSObject");
        assert_eq!(Some(base), record_for(object));
        // SAFETY: as above. `NSObject` is the root, so it has no base.
        assert_eq!(unsafe { (*(base as *const TypeInfo)).base }, 0);
    }

    /// The name is the plain class name, which is what clang puts in the record
    /// it emits for a class the guest defines - so the two compare equal on the
    /// path where libc++abi compares names rather than addresses.
    #[test]
    fn the_name_is_the_class_name() {
        foundation::bootstrap();
        let class = objc::class::lookup(c"NSString").expect("NSString");
        let record = record_for(class).expect("a record");
        // SAFETY: an address this module leaked, whose `name` is a leaked CStr.
        let name = unsafe { std::ffi::CStr::from_ptr((*(record as *const TypeInfo)).name) };
        assert_eq!(name.to_str(), Ok("NSString"));
    }
}
