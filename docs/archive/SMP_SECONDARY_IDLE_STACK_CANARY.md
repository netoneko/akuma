# devbox-smoltcp: three `[STACK-OVERFLOW]` reports every boot that never happened

**Status: FIXED, 2026-08-16.** `SMP=4` devbox-smoltcp boots with zero
`[STACK-OVERFLOW]` lines. The reports were false: the secondary cores' kernel
stacks had never had a canary painted on them.

This closes [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) **Issue 11**, which already
carried this diagnosis and named the fix taken here ("paint the canary in
`adopt_current_as_core_idle`… needs care that the painted words sit below
anything the boot/trampoline context has already pushed"). That issue is the
original analysis, including the A/B that confirmed the behaviour pre-existing at
`79a18cd`; this doc records the address-level reading, the fix, and the
verification.

## Symptom

Every `overlays/devbox/run-smoltcp.sh` boot (`SMP=4`), immediately after the
background polling loop started:

```
[Main] Network ready! Running background polling loop.
[Main] Starting herd supervisor...
[herd] Starting service: sshd
...
[STACK-OVERFLOW] tid=1 ran off its 64KB kernel stack (base=0x402b4090) — kernel memory below it was corrupted
[STACK-OVERFLOW] tid=2 ran off its 64KB kernel stack (base=0x402c4090) — kernel memory below it was corrupted
[STACK-OVERFLOW] tid=3 ran off its 64KB kernel stack (base=0x402d4090) — kernel memory below it was corrupted
```

Three cores, three lines, once per boot, never repeating. The box was otherwise
healthy — sshd came up, sessions worked.

## Reading the addresses

The three bases are the whole diagnosis:

| Evidence | Conclusion |
|---|---|
| Bases exactly `0x10000` apart | 64 KiB stride |
| `STACK_SHIFT = 16` in `src/smp_shared.rs` | the secondary boot stacks are 64 KiB — these are them |
| Every base ends `...4090` | the array is `.balign 16`, not 64 KiB-aligned, so all entries share low bits. Symbol at `0x402a4090`; slot [0] unused (the BSP runs on the boot stack) |
| Reported size 64 KB, but `SYSTEM_THREAD_STACK_SIZE` is 512 KB on release | these are **not** the pre-allocated PMM system stacks |
| tid = 1, 2, 3 | cores 1/2/3 adopt thread-pool slots 1/2/3 in `adopt_current_as_core_idle` |

So: the SMP secondary cores, running on the static `.bss` array
`secondary_boot_stacks_shared`.

## Root cause

`adopt_current_as_core_idle` (`crates/akuma-exec/src/threading/mod.rs`) registered
the stack in the pool:

```rust
pool.stacks[slot] = StackInfo::new(stack_base, stack_size);
```

…and **never painted the canary**. Every other stack-registration path does —
`allocate_stack_for_slot`, the boot stack in `ThreadPool::init`, the recycle path
in `cleanup_terminated`. These stacks are static `.bss`, not PMM-backed, so
nothing had painted them; the linker's `*(.bss .bss.*)` catch-all
(`linker.ld:78`) zeroes the section, so the canary words read `0x0` instead of
`0xDEAD_BEEF_CAFE_BABE` and `check_stack_canary` returned false the first time
anyone looked.

The first look is `report_overrun_stack_canaries()`, called from the background
polling loop at `src/main.rs:1161` — which is exactly where the lines appear.
The report is latched per slot by stack base, giving precisely one line per
secondary and no repeats.

## Why it mattered beyond the noise

The per-slot latch keeps a reported slot quiet forever, so those slots were
permanently "already reported" — a **real** overrun of a secondary's stack could
never have been announced. That is the tightest stack in the system:
`secondary_shared_start` points the core's exception stack at the same 64 KiB
(`exc_top = stack_base + stack_size`).

## Fix

Paint the canary where the stack is registered:

```rust
if config().enable_stack_canaries {
    init_stack_canary(stack_base);
}
```

Safe: the core entered on this stack with SP at its top, so the canary words at
the base sit a whole stack below any live frame.

Deliberately **not** paired with `fill_stack_sentinel` the way
`allocate_stack_for_slot` is — that paints `base..top`, which would clobber the
live SP of the very core executing the code. Secondary high-water measurement
would need a `paint_boot_stack`-style paint that stops short of the current SP.

Also corrected: the doc comment on `secondary_stack_base` said "16 KiB"; the
stacks are 64 KiB (`1 << STACK_SHIFT`).

## Verify

```bash
# Pick an INSTANCE whose ports are free (ssh 2222+100N, model 21434+100N):
lsof -nP -iTCP -sTCP:LISTEN
# snapshot=on still takes a shared WRITE lock, so a second VM cannot share a live
# devbox.img — clone it first (APFS clonefile: instant, no extra space):
cp -c devbox.img /tmp/devbox-test.img
INSTANCE=5 DEVBOX_DISK=/tmp/devbox-test.img ./overlays/devbox/run-smoltcp.sh
```

Expect the secondaries to come up on the same slots that used to report, and no
overflow lines at all:

```
[SMP-shared] core 1 online (idle tid 1)
[SMP-shared] core 2 online (idle tid 2)
[SMP-shared] core 3 online (idle tid 3)
[SMP-shared] ✓ 3 secondary core(s) online (shared kernel)
[Main] Network ready! Running background polling loop.

$ grep -ac STACK-OVERFLOW <log>
0
```

`grep -a` is required — QEMU emits a control byte that makes plain `grep` treat
the log as binary.

## Background

- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) **Issue 11** — the original analysis and
  the A/B that confirmed it pre-existing.
- [`EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`](EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md)
  — why this detector exists at all: a real 10 KB run-off landed in a user
  process's L3 page table and surfaced as an unrelated SIGSEGV.
- [`SMP_ADOPTED_IDLE_SLOT_CLOBBER.md`](SMP_ADOPTED_IDLE_SLOT_CLOBBER.md) — the
  second bug found in the same investigation, and the reason the boot suite's
  `test_stack_canary_overrun_is_reported` (`spurious == 0`) never caught this one.
- `docs/reference/subsystems/smp-shared.md` — shared-kernel SMP architecture.
