# `process/user_access.rs` → `akuma-user-access`

**Date:** 2026-09-02
**Branch:** `oof-part-2`
**Item:** `AKUMA_EXEC_AUDIT.md` §6 step A — the first, cheapest step of splitting
`akuma-exec` toward `#![forbid(unsafe_code)]`.

**Result:** the EL0 memory boundary — the `__arch_copy_user_memory` asm loop, its
fault-recovery trampoline, and the `copy_from/to_user` / `validate_user_range`
helpers built on them — is its own crate. `akuma-exec` goes from **126 to 112**
`unsafe` blocks; the 14 that left are behind one stated contract in a
~760-line crate.

---

## 1. Why this one first

`AKUMA_EXEC_AUDIT.md` §2 counts ~119 of the crate's ~126 `unsafe` blocks in four
files. Of those four, `user_access.rs` is the **most self-contained**: it is the
`akuma-net-nic` shape — "every `unsafe` line here is the same contract, stated
once." The other three (`threading/mod.rs`, `process/mod.rs`, `process/table.rs`)
are either a genuine primitive layer or need the `Process` type; this one just
needs a place that is *allowed* to hold asm.

It also moved *up* into `process/` from `akuma-mmu` in the 2026-08-30 split
"because its eight process references were the whole of the old `mmu <-> process`
cycle" — so a clean extraction means breaking those references, not carrying
them.

## 2. The two cross-crate edges, and how each was cut

### Edge 1 — `set_user_copy_fault_handler` (was `→ akuma-exec::threading`)

`copy_from_user_safe` arms `__arch_copy_user_fault` as the calling thread's
handler around the asm copy; `akuma-exceptions`' EL1 data-abort handler reads it
back to redirect a faulting copy instead of panicking. The storage was a
per-thread `[AtomicU64; MAX_THREADS]` in `threading/mod.rs`.

Both the setter (this crate) and the reader (`akuma-exceptions`) would then need
`akuma-exec`, and `akuma-exec` re-exports this crate — a cycle. So the slot moved
**down to `akuma_primitives::preempt`**, next to `PREEMPTION_DISABLED[MAX_THREADS]`
— identical shape, same `current_tid()` indexing, and `akuma-primitives` is the
leaf everything already depends on. `get_user_copy_fault_handler` /
`set_user_copy_fault_handler` are re-exported from `akuma_exec::threading` so
`akuma-exceptions`' four call sites and the thread-slot scrub are unchanged
(the scrub now calls `preempt::clear_user_copy_fault_handler(i)`).

### Edge 2 — `prefault_user_range` (needs `Process`, the AS lock, lazy regions)

`validate_user_range(_, Prefault::Yes)` demand-pages any lazy pages covering the
range so the copy cannot fault. That body resolves the address-space owner
`Process`, takes `owner.address_space.lock()`, reads the lazy-region table and
reads files through `runtime()` — none of which a crate below `akuma-exec` can
name.

It is a **page-fault-shaped operation**, so it stays in `akuma-exec` as
`process/lazy_prefault.rs` (3 `unsafe` blocks: two `map_user_page` installs and
one file-buffer slice). `akuma-user-access` exposes a `prefault_user_range`
forwarder over an `akuma_primitives::OnceCopy<fn(usize,usize)->bool>` hook;
`akuma_exec::init` registers the real body. Unregistered — early boot, host tests
— the forwarder returns `false`, which is **fail-closed**: a caller that needs a
lazy page faulted in gets `EFAULT`, never a copy into a page that is not there.
(`OnceCopy`, not a hand-rolled `AtomicPtr` + `transmute` — that is the mechanism
§4 of the audit established.)

## 3. What moved, what stayed

| | file | `unsafe` |
|---|---|---:|
| **moved** → `akuma-user-access/src/lib.rs` | `global_asm!` + `extern` block; `user_range_ok` / `validate_user_range` / `USER_VA_LIMIT`; `BYPASS_VALIDATION` + `BypassValidationGuard`; `Prefault`; `copy_{from,to}_user{,_with}`, `copy_{from,to}_user_safe`, `read_user_byte`, `write_user_val{,_with}`, `read_user_into{,_with}`, `as_user_bytes{,_mut}`; `copy_loop_differential_sweep`; the 6 host tests | 14 |
| **stayed** → `akuma-exec/src/process/lazy_prefault.rs` | `prefault_user_range`'s body | 3 |

`akuma-user-access` depends on `akuma-primitives` (`EFAULT`, `current_tid`,
`MAX_THREADS`, the fault-handler slot, `OnceCopy`) and `akuma-mmu`
(`is_current_user_range_mapped` — the "mapped and EL0-accessible" walk
`validate_user_range` does). No cycle: `akuma-mmu` does not depend on this.

## 4. Call-site churn: zero outside `akuma-exec`

`akuma-exec` re-exports the crate as `pub use akuma_user_access as user_access;`
inside `pub mod process`, so every `akuma_exec::process::user_access::…` path —
16 in `akuma-exceptions`, 12 in `akuma-syscalls-glue`, the boot suite, the two in
`process/mod.rs` — resolves unchanged. `git` recorded the move as a rename.

## 5. Verification

Full A/B against the parent per `docs/runbooks/verify-trim-fat-change.md`,
`MEMORY=2048`.

**Tier 1:** four clippy configs clean, **1102 host tests / 0 failed** (unchanged
— the 6 `user_range_ok`/bypass tests moved with the crate). akuma-user-access:
6 host tests pass.

**Full A/B (tiers 1–3) vs `276fa15d`:** the two summaries are **identical except
`smp4.bkl_stuck` (120 base / 102 mine)** — the load-driven counter the runbook
says never to compare. Matched on both arms:

| | value |
|---|---|
| `clippy.{release,extreme-size,devbox-smoltcp,devbox-rump}` | clean |
| `host.tests` / `host.failed` | 1102 / 0 |
| `smp{1,4}.booted` | True / True |
| `smp{1,4}.ex.*` (17 each) | all `ok` |
| `smp{1,4}.fail_set` | empty |
| `smp{1,4}.passed_marker` | 310 / 318 |
| `smp{1,4}.host_timejumps` | 0 / 0 (host quiet) |
| `smp{1,4}.stack_overflow` | 1 / 1 (`stackstress` canary) |

A file move plus a fail-closed hook: behavior-preserving, and the A/B confirms it.

## Background

- [`AKUMA_EXEC_AUDIT.md`](AKUMA_EXEC_AUDIT.md) §1, §2, §6 — why this file, and the
  three steps after it.
- [`AKUMA_NET_SPLIT.md`](AKUMA_NET_SPLIT.md) §5.1c — `akuma-net-nic`, the same
  "one crate holds all the `unsafe`, behind one contract" move.
- [`USER_COPY_BYTE_LOOP.md`](USER_COPY_BYTE_LOOP.md),
  [`BUSYBOX_HASH_MISCOMPUTE.md`](BUSYBOX_HASH_MISCOMPUTE.md) — the asm loop's
  history and the register-preservation invariant that guards it.
