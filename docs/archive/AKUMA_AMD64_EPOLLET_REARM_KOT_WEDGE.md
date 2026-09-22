# amd64: the `kot` deaf-node wedge — `accept4` and the missing `EPOLLET` re-arm (2026-09-23)

**Status: FIXED** (uncommitted in the working tree at time of writing), verified
on QEMU, on the ryzen Firecracker guest and on the trashcan metal. Two amd64
mesh seats (`akuma-metal`, `ryzen-fc`) are back in the litter, so the fleet is
five cats, not three. One **new, open** defect found on the metal afterwards —
a kot worker spinning on `writev` at ~650 k calls/s — is recorded in §6.

The handoff this closes is [`AKUMA_AMD64_KOT_REPLICA_WEDGE.md`](AKUMA_AMD64_KOT_REPLICA_WEDGE.md)
(symptoms, the rig, the four ranked leads). Read it for the rig; read this for
what the bug was.

---

## 1. Symptom (as handed over)

`kot run` — akuma-miot's mesh node + agent loop, a multi-threaded **tokio**
runtime serving **axum** on `:9944` and polling its peers with **reqwest** —
stopped answering HTTP within minutes of joining the mesh, on amd64 only:

- **trashcan metal** (4 cores, rtl8169): `:9944` *connection refused*, even
  from `127.0.0.1` on the box.
- **ryzen Firecracker guest** (1 vCPU, virtio-net): `:9944` *accepts TCP and
  never answers*; later sshd and the console went quiet too.
- **aarch64 Firecracker** (2 vCPUs): never, same binary, same mesh.

## 2. The investigation, in the order it happened

### 2.1 Lead 1 — `accept4` had no row. Real, and not enough.

The ryzen console said it outright:
`[syscall] no row for x86_64 nr=288 — returning ENOSYS`. 288 is `accept4`, the
only accept mio (and so tokio) ever calls. Added the row and a handler
(§4.1). Boot self-tests passed, and a first `curl` of a kot node in QEMU came
back `200` in 12 ms.

**A 10-minute soak then failed anyway: 182 answered, 1 946 not.** Without the
soak this would have been declared fixed on the first `200`. The soak was the
test; a single request was not.

### 2.2 The wedged guest, read from the inside

sshd still answered on the wedged QEMU guest, so the state could be read
rather than inferred:

- `ps`: every kot thread alive, **all parked** — the `[SLOT]` dump put them in
  `futex` (`sc=202`) or `epoll_pwait` (`sc=281`, `poll.rs:923`). Nothing
  spinning, nothing crashed.
- `wget http://127.0.0.1:8080/` **from inside the guest**: *connection
  refused*. The same refusal the metal showed — so this is one bug, not a
  metal one and a guest one.
- **`/proc/net/tcp` decided it.** The kot listener's `BACKLOG` column read
  **`0/8/0`** — 0 listening, **8 established-but-unaccepted**, 0 dead —
  against `8/0/0` for sshd and the healthy peer. And **~40 accepted
  connections sat in `CLOSE_WAIT`**: the client had hung up, the server had
  never noticed.

`CLOSE_WAIT` means *the application never read the FIN*. A server that is
alive, parked in `epoll_pwait`, and not reading data that has arrived is a
server that was **never told** the data arrived. That is a readiness-edge
bug, and it points at the one thing the amd64 socket path does differently.

### 2.3 Root cause — amd64's own send/recv never re-armed `EPOLLET`

On amd64, `read(2)`/`write(2)` go through `akuma-syscalls-glue` (shared with
AArch64), but **`sendto`/`recvfrom`/`sendmsg`/`recvmsg` are served by
`amd64/src/sock.rs`**, a second implementation kept native because the entry
asm drops the 6th syscall argument.

Glue's versions of those four calls maintain the edge-triggered state:

- after **every** successful TCP read, and on `EAGAIN`:
  `poll::epoll_on_fd_drained(fd)` — re-arm `EPOLLIN`;
- on a **short** write or `EAGAIN`: `poll::epoll_on_fd_write_blocked(fd)` —
  re-arm `EPOLLOUT` (the reason is spelled out in that function's comment:
  a 64 KiB hyper POST once hung forever without it).

`crate::sock`'s versions did **neither**. tokio registers every socket
`EPOLLET` and — the part that makes it bite — Rust `std`'s `TcpStream::read`
calls `recv(2)` (musl: `recvfrom`) and `write` calls `send(2)` (`sendto`), so
tokio *never* touches the glue paths that re-arm. The first `EPOLLIN` an
accepted connection reported was its last: the edge stayed "already
reported", its task never woke again, the peer's FIN went unread, and — every
connection task being wedged — the listener's backlog filled with connections
nobody accepted, until new SYNs were answered with RST.

Why nothing else in the tree ever showed it: busybox, `userspace/sshd` and
`httpd` do socket I/O through `read`/`write` (glue) and mostly aren't
edge-triggered. It needed an `EPOLLET` program that uses `recv`/`send` — which
is every tokio program.

Why *some* requests were answered (one of the handoff's open questions): each
connection gets exactly one readiness event. A request that arrived whole in
the first segment, before the task first polled, could be read and answered
on that single edge. Everything needing a second wake — a split request, the
FIN, keep-alive reuse — hung.

## 3. Evidence table

| run | kernel | result |
|---|---|---|
| QEMU TCG `-M microvm`, SMP=2, two kot nodes polling each other over loopback + 4 parallel host `curl`s, 10 min | accept4 only | **182 ok / 1 946 failed**; listener `0/8/0`; ~40 `CLOSE_WAIT` |
| same rig, same disk | accept4 **+ re-arm** | **2 224 ok / 0 failed**; every listener `8/0/0`; 0 `CLOSE_WAIT`; no `[BKL] stuck`; the two nodes elected a leader and replicated (both at head 232) |
| ryzen Firecracker, 1 vCPU | accept4 only | kot up, answering, synced 0 → 1280 blocks (not soaked before the re-deploy) |
| ryzen Firecracker, **2 vCPUs** | accept4 + re-arm | follower of mac-linux, caught up to leader head (1774 at 00:46) |
| trashcan metal, 4 cores, rtl8169, `192.168.1.123` | accept4 + re-arm | 743/743 self-tests; follower of mac-linux, caught up (1774 at 00:46); listeners `8/0/0` |

Boot self-tests: 789/789 on QEMU and on ryzen-fc at 1 and 2 vCPUs, except one
2-vCPU ryzen boot at 786/789 — a pre-existing test race, §6.2.

## 4. The change

All in the working tree, not committed.

### 4.1 `accept4`

- `crates/akuma-syscalls-abi/src/lib.rs`: row
  `Accept4 => ACCEPT4 = 288, nr::ACCEPT4;` (asm-generic 242), pinned by both
  numbers in `the_cargo_batch_carries_both_numbers`.
- `amd64/src/usermode.rs`: `Syscall::Accept4 => crate::sock::sys_accept4(a1, a2, a3, a4)`.
- `amd64/src/sock.rs`: `sys_accept4` — `sys_accept` is now `sys_accept4(.., 0)`.
  Unknown flag bits → `EINVAL` (as Linux); `SOCK_NONBLOCK`/`SOCK_CLOEXEC` are
  applied to the **accepted** fd. The nonblock bit is load-bearing: tokio
  reads an accepted fd until `EAGAIN`, so a blocking one parks a worker inside
  `recv`.

### 4.2 The re-arm (the actual fix)

`amd64/src/sock.rs`, mirroring glue rule for rule:

- `tcp_received(fd)` → `akuma_syscalls_glue::poll::epoll_on_fd_drained`, called
  by `recv` (for `recvfrom`) and `sys_recvmsg`'s TCP arm after every
  successful read and on `EAGAIN`.
- `tcp_sent(fd, want, result)` → `epoll_on_fd_write_blocked` on a short write
  or `EAGAIN`, for `send` (for `sendto`) and `sys_sendmsg`'s TCP arm.
- `sys_accept4` returning `EAGAIN` re-arms the **listener's** `EPOLLIN`: nothing
  was pending, so the next connection is a genuine new edge. (Glue's accept
  does not do this; AArch64 has not needed it. Worth doing there too — it is
  Linux-correct and costs one table walk on an already-failing call.)
- `send`/`recv` now take the fd and are private; their only callers were
  `sys_sendto`/`sys_recvfrom`.

UDP arms are unchanged, matching glue (which doesn't re-arm UDP either).

### 4.3 Also in the same build

- Removed the temporary `[wp0>]`/`[wp1>]`/`[wp>]` per-`write(2)` console trace
  from `amd64/src/usermode.rs` (handoff lead 2). It printed on every write from
  every non-leader thread — for tokio, every socket write — at ~2.4 µs/byte of
  trapping MMIO, part of it under the BKL.
- `amd64/mkdisk.sh` (from the previous session, already uncommitted): pre-grow
  `/bin`, fail if `/bin/sh` is missing.

### 4.4 Tests

Three new boot self-tests, all green:

- `sock: accept4 with an unknown flag is EINVAL`
- `sock: accept4(SOCK_NONBLOCK|SOCK_CLOEXEC) with nothing pending is EAGAIN`
- `dispatch: accept4 (288) reaches the socket layer` (fd 0 → `ENOTSOCK`, not `ENOSYS`)

**No self-test covers the re-arm itself.** It needs a connected loopback pair
and an edge-triggered epoll across two readiness transitions, and the sock
smoke test runs before anything drives netpoll (its own header says so). The
kot soak (§5) is the regression test until one exists. Host: `akuma-syscalls-abi`
18/18; clippy clean on the changed files.

## 5. How to reproduce / re-check (the QEMU rig used)

```sh
IMG=<scratch>/root.img
sh amd64/mkdisk.sh $IMG 256
# two kot nodes from the dev roster, polling each other over loopback;
# mimi on 8080 so run.sh's HTTP_PORT forward reaches it from the host
#   /root/kot/mimi.sh: MIOT_NAME=mimi MIOT_PORT=8080 MIOT_DB=/root/kot/db/mimi.db MIOT_PEERS=http://127.0.0.1:9945; exec /root/kot/bin/kot run
#   /root/kot/tama.sh: MIOT_NAME=tama MIOT_PORT=9945 MIOT_DB=/root/kot/db/tama.db MIOT_PEERS=http://127.0.0.1:8080; exec ...
#   /etc/herd/enabled/{mimi,tama}.conf: command=/bin/sh, args=/root/kot/<n>.sh, restart=true
# (written in with `debugfs -w -R "write …"`; kot from ../akuma-miot/dist/x86_64/kot)
DISK=$IMG INIT=/bin/herd SMP=2 HTTP_PORT=8091 SSH_PORT=2291 sh amd64/run.sh
# then, for 10 minutes: 4 parallel `curl -m 8 http://127.0.0.1:8091/mesh/status` per second
```

No LLM is needed — without `--llm`/`--glm` kot runs node-only. The failure
shows in under 10 minutes on the broken kernel. Inside the guest,
`cat /proc/net/tcp` is the one-look diagnosis: a listener `BACKLOG` of
`0/N/0` plus a pile of `CLOSE_WAIT` is this bug.

## 6. Found on the way — not fixed

### 6.1 OPEN: a kot worker on the metal spins on `writev` (~650 k/s)

Ten minutes after the metal came up, `ps` showed one `tokio-rt-worker`
(PID 33) at **6:15 of CPU** while its siblings had seconds. PSTATS:

```
PID 33 (tokio-rt-worker) 443.24s: 286660534 syscalls (646735/s) in_kernel=330617ms
  | epoll_create1=277798331(216805ms) accept=73883 memfd_create=4893 nr228=8766607 …
```

**PSTATS labels on amd64 come from the aarch64 table** (see
[`MIOT_MESH_ON_AKUMA.md`](MIOT_MESH_ON_AKUMA.md) §4): `epoll_create1` is raw
**20 = x86_64 `writev`**, `accept` is 202 = `futex`, `nr228` is
`clock_gettime`. So: ~278 M `writev`s in 7 minutes, steady (~850 k/s between
two samples 30 s apart), one core of four burned. kot still serves and its
agent loop still completes tool calls, so this is a CPU-burn defect, not a
wedge.

What distinguishes the metal node: 4 cores, and **its agent talks to GLM on
`api.z.ai` over HTTPS**, where ryzen-fc talks plain HTTP to a llama-server on
the host — and ryzen-fc's workers run at ~300 syscalls/s with no spinner. The
z.ai-over-TLS path on this box already has a history
([`AKUMA_AMD64_MEOW_TLS_STALL.md`](AKUMA_AMD64_MEOW_TLS_STALL.md): EOF on a live
socket after think time). Candidates, untested: `sys_writev`
(`amd64/src/usermode.rs`) returning `0` in some case a caller loops on — it
loops `sys_write` per iovec and returns the running total, so an iovec array
whose entries are all zero-length, or a first `sys_write` of `0`, yields `0`,
which a caller may retry forever; or a socket whose `EPOLLOUT` is now re-armed
by glue while `can_send()` keeps saying yes and the write keeps saying
`EAGAIN`. First step: identify the fd (strace the process, or add a one-shot
counter keyed by fd in `sys_writev`), then whether it's a socket.

The `[BKL] stuck … tag=502` lines on the metal (24 in the first ten minutes)
are **not** evidence either way: `tag=502` is `HOLD_TAG_IDLE`, and on a
kernel worker it means *unattributed*, not idle. They may be this spinner.

### 6.2 Boot self-test race at SMP>1

`spawn: the child's registered table holds fd 0 as Stdin` (and its fd 1/2
siblings) failed once on a 2-vCPU ryzen boot (786/789). The test inspects the
`hello` child's fd table after `sys_spawn` returns, and its comment calls that
"the one moment the table is guaranteed populated" — true at SMP=1 only; on
two cores the child can run, exit and `close_all` before the parent looks.
Unrelated to this change; the same kernel passed 789/789 on the same guest a
boot earlier.

### 6.3 `sendfile` (x86_64 40) has no row

kot probes it once at startup (`[syscall] no row for x86_64 nr=40`) and falls
back. Harmless today; same shape as `accept4` and `fadvise64` were.

### 6.4 Handoff leads 3 and 4

Multi-core BKL park starvation and the `kill -9`-on-a-thread-PID metal wedge
were **not needed** to explain this and are untouched. The metal's "connection
refused" was this bug, not lead 3.

### 6.5 Possibly the same bug: the "worker thread never runs" row

`docs/README.md`'s matrix has an OPEN row — *"a service accepts TCP in 0.01 s
and answers nobody"* ([`AMD64_SPAWNED_THREAD_NEVER_RUNS.md`](AMD64_SPAWNED_THREAD_NEVER_RUNS.md)).
Its localization (the child dies in its first `write(2)`) is different, so this
is not a claim that it is fixed — but the headline symptom is this one's, and
it is worth re-running against this kernel before chasing it further.

## 7. Deployment state after this session

- **ryzen Firecracker guest** (`192.168.1.50`): new kernel;
  `akuma-vm.json` **`vcpu_count: 2`** (was 1); previous kernel saved as
  `~/akuma/akuma-amd64.prev`. kot is the herd service, started once per boot.
- **ryzen-linux** (kot on the ryzen host itself, `kot.service`): limited to
  **CPUs 2,3** by the drop-in `/etc/systemd/system/kot.service.d/cpus.conf`
  (`CPUAffinity=2 3`), to match the guest. `available_parallelism` honours the
  affinity mask, so tokio sizes itself to 2 workers there too.
- **trashcan metal** (`192.168.1.123`): installed with `kinstall` (md5
  `7abf9469…`); previous kernel (`22ac34d3`) is `/boot/akuma-amd64.prev`;
  **`/boot/akuma-amd64.good` was deliberately not promoted** — per the runbook,
  promote after the kernel has proved itself:
  `cp -f /boot/akuma-amd64 /boot/akuma-amd64.good && sync`. kot `herd enable`d.
- The QEMU rig was stopped.

## 8. Traps hit this session (so the next one doesn't)

- **A single `200` is not a fix.** The accept4-only kernel answered its first
  requests perfectly. Only the 10-minute concurrent soak showed it was still
  broken — soak before you deploy.
- **The metal Akuma is at `192.168.1.123`** — DHCP gives it the same address
  Ubuntu has. `192.168.1.220` is only the no-DHCP static fallback
  (`BARE_METAL_STATIC_V4`). Polling `.220` waits forever on a healthy box.
- **`/var/log/herd/kot.log` persists across boots.** Three `[node] … on
  0.0.0.0:9944` banners in it are three boots, not three crashes — count
  `[herd] Started kot` in *this* boot's console instead.
- **The guest's ssh host key changes every boot** (sshd generates it), so
  `ssh` to `192.168.1.50` prints the MITM warning; use
  `-o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no`.
- **Host shell traps when writing watch loops on the Mac:** the default shell
  is zsh, which does not word-split `set -- $var`; and `/bin/bash` there is
  3.2, which has no `declare -A`. A monitor built on either silently probes the
  wrong host.
- **PSTATS names are aarch64's** on amd64 — translate the raw number through
  the x86_64 table before believing a label (`epoll_create1` = `writev`,
  `accept` = `futex`).

## Background

- [`AKUMA_AMD64_KOT_REPLICA_WEDGE.md`](AKUMA_AMD64_KOT_REPLICA_WEDGE.md) — the handoff: symptoms, rig, leads.
- [`MIOT_MESH_ON_AKUMA.md`](MIOT_MESH_ON_AKUMA.md) — the aarch64 side of the mesh work, and the metal evidence (§3, §4).
- [`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md) — the same shape one layer over: a `crate::sock` arm that diverged from glue's.
- [`AKUMA_AMD64_MEOW_TLS_STALL.md`](AKUMA_AMD64_MEOW_TLS_STALL.md) — the z.ai TLS history relevant to §6.1.
- `crates/akuma-syscalls-glue/src/poll.rs` — `epoll_on_fd_drained` / `epoll_on_fd_write_blocked` and why each exists.
- [`../runbooks/debug-delayed-first-byte.md`](../runbooks/debug-delayed-first-byte.md) — the aarch64 incident that introduced the `EPOLLOUT` re-arm.
