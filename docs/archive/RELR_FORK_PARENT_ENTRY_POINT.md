# `N × INTERP_BASE + 0x6c964`: fork launched the child at its parent's ELF entry point

**Status**: Root-caused and fixed 2026-08-06. The fix is the removal of
`get_saved_user_context`'s non-trap-frame fallback
(`crates/akuma-exec/src/threading/mod.rs`). Regress with the host test
`threading::state_transition_guard_tests::a_thread_with_no_trap_frame_never_yields_a_child_context`.

**Runbook**: [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md)
§2h is the short, current-state version. This document is the long form: the
evidence, the three readings that were wrong, and why they were wrong.

This class had been open for months and was the last blocker on a green
`-j4` self-host build.

---

## 1. The symptom

A process dies instantly — `SIGSEGV after 0.00s` — taking an **instruction
abort** at a PC whose low half is constant and whose high half grows by exactly
`INTERP_BASE` (`0x3000_0000`) per occurrence:

```
ELR=0x6006c964 → 0x9006c964 → 0xc006c964 → 0xf006c964 → 0x12006c964 → …
```

`N` starts at 2, increments by exactly 1 per fault, and resets on reboot. In
the reference log (`relr_probe_run1.log`, 3035 faults over ~290 s) it reached
the hundreds. The register state is byte-identical across victims:

```
[Fault]  x0=0x30000000 x1=0x203fffe1f8 x2=0x30000000 x3=0x300c03d8
[Fault]  x19=0x0 x20=0x0 x29=0x0 x30=0x0
[Fault]  SP_EL0=0x203fffe1f0 ELR=0xc006c964 SPSR=0x80000400
[Fault] Process 57 (/bin/busybox.static) SIGSEGV after 0.00s
```

Downstream, this is what wedges `-j4`: each fault kills a `rustc` or `cargo`
child and takes its jobserver token with it, leaving the remaining workers in
`FUTEX_WAIT` on a pipe that will never be written. The "live-but-idle rustc"
wedge is a consequence of this class, not a scheduler bug.

## 2. What was established before this session, and was right

- **The accumulating word is exactly one slot.** ld-musl vaddr `0xc03b8`
  (runtime VA `0x300c03b8`), link-time value `0x6c964` — which is the fault
  offset itself. `x3 = 0x300c03d8` points three slots further along the same
  table.
- **The writer is musl's RELR apply loop.** `_dlstart` at `0x69f14`–`0x69f20`
  does `ldr x3,[x2,x5]; add x3,x3,x2; str x3,[x2,x5]` — `*slot += base`, the
  implicit-addend form. Alpine links with `DT_RELR` (`RELRSZ 48` → 80 slots),
  and RELR entries carry no explicit addend by design. The kernel has no RELR
  handling anywhere, which is correct: musl applies its own, once.
- **Therefore `base` is added once per *execution of `_dlstart`* against that
  physical word.** `N = k` means the loop ran `k` times over the same word.
- **`N` advances once per *fault*, not per exec.** Hundreds of successful execs
  sit between consecutive faults.
- **The branch is indirect.** No direct branch to `0x6c964` exists in ld-musl,
  and a PC-relative `b` could not reach `0x30000000` away anyway. The over-
  relocated word is a branch target read from memory.

That is a complete and correct characterisation of the *mechanism*. What
remained was: **who runs `_dlstart` a second time, and against whose page?**

## 3. Three readings that were wrong

### 3a. "The victim is a freshly-exec'd binary"

The class was named "a freshly-exec'd binary dies instantly", and the register
fingerprint supports it: `UserContext::new` (used by `replace_image`) zeroes
every GPR, so all-zero callee-saved registers at an image entry point is
exactly what a fresh `execve` looks like.

**It is also exactly what the fork path produced.** In the reference log the
victims *never appear in an `execve` log line at all* —
`[syscall] execve(path=…) PID n` is unconditional (`src/syscall/proc.rs:678`),
pids 56, 58, 59, 60… each log one, and **57 is missing** and is the pid that
faults. Same for pid 575, pid 66, pid 80. The victims are fork children that
died before ever reaching their exec.

Checking for the *absence* of a log line is as informative as reading one, and
it was available from the start.

### 3b. "The victim is running in another process's page tables"

1521 faults resolving to just **two** `(live ttbr0, slot PA)` pairs, each pair
stable for hundreds of faults over minutes, reads unmistakably as "victims
share one long-lived address space in groups". The `[THR-DUMP]` blocks even
name a process (`pid=44 l0=0x82642000`) holding one of those L0s.

It is a coincidence of a **serial fork/exit loop against LIFO allocators**. The
busybox hammer forks, execs, exits, repeats. Each iteration frees an L0 frame
and an ASID and the next iteration gets both back. Under the concurrent `-j4`
load the ASID space is nearly saturated, so `AsidAllocator::alloc`'s round-robin
scan finds the single just-freed slot every time — which is why the ASID looked
pinned as well.

The instrumentation already said so and it was misread: `as_owner == pid` for
**3034 of 3035** faults, and `address_space_owner_pid_for_fault`
(`crates/akuma-exec/src/process/children.rs:1008`) resolves the live TTBR0 to
its **non-shared** owner. A vfork or `CLONE_VM` child would have named its
*parent* there. The victims own the address spaces they fault in.

### 3c. "`/bin/busybox.static` is a static PIE, so it cannot have ld-musl mapped"

Recorded as the sharpest open lead in the previous session's handoff: both
`/bin/busybox` and `/bin/busybox.static` show no `PT_INTERP` under
`llvm-readobj --program-headers`, so a process the kernel names
`/bin/busybox.static` should have nothing whatsoever at `0x30000000` — yet it
faults executing ld-musl at `0x300c03b8`.

Two things are wrong with it.

1. **The readobj was run against the wrong copy.** In the running system,
   `[mprotect] pid=43 owner=43 addr=0x300bf000 len=0x1000 prot=0x1` is ld-musl's
   RELRO being write-protected, and pid 43 mmaps the whole `0x30100000+` loader
   arena. Pid 43 is dynamically linked.
2. **The name on the fault line is the *parent's* binary.** `do_execve` keeps
   `proc.name` current (`src/syscall/proc.rs:842`) and a fork child inherits
   `parent.name`, so a child that dies before its own exec is reported under
   whatever its parent was running.

The lead was a real contradiction — it just wasn't a contradiction about the
kernel.

## 4. The number that cracked it

Two fields in the fault dump vary between the two fault groups and are constant
within them. Re-parsing all 3035 faults against the `[IA-MISS]` lines (which
carry `ppid`) gives a table with **no exceptions**:

| ppid | live ttbr0 | slot PA | `SP_EL0` | faults |
|---|---|---|---|---|
| 43 | `0x3f000082642000` | `0x828283b8` | `0x203fffe1f0` | 2335 |
| 39 | `0x3d000081fcf000` | `0x8232b3b8` | `0x203fffeab0` | 691 |

`SP_EL0` groups by **parent pid**, and — the decisive part — it does *not* vary
with the child's argv. The hammer alternates `busybox true`, `busybox date` and
`busybox echo x`, which have different argv/envp byte counts and therefore
different initial stack pointers. Anything computed from the *new* image would
have three values per group. Only a field **stored on the parent** can be
argv-independent.

That single observation eliminates every "the exec loaded it wrong" theory and
points at one place: a per-thread saved value being handed to the child.

## 5. The bug

`get_saved_user_context(thread_id)` had two branches:

```rust
// 1. the live EL0 trap frame — the register state at the `svc`
if thread_id == current_thread_id() {
    let frame_ptr = CURRENT_TRAP_FRAME[thread_id].load(Ordering::Acquire);
    if frame_ptr != 0 { /* … full register state … */ }
}

// 2. fallback: the slot's published user-mode triple, every GPR zeroed
if ctx.is_user_process != 0 && ctx.user_entry != 0 {
    Some(UserContext { pc: ctx.user_entry, sp: ctx.user_sp,
                       tpidr: ctx.user_tls, /* x0..x30 = 0 */ .. })
}
```

All three child-creation paths — `fork_process`, `vfork_process`,
`clone_thread` — call it and use the result as the child's starting context.

The `user_entry` / `user_sp` / `user_tls` triple is **not** a stale fork
return. `update_thread_context` writes it once, at execve
(`ctx.user_entry = user_context.pc`), and never again. It records where the
thread's image was **first entered**. For a dynamically linked parent, that
address is **ld-musl's `_dlstart`**.

So whenever the fallback was taken, `fork()` produced a process that:

- starts at its parent's loader entry point,
- on its parent's initial exec stack pointer,
- with all 31 GPRs zero,
- in its own address space — a CoW copy of a parent in which ld-musl is mapped
  **and was relocated long ago**.

`_dlstart` then does what `_dlstart` does: it re-runs the RELR loop,
`*slot += base`, over an already-relocated interpreter data page. One `+= base`
per birth, then the tail branch through the word it just corrupted.

Every element of the fingerprint is emitted by that one branch:

| observed | produced by |
|---|---|
| PC in ld-musl at `N × INTERP_BASE + 0x6c964` | `pc = ctx.user_entry` |
| `SP_EL0` constant per parent, argv-independent | `sp = ctx.user_sp` |
| `x19 = x20 = x29 = x30 = 0`; `x30 = 0` ⇒ tail branch, not a call | the fallback zeroes all 31 GPRs |
| victim owns its address space (`as_owner == pid`) | `fork_process` overrides `child_ctx.ttbr0` with the child's own |
| no `execve` line for the victim | it dies in `_dlstart`, before its exec |
| `N` advances per fault, not per exec | only a birth that takes the fallback runs the loop |
| `SIGSEGV after 0.00s` | it is the process's first few hundred instructions |

## 6. Why the earlier fix in the same function did not close it

Commit `c89daca`, earlier the same day, found this fallback and hardened it two
ways: `init_thread_slot_context` now resets the triple when a slot is recycled,
and the fallback additionally requires `user_entry != 0`. Both are correct.
Both target a **recycled slot** — a dead occupant's context leaking into a new
one — and the session recorded honestly that "reachability from the faulting
path is unproven".

The reachable path does not need a recycled slot. It reads the **parent's own
live slot**, mid-`fork`, where `user_entry` is legitimately non-zero and
legitimately ld-musl's `_dlstart`. Neither guard fires.

The general lesson: a guard written against one instance of a defect can leave
the dominant instance untouched, and "this is a real latent defect, not a
demonstrated root cause" is worth writing down exactly as it was — it kept the
door open instead of closing the investigation on a half-fix.

## 7. The fix

The fallback is deleted. `get_saved_user_context` returns `None` when there is
no live EL0 trap frame, and prints, rate-limited (first 8, then powers of two —
unbounded printing under a fork storm wedges the box by itself):

```
[NO-TRAPFRAME] refusing child of tid=… (cur=…) — no live EL0 frame; \
  stale user_entry=0x… user_sp=0x… is_user=… count=N
```

`sys_clone_pidfd` already maps the `Err` to `ENOMEM` with
`[syscall] clone: fork failed: No saved context`. A failed `fork` is loud,
local and recoverable; a process launched at its loader entry point is none of
those.

`no_trap_frame_child_count()` exposes the running total for a boot suite or a
post-run check.

The rationale is now in the function's doc comment, because the fallback is the
kind of code that looks like graceful degradation and reads as obviously
correct to the next person who wants "some context, better than none".

## 8. Still open

1. **Why is the trap frame missing?** `set_current_trap_frame` runs on every SVC
   before dispatch (`src/exceptions.rs:3072`), there is exactly one production
   `handle_syscall` call site, and all three callers pass `current_thread_id()`
   — so a fork syscall should always find its frame. The `[NO-TRAPFRAME]` line
   prints both `tid` and `cur`: differing means a `current_thread_id()`
   mismatch between publish and read; matching means something cleared
   `CURRENT_TRAP_FRAME` under a live thread (the recycler at
   `threading/mod.rs:1619` and the exit paths at `process/mod.rs:975/1644/1783`
   are the only clears).

2. **Why does the slot accumulate instead of sitting at `N = 2`?** The
   interpreter range *is* CoW-shared and demoted read-only at fork
   (`cow_share_and_demote_range`, `interp_base = 0x30000000`,
   `interp_scan_size = 2 MB`, so `0x300c03b8` is covered). A correct CoW copy
   would give every victim the parent's value plus one base — `N = 2`, forever.
   Monotone `N` on a *constant* PA means the write is landing on a frame
   genuinely shared with the long-lived parent, i.e. some path promotes a CoW
   page to writable without copying. Suspects: the refcount accounting around
   `cow_ref_inc` (inserts `count = 2` on first share) and the write-fault
   handler's `refcount == 1 ⇒ promote in place` arm.

   **This is inference from the log, not a read of a confirmed defect.** With
   the fallback gone nothing re-runs `_dlstart`, so it no longer produces this
   crash — but a forked child writing through to its parent's libc data would
   be a serious hole on its own, and it deserves a session.

## 9. Method notes worth keeping

- **Get the grouping variables out of the log before theorising.** Parsing 3035
  faults and joining them to `ppid` took two minutes and settled in one table
  what three sessions had argued about from single register dumps.
- **When a victim's registers look like a *freshly launched* process, ask which
  code path *constructs* that state**, not which one corrupts memory. All-zero
  GPRs at an image entry point is a signature of construction, not damage.
- **A field that is constant when it should vary is a stronger signal than a
  field that is corrupt.** `SP_EL0` being argv-independent was the whole case.
- **Check for absent log lines.** "The victim never logged an `execve`" was
  sitting in the reference log the entire time and reframes the class from an
  exec bug to a fork bug in one step.

## Related

- [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md)
  — the runbook; §2h is this document's summary, §2b/§2e/§2g are the other
  fixes that came out of the same hunt.
- [`../runbooks/debug-futex-lost-wakeup.md`](../runbooks/debug-futex-lost-wakeup.md)
  — the lost-wakeup class this one was repeatedly confused with; §0 there is
  the "tell them apart" checklist.
- [`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) — the build target
  the reproducer runs on.
