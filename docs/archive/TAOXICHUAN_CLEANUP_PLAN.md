# Taoxichuan cleanup (Opus, from 2026-11-01)

**Stability: C**: a plan being carried out. Each phase's status lives in this file.

The experiment ended on 2026-11-01. Its question was answered: K1 == K2 under Linux KVM on 2026-10-30 and on bare metal on
2026-10-31. The agent log's entry "2026-11-01: the experiment is over" marks the cutover; everything after it is Opus. Before
the cutover, goose + GLM wrote Taoxichuan. This plan does not change what the kernel is for. It turns the experiment's kernel
into something worth keeping, in three phases, each gated by the previous one:

1. **goose + GLM runs on Taoxichuan.** The kernel runs the agent that built it.
2. **No duplicated logic left.** Whatever Taoxichuan decides that amd64 also decides is decided once, in a crate. What stays
   in `taoxichuan/src` is target glue: trap entry, CR3/fs effects, device arms, the boot program list.
3. **Cut a tokio kernel out of it.** A crate (or a small set of them) that a composition root depends on to get exactly the
   syscall surface a tokio program needs. This is the "next frontier" from the agent log's 2026-10-30 entries.

## Starting point (2026-11-01, `c362f165`)

| file | lines | `unsafe` (word count) |
|---|---:|---:|
| `main.rs` | 5,676 | 28 |
| `child.rs` | 2,628 | 26 |
| `sock.rs` | 1,126 | 0 |
| `exec_twin.rs` | 558 | 5 |
| `smp.rs` | 539 | - |
| `epoll.rs` | 534 | - |
| `pipes.rs` | 397 | 0 |
| `net.rs` | 377 | 2 |
| `futex.rs` | 160 | 0 |
| `sched.rs`, `ring.rs` | 136 | 1 |
| **total** | **12,131** | |

`dispatch_inner` is a single 1,747-line `match` (`main.rs`). The spawn-slot ring, the M2 crossing and the private
`SPAWN`/`WAITPID`/`CLOSE_CHILD_STDIN` syscalls are all still in `child.rs` (46 references), although sshd now forks per session.

**Target:** `taoxichuan/src` under 6,000 lines, with the fixed point, the battery and `cow_leak_gate` unchanged. The count is a
guard against moving code around without deleting any, not the goal itself.

## Gates

| gate | where | what passes |
|---|---|---|
| host tests + clippy | Mac | the pre-commit hook; `cargo test -p <crate>` for every touched crate |
| `unsafe` budget | Mac | `scripts/check_unsafe_budget.py` (kernels only ratchet down; new crates start at 0) |
| TCG battery | Mac, QEMU microvm TCG | quick execs, F1, PK1, legacy fs numbers, `apk add git`, `git --version` (the rig in § Rig) |
| goose | Mac TCG, then the ryzen | `goose run --no-session -t "Reply with exactly the word PONG"` prints PONG |
| leak | ryzen (Pop KVM) | `scripts/kvm/goose/cow_leak_gate.py --cycles 20`, delta 0 |
| fixed point | ryzen | K1 == K2 (`TAOXICHUAN_FIXED_POINT.md`); at the end of phase 2 and after anything that touches exec/fork/mm |

The in-guest `-j4` build is **not** a gate until the BKL convoy (`TAOXICHUAN_J4_BKL_CONVOY.md`) is understood. It fails
intermittently on every kernel tested.

## Phase 1: goose + GLM on Taoxichuan

| step | what | status |
|---|---|---|
| 1a | Every fd kept its own copy of the file size (`OpenFile::File.size`), so a second SQLite connection read the WAL short (`522`). Ask ext2 for the size instead and remove the field (`TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md`) | **done** 2026-11-01 |
| 1b | Run `goose run` again on TCG. Expect the next SQLite wall to be one of amd64's (`F_GETLK`, record locks, `MAP_SHARED` coherence of `-shm`), unless Taoxichuan already gets them right | **done**: no further SQLite wall; the next wall was the rig's 1970 clock (`epoch=`, now in `tcg_rig.py`) |
| 1c | goose's TLS to z.ai (rustls), then a tool call (`developer` extension: fork + exec of a shell) | **done** on TCG: PONG in about 8 s; the shell tool ran `uname -a` and a redirect |
| 1d | The same on the ryzen (KVM), against the metal image | |
| 1e | Side bug: a session shell's `exec 4<file` closed sshd's fd 4. Not table sharing: `dup2` displacing a socket, and both exec close-on-exec sweeps, released the connection unconditionally, although the parent's fd still named it. Now `sock::release_unnamed` (last name only) | **done** 2026-11-01; `battery` has a `dup2-sock` line |

## Phase 2: the rest of the duplicates

The order follows dependencies and risk. The pm slices (`taoxichuan/docs/reference/switch-to-crates.md`) and the X list
(`TAOXICHUAN_CRATE_EXTRACTION_PLAN.md`) are one programme here.

**The lever is `akuma-syscalls-glue`, not more per-family crates (decided 2026-11-01, after reading 2a).** The X list proposed
pulling each family's pure decisions into its own crate, so each kernel would keep its own arms over them. Starting on X6 showed
why that stalls. There are three `setsockopt`/`getsockopt` implementations (glue's, amd64's native `setsockopt`, Taoxichuan's),
and glue's carries lessons the other two lack: OpenSSH's `IP_OPTIONS` length-0 rule and the connect-timeout `ETIMEDOUT` for
`SO_ERROR`. amd64 only reaches glue's version because its descriptors live in `akuma-exec`'s registered `SharedFdTable`.
Taoxichuan keeps its own `OpenFile` table, so glue cannot see its fds, and every family it serves is a private copy.

amd64 solved exactly this in 2026-09, and the route is recorded:
- `AKUMA_AMD64_C1_STEP4A_VFS_ADOPTION.md`: its private mount table gave way to `akuma-vfs-glue`'s. That step comes first, because
  a folded arm resolving through an empty mount table answers `ENOENT` for every path, silently.
- `AKUMA_AMD64_4B_PREREQUISITES.md`: the open-flag permutation, now `akuma_syscalls_abi::open_flags`.
- `AKUMA_AMD64_4B_FLIP.md`: one registered `SharedFdTable` per process, with the reference rules for pipe ends and sockets.
- `AKUMA_AMD64_4B_FOLD_BATCH*.md`: the arms moved in batches, each gated by a boot.

Taoxichuan has the prerequisites amd64 had to build. It already registers every task as an `akuma-exec` process, and it already
depends on `akuma-syscalls-abi` (`legacy`, `open_flags`, `stat`). So phase 2 replays amd64's sequence; X6 and X7 as separate
crates are dropped.

| step | source | what | expected deletion |
|---|---|---|---:|
| 2a | amd64 C1 4a | mount the ext2 root into `akuma-vfs-glue`; `root_fs()` callers go through it. Add `akuma-vfs-glue` and `akuma-syscalls-glue` as dependencies, and check they build for `x86_64-unknown-none` the way amd64 builds them | small |
| 2b | amd64 4b flip | `OpenFile` and Taoxichuan's fd table give way to `akuma-exec-core`'s `FileDescriptor` in each process's registered `SharedFdTable`; pipes, sockets, eventfd and epoll keep their reference rules | ~400 |
| 2c | amd64 4b batches | fold arms into `akuma_syscalls_glue::handle_syscall` (`to_aarch64` numbers, `open_flags`/`stat` translation), batch by batch: path-only `*at`, then open/close/read/write/lseek/stat/getdents, then sockets (X6 for free), poll/select/epoll, pipes, then the rest. `dispatch_inner` shrinks to the legacy x86 table plus the arms that are genuinely this kernel's (pm S6) | most of 1,747 + ~1,000 of `sock.rs`/`pipes.rs`/`epoll.rs` |
| 2d | pm S2 | `wait4` through `akuma-exec`'s park/wake over the process table; delete `PARENT_OF` and the private wait scans | ~165 |
| 2e | pm S3 | delete the M2 crossing, the spawn-slot ring and the private syscalls (sshd is already `linux-abi`); `clone` through `fork_process` | ~700 |
| 2f | pm S4 | boot programs become fork+exec children of init | ~150 |
| 2g | X8 / pm S5 | `load_task` onto `akuma-elf-exec` | ~200 |
| 2h | X9, X10 | slab/hole placement into `akuma-mmap`; thread/root lifetime into `akuma-exec`; the FPU save pair into `akuma-fpu-amd64`; `ring.rs` into `akuma-primitives` | ~300 |
| 2i | pm S7 | sweep: `taoxichuan/docs/`, diagrams, dead types; regenerate `crate-safety.md` with `scripts/cloc_akuma.py` | rest |

**2b and 2c's fd families are one step (decided 2026-11-01, after batch A).** Inventory: about 70 `with_fds` sites and 130
`OpenFile` references, almost all inside the arms glue already implements (read/write/open/stat/dup/pipe/socket/poll/epoll/
eventfd/…). Porting them to the shared table and then deleting them is wasted work. And four `OpenFile` kinds are backed by
Taoxichuan's own tables (pipes, eventfd, epoll, socketpair-as-two-pipes), where glue keeps its own. amd64's lesson is that two
instances are two id spaces: a folded `close` closed the wrong pipe. So the table flips and the whole fd family folds in one change:

- **Already in place:** every registered process has `fds: Arc<SharedFdTable>` built `with_stdio()`, and `fork_process`
  deep-clones it. Glue writes `Stdout`/`Stderr` through the process channel or the console. Glue's pipe waits wake through
  `akuma-exec`'s `WakeHandle`, which the threading twin implements.
- **Folds to glue:** read/write/readv/writev/pread64/pwrite64, open/openat/close/lseek, fstat/newfstatat/stat/lstat/statfs/fstatfs,
  getdents64, fcntl, dup/dup2/dup3, pipe2, socketpair, all socket calls, poll/ppoll/select/pselect6, epoll_*, eventfd2,
  truncate/ftruncate, fsync/fdatasync, flock, utimensat, faccessat/access, the `*at` path calls, fchmod*/fchown*, umask,
  getcwd/chdir/fchdir, ioctl, copy_file_range, sendfile.
- **Stays local but reads the shared table:** file-backed `mmap` (fd -> `KernelFile` `(mount_id, inode)`), exec's close-on-exec sweep,
  the exit sweep, the private `SPAWN` (`ChildOut` becomes `FileDescriptor::ChildStdout` until S3 deletes it), and every local path
  user that needs the cwd (exec), which moves to `Process.cwd`.
- **Deleted:** `OpenFile`, `with_fds`, the `Task` fd fields, most of `sock.rs`, `pipes.rs`, `epoll.rs`, and the fd arms in `main.rs`.
- **Gate:** the battery + goose, plus the epoll/eventfd/tokio probe (`nettest-reqwest`, both runtimes) and `apk add`. Then the ryzen
  fixed point before anything else lands on top.

**The x86_64 ABI layer over glue becomes a crate (decided 2026-11-01, before the flip).** Glue speaks asm-generic, so amd64
does not hand it x86_64 fd calls raw. `amd64/src/fd.rs` holds a translation layer: open-flag permutation (`openat`),
asm-generic -> x86_64 `struct stat` (`fstat`, `newfstatat`), timeout conversion (`poll`, `select`), `dup2`'s `old == new` rule,
`pipe` -> `pipe2`, plus the legacy numbers with no asm-generic twin (`open` 2, `stat` 4, `lstat` 6, `poll` 7, `access` 21,
`pipe` 22, `select` 23, `dup2` 33, `epoll_create` 213, `epoll_wait` 232). Measured: of its 23 `sys_*` shims, the translating ones
are small and depend on amd64 only through `crate::uaccess` (a wrapper over `akuma-user-access`) and `MAX_FDS`. The device
intercepts (kvm, tun, wifi, touchpad, fbdev, power) sit only in `openat`, `read` and `ioctl`. Folding Taoxichuan straight into
`handle_syscall` would hand x86_64 programs aarch64-shaped stat structs and permuted open flags, and copying the shims would be
a third copy. So:

| step | what |
|---|---|
| 2c-B1 | new crate `akuma-syscalls-glue-amd64` (the `-glue` exception: it calls glue, so it is not host-testable; `-amd64`: x86_64 ABI only): the translating shims moved out of `amd64/src/fd.rs`, a `dispatch_fd(nr, args) -> Option<u64>` over x86_64 numbers including the legacy ones, and the stat/flag translations' host tests where they are pure. User memory through `akuma-user-access`. |
| 2c-B2 | amd64 calls it after its device intercepts. Gate: amd64 boot suite on QEMU TCG (`amd64/run.sh`) plus `scripts/box_amd64_suite.py`. `amd64/src/fd.rs` shrinks by the moved shims. |
| 2c-B3 | Taoxichuan's flip (below) calls `dispatch_fd` instead of `handle_syscall` for the fd family. |

Risks carried over from amd64: glue takes the BKL-shaped guards (`NetBklGuard`) and expects the exec runtime hooks
Taoxichuan's `exec_twin.rs` registers. Check those before 2c's first batch, not after. A per-family divergence that
Taoxichuan has and glue lacks (its `O_APPEND`-at-open, its `sendmsg` per-iovec datagrams, `ENOPROTOOPT` for unknown socket
options) is resolved toward glue's answer, which is the one the other two kernels run. Each such change is named in that batch's
commit, and the battery decides.

Each step records the kernel's line and `unsafe` counts before and after in the status table below. Each step is one commit
(or a short series), pushed when its gates pass.

## Phase 3: a tokio kernel

The idea (agent log, 2026-10-30): a crate a composition root depends on so that "this kernel runs tokio" is a dependency,
not a port. Phase 2 makes this mostly packaging, because the families tokio needs (readiness and epoll, eventfd, futex,
threads, sockets, clocks, signals, mmap, pipes) are crates already.

| step | what |
|---|---|
| 3a | **Census.** Record the syscalls of unmodified tokio programs on Linux (`strace -f -c` on the ryzen under Pop): `nettest-reqwest` (client + rustls), `kot` (axum server), goose. The union is the feature list. Compare it with Taoxichuan's `[nosys]` and served lists. |
| 3b | **Shape.** Decide between one crate with cargo features per family, and a template composition root. Default: one crate, `akuma-runtime-tokio`, named for what it owns (the syscall surface of a runtime). It owns the dispatch table for the census set and the per-family wiring; it holds no arch code, and the arch half stays in the kernel (`-amd64` suffix if one is needed). |
| 3c | **Taoxichuan consumes it.** Its dispatcher shrinks to "the tokio surface + boot glue". Any syscall outside the census either moves behind a feature or leaves. |
| 3d | **Gate.** The census programs run unmodified in a KVM guest: `nettest-reqwest` GET/POST and the delay sweep, `kot` serving, goose PONG plus a tool call. Run the Linux-differential suites (`scripts/epoll_suite.py --linux` and siblings) against the composed kernel. |
| 3e | **Cost report.** `scripts/cloc_akuma.py --audit` for the tokio kernel: what it takes to review. |

## Rig

TCG on the Mac, no ryzen needed:

- Kernel: `cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests,trace-err`.
- Image: the fork-sshd ext2 image (sshd built `linux-abi`, busybox, apk, CA bundle). For goose, add `/usr/local/bin/goose`
  (the `goose-x86_64-unknown-linux-musl` 1.52.0 release asset; check the digest), `/root/.config/goose/{config,secrets}.yaml`
  and `/etc/goose.env` with `debugfs -w`, **only while the guest is down**.
- Boot: `qemu-system-x86_64 -M microvm,accel=tcg -cpu max -m 3072`, blk on `virtio-mmio-bus.0`, user-net on `bus.1` with a
  `hostfwd` to the guest's port 22, and cmdline `init=/bin/sshd initargs=--port,22` plus four `virtio_mmio.device=` entries.
  Ready in about 20 s.

`scripts/kvm/taoxichuan/tcg_rig.py` holds this (`build`, `boot [fresh]`, `run 'CMD'`, `stop`). It boots through a qcow2 overlay and
passes `epoch=`. Without it the guest clock reads 1970 and every TLS client refuses certificates.

## Status

| step | commit | lines before -> after | notes |
|---|---|---|---|
| plan | - | 12,131 | this file |
| 1a–1c | `c0a921a2` | 12,131 -> 12,137 | the size is the inode's; `tcg_rig.py`; goose + GLM prompt and tool call pass on TCG |
| plan | `9c44ba87` | - | phase 2 replays amd64's glue adoption instead of per-family crates |
| 1e | `a29b690d` | 12,137 -> 12,144 | socket released by its last name only (`dup2`, exec sweeps); `tcg_rig.py battery [goose]`: 10/10 PASS, 0 faults |
| 2a | `dcfe2c20` | 12,144 -> 12,169 | `akuma-vfs-glue` mount table created, its hooks installed, the ext2 root mounted at `/` as the same `Arc` `root_fs()` borrows. Nothing resolves through it yet. Image +32 KB (1,592,120 -> 1,624,112 bytes); battery 9/9 PASS, 0 faults |
| 2c-0 + batch A | `74962ae5` | 12,169 -> 12,167 | **Glue is live on Taoxichuan.** Prerequisites: `akuma-syscalls-glue` + `akuma-user-access` deps; glue's `SyscallHooks` and the RNG hook registered beside the vfs-glue mount; the page-fault handler gains amd64's two ring-0 arms (a write to a present user page breaks CoW from **either** ring, since glue writes user memory directly under `CR0.WP`; a fault inside the shared copy loop resumes at `user_copy_fixup`'s `EFAULT` trampoline). `served_by_glue` is the folded set, checked after the legacy fold. Batch A: `uname`, `sethostname`/`setdomainname` (new), `getpid`/`getppid`/`gettid`, the uid/gid four, `getsid`/`setsid`/`getpgid`/`setpgid`, `sched_getaffinity`, `getrandom`, `membarrier`, `sync`/`syncfs`, plus `sysinfo`/`getrusage`/`getgroups`/`times` (new). Stubs that got real answers: `getppid` was always 1, `getsid`/`getpgid` echoed the pid, `sched_getaffinity` reported one CPU. `uname` now says `Akuma akuma 0.0.8 <sha>-release x86_64`, as amd64 and AArch64 do. `prlimit64` stays local: glue answers `RLIMIT_STACK` from `ExecConfig.user_stack_size` (64 KiB here), which `fork_process` also uses to size the stack copy; revisit at 2e. Image 1.62 -> 2.63 MB: glue is linked whole the first time an arm uses it, and the deletions are what pay it back. Battery 9/9 + goose, 0 faults. `free` stops at `/proc/meminfo` (no `/proc` mounted; was ENOSYS earlier) |
| 2e (S3, first half) | `f821ddda` | 12,167 -> 11,408 | **The private spawn path is deleted.** `SPAWN`, `WAITPID`, `CLOSE_CHILD_STDIN` (nothing on the image uses them: sshd is the `linux-abi` build), `exec_replace` and `exec_return` (reachable only from a `SPAWN` child), `fork_arm_blocking` and the `REAL_FORK` switch (true since session pq), `fork_share_memory`, `release_spawn_image`, the `ChildOut` fd kind with its read, poll and epoll arms, and the crossing record's two parent-resume fields. Done before the fd flip because every one of these copied or swept fds. Found by the compiler: the crate denies dead code, so removing the three dispatch arms named the rest. `child.rs` 2,628 -> 1,933. Battery 9/9 + goose, 0 faults; clippy 18 warnings (was 19) |
| 2b + 2c-B1 (flip, checkpoint) | `0c63f489` | 11,408 -> 11,447 (old fd code still present, shadowed) | **The fd table is `akuma-exec`'s and the fd family is glue's.** New crate `akuma-syscalls-glue-amd64` (439 lines, 5 host tests): `dispatch(nr, args)` serves the x86_64 fd family over glue (open-flag permutation, asm-generic -> x86_64 `struct stat`, the legacy numbers 2/4/6/7/21/22/23/33/85/213/232, `dup2`'s rule, `poll`/`select` timeouts, chunked large reads, the `O_RDONLY` write refusal, `readv`/`writev` as loops). Sockets (AF_INET and AF_UNIX) go to glue through `served_by_glue`. The runtime's release hooks point at glue's pipe/eventfd/epoll/unix/flock tables; glue's `sc-epoll`/`sc-eventfd` are on. Local users ported: exec sweeps close-on-exec on the shared table and resolves its path against `Process.cwd` (relative `execve` now works), exit closes the shared table, file `mmap` finds its inode through the `KernelFile`. `sendfile`/`copy_file_range` answer ENOSYS (glue has neither; busybox and Rust's `fs::copy` fall back). Five bugs on the way, each found on the rig: (1) `akuma_vfs_glue::fs::mark_initialized` was never called, so every glue path answered ENOENT; (2) glue buffers stdout in `current_channel()`, which for a forked child is its *exit* channel, so the console shim keys on `Process.channel`; (3) the syscall stub's user-RSP stash is now saved and restored around every call, since glue's waits switch threads; (4) glue's pipe wake sink is amd64's (wake the slot with a fresh handle); (5) SIGPIPE's default action runs glue's `sys_exit_group`, which kills the caller through `akuma-exec`, so the ring row never left `running` and the shell's `wait4` slept forever. `child::on_thread_terminated` (the slot-purge hook) now records the status and wakes the waiter, a bridge until pm S2. Image 2.63 -> 2.69 MB. Battery 9/9 + goose PONG + goose tool call; `nettest-reqwest` both runtimes OK, first byte 0.9 s (was 2.0 s) |
| 2b/2c (the fd code deleted) | `b2695540` | 11,447 -> 7,222 | **`sock.rs`, `pipes.rs` and `epoll.rs` are gone, and so are `OpenFile`, `with_fds`, the `Task` fd fields and cwd, and 69 `dispatch_inner` arms.** The arms were removed mechanically (every arm whose numbers are all served upstream; the compiler cannot see dead match arms), then the deny-dead-code cascade took 101 items in one round. `execve`'s relative path now resolves through glue against `Process.cwd`; `bind_child_task` no longer copies fds (`fork_process` deep-cloned the table, epoll fds stripped). Six dependencies dropped (`akuma-fdtable`, `akuma-pipes`, `akuma-syscalls-poll`, `akuma-net-yarn`, `akuma-syscalls-eventfd`, `akuma-syscalls-net`); the kernel still has their behaviour, through glue. `main.rs` 5,676 -> 3,563 and `child.rs` 2,628 -> 1,891 since the cleanup began. Image 2.69 -> 2.49 MB. Clippy warnings 18 -> 7. Battery 9/9 + goose PONG + tool call; `nettest-reqwest` both runtimes |
| signals: one table | `3f1a78e9` | 7,222 -> 7,130 | **Dispositions and masks are `akuma-exec`'s.** After the flip, `rt_sigaction` still wrote Taoxichuan's own `Task.sigact`, but glue's SIGPIPE (and `tkill`) read `Process.signal_actions`, so a Rust program (std ignores SIGPIPE at startup) writing into a closed pipe or socket would have been killed instead of getting `EPIPE`. `rt_sigaction`/`rt_sigprocmask`/`sigaltstack` are glue's now; `sigsuspend`'s SIGCHLD delivery (busybox ash's `wait`, the one signal this kernel delivers) and `rt_sigreturn` read and write the per-thread mask (`akuma_threading::{thread_signal_mask, set_thread_signal_mask}`) and the shared action table; exec resets caught handlers to the default on that table (`akuma-exec` had no exec reset); fork no longer copies signal state (`fork_process` clones the table and seeds the mask). The `Task` signal fields and the `akuma-syscalls-signal` dependency are gone. Battery 9/9 + goose; `trap "" PIPE` pipeline and ash `wait` OK |
| toolchain gate; fork depth ceiling removed | `c1c885c5` | 7,130 -> 7,124 | New gate on the Rust toolchain image (`rs.img`, its `/bin/sshd` swapped for the `linux-abi` build): `rustc hello.rs` links and runs; `cargo build -j2 --offline` of a crate with a build script failed at gcc's `collect2: posix_spawnp: Operation not permitted`. Cause: `fork_arm_real` still refused forks deeper than crossing depth 8, a ceiling from the M2 crossing that real forks (own thread, own stacks) do not need, and sshd -> session -> sh -> cargo -> rustc -> cc -> collect2 -> ld reaches it. Only the boot task (no registered process) is refused now, and the depth field is gone. cargo build: 31 s on TCG, runs the build script and the binary. Battery 9/9 + goose |
| prctl, set_tid_address | (this commit) | 7,124 -> 7,034 | Both glue's (its `prctl` is a superset: `PDEATHSIG`, `DUMPABLE`, `NO_NEW_PRIVS`, `PR_SET_VMA`; `set_tid_address` writes the same `Process.clear_child_tid` this kernel's thread exit reads). `my_tid`, `prctl_arm`, `child::current_pid` followed. **Deliberately still local:** the clock family (`clock_gettime`, `gettimeofday`, `nanosleep`) — glue reads `akuma_primitives::clock::uptime_us`, which `exec_twin` registers as the 10 ms LAPIC-tick clock, while these arms use the TSC clock (`net::uptime_us`); one clock (X2) needs the TSC calibrated before `exec_twin::init` registers it, or the clock jumps backwards under every scheduler deadline. And `tkill`/`tgkill` — glue pends a signal this kernel never delivers, where the local arm interrupts the target (EINTR), which cargo's jobserver relies on. Battery 9/9 + goose; `nettest-reqwest` OK |

## Background

- [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md): the experiment, the cutover
  entry, and the tokio-kernel idea (2026-10-30 entries).
- [`TAOXICHUAN_CRATE_EXTRACTION_PLAN.md`](TAOXICHUAN_CRATE_EXTRACTION_PLAN.md) (X1–X10) and
  [`../../taoxichuan/docs/reference/switch-to-crates.md`](../../taoxichuan/docs/reference/switch-to-crates.md) (S0–S7): the two plans
  phase 2 merges.
- [`TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md`](TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md): the deduplication slices already done.
- [`TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md`](TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md): phase 1a.
- [`TAOXICHUAN_J4_BKL_CONVOY.md`](TAOXICHUAN_J4_BKL_CONVOY.md): why `-j4` is not a gate.
