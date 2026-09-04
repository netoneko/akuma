# `akuma-threading`'s missing (not just wrong) `target_arch` gates — 2026-09-05

First step of `proposals/AKUMA_THREADING_ARCH_PORTABILITY.md`, itself the
follow-on to the `akuma-mmu` portability work
(`docs/archive/AKUMA_MMU_X86_ADDRESS_SPACE.md`). This is the "safe 90%" half
of that proposal's recommended split: fix the gates that are unconditionally
wrong, verify the crate's substantial arch-neutral bookkeeping already builds
for `x86_64`, and leave the real switch mechanism as an honest, documented gap
rather than attempt a rushed port of genuinely novel, safety-critical code.

## The bug

`crates/akuma-threading/src/lib.rs` (5,998 lines) had 5 `asm!`/`global_asm!`
sites, of which only 1 sat behind any `cfg` at all — and that one gate was
`target_os = "none"` alone, the same non-discriminating gate `akuma-mmu` had
before 2026-09-05 (`docs/archive/AKUMA_MMU_TARGET_ARCH_GATE_FIX.md`).
`x86_64-unknown-none` satisfies `target_os = "none"` exactly as
`aarch64-unknown-none` does, so all of the following would have tried to
compile literal AArch64 `msr`/`blr`/`wfi`/`tlbi` mnemonics into x86 codegen:

- `set_current_exception_stack` (`msr tpidr_el1`)
- `set_current_thread_register` (`msr tpidrro_el0`)
- the `thread_start`/`thread_start_closure`/`thread_exit_asm` `global_asm!`
  trampoline
- `sgi_scheduler_handler_with_sp` — the actual context-switch handler, whose
  `msr ttbr0_el1`/`tlbi vmalle1` install sits deep inside a ~250-line function

Every one of these except the last already had `not(target_os = "none")`
counterparts for host testing; the fix for those four is the identical
mechanical change Phase 1 made to `akuma-mmu`:
`all(target_os = "none", target_arch = "aarch64")` for the real body,
`not(all(...))` for the existing stub.

## The one deliberately different fix: `sgi_scheduler_handler_with_sp`

This function's non-bare-metal stub was already `unimplemented!("...bare
metal only...")`, not a silent no-op — someone had already reasoned about
"what happens if a host caller reads a stubbed answer as a real scheduling
decision" and chosen to panic rather than return `0`. That reasoning applies
identically to `x86_64-unknown-none`: **no x86_64 implementation of this
function exists**, and pretending otherwise (returning `0`, "no switch
needed") would be worse than refusing. So the fix here is exactly the gate
change, keeping the `unimplemented!()` behavior — now correctly reached from
both "not bare metal" and "bare metal, but not the one architecture this is
written for" — with its message and doc comment updated to say so explicitly
and point at the follow-on proposal.

**This is not a formality the way the other four are.** Unlike the `akuma-mmu`
gate fix, which had `amd64/src/paging.rs` as an already-proven port target,
there is nothing to port here — see the proposal doc for why
`amd64/src/sched.rs`'s cooperative-only switch is not a smaller version of
this SGI-interrupt/fake-IRQ-frame mechanism. Calling this function on
`x86_64` today is a deliberate, documented panic, not a missing feature that
happens to compile.

## Verification

```
cargo build --release                                        # aarch64 kernel — unchanged
cargo clippy --release                                        # clean
cargo build -p akuma-threading --target x86_64-unknown-none --release
cargo clippy -p akuma-threading --target x86_64-unknown-none --release   # clean
cargo clippy -p akuma-threading --target aarch64-unknown-none --release  # clean
cargo test --target aarch64-apple-darwin -p akuma-threading   # 24 passed, 0 failed
cargo test --target aarch64-apple-darwin                       # full workspace, 0 failed
```

**Link-time proof**, same method as the `akuma-mmu` fix: temporarily added
`akuma-threading` as a dependency of `akuma-amd64`, called
`set_current_exception_stack(0)` directly (safe — its x86_64 arm is a real
no-op) and took `sgi_scheduler_handler_with_sp`'s address without calling it
(forces real codegen of the function without triggering its deliberate
panic), then:

1. **Before this fix** (`git stash` on just this file): `cargo build
   --release -p akuma-amd64 --target x86_64-unknown-none` fails with 15
   errors — `invalid instruction mnemonic 'msr'`, `'blr'`, `'wfi'`, `'tlbi'`,
   `'b'` — exactly the `global_asm!` trampoline and the SGI handler's install
   sequence.
2. **After this fix**, the same build and link succeeds. `objdump -d` on the
   resulting ELF (101,069 disassembly lines) contains zero AArch64 mnemonics.

Probe reverted after — `git diff --stat amd64/` shows only the intentional
`amd64/Cargo.toml` version bump (0.1.0 → 0.0.7, to match the main kernel's
own version), nothing from the probe.

## What this does NOT do

`akuma-exec` still does not build for `x86_64` — most of its process/thread
lifecycle calls into functions this pass left untouched (arch-neutral atomic
bookkeeping that already compiled fine, so no fix was needed there) plus the
one real gap: no `x86_64` context switch exists. `sgi_scheduler_handler_with_sp`
calling it panics by design. The actual switch mechanism — what `proposals/
AKUMA_THREADING_ARCH_PORTABILITY.md` calls the open architectural decision —
remains unimplemented, deliberately, pending its own dedicated investigation
(reading `schedule_indices`, `setup_fake_irq_frame`, and the IRQ-restore path
in `akuma-exceptions` in full before writing any x86 switch code).

## Next

The switch mechanism itself — the actual remaining content of
`proposals/AKUMA_THREADING_ARCH_PORTABILITY.md`.
