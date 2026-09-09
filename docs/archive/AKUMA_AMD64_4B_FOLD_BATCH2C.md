# amd64 C1 step 4b, batch 2c: one `/proc`

**Date:** 2026-09-10
**Status:** landed.
**Predecessor:** `AKUMA_AMD64_4B_FOLD_BATCH2B.md` (`close` folded; the boot row
got an identity; `/proc/<pid>/exe` moved into procfs).

This target rendered `/proc` itself — from its own spawn table, intercepting
ahead of the VFS in `openat`, `read`, `pread`, `lseek`, `fstat`, `newfstatat`,
`access` and `write` — while the shared `ProcFilesystem` was *also* mounted at
`/proc` and served whatever the local view declined. Two implementations of one
namespace, differing about what exists.

It is one now. `amd64/src/fd.rs` lost ~500 lines: `open_proc`,
`render_proc_file`, `render_pid_file`, `render_self_maps`, `render_self_statm`,
`self_map_rows`, `render_meminfo`, `proc_metadata`, `proc_is_dir`,
`normalise_proc`, `pid_files`, `proc_rest_of`, `install_synthetic_dir`,
`install_synthetic_file` and their constants; `usermode.rs` lost `proc_list`,
`current_resident_pages` and three quarters of `ProcEntry`.

## Why it existed, and why that reason was spent

`/proc` on this target was a real, empty ext2 directory: `getdents64` succeeded
and returned nothing, so `ps` printed its header and stopped. The synthetic view
answered that. When 5b slice 3 mounted the real `ProcFilesystem` it was mounted
**additively**, and the comment there says why: at that moment this kernel
registered nothing in `akuma-exec`'s process table, so "mounting it would have
replaced a working synthetic `/proc` with a report of an empty machine".

Every process is registered now. The same comment says what that buys: the
mount "renders them for **all** pids, where `fd.rs` could only ever describe
the running one". The AArch64 kernel is the proof — no synthetic `/proc`
anywhere, and 25 `register_at_syscall_process` calls in its suite.

## What had to move first

- **`maps` and `statm`**, and this one needed a decision. Only this kernel
  served them, through the *shared* `akuma_procfs::render_maps_line` /
  `render_statm` — because the walk behind them is not portable:
  `UserAddressSpace::for_each_user_leaf` is `x86_walk_leaves` underneath and
  AArch64 has no counterpart. And the region list alone is not an answer: an
  ELF image and its initial stack are placed by the loader and are not regions,
  so a regions-only `maps` renders **empty** for an ordinary program — worse
  than absent, because a reader scanning for the mapping containing an address
  gets a confident "there is none".

  So `VfsGlueHooks` gained `pid_map_rows: fn(u32) -> Option<Vec<MapRow>>`.
  amd64 registers `fd::pid_map_rows`, which generalises its old *self-only*
  walk to any pid (`with_process(pid, …)` for both the regions and the leaf
  walk); AArch64 registers `|_| None` and keeps today's behaviour exactly. Both
  files exist precisely when the hook can render them — including in the
  directory listing, because advertising a name that does not `stat` is how
  `ls /proc/<pid>` prints `No such file or directory` for its own listing.
- **`/proc/net/dev`** needed nothing: the shared procfs already renders it
  through the same `akuma_syscalls_net::write_proc_net_dev` this kernel used.
- **`/proc` writes stopped being dropped.** `sys_write_file` had a branch that
  "accepted and dropped" writes to a synthetic path. They reach `write_at` now,
  which is what a mount expects.

## What did not move: `/proc/<pid>/fd/0`

The last path this kernel answers for itself, and it stays because the
difference is **semantic, not structural**. `sshd`'s bridge opens it and gets
the *write end of the child's stdin pipe*; on Linux — and in the shared
`ProcFilesystem`, which serves this path through
`akuma_exec::process::write_to_process_stdin` — writing to a process's fd 0
delivers into **its** stdin, and that sink is a `StdioBuffer`/`ProcessChannel`
which this target's children (fd 0 = `PipeRead` since C2 slice 6) never read.

Closing it means teaching the shared sink to find the target's real stdin — its
own `get_fd(0)`, where a `PipeRead(id)` means "write into that pipe". That is a
behaviour change on **both** kernels (an AArch64 child in `cmd | cmd` also has a
pipe at fd 0), so it waits for a working AArch64 verification loop. The payoff
is recorded in BATCH2A: it deletes the interception and gains the spawner
permission check, `delegate_pid` indirection, and the terminal line discipline
that turns an INTR byte into `SIGINT` on the foreground process group.

## What `/proc` looks like now

Richer than the view it replaced, on every rig. `ls /proc` was pids + `self`,
`net`, `meminfo`, `mounts`; it is now those plus `boxes`, `cores`,
`filesystems`, `loadavg`, `stat`, `sysvipc` and `uptime`, with per-pid
`cmdline`, `stat`, `status`, `mounts`, `exe`, `fd/`, `maps` and `statm`.
Measured on the metal: `ps` lists the tree, `/proc/self/maps` renders the real
mappings, `statm` its seven counts, `readlink /proc/self/exe` names the binary.

## Verification

| gate | before 2c | after |
|---|---|---|
| QEMU/TCG `SMP=1` / `SMP=4` | 579/0 / 589/0 | **579/0 / 589/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 565/0 / 575/0 | **565/0 / 575/0** |
| bare metal | 579/0 | **579/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** |
| metal `apk update` | OK | **OK** |
| clippy, both kernels | clean | **clean** |

The check count is unchanged on purpose: `proc_consistency_check` — *open, stat
and access must agree about every `/proc` path* — is the same suite section it
always was, now asserting against the mounted filesystem. It passing unchanged
is the statement that the two implementations really did agree about the paths
that mattered, and that the survivor answers for all of them.

## Left for `openat`

Only the four flags glue does not enforce (`O_DIRECTORY`, `O_EXCL`,
`O_NOFOLLOW`, the directory-write guard) and the `fd/0` interception above.
