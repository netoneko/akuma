# Syscalls — the crossing and the table

**Stability: C** (the table grows every ladder stage; this file is regenerated
from `src/main.rs::dispatch` and must not drift — a new arm adds its row in
the same commit).

## The crossing

```mermaid
sequenceDiagram
    participant U as task (ring 3)
    participant S as syscall stub (akuma-ring3-amd64)
    participant D as dispatch (main.rs)
    participant C as the crate that decides
    U->>S: syscall (rax=nr, args; CPU masks IF)
    Note over S: switch to the syscall stack,<br/>stash user rsp, build SyscallFrame
    S->>D: dispatch(&mut frame)
    D->>C: the arm asks its crate<br/>(fdtable / pipes / net / syscalls-* / ext2 …)
    C-->>D: decision
    D-->>S: u64 answer (errno = negated in rax)
    S-->>U: sysretq (rflags 0x202: IF set)
```

Canonical mermaid source: [`../diagrams/syscall-path.md`](../diagrams/syscall-path.md).

- Numbers are the **x86_64** table. Names in the unhandled arm come from
  [`crates/akuma-syscalls-abi`](../../../crates/akuma-syscalls-abi)
  (`Syscall::from_x86_64`) so the diagnostics cannot drift from the numbering
  the full kernels share; the wire structs come from
  [`crates/akuma-syscalls-linux`](../../../crates/akuma-syscalls-linux) with
  their layout assertions (`fill_stat64` fills the asm-generic `Stat` and
  `stat::to_x86_64` re-lays it — amd64's hop).
- The constant table the dispatcher matches on is `akuma-ring3-amd64`'s `nr`
  module — a **hand table with known archaeology**: its `GETPID` is 38, so the
  arm also matches the ABI crate's x86_64 truth (39). Prefer the ABI crate's
  names for any new arm.
- Unknown numbers print `unhandled <Name> (<nr>)` and answer `-ENOSYS`: each
  boot reads as a checklist of exactly what is still missing.
- A user pointer is only ever touched through `user_ok` + the physmap copy
  pair ([`memory.md`](memory.md) § the user-pointer rule).

## fd routing is table-first

`read`/`write`/`writev`/`readv`/`lseek`/`fstat` route on the fd's **kind in
the table**, not its number: a bound fd is served by its kind whatever the
number, so a shell redirect that dup2's a file onto fd 1 captures the flush.
Only an *unbound* fd falls to the session rules: unbound 1/2 are the console
write-only (or the nested child's pipe for fd 1 while a child runs), unbound 0
reads EOF, any other unbound number is `EBADF`. A dup of an unbound stdio fd
materializes an `OpenFile::Session` entry at its own number first, so ash's
save/restore dance around a redirect (`fcntl(1, F_DUPFD, 10)` … `dup2(10, 1)`)
lands on a real description.

fd slots are fixed: 16 per task (`SLOT_CEILING`), 3–15 allocatable
([`crates/akuma-fdtable`](../../../crates/akuma-fdtable) owns lowest-free,
close, dup_to, fork_copy, the flag sets; this kernel supplies the effects).

## The table

| nr | name | arm (file) | decides | status / notes |
|---|---|---|---|---|
| 0 | read | `read_fd_arm` (main.rs) | kind-routed: ext2 / akuma-net / child.rs / eventfd crate / akuma-pipes | full |
| 1 | write | dispatch WRITE (main.rs) | kind-routed; console via akuma-comconsole | full; 1 MiB cap |
| 2 | open | `open_common` (main.rs) | akuma-vfs path walk + akuma-ext2 | absolute paths; O_CREAT/O_TRUNC served |
| 3 | close | dispatch CLOSE (main.rs) | akuma-fdtable; socket/eventfd/pipe release | full; 0/1/2 are no-ops |
| 4 | stat | dispatch STAT (main.rs) | akuma-ext2 metadata, wire = syscalls-abi | follows final symlink |
| 5 | fstat | `fstat_fd` (main.rs) | fdtable kind → ext2 / conventional modes | full |
| 6 | lstat | `lstat_path` (main.rs) | parent listing, unfollowed final component | full |
| 7 | poll | `poll_arm` (sock.rs) | `fd_events` probe; NIC pumped per lap | ≤64 fds; timeout caps at 60 s |
| 8 | lseek | dispatch LSEEK (main.rs) | fdtable kind | files seek; dirs move the cursor; others ESPIPE |
| 9 | mmap | dispatch MMAP (main.rs) | this kernel (placement) | anon + MAP_PRIVATE file fill; no CoW |
| 10 | mprotect | `mprotect_arm` (main.rs) | PTE re-map under the W^X rule; `cow_privatize` before a shared frame goes RW | real (2026-10-30); `PROT_NONE` still not enforced ([`gaps.md`](../gaps.md)) |
| 11 | munmap | `munmap_arm` (main.rs) | `release_pages` returns the span and drops its frames; the brk/mmap cursors survive | real (2026-10-30); slabs outlive the task |
| 12 | brk | dispatch BRK (main.rs) | this kernel | grows by mapping; shrink is bookkeeping; failure returns the old break |
| 13 | rt_sigaction | `sigaction_arm` (main.rs) | `akuma-syscalls-signal` `SigTable`/wire: validates, stores, replies | real record-keeping; still no delivery |
| 14 | rt_sigprocmask | `sigprocmask_arm` (main.rs) | the mask word is kept and read back; algebra = `akuma_syscalls_signal::procmask` | real (2026-10-30); SIGKILL/SIGSTOP unblockable; still no delivery |
| 16 | ioctl | dispatch (main.rs) | — | `ENOTTY`: the console is a serial line, not a tty |
| 17 | pread64 | dispatch (main.rs) | akuma-ext2 `read_at_by_inode` | fd position untouched (Linux contract) |
| 19 | readv | dispatch READV (main.rs) | routes per iovec through `read_fd_arm` | stops at the first short transfer; error mid-vector returns bytes already moved |
| 20 | writev | dispatch WRITEV (main.rs) | gather once, then kind-routed | musl stdio's flush path; the gather is one message |
| 21 | access | dispatch (main.rs) | akuma-ext2 metadata | existence is the whole check (root, one task) |
| 22 | pipe | `pipe_arm` (pipes.rs) | akuma-pipes | = `pipe2(fds, 0)`; keyed on the literal (ring3's table has no name) |
| 23 | select | `select_arm` (sock.rs) | akuma-syscalls-poll fdset + `fd_events` | exceptfds written back all-zero on purpose; NULL timeout caps at 60 s |
| 25 | mremap | dispatch (main.rs) | this kernel | MAYMOVE grow copies; shrink keeps the address; MREMAP_FIXED refused |
| 32 | dup | dispatch DUP (main.rs) | akuma-fdtable | materializes `Session` for stdio; pipe ends get `clone_ref` |
| 33 | dup2 | dispatch (main.rs) | akuma-fdtable `dup_to` | displaced sockets/pipes released; old==new → newfd if open |
| 35 | nanosleep | dispatch (main.rs) | child::sleep_us over the TSC clock | ≤60 s per call; pumps the NIC each lap |
| 39 | getpid | dispatch (main.rs) | child::current_pid | also matched at 38 (ring3 table archaeology) |
| 41 | socket | `socket_arm` (sock.rs) | akuma-net socket table | AF_INET, STREAM/DGRAM; NONBLOCK/CLOEXEC stick |
| 42 | connect | `connect_arm` (sock.rs) | akuma-net; kernel-side wait with `[cx]` trace | 15 s cap; ETIMEDOUT/ECONNREFUSED |
| 43 | accept | `accept_arm` (sock.rs) | akuma-net | listener's nonblock inherited by the child fd |
| 44 | sendto | `sendto_arm` (sock.rs) | akuma-net | UDP uses the destination (or the connect peer); TCP ignores it |
| 45 | recvfrom | `recvfrom_arm` (sock.rs) | akuma-net | source address filled for UDP |
| 48 | shutdown | `shutdown_arm` (sock.rs) | akuma-net | full |
| 49 | bind | `bind_arm` (sock.rs) | akuma-net | full |
| 50 | listen | `listen_arm` (sock.rs) | akuma-net | backlog clamped 1..8 |
| 51 | getsockname | `getsockname_arm` (sock.rs) | akuma-net | iface IP stands in for 0.0.0.0; sin_family host byte order |
| 52 | getpeername | `getpeername_arm` (sock.rs) | akuma-net | UDP connect peer / stream remote |
| 54 | setsockopt | `setsockopt_arm` (sock.rs) | akuma-net timeouts | RCVTIMEO/SNDTIMEO real; REUSEADDR…/TCP_* accepted-ignored; else ENOPROTOOPT |
| 55 | getsockopt | `getsockopt_arm` (sock.rs) | akuma-net | SO_ERROR is the dial's outcome (the nonblocking-connect idiom) |
| 56 | clone | `fork_arm` → M2 arm (child.rs) | M2 blocking fork | SIGCHLD[|VM|VFORK] only; else EINVAL with flags printed |
| 57 | fork | `fork_arm_real` (child.rs) | akuma-exec `fork_process` | real fork: child published runnable with own slot + band; depth ≤3; stamps `PARENT_OF` |
| 58 | vfork | `fork_arm_real` (child.rs) | as fork | bare; contract holds by construction |
| 59 | execve | `exec_real_task` / `exec_replace` (child.rs) | exec-replace | a real child replaces in place (fds survive); the M2 nested arm serves the shell's tail-exec; both crossings zero fs ([`processes.md`](processes.md) § fs) |
| 60 | exit | dispatch (main.rs) | task_done / child_exit | code & 0xFF |
| 61 | wait4 | `wait4_arm` (child.rs) | spawn ring + `PARENT_OF` | parent-owned: reaps only own stamped children, ECHILD for a stranger's or none; parks on own running children; WNOHANG → 0; rusage ignored |
| 63 | uname | dispatch (main.rs) | syscalls-linux uts::Field | "Linux taoxichuan 6.6.0-akuma … x86_64" |
| 72 | fcntl | dispatch (main.rs) | akuma-fdtable | F_GETFD/F_SETFD/F_GETFL(=0)/F_SETFL(O_NONBLOCK)/F_DUPFD/F_DUPFD_CLOEXEC |
| 73 | flock | dispatch (main.rs) | — | 0: one task, nothing contends |
| 77 | ftruncate | dispatch (main.rs) | akuma-ext2 `truncate_by_inode` | fd's cached size follows |
| 79 | getcwd | dispatch (main.rs) | — | always "/"; no working directory exists |
| 91 | fchmod | dispatch (main.rs) | — | 0: root's mode is the mode |
| 95 | umask | dispatch (main.rs) | — | 0o022, conventional; nothing inherits it |
| 96 | gettimeofday | dispatch (main.rs) | primitives::clock over TSC | wall time with an `epoch=` anchor, else zeros |
| 102/104/107/108 | getuid/getgid/geteuid/getegid | dispatch (main.rs) | — | 0 |
| 110 | getppid | dispatch (main.rs) | — | 1: everything is the init task's child |
| 137 | statfs | dispatch STATFS (main.rs) | akuma-ext2 `stats()` (real superblock) | prints a `[statfs]` evidence line per call |
| 138 | fstatfs | `fill_statfs` (main.rs) | same | same |
| 158 | arch_prctl | dispatch (main.rs) | akuma-cpu msr (canonical-checked) | ARCH_SET_FS/GET_FS; SET_FS also stamps the slot's fs shadow ([`processes.md`](processes.md) § fs) |
| 186 | gettid | dispatch (main.rs) | — | 1: one task |
| 217 | getdents64 | dispatch (main.rs) | syscalls-linux dirent over the open-time cached listing | d_ino = 0; d_off is a monotonic cookie |
| 218 | set_tid_address | dispatch (main.rs) | stores the clear-child-tid word in the task's shared record | a thread's `exit` zeroes the word and wakes its joiner |
| 228 | clock_gettime | dispatch (main.rs) | primitives::clock over TSC | REALTIME adds the epoch anchor; others boot-relative |
| 230 | clock_nanosleep | dispatch (main.rs) | as nanosleep | both spellings, incl. the raw (sec, nanos) register pair |
| 231 | exit_group | dispatch (main.rs) | as exit | nested child returns to the parent; boot tasks chain the program list |
| 257 | openat | dispatch (main.rs) | `resolve_at` + open_common | dirfd-relative; AT_FDCWD resolves at "/" |
| 258 | mkdirat | dispatch (main.rs) | akuma-ext2 `create_dir` | mode accepted and ignored |
| 260 | fchownat | dispatch (main.rs) | — | 0: everything is root already |
| 262 | newfstatat | dispatch (main.rs) | stat/lstat path by AT_SYMLINK_NOFOLLOW; AT_EMPTY_PATH → fstat | full |
| 263 | unlinkat | dispatch (main.rs) | akuma-ext2 remove_file/remove_dir | AT_REMOVEDIR honoured |
| 264 | renameat | dispatch (main.rs) | akuma-ext2 `rename` | both dirfds resolved |
| 265 | linkat | dispatch (main.rs) | — | ENOSYS: no hardlink arm on the ext2 trait yet |
| 266 | symlinkat | dispatch (main.rs) | akuma-ext2 `create_symlink` | full |
| 267 | readlinkat | dispatch (main.rs) | akuma-ext2 `read_symlink` (raw walk, lstat-style) | full |
| 268 | fchmodat | dispatch (main.rs) | — | 0 |
| 269 | faccessat | dispatch (main.rs) | akuma-ext2 metadata | existence only; mode > 7 is EINVAL |
| 280 | utimensat | dispatch (main.rs) | akuma-ext2 metadata (validate) | recorded-not: no wall clock; succeeds without stamping |
| 290 | eventfd2 | `eventfd2_arm` (main.rs) | akuma-syscalls-eventfd | 4 slots; dup'd fd shares the slot; blocking reads answer EAGAIN by construction |
| 292 | dup3 | dispatch (main.rs) | akuma-fdtable `dup_to` | O_CLOEXEC only; old==new → EINVAL |
| 293 | pipe2 | `pipe2_arm` (pipes.rs) | akuma-pipes | O_CLOEXEC recorded; O_NONBLOCK recorded and vacuous |
| 318 | getrandom | dispatch (main.rs) | net::rng_fill | RDRAND (checked via CPUID), SplitMix64 fallback, boot says which |
| 46 | sendmsg | `sendmsg_arm` (sock.rs) | msghdr/iovec marshal (56-byte x86_64 layout) → akuma-net | full; no control messages |
| 47 | recvmsg | `recvmsg_arm` (sock.rs) | same marshal back | source address for UDP; MSG_TRUNC on the small UDP buffers |
| 53 | socketpair | `socketpair_arm` (pipes.rs) | akuma-pipes over the unix domain | AF_UNIX SOCK_STREAM; the dynamic linker's `cc` spawn needs it |
| 74 | fsync | dispatch (main.rs) | akuma-vfs/ext2 | EBADF-checked; the ext2 write path is write-through, so the flush itself is a no-op (git's pack/idx fsync) |
| 75 | fdatasync | dispatch (main.rs) | as fsync | same |
| 80 | chdir | `chdir_to` (main.rs) | akuma-vfs walk; the cwd lives on the task record | inherited by fork children, reset to / on exec |
| 81 | fchdir | `chdir_to` (main.rs) | fd must be a directory description | full |
| 97 | getrlimit | `PRLIMIT64 \| 97` arm (main.rs) | RLIMIT_STACK = the 8 MiB stack ceiling; everything else unlimited | what musl's thread probes read |
| 131 | sigaltstack | `sigaltstack_arm` (main.rs) | SS_DISABLE/MINSIGSTKSZ recorded per task | full record/reply; nothing dispatches on it yet |
| 166 | sync | dispatch (main.rs) | `root_fs().sync()` | accepted; write-through makes it a no-op |
| 200 | tkill | `tkill_arm` (main.rs) | no delivery: a blocked poll/pipe read of that thread answers EINTR | what cargo's jobserver needs; signal 0 probes |
| 202 | futex | `futex::sys_futex` (futex.rs) | `akuma-syscalls-sync` WaiterTable/op algebra | WAIT/WAKE/WAIT_BITSET/REQUEUE/WAKE_OP, private + shared, timeouts |
| 234 | tgkill | `tkill_arm` (main.rs) | as tkill | same |
| 203 | sched_getaffinity | dispatch (main.rs) | one CPU, mask = 1 | the toolchain's thread-count probe |
| 302 | prlimit64 | `PRLIMIT64 \| 97` arm (main.rs) | as getrlimit; sets accepted-ignored | musl's pthread stack sizing |
| 306 | syncfs | dispatch (main.rs) | `root_fs().sync()` | as sync |
| 331 | membarrier | `membarrier_arm` (main.rs) | `akuma-syscalls-mem::membarrier` command probing | queries served; no global barrier needed on one core |
| 0x1000+301 | akuma SPAWN | `spawn_arm` (child.rs) | the child model | run-to-completion; returns pid \| stdout_fd<<32 |
| 0x1000+303 | akuma WAITPID | `waitpid_arm` (child.rs) | spawn ring | 0 for unknown-but-plausible ("still running") |
| 0x1000+319 | akuma UPTIME | dispatch (main.rs) | net::uptime_us | µs since boot |
| 0x1000+326 | akuma CLOSE_CHILD_STDIN | `close_stdin_arm` (child.rs) | — | success without effect (no stdin path exists) |

The legacy x86-only numbers (mkdir 83, rename 82, rmdir 84, unlink 87,
`chmod` 90, `chown` 92/94, `symlink` 88, `readlink` 89, `unlink` …) are folded
to their `*at` spellings by `akuma-syscalls-abi::legacy::fold` at the top of
`dispatch_inner` (main.rs:2699); legacy `open`/`stat`/`lstat` resolve against
the task's cwd. The rows above list only the canonical numbers.

## Still missing (the `unhandled <Name>` checklist, confirmed against `dispatch_inner` 2026-10-30)

| numbers | family | what unblocks it |
|---|---|---|
| 213/232/233/281/291 | epoll (`epoll_create*`, `epoll_ctl`, `epoll_wait`, `epoll_pwait*`) | the epoll brief (`scripts/kvm/goose/epoll-babysit.md`); `userspace/epollprobe` is written for it |
| 157 | `prctl` | musl thread setup beyond the stack probes |
| 326 | `statx` | coreutils' detailed stat paths |
| 40 | `sendfile` | cp/mv fast paths in the guest |
| 265 | `linkat` | no hardlink arm on the ext2 trait yet |
| 98 | `getrusage` | shells' `times`/`time` reporting |

Known deliberate gaps that answer ENOSYS or no-op rather than fail startup:
`execve` outside any task context, `ioctl` (ENOTTY), `PROT_NONE` (not enforced).
Everything else prints its name and ENOSYS.

## Background

`docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` (trim step 2 — wire structs
pulled, not hand-written), `docs/reference/subsystems/syscalls/` (the full
kernels' per-family docs these arms mirror),
`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` (why the number table is
a crate).
