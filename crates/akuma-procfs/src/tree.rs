//! The **shape** of `/proc`: which names exist, what kind each one is, and
//! what a path resolves to — as pure functions over a [`ProcModel`].
//!
//! # Why this exists
//!
//! `akuma-vfs-glue`'s `ProcFilesystem` (mounted by **both** kernels) used to
//! answer one structural question five separate ways: `read_dir`, `exists`,
//! `metadata`, `is_symlink` and `read_symlink` each parsed the path for
//! themselves. They drifted, and every drift is the same bug from a walker's
//! point of view — *a directory lists a name that does not resolve, or that
//! resolves to a different kind than the listing claimed*:
//!
//! - `<tgid>/task/<tid>/` listed `task`, because the thread directory was an
//!   alias of `<tid>/`, whose own listing adds `task` for a group leader. Linux
//!   has no `task` inside a thread directory. `<tgid>/task/<tid>/task` then
//!   *listed* `<tid>`, and `<tgid>/task/<tid>/task/<tid>` did not resolve.
//! - `<pid>/fd` always listed `2`, while `exists` asked the descriptor table,
//!   so a process that had closed stderr listed an entry `stat` refused.
//! - `<tgid>/task/<tid>/exe` and `.../fd/<n>` were listed as symlinks, and
//!   `is_symlink`/`read_symlink` — which never learned the `task` alias —
//!   denied both.
//!
//! [`resolve`] and [`list`] are now the only parser. The renderers (the bytes
//! of `stat`, `status`, `maps`, …) stay where they were; they are handed a
//! [`Node`], or the canonical path [`Node::write_canonical`] spells for it.
//!
//! # What this does not own
//!
//! `.` and `..`. A `Filesystem::read_dir` lists children; the `getdents64`
//! arm adds the two dot records for *every* filesystem
//! (`akuma_syscalls_linux::dirent::DOT_ENTRIES`). [`list`] never emits them,
//! and the simulation asserts that, because a filesystem that added them too
//! would make every directory list them twice.
//!
//! # Allocation
//!
//! None. [`resolve`] parses in place; [`list`] formats each numeric name into
//! a 10-byte stack buffer and hands it to the caller's `emit`. Whatever the
//! caller builds from those names (the kernel builds the `Vec<DirEntry>` the
//! `Filesystem` trait returns) is the caller's allocation.

use core::fmt;

/// A process or thread id, as both kernels spell it.
pub type Pid = u32;

/// What a name is, as `d_type` and `lstat` report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

/// The facts about one process-table row that decide `/proc`'s shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    /// The thread group this row belongs to; equal to its own pid for a leader.
    pub tgid: Pid,
    /// The box (container) it runs in; 0 is the host.
    pub box_id: u64,
    /// Exited but not yet reaped — a zombie, or a thread on its way out.
    pub exited: bool,
}

/// The build-time and hardware switches that add or remove whole entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// `/proc/power` — the kernel registered a battery/AC renderer.
    pub power: bool,
    /// `/proc/sysvipc` — the SysV IPC syscalls are compiled in.
    pub sysvipc: bool,
    /// `/proc/<pid>/syscalls` and the retained logs of exited pids.
    pub syscall_log: bool,
}

/// What the tree needs to know about the machine, and nothing else.
///
/// Every method is a question about **this moment**; the kernel's adapter
/// answers them from one snapshot of its process table so that a single
/// `list` or `resolve` cannot see a process appear half way through.
pub trait ProcModel {
    /// The box the caller is in; 0 sees every process.
    fn viewer_box(&self) -> u64;
    /// The caller's thread-group id — what `/proc/self` names. `None` for a
    /// kernel thread, which has no `self`.
    fn current_tgid(&self) -> Option<Pid>;
    fn features(&self) -> Features;
    /// Every row in the process table, threads included, each pid once, in
    /// the order the listing should present them.
    fn for_each_pid(&self, f: &mut dyn FnMut(Pid));
    /// The row for `pid`, if the table has one (zombies included).
    fn process(&self, pid: Pid) -> Option<ProcInfo>;
    /// The descriptors in `pid`'s table, ascending.
    fn for_each_fd(&self, pid: Pid, f: &mut dyn FnMut(u32));
    /// Whether `pid`'s table holds descriptor `fd`.
    fn has_fd(&self, pid: Pid, fd: u32) -> bool;
    /// Whether a syscall log the caller may read is retained for `pid` (live
    /// or exited). Only consulted when [`Features::syscall_log`] is set.
    fn has_syscall_log(&self, pid: Pid) -> bool;
    /// Every pid with a retained log the caller may read.
    fn for_each_logged_pid(&self, f: &mut dyn FnMut(Pid));
    /// Whether `maps`/`statm` can be rendered for `pid` (a kernel registered
    /// an address-space walk, and the process has one).
    fn has_address_space(&self, pid: Pid) -> bool;

    /// Whether the kernel retained an auxv table for `pid` (`/proc/<pid>/auxv`).
    ///
    /// Default `false` — a model that does not retain one keeps the file
    /// *absent rather than empty*, the `maps` rule (PHASE-2-PROCFS D28).
    fn has_pid_auxv(&self, _pid: Pid) -> bool {
        false
    }

    /// Calls `f` with each live member of thread group `tgid` (the leader
    /// counts), in [`for_each_pid`](Self::for_each_pid) order.
    ///
    /// The default asks [`process`](Self::process) once per pid, which is
    /// quadratic in a kernel whose lookup is itself a table scan; the kernel
    /// adapter overrides it with a single pass.
    fn for_each_live_member(&self, tgid: Pid, f: &mut dyn FnMut(Pid)) {
        self.for_each_pid(&mut |pid| {
            if self.process(pid).is_some_and(|p| p.tgid == tgid && !p.exited) {
                f(pid);
            }
        });
    }
}

/// The system-wide files at the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Global {
    Boxes,
    Cores,
    Dac,
    Mounts,
    Filesystems,
    Meminfo,
    Stat,
    Uptime,
    Loadavg,
    Cpuinfo,
    Power,
    /// Counters for self-heal and pressure-valve machinery that otherwise runs
    /// invisibly (`docs/archive/POST_AMD64_CODE_AUDIT.md` §1.3).
    Health,
}

impl Global {
    /// Listing order at the root.
    pub const ALL: [Self; 12] = [
        Self::Boxes,
        Self::Cores,
        Self::Dac,
        Self::Mounts,
        Self::Filesystems,
        Self::Meminfo,
        Self::Stat,
        Self::Uptime,
        Self::Loadavg,
        Self::Cpuinfo,
        Self::Power,
        Self::Health,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Boxes => "boxes",
            Self::Cores => "cores",
            Self::Dac => "dac",
            Self::Mounts => "mounts",
            Self::Filesystems => "filesystems",
            Self::Meminfo => "meminfo",
            Self::Stat => "stat",
            Self::Uptime => "uptime",
            Self::Loadavg => "loadavg",
            Self::Cpuinfo => "cpuinfo",
            Self::Power => "power",
            Self::Health => "health",
        }
    }

    fn exists<M: ProcModel + ?Sized>(self, m: &M) -> bool {
        match self {
            // Host-only: a box must not see other boxes or flip enforcement.
            Self::Boxes | Self::Cores | Self::Dac => m.viewer_box() == 0,
            Self::Power => m.features().power,
            _ => true,
        }
    }
}

/// `/proc/net/*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetFile {
    Dev,
    Tcp,
    Udp,
}

impl NetFile {
    pub const ALL: [Self; 3] = [Self::Dev, Self::Tcp, Self::Udp];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// `/proc/sys/kernel/*`: the sysctls a namespace-aware program reads before it
/// dares to run. bubblewrap reads `overflowuid`/`overflowgid` unconditionally at
/// start and refuses to go on without them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysKernelFile {
    OverflowUid,
    OverflowGid,
}

impl SysKernelFile {
    pub const ALL: [Self; 2] = [Self::OverflowUid, Self::OverflowGid];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OverflowUid => "overflowuid",
            Self::OverflowGid => "overflowgid",
        }
    }

    /// The file's bytes. Linux's default for both is the `nobody` id, 65534.
    #[must_use]
    pub const fn contents(self) -> &'static [u8] {
        b"65534\n"
    }
}

/// The namespaces `/proc/<pid>/ns/` names. Akuma has boxes, not Linux namespaces,
/// so these report the box the process is in: two processes show the same
/// `mnt`/`pid`/`net`/`ipc`/`uts` link exactly when they share a box.
/// bubblewrap opens the directory and `readlink`s entries to learn whether it
/// is already inside the namespaces it was asked to create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsFile {
    Cgroup,
    Ipc,
    Mnt,
    Net,
    Pid,
    User,
    Uts,
}

impl NsFile {
    pub const ALL: [Self; 7] =
        [Self::Cgroup, Self::Ipc, Self::Mnt, Self::Net, Self::Pid, Self::User, Self::Uts];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cgroup => "cgroup",
            Self::Ipc => "ipc",
            Self::Mnt => "mnt",
            Self::Net => "net",
            Self::Pid => "pid",
            Self::User => "user",
            Self::Uts => "uts",
        }
    }

    /// The inode number of the host's namespace of this kind (Linux's own
    /// initial ones), which a box-0 process reports.
    #[must_use]
    pub const fn host_inode(self) -> u64 {
        match self {
            Self::Pid => 4_026_531_836,
            Self::User => 4_026_531_837,
            Self::Uts => 4_026_531_838,
            Self::Ipc => 4_026_531_839,
            Self::Mnt => 4_026_531_840,
            Self::Net => 4_026_531_992,
            Self::Cgroup => 4_026_531_835,
        }
    }

    /// The inode a process in `box_id` reports: the host's for box 0 and for the
    /// kinds a box does not separate (`user`, `cgroup`), a per-box one otherwise.
    #[must_use]
    pub const fn inode_in_box(self, box_id: u64) -> u64 {
        match self {
            Self::User | Self::Cgroup => self.host_inode(),
            _ if box_id == 0 => self.host_inode(),
            _ => 4_026_600_000 + (box_id % 1_000_000) * 16 + self.host_inode() % 16,
        }
    }
}

/// The files inside a `<pid>/` (or `<tgid>/task/<tid>/`) directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidFile {
    Cmdline,
    Status,
    Stat,
    Mounts,
    /// `mountinfo`: the same mounts with ids, parents and options (`proc(5)`). bubblewrap parses it after a bind
    /// mount to learn the flags it must carry over.
    Mountinfo,
    /// A symlink to the running image.
    Exe,
    OomScoreAdj,
    OomAdj,
    OomScore,
    Maps,
    Statm,
    /// The raw auxv the process was exec'd with (`/proc/<pid>/auxv`), when
    /// the kernel retained one (`ProcModel::has_pid_auxv`) — absent rather
    /// than empty, like `maps` (PHASE-2-PROCFS).
    Auxv,
    Syscalls,
}

impl PidFile {
    /// Listing order inside a pid directory (after `fd` and `task`).
    pub const ALL: [Self; 13] = [
        Self::Cmdline,
        Self::Status,
        Self::Stat,
        Self::Mounts,
        Self::Mountinfo,
        Self::Exe,
        Self::OomScoreAdj,
        Self::OomAdj,
        Self::OomScore,
        Self::Maps,
        Self::Statm,
        Self::Auxv,
        Self::Syscalls,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cmdline => "cmdline",
            Self::Status => "status",
            Self::Stat => "stat",
            Self::Mounts => "mounts",
            Self::Mountinfo => "mountinfo",
            Self::Exe => "exe",
            Self::OomScoreAdj => "oom_score_adj",
            Self::OomAdj => "oom_adj",
            Self::OomScore => "oom_score",
            Self::Maps => "maps",
            Self::Statm => "statm",
            Self::Auxv => "auxv",
            Self::Syscalls => "syscalls",
        }
    }

    #[must_use]
    pub const fn kind(self) -> Kind {
        match self {
            Self::Exe => Kind::Symlink,
            _ => Kind::File,
        }
    }

    /// Whether this file exists in `view`, given that the directory does.
    fn exists<M: ProcModel + ?Sized>(self, m: &M, view: PidDir) -> bool {
        let visible = visible(m, view.pid);
        match self {
            Self::Syscalls => m.features().syscall_log && m.has_syscall_log(view.pid),
            Self::Maps | Self::Statm => visible && m.has_address_space(view.pid),
            Self::Auxv => visible && m.has_pid_auxv(view.pid),
            _ => visible,
        }
    }
}

/// A pid directory, and how it was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidDir {
    /// The process (or thread) whose files these are.
    pub pid: Pid,
    /// `Some(tgid)` when reached as `<tgid>/task/<pid>/` — the per-thread view,
    /// which serves the same files as `<pid>/` but has no `task` of its own.
    pub thread_of: Option<Pid>,
}

impl PidDir {
    const fn top(pid: Pid) -> Self {
        Self { pid, thread_of: None }
    }
}

/// Everything a `/proc` path can resolve to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    Root,
    /// `/proc/self`, a symlink to the caller's tgid.
    SelfLink,
    Global(Global),
    NetDir,
    Net(NetFile),
    SysvipcDir,
    SysvipcMsg,
    /// `/proc/sys`, `/proc/sys/kernel` and the files in it.
    SysDir,
    SysKernelDir,
    SysKernel(SysKernelFile),
    Pid(PidDir),
    PidFile(PidDir, PidFile),
    FdDir(PidDir),
    /// `<pid>/ns`, and `<pid>/ns/<name>` (a symlink reading `name:[inode]`).
    NsDir(PidDir),
    Ns(PidDir, NsFile),
    /// `<pid>/fd/<n>`, a symlink.
    Fd(PidDir, u32),
    /// `<tgid>/task`, listing the group's live thread ids.
    TaskDir(Pid),
}

impl Node {
    #[must_use]
    pub const fn kind(self) -> Kind {
        match self {
            Self::Root
            | Self::NetDir
            | Self::SysvipcDir
            | Self::SysDir
            | Self::SysKernelDir
            | Self::Pid(_)
            | Self::FdDir(_)
            | Self::NsDir(_)
            | Self::TaskDir(_) => Kind::Dir,
            Self::SelfLink | Self::Fd(..) | Self::Ns(..) => Kind::Symlink,
            Self::PidFile(_, f) => f.kind(),
            Self::Global(_) | Self::Net(_) | Self::SysvipcMsg | Self::SysKernel(_) => Kind::File,
        }
    }

    /// The process whose row owns this node (for uid/gid and box checks).
    #[must_use]
    pub const fn owner(self) -> Option<Pid> {
        match self {
            Self::Pid(v) | Self::PidFile(v, _) | Self::FdDir(v) | Self::Fd(v, _) | Self::NsDir(v) | Self::Ns(v, _) => Some(v.pid),
            Self::TaskDir(tgid) => Some(tgid),
            _ => None,
        }
    }

    /// The canonical spelling of this node, relative to the mount: `self/` and
    /// `<tgid>/task/<tid>/` are both folded to `<pid>/`, so a renderer that
    /// parses paths sees exactly one name per file.
    ///
    /// # Errors
    ///
    /// Only those of `out`.
    pub fn write_canonical(self, out: &mut dyn fmt::Write) -> fmt::Result {
        match self {
            Self::Root => Ok(()),
            Self::SelfLink => out.write_str("self"),
            Self::Global(g) => out.write_str(g.name()),
            Self::NetDir => out.write_str("net"),
            Self::Net(f) => write!(out, "net/{}", f.name()),
            Self::SysvipcDir => out.write_str("sysvipc"),
            Self::SysvipcMsg => out.write_str("sysvipc/msg"),
            Self::SysDir => out.write_str("sys"),
            Self::SysKernelDir => out.write_str("sys/kernel"),
            Self::SysKernel(f) => write!(out, "sys/kernel/{}", f.name()),
            Self::Pid(v) => write!(out, "{}", v.pid),
            Self::PidFile(v, f) => write!(out, "{}/{}", v.pid, f.name()),
            Self::FdDir(v) => write!(out, "{}/fd", v.pid),
            Self::NsDir(v) => write!(out, "{}/ns", v.pid),
            Self::Ns(v, f) => write!(out, "{}/ns/{}", v.pid, f.name()),
            Self::Fd(v, n) => write!(out, "{}/fd/{n}", v.pid),
            Self::TaskDir(tgid) => write!(out, "{tgid}/task"),
        }
    }
}

/// Why [`list`] produced nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    NotFound,
    NotADirectory,
}

/// Whether the caller may see process `pid` at all — box 0 sees every row, a
/// box only its own.
#[must_use]
pub fn visible<M: ProcModel + ?Sized>(m: &M, pid: Pid) -> bool {
    match m.process(pid) {
        Some(p) => m.viewer_box() == 0 || p.box_id == m.viewer_box(),
        None => false,
    }
}

/// Whether `tid` is a live member of thread group `tgid` (the leader counts).
#[must_use]
pub fn is_live_member<M: ProcModel + ?Sized>(m: &M, tgid: Pid, tid: Pid) -> bool {
    m.process(tid).is_some_and(|p| p.tgid == tgid && !p.exited)
}

/// [`ProcModel::for_each_live_member`], returning how many there were.
pub fn for_each_live_member<M: ProcModel + ?Sized>(
    m: &M,
    tgid: Pid,
    f: &mut dyn FnMut(Pid),
) -> usize {
    let mut n = 0;
    m.for_each_live_member(tgid, &mut |pid| {
        n += 1;
        f(pid);
    });
    n
}

/// How many live threads `tgid` has — the `2 + n` of `<tgid>/task`'s
/// `st_nlink`, which Chromium reads "single-threaded" off.
#[must_use]
pub fn live_member_count<M: ProcModel + ?Sized>(m: &M, tgid: Pid) -> usize {
    for_each_live_member(m, tgid, &mut |_| {})
}

/// A pid directory exists while its process is visible, or while an exited
/// pid's syscall log is retained (then it holds only `syscalls`).
fn pid_dir_exists<M: ProcModel + ?Sized>(m: &M, pid: Pid) -> bool {
    visible(m, pid) || (m.features().syscall_log && m.has_syscall_log(pid))
}

/// `<pid>/task` exists for a visible pid that leads a group with at least one
/// live member, and only at the top level — never inside a thread view.
fn task_dir_exists<M: ProcModel + ?Sized>(m: &M, view: PidDir) -> bool {
    view.thread_of.is_none() && visible(m, view.pid) && live_member_count(m, view.pid) > 0
}

/// `<pid>/fd/<n>`: 0 and 1 are the process's stdin/stdout buffers and always
/// present; any other number exists exactly when the table holds it.
fn fd_exists<M: ProcModel + ?Sized>(m: &M, pid: Pid, fd: u32) -> bool {
    fd <= 1 || m.has_fd(pid, fd)
}

/// A pid or fd number as `/proc` spells it: plain decimal, no sign, and no
/// leading zero. `u32::from_str` would also accept `+7` and `007`, which made
/// every pid directory reachable under several names — a walker that dedups by
/// path would visit each of them.
fn parse_num(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 10 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if b.len() > 1 && b[0] == b'0' {
        return None;
    }
    s.parse().ok()
}

fn split_first(path: &str) -> (&str, Option<&str>) {
    match path.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (path, None),
    }
}

/// Resolve a path relative to the `/proc` mount (leading and trailing `/` are
/// ignored) to the node it names, or `None` if nothing is there.
///
/// The path must already be normalised (no `.`/`..`, no empty components);
/// the VFS does that before a filesystem sees it. A component that is not
/// normalised resolves to nothing rather than to a guess.
#[must_use]
pub fn resolve<M: ProcModel + ?Sized>(m: &M, path: &str) -> Option<Node> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Some(Node::Root);
    }
    let (first, rest) = split_first(path);
    match first {
        "self" => {
            let tgid = m.current_tgid()?;
            match rest {
                None => Some(Node::SelfLink),
                Some(rest) => resolve_in_pid(m, PidDir::top(tgid), Some(rest)),
            }
        }
        "net" => match rest {
            None => Some(Node::NetDir),
            Some(name) => NetFile::ALL.into_iter().find(|f| f.name() == name).map(Node::Net),
        },
        "sys" => match rest {
            None => Some(Node::SysDir),
            Some("kernel") => Some(Node::SysKernelDir),
            Some(r) => r
                .strip_prefix("kernel/")
                .and_then(|n| SysKernelFile::ALL.into_iter().find(|f| f.name() == n))
                .map(Node::SysKernel),
        },
        "sysvipc" if m.features().sysvipc => match rest {
            None => Some(Node::SysvipcDir),
            Some("msg") => Some(Node::SysvipcMsg),
            Some(_) => None,
        },
        _ => {
            if let Some(g) = Global::ALL.into_iter().find(|g| g.name() == first) {
                return (rest.is_none() && g.exists(m)).then_some(Node::Global(g));
            }
            resolve_in_pid(m, PidDir::top(parse_num(first)?), rest)
        }
    }
}

fn resolve_in_pid<M: ProcModel + ?Sized>(m: &M, view: PidDir, rest: Option<&str>) -> Option<Node> {
    if !pid_dir_exists(m, view.pid) {
        return None;
    }
    let Some(rest) = rest else {
        return Some(Node::Pid(view));
    };
    let (name, rest) = split_first(rest);
    match name {
        "fd" => {
            if !visible(m, view.pid) {
                return None;
            }
            match rest {
                None => Some(Node::FdDir(view)),
                Some(n) => {
                    let n = parse_num(n)?;
                    fd_exists(m, view.pid, n).then_some(Node::Fd(view, n))
                }
            }
        }
        "ns" => {
            if !visible(m, view.pid) {
                return None;
            }
            match rest {
                None => Some(Node::NsDir(view)),
                Some(n) => NsFile::ALL.into_iter().find(|f| f.name() == n).map(|f| Node::Ns(view, f)),
            }
        }
        "task" => {
            if !task_dir_exists(m, view) {
                return None;
            }
            let Some(rest) = rest else {
                return Some(Node::TaskDir(view.pid));
            };
            let (tid, rest) = split_first(rest);
            let tid = parse_num(tid)?;
            if !is_live_member(m, view.pid, tid) {
                return None;
            }
            resolve_in_pid(m, PidDir { pid: tid, thread_of: Some(view.pid) }, rest)
        }
        _ => {
            if rest.is_some() {
                return None;
            }
            let f = PidFile::ALL.into_iter().find(|f| f.name() == name)?;
            f.exists(m, view).then_some(Node::PidFile(view, f))
        }
    }
}

/// Decimal digits of `n` in a stack buffer.
struct Num {
    buf: [u8; 10],
    start: usize,
}

impl Num {
    fn new(mut n: u32) -> Self {
        let mut buf = [0u8; 10];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        Self { buf, start: i }
    }

    fn as_str(&self) -> &str {
        // Only ASCII digits are ever written.
        core::str::from_utf8(&self.buf[self.start..]).unwrap_or("")
    }
}

/// List the children of `node`, calling `emit(name, kind)` once per name.
///
/// Every name emitted resolves, through [`resolve`], to a node of the kind it
/// is emitted with — that is the invariant the host simulation walks the whole
/// tree to check. `.` and `..` are not emitted (see the module header).
///
/// # Errors
///
/// [`ListError::NotADirectory`] when `node` is not a directory.
pub fn list_node<M: ProcModel + ?Sized>(
    m: &M,
    node: Node,
    emit: &mut dyn FnMut(&str, Kind),
) -> Result<(), ListError> {
    match node {
        Node::Root => {
            m.for_each_pid(&mut |pid| {
                if visible(m, pid) {
                    emit(Num::new(pid).as_str(), Kind::Dir);
                }
            });
            for g in Global::ALL {
                if g.exists(m) {
                    emit(g.name(), Kind::File);
                }
                // `net` sits between the host-only files and the rest, as it
                // always has; the order is cosmetic but `ls` shows it.
                if g == Global::Dac {
                    emit("net", Kind::Dir);
                }
            }
            // Exited pids whose syscall log is retained, unless already listed
            // as a live process — a pid must not appear twice.
            if m.features().syscall_log {
                m.for_each_logged_pid(&mut |pid| {
                    if !visible(m, pid) {
                        emit(Num::new(pid).as_str(), Kind::Dir);
                    }
                });
            }
            if m.features().sysvipc {
                emit("sysvipc", Kind::Dir);
            }
            emit("sys", Kind::Dir);
            if m.current_tgid().is_some() {
                emit("self", Kind::Symlink);
            }
            Ok(())
        }
        Node::NetDir => {
            for f in NetFile::ALL {
                emit(f.name(), Kind::File);
            }
            Ok(())
        }
        Node::SysvipcDir => {
            emit("msg", Kind::File);
            Ok(())
        }
        Node::SysDir => {
            emit("kernel", Kind::Dir);
            Ok(())
        }
        Node::SysKernelDir => {
            for f in SysKernelFile::ALL {
                emit(f.name(), Kind::File);
            }
            Ok(())
        }
        Node::TaskDir(tgid) => {
            for_each_live_member(m, tgid, &mut |tid| emit(Num::new(tid).as_str(), Kind::Dir));
            Ok(())
        }
        Node::FdDir(view) => {
            emit("0", Kind::Symlink);
            emit("1", Kind::Symlink);
            m.for_each_fd(view.pid, &mut |fd| {
                if fd > 1 {
                    emit(Num::new(fd).as_str(), Kind::Symlink);
                }
            });
            Ok(())
        }
        Node::NsDir(_) => {
            for f in NsFile::ALL {
                emit(f.name(), Kind::Symlink);
            }
            Ok(())
        }
        Node::Pid(view) => {
            if visible(m, view.pid) {
                emit("fd", Kind::Dir);
                emit("ns", Kind::Dir);
                if task_dir_exists(m, view) {
                    emit("task", Kind::Dir);
                }
            }
            for f in PidFile::ALL {
                if f.exists(m, view) {
                    emit(f.name(), f.kind());
                }
            }
            Ok(())
        }
        Node::SelfLink
        | Node::Global(_)
        | Node::Net(_)
        | Node::SysvipcMsg
        | Node::SysKernel(_)
        | Node::PidFile(..)
        | Node::Ns(..)
        | Node::Fd(..) => Err(ListError::NotADirectory),
    }
}

/// [`list_node`] for a path.
///
/// # Errors
///
/// [`ListError::NotFound`] when the path resolves to nothing,
/// [`ListError::NotADirectory`] when it resolves to a file or symlink. A path
/// *through* `self` lists the directory `self` points at; the bare `self` is
/// a symlink, so listing it is `NotADirectory` — the VFS follows the link
/// before it asks.
pub fn list<M: ProcModel + ?Sized>(
    m: &M,
    path: &str,
    emit: &mut dyn FnMut(&str, Kind),
) -> Result<(), ListError> {
    let node = resolve(m, path).ok_or(ListError::NotFound)?;
    list_node(m, node, emit)
}
