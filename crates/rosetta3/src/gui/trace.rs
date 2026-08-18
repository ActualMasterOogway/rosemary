//! Driving Apple's system tracer from inside the process it is tracing.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use super::draw::{Color, Ui, color, duration};

/// The template recorded when nothing else is asked for.
///
/// `Time Profiler` rather than `Processor Trace`, and that is a finding rather than a
/// preference:
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
    // recording.
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
        // System Settings.
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
/// The working directory, named for the guest and the process, so that two runs and two
/// recordings inside one run never collide.
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
