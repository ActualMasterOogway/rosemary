use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use starnix::process::Identity;

use crate::codec::{self, Malformed};
use crate::location;
use crate::protocol::{Checkin, Reply, Request, VERSION};
use crate::registry::Registry;

pub const DEFAULT_LINGER: Duration = Duration::from_secs(5);

/// How long a guest's check-in waits for launchd to come up. Under the
/// client's ten-second read timeout, so a slow boot is not a dead server.
const BOOT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a process outside launchd's tree has between SIGTERM and SIGKILL.
const OUTSIDER_GRACE: Duration = Duration::from_secs(3);
/// How long launchd has to stop its jobs and exit. Its own `ExitTimeOut`
/// default is 20 seconds per job, and it stops them in parallel.
const LAUNCHD_GRACE: Duration = Duration::from_secs(30);
/// How long what is left once launchd is gone has before SIGKILL.
const DRAIN_GRACE: Duration = Duration::from_secs(3);

/// The prefix's launchd, which the server starts before any guest runs.
pub struct Init {
    /// The host program and arguments that run launchd as a guest.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The host directory launchd starts in: the prefix's root.
    pub current_dir: PathBuf,
    /// Where launchd's console goes; nowhere without one.
    pub console: Option<PathBuf>,
    /// A socket launchd listens on once it is up. Without one, launchd is up
    /// as soon as it is started.
    pub ready: Option<PathBuf>,
}

pub struct Options {
    pub directory: PathBuf,
    pub linger: Duration,
    pub init: Option<Init>,
}

#[derive(Debug)]
pub enum Outcome {
    Finished,
    AlreadyRunning,
}

/// Take this prefix's lock and serve until shut down.
pub fn run(options: &Options) -> io::Result<Outcome> {
    location::prepare(&options.directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(location::lock_in(&options.directory))?;
    // SAFETY: a valid descriptor this function owns for as long as it serves.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(Outcome::AlreadyRunning);
    }
    let socket = location::socket_in(&options.directory);
    // The lock is held, so a socket file still here is a dead server's.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    let result = serve(listener, options.linger, options.init.as_ref());
    // Unlinked before the lock is released, so a successor never removes a
    // socket another successor has just bound.
    let _ = std::fs::remove_file(&socket);
    drop(lock);
    result.map(|()| Outcome::Finished)
}

struct Shared {
    registry: Mutex<Registry>,
    /// Open connections that are not launchd's or its descendants'. A
    /// connection counts until it checks in as one of those.
    users: Mutex<usize>,
    /// When the last user connection closed, while none is open.
    idle_since: Mutex<Option<Instant>>,
    /// Whether launchd is still coming up; user check-ins wait while it is.
    booting: Mutex<bool>,
    booted: Condvar,
    /// A shutdown was asked for.
    shutdown: AtomicBool,
    /// The shutdown sequence has begun: no new process outside launchd's tree
    /// is admitted.
    stopping: AtomicBool,
    next_connection: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        lock(&self.registry)
    }

    fn user_left(&self) {
        let mut users = lock(&self.users);
        *users -= 1;
        if *users == 0 {
            *lock(&self.idle_since) = Some(Instant::now());
        }
    }

    fn finish_boot(&self) {
        *lock(&self.booting) = false;
        self.booted.notify_all();
    }

    /// Block until launchd is up, or has had as long as it gets.
    fn await_boot(&self) {
        let booting = lock(&self.booting);
        let _ = self
            .booted
            .wait_timeout_while(booting, BOOT_TIMEOUT, |booting| *booting);
    }
}

/// Serve `listener` until shut down: asked to, or no process outside
/// launchd's tree has been connected for `linger`.
pub fn serve(listener: UnixListener, linger: Duration, init: Option<&Init>) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let shared = Arc::new(Shared {
        registry: Mutex::new(Registry::new()),
        users: Mutex::new(0),
        idle_since: Mutex::new(Some(Instant::now())),
        booting: Mutex::new(init.is_some()),
        booted: Condvar::new(),
        shutdown: AtomicBool::new(false),
        stopping: AtomicBool::new(false),
        next_connection: AtomicU64::new(1),
    });
    let mut launchd = init.and_then(|init| boot(&shared, init));
    let mut shutdown: Option<Shutdown> = None;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
                *lock(&shared.users) += 1;
                *lock(&shared.idle_since) = None;
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("client-{id}"))
                    .spawn(move || {
                        let mut state = Connection::default();
                        let _ = connection(stream, id, &mut state, &shared);
                        shared.registry().disconnected(id);
                        if !state.system {
                            shared.user_left();
                        }
                    })?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                reap(&shared, &mut launchd);
                match &mut shutdown {
                    Some(sequence) => {
                        if sequence.step(&shared, &mut launchd) {
                            return Ok(());
                        }
                    }
                    None if shared.shutdown.load(Ordering::Relaxed) || idle(&shared, linger) => {
                        shared.stopping.store(true, Ordering::Relaxed);
                        shutdown = Some(Shutdown::begin(&shared));
                    }
                    None => {}
                }
                wait_readable(&listener, Duration::from_millis(100));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn idle(shared: &Shared, linger: Duration) -> bool {
    lock(&shared.idle_since).is_some_and(|since| since.elapsed() >= linger)
}

/// Start launchd, and a watcher that ends the boot once it is listening.
fn boot(shared: &Arc<Shared>, init: &Init) -> Option<Child> {
    let mut command = Command::new(&init.program);
    command
        .args(&init.args)
        .current_dir(&init.current_dir)
        .stdin(Stdio::null());
    let console = init.console.as_ref().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    match console.as_ref().map(|file| file.try_clone()) {
        Some(Ok(copy)) => {
            command.stdout(copy).stderr(console.unwrap());
        }
        _ => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Held across the spawn, so launchd's own check-in cannot be answered
    // before the server knows which process launchd is.
    let mut registry = shared.registry();
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "rosemary server: cannot start launchd ({}): {e}",
                init.program.display()
            );
            drop(registry);
            shared.finish_boot();
            return None;
        }
    };
    let pid = child.id() as i32;
    registry.set_launchd(Some(pid));
    drop(registry);

    let ready = init.ready.clone();
    let watcher = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("boot".into())
        .spawn(move || {
            await_listener(ready.as_deref(), pid);
            watcher.finish_boot();
        });
    if spawned.is_err() {
        shared.finish_boot();
    }
    Some(child)
}

/// Wait until something listens at `ready`, launchd `pid` is gone, or the
/// boot has taken as long as it may.
fn await_listener(ready: Option<&Path>, pid: i32) {
    let Some(ready) = ready else {
        return;
    };
    let deadline = Instant::now() + BOOT_TIMEOUT;
    while Instant::now() < deadline {
        if UnixStream::connect(ready).is_ok() {
            return;
        }
        // SAFETY: a signal-0 probe of a pid.
        if unsafe { libc::kill(pid, 0) } != 0 {
            eprintln!("rosemary server: launchd exited while booting");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "rosemary server: launchd is not listening at {} after {BOOT_TIMEOUT:?}",
        ready.display()
    );
}

/// Collect launchd if it has exited, and forget it.
fn reap(shared: &Shared, launchd: &mut Option<Child>) {
    let Some(child) = launchd else {
        return;
    };
    if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
        *launchd = None;
        shared.registry().set_launchd(None);
        shared.finish_boot();
    }
}

fn signal(pids: &[i32], signal: i32) {
    for &pid in pids {
        // SAFETY: a plain system call on a pid the registry vouched for.
        unsafe { libc::kill(pid, signal) };
    }
}

/// The shutdown sequence, after launchd's own (`launchd_shutdown` and its
/// stray-process sweep): processes outside launchd's tree are ended by the
/// server, launchd is asked to stop its services and exit, and the server
/// leaves once nothing is left.
enum Stage {
    Outsiders,
    Launchd,
    Drain,
}

struct Shutdown {
    stage: Stage,
    since: Instant,
    killed: bool,
}

impl Shutdown {
    fn begin(shared: &Shared) -> Shutdown {
        let outsiders = shared.registry().outsiders();
        signal(&outsiders, libc::SIGTERM);
        Shutdown {
            stage: Stage::Outsiders,
            since: Instant::now(),
            killed: false,
        }
    }

    fn enter(&mut self, stage: Stage) {
        self.stage = stage;
        self.since = Instant::now();
        self.killed = false;
    }

    /// Advance as far as the processes allow. True once the server may exit.
    fn step(&mut self, shared: &Shared, launchd: &mut Option<Child>) -> bool {
        loop {
            match self.stage {
                Stage::Outsiders => {
                    let left = shared.registry().outsiders();
                    if !left.is_empty() {
                        if self.since.elapsed() < OUTSIDER_GRACE {
                            return false;
                        }
                        if !self.killed {
                            self.killed = true;
                            signal(&left, libc::SIGKILL);
                            return false;
                        }
                        // SIGKILLed and still checked in: not worth holding
                        // the prefix up for.
                        if self.since.elapsed() < OUTSIDER_GRACE * 2 {
                            return false;
                        }
                    }
                    self.enter(Stage::Launchd);
                    if let Some(child) = launchd {
                        signal(&[child.id() as i32], libc::SIGTERM);
                    }
                }
                Stage::Launchd => {
                    if let Some(child) = launchd {
                        if self.since.elapsed() >= LAUNCHD_GRACE && !self.killed {
                            self.killed = true;
                            eprintln!(
                                "rosemary server: launchd did not exit in {LAUNCHD_GRACE:?}; killing it"
                            );
                            let _ = child.kill();
                        }
                        return false;
                    }
                    self.enter(Stage::Drain);
                }
                Stage::Drain => {
                    let left = shared.registry().connected();
                    if left.is_empty() {
                        return true;
                    }
                    if self.since.elapsed() < DRAIN_GRACE {
                        return false;
                    }
                    if !self.killed {
                        self.killed = true;
                        signal(&left, libc::SIGKILL);
                        return false;
                    }
                    return self.since.elapsed() >= DRAIN_GRACE * 2;
                }
            }
        }
    }
}

fn wait_readable(listener: &UnixListener, timeout: Duration) {
    let mut fd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`.
    unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as i32) };
}

/// What a connection has established about its peer.
#[derive(Default)]
struct Connection {
    checked_in: Option<i32>,
    /// The peer is launchd or one of its descendants.
    system: bool,
}

fn connection(
    mut stream: UnixStream,
    id: u64,
    state: &mut Connection,
    shared: &Shared,
) -> io::Result<()> {
    // A host that cannot name the peer's pid leaves it 0, and the claimed one
    // is taken from a peer already known to be this user.
    let (peer_pid, peer_uid) = starnix::socket::peer(&stream)
        .map_or((0, u32::MAX), |peer| (peer.pid.unwrap_or(0), peer.uid));
    if peer_uid != unsafe { libc::getuid() } {
        // The directory is private, so this is somebody who should not have
        // got this far.
        return Ok(());
    }
    while let Some(frame) = codec::read_frame(&mut stream)? {
        let reply = match codec::decode::<Request>(&frame) {
            Ok(request) => answer(request, id, peer_pid, state, shared),
            Err(Malformed) => Reply::Refused("malformed request".into()),
        };
        codec::write_frame(&mut stream, &codec::encode(&reply))?;
    }
    Ok(())
}

fn answer(
    request: Request,
    id: u64,
    peer_pid: i32,
    state: &mut Connection,
    shared: &Shared,
) -> Reply {
    match request {
        Request::Checkin(checkin) => match admit(&checkin, peer_pid) {
            Ok(identity) => check_in(checkin, identity, id, state, shared),
            Err(why) => Reply::Refused(why),
        },
        Request::Processes => Reply::Processes(shared.registry().processes()),
        Request::Process { pid } => Reply::Process(shared.registry().process(pid)),
        Request::Usage { pid } => Reply::Usage(shared.registry().usage(pid)),
        Request::Threads { pid } => Reply::Threads(shared.registry().threads(pid)),
        Request::ThreadName { tid, name } => match state.checked_in {
            Some(pid) => {
                shared.registry().name_thread(pid, tid, name);
                Reply::Done
            }
            None => Reply::Refused("a thread is named by its own process".into()),
        },
        Request::Credentials(change) => match state.checked_in {
            Some(pid) => Reply::Credentials(shared.registry().change(pid, &change)),
            None => Reply::Refused("ids are changed by their own process".into()),
        },
        Request::Shutdown => {
            shared.shutdown.store(true, Ordering::Relaxed);
            Reply::Done
        }
    }
}

fn check_in(
    checkin: Checkin,
    identity: Identity,
    id: u64,
    state: &mut Connection,
    shared: &Shared,
) -> Reply {
    let pid = checkin.pid;
    let (credentials, system) = {
        let mut registry = shared.registry();
        let credentials = registry.checkin(pid, id, checkin, identity);
        (credentials, registry.is_system(pid))
    };
    if !system && shared.stopping.load(Ordering::Relaxed) {
        shared.registry().forget(pid);
        return Reply::Refused("the prefix is shutting down".into());
    }
    state.checked_in = Some(pid);
    if system && !state.system {
        state.system = true;
        shared.user_left();
    }
    // A process outside launchd's tree runs once launchd is up, as anything a
    // user starts on a Mac does. launchd's own jobs are part of that boot.
    if !system {
        shared.await_boot();
    }
    Reply::CheckedIn {
        server_pid: starnix::process::me(),
        credentials,
        launchd: shared.registry().launchd().unwrap_or(0),
    }
}

fn admit(checkin: &Checkin, peer_pid: i32) -> Result<Identity, String> {
    if checkin.version != VERSION {
        return Err(format!(
            "protocol version {} is not this server's {VERSION}",
            checkin.version
        ));
    }
    if peer_pid != 0 && checkin.pid != peer_pid {
        return Err(format!(
            "pid {} checked in from pid {peer_pid}",
            checkin.pid
        ));
    }
    starnix::process::identity(checkin.pid)
        .ok_or_else(|| format!("pid {} is not running", checkin.pid))
}

/// Ask a server already listening at `socket` to shut its prefix down.
pub fn stop(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    codec::write_frame(&mut stream, &codec::encode(&Request::Shutdown))?;
    codec::read_frame(&mut stream)?;
    Ok(())
}
