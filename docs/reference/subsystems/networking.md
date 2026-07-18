# Networking

Current-state architecture for how packets and socket syscalls flow in Akuma.
For the rump stack internals see [`rump-stack.md`](rump-stack.md).

## The box model

Akuma routes AF_INET (socket-family) syscalls **per box**, keyed on a process's
`box_id`.

- **Box 0** is the root box every process starts in.
- A process can spawn into or `join_box` into another box for isolation.
- Each box has its own network stack assignment: **native (smoltcp)** or
  **rump**.
- The kernel's dispatch hook (`intercept_box_syscall` in `src/rump_proxy.rs`)
  enforces this as a hard guarantee: a socket syscall from a rump box, or any
  syscall on a rump-owned fd, can never fall through to smoltcp.

Source: `src/rump_proxy.rs` (header + `intercept_box_syscall`).

## The two stacks

| | Native (smoltcp) | Rump (NetBSD) |
|---|---|---|
| **NIC** | NIC0 (always present) | NIC1 (`/dev/net/tap0`), needs `RUMP_NIC=1` |
| **L2 path** | In-kernel smoltcp | Userspace `/bin/rump_server` over a raw tap |
| **Built-in SSH** | Yes (in-kernel, smoltcp-based) | N/A (built-in SSH is smoltcp-only) |
| **Default for box 0** | Yes (normal builds) | Yes when `rump-default` feature is on (devbox) |
| **In-kernel HTTPS** | Yes (`kernel-tls`) | N/A (use a userspace tool) |

### When is each used?

- **Default build** (`cargo run --release`): box 0 = smoltcp. Rump is opt-in
  per box via a herd `stack=rump` service (see `archive/RUMP_PLUS_HERD.md`).
- **Devbox** (`devbox` feature + `--no-default-features`): smoltcp is compiled
  out entirely. Box 0 = rump. There is no native stack at all.

## How box 0 gets its stack

### Native (default build)

Box 0 starts on smoltcp. NIC0 is initialised by the kernel at boot; DHCP runs
on the in-kernel stack. No userspace process owns the stack.

### Rump-default (devbox)

At boot, `rump_proxy::start_default_stack` (`src/rump_proxy.rs:1284`) runs when
the `rump-default` feature is on:

1. Checks `akuma_net::rump_tap::is_ready()` (NIC1 exists). If not, logs and
   returns — box 0 stays native (no-op in a devbox without `RUMP_NIC=1`).
2. `mark_box_rump(0)` — marks box 0 as a rump box **before** spawning the
   server, so subsequent box-0 socket syscalls route to the proxy (which waits
   for the handshake).
3. Spawns `/bin/rump_server --net --fd 3 --log /var/log/box/0/rump_server.log`
   in box 0. The server's own pid is excluded from interception.
4. `attach_server(0, pid)` — wires the kernel sysproxy channel onto the
   server's fd 3 and handshakes in a kthread (~5s: `rump_init` + DHCP over
   `/dev/net/tap0`).

After that, **every ordinary unboxed process** (login shell, sshd, curl, meow)
has its socket syscalls transparently routed to box 0's `rump_server` over that
channel. No herd box, no `box_root`, no `join_box`.

> `main` does **not** block on the handshake — rump_server's rumpsp fiber is
> cooperatively scheduled and only advances while the host scheduler keeps
> churning. `main` must return so herd starts + the background loop pumps the
> fibers. herd's `sshd` `start_delay_ms` + `restart` cover the ~5s bring-up.
> Source: `src/rump_proxy.rs:1312-1321`.

## Syscall routing detail

For a socket-family syscall (or any syscall on an fd the rump proxy owns),
`intercept_box_syscall` forwards it to the box's `rump_server` over the fd-3
kernel pipe pair. The proxy is **synchronous on the calling thread** — every
round-trip blocks the caller until the server replies.

AF_UNIX socketpairs (syscall 199) are **excluded** from proxying: they are pure
local IPC, never networking, so they always run natively regardless of the
box's stack. This matters for Rust's `std::process::Command`, which uses
`socketpair(AF_UNIX, ...)` as its exec-status channel for every subprocess
spawn. Source: `crates/akuma-rump/src/syscall_translation.rs`,
`archive/OPTIONAL_SMOLTCP.md`.

## Port forwarding (host → guest)

`scripts/cargo_runner.sh` sets up SLIRP `hostfwd` rules:

- NIC0 (`net0`): `SSH_PORT→:22`, `HTTP_PORT→:8080`, `MODEL_PORT→:11434`, etc.
  (`cargo_runner.sh:259`). `SSH_PORT` derives from `INSTANCE`.
- NIC1 (`net1`, rump): `RUMP_SSH_PORT` (default 2223) → `:22` on the rump
  SLIRP (`cargo_runner.sh:166`). This is how you reach the devbox's userspace
  sshd.

## Background

- `archive/SMOLTCP_MIGRATION_SUMMARY.md` — the smoltcp migration post-mortem.
- `archive/OPTIONAL_SMOLTCP.md` — making smoltcp optional for the devbox.
- `archive/NATIVE_STACK_INTERNET.md` — validating the native stack.
- `archive/RUMP_SYSPROXY.md` — the committed sysproxy design.
- `archive/HIJACK_VS_KERNEL_PROXY.md` — why kernel-side routing.
