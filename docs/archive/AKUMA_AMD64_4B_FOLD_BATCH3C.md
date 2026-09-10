# amd64 C1 step 4b, batch 3c: `fcntl`, `dup`, `dup2`, `dup3`, `pipe2`, `access`, `utimensat`

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `AKUMA_AMD64_4B_FOLD_BATCH3B.md` (the `stat` family), whose §7
lists this batch as *"mechanical"*.

Seven arms. `amd64/src/fd.rs`: **+83 / −448**, 3 448 → 3 083 lines. The batch is
mostly deletion — the arms it folds carried no vocabulary of their own, so what
came out is bodies, two local helper tables (`clone_refs`/`release_desc`,
`fs_err_errno`), the `resolve_at` dirfd ladder, and four now-unreferenced errno
constants.

## What each arm needed

| arm | how it folds | preamble |
|---|---|---|
| `fcntl` | forward to `glue::fs::sys_fcntl` | none — `cmd` (0–4, 1030) and the one `arg` bit either side reads (`O_NONBLOCK`) are identical across the ABIs |
| `dup` | forward to `glue::fs::sys_dup` | none |
| `dup3` | forward to `glue::fs::sys_dup3` | **`newfd < MAX_FDS`** — glue's table is a `BTreeMap`, this target's lookup refuses a number at or above `MAX_FDS` (the batch-2d finding) |
| `dup2` | x86-only shim over `glue::fs::sys_dup3` | the above, **plus `oldfd == newfd` returns `newfd`** without closing it (glue's `dup3` answers `EINVAL`) |
| `pipe2` | forward to `glue::pipe::sys_pipe2` | **`crate::pipe::at_capacity()`** — `MAX_PIPES` is this target's heap policy; glue's `pipe_create` is unbounded |
| `access` / `faccessat` | `to_glue(Syscall::Faccessat, …)` | none — the old shim dropped `dirfd`, glue's `sys_faccessat2` honours it |
| `utimensat` | `to_glue(Syscall::Utimensat, …)` | none — `struct timespec` is LP64 on both, `UTIME_NOW`/`UTIME_OMIT` and the `AT_*` flags are shared |

Every glue arm involved was already written and dispatched for AArch64; this
batch's diff in `crates/` is **`pub(super)` → `pub` and nothing else** — no
logic change, so the AArch64 kernel binary is unaffected.

## Gains, from folding

- **`fcntl` grew `F_GETLK`/`F_SETLK`/`F_SETLKW` and `F_SETOWN`/`F_GETOWN`** as
  accepted no-ops. nginx's `ngx_spawn_process` treats a failing `F_SETOWN` as
  fatal before it forks; the old arm answered `EINVAL`.
- **`dup`/`dup2`/`dup3`/`fcntl(F_DUPFD)` allocate the lowest free fd from 0**,
  not from `FIRST_FILE_FD` — Linux's "lowest available includes a closed
  0/1/2". The same convergence `openat` made in batch 2d.
- **`faccessat` honours `dirfd`.** The old shim resolved every path from the
  root regardless.
- **`utimensat` gained the `path == NULL` (`futimens(fd)`) form**, the ns-range
  validation, and `touch /dev/null` succeeding — where the old arm answered
  `EFAULT` for a NULL path. (The fd form is a validate-and-return-0 stub in
  glue, shared with AArch64; it does not actually stamp. `touch -d @<t> file`,
  the path form, does — verified on the metal.)

## The gap the fold surfaced: `akuma-syscalls-glue`'s own hook table

Glue's `sys_utimensat` reads `crate::hooks::utc_time_us()` for "set both times
to now", and `futex`'s absolute-deadline arm reads it too. That is
`akuma_syscalls_glue::SyscallHooks` — **a third hook registry**, distinct from
`akuma_vfs_glue::VfsGlueHooks` (which `fs::init_vfs` fills) and
`akuma_syscalls_linux`'s. The AArch64 kernel registers it from
`akuma-kernel-glue`, which does not build for `x86_64`, so on this target
`utc_time_us()` had always returned `None` — `touch`'s "now" was 1970 even on
the metal with SNTP synced, and every absolute-deadline futex wait computed its
deadline from a zero clock.

This is the batch-3a §1b shape exactly ("glue reads a hook amd64 never
registered"), and the fix is the same: `boot::install_shared_sinks` now calls
`akuma_syscalls_glue::set_hooks` with the two fields this target can answer
(`utc_time_us` = `clock::is_synced().then(clock::now_us)`, `probed_core_count` =
`smp::online_cpus`); the rump five are `false`/no-op because there is no rump
kernel here.

## What left `fd.rs`

- `dup_from`, `dup_onto` — the `F_DUPFD` and `dup2`/`dup3` bodies.
- `clone_refs` / `release_desc` — the local `PipeRead`/`PipeWrite`/`Socket`
  refcount helpers those used. `akuma_exec::process::clone_fd_refs` (which glue's
  `sys_dup`/`sys_dup3` call) is the one list now, with the same
  exhaustive-match property.
- `fs_err_errno` — the local `FsError` → errno table. `mkdirat`/`unlinkat`/
  `symlinkat`/`utimensat` each once carried a partial copy of it;
  `akuma-syscalls-glue::fs_error_to_errno` is the survivor.
- `resolve_at` — the local `*at` dirfd ladder. `openat` (2d), `newfstatat` (3b)
  and `utimensat`/`access` (3c) were its callers; `resolve_path_at` in glue is
  the one ladder, and it reads `Process::cwd` where this one hard-coded root.
- `errno::{ENOTEMPTY, EIO, EROFS, ENOSPC}` — only `fs_err_errno` used them.

## Verification

| gate | before 3c | after |
|---|---|---|
| QEMU/TCG `SMP=1` / `SMP=4` | 596/0 / 606/0 | **596/0 / 606/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 580/0 / 590/0 | **580/0 / 590/0** |
| bare metal (HP 500-502nj) | 596/0 | **596/0** |
| `amd64_ring3_check --smp 1 -n 40` / `-n 60` | (cliff at ~44 pre-fix) | **40/40 · 60/60**, heap +37 / +85 kB |
| bare-metal 60-session churn | — | **0 failures**, `free` flat |
| `lazybuf` / `openflags` (QEMU + metal) | 8/8 · 20/20 | **8/8 · 20/20** |
| redirect `>` `>>` `|`, 3-stage pipeline (QEMU + metal) | OK | **OK** |
| `touch -d @<t>` sets a real mtime (metal) | — | **`Sep 13 2020`** |
| `apk add file` (QEMU + metal) | OK | **OK** |
| host tests | 1372 | **1372** |
| clippy, both kernels | clean | **clean** |

No new checks — the batch adds no behaviour the existing suite does not
already exercise (the `dup` value-copy and lost-nonblock divergences are pinned
by boot checks that predate it; shell redirection is the `dup2`/`pipe2` test).
The `-n 40` ring-3 run passing is the standing confirmation that the pipe leak
`AKUMA_AMD64_4B_FOLD_BATCH3A.md` §6 recorded is fixed (it was `f4844617`, the
checkpoint this batch builds on): `-n 40` used to sit one session under the
cliff.

AArch64 is not touched: the only `crates/` change is six `pub(super)` →
`pub`.

## What is left in `fd.rs`

The readiness and terminal arms, which need care rather than a forward:

| arm | why it is not a forward |
|---|---|
| `poll` / `select` | `akuma-syscalls-poll` + `akuma-net-yarn` — the amd64 arms are already `ppoll`/`pselect6`-shaped, but the readiness model (optimistic for regular files, real for a stdin pipe and a UDP socket, not-ready for TCP) is this target's, and folding means reconciling it with the shared `WaitPolicy` |
| `ioctl` | `TCGETS`/`TIOCGWINSZ`/`TIOCSWINSZ` and the `SIOCGIF*` set are shared; the framebuffer and USB-input ones are x86-only |
| `poll_input_event` (313) | x86-only, the USB keyboard — stays forever |
| `sys_write` | the serial console preamble + `WRITE_SEQ` + the `O_ACCMODE` refusal (batch 3a); the console is genuinely this target's |
| `sys_read` / `sys_pread64` / `sys_lseek` | the console / `ESPIPE` / `/dev`-node preambles from batch 3a |

## Background

- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3A.md` §1b — the prefault-hook gap,
  which `utc_time_us` here repeats one registry along.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2D.md` — the `MAX_FDS`-is-a-lookup-bound
  finding that `dup2`/`dup3` inherit.
- `docs/archive/NCA_FD_NONBLOCK_TOCTOU.md` — why `alloc_fd_from` clears the
  per-number flags, which is what makes `dup` lose `O_NONBLOCK` (pinned).
