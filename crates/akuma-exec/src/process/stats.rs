use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::fmt::Write;

use crate::runtime::{runtime, with_irqs_disabled};
use crate::process::types::Pid;
use crate::process::table::PROCESS_TABLE;
use crate::process::children::lookup_process;

static PROCESS_SYSCALL_STATS_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable_process_syscall_stats(enabled: bool) {
    PROCESS_SYSCALL_STATS_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn process_syscall_stats_enabled() -> bool {
    PROCESS_SYSCALL_STATS_ENABLED.load(Ordering::Relaxed)
}

pub struct ProcessSyscallStats {
    counts: [AtomicU64; Self::MAX_NR],
    times_us: [AtomicU64; Self::MAX_NR],
    pub pagefaults: AtomicU64,
    pub pagefault_pages: AtomicU64,
}

impl ProcessSyscallStats {
    const MAX_NR: usize = 512;

    pub const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; Self::MAX_NR],
            times_us: [const { AtomicU64::new(0) }; Self::MAX_NR],
            pagefaults: AtomicU64::new(0),
            pagefault_pages: AtomicU64::new(0),
        }
    }

    pub fn inc(&self, nr: u64) {
        let idx = nr as usize;
        if idx < Self::MAX_NR {
            self.counts[idx].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn add_time_us(&self, nr: u64, us: u64) {
        let idx = nr as usize;
        if idx < Self::MAX_NR {
            self.times_us[idx].fetch_add(us, Ordering::Relaxed);
        }
    }

    pub fn inc_pagefault(&self, pages: u64) {
        self.pagefaults.fetch_add(1, Ordering::Relaxed);
        self.pagefault_pages.fetch_add(pages, Ordering::Relaxed);
    }

    pub fn dump(&self, pid: Pid, name: &str, elapsed_us: u64) {
        let mut total: u64 = 0;
        let mut total_time_us: u64 = 0;
        let mut entries: Vec<(usize, u64, u64)> = Vec::new();
        for i in 0..Self::MAX_NR {
            let c = self.counts[i].load(Ordering::Relaxed);
            if c > 0 {
                let t = self.times_us[i].load(Ordering::Relaxed);
                total += c;
                total_time_us += t;
                entries.push((i, c, t));
            }
        }
        if total == 0 { return; }

        // Sort by time spent (descending) — shows the slowest syscalls first
        entries.sort_by(|a, b| b.2.cmp(&a.2));

        let secs = elapsed_us / 1_000_000;
        let frac = (elapsed_us % 1_000_000) / 10_000;
        let rate = if elapsed_us > 0 { total * 1_000_000 / elapsed_us } else { 0 };
        let (pmm_total, _pmm_alloc, pmm_free) = (runtime().pmm_stats)();
        let pf = self.pagefaults.load(Ordering::Relaxed);
        let pf_pg = self.pagefault_pages.load(Ordering::Relaxed);

        let mut top = String::new();
        for (i, (nr, count, time)) in entries.iter().enumerate() {
            if i > 0 { top.push(' '); }
            let sname = syscall_name(*nr);
            let time_ms = *time / 1000;
            if sname.is_empty() {
                let _ = write!(&mut top, "nr{}={}({}ms)", nr, count, time_ms);
            } else {
                let _ = write!(&mut top, "{}={}({}ms)", sname, count, time_ms);
            }
            if i >= 9 { break; }
        }

        let total_time_ms = total_time_us / 1000;
        let msg = format!(
            "[PSTATS] PID {} ({}) {}.{:02}s: {} syscalls ({}/s) in_kernel={}ms pmm={}free/{}tot pgfault={}({}pg) | {}\n",
            pid, name, secs, frac, total, rate, total_time_ms,
            pmm_free, pmm_total, pf, pf_pg, top,
        );
        (runtime().print_str)(&msg);
    }
}

pub fn dump_running_process_stats() {
    if !process_syscall_stats_enabled() { return; }
    let pids: Vec<(Pid, String, u64)> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table.iter()
            .filter(|(_, p)| !p.exited && p.start_time_us > 0)
            .map(|(&pid, p)| (pid, p.name.clone(), p.start_time_us))
            .collect()
    });
    let now = (runtime().uptime_us)();
    for (pid, name, start_us) in pids {
        let elapsed = now.saturating_sub(start_us);
        if elapsed < 10_000_000 { continue; } // skip processes running < 10s
        if let Some(proc) = lookup_process(pid) {
            proc.syscall_stats.dump(pid, &name, elapsed);
        }
    }
}

pub fn syscall_name(nr: usize) -> &'static str {
    match nr {
        0 => "io_setup", 29 => "ioctl", 46 => "ftruncate",
        48 => "faccessat", 56 => "openat", 57 => "close",
        59 => "pipe2", 61 => "getdents64", 62 => "lseek",
        63 => "read", 64 => "write", 65 => "readv",
        66 => "writev", 67 => "pread64", 68 => "pwrite64",
        72 => "pselect6", 73 => "ppoll",
        78 => "readlinkat", 79 => "fstatat", 80 => "fstat",
        93 => "exit", 94 => "exit_group",
        96 => "set_tid_address", 98 => "futex",
        99 => "set_robust_list",
        113 => "clock_gettime", 115 => "clock_nanosleep",
        124 => "sched_yield",
        130 => "tkill", 131 => "tgkill",
        134 => "rt_sigaction", 135 => "rt_sigprocmask",
        160 => "uname", 167 => "prctl",
        172 => "getpid", 174 => "getuid", 175 => "geteuid",
        176 => "getgid", 177 => "getegid", 178 => "gettid",
        198 => "socket", 200 => "bind", 201 => "listen",
        202 => "accept", 203 => "connect",
        204 => "getsockname", 205 => "getpeername",
        206 => "sendto", 207 => "recvfrom",
        208 => "setsockopt", 209 => "getsockopt",
        210 => "shutdown",
        214 => "brk",
        215 => "munmap", 216 => "mremap", 222 => "mmap",
        226 => "mprotect", 233 => "madvise",
        220 => "clone", 221 => "execve",
        260 => "wait4",
        261 => "prlimit64",
        278 => "getrandom",
        281 => "memfd_create",
        282 => "membarrier",
        20 => "epoll_create1", 21 => "epoll_ctl", 22 => "epoll_pwait",
        25 => "fcntl",
        26 => "inotify_init1", 27 => "inotify_add_watch",
        35 => "unlinkat",
        85 => "timerfd_create", 86 => "timerfd_settime",
        19 => "eventfd2",
        34 => "mkdirat", 45 => "truncate",
        291 => "statx",
        435 => "clone3", 439 => "faccessat2",
        _ => "",
    }
}
