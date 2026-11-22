//! Permission checks at the syscall boundary, and ownership of new files.
//!
//! The policy is `akuma_isolation::dac` (pure, host-tested); this file decides
//! *which* check a call needs and against which object, and maps a refusal to
//! `EACCES`/`EPERM` (`docs/archive/UNIX_PERMISSIONS.md` §6).
//!
//! The global switch enables checks for all callers. An explicit capability
//! drop also enables them for that caller, including when the switch is off.
//! A caller with capabilities still skips the metadata read for
//! everything except exec — root passes every other check — so the only
//! processes that pay are the ones that dropped.
//!
//! Paths arrive resolved (absolute, symlinks chased where the caller follows
//! them). Nothing here allocates: parents are slices of the path.
//!
//! **Search permission on intermediate directories is not checked** — only the
//! final object and its parent. That under-denies (a file you may read inside a
//! directory you may not search opens anyway) and never over-denies, so nothing
//! that works on Linux breaks. Pinned as a divergence in §6.3.

use super::*;
use akuma_isolation::creds::Creds;
use akuma_isolation::dac::{self, Attr, MAY_EXEC, MAY_READ, MAY_WRITE};

/// `path`'s parent: `"/a/b"` → `"/a"`, `"/a"` → `"/"`.
#[must_use]
pub fn parent_of(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

fn attr_of(path: &str) -> Option<Attr> {
    akuma_vfs_glue::metadata(path).ok().map(|m| Attr { mode: m.mode, uid: m.uid, gid: m.gid })
}

/// The caller's credentials, but only if a check could refuse: `None` means
/// "allowed, nothing to read". A caller with capabilities needs the object only for exec.
fn subject(want: u32) -> Option<Creds> {
    let c = akuma_exec::process::current_creds();
    if !dac::enforcing() && !c.capabilities_dropped() {
        return None;
    }
    (!c.has_capabilities() || want & MAY_EXEC != 0).then_some(c)
}

/// May the caller access `path` for `want` (`MAY_*` bits)? `EACCES` if not.
/// A path with no metadata passes: the operation itself will report ENOENT.
pub fn check(path: &str, want: u32) -> Result<(), Errno> {
    let Some(c) = subject(want) else { return Ok(()) };
    match attr_of(path) {
        Some(a) if !dac::may(&c, &a, want) => Err(Errno::EACCES),
        _ => Ok(()),
    }
}

/// The check `open(2)` makes on an existing object: read and/or write by
/// access mode, write for `O_TRUNC`.
pub fn check_open(path: &str, flags: u32) -> Result<(), Errno> {
    use akuma_exec::process::open_flags::{O_RDWR, O_TRUNC, O_WRONLY};
    let mut want = match flags & 3 {
        x if x == O_WRONLY => MAY_WRITE,
        x if x == O_RDWR => MAY_READ | MAY_WRITE,
        _ => MAY_READ,
    };
    if flags & O_TRUNC != 0 {
        want |= MAY_WRITE;
    }
    check(path, want)
}

/// May the caller add an entry to `path`'s parent directory (create, mkdir,
/// symlink, link, bind)? Write and search on the parent.
pub fn check_create(path: &str) -> Result<(), Errno> {
    check(parent_of(path), MAY_WRITE | MAY_EXEC)
}

/// May the caller remove (or rename away) the entry `path`?
///
/// Write and search on the parent (`EACCES`), plus the sticky-directory rule
/// (`EPERM`, as Linux reports it) — the one that keeps a shared `/tmp` from
/// being a free-for-all.
pub fn check_delete(path: &str) -> Result<(), Errno> {
    let Some(c) = subject(0) else { return Ok(()) };
    let (Some(dir), Some(victim)) = (attr_of(parent_of(path)), attr_of(path)) else {
        return Ok(());
    };
    if !dac::may(&c, &dir, MAY_WRITE | MAY_EXEC) {
        return Err(Errno::EACCES);
    }
    if !dac::sticky_allows(&c, &dir, &victim) {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// The source of a hard link: Linux's `fs.protected_hardlinks` rule.
///
/// On by default everywhere that matters. A caller that does not own the file may
/// link it only if it could also read and write it — otherwise anyone could
/// pin someone else's setuid binary or log file under a name of their own.
pub fn check_link_source(path: &str) -> Result<(), Errno> {
    let Some(c) = subject(0) else { return Ok(()) };
    match attr_of(path) {
        Some(a) if c.uid() != a.uid && !dac::may(&c, &a, MAY_READ | MAY_WRITE) => Err(Errno::EPERM),
        _ => Ok(()),
    }
}

/// `chmod`: root or the owner, else `EPERM`. Returns the mode to store — a
/// non-root caller outside the file's group loses `S_ISGID`, as on Linux.
pub fn check_chmod(path: &str, mode: u32) -> Result<u32, Errno> {
    let Some(c) = subject(0) else { return Ok(mode) };
    let Some(a) = attr_of(path) else { return Ok(mode) };
    if !dac::may_chmod(&c, &a) {
        return Err(Errno::EPERM);
    }
    Ok(dac::chmod_mode(&c, &a, mode))
}

/// `chown`: see `dac::may_chown`. `EPERM` if refused.
pub fn check_chown(path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
    let Some(c) = subject(0) else { return Ok(()) };
    match attr_of(path) {
        Some(a) if !dac::may_chown(&c, &a, uid, gid) => Err(Errno::EPERM),
        _ => Ok(()),
    }
}

/// `utimensat`: ownership, or write access when setting "now".
pub fn check_set_times(path: &str, to_now: bool) -> Result<(), Errno> {
    let Some(c) = subject(0) else { return Ok(()) };
    match attr_of(path) {
        Some(a) if !dac::may_set_times(&c, &a, to_now) => {
            Err(if to_now { Errno::EACCES } else { Errno::EPERM })
        }
        _ => Ok(()),
    }
}

/// Make a just-created `path` belong to its creator.
///
/// Every filesystem creates entries as `0:0`, so a root creator — every
/// process until something drops — needs no second write, and this returns
/// after one credential snapshot. Otherwise the owner becomes the caller's
/// uid and the group its gid, or the parent's gid when the parent directory is
/// set-group-ID (BSD semantics, which Linux follows for such directories).
///
/// Not gated on `enforcing()`: ownership is recorded, and reported by `stat`,
/// whether or not anything is refused on its strength. Best-effort, like the
/// `chmod` the create paths already did after creating.
///
/// Divergence, pinned: a *root* creator in a set-group-ID directory keeps gid
/// 0 instead of inheriting the directory's group — that would cost a parent
/// lookup on every create for a case nothing here uses.
pub fn own_new(path: &str) {
    let c = akuma_exec::process::current_creds();
    if c.uid() == 0 && c.gid() == 0 {
        return;
    }
    let gid = match akuma_vfs_glue::metadata(parent_of(path)) {
        Ok(m) if m.mode & 0o2000 != 0 => m.gid,
        _ => c.gid(),
    };
    let _ = akuma_vfs_glue::chown(path, Some(c.uid()), Some(gid));
}

#[cfg(test)]
mod tests {
    use super::parent_of;

    #[test]
    fn parent_of_handles_root_and_trailing_slashes() {
        assert_eq!(parent_of("/a/b/c"), "/a/b");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(parent_of("/"), "/");
        assert_eq!(parent_of("/a/b/"), "/a");
    }
}
