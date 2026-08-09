//! What a crashing guest says on its way out.
//!
//! ## The problem this exists for
//!
//! A guest that faults inside translated code produces a crash report naming a
//! `pc` in an anonymous mapping and nothing else. There is no image to resolve
//! it against — every translation is its own Cranelift module in its own
//! reservation — so the one fact worth having, *which guest instruction was
//! running*, is exactly the fact missing. Both Subnauticas end that way, and
//! [M7.7](../../../docs/ROADMAP.md) recorded "make the crash survive being
//! observed" as the step everything else waits on: under `lldb` the fault does
//! not reproduce, so the postmortem cannot come from a debugger.
//!
//! So this runtime reports the fault itself, from a handler, and then gets out
//! of the way.
//!
//! ## What it can say, and what it cannot
//!
//! Three things, and the third is the one that was missing:
//!
//! - **the fault**: signal, faulting address, and whether that address is
//!   mapped at all;
//! - **the thread**: its Mach port, and whether it was inside guest code;
//! - **the guest region**: the translation whose compiled code contains the
//!   faulting `pc`, reported as the *guest* address range it was lifted from,
//!   plus the guest `rip` the run loop entered it at.
//!
//! What it deliberately does not claim is the exact guest instruction. Microlift
//! emits no host-to-guest line table, so the honest answer is the region and the
//! host offset into it — `objdump` over the guest bytes of a named range is a
//! minute's work, and a guessed instruction would be a lie that reads like a
//! fact.
//!
//! ## Why it hands the signal back rather than handling it
//!
//! This is a *reporter*, not a recovery mechanism, and the difference matters
//! for two reasons. A guest fault is not this runtime's to swallow — Mono
//! installs `SIGSEGV` handlers of its own for null checks, and
//! [`crate::signal`] records them without installing them precisely because a
//! `ucontext_t` describing guest registers is not something the recompiler can
//! build. And the crash report the system writes is *evidence*: the run that
//! produced [M7.9](../../../docs/ROADMAP.md)'s `.ips` is where the ASCII
//! faulting address came from.
//!
//! So the handler prints, restores whatever disposition was in place before it
//! (Rust's own, which turns a stack overflow into a sentence, or the default),
//! and returns. The faulting instruction re-executes, faults again, and the
//! process dies exactly as it would have — with one paragraph more on stderr.
//!
//! ## Signal-handler discipline
//!
//! Everything here runs in a signal handler on a faulting thread, which rules
//! out allocation, formatting machinery, and blocking. So:
//!
//! - the message is assembled in a fixed stack buffer and written with one
//!   `write(2)`, rather than through `eprintln!`;
//! - the region map is consulted with `try_lock`, and the report says
//!   "unavailable" rather than deadlocking against a thread that was compiling;
//! - `SA_ONSTACK`, so a fault caused by *stack exhaustion* still has somewhere
//!   to run — Rust's runtime has already given each thread an alternate stack.

use microlift::core::srcloc::{GuestCodeMap, GuestCodeRange};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

/// One compiled translation, as a fault needs to see it.
struct Placed {
    /// Host address the compiled code begins at.
    code: u64,
    /// Guest addresses the region covers.
    guest: (u64, u64),
    /// The guest `rip` the region was lifted to be entered at.
    lifted_for: u64,
    /// Host offset → guest instruction, from microlift's own source locations.
    ///
    /// Plain `Copy` data, sorted by host offset and non-overlapping, which is
    /// what makes it readable from a handler: the lookup is a binary search
    /// over a slice and touches nothing else.
    ///
    /// Empty when the map budget was already spent — see [`MAP_BUDGET`]. The
    /// report then gives the region and the offset, which is what it gave
    /// before this field existed.
    map: Vec<GuestCodeRange>,
}

impl Placed {
    /// One past the last host byte any guest instruction was lifted into.
    ///
    /// Not the function's length: Cranelift attributes no source location to a
    /// prologue, an epilogue, or the entry dispatch switch, so those are absent
    /// from the map and this under-counts by however many bytes they take. It
    /// is still the right bound to report against, because *under*-counting
    /// makes a `pc` past the end say so rather than claim a region it may not
    /// be in.
    fn attributed_end(&self) -> u64 {
        self.map.last().map_or(0, |r| self.code + u64::from(r.end))
    }
}

/// Compiled translations by the host address of their entry point.
///
/// Sorted, so a faulting `pc` finds the last translation at or below it —
/// which is a *candidate*, not an answer, and is checked against that
/// translation's own extent before the report claims it.
///
/// Global rather than per-thread even though the caches are per-thread: a fault
/// handler has one question, and which thread compiled the code is not part of
/// it.
static PLACED: Mutex<Vec<Placed>> = Mutex::new(Vec::new());

/// How many host-to-guest ranges to keep across the whole process.
///
/// Sixteen bytes each, so this is a 64 MiB ceiling on the one thing here that
/// grows with the program. A guest that generates its own code — which is the
/// guest this module was built for — lifts tens of thousands of regions, and an
/// unbounded map would be a memory leak wearing a diagnostic's clothes. Past
/// the budget the maps stop being kept and the report degrades to naming the
/// region, which is what it did before.
const MAP_BUDGET: usize = 4 * 1024 * 1024;

/// Ranges kept so far, against [`MAP_BUDGET`].
static MAPPED: AtomicUsize = AtomicUsize::new(0);

/// Note where a translation's compiled code went, and what it came from.
///
/// Called from [`crate::cache`] as each region is finalized. Insertion keeps the
/// vector sorted, which for an append-mostly list — JIT modules come from
/// `mmap`, so later ones are usually higher — is a `push` and a rare shift.
pub fn placed(
    code: u64,
    guest_start: u64,
    guest_end: u64,
    lifted_for: u64,
    map: &GuestCodeMap,
) -> usize {
    let ranges = map.ranges();
    let keep = MAPPED.fetch_add(ranges.len(), Ordering::Relaxed) + ranges.len() <= MAP_BUDGET;
    let entry = Placed {
        code,
        guest: (guest_start, guest_end),
        lifted_for,
        map: match keep {
            true => ranges.to_vec(),
            false => Vec::new(),
        },
    };
    let mut placed = PLACED.lock().unwrap_or_else(|e| e.into_inner());
    match placed.binary_search_by_key(&code, |p| p.code) {
        Ok(i) => placed[i] = entry,
        Err(i) => placed.insert(i, entry),
    }
    placed.len()
}

/// Whether [`install`] has run. Handlers are process-wide.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Longest image name the report keeps. `libmonobdwgc-2.0.dylib` is 23.
const NAME: usize = 48;

/// One loaded guest image, flattened so a handler can search it.
///
/// A fixed byte array rather than a `String` because the search happens in a
/// signal handler: the name has to be printable without touching the allocator,
/// and a leaf name is what a reader wants anyway.
struct Image {
    range: (u64, u64),
    name: [u8; NAME],
    used: usize,
}

/// Where the guest's images landed. Filled once, before any guest code runs.
static IMAGES: Mutex<Vec<Image>> = Mutex::new(Vec::new());

/// Record the guest's image map, so a fault can name a guest address rather
/// than only state it.
///
/// Without this the report's guest range is a number that needs a second run
/// with `ROSETTA3_TRACE_VM` to interpret — and the whole reason this module
/// exists is that the interesting crash is the one that does not come back.
pub fn note_images(report: &machish::report::LinkReport) {
    let mut images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    images.clear();
    for image in &report.images {
        let end = image
            .segments
            .iter()
            .filter(|s| s.vmsize != 0)
            .map(|s| s.vmaddr.wrapping_add(s.vmsize))
            .max();
        let Some(end) = end else { continue };
        let leaf = image.name.rsplit('/').next().unwrap_or(&image.name);
        let bytes = leaf.as_bytes();
        let used = bytes.len().min(NAME);
        let mut name = [0u8; NAME];
        name[..used].copy_from_slice(&bytes[..used]);
        images.push(Image {
            range: (image.base, end),
            name,
            used,
        });
    }
    images.sort_by_key(|i| i.range.0);
}

/// Install the reporter for the faults a translation can raise.
///
/// Idempotent, and safe to call before any guest code runs — which is where
/// [`crate::startup`] calls it, because a fault during the first initializer is
/// as much worth naming as one an hour in.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    for signal in [libc::SIGSEGV, libc::SIGBUS] {
        // SAFETY: `action` and `old` are live locals of the right type;
        // `sa_sigaction` holds a handler with the `SA_SIGINFO` signature, which
        // is what `SA_SIGINFO` in the flags declares it to be. Installing a
        // handler is what this function is.
        let old = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = report as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, &action, &mut old) != 0 {
                continue;
            }
            old
        };
        let saved = saved_for(signal);
        saved.handler.store(old.sa_sigaction, Ordering::SeqCst);
        saved.mask.store(old.sa_mask, Ordering::SeqCst);
        saved.flags.store(old.sa_flags, Ordering::SeqCst);
    }
}

/// The disposition [`install`] displaced, taken apart into fields a handler may
/// read without a lock.
///
/// A `sigaction` in a `static mut` would be the obvious shape and the wrong one:
/// a handler runs on any thread at any moment, so the saved value has to be
/// published by the same rules everything else shared between threads is. Three
/// atomics carry the whole of what `sigaction` takes back.
struct Saved {
    handler: AtomicUsize,
    mask: AtomicU32,
    flags: AtomicI32,
}

const fn saved() -> Saved {
    Saved {
        handler: AtomicUsize::new(libc::SIG_DFL),
        mask: AtomicU32::new(0),
        flags: AtomicI32::new(0),
    }
}

static SEGV: Saved = saved();
static BUS: Saved = saved();

fn saved_for(signal: i32) -> &'static Saved {
    match signal {
        libc::SIGBUS => &BUS,
        _ => &SEGV,
    }
}

/// The handler. Prints, restores the previous disposition, and returns.
///
/// Returning is what makes the faulting instruction run again: the second fault
/// meets whatever was installed before this and the process ends the way it
/// always did, with the system's crash report intact.
extern "C" fn report(signal: i32, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // A fault *inside* this handler would recurse until the alternate stack
    // ran out, which would replace one unexplained crash with another. One
    // report is all that is wanted anyway: the first fault is the interesting
    // one.
    static REPORTING: AtomicBool = AtomicBool::new(false);
    if REPORTING.swap(true, Ordering::SeqCst) {
        restore(signal);
        return;
    }

    let mut out = Buffer::new();
    out.str("rosetta3: fault: ");
    out.str(match signal {
        libc::SIGBUS => "SIGBUS",
        libc::SIGSEGV => "SIGSEGV",
        _ => "signal",
    });

    // SAFETY: the kernel passes a live `siginfo_t` for the duration of the
    // handler; `si_addr` is the faulting address for both signals handled here.
    let addr = unsafe { info.as_ref() }.map_or(0, |i| i.si_addr as usize as u64);
    out.str(" at ");
    out.hex(addr);
    if let Some(text) = ascii_word(addr) {
        // The clue that named M7.9's crash. A faulting address whose bytes are
        // printable is not a pointer that went stale; it is a pointer that was
        // never one, and knowing *what text* it is has repeatedly been the
        // fastest way to the structure it was read out of.
        out.str(" (ascii \"");
        out.bytes(&text);
        out.str("\")");
    }

    if let Some(pc) = host_pc(context) {
        out.str("\n  host pc ");
        out.hex(pc);
        say_region(&mut out, pc);
    }

    say_thread(&mut out);
    out.str("\n");
    out.flush();

    restore(signal);
}

/// Name the translation the faulting `pc` fell in, and the guest instruction
/// inside it when microlift's source locations reach that far.
fn say_region(out: &mut Buffer, pc: u64) {
    let Ok(placed) = PLACED.try_lock() else {
        out.str("\n  guest region: unavailable (a thread holds the map)");
        return;
    };
    let found = match placed.binary_search_by_key(&pc, |p| p.code) {
        Ok(i) => Some(&placed[i]),
        Err(0) => None,
        Err(i) => Some(&placed[i - 1]),
    };
    let Some(entry) = found else {
        out.str(
            "\n  guest region: none — the pc is below every translation, \
             so this fault is in host code",
        );
        return;
    };

    out.str("\n  guest region ");
    out.hex(entry.guest.0);
    out.str("..");
    out.hex(entry.guest.1);
    say_image(out, entry.guest.0);
    out.str(", entered at ");
    out.hex(entry.lifted_for);

    // The exact instruction, when the map has it. This is the whole point of
    // carrying microlift's source locations: a guest *address* is a work item
    // and a region is a search.
    let offset = pc - entry.code;
    if let Ok(offset) = u32::try_from(offset) {
        if let Some(va) = lookup(&entry.map, offset) {
            out.str("\n  guest instruction ");
            out.hex(va);
            say_image(out, va);
            return;
        }
    }

    out.str("\n  host offset +");
    out.hex(offset);
    out.str(" into its compiled code");
    // A `pc` outside everything the map attributed is not necessarily inside
    // this translation at all: the candidate is only "the nearest entry point
    // at or below", so an offset past the end usually means the fault was in a
    // *later* translation whose entry this list has not seen, or in host code
    // between two of them. Saying which case it is beats claiming a region.
    if entry.map.is_empty() {
        out.str(" — no source map was kept for this region (map budget spent)");
    } else if pc >= entry.attributed_end() {
        out.str(
            " — past every range this translation attributed to a guest \
                 instruction, so the fault is probably not in it",
        );
    } else {
        out.str(
            " — inside it, but in code with no guest origin: a prologue, \
                 an epilogue, or the entry dispatch",
        );
    }
}

/// The guest instruction covering a host offset, over microlift's own sorted,
/// non-overlapping ranges.
///
/// Open-coded rather than calling [`GuestCodeMap::lookup`] because this runs in
/// a signal handler over a slice the handler borrowed: rebuilding a
/// `GuestCodeMap` to ask it a question would allocate.
fn lookup(map: &[GuestCodeRange], offset: u32) -> Option<u64> {
    let i = map.partition_point(|r| r.start <= offset);
    let range = map.get(i.checked_sub(1)?)?;
    range.contains(offset).then_some(range.guest_va)
}

/// Name the image a guest address is in, as `name+offset`.
///
/// Silent when it is in none of them, which is not a gap: Mono writes its own
/// code into memory it allocated, and a region lifted from there belongs to no
/// image by construction. Saying nothing there is the honest answer, and it is
/// also the answer that identifies generated code.
fn say_image(out: &mut Buffer, addr: u64) {
    let Ok(images) = IMAGES.try_lock() else {
        return;
    };
    let Some(image) = images
        .iter()
        .find(|i| (i.range.0..i.range.1).contains(&addr))
    else {
        return;
    };
    out.str(" (");
    out.bytes(&image.name[..image.used]);
    out.str("+");
    out.hex(addr - image.range.0);
    out.str(")");
}

/// What this runtime knows about the faulting thread.
fn say_thread(out: &mut Buffer) {
    out.str("\n  thread ");
    out.hex(u64::from(crate::thread::mach_port()));
    let Some(state) = crate::thread::current() else {
        out.str(" — not a guest thread; the fault is in this runtime or in a host library");
        return;
    };
    out.str(", guest rsp ");
    out.hex(state.live_rsp());
    out.str(", entry ");
    out.hex(state.entry());
    let stack = state.stack_range();
    out.str(", stack ");
    out.hex(stack.start);
    out.str("..");
    out.hex(stack.end);
    if !stack.contains(&state.live_rsp()) {
        out.str(" — rsp is outside it");
    }
}

/// Put the disposition that was in place before [`install`] back, so the second
/// fault is handled the way the first would have been.
fn restore(signal: i32) {
    let saved = saved_for(signal);
    // SAFETY: `action` is a live local, rebuilt from the three fields `install`
    // read out of the disposition it displaced. Installing it is the inverse of
    // what `install` did.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = saved.handler.load(Ordering::SeqCst);
        action.sa_mask = saved.mask.load(Ordering::SeqCst);
        action.sa_flags = saved.flags.load(Ordering::SeqCst);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

/// The faulting `pc`, out of the `ucontext_t` the kernel passed.
///
/// The layout is Darwin's `_STRUCT_UCONTEXT` and `_STRUCT_ARM_THREAD_STATE64`,
/// which the `libc` crate does not declare for Apple platforms. Rather than
/// guess at the offsets, they were **measured** with `offsetof` against this
/// system's `<sys/ucontext.h>`: `uc_mcontext` at 48 in a 56-byte `ucontext_t`,
/// the thread state at 16 inside the `mcontext`, and `__pc` at 256 inside that —
/// after `__x[29]`, `__fp`, `__lr` and `__sp`.
///
/// `None` on any null, because a handler that dereferences its way to a second
/// fault has destroyed the evidence it was called to preserve.
fn host_pc(context: *mut libc::c_void) -> Option<u64> {
    /// Byte offset of `uc_mcontext` within `ucontext_t`.
    const UC_MCONTEXT: usize = 48;
    /// Byte offset of `__ss.__pc` within `__darwin_mcontext64`.
    const MC_PC: usize = 16 + 256;

    if context.is_null() {
        return None;
    }
    // SAFETY: the kernel passes a live `ucontext_t` for the duration of the
    // handler, and the two offsets are the measured ones above. Both loads are
    // eight-aligned reads of pointer-sized fields inside structures the kernel
    // filled in.
    unsafe {
        let mcontext = std::ptr::read((context as *const u8).add(UC_MCONTEXT) as *const *const u8);
        if mcontext.is_null() {
            return None;
        }
        Some(std::ptr::read(mcontext.add(MC_PC) as *const u64))
    }
}

/// The address's own bytes, when every one of them is printable.
///
/// Little-endian, so the low byte is the first character — which is the order
/// the text was written in memory.
fn ascii_word(addr: u64) -> Option<[u8; 8]> {
    let bytes = addr.to_le_bytes();
    bytes
        .iter()
        .all(|b| (0x20..0x7f).contains(b))
        .then_some(bytes)
}

/// A fixed message buffer, written out in one `write(2)`.
///
/// No allocation, no `core::fmt`: both are reachable from a signal handler only
/// by assuming things about what the faulting thread was doing, and the whole
/// point of this module is to work on the thread that was doing the worst
/// thing.
struct Buffer {
    bytes: [u8; 1024],
    used: usize,
}

impl Buffer {
    fn new() -> Self {
        Buffer {
            bytes: [0; 1024],
            used: 0,
        }
    }

    /// Append what fits. Truncation is silent because the alternative — a
    /// handler that reports its own overflow — is not more useful than a
    /// shorter sentence.
    fn bytes(&mut self, data: &[u8]) {
        let room = self.bytes.len() - self.used;
        let n = data.len().min(room);
        self.bytes[self.used..self.used + n].copy_from_slice(&data[..n]);
        self.used += n;
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// `0x` and the value, with no leading zeros. Hand-rolled for the reason
    /// the type exists.
    fn hex(&mut self, mut value: u64) {
        let mut digits = [0u8; 16];
        let mut n = 0;
        loop {
            digits[n] = b"0123456789abcdef"[(value & 0xf) as usize];
            value >>= 4;
            n += 1;
            if value == 0 {
                break;
            }
        }
        self.str("0x");
        for i in (0..n).rev() {
            self.bytes(&[digits[i]]);
        }
    }

    fn flush(&mut self) {
        // SAFETY: a `write` of `used` bytes from a buffer that has at least
        // that many. `write` is async-signal-safe, which is the whole reason
        // this type exists; a short write loses the tail of a diagnostic and is
        // not worth a retry loop inside a handler.
        unsafe {
            libc::write(2, self.bytes.as_ptr() as *const libc::c_void, self.used);
        }
        self.used = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_has_no_leading_zeros_and_a_zero_is_a_zero() {
        let mut b = Buffer::new();
        b.hex(0);
        b.str(" ");
        b.hex(0x70536573556e6143);
        assert_eq!(&b.bytes[..b.used], b"0x0 0x70536573556e6143");
    }

    #[test]
    fn an_ascii_address_is_recognised_and_an_ordinary_one_is_not() {
        // The word M7.9's crash faulted on. Recognising it is what turned that
        // report from "a bad pointer" into "a pointer read out of text".
        assert_eq!(ascii_word(0x70536573556e6143), Some(*b"CanUseSp"));
        assert_eq!(ascii_word(0x1383bf610), None);
        assert_eq!(ascii_word(0), None);
    }

    /// A map with one range per guest instruction, laid out back to back.
    fn map(guest_start: u64, instructions: u32) -> GuestCodeMap {
        GuestCodeMap::from_ranges(
            (0..instructions)
                .map(|i| GuestCodeRange {
                    start: i * 16,
                    end: (i + 1) * 16,
                    guest_va: guest_start + u64::from(i),
                })
                .collect(),
        )
    }

    #[test]
    fn a_host_pc_resolves_to_the_guest_instruction_it_came_from() {
        // The claim the whole module turns on. Without the source map the best
        // available answer was "some address in this region", which for a
        // sixty-byte function is a search and for a Mono method is a shrug.
        placed(0x40_0000, 0x1_0000, 0x1_0010, 0x1_0000, &map(0x1_0000, 16));
        let placed = PLACED.lock().unwrap();
        let entry = placed.iter().find(|p| p.code == 0x40_0000).unwrap();

        assert_eq!(lookup(&entry.map, 0), Some(0x1_0000));
        assert_eq!(lookup(&entry.map, 15), Some(0x1_0000));
        assert_eq!(lookup(&entry.map, 16), Some(0x1_0001));
        assert_eq!(lookup(&entry.map, 0x57), Some(0x1_0005));
        // Past everything attributed: a prologue, an epilogue, or another
        // translation altogether. `None` is what makes the report say so
        // instead of naming an instruction it cannot see.
        assert_eq!(lookup(&entry.map, 16 * 16), None);
    }

    #[test]
    fn the_nearest_translation_below_the_pc_is_the_one_reported() {
        placed(0x4000, 0x1_0000, 0x1_0100, 0x1_0000, &map(0x1_0000, 4));
        placed(0x8000, 0x2_0000, 0x2_0080, 0x2_0040, &map(0x2_0000, 4));
        let placed = PLACED.lock().unwrap();
        let find = |pc: u64| match placed.binary_search_by_key(&pc, |p| p.code) {
            Ok(i) => Some(placed[i].guest),
            Err(0) => None,
            Err(i) => Some(placed[i - 1].guest),
        };
        assert_eq!(find(0x3fff), None);
        assert_eq!(find(0x4000), Some((0x1_0000, 0x1_0100)));
        assert_eq!(find(0x4010), Some((0x1_0000, 0x1_0100)));
        assert_eq!(find(0x9000), Some((0x2_0000, 0x2_0080)));
    }

    #[test]
    fn a_pc_past_everything_a_translation_covers_is_not_claimed_for_it() {
        // The misattribution this replaced: `+0x57c` into a sixty-two-byte
        // region was reported as if it were inside it. Four sixteen-byte ranges
        // end at 0x40, so anything at or past that is outside.
        placed(0x10_0000, 0x3_0000, 0x3_0004, 0x3_0000, &map(0x3_0000, 4));
        let placed = PLACED.lock().unwrap();
        let entry = placed.iter().find(|p| p.code == 0x10_0000).unwrap();
        assert_eq!(entry.attributed_end(), 0x10_0040);
        assert_eq!(lookup(&entry.map, 0x3f), Some(0x3_0003));
        assert_eq!(lookup(&entry.map, 0x40), None);
    }

    #[test]
    fn a_buffer_truncates_rather_than_panicking() {
        let mut b = Buffer::new();
        for _ in 0..500 {
            b.str("0123456789");
        }
        assert_eq!(b.used, b.bytes.len());
    }
}
