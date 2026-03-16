use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use alloc::format;
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
}

static SYSCALL_LOG: Spinlock<BTreeMap<u32, ProcessSyscallLog>> =
    Spinlock::new(BTreeMap::new());

pub(crate) fn record(pid: u32, nr: u64, timestamp_us: u64, duration_us: u64, result: u64) {
    crate::irq::with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        let entry = log.entry(pid).or_insert_with(|| ProcessSyscallLog {
            entries: VecDeque::new(),
            exited_at_us: None,
        });
        if entry.entries.len() >= crate::config::PROC_SYSCALL_LOG_MAX_ENTRIES {
            entry.entries.pop_front();
        }
        entry.entries.push_back(SyscallEntry { timestamp_us, nr, duration_us, result });
    });
}

pub(crate) fn mark_exited(pid: u32) {
    let now = crate::timer::uptime_us();
    crate::irq::with_irqs_disabled(|| {
        let mut log = SYSCALL_LOG.lock();
        if let Some(entry) = log.get_mut(&pid) {
            entry.exited_at_us = Some(now);
        }
    });
}

pub(crate) fn get_formatted(pid: u32) -> Option<Vec<u8>> {
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

        // Check if expired
        if let Some(exited_at) = entry.exited_at_us {
            if now.saturating_sub(exited_at) >= retain_us {
                return None;
            }
        }

        let mut out = format!("# pid={}\n# TIMESTAMP_US       NR  DUR_US  RESULT\n", pid);
        for e in &entry.entries {
            out.push_str(&format!(
                "  {:19}  {:3}  {:6}  {:6}\n",
                e.timestamp_us, e.nr, e.duration_us, e.result
            ));
        }
        Some(out.into_bytes())
    })
}

pub(crate) fn list_pids_with_logs() -> Vec<u32> {
    let now = crate::timer::uptime_us();
    let retain_us = crate::config::PROC_SYSCALL_LOG_RETAIN_MS * 1_000;

    crate::irq::with_irqs_disabled(|| {
        let log = SYSCALL_LOG.lock();
        log.iter()
            .filter(|(_, v)| {
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
