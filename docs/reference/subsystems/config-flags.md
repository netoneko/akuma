# Cargo features, env vars, and debug knobs

Current-state reference for everything that flips build/runtime behaviour. This
is the single place to look before toggling a knob.

See `file:line` references for the authoritative source. For the investigation
behind any of these, follow the links into `../../archive/`.

## Profiles

Codegen profiles live in `Cargo.toml`. Behaviour comes from **features**, not
profiles — the profile just sets opt level / codegen units / LTO.

| Profile | Inherits | Used by | Notes |
|---|---|---|---|
| `release` | — | default `cargo run` | full feature set |
| `size` | `release` | `scripts/build_size.sh` | `--no-default-features`, re-adds minimal set |
| `extreme-size` | `size` | `scripts/build_extreme_size.sh` | 4 MB floor; omits TLS, block cache |
| `release-smp` | `release` | `cargo build --profile release-smp --features smp` | multikernel; paired with `smp` feature |
| `release-smp-shared` | `release` | `cargo build --profile release-smp-shared --features smp-shared`; also `scripts/build_devbox_smoltcp.sh` | real shared-kernel SMP; paired with `smp-shared`. Mutually exclusive with `smp` (build.rs panics if both) |
| `devbox` | `release` | `scripts/build_devbox.sh`, `overlays/devbox/run.sh` | rump-only, no smoltcp |

There is no `devbox-smoltcp` *profile* — the default devbox target is the
`release-smp-shared` profile plus the `devbox-smoltcp` feature. See
[`../build-profiles.md`](../build-profiles.md) for the target-level view.

Source: `Cargo.toml:77-123`.

## Features

`default` (`Cargo.toml:126-135`):
```
neko, smoltcp, kernel-tls, tls-rsa, sound, rump,
sc-aio, sc-sysv-ipc, sc-framebuffer, sc-containers,
sc-timerfd, sc-eventfd, sc-pidfd, sc-epoll
```

### Networking stack

| Feature | Effect | Source |
|---|---|---|
| `smoltcp` | Native smoltcp TCP/IP stack on NIC0; built-in SSH + DNS/HTTP depend on it. | `Cargo.toml:221` |
| `rump` | Raw L2 tap path on NIC1 (`/dev/net/tap0`) for a userspace NetBSD rump stack. NIC1 only exists with `RUMP_NIC=1`. | `Cargo.toml:215` |
| `rump-default` | Makes the rump stack the **default** for box 0 (kernel brings up `/bin/rump_server` at boot). Implies `rump`. | `Cargo.toml:292` |
| `userspace-sshd` | Disables the built-in (smoltcp) in-kernel SSH server; only herd's `/bin/sshd` runs. Drives `config::ENABLE_USERSPACE_SSHD`. | `Cargo.toml:300` |
| `devbox` | Meta-feature = `["rump-default", "userspace-sshd"]`. | `Cargo.toml:307` |
| `devbox-smoltcp` | Meta-feature = `["userspace-sshd", "smp-shared"]` — the **default** devbox. Keeps smoltcp; drops only the built-in SSH. | `Cargo.toml:319` |

> **Drift note:** `overlays/devbox/README.md:142,211`
> still say "Phase 2 will build with `--no-default-features`" and "smoltcp is
> still compiled in (for now)". That is **stale** — `scripts/build_devbox.sh`
> and `overlays/devbox/run.sh` already pass `--no-default-features`, so smoltcp
> (and the smoltcp-coupled built-in SSH, `kernel-tls`, `tls-rsa`) are compiled
> out entirely in the devbox today.

### TLS

| Feature | Effect | Source |
|---|---|---|
| `kernel-tls` | In-kernel TLS client (embedded-tls + X.509 verifier, ~58 KB). Only consumer is the shell `curl` https path, which is smoltcp-coupled. Dead weight without `smoltcp`. | `Cargo.toml:164` |
| `tls-rsa` | RSA cert verification for outbound HTTPS. Implies `kernel-tls`. Dropped by size/extreme. SSH is Ed25519-only and unaffected. | `Cargo.toml:171` |

### Syscall families (`sc-*`)

Per-family gates. Default-on; minimal builds use `--no-default-features` and
re-add what they need. `Cargo.toml:208-216`.

| Feature | Tier | Notes |
|---|---|---|
| `sc-aio` | 1 (dead weight) | |
| `sc-sysv-ipc` | 1 | |
| `sc-framebuffer` | 1 | |
| `sc-containers` | 1 | |
| `sc-timerfd` | 1 | |
| `sc-eventfd` | 2 (needs ExecRuntime stub when off) | |
| `sc-pidfd` | 2 | |
| `sc-epoll` | 2 | |

### Other

| Feature | Effect | Source |
|---|---|---|
| `neko` | In-kernel text editor. Dropped by size profile. | `Cargo.toml:174` |
| `sound` | virtio-sound output (`/dev/dsp`). | `Cargo.toml:180` |
| `gic-v2` | Legacy GICv2 MMIO driver instead of default GICv3. HVF needs GICv3. | `Cargo.toml:188` |
| `extreme` | Profile discriminator for build.rs (tighter IMAGE_SIZE/stack). | `Cargo.toml:196` |
| `fs-cache` | Large ext2 block cache (clock eviction) — keeps toolchain resident across spawns. Opt-in. Not combinable with `extreme`. | `Cargo.toml:203` |
| `smp` | Multikernel / one-kernel-per-core. Emits `cfg(kernel_smp)` via build.rs. Paired with `release-smp`. | `Cargo.toml:138` |
| `no-tests` | Drops boot self-test suites; sets `akuma-net/small-sockets`. | `Cargo.toml:128` |

### SMP / Big Kernel Lock

`smp` (multikernel) and `smp-shared` (real shared-kernel SMP) are the two SMP
models and are **mutually exclusive** — build.rs asserts. The `no-bkl-*` features
are carve-outs from the Big Kernel Lock and are only meaningful together with
`smp-shared`; each is a byte-for-byte no-op on any build that doesn't set both.
See [`locking.md`](locking.md) for the carve-out playbook and the syscall→lock map.

| Feature | cfg emitted | In `smp-shared` by default? | Effect |
|---|---|---|---|
| `smp-shared` | `kernel_smp_shared` | — | One kernel across all cores; activates the BKL. Paired with `release-smp-shared`. |
| `no-bkl-network` | `kernel_no_bkl_network` | **yes** (since 2026-07-24) | smoltcp net syscalls + socket `read`/`write` run BKL-free on `SOCKET_TABLE`/`NETWORK`. |
| `no-bkl-vfs` | `kernel_no_bkl_vfs` | **yes** (since 2026-07-25) | fs syscalls run BKL-free on the ext2/block-cache/fd-table spinlocks. |
| `no-bkl-process` | `kernel_no_bkl_process` | **yes** (since 2026-07-31) | `fork_process`'s CoW page-copy window runs BKL-free on the address space's `as_lock`, held in 64-page IRQ-masked chunks. Also emitted by `crates/akuma-exec/build.rs` (the only carve-out whose guard is constructed outside the bin crate). |
| `bkl-profile` | `kernel_bkl_profile` | **no — measurement only** | Per-tag BKL-hold profiler + periodic `[BKLPROF]` histogram. Perturbs timing; never ship it. |

Each carve-out also has a **runtime** toggle (default on) for same-binary A/B and
as a kill switch — `vfs_bkl_drop_enabled()`, `exec_bkl_drop_enabled()`,
`fault_bkl_drop_enabled()`, `process_bkl_drop_enabled()`, all reachable from
`src/smp_shared.rs`. Guards latch the toggle at construction and must never
re-read it in `drop()` (that unbalances the BKL ticket FIFO).

## Env vars (runtime — read by `scripts/cargo_runner.sh`)

### Memory / disk / instance

| Var | Default | Effect |
|---|---|---|
| `MEMORY` | (per script) | Guest RAM. Devbox: 4096. Self-host: 14336. Extreme: 4608K. |
| `DISK` | `disk.img` | Disk image path. |
| `INSTANCE` | `0` | Instance id (affects SSH port, logging). |
| `SNAPSHOT` | unset | `1` boots from a QEMU snapshot (writes discarded). |

### Networking

| Var | Default | Effect | Source |
|---|---|---|---|
| `RUMP_NIC` | `0` | `1` adds NIC1 on `virtio-mmio-bus.4` → `/dev/net/tap0` for the rump stack. **Required for the devbox.** | `cargo_runner.sh:153` |
| `RUMP_SSH_PORT` | `2223` | Host port forwarded to `:22` on the rump SLIRP net1. Set empty to disable the forward. | `cargo_runner.sh:164` |
| `SSH_PORT` | (derived from INSTANCE) | Host port → NIC0 `:22` (smoltcp/built-in sshd). | `cargo_runner.sh:259` |
| `TEL_PORT` / `HTTP_PORT` / `MODEL_PORT` / `P44_PORT` / `P4444_PORT` | derived | Other NIC0 hostfwd ports. | `cargo_runner.sh:259` |

### Acceleration

| Var | Default | Effect |
|---|---|---|
| `HVF` | `1` (on Apple Silicon) | `0` forces QEMU TCG (slower but faithful PC reporting; HVF gdbstub misreports PC as exception-vector entry). |
| `GDB` | unset | `1` starts QEMU gdbstub on `:1234` for `lldb -p :1234`. |

## Debug knobs (`src/config.rs`)

These are **compile-time** `pub const bool` — toggle in source and rebuild.

> **Serial output is not free.** The fork/exec/thread-spawn traces below were
> once unconditional: ~20 lines per `fork()`, 5 per `execve`, 2 per thread
> spawn — ~3.5 K lines from one short boot plus a few probes, and far more under
> an in-VM `-j4` build, which does all three continuously. Beyond the log noise
> that is enough UART time to move the timing of the very paths being traced, so
> a race can appear or vanish depending on whether tracing is on. Turn one knob
> at a time, and re-confirm a "fixed" race with tracing back off.

### Tracing

| Knob | Default | Effect | Source |
|---|---|---|---|
| `RUMP_SP_TRACE` | `false` | One line per proxied socket syscall (box, syscall, fd, result). | `config.rs:680` |
| `SYSCALL_DEBUG_INFO_ENABLED` | `false` | Full syscall tracing — **and** the `[FORK-DBG]`/`[TRAMP]` fork/exec/thread-spawn lifecycle traces (`akuma_exec::process::lifecycle_trace`). | `config.rs:316` |
| `TIMER_TICK_HEARTBEAT` | `false` | `[TMR] t=… T=… p=… f=…` from the timer IRQ every 1000 ticks — every **100** while a fork is in progress. | `config.rs:327` |
| `TRACE_TKILL` | `false` | Per-`tkill` line: target tid, caller slot, disposition, mask, blocked/fatal — plus the pending set at each syscall return. The tool for "a signal was raised but nothing happened". | `config.rs:749` |
| `FUTEX_DBG_ENABLED` | `false` | Futex diagnostics. | `config.rs:180` |
| `DEADLOCK_THREAD_DUMP_ENABLED` | `false` | Dumps all threads when a deadlock is suspected. | `config.rs:186` |
| `DEMAND_PAGE_LOG_ENABLED` | `false` | Demand-paging logs. | `config.rs:268` |
| `STDOUT_TO_KERNEL_LOG_COPY_ENABLED` | `false` | Mirrors userspace stdout into the kernel log. | `config.rs:289` |
| `MEM_MONITOR_ENABLED` | `false` | Periodic memory monitor (every `MEM_MONITOR_PERIOD_SECONDS`). | `config.rs:263` |

### Memory / stacks

| Knob | Default | Effect | Source |
|---|---|---|---|
| `USER_STACK_SIZE_OVERRIDE` | `0` (auto-scale) | Set e.g. `8MB` to debug Bun/JSC stack depth. | `config.rs:71` |
| `USER_THREAD_STACK_SIZE` | profile-dependent (512KB release / 64KB size) | Per userspace thread stack. | `config.rs:132-134` |
| `SYSTEM_THREAD_STACK_SIZE` | profile-dependent (512KB / 128KB / 96KB) | Kernel-side system thread stack. | `config.rs:106-116` |
| `MAX_ARG_STRLEN` | `128KB` release / `8KB` size / `4KB` extreme | Max single arg string (Linux = 128KB). | `config.rs:147-151` |
| `KERNEL_HEAP_SIZE_MB` | `0` (auto) | Override kernel heap size. | `config.rs:351` |
| `ENABLE_STACK_CANARIES` | `true` | Stack-overflow detection. | `config.rs:158` |

### Scheduler / concurrency

| Knob | Default | Effect | Source |
|---|---|---|---|
| `COW_FORK_ENABLED` | `true` | Copy-on-Write fork. | `config.rs:301` |
| `VFORK_FASTPATH_ENABLED` | `true` | vfork fast path. | `config.rs:311` |
| `NETWORK_THREAD_RATIO` | `4` | Scheduler weight for the network thread. | `config.rs:222` |
| `MAIN_THREAD_PRIORITY_BOOST` | `false` | Legacy; proportional scheduler is now default. | `config.rs:207` |
| `ENABLE_PREEMPTION_WATCHDOG` | `true` | | `config.rs:275` |

### Test gates (boot self-tests)

| Knob | Default | Effect | Source |
|---|---|---|---|
| `DISABLE_ALL_TESTS` | `false` | Skip all boot self-tests. | `config.rs:231` |
| `SKIP_FILESYSTEM_INIT` | `false` | | `config.rs:259` |
| `SKIP_ASYNC_NETWORK` | `false` | | `config.rs:244` |
| `RUN_NETWORK_TESTS` | `false` | | `config.rs:247` |
| `RUN_CONTAINER_TESTS` | `false` | | `config.rs:250` |
| `IGNORE_THREADING_TESTS` | `false` | | `config.rs:224` |
| `FAIL_TESTS_IF_TEST_BINARY_MISSING` | `false` | | `config.rs:196` |

## Build flags for `populate_disk.sh`

| Flag | Effect |
|---|---|
| `--apk-tools-only` | Build apk bootstrap assets only |
| `--bin-only` | Re-populate binaries only |
| `--with-apk` | Stage apk world |
| `--with-musl-dev` | Stage musl-dev (crt objects, headers) |
| `--with-rust-toolchain` | Download nightly musl-host toolchain to `/usr/local` |
| `--etc-only` | Re-populate `/etc` only |
| `--<name>-only` (via `userspace/build.sh`) | Build a single userspace member |

## Background

- `archive/OPTIONAL_SMOLTCP.md` — why smoltcp was made optional (the devbox).
- `archive/RUST_TOOLCHAIN_ISSUES.md` — why apk stable rust is used over nightly.
