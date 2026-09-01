# `akuma-exceptions`: the exception path leaves `src/`

**2026-09-01.** `src/exceptions.rs` (5245 lines, **80 `unsafe` sites**) became
`crates/akuma-exceptions`. This is the move the previous four extractions were
clearing the way for: it takes `src/` production `unsafe` from **91 to 11**.

| Scope | Before | After |
|---|---|---|
| `src/` production `unsafe` sites | 91 | **11** |
| `crates/` production `unsafe` sites | 331 | 411 |
| **tree** production `unsafe` sites | 422 | **422** |
| Crates carrying `#![forbid(unsafe_code)]` | 23 of 38 | 23 of 39 |

The tree total is unchanged, and that is the point: 80 sites moved, none were
added and none were deleted. An extraction that changes the total is doing two
things at once, and the second one is the one that breaks at 3 a.m. Cleaning
the moved blocks is a separate pass against a crate that now has host clippy
coverage.

`akuma-exceptions` is the sixteenth crate that **cannot** forbid `unsafe`, and
it will never be otherwise: a file with a vector table and register-restore
trampolines cannot. What it does instead — the `akuma-net-nic` DMA and
`akuma-gic` MMIO pattern — is state its obligations once, at the top of
`lib.rs`, in five bullets that cover all 80 sites.

## What `src/` has left

```
unsafe sites in src/ (production):   11
  src/main.rs         9   boot entry, DTB location, boot device-table rebuild
  src/boot.rs         1   `unsafe extern "C"` linker symbols
  src/smp_shared.rs   1   `unsafe extern "C"` for the secondary trampoline
```

None of the three can take `#![forbid(unsafe_code)]` as written, and the reason
is the same one `src/smp_shared.rs`'s header records: `forbid` also rejects
`unsafe extern` blocks, `global_asm!` and `#[unsafe(no_mangle)]`, so a file can
reach **zero `unsafe {}` blocks** and still not be able to carry the ban.
`src/smp_shared.rs` is already at zero blocks by that measure. The remaining
work for the `#![forbid(unsafe_code)]`-across-`src/` goal is therefore no longer
about volume — 80 of the 91 were one file — it is about the boot entry point and
two linker-symbol declarations.

## The seam: hooks down, never `crate::` up

The crate reaches kernel-core state exclusively through two registrations, both
installed by `src/main.rs` at boot. Nothing in the crate names `crate::`.

**`ExceptionHooks`** — 13 function pointers, one per thing the handlers need
that lives above them:

| Hook | Owner in `src/` |
|---|---|
| `dispatch_irq` | `src/irq.rs` — the device-IRQ dispatcher and its handler table |
| `handle_syscall`, `current_syscall_nr` | `src/syscall/mod.rs` — the SVC dispatcher |
| `inc_pagefault`, `inc_qemu_dc_zva_ec15`, `inc_qemu_stp_xzr_ec15` | `src/syscall/mod.rs` counters |
| `sys_exit_group`, `notify_child_channel_exited`, `vfork_complete` | `src/syscall/proc.rs` — fatal-signal termination |
| `signal_is_fatal_default` | `src/syscall/signal.rs` |
| `syscall_log_formatted` | `src/syscall/log.rs` — per-process syscall ring |
| `report_poison_value` | the eret poison tripwire |
| `dp_counters_line` | `src/pmm.rs` — demand-paging counter dump |
| `read_profile_span_new`, `read_profile_span_end` | `src/syscall/utils/read_profile.rs` |

**`ExceptionsConfig`** — the nine `src/config.rs` tunables the handlers gate on
(`VERIFY_SVC_AT_ENTRY`, `SIGNAL_TRACE_ENABLED`, the `DEBUG_SIGSEGV_SYSCALL_STUB`
window, …). `src/config.rs` stays the single source of truth; the crate receives
a struct, exactly as `akuma-fpcache` receives `FpcacheConfig`. The same cost
applies: a `const`-folded branch becomes a load.

Applying the `akuma-alloc` test — *if the lower layer would still work correctly
with the hook permanently unregistered, the call did not belong there* — every
one of the 13 fails it, which is what makes them hooks rather than dependencies
the crate should have taken directly.

### The one hook that changed shape

`read_profile` exports an RAII `Span` guard. A `fn` pointer table cannot name
it, so the span was split into two plain functions over a raw start tick —
`exception_span_start() -> u64` and `exception_span_end(u64)` — that travel
separately and reassemble at the far end. Both compile to nothing without the
feature. This is the only place the extraction changed an interface rather than
moving one.

### Call sites did not move

`src/main.rs` carries `use akuma_exceptions as exceptions;`, so every
`exceptions::` spelling in that file reads as it did before. The crate root also
re-exports `safe_print!`, which is what let the moved bodies keep both of their
original spellings (`safe_print!` and `crate::safe_print!`). `tprint!` is
`src/console.rs`'s and did not travel — the timestamp prefix is the only thing
those call sites lost.

## `build.rs`: three forwarded cfgs

Mirroring `akuma-exec`'s scheme, so the gates read identically on both sides of
the move: `smp-shared` → `kernel_smp_shared`, `no-bkl-irq` → `kernel_no_bkl_irq`,
and **absence** of `no-tests` → `kernel_tests`. The last one is the inversion
the bin crate uses; getting it backwards compiles the boot self-test surface out
of the default build, where it is the only thing that tests this crate at all.

## Host builds: the `target_os` trap, again

The crate is in `default-members`, and the pre-commit hook runs
`cargo clippy -p <crate> --target $HOST -- -D warnings` for **every** directory
under `crates/`. Host-buildability is therefore not optional for a crate that
lives here — it is the price of admission, and a `default-members` exclusion
would not have bought an exemption from the hook.

What blocked it, in the order it surfaced:

1. **`sgi_scheduler_handler_with_sp` is `#[cfg(target_os = "none")]`** in
   `akuma-exec` (its switch arm writes `ttbr0_el1`), and the IRQ handler is its
   only caller. Fixed with a paired host stub next to the real one, the
   convention that file already uses for `thread_start`, `idle_halt` and
   `set_current_thread_register`. The stub **panics** rather than returning `0`:
   `0` is the handler's "no context switch needed" answer, so a stub returning it
   would let a caller read a silent no-op as a real scheduling decision.
2. **The vector table's `global_asm!`** — `.section .text.exceptions` is not a
   directive Mach-O accepts, so this one failed loudly on its own.
3. **Three EL1 control-flow register writes that did *not* fail loudly**:
   `msr vbar_el1` (`install_vbar`), `msr tpidr_el1`
   (`set_current_exception_stack`) and the two `msr elr_el1` redirections in
   `rust_sync_el1_handler`.

Item 3 is the trap `akuma-cpu`'s "Host builds" note records, hit again from a new
direction. The development host **is** `aarch64-apple-darwin`, so those three
`msr`s *assemble* under `cargo test`. They are EL1 instructions: the failure
mode is not a compile error but `SIGILL` on the first host test to reach one.
Only the Mach-O directive in item 2 announced itself; had the file contained no
`global_asm!` at all, the crate would have built green on the host with three
live EL1 instructions in it.

Everything bare-metal is now gated on `target_os = "none"`, and the gated set is
small and closed:

- the two `global_asm!` blobs (vector table, `kernel_tests` GPR-transparency
  probe) and their `unsafe extern "C"` declarations,
- the three control-flow register writes above,
- `el1_sync_gpr_clobber_mask`, the probe's Rust half — whose caller in
  `src/process_tests.rs` was **already** `#[cfg(target_os = "none")]`, so the
  gate matches an existing boundary rather than inventing one.

The gates cost **zero** added `unsafe` sites. An early version routed both
`msr elr_el1` sites through one `unsafe fn redirect_elr_el1` helper — better
prose, and it took the crate from 80 sites to 82, because the counter charges
for the `unsafe fn`, its body and both call sites. Gating the two blocks in
place costs nothing and needs no `let _ =` binding, since both sites' operands
are already read by the surrounding condition. The `install_vbar` precedent (two
independently-written copies of one `msr`, deduplicated the same day) does not
apply: these two redirections target different things and sit 40 lines apart in
one function.

To be explicit about what the host build is and is not worth: there are **no
host tests here**, and there will not be — a trap handler needs a trap. What it
buys is that type checking and `-D warnings` cannot silently stop applying to
the exception path, which is otherwise reachable only by booting a VM.

## Verification

- Pre-commit hook green end to end: clippy `-D warnings` on all 39 crates at
  `$HOST`, on `akuma` release, and on `akuma` `extreme-size`
  (`--no-default-features`).
- 76 host test suites, 0 failures.
- Boot self-test suite, `MEMORY=2048`: **306 PASSED / 0 FAILED** at `SMP=1`,
  **314 PASSED / 0 FAILED** at `SMP=4`.
- `test_el1_sync_exception_preserves_gprs` passes **non-vacuously** on both
  (`1 abort(s), x4-x18 intact`) — the assertion that the EL1 vector is
  transparent to x4–x18 is the one that would catch a botched move of the vector
  table itself, and it reports `FAIL` rather than `PASS` if no abort fires.
- `cargo check` clean on the devbox-smoltcp feature set
  (`--no-default-features --features smoltcp,userspace-sshd,no-tests`).
- `SMP=4` shows 104 `[BKL] stuck: … tag=511` lines. Pre-existing and
  load-driven, not a regression — `tag=511` is only "profiler off"
  ([`docs/README.md`](../README.md) triage rows), and an earlier A/B against a
  HEAD worktree measured 91 vs 90 lines with both arms booting. Measure it as a
  rate, never a boolean; a second VM was competing for host CPU during this run.

## Background

- [`AKUMA_FPCACHE_EXTRACTION.md`](AKUMA_FPCACHE_EXTRACTION.md) — the move made
  explicitly to shed one of the eight `crate::` clusters blocking *this* one,
  and the source of the census that ranked `exceptions.rs` at 79% of `src/`.
- [`AKUMA_GIC_CONSOLIDATION.md`](AKUMA_GIC_CONSOLIDATION.md) — the sibling crate
  that also cannot forbid `unsafe`, and the "one stated contract" shape this
  crate's safety header copies.
- [`AKUMA_SMP_SHARED_SPLIT.md`](AKUMA_SMP_SHARED_SPLIT.md) — how
  `src/smp_shared.rs` reached zero `unsafe` blocks while still being unable to
  carry the ban, the distinction this doc's "What `src/` has left" leans on.
- [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — `src/syscall/`, the
  first enforced ban outside `crates/`, and the reason the crate tally and the
  ban tally differ.
- [`crate-safety.md`](../reference/crate-safety.md) — the current census.
