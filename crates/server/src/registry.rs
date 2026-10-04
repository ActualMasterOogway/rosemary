use std::collections::HashMap;

use starnix::process::{self as host, Identity};

use crate::credentials::{Change, Credentials, Denied};
use crate::protocol::{Checkin, Process, Thread, Usage};

pub const LAUNCHD: i32 = 1;

struct Record {
    identity: Identity,
    checkin: Checkin,
    credentials: Credentials,
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

    pub fn checkin(
        &mut self,
        pid: i32,
        connection: u64,
        checkin: Checkin,
        identity: Identity,
    ) -> Credentials {
        let credentials = match checkin.fresh {
            true => self
                .inherited(pid)
                .unwrap_or_else(|| checkin.credentials.clone())
                .exec(checkin.setid),
            false => checkin.credentials.clone(),
        };
        self.records.insert(
            pid,
            Record {
                identity,
                checkin,
                credentials: credentials.clone(),
                connection: Some(connection),
                thread_names: HashMap::new(),
            },
        );
        credentials
    }

    /// The ids a new image at `pid` starts from: the image it replaced, or
    /// else the guest that started it. `None` when it was started from
    /// outside, by a host process, which is its own user.
    fn inherited(&mut self, pid: i32) -> Option<Credentials> {
        if let Some(previous) = self.live(pid) {
            return Some(previous.credentials.clone());
        }
        let parent = host::process(pid)?.ppid;
        self.live(parent).map(|record| record.credentials.clone())
    }

    /// Change `pid`'s ids, if the rules allow it.
    pub fn change(&mut self, pid: i32, change: &Change) -> Result<Credentials, Denied> {
        let record = self.live(pid).ok_or(Denied::Permission)?;
        let next = record.credentials.changed(change)?;
        record.credentials = next.clone();
        Ok(next)
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
            uid: record.credentials.ruid,
            euid: record.credentials.euid,
            suid: record.credentials.svuid,
            gid: record.credentials.rgid,
            egid: record.credentials.egid,
            sgid: record.credentials.svgid,
            groups: record.credentials.groups.clone(),
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
    use crate::credentials::SetId;

    fn me() -> i32 {
        // SAFETY: `getpid` has no preconditions.
        starnix::process::me()
    }

    fn checkin(path: &str) -> Checkin {
        Checkin {
            path: path.into(),
            argv: vec![path.as_bytes().to_vec()],
            ..Checkin::default()
        }
    }

    const USER: u32 = 501;

    fn fresh(path: &str, setid: SetId) -> Checkin {
        Checkin {
            credentials: Credentials::of(USER, 20, vec![20]),
            fresh: true,
            setid,
            ..checkin(path)
        }
    }

    const SETUID_ROOT: SetId = SetId {
        uid: Some(0),
        gid: None,
    };

    #[test]
    fn a_set_user_id_image_runs_as_its_owner_and_is_listed_so() {
        let mut registry = Registry::new();
        let identity = host::identity(me()).unwrap();
        let creds = registry.checkin(me(), 1, fresh("/usr/bin/sudo", SETUID_ROOT), identity);
        assert_eq!((creds.ruid, creds.euid), (USER, 0));
        let listed = registry.process(me()).unwrap();
        assert_eq!((listed.uid, listed.euid, listed.suid), (USER, 0, 0));
    }

    #[test]
    fn an_exec_inherits_the_ids_of_the_image_it_replaces() {
        let mut registry = Registry::new();
        let identity = host::identity(me()).unwrap();
        registry.checkin(me(), 1, fresh("/usr/bin/sudo", SETUID_ROOT), identity);
        registry
            .change(me(), &Change::SetUid(0))
            .expect("root may become root wholly");
        // The image sudo runs is not set-id, and is root all the same.
        let creds = registry.checkin(me(), 2, fresh("/bin/sh", SetId::default()), identity);
        assert_eq!((creds.ruid, creds.euid, creds.svuid), (0, 0, 0));
        assert!(!creds.tainted);
    }

    #[test]
    fn a_process_that_checks_in_again_keeps_the_ids_it_sends() {
        let mut registry = Registry::new();
        let identity = host::identity(me()).unwrap();
        let mine = Credentials::of(0, 0, vec![0]);
        let again = Checkin {
            credentials: mine.clone(),
            ..checkin("/bin/sh")
        };
        assert_eq!(registry.checkin(me(), 1, again, identity), mine);
    }

    #[test]
    fn an_unprivileged_change_is_refused_and_leaves_the_record_alone() {
        let mut registry = Registry::new();
        let identity = host::identity(me()).unwrap();
        registry.checkin(me(), 1, fresh("/bin/sh", SetId::default()), identity);
        assert_eq!(
            registry.change(me(), &Change::SetUid(0)),
            Err(Denied::Permission)
        );
        assert_eq!(registry.process(me()).unwrap().euid, USER);
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
