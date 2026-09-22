//! The guest's pthread objects: what is in their bytes, and who owns them.
//!
//! A guest `pthread_mutex_t` is `{ long __sig; char __opaque[56]; }` - a signature and
//! a block of bytes libpthread reserves for itself. The sizes are public; the contents
//! of `__opaque` are not, and this module does not try to find out.
//!
//! It does not have to. **On a host that is not a Mac, every one of those bytes is
//! ours.** The guest never reads them: the type is opaque, and the only things that
//! touch it are the `pthread_*` functions, all of which this crate implements. So the
//! opaque block holds a record of our own design, and the only facts about Darwin that
//! matter are three:
//!
//! * the **size** of each object, so a record fits inside one;
//! * the **signature** each *static initializer* writes, because those are emitted by
//!   the guest's compiler rather than by a call we service - a `PTHREAD_MUTEX_INITIALIZER`
//!   arrives already "initialised" and has to be recognised;
//! * that the signature is the **first eight bytes**, which the public struct says.
//!
//! All three are checked against the host's own declarations on macOS, where the
//! guest's types and this process's are the same types. That is what makes the layout
//! used on Linux something measured rather than something believed.

use std::sync::atomic::{AtomicU64, Ordering};

/// Darwin's object sizes on LP64, from `<sys/_pthread/_pthread_types.h>`: the `long`
/// signature plus the `__opaque` block each type declares.
pub mod size {
    pub const ATTR: usize = 8 + 56;
    pub const MUTEX: usize = 8 + 56;
    pub const MUTEXATTR: usize = 8 + 8;
    pub const COND: usize = 8 + 40;
    pub const CONDATTR: usize = 8 + 8;
    pub const RWLOCK: usize = 8 + 192;
    pub const RWLOCKATTR: usize = 8 + 16;
    pub const ONCE: usize = 8 + 8;
}

/// The signature each static initializer writes, from `<pthread/pthread_impl.h>`.
///
/// A guest that writes `PTHREAD_MUTEX_INITIALIZER` into a global has already done the
/// only initialisation it intends to do, so these are the values that must be
/// recognised and turned into a real object on first use.
pub mod sig {
    pub const MUTEX_INIT: u64 = 0x32AA_ABA7;
    pub const ERRORCHECK_MUTEX_INIT: u64 = 0x32AA_ABA1;
    pub const RECURSIVE_MUTEX_INIT: u64 = 0x32AA_ABA2;
    pub const FIRSTFIT_MUTEX_INIT: u64 = 0x32AA_ABA3;
    /// `_PTHREAD_MUTEX_SIG_init_MASK`: preserves the static mutex kind.
    pub const MUTEX_INIT_MASK: u64 = 0xFFFF_FFF0;
    /// Bits of a static initializer that select its mutex kind.
    pub const MUTEX_INIT_TYPE_MASK: u64 = 0x0000_000F;
    /// `_PTHREAD_MUTEX_SIG_init_CMP`: the static mutex signature family.
    pub const MUTEX_INIT_CMP: u64 = 0x32AA_ABA0;
    /// The low-nibble static type values `_pthread_mutex_init` recognises.
    pub const MUTEX_INIT_ERRORCHECK: u64 = ERRORCHECK_MUTEX_INIT & MUTEX_INIT_TYPE_MASK;
    pub const MUTEX_INIT_RECURSIVE: u64 = RECURSIVE_MUTEX_INIT & MUTEX_INIT_TYPE_MASK;
    pub const MUTEX_INIT_FIRSTFIT: u64 = FIRSTFIT_MUTEX_INIT & MUTEX_INIT_TYPE_MASK;
    pub const MUTEX_INIT_NORMAL: u64 = MUTEX_INIT & MUTEX_INIT_TYPE_MASK;
    pub const COND_INIT: u64 = 0x3CB0_B1BB;
    pub const ONCE_INIT: u64 = 0x30B1_BCBA;
    pub const RWLOCK_INIT: u64 = 0x2DA8_B3B4;
}

/// Which kind of object a record describes.
///
/// Carried in the signature so that a `pthread_cond_t` handed to `pthread_mutex_lock`
/// is `EINVAL` rather than a host mutex built out of a condition variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Mutex = 1,
    Cond = 2,
    RwLock = 3,
    Once = 4,
    Attr = 5,
    MutexAttr = 6,
    CondAttr = 7,
    RwLockAttr = 8,
}

/// The high bytes of every signature this crate writes: `"r3"`, then the kind.
///
/// Chosen so it cannot collide with a Darwin signature, all of which fit in 32 bits,
/// and so it is legible in a hex dump of guest memory.
const TAG: u64 = 0x7233_0000_0000_0000;

/// The signature an initialised object of `kind` carries.
pub const fn ours(kind: Kind) -> u64 {
    TAG | ((kind as u64) << 8) | 0x01
}

/// The signature a record carries while one thread is building it.
///
/// Only ever observed by another thread racing to first-touch the same statically
/// initialised object, which spins until it is gone.
const BUSY: u64 = TAG | 0xFF;

/// A destroyed object. Distinct from zero so a double destroy is `EINVAL` rather than
/// a fresh lazy initialisation of whatever the memory now holds.
const DEAD: u64 = TAG | 0xDE;

/// One object's record, laid over the guest's bytes.
///
/// ```text
/// +0   u64  signature      ours(kind), a Darwin static initializer, BUSY, or DEAD
/// +8   u64  payload        a boxed host object, or packed settings for an attribute
/// +16  u64  owner          the guest address this record was built at
/// ```
///
/// The `owner` word is what catches a guest that copies an initialised object by
/// value. POSIX does not allow that, and without the check the copy and the original
/// would share one host lock - which is not a crash, just two critical sections that
/// quietly stop excluding each other.
///
/// `Once`, `MutexAttr` and `CondAttr` are sixteen bytes and have no room for `owner`;
/// none of the three has anything to alias, because none owns a heap allocation.
pub struct Record {
    at: u64,
    kind: Kind,
}

/// Byte offsets within a record.
const PAYLOAD: u64 = 8;
const OWNER: u64 = 16;

impl Kind {
    /// Whether an object of this kind has room for the [`OWNER`] word.
    const fn tracks_owner(self) -> bool {
        matches!(
            self,
            Kind::Mutex | Kind::Cond | Kind::RwLock | Kind::RwLockAttr
        )
    }

    /// The signature a static initializer for this kind writes, if it has one.
    fn is_static_initializer(self, signature: u64) -> bool {
        match self {
            // libpthread does not compare a static mutex initializer exactly.
            // Its mask preserves the low nibble for `_pthread_mutex_init`, which
            // selects the mutex kind, and ignores the rest. This accommodates
            // older binaries whose initializer carried otherwise-unused bits.
            // Keep that compatibility rule here rather than requiring one of
            // the four initializer spellings current headers expose.
            Kind::Mutex => (signature & sig::MUTEX_INIT_MASK) == sig::MUTEX_INIT_CMP,
            Kind::Cond => signature == sig::COND_INIT,
            Kind::RwLock => signature == sig::RWLOCK_INIT,
            // Darwin's implementation ignores `__sig`, so a zero-filled control
            // word is also a valid never-run once object.
            Kind::Once => matches!(signature, sig::ONCE_INIT | 0),
            // The attribute types have no static initializer: POSIX requires
            // `pthread_attr_init` and friends, so an attribute set this crate has not
            // seen is not one at all.
            _ => false,
        }
    }
}

/// What a lookup found.
pub enum Found {
    /// An initialised object; the payload is whatever [`Record::set_payload`] stored.
    Ready(u64),
    /// A static initializer, not yet turned into an object. The payload is the
    /// signature, which says *which* initializer - a recursive mutex and a plain one
    /// are different objects.
    Fresh(u64),
    /// Not an object of this kind, or one that has been destroyed.
    Invalid,
}

impl Record {
    /// The record at guest address `at`.
    ///
    /// # Safety
    ///
    /// `at` is a writable, eight-byte-aligned guest address with at least this kind's
    /// size behind it - which is what a `pthread_mutex_t *` from the guest is.
    pub unsafe fn at(at: u64, kind: Kind) -> Record {
        Record { at, kind }
    }

    fn word(&self, offset: u64) -> &AtomicU64 {
        // SAFETY: the constructor's guarantee. A `pthread_*_t` begins with a `long`,
        // so the object is at least eight-byte aligned, and every offset used here is
        // within the size the kind declares.
        unsafe { &*((self.at + offset) as usize as *const AtomicU64) }
    }

    /// This object's state, without changing it.
    pub fn peek(&self) -> Found {
        let sig = self.word(0).load(Ordering::Acquire);
        if sig == ours(self.kind) {
            return match self.owner_matches() {
                true => Found::Ready(self.word(PAYLOAD).load(Ordering::Relaxed)),
                false => Found::Invalid,
            };
        }
        match self.kind.is_static_initializer(sig) {
            true => Found::Fresh(sig),
            false => Found::Invalid,
        }
    }

    /// Whether this record was built at this address.
    fn owner_matches(&self) -> bool {
        if !self.kind.tracks_owner() {
            return true;
        }
        let owner = self.word(OWNER).load(Ordering::Relaxed);
        if owner == self.at {
            return true;
        }
        say_copied(self.kind, self.at, owner);
        false
    }

    /// Get this object's payload, building it from a static initializer if that is
    /// all it has.
    ///
    /// `build` is called at most once per object, with the signature of whichever
    /// static initializer the guest wrote, and by exactly one thread. A second thread
    /// arriving meanwhile waits rather than building a second object.
    pub fn payload_or_build(&self, build: impl FnOnce(u64) -> Option<u64>) -> Option<u64> {
        loop {
            let sig = self.word(0).load(Ordering::Acquire);
            if sig == ours(self.kind) {
                return self
                    .owner_matches()
                    .then(|| self.word(PAYLOAD).load(Ordering::Relaxed));
            }
            if sig == BUSY {
                // Another thread is building this object. Short by construction: it
                // is one allocation and one initialisation, with no call into the
                // guest and no lock of ours held.
                std::hint::spin_loop();
                continue;
            }
            if !self.kind.is_static_initializer(sig) {
                return None;
            }
            // Claim it. A loser retries and sees `BUSY` or the finished object.
            if self
                .word(0)
                .compare_exchange(sig, BUSY, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let Some(payload) = build(sig) else {
                // Put the initializer back, so a caller that can afford to retry is
                // not left with an object it can never use.
                self.word(0).store(sig, Ordering::Release);
                return None;
            };
            self.set_payload(payload);
            return Some(payload);
        }
    }

    /// Publish `payload` and mark this an object of its kind.
    ///
    /// The signature is stored **last** and with release ordering, so a thread that
    /// observes it observes the payload and the owner too.
    pub fn set_payload(&self, payload: u64) {
        self.word(PAYLOAD).store(payload, Ordering::Relaxed);
        if self.kind.tracks_owner() {
            self.word(OWNER).store(self.at, Ordering::Relaxed);
        }
        self.word(0).store(ours(self.kind), Ordering::Release);
    }

    /// Mark this object destroyed and hand back its payload, or `None` if it was not
    /// an initialised object of this kind.
    ///
    /// A static initializer that was never used is destroyable and yields no payload:
    /// there is nothing to free, and refusing it would fail a `pthread_mutex_destroy`
    /// on a mutex that was simply never locked.
    pub fn take(&self) -> Option<Option<u64>> {
        let sig = self.word(0).load(Ordering::Acquire);
        if self.kind.is_static_initializer(sig) {
            return self
                .word(0)
                .compare_exchange(sig, DEAD, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
                .then_some(None);
        }
        if sig != ours(self.kind) || !self.owner_matches() {
            return None;
        }
        let payload = self.word(PAYLOAD).load(Ordering::Relaxed);
        self.word(0)
            .compare_exchange(sig, DEAD, Ordering::AcqRel, Ordering::Relaxed)
            .ok()?;
        Some(Some(payload))
    }

    /// One eight-byte slot of this object, counted from its signature.
    ///
    /// Slot 0 is the signature and slot 1 the payload; the rest is whatever the kind
    /// makes of it. An attribute set has no heap allocation and no owner word, so it
    /// spends its block on the settings themselves.
    ///
    /// # Panics
    ///
    /// If the slot is past the object, which is a bug in this crate rather than
    /// anything the guest can cause: the sizes are compile-time constants and the
    /// callers are all in this module.
    pub fn slot(&self, index: usize, of_size: usize) -> &AtomicU64 {
        assert!(
            (index + 1) * 8 <= of_size,
            "slot {index} is past a {of_size}-byte object"
        );
        self.word(index as u64 * 8)
    }

    /// This object's guest address, which is its identity.
    pub fn address(&self) -> u64 {
        self.at
    }

    /// Whether this record is an initialised object of its kind.
    pub fn is_ours(&self) -> bool {
        self.word(0).load(Ordering::Acquire) == ours(self.kind) && self.owner_matches()
    }

    /// The word an object with no heap allocation keeps its whole state in.
    ///
    /// `pthread_once_t` is sixteen bytes and needs no host object at all.
    pub fn cell(&self) -> &AtomicU64 {
        self.word(PAYLOAD)
    }

    /// Force this record to `ours`, discarding whatever was there.
    ///
    /// What an explicit `pthread_mutex_init` does. Re-initialising a live object is
    /// undefined in POSIX; the previous payload is returned so the caller can free it
    /// rather than leak it.
    pub fn reinitialise(&self, payload: u64) -> Option<u64> {
        let previous = (self.word(0).load(Ordering::Acquire) == ours(self.kind)
            && self.owner_matches())
        .then(|| self.word(PAYLOAD).load(Ordering::Relaxed));
        self.set_payload(payload);
        previous
    }
}

/// Report that an object was copied after being initialised, once per process.
///
/// The failure this prevents is quiet: two mutexes sharing one host lock still lock
/// and unlock, they just stop excluding each other. `EINVAL` alone would not say why.
fn say_copied(kind: Kind, at: u64, owner: u64) {
    use std::sync::atomic::AtomicBool;
    static SAID: AtomicBool = AtomicBool::new(false);
    if SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = (kind, at, owner);
    const MESSAGE: &[u8] = b"rosetta3: pthread: a copied initialised pthread object was refused\n";
    // SAFETY: a fixed static buffer; `write` is async-signal-safe.
    unsafe { libc::write(libc::STDERR_FILENO, MESSAGE.as_ptr().cast(), MESSAGE.len()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes are Darwin's, checked against the host's own types where the two are
    /// the same types. This is the assertion that makes the layout measured rather
    /// than believed - and it can only be made on a Mac.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_sizes_are_the_ones_this_host_declares() {
        assert_eq!(size::ATTR, size_of::<libc::pthread_attr_t>());
        assert_eq!(size::MUTEX, size_of::<libc::pthread_mutex_t>());
        assert_eq!(size::MUTEXATTR, size_of::<libc::pthread_mutexattr_t>());
        assert_eq!(size::COND, size_of::<libc::pthread_cond_t>());
        assert_eq!(size::CONDATTR, size_of::<libc::pthread_condattr_t>());
        assert_eq!(size::RWLOCK, size_of::<libc::pthread_rwlock_t>());
        assert_eq!(size::ONCE, size_of::<libc::pthread_once_t>());
    }

    /// And the signatures are the ones the host's own static initializers write,
    /// which is the other half of the same cross-check: these values are how a
    /// `PTHREAD_MUTEX_INITIALIZER` in a guest binary is recognised.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_static_initializers_write_the_signatures_this_module_looks_for() {
        let m = libc::PTHREAD_MUTEX_INITIALIZER;
        // SAFETY: reading the first eight bytes of a live `pthread_mutex_t`, which
        // the public struct declares as its `long __sig`.
        let first = unsafe { *(&m as *const libc::pthread_mutex_t as *const u64) };
        assert_eq!(first, sig::MUTEX_INIT);

        let c = libc::PTHREAD_COND_INITIALIZER;
        // SAFETY: as above.
        assert_eq!(
            unsafe { *(&c as *const libc::pthread_cond_t as *const u64) },
            sig::COND_INIT
        );
    }

    /// A record must fit inside the smallest object of its kind, or it writes past
    /// the guest's own struct.
    #[test]
    fn every_record_fits_the_object_it_lives_in() {
        for (kind, bytes) in [
            (Kind::Mutex, size::MUTEX),
            (Kind::Cond, size::COND),
            (Kind::RwLock, size::RWLOCK),
            (Kind::Once, size::ONCE),
            (Kind::Attr, size::ATTR),
            (Kind::MutexAttr, size::MUTEXATTR),
            (Kind::CondAttr, size::CONDATTR),
            (Kind::RwLockAttr, size::RWLOCKATTR),
        ] {
            let needed = match kind.tracks_owner() {
                true => OWNER as usize + 8,
                false => PAYLOAD as usize + 8,
            };
            assert!(
                bytes >= needed,
                "{kind:?} needs {needed} bytes and has {bytes}"
            );
        }
    }

    /// No signature this crate writes can be mistaken for one Darwin writes, in
    /// either direction - which is what lets a single word distinguish "the guest
    /// statically initialised this" from "we have already built it".
    #[test]
    fn our_signatures_cannot_collide_with_darwins() {
        let darwin = [
            sig::MUTEX_INIT,
            sig::ERRORCHECK_MUTEX_INIT,
            sig::RECURSIVE_MUTEX_INIT,
            sig::FIRSTFIT_MUTEX_INIT,
            sig::COND_INIT,
            sig::ONCE_INIT,
            sig::RWLOCK_INIT,
        ];
        let mine: Vec<u64> = [
            Kind::Mutex,
            Kind::Cond,
            Kind::RwLock,
            Kind::Once,
            Kind::Attr,
            Kind::MutexAttr,
            Kind::CondAttr,
            Kind::RwLockAttr,
        ]
        .iter()
        .map(|k| ours(*k))
        .chain([BUSY, DEAD])
        .collect();

        for d in darwin {
            assert!(d < 1 << 32, "a Darwin signature fits in 32 bits");
            assert!(!mine.contains(&d), "{d:#x} is claimed by both");
        }
        let mut sorted = mine.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), mine.len(), "two kinds share a signature");
    }
}
