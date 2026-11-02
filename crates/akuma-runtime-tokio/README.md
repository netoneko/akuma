# akuma-runtime-tokio

The syscall surface a tokio program needs, as one dependency. A kernel that depends on this crate runs
unmodified Rust std + tokio binaries: "this kernel runs tokio" is a dependency that ensures Linux binary compatibility.
`#![forbid(unsafe_code)]`, `#![no_std]`.

It implements no syscall. The bodies are `akuma-syscalls-glue`'s (shared by all three kernels) and the x86_64
fd-family layer is `akuma-syscalls-glue-amd64`'s. This crate owns three things:

- **The allow-list** (`served_by_glue`): which of glue's calls the kernel serves, grouped by the tokio feature
  that needs them.
- **The setup** (`install`): glue's and `akuma-vfs-glue`'s hooks with a tokio kernel's answers (no rump,
  console poller, mouse, tun, audio or file-page cache), the pipe wake sink and the RNG hook.
- **The dispatch** (`dispatch`): an x86_64 syscall number in, `Some(return value)` or `None` out.

## The picture

```text
          BUILDING A KERNEL THAT RUNS TOKIO   (akuma-runtime-tokio)
   "this kernel runs tokio" is one Cargo dependency that ensures Linux binary compatibility

 ─── 1. BUILD TIME: Cargo.toml ─────────────────────────────────────────────────────────

   YOUR KERNEL CRATE (composition root, e.g. taoxichuan/)
   +--------------------------------------+      +--------------------------------------+
   |  YOU WRITE / PULL                    |      |  YOU GET  (one line in Cargo.toml)   |
   |                                      |      |                                      |
   |  boot + arch   boot-amd64, gdt, idt, |      |  akuma-runtime-tokio                 |
   |                trap, lapic, fpu      |      |    features: net (default),          |
   |  memory        pmm, alloc, mmap, cow |      |              process, shell          |
   |  disk + fs     virtio, ext2, vfs     |      |            |                         |
   |  network       net, net-nic          |      |            +-> akuma-syscalls-glue   |
   |  scheduling    sched-amd64, smp/BKL  |      |            |     the syscall BODIES, |
   |  processes     akuma-exec,           |      |            |     shared by 3 kernels |
   |                akuma-threading       |      |            +-> glue-amd64            |
   |                                      |      |            |     x86_64 fd ABI: open,|
   |  + the syscall arms only a kernel    |      |            |     stat, poll, select  |
   |    can answer (see box 3)            |      |            +-> syscalls-abi / -linux |
   +--------------------------------------+      |            |     which number is what|
                                                 |            +-> vfs-glue, primitives  |
                                                 +--------------------------------------+

 ─── 2. BOOT TIME: wire it once ────────────────────────────────────────────────────────

   power on --> traps, memory, cores, NIC
            --> register akuma-exec + akuma-threading hooks   (pidfd_close: PIDFD_CLOSE)
            --> akuma_runtime_tokio::install(Kernel {
                    utc_time_us,     // your clock
                    online_cpus,     // your cores
                    rng_fill,        // your entropy (RDRAND)
                    after_reap,      // your cleanup after wait4/waitid
                })                   // 4 callbacks: everything else glue already knows
            --> mount ext2 at /  -->  exec init (sshd)  -->  ring 3

 ─── 3. RUN TIME: one syscall from a tokio program ─────────────────────────────────────

   ring 3:  tokio / mio / std  ── syscall nr, args ──+
                                                     v
   YOUR KERNEL    +------------------------------------------------------+
                  | entry stub -> take the BKL                           |
                  | your intercepts (Taoxichuan: legacy fold mkdir->     |
                  |   mkdirat, console stdio)                            |
                  +--------------------------+---------------------------+
                                             v
   RUNTIME-TOKIO  akuma_runtime_tokio::dispatch(nr, args)
                  +------------------------------------------------------+
                  | a. pidfd_open / pidfd_send_signal  --> glue          |  Some(r)
                  | b. on the allow-list?              --> glue          |--------+
                  |      runtime: futex, clock_gettime, getrandom, ...   |        |
                  |      net:     socket, connect, epoll, eventfd, ...   |        |
                  |      process: wait4, waitid    (+ after_reap)        |        |
                  |      shell:   setpgid, getrlimit, sync, ...          |        |
                  | c. else        --> glue-amd64 (open, read, stat ...) |        |
                  +--------------------------+---------------------------+        |
                                             | None                               |
                                             v                                    |
   YOUR KERNEL    +------------------------------------------------------+        |
                  | your own arms:                                       |        |
                  |   memory   mmap brk mprotect munmap mremap madvise   |        |
                  |   procs    clone/fork execve exit                    |        |
                  |   signals  delivery, rt_sigreturn, kill              |        |
                  |   cpu      arch_prctl sched_yield                    |        |
                  | none of those?  --> -ENOSYS + "[nosys] nr=N" (once)  |        |
                  +--------------------------+---------------------------+        |
                                             v                                    |
   ring 3:  <──────────────────────── return value <──────────────────────────────+

 ─── 4. PROVING IT: the census loop ────────────────────────────────────────────────────

   your tokio app on Linux --strace -f -c--> syscall list --census_classify.py--> who serves each?
                                                                                     |
                     names an unserved call, exit 1  <-------------------------------+
                     every call has a server, exit 0
   Taoxichuan:  3 tokio programs (nettest-reqwest, kot, goose), 76 calls, 0 unserved
```

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
- A whole kernel on top of it, from one short file (proposal):
  [`proposals/B15_TOKIO_KERNEL_KIT.md`](../../proposals/B15_TOKIO_KERNEL_KIT.md)
- Its one consumer's dispatch order: [`taoxichuan/docs/reference/syscalls.md`](../../taoxichuan/docs/reference/syscalls.md)
