use std::path::{Path, PathBuf};

use crate::bundle;
use crate::cache::{Cache, remove_any};
use crate::fetch;
use crate::format::manifest::Manifest;
use crate::format::spec::{Pin, Policy};
use crate::format::{Invalid, MANIFEST_LIMIT, SPEC_LIMIT, Spec, read_limited, sha256_file};
use crate::layout::{self, Prefix};
use crate::repair::{self, Outcome};
use crate::state::{Base, State, Trust};
use crate::system::{self, SystemFiles};
use crate::verify::{self, Problem, Scope};

#[derive(Debug)]
pub enum Error {
    /// The base is not available offline, and the user said no to fetching
    /// it (or could not be asked).
    Declined,
    Invalid(Invalid),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Declined => f.write_str("the base is not cached, and was not downloaded"),
            Error::Invalid(e) => e.fmt(f),
        }
    }
}

impl From<Invalid> for Error {
    fn from(e: Invalid) -> Error {
        Error::Invalid(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Invalid(e.into())
    }
}

/// A spec to trust, and where it came from.
#[derive(Debug, Clone)]
pub struct Chosen {
    pub spec: Spec,
    pub text: String,
    pub trust: Trust,
    /// A bundle to take the manifest and artifact from before any network.
    /// Its own spec is ignored: the trusted one is this one.
    pub bundle: Option<PathBuf>,
}

impl Chosen {
    /// The spec compiled into this build for `arch`.
    pub fn pinned(arch: &str) -> Result<Chosen, Invalid> {
        let text = crate::pinned::text(arch).ok_or_else(|| {
            Invalid::new(format!(
                "no MiniDarwin release is pinned for '{arch}'; this build has {}",
                crate::pinned::architectures()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok(Chosen {
            spec: Spec::parse(text.as_bytes(), &format!("pinned {arch} spec"))?,
            text: text.to_string(),
            trust: Trust::Pinned,
            bundle: None,
        })
    }

    /// A spec file the user named.
    pub fn user(path: &Path) -> Result<Chosen, Invalid> {
        let what = path.display().to_string();
        let raw = read_limited(path, SPEC_LIMIT, &what)?;
        let text =
            String::from_utf8(raw).map_err(|_| Invalid::new(format!("{what}: not UTF-8")))?;
        Ok(Chosen {
            spec: Spec::parse(text.as_bytes(), &what)?,
            text,
            trust: Trust::User,
            bundle: None,
        })
    }
}

/// What the user is asked before anything is downloaded.
pub struct Request<'a> {
    pub spec: &'a Spec,
    /// The download's size, when the spec gives it.
    pub bytes: Option<u64>,
    pub sources: Vec<&'a str>,
}

pub enum Answer {
    Download,
    /// Take the files from this bundle instead.
    Bundle(PathBuf),
    Decline,
}

/// A verified base, ready to be put into a prefix.
pub struct Obtained {
    pub manifest: Manifest,
    pub manifest_raw: Vec<u8>,
    pub manifest_sha256: String,
    pub artifact_sha256: Option<String>,
    /// The verified, unpacked tree in the cache.
    pub tree: PathBuf,
}

struct Session<'a> {
    cache: &'a Cache,
    spec: &'a Spec,
    staging: PathBuf,
    bundle: Option<bundle::Opened>,
    consented: bool,
    ask: &'a mut dyn FnMut(&Request) -> Answer,
    say: &'a mut dyn FnMut(&str),
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = remove_any(&self.staging);
    }
}

/// Find, check and cache the base `chosen` names. `ask` is called at most
/// once, and only if the network is the one place left to look; `say` is told
/// what was checked.
pub fn obtain(
    cache: &Cache,
    chosen: &Chosen,
    ask: &mut dyn FnMut(&Request) -> Answer,
    say: &mut dyn FnMut(&str),
) -> Result<Obtained, Error> {
    let spec = &chosen.spec;
    if spec.policy == Policy::Insecure {
        say("spec: *** verification policy is INSECURE ***");
        say("      Missing hashes are not checked. Whatever is verified below is");
        say("      checked against files that nothing authenticates.");
    }
    let mut session = Session {
        cache,
        spec,
        staging: cache.staging()?,
        bundle: None,
        consented: false,
        ask,
        say,
    };
    if let Some(path) = &chosen.bundle {
        session.open_bundle(path)?;
    }

    // The manifest.
    let cached = spec.manifest.sha256.as_ref().map(|sha| cache.manifest(sha));
    let (manifest_path, manifest_sha256) = session.locate(
        "manifest",
        &spec.manifest,
        cached,
        |b| &b.manifest,
        Some(MANIFEST_LIMIT),
    )?;
    let manifest_raw = read_limited(&manifest_path, MANIFEST_LIMIT, "manifest")?;
    let manifest = Manifest::parse(&manifest_raw, "manifest")?;
    let s = manifest.summary;
    (session.say)(&format!(
        "manifest: canonical; tree {}; {} files, {} symlinks, {} directories, {} bytes",
        manifest.tree, s.files, s.symlinks, s.directories, s.bytes
    ));
    if let Some(tree) = &spec.tree
        && *tree != manifest.tree
    {
        return Err(Invalid::new(format!(
            "manifest: tree {}, spec says {tree}",
            manifest.tree
        ))
        .into());
    }
    if let Some(identity) = &spec.identity
        && *identity != manifest.identity
    {
        return Err(Invalid::new(format!(
            "manifest: identity {:?} differs from the spec's {identity:?}",
            manifest.identity
        ))
        .into());
    }
    session.keep(&manifest_path, &cache.manifest(&manifest_sha256))?;

    // The tree, from the cache if it is still intact.
    let tree = cache.tree(&manifest.tree);
    if tree.is_dir() {
        if verify::compare(&tree, &manifest, false, &Scope::All)
            .is_ok_and(|problems| problems.is_empty())
        {
            (session.say)("tree:     cached, and verified");
            return Ok(Obtained {
                manifest,
                manifest_raw,
                manifest_sha256,
                artifact_sha256: spec.artifact.sha256.clone(),
                tree,
            });
        }
        (session.say)("tree:     the cached copy is damaged; unpacking it again");
        remove_any(&tree)?;
    }

    // The artifact, unpacked.
    let cached = spec.artifact.sha256.as_ref().map(|sha| cache.artifact(sha));
    let (artifact_path, artifact_sha256) =
        session.locate("artifact", &spec.artifact, cached, |b| &b.artifact, None)?;
    let dest = session.staging.join("root");
    let file = std::fs::File::open(&artifact_path)?;
    let count = crate::unpack::extract(std::io::BufReader::new(file), &dest, &manifest)?;
    (session.say)(&format!(
        "extract:  {count} entries, every one named by the manifest"
    ));
    let problems = verify::compare(&dest, &manifest, false, &Scope::All)?;
    if !problems.is_empty() {
        return Err(Invalid::new(format!(
            "the unpacked artifact differs from its manifest: {}",
            describe(&problems)
        ))
        .into());
    }
    (session.say)(&format!(
        "tree:     {} entries verified",
        manifest.entries.len()
    ));
    session.keep(&artifact_path, &cache.artifact(&artifact_sha256))?;
    cache.publish(&dest, &tree)?;
    Ok(Obtained {
        manifest,
        manifest_raw,
        manifest_sha256,
        artifact_sha256: Some(artifact_sha256),
        tree,
    })
}

impl Session<'_> {
    fn open_bundle(&mut self, path: &Path) -> Result<(), Invalid> {
        let into = self.staging.join("bundle");
        self.bundle = Some(bundle::open(path, &into, self.spec.artifact.size)?);
        (self.say)(&format!(
            "bundle:   {}; its own spec is ignored",
            path.display()
        ));
        Ok(())
    }

    /// A checked copy of the file `pin` names: from the cache, then the
    /// bundle, then - with consent - its sources.
    fn locate(
        &mut self,
        what: &str,
        pin: &Pin,
        cached: Option<PathBuf>,
        from_bundle: fn(&bundle::Opened) -> &PathBuf,
        limit: Option<u64>,
    ) -> Result<(PathBuf, String), Error> {
        if let Some(cached) = cached.filter(|path| path.is_file()) {
            match check(&cached, pin, what) {
                Ok((sha, _)) => {
                    (self.say)(&format!("{what}: cached, sha256 ok {sha}"));
                    return Ok((cached, sha));
                }
                Err(e) => {
                    (self.say)(&format!(
                        "{what}: the cached copy fails its check ({e}); discarding it"
                    ));
                    let _ = std::fs::remove_file(&cached);
                }
            }
        }
        if self.bundle.is_none() && fetch::needs_network(&pin.sources) && !self.consented {
            let spec = self.spec;
            let bytes = match (spec.artifact.size, spec.manifest.size) {
                (Some(a), Some(m)) => Some(a + m),
                _ => None,
            };
            let request = Request {
                spec,
                bytes,
                sources: pin.sources.iter().map(String::as_str).collect(),
            };
            match (self.ask)(&request) {
                Answer::Download => self.consented = true,
                Answer::Bundle(path) => self.open_bundle(&path)?,
                Answer::Decline => return Err(Error::Declined),
            }
        }
        if let Some(opened) = &self.bundle {
            let path = from_bundle(opened).clone();
            let (sha, checked) = check(&path, pin, what)?;
            (self.say)(&format!(
                "{what}: from the bundle, {}",
                verdict(checked, &sha)
            ));
            return Ok((path, sha));
        }
        let dest = self.staging.join(what);
        fetch::fetch(&pin.sources, pin.size.or(limit), &dest)
            .map_err(|e| Invalid::new(format!("{what}: {e}")))?;
        let (sha, checked) = check(&dest, pin, what)?;
        (self.say)(&format!("{what}: fetched, {}", verdict(checked, &sha)));
        Ok((dest, sha))
    }

    /// Copy a checked file into the cache under its hash, unless it is
    /// already there.
    fn keep(&self, path: &Path, dest: &Path) -> std::io::Result<()> {
        if path == dest || dest.exists() {
            return Ok(());
        }
        let staged = self.staging.join("keep");
        let _ = std::fs::remove_file(&staged);
        std::fs::copy(path, &staged)?;
        self.cache.publish(&staged, dest)
    }
}

fn verdict(checked: bool, sha: &str) -> String {
    match checked {
        true => format!("sha256 ok {sha}"),
        false => format!("NOT CHECKED {sha}"),
    }
}

/// The file's SHA-256, and whether the spec pinned it; an error if it pinned a
/// different hash or size.
fn check(path: &Path, pin: &Pin, what: &str) -> Result<(String, bool), Invalid> {
    let (sha, size) = sha256_file(path).map_err(|e| Invalid::new(format!("{what}: {e}")))?;
    if let Some(expected) = pin.size
        && size != expected
    {
        return Err(Invalid::new(format!(
            "{what}: {size} bytes, spec says {expected}"
        )));
    }
    match &pin.sha256 {
        None => Ok((sha, false)),
        Some(expected) if *expected == sha => Ok((sha, true)),
        Some(expected) => Err(Invalid::new(format!(
            "{what}: sha256 {sha}, spec says {expected}"
        ))),
    }
}

/// A short list of problems for an error message.
pub fn describe(problems: &[Problem]) -> String {
    let mut parts: Vec<String> = problems
        .iter()
        .take(5)
        .map(|p| format!("{} {}", p.kind, p.path))
        .collect();
    if problems.len() > 5 {
        parts.push(format!("and {} more", problems.len() - 5));
    }
    parts.join(", ")
}

pub struct Options {
    pub policy: SystemFiles,
    /// Accept a pinned release older than one this prefix already took.
    pub allow_downgrade: bool,
}

pub struct Installed {
    pub outcome: Outcome,
    /// Paths an older base had and this one does not, removed because they
    /// were unchanged.
    pub retired: Vec<String>,
}

/// Put `obtained` into `prefix`, which must already have been created.
pub fn install(
    prefix: &Prefix,
    cache: &Cache,
    chosen: &Chosen,
    obtained: &Obtained,
    options: &Options,
) -> Result<Installed, Invalid> {
    let _lock = repair::lock(prefix)?;
    let state_path = layout::state_path(prefix.directory());
    let mut state = State::read(&state_path)?;

    let sequence = chosen.spec.release.sequence;
    if chosen.trust == Trust::Pinned
        && let (Some(new), Some(accepted)) = (sequence, state.accepted_sequence)
        && new < accepted
        && !options.allow_downgrade
    {
        return Err(Invalid::new(format!(
            "this prefix has accepted MiniDarwin release {accepted}, and this build pins release \
             {new}: that is a rollback. Pass --allow-downgrade if it is intended"
        )));
    }
    let old = match state.base {
        Some(_) => load(prefix).ok().map(|p| p.manifest),
        None => None,
    };

    let control = prefix.control();
    write_atomically(
        &control.join("spec.yaml"),
        system::with_policy(&chosen.text, options.policy).as_bytes(),
    )?;
    write_atomically(&control.join("manifest.yaml"), &obtained.manifest_raw)?;

    let root = prefix.drive();
    let mut problems = verify::compare(&root, &obtained.manifest, true, &Scope::All)?;
    let hardlinks = options.policy == SystemFiles::Immutable;
    let was_hardlinked = state
        .base
        .as_ref()
        .is_some_and(|b| b.materialised.iter().any(|m| m == "hardlink"));
    if was_hardlinked && !hardlinks {
        // Leaving `immutable`: every file is still the cache's own inode, and a
        // guest now allowed to write one in place would corrupt the shared
        // base. They all get their own copy.
        problems = unlink_everything(&obtained.manifest, problems);
    }
    let outcome = repair::apply(
        prefix,
        &root,
        &obtained.manifest,
        &obtained.tree,
        &problems,
        hardlinks,
    )?;
    let retired = match &old {
        Some(old) if old.tree != obtained.manifest.tree => {
            repair::retire(&root, old, &obtained.manifest)?
        }
        _ => Vec::new(),
    };

    let manifest = &obtained.manifest;
    state.base = Some(Base {
        project: manifest
            .identity
            .get("project")
            .cloned()
            .unwrap_or_default(),
        arch: manifest.arch().to_string(),
        sequence,
        tag: chosen.spec.release.tag.clone(),
        tree: manifest.tree.clone(),
        manifest_sha256: obtained.manifest_sha256.clone(),
        artifact_sha256: obtained.artifact_sha256.clone(),
        trust: chosen.trust,
        policy: chosen.spec.policy,
        materialised: match outcome.methods.is_empty() {
            // Nothing needed putting in; keep what was recorded before.
            true => state
                .base
                .as_ref()
                .map(|b| b.materialised.clone())
                .unwrap_or_default(),
            false => outcome
                .methods
                .iter()
                .map(|m| m.name().to_string())
                .collect(),
        },
    });
    if chosen.trust == Trust::Pinned
        && let Some(new) = sequence
    {
        state.accepted_sequence = Some(state.accepted_sequence.map_or(new, |old| old.max(new)));
    }
    state.write(&state_path)?;
    system::clear_dirty(prefix)?;
    let _ = cache.register(prefix.directory());
    Ok(Installed { outcome, retired })
}

/// `problems`, plus a replacement for every file that has none yet, in
/// manifest order.
fn unlink_everything(manifest: &Manifest, problems: Vec<Problem>) -> Vec<Problem> {
    let mut out = Vec::with_capacity(manifest.entries.len());
    let mut problems = problems.into_iter().peekable();
    for entry in &manifest.entries {
        let mut any = false;
        while let Some(problem) = problems.next_if(|p| p.path == entry.path) {
            out.push(problem);
            any = true;
        }
        if !any && matches!(entry.kind, crate::format::manifest::Kind::File { .. }) {
            out.push(Problem {
                kind: verify::Difference::Modified,
                path: entry.path.clone(),
                detail: "shares the cache's inode".into(),
            });
        }
    }
    out.extend(problems);
    out
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staged = path.with_extension("new");
    std::fs::write(&staged, bytes)?;
    std::fs::rename(staged, path)
}

/// A provisioned prefix, as it describes itself.
pub struct Provisioned {
    pub state: State,
    pub base: Base,
    pub spec: Spec,
    pub spec_text: String,
    pub manifest: Manifest,
    pub policy: SystemFiles,
}

impl Provisioned {
    /// The spec this prefix was made from, to obtain its base again.
    pub fn chosen(&self) -> Chosen {
        Chosen {
            spec: self.spec.clone(),
            text: self.spec_text.clone(),
            trust: self.base.trust,
            bundle: None,
        }
    }
}

/// Read a prefix's base description, or `Ok(None)` for a prefix with none.
///
/// The prefix's manifest is checked against the hash recorded when it was
/// installed, so a manifest edited to match a damaged tree is caught.
pub fn load(prefix: &Prefix) -> Result<Provisioned, Invalid> {
    let state = State::read(&layout::state_path(prefix.directory()))?;
    let Some(base) = state.base.clone() else {
        return Err(Invalid::new(format!(
            "{}: no base has been installed; run `rosetta3 prefix create`",
            prefix.directory().display()
        )));
    };
    let control = prefix.control();
    let spec_path = control.join("spec.yaml");
    let spec_raw = read_limited(&spec_path, SPEC_LIMIT, &spec_path.display().to_string())?;
    let spec_text = String::from_utf8(spec_raw)
        .map_err(|_| Invalid::new(format!("{}: not UTF-8", spec_path.display())))?;
    let spec = Spec::parse(spec_text.as_bytes(), &spec_path.display().to_string())?;
    let policy = SystemFiles::of(&spec)?;
    let manifest_path = control.join("manifest.yaml");
    let raw = read_limited(
        &manifest_path,
        MANIFEST_LIMIT,
        &manifest_path.display().to_string(),
    )?;
    let sha = crate::format::sha256_hex(&raw);
    if sha != base.manifest_sha256 {
        return Err(Invalid::new(format!(
            "{}: sha256 {sha}, but {} was installed; run `rosetta3 prefix reset-system`",
            manifest_path.display(),
            base.manifest_sha256
        )));
    }
    let manifest = Manifest::parse(&raw, &manifest_path.display().to_string())?;
    Ok(Provisioned {
        state,
        base,
        spec,
        spec_text,
        manifest,
        policy,
    })
}

/// Repair `problems` from the cached base, fetching it again only if the
/// cache has lost it and `ask` agrees.
pub fn repair(
    prefix: &Prefix,
    cache: &Cache,
    provisioned: &Provisioned,
    problems: &[Problem],
    ask: &mut dyn FnMut(&Request) -> Answer,
    say: &mut dyn FnMut(&str),
) -> Result<Outcome, Error> {
    let obtained = obtain(cache, &provisioned.chosen(), ask, say)?;
    if obtained.manifest_sha256 != provisioned.base.manifest_sha256 {
        return Err(
            Invalid::new("the cached base is not the one this prefix was made from").into(),
        );
    }
    let _lock = repair::lock(prefix)?;
    let hardlinks = provisioned.policy == SystemFiles::Immutable;
    Ok(repair::apply(
        prefix,
        &prefix.drive(),
        &provisioned.manifest,
        &obtained.tree,
        problems,
        hardlinks,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::manifest::tests::sample;
    use crate::unpack::tests::{good_members, tarball};
    use crate::verify::tests::scratch;

    /// A user spec pinning the sample tree, with its files as `file://`
    /// sources, so the whole path runs offline.
    fn fixture(at: &Path) -> (Chosen, PathBuf) {
        let (manifest, _) = sample();
        let manifest_text = manifest.render();
        let artifact = tarball(&good_members());
        let files = at.join("release");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("m.yaml"), &manifest_text).unwrap();
        std::fs::write(files.join("a.tar.gz"), &artifact).unwrap();
        let spec = format!(
            "format: 1\nidentity:\n  arch: \"x86_64\"\n  project: \"minidarwin\"\n\
             release:\n  sequence: 1\n\
             artifact:\n  format: \"tar+gzip\"\n  size: {}\n  sha256: \"{}\"\n  sources:\n    - \"file://{}\"\n\
             manifest:\n  size: {}\n  sha256: \"{}\"\n  tree: \"{}\"\n  sources:\n    - \"file://{}\"\n",
            artifact.len(),
            crate::format::sha256_hex(&artifact),
            files.join("a.tar.gz").display(),
            manifest_text.len(),
            crate::format::sha256_hex(manifest_text.as_bytes()),
            manifest.tree,
            files.join("m.yaml").display(),
        );
        let spec_path = files.join("spec.yaml");
        std::fs::write(&spec_path, spec).unwrap();
        (Chosen::user(&spec_path).unwrap(), files)
    }

    fn never(_: &Request) -> Answer {
        panic!("nothing here needs the network")
    }

    #[test]
    fn a_prefix_is_provisioned_verified_and_healed_offline() {
        let at = scratch("provision");
        let (chosen, files) = fixture(&at);
        let cache = Cache::at(at.join("cache"));
        let prefix = Prefix::at(at.join("prefix"));
        layout::create(prefix.directory()).unwrap();

        let obtained = obtain(&cache, &chosen, &mut never, &mut |_| {}).expect("obtained");
        let options = Options {
            policy: SystemFiles::Repair,
            allow_downgrade: false,
        };
        install(&prefix, &cache, &chosen, &obtained, &options).expect("installed");
        let provisioned = load(&prefix).expect("loads");
        let root = prefix.drive();
        assert_eq!(
            verify::compare(&root, &provisioned.manifest, true, &Scope::All).unwrap(),
            []
        );

        // The release files are gone: from here on only the cache is left.
        std::fs::remove_dir_all(&files).unwrap();
        std::fs::write(root.join("usr/lib/data.txt"), b"broken").unwrap();
        let problems = verify::compare(&root, &provisioned.manifest, true, &Scope::All).unwrap();
        assert_eq!(problems.len(), 1);
        repair(
            &prefix,
            &cache,
            &provisioned,
            &problems,
            &mut never,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            verify::compare(&root, &provisioned.manifest, true, &Scope::All).unwrap(),
            []
        );

        // A damaged cached tree is noticed and rebuilt from the cached
        // artifact, still offline.
        std::fs::write(obtained.tree.join("usr/lib/data.txt"), b"rot").unwrap();
        let again = obtain(&cache, &chosen, &mut never, &mut |_| {}).expect("rebuilt");
        assert_eq!(
            std::fs::read(again.tree.join("usr/lib/data.txt")).unwrap(),
            b"data\n"
        );
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn a_tampered_artifact_is_refused() {
        let at = scratch("provision-tamper");
        let (chosen, files) = fixture(&at);
        let mut bytes = std::fs::read(files.join("a.tar.gz")).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(files.join("a.tar.gz"), bytes).unwrap();
        let cache = Cache::at(at.join("cache"));
        let result = obtain(&cache, &chosen, &mut never, &mut |_| {});
        assert!(matches!(result, Err(Error::Invalid(e)) if e.0.contains("sha256")));
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn without_consent_nothing_is_downloaded() {
        let at = scratch("provision-decline");
        let (mut chosen, _) = fixture(&at);
        chosen.spec.manifest.sources = vec!["https://example.invalid/m.yaml".into()];
        let cache = Cache::at(at.join("cache"));
        let mut asked = 0;
        let result = obtain(
            &cache,
            &chosen,
            &mut |_| {
                asked += 1;
                Answer::Decline
            },
            &mut |_| {},
        );
        assert!(matches!(result, Err(Error::Declined)));
        assert_eq!(asked, 1);
        let _ = std::fs::remove_dir_all(&at);
    }

    /// Leaving `immutable` gives every file its own inode back, so a guest
    /// that may now write one cannot reach the shared cache through it.
    #[test]
    fn leaving_immutable_unshares_every_file() {
        use std::os::unix::fs::MetadataExt;
        let at = scratch("provision-unshare");
        let (chosen, _) = fixture(&at);
        let cache = Cache::at(at.join("cache"));
        let prefix = Prefix::at(at.join("prefix"));
        layout::create(prefix.directory()).unwrap();
        let obtained = obtain(&cache, &chosen, &mut never, &mut |_| {}).unwrap();
        let mut options = Options {
            policy: SystemFiles::Immutable,
            allow_downgrade: false,
        };
        install(&prefix, &cache, &chosen, &obtained, &options).unwrap();
        // Whether or not this filesystem could reflink, pretend the files are
        // the cache's own, as they are wherever it cannot.
        let file = "usr/lib/data.txt";
        std::fs::remove_file(prefix.drive().join(file)).unwrap();
        std::fs::hard_link(obtained.tree.join(file), prefix.drive().join(file)).unwrap();
        let state_path = layout::state_path(prefix.directory());
        let mut state = State::read(&state_path).unwrap();
        state.base.as_mut().unwrap().materialised = vec!["hardlink".into()];
        state.write(&state_path).unwrap();

        options.policy = SystemFiles::Repair;
        let installed = install(&prefix, &cache, &chosen, &obtained, &options).unwrap();
        assert_eq!(
            installed.outcome.restored.len(),
            3,
            "every file, and only files"
        );
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        assert_ne!(
            ino(&prefix.drive().join(file)),
            ino(&obtained.tree.join(file))
        );
        assert_eq!(load(&prefix).unwrap().policy, SystemFiles::Repair);
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn a_pinned_rollback_is_refused_unless_allowed() {
        let at = scratch("provision-rollback");
        let (mut chosen, _) = fixture(&at);
        let cache = Cache::at(at.join("cache"));
        let prefix = Prefix::at(at.join("prefix"));
        layout::create(prefix.directory()).unwrap();
        let obtained = obtain(&cache, &chosen, &mut never, &mut |_| {}).unwrap();
        let mut state = State::read(&layout::state_path(prefix.directory())).unwrap();
        state.accepted_sequence = Some(5);
        state
            .write(&layout::state_path(prefix.directory()))
            .unwrap();

        chosen.trust = Trust::Pinned;
        let mut options = Options {
            policy: SystemFiles::Repair,
            allow_downgrade: false,
        };
        assert!(install(&prefix, &cache, &chosen, &obtained, &options).is_err());
        options.allow_downgrade = true;
        install(&prefix, &cache, &chosen, &obtained, &options).expect("allowed");
        // A user-supplied spec is exempt.
        chosen.trust = Trust::User;
        options.allow_downgrade = false;
        install(&prefix, &cache, &chosen, &obtained, &options).expect("user spec");
        let _ = std::fs::remove_dir_all(&at);
    }
}
