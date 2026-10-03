use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;

use super::{Identity, Process, state_from_letter};
use crate::protocol::{State, Thread, Usage};

/// `/proc/<pid>/stat` from the state onwards, split on whitespace.
///
/// The command name sits in parentheses before the state and may itself hold
/// spaces and parentheses, so the split starts after the *last* `)`.
struct Stat {
    fields: Vec<String>,
}

impl Stat {
    fn read(path: &str) -> Option<Stat> {
        let text = std::fs::read_to_string(path).ok()?;
        let rest = &text[text.rfind(')')? + 1..];
        Some(Stat {
            fields: rest.split_whitespace().map(str::to_owned).collect(),
        })
    }

    /// Field `n` as `proc(5)` numbers them, from one.
    fn field(&self, n: usize) -> &str {
        self.fields.get(n - 3).map_or("", String::as_str)
    }

    fn num(&self, n: usize) -> i64 {
        self.field(n).parse().unwrap_or(0)
    }

    fn state(&self) -> State {
        state_from_letter(self.field(3).bytes().next().unwrap_or(b'S'))
    }
}

fn ticks_per_second() -> u64 {
    static TICKS: OnceLock<u64> = OnceLock::new();
    // SAFETY: `sysconf` only reads its argument.
    *TICKS.get_or_init(|| (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64)
}

fn page_size() -> u64 {
    // SAFETY: as above.
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(4096) as u64
}

fn ticks_to_ns(ticks: i64) -> u64 {
    (ticks.max(0) as u64).saturating_mul(1_000_000_000) / ticks_per_second()
}

/// The wall-clock time the machine booted, in seconds.
fn boot_time() -> i64 {
    static BOOT: OnceLock<i64> = OnceLock::new();
    *BOOT.get_or_init(|| {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("btime "))
                    .and_then(|value| value.trim().parse().ok())
            })
            .unwrap_or(0)
    })
}

/// A `Key:` line of `/proc/<pid>/status`, as its whitespace-separated words.
fn status_words<'a>(status: &'a str, key: &str) -> Vec<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|rest| rest.split_whitespace().collect())
        .unwrap_or_default()
}

fn status_number(status: &str, key: &str) -> u64 {
    status_words(status, key)
        .first()
        .and_then(|word| word.parse().ok())
        .unwrap_or(0)
}

pub fn identity(pid: i32) -> Option<Identity> {
    let stat = Stat::read(&format!("/proc/{pid}/stat"))?;
    let exe = std::fs::metadata(format!("/proc/{pid}/exe"))
        .map(|meta| (meta.dev(), meta.ino()))
        // A zombie has no executable any more, and is still the same process.
        .unwrap_or((0, 0));
    Some(Identity {
        start: stat.num(22) as u64,
        exe,
    })
}

/// Whether `pid` is still the process `identity` was taken from.
///
/// A zombie keeps its start time and loses its executable, and is still the
/// process: its parent has yet to collect it, and `ps` shows it until then.
pub fn alive(pid: i32, identity: &Identity) -> bool {
    match self::identity(pid) {
        Some(now) => now.start == identity.start && (now.exe == identity.exe || now.exe == (0, 0)),
        None => false,
    }
}

pub fn process(pid: i32) -> Option<Process> {
    let stat = Stat::read(&format!("/proc/{pid}/stat"))?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let id = |key: &str, at: usize| -> u32 {
        status_words(&status, key)
            .get(at)
            .and_then(|word| word.parse().ok())
            .unwrap_or(0)
    };
    let tty = stat.num(7);
    let (tty_major, tty_minor) = match tty {
        0 => (-1, -1),
        nr => (
            ((nr >> 8) & 0xfff) as i32,
            ((nr & 0xff) | ((nr >> 12) & 0xfff00)) as i32,
        ),
    };
    let ticks = stat.num(22).max(0) as u64;
    let per_second = ticks_per_second();
    Some(Process {
        ppid: stat.num(4) as i32,
        pgid: stat.num(5) as i32,
        sid: stat.num(6) as i32,
        tty_major,
        tty_minor,
        tpgid: stat.num(8) as i32,
        uid: id("Uid:", 0),
        euid: id("Uid:", 1),
        suid: id("Uid:", 2),
        gid: id("Gid:", 0),
        egid: id("Gid:", 1),
        sgid: id("Gid:", 2),
        state: stat.state(),
        nice: stat.num(19) as i32,
        start_sec: boot_time() + (ticks / per_second) as i64,
        start_usec: ((ticks % per_second) * 1_000_000 / per_second) as i32,
    })
}

pub fn usage(pid: i32) -> Option<Usage> {
    let stat = Stat::read(&format!("/proc/{pid}/stat"))?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).unwrap_or_default();
    let mut pages = statm
        .split_whitespace()
        .map(|word| word.parse::<u64>().unwrap_or(0));
    let page = page_size();
    let virtual_size = pages.next().unwrap_or(0) * page;
    let resident_size = pages.next().unwrap_or(0) * page;
    let shared_size = pages.next().unwrap_or(0) * page;
    // Unreadable for a process that changed credentials, which leaves the
    // counters at zero rather than refusing the whole answer.
    let io = std::fs::read_to_string(format!("/proc/{pid}/io")).unwrap_or_default();
    let io_number = |key: &str| status_number(&io, key);
    let regions = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_or(0, |maps| maps.lines().count() as u32);
    let threads = threads(pid).unwrap_or_default();
    Some(Usage {
        virtual_size,
        resident_size,
        resident_peak: (status_number(&status, "VmHWM:") * 1024).max(resident_size),
        shared_size,
        user_ns: ticks_to_ns(stat.num(14)),
        system_ns: ticks_to_ns(stat.num(15)),
        minor_faults: stat.num(10).max(0) as u64,
        major_faults: stat.num(12).max(0) as u64,
        voluntary_switches: status_number(&status, "voluntary_ctxt_switches:"),
        involuntary_switches: status_number(&status, "nonvoluntary_ctxt_switches:"),
        threads: (threads.len() as u32).max(stat.num(20).max(0) as u32),
        running_threads: threads
            .iter()
            .filter(|thread| thread.state == State::Running)
            .count() as u32,
        regions,
        read_bytes: io_number("read_bytes:"),
        write_bytes: io_number("write_bytes:"),
        start_ns: ticks_to_ns(stat.num(22)),
    })
}

pub fn threads(pid: i32) -> Option<Vec<Thread>> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut threads: Vec<Thread> = entries
        .flatten()
        .filter_map(|entry| {
            let tid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let base = format!("/proc/{pid}/task/{tid}");
            // A thread that exited between the listing and this read is gone,
            // not an error.
            let stat = Stat::read(&format!("{base}/stat"))?;
            let name = std::fs::read_to_string(format!("{base}/comm"))
                .map(|name| name.trim_end_matches('\n').to_owned())
                .unwrap_or_default();
            Some(Thread {
                tid,
                name,
                user_ns: ticks_to_ns(stat.num(14)),
                system_ns: ticks_to_ns(stat.num(15)),
                state: stat.state(),
                nice: stat.num(19) as i32,
            })
        })
        .collect();
    threads.sort_by_key(|thread| thread.tid);
    Some(threads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> i32 {
        // SAFETY: `getpid` has no preconditions.
        unsafe { libc::getpid() }
    }

    #[test]
    fn this_process_is_alive_under_its_own_identity() {
        let identity = identity(me()).unwrap();
        assert!(alive(me(), &identity));
        let other = Identity {
            start: identity.start + 1,
            ..identity
        };
        assert!(!alive(me(), &other), "a reused pid passed for the original");
    }

    #[test]
    fn this_process_reads_back_its_own_credentials_and_threads() {
        let process = process(me()).unwrap();
        // SAFETY: neither has preconditions.
        assert_eq!(process.uid, unsafe { libc::getuid() });
        assert_eq!(process.ppid, unsafe { libc::getppid() });
        assert!(process.start_sec > 0);

        let blocker = std::sync::Arc::new(std::sync::Barrier::new(2));
        let held = blocker.clone();
        let other = std::thread::Builder::new()
            .name("server-probe".into())
            .spawn(move || held.wait())
            .unwrap();
        let threads = threads(me()).unwrap();
        blocker.wait();
        other.join().unwrap();
        assert!(threads.len() >= 2);
        assert!(threads.iter().any(|thread| thread.name == "server-probe"));
        assert!(threads.iter().any(|thread| thread.tid == me()));
    }

    #[test]
    fn this_process_has_memory_and_has_used_time() {
        let usage = usage(me()).unwrap();
        assert!(usage.resident_size > 0);
        assert!(usage.virtual_size >= usage.resident_size);
        assert!(usage.resident_peak >= usage.resident_size);
        assert!(usage.regions > 0);
        assert!(usage.threads >= 1);
    }

    #[test]
    fn a_pid_nobody_has_is_not_answered() {
        assert!(process(i32::MAX).is_none());
        assert!(usage(i32::MAX).is_none());
        assert!(threads(i32::MAX).is_none());
    }
}
