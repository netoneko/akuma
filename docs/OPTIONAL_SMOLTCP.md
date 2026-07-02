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

## Status

| Build | Compiles | Runtime |
|-------|----------|---------|
| default (`smoltcp` on) | ✅ clean, clippy-clean | unchanged |
| `size` / `extreme` (smoltcp re-added) | ✅ | unchanged |
| **devbox (`smoltcp` off)** | ✅ clean, clippy-clean, 1.4 MB | ⚠️ **rump bring-up blocked** — see below |

### Open blocker: rump handshake stalls on the idle, smoltcp-free build

On the rump-only devbox build, box 0's `rump_server` boots fully (DHCP `10.0.2.15`,
`SERVING sysproxy on fd 3`), and the kernel-side handshake gets **most** of the way:

1. kernel reads the complete banner off the reply pipe ✅
   (`RUMPSP-…-NetBSD-7.99.34/evbarm64\n`)
2. kernel writes the `HANDSHAKE_GUEST` request to the request pipe ✅ (24 + 13 bytes)
3. `poll` reports the request pipe readable and `rump_server` reads the request ✅
4. `rump_server` **never writes the RESP back** ➜ kernel times out ➜ userspace `sshd`
   can't get a socket ➜ herd crash-loops it.

So the stall is inside `rump_server`'s `handlereq` for `HANDSHAKE_GUEST` (which rforks a
rump lwp/client context). It only manifests when the system is otherwise idle: in a build
with `smoltcp`, the kernel's background `smoltcp_net::poll()` loop keeps the scheduler
churning, which pumps rump's cooperative fibers to completion; with the native stack gone
there is no such churn, and a rump fiber-wake / `steady_tick` path does not advance. This
is the same cooperative-scheduling sensitivity noted in the rump port work
(`userspace/rumpkernel/…`, the sub-tick fiber-sleep / `steady_tick_interval` lever).

**This is the one thing standing between the current tree and a fully smoltcp-free devbox.**

### Ways forward

- **A — ship the working config, keep this infra behind the flag.** Point
  `overlays/devbox/run.sh` + `scripts/build_devbox.sh` back at the Phase-1 feature set
  (`--features devbox` *with* default features → `smoltcp` compiled but unused; rump is
  still the default stack, still no built-in SSH — verified SSH-over-rump working). The
  smoltcp-out build stays available for finishing later.
- **B — fix the rump bring-up.** Make the handshake complete without smoltcp's churn:
  drive the rump clock / steady-tick faster during bring-up, or have the kernel pump the
  rump kthreads while the box-0 proxy is initializing, so `handlereq`'s client-context
  fork finishes.

## Touchpoints

- `Cargo.toml`, `crates/akuma-net/Cargo.toml` — the `smoltcp` feature + optional dep.
- `crates/akuma-net/src/lib.rs`, `socket.rs` — module gating + Tier-C stubs + split `init`.
- `src/main.rs` — `mod ssh`/tests gating, built-in-SSH-spawn gating, background-poll gating,
  the `compile_error!` guard (no smoltcp ⇒ must have `userspace-sshd`), crate-level
  `allow(dead_code)` for the rump-only build.
- `src/syscall/{mod,net,poll,fs,term}.rs` — dispatch cfg-else, per-fn gating, UnixSocket
  `send`/`recv` variants.
- `src/rump_proxy.rs` — `start_default_stack` (unchanged by this doc's split; smoltcp-free).
- `scripts/build_size.sh`, `scripts/build_extreme_size.sh`, `scripts/build_devbox.sh`,
  `overlays/devbox/run.sh` — explicit `smoltcp` in the profiles that need it.
