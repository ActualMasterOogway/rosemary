use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use starnix::process::Identity;

use crate::codec::{self, Malformed};
use crate::location;
use crate::protocol::{Checkin, Reply, Request, VERSION};
use crate::registry::Registry;

pub const DEFAULT_LINGER: Duration = Duration::from_secs(5);

pub struct Options {
    pub directory: PathBuf,
    pub linger: Duration,
}

#[derive(Debug)]
pub enum Outcome {
    Finished,
    AlreadyRunning,
}

/// Take this prefix's lock and serve until idle.
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
    let result = serve(listener, options.linger);
    // Unlinked before the lock is released, so a successor never removes a
    // socket another successor has just bound.
    let _ = std::fs::remove_file(&socket);
    drop(lock);
    result.map(|()| Outcome::Finished)
}

struct Shared {
    registry: Mutex<Registry>,
    clients: Mutex<usize>,
    /// When the last client left, while none is connected.
    idle_since: Mutex<Option<Instant>>,
    shutdown: AtomicBool,
    next_connection: AtomicU64,
}

impl Shared {
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Serve `listener` until no client has been connected for `linger`.
pub fn serve(listener: UnixListener, linger: Duration) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let shared = Arc::new(Shared {
        registry: Mutex::new(Registry::new()),
        clients: Mutex::new(0),
        idle_since: Mutex::new(Some(Instant::now())),
        shutdown: AtomicBool::new(false),
        next_connection: AtomicU64::new(1),
    });
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
                {
                    let mut clients = shared.clients.lock().unwrap_or_else(|e| e.into_inner());
                    *clients += 1;
                    *shared.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("client-{id}"))
                    .spawn(move || {
                        let _ = connection(stream, id, &shared);
                        shared.registry().disconnected(id);
                        let mut clients = shared.clients.lock().unwrap_or_else(|e| e.into_inner());
                        *clients -= 1;
                        if *clients == 0 {
                            *shared.idle_since.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(Instant::now());
                        }
                    })?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let idle = *shared.idle_since.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(since) = idle {
                    let wanted = match shared.shutdown.load(Ordering::Relaxed) {
                        true => Duration::ZERO,
                        false => linger,
                    };
                    if since.elapsed() >= wanted {
                        return Ok(());
                    }
                }
                wait_readable(&listener, Duration::from_millis(100));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
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

fn connection(mut stream: UnixStream, id: u64, shared: &Shared) -> io::Result<()> {
    // A host that cannot name the peer's pid leaves it 0, and the claimed one
    // is taken from a peer already known to be this user.
    let (peer_pid, peer_uid) = starnix::socket::peer(&stream)
        .map_or((0, u32::MAX), |peer| (peer.pid.unwrap_or(0), peer.uid));
    if peer_uid != unsafe { libc::getuid() } {
        // The directory is private, so this is somebody who should not have
        // got this far.
        return Ok(());
    }
    let mut checked_in: Option<i32> = None;
    while let Some(frame) = codec::read_frame(&mut stream)? {
        let reply = match codec::decode::<Request>(&frame) {
            Ok(request) => answer(request, id, peer_pid, &mut checked_in, shared),
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
    checked_in: &mut Option<i32>,
    shared: &Shared,
) -> Reply {
    match request {
        Request::Checkin(checkin) => match admit(&checkin, peer_pid) {
            Ok(identity) => {
                let pid = checkin.pid;
                let credentials = shared.registry().checkin(pid, id, checkin, identity);
                *checked_in = Some(pid);
                Reply::CheckedIn {
                    server_pid: starnix::process::me(),
                    credentials,
                }
            }
            Err(why) => Reply::Refused(why),
        },
        Request::Processes => Reply::Processes(shared.registry().processes()),
        Request::Process { pid } => Reply::Process(shared.registry().process(pid)),
        Request::Usage { pid } => Reply::Usage(shared.registry().usage(pid)),
        Request::Threads { pid } => Reply::Threads(shared.registry().threads(pid)),
        Request::ThreadName { tid, name } => match checked_in {
            Some(pid) => {
                shared.registry().name_thread(*pid, tid, name);
                Reply::Done
            }
            None => Reply::Refused("a thread is named by its own process".into()),
        },
        Request::Credentials(change) => match checked_in {
            Some(pid) => Reply::Credentials(shared.registry().change(*pid, &change)),
            None => Reply::Refused("ids are changed by their own process".into()),
        },
        Request::Shutdown => {
            shared.shutdown.store(true, Ordering::Relaxed);
            Reply::Done
        }
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

/// Ask a server already listening at `socket` to exit as soon as it is idle.
pub fn stop(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    codec::write_frame(&mut stream, &codec::encode(&Request::Shutdown))?;
    codec::read_frame(&mut stream)?;
    Ok(())
}
