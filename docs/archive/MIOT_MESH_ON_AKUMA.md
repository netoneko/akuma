# akuma-miot's mesh on Akuma — 2026-09-22

What `../akuma-miot` did to this repo, and what running its new 5-agent
mesh on Akuma found. Session narrative plus findings. Edits: two overlay
scripts, one README row, and one aarch64 syscall arm (`fadvise64`,
finding 1) plus a wrong syscall number in a doc comment.

`akuma-miot` merged its node and agent into one binary (`kot`) and added
leader election (a Raft-style `miot-mesh`, election only). Every `kot run`
is a mesh member: an HTTP server (axum) *and* an HTTP client (reqwest). It
polls every peer's `/mesh/status` once a second, a replica pulls the
primary's block log every 2 s, and the in-process agent loop polls its own
node over loopback every 700 ms. That's far more TCP churn than the old
`miot node` primary ever put on an Akuma box. The findings below all come
from that load.

## Changes made here

| file | change | why |
|---|---|---|
| `overlays/devbox-firecracker/host-setup.sh` | creates the Lima VM with guest ports **9944–9949 forwarded on `0.0.0.0`** (`LIMA_LAN_PORTS`), and refuses an existing instance without that rule | Lima forwards guest listeners to the Mac's *loopback* only by default, so other LAN hosts could not reach a node in `fc`. The `fc` VM was deleted and recreated with this (operator OK'd). |
| `overlays/devbox-firecracker/README.md` | the `host-setup.sh` row mentions the LAN ports | |
| `crates/akuma-syscalls-glue/src/lib.rs` | `nr::FADVISE64 => 0` | the aarch64 dispatcher had no arm; parity-db couldn't reopen a DB (finding 1) |
| `crates/akuma-syscalls-abi/src/lib.rs` | doc comment: asm-generic `fadvise64` is 223, not 233 | 233 is `madvise`; the code already used 223 |
| `overlays/devbox-firecracker/guest-setup.sh` | the "socat already forwarding" check is `pgrep -f '^socat TCP-LISTEN:$SSH_PORT'`, was `pgrep -f 'socat.*$SSH_PORT'` | **the old check matched its own `sh -c` command line**, so it always reported the 4444 forward as running and never started it. Found on a freshly recreated `fc`: no socat at all, and `ssh -p 4444 root@localhost` refused. |

Outside the scripts: `fc` also runs `kot-relay-mac-fc.service` (socat
`fc:9945` → `10.0.2.15:9944`), installed by akuma-miot's
`overlays/deploy/deploy.sh`, so the LAN can reach a node inside
`akuma-guest`. Lima exposes only sockets *listening in fc*, so a relay is
the only way to put the nested guest on the LAN.

## Findings

### 1. aarch64: `fadvise64` has no dispatch arm → ParityDB can't reopen

`kot` on `akuma-guest` (aarch64, `781cfa92-release-smp-shared`,
`FC_FEATURES=devbox-smoltcp,no-tests`) runs fine on a **fresh** ParityDB,
and dies on the next start:

```
kot: open store at "/root/kot/db/kot.db": parity-db: IO Error: Function not implemented (os error 38)
```

`storeprobe` (akuma-miot `crates/miot-store/src/bin/storeprobe.rs`)
reproduces it standalone: stages 1–5 pass, **stage 6 (reopen across a
process boundary) aborts with `Os { code: 38 }`**. That's the same stage
and errno the amd64 metal box failed at before its 2026-09-22 fixes.

The `fadvise64` row went in shared (`akuma-syscalls-abi`: x86_64 221,
asm-generic 223), but only one dispatcher handles it:
`amd64/src/usermode.rs:1551` (`Syscall::Fadvise64 => 0`). The aarch64
dispatcher, `crates/akuma-syscalls-glue/src/lib.rs`, has **no
`nr::FADVISE64` arm**, so parity-db's `posix_fadvise` at open falls through
to ENOSYS. **Fixed in this session**: `nr::FADVISE64 => 0` in
`akuma-syscalls-glue`, the aarch64 twin of the amd64 arm. (The ABI crate's
comment also said "asm-generic 233"; 233 is `madvise`, and the number is
**223**. Fixed too; the code already used 223.) Verified on a rebooted
guest: `kot` reopened its existing 622-block store and replayed it
(`[node] replaying 622 block(s)`) where it had died with ENOSYS before.

**A second aarch64 gap is behind it, and it loses data.** With the ENOSYS
gone, `storeprobe` stage 6 now *reopens*, but finds the store **empty**:
`assertion left == right failed, left: 0, right: 200` (`head()` after
reopen). Reading, unproven: parity-db writes land in its `write()`-based
**log** files first, and a clean close enacts them into the index/table
files, which parity-db writes through **writable `MAP_SHARED` mmaps**, and
retires the logs. If aarch64 never writes mapped pages back to the file,
**a clean close loses everything since the last reopen, and a crash loses
nothing**: the logs survive and replay. That fits both observations. kot
was killed uncleanly (firecracker restarted under it) and came back whole;
storeprobe closes cleanly and came back empty. It's exactly the other half
of the amd64 fix (`docs/reference/subsystems/amd64-shared-write-mmap.md`:
demand-paged writable `MAP_SHARED` with whole-region write-back on
`munmap`/`msync`), which aarch64 never got. akuma-miot's `mmapprobe`
would pin it, but it is x86_64-only (raw syscalls); an aarch64 build of it
is the next step. Until this is fixed, **a clean shutdown of anything
ParityDB-backed on aarch64 Akuma is the dangerous one.**

"storeprobe is fixed" is true of the amd64 metal box, not of aarch64.

Likely what `node4`'s old "index-growth panic" (`parity-db index.rs:237`)
was masking or related to. Not re-checked.

### 2. amd64 metal: a `kot` *replica* goes deaf within minutes

On the trashcan (`ssh akuma`, `22ac34d3-release-smp-shared`), `kot run` as a
**replica**, reproduced across a reboot:

- Starts clean: replays its store, binds `:9944`, follows the primary, the
  agent connects.
- Within minutes `:9944` answers **connection refused, even from
  `127.0.0.1` on the box**, while the process lives on. Replica sync stops
  too (head frozen, peers mark it stale).
- PSTATS (numbers read as x86_64, see finding 3): the tokio workers are
  parked. `futex` (202) takes 125–162 s of 213 s, `epoll_pwait` (281) 27–55 s.
  The runtime is idle, not spinning; `/proc/*/status` saying `R` doesn't
  mean busy here.
- Throughout: a `[BKL] stuck: owner=3 … tag=502 spins=8388608` storm
  (`HOLD_TAG_IDLE`, benign in the symptom matrix's other case).

Refused-from-loopback means no listener at all. Hypothesis, not confirmed:
**the listener pool drains.** `SocketType::Listener` is `MAX_BACKLOG`
smoltcp handles that must each find their way back to `Listen`
(`reference/subsystems/networking.md` "The listener is a pool"). One
leaking path, reset-before-accept, was fixed 2026-08-20. Another remaining
one would produce exactly this. A replica's traffic is that churn: two
peers poll it every second with a 2 s client timeout, so slow answers end
as aborted connections, and it opens its own outbound polls and pulls. The
old `miot node` on this box was durable, but only as a primary nobody
polled. As a replica it wedged mid-catch-up before the mmap fixes, and
that path was never re-tested until now.

Next step if chased: a probe that runs connect-then-abort churn against a
listener and reports how many pool handles are in `Listen`. Cheap, and it
confirms or kills the pool theory before any kernel code is read.

### 3. PSTATS names amd64 syscalls from the aarch64 table

`crates/akuma-exec/src/process/stats.rs::syscall_name` is the asm-generic
(aarch64) table, and amd64 prints through it. So on amd64 every *named*
PSTATS entry is wrong: `accept` is really futex (202), `memfd_create` is
epoll_pwait (281), `io_setup` is read (0), `epoll_create1` is writev (20),
`inotify_init1` is msync (26), `ftruncate` is sendmsg (46). Only the raw
`nrN` entries, which have no aarch64 name, are right, which is why the
symptom matrix reads `nr43` as accept. This cost time reading finding 2:
kot appeared to call `accept`/`memfd_create`/`io_setup` hundreds of times.
Fix: pick the table by target.

### 4. The Sep-17 `disk.img` herd: no config reload, no respawn after exit

On `akuma-guest` booted from the repo's `disk.img` (dated 2026-09-17):
`herd enable kot` added the conf, and `herd status` listed it, but it
never started until a reboot. This herd predates config reload, the same
as the metal box's old herd (akuma-miot HANDOFF traps). And after `kot`
exited on the ENOSYS above, herd **did not restart it** despite
`restart = true`. Refresh the disk's `/bin/herd` before relying on either.

### 5. For contrast: aarch64 as primary and as replica, same binary

With the fresh-store caveat from finding 1, the aarch64 guest ran the exact
role that wedges the metal box, and didn't wedge:

- **Primary**, 10 min: 100 blocks, one every ~6 s, both Linux peers
  following within a block, polled by two peers throughout.
- **Replica** after a restart with a wiped store: caught up **514 blocks
  from genesis in under a minute** and tailed the primary block for block
  for 11 minutes (head 616 vs the leader's 617 at the end), never stale.
  That's twice as long as the metal box ever lasted as a replica.

So the replica wedge in finding 2 is amd64-kernel-specific, or specific to
that box's NIC path. It isn't `kot` and it isn't the workload shape.

## Where to read

- akuma-miot `HANDOFF.md` (traps), `docs/TOPOLOGY_TARGET.md` (which agent
  runs where), `overlays/deploy/deploy.sh` (how things get onto these boxes).
- This repo: `docs/reference/subsystems/networking.md` (socket lifetime,
  listener pool), `docs/reference/subsystems/amd64-shared-write-mmap.md`
  (the amd64 fixes finding 1 is missing on aarch64).
