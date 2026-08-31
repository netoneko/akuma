# Splitting `src/smp_shared.rs`

**2026-09-01, commit `b9a876fb`.** The BKL policy toggles moved to
`akuma_bkl::policy`. `src/smp_shared.rs` went **1173 → 808 lines**.

This is step one of a split, not the whole thing, so the second half of this doc
is an inventory of what is left and what it is waiting on.

## Why `smp_shared.rs` is being split at all

Not for host tests — for `#![forbid(unsafe_code)]` across `src/`. The census:

```
unsafe { } blocks in src/ (non-test):   97
  src/exceptions.rs                     77
  src/main.rs                            7
  src/gic_v3.rs                          5
  src/gic.rs                             4
  src/smp_shared.rs                      4
```

`exceptions.rs` is the whole goal, and it cannot leave `src/` while it names
eight `crate::` clusters that live there. `crate::smp_shared` was one of them —
6 references, 5 distinct functions.

## What moved

Seven `no-bkl-*` phase toggles and the per-syscall opt-out bitmap:

| Moved | Shape |
|---|---|
| `fault` / `exec` / `vfs` / `mm` / `drivers` / `irq` `_bkl_drop_enabled` | `AtomicBool` + relaxed load + A/B setter |
| `sched_bklfree_el0_enabled` | same |
| `syscall_bkl_optout` / `set_syscall_bkl_optout` | `[AtomicU64; 8]` bitmap, `const fn` seed, structural deny list |

They are pure policy state with no `unsafe`, and the crate that owns the BKL
protocol is where the decision to take or skip it belongs. `akuma-bkl` gained
one dependency, `akuma-syscalls-linux`, so the seed table can name syscalls
(`nr::SOCKET`) instead of carrying bare numbers with a comment that can drift.
That crate has zero dependencies, so it cannot cycle back.

`src/smp_shared.rs` re-exports all sixteen names, so no call site changed.

### What deliberately stayed

`process_bkl_drop_enabled` / `set_process_bkl_drop_enabled`. Their atomic lives
in `akuma_exec::process::bkl_guard` because the guard is constructed inside
`fork_process`; `akuma-exec` depends on `akuma-bkl`, so moving the toggle down
would invert that edge. They remain forwarders.

### The kernel now depends on `akuma-bkl` directly

`src/` had reached the BKL only through `akuma_exec::{bkl, sync}`. Routing a
policy module through the exec crate that does not read it would be indirection
for its own sake, so the root manifest names `akuma-bkl` itself. Cargo dedups;
the two paths are the same crate.

## What the move bought: seven tests that could not exist before

The opt-out bitmap has real logic — a range bound, `nr / 64` word indexing,
`1 << (nr % 64)` bit math, a `const fn` seed, and a deny list — and none of it
had a test, because it lived in a bin crate. It now has:

- out-of-range (`512`, `u64::MAX`) reads false **and** refuses the write
- the structural deny list (`exit`, `exit_group`, `rt_sigreturn`) cannot be set
- set/clear round trip
- **63 vs 64** — the word-0-bit-63 / word-1-bit-0 boundary that a shift by `nr`
  instead of `nr % 64` aliases
- **447 vs 448** and bit **511**, the top word and the last valid bit
- the `const fn` seed is actually applied (a silent zero here boots
  correct-but-slow, with no symptom)
- all seven phase toggles default on

**Trap for whoever adds one:** the bitmap is a `static`, so `cargo test`'s
thread pool races two tests that touch one word. Every mutating test owns a
number on neither the seed nor the deny list. `300` looks free and **is
seeded** — the free set is pinned in a comment beside the tests.

## Verification

Byte-identical `extreme-size` image (724,328 B both arms) — these were already
atomics, so unlike the `akuma-fpcache` move there was no const-fold to lose.

Host: 1094 → 1101 tests, 0 failed. Clippy clean on host, `--release`, and
`extreme-size`.

The real check is that the toggles still *reach* their paths. The kernel's own
boot self-tests A/B them, and all passed at SMP=4 and SMP=2, 0 FAILED anywhere:

```
smp_shared_fault_parallelism: BKL-spins drop_OFF=797046 drop_ON=165663
  PASSED (BKL wait reduced by 631383 spins with drop ON)
syscall_bkl_optout PASSED (list + latch + ledger + pause + dead-thread clear)
fork-bkl-drop PASSED (... 133 pages shared/demoted x2 toggles ...)
mm-bkl-drop / drivers-bkl-drop PASSED (... + kill switch)
no_bkl_ticket_recoveries PASSED (0 BKL ticket self-heals)
```

`fault_parallelism` is the one that matters: it flips the toggle off and on and
measures a 4.8x difference in BKL spins. A stubbed or stale read would show
identical numbers on both arms. Note it is **SKIPPED at SMP=4** — it runs only
at SMP=2, so an SMP=4-only run leaves `fault_bkl_drop_enabled` unverified.

## What is left in `smp_shared.rs`, and what it is waiting on

808 lines in six clusters. **Destinations are deliberately not decided here** —
see the note after the table.

| Cluster | Roughly | `unsafe` | Notes |
|---|---|---|---|
| PSCI + secondary bring-up | `psci_call`, `psci_is_hvc`, `probe_dtb`, `bringup_secondaries`, `system_reset`/`system_off`, `secondary_stack_base`, `set_shared_vbar` | **1** (the SMC/HVC conduit call) | The genuine core of the file. `akuma-boot` already owns the `reboot(2)` ABI decode and is named for a wider remit |
| Per-PE GICv3 receive path | `mmio_w32`/`mmio_r32`, `secondary_gic_init`, the `GICR_*` constants | **3** | See below |
| Idle mask / cross-core wake | `CORE_IDLE_MASK`, `set_core_idle`, `wake_remote_idle`, `wake_core` | 0 | Scheduler-adjacent |
| Per-core diagnostics | `CORES_SEEN`, `CORES_SEEN_USER`, `record_el0_trap`, `cores_that_ran_userspace`, `user_traps` | 0 | The other half of `exceptions.rs`'s `crate::smp_shared` cluster (2 refs) |
| SMP demo / self-test workers | `migration_worker`, `smp_worker`, `blocking_relax_waiter`, their spawners and counters | 0 | ~130 lines of scaffolding |
| BKL forwarder | `process_bkl_drop_enabled` | 0 | Stays, by construction (above) |

**GIC is consolidated first, before any of the rest is decided.** Three of the
four remaining `unsafe` blocks are the GIC cluster, and it is already duplicated
across the tree:

- `mmio_w32` / `mmio_r32` exist **verbatim twice** — `src/smp_shared.rs:384-400`
  and `src/gic_v3.rs:90-98`. The `smp_shared` copy's own comment says "same
  reasoning as `gic_v3::mmio_w32`", so the duplication was known and recorded
  rather than accidental.
- `GICR_WAKER_PROCESSOR_SLEEP` / `GICR_WAKER_CHILDREN_ASLEEP` are defined in
  both files.
- `src/gic.rs` (4 `unsafe`) and `src/gic_v3.rs` (5 `unsafe`) are a third and
  fourth home for the same hardware.

Consolidating those first means **12 `unsafe` blocks** across three files get one
destination, and `smp_shared.rs` drops to a single `unsafe` — the PSCI conduit
call. Deciding homes for the other four clusters before that would be deciding
against a picture that is about to change.

`record_el0_trap` specifically is **not** an `akuma-cpu` candidate, despite
containing an `mpidr` read: `read_mpidr` is already a three-line wrapper over
`akuma_cpu::sysreg::mpidr_el1()`, so nothing instruction-shaped is left in it.
What remains is a per-core diagnostic counter array read by one boot self-test,
and `akuma-cpu` is the crate of *instructions that are safe to execute* — a
static counter array would be the first thing in it that is not one.

## Background

- [`AKUMA_FPCACHE_EXTRACTION.md`](AKUMA_FPCACHE_EXTRACTION.md) — the previous
  step toward the same goal, and the source of the `-D unused-imports` trap:
  `cargo check --release` cannot catch it, only the `extreme-size` arm can.
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) — the
  phases these toggles gate, and the A/B discipline the setters exist for.
- [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — the `src/syscall/`
  precedent: move the operation to the crate that owns what it pokes.
- [`AKUMA_FDT_EXTRACTION.md`](AKUMA_FDT_EXTRACTION.md) — took `smp_shared.rs`
  from 8 `unsafe` blocks to 4, immediately before this.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — unchanged by
  this move (23 of 36 crates; `src/` 104, `crates/` 324), because the moved code
  held no `unsafe`.
