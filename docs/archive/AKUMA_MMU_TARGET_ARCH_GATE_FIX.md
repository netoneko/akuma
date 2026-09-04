# `akuma-mmu`'s `target_os` gate did not discriminate architecture — fixed 2026-09-05

Phase 1 of `proposals/AKUMA_MMU_ARCH_PORTABILITY.md` (accepted 2026-09-05), itself
a follow-on from `REDUCING_PLATFORM_DEPENDENCY.md` §9.4. That section predicted
this exact failure and this doc is the confirmation, the fix, and the proof —
measured, not assumed.

## The bug

`crates/akuma-mmu/src/lib.rs` had 19 occurrences of `target_os = "none"` gating
AArch64-only bodies — `TTBR0_EL1` reads, boot-identity-map page-table edits in
AArch64's 3-level block-descriptor format, and (in three places) literal
`core::arch::asm!` containing `adrp`, `:lo12:` and `msr ttbr0_el1`. None were
conjoined with `target_arch`. `x86_64-unknown-none` satisfies `target_os =
"none"` exactly as `aarch64-unknown-none` does, so every one of those gates
picked the AArch64 body on x86 too — the identical bug class
`akuma-primitives::preempt::current_tid()` carried until 2026-09-04, and the one
`akuma-cpu`'s own module doc names as "the lesson generalises... those are still
`target_os`-only. They are latent duplicates of this bug."

**A standalone `cargo build -p akuma-mmu --target x86_64-unknown-none` does not
catch it.** This workspace's `[profile.release]` sets `lto = "thin"`, so a
per-crate build defers real instruction selection to the final link and reports
success regardless of what an AArch64-only branch contains. Confirmed directly
below.

## The fix

Every one of the 19 sites (`grep -n 'target_os = "none"'
crates/akuma-mmu/src/lib.rs` before this change) became `all(target_os =
"none", target_arch = "aarch64")` for the real body and `not(all(target_os =
"none", target_arch = "aarch64"))` for the existing fallback stub — the exact
conjunction `akuma-cpu`'s `barrier`/`park`/`cache`/`tlb`/`daif`/`sysreg` modules
already use, and for the same two reasons stated in that crate's own header:
not `target_arch` alone (the dev host is `aarch64-apple-darwin`, so that alone
would make host tests execute real EL1 instructions and `SIGILL`), and not
`target_os` alone (this bug).

Three call sites (`ensure_boot_identity_covers`, `with_boot_identity_fdt`,
`extend_boot_ram_identity_map`) had no existing non-aarch64 fallback at all —
compiled out entirely off `target_os = "none"` — and stay that way, now
additionally gated on `target_arch = "aarch64"`. Confirmed safe: neither is
called from `akuma-exec`, `akuma-threading`, `akuma-elf`, or
`akuma-user-access` (the four crates this whole effort exists to unblock for
x86_64) — their only callers are `akuma-kernel-glue`/`akuma-kernel-core`, which
are not in `amd64/Cargo.toml` and have no x86_64 discovery path (PVH boot uses
`crates/akuma-ryzen-amd64`, not a DTB).

**No AArch64 runtime behavior changed.** Every gate that previously read
`target_os = "none"` still reads true under `target_os = "none", target_arch =
"aarch64"` — the aarch64 body is unconditionally the same body, just now also
excluded correctly from a second architecture it was never written for.

## What this does NOT fix

The crate now **builds and links** for `x86_64-unknown-none` instead of
failing at codegen. It does not make `akuma-mmu` **work** on x86_64:
`UserAddressSpace::activate()`/`deactivate()` — the process context-switch
path — silently do nothing on x86_64 now (the `msr ttbr0_el1` block is simply
absent), and the ~1,800 lines of page-table walking (`TableFrames`, the L0-L3
walk, the `AP`/`UXN`/`PXN`/`AF`/`SH_*` bit layout) remain AArch64's translation
format with no x86 branch. That is proposal items §1 (portable `Prot`/`MemAttr`
vocabulary) and §3 (TLB target vocabulary), plus the open architectural
decision the proposal poses in "What 'fixed' looks like" — a real
`target_arch` split inside the crate vs. a second x86-native crate below
`akuma-exec` — neither of which this pass attempts. **Still open.**

## Verification

```
$ cargo build --release                                    # full aarch64 kernel — unchanged
Finished `release` profile [optimized] target(s) in 8.94s
$ cargo test --target aarch64-apple-darwin -p akuma-mmu     # 11 passed, 0 failed
$ cargo clippy -p akuma-mmu --target aarch64-unknown-none --release   # clean
$ cargo clippy -p akuma-mmu --target x86_64-unknown-none --release    # clean
```

**The link-time proof**, per the proposal's own instruction not to trust a bare
per-crate build: temporarily added `akuma-mmu` as a dependency of the
`akuma-amd64` binary (a real `[[bin]]`, workspace member, real linker script —
not a standalone `.rlib`) and called `get_boot_ttbr0`, `get_current_ttbr0`,
`flush_tlb_all`, `flush_tlb_asid`, `flush_tlb_page` from `kmain`, then:

1. **Before this fix** (`git stash` on just this file, same probe): `cargo
   build --release -p akuma-amd64 --target x86_64-unknown-none` fails exactly
   as predicted —
   ```
   error: invalid instruction mnemonic 'adrp'
   error: unknown token in expression   (:lo12:boot_ttbr0_addr)
   error: invalid instruction mnemonic 'ldr'
   ```
2. **After this fix**, the same build and link succeeds. `objdump -d` on the
   resulting ELF (101,150 disassembly lines) contains zero AArch64 mnemonics
   (`grep -ci 'adrp\|tlbi\b'` → 0) — the stub bodies were selected, not the
   real ones, confirmed rather than assumed.

The probe (the `akuma-mmu` dependency line in `amd64/Cargo.toml` and the call
block in `amd64/src/main.rs::kmain`) was reverted after verification —
`git diff --stat amd64/` is empty. Per the proposal's explicit non-goal, this
pass does not wire `amd64/` to actually use `akuma-mmu`.

## Next

`proposals/AKUMA_MMU_ARCH_PORTABILITY.md` §1 and §3, and the real architectural
decision (target_arch split vs. second crate) for `UserAddressSpace` itself.
