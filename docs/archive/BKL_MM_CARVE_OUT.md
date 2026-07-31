# Phase 5: carving memory-management syscalls out from under the BKL

**Status: COMPLETE, 2026-08-01.** `sys_mprotect`/`sys_madvise`/`sys_munmap`/`sys_mremap`/
`sys_mmap` now run without the Big Kernel Lock under `smp-shared`, behind the
`no-bkl-mm` feature (default-on in the `smp-shared` bundle since 2026-08-01 —
see §6).
Mirrors the net/vfs/process carve-outs (`docs/archive/BKL_VFS_CARVE_OUT.md`,
`BKL_PROCESS_CARVE_OUT.md`) but is the first phase where the audit found **real,
unaudited gaps** rather than rediscovering an existing lock — see §1.

Unlike every prior conversion in this campaign, this phase was **not** picked by
attribution: no mm syscall has ever been named a significant BKL holder in a
`bkl-profile` run (`mmap` was 2.4% of the pool in `BKL_VFS_CARVE_OUT.md` §19.2, and
that pool has since shrunk 67% from the `netpoll_drain` carve). It was undertaken
because it was next in `BKL_FINE_GRAINED_LOCKING_PLAN.md`'s phase list, at the user's
direction. Section 5 is explicit about what that means for the numbers below.

## 1. The audit: what already had a lock, and what didn't

Every mm-syscall code path in `src/syscall/mem.rs` was traced against the state it
touches, the same method `BKL_PROCESS_CARVE_OUT.md` used for `fork_process`.

**Already covered by an existing fine-grained lock, no new work needed:**

| state | lock | scope |
|---|---|---|
| `mmap_regions` (Vec) | `Process::vm_lock` (via `vm_with_regions`) | unconditional, not `kernel_smp_shared`-gated |
| page-table PTEs | `Process::as_lock` (via `AsLockHold`/`as_lock_hold`) | `kernel_smp_shared`-gated; the SAME lock the CoW fault handler and `fork_process`'s share/demote pass already take BKL-free |
| `UserAddressSpace::user_frames`/`page_table_frames` (bookkeeping) | their own `Spinlock`s | unconditional |
| `LAZY_REGION_TABLE` | its own `Spinlock` | unconditional |
| PMM / `FRAME_TRACKER` | their own `Spinlock`s, never held across a yield | unconditional |
| `SHARED_FILE_MAPPINGS` | its own `Spinlock`, dropped before `writeback_shared_pages`'s disk I/O | unconditional |
| `COW_REFCOUNTS`/`COW_FAULT_LOCK` | their own `Spinlock`s, reached only via `pmm::free_page` after `as_lock` closes | unconditional |

This means `sys_mprotect` and `sys_madvise` were carveable with **zero new locking
work** — the same "nothing to build" finding as net (Phase 2) and vfs (Phase 4).

**Two real gaps, closed as a prerequisite:**

1. **`ProcessMemory::free_regions`/`alloc_mmap()`** (`crates/akuma-exec/src/process/
   types.rs`) was a plain, entirely unguarded `Vec<(usize, usize)>`. `alloc_mmap`
   takes `&mut self` and mutates it directly with no lock of any kind — every caller
   relied solely on the BKL for exclusivity. This is not just a BKL-drop hazard: it
   was already racy under IRQ preemption of a CLONE_VM sibling thread (the identical
   bug class `Process::vm_lock` itself was introduced to fix for `mmap_regions`, per
   that field's own doc comment). **Fix:** two new `Process` methods,
   `vm_alloc_mmap`/`vm_free_mmap`, reuse the existing `vm_lock` (IRQ-disabled, pure
   bookkeeping only — the same discipline `vm_lock` already enforces for
   `mmap_regions`) rather than adding a new lock. Every call site in `src/syscall/
   mem.rs`, `src/syscall/aio.rs`, and `crates/akuma-exec/src/process/children.rs`
   was moved onto the new methods.
2. **`sys_mmap`'s OOM/reclaim fallback** (`reclaim_clean_file_pages` →
   `UserAddressSpace::try_evict_ro_page`, reached from `mem.rs`'s eager-allocation
   OOM path) mutated live page-table PTEs with **no `as_lock` hold at all** — the
   only mm-syscall page-table mutation site that didn't already take it. **Fix:**
   `reclaim_clean_file_pages`'s sweep loop now takes a fresh, short `as_lock_hold`
   **per page** (not once across the whole up-to-262144-page scan — see the comment
   at the call site for why a single long hold would starve this core's timer for
   the sweep's duration, violating the "mask per attempt, never across an unbounded
   wait" rule in `docs/reference/subsystems/locking.md`).

Both fixes landed and were host-tested (155/155 `akuma-exec` tests, clippy clean on
`--release`, `release-smp-shared --features smp-shared`, and
`release-smp-shared --features smp-shared,no-bkl-mm`) **before** the BKL-drop guard
was added, so the carve-out itself introduces no new locking surface — same
methodology as every prior phase.

## 2. `MmBklGuard`

`src/syscall/mem.rs`, mirrors `VfsBklGuard` exactly, including the latching
discipline (§2.4 of `BKL_VFS_CARVE_OUT.md`: the runtime toggle is read once at
construction and never re-read in `drop()`, so a toggle flip mid-syscall can't
unbalance the ticket FIFO). Gated `#[cfg(all(kernel_smp_shared, kernel_no_bkl_mm))]`;
runtime toggle `smp_shared::mm_bkl_drop_enabled()`/`set_mm_bkl_drop_enabled()`,
default **on**.

Constructed after the syscall resolves its `Process` reference (kept BEFORE the
guard opens, never re-looked-up inside the window — the process table itself has no
inner lock, `BKL_PROCESS_CARVE_OUT.md`'s original finding, so every existing
carve-out already follows this discipline) and after early-error/arg-validation
returns (`EINVAL`/`EFAULT` on malformed arguments never touches guarded state).

One thing this carve-out does NOT need that net/vfs did: `PreemptGuard`'s IRQ-masking
arm is conditionally gated on `no-bkl-network`/`no-bkl-vfs` specifically (not
`kernel_smp_shared` alone) — that's what closes the AB-BA nested-IRQ hazard those
carve-outs found (`BKL_VFS_CARVE_OUT.md` §19.3). `as_lock`'s `AsLockHold` and
`vm_lock`'s `with_irqs_disabled` already mask IRQs **unconditionally** whenever
`kernel_smp_shared` is on, independent of any `no-bkl-*` feature — so there's no
equivalent gating subtlety to get wrong here.

`sys_mmap`'s existing `VfsBklGuard` windows (inode resolution, file-backed fill) now
nest inside the outer `MmBklGuard` window. This is safe by construction: the dropped-
window ledger (`akuma_exec::bkl::DroppedWindowLedger`) is a **depth counter**, not a
boolean — `dropped_window_open()`'s inner `leave_kernel()` no-ops on a core that
already released the BKL, and `dropped_window_close()` only re-acquires when the
ledger depth returns to zero. Nesting guards was already a proven pattern before
this phase (fork's `ProcessBklGuard` window can itself contain nested VFS reads).

## 3. Verification

**Host:** 155/155 `akuma-exec` tests, clippy clean across `--release`,
`release-smp-shared --features smp-shared`, and `--features smp-shared,no-bkl-mm`.

**Boot self-test** (`test_mm_bkl_drop`, `src/process_tests.rs`): drives the real
syscall entry points via `handle_syscall`, checking both the documented return value
and that `bkl::in_dropped_window()` is false once the call returns. Deliberately does
NOT attempt a real PTE install — `mmu::map_user_page_no_flush` (what `sys_mmap`'s
eager fill and `sys_mremap`'s growth path use) reads the live `TTBR0_EL1`, so
exercising it correctly from a boot self-test would need a genuine context switch to
a synthetic process's own page tables, which no self-test in this suite does for
mmap (`run_cow_benchmarks` sidesteps the same problem by taking an explicit L0
pointer instead of the ambient-TTBR0 mmu calls). Instead: 3 early-error cases (must
never open the window), `mprotect`/`madvise` on a never-mapped VA (real `as_lock`/
`LAZY_REGION_TABLE` touch, no PTE actually flips), an `mmap`/`munmap` round trip on a
fresh anonymous `PROT_NONE` region (takes the lazy fast path — `push_lazy_region`,
never installs a PTE — exercising `vm_alloc_mmap`/`vm_free_mmap` for real), an
`mremap`-grow EFAULT case on an unmapped VA, and the runtime kill-switch. **PASSED at
both SMP=2 and SMP=4** (real QEMU boots, `devbox-smoltcp`-independent plain
`smp-shared,no-bkl-mm` builds): 0 PANIC/WILD, 0 stale-dropped-window heals, only the
two pre-existing unrelated failures (`fs_error_to_errno_mapping`'s stale EPERM/EACCES
expectation, `stp_xzr_ec15_handler_fires`'s QEMU EC-generation quirk — both predate
this session).

**Real PTE-install correctness** — the part the boot self-test structurally can't
cover — validated end-to-end with the same tools Phase 2e used
(`BKL_VFS_CARVE_OUT.md` §10.2), re-run against the `no-bkl-mm` build,
`devbox-smoltcp`, SMP=4, 4 GB, against the same `qwen3.5-0.8b-q4.gguf` (508 MB):

| check | result |
|---|---|
| `mmap_stress` (anon mmap/memset/munmap churn) | 21 iterations, clean |
| `mmap_file` (508 MB gguf, touch all pages) | mapped 532517120 bytes, all pages touched |
| `mmapsum` (read vs mmap ×2 vs madvise-prefaulted vs 2-thread-concurrent) | all 4 digests byte-identical (`942346dccb5a7a30`) |
| `fpfault` (32 Q-reg canary across every demand fault) | 0/130009 corrupted |
| `neonfault` (page-crossing NEON loads into faulting pages) | 0/130008 wrong |
| `llama-bench` chat model load + inference | loads and runs (see §4 for numbers; `llama-cli`'s interactive mode hit an unrelated chat-template parse bug in this llama.cpp build, not a kernel issue — `llama-bench` doesn't go through that code path) |

Matches or exceeds every figure in the original Phase 2e table — no regression from
adding the mm carve-out on top of the existing VFS one.

**Contention regimen** (`net4→net4→read4→cp2→rm`, SMP=4, `bkl-profile`,
`devbox-smoltcp`): 6/6 digests exact, 0 `[BKL] stuck`, 0 PANIC/WILD/stale-ledger.
Total contended spins **42.6M vs. the documented post-`netpoll_drain` baseline of
47.3M** (`BKL_VFS_CARVE_OUT.md` §20.4) — a ~10% cut. `mmap`/`munmap`/`mprotect`/
`madvise` do not appear as named holders in this regimen's top-12 either before or
after; this workload was never designed to exercise the mm syscalls (unlike
`net4`/`read4`/`cp2`/`rm`, which are named for what they stress), so its ~10% total
reduction is suggestive but not attributable specifically to this carve — see §5.

## 4. Bonus: real Akuma tok/s numbers, and a same-binary native comparison

Neither of these existed before this session. `llama-bench -m qwen3.5-0.8b-q4.gguf -t
1 -p 64 -n 32`, same musl-static binary (`bootstrap/bin/llama-bench`) on both sides:

| environment | prompt (t/s) | generation (t/s) |
|---|---|---|
| Akuma OS (QEMU/HVF, `no-bkl-mm`, SMP=4 kernel) | 107.57 ± 4.63 | 20.32 ± 0.34 |
| Native ARM64 Linux (Docker/Alpine container, same binary, same host) | 110.80 ± 0.25 | 22.78 ± 0.09 |
| **Akuma as % of native** | **97.1%** | **89.2%** |

This is the same binary both sides — no compiler/build differences, only kernel and
virtualization layer differ. Contrast with the ~256×/~3250× TCG-era gap recorded in
`docs/archive/LLAMA_CPP_AKUMA_VS_ALPINE_PERFORMANCE_GAP.md`: since Akuma moved to HVF
(2026-06-09) the remaining gap is single-digit-to-low-double-digit percent, consistent
with that doc's prediction ("likely within 10-20% of a Linux kernel"). Not a controlled
A/B for THIS carve-out specifically (no BEFORE-carve number was captured — the task
was to compare against a native baseline, not to re-litigate the netpoll-era
measurement), but a useful standing reference point for future phases.

A prebuilt `ghcr.io/ggml-org/llama.cpp:full` image was tried first for the native
comparison and failed twice (a from-source `cmake` build stalled with no output for
10+ minutes; the prebuilt image's largest layer wouldn't finish pulling in 3+
minutes) — both abandoned in favor of a pre-existing local image
(`llama-bench-kv-mmap:latest`) that already bundled Akuma's own `bootstrap/bin/
llama-bench`/`llama-server`, which turned out to be the better comparison anyway
(identical binary, not just identical source).

## 5. Why this phase is plan-driven, not evidence-driven — and what that means

Every prior conversion in this campaign followed §7 of `docs/reference/subsystems/
locking.md`: "let attribution — not intuition — pick the next target." This phase
broke that pattern deliberately, at explicit user direction, once the VFS/net/process
list was exhausted and `netpoll_drain`'s 67% cut had shrunk the remaining pool enough
that no single syscall stood out. The mm syscalls were carved because they were next
in the plan, not because a `bkl-profile` run ever named them.

Consequence: unlike `unlinkat` (72.6% → absent) or `netpoll_drain` (57.2% → absent),
there is no "this specific syscall's share dropped to zero" result to report here,
because it was never measurably above zero to begin with. The audit-and-fix work
(§1) is real and was worth doing regardless — the `free_regions` race was a genuine
latent bug, found by asking "what does a BKL-free window need" rather than "what is
contended" — but the performance story for this phase is "closed a correctness gap
and shipped a carve-out with no regression," not "cut contention by N%."

## 6. Status and next steps

**Shipped and promoted to default-on (2026-08-01).** `no-bkl-mm` is now in the
`smp-shared` feature bundle in `Cargo.toml`, alongside net/vfs/process. The
audit + boot-suite + stress-tool verification in this doc was accepted as
sufficient evidence on its own, matching the bar `no-bkl-process` cleared
before its promotion (that phase DID have a contention number — `clone`
19.5%→2.5% — this phase doesn't, but the correctness work — the `free_regions`
race fix and the `as_lock` gap in the OOM-reclaim path — stands regardless of
whether any workload currently exercises these syscalls under contention).

`sys_msync`, `sys_brk`, and `sys_fchmod`/`sys_fallocate`/`sys_ftruncate`/
`sys_truncate` remain fully BKL-held and unaudited by this phase.

## Background

- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) — the
  overall phased plan; Phase 5 is "Memory Management Locks."
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) — the VFS carve-out (§19-20 cover
  the `netpoll_drain` carve whose post-carve numbers this phase's regimen run is
  compared against).
- [`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) — the process carve-out;
  its audit methodology (trace every touched state to a lock or the lack of one) is
  the template §1 above follows.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) —
  current-state carve-out playbook and syscall→lock map (Grade B).
- [`LLAMA_CPP_AKUMA_VS_ALPINE_PERFORMANCE_GAP.md`](LLAMA_CPP_AKUMA_VS_ALPINE_PERFORMANCE_GAP.md)
  — the TCG-vs-HVF performance history §4's comparison extends.
