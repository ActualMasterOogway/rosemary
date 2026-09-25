use super::*;

/// The process's Vulkan tracing switch, shared with `vulkan.rs`.
#[inline(always)]
fn tracing() -> bool {
    #[dynamic]
    static ENABLED: bool = std::env::var_os("ROSETTA3_TRACE_METAL").is_some();

    *ENABLED
}

/// Write one module to `ROSETTA3_SPIRV_OUT`, so that a driver which rejects or
/// crashes on a shader can be met with the exact bytes it was given.
///
/// A driver's SPIR-V front end is not a validator, and a module `spirv-val`
/// accepts can still take one down; the file is the only way to tell which of
/// the thousands a game compiles was the one.
pub(super) fn dump_spirv(entry: &str, words: &[u32]) {
    let Some(directory) = std::env::var_os("ROSETTA3_SPIRV_OUT") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    // An entry point name is a Metal identifier, but nothing guarantees that of
    // a name a translated library carries, and this becomes a path.
    let safe: String = entry
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() || c == '_' {
            true => c,
            false => '_',
        })
        .collect();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    let mut path = directory.join(&safe);
    // Two libraries may carry the same entry point name with different bodies.
    let mut ordinal = 0;
    while path.exists() {
        ordinal += 1;
        path = directory.join(format!("{safe}.{ordinal}"));
    }
    path.set_extension("spv");
    let _ = std::fs::write(path, bytes);
}

pub fn trace(message: impl std::fmt::Display) {
    if tracing() {
        eprintln!("rosetta3: Metal: {message}");
    }
}

/// Report a message once however often the runtime reaches it, so that a gap a
/// frame runs into does not fill the log.
pub fn trace_once(message: impl std::fmt::Display) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if !tracing() {
        return;
    }
    let message = message.to_string();
    let mut seen = SEEN.lock().expect("trace list");
    if !seen.contains(&message) {
        eprintln!("rosetta3: Metal: {message}");
        seen.push(message);
    }
}

/// Report one milestone the first time the runtime reaches it.
///
/// A runtime which never draws and a runtime which draws nothing visible look
/// the same in a game's own log, so each first dispatch, draw and present says
/// so once.
pub fn milestone(what: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    if !tracing() {
        return;
    }
    let mut seen = SEEN.lock().expect("milestone list");
    if !seen.contains(&what) {
        seen.push(what);
        eprintln!("rosetta3: Metal: first {what}");
    }
}
