# Cargo features, env vars, and debug knobs

Current-state reference for everything that flips build/runtime behaviour. This
is the single place to look before toggling a knob.

See `file:line` references for the authoritative source. For the investigation
behind any of these, follow the links into `../../archive/`.

## Profiles

Codegen profiles live in `Cargo.toml`. Behaviour comes from **features**, not
profiles — the profile just sets opt level / codegen units / LTO. Only three
profiles exist since the 2026-08-10 consolidation removed five that were pure
`inherits = "release"` duplication (`size`, `release-smp`,
`release-smp-shared`, `devbox` — see
[`../build-profiles.md`](../build-profiles.md#profiles-were-consolidated-2026-08-10)).

| Profile | Inherits | Used by | Notes |
|---|---|---|---|
| `release` | — | default `cargo run` / `cargo build` | full `default` feature set (shared-kernel SMP is unconditional) |
| `extreme-size` | `release` | `cargo build --profile extreme-size …` (manual) | codegen only: `opt-level=z`, LTO, `codegen-units=1`, stripped. The same kernel as `release` for a given feature set; no cfg reads the profile since the `extreme` feature was removed 2026-11-11 |
| `release-debug` | `release` | `cargo build --profile release-debug --features ...` (manual only) | adds `debug = true` (DWARF) for source-level `lldb` against the gdbstub |

`devbox` and `devbox-smoltcp` are **not** profiles — both build on plain
`release` and are told apart entirely by feature set. See
[`../build-profiles.md`](../build-profiles.md) for the target-level view.

Source: `Cargo.toml:73-105`.

## Features

`default` (`Cargo.toml:109-124`):
```
smoltcp, sound, linux-extra-syscalls, sc-reboot
```

`linux-extra-syscalls` is the group of the seven extra families, with the same
name and members on amd64: six `sc-*` features (`sc-aio`, `sc-containers`,
`sc-timerfd`, `sc-eventfd`, `sc-pidfd`, `sc-epoll`) that stay individually
settable, plus SysV message queues and `/proc/sysvipc/msg`, which only the group
turns on (the `sc-sysv-ipc` kernel feature was folded into it 2026-11-11; the
crates keep a feature of that name). `sc-reboot` is outside it because only the AArch64
kernel has it.

There is no `neko`/`kernel-tls`/`tls-rsa` feature anymore — the in-kernel
editor, shell, and all in-kernel cryptography were deleted outright on
2026-08-10 (not gated out), see
[`../../archive/BUILTIN_SSH_REMOVAL.md`](../../archive/BUILTIN_SSH_REMOVAL.md)
and commit `bade6ab` for the earlier crypto removal. There is no in-kernel
HTTPS client anywhere in this tree now; use a userspace tool.

### Networking stack

| Feature | Effect | Source |
|---|---|---|
| `smoltcp` | Native smoltcp TCP/IP stack on NIC0; DNS/HTTP depend on it. (No longer gates a built-in SSH server — that was deleted entirely, not just from non-smoltcp builds.) | `Cargo.toml:315` |
| `rump` | **Off by default** since 2026-11-11. Raw L2 tap path on NIC1 (`/dev/net/tap0`) for a userspace NetBSD rump stack, the sysproxy client, and the rump regression guards, which every `rump` build runs at boot even with `no-tests` (the old `rump-tests`). NIC1 only exists with `RUMP_NIC=1`. Acceptance 11 builds it with box 0 on the native stack. | `Cargo.toml:309` |
| `rump-default` | Makes the rump stack the **default** for box 0 (kernel brings up `/bin/rump_server` at boot). Implies `rump`. The rump devbox's feature; it replaces the removed `devbox` bundle. A separate feature because the AArch64 kernel reads no boot command line to choose it at runtime. | `Cargo.toml:378` |
| (`userspace-sshd`) | **Removed 2026-11-11** with `extreme`, the only build that selected it. herd starts `/bin/sshd` on every build. |
| (`many-sessions`) | **Removed 2026-11-11.** The listener backlog is 8 on every build; configure sshd for it or raise `MAX_BACKLOG` — [`../build-profiles.md`](../build-profiles.md) § Listener backlog. |
| (`devbox-smoltcp`) | **Removed 2026-11-11** (it had become an empty alias). The default devbox is `--release --features no-tests`; `scripts/build_devbox_smoltcp.sh` builds it. | `Cargo.toml` |

> **Drift note:** `overlays/devbox/README.md:142,211`
> still say "Phase 2 will build with `--no-default-features`" and "smoltcp is
> still compiled in (for now)". That is **stale** — `scripts/build_devbox.sh`
> and `overlays/devbox/run.sh` already pass `--no-default-features`, so
> smoltcp is compiled out entirely in the devbox today.

### Syscall families (`sc-*`)

Per-family gates. Default-on; minimal builds use `--no-default-features` and
re-add what they need. `Cargo.toml:361-369`.

| Feature | Tier | Notes |
|---|---|---|
| `sc-aio` | 1 (dead weight) | |
| (`sc-sysv-ipc`) | 1 | Folded into `linux-extra-syscalls` 2026-11-11; still a feature of `akuma-syscalls-glue`/`akuma-vfs-glue`. |
| `sc-containers` | 1 | |
| `sc-timerfd` | 1 | |
| `sc-eventfd` | 2 (needs ExecRuntime stub when off) | |
| `sc-pidfd` | 2 | |
| `sc-epoll` | 2 | |
| `sc-reboot` | — | **In `default`** since 2026-08-25 (was devbox-only before — the plan had been to land it alongside a self-hosted kexec, which was scoped and rejected, so there was no reason left to gate it). Linux `reboot(2)` (`nr` 142) → PSCI `SYSTEM_RESET`/`SYSTEM_OFF`. ABI decode in `crates/akuma-boot` (host-tested); the PSCI call reuses `smp_shared`'s existing SMC/HVC conduit code, so `sc-reboot` depends on `smp-shared`. **Box-0 only**: `caller_may_reboot` (`src/syscall/reboot.rs`) returns `EPERM` for any caller with `box_id != 0` — a whole-machine PSCI reset/off would take every other box down with it, so it gets the same host-only restriction `mount`/`umount` already have (`caller_may_mount`, `src/syscall/container.rs`). See [`syscalls/proc.md`](syscalls/proc.md#reboot) and [`../../archive/AKUMA_BOOT_EXTRACTION.md`](../../archive/AKUMA_BOOT_EXTRACTION.md). |

### Other

| Feature | Effect | Source |
|---|---|---|
| `sound` | virtio-sound output (`/dev/dsp`). The only thing that still compiles `virtio-drivers` (and `zerocopy`, `safe-mmio`, `bitflags` 2, `thiserror`): block and net are driven in-tree (`archive/AKUMA_VIRTIO_IN_TREE.md`). | `Cargo.toml` |
| (`extreme`) | **Removed 2026-11-11** — it selected the 4 MB-floor constants (stacks, heap seed, socket table, no block cache). The `extreme-size` profile stays, codegen only. [`../../archive/EXTREME_SIZE_BUILD.md`](../../archive/EXTREME_SIZE_BUILD.md). | |
| (`fs-cache`) | **Folded in 2026-11-11**: the ext2 clock-eviction block cache is on in every build (both kernels, Taoxichuan, host tests). Cap set at mount by `src/fs.rs` as `min(RAM/8, 384 MB)`, 16 MB if the kernel never sets one. Observe it via the `[FSCACHE]` PSTATS line. | `crates/akuma-ext2` |
| `no-tests` | Drops boot self-test suites. | `Cargo.toml:125` |

### Crate features the amd64 kernels leave off

Not bin features: each is a crate's own, turned on by the AArch64 kernel's manifests and left off by `akuma-amd64` (its library, `akuma-kernel-amd64`) and
Taoxichuan, which load binaries with their own loaders and reach `akuma-exec` only through `Process::install_image`.

| Feature | Effect | Source |
|---|---|---|
| `akuma-exec/elf-loader` | The ELF loader: the `akuma-elf` (+ `elf`) dependency and its `elf_loader` re-export, `Process::from_elf*`, `replace_image*`, the `spawn_process*` / `exec*` helpers, and `embedded-io-async`. Off, `ImageInstall::deferred_segments` takes an uninhabited `DeferredLazySegment`, so it can only be `&[]`. Turned on by the bin, `akuma-kernel-glue` (herd/sshd/rump spawns) and `akuma-exceptions` (`INTERP_BASE`). Off saves Taoxichuan ~5,400 audit lines. | `crates/akuma-exec/Cargo.toml` |
| `akuma-syscalls-glue/exec` | `execve` and the native `SPAWN`/`SPAWN_EXT` (and their helpers); forwards `akuma-exec/elf-loader`. In glue's `default`, and named explicitly by the bin (which uses `default-features = false`). Off, the three numbers fall to `ENOSYS`. | `crates/akuma-syscalls-glue/Cargo.toml` |

### Crate features Taoxichuan leaves off

Turned on by both other kernels, left off by Taoxichuan, which serves only the Linux ABI through `akuma-runtime-tokio`.
What a tokio program calls, and so what may be left off, is measured by B10's census
([`../../archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md`](../../archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md)).

| Feature | Effect | Source |
|---|---|---|
| `akuma-syscalls-glue/akuma-native` | Akuma's own numbers (asm-generic 300-328; `0x1000 +` that on x86_64): `spawn`, `spawn_ext`, `waitpid`, `kill`, the terminal and cursor calls, `resolve_host`, `uptime`, `time`, `get_version`, `core_init`, `set_box_stack`, `close_child_stdin`, `set_tpidr_el0`. libakuma's programs use them. In glue's `default`; named by the bin and `akuma-amd64`. Off, the numbers fall to `ENOSYS`. `spawn`/`spawn_ext` also need `exec`. | `crates/akuma-syscalls-glue/Cargo.toml` |
| `akuma-runtime-tokio/shell` | Not a tokio feature: the 23 calls glue serves that a shell and its tools make and no tokio program does (job control, the other ids, limits and accounting, `sync`, `membarrier`, the clock and hostname setters). Taoxichuan's own `shell` feature (in its `default`) turns it on; `--no-default-features` builds a kernel that serves only the tokio surface. | `crates/akuma-runtime-tokio/Cargo.toml`, `taoxichuan/Cargo.toml` |

### The `akuma-amd64` package

Two packages, both deliberately outside `default-members`, so none of the features
above apply to them and none of these apply to the AArch64 kernel. The features are
defined by the kernel library, `akuma-kernel-amd64` (`amd64/Cargo.toml`). The bin,
`akuma-amd64` (`amd64/bin/Cargo.toml`), is `akuma_kernel::entry!()` and forwards
every one of them under the same name, so `-p akuma-amd64 --features X` works. A
feature added to the library needs its forwarding line in the bin; if it is missing,
cargo reports an error rather than silently ignoring the feature.

| Feature | Effect | Source |
|---|---|---|
| *(no SMP feature)* | Shared-kernel SMP is unconditional: `amd64/Cargo.toml` turns on `bkl-smp` on every shared crate it depends on, and `const _: () = assert!(akuma_bkl::ACTIVE)` in `amd64/src/smp.rs` fails the build if one is ever missed (the BKL's entry points are no-ops without it). The `smp-shared` feature was removed 2026-11-11. | `amd64/Cargo.toml` |
| `no-tests` | Drops the boot self-test suite, as on AArch64. `boot::self_tests` and every `*_test`/`smoke_test` function in `amd64/src` disappear and `boot::late_init` performs the bring-up the suite otherwise does on the way past (the LAPIC, the secondaries, the console fd, the syscall MSRs, the timer, `sti`) — the same call the runtime `skiptests` command-line lever makes. Verified by booting, not only by compiling. Forwards to `akuma-kernel-core`, `akuma-exec` and `akuma-syscalls-glue`, as AArch64's does, so their test-only helpers leave the image too (−1.9 KB `.text`). | `amd64/Cargo.toml` |
| `syscall-debug-info` | Off. Forwards the AArch64 kernel's compile-time syscall debug mode (`akuma-config`, `akuma-syscalls`, `akuma-syscalls-time`, `akuma-exec`). On amd64 it prints glue's per-arm `[syscall]` lines only; the `[FORK-DBG]`/`[TRAMP]` traces never fire, because fork and exec are this target's own. For "which syscall failed" use the runtime `strace_err` flag instead: [`../../runbooks/trace-failing-syscalls-amd64.md`](../../runbooks/trace-failing-syscalls-amd64.md). | `amd64/Cargo.toml` |

| Env knob | Effect | Source |
|---|---|---|
| `FEATURES=a,b` | Extra cargo features for `amd64/run.sh` — `FEATURES=no-tests sh amd64/run.sh`. Without it that feature could only be compiled, never booted, and a configuration nobody boots is one that rots. | `amd64/run.sh` |

`no-tests` is also what makes `scripts/cloc_akuma.py` able to tell this target's
kernel code from its test code: the script files an item under *tests* exactly
when its `cfg` cannot hold in a `no-tests` world. Before the feature existed
`amd64/src` measured **0.0% test** against ~3 300 lines of boot suite.

#### build.rs-emitted cfgs for the above

| cfg | Emitted when | Gates |
|---|---|---|
| `kernel_tests` | `!no-tests` | Kernel APIs the boot suite needs — `src/main.rs`'s boot self-test suite (`{tests,process_tests,sync_tests,pthread_tests,network_tests}.rs`). There is no `kernel_builtin_ssh` cfg anymore; the in-kernel SSH server, shell, and editor it used to gate were deleted outright on 2026-08-10, not compiled-out-by-cfg (`docs/archive/BUILTIN_SSH_REMOVAL.md`). |

### SMP / Big Kernel Lock

Every Akuma build is the real shared-kernel SMP model — one shared kernel across
all cores under real cross-core locks. It also runs five syscall families without
the Big Kernel Lock: network, VFS, fork's page copy, mm and the drivers. And it makes
`PreemptGuard` mask local IRQs for inner-lock holds, which is what makes those
carve-outs safe. There is no compile-time switch per family. The six `no-bkl-*`
features were folded into what is now `bkl-smp` on 2026-11-11, because every build of both
kernels had them all on; `no-bkl-irq` had gated nothing since the SGI scheduler was
deleted (`docs/archive/AKUMA_FEATURE_AUDIT.md` F2). A/B a family with its **runtime**
toggle, described below. See [`locking.md`](locking.md) for the carve-out playbook
and the syscall→lock map.

| Feature | cfg emitted | In `default`? | Effect |
|---|---|---|---|
| `bkl-smp` (shared crates only) | `kernel_bkl_smp` | — | The BKL is real and the shared crates open its dropped windows. **Not a kernel feature**: both Akuma kernels enable it on every shared crate unconditionally (the root `Cargo.toml` through `akuma-kernel-glue`, `amd64/Cargo.toml` directly) and assert `akuma_bkl::ACTIVE`; Taoxichuan leaves it off and serializes under its own lock ([`../../../proposals/ONE_BIG_KERNEL_LOCK.md`](../../../proposals/ONE_BIG_KERNEL_LOCK.md)). Was the `smp-shared` feature / `kernel_smp_shared` until 2026-11-11, when the single-core AArch64 build was removed. |
| `bkl-profile` | `kernel_bkl_profile` | **no — measurement only** | Per-tag BKL-hold profiler + periodic `[BKLPROF]` histogram. Perturbs timing; never ship it. |
| `read-profile` | *(feature only)* | **no — measurement only** | Per-stage `read(2)` accounting: `sys_read`'s `File` arm, `handle_syscall` and the EL0 handler each time themselves, and a `[READPROF]` window prints every 256 file reads — which stage of a warm read is validation, fd lookup, the staging allocation, ext2, the user copy, or the two wrappers. Answers what neither `bkl-profile` ("which syscall held the lock") nor `net-profile` ("how long was the device busy") can: the split *inside* one syscall. **Single-core only** (the wrapper spans are handed up by one flag — run `SMP=1`), and **never read wall-clock throughput off this build**: the dump's own console write is ~55 ms per window. Use `scripts/benchmarks/read_stage_profile.py`, which drops windows with mixed request sizes or preempted samples. Background: `docs/archive/EXT2_READ_PATH_STAGE_PROFILE.md`. |
| `CONSOLE_LOCK` (env) | `kernel_console_lock` | **default-on** | Cross-core spinlock + owner-core-ID reentrancy guard around `console::emit`'s UART write loop, so two cores under `smp-shared` can't byte-interleave each other's lines at the shared PL011 register. On in every build since 2026-11-11 (it was off under `opt-level = "z"`, the old single-core `extreme` target). `CONSOLE_LOCK=0` opt-out (debug). Background: `docs/archive/UART_SMP_INTERLEAVE_FIX.md`. |
| `sound` + platform | `kernel_audio` | **on for QEMU virt; off for `platform-firecracker`** | The virtio-sound driver and `/dev/dsp`. Firecracker implements net/block/balloon/vsock/rng and no sound device — confirmed by the FDT it emits (`docs/reference/firecracker/fdt/`), which carries three virtio nodes, none of them sound. With this off, `src/main.rs` provides a stub `audio` module mirroring `akuma-virtio`'s own feature-off `imp`, so the syscall layer compiles unchanged and `/dev/dsp` never opens (`is_available()` is false, the gate `syscall/fs.rs` already applied). The boot probe is skipped too: walking eight virtio slots to report "not available" every boot is noise. Asserted by `test_platform_device_gates`. |

Each carve-out also has a **runtime** toggle (default on) for same-binary A/B and
as a kill switch — `set_fault_bkl_drop_enabled()`, `set_exec_bkl_drop_enabled()`,
`set_vfs_bkl_drop_enabled()`, `set_process_bkl_drop_enabled()`,
`set_mm_bkl_drop_enabled()`, `set_drivers_bkl_drop_enabled()`,
`set_irq_bkl_drop_enabled()`, all reachable from `src/smp_shared.rs`. Guards
latch the toggle at construction and must never re-read it in `drop()` (that
unbalances the BKL ticket FIFO).

## Env vars (runtime — read by `scripts/cargo_runner.sh`)

### Memory / disk / instance

| Var | Default | Effect |
|---|---|---|
| `MEMORY` | (per script) | Guest RAM. Devbox: 4096. Self-host: 14336. |
| `DISK` | `disk.img` | Disk image path. |
| `INSTANCE` | `0` | Instance id (affects SSH port, logging). |
| `SNAPSHOT` | unset | `1` boots from a QEMU snapshot (writes discarded). |
| `KERNEL_DROPOFF` | `0` | `1` also mounts `$BIN` (the file `-kernel` loads) as a second `virtio-blk` drive on `bus.6`, adds `-action reboot=shutdown`, and replaces the trailing `exec qemu` with a relaunch loop — so a guest that self-hosts a build (`docs/runbooks/selfhost-kernel-build.md`) can `dd` the new kernel over the file and `reboot(2)` (`sc-reboot`) straight into it. **Working end-to-end** as of 2026-08-25 — `/dev/vdX` raw block-device fds landed ([`../../archive/RAW_BLOCK_DEVICE_FD.md`](../../archive/RAW_BLOCK_DEVICE_FD.md)), closing the "no `/dev`" blocker [`../../archive/AKUMA_BOOT_EXTRACTION.md`](../../archive/AKUMA_BOOT_EXTRACTION.md) §5 used to describe. `$BIN` is also padded up to `$SIZE_LIMIT` (the release size-guard ceiling, 4 MB) before being mounted, so an in-guest rebuild that's bigger than *this boot's* kernel still fits the fixed virtio-blk capacity instead of `ENOSPC`ing mid-`dd` and corrupting the live (unsnapshotted) `$BIN` — see `docs/runbooks/selfhost-kernel-build.md` §7. | `cargo_runner.sh` |

### Networking

| Var | Default | Effect | Source |
|---|---|---|---|
| `RUMP_NIC` | `0` | `1` adds NIC1 on `virtio-mmio-bus.4` → `/dev/net/tap0` for the rump stack. **Required for the devbox.** | `cargo_runner.sh:153` |
| `RUMP_SSH_PORT` | `2223` | Host port forwarded to `:22` on the rump SLIRP net1. Set empty to disable the forward. | `cargo_runner.sh:164` |
| `SSH_PORT` | (derived from INSTANCE) | Host port → NIC0 `:22` (smoltcp, userspace `/bin/sshd`). | `cargo_runner.sh:259` |
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
| `PIPE_TRACE_ENABLED` | `false` | `[pipe] create` / `clone_ref` / `close_write` / `close_read` refcount lines. Unconditional until 2026-08-08; 6.6k lines in one `-j4` build. These are how the SIGPIPE/close-ordering deadlocks were cracked, so they are one flag away, not gone. `WARN`/`DESTROY` are not gated. | `config.rs:341` |
| `DEMAND_PAGE_LOG_ENABLED` | `false` | The per-fault `[IA-DP] file region:` trace. **The const existed and was documented here, but had no reader anywhere in the tree** until 2026-08-08 — it gated nothing, while that line printed unconditionally at 34.7k lines per `-j4` build (the single largest source). Now wired to it. Its old docstring claimed `[DA-DP]`/`[DP]`/`[DP-eager]`; those are *anomaly* lines (pool exhausted, OOM, region miss) and stay unconditional by design. | `config.rs:332` |
| `MEM_SYSCALL_TRACE_ENABLED` | `false` | `[mmap]` / `[mprotect]` per-call lines. **Unconditional until 2026-08-08**, which cost a `-j4` self-host build 68 MB of serial output in 20 minutes (~270 KB/s through one console, four cores contending) and stretched a ~10-minute build past an hour. Turn it on to debug mmap itself, never for timing work. Failures (`EINVAL`, region complaints) are not gated and stay visible. | `config.rs:354` |
| `SYSCALL_DEBUG_EPOLL_EDGE` | `false` | One `[epoll] ET epfd=… fd=… rev=… last=… new=… deliver\|SUPPRESSED` line per ready fd per `epoll_pwait` scan. The flag for a **lost edge** — the failure where the fd is permanently ready, the watcher is parked forever, and nothing else in the log looks wrong. A first `deliver` followed by an unbroken run of `SUPPRESSED` on one fd *is* the bug. Independent of `SYSCALL_DEBUG_NET_ENABLED`, which buries the same trace in TCP/UDP/DNS noise. Added by [`../../archive/TOKIO_PIPE_EPOLL_HANG.md`](../../archive/TOKIO_PIPE_EPOLL_HANG.md); used by [`../../runbooks/debug-async-subprocess-hang.md`](../../runbooks/debug-async-subprocess-hang.md) step 4. | `config.rs` |
| `SYSCALL_DEBUG_NET_ENABLED` | `false` | Verbose network/epoll tracing: `epoll_pwait` returns (sampled, see `EPOLL_ZERO_SAMPLE_INTERVAL`), UDP recv/send, DNS. Note the `interest_fds=` field in `[epoll] pwait ret` is **always 0** — its only caller passes a literal — so it means nothing. | `config.rs` |
| `TIMER_TICK_HEARTBEAT` | `false` | `[TMR] t=… T=… p=… f=…` from the timer IRQ every 1000 ticks — every **100** while a fork is in progress. | `config.rs:327` |
| `TRACE_TKILL` | `false` | Per-`tkill` line: target tid, caller slot, disposition, mask, blocked/fatal — plus the pending set at each syscall return. The tool for "a signal was raised but nothing happened". | `config.rs:749` |
| `FUTEX_DBG_ENABLED` | `false` | Futex diagnostics. | `config.rs:180` |
| `DEADLOCK_THREAD_DUMP_ENABLED` | `false` | Dumps all threads when a deadlock is suspected. | `config.rs:186` |
| `STDOUT_TO_KERNEL_LOG_COPY_ENABLED` | `false` | Mirrors userspace stdout into the kernel log. | `config.rs:289` |
| `MEM_MONITOR_ENABLED` | `false` | Periodic memory monitor (every `MEM_MONITOR_PERIOD_SECONDS`). | `config.rs:263` |

### Memory / stacks

| Knob | Default | Effect | Source |
|---|---|---|---|
| `USER_STACK_SIZE_OVERRIDE` | `0` (auto-scale) | Set e.g. `8MB` to debug Bun/JSC stack depth. | `config.rs:71` |
| `USER_THREAD_STACK_SIZE` | 512 KB | Per userspace thread stack. | `crates/akuma-config` |
| `SYSTEM_THREAD_STACK_SIZE` | 512 KB | Kernel-side system thread stack. | `crates/akuma-config` |
| `MAX_ARG_STRLEN` | 128 KB | Max single arg string (Linux = 128KB). | `crates/akuma-config` |
| `KERNEL_HEAP_SIZE_MB` | `0` (auto) | Override kernel heap size. | `config.rs:351` |
| `ENABLE_STACK_CANARIES` | `true` | Stack-overflow detection. | `config.rs:158` |

### Scheduler / concurrency

| Knob | Default | Effect | Source |
|---|---|---|---|
| `COW_FORK_ENABLED` | `true` | Copy-on-Write fork. | `config.rs:301` |
| `VFORK_FASTPATH_ENABLED` | `true` | vfork fast path. | `config.rs:311` |
| `SHARED_FILE_PAGES_ENABLED` | `true` | Share one physical frame between all read-only mappers of a file page, keyed `(inode, file_offset)`, instead of a private copy per process. Fixes the `-jN` memory/I-O amplification. Off = per-process private file pages. | `config.rs` |
| `FPCACHE_BASE_RAM_DIVISOR` | `8` | Base file-page cache cap as a divisor of total RAM. 8 = RAM/8 = 512 MB at `MEMORY=4096`. Lower it to starve the cache for an A/B. | `config.rs` |
| `FPCACHE_INFLATE_PCT` | `20` | Extra cap allowed **on top of the base**, as a percent of the base, granted only while free RAM can spare it. 614 MB at `MEMORY=4096` — enough to hold a 532 MB mmap'd model that the base cap misses by 20 MB. `0` pins the cap at the base (old fixed behaviour). | `config.rs` |
| `FPCACHE_INFLATE_HEADROOM_MULT` | `2` | Free RAM demanded before the inflation is granted, as a multiple of the inflation. Withdrawal happens at `1x`, so the two form a hysteresis band and the cap cannot flap. | `config.rs` |
| `NETWORK_THREAD_RATIO` | `4` | Scheduler weight for the network thread. | `config.rs:222` |
| `MAIN_THREAD_PRIORITY_BOOST` | `false` | Legacy; proportional scheduler is now default. | `config.rs:207` |
| `ENABLE_PREEMPTION_WATCHDOG` | `true` | | `config.rs:275` |

### Service autostart

| Knob | Default | Effect | Source |
|---|---|---|---|
| `AUTO_START_HERD` | `true` | Spawn `/bin/herd daemon` after the network comes up; herd starts `/bin/sshd`. (The herd-less `AUTO_START_SSHD` path went with the `extreme`/`userspace-sshd` features, 2026-11-11.) | `crates/akuma-config` |

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
- `archive/RUST_TOOLCHAIN_ISSUES.md` — the original nightly-cargo-crash
  investigation (superseded — see `archive/NIGHTLY_CARGO_HVF_SIGILL.md` for
  the actual root cause + fix, 2026-08-06; nightly and apk-stable rust now
  both work and ship side by side on the devbox, see `runbooks/build-devbox.md`).
