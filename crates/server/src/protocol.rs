//! What a client may ask and what the server answers.
//!
//! Everything here is in host-neutral terms - nanoseconds, bytes, plain ids -
//! and Darwin's structures are built from it on the client side, by `darwin`,
//! which owns their layouts. The server is not a Darwin kernel; it is the one
//! place that can see every guest process at once.

use crate::codec::{Decoder, Encoder, List, Malformed, Wire};
use crate::wire_struct;

/// Bumped on any change to a message's shape. Client and server are the same
/// binary in practice; a mismatch means a server left over from another build
/// is still lingering, and it is refused rather than misread.
pub const VERSION: u32 = 1;

/// What a process tells the server about itself.
///
/// Where the host can say who is on the other end of the socket, the server
/// holds `pid` to that, so a process cannot check in as another.
#[derive(Clone, Debug, Default)]
pub struct Checkin {
    pub version: u32,
    pub pid: i32,
    /// The guest's spelling of its executable, which is what `ps` and `top`
    /// show - the host's would name the runtime for every process.
    pub path: String,
    pub argv: Vec<Vec<u8>>,
    pub cputype: i32,
    pub cpusubtype: i32,
    /// Whether the guest runs through the translator, `sysctl.proc_translated`.
    pub translated: bool,
}

impl Wire for Checkin {
    fn put(&self, out: &mut Encoder) {
        out.put(&self.version)
            .put(&self.pid)
            .put(&self.path)
            .put(&List(self.argv.clone()))
            .put(&self.cputype)
            .put(&self.cpusubtype)
            .put(&self.translated);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(Self {
            version: input.take()?,
            pid: input.take()?,
            path: input.take()?,
            argv: input.take::<List<Vec<u8>>>()?.0,
            cputype: input.take()?,
            cpusubtype: input.take()?,
            translated: input.take()?,
        })
    }
}

/// A process's scheduling state, in the coarse terms both kernels share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum State {
    #[default]
    Running = 1,
    Sleeping = 2,
    /// Uninterruptible: waiting on I/O and not answering signals.
    Waiting = 3,
    Stopped = 4,
    Zombie = 5,
}

impl Wire for State {
    fn put(&self, out: &mut Encoder) {
        (*self as u8).put(out);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => State::Running,
            2 => State::Sleeping,
            3 => State::Waiting,
            4 => State::Stopped,
            5 => State::Zombie,
            _ => return Err(Malformed),
        })
    }
}

/// One guest process, as `sysctl(KERN_PROC)` describes it.
#[derive(Clone, Debug, Default)]
pub struct Process {
    pub pid: i32,
    /// The parent *as a guest sees it*: the host parent when that is a guest
    /// too, and 1 - launchd - when it is not, so a chain of parents always
    /// ends inside the system the guest can see.
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    /// The controlling terminal as a major and minor number, or -1 for none.
    pub tty_major: i32,
    pub tty_minor: i32,
    pub tpgid: i32,
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub state: State,
    pub nice: i32,
    /// Wall-clock start time.
    pub start_sec: i64,
    pub start_usec: i32,
    pub path: String,
    pub argv: Vec<Vec<u8>>,
    pub cputype: i32,
    pub cpusubtype: i32,
    pub translated: bool,
}

impl Process {
    /// `p_comm`: the executable's leaf name.
    pub fn command(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

impl Wire for Process {
    fn put(&self, out: &mut Encoder) {
        out.put(&self.pid)
            .put(&self.ppid)
            .put(&self.pgid)
            .put(&self.sid)
            .put(&self.tty_major)
            .put(&self.tty_minor)
            .put(&self.tpgid)
            .put(&self.uid)
            .put(&self.euid)
            .put(&self.suid)
            .put(&self.gid)
            .put(&self.egid)
            .put(&self.sgid)
            .put(&self.state)
            .put(&self.nice)
            .put(&self.start_sec)
            .put(&self.start_usec)
            .put(&self.path)
            .put(&List(self.argv.clone()))
            .put(&self.cputype)
            .put(&self.cpusubtype)
            .put(&self.translated);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(Self {
            pid: input.take()?,
            ppid: input.take()?,
            pgid: input.take()?,
            sid: input.take()?,
            tty_major: input.take()?,
            tty_minor: input.take()?,
            tpgid: input.take()?,
            uid: input.take()?,
            euid: input.take()?,
            suid: input.take()?,
            gid: input.take()?,
            egid: input.take()?,
            sgid: input.take()?,
            state: input.take()?,
            nice: input.take()?,
            start_sec: input.take()?,
            start_usec: input.take()?,
            path: input.take()?,
            argv: input.take::<List<Vec<u8>>>()?.0,
            cputype: input.take()?,
            cpusubtype: input.take()?,
            translated: input.take()?,
        })
    }
}

/// A task's resource use: what `task_info` and `proc_pid_rusage` report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub virtual_size: u64,
    pub resident_size: u64,
    pub resident_peak: u64,
    /// Resident memory shared with another process; what is not shared is
    /// the closest thing to Mach's `phys_footprint`.
    pub shared_size: u64,
    pub user_ns: u64,
    pub system_ns: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub threads: u32,
    pub running_threads: u32,
    pub regions: u32,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// When the process started, on the monotonic clock `mach_absolute_time`
    /// reads, in nanoseconds.
    pub start_ns: u64,
}

wire_struct!(Usage {
    virtual_size,
    resident_size,
    resident_peak,
    shared_size,
    user_ns,
    system_ns,
    minor_faults,
    major_faults,
    voluntary_switches,
    involuntary_switches,
    threads,
    running_threads,
    regions,
    read_bytes,
    write_bytes,
    start_ns,
});

/// One thread of a task.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thread {
    pub tid: i32,
    /// The name the guest gave it, which can be longer than the host keeps.
    pub name: String,
    pub user_ns: u64,
    pub system_ns: u64,
    pub state: State,
    pub nice: i32,
}

wire_struct!(Thread {
    tid,
    name,
    user_ns,
    system_ns,
    state,
    nice,
});

#[derive(Clone, Debug)]
pub enum Request {
    /// The first message on a connection; nothing else is answered before it.
    Checkin(Checkin),
    /// Every live guest process.
    Processes,
    Process {
        pid: i32,
    },
    Usage {
        pid: i32,
    },
    Threads {
        pid: i32,
    },
    /// Record a thread's guest-visible name.
    ThreadName {
        tid: i32,
        name: String,
    },
    /// Ask the server to exit once this connection closes, rather than linger.
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum Reply {
    Done,
    /// Why a request was not answered. Not an error in the transport: the
    /// connection stays usable.
    Refused(String),
    CheckedIn {
        server_pid: i32,
    },
    Processes(Vec<Process>),
    /// `None` for a pid that is not a live guest process.
    Process(Option<Process>),
    Usage(Option<Usage>),
    Threads(Option<Vec<Thread>>),
}

impl Wire for Request {
    fn put(&self, out: &mut Encoder) {
        match self {
            Request::Checkin(checkin) => out.put(&1u8).put(checkin),
            Request::Processes => out.put(&2u8),
            Request::Process { pid } => out.put(&3u8).put(pid),
            Request::Usage { pid } => out.put(&4u8).put(pid),
            Request::Threads { pid } => out.put(&5u8).put(pid),
            Request::ThreadName { tid, name } => out.put(&6u8).put(tid).put(name),
            Request::Shutdown => out.put(&7u8),
        };
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => Request::Checkin(input.take()?),
            2 => Request::Processes,
            3 => Request::Process { pid: input.take()? },
            4 => Request::Usage { pid: input.take()? },
            5 => Request::Threads { pid: input.take()? },
            6 => Request::ThreadName {
                tid: input.take()?,
                name: input.take()?,
            },
            7 => Request::Shutdown,
            _ => return Err(Malformed),
        })
    }
}

impl Wire for Reply {
    fn put(&self, out: &mut Encoder) {
        match self {
            Reply::Done => out.put(&1u8),
            Reply::Refused(why) => out.put(&2u8).put(why),
            Reply::CheckedIn { server_pid } => out.put(&3u8).put(server_pid),
            Reply::Processes(list) => out.put(&4u8).put(&List(list.clone())),
            Reply::Process(process) => out.put(&5u8).put(process),
            Reply::Usage(usage) => out.put(&6u8).put(usage),
            Reply::Threads(threads) => out
                .put(&7u8)
                .put(&threads.as_ref().map(|list| List(list.clone()))),
        };
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => Reply::Done,
            2 => Reply::Refused(input.take()?),
            3 => Reply::CheckedIn {
                server_pid: input.take()?,
            },
            4 => Reply::Processes(input.take::<List<Process>>()?.0),
            5 => Reply::Process(input.take()?),
            6 => Reply::Usage(input.take()?),
            7 => Reply::Threads(input.take::<Option<List<Thread>>>()?.map(|list| list.0)),
            _ => return Err(Malformed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn a_process_list_round_trips() {
        let process = Process {
            pid: 42,
            ppid: 1,
            tty_major: -1,
            state: State::Zombie,
            path: "/usr/bin/top".into(),
            argv: vec![b"top".to_vec(), b"-l".to_vec()],
            translated: true,
            ..Process::default()
        };
        let back = match decode(&encode(&Reply::Processes(vec![process]))).unwrap() {
            Reply::Processes(list) => list,
            other => panic!("decoded as {other:?}"),
        };
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].pid, 42);
        assert_eq!(back[0].state, State::Zombie);
        assert_eq!(back[0].argv[1], b"-l");
        assert_eq!(back[0].command(), "top");
        assert!(back[0].translated);
    }

    #[test]
    fn an_unknown_request_tag_is_malformed() {
        assert!(decode::<Request>(&[0x7f]).is_err());
    }
}
