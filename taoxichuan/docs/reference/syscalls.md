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
| 10 | mprotect | dispatch (main.rs) | — | accepted no-op |
| 11 | munmap | dispatch (main.rs) | — | accepted no-op (slabs outlive the task) |
| 12 | brk | dispatch BRK (main.rs) | this kernel | grows by mapping; shrink is bookkeeping; failure returns the old break |
| 13 | rt_sigaction | dispatch (main.rs) | — | accepted no-op: no signal delivery exists to register against |
| 14 | rt_sigprocmask | dispatch (main.rs) | — | accepted no-op, same reason |
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
| 56 | clone | `fork_arm` (child.rs) | M2 blocking fork | SIGCHLD[|VM|VFORK] only; else EINVAL with flags printed |
| 57 | fork | `fork_arm` (child.rs) | M2 blocking fork | bare (no flags word) |
| 58 | vfork | `fork_arm` (child.rs) | M2 blocking fork | bare; contract holds by construction |
| 59 | execve | `exec_replace` (child.rs) | exec-replace | nested child only; else ENOSYS |
| 60 | exit | dispatch (main.rs) | task_done / child_exit | code & 0xFF |
| 61 | wait4 | `wait4_arm` (child.rs) | spawn ring | reaps; double-wait = ECHILD; rusage ignored |
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
| 218 | set_tid_address | dispatch (main.rs) | — | 1; nothing to clear |
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
| 0x1000+301 | akuma SPAWN | `spawn_arm` (child.rs) | the child model | run-to-completion; returns pid \| stdout_fd<<32 |
| 0x1000+303 | akuma WAITPID | `waitpid_arm` (child.rs) | spawn ring | 0 for unknown-but-plausible ("still running") |
| 0x1000+319 | akuma UPTIME | dispatch (main.rs) | net::uptime_us | µs since boot |
| 0x1000+326 | akuma CLOSE_CHILD_STDIN | `close_stdin_arm` (child.rs) | — | success without effect (no stdin path exists) |

Known deliberate gaps that answer ENOSYS or no-op rather than fail startup:
`rt_sigaction`/`rt_sigprocmask` (no delivery), `munmap`/`mprotect` (slabs),
`linkat` (no ext2 arm), `execve` outside a nested child, `ioctl` (ENOTTY).
Everything else prints its name and ENOSYS.

## Background

`docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` (trim step 2 — wire structs
pulled, not hand-written), `docs/reference/subsystems/syscalls/` (the full
kernels' per-family docs these arms mirror),
`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` (why the number table is
a crate).
