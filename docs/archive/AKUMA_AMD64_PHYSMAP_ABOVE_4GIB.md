# amd64: the kernel could only see 15% of a 16 GiB machine

**2026-09-12.** The HP reference box ("the dumpster") has 16 GiB. It booted,
printed `ram: 16321 MiB usable across 8 regions`, and then gave userspace
2504 MiB. `free` inside the guest agreed:

```
Mem:        3354104     1047097     2296808           0       10199     2296792
```

3.2 GiB total on a 16 GiB machine, and nothing in the boot log between the two
numbers accounting for the difference.

## What the log actually said

```
  ram:  16321 MiB usable across 8 regions          <- the memory map
  ram:  0x0000000000100000 .. 0x00000000ccc7e000   <- what the PMM was given
  kernel ends 0x000000000043f3f0
  heap: 0x0000000010440000 + 512 MiB ... ok
  pmm:  init(base=0x100000, size=3275 MiB, reserved_to=0x30440000)
  pmm:  641086 free frames (2504 MiB)
```

3275 MiB chosen out of 16321, then 771 MiB of that reserved (a 4 MiB kernel, a
256 MiB root image GRUB placed at `0x440000`, a 512 MiB heap), leaving 2504.

## Three causes, in order of size

### 1. `PHYSMAP_LIMIT` was 4 GiB — ~13 GiB, silently

`boot.s` built **four** page directories, so the physmap covered the low 4 GiB,
and `phys_to_virt` asserts against that. A PC displaces the RAM sitting behind
the MMIO hole to just above 4 GiB, so seven of this machine's eight usable
regions began at `0x1_0000_0000`. `mem::init` clipped each one:

```rust
let end = r.end().min(PHYSMAP_LIMIT);
if end <= base { continue; }          // every region with base >= 4 GiB
```

13046 MiB left through that `continue`, and it printed nothing.

### 2. The PMM manages exactly one region

`akuma_pmm::init(base, size, kernel_end)` has one `base_addr`, one bitmap, one
`total_pages`. `mem::init` therefore picked one region for **both** the heap and
the PMM, ranked by "most room". Raising the limit alone would not have helped
much: the ranking would simply have flipped to the 13 GiB region and dropped the
3 GiB one instead.

### 3. 771 MiB reserved inside a 3275 MiB region

A 16% overhead when the region is 3 GiB; 5% once it is 13.

## The fix

* **`PHYSMAP_LIMIT` 4 GiB → 64 GiB**, one page directory per GiB. The count is
  `phys::PHYSMAP_PDS`, passed into `boot.s` as a `global_asm!` `const` operand
  rather than written down twice — the constant and the tables are now the same
  value.
* **`mem::init_reserving` chooses two regions, not one**: the PMM gets the
  largest reachable one, and the heap is carved out of the region holding the
  kernel image when that region has room (falling back to the PMM's otherwise,
  which is what the 2026-09-06 `HEAP_SIZE` bump needs). A machine reporting one
  region selects it for both and the behaviour is unchanged.
* **Every region is accounted for in the log**, with what became of it.
* `akuma-multiboot2`'s `usable_coalesced` **merges on the way in** instead of
  filling `out` with raw fragments and merging afterwards — see below.

## Two bugs found while validating, both of which would have hit the metal

### 2a. The physmap past 4 GiB aliased physical 0

The fill loop builds each PDE's physical address in `%eax`, because it runs in
32-bit protected mode. At 2048 entries `addl $0x200000, %eax` **wraps**, so
entry 2048 was written with physical 0 instead of 4 GiB and the physmap became a
second alias of the low 4 GiB.

Nothing faulted. A page table allocated up there is written and read back
through the same wrong alias, so it is *self-consistent to every walk the kernel
performs* — only the CPU's own page walker, which uses the real frame, disagrees.
The symptom was a not-present `#PF` on the first virtio register read, at a
virtual address `paging::translate` reported in the same breath as correctly
mapped:

```
  [dbg] map 0xffff8080feb00000 -> pa 0xfeb00000 translate=0xfeb00000
  #PF: not-present read from ring 0   cr2=0xffff8080feb00008
```

`adcl $0, %edx` into the high dword is the whole fix. `mem::smoke_test` now
walks the live tables for `PHYSMAP_LIMIT - 4096` and for 4 GiB on every boot;
reverting the `adcl` turns both red at `-m 2048`, so the test does not need a
large machine to catch it.

### 2b. The AP boot root must be below 4 GiB

`ApBootTables::build` allocated the secondaries' PML4 from the PMM. Once the PMM
was above 4 GiB, `SMP=4` died during `smp: cpu 1 online` with QEMU exiting and
no output at all.

The AP trampoline loads CR3 with `movl AP_MB_CR3, %eax; movl %eax, %cr3`, and it
has no choice: CR3 is a 32-bit register until long mode is on, and setting CR3 is
how the trampoline gets *into* long mode. A root at `0x1_2000_0000` is loaded as
`0x2000_0000` and the core triple-faults on its first paged instruction.

`smp::ap_boot_root` now builds it in a static page in `.bss.pagetables`
(`__ap_pml4`) whose slot 0 points at `boot.s`'s own `__pdpt_low` — the identity
map `drop_identity_map` unlinked but never destroyed. One static page instead of
three PMM frames, no second identity map to keep in agreement with the first, and
the "no frames for the AP boot tables" failure path is gone. A runtime check
refuses bring-up with a message if the root is ever above 4 GiB.

### And one latent one: the region cap was applied to fragments

`usable_coalesced` filled a 16-slot array from the **raw** map and merged
afterwards, dropping overflow in arrival order. UEFI fragments heavily and GRUB
reports ascending, so on a machine with more than 16 raw usable entries the
**last** one is dropped — and on a PC the last one is the high-memory region
holding most of the RAM. This machine's map merged to 8, so it never bit.
Merging on the way in means a full array means 16 genuinely disjoint regions;
if it does overflow, the smallest run is what goes.

## Measured, QEMU `-M microvm`, same disk, same command line

| | baseline | fixed |
|---|---|---|
| `-m 2048` | 1530 MiB free, 665 tests | 1530 MiB, 667 tests |
| `-m 8192` | 2554 MiB free | **5120 MiB** |
| `-m 16384` | 2554 MiB free | **13312 MiB** |
| `-m 8192 SMP=4` | 675 tests pass | 677 tests pass |
| `-m 16384 SMP=4` | (not run) | 677 tests pass |

The two extra tests are 2a's. The 61-frame difference at `-m 2048` is the
244 KiB of extra `.bss` the page directories cost.

`-m 512` fails to boot in both, with a clearer message in the second: the
512 MiB heap does not fit in a 510 MiB machine. Pre-existing, since `HEAP_SIZE`
was raised on 2026-09-06.

## What is still on the table

The PMM is still one region. At `-m 5120` QEMU reports 3070 MiB low and 2048 MiB
high, the low one wins the ranking, and the high one is unused — 2554 MiB of
5119, exactly as before. The crossover is around 7 GiB, above which the high
region is the bigger one and the low region (~2.5 GiB, minus the heap) is what
goes unused. On the 16 GiB reference machine that residual is 3275 MiB.

Closing it means giving `akuma-pmm` the span from the lowest usable address to
the highest and marking the holes used. The bitmap is cheap — 512 KiB for
16.7 GiB — but it widens `akuma_pmm::contains`, which is the bounds check behind
`akuma-mmu`'s physical-memory copies, from "this region, all of it RAM" to "this
span, including an 819 MiB MMIO hole with the framebuffer BAR in it". That is a
real weakening of a real guard and wants its own change, with `contains` taught
about the holes rather than left to vouch for them.

## Background

- `amd64/src/phys.rs` — `PHYSMAP_LIMIT`, `PHYSMAP_PDS`, and why the limit moved twice
- `amd64/src/boot.s` — the fill loop and the `%edx` note
- `amd64/src/smp.rs` — `ap_boot_root`
- [`AKUMA_SELF_HEALING_PORT.md`](AKUMA_SELF_HEALING_PORT.md) — where the 4 GiB ceiling was first written down as debt
- [`AMD64_SMP_BRINGUP.md`](AMD64_SMP_BRINGUP.md) — the trampoline and mailbox this rests on
