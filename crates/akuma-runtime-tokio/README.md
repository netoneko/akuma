# akuma-runtime-tokio

The syscall surface a tokio program needs, as one dependency. A kernel that depends on this crate runs
unmodified Rust std + tokio binaries: "this kernel runs tokio" is a dependency, not a port.
`#![forbid(unsafe_code)]`, `#![no_std]`.

It implements no syscall. The bodies are `akuma-syscalls-glue`'s (shared by all three kernels) and the x86_64
fd-family layer is `akuma-syscalls-glue-amd64`'s. This crate owns three things:

- **The allow-list** (`served_by_glue`): which of glue's calls the kernel serves, grouped by the tokio feature
  that needs them.
- **The setup** (`install`): glue's and `akuma-vfs-glue`'s hooks with a tokio kernel's answers (no rump,
  console poller, mouse, tun, audio or file-page cache), the pipe wake sink and the RNG hook.
- **The dispatch** (`dispatch`): an x86_64 syscall number in, `Some(return value)` or `None` out.

## Using it

A composition root does three things (Taoxichuan's are `taoxichuan/src/main.rs` and `exec_twin.rs`):

```rust
// 1. Once, before mounting the root and before the first ring-3 crossing.
akuma_runtime_tokio::install(akuma_runtime_tokio::Kernel {
    utc_time_us: || akuma_primitives::clock::utc_time_us(net::uptime_us()),
    online_cpus: smp::online_cpus,
    rng_fill: |b| { net::rng_fill(b); true },
    after_reap: child::collect, // free what a reap leaves; `|| {}` if nothing
});

// 2. In the syscall handler, after any kernel-specific intercepts.
if let Some(r) = akuma_runtime_tokio::dispatch(nr, args) {
    return r;
}
// None: the kernel's own arms (mmap/brk/mprotect, clone/execve/exit, signal frames), else ENOSYS.

// 3. With the `process` feature, in the `ExecRuntime` registration.
pidfd_close: akuma_runtime_tokio::PIDFD_CLOSE,
```

What the kernel still serves itself: memory (`mmap` and friends, `brk`), `clone`/`execve`/`exit`, signal
delivery and the signal frame, and its own devices. The kernel also registers `akuma-exec`'s runtime hooks
(processes, threads, fds); this crate assumes them.

## Features

| feature | default | serves |
|---|---|---|
| (none) | — | `runtime`: what every census program called (identity, `sched_getaffinity`, `getrandom`, signal dispositions and masks, `sigaltstack`, `prctl`, `set_tid_address`, `clock_gettime`, `futex`) plus `nanosleep` and `clock_nanosleep` |
| `net` | yes | the socket calls (AF_INET and AF_UNIX), and glue's smoltcp socket table |
| `process` | no | `wait4`, `waitid`, `pidfd_open`, `pidfd_send_signal` |
| `shell` | no | what busybox and bash call and no tokio program does: job-control ids, limits and accounting, `sync`, `membarrier`, the clock and hostname setters |

epoll and eventfd are always in: mio's `Poll` and `Waker` are every runtime's first two calls. A kernel whose
images run a shell turns `shell` on (Taoxichuan does, through its own default `shell` feature).

## Tests

```bash
cargo test -p akuma-runtime-tokio --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

They pin the groups: together they route exactly the list Taoxichuan used before the crate existed, no call is in
two groups, the allow-list follows the enabled features, the always-on group is the census's every-program set,
every served call decodes from its x86_64 number, and the shared pidfd numbers agree with `akuma-syscalls-linux`.

## Checking a workload

Trace it on Linux with `strace -f -c`, then run `scripts/kvm/taoxichuan/census_classify.py OUT UNISTD_64_H`. It
reads this crate's groups, glue-amd64's dispatch, the legacy fold and Taoxichuan's own arms from the source, names
who serves each call, and exits 1 on one nobody serves.

## More

- Reference: [`docs/reference/subsystems/syscalls/runtime-tokio.md`](../../docs/reference/subsystems/syscalls/runtime-tokio.md)
- The census (76 calls over three tokio programs, 0 unserved):
  [`docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md`](../../docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md)
- The analysis that shaped it:
  [`docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](../../docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md)
- Its one consumer's dispatch order: [`taoxichuan/docs/reference/syscalls.md`](../../taoxichuan/docs/reference/syscalls.md)
