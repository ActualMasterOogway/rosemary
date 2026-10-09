use std::path::PathBuf;

/// Set by the packaged launcher (`crates/launcher`) to its own path.
pub const VAR: &str = "ROSEMARY_EXE";

pub fn current() -> std::io::Result<PathBuf> {
    match std::env::var_os(VAR) {
        Some(path) if !path.is_empty() => Ok(path.into()),
        _ => std::env::current_exe(),
    }
}
