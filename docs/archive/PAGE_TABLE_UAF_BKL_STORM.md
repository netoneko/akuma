# The `[BKL] stuck` storm: a page table freed under a running core

**Date:** 2026-08-08. **Branch:** `stabilize-devbox`. **Kernel:** `159c3db`
(`release-smp-shared` + `devbox-smoltcp,no-tests`), SMP=4, MEMORY=14336, HVF,
in-guest `cargo build -j4`. **Status:** CLOSED 2026-08-09 — the remaining
trigger was root-caused with live captures (§7: the retired-process reclaim
frees a dead process's address space while the exiting thread's core still
has it in `TTBR0_EL1`; two concrete routes caught in the act), and the
structural gap is fixed by a per-core live-TTBR0 registry gating every
page-table-frame free (§7.3). §4.1's `execve` sibling-kill fix stands on its
own and remains in place.

**One line:** a core kept running with `TTBR0_EL1` pointing at an address space
whose page-table frames had already been freed and **poisoned**, so it could no
longer translate kernel text — including the exception vector it faulted to —
and spun in an unbreakable instruction-abort loop while holding the BKL.

This is the *other* defect behind the `-j4` campaign deaths. Its sibling — the
majority case, a `TALC`↔`PMM` lock cycle — is fixed and written up in
[`PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md`](PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md).
The two were previously filed as one "storm/wedge class"
([`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
§12.7–§12.8); they are unrelated.

---

## 1. Signature

```
[BKL] stuck: owner=4 waiter=3 tag=511 (aff0+1)     ... x18,382, ALL owner=4
```

`owner=4` means core 3 (`aff0 + 1`). `tag=511` is `HOLD_TAG_UNKNOWN` and means
nothing on a normal build — the profiler is behind the `bkl-profile` feature, so
every tag-writing function early-returns. **Read `owner=`, never `tag=`.**

Distinguishing it from the silent wedge, which looks superficially identical from
outside (all cores pinned, ~400% host CPU):

| | this storm | silent wedge |
| --- | --- | --- |
| `KERNEL_LOCK` | **HELD**, `owner=4` | free, `owner=0` |
| `allocator::TALC` / `pmm::PMM` | **both free** | **both `0x01`** |
| `[BKL] stuck` lines | thousands | 0–1 |
| cores | 3 × `KernelLock::acquire`, 1 × `exception_vector_table+512` | 3 × `TALC`, 1 × `PMM` |

## 2. The decisive read

Captured with [`../../scripts/lockprobe.py`](../../scripts/lockprobe.py) against
the gdbstub (`GDB=1`) on a live storming VM:

```
BKL owner=4  next_ticket=1100195  now_serving=1100191      (4 waiters queued)
allocator::TALC @ 0x402e4690: 0x00   <- free
pmm::PMM        @ 0x402460e0: 0x00   <- free

CPU#0/1/2  KernelLock::acquire+620          (spinning for the BKL)
CPU#3      exception_vector_table+512
           ESR_EL1 = 0x86000005   EC=0x21 instruction abort, SAME EL; IFSC=0x05
                                  translation fault, level 1
           ELR_EL1 = 0x4011a200   = exception_vector_table+512
           FAR_EL1 = 0x4011a200   = exception_vector_table+512
```

`FAR == ELR == PC == the vector entry`. The core is not *running* the handler —
it faults **fetching** it. The abort vectors to `0x4011a200`, whose fetch needs
the same broken translation, so it aborts again. No instruction ever retires.

That single fact explains every earlier observation that had looked
contradictory: byte-identical registers across an 8 s gap, ~100% CPU on that
core, `PSTATE=0x3c5` (DAIF fully masked, set on exception entry), and a BKL that
is never released.

> An earlier reading in this investigation called it a
> `fault → handler → eret → refault` loop. Right shape, wrong mechanism: there is
> no `eret`, because control never reaches the handler at all.

## 3. Root cause: the page table is poisoned freed memory

Kernel text lives at a **low** VA (`0x4011a200`), so it is translated through
`TTBR0_EL1`, not `TTBR1`. Every user address space therefore has to carry the
kernel identity map. Per-core `TTBR0_EL1` at the moment of the storm:

| core | `TTBR0_EL1` | |
| --- | --- | --- |
| CPU#0 | `0x40313000` | the kernel table (equals `TTBR1_EL1` on all cores) |
| CPU#1 | `0x0002_000090648000` | user AS, ASID 2 |
| CPU#2 | `0x0052_0000b1ed1000` | user AS, ASID 0x52 |
| **CPU#3** | `0x002a_0000996c0000` | user AS, ASID 0x2a — the stuck core |

Walking CPU#3's table for VA `0x4011a200` (L0 index 0, L1 index 1):

```
L0 @ 0x996c0000 : 0x00000000996c6003   -> table at 0x996c6000
L1 @ 0x996c6000 : 0xfeedface47c16000  0xfeedface47c16000  0xfeedface47c16000 ...
kernel L1 for comparison @ 0x40315000 : 0x...0601  0x40003003  0x8000070d  0xc000070d
```

The L1 table is not a page table any more. It is **PMM poison**, and it decodes
exactly:

```
poison_word(pa) = POISON_MAGIC ^ pa
0xFEEDFACEDEAD0000 ^ 0x996c6000 = 0xfeedface47c16000      ✓
```

`0x996c6000` is that table's **own** address. So the frame was freed to the PMM
and stamped by the quarantine while it was still installed as a live page table
in a running core's `TTBR0`. Poison has bit[0] = 0, i.e. an invalid descriptor —
hence `IFSC=0x05`, translation fault at level 1.

Also visibly corrupt on the same table: `L0[1] = 0x0000000040000003`, a table
descriptor pointing at `0x40000000` (the kernel load base, not a page-table
frame).

## 4. The trigger: `execve`

The storm begins immediately after an address-space **replacement**:

```
[T36.48] [syscall] execve(path="/usr/local/bin/rustc", ... ) PID 15
[eventfd] close id=1 ref_count=1
[pipe] DESTROY id=4 (both counts 0)
[pipe] DESTROY id=5 (both counts 0)
[BKL] stuck: owner=4 waiter=3 tag=511 (aff0+1)      <- first stuck line
```

`execve` tears down the old address space and installs a new one. Nothing
observed here establishes that no *other* core is still executing on the old one:
freeing its page-table frames while a peer's `TTBR0` still points at them is
precisely the observed state. The same family as the previously fixed vfork
stale-`TTBR0` bug and the AS-MISMATCH class — a core left running on an address
space that was reclaimed underneath it.

**Not yet established:** which teardown path frees the frames, and whether the
gap is a missing cross-core quiescence (no "is any core still on this AS?" check
/ no TLB+TTBR shootdown) or a refcount that drops early. That is the next step.

## 4.1 A confirmed, concrete gap: `execve` never killed thread-group siblings

`execve` is POSIX-required to destroy every other thread in the calling
process's thread group before the image is replaced — the calling thread is
the only survivor. Akuma's `exit_group`/`kill` path already implements exactly
this for its own teardown (`crates/akuma-exec/src/process/mod.rs:1894-1899`):

> "If this process owns the address space (not shared), kill all sibling
> CLONE_VM threads BEFORE dropping. Dropping the owner frees all page tables;
> siblings still using them would cause EL1 faults."

— i.e. the exact symptom this storm produces. `Process::replace_image` /
`replace_image_from_path` (`crates/akuma-exec/src/process/image.rs`), however,
swapped `self.address_space` with no equivalent call anywhere in the `execve`
path. A `CLONE_THREAD` sibling that outlives the phase that spawned it (a
parked rayon/thread-pool worker is the textbook case — rustc uses one) keeps
running under the old address space's `l0`/ASID while `execve` drops it: if the
software refcount (`SHARED_L0_TABLE`, `mmu/mod.rs`) doesn't see that sibling as
a live view for any reason, or the sibling is mid-teardown itself, the frames
free and poison out from under a peer core that still has them loaded in
`TTBR0_EL1` — the fault this doc documents.

**Fixed** (`crates/akuma-exec/src/process/image.rs`, `kill_exec_siblings`):
both `replace_image` and `replace_image_from_path` now call
`kill_thread_group` on any other thread-group member, mirroring the exit
path, right after the new ELF has loaded successfully (so a failed exec still
leaves siblings untouched) and before the destructive AS swap.

Verified live (SMP=4, `devbox-smoltcp`): a pthread worker parked in a tight
loop, then the main thread `execve`'s into `/bin/echo`. The console shows the
new call firing and the sibling being fully reaped before the swap:

```
[KTG] my_pid=21 my_tgid=21 by_tid=10 code=0 siblings=1 first=Some((22, Some(11)))
[KTG] my_pid=21 my_tgid=21 by_tid=10 code=0 siblings=0 first=None   <- exit_group's own
                                                                         call now finds nothing left
```

Repeated 6× back to back: exec succeeded every time, PMM free count held
steady across runs (no leak from the new kill), and no `[BKL] stuck` /
poison / `TTBR SAVE-MISMATCH` / `TTBR LOAD-MISMATCH` line appeared.

**What this does and doesn't close:** this removes a real, previously-missing
POSIX-correctness guarantee and the most plausible concrete trigger for a
sibling thread outliving an `execve`-time AS drop. It has **not** been
verified against the storm's actual reproduction rate (~1 round in 12 of a
`-j4` self-host `cargo build`) — that requires a multi-hour, multi-round
campaign of the kind run for
[`PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md`](PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md),
not yet repeated here. It also does **not** address the deeper, independently
real gap: there is still no cross-core check ("is any core's live `TTBR0_EL1`
resident on this L0 right now") before `UserAddressSpace::drop` frees
page-table frames (§5) — only a refcount over *software objects*. A `kill_thread_group`
straggler that is hard-terminated after its 2 s grace period (`process/mod.rs`,
the `KILL_GRACE_US` path) is marked dead from the killer's side while its own
core may not have run the switch-out that would reprogram that core's
`TTBR0_EL1` away from the dying AS — a second, independent way to reach the
same fault, unaffected by this fix, that a real IPI-based TTBR shootdown or a
liveness check in the PMM free path would still be needed to close.

## 4.2 Post-fix campaign: the storm still reproduces

Methodology mirrors [`PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md`](PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md)
§7: fresh VM boot per round (`SNAPSHOT=1`, writes discarded), `rm -rf
target/aarch64-unknown-none` before each round, then an in-guest self-host
`cargo build ... -j4` against the fixed kernel (§4.1's `execve` sibling-kill
committed at `9a9eb04`), SMP=4, MEMORY=14336. Two parallel lanes, 1200s budget
per round. The cloned in-guest repo on this run's `disk_selfhost.img` is an
older/smaller snapshot than the one behind the original ~1-in-12 measurement,
so individual GREEN rounds finish in ~165-190s rather than ~11+ minutes — cheap
enough to run far more rounds for the same wall-clock budget.

Results after 17 completed rounds (campaign still running past this snapshot):

| outcome | n |
| --- | ---: |
| `GREEN` | 15 |
| **`BKL_STORM`** | **2** |
| silent wedge | 0 |
| `EXIT=139` | 0 |

~1 storm per 8-9 rounds here — noisier than the original ~1-in-12, consistent
with a smaller/faster per-round build giving less total build time between
samples rather than a higher true underlying rate; not a rate this campaign
size can pin down precisely (see §4.2.2). Both storms reproduce the exact
signature from §1-§2: `KERNEL_LOCK` HELD (`owner=4` = core 3 both times),
`allocator::TALC`/`pmm::PMM` both free (rules out the silent-wedge class), 3
cores spinning at `KernelLock::acquire+620`, the 4th core's PC pinned at
`exception_vector_table+0x200` — unable to even fetch its own fault handler.
Round 9 (lane 1) ran the full 1200s budget (finalized at 1227s, 37,719
`[BKL] stuck` lines — round 31's earlier capture was 40,082, same order of
magnitude) and was live-probed via `lockprobe.py` while it was actually
storming:

```
BKL @ 0x40329150 owner=4 next_ticket=6474823 now_serving=6474819
VERDICT: BKL HELD by core 3
allocator::TALC: 0x00 (free)   pmm::PMM: 0x00 (free)

CPU#0/1/2  PC = KernelLock::acquire+620
CPU#3      PC = 0x4011a200 (exception_vector_table)
           SP = 0x88773330 — Cannot access memory at that address
```

Even CPU#3's own stack pointer is unreadable through its live translation —
not just the vector table. **The fix in §4.1 is real and necessary (it closes
a genuine POSIX gap and a plausible concrete trigger), but it is not
sufficient**: this campaign is running the fixed binary and the storm still
hit twice in the first 17 rounds. That is fully consistent with §4.1's own
"what this does and doesn't close" — the missing cross-core TTBR liveness
check ahead of `UserAddressSpace::drop`'s frame-free is still the open gap,
and nothing observed in this campaign points at CLONE_THREAD siblings as this
particular occurrence's trigger (the self-host build's `execve` targets are
ordinary fork+exec'd single-threaded tools, not multi-threaded processes) —
so the "hard-terminated straggler leaves its own core's `TTBR0_EL1`
unreprogrammed" path flagged in §4.1, or some other still-unidentified
teardown race, remains the live suspect.

### 4.2.1 Rate caveat — don't over-read 2/17

2 storms in 17 rounds (~12%) looks similar to the original ~1-in-12 estimate,
but the two campaigns aren't measuring the same thing precisely: rounds here
are a smaller/older self-host snapshot finishing in ~165-190s rather than the
original's ~11+ minutes, so each round represents far less total build/exec
activity. If the storm's true trigger scales with wall-clock build time (or
total execve count) rather than with "one round," this campaign's per-round
rate would be expected to *undercount* relative to the original measurement,
not match it — the fact that it instead came out similar could mean the
trigger doesn't scale that way, or could just be small-sample noise (a 95%
CI on 2/17 is roughly 3%-38%, wide enough to be consistent with almost
anything from "somewhat rarer" to "somewhat more common" than 1-in-12). Take
this campaign as confirmation the storm still reproduces on the fixed kernel
and that its signature is unchanged — not as a precise post-fix rate.

### 4.2.2 New instrument: per-core exception-entry counters

Added `exceptions::EXCEPTION_ENTRIES` (`src/exceptions.rs`), an 8-slot
`AtomicU64` array incremented as the first statement of every exception
handler (`rust_sync_el1_handler`, `rust_sync_el0_handler`,
`rust_irq_handler_with_sp`, `rust_default_exception_handler`), indexed by
`bkl::current_core_id()`. Printed every 30s alongside `[PSTATS]` as `[EXC]
core0=N core1=N ...`, and independently readable live via gdbstub even when
the printer itself can't run.

**What it confirmed, read live via `aarch64-elf-gdb -ex "target remote
:1235" -ex "x/8gx 0x40258810"` against the storming round-9 VM, two samples
6s apart:** all four cores' counts were byte-identical across the gap —
`0xf4ff4 0xffac1 0xfaf5b 0x10507d` unchanged. That is a clean confirmation
that **zero exception-vector entries succeeded anywhere in the machine** for
that whole window, not merely on the one core sitting at the unreachable
vector.

**What it did *not* show, and why:** the original hope was a frozen-vs-climbing
split — core 3 flat, the other three still climbing, as sharp proof that
*specifically* core 3 stopped while its peers kept running. That didn't
happen, because the three peers spin on `KernelLock::acquire` with IRQs
masked (by design, for the spin loop's own bookkeeping) — they take no
exceptions at all while waiting, healthy or not. So this counter proves the
*storm* freezes exception traffic system-wide (useful, and it also would have
caught the disproven `fault → handler → eret → refault` reading, since that
shape *would* show the stuck core's count climbing fast while doing nothing
else — see §2), but it cannot isolate "this one core is uniquely broken" the
way a per-core instructions-retired count would.

Also tried live: reading `PMCCNTR_EL0` (the ARM PMU cycle counter) via the
same gdbstub connection, on all four threads. It read `0x0` on every core —
Akuma's kernel never enables the PMU, so this register carries no signal here.
Confirming "the broken core retires literally zero instructions while its
spinning peers keep retiring plenty" would need the kernel to explicitly
configure and enable a PMU event counter first; not done in this session.

## 5. Why no instrument caught it

The PMM already has a use-after-free detector (`config::PMM_UAF_QUARANTINE`), and
it reported **nothing** — `PMM-UAF` and `PMM-QUAR-DF` counts were both 0 in the
storming console.

That is not a broken detector; it is out of scope by construction. Quarantine
verifies poison on the way *out* of quarantine, so it catches a **CPU write**
through a freed frame. Here the frame is consumed by the **MMU as a page table**:
the hardware page-table walker *reads* it. Nothing in software ever touches the
page, so there is nothing to detect.

Any instrument for this class has to look at the other end — e.g. validating that
a frame about to be freed is not referenced by any live `TTBR0`, or refusing to
free page-table frames of an address space that is still installed on some core.

## 6. Frequency

Two independent 2-lane `-j4` campaigns (fresh VM per round, clean
`target/aarch64-unknown-none`, SMP=4):

| kernel | rounds | GREEN | silent wedge | **this storm** | `EXIT=139` |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline `600640711...` | 25 | 18 | 6 | 1 | 0 |
| PMM/TALC fixed `27903e0e...` | 23 | 20 | **0** | **2** | 1 |
| `execve`-siblings fixed `9a9eb04` (§4.2, smaller/faster build, don't compare rates directly — §4.2.1) | 17 | 15 | 0 | **2** | 0 |

So this storm is **unaffected** by the allocator fix — as expected — and with the
silent wedge eliminated it is now the **dominant remaining failure** of in-guest
`-j4` builds, at roughly 1 round in 12. The `execve`-siblings fix (§4.1) doesn't
move this number either: it closes a real, separate gap, but this storm's
actual trigger is still open.

### 6.1 Second instance, same signature

The second capture (fixed kernel, 29,488 `[BKL] stuck` lines, **29,541 ×
`owner=1`**) reproduces §2 exactly, with only the core identity changed:

```
CPU#0  exception_vector_table+512   ESR_EL1=0x86000005  FAR_EL1=0x4011a200
CPU#1/2/3                            KernelLock::acquire (spinning)
BKL owner byte @ 0x40329178 = 0x1    -> core 0 holds it
TALC @ 0x402e4690 = 0x0              free
PMM  @ 0x402460e0 = 0x0              free
```

That preserves the discriminator in §1: **storm = BKL held + allocator locks
free; silent wedge = BKL idle + allocator locks held.**

> Read those two lock bytes from a *direct* `p/x *(unsigned char*)ADDR`, not from
> a summary. In this very capture `lockprobe.py` first reported both as "HELD"
> because gdb's `echo` marker landed on the same line as the `x/8xb` output, the
> byte values never parsed, and the unparsed case defaulted to HELD. Fixed to
> print `<UNPARSED — state UNKNOWN>`; the direct read above is what settled it.

### 6.2 Harness note

This storm is a **slow burn** — roughly 20 `[BKL] stuck` lines/second, ~18k over
the round. A watchdog that flags "more than N stuck lines in a 20 s window" can
sit just under the threshold and, because the console *is* still growing, keep
reporting the VM as healthy. The round then burns its entire budget. Detect it by
`owner=` being pinned to one value across thousands of lines, not by console
growth or by rate alone.

## 7. Root cause confirmed: reclaim frees the AS before the exiting core moves off it (2026-08-09)

§4.2 left the trigger open: the storm reproduced on the `execve`-siblings-fixed
kernel, and the self-host build's exec targets are not CLONE_THREAD-heavy. The
missing link was that the console had **no way to tie a PID to an ASID/L0** —
a lockprobe capture names the stuck core's `TTBR0` precisely, but nothing in
the log said whose table that was or who freed it.

### 7.1 The instrument that settled it

Heap-free `[AS-*]` lifecycle traces (`mmu::as_trace`, same stack-buffer pattern
as `[KTG]`), emitted at every point an address space is created, swapped, or
has its frames freed:

```
[AS-NEW]  pid=N l0=0x… asid=0x… via=fork|vfork|clone|spawn parent=M
[AS-EXEC] pid=N old_l0=0x… old_asid=0x… new_l0=0x… new_asid=0x… core=C
[AS-DEFER] / [AS-FREE] l0=0x… asid=0x… path=owner|last-view core=C
[KTG-HARD] my_pid=… sib_pid=… tid=… core=C      (grace-expiry hard terminate)
```

With these, a storm capture's `TTBR0 = (asid, l0)` greps straight to the
owning pid and the exact free that killed the machine. Both lanes of the very
first instrumented campaign stormed (rounds 2 and 14, `logs/j4_campaign_astrace/`),
and both correlated completely.

### 7.2 Two routes, one gap — caught in the act

**Route A — cross-core: the reaper races the exiting core's final switch**
(lane 1 round 2). pid 323 (`num-traits` build script) runs on CPU#2 under
`(asid 0x88, l0 0x99f3c000)`:

```
[T41.22] [PROC-EXIT] pid=323 … code=0
[TERM] tid=15 pid=Some(323) by_tid=17 state=1 … at=table.rs:199   <- parent's wait4 reap
        (state=1 = READY: tid 15 had NOT finished its own exit path)
  … ~100 ms of thread-slot cleanup …
[AS-FREE] l0=0x99f3c000 asid=0x88 path=owner core=3               <- reclaim, on core 3
[BKL] stuck: owner=3 …                                            <- CPU#2 storms
```

The live probe shows CPU#2 pinned at `exception_vector_table+512`
(`ESR=0x86000004`) with `x8 = 0x0088_0000_99f3c000` — pid 323's exact
ASID+L0. The parent's `wait4` → `unregister_process` (table.rs:199) marked the
still-live thread TERMINATED from another core; `reclaim_retired_processes`
then freed the `Process` — and with it the address space — on the 10 ms
cooldown, while tid 15's core had never run the switch that would move its
`TTBR0_EL1` off the dying table. Under `-j4` BKL contention an exit epilogue
easily outlives 10 ms.

**Route B — same-core: exit_group's own drain frees the table it stands on**
(lane 2 round 14). pid 1185 (`collect2`) on core 2 under `(0x97, 0xcbbdd000)`:

```
[T124.52] [PROC-EXIT] pid=1185 … code=0
[TERM] tid=14 pid=Some(1185) by_tid=17 state=1 … at=table.rs:199  <- parent reap (retires 1185)
[TERM] tid=14 pid=Some(1185) by_tid=14 state=2 at=proc.rs:415     <- its own exit_group
[AS-FREE] l0=0xcbbdd000 asid=0x97 path=owner core=2               <- freed BY core 2, ON core 2
```

`sys_exit_group` marks its own thread TERMINATED and then calls
`drain_retired_before_parking()` → `reclaim_retired_processes` — which
reclaims its **own just-retired Process** and frees the L0 the calling core is
still executing under, mid-syscall, before the `loop { yield_now() }` ever
switches away. The stuck core (CPU#2, holding the BKL) matches, with
`x8 = 0xcbbdd000`.

Same round: 39 `[KTG-HARD]` grace-expiry hard-terminates fired — §4.1's
"straggler's core keeps its TTBR0" path is real and frequent too. All three
routes (reap-race, self-drain, KTG straggler) are the one structural gap
from §5: **nothing on the free side ever checked the hardware.**

### 7.3 The fix: a per-core live-TTBR0 registry gating every frame free

`crates/akuma-exec/src/mmu/mod.rs`:

- `ACTIVE_L0[core]` / `PREV_L0[core]` — published atomics updated by **every**
  `TTBR0_EL1` write in the kernel (there are exactly three: `activate()`,
  `deactivate()`, and the SGI context-switch in `threading/mod.rs`). Publish
  protocol on a transition A→B: `PREV=A; ACTIVE=B; msr ttbr0_el1; PREV=0` —
  at every instant the live L0 base is in {ACTIVE, PREV}, so a scanner that
  sees neither knows the core is off the table. A dying L0 cannot be *newly*
  installed (installing requires a live `UserAddressSpace`, which forbids the
  drop), so "not held" is stable, not a racy snapshot.
- `free_or_defer_as_frames(...)` — the **sole exit** for page-table + user
  frames from both `UserAddressSpace::drop` branches (owner-immediate and
  last-shared-view). If `any_core_on_l0(l0)` hits, the frames park in
  `PENDING_TTBR_FREES` (`[AS-FREE-DEFER]` trace) instead of being freed and
  poisoned; `drain_pending_ttbr_frees()` releases them once the holder has
  demonstrably reprogrammed (retried from every `UserAddressSpace::drop` and
  from `drain_retired_if_requested`). ASID flush/release ordering unchanged.
- Worst case (a core wedged in a non-yielding EL1 loop forever) converts the
  storm into a bounded deferred free — a leak-until-switch, not a dead machine.

Boot-suite regression test: `test_as_drop_defers_while_core_on_l0`
(`src/process_tests.rs`) — fakes a peer core on the L0 via
`mmu::test_publish_core_l0`, asserts drop parks instead of freeing, drains
refuse while held, release after, and PMM conservation. PASSED at SMP=4.

Note for future readers: `[Test] thread_slot_reclaim_on_spawn` FAILS
(`hot_reclaim≈190, want 0`) on this boot config (disk_selfhost.img, SMP=4,
MEMORY=14336) on **unmodified HEAD `1f263a0` too** — pre-existing timing
sensitivity in that test (the ~250-slot fill loop outlives the 10 ms
cooldown), unrelated to this fix.

### 7.4 Post-fix campaign

Same methodology as §4.2 (2 lanes, fresh VM + clean `target/` per round,
SMP=4, MEMORY=14336, 1200 s budget, `logs/j4_campaign_ttbrgate/`). On the
unfixed kernel this session, **both of the first 2 rounds per lane stormed**
(2 storms in 4 rounds; combined with §4.2, 4 storms in 21 rounds on
`9a9eb04`+instrumentation). Results on the fixed kernel (`logs/j4_campaign_ttbrgate/results.jsonl`,
32 rounds, 2026-08-09):

| outcome | n | note |
| --- | ---: | --- |
| `GREEN` | 31 | ~166-227 s/round |
| **this storm** | **0** | |
| build stall (driver label `SILENT_WEDGE`) | 1 | round 29 — **not this defect and not the TALC/PMM wedge**, see below |
| `EXIT=139` / `BOOT_FAIL` | 0 | |

Against the unfixed baseline — 4 storms in 21 rounds on
`9a9eb04`+instrumentation (§4.2's 2/17 plus 2 storms in the first 4
instrumented rounds of `logs/j4_campaign_astrace/`), a ~19% per-round rate —
0 storms in 32 rounds has a one-sided probability of about
(1 − 0.19)³² ≈ 0.001 under the old rate. The storm is gone at this
campaign's power, not merely rarer.

**The gate demonstrably fired in anger, five times, all in GREEN rounds:**
`[AS-FREE-DEFER]` once each in lane 1 rounds 8, 9, 13 and lane 2 rounds
25, 26 — a mix of `path=owner` and `path=last-view`, round 9's directly
adjacent to a `[KTG-HARD]` (the §4.1 straggler route, caught live). Every
defer was released by exactly one later `path=drained` free — no leaks. On
the unfixed kernel each of those five events would have been a poisoned
page table under a live `TTBR0`.

**Round 29's stall is a different, pre-existing defect.** Its auto-probe
(`lockprobe_lane2_round29.txt`) shows the BKL idle, `TALC`/`PMM`/COW/ext2
all free, and all four cores *executing* (PCs moving across samples) in
`smoltcp_net::poll` — the machine is alive, the in-guest build simply hung
at guest T81.7 s (last activity: a fork mid-flight). Only 2 `[BKL] stuck`
lines all round, zero `[AS-FREE-DEFER]`, no OOM/ENOSPC markers. This
matches the known open "build hang, cores idle, everything free" residual
(the rustc futex-hang class that predates this work) — the driver's
`SILENT_WEDGE` label is a timeout bucket, not a diagnosis; the real silent
wedge (§1's discriminator) requires `TALC`/`PMM` held, which they are not.

## Background

- [`PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md`](PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md) —
  the sibling defect (fixed), and why the two were confused.
- [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
  §12.7 — the first sighting of this storm, including two reading errors
  (`tag=511`, and the barge/lost-ticket theory) worth not repeating.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) —
  the BKL's FIFO ticket invariant; the lost-ticket bug fixed earlier the same day
  is a *different* cause of `[BKL] stuck` and shows `owner=0`.
- [`../reference/scripts/multi-vm.md`](../reference/scripts/multi-vm.md) —
  `lockprobe.py`, and the traps in reading its output.
