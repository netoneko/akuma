# amd64 `[TLB] stuck` with a silent core: the address-space lock (2026-10-01)

**Status: fixed in the tree, root cause inferred by elimination, NOT verified on the metal.**

## Symptom

Photographed on the HP box, SMP=4: `[BKL] stuck: owner=2 waiter=3/1 tag=11 ... (aff0+1)`
alternating with `[TLB] stuck: 1 peer(s) unacked, generation=5356736 sender=1 missing=0x8`,
generation never moving. Only the tail of the log was visible, so no fatal dump was seen.

## Reading it

- `owner=` is aff0+1: `owner=2` is **core 1**, which is also `sender=1`. The sender holds
  the BKL, as `broadcast`'s contract requires. `waiter=3/1` are cores 2 and 0.
- `missing=0x8` is **core 3**: not the BKL owner, and not a visible waiter (waiter lines are
  deduplicated, `folded=8`). A core in the "fifth state": IRQ-masked, outside the BKL, not
  servicing shootdowns. Earlier members of the family: the xHCI poll (09-25) and the
  `#DB` dead core (`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`).

## Root cause

`akuma-mmu`'s deadlock argument says the BKL is the outermost lock, so no peer holds another
lock while a sender waits. The BKL carve-outs break that on amd64 (`smp-shared` enables
`no-bkl-process`, `-mm`, `-vfs`, `-drivers`).

- `munmap` (`akuma-syscalls-glue/src/mem.rs`) calls `flush_tlb_range_all_asid` **inside**
  `proc.with_address_space`: BKL + the process's `as_lock` held across `wait_for_acks`.
- `fork`'s share pass (`usermode.rs`, `akuma-exec` `fork_process`) takes the same owner's
  `as_lock` with the BKL **dropped**. `ProcAddressSpace::lock` masked IRQs and then did a
  plain `spinning_top` `lock()` — no IPI, no assist, not a BKL waiter. It could never
  acknowledge the sender that held the lock it was waiting for.

Second, related hole: fork's BKL-free window broadcasts itself (`process/mod.rs` per-chunk
`flush_tlb_range_all_asid` under `as_lock`, and the closing `flush_tlb_all`), violating
"senders hold the BKL". `wait_for_acks` never serviced its own mailbox, so two IRQ-masked
senders would wait on each other forever.

Not verified: that core 3 was in fork's share pass. Candidates ruled out: sender without the
BKL, `acquire_no_ticket` (never called on amd64), a lost-ack race in `service_pending` (every
caller is IRQ-masked).

## Fix

1. `akuma_bkl::sync::masked_spin_assist()` — public, always defined, no-op off x86 bare metal.
2. `ProcAddressSpace::lock_inner()` — `try_lock` loop calling it; every `inner.lock()` in
   `address_space.rs` (the guard and the one-shot passthroughs) goes through it.
3. `shootdown::wait_for_acks` calls `service_pending()` each spin (own slot only).

Cost: one relaxed load per spin of a contended `as_lock`; none uncontended.

## Rule

Any IRQ-masked spin on an inner lock that a shootdown sender can hold needs the assist.
Not audited: BKL-free spins on locks a sender does not hold across the flush (VFS mount
table, PMM, fpcache) — they cannot form this cycle.

## Verify

`cargo build -p akuma-amd64 --target x86_64-unknown-none --release --offline`; host tests for
`akuma-bkl`/`akuma-exec`. On the metal: the `-j4` loop (`scripts/benchmarks/amd64_metal_j4_loop.sh`)
shows no `[TLB] stuck`. If one recurs, `missing=` naming a non-waiter core is still the
signature — photograph the **top** of the screen for a fatal dump before concluding anything.
