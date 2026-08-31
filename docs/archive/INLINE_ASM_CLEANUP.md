# Migrating the tree onto `akuma-cpu`

**2026-08-31.** `akuma-cpu` was extracted in `3f5b961e` with the full instruction
surface and a crate header arguing the case — and then nothing was moved onto it.
One crate (`akuma-pmm`, 4 call sites) used it; the other 218 `asm!` sites in the
tree still open-coded their own `unsafe` block. This is the migration.

**Result: 218 `asm!` invocations outside `akuma-cpu` became 35.** Tree `unsafe`
sites fell **645 → 543**, of which production fell **518 → 455**.

---

## 1. The argument (restated, because it decides what moves)

`core::arch::asm!` is unconditionally `unsafe`. So `dsb ish` — an instruction
that orders accesses and dereferences nothing — needed the same syntactic
ceremony as `msr ttbr0_el1`, which swaps the address space out from under every
live pointer. An `unsafe` block that is always trivially discharged is worse than
no block at all: it trains the eye to skip exactly the construct that is supposed
to stop it.

So the test for "does this move" is **not** "is it an instruction". It is:

> Can executing this violate memory safety, for *any* argument the type allows?

`dsb`, `isb`, `tlbi`, `dc cvau`, `ic ivau`, `wfi`, `wfe`, `sev`, `nop` and every
`mrs` of a read-only register answer no. A `tlbi` on a garbage VA only forces a
re-walk; a `dc cvau` on an unmapped VA faults or is ignored per the architecture,
it does not read through the pointer. That is what makes an arbitrary `usize` a
safe argument, and it is why these are safe *functions*, not `unsafe fn`s with a
`# Safety` section nobody can discharge.

## 2. What did not move, and why

| Stayed `unsafe` | Because |
|---|---|
| `msr ttbr0_el1` | swaps the address space under live pointers |
| `msr elr_el1` / `spsr_el1` | redirects where the CPU returns to (resolve-and-retry) |
| `msr vbar_el1` | installs the vector table |
| `msr tpidr_el1` / `tpidrro_el0` / `tpidr_el0` | re-points every per-thread static, or userspace's whole TLS |
| `mov sp, x`, `mov x30, x` | retarget every later stack access, and the next `ret` |
| `dc zva` | unlike every other `dc`, it **writes** the block it names |
| raw `ldr` / `str` (device MMIO) | a real dereference of a caller-supplied address |
| GICv3 `ICC_*` writes | gate interrupt delivery for a whole PE |
| PSCI `hvc` / `smc`, `adrp` symbol loads, `hlt` semihosting, `global_asm!` | not single safe instructions |

**Reading `TTBR0_EL1` is in the crate; writing it is not.** That asymmetry is the
whole design, and it mirrors the seam `akuma_primitives::preempt` already
documents for `TPIDRRO_EL0`: reading moves, writing does not.

The 35 remaining sites are exactly this table. Nothing was left behind by
oversight — the count is the exclusion list.

## 3. The two judgment calls

Two modules sit **inside** the line even though both change control flow, so both
look excludable. Recorded here because the reasoning is the part that is easy to
get wrong later.

### `daif` — the interrupt mask

The danger of `unmask_irq` is unmasking *inside* a critical section that assumed
IRQs were off. That is a **lock-discipline property of the surrounding code**, not
of the instruction, and no `unsafe` block at the call site could discharge it —
the block cannot see the section it sits in. What actually enforces it is
`akuma_primitives::irq::IrqGuard`, a scope whose `Drop` restores the saved `DAIF`.

Decisively: that crate has presented all six of these as **safe functions** since
long before `akuma-cpu` existed. The safety judgement was made in-tree years ago;
what moved is the `asm!`, not the judgement. `akuma-primitives`' asm-derived
`unsafe` went 14 sites → 4.

### `vtimer` — the comparator

Same shape: arming a deadline the tick policy did not choose is a policy bug, not
a soundness one. `akuma-timer` owns the policy; the crate owns the two
instructions. Its `unsafe` went 8 sites → 1 (the remaining one is
`arm_pl031::Rtc::new`, a genuinely `unsafe fn` from a dependency).

## 4. What moved

| | `asm!` before | after |
|---|---:|---:|
| `src/exceptions.rs` | 48 | **4** |
| `src/tests.rs` + `process_tests.rs` | 59 | **6** |
| `crates/akuma-mmu` | 33 | **3** |
| `src/smp_shared.rs` | 18 | **11** |
| `crates/akuma-exec` | 15 | **4** |
| `crates/akuma-primitives` | 11 | **0** |
| `crates/akuma-timer` | 9 | **0** |
| `src/syscall/mem.rs` | 8 | **0** |
| `src/gic_v3.rs` | 7 | **5** |
| everything else | 10 | **2** |

`unsafe` sites, from `scripts/cloc_akuma.py src crates`:

| scope | before | after | production before → after |
|---|---:|---:|---|
| `crates/` | 330 | **304** | 319 → 293 |
| `src/` | 315 | **239** | 199 → 162 |
| tree | 645 | **543** | 518 → **455** |

Production density fell from 9.8 to **8.6 `unsafe` sites per kloc**. Binary got
**4,408 bytes smaller**; no crate gained `forbid(unsafe_code)` (the ones that
came closest — `akuma-timer`, `akuma-primitives` — are each blocked by one
genuine `unsafe`, an RTC constructor and `mmio`/`MmioReg` respectively).

### Additions to the crate

`tlb::aside1`, `reg::{sp, lr}`, the `daif` and `vtimer` modules, and
`sysreg::{tpidr_el0, fpcr, cntvct_el0_ordered}`.

`cntvct_el0_ordered` fuses `isb` + `mrs cntvct_el0` in one `asm!` so no optimiser
can separate them. It exists because a bare counter read is not ordered against
the work being measured — on this project that made an 8 KB `copy_to_user`
measure as **0 ns**. `src/syscall/utils/read_profile.rs` was open-coding exactly
that fusion; now the lesson lives where the next caller will find it.

## 5. Verification

- **Host unit tests**: ~1,050 across all crates, green.
- **Kernel build**: `--release`, `extreme-size` (`--no-default-features`, which is
  what exercises the non-`smp-shared` arms of the `flush_tlb_*` restructure), and
  `--features platform-firecracker`. Clippy clean on all three.
- **QEMU boot A/B** (`MEMORY=2048M SMP=2`): baseline and migrated both
  **165 `Result: PASS` / 0 FAIL / 316 `PASSED` / 0 `FAILED`**, and both emit the
  same 29 pre-existing `[BKL] stuck: owner=1` contention lines.
- **Firecracker boot A/B** (Lima `fc`, KVM, 1 vCPU): FDT device map correct
  (`GICR=0x3ffd0000`), 1022 MB detected, boot suite runs, herd starts httpd.
  See §6 for the flaky test this turned up.
- **Codegen diff**: disassembled both kernels and diffed barrier/park mnemonics
  per symbol. Every delta is an inlining-boundary move that cancels (`-8 wfi`
  from `policy::pick_tick`, `+8` into `timer::probe_host_tick`); the `flush_tlb_*`
  helpers are fully inlined at every site and have no standalone symbol to diff,
  which is why the boot A/B above is what actually carries this claim.

## 6. Three traps, all of them method

**(a) `Assertion failed: (isv) … hvf.c` is a RAM problem, not a kernel problem.**
The first boot died there. It is documented (`QEMU_HVF_ISV_BUG.md`, and a row in
`docs/README.md`'s symptom matrix): `src/tests.rs`'s user-copy trampoline test
deliberately copies off the end of mapped kernel memory, and below `MEMORY=2048`
that cliff lands where HVF cannot resolve ISV. The kernel was fine. **Read the
symptom matrix before diagnosing a boot failure.**

**(b) n=1 per arm produced a confident wrong answer.** Firecracker's first
baseline run was 0 `FAILED` and the first migrated run was 1 — `thread_slot_
reclaim_on_spawn`, in the timer area this change touches. That looked exactly
like a regression, and a fix was already being reasoned toward. Three more runs
per arm:

| arm | runs | failed |
|---|---:|---:|
| baseline | 4 | 2 |
| migrated | 4 | 3 |

The test is **flaky on Firecracker at 1 vCPU on both arms**, `hot_reclaim`
ranging 68–133. Not a regression. This is the third time this repo has recorded
the same lesson (`project_stress_ab_needs_deterministic_probe`, the 10-runs-per-arm
rule); it cost another hour here.

Worth noting for whoever fixes the flake: the test can only fail as it does if
those slots' `TERMINATION_TIME` was `0`, since `reclaim_terminated_slots` skips
the cooldown check entirely when `term_time == 0`. The arithmetic rules out a
genuine cooldown-timing violation — every terminated slot's timestamp is `>=
run_loop_start`, so `now - term_time <= now - run_loop_start`, which the test has
already proved is under the 10 ms window. **The bug is a missing timestamp, not a
short cooldown**, and the test's message ("in_cooldown_window=true") points away
from that.

**(c) A real divergence, found by looking rather than by a test.** The `vtimer`
setters were first written with `options(nomem, nostack)`, copied from the
`sysreg` read macro. The `asm!` they replaced in `akuma-timer` and `src/timer.rs`
carried **no options at all** — the most conservative contract available — so
this silently licensed the optimiser to move a comparator write across the stores
the tick path publishes alongside it. `nomem` is right for `mrs esr_el1`, whose
value does not depend on memory; it is wrong for an observable device effect.
Fixed to `options(nostack)`. No test caught this and none would have; it was
caught by asking what the old options were.

## 7. `cloc_akuma.py` now counts `src/` too

The `Unsafe by crate` table filtered to `crates/*` on the grounds that only a
library crate can be `forbid`-enforced. True — and it meant **the tree's single
largest concentration of `unsafe` never appeared on the one table that measures
`unsafe`**. `src/` is now listed, marked `bin` in the enforced column, and the
counter emits the production/test split per scope that
`docs/reference/crate-safety.md` had been assembling by hand from two runs.

The patched counter reproduces the old hand-written `src/` row (315 / 199 / 116)
exactly on the pre-migration tree, which is what validates it.

`enforced unsafe-free ... N of M crates` still counts `crates/*` only: a bin crate
was never a candidate, and folding it in would have made that ratio worse for no
reason.

---

## Background

- `docs/reference/crate-safety.md` — current per-crate state, regenerated
- `crates/akuma-cpu/src/lib.rs` — the crate header carries the argument
- `docs/archive/QEMU_HVF_ISV_BUG.md` — §6(a)
- `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §7.9 — the `akuma_primitives::cpu`
  precedent for moving one `asm!` to let a crate reach `forbid`
