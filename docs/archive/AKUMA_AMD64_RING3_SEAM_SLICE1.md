# amd64 ring-3 entry seam, slice 1: `UserContext` splits, and `fork_process` compiles for x86_64

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row — the step `AKUMA_AMD64_C1_5C_SURVEY.md` named and
`proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` scopes.
**Slice:** 1 of 4. See that prompt § 6 for the other three.

`akuma_exec::process::fork_process` now **compiles for
`x86_64-unknown-none`**, and the AArch64 kernel it is shared with is
**byte-for-byte identical** in every loaded section. That is the whole of slice
1: the register file, and nothing about the lifecycle.

## 1. The measurement that made this a small slice

The roadmap's phrasing — "`fork_process` builds an AArch64 `UserContext`;
amd64 has no `eret`" — reads as though the type were the problem. Measured, it
is 36 `u64`s with **335 field reads** across nine files, two of which index it
by literal byte offset from `global_asm!`. That sounds like a redesign.

It is not, because of what happens when you just split it. Gating the AArch64
struct to `not(target_arch = "x86_64")`, adding an x86_64 arm, and building the
amd64 kernel produces **ten errors, all in one file**:

```
crates/akuma-exec/src/process/mod.rs — 10 errors
  3 × no field `x0`      (fork_process, vfork, clone_thread)
  3 × no field `spsr`    (the same three)
  3 × no field `ttbr0`   (the same three)
  1 × no field `tpidr`   (clone_thread)
```

Every other reader — all of `akuma-threading`'s 18 `sp` / 12 `ttbr0` / 7 `spsr`
reads, `akuma-mmu`'s, `akuma-exec-core::thread`'s — was **already**
`target_arch`-gated, because the x86 threading work
(`AKUMA_THREADING_X86_SWITCH.md`) had gated them when it split the kernel-side
`Context`. So the entire arch-specific surface of `UserContext` that shared code
touches un-gated is *the three child-context builders*, which is exactly the
code this step exists to fold.

**`pc` and `sp` were already neutral and already load-bearing on both
kernels.** The amd64 kernel has stored `UserContext::new(image.entry,
image.stack)` on every registered process since 5b slice 4 and reads the pair
back through `current_entry_stack()`; its `execve` rewrites both under the
`image` lock so a re-entry cannot pair a new entry point with an old stack. That
is why the x86_64 arm spells its fields `pc` and `sp` rather than `rip` and
`rsp` — naming them for the machine would have made every shared reader need a
`cfg`.

## 2. What was done

### 2.1 `UserContext` is two structs with one name

The same pattern `Context` already carries one level down in
`akuma-exec-core/src/thread.rs`: `#[cfg(target_arch = ...)]` on the struct
itself, identical name, different fields.

The x86_64 arm is `{ regs: [u64; 12], sp, pc, fs_base, gs_base, rax }` — and
the field list is **not a design choice, it is what the `syscall_entry`
assembly saves**: `rdi, rsi, rdx, r10, r8, r9, rbx, rbp, r12..r15` (the System V
argument registers plus the callee-saved set), in that order, because the order
is the assembly's. `rcx` and `r11` are absent because `syscall` itself clobbers
them; `rax` is a named field rather than an array slot because it is the one
register shared code cares about by name.

Three AArch64 fields have **no x86_64 counterpart, and each absence is a
design statement** recorded at the field:

| absent | why |
|---|---|
| `spsr` | Privilege on entry comes from the `sysret`/`iretq` selectors in the entry assembly, which no caller can influence. The AArch64 arm carries one because `eret` *reads* it — which is why `enter_user_mode_checked` exists to refuse a context whose `spsr` targets EL1. There is no equivalent mistake to guard against. |
| `ttbr0` | The page-table root is installed by the scheduler from the task slot's own `space_root`, before the entry. It is not part of the register file, so the staleness bug §2.2 describes **cannot arise** here: one authority, not a copy. |
| `tpidr` | Its counterpart is `fs_base`, named for the MSR it is written to. |

### 2.2 Four arch-neutral setters replace ten field writes

The three builders now say what they mean rather than which register they poke:

| setter | AArch64 | x86_64 |
|---|---|---|
| `set_child_return_zero()` | `x0 = 0` | `rax = 0` |
| `set_unprivileged_entry()` | `spsr = 0` (EL0t, DAIF clear) | **nothing** |
| `set_address_space_root(root)` | `ttbr0 = root` | **nothing** |
| `set_tls_base(tls)` | `tpidr = tls` | `fs_base = tls` |

Two of them are empty on x86_64, and that is the point rather than a stub: the
doc on each is the argument for the emptiness, and neither is a `todo!()`.

`set_address_space_root` carries the bug all three builders had a comment
about, in one place instead of three. The inherited value comes from the
thread's *saved* context, refreshed only when the scheduler switches **away**
from a thread, so a parent that has `execve`'d or `mmap`'d since its last
switch-out has a stale root there. Loading it on the child's first schedule
wedged the CPU: a TLB flush, an instruction fetch against a garbage page table,
`ec=0x20` with IRQs masked, and a silent VM hang. The method's name says the
root must be the child's own.

### 2.3 The host test that would not have compiled on an x86_64 host

`user_context_new` asserted `ctx.x0 == 0`. `#[cfg(test)]` code is built **for
the host**, so `target_arch` there is the host's — and an x86_64 developer
running `cargo test` would have got the x86 struct and a compile error. This is
the same trap `akuma_syscalls_linux::nr` carries and CLAUDE.md states
("`cfg!(target_arch)` resolves to the *host* under `cargo test`"); it was
invisible here only because the tree's development machine is arm64.

Split in two: `user_context_new` now asserts the **neutral** pair only, and a
new `user_context_setters_map_to_this_arch` asserts the per-arch mapping of all
four setters under `cfg`, **poisoning the fields first** so a setter that
silently did nothing on the arm that needs it fails rather than passing against
a zeroed struct. Host tests 1372 → **1373**.

## 3. Verification

| gate | before | after |
|---|---|---|
| `cargo build -p akuma-amd64 --target x86_64-unknown-none` | OK | **OK** |
| `akuma-exec::fork_process` compiles for x86_64 | **no** (10 errors) | **yes** |
| QEMU/TCG `SMP=1` | 616/0 | **616/0** |
| QEMU/TCG `SMP=4` | 626/0 | **626/0** |
| amd64 `--features no-tests` | OK | **OK** |
| host tests | 1372 | **1373** |
| clippy — aarch64 `release`, `extreme-size`, amd64, the three touched crates on host | clean | **clean** |

### 3.1 The AArch64 kernel is byte-for-byte identical

This slice edits code the AArch64 kernel *executes*, and that kernel cannot be
booted on this machine (HVF asserts; TCG panics in a pre-existing self-test —
`AKUMA_SELF_HOSTING_AMD64.md` Open issue 4). So the check is stronger than a
side-by-side boot: build committed HEAD and the change, and compare every
loaded section.

```
section                  addr       size  content
.text.boot         0x40100000        704  identical
.data.boot         0x401002c0         24  identical
.text              0x40100800    3217420  identical
.rodata            0x40413000     270456  identical
.data              0x40456000     199896  identical
.bss               0x40487000    1105968  identical
```

The **only** difference in the two ELFs is `.strtab`, 231 bytes longer, holding
the four new method names. The setters inline to the field writes they replaced,
which is what "this is a rename, not a change" means when you can measure it.

Recipe, for the next slice — slice 2 changes `Process::run`'s tail and will
*not* come out identical, so it needs the boot instead:

```bash
cargo build --release && cp target/aarch64-unknown-none/release/akuma /tmp/change.elf
git stash push -- crates/ amd64/
cargo build --release && cp target/aarch64-unknown-none/release/akuma /tmp/head.elf
git stash pop
# then compare loaded sections (a 30-line Python ELF section walk; rust-objcopy
# and llvm-objdump are both absent on this machine, and `rust-readobj
# --sections` only gives you sizes, not contents)
```

## 4. What slice 1 deliberately did not do

- **The lifecycle.** `Process::run()` is `-> !` and `eret`s; amd64's
  `run_process` is a **loop** whose `enter_user` *returns* an exit status and
  whose teardown is the forty lines after it. That mismatch — not the register
  file — is the real seam, and it is slice 2. The prompt's § 4 has the three
  options and the recommendation.
- **Calling `fork_process`.** `sys_fork` still runs, unchanged; it just goes
  through the shared type now. Folding it is slice 3, and the
  `CHILD_CHANNELS` decision (§ 5 of the prompt) bites there and nowhere
  earlier.
- **`update_thread_context`'s x86 arm.** Still `#[cfg(not(target_arch =
  "aarch64"))]` and still a no-op (`crates/akuma-threading/src/lib.rs`). It is
  the most mechanical piece left — today's `amd64::sched::seed_forked_task`,
  moved — and belongs with slice 2 or at the head of slice 3.

## Background

- `proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` — the step's prompt, with the
  measurements this slice acted on and the remaining slices.
- `docs/archive/AKUMA_AMD64_C1_5C_SURVEY.md` — why this is a step and not a fold.
- `proposals/AKUMA_THREADING_ARCH_PORTABILITY.md` § "The open decision" /
  § "Status" — the `UserContext`-with-accessors redesign was posed and rejected
  once already. §2.1 follows that verdict; §1 is the measurement that says it
  was the right one.
- `docs/archive/AKUMA_THREADING_X86_SWITCH.md` — the kernel-side `Context`
  split, whose `target_arch` gating is why only ten call sites broke.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md` — the previous step, and § 6's
  argument that the `ProcessChannel` work may be worth more than this one if
  interactive use matters more than `fork`.
