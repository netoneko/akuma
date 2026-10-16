# `/proc` tree unification and the walker simulation (2026-10-16)

**Status:** code complete, host-verified. **Not yet verified on a booted kernel**
(see § 7). Worktree `fix-stale-tid-kill`; the change applies cleanly onto
`more-amd64-fixes` (4211281b).

## 1. What the request assumed, and what was true

The request said `/proc` "exists twice in spirit: one for AArch64 and one for
amd64". **It doesn't.** Since 5b slice 3, amd64 registers every process with
`akuma-exec` and mounts `akuma_vfs_glue::proc::ProcFilesystem` at `/proc`
(`amd64/src/fs.rs::init_vfs`). Its own synthetic `/proc` in `fd.rs` was deleted
in 4b batch 2c. Two comments said otherwise, and both are now corrected:
`crates/akuma-vfs-glue/Cargo.toml` ("still not mounted there") and the
`amd64/src/fs.rs` module doc ("What deliberately did not come with it is
`/proc`").

| Entry point | AArch64 | amd64 | Shared? |
|---|---|---|---|
| `/proc` listing / lookup / `stat` / `readlink` | `ProcFilesystem` (vfs-glue) | the same mounted `ProcFilesystem` | yes |
| Byte formats (`stat`, `status`, `cmdline`, `statm`, `maps` line) | `akuma-procfs` | `akuma-procfs` | yes |
| `maps`/`statm` rows (page-table walk) | `pid_map_rows` hook | `fd::pid_map_rows` hook | renderer shared, walk per-arch |
| `getdents64` | `akuma-syscalls-glue::fs::sys_getdents64` | forwards to the same function | yes |
| amd64 `sys_openat` fast paths | — | none left for `/proc` | — |
| `proc_consistency_check` | — | amd64 boot self-test (open/stat/access agree) | per-kernel test |

The duplication was **inside** `ProcFilesystem`. `read_dir`, `exists`,
`metadata`, `is_symlink` and `read_symlink` each parsed the path themselves,
alongside `resolve_self`/`resolve_task` string rewriting that only some of them
applied. Their disagreements are the bugs below. Each one has the same shape
from a walker's point of view: **a directory lists a name that doesn't resolve,
or that resolves to a different kind than the listing said.**

## 2. Bugs found and fixed

All were reproduced on the ryzen (kernel 4211281b) before the fix. The probe
output is in § 7.

1. **No `.`/`..` in any directory** (`getdents64`, both kernels). An empty
   directory streamed zero records. `ls -la` printed only `total 0`, and an empty
   directory took **one** `getdents64` call where Linux takes two. Fixed in the
   arm: `akuma_syscalls_linux::dirent::DOT_ENTRIES` goes in front of every
   filesystem's listing, and any `.`/`..` a filesystem lists itself are dropped.
2. **A thread directory listed `task`.** `<tgid>/task/<tid>` was an alias of
   `<tid>/`, and `<tid>/`'s listing adds `task` for a group leader. Then
   `.../task/<tid>/task` *listed* the group, and `.../task/<tid>/task/<tid>`
   didn't resolve. On the box, `find /proc -maxdepth 6` printed
   `find: /proc/N/task/N/task/N: No such file or directory` for **every** pid.
3. **`<pid>/fd` always listed `2`**, but `exists` asked the descriptor table. A
   process that had closed stderr listed an entry `stat` refused.
4. **Thread-view symlinks.** `is_symlink`/`read_symlink` never learned the `task`
   alias, so `<tgid>/task/<tid>/exe` and `.../fd/<n>` were listed as symlinks
   and denied as symlinks.
5. **`is_symlink` answered yes for nonexistent names** (`123/fd/5` with no pid
   123). `lstat` of an absent fd reported a symlink rather than `ENOENT`.
6. **Non-canonical numbers resolved.** `u32::from_str` accepts `+7` and `007`,
   so each pid directory was reachable under several names.
7. **`<tgid>/task/<tgid>` had the inode of its own ancestor `<tgid>`.** The
   inode was a hash of the *aliased* path. `find` and `rg --follow` treat an
   ancestor's `(st_dev, st_ino)` reappearing below it as a filesystem loop. The
   thread view now mixes the tgid into the hash.
8. **A too-small `getdents64` buffer returned 0 ("end of directory")** instead
   of `EINVAL`, which silently truncated the listing.
9. **`/proc` root did not list `self`.** It does now, as a symlink, and only for
   a caller that has a process.
10. **`read_symlink` for `<pid>/fd/<n>` skipped the box rule.** It is now behind
    the same resolve as everything else.

Allocation cuts along the way:
- `exists`/`stat` of a pid no longer render the whole syscall log
  (`akuma_syscalls_log::has_log`).
- `has_fd` no longer clones a `FileDescriptor`.
- `maps`/`statm` existence asks for rows without formatting them.
- The canonical path is built on the stack; it was a `String` per call.

## 3. What the model owns now

`crates/akuma-procfs/src/tree.rs` (no_std, `forbid(unsafe_code)`, no
allocation) holds:
- `ProcModel`: ten questions about the machine (pids, `process(pid) ->
  {tgid, box_id, exited}`, fds, syscall logs, address space, the caller's box
  and tgid, feature switches), plus an overridable `for_each_live_member`.
- `resolve(model, path) -> Option<Node>` and `list(model, path, emit)`.
  `Node` names every kind of `/proc` entry. A thread view carries `thread_of`,
  so it has no `task` of its own **by construction**, not by a check someone
  can forget.
- `Node::kind()`, `Node::owner()` and `Node::write_canonical()`. The last folds
  `self/` and `<tgid>/task/<tid>/` to `<pid>/` for the renderers.

`ProcFilesystem` routes `read_dir`, `exists`, `is_symlink`, `read_symlink` and
`metadata` through it. `metadata` is one `match` on the node instead of about
300 lines of re-parsing. `read_at`/`read_file` get the canonical path. The
kernel adapter (`KernelModel`) holds three scalars, not a table snapshot,
because 256 rows is about 6 KB and these calls run deep in path resolution on
amd64's 32 KB stacks. Pid scans use a 1 KB stack array, so no callback runs with
IRQs off.

**Still per-kernel:** the `maps`/`statm` walk (hook), and the renderers'
`Vec`/`String` returns (the trait's signatures).

## 4. The simulation

`crates/akuma-procfs/src/tree_tests.rs` builds a fake machine: a three-thread
group, a zombie, a group whose leader exited while a thread lives on, a boxed
process with stderr closed, and an exited pid with only a retained log. It then
walks the whole tree breadth-first with a depth cap. For every directory it
asserts:
- it lists the same thing twice in a row;
- no name appears twice, and the listing never contains `.`/`..` itself;
- every listed name resolves, to the kind it was listed as;
- no path is reached twice;
- the walk never goes deeper than the real maximum (4: `<tgid>/task/<tid>/fd`);
- the `getdents64` stream (dot records, then the listing, through the real
  `dirent::fill`/`records` with a 48-byte buffer) carries `.` and `..` exactly
  once, first, and every name in order.

A second walk follows `self` the way `rg --follow` / `find -L` do, deduplicating
by canonical node, and must terminate. The machine is also walked from inside a
box (no host pid is reachable), as a kernel thread (no `self`), with every
feature off, and over **300 random process tables × 3 viewers**.

There is a named regression test for each bug in § 2. **Mutation-checked:**
putting bug 2 or bug 3 back into `tree.rs` fails 6 tests each. The walk reports
bug 2 as `"1/task/1" lists "task" but "1/task/1/task" does not resolve`, which
is the ryzen's `find` error.

What it cannot see:
- the renderers' bytes;
- the VFS path normaliser;
- the syscall layer's followed `stat` (it checks `lstat`-kind agreement);
- races between a listing and a `stat`.

## 5. Consequences for userspace

`libakuma::ReadDir` now skips `.`/`..`, as `std::fs::read_dir` does. **Binaries
built against the old libakuma see them**, and three callers recurse on
`is_dir` without checking names: `box` (`main.rs:466` tree copy) and `scratch`
(`commit.rs:392`, `index.rs:235`). Rebuild and restage those along with the
kernel. herd (filters `.conf`), `wifi` (skips directories) and `box/fsx.rs`
(filters dots) are safe.

## 6. The Bun / Claude Code symptom: still open

Interactive Claude Code on the ryzen runs `getdents64` + `madvise` at about
45,000/s each, and its `[FileIndex] ripgrep: 0 files` takes about 4.5 s. It
reproduced again on 4211281b at 18:49:53 on 2026-10-16: `ripgrep: 0 files in
4853ms`, just after `not a git repo, falling back to ripgrep`.

**The walker has not been identified.** What the code establishes:
- No `/proc` path is unbounded, either before or after this change. The old
  nesting stopped at `task/<tid>/task/<tid>`, and the simulation proves the new
  tree is finite.
- With no dot records, an empty directory is **one** `getdents64` returning 0.
  An exact 1:1 getdents64:madvise ratio while the cwd is empty fits **one
  directory read in a loop** (open, one `getdents64` → 0, close, free →
  allocator purge) at least as well as a deep walk. inotify is `ENOSYS`, so a
  file watcher that falls back to polling is a live candidate.
- If the loop is in Claude Code itself, the 4.5 s ripgrep time may be
  starvation, not ripgrep walking: `getdents64` takes the VFS BKL guard on every
  call.

The discriminator is the **result** column of the looping process's
`/proc/<pid>/syscalls`. A run of `217 … 0` (getdents64 → 0) means an
empty-directory re-read; positive results mean a walk. The kernel change alters
the shape: an empty directory now costs two calls.

## 7. Verification

**Done:**
- Host tests: the whole workspace, 2156 passed, 0 failed. `akuma-procfs` 40
  (16 of them the tree simulation), `akuma-syscalls-linux` dirent 8,
  `akuma-syscalls-log` 3.
- Clippy `-D warnings` on every touched crate (host target), the AArch64
  release kernel and extreme-size.
- The amd64 kernel builds, plain and `no-tests`.

**Before-fix probe on metal** (ryzen, 4211281b):

```
--- ls -la empty dir
total 0
--- ls /proc/818/task/818
cmdline  maps  oom_score  statm  task  exe  mounts  oom_score_adj  status  fd  oom_adj  stat  syscalls
--- find /proc -maxdepth 6 (errors)
find: /proc/1/task/1/task/1: No such file or directory   (one per pid)
```

**Not done:**
- **The metal after-fix run.** Installing a kernel needs a reboot to Pop, which
  wasn't permitted in this session.
- **A local AArch64 QEMU boot.** The patched default `cargo run --release`
  faulted in the boot self-tests: `EC=0x0`, ELR in `run_memory_tests`, and the
  word at ELR was `0xabababab`, i.e. code memory read as filler. That is right
  after the DAIF tests and before any `/proc` or `getdents64` code runs. The
  clean-HEAD A/B that would separate a pre-existing failure from this change was
  not run, so treat the AArch64 boot as **unverified**.

Commands for the ryzen are in the session report. Use
`scripts/utils/ssh_tui_probe.py` for Claude Code (it never kills).

## Background

- [`AKUMA_PROC_ENUMERATION_HANG.md`](AKUMA_PROC_ENUMERATION_HANG.md): the
  previous `/proc` walker hang (unbounded listing of dead pids).
- [`AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`](AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md)
  Fix 13: where `<pid>/task` came from.
- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issues 4 and 24: the earlier
  advertised-but-unreadable and box-leak bugs from the same five-parser design.
- [`AKUMA_STALE_TID_ZOMBIE_SLOT.md`](AKUMA_STALE_TID_ZOMBIE_SLOT.md): the
  zombie-slot semantics the "zombie is a stable stub" rule leans on.
