# `TlbTarget`/`TlbFlush`: TLB invalidation can now say which cores it covers — 2026-09-05

Phase 2 of `proposals/AKUMA_MMU_ARCH_PORTABILITY.md` (§3 of
`REDUCING_PLATFORM_DEPENDENCY.md`), landed right after Phase 1
(`AKUMA_MMU_TARGET_ARCH_GATE_FIX.md`).

## The change

`akuma-mmu`'s five TLB functions —
`flush_tlb_all`/`flush_tlb_asid`/`flush_tlb_page`/`flush_tlb_range`/
`flush_tlb_range_all_asid` — gained a `TlbTarget` parameter (`ThisCore` |
`AllCores`) and now return a `#[must_use] TlbFlush` token instead of `()`.
`TlbFlush`'s `Drop` is where the completion barrier (`dsb ish` + `isb` on
AArch64) now lives — previously inline at the end of each function body. A
bare `flush_tlb_page(va, TlbTarget::AllCores);` statement drops the token at
the same point the barrier used to run, so timing is unchanged; every call
site that needs the barrier *before* proceeding (all of them, in this
codebase — see below) binds with `let _ = ...;`, which drops the temporary
immediately rather than at end-of-scope the way `let _flush = ...;` would.

**Every one of the 25 call sites — 13 inside `akuma-mmu` itself, 12 in
`akuma-exec`/`akuma-exceptions`/`akuma-syscalls-glue`/`src/tests.rs`/
`src/process_tests.rs` — passes `TlbTarget::AllCores`.** Audited each one
individually rather than assumed: every call invalidates a *user address
space's* translation, and under `kernel_smp_shared` a process's threads can
run on any core, so every current caller already wanted the broadcast
semantics `kernel_smp_shared` was giving them at compile time. No call site
wants `ThisCore` today — the variant exists for a translation provably
private to the calling core, which nothing in this tree currently is. This
means **the change is behavior-preserving by construction**: the runtime
`match target` inside each function reduces to exactly the compile-time
`#[cfg(kernel_smp_shared)]` branch it replaced, because the only value ever
passed is the one that branch always selected.

## Why the vocabulary, if it changes nothing today

Quoting the proposal: four of this project's most expensive investigations —
`fork_cow_tlb_asid_flush`, `page_table_uaf_ttbr_gate_fix`,
`oncpu_gate_scheduler_race`, `cowstale_stale_write_fault_fixed` — were each,
in the end, a version of "which cores could still be holding this
translation," and the old API had no place to write the answer down. It now
does, on both sides: `TlbTarget` states the caller's intent, and `TlbFlush`
makes the completion an obligation the type system tracks rather than a
trailing function call a future refactor can silently drop.

It is also the seam the x86_64 port needs later: `akuma-cpu::tlb` has no
broadcast primitive (x86 needs an IPI-based shootdown for that, which this
change deliberately does not build — see `REDUCING_PLATFORM_DEPENDENCY.md`
§3.2, "do not build shootdown machinery"). `TlbTarget::AllCores` is where that
IPI would eventually get issued, and `TlbFlush::Drop` is where its
acknowledgement wait would go, without a second vocabulary bolted on beside
this one.

## What this does NOT do

No x86_64 body was added to any of the five functions — they still return an
inert `TlbFlush::done()` off `aarch64`, exactly as they returned `()` and did
nothing before. This phase is the vocabulary; a real x86 TLB implementation is
part of Phase 4 (the `UserAddressSpace` architectural decision), not this one.

## Verification

```
$ cargo build --release                              # full aarch64 kernel — unchanged
$ cargo clippy --release                              # clean
$ cargo test --target aarch64-apple-darwin -p akuma-mmu -p akuma-exec \
    -p akuma-exceptions -p akuma-syscalls-glue        # all passing, 0 failed
$ cargo test --target aarch64-apple-darwin            # full workspace — 0 failed
$ cargo clippy -p akuma-mmu --target x86_64-unknown-none --release   # clean
$ cargo clippy -p akuma-mmu --target aarch64-unknown-none --release  # clean
```

`src/tests.rs` and `src/process_tests.rs` are compiled into every default
release build (`kernel_tests` cfg is on unless the `no-tests` feature is set),
so `cargo build --release` above is a real compile of the fork/CoW/mmap
benchmark call sites this change touched, not just of `akuma-mmu` in
isolation.

**Boot verification was attempted and is blocked by an unrelated, pre-existing
host issue, not by this change.** Booting the built kernel under this
project's QEMU/HVF setup (`INSTANCE=8`, `scripts/cargo_runner.sh`) crashes
QEMU itself partway through the self-test suite:

```
[TEST] user copy: widened loop, all lengths/alignments, mid-copy fault
  OK: 81 lengths x 8 x 8 alignments copy exactly, no overrun
...
  OK: fully unmapped source returns EFAULT
Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 2437.
```

Confirmed this is **not caused by this change**: stashed every file this
phase touched (`git stash push` on the six modified files), rebuilt, and
booted a clean tree at the last commit (`b053d8b8`) — the identical assertion
fires at the identical point in the identical test (`Safe user access fault
redirection` → `user copy: widened loop...` → crash), byte-for-byte the same
register dump shape. Restored the stash afterward (`git stash pop`); `git
diff` against the pre-boot-test state is empty. The crashing test
(`copy_from_user`/`copy_to_user` EFAULT injection via `EC=0x25` exceptions) is
unrelated to TLB or page-table code, and the assertion is QEMU's HVF backend
rejecting an exception whose `ESR.ISV` bit came back clear — the same failure
class `crates/akuma-gic`'s own header warns about for MMIO writeback forms,
here from a different, not-yet-diagnosed source. **Out of scope for this
phase**; worth its own investigation if boot-testing on this host is needed
again before Phase 3/4 land.

## Next

Phase 3 (§1, `Prot`/`MemAttr` vocabulary) or Phase 4 (`UserAddressSpace`'s
page-table format itself) — see `proposals/AKUMA_MMU_ARCH_PORTABILITY.md`'s
updated Disposition.
