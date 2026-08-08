# The `[BKL] stuck` storm: a page table freed under a running core

**Date:** 2026-08-08. **Branch:** `stabilize-devbox`. **Kernel:** `159c3db`
(`release-smp-shared` + `devbox-smoltcp,no-tests`), SMP=4, MEMORY=14336, HVF,
in-guest `cargo build -j4`. **Status:** root-caused, **NOT fixed**.

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

So this storm is **unaffected** by the allocator fix — as expected — and with the
silent wedge eliminated it is now the **dominant remaining failure** of in-guest
`-j4` builds, at roughly 1 round in 12.

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

### 6.1 Harness note

This storm is a **slow burn** — roughly 20 `[BKL] stuck` lines/second, ~18k over
the round. A watchdog that flags "more than N stuck lines in a 20 s window" can
sit just under the threshold and, because the console *is* still growing, keep
reporting the VM as healthy. The round then burns its entire budget. Detect it by
`owner=` being pinned to one value across thousands of lines, not by console
growth or by rate alone.

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
