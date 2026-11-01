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
| 2a | (this commit) | 12,144 -> 12,169 | `akuma-vfs-glue` mount table created, its hooks installed, the ext2 root mounted at `/` as the same `Arc` `root_fs()` borrows. Nothing resolves through it yet. Image +32 KB (1,592,120 -> 1,624,112 bytes); battery 9/9 PASS, 0 faults |

## Background

- [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md): the experiment, the cutover
  entry, and the tokio-kernel idea (2026-10-30 entries).
- [`TAOXICHUAN_CRATE_EXTRACTION_PLAN.md`](TAOXICHUAN_CRATE_EXTRACTION_PLAN.md) (X1–X10) and
  [`../../taoxichuan/docs/reference/switch-to-crates.md`](../../taoxichuan/docs/reference/switch-to-crates.md) (S0–S7): the two plans
  phase 2 merges.
- [`TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md`](TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md): the deduplication slices already done.
- [`TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md`](TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md): phase 1a.
- [`TAOXICHUAN_J4_BKL_CONVOY.md`](TAOXICHUAN_J4_BKL_CONVOY.md): why `-j4` is not a gate.
