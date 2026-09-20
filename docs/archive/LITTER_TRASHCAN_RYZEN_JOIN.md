# Joining the trashcan and ryzen litters — 2026-09-20

**Outcome: the two litters did NOT join.** The trashcan half works end to end
and now *discovers* its peer; the ryzen half cannot serve its hub because a
thread it spawns never runs. Along the way one real bug was fixed and verified
on hardware, and five more were found and written up.

This is the session narrative. Each finding has its own document; this one
exists so the next person does not have to reconstruct the order things were
learned in, or repeat the four wrong turns.

## 1. Where it started

Goal: configure the swarm on the trashcan (bare-metal Akuma/amd64) and join it
to the swarm on ryzen (Akuma/amd64 in a Firecracker guest). Both sides already
had the cross-litter relay built (`userspace/meow/docs/LITTER_RELAY_TOPOLOGY.md`).

Neither side was actually running when the session began:

* The trashcan had three agents staged from an earlier session, all stopped, and
  a kernel that would lose its NIC minutes into any boot.
* The ryzen guest **had no `/bin/sh` at all** — `debugfs` showed `busybox` and
  `ash` linked, no `sh` — so every ssh exec died with
  `sshd: failed to spawn '/bin/sh' for exec` and nothing had ever run there.
  The Mac's third litter (the Docker `yard`) was also down.

## 2. What was fixed and verified

### 2.1 The RTL8169 receiver that could not recover — FIXED on hardware

The trashcan kept losing the network: `rx=` frozen, `tx=` climbing,
`dhcp=pending`, unreachable at both `.123` and the static `.220`. Ubuntu on the
same box, same cable, leased fine — which is what proved it was the driver.

Root cause: the stall watch had two arms and this fault was neither. `RDU`
cannot fire once the receiver has stopped (a chip taking nothing off the wire
has nothing to report), and the `blind` arm is retired by the **first frame** for
the rest of the boot. So a receiver that delivered a few frames and then stopped
disarmed both detectors on its way past, and `kicks=` stayed at `0` with the
resync/kick/re-init ladder intact and unreachable. **Receiving a little was
strictly worse than receiving nothing.**

Fixed with a third arm, `Silent`, as pure host-tested logic in
`akuma-net-rtl8169::stall` (11 tests, one per conflict). Measured on the metal:

```
[rtl] SILENT: no frame for 60000000 us with no RDU (rx=16 frames) - receiver stopped
[probe] rx=357 tx=101 dry=5 kicks=1 ip=192.168.1.123/24 dhcp=leased dns=1.1.1.1 clk=set
```

`rx=16` is exactly `RING_LEN`. One detection, one kick, full recovery, lease on
its own — the first time this fault has ever self-healed. The horizon was then
cut from 60 s to 10 s, since the box is off the network for every second of it.
[`AMD64_RTL8169_SILENT_STALL.md`](AMD64_RTL8169_SILENT_STALL.md)

### 2.2 meow's probe starving its own hub — FIXED

A peer probe used the same 5 s I/O budget as a real request and ran inline in
the thread that serves the hub. Two litters listing each other therefore starve
each other's serve loops, symmetrically and permanently. Probes now get 500 ms
(`PROBE_TIMEOUT_US`) plus doubling backoff on a silent peer
(`StaticPeer::probe_failures`). The trashcan's hub went from answering nobody to
answering in 0.01 s.

**Still open in the same area:** `TcpStream::connect` inside the probe is not
bounded by anything meow controls — an unanswered SYN sits for Akuma's 10 s
`CONNECT_TIMEOUT_US`, which is longer than the pulse interval.

### 2.3 "raft thread up" that meant nothing — FIXED

`start_raft_thread`'s return value was discarded and the leader printed
`raft thread up` unconditionally. Worse, the clone returning success is not
evidence the child runs. The child now flips `RAFT_ALIVE` as its first act and
the parent waits for it, so the log states what is true. This is what later
proved the thread *does* start — correcting an earlier wrong conclusion of mine.

### 2.4 Logging that cannot be trusted to describe a sick process — FIXED

meow's raft thread logged through the shared stdout herd captures, using
`format!`. It now writes to its own file through a single held fd, formatting
into a fixed stack buffer: `libakuma::FixedBuf<N>` with `safe_print!` /
`safe_write!`, **one** implementation in libakuma. The kernel's `safe_print!`
could not be reused — it writes to the *kernel* console — but the rule behind it
is the same one (`docs/reference/subsystems/console.md` § "Printing rules"): a
diagnostic that needs a healthy heap to report on the heap is the wrong shape.

## 3. What is open

| finding | document |
|---|---|
| A spawned thread starts and never runs its body (the join blocker) | [`AMD64_SPAWNED_THREAD_NEVER_RUNS.md`](AMD64_SPAWNED_THREAD_NEVER_RUNS.md) |
| Two Akuma boxes cannot reach each other across proxy-ARP | [`AMD64_PROXY_ARP_UNREACHABLE.md`](AMD64_PROXY_ARP_UNREACHABLE.md) |
| `SPAWN_EXT` with `box_id=0` and a `cwd` triple-faults the kernel | [`AMD64_SPAWN_EXT_WORKDIR_CRASH.md`](AMD64_SPAWN_EXT_WORKDIR_CRASH.md) |
| herd starts a service repeatedly while it is still running | [`HERD_DUPLICATE_SERVICE_PROCESSES.md`](HERD_DUPLICATE_SERVICE_PROCESSES.md) |
| Nothing reaps an orphan, so `kill -9` looks like an unkillable process | [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) appendix |

The first one is why the join failed. The ryzen guest's hub accepts a TCP
connection in 0.01 s and answers nobody, because the thread that serves it
executes one atomic store and never reaches its first loop iteration
(`alive=true`, `RAFT_TICKS=0`, forever). The agent loop's own `serve::drain`
answers *sometimes*, which is why the trashcan sees the peer flap
discovered/lost rather than never appearing.

## 4. How far the join actually got

| step | state |
|---|---|
| trashcan litter running under herd, `sherlock` leader | ✅ |
| trashcan hub reachable on the LAN | ✅ 0.01 s |
| ryzen guest repaired, `panther` leader under herd, ollama reachable | ✅ |
| a path between the two boxes | ✅ **only via the bridge on the Ryzen host** (§ proxy-ARP doc) |
| trashcan **discovers** ryzen | ✅ `[event] static peer ryzen discovered at 192.168.1.126:7701`, flapping |
| a message crosses | ❌ `Inbox for 'panther' is empty` after two sends |

## 5. Four wrong turns, kept so they are not repeated

1. **"The box is running an old kernel."** The file was dated `03:29` read from
   inside Akuma and `06:29` from Ubuntu — **Akuma's clock is UTC**. It was not
   from before the day's network work; it was from the middle of it.
2. **"`Threads: 1`, so the raft thread never spawned."** That field reads `1`
   even for a process whose second thread is provably running. It cannot answer
   that question on this kernel.
3. **"Ryzen's networking is at fault."** The Mac reaches the guest through the
   same proxy-ARP path in 60 ms. Ryzen was exonerated by a control test that
   should have been run first — and no firewall rule was ever touched.
4. **"Only the first write per iteration lands."** Four rounds and three
   theories (fd exhaustion, stdout contention, dropped writes) died here. The
   three log statements were **in two different functions**: the anchor
   `loop {` + `tick += 1;` matches the agent loop in `run()` first. The healthy
   1 Hz ticks being watched were the main thread's. Prevention is in
   `AMD64_SPAWNED_THREAD_NEVER_RUNS.md` §5.

The pattern in all four: a conclusion drawn from one reading, with the control
test available and not run. The controls that eventually settled things were
cheap — Ubuntu on the same NIC, the Mac to the same address, a counter the other
thread reports.

## 6. State left behind

* **Trashcan** — kernel `bb78b322…` (10 s `Silent` horizon), `/bin/meow-live`
  beside the untouched `/bin/meow`, litter under herd (`llama` + `sherlock`),
  peer `ryzen@192.168.1.126:7701`. Previous kernels kept as
  `akuma-amd64.bak-*`; `.good` untouched.
* **Ryzen guest** — repaired image (`/bin/sh`, `meow`, personas, `/agents`),
  same kernel, 1 vCPU, herd supervising `panther` on `qwen2.5:7b`, peer
  `trashcan@192.168.1.126:7702`, hub back on `127.0.0.1:7700`. **Running a
  diagnostic build** carrying the instrumentation from §3.
* **Ryzen host** — `/tmp/ryzen_bridges.py` under `setsid`, three forwarders,
  log at `/tmp/bridges.log`. Nothing else on that host was modified.
* **`herd`** — gained a `workdir` key that **cannot be used** until the
  `SPAWN_EXT` crash is fixed; `llama` uses a script launcher instead.

## 7. Next

1. Narrow the spawned thread with a staged counter (that document's §6), then
   vary the guest's vCPU count — bare metal with four cores does not reproduce
   it.
2. Bound the connect inside the peer probe (§2.2).
3. Decide whether the static-peer list is the right shape at all. It is a
   stopgap: two litters, hand-maintained addresses, and every address in this
   session was wrong at least once. Peer discovery — gossip, or a small DHT —
   is the real answer, and a `no_std` crate for it is worth a look before more
   is built on the list.
