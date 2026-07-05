# Akuma Devbox

A reproducible "sit down and develop **inside** Akuma" image. You SSH into it and use it
like a tiny Unix workstation. The point is **dogfooding** — surfacing and fixing the
concrete papercuts that only show up under daily use.

Everything here lives under `overlays/devbox/` and does not disturb the default
`bootstrap/` tree or the normal run scripts.

> **Current state:** the image is being built up incrementally. Right now it is the
> **minimal** target — *SSH in over the rump network stack, nothing else* — so that the
> networking + login foundation is solid before the toolchains, editor, `meow`, and the
> in-VM source tree get layered back on (see [Roadmap — the rest](#roadmap--the-rest)).

---

## Quick start

```bash
# 1. Build the minimal image (host). Needs Docker; builds herd + sshd, ~1 GB image.
overlays/devbox/bootstrap.sh

# 2. Boot it (devbox profile: rump is the default stack, no built-in SSH, RUMP_NIC=1).
overlays/devbox/run.sh

# 3. SSH in once box 0's rump stack is up and herd has started sshd.
ssh -o StrictHostKeyChecking=no -p 2223 root@localhost
```

---

## How it works

The devbox differs from a normal Akuma boot in exactly two ways, and both are selected
together by the **`devbox` profile + `devbox` feature** (mirroring how `size`/`extreme`
are a `[profile.*]` plus a feature set):

```
cargo run --profile devbox --features devbox      # what overlays/devbox/run.sh does
scripts/build_devbox.sh                            # build-only equivalent
```

```
[features]
devbox         = ["rump-default", "userspace-sshd"]   # the meta-feature
rump-default   = ["rump"]                             # rump is the DEFAULT stack for box 0
userspace-sshd = []                                   # no built-in (smoltcp) SSH server
```

`[profile.devbox]` inherits `release`, so it carries `rump` + `sound` codegen; the
behavioural difference is entirely in those two features.

### 1. rump is the DEFAULT network stack (not a box)

Akuma routes AF_INET syscalls **per box**, keyed on a process's `box_id`. Box 0 is the
root box every process starts in; normally box 0 is on the native smoltcp stack, and rump
is opt-in per box (a herd `stack=rump` service in its own box that other processes must
`join_box` into).

The `rump-default` feature flips box 0 itself to rump. At boot the kernel
(`rump_proxy::start_default_stack`, `src/rump_proxy.rs`):

1. marks **box 0** as `stack=rump` (`mark_box_rump(0)`),
2. spawns `/bin/rump_server --net --fd 3` in box 0, and
3. wires the kernel sysproxy channel onto its fd 3 and handshakes (`attach_server(0, …)`)
   — `rump_init` + ~19 kthreads + DHCP over `/dev/net/tap0`, ~5 s.

After that, **every ordinary unboxed process** — the login shell, `sshd`, and anything you
spawn from your session — has its socket syscalls transparently routed to box 0's
`rump_server` over that channel. No herd box, no `box_root`, no `join_box`. The kernel's
dispatch hook (`intercept_box_syscall`) enforces this as a hard guarantee: a socket-family
syscall (or any syscall on a rump-owned fd) from a rump box can never fall through to
smoltcp.

This needs the second NIC (`/dev/net/tap0`) — `run.sh` sets `RUMP_NIC=1`, which adds a
virtio-net on `virtio-mmio-bus.4` and forwards host port **2223** → guest port 22 on that
NIC. Without `RUMP_NIC=1` there is no tap for the stack to DHCP on, so `start_default_stack`
logs and leaves box 0 on the native stack.

### 2. No built-in SSH

Akuma normally runs an **in-kernel** SSH server (the one that prints
`[SSH Server] Listening`). It is built on smoltcp sockets and runs unboxed on the native
stack — exactly the one thing that would *not* go through rump. The `userspace-sshd`
feature sets `config::ENABLE_USERSPACE_SSHD = true` so the kernel never starts it
(`[Main] Built-in SSH server disabled`).

The only sshd is then the **userspace `/bin/sshd`**, started by herd
(`/etc/herd/enabled/sshd.conf`) **unboxed** — so it lives in box 0 and its
listen/accept/session I/O is automatically rump-routed. There is no `join_box` anymore.

**Login auth:** this is a local dev VM reachable only via the host port-forward, so
`/etc/sshd/sshd.conf` sets `disable_key_verification = true` — `ssh -o
StrictHostKeyChecking=no -p 2223 root@localhost` gets in with no key setup. The host key is
auto-generated on first boot at `/etc/sshd/id_ed25519`. To require a key instead, set that
false and drop your pubkey in `/etc/sshd/authorized_keys`.

### `/etc` comes from the overlay only

`bootstrap.sh` populates the base binaries from `bootstrap/`, then **wipes `/etc`
entirely** and lays down `overlays/devbox/rootfs/` as the *sole* source of `/etc`. Nothing
from `bootstrap/etc/` (stale herd/sshd/httpd demo configs) is inherited unreviewed — the
devbox owns its `/etc` outright.

### smoltcp is still compiled in (for now)

`smoltcp` and the in-kernel SSH server are **compile-coupled** — the SSH server, plus
`syscall/net.rs`, `poll.rs`, and the DNS/HTTP paths, are all built on smoltcp types across
~15 files. So the current `devbox` feature simply layers on top of the default feature set:
smoltcp is *compiled* but box 0 never routes to it, and the built-in SSH server never
starts. **Phase 2** will build the devbox with `--no-default-features` to compile smoltcp
(and the smoltcp-coupled built-in SSH) out entirely — see `scripts/build_devbox.sh`.

---

## What's on the minimal image

| Area | Contents |
|------|----------|
| **Networking** | `/bin/rump_server` — box 0's NetBSD rump TCP/IP stack, brought up by the kernel at boot. |
| **Supervisor** | `/bin/herd` — starts the userspace `sshd`. |
| **SSH** | `/bin/sshd` (userspace), unboxed → rump. The only sshd on the image. |
| **Shell** | `busybox` with a full applet symlink set (`--full-busybox`), `/bin/sh`. |

`/etc` (all from the overlay):
- `etc/herd/enabled/sshd.conf` — the unboxed userspace sshd.
- `etc/sshd/sshd.conf` — `disable_key_verification = true` (local dev VM).
- `etc/resolv.conf`, `etc/meow/config` — staged for the roadmap items below (not exercised
  by the minimal image).

---

## Files in this overlay

```
overlays/devbox/
  bootstrap.sh   # host build: build herd+sshd → create image → base binaries → wipe /etc → overlay
  run.sh         # boot QEMU: cargo run --profile devbox --features devbox, RUMP_NIC=1, MEMORY=4096
  README.md      # this file
  NOTICE         # third-party attribution (neatvi — used by a roadmap item)
  rootfs/        # the SOLE source of the image's /etc
    etc/herd/enabled/sshd.conf
    etc/sshd/sshd.conf
    etc/resolv.conf
    etc/meow/config
    root/DEVBOX.txt
```

`bootstrap.sh` env knobs (all optional):

| Var | Default | Meaning |
|-----|---------|---------|
| `DEVBOX_DISK` | `devbox.img` | Output image path (`DISK=` passthrough). |
| `DEVBOX_DISK_MB` | `1024` | Image size in MB. Bumped when the toolchain + `/src` tree are added. |
| `DEVBOX_BUILD_USERSPACE` | `true` | Rebuild `herd` + `sshd` from source; `false` reuses `bootstrap/bin`. |

`run.sh` env knobs: `DEVBOX_DISK`, `DEVBOX_MEMORY` (default 4096), `RUMP_SSH_PORT` (default 2223).

---

## Verification

1. `overlays/devbox/bootstrap.sh` completes; `devbox.img` exists with `/etc` = overlay only.
2. `overlays/devbox/run.sh`; in the log, watch for:
   - `[RUMP-SP] box 0 marked stack=rump` and `rump-default: … spawning /bin/rump_server`
   - `[Main] Built-in SSH server disabled (ENABLE_USERSPACE_SSHD=true)`
   - `[RUMP-SP] box=0 proxy ready` (box 0's rump stack up, DHCP done)
   - `[herd] Started sshd` and `[SSHD] Listening on …`
3. `ssh -o StrictHostKeyChecking=no -p 2223 root@localhost` lands in a shell — and the log
   shows `[RUMP-SP] accept -> box_fd=…` + `sendto`/`recvfrom` for the session, i.e. the SSH
   session is running over rump with no box.
4. Log every papercut hit — those become follow-up fix tasks (see Backlog).

---

## Roadmap — the rest

Layered back on now that SSH-over-rump is solid:

- **Phase 2 networking:** build with `--no-default-features` so smoltcp (and the
  smoltcp-coupled built-in SSH) are compiled out entirely, not just unused.
- **Toolchains:** `apk` + `musl-dev` + `libtcc1`, `tcc` (`tcc -static`), and a nightly Rust
  toolchain (musl host) under `/usr/local` with `rust-src` and the `aarch64-unknown-linux-musl`
  / `aarch64-unknown-none` std — via `populate_disk.sh --with-apk --with-musl-dev
  --with-rust-toolchain`. (Needs `etc/apk` + `etc/ssl` staged into the overlay so `/etc`
  stays overlay-only.)
- **Editor:** `neatvi` at `/bin/vi` (ISC-licensed C vi clone — see [`NOTICE`](./NOTICE)),
  cross-compiled with the repo's `aarch64-linux-musl-gcc`.
- **`git`:** `scratch` symlinked to `/bin/git` (clone/push over HTTPS with a GitHub PAT).
- **Agent `meow` → z.ai GLM-5.2.** meow's HTTPS already rides rump automatically: its socket
  syscalls (`libakuma`, numbers 198/203/206) are the Linux ABI numbers the kernel intercepts,
  so `libakuma-tls` (embedded-tls) goes through rump with no change. The one leak is DNS
  (custom `RESOLVE_HOST` syscall 300, not intercepted); meow's **`linux-net` feature** swaps
  it for ordinary UDP-socket DNS that *is* intercepted. Build (freestanding no_std, its own
  `_start`, links with rust-lld directly):
  ```bash
  cd userspace/meow
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-Clinker=rust-lld -Clinker-flavor=ld.lld -Clink-self-contained=no" \
  cargo build --release --target aarch64-unknown-linux-musl --features linux-net \
    -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
  ```
  z.ai key + GitHub PAT are filled in **after** SSH (placeholders only; `/root/DEVBOX.txt`).
- **Source tree:** clone the full Akuma repo (+submodules) into `/src/github.com/netoneko/akuma`
  so you develop against it in-VM (this is what bumps `DEVBOX_DISK_MB`).

---

## Backlog (papercuts)

Papercuts surfaced by dogfooding, to fix later:

- ~~**`ps` shows nothing**~~ — **FIXED.** `ps`/`top` parse `/proc/<pid>/stat` (Linux's
  compact single-line format), not `/proc/<pid>/status` (the human-readable one, the only
  one procfs implemented). Added `/proc/<pid>/stat` in `src/vfs/proc.rs`.
- ~~**One-shot `ssh host <cmd>` returns no output**~~ — **FIXED**, see
  `docs/OPTIONAL_SMOLTCP.md`'s Concurrent SSH section and
  [`userspace/sshd/docs/FLOW.md`](../../userspace/sshd/docs/FLOW.md).
- **`/bin/rump_server` is ~13 MB** — the box-0 network stack binary is huge (it dominates
  the image and its cold-load demand-paging). Trim it down later (strip / drop unused rump
  components / link-time GC).
- ~~**`curl <hostname>` crashes the DNS resolver thread and wedges the VM.**~~ — **FIXED**
  (for real this time as of 2026-07-05; see below). Same stale-TTBR0 bug class, three call
  sites, all now patched:
  (1) The resolver thread's `ec=0x20` instruction abort was a stale-TTBR0 bug in the
  scheduler context. `clone_thread` (curl's `AsynchDNS` path) was fixed first — see
  `docs/OPTIONAL_SMOLTCP.md`'s "curl https freeze" section.
  (2) The identical code path in `fork_process` (plain `fork()`+execve) was patched
  2026-07-04 — same one-line override: `child_ctx.ttbr0 = new_proc.address_space.ttbr0()`.
  This was *believed* at the time to fix `git clone`/`wget`, but did not: those spawn
  subprocesses via `CLONE_VFORK` (musl `posix_spawn`), which routes to a third, separate
  function — `vfork_process` — not `fork_process`.
  (3) `vfork_process` had the exact same bug and was still unpatched until 2026-07-05 —
  it's the one `git clone` actually hits, and was verified (in devbox) to no longer wedge
  the VM. See `docs/OPTIONAL_SMOLTCP.md`'s "`vfork_process` had the exact same bug" section.
  DNS itself was never broken via the threaded resolver (`nslookup`/`curl` both work); the
  "DNS error" people saw was the SSH session dying when the VM wedged mid-fork/vfork, before
  any real output was produced. The kernel still loops on unhandled EL0 instruction aborts
  instead of killing the process — a general EL0-fault → SIGSEGV robustness fix for the
  handler is still desirable but no longer load-bearing now that all three TTBR0 call sites
  are patched.
- ~~**`git clone` still fails after the wedge fix — c-ares can't reach DNS servers.**~~ —
  **FIXED.** With the vfork wedge gone, `git clone https://...` failed cleanly instead of
  hanging, with `Could not resolve host: ... (Could not contact DNS servers)` — that
  parenthetical is a c-ares error string (Alpine's apk-packaged `git` links a c-ares-enabled
  libcurl). Root cause: c-ares calls `fcntl(fd, F_SETFD, FD_CLOEXEC)` on every UDP socket it
  opens for a DNS query; `rump_fcntl` (`src/rump_proxy.rs`) only implemented `F_GETFL`/
  `F_SETFL` and returned `EOPNOTSUPP` for `F_SETFD`, which c-ares treats as fatal — it closed
  the socket and retried (2 nameservers × 3 tries = 6 socket-open/close cycles, confirmed via
  `RUMP_SP_TRACE`) before giving up entirely. musl's resolver and libcurl's threaded resolver
  never call `fcntl(F_SETFD)` on their DNS sockets, so `curl`/`nslookup` never hit this.
  Fixed by making `F_GETFD`/`F_SETFD` a no-op success, same precedent as
  `proxy_setsockopt`'s handling of unsupported options. See `docs/OPTIONAL_SMOLTCP.md`.
- **Shell pipeline with an early-closing reader can wedge the whole VM.** `cmd | head -N`
  (or `| tail`), where `cmd` writes more than `N` lines, has twice wedged the VM at ~99%
  CPU with the last log line being `[signal] tkill(tid=X, sig=13)` (SIGPIPE delivery to the
  writer). New SSH sessions still connect (the VM isn't fully dead), but the stuck
  processes never clean up and the CPU stays pegged. Not yet root-caused — suspect the
  writer's SIGPIPE handling/delivery path spins instead of terminating the write syscall.
  Workaround: redirect verbose output to a file instead of piping through `head`/`tail`.
- ~~**`cargo build`/`rustc` can't spawn a subprocess at all — `os error 95` before any
  `clone()`.**~~ — **FIXED.** Found while checking that a real crate (`cargo build
  --release` on a Rust project) works, not just `git clone`. Root cause:
  `is_socket_family_sysno()` (`crates/akuma-rump/src/syscall_translation.rs`) claimed
  `socketpair()` (199) for the rump proxy, but the proxy's dispatch has no arm for
  `Op::Socketpair` — it silently fell into the generic `_ => EOPNOTSUPP` catch-all.
  Modern Rust's `std::process::Command` uses `socketpair(AF_UNIX, SOCK_SEQPACKET|
  SOCK_CLOEXEC)` as its exec-status channel for every subprocess spawn, so this broke
  spawning *any* subprocess from a Rust program on a rump box (box 0 under devbox) — e.g.
  `rustc` couldn't even invoke its own linker (`cc`). Fixed by excluding 199 from the
  proxied range: AF_UNIX socketpairs are pure local IPC, never networking, so they always
  run natively regardless of the box's stack. See `docs/OPTIONAL_SMOLTCP.md`.
- **`cargo build`/`rustc` subprocess spawn panics with CLOEXEC-pipe `EBADF` — in progress.**
  With the socketpair fix above, `rustc` gets past `socketpair()` but then panics:
  `the CLOEXEC pipe failed: Os { code: 9, ... "Bad file descriptor" }` — the parent fails
  to read its own end of the exec-status socketpair after forking the child. Points to a
  `fork_process`/`vfork_process`/`execve` bug specific to `FileDescriptor::UnixSocket` fds
  across fork, separate from the TTBR0 bug class. See `docs/OPTIONAL_SMOLTCP.md`.
- ~~**Only one SSH session at a time / parallel shells hang**~~ — **FIXED**, and it turned
  out to be unrelated to the `curl` DNS-crash wedge above. Three separate bugs, all fixed:
  `sshd`'s own accept
  loop was fully serial by design; the kernel hard-rejected the `fcntl(O_NONBLOCK)` a
  cooperative multiplexer needs on rump sockets; and a blocking `sleep_ms` inside an
  `async fn` loop (no `.await` on it) never actually yielded, so the first idle session
  monopolized `sshd`'s one thread until it exited. See `docs/OPTIONAL_SMOLTCP.md`'s
  Concurrent SSH section for all three.   Still open: the single box-0 rump proxy serializes
  socket syscalls, which may head-of-line block truly-simultaneous sessions under load — not
  yet re-measured.
- **Rump stack is slow — needs investigation.** Network I/O over box 0's rump stack is
  dramatically slower than the native smoltcp path. Observed 2026-07-05: `git clone
  https://github.com/netoneko/teddy.git` (a tiny repo) took well over 2 minutes and exceeded
  a 120 s client-side SSH timeout; `cargo fetch`/build dependency pulls are similarly
  sluggish. Not yet characterized whether this is the per-syscall proxy round-trip cost (every
  socket syscall crosses the kernel→`rump_server` fd-3 channel and back, serialized), the
  virtio-net + DHCP path, rump's own TCP, or something else. Worth measuring against the
  native smoltcp stack on the same workload before assuming it's fundamental. See
  `docs/OPTIONAL_SMOLTCP.md`'s backlog for the same item.
- **Rump net thread (and sshd) starve under CPU-bound load — bump scheduling priority.**
  Under a single-core guest running a heavy CPU-bound process (e.g. `rustc`/LLVM codegen,
  which is pure compute with no syscalls for minutes at a stretch), SSH connections time out
  at the banner exchange and the box looks unreachable even though QEMU stays pegged at ~100%
  CPU and the guest is not crashed. Observed 2026-07-05: during a `cargo build` of `teddy`,
  every SSH attempt failed with `Connection timed out during banner exchange` for ~10+
  minutes of solid rustc codegen. Root cause is the scheduler giving the CPU-bound process
  equal standing with the rump network thread and sshd, so on a single core the
  latency-sensitive net/SSH path can't get a timeslice in time. Likely fix: raise the
  scheduling weight/priority of the rump proxy thread (and/or sshd) so the network stays
  responsive under load. See `docs/OPTIONAL_SMOLTCP.md`'s backlog for the same item.
