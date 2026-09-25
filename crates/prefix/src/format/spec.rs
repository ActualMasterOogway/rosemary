use std::collections::BTreeMap;

use super::manifest::{hex64, keys, mapping, non_negative, string, string_map};
use super::yaml::{self, Map, Value};
use super::{FORMAT, Invalid};

pub const HEADER: &str = "# minidarwin rootfs spec. Format: docs/rootfs-spec.md\n";
pub const ARTIFACT_FORMAT: &str = "tar+gzip";

const TOP_LEVEL: [&str; 6] = [
    "format",
    "identity",
    "release",
    "artifact",
    "manifest",
    "verification",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Both hashes are required and every check must pass.
    Strict,
    /// A hash may be null. One that is present is still checked; one that is
    /// missing is reported as not checked, loudly and every time.
    Insecure,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Policy::Strict => "strict",
            Policy::Insecure => "insecure",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Release {
    /// Only ever increases between releases: the rollback-protection counter.
    pub sequence: Option<u64>,
    pub tag: Option<String>,
    pub commit: Option<String>,
    pub repository: Option<String>,
}

/// The artifact or the manifest, as the spec pins it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pin {
    /// A hint only: the file is identified by its hash.
    pub name: Option<String>,
    pub size: Option<u64>,
    /// `None` only under [`Policy::Insecure`].
    pub sha256: Option<String>,
    /// Places to fetch from. They identify nothing; any may be a mirror.
    pub sources: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub identity: Option<BTreeMap<String, String>>,
    pub release: Release,
    pub artifact: Pin,
    pub manifest: Pin,
    /// The manifest's tree digest, when the spec gives it.
    pub tree: Option<String>,
    pub policy: Policy,
    /// Top-level keys the format does not define. They belong to the consumer:
    /// this is where `system_files` lives.
    pub extra: Map,
}

impl Spec {
    pub fn parse(raw: &[u8], what: &str) -> Result<Spec, Invalid> {
        if raw.len() as u64 > super::SPEC_LIMIT {
            return Err(Invalid::new(format!(
                "{what}: larger than {} bytes",
                super::SPEC_LIMIT
            )));
        }
        let text =
            std::str::from_utf8(raw).map_err(|_| Invalid::new(format!("{what}: not UTF-8")))?;
        let Value::Map(data) = yaml::parse(text, what)? else {
            return Err(Invalid::new(format!("{what}: expected a mapping")));
        };
        for key in ["format", "artifact", "manifest"] {
            if !data.contains(key) {
                return Err(Invalid::new(format!("{what}: missing '{key}'")));
            }
        }
        if data.get("format") != Some(&Value::Int(FORMAT)) {
            return Err(Invalid::new(format!("{what}: format is not {FORMAT}")));
        }

        let empty = Value::Map(Map::default());
        let or_empty = |key: &str| match data.get(key) {
            None | Some(Value::Null) => &empty,
            Some(value) => value,
        };

        let verification = mapping(
            Some(or_empty("verification")),
            &format!("{what}.verification"),
        )?;
        keys(
            verification,
            &["policy"],
            &[],
            &format!("{what}.verification"),
        )?;
        let policy = match verification.get("policy") {
            None => Policy::Strict,
            Some(Value::Str(p)) if p == "strict" => Policy::Strict,
            Some(Value::Str(p)) if p == "insecure" => Policy::Insecure,
            _ => {
                return Err(Invalid::new(format!(
                    "{what}.verification.policy: must be one of strict, insecure"
                )));
            }
        };
        let strict = policy == Policy::Strict;

        let identity = match data.get("identity") {
            None | Some(Value::Null) => None,
            value => Some(string_map(value, &format!("{what}.identity"))?),
        };

        let release_map = mapping(Some(or_empty("release")), &format!("{what}.release"))?;
        keys(
            release_map,
            &["sequence", "tag", "commit", "repository"],
            &[],
            &format!("{what}.release"),
        )?;
        let release = Release {
            sequence: optional(release_map.get("sequence"), |v| {
                non_negative(v, &format!("{what}.release.sequence"))
            })?,
            tag: optional(release_map.get("tag"), |v| {
                string(v, &format!("{what}.release.tag"))
            })?,
            commit: optional(release_map.get("commit"), |v| {
                string(v, &format!("{what}.release.commit"))
            })?,
            repository: optional(release_map.get("repository"), |v| {
                string(v, &format!("{what}.release.repository"))
            })?,
        };
        if let Some(commit) = &release.commit
            && !(commit.len() == 40
                && commit
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        {
            return Err(Invalid::new(format!(
                "{what}.release.commit: expected a full 40-digit commit hash"
            )));
        }

        let a = mapping(data.get("artifact"), &format!("{what}.artifact"))?;
        keys(
            a,
            &["name", "format", "size", "sha256", "sources"],
            &["format"],
            &format!("{what}.artifact"),
        )?;
        if a.get("format") != Some(&Value::Str(ARTIFACT_FORMAT.into())) {
            return Err(Invalid::new(format!(
                "{what}.artifact.format: only '{ARTIFACT_FORMAT}' is defined"
            )));
        }
        let artifact = pin(a, strict, &format!("{what}.artifact"))?;

        let m = mapping(data.get("manifest"), &format!("{what}.manifest"))?;
        keys(
            m,
            &["name", "size", "sha256", "tree", "sources"],
            &[],
            &format!("{what}.manifest"),
        )?;
        let manifest = pin(m, strict, &format!("{what}.manifest"))?;
        let tree = optional(m.get("tree"), |v| {
            hex64(v, &format!("{what}.manifest.tree"))
        })?;

        let extra = Map(data
            .0
            .iter()
            .filter(|(key, _)| !TOP_LEVEL.contains(&key.as_str()))
            .cloned()
            .collect());
        Ok(Spec {
            identity,
            release,
            artifact,
            manifest,
            tree,
            policy,
            extra,
        })
    }

    /// `identity.arch`, if the spec names one.
    pub fn arch(&self) -> Option<&str> {
        self.identity.as_ref()?.get("arch").map(String::as_str)
    }

    /// One line saying which release this is.
    pub fn describe(&self) -> String {
        let project = self
            .identity
            .as_ref()
            .and_then(|i| i.get("project"))
            .map(String::as_str)
            .unwrap_or("rootfs");
        let arch = self.arch().unwrap_or("?");
        let r = &self.release;
        match (&r.tag, r.sequence) {
            (Some(tag), sequence) => format!(
                "{project} {arch}, release {} ({tag}, {}) from {}",
                sequence.map_or("?".into(), |s| s.to_string()),
                r.commit.as_deref().map_or("?", |c| &c[..12.min(c.len())]),
                r.repository.as_deref().unwrap_or("?")
            ),
            (None, Some(sequence)) => format!("{project} {arch}, release {sequence}, unpublished"),
            (None, None) => format!("{project} {arch}, unreleased"),
        }
    }
}

fn optional<T>(
    value: Option<&Value>,
    read: impl FnOnce(Option<&Value>) -> Result<T, Invalid>,
) -> Result<Option<T>, Invalid> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => read(value).map(Some),
    }
}

fn pin(map: &Map, strict: bool, what: &str) -> Result<Pin, Invalid> {
    let sha256 = match map.get("sha256") {
        None | Some(Value::Null) if !strict => None,
        value => Some(hex64(value, &format!("{what}.sha256"))?),
    };
    let sources = match map.get("sources") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::List(items)) => {
            let mut sources = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let url = string(Some(item), &format!("{what}.sources[{index}]"))?;
                if !(url.starts_with("https://") || url.starts_with("file://")) {
                    return Err(Invalid::new(format!(
                        "{what}.sources[{index}]: '{url}' is not an https:// or file:// URL"
                    )));
                }
                sources.push(url);
            }
            sources
        }
        _ => return Err(Invalid::new(format!("{what}.sources: expected a list"))),
    };
    Ok(Pin {
        name: optional(map.get("name"), |v| string(v, &format!("{what}.name")))?,
        size: optional(map.get("size"), |v| {
            non_negative(v, &format!("{what}.size"))
        })?,
        sha256,
        sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_specs_parse() {
        for (arch, text) in crate::pinned::ALL {
            let spec = Spec::parse(text.as_bytes(), arch).expect("a pinned spec is valid");
            assert_eq!(spec.arch(), Some(*arch));
            assert_eq!(spec.policy, Policy::Strict);
            assert!(spec.release.sequence.is_some());
            assert!(spec.extra.0.is_empty());
        }
    }

    fn minimal(extra: &str) -> String {
        format!(
            "format: 1\nartifact:\n  format: \"tar+gzip\"\n  sha256: \"{h}\"\n\
             manifest:\n  sha256: \"{h}\"\n{extra}",
            h = "a".repeat(64)
        )
    }

    #[test]
    fn unknown_top_level_keys_belong_to_the_consumer() {
        let spec = Spec::parse(
            minimal("system_files:\n  policy: \"repair\"\n").as_bytes(),
            "t",
        )
        .expect("valid");
        assert!(spec.extra.contains("system_files"));
    }

    /// A verifier that skipped a field it did not understand could skip a
    /// check, so the sections the format owns are closed.
    #[test]
    fn unknown_keys_inside_a_section_are_errors() {
        let text = minimal("").replace(
            "  format: \"tar+gzip\"\n",
            "  format: \"tar+gzip\"\n  signature: \"x\"\n",
        );
        assert!(Spec::parse(text.as_bytes(), "t").is_err());
        let text = minimal("verification:\n  policy: \"strict\"\n  skip: true\n");
        assert!(Spec::parse(text.as_bytes(), "t").is_err());
    }

    #[test]
    fn a_strict_spec_needs_both_hashes() {
        let text = minimal("").replacen(&format!("\"{}\"", "a".repeat(64)), "null", 1);
        assert!(Spec::parse(text.as_bytes(), "t").is_err());
        let insecure = format!("{text}verification:\n  policy: \"insecure\"\n");
        let spec = Spec::parse(insecure.as_bytes(), "t").expect("insecure allows null");
        assert_eq!(spec.artifact.sha256, None);
        assert_eq!(spec.policy, Policy::Insecure);
    }

    #[test]
    fn sources_must_be_https_or_file() {
        let text = minimal("").replace(
            "  format: \"tar+gzip\"\n",
            "  format: \"tar+gzip\"\n  sources:\n    - \"http://example.com/x\"\n",
        );
        assert!(Spec::parse(text.as_bytes(), "t").is_err());
    }
}
