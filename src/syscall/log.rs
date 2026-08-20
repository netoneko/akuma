use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use alloc::format;
use core::fmt::Write as _;
use spinning_top::Spinlock;

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
    crate::irq::with_irqs_disabled(|| {
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
        if entry.entries.len() >= crate::config::PROC_SYSCALL_LOG_MAX_ENTRIES {
            entry.entries.pop_front();
        }
        entry.entries.push_back(SyscallEntry { timestamp_us, nr, duration_us, result });
    });
}

pub fn mark_exited(pid: u32) {
    let now = crate::timer::uptime_us();
    crate::irq::with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        if let Some(entry) = log.get_mut(&pid) {
            entry.exited_at_us = Some(now);
        }
    });
}

/// Render `pid`'s retained log, or `None` if there is none, it has expired, or
/// `viewer_box_id` is not allowed to see it.
///
/// **Visibility is decided here, not by the callers.** `/proc/<pid>/syscalls` is
/// reachable through four `ProcFilesystem` methods, and when each carried its own
/// copy of the rule only one of them actually had it — and it was `read_file`,
/// the one a `read()` never reaches (docs/archive/DEVBOX_ISSUES.md Issue 24).
pub fn get_formatted(pid: u32, viewer_box_id: u64) -> Option<Vec<u8>> {
    let now = crate::timer::uptime_us();
    let retain_us = crate::config::PROC_SYSCALL_LOG_RETAIN_MS * 1_000;

    crate::irq::with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();

        // Lazily remove expired entries
        log.retain(|_, v| {
            if let Some(exited_at) = v.exited_at_us {
                now.saturating_sub(exited_at) < retain_us
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
            && now.saturating_sub(exited_at) >= retain_us {
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

/// Every pid with a live retained log that `viewer_box_id` may see.
pub fn list_pids_with_logs(viewer_box_id: u64) -> Vec<u32> {
    let now = crate::timer::uptime_us();
    let retain_us = crate::config::PROC_SYSCALL_LOG_RETAIN_MS * 1_000;

    crate::irq::with_irqs_disabled(|| {
        let log = SYSCALL_LOG.lock();
        log.iter()
            .filter(|(_, v)| {
                if viewer_box_id != 0 && v.box_id != viewer_box_id {
                    return false;
                }
                if let Some(exited_at) = v.exited_at_us {
                    now.saturating_sub(exited_at) < retain_us
                } else {
                    true
                }
            })
            .map(|(pid, _)| *pid)
            .collect()
    })
}
