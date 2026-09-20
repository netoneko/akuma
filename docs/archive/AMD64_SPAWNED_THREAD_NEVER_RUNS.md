# meow's spawned thread starts and never runs — Firecracker guest, amd64

**Status: OPEN, narrowed to a few instructions.** Found 2026-09-20 on the Ryzen
Firecracker guest (Akuma/amd64, 1 vCPU) while chasing a litter hub that accepted
TCP connections and answered none of them. **This document was first written
around a different theory — "only the first write per iteration lands" — and
that theory was an artefact of my own instrumentation being in the wrong
function. §5 keeps the mistake, because it cost four rounds and would cost them
again.**

## 1. What is actually happening

`meow litter live` spawns a raft thread (`rt::spawn_detached`) that serves the
hub; the main thread runs the agent loop. On this guest the child runs its
**first instruction and nothing else**.

The measurement that shows it — the child stores a tick counter at the top of
every loop iteration, and the **main** thread reports it:

```
[13] agent tick 10 sees RAFT_TICKS=0 alive=true
[23] agent tick 20 sees RAFT_TICKS=0 alive=true
[33] agent tick 30 sees RAFT_TICKS=0 alive=true
```

* `alive=true` — set by the child itself, as the first statement of
  `raft_entry`. The child exists and executed at least that store.
* `RAFT_TICKS=0`, forever — the child never reaches the top of its first tick.

Between those two points there is almost nothing: one `raft_logf!` (a `write` to
an already-open fd), an atomic load of `RAFT_CTX`, a null check, and the call
into `raft_tick_loop` (which logs on entry, then stores `RAFT_TICKS = 1`). **No
raft log line has ever appeared**, so the child does not survive its first file
write, or does not survive the call.

## 2. Why it matters

The hub is served from that thread. With it dead:

* `192.168.1.50:7700` **accepts** a connection in 0.01 s (the kernel's listener
  backlog) and then nobody answers — from the LAN, from the host, and from the
  agent's own operator on the same box.
* The agent reports itself healthy: it wins the bind, logs `raft thread up`
  (which is true — the liveness flag *was* flipped), and idles.
* Two litters therefore cannot join, because the probe that would discover this
  peer never gets a reply. That is the whole reason the `trashcan` ↔ `ryzen`
  join did not happen.

## 3. The same binary works on bare metal

The trashcan (Akuma/amd64, 4 cores, no hypervisor) runs the identical `meow`
build with the identical two-thread structure, and its hub answers in 0.01 s
with its raft thread serving. So this is not "meow's threading is broken"; it is
specific to this guest — 1 vCPU, Firecracker, virtio — and the difference is
the first thing to vary.

## 4. What has been ruled out

* **`spawn_detached` returning a false success.** It reports true *and* the
  child demonstrably runs (`alive=true` comes from the child). The parent now
  waits for that flag rather than trusting the clone's return value.
* **`/proc/<pid>/status` `Threads:`** — it reads `1` even for a process whose
  second thread is provably running, so it cannot answer this question. An
  earlier conclusion drawn from it ("panther has no raft thread") was withdrawn.
* **Output being lost.** Four writes issued back-to-back from the main thread
  all land, in the same second, against the same fd.
* **fd exhaustion.** Holding one fd open for the process's life instead of
  `open`/`close` per line changed nothing.
* **Heap on the logging path.** All logging now formats into a fixed stack
  buffer (`libakuma::FixedBuf`, `safe_write!`) and allocates nothing.

## 5. The methodology error, kept on purpose

Four rounds were spent on a phantom: "only the first log line per iteration
appears, on every sink, which is impossible in program order". It was not
impossible — **the three log calls were in two different functions.** The
anchor used to insert them, `loop {` followed by `tick += 1;`, matches the
*agent* loop in `pub fn run()` first, and that is where two of them landed,
while the third went into `raft_tick_loop`. The healthy 1 Hz "ticks" being
watched were the main thread's.

Three checks that would have caught it immediately, and are cheap:

* After inserting instrumentation, **print which function it is in**:
  `awk 'NR<=N && /^fn |^pub fn /{f=NR": "$0} NR==N{print f}'`.
* Give each loop's markers a **distinct prefix** (`agent tick` vs `raft tick`),
  never a shared one.
* Prefer an anchor that exists **once** in the file; `tick += 1` is not one.

## 6. Next measurement

Narrow the few instructions: have the child, before touching a file or the
context pointer, store a second atomic (`RAFT_STAGE = 1, 2, 3 …`) after each
step, and let the main thread report it as above. That separates "dies at the
first syscall" from "dies at the `RAFT_CTX` load" from "dies entering the loop"
without the child needing to do any I/O at all. Then vary the guest's vCPU
count, since bare metal with four cores does not reproduce it.

## 7. Background

* `userspace/meow/src/rt.rs` — the raw `clone` trampoline and why these are not
  musl pthreads; `CLONE_SETTLS` is per-target and x86_64 requires it.
* [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) — amd64
  thread-lifecycle gaps generally.
* [`HERD_DUPLICATE_SERVICE_PROCESSES.md`](HERD_DUPLICATE_SERVICE_PROCESSES.md) —
  found the same session; shares the `Threads:` warning above.
* `userspace/meow/docs/LITTER_RAFT_LOOP.md` — what the thread is supposed to do.
