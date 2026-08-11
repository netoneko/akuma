# Debug the devbox

Symptom-driven debugging for the devbox (rump-only build). Start from the
symptom on the left. For build/verify steps, see
[`build-devbox.md`](build-devbox.md); for recovery when the VM is wedged, see
[`recover-wedged-vm.md`](recover-wedged-vm.md).

## Reading the logs

- **QEMU serial console** (stdout) is the primary kernel log. QEMU/HVF emits
  a control byte that makes plain `grep` treat the log as binary — use
  `grep -a`.
- **rump_server log** (in-VM): `/var/log/box/0/rump_server.log`.
- **Boot self-tests** (`src/process_tests.rs`, `src/pthread_tests.rs`) halt on
  regression.

## Symptom → cause → fix

### SSH unreachable

| Likely cause | How to check | Fix |
|---|---|---|
| Forgot `RUMP_NIC=1` (no tap0) | Log: `rump-default: no NIC1 (/dev/net/tap0) — box 0 stays on native stack` | Boot via `overlays/devbox/run.sh` (sets it) or `export RUMP_NIC=1` |
| box 0 rump stack not up yet | No `box=0 proxy ready` line | Wait ~5-10s (DHCP + handshake). herd sshd has `start_delay_ms=10000` + `restart=true` |
| Wrong SSH port | `ssh -p 22` | Use `-p 2223` (or `RUMP_SSH_PORT`) |
| Host key changed after rebuild | `REMOTE HOST IDENTIFICATION HAS CHANGED` | `ssh-keygen -R "[localhost]:2223"` |
| Built-in SSH started instead of userspace | Log lacks `[Main] Built-in SSH server disabled` | Confirm `userspace-sshd` feature is on (`devbox` feature implies it) |
| Port 2223 busy on host | `ssh` connects then drops | `pkill -9 qemu-system-aarch64` or set `RUMP_SSH_PORT` |

### SSH slow / first-connection lag

**Mostly fixed (2026-07-18).** Was ~3.4–4.0s; now ~1.8–1.85s for a one-shot
`ssh ... <cmd>`, and multi-write bursts (a TUI redraw, streaming output) that
used to be pathologically slow (Nagle + no `TCP_NODELAY`) are now 3–4x
faster. See `archive/RUMP_SYSPROXY_LATENCY_FIX.md` Phases 2a/2b/3a/3b for the
fixes (network-thread scheduling boost, rump-aware poll cadence, tap-fd poll
classification, forced `TCP_NODELAY` on every rump TCP socket).

**Still open**: a ~300ms floor on a single synchronous round-trip (one
keystroke echo doesn't benefit from the above — it was never Nagle- or
poll-cadence-bound). Phase 3e root-caused this to round-robin contention
among `rump_server`'s fibers (many are simultaneously runnable;
`schedule()`'s idle-sleep path is never even entered), and proved — via a
fast, host-testable benchmark (`userspace/rumpkernel/test-fiber.sh`,
no QEMU/no rump kernel needed) — that the fiber *scheduler mechanism* itself
is not the cost (microseconds even at 50 competing fibers). The actual cost
is in whatever work each NetBSD kthread-as-fiber does once scheduled; that's
unidentified. See `archive/RUMP_SYSPROXY_LATENCY_FIX.md` Phase 3e/3f.

### Toolchain crashes

| Symptom | Cause | Fix |
|---|---|---|
| Nightly `cargo --version`/`cargo build` faults, `EC=0x0` at a constant `ELR` (historical) | **FIXED 2026-08-06.** OpenSSL's `OPENSSL_cpuid_setup` armcaps probe executes `SM3SS1` (FEAT_SM3), which Apple Silicon's HVF lacks; the probe expects `SIGILL` to detect the missing feature and recover, but the kernel's `EC=0x0` handler hard-killed the process instead of delivering it. (The old "traps HVF `CNTP_*`" reading was a misattribution — that would be `EC=0x18`.) `rustc` alone was never affected. | Kernel now delivers `SIGILL` via `try_deliver_signal` in `src/exceptions.rs`'s `EC=0x0` arm. Nightly `cargo` (`/usr/local/bin/cargo`, installed by `bootstrap.sh` step 7b, default on) now runs normally — re-verified 2026-08-11: `cargo new`/`cargo build`/running the binary all succeeded on devbox-smoltcp. Not re-tested on the rump path (`run.sh`). Full writeup: `archive/NIGHTLY_CARGO_HVF_SIGILL.md`; also `selfhost-kernel-build.md` §6. `archive/RUST_TOOLCHAIN_ISSUES.md` §1 is the original, now-superseded investigation. |
| apk rustc: "chunk header is zero" (Scudo heap corruption) under release/LTO build of a real crate | Unconfirmed kernel brk/CoW race serving a fresh zero page | `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_LTO=false cargo build --release`. See `archive/RUST_TOOLCHAIN_ISSUES.md` §2. |
| `cargo build`/`rustc` can't spawn subprocess: `os error 95` before any `clone()` | `socketpair()` (199) was claimed by the rump proxy but had no dispatch arm → `EOPNOTSUPP`. | **FIXED.** AF_UNIX socketpairs excluded from proxying. See `archive/OPTIONAL_SMOLTCP.md`. |
| `cargo build`/`rustc` CLOEXEC-pipe `EBADF` after fork | `FileDescriptor::UnixSocket` fds across fork in `fork_process`/`vfork_process`/`execve` | See `archive/OPTIONAL_SMOLTCP.md` (in-progress). |

### `git clone` hangs or wedges

| Symptom | Cause | Fix |
|---|---|---|
| `git clone` hangs the VM | Stale-TTBR0 bug in `clone_thread` / `fork_process` / **`vfork_process`** (musl `posix_spawn` uses CLONE_VFORK → vfork). | **FIXED** (all three call sites, 2026-07-05). The `vfork_process` site is the one `git clone` actually hits. |
| `git clone` fails clean: `Could not contact DNS servers` (c-ares) | `rump_fcntl` returned `EOPNOTSUPP` for `F_SETFD`; c-ares treats as fatal. | **FIXED.** `F_GETFD`/`F_SETFD` are now no-op success. See `archive/OPTIONAL_SMOLTCP.md`. |
| `git clone` very slow (>2min for tiny repo) | Per-syscall rump proxy round-trip; serialized. | **Open.** Measure against native smoltcp first. See `archive/OPTIONAL_SMOLTCP.md` backlog. |

### VM pegged at 100% CPU

| Symptom | Cause | Fix |
|---|---|---|
| Idle VM at ~100% CPU, responsive | **FIXED (2026-07-06/07).** (1) tap-poll busy-spin in `rumpcomp_tap.c` RX fiber; (2) BSP idle threads busy-yielding instead of `WFI`. | If it recurs, check `rumpcomp_tap.c` and `idle_halt()` in `src/main.rs`. See `archive/KNOWN_ISSUES.md` §10-11. |
| CPU-bound load (rustc codegen) starves SSH → banner timeout | Single core; rump_server waited many scheduler quanta for a timeslice. | **FIXED (2026-07-18).** `start_default_stack` registers rump_server's actual TID (not a parked kthread) for the `NETWORK_THREAD_RATIO` boost. See `archive/RUMP_SYSPROXY_LATENCY_FIX.md` Phase 2a. If it recurs, check `threading::set_network_thread_id` is still called with the right TID. |
| Shell pipeline `cmd \| head -N` wedges VM at ~99%, `[signal] tkill(tid=X, sig=13)` | SIGPIPE delivery to the writer spins instead of terminating the write syscall. | **Open.** Workaround: redirect to a file instead of piping through `head`/`tail`. |

### Network doesn't work

| Symptom | Cause | Fix |
|---|---|---|
| `curl https://...` fails cert verification | CA bundle missing | Confirm `DEVBOX_CA_CERTS=true` (default) in `bootstrap.sh`; check `/etc/ssl/certs/ca-certificates.crt` exists. |
| `curl <hostname>` crashed DNS resolver / wedged VM | Stale-TTTR0 in `clone_thread` (curl AsynchDNS path) | **FIXED** (same bug class as `git clone`). |
| `nslookup`/`curl` work but c-ares (git) didn't | c-ares calls `fcntl(F_SETFD)` | **FIXED** (see git row above). |
| No IP on virt0 | DHCP didn't complete | Check `box=0 proxy ready` in log; check `/var/log/box/0/rump_server.log`. |

### Only one SSH session / parallel shells hang

**FIXED** — three bugs: sshd's accept loop was serial; kernel rejected
`fcntl(O_NONBLOCK)` on rump sockets; a blocking `sleep_ms` in an `async fn`
starved the multiplexer. Residual: single box-0 proxy serializes syscalls
(head-of-line blocking under truly simultaneous sessions). See
`archive/OPTIONAL_SMOLTCP.md` "Concurrent SSH".

## Debug knobs to flip

All in `src/config.rs` (compile-time; rebuild after). See
[`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md):

- `RUMP_SP_TRACE = true` — one line per proxied socket syscall (`[RUMP-SP] ...`,
  includes per-call `us`/`hops`/`blk` timing). **Revert to `false` before
  shipping** — both this and the next flag were left on after the 2026-07-18
  session and flooded the console (10k+ lines/boot) until caught; neither is
  meant to stay on.
- `SYSCALL_DEBUG_NET_ENABLED = true` — verbose network/epoll/ppoll tracing
  (`[ppoll] enter: ...` etc.). Same "revert after use" warning as above.
- `SYSCALL_DEBUG_INFO_ENABLED = true` — full syscall tracing.
- `FUTEX_DBG_ENABLED` / `DEADLOCK_THREAD_DUMP_ENABLED` — futex/deadlock dumps.
- `[THR-DUMP]` heartbeat prints when ≥2 threads waiting.

**`rump_server`'s own internals** are separate from the above (those are all
kernel-side `src/config.rs` flags; `rump_server` is a userspace binary built
from `userspace/rumpkernel/`, not part of the kernel build):
- `rumpuser_debug` Cargo feature (`userspace/rumpkernel/rumpuser/Cargo.toml`)
  — traces every rumpuser hypercall. Under the default fiber backend this only
  covers what `fiber.rs` explicitly instruments (`schedule()`'s idle path,
  `wait()`/`wakeup_all` — added 2026-07-18); the old pthread-backend `tr!`
  traces in `lib.rs` don't fire under fiber. **Keep this rate-limited** — an
  earlier unconditional per-event version made `rump_server` spend ~98% of its
  CPU time in `write()` tracing itself (see `archive/RUMP_SYSPROXY_LATENCY_FIX.md`
  Phase 3e). Rebuild via `docker-build-rump-server.sh` with the feature added
  to the `cargo build` line, deploy with
  `DISK=devbox.img scripts/populate_disk.sh --overlay <dir with bin/rump_server>`
  (reversible, no base-image rebuild), read output from
  `/var/log/box/0/rump_server.log` (its `--log` flag redirects its own
  stdout+stderr there, not the kernel console).
- `userspace/rumpkernel/test-fiber.sh` — cross-builds and runs `fiber.rs`'s
  own `#[cfg(test)]` suite (cross-build + Docker linux/arm64, no rump kernel,
  no QEMU, no disk image — seconds, not minutes). The right place to
  reproduce/benchmark fiber-scheduler behavior in isolation before touching
  the live stack; see `round_robin_contention_scales_with_fiber_count` for an
  example (N concurrent cv_wait/broadcast pairs, timing one tracked pair).

Runtime: `HVF=0` forces TCG (faithful PC; HVF gdbstub misreports PC as
exception-vector entry). `GDB=1` for QEMU gdbstub on `:1234` →
`lldb -p :1234`.

In-VM: `ps` builtin prints each process's saved kernel resume point
(`x30`/`elr`) + `current_syscall` — useful for locating a wedged thread.

## Background

- `archive/RUST_TOOLCHAIN_ISSUES.md` — nightly cargo crash, Scudo corruption.
- `archive/KNOWN_ISSUES.md` §10-11 — the two 100%-CPU bugs (FIXED).
- `archive/OPTIONAL_SMOLTCP.md` — the rump-path bug-fix history.
- `archive/FIBER_HANDOFF.md` — rump latency root cause + open items.
- `archive/RUMP_SYSPROXY_LATENCY_FIX.md` — the full Phase 2/3 latency fix
  history (scheduling boost, poll cadence, `TCP_NODELAY`, the fiber-contention
  root-cause investigation) — current as of 2026-07-18.
