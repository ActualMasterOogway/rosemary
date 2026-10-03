//! The process table.
//!
//! A record exists for every process that checked in and is still the process
//! that did. Liveness is the host's to say, not the connection's: a process
//! that exits stays a zombie until its parent collects it, and `ps` is meant
//! to show it until then; one that `exec`s a host program is no longer a guest
//! although its pid lives on. [`starnix::process::alive`] tells both apart.

use std::collections::HashMap;

use starnix::process::{self as host, Identity};

use crate::protocol::{Checkin, Process, Thread, Usage};

/// The pid a guest whose parent is not a guest is given as its parent:
/// launchd's, so that walking up from any process ends where it does on a Mac.
pub const LAUNCHD: i32 = 1;

struct Record {
    identity: Identity,
    checkin: Checkin,
    /// The connection the process checked in on, while it is open.
    connection: Option<u64>,
    /// Names the guest gave its threads, which the host may have truncated.
    thread_names: HashMap<i32, String>,
}

#[derive(Default)]
pub struct Registry {
    records: HashMap<i32, Record>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `pid` as the process described by `checkin`, replacing whatever
    /// was recorded for the pid before - a process that `exec`s another guest
    /// checks in again as itself.
    pub fn checkin(&mut self, pid: i32, connection: u64, checkin: Checkin, identity: Identity) {
        self.records.insert(
            pid,
            Record {
                identity,
                checkin,
                connection: Some(connection),
                thread_names: HashMap::new(),
            },
        );
    }

    /// The connection `connection` closed. Its process may still be alive
    /// (a zombie, or about to check in again from a new image), so the record
    /// stays until the host says otherwise.
    pub fn disconnected(&mut self, connection: u64) {
        for record in self.records.values_mut() {
            if record.connection == Some(connection) {
                record.connection = None;
            }
        }
    }

    pub fn name_thread(&mut self, pid: i32, tid: i32, name: String) {
        if let Some(record) = self.records.get_mut(&pid) {
            record.thread_names.insert(tid, name);
        }
    }

    /// Drop every record whose process is gone or is no longer a guest.
    pub fn prune(&mut self) {
        self.records
            .retain(|pid, record| host::alive(*pid, &record.identity));
    }

    fn live(&mut self, pid: i32) -> Option<&mut Record> {
        let alive = host::alive(pid, &self.records.get(&pid)?.identity);
        if !alive {
            self.records.remove(&pid);
            return None;
        }
        self.records.get_mut(&pid)
    }

    pub fn is_guest(&mut self, pid: i32) -> bool {
        self.live(pid).is_some()
    }

    /// Every connected process, for diagnostics and for tests.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn describe(&self, pid: i32) -> Option<Process> {
        let record = self.records.get(&pid)?;
        let host = host::process(pid)?;
        let ppid = match self.records.contains_key(&host.ppid) {
            true => host.ppid,
            false => LAUNCHD,
        };
        Some(Process {
            pid,
            ppid,
            pgid: host.pgid,
            sid: host.sid,
            tty_major: host.tty_major,
            tty_minor: host.tty_minor,
            tpgid: host.tpgid,
            uid: host.uid,
            euid: host.euid,
            suid: host.suid,
            gid: host.gid,
            egid: host.egid,
            sgid: host.sgid,
            state: host.state,
            nice: host.nice,
            start_sec: host.start_sec,
            start_usec: host.start_usec,
            path: record.checkin.path.clone(),
            argv: record.checkin.argv.clone(),
            cputype: record.checkin.cputype,
            cpusubtype: record.checkin.cpusubtype,
            translated: record.checkin.translated,
        })
    }

    pub fn process(&mut self, pid: i32) -> Option<Process> {
        self.live(pid)?;
        self.describe(pid)
    }

    /// Every live guest process, in pid order.
    pub fn processes(&mut self) -> Vec<Process> {
        self.prune();
        let mut pids: Vec<i32> = self.records.keys().copied().collect();
        pids.sort_unstable();
        pids.into_iter()
            .filter_map(|pid| self.describe(pid))
            .collect()
    }

    pub fn usage(&mut self, pid: i32) -> Option<Usage> {
        self.live(pid)?;
        host::usage(pid)
    }

    pub fn threads(&mut self, pid: i32) -> Option<Vec<Thread>> {
        let record = self.live(pid)?;
        let mut threads = host::threads(pid)?;
        record
            .thread_names
            .retain(|tid, _| threads.iter().any(|thread| thread.tid == *tid));
        for thread in &mut threads {
            if let Some(name) = record.thread_names.get(&thread.tid) {
                thread.name = name.clone();
            }
        }
        Some(threads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> i32 {
        // SAFETY: `getpid` has no preconditions.
        unsafe { libc::getpid() }
    }

    fn checkin(path: &str) -> Checkin {
        Checkin {
            path: path.into(),
            argv: vec![path.as_bytes().to_vec()],
            ..Checkin::default()
        }
    }

    #[test]
    fn a_checked_in_process_is_listed_with_its_guest_path() {
        let mut registry = Registry::new();
        registry.checkin(
            me(),
            1,
            checkin("/usr/bin/top"),
            host::identity(me()).unwrap(),
        );
        let list = registry.processes();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pid, me());
        assert_eq!(list[0].command(), "top");
        // The test runner's parent did not check in, so it is launchd's child.
        assert_eq!(list[0].ppid, LAUNCHD);
    }

    #[test]
    fn a_closed_connection_does_not_end_a_live_process() {
        let mut registry = Registry::new();
        registry.checkin(me(), 7, checkin("/bin/sh"), host::identity(me()).unwrap());
        registry.disconnected(7);
        assert!(registry.is_guest(me()));
    }

    #[test]
    fn a_process_that_is_no_longer_itself_is_dropped() {
        let mut registry = Registry::new();
        let stale = Identity {
            start: host::identity(me()).unwrap().start + 1,
            ..Identity::default()
        };
        registry.checkin(me(), 1, checkin("/bin/sh"), stale);
        assert!(registry.processes().is_empty());
        assert!(!registry.is_guest(me()));
    }

    #[test]
    fn a_guest_thread_name_outlives_the_hosts_truncation() {
        let mut registry = Registry::new();
        registry.checkin(me(), 1, checkin("/bin/sh"), host::identity(me()).unwrap());
        let long = "com.apple.main-thread.with-a-long-name".to_string();
        registry.name_thread(me(), me(), long.clone());
        let threads = registry.threads(me()).unwrap();
        let main = threads.iter().find(|thread| thread.tid == me()).unwrap();
        assert_eq!(main.name, long);
    }

    #[test]
    fn only_guests_are_answered() {
        let mut registry = Registry::new();
        assert!(registry.usage(me()).is_none());
        assert!(registry.process(me()).is_none());
    }
}
