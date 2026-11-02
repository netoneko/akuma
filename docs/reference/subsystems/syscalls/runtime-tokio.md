# `akuma-runtime-tokio` — the syscall surface of a tokio program

> **Stability: B** (one consumer, Taoxichuan; the allow-list is pinned by host tests against a measured
> census, but a second consumer has not tried it yet).

A composition root that depends on `crates/akuma-runtime-tokio` runs tokio: "this kernel runs tokio" is a
dependency that ensures Linux binary compatibility. The crate is a **surface**, not a family: it implements no syscall. It decides which of
`akuma-syscalls-glue`'s calls a kernel serves and sets glue up to serve them. Source:
`crates/akuma-runtime-tokio/src/lib.rs`.

## What it owns

- **The allow-list**, `served_by_glue(call)`: which calls glue answers, grouped by the tokio feature that needs
  them. A call outside it is the kernel's or nobody's.
- **The setup**, `install(Kernel)`: glue's and `akuma-vfs-glue`'s hooks with a tokio kernel's answers (no rump,
  console poller, mouse, tun, audio or file-page cache), the pipe wake sink and the RNG hook. The kernel
  supplies only what is its own: `utc_time_us`, `online_cpus`, `rng_fill`, and `after_reap` (run after every
  `wait4`/`waitid`).
- **The dispatch**, `dispatch(nr, args) -> Option<u64>` over **x86_64** numbers: `pidfd_open` and
  `pidfd_send_signal` by their shared numbers, then an allow-listed call to `handle_syscall` by its asm-generic
  number (`Syscall::to_aarch64`), then everything else to `akuma_syscalls_glue_amd64::dispatch` (the x86_64
  fd-family layer), which answers `None` for what it does not know. `None` means the kernel must answer.

What stays in the kernel: memory (`mmap` and friends, `brk`), `clone`/`execve`/`exit`, signal delivery and the
signal frame, and its own devices. They move here only when glue owns one implementation of each.

## Features

| feature | default | calls |
|---|---|---|
| (always) | — | `runtime`: the 13 calls every tokio program made in the census (identity, `sched_getaffinity`, `getrandom`, signal dispositions and masks, `sigaltstack`, `prctl`, `set_tid_address`, `clock_gettime`, `futex`) plus `nanosleep` and `clock_nanosleep` |
| `net` | yes | the socket calls; also brings glue's smoltcp socket table into the build |
| `process` | no | `wait4`, `waitid`, the two pidfd calls |
| `shell` | no | 23 calls busybox and bash make and no tokio program does (job-control ids, limits and accounting, `sync`, `membarrier`, the clock and hostname setters) |

## Invariants

- **The groups are pinned by host tests**: `groups_are_taoxichuans_former_list` (all four groups together route
  exactly as Taoxichuan's own list did before the move), `groups_are_disjoint`, and `runtime_is_the_census` (the
  always-on group is the B10 census's every-program set plus the two sleeps). Adding a call to a group changes
  one of these on purpose.
- **A call is answered by one layer.** An allow-listed call never reaches the fd-family layer, so a number that
  both could serve is decided by which group it is in, not by order of luck.
- **Akuma-native calls are not on the surface.** Glue's private numbers (`SPAWN`, `WAITPID`, the terminal calls,
  …) are behind glue's `akuma-native` feature, which a tokio kernel leaves off
  ([`../config-flags.md`](../config-flags.md) § "Crate features Taoxichuan leaves off").

## Checking a workload against it

`scripts/kvm/taoxichuan/census_classify.py OUT UNISTD_64_H` reads an `strace -f -c` census and the source (the
groups, glue-amd64's dispatch, the legacy fold, the kernel's own arms) and names who serves each call; it exits 1
on an unserved one. `census_pop.sh` records the census on Linux.

## Background

[`archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md`](../../../archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md) (the census: 76
calls over three tokio programs, 0 unserved), [`archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](../../../archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md)
(the static analysis that shaped it), [`archive/TAOXICHUAN_CLEANUP_PLAN.md`](../../../archive/TAOXICHUAN_CLEANUP_PLAN.md)
(B10, B11), `taoxichuan/docs/reference/syscalls.md` (its one consumer's dispatch order).
