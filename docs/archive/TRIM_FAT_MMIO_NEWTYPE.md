# `MmioReg<T>`: concentrate device-register volatile access (implementation plan)

> **Status: IMPLEMENTED 2026-08-15.** The plan below is kept verbatim; what
> actually happened is recorded in [§5 Outcome](#5-outcome-2026-08-15) at the
> bottom, including the two optional files that were **not** converted and why,
> and a correction to this document's own headline estimate.

**Status: plan, not started.** Written 2026-08-15 as a handoff for
implementation; all site counts and line numbers verified against `main` after
the `trim-some-more-fat` merge. Origin: [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md)
§5.2 ("deferred" item 7 in its §7 table), re-scoped here because the driver
layer moved since that audit — `src/rng.rs` / `src/block.rs` / `src/audio.rs`
no longer exist.

**Goal:** collapse ~35 raw `read_volatile`/`write_volatile` device-register
sites into a handful of `unsafe` constructor calls, so `unsafe` marks *which
addresses are device registers* once instead of marking every access.
Expected: **−25 to −35 `unsafe` blocks**, zero behavior change.

Estimated effort: half a day including verification. Low risk — every
conversion is behaviorally identical (same width, same address, same volatile
op), and the biggest consumer (rng) is on the boot path, so a broken
conversion fails loudly and immediately.

---

## 1. The site inventory (measured 2026-08-15)

In scope — all the same shape, `read_volatile((base + OFFSET) as *const uN)`:

| file | sites | widths | notes |
|---|---|---|---|
| `crates/akuma-virtio/src/rng.rs` | 30 | u32 | virtio-mmio legacy header registers. **The pilot** — self-contained driver, two thirds of the whole population |
| `crates/akuma-virtio/src/probe.rs:45` | 1 | u32 | `VIRTIO_MMIO_DEVICE_ID` read |
| `src/fw_cfg.rs` | 3 of its 5 | u16 (`:42` selector), u8 (`:50` data), u64 (`:129` DMA) | see exclusion below for the other two; this is why the type must be generic over width |
| `src/gic.rs` | 4 | u32, u8 | **optional** — already concentrated in four one-line accessors (`write_dist`/`write_dist8`/`write_cpu`/`read_cpu` at `:68`–`:94`); conversion is cosmetic |
| `src/console.rs` | 3 | u8 (DR), u32 (FR) | **optional, last** — this is the survives-when-the-allocator-broke path (`docs/reference/subsystems/console.md`). Convert only because `MmioReg::new` is `const` and needs no init; skipping it entirely is also a fine call |

Explicitly **out of scope**, each for a stated reason:

| file | sites | why excluded |
|---|---|---|
| `src/gic_v3.rs` | 2 | **Deliberate.** `gic_v3.rs:67` documents choosing non-volatile access because the optimiser lowers a volatile loop to a post-indexed store. Do not touch |
| `src/fw_cfg.rs:134` | 1 | `read_volatile(addr_of!(dma.control))` polls a **RAM** DMA descriptor the device writes back — not a register. Leave raw, with a comment saying so |
| `src/main.rs:204,206` | 2 | DTB header reads from RAM at boot, not device registers |
| `crates/akuma-pmm/src/lib.rs` | 3 | poison-word write/verify on RAM frames |
| `src/exceptions.rs`, `src/allocator.rs`, `threading/`, test files | — | not MMIO at all |

Page-table volatile access (`crates/akuma-exec/src/mmu/`) is a **separate
plan**: [`TRIM_FAT_PTE_NEWTYPE.md`](TRIM_FAT_PTE_NEWTYPE.md). Do not mix the
two sweeps — different files, different blast radius, different verification.

## 2. The type

Home: **`crates/akuma-primitives`** — the dependency-free leaf
([`AKUMA_PRIMITIVES_EXTRACTION.md`](AKUMA_PRIMITIVES_EXTRACTION.md)); both the
kernel and `akuma-virtio` already depend on it (checked 2026-08-15).

```rust
/// A memory-mapped device register of width `T`.
///
/// `unsafe` lives at construction: the caller vouches that `addr` is a device
/// register of exactly this width, mapped (Device-nGnRnE) for the kernel's
/// lifetime. After that, reads and writes are safe — a volatile access to a
/// vouched-for register cannot violate memory safety on its own.
#[derive(Clone, Copy)]
pub struct MmioReg<T>(*mut T);

impl<T: Copy> MmioReg<T> {
    /// # Safety
    /// `addr` is a device register of width `T`, mapped for the kernel's lifetime.
    pub const unsafe fn new(addr: usize) -> Self { Self(addr as *mut T) }
    #[inline] pub fn read(&self) -> T  { unsafe { self.0.read_volatile() } }
    #[inline] pub fn write(&self, v: T) { unsafe { self.0.write_volatile(v) } }
}
```

Decisions already made — do not re-litigate during implementation:

- **Generic over `T`**, because fw_cfg needs u8/u16/u64 and everything else is
  u32. No `u32`-only shortcut.
- **`const unsafe fn new`** so `src/console.rs` (if converted) and `fw_cfg`'s
  `const` register pointers keep working with zero init-order dependency.
- **No named-register struct layer** (`VirtioMmioLegacy { status, queue_sel, … }`)
  in this pass. It reads nicer but multiplies review surface; the win here is
  moving `unsafe`, not redesigning drivers. A per-driver
  `fn reg(base: usize, off: usize) -> MmioReg<u32>` helper (itself `unsafe` or
  private) is as far as this goes.
- `Send`/`Sync`: the raw-pointer field makes the type `!Send`/`!Sync` by
  default. rng's registers are accessed under its own locking today; only add
  `unsafe impl Send/Sync` if a converted driver actually stores the regs in a
  static, and say why at the impl.

Host unit test (in `akuma-primitives`): construct over a stack `u32`,
round-trip read/write. Volatile-on-RAM is well-defined, so this tests the
plumbing even though it can't test device semantics.

## 3. Conversion rules

The three ways this refactor could silently change behavior, and the rule for
each:

1. **Fences stay exactly where they are.** rng.rs interleaves
   `fence(Ordering::SeqCst)` between feature-select/read pairs (`:265`–`:293`).
   The conversion touches only the access expression, never the ordering
   around it.
2. **Read-and-discard stays a read.** `let _ = read_volatile(...)` in the
   feature negotiation becomes `let _ = reg.read();` — the read is the
   protocol step. Do not let a lint remove it.
3. **Endianness conversions stay at the call site.** fw_cfg's selector/DMA
   registers are big-endian (`key.to_be()`, `desc_phys.to_be()`); `MmioReg`
   stays endian-agnostic.

Order of work: type + unit test → `rng.rs` (pilot, boot-verified immediately)
→ `probe.rs` → `fw_cfg.rs` → optionally `gic.rs`, `console.rs`.

## 4. Verify

Per [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md):

- **Tier 1 + Tier 2** via `scripts/verify_trim.py`, A/B against a worktree at
  the parent commit. rng is v2-only and on the boot path (the runner sets
  `force-legacy=FALSE`), so Tier 2's boot is the real test of the pilot.
- `scripts/build_extreme_size.sh` — the 4.0 MB floor must hold. `MmioReg` is
  a zero-cost wrapper; confirm rather than assume.
- **One or two self-host clean-build trials**
  ([`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)
  § "Run a build trial") — rustc pulls entropy through `getrandom`, which is
  this driver. `cargo clean` first; a green incremental proves nothing.
- Tiers 3–5 do not apply: not an I/O-path, memory-path, or mmu change.

Metric, before and after (report the delta, per the trim-fat lesson that
line counts are the wrong metric — count `unsafe` sites):

```bash
grep -c 'read_volatile\|write_volatile' \
  crates/akuma-virtio/src/rng.rs crates/akuma-virtio/src/probe.rs \
  src/fw_cfg.rs src/gic.rs src/console.rs
```

Also report any **behavioral differences found between sites** during
conversion — every clone family so far has had 3–4, usually hiding in
comments and discarded reads.

Do not commit — leave the tree for review (repo convention: the user drives
all commits; run clippy + host tests first).

## 5. Outcome (2026-08-15)

Implemented as specified, with two deviations, both listed below. The type is
`crates/akuma-primitives/src/mmio.rs`; converted: `rng.rs`, `probe.rs`,
`fw_cfg.rs`. Not converted: `gic.rs`, `console.rs`.

**The metric, before → after.** The plan's §4 grep, over its five files:

| file | volatile ops before | after | note |
|---|---|---|---|
| `crates/akuma-virtio/src/rng.rs` | 30 | 0 | +2 textual hits remain, both in prose comments |
| `crates/akuma-virtio/src/probe.rs` | 1 | 0 | |
| `src/fw_cfg.rs` | 4 | 1 | the survivor is the RAM DMA-descriptor poll, left raw on purpose |
| `src/gic.rs` | 4 | 4 | not converted |
| `src/console.rs` | 3 | 3 | not converted |

**The headline estimate in this document was wrong, and worth correcting for the
next sweep.** It predicted "−25 to −35 `unsafe` blocks" from ~35 sites, which
silently assumed one `unsafe` block per access. Real code groups them: rng's 30
accesses lived inside **16** `unsafe` blocks, not 30. Counting blocks that vouch
for an MMIO address, the three converted files went **20 → 5** (rng 16 → 1,
probe 1 → 1, fw_cfg 3 → 3-but-now-`const`-and-once-per-register rather than
once-per-access-site, one of them formerly inside a loop). That is a −15, not a
−25/−35. The *shape* win is the real one and it did land: 34 vouching-at-access
sites became 5 vouching-at-construction sites.

Tree-wide the net is smaller still, and the honest number to quote: `MmioReg`
pays 2 `unsafe` blocks of its own (`read`, `write`) plus 2 in its unit tests, so
**43 → 32 blocks, −11 net** (−13 excluding tests). A newtype that concentrates
`unsafe` does not delete it — it moves it somewhere a reviewer can check once,
and the ledger should say so.

**Deviation 1 — `gic.rs` and `console.rs` were left alone.** Both take a
*runtime* base (`self.dist_base`, `self.base`) and both already concentrate
every access into three or four one-line accessors. Converting them means
constructing an `MmioReg` inside each accessor, i.e. an `unsafe` per accessor —
exactly the count they have now. The plan called both "optional" and "cosmetic";
measured, they are not even cosmetic, they are net-zero. Converting them would
only pay if the register handles were built once and stored as fields, which for
`console.rs` collides with its const-constructible, works-before-init contract
(`docs/reference/subsystems/console.md`) — the one file where that contract is
the whole point.

**Deviation 2 — `probe.rs` was converted despite also being net-zero** (1
`unsafe` → 1). It is one line and it is the *other* virtio register read in the
tree; leaving it raw would have made "all virtio-mmio register access goes
through `MmioReg`" false for a single line.

**Behavioral differences found between sites: none.** Every clone family so far
had 3–4; this one had zero, which is unsurprising — these were not clones of
each other, just repetitions of one idiom.

**Verification.** Stronger than the plan asked for, and the result is worth
recording as a template: `llvm-objdump -d --symbolize-operands --no-addresses`
over the whole kernel, before and after, normalising `.llvm.<cgu-hash>` suffixes
(they change on any recompile and are pure noise) — **0 of 8206 functions
differ**, and every section size is identical. The 144-byte file-size delta is
CGU-hash digits in the symbol/string tables. "Zero behavior change" is therefore
not an argument here, it is a measurement.

`scripts/verify_trim.py --tier all` A/B against a worktree at `24f7e1c1`
diffs to exactly two lines: `host.tests 555 → 557` (the two new `MmioReg` unit
tests) and `smp4.bkl_stuck 93 → 96`, which is the load-driven counter the script
itself documents as informational — a re-run on the same tree gave 93. All four
clippy configurations clean; SMP=1 and SMP=4 boot with `fail_set: (empty)` and
every Tier 3 exercise `ok`.

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) §5.2 — the original proposal and the
  audit that counted the (since-moved) sites; §6 for what must stay unsafe.
- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  — "Still deferred, genuinely" is where this item was parked, including the
  warning not to pick it up mid-Phase-3 (different blast radius than the
  driver merges).
- [`AKUMA_PRIMITIVES_EXTRACTION.md`](AKUMA_PRIMITIVES_EXTRACTION.md) — why
  primitives is the leaf and the feature-forward hazard to keep in mind when
  adding to it.
- `docs/reference/subsystems/console.md` § "Printing rules" — why touching
  `src/console.rs` is optional and last.
