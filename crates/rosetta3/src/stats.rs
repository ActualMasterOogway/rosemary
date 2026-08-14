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
