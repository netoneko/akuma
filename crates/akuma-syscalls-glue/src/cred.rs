//! The credential syscalls: `get*id`, `set*id`, `*groups`, `umask`.
//!
//! The rules live in `akuma_isolation::creds` and are host-tested there; this
//! file is the user-memory marshalling and the errno mapping. Both kernels reach
//! it — amd64 through `akuma-syscalls-abi`'s rows — so there is one answer.
//!
//! Model in one line: real = effective = saved, only uid 0 may change anything,
//! a drop is permanent, re-setting the current value always succeeds
//! (`docs/archive/UNIX_PERMISSIONS.md` §4).

use super::*;
use akuma_isolation::creds::{CredError, NGROUPS};

fn errno(e: CredError) -> u64 {
    match e {
        CredError::Perm => EPERM,
        CredError::Inval => EINVAL,
    }
}

/// `-1` (as the `u32` the ABI passes) means "leave this id alone".
fn opt_id(raw: u64) -> Option<u32> {
    let id = raw as u32;
    (id != u32::MAX).then_some(id)
}

/// Apply `f` to the caller's shared credentials. A kernel thread has none to
/// change; it is root, and every request from root that this model can express
/// succeeds, so answering 0 is the same answer it would get.
fn update(f: impl FnOnce(&mut akuma_isolation::creds::Creds) -> Result<(), CredError>) -> u64 {
    match akuma_exec::process::current_process_shared() {
        Some(p) => p.creds.update(f).map_or_else(errno, |()| 0),
        None => 0,
    }
}

pub(super) fn sys_getuid() -> u64 {
    u64::from(akuma_exec::process::current_creds().uid())
}

pub(super) fn sys_getgid() -> u64 {
    u64::from(akuma_exec::process::current_creds().gid())
}

/// `getresuid`/`getresgid`: real, effective and saved — one value three times.
///
/// Answering at all matters: util-linux's `setpriv` treats ENOSYS from these as
/// fatal (`docs/archive/DEVBOX_ISSUES.md` Issue 15). A NULL pointer is EFAULT,
/// matching Linux.
pub(super) fn sys_getres(rptr: u64, eptr: u64, sptr: u64, gid: bool) -> u64 {
    let c = akuma_exec::process::current_creds();
    let id = if gid { c.gid() } else { c.uid() };
    for ptr in [rptr, eptr, sptr] {
        if ptr == 0 || !validate_user_ptr(ptr, 4) {
            return EFAULT;
        }
        if write_user_val(ptr, &id).is_err() {
            return EFAULT;
        }
    }
    0
}

pub(super) fn sys_setuid(uid: u64) -> u64 {
    let uid = uid as u32;
    update(|c| c.set_uid(uid))
}

pub(super) fn sys_setgid(gid: u64) -> u64 {
    let gid = gid as u32;
    update(|c| c.set_gid(gid))
}

/// `setresuid(r, e, s)`; `setreuid(r, e)` is the same call with `s = -1`.
pub(super) fn sys_setresuid(r: u64, e: u64, s: u64) -> u64 {
    let (r, e, s) = (opt_id(r), opt_id(e), opt_id(s));
    update(|c| c.set_resuid(r, e, s))
}

pub(super) fn sys_setresgid(r: u64, e: u64, s: u64) -> u64 {
    let (r, e, s) = (opt_id(r), opt_id(e), opt_id(s));
    update(|c| c.set_resgid(r, e, s))
}

/// `setfsuid`/`setfsgid`: return the current id and change nothing. The
/// filesystem ids *are* the effective ids here, and Linux's own contract is that
/// these calls report the previous value and never fail.
pub(super) fn sys_setfsid(gid: bool) -> u64 {
    let c = akuma_exec::process::current_creds();
    u64::from(if gid { c.gid() } else { c.uid() })
}

/// `getgroups(size, list)`. `size == 0` is the "how many?" probe and touches
/// no memory; a buffer smaller than the list is EINVAL, per Linux.
pub(super) fn sys_getgroups(size: i32, list_ptr: u64) -> u64 {
    if size < 0 {
        return EINVAL;
    }
    let c = akuma_exec::process::current_creds();
    let groups = c.groups();
    if size == 0 {
        return groups.len() as u64;
    }
    if (size as usize) < groups.len() {
        return EINVAL;
    }
    let mut buf = [0u8; NGROUPS * 4];
    for (i, g) in groups.iter().enumerate() {
        buf[i * 4..i * 4 + 4].copy_from_slice(&g.to_ne_bytes());
    }
    let bytes = groups.len() * 4;
    if bytes > 0 {
        if !validate_user_ptr(list_ptr, bytes) {
            return EFAULT;
        }
        if copy_to_user(list_ptr, &buf[..bytes]).is_err() {
            return EFAULT;
        }
    }
    groups.len() as u64
}

/// `setgroups(size, list)`. Read into a stack array: no heap on this path.
pub(super) fn sys_setgroups(size: u64, list_ptr: u64) -> u64 {
    let n = size as usize;
    if n > NGROUPS {
        return EINVAL;
    }
    let mut buf = [0u8; NGROUPS * 4];
    if n > 0 {
        if !validate_user_ptr(list_ptr, n * 4) {
            return EFAULT;
        }
        if copy_from_user(&mut buf[..n * 4], list_ptr).is_err() {
            return EFAULT;
        }
    }
    let mut groups = [0u32; NGROUPS];
    for (i, g) in groups.iter_mut().take(n).enumerate() {
        *g = u32::from_ne_bytes([buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]]);
    }
    update(|c| c.set_groups(&groups[..n]))
}

/// `umask(mask)`: set the caller's mask, return the old one. Shared by the
/// thread group, like the rest of the credentials (Linux shares it per
/// `CLONE_FS`, which every thread-creating path sets).
pub(super) fn sys_umask(mask: u64) -> u64 {
    match akuma_exec::process::current_process_shared() {
        Some(p) => u64::from(p.creds.update(|c| c.set_umask(mask as u32))),
        None => u64::from(akuma_isolation::creds::DEFAULT_UMASK),
    }
}

/// The caller's umask, for the create paths that apply it.
#[must_use]
pub fn current_umask() -> u32 {
    u32::from(akuma_exec::process::current_creds().umask())
}
