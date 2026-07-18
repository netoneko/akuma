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

### SSH slow / first-connection lag (~3.4s)

Known: per-syscall sysproxy round-trip + MSG_PEEK poll + 10ms re-poll floor.
**Open** — no fix yet. See [`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md)
"Known limitations". Under CPU-bound load SSH can time out at banner exchange
(see "VM pegged at 100% CPU" below).

### Toolchain crashes

| Symptom | Cause | Fix |
|---|---|---|
| Nightly `cargo --version` faults, EC=0x0 | Nightly cargo binary traps on HVF `CNTP_*` (physical timer). `rustc` works; `cargo` doesn't. | Use **apk stable** cargo at `/usr/bin/cargo` (what `bootstrap.sh` installs). Or boot `HVF=0` (TCG). Diagnose: disassemble `ELR-4`. See `archive/RUST_TOOLCHAIN_ISSUES.md` §1. |
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
| CPU-bound load (rustc codegen) starves SSH → banner timeout | Single core; scheduler gives the compute job equal standing with rump thread + sshd. | **Open.** Likely fix: raise scheduling weight of the rump proxy thread. Meanwhile: run lighter, or accept timeouts during codegen. |
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

- `RUMP_SP_TRACE = true` — one line per proxied socket syscall.
- `SYSCALL_DEBUG_INFO_ENABLED = true` — full syscall tracing.
- `FUTEX_DBG_ENABLED` / `DEADLOCK_THREAD_DUMP_ENABLED` — futex/deadlock dumps.
- `[THR-DUMP]` heartbeat prints when ≥2 threads waiting.

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
