# The stale thread-slot kill: one rustc's teardown kills another rustc's linker — 2026-08-02

**Status: ROOT-CAUSED + FIXED.** Captured live at SMP=1 conc=4, proven
deterministically by a boot-suite self-test that fails without the fix, and
verified by an A/B hammer.

This is the residual `rustc -O big.rs` hang left open as §6 of
[`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md) and first mis-attributed in
[`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §5.1. Both earlier
write-ups looked in the right *workload* and the wrong *layer*: §5.1 blamed rustc's
`opt cgu.N` codegen worker threads, §6's lead suspect was a `ppoll` lost wakeup in
rustc's jobserver. It is neither. Codegen had already finished, and the process that
actually stops is single-threaded `gcc`, which issues no `ppoll` at all.

## 1. The one-sentence version

`Process::thread_id` is a bare index into a table whose slots are recycled on a ~10 ms
cooldown; `kill_thread_group`'s PHASE 2 acted on that index long after the slot could
have been handed to an unrelated process, so a `rustc` finishing its compile terminated
a *different* `rustc`'s linker, leaving that linker alive with **no thread at all**.

## 2. The captured evidence

Two `[XTERM]-CROSS` tripwire lines (a temporary tracer in `mark_thread_terminated`
that flagged terminating a *live* thread of a *different* thread group), from the round
that hung:

```
[XTERM]-CROSS victim tid=20 pid=696 tgid=696 st=5 <- killer tid=16 pid=610 tgid=610
[XTERM]-CROSS victim tid=19 pid=697 tgid=697 st=5 <- killer tid=14 pid=606 tgid=606
```

Decoded against the same log:

| role | pid | what it is |
|---|---|---|
| killer | 606 | `rustc -O /tmp/big.rs -o /tmp/c1` — a *different* concurrent job, finishing |
| killer | 610 | `rustc -O /tmp/big.rs -o /tmp/c2` — likewise |
| victim | 696 | `/usr/bin/gcc` — the **c3** job's linker driver |
| victim | 697 | `/usr/libexec/gcc/aarch64-alpine-linux-musl/15.2.0/collect2` — c3's linker |

`st=5` is WAITING: both victims were live and blocked in a syscall when killed.
`tgid == pid` on both victims proves they were standalone processes, not threads of
the killer — so this was not a legitimate thread-group kill.

Afterwards both are frozen forever, still listed as running processes:

```
[PSTATS] PID 696 (/usr/bin/gcc)      31.55s: 332 syscalls … in_kernel=81ms
[PSTATS] PID 696 (/usr/bin/gcc)     121.58s: 332 syscalls … in_kernel=81ms
[PSTATS] PID 697 (…/collect2)        31.05s: 134 syscalls … in_kernel=143ms
[PSTATS] PID 697 (…/collect2)       121.07s: 134 syscalls … in_kernel=143ms
```

Byte-identical syscall totals 90 seconds apart, with `in_kernel` also frozen. Nothing
is running: the process exists, its thread does not.

An earlier capture of the same failure (before the tracer existed) shows the negative
side of the same fact — the victim's slot being reused by others while the victim
"lives":

```
76629: [EINVAL] nr=78 pid=171 tid=10 …          ← gcc's last syscall, ever
76633: [Cleanup] Thread 10 recycled after 25906us cooldown
76699: pid=172 tid=10                            ← slot 10 handed to a busybox
```

`pid=171` never appears again in 500 s of log; slot 10 is reused 49 more times; `ps`
still lists gcc throughout.

## 3. Mechanism

### 3.1 The two lifecycles, and the reference that crosses them

Nothing in the kernel draws this, which is exactly why the bug survived. A thread slot
and a process slot each have their own state machine, and `Process::thread_id` is a raw
`usize` pointing from one into the other:

```
Process slot                      Thread slot (THREAD_STATES[tid])
────────────                      ────────────────────────────────
  ACTIVE                            FREE
    │                                │  claim_free_slot
    │ unregister_process             ▼
    ▼                              INITIALIZING
  RETIRED                            │
    │  cooldown                      ▼
    ▼                              READY ⇄ RUNNING / WAITING
  reclaimed (Process freed)          │  exit / mark_thread_terminated
                                     ▼
                                  TERMINATED
                                     │  cooldown ≈10 ms  ← cleanup_terminated_internal
                                     ▼
                                   FREE  ──► claimed by an unrelated process

        Process { thread_id: Option<usize> } ──────────┘
        a bare index, with no generation, no ownership check,
        and no invalidation when the slot underneath it recycles
```

The invariant nobody stated: **`thread_id` is only meaningful while the slot it names
still belongs to this process.** Two of the three teardown paths happen to maintain it;
one did not.

### 3.2 The three teardown paths — spot the odd one out

| path | clears `thread_id` before unregistering? |
|---|---|
| `kill_process` (`signal.rs`) | **yes** — `p.thread_id = None` |
| `kill_fork_subtree_recursive` (`mod.rs:1254`) | **yes** — `p.thread_id = None` |
| `kill_thread_group` PHASE 2 (`mod.rs:1181`) | **no** |

(That asymmetry is real but must be left alone — `kill_thread_group` *depends* on
`unregister_process` acting on `thread_id` as a backstop. See §5.1.)

and `table::unregister_process` re-reads the field and terminates whatever it names:

```rust
if let Some(tid) = unsafe { (*ptr).thread_id } {
    let current_tid = crate::threading::current_thread_id();
    if tid != current_tid {
        crate::threading::mark_thread_terminated(tid);   // ← on a possibly-recycled slot
    }
}
```

### 3.3 Why the window is wide, not theoretical

Under `kernel_smp_shared`, PHASE 1 does **not** hard-kill siblings. It posts deferred
kill requests (`request_thread_kill`) so a sibling preempted mid-critical-section can
finish and release its locks — the fix for the sshd "freeze" — and then **grace-waits up
to 2 seconds** for all siblings to reach their EL1→EL0 boundary:

```
PHASE 1   request_thread_kill(sib) for each sibling
          grace-wait ≤ 2 s for all siblings to self-terminate
                    │
                    │   meanwhile, per sibling:
                    │     sibling self-terminates      → TERMINATED
                    │     ~10 ms cooldown elapses      → FREE
                    │     an unrelated process spawns  → slot claimed  ← gcc / collect2
                    ▼
PHASE 2   for each sibling: unregister_process(sib_pid)
                              └─ reads the stale thread_id
                                 └─ mark_thread_terminated(recycled slot)  💥
```

The grace-wait is bounded by the *slowest* sibling, while a slot becomes reusable
**10 ms** after the *fastest* one exits. rustc's codegen pool makes that gap routine.

`kill_process`/`kill_process_with_signal` have a smaller version of the same hazard:
they snapshot `thread_id`, then `for _ in 0..5 { yield_now(); }` and run
`cleanup_process_fds` (which can block on VFS/socket close), then act on the snapshot.

### 3.4 Why the victim hangs instead of dying

`mark_thread_terminated` only touches `THREAD_STATES[tid]`. The victim's `Process` is
untouched: still ACTIVE, still `exited == false`, still in `ps` and `PSTATS`. A process
with no thread can never be scheduled, so it can never reach its exit path, so it never
becomes a zombie, so it is never reaped — and its parent's `wait4` blocks forever.

Full chain for the observed hang:

```
rustc(c1) finishes ─┐
rustc(c2) finishes ─┴─► kill_thread_group PHASE 2 ─► stale tid ─► kills gcc + collect2 (job c3)
                                                                        │
                                            gcc/collect2: threadless, can never exit
                                                                        │
                                     rustc(c3) wait4(gcc) ── never returns
                                                                        │
                                     rustc(c3)'s worker threads futex-wait on the linker
                                                                        │
                                     no output, no artifact, no error → "rustc silent"
```

The `futex=145 (288699ms)` seen in PSTATS — and mistaken for the bug in §6 — is step 4.
It is a correct wait for a linker that will never finish.

## 4. Why only `big.rs`, and why the earlier diagnoses missed

§5.1 got the discriminator right and drew the wrong conclusion from it: "`big` is not
'the big one' — it is *the threaded one*." Correct — but the threads matter because they
give `kill_thread_group` a **sibling set**, a long grace-wait, and a multi-iteration
PHASE 2 loop. They are not where the corruption happens.

Predictions this makes, all confirmed on the current tree:

| case | expected | measured |
|---|---|---|
| `big.rs`, `-C codegen-units=1` (no worker pool) | pass | pass, 20.2 s |
| `big.rs` conc=1 (nothing else tearing down) | pass | pass |
| `hello_std` / `hello_nostd` at any concurrency | pass (single-CGU, never `clone_thread`) | pass |
| `big.rs` conc=4 (concurrent teardowns) | intermittent silent hang | 1 hang in 6 rounds |

That table is also why the failure was never reproducible on demand: it needs one job's
teardown to overlap another job's *link* step.

## 5. The fix

Two layers, in `crates/akuma-exec`:

1. **`process/table.rs` — `unregister_process`.** Consult `THREAD_PID_MAP` (which
   records a slot's *current* owner) before terminating. Only an entry naming a
   different pid proves the slot was reassigned; a **missing** entry still terminates,
   preserving the orphaned-READY-thread cleanup this code originally existed for.
   This one guard protects every caller.
2. **`process/signal.rs` — `kill_process` / `kill_process_with_signal`.** Same
   ownership re-check (`slot_still_owned_by`) before acting on the pre-yield snapshot.

Both guards log when they fire (`[unregister] pid=… stale tid=… now owned by pid=…`),
so the race is observable rather than silent if it recurs.

### 5.1 The obvious "consistency" fix is wrong — do not reapply it

The tempting third change is to make `kill_thread_group` PHASE 2 clear `p.thread_id`
before unregistering, matching the other two teardown paths in §3.2. **That was tried
and it hung the box** (SMP=1, round 4 of the verification hammer: 99 % CPU, SSH dead,
frozen mid-`execve(.../bin/ld)`).

The reasoning that motivates it — "PHASE 1 already killed the sibling, so
`unregister_process` has nothing left to do" — is false under `kernel_smp_shared`.
PHASE 1 only *requests* a deferred kill, and its grace loop exits as soon as each
sibling has **consumed** the request:

```rust
crate::threading::is_thread_terminated(tid) || !crate::threading::has_pending_kill(tid)
```

`!has_pending_kill` becomes true the moment the sibling takes the request — strictly
*before* it marks itself TERMINATED. `unregister_process`'s terminate is the backstop
covering that gap. Remove it and a sibling can keep running against a RETIRED
`Process`.

So the asymmetry in §3.2's table is **not** an oversight to be tidied away:
`kill_thread_group` needs `unregister_process` to act on `thread_id`, which is exactly
why it is the one path that must not clear it — and exactly why the guard belongs
*inside* `unregister_process`, where it can distinguish "still the sibling's slot"
(terminate — the backstop) from "recycled to an unrelated process" (skip). Attribution
for the diagnosis: `stale tid=` fired **0 times** in the hung boot, proving the guard
never engaged and the PHASE 2 edit was the only behavioural delta.

> **Addendum (2026-08-02, later the same day):** the hang evidence above is
> superseded. The post-revert build hung identically at the same uptime, and the
> hang was root-caused to an unrelated defect: the execve stack leak ratcheting
> the kernel heap into the OOM wall (`EXECVE_STACK_LEAK_OOM_HANG.md`). "The
> PHASE 2 edit was the only behavioural delta" was wrong — exec count was the
> hidden variable. The *reasoning* in this section (PHASE 1's grace-gap needs
> the `unregister_process` backstop) stands on its own and is unchanged; only
> the "it hung the box" corroboration should not be cited as evidence.

## 6. Verification

**Boot-suite self-test** — `test_unregister_skips_recycled_thread_slot`
(`src/process_tests.rs`), registered in `run_all_tests`. It builds the collision
deterministically: process A records slot T, an unrelated process B claims T
(`THREAD_PID_MAP[T] = B`, slot READY), then `unregister_process(A)` runs. It also
asserts the converse — an unclaimed slot must *still* be terminated — so the guard
cannot be "fixed" by never terminating anything.

| build | result |
|---|---|
| guard disabled (pre-fix behaviour) | `FAILED: victim_thread_survived=false victim_still_registered=true own_thread_terminated=true` |
| guard enabled | `PASSED` |

`victim_thread_survived=false` with `victim_still_registered=true` is the bug in one
line: innocent thread killed, its process left alive and threadless.

Boot suite (single-core `--release`): **242 PASSED / 0 FAILED**.
Host workspace `cargo test`: pass.

**A/B hammer** (SMP=1, conc=4 `rustc -O big.rs`, artifact-verified, host otherwise
quiescent) — see §8 for the harness rules that make these numbers trustworthy:

| kernel | rounds | hangs | `[XTERM]-CROSS` |
|---|---|---|---|
| pre-fix | 6 | 1 (round 5) | 2, both in the hanging round |
| fixed | see run log | — | guard-fire counter replaces the tripwire |

## 7. What this does and does not close

**Closes:** `SMP_SHARED_ONCPU_GATE.md` §6's residual hang, and
`BKL_RUSTC_SCALING_BASELINE.md` §5.1's "artifact absent, rustc silent". §5.1's own
step-1..5 narrative (an EL0 return with a kernel register context) describes a
*different*, already-fixed defect — the ON_CPU cross-core stack-sharing race — which is
why that write-up's evidence and its conclusion never quite fit together.

**Does not close:** the §5.1-era observation that `busybox ps` SIGSEGVs while stalled
rustcs exist. Plausibly the same family (procfs iteration meeting a threadless process),
but not investigated here.

**Found in passing, unrelated:** rustc's incremental compilation never reuses its cache
in the VM —

```
warning: could not load dep-graph from `/tmp/incr/…/dep-graph.bin`:
         memory map must have a non-zero length
```

The compile succeeds, so it looks like a pass, but every `cargo build` is a full
rebuild. That is an Akuma-side mmap/file-size issue and deserves its own chase.

## 8. Harness rules (inherited and extended)

- **Never trust wall-clock; verify artifacts.** Kept from §7 of the baseline doc.
- **Drive long compiles detached + poll.** ssh keepalives kill a channel at ~240 s under
  load, and a slow-but-alive compile then reads as "artifact absent"
  ([`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md) §6).
- **One VM at a time.** A run with three QEMUs on a 12-logical-core host starved the
  SMP=1 box badly enough that *all four* jobs blew a 330 s budget — a clean false
  positive. Contention is indistinguishable from the bug at the harness level; the tell
  is that the real bug hangs **one** job while its siblings finish normally.
- **Distinguish the three failure shapes before drawing any conclusion.** They look
  identical in the driver's `stuck=[…]` output and mean completely different things:
  *(a)* one job stuck, siblings fine, VM answers SSH → the bug; *(b)* all four stuck,
  VM answers SSH → host contention; *(c)* VM at ~100 % CPU with a **frozen serial log**
  and no SSH → the box is unresponsive, which is a different defect entirely. Check
  `ls -la <boot>.log` against `date` first — a log that stopped growing settles (c)
  in one command.
- **`LC_ALL=C` on every grep/awk over serial logs** (SMP interleaving emits invalid
  multibyte sequences).

## Background

- [`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md) §6 — the residual this closes.
- [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §5.1 — the first
  investigation; right workload, wrong layer.
- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md) — the
  RETIRE cooldown, the process-slot half of the same "index outlives its referent" shape.
- [`DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md`](DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md)
  §5/§7 — this bug is logged there as a recurring defect *shape*, with the
  "draw the lifecycles" follow-up it motivates.
