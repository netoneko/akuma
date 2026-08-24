//! Process Filesystem (procfs)
//!
//! A virtual filesystem that exposes process stdin/stdout as files.
//! Mounted at /proc, provides:
//! - /proc/<pid>/fd/0 - stdin (readable by all, writable by spawner/kernel)
//! - /proc/<pid>/fd/1 - stdout (readable by all, writable by owning process)
//! - /proc/<pid>/cmdline - argv as NUL-separated bytes (Linux-compatible)
//! - /proc/<pid>/status - human-readable `Name`, `State`, `Pid`, `PPid`, etc.
//!
//! # TODO: Rewrite without allocations
//!
//! The current implementation uses `format!`, `String`, and `Vec` allocations
//! in `read_symlink()` and `is_symlink()`. These can deadlock if called while
//! the allocator lock is held (e.g., during exception handling or with
//! preemption disabled). Should be rewritten to use fixed-size stack buffers
//! or return references to static strings where possible.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::fmt::Write as _;

use super::{DirEntry, Filesystem, FsError, FsStats, Metadata};
use crate::config::PROC_STDOUT_MAX_SIZE;
use akuma_exec::process::{self, Pid, ProcessState};

// ============================================================================
// /proc/<pid>/cmdline + status (Linux-style)
// ============================================================================

/// Rewrite a leading `self/` to the calling process's own pid.
///
/// `/proc/self` is a symlink on Linux, and everything under it resolves through
/// that symlink. Akuma's VFS hands procfs the literal path instead of chasing it,
/// so `/proc/self/status` — a path essentially every libc and tool uses — reached
/// this filesystem as the string `self/status` and matched nothing. `read_symlink`
/// reported `self` correctly all along, which is why the gap was easy to miss.
///
/// The consequences were not cosmetic. It is the reason `redis-server` could not
/// start at all (`/proc/self/smaps`,
/// docs/archive/LONG_ROAD_TO_REDIS.md), and later the reason `redis:alpine`'s
/// entrypoint died at `setpriv: activate capabilities: No error information` —
/// libcap-ng reads a process's capabilities out of `/proc/self/status` and returns
/// -1 *without setting errno* when it cannot.
///
/// The bare string `self` is deliberately NOT rewritten: it must stay a symlink so
/// `readlink("/proc/self")` keeps working.
fn resolve_self(path: &str) -> alloc::borrow::Cow<'_, str> {
    match path.strip_prefix("self/") {
        Some(rest) => match akuma_exec::process::read_current_pid() {
            Some(pid) => alloc::borrow::Cow::Owned(format!("{pid}/{rest}")),
            // No current process (kernel thread): leave it alone and let the
            // caller's normal not-found path report it.
            None => alloc::borrow::Cow::Borrowed(path),
        },
        None => alloc::borrow::Cow::Borrowed(path),
    }
}

/// The box the calling process belongs to; 0 (the host) when there is no current
/// process, which is the kernel-thread case.
///
/// Every `ProcFilesystem` method needs this, and several used to recompute it
/// inline while others simply forgot to — `/proc/<pid>/syscalls` was reachable
/// through four of them and only one applied the rule. Reaching for this helper
/// is the cheap habit that keeps the four in step.
fn caller_box_id() -> u64 {
    akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id)
}

/// Render a `/proc/mounts` listing for the **read** paths. The mount
/// spinlocks are held only inside `crate::vfs::render_mounts`, which writes
/// into the stack buffer below allocation-free; the `.to_vec()` copy happens
/// after the locks are gone and exists only because the `Filesystem` trait's
/// `read_file` returns `Vec<u8>` — the same contract every procfs virtual
/// file (`boxes`, `net/tcp`, …) already lives with. `metadata` and `exists`
/// use `render_mounts` directly and do not go through here.
///
/// `target_pid` selects whose mount set is listed; `None` (or an unknown
/// pid) means the global table. Returns `None` when the viewer may not see
/// the target (box rule) — callers turn that into `NotFound`.
pub fn mounts_bytes(target_pid: Option<Pid>, viewer_box_id: u64) -> Option<Vec<u8>> {
    if let Some(pid) = target_pid {
        let proc = process::lookup_process_shared(pid)?;
        if viewer_box_id != 0 && proc.box_id != viewer_box_id {
            return None;
        }
    }
    let mut buf = [0u8; 2048];
    let n = crate::vfs::render_mounts(viewer_box_id, target_pid, &mut buf);
    Some(buf[..n].to_vec())
}

fn proc_cmdline_bytes(p: &process::Process) -> Vec<u8> {
    if p.args.is_empty() {
        let mut v: Vec<u8> = p.name.as_bytes().to_vec();
        v.push(0);
        return v;
    }
    let mut out = Vec::new();
    for a in &p.args {
        out.extend_from_slice(a.as_bytes());
        out.push(0);
    }
    out
}

/// The capability mask a full-root process reports: every capability Linux
/// defines up to `CAP_LAST_CAP`.
///
/// This kernel has no capability model — everything runs as root — but the
/// `Cap*` lines still have to be here, because that is where **libcap-ng** reads
/// a process's capabilities from (`capng_get_caps_process` parses
/// `/proc/self/status`, it does not call `capget`). With the lines absent it
/// fails, and `capng_apply` then returns -1 *without setting errno* — which is
/// exactly the `setpriv: activate capabilities: No error information` that
/// killed `redis:alpine`'s entrypoint. Stubbing `capset(2)` did not help,
/// because `capset` was never the call that failed.
const CAP_FULL_MASK: &str = "000001ffffffffff";

fn proc_status_text(p: &process::Process) -> String {
    let name = p.name.as_str();
    let name_field = if name.len() > 15 { &name[..15] } else { name };
    let state = match p.state {
        ProcessState::Zombie(_) => "Z (zombie)",
        ProcessState::Ready | ProcessState::Running => "R (running)",
        ProcessState::Blocked => "S (sleeping)",
    };
    // One format for every state — the four arms this replaced were identical
    // apart from the state string and the zombie's trailing ExitCode, so adding
    // a field meant remembering to add it four times.
    let mut out = format!(
        "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\n\
         Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nFDSize:\t256\nGroups:\t\n\
         VmPeak:\t0 kB\nVmSize:\t0 kB\nVmRSS:\t0 kB\nThreads:\t1\n\
         CapInh:\t0000000000000000\nCapPrm:\t{}\nCapEff:\t{}\nCapBnd:\t{}\n\
         CapAmb:\t0000000000000000\nNoNewPrivs:\t0\nSeccomp:\t0\n",
        name_field, state, p.pid, p.pid, p.parent_pid,
        CAP_FULL_MASK, CAP_FULL_MASK, CAP_FULL_MASK,
    );
    if let ProcessState::Zombie(code) = p.state {
        use core::fmt::Write as _;
        let _ = writeln!(out, "ExitCode:\t{code}");
    }
    out
}

/// `/proc/<pid>/stat` (Linux-compatible, space-separated, single line).
///
/// This is the file `ps`/`top` actually parse (not `status`) — busybox's
/// `ps` applet showed an empty process list despite `status`/`cmdline` being
/// populated because this file didn't exist at all. Accounting fields we
/// don't track per-process yet (times, memory, scheduling) read as 0/a
/// neutral placeholder — enough for `ps`/`top` to list PID/PPID/STATE/
/// COMMAND, which is what they read this file for; real per-process CPU/mem
/// stats are a separate, larger feature.
fn proc_stat_text(p: &process::Process) -> String {
    let name = p.name.as_str();
    // Linux's `comm` is the executable's basename, truncated to 15 bytes.
    let base = name.rsplit('/').next().unwrap_or(name);
    let comm = if base.len() > 15 { &base[..15] } else { base };
    let state = match p.state {
        ProcessState::Zombie(_) => 'Z',
        ProcessState::Blocked => 'S',
        ProcessState::Ready | ProcessState::Running => 'R',
    };
    let pid = p.pid;
    let ppid = p.parent_pid;
    // pid comm state ppid pgrp session tty_nr tpgid flags minflt cminflt
    // majflt cmajflt utime stime cutime cstime priority nice num_threads
    // itrealvalue starttime vsize rss rsslim startcode endcode startstack
    // kstkesp kstkeip signal blocked sigignore sigcatch wchan nswap cnswap
    // exit_signal processor rt_priority policy delayacct_blkio_ticks
    // guest_time cguest_time
    format!(
        "{pid} ({comm}) {state} {ppid} {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
    )
}

// ============================================================================
// ProcFilesystem
// ============================================================================

/// Virtual filesystem for process information
pub struct ProcFilesystem;

impl ProcFilesystem {
    /// Create a new procfs instance
    pub fn new() -> Self {
        Self
    }

    /// Parse a path like "/<pid>/fd/<n>" into (pid, fd_num)
    fn parse_fd_path(path: &str) -> Result<(Pid, u32), FsError> {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        // Expected: ["<pid>", "fd", "<n>"]
        if parts.len() != 3 || parts[1] != "fd" {
            return Err(FsError::NotFound);
        }

        let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
        let fd_num: u32 = parts[2].parse().map_err(|_| FsError::NotFound)?;

        Ok((pid, fd_num))
    }

    /// Parse path to get just the PID (for /<pid> or /<pid>/fd)
    fn parse_pid_path(path: &str) -> Result<Pid, FsError> {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        if parts.is_empty() || parts[0].is_empty() {
            return Err(FsError::NotFound);
        }

        parts[0].parse().map_err(|_| FsError::NotFound)
    }

    /// Whether `viewer_box_id` may see process `pid` at all.
    ///
    /// Box 0 (the host) sees every process; a box sees only its own. This
    /// replaced a bare `process_exists`, which answered "is there such a pid"
    /// and was used at eight sites as if it also answered "and may I see it" —
    /// so `ls /proc/<host-pid>` from inside a container listed a host process's
    /// `fd`/`cmdline`/`status`/`stat` entries, and `exists("/proc/2")` said yes.
    /// The individual files were refused on read, so the leak was structure and
    /// existence rather than content, but it is the same rule either way
    /// (docs/archive/DEVBOX_ISSUES.md Issue 24).
    fn process_visible(pid: Pid, viewer_box_id: u64) -> bool {
        match process::lookup_process_shared(pid) {
            Some(proc) => viewer_box_id == 0 || proc.box_id == viewer_box_id,
            None => false,
        }
    }

    /// Returns a human-readable description string for an fd entry (used by readlinkat).
    /// These strings are NOT necessarily valid filesystem paths.
    pub fn fd_description(fd_entry: &akuma_exec::process::FileDescriptor, fd: u32) -> String {
        use akuma_exec::process::FileDescriptor;
        match fd_entry {
            FileDescriptor::File(f) => f.path.clone(),
            FileDescriptor::Socket(_) => format!("socket:[{fd}]"),
            FileDescriptor::PipeRead(id) => format!("pipe:[{id}]"),
            FileDescriptor::PipeWrite(id) => format!("pipe:[{id}]"),
            FileDescriptor::UnixSocket { rx, .. } => format!("socket:[{rx}]"),
            FileDescriptor::EpollFd(_) => String::from("anon_inode:[eventpoll]"),
            FileDescriptor::TimerFd(_) => String::from("anon_inode:[timerfd]"),
            FileDescriptor::EventFd(_) => String::from("anon_inode:[eventfd]"),
            FileDescriptor::DevNull => String::from("/dev/null"),
            FileDescriptor::DevUrandom => String::from("/dev/urandom"),
            FileDescriptor::DevZero => String::from("/dev/zero"),
            FileDescriptor::DevDsp => String::from("/dev/dsp"),
            FileDescriptor::Tap { .. } => String::from("/dev/net/tap0"),
            FileDescriptor::RumpSocket { rump_fd, .. } => format!("socket:[rump:{rump_fd}]"),
            FileDescriptor::Stdin => String::from("/dev/stdin"),
            FileDescriptor::Stdout => String::from("/dev/stdout"),
            FileDescriptor::Stderr => String::from("/dev/stderr"),
            FileDescriptor::ChildStdout(child_pid) => format!("pipe:[child:{child_pid}]"),
            FileDescriptor::PidFd(id) => format!("anon_inode:[pidfd:{id}]"),
        }
    }

    /// Permission-check and dispatch a write to `/proc/<pid>/fd/<n>`, returning
    /// how many bytes were **accepted**.
    ///
    /// Shared by `write_file` and `write_at` so the two cannot drift on the
    /// isolation and ownership checks. The count matters for fd 0: the target's
    /// stdin buffer is bounded and no longer drops buffered bytes to make room,
    /// so a full buffer produces a short write (possibly 0) that the caller must
    /// carry back to userspace to retry.
    fn write_fd_data(path: &str, data: &[u8]) -> Result<usize, FsError> {
        let (target_pid, fd_num) = Self::parse_fd_path(path)?;
        let caller_proc = akuma_exec::process::current_process_shared();
        let caller_pid = process::read_current_pid();
        let caller_box_id = caller_proc.as_ref().map_or(0, |p| p.box_id);

        let target = process::lookup_process_shared(target_pid).ok_or(FsError::NotFound)?;

        // BOX ISOLATION: Box N only sees its own processes.
        if caller_box_id != 0 && target.box_id != caller_box_id {
            return Err(FsError::NotFound);
        }

        match fd_num {
            0 => {
                // stdin: allow if caller is spawner, or if kernel-spawned (spawner_pid == None)
                if let Some(spawner) = target.spawner_pid {
                    // Process was spawned by another process
                    if caller_pid != Some(spawner) {
                        return Err(FsError::PermissionDenied);
                    }
                }
                // If spawner_pid is None (kernel spawned), allow any caller
                // If caller_pid is None (kernel context), also allow

                // Use the unified helper to write to both legacy buffer and ProcessChannel
                process::write_to_process_stdin(target_pid, data).map_err(|_| FsError::Internal)
            }
            1 => {
                // stdout: only owning process can write
                if caller_pid != Some(target_pid) {
                    return Err(FsError::PermissionDenied);
                }

                // Write with size limit (thread-safe via Spinlock)
                target.stdout.lock().write_with_limit(data, PROC_STDOUT_MAX_SIZE);
                Ok(data.len())
            }
            _ => Err(FsError::NotFound),
        }
    }
}

impl Default for ProcFilesystem {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the readlinkat description string for a procfs fd path like "/proc/<pid>/fd/<n>".
/// Unlike `read_symlink`, this returns descriptions for ALL fd types (pipes, sockets, etc.),
/// not just File fds. Used by the readlinkat syscall to report virtual fd targets.
pub fn proc_fd_description(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    // Strip leading "proc/" if present
    let path = path.strip_prefix("proc/").unwrap_or(path);

    // Handle "self/fd/<n>"
    let path = if let Some(rest) = path.strip_prefix("self/") {
        let pid = process::current_process_shared().map(|p| p.pid)?;
        alloc::format!("{pid}/{rest}")
    } else {
        alloc::string::String::from(path)
    };

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() == 3 && parts[1] == "fd" {
        let pid: Pid = parts[0].parse().ok()?;
        let fd: u32 = parts[2].parse().ok()?;
        let proc = process::lookup_process_shared(pid)?;
        let fd_entry = proc.get_fd(fd)?;
        Some(ProcFilesystem::fd_description(&fd_entry, fd))
    } else {
        None
    }
}

impl Filesystem for ProcFilesystem {
    fn name(&self) -> &'static str {
        "proc"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let path = resolve_self(path.trim_matches('/'));
        let path = path.as_ref();
        let current_box_id = akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);

        if path.is_empty() {
            // Root: list all process PIDs as directories, filtered by box_id
            let processes = process::list_processes();
            let mut entries: Vec<DirEntry> = processes
                .into_iter()
                .filter(|p| {
                    // Box 0 (Host) sees everything. Box N only sees its own.
                    if current_box_id == 0 {
                        true
                    } else if let Some(proc) = process::lookup_process_shared(p.pid) {
                        proc.box_id == current_box_id
                    } else {
                        false
                    }
                })
                .map(|p| DirEntry {
                    name: format!("{}", p.pid),
                    is_dir: true,
                    is_symlink: false,
                    size: 0,
                })
                .collect();

            // Host context only: add "boxes" virtual file
            if current_box_id == 0 {
                entries.push(DirEntry {
                    name: String::from("boxes"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
                // Multikernel: per-core state table (one row per CPU; see read_file).
                entries.push(DirEntry {
                    name: String::from("cores"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
            }

            // Everyone sees "net" directory
            entries.push(DirEntry {
                name: String::from("net"),
                is_dir: true,
                is_symlink: false,
                size: 0,
            });

            // Mount listing + supported filesystems. Rendered per-viewer: a
            // boxed process sees its own namespace's mounts with no source
            // column (crate::vfs::render_mounts).
            entries.push(DirEntry {
                name: String::from("mounts"),
                is_dir: false,
                is_symlink: false,
                size: 0,
            });
            entries.push(DirEntry {
                name: String::from("filesystems"),
                is_dir: false,
                is_symlink: false,
                size: 0,
            });

            // Add recently-exited PIDs that still have retained syscall logs.
            // This runs AFTER the box filter above and must repeat it — appending
            // the unfiltered list here is what put every host pid into a
            // container's `ls /proc`, while `ps` looked correct because it stats
            // each entry and the foreign ones are refused.
            if crate::config::PROC_SYSCALL_LOG_ENABLED {
                let logged_pids = crate::syscall::log::list_pids_with_logs(current_box_id);
                let live_pids: alloc::collections::BTreeSet<u32> = entries.iter()
                    .filter_map(|e| e.name.parse::<u32>().ok())
                    .collect();
                for pid in logged_pids {
                    if !live_pids.contains(&pid) {
                        entries.push(DirEntry {
                            name: format!("{pid}"),
                            is_dir: true,
                            is_symlink: false,
                            size: 0,
                        });
                    }
                }
            }

            // sysvipc directory
            if crate::config::PROC_SYSVIPC_ENABLED {
                entries.push(DirEntry {
                    name: String::from("sysvipc"),
                    is_dir: true,
                    is_symlink: false,
                    size: 0,
                });
            }

            return Ok(entries);
        }

        let parts: Vec<&str> = path.split('/').collect();

        if parts.len() == 1 {
            if parts[0] == "net" {
                return Ok(alloc::vec![
                    DirEntry { name: String::from("dev"), is_dir: false, is_symlink: false, size: 0 },
                    DirEntry { name: String::from("tcp"), is_dir: false, is_symlink: false, size: 0 },
                    DirEntry { name: String::from("udp"), is_dir: false, is_symlink: false, size: 0 },
                ]);
            }

            if parts[0] == "sysvipc" && crate::config::PROC_SYSVIPC_ENABLED {
                return Ok(alloc::vec![
                    DirEntry { name: String::from("msg"), is_dir: false, is_symlink: false, size: 0 },
                ]);
            }

            // /<pid> - list "fd" directory and optionally "syscalls"
            let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
            // Process may have exited but still have a retained log
            let pid_has_log = crate::config::PROC_SYSCALL_LOG_ENABLED
                && crate::syscall::log::get_formatted(pid, current_box_id).is_some();
            if !Self::process_visible(pid, current_box_id) && !pid_has_log {
                return Err(FsError::NotFound);
            }
            let mut pid_entries = alloc::vec![];
            if Self::process_visible(pid, current_box_id) {
                pid_entries.push(DirEntry {
                    name: String::from("fd"),
                    is_dir: true,
                    is_symlink: false,
                    size: 0,
                });
                pid_entries.push(DirEntry {
                    name: String::from("cmdline"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
                pid_entries.push(DirEntry {
                    name: String::from("status"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
                pid_entries.push(DirEntry {
                    name: String::from("stat"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
                pid_entries.push(DirEntry {
                    name: String::from("mounts"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
            }
            if crate::config::PROC_SYSCALL_LOG_ENABLED
                && crate::syscall::log::get_formatted(pid, current_box_id).is_some()
            {
                pid_entries.push(DirEntry {
                    name: String::from("syscalls"),
                    is_dir: false,
                    is_symlink: false,
                    size: 0,
                });
            }
            return Ok(pid_entries);
        }

        if parts.len() == 2 && parts[1] == "fd" {
            // /<pid>/fd - list all open file descriptors
            let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
            if !Self::process_visible(pid, current_box_id) {
                return Err(FsError::NotFound);
            }

            let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
            let mut entries = alloc::vec![];
            // Always include std fds (Stdin/Stdout/Stderr are not in the BTreeMap)
            for fd_num in [0u32, 1, 2] {
                entries.push(DirEntry {
                    name: format!("{fd_num}"),
                    is_dir: false,
                    is_symlink: true,
                    size: 0,
                });
            }
            // Add all allocated fds from the table
            for &fd_num in proc.fds.table.lock().keys() {
                if fd_num > 2 {
                    entries.push(DirEntry {
                        name: format!("{fd_num}"),
                        is_dir: false,
                        is_symlink: true,
                        size: 0,
                    });
                }
            }
            return Ok(entries);
        }

        Err(FsError::NotFound)
    }

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        let path = resolve_self(path.trim_start_matches('/'));
        let path = path.as_ref();

        // Handle virtual files first (boxes, cores, net/tcp, sysvipc/msg,
        // mounts, filesystems, <pid>/syscalls, etc.). This list must name every
        // path `read_file` renders — `metadata` and `list_dir` advertising a
        // file that `read_at` does not forward is how `/proc/cores` came to
        // `open()` and `stat()` fine while every `read()` returned NotFound
        // (docs/archive/DEVBOX_ISSUES.md Issue 4).
        if path == "boxes"
            || path == "cores"
            || path == "mounts"
            || path == "filesystems"
            || path.starts_with("net/")
            || path == "sysvipc/msg"
        {
            let data = self.read_file(path)?;
            if offset >= data.len() {
                return Ok(0);
            }
            let n = buf.len().min(data.len() - offset);
            buf[..n].copy_from_slice(&data[offset..offset + n]);
            return Ok(n);
        }

        // Handle <pid>/mounts — the target's namespace mounts, box-filtered.
        {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "mounts"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let data = mounts_bytes(Some(pid), caller_box_id())
                        .ok_or(FsError::NotFound)?;
                    if offset >= data.len() {
                        return Ok(0);
                    }
                    let n = buf.len().min(data.len() - offset);
                    buf[..n].copy_from_slice(&data[offset..offset + n]);
                    return Ok(n);
                }
        }

        // Handle <pid>/syscalls. `get_formatted` applies the box rule itself —
        // this is the method a `read()` actually lands in, and it is the one that
        // used to have no isolation check at all.
        if crate::config::PROC_SYSCALL_LOG_ENABLED {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "syscalls"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let data = crate::syscall::log::get_formatted(pid, caller_box_id())
                        .ok_or(FsError::NotFound)?;
                    if offset >= data.len() {
                        return Ok(0);
                    }
                    let n = buf.len().min(data.len() - offset);
                    buf[..n].copy_from_slice(&data[offset..offset + n]);
                    return Ok(n);
                }
        }

        // Handle <pid>/cmdline and <pid>/status
        {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && (parts[1] == "cmdline" || parts[1] == "status" || parts[1] == "stat")
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
                    let current_box_id =
                        akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);
                    if current_box_id != 0 && proc.box_id != current_box_id {
                        return Err(FsError::NotFound);
                    }
                    let data = match parts[1] {
                        "cmdline" => proc_cmdline_bytes(proc),
                        "stat" => proc_stat_text(proc).into_bytes(),
                        _ => proc_status_text(proc).into_bytes(),
                    };
                    if offset >= data.len() {
                        return Ok(0);
                    }
                    let n = buf.len().min(data.len() - offset);
                    buf[..n].copy_from_slice(&data[offset..offset + n]);
                    return Ok(n);
                }
        }

        let (pid, fd_num) = Self::parse_fd_path(path)?;
        let current_proc = akuma_exec::process::current_process_shared();
        let current_box_id = current_proc.as_ref().map_or(0, |p| p.box_id);

        let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
        
        // BOX ISOLATION: Box N only sees its own processes.
        if current_box_id != 0 && proc.box_id != current_box_id {
            return Err(FsError::NotFound);
        }

        // Lock the appropriate buffer
        match fd_num {
            0 => {
                let stdin = proc.stdin.lock();
                if offset >= stdin.data.len() {
                    return Ok(0);
                }
                let n = buf.len().min(stdin.data.len() - offset);
                buf[..n].copy_from_slice(&stdin.data[offset..offset + n]);
                Ok(n)
            }
            1 => {
                let stdout = proc.stdout.lock();
                if offset >= stdout.data.len() {
                    return Ok(0);
                }
                let n = buf.len().min(stdout.data.len() - offset);
                buf[..n].copy_from_slice(&stdout.data[offset..offset + n]);
                Ok(n)
            }
            _ => {
                // For other fds (pipes, sockets, etc.) return a description string
                let fd_entry = proc.get_fd(fd_num).ok_or(FsError::NotFound)?;
                let desc = Self::fd_description(&fd_entry, fd_num);
                let bytes = desc.as_bytes();
                if offset >= bytes.len() {
                    return Ok(0);
                }
                let n = buf.len().min(bytes.len() - offset);
                buf[..n].copy_from_slice(&bytes[offset..offset + n]);
                Ok(n)
            }
        }
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let path = resolve_self(path.trim_start_matches('/'));
        let path = path.as_ref();
        let current_proc = akuma_exec::process::current_process_shared();
        let current_box_id = current_proc.as_ref().map_or(0, |p| p.box_id);

        // Handle /proc/boxes
        if path == "boxes" {
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            
            let boxes = process::list_boxes();
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[ProcFS] Reading boxes (count={})\n", boxes.len());
            }
            let mut out = String::from("ID,NAME,ROOT,CREATOR,PRIMARY\n");
            for b in boxes {
                let _ = writeln!(out, "{},{},{},{},{}", b.id, b.name, b.root_dir, b.creator_pid, b.primary_pid);
            }
            return Ok(out.into_bytes());
        }

        // /proc/cores — kept for herd's benefit: it used to list per-core lifecycle state
        // under the removed one-kernel-per-core multikernel
        // (docs/archive/TRIM_FAT_MULTIKERNEL.md). Every core now runs the same shared
        // kernel, so this is a static single-row table.
        if path == "cores" {
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            return Ok(b"core state role\n0 online bsp\n".to_vec());
        }

        if path == "net/tcp" {
            let sockets = akuma_net::socket::list_sockets();
            // BACKLOG is listening/pending/dead over a listener's smoltcp handle
            // pool, and `-` for everything else — see `SocketStat::backlog`. It is
            // the only view of the one number that decides whether a port still
            // answers.
            let mut out = String::from("LOCAL_PORT,REMOTE_ADDR,STATE,BOX,BACKLOG\n");
            for s in sockets {
                let _ = write!(out, "{},{}.{}.{}.{}:{},{},{},",
                    s.local_port,
                    s.remote_ip[0], s.remote_ip[1], s.remote_ip[2], s.remote_ip[3],
                    s.remote_port,
                    s.state,
                    s.box_id);
                let (listening, pending, dead) = s.backlog;
                if listening == 0 && pending == 0 && dead == 0 {
                    let _ = writeln!(out, "-");
                } else {
                    let _ = writeln!(out, "{listening}/{pending}/{dead}");
                }
            }
            return Ok(out.into_bytes());
        }

        if path == "net/udp" {
            return Ok(String::from("LOCAL_PORT,REMOTE_ADDR,STATE,BOX\n").into_bytes());
        }

        // /proc/mounts — the viewer's own mount set (its namespace if boxed,
        // the global table on the host). A box sees which paths are mounted
        // into it, never where they came from — crate::vfs::render_mounts
        // enforces that and keeps the whole render allocation-free.
        if path == "mounts" {
            let pid = process::current_process_shared().map(|p| p.pid);
            return Ok(mounts_bytes(pid, current_box_id).unwrap_or_default());
        }

        // /proc/filesystems — the kernel's supported fstypes, one per line.
        if path == "filesystems" {
            let out = if cfg!(feature = "sc-containers") {
                "ext2\nproc\ntmpfs\noverlay\n"
            } else {
                "ext2\nproc\ntmpfs\n"
            };
            return Ok(String::from(out).into_bytes());
        }

        // Real Linux `/proc/net/dev` format, verbatim (header + `%6s: %7llu
        // %7llu %4llu %4llu %4llu %5llu %10llu %9llu %8llu %7llu %4llu %4llu
        // %4llu %5llu %7llu %10llu` per interface, from `net/core/net-procfs.c`)
        // — unlike `net/tcp`/`net/udp` above, this one has a real consumer that
        // parses it strictly: busybox `ifconfig` with no interface name reads
        // this file to enumerate devices before it can print anything, and its
        // ioctl-based fallback (`SIOCGIFCONF`) is a struct-layout guess this
        // sidesteps entirely. Only `lo` and `eth0` exist (`docs/reference/subsystems/networking.md`);
        // this kernel doesn't track byte/packet counters per interface, so
        // every field is `0` — a legitimate value ifconfig already prints for
        // any idle interface, not a placeholder that reads as broken.
        if path == "net/dev" {
            let mut out = String::from(
                "Inter-|   Receive                                                |  Transmit\n \
                 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n"
            );
            for name in ["lo", "eth0"] {
                let _ = writeln!(out,
                    "{name:>6}: {:>7} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>7} {:>10}",
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
            }
            return Ok(out.into_bytes());
        }

        #[cfg(feature = "sc-sysv-ipc")]
        if path == "sysvipc/msg" && crate::config::PROC_SYSVIPC_ENABLED {
            let queues = crate::syscall::msgqueue::list_msg_queues();
            let mut out = String::from(
                "       key      msqid perms      cbytes       qnum lspid lrpid   stime   rtime   ctime\n"
            );
            for q in queues {
                // Box isolation: box N only sees its own queues
                if current_box_id != 0 && q.box_id != current_box_id {
                    continue;
                }
                let _ = writeln!(out,
                    "{:10} {:10} {:5o} {:10} {:10}     0     0       0       0       0",
                    q.key, q.msqid, q.mode, q.cbytes, q.qnum
                );
            }
            return Ok(out.into_bytes());
        }

        // Handle <pid>/syscalls. The check that used to live here consulted the
        // process table, which cannot answer for a pid that has already exited —
        // and an exited pid is exactly what a retained log is for, so it fell open.
        // The log carries its own `box_id` now and decides for itself.
        if crate::config::PROC_SYSCALL_LOG_ENABLED {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "syscalls"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    return crate::syscall::log::get_formatted(pid, current_box_id)
                        .ok_or(FsError::NotFound);
                }
        }

        // Handle <pid>/mounts — the target's namespace mounts. Same box rule
        // as <pid>/status: a box only sees pids in its own box.
        {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "mounts"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    return mounts_bytes(Some(pid), current_box_id)
                        .ok_or(FsError::NotFound);
                }
        }

        // Handle <pid>/cmdline and <pid>/status
        {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && (parts[1] == "cmdline" || parts[1] == "status" || parts[1] == "stat")
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
                    if current_box_id != 0 && proc.box_id != current_box_id {
                        return Err(FsError::NotFound);
                    }
                    return Ok(match parts[1] {
                        "cmdline" => proc_cmdline_bytes(proc),
                        "stat" => proc_stat_text(proc).into_bytes(),
                        _ => proc_status_text(proc).into_bytes(),
                    });
                }
        }

        let (pid, fd_num) = Self::parse_fd_path(path)?;

        let current_proc = akuma_exec::process::current_process_shared();
        let current_box_id = current_proc.as_ref().map_or(0, |p| p.box_id);

        let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
        
        // BOX ISOLATION: Box N only sees its own processes.
        if current_box_id != 0 && proc.box_id != current_box_id {
            return Err(FsError::NotFound);
        }

        // Lock the appropriate buffer and clone data (thread-safe)
        match fd_num {
            0 => Ok(proc.stdin.lock().clone_data()),
            1 => Ok(proc.stdout.lock().clone_data()),
            _ => Err(FsError::NotFound),
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        // All-or-nothing contract: a caller with no way to report a residue must
        // not be told a partial stdin write succeeded. `write_at` is the path
        // `sys_write` actually takes and it reports the count instead.
        match Self::write_fd_data(path, data)? {
            n if n == data.len() => Ok(()),
            _ => Err(FsError::NoSpace),
        }
    }

    fn write_at(&self, path: &str, _offset: usize, data: &[u8]) -> Result<usize, FsError> {
        // For ProcFS, we ignore the offset and treat it as a direct write/append
        // to the process buffers. This avoids the default read-modify-write behavior.
        //
        // Returns the ACCEPTED count, which for fd 0 can be short (or 0) once the
        // target's bounded stdin buffer fills — `sys_write` turns that into a
        // short write / EAGAIN for the caller to retry. It used to report
        // `data.len()` unconditionally, which is what let the drop-oldest
        // overflow inside `write_stdin` go unnoticed.
        Self::write_fd_data(path, data)
    }


    fn create_dir(&self, _path: &str) -> Result<(), FsError> {
        // Cannot create directories in procfs
        Err(FsError::NotSupported)
    }

    fn remove_file(&self, _path: &str) -> Result<(), FsError> {
        // Cannot remove files in procfs
        Err(FsError::NotSupported)
    }

    fn remove_dir(&self, _path: &str) -> Result<(), FsError> {
        // Cannot remove directories in procfs
        Err(FsError::NotSupported)
    }

    fn exists(&self, path: &str) -> bool {
        let path = resolve_self(path.trim_matches('/'));
        let path = path.as_ref();
        let current_box_id = caller_box_id();

        if path.is_empty() {
            return true; // Root always exists
        }

        if path == "boxes" || path == "cores" {
            return current_box_id == 0;
        }

        if path == "mounts" || path == "filesystems" {
            return true;
        }

        // <pid>/mounts — same visibility rule as <pid>/status; no render
        // needed to answer an existence question.
        {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() == 2 && parts[1] == "mounts"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    return Self::process_visible(pid, caller_box_id());
            }
        }

        if path == "net" || path == "net/tcp" || path == "net/udp" || path == "net/dev" {
            return true;
        }

        if crate::config::PROC_SYSVIPC_ENABLED && (path == "sysvipc" || path == "sysvipc/msg") {
            return true;
        }

        // Try to parse as fd path first
        if let Ok((pid, fd_num)) = Self::parse_fd_path(path) {
            if !Self::process_visible(pid, current_box_id) { return false; }
            if fd_num <= 1 { return true; }
            return process::lookup_process_shared(pid)
                .is_some_and(|p| p.get_fd(fd_num).is_some());
        }

        // Try to parse as pid path
        if let Ok(pid) = Self::parse_pid_path(path) {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() == 1 {
                if Self::process_visible(pid, current_box_id) { return true; }
                if crate::config::PROC_SYSCALL_LOG_ENABLED {
                    return crate::syscall::log::get_formatted(pid, current_box_id).is_some();
                }
                return false;
            }
            if parts.len() == 2 && parts[1] == "fd" {
                return Self::process_visible(pid, current_box_id);
            }
            if parts.len() == 2 && (parts[1] == "cmdline" || parts[1] == "status" || parts[1] == "stat") {
                return Self::process_visible(pid, current_box_id);
            }
            if parts.len() == 2 && parts[1] == "syscalls" && crate::config::PROC_SYSCALL_LOG_ENABLED {
                return crate::syscall::log::get_formatted(pid, current_box_id).is_some();
            }
        }

        false
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        let path = resolve_self(path.trim_matches('/'));
        let path = path.as_ref();

        let inode = {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in path.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100000001b3);
            }
            h
        };

        if path.is_empty() {
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o40555,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if path == "boxes" || path == "cores" {
            if caller_box_id() != 0 {
                return Err(FsError::NotFound);
            }
            return Ok(Metadata {
                is_dir: false,
                size: 0,
                inode,
                mode: 0o100444,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if path == "mounts" || path == "filesystems" {
            // Size without allocating: render into a stack buffer and take the
            // byte count (the Vec-returning `mounts_bytes` is for the read
            // path only — `metadata` must not build the whole listing).
            let size = if path == "mounts" {
                let pid = process::current_process_shared().map(|p| p.pid);
                let mut probe = [0u8; 2048];
                crate::vfs::render_mounts(caller_box_id(), pid, &mut probe) as u64
            } else {
                0
            };
            return Ok(Metadata {
                is_dir: false,
                size,
                inode,
                mode: 0o100444,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        // <pid>/mounts — same visibility rule as <pid>/status; the size probe
        // renders into a stack buffer, no allocation.
        {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() == 2 && parts[1] == "mounts"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    if !Self::process_visible(pid, caller_box_id()) {
                        return Err(FsError::NotFound);
                    }
                    let mut probe = [0u8; 2048];
                    let size = crate::vfs::render_mounts(caller_box_id(), Some(pid), &mut probe) as u64;
                    return Ok(Metadata {
                        is_dir: false,
                        size,
                        inode,
                        mode: 0o100444,
                        created: None,
                        modified: None,
                        accessed: None,
                    });
                }
        }

        if path == "net" {
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o40555,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if path == "net/tcp" || path == "net/udp" || path == "net/dev" {
            return Ok(Metadata {
                is_dir: false,
                size: 0,
                inode,
                mode: 0o100444,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if crate::config::PROC_SYSVIPC_ENABLED && path == "sysvipc" {
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o40555,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if crate::config::PROC_SYSVIPC_ENABLED && path == "sysvipc/msg" {
            return Ok(Metadata {
                is_dir: false,
                size: 0,
                inode,
                mode: 0o100444,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        // Try fd path first
        if let Ok((pid, fd_num)) = Self::parse_fd_path(path) {
            let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
            let size = match fd_num {
                0 => proc.stdin.lock().len() as u64,
                1 => proc.stdout.lock().len() as u64,
                _ => {
                    if proc.get_fd(fd_num).is_none() { return Err(FsError::NotFound); }
                    0
                }
            };
            return Ok(Metadata {
                is_dir: false,
                size,
                inode,
                mode: 0o100444,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        // Try pid or pid/fd path
        if let Ok(pid) = Self::parse_pid_path(path) {
            let parts: Vec<&str> = path.split('/').collect();
            // <pid>/syscalls
            if parts.len() == 2 && parts[1] == "syscalls" && crate::config::PROC_SYSCALL_LOG_ENABLED {
                if crate::syscall::log::get_formatted(pid, caller_box_id()).is_some() {
                    return Ok(Metadata {
                        is_dir: false,
                        size: 0,
                        inode,
                        mode: 0o100444,
                        created: None,
                        modified: None,
                        accessed: None,
                    });
                }
                return Err(FsError::NotFound);
            }
            // <pid>/cmdline and <pid>/status
            if parts.len() == 2 && (parts[1] == "cmdline" || parts[1] == "status" || parts[1] == "stat") {
                let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
                let current_box_id =
                    akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);
                if current_box_id != 0 && proc.box_id != current_box_id {
                    return Err(FsError::NotFound);
                }
                let size = match parts[1] {
                    "cmdline" => proc_cmdline_bytes(proc).len() as u64,
                    "stat" => proc_stat_text(proc).len() as u64,
                    _ => proc_status_text(proc).len() as u64,
                };
                return Ok(Metadata {
                    is_dir: false,
                    size,
                    inode,
                    mode: 0o100444,
                    created: None,
                    modified: None,
                    accessed: None,
                });
            }
            let pid_exists = Self::process_visible(pid, caller_box_id())
                || (crate::config::PROC_SYSCALL_LOG_ENABLED
                    && crate::syscall::log::get_formatted(pid, caller_box_id()).is_some());
            if !pid_exists {
                return Err(FsError::NotFound);
            }
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                inode,
                mode: 0o40555,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        Err(FsError::NotFound)
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        // Procfs doesn't use block storage
        Ok(FsStats {
            block_size: 4096,
            total_blocks: 0,
            free_blocks: 0,
        })
    }

    fn read_symlink(&self, path: &str) -> Result<String, FsError> {
        let path = path.trim_start_matches('/');

        // Handle "self/fd/<n>" -> resolve to current process
        if let Some(rest) = path.strip_prefix("self/") {
            let pid = process::current_process_shared().map(|p| p.pid).ok_or(FsError::NotFound)?;
            let new_path = format!("{pid}/{rest}");
            return self.read_symlink(&new_path);
        }

        // Handle "<pid>/fd/<n>" -> symlink to the file path.
        // Only return a path for File fds (resolvable filesystem paths).
        // For other fd types (pipes, sockets, etc.) return Err so that resolve_symlinks
        // does NOT chase the virtual description string (e.g. "pipe:[5]" is not a real path).
        // readlinkat uses proc_fd_description() to get the description string instead.
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() == 3 && parts[1] == "fd" {
            let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
            let fd: u32 = parts[2].parse().map_err(|_| FsError::NotFound)?;

            let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
            if let Some(fd_entry) = proc.get_fd(fd) {
                use akuma_exec::process::FileDescriptor;
                if let FileDescriptor::File(f) = fd_entry {
                    return Ok(f.path);
                }
                // Non-file fds: not a resolvable symlink target
                return Err(FsError::NotFound);
            }
            return Err(FsError::NotFound);
        }

        Err(FsError::NotFound)
    }

    fn is_symlink(&self, path: &str) -> bool {
        let path = path.trim_start_matches('/');

        // "self" is a symlink to the current PID
        if path == "self" {
            return true;
        }

        // "self/fd/<n>" and "<pid>/fd/<n>" are symlinks
        if let Some(rest) = path.strip_prefix("self/")
            && rest.starts_with("fd/") {
                let fd_part = &rest[3..];
                return fd_part.parse::<u32>().is_ok();
            }

        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() == 3 && parts[1] == "fd" {
            return parts[0].parse::<Pid>().is_ok() && parts[2].parse::<u32>().is_ok();
        }

        false
    }
}
