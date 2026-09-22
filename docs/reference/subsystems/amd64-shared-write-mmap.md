# Writable `MAP_SHARED` file mappings (amd64)

**Grade: C** — landed 2026-09-22, verified on bare metal the same day, one
caller (ParityDB via akuma-miot's `storeprobe`) exercised end to end.

> **aarch64, same day:** the `mmap::plan` change below is *shared*, and it
> silently broke aarch64. Its lazy-file path never registered shared-writable
> mappings for write-back, so a clean close lost every write. aarch64 now has
> the same present-page write-back contract on its own `SharedFileMapping`
> record (`akuma-syscalls-glue/src/mem.rs`), plus the missing `fadvise64`
> arm. See [`../../archive/MIOT_MESH_ON_AKUMA.md`](../../archive/MIOT_MESH_ON_AKUMA.md)
> §1. A change to `mmap::plan` must be checked against **both** dispatchers.

`mmap(MAP_SHARED, PROT_WRITE)` on a regular file used to be `ENOSYS` on this
target: coherence needs either a page cache or write-back, and neither
existed. It is now served, without a page cache, by **demand-paged fills plus
whole-region write-back**:

- `akuma-syscalls-mem::mmap::plan` marks the request `is_shared_writable` and
  — since this change — leaves it `file_lazy_eligible` like every other file
  mapping. The old rule excluded it ("stay resident so pages can be written
  back") and was wrong in the other direction: parity-db maps
  `len + ~1 GiB` of **reserve address space** per file, and an eager fill of
  that is a gigabyte of zero frames.
- `amd64/src/mm.rs::sys_mmap` refuses a shared-writable request whose fd is
  not opened for writing (`EACCES`) or has no write identity — a `/dev` node,
  or no path. The region then carries **both** records: `file` (the
  `FileBacking` the demand-fill reads from, per fault, with the file's
  *current* bytes) and `shared_write` (a `MmapRegion::SharedWriteBack`:
  mount, inode, offset, path).
- Write-back runs on **`munmap`** (`unmap_range`, which snapshots the
  overlapping shared-writable regions *before* detaching anything), on
  **`msync`** (`mm::sys_msync`, x86_64 nr 26, routed before the shared table
  because asm-generic has no msync number and the abi table's invariant is
  "a variant only exists if it has a number on both architectures"), and on
  **`madvise(MADV_DONTNEED)`** (`dontneed_range` flushes a page before
  zeroing it). The flush walks the region's **present leaves** — not the
  region's frame list, which an eager fill does not populate and a CoW break
  does not update — and writes each page whose offset is below the file's
  size *queried at flush time* through `akuma_vfs_glue::write_at`. No dirty
  tracking: whole pages, I/O spent, never bytes miswritten.
- `madvise`/`msync` flushing does ext2 I/O with no region or address-space
  lock held: the walk collects, the locks drop, then the writes run.

## What this does **not** promise

No page cache means no cross-mapper coherence. Two mappers of one file do not
see each other's writes until a flush lands; `read(2)`/`write(2)` on the same
file see a mapping's writes only after one. A process **killed** with the
mapping live loses writes since its last flush — the exit path does not
flush. Stores past the file's current EOF are dropped by the flush (the
caller extends with `ftruncate`, which parity-db does). A `fork` child of a
shared-writable mapping drops the record — it owns no frames, and flushing a
CoW-broken private copy would push one address space's divergence into the
file. Pages filled on fault read the file as it is *at fault time*, which is
what makes file growth under a long-lived mapping work.

## The three bugs this uncovered (all fixed same day)

1. **ext2 `truncate` answered `Ok(())` for extend.** "Extending would require
   allocating blocks — not implemented; bun only shrinks" — and then returned
   success. Every `ftruncate`-extend was a silent no-op, which made
   `parity-db`'s `set_len`-then-write pattern a zero-byte file. Extension now
   allocates zero blocks (`ensure_block(_, zero_leaf = true)`), zeroes the
   tail of the old EOF's block, and reports ENOSPC honestly if allocation
   fails. Host test: `akuma-ext2::tests::ftruncate_extends_with_zero_blocks`.
2. **`posix_fadvise` had no table row** (x86_64 221, asm-generic 223 — the
   one place the two ABIs are *allowed* to agree on this table is checked by
   name in `akuma-syscalls-abi`'s tests). parity-db `try_io!`s it after
   opening every file, so ENOSYS aborted the whole open. The handler returns
   0: advisory by Linux's own contract, and there is no readahead state to
   tune.
3. **The demand-fill path is growth-tolerant by construction** — the fill
   reads the file's current bytes per fault — which is why shared-writable
   mappings could join the lazy path rather than needing an EOF snapshot at
   `mmap` time. `SharedWriteBack` deliberately carries no `filesz` for the
   same reason.

## Verified

`dist/storeprobe` (akuma-miot) — fresh ParityDB open, 256 appends, read-back,
compact, rewind, **reopen across a process boundary**, 64 KiB value — all 7
stages on real bare metal (`ssh akuma`, the trashcan) and under the amd64
QEMU loop, after previously dying at stage 6 with `ENOSYS` on the metal.
The amd64 boot suite matches the stock baseline (`--local-only --smp 1`:
731 passed, the one pre-existing `lazy path` flake unchanged). `miot node`
survives `kill`/reboot over a grown database, supervised by `herd`, on the
trashcan.

Background: found from akuma-miot, where ParityDB's `MmapMut` died at
`reopen` on the physical box with `ENOSYS` — see that repo's
`docs/TOPOLOGY.md` `node5` section for the deployment and
`crates/miot-store/src/bin/mmapprobe.rs` for the raw-syscall probe that
pinned the original refusal (which **segfaulted** rather than returning
`ENOSYS` — the refusal lived inside `plan`'s eager arm, and the probe's
4th shape never came back). The full investigation, including the two
silent-wrong-answer bugs the new path exposed and the probe series that
eliminated the suspects: [`../../archive/AKUMA_AMD64_SHARED_WRITE_MMAP.md`](../../archive/AKUMA_AMD64_SHARED_WRITE_MMAP.md).
