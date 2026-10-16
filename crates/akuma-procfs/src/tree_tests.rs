//! The `/proc` **tree simulation**: build a fake process table, walk the whole
//! tree the way `find`, ripgrep and Bun's readdir walkers do, and fail on any
//! structural bug.
//!
//! The walk is breadth-first with a depth cap. The deepest real directory is
//! four below the mount (`<tgid>/task/<tid>/fd`), so going deeper — let alone
//! reaching the cap — means some directory lists a path back into itself. Every listing is read through the same
//! `getdents64` stream shape the kernel arm produces
//! (`akuma_syscalls_linux::dirent::{DOT_ENTRIES, fill, records}`) with a small
//! buffer, so a name that only survives a large buffer, or a dot record that
//! appears twice, is caught too.
//!
//! What this cannot see: the renderers' bytes, the VFS path normaliser, the
//! syscall layer's `stat` (it asserts `lstat`-kind agreement through
//! [`resolve`]), and anything that depends on timing — a process that exits
//! *between* a listing and the `stat` of one of its names is a real race on
//! both Linux and Akuma, and is out of a single-snapshot model's reach.

extern crate std;

use std::collections::{BTreeSet, VecDeque};
use std::format;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use super::tree::*;
use akuma_syscalls_linux::dirent;

// ── the fake machine ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Row {
    pid: Pid,
    tgid: Pid,
    box_id: u64,
    exited: bool,
    /// Descriptors in the table (0/1/2 only when actually open).
    fds: Vec<u32>,
    address_space: bool,
}

impl Row {
    fn leader(pid: Pid) -> Self {
        Self { pid, tgid: pid, box_id: 0, exited: false, fds: vec![0, 1, 2], address_space: true }
    }
    fn thread(tid: Pid, tgid: Pid) -> Self {
        Self { tgid, ..Self::leader(tid) }
    }
}

#[derive(Clone, Debug)]
struct Fake {
    rows: Vec<Row>,
    viewer_box: u64,
    me: Option<Pid>,
    features: Features,
    /// Pids with a retained syscall log the viewer may read.
    logs: Vec<Pid>,
}

impl Fake {
    fn new(rows: Vec<Row>) -> Self {
        let me = rows.first().map(|r| r.tgid);
        Self {
            rows,
            viewer_box: 0,
            me,
            features: Features { power: true, sysvipc: true, syscall_log: true },
            logs: Vec::new(),
        }
    }
    fn row(&self, pid: Pid) -> Option<&Row> {
        self.rows.iter().find(|r| r.pid == pid)
    }
}

impl ProcModel for Fake {
    fn viewer_box(&self) -> u64 {
        self.viewer_box
    }
    fn current_tgid(&self) -> Option<Pid> {
        self.me
    }
    fn features(&self) -> Features {
        self.features
    }
    fn for_each_pid(&self, f: &mut dyn FnMut(Pid)) {
        let mut pids: Vec<Pid> = self.rows.iter().map(|r| r.pid).collect();
        pids.sort_unstable();
        for p in pids {
            f(p);
        }
    }
    fn process(&self, pid: Pid) -> Option<ProcInfo> {
        self.row(pid).map(|r| ProcInfo { tgid: r.tgid, box_id: r.box_id, exited: r.exited })
    }
    fn for_each_fd(&self, pid: Pid, f: &mut dyn FnMut(u32)) {
        if let Some(r) = self.row(pid) {
            let mut fds = r.fds.clone();
            fds.sort_unstable();
            for fd in fds {
                f(fd);
            }
        }
    }
    fn has_fd(&self, pid: Pid, fd: u32) -> bool {
        self.row(pid).is_some_and(|r| r.fds.contains(&fd))
    }
    fn has_syscall_log(&self, pid: Pid) -> bool {
        self.logs.contains(&pid)
    }
    fn for_each_logged_pid(&self, f: &mut dyn FnMut(Pid)) {
        for &p in &self.logs {
            f(p);
        }
    }
    fn has_address_space(&self, pid: Pid) -> bool {
        self.row(pid).is_some_and(|r| r.address_space)
    }
}

/// The machine the named tests use:
/// - 1: init, single-threaded, host
/// - 10: a three-thread group (10 leads 11 and 12), host — the caller
/// - 20: a zombie (exited, still in the table), host
/// - 30: a group whose leader exited while thread 31 lives on
/// - 40: a process in box 7, with stderr closed
/// - 50: an exited pid with only a retained syscall log
fn machine() -> Fake {
    let mut rows = vec![
        Row::leader(10),
        Row::thread(11, 10),
        Row::thread(12, 10),
        Row::leader(1),
        Row { exited: true, ..Row::leader(20) },
        Row { exited: true, ..Row::leader(30) },
        Row::thread(31, 30),
        Row { box_id: 7, fds: vec![0, 1, 5], ..Row::leader(40) },
    ];
    rows[0].fds.push(3);
    let mut m = Fake::new(rows);
    m.me = Some(10);
    m.logs = vec![50];
    m
}

// ── the walk ─────────────────────────────────────────────────────────────

const DEPTH_CAP: usize = 12;
/// The deepest directory is `<tgid>/task/<tid>/fd`, four below the mount (its
/// `<n>` entries are symlinks, which the walk does not enter).
const REAL_MAX_DEPTH: usize = 4;

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn listing(m: &Fake, path: &str) -> Vec<(String, Kind)> {
    let mut out = Vec::new();
    list(m, path, &mut |n, k| out.push((n.to_string(), k))).unwrap_or_else(|e| {
        panic!("list({path:?}) failed with {e:?} although the walk reached it as a directory")
    });
    out
}

fn d_type(k: Kind) -> u8 {
    match k {
        Kind::Dir => dirent::DT_DIR,
        Kind::File => dirent::DT_REG,
        Kind::Symlink => dirent::DT_LNK,
    }
}

/// What `readdir(3)` sees: the `getdents64` arm's stream — the dot records,
/// then the listing — read with a buffer that holds one or two records a call.
fn getdents_stream(entries: &[(String, Kind)]) -> Vec<(u8, String)> {
    let stream: Vec<(&str, u8)> = dirent::DOT_ENTRIES
        .iter()
        .copied()
        .chain(entries.iter().map(|(n, k)| (n.as_str(), d_type(*k))))
        .collect();
    let mut buf = [0u8; 48];
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        let (bytes, n) = dirent::fill(stream.iter().copied().skip(pos), 1, &mut buf);
        if n == 0 {
            assert_eq!(pos, stream.len(), "a record did not fit a 48-byte buffer");
            return out;
        }
        for (t, name) in dirent::records(&buf[..bytes]) {
            out.push((t, String::from_utf8(name.to_vec()).unwrap()));
        }
        pos += n;
    }
}

#[derive(Default, Debug)]
struct WalkReport {
    dirs: usize,
    entries: usize,
    max_depth: usize,
    paths: BTreeSet<String>,
}

/// Walk the whole tree breadth-first without following symlinks (ripgrep's
/// and `find`'s default), asserting every structural invariant as it goes.
fn walk(m: &Fake) -> WalkReport {
    let mut report = WalkReport::default();
    let mut queue: VecDeque<(String, usize)> = VecDeque::from([(String::new(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        assert!(depth < DEPTH_CAP, "walk reached depth {depth} at {dir:?}: an unbounded structure");
        report.dirs += 1;
        report.max_depth = report.max_depth.max(depth);

        let first = listing(m, &dir);
        let second = listing(m, &dir);
        assert_eq!(first, second, "{dir:?} listed differently on a second read");

        let mut seen = BTreeSet::new();
        for (name, kind) in &first {
            assert!(name != "." && name != "..", "{dir:?}: the filesystem listed {name:?} itself");
            assert!(!name.is_empty() && !name.contains('/'), "{dir:?}: bad name {name:?}");
            assert!(seen.insert(name.clone()), "{dir:?} lists {name:?} twice");

            let path = join(&dir, name);
            let node = resolve(m, &path)
                .unwrap_or_else(|| panic!("{dir:?} lists {name:?} but {path:?} does not resolve"));
            assert_eq!(node.kind(), *kind, "{path:?} is listed as {kind:?} but resolves to {node:?}");
            assert!(report.paths.insert(path.clone()), "{path:?} reached twice");
            report.entries += 1;
            if *kind == Kind::Dir {
                queue.push_back((path, depth + 1));
            }
        }

        // The getdents64 layer: dots exactly once, first, and the same names.
        let stream = getdents_stream(&first);
        assert_eq!(stream[0], (dirent::DT_DIR, ".".to_string()), "{dir:?}");
        assert_eq!(stream[1], (dirent::DT_DIR, "..".to_string()), "{dir:?}");
        assert_eq!(stream.iter().filter(|(_, n)| n == ".").count(), 1, "{dir:?}");
        assert_eq!(stream.iter().filter(|(_, n)| n == "..").count(), 1, "{dir:?}");
        let names: Vec<&str> = stream[2..].iter().map(|(_, n)| n.as_str()).collect();
        let want: Vec<&str> = first.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, want, "{dir:?}: getdents stream lost or reordered names");
    }
    report
}

/// The same walk, but following `/proc/self` the way `rg --follow` and
/// `find -L` do, deduplicating directories by the canonical node they resolve
/// to. Must terminate, and following `self` must reach only directories the
/// plain walk already reached under their own names.
fn walk_following_self(m: &Fake, plain: &WalkReport) -> usize {
    let mut seen_nodes: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::from([(String::new(), 0)]);
    let mut visited = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        assert!(depth < DEPTH_CAP, "following walk reached depth {depth} at {dir:?}");
        let node = resolve(m, &dir).unwrap();
        let mut canon = String::new();
        node.write_canonical(&mut canon).unwrap();
        if !seen_nodes.insert(format!("{canon}|{node:?}")) {
            continue;
        }
        visited += 1;
        for (name, kind) in listing(m, &dir) {
            let path = join(&dir, &name);
            match kind {
                Kind::Dir => queue.push_back((path, depth + 1)),
                Kind::Symlink if path == "self" => {
                    let target = format!("{}", m.current_tgid().unwrap());
                    assert!(plain.paths.contains(&target), "self -> {target}, which the walk never saw");
                    // Follow the link the way the VFS does: by its target.
                    // The dedup above must then stop the walk at once.
                    queue.push_back((target, depth + 1));
                }
                _ => {}
            }
        }
    }
    visited
}

fn assert_tree_sound(m: &Fake) -> WalkReport {
    let r = walk(m);
    assert!(r.max_depth <= REAL_MAX_DEPTH, "max depth {} > {REAL_MAX_DEPTH}", r.max_depth);
    if m.current_tgid().is_some() {
        walk_following_self(m, &r);
    }
    r
}

// ── the whole-tree tests ─────────────────────────────────────────────────

#[test]
fn the_machine_walks_clean_from_the_host() {
    let r = assert_tree_sound(&machine());
    // root, 7 visible pids + 50's log dir, their fd/task dirs, thread dirs...
    // a floor rather than an exact count, so adding a file is not a test edit.
    assert!(r.dirs > 20, "walk visited only {} directories", r.dirs);
    assert_eq!(r.max_depth, REAL_MAX_DEPTH, "the deepest path should be <tgid>/task/<tid>/fd");
}

#[test]
fn the_machine_walks_clean_from_inside_a_box() {
    let mut m = machine();
    m.viewer_box = 7;
    m.me = Some(40);
    let r = assert_tree_sound(&m);
    for p in &r.paths {
        let first = p.split('/').next().unwrap();
        if let Ok(pid) = first.parse::<Pid>() {
            assert!(pid == 40 || pid == 50, "box 7 reached host pid {pid} via {p:?}");
        }
        assert!(!["boxes", "cores", "dac"].contains(&first), "box 7 sees host-only {first:?}");
    }
}

#[test]
fn a_kernel_thread_has_no_self() {
    let mut m = machine();
    m.me = None;
    assert_tree_sound(&m);
    assert!(!listing(&m, "").iter().any(|(n, _)| n == "self"));
    assert_eq!(resolve(&m, "self"), None);
    assert_eq!(resolve(&m, "self/status"), None);
}

#[test]
fn features_off_remove_their_entries_consistently() {
    let mut m = machine();
    m.features = Features::default();
    let r = assert_tree_sound(&m);
    for gone in ["power", "sysvipc", "50", "10/syscalls"] {
        assert!(!r.paths.contains(gone), "{gone} is still reachable with its feature off");
        assert_eq!(resolve(&m, gone), None, "{gone} still resolves with its feature off");
    }
}

/// Many random tables — thread groups, zombies, boxes, closed descriptors,
/// retained logs — each walked from every viewer. A tiny LCG keeps this
/// deterministic and dependency-free.
#[test]
fn random_process_tables_walk_clean() {
    let mut seed: u64 = 0x5eed_1234;
    let mut next = move |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for _ in 0..300 {
        let mut rows: Vec<Row> = Vec::new();
        let mut pid = 1;
        for _ in 0..(1 + next(6)) {
            let leader = pid;
            let box_id = next(3);
            let threads = next(4);
            for t in 0..=threads {
                let mut fds: Vec<u32> = (0..3).filter(|_| next(4) != 0).collect();
                for _ in 0..next(4) {
                    let fd = 3 + next(20) as u32;
                    if !fds.contains(&fd) {
                        fds.push(fd);
                    }
                }
                rows.push(Row {
                    pid,
                    tgid: leader,
                    box_id,
                    exited: next(5) == 0,
                    fds,
                    address_space: next(3) != 0 || t > 0,
                });
                pid += 1;
            }
            pid += next(3) as u32;
        }
        let mut m = Fake::new(rows.clone());
        m.features = Features { power: next(2) == 0, sysvipc: next(2) == 0, syscall_log: next(2) == 0 };
        // Logs for some live pids and some pids beyond the table.
        m.logs = (1..pid + 3).filter(|_| next(6) == 0).collect();
        for viewer in 0..3 {
            m.viewer_box = viewer;
            let candidates: Vec<Pid> =
                rows.iter().filter(|r| viewer == 0 || r.box_id == viewer).map(|r| r.tgid).collect();
            m.me = if candidates.is_empty() { None } else { Some(candidates[next(candidates.len() as u64) as usize]) };
            assert_tree_sound(&m);
        }
    }
}

// ── the named regressions ────────────────────────────────────────────────

/// Observed on the ryzen (kernel 4211281b): `/proc/<pid>/task/<tid>/` listed
/// `task`. Linux has no `task` inside a thread directory. Because the thread
/// directory was an alias of `<tid>/`, `.../task/<tid>/task` then *listed* the
/// group again, and `.../task/<tid>/task/<tid>` did not resolve.
#[test]
fn regression_thread_dir_lists_no_task() {
    let m = machine();
    for tid in [10, 11, 12] {
        let dir = format!("10/task/{tid}");
        let names: Vec<String> = listing(&m, &dir).into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"task".to_string()), "{dir} lists task: {names:?}");
        assert!(names.contains(&"fd".to_string()) && names.contains(&"status".to_string()));
        assert_eq!(resolve(&m, &format!("{dir}/task")), None);
        assert_eq!(resolve(&m, &format!("{dir}/task/{tid}")), None);
    }
    // The top-level pid directory of a leader keeps its `task`.
    assert!(listing(&m, "10").iter().any(|(n, k)| n == "task" && *k == Kind::Dir));
}

/// Observed on the ryzen: `getdents64` returned no `.` and `..` in any
/// directory, so `ls -la` of an empty one printed only `total 0`. The tree
/// must not emit them (the arm does, for every filesystem), and the stream
/// the arm produces must carry each exactly once — `walk` checks that for
/// every directory; this pins it for the ones the report named.
#[test]
fn regression_dot_and_dotdot_once_in_every_directory() {
    let m = machine();
    for dir in ["", "10", "10/task", "10/fd", "10/task/11", "net"] {
        let entries = listing(&m, dir);
        assert!(entries.iter().all(|(n, _)| n != "." && n != ".."), "{dir:?} lists a dot itself");
        let stream = getdents_stream(&entries);
        assert_eq!(&stream[0].1, ".");
        assert_eq!(&stream[1].1, "..");
        assert_eq!(stream.len(), entries.len() + 2);
    }
}

#[test]
fn task_lists_exactly_the_live_threads_of_the_group() {
    let m = machine();
    let tids = |dir: &str| -> Vec<String> { listing(&m, dir).into_iter().map(|(n, _)| n).collect() };
    assert_eq!(tids("10/task"), ["10", "11", "12"]);
    assert_eq!(tids("1/task"), ["1"]);
    // Leader 30 exited, thread 31 lives: the group's task lists only 31.
    assert_eq!(tids("30/task"), ["31"]);
    assert_eq!(resolve(&m, "30/task/30"), None);
    assert!(matches!(resolve(&m, "30/task/31/status"), Some(Node::PidFile(..))));
    // A non-leader thread has no task of its own, and lists none.
    assert_eq!(resolve(&m, "11/task"), None);
    assert!(!tids("11").contains(&"task".to_string()));
    // A thread of another group is not reachable through this one.
    assert_eq!(resolve(&m, "10/task/31"), None);
    assert_eq!(live_member_count(&m, 10), 3);
}

/// A zombie (exited, not yet reaped) is a stable stub: its directory and files
/// stay, and its `task` is absent from both the listing and `resolve` — it
/// never appears in one and not the other.
#[test]
fn a_zombie_is_a_consistent_stub() {
    let m = machine();
    assert!(listing(&m, "").iter().any(|(n, _)| n == "20"));
    assert!(matches!(resolve(&m, "20"), Some(Node::Pid(_))));
    assert!(!listing(&m, "20").iter().any(|(n, _)| n == "task"));
    assert_eq!(resolve(&m, "20/task"), None);
    let fds: Vec<String> = listing(&m, "20/fd").into_iter().map(|(n, _)| n).collect();
    for fd in &fds {
        assert!(resolve(&m, &format!("20/fd/{fd}")).is_some());
    }
    // And once reaped, it is gone from everywhere at once.
    let mut reaped = m.clone();
    reaped.rows.retain(|r| r.pid != 20);
    assert!(!listing(&reaped, "").iter().any(|(n, _)| n == "20"));
    for p in ["20", "20/fd", "20/fd/0", "20/status", "20/task"] {
        assert_eq!(resolve(&reaped, p), None, "{p} survived the reap");
    }
}

/// `<pid>/fd` listed `2` unconditionally while `exists` asked the table, so a
/// process with stderr closed listed an entry `stat` refused.
#[test]
fn regression_a_closed_fd_2_is_not_listed() {
    let mut m = machine();
    m.viewer_box = 7;
    let fds: Vec<String> = listing(&m, "40/fd").into_iter().map(|(n, _)| n).collect();
    assert_eq!(fds, ["0", "1", "5"]);
    assert_eq!(resolve(&m, "40/fd/2"), None);
}

/// Thread views serve the symlinks too: `is_symlink`/`read_symlink` never
/// learned the `task` alias, so `<tgid>/task/<tid>/exe` was listed as a
/// symlink and denied as one.
#[test]
fn regression_thread_view_symlinks_resolve_as_symlinks() {
    let m = machine();
    let exe = resolve(&m, "10/task/11/exe").unwrap();
    assert_eq!(exe.kind(), Kind::Symlink);
    let fd = resolve(&m, "10/task/10/fd/3").unwrap();
    assert_eq!(fd.kind(), Kind::Symlink);
    let mut c = String::new();
    fd.write_canonical(&mut c).unwrap();
    assert_eq!(c, "10/fd/3");
}

#[test]
fn self_resolves_to_the_callers_group_and_does_not_nest() {
    let mut m = machine();
    m.me = Some(10);
    assert_eq!(resolve(&m, "self"), Some(Node::SelfLink));
    assert_eq!(resolve(&m, "self").unwrap().kind(), Kind::Symlink);
    assert_eq!(
        resolve(&m, "self/status"),
        Some(Node::PidFile(PidDir { pid: 10, thread_of: None }, PidFile::Status))
    );
    let mut c = String::new();
    resolve(&m, "self/task/12/stat").unwrap().write_canonical(&mut c).unwrap();
    assert_eq!(c, "12/stat");
    // `self` is listed once, at the root only, and never inside what it names.
    assert_eq!(listing(&m, "").iter().filter(|(n, _)| n == "self").count(), 1);
    // The bare `self` is the link; listing it is the VFS's job after it
    // follows the link, so the tree refuses rather than guessing.
    assert_eq!(list(&m, "self", &mut |_, _| {}), Err(ListError::NotADirectory));
    assert!(!listing(&m, "self/task").iter().any(|(n, _)| n == "self"));
    assert_eq!(listing(&m, "self/task"), listing(&m, "10/task"));
    assert_eq!(resolve(&m, "self/self"), None);
    assert_eq!(resolve(&m, "10/self"), None);
}

/// One name per node: `u32::from_str` accepts `+7` and `007`, which made each
/// pid directory reachable under several spellings.
#[test]
fn non_canonical_numbers_do_not_resolve() {
    let m = machine();
    for p in ["010", "+10", "10/fd/03", "10/task/011", "10/fd/+1", "4294967296", "", "10//fd"] {
        if p.is_empty() {
            continue;
        }
        assert_eq!(resolve(&m, p), None, "{p:?} resolved");
    }
    assert!(resolve(&m, "0").is_none(), "no pid 0 in the table");
}

#[test]
fn an_exited_pid_with_a_log_holds_only_syscalls() {
    let m = machine();
    assert_eq!(listing(&m, "").iter().filter(|(n, _)| n == "50").count(), 1);
    assert_eq!(listing(&m, "50"), [("syscalls".to_string(), Kind::File)]);
    assert_eq!(resolve(&m, "50/fd"), None);
    assert_eq!(resolve(&m, "50/status"), None);
    // A live pid with a log is listed once, not again as a log dir.
    let mut both = m.clone();
    both.logs.push(10);
    assert_eq!(listing(&both, "").iter().filter(|(n, _)| n == "10").count(), 1);
}

#[test]
fn listing_a_file_is_not_a_directory_and_a_missing_path_is_not_found() {
    let m = machine();
    let mut sink = |_: &str, _: Kind| {};
    assert_eq!(list(&m, "10/status", &mut sink), Err(ListError::NotADirectory));
    assert_eq!(list(&m, "10/fd/0", &mut sink), Err(ListError::NotADirectory));
    assert_eq!(list(&m, "999", &mut sink), Err(ListError::NotFound));
    assert_eq!(list(&m, "10/nope", &mut sink), Err(ListError::NotFound));
}
