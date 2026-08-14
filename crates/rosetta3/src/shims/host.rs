//! Shared lookup for host functions used by hand-written shims.
//!
//! Successful lookups are cached in an immutable snapshot published through
//! an atomic pointer. Reads therefore do not take a lock, which matters because
//! these helpers sit on the slow-but-frequently-reached shim path. A miss may
//! briefly take the writer lock to publish a larger snapshot; misses already
//! in flight can harmlessly race and perform the same `dlsym`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::abi::{AbiError, Result};

type HostSymbols = BTreeMap<&'static str, u64>;

static HOST_SYMBOLS: AtomicPtr<HostSymbols> = AtomicPtr::new(std::ptr::null_mut());
static HOST_SYMBOL_WRITER: Mutex<()> = Mutex::new(());

/// Resolve a shim entry point, refusing by name when the host does not export it.
pub(crate) fn host_or_refuse(symbol: &str) -> Result<u64> {
    let name = symbol.strip_prefix('_').unwrap_or(symbol);

    let snapshot = HOST_SYMBOLS.load(Ordering::Acquire);
    if !snapshot.is_null() {
        // SAFETY: snapshots are leaked before publication and remain valid for
        // the lifetime of the process.
        if let Some(&address) = unsafe { &*snapshot }.get(name) {
            return Ok(address);
        }
    }

    let address = crate::dyld::host_symbol(name).ok_or_else(|| AbiError::NoShim {
        symbol: symbol.to_string(),
    })?;

    // Publish a new immutable map. The lock is only on the uncommon successful
    // miss path; all later lookups use the atomic snapshot above.
    let _writer = HOST_SYMBOL_WRITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current = HOST_SYMBOLS.load(Ordering::Acquire);
    let mut next = if current.is_null() {
        HostSymbols::new()
    } else {
        // SAFETY: `current` points to a leaked snapshot.
        unsafe { (&*current).clone() }
    };
    if let Some(&cached) = next.get(name) {
        return Ok(cached);
    }
    next.insert(Box::leak(name.to_owned().into_boxed_str()), address);
    let published = Box::into_raw(Box::new(next));
    HOST_SYMBOLS.store(published, Ordering::Release);

    Ok(address)
}
