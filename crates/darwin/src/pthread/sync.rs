use std::sync::atomic::Ordering;

use super::consts::*;
use super::object::{Kind, Record};

pub use super::cond::{
    cond_broadcast, cond_destroy, cond_init, cond_signal, cond_timedwait, cond_timedwait_relative,
    cond_wait,
};
pub use super::mutex::{mutex_destroy, mutex_init, mutex_lock, mutex_trylock, mutex_unlock};
pub use super::once::{once, once_entry};
pub use super::rwlock::{
    rwlock_destroy, rwlock_init, rwlock_rdlock, rwlock_tryrdlock, rwlock_trywrlock, rwlock_unlock,
    rwlock_wrlock,
};

/// What a `pthread_mutexattr_t` says.
#[derive(Clone, Copy, Debug)]
pub struct MutexAttr {
    pub kind: i32,
    pub pshared: i32,
    pub protocol: i32,
}

impl Default for MutexAttr {
    fn default() -> Self {
        MutexAttr {
            kind: PTHREAD_MUTEX_DEFAULT,
            pshared: PTHREAD_PROCESS_PRIVATE,
            protocol: PTHREAD_PRIO_NONE,
        }
    }
}

impl MutexAttr {
    /// Pack into the one spare word a sixteen-byte attribute object has.
    pub fn packed_public(self) -> u64 {
        (self.kind as u32 as u64)
            | ((self.pshared as u32 as u64) << 16)
            | ((self.protocol as u32 as u64) << 32)
    }

    /// [`MutexAttr::packed_public`] in reverse.
    pub fn unpacked_public(word: u64) -> MutexAttr {
        MutexAttr {
            kind: (word & 0xffff) as i32,
            pshared: ((word >> 16) & 0xffff) as i32,
            protocol: ((word >> 32) & 0xffff) as i32,
        }
    }

    /// What the guest's `pthread_mutexattr_t` at `attr` says.
    ///
    /// A null pointer is the defaults, which is what a null attribute argument to
    /// `pthread_mutex_init` means. `None` is "that is not an attribute set".
    ///
    /// Always this crate's own record, on either host - unlike [`super::Attributes`],
    /// which reads a *thread* attribute set and may be handed one libpthread wrote.
    /// The difference is who filled it in: a `pthread_mutexattr_t` only ever reaches
    /// here from this crate's `pthread_mutexattr_init`, because the two are bound to
    /// the guest together or not at all.
    ///
    /// # Safety
    ///
    /// `attr` is null or a guest `pthread_mutexattr_t *`.
    pub unsafe fn of_guest(attr: u64) -> Option<MutexAttr> {
        if attr == 0 {
            return Some(MutexAttr::default());
        }
        let r = unsafe { Record::at(attr, Kind::MutexAttr) };
        r.is_ours()
            .then(|| MutexAttr::unpacked_public(r.cell().load(Ordering::Relaxed)))
    }
}

/// The `pshared` of a `pthread_condattr_t` or `pthread_rwlockattr_t`, which is the
/// only setting either carries that this crate acts on.
///
/// The outer `None` is "that is not an attribute set"; the inner one is "no attribute
/// set was given", which means the defaults.
///
/// # Safety
///
/// `attr` is null or a guest attribute object of `kind`.
pub(super) unsafe fn simple_attr_pshared(attr: u64, kind: Kind) -> Option<Option<i32>> {
    if attr == 0 {
        return Some(None);
    }
    let r = unsafe { Record::at(attr, kind) };
    r.is_ours()
        .then(|| Some(r.cell().load(Ordering::Relaxed) as i32))
}

/// Report a pthread feature this crate declines to fake, once per process.
pub(super) fn say_unsupported(what: &str) {
    use std::sync::atomic::AtomicBool;
    static SAID: AtomicBool = AtomicBool::new(false);
    if SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = what;
    const MESSAGE: &[u8] = b"rosemary: pthread: a process-shared object is unsupported\n";
    // SAFETY: a fixed static buffer; `write` is async-signal-safe.
    unsafe { libc::write(libc::STDERR_FILENO, MESSAGE.as_ptr().cast(), MESSAGE.len()) };
}
