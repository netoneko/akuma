# `akuma-user-access` gated for x86_64, with the real mechanism left open — 2026-09-05

Continuation of this session's `akuma-mmu`/`akuma-threading` portability work.
Where those two crates each had a proven AArch64→x86_64 port target
(`amd64/src/paging.rs`, `amd64/src/sched.rs`), this crate does not — and an
isolated experiment toward building one found a real, reproducible ABI
problem worth recording precisely.

## What this crate is

`crates/akuma-user-access` is the EL0 memory boundary: `__arch_copy_user_memory`
(AArch64 asm, widest-first 64/16/8/1-byte copy loop) plus
`__arch_copy_user_fault`, a trampoline the EL1 exception handler jumps to by
rewriting `ELR_EL1` when the copy loop faults on an unmapped or
non-EL0-accessible address — turning what would otherwise be a kernel panic
into a returned `EFAULT`. `docs/archive/BUSYBOX_HASH_MISCOMPUTE.md` is the
record of how easy this exact mechanism is to get subtly wrong (a widened
copy loop that silently corrupted `read(2)` results ~50% of the time).

## No proven port target exists

`amd64/src/usermode.rs` — the closest thing amd64 has to a user-copy path —
says so about itself, on `sys_write`:

> Reading `buf` from ring 0 works because `CR4.SMAP` is not enabled... That
> is a real gap and not a hypothetical.

amd64 currently dereferences user pointers raw and relies on the `#PF`
handler being merely diagnostic (fatal) rather than recoverable. There is no
x86_64 "already-working, just needs porting" implementation the way `paging.rs`
and `sched.rs` were for the previous two crates.

## The gate fix (done)

Same mechanical shape as before: `global_asm!`, its `extern` block, and the
two functions that call directly into the asm
(`copy_from_user_safe`/`copy_loop_differential_sweep`) are now
`#[cfg(target_arch = "aarch64")]`. `copy_to_user_safe` needed no change — it
already just calls `copy_from_user_safe`. Every higher-level function
(`copy_to_user`, `copy_from_user`, `write_user_val`, `read_user_into`,
`validate_user_range`, the `BypassValidation`/`BypassValidationGuard` pair)
was already portable (plain range-check logic, no asm) and needed no
changes at all.

**The x86_64 stub is `unimplemented!()`, deliberately, not a fake `EFAULT`.**
A caller reading a synthetic EFAULT as "the address was bad" instead of
"this isn't built here yet" would silently mask every future x86_64 call
site that assumes user-copy already works — the same reasoning
`akuma-threading`'s `sgi_scheduler_handler_with_sp` stub already uses.

## The real mechanism: an isolated experiment, and a negative result

Before committing to a design, ran the same kind of targeted experiment this
session used for the threading switch: **can `page_fault` in
`amd64/src/idt.rs` redirect `iretq` by rewriting the faulting `rip`?** This
is the x86 equivalent of AArch64's `ELR_EL1` rewrite, and the prerequisite
for any real fault-recovery copy mechanism.

Changed `page_fault`'s signature from
`extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: u64)`
to `fn page_fault(frame: &mut InterruptStackFrame, code: u64)`, and had it
write `frame.rip = <a known recovery address>` on a deliberately armed test
fault, then checked whether control actually resumed there.

**It did not work cleanly.** The CPU resumed at an address **5 bytes inside
a real instruction's byte encoding**, not at the intended recovery target —
decoded as garbage and raised `#UD` (invalid opcode) rather than either
succeeding or failing safely. Confirmed by disassembling the linked binary
at the faulting `rip` (`objdump -d`): it landed mid-way through an unrelated
`movq $0x1, %rax` a few bytes before the actual target label.

**This was reverted immediately** (`git checkout -- amd64/src/idt.rs`) —
`amd64/`'s diff is empty. It was never left in a state that could ship.

### What this means, and what it doesn't

This rules out the naive fix (just add `&mut` to the parameter) — it does
**not** establish that rip-rewrite recovery is impossible on this target,
only that this codebase's hand-rolled `InterruptStackFrame` plus rustc's
unstable `extern "x86-interrupt"` ABI don't compose the way a first guess
suggests. Plausible root causes, none verified:

- rustc's `abi_x86_interrupt` lowering may compute the on-stack frame
  location using a rule keyed to the *exact* by-value struct shape it
  expects (informed by whether a second `code: u64` parameter is present,
  to account for the error-code stack slot on some vectors) — and a `&mut`
  reference parameter may not be a case that internal rule handles, silently
  computing a wrong pointer instead of refusing to compile.
- This is genuinely unstable, sparsely documented compiler internals — the
  right way to resolve it is reading rustc's actual codegen for this ABI
  (not present in this repo), or bypassing it entirely.

### The real next step, not attempted here

Bypass `x86-interrupt` sugar for this one vector: a hand-written entry stub
(`global_asm!`, or an `#[unsafe(naked)]` function) for vector 14 that saves
registers itself, reads the real hardware-pushed frame (error code + return
state) at byte offsets it controls completely, calls a plain Rust function
with a raw pointer, and executes its own `iretq` after that function
returns — the same amount of manual control AArch64's `sync_el1_handler`
already has over `ELR_EL1`, and the same approach real production kernels
(including Linux's actual exception tables) use. This removes the dependency
on an unstable compiler feature's undocumented internals entirely, at the
cost of writing the entry/exit sequence by hand once.

## Verification

```
cargo build --release                                          # aarch64 kernel — unchanged
cargo clippy -p akuma-user-access --target aarch64-unknown-none --release   # clean
cargo clippy -p akuma-user-access --target x86_64-unknown-none --release   # clean
cargo test --target aarch64-apple-darwin -p akuma-user-access  # 6 passed, 0 failed
```

`cargo build -p akuma-exec --target x86_64-unknown-none` now gets **past**
`akuma-user-access` (previously the first thing to fail, with `cbz`/`b.lo`/
`b.hs`/`b.ne` invalid-mnemonic errors) and reaches two further, separate,
previously-hidden blockers: `akuma-el0-entry` (an `invalid register 'x30'`
error — another unconditional-AArch64-asm crate, not examined this session)
and `akuma-elf` (needs `UserAddressSpace::alloc_and_map`/`write_page_bytes`,
outside the 6-method subset `docs/archive/AKUMA_MMU_X86_ADDRESS_SPACE.md`
implemented). Neither is new evidence of a regression — both are exactly
the next names in the dependency chain, now measured rather than assumed.

## Next

**Done 2026-09-05** — the hand-written vector-14 stub described above was
built and boot-verified the same day: `docs/archive/AKUMA_USER_ACCESS_X86_FIXUP.md`.
The `unimplemented!()` stub described above is history; x86_64 has a real
`copy_from_user_safe`, and the experiment's negative result stands as the
reason the stub was written by hand.

`proposals/AKUMA_USER_ACCESS_ARCH_PORTABILITY.md` records the naked-stub
approach as the real next step. `akuma-el0-entry` is a newly-found,
unscoped blocker — nobody has looked at it yet.
