# akuma-miot's mesh on Akuma — 2026-09-22

What `../akuma-miot` did to this repo, and what running its new 5-agent mesh
on Akuma found and fixed. Session narrative plus findings.

**Outcome.** ParityDB now survives on the **aarch64** kernel in both
directions: a crash (`kill -9`) and a clean close then reopen. `storeprobe`
completes all 7 stages on `akuma-guest` (exit status 7). Before this it died
at stage 6 with ENOSYS, and then, once that was fixed, reopened to an empty
store. Two kernel bugs, both aarch64-only, both found by running akuma-miot's
`kot` there. The **amd64** metal box has a separate open problem (finding 3):
a `kot` replica goes deaf within minutes. Not fixed; it has a hypothesis and
a next step.

## Why this load is different

`akuma-miot` merged its chain node and agent loop into one binary (`kot`)
and added leader election (a Raft-style `miot-mesh`, election only). Every
`kot run` is a mesh member: an HTTP server (axum) *and* an HTTP client
(reqwest), durable on ParityDB.
- It polls every peer's `/mesh/status` once a second, with a 2 s timeout.
- As a replica it pulls the primary's block log every 2 s.
- Its in-process agent loop polls its own node over loopback every 700 ms.

That's far more TCP churn than the old `miot node` primary ever put on an
Akuma box. It also reopens its store on every restart, which the old
deployments rarely did on aarch64.

## Changes made here

| file | change | finding |
|---|---|---|
| `crates/akuma-syscalls-glue/src/lib.rs` | `nr::FADVISE64 => 0` in the aarch64 dispatcher | 1a |
| `crates/akuma-syscalls-glue/src/mem.rs` | lazy writable `MAP_SHARED` file mappings are **registered** for write-back and flushed by a **present-page walk** bounded by the file's current size; `msync`, `munmap` and exit all route through one `flush_shared_mapping` | 1b |
| `crates/akuma-syscalls-mem/src/mmap.rs` | the stale host test that asserted the pre-2026-09-22 rule (`!file_lazy_eligible` for shared-writable) now asserts the rule in force, renamed `shared_writable_file_is_lazy_eligible_and_marked_for_writeback` | 1b |
| `crates/akuma-syscalls-abi/src/lib.rs` | doc comment: asm-generic `fadvise64` is **223**, not 233 (233 is `madvise`; the code already used 223) | 1a |
| `overlays/devbox-firecracker/host-setup.sh` | creates the Lima VM with guest ports **9944–9949 forwarded on `0.0.0.0`** (`LIMA_LAN_PORTS`), and refuses an existing instance without that rule | 5 |
| `overlays/devbox-firecracker/README.md` | the `host-setup.sh` row mentions the LAN ports | 5 |
| `overlays/devbox-firecracker/guest-setup.sh` | the "socat already forwarding" check is `pgrep -f '^socat TCP-LISTEN:$SSH_PORT'`, was `pgrep -f 'socat.*$SSH_PORT'` | 5 |

Nothing on the amd64 side changed.

## 1. ParityDB on aarch64: two bugs, one symptom at a time

Test subject: `akuma-guest` (Firecracker nested in Lima `fc`), aarch64,
`FC_FEATURES=devbox-smoltcp,no-tests overlays/devbox-firecracker/build.sh`,
booted `--vcpus 2 --mem 2048` from the repo's `disk.img`. The probe is
akuma-miot's `storeprobe` (`crates/miot-store/src/bin/storeprobe.rs`,
shipped as `dist/aarch64/storeprobe`). It runs seven stages against a fresh
ParityDB: open, 256 appends, read-back, compact, rewind, **drop the handle
and reopen**, and a 64 KiB value. Exit status = stages completed.

### 1a. `fadvise64` had no aarch64 dispatch arm → reopen fails with ENOSYS

**Symptom.** `kot` ran fine on a fresh store and died on its next start:

```
kot: open store at "/root/kot/db/kot.db": parity-db: IO Error: Function not implemented (os error 38)
```

`storeprobe`: stages 1–5 pass; stage 6 panics with
`reopen: Db(Io(Os { code: 38, kind: Unsupported, … }))`, exit 134. Same
stage and errno as the amd64 metal box before its fixes.

**Cause.** parity-db calls `posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM)`
after opening every file and `try_io!`s the result (`parity-db
0.5.6/src/file.rs:34`), so ENOSYS aborts the whole open. It only runs on an
*existing* file, which is why a fresh store worked. The 2026-09-22 amd64
work added the row to the shared ABI (`akuma-syscalls-abi`: x86_64 221,
asm-generic 223) and an arm to **one** dispatcher,
`amd64/src/usermode.rs` (`Syscall::Fadvise64 => 0`). The aarch64
dispatcher, `crates/akuma-syscalls-glue/src/lib.rs`, had none, so the call
fell through to ENOSYS.

**Fix.** `nr::FADVISE64 => 0,` next to `nr::FTRUNCATE`. It's advisory by
Linux's own contract, there's no readahead state to tune, and it matches
the amd64 arm.

**Verified.** Rebooted the guest onto the new kernel. `kot` reopened its
existing store, written *before* the fix: `[node] replaying 622 block(s)`.

### 1b. Lazy writable `MAP_SHARED` was never written back → clean close loses everything

**Symptom.** With 1a fixed, `storeprobe` stage 6 *reopens*, but the store
is empty:

```
panicked at crates/miot-store/src/bin/storeprobe.rs:64:5:
assertion `left == right` failed
  left: 0      # head() after reopen
 right: 200
```

`kot` hadn't shown this, because it never closes its store cleanly. It is
killed, and a killed parity-db leaves its `write()`-based log files behind,
which replay on open. A clean close is different: parity-db enacts the logs
into its index/table files, which it writes through **writable
`MAP_SHARED` mmaps**, and retires the logs. On aarch64 those mmap writes
never reached the file.

**Cause: a cross-architecture regression from the amd64 fix of the same
day** (commit `22ac34d3`, "fix storage on amd64"):

1. aarch64's design for writable `MAP_SHARED` file mappings was: map
   **eagerly** (every page resident), record the mapping in
   `SHARED_FILE_MAPPINGS`, and on `munmap`/`msync`/exit copy the region's
   `frames` list back to the file. It relied on the shared
   `akuma_syscalls_mem::mmap::plan` returning `file_lazy_eligible = false`
   for shared-writable. The code said so: "Writable MAP_SHARED is forced
   eager (see below)".
2. amd64 needed the opposite. parity-db maps each **table** file at
   `len + 1 GiB` of reserve (`RESERVE_ADDRESS_SPACE`, `parity-db
   0.5.6/src/file.rs:70–76`), which no eager fill can back. So `22ac34d3`
   changed the **shared** plan to `file_lazy_eligible: is_file_backed`, and
   built amd64's own present-page write-back (`MmapRegion::SharedWriteBack`,
   `docs/reference/subsystems/amd64-shared-write-mmap.md`).
3. On aarch64 that sent shared-writable mappings down the **lazy-file**
   path in `akuma-syscalls-glue/src/mem.rs::sys_mmap`. That path returns
   early and **never registered the mapping in `SHARED_FILE_MAPPINGS`**. No
   record means no write-back on `msync`, on `munmap`, or at exit: every
   write through the mapping was silently dropped.
4. The plan crate's own host test,
   `shared_writable_file_is_eager_and_not_lazy_eligible`, still asserted
   `!file_lazy_eligible`, and can only have failed against the changed
   code. It wasn't being run. Host tests for these crates need
   `--target aarch64-apple-darwin` (the workspace default target is bare
   metal, and plain `cargo test` fails with "can't find crate for `test`").

Going back to eager isn't an option on aarch64 either. A 1 GiB reserve per
table file, eagerly zero-filled, on a 2 GiB guest, fails `ENOMEM`.

**Fix: aarch64 gets present-page write-back for lazy mappings**, the same
contract amd64 has but built on aarch64's own record
(`crates/akuma-syscalls-glue/src/mem.rs`):

- `SharedFileMapping` gains `lazy: bool`. The eager path records
  `lazy: false` and behaves exactly as before. That's what small mappings
  such as `rust-lld`'s output buffer still get, and the self-host link
  depends on it.
- The lazy-file branch of `sys_mmap` now records shared-writable mappings
  with `lazy: true` before it returns.
- New `writeback_present_pages(proc, path, base, file_offset, lo, hi)`:
  walks `[lo, hi)` a page at a time. `aspace.translate(va)` takes the
  address-space lock per page and drops it before any I/O. Each **present**
  page is copied out (`copy_from_phys`) and written at its file offset
  (`write_at`). The walk stops at the **file's size queried at flush
  time**: parity-db grows files with `ftruncate` before writing into the
  reserve, and nothing past the current end may be written, or a flush
  would extend the file by the whole reserve. Absent pages are skipped;
  they were never touched, or were read-only and evicted as clean.
- New `flush_shared_mapping(proc, base, m, lo, hi)` dispatches on `lazy`.
  `msync` (only the msync'd window, for lazy records), exit
  (`flush_and_clear_shared_file_mappings`) and `munmap` all go through it.
- `munmap` flushes any lazy record overlapping the range **before** the lazy
  pages are unmapped, and drops the record only when the whole mapping is
  gone. A partial unmap keeps it; the unmapped part simply has no present
  pages next time.

**Why reclaim isn't a hole.** `reclaim_clean_file_pages` evicts through
`try_evict_ro_page`, **read-only PTEs only**. A page that has been written
is RW and is never evicted; a read-only one still equals the file.

**Verified.**

```
storeprobe exit=7 (7 = all stages)
[db] 6 reopen — does it survive a process boundary?
[db] 7 a large value (64 KiB, the artifact cap)
[db]   on-disk apparent size: 99635 KiB
[db] all 7 stages complete
```

Crash direction: `kot` on the guest was `kill -9`'d, then a second `kot`
was started on the same store with **no peers**, so it couldn't sync and
anything it had must have come off the reopened store. It printed
`[node] replaying 753 block(s)`, all of it. mac-fc then rejoined the live
mesh at the leader's head (763). Host tests: `akuma-syscalls-mem` 41/41.

**Not covered, known.**
- No dirty tracking. Every *present* page below EOF is rewritten on each
  flush, including pages only ever read. That's extra I/O, never wrong
  bytes. A flush costs O(file size / 4 KiB) translations, not O(mapping
  size), because the walk stops at EOF.
- `MADV_DONTNEED` on a lazy shared-writable page drops it without a flush.
  amd64 flushes first (`dontneed_range`). parity-db only ever uses
  `MADV_RANDOM`, so it isn't hit here, but it's a real gap for other
  callers.
- A process killed with such a mapping live loses writes since its last
  flush (same as amd64). parity-db's own log covers that, and is why the
  crash direction worked even before this fix.
- Cross-mapper coherence: none, same as amd64. There's still no page cache.

**Probably related, not re-checked:** `node4`'s old intermittent parity-db
panic on this guest (`index.rs:237: range start index 512 out of range for
slice of length 1`). An index that never got written back reads back as
zeros, which is the kind of input that slicing fails on.

## 2. For contrast: aarch64 as primary and as replica, same binary

The aarch64 guest ran the exact role that wedges the metal box (finding 3),
and didn't wedge:

- **Primary**, 10 min: 100 blocks, one every ~6 s, both Linux peers
  following within a block, polled by two peers throughout.
- **Replica** (restart, wiped store): caught up **514 blocks from genesis in
  under a minute**, then tailed the primary block for block for 11 minutes
  (head 616 vs the leader's 617 at the end), never stale.

So finding 3 is specific to the amd64 kernel or that box's NIC path. It's
not `kot`, and it's not the workload shape.

## 3. amd64 metal: a `kot` *replica* goes deaf within minutes (open)

On the trashcan (`ssh akuma`, `22ac34d3-release-smp-shared`), `kot run` as a
**replica**, reproduced across a reboot:

- It starts clean: replays its store, binds `:9944`, follows the primary,
  and the agent connects.
- Within minutes `:9944` answers **connection refused, even from
  `127.0.0.1` on the box**, while the process lives on. Replica sync stops
  too: the head freezes and peers mark it stale.
- PSTATS, reading the numbers as x86_64 (see finding 4): the tokio workers
  are parked. `futex` (202) takes 125–162 s of 213 s, `epoll_pwait` (281)
  27–55 s. The runtime is idle, not spinning; `/proc/*/status` saying `R`
  doesn't mean busy here.
- Throughout, a `[BKL] stuck: owner=3 … tag=502 spins=8388608` storm
  (`HOLD_TAG_IDLE`, benign in the symptom matrix's other case).

Refused-from-loopback means no listener at all. Hypothesis, not confirmed:
**the listener pool drains.** `SocketType::Listener` is `MAX_BACKLOG`
smoltcp handles that must each find their way back to `Listen`
(`reference/subsystems/networking.md`, "The listener is a pool"). One
leaking path, reset-before-accept, was fixed 2026-08-20; another remaining
one would produce exactly this. A replica's traffic is that churn: two
peers poll it every second with a 2 s client timeout, so slow answers end
as aborted connections, and it opens its own outbound polls and pulls. The
old `miot node` on this box was durable, but only as a primary nobody
polled.

**Next step:** a probe that runs connect-then-abort churn against a
listener and reports how many pool handles are in `Listen`. Cheap, and it
confirms or kills the pool theory before any kernel code is read.
Meanwhile akuma-miot runs the mesh without the metal box (herd `kot`
disabled there).

## 4. PSTATS names amd64 syscalls from the aarch64 table (open)

`crates/akuma-exec/src/process/stats.rs::syscall_name` is the asm-generic
(aarch64) table, and amd64 prints through it. So on amd64 every *named*
PSTATS entry is wrong:

| printed | x86_64 nr | really |
|---|---|---|
| `accept` | 202 | `futex` |
| `memfd_create` | 281 | `epoll_pwait` |
| `io_setup` | 0 | `read` |
| `epoll_create1` | 20 | `writev` |
| `inotify_init1` | 26 | `msync` |
| `ftruncate` | 46 | `sendmsg` |

Only the raw `nrN` entries, which have no aarch64 name, are right. That's
why the symptom matrix reads `nr43` as accept. This cost time on finding 3:
kot appeared to call `accept`/`memfd_create`/`io_setup` hundreds of times.
Fix: pick the table by target.

## 5. Tooling found along the way

- **Lima exposes guest ports on the Mac's loopback only.** A node in `fc`
  was invisible to the LAN. `host-setup.sh` now creates `fc` with
  9944–9949 on `0.0.0.0` (`LIMA_LAN_PORTS`); `fc` was deleted and recreated
  with it (operator OK'd). A node inside `akuma-guest` still needs a relay
  *listening in `fc`*, because Lima only forwards sockets listening there:
  akuma-miot installs `kot-relay-mac-fc.service` (socat `fc:9945` →
  `10.0.2.15:9944`).
- **`guest-setup.sh`'s socat check matched itself.** `pgrep -f
  'socat.*4444'`, run inside `sh -c "…socat.*4444…"`, found its own shell,
  so it always reported the SSH forward as running and never started it.
  On a freshly recreated `fc` there was no socat at all, and `ssh -p 4444
  root@localhost` refused. The check is anchored now (`^socat TCP-LISTEN:`).
- **The Sep-17 `disk.img` herd**: no config reload (`herd enable kot`
  listed it but it only started after a reboot, like the metal box's old
  herd), and it did **not** respawn `kot` after it exited on 1a's ENOSYS,
  despite `restart = true`. Refresh the disk's `/bin/herd` before relying on
  either.
- **Firecracker from `run.sh` dies with its `limactl shell` session**, and
  `--timeout` caps a run. For a long-lived guest, relaunch it detached in
  `fc` with the config `run.sh` wrote:
  `setsid nohup firecracker --api-sock /tmp/fc.sock --config-file /tmp/akuma-fc.json …`.
  Staging a new kernel is then: copy `akuma-fc.bin` over the
  `kernel_image_path` in that config (`/tmp/akuma-fc.bin` in `fc`), then
  restart firecracker.

## Reproduce

```bash
# on the Mac, in ../akuma
overlays/devbox-firecracker/host-setup.sh
overlays/devbox-firecracker/guest-setup.sh
FC_FEATURES=devbox-smoltcp,no-tests overlays/devbox-firecracker/build.sh
overlays/devbox-firecracker/run.sh --vcpus 2 --mem 2048 --timeout 604800   # then relaunch detached, see §5

# storeprobe into the guest over HTTP (no scp): serve ../akuma-miot/dist/aarch64
# on the Mac's 127.0.0.1:8765, then in the guest (Lima's 192.168.5.2 is the Mac's loopback):
wget http://192.168.5.2:8765/storeprobe && chmod +x storeprobe
./storeprobe /tmp/sp.db; echo $?      # 7 = all stages
```

## Where to read

- akuma-miot `HANDOFF.md` (traps), `docs/TOPOLOGY_TARGET.md` (which agent
  runs where), `overlays/deploy/deploy.sh` (how things get onto these boxes,
  including the `fcguest` shape).
- This repo: `docs/reference/subsystems/amd64-shared-write-mmap.md` (the
  amd64 design 1b now mirrors on aarch64),
  `docs/reference/subsystems/networking.md` (socket lifetime, listener
  pool: finding 3).
