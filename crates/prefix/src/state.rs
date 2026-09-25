use std::path::Path;

use crate::format::yaml::{self, Map, Value};
use crate::format::{Invalid, spec};

/// Which spec a prefix's base was verified against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// The one compiled into this build.
    Pinned,
    /// One a user handed over. Integrity, not authenticity, and exempt from
    /// rollback protection because the user chose it.
    User,
}

impl Trust {
    pub fn name(self) -> &'static str {
        match self {
            Trust::Pinned => "pinned",
            Trust::User => "user",
        }
    }
}

/// The base a prefix was provisioned from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub project: String,
    pub arch: String,
    pub sequence: Option<u64>,
    pub tag: Option<String>,
    /// The tree digest, which is also the cache key of the unpacked tree.
    pub tree: String,
    pub manifest_sha256: String,
    /// `None` when the base came from the cache without the artifact being
    /// needed, and the spec did not pin one either.
    pub artifact_sha256: Option<String>,
    pub trust: Trust,
    pub policy: spec::Policy,
    /// How the system files were put in: `reflink`, `hardlink`, `copy`.
    pub materialised: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub runtime_version: Option<String>,
    pub base: Option<Base>,
    /// The highest pinned `release.sequence` this prefix has accepted: an
    /// automatic update to a lower one is a rollback, and refused.
    pub accepted_sequence: Option<u64>,
}

impl State {
    pub fn read(path: &Path) -> Result<State, Invalid> {
        let raw =
            std::fs::read(path).map_err(|e| Invalid::new(format!("{}: {e}", path.display())))?;
        let text = std::str::from_utf8(&raw)
            .map_err(|_| Invalid::new(format!("{}: not UTF-8", path.display())))?;
        let Value::Map(map) = yaml::parse(text, "state.yaml")? else {
            return Err(Invalid::new("state.yaml: expected a mapping"));
        };
        let runtime_version = match map.get("created_with") {
            Some(Value::Map(created)) => text_of(created.get("runtime_version")),
            _ => None,
        };
        let base = match map.get("base") {
            Some(Value::Map(base)) => Some(read_base(base)?),
            _ => None,
        };
        Ok(State {
            runtime_version,
            base,
            accepted_sequence: number(map.get("accepted_sequence")),
        })
    }

    pub fn render(&self) -> String {
        let q = |s: &str| yaml::quote(s);
        let mut out = vec![
            "# Written by `rosetta3 prefix`. See docs/prefixes.md.".to_string(),
            "version: 1".to_string(),
            "created_with:".to_string(),
            format!(
                "  runtime_version: {}",
                q(self
                    .runtime_version
                    .as_deref()
                    .unwrap_or(env!("CARGO_PKG_VERSION")))
            ),
        ];
        if let Some(base) = &self.base {
            out.push("base:".into());
            out.push(format!("  project: {}", q(&base.project)));
            out.push(format!("  arch: {}", q(&base.arch)));
            if let Some(sequence) = base.sequence {
                out.push(format!("  sequence: {sequence}"));
            }
            if let Some(tag) = &base.tag {
                out.push(format!("  tag: {}", q(tag)));
            }
            out.push(format!("  tree: {}", q(&base.tree)));
            out.push(format!("  manifest_sha256: {}", q(&base.manifest_sha256)));
            if let Some(artifact) = &base.artifact_sha256 {
                out.push(format!("  artifact_sha256: {}", q(artifact)));
            }
            out.push(format!("  trust: {}", q(base.trust.name())));
            out.push(format!("  policy: {}", q(base.policy.name())));
            let methods: Vec<String> = base.materialised.iter().map(|m| q(m)).collect();
            out.push(format!("  materialised: [{}]", methods.join(", ")));
        }
        if let Some(sequence) = self.accepted_sequence {
            out.push(format!("accepted_sequence: {sequence}"));
        }
        out.push(String::new());
        out.join("\n")
    }

    /// Write atomically: a state file is what says a prefix exists at all.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let staged = path.with_extension("yaml.new");
        std::fs::write(&staged, self.render())?;
        std::fs::rename(staged, path)
    }
}

fn read_base(map: &Map) -> Result<Base, Invalid> {
    let need = |key: &str| {
        text_of(map.get(key))
            .ok_or_else(|| Invalid::new(format!("state.yaml: base.{key} is missing")))
    };
    Ok(Base {
        project: need("project")?,
        arch: need("arch")?,
        sequence: number(map.get("sequence")),
        tag: text_of(map.get("tag")),
        tree: need("tree")?,
        manifest_sha256: need("manifest_sha256")?,
        artifact_sha256: text_of(map.get("artifact_sha256")),
        trust: match text_of(map.get("trust")).as_deref() {
            Some("pinned") => Trust::Pinned,
            _ => Trust::User,
        },
        policy: match text_of(map.get("policy")).as_deref() {
            Some("strict") => spec::Policy::Strict,
            _ => spec::Policy::Insecure,
        },
        materialised: match map.get("materialised") {
            Some(Value::List(items)) => items.iter().filter_map(|v| text_of(Some(v))).collect(),
            _ => Vec::new(),
        },
    })
}

fn text_of(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Str(s)) => Some(s.clone()),
        Some(Value::Int(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn number(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Int(n)) if *n >= 0 => Some(*n as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_file_round_trips() {
        let state = State {
            runtime_version: Some("0.1.0".into()),
            base: Some(Base {
                project: "minidarwin".into(),
                arch: "x86_64".into(),
                sequence: Some(3),
                tag: Some("v3.0.0".into()),
                tree: "c".repeat(64),
                manifest_sha256: "d".repeat(64),
                artifact_sha256: None,
                trust: Trust::Pinned,
                policy: spec::Policy::Strict,
                materialised: vec!["reflink".into(), "copy".into()],
            }),
            accepted_sequence: Some(3),
        };
        let at = crate::verify::tests::scratch("state");
        let path = at.join("state.yaml");
        state.write(&path).unwrap();
        assert_eq!(State::read(&path).unwrap(), state);
        let _ = std::fs::remove_dir_all(&at);
    }

    /// What phase 1 wrote, with its bare version number, still reads.
    #[test]
    fn a_phase_one_state_file_reads() {
        let at = crate::verify::tests::scratch("state-old");
        let path = at.join("state.yaml");
        std::fs::write(
            &path,
            "# Written by `rosetta3 prefix create`. See docs/prefixes.md.\n\
             version: 1\ncreated_with:\n  runtime_version: 0.1.0\n",
        )
        .unwrap();
        let state = State::read(&path).unwrap();
        assert_eq!(state.runtime_version.as_deref(), Some("0.1.0"));
        assert_eq!(state.base, None);
        let _ = std::fs::remove_dir_all(&at);
    }
}
