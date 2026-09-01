//! Reference counting, autorelease pools, weak references and `@synchronized`.
//!
//! The retain count lives in a side table rather than in the object, because an
//! object here is only `isa` plus the ivars its class declared - a guest builds
//! objects with that layout by hand, and there is no spare word to claim. A side
//! table costs a hash per retain; correctness first, and the count is not on any
//! path this runtime has measured.
//!
//! An absent entry means one reference, so an object that is never retained
//! costs nothing and `-release` on it deallocates.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, MutexGuard, RwLock};

use crate::object::Id;

/// Extra references beyond the one an object is born with.
static COUNTS: LazyLock<Mutex<HashMap<usize, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn counts() -> MutexGuard<'static, HashMap<usize, usize>> {
    COUNTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Add a reference. Returns `object`, as `objc_retain` does.
pub fn retain(object: Id) -> Id {
    if object.is_null() {
        return object;
    }
    // A block counts its own references, in its `flags` word: see
    // `crate::block`. Retaining one here would leave the count in the block
    // untouched and a stack block on the stack.
    // SAFETY: a live object, whose first word is the `isa`.
    if unsafe { crate::block::is_block(object) } {
        return crate::block::copy(object);
    }
    *counts().entry(object as usize).or_insert(0) += 1;
    object
}

/// Drop a reference, deallocating at zero.
///
/// Deallocation is `-dealloc` if the class implements it, and a plain `free`
/// otherwise - the same order Apple's runtime uses, and the reason a class that
/// owns memory gets a chance to give it back.
pub fn release(object: Id) {
    if object.is_null() {
        return;
    }
    // SAFETY: a live object; see [`retain`].
    if unsafe { crate::block::is_block(object) } {
        return crate::block::release(object);
    }
    {
        let mut map = counts();
        if let Some(extra) = map.get_mut(&(object as usize)) {
            *extra -= 1;
            if *extra == 0 {
                map.remove(&(object as usize));
            }
            return;
        }
    }
    deallocate(object);
}

/// The current reference count, as `-retainCount` reports it.
pub fn retain_count(object: Id) -> usize {
    if object.is_null() {
        return 0;
    }
    1 + counts().get(&(object as usize)).copied().unwrap_or(0)
}

/// Send `-dealloc` if there is one, then give the memory back.
fn deallocate(object: Id) {
    clear_weak_references(object);
    let dealloc = crate::sel::get("dealloc");
    // SAFETY: `object` is an object this runtime allocated.
    if let Some(class) = unsafe { crate::object::class_of(object) }
        && let Some(method) = class.method(dealloc)
    {
        let imp = method.implementation();
        // SAFETY: `-dealloc` is `void (*)(id, SEL)` in every Objective-C there
        // has ever been.
        let f: unsafe extern "C" fn(Id, crate::sel::Sel) = unsafe { std::mem::transmute(imp) };
        // SAFETY: as above. An override is expected to end by calling super's,
        // and `NSObject`'s is what frees.
        unsafe { f(object, dealloc) };
        return;
    }
    // SAFETY: nothing holds a reference; see `release`.
    unsafe { crate::object::dispose_instance(object) };
}

/// Free an object whose `-dealloc` chain has finished. `NSObject`'s `-dealloc`.
pub fn finalize(object: Id) {
    // SAFETY: reached only from the end of a `-dealloc` chain.
    unsafe { crate::object::dispose_instance(object) }
}

thread_local! {
    /// The autorelease stack for this thread: one `Vec` of objects per pool.
    static POOLS: std::cell::RefCell<Vec<Vec<Id>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The sentinel `objc_autoreleasePoolPop` is given back.
///
/// The depth of the pool that was pushed, plus one so it is never null. Apple
/// returns an interior pointer; nothing may read it either way.
pub fn pool_push() -> *mut c_void {
    POOLS.with(|pools| {
        let mut pools = pools.borrow_mut();
        pools.push(Vec::new());
        pools.len() as *mut c_void
    })
}

/// Release everything added since the matching [`pool_push`].
///
/// Pools nest, so a token naming an outer pool pops the inner ones too - which
/// is what happens when a `@autoreleasepool` unwinds past one.
pub fn pool_pop(token: *mut c_void) {
    let depth = token as usize;
    loop {
        let Some(pool) = POOLS.with(|pools| pools.borrow_mut().pop()) else {
            return;
        };
        for object in pool {
            release(object);
        }
        if POOLS.with(|pools| pools.borrow().len()) < depth {
            return;
        }
    }
}

/// Hand `object` to the innermost pool. Returns `object`.
///
/// With no pool at all the object is leaked rather than released, and said so
/// once: releasing it here would be a use-after-free in the caller, which is
/// strictly worse than a leak.
pub fn autorelease(object: Id) -> Id {
    if object.is_null() {
        return object;
    }
    POOLS.with(|pools| match pools.borrow_mut().last_mut() {
        Some(pool) => pool.push(object),
        None => no_pool(),
    });
    object
}

fn no_pool() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        eprintln!("rosetta3: objc: autorelease with no pool in place; leaking");
    }
}

/// Every live `__weak` slot, by the object it points at.
static WEAK: LazyLock<RwLock<HashMap<usize, Vec<usize>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Point `slot` at `object`, registering it to be zeroed on deallocation.
pub fn store_weak(slot: *mut Id, object: Id) -> Id {
    destroy_weak(slot);
    // SAFETY: the caller owns `slot`, which is a word of its own storage.
    unsafe { *slot = object };
    if !object.is_null() {
        WEAK.write()
            .unwrap_or_else(|e| e.into_inner())
            .entry(object as usize)
            .or_default()
            .push(slot as usize);
    }
    object
}

/// The object in `slot`, retained and autoreleased so the caller can use it.
pub fn load_weak_retained(slot: *mut Id) -> Id {
    // SAFETY: the caller owns `slot`.
    let object = unsafe { *slot };
    if object.is_null() {
        return object;
    }
    retain(object)
}

/// Unregister `slot` and clear it.
pub fn destroy_weak(slot: *mut Id) {
    // SAFETY: the caller owns `slot`.
    let object = unsafe { *slot };
    if object.is_null() {
        return;
    }
    let mut guard = WEAK.write().unwrap_or_else(|e| e.into_inner());
    if let Some(slots) = guard.get_mut(&(object as usize)) {
        slots.retain(|s| *s != slot as usize);
    }
    drop(guard);
    // SAFETY: as above.
    unsafe { *slot = std::ptr::null_mut() };
}

/// Zero every weak slot that pointed at `object`.
fn clear_weak_references(object: Id) {
    let Some(slots) = WEAK
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(object as usize))
    else {
        return;
    };
    for slot in slots {
        // SAFETY: a slot registered by `store_weak` and not yet destroyed. This
        // is the whole point of the registration: the owner is about to read a
        // pointer that would otherwise dangle.
        unsafe { *(slot as *mut Id) = std::ptr::null_mut() };
    }
}

/// One recursive lock per object, created on first use and never reclaimed.
static MONITORS: LazyLock<Mutex<HashMap<usize, &'static Monitor>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A recursive mutex, which is what `@synchronized` needs and what a plain
/// `Mutex` is not: the same thread may enter twice.
struct Monitor {
    owner: Mutex<Option<(u64, usize)>>,
    condition: std::sync::Condvar,
}

fn monitor(object: Id) -> &'static Monitor {
    let mut guard = MONITORS.lock().unwrap_or_else(|e| e.into_inner());
    guard.entry(object as usize).or_insert_with(|| {
        Box::leak(Box::new(Monitor {
            owner: Mutex::new(None),
            condition: std::sync::Condvar::new(),
        }))
    })
}

fn thread_id() -> u64 {
    // A value unique to this thread for as long as it runs, which is all the
    // recursion check needs.
    thread_local! {
        static SLOT: u8 = const { 0 };
    }
    SLOT.with(|slot| slot as *const u8 as u64)
}

pub fn sync_enter(object: Id) {
    if object.is_null() {
        return;
    }
    let monitor = monitor(object);
    let me = thread_id();
    let mut owner = monitor.owner.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match *owner {
            Some((holder, ref mut depth)) if holder == me => {
                *depth += 1;
                return;
            }
            None => {
                *owner = Some((me, 1));
                return;
            }
            Some(_) => {
                owner = monitor
                    .condition
                    .wait(owner)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

pub fn sync_exit(object: Id) {
    if object.is_null() {
        return;
    }
    let monitor = monitor(object);
    let mut owner = monitor.owner.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((holder, ref mut depth)) = *owner
        && holder == thread_id()
    {
        *depth -= 1;
        if *depth == 0 {
            *owner = None;
            monitor.condition.notify_one();
        }
    }
}

/// `objc_setAssociatedObject`'s table, keyed by object and key pointer.
///
/// Addresses rather than pointers, so the table is `Send` without an assertion
/// this crate would then have to justify.
static ASSOCIATIONS: LazyLock<RwLock<HashMap<(usize, usize), usize>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn set_associated(object: Id, key: *const c_void, value: Id) {
    let mut map = ASSOCIATIONS.write().unwrap_or_else(|e| e.into_inner());
    match value.is_null() {
        true => map.remove(&(object as usize, key as usize)),
        false => map.insert((object as usize, key as usize), value as usize),
    };
}

pub fn associated(object: Id, key: *const c_void) -> Id {
    ASSOCIATIONS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(object as usize, key as usize))
        .map_or(std::ptr::null_mut(), |v| *v as Id)
}
