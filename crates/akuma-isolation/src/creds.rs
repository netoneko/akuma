//! Process credentials: uid, gid, supplementary groups, umask.
//!
//! The model (`docs/archive/UNIX_PERMISSIONS.md` §4):
//!
//! - **Real, effective and saved ids are one value.** There is no saved uid to
//!   climb back to, so a drop is permanent by construction rather than by a check.
//!   Any call that would split them — `seteuid(1000)` from root, which on Linux
//!   keeps a saved 0 to return to — is refused with `EPERM`.
//! - **Only uid 0 may change anything**, and it may change each id exactly once
//!   in effect: after `setuid(1000)` nothing is uid 0 any more, so nothing can
//!   change again.
//! - **Re-setting the current value always succeeds.** musl implements
//!   `setuid()` as a `__synccall` broadcast that makes every thread repeat the
//!   syscall; credentials here are shared by the whole thread group, so the
//!   second and later threads set the value the first one already set. Refusing
//!   those would make every privilege drop report failure.
//!
//! Pure and `Copy`: a syscall snapshots the caller's `Creds` once and every
//! permission decision reads the snapshot. No heap anywhere — the group list is a
//! fixed array, and `setgroups` past [`NGROUPS`] is `EINVAL`.

use spinning_top::Spinlock;

/// Supplementary groups a process may hold. Linux allows 65536; an OCI image's
/// `/etc/group` puts a user in a handful, so 32 is generous and keeps `Creds`
/// small enough to copy per syscall.
pub const NGROUPS: usize = 32;

/// The `umask` a process starts with — Linux's default for `init`.
pub const DEFAULT_UMASK: u16 = 0o022;

/// Why a credential change was refused. The syscall layer maps these to errnos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredError {
    /// `EPERM`: not root, or the call would split real/effective/saved.
    Perm,
    /// `EINVAL`: more than [`NGROUPS`] groups.
    Inval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Creds {
    uid: u32,
    gid: u32,
    groups: [u32; NGROUPS],
    ngroups: u8,
    umask: u16,
}

impl Creds {
    /// uid 0, gid 0, no supplementary groups. What every process is until one
    /// drops.
    pub const ROOT: Self = Self {
        uid: 0,
        gid: 0,
        groups: [0; NGROUPS],
        ngroups: 0,
        umask: DEFAULT_UMASK,
    };

    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    #[must_use]
    pub const fn gid(&self) -> u32 {
        self.gid
    }

    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.uid == 0
    }

    #[must_use]
    pub fn groups(&self) -> &[u32] {
        &self.groups[..usize::from(self.ngroups)]
    }

    #[must_use]
    pub const fn umask(&self) -> u16 {
        self.umask
    }

    /// Whether `gid` is this process's primary or one of its supplementary
    /// groups — the test that selects the *group* class of a mode.
    #[must_use]
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups().contains(&gid)
    }

    /// `umask(2)`: set the mask (permission bits only), return the old one.
    pub fn set_umask(&mut self, mask: u32) -> u16 {
        let old = self.umask;
        self.umask = (mask & 0o777) as u16;
        old
    }

    /// `setuid(2)`.
    pub fn set_uid(&mut self, uid: u32) -> Result<(), CredError> {
        self.set_resuid(Some(uid), Some(uid), Some(uid))
    }

    /// `setresuid(2)` / `setreuid(2)`, with `None` for `-1` ("leave it").
    ///
    /// Because real/effective/saved are one value, the call is accepted only if
    /// every id it names is the *same* id: from root that is the drop, otherwise
    /// it must be the current uid. `None` follows the others, so
    /// `setresuid(1000, 1000, -1)` from root — `setpriv --reuid` — drops all
    /// three, where Linux would keep a saved 0.
    pub fn set_resuid(
        &mut self,
        r: Option<u32>,
        e: Option<u32>,
        s: Option<u32>,
    ) -> Result<(), CredError> {
        let target = single_id([r, e, s])?;
        let Some(uid) = target else { return Ok(()) };
        if uid == self.uid {
            return Ok(());
        }
        if !self.is_root() {
            return Err(CredError::Perm);
        }
        self.uid = uid;
        Ok(())
    }

    /// `setgid(2)`.
    pub fn set_gid(&mut self, gid: u32) -> Result<(), CredError> {
        self.set_resgid(Some(gid), Some(gid), Some(gid))
    }

    /// `setresgid(2)` / `setregid(2)` — the [`Self::set_resuid`] rule, gated on
    /// uid 0 (Linux's `CAP_SETGID`).
    pub fn set_resgid(
        &mut self,
        r: Option<u32>,
        e: Option<u32>,
        s: Option<u32>,
    ) -> Result<(), CredError> {
        let target = single_id([r, e, s])?;
        let Some(gid) = target else { return Ok(()) };
        if gid == self.gid {
            return Ok(());
        }
        if !self.is_root() {
            return Err(CredError::Perm);
        }
        self.gid = gid;
        Ok(())
    }

    /// `setgroups(2)`. Root sets; anyone may "set" the list they already have
    /// (same `__synccall` reasoning as the id setters).
    pub fn set_groups(&mut self, groups: &[u32]) -> Result<(), CredError> {
        if groups.len() > NGROUPS {
            return Err(CredError::Inval);
        }
        if groups == self.groups() {
            return Ok(());
        }
        if !self.is_root() {
            return Err(CredError::Perm);
        }
        self.groups = [0; NGROUPS];
        self.groups[..groups.len()].copy_from_slice(groups);
        // Bounded by the NGROUPS check above, and NGROUPS fits in a u8.
        self.ngroups = groups.len() as u8;
        Ok(())
    }
}

/// The one id a `set*res*id` call names, or `None` if it names none.
/// Two different ids is a split, which this model cannot represent.
fn single_id(ids: [Option<u32>; 3]) -> Result<Option<u32>, CredError> {
    let mut found: Option<u32> = None;
    for id in ids.into_iter().flatten() {
        match found {
            Some(f) if f != id => return Err(CredError::Perm),
            _ => found = Some(id),
        }
    }
    Ok(found)
}

/// Credentials shared by a thread group: `fork` gets a private copy,
/// `clone(CLONE_THREAD)` shares the `Arc`, `execve` keeps them.
///
/// Shared rather than per-thread so one thread's `setuid` is the whole
/// process's, which is what POSIX requires and what musl's broadcast emulates
/// on Linux.
pub struct SharedCreds(Spinlock<Creds>);

impl SharedCreds {
    #[must_use]
    pub const fn new(c: Creds) -> Self {
        Self(Spinlock::new(c))
    }

    /// A snapshot. Copy it out once per syscall and decide on the copy.
    #[must_use]
    pub fn get(&self) -> Creds {
        *self.0.lock()
    }

    /// Run `f` on the live credentials under the lock. `f` must not block or
    /// allocate; every [`Creds`] method satisfies that.
    pub fn update<R>(&self, f: impl FnOnce(&mut Creds) -> R) -> R {
        f(&mut self.0.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(uid: u32) -> Creds {
        let mut c = Creds::ROOT;
        c.set_uid(uid).unwrap();
        c
    }

    #[test]
    fn root_drops_once_and_cannot_come_back() {
        let mut c = Creds::ROOT;
        assert_eq!(c.set_uid(1000), Ok(()));
        assert_eq!(c.uid(), 1000);
        assert_eq!(c.set_uid(0), Err(CredError::Perm));
        assert_eq!(c.set_uid(1001), Err(CredError::Perm));
        assert_eq!(c.uid(), 1000);
    }

    #[test]
    fn re_setting_the_current_id_succeeds_for_the_synccall_broadcast() {
        let mut c = user(1000);
        assert_eq!(c.set_uid(1000), Ok(()));
        assert_eq!(c.set_resuid(Some(1000), Some(1000), Some(1000)), Ok(()));
        assert_eq!(c.set_resuid(None, None, None), Ok(()));
    }

    #[test]
    fn setpriv_reuid_with_saved_left_alone_drops_all_three() {
        // `setresuid(999, 999, -1)` — Linux would keep saved 0.
        let mut c = Creds::ROOT;
        assert_eq!(c.set_resuid(Some(999), Some(999), None), Ok(()));
        assert_eq!(c.uid(), 999);
        assert_eq!(c.set_uid(0), Err(CredError::Perm));
    }

    #[test]
    fn a_split_is_refused_even_from_root() {
        // `seteuid(1000)` from root = setresuid(-1, 1000, -1) is fine (one id);
        // an explicit split is not.
        let mut c = Creds::ROOT;
        assert_eq!(c.set_resuid(Some(0), Some(1000), None), Err(CredError::Perm));
        assert_eq!(c.uid(), 0);
    }

    #[test]
    fn gid_and_groups_change_only_while_root() {
        let mut c = Creds::ROOT;
        assert_eq!(c.set_groups(&[10, 20]), Ok(()));
        assert_eq!(c.set_gid(100), Ok(()));
        assert_eq!(c.set_uid(1000), Ok(()));
        assert_eq!(c.set_gid(101), Err(CredError::Perm));
        assert_eq!(c.set_groups(&[10]), Err(CredError::Perm));
        assert_eq!(c.set_groups(&[10, 20]), Ok(()), "same list is a no-op");
        assert!(c.in_group(100) && c.in_group(20) && !c.in_group(30));
    }

    #[test]
    fn too_many_groups_is_einval() {
        let mut c = Creds::ROOT;
        let many = [7u32; NGROUPS + 1];
        assert_eq!(c.set_groups(&many), Err(CredError::Inval));
        assert_eq!(c.set_groups(&many[..NGROUPS]), Ok(()));
        assert_eq!(c.groups().len(), NGROUPS);
    }

    #[test]
    fn umask_keeps_permission_bits_and_returns_the_old_mask() {
        let mut c = Creds::ROOT;
        assert_eq!(c.set_umask(0o7077), DEFAULT_UMASK);
        assert_eq!(c.umask(), 0o077);
    }

    #[test]
    fn shared_creds_is_one_value_for_every_holder() {
        let s = alloc::sync::Arc::new(SharedCreds::new(Creds::ROOT));
        let t = s.clone();
        s.update(|c| c.set_uid(5)).unwrap();
        assert_eq!(t.get().uid(), 5);
    }
}
