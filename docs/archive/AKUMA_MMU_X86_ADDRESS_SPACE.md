# `akuma-mmu` gets a real x86_64 `UserAddressSpace` — 2026-09-05

Phase 3+4 of `proposals/AKUMA_MMU_ARCH_PORTABILITY.md`, landed right after
Phase 1 (`AKUMA_MMU_TARGET_ARCH_GATE_FIX.md`) and Phase 2
(`AKUMA_MMU_TLB_TARGET_VOCABULARY.md`). Where those made `akuma-mmu` **build
and link** for `x86_64-unknown-none`, this makes its core memory-management
type **actually work** there — proven by booting real page-table code under
QEMU, not just by compiling it.

## What changed, and what it deliberately did not

The original plan (`proposals/AKUMA_MMU_ARCH_PORTABILITY.md`'s Phase 3) called
for the full §1 `Prot`/`MemAttr` migration — replacing `akuma-mmap`'s raw
`u64` permission constants everywhere, across the ~254 call sites the original
`REDUCING_PLATFORM_DEPENDENCY.md` §1 measured. Two things found while actually
starting that work changed the plan:

1. **`is_exec(RO) == true` is not the bug the original doc called it.** The
   current `akuma-mmap/src/types.rs` carries a test,
   `user_flags_is_exec_reads_only_uxn`, that explicitly asserts this with the
   comment *"that is the AArch64 encoding, not an oversight: a PTE without
   UXN is EL0-executable."* Whoever wrote that test re-litigated the question
   after the original doc and reached the opposite conclusion, without ever
   updating the doc. The "fix" was dropped rather than break a deliberately
   tested invariant — a stale doc claim corrected here, not carried forward.
2. **The x86_64 `UserAddressSpace` doesn't actually need the migration.**
   Since it lives behind its own `#[cfg(target_arch = "x86_64")]` block,
   completely separate from the aarch64 `impl`, its `map_page` can simply take
   `Prot` as its own parameter type — it never has to match the aarch64
   side's `u64` signature, because the two are never compiled together.

So what actually landed is **narrower and additive**: `Prot`/`MemAttr` are new
types, used only by the new x86_64 code. Zero existing call sites, zero
existing tests, and zero AArch64 behavior changed. The full §1 migration
(fixing the `EXEC`/`RO` aliasing as a *simplification* rather than a
correctness fix, and the real `LazyRegion`/`lazy_map_flags` wrinkle found
along the way — it depends on `0_u64` as an "unrecorded" sentinel, a
different mechanism than `MmapRegion`'s `prot_recorded` bool) remains exactly
as `REDUCING_PLATFORM_DEPENDENCY.md` documented it: real, valuable, and still
undone.

## The x86_64 `UserAddressSpace`

A `target_arch`-gated struct+impl split, same name on both sides:

```rust
#[cfg(target_arch = "aarch64")]
pub struct UserAddressSpace { /* existing fields, byte-for-byte unchanged */ }
#[cfg(target_arch = "x86_64")]
pub struct UserAddressSpace { root: usize }
```

`akuma-exec` and everything above it keeps naming `akuma_mmu::UserAddressSpace`
unchanged. On `aarch64` that name resolves to the exact type it always did. On
`x86_64` it resolves to this new, much smaller type. Any code that calls a
method only the aarch64 side has — CoW sharing, ASID, lazy regions, all of
it — simply fails to compile for `x86_64`, which is the honest boundary of
what this pass proves.

**Ported directly from `amd64/src/paging.rs`, not redesigned** — that file
already built exactly this vocabulary and encoding, including a `Prot`
matching item 1's proposed shape and real `read_cr3`/`write_cr3`/`invlpg`
wrappers with a comment citing item 3 by name:

| Method | Ported from |
|---|---|
| `new() -> Option<Self>` | `AddressSpace::new` — alloc a PML4, zero it, alias the kernel's shared PML4 slots (`[256, 257, 511]` — physmap, device window, kernel image, matching `amd64`'s own `SHARED_PML4_SLOTS`) |
| `map_page(&mut self, va, pa, prot: Prot)` | `map_page_in` |
| `unmap_page(&mut self, va)` | `unmap_page_in` |
| `translate(&self, va) -> Option<usize>` | `translate_in` |
| `activate(&self)` | `write_cr3` |
| `deactivate()` | `write_cr3` back to a captured boot root |

**`akuma-cpu::tlb` gained `invlpg(va)`** — the concrete gap
`REDUCING_PLATFORM_DEPENDENCY.md` §3 named ("no `invlpg` equivalent"), real
body on `x86_64`. Phase 2's `flush_tlb_page`/`flush_tlb_asid`/`flush_tlb_all`
x86_64 arms were upgraded from the inert `TlbFlush::done()` stub to real
bodies: `flush_tlb_page` calls `invlpg`; `flush_tlb_all` and `flush_tlb_asid`
(no PCID support in this pass's scope, so no cheaper form exists) both reload
`CR3` — a correct, more aggressive superset, the same trade-off this file
already makes for a large `flush_tlb_range_all_asid` call.

`deactivate()`'s boot root comes from a first-caller-wins `AtomicU64`
(`X86_BOOT_ROOT`), captured inside `new()` the first time it runs — there is
no `boot.s`-recorded symbol the way AArch64's `get_boot_ttbr0` has, and
adding one was out of scope. Correct for one boot address space at a time,
which is this pass's whole scope.

**Deliberately not a `Drop` impl**, matching `amd64::paging::AddressSpace`'s
own explicit choice and its stated reason: freeing an address space still
installed in `CR3` unmaps the code doing the freeing. Dropping a
`UserAddressSpace` on this target leaks its page-table frames rather than
risk that.

## Verification

```
cargo build --release                                    # full aarch64 kernel — unchanged
cargo build -p akuma-mmu --target x86_64-unknown-none --release
cargo clippy -p akuma-mmu --target aarch64-unknown-none --release   # clean
cargo clippy -p akuma-mmu --target x86_64-unknown-none --release    # clean
cargo test --target aarch64-apple-darwin -p akuma-mmu -p akuma-cpu  # all passing
```

**The real proof — a live boot, not a link check.** Temporarily added
`akuma-mmu` as a dependency of `akuma-amd64` and, from `kmain` right after
`paging::smoke_test`, exercised the new type directly: build a second
`UserAddressSpace`, `map_page` a scratch frame into it with `Prot::USER_RW`,
`activate()` into it, write a pattern through the mapped VA and read it back,
`translate()` it, `deactivate()` back to the kernel root, `unmap_page`, and
confirm `translate()` now returns `None`. Booted under this project's real
amd64 QEMU path (`amd64/run.sh`, `-M microvm`, TCG-emulated x86_64 on this
Apple Silicon host — not the HVF backend, so the unrelated ARM-guest HVF
assertion this session hit earlier for the aarch64 kernel does not apply
here). All four checks passed:

```
akuma-mmu x86_64: map_page succeeds   [OK]
akuma-mmu x86_64: write/read through the mapping round-trips   [OK]
akuma-mmu x86_64: translate() resolves to the mapped frame   [OK]
akuma-mmu x86_64: translate() is None after unmap_page   [OK]
```

Full boot: **177 self-tests passed, 0 failed** (the pre-existing 173 plus
these 4) — no regression anywhere else in the amd64 self-test suite. The
probe was reverted after — `git diff --stat amd64/` is empty (the
`amd64/Cargo.toml` dependency line and the `kmain` block were both removed).

One unrelated finding along the way, ruled out rather than chased: booting
with `DISK=none` also strips `amd64/run.sh`'s NIC configuration (both live in
the same `if [ "$DISK" != "none" ]` block), and `net::init(true)` page-faults
with no virtio-net device present. Confirmed this reproduces on a clean
checkout with none of this session's changes applied, so it's pre-existing
and out of scope here — worth a `run.sh` fix (skip `net::init` when
`DISK=none`, or split the NIC config out of the disk conditional) as a small,
separate follow-up.

## What this does NOT do

`akuma-exec` still does not build for `x86_64` — its `Process`/fork/exec code
calls dozens of `UserAddressSpace` methods this pass didn't implement (CoW
sharing, ASID tracking, lazy regions), and `akuma-threading` (a sibling
dependency) has its own, separate, comparably-sized portability problem —
see `proposals/AKUMA_THREADING_ARCH_PORTABILITY.md`. This pass proves
`akuma-mmu`'s core primitive works on real x86_64; it does not make the
crates above it usable there yet.

## Next

`proposals/AKUMA_THREADING_ARCH_PORTABILITY.md` (written alongside this doc),
and — once that lands — the actual rewire of `amd64/src/usermode.rs` onto
`akuma-exec::process` this whole effort has been building toward.
