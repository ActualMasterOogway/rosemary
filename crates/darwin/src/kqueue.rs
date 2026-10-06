#![cfg(not(target_os = "macos"))]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use static_init::dynamic;

/// `struct kevent`, which the guest both fills in and reads back.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Kevent {
    pub ident: usize,
    pub filter: i16,
    pub flags: u16,
    pub fflags: u32,
    pub data: isize,
    pub udata: usize,
}

const _: () = {
    assert!(size_of::<Kevent>() == 32);
    assert!(std::mem::offset_of!(Kevent, filter) == 8);
    assert!(std::mem::offset_of!(Kevent, data) == 16);
    assert!(std::mem::offset_of!(Kevent, udata) == 24);
};

const EVFILT_READ: i16 = -1;
const EVFILT_WRITE: i16 = -2;
const EVFILT_SIGNAL: i16 = -6;
const EVFILT_USER: i16 = -10;

const EV_ADD: u16 = 0x0001;
const EV_DELETE: u16 = 0x0002;
const EV_ENABLE: u16 = 0x0004;
const EV_DISABLE: u16 = 0x0008;
const EV_ONESHOT: u16 = 0x0010;
const EV_RECEIPT: u16 = 0x0040;
const EV_DISPATCH: u16 = 0x0080;
const EV_ERROR: u16 = 0x4000;
const EV_EOF: u16 = 0x8000;

/// `NOTE_TRIGGER`, the one `fflags` bit `EVFILT_USER` acts on.
const NOTE_TRIGGER: u32 = 0x0100_0000;

/// What one `EV_ADD` recorded.
#[derive(Clone, Copy)]
struct Registration {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    udata: usize,
    enabled: bool,
    /// `EVFILT_SIGNAL`: how many deliveries of the signal were already
    /// reported, or happened before the registration.
    seen: u64,
}

impl Registration {
    /// The event to hand back when this registration fires.
    ///
    /// `NOTE_TRIGGER` is cleared: it is the caller's *request*, and a caller
    /// that read it back in a delivered event would see its own trigger as a
    /// property of the event.
    fn fired(&self) -> Kevent {
        Kevent {
            ident: self.ident,
            filter: self.filter,
            flags: 0,
            fflags: self.fflags & !NOTE_TRIGGER,
            data: 0,
            udata: self.udata,
        }
    }
}

struct Queue<B> {
    registrations: HashMap<(usize, i16), Registration>,
    /// Triggered `EVFILT_USER` idents, in the order they were triggered, and
    /// the set form of the same thing - a second trigger before the first is
    /// delivered is one event, not two.
    triggered: VecDeque<usize>,
    triggered_set: HashSet<usize>,
    backend: Arc<B>,
}

impl<B> Drop for Queue<B> {
    fn drop(&mut self) {
        for registration in self.registrations.values() {
            if registration.filter == EVFILT_SIGNAL {
                let _ = crate::signal::observe(registration.ident as i32, false);
            }
        }
    }
}

impl<B> Queue<B> {
    fn new(backend: B) -> Self {
        Self {
            registrations: HashMap::new(),
            triggered: VecDeque::new(),
            triggered_set: HashSet::new(),
            backend: Arc::new(backend),
        }
    }
}

/// A ready descriptor reported by one concrete host readiness mechanism.
#[derive(Clone, Copy)]
struct Ready {
    fd: c_int,
    revents: i16,
    wake: bool,
}

/// The host-specific part of a kqueue. `Queue<B>` is monomorphized with the
/// selected backend; this is deliberately not a trait object or a run-time
/// host capability choice.
trait ReadinessBackend: Sized {
    fn new() -> Result<(Self, c_int), c_int>;
    fn wake(&self);
    /// The descriptor `wake` writes, for a signal handler to write.
    fn wake_descriptor(&self) -> c_int;
    fn drain_wake(&self);
    fn update(&self, fd: c_int, before: i16, after: i16) -> Result<(), c_int>;
    fn wait(
        &self,
        queue: c_int,
        registrations: &HashMap<(usize, i16), Registration>,
        capacity: usize,
        timeout: c_int,
    ) -> Result<Vec<Ready>, c_int>;
}

/// The portable reconstruction of a readiness set. It remains the complete
/// implementation outside Linux.
#[cfg(not(target_os = "linux"))]
struct PollBackend {
    wake: c_int,
}

#[cfg(not(target_os = "linux"))]
impl ReadinessBackend for PollBackend {
    fn new() -> Result<(Self, c_int), c_int> {
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            Err(crate::errno::get())
        } else {
            Ok((Self { wake }, wake))
        }
    }

    fn wake(&self) {
        wake_fd(self.wake)
    }
    fn wake_descriptor(&self) -> c_int {
        self.wake
    }
    fn drain_wake(&self) {
        drain_fd(self.wake)
    }
    fn update(&self, _: c_int, _: i16, _: i16) -> Result<(), c_int> {
        Ok(())
    }

    fn wait(
        &self,
        queue: c_int,
        registrations: &HashMap<(usize, i16), Registration>,
        _: usize,
        timeout: c_int,
    ) -> Result<Vec<Ready>, c_int> {
        let mut watched = poll_set(queue, registrations);
        let ready =
            unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            return Err(crate::errno::get());
        }
        Ok(watched
            .into_iter()
            .filter(|entry| entry.revents != 0)
            .map(|entry| Ready {
                fd: entry.fd,
                revents: entry.revents,
                wake: entry.fd == queue,
            })
            .collect())
    }
}

#[cfg(target_os = "linux")]
struct EpollBackend {
    epoll: c_int,
    wake: c_int,
    poll_only: Mutex<HashSet<c_int>>,
}

#[cfg(target_os = "linux")]
impl Drop for EpollBackend {
    fn drop(&mut self) {
        // This is the private wakeup descriptor, never the guest-visible
        // kqueue descriptor (which the guest owns and may already have closed).
        forget_waker(self.wake);
        unsafe { libc::close(self.wake) };
    }
}

#[cfg(target_os = "linux")]
impl EpollBackend {
    const WAKE_TOKEN: u64 = u64::MAX;

    fn event(fd: c_int, interest: i16) -> libc::epoll_event {
        libc::epoll_event {
            events: ((interest & libc::POLLIN != 0) as u32 * libc::EPOLLIN as u32)
                | ((interest & libc::POLLOUT != 0) as u32 * libc::EPOLLOUT as u32),
            u64: fd as u64,
        }
    }

    fn ctl(epoll: c_int, op: c_int, fd: c_int, interest: i16) -> Result<(), c_int> {
        let mut event = Self::event(fd, interest);
        if unsafe { libc::epoll_ctl(epoll, op, fd, &raw mut event) } < 0 {
            Err(crate::errno::get())
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
impl ReadinessBackend for EpollBackend {
    fn new() -> Result<(Self, c_int), c_int> {
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            return Err(crate::errno::get());
        }
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            unsafe { libc::close(epoll) };
            return Err(crate::errno::get());
        }
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: Self::WAKE_TOKEN,
        };
        if unsafe { libc::epoll_ctl(epoll, libc::EPOLL_CTL_ADD, wake, &raw mut event) } < 0 {
            let errno = crate::errno::get();
            unsafe {
                libc::close(wake);
                libc::close(epoll)
            };
            return Err(errno);
        }
        Ok((
            Self {
                epoll,
                wake,
                poll_only: Mutex::new(HashSet::new()),
            },
            epoll,
        ))
    }

    fn wake(&self) {
        wake_fd(self.wake)
    }
    fn wake_descriptor(&self) -> c_int {
        self.wake
    }
    fn drain_wake(&self) {
        drain_fd(self.wake)
    }

    fn update(&self, fd: c_int, before: i16, after: i16) -> Result<(), c_int> {
        if before == after {
            return Ok(());
        }
        let mut poll_only = self.poll_only.lock().unwrap_or_else(|e| e.into_inner());
        if poll_only.contains(&fd) {
            if after == 0 {
                poll_only.remove(&fd);
            }
            return Ok(());
        }
        let result = match (before != 0, after != 0) {
            (false, true) => Self::ctl(self.epoll, libc::EPOLL_CTL_ADD, fd, after),
            (true, true) => Self::ctl(self.epoll, libc::EPOLL_CTL_MOD, fd, after),
            (true, false) => Self::ctl(self.epoll, libc::EPOLL_CTL_DEL, fd, 0),
            (false, false) => Ok(()),
        };
        match result {
            Err(errno) if before == 0 && errno == libc::EPERM => {
                poll_only.insert(fd);
                Ok(())
            }
            // Closing a watched FD removes it from epoll. Keep Darwin's
            // deletion semantics even if the host has already done that work.
            Err(errno) if after == 0 && matches!(errno, libc::ENOENT | libc::EBADF) => Ok(()),
            other => other,
        }
    }

    fn wait(
        &self,
        queue: c_int,
        registrations: &HashMap<(usize, i16), Registration>,
        capacity: usize,
        timeout: c_int,
    ) -> Result<Vec<Ready>, c_int> {
        let mut result = Vec::new();
        let poll_only = self
            .poll_only
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if poll_only.is_empty() {
            return self.wait_epoll(queue, capacity, timeout);
        }
        let mut watched = Vec::with_capacity(poll_only.len() + 1);
        watched.push(libc::pollfd {
            fd: queue,
            events: libc::POLLIN,
            revents: 0,
        });
        for fd in &poll_only {
            let interest = interest_for(registrations, *fd);
            if interest != 0 {
                watched.push(libc::pollfd {
                    fd: *fd,
                    events: interest,
                    revents: 0,
                });
            }
        }
        if unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, timeout) } < 0 {
            return Err(crate::errno::get());
        }
        for entry in watched.iter().skip(1) {
            if entry.revents != 0 {
                result.push(Ready {
                    fd: entry.fd,
                    revents: entry.revents,
                    wake: false,
                });
            }
        }
        if watched[0].revents != 0 {
            result.extend(self.wait_epoll(
                queue,
                capacity.saturating_sub(result.len()).max(1),
                0,
            )?);
        }
        Ok(result)
    }
}

#[cfg(target_os = "linux")]
impl EpollBackend {
    fn wait_epoll(
        &self,
        queue: c_int,
        capacity: usize,
        timeout: c_int,
    ) -> Result<Vec<Ready>, c_int> {
        let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; capacity.max(1)];
        let count =
            unsafe { libc::epoll_wait(queue, events.as_mut_ptr(), events.len() as c_int, timeout) };
        if count < 0 {
            return Err(crate::errno::get());
        }
        Ok(events
            .into_iter()
            .take(count as usize)
            .map(|event| Ready {
                fd: event.u64 as c_int,
                revents: epoll_revents(event.events),
                wake: event.u64 == Self::WAKE_TOKEN,
            })
            .collect())
    }
}

#[cfg(target_os = "linux")]
fn epoll_revents(events: u32) -> i16 {
    ((events & libc::EPOLLIN as u32 != 0) as i16 * libc::POLLIN)
        | ((events & libc::EPOLLOUT as u32 != 0) as i16 * libc::POLLOUT)
        | ((events & libc::EPOLLERR as u32 != 0) as i16 * libc::POLLERR)
        | ((events & libc::EPOLLHUP as u32 != 0) as i16 * libc::POLLHUP)
}

#[cfg(target_os = "linux")]
type SelectedBackend = EpollBackend;
#[cfg(not(target_os = "linux"))]
type SelectedBackend = PollBackend;

fn queues() -> &'static Mutex<HashMap<c_int, Arc<Mutex<Queue<SelectedBackend>>>>> {
    #[dynamic]
    static QUEUES: Mutex<HashMap<c_int, Arc<Mutex<Queue<SelectedBackend>>>>> = Default::default();
    &QUEUES
}

fn queue_of(fd: c_int) -> Option<Arc<Mutex<Queue<SelectedBackend>>>> {
    queues()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&fd)
        .cloned()
}

/// `int kqueue(void)`
pub extern "C" fn kqueue() -> c_int {
    let Ok((backend, fd)) = SelectedBackend::new() else {
        return -1;
    };
    // Whatever this number held before is gone with the descriptor it named;
    // see the module note on `close`.
    queues()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(fd, Arc::new(Mutex::new(Queue::new(backend))));
    fd
}

/// Wake a `kevent` blocked on `queue`.
fn wake_fd(queue: c_int) {
    let one: u64 = 1;
    // SAFETY: an eventfd this module created, and eight bytes to write.
    unsafe { libc::write(queue, (&raw const one).cast::<c_void>(), size_of::<u64>()) };
}

/// The wakeup descriptors of queues that watch signals. A signal handler may
/// take no lock, so this is a fixed table it can scan.
const WAKERS: usize = 64;
static WAKER_FDS: [AtomicI32; WAKERS] = [const { AtomicI32::new(-1) }; WAKERS];

fn register_waker(fd: c_int) -> Result<(), c_int> {
    if WAKER_FDS
        .iter()
        .any(|slot| slot.load(Ordering::Acquire) == fd)
    {
        return Ok(());
    }
    for slot in &WAKER_FDS {
        if slot
            .compare_exchange(-1, fd, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(libc::ENOMEM)
}

/// Before the descriptor closes, so its number is not written once it names
/// something else.
fn forget_waker(fd: c_int) {
    for slot in &WAKER_FDS {
        let _ = slot.compare_exchange(fd, -1, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// A watched signal arrived: wake every queue that watches signals, and let
/// each decide whether it is theirs. Async-signal-safe.
pub(crate) fn signalled() {
    for slot in &WAKER_FDS {
        let fd = slot.load(Ordering::Acquire);
        if fd >= 0 {
            wake_fd(fd);
        }
    }
}

/// Drain the wakeup channel, which is level-triggered and would otherwise
/// return from every later `poll` at once.
fn drain_fd(queue: c_int) {
    let mut value: u64 = 0;
    // SAFETY: as [`wake`]; the descriptor is non-blocking, so this ends with
    // `EAGAIN` rather than waiting.
    while unsafe { libc::read(queue, (&raw mut value).cast::<c_void>(), size_of::<u64>()) } > 0 {}
}

/// Apply one change, answering with the `errno` it failed with.
///
/// The number is **Darwin's**: it reaches the guest as an `EV_ERROR` event's
/// `data`, which is a value rather than the `errno` slot, so nothing later
/// translates it.
fn apply<B: ReadinessBackend>(state: &mut Queue<B>, change: &Kevent) -> Result<(), c_int> {
    if !matches!(
        change.filter,
        EVFILT_READ | EVFILT_WRITE | EVFILT_USER | EVFILT_SIGNAL
    ) {
        return Err(crate::errno::from_host(libc::EINVAL));
    }
    let key = (change.ident, change.filter);

    let mut registrations = state.registrations.clone();
    let mut triggered = state.triggered.clone();
    let mut triggered_set = state.triggered_set.clone();

    if change.flags & EV_DELETE != 0 {
        if registrations.remove(&key).is_none() {
            return Err(crate::errno::from_host(libc::ENOENT));
        }
        triggered.retain(|ident| *ident != change.ident);
        triggered_set.remove(&change.ident);
        sync_interest(state, &registrations, change.filter, change.ident)?;
        state.registrations = registrations;
        state.triggered = triggered;
        state.triggered_set = triggered_set;
        if change.filter == EVFILT_SIGNAL {
            let _ = crate::signal::observe(change.ident as i32, false);
        }
        return Ok(());
    }

    if change.flags & EV_ADD != 0 {
        // A readiness filter names a descriptor, and one that is not open is
        // `EBADF` at registration rather than at the first `poll` - which is
        // where BSD reports it, and the only place a caller looks.
        let mut seen = 0;
        if change.filter == EVFILT_SIGNAL {
            let darwin =
                i32::try_from(change.ident).map_err(|_| crate::errno::from_host(libc::EINVAL))?;
            seen = match registrations.get(&key) {
                Some(existing) => existing.seen,
                None => {
                    register_waker(state.backend.wake_descriptor())
                        .map_err(crate::errno::from_host)?;
                    crate::signal::observe(darwin, true).map_err(crate::errno::from_host)?;
                    crate::signal::delivered(darwin)
                }
            };
        } else if change.filter != EVFILT_USER {
            let fd =
                c_int::try_from(change.ident).map_err(|_| crate::errno::from_host(libc::EBADF))?;
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(crate::errno::from_host(crate::errno::get()));
            }
        }
        registrations.insert(
            key,
            Registration {
                ident: change.ident,
                filter: change.filter,
                flags: change.flags,
                fflags: change.fflags,
                udata: change.udata,
                enabled: change.flags & EV_DISABLE == 0,
                seen,
            },
        );
    } else {
        let Some(registration) = registrations.get_mut(&key) else {
            return Err(crate::errno::from_host(libc::ENOENT));
        };
        if change.flags & EV_ENABLE != 0 {
            registration.enabled = true;
        }
        if change.flags & EV_DISABLE != 0 {
            registration.enabled = false;
        }
    }

    if change.filter == EVFILT_USER && change.fflags & NOTE_TRIGGER != 0 {
        let enabled = registrations
            .get(&key)
            .is_some_and(|registration| registration.enabled);
        if enabled && triggered_set.insert(change.ident) {
            triggered.push_back(change.ident);
        }
    }
    sync_interest(state, &registrations, change.filter, change.ident)?;
    let woke = change.filter == EVFILT_USER
        && change.fflags & NOTE_TRIGGER != 0
        && triggered != state.triggered;
    state.registrations = registrations;
    state.triggered = triggered;
    state.triggered_set = triggered_set;
    if woke {
        state.backend.wake();
    }
    Ok(())
}

fn interest_for(registrations: &HashMap<(usize, i16), Registration>, fd: c_int) -> i16 {
    registrations
        .values()
        .filter(|registration| registration.enabled && registration.ident == fd as usize)
        .fold(0, |interest, registration| {
            interest
                | match registration.filter {
                    EVFILT_READ => libc::POLLIN,
                    EVFILT_WRITE => libc::POLLOUT,
                    _ => 0,
                }
        })
}

fn sync_interest<B: ReadinessBackend>(
    state: &mut Queue<B>,
    registrations: &HashMap<(usize, i16), Registration>,
    filter: i16,
    ident: usize,
) -> Result<(), c_int> {
    if matches!(filter, EVFILT_USER | EVFILT_SIGNAL) {
        return Ok(());
    }
    let fd = c_int::try_from(ident).map_err(|_| crate::errno::from_host(libc::EBADF))?;
    state
        .backend
        .update(
            fd,
            interest_for(&state.registrations, fd),
            interest_for(registrations, fd),
        )
        .map_err(crate::errno::from_host)
}

/// The `poll` timeout in milliseconds, or -1 for "wait forever".
///
/// Rounded **up**: a caller asking for half a millisecond asked to wait, and a
/// zero here would make its wait a poll.
fn timeout_ms(timeout: *const libc::timespec) -> Result<c_int, c_int> {
    // SAFETY: null or the caller's `struct timespec`, whose layout is the same
    // two 64-bit words on both platforms.
    let Some(timeout) = (unsafe { timeout.as_ref() }) else {
        return Ok(-1);
    };
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(crate::errno::from_host(libc::EINVAL));
    }
    let ms = (timeout.tv_sec as i64)
        .saturating_mul(1000)
        .saturating_add((timeout.tv_nsec as i64 + 999_999) / 1_000_000);
    Ok(ms.min(c_int::MAX as i64) as c_int)
}

/// The descriptors to wait on: the wakeup channel, and every enabled readiness
/// registration folded together by descriptor.
#[cfg(not(target_os = "linux"))]
fn poll_set(
    queue: c_int,
    registrations: &HashMap<(usize, i16), Registration>,
) -> Vec<libc::pollfd> {
    let mut interest: HashMap<c_int, i16> = HashMap::new();
    interest.insert(queue, libc::POLLIN);
    for registration in registrations.values() {
        if !registration.enabled || matches!(registration.filter, EVFILT_USER | EVFILT_SIGNAL) {
            continue;
        }
        let Ok(fd) = c_int::try_from(registration.ident) else {
            continue;
        };
        *interest.entry(fd).or_default() |= match registration.filter {
            EVFILT_READ => libc::POLLIN,
            _ => libc::POLLOUT,
        };
    }
    interest
        .into_iter()
        .map(|(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect()
}

/// How many bytes can be read from `fd`, which is what `EVFILT_READ` reports as
/// its `data` and what a caller sizes its next read from.
fn readable_bytes(fd: c_int) -> isize {
    let mut bytes: c_int = 0;
    // SAFETY: `FIONREAD` writes one `int` through the pointer it is given.
    match unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut bytes) } {
        0 => bytes as isize,
        _ => 0,
    }
}

/// `int kevent(int kq, const struct kevent *changelist, int nchanges,
/// struct kevent *eventlist, int nevents, const struct timespec *timeout)`
///
/// # Safety
///
/// `changes` holds `nchanges` events and `events` has room for `nevents`.
pub unsafe extern "C" fn kevent(
    queue: c_int,
    changes: *const Kevent,
    nchanges: c_int,
    events: *mut Kevent,
    nevents: c_int,
    timeout: *const libc::timespec,
) -> c_int {
    if nchanges < 0
        || nevents < 0
        || (nchanges != 0 && changes.is_null())
        || (nevents != 0 && events.is_null())
    {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    let Some(state) = queue_of(queue) else {
        crate::errno::set(libc::EBADF);
        return -1;
    };
    let capacity = nevents as usize;
    let mut out: Vec<Kevent> = Vec::with_capacity(capacity);

    let changes = match nchanges {
        0 => &[][..],
        // SAFETY: the caller states there are `nchanges` of them.
        n => unsafe { std::slice::from_raw_parts(changes, n as usize) },
    };
    {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        for change in changes {
            let failure = apply(&mut state, change).err();
            // `EV_RECEIPT` asks for the *outcome of every change* rather than
            // for readiness, and it asks for it whether or not the change
            // failed - which is how a caller applies a batch and finds out
            // which one of them the kernel rejected.
            if change.flags & EV_RECEIPT != 0 {
                if out.len() < capacity {
                    out.push(Kevent {
                        flags: EV_ERROR,
                        data: failure.unwrap_or(0) as isize,
                        ..*change
                    });
                }
                continue;
            }
            if let Some(errno) = failure {
                crate::errno::set(crate::errno::to_host(errno));
                return -1;
            }
        }
    }
    if capacity == 0 {
        return 0;
    }
    if !out.is_empty() {
        // SAFETY: the caller states there is room for `nevents`.
        unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), events, out.len()) };
        return out.len() as c_int;
    }

    let triggered = {
        let state = state.lock().unwrap_or_else(|e| e.into_inner());
        !state.triggered.is_empty() || signals_pending(&state.registrations)
    };
    // Something is already waiting, so this is a collection rather than a wait.
    let wait = match triggered {
        true => 0,
        false => match timeout_ms(timeout) {
            Ok(ms) => ms,
            Err(errno) => {
                crate::errno::set(crate::errno::to_host(errno));
                return -1;
            }
        },
    };
    let ready = {
        let state = state.lock().unwrap_or_else(|e| e.into_inner());
        let backend = Arc::clone(&state.backend);
        let registrations = state.registrations.clone();
        drop(state);
        match backend.wait(queue, &registrations, capacity, wait) {
            Ok(ready) => ready,
            Err(errno) => {
                crate::errno::set(errno);
                return -1;
            }
        }
    };

    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    if ready.iter().any(|entry| entry.wake) {
        state.backend.drain_wake();
    }

    // The user events first: a program triggers one to be told to stop, and a
    // busy descriptor must not keep that behind it.
    while out.len() < capacity {
        let Some(ident) = state.triggered.pop_front() else {
            break;
        };
        state.triggered_set.remove(&ident);
        if let Some(registration) = state.registrations.get(&(ident, EVFILT_USER))
            && registration.enabled
        {
            out.push(registration.fired());
        }
    }

    let mut signals: Vec<(usize, i16)> = state
        .registrations
        .keys()
        .filter(|key| key.1 == EVFILT_SIGNAL)
        .copied()
        .collect();
    signals.sort_unstable();
    for key in signals {
        if out.len() == capacity {
            break;
        }
        let Some(registration) = state.registrations.get_mut(&key) else {
            continue;
        };
        let count = crate::signal::delivered(registration.ident as i32);
        if !registration.enabled || count <= registration.seen {
            continue;
        }
        let mut event = registration.fired();
        event.data = (count - registration.seen) as isize;
        registration.seen = count;
        out.push(event);
        let (ident, flags) = (registration.ident, registration.flags);
        if flags & EV_ONESHOT != 0 {
            state.registrations.remove(&key);
            let _ = crate::signal::observe(ident as i32, false);
        } else if flags & EV_DISPATCH != 0 {
            registration.enabled = false;
        }
    }

    for entry in ready {
        if out.len() == capacity || entry.wake || entry.revents == 0 {
            continue;
        }
        let firing: Vec<((usize, i16), Registration)> = state
            .registrations
            .iter()
            .filter(|(_, r)| r.enabled && r.ident == entry.fd as usize)
            .map(|(key, r)| (*key, *r))
            .collect();
        for (key, registration) in firing {
            if out.len() == capacity {
                break;
            }
            let wanted = match registration.filter {
                EVFILT_READ => libc::POLLIN,
                EVFILT_WRITE => libc::POLLOUT,
                _ => continue,
            };
            // The three conditions `poll` reports whether or not they were
            // asked for. A hangup or an error is readiness as far as kqueue is
            // concerned - the read that follows is what reports it.
            if entry.revents & (wanted | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0 {
                continue;
            }
            let mut event = registration.fired();
            if entry.revents & libc::POLLNVAL != 0 {
                event.flags |= EV_ERROR;
                event.data = crate::errno::from_host(libc::EBADF) as isize;
            } else {
                if entry.revents & libc::POLLHUP != 0 {
                    event.flags |= EV_EOF;
                }
                if registration.filter == EVFILT_READ {
                    event.data = readable_bytes(entry.fd);
                }
            }
            out.push(event);
            // `EV_ONESHOT` deletes the registration once it has fired and
            // `EV_DISPATCH` disables it until it is enabled again. Both are how
            // a caller avoids being handed the same readiness on every turn.
            if registration.flags & EV_ONESHOT != 0 {
                let before = interest_for(&state.registrations, entry.fd);
                state.registrations.remove(&key);
                let after = interest_for(&state.registrations, entry.fd);
                let _ = state.backend.update(entry.fd, before, after);
            } else if registration.flags & EV_DISPATCH != 0 {
                let before = interest_for(&state.registrations, entry.fd);
                if let Some(registration) = state.registrations.get_mut(&key) {
                    registration.enabled = false;
                }
                let after = interest_for(&state.registrations, entry.fd);
                let _ = state.backend.update(entry.fd, before, after);
            }
        }
    }
    // SAFETY: the caller states there is room for `nevents`, and `out` never
    // grew past that.
    unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), events, out.len()) };
    out.len() as c_int
}

/// Whether an enabled signal registration has deliveries it has not reported.
fn signals_pending(registrations: &HashMap<(usize, i16), Registration>) -> bool {
    registrations.values().any(|r| {
        r.filter == EVFILT_SIGNAL && r.enabled && crate::signal::delivered(r.ident as i32) > r.seen
    })
}

/// `kevent64` and the filters `poll` cannot answer; see the module note.
pub const REFUSED: &[&str] = &["kevent64", "kevent_qos"];

exports::exports! {
    functions {
        "kqueue" => kqueue,
        "kevent" => kevent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero timeout, which makes `kevent` a collection rather than a wait.
    static NO_WAIT: libc::timespec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    /// A pipe, as the two descriptors a readiness test needs.
    fn pipe() -> (c_int, c_int) {
        let mut ends = [0 as c_int; 2];
        // SAFETY: a live array of two `int`.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        (ends[0], ends[1])
    }

    /// The case the client needs: a descriptor is registered, nothing is ready,
    /// a byte arrives, and the *same* registration reports how much there is.
    #[test]
    fn a_readable_descriptor_is_reported_with_the_bytes_waiting() {
        let queue = kqueue();
        assert!(queue >= 0);
        let (read_end, write_end) = pipe();
        let add = Kevent {
            ident: read_end as usize,
            filter: EVFILT_READ,
            flags: EV_ADD,
            udata: 0xbeef,
            ..Kevent::default()
        };
        let mut got = [Kevent::default(); 4];
        // SAFETY: one change and room for four events.
        unsafe {
            assert_eq!(
                kevent(
                    queue,
                    &raw const add,
                    1,
                    got.as_mut_ptr(),
                    4,
                    &raw const NO_WAIT
                ),
                0,
                "nothing is ready yet"
            );
            assert_eq!(libc::write(write_end, c"hello".as_ptr().cast(), 5), 5);
            assert_eq!(
                kevent(
                    queue,
                    std::ptr::null(),
                    0,
                    got.as_mut_ptr(),
                    4,
                    &raw const NO_WAIT
                ),
                1
            );
            libc::close(read_end);
            libc::close(write_end);
            libc::close(queue);
        }
        assert_eq!(got[0].ident, read_end as usize);
        assert_eq!(got[0].filter, EVFILT_READ);
        assert_eq!(got[0].udata, 0xbeef);
        assert_eq!(got[0].data, 5, "five bytes are waiting");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_queue_descriptor_is_readable_when_a_watched_descriptor_is_ready() {
        let queue = kqueue();
        let (read_end, write_end) = pipe();
        let add = Kevent {
            ident: read_end as usize,
            filter: EVFILT_READ,
            flags: EV_ADD,
            ..Kevent::default()
        };
        let mut got = [Kevent::default(); 1];
        // SAFETY: the descriptors and event buffers belong to this test.
        unsafe {
            assert_eq!(
                kevent(
                    queue,
                    &raw const add,
                    1,
                    got.as_mut_ptr(),
                    1,
                    &raw const NO_WAIT
                ),
                0
            );
            assert_eq!(libc::write(write_end, c"x".as_ptr().cast(), 1), 1);
            let mut watched = libc::pollfd {
                fd: queue,
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(libc::poll(&raw mut watched, 1, 0), 1);
            assert_ne!(watched.revents & libc::POLLIN, 0);
            assert_eq!(
                kevent(
                    queue,
                    std::ptr::null(),
                    0,
                    got.as_mut_ptr(),
                    1,
                    &raw const NO_WAIT
                ),
                1
            );
            libc::close(read_end);
            libc::close(write_end);
            libc::close(queue);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_epoll_keeps_regular_files_on_the_poll_compatibility_path() {
        use std::os::fd::AsRawFd;

        let queue = kqueue();
        let file = std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/src/kqueue.rs"))
            .expect("this Rust source file");
        let add = Kevent {
            ident: file.as_raw_fd() as usize,
            filter: EVFILT_READ,
            flags: EV_ADD,
            ..Kevent::default()
        };
        let mut got = [Kevent::default(); 1];
        // SAFETY: one valid change and one event slot.
        let n = unsafe {
            kevent(
                queue,
                &raw const add,
                1,
                got.as_mut_ptr(),
                1,
                &raw const NO_WAIT,
            )
        };
        assert_eq!(n, 1);
        assert_eq!(got[0].ident, file.as_raw_fd() as usize);
        // SAFETY: this test's queue descriptor.
        unsafe { libc::close(queue) };
    }

    /// `EVFILT_USER` is what a program wakes its own I/O thread with, and the
    /// trigger has to survive being set from another thread while `kevent` is
    /// blocked with no timeout.
    #[test]
    fn a_user_event_wakes_a_wait_that_had_no_timeout() {
        let queue = kqueue();
        let add = Kevent {
            ident: 42,
            filter: EVFILT_USER,
            flags: EV_ADD | EV_ENABLE,
            udata: 7,
            ..Kevent::default()
        };
        let mut got = [Kevent::default(); 1];
        // SAFETY: one change and room for one event.
        unsafe {
            assert_eq!(
                kevent(
                    queue,
                    &raw const add,
                    1,
                    got.as_mut_ptr(),
                    1,
                    &raw const NO_WAIT
                ),
                0
            );
        }
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let fire = Kevent {
                ident: 42,
                filter: EVFILT_USER,
                fflags: NOTE_TRIGGER,
                ..Kevent::default()
            };
            // SAFETY: one change and no room for events, which applies it and
            // returns.
            unsafe {
                kevent(
                    queue,
                    &raw const fire,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            }
        });
        // SAFETY: no changes, room for one event, and a null timeout - which is
        // the wait the trigger has to break.
        let n = unsafe {
            kevent(
                queue,
                std::ptr::null(),
                0,
                got.as_mut_ptr(),
                1,
                std::ptr::null(),
            )
        };
        assert_eq!(trigger.join().expect("the trigger thread"), 0);
        assert_eq!(n, 1);
        assert_eq!(got[0].ident, 42);
        assert_eq!(got[0].udata, 7);
        assert_eq!(
            got[0].fflags & NOTE_TRIGGER,
            0,
            "the caller's request is not handed back as a property of the event"
        );
        // SAFETY: this test's own descriptor.
        unsafe { libc::close(queue) };
    }

    /// Applies `change` and collects what is ready, without waiting.
    fn collect(queue: c_int, change: Option<&Kevent>) -> (c_int, Kevent) {
        let mut got = [Kevent::default(); 1];
        // SAFETY: at most one change, room for one event, and no wait.
        let n = unsafe {
            kevent(
                queue,
                change.map_or(std::ptr::null(), |c| c as *const Kevent),
                change.is_some() as c_int,
                got.as_mut_ptr(),
                1,
                &raw const NO_WAIT,
            )
        };
        (n, got[0])
    }

    /// A signal whose default action is to do nothing is still reported, with
    /// the number of deliveries since the last report.
    #[test]
    fn a_signal_event_counts_deliveries_of_an_ignored_by_default_signal() {
        use crate::signal::signum::SIGWINCH;
        let queue = kqueue();
        let add = Kevent {
            ident: SIGWINCH as usize,
            filter: EVFILT_SIGNAL,
            flags: EV_ADD | EV_ENABLE,
            udata: 9,
            ..Kevent::default()
        };
        assert_eq!(collect(queue, Some(&add)).0, 0, "nothing delivered yet");
        let host = crate::signal::signal_to_host(SIGWINCH).expect("a host signal");
        // SAFETY: a signal this process takes by default as a no-op.
        unsafe {
            libc::raise(host);
            libc::raise(host);
        }
        let (n, event) = collect(queue, None);
        assert_eq!(n, 1);
        assert_eq!(event.ident, SIGWINCH as usize);
        assert_eq!(event.filter, EVFILT_SIGNAL);
        assert_eq!(event.data, 2);
        assert_eq!(event.udata, 9);
        assert_eq!(collect(queue, None).0, 0, "reported once");
        // SAFETY: this test's own descriptor.
        unsafe { libc::close(queue) };
    }

    /// `SIG_IGN` does not hide a signal from the queue, and removing the last
    /// registration puts the disposition back, so the signal stays ignored.
    #[test]
    fn a_signal_event_sees_an_ignored_signal_and_restores_the_disposition() {
        use crate::signal::signum::SIGUSR2;
        let host = crate::signal::signal_to_host(SIGUSR2).expect("a host signal");
        // SAFETY: SIG_IGN is a value, not a function.
        unsafe { crate::signal::signal(SIGUSR2, 1) };
        let queue = kqueue();
        let mut change = Kevent {
            ident: SIGUSR2 as usize,
            filter: EVFILT_SIGNAL,
            flags: EV_ADD,
            ..Kevent::default()
        };
        assert_eq!(collect(queue, Some(&change)).0, 0);
        // SAFETY: ignored, so this returns.
        unsafe { libc::raise(host) };
        let (n, event) = collect(queue, None);
        assert_eq!((n, event.data), (1, 1));

        change.flags = EV_DELETE;
        assert_eq!(collect(queue, Some(&change)).0, 0);
        // SAFETY: the disposition is SIG_IGN again, or this kills the test.
        unsafe { libc::raise(host) };
        assert_eq!(collect(queue, None).0, 0);
        // SAFETY: this test's own descriptor.
        unsafe { libc::close(queue) };
    }

    /// `SIGCHLD`, which is what `opend` watches: a child exiting is reported.
    #[test]
    fn a_signal_event_reports_a_child_exiting() {
        use crate::signal::signum::SIGCHLD;
        let queue = kqueue();
        let add = Kevent {
            ident: SIGCHLD as usize,
            filter: EVFILT_SIGNAL,
            flags: EV_ADD,
            ..Kevent::default()
        };
        assert_eq!(collect(queue, Some(&add)).0, 0);
        let mut child = std::process::Command::new("true").spawn().expect("true");
        child.wait().expect("the child");
        let mut got = [Kevent::default(); 1];
        let timeout = libc::timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        // SAFETY: no changes, room for one event.
        let n = unsafe {
            kevent(
                queue,
                std::ptr::null(),
                0,
                got.as_mut_ptr(),
                1,
                &raw const timeout,
            )
        };
        assert_eq!(n, 1);
        assert!(got[0].data >= 1);
        // SAFETY: this test's own descriptor.
        unsafe { libc::close(queue) };
    }

    /// A filter this module cannot answer is refused, and `EV_RECEIPT` is how
    /// the caller finds out which change it was.
    #[test]
    fn an_unsupported_filter_is_refused_by_number() {
        let queue = kqueue();
        const EVFILT_VNODE: i16 = -4;
        let add = Kevent {
            ident: 1,
            filter: EVFILT_VNODE,
            flags: EV_ADD | EV_RECEIPT,
            ..Kevent::default()
        };
        let mut got = [Kevent::default(); 1];
        // SAFETY: one change and room for one event.
        let n = unsafe {
            kevent(
                queue,
                &raw const add,
                1,
                got.as_mut_ptr(),
                1,
                &raw const NO_WAIT,
            )
        };
        assert_eq!(n, 1);
        assert_eq!(got[0].flags & EV_ERROR, EV_ERROR);
        assert_eq!(got[0].data as i32, crate::errno::from_host(libc::EINVAL));
        // And without `EV_RECEIPT` the whole call fails.
        let add = Kevent {
            flags: EV_ADD,
            ..add
        };
        assert_eq!(
            unsafe {
                kevent(
                    queue,
                    &raw const add,
                    1,
                    got.as_mut_ptr(),
                    1,
                    &raw const NO_WAIT,
                )
            },
            -1
        );
        // SAFETY: this test's own descriptor.
        unsafe { libc::close(queue) };
    }

    /// A descriptor that is not a queue is `EBADF` rather than a queue with
    /// nothing in it.
    #[test]
    fn a_descriptor_that_is_not_a_queue_is_refused() {
        let mut got = [Kevent::default(); 1];
        // SAFETY: standard input is open and is not a kqueue.
        let n = unsafe {
            kevent(
                0,
                std::ptr::null(),
                0,
                got.as_mut_ptr(),
                1,
                &raw const NO_WAIT,
            )
        };
        assert_eq!(n, -1);
        assert_eq!(crate::errno::get(), libc::EBADF);
    }
}
