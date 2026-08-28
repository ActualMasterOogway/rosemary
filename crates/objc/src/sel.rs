use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char};
use std::sync::{LazyLock, RwLock};

pub type Sel = *const c_char;

/// Every name registered so far, leaked so the pointers stay valid forever.
static TABLE: LazyLock<RwLock<HashSet<&'static CStr>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// The selector for `name`, registering it if this is the first sighting.
pub fn register(name: &CStr) -> Sel {
    if let Some(found) = TABLE.read().unwrap_or_else(|e| e.into_inner()).get(name) {
        return found.as_ptr();
    }
    let mut table = TABLE.write().unwrap_or_else(|e| e.into_inner());
    // Re-checked under the write lock: two threads can miss the read.
    if let Some(found) = table.get(name) {
        return found.as_ptr();
    }
    let leaked: &'static CStr = Box::leak(CString::from(name).into_boxed_c_str());
    table.insert(leaked);
    leaked.as_ptr()
}

/// The selector for a Rust string, for this crate's own classes.
///
/// Panics on an interior NUL, which only a bug here could produce.
pub fn get(name: &str) -> Sel {
    let c = CString::new(name).expect("a selector name has no interior NUL");
    register(&c)
}

/// A selector's name, which is the selector.
pub fn name(sel: Sel) -> *const c_char {
    sel
}

/// Whether two selectors are the same.
///
/// Pointer equality, because registration is what makes them equal. Null is
/// compared as a value so `sel_isEqual(NULL, NULL)` is true, as it is on Apple's.
pub fn is_equal(a: Sel, b: Sel) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_name_registers_to_the_same_pointer() {
        let a = get("countByEnumeratingWithState:objects:count:");
        let b = get("countByEnumeratingWithState:objects:count:");
        assert_eq!(a, b);
        assert_ne!(a, get("count"));
    }

    #[test]
    fn a_selector_is_readable_as_its_own_name() {
        let sel = get("initWithBytes:length:encoding:");
        // SAFETY: `register` leaked a NUL-terminated copy of the name.
        let read = unsafe { CStr::from_ptr(name(sel)) };
        assert_eq!(read.to_str().unwrap(), "initWithBytes:length:encoding:");
    }
}
