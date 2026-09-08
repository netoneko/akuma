# amd64 C1: unblocking `kernel_smp-shared` — landed pieces, and the open `cowstale` regression

**Status 2026-09-08, mid-5b.** The `smp-shared` feature flip for the amd64
target was chosen as its own landing before the process-table step
(`proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` § "Order" step 3). Most of it is
landed and verified; **one memory-probe regression is open** — § "The open
issue" below is the part to read if you are picking this up.

## What landed

### 1. x86 `daif` arm in `akuma-cpu`

`mask_irq`/`unmask_irq`/`restore`/`read`/`mask_irq_sync`/`unmask_irq_sync` are
real on `x86_64-unknown-none` (`cli`/`sti`/`pushfq`-`popfq`). Every `IrqGuard`
and `akuma_primitives::irq::*` on this target was a silent no-op before; they
are real masks now. Two documented divergences:

- **Polarity is not normalised.** AArch64's `DAIF.I` is set-when-masked; x86's
  `RFLAGS.IF` is set-when-enabled. `daif::read` returns the raw register on
  each — a normalised read could not round-trip through `restore` — so callers
  that *inspect* the value must know their architecture. Callers that only
  save/mask/restore (every `IrqGuard`) need to know nothing.
- `unmask_irq_sync` is `sti; nop`: bare `sti` has a one-instruction window in
  which interrupts stay blocked, and the `nop` closes it — the x86 counterpart
  of the AArch64 arm's `isb`. `mask_irq_sync` is plain `cli` (`cli` blocks
  delivery from the next instruction architecturally; nothing to add).

`paging.rs`'s private `pushfq`/`cli` pair and `exec_runtime.rs`'s private
`cli`/`sti` hooks migrated onto the crate; their "silent no-op" warnings are
retired. `amd64/Cargo.toml` gains a direct `akuma-cpu` dependency.

### 2. x86 per-core identity

`akuma_primitives::cpu::current_core_id` under the feature read
`akuma_cpu::sysreg::mpidr_el1()`, whose x86 stub returns 0 — the flip would
have compiled clean and put **four cores on core 0**: the exact trap
`AKUMA_SELF_HOSTING_AMD64.md` § "Traps" #2 warns about, now live. Same for
`preempt::current_tid` (a stated 0), which under the feature keys
`PREEMPTION_DISABLED` and `akuma-bkl`'s dropped-window ledger — four cores
sharing one counter.

Fix: `akuma_cpu::percpu` (`core_id()` = `gs:[24]`, `current_task()` =
`gs:[32]`) reading amd64's `PerCpu.index` / `PerCpu.current_task`. The
cross-crate contract is enforced by `amd64/src/smp.rs`'s `OFFSETS_PINNED`
(extended with `index==24`, `current_task==32`); the writer side's assert is
the only enforcement `akuma-cpu` gets, and the module header says so. Both
fields were already maintained per core (`install_percpu`, the context
switch). `current_core_id`/`current_tid` gain real x86 arms **gated on
`kernel_smp_shared`** — off-feature builds keep the 0 shims and their exact
behaviour. Before-`install_percpu` callers must not ask (a `gs:` load through
a zero base faults); `ap_entry64` → `activate_unpublished` already takes the
core as an argument for exactly this reason (5a's finding).

### 3. BKL unification: `akuma_bkl` is *the* amd64 BKL

Flipping the feature would have activated `akuma_bkl`'s `KernelLock` alongside
`amd64/src/smp.rs`'s private ticket lock — two locks answering one question
(`idle_halt`, `blocking_relax`, `akuma_elf::interp` taking the crate's; every
ring-3 boundary taking the private one). The private lock is deleted;
`bkl_enter`/`bkl_leave`/`bkl_abandon`/`bkl_drop_window`/`bkl_run_unlocked`
forward to `enter_kernel`/`leave_kernel`. The old lock's discipline maps
exactly:

- **Depth was vestigial.** The old lock tracked per-core recursion depth and
  transferred it between tasks on a switch (`Machine::bkl_depth`,
  `hook_transfer_lock_depth`). Site-by-site: every `bkl_enter` arrives *from
  ring 3* and every `bkl_leave` goes *to* it — depth > 1 never existed; the
  depth simulated "a thread born in kernel code holds without entering", which
  `KernelLock`'s reentrant-by-owner model expresses directly. The hook is a
  no-op now; the `Machine` field is gone.
- **`reconcile_for_spsr` has no amd64 counterpart and needs none.** AArch64's
  eret epilogue infers EL from SPSR because its boundary is one shared
  instruction; amd64's boundaries are explicit calls. Carried divergence, not
  a gap.
- **The stuck diagnostic is the crate's** (`log_kernel_lock_stuck`, plus a
  lost-ticket self-heal the private lock never had). Message text moved from
  `[BKL] stuck: cpu N waiting on owner M` to
  `[BKL] stuck: owner=N waiter=M tag=… (aff0+1)` — greps keyed on the old
  wording move to `stuck: owner=`; the `[BKL] stuck` prefix the runbooks use
  is unchanged.
- **`smp-shared` is now a required amd64 feature** (in `default`; a
  `compile_error`-class const assert in `smp.rs` refuses a build without it,
  because the crate's entry points are no-ops there and four cores would run
  with no lock at all). amd64's `build.rs` forwards the feature to
  `cfg(kernel_smp_shared)` for exactly this gate.

### 4. `park::wfi` semantics repair (the first `cowstale` bug)

The x86 `wfi` arm was a bare `hlt`. AArch64 `wfi` wakes on a *pending*
interrupt even when `DAIF.I` masks it; `hlt` with `IF=0` sleeps until an NMI.
amd64 kernel code runs `IF` clear, so the first `blocking_relax_net` →
`idle_halt` park under the feature slept forever. The arm is `sti; hlt; cli`
now — `sti` takes effect after the following instruction, so the sequence is
atomic with respect to the tick — and **returns with interrupts masked**,
documented at the arm. `sched.rs`'s idle loop had always used this exact
idiom by hand; the crate's other callers ran bare `hlt`.

## Verification so far

- Boot suites with the feature on: QEMU/TCG **516/0**, Firecracker **503/0**.
- Host tests **1360/0**; clippy clean on the amd64 target.
- AArch64 `.text`/`.data` **byte-identical**; `.rodata` differs by **10 bytes,
  all the same change**: panic `Location` line numbers in
  `akuma-primitives/src/preempt.rs` moving 132 → 164, exactly the 32 lines the
  x86 `current_tid` arm added above them — the same benign shape 5a recorded
  for a one-byte `.rodata` diff.
- `c_stress` memory probes: **`cowstale` flaky at SMP=4 — resolved below as a
  pre-existing amd64 race, not the flip.** Full run after the A/B:
  Firecracker **8/10, 0 unexpected (exactly baseline)**; QEMU/TCG hit the
  pre-existing `cowstale` hang in this sample (cascading NOT-REACHED), which
  the same A/B shows the pre-flip kernel does too.

## The open issue: `cowstale` at SMP=4 — **resolved as pre-existing, not the flip**

Baseline is 8/10 with **0 unexpected**. With the feature on, `cowstale` fails at
`SMP=4` on both rigs (hang, or a ring-3 `#PF` in a reader thread); `SMP=1`
passes both. **A/B on the box (KVM, five `cowstale` runs each) settles cause:**

| kernel | result |
|---|---|
| pre-flip (`1ff515cc amd64 elf`) | 4 PASS, **1 HANG** |
| branch tip (feature on) | 3 PASS, **2 HANG** |

The pre-flip kernel fails in the same shape at a comparable rate. The race is
**pre-existing on amd64**; the feature flip only changed the timing (and
whether TCG manifested it as a hang or a `#PF`). It is *exposed*, not *caused*
— and the boot suites are unaffected (503/0 across the flip).

### The mechanism, as evidenced

1. The `#PF` diagnostic (added to `user_fault`: `cr3`/`task`/`pid`) showed the
   faulting reader running against **the parent's own root, the parent's own
   pid** — the wrong-root theory is dead. Two readers faulting near-simultaneously
   is expected once their shared address space reads zeros.
2. `objdump` of the probe resolves `rip=0x400ade` to `reader`'s first load,
   `rax = g_map + (p << 12)`; `cr2` values `0x0` and `0x1d000` both decode as
   **`g_map` itself reading as zero**, `cr2 == p << 12`. The probe's header had
   already printed `map=0x100000000`, so the value was correct when written.
3. `g_map` lives in `.bss`. Reading it as **zero through a present mapping**
   means the translation resolves to a frame whose *current* contents are
   zeros — a freed frame PMM has re-zeroed.

That is a **use-after-free read through a stale peer-core TLB entry**:
`CLONE_VM` threads make the parent's address space active on several cores at
once; a fork demotes bss, a parent write-fault CoW-copies a page, and the old
frame is freed when the child drops its ref. A reader on another core still
holds the pre-demote `RO` translation — x86 has **no TLB shootdown**
(`amd64/src/smp.rs`'s own header: "complete here because an address space is
only ever active on the core running its one task"), and that invariant is
exactly what `pthread_create` breaks. When PMM hands the freed frame out
`alloc_page_zeroed`, the stale translation reads zeros. The fork self-tests
(all pass, consistently, `SMP=4`) have no threads, which is why only this
probe lands on it.

### The fix, and its scope

Peer-core invalidation for address spaces active on more than one core — an
IPI shootdown keyed off the per-core L0 registry (`akuma_mmu`'s
`publish_l0_begin` bookkeeping already names which cores are on which root),
or at minimum no freeing of a CoW frame until every core that could hold a
translation for the space has invalidated. That is its own step (the AArch64
kernel has the ASID machinery; x86 has none); it should land **before** 5c
leans on threads, and the probe is the regression test for it.

### Judging the mem suite until then

`cowstale` at `SMP=4` on KVM is a coin flip on *any* amd64 kernel of this
vintage. A 5b verification run should score `cowstale` against the pre-flip
rate (hang ≈ 1-in-3..5, `#PF` shape on TCG) rather than against 8/10, and
treat a *new* failure shape or a different probe regressing as the real
signal.

## Background

- `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` — the 5b hand-off this work
  serves; § "The decision 5b cannot defer" is what commissioned it.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § "Traps" — trap 2 is the
  four-cores-on-core-0 identity trap this flip would have tripped silently.
- `docs/archive/COWSTALE_STALE_WRITE_FAULT_FIXED.md` — the probe's origin; its
  "stale translation on a peer core" condition is exactly what a wrong-root or
  demote-flush bug here would produce.
- `docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md` — the `.rodata`
  line-number-diff precedent and the `ProcAddressSpace` lock discipline this
  flip makes IRQ-masked.
