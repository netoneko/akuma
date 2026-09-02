# `pthread_create` fails at ~251 in a tight loop — ASID exhaustion, not thread slots

**Date:** 2026-08-30
**Status:** **OPEN** — root-caused and measured, not fixed. Pre-existing; reproduces
identically on `1841b32e`, so it is unrelated to the kill-interrupt fix
([`KTG_GRACE_EXPIRY_KILL_INTERRUPT.md`](KTG_GRACE_EXPIRY_KILL_INTERRUPT.md)) that was
being verified when it surfaced.
**Reproduce:** `FUTEXTEST_PHASE=6 /tmp/futextest` (or phase 7) on any devbox.

---

## 1. Symptom

`scripts/futex_suite.py` reports `futextest` failing at phase 6:

```
[6] wake-before-wait race x 500: start
pthread_create: No error information
```

Phases 6 and 7 both do **500 serial `pthread_create` + `pthread_join` cycles**.
Phase 2 does 200 and passes. Each phase runs one thread at a time and joins it
before the next, so this is not a concurrency limit.

`pthread_create` returns `EAGAIN` (rc=11). musl's `perror` prints "No error
information" because it has no string for that path.

## 2. Two wrong attributions, and why each looked right

**Wrong answer 1: thread-slot exhaustion.** The boot log is full of

```
[threads] slots exhausted (live=7 terminated=241 ceiling=248) — reclaimed 131 and retrying
```

and a minimal probe fails at **iteration 251** against a **ceiling of 248**. Live
threads at the moment of failure: seven. So the story writes itself — 248 slots,
`THREAD_CLEANUP_COOLDOWN_US = 10 ms`, a loop that outruns reclamation.

It is wrong, and one line in the log says so: **there are zero
`[threads] SPAWN FAILED` lines.** That is the only place slot exhaustion becomes an
error; every `slots exhausted` line is followed by a successful reclaim-and-retry.
The slot pressure is real and it is *survived*. 251 ≈ 248 is a coincidence, and a
vicious one — the two pools are nearly the same size.

**Wrong answer 2: the ceiling scales with RAM, so give it more.** It does not.
`compute_thread_limit` (`src/main.rs:441`) takes ¼ of user pages ÷
`USER_THREAD_STACK_SIZE` and then `.clamp(reserved + 6, config::MAX_THREADS)`. At
`MEMORY=4096` the division already exceeds `MAX_THREADS = 256`, so the value is
clamped: memory scales this **down** on small boxes and never up. Ceiling 248 =
256 − `RESERVED_THREADS` (8).

## 3. The actual cause

`sys_clone`'s `CLONE_THREAD` arm prints its reason, ungated
(`src/syscall/proc.rs:505`), and the log has it seven times:

```
[syscall] clone_thread failed: Failed to create shared address space
```

which comes from `clone_thread` → `mmu::UserAddressSpace::new_shared`, whose first
act is to allocate an ASID:

```rust
let asid = match with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc()) {
    Some(a) => a,
    None => { asid_exhausted_warn(); return None; }
};
```

and the log confirms it directly:

```
[asid] EXHAUSTED: no free ASID (MAX_ASID=256) — address-space creation failing;
       suspect leaked ASIDs from address spaces whose Drop never ran
```

**`MAX_ASID = 256`.** Every `clone_thread` takes one. It is returned in
`UserAddressSpace::drop` (`crates/akuma-exec/src/mmu/mod.rs:1647`), and that drop's
own comment states when it runs:

> *"this drop runs on the `PROCESS_RECLAIM_COOLDOWN_US` (10 ms) reclaim path"*

So ASID return is gated by the same 10 ms deferred reclaim as everything else, and
251 is `MAX_ASID` minus the handful already in use — not the thread ceiling.

## 4. It is a rate problem, not a leak

The warning names its own suspect — *"leaked ASIDs from address spaces whose Drop
never ran"* — and for this case that is **wrong**. One experiment settles it:

| serial `pthread_create` + `join` | result |
|---|---|
| no pause | FAILED at iteration **251** |
| no pause (repeat) | FAILED at iteration **252** |
| `usleep(1000)` between iterations | **OK — 2000 cycles** |

A leak does not heal when you slow down. ASIDs come back; they come back on a
10 ms deferred path, and a tight loop outruns it.

The arithmetic closes: a bare create+join costs roughly 40 µs, so one 10 ms
cooldown window admits ~250 iterations — against a pool of 256. At 1 ms per
iteration the same window needs ~10, and the loop runs indefinitely. The
sustainability condition is

```
MAX_ASID  >  PROCESS_RECLAIM_COOLDOWN_US / per-iteration-time
```

which today is `256 > 250` — passing by six.

**The warning text should be corrected**: exhaustion means "leaked **or** allocated
faster than the deferred path returns them", and the second is reachable from
ordinary userspace with no bug anywhere.

## 5. Is Linux like this?

No. A real program doing 500 sequential `pthread_create`s is ordinary and would not
see `EAGAIN`. So this is a genuine divergence, not an over-demanding probe — the
probe is the messenger. Anything that spawns threads in a tight loop (a test
harness, a thread-per-request server warming up, a build tool) can hit it.

## 6. Where a fix could live — and where it cannot

This was reached from the question *"is faster reclamation a decision for
`akuma-scheduler`?"*. It is not, and now for two reasons rather than one:

1. **The resource is not scheduler state.** It is an MMU/address-space lifecycle
   resource. `akuma-scheduler` is a discrete-event simulator for placement and
   netpoll wake policy; nothing it models touches the ASID pool.
2. **Reclaim timing is a correctness predicate, not a rankable policy.** The
   cooldown is a *proxy* for "no core is still using this". A model that scored
   "reclaim sooner" as faster would be right about throughput and wrong about
   memory.

The candidate levers, cheapest first:

| lever | cost | note |
|---|---|---|
| **16-bit ASIDs** (`TCR_EL1.AS = 1`) | one config bit + allocator width + a feature check | See §6.1 — the software is already 16-bit-clean; only the pool size and one register bit are 8-bit. |
| **Return the ASID before the rest of the drop** | needs an audit | The ASID is freed *after* `flush_tlb_asid`, which is correct and must stay. The question is whether the ASID must wait for the whole 10 ms reclaim, or only for the flush. |
| **Pressure-driven reclaim** (`akuma-kacho` shape) | policy + a correctness precondition | On exhaustion, run reclaim harder rather than returning `EAGAIN`. This is observe/decide/hysteresis, which is exactly what `kacho` is for — but it is only safe once the precondition below is established. |

### 6.1 The 16-bit ASID lever, concretely

An ASID is the tag AArch64 puts on TLB entries so translations for different
address spaces coexist without a full TLB flush per context switch. It lives in
`TTBR0_EL1[63:48]`, and **`TCR_EL1.AS` (bit 36) chooses the width**: 0 = 8 bits
(256 ASIDs), 1 = 16 bits (65536).

Today `AS = 0`. `TCR_EL1` is written as `0x0000_0005_B510_3510` in two places that
must agree — `src/boot.rs:326-330` (BSP) and `src/smp_shared.rs:1133-1137`
(secondaries) — and bits [39:36] are zero. Setting bit 36 is `movk x0, #0x5, lsl #32`
becoming `#0x15`.

**The good news is that the software is already 16-bit-clean.** Nothing assumes a
byte:

- `AsidAllocator` (`crates/akuma-exec/src/mmu/asid.rs`) is already `u16`-typed, and
  is a pure bit-manipulation module with host tests and no arch dependency.
- The TTBR0 composition is already `(asid as u64) << 48` (`mmu/mod.rs:470, 478, 1004`)
  — a 16-bit field placed at 48.
- `flush_tlb_asid(asid: u16)` already builds its `tlbi aside1is` operand the same way.

So the change is `MAX_ASID: u16 = 256` → `65536` (needs `u32` or a saturating
comparison, since 65536 does not fit `u16`), `used: [u64; 4]` → `[u64; 1024]`
(32 bytes → 8 KB of BSS), and the TCR bit in both boot paths.

**It must be gated on `ID_AA64MMFR0_EL1.ASIDBits`** (bits [7:4]: `0b0000` = 8-bit
only, `0b0010` = 16-bit supported). This is the part that makes it a real change
rather than a constant bump: if the CPU implements only 8-bit ASIDs, `TCR_EL1.AS` is
ignored, and an allocator handing out values above 255 would produce **two live
address spaces sharing one hardware TLB tag** — silent cross-process translation
reuse, which is about the worst failure this tree could have. Read the field at boot,
size the pool from it, and keep 256 when the CPU says 8.

Note what this does and does not do: it does not make reclamation faster, so it does
not need §6.1's enumeration. It widens the pool so the existing 10 ms deferred path
has time to work — the tight-loop runway goes from ~10 ms to ~2.6 s, which no real
program reaches. That is a fix rather than a paper-over precisely *because* §4 showed
the ASIDs do come back.

### 6.2 The precondition for anything that reclaims sooner — unresolved

`cleanup_terminated_internal` already applies **two** gates for the same hazard, one
line apart (`crates/akuma-exec/src/threading/mod.rs:1840`):

```rust
if ON_CPU[i].load(Ordering::SeqCst) != 0 { continue; }   // the exact fact
if now - TERMINATION_TIME[i] < cooldown  { continue; }   // a 10 ms proxy for it
```

`ON_CPU` is precise and observable; the cooldown is conservative and blind, and it
**predates** the `ON_CPU` gate (added as the root-cause fix for the cross-core
stack-sharing races). So the open question is whether the older proxy is still
load-bearing. Two things might hide under it that `ON_CPU` does not cover:

- The cooldown's doc claims **exception handlers**, not just context switches.
  `ON_CPU` tracks the scheduler's run/parked bookkeeping.
- Its sibling `PROCESS_RECLAIM_COOLDOWN_US` is documented as needing to *"outlast
  any BKL-dropped window that could still hold a raw pointer"*, and says the windows
  are "the same kind". If anything holds a slot index or an `&Process` across a
  dropped lock, the timer is what saves it and `ON_CPU` would not.

Answering that means **enumerating everything that holds a slot or address-space
reference across a dropped window** — the same discipline as
[`GRANT_RECORDS_VS_DENY_RECORDS.md`](GRANT_RECORDS_VS_DENY_RECORDS.md) ("before
reading a record to refuse something, enumerate every writer"). Until that exists,
do not shorten either cooldown.

**Note the ordering this implies:** the 16-bit-ASID lever needs none of that
analysis, because it does not make anything reclaim sooner — it just makes the pool
big enough that the deferred path has time to work. That is the change to make
first.

## 7. What to check when this recurs

1. `grep -a "clone_thread failed"` — the reason string is ungated and names the
   subsystem. **Read it before theorising**; two plausible theories died to it here.
2. `grep -a "\[asid\] EXHAUSTED"` — rate-limited to the first 5 then every 100, so
   its absence is meaningful and its presence understates the count.
3. `grep -ac "SPAWN FAILED"` — distinguishes real thread-slot exhaustion (non-zero)
   from slot *pressure* that was survived (zero, plus `slots exhausted … retrying`).
4. Re-run the loop with a 1 ms pause. Healing under a pause means a rate problem;
   not healing means a genuine leak.

## Background

- [`KTG_GRACE_EXPIRY_KILL_INTERRUPT.md`](KTG_GRACE_EXPIRY_KILL_INTERRUPT.md) — the
  fix being verified when this surfaced; also the other case where a 10 ms constant
  turned out to be the whole story.
- [`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md) §13 — gating the console flood is what
  made `[asid] EXHAUSTED` and `clone_thread failed` visible at all.
- `docs/reference/subsystems/memory.md` — the reclaim path the ASID drop rides on.
- `docs/reference/subsystems/thread-lifecycle.md` — slot states and the cooldown.
