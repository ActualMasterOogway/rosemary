//! Where the *guest* spends its time.
//!
//! ## The half of the profile that has always been blank
//!
//! A sample of this process lands at a host `pc`. If that `pc` is in this
//! runtime's Rust, or in one of the host's libraries, `sample`, Instruments and
//! `dladdr` all name it. If it is in **translated guest code** — which is where
//! a working translation spends most of its time by construction — there is no
//! image, no symbol and no line table, because every region is its own
//! Cranelift module in its own anonymous mapping. So the two halves of a
//! profile are not comparable and the half that matters is empty.
//!
//! [M8.1](../../../docs/ROADMAP.md) is the record of what that costs: a 2.4×
//! microbenchmark win that the application it was built for could not detect,
//! and no way to say whether the seam it optimised was on the path at all.
//! Every remaining performance candidate is a plausible story with no
//! measurement behind it, which is why this exists before any of them.
//!
//! ## What it does
//!
//! A thread of its own, at `ROSETTA3_PROFILE` hertz, walks the guest threads
//! [`crate::thread`] has registered and for each one suspends it, reads
//! `ARM_THREAD_STATE64`, resumes it, and attributes the `pc`. Two questions in
//! that order:
//!
//! - **`dladdr` first.** A `pc` inside a real image is host code, full stop, and
//!   the answer comes with an image and a symbol. This is what separates "in
//!   this runtime's Rust" from "in the host's libraries" — the two are the same
//!   colour in every existing tool and are completely different work items.
//! - **[`crate::fault::attribute`] otherwise.** No image means an anonymous
//!   mapping, which here means JIT code, and the source-location map already
//!   kept for the crash path turns the `pc` into the *guest instruction* it was
//!   lifted from.
//!
//! The histogram that comes out is the one this project has never had: guest
//! addresses, resolved through [`crate::program::Program::locate`] to
//! `image+offset`, which is the form that survives across runs and can be
//! looked up in a disassembler.
//!
//! ## Which of the two rankings to believe
//!
//! Regions, not instructions. Cranelift attributes no source location to a
//! prologue, an epilogue, the entry dispatch, or to anything its optimiser
//! produced without one, and at `opt_level=speed` — the default here, because
//! optimising is what *deletes* the emulation rather than trimming it — that
//! last category is most of the gap. Measured between 2% and 47% of running
//! samples on one small fixture. Every one of those samples still carries a
//! region, so the region rollup is complete and the instruction ranking is a
//! lead worth following rather than a share to quote.
//!
//! ## Why not `sample`, or Instruments
//!
//! Because neither symbolicates JIT code, and that is the whole question. They
//! remain worth running for the host half and for what self-sampling cannot
//! reach — cycles, branch mispredictions, cache misses, and Apple Processor
//! Trace's exact branch history. [`write_map`] is what joins the two: it emits
//! the host-address-to-guest-instruction table an *external* tool needs, so a
//! trace recorded by `xctrace` can be resolved against guest addresses after the
//! fact. See `docs/PROFILING.md`.
//!
//! ## Discipline while a thread is stopped
//!
//! Between `thread_suspend` and `thread_resume` the sampler makes exactly two
//! Mach calls and touches nothing else. No allocation, no lock, no formatting:
//! a suspended thread may hold the allocator's lock or `stderr`'s, and a
//! sampler that waited on either would deadlock the process it was measuring.
//! Attribution, which does allocate and does take a lock, happens after the
//! resume.
//!
//! One thread is stopped at a time rather than the world, because a histogram
//! does not care that its samples are skewed by a few microseconds relative to
//! each other, and stopping the world at a kilohertz would change what it is
//! measuring.
//!
//! ## Configuration
//!
//! - `ROSETTA3_PROFILE=1` turns it on at the default rate; `=<hz>` sets it.
//! - `ROSETTA3_PROFILE_EVERY=<seconds>` prints a running report that often,
//!   default 10, `0` for only at exit. A measured run is killed rather than
//!   exited — a game does not finish — so a figure printed only on the way out
//!   is a figure printed for almost no run at all. Same reasoning as
//!   [`crate::stats`].
//! - `ROSETTA3_PROFILE_OUT=<path>` writes the full ranked histogram there
//!   instead of only the top of it to stderr.
//! - `ROSETTA3_PROFILE_MAP=<path>` writes the host→guest address map, for an
//!   external trace to be resolved against. `ROSETTA3_PROFILE_MAP=<path>` plus
//!   `ROSETTA3_PROFILE_MAP_INSTRUCTIONS=1` emits one line per guest
//!   instruction rather than one per region.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use static_init::dynamic;

use crate::fault::Attribution;

// ---------------------------------------------------------- configuration ---

/// Samples per second per guest thread, when `ROSETTA3_PROFILE` names no rate.
///
/// A kilohertz is the rate at which a sample costs about a microsecond of Mach
/// calls out of every millisecond of the thread it is measuring — a tenth of a
/// percent, which is below the noise of the thing being measured — and still
/// puts thousands of samples into a ten-second window.
const DEFAULT_HZ: u32 = 1000;

/// Seconds between running reports, when `ROSETTA3_PROFILE_EVERY` names none.
const DEFAULT_EVERY: u64 = 10;

/// How many lines of each ranked list stderr gets. The file, when one is asked
/// for, gets all of them.
const TOP: usize = 15;

/// The rate `ROSETTA3_PROFILE` asked for, or `None` if it asked for nothing.
///
/// Read once: a `getenv` per sample would be a cost inside the measurement.
fn configured_hz() -> Option<u32> {
    #[dynamic]
    static HZ: Option<u32> = {
        let raw = std::env::var("ROSETTA3_PROFILE").ok()?;
        match raw.trim() {
            "" | "0" | "no" | "off" => None,
            // `1` is "on", not "one sample a second": the variable is a switch
            // first and a rate second, and every other `ROSETTA3_*` flag in
            // this runtime is turned on by setting it to 1.
            "1" | "yes" | "on" => Some(DEFAULT_HZ),
            n => n.parse().ok().filter(|&hz| hz > 0),
        }
    };
    *HZ
}

/// Whether sampling was asked for.
pub fn enabled() -> bool {
    configured_hz().is_some()
}

/// An environment variable parsed as a number, with a default.
fn number(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

// --------------------------------------------------------------- the Mach ---

/// `KERN_SUCCESS`.
const KERN_SUCCESS: i32 = 0;

/// `ARM_THREAD_STATE64`, from `<mach/arm/thread_status.h>`.
const ARM_THREAD_STATE64: i32 = 6;

/// `arm_thread_state64_t` in 32-bit words: `__x[29]`, `__fp`, `__lr`, `__sp`,
/// `__pc` — thirty-three doublewords — then `__cpsr` and its padding.
const ARM_THREAD_STATE64_COUNT: u32 = 68;

/// Index of `__pc` within that state, in doublewords. Twenty-nine `__x`
/// registers, then `__fp`, `__lr` and `__sp`. The same offset
/// [`crate::fault::host_pc`] measured against `<sys/ucontext.h>`, in different
/// units.
const PC_WORD: usize = 32;

/// `THREAD_BASIC_INFO`, and its size in 32-bit words.
const THREAD_BASIC_INFO: u32 = 3;
const THREAD_BASIC_INFO_COUNT: u32 = 10;

/// Index of `run_state` in `thread_basic_info_data_t`: two `time_value_t` (two
/// `int` each), `cpu_usage`, `policy`, then `run_state`.
const RUN_STATE_WORD: usize = 6;

/// `TH_STATE_RUNNING`. Everything else — waiting, stopped, halted — is a thread
/// that is not consuming the CPU this is trying to account for.
const TH_STATE_RUNNING: i32 = 1;

/// `mach_timebase_info_data_t`: the rational number converting the absolute
/// clock's ticks to nanoseconds.
#[repr(C)]
#[derive(Clone, Copy)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
    fn mach_wait_until(deadline: u64) -> i32;
}

/// Nanoseconds as ticks of the absolute clock.
///
/// `std::thread::sleep` is not usable here and the reason is measured rather
/// than assumed: asked for a kilohertz it delivered about 190 Hz, because a
/// `nanosleep` is rounded up to the scheduler's own granularity and then
/// returns whenever the thread is next picked. `mach_wait_until` schedules
/// against the same clock the deadline is expressed in, which is what a
/// profiler's timer has to do — a sampler that silently runs at a fifth of its
/// stated rate is a sampler whose stated rate is a lie.
fn ticks(nanos: u64) -> u64 {
    #[dynamic]
    static BASE: (u32, u32) = {
        let mut base = Timebase { numer: 1, denom: 1 };
        // SAFETY: `base` is a live local of the type the call fills in.
        match unsafe { mach_timebase_info(&mut base) } == KERN_SUCCESS {
            true => (base.numer.max(1), base.denom.max(1)),
            false => (1, 1),
        }
    };
    let (numer, denom) = *BASE;
    // ticks = nanos * denom / numer. Widened, because on Apple silicon the
    // ratio is 125/3 and a run measured in seconds overflows sixty-four bits
    // of nanoseconds-times-three long before it overflows this.
    (u128::from(nanos) * u128::from(denom) / u128::from(numer)) as u64
}

// SAFETY (for every call below): these are the Mach thread routines exactly as
// `<mach/thread_act.h>` declares them. Each is given a port this process owns,
// a flavour whose buffer is sized by the matching `_COUNT` constant above, and
// an in-out count initialised to that size.
unsafe extern "C" {
    fn thread_suspend(target: u32) -> i32;
    fn thread_resume(target: u32) -> i32;
    fn thread_get_state(target: u32, flavor: i32, state: *mut u32, count: *mut u32) -> i32;
    fn thread_info(target: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
}

/// One thread's `pc`, and whether it was on a CPU when it was read.
///
/// `None` when the thread could not be stopped — it has exited between the
/// registry naming its port and this call, which is the ordinary race and not
/// an error.
///
/// **The run state is read before the suspend, and that ordering is the whole
/// of whether this measures anything.** `thread_suspend` is exactly the
/// operation that takes a thread off a CPU, so a `thread_info` after it reports
/// `TH_STATE_WAITING` for every thread in the process — including the one in a
/// hot loop. The first version of this function asked in the other order and
/// classified 683 of 690 samples as blocked while the guest was spinning.
///
/// Between the suspend and the resume there is one Mach trap into a fixed stack
/// buffer. See the module note on why that matters.
fn sample_thread(port: u32) -> Option<(u64, bool)> {
    let mut info = [0i32; THREAD_BASIC_INFO_COUNT as usize];
    let mut info_count = THREAD_BASIC_INFO_COUNT;
    // SAFETY: `port` names a thread in this task and the buffer is exactly
    // `info_count` `int`s, which is what `THREAD_BASIC_INFO` writes.
    let got_info =
        unsafe { thread_info(port, THREAD_BASIC_INFO, info.as_mut_ptr(), &mut info_count) };
    // A thread whose run state could not be read is counted as running:
    // dropping the sample would understate the very thing being measured, and
    // the failure is not one that has been seen.
    let running = got_info != KERN_SUCCESS || info[RUN_STATE_WORD] == TH_STATE_RUNNING;

    // SAFETY: as above. A failed suspend leaves nothing to undo, which is why
    // the early return is before the resume.
    if unsafe { thread_suspend(port) } != KERN_SUCCESS {
        return None;
    }
    let mut state = [0u32; ARM_THREAD_STATE64_COUNT as usize];
    let mut count = ARM_THREAD_STATE64_COUNT;
    // SAFETY: the buffer is exactly `count` words of the flavour asked for.
    let got_state =
        unsafe { thread_get_state(port, ARM_THREAD_STATE64, state.as_mut_ptr(), &mut count) };
    // SAFETY: balances the suspend above, which succeeded.
    unsafe { thread_resume(port) };

    if got_state != KERN_SUCCESS {
        return None;
    }
    let pc = u64::from(state[PC_WORD * 2]) | u64::from(state[PC_WORD * 2 + 1]) << 32;
    Some((pc, running))
}

// ------------------------------------------------------------- the counts ---

/// One host symbol the guest's threads were found inside.
struct HostSite {
    count: u64,
    /// Leaf name of the image, which is what a reader recognises.
    image: String,
    /// The symbol, or `+offset` into the image when the symbol table has none.
    symbol: String,
    /// Whether the image is this runtime rather than one of the host's
    /// libraries. The distinction the roadmap's first question turns on.
    runtime: bool,
}

/// Everything the sampler has seen. One lock, taken by the sampler thread after
/// each resume and by whoever prints a report.
#[derive(Default)]
struct Counts {
    /// Samples taken on a thread that was on a CPU. The denominator every share
    /// below is a share of.
    running: u64,
    /// Samples taken on a thread that was parked. Counted and reported, never
    /// mixed in: a guest thread waiting on a condition variable is not time the
    /// translation is responsible for, and silently dropping it would make the
    /// run look shorter than it was.
    blocked: u64,
    /// Guest instruction address → samples. The histogram this module exists
    /// for.
    guest: HashMap<u64, u64>,
    /// First guest address of a region → samples anywhere in its translation,
    /// including the parts no guest instruction was attributed to. The rollup
    /// that survives a region whose source map was dropped.
    region: HashMap<u64, u64>,
    /// Samples inside a translation's own attributed extent that no guest
    /// instruction covered: a prologue, the entry dispatch, a gap between two
    /// ranges, or a region whose map was dropped past the budget.
    translated_unnamed: u64,
    /// Samples past everything a translation attributed but close enough to be
    /// its epilogue. Counted apart because the region here is an inference,
    /// not a fact — see [`Attribution::Translated::beyond`].
    translated_beyond: u64,
    /// Samples in executable memory belonging to neither a loaded image nor a
    /// translation. Kept apart from both because it is the bucket that says
    /// this module's model of the process is incomplete, and folding it into
    /// either would hide that.
    unknown: u64,
    /// Host symbol start address → what was found there.
    host: HashMap<u64, HostSite>,
    /// Samples in this runtime's own code, and in the host's libraries.
    runtime: u64,
    library: u64,
    /// Guest threads found in the registry on the most recent sweep.
    threads: usize,
}

static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);

/// When sampling started, so a share can be a share of something.
static STARTED: Mutex<Option<Instant>> = Mutex::new(None);

/// Sweeps completed, which is what the sample rate is checked against.
static SWEEPS: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds spent inside a sweep rather than waiting for the next one.
///
/// A profiler that does not report its own cost is asking to be believed. This
/// is also the number that says whether a rate the sampler failed to reach was
/// the timer's fault or its own — and the first time it was asked, the answer
/// decided which of the two to fix.
static SWEEP_NS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------- attribution -----

/// This binary's own load address, so a host sample can be told from a library
/// one.
///
/// Taken from `dladdr` of a function in this crate rather than from
/// `_dyld_get_image_header(0)`, because the two disagree when the process was
/// re-executed out of a bundle mirror — see [`crate::bundle`] — and the
/// question here is "is this code ours", which the address of our own code
/// answers directly.
fn self_base() -> u64 {
    static BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: `self_base` is a live function in this image and `info` is a
        // live local of the right type.
        let ok = unsafe { libc::dladdr(self_base as *const libc::c_void, &mut info) };
        match ok != 0 {
            true => info.dli_fbase as u64,
            false => 0,
        }
    })
}

/// A `dli_*` string, or `""`. Null is what `dladdr` reports for a symbol it
/// could not name, which is common inside a stripped library.
fn name(raw: *const libc::c_char) -> &'static str {
    if raw.is_null() {
        return "";
    }
    // SAFETY: `dladdr` fills these with pointers into the image's own string
    // tables, which live as long as the image, which is the process.
    unsafe { CStr::from_ptr(raw) }.to_str().unwrap_or("")
}

/// Fold one `pc` into the counts.
///
/// Runs on the sampler thread with the target already resumed, so it may
/// allocate and take locks.
fn attribute(counts: &mut Counts, pc: u64) {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live local; `dladdr` writes only into it and returns
    // zero when the address is in no loaded image.
    if unsafe { libc::dladdr(pc as *const libc::c_void, &mut info) } != 0 {
        let base = info.dli_fbase as u64;
        let runtime = base != 0 && base == self_base();
        match runtime {
            true => counts.runtime += 1,
            false => counts.library += 1,
        }
        // Keyed by the symbol's own address so two samples in one function are
        // one row, and by the image's base when the symbol table could not name
        // it — which buckets a whole stripped library into one row rather than
        // one row per sampled address.
        let key = match info.dli_saddr.is_null() {
            false => info.dli_saddr as u64,
            true => base,
        };
        let site = counts.host.entry(key).or_insert_with(|| {
            let image = name(info.dli_fname);
            let symbol = match name(info.dli_sname) {
                "" => format!("+{:#x}", pc.wrapping_sub(base)),
                s => s.to_string(),
            };
            HostSite {
                count: 0,
                image: image.rsplit('/').next().unwrap_or(image).to_string(),
                symbol,
                runtime,
            }
        });
        site.count += 1;
        return;
    }

    // No image: an anonymous mapping, which in this process means JIT code.
    match crate::fault::attribute(pc) {
        Attribution::Instruction { guest_va, region } => {
            *counts.guest.entry(guest_va).or_default() += 1;
            *counts.region.entry(region).or_default() += 1;
        }
        Attribution::Translated { region, beyond } => {
            match beyond {
                true => counts.translated_beyond += 1,
                false => counts.translated_unnamed += 1,
            }
            *counts.region.entry(region).or_default() += 1;
        }
        // Executable memory belonging to neither an image nor a translation.
        // Code the guest generated is not this case — that is lifted like any
        // other guest code and lands in a translation — so this is a thunk, a
        // host library's own generated code, or a translation placed since the
        // `pc` was read.
        Attribution::Elsewhere => counts.unknown += 1,
    }
}

// ------------------------------------------------------------- the driver ---

/// Start sampling, if `ROSETTA3_PROFILE` asked for it. Idempotent.
///
/// Called from [`crate::startup::run`] beside [`crate::fault::install`] and for
/// the same reason: before any guest code, so the first initializer is as
/// sampled as anything an hour in.
pub fn start() {
    if configured_hz().is_some() {
        set_sampling(true);
    }
}

/// Whether the sampler is taking samples right now.
///
/// Distinct from "the thread exists": the thread, once started, runs for the
/// life of the process and simply declines to sweep while this is false. A
/// paused sampler costs one relaxed load per timer tick.
static SAMPLING: AtomicBool = AtomicBool::new(false);

/// Whether sampling is on at this instant.
pub fn sampling() -> bool {
    SAMPLING.load(Relaxed)
}

/// Start or stop sampling. Returns the state it is now in.
///
/// **Turning it on resets the histogram**, and that is the interesting decision
/// rather than an implementation detail. A profile's every figure is a share of
/// *running samples*, so a histogram that spanned a pause would answer "where
/// did time go across the two windows I happened to sample", which is a question
/// nobody has. Resetting makes every number mean "since you turned this on",
/// which is what someone who just pressed a key is asking.
///
/// Turning it off keeps the counts, so the panel goes on showing the last
/// window's answer rather than blanking.
pub fn set_sampling(on: bool) -> bool {
    if !on {
        SAMPLING.store(false, Relaxed);
        return false;
    }
    let mut guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Counts::default());
    *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    SWEEPS.store(0, Relaxed);
    SWEEP_NS.store(0, Relaxed);
    drop(guard);
    ensure_thread();
    SAMPLING.store(true, Relaxed);
    true
}

/// Start the sampler thread, once, whether or not the environment asked.
///
/// Separate from [`start`] because a run that never set `ROSETTA3_PROFILE` can
/// still be asked for a profile from [`crate::gui`], and the rate it gets is the
/// default one — there is no keystroke that could name a number.
fn ensure_thread() {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Relaxed) {
        return;
    }
    let hz = configured_hz().unwrap_or(DEFAULT_HZ);
    install_exit_report();

    let period = Duration::from_nanos(1_000_000_000 / u64::from(hz));
    let every = match number("ROSETTA3_PROFILE_EVERY", DEFAULT_EVERY) {
        0 => None,
        s => Some(Duration::from_secs(s)),
    };
    eprintln!(
        "rosetta3: profile  sampling guest threads at {hz} Hz{}",
        match every {
            Some(d) => format!(", reporting every {}s", d.as_secs()),
            None => String::new(),
        }
    );

    std::thread::Builder::new()
        .name("rosetta3-profile".into())
        .spawn(move || sweep_forever(period, every))
        .map(|_| ())
        // Not fatal. A runtime that refused to run a program because it could
        // not profile it would be trading the thing for the measurement of it.
        .unwrap_or_else(|e| eprintln!("rosetta3: profile  could not start the sampler: {e}"));
}

/// The sampler thread.
///
/// Never stops: there is nothing to stop it *for*. The process ends by the
/// guest calling `exit`, by a fault, or by a signal, and in all three a
/// detached thread costs nothing on the way out.
fn sweep_forever(period: Duration, every: Option<Duration>) {
    let period = ticks(period.as_nanos() as u64).max(1);
    // Jitter, up to a tenth of the period. A sampler on an exact timer aliases
    // against anything the guest does periodically — a frame, an audio buffer,
    // a collection — and reports the phase it happens to share with it rather
    // than the distribution.
    let spread = period / 10 + 1;
    let mut jitter = Xorshift::seeded();
    // SAFETY: `mach_absolute_time` reads the clock and takes nothing.
    let mut next = unsafe { mach_absolute_time() };
    let mut last_report = Instant::now();
    let mine = crate::thread::mach_port();
    loop {
        next += period;
        // SAFETY: as above.
        let now = unsafe { mach_absolute_time() };
        // A sweep that took longer than its own period has nothing to make up
        // by spinning — the samples it missed are gone — so the deadline is
        // moved to now rather than chased. Without this a sampler that fell
        // behind once would free-run at whatever rate the machine allowed.
        if next < now {
            next = now;
        }
        // SAFETY: a deadline on the same absolute clock the value came from.
        unsafe { mach_wait_until(next + jitter.next() % spread) };

        // Paused. The thread keeps its timer rather than exiting, because the
        // key that turned sampling off is the same key that turns it back on and
        // a sampler that had to be respawned would lose the first samples after
        // it — which, on a keystroke, are the ones being asked for.
        if !SAMPLING.load(Relaxed) {
            continue;
        }

        let began = Instant::now();
        let mut guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(counts) = guard.as_mut() else {
            continue;
        };
        let mut found = 0;
        // The registry is scanned with the counts held, which is safe in the
        // one direction that matters: a sampled thread never takes this lock —
        // only the sampler and a report do, and a report either runs on this
        // thread or is blocked on this same lock, so a suspended thread cannot
        // be holding it. The registry itself takes no lock at all, by design;
        // see [`crate::thread`] for why a collector's reader must not.
        crate::thread::each_port(|port| {
            if port == mine {
                return;
            }
            found += 1;
            let Some((pc, running)) = sample_thread(port) else {
                return;
            };
            match running {
                true => {
                    counts.running += 1;
                    attribute(counts, pc);
                }
                false => counts.blocked += 1,
            }
        });
        counts.threads = found;
        drop(guard);
        SWEEPS.fetch_add(1, Relaxed);
        SWEEP_NS.fetch_add(began.elapsed().as_nanos() as u64, Relaxed);

        if every.is_some_and(|d| last_report.elapsed() >= d) {
            last_report = Instant::now();
            report();
        }
    }
}

/// Sixty-four bits of jitter, without a dependency.
///
/// Not cryptographic and not required to be: all it has to do is stop the
/// sample points from landing in the same phase of the guest's own period.
struct Xorshift(u64);

impl Xorshift {
    fn seeded() -> Self {
        // The clock, which is the only entropy this needs and the only source
        // available without pulling in a crate.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x2545_f491_4f6c_dd1d, |d| d.as_nanos() as u64);
        Xorshift(nanos | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Print the report when the process ends, however it ends.
fn install_exit_report() {
    // `atexit` rather than a `Drop` guard, for the reason [`crate::stats`]
    // gives: the guest calls `exit`, which runs the host's handlers and never
    // unwinds this crate's frames.
    extern "C" fn report_at_exit() {
        report();
    }
    // SAFETY: `atexit` takes an `extern "C" fn()`, which this is.
    unsafe { libc::atexit(report_at_exit) };
}

// -------------------------------------------------------------- reporting ---

/// One row of a ranked list, already resolved.
struct Row {
    count: u64,
    /// The address, as the guest's own program sees it.
    va: u64,
    /// `image+offset`, or empty for code that belongs to no image — which is
    /// code the guest generated, and is the honest answer for it.
    located: String,
}

/// Rank a histogram, resolving each key through the program's image map.
///
/// `locate` allocates and walks every image's segments, so it runs here — once
/// per distinct address at report time — rather than once per sample.
fn rank(histogram: &HashMap<u64, u64>) -> Vec<Row> {
    let vm = crate::thunk::current();
    let mut rows: Vec<Row> = histogram
        .iter()
        .map(|(&va, &count)| Row {
            count,
            va,
            located: vm.and_then(|vm| vm.program.locate(va)).unwrap_or_default(),
        })
        .collect();
    // By count, then by address, so two runs of the same program print the
    // same order for a tie rather than whatever the hash map felt like.
    rows.sort_by(|a, b| b.count.cmp(&a.count).then(a.va.cmp(&b.va)));
    rows
}

/// Print what the guest's threads were doing, if sampling was asked for.
pub fn report() {
    let guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(counts) = guard.as_ref() else { return };
    let taken = counts.running + counts.blocked;
    if taken == 0 {
        return;
    }
    let wall = STARTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t.elapsed())
        .unwrap_or_default();
    let share = |n: u64| 100.0 * n as f64 / counts.running.max(1) as f64;

    let translated =
        counts.guest.values().sum::<u64>() + counts.translated_unnamed + counts.translated_beyond;
    // The *achieved* rate, not the configured one. A timer that quietly
    // delivers a fraction of what it was asked for is the difference between a
    // thin profile and a wrong one, and the only way a reader can tell is if
    // the number printed is the one that happened.
    let sweeps = SWEEPS.load(Relaxed);
    let achieved = sweeps as f64 / wall.as_secs_f64().max(f64::EPSILON);
    let in_sweep = Duration::from_nanos(SWEEP_NS.load(Relaxed));
    eprintln!(
        "rosetta3: profile  {taken} sample(s) over {wall:.1?} on {} guest thread(s) \
         ({} running, {} blocked)",
        counts.threads, counts.running, counts.blocked,
    );
    eprintln!(
        "rosetta3: profile  {achieved:.0} sweep(s)/s, {:.1?} per sweep in the sampler \
         ({:.2}% of one core)",
        in_sweep
            .checked_div(sweeps.max(1) as u32)
            .unwrap_or_default(),
        100.0 * in_sweep.as_secs_f64() / wall.as_secs_f64().max(f64::EPSILON),
    );
    eprintln!(
        "rosetta3: profile  translated {:.1}%  runtime {:.1}%  host libraries {:.1}%  \
         unaccounted {:.1}%  (of {} running sample(s))",
        share(translated),
        share(counts.runtime),
        share(counts.library),
        share(counts.unknown),
        counts.running,
    );

    let instructions = rank(&counts.guest);
    let regions = rank(&counts.region);
    let mut hosts: Vec<&HostSite> = counts.host.values().collect();
    hosts.sort_by(|a, b| b.count.cmp(&a.count).then(a.symbol.cmp(&b.symbol)));

    say(
        "guest regions, by share of running time",
        &regions[..],
        share,
    );
    say("guest instructions", &instructions[..], share);

    eprintln!("rosetta3: profile  host symbols");
    for site in hosts.iter().take(TOP) {
        eprintln!(
            "rosetta3: profile    {:>6.2}%  {} {} {}",
            share(site.count),
            match site.runtime {
                true => "[runtime]",
                false => "[library]",
            },
            site.image,
            site.symbol,
        );
    }
    for (n, what) in [
        (
            counts.translated_unnamed,
            "[translated] inside a region, with no guest instruction \
             (a prologue, the entry dispatch, or a region past the map budget)",
        ),
        (
            counts.translated_beyond,
            "[translated] past every range a region attributed, so the region \
             is an inference (probably its epilogue)",
        ),
    ] {
        if n > 0 {
            eprintln!("rosetta3: profile    {:>6.2}%  {what}", share(n));
        }
    }

    if let Ok(path) = std::env::var("ROSETTA3_PROFILE_OUT") {
        if let Err(e) = write_histogram(&path, &instructions, &regions, share) {
            eprintln!("rosetta3: profile  could not write {path}: {e}");
        }
    }
    // Dropped before the map is written: `write_map` takes the *fault* module's
    // lock, and holding two is how an ordering bug is written.
    drop(guard);

    if let Ok(path) = std::env::var("ROSETTA3_PROFILE_MAP") {
        if let Err(e) = write_map(&path) {
            eprintln!("rosetta3: profile  could not write {path}: {e}");
        }
    }

    // And [`crate::stats`], which has the same problem this module solves and
    // one fewer way to solve it: its own periodic report is driven by
    // *translations*, and a program in steady state has stopped translating —
    // which is exactly the window an exit mix is wanted for. A run measured to
    // the point of being killed would otherwise print translation figures every
    // few seconds during start-up and nothing at all afterwards.
    crate::stats::report();
}

// -------------------------------------------------------------- snapshots ---

/// One row of a ranked list, resolved and with its share already worked out.
#[derive(Clone)]
pub struct Entry {
    pub count: u64,
    pub share: f64,
    /// The address, as the guest's own program sees it.
    pub va: u64,
    /// `image+offset`, or empty for code that belongs to no image — which is
    /// code the guest generated, and is the honest answer for it.
    pub located: String,
}

/// One host symbol the guest's threads were found inside.
#[derive(Clone)]
pub struct HostEntry {
    pub count: u64,
    pub share: f64,
    pub image: String,
    pub symbol: String,
    /// This runtime, rather than one of the host's libraries. The distinction
    /// the first performance question turns on, and the one every existing tool
    /// paints the same colour.
    pub runtime: bool,
}

/// What the sampler has seen, ranked, for a consumer that is not stderr.
///
/// [`report`] renders the same counts as prose. This hands them over as data so
/// [`crate::gui`] can draw them, and it is deliberately a *copy*: the sampler's
/// lock is held for the length of the ranking and not a moment longer, because
/// every millisecond it is held is a sweep that does not happen.
#[derive(Clone)]
pub struct Snapshot {
    /// Wall time since sampling started; the denominator for a rate.
    pub wall: Duration,
    /// Samples on a thread that was on a CPU. Every share below is of this.
    pub running: u64,
    /// Samples on a thread that was parked, kept out of the denominator.
    pub blocked: u64,
    /// Guest threads found in the registry on the most recent sweep.
    pub threads: usize,
    /// Sweeps completed, and the rate that works out to. The *achieved* rate,
    /// which is the only one worth showing — see the module note.
    pub sweeps: u64,
    pub sweeps_per_second: f64,
    /// Share of one core the sampler itself is costing.
    pub sampler_share: f64,
    /// Shares of running time, in the four buckets that decide what to work on.
    pub translated: f64,
    pub runtime: f64,
    pub library: f64,
    pub unaccounted: f64,
    /// The two rankings, longest first. **Believe the first one**: Cranelift
    /// attributes no source location to a prologue, an epilogue or to anything
    /// its optimiser produced without one, so an instruction's share is a lead
    /// rather than a measurement. See the module note.
    pub regions: Vec<Entry>,
    pub instructions: Vec<Entry>,
    pub host: Vec<HostEntry>,
}

/// Rank everything the sampler has, keeping the top `top` of each list.
///
/// `None` when sampling was never asked for, which a caller must show as
/// "`ROSETTA3_PROFILE` is not set" rather than as an empty profile: the two look
/// identical in the data and mean opposite things.
pub fn snapshot(top: usize) -> Option<Snapshot> {
    let guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let counts = guard.as_ref()?;
    let wall = STARTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| t.elapsed())
        .unwrap_or_default();
    let share = |n: u64| 100.0 * n as f64 / counts.running.max(1) as f64;
    let translated =
        counts.guest.values().sum::<u64>() + counts.translated_unnamed + counts.translated_beyond;
    let sweeps = SWEEPS.load(Relaxed);
    let in_sweep = Duration::from_nanos(SWEEP_NS.load(Relaxed));

    let entries = |rows: Vec<Row>| -> Vec<Entry> {
        rows.into_iter()
            .take(top)
            .map(|row| Entry {
                count: row.count,
                share: share(row.count),
                va: row.va,
                located: row.located,
            })
            .collect()
    };
    let mut hosts: Vec<&HostSite> = counts.host.values().collect();
    hosts.sort_by(|a, b| b.count.cmp(&a.count).then(a.symbol.cmp(&b.symbol)));

    Some(Snapshot {
        wall,
        running: counts.running,
        blocked: counts.blocked,
        threads: counts.threads,
        sweeps,
        sweeps_per_second: sweeps as f64 / wall.as_secs_f64().max(f64::EPSILON),
        sampler_share: 100.0 * in_sweep.as_secs_f64() / wall.as_secs_f64().max(f64::EPSILON),
        translated: share(translated),
        runtime: share(counts.runtime),
        library: share(counts.library),
        unaccounted: share(counts.unknown),
        regions: entries(rank(&counts.region)),
        instructions: entries(rank(&counts.guest)),
        host: hosts
            .into_iter()
            .take(top)
            .map(|site| HostEntry {
                count: site.count,
                share: share(site.count),
                image: site.image.clone(),
                symbol: site.symbol.clone(),
                runtime: site.runtime,
            })
            .collect(),
    })
}

/// One ranked list on stderr, top [`TOP`] only.
fn say(what: &str, rows: &[Row], share: impl Fn(u64) -> f64) {
    if rows.is_empty() {
        return;
    }
    eprintln!("rosetta3: profile  {what}");
    for row in rows.iter().take(TOP) {
        eprintln!(
            "rosetta3: profile    {:>6.2}%  {:#018x}  {}",
            share(row.count),
            row.va,
            match row.located.is_empty() {
                // Not a gap: a region lifted out of memory the guest generated
                // code into belongs to no image by construction, and saying so
                // is what identifies it as generated.
                true => "(generated code)",
                false => &row.located,
            },
        );
    }
}

/// The whole histogram, for the run that wants more than the top of it.
fn write_histogram(
    path: &str,
    instructions: &[Row],
    regions: &[Row],
    share: impl Fn(u64) -> f64,
) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(out, "# rosetta3 guest profile, pid {}", std::process::id())?;
    writeln!(out, "# kind\tsamples\tshare\tguest_va\timage+offset")?;
    for (kind, rows) in [("region", regions), ("instruction", instructions)] {
        for row in rows {
            writeln!(
                out,
                "{kind}\t{}\t{:.4}\t{:#x}\t{}",
                row.count,
                share(row.count),
                row.va,
                match row.located.is_empty() {
                    true => "-",
                    false => &row.located,
                },
            )?;
        }
    }
    out.flush()
}

/// Emit the host-address-to-guest-address map an external profiler needs.
///
/// This is the file that makes Apple's own tooling usable against this runtime.
/// `xctrace` — the `Processor Trace` template on an M4, or `CPU Counters`, or
/// `Time Profiler` — records host addresses and symbolicates them against
/// *loaded images*. Translated code is in none, so every sample in the half of
/// the profile that matters comes back as a bare address in an anonymous
/// mapping. There is no Darwin equivalent of a jitdump for a tool to read, so
/// the join is done afterwards, and this is its left-hand side.
///
/// One line per region by default. `ROSETTA3_PROFILE_MAP_INSTRUCTIONS=1` emits
/// a line per guest *instruction* instead, which is what a branch trace wants
/// and which is also tens of megabytes for a program that generates its own
/// code.
///
/// ```text
/// R 0x10a4c0000 0x10a4c1a8 0x100002340 0x1000023f0 Mac_Runner+0x2340
/// I 0x10a4c0000 0x10a4c018 0x100002340 Mac_Runner+0x2340
/// ```
pub fn write_map(path: &str) -> std::io::Result<()> {
    let instructions = number("ROSETTA3_PROFILE_MAP_INSTRUCTIONS", 0) != 0;
    let vm = crate::thunk::current();
    let locate = |va: u64| {
        vm.and_then(|vm| vm.program.locate(va))
            .unwrap_or_else(|| "-".to_string())
    };

    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(out, "# rosetta3 jit map, pid {}", std::process::id())?;
    writeln!(
        out,
        "# R host_start host_end guest_start guest_end image+offset"
    )?;
    writeln!(out, "# I host_start host_end guest_va image+offset")?;

    let mut error = None;
    crate::fault::for_each_placed(|region, ranges| {
        if error.is_some() {
            return;
        }
        let mut write = || -> std::io::Result<()> {
            writeln!(
                out,
                "R {:#x} {:#x} {:#x} {:#x} {}",
                region.code,
                region.code_end,
                region.guest.0,
                region.guest.1,
                locate(region.guest.0),
            )?;
            if instructions {
                for r in ranges {
                    writeln!(
                        out,
                        "I {:#x} {:#x} {:#x} {}",
                        region.code + u64::from(r.start),
                        region.code + u64::from(r.end),
                        r.guest_va,
                        locate(r.guest_va),
                    )?;
                }
            }
            Ok(())
        };
        error = write().err();
    });
    match error {
        Some(e) => Err(e),
        None => out.flush(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_a_switch_and_a_number_is_a_rate() {
        // `ROSETTA3_PROFILE=1` must mean "on", not "one sample a second":
        // every other flag in this runtime is turned on that way, and a
        // one-hertz profile of a ten-second run is eleven samples.
        let parse = |raw: &str| -> Option<u32> {
            match raw.trim() {
                "" | "0" | "no" | "off" => None,
                "1" | "yes" | "on" => Some(DEFAULT_HZ),
                n => n.parse().ok().filter(|&hz| hz > 0),
            }
        };
        assert_eq!(parse("1"), Some(DEFAULT_HZ));
        assert_eq!(parse("on"), Some(DEFAULT_HZ));
        assert_eq!(parse("4000"), Some(4000));
        assert_eq!(parse("0"), None);
        assert_eq!(parse("off"), None);
        assert_eq!(parse("nonsense"), None);
    }

    #[test]
    fn the_pc_word_index_agrees_with_the_offset_the_fault_handler_measured() {
        // `crate::fault::host_pc` reads `__pc` at byte 256 into the thread
        // state, measured with `offsetof` against this system's headers. Same
        // field, different units, and a disagreement here would be a profile
        // that reported `__sp` as a program counter.
        assert_eq!(PC_WORD * 8, 256);
        // And the state is thirty-three doublewords plus `__cpsr` and its pad.
        assert_eq!(ARM_THREAD_STATE64_COUNT as usize, 33 * 2 + 2);
    }

    #[test]
    fn jitter_stays_inside_a_tenth_of_the_period_and_moves() {
        let period = Duration::from_millis(1);
        let bound = period.as_nanos() as u64 / 10 + 1;
        let mut x = Xorshift::seeded();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let j = x.next() % bound;
            assert!(j < bound);
            seen.insert(j);
        }
        // A generator that returned a constant would still satisfy the bound
        // and would defeat the entire point of jittering.
        assert!(seen.len() > 1, "the jitter never moved");
    }

    #[test]
    fn ranking_is_by_count_and_ties_break_on_the_address() {
        let mut h = HashMap::new();
        h.insert(0x2000, 5);
        h.insert(0x1000, 5);
        h.insert(0x3000, 9);
        let rows = rank(&h);
        assert_eq!(
            rows.iter().map(|r| r.va).collect::<Vec<_>>(),
            [0x3000, 0x1000, 0x2000],
            "a tie must not depend on hash order, or two runs disagree"
        );
    }
}
