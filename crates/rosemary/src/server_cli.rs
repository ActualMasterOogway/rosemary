//! `rosemary server`, and joining a guest to its prefix's server.
//!
//! The server is this same binary run as `rosemary server`. A guest's first
//! question starts one if none is listening (see `server::client`), so nobody
//! has to start it by hand; the subcommand exists for that launch, and for a
//! person who wants to run one in the foreground or stop one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Whether guests use a process server, from `ROSEMARY_SERVER`.
fn enabled() -> bool {
    !matches!(
        std::env::var("ROSEMARY_SERVER").as_deref(),
        Ok("off" | "0" | "false")
    )
}

/// The server directory for the prefix this process runs in.
fn directory() -> PathBuf {
    let prefix = prefix::ns::current().map(|prefix| prefix.directory());
    server::location::directory_for(prefix)
}

/// Start a detached server for `directory`.
///
/// The child daemonizes itself and its first half exits at once, so waiting
/// for it here costs nothing and leaves no zombie behind in a guest that
/// never reaps children it did not make.
fn launch(directory: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new(exe)
        .arg("server")
        .arg("--directory")
        .arg(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(std::io::Error::other(format!("server launch {status}"))),
    }
}

/// Point this process at its prefix's server and check in as the guest at
/// `guest`, before the program's first instruction.
pub fn join(guest: &str, argv: &[OsString]) {
    if !enabled() {
        return;
    }
    server::client::configure(server::client::Config {
        directory: directory(),
        launch: Some(launch),
    });
    let argv = argv
        .iter()
        .map(|arg| arg.as_encoded_bytes().to_vec())
        .collect();
    darwin::process::check_in(guest, argv);
}

/// How long a server outlives its last guest, from `ROSEMARY_SERVER_LINGER`.
fn linger() -> Duration {
    std::env::var("ROSEMARY_SERVER_LINGER")
        .ok()
        .and_then(|seconds| seconds.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map_or(server::daemon::DEFAULT_LINGER, Duration::from_secs_f64)
}

/// Leave the caller's session and process group, as a daemon does: a server
/// started from a terminal must not die with it or take its signals.
///
/// Returns in the daemon only; the first half exits here.
fn detach() {
    // SAFETY: `setsid` and `fork` have no memory-safety preconditions; nothing
    // in this process has started a thread yet.
    unsafe {
        libc::setsid();
        match libc::fork() {
            0 => {}
            -1 => {}
            _ => libc::_exit(0),
        }
    }
    let _ = std::env::set_current_dir("/");
}

/// `rosemary server [--directory DIR] [--foreground] [--stop]`.
pub fn command(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut directory_arg = None;
    let mut foreground = false;
    let mut stop = false;
    while let Some(arg) = args.next() {
        match arg.to_str().unwrap_or_default() {
            "--directory" => directory_arg = args.next().map(PathBuf::from),
            "--foreground" => foreground = true,
            "--stop" => stop = true,
            other => {
                eprintln!("rosemary server: unknown argument {other:?}");
                eprintln!("usage: rosemary server [--directory DIR] [--foreground] [--stop]");
                return ExitCode::FAILURE;
            }
        }
    }
    let directory = directory_arg.unwrap_or_else(|| {
        crate::prefix_cli::settle_prefix();
        directory()
    });
    if stop {
        return match server::daemon::stop(&server::location::socket_in(&directory)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("rosemary server: no server at {}: {e}", directory.display());
                ExitCode::FAILURE
            }
        };
    }
    if !foreground {
        detach();
    }
    let options = server::daemon::Options {
        directory,
        linger: linger(),
    };
    match server::daemon::run(&options) {
        Ok(server::daemon::Outcome::Finished) => ExitCode::SUCCESS,
        // Somebody else won the race to start one, which is what was wanted.
        Ok(server::daemon::Outcome::AlreadyRunning) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rosemary server: {e}");
            ExitCode::FAILURE
        }
    }
}
