//! The guest's exit handlers: `atexit`, `atexit_b` and `__cxa_atexit` in one list
//! this crate owns, run from one host exit hook.
//!
//! Owned rather than handed to the host's `__cxa_atexit` for two reasons. One list
//! keeps Darwin's order across all three registration calls, with stdio's flush
//! after every one of them. And `dlclose` has to run exactly the handlers of the
//! image it unloads - Darwin's `__cxa_finalize_ranges` - which the host's list,
//! keyed by ELF objects, cannot be asked to do for a Mach-O image.

use std::ffi::{c_int, c_void};
use std::ops::Range;
use std::sync::{Mutex, MutexGuard, Once};

/// What a handler is called as. A zero-argument `atexit` handler ignores the
/// register its argument arrives in.
pub type Handler = unsafe extern "C" fn(*mut c_void);

struct Entry {
    func: Handler,
    arg: usize,
    /// The `__dso_handle` it was registered against, or 0 for `atexit`.
    dso: u64,
    /// The guest code it will run, which is what an image's ranges are matched
    /// against: `func` itself, except where `func` is this crate's own wrapper.
    owner: u64,
}

/// Last registered last; run from the end.
static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

fn entries() -> MutexGuard<'static, Vec<Entry>> {
    ENTRIES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Add a handler, as `__cxa_atexit(func, arg, dso)` does. Returns 0.
///
/// `owner` is the address `dlclose` decides ownership by; see [`Entry::owner`].
pub fn register(func: Handler, arg: *mut c_void, dso: u64, owner: u64) -> c_int {
    // Both before the hook: the host runs its handlers last-registered first,
    // and stdio's flush has to come after every guest handler.
    crate::stdio::register_cleanup();
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        extern "C" fn at_exit() {
            run(|_| true);
        }
        // SAFETY: a plain `extern "C"` function with no captures.
        unsafe { libc::atexit(at_exit) };
    });
    entries().push(Entry {
        func,
        arg: arg as usize,
        dso,
        owner,
    });
    0
}

/// Run and remove every handler `wanted` accepts, last registered first.
///
/// One at a time with the lock released, because a handler may register another
/// or call `exit`, and Darwin runs a handler registered during finalization too.
fn run(wanted: impl Fn(&Entry) -> bool) {
    loop {
        let next = {
            let mut list = entries();
            match list.iter().rposition(&wanted) {
                Some(i) => list.remove(i),
                None => return,
            }
        };
        // SAFETY: the registrant's promise that `func` accepts `arg`, and the
        // image holding `func` is still mapped: an unload finalizes before it
        // unmaps.
        unsafe { (next.func)(next.arg as *mut c_void) };
    }
}

/// `__cxa_finalize(dso)`: the handlers registered against `dso`, or every
/// handler for a null one.
pub fn finalize(dso: u64) {
    run(|e| dso == 0 || e.dso == dso);
}

/// `__cxa_finalize_ranges`: the handlers whose code or DSO handle lies in one of
/// `ranges`, which are an image's mapped extents. What `dlclose` calls before it
/// unmaps them.
pub fn finalize_ranges(ranges: &[Range<u64>]) {
    run(|e| {
        ranges
            .iter()
            .any(|r| r.contains(&e.owner) || r.contains(&e.dso))
    });
    // A macOS host's guest registers with the real libSystem, which keeps its own
    // list and has the real function for it.
    #[cfg(target_os = "macos")]
    host_finalize_ranges(ranges);
}

#[cfg(target_os = "macos")]
fn host_finalize_ranges(ranges: &[Range<u64>]) {
    #[repr(C)]
    struct CxaRange {
        addr: *const c_void,
        length: usize,
    }
    // SAFETY: a NUL-terminated name looked up in the global scope.
    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"__cxa_finalize_ranges".as_ptr()) };
    if f.is_null() {
        return;
    }
    let list: Vec<CxaRange> = ranges
        .iter()
        .map(|r| CxaRange {
            addr: r.start as usize as *const c_void,
            length: (r.end - r.start) as usize,
        })
        .collect();
    // SAFETY: libSystem's `void __cxa_finalize_ranges(const __cxa_range_t *, unsigned)`.
    let f: unsafe extern "C" fn(*const CxaRange, u32) = unsafe { std::mem::transmute(f) };
    unsafe { f(list.as_ptr(), list.len() as u32) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static RAN: Mutex<Vec<u64>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record(arg: *mut c_void) {
        RAN.lock().unwrap().push(arg as u64);
    }

    // Unique per test, so two tests running at once never claim each other's.
    static NEXT: AtomicU64 = AtomicU64::new(0x7000_0000_0000);

    fn range() -> Range<u64> {
        let base = NEXT.fetch_add(0x1_0000, Ordering::Relaxed);
        base..base + 0x1_0000
    }

    fn ran(within: &Range<u64>) -> Vec<u64> {
        RAN.lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|a| within.contains(a))
            .collect()
    }

    #[test]
    fn finalizing_an_images_ranges_runs_only_its_handlers_in_reverse() {
        let (image, other) = (range(), range());
        register(record, image.start as _, 0, image.start + 1);
        register(record, other.start as _, 0, other.start + 1);
        register(record, (image.start + 2) as _, image.start, 0);

        finalize_ranges(std::slice::from_ref(&image));

        // The second matched by its DSO handle alone.
        assert_eq!(ran(&image), [image.start + 2, image.start]);
        assert!(ran(&other).is_empty());
        // Gone once run: a second unload of the same range finds nothing.
        finalize_ranges(std::slice::from_ref(&image));
        assert_eq!(ran(&image).len(), 2);
        finalize_ranges(std::slice::from_ref(&other));
        assert_eq!(ran(&other), [other.start]);
    }

    #[test]
    fn finalize_matches_by_dso_handle() {
        let image = range();
        register(record, image.start as _, image.start, 0);
        register(record, (image.start + 1) as _, image.start + 3, 0);
        finalize(image.start);
        assert_eq!(ran(&image), [image.start]);
        finalize(image.start + 3);
    }
}
