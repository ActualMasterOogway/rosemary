//! What a crashing guest says on its way out.

use microlift::core::srcloc::{GuestCodeMap, GuestCodeRange};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

/// One compiled translation, as a fault needs to see it.
struct Placed {
    /// Lowest host address of the compiled blob, and the key this vector is sorted
    /// by.
    code: u64,
    /// The address [`map`](Self::map)'s offsets are relative to, which is
    /// `link_target` and not necessarily [`code`](Self::code).
    map_base: u64,
    /// The host-callable entry thunk: the prologue and the `rip` dispatch every
    /// run-loop entry goes through.
    stub: u64,
    /// Guest addresses the region covers.
    guest: (u64, u64),
    /// The guest `rip` the region was lifted to be entered at.
    lifted_for: u64,
    /// Host offset -> guest instruction, from microlift's own source locations.
    map: Vec<GuestCodeRange>,
}

impl Placed {
    /// One past the last host byte any guest instruction was lifted into.
    fn attributed_end(&self) -> u64 {
        self.map
            .last()
            .map_or(self.code, |r| self.map_base + u64::from(r.end))
    }
}

/// How far past a blob's start a `pc` may be and still be believed to be in it, when
/// nothing follows it to say otherwise.
const MAX_BLOB: u64 = 1 << 20;

/// Compiled translations by the host address of their entry point.
///
/// Global rather than per-thread even though the caches are per-thread.
static PLACED: Mutex<Vec<Placed>> = Mutex::new(Vec::new());

/// How many host-to-guest ranges to keep across the whole process.
const MAP_BUDGET_MIB: usize = 64;

/// Bytes per kept range, which is what turns the environment's MiB into a count.
const RANGE_BYTES: usize = 16;

/// The ceiling this run uses.
fn map_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        let mib = std::env::var("ROSETTA3_MAP_BUDGET")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(MAP_BUDGET_MIB);
        mib.saturating_mul(1024 * 1024) / RANGE_BYTES
    })
}

/// Ranges kept so far, against [`map_budget`].
static MAPPED: AtomicUsize = AtomicUsize::new(0);

/// Note where a translation's compiled code went, and what it came from.
pub fn placed(
    entry_point: u64,
    link_target: u64,
    guest_start: u64,
    guest_end: u64,
    lifted_for: u64,
    map: &GuestCodeMap,
) -> usize {
    let code = entry_point.min(link_target);
    // Keyed low, but the thunk's own address is kept so a sample in it can be
    // named as what it is rather than as whichever side of the body it fell on.
    let stub = entry_point;
    let ranges = map.ranges();
    let keep = MAPPED.fetch_add(ranges.len(), Ordering::Relaxed) + ranges.len() <= map_budget();
    let entry = Placed {
        code,
        map_base: link_target,
        stub,
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

/// What a host `pc` turned out to be, as far as the placed translations can say.
///
/// The crash path answers this question with [`say_region`], inside a signal handler,
/// into a fixed buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// The `pc` is inside a translation, and microlift's source locations name the
    /// guest instruction it was lifted from.
    Instruction {
        /// The guest address of the instruction.
        guest_va: u64,
        /// The first guest address of the region it belongs to, which is the
        /// key a per-region rollup aggregates on.
        region: u64,
    },
    /// Inside a translation, in code no guest instruction was attributed to.
    Translated {
        region: u64,
        why: Unnamed,
    },
    /// In no translation this process placed.
    Elsewhere,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unnamed {
    /// The host-callable entry thunk: the prologue and the `rip` dispatch that
    /// begins every run-loop entry. Region entry overhead, not guest work.
    Prologue,
    /// This region kept no map, because [`map_budget`] was spent before it was
    /// compiled. The `pc` could be any instruction in it.
    Budget,
    /// The region has a map and the `pc` is inside its span, in a range no guest
    /// instruction claimed.
    Gap,
    /// Past everything the map attributed - most likely the epilogue.
    Beyond,
}

/// Which guest code a host `pc` is, if it is any.
pub fn attribute(pc: u64) -> Attribution {
    let placed = PLACED.lock().unwrap_or_else(|e| e.into_inner());
    let at = match placed.binary_search_by_key(&pc, |p| p.code) {
        Ok(i) => i,
        Err(0) => return Attribution::Elsewhere,
        Err(i) => i - 1,
    };
    let entry = &placed[at];
    let end = placed
        .get(at + 1)
        .map_or(u64::MAX, |next| next.code)
        .min(entry.code.saturating_add(MAX_BLOB));
    if pc >= end {
        return Attribution::Elsewhere;
    }
    let region = entry.guest.0;
    let in_stub = match entry.stub >= entry.map_base {
        true => pc >= entry.stub && entry.stub != entry.map_base,
        false => pc < entry.map_base,
    };
    if in_stub {
        return Attribution::Translated {
            region,
            why: Unnamed::Prologue,
        };
    }
    // Instruction-level, when the map for this one survived the budget.
    if let Ok(offset) = u32::try_from(pc.wrapping_sub(entry.map_base))
        && let Some(guest_va) = lookup(&entry.map, offset)
    {
        return Attribution::Instruction { guest_va, region };
    }
    Attribution::Translated {
        region,
        why: match () {
            _ if entry.map.is_empty() => Unnamed::Budget,
            _ if pc >= entry.attributed_end() => Unnamed::Beyond,
            _ => Unnamed::Gap,
        },
    }
}

/// One placed translation, as anything outside this module may see it.
///
/// A view rather than a borrow of [`Placed`] because the vector is behind a mutex that
/// must not escape.
pub struct PlacedRegion {
    /// Host address the compiled code begins at.
    pub code: u64,
    /// One past the last host byte attributed to a guest instruction - see
    /// [`Placed::attributed_end`] for why this is not the code's length.
    pub code_end: u64,
    /// Guest addresses the region covers.
    pub guest: (u64, u64),
    /// The guest `rip` the region was lifted to be entered at.
    pub lifted_for: u64,
}

/// Walk every translation placed so far, newest layout order (by host address).
///
/// The lock is held for the whole walk, so `f` must not compile anything.
pub fn for_each_placed(mut f: impl FnMut(&PlacedRegion, &[GuestCodeRange])) {
    let placed = PLACED.lock().unwrap_or_else(|e| e.into_inner());
    for entry in placed.iter() {
        let view = PlacedRegion {
            code: entry.code,
            code_end: entry.attributed_end(),
            guest: entry.guest,
            lifted_for: entry.lifted_for,
        };
        f(&view, &entry.map);
    }
}

/// Whether [`install`] has run. Handlers are process-wide.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Longest image name the report keeps. `libmonobdwgc-2.0.dylib` is 23.
const NAME: usize = 48;

/// One loaded guest image, flattened so a handler can search it.
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
/// with `ROSETTA3_TRACE_VM` to interpret - and the whole reason this module
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
/// Idempotent, and safe to call before any guest code runs - which is where
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

/// The disposition [`install`] displaced, taken apart into fields a handler may read
/// without a lock.
///
/// A `sigaction` in a `static mut` would be the obvious shape and the wrong one.
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
extern "C" fn report(signal: i32, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // A fault *inside* this handler would recurse until the alternate stack ran
    // out, which would replace one unexplained crash with another.
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
            "\n  guest region: none - the pc is below every translation, \
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
    // carrying microlift's source locations. Relative to `map_base`, which is what
    // the offsets were recorded against - not to `code`, which may be the entry
    // stub below it.
    let offset = pc.wrapping_sub(entry.map_base);
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
    // this translation at all.
    if entry.map.is_empty() {
        out.str(" - no source map was kept for this region (map budget spent)");
    } else if pc >= entry.attributed_end() {
        out.str(
            " - past every range this translation attributed to a guest \
                 instruction, so the fault is probably not in it",
        );
    } else {
        out.str(
            " - inside it, but in code with no guest origin: a prologue, \
                 an epilogue, or the entry dispatch",
        );
    }
}

/// The guest instruction covering a host offset, over microlift's own sorted,
/// non-overlapping ranges.
fn lookup(map: &[GuestCodeRange], offset: u32) -> Option<u64> {
    let i = map.partition_point(|r| r.start <= offset);
    let range = map.get(i.checked_sub(1)?)?;
    range.contains(offset).then_some(range.guest_va)
}

/// Name the image a guest address is in, as `name+offset`.
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
        out.str(" - not a guest thread; the fault is in this runtime or in a host library");
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
        out.str(" - rsp is outside it");
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
/// The layout is Darwin's `_STRUCT_UCONTEXT` and the host's thread state, which the
/// `libc` crate does not declare for Apple platforms.
///
/// `None` on any null, because a handler that dereferences its way to a second fault
/// has destroyed the evidence it was called to preserve.
fn host_pc(context: *mut libc::c_void) -> Option<u64> {
    /// Byte offset of `uc_mcontext` within `ucontext_t`. The same on both hosts:
    /// nothing ahead of it is architecture-dependent.
    const UC_MCONTEXT: usize = 48;
    /// Byte offset of `__ss.__pc` within `__darwin_mcontext64` - sixteen bytes of
    /// `__es`, then twenty-nine `__x` plus `__fp`, `__lr` and `__sp`.
    #[cfg(target_arch = "aarch64")]
    const MC_PC: usize = 16 + 256;
    /// Byte offset of `__ss.__rip` - sixteen bytes of `__es`, then the sixteen
    /// general-purpose registers `_STRUCT_X86_THREAD_STATE64` lists before it.
    #[cfg(target_arch = "x86_64")]
    const MC_PC: usize = 16 + 128;

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
/// Little-endian, so the low byte is the first character - which is the order
/// the text was written in memory.
fn ascii_word(addr: u64) -> Option<[u8; 8]> {
    let bytes = addr.to_le_bytes();
    bytes
        .iter()
        .all(|b| (0x20..0x7f).contains(b))
        .then_some(bytes)
}

/// A fixed message buffer, written out in one `write(2)`.
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

    /// Append what fits. Truncation is silent because the alternative - a handler that
    /// reports its own overflow - is not more useful than a shorter sentence.
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
        // The word the crash faulted on. Recognising it is what turned that
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
        // The claim the whole module turns on.
        placed(0x40_0000, 0x40_0000, 0x1_0000, 0x1_0010, 0x1_0000, &map(0x1_0000, 16));
        let placed = PLACED.lock().unwrap();
        let entry = placed.iter().find(|p| p.code == 0x40_0000).unwrap();

        assert_eq!(lookup(&entry.map, 0), Some(0x1_0000));
        assert_eq!(lookup(&entry.map, 15), Some(0x1_0000));
        assert_eq!(lookup(&entry.map, 16), Some(0x1_0001));
        assert_eq!(lookup(&entry.map, 0x57), Some(0x1_0005));
        // Past everything attributed: a prologue, an epilogue, or another
        // translation altogether.
        assert_eq!(lookup(&entry.map, 16 * 16), None);
    }

    #[test]
    fn the_nearest_translation_below_the_pc_is_the_one_reported() {
        placed(0x4000, 0x4000, 0x1_0000, 0x1_0100, 0x1_0000, &map(0x1_0000, 4));
        placed(0x8000, 0x8000, 0x2_0000, 0x2_0080, 0x2_0040, &map(0x2_0000, 4));
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
        // region was reported as if it were inside it.
        placed(0x10_0000, 0x10_0000, 0x3_0000, 0x3_0004, 0x3_0000, &map(0x3_0000, 4));
        let placed = PLACED.lock().unwrap();
        let entry = placed.iter().find(|p| p.code == 0x10_0000).unwrap();
        assert_eq!(entry.attributed_end(), 0x10_0040);
        assert_eq!(lookup(&entry.map, 0x3f), Some(0x3_0003));
        assert_eq!(lookup(&entry.map, 0x40), None);
    }

    /// The run loop enters through the thunk on every host dispatch, so a profile
    /// that cannot name it cannot see the busiest code in the process. Cranelift
    /// emits the thunk *above* the body, which is why it read as an epilogue.
    #[test]
    fn the_entry_stub_is_named_when_it_sits_above_the_body() {
        let (link_target, entry_point) = (0x50_0000, 0x50_0100);
        placed(
            entry_point,
            link_target,
            0x5_0000,
            0x5_0010,
            0x5_0000,
            &map(0x5_0000, 4),
        );
        assert_eq!(
            attribute(entry_point + 8),
            Attribution::Translated {
                region: 0x5_0000,
                why: Unnamed::Prologue,
            }
        );
        assert_eq!(
            attribute(link_target),
            Attribution::Instruction {
                guest_va: 0x5_0000,
                region: 0x5_0000,
            }
        );
    }

    /// And the other layout, which is what a region with no separate thunk has.
    #[test]
    fn the_entry_stub_below_the_link_target_belongs_to_its_region() {
        let (entry_point, link_target) = (0x50_0000, 0x50_0100);
        placed(
            entry_point,
            link_target,
            0x5_0000,
            0x5_0010,
            0x5_0000,
            &map(0x5_0000, 4),
        );
        // In the stub, below the address a link row names.
        assert_eq!(
            attribute(entry_point + 8),
            Attribution::Translated {
                region: 0x5_0000,
                why: Unnamed::Prologue,
            }
        );
        // And the map still reads against the body, not against the stub.
        assert_eq!(
            attribute(link_target),
            Attribution::Instruction {
                guest_va: 0x5_0000,
                region: 0x5_0000,
            }
        );
    }

    /// A translation whose map the budget discarded still owns its addresses.
    /// Reporting it as `Elsewhere` is what put 70% of a real profile in the
    /// unaccounted bucket.
    #[test]
    fn a_region_with_no_map_still_owns_its_code() {
        let code = 0x60_0000;
        placed(code, code, 0x6_0000, 0x6_0010, 0x6_0000, &map(0x6_0000, 0));
        assert_eq!(
            attribute(code + 0x40),
            Attribution::Translated {
                region: 0x6_0000,
                // Nothing was attributed, so this is the budget and not an epilogue.
                why: Unnamed::Budget,
            }
        );
    }

    /// The bound is the next blob, so an address between two translations belongs
    /// to the lower one and an address past the last is still refused.
    #[test]
    fn a_blob_ends_where_the_next_one_starts() {
        placed(0x70_0000, 0x70_0000, 0x7_0000, 0x7_0010, 0x7_0000, &map(0x7_0000, 1));
        placed(0x70_1000, 0x70_1000, 0x7_1000, 0x7_1010, 0x7_1000, &map(0x7_1000, 1));

        // Past its map but before its neighbour: still the first region's.
        assert_eq!(
            attribute(0x70_0800),
            Attribution::Translated {
                region: 0x7_0000,
                why: Unnamed::Beyond,
            }
        );
        // The neighbour's first byte is the neighbour's.
        assert!(matches!(
            attribute(0x70_1000),
            Attribution::Instruction { region: 0x7_1000, .. }
        ));
        // And nothing is claimed a megabyte past the last one.
        assert_eq!(attribute(0x70_1000 + MAX_BLOB), Attribution::Elsewhere);
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
