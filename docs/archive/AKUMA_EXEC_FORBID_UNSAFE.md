# `akuma-exec` reaches `#![forbid(unsafe_code)]`

**Date:** 2026-09-02
**Branch:** `oof-part-2`
**Closes:** [`AKUMA_EXEC_AUDIT.md`](AKUMA_EXEC_AUDIT.md) §6 — the whole recommended
order, A through E.

**Result:** `crates/akuma-exec` — **9,311 production lines, the largest crate in
the tree** — carries `#![forbid(unsafe_code)]`. It reads **0 `unsafe` sites,
100.0% safe** (`scripts/cloc_akuma.py src crates`). The audit opened at 123
sites in 8 kinds; this is the end of that arc.

Tree-wide: **32 of 52 crates** enforced unsafe-free, **73.9%** of the code under
`crates/` (was 62.1% — `akuma-exec` alone is the 11.8-point jump).

---

## 1. Where the 123 went

Nothing was removed by making an operation safe in place. Every genuine `unsafe`
moved to a crate that *owns the thing it pokes*, the pattern
`docs/reference/crate-safety.md` calls "extract the obligation":

| what | to | doc |
|---|---|---|
| the EL0 user-copy loop + fault trampoline | `akuma-user-access` | [`AKUMA_EXEC_USER_ACCESS_EXTRACTION.md`](AKUMA_EXEC_USER_ACCESS_EXTRACTION.md) |
| the scheduler, context switch, per-slot state arrays | `akuma-threading` | `AKUMA_EXEC_AUDIT.md` §6.C |
| `enter_user_mode`'s register-load + `eret` `asm!` | `akuma-el0-entry` | `AKUMA_EXEC_AUDIT.md` §6.E |
| the `*mut Process` slot store + every deref | `akuma-slot-table` (generic `SlotTable<T, N>`) | [`AKUMA_SLOT_TABLE_EXTRACTION.md`](AKUMA_SLOT_TABLE_EXTRACTION.md) |
| page-table walks (`demote_range_to_ro`, `map_user_page`) | `akuma-mmu` — each gained a `&self`-safe form | `AKUMA_EXEC_AUDIT.md` §6.E group 3 |

And the six `&self -> &mut` field casts the audit found (§5) became atomics or
moved *inside* the lock that already guarded them —
[`AKUMA_EXEC_ADDRESS_SPACE_MERGE.md`](AKUMA_EXEC_ADDRESS_SPACE_MERGE.md) for the
last two, `AKUMA_EXEC_AUDIT.md` §5a/§5c-bis/§6.E for the rest.

## 2. The three groups that stood between §6.D and `forbid`

`AKUMA_EXEC_AUDIT.md` §6.E's correction — "A–D do **not** leave the crate
`unsafe`-free" — named three residues. All three cleared 2026-09-02:

- **Group 1 — the `table.rs` slot store (25 → 7 sites).** Genericised into
  `akuma-slot-table`. `table.rs` keeps every domain concern (the per-thread
  identity cache, `THREAD_PID_MAP`, the reclaim hooks); the orderings and the
  RETIRED-window generation bump are preserved verbatim. The identity cache lost
  its `own_ptr`/`tgid_ptr` fields — `identity_get` derives the pointer from
  `SlotTable::ref_if_current(slot, generation)`, which within one generation is
  provably equal to what the cached pointer held.

- **Group 2 — the execve / first-run exclusive `&mut Process` window (7 → 2).**
  Phase 7f, in two shapes:
  - **2a:** `Process::{state, exited, exit_code}` → atomics (`state` an
    `AtomicU64` packing `Zombie(i32)`), so `Process::run` / `prepare_for_execution`
    take `&self` and `entry_point_trampoline` / `run_registered_process` reach
    their process through the safe `table::active_process_ref`.
  - **2b:** `Process::{entry_point, initial_brk, process_info_phys,
    clear_child_tid, sigaltstack_*}` and the five `ProcessMemory` scalars →
    atomics; `name`/`args`/`context` → one `Spinlock<ProcessImage>`;
    `replace_image` and `ProcAddressSpace::replace` take `&self`.
    `table::{get_process_ptr, with_process_exclusive}` and
    `SlotTable::active_exclusive` deleted; `with_own_process_exclusive` kept but
    now safe (`&Process` via `lookup_process_shared`, BKL check retained as the
    peer guard until Phase 7f finishes).

- **Group 3 — the two decoupled-pointer sites (2 → 0).** Both were
  non-problems. `fork`'s `demote_range_to_ro` `parent_l0`/`parent_as` were only
  "independent" in the self-test; in production the live TTBR0 *is* the leader's
  address space, so the raw `*const u64` param went away and the demote is a
  `&mut self` method on `UserAddressSpace` (`akuma-mmu` +1). The vfork-prefault
  "lock A, record in B" edge shares **one L0** between A and B — the faulted
  page is the leader's to free — so the special case was deleted and the page
  tracks against the leader.

## 3. Verification — the full trim-fat gate + Tier 5

Baseline `25c817b8` (pre-campaign), `docs/runbooks/verify-trim-fat-change.md`.

**Tiers 1–3 (`verify_trim.py` A/B).** Diff vs baseline: `host.tests` **1102 →
1113** (+11 — `SlotTable` ×9, `ProcessState` bits ×1, `ProcessMemory::reset`
×1), plus two "weather" rows (`smp4.bkl_stuck` 141→150, `smp4.host_timejumps`
4→2). **Every substantive row identical**: 4 clippy configs clean, `host.failed`
0, both SMP levels booted, all 16 fork/CoW/fault exercises `ok` on both arms,
`fail_set` empty, `flaky_seen` none, `pass_marker`/`passed_marker` 100 / 310 /
318.

**Boot self-test suite:** 406 PASS / 0 FAIL / 0 PANIC — including
`identity_recycled_slot_rejected`, `epilogue_identity_revalidated`,
`fork-bkl-drop`, `fork_cow_share_incs_once_per_frame`, `cow_ref_ledger`,
`signal_reset_on_exec`, `execve_no_heap_leak`, `replace_image_preserves_pid`,
`execve_kills_thread_group_siblings`, `pmm_conserved_across_spawn_exit_reap`
(0-page drift).

**SMP=4 `forktest_smp_matrix.py`:** 7/7 PASS, 0 `[BKL] RECOVERED`, 0 PANIC, 0
WILD-DA, run three times across the three group commits.

**Tier 5 — self-host clean-build A/B** (devbox-smoltcp kernel, `MEMORY=8192
SMP=4`, 8 in-guest `cargo clean && cargo build -p akuma -j4 --offline` trials
per arm):

| | HEAD | baseline |
|---|---|---|
| trials GREEN | 8/8 | 8/8 |
| artifact | 4,545,944 B (all) | 4,545,944 B (all) |
| PMM-RESURRECT / UAF / POISON / WILD-DA / PANIC / rustc-ICE / FILL-SHORT `Ok(0)` / BKL-RECOVERED | 0 | 0 |
| `[BKL] stuck` (contention) | 67 | 63 |
| WATCHDOG time-jump (host) | 8 | 7 |

Byte-identical build artifacts, zero on every corruption tripwire on both arms.
The campaign is behaviour-preserving and confirmed.

## Background

- [`AKUMA_EXEC_AUDIT.md`](AKUMA_EXEC_AUDIT.md) — the audit this closes; §6 is the
  recommended order, §6.E the correction and the three groups.
- [`AKUMA_SLOT_TABLE_EXTRACTION.md`](AKUMA_SLOT_TABLE_EXTRACTION.md),
  [`AKUMA_EXEC_ADDRESS_SPACE_MERGE.md`](AKUMA_EXEC_ADDRESS_SPACE_MERGE.md),
  [`AKUMA_EXEC_USER_ACCESS_EXTRACTION.md`](AKUMA_EXEC_USER_ACCESS_EXTRACTION.md)
  — the per-step records.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5 — Phase 7f, which Group 2
  advances (the BKL check in `with_own_process_exclusive` is the last thing
  standing between "safe" and "provably no peer").
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the enforced /
  not-enforceable tables `akuma-exec` moved between.
