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
| `release` | — | default `cargo run` / `cargo build` | full `default` feature set, including real shared-kernel SMP (`smp-shared`) |
| `extreme-size` | `release` | `scripts/build_extreme_size.sh` | `opt-level=z`, LTO, `codegen-units=1`; 4 MB floor; `--no-default-features` re-adds a minimal set |
| `release-debug` | `release` | `cargo build --profile release-debug --features ...` (manual only) | adds `debug = true` (DWARF) for source-level `lldb` against the gdbstub |

`devbox` and `devbox-smoltcp` are **not** profiles — both build on plain
`release` and are told apart entirely by feature set. See
[`../build-profiles.md`](../build-profiles.md) for the target-level view.

Source: `Cargo.toml:73-105`.

## Features

`default` (`Cargo.toml:109-124`):
```
smp-shared, smoltcp, sound, rump, fs-cache,
sc-aio, sc-sysv-ipc, sc-framebuffer, sc-containers,
sc-timerfd, sc-eventfd, sc-pidfd, sc-epoll,
many-sessions
```

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
| `rump` | Raw L2 tap path on NIC1 (`/dev/net/tap0`) for a userspace NetBSD rump stack. NIC1 only exists with `RUMP_NIC=1`. | `Cargo.toml:309` |
| `rump-default` | Makes the rump stack the **default** for box 0 (kernel brings up `/bin/rump_server` at boot). Implies `rump`. | `Cargo.toml:378` |
| `rump-tests` | Compiles only `rump_tests` even under `no-tests` — used by the devbox to verify rump regression guards at boot without pulling in the full boot-test suite. | `Cargo.toml:142` |
| `userspace-sshd` | Selects the herd-less startup path: `AUTO_START_HERD` off, kernel spawns `/bin/sshd` directly via `AUTO_START_SSHD`. Drives `config::ENABLE_USERSPACE_SSHD`. There is only ever one sshd (userspace) now — this no longer toggles between a built-in and a userspace server, only who starts the userspace one. | `Cargo.toml:386` |
| `many-sessions` | Deepens the per-listener backlog (8→32) and raises the socket budget on `small-sockets` builds — the kernel half of the process-per-session `/bin/sshd` (its `fork-sessions` feature is the userspace half). **In `default`** since 2026-08-10 — without it the stack RSTs past 8 simultaneous arrivals. Costs ~1 MB heap per listening socket + ~44 KB BSS. `kernel_profile_extreme` overrides the constants back to 8/32 regardless. | `Cargo.toml:138` |
| `devbox` | Meta-feature = `["rump-default", "userspace-sshd"]`. | `Cargo.toml:393` |
| `devbox-smoltcp` | Meta-feature = `["userspace-sshd", "smp-shared"]` — the **default** devbox. Keeps smoltcp; `smp-shared` is a no-op here since it's already in `default`. | `Cargo.toml:405` |

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
| `sound` | virtio-sound output (`/dev/dsp`). | `Cargo.toml:321` |
| `gic-v2` | Legacy GICv2 MMIO driver instead of default GICv3. HVF needs GICv3. | `Cargo.toml:329` |
| `extreme` | Profile discriminator for build.rs (tighter IMAGE_SIZE/stack). | `Cargo.toml:337` |
| `fs-cache` | Large ext2 block cache (clock eviction) — keeps toolchain resident across spawns. **In `default`**, so any build that doesn't pass `--no-default-features` has it. Cap set at mount by `src/fs.rs` as `min(RAM/8, 384 MB)`. Observe it via the `[FSCACHE]` PSTATS line. Not combinable with `extreme`. | `Cargo.toml:356` |
| `no-tests` | Drops boot self-test suites; sets `akuma-net/small-sockets`. | `Cargo.toml:125` |

`userspace-sshd` and `many-sessions` are documented under
[Networking stack](#networking-stack) above, alongside the other
sshd-related features.

#### build.rs-emitted cfgs for the above

| cfg | Emitted when | Gates |
|---|---|---|
| `kernel_tests` | `!no-tests && OPT_LEVEL != "z"` | Kernel APIs the boot suite needs — `src/main.rs`'s boot self-test suite (`{tests,process_tests,sync_tests,pthread_tests,network_tests}.rs`). There is no `kernel_builtin_ssh` cfg anymore; the in-kernel SSH server, shell, and editor it used to gate were deleted outright on 2026-08-10, not compiled-out-by-cfg (`docs/archive/BUILTIN_SSH_REMOVAL.md`). |

### SMP / Big Kernel Lock

`smp-shared` is the real shared-kernel SMP model — one shared kernel across
all cores under real cross-core locks. The `no-bkl-*` features are carve-outs
from the Big Kernel Lock and are only meaningful together with `smp-shared`;
each is a byte-for-byte no-op on any build that doesn't set both. See
[`locking.md`](locking.md) for the carve-out playbook and the syscall→lock map.

| Feature | cfg emitted | In `smp-shared` by default? | Effect |
|---|---|---|---|
| `smp-shared` | `kernel_smp_shared` | — | One kernel across all cores; activates the BKL. **In `default`** since 2026-08-10 — this is *the* SMP now, not a separate profile/CLI opt-in (the one-kernel-per-core "multikernel" `smp` feature was removed the same day). |
| `no-bkl-network` | `kernel_no_bkl_network` | **yes** (since 2026-07-24) | smoltcp net syscalls + socket `read`/`write` run BKL-free on `SOCKET_TABLE`/`NETWORK`. |
| `no-bkl-vfs` | `kernel_no_bkl_vfs` | **yes** (since 2026-07-25) | fs syscalls run BKL-free on the ext2/block-cache/fd-table spinlocks. |
| `no-bkl-process` | `kernel_no_bkl_process` | **yes** (since 2026-07-31) | `fork_process`'s CoW page-copy window runs BKL-free on the address space's `as_lock`, held in 64-page IRQ-masked chunks. Also emitted by `crates/akuma-exec/build.rs` (the only carve-out whose guard is constructed outside the bin crate). |
| `no-bkl-mm` | `kernel_no_bkl_mm` | **yes** (since 2026-08-01) | `mprotect`/`madvise`/`munmap`/`mremap`/`mmap` run BKL-free on `as_lock`/`vm_lock`/`LAZY_REGION_TABLE`/PMM/`SHARED_FILE_MAPPINGS`. Plan-driven, not attribution-driven — bin-crate-only, nothing forwarded to `akuma-exec`. |
| `no-bkl-drivers` | `kernel_no_bkl_drivers` | **yes** (since 2026-08-01) | `getrandom`, `/dev/urandom`, `/dev/dsp`, `fb_*` run BKL-free on their own driver spinlocks (`RNG_DEVICE`/`SOUND_DEVICE`/`FB_STATE`). Bin-crate-only. |
| `no-bkl-irq` | `kernel_no_bkl_irq` | **yes** (since 2026-08-01) | Timer IRQ (27) dispatch runs BKL-free — the only device IRQ this kernel registers. A/B: `irq/sched` BKL contention 24.7% → 10.2%. |
| `bkl-profile` | `kernel_bkl_profile` | **no — measurement only** | Per-tag BKL-hold profiler + periodic `[BKLPROF]` histogram. Perturbs timing; never ship it. |
| `CONSOLE_LOCK` (env) | `kernel_console_lock` | **default-on in `release`; off in `extreme-size`** | Cross-core spinlock + owner-core-ID reentrancy guard around `console::emit`'s UART write loop, so two cores under `smp-shared` can't byte-interleave each other's lines at the shared PL011 register. Default-on for `release` (OPT_LEVEL != "z") since 2026-08-11 after SMP=4 verification; off in `extreme-size` (single-core target, lock is pure overhead). `CONSOLE_LOCK=0` opt-out (debug), `CONSOLE_LOCK=1` force-on in `extreme-size` (test). Background: `docs/archive/UART_SMP_INTERLEAVE_FIX.md`. |

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
| `MEMORY` | (per script) | Guest RAM. Devbox: 4096. Self-host: 14336. Extreme: 4096K (4.0 MB floor). |
| `DISK` | `disk.img` | Disk image path. |
| `INSTANCE` | `0` | Instance id (affects SSH port, logging). |
| `SNAPSHOT` | unset | `1` boots from a QEMU snapshot (writes discarded). |

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
| `USER_THREAD_STACK_SIZE` | `cfg(kernel_profile_extreme)`-gated (512KB release / 64KB extreme-size) | Per userspace thread stack. | `config.rs:53-56` |
| `SYSTEM_THREAD_STACK_SIZE` | `cfg(kernel_profile_extreme)`-gated (512KB release / 96KB extreme-size) | Kernel-side system thread stack. | `config.rs:30-38` |
| `MAX_ARG_STRLEN` | `cfg(kernel_profile_extreme)`-gated (`128KB` release / `4KB` extreme-size) | Max single arg string (Linux = 128KB). | `config.rs:167-170` |
| `KERNEL_HEAP_SIZE_MB` | `0` (auto) | Override kernel heap size. | `config.rs:351` |
| `ENABLE_STACK_CANARIES` | `true` | Stack-overflow detection. | `config.rs:158` |

### Scheduler / concurrency

| Knob | Default | Effect | Source |
|---|---|---|---|
| `COW_FORK_ENABLED` | `true` | Copy-on-Write fork. | `config.rs:301` |
| `VFORK_FASTPATH_ENABLED` | `true` | vfork fast path. | `config.rs:311` |
| `SHARED_FILE_PAGES_ENABLED` | `true` | Share one physical frame between all read-only mappers of a file page, keyed `(inode, file_offset)`, instead of a private copy per process. Fixes the `-jN` memory/I-O amplification. Off = per-process private file pages. | `config.rs` |
| `NETWORK_THREAD_RATIO` | `4` | Scheduler weight for the network thread. | `config.rs:222` |
| `MAIN_THREAD_PRIORITY_BOOST` | `false` | Legacy; proportional scheduler is now default. | `config.rs:207` |
| `ENABLE_PREEMPTION_WATCHDOG` | `true` | | `config.rs:275` |

### Service autostart

| Knob | Default | Effect | Source |
|---|---|---|---|
| `AUTO_START_HERD` | `!(extreme && userspace-sshd)` | Spawn `/bin/herd daemon` after the network comes up. Off **only** in the extreme+`userspace-sshd` combination, where herd plus its service tree costs more RAM than a 4 MB box has to spare. | `config.rs` |
| `AUTO_START_SSHD` | `userspace-sshd && !AUTO_START_HERD` | Spawn `/bin/sshd --port 22 --shell /bin/sh` straight from `kernel_main` when there is no supervisor — otherwise the image has no way in. Never both this and herd's sshd (they would collide on the port). | `config.rs` |
| `ENABLE_USERSPACE_SSHD` | `cfg!(feature = "userspace-sshd")` | Legacy name from when there was a built-in server to suppress; that server no longer exists on any profile (deleted 2026-08-10), so this now just gates which startup path (`AUTO_START_HERD` vs `AUTO_START_SSHD`) spawns the one userspace `/bin/sshd`. | `config.rs` |

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
