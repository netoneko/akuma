# Rust syscalls on this kernel — what is served, what is not, the order

**Stability: C** (current state after the stage 9–11 planning session pn; re-measure
against the `[err]` trace after every slice). The full gap analysis with routes and
costs is `PLAN-pn.md` at the repo root; the scored archive lists are
[`rust-toolchain.md`](rust-toolchain.md). The code wins where any of these disagree.

## Measured, not assumed

`rustc --version` as init (tip `bc626f98`, QEMU TCG, nightly
`1.100.0-nightly (420ed2a0c 2026-09-18)`, 3 GiB RAM, slab raised to 1 GiB) makes
~25 distinct syscalls before it stops at its **first `clone`** — the ctrlc thread —
which the kernel refuses (`EPERM`, reported by musl as `EAGAIN`): the loader,
`arch_prctl`, `set_tid_address`, `brk`, `mmap` ×1,238 / `munmap` ×612 / `mremap` ×8 /
`mprotect` ×5 (the slab), `fcntl`, `rt_sigprocmask`, `rt_sigaction`, `sigaltstack`,
`membarrier`, `getrandom`, `getpid`, `getcwd`, `clock_gettime`, the file family, the
legacy `open`(2)/`stat`(4)/`poll`(7)/`readlink`(89), `ioctl` (ENOTTY), then `clone`.
`rustc` needed **≥2 GB RAM for `hello.rs`** in the archive
(`docs/archive/AKUMA_SELF_HOSTING.md` §1.4) — the 64 MiB slab is why it ENOMEMs here.

## Served today (rustc's startup path minus the wall)

`execve`, ELF load (eager), `brk`, `mmap` (anon + MAP_PRIVATE fill), `mremap`
(MAYMOVE), `mprotect`/**`munmap` as accepted no-ops**, `fstat`/`newfstatat`/`lseek`/
`read`/`write`/`writev`, `openat` + the `*at` fs family, `getcwd` ("`/`"), `fcntl`,
`getrandom`, `clock_gettime`, `getpid`/`gettid`(1), `arch_prctl`, `set_tid_address`,
`uname`, `eventfd2`, `ftruncate`, `umask`, `gettimeofday`, `pipe2`/`dup`/`dup2`/`dup3`,
`wait4`/`fork`/`vfork`/`execve` (real, pm's M4 slice 3), `exit`/`exit_group`,
`rt_sigaction`/`rt_sigprocmask` as no-ops. Benign by design: `ioctl`→`ENOTTY`
(serial console), probe `ENOENT`s.

## Not served, in the order the work happens

1. **Independent of the process-model switch (session pm) — rustc start:**
   the slab size (one 64 MiB `SLAB` const, `main.rs:642`, bounds brk *and* mmap),
   a real `mprotect` (with `PROT_EXEC`), `munmap` effects, `sigaltstack`,
   `membarrier`, `madvise`, the legacy x86_64 numbers (`readlink` 89, and git's
   82/83/84/87/88/90/92/94), long argv/env + `E2BIG`, env seeding
   (`LD_LIBRARY_PATH` — musl expands `$ORIGIN` via `/proc/self/exe`, which does
   not exist here).
2. **After pm S1–S3 (init as a registered `Process`; clone → `fork_process`) —
   the thread wall:** thread `clone` (`akuma_exec::process::clone_thread`), futex
   (`akuma-syscalls-sync` decisions: WAIT/WAKE, `FUTEX_WAIT_BITSET` absolute
   deadlines, `WAKE_OP`, requeue; effects = park/wake), thread groups + `exit_group`
   worker reaping, `set_robust_list`, `sched_getaffinity` (rustc sizes its codegen
   pool from it), `getpriority`/`setpriority`, `prlimit64`, `getrusage`, `rseq`
   (honest `ENOSYS`), `tkill`/`tgkill` + real signal dispositions and delivery.
3. **The linker wall — `rustc hello.rs`:** `socketpair` + `sendmsg`/`recvmsg`
   (AF_UNIX, the libstd spawn handshake), `close_range`, the linker fork's eager
   ~2 GB copy (slow, not wrong).
4. **cargo:** `pwrite64`/`pwritev2`, `fsync`/`fdatasync`, `linkat` (no ext2
   hardlink arm yet), real `chdir`/`getcwd`, and writable `MAP_SHARED` file
   mappings + `msync` writeback — without which `rust-lld`'s output is a
   **0-byte binary with cargo reporting success** (the archive's recorded failure).

Every item names its crate route in `PLAN-pn.md` §2; the order with gates and
per-step costs is `PLAN-pn.md` §3.


## Status 2026-10-30 (supervisor's QEMU loop; code wins over the tables above)
Served since this page was written, each with the A/B or gate that showed it: the legacy path numbers (`akuma-syscalls-abi::legacy`, one fold for amd64 and this kernel), real `mprotect` (PTE re-map; `PROT_NONE` stays a no-op), `sigaltstack` (per-task record), `membarrier` (decode from `akuma-syscalls-mem`), `chdir`/`fchdir`/`getcwd` with a per-task cwd, `futex` (effects in `futex.rs` over `akuma-syscalls-sync`), `clone(CLONE_THREAD)` through `akuma_exec::process::clone_thread` (threads share their creator's task record, `TASK_OF`), `set_tid_address`/`gettid`, `fsync`/`fdatasync`/`sync`/`syncfs` (`fs.sync()`), `/dev/urandom`, `socketpair(AF_UNIX)` (two cross-wired pipes: a byte stream, not `SEQPACKET` messages), 1 GiB mmap / 256 MiB brk slabs, argv/env up to 128 strings of 4 KiB.
Measured: `rustc --version` exits 0 from an ssh shell; `rustc hello.rs` reaches the link step (all codegen runs); `git clone git://...` completes. `[nosys]` still printed: `sendfile` (40), `mount`/`umount2` (165/166, apk's busybox trigger), `sched_getaffinity` (204), `prctl` (157). Not served: `sendmsg`/`recvmsg` with `SCM_RIGHTS` on a socketpair, signals beyond no-ops (S8), writable `MAP_SHARED` + `msync` (S7: without it a linker writes a 0-byte output while cargo reports success), `linkat` (ext2 op), `pwrite64`/`pwritev2` coverage for cargo.
A program run AS init (`init=.../rustc`) cannot create a thread: init is not a registered Process (`pm` S1).
