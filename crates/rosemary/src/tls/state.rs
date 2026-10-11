//! What a thread knows about its thread-locals beyond the blocks themselves: the
//! destructors `_tlv_atexit` registered, and the images whose constructors failed.
//!
//! Held under a Darwin key of its own - dyld's `tlv_finalize` key - which is created
//! before any image's, so in every teardown pass its destructor runs ahead of theirs
//! and a C++ `thread_local` destructor still finds the blocks it reads.

use core::ffi::c_void;
use std::sync::OnceLock;

use darwin::tls::key::Key;

/// A `_tlv_atexit` destructor: a C++ `thread_local`'s, called with its object.
type Terminator = unsafe extern "C" fn(*mut c_void);

#[derive(Default)]
struct State {
    /// `(destructor, object)` in registration order; run in reverse.
    terminators: Vec<(Terminator, usize)>,
    /// Registry keys whose block failed to construct on this thread.
    failed: Vec<u32>,
}

/// The key, created on the first registration so that it is older than every image's.
pub(super) fn key() -> Key {
    static KEY: OnceLock<Key> = OnceLock::new();
    *KEY.get_or_init(|| {
        Key::create(Some(finalize)).expect("a guest cannot run without thread-local storage")
    })
}

/// This thread's state, created on first use.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let key = key();
    let mut state = key.get() as *mut State;
    if state.is_null() {
        state = Box::into_raw(Box::<State>::default());
        key.set(state as *mut c_void);
    }
    // SAFETY: a `Box<State>` this function stored, on the thread that owns it; the
    // borrow ends before anything that could re-enter here runs.
    f(unsafe { &mut *state })
}

/// `void _tlv_atexit(void (*termfunc)(void *), void *objAddr)`.
///
/// # Safety
///
/// `terminator` is null or a function this process can call with `object`.
pub unsafe extern "C" fn tlv_atexit(terminator: Option<Terminator>, object: *mut c_void) {
    let Some(terminator) = terminator else { return };
    with(|state| state.terminators.push((terminator, object as usize)));
}

/// Remember that `image`'s constructors failed on this thread.
pub(super) fn mark_failed(image: u32) {
    with(|state| state.failed.push(image));
}

/// Whether `image`'s constructors failed on this thread.
pub(super) fn failed(image: u32) -> bool {
    let state = key().get() as *const State;
    // SAFETY: as in `with`.
    !state.is_null() && unsafe { &*state }.failed.contains(&image)
}

/// `_tlv_exit`: the calling thread's terminators, run now rather than at thread exit.
///
/// libSystem's `exit` calls it before any `atexit` handler, so a C++ `thread_local`
/// of the thread that ends the process is destroyed first - the main thread's,
/// ordinarily, which no pthread teardown ever reaches.
pub(super) fn exit() {
    let state = key().get();
    if state.is_null() {
        return;
    }
    key().set(core::ptr::null_mut());
    // SAFETY: a value `with` stored, now taken out of the key.
    unsafe { finalize(state) };
}

/// dyld's `tlv_finalize`: the thread's terminators, last registered first.
///
/// The key's value is cleared before this is called, so a terminator that registers
/// another lands in a fresh state - which is run here too, until none is left.
///
/// # Safety
///
/// Called by the key's teardown with a value [`with`] stored.
unsafe extern "C" fn finalize(state: *mut c_void) {
    // SAFETY: the caller's guarantee; ownership comes back here.
    let mut state = unsafe { Box::from_raw(state as *mut State) };
    loop {
        while let Some((terminator, object)) = state.terminators.pop() {
            // SAFETY: registered through `tlv_atexit` with exactly this object.
            unsafe { terminator(object as *mut c_void) };
        }
        let more = key().get() as *mut State;
        if more.is_null() {
            return;
        }
        key().set(core::ptr::null_mut());
        // SAFETY: as above, a later state this thread's terminators created.
        state = unsafe { Box::from_raw(more) };
    }
}
