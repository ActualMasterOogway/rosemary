//! An in-process profiling overlay, drawn on the guest's own window.
//!
//! ## What it is for
//!
//! `docs/PROFILING.md` describes four instruments. Every one of them reports to
//! **stderr, on a timer, from a run that gets killed rather than exited**, and
//! that shape has a cost this project has already paid twice: the figures arrive
//! separated from the thing that produced them. A game stutters for two seconds
//! in one room, and the report that covers those two seconds is somewhere in a
//! log alongside the game's own logging, cumulative since launch, needing a
//! subtraction against the report before it to say anything about the room.
//!
//! So the same numbers, in the window, over the scene that produced them, with
//! the rates differenced over the last half second. Nothing here measures
//! anything new — [`crate::profile`] and [`crate::stats`] are still the
//! instruments, and [`panel`] is careful to inherit their caveats rather than
//! smooth them over. What is new is *when* they can be read.
//!
//! And one thing that is genuinely new: **shift+F6 attaches Instruments to this
//! process**, so the cycles-and-branch-misses half of the profile can be
//! recorded for a window someone chose while watching, instead of for whatever
//! the first thirty seconds after launch happened to contain. See [`trace`].
//!
//! ## The keys
//!
//! | | |
//! |---|---|
//! | **shift+F7** | show or hide the statistics panel |
//! | **shift+F6** | start or stop an Instruments recording of this process |
//! | **shift+F8** | give the mouse to the panel, or hand it back |
//! | **shift+F9** | start or stop counting region entries (`ROSETTA3_STATS`) |
//! | **shift+F10** | start or stop sampling the guest (`ROSETTA3_PROFILE`) |
//!
//! Registered with Carbon's `RegisterEventHotKey`, which needs no permission
//! and *consumes* the keystroke so the guest never sees it. See [`hotkey`].
//!
//! The last two are the ones worth explaining. `ROSETTA3_STATS` and
//! `ROSETTA3_PROFILE` used to be read once, at start-up, into a `OnceLock` — so
//! deciding you wanted a rate meant killing the game and playing back to the
//! scene. They are switches now, and `stats`'s in particular matters because it
//! is the counter this project has *measured* as perturbing its guest: 44% of a
//! steady-state window in the runtime with it off, 61% with it on. Being able to
//! turn that on for ten seconds and off again is the difference between
//! measuring a scene and re-creating it.
//!
//! shift+F8 makes the panel behave like a window — drag to move, drag an edge to
//! resize, wheel to scroll — and takes only the events aimed at it, so the guest
//! keeps working around it. See [`input`], which says why swallowing the rest is
//! the point rather than a side effect.
//!
//! ## How it draws
//!
//! Both graphics APIs a Mac program uses, at the one moment per frame that
//! matters — after the guest has finished drawing and before it is shown.
//! [`hook`] is where those two moments are caught, [`metal`] appends a render
//! pass to the guest's own command buffer, and [`gl`] draws into the back buffer
//! immediately before the swap and puts every piece of state it touched back.
//!
//! There is no window of this runtime's own, and that is the whole design. An
//! overlay in a second window would be a second window in the Dock, over the
//! wrong monitor, behind a full-screen game, and invisible in a screen
//! recording — which is exactly when a frame-time graph is wanted.
//!
//! ## What it costs when it is closed
//!
//! The panel is **hidden until asked for**. With it hidden the per-frame work is
//! a mutex, a timestamp and a comparison; the per-message work in [`hook`] is a
//! load and four integer compares. Nothing is compiled, no font is rasterized,
//! and CoreText is not even loaded until the first time the panel is opened —
//! see [`sys`] for why that is a `dlopen` rather than a link.
//!
//! ## Configuration
//!
//! | variable | |
//! |---|---|
//! | `ROSETTA3_GUI=0` | off entirely: no hotkeys, no hooks, no shim |
//! | `ROSETTA3_GUI_SCALE=<n>` | font scale, when the automatic one is wrong |
//! | `ROSETTA3_GUI_TEMPLATE=<name>` | the Instruments template shift+F6 records |
//! | `ROSETTA3_GUI_TRACE_DIR=<path>` | where a recording is written |
//! | `ROSETTA3_GUI_DEBUG=1` | narrate: which context, which path, which key route |
//! | `ROSETTA3_GUI_OPEN=1` | open the panel at start-up, with no keystroke |

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

pub mod draw;
pub mod font;
pub mod gl;
pub mod hook;
pub mod hotkey;
pub mod input;
pub mod metal;
pub mod objc;
pub mod panel;
pub mod sys;
pub mod trace;

use draw::{DrawList, color};
use panel::Model;

/// Whether the overlay is switched on at all.
///
/// On unless told otherwise, which is the opposite of every other
/// `ROSETTA3_*` switch in this runtime and is deliberate. The others turn on
/// *measurement*, which costs something; this turns on a **key binding**, and a
/// key binding nobody knows to set an environment variable for is a key binding
/// nobody uses. The cost of being on is stated in the module note, and
/// `ROSETTA3_GUI=0` removes even that — including the `CGLFlushDrawable` shim,
/// so a run with it off translates exactly as it did before this existed.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("ROSETTA3_GUI").as_deref().map(str::trim),
            Ok("0" | "no" | "off" | "false"),
        )
    })
}

/// Whether the present hooks should do anything.
///
/// Distinct from [`enabled`]: this goes false if a backend has decided it cannot
/// draw on this guest, so a failure costs one frame's attempt rather than one
/// per frame forever.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Whether the statistics panel is open.
static PANEL: AtomicBool = AtomicBool::new(false);

pub(crate) fn armed() -> bool {
    ARMED.load(Relaxed)
}

/// The model behind the panel, and the frame clock. One lock, taken once per
/// presented frame.
static MODEL: Mutex<Option<Model>> = Mutex::new(None);

/// Install the hotkeys and arm the present hooks.
///
/// Called from [`crate::startup::run`] before any guest code, beside the
/// profiler's own start, so that a program which draws its first frame during an
/// initializer is as overlayable as one that draws it an hour in.
pub fn install() {
    if !enabled() {
        return;
    }
    let _errno = crate::thread::Errno::held();
    hotkey::install();
    ARMED.store(true, Relaxed);
    // The panel, open from the first frame, without a keystroke.
    //
    // This exists to **take the hotkeys out of the question**. When the overlay
    // does not appear there are two entirely separate explanations — the
    // keystroke never arrived, or it arrived and the drawing failed — and from
    // outside the process they look identical. Setting this and seeing a panel
    // says it was the keys; setting it and seeing nothing says it was the
    // drawing, and the backends now name which step declined.
    if std::env::var_os("ROSETTA3_GUI_OPEN").is_some() {
        PANEL.store(true, Relaxed);
    }
    eprintln!(
        "rosetta3: gui  shift+F7 statistics, F6 record, F8 mouse, F9 entry counts, F10 sampling"
    );
}

/// Run `body` on a guest's thread without leaving a trace on it.
///
/// Two obligations, both about not being noticed by the program underneath.
///
/// **It must not unwind.** Everything the overlay does runs either on a guest
/// render thread inside a `presentDrawable:` the guest is in the middle of, or
/// inside a Carbon event handler. A panic crossing either is undefined behaviour
/// across a language boundary — the same rule this runtime already keeps for
/// shims, see [`crate::objc::dispatch`] on a message that raises. An overlay is
/// never worth a process, so a failure disarms it and says so once.
///
/// **It must not move `errno`.** The overlay is full of calls that would —
/// `dlopen` of a framework this system does not have sets `ENOENT`, and [`sys`]
/// does exactly that on first use. Every path from a guest thread into this
/// module holds a guard: [`install`], on the main thread before the guest
/// starts, and this, which wraps every present hook and the hotkey handler. See
/// [`crate::thread::Errno`].
pub(crate) fn guarded(body: impl FnOnce()) {
    let _errno = crate::thread::Errno::held();
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
        ARMED.store(false, Relaxed);
        say_once("the overlay panicked and has been switched off for this run");
    }
}

/// A diagnostic, at most once per distinct message.
///
/// Once, because every one of these is on a per-frame path: a backend that
/// could not build its pipeline would otherwise print sixty lines a second for
/// as long as the program runs, which is a way of destroying the log that
/// contains the answer.
pub(crate) fn say_once(message: &'static str) {
    static SAID: Mutex<Option<std::collections::BTreeSet<&'static str>>> = Mutex::new(None);
    let mut guard = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if guard.get_or_insert_with(Default::default).insert(message) {
        eprintln!("rosetta3: gui  {message}");
    }
}

/// A hotkey was pressed.
///
/// Debounced, and not only for key repeat: [`hotkey`] runs Carbon's registered
/// hot keys and a key-state poller at the same time, so one keystroke can arrive
/// twice before the poller learns it is redundant. Two toggles of a panel look
/// exactly like no toggle at all, which is the most confusing failure this could
/// have.
pub(crate) fn pressed(key: hotkey::Key) {
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);
    static LAST: Mutex<Option<(usize, std::time::Instant)>> = Mutex::new(None);
    {
        let slot = key as usize;
        let now = std::time::Instant::now();
        let mut guard = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some_and(|(was, at)| was == slot && now.duration_since(at) < DEBOUNCE) {
            return;
        }
        *guard = Some((slot, now));
    }
    guarded(|| match key {
        hotkey::Key::Trace => trace::toggle(),
        // Every one of these says what it did on stderr as well as on screen.
        // Three of them change *what is being measured*, and a measurement whose
        // instrument was switched on halfway through is worth nothing unless the
        // log says when.
        hotkey::Key::Focus => eprintln!(
            "rosetta3: gui  the {} has the mouse",
            match input::toggle_focus() {
                true => "panel",
                false => "guest",
            },
        ),
        hotkey::Key::Stats => eprintln!(
            "rosetta3: gui  region-entry counting {} (ROSETTA3_STATS)",
            match crate::stats::set_enabled(!crate::stats::enabled()) {
                true => "on — this costs the guest measurably; see docs/PROFILING.md",
                false => "off",
            },
        ),
        hotkey::Key::Profile => eprintln!(
            "rosetta3: gui  sampling {} (ROSETTA3_PROFILE)",
            match crate::profile::set_sampling(!crate::profile::sampling()) {
                true => "on — the histogram starts again from here",
                false => "off",
            },
        ),
        hotkey::Key::Statistics => {
            let open = !PANEL.fetch_xor(true, Relaxed);
            // On stderr too. A hidden panel and a broken overlay look identical
            // from the outside, and this is the line that tells them apart —
            // "opened" with nothing on screen means the guest presents through a
            // path [`hook`] does not catch.
            eprintln!(
                "rosetta3: gui  statistics panel {}",
                match open {
                    true => "opened",
                    false => "closed",
                }
            );
        }
    });
}

/// Build this frame's geometry, or `None` if there is nothing to draw.
///
/// The single entry point both backends share, which is what keeps them free of
/// any opinion about content. `width` and `height` are the drawable's, in
/// pixels.
pub(crate) fn compose(width: f32, height: f32) -> Option<DrawList> {
    let mut guard = MODEL.lock().unwrap_or_else(|e| e.into_inner());
    let model = guard.get_or_insert_with(Model::default);
    // Before the early return: frame timing is kept whether or not anyone is
    // looking, so that opening the panel shows the frame rate of the last two
    // seconds instead of starting a graph from nothing.
    model.note_frame();

    let panel_open = PANEL.load(Relaxed);
    let recording = trace::recording_for();
    let toast = trace::toast();
    if !panel_open && recording.is_none() && toast.is_none() {
        return None;
    }

    model.refresh();
    // Named rather than propagated with `?`. Every early return in this
    // function used to be silent, and a silent `None` here is indistinguishable
    // from a backend that drew and produced nothing visible — which cost two
    // rounds of guessing on a real game. The overlay now says which of its own
    // steps declined.
    let Some(atlas) = font::atlas(font_size(height)) else {
        say_once(
            "no font atlas: CoreText would not rasterize one, so there is nothing to draw with",
        );
        return None;
    };
    let mut list = DrawList::new(width, height, atlas);
    if panel_open {
        panel::statistics(&mut list, model);
    }
    // A recording always shows, panel or no panel. An overlay that records the
    // user's process without saying so on screen is not one worth having.
    match recording {
        Some(elapsed) => panel::badge(
            &mut list,
            &format!("REC {}", draw::duration(elapsed)),
            color::ALERT,
        ),
        None => {
            if let Some((message, colour)) = toast {
                panel::badge(&mut list, &message, colour);
            }
        }
    }
    if list.is_empty() {
        say_once("the panel produced no geometry, which is a bug in the panel");
        return None;
    }
    Some(list)
}

/// Whether `ROSETTA3_GUI_DEBUG` asked the overlay to narrate itself.
///
/// `ROSETTA3_GUI_GL` is accepted as the same thing: it was the OpenGL-only
/// spelling, and the questions it answers turned out not to be OpenGL's.
pub(crate) fn debugging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("ROSETTA3_GUI_DEBUG").is_some()
            || std::env::var_os("ROSETTA3_GUI_GL").is_some()
    })
}

/// The overlay reached a backend and put geometry in front of the guest.
///
/// **The single most useful line this thing prints**, and it is printed once,
/// unconditionally. Everything before it — the hook firing, the surface size,
/// the atlas, the panel's layout — is invisible from outside the process, so
/// "nothing appeared" has until now been consistent with a dozen different
/// failures. After this line it is consistent with exactly one: the geometry
/// went somewhere the display does not show.
pub(crate) fn drew(backend: &str, width: f32, height: f32, vertices: usize) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Relaxed) {
        eprintln!(
            "rosetta3: gui  drew {vertices} vertices over {width:.0}x{height:.0} through {backend}"
        );
    }
}

/// The font size for a surface this tall.
///
/// Scaled off the drawable rather than off the display's backing factor, and
/// that is the more honest input: the overlay is drawn in the drawable's own
/// pixels, and a guest that renders at half resolution and lets the compositor
/// upscale has a smaller drawable than its window. Reading the screen's scale
/// would make the text half-size in exactly that case.
fn font_size(height: f32) -> f32 {
    let scale = std::env::var("ROSETTA3_GUI_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or_else(|| (height / 900.0).clamp(1.0, 3.0));
    (13.0 * scale).round()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that drive [`PANEL`].
    ///
    /// The panel's open flag is a process-global, correctly: there is one
    /// overlay per process and a hotkey handler on a thread of Carbon's choosing
    /// has to reach it. Three tests here set it, and `cargo test` runs them on
    /// three threads at once — so `a_repeated_press_within_the_debounce_is_one_toggle`
    /// closing the panel between another test's `store(true)` and its `compose`
    /// made that test fail on roughly one run in six, with a message ("an open
    /// panel produces a draw list") about the composer rather than about the
    /// race.
    ///
    /// Serializing rather than making the flag per-test, because the flag being
    /// global is the thing under test in the debounce case: a `PANEL` a test
    /// could have to itself would no longer be the one the hotkey path toggles.
    fn panel() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        // A panicking test leaves the flag wrong for the next one, which the
        // `store` at the top of each of these already fixes; the lock itself
        // only has to stay usable.
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Everything both backends share, exercised without a GPU.
    ///
    /// **This is the test that was missing**, and its absence cost two rounds of
    /// guessing at a real game. When A Hat in Time and Roblox both showed
    /// nothing — on OpenGL and on Metal, two backends with no code in common —
    /// the shared suspects were the font atlas, the panel's layout and the
    /// two-pass measure, and there was no way to rule any of them out except by
    /// asking someone to run a game again.
    ///
    /// [`compose`] is exactly that shared part: everything from "the panel is
    /// open" to "here are the triangles", with no graphics API below it. If this
    /// passes, a blank overlay is the backend's or the hotkey's, and those are
    /// separately reported now.
    #[test]
    fn the_panel_produces_geometry_without_a_gpu() {
        let _panel = panel();
        if font::atlas(13.0).is_none() {
            // No CoreText, no overlay — a supported configuration.
            return;
        }
        // Closed, there is nothing to draw and that is not a failure.
        PANEL.store(false, Relaxed);
        assert!(compose(1920.0, 1080.0).is_none());

        PANEL.store(true, Relaxed);
        let list = compose(1920.0, 1080.0).expect("an open panel produces a draw list");
        assert!(
            list.vertices.len() > 600,
            "the panel drew only {} vertices, which is too few to be the panel",
            list.vertices.len(),
        );
        // Every vertex inside the surface, because a projection that is off by a
        // factor — the retina case — puts the panel off-screen rather than
        // making it look wrong, and off-screen is indistinguishable from not
        // drawn.
        for vertex in &list.vertices {
            let [x, y] = vertex.pos;
            assert!(
                (0.0..=1920.0).contains(&x) && (0.0..=1080.0).contains(&y),
                "vertex at ({x}, {y}) is outside the 1920x1080 surface",
            );
        }
        PANEL.store(false, Relaxed);
    }

    /// A scrolled panel stays inside its own rectangle.
    ///
    /// Scrolling without clipping does not look like a scrolling bug — it looks
    /// like the overlay is drawing over the game, in rows that appear above the
    /// panel and never go away. This asserts the thing that prevents it, on the
    /// content of a real panel rather than on a synthetic quad.
    #[test]
    fn a_scrolled_panel_draws_only_inside_itself() {
        let _panel = panel();
        if font::atlas(13.0).is_none() {
            return;
        }
        PANEL.store(true, Relaxed);
        // A surface short enough that the panel certainly overflows it, which is
        // the only case scrolling exists for.
        let list = compose(1280.0, 320.0).expect("an open panel produces a draw list");
        let (x, y, ..) = input::view();
        for vertex in &list.vertices {
            let [vx, vy] = vertex.pos;
            assert!(
                vx >= x - 1.0 && vy >= y - 1.0 && vy <= 320.0,
                "a vertex at ({vx}, {vy}) escaped a panel at ({x}, {y}) on a 1280x320 surface",
            );
        }
        PANEL.store(false, Relaxed);
    }

    /// Clipping trims the texture along with the geometry.
    ///
    /// A quad whose position is clipped but whose texture coordinates are not
    /// shows a *squashed* glyph rather than a cropped one — legible enough to
    /// pass a glance and wrong everywhere it matters. The proportions are what
    /// this checks.
    #[test]
    fn a_clipped_quad_trims_its_texture_by_the_same_fraction() {
        let Some(atlas) = font::atlas(13.0) else {
            return;
        };
        let mut list = DrawList::new(100.0, 100.0, atlas);
        // Half of a glyph, cut horizontally through the middle.
        list.clip((0.0, 0.0, 100.0, 50.0));
        list.text(0.0, 40.0, "M", color::VALUE);
        assert!(!list.vertices.is_empty(), "the glyph was dropped entirely");

        let (u0, v0, u1, v1) = atlas.glyph(b'M');
        let top = list
            .vertices
            .iter()
            .map(|v| v.uv[1])
            .fold(f32::MAX, f32::min);
        let bottom = list
            .vertices
            .iter()
            .map(|v| v.uv[1])
            .fold(f32::MIN, f32::max);
        let cut = (50.0 - 40.0) / atlas.cell_h as f32;
        assert!((top - v0).abs() < 1e-4, "the top edge moved: {top} vs {v0}");
        assert!(
            (bottom - (v0 + (v1 - v0) * cut)).abs() < 1e-3,
            "the bottom texture coordinate is {bottom}, not the {cut} fraction of {v0}..{v1}",
        );
        // Horizontally untouched, because nothing cut it there.
        let left = list
            .vertices
            .iter()
            .map(|v| v.uv[0])
            .fold(f32::MAX, f32::min);
        let right = list
            .vertices
            .iter()
            .map(|v| v.uv[0])
            .fold(f32::MIN, f32::max);
        assert!((left - u0).abs() < 1e-4 && (right - u1).abs() < 1e-4);
    }

    /// One keystroke is one toggle, however many mechanisms deliver it.
    ///
    /// [`hotkey`] deliberately runs Carbon's hot keys and a poller at once, so
    /// the same press can arrive twice — and two toggles look exactly like none.
    #[test]
    fn a_repeated_press_within_the_debounce_is_one_toggle() {
        let _panel = panel();
        PANEL.store(false, Relaxed);
        pressed(hotkey::Key::Statistics);
        assert!(
            PANEL.load(Relaxed),
            "the first press did not open the panel"
        );
        pressed(hotkey::Key::Statistics);
        assert!(
            PANEL.load(Relaxed),
            "a duplicate delivery of one press closed the panel again",
        );
        PANEL.store(false, Relaxed);
    }
}
