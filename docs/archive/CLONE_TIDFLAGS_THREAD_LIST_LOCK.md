# The kernel wrote a live TID into musl's thread-list lock (2026-08-06)

Investigation record for the crash class tracked in
[`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md)
and prompted by [`../../proposals/THREAD_SPAWN_SIGSEGV.md`](../../proposals/THREAD_SPAWN_SIGSEGV.md).
Archived verbatim; the runbook §2e carries the maintained summary.

## Symptom

A `pthread_create`d thread died within milliseconds of birth, at a fixed PC,
with a near-null `FAR` — usually `0x0` or `0x8` — and the kernel killed the
whole thread group:

```
[T24.34] [Fault] Data abort from EL0 at FAR=0x8, ELR=0x4071d0, ISS=0x47
[Fault]  x0=0x0 x1=0x0 x2=0x0 x3=0x420a90
[Fault]  x19=0x10a66f20 x20=0x2 x29=0x201ffffbb0 x30=0x4071ac
[Fault]  tid=83 ttbr0_live=0xc50000687d4000 ttbr0_proc=0xc50000687d4000
[Fault]  clone-handoff tid=83 stack=0x10a66ef0 from pid=8/tid=14
[Fault]    at clone: entry=0x407244 arg=0x10a66f00
[Fault]  [x19=0x10a66f20] = 0x10a66f20 0x0 0x0 0x0
[Fault] Process 190 (/root/spawnalias) SIGSEGV after 0.00s
[Fault] SIGSEGV in clone_thread, calling exit_group
```

It was the last crash class killing an in-VM `-j4` self-host build, and it had
survived three prior sessions of investigation.

## How it was actually found

By symbolizing the faulting PC — which the runbook's §1 had told everyone to do
first, and which a previous session's handoff note had explicitly advised
*skipping* in favour of reading instrumentation flags. That advice was wrong and
cost time.

`ELR=0x4071d0` in the static musl binary:

```
0000000000407074 <__pthread_exit>:
...
4071cc:	a9408660 	ldp	x0, x1, [x19, #8]    ; x0 = self->next, x1 = self->prev
4071d0:	f9000420 	str	x0, [x1, #8]         ; prev->next = next     <-- FAULT
4071d4:	f9400a61 	ldr	x1, [x19, #16]
4071d8:	f9000801 	str	x1, [x0, #16]
4071dc:	a900ce73 	stp	x19, x19, [x19, #8]
```

`ISS=0x47` decodes as WnR=1 (a write), DFSC=`0b000111` (translation fault,
level 3) — a write to address `0x8`, i.e. `[x1 + 8]` with `x1 == 0`.

The diagnostic's memory dump named the cause outright:
`[x19] = 0x10a66f20 0x0 0x0 0x0`. Offset 0 is `self->self` and it is **correct**.
Offsets 8 and 16 are `self->next` and `self->prev` and both are **NULL** — the
thread was never linked into musl's thread list.

That is the whole diagnosis in one line, because in musl the parent writes those
two fields *after* `__clone` returns while holding `__thread_list_lock`, and the
child cannot reach the unlink without taking that same lock. Everything written
before the clone was intact; everything written after it was missing. Not memory
corruption — a lock that failed to lock.

## Mechanism

Linux keeps three `clone(2)` tid flags strictly separate:

| flag | pointer | value | when |
|---|---|---|---|
| `CLONE_PARENT_SETTID` (`0x0010_0000`) | `ptid` | child tid | at clone, in the parent |
| `CLONE_CHILD_SETTID` (`0x0100_0000`) | `ctid` | child tid | when the child first runs, **in the child's context** (`schedule_tail`) |
| `CLONE_CHILD_CLEARTID` (`0x0020_0000`) | `ctid` | **zero** | at child **exit**, then a futex wake |

`CLEARTID` says nothing whatsoever about clone time. Akuma's `clone_thread` did
not receive the flag word at all and wrote unconditionally:

```rust
if child_tid_ptr != 0 {
    unsafe { core::ptr::write(child_tid_ptr as *mut u32, child_tid); }   // BUG
}
```

musl's `pthread_create` passes `CLEARTID` **without** `CHILD_SETTID`, and the
pointer it passes is `&__thread_list_lock` — a global mutex word, not a tid
slot. (musl uses `CLONE_CHILD_CLEARTID` on that address deliberately: a thread
holds the thread-list lock across its own exit, and the kernel's clear-and-wake
*is* the unlock. See `__pthread_exit`'s tail: `mov x8, #93; svc` executed with
the lock still held.)

So every thread spawn stamped the new thread's own tid into musl's thread-list
lock. That is far worse than writing garbage there, because of `__tl_lock`'s
recursion fast path:

```c
void __tl_lock(void) {
    int tid = __pthread_self()->tid;
    int val = __thread_list_lock;
    if (val == tid) { tl_lock_count++; return; }      /* "already mine" */
    while ((val = a_cas(&__thread_list_lock, 0, tid)))
        __wait(&__thread_list_lock, &tl_lock_waiters, val, 0);
}
```

compiled to:

```
406f54:	6b00029f 	cmp	w20, w0                  ; w20 = self->tid, w0 = the lock word
406f58:	54000161 	b.ne	406f84                   ; not mine -> real CAS acquire
406f5c..68:	                                     ; tl_lock_count++ and return, unlocked
```

The value the kernel wrote is *exactly* the child's tid. So the lock appeared to
be already held by the one thread that must not hold it. The child's very first
`__tl_lock()` — the one at the top of `__pthread_exit` — returned without
acquiring, and the child unlinked itself from the thread list while its parent
was still linking it. `self->prev` was still NULL. Write to `0x8`.

**Second-order damage.** The bogus `tl_lock_count++` is never undone, and
`__tl_unlock` checks the counter first:

```c
void __tl_unlock(void) {
    if (tl_lock_count) { tl_lock_count--; return; }
    a_store(&__thread_list_lock, 0);
    if (tl_lock_waiters) __wake(&__thread_list_lock, 1, 0);
}
```

so the parent's unlock only decremented the counter and **never released the
lock**. Every later `pthread_create` / `pthread_exit` in that process blocked
forever. That is why the fault and the "wedged with threads parked in futex"
symptom always travelled together, and why the parked `pthread_join` kept
looking like a lost wakeup when it was a survivor.

## Fix

`crates/akuma-exec/src/process/mod.rs` — `clone_thread` now takes the raw
`flags` word and gates all three writes on their actual flags:

```rust
if parent_tid_ptr != 0 && flags & CLONE_PARENT_SETTID != 0 { … }
if child_tid_ptr  != 0 && flags & CLONE_CHILD_SETTID  != 0 { … }
clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 { child_tid_ptr } else { 0 },
```

`src/syscall/proc.rs` passes `flags` through at the single call site. Go's
`newosproc` passes 0 for both pointers, so Go is unaffected either way.

## Verification

The stress reproducer written for this class (`spawnalias.c`) turned out to be
useless for A/B: it reproduced the crash in 9 seconds on one run and then
**passed cleanly in 22 seconds on the same broken kernel** on the next. This is
the same trap recorded for the 2026-08-04 futex fix — a stress repro that passes
on both arms proves nothing.

So a deterministic probe was written instead:
`userspace/forktest/c_stress/tidflags.c`. One clone and one load per check, no
stress loop, calibrated against real Linux first.

| | Linux (calibration) | Akuma before | Akuma after |
|---|---|---|---|
| `CLEARTID alone leaves ctid untouched at clone` | PASS | **FAIL** — `ctid=0xc`, child tid 12 | PASS |
| `CLEARTID zeroes ctid at child exit` | PASS | PASS | PASS |
| `CHILD_SETTID writes the child tid once child runs` | PASS | PASS | PASS |
| `CHILD_SETTID alone does NOT clear at exit` | PASS | **FAIL** | PASS |
| `PARENT_SETTID writes the child tid at clone` | PASS | PASS | PASS |
| `no tid flags: ctid untouched at clone` | PASS | **FAIL** | PASS |
| `no tid flags: ctid untouched at exit` | PASS | **FAIL** | PASS |
| `pthread churn survives` | PASS | PASS | PASS |
| **total** | **8 PASS** | **4 FAIL** | **8 PASS** |

The three extra FAILs are the same defect from other angles: `ctid` written with
no tid flags set at all, and `ctid` cleared at exit with no `CLEARTID` set.

One calibration subtlety, worth keeping because it nearly produced a false
verdict: an earlier version of the probe asserted the `CHILD_SETTID` write
synchronously after `clone` returned, and **failed on real Linux**. Linux
performs that write in the *child's* context, so the parent cannot observe it
immediately. The probe polls for it now. Every probe in `c_stress/` is
calibrated on Linux before it is trusted, for exactly this reason.

## What this did NOT fix

A full `-j4` in-VM build on the fixed kernel went from a steady stream of faults
to **one** — but that one was a different bug, and the fault dump's
address-space diagnostic caught it:

```
[Fault] Data abort from EL0 at FAR=0x7, ELR=0x3801c58c, ISS=0x7
[Fault]  tid=28 ttbr0_live=0xf7000088f1d000 ttbr0_proc=0x4000088b1d000  *** AS MISMATCH ***
[Fault]  clone-handoff tid=28 stack=0x145f4d590 from pid=1988/tid=20 ttbr0=0x2000088b1d000
[Fault]  [x19=0x3ceb84f0] = 0x7 0x3ceb8ef0 0x6f666e69657363 0x10a80000000000
[Fault]    ascii: "cseinfo."
```

Decoded as `ASID:base` — parent `2:0x88B1D000`, this thread's `Process`
`4:0x88B1D000`, live `TTBR0_EL1` **`0xF7:0x88F1D000`**. The parent and the
child's `Process` agree on the L0 base as they must; the live register points at
a third address space entirely. The core was running this thread in someone
else's page tables, which is why `x19` dereferenced to a plausible-looking but
foreign heap word.

That is the old theory T1 (cross-address-space aliasing), now with direct
evidence. The same run also logged **81 `[BKL] stuck` events** and eventually
wedged with live-but-idle `rustc` processes. Both are open; see the runbook §2f.

## Instrumentation notes

Two flags added earlier in this investigation needed correcting once real faults
arrived:

- **`*** HANDOFF CHANGED ***` was a false positive and has been removed.** musl's
  child starts with `ldp x1, x0, [sp], #16` — it pops both handoff words and then
  uses that same address as its stack, so its own first frame overwrites them
  within a few instructions. The very first fault caught by the instrumentation
  printed this flag, and it was pure noise; the real signal in that dump was the
  NULL `self->prev`. The `at clone:` values remain trustworthy.
- **`*** AS MISMATCH ***` now distinguishes ASID from L0 base.** A CLONE_THREAD
  child is handed the parent's ttbr0 verbatim (`shared_ttbr0`) while
  `new_shared` gives its `Process` a fresh ASID over the same L0 — so ASIDs
  differ *by design* on every cloned thread. Only a differing **L0 base** is a
  finding. The original flag would have made every cloned-thread fault look like
  a smoking gun.

## Lessons

1. **Symbolize the faulting PC before theorising.** Three sessions reasoned about
   memory aliasing, heap use-after-free, TLB staleness and relocation
   accumulation from register dumps. Five minutes of `objdump` named the exact
   musl function and instruction, and the cause followed immediately.
2. **A kernel doing *more* than the flags asked for is a corruption bug.**
   Writing an unrequested output pointer is indistinguishable, from userspace,
   from memory corruption — and userspace is entitled to keep something else in
   that word. The failure mode here was maximally confusing because the value
   written was *meaningful* (a real tid) rather than garbage.
3. **A flaky stress repro cannot A/B a fix.** `spawnalias` passed on a
   known-broken kernel. Build the deterministic probe as soon as you have a
   mechanism hypothesis; it is usually less work than one more stress run.
4. **Calibrate probes on real Linux first.** The probe's own first version was
   wrong about Linux semantics.

## Background

- [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md)
  §2e (this fix), §2f (what survives it), §3c (theory ranking, kept for history).
- [`../reference/subsystems/syscalls/proc.md`](../reference/subsystems/syscalls/proc.md)
  — "The three tid flags are not interchangeable".
- [`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) — the original
  "thread-spawn SIGABRT under real `-j4` parallelism" report.
- `userspace/forktest/c_stress/tidflags.c`, `spawnalias.c`, `clonearg.c`.
