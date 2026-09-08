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
- `c_stress` memory probes: **regressed — see below.**

## The open issue: `cowstale` at SMP=4

Baseline is 8/10 with **0 unexpected**. With the feature on, `cowstale`
fails at `SMP=4` on **both** rigs; `SMP=1` passes both. Two failure shapes
observed, sometimes in the same run:

1. **Ring-3 `#PF` in a reader thread** — `err=0x4` (user, read, not-present),
   `rip=0x400ade`, which `objdump` resolves to `reader`'s first load
   `movq (%rax), %rcx` with `rax = g_map + (p << 12)`. Across runs
   `cr2` was `0x0` (p=0) and `0x1d000` (p=29) — **in both cases
   `g_map` itself read as zero** and `cr2 == p << 12`. The probe's own header
   had already printed `map=0x100000000`, so the global *was* correct when
   written.
2. **No end marker** — the probe never reports, on either rig.

A manual QEMU run (`INIT=/probes/run_all INITARGS=cowstale`, SMP=4) **passed**
— 1 996 304 reader checks, 0 faults — minutes after the harness run failed.
So it is timing-dependent, not deterministic on rig or probe count.

### What the symptom means

`g_map` lives in `.bss`. A thread that reads it as **zero through a present
mapping** (the `mov` of `g_map` itself did not fault — the fault is on the
`(%rax)` load it feeds) is reading bss from an address space where that VA is
backed by a **zeroed anon frame**, not the parent's data. That is the signature
of running against the wrong root (a fresh/child space whose region list
demand-pages zeros) or of a parent space whose bss got re-demand-paged over.
The probe was built to hold translations live on peer cores across a fork's
demote — the stale-translation condition — and its `map=` print proves the
value was right at start.

### Not the cause (checked)

- `akuma-bkl`'s `irq_save_mask`/`irq_restore` route through
  `akuma_primitives::irq` — real on x86 since piece 1; `enter_kernel`'s
  mask-wait is not a no-op here.
- `demote_range_to_ro`'s feature-gated `dsb_ish` is `mfence` on x86 — correct
  and harmless.
- `get_or_create_table_atomic`'s `free_page` now receives a real tid instead
  of 0 — a stats/debug argument only.
- The bare-`hlt` sleep (piece 4) is fixed; the failure survives the fix with a
  changed shape (both rigs hang now; earlier the TCG run `#PF`ed instead),
  which is consistent with the sleep having been one *consequence channel* of
  the race rather than its cause.
- Firecracker's other probe results are at baseline (8/10 with the same two
  `known` entries); the regression is specific to `cowstale`'s
  fork-demote-under-peer-traffic shape.

### Leading theories, in order

1. **Wrong root on a peer core.** A parent reader resumed against the child's
   (or a fresh) root whose bss demand-pages as zeros. Test: print `CR3` and
   `current_task` in amd64's `#PF` kill path and compare against the fork
   child's root (`mm.rs` fork builds it; a boot-log print during the probe
   run names both).
2. **Region-list surgery corrupting the parent's regions during fork**, so a
   later parent fault re-demand-pages bss as zeros — same print, plus a dump
   of the parent's region list at fault time.
3. A `x86_yield`/switch-path behaviour change under the feature
   (`akuma-threading`'s feature-gated arms) publishing a task before its
   `space_root`.

### Next steps

- Add `CR3` + `current_task` to the `#PF` kill diagnostic in `amd64/src/idt.rs`
  (one line each; the value is `paging::active_root()`), rerun
  `amd64_mem_trials.py --only cowstale --smp 4` until the `#PF` shape
  reproduces, and compare the faulting root against the child's.
- If the root is the parent's, dump the parent's region list — theory 2.
- A/B `x86_yield`'s feature arms if neither lands.

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
