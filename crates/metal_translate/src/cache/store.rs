//! Locating the cache directory, reading an entry, and the background
//! writer that fills it.
//!
//! Writes are queued and drained off-thread: a cache miss must cost a
//! translation, not a translation plus a disk write.

use super::*;

/// The directory this build's entries live in, or `None` where the cache is
/// disabled or the process has nowhere to put one.
pub(super) fn root() -> Option<&'static Path> {
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

pub(super) fn base_directory() -> Option<PathBuf> {
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
pub(super) fn base_directory_from(
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
pub(super) fn prune(base: &Path, version: &str) {
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
pub(super) fn entry_path(root: &Path, key: &[u8; 32]) -> PathBuf {
    let name = sha256::hex(key);
    root.join(&name[..2]).join(format!("{name}.shader"))
}

pub(super) fn read_entry(path: &Path, key: &[u8; 32]) -> Option<TranslatedLibrary> {
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

pub(super) fn pending_entry(path: &Path, key: &[u8; 32]) -> Option<TranslatedLibrary> {
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
pub(super) fn queue_entry(path: PathBuf, key: [u8; 32], library: &TranslatedLibrary) {
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
