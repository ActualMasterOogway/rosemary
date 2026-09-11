//! An on-disk cache of translated shader libraries.
//!
//! An entry is a whole [`TranslatedLibrary`]: its SPIR-V words and the
//! reflection the runtime binds resources from. Storing the modules without
//! the reflection would only move the cost, since the reflection comes out of
//! the same translation.
//!
//! Only successful translations are cached. A rejection carries diagnostics
//! whose value is that they are read, and a stale one would be reported as
//! though this build had produced it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, mpsc};

use crate::ir::{Access, ResourceKind, Stage};
use crate::sha256::{self, Sha256};
use crate::{
    AirInput, EntryArtifact, RequiredFeatures, ResourceBinding, TargetProfile, TranslateError,
    TranslatedLibrary,
};

/// The SHA-256 of every file in this crate, as lowercase hexadecimal.
///
/// `build.rs` computes it and re-runs whenever any of those files changes.
pub const CACHE_VERSION: &str = env!("METAL_TRANSLATE_CACHE_VERSION");

/// The container magic, which changes when the entry encoding does.
const MAGIC: &[u8; 8] = b"R3MTLC01";

/// Directory to hold the cache, overriding the user's cache directory.
///
/// Set to `0`, `off` or the empty string to translate every library afresh.
const DIRECTORY_VARIABLE: &str = "ROSETTA3_SHADER_CACHE";

/// Set to keep the entries of every other build of this crate on disk.
const KEEP_VARIABLE: &str = "ROSETTA3_SHADER_CACHE_KEEP";

/// Which frontend produced an entry, so that two frontends given identical
/// bytes cannot collide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Frontend {
    AirText,
    AirBitcode,
    Msl,
}

impl Frontend {
    fn tag(self) -> u8 {
        match self {
            Self::AirText => 0,
            Self::AirBitcode => 1,
            Self::Msl => 2,
        }
    }
}

/// What the cache did over the life of the process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    /// Reads and writes which failed, or entries which did not decode. Every
    /// one of them is recoverable - the library is translated instead - and a
    /// count that climbs is the only sign a cache directory is unusable.
    pub errors: u64,
}

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static WRITES: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);

/// What the cache has done so far, for a runtime which reports counters.
pub fn stats() -> Stats {
    Stats {
        hits: HITS.load(Ordering::Relaxed),
        misses: MISSES.load(Ordering::Relaxed),
        writes: WRITES.load(Ordering::Relaxed),
        errors: ERRORS.load(Ordering::Relaxed),
    }
}

/// [`crate::translate`] through the on-disk cache.
///
/// A cache which cannot be read or written is not an error: the library is
/// translated exactly as it would have been without one.
pub fn translate_cached(
    input: AirInput<'_>,
    target: &TargetProfile,
) -> Result<TranslatedLibrary, TranslateError> {
    let (frontend, bytes) = match input {
        AirInput::Text(text) => (Frontend::AirText, text.as_bytes()),
        AirInput::Bitcode(bytes) => (Frontend::AirBitcode, bytes),
    };
    cached(frontend, bytes, target, || crate::translate(input, target))
}

/// [`crate::translate_msl`] through the on-disk cache.
pub fn translate_msl_cached(
    source: &str,
    target: &TargetProfile,
) -> Result<TranslatedLibrary, TranslateError> {
    cached(Frontend::Msl, source.as_bytes(), target, || {
        crate::translate_msl(source, target)
    })
}

fn cached(
    frontend: Frontend,
    bytes: &[u8],
    target: &TargetProfile,
    translate: impl FnOnce() -> Result<TranslatedLibrary, TranslateError>,
) -> Result<TranslatedLibrary, TranslateError> {
    match root() {
        Some(root) => cached_in(root, frontend, bytes, target, translate),
        None => translate(),
    }
}

/// As [`cached`], against a named directory rather than the process-wide one.
fn cached_in(
    root: &Path,
    frontend: Frontend,
    bytes: &[u8],
    target: &TargetProfile,
    translate: impl FnOnce() -> Result<TranslatedLibrary, TranslateError>,
) -> Result<TranslatedLibrary, TranslateError> {
    let key = key(frontend, bytes, target);
    let path = entry_path(root, &key);

    if let Some(library) = read_entry(&path, &key) {
        HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(library);
    }
    if let Some(library) = pending_entry(&path, &key) {
        HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(library);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);

    let library = translate()?;
    queue_entry(path, key, &library);
    Ok(library)
}

/// The key naming this input, this target and this build of the translator.
fn key(frontend: Frontend, bytes: &[u8], target: &TargetProfile) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update_field(MAGIC);
    hasher.update_field(CACHE_VERSION.as_bytes());
    hasher.update(&[frontend.tag()]);
    hasher.update_field(&encode_target(target));
    hasher.update_field(bytes);
    hasher.finish()
}

/// The target profile as the key sees it.
///
/// Every field is included, because emission reads every field: a module
/// emitted for an adapter without `variablePointers` is not the module the
/// same source emits for one with it.
fn encode_target(target: &TargetProfile) -> Vec<u8> {
    let TargetProfile {
        vulkan_version,
        spirv_version,
        max_bound_descriptor_sets,
        shader,
        shader_int64,
        variable_pointers_storage_buffer,
        variable_pointers,
    } = target;

    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&[vulkan_version.0, vulkan_version.1]);
    bytes.extend_from_slice(&[spirv_version.0, spirv_version.1]);
    bytes.extend_from_slice(&max_bound_descriptor_sets.to_le_bytes());
    for flag in [
        shader,
        shader_int64,
        variable_pointers_storage_buffer,
        variable_pointers,
    ] {
        bytes.push(u8::from(*flag));
    }
    bytes
}

/// The directory this build's entries live in, or `None` where the cache is
/// disabled or the process has nowhere to put one.
fn root() -> Option<&'static Path> {
    static ROOT: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
        let base = base_directory()?;
        // The version directory is named by a prefix of the digest: a full
        // one is 64 characters of path in front of every entry, and 64 bits
        // is far more than the number of builds a machine ever holds.
        let version = &CACHE_VERSION[..16];
        if std::env::var_os(KEEP_VARIABLE).is_none() {
            prune(&base, version);
        }
        let root = base.join(version);
        std::fs::create_dir_all(&root).ok()?;
        Some(root)
    });
    ROOT.as_deref()
}

fn base_directory() -> Option<PathBuf> {
    base_directory_from(
        std::env::var_os(DIRECTORY_VARIABLE),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Where the cache lives, given the three variables which can name it.
///
/// Taking them as arguments keeps the choice testable: the process-wide root
/// is resolved once, so a test which set a variable could not observe it.
fn base_directory_from(
    override_directory: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(directory) = override_directory {
        let disabled = matches!(directory.to_str(), Some("") | Some("0") | Some("off"));
        return (!disabled).then(|| PathBuf::from(directory));
    }
    if let Some(cache) = xdg_cache_home.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(cache).join("rosetta3/metal-shaders"));
    }
    let home = home.filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".cache/rosetta3/metal-shaders"))
}

/// Remove the entries of every other build of this crate.
///
/// A cache keyed by crate content would otherwise keep one pack per build, and
/// a session which rebuilds the translator a dozen times leaves a dozen packs
/// behind. Set `ROSETTA3_SHADER_CACHE_KEEP` while bisecting, where two builds
/// are run in turn and each would otherwise discard the other's work.
fn prune(base: &Path, version: &str) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == version {
            continue;
        }
        // Only ever remove a directory this module could have created: a
        // 16-character hexadecimal name, and nothing a user put here.
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() != 16 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Entries are spread over 256 directories by the first byte of their key,
/// because a single directory of thousands of files is slow to open on some
/// filesystems and unpleasant to look at on all of them.
fn entry_path(root: &Path, key: &[u8; 32]) -> PathBuf {
    let name = sha256::hex(key);
    root.join(&name[..2]).join(format!("{name}.shader"))
}

fn read_entry(path: &Path, key: &[u8; 32]) -> Option<TranslatedLibrary> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // A miss is the common case and is not an error.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    match decode(&bytes, key) {
        Some(library) => Some(library),
        None => {
            // A truncated or corrupt entry is replaced rather than trusted.
            // Do not unlink it here: another process may have atomically
            // replaced it between our read and this decision.
            ERRORS.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

struct WriteJob {
    path: PathBuf,
    key: [u8; 32],
    library: Arc<TranslatedLibrary>,
}

struct PendingEntry {
    key: [u8; 32],
    library: Arc<TranslatedLibrary>,
}

struct WriterState {
    inner: Mutex<WriterStateInner>,
    drained: Condvar,
}

struct WriterStateInner {
    accepting: bool,
    pending: HashMap<PathBuf, PendingEntry>,
}

struct Writer {
    sender: mpsc::Sender<WriteJob>,
    state: Arc<WriterState>,
}

fn pending_entry(path: &Path, key: &[u8; 32]) -> Option<TranslatedLibrary> {
    let writer = writer()?;
    let inner = writer
        .state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = inner.pending.get(path)?;
    (entry.key == *key).then(|| (*entry.library).clone())
}

/// Queue an owned cache entry for the single disk writer.
///
/// Keeping the library in `pending` makes a second lookup a hit even before
/// the rename reaches disk. It also coalesces simultaneous writes of the same
/// key, while the worker's owned values make the caller's lifetime irrelevant.
fn queue_entry(path: PathBuf, key: [u8; 32], library: &TranslatedLibrary) {
    let Some(writer) = writer() else {
        return;
    };
    let mut inner = writer
        .state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !inner.accepting || inner.pending.contains_key(&path) {
        return;
    }
    let library = Arc::new(library.clone());
    inner.pending.insert(
        path.clone(),
        PendingEntry {
            key,
            library: Arc::clone(&library),
        },
    );
    if writer
        .sender
        .send(WriteJob {
            path: path.clone(),
            key,
            library,
        })
        .is_err()
    {
        inner.pending.remove(&path);
        ERRORS.fetch_add(1, Ordering::Relaxed);
        writer.state.drained.notify_all();
    }
}

fn writer() -> Option<&'static Writer> {
    static WRITER: OnceLock<Option<Writer>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            let state = Arc::new(WriterState {
                inner: Mutex::new(WriterStateInner {
                    accepting: true,
                    pending: HashMap::new(),
                }),
                drained: Condvar::new(),
            });
            let worker_state = Arc::clone(&state);
            if std::thread::Builder::new()
                .name("metal:shader-cache".to_owned())
                .spawn(move || write_entries(receiver, &worker_state))
                .is_err()
            {
                ERRORS.fetch_add(1, Ordering::Relaxed);
                return None;
            }

            // A guest commonly terminates through libc's `exit`, which does
            // not unwind Rust frames. Drain there so a successful translation
            // queued just before exit is not silently lost.
            extern "C" fn flush_at_exit() {
                drain(true);
            }
            // SAFETY: the callback has C's required signature and touches only
            // process-lifetime state. The worker owns everything it writes.
            if unsafe { libc::atexit(flush_at_exit) } != 0 {
                ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            Some(Writer { sender, state })
        })
        .as_ref()
}

fn write_entries(receiver: mpsc::Receiver<WriteJob>, state: &WriterState) {
    while let Ok(job) = receiver.recv() {
        if write_entry_inner(&job.path, &job.key, &job.library).is_some() {
            WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            ERRORS.fetch_add(1, Ordering::Relaxed);
        }
        let mut inner = state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.pending.remove(&job.path);
        state.drained.notify_all();
    }
}

/// Wait until all cache writes queued so far have reached their final state.
///
/// Normal translation does not call this. It is useful to embedders before a
/// controlled shutdown, and the process exit handler uses its stopping form.
pub fn flush() {
    drain(false);
}

fn drain(stop: bool) {
    let Some(writer) = writer() else {
        return;
    };
    let mut inner = writer
        .state
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if stop {
        inner.accepting = false;
    }
    while !inner.pending.is_empty() {
        inner = writer
            .state
            .drained
            .wait(inner)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

/// Write the entry through a temporary file in the same directory.
///
/// A reader must never see half an entry, and several processes may translate
/// the same library at once, so the file appears at its final name by a
/// rename, which is atomic within a filesystem, and never by being written in
/// place. Two writers of the same key write the same bytes, so whichever
/// rename lands last is correct.
fn write_entry_inner(path: &Path, key: &[u8; 32], library: &TranslatedLibrary) -> Option<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let directory = path.parent()?;
    std::fs::create_dir_all(directory).ok()?;
    let temporary = directory.join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let encoded = encode(key, library);
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()
    };
    if write().is_err() {
        let _ = std::fs::remove_file(&temporary);
        return None;
    }
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return None;
    }
    Some(())
}

fn encode(key: &[u8; 32], library: &TranslatedLibrary) -> Vec<u8> {
    let mut payload = Vec::new();
    write_u32(&mut payload, library.entries.len() as u32);
    for entry in &library.entries {
        write_string(&mut payload, &entry.name);
        payload.push(stage_tag(entry.stage));
        write_u32(&mut payload, entry.words.len() as u32);
        for word in &entry.words {
            write_u32(&mut payload, *word);
        }
        write_u32(&mut payload, entry.resources.len() as u32);
        for resource in &entry.resources {
            payload.push(stage_tag(resource.stage));
            payload.push(resource_kind_tag(resource.kind));
            write_u32(&mut payload, resource.metal_index);
            write_u32(&mut payload, resource.descriptor_set);
            write_u32(&mut payload, resource.binding);
            write_u32(&mut payload, resource.count);
            payload.push(access_tag(resource.access));
            payload.push(u8::from(resource.sampled));
        }
        write_strings(&mut payload, &entry.required.capabilities);
        write_strings(&mut payload, &entry.required.extensions);
    }

    let mut bytes = Vec::with_capacity(payload.len() + 112);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&sha256::digest(CACHE_VERSION.as_bytes()));
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&sha256::digest(&payload));
    bytes.extend_from_slice(&payload);
    bytes
}

fn decode(bytes: &[u8], key: &[u8; 32]) -> Option<TranslatedLibrary> {
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len())? != MAGIC {
        return None;
    }
    // The key already covers the crate version, so this only catches a file
    // moved between version directories by hand.
    if reader.take(32)? != sha256::digest(CACHE_VERSION.as_bytes()) {
        return None;
    }
    if reader.take(32)? != key {
        return None;
    }
    let length = usize::try_from(reader.u64()?).ok()?;
    let checksum = reader.take(32)?.to_vec();
    let payload = reader.take(length)?;
    if !reader.at_end() || sha256::digest(payload) != checksum.as_slice() {
        return None;
    }

    let mut reader = Reader::new(payload);
    let count = reader.u32()? as usize;
    let mut entries = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let name = reader.string()?;
        let stage = stage_of_tag(reader.u8()?)?;
        let word_count = reader.u32()? as usize;
        let mut words = Vec::with_capacity(word_count.min(1 << 16));
        for _ in 0..word_count {
            words.push(reader.u32()?);
        }
        let resource_count = reader.u32()? as usize;
        let mut resources = Vec::with_capacity(resource_count.min(1024));
        for _ in 0..resource_count {
            resources.push(ResourceBinding {
                stage: stage_of_tag(reader.u8()?)?,
                kind: resource_kind_of_tag(reader.u8()?)?,
                metal_index: reader.u32()?,
                descriptor_set: reader.u32()?,
                binding: reader.u32()?,
                count: reader.u32()?,
                access: access_of_tag(reader.u8()?)?,
                sampled: reader.boolean()?,
            });
        }
        entries.push(EntryArtifact {
            name,
            stage,
            words,
            resources,
            required: RequiredFeatures {
                capabilities: reader.strings()?,
                extensions: reader.strings()?,
            },
        });
    }
    reader.at_end().then_some(TranslatedLibrary { entries })
}

fn write_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_string(bytes: &mut Vec<u8>, text: &str) {
    write_u32(bytes, text.len() as u32);
    bytes.extend_from_slice(text.as_bytes());
}

fn write_strings(bytes: &mut Vec<u8>, texts: &[String]) {
    write_u32(bytes, texts.len() as u32);
    for text in texts {
        write_string(bytes, text);
    }
}

/// A bounds-checked reader: a corrupt entry answers `None` and is discarded,
/// and no length in the file is ever trusted enough to index with.
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(length)?;
        let slice = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn boolean(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn string(&mut self) -> Option<String> {
        let length = self.u32()? as usize;
        std::str::from_utf8(self.take(length)?)
            .ok()
            .map(str::to_owned)
    }

    fn strings(&mut self) -> Option<Vec<String>> {
        let count = self.u32()? as usize;
        let mut texts = Vec::with_capacity(count.min(256));
        for _ in 0..count {
            texts.push(self.string()?);
        }
        Some(texts)
    }

    fn at_end(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

// The tags below are the file format rather than the IR, so each is written
// out: a variant added to the IR must be given a tag here, and reordering the
// IR must not silently reinterpret entries already on disk.

fn stage_tag(stage: Stage) -> u8 {
    match stage {
        Stage::Vertex => 0,
        Stage::Fragment => 1,
        Stage::Kernel => 2,
        Stage::Visible => 3,
        Stage::Intersection => 4,
        Stage::Mesh => 5,
        Stage::Object => 6,
        Stage::Tile => 7,
    }
}

fn stage_of_tag(tag: u8) -> Option<Stage> {
    Some(match tag {
        0 => Stage::Vertex,
        1 => Stage::Fragment,
        2 => Stage::Kernel,
        3 => Stage::Visible,
        4 => Stage::Intersection,
        5 => Stage::Mesh,
        6 => Stage::Object,
        7 => Stage::Tile,
        _ => return None,
    })
}

fn resource_kind_tag(kind: ResourceKind) -> u8 {
    match kind {
        ResourceKind::Buffer => 0,
        ResourceKind::Texture => 1,
        ResourceKind::Sampler => 2,
        ResourceKind::AccelerationStructure => 3,
        ResourceKind::VisibleFunctionTable => 4,
        ResourceKind::IntersectionFunctionTable => 5,
        ResourceKind::ArgumentBuffer => 6,
        ResourceKind::Imageblock => 7,
    }
}

fn resource_kind_of_tag(tag: u8) -> Option<ResourceKind> {
    Some(match tag {
        0 => ResourceKind::Buffer,
        1 => ResourceKind::Texture,
        2 => ResourceKind::Sampler,
        3 => ResourceKind::AccelerationStructure,
        4 => ResourceKind::VisibleFunctionTable,
        5 => ResourceKind::IntersectionFunctionTable,
        6 => ResourceKind::ArgumentBuffer,
        7 => ResourceKind::Imageblock,
        _ => return None,
    })
}

fn access_tag(access: Access) -> u8 {
    match access {
        Access::Sample => 0,
        Access::Read => 1,
        Access::Write => 2,
        Access::ReadWrite => 3,
    }
}

fn access_of_tag(tag: u8) -> Option<Access> {
    Some(match tag {
        0 => Access::Sample,
        1 => Access::Read,
        2 => Access::Write,
        3 => Access::ReadWrite,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Stage;

    /// A directory of this test's own, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "metal-translate-cache-{}-{name}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            flush();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn library() -> TranslatedLibrary {
        TranslatedLibrary {
            entries: vec![
                EntryArtifact {
                    name: "kernel_store_const".to_owned(),
                    stage: Stage::Kernel,
                    words: vec![0x0723_0203, 1, 2, 3, u32::MAX],
                    resources: vec![
                        ResourceBinding {
                            stage: Stage::Kernel,
                            kind: ResourceKind::Buffer,
                            metal_index: 0,
                            descriptor_set: 0,
                            binding: 0,
                            count: 1,
                            access: Access::ReadWrite,
                            sampled: false,
                        },
                        ResourceBinding {
                            stage: Stage::Fragment,
                            kind: ResourceKind::Texture,
                            metal_index: 3,
                            descriptor_set: 1,
                            binding: 515,
                            count: 1,
                            access: Access::Sample,
                            sampled: true,
                        },
                    ],
                    required: RequiredFeatures {
                        capabilities: vec!["Shader".to_owned(), "VariablePointers".to_owned()],
                        extensions: vec![],
                    },
                },
                EntryArtifact {
                    name: String::new(),
                    stage: Stage::Vertex,
                    words: vec![],
                    resources: vec![],
                    required: RequiredFeatures {
                        capabilities: vec![],
                        extensions: vec!["SPV_KHR_variable_pointers".to_owned()],
                    },
                },
            ],
        }
    }

    /// The narrowest MSL the frontend accepts, so a test can translate.
    const SOURCE: &str = "kernel void clear(device uint* output [[buffer(3)]]) { output[0] = 42; }";

    #[test]
    fn an_entry_round_trips_through_its_file_encoding() {
        let key = [0x5a; 32];
        let encoded = encode(&key, &library());
        assert_eq!(decode(&encoded, &key), Some(library()));
    }

    #[test]
    fn a_corrupt_or_foreign_entry_decodes_to_nothing() {
        let key = [0x5a; 32];
        let encoded = encode(&key, &library());

        assert_eq!(decode(&encoded, &[0x5b; 32]), None, "another key's entry");
        assert_eq!(decode(&encoded[..encoded.len() - 1], &key), None, "cut off");
        assert_eq!(decode(&[], &key), None, "empty");

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing, &key), None, "trailing bytes");

        let mut flipped = encoded.clone();
        *flipped.last_mut().expect("a payload") ^= 0xff;
        assert_eq!(decode(&flipped, &key), None, "a flipped payload bit");

        let mut magic = encoded;
        magic[0] = b'X';
        assert_eq!(decode(&magic, &key), None, "another container");
    }

    #[test]
    fn a_key_covers_the_input_the_target_and_the_frontend() {
        let target = TargetProfile::vulkan_1_3();
        let base = key(Frontend::AirBitcode, b"air", &target);

        assert_ne!(base, key(Frontend::AirBitcode, b"air2", &target));
        assert_ne!(base, key(Frontend::AirText, b"air", &target));
        assert_ne!(base, key(Frontend::Msl, b"air", &target));

        let mut wider = TargetProfile::vulkan_1_3();
        wider.variable_pointers = true;
        assert_ne!(base, key(Frontend::AirBitcode, b"air", &wider));

        let mut int64 = TargetProfile::vulkan_1_3();
        int64.shader_int64 = true;
        assert_ne!(base, key(Frontend::AirBitcode, b"air", &int64));

        assert_eq!(base, key(Frontend::AirBitcode, b"air", &target), "stable");
    }

    #[test]
    fn a_second_translation_of_the_same_source_comes_from_the_cache() {
        let scratch = Scratch::new("hit");
        let target = TargetProfile::vulkan_1_3();
        let translate = || crate::translate_msl(SOURCE, &target);

        let first = cached_in(
            &scratch.0,
            Frontend::Msl,
            SOURCE.as_bytes(),
            &target,
            translate,
        )
        .expect("the MSL frontend accepts this kernel");

        let mut translated_again = false;
        let second = cached_in(
            &scratch.0,
            Frontend::Msl,
            SOURCE.as_bytes(),
            &target,
            || {
                translated_again = true;
                translate()
            },
        )
        .expect("a cached library");

        assert_eq!(first, second);
        assert!(!translated_again, "the second call retranslated");
    }

    #[test]
    fn a_damaged_entry_is_replaced_rather_than_trusted() {
        let scratch = Scratch::new("damaged");
        let target = TargetProfile::vulkan_1_3();
        let translate = || crate::translate_msl(SOURCE, &target);
        let key = key(Frontend::Msl, SOURCE.as_bytes(), &target);
        let path = entry_path(&scratch.0, &key);

        let expected = cached_in(
            &scratch.0,
            Frontend::Msl,
            SOURCE.as_bytes(),
            &target,
            translate,
        )
        .expect("the MSL frontend accepts this kernel");
        flush();
        std::fs::write(&path, b"not a shader cache entry").expect("damage the entry");

        let recovered = cached_in(
            &scratch.0,
            Frontend::Msl,
            SOURCE.as_bytes(),
            &target,
            translate,
        )
        .expect("a retranslated library");
        flush();
        assert_eq!(expected, recovered);
        assert!(path.exists(), "the entry was not written again");
        assert_eq!(
            decode(&std::fs::read(&path).expect("the entry"), &key),
            Some(expected)
        );
    }

    #[test]
    fn a_translation_failure_is_not_cached() {
        let scratch = Scratch::new("failure");
        let target = TargetProfile::vulkan_1_3();
        let source = "this is not Metal shading language";
        let key = key(Frontend::Msl, source.as_bytes(), &target);

        assert!(
            cached_in(
                &scratch.0,
                Frontend::Msl,
                source.as_bytes(),
                &target,
                || { crate::translate_msl(source, &target) }
            )
            .is_err()
        );
        assert!(!entry_path(&scratch.0, &key).exists());
    }

    #[test]
    fn flush_finishes_an_asynchronous_write() {
        let scratch = Scratch::new("flush");
        let key = [0x7b; 32];
        let path = entry_path(&scratch.0, &key);
        let expected = library();

        queue_entry(path.clone(), key, &expected);
        flush();

        assert_eq!(read_entry(&path, &key), Some(expected));
        let temporary_files = std::fs::read_dir(path.parent().expect("entry directory"))
            .expect("entry directory")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
    }

    #[test]
    fn the_cache_directory_follows_the_environment() {
        let named = base_directory_from(Some("/named".into()), None, None);
        assert_eq!(named, Some(PathBuf::from("/named")));

        for disabled in ["", "0", "off"] {
            assert_eq!(base_directory_from(Some(disabled.into()), None, None), None);
        }

        assert_eq!(
            base_directory_from(None, Some("/xdg".into()), Some("/home/user".into())),
            Some(PathBuf::from("/xdg/rosetta3/metal-shaders")),
        );
        assert_eq!(
            base_directory_from(None, Some("".into()), Some("/home/user".into())),
            Some(PathBuf::from("/home/user/.cache/rosetta3/metal-shaders")),
        );
        assert_eq!(base_directory_from(None, None, None), None);
    }

    #[test]
    fn pruning_removes_other_builds_and_nothing_else() {
        let scratch = Scratch::new("prune");
        let version = &CACHE_VERSION[..16];
        let other = "0123456789abcdef";
        for name in [version, other, "notes", "0123456789abcdefff"] {
            std::fs::create_dir_all(scratch.0.join(name)).expect("a version directory");
        }
        std::fs::write(scratch.0.join("README"), b"kept").expect("a file");

        prune(&scratch.0, version);

        assert!(scratch.0.join(version).exists(), "this build's entries");
        assert!(!scratch.0.join(other).exists(), "another build's entries");
        assert!(scratch.0.join("notes").exists(), "not a version directory");
        assert!(scratch.0.join("0123456789abcdefff").exists(), "too long");
        assert!(scratch.0.join("README").exists(), "not a directory");
    }

    /// The version is a digest of this crate's own files, so it must be a
    /// full SHA-256 in hexadecimal or the directory name below is wrong.
    #[test]
    fn the_cache_version_is_a_digest() {
        assert_eq!(CACHE_VERSION.len(), 64);
        assert!(CACHE_VERSION.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
