# `N × INTERP_BASE + 0x6c964`: the trampoline ran the wrong process

**Status**: Root-caused and fixed 2026-08-06. The fix is
`resolve_thread_process` (`crates/akuma-exec/src/process/mod.rs`) — resolve a
new thread's `Process` from `THREAD_PID_MAP`, not from a table scan — plus a
refusal gate in `Process::run`. Regress with `test_trampoline_resolves_via_thread_pid_map`
(boot suite).

**Runbook**: [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md)
§2h is the short, current-state version. This is the long form: the evidence,
the readings that were wrong, and how each was disproved.

This class had been open for months. It was the last blocker on a green `-j4`
self-host build, and it turned out to be the same bug as the separately-tracked
`AS MISMATCH` class.

---

## 1. The symptom

A process dies instantly — `SIGSEGV after 0.00s` — taking an **instruction
abort** at a PC whose low half is constant and whose high half grows by exactly
`INTERP_BASE` (`0x3000_0000`) per occurrence:

```
ELR=0x6006c964 → 0x9006c964 → 0xc006c964 → 0xf006c964 → 0x12006c964 → …
```

`N` starts at 2, increments by exactly 1 per fault, and resets on reboot. The
register state is byte-identical across victims:

```
[Fault]  x0=0x30000000 x1=0x203fffebd0 x2=0x30000000 x3=0x300c03d8
[Fault]  x19=0x0 x20=0x0 x29=0x0 x30=0x0
[Fault]  SP_EL0=0x203fffebd0 ELR=0x6006c964 SPSR=0x80000400
[Fault] Process 139 (rustc) SIGSEGV after 0.01s
```

Downstream, this is what wedges `-j4`: each fault kills a `rustc` or `cargo`
child and takes its jobserver token with it, leaving the rest in `FUTEX_WAIT`
on a pipe nobody will write. The "live-but-idle rustc" wedge is a consequence,
not a scheduler bug.

## 2. What was already established, and was right

- **The accumulating word is one slot.** ld-musl vaddr `0xc03b8` (runtime VA
  `0x300c03b8`), link-time value `0x6c964` — the fault offset itself. `x3 =
  0x300c03d8` points three slots further along the same table.
- **The writer is musl's RELR apply loop.** `_dlstart` at `0x69f14`–`0x69f20`:
  `ldr x3,[x2,x5]; add x3,x3,x2; str x3,[x2,x5]` — `*slot += base`, the
  implicit-addend form. Alpine links with `DT_RELR`, which is implicit-addend by
  design. The kernel has no RELR handling, which is correct: musl applies its
  own, once.
- **So `N = k` means `_dlstart` ran `k` times against that one physical word.**
- **`N` advances per *fault*, not per exec.** Hundreds of successful execs sit
  between consecutive faults.
- **The branch is indirect.** No direct branch to `0x6c964` exists in ld-musl,
  and a PC-relative `b` could not reach `0x30000000` away. The over-relocated
  word is a branch target read from memory.

A complete and correct account of the *mechanism*. What remained was: **who runs
`_dlstart` a second time, and against whose page?**

## 3. The bug

`entry_point_trampoline` resolved a new thread's `Process` like this:

```rust
table::find_process(|p| if p.thread_id == Some(tid) { Some(p.pid) } else { None })
```

Two things make that unsound:

1. **`thread_id` is a recorded slot number that teardown paths deliberately
   leave set.** `kill_thread_group` PHASE 2 documents exactly why: clearing it
   removes the backstop `unregister_process` depends on. So an ACTIVE process
   can keep naming a slot it no longer owns.
2. **`find_process` returns the *first ACTIVE slot* that matches.** A stale
   process at a lower table index wins outright — and keeps winning, for every
   future occupant of that slot.

The thread then runs *that* process. `Process::run` activates **its** address
space and erets to **its** `Process.context`, which `replace_image` left as
`UserContext::new(entry_point, sp)` — the image's entry point, every GPR zeroed.
When the stale process is dynamically linked, that entry point is **ld-musl's
`_dlstart`**, so the thread re-runs the RELR apply loop over an interpreter data
page that address space already relocated.

Because the stale process is long-lived, its ld-musl data page is one fixed
frame. That is why `N` is a single global counter that only ever climbs.

The runbook had flagged this exact line in §3b as "hardening worth doing
regardless of the outcome — the scan is *currently* safe; it is safe by an
ordering two subsystems away, which is not where you want a hot-path invariant
to live." It was not safe.

## 4. The evidence

### 4a. The tripwire

The fix ships with a `[TRAMP-MISMATCH]` line whenever `THREAD_PID_MAP` and the
scan disagree. On a fixed kernel under `-j4`:

```
[TRAMP-MISMATCH] tid=21 THREAD_PID_MAP=89  but table scan found 84 — using 89
[TRAMP-MISMATCH] tid=21 THREAD_PID_MAP=101 but table scan found 84 — using 101
[TRAMP-MISMATCH] tid=21 THREAD_PID_MAP=104 but table scan found 84 — using 104
[TRAMP-MISMATCH] tid=21 THREAD_PID_MAP=119 but table scan found 84 — using 119
[TRAMP-MISMATCH] tid=26 THREAD_PID_MAP=129 but table scan found 107 — using 129
[TRAMP-MISMATCH] tid=24 THREAD_PID_MAP=143 but table scan found 133 — using 143
```

One stale process (84) captures slot 21 and wins it back for every new occupant.
Pre-fix, each of those lines is a thread that ran pid 84's image in pid 84's
address space.

### 4b. The `ttbr0_live` / `ttbr0_proc` pair

Three sessions never captured this on a live fault. From a pre-fix kernel
carrying the current `[RELR]` forensics block:

```
[RELR] fault_pid=136 cur_pid=139 ppid=138 tid=26 N=2 off=0x6c964
[RELR] ttbr0_live=0xaf000092d57000 ttbr0_proc=0xb9000073b5d000
       *** AS MISMATCH (foreign page tables) *** expected_l0=0x92d57000 switch_ins=1 gen=15
[RELR] slot va=0x300c03b8 pa=0x942be3b8 val=0x6006c964
```

Every field names the mechanism:

| field | value | what it says |
|---|---|---|
| `ttbr0_live != ttbr0_proc` | — | the thread is in another process's tables |
| `expected_l0 == ttbr0_live` | `0x92d57000` | `activate()` **chose** that L0 — not a hardware switch glitch |
| `switch_ins=1` | — | switched in exactly once: it ran from birth and faulted, i.e. the trampoline path |
| `tid` constant, `gen` climbing | `tid=26`; `gen=15,17,19` | one recycled **slot**; the faults group by tid, not by parent |
| `slot pa` constant | `0x942be3b8` | one long-lived frame carries the counter |
| `SP_EL0` constant, argv-independent | `0x203fffebd0` | the stale process's initial exec SP, not the new image's |

### 4c. A/B

Same disk clone, same workload (an in-guest supervisor restarting
`cargo build -j4` on the kernel), same kernel except the fix:

| | pre-fix | fixed |
|---|---|---|
| `[RELR]` faults | 27 | **0** |
| `AS MISMATCH` | 71 | **0** |
| `SIGSEGV` | 105 | **0** |
| `[TRAMP-MISMATCH]` | n/a | 15, caught and corrected |
| in-guest `-j4` build | crash-looping | progressing |

The 15 mismatches are the point: the race still happens at the same rate; where
the old code ran the wrong process, the new code does not.

## 5. `AS MISMATCH` was the same bug

`ttbr0_live != ttbr0_proc` is precisely what "`run()` activated the wrong
process's address space" looks like. The two were tracked as separate open
classes (§2f and this one) with separate theories, and both go to zero with one
fix. The `AS MISMATCH` class's own candidate fix (the `THREAD_STATES`
check-then-store races, §2g) was landed "pending `-j4` proof" and was not the
cure — this run is that proof.

## 6. Three readings that were wrong

### 6a. "The victim is a freshly-exec'd binary"

The class was named "a freshly-exec'd binary dies instantly", and the register
fingerprint supports it: `UserContext::new` zeroes every GPR, so all-zero
callee-saved registers at an image entry point is what a fresh `execve` looks
like.

It is also what `run()`-on-the-wrong-`Process` looks like, because it erets to a
context built by `replace_image` — just *someone else's*. In the reference log
the victims never appear in an `execve` line at all
(`[syscall] execve(path=…) PID n` is unconditional; pids 56, 58, 59, 60 each log
one, and 57 is the pid that faults).

### 6b. "The victim owns the address space it faults in"

Mid-session I read the *old* `[RELR]` instrumentation's `as_owner` field —
`as_owner == pid` for 3034 of 3035 faults — and concluded the victims owned
their address spaces, which retired the aliasing theory and sent me after a
`fork` bug instead.

`address_space_owner_pid_for_fault` falls back to the current pid when the L0
lookup misses (`crates/akuma-exec/src/process/children.rs:1008`). It was falling
back. **Do not read a field with a silent fallback as evidence of the thing it
falls back from.** The `ttbr0_live` / `ttbr0_proc` pair has no fallback; it
settled the question in one line the first time it ran on a live fault.

### 6c. "`/bin/busybox.static` is a static PIE, so it cannot have ld-musl mapped"

Recorded as the sharpest open lead in the previous session's handoff. Two things
are wrong with it:

1. **The `readobj` was run against the wrong copy.** In the running system,
   `[mprotect] pid=43 owner=43 addr=0x300bf000 len=0x1000 prot=0x1` is ld-musl's
   RELRO being write-protected, and the same pid mmaps the whole `0x30100000+`
   loader arena. It is dynamically linked.
2. **The name on the fault line is not necessarily the victim's image.**
   `do_execve` keeps `proc.name` current (`src/syscall/proc.rs:842`) and
   children inherit `parent.name`, so a process that dies before its own exec is
   reported under whatever its parent ran.

The lead was a real contradiction — just not one about the kernel.

## 7. Also fixed, but not this bug's cause

`get_saved_user_context`'s non-trap-frame fallback returned the slot's
`user_entry` / `user_sp` / `user_tls` triple to `fork_process`,
`vfork_process` and `clone_thread`. That triple is written once, at execve, by
`update_thread_context`; it records where the image was *first entered*, which
for a dynamically linked parent is ld-musl's `_dlstart`. Handing it to a child
produces the *same visible fingerprint* as the bug above — which is why it was
mistaken for the cause for part of this session.

It is removed (`[NO-TRAPFRAME]`; the syscall fails with `ENOMEM` instead of
launching a process at its loader entry). But **it has not fired once in any
instrumented run** — `[NO-TRAPFRAME]` count 0 across every arm. Treat it as
hardening. If that line ever appears, it is a different defect and the missing
trap frame is itself the finding.

An earlier fix in the same function (`c89daca`: reset the triple on slot
recycle, require `user_entry != 0`) targeted a third variant, the recycled-slot
one. Three defects, one function, none of them this bug.

## 8. Still open

**Why does the slot accumulate rather than sit at `N = 2`?** Each victim runs
`_dlstart` once against the stale process's live page, so a global climbing `N`
is consistent — *provided* the write really lands on that shared page rather
than on a CoW copy. The interpreter range *is* CoW-shared and demoted read-only
at fork (`cow_share_and_demote_range`, `interp_base = 0x30000000`,
`interp_scan_size = 2 MB`, so `0x300c03b8` is covered), and a correct copy would
pin every victim at `N = 2`. With the trampoline fixed this no longer produces a
crash, but the arithmetic deserves confirming rather than assuming.

## 9. Method notes worth keeping

- **Instrument the discriminator, then actually run it.** The `ttbr0_live` /
  `ttbr0_proc` pair had been *written* in a previous session and never captured
  on a live fault. One pre-fix run with it printing ended a months-old argument.
- **A field with a silent fallback is not evidence.** §6b cost most of a
  session. Prefer fields that fail loudly to fields that degrade quietly.
- **Check for absent log lines.** "The victim never logged an `execve`" was in
  the reference log the whole time.
- **When a victim's registers look like a *freshly launched* process, ask which
  code path *constructs* that state**, not which one corrupts memory. All-zero
  GPRs at an image entry point is a signature of construction, not damage.
- **A fingerprint can have more than one producer.** Two different defects in
  this codebase emit the identical register dump. Distinguishing them needed a
  field neither could fake — the live TTBR0.
- **A/B on the tripwire, not just on the symptom.** "Zero faults after the fix"
  was inconclusive here, because one candidate fix's guard never fired at all.
  "The race fired 15 times and nothing crashed" is the measurement that decides.

## Related

- [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md)
  — the runbook; §2h summarises this, §2b/§2e/§2g are the other fixes from the
  same hunt, §3b is where the scan was flagged and wrongly cleared.
- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) — the neighbouring
  hazard (`unregister_process` terminating a recycled slot) and why
  `kill_thread_group` leaves `thread_id` set.
- [`../runbooks/debug-futex-lost-wakeup.md`](../runbooks/debug-futex-lost-wakeup.md)
  — the lost-wakeup class this one was repeatedly confused with.
- [`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) — the build target
  the reproducer runs on.
