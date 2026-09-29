# amd64: a `kot` mesh node goes deaf within minutes — handoff, 2026-09-23

## Resolution (2026-09-23, later the same day): two amd64-only socket gaps

**Status: FIXED** for the symptom below — full write-up in
[`AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md`](AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md),
including a new open defect found afterwards (a kot worker spinning on
`writev` on the metal). The rest of this document is the handoff as written,
kept for its rig notes. Two defects, both the tree's
characteristic amd64 shape — the AArch64 path had it, the amd64 path was a
second implementation that didn't:

1. **`accept4` (x86_64 288) had no ABI row** (lead 1). Added:
   `Syscall::Accept4 => 288, nr::ACCEPT4`, routed to a new
   `amd64/src/sock.rs::sys_accept4` that applies `SOCK_NONBLOCK`/`SOCK_CLOEXEC`
   to the *accepted* fd and refuses other bits with `EINVAL`. Necessary, not
   sufficient: with only this, the QEMU soak below still failed.
2. **The real wedge: amd64's own `recvfrom`/`recvmsg`/`sendto`/`sendmsg` never
   re-armed the `EPOLLET` edge.** Glue's versions call
   `poll::epoll_on_fd_drained` after every TCP read (and on `EAGAIN`) and
   `epoll_on_fd_write_blocked` on a short write/`EAGAIN`; `crate::sock`'s did
   neither. tokio registers every socket edge-triggered and reads through
   `recv(2)` = `recvfrom` — so an accepted connection's first `EPOLLIN` was its
   last. Its task never woke again, the peer's FIN went unread, and the listener
   backlog filled with connections nobody accepted. `read(2)`/`write(2)` go
   through glue and always re-armed, which is why busybox, sshd and httpd never
   showed it. Also: `accept` returning `EAGAIN` now re-arms the listener's
   `EPOLLIN`.

The evidence that pinned (2), from `/proc/net/tcp` on the wedged guest while
sshd still answered: the kot listener's `BACKLOG` read **`0/8/0`** (0 listening,
8 established-unaccepted) against `8/0/0` on healthy listeners, plus ~40
accepted connections in **`CLOSE_WAIT`** — client gone, server never noticed.
Every kot thread was parked in `futex` or `epoll_pwait`.

| run | kernel | result |
|---|---|---|
| QEMU TCG, SMP=2, two kot nodes polling each other + 4 parallel host clients, 10 min | accept4 only | **182 ok / 1946 failed**, `0/8/0`, CLOSE_WAIT pile |
| same | accept4 + re-arm | **2224 ok / 0 failed**, every listener back to `8/0/0`, 0 CLOSE_WAIT, no `[BKL] stuck` |
| ryzen-fc Firecracker, **2 vCPUs** (bumped from 1) | accept4 + re-arm | follower of mac-linux, caught up to leader head |
| trashcan metal, 4 cores, rtl8169 (`192.168.1.123`) | accept4 + re-arm | 743/743 self-tests, follower of mac-linux, caught up to leader head |

Also removed: the temporary `[wp0>]`/`[wp1>]`/`[wp>]` per-write trace (lead 2).
Leads 3 and 4 (multi-core BKL park starvation, `kill -9` wedge) were not
needed to explain this and are **not** addressed.

Found on the way, not fixed: the boot self-test `spawn: the child's registered
table holds fd 0 as Stdin` (and its fd 1/2 siblings) races at SMP>1 — the
`hello` child can exit and `close_all` on the other core before the parent
inspects its table (seen once on ryzen-fc at 2 vCPUs, 786/789). And
`sendfile` (x86_64 40) still has no row; kot probes it once at startup.

---

**Original status: OPEN. Written for the next agent; nothing here is fixed.** The
aarch64 side of the same work is done and documented in
[`MIOT_MESH_ON_AKUMA.md`](MIOT_MESH_ON_AKUMA.md). Read that doc's §3 and §4
first; this one supersedes them for the amd64 wedge.

## What `kot` is, in one paragraph

akuma-miot's single binary (`../akuma-miot`, `dist/x86_64/kot`, static musl,
11 MB). `kot run` is a chain node plus an LLM agent loop. It's a multi-threaded
**tokio** runtime running an **axum** HTTP server on `:9944` and a
**reqwest** client. Durable on ParityDB. As a mesh member it:
- polls every peer's `/mesh/status` once a second (2 s client timeout);
- pulls the primary's blocks every 2 s when it's a replica;
- has its own agent loop poll it over loopback every 700 ms.

Each peer does the same to it. So it's a busy multi-threaded TCP server
*and* client at once. **Don't edit kot's source**: another agent owns it.
The kernel is the thing to fix.

## Symptom

Within minutes of starting as a mesh member, the node stops answering HTTP:

| where | kernel | cores | NIC | what "deaf" looks like |
|---|---|---|---|---|
| trashcan metal (`ssh akuma`) | `22ac34d3-release-smp-shared` | 4 | rtl8169 | `:9944` **connection refused**, even from `127.0.0.1` on the box; replica sync stops; process alive, threads `R`; `[BKL] stuck: owner=3 … tag=502 spins=8388608` storm in `dmesg`. Reproduced across a reboot. |
| ryzen Firecracker guest (`192.168.1.50`) | `c0487c92-release-smp-shared` (this tree, 2026-09-22) | **1 vCPU** | virtio-net | `:9944` **accepts TCP, never answers HTTP** (curl `000` after 8 s); then sshd on `:2222` also goes deaf (TCP ok, banner exchange times out); then the **serial console goes completely silent** (0 bytes in 45 s, not even herd's 20 s `Reloading config`); **ping still answers**. |
| aarch64 Firecracker guest (`akuma-guest` in Lima `fc`) | `781cfa92`+this session's fixes | 2 vCPUs | virtio-net | **never**: 10 min as primary, 11 min as replica, same binary, same mesh, same traffic. |

The old `miot node` on the trashcan (`node5`) *was* durable for a day or
more, but only as a primary nobody polled. It had almost no inbound traffic
and no outbound HTTP.

## Answers already given (don't redo)

- **Multi-core starvation on Firecracker: not observed on either
  architecture.** aarch64 at 2 vCPUs ran clean. amd64 Firecracker has only
  been run at **1 vCPU**. The multi-core BKL-park starvation in
  [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md) (a parked
  holder freezes `owner`, and *other cores* starve) is a plausible fit for the
  4-core metal box. It **cannot** explain the 1-vCPU wedge: the lock is
  reentrant by owner core, so every task on a single core free-rides it.
- **The state of that BKL fix, as of 2026-09-20** (from that doc's
  "(night)" section):
  1. **Landed** and verified on metal: the `prev` handoff in
     `x86_yield_now`/`x86_pick_next`, the pick's 0→1 CAS claim, and
     `rust_switch_finished()` clearing the predecessor's gate.
  2. **Reverted**: release-across-park in `sched::block_current` /
     `block_until_deadline`. It wedged the box at the netpoll daemon's first
     parks (`[SWITCH NO-BKL] … via=block_until_deadline`), twice. The audit
     it's waiting on: `publish_waiting_and_take_pending_wake`,
     `x86_wake_pass`, and the switch's POOL accounting.
  3. **Not started**: a ring-3-return reconcile mirroring aarch64's
     `reconcile_for_spsr`.

  Also landed and verified: `yield_now`'s BKL drop window.
  (Its dropped-window exception, which let a thread switch without the lock, was
  removed 2026-09-26 after two `[SWITCH NO-BKL] … via=yield_now` hangs:
  [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md) 2026-09-26.)

## Leads, ranked by how much of the symptom each explains

### 1. `accept4` (x86_64 nr 288) has no amd64 ABI row → ENOSYS. **Strongest, and not core-count-specific.**

The ryzen guest's console, right after kot started:

```
[herd] Started kot (pid=115)
[syscall] no row for x86_64 nr=288 — returning ENOSYS (add it to akuma-syscalls-abi's table)
```

288 is `accept4`. tokio accepts through mio, which uses `accept4`
(`SOCK_CLOEXEC|SOCK_NONBLOCK`). `crates/akuma-syscalls-abi/src/lib.rs` has
**no** `Accept4` row. `git grep -i accept4` hits only the aarch64
dispatcher: `crates/akuma-syscalls-glue/src/lib.rs:776`,
`nr::ACCEPT4 => net::dispatch_accept(…)` ("`accept` is `accept4` with
flags == 0; one path serves both"). The trashcan's kernel `22ac34d3` has
no row either. This is the tree's characteristic amd64 failure (see
`runbooks/amd64-bare-metal-loop.md`: "a prior fix may be real *and* the bug
still present, because the fix landed in AArch64-only code"), and the same
shape as this session's `fadvise64` bug.

What it explains: the smoltcp listener completes handshakes (so clients
see TCP connect succeed, the ryzen symptom), while the application's
`accept4` fails every time, so no request is ever read. On the metal box, a
backlog that never drains turns SYNs into RSTs: connection refused.

What it does **not** explain yet, so check these first:
- **Some requests were answered.** The agent loop printed `connected.`
  after a successful `GET /head` on loopback, and a peer saw the metal node's
  `/mesh/status` once. Either an accept path other than 288 works (does mio
  fall back to `accept` (43) on ENOSYS? Does axum's accept loop sleep 1 s and
  retry?), or the ENOSYS comes from a specific path. `strace` on the kernel
  command line, or PSTATS counts of 288 versus 43, will say.
- **sshd and the whole console dying afterwards** on ryzen. That points to
  a second problem (lead 2 or 3), or to the accept-error loop spinning the
  single vCPU.

The fix, if it holds: an `Accept4` row in the ABI (x86_64 288, asm-generic
242) routed to the same `net::dispatch_accept` with `flags`. Put a host test
next to the `(Syscall::Fadvise64, 221, 223)` table entry.

### 2. The `[wp>]` per-write console trace, printed while holding the BKL

`amd64/src/usermode.rs` has a **temporary** trace from 2026-09-20 (commit
`91532c68`, the never-runs thread hunt): `[wp0>]`/`[wp1>]` in the syscall
entry path, and `[wp>] thread write task=… fd=… len=…` / `returned …` in
`sys_write`. It prints for **every `write(2)` from any non-leader thread**,
which for a tokio process is every socket write. The ryzen log has 660 such
lines, 360 of them from task 6 alone. Console output costs about
2,400 ns per byte (one trapping MMIO store each, `CONSOLE_LOG_COST.md`), so
each write carries about 250 µs of serial time, and the `sys_write` half runs
with the BKL held. It's marked "Remove once the never-runs thread is
understood". [`AMD64_SPAWNED_THREAD_NEVER_RUNS.md`](AMD64_SPAWNED_THREAD_NEVER_RUNS.md)
§0 then traced that to the BKL itself ("the child is a victim, not a
patient"), so the trace has served its purpose. Remove it before judging
anything else by timing.

### 3. The multi-core BKL park starvation (metal only)

See "Answers" above. It's relevant to the 4-core trashcan, and
`[BKL] stuck … owner=3` is in its `dmesg`. It's irrelevant at 1 vCPU. Only
worth chasing once leads 1 and 2 are closed and the metal box still wedges.

### 4. Known side trap: `kill -9` on a thread-PID can wedge the metal box

Already recorded in `AKUMA_AMD64_BKL_NETWORKING.md` ("(later)"). It
happened again 2026-09-22: right after akuma-miot's `deploy.sh retire-old`
killed the old `miot node` by PID, sshd on the metal box kept authenticating
but every exec returned status 241 until a power cycle. Let herd restart
things; don't `kill` by hand there.

> **Correction (2026-09-24):** the "every exec returned 241" half of this is
> root-caused and fixed, and it was neither `kill -9` nor a thread-PID. 241 is a
> SIGTERM death. The kill took out a kot worker, which stamped kot's process slot
> with a group death status that `sys_spawn` never cleared, so every later spawn
> into that slot died of SIGTERM. See
> [`AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md`](AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md).

## The rig: ryzen Firecracker guest (cheap to reboot)

This is the place to iterate. A reboot is a process restart, with no
walking to a machine.

- **Host:** ryzen (`ssh ryzen`, root; Pop!_OS, Ryzen 7 8845HS, real KVM).
  Files: `/home/netoneko/akuma/`, owned by `netoneko`:
  - `akuma-amd64`: kernel, this tree at `c0487c92`
  - `disk.img`: 1 GiB, fresh from `amd64/mkdisk.sh`
  - `akuma-vm.json`: **1 vCPU**, 2 GiB, `tap0`, MAC `02:FC:00:00:00:01`, `init=/bin/herd`
  - `run.sh`: the host's launcher
  - `boot.log`: the serial console
  - the Sep-20 disk backups
- **Network**, reused as it was: `tap0` on the host carries `192.168.1.49`
  plus proxy-ARP, and dnsmasq pins the guest to **`192.168.1.50`** (LAN
  address, reachable from everywhere). Nothing new was added on the host.
- **Get in:** `ssh -p 2222 -i ../akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key root@192.168.1.50`.
  Only works while the guest isn't wedged.
- **Console:** `ssh ryzen 'tail -f /home/netoneko/akuma/boot.log'`. After a
  wedge, this is all that's left.
- **Reboot:** kill firecracker **by PID** (`ps -eo pid,comm | awk
  '$2=="firecracker"'`), then as `netoneko`:
  `cd ~/akuma && setsid nohup env TIMEOUT=0 ./run.sh >/dev/null 2>&1 </dev/null &`.
  **Never `pgrep -f`/`pkill -f` a pattern inside an `ssh … '…'` string**: the
  remote `sh -c` contains the pattern and matches itself. It killed the
  controlling shell three times on 2026-09-22.
- **New kernel:** `cargo build -p akuma-amd64 --target x86_64-unknown-none --release`,
  scp to `…/akuma/akuma-amd64`, reboot. (Or `FC_HOST=netoneko@192.168.1.126
  FC_KEEP_DISK=1 amd64/run-firecracker.sh`, which stages only the ELF. But
  it runs `run.sh` in the foreground with `TIMEOUT`.)
- **kot in the guest:** `/root/kot/{bin/kot,start.sh,persona.md,id_ed25519.seed}`,
  herd service `kot` (`/etc/herd/{available,enabled}/kot.conf`). Log:
  `/var/log/herd/kot.log`. Its LLM is a llama-server on the host at
  `192.168.1.49:8082` (`llama-ryzen-fc.service`).
  - Redeploy: `../akuma-miot/overlays/deploy/deploy.sh up ryzen-fc`. Pulling
    the 11 MB binary into this guest takes about 2 minutes (~90 KB/s
    inbound, itself worth a look).
  - Reproduce the wedge: start it and wait a few minutes. It joins the mesh
    (ryzen-linux, mac-linux, mac-fc) as a follower.
- **Rebuilding a disk:** `sh amd64/mkdisk.sh <img> 1024`. As of this session
  `mkdisk.sh` pre-grows `/bin` and fails if `/bin/sh` is missing. Before
  that, a full `/bin` directory block silently dropped every applet from
  `pwd` on, including `sh`, and sshd answered every exec with
  `failed to spawn '/bin/sh'`. That's also the "ryzen guest had no `/bin/sh`"
  in `LITTER_TRASHCAN_RYZEN_JOIN.md`.

**State at handoff (2026-09-23):**
- The ryzen guest is **wedged** in the state described above. Its console
  is silent, but it's left running in case its state is worth anything;
  reboot it to start fresh.
- The trashcan: kot is `herd disable`d, and `/root/kot` is staged.

## Suggested order

1. Add the `accept4` row. Re-run kot on the ryzen guest (1 vCPU). Watch
   `curl http://192.168.1.50:9944/mesh/status` for 10+ minutes.
2. Remove the `[wp>]`/`[wp0>]`/`[wp1>]` trace in the same build or the
   next.
3. Set `vcpu_count` to 2 in `akuma-vm.json` and repeat. That's the missing
   multi-core Firecracker data point.
4. Only then take it to the metal. Per the runbook: the fast lane first, and
   `cp /boot/akuma-amd64 /boot/akuma-amd64.prev` before pushing. Remember
   the metal's NIC (`rtl8169`) isn't exercised by any Firecracker or QEMU
   run.
5. If the metal still wedges after 1–2, it's lead 3.

## Where to read

- [`MIOT_MESH_ON_AKUMA.md`](MIOT_MESH_ON_AKUMA.md): the aarch64 fixes, and
  the metal-box evidence (§3, §4: PSTATS on amd64 names syscalls from the
  **aarch64** table, so `accept` there is really `futex`).
- [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md),
  [`AMD64_SPAWNED_THREAD_NEVER_RUNS.md`](AMD64_SPAWNED_THREAD_NEVER_RUNS.md):
  the BKL park work.
- `runbooks/amd64-bare-metal-loop.md`: the trashcan's two personalities,
  `reboot -f`, the rigs, what not to do.
- akuma-miot: `HANDOFF.md` (traps), `docs/TOPOLOGY_TARGET.md`,
  `overlays/deploy/deploy.sh` (shapes `akuma`, `fcguest`).
