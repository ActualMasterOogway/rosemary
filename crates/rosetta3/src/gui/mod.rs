//! An in-process profiling overlay, drawn on the guest's own window.

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
pub fn install() {
    if !enabled() {
        return;
    }
    let _errno = crate::thread::Errno::held();
    hotkey::install();
    ARMED.store(true, Relaxed);
    // The panel, open from the first frame, without a keystroke. This exists to
    // **take the hotkeys out of the question**.
    if std::env::var_os("ROSETTA3_GUI_OPEN").is_some() {
        PANEL.store(true, Relaxed);
    }
    eprintln!(
        "rosetta3: gui  shift+F7 statistics, F6 record, F8 mouse, F9 entry counts, F10 sampling"
    );
}

/// Run `body` on a guest's thread without leaving a trace on it.
///
/// **It must not unwind.** Everything the overlay does runs either on a guest render
/// thread inside a `presentDrawable:` the guest is in the middle of, or inside a Carbon
/// event handler.
///
/// **It must not move `errno`.** The overlay is full of calls that would — `dlopen` of
/// a framework this system does not have sets `ENOENT`, and [`sys`] does exactly that
/// on first use.
pub(crate) fn guarded(body: impl FnOnce()) {
    let _errno = crate::thread::Errno::held();
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
        ARMED.store(false, Relaxed);
        say_once("the overlay panicked and has been switched off for this run");
    }
}

/// A diagnostic, at most once per distinct message.
pub(crate) fn say_once(message: &'static str) {
    static SAID: Mutex<Option<std::collections::BTreeSet<&'static str>>> = Mutex::new(None);
    let mut guard = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if guard.get_or_insert_with(Default::default).insert(message) {
        eprintln!("rosetta3: gui  {message}");
    }
}

/// A hotkey was pressed.
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
            // On stderr too.
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
pub(crate) fn compose(width: f32, height: f32) -> Option<DrawList> {
    let mut guard = MODEL.lock().unwrap_or_else(|e| e.into_inner());
    let model = guard.get_or_insert_with(Model::default);
    // Before the early return.
    model.note_frame();

    let panel_open = PANEL.load(Relaxed);
    let recording = trace::recording_for();
    let toast = trace::toast();
    if !panel_open && recording.is_none() && toast.is_none() {
        return None;
    }

    model.refresh();
    // Named rather than propagated with `?`.
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
pub(crate) fn debugging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("ROSETTA3_GUI_DEBUG").is_some()
            || std::env::var_os("ROSETTA3_GUI_GL").is_some()
    })
}

/// The overlay reached a backend and put geometry in front of the guest.
pub(crate) fn drew(backend: &str, width: f32, height: f32, vertices: usize) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Relaxed) {
        eprintln!(
            "rosetta3: gui  drew {vertices} vertices over {width:.0}x{height:.0} through {backend}"
        );
    }
}

/// The font size for a surface this tall.
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
    fn panel() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        // A panicking test leaves the flag wrong for the next one, which the
        // `store` at the top of each of these already fixes; the lock itself
        // only has to stay usable.
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Everything both backends share, exercised without a GPU.
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
    /// Scrolling without clipping does not look like a scrolling bug — it looks like
    /// the overlay is drawing over the game, in rows that appear above the panel and
    /// never go away.
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
    /// A quad whose position is clipped but whose texture coordinates are not shows a
    /// *squashed* glyph rather than a cropped one — legible enough to pass a glance and
    /// wrong everywhere it matters.
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
