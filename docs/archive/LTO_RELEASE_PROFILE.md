# Should `[profile.release]` set `lto`? (started 2026-08-14)

> **Status: IN PROGRESS — measured, verified correct, decision not made.** Three
> arms built and sized (§3); **`lto = "fat"` passes the full gate clean at SMP=1 and
> SMP=4** (§5.1). What is missing is the *speed* measurement that would justify it
> and the *self-host* measurement that could rule it out (§5.2–5.3) — i.e. both
> halves of the actual decision. Handed off from a session that ran out of budget.
>
> `Cargo.toml` is left at the **baseline** (no `lto` key) so nothing is silently in
> effect. To re-apply the arm that works, add one line under `[profile.release]`:
>
> ```toml
> lto = "fat"
> ```

## 1. Why this question exists, and what it decides

`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.10 wants an `#[inline]` audit across
the crate boundary: attributes have been applied ad hoc as code moved from `src/`
into `crates/`, nobody has swept them, and just **75 of 756 `pub fn` in `crates/`
carry one (~10%)**.

The load-bearing fact under that audit:

> `[profile.release]` sets **only** `panic = "abort"` — no `lto`, no
> `codegen-units`. So a call from `src/` into `crates/akuma-*` crosses a codegen-unit
> boundary and is inlined **only** if the callee carries an attribute or is generic.
> Without one, the IR isn't there and there is no decision for LLVM to make.

That is not a heuristic-tuning problem, it is a *visibility* problem — which is why
LTO is the lever. **Settle this first**: `lto = "thin"` or `"fat"` makes most of the
~700-function audit moot, and it is one line rather than 700 judgement calls. If LTO
turns out not to pay, that is equally decisive — it means the cross-crate calls were
not costing what §5.10 assumed, and the audit can be *closed* rather than done.

Only `extreme-size` sets `lto` today (`lto = true`, `codegen-units = 1`,
`opt-level = "z"`), so it is unaffected by this question **and** serves as a working
control: fat LTO demonstrably links this kernel.

## 2. What was measured, and how

Same tree for every arm (the Phase 7 `#[repr(C)]` sigframe work, uncommitted), only
the `[profile.release]` `lto` key changed. Per arm:

```bash
touch src/exceptions.rs                       # force a rebuild + relink, not a cache hit
/usr/bin/time -l cargo build --release        # wall time + peak RSS (macOS: -l)
scratchpad/measure.py target/aarch64-unknown-none/release/akuma   # ELF section sizes
```

`measure.py` parses the ELF section headers directly (there is no `llvm-readelf` in
this toolchain and macOS `size` only understands Mach-O). It is 20 lines; recreate it
or read section sizes any way you like — the numbers below are `.text`/`.rodata`/
`.data`/`.bss` and the total file size.

**The rebuild+link number is the one that matters operationally**, not clean-build
time: it is what an edit-compile cycle costs, and therefore what acceptance 10's
self-hosted kernel build pays per iteration.

## 3. Results

| arm | file | `.text` | `.rodata` | rebuild+link | peak RSS |
|---|---:|---:|---:|---:|---:|
| **baseline** (no `lto`) | 3,480,848 | 2,175,428 | 384,968 | 5.31 s | 738 MB |
| `lto = "thin"` | 3,715,496 | **2,404,116** (+10.5%) | 383,576 | 10.60 s (**2.0×**) | 779 MB |
| `lto = "fat"` | **3,355,824** (−125 KB) | 2,289,896 (+5.3%) | 367,352 | 19.03 s (**3.6×**) | 1,090 MB (**+48%**) |

`.data`/`.bss` move by <3 KB across all three; they are not interesting here.

Two things worth not misreading:

- **`.text` grows under LTO, in both arms.** That is inlining doing its job —
  duplicating callee bodies into call sites — not a regression. Size is a *proxy*
  here, not the objective; the objective is speed on boundary-crossing paths, which
  is still unmeasured (§5).
- **Fat produces a smaller total file than baseline while having a bigger `.text`.**
  The saving is elsewhere in the image (`.rodata` −17.6 KB and non-allocated
  sections), so do not read "smaller file" as "less code".

### It really did inline

`llvm-nm` on the fat arm, for the three cross-crate calls Phase 7 added on the
signal path (none of which carry `#[inline]`):

| symbol | present in image |
|---|---|
| `sigframe::save_regs` | **0** |
| `sigframe::restore_regs` | **0** |
| `sigframe::sync_frame_neon` | **0** |
| `akuma_primitives::preempt::current_tid` | 2 |

All three were inlined away. This is the direct evidence that §5.10's premise was
real — those calls *were* crossing a codegen-unit boundary uninlined at baseline —
and that LTO fixes it without touching a single attribute.

## 4. The constraint that should probably decide this

**Acceptance 10 builds this kernel inside akuma.** LTO's cost lands there twice: a
3.6× longer link (fat) on a guest that is already slow, and — the real risk — peak
linker memory, +48% on the host for fat. This is a guest whose memory pressure has
its own archive shelf (`FPCACHE_UNDERSIZED_AT_LOW_RAM.md`, the OOM escalation ladder,
the `-j4` campaign).

So the honest framing is not "thin vs fat" but:

1. Does LTO measurably speed up the paths §5.10 named? (**unmeasured** — §5)
2. If yes, can the self-host build still complete with it on?

If (2) fails, the answer is **not** "no LTO" — it is a separate `release-lto`
profile, leaving the self-host path on the cheap default. That keeps the decision
from being all-or-nothing, and it is a smaller change than either alternative.

On the thin-vs-fat axis specifically, **fat currently looks better than thin on every
axis except build cost**: smaller image, smaller `.text` growth, smaller `.rodata`.
Thin's usual selling point is being most of the win for a fraction of the cost, and
here it is *neither* — 10.5% `.text` growth for a 234 KB bigger image. That is worth
a second look before trusting it; a plausible explanation is that ThinLTO's
summary-based inlining is duplicating across modules without fat's whole-program view
to clean up after it, but that is a hypothesis, not a measurement.

## 5. What is NOT done yet

1. ~~**Correctness.**~~ **Fat: DONE and clean (2026-08-14).** Full gate on
   `lto = "fat"`, diffed against the baseline summary: four clippy configs clean,
   booted at SMP=1 **and** SMP=4, `[PASS]` 95 at both, **empty failure sets at both**,
   all seven exercise binaries `ok` at both, `stack_overflow` 1 (the deliberate
   canary), `host_timejumps` 0. The only diffs from baseline were the +5 host tests
   Phase 7 adds (528 → 533) and `bkl_stuck` (96 → 93, load-driven noise). So fat LTO
   does **not** disturb this kernel's observable behaviour on the gate's coverage.

   **`lto = "thin"` has NOT been gate-tested** — only built and sized. Given fat beats
   it on every axis except build cost (§3), the thin arm is probably not worth the
   run; if you disagree, run it before trusting the numbers in §3 for anything.

   Caveat on what "clean" covers: the gate exercises fork/CoW, ELF loading, mmap and
   the stdio paths. It does **not** cover the self-host build, the devbox profiles, or
   long-running workloads — the places where an LTO-surfaced UB would most plausibly
   hide. Items 3 and 4 below are still the risk.
2. **The actual speed measurement**, which is the whole point. Per §5.10 this must
   be timed with the `PSTATS_TIMING_PREEMPTION_ARTIFACT` method, **not** PSTATS. A
   good candidate path is now the signal frame: `save_regs`/`restore_regs`/
   `sync_frame_neon` are cross-crate, on the fault path, uninlined at baseline and
   inlined under LTO — a clean before/after with a known mechanism.
3. **The self-host build under LTO** (acceptance 10) — the constraint in §4.
4. **`devbox-smoltcp` and `devbox-rump`**, which also build `--release` and therefore
   inherit any `lto` key. The gate compiles them, but they have not been booted.
5. **Whether the precompiled `core`/`alloc` participate.** `--release` uses the
   distributed `aarch64-unknown-none` std (only `extreme-size` passes
   `-Z build-std`). Both LTO arms linked without complaint, so the shipped rlibs
   carry usable bitcode — but nothing here checks how *much* of core got optimized
   in. If that turns out to matter, `populate_disk.sh` notes `rust-src` is already
   shipped "for any future build-std profile".

## 6. If you pick this up

The cheap order, given what is already known:

1. Re-apply `lto = "fat"`, run the full gate, diff against a baseline summary. If
   the failure sets differ **at all**, that is the finding — stop and investigate,
   because it means LTO surfaced something real.
2. Same for `lto = "thin"` only if fat fails or self-host rules it out; on these
   numbers thin has no advantage worth the extra arm.
3. Only then measure speed (§5.2). If the win is not visible on a path that
   demonstrably lost a real call, close §5.10 rather than doing it.

Do not skip step 1 for step 3. A faster kernel that boots differently is not a
faster kernel.

## Background

- [`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  §5.10 (the audit this decides), Phase 8 (where it is filed)
- [`REPR_C_SIGFRAME_STATX.md`](REPR_C_SIGFRAME_STATX.md) — the cross-crate calls used
  as the inlining probe
- [`../reference/build-profiles.md`](../reference/build-profiles.md) — profile +
  feature set pairs; `extreme-size` is the existing `lto = true` example
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md) —
  the gate every arm must pass
