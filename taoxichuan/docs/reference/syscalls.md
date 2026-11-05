# Syscalls — the crossing and who answers

**Stability: B** (the routing is code in two crates and a classifier checks it against
real traces; re-run the classifier rather than trusting a list here).

## The crossing

```mermaid
sequenceDiagram
    participant U as task (ring 3)
    participant S as syscall stub (akuma-ring3-amd64)
    participant D as dispatch (lib.rs)
    participant T as akuma-runtime-tokio
    participant G as glue / glue-amd64
    U->>S: syscall (rax=nr, args; CPU masks IF)
    Note over S: switch to the syscall stack,<br/>stash user rsp, build SyscallFrame
    S->>D: dispatch(&mut frame)
    Note over D: take the big kernel lock;<br/>fold the legacy x86 numbers
    D->>T: dispatch(nr, args)
    T->>G: allow-listed: glue by asm-generic number<br/>else: the x86_64 fd family (glue-amd64)
    G-->>D: Some(answer), or None
    Note over D: None: this kernel's own arms<br/>(memory, clone/exec/exit, signals)
    Note over D: deliver pending signals,<br/>restore the user-rsp stash, drop the lock
    D-->>S: u64 answer (errno = negated in rax)
    S-->>U: sysretq (rflags 0x202: IF set)
```

Canonical mermaid source: [`../diagrams/syscall-path.md`](../diagrams/syscall-path.md).

Numbers are the **x86_64** table, named by
[`crates/akuma-syscalls-abi`](../../../crates/akuma-syscalls-abi) (`Syscall::from_x86_64`); the wire structs
come from [`crates/akuma-syscalls-linux`](../../../crates/akuma-syscalls-linux) with their layout
assertions.

## Who answers, in order

`dispatch` (lib.rs) takes the big kernel lock for the whole call (nesting if this core holds it), ends
the caller first if a sibling's `exit_group` asked it to, and then `dispatch_inner` asks:

1. **`akuma_syscalls_abi::legacy::fold`.** The x86-only path spellings (`rename` 82, `mkdir` 83, `rmdir` 84,
   `link` 86, `unlink` 87, `symlink` 88, `readlink` 89, `chmod` 90, `chown` 92, `lchown` 94) become their
   `*at` call with `AT_FDCWD`, rewritten in the frame. amd64 folds through the same table.
2. **`console_stdio_write`.** A `write`/`writev` on `Stdout`/`Stderr`/`DevTty` from a process with no I/O
   channel of its own goes to the console. It reads `Process.channel`, not `current_channel()`, which falls
   back to a forked child's exit channel. Neither call is on the allow-list, so this order changes no
   routing.
3. **`tokio_surface`** (`akuma-tokio-kernel-amd64`): [`akuma_runtime_tokio::serve_shared` and
   `serve`](../../../crates/akuma-runtime-tokio), arch-neutral, send an allow-listed call to
   `akuma_syscalls_glue::handle_syscall` by its asm-generic number; anything else goes to
   `akuma_syscalls_glue_amd64::dispatch`, the x86_64 ABI layer over glue that amd64 uses too (open-flag
   permutation, the x86_64 `struct stat`, poll/select timeouts, `dup2`'s `old == new` rule, the legacy
   numbers with no asm-generic twin), which answers `None` for what it does not know.
4. **The local arms** (below): what a kernel owns because it owns the address space and the machine.
5. **Anything else** answers `ENOSYS` and prints `[nosys] nr=N` once per number per boot, so a probe run is
   its own gap list.

With the `middleware` feature (off by default) the table of `akuma_tokio_kernel_core::middleware` wraps this: its `own` tier
runs in `dispatch` outside the lock (raw number, before the fold), its `kernel` tier wraps steps 2-5 inside it (after the fold),
and `fallback` runs only where steps 2-5 answered `ENOSYS`. aarch64 has the same two seams (`trap::el0_sync` and
`syscall::dispatch`). Without the feature none of it is compiled. How to use it and the rules: [the middleware guide](../../../crates/akuma-tokio-kernel-core/docs/middleware.md); the design record: [B22](../../../docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md).

On the way out, `dispatch` restores the user-RSP stash (the stub's per-CPU slot, which any switch inside the
call lets another thread overwrite), takes pending signals (`child::deliver_at_return`: a handler frame, an
ignore, or a default action that ends the group) and drops the lock.

## The allow-list is a crate's, grouped by feature

`akuma-runtime-tokio`'s `served_by_glue` is the union of four groups:

| group | feature | what |
|---|---|---|
| `runtime` | always | the 13 calls every tokio program made in B10's census (identity, `sched_getaffinity`, `getrandom`, signal dispositions and masks, `sigaltstack`, `prctl`, `set_tid_address`, `clock_gettime`, `futex`) plus the two sleeps |
| `net` | `net` (default) | the socket calls, AF_INET and AF_UNIX; `sockaddr`/`msghdr` have one layout on both ABIs |
| `process` | `process` | `wait4`, `waitid`, and `pidfd_open`/`pidfd_send_signal` by their shared numbers |
| `shell` | `shell` | what busybox and bash call and no tokio program does: job-control ids, `getrusage`/`times`/`sysinfo`, limits, `sync`/`syncfs`, `membarrier`, the clock setters, `sethostname` |

Taoxichuan's own `shell` feature turns on the crate's (`default = ["shell"]`, since the images run busybox
and bash). Glue's Akuma-native calls (`SPAWN`, `WAITPID`, `UPTIME`, the terminal calls, `RESOLVE_HOST`, …)
are behind glue's `akuma-native` feature, which this kernel leaves off
([`config-flags.md`](../../../docs/reference/subsystems/config-flags.md) § "Crate features Taoxichuan
leaves off").

To check a workload against the routing, trace it on Linux with `strace -f -c` and run
`scripts/kvm/taoxichuan/census_classify.py`: it reads the groups, glue-amd64's dispatch, the legacy table
and this kernel's arms from the source, and exits 1 if any call is unserved
([`TAOXICHUAN_B10_SYSCALL_CENSUS.md`](../../../docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md): 76 calls
over three tokio programs, 0 unserved).

## The local arms

Every arm `dispatch_inner` keeps after the steps above:

| nr | call | arm (file) | notes |
|---|---|---|---|
| 60, 231 | `exit`, `exit_group` | dispatch (lib.rs) → `child::task_exit_*` | `akuma-syscalls-proc::exit::scope`: `exit` from a thread ends the thread, otherwise the group. The boot task's exit is `task_done` |
| 12 | `brk` | dispatch (lib.rs) | `akuma-user-layout` plans it; grows by mapping zeroed pages; shrink is bookkeeping; failure returns the old break |
| 158 | `arch_prctl` | dispatch (lib.rs) | `ARCH_SET_FS`/`ARCH_GET_FS` through `akuma-cpu`'s canonical-checked MSR write; `SET_FS` also stamps the slot's fs shadow ([`processes.md`](processes.md) § fs) |
| 9 | `mmap` | dispatch (lib.rs) | anonymous and file-backed (filled from the inode); W^X check from `akuma-syscalls-mem`; placement from `akuma-user-layout` ([`memory.md`](memory.md)) |
| 10 | `mprotect` | `mprotect_arm` (lib.rs) | re-maps the PTEs under the W^X rule; `cow_privatize` before a shared frame goes writable |
| 11 | `munmap` | `munmap_arm` (lib.rs) | returns the frames and the span to the hole table |
| 26 | `msync` | `msync_arm` (lib.rs) | writes a writable `MAP_SHARED` file mapping back to its file |
| 28 | `madvise` | `madvise_arm` (lib.rs) | `akuma-syscalls-mem` decode; `MADV_DONTNEED` zeroes the mmap-slab pages (a frame a fork peer maps is replaced, not zeroed); `MADV_FREE` is `EINVAL` |
| 25 | `mremap` | dispatch (lib.rs) | shrink frees the tail; grow in place at the slab top, else move and copy; `MREMAP_FIXED` refused |
| 24 | `sched_yield` | dispatch (lib.rs) | `akuma_threading::x86_yield` when another thread is runnable |
| 130, 127 | `rt_sigsuspend`, `rt_sigpending` | glue, called directly | glue's arms, as on the other kernels |
| 34 | `pause` | `akuma_syscalls_glue::sys_pause` | |
| 15 | `rt_sigreturn` | `child::sigreturn_arm` | restores the frame `push_handler` saved |
| 56, 57, 58 | `clone`, `fork`, `vfork` | `child::fork_arm` | `akuma-syscalls-proc::clone::decode`: `CLONE_THREAD` → `clone_thread`, everything else → `fork_process` ([`processes.md`](processes.md)) |
| 59 | `execve` | `child::exec_real_task` | the boot thread has no process to replace: `ENOSYS` |
| 62, 200, 234 | `kill`, `tkill`, `tgkill` | `akuma_syscalls_glue_amd64::kill` | the x86 kernels' shared bodies: pend, and interrupt the target only when the default action kills |

## Facts the shared layers rely on

- **The descriptor table is `akuma-exec`'s.** Every descriptor lives in the calling process's
  `SharedFdTable` (`Process.fds`): `fork_process` deep-clones it, `clone_thread` shares it, exec sweeps its
  close-on-exec entries, and exit closes it (`child::close_process_fds`, after the siblings are stopped, so a
  pipe's EOF is its last writer's close). The tables behind the descriptors (pipes, eventfd, epoll, AF_UNIX,
  flock) are glue's; `exec_twin.rs` points the runtime's release hooks at them.
- **A file's size is the inode's**, never the description's: glue reads the inode
  ([`TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md`](../../../docs/archive/TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md)).
- **A socket goes with its last name**: `akuma-net` reference-counts it (`socket_clone_ref` / `remove_socket`).
- **Glue writes user memory directly**, so the page-fault handler breaks CoW for a ring-0 write to a present
  user page and resumes a faulting copy at `akuma_user_access::user_copy_fixup`.
- **Glue's waits switch threads**, so the user-RSP stash is saved and restored around every call (above).
- **A fatal default action inside glue** (SIGPIPE on a broken pipe) ends the process through `akuma-exec`;
  the slot-purge hook (`child::on_thread_terminated`) finishes the slot.
- `sendfile` and `copy_file_range` answer `ENOSYS`; busybox and Rust's `fs::copy` fall back to
  `read`/`write`. `sendfile` (40) is the one `[nosys]` line the battery prints.

## Background

[`TAOXICHUAN_CLEANUP_PLAN.md`](../../../docs/archive/TAOXICHUAN_CLEANUP_PLAN.md) (steps 2c, B10, B11: the fd
family to glue, the census, the tokio crate),
[`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](../../../docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md),
`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` (why the number table is a crate),
`docs/reference/subsystems/syscalls/` (the per-family docs glue's arms follow). The per-call table this file
held until 2026-11-02 is in git history; most of its rows had become glue's.
