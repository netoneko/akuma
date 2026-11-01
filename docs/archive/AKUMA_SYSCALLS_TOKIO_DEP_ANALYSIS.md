# A tokio kernel: syscall surface, dependency weight, and how small it can get (2026-11-01)

**Summary.** Taoxichuan's runtime image today has an audit surface of **101,041 lines** (measured: `scripts/cloc_akuma.py
--audit taoxichuan`). That splits into 4,229 lines of Taoxichuan itself, 55,004 lines in 72 workspace crates and **41,808
lines in 23 third-party crates**. `akuma-syscalls-glue`'s own dependency closure accounts for 91,645 of those lines (91%).
The glue crate itself is only 11,411 audit lines (11%). The big costs are elsewhere: the third-party virtio stack
(virtio-drivers + zerocopy + safe-mmio + bitflags 2 + thiserror, 14,521 lines), smoltcp and its helpers (17,937 lines), and
process/thread/memory management (16,833 lines). A tokio program needs about 75 Linux syscalls. All but a few already
reach glue or a local arm in Taoxichuan; the gaps are `kill`, `madvise`, `clock_getres`, `waitid` and `pidfd_open`. The
leanness work that pays is mostly **not** splitting glue. It is: (1) compiling out code that no kernel uses, or that
Taoxichuan duplicates; (2) feature-gating glue's Akuma-native and terminal families; (3) replacing the virtio-drivers stack.
Estimated result: about **88,600** lines with the low/medium-effort steps (−12%), and about **75,000** with the virtio
replacement (−26%). Measuring also showed that dead-code elimination does **not** remove unreached glue arms from the
image. `handle_syscall` is one runtime `match`, so glue's `sys_spawn_ext`, `pty`, `mem::sys_mmap` and `sync::sys_futex` are
all linked into Taoxichuan even though it never routes those calls to glue. Only `#[cfg]` (features) removes code, from both
the image and the audit count.

Tree measured: HEAD `8cb7ac37` plus uncommitted edits in `taoxichuan/src/{child,main,net}.rs`. Taoxichuan was being edited
in parallel during this analysis, so its own ~4.2k figure moves by a few hundred lines. The crate figures do not.
Every cargo invocation used `CARGO_TARGET_DIR` in a scratch directory, so the repo's `target/` was not touched.

---

## 1. The tokio syscall surface

**Method.** No strace census exists yet (that is Phase 3 step 3a). This list comes from what musl 1.2.x static startup,
Rust std (`x86_64-unknown-linux-musl`), mio 1.x and tokio 1.x call. It is cross-checked against the repo's evidence:

- `userspace/nettest/rust/reqwest/Cargo.toml`: tokio features `rt, rt-multi-thread, macros, time, io-util, net`, plus
  reqwest 0.12 + hyper 1.x + rustls 0.23 + ring. Its comment says `full` was left out on purpose because it drags in
  `process`/`signal`.
- `MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`, 2026-10-30 (late): goose panicked with `Failed to build Tokio runtime`
  on `[nosys] nr=291` (`epoll_create1`), after a harmless `nr=157` (`prctl(PR_SET_NAME)`).
- `TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md`: mio's waker is an `eventfd` that is written on every wake and never read,
  so `EPOLLET` must re-report on every write. `sched_getaffinity` sets the worker count.
- `AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md`, `TOKIO_PIPE_EPOLL_HANG.md` and
  `docs/reference/subsystems/syscalls/poll.md` §"`epoll_on_fd_drained`": the edge re-arm rules for sockets and pipes.
  `tokio::process` output depends on the pipe `EPOLLHUP` edge.

Treat this as a derivation and confirm it with step 3a's `strace -f -c`. Rows marked *(verify)* are ones where I am not
certain of the library's exact behaviour.

Status key for the "Taoxichuan today" column:

- **G**: glue, through `served_by_glue` (`taoxichuan/src/main.rs:1828`).
- **GA**: glue through `akuma_syscalls_glue_amd64::dispatch` (`crates/akuma-syscalls-glue-amd64/src/lib.rs:73`).
- **L**: a local arm in `dispatch_inner` (`taoxichuan/src/main.rs:1886`).
- **ENOSYS**: no arm.

### 1a. Required by any Rust std + tokio (rt-multi-thread, net, time) binary

| syscall (x86_64 nr) | who calls it | Taoxichuan today |
|---|---|---|
| `arch_prctl` (158) | musl `__init_tp` (ARCH_SET_FS) | L |
| `set_tid_address` (218) | musl `__init_tp` | G |
| `poll` (7) | std `sanitize_standard_fds` (fds 0–2); musl `res_msend` (DNS) | GA (`poll`) |
| `rt_sigaction` (13) | std: SIGPIPE→SIG_IGN; SIGSEGV/SIGBUS guard handlers | G |
| `sigaltstack` (131) | std stack-overflow handler, main thread + every spawned thread | G |
| `rt_sigprocmask` (14) | musl `pthread_create` blocks signals around `clone` | G |
| `mmap`/`munmap`/`mprotect` (9/11/10) | musl malloc; thread stacks + guard page; sigaltstack + guard | L |
| `mremap` (25) | musl `pthread_getattr_np` main-thread probe (std main-thread guard) | L |
| `brk` (12) | musl malloc | L |
| `madvise` (28) | musl mallocng `MADV_FREE` (ignores errors) *(verify)* | **ENOSYS** (harmless) |
| `clone` (56) | `std::thread::spawn` → musl `pthread_create` (CLONE_VM\|THREAD\|SETTLS\|…) | L (`child::fork_arm`) |
| `exit` (60), `exit_group` (231) | thread exit (musl `__unmapself`), process exit | L |
| `futex` (202) | std Mutex/Condvar/park (`FUTEX_WAIT_BITSET` absolute CLOCK_MONOTONIC, `FUTEX_WAKE`), musl thread join | L (`taoxichuan/src/futex.rs`) |
| `sched_getaffinity` (204) | `available_parallelism` → tokio worker count | G |
| `sched_yield` (24) | `thread::yield_now` in spin/backoff paths | L |
| `prctl` (157) | std thread naming (`PR_SET_NAME`) | G |
| `getrandom` (318) | std `RandomState`; ring/rustls | G |
| `clock_gettime` (228) | `Instant`/`SystemTime` (no vDSO on Akuma) | L |
| `nanosleep`/`clock_nanosleep` (35/230) | `thread::sleep` | L |
| `epoll_create1` (291) | mio `Poll::new` | GA |
| `epoll_ctl` (233) | mio register/reregister/deregister (`EPOLLET\|IN\|OUT\|RDHUP`) | GA |
| `epoll_pwait` (281) / `epoll_wait` (232) | mio `Poll::poll` (musl calls `epoll_pwait`, falls back to 232) | GA |
| `eventfd2` (290) | mio `Waker` | GA |
| `read`/`write` (0/1) | eventfd waker; stdio; std `TcpStream` reads | GA |
| `readv`/`writev` (19/20) | hyper/tokio vectored writes | GA |
| `close` (3), `fcntl` (72) | everywhere; `F_GETFL`/`F_SETFL`/`F_DUPFD_CLOEXEC` | GA |
| `ioctl` (16) | `isatty` (TCGETS) via `IsTerminal`; FIONBIO from std `set_nonblocking` | GA (`term::sys_ioctl`) |
| `socket` (41) | mio (`SOCK_NONBLOCK\|SOCK_CLOEXEC`) | G |
| `connect` (42), `getsockopt` (55) | non-blocking connect + `SO_ERROR` | G |
| `bind`/`listen`/`accept4` (49/50/288) | server side (`TcpListener`) | G |
| `setsockopt` (54) | `TCP_NODELAY`, `SO_REUSEADDR`, keepalive | G |
| `sendto`/`recvfrom` (44/45) | musl `send`/`recv` (std `TcpStream`), UDP, DNS | G |
| `sendmsg`/`recvmsg` (46/47) | UDP / ancillary paths | G |
| `shutdown` (48), `getsockname`/`getpeername` (51/52) | hyper, connection pool | G |
| `getpid`/`gettid` (39/186) | std/musl internals | G |
| `uname` (63) | rustls/ring CPU features, some crates *(verify)* | G |
| `prlimit64`/`getrlimit` (302/97) | std main-thread stack size | L |
| `rt_sigreturn` (15) | any delivered signal (stack-overflow handler, `signal` feature) | L |

DNS (`tokio::net::lookup_host`, and reqwest's default resolver = musl `getaddrinfo` on the blocking pool) adds:
`open`/`openat` + `read` + `close` of `/etc/hosts` and `/etc/resolv.conf`; UDP `socket`/`sendto`/`poll`/`recvfrom`; and,
only when the answer has IPv6 results, the RFC 3484 sort (`socket(AF_INET6, SOCK_DGRAM)` + `connect` + `getsockname`).
All of these are served today (GA/G).

### 1b. Optional, by tokio feature

| tokio feature | adds | Taoxichuan today |
|---|---|---|
| `fs` (std::fs on the blocking pool) | `open`(2)/`openat`(257), `newfstatat`(262)/`fstat`(5)/`stat`(4)/`lstat`(6), `statx`(332; ENOSYS falls back), `lseek`(8), `pread64`/`pwrite64`(17/18), `getdents64`(217), `mkdir(at)`, `unlink(at)`/`rmdir`, `rename(at)`(82/264), `readlink(at)`, `symlink(at)`, `link(at)`, `fsync`/`fdatasync`(74/75), `ftruncate`(77), `chmod`/`fchmod`, `utimensat`(280), `getcwd`(79), `chdir`(80), `access`(21), `copy_file_range`/`sendfile` (ENOSYS falls back to read/write) | GA, with legacy numbers folded by `akuma_syscalls_abi::legacy::fold`; `copy_file_range`/`sendfile` ENOSYS (works) |
| `io-std` | `read`/`write` on fds 0–2 from the blocking pool | GA (+ Taoxichuan's `console_stdio_write`) |
| `signal` | `rt_sigaction` (SA_SIGINFO\|SA_RESTART\|SA_ONSTACK), `socketpair(AF_UNIX)` (mio `UnixStream::pair`), real signal delivery + `rt_sigreturn` | G/GA; Taoxichuan delivers only SIGCHLD to `sigsuspend` today, so **signal delivery is the gap**, not the syscalls |
| `process` | `pipe2`(293), `clone(CLONE_VM\|CLONE_VFORK\|SIGCHLD)` + `execve`(59) (musl `posix_spawn`), `dup2`/`dup3`(33/292), `wait4`(61) with `WNOHANG`, **`kill`(62)**, SIGCHLD (needs `signal`); tokio ≥1.37's pidfd reaper tries `pidfd_open`(434) first and falls back on error *(verify)* | pipe2/dup GA; clone/execve/wait4 L; **`kill` (62) ENOSYS** (`Child::kill` fails); `pidfd_open` ENOSYS (fallback) |
| `net` + Unix sockets | `socket(AF_UNIX)`, `bind`/`connect` on a path | G (glue `unixsock.rs`) |
| `time` | nothing beyond `clock_gettime` + the epoll timeout | — |

**Not needed** by a static-musl tokio binary: `rseq`/`set_robust_list` (glibc only), `clone3` (glibc), `io_uring_*`
(tokio-uring only), `timerfd_*` (mio does not use it), `inotify`, SysV IPC, aio, `mount`, ptys, and every Akuma-native
number (`SPAWN`, `SPAWN_EXT`, `SET_BOX_STACK`, terminal/cursor calls, `RESOLVE_HOST`, `GET_CPU_STATS`).

**Gaps found in Taoxichuan for a full-feature tokio program:** `kill` (62), `waitid` (247), `clock_getres` (229) and
`madvise` (28) have no arm (glue implements all four, at `crates/akuma-syscalls-glue/src/lib.rs:884/874/1058/912`).
`pidfd_open` is compiled out (`sc-pidfd` off). Signal delivery beyond SIGCHLD-to-`sigsuspend` is missing.

## 2. Where each syscall lives today

Each row gives the arm in `handle_syscall` (`crates/akuma-syscalls-glue/src/lib.rs`, line of the arm), the body, and the
crates under it. The "crates under it" column comes from `grep -oh 'akuma_[a-z_]*' <module>.rs | sort | uniq -c`.

| syscalls | glue arm → body | crates under it |
|---|---|---|
| read, write, readv, writev, pread64, pwrite64 | :743–746, :921–922 → `fs::sys_read`/`sys_write`/… | `akuma-exec` (fd table, `SharedFdTable` over `akuma-fdtable`), `akuma-vfs-glue`, `akuma-vfs`, `akuma-ext2`; fans out to `pipe`, `pty`, `net`, `eventfd`, `unixsock` by fd kind |
| openat, close, lseek, fstat, newfstatat, statx, getdents64, fcntl, dup, dup3, mkdirat, unlinkat, renameat(2), readlinkat, getcwd, chdir, fsync, ftruncate, utimensat, faccessat | :754–757, :813, :816–823, :748, :751, :864–867, :895, :903, :893 → `fs::*` | same as above; `akuma-isolation` (DAC via `perm.rs`) |
| ioctl | :747 → `term::sys_ioctl` (280 code lines) | `akuma-tty`, `akuma-exec` |
| pipe2 | :752 → `pipe::sys_pipe2` | `akuma-pipes` → `akuma-pipe` |
| socket, bind, listen, accept(4), connect, sendto, recvfrom, sendmsg, recvmsg, get/setsockopt, shutdown, getsockname, getpeername | :774–797 → `net::dispatch_*` | `akuma-net` (smoltcp socket table, `socket.rs` 1,371 audit lines), `akuma-net-nic`, `akuma-syscalls-net`; AF_UNIX → `unixsock.rs` → `akuma-net-unix` |
| socketpair | :775 → `net::sys_socketpair` | `akuma-net-unix`, `akuma-pipes` |
| ppoll, pselect6 (and x86 `poll`/`select` via glue-amd64) | :814–815 → `poll::sys_ppoll`/`sys_pselect6` | `akuma-syscalls-poll` (readiness map, wire), `akuma-net-yarn` (the wait loop) |
| epoll_create1, epoll_ctl, epoll_pwait | :1060–1064 (`sc-epoll`) → `poll::sys_epoll_*` | `akuma-syscalls-poll` (interest list, `EPOLLET` armed state), `akuma-net-yarn` |
| eventfd2 | :920 (`sc-eventfd`) → `eventfd::sys_eventfd2` | `akuma-syscalls-eventfd` |
| futex | :914 → `sync::sys_futex` | `akuma-syscalls-sync` (op decode, waiter table, deadlines) |
| mmap, munmap, mprotect, mremap, madvise, brk, msync, membarrier | :799–800, :913, :798, :912, :753, :940, :936 → `mem::*` | `akuma-syscalls-mem`, `akuma-mmap`, `akuma-mmu`, `akuma-pmm`, `akuma-exec` |
| clone, clone3, execve, wait4, waitid, exit, exit_group, kill | :801–803, :873–874, :738, :854, :825/:884 → `proc::*` | `akuma-exec` (process table, `children.rs`, `spawn.rs`), `akuma-threading`, `akuma-elf` (through `akuma_exec::elf_loader`) |
| rt_sigaction, rt_sigprocmask, sigaltstack, rt_sigsuspend, tkill, tgkill | :863, :859, :916, :860, :1035–1036 → `signal::*` | `akuma-syscalls-signal` (mask algebra), `akuma-exec` `process/signal.rs`, `akuma-threading` `sigframe.rs` |
| clock_gettime, clock_getres, nanosleep, clock_nanosleep, times, getrusage | :868, :1058, :758–759, :938–939 → `time::*` (= `akuma-syscalls-time`, aliased at lib.rs:164) | `akuma-syscalls-time`, `akuma-timer`, `akuma-sntp` |
| getpid, gettid, uname, prctl, set_tid_address, prlimit64, getrandom, sched_getaffinity, sched_yield | :876, :880, :888, :937, :852, :918, :829, :998, :981 → `proc::*` / inline | `akuma-exec`, `akuma-threading` |
| getuid/getgid & co. | :878–883 → `cred::*` | `akuma-isolation` (creds) |
| pidfd_open | :1038 (`sc-pidfd`, off in Taoxichuan) → `pidfd::*` | `akuma-exec` |
| excursion (every call) | `handle_syscall` prologue/epilogue | `akuma-syscalls` (shape), `akuma-bkl`, `akuma-syscalls-log`, `utils/read_profile.rs` |

The x86_64 ABI layer for the fd family is `akuma-syscalls-glue-amd64` (285 audit lines). It handles open-flag permutation,
the `struct stat` re-layout, the legacy numbers 2/4/6/7/21/22/23/33/85/213/232, and chunked reads.

## 3. Dependency tree and line weight

### 3a. Commands and totals (measured)

| measurement | command | result |
|---|---|---|
| Taoxichuan runtime image audit surface | `CARGO_TARGET_DIR=<scratch> python3 scripts/cloc_akuma.py --audit taoxichuan --json` (runs `cargo check --release -p taoxichuan --target x86_64-unknown-none --features no-tests`, reads rustc dep-info, evaluates `#[cfg]`) | **101,041** = kernel 4,229 + workspace 55,004 (72 crates) + third-party 41,808 (23 crates). Build-time (proc-macros, build scripts) is a separate 31,871 lines in 27 units and is not counted |
| glue's own closure (Taoxichuan's glue features + `no-tests`) | same method, `-p akuma-syscalls-glue --no-default-features --features no-tests,smoltcp,sc-epoll,sc-eventfd` (scratch helper calling `cloc_akuma.audit_units` + `count_text`) | 91,645 = glue 11,046 + workspace 38,791 + third-party 41,808 |
| crates Taoxichuan adds beyond glue's closure | difference of the two | 9,031 (taoxichuan 4,229; boot-amd64 700; ring3-amd64 669; ryzen-amd64 570; vm-spec 533; abi 334; trap-amd64 307; sched-amd64 305; glue-amd64 285; lapic 248; port 230; and 7 smaller) |
| package count | `cargo tree -p taoxichuan --target x86_64-unknown-none -e normal --prefix none` | 103 unique lines (incl. proc-macros); glue alone 85 |

Every third-party crate in Taoxichuan reaches it through glue's closure. The third-party set is identical with and without
Taoxichuan's own direct dependencies.

### 3b. Workspace crates by purpose (measured audit lines)

| group | lines | largest members |
|---|---|---|
| syscall layer | 15,875 | glue 11,411 · syscalls-linux 1,204 · syscalls 476 · syscalls-poll 452 · syscalls-sync 444 · abi 334 · glue-amd64 285 · syscalls-time 277 · syscalls-mem 218 · syscalls-net 200 |
| process / threads / memory | 16,833 | exec 5,805 · threading 3,146 · mmu 1,805 · pmm 1,363 · elf 1,033 · alloc 620 · mmap 568 · exec-core 559 · user-access 357 · fpcache 243 · fdtable 239 |
| filesystems | 8,290 | ext2 2,785 · vfs-glue 2,261 · isolation 1,145 · vfs 1,086 · procfs 653 |
| net + IPC | 4,873 | net 2,450 · net-nic 1,014 · net-unix 710 · pipes 222 · net-yarn 213 · dns 182 |
| tty | 1,189 | tty 1,189 |
| machine / boot / infra | 7,944 | primitives 1,133 · virtio 815 · bkl 719 · boot-amd64 700 · ring3-amd64 669 · ryzen-amd64 570 · vm-spec 533 · cpu 438 · dma 322 |
| **total** | **55,004** | |

Inside glue (measured audit lines, Taoxichuan build): fs.rs 2,492 · net.rs 1,548 · proc.rs 1,237 · poll.rs 1,025 ·
mem.rs 917 · unixsock.rs 794 · lib.rs 786 · pty.rs 550 · sync.rs 523 · term.rs 452 · signal.rs 253 · pipe.rs 214 · eventfd.rs
115 · cred.rs 114 · recordlock.rs 98 · perm.rs 97 · flock.rs 83 · utils/read_profile.rs 83 · version.rs 28.

### 3c. Third-party crates (measured audit lines; registry sources under `~/.cargo/registry/src`)

| purpose | crates (version: audit lines) | total |
|---|---|---|
| TCP/IP | smoltcp 0.12.0: 11,897 · heapless 0.8.0: 3,872 · managed 0.8.0: 777 · byteorder 1.5.0: 698 · bitflags 1.3.2: 394 · hash32 0.3.1: 248 · cfg-if 1.0.4: 41 · stable_deref_trait 1.2.1: 10 | 17,937 |
| virtio device layer | zerocopy 0.8.56: 8,334 · virtio-drivers 0.13.0: 3,550 · bitflags 2.13.1: 1,752 · safe-mmio 0.3.1: 746 · thiserror 2.0.18: 139 | 14,521 |
| ELF parsing | elf 0.7.4: 3,585 (via `akuma-elf` ← `akuma-exec`) | 3,585 |
| locks | lock_api 0.4.14: 2,061 · spinning_top 0.3.0: 278 · scopeguard 1.2.0: 117 | 2,456 |
| other | talc 4.4.3: 1,126 · fdt 0.1.5: 914 (an AArch64 DTB parser, via `akuma-fdt` ← `akuma-mmu`) · log 0.4.29: 799 · embedded-io(-async) 0.6.1: 410 · arm_pl031 0.2.1: 60 (an ARM RTC, via `akuma-timer`) | 3,309 |
| **total** | | **41,808** |

Two of these are AArch64 hardware crates compiled into an x86_64 kernel: `fdt` and `arm_pl031`. smoltcp's `socket-dns`
feature is on in four manifests (`akuma-net`, `akuma-net-nic`, `akuma-syscalls-glue`, `akuma-kernel-core`), but no code in
`crates/`, `src/`, `amd64/src` or `taoxichuan/src` names `smoltcp::socket::dns`. DNS goes through `akuma-dns` over UDP
sockets (`crates/akuma-net/src/dns.rs:6`). Its files are 831 live lines: `socket/dns.rs` 495 + `wire/dns.rs` 336.

## 4. Leanness options

### 4a. Existing cargo features

| crate | features (relevant) | state in Taoxichuan | what a tokio kernel can turn off |
|---|---|---|---|
| akuma-syscalls-glue | `smoltcp`, `sc-epoll`, `sc-eventfd`, `sc-timerfd`, `sc-pidfd`, `sc-aio`, `sc-sysv-ipc`, `sc-containers`, `sc-reboot`, `rump`, `read-profile`, `no-tests`, `smp-shared`, `no-bkl-*`, `extreme` | on: `smoltcp, sc-epoll, sc-eventfd` | **nothing more.** The gated modules (aio 126, container 258, timerfd 146, pidfd 79, reboot 27, msgqueue 7 = 643 code lines, by `cloc_akuma.py crates/akuma-syscalls-glue/src --by-file`) are already out. `sc-pidfd` is a candidate to turn **on** for `process` |
| akuma-net | `smoltcp` (default), `rump`, `rtl8169`, `small-sockets`, `net-noalloc`, `many-sessions`, … | default | nothing; TCP is needed |
| akuma-vfs-glue | `sc-containers`, `sc-sysv-ipc`, `sc-reboot`, `extreme`, `smp-shared` | none | nothing (procfs is not gated) |
| akuma-exec | `no-tests`, `extreme`, `smp-shared`, `debug-info`, `leak-instr`, `no-bkl-*` | none | `no-tests` is **not forwarded** |
| akuma-ext2 | `fs-cache`, `pmm-instr`, `extreme`, `no-bkl-vfs` | none | nothing |

The one free win from existing features is forwarding `no-tests`. Taoxichuan's `no-tests = []` does not enable
`akuma-syscalls-glue/no-tests` or `akuma-exec/no-tests`, so glue's `#[cfg(kernel_tests)]` boot tests are compiled into the
image. **Measured:** `--features no-tests,akuma-syscalls-glue/no-tests,akuma-exec/no-tests` takes the audit from 101,041 to
**100,525 (−516)**: glue −365, exec −36, taoxichuan −115.

### 4b. Glue modules a tokio kernel never reaches (new features)

| candidate feature | contents (code lines; function spans from a rough brace counter = estimate) | saving |
|---|---|---|
| `pty` | `pty.rs` 550 (measured audit) | 550 |
| `akuma-native` | Akuma-only numbers: `term.rs` terminal/cursor/input/cpu-stats calls ≈173; `proc.rs` `sys_spawn`/`spawn_ext`/`spawn_creds`/`set_box_stack`/`close_child_stdin`/`waitpid`/`core_init` ≈186; `net::sys_resolve_host` ≈19 | ≈380 (est.) |
| `flock` | `flock.rs` 83 + `recordlock.rs` 98 (measured) | 181 |
| `read-profile` stubs | `utils/read_profile.rs` 83 live without the feature (measured) | ≈80 (est.) |
| `unix` (only if tokio `signal`/`process`/`UnixStream` are unused) | `unixsock.rs` 794 + `akuma-net-unix` 710 (measured) | 1,504 |
| `procfs` in akuma-vfs-glue | `akuma-vfs-glue/src/proc.rs` 1,123 + `akuma-procfs` 653 (measured). Neither tokio nor std needs `/proc`: std's cgroup quota probe tolerates its absence, and musl `realpath` ≥1.2.4 does not read `/proc` | ≈1,776 |
| `containers` in akuma-isolation | `overlay_fs.rs` 450 + `subdir_fs.rs` 177 (measured) | ≈600 (est., some types stay) |
| `akuma-tty` out of `Process` | `akuma-tty` 1,189. Harder: `akuma-exec` names it in 5 files (`process/{mod,channel,signal,children,image}.rs`) | 1,189 (medium effort) |

The `pty`, `akuma-native` and `flock` rows total about 1,200 lines. With `procfs` and `containers` it is about 3,500.

**Duplicated implementations (Taoxichuan local vs glue).** Taoxichuan serves memory, process lifecycle, futex and clocks
locally, but glue's versions are compiled and linked anyway:

- `mem.rs` 917 (only `membarrier_cmd` is reached).
- `sync.rs` 523 (unreached; Taoxichuan's own is `taoxichuan/src/futex.rs`).
- `proc.rs`'s `sys_clone`/`clone_pidfd`/`clone3`/`execve`/`do_execve`/`exec_shebang`/`wait4`/`waitid`, about 550 (estimate).

That is about 2,000 audit lines of glue that Taoxichuan never calls, beside Taoxichuan's own versions in `child.rs`,
`futex.rs` and the `main.rs` memory arms. Finishing phase 2 in either direction removes about 2,000 lines. The plan's
direction is glue.

**Other compiled-but-foreign code.**

- `akuma-fdt` 67 + `fdt` 914 + `arm_pl031` 60 = 1,041 measured lines of AArch64 hardware support. Fix: target-specific
  dependencies plus a `cfg` on `akuma_mmu::with_boot_identity_fdt` (`crates/akuma-mmu/src/lib.rs:322`).
- `akuma-vm-spec` (533) is compiled whole for one call, `affinity::can_run`
  (`crates/akuma-sched-amd64/src/platform.rs:195`).
- smoltcp `socket-dns` (831, unused, §3c).
- smoltcp `socket-dhcpv4` (`socket/dhcpv4.rs` 536 + `wire/dhcpv4.rs` 657 = 1,193 measured). DHCP **is** used
  (`taoxichuan/src/net.rs:354`). It can go only for a static-address composition, such as a kvm0/Firecracker guest given its
  IP on the command line.
- Two ELF loaders. `akuma-exec` re-exports `akuma-elf` (`crates/akuma-exec/src/lib.rs:123`) = 1,033 + `elf` 3,585, while
  Taoxichuan loads binaries with its own parser (`akuma-ring3-amd64` + `akuma-elf-exec`). Keeping one loader saves about
  4,600 (estimate), but only if glue's `execve` path is gated with it.

### 4c. Splitting the big crates

**akuma-syscalls-glue.** The modules are not independent. Cross-module references (`grep -ohE '(crate|super)::<mod>'`
per file; counts are references, and `use crate::x` + bare `x::` uses are not counted, so these are lower bounds):

| module | refers to |
|---|---|
| fs | pty 21, perm 21, pipe 20, poll 13, net 11, utils 9, eventfd 7, unixsock 3, cred 2, timerfd/recordlock/pidfd/mem/flock 1 each |
| net | unixsock 30, fs 13, pipe 11, poll 8 |
| poll | pipe 14, net 6, timerfd 2, pty 2, pidfd 2, eventfd 2, unixsock 1 |
| proc | pipe 4, fs 4, pty 3, pidfd 3, version 2, mem 2, log 2, and 6 more |
| unixsock | pipe 23, net 8, poll 2, perm 2 |
| term | net 11, pty 2, timerfd/pipe/eventfd 1 |
| pty | poll 5, net 3, pipe 2, proc 1 |
| leaves | aio, cred, eventfd, log, msgqueue, perm, reboot, timerfd, version (no outgoing refs); sync → mem 1; signal → proc 2; mem → fs 3 |

The cause is structural. `FileDescriptor` (`crates/akuma-exec-core/src/process.rs:179`) is a closed enum with 29 variants,
including `DevDsp`, `DevFb`, `DevWifi`, `DevKvm`, `DevMouse`, `DevTun`, `Tap`, `RumpSocket`, `PtyMaster`/`PtySlave`,
`TimerFd`, `PidFd` and `BlockDev`. `read`, `write`, `close`, `fstat`, `fcntl` and the readiness probe each `match` over
all of it, so `fs.rs` and `poll.rs` depend on every family that owns an fd kind. A per-family split (`-fs`, `-net`,
`-poll`, `-proc`, `-signal`, `-mem`, `-time`) first needs a per-kind operations seam: a registration table of
read/write/poll/close per kind, on the `SyscallHooks` function-pointer model glue already uses for rump, mouse, tun and
console (`lib.rs`, `struct SyscallHooks`). Effort: high. Risk: the readiness/edge rules in `poll.md` are per fd kind
("the hook belongs to the fd type, not to the syscall"), so moving kinds behind an interface is where the
`EPOLLET` regressions of `TOKIO_PIPE_EPOLL_HANG.md` would come back. **Lines saved by the split itself: about zero.** The
savings come from not compiling a family, and a feature on one crate does that just as well. The split pays only if
per-crate review isolation or `forbid` boundaries are wanted.

**akuma-exec** (5,805 audit lines). Threading (`akuma-threading`), ELF parsing (`akuma-elf`) and core types
(`akuma-exec-core`) are already separate crates. What remains is the process table and lifecycle, and it is tightly knit:
`children`→`table` 13 refs, `signal`→`types` 6, `spawn`→`table` 4, `table`→`reclaim` 3, and `process/mod.rs` alone is
1,729. Separable pieces: the `akuma_elf` re-export + `spawn.rs` (303) + `image.rs` (269) behind an `elf-loader` feature;
`channel.rs` (310, the Akuma-native stdio channel) once Taoxichuan stops keying console output on it; the
`akuma-tty` field. Probably about 600–900 lines, plus the 4.6k of `akuma-elf` + `elf` it unlocks.

**akuma-vfs-glue** (2,261). The cleanest seam in the tree. `proc.rs` (1,123) is one module, referenced 2× from `lib.rs`
and 1× from `fs.rs`. A `procfs` feature is low effort. `lib.rs` (939) is the mount table, with containers already behind
`sc-containers`.

### 4d. Image bytes vs auditable source lines

**Measured.** I built `cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests` into a scratch
target dir. The result is 2,340,368 bytes unstripped. `llvm-nm -C -S --size-sort` shows 434 `akuma_syscalls_glue` symbols,
including:

- `proc::sys_spawn_ext`, `proc::sys_set_box_stack` and `term::sys_clear_screen` (Akuma-native, never called by a Linux
  program);
- 16 `pty::` symbols;
- `mem::sys_mmap`, `mem::sys_mremap`, `sync::sys_futex`, `proc::sys_clone`, `proc::do_execve`, `proc::sys_wait4` (served
  locally by Taoxichuan, so never reached through glue).

Text bytes by glue module (lower bounds: generic instantiations are attributed to `alloc`/`core`): fs 68 KB · proc 33 KB ·
unixsock 33 KB · pty 30 KB · net 27 KB · mem 27 KB · poll 23 KB · sync 13 KB · term 11 KB. Glue total ≈304 KB of 1.40 MB of
text symbols.

So the assumption that DCE removes unreached glue code from the image is wrong for this design. `served_by_glue` and
`glue_amd64::dispatch` filter at **run time**, then call `handle_syscall(nr, args)` with a dynamic `nr`. Every arm of its
`match` is therefore live code, and so is everything those arms call. DCE only drops functions that no live code
references.

Recommendation: track **`scripts/cloc_akuma.py --audit` lines** as the plan's primary metric, because it is what a reviewer
must read and it already evaluates features. Report image bytes beside it as a secondary metric. Both move together only
when code is removed by `#[cfg]`, and with this dispatcher that is the only lever. A compile-time allow-list (each
`handle_syscall` arm cfg'd on the family feature that owns it, as `sc-epoll` already does) makes an unlisted call have no
code behind it in both metrics. That is the agent log's "syscall table as a compile-time allow-list".

## 5. Recommended shape

**One crate with features**, `akuma-runtime-tokio` (the plan's 3b default). Do not use a composition template. A template
copies the wiring but cannot remove a line from its dependencies; only features passed down to glue, vfs-glue, exec and net
change the audit count.

- **What it owns.** The census allow-list (a table of `akuma_syscalls_abi::Syscall` rows) and the per-family setup:
  installing glue's `SyscallHooks`, the vfs-glue mount hooks, and the RNG hook that Taoxichuan's `main.rs` does by hand
  today. No arch code; the x86_64 ABI stays in `akuma-syscalls-glue-amd64`.
- **Its features mirror tokio's:** `net` (default), `time` (default, no extra code), `fs`, `io-std`, `signal` (→ glue
  `unix`), `process` (→ `signal`, `sc-pidfd`, pipes, the `kill`/`waitid` arms).
- **Dependencies:** `akuma-syscalls-glue` with `default-features = false, features = ["smoltcp", "sc-epoll", "sc-eventfd",
  "no-tests"]`, plus new opt-outs, or opt-ins if glue's defaults are inverted: `pty`, `akuma-native`, `flock`, `unix`
  off. Also `akuma-syscalls-glue-amd64`, `akuma-vfs-glue` with `procfs` off, and `akuma-exec` with `no-tests` and,
  later, `elf-loader` off.
- **What it requires of the kernel** (stays in the composition root): memory, clone/exec/wait, futex and clocks. Phase 2
  should move these to glue so that one implementation exists. Signal delivery on the x86 frame with a full register/FP
  restore is a prerequisite for `signal` and `process`.

### If we do X, auditable lines go from A to B

Baseline: 101,041 (measured). Rows are cumulative. "est." = estimate from measured per-file or per-crate audit lines of
the code that would leave. The real change will be a little smaller, because each gate leaves a stub or an `ENOSYS` arm.

| # | change | effort / risk | lines | from → to |
|---|---|---|---|---|
| 1 | Forward `no-tests` to glue and exec | trivial / none | −516 (**measured**) | 101,041 → 100,525 |
| 2 | Drop smoltcp `socket-dns` in the 4 manifests (unused) | trivial / low (all kernels) | −831 est. | → 99,694 |
| 3 | Target-gate `akuma-fdt`/`fdt` (akuma-mmu) and `arm_pl031` (akuma-timer) to aarch64 | low / low | −1,041 est. | → 98,653 |
| 4 | Move `affinity::can_run` out of `akuma-vm-spec` | low / low | −450 est. | → 98,203 |
| 5 | `procfs` feature in akuma-vfs-glue, off | low–medium / low | −1,776 est. | → 96,427 |
| 6 | Glue `pty`, `akuma-native`, `flock` features off; isolation `containers` off | medium / low | −1,800 est. | → 94,627 |
| 7 | One implementation per syscall (finish phase 2: mem, futex, clone/exec/wait, clocks via glue; delete the local copies) | medium–high / medium (the X2 clock note in the cleanup plan) | −2,000 est. | → 92,627 |
| 8 | One ELF loader (gate `akuma-exec`'s `akuma-elf` re-export + glue's `execve` path, or switch Taoxichuan to `akuma-elf` and drop its parser; the first removes more) | medium / medium | −4,000 to −4,600 est. | → ~88,600 |
| 9 | `unix` off (programs without tokio `signal`/`process`/`UnixStream`) | medium / low | −1,504 est. | → ~87,100 |
| 10 | Replace virtio-drivers with in-tree split-ring blk + net drivers (precedents: `crates/akuma-virtio/src/rng.rs`, `crates/akuma-net-nic/src/virtio_rings.rs`) | high / medium (DMA `unsafe` moves in-tree, budget grows) | −14,521 third-party, +~1,200 in-tree = −13,300 est. | → ~73,800 |
| 11 | No DHCP (static-address guests) | low / low for that composition | −1,193 est. | → ~72,600 |

**Measured 2026-11-01 (rows 2-4 done, `TAOXICHUAN_CLEANUP_PLAN.md` status table):** 100,530 -> **97,853** (-2,677 against
-2,322 estimated). Row 2 was five manifests, not four (the root package's too): -983. Row 3 took `akuma-timer` out of the
image as well, because its two other users never needed it (`akuma-vfs-glue` named it unused; glue only under `read-profile`):
-1,208. Row 4 became a new crate, `akuma-affinity`: -486.

Headline: about **88,600** with steps 1–8, keeping `process` + `signal` (−12%). About **75,000** if step 10 is also done
(−26%). The floor of this design, everything above, is about **72,600**. What remains at the floor (estimates, from §3b/§3c minus the rows above):

- smoltcp TCP/UDP/IPv4 with its helpers, about 15.9k;
- process/threads/memory, about 15.8k;
- the syscall layer (glue's fs/net/poll/proc/signal bodies and the family crates), about 12k;
- machine/boot/infra including the in-tree virtio drivers, about 8.7k;
- ext2 + VFS + isolation, about 5.9k;
- locks/allocator/log/embedded-io, about 4.8k;
- net/IPC glue crates, about 3.4k;
- Taoxichuan itself, about 3k;
- `akuma-tty`, 1.2k (until it leaves `Process`).

Steps 1–4 are worth doing regardless of the tokio kernel: they shrink every amd64 composition and change no behaviour.

**Not recommended as a first step:** splitting `akuma-syscalls-glue` into per-family crates (§4c). It costs a new fd-kind
interface across the code where the epoll edge bugs lived, and on its own it saves no lines.

---

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): "Phase 3: a tokio kernel" (steps 3a–3e) and the phase-2
  status table this analysis measures against.
- [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md): 2026-10-30 entries
  "the user's conclusions … next frontier" (`akuma-syscalls-tokio`), "pushing verification, composability and
  compatibility further" (compile-time allow-list), "kernels preconfigured for a language runtime", and "goose + GLM on
  Taoxichuan - blocked: no epoll".
- [`TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md`](TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md),
  [`AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md`](AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md),
  [`AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md`](AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md),
  [`TOKIO_PIPE_EPOLL_HANG.md`](TOKIO_PIPE_EPOLL_HANG.md): the tokio-specific kernel behaviour.
- [`AUDIT_SURFACE_METHODOLOGY.md`](AUDIT_SURFACE_METHODOLOGY.md): what `--audit` counts and why.
