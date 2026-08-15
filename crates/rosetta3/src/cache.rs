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
//! only for addresses the dispatch was given an arm for, and by default it
//! silently falls back to the region's first block for anything else. Silently
//! executing from the wrong address is the worst failure mode available here,
//! and closing it is what the two knobs below are for.
//!
//! The obvious answer — `LifterConfig::emit_resume_blocks`, which makes every
//! instruction boundary an arm — is the one this cache used to take, and it is
//! **the single most expensive thing the runtime did**. A resume point per
//! instruction multiplies a region's block count by roughly the number of
//! instructions per basic block, and Cranelift's register allocator is
//! superlinear in the live ranges that produces. Measured over the same 128 KiB
//! of a real program's `__text`, in 4 KiB pieces: 19.36s of compile time with a
//! resume block per instruction, 1.42s without. Thirteen times, for a property
//! that is only ever needed at the handful of addresses control actually
//! arrives at from outside.
//!
//! So the cache instead **declares** those addresses
//! ([`LifterConfig::resume_points`]) and asks microlift to *refuse* rather than
//! guess at anything else ([`LifterConfig::strict_resume`]). Four sources, and
//! only the first is this crate's to name — the rest are things microlift adds
//! itself, because they are facts about the lift that a host cannot know
//! without redoing it:
//!
//! - the enclosing image's recorded **function starts** inside the region,
//!   which is where a call through a pointer lands;
//! - the region's own entry, which is the PC it was lifted for;
//! - the resume point after every call that leaves the region;
//! - the resume point of every **VM exit that resumes** — the instruction after
//!   a `CPUID`, `RDTSC`, `SYSCALL`, `INT`, or port-I/O access.
//!
//! Which is to say: what a translation offers is its *exit points*, and that is
//! all a JIT ever needed. The instruction-boundary version was answering a
//! question nobody asked.
//!
//! The set that comes back is [`LiftedCode::resume_points`], and it — not the
//! region's instruction grid — is what [`Translation::covers`] answers from, so
//! the cache never hands microlift a PC its dispatch would refuse. If one ever
//! slipped through, the result is now an `UnknownJump` naming itself
//! ([`TransferKind::NoResumePoint`]) rather than a program executing from the
//! wrong address.
//!
//! [`LifterConfig::resume_points`]: microlift::core::LifterConfig::resume_points
//! [`LifterConfig::strict_resume`]: microlift::core::LifterConfig::strict_resume
//! [`LiftedCode::resume_points`]: microlift::core::LiftedCode::resume_points
//! [`TransferKind::NoResumePoint`]: microlift::core::exit::TransferKind::NoResumePoint
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
use microlift::core::link::BlockLinkEntry;
use microlift::core::{LifterConfig, VmResumptionPoints, lift_to_code};

use crate::program::Program;
use crate::vm::VmError;

/// One lifted, compiled guest region.
pub struct Translation {
    /// Guest address the region begins at.
    pub start: u64,
    /// One past the last guest byte it covers.
    pub end: u64,
    /// The addresses it may be entered at, ascending — microlift's own account
    /// of which arms its entry dispatch has, not the region's instruction grid.
    ///
    /// A range test is not enough and neither is the grid: see the module note
    /// on what a resume point costs and why there are far fewer of them than
    /// there are instructions.
    entries: Vec<u64>,
    /// The compiled entry point. Takes a pointer to the guest context and
    /// returns having written an exit reason into it.
    entry: unsafe extern "C" fn(*mut RegisterContext),
    /// The address a block-link row must name to reach this translation, as a
    /// plain integer — a row is read by *compiled code* and so cannot hold a
    /// Rust function pointer, only its bits. See [`crate::thread::CodeWindow`].
    ///
    /// The same address as [`entry`](Self::entry), **except under a pinned link
    /// ABI** ([`crate::jit::pinned_link_abi`]), where a link enters the region's
    /// tail-convention body with the guest registers already in place and
    /// `entry` is the thunk that fills them from the context. Calling the body
    /// or linking to the thunk would each be a wrong-code bug, which is why
    /// microlift hands the two back separately rather than letting this be
    /// derived.
    code: u64,
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
        ctx.link_hit = 0;
        // SAFETY: `entry` is the finalized address of a Cranelift function whose
        // signature is `(*mut RegisterContext) -> ()` in the host C convention,
        // compiled by the backend this `Translation` owns and therefore still
        // mapped. The lifted code's only memory effects outside `ctx` are the
        // guest's own — the guest stack and its mapped images, all live for the
        // duration of the call.
        unsafe { (self.entry)(ctx as *mut RegisterContext) };
        follow_links(ctx)
    }

    /// Whether this translation can be entered at `pc`.
    pub fn covers(&self, pc: u64) -> bool {
        self.entries.binary_search(&pc).is_ok()
    }

    /// The row a block-link table needs to name this translation at `pc`.
    ///
    /// `image_base` is [`start`](Self::start) for the reason
    /// [`enter`](Self::enter) sets it there: the entry dispatch reconciles the
    /// PC against it, and the guest is identity-mapped.
    ///
    /// The caller is asserting [`covers`](Self::covers) — this does not check,
    /// because it is called on the path a window `put` is already on and the
    /// window's own key is where that guarantee comes from.
    pub fn link_row(&self, pc: u64) -> BlockLinkEntry {
        debug_assert!(
            self.covers(pc),
            "a link row would offer {pc:#x} to a translation that cannot be entered there"
        );
        BlockLinkEntry {
            guest_pc: pc,
            code: self.code,
            image_base: self.start,
            reserved: 0,
        }
    }
}

/// Run out the chain of regions the last exit resolved for itself, and report
/// how the last one ended.
///
/// **This is the whole host side of block linking.** A region that ends in a
/// `ret`, a jump or a call out of itself probes this thread's link table for the
/// address it continues at (see [`microlift::core::link`]); when it finds a row,
/// it names it in `link_hit` instead of leaving the host to look the address up.
/// All that is left here is to believe it: set the `image_base` the row carries
/// and call the code it names.
///
/// What is *not* done per link is the point of the exercise. The run loop's own
/// answer to the same exit — a code-window probe with a reference count, an exit
/// classification, the transfer bookkeeping, the pending-error check — was on
/// the order of a hundred and seventy-five cycles, against a guest function that
/// is frequently shorter than that. See `docs/HOTSPOTS.md`.
///
/// Which is also why the exits that *do* need the run loop must never be linked,
/// and they are not: microlift emits no probe for an import call, a syscall, a
/// fault or a return to the exit sentinel, and a bridge asks to be linked only
/// when it has nothing parked (see [`crate::bridge`]). A link is therefore never
/// a skipped step — it is a step that was only ever "enter what is at `rip`".
fn follow_links(ctx: &mut RegisterContext) -> ExitReason {
    while ctx.link_hit != 0 {
        // SAFETY: `link_hit` holds an address lifted code read out of the table
        // published in `ctx.link_table` — this thread's own, whose rows only
        // this thread writes, and which is alive for as long as the thread is.
        // A row's `code` is kept mapped by the `Arc` the window holds in the
        // same slot, and by `CodeCache::retired` if that slot is ever replaced.
        let row = unsafe { *(ctx.link_hit as *const BlockLinkEntry) };
        debug_assert_eq!(
            row.guest_pc, ctx.rip,
            "a link was taken for an address the region did not commit"
        );
        // Both counters, so that the two figures `docs/HOTSPOTS.md` is written
        // against — the entry *rate* and the exit *mix* — keep counting the
        // events they always counted. An exit that linked is still an exit of
        // some kind, and the mix is how this runtime decides what to work on
        // next; measured only over the entries that reached the run loop it
        // would describe the residue rather than the program.
        crate::stats::record_exit(crate::vm::classify(
            ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None),
            ctx.exit_code,
        ));
        crate::stats::record_linked_entry();
        ctx.image_base = row.image_base;
        ctx.exit_reason = ExitReason::None as u32;
        ctx.link_hit = 0;
        // SAFETY: `code` is the entry point of a compiled translation, of the
        // same signature and with the same effects as `Translation::enter`'s
        // own call; it is only ever written by `CodeWindow::put`, from the
        // `Translation` that had already been entered at this address.
        let code: unsafe extern "C" fn(*mut RegisterContext) =
            unsafe { std::mem::transmute(row.code as usize as *const u8) };
        unsafe { code(ctx as *mut RegisterContext) };
    }
    ExitReason::try_from(ctx.exit_reason).unwrap_or(ExitReason::None)
}

/// Translations, keyed by the guest address each begins at.
pub struct CodeCache {
    regions: BTreeMap<u64, Arc<Translation>>,
    /// Translations displaced from [`regions`](Self::regions) by a re-lift of
    /// the same address, kept so their compiled code stays mapped. See the
    /// insert in [`CodeCache::get_or_lift`] for why raw code addresses outlive
    /// the map that produced them.
    retired: Vec<Arc<Translation>>,
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
        config.resumption = VmResumptionPoints::ExitOnly;
        config.strict_resume = true;
        CodeCache {
            regions: BTreeMap::new(),
            retired: Vec::new(),
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

        let selecting = crate::stats::enabled().then(std::time::Instant::now);
        // A region always begins at the address it was made for, so its cache
        // key is `pc` and two entry points into one function are two keys. The
        // previous design preferred to begin at the enclosing function's first
        // instruction and had to detect and undo that preference here, because
        // a second entry asking for a region with the *same* key evicted the
        // first and the two then took turns re-compiling the function forever.
        let (region, bytes) = program
            .lift_region(pc)
            .ok_or(VmError::NoCode { pc, from: None })?;
        if let Some(selecting) = selecting {
            crate::stats::record_select(selecting.elapsed());
        }
        let start = region.range.start;

        // How long one region took to compile, when tracing.
        //
        // Not curiosity. A big region is not slow in proportion to its size:
        // Cranelift's register allocator is where the time goes, and it is
        // superlinear in live ranges, so one function of a hundred thousand
        // instructions can take *minutes* while ten thousand of a hundred take
        // no time at all. Terraria has such a function — Mono's `mono_method_to_ir`
        // is one enormous switch — and what it looks like from outside is a
        // process at 100% CPU that has stopped: no output, no fault, and
        // `ROSETTA3_TRACE_PC` silent, because the run loop has not entered a
        // region in minutes and never will until this returns.
        //
        // A profile of the host says `regalloc2::ion::Env::run` and nothing
        // about *which* guest function, which is the one thing needed.
        let started = crate::shims::vmmap::tracing().then(std::time::Instant::now);

        // Where control can arrive from outside without any lift knowing: a
        // function reached only through a pointer. See the module note.
        //
        // `pc` is in the set unconditionally, and that is not belt and braces —
        // it is what makes the loop terminate. A region is often lifted from the
        // *enclosing function's* first instruction so that every call site
        // shares one translation, which leaves `pc` somewhere in the middle with
        // no guarantee of being a leader. Caching a translation that cannot be
        // entered at the address it was made for would send the run loop back
        // for the same `pc` forever.
        let mut declared = program.function_starts_in(region.range.clone());
        declared.push(pc);
        declared.sort_unstable();
        declared.dedup();

        // A region that will not compile is refused outright. There used to be
        // a prefix bisection here — up to a dozen extra lift-and-compile
        // attempts, cutting the region shorter until one stuck — and what it
        // existed for was Mono's inline call arguments being decoded as
        // instructions. The region walk no longer decodes them: a non-entry
        // block that will not decode is dropped, and in writable memory a call
        // ends its block outright. See [`crate::region`].
        let compiled = match compile_region(&self.config, &bytes, &region, &declared) {
            Ok(compiled) => compiled,
            Err(source) => {
                crate::stats::record_refused();
                if crate::shims::vmmap::tracing() {
                    let head: Vec<String> =
                        bytes.iter().take(64).map(|b| format!("{b:02x}")).collect();
                    eprintln!(
                        "rosetta3: region {start:#x}..{:#x} would not compile, bytes {}",
                        region.range.end,
                        head.join(" ")
                    );
                }
                return Err(VmError::Lift {
                    pc,
                    start,
                    source,
                    at: program.locate(start),
                });
            }
        };
        let end = region.range.end;

        if let Some(started) = started {
            let took = started.elapsed();
            // Only the ones worth reading. A warm program lifts thousands of
            // regions and a line each would bury the one that matters.
            if took.as_millis() >= 50 {
                eprintln!(
                    "rosetta3: compiled {} ({start:#x}..{end:#x}, {} instruction(s), \
                     {} byte(s)) in {took:.1?}",
                    program
                        .locate(start)
                        .unwrap_or_else(|| "generated code".to_string()),
                    region.instructions(),
                    end - start,
                );
            }
        }

        // SAFETY: `compile` returns the finalized entry of a function whose
        // signature the lifter fixed as `fn(*mut RegisterContext)` in the host
        // calling convention.
        let entry: unsafe extern "C" fn(*mut RegisterContext) =
            unsafe { std::mem::transmute(compiled.code) };

        debug_assert!(
            compiled.entries.binary_search(&pc).is_ok(),
            "translation {start:#x}..{end:#x} cannot be entered at the {pc:#x} \
             it was lifted for"
        );
        // Where this region's compiled code went, so a fault inside it can be
        // named as a *guest* range rather than as an address in an anonymous
        // mapping. See [`crate::fault`].
        // The *body*, not the entry: under a pinned ABI those differ, and the
        // map's host offsets are relative to the body, which is also where every
        // guest instruction ended up. A fault inside the entry thunk - which
        // does nothing but load and store the context - is then unattributed,
        // which is the right trade for keeping every guest address nameable.
        crate::fault::placed(
            compiled.link_target as usize as u64,
            start,
            end,
            pc,
            &compiled.map,
        );
        crate::lldb::register(
            compiled.link_target as usize as u64,
            start,
            end,
            &program
                .locate(start)
                .unwrap_or_else(|| "generated code".to_string()),
            &compiled.guest_bytes,
            &compiled.map,
        );

        let translation = Arc::new(Translation {
            start,
            end,
            entries: compiled.entries,
            entry,
            code: compiled.link_target as usize as u64,
            _backend: compiled.backend,
        });
        // A replaced translation is *retired*, not dropped. Its code address may
        // already be in some thread's block-link table, where it is a raw
        // pointer with no reference count behind it — and where the only thing
        // keeping the pages mapped is this `Arc`. Two translations of one
        // address are interchangeable (see the module note), so nothing needs
        // the old one to go away; it needs never to become unmapped while a
        // table still names it.
        if let Some(replaced) = self.regions.insert(start, Arc::clone(&translation)) {
            self.retired.push(replaced);
        }
        //eprintln!("MISS at {pc:#x} - regions: {}", self.len());
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

/// Lift and compile `region`'s `bytes` into native code.
///
/// The two halves are one operation for every caller's purposes, and keeping
/// them together is what lets a region that *lifts* but does not *compile* be
/// salvaged the same way as one that does not lift. The distinction is not
/// academic: an instruction the lifter mistranslates into invalid IR is caught
/// by Cranelift's verifier at compile time, which is a stage after the one the
/// first version of this retried at.
///
/// `declared` are the addresses the host knows control can arrive at from
/// outside — the recorded function starts within the region. See the module
/// note on resume points.
fn compile_region(
    config: &microlift::core::LifterConfig,
    bytes: &[u8],
    region: &crate::region::Region,
    declared: &[u64],
) -> microlift::Result<Compiled> {
    let clock = crate::stats::enabled().then(std::time::Instant::now);
    crate::stats::record_extent(region.range.start, region.range.end);

    // Cheap: the two fields with a program's worth of data in them — the
    // import map and the immutable-memory segments — are behind `Arc`s, so this
    // copies pointers rather than the guest's whole image. It did not always,
    // and that clone was 7% of a process's wall clock; see
    // `LifterConfig::segments`.
    let mut config = config.clone();
    // Declared per region rather than once, because the answer is per region:
    // which function starts fall inside these bytes. Microlift adds the
    // region's entry and its call resume points to whatever is here.
    config.resume_points = declared.to_vec();
    // Which of these bytes are code. The slice spans the region's blocks and
    // the holes between them — a jump table, alignment padding, a call's inline
    // argument — and this is what keeps the holes out of the lift. See
    // [`crate::region`].
    config.block_ranges = region.blocks.clone();

    let lifted = lift_to_code::<X86_64>(bytes, region.range.start, config)?;
    let entries = lifted.resume_points.clone();
    let lift_took = clock.map(|c| c.elapsed());
    let mut backend = crate::jit::backend()?;
    let (region_code, map) = backend.compile_lifted_region(lifted)?;
    if let (Some(clock), Some(lift)) = (clock, lift_took) {
        crate::stats::record_lift(
            region.instructions(),
            bytes.len(),
            lift,
            clock.elapsed() - lift,
        );
    }
    Ok(Compiled {
        backend,
        code: region_code.entry,
        link_target: region_code.link_target,
        map,
        guest_bytes: crate::lldb::enabled()
            .then(|| bytes.to_vec())
            .unwrap_or_default(),
        entries,
    })
}

/// One region through the lifter and the compiler, with the one fact about it
/// that only the lift knows.
struct Compiled {
    backend: microlift::backend::CraneliftBackend,
    code: *const u8,
    /// What a block-link row names — see [`Translation::code`].
    link_target: *const u8,
    /// Host offset → guest instruction, so a fault inside this code can name
    /// the instruction rather than the region. See [`crate::fault`].
    map: microlift::core::srcloc::GuestCodeMap,
    /// Guest bytes retained only so the opt-in LLDB listing can show x86-64.
    guest_bytes: Vec<u8>,
    /// Where the compiled code may be entered — see [`Translation::covers`].
    entries: Vec<u64>,
}
