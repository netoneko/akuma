//! Process Filesystem (procfs)
//!
//! A virtual filesystem that exposes process stdin/stdout as files.
//! Mounted at /proc, provides:
//! - /proc/<pid>/fd/0 - stdin (readable by all, writable by spawner/kernel)
//! - /proc/<pid>/fd/1 - stdout (readable by all, writable by owning process)
//! - /proc/<pid>/cmdline - argv as NUL-separated bytes (Linux-compatible)
//! - /proc/<pid>/status - human-readable `Name`, `State`, `Pid`, `PPid`, etc.
//!
//! Which names exist and what a path resolves to is `akuma_procfs::tree`'s
//! decision (see "The tree" below); this file supplies the process facts and
//! renders the files.
//!
//! # TODO: Rewrite without allocations
//!
//! Path resolution — `exists`, `is_symlink`, `metadata`'s lookup — allocates
//! nothing since 2026-10-16 (the tree parses in place; the canonical path is
//! on the stack). `read_symlink` still returns a `String` (the trait's
//! signature), `read_dir` a `Vec<DirEntry>`, and several renderers build a
//! `Vec`/`String`; those can deadlock if reached with the allocator lock held.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::fmt::Write as _;

use super::{DirEntry, Filesystem, FsError, FsStats, Metadata};
use akuma_exec::{process::{self, Pid, ProcessState}, threading};

// ============================================================================
// /proc/<pid>/cmdline + status (Linux-style)
// ============================================================================

/// `/proc/<pid>/maps` — the process's mappings, ascending.
///
/// Rows come from the [`crate::VfsGlueHooks::pid_map_rows`] hook, because the
/// walk behind them is not portable; the *format* is
/// [`akuma_procfs::render_maps_line`], shared with everything else in this
/// file. `None` when no kernel registered a walk, which is how the file stays
/// absent rather than empty — an empty `maps` is a confident "this process has
/// no mappings", which no process has.
fn render_pid_maps(pid: Pid) -> Option<Vec<u8>> {
    let rows = crate::pid_map_rows(pid)?;
    let mut out = Vec::new();
    let mut line = [0u8; akuma_procfs::MAPS_LINE_MAX];
    for (start, end, read, write, exec, private) in rows {
        let n = akuma_procfs::render_maps_line(start, end, read, write, exec, private, "", &mut line);
        out.extend_from_slice(&line[..n]);
    }
    Some(out)
}

/// `/proc/<pid>/statm` — the seven page counts, from the **same** rows `maps`
/// renders.
///
/// One source for both, deliberately: two files disagreeing about one address
/// space is the failure this file keeps producing, and the amd64 kernel's own
/// version of these two was written after exactly that (`statm` summed the
/// region list alone and reported `size 0` for a program whose `maps` listed
/// three mappings).
///
/// `resident` is the frame ledger's count — the only real number for physical
/// pages held. `shared`, `lib` and `dt` are 0: nothing here tracks them per
/// process, and a fabricated breakdown would be read as a real one.
fn render_pid_statm(proc: &process::Process) -> Option<Vec<u8>> {
    let rows = crate::pid_map_rows(proc.pid)?;
    let pages = |(s, e, _, _, _, _): &crate::MapRow| e.saturating_sub(*s) / 4096;
    let size_pages: usize = rows.iter().map(pages).sum();
    let text_pages: usize = rows.iter().filter(|r| r.4).map(pages).sum();
    let mut buf = [0u8; akuma_procfs::STATM_MAX];
    let n = akuma_procfs::render_statm(
        size_pages as u64,
        proc.address_space.resident_pages() as u64,
        0,
        text_pages as u64,
        size_pages.saturating_sub(text_pages) as u64,
        &mut buf,
    );
    Some(buf[..n].to_vec())
}

/// The bytes of `<pid>/maps` or `<pid>/statm`, or `None` if this kernel cannot
/// describe the address space (or the pid is not visible).
fn pid_address_space_file(pid: Pid, name: &str) -> Option<Vec<u8>> {
    if !ProcFilesystem::process_visible(pid, caller_box_id()) {
        return None;
    }
    match name {
        "maps" => render_pid_maps(pid),
        "statm" => {
            let proc = process::lookup_process_shared(pid)?;
            render_pid_statm(proc)
        }
        _ => None,
    }
}

// ============================================================================
// The tree: `akuma_procfs::tree` over this kernel's process table
// ============================================================================
//
// Which names exist under `/proc`, what kind each is, and what a path resolves
// to are decided in ONE place — `akuma_procfs::tree::{resolve, list}`, host-
// tested by a simulation that walks the whole tree. This file supplies the
// facts (`KernelModel`) and keeps the renderers. Before 2026-10-16 `read_dir`,
// `exists`, `metadata`, `is_symlink` and `read_symlink` each parsed paths for
// themselves, and their disagreements were the bugs: a thread directory that
// listed `task`, an `fd/2` listed but refused, thread-view symlinks that
// `is_symlink` denied (docs/archive/AKUMA_PROC_TREE_UNIFICATION.md).
//
// `/proc/self/…` is still answered here rather than chased by the VFS: Akuma's
// VFS hands procfs the literal `self/status`, which is why `redis-server`
// could not start and libcap-ng returned -1 with no errno
// (docs/archive/LONG_ROAD_TO_REDIS.md). The tree resolves `self/…` to the
// caller's thread-group id; the bare `self` stays a symlink so
// `readlink("/proc/self")` keeps working.

use akuma_procfs::tree::{self, Kind, Node, PidFile, ProcInfo, ProcModel};

/// The tree's view of this kernel, taken once per `ProcFilesystem` call.
///
/// Three scalars, not a copy of the process table: a 256-row snapshot is
/// ~6 KB, and these methods run deep inside path resolution on amd64's 32 KB
/// kernel stacks. Each question goes to the table when it is asked.
struct KernelModel {
    viewer_box: u64,
    current_tgid: Option<Pid>,
    features: tree::Features,
}

impl KernelModel {
    fn now() -> Self {
        Self {
            viewer_box: caller_box_id(),
            // `read_current_pid` answers the thread-group id, which is what
            // `/proc/self` names on Linux.
            current_tgid: process::read_current_pid(),
            features: tree::Features {
                power: crate::power_renderer().is_some(),
                sysvipc: crate::cfg_proc_sysvipc_enabled(),
                syscall_log: crate::cfg_proc_syscall_log_enabled(),
            },
        }
    }

    /// Pids of the rows `keep` accepts, ascending, into `buf`; returns how
    /// many. The scan runs with IRQs off and must not allocate, so it fills a
    /// stack array (1 KB) and callers run their callbacks after it.
    fn pids_where(
        buf: &mut [Pid; process::MAX_PROCESSES],
        mut keep: impl FnMut(&process::Process) -> bool,
    ) -> usize {
        let mut n = 0;
        process::for_each_process(|p| {
            if n < buf.len() && keep(p) {
                buf[n] = p.pid;
                n += 1;
            }
        });
        buf[..n].sort_unstable();
        n
    }
}

impl ProcModel for KernelModel {
    fn viewer_box(&self) -> u64 {
        self.viewer_box
    }

    fn current_tgid(&self) -> Option<Pid> {
        self.current_tgid
    }

    fn features(&self) -> tree::Features {
        self.features
    }

    fn for_each_pid(&self, f: &mut dyn FnMut(Pid)) {
        let mut buf = [0; process::MAX_PROCESSES];
        let n = Self::pids_where(&mut buf, |_| true);
        for &pid in &buf[..n] {
            f(pid);
        }
    }

    fn process(&self, pid: Pid) -> Option<ProcInfo> {
        // Scalars copied out during the table scan, so no `&'static Process`
        // escapes to be read after its row is freed.
        process::find_process(|p| {
            (p.pid == pid).then(|| ProcInfo {
                tgid: p.tgid,
                box_id: p.box_id,
                exited: p.exited.load(core::sync::atomic::Ordering::Relaxed),
            })
        })
    }

    fn for_each_fd(&self, pid: Pid, f: &mut dyn FnMut(u32)) {
        let Some(p) = process::lookup_process_shared(pid) else { return };
        // Reserve outside the table lock and copy at most that many keys under
        // it, so nothing allocates while the lock is held. A descriptor opened
        // between the two locks is missed — the race any listing of a live
        // table has.
        let cap = akuma_primitives::irq::with_irqs_disabled(|| p.fds.table.lock().len());
        let mut fds: Vec<u32> = Vec::with_capacity(cap);
        akuma_primitives::irq::with_irqs_disabled(|| {
            let table = p.fds.table.lock();
            fds.extend(table.keys().copied().take(cap));
        });
        for fd in fds {
            f(fd);
        }
    }

    fn has_fd(&self, pid: Pid, fd: u32) -> bool {
        // `contains_key`, not `get_fd(..).is_some()`: `get_fd` clones the
        // descriptor — a `String` for every file — under the lock, to answer a
        // yes/no.
        process::lookup_process_shared(pid).is_some_and(|p| {
            akuma_primitives::irq::with_irqs_disabled(|| p.fds.table.lock().contains_key(&fd))
        })
    }

    fn has_syscall_log(&self, pid: Pid) -> bool {
        akuma_syscalls_log::has_log(pid, self.viewer_box)
    }

    fn for_each_logged_pid(&self, f: &mut dyn FnMut(Pid)) {
        for pid in akuma_syscalls_log::list_pids_with_logs(self.viewer_box) {
            f(pid);
        }
    }

    fn has_address_space(&self, pid: Pid) -> bool {
        // "Exists exactly when it can be rendered" — the rule that keeps
        // `open`, `stat` and `access` agreeing about `maps`/`statm`. Still a
        // walk per question (the hook is the only authority on whether this
        // kernel can describe `pid`), but rows only: the old check rendered and
        // formatted the whole of `maps` to throw it away.
        crate::pid_map_rows(pid).is_some()
    }

    fn for_each_live_member(&self, tgid: Pid, f: &mut dyn FnMut(Pid)) {
        // One table pass; the trait's default would look every pid up again.
        let mut buf = [0; process::MAX_PROCESSES];
        let n = Self::pids_where(&mut buf, |p| {
            p.tgid == tgid && !p.exited.load(core::sync::atomic::Ordering::Relaxed)
        });
        for &pid in &buf[..n] {
            f(pid);
        }
    }
}

/// Resolve a path relative to the mount through the tree.
fn resolve_node(path: &str) -> Option<Node> {
    tree::resolve(&KernelModel::now(), path)
}

/// The longest canonical spelling is `4294967295/fd/4294967295` (24 bytes).
const CANONICAL_MAX: usize = 32;

/// `node`'s canonical spelling into `buf`; returns its length.
fn write_canonical(node: Node, buf: &mut [u8; CANONICAL_MAX]) -> usize {
    let mut pos = 0;
    let _ = node.write_canonical(&mut akuma_primitives::console::FmtBuf { buf, pos: &mut pos });
    pos
}

/// A node's canonical spelling, or the path as given when it resolves to
/// nothing — so a renderer that parses paths sees `self/` and
/// `<tgid>/task/<tid>/` folded to `<pid>/`, and an unknown path still reaches
/// its own not-found answer. On the stack; the old `resolve_self` allocated a
/// `String` per call.
enum Canonical<'a> {
    Stack([u8; CANONICAL_MAX], usize),
    Given(&'a str),
}

impl Canonical<'_> {
    fn as_str(&self) -> &str {
        match self {
            Canonical::Stack(buf, len) => core::str::from_utf8(&buf[..*len]).unwrap_or(""),
            Canonical::Given(s) => s,
        }
    }
}

fn canonical(path: &str) -> Canonical<'_> {
    let path = path.trim_matches('/');
    match resolve_node(path) {
        Some(node) => {
            let mut buf = [0u8; CANONICAL_MAX];
            let len = write_canonical(node, &mut buf);
            Canonical::Stack(buf, len)
        }
        None => Canonical::Given(path),
    }
}

/// A stable inode per node. Hashes the canonical spelling, so `self/x` and
/// `<pid>/x` share one (they are one node, as on Linux), and mixes in the
/// thread-group id for a thread view, so `<tgid>/task/<tgid>` is **not** the
/// inode of its own ancestor `<tgid>` — `find` and `rg --follow` read an
/// ancestor's `(st_dev, st_ino)` reappearing below it as a filesystem loop.
/// The old hash was of the aliased path, which made it exactly that.
fn node_inode(node: Node) -> u64 {
    let mut buf = [0u8; CANONICAL_MAX];
    let len = write_canonical(node, &mut buf);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    for &b in &buf[..len] {
        mix(b);
    }
    let thread_of = match node {
        Node::Pid(v) | Node::PidFile(v, _) | Node::FdDir(v) | Node::Fd(v, _) => v.thread_of,
        _ => None,
    };
    if let Some(tgid) = thread_of {
        mix(b'@');
        for b in tgid.to_le_bytes() {
            mix(b);
        }
    }
    h
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

/// Render a `/proc/mounts` listing for the **read** paths.
///
/// The mount
/// spinlocks are held only inside `crate::render_mounts`, which writes
/// into the stack buffer below allocation-free; the `.to_vec()` copy happens
/// after the locks are gone and exists only because the `Filesystem` trait's
/// `read_file` returns `Vec<u8>` — the same contract every procfs virtual
/// file (`boxes`, `net/tcp`, …) already lives with. `metadata` and `exists`
/// use `render_mounts` directly and do not go through here.
///
/// `target_pid` selects whose mount set is listed; `None` (or an unknown
/// pid) means the global table. Returns `None` when the viewer may not see
/// the target (box rule) — callers turn that into `NotFound`.
#[must_use]
pub fn mounts_bytes(target_pid: Option<Pid>, viewer_box_id: u64) -> Option<Vec<u8>> {
    if let Some(pid) = target_pid {
        let proc = process::lookup_process_shared(pid)?;
        if viewer_box_id != 0 && proc.box_id != viewer_box_id {
            return None;
        }
    }
    let mut buf = [0u8; 2048];
    let n = crate::render_mounts(viewer_box_id, target_pid, &mut buf);
    Some(buf[..n].to_vec())
}

/// Map a `Process` onto the plain value the `akuma-procfs` renderers take.
///
/// `name` is borrowed rather than owned because `image_name()` returns an owned
/// `String` and [`ProcStat`] holds a `&str` — the caller keeps the `String`
/// alive across the render. That is the whole adaptation layer: everything
/// below is `akuma-procfs`, byte for byte the same as the amd64 kernel emits.
///
/// [`ProcStat`]: akuma_procfs::ProcStat
fn proc_stat_of<'a>(
    p: &process::Process,
    name: &'a str,
    creds: &'a process::Creds,
) -> akuma_procfs::ProcStat<'a> {
    use akuma_procfs::{ProcStat, ProcState as PS};
    let state = match p.state.load() {
        ProcessState::Zombie(code) => PS::Zombie(code),
        ProcessState::Blocked => PS::Sleeping,
        ProcessState::Ready | ProcessState::Running => PS::Running,
    };
    ProcStat {
        pid: p.pid,
        ppid: p.parent_pid,
        state,
        name,
        // The same per-thread microsecond counter `top`'s CORE column reads via
        // `sys_get_cpu_stats`. We do not split user from system time, so all of
        // it lands in `utime` and `stime` stays 0.
        cpu_time_us: p.thread_id.map_or(0, threading::get_thread_cpu_time),
        uid: creds.uid(),
        gid: creds.gid(),
        groups: creds.groups(),
    }
}

fn proc_cmdline_bytes(p: &process::Process) -> Vec<u8> {
    let name = p.image_name();
    let args = p.image_args();
    let creds = p.creds.get();
    let stat = proc_stat_of(p, &name, &creds);
    // Bounded by what the caller could ever read back: `MAX_ARGV` elements is
    // not a thing this crate knows, so size from the arguments themselves and
    // let the renderer's own truncation cap a pathological one.
    let need: usize = args.iter().map(|a| a.len() + 1).sum::<usize>().max(name.len() + 1);
    let mut buf = alloc::vec![0u8; need];
    let n = akuma_procfs::render_cmdline(&stat, args.iter().map(alloc::string::String::as_bytes), &mut buf);
    buf.truncate(n);
    buf
}

fn proc_status_text(p: &process::Process) -> String {
    let name = p.image_name();
    let creds = p.creds.get();
    let stat = proc_stat_of(p, &name, &creds);
    let mut buf = [0u8; akuma_procfs::STATUS_MAX];
    let n = akuma_procfs::render_status(&stat, &mut buf);
    // The renderer emits ASCII only, so this cannot lose bytes; `from_utf8_lossy`
    // rather than an `unwrap` because a panic here would be a kernel panic
    // reachable from `cat /proc/1/status`.
    String::from(alloc::string::String::from_utf8_lossy(&buf[..n]))
}

/// `/proc/<pid>/stat` (Linux-compatible, space-separated, single line).
///
/// **This is the file `ps`/`top` actually parse** — not `status`. Busybox `ps`
/// showed an empty process list here despite `status`/`cmdline` being fully
/// populated, because this file did not exist at all.
///
/// The line itself is [`akuma_procfs::render_pid_stat`], shared with the amd64
/// kernel and host-tested there. Moving it out found that this line had been
/// emitting **41 fields where its own comment listed 44** — harmless only
/// because `ps` counts no further than `utime` at field 14.
fn render_pid_stat(p: &process::Process, buf: &mut [u8]) -> usize {
    let name = p.image_name();
    let creds = p.creds.get();
    akuma_procfs::render_pid_stat(&proc_stat_of(p, &name, &creds), buf)
}

/// USER_HZ: Linux's jiffy rate for every `/proc` CPU-time field (`utime`,
/// `stime`, the `/proc/stat` `cpu` line, …) is 100 Hz on every arch that
/// matters here, regardless of the kernel's actual timer tick
/// (`config::TIMER_INTERVAL_US`) — it's an ABI constant `sysconf(_SC_CLK_TCK)`
/// reports, not a reflection of how often we actually interrupt.
use akuma_procfs::JIFFY_US;

use akuma_exec::threading::types::MAX_CORES;

/// Number of cores the CPU-time accounting should treat as "the system" —
/// `smp_shared::probed_core_count()` under real SMP, 1 otherwise (including
/// the legacy `smp` multikernel feature, which doesn't track a shared count).
/// Clamped to `MAX_CORES` (the SMP bringup cap `LAST_CORE` values are already
/// bounded by), so callers can size a fixed `[_; MAX_CORES]` array instead of
/// allocating.
#[cfg(kernel_smp_shared)]
fn active_core_count() -> usize {
    crate::probed_core_count().clamp(1, MAX_CORES)
}
#[cfg(not(kernel_smp_shared))]
fn active_core_count() -> usize {
    1
}

/// Sum every thread slot's live CPU time (`akuma_exec::threading::get_thread_cpu_time`,
/// the same counter `sys_get_cpu_stats`/`top`'s CORE column reads) into a
/// system-wide busy total and a per-core busy total, bucketed by each
/// thread's last-run core. Free slots contribute 0 (their counter is reset on
/// free), so this needs no separate liveness check. Per-core bucketing is an
/// approximation — a thread that migrated mid-quantum is billed entirely to
/// wherever it happened to run last — but it's the only per-core signal this
/// kernel keeps, and it's exactly what already backs `top`'s CORE column.
///
/// Returns `(total_busy_us, [u64; MAX_CORES], cores)` — a fixed array rather
/// than a `Vec`: this runs on every `/proc/stat`/`/proc/uptime` read, which
/// `top` hits on every refresh, so it stays off the heap like the rest of
/// this module's virtual-file renderers (`render_mounts` et al.).
fn cpu_time_snapshot() -> (u64, [u64; MAX_CORES], usize) {
    let cores = active_core_count();
    let mut per_core = [0u64; MAX_CORES];
    let mut total = 0u64;
    for tid in 0..crate::cfg_max_threads() {
        let us = threading::get_thread_cpu_time(tid);
        if us == 0 {
            continue;
        }
        total += us;
        let core = threading::get_thread_last_core(tid) as usize;
        if core < cores {
            per_core[core] += us;
        }
    }
    (total, per_core, cores)
}

/// `/proc/stat` (Linux `cpu`/`cpuN`/`processes`/`procs_running`/`procs_blocked`
/// lines) — what busybox `top` reads to compute overall CPU%; without this
/// file `top` refuses to start at all (`xfopen_for_read("/proc/stat")` is
/// unconditional). We don't track a user/kernel-time split, so all busy time
/// is attributed to the `user` field; `idle` is derived as `wall_time_per_core
/// - busy_time`, which is exactly what makes the number move sensibly between
/// two reads (the only thing `top` actually needs from it).
fn render_stat_system(buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    let uptime_us = akuma_primitives::clock::uptime_us();
    let (busy_total, busy_per_core, cores) = cpu_time_snapshot();
    let idle_total = (uptime_us.saturating_mul(cores as u64)).saturating_sub(busy_total) / JIFFY_US;
    let user_total = busy_total / JIFFY_US;

    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = writeln!(w, "cpu  {user_total} 0 0 {idle_total} 0 0 0 0 0 0");
    for (i, busy) in busy_per_core[..cores].iter().enumerate() {
        let idle = uptime_us.saturating_sub(*busy) / JIFFY_US;
        let user = busy / JIFFY_US;
        let _ = writeln!(w, "cpu{i} {user} 0 0 {idle} 0 0 0 0 0 0");
    }

    let (_total, running, blocked) = process::count_process_states();
    let total_forked = process::NEXT_PID.load(core::sync::atomic::Ordering::Relaxed).saturating_sub(1);

    let _ = writeln!(w, "intr 0");
    let _ = writeln!(w, "ctxt 0");
    let _ = writeln!(w, "btime 0");
    let _ = writeln!(w, "processes {total_forked}");
    let _ = writeln!(w, "procs_running {running}");
    let _ = writeln!(w, "procs_blocked {blocked}");
    pos
}

/// `/proc/meminfo` — what busybox `free` reads (`MemTotal`/`MemFree`/
/// `Buffers`/`Cached`/`SwapTotal`/`SwapFree`, matched by key prefix, not by
/// column position). Real numbers off the PMM page allocator
/// (`akuma_pmm::stats`) and the file page cache (`akuma_fpcache::len`,
/// the reclaimable-on-demand cache `MemAvailable` accounts for). No swap
/// exists on this kernel, so the `Swap*` fields are always 0 — a legitimate
/// value, not a placeholder.
///
/// `Slab:` is the **kernel heap** (`akuma_alloc::stats().allocated`), and it is
/// here because it is the only number in the system that can witness a
/// kernel-heap leak from ring 3: `free` reads the PMM, and the PMM did not move
/// at all across the 135 MB excursion into amd64's whole-file `fd` cache
/// (`docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md` § "And a method correction").
/// amd64 used to render that number in `Cached:` from its own synthetic
/// `/proc`; 4b batch 2c deleted that view in favour of this one, and with it —
/// silently — the witness, because `Cached:` here is the *file page* cache and
/// amd64 has none. Reported under its Linux name instead of borrowing another
/// field's, so `scripts/utils/amd64_ring3_check.py` reads a row that means what
/// it says on both kernels. busybox `free` matches by key prefix and ignores
/// rows it does not know, so adding one costs its output nothing.
fn render_meminfo(buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    let (total_pages, _allocated, free_pages) = akuma_pmm::stats();
    let page_kb = (akuma_pmm::PAGE_SIZE / 1024) as u64;
    let total_kb = total_pages as u64 * page_kb;
    let free_kb = free_pages as u64 * page_kb;
    let cached_kb = akuma_fpcache::len() as u64 * page_kb;
    let available_kb = free_kb + cached_kb;
    let slab_kb = akuma_alloc::stats().allocated as u64 / 1024;

    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = writeln!(w, "MemTotal:       {total_kb:>10} kB");
    let _ = writeln!(w, "MemFree:        {free_kb:>10} kB");
    let _ = writeln!(w, "MemAvailable:   {available_kb:>10} kB");
    let _ = writeln!(w, "Buffers:        {:>10} kB", 0);
    let _ = writeln!(w, "Cached:         {cached_kb:>10} kB");
    let _ = writeln!(w, "SwapCached:     {:>10} kB", 0);
    let _ = writeln!(w, "SwapTotal:      {:>10} kB", 0);
    let _ = writeln!(w, "SwapFree:       {:>10} kB", 0);
    let _ = writeln!(w, "Shmem:          {:>10} kB", 0);
    let _ = writeln!(w, "Slab:           {slab_kb:>10} kB");
    pos
}

/// The largest `/proc/cpuinfo` block a renderer may produce; the stack scratch
/// every read renders one block into.
const CPUINFO_BLOCK: usize = 2048;

/// Render logical CPU `core`'s block into `scratch`: the kernel's renderer if it
/// registered one, else a bare `processor : N` block.
fn cpuinfo_block(core: usize, scratch: &mut [u8; CPUINFO_BLOCK]) -> usize {
    use akuma_primitives::console::FmtBuf;
    if let Some(f) = crate::cpuinfo_renderer() {
        return f(core, scratch).min(CPUINFO_BLOCK);
    }
    let mut pos = 0usize;
    let mut w = FmtBuf { buf: &mut scratch[..], pos: &mut pos };
    let _ = writeln!(w, "processor\t: {core}\n");
    pos
}

/// Copy `/proc/cpuinfo` from byte `offset` into `buf`, one block at a time:
/// each core's block is rendered into a stack scratch, the part overlapping
/// `[offset, offset + buf.len())` is copied out and the block is dropped. A
/// read costs `cores` renders, which is nothing next to the syscall.
fn cpuinfo_read(offset: usize, buf: &mut [u8]) -> usize {
    let mut scratch = [0u8; CPUINFO_BLOCK];
    let (mut base, mut out) = (0usize, 0usize);
    for core in 0..active_core_count() {
        let len = cpuinfo_block(core, &mut scratch);
        let pos = offset + out;
        if pos >= base && pos < base + len {
            let n = (len - (pos - base)).min(buf.len() - out);
            buf[out..out + n].copy_from_slice(&scratch[pos - base..pos - base + n]);
            out += n;
            if out == buf.len() {
                break;
            }
        }
        base += len;
    }
    out
}

/// `/proc/cpuinfo`'s length.
fn cpuinfo_len() -> usize {
    let mut scratch = [0u8; CPUINFO_BLOCK];
    (0..active_core_count()).map(|c| cpuinfo_block(c, &mut scratch)).sum()
}

/// `/proc/uptime` — `uptime_seconds idle_seconds`. `idle_seconds` follows
/// Linux SMP semantics (summed across every core, so it can exceed wall time).
fn render_uptime(buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    let uptime_us = akuma_primitives::clock::uptime_us();
    let (busy_total, _busy_per_core, cores) = cpu_time_snapshot();
    let idle_us = (uptime_us.saturating_mul(cores as u64)).saturating_sub(busy_total);
    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = writeln!(
        w,
        "{}.{:02} {}.{:02}",
        uptime_us / 1_000_000, (uptime_us / 10_000) % 100,
        idle_us / 1_000_000, (idle_us / 10_000) % 100,
    );
    pos
}

/// `/proc/loadavg` — this kernel doesn't track a decaying run-queue average,
/// so the three load figures are always `0.00` (an honest "not tracked", not
/// a plausible-looking fake); the `runnable/total` and `last_pid` fields are
/// real.
fn render_loadavg(buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    let (total, running, _blocked) = process::count_process_states();
    let last_pid = process::NEXT_PID.load(core::sync::atomic::Ordering::Relaxed).saturating_sub(1);
    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = writeln!(w, "0.00 0.00 0.00 {running}/{total} {last_pid}");
    pos
}

/// Room for `/proc/health`. Seven short `key value` lines; the renderer
/// tolerates a short buffer by truncating, never by panicking.
const HEALTH_BUF: usize = 512;

/// `/proc/health` — counters for machinery that heals or sheds load silently.
///
/// Each of these runs without announcing itself: a self-heal that unwedges a
/// lock, a valve that reclaims pages or sockets under pressure, a spawn that is
/// abandoned before its first instruction. Their counters existed and nothing
/// read them, so a box that was *using* them looked identical to one that never
/// needed to (`docs/archive/POST_AMD64_CODE_AUDIT.md` §1.3). A file rather than
/// a periodic console line because the console is the channel of last resort on
/// the bare-metal boxes and these should not push `[rtl]`'s stall dump off it.
///
/// One `key value` per line, all decimal. A kernel may append more through
/// [`crate::set_health_extra`]. The keys this file renders itself:
///
/// * `bkl_recoveries` — every forced `now_serving` advance, benign or not.
/// * `bkl_lost_ticket_recoveries` — the wedge subset. Must stay `0`.
/// * `bkl_stuck_suppressed` — `[BKL] stuck` lines folded into a reported episode.
/// * `bkl_max_wait_spins` — the longest ticket wait seen this boot.
/// * `trampoline_bails` — threads abandoned before their first user instruction.
/// * `pressure_reclaimed_pages` — pages the allocator's pressure path drained
///   from retired processes.
/// * `socket_slots_reclaimed` — sockets reclaimed early under table pressure.
fn render_health(buf: &mut [u8]) -> usize {
    use akuma_primitives::console::FmtBuf;
    let mut pos = 0usize;
    {
        let mut w = FmtBuf { buf: &mut *buf, pos: &mut pos };
        let _ = writeln!(w, "bkl_recoveries {}", akuma_bkl::sync::kernel_lock_recoveries());
        let _ = writeln!(
            w,
            "bkl_lost_ticket_recoveries {}",
            akuma_bkl::sync::kernel_lock_lost_ticket_recoveries()
        );
        let _ = writeln!(w, "bkl_stuck_suppressed {}", akuma_bkl::sync::kernel_lock_stuck_suppressed());
        let _ = writeln!(w, "bkl_max_wait_spins {}", akuma_bkl::sync::max_wait_spins());
        let _ = writeln!(w, "trampoline_bails {}", process::trampoline_bail_count());
        let _ = writeln!(w, "pressure_reclaimed_pages {}", process::reclaim::pressure_reclaimed_pages());
        let _ = writeln!(
            w,
            "socket_slots_reclaimed {}",
            akuma_net::SOCKET_SLOTS_RECLAIMED.load(core::sync::atomic::Ordering::Relaxed)
        );
    }
    if let Some(extra) = crate::health_extra() {
        pos += extra(&mut buf[pos..]).min(buf.len() - pos);
    }
    pos
}

// ============================================================================
// ProcFilesystem
// ============================================================================

/// Virtual filesystem for process information
pub struct ProcFilesystem;

impl ProcFilesystem {
    /// Create a new procfs instance
    #[must_use]
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
    /// `<pid>/oom_score_adj` and its two siblings. They are constant-`0` files
    /// that swallow writes: Akuma has no OOM killer to steer, but Chromium's
    /// zygote host writes a renderer's `oom_score_adj` on every spawn and
    /// logged `Failed to adjust OOM score of renderer with pid N: No such file
    /// or directory` ~45 times a page (2026-10-09; the failure is non-fatal in
    /// Chromium's source, `AdjustRendererOOMScore` only `PLOG(ERROR)`s it).
    /// Reading `0` is the truthful answer for a kernel that never kills by score.
    const OOM_FILES: [&'static str; 3] = ["oom_score_adj", "oom_adj", "oom_score"];

    /// `metadata` before ownership is applied — see `Filesystem::metadata`
    /// below. The node has already been resolved, so existence and the box
    /// rule are settled; this only measures it.
    ///
    /// Sizes are fresh renders into stack buffers where the file is small (a
    /// `/proc` file's length is a property of the moment, and a reader that
    /// trusts `st_size` truncates on a stale one). `cmdline`, `status`,
    /// `maps` and `statm` still render through their `Vec`/`String`
    /// renderers to be measured — unchanged from before, and the one
    /// remaining allocation on this path.
    fn metadata_of(node: Node) -> Result<Metadata, FsError> {
        let inode = node_inode(node);
        let meta = |mode: u32, size: u64, links: Option<u32>| Metadata {
            is_dir: mode & 0o170000 == 0o040000,
            size,
            inode,
            mode,
            created: None,
            modified: None,
            accessed: None,
            links,
            uid: 0,
            gid: 0,
        };
        const DIR: u32 = 0o40555;
        const RO: u32 = 0o100444;
        const RW: u32 = 0o100644;
        const LINK: u32 = 0o120_777;

        Ok(match node {
            Node::Root | Node::NetDir | Node::SysvipcDir | Node::Pid(_) | Node::FdDir(_) => {
                meta(DIR, 0, None)
            }
            // A link count of `2 + threads`, as on Linux — Chromium reads
            // "single-threaded" off `st_nlink == 3`.
            Node::TaskDir(tgid) => {
                let threads = tree::live_member_count(&KernelModel::now(), tgid) as u32;
                meta(DIR, 0, Some(2 + threads))
            }
            // A symlink's size is its target's length.
            Node::SelfLink => {
                let tgid = KernelModel::now().current_tgid.unwrap_or(0);
                let mut digits = [0u8; 10];
                let mut pos = 0;
                let _ = write!(
                    akuma_primitives::console::FmtBuf { buf: &mut digits, pos: &mut pos },
                    "{tgid}"
                );
                meta(LINK, pos as u64, None)
            }
            Node::Global(g) => match g {
                tree::Global::Dac => meta(RW, 2, None),
                tree::Global::Boxes | tree::Global::Cores | tree::Global::Filesystems => {
                    meta(RO, 0, None)
                }
                tree::Global::Mounts => {
                    let pid = process::current_process_shared().map(|p| p.pid);
                    let mut probe = [0u8; 2048];
                    meta(RO, crate::render_mounts(caller_box_id(), pid, &mut probe) as u64, None)
                }
                tree::Global::Power => {
                    let size = crate::power_renderer().map_or(0, |render| {
                        let mut scratch = [0u8; 768];
                        render(&mut scratch).min(scratch.len())
                    });
                    meta(RO, size as u64, None)
                }
                tree::Global::Cpuinfo => meta(RO, cpuinfo_len() as u64, None),
                tree::Global::Health => {
                    let mut scratch = [0u8; HEALTH_BUF];
                    meta(RO, render_health(&mut scratch) as u64, None)
                }
                tree::Global::Meminfo | tree::Global::Stat | tree::Global::Uptime | tree::Global::Loadavg => {
                    let mut scratch = [0u8; 1024];
                    let size = match g {
                        tree::Global::Meminfo => render_meminfo(&mut scratch),
                        tree::Global::Stat => render_stat_system(&mut scratch),
                        tree::Global::Uptime => render_uptime(&mut scratch),
                        _ => render_loadavg(&mut scratch),
                    };
                    meta(RO, size as u64, None)
                }
            },
            Node::Net(_) | Node::SysvipcMsg => meta(RO, 0, None),
            Node::PidFile(v, f) => match f {
                PidFile::Mounts => {
                    let mut probe = [0u8; 2048];
                    let size = crate::render_mounts(caller_box_id(), Some(v.pid), &mut probe);
                    meta(RO, size as u64, None)
                }
                PidFile::OomScoreAdj | PidFile::OomAdj | PidFile::OomScore => meta(RW, 2, None),
                PidFile::Maps | PidFile::Statm => {
                    let data = pid_address_space_file(v.pid, f.name()).ok_or(FsError::NotFound)?;
                    meta(RO, data.len() as u64, None)
                }
                PidFile::Exe => {
                    let proc = process::lookup_process_shared(v.pid).ok_or(FsError::NotFound)?;
                    meta(LINK, proc.image_exe().len() as u64, None)
                }
                PidFile::Syscalls => meta(RO, 0, None),
                PidFile::Cmdline | PidFile::Status | PidFile::Stat => {
                    let proc = process::lookup_process_shared(v.pid).ok_or(FsError::NotFound)?;
                    let size = match f {
                        PidFile::Cmdline => proc_cmdline_bytes(proc).len(),
                        PidFile::Stat => {
                            let mut stat_buf = [0u8; 256];
                            render_pid_stat(proc, &mut stat_buf)
                        }
                        _ => proc_status_text(proc).len(),
                    };
                    meta(RO, size as u64, None)
                }
            },
            // Listed and `lstat`ed as a symlink (the syscall layer asks
            // `is_symlink` first); a followed `stat` that could not chase it —
            // a pipe, a socket, the stdio buffers — lands here and reports the
            // buffer as a regular file, as it always has.
            Node::Fd(v, fd) => {
                let proc = process::lookup_process_shared(v.pid).ok_or(FsError::NotFound)?;
                let size = match fd {
                    0 => proc.stdin.lock().len() as u64,
                    1 => proc.stdout.lock().len() as u64,
                    _ => 0,
                };
                meta(RO, size, None)
            }
        })
    }

    /// A write to `/proc/dac`: `1` turns permission enforcement on, `0` off.
    /// Only a host (box 0) uid-0 process may flip it — a boxed or dropped
    /// process switching enforcement off would be the whole policy undone.
    /// Anything else written is `InvalidPath` (`EINVAL`).
    fn write_dac(data: &[u8]) -> Result<usize, FsError> {
        let me = process::current_process_shared();
        if me.is_some_and(|p| p.box_id != 0) {
            return Err(FsError::NotFound);
        }
        if !process::current_creds().is_root() {
            return Err(FsError::PermissionDenied);
        }
        match data.first() {
            Some(b'1') => akuma_isolation::dac::set_enforcing(true),
            Some(b'0') => akuma_isolation::dac::set_enforcing(false),
            _ => return Err(FsError::InvalidPath),
        }
        Ok(data.len())
    }

    /// The pid in `<pid>/<one of OOM_FILES>`, if `path` is one.
    fn pid_oom_file(path: &str) -> Option<Pid> {
        let (pid, name) = path.split_once('/')?;
        if Self::OOM_FILES.contains(&name) { pid.parse().ok() } else { None }
    }

    /// Whether the current caller may see `pid` (the box rule `cmdline` uses).
    fn oom_visible(pid: Pid) -> bool {
        let box_id = akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);
        Self::process_visible(pid, box_id)
    }

    fn process_visible(pid: Pid, viewer_box_id: u64) -> bool {
        match process::lookup_process_shared(pid) {
            Some(proc) => viewer_box_id == 0 || proc.box_id == viewer_box_id,
            None => false,
        }
    }

    /// Returns a human-readable description string for an fd entry (used by readlinkat).
    /// These strings are NOT necessarily valid filesystem paths.
    #[must_use]
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
            FileDescriptor::DevFb => String::from("/dev/fb0"),
            FileDescriptor::DevWifi => String::from("/dev/wifi0"),
            FileDescriptor::DevPower => String::from("/dev/power"),
            FileDescriptor::DevKvm => String::from("/dev/kvm0"),
            FileDescriptor::DevMouse => String::from("/dev/input/mouse0"),
            // `ttyname(3)` reads this link: the slave's name is what `tty`,
            // `ps` and `sshd` print for a pty session.
            FileDescriptor::PtyMaster(_) => String::from("/dev/ptmx"),
            FileDescriptor::PtySlave(n) => format!("/dev/pts/{n}"),
            FileDescriptor::Tap { .. } => String::from("/dev/net/tap0"),
            FileDescriptor::RumpSocket { rump_fd, .. } => format!("socket:[rump:{rump_fd}]"),
            FileDescriptor::Stdin => String::from("/dev/stdin"),
            FileDescriptor::DevTty => String::from("/dev/tty"),
            FileDescriptor::Stdout => String::from("/dev/stdout"),
            FileDescriptor::Stderr => String::from("/dev/stderr"),
            FileDescriptor::ChildStdout(child_pid) => format!("pipe:[child:{child_pid}]"),
            FileDescriptor::PidFd(id) => format!("anon_inode:[pidfd:{id}]"),
            FileDescriptor::BlockDev { idx, .. } => match akuma_virtio::block::device_name(*idx as usize) {
                Some(name) => format!("/dev/{name}"),
                None => format!("blockdev:[{idx}]"),
            },
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
                target.stdout.lock().write_with_limit(data, crate::cfg_proc_stdout_max_size());
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
///
/// Unlike `read_symlink`, this returns descriptions for ALL fd types (pipes, sockets, etc.),
/// not just File fds. Used by the readlinkat syscall to report virtual fd targets.
#[must_use]
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

    /// The listing is the tree's (`akuma_procfs::tree::list`): every name it
    /// emits resolves, to the kind it is emitted with, which the host
    /// simulation checks across the whole tree. `.` and `..` are not listed
    /// here — the `getdents64` arm adds them for every filesystem.
    ///
    /// Allocates the `Vec<DirEntry>` the trait returns and a `String` per
    /// name; nothing under a lock (`KernelModel` collects pids and descriptor
    /// numbers before it calls back).
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let mut entries = Vec::new();
        tree::list(&KernelModel::now(), path, &mut |name, kind| {
            entries.push(DirEntry {
                name: String::from(name),
                is_dir: kind == Kind::Dir,
                is_symlink: kind == Kind::Symlink,
                size: 0,
            });
        })
        .map_err(|e| match e {
            tree::ListError::NotFound => FsError::NotFound,
            tree::ListError::NotADirectory => FsError::NotADirectory,
        })?;
        Ok(entries)
    }

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        let canon = canonical(path);
        let path = canon.as_str();

        // Handle virtual files first (boxes, cores, net/tcp, sysvipc/msg,
        // mounts, filesystems, <pid>/syscalls, etc.). This list must name every
        // path `read_file` renders — `metadata` and `list_dir` advertising a
        // file that `read_at` does not forward is how `/proc/cores` came to
        // `open()` and `stat()` fine while every `read()` returned NotFound
        // (docs/archive/DEVBOX_ISSUES.md Issue 4).
        if path == "boxes"
            || path == "cores"
            || path == "dac"
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

        // /proc/power — same shape as the four below: a stack buffer, no `Vec`.
        if path == "power" && let Some(render) = crate::power_renderer() {
            let mut scratch = [0u8; 768];
            let len = render(&mut scratch).min(scratch.len());
            if offset >= len {
                return Ok(0);
            }
            let n = buf.len().min(len - offset);
            buf[..n].copy_from_slice(&scratch[offset..offset + n]);
            return Ok(n);
        }

        // /proc/health — a stack buffer, like the files around it.
        if path == "health" {
            let mut scratch = [0u8; HEALTH_BUF];
            let len = render_health(&mut scratch);
            if offset >= len {
                return Ok(0);
            }
            let n = buf.len().min(len - offset);
            buf[..n].copy_from_slice(&scratch[offset..offset + n]);
            return Ok(n);
        }

        // /proc/meminfo, /proc/stat, /proc/uptime, /proc/loadavg — rendered
        // straight into the caller's buffer, no intermediate `Vec`: this is
        // what busybox `top`/`free` re-`read()` on every refresh.
        if path == "cpuinfo" {
            return Ok(cpuinfo_read(offset, buf));
        }
        if path == "meminfo" || path == "stat" || path == "uptime" || path == "loadavg" {
            let mut scratch = [0u8; 1024];
            let len = match path {
                "meminfo" => render_meminfo(&mut scratch),
                "stat" => render_stat_system(&mut scratch),
                "uptime" => render_uptime(&mut scratch),
                _ => render_loadavg(&mut scratch),
            };
            if offset >= len {
                return Ok(0);
            }
            let n = buf.len().min(len - offset);
            buf[..n].copy_from_slice(&scratch[offset..offset + n]);
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
        if crate::cfg_proc_syscall_log_enabled() {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "syscalls"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let data = akuma_syscalls_log::get_formatted(pid, caller_box_id())
                        .ok_or(FsError::NotFound)?;
                    if offset >= data.len() {
                        return Ok(0);
                    }
                    let n = buf.len().min(data.len() - offset);
                    buf[..n].copy_from_slice(&data[offset..offset + n]);
                    return Ok(n);
                }
        }

        // Handle <pid>/stat separately, straight into the caller's buffer with
        // no intermediate `Vec` — this is what `top`/`ps` re-read on every
        // refresh for every visible process, unlike `cmdline`/`status` below.
        {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "stat"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
                    let current_box_id =
                        akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);
                    if current_box_id != 0 && proc.box_id != current_box_id {
                        return Err(FsError::NotFound);
                    }
                    let mut stat_buf = [0u8; 256];
                    let len = render_pid_stat(proc, &mut stat_buf);
                    if offset >= len {
                        return Ok(0);
                    }
                    let n = buf.len().min(len - offset);
                    buf[..n].copy_from_slice(&stat_buf[offset..offset + n]);
                    return Ok(n);
                }
        }

        // <pid>/oom_score_adj and siblings: constant "0\n" (see `OOM_FILES`).
        if let Some(pid) = Self::pid_oom_file(path) {
            if !Self::oom_visible(pid) {
                return Err(FsError::NotFound);
            }
            let data = b"0\n";
            if offset >= data.len() {
                return Ok(0);
            }
            let n = buf.len().min(data.len() - offset);
            buf[..n].copy_from_slice(&data[offset..offset + n]);
            return Ok(n);
        }

        // <pid>/maps and <pid>/statm — see `pid_address_space_file`.
        {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && (parts[1] == "maps" || parts[1] == "statm")
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let data = pid_address_space_file(pid, parts[1]).ok_or(FsError::NotFound)?;
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
            if parts.len() == 2 && (parts[1] == "cmdline" || parts[1] == "status")
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    let proc = process::lookup_process_shared(pid).ok_or(FsError::NotFound)?;
                    let current_box_id =
                        akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id);
                    if current_box_id != 0 && proc.box_id != current_box_id {
                        return Err(FsError::NotFound);
                    }
                    let data = match parts[1] {
                        "cmdline" => proc_cmdline_bytes(proc),
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
        let canon = canonical(path);
        let path = canon.as_str();
        let current_proc = akuma_exec::process::current_process_shared();
        let current_box_id = current_proc.as_ref().map_or(0, |p| p.box_id);

        // /proc/dac — whether file-permission and signal-uid checks refuse
        // anything: `1` or `0`. Host only, like `boxes`. Writing it is the A/B
        // lever and the escape hatch (`akuma_isolation::dac::set_enforcing`,
        // `docs/archive/UNIX_PERMISSIONS.md` §6.5).
        if path == "dac" {
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            let on = akuma_isolation::dac::enforcing();
            return Ok(alloc::vec![if on { b'1' } else { b'0' }, b'\n']);
        }

        // Handle /proc/boxes
        if path == "boxes" {
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            
            let boxes = process::list_boxes();
            if crate::cfg_syscall_debug_info_enabled() {
                akuma_primitives::safe_print!(128, "[ProcFS] Reading boxes (count={})\n", boxes.len());
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
        // kernel, so every row is `online`: one per core the kernel brought up
        // (`active_core_count`, the same count `/proc/stat`'s `cpuN` rows use),
        // core 0 as `bsp` and the rest `ap`. Until 2026-09-26 this was a static
        // `0 online bsp` on every build, so a four-core box read as one.
        if path == "cores" {
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            let cores = active_core_count();
            let mut out = String::with_capacity(16 + 12 * cores);
            out.push_str("core state role\n");
            for i in 0..cores {
                let role = if i == 0 { "bsp" } else { "ap" };
                let _ = writeln!(out, "{i} online {role}");
            }
            return Ok(out.into_bytes());
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

        // /proc/power — the one allocation is the trait boundary's.
        if path == "power" && let Some(render) = crate::power_renderer() {
            let mut scratch = [0u8; 768];
            let len = render(&mut scratch).min(scratch.len());
            return Ok(scratch[..len].to_vec());
        }

        // /proc/health — the one allocation is the trait boundary's.
        if path == "health" {
            let mut scratch = [0u8; HEALTH_BUF];
            let len = render_health(&mut scratch);
            return Ok(scratch[..len].to_vec());
        }

        // /proc/meminfo, /proc/stat, /proc/uptime, /proc/loadavg — system-wide,
        // same visibility as /proc/mounts/filesystems (every box sees them;
        // real Linux containers do too absent a dedicated cgroup shim).
        // `read_at` (what `top`/`free`'s actual `read()` calls land in) renders
        // these directly into its own buffer and never reaches here; this path
        // exists for callers that want the whole file as a `Vec` (e.g. `cat`
        // via a full-file read), so the one allocation below is the trait
        // boundary's, not this renderer's.
        if path == "cpuinfo" {
            let mut v = alloc::vec![0u8; cpuinfo_len()];
            let n = cpuinfo_read(0, &mut v);
            v.truncate(n);
            return Ok(v);
        }
        if path == "meminfo" || path == "stat" || path == "uptime" || path == "loadavg" {
            let mut scratch = [0u8; 1024];
            let len = match path {
                "meminfo" => render_meminfo(&mut scratch),
                "stat" => render_stat_system(&mut scratch),
                "uptime" => render_uptime(&mut scratch),
                _ => render_loadavg(&mut scratch),
            };
            return Ok(scratch[..len].to_vec());
        }

        // /proc/mounts — the viewer's own mount set (its namespace if boxed,
        // the global table on the host). A box sees which paths are mounted
        // into it, never where they came from — crate::render_mounts
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
        // bytes, packets and drops are real since 2026-10-07
        // (`akuma_net::ifstats`, counted in `LoopbackAwareDevice`); the other
        // columns are untracked and read `0`.
        if path == "net/dev" {
            // The column layout is `akuma-syscalls-net`, shared with the
            // `SIOCGIFCONF` path and the amd64 port. The counters are
            // `akuma_net::ifstats`, counted where every frame passes.
            let (lo, wire) = akuma_net::ifstats::snapshot();
            let ifaces = [
                akuma_syscalls_net::Interface::loopback().with_stats(if_stats(lo)),
                akuma_syscalls_net::Interface::ethernet([0; 4], 0, [0; 6], 0).with_stats(if_stats(wire)),
            ];
            let mut out = String::new();
            let _ = akuma_syscalls_net::write_proc_net_dev(&ifaces, &mut out);
            return Ok(out.into_bytes());
        }

        #[cfg(feature = "sc-sysv-ipc")]
        if path == "sysvipc/msg" && crate::cfg_proc_sysvipc_enabled() {
            let queues = akuma_syscalls_ipc::list_msg_queues();
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
        if crate::cfg_proc_syscall_log_enabled() {
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 && parts[1] == "syscalls"
                && let Ok(pid) = parts[0].parse::<Pid>() {
                    return akuma_syscalls_log::get_formatted(pid, current_box_id)
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

        if let Some(pid) = Self::pid_oom_file(path) {
            if !Self::oom_visible(pid) {
                return Err(FsError::NotFound);
            }
            return Ok(b"0\n".to_vec());
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
                        "stat" => {
                            let mut stat_buf = [0u8; 256];
                            let len = render_pid_stat(proc, &mut stat_buf);
                            stat_buf[..len].to_vec()
                        }
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
        if path.trim_start_matches('/') == "dac" {
            return Self::write_dac(data).map(|_| ());
        }
        if let Some(pid) = Self::pid_oom_file(path) {
            return if Self::oom_visible(pid) { Ok(()) } else { Err(FsError::NotFound) };
        }
        // All-or-nothing contract: a caller with no way to report a residue must
        // not be told a partial stdin write succeeded. `write_at` is the path
        // `sys_write` actually takes and it reports the count instead.
        match Self::write_fd_data(path, data)? {
            n if n == data.len() => Ok(()),
            _ => Err(FsError::NoSpace),
        }
    }

    fn write_at(&self, path: &str, _offset: usize, data: &[u8]) -> Result<usize, FsError> {
        if path.trim_start_matches('/') == "dac" {
            return Self::write_dac(data);
        }
        if let Some(pid) = Self::pid_oom_file(path) {
            return if Self::oom_visible(pid) { Ok(data.len()) } else { Err(FsError::NotFound) };
        }
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
        resolve_node(path).is_some()
    }

    /// Every entry under `<pid>/` belongs to that process's uid and gid, as on
    /// Linux — busybox `ps` reads a process's user off `stat("/proc/<pid>")`.
    /// Applied once here rather than in each of `metadata_unowned`'s arms.
    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        let node = resolve_node(path).ok_or(FsError::NotFound)?;
        let mut meta = Self::metadata_of(node)?;
        if let Some(pid) = node.owner()
            && let Some(p) = process::lookup_process_shared(pid)
        {
            let c = p.creds.get();
            meta.uid = c.uid();
            meta.gid = c.gid();
        }
        Ok(meta)
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        // Procfs doesn't use block storage
        Ok(FsStats {
            block_size: 4096,
            total_blocks: 0,
            free_blocks: 0,
        })
    }

    /// The three symlinks the tree has: `self`, `<pid>/exe` and
    /// `<pid>/fd/<n>` — through `self/` and `<tgid>/task/<tid>/` alike, since
    /// the tree resolved the path (and applied the box rule) first.
    fn read_symlink(&self, path: &str) -> Result<String, FsError> {
        match resolve_node(path).ok_or(FsError::NotFound)? {
            // `/proc/self` itself: Linux answers with the caller's thread-group
            // id. Nothing answered the read once, so a program that opens
            // `/proc/self` as a directory (`open(…, O_PATH | O_DIRECTORY)`,
            // which is how procps-ng's `ps` starts) was told `ENOENT` and
            // printed "Error, do this: mount -t proc proc /proc".
            Node::SelfLink => {
                let tgid = KernelModel::now().current_tgid.ok_or(FsError::NotFound)?;
                Ok(format!("{tgid}"))
            }
            // `<pid>/exe` — the binary the process is running, as a magic
            // symlink. It lived in `akuma-syscalls-glue` as two
            // `/proc/self/exe` interceptions ahead of the VFS, which could only
            // ever describe **self**; this serves any pid the caller can see.
            Node::PidFile(v, PidFile::Exe) => {
                let proc = process::lookup_process_shared(v.pid).ok_or(FsError::NotFound)?;
                Ok(proc.image_exe())
            }
            // `<pid>/fd/<n>`: a path only for a `File` descriptor. A pipe or
            // socket answers `NotFound` so `resolve_symlinks` does not chase
            // "pipe:[5]" as if it were a path; `readlinkat` asks
            // `proc_fd_description` for those strings instead.
            Node::Fd(v, fd) => {
                let proc = process::lookup_process_shared(v.pid).ok_or(FsError::NotFound)?;
                match proc.get_fd(fd) {
                    Some(akuma_exec::process::FileDescriptor::File(f)) => Ok(f.path),
                    _ => Err(FsError::NotFound),
                }
            }
            _ => Err(FsError::NotFound),
        }
    }

    fn is_symlink(&self, path: &str) -> bool {
        resolve_node(path).is_some_and(|n| n.kind() == Kind::Symlink)
    }
}

/// `akuma_net::ifstats` counts into `/proc/net/dev`'s columns.
fn if_stats(c: akuma_net::ifstats::Counts) -> akuma_syscalls_net::IfStats {
    akuma_syscalls_net::IfStats {
        rx_bytes: c.rx_bytes,
        rx_packets: c.rx_packets,
        rx_drop: c.rx_drop,
        tx_bytes: c.tx_bytes,
        tx_packets: c.tx_packets,
        tx_drop: c.tx_drop,
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    fn lines(buf: &[u8]) -> Vec<&str> {
        core::str::from_utf8(buf).unwrap().lines().collect()
    }

    #[test]
    fn health_is_key_value_lines_that_fit() {
        let mut buf = [0u8; HEALTH_BUF];
        let n = render_health(&mut buf);
        assert!(n > 0 && n < HEALTH_BUF, "rendered {n} of {HEALTH_BUF} bytes - keep headroom");
        let rows = lines(&buf[..n]);
        assert_eq!(rows.len(), 7);
        for row in rows {
            let (key, value) = row.split_once(' ').expect("`key value`");
            assert!(!key.is_empty() && value.parse::<u64>().is_ok(), "bad row {row:?}");
        }
    }

    #[test]
    fn a_short_buffer_truncates_instead_of_panicking() {
        let mut tiny = [0u8; 8];
        assert!(render_health(&mut tiny) <= tiny.len());
    }
}
