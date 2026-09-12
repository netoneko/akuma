# amd64: one bitmap across the MMIO hole — the PMM stops dropping half the machine

**2026-09-12.** Firecracker was configured with `mem_size_mib: 6144`. The guest
booted, reported `usable RAM: 6143 MiB`, and `free` inside it said:

```
              total        used        free      shared  buff/cache   available
Mem:           3.0G     1019.9K        3.0G           0        3.1M        3.0G
```

Half the machine, and — as in
[`AKUMA_AMD64_PHYSMAP_ABOVE_4GIB.md`](AKUMA_AMD64_PHYSMAP_ABOVE_4GIB.md), which
is the direct predecessor of this document and ends by describing the fix made
here — nothing between the two numbers saying where it went.

## Two questions, and only one of them was about the heap

The boot log's accounting block read:

```
  mem:  RAM the machine reported, and what became of each:
    0x0000000000000000 + 0 MiB  unused (the PMM manages one region)
    0x0000000000100000 + 3071 MiB  heap
    0x0000000100000000 + 3072 MiB  pmm
  mem:  6143 MiB usable, 3072 MiB in the PMM's region, physmap reaches 64 GiB
  heap: 0x00000000005ac000 + 512 MiB ... ok
```

`3071 MiB heap` is a **region size tagged with a fate**, not a heap size — the
heap is 512 MiB and says so one line later. The label meant "the heap was carved
out of this region"; what it did not say is that the other 2554 MiB of that
region went nowhere at all. The accounting block exists precisely so memory
cannot vanish unexplained, and it had grown a new way to vanish.

So the answer to "why do we have a 3 GB heap?" is that we do not. The answer to
"why does a 6 GB guest see 3 GB?" is below.

## Cause: `akuma-pmm` manages one contiguous range

`akuma_pmm::init(base, size, kernel_end)` is a single bitmap over a single
`base..base+size`. A PC does not have one range: the chipset leaves a hole below
4 GiB for MMIO and the RAM displaced by it reappears just above 4 GiB. So:

| machine | low region | high region |
|---|---|---|
| Firecracker, 6144 MiB | `0x10_0000` + 3071 MiB | `0x1_0000_0000` + 3072 MiB |
| the trashcan (16 GiB) | ~3 GiB | ~13 GiB |

Two answers had already been tried, and both lose a region:

1. **pick the largest for heap *and* PMM** — costs the other one, which on the
   trashcan is 13 GiB of 16;
2. **pick the largest for the PMM, put the heap in the kernel's region**
   (2026-09-12, earlier the same day) — costs that region's *remainder*
   instead. On the 6 GiB guest the high region won the ranking by **5.7 MiB**
   (the kernel image and the heap floor are exactly what push the low region
   below it), so 2554 MiB was dropped.

The second shape also makes the loss depend on the configured size in a way that
is not monotonic: at `MEMORY=4096` the low region (3071 MiB) wins, the heap
shares it, and the guest sees ~2.5 GiB — *less* than the same kernel gives a
6 GiB machine.

## Fix: a sparse arena

One bitmap spanning **from the lowest usable address to the highest**, with the
gap between the regions simply never described.

```rust
akuma_pmm::init_sparse(span_base, span_end - span_base);   // nothing is RAM yet
for u in usable_regions(..) {
    akuma_pmm::add_ram(u.base, u.end - u.base);            // this part is
    akuma_pmm::reserve_range(u.base, u.floor - u.base);    // ... minus the image
}
akuma_pmm::reserve_range(heap_start, HEAP_SIZE);
```

**The polarity is the design.** Init-everything-used and hand back the RAM, not
init-everything-free and punch out the gaps: the gaps are what the caller does
not know — it holds a list of RAM regions in no guaranteed order — so making
"not memory" the default state means a region nobody describes is merely unused,
never handed out as RAM. The failure directions are not symmetric.

Three things follow from that and each is a separate property:

* **`add_ram` is what counts.** `total_pages` is the span; `ram_pages` is what
  was described. `total_count()` reports the latter, because it feeds
  `sysinfo.totalram` and therefore `free(1)`. Reporting the span would have
  turned a 6 GiB machine into a 7 GiB one — the same bug in the opposite
  direction, and harder to notice.
* **`reserve_range` does not change `ram_pages`.** The kernel image and the heap
  are memory the machine has; `free` should count them *used*, not pretend the
  box is smaller.
* **`contains` had to learn the hole.** It is the bounds check under
  `akuma-mmu`'s safe physical copies — the thing that lets a caller pass an
  arbitrary `usize` without reaching device MMIO. Being inside the span stopped
  being the same question as being RAM the moment the span crossed a device
  window (Firecracker's virtio-MMIO at `0xC000_1000`, the LAPIC at
  `0xFEE0_0000`, and on the metal a framebuffer BAR). So `add_ram` records each
  region in a fixed eight-entry table and `contains` requires the range to lie
  entirely within one of them. A plain `init` arena records none, and the check
  short-circuits to exactly what it did before — aarch64 is untouched, by
  construction rather than by accident.

The table is fixed, not a `Vec`, because `contains` runs on the copy path: a
lock there would sit *inside* the safe copies, under every caller's own locks.
`add_ram` refuses (and adds nothing) when the table is full, so memory the
bounds check cannot describe can never become allocatable.

## Result

```
  mem:  RAM the machine reported, and what became of each:
    0x0000000000000000 + 0 MiB  unused (below the 1 MiB floor)
    0x0000000000100000 + 3071 MiB  heap + pmm
    0x0000000100000000 + 3072 MiB  pmm
  mem:  6143 MiB usable, 6143 MiB managed, physmap reaches 64 GiB
  heap: 0x00000000005ac000 + 512 MiB ... ok
  pmm:  arena 0x0000000000100000 + 7167 MiB spanning 2 RAM region(s)
  pmm:  6143 MiB RAM, 1440340 free frames (5626 MiB)
```

```
              total        used        free      shared  buff/cache   available
Mem:           6143         517        5622           0           3        5622
```

676/0 self-tests under Firecracker on the trashcan, host tests green, aarch64
`cargo build --release` unchanged.

**`usable` and `managed` are now printed side by side**, both known before the
PMM exists. That pairing is the part worth keeping: a future region that goes
unmanaged is a difference between two numbers on one line, with the region that
caused it named three lines above, instead of a discrepancy the reader has to
notice between a banner and a frame count.

Two smaller things the rewrite carried:

* **A 1 MiB floor.** The sub-1 MiB region (639 KiB: IVT, BIOS data area, EBDA)
  is reported as ordinary RAM by every machine here and is now explicitly not
  ours — which also means frame 0, whose address reads as a null pointer
  everywhere it is passed, is never handed out.
* **The string-pointer arrays left the kernel stack.** Unrelated to this bug but
  in the same function's neighbourhood — see
  [`RUST_TOOLCHAIN_AMD64.md`](RUST_TOOLCHAIN_AMD64.md)'s session 3.

## Host tests

`crates/akuma-pmm/src/lib.rs`, `bitmap_allocator_tests` and `ram_region_tests`:
a sparse arena owns nothing until `add_ram` runs; `add_ram` counts regions and
not the span; `alloc_page` never returns an address in the gap (exhaustive —
the arena is drained and every frame checked); reserving does not shrink the
machine; reserve is idempotent; both calls clip rather than wrap; a partially
covered page is a covered page; and the 6 GiB shape reproduces 6143 MiB exactly.
For `contains`: the MMIO hole is refused by address (virtio-MMIO and the LAPIC
by name), and a copy straddling a region's edge is refused whole.

## Verify

- Boot any amd64 target and read the two `mem:` lines: **`usable` must equal
  `managed`**, or a region three lines above says why not.
- `busybox free -m` in the guest reports the configured size, minus ~517 MiB for
  the heap and the image.
- `MEMORY=4096 …` and `MEMORY=8192 …` both report their configured size: the old
  shape was non-monotonic across the 4 GiB crossover and this is where that
  showed.

## Background

- [`AKUMA_AMD64_PHYSMAP_ABOVE_4GIB.md`](AKUMA_AMD64_PHYSMAP_ABOVE_4GIB.md) — the
  predecessor: `PHYSMAP_LIMIT` 4 GiB → 64 GiB, the 16 GiB machine that saw 15%
  of itself, and the closing section that specifies this change including the
  `contains` obligation.
- `amd64/src/mem.rs` — `init_reserving`, the `Fate` labels, `LOW_RAM_FLOOR`.
- `crates/akuma-pmm/src/lib.rs` — `init_sparse`, `add_ram`, `reserve_range`,
  `MAX_RAM_REGIONS`, `range_within_any`.
