use std::ffi::{CStr, c_void};

use objc::class::Class;
use static_init::dynamic;

use crate::base::CFTypeRef;

/// Whether a foreign object is handed back to the framework that made it.
///
/// `ROSETTA3_NO_FOREIGN` turns it off, which is how a run proves it needs no
/// host framework at all: an object that is not this runtime's is then refused
/// by name rather than quietly serviced by Apple's. It is a refusal rather than
/// an error return because there is no answer to give - carrying on would read
/// Apple's object as one of ours.
///
/// `coreaudio::foreign` reads the same variable, for the same reason and with
/// the same default. Its refusal *is* an error return, because
/// `kAudioUnitErr_InvalidElement` is an answer its callers already check for.
pub fn forwarding() -> bool {
    #[dynamic]
    static ON: bool = std::env::var_os("ROSETTA3_NO_FOREIGN").is_none();
    *ON
}

/// Whether `object` came from a CoreFoundation that is not this one.
///
/// Told apart by the isa: an object of this runtime's has a class this runtime
/// built. Asked whatever [`forwarding`] answers - with forwarding off the point
/// is to *name* such an object rather than treat it as one of ours, which is
/// the memory corruption this module exists to stop.
///
/// # Safety
///
/// `object` must be null or an object, so that its first word is readable.
pub unsafe fn is(object: CFTypeRef) -> bool {
    if object.is_null() {
        return false;
    }
    // SAFETY: the caller's obligation; the first word is the isa.
    let isa = unsafe { *(object as *const *const Class) };
    !objc::class::is_known(isa)
}

/// The host's CoreFoundation entry point of that name.
///
/// `dlsym` of the whole process: this tree's own CoreFoundation is published to
/// the guest through a synthetic library rather than exported from the binary,
/// so the name resolves to Apple's and cannot find its way back here.
pub fn entry(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: a NUL-terminated name, and the default search order.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!found.is_null()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object this runtime made is never foreign, whatever else is loaded.
    #[test]
    fn an_object_of_this_runtimes_is_not_foreign() {
        crate::bootstrap();
        let string = crate::string::from_bytes(b"a string of ours", false);
        // SAFETY: an object this call just made.
        assert!(!unsafe { is(string) });
        // SAFETY: as above, and nothing else holds a reference.
        unsafe { crate::api::CFRelease(string) };
    }

    /// The test that matters: a structure whose first word is *not* a class this
    /// runtime built is recognised without following it. Standing in for a
    /// `CGEventRef`, which is what reached `CFRelease` and crashed it.
    #[test]
    fn a_structure_with_an_unknown_isa_is_foreign() {
        crate::bootstrap();
        let not_a_class = 0_usize;
        let object = [std::ptr::from_ref(&not_a_class) as usize, 0, 0, 0];
        // SAFETY: four readable words, which is more than the isa this reads.
        assert!(unsafe { is(std::ptr::from_ref(&object).cast()) });
    }

    #[test]
    fn nil_is_not_foreign() {
        // SAFETY: null is the one value this is explicitly asked about.
        assert!(!unsafe { is(std::ptr::null()) });
    }
}
