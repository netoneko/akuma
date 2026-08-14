# Should `[profile.release]` set `lto`? (started 2026-08-14)

> **Status: DECIDED 2026-08-14 — `[profile.release]` now sets `lto = "thin"`.**
> Both LTO arms pass the full gate clean at SMP=1 and SMP=4 (§5.1). **Thin was
> chosen over fat on peak linker memory, not on code quality:** fat peaks at
> ~1.09 GB against thin's ~779 MB, and this kernel builds itself (acceptance 10),
> so fat will not link on a 1 GB guest where thin will. Fat also costs 3.6× the
> link time per iteration against thin's 2.0×.
>
> `extreme-size` keeps `lto = true` (fat) — it declares it explicitly, so it
> overrides the inherited value; verified byte-identical after the change (§5.5).
> Every other profile and feature set inherits thin.
>
> **Still unmeasured, and still the point:** the *speed* win this was supposed to
> buy (§5.2). The image and the inlining evidence say LTO is doing real work; no
> benchmark yet says it is faster. Also unmeasured: the self-host build itself
> under thin (§5.3) — the memory argument above is a host-side proxy for it.

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

**How that resolved: thin, on memory.** Fat is better on every code-quality axis
here — smaller image, smaller `.text` growth, smaller `.rodata` — and thin is, oddly,
*neither* cheap nor small (10.5% `.text` growth for a 234 KB bigger image than
baseline, which is not what ThinLTO's usual pitch predicts; a plausible explanation
is summary-based inlining duplicating across modules without fat's whole-program view
to clean up after it, but that is a hypothesis, not a measurement).

None of that outweighs the one axis this build cares about: **fat peaks at ~1.09 GB
of linker memory and thin at ~779 MB.** A kernel that compiles itself must link on a
1 GB machine, so fat is disqualified regardless of producing nicer code, and thin's
2.0× link cost is the cheaper of the two tolls. If the self-host build later proves
comfortable with more memory than assumed, fat is a one-word change and §3 has its
numbers ready.

## 5. What is NOT done yet

1. ~~**Correctness.**~~ **Fat: DONE and clean (2026-08-14).** Full gate on
   `lto = "fat"`, diffed against the baseline summary: four clippy configs clean,
   booted at SMP=1 **and** SMP=4, `[PASS]` 95 at both, **empty failure sets at both**,
   all seven exercise binaries `ok` at both, `stack_overflow` 1 (the deliberate
   canary), `host_timejumps` 0. The only diffs from baseline were the +5 host tests
   Phase 7 adds (528 → 533) and `bkl_stuck` (96 → 93, load-driven noise). So fat LTO
   does **not** disturb this kernel's observable behaviour on the gate's coverage.

   **Thin: DONE and clean too (2026-08-14).** Same result — four clippy configs
   clean, both SMP widths booted, `[PASS]` 95 and empty failure sets at both, all
   seven exercises `ok`, 533/0 host tests. So the choice between the two is *not* a
   correctness question on this coverage; it is the memory/time question in §4.

   One scare on the way, worth knowing about: thin's **first** gate run reported
   `host.failed: 1` with the total at 430 instead of 533. It did not reproduce in
   four re-runs, and it is **not LTO** — the same failure with the **identical
   103-test gap** is recorded in the runbook from the Phase 5 sweep, on a tree with
   no `lto` key at all. `tier1_tests` now saves `verify_host_tests.log` on every run
   and reports `host.failed_names`, so the third occurrence should be diagnosable;
   see the runbook's "One reading this gate produced that nobody could reproduce".

5. **`extreme-size` is unaffected**, as intended: it declares `lto = true`
   explicitly, so it overrides the inherited `"thin"`. Rebuilt after the change and
   compared byte for byte — `file=628376 .text=454976 .rodata=66791`, identical to
   the pre-change build. The 4.0 MB floor acceptance 05 gates on is untouched.

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

Both arms are gate-verified, so the remaining work is the two measurements that
actually justify the setting:

1. **Run the self-host build (acceptance 10) under `lto = "thin"`.** This is the
   constraint the choice was made on, measured only by host-side proxy. If it
   fails at the link step, the answer is a separate `release-lto` profile, not
   reverting — see §4.

   **Attempted 2026-08-14 and BLOCKED — by something unrelated to LTO.** A
   devbox-smoltcp self-host run at **1024 MB** under `lto = "thin"` produced two
   rustc ICEs (`decode error: Expected header tag [79, 68, 72, 84]`, i.e. `ODHT`,
   `but found [0, 0, 0, 0]` — zeros where a dependency's metadata should be, in
   `zerocopy-derive` and `enumn`, two parallel proc-macro jobs, within one second
   of each other) and then a `Segmentation fault`. A later run **hung** after
   `akuma-exec` finished: `cargo build --release -p akuma` alive with **no `rustc`
   child process at all**, no progress.

   **LTO is not the cause of the hang** — it reproduces with the `lto` key
   commented out. The hang is the known self-host wedge shape
   (`../runbooks/selfhost-kernel-build.md` §5.1). The ICE is unattributed: it has
   the shape of a *read* returning zeros under memory pressure rather than a
   corrupt file, but that was not established (see the method warning below).

   So this item is still open, and it is now **blocked on the self-host wedge**,
   not on a measurement anyone can just take.

   > **Method warning, learned the hard way in that session.** Do **not** test a
   > guest-built rlib for corruption with `grep -c ODHT`. That tag is not stored
   > literally in artifacts produced by the guest toolchain: its **own sysroot**
   > `libcore.rlib` — known-good by definition — also scores 0. A scan using it
   > reported "112 of 112 artifacts corrupt", including a 5 KB file, on a build
   > whose crates had demonstrably read each other's metadata successfully.
   > Establish the control **on an artifact from the same toolchain** before
   > believing any corruption test.
2. **Measure the speed win** (§5.2), timed with the
   `PSTATS_TIMING_PREEMPTION_ARTIFACT` method rather than PSTATS, on a path that
   demonstrably lost a real call — the signal frame's `save_regs`/`restore_regs`
   are the cleanest candidates. **If there is no measurable win, that is a result:
   close §5.10's `#[inline]` audit rather than doing it**, and reconsider whether
   thin is worth 2× link time at all.
3. Boot the two devbox profiles, which inherit thin and have only been compiled.

Do not reorder 1 ahead of nothing: a kernel that cannot build itself is worse than
one that inlines poorly.

## Background

- [`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  §5.10 (the audit this decides), Phase 8 (where it is filed)
- [`REPR_C_SIGFRAME_STATX.md`](REPR_C_SIGFRAME_STATX.md) — the cross-crate calls used
  as the inlining probe
- [`../reference/build-profiles.md`](../reference/build-profiles.md) — profile +
  feature set pairs; `extreme-size` is the existing `lto = true` example
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md) —
  the gate every arm must pass
