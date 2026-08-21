//! What the overlay actually says, and the windows it says it in.

use std::time::{Duration, Instant};

use super::draw::{Color, DrawList, Ui, bytes, color, duration, grouped, rate};
use crate::{profile, stats};

/// One window's worth of the overlay.
///
/// The enumeration is the whole catalogue: [`super::dock`] arranges these and
/// the Windows menu lists them, so a new section is a variant and a `body` arm
/// and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Frames,
    Time,
    Regions,
    Instructions,
    HostSymbols,
    Entries,
    Translation,
    Footprint,
    Trace,
    Keys,
}

impl View {
    /// Every view, in the order the Windows menu lists them.
    pub const ALL: [View; 10] = [
        View::Frames,
        View::Time,
        View::Regions,
        View::Instructions,
        View::HostSymbols,
        View::Entries,
        View::Translation,
        View::Footprint,
        View::Trace,
        View::Keys,
    ];

    /// What the tab says.
    pub fn title(self) -> &'static str {
        match self {
            View::Frames => "FPS",
            View::Time => "Time Profiling",
            View::Regions => "Guest Regions",
            View::Instructions => "Guest Instructions",
            View::HostSymbols => "Host Symbols",
            View::Entries => "Region Entries",
            View::Translation => "Translation",
            View::Footprint => "Footprint",
            View::Trace => "xctrace",
            View::Keys => "Keys",
        }
    }

    /// Its slot in the array the hidden flags live in.
    pub fn slot(self) -> usize {
        View::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }
}

/// Draw one view's contents at the cursor.
pub fn body(ui: &mut Ui, model: &Model, view: View) {
    match view {
        View::Frames => frames(ui, model),
        View::Time => where_time_goes(ui, model),
        View::Regions => regions(ui, model),
        View::Instructions => instructions(ui, model),
        View::HostSymbols => host_symbols(ui, model),
        View::Entries => entries(ui, model),
        View::Translation => translation(ui, model),
        View::Footprint => footprint(ui, model),
        View::Trace => super::trace::window(ui),
        View::Keys => keys(ui),
    }
}

/// How many entries of each ranked list to ask the profiler for.
const RANK: usize = 8;

/// How often the snapshots behind the panel are rebuilt.
const REFRESH: Duration = Duration::from_millis(500);

/// Frames kept for the frame-time graph.
const FRAMES: usize = 120;

/// Everything the panel draws, sampled off the hot path.
pub struct Model {
    /// When each of the last [`FRAMES`] frames was presented, as intervals.
    frames: [f32; FRAMES],
    next_frame: usize,
    frames_seen: usize,
    last_present: Option<Instant>,

    /// The last refresh, and what it produced.
    refreshed: Option<Instant>,
    profile: Option<profile::Snapshot>,
    stats: stats::Snapshot,
    /// The refresh before last, which is what turns a cumulative counter into a
    /// rate over a window a person can perceive.
    previous: Option<(Instant, stats::Snapshot)>,
    window: Duration,

    /// Translations and thunks, read from the VM rather than from a counter.
    translations: usize,
    thunks: usize,
    resident: u64,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            frames: [0.0; FRAMES],
            next_frame: 0,
            frames_seen: 0,
            last_present: None,
            refreshed: None,
            profile: None,
            stats: stats::Snapshot::default(),
            previous: None,
            window: Duration::ZERO,
            translations: 0,
            thunks: 0,
            resident: 0,
        }
    }
}

impl Model {
    /// One presented frame.
    pub fn note_frame(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_present.replace(now) {
            self.frames[self.next_frame] = now.duration_since(last).as_secs_f32();
            self.next_frame = (self.next_frame + 1) % FRAMES;
            self.frames_seen = (self.frames_seen + 1).min(FRAMES);
        }
    }

    /// Rebuild the snapshots, if they are old enough to be worth rebuilding.
    pub fn refresh(&mut self) {
        let now = Instant::now();
        if self
            .refreshed
            .is_some_and(|t| now.duration_since(t) < REFRESH)
        {
            return;
        }
        // The snapshot about to be replaced becomes the one the next window is
        // differenced against.
        if let Some(at) = self.refreshed {
            self.window = now.duration_since(at);
            self.previous = Some((at, std::mem::take(&mut self.stats)));
        }
        self.refreshed = Some(now);
        self.profile = profile::snapshot(RANK);
        self.stats = stats::snapshot();
        if let Some(vm) = crate::thunk::current() {
            self.translations = vm.translated();
            self.thunks = vm.thunks();
        }
        self.resident = resident_bytes();
    }

    /// Mean frame interval over the ring, in seconds.
    fn frame_time(&self) -> f32 {
        match self.frames_seen {
            0 => 0.0,
            n => self.frames[..n].iter().sum::<f32>() / n as f32,
        }
    }

    /// The worst frame in the ring - the number a stutter shows up in, and the
    /// one a mean hides completely.
    fn worst_frame(&self) -> f32 {
        self.frames[..self.frames_seen]
            .iter()
            .copied()
            .fold(0.0, f32::max)
    }

    /// A counter's increase over the last window, and the rate that works out to.
    fn per_second(&self, of: impl Fn(&stats::Snapshot) -> u64) -> Option<f64> {
        let (_, previous) = self.previous.as_ref()?;
        let delta = of(&self.stats).saturating_sub(of(previous));
        Some(delta as f64 / self.window.as_secs_f64().max(f64::EPSILON))
    }
}

/// This process's resident size, or zero.
///
/// The guest's memory and this runtime's are the same footprint and there is no way to
/// separate them from inside - every translation, every thunk and every guest page is
/// in one task.
fn resident_bytes() -> u64 {
    /// `MACH_TASK_BASIC_INFO`, and its size in 32-bit words: two `mach_vm_size_t`
    /// and a third, two `time_value_t`, then `policy` and `suspend_count`.
    const MACH_TASK_BASIC_INFO: u32 = 20;
    const COUNT: u32 = 12;

    #[repr(C)]
    #[derive(Default)]
    struct Basic {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: [i32; 2],
        system_time: [i32; 2],
        policy: i32,
        suspend_count: i32,
    }

    unsafe extern "C" {
        fn task_info(task: u32, flavor: u32, out: *mut Basic, count: *mut u32) -> i32;
    }

    // `mach_task_self()` is a macro over a global the kernel filled in, which
    // is why this is a symbol lookup and not a call.
    let Some(slot) = crate::host::symbol("mach_task_self_") else {
        return 0;
    };
    // SAFETY: the symbol is `mach_port_t mach_task_self_`, a live global in
    // libsystem_kernel for the life of the process.
    let task = unsafe { *(slot as *const u32) };
    let mut info = Basic::default();
    let mut count = COUNT;
    // SAFETY: `info` is a live `mach_task_basic_info_data_t` and `count` says so.
    match unsafe { task_info(task, MACH_TASK_BASIC_INFO, &mut info, &mut count) } {
        0 => info.resident_size,
        _ => 0,
    }
}

/// What each key does, and what state it is in.
fn keys(ui: &mut Ui) {
    ui.heading("KEYS");
    for (key, what, state, colour) in [
        (
            "shift+F6",
            "record with Instruments",
            match super::trace::recording_for().is_some() {
                true => "recording",
                false => "idle",
            },
            color::VALUE,
        ),
        ("shift+F7", "the whole overlay", "open", color::VALUE),
        (
            "shift+F8",
            "give the mouse to the overlay",
            match super::input::focused() {
                true => "the panel has the mouse",
                false => "the guest has the mouse",
            },
            match super::input::focused() {
                true => color::ALERT,
                false => color::VALUE,
            },
        ),
        (
            "shift+F9",
            "count region entries",
            match crate::stats::enabled() {
                true => "on, and costing the guest",
                false => "off",
            },
            match crate::stats::enabled() {
                true => color::RUNTIME,
                false => color::MUTED,
            },
        ),
        (
            "shift+F10",
            "sample the guest",
            match crate::profile::sampling() {
                true => "on",
                false => "off",
            },
            match crate::profile::sampling() {
                true => color::GUEST,
                false => color::MUTED,
            },
        ),
    ] {
        ui.run(key, color::LABEL);
        ui.column(11.0);
        ui.run(what, color::LABEL);
        ui.column(40.0);
        ui.run(state, colour);
        ui.newline();
    }
}

fn frames(ui: &mut Ui, model: &Model) {
    ui.heading("FRAME");
    let mean = model.frame_time();
    let worst = model.worst_frame();
    match mean > 0.0 {
        true => ui.row(
            "presented",
            &format!(
                "{:.2} ms  {:.1} fps    worst {:.2} ms",
                mean * 1e3,
                1.0 / mean,
                worst * 1e3,
            ),
            color::VALUE,
        ),
        // A window that has never presented is the console case, and also the
        // case where the overlay is being drawn by a backend that is not the one
        // the guest presents through. Saying so beats printing `inf fps`.
        false => ui.row("presented", "no frame seen yet", color::MUTED),
    }
    graph(ui, model, worst);
}

/// The last two seconds of frame intervals, as bars.
fn graph(ui: &mut Ui, model: &Model, worst: f32) {
    if model.frames_seen == 0 {
        return;
    }
    let h = ui.leading * 2.0;
    let w = ui.width;
    let (x, y) = (ui.left, ui.y);
    ui.list.rect(x, y, w, h, color::TRACK);
    let scale = worst.max(1.0 / 30.0);
    let step = w / FRAMES as f32;
    for i in 0..model.frames_seen {
        // Oldest first: the ring's write cursor is where the oldest sample is.
        let slot = (model.next_frame + FRAMES - model.frames_seen + i) % FRAMES;
        let dt = model.frames[slot];
        let bar = (dt / scale * h).clamp(1.0, h);
        // Anything past a sixtieth of a second is a frame that missed, on the
        // only display refresh rate it is safe to assume something about.
        let colour = match dt > 1.0 / 60.0 {
            true => color::RUNTIME,
            false => color::GUEST,
        };
        ui.list
            .rect(x + i as f32 * step, y + h - bar, step.max(1.0), bar, colour);
    }
    ui.y += h + ui.leading * 0.3;
}

fn where_time_goes(ui: &mut Ui, model: &Model) {
    ui.heading("TIME PROFILING");
    let Some(p) = model.profile.as_ref() else {
        sampler_off(ui);
        return;
    };
    if p.running == 0 {
        ui.line("no running samples yet", color::MUTED);
        return;
    }
    ui.bar(
        "translated",
        &format!("{:>6.2}%", p.translated),
        p.translated,
        color::GUEST,
    );
    ui.bar(
        "runtime",
        &format!("{:>6.2}%", p.runtime),
        p.runtime,
        color::RUNTIME,
    );
    ui.bar(
        "host libraries",
        &format!("{:>6.2}%", p.library),
        p.library,
        color::HOST,
    );
    ui.bar(
        "unaccounted",
        &format!("{:>6.2}%", p.unaccounted),
        p.unaccounted,
        color::MUTED,
    );
    ui.row(
        "samples",
        &format!(
            "{} running, {} blocked, over {}",
            grouped(p.running),
            grouped(p.blocked),
            duration(p.wall),
        ),
        color::VALUE,
    );
    ui.row(
        "sampler",
        &format!(
            "{} thread(s), {:.0} sweep/s, {:.2}% of a core",
            p.threads, p.sweeps_per_second, p.sampler_share,
        ),
        color::MUTED,
    );
}

fn regions(ui: &mut Ui, model: &Model) {
    match model.profile.as_ref() {
        Some(p) => rows(ui, "GUEST REGIONS", &p.regions, color::GUEST, None),
        None => sampler_off(ui),
    }
}

fn instructions(ui: &mut Ui, model: &Model) {
    match model.profile.as_ref() {
        Some(p) => rows(
            ui,
            "GUEST INSTRUCTIONS",
            &p.instructions,
            color::MUTED,
            // The caveat is on the panel rather than in the documentation because
            // this is the ranking someone reads at three in the morning and acts on.
            Some("source symbols likely not available"),
        ),
        None => sampler_off(ui),
    }
}

fn host_symbols(ui: &mut Ui, model: &Model) {
    ui.heading("HOST SYMBOLS");
    let Some(p) = model.profile.as_ref() else {
        ui.line(SAMPLER_OFF, color::MUTED);
        return;
    };
    if p.host.is_empty() {
        ui.line("no host samples yet", color::MUTED);
        return;
    }
    for site in &p.host {
        ui.run(&format!("{:>6.2}%", site.share), color::VALUE);
        ui.column(9.0);
        let (tag, colour) = match site.runtime {
            true => ("[runtime]", color::RUNTIME),
            false => ("[library]", color::HOST),
        };
        ui.run(tag, colour);
        ui.column(19.0);
        ui.run(&format!("{} {}", site.image, site.symbol), color::LABEL);
        ui.newline();
    }
}

/// What every sampled view says when the sampler is off.
const SAMPLER_OFF: &str = "the in-process sampler is off: shift+F10, or ROSETTA3_PROFILE=1";

fn sampler_off(ui: &mut Ui) {
    ui.line(SAMPLER_OFF, color::MUTED);
}

fn rows(ui: &mut Ui, title: &str, entries: &[profile::Entry], colour: Color, caveat: Option<&str>) {
    ui.heading(title);
    if entries.is_empty() {
        ui.line("no samples yet", color::MUTED);
        return;
    }
    if let Some(note) = caveat {
        ui.line(note, color::MUTED);
    }
    for entry in entries {
        ui.run(&format!("{:>6.2}%", entry.share), colour);
        ui.column(9.0);
        ui.run(&format!("{:#018x}", entry.va), color::VALUE);
        ui.column(29.0);
        ui.run(
            match entry.located.is_empty() {
                // Not a gap.
                true => "(generated code)",
                false => &entry.located,
            },
            color::LABEL,
        );
        ui.newline();
    }
}

fn entries(ui: &mut Ui, model: &Model) {
    ui.heading("REGION ENTRIES");
    let s = &model.stats;
    if !s.counted {
        ui.line(
            "not counted: ROSETTA3_STATS=1 turns the exit histogram on",
            color::MUTED,
        );
        ui.line(
            "it costs measurably - quote shares from a run with it off",
            color::MUTED,
        );
        return;
    }
    let now = model
        .per_second(|s| s.entries)
        .map(|r| format!("{} now", rate(r)))
        .unwrap_or_else(|| "measuring".into());
    ui.row(
        "entries",
        &format!("{} total, {now}", grouped(s.entries)),
        color::VALUE,
    );
    let share = |n: u64| 100.0 * n as f64 / s.entries.max(1) as f64;
    ui.bar(
        "window misses",
        &format!("{:>6.2}%", share(s.window_misses)),
        share(s.window_misses),
        color::RUNTIME,
    );
    ui.bar(
        "tail call to import",
        &format!("{:>6.2}%", share(s.tail_imports)),
        share(s.tail_imports),
        color::HOST,
    );
    match s.pinned_link_registers {
        // Under a pinned link ABI a link is a branch inside translated code, so
        // nothing on that path can count without putting a contended store on
        // it - and what `entries` then counts is chains, not regions.
        Some(n) => {
            ui.row(
                "block linking",
                &format!("pinned ABI, {n} register(s)"),
                color::GUEST,
            );
            ui.line(
                "entries above are chains: a link is a branch and is not counted",
                color::MUTED,
            );
        }
        None => ui.bar(
            "linked",
            &format!("{:>6.2}%", share(s.linked)),
            share(s.linked),
            color::GUEST,
        ),
    }
    if s.exits.is_empty() {
        return;
    }
    ui.gap(0.2);
    for (label, count) in s.exits.iter().take(6) {
        let share = share(*count);
        ui.bar(label, &format!("{share:>6.2}%"), share, color::HOST);
    }
}

fn translation(ui: &mut Ui, model: &Model) {
    ui.heading("TRANSLATION");
    let s = &model.stats;
    if s.regions == 0 {
        ui.line("nothing lifted yet", color::MUTED);
        return;
    }
    ui.row(
        "regions",
        &format!(
            "{} lifted, {:.1} instr each, {}",
            grouped(s.regions),
            s.instructions as f64 / s.regions as f64,
            bytes(s.bytes),
        ),
        color::VALUE,
    );
    // A rate, because the question a live panel answers is whether the program
    // has *stopped* translating - steady state is what every other figure here
    // is worth reading in.
    if let Some(per_second) = model.per_second(|s| s.regions) {
        ui.row(
            "lifting",
            &match per_second {
                r if r < 0.5 => "steady state: nothing new is being lifted".to_string(),
                r => format!("{} region(s) now", rate(r)),
            },
            match per_second < 0.5 {
                true => color::GUEST,
                false => color::RUNTIME,
            },
        );
    }
    let total = s.select + s.lift + s.compile;
    let share = |d: Duration| 100.0 * d.as_secs_f64() / s.wall.as_secs_f64().max(f64::EPSILON);
    ui.row(
        "select/lift/compile",
        &format!(
            "{} / {} / {}",
            duration(s.select),
            duration(s.lift),
            duration(s.compile)
        ),
        color::VALUE,
    );
    ui.bar(
        "of wall clock",
        &format!("{:>6.2}%", share(total)),
        share(total),
        color::RUNTIME,
    );
    if s.refused > 0 || s.tail_exits > 0 {
        ui.row(
            "refused / tail exits",
            &format!("{} / {}", grouped(s.refused), grouped(s.tail_exits)),
            match s.refused > 0 {
                true => color::ALERT,
                false => color::VALUE,
            },
        );
    }
}

fn footprint(ui: &mut Ui, model: &Model) {
    ui.heading("FOOTPRINT");
    ui.row(
        "translations",
        &format!(
            "{} in the cache, {} thunk(s) handed out",
            grouped(model.translations as u64),
            grouped(model.thunks as u64),
        ),
        color::VALUE,
    );
    if let Some((small, large)) = model.stats.pages {
        ui.row(
            "guest code touched",
            &format!(
                "{} page(s) of 4 KiB, {} of 16 KiB",
                grouped(small as u64),
                grouped(large as u64)
            ),
            color::VALUE,
        );
    }
    if let Some((made, substituted, reached)) = model.stats.trampolines {
        ui.row(
            "import trampolines",
            &format!(
                "{} made, {} substituted, {} reached",
                grouped(made),
                grouped(substituted),
                grouped(reached)
            ),
            color::VALUE,
        );
    }
    ui.row(
        "resident",
        &format!("{} (guest and runtime together)", bytes(model.resident)),
        color::VALUE,
    );
}

/// The badge shown when the panel is closed but something is happening.
pub fn badge(list: &mut DrawList, text: &str, colour: Color) {
    let line = list.atlas().line_h;
    let w = list.measure(text) + 20.0;
    let h = line + 12.0;
    let x = list.width - w - 12.0;
    let y = 12.0;
    list.rect(x, y, w, h, color::PANEL);
    list.frame(x, y, w, h, colour);
    list.text(x + 10.0, y + 6.0, text, colour);
}
