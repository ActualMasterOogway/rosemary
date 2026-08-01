//! The code cache: guest addresses to native translations.
//!
//! A region is lifted the first time control is about to reach it and kept for
//! every later entry. That is the whole of the JIT's translation policy, and
//! deliberately so — see the crate note on why nothing is translated ahead of
//! time.
//!
//! ## Entering at an arbitrary PC
//!
//! A translation covers a range of guest addresses, and control does not always
//! arrive at its first one: a `switch` case, the resume point after an
//! intercepted call, the target of a backward branch. Microlift emits an entry
//! dispatch that jumps to the block matching `ctx.rip`, so re-entry works — but
//! only for addresses that are *block leaders*, and it silently falls back to
//! the region's first block for anything else. Silently executing from the
//! wrong address is the worst failure mode available here, so the cache lifts
//! with [`LifterConfig::emit_resume_blocks`], which makes every instruction
//! boundary a leader. Re-entry is then correct at any address the guest can
//! actually name.
//!
//! ## One module per translation
//!
//! Each region gets its own [`CraneliftBackend`], because a backend is a JIT
//! module and a module can define the symbol `lifted_func` once. The cost is a
//! page or so of overhead per region; the alternative — one module, many
//! functions — buys nothing until translations start calling each other
//! directly, which is a later optimisation (see the roadmap's cross-cutting
//! notes on linking cached guest→guest calls).
//!
//! ## Guest code is assumed immutable
//!
//! Nothing invalidates a translation. Self-modifying code, and the write
//! protection and flushing that would detect it, are deliberately deferred —
//! but the assumption is stated here rather than left to be discovered, because
//! a stale translation produces wrong answers rather than an error.

use std::collections::BTreeMap;
use std::sync::Arc;

use machish::memory::Regions;

use microlift::arch::x86_64::X86_64;
use microlift::arch::x86_64::context::RegisterContext;
use microlift::backend::CraneliftBackend;
use microlift::core::exit::ExitReason;
use microlift::core::{LifterConfig, lift_to_function};

use crate::program::Program;
use crate::vm::VmError;

/// One lifted, compiled guest region.
pub struct Translation {
    /// Guest address the region begins at.
    pub start: u64,
    /// One past the last guest byte it covers.
    pub end: u64,
    /// The instruction boundaries inside it — the addresses it may be entered
    /// at. See [`crate::region::Region::starts`] for why a range test is not
    /// enough.
    starts: Vec<u64>,
    /// The compiled entry point. Takes a pointer to the guest context and
    /// returns having written an exit reason into it.
    entry: unsafe extern "C" fn(*mut RegisterContext),
    /// Keeps the JIT module — and therefore `entry`'s pages — mapped.
    _backend: CraneliftBackend,
}

// SAFETY: a `Translation` is immutable once built, and what the guest's threads
// do with one is run it — which is what threads do with any function. `enter`
// touches only the caller's own `RegisterContext` and guest memory, whose races
// are the guest's own (see the note on `Vm`). `_backend` is never read after
// construction; it is held so the code stays mapped, and its `Drop` runs once,
// when the last `Arc` goes, by which time no thread can be inside the code.
//
// The reason this is written out rather than derived is that `CraneliftBackend`
// is neither `Send` nor `Sync` — it is a JIT module, built on one thread and, in
// microlift's intended use, used there. We use it as a finished artefact, which
// is a narrower thing than the type allows and so has to be asserted.
unsafe impl Send for Translation {}
unsafe impl Sync for Translation {}

impl Translation {
    /// Enter the translation, resuming at `ctx.rip`.
    ///
    /// `image_base` is set to the region's start because that is what
    /// microlift's entry dispatch reconciles the PC against: it lifts blocks at
    /// offsets from the lift base and compares `rip - image_base`. Since the
    /// guest is identity-mapped, the lift base *is* the runtime address, so the
    /// two agree exactly when `image_base` is the region start.
    pub fn enter(&self, ctx: &mut RegisterContext) -> ExitReason {
        ctx.image_base = self.start;
        ctx.exit_reason = ExitReason::None as u32;
        // SAFETY: `entry` is the finalized address of a Cranelift function whose
        // signature is `(*mut RegisterContext) -> ()` in the host C convention,
        // compiled by the backend this `Translation` owns and therefore still
        // mapped. The lifted code's only memory effects outside `ctx` are the
        // guest's own — the guest stack and its mapped images, all live for the
        // duration of the call.
        unsafe { (self.entry)(ctx as *mut RegisterContext) };
        ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None)
    }

    /// Whether this translation can be entered at `pc`.
    pub fn covers(&self, pc: u64) -> bool {
        self.starts.binary_search(&pc).is_ok()
    }
}

/// Translations, keyed by the guest address each begins at.
pub struct CodeCache {
    regions: BTreeMap<u64, Arc<Translation>>,
    /// Everything microlift needs to know before it lifts anything: the
    /// registered import addresses and the read-only segment hints. Built once,
    /// cloned per lift, because it does not change while a program runs.
    config: LifterConfig,
}

// SAFETY: the one thing keeping a cache off a second thread is the raw pointer
// inside `LifterConfig`'s `DynamicSymbol`s — the *host address* a fast import
// resolved to. It is a function address, cloned into each lift and never
// dereferenced on this side; the code that eventually calls it is compiled arm64
// doing what any thread may do with a function pointer. The translations beside
// it carry their own claim (see `Translation` above).
unsafe impl Send for CodeCache {}

impl CodeCache {
    /// A cache that lifts with `config`.
    pub fn new(mut config: LifterConfig) -> Self {
        // See the module note: without this, re-entering a region anywhere but
        // its first instruction quietly runs the wrong code.
        config.emit_resume_blocks = true;
        CodeCache {
            regions: BTreeMap::new(),
            config,
        }
    }

    /// How many regions have been translated.
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// The translation that can execute `pc`, lifting one if necessary.
    pub fn get_or_lift<M: Regions>(
        &mut self,
        program: &Program<M>,
        pc: u64,
    ) -> Result<Arc<Translation>, VmError> {
        if let Some(hit) = self.lookup(pc) {
            return Ok(hit);
        }

        let (region, bytes) = program
            .lift_region(pc)
            .ok_or(VmError::NoCode { pc, from: None })?;
        let (start, end) = (region.range.start, region.range.end);

        let func = lift_to_function::<X86_64>(&bytes, start, self.config.clone())
            .map_err(|source| VmError::Lift { pc, start, source })?;
        let mut backend =
            CraneliftBackend::new().map_err(|source| VmError::Lift { pc, start, source })?;
        let code = backend
            .compile(func)
            .map_err(|source| VmError::Lift { pc, start, source })?;

        // SAFETY: `compile` returns the finalized entry of a function whose
        // signature the lifter fixed as `fn(*mut RegisterContext)` in the host
        // calling convention.
        let entry: unsafe extern "C" fn(*mut RegisterContext) =
            unsafe { std::mem::transmute(code) };

        let translation = Arc::new(Translation {
            start,
            end,
            starts: region.starts,
            entry,
            _backend: backend,
        });
        self.regions.insert(start, Arc::clone(&translation));
        Ok(translation)
    }

    /// An existing translation covering `pc`, if there is one.
    fn lookup(&self, pc: u64) -> Option<Arc<Translation>> {
        // Regions may overlap: the same bytes are reachable from two PCs when
        // one lift stopped at a hole, and a function reached only through a
        // pointer may sit inside the span of a region lifted from elsewhere.
        // So this asks each candidate whether `pc` is on *its* instruction
        // grid, nearest first, and a miss means lifting a region that starts at
        // `pc` rather than entering one that would run the wrong code.
        self.regions
            .range(..=pc)
            .rev()
            .find(|(_, t)| t.covers(pc))
            .map(|(_, t)| Arc::clone(t))
    }
}
