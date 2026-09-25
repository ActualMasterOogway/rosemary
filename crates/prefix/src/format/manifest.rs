use std::collections::{BTreeMap, HashMap};

use super::yaml::{self, Map, Value};
use super::{FORMAT, Invalid, check_symlink_target, is_hex64, parent_of, split_path};
use crate::digest;

pub const HEADER: &str = "# minidarwin rootfs manifest. Format: docs/rootfs-spec.md\n";

/// The one owner a manifest may name: every path it lists is MiniDarwin's.
pub const OWNER: &str = "minidarwin";

pub const FILE_MODES: [u32; 2] = [0o644, 0o755];
pub const DIR_MODE: u32 = 0o755;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File {
        mode: u32,
        size: u64,
        sha256: String,
    },
    Symlink {
        target: String,
    },
    Directory {
        mode: u32,
        digest: String,
    },
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::File { .. } => "file",
            Kind::Symlink { .. } => "symlink",
            Kind::Directory { .. } => "directory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleSource {
    pub name: String,
    pub rev: String,
    pub hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Build {
    pub llvm: Option<String>,
    pub apple_sources: Option<Vec<AppleSource>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: u64,
    pub symlinks: u64,
    pub directories: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub identity: BTreeMap<String, String>,
    pub build: Option<Build>,
    /// The tree digest.
    pub tree: String,
    pub summary: Summary,
    pub entries: Vec<Entry>,
    by_path: HashMap<String, usize>,
}

impl Manifest {
    /// A manifest for `entries`, in canonical order, with every directory
    /// digest, the tree digest and the summary computed - what `mdrootfs
    /// manifest` makes from a tree.
    pub fn new(
        identity: BTreeMap<String, String>,
        build: Option<Build>,
        mut entries: Vec<Entry>,
    ) -> Manifest {
        let (tree, digests) = digest::merkle(&entries);
        for entry in &mut entries {
            if let Kind::Directory { digest, .. } = &mut entry.kind {
                *digest = digests[&entry.path].clone();
            }
        }
        let by_path = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.path.clone(), index))
            .collect();
        Manifest {
            identity,
            build,
            tree,
            summary: summarise(&entries),
            entries,
            by_path,
        }
    }

    /// Parse and fully check a manifest's bytes.
    pub fn parse(raw: &[u8], what: &str) -> Result<Manifest, Invalid> {
        if raw.len() as u64 > super::MANIFEST_LIMIT {
            return Err(Invalid::new(format!(
                "{what}: larger than {} bytes",
                super::MANIFEST_LIMIT
            )));
        }
        let text = std::str::from_utf8(raw)
            .ok()
            .filter(|text| text.is_ascii())
            .ok_or_else(|| Invalid::new(format!("{what}: not ASCII")))?;
        let Value::Map(data) = yaml::parse(text, what)? else {
            return Err(Invalid::new(format!("{what}: expected a mapping")));
        };
        keys(
            &data,
            &[
                "format", "owner", "identity", "build", "tree", "summary", "entries",
            ],
            &["format", "owner", "identity", "tree", "summary", "entries"],
            what,
        )?;
        if data.get("format") != Some(&Value::Int(FORMAT)) {
            return Err(Invalid::new(format!("{what}: format is not {FORMAT}")));
        }
        if data.get("owner") != Some(&Value::Str(OWNER.into())) {
            return Err(Invalid::new(format!("{what}: owner must be '{OWNER}'")));
        }
        let identity = string_map(data.get("identity"), &format!("{what}.identity"))?;
        for key in ["project", "arch"] {
            if !identity.contains_key(key) {
                return Err(Invalid::new(format!("{what}.identity: missing '{key}'")));
            }
        }
        let build = match data.get("build") {
            Some(value) => Some(build_section(value, &format!("{what}.build"))?),
            None => None,
        };
        let tree = mapping(data.get("tree"), &format!("{what}.tree"))?;
        keys(
            tree,
            &["algorithm", "digest"],
            &["algorithm", "digest"],
            &format!("{what}.tree"),
        )?;
        if tree.get("algorithm") != Some(&Value::Str(digest::ALGORITHM.into())) {
            return Err(Invalid::new(format!(
                "{what}.tree: algorithm is not {}",
                digest::ALGORITHM
            )));
        }
        let tree_digest = hex64(tree.get("digest"), &format!("{what}.tree.digest"))?;
        let Some(Value::List(raw_entries)) = data.get("entries") else {
            return Err(Invalid::new(format!("{what}.entries: expected a list")));
        };
        let entries = check_entries(raw_entries, what)?;

        let (computed, digests) = digest::merkle(&entries);
        if computed != tree_digest {
            return Err(Invalid::new(format!(
                "{what}: tree digest does not match its entries"
            )));
        }
        for entry in &entries {
            if let Kind::Directory { digest, .. } = &entry.kind
                && digests.get(&entry.path) != Some(digest)
            {
                return Err(Invalid::new(format!(
                    "{what}: digest of directory '{}' does not match its contents",
                    entry.path
                )));
            }
        }
        let summary = summarise(&entries);
        let written = mapping(data.get("summary"), &format!("{what}.summary"))?;
        let expected = Map(vec![
            ("files".into(), Value::Int(summary.files as i64)),
            ("symlinks".into(), Value::Int(summary.symlinks as i64)),
            ("directories".into(), Value::Int(summary.directories as i64)),
            ("bytes".into(), Value::Int(summary.bytes as i64)),
        ]);
        if !same_mapping(written, &expected) {
            return Err(Invalid::new(format!(
                "{what}: summary does not match its entries"
            )));
        }
        let by_path = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.path.clone(), index))
            .collect();
        let manifest = Manifest {
            identity,
            build,
            tree: tree_digest,
            summary,
            entries,
            by_path,
        };
        // Its SHA-256 is only an identity if there is exactly one way to write
        // it down.
        if manifest.render() != text {
            return Err(Invalid::new(format!(
                "{what}: not in canonical form (regenerate it with `mdrootfs manifest`)"
            )));
        }
        Ok(manifest)
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.by_path.get(path).map(|index| &self.entries[*index])
    }

    pub fn contains(&self, path: &str) -> bool {
        self.by_path.contains_key(path)
    }

    pub fn arch(&self) -> &str {
        self.identity.get("arch").map(String::as_str).unwrap_or("")
    }

    /// The canonical text, exactly as `mdrootfs manifest` writes it.
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        out.push_str(&format!("format: {FORMAT}\n"));
        out.push_str(&format!("owner: {}\n", yaml::quote(OWNER)));
        out.push_str("identity:\n");
        for (key, value) in &self.identity {
            out.push_str(&format!("  {key}: {}\n", yaml::quote(value)));
        }
        if let Some(build) = &self.build {
            if build.llvm.is_none() && build.apple_sources.is_none() {
                out.push_str("build: {}\n");
            } else {
                out.push_str("build:\n");
                if let Some(llvm) = &build.llvm {
                    out.push_str(&format!("  llvm: {}\n", yaml::quote(llvm)));
                }
                match &build.apple_sources {
                    Some(sources) if sources.is_empty() => out.push_str("  apple_sources: []\n"),
                    Some(sources) => {
                        out.push_str("  apple_sources:\n");
                        for s in sources {
                            out.push_str(&format!(
                                "    - {{name: {}, rev: {}, hash: {}}}\n",
                                yaml::quote(&s.name),
                                yaml::quote(&s.rev),
                                yaml::quote(&s.hash)
                            ));
                        }
                    }
                    None => {}
                }
            }
        }
        out.push_str("tree:\n");
        out.push_str(&format!(
            "  algorithm: {}\n",
            yaml::quote(digest::ALGORITHM)
        ));
        out.push_str(&format!("  digest: {}\n", yaml::quote(&self.tree)));
        let s = self.summary;
        out.push_str(&format!(
            "summary:\n  files: {}\n  symlinks: {}\n  directories: {}\n  bytes: {}\n",
            s.files, s.symlinks, s.directories, s.bytes
        ));
        if self.entries.is_empty() {
            out.push_str("entries: []\n");
            return out;
        }
        out.push_str("entries:\n");
        for entry in &self.entries {
            let path = yaml::quote(&entry.path);
            match &entry.kind {
                Kind::File { mode, size, sha256 } => out.push_str(&format!(
                    "  - {{path: {path}, type: \"file\", mode: \"{mode:04o}\", size: {size}, sha256: {}}}\n",
                    yaml::quote(sha256)
                )),
                Kind::Symlink { target } => out.push_str(&format!(
                    "  - {{path: {path}, type: \"symlink\", target: {}}}\n",
                    yaml::quote(target)
                )),
                Kind::Directory { mode, digest } => out.push_str(&format!(
                    "  - {{path: {path}, type: \"directory\", mode: \"{mode:04o}\", digest: {}}}\n",
                    yaml::quote(digest)
                )),
            }
        }
        out
    }
}

pub fn summarise(entries: &[Entry]) -> Summary {
    let mut summary = Summary::default();
    for entry in entries {
        match &entry.kind {
            Kind::File { size, .. } => {
                summary.files += 1;
                summary.bytes += size;
            }
            Kind::Symlink { .. } => summary.symlinks += 1,
            Kind::Directory { .. } => summary.directories += 1,
        }
    }
    summary
}

/// Order, uniqueness, parents and per-type fields. Digests are checked by the
/// caller, over the whole list.
fn check_entries(raw: &[Value], what: &str) -> Result<Vec<Entry>, Invalid> {
    let mut directories: std::collections::HashSet<String> = ["/".to_string()].into();
    let mut siblings: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut previous: Option<Vec<String>> = None;
    let mut entries = Vec::with_capacity(raw.len());
    for (index, value) in raw.iter().enumerate() {
        let where_ = format!("{what}: entries[{index}]");
        let Value::Map(entry) = value else {
            return Err(Invalid::new(format!("{where_}: expected a mapping")));
        };
        let kind = match entry.get("type") {
            Some(Value::Str(kind)) => kind.as_str(),
            _ => "",
        };
        let expected: &[&str] = match kind {
            "file" => &["path", "type", "mode", "size", "sha256"],
            "symlink" => &["path", "type", "target"],
            "directory" => &["path", "type", "mode", "digest"],
            other => {
                return Err(Invalid::new(format!("{where_}: unknown type '{other}'")));
            }
        };
        if !entry.keys().eq(expected.iter().copied()) {
            return Err(Invalid::new(format!(
                "{where_}: a {kind} has exactly the keys {}, in that order",
                expected.join(", ")
            )));
        }
        let Some(Value::Str(path)) = entry.get("path") else {
            return Err(Invalid::new(format!("{where_}: path is not a string")));
        };
        let parts: Vec<String> = split_path(path, &where_)?
            .into_iter()
            .map(String::from)
            .collect();
        // Canonical order is the paths sorted by their list of components,
        // compared bytewise.
        if let Some(previous) = &previous
            && parts
                .iter()
                .map(|p| p.as_bytes())
                .cmp(previous.iter().map(|p| p.as_bytes()))
                != std::cmp::Ordering::Greater
        {
            return Err(Invalid::new(format!(
                "{where_}: '{path}' is out of order or repeated"
            )));
        }
        let parent = parent_of(path);
        if !directories.contains(parent) {
            return Err(Invalid::new(format!(
                "{where_}: parent of '{path}' is not a directory listed before it"
            )));
        }
        let folded = siblings.entry(parent.to_string()).or_default();
        let name = parts.last().expect("split_path refuses an empty path");
        if let Some(other) = folded.get(&name.to_ascii_lowercase()) {
            return Err(Invalid::new(format!(
                "{where_}: '{path}' and '{other}' differ only in case"
            )));
        }
        folded.insert(name.to_ascii_lowercase(), path.clone());
        let kind = match kind {
            "file" => {
                let mode = mode(entry.get("mode"), &where_)?;
                if !FILE_MODES.contains(&mode) {
                    return Err(Invalid::new(format!(
                        "{where_}: file mode must be one of 0644, 0755"
                    )));
                }
                Kind::File {
                    mode,
                    size: non_negative(entry.get("size"), &format!("{where_}.size"))?,
                    sha256: hex64(entry.get("sha256"), &format!("{where_}.sha256"))?,
                }
            }
            "directory" => {
                if mode(entry.get("mode"), &where_)? != DIR_MODE {
                    return Err(Invalid::new(format!(
                        "{where_}: directory mode must be 0755"
                    )));
                }
                directories.insert(path.clone());
                Kind::Directory {
                    mode: DIR_MODE,
                    digest: hex64(entry.get("digest"), &format!("{where_}.digest"))?,
                }
            }
            _ => {
                let Some(Value::Str(target)) = entry.get("target") else {
                    return Err(Invalid::new(format!(
                        "{where_}: symlink '{path}' has no target"
                    )));
                };
                check_symlink_target(path, target, &where_)?;
                Kind::Symlink {
                    target: target.clone(),
                }
            }
        };
        entries.push(Entry {
            path: path.clone(),
            kind,
        });
        previous = Some(parts);
    }
    Ok(entries)
}

/// A mode is the quoted string `"0644"`, `"0755"`. Parsed from the string,
/// never from a number: a bare `0755` is refused by the parser already.
fn mode(value: Option<&Value>, what: &str) -> Result<u32, Invalid> {
    let Some(Value::Str(text)) = value else {
        return Err(Invalid::new(format!("{what}: mode is not a quoted string")));
    };
    if text.len() != 4 || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err(Invalid::new(format!(
            "{what}: mode '{text}' is not four octal digits"
        )));
    }
    Ok(u32::from_str_radix(text, 8).expect("checked above"))
}

fn build_section(value: &Value, what: &str) -> Result<Build, Invalid> {
    let build = mapping(Some(value), what)?;
    keys(build, &["llvm", "apple_sources"], &[], what)?;
    let mut result = Build::default();
    if let Some(llvm) = build.get("llvm") {
        result.llvm = Some(string(Some(llvm), &format!("{what}.llvm"))?);
    }
    if let Some(sources) = build.get("apple_sources") {
        let Value::List(sources) = sources else {
            return Err(Invalid::new(format!(
                "{what}.apple_sources: expected a list"
            )));
        };
        let mut clean = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            let where_ = format!("{what}.apple_sources[{index}]");
            let source = mapping(Some(source), &where_)?;
            if !source.keys().eq(["name", "rev", "hash"]) {
                return Err(Invalid::new(format!(
                    "{where_}: expected exactly name, rev, hash"
                )));
            }
            clean.push(AppleSource {
                name: string(source.get("name"), &format!("{where_}.name"))?,
                rev: string(source.get("rev"), &format!("{where_}.rev"))?,
                hash: string(source.get("hash"), &format!("{where_}.hash"))?,
            });
        }
        let names: Vec<&str> = clean.iter().map(|s| s.name.as_str()).collect();
        let mut sorted: Vec<&str> = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if names != sorted {
            return Err(Invalid::new(format!(
                "{what}.apple_sources: names must be unique and sorted"
            )));
        }
        result.apple_sources = Some(clean);
    }
    Ok(result)
}

pub(crate) fn mapping<'a>(value: Option<&'a Value>, what: &str) -> Result<&'a Map, Invalid> {
    match value {
        Some(Value::Map(map)) => Ok(map),
        _ => Err(Invalid::new(format!("{what}: expected a mapping"))),
    }
}

/// Refuse a key outside `allowed`, and require every one of `required`.
pub(crate) fn keys(
    map: &Map,
    allowed: &[&str],
    required: &[&str],
    what: &str,
) -> Result<(), Invalid> {
    if let Some(unknown) = map.keys().find(|key| !allowed.contains(key)) {
        return Err(Invalid::new(format!("{what}: unknown key '{unknown}'")));
    }
    if let Some(missing) = required.iter().find(|key| !map.contains(key)) {
        return Err(Invalid::new(format!("{what}: missing '{missing}'")));
    }
    Ok(())
}

pub(crate) fn string(value: Option<&Value>, what: &str) -> Result<String, Invalid> {
    match value {
        Some(Value::Str(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(Invalid::new(format!("{what}: expected a non-empty string"))),
    }
}

pub(crate) fn non_negative(value: Option<&Value>, what: &str) -> Result<u64, Invalid> {
    match value {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        _ => Err(Invalid::new(format!(
            "{what}: expected a non-negative integer"
        ))),
    }
}

pub(crate) fn hex64(value: Option<&Value>, what: &str) -> Result<String, Invalid> {
    match value {
        Some(Value::Str(text)) if is_hex64(text) => Ok(text.clone()),
        _ => Err(Invalid::new(format!(
            "{what}: expected 64 lowercase hex digits"
        ))),
    }
}

pub(crate) fn string_map(
    value: Option<&Value>,
    what: &str,
) -> Result<BTreeMap<String, String>, Invalid> {
    let map = mapping(value, what)?;
    let mut out = BTreeMap::new();
    for (key, item) in &map.0 {
        out.insert(key.clone(), string(Some(item), &format!("{what}.{key}"))?);
    }
    Ok(out)
}

/// Python's dict equality: the same keys with the same values, in any order.
fn same_mapping(a: &Map, b: &Map) -> bool {
    a.0.len() == b.0.len() && a.0.iter().all(|(key, value)| b.get(key) == Some(value))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::format::sha256_hex;

    /// A small, valid manifest, built the way `mdrootfs manifest` would.
    pub(crate) fn sample() -> (Manifest, Vec<(&'static str, &'static [u8])>) {
        let files: Vec<(&str, &[u8])> = vec![
            ("/usr/lib/data.txt", b"data\n"),
            (
                "/usr/lib/libA.dylib",
                b"\xcf\xfa\xed\xfe not really a mach-o\n",
            ),
            ("/usr/share/doc/readme", b""),
        ];
        let mode = |path: &str| {
            if path.ends_with(".dylib") {
                0o755
            } else {
                0o644
            }
        };
        let file = |path: &str, content: &[u8]| Entry {
            path: path.into(),
            kind: Kind::File {
                mode: mode(path),
                size: content.len() as u64,
                sha256: sha256_hex(content),
            },
        };
        let directory = |path: &str| Entry {
            path: path.into(),
            kind: Kind::Directory {
                mode: DIR_MODE,
                digest: String::new(),
            },
        };
        let entries = vec![
            directory("/usr"),
            directory("/usr/lib"),
            file(files[0].0, files[0].1),
            file(files[1].0, files[1].1),
            Entry {
                path: "/usr/lib/libB.dylib".into(),
                kind: Kind::Symlink {
                    target: "libA.dylib".into(),
                },
            },
            directory("/usr/share"),
            directory("/usr/share/doc"),
            file(files[2].0, files[2].1),
        ];
        let manifest = Manifest::new(
            [
                ("arch".to_string(), "x86_64".to_string()),
                ("project".to_string(), "minidarwin".to_string()),
            ]
            .into(),
            None,
            entries,
        );
        (manifest, files)
    }

    #[test]
    fn a_rendered_manifest_parses_back_to_itself() {
        let (manifest, _) = sample();
        let text = manifest.render();
        let parsed = Manifest::parse(text.as_bytes(), "sample").expect("valid");
        assert_eq!(parsed.entries, manifest.entries);
        assert_eq!(parsed.tree, manifest.tree);
        assert_eq!(
            parsed.tree,
            "1cb4990e4557b548759f593c103a1a05e0d4558f9c5fb0e3b53969165ae88852"
        );
    }

    #[test]
    fn a_manifest_that_lies_about_itself_is_refused() {
        let (manifest, _) = sample();
        let text = manifest.render();
        let cases = [
            // A file's hash changed without the directory digests following.
            text.replacen(&sha256_hex(b"data\n"), &sha256_hex(b"evil\n"), 1),
            // Not canonical: extra whitespace.
            text.replacen("format: 1", "format:  1", 1),
            // A mode that is not allowed.
            text.replacen("mode: \"0644\"", "mode: \"4755\"", 1),
            // An unknown top-level key.
            format!("{text}extra: 1\n"),
            // A summary that disagrees.
            text.replacen("files: 3", "files: 4", 1),
            // An escape through a symlink.
            text.replacen(
                "target: \"libA.dylib\"",
                "target: \"../../../etc/passwd\"",
                1,
            ),
        ];
        for (index, bad) in cases.iter().enumerate() {
            assert!(
                Manifest::parse(bad.as_bytes(), "bad").is_err(),
                "case {index} was accepted"
            );
        }
    }

    #[test]
    fn siblings_that_differ_only_in_case_are_refused() {
        let text = concat!(
            "format: 1\nowner: \"minidarwin\"\nidentity:\n  arch: \"x\"\n  project: \"p\"\n",
            "tree:\n  algorithm: \"sha256-merkle-v1\"\n  digest: \"",
            "0000000000000000000000000000000000000000000000000000000000000000\"\n",
            "summary:\n  files: 0\n  symlinks: 2\n  directories: 0\n  bytes: 0\n",
            "entries:\n",
            "  - {path: \"/A\", type: \"symlink\", target: \"b\"}\n",
            "  - {path: \"/a\", type: \"symlink\", target: \"b\"}\n",
        );
        let error = Manifest::parse(text.as_bytes(), "case").unwrap_err();
        assert!(error.0.contains("differ only in case"), "{error}");
    }
}
