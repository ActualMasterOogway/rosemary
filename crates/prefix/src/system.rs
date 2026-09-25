use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::format::yaml::Value;
use crate::format::{Invalid, Spec};
use crate::layout::Prefix;

/// The top-level spec key this runtime owns.
pub const KEY: &str = "system_files";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SystemFiles {
    /// Allow the write, mark it, and put the file back at the next start.
    #[default]
    Repair,
    /// Allow the write, mark it, and say so at every start until repaired.
    Warn,
    /// Refuse the write with `EROFS`. The only policy under which the base is
    /// hardlinked from the cache rather than copied.
    Immutable,
}

impl SystemFiles {
    pub fn name(self) -> &'static str {
        match self {
            SystemFiles::Repair => "repair",
            SystemFiles::Warn => "warn",
            SystemFiles::Immutable => "immutable",
        }
    }

    pub fn parse(name: &str) -> Option<SystemFiles> {
        match name {
            "repair" => Some(SystemFiles::Repair),
            "warn" => Some(SystemFiles::Warn),
            "immutable" => Some(SystemFiles::Immutable),
            _ => None,
        }
    }

    /// The policy a spec's `system_files: {policy: ...}` asks for, defaulting
    /// to [`SystemFiles::Repair`].
    pub fn of(spec: &Spec) -> Result<SystemFiles, Invalid> {
        match spec.extra.get(KEY) {
            None | Some(Value::Null) => Ok(SystemFiles::default()),
            Some(Value::Map(map)) => {
                if let Some(unknown) = map.keys().find(|key| *key != "policy") {
                    return Err(Invalid::new(format!("{KEY}: unknown key '{unknown}'")));
                }
                match map.get("policy") {
                    None => Ok(SystemFiles::default()),
                    Some(Value::Str(name)) => SystemFiles::parse(name).ok_or_else(|| {
                        Invalid::new(format!(
                            "{KEY}.policy: '{name}' is not repair, warn or immutable"
                        ))
                    }),
                    Some(_) => Err(Invalid::new(format!("{KEY}.policy: expected a string"))),
                }
            }
            Some(_) => Err(Invalid::new(format!("{KEY}: expected a mapping"))),
        }
    }
}

/// Top-level spec keys this runtime does not understand, which the format asks
/// a consumer to mention.
pub fn ignored_keys(spec: &Spec) -> Vec<String> {
    spec.extra
        .keys()
        .filter(|key| *key != KEY)
        .map(String::from)
        .collect()
}

/// `text` with its `system_files` section, if any, replaced by `policy`.
///
/// Line-based rather than parse-and-rerender, so that the rest of a stamped
/// spec stays byte-for-byte what was released.
pub fn with_policy(text: &str, policy: SystemFiles) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let top_level = !line.starts_with([' ', '\t', '#', '\n', '\r']);
        if top_level {
            skipping = line.starts_with(KEY) && line[KEY.len()..].starts_with(':');
        }
        if !skipping {
            out.push_str(line);
        }
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{KEY}:\n  policy: \"{}\"\n", policy.name()));
    out
}

pub fn dirty_path(prefix: &Prefix) -> PathBuf {
    prefix.control().join("dirty")
}

/// Append `path` to the dirty list.
pub fn mark(dirty: &Path, path: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dirty)?;
    // One `write` per line, so two processes appending at once interleave
    // whole lines.
    file.write_all(format!("{path}\n").as_bytes())
}

/// The paths in the dirty list, or none if there is no list.
pub fn dirty(prefix: &Prefix) -> HashSet<String> {
    std::fs::read_to_string(dirty_path(prefix))
        .map(|text| {
            text.lines()
                .filter(|line| line.starts_with('/'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn clear_dirty(prefix: &Prefix) -> std::io::Result<()> {
    match std::fs::remove_file(dirty_path(prefix)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_is_a_consumer_key_in_the_spec() {
        let text = crate::pinned::text("x86_64").unwrap();
        let spec = Spec::parse(text.as_bytes(), "t").unwrap();
        assert_eq!(SystemFiles::of(&spec).unwrap(), SystemFiles::Repair);

        let written = with_policy(text, SystemFiles::Immutable);
        assert!(
            written.starts_with(text),
            "the released spec is kept verbatim"
        );
        let spec = Spec::parse(written.as_bytes(), "t").unwrap();
        assert_eq!(SystemFiles::of(&spec).unwrap(), SystemFiles::Immutable);
        assert!(ignored_keys(&spec).is_empty());

        // Rewriting replaces rather than appends a second section.
        let again = with_policy(&written, SystemFiles::Warn);
        assert_eq!(again.matches("system_files:").count(), 1);
        let spec = Spec::parse(again.as_bytes(), "t").unwrap();
        assert_eq!(SystemFiles::of(&spec).unwrap(), SystemFiles::Warn);
    }

    #[test]
    fn an_unknown_policy_is_an_error() {
        let text = format!(
            "{}system_files:\n  policy: \"sometimes\"\n",
            crate::pinned::text("x86_64").unwrap()
        );
        let spec = Spec::parse(text.as_bytes(), "t").unwrap();
        assert!(SystemFiles::of(&spec).is_err());
    }
}
