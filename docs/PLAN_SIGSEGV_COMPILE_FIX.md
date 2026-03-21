# Plan: `compile` SIGSEGV / bad fault PC on Akuma

This document describes a staged fix strategy for the failure mode seen when
building Go on Akuma: **`SIGSEGV` (11)** with **`fault_pc` around `0x6006…`**,
sometimes followed by teardown reported as **exit code 137** (see
`GOLANG_IPC.md`).

## What we already know

- **`fault_pc` in `[signal] deliver` logs** is the **saved user PC at fault
  time**, not the handler address. Values in **`0x4000_0000`–`0x7FFF_FFFF`**
  are **kernel identity RAM** in the user page tables (normally **UXN**):
  execution there causes an **instruction abort** before the handler runs.
- **Handler and restorer `RX`** are already applied in `try_deliver_signal`
  (`update_page_flags` for handler page and restorer page) in
  `src/exceptions.rs`.
- **`[JIT] IC flush + replay`** in `rust_sync_el0_handler` addresses **stale
  instruction cache** for **bogus syscall numbers** after generated code
  changes; a similar class of bug may hit **instruction aborts** from EL0.

## Goals

1. Reduce **spurious** instruction aborts when the only issue is **cache
   coherency** after **PTE permission** or **code** changes.
2. Add **kernel-level regression tests** so future changes cannot silently
   break signal delivery or `RX` promotion.
3. Keep **observability** so we can tell “bad PC / corruption” from “cache
   stale”.

---

## Phase 1 — I-cache maintenance after `update_page_flags` (handler, restorer)

**Hypothesis:** Promoting a page to **RX** updates the **PTE** and **TLB**
(`flush_tlb_page` in `UserAddressSpace::update_page_flags`), but the
**instruction cache** may still hold entries for that VA from an earlier
mapping or alias, so the CPU can still fault or execute stale bytes.

**Change (kernel):**

- In `try_deliver_signal`, **after** successful `update_page_flags` for
  `handler_va` and `restorer_va`, invalidate the **I-cache** for that page’s
  KVA (same pattern as demand-paged file text in `exceptions.rs`: loop
  `0..4096` step `64` with `ic ivau` on `phys_to_virt(pa) + off`), then
  `dsb ish` + `isb`.
- Factor a small helper (e.g. `mmu::invalidate_icache_for_user_page(va)`)
  implemented once to avoid duplicating the stride loop.

**Risk:** Low. Cost: bounded per signal delivery.

**Kernel tests:**

| Test | Location | What it checks |
|------|----------|----------------|
| `test_pte_rx_clears_uxn_after_update` | `src/process_tests.rs` (or new `src/mmu_tests.rs`) | Map a user page **RW**, then `update_page_flags(va, RX)`; **read L3 PTE** and assert **UXN** is cleared (requires a **test-only** `UserAddressSpace` accessor, e.g. `pub(crate) fn test_read_l3_flags_for_va(&self, va) -> Option<u64>` gated by `CONFIG_IN_KERNEL_TESTS` or `cfg!(feature = "kernel-tests")`). |
| `test_icache_invalidate_runs` | `src/process_tests.rs` or `crates/akuma-exec/src/kernel_tests.rs` | After the helper exists, call it for a known-mapped page and assert **no panic** / deterministic completion (smoke). |

**Implementation note:** If adding a PTE reader is too invasive for phase 1,
ship **Phase 1** without the PTE read test and add the test in the same PR as
the accessor.

---

## Phase 2 — Instruction-abort “replay” (QEMU / stale translation)

**Hypothesis:** On QEMU TCG, **translated blocks** can go stale when **user
  code** writes new instructions (same idea as the existing **JIT syscall
  replay** block).

**Change (kernel):**

- In `EC_INST_ABORT_LOWER`, for **permission** or **instruction** faults that
  look like **executable code** (e.g. FAR in a **file-backed** or **anonymous
  R-X** region), optionally retry **once** with **`ic iallu`** + **`dsb ish`**
  + **`isb`** and **re-execute** the faulting instruction (ELR unchanged), with
  a **per-thread or static counter** to avoid infinite loops (mirror the
  syscall JIT retry cap, e.g. ≤16).

**Risk:** Medium — must **not** retry on **genuine** UXN faults at
`0x6000…` (kernel RAM execute). **Gate** on FAR **not** in the kernel
identity-RAM range used for user mappings, or only on **permission fault** where
PTE was just fixed.

**Kernel tests:**

- **Unit:** `test_el0_ia_fair_not_retried_for_kernel_ram_va` — pure function
  that encodes “should retry” policy given FAR + ISS (table-driven asserts).
- **Integration:** Manual — `CGO_ENABLED=0 go build` on QEMU; no automated
  kernel test without fault injection.

---

## Phase 3 — PTE read helper + regression tests for `update_page_flags`

**Goal:** Lock in the **RX** semantics (including **UXN** clearing) independent
of signal delivery.

**Change:**

- Add `UserAddressSpace::read_l3_entry_flags(va) -> Option<u64>` (or
  name by purpose) for **internal tests + debug** only.
- Tests in **`src/process_tests.rs`** (same runner as other signal tests):

  - `test_update_page_flags_rw_to_rx`
  - `test_update_page_flags_idempotent_rx`

---

## Phase 4 — Signal frame + `SA_SIGINFO` invariants

**Goal:** Ensure **`x1`/`x2`** and **frame layout** stay Linux-compatible when
Go uses **`SA_SIGINFO`**.

**Existing coverage:** `test_rt_sigreturn_restores_registers` and
`test_sigframe_layout_constants` in `sync_tests.rs` / `process_tests.rs`.

**Additional tests:**

| Test | What it checks |
|------|----------------|
| `test_sigframe_siginfo_ucontext_offsets` | `SIGFRAME_SIGINFO` / `SIGFRAME_UCONTEXT` / `SIGFRAME_MCONTEXT` match `exceptions.rs` constants (already partially covered — extend if new fields added). |
| `test_sa_siginfo_sets_x1_x2` | If `try_deliver_signal` is refactored to a **pure** “compute register updates” helper, unit-test that for `SA_SIGINFO`, `x1`/`x2` equal `new_sp + off` (optional refactor). |

---

## Phase 5 — Observability (optional, low cost)

- Increment a **per-CPU or global counter** when **`EC_INST_ABORT_LOWER`**
  fires with FAR in **`0x4000_0000`–`0x7FFF_FFFF`** (or log once with rate
  limit) to separate **“execute kernel RAM”** from **PIE code** faults.
- Expose in **`/proc`** or a debug syscall only if needed for bring-up.

---

## Execution order (recommended)

1. **Phase 1** (invalidate I-cache after `RX` for handler/restorer) + **Phase 3**
   PTE tests if the accessor lands quickly.
2. **Phase 2** only if Phase 1 does not fix **`go build`**; keep retries
   narrowly scoped.
3. **Phase 4** as follow-up hardening.
4. **Phase 5** if still debugging.

---

## Success criteria

- **`CGO_ENABLED=0 go build -o ./hello_go .`** completes on Akuma QEMU for a
  small module (e.g. `hello` package).
- New **kernel tests** pass in the normal in-kernel test suite (`run_all` /
  process tests path used today).
- No increase in **spurious** retries that hide real **memory safety** bugs
  (Phase 2 must be gated).

---

## References

- `src/exceptions.rs` — `try_deliver_signal`, `rust_sync_el0_handler`, JIT replay
- `crates/akuma-exec/src/mmu/mod.rs` — `update_page_flags`, `flush_tlb_page`
- `src/process_tests.rs`, `src/sync_tests.rs` — existing signal / sigframe tests
- `docs/GOLANG_IPC.md` — exit **137**, log interpretation
