//! Where translation time actually goes.
//!
//! "Recompiling is slow" is not a thing this runtime can act on. Lifting a
//! region is four distinct costs — deciding which bytes to hand the lifter,
//! decoding them into IR, letting Cranelift compile that IR, and the cache
//! lookups around all of it — and they differ by more than an order of
//! magnitude from each other. A change that halves the first is invisible if
//! the third is 95% of the total, and the only way to know which is which is to
//! measure them separately.
//!
//! `ROSETTA3_STATS=1` turns this on. Everything here is atomics behind one
//! cached `bool`, so it costs a relaxed load per region entry when off.
//!
//! The counters are process-global rather than per-`Vm` for the same reason the
//! code cache's registry is: guest threads lift into the same cache, and a
//! per-thread total would answer a question nobody asked.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Whether `ROSETTA3_STATS` asked for translation to be accounted for.
///
/// Read once. The alternative is a `getenv` per lift, which would be a cost
/// large enough to change what it is measuring.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ROSETTA3_STATS").is_some())
}

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        $(static $name: AtomicU64 = AtomicU64::new(0);)*
    };
}

counters! {
    REGIONS, INSTRUCTIONS, BYTES,
    SELECT_NS, LIFT_NS, COMPILE_NS,
    REFUSED, TAIL_EXITS,
    ENTRIES, WINDOW_MISSES, TAIL_IMPORTS, LINKED,
}

// ------------------------------------------------------------ the exit mix ---

/// How a region entry ended.
///
/// Translation time is one question and **exit rate** is the other, and until
/// this existed only the first had an instrument. A profile of A Hat in Time
/// put 44% of steady-state CPU in this runtime's own Rust with translation at
/// 0.02% of it, which says the cost is the seam between regions rather than the
/// code inside them — but *which* seam is a different question from *how much*,
/// and the run loop's answer differs by more than an order of magnitude between
/// these arms. A `Return` that leaves a leaf function is a lookup and a
/// re-entry; an `Indirect` is that plus three map probes and a guest stack
/// write.
///
/// Named here rather than reusing microlift's `ExitReason` on purpose: the
/// interesting split is *inside* `UnknownJump`, whose `TransferKind` separates
/// "the guest jumped somewhere" from "the translation could not serve this
/// address", and those are not the same work item.
#[derive(Clone, Copy)]
pub enum Exit {
    /// The translation's own terminating block: a `ret` to the sentinel this VM
    /// pushed, or a return into a region already entered.
    Return,
    /// A `ret` that popped an address outside the region — the ordinary way a
    /// leaf function ends, and one region entry each time.
    ReturnOut,
    /// An unconditional `jmp` to a static address outside the region.
    DirectJump,
    /// A conditional branch whose taken edge left the region.
    CondJump,
    /// Control ran off the bottom of the bytes the lifter was given.
    Fallthrough,
    /// A direct `call` to guest code that was not lifted into this region.
    DirectCall,
    /// The translation declined to serve the PC — a region cut mid-instruction,
    /// or an entry point its dispatch was never offered. See
    /// [`record_tail_exit`].
    RegionTail,
    /// A tail `jmp` through a register or a memory operand.
    IndirectJump,
    /// Any other transfer out of the region.
    OtherJump,
    /// An intercepted `call` to a registered import.
    Import,
    /// An indirect `call` — a virtual call, or a call through a pointer.
    Indirect,
    /// `cpuid`, `xgetbv`, and the rest of the resumable one-instruction exits.
    Instruction,
    /// Anything else, which is an error path.
    Other,
}

impl Exit {
    /// Every arm, in report order.
    const ALL: [Exit; 13] = [
        Exit::Return,
        Exit::ReturnOut,
        Exit::DirectJump,
        Exit::CondJump,
        Exit::Fallthrough,
        Exit::DirectCall,
        Exit::RegionTail,
        Exit::IndirectJump,
        Exit::OtherJump,
        Exit::Import,
        Exit::Indirect,
        Exit::Instruction,
        Exit::Other,
    ];

    fn label(self) -> &'static str {
        match self {
            Exit::Return => "terminating block",
            Exit::ReturnOut => "ret out of region",
            Exit::DirectJump => "direct jmp",
            Exit::CondJump => "conditional branch",
            Exit::Fallthrough => "fallthrough",
            Exit::DirectCall => "direct call",
            Exit::RegionTail => "region tail",
            Exit::IndirectJump => "indirect jmp",
            Exit::OtherJump => "other jump",
            Exit::Import => "import call",
            Exit::Indirect => "indirect call",
            Exit::Instruction => "instruction",
            Exit::Other => "other",
        }
    }
}

/// One counter per [`Exit`]. A fixed array rather than a map because this is
/// incremented once per region entry on every guest thread, and a lock there
/// would be measuring the instrument.
static EXITS: [AtomicU64; Exit::ALL.len()] = [const { AtomicU64::new(0) }; Exit::ALL.len()];

/// One region entry: the event whose *rate* is the whole question.
///
/// Gated on [`enabled`] rather than counted always, and that is not a
/// micro-optimisation. At the measured rate — millions a second across several
/// guest threads — an unconditional `fetch_add` on one cache line is a
/// contended store the guest threads would queue behind, which would slow the
/// program by more than the thing being counted costs.
pub fn record_entry(missed_window: bool) {
    if !enabled() {
        return;
    }
    ENTRIES.fetch_add(1, Relaxed);
    if missed_window {
        WINDOW_MISSES.fetch_add(1, Relaxed);
    }
}

/// A region entry the run loop never saw: one region **linked** to the next,
/// resolving the successor itself out of this thread's block-link table.
///
/// Counted into [`ENTRIES`] as well as separately, so that the entry *rate* —
/// the figure `docs/HOTSPOTS.md` is written against — still counts the same
/// event it always did, and stays comparable across the change that introduced
/// linking. The separate counter is the new question: what share of entries
/// never reached Rust.
pub fn record_linked_entry() {
    if !enabled() {
        return;
    }
    ENTRIES.fetch_add(1, Relaxed);
    LINKED.fetch_add(1, Relaxed);
}

/// A region entry that turned out to be a *tail call to an import*: the run
/// loop's first question, asked before every entry and answered "yes" only
/// here. Its cost when the answer is "no" is what
/// [`crate::Program::import_at`] is doing at the top of a profile.
pub fn record_tail_import() {
    if enabled() {
        TAIL_IMPORTS.fetch_add(1, Relaxed);
    }
}

/// How one region entry ended.
pub fn record_exit(exit: Exit) {
    if enabled() {
        EXITS[exit as usize].fetch_add(1, Relaxed);
    }
}

/// One completed region selection: how long it took to decide the extent.
pub fn record_select(took: Duration) {
    SELECT_NS.fetch_add(took.as_nanos() as u64, Relaxed);
}

/// How many regions between progress reports.
///
/// A run that is killed rather than exiting — which is every measured run, since
/// a game does not finish — would otherwise report nothing at all, and a single
/// figure at the end could not show *when* the cost was paid anyway.
const REPORT_EVERY: u64 = 2048;

/// One completed translation.
pub fn record_lift(instructions: usize, bytes: usize, lift: Duration, compile: Duration) {
    let n = REGIONS.fetch_add(1, Relaxed) + 1;
    INSTRUCTIONS.fetch_add(instructions as u64, Relaxed);
    BYTES.fetch_add(bytes as u64, Relaxed);
    LIFT_NS.fetch_add(lift.as_nanos() as u64, Relaxed);
    COMPILE_NS.fetch_add(compile.as_nanos() as u64, Relaxed);
    if n.is_multiple_of(REPORT_EVERY) {
        report();
    }
}

/// A region that could not be translated at all.
pub fn record_refused() {
    REFUSED.fetch_add(1, Relaxed);
}

/// An exit taken because the translation could not serve the PC it was entered
/// at — either the instruction there runs past the bytes it was given, or its
/// dispatch was never offered that address. A run full of them means the region
/// walk is cutting in the wrong places; see [`crate::region`].
pub fn record_tail_exit() {
    TAIL_EXITS.fetch_add(1, Relaxed);
}

/// The pages a lifted region touched, at both page sizes that matter.
///
/// How *dense* the code a program actually reaches is within the pages holding
/// it, which is a figure no amount of reasoning produces and one counter does.
/// It decided against page-bounded lifting when that was a live option, and it
/// is what says whether a persistent code cache keyed by page would be storing
/// mostly code or mostly padding.
static PAGES: std::sync::Mutex<
    Option<(
        std::collections::BTreeSet<u64>,
        std::collections::BTreeSet<u64>,
    )>,
> = std::sync::Mutex::new(None);

/// Record which pages a region of `[start, end)` fell in.
pub fn record_extent(start: u64, end: u64) {
    if !enabled() {
        return;
    }
    let mut guard = PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let (small, large) = guard.get_or_insert_with(Default::default);
    for page in (start >> 12)..=(end.saturating_sub(1) >> 12) {
        small.insert(page);
    }
    for page in (start >> 14)..=(end.saturating_sub(1) >> 14) {
        large.insert(page);
    }
}

/// When the process started translating, so the report can say what share of
/// the run went into it.
fn origin() -> Instant {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// Start the clock the report is a share of.
///
/// Called from the driver before the guest runs. Idempotent, and calling it
/// late only makes the reported share pessimistic.
pub fn start() {
    if enabled() {
        origin();
        // A run that ends in the guest's own `exit` never returns to the
        // driver, and a report printed only on the way out would therefore be
        // printed for almost no run at all.
        install_exit_report();
    }
}

/// Print the report when the process ends, however it ends.
fn install_exit_report() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Relaxed) {
        return;
    }
    // `atexit` rather than a `Drop` guard: the guest calls `exit`, which runs
    // the host's `atexit` handlers and never unwinds this crate's frames.
    extern "C" fn report_at_exit() {
        report();
    }
    // SAFETY: `atexit` takes an `extern "C" fn()`, which this is, and the
    // handler touches only process-global atomics and `stderr`.
    unsafe { libc::atexit(report_at_exit) };
}

/// Print what translation cost, if it was asked for.
pub fn report() {
    if !enabled() || REGIONS.load(Relaxed) == 0 {
        return;
    }
    let wall = origin().elapsed();
    let ns = |c: &AtomicU64| Duration::from_nanos(c.load(Relaxed));
    let (select, lift, compile) = (ns(&SELECT_NS), ns(&LIFT_NS), ns(&COMPILE_NS));
    let total = select + lift + compile;
    let regions = REGIONS.load(Relaxed);
    let instructions = INSTRUCTIONS.load(Relaxed);
    let share = |d: Duration| 100.0 * d.as_secs_f64() / wall.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "rosetta3: stats  {regions} region(s), {instructions} instruction(s), \
         {} byte(s), {:.1} instr/region",
        BYTES.load(Relaxed),
        instructions as f64 / regions as f64,
    );
    eprintln!(
        "rosetta3: stats  select {select:.2?} ({:.1}%)  lift {lift:.2?} ({:.1}%)  \
         compile {compile:.2?} ({:.1}%)  total {total:.2?} ({:.1}%) of {wall:.2?}",
        share(select),
        share(lift),
        share(compile),
        share(total),
    );
    eprintln!(
        "rosetta3: stats  {} refused, {} tail exit(s)",
        REFUSED.load(Relaxed),
        TAIL_EXITS.load(Relaxed),
    );
    // The exit mix, which is the other half of "why is this slow" and the half
    // translation time cannot answer. Printed as a rate as well as a count:
    // a share says which arm to work on and a rate says whether any of them is
    // worth working on at all.
    let entries = ENTRIES.load(Relaxed);
    if entries > 0 {
        let per_second = entries as f64 / wall.as_secs_f64().max(f64::EPSILON);
        eprintln!(
            "rosetta3: stats  {entries} region entr(ies), {:.2}M/s, {} window miss(es) ({:.2}%), \
             {} tail call(s) to an import ({:.2}%)",
            per_second / 1e6,
            WINDOW_MISSES.load(Relaxed),
            100.0 * WINDOW_MISSES.load(Relaxed) as f64 / entries as f64,
            TAIL_IMPORTS.load(Relaxed),
            100.0 * TAIL_IMPORTS.load(Relaxed) as f64 / entries as f64,
        );
        // How many of those entries the run loop never saw. This is the figure
        // block linking is judged on: an entry that linked paid a compare and an
        // indirect call, and one that did not paid the run loop.
        //
        // Under a **pinned** link ABI there is no such figure, and the reason is
        // the mechanism working: a link is a branch inside translated code, so
        // nothing on that path can count without putting a contended store on
        // it. What `entries` then counts is *chains* — one host entry, however
        // many regions ran inside it — and saying so is better than printing a
        // zero that reads as "linking stopped happening". See
        // [`crate::jit::pinned_link_abi`].
        match crate::jit::pinned_link_abi() {
            Some(abi) => eprintln!(
                "rosetta3: stats  entries above are chains, not regions: links are branches \
                 under a pinned ABI of {} register(s) and are not counted",
                abi.len(),
            ),
            None => {
                let linked = LINKED.load(Relaxed);
                eprintln!(
                    "rosetta3: stats  {linked} linked ({:.2}%), {} through the run loop",
                    100.0 * linked as f64 / entries as f64,
                    entries - linked,
                );
            }
        }
        let mut mix: Vec<(u64, &str)> = Exit::ALL
            .iter()
            .map(|&e| (EXITS[e as usize].load(Relaxed), e.label()))
            .filter(|&(n, _)| n > 0)
            .collect();
        mix.sort_by_key(|&(n, _)| std::cmp::Reverse(n));
        eprintln!(
            "rosetta3: stats  exits by kind  {}",
            mix.iter()
                .map(|(n, l)| format!("{l} {:.1}%", 100.0 * *n as f64 / entries as f64))
                .collect::<Vec<_>>()
                .join("  "),
        );
    }
    // Whether the import stubs did anything, which is three questions and not
    // one. See [`crate::trampoline::counts`].
    if let Some((made, substituted, reached)) = crate::trampoline::counts() {
        eprintln!(
            "rosetta3: stats  {made} trampoline(s), {substituted} slot(s) substituted, \
             {reached} reached"
        );
    }
    if let Some((small, large)) = PAGES.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        eprintln!(
            "rosetta3: stats  footprint {} page(s) of 4 KiB, {} of 16 KiB \
             ({} / {} byte(s) if lifted whole)",
            small.len(),
            large.len(),
            small.len() * 4096,
            large.len() * 16384,
        );
    }
}
