//! Driving Apple's system tracer from inside the process it is tracing.
//!
//! ## What shift+F6 is for
//!
//! The in-process sampler answers *which guest instruction*. It cannot answer
//! *cycles*, *branch mispredictions* or *cache misses*, and `docs/PROFILING.md`
//! is explicit that those are what separate "this guest loop is hot because it
//! runs a lot" from "this guest loop is hot because its translation mispredicts
//! every iteration" — two findings with nothing in common as work items. Only
//! Apple's own tooling can see them.
//!
//! Until now getting that meant `tools/guest-trace.sh`, which *launches* the
//! guest under `xctrace`. That works and has one hard limitation: the window
//! being recorded is decided before the program starts. A game's interesting
//! seconds are forty minutes in, in one particular scene, and a trace that
//! started at the launch screen is forty minutes of menu.
//!
//! So this **attaches** instead, on a keystroke, to the process it is running
//! inside. The recording is the seconds someone chose to record.
//!
//! ## The map, and why it comes from the same run
//!
//! A trace records host addresses in whatever anonymous mappings the JIT got
//! *this* time. [`crate::profile::write_map`] is the table that says what those
//! addresses were lifted from, and it is only meaningful for the run that
//! produced it — so the map is written when the recording stops, beside the
//! trace, from the process that was recorded. `tools/guest-symbolicate.py` joins
//! the two afterwards.
//!
//! ## What will stop you
//!
//! Both of the conditions `docs/PROFILING.md` lists for the launch path apply
//! here, and one more that is specific to attaching:
//!
//! - **The full Xcode**, not the Command Line Tools. `/usr/bin/xctrace` exists
//!   either way and fails with "requires Xcode" in the second case, so this
//!   tests it by *running* it rather than by looking for the file.
//! - **`get-task-allow`** on the driver, or `xctrace` cannot open the task. A
//!   `cargo`-built binary is linker-signed with no entitlements. See the
//!   `codesign` line in `docs/PROFILING.md`.
//! - **Attaching needs the recording to outlive the keystroke**, which means
//!   `xctrace` is a child process this one does not wait for. It is stopped
//!   with `SIGINT`, which is what makes `xctrace` finalize the trace rather than
//!   discard it — killing it any other way leaves an unreadable bundle.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use super::draw::{Color, Ui, color, duration};

/// The template recorded when nothing else is asked for.
///
/// `Time Profiler` rather than `Processor Trace`, and that is a finding rather
/// than a preference: a Processor Trace's locations are `(binary UUID, Mach-O
/// offset)` pairs symbolicated at record time, so every translation folds into
/// one all-zero sentinel and the map has nothing to join against. Time Profiler
/// carries `<frame addr="0x…">`, which is an address, which is what a map
/// resolves. See `docs/PROFILING.md`, "What Processor Trace turned out to be".
const DEFAULT_TEMPLATE: &str = "Time Profiler";

/// A recording in flight.
struct Recording {
    child: Child,
    started: Instant,
    output: String,
}

/// What the overlay is doing about tracing, and the last thing it said.
#[derive(Default)]
struct State {
    recording: Option<Recording>,
    /// The last outcome, and when — shown on the panel and as a badge for a few
    /// seconds, because a keystroke that silently did nothing is worse than no
    /// keystroke at all.
    said: Option<(Instant, String, Color)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// How long a message stays on screen after the keystroke that produced it.
const TOAST: std::time::Duration = std::time::Duration::from_secs(6);

/// Start a recording, or stop the one in flight. Bound to shift+F6.
pub fn toggle() {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(State::default);
    match state.recording.is_some() {
        true => stop(state),
        false => start(state),
    }
}

/// Whether a recording is in flight, and for how long.
pub fn recording_for() -> Option<std::time::Duration> {
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.as_ref()?;
    state.recording.as_ref().map(|r| r.started.elapsed())
}

/// The message to show as a badge, if there is a recent one.
pub fn toast() -> Option<(String, Color)> {
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let (at, message, colour) = guard.as_ref()?.said.as_ref()?;
    (at.elapsed() < TOAST).then(|| (message.clone(), *colour))
}

fn say(state: &mut State, message: impl Into<String>, colour: Color) {
    let message = message.into();
    // On stderr as well as on screen. A run being measured is a run whose
    // terminal is being watched, and the path a trace landed at is a thing to
    // copy and paste rather than read off a game.
    eprintln!("rosetta3: gui  {message}");
    state.said = Some((Instant::now(), message, colour));
}

fn start(state: &mut State) {
    let Some(xctrace) = xctrace() else {
        say(
            state,
            "no working xctrace: this needs the full Xcode, not the Command Line Tools",
            color::ALERT,
        );
        return;
    };
    let template =
        std::env::var("ROSETTA3_GUI_TEMPLATE").unwrap_or_else(|_| DEFAULT_TEMPLATE.to_string());
    let output = output_path();

    // `--attach` rather than `--launch`: the process to record is this one, and
    // it has been running for as long as it takes to get to the scene worth
    // recording. Output is inherited so that `xctrace`'s own progress and its
    // errors — which are the interesting half when this does not work — reach
    // the same terminal as everything else this runtime prints.
    let child = Command::new(xctrace)
        .arg("record")
        .arg("--template")
        .arg(&template)
        .arg("--output")
        .arg(&output)
        .arg("--attach")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .spawn();

    match child {
        Ok(child) => {
            state.recording = Some(Recording {
                child,
                started: Instant::now(),
                output: output.clone(),
            });
            say(
                state,
                format!("recording '{template}' to {output} — shift+F6 stops"),
                color::ALERT,
            );
        }
        Err(e) => say(state, format!("could not start xctrace: {e}"), color::ALERT),
    }
}

fn stop(state: &mut State) {
    let Some(mut recording) = state.recording.take() else {
        return;
    };
    let elapsed = recording.started.elapsed();

    // `SIGINT`, not `kill`. `xctrace` finalizes the trace on an interrupt and
    // discards it on anything harsher, and a recording that cannot be opened is
    // indistinguishable from one that never happened.
    //
    // SAFETY: `kill` with a pid this process spawned and a signal number that
    // exists. A child that has already exited gives `ESRCH`, which is ignored.
    unsafe { libc::kill(recording.child.id() as i32, libc::SIGINT) };
    let status = recording.child.wait();

    // The map, from the run that was just recorded, beside the trace it belongs
    // to. Written after the recording stops so that every region translated
    // during it is in the table.
    let map = format!("{}.map", recording.output.trim_end_matches(".trace"));
    let map_note = match crate::profile::write_map(&map) {
        Ok(()) => format!(", map at {map}"),
        Err(e) => format!(", but the map could not be written: {e}"),
    };

    match status {
        Ok(status) if status.success() => say(
            state,
            format!(
                "recorded {} to {}{map_note}",
                duration(elapsed),
                recording.output,
            ),
            color::GUEST,
        ),
        // A non-zero exit is where the two well-known refusals land: no
        // `get-task-allow` on the driver, and Processor Trace not enabled in
        // System Settings. `xctrace`'s own message went to the terminal, which
        // says which — the panel cannot, so it points at where it is.
        Ok(status) => say(
            state,
            format!("xctrace exited {status}: see its message above, and docs/PROFILING.md"),
            color::ALERT,
        ),
        Err(e) => say(
            state,
            format!("xctrace could not be waited for: {e}"),
            color::ALERT,
        ),
    }
}

/// Where this recording goes.
///
/// The working directory, named for the guest and the process, so that two runs
/// and two recordings inside one run never collide. `ROSETTA3_GUI_TRACE_DIR`
/// moves it, for the ordinary case of a game whose working directory is inside
/// its own read-only bundle.
fn output_path() -> String {
    let program = crate::thunk::current()
        .and_then(|vm| vm.program.report.images.first())
        .map(|image| image.name.rsplit('/').next().unwrap_or(&image.name))
        .unwrap_or("guest")
        .to_string();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let directory = std::env::var("ROSETTA3_GUI_TRACE_DIR").unwrap_or_else(|_| ".".into());
    format!("{directory}/{program}.{}.{stamp}.trace", std::process::id())
}

/// A working `xctrace`, found by running one.
///
/// Presence is not the test. On a machine with the Command Line Tools selected
/// `/usr/bin/xctrace` exists, is executable, and fails with "requires Xcode" —
/// the same trap `tools/guest-trace.sh` documents, answered the same way.
fn xctrace() -> Option<&'static str> {
    static FOUND: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        [
            "xctrace",
            "/Applications/Xcode.app/Contents/Developer/usr/bin/xctrace",
        ]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .arg("version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    })
}

// -------------------------------------------------------------- the panel ---

/// The panel's trace section.
pub fn describe(ui: &mut Ui) {
    ui.heading("INSTRUMENTS");
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.as_ref();
    match state.and_then(|s| s.recording.as_ref()) {
        Some(recording) => {
            ui.row(
                "recording",
                &format!(
                    "{} into {}",
                    duration(recording.started.elapsed()),
                    recording.output
                ),
                color::ALERT,
            );
            ui.line(
                "shift+F6 stops it and writes the map beside it",
                color::MUTED,
            );
        }
        None => {
            ui.row(
                "shift+F6",
                "attach xctrace and record cycles, branch misses, cache misses",
                color::VALUE,
            );
            ui.line(
                "needs the full Xcode, and the driver signed with get-task-allow",
                color::MUTED,
            );
        }
    }
    if let Some((at, message, colour)) = state.and_then(|s| s.said.as_ref())
        && at.elapsed() < TOAST
    {
        ui.line(message, *colour);
    }
}
