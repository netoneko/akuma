# akuma-ext2

A read/write **ext2** filesystem driver for `no_std` kernels. The caller supplies
a `BlockDevice` (raw byte-addressed I/O) and a timestamp callback; the crate
implements the kernel's `akuma_vfs::Filesystem` trait on top.

`no_std` + `alloc`. Host-testable: `#![cfg_attr(not(test), no_std)]` lets the same
code link against `std` for `cargo test`, backed by an in-RAM `Vec<u8>` device
and real `mke2fs` fixture images (`tests/fixtures/`).

- **In the kernel:** mounted as the root filesystem from `vda`
  (`src/vfs/ext2.rs` → `KernelBlockDevice` → `crates/akuma-virtio/src/block.rs`).
- **Consumers:** every VFS syscall (`open`, `read`, `write`, `unlink`, `getdents64`,
  `stat`, `rename`, …), plus `mmap` demand-fill via `read_at_by_inode`.

> **If you came here because "fs performance looks like shit":** jump to
> [Performance characteristics](#performance-characteristics). Short version — a
> single-block file `create` costs **~11 synchronous device writes** and a 2 MB
> sequential write has **5.7× write amplification**, because there is no
> write-back cache and the superblock + block-group descriptor are rewritten on
> every block/inode allocate and free. This is unconditional; it is not caused by
> fragmentation or by bulk deletes (see
> [`docs/archive/EXT2_PERFORMANCE_AUDIT.md`](../../docs/archive/EXT2_PERFORMANCE_AUDIT.md)).

---

## Contents

- [Where it sits](#where-it-sits)
- [On-disk layout the driver assumes](#on-disk-layout-the-driver-assumes)
- [Core types](#core-types)
- [Locking, and the IRQ-masked hold](#locking-and-the-irq-masked-hold)
- [The block cache](#the-block-cache)
- [Operation walkthroughs](#operation-walkthroughs)
- [Deferred inode frees](#deferred-inode-frees)
- [Performance characteristics](#performance-characteristics)
- [Allocation audit](#allocation-audit)
- [Feature flags](#feature-flags)
- [Testing](#testing)
- [Known limitations](#known-limitations)
- [Further reading](#further-reading)

---

## Where it sits

```mermaid
flowchart TD
    SC["VFS syscalls<br/>open / read / write / unlink / getdents64 / stat"] --> VFS["akuma_vfs::Filesystem (trait)"]
    MM["mmap demand-fill"] --> RABI["Ext2Filesystem::read_at_by_inode"]
    VFS --> EXT2["akuma-ext2 :: Ext2Filesystem&lt;B&gt;"]
    RABI --> EXT2
    EXT2 --> ST["RwSpinlock&lt;Ext2State&gt;<br/>superblock + geometry"]
    EXT2 --> BC["Spinlock&lt;BlockCache&gt;<br/>read-only, write-invalidated"]
    EXT2 --> DF["DeferredFrees<br/>256 atomic slots"]
    EXT2 --> DEV["B: BlockDevice<br/>read_bytes / write_bytes"]
    DEV --> KBD["src/vfs/ext2.rs :: KernelBlockDevice"]
    KBD --> VBLK["akuma-virtio :: VirtioBlockDevice<br/>sector RMW, busy-polled virtqueue"]
```

The crate is deliberately thin: no directory-entry cache, no inode cache beyond
what the block cache incidentally holds, no write-back buffer, no journal. Every
mutation is written through to the device before the call returns.

---

## On-disk layout the driver assumes

Standard ext2, revision ≥ 1, no `incompat` features negotiated (the driver does
not check `feature_incompat`, so a disk with `filetype` absent or `sparse_super`
tricks will misbehave — the kernel's own images are made with a fixed `mke2fs`
recipe, `scripts/create_disk.sh`).

```
byte 0      1024        block 1        per block group ...
+-----------+-----------+--------------+--------------------------------------------+
| boot area | superblock| group-desc   |  [ block bitmap | inode bitmap |          |
| (1024 B)  | (1024 B)  | table (GDT)  |    1 block      |   1 block    |          |
|           |           |              |    inode table (inodes_per_group entries) |
|           |           |              |    data blocks ...                        |
+-----------+-----------+--------------+--------------------------------------------+
                        \___________________ repeated for every block group ______/

block size   : 1024 << superblock.block_size_log     (kernel images: 4096)
inode size   : superblock.inode_size (rev >= 1) else 128
group count  : ceil((total_blocks - first_data_block) / blocks_per_group)
```

`Ext2State` caches the parsed superblock and the six geometry numbers derived
from it (`block_size`, `inodes_per_group`, `inode_size`, `block_group_count`,
`blocks_per_group`, `first_data_block`). The GDT, bitmaps and inode tables are
**not** cached in `Ext2State` — they are re-read from the device (through the
block cache) on every allocation.

**Block/inode addressing.** `read_inode(n)`: group = `(n-1) / inodes_per_group`,
index = `(n-1) % inodes_per_group`, byte offset =
`bgd.inode_table * block_size + index * inode_size`. `free_block(b)` adjusts by
`first_data_block` first. Inode `2` is the root directory. Inode `0` is "none".

**Block map.** 12 direct + single-indirect + double-indirect. Triple-indirect is
`FsError::NotSupported`. `read_inode_data` refuses files larger than 16 MB — large
files must go through `read_at` / `read_at_by_inode`, which stream.

**Fast symlinks.** Targets ≤ 60 bytes are stored inline in the
`direct_blocks`/indirect pointer area with `sectors_used == 0`; longer targets
allocate data blocks.

---

## Core types

| Type | What it is |
|---|---|
| `Ext2Filesystem<B: BlockDevice>` | the public handle; owns `dev`, `state`, `block_cache`, `deferred`, `write_lock_owner`, `time_fn` |
| `Ext2State` | parsed superblock + geometry, behind `RwSpinlock`; also the in-memory `bgd_cache` / `bgd_dirty` / `sb_dirty` for deferred metadata writeback (see below) |
| `Ext2ReadGuard` / `Ext2WriteGuard` | RAII lock guards that also carry the `StateHoldGuard` (see below); `Ext2WriteGuard::drop` clears `write_lock_owner` |
| `BlockCache` | `ClockBlockCache` with the `fs-cache` feature, else the 64-slot `BlockRingCache`; absent entirely on `extreme` |
| `DeferredFrees` | 256 `AtomicU32` slots for inodes unlinked while still mmap-pinned |

### Deferred metadata writeback

The superblock free counts, every block-group descriptor's free counts, and the
allocation bitmap blocks change on *every* block/inode allocate and free —
~1000 device writes for a 2 MB file (see
[Performance](#performance-characteristics)). Instead, the four allocator
functions (`allocate_block`, `free_block`, `allocate_inode`, `free_inode`) keep
the authoritative copy in `Ext2State::{superblock, bgd_cache, bitmap_cache}` and
only set a dirty flag; `flush_meta` writes the dirty ones at the end of every
mutating `Filesystem` method and on `sync()`. On-disk metadata is therefore
consistent at every syscall boundary — the same guarantee the per-block writes
gave, since a crash *mid*-syscall was already inconsistent.

Only the write-locked allocator paths touch these fields, and only through
`read_bgd_staged` / `stage_bgd` / `bitmap_slot`, so they are the single source of
truth for allocation state — a plain `Vec` with no interior mutability is sound.
Read-only callers of `read_bgd` only read `bgd.inode_table` (immutable), and
nothing outside the allocators reads a bitmap block at all.

`write_dir_range` (Fix C) is the directory analogue: `add_dir_entry` /
`remove_dir_entry` mutate the in-RAM `dir_data` and then write back only the one
block that changed, instead of `write_inode_data` rewriting every block.

Raw on-disk structs (`Superblock`, `BlockGroupDescriptor`, `Inode`,
`DirEntryRaw`) are `#[repr(C, packed)]` and read with `read_unaligned`.

### Constructors

```rust
// Root filesystem — block cache sized from the crate-global cap.
Ext2Filesystem::new(dev, || utc_micros())?;

// Second and later mounts — explicit smaller cap so a data disk does not
// re-commit the whole global cache budget (the cache never shrinks).
Ext2Filesystem::new_with_cache_cap(dev, || utc_micros(), Some(16 << 20))?;
```

`set_cache_cap_bytes(bytes)` is called once by `src/fs.rs` before mount; the
kernel derives it as `min(RAM/8, 128 MB)`.

---

## Locking, and the IRQ-masked hold

`Ext2State` is an `RwSpinlock`. Reads run concurrently; writes are exclusive.
Both `read_state()` and `write_state()` are **unbounded** `try_*` + backoff
loops with a periodic orphaned-lock check: every 10 000 attempts they look at
`write_lock_owner`, and if that thread is dead (`ThreadHooks::is_thread_dead`)
they `force_unlock_write()`. This exists because an fs syscall can be killed
mid-hold.

### `StateHoldGuard`

Every lock guard carries a `StateHoldGuard`, taken *per `try_*` attempt* and
either kept (on success, covering the whole hold) or dropped before the backoff
spin:

| build | `StateHoldGuard` | effect while a state guard is held |
|---|---|---|
| default (`cargo build --release` → `smp-shared` → `no-bkl-vfs`) | `akuma_primitives::PreemptGuard` | **preemption disabled + local IRQs masked** |
| without `no-bkl-vfs` | `NoStateHold` (ZST) | nothing (the Big Kernel Lock covers it) |

Under `no-bkl-vfs` a core can be inside an ext2 critical section *without*
holding the BKL. The IRQ mask is what stops a nested IRQ on that core from
running `enter_kernel()` and hard-spinning for the BKL while this core holds the
inner lock — the AB-BA wedge that `no-bkl-network` closed the same way
(`crates/akuma-virtio/src/block.rs` has the full note). The device lock in
`akuma-virtio` is covered *transitively*: it is only ever reached from inside a
`read_block`/`write_block`/`write_superblock` call, which always runs with a
state guard held.

```
write_at("/x", off, data)              ┌─ PreemptGuard::new()  (preempt off, IRQs MASKED)
  let mut state = self.write_state();  ─┤
  ... resolve, allocate, per-block loop │   << every heap allocation in here
  ... write_inode                       │      runs with IRQs masked >>
  return written                       ─┘─ guard drops: irq_restore(), then preempt on
```

The design intent is *"IRQs masked only for the short hold"*. The crate does not
currently honour that — see the [allocation audit](#allocation-audit).

---

## The block cache

A single `Spinlock<BlockCache>`, separate from the state lock. **Write-back**:
`write_block` updates the cached slot and marks it dirty; the device write is
deferred to `flush_meta` (end of every mutating op), `sync()`, or eviction of a
dirty victim. Read-after-write is therefore a cache hit, and repeated writes to
one block coalesce into a single device write. Design + rationale:
`docs/archive/EXT2_WRITEBACK_DESIGN.md`.

```
read_block(b):
   cache.lock().get(b)  ──hit──▶  to_vec()  ─▶  return  (no device I/O)
        │miss
        ▼
   dev.read_bytes(b*bs, buf)  ─▶  cache.lock().insert(b, buf)  ─▶  return

write_block(b, data):
   cache.lock().write(b, data)        (marks slot dirty; flushes a dirty
                                       eviction victim via the dev callback)

flush_meta() / sync():
   flush_dirty(keep = !alloc_meta)    (data + inode blocks — phase 1)
   ...staged bitmaps / BGDs / superblock written through the cache...
   flush_dirty(keep = all)            (phase 3)
```

Two invariants the write-back adds:

- **Invalidate-on-free never flushes** (`free_block` → `invalidate_block` →
  `cache.remove`): a freed block's stale dirty copy is dropped, not written,
  so it can never clobber a reallocated block.
- **Data before allocation metadata** in `flush_meta` (`is_alloc_meta` +
  the two `flush_dirty` phases): an unclean crash can leak an allocated
  block (e2fsck-recoverable) but never publishes a bitmap/BGD claiming
  blocks whose contents never landed.

### `BlockRingCache` (default without `fs-cache`)

64 slots, one contiguous `Vec<u8>` backing + a `[u32; 64]` tag array. Linear-scan
lookup, pure ring eviction. ~256 KB at 4 KB blocks. Too small to give any reuse
against a self-host build's working set (measured warm/cold ratio 1.00×) — hence:

### `ClockBlockCache` (feature `fs-cache`, in the kernel `default` set)

Sized from detected RAM. Backing grows one `CACHE_CHUNK_BYTES` (1 MB) chunk at a
time up to `capacity_blocks`; slots never move once allocated. `block_num → slot`
`BTreeMap` for O(log n) lookup. CLOCK (second-chance) eviction so a hot working
set survives cold blocks streaming past. Instrumented: `cache_stats()` →
`(hits, misses)`, `cache_occupancy()` → `(slots_used, slots_cap)`.

Under the block cache (all non-`extreme` profiles), inode-table, GDT,
bitmap and directory reads are routed through it (`read_range` /
`write_range`; sub-block writes patch a resident slot in place — no RMW
clone), so a hot `Inode` read is a memcpy.

### Coherence instrumentation (default off)

`E2_VERIFY_HITS` gates `verify_cached_block`, which re-reads every cache hit
straight from the device and compares (`[E2C-BAD]` on mismatch). Under
write-back a *dirty* hit legitimately differs from the disk, so dirty blocks
are skipped — the oracle now hunts exactly "clean slot ≠ disk", i.e. real
staleness. Doubles read I/O — only for chasing the self-host zero-page class
(`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md`). `E2_READ_AT_EOF` counts
`read_at_by_inode` calls whose offset is past `i_size` (`[E2-EOF]`) — an i_size
incoherence signal, not a normal EOF.

---

## Operation walkthroughs

### Read (`read_at` / `read_at_by_inode`)

1. `read_state()` (shared).
2. `lookup_path_internal` walk (for the path form) → inode number.
3. `read_inode` → `Inode`.
4. Resolve logical→physical for the touched block range via `get_block_num`
   (reads indirect blocks through the cache).
5. `read_at_by_inode` batches: it coalesces a run of physically-contiguous
   *uncached* blocks into **one** `dev.read_bytes`, then populates the cache
   per block. Cached blocks are served by memcpy. `read_at` (the simpler path)
   does one `read_block` per logical block.

Reads are cheap once warm — no device writes, and the cache absorbs metadata
(`stat()` on a 7-deep path is ~0.03 device reads/call). But **read-after-write
is never warm**: `write_block` calls `cache.remove` for every block it writes, so
reading back a file you just wrote re-fetches every block from the device (the
514-read row above). A write-*through* cache that kept the just-written bytes in
its slot would fix this — one of the pending items.

### Write (`write_at`) — the expensive path

Device writes for one 4 KB file `create`, `baseline` vs `now` (Fix A + B + C + D-lite):

```
write_at("/dir/f", 0, <4096 bytes>)                    base  now
  write_state()  (exclusive, IRQs masked)
  lookup + lookup_parent -> ("/dir" inode, "f")
  allocate_inode(state, false):
    drain_deferred_frees(state)               (scans 256 slots)
    set inode_bitmap bit                        1     0   (in bitmap_cache — D-lite)
    bgd.free_inodes_count -= 1                  1     0   (staged — B)
    sb.unallocated_inodes  -= 1                 1     0   (staged — B)
  write_inode  ..........................      1     1   (inode table)
  add_dir_entry("/dir", "f"):
    splice entry, write ONLY the changed
      directory block (write_dir_range)  ..    ~1    ~1   (data — was every block, Fix C)
    write dir inode  .....................     1     1   (inode table)
  ensure_block(state, inode, 0, zero_leaf=false):
    allocate_block_inner(state, false):
      scan block_bitmap from bit 0           (O(blocks_per_group), still)
      set block_bitmap bit                     1     0   (in bitmap_cache — D-lite)
      bgd.free_blocks_count -= 1                1     0   (staged — B)
      sb.unallocated_blocks  -= 1               1     0   (staged — B)
      zero-fill the new block  ............    1     0   (skipped — Fix A)
  write_block(phys, data)  ...............     1     1   (data — the real bytes)
  inode.size = 4096; write_inode  .......     1     1   (inode table)
  flush_meta(state):
    write dirty bitmap block(s) + BGD(s)
      + superblock, once each  ...........    -   ~4   (bitmap×2 + gdt + sb)
                                             ----  ----
                                     total   ~11   ~9   device write calls
```

(For a many-small-files workload each `create` is its own syscall, so
`flush_meta` still writes ~4 metadata blocks per file — the staging pays off
when one op does *many* allocations. A 2 MB `write_at` in one call stages 512
block allocations and flushes bitmap + GDT + superblock **once**: 514+514+514 →
~3.)

Then, at the `akuma-virtio` layer, each `write_bytes` is a **sector
read-modify-write**: read the touched 512-byte sectors, patch, write them back,
busy-polling the virtqueue. A 1 KB superblock write is a 2-sector read + 2-sector
write round-trip.

### Lookup (`lookup_path_internal`)

The path walk itself allocates nothing (`path_components` is an iterator). But
for **every component** it calls `read_inode_data` on the parent directory
(materialising the whole directory into a `Vec<u8>`) and `parse_directory`
(a `Vec<(u32, String, u8)>` with a heap `String` per entry), then linear-scans
for the name. Warm, this is all cache hits and CPU-bound; cold, it is one device
read per directory block per component.

### Unlink (`remove_file`)

`lookup_path` + `lookup_parent` (two independent read-locked walks), then
`write_state`, `drain_deferred_frees`, `remove_dir_entry` (rewrites the whole
parent directory), `truncate_inode` (frees every block — bitmap + gdt +
**superblock** write per block), `write_inode`, `free_inode` (bitmap + gdt +
**superblock** again, plus `on_inode_freed` → page-cache invalidation).
Measured: **10 device writes per 4 KB file deleted.**

---

## Deferred inode frees

The one subtle correctness mechanism. A `mmap` region names its file by raw
inode number + `filesz` captured at map time, holding an
`akuma_primitives::inode_pin` rather than a `struct file`. So if `unlink` freed a
pinned inode, `truncate_inode` would zero `i_size` (mapper faults read `Ok(0)` →
zero page) and `free_inode` would hand the number to the next `create` (mapper
faults read **another file's bytes**). Both were silent — root cause #2 of the
self-host `rustc` ICE (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14).

```
remove_file("/x"):  hard_links -> 0
    inode_pin::is_pinned(n) ?
      no  -> truncate_inode + free_inode now
      yes -> deferred.push(n); keep i_size and block pointers; return
                 │  (256 fixed slots; a 257th concurrent pin OVERFLOWS
                 │   and the inode is LEAKED — recoverable by e2fsck.
                 │   Bytes to the wrong reader are not recoverable, so
                 │   the overflow deliberately falls this way.)
                 ▼
drain_deferred_frees(state):   called from every unlink AND every allocate_inode
    for slot in 256:
        n = slot; if n == 0 or is_pinned(n): skip
        CAS(slot, n -> 0)                       (claim; one drainer wins)
        read_inode(n); truncate_inode; write_inode(deletion_time)
        free_inode(n, false)                    -> on_inode_freed(n) -> page cache drop
```

`is_pinned` is **re-checked** in the drain (a mapping created *after* the unlink
still names the inode). The list is per-filesystem, not global — an inode number
only means something relative to its issuing mount.

Counters (`DEFERRED_FREE_PENDING`, `DEFERRED_FREE_LEAKED`, `deferred_free_pending()`)
feed the `[Mem]` dump. A non-zero `DEFERRED_FREE_LEAKED` means `DEFERRED_FREE_SLOTS`
needs raising.

---

## Performance characteristics

Measured with `userspace/ext2probe` (the `ext2probe-host` binary — see
[Testing](#testing)): `Ext2Filesystem` over an in-RAM copy of a real image, with a
`BlockDevice` shim that counts every call and buckets it by on-disk region. Call
counts are the metric because each `write_bytes` is one synchronous busy-polled
virtqueue round-trip on the guest, and a *sector* read-modify-write at that.

**Status 2026-08-26.** Five changes landed: Fix A (skip redundant zero-fill),
Fix B (defer superblock + BGD writes), Fix C (incremental directory block writes
— kills the O(N²)), Fix D-lite (defer bitmap writes + keep bitmap blocks
resident), and **write-back** (the block cache is now authoritative and dirty
blocks are flushed at op end / eviction / `sync()` instead of the cache being
invalidated on every write — `docs/archive/EXT2_WRITEBACK_DESIGN.md`).

Device I/O per operation (`ext2probe-host`, `disk.img`), baseline → each fix:

| Operation | baseline | +A+B | +C | +A+B+C+D-lite | **+write-back** | note |
|---|---:|---:|---:|---:|---:|---|
| create 1 × 4 KB file (300 in one dir) | 11.3 wr | 9.3 | 9.0 | 9.0 wr | **8.0 wr** (reads 1502 → **18**) | inode-table block patched in place, not re-read |
| delete 1 × 4 KB file | 10.0 wr | 9.0 | 8.0 | 8.0 wr | **7.0 wr** (reads 1496 → **0**) | |
| 2 MB sequential write (256 × 8 KB `write()`) | 3327 / 5.7× | 2300 / 4.1× | 2299 / 4.1× | 2042 / 3.6× | **1789 / 3.1×** | reads 1014 → **0** |
| 2 MB read right after that write | 514 reads | 514 | 514 | 514 | **0 reads** | the write-back headline: fully warm |
| build 3200-file tree (16 × 200) | 35 404 wr | — | 28 987 | 28 987 wr / 16 107 rd | **25 613 wr / 205 rd** | 9.1 → 8.0 wr per file; reads −99% |
| mass-delete that tree | 28 970 wr | — | 25 753 | 25 753 wr / 16 115 rd | **22 396 wr / 0 rd** | 8.0 → 7.0 wr per file |
| 100 × warm `stat()` on a 7-component path | — | — | — | 3 reads | **0 reads** | |
| **Nth file into one flat directory** | 11 → **18** as N: 0→2000 | 9 → 16 | 9.0 flat | 9.0 flat | **8.0 flat** | Fix C: O(N²) → O(1) |

Write-back removes essentially *all* remaining device reads (the read side is now
a warm cache) and one write per file (the inode-table block is patched in place —
design doc D-5 — instead of read-modify-written).

Real-kernel A/B (QEMU boot, `ext2probe` guest, BEFORE-pass, median of 3 runs,
2026-08-26): A–D (commit `4b086f3d`) vs the same tree **+ write-back**.

| op | A–D | +write-back | Δ | A–D range | +wb range |
|---|---:|---:|---:|---:|---:|
| create 300 × 4 KB | 1858 ms | 1433 ms | **−23%** | 1699–1874 | 1286–1441 |
| seq_write 2 MB | 858 ms | 669 ms | **−22%** | 803–895 | 637–753 |
| seq_read 2 MB (after write) | 143 ms | 43 ms | **−70%** | 122–175 | 42–54 |
| delete 300 | 1062 ms | 809 ms | **−24%** | 955–1075 | 713–850 |
| build 3200-file tree | 19.3 s | 15.9 s | **−18%** | 18.0–19.7 | 13.5–15.9 |
| mass-delete 3200 | 11.4 s | 7.5 s | **−35%** | 11.3–12.0 | 7.5–8.6 |

Unusually for wall-clock on this host, **the two arms' ranges are disjoint on
every one of the six ops** — the slowest write-back run beats the fastest A–D run
each time. That is what makes this A/B carry a claim on its own; the earlier A–D
A/B did not (see the caveat below), and the deterministic device-I/O counts
remain the primary evidence.

**Caveat (still true):** wall-clock A/B on this host has large run-to-run
variance, worse when other VMs contend for CPU — the A–D measurement it was
written for spanned 2.4–3.6 s on unpatched `create` and 9.8–17.1 s on `mass
delete`. Treat the **device-I/O counts as the number to trust**; wall-clock
confirms direction and magnitude.

### Reference: the same workload on real Linux ext2

`ext2probe-stdfs` (the shared `workload::*` over `std::fs`) run in a Docker
`--privileged` container against a loop-mounted 256 MB ext2 image — same 300
files / 2 MB / 3200-file-tree shapes, median of 3, BEFORE pass. Two mount modes:
`-o sync` (durable on every syscall — Akuma's own model) and default (writeback).

| op | Linux ext2 `-o sync` | Linux ext2 default | **Akuma +write-back** | Akuma vs `-o sync` |
|---|---:|---:|---:|---:|
| create 300 × 4 KB | 71 ms | 1.1 ms | 1433 ms | **~20×** (was ~28×) |
| seq_write 2 MB | 65 ms | 0.35 ms | 669 ms | **~10×** (was ~15×) |
| seq_read 2 MB (after write) | 0.14 ms | 0.14 ms | 43 ms | **~310×** (was ~1300×) |
| delete 300 | 5.4 ms | 0.4 ms | 809 ms | **~150×** (was ~220×) |
| build 3200-file tree | 1.06 s | 11 ms | 15.9 s | **~15×** (was ~20×) |
| mass-delete 3200 | 96 ms (33k files/s) | 3.9 ms (820k files/s) | 7.5 s (~427 files/s) | **~78×** (was ~130×) |

(The container's own overlay/ext4 root — a third data point — matches the
default-writeback column: create ~1.8 ms, tree build ~18 ms.)

Two gaps:

- **vs default (writeback) Linux ext2: still ~1300–2000×** across every op.
  Inherent — a real OS batches every dirty page and flushes asynchronously.
  Akuma flushes at every syscall boundary by design (`flush_meta`), and every
  write is a synchronous busy-polled virtqueue round-trip.
- **vs `-o sync` Linux ext2 (durable every op, the same guarantee Akuma gives):
  ~10–310×**, and *this* is the achievable target. Write-back closed the read
  half of the gap; what is left is all on the write side:
  - ~~**`seq_read` after a write** — pure write-invalidate cold cache.~~
    **Fixed by write-back:** 0 device reads, 143 ms → 43 ms, and the ratio vs
    Linux fell ~1300× → ~310×. The residual 43 ms is the guest's own `read()`
    syscall + copy path, not device I/O.
  - **`delete`: ~150×** — Linux `-o sync` unlinks 300 files in 5 ms; Akuma still
    does ~7 synchronous device *writes* per unlink.
  - `create` / `seq_write` (~10–20×) are the closest. The residue is the ~8
    writes per file that `flush_meta` must still issue at every syscall
    boundary (sb + gdt + 2 bitmap + inode table + data), plus the virtio
    sector-RMW. Coalescing those across syscalls means weakening the
    every-syscall durability guarantee — a separate decision, not a bug fix.

### Root causes, and status

1. **No write-back / write coalescing at any layer.** **Mostly addressed:**
   `write_superblock` / `write_bgd` (Fix B) and the allocation bitmap blocks
   (Fix D-lite) are now kept in memory (`Ext2State::{superblock, bgd_cache,
   bitmap_cache}` + dirty flags) and written by `flush_meta` once at the end of
   every mutating `Filesystem` method and on `sync()`. On-disk metadata is still
   consistent at every syscall boundary — same guarantee the per-block writes
   gave. `sync()` is no longer a no-op. **Data + inode blocks are now write-back
   too:** `write_block` marks the cache slot dirty and the device write is
   deferred to dirty-victim eviction, `flush_meta` (end of every mutating op) or
   `sync()`, so read-after-write is warm (0 device reads). Freed blocks are
   invalidated *without* flushing (design doc D-3) so a stale copy can never
   land on a reallocated block. What remains is coalescing writes *across*
   syscall boundaries, which would weaken the durability guarantee.
2. ~~The superblock (full 1 KB) is rewritten on every block/inode alloc/free.~~
   **Fixed (Fix B).**
3. ~~The block-group descriptor is rewritten the same way.~~ **Fixed (Fix B).**
4. ~~`allocate_block` zeroes each new block with its own device write, then the
   caller overwrites it.~~ **Fixed (Fix A).** `ensure_block` takes a `zero_leaf`
   flag; `write_inode_data`, the full-block arm of `write_at`, and
   `write_dir_range` pass `false`. Indirect/double-indirect pointer blocks and
   partially-written data blocks are still zeroed.
5. ~~`add_dir_entry` / `remove_dir_entry` rewrite the entire directory → O(N²).~~
   **Fixed (Fix C).** `write_dir_range` writes only the block(s) overlapping the
   bytes that changed — one block for a splice/clear, one new block for an
   append. A flat directory is now O(1) per entry (was O(N)). Also closed a
   latent bug: a cross-block `rec_len` merge (removing the first entry of a
   block) now falls back to clearing the inode field, which is what valid ext2
   requires.
6. ~~`allocate_block` re-reads the bitmap block on every allocation.~~ **Fixed
   (Fix D-lite).** Touched bitmap blocks stay in `bitmap_cache`; the write is
   deferred to `flush_meta`. Big read reduction; the write reduction only shows
   on large single ops (2 MB write: 514 → 257 bitmap writes).
7. **`allocate_block` linear-scans the bitmap from bit 0** every call — still
   O(`blocks_per_group`). Pending: resume from a per-group cursor.
8. **`ensure_block` re-reads the indirect block on every block** past the 12th
   during a large sequential write. Pending.

### Why the earlier audit found "NO REGRESSION"

[`docs/archive/EXT2_PERFORMANCE_AUDIT.md`](../../docs/archive/EXT2_PERFORMANCE_AUDIT.md)
compared operation timings *before vs after* a bulk delete and found them equal
(or faster after, from cache warmth). Consistent with the analysis: the cost is
an **unconditional constant multiplier**, not a post-delete degradation, so a
before/after ratio is ~1.0. "Slow after `rm -rf /tmp/akuma`" was the `rm -rf`
itself — deleting a source checkout is (files × ~8-10) synchronous device writes.

### Still pending

- **Write-back block cache** for data + inode blocks: keep written bytes in the
  slot instead of `cache.remove`, mark dirty, flush later. Fixes read-after-write
  and the residual per-op inode/data writes. Highest remaining leverage; also
  the riskiest (coherence — see the `E2C-BAD` history).
- Bitmap-scan cursor; indirect-block reuse across a sequential write.
- A less aggressive `flush_meta` (every N ops, not every op) would cut the
  residual `sb`/`gdt`/`bitmap` writes on many-small-file workloads — but needs
  `sync_all()` wired into the reboot path first (nothing calls it today), or an
  unclean shutdown loses those free-count updates (e2fsck-recoverable).

---

## Allocation audit

Every heap allocation in `src/ext2.rs` outside `#[cfg(test)]`, and whether it is
justifiable. **Context that raises the bar:** under the kernel `default`
(`no-bkl-vfs`), all of this runs with **local IRQs masked and preemption
disabled** for the whole `Ext2State` guard hold (see
[above](#locking-and-the-irq-masked-hold)). An allocation here can therefore:
take the kernel heap lock (spins with IRQs masked if another core holds it),
trigger a `[HEAP-GROW]` / PMM refill under the mask, and stretch a window whose
whole design premise is *"short"*. Unbounded allocations under that mask are the
real defect, over and above the throughput cost.

### Justifiable

| Site | Freq / size | Why it's fine |
|---|---|---|
| `BlockRingCache::new` / `ClockBlockCache` chunks | once at mount / 1 MB chunk amortized | deliberate, heavily documented; chunking bounds the single allocation and never copies existing slots |
| `read_at_by_inode`: `Vec::with_capacity(num_blocks)` phys list | per read, bounded by `buf.len()/block_size` | bounded by the caller's request; small |
| `read_at_by_inode`: `vec![0u8; run_bytes]` run buffer | per cache-miss run, bounded by `buf.len()` | batches a real contiguous disk read — the point of the function |
| `verify_cached_block`: `vec![0u8; bs]` / `to_vec()` snapshot | gated `E2_VERIFY_HITS`, default **off** | diagnostic-only, documented as never-on in production |
| `read_symlink_inode`: `String::from` | per `readlink`, ≤ target length | must return owned; bounded |
| `lookup_parent_internal`: `name.to_string()` | one small `String` per mutating op | needed to carry the final component past the borrow of `state` |
| `read_dir` → `Vec<DirEntry>` (`.collect`, owned names) | per `getdents64` | forced by the `Filesystem` trait signature (see "not justifiable" for the trait itself) |

### Not justifiable (or: the API forces it and the API is the bug)

| Site | Freq / size | Problem | Fix direction |
|---|---|---|---|
| **`read_inode_data`: `Vec::with_capacity(size)`** | **every path-lookup component**, up to **16 MB** | Materialises an entire file/**directory** into heap on every lookup, under IRQ mask. This is the worst one: large, hot, and on the resolution path. | stream directories; give `lookup`/`read_dir` a block-at-a-time internal reader like `read_at_by_inode` already has |
| **`parse_directory`: `Vec<(u32,String,u8)>` + `String` per entry** | every lookup that crosses that dir; **N+1 allocs for N entries** | 2001 allocations to resolve a name in a 2000-entry directory, under IRQ mask | in-place scan yielding `&str` for the "find one name" callers; iterator for `read_dir` |
| ~~`allocate_block`: `vec![0u8; block_size]` zero-fill~~ | ~~every block allocation~~ | **Fixed (Fix A)** — `allocate_block_inner(state, zero_new)` skips it for full-block-overwrite callers. Still allocated (+ written) for metadata and partial-block callers; a shared `static` zero page would drop the remaining allocs. | |
| `read_block`: `vec![0u8; block_size]` return, and `to_vec()` on cache hit | every block read incl. **cache hits** | the signature returns an owned `Vec`, so even a cache hit allocates + memcpys a full block — half the cache's value lost | `read_block_into(&mut [u8])`; callers pass a reused/stack buffer |
| `write_inode_data`: `vec![0u8; block_size]` per block | every block written | fresh zeroed heap block just to copy caller data through it | stack `[u8; 4096]` or a reused scratch buffer on the guard |
| `read_inode`: `vec![0u8; inode_size]` | every inode read | `inode_size` is 128 or 256 — bounded, belongs on the stack | `[u8; 256]` stack buffer |
| `remove_dir`: `Vec<_>` of filtered entry refs | per `rmdir` | built only to call `.is_empty()` | `.iter().any(|(_,n,_)| n != "." && n != "..")` |
| `add_dir_entry` / `remove_dir_entry`: `dir_data.resize` / full `write_inode_data` | per create/unlink | rewrites every directory block (the O(N²) in the perf section) | write only the changed block |

### The trait-level issue

`Filesystem::read_dir -> Result<Vec<DirEntry>, FsError>` and
`read_file -> Result<Vec<u8>, FsError>` force full materialisation. The kernel
already has the streaming shape it wants (`read_at_by_inode`); the directory and
whole-file reads should get the same treatment so nothing allocates
proportionally to file/directory size while holding the IRQ-masked guard.

---

## Feature flags

| Feature | cfg emitted | Effect |
|---|---|---|
| *(none)* | — | 64-slot `BlockRingCache`; `StateHoldGuard = NoStateHold` |
| `fs-cache` | `ext2_fs_cache` | RAM-sized `ClockBlockCache`; GDT/inode reads routed through it |
| `no-bkl-vfs` | (via `akuma-primitives/no-bkl-vfs`) | `StateHoldGuard = PreemptGuard` — preemption off **+ IRQs masked** for every state-lock hold |
| `extreme` | `kernel_profile_extreme` (also from `OPT_LEVEL=z`) | **no block cache at all**; `read_block` always hits the device |

The kernel `default` set includes `fs-cache` and (through `smp-shared`)
`no-bkl-vfs`. `cargo build --release` is real SMP.

---

## Testing

```bash
# host unit tests (fixture images in tests/fixtures/, in-RAM MemBlockDevice)
cargo test -p akuma-ext2 --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

`tests.rs` covers mount, directory listing, create/read/write/unlink, symlinks
(fast + slow), rename, `read_at` boundaries, deferred frees (`deferred_free_len`,
a `#[cfg(test)]` accessor), and the block-cache coherence paths. `Ext2State` and
`try_lock_state` are `pub(crate)` / `#[cfg(test)]`.

Fixtures are real ext2 images (`mkfs.ext2 -b 4096`); regenerate with
`scripts/create_disk.sh` shapes if the layout assumptions change.

### Device-I/O probe

The `ext2probe-host` binary in `userspace/ext2probe` drives create / write /
read / delete / flat-directory / deep-lookup workloads through this crate and
prints device `read_bytes` / `write_bytes` call counts bucketed by region — the
"baseline → now" numbers in [Performance](#performance-characteristics). It
shares its workload definitions (`FsOps`, `workload::*`) with the guest
`ext2probe` ELF, so the two probes never drift.

```bash
cd userspace && HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo run -p ext2probe --bin ext2probe-host \
  --no-default-features --features host-probe --target "$HOST" -- ../disk.img
```

A third binary, `ext2probe-stdfs` (`--features std-probe`), runs the same
workload shapes against the host OS filesystem via `std::fs` — the real-Linux
reference numbers above come from running it in a Docker container against ext2
mounted `-o sync` and default.

---

## Known limitations

- No triple-indirect blocks (`FsError::NotSupported`).
- `read_inode_data` / `read_file` cap at 16 MB.
- `truncate` only shrinks — extending is a silent no-op (`bun`'s use case).
- No `feature_incompat` negotiation — assumes the kernel's own `mke2fs` recipe.
- No `atime` updates on read; `mtime`/`ctime` best-effort via `time_fn`.
- No journal. `sync()` flushes deferred superblock/BGD/bitmap metadata but there
  is no ordering barrier and data/inode blocks are still write-through only.
- Nothing in the kernel calls `sync_all()` today (no `sys_sync`, no reboot
  flush) — deferred metadata still reaches disk because every mutating method
  flushes its own, but an every-N-ops batching optimization would need that
  wired up first.

---

## Further reading

- [`docs/archive/EXT2_PERFORMANCE_AUDIT.md`](../../docs/archive/EXT2_PERFORMANCE_AUDIT.md) — the prior audit this README's perf section supersedes
- [`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md`](../../docs/archive/SELFHOST_ZERO_PAGE_HUNT.md) §14–15 — why deferred frees and `on_inode_freed` exist
- [`docs/reference/subsystems/vfs.md`](../../docs/reference/subsystems/vfs.md) — the VFS layer above this crate
- [`docs/reference/subsystems/locking.md`](../../docs/reference/subsystems/locking.md) — the BKL / `no-bkl-*` model
- `crates/akuma-primitives/src/inode_pin.rs` — the pin mechanism module doc
- `crates/akuma-virtio/src/block.rs` — the device layer, sector RMW, the transitive-guard note
