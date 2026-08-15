# Extracting `akuma-primitives`: the missing leaf crate

**Date:** 2026-08-13
**Scope:** Phase 4 of [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
(§5.55 / §5.555). New crate `crates/akuma-primitives`; changes across the bin
crate, `akuma-exec`, `akuma-net`, `akuma-ext2` and `akuma-virtio`.
**Current-state doc:** [`../reference/subsystems/primitives.md`](../reference/subsystems/primitives.md)
— read that if you want to *use* the crate. This one is why it looks the way it does.
**Commits:** `13f5263` (rungs 1–2), `069f1f0` (rungs 3–6).

---

## 1. The diagnosis this started from

§5.55 of the duplication survey named a single cause for most duplicated
primitives in the tree:

> **Most of these copies exist because the canonical version lives in a crate the
> duplicator cannot depend on.** The bin crate owns `console::StackWriter`, and
> `src/main.rs:1656` even carries a comment telling you to use it rather than
> hand-rolling a local buffer. `akuma-exec` cannot — depending on the bin crate
> is a cycle — so it grew **three** of its own. This is not carelessness; it is
> a missing crate.

It also named the blocker, and told whoever picked it up where to start:

> `PreemptGuard` … **Tier A alone does not achieve that**: ext2 needs `OnceCopy`
> *and* `PreemptGuard`, and `PreemptGuard` is Tier B. The guard is the long pole
> for the whole untangling, so if this is ever picked up, start by deciding what
> happens to the thread-slot table — not by moving the easy four.

That instruction was correct and produced an answer that sounds like a
contradiction: **the table can move — but only after three smaller things move
first, and those three turned out to hold most of the actual duplication.**

## 2. Why the table looked immovable, and why it wasn't

`PreemptGuard::new()` calls `threading::disable_preemption()`, which is not a
standalone counter: it indexes `PREEMPTION_DISABLED[tid]` by `TPIDRRO_EL0` and
maintains two diagnostic arrays beside it. §5.55 called that scheduler state, and
it is.

But the things it actually needed from outside `core` were only three, and each is
a primitive in its own right:

| Need | Where it is used | Rung |
|---|---|---|
| a console | `get_current_thread_register`'s `[FATAL] TPIDRRO_EL0 CORRUPT` halt; the watchdog's two lines | 2 |
| IRQ masking | the guard's `no-bkl-*` half | 3 |
| a clock | **one** diagnostic timestamp on the 0→1 transition | 4 |

So the ordering fell out: build the console, the DAIF module and the clock in the
leaf crate first, and the table becomes movable without inventing anything.

**None of it reintroduces the callback that was deliberately removed.**
`akuma-exec`'s `sync.rs` records why `PreemptGuard` stopped dispatching through a
registered function pointer: a direct call works during early boot and in host
tests, a registered callback does not. That reasoning is about *the guard's own
operation*, which must be correct before anything is registered. A print sink and
a clock are neither — and the clock read it replaces **already** degraded, as
`if runtime::is_registered() { (runtime().uptime_us)() } else { 0 }`.

### The seam: reading `TPIDRRO_EL0` moves, writing it does not

`current_tid()` is a bounds-checked `mrs` with nothing behind it. The write,
`set_current_thread_register`, also re-points the per-core BKL attribution cache
(`load_thread_tag_to_core`) — genuinely the scheduler's business. Splitting the
pair at read-vs-write is what let the counters move while the scheduler kept the
thing that is actually scheduler state.

## 3. The six rungs

| Rung | Contents | Cuts |
|---|---|---|
| 1 | `OnceCopy<T>` | — |
| 2 | console hook, `StackWriter`, `FmtBuf`, `safe_print!` | — |
| 3 | all DAIF: `IrqGuard`, `irq_save_mask`/`irq_restore`, `unmask_irqs{,_sync}`, `mask_irqs_sync`, `read_daif` | — |
| 4 | the clock hook | — |
| 5 | `MAX_THREADS`, `current_tid`, `PREEMPTION_DISABLED*`, `PreemptGuard` | **`akuma-ext2`** |
| 6 | identity `virt_to_phys`/`phys_to_virt` + the `DEV_*_VA` window | **`akuma-virtio`**, and via it **`akuma-net`** |

## 4. Two places the plan was wrong

### 4.1 `cargo tree`, not the import list

Rungs 1–2 were reported with the claim that rung 5 would free *two* crates. It
frees **one**. The measurement behind the claim was a grep of `akuma_exec`
references:

- `akuma-net`: one line — `pub use akuma_exec::sync::PreemptGuard;`
- `akuma-ext2`: three references to two symbols

Both accurate, and the conclusion was still wrong, because `akuma-net` depends
unconditionally on `akuma-virtio`, which depended on `akuma-exec`. The edge
`akuma-net → akuma-virtio → akuma-exec` survives deleting that `pub use`
entirely.

**Import lists measure coupling; only the dependency graph measures what gets
compiled** — and compile cost was the whole point of §5.55. That is what added
rung 6. Check with `cargo tree -p <crate> --edges normal`, and note that a
`grep -l akuma-exec crates/*/Cargo.toml` still hits three crates *whose comments
mention it* after the edges are gone.

### 4.2 "How many DAIF implementations?" — 3 was the wrong count

Rung 3 was scoped as "merge the three save/mask/restore implementations":

| copy | `isb` after the mask? |
|---|---|
| `src/irq.rs:12` `IrqGuard` | yes |
| `akuma-exec/src/runtime.rs:280` `IrqGuard` (**same name, second crate**) | yes |
| `akuma-exec/src/sync.rs:17` `irq_save_mask`/`irq_restore` | **no** |

Counting every DAIF access instead of every *guard* found **nine more sites** —
unconditional mask/unmask with no saved state, spread over six files, plus one
bare `mrs daif` read. Two facts came out of that count:

- `src/irq.rs`'s `disable_irqs()` and `enable_irqs()` had **zero callers**. The
  only greps were their own doc comments and two comments elsewhere referring to
  them. `dead_code = "deny"` is workspace-wide; they survived only by being `pub`
  in a bin crate.
- **Two** of the six open-coded `msr daifclr` sites were in `akuma-exec`
  (`process/mod.rs:1803`, `:1942`), which cannot call the bin crate's
  `enable_irqs()`. The §5.55 shape again, for the third time in this phase.

So rung 3 grew to cover all of it. Everything except one site now routes through
`akuma_primitives::irq`; the exception is `src/exceptions.rs`'s vector-install
block, where the surrounding `msr vbar_el1` / `isb` has to stay one asm unit.

## 5. Judgement calls, and what was deliberately *not* resolved

### The `isb` divergence is preserved, not fixed

Two of the three guard implementations emitted an `isb` after masking; the third
did not. The obvious "cleanup" is to pick one. That was declined, because both
directions are a behaviour change on a hot path:

- **Dropping the `isb`** is very likely correct — AArch64 masks interrupts
  synchronously on a direct PSTATE write, and Linux's arm64
  `local_irq_disable()` is a bare `msr daifset` — and `irq_save_mask` has run
  without one on the contended `KernelLock::acquire` path under real SMP for a
  long time. But "very likely" is not a measurement, and the failure mode is a
  rare lost-window bug in the exception path.
- **Adding it to `irq_save_mask`** is the conservative direction, and it puts a
  pipeline flush on the BKL acquire path and inside every `PreemptGuard` — hot
  enough that Phase 3 deleted a *spinlocked struct read* from a comparable path
  for cost.

So: one implementation, layered. `irq_save_mask`/`irq_restore` are the bare DAIF
accesses; `IrqGuard` is `irq_save_mask()` **plus** an `isb`. Every call site's
codegen is byte-identical to before, and the choice is left to whoever measures
it. `unmask_irqs` vs `unmask_irqs_sync` follows the same principle rather than
flattening a barrier away.

`#[inline(always)]` on all of them is load-bearing, not a hint: they replace
open-coded `asm!`, so the extraction is only behaviour-preserving with no call
overhead.

### The device-VA table is the weakest part of the design

`DEV_*_VA` is the L0[1] device mapping, which is genuinely `mmu`'s business, and
only `DEV_VIRTIO_VA` is needed outside `akuma-exec`. Moving the whole table into a
crate called "primitives" is defensible but not obviously right.

It moved as one table rather than one constant because splitting a fixed layout
across two crates is how layouts drift. The alternative — passing the virtio base
in as a parameter — was rejected because Phase 3 had just *removed* the
`mmio_addrs` parameter from `akuma_net::init` / `smoltcp_net::init` /
`rump_tap::init` on the grounds that every caller passed the same table.

### The translators must never become hooks

`virt_to_phys`/`phys_to_virt` are the identity. `akuma-net` once reached the
kernel's versions through `NetRuntime` function pointers *specifically* to avoid
depending on `akuma-exec`, and Phase 3 deleted that indirection for costing a
spinlocked struct read on the per-packet DMA path to reach two identity
functions. Moving them relocates an assumption that was already baked into
`#[inline(always)] { vaddr }`; it does not introduce one. If the kernel ever gains
a non-identity kernel map, the options are a compile-time offset or a
caller-passed translation — not a registered pointer.

## 6. The dangerous part: a silent feature-forwarding failure

`PreemptGuard`'s whole body is behind `#[cfg(kernel_smp_shared)]`, emitted by
**this crate's own** `build.rs` from **its own** forwarded `smp-shared` feature.
If the chain from the bin crate ever fails to reach
`akuma-primitives/smp-shared`, the guard compiles to a zero-sized no-op. Nothing
fails to build. Nothing warns. Every inner-spinlock critical section in the
kernel quietly stops being protected from preemption, and the symptom is a rare
SMP corruption or wedge somewhere else entirely.

This is the same dormant-`cfg` class the tree has been bitten by before:
`akuma-exec` once shipped without a `build.rs` at all, leaving the demand-paged
ELF loader, the page-by-page interpreter loader and `HEAP_SLURP_MAX = 0` silently
inactive on the size profile. That autopsy lives in `crates/akuma-exec/build.rs`'s
own opening comment, which is worth reading before adding any `cfg` to a crate in
this tree.

Three mitigations, all in place:

1. **Forwarded twice.** The bin crate's `Cargo.toml` names
   `akuma-primitives/{smp-shared,no-bkl-network,no-bkl-vfs,extreme}` directly, in
   addition to `akuma-exec` forwarding them. Relying on cargo's feature
   unification through a third crate means a graph without that crate
   (`cargo test -p akuma-ext2`) silently gets the no-op. `akuma-ext2` forwards
   `no-bkl-vfs` for the same reason.
2. **A boot self-test.** `test_preempt_guard_is_live` (`src/process_tests.rs`)
   asserts the guard is non-zero-sized, that nesting is counted (an inner guard
   dropping must not re-enable preemption under an outer one), and that
   `MAX_THREADS` matches the profile and agrees with `akuma-exec`'s re-export.
3. **A named failure signature.** A healthy boot prints
   `live=true counts 0->1/2->0 held=true size=16 max_threads=256`. `size=16` is
   `bool` + saved `u64` DAIF; **`size=0` means the forwarding broke.**

## 7. The other silent failure: early-boot console

The `akuma-exec` writers called `(runtime().print_str)(s)` directly, which
*panics* if unregistered. The shared `print_str` is a no-op instead — strictly
safer, except in one direction.

`akuma_exec::init` is at `src/main.rs` ~`:760`. Everything from the kernel's Rust
entry (`rust_start`, `:151`) to there prints: DTB scan, memory detection, MMU and
heap bring-up, the layout assertions. Registering the hook only in
`runtime::register` would have **silently swallowed all of it** — no panic, no
warning, just a quieter boot log that nobody diffs.

So `rust_start` installs it as its **first statement**, before any output at all.
`console::print` needs no initialisation (a const MMIO base and a volatile
store), so there is nothing to order it after, and `OnceCopy::set` ignores the
later duplicate from `init`.

Verified by reading the boot log rather than reasoning about it:
`Akuma Kernel starting…`, `Kernel binary: …`,
`WARNING: Kernel is within 4MB of stack!` and the whole
`=== Memory Layout ===` block are all present, and all pre-`init`.

## 8. What was duplicated, measured

### Console: five writers *and three macros*

§5.5 counted four stack writers; §5.55 corrected it to five. Both undercounted,
because they counted writers and the duplication was in the macro on top:

| copy | sink |
|---|---|
| `src/console.rs:251` `safe_print!` + `:205` `StackWriter<N>` | `console::print` |
| `akuma-exec` `threading/mod.rs:68` `safe_print!` + `:35` `StackWriter<N>` | `runtime().print_str` |
| `akuma-virtio` `print.rs:24` `vprint!` | `runtime().print_str`, guarded |
| `akuma-exec` `process/mod.rs:89` `FmtBuf<'a>` | caller's buffer |
| `akuma-exec` `process/children.rs:1039` `LazyDebugWriter<N>` | `runtime().print_str` |
| `akuma-exec` `mmu/mod.rs:340` `Buf<'a>` (function-local) | `runtime().print_str` |

Two `StackWriter<N>`s **under the same name in different crates** — which is how
the second stayed hidden, exactly as §5.55 predicted for that pair.

`akuma-virtio/src/print.rs` deserves quoting, because it is the §5.55 diagnosis
written by the duplicate itself: *"A library crate cannot reach that macro, and
the obvious substitute — `log::info!` … is not one."* It then reproduces
`safe_print!`'s contract in full.

**Result: five writers → two shapes** (`StackWriter<N>` owns, `FmtBuf<'a>`
borrows — kept because `[PSTATS]`'s top-N line builds two side by side in one
frame), **three macros → one.** `tprint!` stays in the bin crate; its
`[T<secs>.<cs>]` stamp comes from `crate::timer::uptime_us()`.

### CPD scores this work at 6%, and that is the finding

Whole-tree PMD CPD at 50 tokens, `git worktree` at `HEAD` vs the working tree —
a controlled A/B, not a recollection:

| | blocks | duplicated lines |
|---|---:|---:|
| before | 434 | 4,856 |
| after | **433** | **4,848** |

**One block. Eight lines.** That block is the `flush()` body shared by
`threading::StackWriter` and `LazyDebugWriter` — the only two of the six copies
byte-identical anywhere. The other ~120 lines differ by type name, field name,
and `N` versus `self.buf.len()`, so CPD never saw them.

§1 and §6 of the survey said CPD is Type-1-only for Rust; this turns that caveat
into a measured ratio. **Do not report a CPD delta as the result of Type-2 work,
and do not expect the §9 baseline to move.** Count definitions collapsed and
dependency edges cut.

### Line counts

Rung 2, on its own: the 14 touched files went **9,943 → 9,721 (−222)** code lines
(non-blank, non-comment); the new crate added **127** non-test lines. **Net −95**,
plus **113 lines of tests the five writers never had.**

That is the third consecutive over-estimate in this document family — §3's ELF
figures were 3× optimistic for Phase 2a and 1.6× for 2b — and one step further:
**when the point of the work is to build a seam, the seam is most of what you
"save".** §5.5's "≈ −180 lines" for Phase 4 was never the right target.

## 9. What it bought

Nothing depends on `akuma-exec` except the kernel:

| Crate | `akuma-exec` edge before | now |
|---|---|---|
| `akuma-ext2` | direct (`OnceCopy` + `PreemptGuard`) | **gone** — deps are `akuma-vfs` + `spinning_top` |
| `akuma-virtio` | direct (`PreemptGuard`, `mmu` ×3) | **gone** |
| `akuma-net` | direct (1 `pub use`) **+ transitive via `akuma-virtio`** | **gone** |

And "depends on the execution crate" means something again.

## 10. Verification

`cargo clippy` warning-clean on all four build targets: `--release`,
`extreme-size`, devbox-smoltcp, devbox-rump. **437 host tests green** (414 before
the phase; +28 in `akuma-primitives`, −5 as `OnceCopy`'s moved with it).

QEMU `MEMORY=2048`, `--release`: 94 `[PASS]`, failure set identical to a clean
tree (`retired_reclaim_ab` only — the known-bad threshold documented in
[`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
§8.5 Phase 0), and `test_preempt_guard_is_live PASSED` with `size=16`.

Every rewired console sink confirmed live in one boot — which matters, because
Phase 3 had already shown how quietly console output can vanish:

| path | evidence |
|---|---|
| bin crate, pre-`init` | `Akuma Kernel starting…` + `=== Memory Layout ===` |
| bin crate `tprint!` | 122 `[T<n>.<n>]` lines |
| `akuma-exec` `threading` | 631 `[Cleanup]` lines |
| `as_trace` → `print_args::<160>` | 348 `[AS-NEW]`/`[AS-FREE]`/`[AS-EXEC]`/`[AS-DEFER]` |
| `FmtBuf` (two into one buffer) | the `[PSTATS]` top-N syscall lines |
| `akuma-virtio` (was `vprint!`) | `[RNG] Found virtio-rng at slot 2`, `[Block] Capacity: 3072 MB`, `[SND]` |

That last row is the one Phase 3 warned about: following `akuma-net`'s `log::`
pattern would have deleted those lines silently, since every crate pins `log`
with `max_level_off` and no logger is ever registered.

## 11. Loose ends

- **`src/console.rs`'s `print_dec` (`:160`) / `print_u64` (`:180`)** are a real
  21-line / 82-token **Type-1** clone — CPD has always reported it — differing
  only in `usize` vs `u64`. Untouched here because it is unrelated to the writer
  cluster. Phase 6 one-liner.
- **`akuma-vfs` does not depend on `akuma-primitives`** and has no need to today.
  If it ever wants `OnceCopy`, that is a free edge.
- **The `isb` divergence** (§5) is open by choice, and wants a measurement rather
  than a decision.
- **The rest of Phase 4's unblocked half** is untouched: `impl_display!` for the
  three `akuma-virtio` error enums (now intra-crate, since Phase 3 collected
  them), the four intra-crate BKL guards in `src/syscall/`, the twice-defined
  `MultiPollFuture` in `src/tests.rs`, and `ClientMem`/`NoMem` across
  `src/rump_proxy.rs` and `akuma-rump` (which needs `ClientMem`'s home settled,
  not this crate).

## Background

- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  — the survey. §5.5 (trait-impl clusters), §5.55 (the missing-crate diagnosis),
  §5.555 (this work's running record), §8.5 Phase 4.
- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — the `unsafe` census; the DAIF and
  virtio findings overlap.
- [`../reference/subsystems/primitives.md`](../reference/subsystems/primitives.md)
  — current-state reference for the crate.
- [`../reference/subsystems/console.md`](../reference/subsystems/console.md)
  § "Printing rules" — the no-alloc console rule these writers all serve.
- [`ALLOC_PRINT_AUDIT.md`](ALLOC_PRINT_AUDIT.md) — the violations that motivated
  that rule.
