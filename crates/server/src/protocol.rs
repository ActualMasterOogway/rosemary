pub use starnix::process::{State, Thread, Usage};

use crate::codec::{Decoder, Encoder, List, Malformed, Wire};
use crate::credentials::{Change, Credentials, Denied, SetId};
use crate::wire_struct;

// TODO: make this automatic based on source hashes like `metal_translate`
pub const VERSION: u32 = 4;

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
    /// Who the process is. A fresh image does not know yet - it is whoever
    /// ran it - and sends the host's user here, for the server to replace with
    /// what it inherits; see [`Checkin::fresh`].
    pub credentials: Credentials,
    /// Whether this is a new image, which inherits its ids from the image it
    /// replaced or the guest that started it and then applies `setid`. A
    /// `fork` child, or a process checking in again on a new connection, is
    /// not, and `credentials` are its own.
    pub fresh: bool,
    pub setid: SetId,
}

impl Wire for Checkin {
    fn put(&self, out: &mut Encoder) {
        out.put(&self.version)
            .put(&self.pid)
            .put(&self.path)
            .put(&List(self.argv.clone()))
            .put(&self.cputype)
            .put(&self.cpusubtype)
            .put(&self.translated)
            .put(&self.credentials)
            .put(&self.fresh)
            .put(&self.setid);
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
            credentials: input.take()?,
            fresh: input.take()?,
            setid: input.take()?,
        })
    }
}

impl Wire for State {
    fn put(&self, out: &mut Encoder) {
        let tag: u8 = match self {
            State::Running => 1,
            State::Sleeping => 2,
            State::Waiting => 3,
            State::Stopped => 4,
            State::Zombie => 5,
        };
        tag.put(out);
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
    /// The group access list, `cr_groups`.
    pub groups: Vec<u32>,
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
            .put(&List(self.groups.clone()))
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
            groups: input.take::<List<u32>>()?.0,
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

wire_struct!(Thread {
    tid,
    name,
    user_ns,
    system_ns,
    state,
    nice,
});

/// A running application, as LaunchServices knows it: one bundle's process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Application {
    pub pid: i32,
    /// The bundle as the guest spells it, symlinks resolved, so two spellings
    /// of one bundle are one application.
    pub bundle: String,
    pub identifier: Option<String>,
    pub name: Option<String>,
    /// `LSMultipleInstancesProhibited`: one instance per bundle identifier,
    /// wherever the bundle is and however it is launched.
    pub prohibited: bool,
}

impl Application {
    /// Whether this running instance is the application a `launch` of
    /// `wanted` means, so that the launch is handed to it.
    ///
    /// LaunchServices tells instances apart by bundle, so two copies of one
    /// application run side by side, except that an application prohibiting
    /// multiple instances is one per bundle identifier, which `-n` does not
    /// get past either.
    pub fn answers(&self, wanted: &Application, launch: Launch) -> bool {
        let same_identifier = (self.prohibited || wanted.prohibited)
            && wanted.identifier.is_some()
            && self.identifier == wanted.identifier;
        self.pid != wanted.pid
            && match launch {
                Launch::Open => self.bundle == wanted.bundle || same_identifier,
                Launch::NewInstance => same_identifier,
                Launch::Direct => false,
            }
    }
}

wire_struct!(Application {
    pid,
    bundle,
    identifier,
    name,
    prohibited,
});

/// How a launch treats an instance of its application that already runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Launch {
    /// LaunchServices' launch: a running instance is the application, and the
    /// launch is handed to it.
    #[default]
    Open,
    /// `open -n`, `kLSLaunchNewInstance`: another instance, unless the
    /// application prohibits one.
    NewInstance,
    /// The bundle's executable run directly, which LaunchServices is not
    /// asked about: listed, never refused.
    Direct,
}

impl Wire for Launch {
    fn put(&self, out: &mut Encoder) {
        let tag: u8 = match self {
            Launch::Open => 1,
            Launch::NewInstance => 2,
            Launch::Direct => 3,
        };
        tag.put(out);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => Launch::Open,
            2 => Launch::NewInstance,
            3 => Launch::Direct,
            _ => return Err(Malformed),
        })
    }
}

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
    /// Shut the prefix down now: every process outside launchd's tree, then
    /// launchd and its services, then the server.
    Shutdown,
    /// Change the caller's own ids, by the rules in [`crate::credentials`].
    Credentials(Change),
    /// Become `application`, unless `launch` says an instance already running
    /// is the application instead. Answered with [`Reply::Launch`].
    Launch {
        application: Application,
        launch: Launch,
    },
    /// The running applications, oldest first; only those with the bundle
    /// identifier when one is given.
    Applications {
        identifier: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub enum Reply {
    Done,
    /// Why a request was not answered. Not an error in the transport: the
    /// connection stays usable.
    Refused(String),
    CheckedIn {
        server_pid: i32,
        /// Who the process is, inherited and with its image's set-id applied.
        credentials: Credentials,
        /// The host pid of the prefix's launchd, which guests know as pid 1,
        /// or 0 when the prefix has none running.
        launchd: i32,
    },
    Processes(Vec<Process>),
    /// `None` for a pid that is not a live guest process.
    Process(Option<Process>),
    Usage(Option<Usage>),
    Threads(Option<Vec<Thread>>),
    /// The caller's ids after a [`Request::Credentials`], or why they did not
    /// change.
    Credentials(Result<Credentials, Denied>),
    /// `None`: the caller is the application now. Otherwise the instance that
    /// already is, which the launch belongs to.
    Launch(Option<Application>),
    Applications(Vec<Application>),
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
            Request::Credentials(change) => out.put(&8u8).put(change),
            Request::Launch {
                application,
                launch,
            } => out.put(&9u8).put(application).put(launch),
            Request::Applications { identifier } => out.put(&10u8).put(identifier),
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
            8 => Request::Credentials(input.take()?),
            9 => Request::Launch {
                application: input.take()?,
                launch: input.take()?,
            },
            10 => Request::Applications {
                identifier: input.take()?,
            },
            _ => return Err(Malformed),
        })
    }
}

impl Wire for Reply {
    fn put(&self, out: &mut Encoder) {
        match self {
            Reply::Done => out.put(&1u8),
            Reply::Refused(why) => out.put(&2u8).put(why),
            Reply::CheckedIn {
                server_pid,
                credentials,
                launchd,
            } => out.put(&3u8).put(server_pid).put(credentials).put(launchd),
            Reply::Processes(list) => out.put(&4u8).put(&List(list.clone())),
            Reply::Process(process) => out.put(&5u8).put(process),
            Reply::Usage(usage) => out.put(&6u8).put(usage),
            Reply::Threads(threads) => out
                .put(&7u8)
                .put(&threads.as_ref().map(|list| List(list.clone()))),
            Reply::Credentials(Ok(credentials)) => out.put(&8u8).put(credentials),
            Reply::Credentials(Err(denied)) => out.put(&9u8).put(denied),
            Reply::Launch(running) => out.put(&10u8).put(running),
            Reply::Applications(list) => out.put(&11u8).put(&List(list.clone())),
        };
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => Reply::Done,
            2 => Reply::Refused(input.take()?),
            3 => Reply::CheckedIn {
                server_pid: input.take()?,
                credentials: input.take()?,
                launchd: input.take()?,
            },
            4 => Reply::Processes(input.take::<List<Process>>()?.0),
            5 => Reply::Process(input.take()?),
            6 => Reply::Usage(input.take()?),
            7 => Reply::Threads(input.take::<Option<List<Thread>>>()?.map(|list| list.0)),
            8 => Reply::Credentials(Ok(input.take()?)),
            9 => Reply::Credentials(Err(input.take()?)),
            10 => Reply::Launch(input.take()?),
            11 => Reply::Applications(input.take::<List<Application>>()?.0),
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
    fn a_launch_and_its_answer_round_trip() {
        let application = Application {
            pid: 7,
            bundle: "/Applications/A.app".into(),
            identifier: Some("com.example.a".into()),
            name: None,
            prohibited: true,
        };
        let request = Request::Launch {
            application: application.clone(),
            launch: Launch::NewInstance,
        };
        match decode(&encode(&request)).unwrap() {
            Request::Launch {
                application: back,
                launch,
            } => {
                assert_eq!(back, application);
                assert_eq!(launch, Launch::NewInstance);
            }
            other => panic!("decoded as {other:?}"),
        }
        match decode(&encode(&Reply::Launch(Some(application.clone())))).unwrap() {
            Reply::Launch(Some(back)) => assert_eq!(back, application),
            other => panic!("decoded as {other:?}"),
        }
        match decode(&encode(&Reply::Applications(vec![application.clone()]))).unwrap() {
            Reply::Applications(list) => assert_eq!(list, vec![application]),
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn an_unknown_request_tag_is_malformed() {
        assert!(decode::<Request>(&[0x7f]).is_err());
    }
}
