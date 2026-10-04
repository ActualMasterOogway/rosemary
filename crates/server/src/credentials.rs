use crate::codec::{Decoder, Encoder, List, Malformed, Wire};

pub const UNCHANGED: u32 = u32::MAX;

pub const MAX_GROUPS: usize = 16;

pub const ROOT: u32 = 0;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub ruid: u32,
    pub euid: u32,
    pub svuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub svgid: u32,
    pub groups: Vec<u32>,
    /// `P_SUGID`: the ids changed since the last `exec`, or that `exec` was of
    /// a set-id program. What `issetugid` answers.
    pub tainted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    Permission,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    SetUid(u32),
    SetEuid(u32),
    SetReuid(u32, u32),
    SetGid(u32),
    SetEgid(u32),
    SetRegid(u32, u32),
    SetGroups(Vec<u32>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetId {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl Credentials {
    pub fn of(uid: u32, gid: u32, groups: Vec<u32>) -> Credentials {
        Credentials {
            ruid: uid,
            euid: uid,
            svuid: uid,
            rgid: gid,
            egid: gid,
            svgid: gid,
            groups,
            tainted: false,
        }
    }

    pub fn is_root(&self) -> bool {
        self.euid == ROOT
    }

    pub fn in_group(&self, gid: u32) -> bool {
        self.egid == gid || self.groups.contains(&gid)
    }

    pub fn changed(&self, change: &Change) -> Result<Credentials, Denied> {
        let mut next = self.clone();
        match *change {
            Change::SetUid(uid) => {
                if uid == UNCHANGED {
                    return Err(Denied::Invalid);
                }
                if uid != self.ruid && uid != self.svuid && !self.is_root() {
                    return Err(Denied::Permission);
                }
                if self.is_root() {
                    next.ruid = uid;
                    next.svuid = uid;
                }
                next.euid = uid;
            }
            Change::SetEuid(euid) => {
                if euid == UNCHANGED {
                    return Err(Denied::Invalid);
                }
                if euid != self.ruid && euid != self.svuid && !self.is_root() {
                    return Err(Denied::Permission);
                }
                next.euid = euid;
            }
            Change::SetReuid(ruid, euid) => {
                let held = |id: u32| id == self.ruid || id == self.euid || id == self.svuid;
                let allowed =
                    (ruid == UNCHANGED || held(ruid)) && (euid == UNCHANGED || held(euid));
                if !allowed && !self.is_root() {
                    return Err(Denied::Permission);
                }
                if euid != UNCHANGED {
                    next.euid = euid;
                }
                if ruid != UNCHANGED {
                    next.ruid = ruid;
                }
                // XNU: a new id that is not the saved one moves the saved id to
                // the new effective one, so the old privilege cannot be got back.
                let differs = |id: u32| id != UNCHANGED && id != self.svuid;
                if differs(ruid) || differs(euid) {
                    next.svuid = next.euid;
                }
            }
            Change::SetGid(gid) => {
                if gid == UNCHANGED {
                    return Err(Denied::Invalid);
                }
                if gid != self.rgid && gid != self.svgid && !self.is_root() {
                    return Err(Denied::Permission);
                }
                if self.is_root() {
                    next.rgid = gid;
                    next.svgid = gid;
                }
                next.egid = gid;
            }
            Change::SetEgid(egid) => {
                if egid == UNCHANGED {
                    return Err(Denied::Invalid);
                }
                if egid != self.rgid && egid != self.svgid && !self.is_root() {
                    return Err(Denied::Permission);
                }
                next.egid = egid;
            }
            Change::SetRegid(rgid, egid) => {
                let held = |id: u32| id == self.rgid || id == self.egid || id == self.svgid;
                let allowed =
                    (rgid == UNCHANGED || held(rgid)) && (egid == UNCHANGED || held(egid));
                if !allowed && !self.is_root() {
                    return Err(Denied::Permission);
                }
                if egid != UNCHANGED {
                    next.egid = egid;
                }
                if rgid != UNCHANGED {
                    next.rgid = rgid;
                }
                let differs = |id: u32| id != UNCHANGED && id != self.svgid;
                if differs(rgid) || differs(egid) {
                    next.svgid = next.egid;
                }
            }
            Change::SetGroups(ref groups) => {
                if groups.len() > MAX_GROUPS {
                    return Err(Denied::Invalid);
                }
                if !self.is_root() {
                    return Err(Denied::Permission);
                }
                next.groups = groups.clone();
            }
        }
        if next != *self {
            next.tainted = true;
        }
        Ok(next)
    }

    pub fn exec(&self, setid: SetId) -> Credentials {
        let mut next = self.clone();
        next.tainted = false;
        if let Some(uid) = setid.uid
            && uid != self.euid
        {
            next.euid = uid;
            next.tainted = true;
        }
        if let Some(gid) = setid.gid
            && gid != self.egid
        {
            next.egid = gid;
            next.tainted = true;
        }
        next.svuid = next.euid;
        next.svgid = next.egid;
        next
    }
}

impl Wire for Credentials {
    fn put(&self, out: &mut Encoder) {
        out.put(&self.ruid)
            .put(&self.euid)
            .put(&self.svuid)
            .put(&self.rgid)
            .put(&self.egid)
            .put(&self.svgid)
            .put(&List(self.groups.clone()))
            .put(&self.tainted);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(Self {
            ruid: input.take()?,
            euid: input.take()?,
            svuid: input.take()?,
            rgid: input.take()?,
            egid: input.take()?,
            svgid: input.take()?,
            groups: input.take::<List<u32>>()?.0,
            tainted: input.take()?,
        })
    }
}

impl Wire for SetId {
    fn put(&self, out: &mut Encoder) {
        out.put(&self.uid).put(&self.gid);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(Self {
            uid: input.take()?,
            gid: input.take()?,
        })
    }
}

impl Wire for Denied {
    fn put(&self, out: &mut Encoder) {
        let tag: u8 = match self {
            Denied::Permission => 1,
            Denied::Invalid => 2,
        };
        tag.put(out);
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => Denied::Permission,
            2 => Denied::Invalid,
            _ => return Err(Malformed),
        })
    }
}

impl Wire for Change {
    fn put(&self, out: &mut Encoder) {
        match self {
            Change::SetUid(id) => out.put(&1u8).put(id),
            Change::SetEuid(id) => out.put(&2u8).put(id),
            Change::SetReuid(r, e) => out.put(&3u8).put(r).put(e),
            Change::SetGid(id) => out.put(&4u8).put(id),
            Change::SetEgid(id) => out.put(&5u8).put(id),
            Change::SetRegid(r, e) => out.put(&6u8).put(r).put(e),
            Change::SetGroups(groups) => out.put(&7u8).put(&List(groups.clone())),
        };
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            1 => Change::SetUid(input.take()?),
            2 => Change::SetEuid(input.take()?),
            3 => Change::SetReuid(input.take()?, input.take()?),
            4 => Change::SetGid(input.take()?),
            5 => Change::SetEgid(input.take()?),
            6 => Change::SetRegid(input.take()?, input.take()?),
            7 => Change::SetGroups(input.take::<List<u32>>()?.0),
            _ => return Err(Malformed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: u32 = 501;
    const STAFF: u32 = 20;

    fn user() -> Credentials {
        Credentials::of(USER, STAFF, vec![STAFF])
    }

    fn setuid_root() -> Credentials {
        user().exec(SetId {
            uid: Some(ROOT),
            gid: None,
        })
    }

    #[test]
    fn a_set_user_id_program_runs_as_its_owner_and_says_so() {
        let creds = setuid_root();
        assert_eq!((creds.ruid, creds.euid, creds.svuid), (USER, ROOT, ROOT));
        assert!(creds.tainted);
        // The next ordinary exec keeps the effective id and clears the mark.
        let after = creds.exec(SetId::default());
        assert_eq!((after.ruid, after.euid, after.svuid), (USER, ROOT, ROOT));
        assert!(!after.tainted);
    }

    #[test]
    fn root_setuid_gives_every_id_away() {
        let creds = setuid_root().changed(&Change::SetUid(USER)).unwrap();
        assert_eq!((creds.ruid, creds.euid, creds.svuid), (USER, USER, USER));
        assert_eq!(
            creds.changed(&Change::SetUid(ROOT)),
            Err(Denied::Permission)
        );
    }

    #[test]
    fn seteuid_moves_between_held_ids_only() {
        let dropped = setuid_root().changed(&Change::SetEuid(USER)).unwrap();
        assert_eq!(
            (dropped.ruid, dropped.euid, dropped.svuid),
            (USER, USER, ROOT)
        );
        let back = dropped.changed(&Change::SetEuid(ROOT)).unwrap();
        assert_eq!(back.euid, ROOT);
        assert_eq!(
            dropped.changed(&Change::SetEuid(7)),
            Err(Denied::Permission)
        );
    }

    #[test]
    fn an_unprivileged_setuid_moves_only_the_effective_id() {
        let creds = user().changed(&Change::SetUid(USER)).unwrap();
        assert_eq!(creds, user(), "no change is no change");
        assert_eq!(
            user().changed(&Change::SetUid(ROOT)),
            Err(Denied::Permission)
        );
    }

    #[test]
    fn setreuid_to_root_makes_the_saved_id_root() {
        let creds = setuid_root()
            .changed(&Change::SetReuid(ROOT, UNCHANGED))
            .unwrap();
        assert_eq!((creds.ruid, creds.euid, creds.svuid), (ROOT, ROOT, ROOT));
    }

    #[test]
    fn setreuid_swapping_ids_saves_the_new_effective_one() {
        let creds = setuid_root()
            .changed(&Change::SetReuid(ROOT, USER))
            .unwrap();
        assert_eq!((creds.ruid, creds.euid, creds.svuid), (ROOT, USER, USER));
    }

    #[test]
    fn only_root_sets_groups_and_only_so_many() {
        assert_eq!(
            user().changed(&Change::SetGroups(vec![0])),
            Err(Denied::Permission)
        );
        let root = setuid_root();
        assert_eq!(
            root.changed(&Change::SetGroups(vec![0; MAX_GROUPS + 1])),
            Err(Denied::Invalid)
        );
        let creds = root.changed(&Change::SetGroups(vec![0, 80])).unwrap();
        assert!(creds.in_group(80));
    }

    #[test]
    fn setgid_follows_the_same_rules_as_setuid() {
        let root = setuid_root();
        let creds = root.changed(&Change::SetGid(0)).unwrap();
        assert_eq!((creds.rgid, creds.egid, creds.svgid), (0, 0, 0));
        assert_eq!(user().changed(&Change::SetGid(0)), Err(Denied::Permission));
        assert_eq!(user().changed(&Change::SetEgid(0)), Err(Denied::Permission));
    }

    #[test]
    fn a_change_round_trips() {
        use crate::codec::{decode, encode};
        for change in [
            Change::SetReuid(UNCHANGED, 0),
            Change::SetGroups(vec![0, 80, 20]),
        ] {
            assert_eq!(decode::<Change>(&encode(&change)).unwrap(), change);
        }
        let creds = setuid_root();
        assert_eq!(decode::<Credentials>(&encode(&creds)).unwrap(), creds);
    }
}
