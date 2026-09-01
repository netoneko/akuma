# `akuma-entry`: giving `akuma-kernel-glue` its `forbid`

**Date:** 2026-09-01
**Outcome:** `akuma-kernel-glue` (2416 production lines) carries
`#![forbid(unsafe_code)]`. Enforced-crate coverage 59.5% -> 63.4% of the tree.
Total `unsafe` sites unchanged at 426.

## 1. What started it

A row in `scripts/cloc_akuma.py`'s output:

```
crates/akuma-kernel-glue         3220      3       3   99.9%    forbid
```

The crate does **not** forbid `unsafe` — its own header said so — yet the table
marked it as if it did, while also reporting three `unsafe` sites inside it. The
script knew this was contradictory and said so two lines later:

```
  !! non-zero inside a `forbid(unsafe_code)` crate — check the counter
```

The counter was right. The mark was wrong. `CrateAgg.add` set the crate's flag
if **any** file in it carried the attribute, and exactly one did:
`console.rs`, which held a module-level `#![forbid(unsafe_code)]` because it is
the file that has to keep working when the allocator is what broke. A
module-level ban covers its own module tree; the boot assembly two files over is
untouched by it.

So there were two separate things to fix — a reporting bug, and the fact that a
3220-line crate sitting one hop from `kernel_main` could not take the ban.

## 2. What was actually blocking the ban

Not `unsafe` *operations*. `src/smp_shared.rs` had already reached zero `unsafe`
blocks on 2026-09-01 (`AKUMA_SMP_SHARED_SPLIT.md`). What remained were four
things the `unsafe_code` lint rejects on sight, three of them link-level:

| site | construct |
|---|---|
| `boot.rs` | `global_asm!` (`_boot`), `#[unsafe(no_mangle)] #[unsafe(link_section)]` |
| `smp_shared.rs` | `global_asm!` (secondary trampoline), `unsafe extern "C"`, `#[unsafe(no_mangle)]` |
| `lib.rs:576` | `unsafe extern "C"` — `linker.ld`'s `_kernel_phys_end` / `STACK_BOTTOM` / `STACK_TOP` |
| `lib.rs:661` | `akuma_fdt::locate(dtb_ptr)` — the one genuine `unsafe {}` block |

Verified against rustc rather than assumed — all three constructs are rejected
under edition 2024, `global_asm!` with the explicit note *"using this macro is
unsafe even though it does not need an `unsafe` block"*.

`forbid` is all-or-nothing at the crate root, so all four had to leave.

## 3. Rejected: move it into `src/`

The first proposal was to push the link-level `unsafe` back down into the bin
crate, on the precedent `src/main.rs` already sets for `#[global_allocator]` and
`#[alloc_error_handler]` ("binary-level declarations, so they stay here rather
than in `akuma-alloc`"). It is a real argument: `src/` **cannot** forbid anyway,
because `rust_start` needs `#[unsafe(no_mangle)]` for the boot assembly to `bl`
it. Moving link-level unsafe there costs a count, not a guarantee.

It was rejected for the `smp_shared` half specifically. `secondary_shared_start`
is ~100 lines of real logic (idle-thread adoption, GIC bring-up, tick arming),
and `akuma-entry` cannot depend on the bin crate, so splitting the
`#[unsafe(no_mangle)]` shim from its body would have needed a hook table
back — the `BootTestHooks` pattern — for two values (`entry_pa` and
`secondary_stack_base`). A crate boundary is cheaper and states the contract
once.

## 4. Rejected: `akuma-boot`, and `akuma-psci`

Both were proposed as homes, `akuma-boot` on the strength of its own
description ("a natural home for src/boot.rs's logic later").

- **`akuma-boot`** is 158 lines with **one** dependency (`akuma-psci`) and
  carries `#![forbid(unsafe_code)]`. `smp_shared` needs `akuma-exec`,
  `akuma-gic`, `akuma-exceptions`, `akuma-bkl`, `akuma-fdt`, `akuma-cpu`. It is
  also `optional = true`, pulled in by the `sc-reboot` feature — so every
  `sc-reboot` build would drag half the kernel through a feature edge, and boot
  assembly cannot be optional. And it would cost the crate its ban, which is the
  exact trade the tree already refused once: `akuma-psci` exists as a *sibling*
  of `akuma-boot`, not part of it, precisely so the `smc`/`hvc` does not.
- **`akuma-psci`** is worse for the same reason, one hop lower: `akuma-boot`
  sits on top of it, so the dependency bloat arrives at the same place having
  also violated `akuma-psci`'s deliberate minimalism.

The `-glue` suffix was considered and dropped: in this tree `X-glue` pairs with
`X` (`akuma-syscalls-glue` -> `akuma-syscalls`, `akuma-vfs-glue` -> `akuma-vfs`),
and this crate has no `akuma-entry` beneath it.

## 5. The split as landed

```
akuma-kernel-core   (forbid)      console, platform, timer, config, pmm, irq, …
        ^
akuma-entry         (NO forbid)   boot asm, secondary trampoline, linker syms
        ^
akuma-kernel-glue   (forbid)      kernel_main, rump_proxy, syscall, vfs
        ^
src/main.rs         (bin)         #[unsafe(no_mangle)] rust_start, panic, allocator
```

Four moves:

1. **`boot.rs` + `smp_shared.rs` -> `akuma-entry`** (589 lines). Verbatim, except
   `crate::{console,platform,timer}` becoming `akuma_kernel_core::…`.
2. **`console.rs` + `platform.rs` -> `akuma-kernel-core`** (+282 lines under the
   ban). Neither holds any `unsafe` — `console`'s three PL011 accesses became one
   call into `akuma-uart` in `AKUMA_UART_EXTRACTION.md`, and `platform` is
   machine constants plus FDT parsing. They were on the wrong side of the ban by
   accident of where `src/` was cut. This move is also what breaks the cycle: it
   is why `akuma-entry` can depend *down* on `akuma-kernel-core` for the console
   the trampoline prints to.
3. **The linker symbols -> `akuma-entry::linker_syms`**, as safe accessors.
   Reading a linker symbol's *address* never needed `unsafe` — `&raw const` is
   safe — only naming it in an `extern` block did. `src/process_tests.rs` carried
   a second, byte-identical declaration of the same three symbols; it now calls
   the accessors, removing one `unsafe extern` from `src/` as well.
4. **`akuma_fdt::locate` -> `akuma_mmu::with_boot_identity_fdt`**. See below.

## 6. The FDT site, and why it did not go to `akuma-entry`

`locate` is `unsafe` because it speculatively reads eight bytes at an address
nothing has validated. The call could not be hoisted out of `kernel_main`: it has
to run *after* `mmu::ensure_boot_identity_covers` and *before* heap init, because
on large-RAM configs the heap is placed on top of the blob.

`akuma-mmu` owns the boot identity map, so it can **check** rather than vouch.
`ensure_boot_identity_covers` now returns `bool` — "is `addr` readable through
the boot identity map on return" — and `with_boot_identity_fdt` refuses to read
when it is not. An out-of-range pointer, or one arriving after `init` has closed
the boot-table window, yields `None` instead of a translation fault before the
console has said anything about memory.

The first draft added a separate `boot_identity_covers` probe that re-walked the
table with two more `unsafe` blocks. Folding the answer into the existing
traversal removed both: the net `unsafe`-site count for the whole refactor is
**zero**.

The closure form is load-bearing beyond safety. The old code scoped the blob in a
bare block with a comment claiming the `'_` lifetime enforced the ordering. It
did not: `locate<'a>(pa) -> Option<Dtb<'a>>` returns an **unbounded** lifetime,
so nothing in the type system stopped a `Dtb` from outliving its bytes. The
closure is what actually bounds it.

## 7. Two bugs found on the way

- **`platform.rs`** carried a broken intra-doc link to `crate::fdt_devices`, a
  module that does not exist.
- **`ensure_boot_identity_covers`** had exactly one caller left in the tree, so
  the return-type change was free.

## 8. The reporting fix

`FileCount.forbids_unsafe` now requires the file to be a crate root
(`lib.rs`/`main.rs`). A module-level ban is no longer counted at all — it
understates the crate rather than overstating it, which is the safe direction.
A two-state display (`forbid` vs `mod-forbid`) was written and then removed: the
tree has **zero** module-level bans left, `console.rs`'s having been the last,
and a display category with no members is not worth its own code path.

Note for anyone reading older docs: `src/syscall/`'s module-level ban, described
in `CLAUDE.md` as "the first one outside `crates/`", is a crate-root ban now —
that code lives in `akuma-syscalls-glue`.

## 9. Numbers

| | before | after |
|---|---|---|
| crates | 46 | 47 |
| crates with a real crate-level ban | 29 | 30 |
| code under a ban | 40 379 (59.5%) | 43 077 (63.4%) |
| `unsafe` sites, `crates/` | 426 | 426 |
| sites inside "enforced" crates | 3 (the bug) | 0 |
| `akuma-kernel-glue` | 3220 lines, no ban | 2416 lines, **forbid** |

The before-column's *reported* figures were 30 crates and 43 599 lines (64.3%);
both were inflated by counting `akuma-kernel-glue` as enforced. The true
baseline is the 29 / 40 379 above, so the real gain is +2698 lines and one crate.

## 10. Verification

- `cargo check --release`, `cargo clippy --release` — clean.
- Host tests: all green.
- Boot suite at `SMP=1`, `SMP=2`, `SMP=4`: **165 `Result: PASS` each, 0 failures,
  0 panics**, all three answering ssh (`scripts/vm_ready.py`).

One trap worth recording. The first `SMP=1` boot died on
`Assertion failed: (isv), function hvf_handle_exception` with QEMU exiting 134,
which reads exactly like a regression from touching the boot path. It is not: it
is `QEMU_HVF_ISV_BUG.md` **Root cause 5**, an under-provisioned-RAM *runner
argument*. The user-copy trampoline test deliberately runs off the end of mapped
kernel memory with an `LDP` (`0xa9401023`), which carries no syndrome, so HVF
reports ISV=0 and asserts before the guest sees the fault. `cargo_runner.sh`
prints the diagnosis itself. Re-run with `MEMORY=2048M`.

## Background

- `docs/archive/AKUMA_SMP_SHARED_SPLIT.md` — `smp_shared` reaching zero `unsafe`
  blocks, and `akuma-psci` as the sibling that let `akuma-boot` keep its ban.
- `docs/archive/SYSCALL_UNSAFE_CLEANUP.md` — the same move for `src/syscall/`.
- `docs/archive/AKUMA_UART_EXTRACTION.md` — why `console.rs` had no `unsafe` left.
- `docs/archive/AKUMA_NET_SPLIT.md` — the `akuma-net` / `akuma-net-nic` precedent.
- `docs/archive/QEMU_HVF_ISV_BUG.md` — §10's assert.
- `docs/reference/crate-safety.md` — which crates forbid and why the rest cannot.
