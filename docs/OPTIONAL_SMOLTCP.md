# Optional `smoltcp` — the rump-only (devbox) build

## Why

The devbox (`overlays/devbox/`) makes the NetBSD **rump** TCP/IP stack the *default*
network stack for box 0 (see `rump-default` in `Cargo.toml` and
`rump_proxy::start_default_stack`). To make that a genuinely rump-only image — and to
reclaim the ~2 MB the native stack costs — the `smoltcp` native stack (and the in-kernel
SSH server, which is built on it) can now be compiled **out** entirely via a new
`smoltcp` cargo feature.

Result: the devbox kernel drops from **3.5 MB → 1.4 MB** with `smoltcp` off.

## The feature

`smoltcp` is **default-on** in both `akuma` (root) and `akuma-net`. Every existing build
keeps the native stack; only a `--no-default-features` build that omits `smoltcp` drops it.

```
# root Cargo.toml
smoltcp = { version = "0.12.0", …, optional = true }        # was non-optional
akuma-net = { path = "crates/akuma-net", default-features = false }
[features]
default  = [ …, "smoltcp", … ]
smoltcp  = ["dep:smoltcp", "akuma-net/smoltcp"]

# crates/akuma-net/Cargo.toml
smoltcp = { …, optional = true }
[features]
default = ["smoltcp"]
smoltcp = ["dep:smoltcp"]
```

`kernel-tls`/`tls-rsa` are kept **orthogonal** to `smoltcp` (the TLS/verifier crates are
smoltcp-free), but their only runtime consumer — `http_get` (shell `curl https://`) — is
smoltcp-coupled, so with `smoltcp` off they are dead weight; the devbox omits them.

### Profiles that build `--no-default-features` must now list `smoltcp` explicitly

Because `smoltcp` is optional, any profile built with `--no-default-features` that still
wants the native stack has to re-add it:

- `scripts/build_size.sh` — added `smoltcp` (keeps native stack + built-in SSH + HTTPS).
- `scripts/build_extreme_size.sh` — added `smoltcp` (unchanged behavior; drop it later to
  reclaim space if extreme goes netless).
- `scripts/build_devbox.sh` / `overlays/devbox/run.sh` — deliberately **omit** `smoltcp`
  (and `kernel-tls`/`tls-rsa`); rump is the only stack.

The `smp` builds keep the default feature set, so they still get `smoltcp`.

Devbox build line:
```
cargo build --profile devbox --no-default-features \
  --features devbox,neko,sound,no-tests,\
sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll
```

## The gating split

`smoltcp` is woven through ~15 kernel files plus `akuma-net`, and the in-kernel SSH server
is built on smoltcp sockets — so **smoltcp and the built-in SSH are compile-coupled** and
drop together. The code splits into three tiers:

- **Tier A — smoltcp-free, kept as-is.** `akuma-net` `runtime`, `hal`, `stats`,
  `rump_tap`, and the whole `akuma-rump` crate; and in `socket.rs` the *types* used
  pervasively by non-network code: `SockAddrIn`, `SocketAddrV4`, `socket_const`,
  `SocketStat`, `libc_errno`. `rump_proxy.rs` itself is smoltcp-free.
- **Tier B — smoltcp-coupled, gated `#[cfg(feature = "smoltcp")]`.** `akuma-net`
  `smoltcp_net`, `dns`, `http`, and the socket-table internals of `socket.rs`; in the
  kernel: the socket ops in `syscall/net.rs`, the `Socket` fd arms in
  `syscall/{poll,fs,term}.rs`, the whole in-kernel `ssh` module, `shell/commands/net.rs`,
  and the network/ssh boot tests.
- **Tier C — unconditional callers, satisfied by stubs.** `socket::remove_socket` and
  `socket::list_sockets` are called from FD teardown, `/proc/net/tcp`, and the
  `ExecRuntime` callback regardless of stack, so `socket.rs` provides
  `#[cfg(not(feature = "smoltcp"))]` no-op / empty-Vec stubs. `sys_socketpair`
  (pipe-backed) and `sys_shutdown` stay ungated.

The socket-syscall dispatch in `syscall/mod.rs` returns `ENETDOWN` for the smoltcp-only
ops when the feature is off — **except** `SENDTO`/`RECVFROM`, which are always dispatched
(see next section).

A crate-level `#![cfg_attr(not(feature = "smoltcp"), allow(dead_code))]` in `main.rs`
covers the in-kernel shell/editor/async-fs surface that is reachable only through the
built-in SSH server: it is dead in a rump-only image (busybox over the userspace `/bin/sshd`
is used instead) but is not smoltcp-specific, so gating each item individually would be
wrong. `default`/`size`/`extreme` keep dead-code denied.

## `send`/`recv` on the fd-3 sysproxy channel must survive without smoltcp

box 0's `rump_server` is excluded from box interception (`SERVER_PIDS`), so its own
`send()`/`recv()` on the fd-3 sysproxy channel — a **UnixSocket (pipe-backed)** fd — fall
through to native dispatch. `sys_sendto`/`sys_recvfrom` therefore keep a
`#[cfg(not(feature = "smoltcp"))]` variant that handles the UnixSocket case
(`fs::sys_write`/`fs::sys_read` on the pipe) and `EBADF`s everything else, and their
dispatch arms are **not** gated to `ENETDOWN`. Without this the rump handshake banner send
fails and box 0's rump stack never comes up. (See `syscall/net.rs` +
`syscall/mod.rs` SENDTO/RECVFROM.)

## Status — WORKING

| Build | Compiles | Runtime |
|-------|----------|---------|
| default (`smoltcp` on) | ✅ clean, clippy-clean | unchanged |
| `size` / `extreme` (smoltcp re-added) | ✅ | unchanged |
| **devbox (`smoltcp` off)** | ✅ clean, clippy-clean, **1.4 MB** | ✅ **rump default stack + interactive SSH-over-rump verified** |

Verified end-to-end on the fully smoltcp-free build: box 0's `rump_server` boots (DHCP
`10.0.2.15`, `SERVING sysproxy on fd 3`), the kernel handshake completes (`box=0 proxy
ready`), herd's userspace `sshd` binds/accepts over rump, and an **interactive SSH session
runs commands** (`echo`, `uname -a`, `ls /`) with output returned — all over the NetBSD
rump stack, no smoltcp compiled in.

### Two gating bugs found + fixed (no NetBSD-source patch)

Getting there surfaced two real bugs, both from over-gating socket syscalls that the rump
path still needs. Neither is in the NetBSD source (`rumpuser_sp.c` is byte-identical to the
working smoltcp build); both fixes live in our kernel dispatch:

1. **`sendmsg` UnixSocket passthrough (rump bring-up).** box 0's `rump_server` is excluded
   from box interception, so its own channel I/O falls through to native dispatch. Its
   sysproxy replies — the handshake RESP and *every* proxied-syscall reply — go through
   `dosend` → `host_sendmsg` (only the initial banner uses `send`→`sendto`). Gating
   `sys_sendmsg` to `ENETDOWN` made the RESP fail, so the handshake timed out and the stack
   never came up. Fix: a `#[cfg(not(feature = "smoltcp"))]` `sys_sendmsg` variant that
   writes every iovec to the UnixSocket tx pipe, dispatched unconditionally (same pattern
   as the `sendto`/`recvfrom` UnixSocket variants). `readframe` uses `read`, already
   handled. (`src/syscall/net.rs`, `src/syscall/mod.rs`.)

2. **WAITPID pid ↔ rump-fd collision (session hang).** `rump_proxy::intercept_box_syscall`
   treated *any* syscall whose `args[0]` numerically matched a rump socket fd as
   proxy-owned. `sshd`'s `waitpid(child_pid)` (nr 303) on its shell child, whose pid `4`
   equalled the accepted rump-socket fd `4`, was thus misrouted and returned `EOPNOTSUPP`
   in a tight retry loop → the session hung. (Phase 1's larger pid/fd numbers never
   collided; the minimal `no-tests` build makes them small and collide.) Fix: a syscall
   with no translation op is owned **only if it is socket-family by number** — `args[0]` is
   not reliably an fd for arbitrary syscalls (WAITPID/KILL/SPAWN take a pid). Read/write/
   close on a rump fd, and socket-family ops, are still owned as before.
   (`src/rump_proxy.rs`.)

### Backlog

- **One-shot `ssh host <cmd>` doesn't spawn the child** (`ssh -p 2223 root@localhost echo
  hi` closes without output); the **interactive** session works. Appears to be a
  sshd one-shot-exec path issue, not rump/smoltcp — to investigate separately.

## Touchpoints

- `Cargo.toml`, `crates/akuma-net/Cargo.toml` — the `smoltcp` feature + optional dep.
- `crates/akuma-net/src/lib.rs`, `socket.rs` — module gating + Tier-C stubs + split `init`.
- `src/main.rs` — `mod ssh`/tests gating, built-in-SSH-spawn gating, background-poll gating,
  the `compile_error!` guard (no smoltcp ⇒ must have `userspace-sshd`), crate-level
  `allow(dead_code)` for the rump-only build.
- `src/syscall/{mod,net,poll,fs,term}.rs` — dispatch cfg-else, per-fn gating, and the
  rump-only UnixSocket `sendto`/`recvfrom`/**`sendmsg`** variants (always dispatched) that
  service box 0's rump_server fd-3 channel.
- `src/rump_proxy.rs` — `start_default_stack` (smoltcp-free) + the
  `intercept_box_syscall` fix so a non-socket-family syscall is not proxy-owned merely
  because `args[0]` collides with a rump fd number (the WAITPID hang).
- `scripts/build_size.sh`, `scripts/build_extreme_size.sh`, `scripts/build_devbox.sh`,
  `overlays/devbox/run.sh` — explicit `smoltcp` in the profiles that need it.
