//! `flock(2)` — whole-file BSD advisory locking.
//!
//! Added 2026-08-22: not implemented at all before this (`grep -rn "fn
//! sys_flock" src/syscall/` was empty), found while chasing an nca hang —
//! see `userspace/nca/docs/ISSUES.md`.
//!
//! Simplification versus real Linux: a lock's holder identity is the calling
//! process's `SharedFdTable` (its `Arc` pointer) plus the fd number, not a
//! true per-`open()` "open file description". Two fds from *separate*
//! `open()` calls in the same process (or `CLONE_FILES` siblings sharing one
//! table) are correctly two independent lock attempts; but `dup()`ing an
//! already-locked fd does not automatically make the new fd number a joint
//! holder the way real Linux's open-file-description sharing would — the
//! dup'd fd would have to `flock()` again itself. Good enough for the
//! motivating cases (a shell `flock` invocation, a single-fd lockfile), not
//! attempted for full POSIX fidelity.
//!
//! Locked by **path string**, not inode — two different paths to the same
//! inode (hardlinks, bind mounts) are NOT recognised as the same lock. This
//! used to be the same trade-off `KernelFile` made, and so cost nothing extra;
//! since per-fd inode caching a `KernelFile` *does* carry the inode
//! (`KernelFile::inode`), so keying locks on it is now a small, self-contained
//! change rather than a missing mechanism. Not done here: the lock table is
//! path-keyed end to end (`flock_release` takes a `&str`), and nothing in tree
//! flocks a hardlink.

use super::*;
use alloc::sync::Arc;

const LOCK_SH: u32 = 1;
const LOCK_EX: u32 = 2;
const LOCK_NB: u32 = 4;
const LOCK_UN: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockKind {
    Shared,
    Exclusive,
}

struct FlockEntry {
    kind: LockKind,
    /// (fd-table identity, fd number). See module docs for what "holder" means here.
    holders: Vec<(usize, u32)>,
}

static FLOCK_TABLE: Spinlock<BTreeMap<String, FlockEntry>> = Spinlock::new(BTreeMap::new());

/// Release whatever lock `(holder, fd)` holds on `path`, if any. A no-op if
/// it holds none — safe to call unconditionally from every fd-teardown path
/// regardless of whether that fd ever called `flock()`.
///
/// Wired into both `sys_close` (explicit `close(2)`) and
/// `SharedFdTable::close_all` (process exit / last-table-drop, via the
/// `runtime().flock_release` hook — `crates/akuma-exec` cannot call this
/// crate directly). Missing either call site would leave a lock dangling
/// forever after its holder is gone, exactly the shape of the pipe
/// `write_count` leaks this codebase has hit before
/// (`crates/akuma-exec/src/process/fd.rs`'s own `close_all` doc comment).
pub fn flock_release(path: &str, holder: usize, fd: u32) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = FLOCK_TABLE.lock();
        if let Some(entry) = table.get_mut(path) {
            entry.holders.retain(|&(h, f)| !(h == holder && f == fd));
            if entry.holders.is_empty() {
                table.remove(path);
            }
        }
    });
}

pub(super) fn sys_flock(fd: u32, operation: u32) -> u64 {
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return ENOSYS;
    };
    let path = match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => f.path,
        // Real Linux supports flock() on more than just regular files
        // (pipes, sockets...); nothing here needs that, so treat it as a
        // trivial always-succeeds lock rather than failing callers that
        // flock() defensively on an fd type we don't model contention for.
        Some(_) => return 0,
        None => return EBADF,
    };
    let holder = Arc::as_ptr(&proc.fds) as usize;
    let nonblock = operation & LOCK_NB != 0;

    match operation & !LOCK_NB {
        LOCK_UN => {
            flock_release(&path, holder, fd);
            0
        }
        op @ (LOCK_SH | LOCK_EX) => {
            let want = if op == LOCK_SH { LockKind::Shared } else { LockKind::Exclusive };
            loop {
                let acquired = akuma_primitives::irq::with_irqs_disabled(|| {
                    let mut table = FLOCK_TABLE.lock();
                    match table.get_mut(&path) {
                        None => {
                            table.insert(path.clone(), FlockEntry { kind: want, holders: alloc::vec![(holder, fd)] });
                            true
                        }
                        Some(entry) => {
                            if entry.holders.iter().any(|&(h, f)| h == holder && f == fd) {
                                // Re-locking (or SH<->EX converting) a fd that already
                                // holds this lock always succeeds, matching flock(2):
                                // a single holder can never deadlock against itself.
                                entry.kind = want;
                                true
                            } else if want == LockKind::Shared && entry.kind == LockKind::Shared {
                                entry.holders.push((holder, fd));
                                true
                            } else {
                                false
                            }
                        }
                    }
                });
                if acquired {
                    return 0;
                }
                if nonblock {
                    return EAGAIN; // EWOULDBLOCK == EAGAIN in the Linux ABI
                }
                if akuma_exec::process::should_interrupt_blocking_syscall() {
                    return EINTR;
                }
                // No waiter/wake list — unlock doesn't wake anyone directly.
                // Bounded re-poll instead, same cadence `sigsuspend`'s wait
                // loop uses (`signal.rs`) for the same reason (a wake here
                // would need its own per-path waiter list for a syscall that,
                // unlike pipes/sockets, is not on any hot path).
                let now = akuma_primitives::clock::uptime_us();
                akuma_exec::threading::schedule_blocking(now + 10_000);
            }
        }
        _ => EINVAL,
    }
}
