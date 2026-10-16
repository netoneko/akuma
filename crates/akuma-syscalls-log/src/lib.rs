//! Per-pid syscall trace rings — the data behind `/proc/<pid>/syscalls`.
//!
//! # Why this is a crate
//!
//! Not for host tests, and not to localise `unsafe` (it has none). It moved out
//! of `src/syscall/` on 2026-09-01 to **break a dependency cycle**:
//! `src/vfs/proc.rs` read `crate::syscall::log`, and `src/syscall/` reads 37
//! symbols back out of `src/vfs/`. Cargo crates cannot be mutually dependent, so
//! that loop had to be cut before either directory could leave the binary
//! (`docs/archive/SRC_SYSCALL_EXTRACTION.md`, Blocker 1).
//!
//! It sits below both: the syscall epilogue writes, `/proc` reads, and neither
//! needs to know about the other.
//!
//! # The retained-log hazard this type exists to contain
//!
//! **A log outlives the process that wrote it** — that is the entire point, so
//! `/proc/<pid>/syscalls` still answers after the process is gone. Two
//! consequences drive the shape of the code below, and both were real bugs:
//!
//! 1. The process table cannot be asked who owned a retained log, so the owning
//!    box is recorded in the entry. Without it the isolation check had nothing
//!    to consult once the process exited and **fell open** — a container read
//!    `/proc/1/syscalls`.
//! 2. A pid can be recycled while the previous occupant's log is still retained.
//!    [`record`] therefore clears on a box change rather than appending, or a
//!    container handed a recycled pid inherits a host process's trace.
//!
//! Visibility is decided in [`get_formatted`], never by callers: the file is
//! reachable through four `ProcFilesystem` methods, and when each carried its
//! own copy of the rule only one had it — `read_file`, the one a `read()` never
//! reaches (`docs/archive/DEVBOX_ISSUES.md` Issue 24).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec::Vec;
use core::fmt::Write as _;
use spinning_top::Spinlock;

use akuma_primitives::clock::uptime_us;
use akuma_primitives::irq::with_irqs_disabled;

/// Ring depth per pid, from `akuma_config`. Read as a `const`, not handed over:
/// see that crate's header for why the four `…Config` structs this file used to
/// carry were a mistake.
fn max_entries() -> usize {
    akuma_config::PROC_SYSCALL_LOG_MAX_ENTRIES
}

fn retain_us() -> u64 {
    akuma_config::PROC_SYSCALL_LOG_RETAIN_MS * 1_000
}

struct SyscallEntry {
    timestamp_us: u64,
    nr: u64,
    duration_us: u64,
    result: u64,
}

struct ProcessSyscallLog {
    entries: VecDeque<SyscallEntry>,
    exited_at_us: Option<u64>,
    /// The box that produced these entries.
    ///
    /// The log outlives the process — that is its whole point — so the process
    /// table cannot be asked who owned a retained log. Without this recorded
    /// here, the isolation check had nothing to consult once the process was
    /// gone and fell open, which is how a container came to read `/proc/1/syscalls`.
    box_id: u64,
}

static SYSCALL_LOG: Spinlock<BTreeMap<u32, ProcessSyscallLog>> =
    Spinlock::new(BTreeMap::new());

pub fn record(pid: u32, box_id: u64, nr: u64, timestamp_us: u64, duration_us: u64, result: u64) {
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        let entry = log.entry(pid).or_insert_with(|| ProcessSyscallLog {
            entries: VecDeque::new(),
            exited_at_us: None,
            box_id,
        });
        // A different box writing under the same pid means the pid was recycled
        // while the previous occupant's log was still retained. Start over rather
        // than append: otherwise the new process inherits the old one's trace, and
        // a container handed a recycled pid would inherit a host process's.
        if entry.box_id != box_id {
            entry.entries.clear();
            entry.exited_at_us = None;
            entry.box_id = box_id;
        }
        // A syscall recorded for a pid already marked exited is a *new* process
        // that was handed the pid: start its log over, as for a different box.
        if entry.exited_at_us.take().is_some() {
            entry.entries.clear();
        }
        if entry.entries.len() >= max_entries() {
            entry.entries.pop_front();
        }
        entry.entries.push_back(SyscallEntry { timestamp_us, nr, duration_us, result });
    });
}

/// Drop every log whose retention window has closed. Run from every place that
/// already walks the map (`mark_exited`, `list_pids_with_logs`) so the map is
/// bounded by "processes that exited in the last `retain_us`" plus live ones —
/// it used to be pruned only inside `get_formatted`, which nothing calls unless
/// someone reads `/proc/<pid>/syscalls`, so it grew for the life of the boot
/// (one entry per process ever run, ~2 KB each, and every one of them listed in
/// `/proc`).
fn prune_expired(log: &mut BTreeMap<u32, ProcessSyscallLog>, now: u64) {
    log.retain(|_, v| match v.exited_at_us {
        Some(exited_at) => now.saturating_sub(exited_at) < retain_us(),
        None => true,
    });
}

pub fn mark_exited(pid: u32) {
    let now = uptime_us();
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        if let Some(entry) = log.get_mut(&pid) {
            entry.exited_at_us = Some(now);
        }
        prune_expired(&mut log, now);
    });
}

/// Render `pid`'s retained log, or `None` if there is none, it has expired, or
/// `viewer_box_id` is not allowed to see it.
///
/// **Visibility is decided here, not by the callers.** `/proc/<pid>/syscalls` is
/// reachable through four `ProcFilesystem` methods, and when each carried its own
/// copy of the rule only one of them actually had it — and it was `read_file`,
/// the one a `read()` never reaches (docs/archive/DEVBOX_ISSUES.md Issue 24).
#[must_use]
pub fn get_formatted(pid: u32, viewer_box_id: u64) -> Option<Vec<u8>> {
    let now = uptime_us();
    
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();

        // Lazily remove expired entries
        log.retain(|_, v| {
            if let Some(exited_at) = v.exited_at_us {
                now.saturating_sub(exited_at) < retain_us()
            } else {
                true
            }
        });

        let entry = log.get(&pid)?;

        // Box 0 is the host and sees every log; a box sees only its own.
        if viewer_box_id != 0 && entry.box_id != viewer_box_id {
            return None;
        }

        // Check if expired
        if let Some(exited_at) = entry.exited_at_us
            && now.saturating_sub(exited_at) >= retain_us() {
                return None;
            }

        let mut out = format!("# pid={pid}\n# TIMESTAMP_US       NR  DUR_US  RESULT\n");
        for e in &entry.entries {
            let _ = writeln!(out,
                "  {:19}  {:3}  {:6}  {:6}",
                e.timestamp_us, e.nr, e.duration_us, e.result
            );
        }
        Some(out.into_bytes())
    })
}

/// Whether [`get_formatted`] would return `Some`, without rendering.
///
/// The same retention and box rule. `/proc`'s structure (`exists`, `stat`, listing a
/// pid directory) asks this on every lookup; rendering the whole log to throw
/// it away allocated a few KB, under this spinlock with IRQs off, per `stat`.
#[must_use]
pub fn has_log(pid: u32, viewer_box_id: u64) -> bool {
    let now = uptime_us();
    with_irqs_disabled(|| {
        let log = SYSCALL_LOG.lock();
        log.get(&pid).is_some_and(|entry| {
            (viewer_box_id == 0 || entry.box_id == viewer_box_id)
                && entry
                    .exited_at_us
                    .is_none_or(|exited_at| now.saturating_sub(exited_at) < retain_us())
        })
    })
}

/// Every pid with a live retained log that `viewer_box_id` may see.
#[must_use]
pub fn list_pids_with_logs(viewer_box_id: u64) -> Vec<u32> {
    let now = uptime_us();
    
    with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        prune_expired(&mut log, now);
        log.iter()
            .filter(|(_, v)| viewer_box_id == 0 || v.box_id == viewer_box_id)
            .map(|(pid, _)| *pid)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_of(exits: &[(u32, Option<u64>)]) -> BTreeMap<u32, ProcessSyscallLog> {
        exits
            .iter()
            .map(|&(pid, exited_at_us)| {
                (pid, ProcessSyscallLog { entries: VecDeque::new(), exited_at_us, box_id: 0 })
            })
            .collect()
    }

    /// `has_log` is `get_formatted(..).is_some()` without the render: the same
    /// box rule and the same retention rule, or `/proc` lists a `syscalls` file
    /// that then reads as absent. Pids far above any real ones, because the map
    /// is a process-wide static and `cargo test` runs tests in threads.
    #[test]
    fn has_log_agrees_with_get_formatted() {
        record(900_001, 0, 63, 1, 1, 0);
        record(900_002, 7, 63, 1, 1, 0);
        for pid in [900_001, 900_002, 900_003] {
            for viewer in [0, 7, 8] {
                assert_eq!(
                    has_log(pid, viewer),
                    get_formatted(pid, viewer).is_some(),
                    "pid {pid} viewer {viewer}"
                );
            }
        }
        assert!(has_log(900_002, 7) && !has_log(900_002, 8) && has_log(900_002, 0));
    }

    #[test]
    fn prune_drops_only_expired_exits() {
        let retain = retain_us();
        // pid 1 live; pid 2 exited long ago; pid 3 exited just now; pid 4 exactly at the edge.
        let now = 10 * retain;
        let mut log = log_of(&[
            (1, None),
            (2, Some(now - 2 * retain)),
            (3, Some(now - 1)),
            (4, Some(now - retain)),
        ]);
        prune_expired(&mut log, now);
        let kept: alloc::vec::Vec<u32> = log.keys().copied().collect();
        assert_eq!(kept, [1, 3], "live and recently-exited stay; expired (>= retain) go");
    }

    #[test]
    fn a_live_log_is_never_pruned() {
        let mut log = log_of(&[(7, None), (8, None)]);
        prune_expired(&mut log, u64::MAX);
        assert_eq!(log.len(), 2);
    }
}
