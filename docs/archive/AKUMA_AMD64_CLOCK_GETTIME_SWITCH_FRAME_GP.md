# Akuma/amd64: `#GP` in `sys_clock_gettime`, a corrupted switch frame on the console pump daemon's slot

**Grade: C — active risk, STILL OPEN.** Root cause not confirmed; this is a
forensic reconstruction from a single photographed console dump plus static
analysis (disassembly, symbol resolution, source reading) against today's
build. Nothing here has been reproduced yet. Picked up again 2026-09-22.

## Symptom

A photograph of the trashcan's HDMI/TV console (`/Users/netoneko/Downloads/boom.HEIC`,
2026-09-22) shows a fatal `#GP` on the amd64 kernel, preceded by a `[SWITCH
BADFRAME]` diagnostic and a BKL stall storm:

```
[SWITCH BADFRAME]    +0x18 = 0x246
[bkls>] core=4 ticket=12514534 serving=12514531 owner=2 spins=2097152
[SWITCH BADFRAME]    +0x20 = 0x60
[bkls>] core=1 ticket=12514533 serving=12514531 owner=2 spins=4194304
[SWITCH BADFRAME]    +0x28 = 0x6
[bkls>] core=3 ticket=12514532 serving=12514531 owner=2 spins=4194304
[SWITCH BADFRAME]    +0x30 = 0x1
[SWITCH BADFRAME]    +0x38 = 0xffffffff8042c460

[EXCEPTION] #GP general protection err=0x0000000000000000
  rip=0xffffffff8042c489 rsp=0xffff8000207b2d80
  cs=0x0000000000000008 rflags=0x0000000000010086
  cr2=0x0000000100001010 cr3=0x0000000000202000 freed=0
  core=1 task_slot=6 slot_root=0x0000000000000000
  [rsp-64..rsp)
    0000000000000000 ffffffff8042c114 0000000000000000 ffffffff8042c489
    0000000000000008 0000000000010086 ffff8000207b2d80 0000000000000010
  [rsp/physmap]
    ... (64 words; contains c0ffee15deadbeef and beef123456780000) ...
```

The binary that produced this is very likely `target/x86_64-unknown-none/release/akuma-amd64`
built 2026-09-22 17:35 (matches recent `amd64 mm fixes` / `fix storage on amd64`
commits) — symbol/disassembly resolution below assumes this build; a different
revision would shift addresses.

## Confirmed facts

1. **`[SWITCH BADFRAME]`** is `x86_check_incoming_frame` (`crates/akuma-threading/src/lib.rs:3628`),
   called from `x86_yield_now` (`:3576`) right after the machine-level switch
   (`hooks.switch_to`) but before the `popfq;ret` that resumes the incoming
   thread. It only *reports* a bad frame — "the value of stopping here is
   naming it" — it never refuses the switch.

2. **Decoded against the real push order**, taken from the actual asm
   (`crates/akuma-threading/src/lib.rs:2649-2684`, `pushfq; push rbp; push rbx;
   push r12; push r13; push r14; push r15`), which gives layout `+0x00=r15,
   +0x08=r14, +0x10=r13, +0x18=r12, +0x20=rbx, +0x28=rbp, +0x30=rflags,
   +0x38=retaddr` — matching the code comment exactly, no misalignment:

   | offset | field | value | verdict |
   |---|---|---|---|
   | +0x18 | r12 | `0x246` | plausible flags-shaped bit pattern, but wrong field |
   | +0x20 | rbx | `0x60` | implausibly small for a saved pointer |
   | +0x28 | rbp | `0x6` | implausibly small for a saved frame pointer |
   | +0x30 | **rflags** | `0x1` | **architecturally impossible** — real `pushfq` always has bit 1 set |
   | +0x38 | retaddr | `0xffffffff8042c460` | resolves to `sys_clock_gettime+0x90` |

3. **Symbol resolution** (via `nm -n` + nearest-preceding-symbol lookup against
   the build above):
   - `0x8042c460` (BADFRAME's saved retaddr) → `akuma_syscalls_time::sys_clock_gettime+0x90`
   - `0x8042c489` (the actual fault `rip`) → the **same function**, `+0xb9` — 41 bytes later
   - Other `[rsp/physmap]` words resolve near-exactly to: `akuma_amd64::mm::flush_shared_write+0x220`,
     `alloc::collections::btree::map::BTreeMap::drop+0xd0`, `akuma_slot_table::SlotTable::with_active_mut
     (...usermode::spawn_test)+0x22a`, `akuma_amd64::serial::klog_only_dec+0x3f7`,
     `akuma_threading::x86_yield_now+0x465`, and a very-close match inside
     `SlotTable::with_active_mut (...fd::pid_map_rows)+0x20`.

4. **Disassembling `sys_clock_gettime` directly** (`objdump -d`, function bounds
   `0x8042c3d0`–`0x8042c4b0`) shows both addresses land **inside** existing
   instructions, not on instruction boundaries: `0x8042c460` is byte 2 of the
   5-byte `call validate_user_range` at `0x8042c45e`; `0x8042c489` is the last
   byte of the 4-byte `cmoveq %rcx,%rax` at `0x8042c486`. `sys_clock_gettime`
   itself never calls into the scheduler and is a short leaf syscall (read a
   clock hook → validate the user pointer → `copy_to_user` the timespec) — there
   is no legitimate way a thread could be cooperatively switched out with a
   return address inside it, and a genuine one would sit on an instruction
   boundary regardless. **Both anomalies point the same way: these bytes were
   never a real saved frame.**

5. **The `[bkls>]` owner encoding**: `crates/akuma-bkl/src/sync.rs:573`,
   `let me = core_id + 1`; the print at `:706` uses `me` (not raw `core_id`) for
   its own `core=` field, and `owner` stores the same `+1` encoding. Decoding:
   `owner=2` → holder is `core_id=1`. `idt.rs`'s `EXCEPTION core=1` is the raw
   `cpu_index()` (no `+1`). **Same core** — core 1 held the BKL while performing
   this switch (switches on amd64 are only safe under the BKL,
   `amd64/src/sched.rs:987-1000`), and the three waiters (`core_id` 3, 0, 2,
   printed as `core=4/1/3` due to the `+1` offset) spun 2–4M times and never
   stopped, because `idt::fatal()` halts without releasing anything.

6. **`c0ffee15deadbeef`** in the raw `[rsp/physmap]` dump is not garbage — it is
   `amd64/src/sched.rs:1366`'s `STACK_CANARY`, a deliberate 4-word sentinel
   painted at the base (bottom) of every amd64 kernel/trap stack
   (`paint_canary`), checked on both sides of every switch
   (`check_canaries`, `:1650`, called from `hook_switch_to` before
   `x86_check_incoming_frame` runs). Its presence within 64 words of `rsp`
   means the fault's `rsp` is close to a stack boundary — consistent with
   normal shallow call depth landing near a 32 KiB (`STACK_SIZE`, `:82`)
   allocation's edge, adjacent to the next `vec![0u8;32K].leak()` allocation.

7. **`freed=0`** rules out the previously-fixed "switch into a freed CR3" bug
   (`AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md`). `task_slot=6 slot_root=0x0` is
   ambiguous on its own (could be a legitimate kernel-only thread with no user
   address space, or a "never got a real value" gap).

## This is very likely the same bug family as two recent, already-documented investigations

```
2026-09-12  AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md
            "Two cores on one stack": ON_CPU[cur] cleared before the stack
            moves, visible to peers before the switch finishes, UNLESS the
            switch is BKL-protected. Fixed for the one known unlocked caller
            (sched::yield_now now takes the BKL if the caller holds none).
            Canaries + x86_check_incoming_frame added as DETECTORS only —
            "A canary does not fix that; it says which stack and which
            direction, which no capture so far has."
            One of that bug's four captures: "slot 6, kernel root
            (the console pump daemon)" — flags word alone corrupted,
            return address always intact and correct.
            Doc's own words: "The deeper fragility is not fixed and should be."

2026-09-20  AKUMA_AMD64_BKL_NETWORKING.md
            Same fragility, syscall-park angle: a parked syscall holds the BKL
            for its whole sleep (amd64 has no AArch64-style reconcile-on-return).
            Three-step fix plan:
              1. Linux-style prev-handoff in x86_yield_now/x86_pick_next
                 -- LANDED, bare-metal-verified 2026-09-20 night.
              2. Release-the-BKL-across-park (block_current/block_until_deadline)
                 -- ATTEMPTED, REVERTED same night: wedged the box at the
                 *netpoll daemon's* first parks ("[SWITCH NO-BKL] from=4 to=1
                 core=1 via=block_until_deadline", then silence, twice).
                 Live in `amd64/src/sched.rs:892-901` as a still-reverted comment.
              3. A reconcile in amd64's ring-3 return paths (syscall exit,
                 enter_user, IRQ epilogues) mirroring AArch64's
                 reconcile_for_spsr -- NOT DONE, explicitly listed as pending.

2026-09-22  <- this crash
```

`console::pump_daemon` (`amd64/src/console.rs:392`, spawned via
`crate::sched::spawn_daemon`) is spawned from `boot.rs:273`
(`console::init()`), before `net::spawn_netpoll()` (`multiboot2.rs:552`, later
in network bring-up) — so it plausibly still gets a lower, earlier slot number
than the netpoll daemon, making task_slot 6 a plausible repeat appearance of
the *same specific victim thread* named in the 2026-09-12 capture table. Not
proven for this exact boot (slot numbers shift with `SMP=N` and boot-path
details), but consistent.

## Theories

### A (leading): recurrence of the "two cores, one stack" race, via a still-open gap

```
core 2                              core 1 (owner of BKL, per bkls decode)
──────                              ──────
console pump daemon (slot 6)        picks slot 6 as `next` via x86_pick_next
running normally, rsp near          (slot 6 is READY, not ON_CPU, can_run==true)
its stack's mid-region
   │
   │  <-- something reaches x86_yield_now's switch machinery for
   │      slot 6 WITHOUT the BKL held (a caller reachable from the
   │      still-open gap: item 3's missing reconcile, or a residual
   │      path adjacent to the reverted item 2 experiment)
   │
   ▼
ON_CPU[6] cleared BEFORE the stack
actually stops moving  ────────────►  x86_pick_next sees ON_CPU[6]==0,
                                       picks slot 6, reads Context[6].rsp
                                       -- STILL the OLD value, because
                                       core 2 hasn't finished pushing
                                       its own frame yet
   │
   │  core 2 keeps pushing:                core 1 starts POPPING off
   │  pushfq; push rbp; push rbx;          the SAME stack address,
   │  push r12; push r13; push r14;        interleaved with core 2's
   │  push r15; mov [rdi],rsp   <── SAME MEMORY ──►  writes landing UNDER it
   ▼
result: the 8-word frame core 1 reads is a torn mix of core 2's
in-flight pushes and whatever was there before → rflags=0x1 (not a
real pushfq value), retaddr lands mid-instruction (not a real `call`
return address either)
```

Matches the *class* exactly (busy permanent kernel daemon; corrupted
`rflags`; the exact detectors that exist *because of* this failure mode).
Doesn't match perfectly: 2026-09-12's captures always showed **exactly one**
corrupted word (`rflags`; retaddr always intact/correct). Ours has **two**
words that don't check out, arguing for a wider or doubled race window —
consistent with something having changed in the last two days (item 2's
revert, or an adjacent change) rather than the original, fully-patched
mechanism recurring unmodified.

### B: stale `Context` from amd64's missing slot recycler

```
crates/akuma-threading/src/lib.rs — TWO different reclaim paths exist:

 path 1 (generic "cleanup", :2199-2340)      path 2 (x86_claim_slot, :3024)
 ─────────────────────────────────           ─────────────────────────
 TERMINATED → INITIALIZING (CAS)             TERMINATED → INITIALIZING (CAS)
 *get_context_mut(i) = Context::zero()       scrub_thread_slot(i)  <- does NOT
 (explicitly zeroes the saved frame)            touch THREAD_CONTEXTS at all
 → FREE                                      → INITIALIZING, waits for caller
                                                to call x86_build_closure_context
                                                before ever publishing READY
```

`crates/akuma-threading/src/lib.rs:5212` states the invariant other code
*assumes*: "TERMINATED contexts are zeroed by the recycler" — true on AArch64,
not general on amd64 (only whichever path actually rewrites `Context` makes it
fresh). `AKUMA_AMD64_NO_SLOT_RECYCLER.md` already found this shape for
`Machine::space_root`. Weaker for slot 6 specifically since a permanent daemon
is never supposed to terminate — worth ruling out only if slot 6 was ever
recycled this boot.

### C: adjacent-stack overflow, canary blind on the victim's side

```
memory, low → high address
┌──────────────────────┬──────────────────────┐
│ Task A's stack        │ Task B's stack (slot 6)│
│ (32 KiB, vec::leak)   │ (32 KiB, vec::leak)    │
├──────────┬────────────┼────────────┬───────────┤
│ canary   │ ... grows  │ ... grows  │  canary    │
│ (base,   │ DOWN from  │ DOWN from  │  (base,    │
│ checked) │ top        │ top        │  checked)  │
└──────────┴─────▲──────┴─────▲──────┴────────────┘
                  │            │
        A overflows PAST      B's live saved-frame,
        its OWN base, into    near ITS OWN stack_top,
        whatever sits         gets scribbled by A's
        just below in         normal push/pop traffic
        memory — B's TOP      (B's own canary, at B's
        end                   OWN base, is untouched —
                               so check_canaries(6,...)
                               reports nothing)
```

`check_canaries` only inspects the 4 words at each stack's *base* (bottom). A
neighbor overflowing into slot 6's *top* region — where its live saved-frame
lives — corrupts exactly these bytes while leaving slot 6's own canary intact
and silent. Directly testable against a fuller log (see below).

## What would settle this without running anything new

The photo is a snapshot of a scrolling console and almost certainly cuts off
lines above what's visible. `check_canaries` runs on both sides of *every*
switch, before `x86_check_incoming_frame`:

- A `[STACK CANARY] slot=6 ... at=switch-in` line just above what's captured
  → Theory C, and it names which stack (kernel/trap).
- A `[STACK CANARY] slot=<other> ... at=switch-out` for a different slot,
  anywhere earlier in the boot → the overflowing culprit for Theory C.
- Neither → favors Theory A or B.

Also worth a plain read: `git log -p --since=2026-09-20 -- amd64/src/sched.rs
crates/akuma-threading/src/lib.rs` for anything touching `yield_now`,
`block_current`, `block_until_deadline`, `x86_pick_next`, or the BKL-drop-window
helpers since the revert, not already accounted for in the two archive docs
above. The recent `amd64 mm fixes` / `fix storage on amd64` commits are worth a
skim for anything that brushes the scheduler, park, or BKL paths incidentally.

## A way to trigger it

The 2026-09-20 doc's own repro hits this fragility via socket/park churn; this
needs the console-pump-daemon-and-clock corner specifically — combine console
pressure (keep slot 6 switching constantly) with clock-syscall pressure (make
`sys_clock_gettime`'s code a live target for whatever's executing when a tear
happens):

```bash
# amd64, SMP >= 2 (the race needs a second core to interleave against slot 6)
SMP=4 HTTP_PORT=8084 SSH_PORT=2258 INIT=/bin/sshd sh amd64/run.sh

# from several concurrent ssh sessions:
#  (a) console-pump churn: heavy, continuous stdout, so the pump daemon
#      parks/wakes/switches constantly
for i in 1 2 3 4; do
  ssh -p 2258 root@localhost 'yes "console pressure line $$" | head -c 50000000' &
done
#  (b) clock syscall churn: many processes calling clock_gettime tightly,
#      so something is likely mid-syscall in that exact function at tear time
for i in 1 2 3 4; do
  ssh -p 2258 root@localhost 'while true; do date +%s.%N >/dev/null; done' &
done
# fork/exec churn, matching the original doc's trigger shape:
ssh -p 2258 root@localhost 'while true; do ( ls /bin; ls /bin ); done' &
```

Watch for `[SWITCH NO-BKL]`, `[STACK CANARY]`, or `[SWITCH BADFRAME]` before
the eventual `#GP`/`#DB`. Per `AKUMA_AMD64_BKL_NETWORKING.md`, this class is
intermittent and load-shaped, and TCG-serialized QEMU can hide it entirely —
run **one VM at a time** (ideally the real hardware or a KVM rig), not
concurrent QEMU instances sharing a disk image.

## Status / next steps

Nothing here has been reproduced. In order of cheapest-first:

1. Check the fuller boot log (if it still exists) for `[STACK CANARY]` lines
   before the captured `[SWITCH BADFRAME]` block.
2. `git log -p` the scheduler/threading files since 2026-09-20 for anything not
   already covered by the two archive docs.
3. If neither turns up an answer, run the trigger recipe above on an isolated
   rig (not the trashcan, until the mechanism is understood — see
   `AKUMA_AMD64_XHCI_WEDGED_BOX` for what a bad power state costs on that
   machine) and capture with `GDB=1` for a live catch.

## Background

- [`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`](AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md) —
  the original "two cores, one stack" bug, the canary and `x86_check_incoming_frame`
  detectors, and its own explicit "deeper fragility... not fixed" callout.
- [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md) — the
  2026-09-20 audit, the landed prev-handoff fix, the reverted release-across-park
  attempt, and the still-pending ring-3 reconcile.
- [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) — the
  general "amd64 doesn't zero a recycled slot's fields" pattern (Theory B).
- [`AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md`](AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md) —
  the freed-CR3 shape this crash's `freed=0` rules out.
