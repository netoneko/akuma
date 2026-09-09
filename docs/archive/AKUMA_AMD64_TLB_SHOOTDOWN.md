# amd64 TLB shootdown — `TlbTarget::AllCores` becomes true

**Date:** 2026-09-09
**Status:** landed. `cowstale` deterministic at `SMP=4` — 4 of 4 clean where
the baseline rate on this rig was **1 of 4**. Firecracker suite 512/0, bare
metal 516/0 (511/515 baselines + 1 new check each), host tests 1360/0, AArch64
`.text`/`.data` byte-identical against `HEAD`.

## The premise that died

Two comments justified having no shootdown, and both rested on "an address
space is only ever active on one core". `clone(CLONE_VM)` (2026-09-06) made
that false. From then on CoW `fork` demoted the **parent's live PTEs** with a
core-local `invlpg`, and a peer core holding a stale writable translation wrote
straight through a page it was supposed to fault on. That is not a model of the
bug; it is the measurement — `cowstale`'s `NO END MARKER` failure, which takes
the two probes after it down as `NOT REACHED`, so one flake reads as three.
Baseline rates measured 2026-09-09: 2/4 and 3/4 clean on the recorded trees,
**1/4 on this rig** — the flake was never rare, only irregular.

## The design, and the one question that mattered

The vocabulary already existed: `akuma_mmu::TlbTarget` and the `#[must_use]`
`TlbFlush` token whose `Drop` is the completion point. `AllCores` is two steps
on x86 — the core-local flush (`invlpg` / `CR3` reload) at the call, and a
shootdown IPI whose acknowledgement `TlbFlush::drop` waits for. Nothing above
`akuma-mmu` learned a new call.

**The IPI carries a generation counter and "flush everything"** — no per-VA
mailbox. A range past `FULL_FLUSH_THRESHOLD` (512 pages) already degrades to a
full flush in `akuma-mmu`, so the finer payload buys nothing this target needs
and would sit on the path that must never wedge. Receiver and assist are
idempotent, so servicing a generation twice (assist, then the delivered IPI)
is a redundant flush, not an error.

**The deadlock question (proposal §4) is answered by lock order, and it is
worth writing down because it is the whole design:**

> Every flush sender holds the BKL. The BKL is the outermost lock on this
> target, so while the sender waits, no peer can be inside kernel code holding
> any other lock. A peer is therefore one of: ring 3 (interrupts on), `hlt`
> (interrupts on — the IPI itself wakes it), an interrupt handler (bounded, and
> the handlers take no lock), or **spinning IRQ-masked in the BKL ticket wait**
> — the one state that cannot make progress on its own.

That last state is why `akuma-bkl`'s acquire loop now services pending
shootdowns inline (`set_spin_assist`, behind
`cfg(all(target_os = "none", target_arch = "x86_64"))` so the AArch64 build is
untouched): the sender publishes the generation in a per-CPU **mailbox before
sending**, so a core that cannot take the IPI can still flush and acknowledge
from inside the spin. A peer IRQ-masked on the address-space or regions lock
cannot happen — taking either requires the BKL the sender holds.

The trap this argument has: **a ring-3 page fault serviced without the BKL
would break it.** The fault path holds `ProcAddressSpace::lock` (which masks
IRQs) but pre-fix held no BKL, so a peer *with* the BKL could spin masked on
that same address-space lock while the lockless fault sender waited for its
acknowledgement — a genuine cycle. The fix is one bracket in
`page_fault_dispatch`: the demand-paging and CoW arms take the BKL for the
servicing window (`bkl_held` first — a fault from ring 0 already holds it, and
`enter_kernel` is reentrant by owner core). The invariant is now "every flush
sender holds the BKL", full stop.

The handler itself takes **no lock and no BKL** — a peer can be running ring 3
while the sender edits page tables; that is the point. Full flush = reload this
core's own `CR3` (non-global entries are every user entry; the kernel upper
half never changes after boot), mailbox store, `EOI`.

## What was wired where

- `amd64/src/shootdown.rs` (new) — vector **33** (32 is the timer, 0xFF
  spurious), per-core `PENDING`/`ACKED`/`SERVICED` mailboxes, generation
  counter, hand-assembled entry stub on the timer's pattern (`swapgs` on a
  ring-3 origin), `broadcast`/`wait_for_acks`, and the boot self-test. The
  `SERVICED` counters are permanent diagnostics, not test-only: the boot log
  answers "did the shootdown reach all four cores" without a debugger.
- `akuma-mmu` — hooks, not dependencies (`set_shootdown_hooks`, the
  `akuma_primitives::console` pattern), because the crate is shared with the
  AArch64 kernel. Before the hooks are armed, and on a one-core machine,
  `AllCores` degrades to the core-local flush it always was.
- `akuma-bkl` — `set_spin_assist` + one call in the ticket wait loop. The
  crate has `#![forbid(unsafe_code)]`, so the hook is a
  `OnceCopy<fn()>`, not an `AtomicUsize` + `transmute`.
- `lapic.rs` — `send_fixed` (delivery mode 000, vector in bits 7:0) and
  `ready()`; the handler is registered in `lapic::init`, which both boot
  protocols reach — the "registered in one `kmain` and not the other" trap.
  Hook *arming* lives in `boot::install_shared_sinks` for the same reason; it
  is safe there because `broadcast` answers `false` until the LAPIC is mapped
  and a second core is online.
- `x86_walk_leaves` — per-leaf `invlpg` stays core-local; **one** ranged
  broadcast per editing walk (Unmap/Reprotect/Remap), not one IPI per leaf.
- `flush_tlb_range_all_asid`'s x86 arm — previously **emitted nothing** below
  512 pages (`vaae1`/`vaae1is` are `cfg(aarch64)` bodies). Latent, not live —
  until the C1 mem fold installed it. It landed here first.

## Proof

- **The verdict probe**: `cowstale` 4/4 clean at `SMP=4` on Firecracker (real
  KVM) after 1/4 clean on the unmodified tree, same rig, same day.
- **The self-test**: BSP broadcasts one generation; every peer's `SERVICED`
  counter must move (`mask 0b1110` in the log). First box run "failed" this
  check because the check counted the **sender**, which correctly never IPIs
  itself — the test now requires every *peer*.
- **AArch64 unchanged, proven not assumed**: worktree build at `HEAD`,
  `llvm-objcopy` per section. `.text` 3 207 052 bytes identical, `.data`
  identical; `.rodata` differs at exactly two bytes — a `core::panic::Location`
  **line number** (1944 → 2067) for an assert in `akuma-mmu`, shifted by the
  inserted doc comments. Source-position metadata, not behaviour.
- Firecracker 4-vCPU suite **512/0**, bare metal **516/0** (baselines 511/515
  + the one new check), single-core suite green with the shootdown noting and
  skipping.

## Costs and things not done

- A CoW break now broadcasts one IPI per fault, and the fault path takes the
  BKL for its servicing window — both are correctness-first costs paid on the
  path `cowstale` measures. Nobody has measured the compile floor yet; the
  probe suite is the witness that it is survivable.
- The `[BKL] stuck` sampled diagnostic appeared once during the metal suite
  (owner/waiter lines, "0 failed" after) — contention chatter under the
  self-tests' fork load, seen before this change; noted here so the next
  reader does not pin it on the assist.
- The **wake IPI** (`smp.rs`'s other missing item: a core in `hlt` learns of
  new work at its next tick) is the same plumbing and a deliberately separate
  change. Do not fold it into anything.

## Background

- `proposals/NEXT_AGENT_AMD64_TLB_SHOOTDOWN.md` — the plan this executed,
  including the measured baseline table.
- `docs/archive/AKUMA_AMD64_COW.md` — why CoW fork was SMP=1-only, and the x86
  bit-9 marker.
- `docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md` § "The open issue" — the
  `cowstale` race as first recorded.
- `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §3 — where `TlbTarget` came
  from.
