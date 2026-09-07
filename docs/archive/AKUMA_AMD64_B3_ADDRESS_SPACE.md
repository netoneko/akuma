# B3: widening the x86 `UserAddressSpace`, and the gate going green

**Date:** 2026-09-07
**Scope:** item **B3** of `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — give the
x86_64 `akuma_mmu::UserAddressSpace` the method surface the shared crates
program against, so `akuma-syscalls-glue` builds for `x86_64-unknown-none`.
**Status:** done. **The gate is green.**

## Result

```
cargo check -p akuma-syscalls-glue --target x86_64-unknown-none
```

passes. It was 41 errors, all in `akuma-exec`, before this work.

| | before | after |
|---|---|---|
| gate errors | 41 | **0** |
| public methods on the x86 `UserAddressSpace` | 6 | **38** |
| `akuma-mmu` host tests | 15 | **21** |
| amd64 boot self-tests, QEMU `SMP=1` | 357/0 | **405/0** |
| amd64 boot self-tests, QEMU `SMP=4` | 366/0 | **414/0** |
| amd64 boot self-tests, **bare metal** (HP 500-502nj) | — | **407/0** |

+48 checks on both QEMU core counts — one new suite, `crate::uas::smoke_test`,
and nothing else moved. **All 48 pass on real hardware too**, with the whole
suite at 407/0 (RAM image, `stage()`'s default cmdline, so the USB controller is
never touched). No matched pre-B3 metal baseline exists at that exact boot
config, so no delta is claimed for that row — what is claimed is 48/48 and zero
failures anywhere in the suite. The AArch64 kernel's `.text`, `.rodata` and `.data` are
**byte-for-byte identical** to `HEAD`'s (method below). 1355 host tests pass;
clippy is clean on the AArch64 kernel, the amd64 kernel, `akuma-syscalls-glue`
for x86_64, and `akuma-mmu`/`akuma-elf` on the host.

## The error curve, and what each step was

The gate is one command, so the work is legible as the shape of its output:

| errors | where | what was missing |
|---|---|---|
| 41 | `akuma-exec` | the whole `UserAddressSpace` surface — 19 methods, the ledger, the `u64`-vs-`Prot` mismatch |
| 5 | `akuma-exec` | `impl UserPages` (the ELF loader's three methods) |
| 11 | `akuma-syscalls-glue` | `Prot::from_prot` ×2, and three more `UserAddressSpace` methods |
| 9 | `akuma-syscalls-glue` | `unmap_and_free_page{,_no_flush}`, `update_page_flags_no_flush` |
| 0 | — | |

`akuma-exec` reaching zero is the interesting line: it is the crate
`proposals/AKUMA_MMU_ARCH_PORTABILITY.md`'s Phase-5 note calls "the real wall",
and the wall turned out to be entirely this type.

## 1. The permission currency: a `u64` that is not a portability defect

`akuma-exec` calls `map_page(va, pa, mmu::user_flags::RW)` — an **AArch64 PTE
flag word**. The AArch64 walker consumes it directly. The x86 walker cannot: the
two permission masks share exactly zero bits, and AArch64's `AP_MASK` (bits
[7:6]) lands on x86's **Dirty** and **PAT**, so reading `is_write` off an x86 PTE
answers *"has this page been written?"* when asked *"may it be written?"*. That
measurement is `akuma_mmap::types`' own module header, and it is why `Prot`
exists.

Phase 3 (2026-09-05) recorded that "the x86_64 `UserAddressSpace` impl block
never needed to match the aarch64 side's `u64` signature at all, since the two
are `target_arch`-gated and never compiled together." That was true and is now
false: a **shared** crate calls these methods, so the signatures must match
exactly. The circumstance changed, not the reasoning.

Two ways to resolve it:

- **Migrate the callers to `Prot`.** 59 call sites across `src/` (34 of them in
  `src/*_tests.rs`), plus `LazyRegion::flags`, which stores a raw PTE word and
  depends on `0_u64` as an "unrecorded" sentinel — a mechanism `Prot` does not
  have. This is `REDUCING_PLATFORM_DEPENDENCY.md` §1, still exactly as valuable
  and exactly as undone as it was.
- **Decode at the walker.** The x86 methods take the same `u64` and turn it into
  a `Prot` first.

B3 takes the second, and the reason it is not the defect `akuma_mmap` warns
about is worth stating precisely: **the word is produced by `user_flags` and
consumed by `user_flags`.** It never touches an x86 page table. What crosses the
architecture boundary is a `Prot`. That is `akuma-cow`'s shape — decoded
booleans, never a raw PTE — which is exactly why that crate already serves both
kernels.

The decode is `akuma_mmu::user_flags::from_pte`, new here:

- **Total.** Every `u64` gets an answer, by asking the three questions
  `is_write`/`is_exec`/the `AP` field already answer, so no caller can construct
  a value that decodes to something the AArch64 walker would disagree with.
- **Fail-closed.** An EL0-inaccessible word decodes to `Prot::NONE`. That
  includes `0`, which is `LazyRegion`'s "unrecorded" sentinel — it has `AP =
  AP_RW_EL1`, so it decodes to *no user access*, not to a writable page.
- **Exactly invertible** over `to_pte`'s six outputs, which is the property the
  x86 walker rests on. `from_pte_inverts_to_pte` pins it.

## 2. Three pins, so the two x86 walkers cannot drift

`amd64/src/paging.rs` already has a working x86 walker with its own
`PteProt`/`encode` pair, and its boot self-test asserts each of six encodings.
The widened `akuma-mmu` type is a *second* x86 walker — the one the shared crates
reach through, and the one C1 folds this kernel onto. Two walkers that disagree
about what `Prot::RO` means is a bug nothing would catch until a page mapped
through `akuma-exec` behaved differently from one mapped through
`amd64/src/mm.rs`.

So `PteProt::from_region` here is a byte-for-byte port of that file's, and three
host tests pin it:

- `x86_prot_matches_amd64_encoding` — the same six hex literals
  `region_prot_roundtrip_check` asserts, spelled as literals rather than as the
  crate's own constants, for the reason `prot_roundtrips_to_todays_bits` gives.
- `x86_from_region_covers_every_variant` — six variants, **four** distinct
  encodings. `RO`/`RX` collapse (one execute bit, no `PXN`) and
  `RW`/`RW_NO_EXEC` collapse (nothing here may be writable *and* executable);
  both are pinned divergences from AArch64, and the test fails if a third
  collapse appears.
- `aarch64_flag_word_reaches_the_right_x86_bits` — the whole chain a shared
  caller actually travels, `u64 -> Prot -> PteProt -> bits`. The hops are pinned
  individually above; this pins the composition, which is what `map_page` runs.

They are host tests on an AArch64 Mac because the gate on the *pure encoding*
was widened from `#[cfg(target_arch = "x86_64")]` to
`#[cfg(any(target_arch = "x86_64", test))]`. The bits are portable data even
where the walker that writes them is not — which is precisely how the AArch64
`user_flags` table is already treated, and the kernel build is unaffected
because `cfg(test)` is false there.

## 3. The `Prot` shadow — found by the gate, not by reading

`akuma-mmu` does `pub use types::*`, which re-exports the neutral
`akuma_mmap::Prot`. The x86 block then defined its own `pub struct Prot`, which
**shadowed it**. So `akuma_mmu::Prot` meant one type on AArch64 and a different
one on x86_64.

That is invisible while nothing shared names the type. `akuma-syscalls-glue`
names it: `mmu::Prot::from_prot(prot)`, turning an `mmap` argument into a
region's protection. On AArch64 that is `akuma_mmap::Prot::from_prot`; on x86_64
it resolved to the page-table struct and failed to compile — which is the lucky
outcome. Had this struct happened to carry a `from_prot`, the same source line
would have produced a **page-table encoding** on one architecture and a **region
record** on the other, silently.

Fixed by naming the two levels apart, matching the sibling walker: `Prot` is
what a region records, `PteProt` is what the hardware is told, and
`PteProt::from_region` is the only bridge. Six errors became zero as a side
effect of the rename.

The generalisable part: a glob re-export plus a `cfg`-gated type of the same
name is a **silent per-architecture type substitution**. It cannot be seen at
either definition — only at a use site, in the build that resolves differently.

## 4. What the boot test found: `alloc_and_map` leaked its page tables

The widened type had no runtime caller — `amd64/` drives its own
`paging::AddressSpace`, and Phase 4's live test is gone. A compile-only proof of
thirty methods proves that they *type-check*, so `amd64/src/uas.rs` drives the
whole surface at boot: map, translate, write-through-physmap, the permission
bits, the CoW marker, eviction, zeroing, unmap-and-free, and a shared view.

It failed on its first run:

```
uas: three page-table frames tracked   [FAIL] got 0x0 want 0x3
```

`map_page` — the walk that allocates the **first three tables of every address
space** — used the untracked walker. The tracked one existed and was reached only
from `map_user_page_tracked`. An untracked table frame is not a leak the PMM can
see: it is reachable only from the page table that references it, so an address
space torn down without it loses the frame permanently.

The cause was a parameter shaped `ledger: Option<&FrameLedger>`, defaulting to
`None` so `map_page` could stay a one-liner. It is now `&FrameLedger`,
mandatory, and the walk also calls `track_frame(.., FrameSource::UserPageTable)`
— both halves of what the AArch64 `get_or_create_table` does. **A parameter that
can be omitted will be**, and this one could be omitted on the path that runs
first, most often, and for every process.

Worth holding next to the two latent bugs B1 found the same way (a `fork` child
leaking every page it `mmap`ed after forking; `munmap` double-freeing a loader
page). Three for three: the frame-ownership questions on this target are found by
running, not by reading.

## 5. Pinned divergences, recorded rather than smoothed over

Each of these is in the source at the method that has it, not only here.

| | AArch64 | x86_64 |
|---|---|---|
| `asid()` | a real ASID, allocated and freed | always `0`. `CR3` rewrite flushes non-global entries wholesale; PCID is the equivalent and is deliberately unused, because enabling it without also porting the invalidation discipline leaves stale translations live under a correct-looking ASID number |
| `ttbr0()` | `(asid << 48) \| l0_phys` | the bare root — same packing, high half always zero, because `ProcAddressSpace`'s lock-free scalar mirror unpacks this word on both targets |
| `new_shared` | allocates an ASID (can fail), refcounts in `SHARED_L0_TABLE` | cannot fail; no registry, because this target has no `Drop` for an address space and so nothing for a registry to arbitrate |
| `demote_range_to_ro` | clears `AP_RW_ALL`; no marker exists | clears `RW` **and sets PTE bit 9**. Without the marker every CoW fault here looks like a real protection violation — `akuma-cow`'s pinned divergence, from the other end |
| `map_user_page_tracked_no_flush` | genuinely skips the `tlbi`; caller must range-flush | still issues `invlpg`. Suppressing it would need a range flush to exist first; the batch form costs one `invlpg` per page rather than being wrong |
| `invalidate_icache_for_page_va` | `dc cvau` + `ic ivau` | a no-op. x86 instruction caches are coherent with stores. It exists so the shared callers need no `cfg` |
| `read_l3_page_entry` | the L3 descriptor | the PT entry, in **x86 bits**. The name is kept so shared callers need no `cfg`; a caller that decodes the result must do so per-architecture |
| `update_page_flags` | replaces `AP`/`UXN`/`PXN`, preserves the rest | replaces `RW`/`US`/`NX`, preserves the rest — **including the CoW marker**. An `mprotect(PROT_WRITE)` over a demoted page then yields writable-and-marked, which never faults; that is exactly what the AArch64 body produces from the same call, and clearing the bit would make the two diverge under `mprotect`, which is worse than the shared behaviour they already have |

## What is NOT covered

- **The two `map_user_page_tracked*` methods are covered only by their refusal
  path.** They require `CR3` to already hold this address space's root. Reaching
  them at boot means installing a second address space while the kernel is
  running on the first — a mistake there is a triple fault with no console, on a
  box whose only recovery is a physical reset. The test checks that they decline
  when not installed, which is the branch that runs.
- ~~**Nothing here runs on bare metal yet.**~~ **Ran on the HP box 2026-09-07,
  same day: 48/48 `uas:` checks, suite 407/0.** The prediction below held — the
  code under test allocates frames and edits page tables it builds itself, with
  no device or firmware dependency, so there was nothing for the metal to
  diverge on. Recorded rather than deleted because "lower risk of a metal-only
  divergence" was a *guess* until it was checked, and the archive is where
  guesses get their answers.
- **`amd64/` still uses `paging::AddressSpace`.** B3's success criterion is that
  the shared crates build and the type works, not that this kernel uses it.
  That is C1.
- **`akuma-exec` is not *complete* on x86_64, it *compiles*.** Whether every one
  of those code paths is correct on this target is what C1's fold will find out,
  one dispatch arm at a time.

## Method: proving the AArch64 kernel unchanged

`crates/akuma-mmu` is compiled into both kernels, so "I only touched x86 code"
is a claim, not a fact. It was checked:

```bash
git worktree add --detach $TMP/baseline HEAD
# seed submodules — a fresh worktree has none, and the build dies on
# crates/akuma-fbcon/vendor/spleen with a `git submodule update` hint
(cd $TMP/baseline && cargo build --release)
rust-objcopy --dump-section .text=a.bin $TMP/baseline/target/.../akuma /dev/null
rust-objcopy --dump-section .text=b.bin           target/.../akuma /dev/null
cmp a.bin b.bin
```

`.text` (3,207,016 bytes), `.rodata` (270,368) and `.data` (199,896) are
**identical**. The whole-file hashes differ by 144 bytes, entirely in `.strtab`
— symbol name strings embed the build directory, and the baseline was built at a
longer path. Section *sizes* match everywhere except there.

Do not compare whole-binary hashes across worktrees and conclude a change: the
build path is in the binary. Compare sections.

## Files

- `crates/akuma-mmu/src/types.rs` — `user_flags::from_pte` + 2 host tests.
- `crates/akuma-mmu/src/lib.rs` — the widened x86 `UserAddressSpace`, the
  `Prot` -> `PteProt` rename, `x86_leaf_slot_in`, the mandatory ledger in the
  walker, + 4 host tests.
- `crates/akuma-elf/src/pages.rs` — the `UserPages` impl's `cfg` widened to both
  architectures. One body, not two: nothing in those three methods is
  architecture-specific once the surface matches.
- `amd64/src/uas.rs` — new. 48 boot checks.
- `amd64/src/main.rs`, `amd64/src/boot.rs` — wire it in, right after
  `paging::smoke_test`, because the two are the same job done twice and a
  divergence is easiest to read when their results are adjacent.
- `amd64/Cargo.toml` — `akuma-mmu` becomes a direct dependency. It arrived
  transitively through `akuma-user-access` already; naming it is what lets the
  boot test drive it.

798 insertions, 34 deletions, plus `amd64/src/uas.rs`.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree this closes B3 of.
- `proposals/AKUMA_MMU_ARCH_PORTABILITY.md` — Phase 4 built the six-method x86
  type this widens; its non-goals define what "port, not redesign" means.
- `docs/archive/AKUMA_MMU_X86_ADDRESS_SPACE.md` — that Phase 4.
- `docs/archive/AKUMA_AMD64_MMAP_REGIONS.md` — B1/B2, and the two frame-ownership
  bugs found the same way §4's was.
- `docs/archive/AKUMA_AMD64_COW.md` — the CoW marker divergence from the other end.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why `from_pte` fails closed.
