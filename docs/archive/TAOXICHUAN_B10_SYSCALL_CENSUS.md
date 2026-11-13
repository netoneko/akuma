# B10: the syscall census of unmodified tokio programs (2026-11-02)

**Stability: A**: a measurement, repeatable with the two scripts below.

B10 of [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md) (Phase 3 step 3a) asks which syscalls unmodified
tokio programs make, and whether Taoxichuan serves them. Until now the answer was a derivation from the sources
([`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md) §1, "about 75"). This census is the
measured version: **76 distinct calls, every one served on Taoxichuan**, and 28 calls on `akuma-runtime-tokio`'s
allow-list that none of the programs make.

## Method

The programs ran on Linux 6.17 (Pop on the ryzen) under `strace -f`, as the same static musl binaries the Taoxichuan
images carry (goose copied from the guest image):

| run | what it exercises |
|---|---|
| `tokioprobe` | the multi-thread runtime, timers, TCP, pipes, `tokio::process` |
| `reqwest-get`, `reqwest-post` | `nettest-reqwest`: hyper + rustls client, `GET https://akuma.sh/`, a 64-byte `POST` |
| `kot` | the axum server: bind, listen, accept, a TLS handshake from `curl` (refused, as the mesh wants client certs; the accept and handshake paths still run), its SQLite database |
| `goose-pong` | goose against GLM, no tools |
| `goose-tool` | goose with a shell tool call (`echo census-ok`, printed back) |

`scripts/kvm/taoxichuan/census_pop.sh` runs them on Pop and writes one `strace -f -c` summary per run, plus a full
`strace -f` trace for the two runs that spawn a program. `scripts/kvm/taoxichuan/census_classify.py OUT UNISTD_64_H`
classifies the union. Two rules decide what counts:

- **A spawned program's own calls do not count.** After a child's `execve`, its calls belong to that program (Pop's
  glibc `uname` in `tokioprobe`, `bash` in the goose tool call), which a tokio kernel has no reason to serve on tokio's
  account. The full trace's pids attribute each call.
- **The window between `clone` and `execve` counts.** That is Rust std's spawn path in the parent's image (`dup2`,
  `chdir`, `exit` on a failed exec), and the kernel must serve it.

The classifier reads the servers from the sources, not from a copy: the three groups of `akuma-runtime-tokio`'s
`served_by_glue`, the dispatch of `akuma-syscalls-glue-amd64` (the x86 fd family), `akuma_syscalls_abi::legacy`'s fold,
and the arms of Taoxichuan's own `dispatch_inner`. It exits 1 if any call is unserved.

## Result

| served by | calls | which |
|---|---:|---|
| `akuma-syscalls-glue-amd64` (fd family) | 31 | `read write open close stat fstat lstat poll lseek ioctl pread64 pwrite64 writev access dup2 socketpair fcntl flock fsync ftruncate getcwd chdir fchown getdents64 epoll_ctl openat unlinkat epoll_pwait eventfd2 epoll_create1 pipe2` |
| legacy fold to glue-amd64 | 5 | `rename mkdir unlink readlink chmod` |
| Taoxichuan's own arms | 12 | `mmap mprotect munmap brk sched_yield mremap madvise clone execve exit arch_prctl exit_group` |
| `akuma-runtime-tokio` `net` | 13 | `socket connect sendto recvfrom recvmsg shutdown bind listen getsockname getpeername setsockopt getsockopt accept4` |
| `akuma-runtime-tokio` `process` | 2 | `wait4 pidfd_open` |
| `akuma-runtime-tokio` `runtime` | 13 | `rt_sigaction rt_sigprocmask getpid uname geteuid sigaltstack prctl gettid futex sched_getaffinity set_tid_address clock_gettime getrandom` |
| **unserved** | **0** | |

Every run makes the 30-odd calls of a static musl tokio program starting up and polling (`mmap`, `futex`,
`epoll_pwait`, `eventfd2`, `sched_getaffinity`, `sigaltstack`, `prctl(PR_SET_NAME)` per worker, `getrandom`). Goose adds
the filesystem half (its config, session store and SQLite: `pread64`, `pwrite64`, `fsync`, `ftruncate`, `flock` from
`kot`'s database, `rename`, `unlinkat`) and is the only caller of `uname`, `getpid`, `geteuid`, `mremap` and `madvise`.

On the Taoxichuan side, the rig gates of B9's remainder (battery 11/11 with goose, `tokioprobe` at `smp=1` and `smp=4`,
the toolchain gate) leave one `[nosys]`: 40, `sendfile`. No census program makes it, nor `setitimer` (38); both come
from the shell and tools the battery runs, so they are not on tokio's account.

### Allow-listed, made by no census program

| group | calls |
|---|---|
| `runtime` (25 of 38) | `sethostname setdomainname getppid getuid getgid getegid getsid setsid getpgid setpgid membarrier sync syncfs sysinfo getrusage getgroups times clock_getres clock_settime adjtimex clock_adjtime nanosleep clock_nanosleep prlimit64 getrlimit` |
| `net` | `accept sendmsg` |
| `process` | `waitid` |

These are what a shell and its tools call: job control (`setsid`, `setpgid`, `getpgid`), `id`, `uptime`, `date -s`,
`ulimit`, `sleep` (tokio itself sleeps in `epoll_pwait`'s timeout, never in `nanosleep`). They were on the list because
the list was the set glue served for the B11 first cut (55 calls), not a set tokio needs. `waitid` and `accept`/`sendmsg`
are tokio APIs no census program reached (`Child::wait` uses the pidfd and `wait4`; tokio accepts with `accept4`).

## Conclusions for B11

- **`runtime` splits in two**: the 13 calls above stay unconditional, with `nanosleep` and `clock_nanosleep` (no census
  program made them, but `std::thread::sleep` does, from any blocking task); the other 23 go behind a `shell` feature,
  which Taoxichuan turns on (its images run busybox and bash) and a tokio-only kernel leaves off.
- **Checked on a kernel without it.** Taoxichuan built `--no-default-features` (no `shell`), on the Mac TCG rig with
  the goose image: `tokioprobe` (4 workers) PASS, `nettest-reqwest get` 200, goose PONG, and the goose tool call prints
  `census-ok`, 0 faults. Its `[nosys]` lines are all busybox's: `getuid` (102), `getgid` (104), `getppid` (110) from
  `sh` and its applets, `setsid` (112) from the `timeout` applet's child, and `setgid`/`setuid` (106/105), which
  busybox calls only because `getuid` failed. None comes from a tokio program.
- `waitid`, `accept` and `sendmsg` stay in their groups: they are tokio's API surface, only not exercised here.
- `sendfile` and `setitimer` belong to the tools feature if they are served at all; tokio does not need them.
- The derivation was close: it predicted about 75 calls. What it could not know is which of them goose and std's spawn
  path add (the fd family's file calls, `chdir`, `dup2`).

## Background

- [`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md): the static derivation this measures.
- [`TAOXICHUAN_B9_MADVISE_PIDFD_SIGSUSPEND.md`](TAOXICHUAN_B9_MADVISE_PIDFD_SIGSUSPEND.md): the gate whose `[nosys]` set is
  the Taoxichuan side.
