# Making the AArch64 crate chain reachable from amd64

**Date:** 2026-09-06
**Status:** `akuma-elf` done and verified. `akuma-exec` is the next wall and is a
real one.

## The question

"What specifically doesn't build for amd64?" — asked because the amd64 target
keeps re-implementing things the AArch64 side already has, and the standing
answer ("`akuma-exec`/`akuma-elf` are written against `akuma-mmu`") had never
been checked.

## Measured, not asserted

`cargo check -p <crate> --target x86_64-unknown-none` across the chain:

| builds for x86_64 already | blocked |
|---|---|
| `akuma-mmu`, `akuma-exec-core`, `akuma-vfs`, `akuma-net`, `akuma-ext2`, `akuma-isolation`, `akuma-scheduler`, `akuma-timer`, `akuma-bkl` | `akuma-el0-entry`, `akuma-elf`, `akuma-exec`, `akuma-vfs-glue`, `akuma-syscalls-glue`, `akuma-kernel-core`, `akuma-gic` |

**`akuma-mmu` builds for x86_64.** The `amd64/Cargo.toml` comment saying the
AArch64 loader "is written against `akuma-mmu`" is misleading — the crate is
fine; what is AArch64-only is the `UserAddressSpace` *type* inside it
(`#[cfg(target_arch = "aarch64")]`, the L0–L3 walker with ASIDs and `TTBR`).
~28% of its 3926 lines sit inside `aarch64`-gated items.

And the seven blocked crates had only **three** root causes between them:

1. **`akuma-el0-entry` — a `cfg` bug.** The gate was
   `#[cfg(target_os = "none")]`, chosen back when "bare metal" and "AArch64"
   were the same thing. `x86_64-unknown-none` **is** `target_os = "none"`, so it
   selected the AArch64 assembly arm and failed on `invalid register x30`. The
   host stub arm existed and was simply never reachable from the second bare
   target. Fixed to `all(target_os = "none", target_arch = "aarch64")` — the
   pattern `akuma-mmu` already uses 24 times. **One line.** This alone unblocked
   `akuma-vfs-glue`, `akuma-syscalls-glue` and `akuma-kernel-core` down to their
   next dependency.
2. **`akuma-elf` — three method calls.** Below.
3. **`akuma-gic` — genuinely AArch64.** It is the GICv3 driver; amd64 uses
   LAPIC/IOAPIC. Not a reuse candidate, and it only blocks `akuma-kernel-core`.
   (It should still carry the same `cfg` discipline so it *compiles* to a stub,
   but nothing wants to call it.)

## The elf fix

`akuma-elf` named `akuma_mmu::UserAddressSpace` concretely, in 16 places — but
called exactly **three** methods on it:

```
1  UserAddressSpace::new()
1  address_space.alloc_and_map(va, page_flags)
3  address_space.write_page_bytes(page_va, offset, bytes)
```

Make an address space, get a zeroed page mapped at a VA, put bytes in a page you
just mapped. Nothing there is architecture-specific — the page *tables* are, the
loader is not. So `crates/akuma-elf/src/pages.rs` now defines:

```rust
pub enum SegProt { Code, Data }
pub trait UserPages: Sized {
    fn new_space() -> Option<Self>;
    fn alloc_and_map(&mut self, va: usize, prot: SegProt) -> Result<usize, &'static str>;
    fn write_page_bytes(&mut self, page_va: usize, offset: usize, bytes: &[u8]) -> bool;
}
```

`LoadedElf<A>`, `LoadedWithStack<A>` and `UserStack<'a, A>` gained the parameter;
`akuma_mmu::UserAddressSpace` implements the trait under `cfg(aarch64)`, in this
crate (local trait, foreign type — so `akuma-mmu` stays unaware an ELF loader
exists). **`akuma-exec` needed one line changed.**

Three design notes:

- **`SegProt`, not the `u64` of PTE bits.** `segment_page_flags` returned
  `user_flags::RX` or `user_flags::RW_NO_EXEC` and nothing else — AArch64 PTE
  encodings, meaningless on x86, which an implementor would have had to
  pattern-match to discover what was being asked. Two variants is the faithful
  abstraction, and it makes W^X a property of the type rather than of a bitmask.
  `SegProt::to_user_flags()` remains for `DeferredLazySegment::page_flags`, which
  feeds AArch64-only lazy-region machinery.
- **Zeroing is in the trait's contract**, stated explicitly. A `PT_LOAD` whose
  `memsz` exceeds its `filesz` relies on the tail being zero — that is what
  `.bss` is — and the loader deliberately never writes those bytes. An
  implementation returning a recycled dirty frame corrupts `.bss` silently, only
  for programs that read a variable before writing it.
- **`e_machine` is now `cfg`-selected.** The check was
  `ehdr.e_machine != EM_AARCH64`, unconditionally. Left alone, the crate would
  have compiled for x86_64 and then refused **every** binary with
  `WrongArchitecture` — a correct-looking error for a bug entirely in the
  kernel. It is a `cfg` and not a `UserPages` associated constant on purpose:
  which binaries a kernel can run is a property of the CPU it was built for, not
  of how it allocates pages.

## Where the wall is now

```
akuma-elf            OK for x86_64
akuma-exec           <- akuma-exec: `UserAddressSpace: UserPages` not satisfied
akuma-vfs-glue       <- akuma-exec
akuma-syscalls-glue  <- akuma-exec
akuma-kernel-core    <- akuma-gic
```

`akuma-exec` names `UserAddressSpace` **54 times** (27 of them in
`process/address_space.rs`), and unlike the loader it genuinely uses the MMU:
CoW share passes, ASID allocation, `TTBR0` installs, lazy-region demand paging.
That is not a seam to draw in an afternoon and it is not a `cfg` accident — it
is the real thing. Whether it is worth doing is a separate question from whether
it is possible.

## Verified

Both kernels rebuilt and booted; full host suite and clippy clean on both
targets.

- **AArch64** (`INSTANCE=1`, snapshot disk): 165 `Result: PASS`, 97 `[PASS]`,
  one `[FAIL]` — the known clean-tree `retired_reclaim_ab`. `ps`, `uname -m`
  (`aarch64`) and `sh -c` all work, which exercises the genericised loader's
  hardest path end to end: `PT_INTERP` plus relocations on dynamically-linked
  musl binaries.
- **amd64**: 240/0 self-tests, unchanged.

## Follow-on

The ledger half of `UserAddressSpace` was extracted the same day —
`crates/akuma-user-space`, `docs/archive/AKUMA_USER_SPACE_LEDGER.md`. It does not
move the wall (it is 16% of the type) but it makes the walker's remaining content
honest and puts host tests on the two-counts rule.

## Background

- `docs/archive/AKUMA_AMD64_STREAMLINING.md` — the survey this came out of.
- `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §7 — why the tree prefers a
  narrow trait at a real seam over a `trait Arch`. `UserPages` is an instance of
  that argument: three methods, one job, no MMU concepts.
