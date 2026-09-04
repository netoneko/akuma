# Reducing platform dependency

**Date:** 2026-09-03. **Moved** from `proposals/` to `docs/archive/` on 2026-09-04:
two of its six items have landed and the rest have been argued against real evidence
rather than a hypothetical port, so this is now a record of findings with open items
in it, not a proposal awaiting a decision. It keeps its original structure — the
reasoning for an item that has *not* been done is the part worth preserving.

**Re-verified against current code 2026-09-04 — see §9.** Five of the six original
items hold up unchanged; §4 pointed at code that had already been fixed and needed
retargeting; a seventh item (process lifecycle) and a traced answer for what
actually blocks `akuma-exec` from x86_64 (§9.3-§9.4) are new. Read §9 before citing
this table — it corrects two rows below.

## Status of each item

| | Item | State |
|---|---|---|
| §0 | The arch gate (`target_os` alone stopped discriminating) | **APPLIED 2026-09-03.** 13 → 36 crates build for `x86_64-unknown-none`. Re-confirmed 2026-09-04 — and see §9.4 for a live instance of the exact bug this fixed, still unfixed inside `akuma-mmu` |
| §1 | PTE permissions are AArch64 bits inside a crate that forbids knowing that | **OPEN**, re-confirmed 2026-09-04: `crates/akuma-mmap/src/region.rs` still has `pub flags: u64` in `mmu::user_flags` terms; no portable `Prot` type exists anywhere in the tree yet. `amd64/src/paging.rs` has the `Prot`/`MemAttr` vocabulary §1.2 asks for, written from scratch because `MmapRegion.flags` cannot cross (`AKUMA_FIRECRACKER_AMD64.md` §3.5, §3.9.1). Two encodings now exist and neither can be handed to the other |
| §2 | Device discovery arrives as a DTB, threaded through the MMU | **OPEN**, re-confirmed 2026-09-04: x86_64 Firecracker passes no DTB at all — the memory map comes from `hvm_start_info`, parsed by `crates/akuma-ryzen-amd64` (moved out of `amd64/src/hvm.rs`, which no longer exists, 2026-09-04). The second consumer §2 predicted exists now, correctly on its own crate rather than merged with `akuma-fdt` |
| §3 | TLB invalidation cannot express *who* | **OPEN**, re-confirmed 2026-09-04: `crates/akuma-cpu/src/lib.rs`'s `mod tlb` still has only `tlbi` forms (`vmalle1`, `vaae1is`, `aside1is`, ...); `amd64/src/paging.rs` still carries its own local `invlpg`, not in the shared crate. `invlpg` is **core-local** where `tlbi ...is` broadcasts to the inner-shareable domain, so an x86 multi-core kernel must IPI. There is no way to say that difference in today's vocabulary (`AKUMA_FIRECRACKER_AMD64.md` §3.10.3) |
| §4 | `Context` is built by register name outside the crates that own registers | **RETARGETED 2026-09-04 — see §9.1.** `amd64/src/sched.rs`'s `Context` is no longer the evidence; it was rewritten to `{ rsp: u64 }`, private field, and its own header cites this as §4's fix shape *already applied*. The current second hand-built register block is `amd64/src/usermode.rs`'s `UserCtx` (7 public fields, `#[repr(C)]`, asm-indexed by offset) |
| §5 | Syscall numbers are constants, not a table | **APPLIED 2026-09-04.** `crates/akuma-syscalls-abi`, and the amd64 syscall handler dispatches through it. Re-confirmed 2026-09-04 |
| §6 | Write down the per-CPU-identity rule | **OPEN.** One paragraph, no code change |
| §9.2 | *(new)* Process lifecycle (fork/exec/wait4/spawn) hand-rolled a second time | **OPEN, not previously tracked.** `amd64/src/usermode.rs` reimplements this against a flat `PROCS[16]` array; getting `apk` working required rediscovering two bug classes `akuma-exec` already closed (publish-before-ready, no resource reclaim at exit) |

Every number below is measured — `scripts/cloc_akuma.py` for line counts, greps
reproduced inline for call-site counts. No estimate is inferred from `wc -l`; see §8
for why that distinction matters here. The crate counts in §0.1 were 34 when written
and are 36 now; regenerate rather than trusting either (§8.1).

**Related:** `docs/archive/AKUMA_FIRECRACKER_AMD64.md` (the port that produced the
evidence, one section per stage), `docs/archive/FIRECRACKER_PORT.md` (a second *machine*,
same architecture), `docs/reference/crate-safety.md`,
`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`,
`docs/archive/INLINE_ASM_CLEANUP.md`.

## The claim

Six places in this tree encode an AArch64 hardware fact inside a crate whose stated
contract is that it does not know about hardware. Each one has a cost **today**, on
the only architecture we run: a lossy encoding in the CoW and icache paths, a
device-discovery format threaded through the MMU, an invalidation API that cannot say
who it invalidated for.

This is not a port proposal. A second architecture would pay for all six at once,
which is how they were found, but none of them needs a port to be worth fixing and
none should be fixed *speculatively*. The ordering below is by present-day leverage,
not by how much of a hypothetical port each removes.

**Do not read this as a plan to build an arch abstraction layer.** §7 argues against
that specifically. The existing shape — one crate that owns instructions, `cfg`'d
function bodies, neutral vocabulary at the seams — is right. Four of the six items
below are "finish a seam that is already half-drawn."

## What is already right, and must not be regressed

Stated first because three of the items below touch this code, and the temptation
while touching it will be to "unify" something that is deliberately split.

- **`akuma-cpu` as the single instruction chokepoint.** 218 `asm!` sites became 35
  (`docs/archive/INLINE_ASM_CLEANUP.md`), and the discipline about what is
  deliberately *excluded* — `ttbr0_el1`, `elr_el1`, `vbar_el1`, `tpidr_el1`,
  `mov sp,x` — is the load-bearing half. Everything in this document gets easier
  because that crate exists.
- ~~**`target_os = "none"` gating, not `target_arch`.**~~ **Corrected 2026-09-03 —
  see §0.** This was wrong, and it was the single biggest blocker to an amd64
  build. `x86_64-unknown-none` is *also* `target_os = "none"`, so the gate stopped
  discriminating the moment a second bare-metal target existed. The correct gate is
  the conjunction, `all(target_os = "none", target_arch = "aarch64")`: the original
  reasoning (the dev host is itself `aarch64`, so `target_arch` alone is true under
  `cargo test`) is still valid and is why neither half works on its own. The rest of
  the tree — `akuma_primitives::preempt::current_tid` among others — still gates on
  `target_os` alone and carries the same latent bug.
- **`ENTRIES_PER_TABLE = 512`, `BITS_PER_LEVEL = 9`, `PAGE_SIZE = 4096`,
  `BLOCK_2MB` / `BLOCK_1GB`.** These are not AArch64 facts that leaked; they are
  genuinely shared by every 4-level 4 KB-granule MMU. Leave them alone.
- **`DevRegion` / `set_device_map()` / `device_map()`** in `akuma-mmu` is already a
  neutral device-window abstraction. Item 2 extends it rather than replacing it.
- **The `flush_tlb_{all,asid,page,range,range_all_asid}` vocabulary.** It already
  names address space and range. Item 3 adds one thing to it and changes nothing else.
- **virtio-MMIO with zero PCI** (`grep -c pci crates/akuma-virtio/src/*.rs` → 0).
- **36,391 lines of kernel test code containing six `asm!` sites**, all in
  `src/tests.rs` and `src/process_tests.rs` (`dc zva`, and the FP/SIMD register
  tests). The boot suite is neutral by accident and it is worth keeping that way.

---

## 0. The arch gate — found by building, fixed 2026-09-03

**APPLIED.**

Full record: `docs/archive/AKUMA_FIRECRACKER_AMD64.md`.

**Not in the original six. It outranks all of them**, and it was invisible until
something actually tried to build for a second architecture, which is the argument
for doing a bring-up spike before a refactor rather than after.

`akuma-cpu` gated every instruction on `target_os = "none"` and documented that
choice at length. That gate is true for `x86_64-unknown-none` too, so building any
consumer for x86_64 emitted AArch64 instructions into x86 codegen:

```
error: invalid instruction mnemonic 'mrs'
  --> crates/akuma-cpu/src/lib.rs:552:29
   |   mrs rax, tpidrro_el0
error: invalid instruction mnemonic 'wfi'
```

Because `akuma-primitives` calls this crate and almost everything calls
`akuma-primitives`, one crate took down three quarters of the tree.

### 0.1 Measured, before and after

```bash
for c in $(ls crates); do
  cargo build -q -p $c --target x86_64-unknown-none >/dev/null 2>&1 \
    && echo "OK   $c" || echo "FAIL $c"
done | sort | uniq -c -w4
```

| | builds for `x86_64-unknown-none` | fails |
|---|---:|---:|
| Before | 13 | 39 |
| After the gate fix | **34** | 18 |

The fix is two mechanical substitutions in `crates/akuma-cpu/src/lib.rs` — 33
`#[cfg(target_os = "none")]` arms became
`#[cfg(all(target_os = "none", target_arch = "aarch64"))]`, and 18 host-stub arms
were widened to `not(all(...))` so x86_64 takes them too. No aarch64 codegen
changed: the kernel builds and the host test suite passes unchanged.

### 0.2 What the remaining 18 are

Seven root failures holding 29 raw `asm!` sites; the other eleven are cascades.

| Crate | `asm!` sites | Neutral-able? |
|---|---:|---|
| `akuma-entry` | 8 | No — AArch64 exception-vector and boot entry |
| `akuma-gic` | 5 | No — it *is* the ARM interrupt controller |
| `akuma-threading` | 5 | Partly — context switch is arch, the slot table is not |
| `akuma-el0-entry` | 4 | No — `eret` and the EL0 trap frame |
| `akuma-mmu` | 3 | Partly — see §1; the walker is arch, the vocabulary is not |
| `akuma-psci` | 2 | No — `smc`/`hvc` are ARM firmware calls |
| `akuma-user-access` | 2 | Partly |

This is a *better* result than the 18.3% in §8 suggests, and it sharpens what that
number means: 18.3% of production code lives in crates that touch hardware, but
only 29 `asm!` sites are the part that cannot cross. The rest of those crates is
neutral code sharing a compilation unit with arch code — which is the seam §1-§4
are about moving.

### 0.3 The stub is a placeholder, not a port

x86_64 currently takes the *host* arm: `dsb_ish` is a no-op, `park::wfi` does not
park, `reg::sp` returns 0. That is survivable only because the amd64 target does
not call this crate yet, and it must not be mistaken for x86 support.

The split runs along exactly the fault line §1 and §3 describe. `barrier`, `park`
and `cache` have honest x86 bodies (`mfence`, `hlt`/`pause`, and no-ops that are
*correct* because x86 caches are coherent) — **done 2026-09-03**, with the two
lossy mappings (`isb`→`lfence`, and the `wfe`/`sev` pair) written up in those
modules' docs. `daif`, `tlb`,
`vtimer` and `sysreg` do not, because they return raw AArch64 encodings:
`daif::read()` yields a register whose bit 7 set means *masked*, where the x86
counterpart is `RFLAGS.IF` whose set bit means *enabled* — inverted polarity in a
`u64` that callers bit-test against AArch64 positions. Giving those an x86 arm
under an AArch64 mnemonic would be the "lossy encoding at a neutral seam" failure
this whole document is about, one level down.

---

## 1. The PTE permission vocabulary is AArch64 bits, in the crate that forbids knowing that

**OPEN.** Independent evidence arrived after this was written — see the status table.

**Highest leverage. The only item here that is a live imprecision rather than debt.**

`akuma-mmap` is the crate whose `Cargo.toml` carries an empty `[dependencies]` table
with a comment explaining that the emptiness *is* the enforcement — it "cannot
allocate a frame, edit a page table, take a lock, or name a `Process`." Its
`types.rs` header says it owns "the PTE permission vocabulary regions speak."

It does not own a vocabulary. It owns an encoding:

```rust
// crates/akuma-mmap/src/types.rs
pub mod flags {
    pub const AP_RW_ALL: u64 = 1 << 6;
    pub const AP_RO_ALL: u64 = 3 << 6;
    pub const AF:        u64 = 1 << 10;
    pub const NG:        u64 = 1 << 11;
    pub const SH_INNER:  u64 = 3 << 8;
    pub const PXN:       u64 = 1 << 53;
    pub const UXN:       u64 = 1 << 54;
}
```

Every `MmapRegion.flags` is a `u64` in that encoding, and it is passed raw across
public API boundaries that have nothing to do with page tables:

```rust
pub fn remap_current_user_page(va: usize, pa: usize, user_flags_val: u64) -> bool
pub fn update_current_user_page_flags(va: usize, new_flags: u64)
```

### 1.1 Why it is a defect today

`user_flags::EXEC` and `user_flags::RO` are **the same value**:

```rust
pub const RO:   u64 = flags::AP_RO_ALL;
pub const EXEC: u64 = flags::AP_RO_ALL;   // identical
```

and `is_exec` reads `UXN` alone:

```rust
pub const fn is_exec(flags: u64) -> bool { flags & flags::UXN == 0 }
```

`RO` does not set `UXN`. So **`is_exec(user_flags::RO)` is `true`**: a `PROT_READ`
mapping reports itself executable. Per `is_exec`'s own docstring, that predicate is
"the one that decides whether a demand-paged frame needs the `dc cvau` + `ic ivau`
sequence" — so today every read-only demand-paged frame buys icache maintenance it
cannot need, and `is_exec` can never be read to *deny* a fetch. Which is precisely
the failure shape `GRANT_RECORDS_VS_DENY_RECORDS.md` is about: a record that only
ever grants, whose first use as a denial is a false refusal.

The aliasing is currently invisible because `user_flags::EXEC` has **no production
consumer** — it appears three times, all in `akuma-mmap`'s own test module:

```
$ grep -rn 'user_flags::EXEC' crates src --include='*.rs'
crates/akuma-mmap/src/types.rs:262   # test
crates/akuma-mmap/src/types.rs:285   # test
crates/akuma-mmap/src/types.rs:305   # test
```

A dead constant that is silently equal to a live one is a trap primed for whoever
reaches for the obvious name.

### 1.2 Fix shape

Replace the `u64` with two types and make `akuma-mmu` the only crate that knows bit
positions:

1. `Prot` — a small `Copy` struct or bitflags over `{read, write, exec, user}`.
   `NONE`/`RO`/`RW`/`RX`/`RW_NO_EXEC` become constructors; `from_prot(prot: u32)`
   stays where it is. `EXEC` ceases to exist, because "readable and executable" is
   `RX` and there is no third thing.
2. `MemAttr` — `NormalWb` / `NormalNc` / `DeviceNGnRnE`, replacing the `MAIR_*`
   indices that `akuma-mmu::types` re-exports. AArch64 encodes this as an AttrIndx
   into MAIR; other MMUs encode it differently; no consumer cares which.
3. `akuma-mmu` gains `encode(prot: Prot, attr: MemAttr) -> u64` and
   `decode(pte: u64) -> (Prot, MemAttr)`, and is the only crate that names `AP_*`,
   `UXN`, `PXN`, `AF`, `SH_*`, `NG`.

The predicates survive as methods with their existing names and their existing
docstrings — `is_write`, `is_exec`, `is_none`, `is_read_only_to_user`,
`prot_recorded`. That is what keeps the diff mechanical: most consumers already call
a predicate rather than masking bits themselves.

### 1.3 Blast radius — measured

```
$ grep -rn 'user_flags::\|mmu::flags::\|mmap::flags::' crates src --include='*.rs' | wc -l
254
```

Distributed:

| Where | Sites | Note |
|---|---:|---|
| `akuma-mmap` (defining crate) | 111 | mostly its own tests |
| `src/tests.rs`, `src/process_tests.rs` | 79 | boot suite; nearly all `map_page(va, pa, user_flags::RW_NO_EXEC)` |
| `akuma-exceptions` | 23 | the fault handler — the one place that genuinely reasons about bits |
| `akuma-exec` | 16 | |
| `akuma-mmu` | 12 | stays; this crate is allowed to know |
| `akuma-syscalls-glue` | 5 | |
| `akuma-elf` | 5 | |
| `akuma-fpcache` | 3 | |

**64 production sites outside the defining crate**, of which 23 are in one file. The
79 boot-suite sites are a find-and-replace of constant names. This is a two-to-three
day change, not a refactor.

### 1.4 Verification

The permission logic is already covered by boot-suite tests that assert the
`FaultAccess` → `lazy_map_flags` → `is_exec` chain (`src/process_tests.rs:9407`
onward, the `data/file RX`, `inst/anon`, `non-exec file mapping` cases). Those tests
should be converted, not rewritten, and their assertions should get *stronger* on the
way: `is_exec(Prot::RO)` must become `false`, which is a behaviour change and needs a
line in the archive doc when it lands.

---

## 2. Device discovery arrives as a DTB and is threaded through the MMU

**OPEN.** The second, non-DTB machine this predicted now exists: PVH's
`hvm_start_info`, parsed by `crates/akuma-ryzen-amd64` (not `amd64/src/hvm.rs` —
that module moved into the crate 2026-09-04 along with `cmdline.rs`/`acpi.rs`;
see `amd64/src/machine.rs`'s header. Corrected 2026-09-04, §9).

`akuma-mmu` depends on `akuma-fdt` and hands a device tree to its callers:

```rust
pub fn with_boot_identity_fdt<R>(pa: usize, f: impl FnOnce(Option<&akuma_fdt::Dtb<'_>>) -> R) -> R
```

The memory map, the device windows, the CPU list and the timer frequency all reach
their consumers in device-tree shape, through the crate that manages page tables.

### 2.1 Why it is a defect today

We already run two machines with different discovery quirks — QEMU virt and
Firecracker — and `docs/archive/FIRECRACKER_PORT.md` documents the memory-map and
constant differences between them as a table of corrections rather than as data the
kernel reads. The `akuma-firecracker` crate (221 production lines) exists partly to
hold what amounts to platform facts. Those facts have no single home.

A DTB is also the wrong *lifetime* for this: `with_boot_identity_fdt` exists because
the blob is only reachable through the boot identity map, so every consumer that
wants a platform fact must either take it early or re-enter that window.

### 2.2 Fix shape

One neutral `PlatformInfo`, produced once during boot and stored:

```rust
pub struct PlatformInfo {
    pub ram: [MemRegion; MAX_MEM_REGIONS],
    pub ram_len: usize,
    pub devices: [DevRegion; MAX_DEV_REGIONS],   // already exists, unchanged
    pub cpus: [CpuInfo; MAX_CPUS],               // id + enable method
    pub cpu_len: usize,
    pub timer_hz: u32,
    pub console: Option<DevRegion>,
}
```

`akuma-fdt` becomes a *producer* of that struct rather than a type consumers name.
`akuma-mmu` takes a `&PlatformInfo` and loses its `akuma-fdt` dependency.
`with_boot_identity_fdt` becomes an implementation detail of the producer.

`DevRegion` and `MAX_DEV_REGIONS` already exist and are already the right shape —
this item is mostly "extend the half of this that was done to cover the rest," which
is why it is item 2 and not item 5.

### 2.3 Cost

Roughly 200-300 lines of new type plus producer, and a mechanical rewrite of the
`with_boot_identity_fdt` call sites. The honest risk is boot ordering: the struct must
be populated before the MMU wants the device map and after the identity window
exists, and getting that wrong is a boot hang rather than a compile error. Land it
behind the existing `Registered<T>` discipline in `akuma-not-even-once` so a
missing-population bug names the `init` that was skipped instead of faulting.

---

## 3. TLB invalidation cannot express *who*

**OPEN**, and sharper than when written: see the status table.

```rust
pub fn flush_tlb_all()
pub fn flush_tlb_asid(asid: u16)
pub fn flush_tlb_page(va: usize)
pub fn flush_tlb_range(start_va: usize, pages: usize)
pub fn flush_tlb_range_all_asid(start_va: usize, pages: usize)
```

Eighteen call sites in `crates/` outside `akuma-mmu`:

```
$ grep -rn 'flush_tlb_' crates --include='*.rs' | grep -v 'akuma-mmu/' | wc -l
18
```

The vocabulary is good — it already names the address space and the range. What it
cannot say is which cores the invalidation covers, or when it has landed on them,
because `tlbi ...is` broadcasts and `dsb ish` is the completion. Both are implicit in
the instruction.

### 3.1 Why it is a defect today

Four of this project's most expensive investigations are properties of exactly this
API's implicit half — `fork_cow_tlb_asid_flush` (wrong-ASID flush let a parent write
a shared CoW page), `page_table_uaf_ttbr_gate_fix`, `oncpu_gate_scheduler_race`,
`cowstale_stale_write_fault_fixed`. In each, the question that took the time was
"which cores could still be holding this translation," and the code offered no place
to write the answer down. The `TTBR_TRACK_CORES` / `publish_l0_begin` /
`any_core_on_l0` machinery in `akuma-mmu` is the answer, built separately, and it is
not connected to the invalidation calls that need it.

There is a performance argument too, though it is secondary: several of the 18 sites
broadcast inner-shareable when the mapping is provably single-core, and there is
currently no way to say so.

### 3.2 Fix shape

Do **not** build shootdown machinery. Add only the ability to state the target and
the completion:

```rust
pub enum TlbTarget { ThisCore, AllCores }

#[must_use]
pub fn flush_tlb_asid(asid: u16, target: TlbTarget) -> TlbFlush;
```

where `TlbFlush` is a `#[must_use]` token whose `Drop`, on AArch64, is the `dsb ish`
we already emit — so the change is free at runtime and the type is what carries the
obligation. On a machine without broadcast invalidation the same token is where an
IPI acknowledgement wait would go.

`#[must_use]` here is deliberate and matches the precedent in `akuma-psci`, where the
PSCI call functions carry it because "dropping a PSCI return is how a failed `CPU_ON`
goes unnoticed." Dropping a flush completion is the same kind of silence.

### 3.3 Cost

18 call sites plus the `akuma-mmu` internals. Half a day of typing; the real work is
deciding `ThisCore` vs `AllCores` per site, which is exactly the audit that has been
implicitly re-run during each of the four investigations above.

---

## 4. `Context` is built by register name outside the crates that own registers

**OPEN.** A second hand-built `Context` now exists in `amd64/src/sched.rs`.

```rust
// crates/akuma-exec-core/src/thread.rs
pub struct Context {
    pub magic: u64,
    pub x19: u64, /* ... */ pub x30: u64,
    pub sp: u64, pub daif: u64, pub elr: u64, pub spsr: u64, pub ttbr0: u64,
    pub user_entry: u64, pub user_sp: u64, pub user_tls: u64, pub is_user_process: u64,
}
```

Nineteen sites in `akuma-exec` name these fields directly:

```
$ grep -rn '\.x19\|\.x20\|\.x30\b\|\.spsr\b\|\.daif\b\|\.ttbr0\b\|\.elr\b' \
    crates/akuma-exec/src --include='*.rs' | wc -l
19
```

(13 in `process/mod.rs`, 6 in `address_space.rs`, plus 24 more in the boot suite.)

### 4.1 Why it is a defect today

`akuma-exec` is fork, exec, signals, channels, fds and lifecycle. It has no business
knowing the callee-saved register set. More concretely: the fields are `pub` and
mutable, so nothing prevents a caller from setting `spsr` — which is the single field
`akuma-el0-entry` added a runtime check for, because an `EL1h` value there converts
`enter_user_mode` into "jump to this address with kernel privilege." That crate
discharges the obligation at the `eret`; the type still lets anyone create the
violation 19 call sites earlier.

### 4.2 Fix shape

Constructors and accessors; register block private:

```rust
impl Context {
    pub fn for_user(entry: u64, user_sp: u64, tls: u64, root: u64) -> Self;
    pub fn for_kernel(entry: u64, sp: u64) -> Self;
    pub fn pc(&self) -> u64;
    pub fn sp(&self) -> u64;
    pub fn set_return_value(&mut self, v: u64);
}
```

`for_user` sets `spsr` to EL0t and there is no other way to reach the field, which
turns `enter_user_mode_checked`'s runtime compare into a defence in depth rather than
the only defence.

Cheap, mechanical, and independently worth doing. Keep `#[repr(C)]` — the context
switch asm indexes it by offset.

---

## 5. Syscall numbers are constants, not a table

**APPLIED 2026-09-04** — `crates/akuma-syscalls-abi`.

`crates/akuma-syscalls-linux/src/nr.rs` is 273 lines of `pub const NAME: u64 = n;` in
the Linux `asm-generic` numbering, and dispatch in `akuma-syscalls-glue` matches on
those constants directly (192 `nr::` references).

The file's own header already made the right call once — it removed the
`#[cfg(feature = ...)]` gates because "a syscall number is a fact about Linux, not
about which features this build compiles in." The remaining coupling is one level up:
the numbering is a fact about Linux *on a particular architecture*, and the file name
does not say which.

### 5.1 Why it is a defect today

Minor, and this item is here for completeness rather than urgency. The practical
cost is that there is no way to ask the tree "which syscalls does this build actually
implement," because that answer is spread across 192 match arms and a set of `sc-*`
feature gates. `scripts/` has several harnesses that would rather ask than grep.

### 5.2 Fix shape

A `Syscall` enum, plus `decode(nr: u64) -> Option<Syscall>` owned by the ABI crate,
with dispatch matching the enum. The constants stay — they are still the wire facts —
but they stop being the dispatch key.

~~Defer this one.~~ **Started 2026-09-04 — the amd64 port overtook this.** The
argument for deferring was that the numbering's architecture-dependence had no
present-day cost. With a second architecture it has one, and a sharp one: `0` is
`read` on x86_64 and `io_setup` under `asm-generic`, so the wrong table finds the
*wrong handler* rather than no handler.

`crates/akuma-syscalls-abi` now owns a `Syscall` enum plus both tables, with host
tests pinning the divergence. It is a **new crate**, not an addition to
`akuma-syscalls-linux`, because that crate is precisely "the Linux/**aarch64**
ABI" and a second architecture's numbers inside it would make its own name false.
The constants stay where they are and the 192 `nr::` call sites are untouched —
this adds a way to dispatch by name, it does not migrate anything.
`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.12.

---

## 6. Write down the per-CPU-identity rule before something depends on it being free

**OPEN.** Still one paragraph and no code change.

`akuma-primitives::preempt::current_tid()` reads `TPIDRRO_EL0`;
`akuma-primitives::cpu::current_core_id()` is the same shape. Both are callable at
every instruction boundary, including the first instruction of an exception vector,
and a great deal of code assumes this without saying so.

That property is an AArch64 gift, not a design decision, and it is worth one
paragraph in `akuma-primitives`' header stating it as an invariant that the kernel
*relies on* — because the moment it is not true (any architecture where the per-CPU
base is established by an instruction rather than always live), the failure mode is a
fault inside the fault handler, and no compiler diagnostic precedes it.

No code change. A comment, in the crate that would be lying if it were removed.

---

## 7. What not to do

**Do not introduce `trait Arch` with dynamic dispatch.** It is the obvious shape and
it is wrong here. A vtable in the fault path and the TLB path costs real time on the
hottest code in the kernel, and it buys nothing the existing pattern lacks: the tree
already selects implementations with `cfg`'d function bodies (`flush_tlb_all` has
three, `current_tid` has two) and by swapping whole crates. That is monomorphic,
zero-cost, and already understood by everyone reading it.

**Do not do any of this for a hypothetical port.** Item 1 is worth doing because
`is_exec(RO)` is wrong today. Item 3 is worth doing because four archive documents
say so. Items 2 and 4 are worth doing because they close seams that were already
drawn most of the way. If the argument for a change is only "a second architecture
would need it," the change should wait for the second architecture.

**Do not widen `akuma-cpu`.** Three of the items touch it, and the pressure will be
to move "just one more" instruction in. Its exclusion list — writes to `ttbr0_el1`,
`elr_el1`, `vbar_el1`, `tpidr_el1`, `mov sp,x`, the GIC `ICC_*` writes — is the part
that carries the safety argument.

---

## 8. Appendix: measurement, and a correction

An earlier pass at sizing this used `wc -l` and reported "150k lines of kernel."
That number is physical lines. This tree carries 0.52 comment lines per code line
and 40% of its code is tests, so physical lines overstate the production surface by
2.8x (152,719 / 54,628). From
`python3 scripts/cloc_akuma.py src crates`:

| | value |
|---|---:|
| Rust code lines (`src` + `crates`) | 89,315 |
| **Production code** | **54,628** |
| Test code | 36,391 (40%) |
| Physical lines | 152,719 |
| comment / code | 51.7% |

A second correction worth recording: **`src/` is 22,181 code lines of which 136 are
production.** After the extraction campaign it is the boot self-test suite, not
kernel code. Any future sizing that treats `src/` as kernel is wrong by two orders of
magnitude.

Production code in the crates that encode a hardware fact, for scale:

| Crate | Production lines |
|---|---:|
| `akuma-exceptions` | 3,109 |
| `akuma-threading` | 2,978 |
| `akuma-mmu` | 1,842 |
| `akuma-entry` | 589 |
| `akuma-cpu` | 383 |
| `akuma-user-access` | 334 |
| `akuma-gic` | 255 |
| `akuma-timer` | 197 |
| `akuma-el0-entry` | 100 |
| `akuma-fdt` | 76 |
| `akuma-psci` | 67 |
| `akuma-uart` | 53 |
| **Total** | **9,983 — 18.3% of production** |

That 18.3% is the number this document is about. It is not large, and the six items
above are the reason it is not larger: the crate split, `akuma-cpu`, the `target_os`
gating and the `DevRegion` table already did most of the work. What remains is
finishing four seams and writing down two invariants.

### 8.1 Regenerating these numbers

```bash
python3 scripts/cloc_akuma.py src crates          # tables above
grep -rn 'user_flags::\|mmu::flags::\|mmap::flags::' crates src --include='*.rs' | wc -l
grep -rn 'flush_tlb_' crates --include='*.rs' | grep -v 'akuma-mmu/' | wc -l
grep -rn '\.x19\|\.x30\b\|\.spsr\b\|\.daif\b\|\.ttbr0\b' crates/akuma-exec/src --include='*.rs' | wc -l
```

Never increment these by hand.

## 9. Addendum, 2026-09-04: re-verified against current code, not against this document

Re-checked every claim above by reading the current source and `amd64/Cargo.toml`
directly — not by trusting the table, and not by trusting commit dates (this tree's
history is same-day for nearly everything touched here, so "when" a line landed is
not a reliable signal; "what the line says right now" is). Four items (§0, §1, §2,
§3, §5, §6) hold up exactly as stated. §4 does not, and one real gap has opened up
since this document's original pass that none of the six items describe.

### 9.1 §4's evidence moved out from under it

The claim as written — *"a second hand-built `Context` now exists in
`amd64/src/sched.rs`"* — is no longer true, and `sched.rs` says so about itself:

```rust
// amd64/src/sched.rs
pub struct Context {
    rsp: u64,   // private; built only by Context::for_task
}
```

Its own module header, under "Relationship to proposal item 4", calls this **the
fix already applied**: *"the `Context` here is what that argument looks like taken
seriously: it holds one field, `rsp` ... once the only way to build a context is a
constructor, the register set stops being part of the interface at all."* Whoever
wrote it read §4, agreed, and built the minimal-context shape it recommends —
independently of `akuma-exec-core::thread::Context`, which is still exactly as §4
describes it (nineteen named-register call sites, public mutable fields). The
table below was never updated to say that one instance of the defect is now a
worked example of the fix instead of a second occurrence of it.

The defect itself is real and current — it just moved. `amd64/src/usermode.rs`'s
`UserCtx` is today's second hand-built register block, and it is the textbook
shape §4 warns about:

```rust
#[repr(C)]
pub struct UserCtx {
    pub kernel_rsp: u64,       // Offset 0  — syscall_entry indexes [rax + 0]
    pub user_rsp: u64,         // Offset 8  — [rax + 8]
    pub leave: u64,            // Offset 16 — [rax + 16]
    pub proc_slot: usize,      // Offset 24
    pub user_rip: u64,         // Offset 32 — [rax + 32]
    pub fs_base: u64,          // Offset 40
    pub saved_regs: [u64; 12], // Offset 48
}
```

Seven public fields, asm-indexed by literal offset, no constructor gate — nothing
stops any caller setting `user_rip` to a kernel address the same way nothing
stopped `akuma-exec-core::Context::spsr` from naming `EL1h` before `akuma-el0-entry`
added a runtime check for it. §4.2's fix shape (constructors, private register
block, keep `#[repr(C)]` for the asm) applies to this struct unmodified.

### 9.2 A seventh item, not covered by any of the six: process lifecycle

None of §1-§6 existed as a concern when this document was written because the code
they would describe did not exist yet — `amd64/src/usermode.rs`'s `sys_execve`
(~84 lines), `sys_fork` (~102 lines), `sys_spawn` (~98 lines), plus `wait4`/`vfork`/
`spawn_stdio`/`spawn_record_exit` scattered across another ~200 lines, all against
a flat `PROCS: [Option<Process>; 16]` array — has no analogue in the six items
above and duplicates a real, working thing: `akuma-exec`'s fork/exec/wait4/spawn
state machine, unreachable here for the same root cause as §1 and §3 (§9.3 below).

This is not free duplication. Getting `apk` to install packages
(`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.31-§3.32) required independently
rediscovering two bug classes `akuma-exec` already closed once:

- **Publish-before-ready.** `sched::spawn()` marked a new slot `Runnable` before
  `spawn_in_space` had written `space_root` into it. A LAPIC tick landing in that
  window let the scheduler pick the slot with a stale root, install the kernel's
  CR3, and `eret`/`sysret` a freshly-spawned `sshd` into ring 3 with kernel page
  tables — a page fault on its own entry point, intermittent (timer-phase
  dependent), and silent until it was decoded from a raw `#PF` log. `akuma-exec`'s
  threading module encodes "don't become visible to the scheduler until every
  field is live" as an invariant (the TERMINATED→FREE slot-reuse discipline, the
  reap callback wired at exactly one point). `amd64/src/sched.rs` had to relearn
  the same rule from a live crash and fix it by hand
  (`spawn_unpublished`/`publish_task`, state = Runnable as the last store).
- **Nothing closes a task's resources at exit.** `amd64/src/fd.rs`'s descriptor
  table is one 16-slot array shared by every task, and nothing freed a task's
  entries when it exited — real Linux does this implicitly at `exit`/
  `exit_group`. A single `apk add curl` run left the table full; the *next*
  `apk` invocation, a fresh process, inherited an already-exhausted table and
  failed before doing any work. Fixed with an `owner` field per entry and a
  `close_owned_by()` sweep called once from `run_process` at exit — a smaller,
  local version of the same "reclaim what a dead task held" job
  `akuma-exec`'s slot-table reap callback already does generically.

Not proposing a merge — §9.3 explains why one isn't reachable today. Proposing
this get a name, the way §1-§6 have names, so the next instance of "amd64
rediscovers an invariant `akuma-exec` already encodes" is recognized as that on
sight instead of debugged from first principles again.

### 9.3 What actually blocks `akuma-exec` from x86_64, traced by dependency, not by guess

`akuma-exec`'s own `[dependencies]` names eleven `akuma-*` crates. Checked each
one directly (its own `Cargo.toml`, not what a comment elsewhere claims about it):

```
akuma-exec
├─ akuma-mmu ─────────────────────────────► the one real blocker (9.4)
├─ akuma-elf     ── akuma-mmu ────────────► blocked transitively
├─ akuma-threading ── akuma-mmu ──────────► blocked transitively
├─ akuma-el0-entry ── akuma-threading ────► blocked transitively (two hops)
├─ akuma-user-access ── akuma-mmu ────────► blocked directly
├─ akuma-bkl ──────────────────────────────► clean (deps: primitives, syscalls-linux only)
├─ akuma-slot-table ───────────────────────► clean (deps: primitives only)
├─ akuma-mmap ──────────────────────────────► clean (empty [dependencies])
├─ akuma-cpu, akuma-exec-core, akuma-primitives,
│  akuma-pmm, akuma-syscalls-linux, akuma-terminal ► already in amd64/Cargo.toml directly
```

Every blocked crate is blocked because it names `akuma-mmu`, directly or through
one more hop. Three crates in the list (`akuma-bkl`, `akuma-slot-table`,
`akuma-mmap`) are **not** blocked at all — their own dependency graphs are already
arch-neutral, they simply aren't wired into `amd64/Cargo.toml` yet because nothing
on that target needs them yet (no BKL contention with one core, no generic
process table, no region bookkeeping — `amd64/src/mm.rs`'s own header names this
gap explicitly: building one locally "would be building the thing `akuma-mmap`
already is").

### 9.4 `akuma-mmu` is not blocked by a dependency — it names AArch64 hardware in its own body, and one live gate hides that

`crates/akuma-mmu/src/lib.rs` (3,470 lines) has ten `#[cfg(target_os = "none")]` /
`#[cfg(not(target_os = "none"))]` pairs and **zero** `#[cfg(target_arch = ...)]`
anywhere in the file. Every `target_os = "none"` branch — the one that runs on
real hardware — contains unconditional AArch64: `TTBR0_EL1` reads, ASID-broadcast
`tlbi`, the `AP[7:6]`/`UXN`/`PXN`/`AF` translation-table bit layout the amd64
`paging.rs` comparison table lists as having no x86 encoding at all. The
`not(target_os = "none")` branch is a host-test stub, not an x86-kernel
implementation — it exists so `cargo test` can link, not so `x86_64-unknown-none`
can run the real path.

`get_boot_ttbr0` is the sharpest example, because it contains real assembly:

```rust
#[cfg(target_os = "none")]
pub fn get_boot_ttbr0() -> u64 {
    unsafe {
        core::arch::asm!(
            "adrp {tmp}, boot_ttbr0_addr",
            "add {tmp}, {tmp}, :lo12:boot_ttbr0_addr",
            "ldr {out}, [{tmp}]",
            tmp = out(reg) _, out = out(reg) addr,
        );
        addr
    }
}
#[cfg(not(target_os = "none"))]
pub fn get_boot_ttbr0() -> u64 { 0 }
```

`x86_64-unknown-none` satisfies `target_os = "none"` exactly as `aarch64-unknown-none`
does (confirmed directly: `rustc --print cfg --target x86_64-unknown-none` reports
`target_os="none"`) — so this gate does not distinguish the two architectures at
all; it distinguishes "real kernel" from "host test." **This is the identical bug
class `akuma-primitives::preempt.rs`'s `current_tid()` carried until it was found
and fixed on 2026-09-04** (`crates/akuma-primitives/src/preempt.rs`'s own header:
*"This read `#[cfg(target_os = "none")]` until 2026-09-04 ... `x86_64-unknown-none`
is also `target_os = "none"`, so that gate selected the AArch64 body on x86"*). That
fix was made *because* the amd64 target started linking a crate reachable through
this exact mistake. `akuma-mmu` still has ten instances of the same gate, unfixed,
because nothing has linked it into an x86_64 binary yet to find them — `adrp`/
`:lo12:`/`ldr` are not valid x86_64 assembly, so the first thing to actually try
would fail loudly at codegen, not silently the way `current_tid()` did. (A plain
`cargo build -p akuma-mmu --target x86_64-unknown-none` does **not** catch this:
the workspace release profile uses ThinLTO, so a standalone per-crate build defers
real instruction selection to the final link and reports success regardless of
what the AArch64-only branches contain. Confirmed directly — the resulting
`.rlib` carries zero defined text symbols. Don't use that command as a portability
check; it doesn't do one.)

So, directly: **nothing blocks `akuma-mmu` from *building* standalone. What blocks
it from *working*, and from `akuma-exec` reaching x86_64 through it, is that its
page-table logic is AArch64's translation-table format with no x86_64 branch
anywhere in the crate** — not a missing feature, not a Cargo-graph problem, a
different CPU's hardware format written as if it were the only one. Fixing this
is §1 and §3's fix shape applied to the crate that most needs it: a portable
`Prot`/permission vocabulary at the boundary (§1), and an explicit target-arch
split — or a second, x86-native crate below `akuma-exec` the way `amd64/src/
paging.rs` already is — for the parts that are page-table format, not policy.
Nobody has done this because nothing has needed `akuma-exec` on x86_64 badly
enough yet to pay for it; §9.2 is what that cost looks like when it's paid
piecemeal, by hand, one rediscovered bug at a time, instead.
