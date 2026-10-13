//! Discretionary access control: owner/group/other mode bits.
//!
//! Every decision is a pure function of a [`Creds`] snapshot and an object's
//! `(mode, uid, gid)`, so the whole policy is host-tested here and the syscall
//! layer only chooses *which* check a call needs
//! (`docs/archive/UNIX_PERMISSIONS.md` §6).
//!
//! uid 0 bypasses every check except "execute a file with no execute bit at
//! all". All existing workloads run as uid 0, which is why turning enforcement
//! on is a no-op for them: only a process that has dropped can be refused.
//!
//! The checks are gated by [`enforcing`], a runtime switch that starts **off**.
//! With it off, credentials and ownership are still real and reported — only the
//! refusals are skipped. That is the A/B lever, and the escape hatch if an image
//! trips a check nobody anticipated.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::creds::Creds;

pub const MAY_EXEC: u32 = 1;
pub const MAY_WRITE: u32 = 2;
pub const MAY_READ: u32 = 4;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_ISGID: u32 = 0o2000;
const S_ISVTX: u32 = 0o1000;

/// The ownership and mode of one filesystem object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Attr {
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

static ENFORCE: AtomicBool = AtomicBool::new(false);

/// Whether permission checks refuse anything. Read once per check site; a
/// relaxed load, same cost model as the BKL phase toggles.
#[must_use]
pub fn enforcing() -> bool {
    ENFORCE.load(Ordering::Relaxed)
}

/// Turn enforcement on or off. Not test-only: it is how an A/B flips the policy
/// in one binary.
pub fn set_enforcing(on: bool) {
    ENFORCE.store(on, Ordering::Relaxed);
}

/// May `c` access `a` for every bit in `want` (`MAY_*`)?
///
/// The three classes are **exclusive**, not unioned: an owner is judged by the
/// owner bits alone, so a file at `0o077` is closed to its owner even though
/// "other" may do anything. Linux does the same.
#[must_use]
pub fn may(c: &Creds, a: &Attr, want: u32) -> bool {
    if c.is_root() {
        // Root may search any directory, but executes a file only if somebody
        // could: some execute bit must be set.
        return want & MAY_EXEC == 0 || a.is_dir() || a.mode & 0o111 != 0;
    }
    let bits = if c.uid() == a.uid {
        a.mode >> 6
    } else if c.in_group(a.gid) {
        a.mode >> 3
    } else {
        a.mode
    };
    bits & 7 & want == want
}

/// May `c` remove or rename the entry for `victim` out of directory `dir`?
///
/// Write and search on the directory, plus the sticky-bit rule that makes a
/// shared `/tmp` (mode `1777`) safe: in a sticky directory only the file's
/// owner, the directory's owner, or root may remove an entry.
///
/// The two halves fail differently on Linux — no write or search on the
/// directory is `EACCES`, the sticky rule is `EPERM` — so callers that report
/// an errno check [`may`] on the directory and then [`sticky_allows`].
#[must_use]
pub fn may_delete(c: &Creds, dir: &Attr, victim: &Attr) -> bool {
    may(c, dir, MAY_WRITE | MAY_EXEC) && sticky_allows(c, dir, victim)
}

/// The sticky-directory half of [`may_delete`] alone.
#[must_use]
pub fn sticky_allows(c: &Creds, dir: &Attr, victim: &Attr) -> bool {
    dir.mode & S_ISVTX == 0 || c.is_root() || c.uid() == victim.uid || c.uid() == dir.uid
}

/// May `c` change the mode of `a`? Root or the owner.
#[must_use]
pub fn may_chmod(c: &Creds, a: &Attr) -> bool {
    c.is_root() || c.uid() == a.uid
}

/// The mode a `chmod` by `c` actually stores: a non-root caller that is not in
/// the file's group cannot set `S_ISGID` (Linux silently clears it).
#[must_use]
pub fn chmod_mode(c: &Creds, a: &Attr, mode: u32) -> u32 {
    if !c.is_root() && !c.in_group(a.gid) {
        mode & !S_ISGID
    } else {
        mode
    }
}

/// May `c` change `a`'s owner to `uid` and/or group to `gid` (`None` = leave)?
///
/// Root may do anything. Otherwise the owner may change the group to one it is
/// in, and nobody may give a file away — that is how a quota or a
/// setuid-on-exec bit would be laundered, and Linux refuses it too.
#[must_use]
pub fn may_chown(c: &Creds, a: &Attr, uid: Option<u32>, gid: Option<u32>) -> bool {
    if c.is_root() {
        return true;
    }
    if uid.is_some_and(|u| u != a.uid) {
        return false;
    }
    match gid {
        Some(g) if g != a.gid => c.uid() == a.uid && c.in_group(g),
        _ => c.uid() == a.uid || uid.is_none() && gid.is_none(),
    }
}

/// May `c` set `a`'s timestamps? To "now" needs ownership or write access; to
/// an arbitrary time needs ownership (`utimensat(2)`).
#[must_use]
pub fn may_set_times(c: &Creds, a: &Attr, to_now: bool) -> bool {
    c.is_root() || c.uid() == a.uid || to_now && may(c, a, MAY_WRITE)
}

/// May a process with `sender` credentials signal one with `target`'s?
/// Linux compares the sender's real/effective uid with the target's
/// real/saved uid; here those are all one value.
///
/// This is only the uid half — the box boundary is checked separately and
/// first, so uid 0 inside a box still cannot reach outside it.
#[must_use]
pub fn may_signal(sender: &Creds, target: &Creds) -> bool {
    sender.is_root() || sender.uid() == target.uid()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(uid: u32, gid: u32, groups: &[u32]) -> Creds {
        let mut c = Creds::ROOT;
        c.set_groups(groups).unwrap();
        c.set_gid(gid).unwrap();
        c.set_uid(uid).unwrap();
        c
    }

    fn file(mode: u32, uid: u32, gid: u32) -> Attr {
        Attr { mode: 0o100_000 | mode, uid, gid }
    }

    fn dir(mode: u32, uid: u32, gid: u32) -> Attr {
        Attr { mode: S_IFDIR | mode, uid, gid }
    }

    #[test]
    fn owner_group_other_truth_table() {
        let f = file(0o640, 1000, 50);
        let owner = user(1000, 1000, &[]);
        let member = user(2000, 2000, &[50]);
        let other = user(3000, 3000, &[]);
        assert!(may(&owner, &f, MAY_READ | MAY_WRITE));
        assert!(!may(&owner, &f, MAY_EXEC));
        assert!(may(&member, &f, MAY_READ));
        assert!(!may(&member, &f, MAY_WRITE));
        assert!(!may(&other, &f, MAY_READ));
    }

    #[test]
    fn classes_are_exclusive_so_an_owner_can_be_locked_out() {
        let f = file(0o077, 1000, 1000);
        assert!(!may(&user(1000, 1000, &[]), &f, MAY_READ));
        assert!(may(&user(2000, 2000, &[]), &f, MAY_READ));
    }

    #[test]
    fn primary_gid_selects_the_group_class() {
        let f = file(0o070, 0, 50);
        assert!(may(&user(1000, 50, &[]), &f, MAY_READ | MAY_WRITE | MAY_EXEC));
    }

    #[test]
    fn root_bypasses_everything_but_exec_without_any_x_bit() {
        let r = Creds::ROOT;
        assert!(may(&r, &file(0o000, 1000, 1000), MAY_READ | MAY_WRITE));
        assert!(!may(&r, &file(0o644, 0, 0), MAY_EXEC));
        assert!(may(&r, &file(0o100, 1000, 1000), MAY_EXEC), "any x bit will do");
        assert!(may(&r, &dir(0o000, 1000, 1000), MAY_EXEC), "search is not exec");
    }

    #[test]
    fn sticky_tmp_lets_you_delete_only_your_own() {
        let tmp = dir(0o1777, 0, 0);
        let me = user(1000, 1000, &[]);
        assert!(may_delete(&me, &tmp, &file(0o644, 1000, 1000)));
        assert!(!may_delete(&me, &tmp, &file(0o644, 2000, 2000)));
        assert!(may_delete(&Creds::ROOT, &tmp, &file(0o644, 2000, 2000)));
        // The directory's owner may delete anything in it.
        assert!(may_delete(&me, &dir(0o1777, 1000, 1000), &file(0o644, 2000, 2000)));
        // Without the sticky bit, write on the directory is enough.
        assert!(may_delete(&me, &dir(0o777, 0, 0), &file(0o644, 2000, 2000)));
    }

    #[test]
    fn delete_needs_write_and_search_on_the_directory() {
        let me = user(1000, 1000, &[]);
        assert!(!may_delete(&me, &dir(0o755, 0, 0), &file(0o644, 1000, 1000)));
        assert!(may_delete(&me, &dir(0o733, 0, 0), &file(0o644, 1000, 1000)), "wx is enough");
        assert!(!may_delete(&me, &dir(0o766, 0, 0), &file(0o644, 1000, 1000)), "no x");
    }

    #[test]
    fn chown_rules() {
        let me = user(1000, 1000, &[50]);
        let mine = file(0o644, 1000, 1000);
        assert!(!may_chown(&me, &mine, Some(2000), None), "cannot give a file away");
        assert!(may_chown(&me, &mine, Some(1000), None), "no-op uid is fine");
        assert!(may_chown(&me, &mine, None, Some(50)), "to a group I am in");
        assert!(!may_chown(&me, &mine, None, Some(60)));
        assert!(!may_chown(&me, &file(0o644, 2000, 50), None, Some(50)), "not owner");
        assert!(may_chown(&Creds::ROOT, &mine, Some(0), Some(0)));
    }

    #[test]
    fn chmod_rules() {
        let me = user(1000, 1000, &[]);
        assert!(may_chmod(&me, &file(0o644, 1000, 1)));
        assert!(!may_chmod(&me, &file(0o644, 2000, 1)));
        assert_eq!(chmod_mode(&me, &file(0, 1000, 77), 0o2755), 0o755);
        assert_eq!(chmod_mode(&me, &file(0, 1000, 1000), 0o2755), 0o2755);
        assert_eq!(chmod_mode(&Creds::ROOT, &file(0, 1, 77), 0o2755), 0o2755);
    }

    #[test]
    fn set_times_rules() {
        let me = user(1000, 1000, &[]);
        let theirs_writable = file(0o666, 2000, 2000);
        assert!(may_set_times(&me, &theirs_writable, true));
        assert!(!may_set_times(&me, &theirs_writable, false));
        assert!(may_set_times(&me, &file(0o400, 1000, 1000), false));
    }

    #[test]
    fn signal_rules() {
        let a = user(1000, 1000, &[]);
        let b = user(2000, 2000, &[]);
        assert!(may_signal(&a, &a));
        assert!(!may_signal(&a, &b));
        assert!(may_signal(&Creds::ROOT, &b));
        assert!(!may_signal(&a, &Creds::ROOT));
    }

    #[test]
    fn enforcement_switch_round_trips() {
        let was = enforcing();
        set_enforcing(true);
        assert!(enforcing());
        set_enforcing(was);
    }
}
